use async_graphql::ID;
use deadpool_redis::{redis::AsyncCommands, Connection as RedisConnection};
use sqlx::{mysql, FromRow, MySqlConnection};

use crate::models::Map;
use crate::utils::format_map_key;
use crate::{
    models::{self, Record},
    redis, RecordsResult,
};

#[derive(FromRow)]
pub struct RecordAttr {
    #[sqlx(flatten)]
    pub record: Record,
    #[sqlx(flatten)]
    pub map: Map,
}

pub fn decode_id(id: Option<&ID>) -> Option<u32> {
    let parts: Vec<&str> = id?.split(':').collect();
    if parts.len() != 3 || parts[0] != "v0" || (parts[1] != "Map" && parts[1] != "Player") {
        println!(
            "invalid, len: {}, [0]: {}, [1]: {}",
            parts.len(),
            parts[0],
            parts[1]
        );
        None
    } else {
        parts[2].parse::<u32>().ok()
    }
}

pub fn connections_append_query_string_page(
    query: &mut String,
    has_where_clause: bool,
    after: Option<u32>,
    before: Option<u32>,
) {
    if before.is_some() || after.is_some() {
        query.push_str(if !has_where_clause { "WHERE " } else { "and " });

        match (before, after) {
            (Some(_), Some(_)) => query.push_str("id > ? and id < ? "), // after, before
            (Some(_), _) => query.push_str("id < ? "),                  // before
            (_, Some(_)) => query.push_str("id > ? "),                  // after
            _ => unreachable!(),
        }
    }
}

pub fn connections_append_query_string_order(
    query: &mut String,
    first: Option<usize>,
    last: Option<usize>,
) {
    if first.is_some() {
        query.push_str("ORDER BY id ASC LIMIT ? "); // first
    } else if last.is_some() {
        query.push_str("ORDER BY id DESC LIMIT ? "); // last
    }
}

pub fn connections_append_query_string(
    query: &mut String,
    has_where_clause: bool,
    after: Option<u32>,
    before: Option<u32>,
    first: Option<usize>,
    last: Option<usize>,
) {
    connections_append_query_string_page(query, has_where_clause, after, before);
    connections_append_query_string_order(query, first, last);
}

pub type SqlQuery<'q> = sqlx::query::Query<
    'q,
    mysql::MySql,
    <mysql::MySql as sqlx::database::HasArguments<'q>>::Arguments,
>;

pub fn connections_bind_query_parameters_page(
    mut query: SqlQuery,
    after: Option<u32>,
    before: Option<u32>,
) -> SqlQuery {
    match (before, after) {
        (Some(before), Some(after)) => query = query.bind(before).bind(after),
        (Some(before), _) => query = query.bind(before),
        (_, Some(after)) => query = query.bind(after),
        _ => {}
    }
    query
}

pub fn connections_bind_query_parameters_order(
    mut query: SqlQuery,
    first: Option<usize>,
    last: Option<usize>,
) -> SqlQuery {
    // Actual limits are N+1 to check if previous/next pages
    if let Some(first) = first {
        query = query.bind(first as u32 + 1);
    } else if let Some(last) = last {
        query = query.bind(last as u32 + 1);
    }

    query
}

pub fn connections_bind_query_parameters(
    mut query: SqlQuery,
    after: Option<u32>,
    before: Option<u32>,
    first: Option<usize>,
    last: Option<usize>,
) -> SqlQuery {
    query = connections_bind_query_parameters_page(query, after, before);
    query = connections_bind_query_parameters_order(query, first, last);
    query
}

pub fn connections_pages_info(
    results_count: usize,
    first: Option<usize>,
    last: Option<usize>,
) -> (bool, bool) {
    let mut has_previous_page = false;
    let mut has_next_page = false;

    if let Some(first) = first {
        if results_count == first + 1 {
            has_next_page = true;
        }
    }

    if let Some(last) = last {
        if results_count == last + 1 {
            has_previous_page = true;
        }
    }

    (has_previous_page, has_next_page)
}

/// Get the rank of a time in a map, or fully updates its leaderboard if not found.
///
/// The full update means a delete of the Redis key then a reinsertion of all the records.
/// This may be called when the SQL and Redis databases had the same amount of records on a map,
/// but the times were not corresponding. It generally happens after a database migration.
pub async fn get_rank_or_full_update(
    (db, redis_conn): (&mut MySqlConnection, &mut RedisConnection),
    map @ models::Map {
        id: map_id,
        reversed,
        ..
    }: &models::Map,
    time: i32,
    event: Option<&(models::Event, models::EventEdition)>,
) -> RecordsResult<i32> {
    async fn get_rank(
        redis_conn: &mut RedisConnection,
        key: &str,
        time: i32,
        reversed: bool,
    ) -> RecordsResult<Option<i32>> {
        let player_id: Vec<u32> = if reversed {
            redis_conn.zrevrangebyscore_limit(key, time, time, 0, 1)
        } else {
            redis_conn.zrangebyscore_limit(key, time, time, 0, 1)
        }
        .await?;

        match player_id.first() {
            Some(id) => {
                let rank: i32 = if reversed {
                    redis_conn.zrevrank(key, id)
                } else {
                    redis_conn.zrank(key, id)
                }
                .await?;
                Ok(Some(rank + 1))
            }
            None => Ok(None),
        }
    }

    let reversed = reversed.unwrap_or(false);
    let key = &format_map_key(*map_id, event);

    match get_rank(redis_conn, key, time, reversed).await? {
        Some(rank) => Ok(rank),
        None => {
            redis_conn.del(key).await?;
            redis::update_leaderboard((db, redis_conn), map, event).await?;
            let rank = get_rank(redis_conn, key, time, reversed)
                .await?
                .unwrap_or_else(|| {
                    // TODO: make a more clear message showing diff
                    panic!(
                        "redis leaderboard for (`{key}`) should be updated \
                        at this point"
                    )
                });
            Ok(rank)
        }
    }
}
