use actix_web::{
    web::{Data, Query},
    Responder,
};
use deadpool_redis::redis::AsyncCommands;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{
    graphql::get_rank_or_full_update,
    models::{self, Map},
    must, redis,
    utils::{format_map_key, json},
    Database, RecordsResult,
};

use super::event;

#[derive(Deserialize)]
pub struct OverviewQuery {
    #[serde(alias = "playerId")]
    login: String,
    #[serde(alias = "mapId")]
    map_uid: String,
}

pub type OverviewReq = Query<OverviewQuery>;

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct RecordQueryRow {
    pub login: String,
    pub nickname: String,
    pub time: i32,
    #[sqlx(flatten)]
    pub map: Map,
}

#[derive(Debug, Clone, Deserialize, Serialize, sqlx::FromRow)]
#[serde(rename = "records")]
pub struct RankedRecord {
    pub rank: u32,
    pub login: String,
    pub nickname: String,
    pub time: i32,
}

async fn get_range(
    db: &Database,
    Map {
        id: map_id,
        reversed,
        ..
    }: &Map,
    (start, end): (u32, u32),
    event: Option<&(models::Event, models::EventEdition)>,
) -> RecordsResult<Vec<RankedRecord>> {
    let reversed = reversed.unwrap_or(false);
    let key = format_map_key(*map_id, event);

    let (join_event, and_event) = event
        .is_some()
        .then(event::get_sql_fragments)
        .unwrap_or_default();

    let mut redis_conn = db.redis_pool.get().await?;

    // transforms exclusive to inclusive range
    let end = end - 1;
    let ids: Vec<i32> = if reversed {
        redis_conn.zrevrange(key, start as isize, end as isize)
    } else {
        redis_conn.zrange(key, start as isize, end as isize)
    }
    .await?;

    if ids.is_empty() {
        // Avoids the query building to have a `AND record_player_id IN ()` fragment
        return Ok(Vec::new());
    }

    let params = ids
        .iter()
        .map(|_| "?".to_string())
        .collect::<Vec<String>>()
        .join(",");

    let query = format!(
        "SELECT CAST(0 AS UNSIGNED) AS rank,
            p.login AS login,
            p.name AS nickname,
            {func}(time) as time,
            m.*
        FROM records r
        {join_event}
        INNER JOIN players p ON r.record_player_id = p.id
        INNER JOIN maps m ON m.id = r.map_id
        WHERE map_id = ? AND record_player_id IN ({params})
            {and_event}
        GROUP BY record_player_id
        ORDER BY time {order}, record_date ASC",
        params = params,
        func = if reversed { "MAX" } else { "MIN" },
        order = if reversed { "DESC" } else { "ASC" },
        join_event = join_event,
        and_event = and_event,
    );

    let mut query = sqlx::query_as(&query).bind(map_id);
    for id in ids {
        query = query.bind(id);
    }

    if let Some((event, edition)) = event {
        query = query.bind(event.id).bind(edition.id);
    }

    let mut records = query.fetch(&db.mysql_pool);
    let mut out = Vec::with_capacity(records.size_hint().0);
    while let Some(record) = records.next().await {
        let RecordQueryRow {
            login,
            nickname,
            time,
            map,
        } = record?;

        out.push(RankedRecord {
            rank: get_rank_or_full_update(db, &map, time, event).await? as u32,
            login,
            nickname,
            time,
        });
    }

    Ok(out)
}

pub async fn overview(
    db: Data<Database>,
    Query(body): Query<OverviewQuery>,
    event: Option<(String, u32)>,
) -> RecordsResult<impl Responder> {
    let ref map @ Map {
        id,
        linked_map,
        reversed,
        ..
    } = must::have_map(&db, &body.map_uid).await?;
    let player_id = must::have_player(&db, &body.login).await?.id;
    let map_id = linked_map.unwrap_or(id);
    let reversed = reversed.unwrap_or(false);

    let event = match event {
        Some((event_handle, edition_id)) => Some(
            must::have_event_edition_with_map(&db, &body.map_uid, event_handle, edition_id).await?,
        ),
        None => None,
    };

    let mut redis_conn = db.redis_pool.get().await.unwrap();

    // Update redis if needed
    let key = format_map_key(map_id, event.as_ref());
    let count = redis::update_leaderboard(&db, map, event.as_ref()).await? as u32;

    let mut ranked_records: Vec<RankedRecord> = vec![];

    // -- Compute display ranges
    const TOTAL_ROWS: u32 = 15;
    const NO_RECORD_ROWS: u32 = TOTAL_ROWS - 1;

    let player_rank: Option<i64> = if reversed {
        redis_conn.zrevrank(&key, player_id)
    } else {
        redis_conn.zrank(&key, player_id)
    }
    .await?;
    let player_rank = player_rank.map(|r| r as u64 as u32);

    if let Some(player_rank) = player_rank {
        // The player has a record and is in top ROWS, display ROWS records
        if player_rank < TOTAL_ROWS {
            ranked_records.extend(get_range(&db, map, (0, TOTAL_ROWS), event.as_ref()).await?);
        }
        // The player is not in the top ROWS records, display top3 and then center around the player rank
        else {
            // push top3
            ranked_records.extend(get_range(&db, map, (0, 3), event.as_ref()).await?);

            // the rest is centered around the player
            let row_minus_top3 = TOTAL_ROWS - 3;
            let range = {
                let start = player_rank - row_minus_top3 / 2;
                let end = player_rank + row_minus_top3 / 2;
                if end >= count {
                    (start - end - count, count)
                } else {
                    (start, end)
                }
            };

            ranked_records.extend(get_range(&db, map, range, event.as_ref()).await?);
        }
    }
    // The player has no record, so ROWS = ROWS - 1 to keep one last line for the player
    else {
        // There is more than ROWS record + top3,
        // So display all top ROWS records and then the last 3
        if count > NO_RECORD_ROWS {
            // top (ROWS - 1 - 3)
            get_range(&db, map, (0, NO_RECORD_ROWS - 3), event.as_ref()).await?;

            // last 3
            ranked_records.extend(get_range(&db, map, (count - 3, count), event.as_ref()).await?);
        }
        // There is enough records to display them all
        else {
            ranked_records.extend(get_range(&db, map, (0, NO_RECORD_ROWS), event.as_ref()).await?);
        }
    }

    #[derive(Serialize)]
    struct Response {
        response: Vec<RankedRecord>,
    }

    let response = ranked_records;
    json(Response { response })
}
