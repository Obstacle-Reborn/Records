use std::time::SystemTime;

use async_graphql::SimpleObject;
use deadpool_redis::redis::{AsyncCommands, SetExpiry, SetOptions};
use sqlx::{pool::PoolConnection, MySql};
#[cfg(feature = "tracing")]
use tracing::Instrument;

use crate::{
    error::RecordsResult,
    escaped::Escaped,
    models, must,
    redis_key::{
        mappack_key, mappack_lb_key, mappack_map_last_rank, mappack_nb_map_key,
        mappack_player_map_finished_key, mappack_player_rank_avg_key, mappack_player_ranks_key,
        mappack_player_worst_rank_key, mappack_time_key, mappacks_key, no_ttl_mappacks,
        NoTtlMappacks,
    },
    update_ranks::get_rank_or_full_update,
    RedisConnection,
};

#[derive(SimpleObject, Default, Clone, Debug)]
pub struct Rank {
    pub rank: i32,
    pub map_idx: usize,
}

#[derive(SimpleObject, Debug)]
pub struct PlayerScore {
    pub player_id: u32,
    pub login: String,
    pub name: Escaped,
    pub ranks: Vec<Rank>,
    pub score: f64,
    pub maps_finished: usize,
    pub rank: u32,
    pub worst: Rank,
}

#[derive(SimpleObject, Debug)]
pub struct MappackMap {
    pub map: Escaped,
    pub map_id: Escaped,
    pub last_rank: i32,
    #[graphql(skip)]
    pub records: Option<Vec<RankedRecordRow>>,
}

#[derive(SimpleObject)]
struct PlayerScoresDetails {
    pub a: i32,
}

#[derive(Debug)]
pub struct MappackScores {
    pub maps: Vec<MappackMap>,
    pub scores: Vec<PlayerScore>,
}

#[derive(sqlx::FromRow, Debug)]
struct RecordRow {
    #[sqlx(flatten)]
    pub record: models::Record,
    pub player_id2: u32,
    pub player_login: String,
    #[sqlx(try_from = "String")]
    pub player_name: Escaped,
}

#[derive(Debug)]
pub struct RankedRecordRow {
    rank: i32,
    record: RecordRow,
}

pub async fn persist_mappack(
    redis_conn: &mut RedisConnection,
    mappack_id: &str,
) -> RecordsResult<()> {
    redis_conn.sadd(mappacks_key(), mappack_id).await?;
    redis_conn.sadd(no_ttl_mappacks(), mappack_id).await?;
    redis_conn.persist(mappack_key(mappack_id)).await?;
    Ok(())
}

pub async fn update_mappack(
    mappack_id: &str,
    mysql_conn: &mut PoolConnection<MySql>,
    redis_conn: &mut RedisConnection,
) -> RecordsResult<()> {
    // Calculate the scores

    let key = mappack_key(mappack_id);

    // FIXME: avoid to recreate the key because we most likely already have it before calling this function
    #[cfg(feature = "tracing")]
    let key_str = key.to_string();

    #[cfg(feature = "tracing")]
    let scores = {
        // Spans the process scores calculation process
        let span = tracing::info_span!("calc_scores", mappack_key = key_str);
        async { calc_scores(mappack_id, mysql_conn, redis_conn).await }
            .instrument(span)
            .await?
    };
    #[cfg(not(feature = "tracing"))]
    let scores = { calc_scores(mappack_id, mysql_conn, redis_conn).await? };

    // Early return if the mappack has expired
    let Some(scores) = scores else {
        return Ok(());
    };

    // Then save them to the Redis database for cache-handling

    let no_ttl: Vec<String> = redis_conn.smembers(NoTtlMappacks).await?;
    let mappack_ttl = no_ttl
        .iter()
        .all(|x| x != mappack_id)
        .then_some(crate::env().mappack_ttl);

    #[cfg(feature = "tracing")]
    {
        // Spans the score storage process
        let span = tracing::info_span!("saving scores", mappack_key = key_str, ttl = mappack_ttl);
        async { save(mappack_id, scores, mappack_ttl, redis_conn).await }
            .instrument(span)
            .await?;
    }
    #[cfg(not(feature = "tracing"))]
    {
        save(mappack_id, scores, mappack_ttl, redis_conn).await?;
    }

    // And we save it to the registered mappacks set.
    // If the mappack has a TTL, its member will be removed from the set when attempting to retrieve its maps.
    redis_conn.sadd(mappacks_key(), mappack_id).await?;

    Ok(())
}

// FIXME: this involves a lot of repetitive code
async fn save(
    mappack_id: &str,
    scores: MappackScores,
    mappack_ttl: Option<i64>,
    redis_conn: &mut RedisConnection,
) -> RecordsResult<()> {
    #[cfg(feature = "tracing")]
    tracing::info!("Saving scores");

    let set_options = SetOptions::default();

    let set_options = match mappack_ttl {
        Some(ex) => {
            // Update expiration time of some keys btw
            redis_conn.expire(mappack_key(mappack_id), ex).await?;
            redis_conn.expire(mappack_lb_key(mappack_id), ex).await?;

            set_options.with_expiration(SetExpiry::EX(ex as _))
        }
        None => {
            // Persist some keys btw
            redis_conn.persist(mappack_key(mappack_id)).await?;
            redis_conn.persist(mappack_lb_key(mappack_id)).await?;

            set_options
        }
    };

    // --- Save the number of maps of the campaign

    let key = mappack_nb_map_key(mappack_id);
    redis_conn
        .set_options(&key, scores.maps.len(), set_options)
        .await?;

    if mappack_ttl.is_none() {
        redis_conn.persist(&key).await?;
    }

    for map in &scores.maps {
        // --- Save the last rank on each map

        redis_conn
            .set_options(
                mappack_map_last_rank(mappack_id, &map.map_id),
                map.last_rank,
                set_options,
            )
            .await?;

        if mappack_ttl.is_none() {
            redis_conn
                .persist(mappack_map_last_rank(mappack_id, &map.map_id))
                .await?;
        }
    }

    for score in scores.scores {
        redis_conn
            .zadd(mappack_lb_key(mappack_id), score.player_id, score.rank)
            .await?;

        // --- Save the rank average

        let rank_avg = ((score.score + f64::EPSILON) * 100.).round() / 100.;

        redis_conn
            .set_options(
                mappack_player_rank_avg_key(mappack_id, score.player_id),
                rank_avg,
                set_options,
            )
            .await?;

        // --- Save the amount of finished map

        redis_conn
            .set_options(
                mappack_player_map_finished_key(mappack_id, score.player_id),
                score.maps_finished,
                set_options,
            )
            .await?;

        // --- Save their worst rank

        redis_conn
            .set_options(
                mappack_player_worst_rank_key(mappack_id, score.player_id),
                score.worst.rank,
                set_options,
            )
            .await?;

        if let Some(ttl) = mappack_ttl {
            redis_conn
                .expire(mappack_player_ranks_key(mappack_id, score.player_id), ttl)
                .await?;
        } else {
            redis_conn
                .persist(mappack_player_ranks_key(mappack_id, score.player_id))
                .await?;
            redis_conn
                .persist(mappack_player_rank_avg_key(mappack_id, score.player_id))
                .await?;
            redis_conn
                .persist(mappack_player_map_finished_key(mappack_id, score.player_id))
                .await?;
            redis_conn
                .persist(mappack_player_worst_rank_key(mappack_id, score.player_id))
                .await?;
        }

        for (game_id, rank) in score
            .ranks
            .into_iter()
            .map(|rank| (&scores.maps[rank.map_idx].map_id, rank.rank))
        {
            // --- Save their rank on each map

            redis_conn
                .zadd(
                    mappack_player_ranks_key(mappack_id, score.player_id),
                    game_id,
                    rank,
                )
                .await?;
        }
    }

    if let Ok(time) = SystemTime::UNIX_EPOCH.elapsed() {
        redis_conn
            .set(mappack_time_key(mappack_id), time.as_secs())
            .await?;
        if let Some(ttl) = mappack_ttl {
            redis_conn.expire(mappack_time_key(mappack_id), ttl).await?;
        } else {
            redis_conn.persist(mappack_time_key(mappack_id)).await?;
        }
    }

    #[cfg(feature = "tracing")]
    tracing::info!("Finished saving");

    Ok(())
}

/// Returns an `Option` because the mappack may have expired.
async fn calc_scores(
    mappack_id: &str,
    mysql_conn: &mut PoolConnection<MySql>,
    redis_conn: &mut RedisConnection,
) -> RecordsResult<Option<MappackScores>> {
    let mappack_key = mappack_key(mappack_id);
    let mappack_uids: Vec<String> = redis_conn.smembers(&mappack_key).await?;

    let mut maps = Vec::with_capacity(mappack_uids.len().max(5));

    let mappack = if mappack_uids.is_empty() {
        // If the mappack is empty, it means either that it's an invalid/unknown mappack ID,
        // or that its TTL has expired. So we remove its entry in the registered mappacks set.
        // The other keys related to this mappack were set with a TTL so they should
        // be deleted too.
        let _: i32 = redis_conn.srem(mappacks_key(), mappack_id).await?;
        return Ok(None);
    } else {
        let mut out = Vec::with_capacity(mappack_uids.len());
        for map_uid in &mappack_uids {
            let map = must::have_map(&mut **mysql_conn, map_uid).await?;
            maps.push(MappackMap {
                map: map.name.clone().into(),
                map_id: map.game_id.clone().into(),
                last_rank: 0,
                records: None,
            });
            out.push(map);
        }
        out
    };

    let mut scores = Vec::<PlayerScore>::with_capacity(mappack.len());

    for (i, map) in mappack.iter().enumerate() {
        let res = sqlx::query_as::<_, RecordRow>(
            "SELECT r.*, p.id as player_id2, p.login as player_login, p.name as player_name
            FROM global_records r
            INNER JOIN players p ON p.id = r.record_player_id
            WHERE map_id = ?
            ORDER BY time ASC",
        )
        .bind(map.id)
        // Use of fetch_all instead of fetch
        // because we can't mutably-borrow `mysql_conn` twice at same time
        .fetch_all(&mut **mysql_conn)
        .await?;

        let mut records = Vec::with_capacity(res.len());

        for record in res {
            if !scores.iter().any(|p| p.player_id == record.player_id2) {
                scores.push(PlayerScore {
                    player_id: record.player_id2,
                    login: record.player_login.clone(),
                    name: record.player_name.clone(),
                    ranks: Vec::new(),
                    score: 0.,
                    maps_finished: 0,
                    rank: 0,
                    worst: Default::default(),
                });
            }

            let record = RankedRecordRow {
                rank: get_rank_or_full_update(
                    (mysql_conn, redis_conn),
                    map.id,
                    record.record.time,
                    None,
                )
                .await?,
                record,
            };
            records.push(record);
        }

        maps[i].records = Some(records);
    }

    let mut map_number = 1;

    for (map_idx, map) in maps.iter_mut().enumerate() {
        let records = map.records.take().unwrap();

        let last_rank = records.iter().map(|p| p.rank).max().unwrap_or(0);

        for record in records {
            let player = scores
                .iter_mut()
                .find(|p| p.player_id == record.record.player_id2)
                .unwrap();

            player.ranks.push(Rank {
                rank: record.rank,
                map_idx,
            });
            map.last_rank = last_rank;

            player.maps_finished += 1;
        }

        for player in &mut scores {
            if player.ranks.len() < map_number {
                player.ranks.push(Rank {
                    rank: last_rank + 1,
                    map_idx,
                });
                map.last_rank = last_rank;
            }
        }

        map_number += 1;
    }

    for player in &mut scores {
        player.ranks.sort_by(|a, b| {
            ((a.rank / maps[a.map_idx].last_rank.max(1)
                - b.rank / maps[b.map_idx].last_rank.max(1))
                + (a.rank - b.rank) / 1000)
                .cmp(&0)
        });

        player.worst = player
            .ranks
            .iter()
            .reduce(|a, b| if a.rank > b.rank { a } else { b })
            .unwrap()
            .clone();

        player.score = player
            .ranks
            .iter()
            .fold(0., |acc, rank| acc + rank.rank as f64)
            / player.ranks.len() as f64;
    }

    scores.sort_by(|a, b| {
        if a.maps_finished != b.maps_finished {
            b.maps_finished.cmp(&a.maps_finished)
        } else {
            a.score.partial_cmp(&b.score).unwrap()
        }
    });

    let mut old_score = 0.;
    let mut old_finishes = 0;
    let mut old_rank = 0;

    for (rank, player) in scores.iter_mut().enumerate() {
        player.rank = if old_score.eq(&player.score) && old_finishes == player.maps_finished {
            old_rank
        } else {
            rank as u32 + 1
        };

        old_score = player.score;
        old_finishes = player.maps_finished;
        old_rank = player.rank;
    }

    Ok(Some(MappackScores { maps, scores }))
}
