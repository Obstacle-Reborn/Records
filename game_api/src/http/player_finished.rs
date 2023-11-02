use actix_web::web::Json;
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::Connection;

use crate::{
    graphql::get_rank_or_full_update,
    models::{self, Map, Record},
    must, redis,
    utils::format_map_key,
    Database, RecordsError, RecordsResult,
};

use super::event;

#[derive(Deserialize, Debug)]
pub struct HasFinishedBody {
    pub time: i32,
    pub respawn_count: i32,
    pub map_uid: String,
    pub flags: Option<u32>,
    pub cps: Vec<i32>,
}

pub type PlayerFinishedBody = Json<HasFinishedBody>;

#[derive(Deserialize, Serialize)]
pub struct HasFinishedResponse {
    has_improved: bool,
    login: String,
    old: i32,
    new: i32,
    current_rank: i32,
    reversed: bool,
}

#[derive(Clone)]
struct InsertRecordParams {
    time: i32,
    respawn_count: i32,
    flags: Option<u32>,
    cps: Vec<i32>,
}

async fn send_query(
    db: &Database,
    map_id: u32,
    player_id: u32,
    body: InsertRecordParams,
) -> RecordsResult<u32> {
    let mut mysql_conn = db.mysql_pool.acquire().await?;
    let now = Utc::now().naive_utc();

    let record_id = mysql_conn
        .transaction(|txn| {
            Box::pin(async move {
                let record_id: u32 = sqlx::query_scalar(
                "INSERT INTO records (record_player_id, map_id, time, respawn_count, record_date, flags)
                    VALUES (?, ?, ?, ?, ?, ?) RETURNING record_id",
                )
                .bind(player_id)
                .bind(map_id)
                .bind(body.time)
                .bind(body.respawn_count)
                .bind(now)
                .bind(body.flags)
                .fetch_one(&mut **txn)
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
                .execute(&mut **txn)
                .await?;

                Ok::<_, RecordsError>(record_id)
            })
        })
        .await?;

    Ok(record_id)
}

async fn insert_record(
    db: &Database,
    map @ Map { id: map_id, .. }: &Map,
    player_id: u32,
    body: &InsertRecordParams,
    event: Option<&(models::Event, models::EventEdition)>,
) -> RecordsResult<u32> {
    let mut redis_conn = db.redis_pool.get().await?;
    let key = format_map_key(*map_id, event);
    let added: Option<i64> = redis_conn.zadd(key, player_id, body.time).await.ok();
    if added.is_none() {
        let _count = redis::update_leaderboard(db, map, event).await?;
    }

    let record_id = send_query(db, *map_id, player_id, body.clone()).await?;

    Ok(record_id)
}

pub struct FinishedOutput {
    pub record_id: u32,
    pub res: HasFinishedResponse,
}

pub async fn finished(
    login: String,
    db: &Database,
    Json(body): Json<HasFinishedBody>,
    event: Option<&(models::Event, models::EventEdition)>,
) -> RecordsResult<FinishedOutput> {
    // First, we retrieve all what we need to save the record
    let player_id = must::have_player(db, &login).await?.id;
    let ref map @ Map {
        id: map_id,
        cps_number,
        reversed,
        ..
    } = must::have_map(db, &body.map_uid).await?;
    let reversed = reversed.unwrap_or(false);

    let params = InsertRecordParams {
        time: body.time,
        respawn_count: body.respawn_count,
        flags: body.flags,
        cps: body.cps,
    };

    let (join_event, and_event) = event
        .is_some()
        .then(event::get_sql_fragments)
        .unwrap_or_default();

    // We check that the cps times are coherent to the final time
    if matches!(cps_number, Some(num) if num + 1 != params.cps.len() as u32)
        || params.cps.iter().sum::<i32>() != params.time
    {
        return Err(RecordsError::InvalidTimes);
    }

    let query = format!(
        "SELECT r.* FROM records r
        {join_event}
        WHERE map_id = ? AND record_player_id = ?
        {and_event}
        ORDER BY time {order} LIMIT 1",
        join_event = join_event,
        and_event = and_event,
        order = if reversed { "DESC" } else { "ASC" },
    );

    // We retrieve the optional old record to compare with the new one
    let mut query = sqlx::query_as::<_, Record>(&query)
        .bind(map_id)
        .bind(player_id);

    if let Some((event, edition)) = event {
        query = query.bind(event.id).bind(edition.id);
    }

    let old_record = query.fetch_optional(&db.mysql_pool).await?;

    let (old, new, has_improved) = if let Some(Record { time: old, .. }) = old_record {
        let improved = if reversed {
            params.time > old
        } else {
            params.time < old
        };

        (old, params.time, improved)
    } else {
        (params.time, params.time, true)
    };

    // We insert the record (whether it is the new personal best or not)
    let record_id = insert_record(db, map, player_id, &params, event).await?;

    // TODO: Remove this after having added event mode into the TP
    let original_uid = body.map_uid.replace("_benchmark", "");
    if original_uid != body.map_uid {
        let ref map @ Map {
            cps_number: original_cps_number,
            reversed: original_reversed,
            ..
        } = must::have_map(db, &original_uid).await?;

        if cps_number == original_cps_number && reversed == original_reversed.unwrap_or(false) {
            insert_record(db, map, player_id, &params, None).await?;
        } else {
            return Err(RecordsError::MapNotFound(original_uid));
        }
    }

    let current_rank = get_rank_or_full_update(
        db,
        map,
        if reversed { old.max(new) } else { old.min(new) },
        event,
    )
    .await?;

    Ok(FinishedOutput {
        record_id,
        res: HasFinishedResponse {
            has_improved,
            login,
            old,
            new,
            current_rank,
            reversed,
        },
    })
}
