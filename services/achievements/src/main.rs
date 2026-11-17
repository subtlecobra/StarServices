//! sc.external.services.achievements.v1.AchievementService.
//!
//! The client has no code calling this service and no catalogue of achievements reaches the backend, so an
//! id is registered the first time it is reported. A row in achievement_definitions supplies its name and
//! target. Progress only rises and an unlock happens once, so a repeated call or batch changes nothing.

use std::{collections::HashMap, net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost_types::Timestamp;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::services::achievements::v1::{
    achievement_event::EventType,
    achievement_service_server::{AchievementService, AchievementServiceServer},
    AchievementEvent, CompleteAchievementRequest, CompleteAchievementResponse, GetAchievementStatsRequest,
    GetAchievementStatsResponse, GetPlayerAchievementsRequest, GetPlayerAchievementsResponse, PlatformType,
    PlayerAchievement, UpdateProgressRequest, UpdateProgressResponse, UploadBatchRequest, UploadBatchResponse,
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, metadata::MetadataMap, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS achievement_definitions (
    achievement_id TEXT PRIMARY KEY,
    game_id INT NOT NULL DEFAULT 0,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    target INT,
    category TEXT NOT NULL DEFAULT '',
    is_secret BOOLEAN NOT NULL DEFAULT false,
    icon_url TEXT
);
CREATE TABLE IF NOT EXISTS achievement_progress (
    geid BIGINT NOT NULL,
    achievement_id TEXT NOT NULL,
    progress INT NOT NULL DEFAULT 0,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (geid, achievement_id)
);
CREATE INDEX IF NOT EXISTS achievement_progress_by_id ON achievement_progress (achievement_id);
CREATE TABLE IF NOT EXISTS achievement_platforms (
    geid BIGINT NOT NULL,
    achievement_id TEXT NOT NULL,
    platform INT NOT NULL,
    PRIMARY KEY (geid, achievement_id, platform)
);
CREATE INDEX IF NOT EXISTS achievement_platforms_by_id ON achievement_platforms (achievement_id);
CREATE TABLE IF NOT EXISTS achievement_batches (
    batch_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    geid BIGINT NOT NULL,
    session_id TEXT NOT NULL,
    total_events INT NOT NULL,
    processed INT NOT NULL,
    uploaded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Achievements {
    state: State,
    db: Postgres,
}

struct Progress {
    stored: i32,
    completed: bool,
    newly_unlocked: bool,
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp { seconds: ms.div_euclid(1000), nanos: (ms.rem_euclid(1000) * 1_000_000) as i32 }
}

fn millis(time: Option<&Timestamp>) -> Option<i64> {
    time.map(|time| time.seconds * 1000 + i64::from(time.nanos) / 1_000_000)
}

/// 0 means the caller. Nobody may report progress for another player.
fn writable_player(account: &Account, player_id: u64) -> Result<i64, Status> {
    if player_id != 0 && player_id != account.geid {
        return Err(Status::permission_denied("not your player"));
    }
    Ok(account.geid as i64)
}

fn share(unlocks: i64, players: i64) -> f32 {
    if players == 0 {
        0.0
    } else {
        unlocks as f32 / players as f32
    }
}

impl Achievements {
    fn account(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    async fn register(&self, geid: i64, id: &str, platform: i32) -> Result<(), Status> {
        self.db
            .execute(
                "INSERT INTO achievement_definitions (achievement_id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING",
                &[&id],
            )
            .await
            .map_err(internal)?;
        if platform != PlatformType::Unspecified as i32 {
            self.db
                .execute(
                    "INSERT INTO achievement_platforms (geid, achievement_id, platform) VALUES ($1, $2, $3)
                     ON CONFLICT DO NOTHING",
                    &[&geid, &id, &platform],
                )
                .await
                .map_err(internal)?;
        }
        Ok(())
    }

    /// Unlocks the achievement unless it already is. Returns the unlock time and whether this call did it.
    async fn unlock(&self, geid: i64, id: &str, at: Option<i64>) -> Result<(i64, bool), Status> {
        let unlocked = self
            .db
            .query_opt(
                "UPDATE achievement_progress
                 SET completed_at = COALESCE(to_timestamp($3::bigint / 1000.0), now()), updated_at = now()
                 WHERE geid = $1 AND achievement_id = $2 AND completed_at IS NULL
                 RETURNING (extract(epoch FROM completed_at) * 1000)::bigint AS at",
                &[&geid, &id, &at],
            )
            .await
            .map_err(internal)?;
        if let Some(row) = unlocked {
            return Ok((row.get("at"), true));
        }
        let row = self
            .db
            .query_opt(
                "SELECT (extract(epoch FROM completed_at) * 1000)::bigint AS at FROM achievement_progress
                 WHERE geid = $1 AND achievement_id = $2",
                &[&geid, &id],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("progress row disappeared"))?;
        Ok((row.get("at"), false))
    }

    /// Raises progress and unlocks the achievement when its target is known and reached.
    async fn progress(&self, geid: i64, id: &str, platform: i32, value: i32, at: Option<i64>) -> Result<Progress, Status> {
        self.register(geid, id, platform).await?;
        let row = self
            .db
            .query_opt(
                "INSERT INTO achievement_progress AS p (geid, achievement_id, progress) VALUES ($1, $2, $3)
                 ON CONFLICT (geid, achievement_id) DO UPDATE
                     SET progress = GREATEST(p.progress, excluded.progress), updated_at = now()
                 RETURNING p.progress, p.completed_at IS NOT NULL AS completed,
                     (SELECT target FROM achievement_definitions d WHERE d.achievement_id = $2) AS target",
                &[&geid, &id, &value],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("upsert returned nothing"))?;
        let stored: i32 = row.get("progress");
        let completed: bool = row.get("completed");
        let target: Option<i32> = row.get("target");
        if !completed && target.is_some_and(|target| stored >= target) {
            let (_, newly_unlocked) = self.unlock(geid, id, at).await?;
            return Ok(Progress { stored, completed: true, newly_unlocked });
        }
        Ok(Progress { stored, completed, newly_unlocked: false })
    }

    async fn complete(&self, geid: i64, id: &str, platform: i32, last: Option<i32>, at: Option<i64>) -> Result<(i64, bool), Status> {
        self.progress(geid, id, platform, last.unwrap_or(0), at).await?;
        self.unlock(geid, id, at).await
    }

    async fn platforms(&self, geid: i64) -> Result<HashMap<String, Vec<i32>>, Status> {
        let rows = self
            .db
            .query("SELECT achievement_id, platform FROM achievement_platforms WHERE geid = $1", &[&geid])
            .await
            .map_err(internal)?;
        let mut platforms: HashMap<String, Vec<i32>> = HashMap::new();
        for row in &rows {
            platforms.entry(row.get("achievement_id")).or_default().push(row.get("platform"));
        }
        Ok(platforms)
    }
}

#[tonic::async_trait]
impl AchievementService for Achievements {
    async fn update_progress(
        &self,
        request: Request<UpdateProgressRequest>,
    ) -> Result<Response<UpdateProgressResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let geid = writable_player(&account, request.player_id)?;
        if request.achievement_id.is_empty() {
            return Err(Status::invalid_argument("missing achievement id"));
        }
        let at = millis(request.updated_at.as_ref());
        let progress =
            self.progress(geid, &request.achievement_id, request.platform_type, request.current_progress, at).await?;
        Ok(Response::new(UpdateProgressResponse {
            success: true,
            achievement_id: request.achievement_id,
            stored_progress: progress.stored,
            completed: progress.completed,
        }))
    }

    async fn complete_achievement(
        &self,
        request: Request<CompleteAchievementRequest>,
    ) -> Result<Response<CompleteAchievementResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let geid = writable_player(&account, request.player_id)?;
        if request.achievement_id.is_empty() {
            return Err(Status::invalid_argument("missing achievement id"));
        }
        let at = millis(request.completed_at.as_ref());
        let (unlocked, newly_unlocked) =
            self.complete(geid, &request.achievement_id, request.platform_type, request.final_progress, at).await?;
        Ok(Response::new(CompleteAchievementResponse {
            success: true,
            achievement_id: request.achievement_id,
            unlocked_at: Some(timestamp(unlocked)),
            newly_unlocked,
        }))
    }

    async fn upload_batch(&self, request: Request<UploadBatchRequest>) -> Result<Response<UploadBatchResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let geid = writable_player(&account, request.player_id)?;
        let mut reply = UploadBatchResponse { total_events: request.events.len() as i32, ..Default::default() };
        for AchievementEvent { r#type, achievement_id, progress, final_progress, timestamp } in request.events {
            let at = millis(timestamp.as_ref());
            let kind = EventType::try_from(r#type).unwrap_or(EventType::Unspecified);
            if achievement_id.is_empty() || kind == EventType::Unspecified {
                reply.invalid_achievement_ids.push(achievement_id);
                continue;
            }
            if kind == EventType::Progress {
                let id = &achievement_id;
                let done = self.progress(geid, id, request.platform_type, progress.unwrap_or(0), at).await?;
                reply.progress_updates += 1;
                reply.newly_unlocked += i32::from(done.newly_unlocked);
            } else {
                let (_, newly) = self.complete(geid, &achievement_id, request.platform_type, final_progress, at).await?;
                reply.completions += 1;
                reply.newly_unlocked += i32::from(newly);
            }
            reply.processed += 1;
        }
        let row = self
            .db
            .query_opt(
                "INSERT INTO achievement_batches (geid, session_id, total_events, processed) VALUES ($1, $2, $3, $4)
                 RETURNING batch_id::text AS id",
                &[&geid, &request.session_id, &reply.total_events, &reply.processed],
            )
            .await
            .map_err(internal)?;
        reply.batch_id = row.map(|row| row.get("id")).unwrap_or_default();
        Ok(Response::new(reply))
    }

    async fn get_player_achievements(
        &self,
        request: Request<GetPlayerAchievementsRequest>,
    ) -> Result<Response<GetPlayerAchievementsResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let geid = if request.player_id == 0 { account.geid as i64 } else { request.player_id as i64 };
        let rows = self
            .db
            .query(
                "SELECT d.achievement_id, d.name, d.description, d.target, d.category, d.is_secret, d.icon_url,
                        COALESCE(p.progress, 0) AS progress,
                        (extract(epoch FROM p.completed_at) * 1000)::bigint AS unlocked_ms
                 FROM achievement_definitions d
                 LEFT JOIN achievement_progress p ON p.achievement_id = d.achievement_id AND p.geid = $1
                 WHERE $2 = 0 OR d.game_id = 0 OR d.game_id = $2
                 ORDER BY d.achievement_id",
                &[&geid, &request.game_id],
            )
            .await
            .map_err(internal)?;
        let mut platforms = self.platforms(geid).await?;
        let mut achievements: Vec<PlayerAchievement> = rows
            .iter()
            .map(|row| {
                let id: String = row.get("achievement_id");
                let unlocked: Option<i64> = row.get("unlocked_ms");
                PlayerAchievement {
                    platforms: platforms.remove(&id).unwrap_or_default(),
                    achievement_id: id,
                    name: row.get("name"),
                    description: row.get("description"),
                    completed: unlocked.is_some(),
                    unlocked_at: unlocked.map(timestamp),
                    progress: row.get("progress"),
                    target: row.get("target"),
                    category: row.get("category"),
                    is_secret: row.get("is_secret"),
                    icon_url: row.get("icon_url"),
                }
            })
            .collect();
        if let Some(platform) = request.platform_filter {
            achievements.retain(|achievement| achievement.platforms.contains(&platform));
        }
        if request.completed_only == Some(true) {
            achievements.retain(|achievement| achievement.completed);
        }
        Ok(Response::new(GetPlayerAchievementsResponse {
            total: achievements.len() as i32,
            unlocked: achievements.iter().filter(|achievement| achievement.completed).count() as i32,
            achievements,
        }))
    }

    async fn get_achievement_stats(
        &self,
        request: Request<GetAchievementStatsRequest>,
    ) -> Result<Response<GetAchievementStatsResponse>, Status> {
        let id = request.into_inner().achievement_id;
        let row = self
            .db
            .query_opt(
                "SELECT (SELECT count(*) FROM achievement_progress
                         WHERE achievement_id = $1 AND completed_at IS NOT NULL) AS unlocks,
                        (SELECT count(DISTINCT geid) FROM achievement_progress) AS players",
                &[&id],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("count returned nothing"))?;
        let per_platform = self
            .db
            .query(
                "SELECT a.platform, count(*) AS unlocks FROM achievement_platforms a
                 JOIN achievement_progress p ON p.geid = a.geid AND p.achievement_id = a.achievement_id
                 WHERE a.achievement_id = $1 AND p.completed_at IS NOT NULL GROUP BY a.platform",
                &[&id],
            )
            .await
            .map_err(internal)?;
        let unlocks: i64 = row.get("unlocks");
        let mut reply = GetAchievementStatsResponse {
            total_unlocks: unlocks as i32,
            completion_rate: share(unlocks, row.get("players")),
            ..Default::default()
        };
        for row in &per_platform {
            let count = row.get::<_, i64>("unlocks") as i32;
            match PlatformType::try_from(row.get::<_, i32>("platform")) {
                Ok(PlatformType::Psn) => reply.psn_unlocks = count,
                Ok(PlatformType::Steam) => reply.steam_unlocks = count,
                Ok(PlatformType::Rsi) => reply.rsi_unlocks = count,
                _ => {}
            }
        }
        reply.achievement_id = id;
        Ok(Response::new(reply))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Achievements {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    };
    let server = AchievementServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "achievements listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> Account {
        Account { account_id: 1, handle: "a".into(), geid: 5, created_at_ms: 0 }
    }

    #[test]
    fn only_the_caller_can_be_written() {
        assert_eq!(writable_player(&account(), 0).unwrap(), 5);
        assert_eq!(writable_player(&account(), 5).unwrap(), 5);
        assert!(writable_player(&account(), 6).is_err());
    }

    #[test]
    fn timestamps_round_trip_through_milliseconds() {
        assert_eq!(millis(Some(&timestamp(1_500))), Some(1_500));
        assert_eq!(millis(None), None);
    }

    #[test]
    fn no_players_is_a_zero_rate() {
        assert_eq!(share(0, 0), 0.0);
        assert_eq!(share(1, 4), 0.25);
    }
}
