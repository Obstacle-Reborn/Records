use std::sync::OnceLock;

use actix_session::Session;
use actix_web::{
    web::{self, Data, Json, Query},
    HttpResponse, Responder, Scope,
};
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use futures::StreamExt;
use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use tokio::time::timeout;
use tracing::Level;

use crate::{
    auth::{
        self, privilege, AuthHeader, AuthState, MPAuthGuard, Message, WebToken, TIMEOUT,
        WEB_TOKEN_SESS_KEY,
    },
    graphql::get_rank_or_full_update,
    models::{Banishment, Map, Player, Record},
    read_env_var_file, redis,
    utils::{format_map_key, json},
    AccessTokenErr, Database, RecordsError, RecordsResult,
};

use super::admin;

pub fn player_scope() -> Scope {
    web::scope("/player")
        .route("/update", web::post().to(update))
        .route("/finished", web::post().to(finished))
        .route("/get_token", web::post().to(get_token))
        .route("/give_token", web::post().to(post_give_token))
        .route("/pb", web::get().to(pb))
        .route("/times", web::post().to(times))
        .route("/info", web::get().to(info))
}

#[derive(Serialize, Deserialize, Clone, FromRow, Debug)]
pub struct UpdatePlayerBody {
    pub login: String,
    pub name: String,
    pub zone_path: Option<String>,
}

async fn insert_player(db: &Database, login: &str, body: UpdatePlayerBody) -> RecordsResult<u32> {
    let id = sqlx::query_scalar(
        "INSERT INTO players
        (login, name, join_date, zone_path, admins_note, role)
        VALUES (?, ?, SYSDATE(), ?, NULL, 0) RETURNING id",
    )
    .bind(login)
    .bind(body.name)
    .bind(body.zone_path)
    .fetch_one(&db.mysql_pool)
    .await?;

    Ok(id)
}

pub async fn get_or_insert(
    db: &Database,
    login: &str,
    body: UpdatePlayerBody,
) -> RecordsResult<u32> {
    if let Some(id) = sqlx::query_scalar("SELECT id FROM players WHERE login = ?")
        .bind(login)
        .fetch_optional(&db.mysql_pool)
        .await?
    {
        return Ok(id);
    }

    insert_player(db, login, body).await
}

pub async fn update(
    db: Data<Database>,
    AuthHeader { login, token }: AuthHeader,
    Json(body): Json<UpdatePlayerBody>,
) -> RecordsResult<impl Responder> {
    match auth::check_auth_for(&db, &login, &token, privilege::PLAYER).await {
        Ok(()) => update_or_insert(&db, &login, body).await?,
        Err(RecordsError::PlayerNotFound(_)) => {
            let _ = insert_player(&db, &login, body).await?;
        }
        Err(e) => return Err(e),
    }

    Ok(HttpResponse::Ok().finish())
}

pub async fn update_or_insert(
    db: &Database,
    login: &str,
    body: UpdatePlayerBody,
) -> RecordsResult<()> {
    if let Some(id) = sqlx::query_scalar::<_, u32>("SELECT id FROM players WHERE login = ?")
        .bind(login)
        .fetch_optional(&db.mysql_pool)
        .await?
    {
        sqlx::query("UPDATE players SET name = ?, zone_path = ? WHERE id = ?")
            .bind(body.name)
            .bind(body.zone_path)
            .bind(id)
            .execute(&db.mysql_pool)
            .await?;

        return Ok(());
    }

    let _ = insert_player(db, login, body).await?;
    Ok(())
}

#[derive(Deserialize, Debug)]
pub struct HasFinishedBody {
    pub time: i32,
    pub respawn_count: i32,
    pub map_uid: String,
    pub flags: Option<u32>,
    pub cps: Vec<i32>,
}

#[derive(Deserialize, Serialize)]
struct HasFinishedResponse {
    has_improved: bool,
    login: String,
    old: i32,
    new: i32,
    current_rank: i32,
    reversed: bool,
}

pub async fn finished(
    MPAuthGuard { login }: MPAuthGuard<{ privilege::PLAYER }>,
    db: Data<Database>,
    Json(body): Json<HasFinishedBody>,
) -> RecordsResult<impl Responder> {
    let body = body;

    async fn insert_record(
        db: &Database,
        redis_conn: &mut deadpool_redis::Connection,
        player_id: u32,
        map_id: u32,
        body: &HasFinishedBody,
        key: &str,
        reversed: bool,
    ) -> RecordsResult<()> {
        let added: Option<i64> = redis_conn.zadd(&key, player_id, body.time).await.ok();
        if added.is_none() {
            let _count = redis::update_leaderboard(db, &key, map_id, reversed).await?;
        }

        let now = Utc::now().naive_utc();

        let record_id: u32 = sqlx::query_scalar(
            "INSERT INTO records (player_id, map_id, time, respawn_count, record_date, flags)
            VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(player_id)
        .bind(map_id)
        .bind(body.time)
        .bind(body.respawn_count)
        .bind(now)
        .bind(body.flags)
        .fetch_one(&db.mysql_pool)
        .await?;

        let cps_times = body
            .cps
            .iter()
            .enumerate()
            .map(|(i, t)| format!("({i}, {map_id}, {record_id}, {t})"))
            .collect::<Vec<String>>()
            .join(", ");

        sqlx::query(
            format!(
                "INSERT INTO checkpoint_times (cp_num, map_id, record_id, time)
                VALUES {cps_times}"
            )
            .as_str(),
        )
        .execute(&db.mysql_pool)
        .await?;

        Ok(())
    }

    let Some(Player { id: player_id, .. }) = get_player_from_login(&db, &login).await? else {
        return Err(RecordsError::PlayerNotFound(login));
    };

    let Some(Map { id: map_id, cps_number, reversed, .. }) = get_map_from_game_id(&db, &body.map_uid).await? else {
        return Err(RecordsError::MapNotFound(body.map_uid));
    };

    let reversed = reversed.unwrap_or(false);
    let map_key = format_map_key(map_id);

    if matches!(cps_number, Some(num) if num + 1 != body.cps.len() as u32)
        || body.cps.iter().sum::<i32>() != body.time
    {
        return Err(RecordsError::InvalidTimes);
    }

    let mut redis_conn = db.redis_pool.get().await?;

    let old_record = sqlx::query_as::<_, Record>(&format!(
        "SELECT * FROM records WHERE map_id = ? AND player_id = ?
            ORDER BY time {} LIMIT 1",
        if reversed { "DESC" } else { "ASC" }
    ))
    .bind(map_id)
    .bind(player_id)
    .fetch_optional(&db.mysql_pool)
    .await?;

    let (old, new, has_improved) = if let Some(Record { time: old, .. }) = old_record {
        let improved = if reversed {
            body.time > old
        } else {
            body.time < old
        };

        (old, body.time, improved)
    } else {
        (body.time, body.time, true)
    };

    insert_record(
        &db,
        &mut redis_conn,
        player_id,
        map_id,
        &body,
        &map_key,
        reversed,
    )
    .await?;

    let re = Regex::new("(?<gameId>\\w+)_benchmark").unwrap();
    if let Some(game_id) = re
        .captures(&body.map_uid)
        .and_then(|cap| cap.name("gameId"))
    {
        if let Some(Map {
            id: map_id,
            cps_number: regular_cps_number,
            reversed: regular_reversed,
            ..
        }) = get_map_from_game_id(&db, game_id.as_str()).await?
        {
            if cps_number == regular_cps_number && reversed == regular_reversed.unwrap_or(false) {
                let map_key = format_map_key(map_id);
                insert_record(
                    &db,
                    &mut redis_conn,
                    player_id,
                    map_id,
                    &HasFinishedBody {
                        time: body.time,
                        respawn_count: body.respawn_count,
                        map_uid: game_id.as_str().to_owned(),
                        flags: body.flags,
                        cps: body.cps,
                    },
                    &map_key,
                    reversed,
                )
                .await?;
            }
        }
    }

    let current_rank = get_rank_or_full_update(
        &db,
        &mut redis_conn,
        &map_key,
        map_id,
        if reversed { old.max(new) } else { old.min(new) },
        reversed,
    )
    .await?;

    json(HasFinishedResponse {
        has_improved,
        login: login,
        old,
        new,
        current_rank,
        reversed,
    })
}

pub async fn get_player_from_login(
    db: &Database,
    player_login: &str,
) -> Result<Option<Player>, RecordsError> {
    let r = sqlx::query_as("SELECT * FROM players WHERE login = ?")
        .bind(player_login)
        .fetch_optional(&db.mysql_pool)
        .await?;
    Ok(r)
}

pub async fn check_banned(
    db: &MySqlPool,
    player_id: u32,
) -> Result<Option<Banishment>, RecordsError> {
    let r = sqlx::query_as("SELECT * FROM current_bans WHERE player_id = ?")
        .bind(player_id)
        .fetch_optional(db)
        .await?;
    Ok(r)
}

pub async fn get_map_from_game_id(
    db: &Database,
    map_game_id: &str,
) -> Result<Option<Map>, RecordsError> {
    let r = sqlx::query_as("SELECT * FROM maps WHERE game_id = ?")
        .bind(map_game_id)
        .fetch_optional(&db.mysql_pool)
        .await?;
    Ok(r)
}

#[derive(Serialize)]
struct MPAccessTokenBody<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MPAccessTokenResponse {
    AccessToken { access_token: String },
    Error(AccessTokenErr),
}

#[derive(Deserialize, Debug)]
struct MPServerRes {
    #[serde(alias = "login")]
    res_login: String,
}

static MP_APP_CLIENT_ID: OnceLock<String> = OnceLock::new();
static MP_APP_CLIENT_SECRET: OnceLock<String> = OnceLock::new();

fn get_mp_app_client_id() -> &'static str {
    MP_APP_CLIENT_ID.get_or_init(|| read_env_var_file("RECORDS_MP_APP_CLIENT_ID_FILE"))
}

fn get_mp_app_client_secret() -> &'static str {
    MP_APP_CLIENT_SECRET.get_or_init(|| read_env_var_file("RECORDS_MP_APP_CLIENT_SECRET_FILE"))
}

async fn test_access_token(
    client: &Client,
    login: &str,
    ref code: String,
    ref redirect_uri: String,
) -> RecordsResult<bool> {
    let res = client
        .post("https://prod.live.maniaplanet.com/login/oauth2/access_token")
        .form(&MPAccessTokenBody {
            grant_type: "authorization_code",
            client_id: get_mp_app_client_id(),
            client_secret: get_mp_app_client_secret(),
            code,
            redirect_uri,
        })
        .send()
        .await?
        .json()
        .await?;

    let access_token = match res {
        MPAccessTokenResponse::AccessToken { access_token } => access_token,
        MPAccessTokenResponse::Error(err) => return Err(RecordsError::AccessTokenErr(err)),
    };

    check_mp_token(client, login, access_token).await
}

async fn check_mp_token(client: &Client, login: &str, token: String) -> RecordsResult<bool> {
    let res = client
        .get("https://prod.live.maniaplanet.com/webservices/me")
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await?;
    let MPServerRes { res_login } = match res.status() {
        StatusCode::OK => res.json().await?,
        _ => return Ok(false),
    };

    Ok(res_login.to_lowercase() == login.to_lowercase())
}

#[derive(Deserialize)]
pub struct GetTokenBody {
    login: String,
    state: String,
    redirect_uri: String,
}

#[derive(Serialize)]
struct GetTokenResponse {
    token: String,
}

pub async fn get_token(
    db: Data<Database>,
    client: Data<Client>,
    state: Data<AuthState>,
    Json(body): Json<GetTokenBody>,
) -> RecordsResult<impl Responder> {
    // retrieve access_token from browser redirection
    let (tx, rx) = state.connect_with_browser(body.state.clone()).await?;
    let code = match timeout(TIMEOUT, rx).await {
        Ok(Ok(Message::MPCode(access_token))) => access_token,
        _ => {
            tracing::event!(
                Level::WARN,
                "Token state `{}` timed out, removing it",
                body.state.clone()
            );
            state.remove_state(body.state).await;
            return Err(RecordsError::Timeout);
        }
    };

    let err_msg = "/get_token rx should not be dropped at this point";

    // check access_token and generate new token for player ...
    match test_access_token(&client, &body.login, code, body.redirect_uri).await {
        Ok(true) => (),
        Ok(false) => {
            tx.send(Message::InvalidMPCode).expect(err_msg);
            return Err(RecordsError::InvalidMPCode);
        }
        Err(RecordsError::AccessTokenErr(err)) => {
            tx.send(Message::AccessTokenErr(err.clone()))
                .expect(err_msg);
            return Err(RecordsError::AccessTokenErr(err));
        }
        err => {
            let _ = err?;
        }
    }

    let (mp_token, web_token) = auth::gen_token_for(&db, &body.login).await?;
    tx.send(Message::Ok(WebToken {
        login: body.login,
        token: web_token,
    }))
    .expect(err_msg);

    json(GetTokenResponse { token: mp_token })
}

#[derive(Deserialize)]
pub struct GiveTokenBody {
    code: String,
    state: String,
}

#[derive(Serialize)]
pub struct GiveTokenResponse {
    login: String,
    token: String,
}

pub async fn post_give_token(
    session: Session,
    state: Data<AuthState>,
    Json(body): Json<GiveTokenBody>,
) -> RecordsResult<impl Responder> {
    let web_token = state.browser_connected_for(body.state, body.code).await?;
    session
        .insert(WEB_TOKEN_SESS_KEY, web_token)
        .expect("unable to insert session web token");
    Ok(HttpResponse::Ok().finish())
}

#[derive(Serialize)]
struct IsBannedResponse {
    login: String,
    banned: bool,
    current_ban: Option<admin::Banishment>,
}

#[derive(Deserialize)]
struct PbBody {
    map_uid: String,
}

#[derive(Serialize)]
struct PbResponse {
    rs_count: i32,
    cps_times: Vec<PbCpTimesResponseItem>,
}

#[derive(FromRow)]
struct PbResponseItem {
    rs_count: i32,
    cp_num: u32,
    time: i32,
}

#[derive(Serialize, FromRow)]
struct PbCpTimesResponseItem {
    cp_num: u32,
    time: i32,
}

async fn pb(
    MPAuthGuard { login }: MPAuthGuard<{ privilege::PLAYER }>,
    db: Data<Database>,
    Query(PbBody { map_uid }): Query<PbBody>,
) -> RecordsResult<impl Responder> {
    let mut times = sqlx::query_as::<_, PbResponseItem>(
        "SELECT r.respawn_count AS rs_count, cps.cp_num AS cp_num, cps.time AS time
            FROM checkpoint_times cps
            INNER JOIN maps m ON m.id = cps.map_id
            INNER JOIN records r ON r.id = cps.record_id
            INNER JOIN players p on r.player_id = p.id
            WHERE m.game_id = ? AND p.login = ?
                AND r.time = (
                    SELECT MIN(time) FROM records r2
                    WHERE r2.map_id = m.id AND p.id = r2.player_id
                )",
    )
    .bind(map_uid)
    .bind(login)
    .fetch(&db.mysql_pool);

    let mut res = PbResponse {
        rs_count: 0,
        cps_times: Vec::with_capacity(times.size_hint().0),
    };

    while let Some(PbResponseItem {
        rs_count,
        cp_num,
        time,
    }) = times.next().await.transpose()?
    {
        res.rs_count = rs_count;
        res.cps_times.push(PbCpTimesResponseItem { cp_num, time });
    }

    json(res)
}

#[derive(Deserialize)]
struct TimesBody {
    maps_uids: Vec<String>,
}

#[derive(Serialize, FromRow)]
struct TimesResponseItem {
    map_uid: String,
    time: i32,
}

async fn times(
    MPAuthGuard { login }: MPAuthGuard<{ privilege::PLAYER }>,
    db: Data<Database>,
    Json(body): Json<TimesBody>,
) -> RecordsResult<impl Responder> {
    let Some(player) = get_player_from_login(&db, &login).await? else {
        return Err(RecordsError::PlayerNotFound(login));
    };

    let query = format!(
        "SELECT m.game_id AS map_uid, MIN(r.time) AS time
        FROM maps m
        INNER JOIN records r ON r.map_id = m.id
        WHERE r.player_id = ? AND m.game_id IN ({})
        GROUP BY m.id",
        body.maps_uids
            .iter()
            .map(|_| "?".to_owned())
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut query = sqlx::query_as::<_, TimesResponseItem>(&query).bind(player.id);

    for map_uid in body.maps_uids {
        query = query.bind(map_uid);
    }

    let result = query.fetch_all(&db.mysql_pool).await?;
    json(result)
}

#[derive(Deserialize)]
pub struct InfoBody {
    login: String,
}

#[derive(Serialize, FromRow)]
struct InfoResponse {
    id: u32,
    login: String,
    name: String,
    join_date: Option<chrono::NaiveDateTime>,
    zone_path: Option<String>,
    role_name: String,
}

pub async fn info(
    db: Data<Database>,
    Query(body): Query<InfoBody>,
) -> RecordsResult<impl Responder> {
    let Some(info) = sqlx::query_as::<_, InfoResponse>(
        "SELECT *, (SELECT role_name FROM role WHERE id = role) as role_name
        FROM players WHERE login = ?")
    .bind(&body.login)
    .fetch_optional(&db.mysql_pool).await? else {
        return Err(RecordsError::PlayerNotFound(body.login));
    };

    json(info)
}
