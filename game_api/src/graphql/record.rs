use async_graphql::{dataloader::DataLoader, Context};

use crate::{
    models::{CheckpointTimes, Map, Player, RankedRecord},
    Database,
};

use super::{map::MapLoader, player::PlayerLoader};

#[async_graphql::Object]
impl RankedRecord {
    async fn rank(&self) -> i32 {
        self.rank
    }

    async fn map(&self, ctx: &Context<'_>) -> async_graphql::Result<Map> {
        ctx.data_unchecked::<DataLoader<MapLoader>>()
            .load_one(self.record.map_id)
            .await?
            .ok_or_else(|| async_graphql::Error::new("Map not found."))
    }

    async fn player(&self, ctx: &Context<'_>) -> async_graphql::Result<Player> {
        ctx.data_unchecked::<DataLoader<PlayerLoader>>()
            .load_one(self.record.player_id)
            .await?
            .ok_or_else(|| async_graphql::Error::new("Player not found."))
    }

    async fn cps_times(
        &self,
        ctx: &async_graphql::Context<'_>,
    ) -> async_graphql::Result<Vec<CheckpointTimes>> {
        let db = &ctx.data_unchecked::<Database>().mysql_pool;

        Ok(sqlx::query_as!(
            CheckpointTimes,
            "SELECT * FROM checkpoint_times WHERE record_id = ? AND map_id = ? ORDER BY cp_num",
            self.record.id,
            self.record.map_id,
        )
        .fetch_all(db)
        .await?)
    }

    async fn time(&self) -> i32 {
        self.record.time
    }

    async fn respawn_count(&self) -> i32 {
        self.record.respawn_count
    }

    async fn try_count(&self) -> i32 {
        self.record.respawn_count
    }

    async fn record_date(&self) -> chrono::NaiveDateTime {
        self.record.record_date
    }

    async fn flags(&self) -> u32 {
        self.record.flags
    }
}
