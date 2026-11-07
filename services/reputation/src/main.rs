//! sc.external.services.reputation.v1.ReputationService.
//!
//! This API never changes a score. A reputation the player has not met yet starts when first read, at the
//! initial value of its scope, and every other score comes from a writer outside this API. `sc-admin seed`
//! hands the scopes and standings to `ReputationInternal.SeedReputation` (seed.rs), and a scope names the ladder a reputation is ranked on. The client reads
//! ladder and standing ids as GUIDs, and a standing can sit on several ladders, so membership has its own table.

mod seed;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_proto::services::reputation::v1::{
    get_preferences_response::PlayerPreferences,
    get_score_history_response::{scores_history::Score, ScoresHistory},
    reputation_service_server::{ReputationService, ReputationServiceServer},
    AddFavoriteEntitiesRequest, AddFavoriteEntitiesResponse, AddFavoritesRequest, AddFavoritesResponse, Drift,
    Favorites, GetPreferencesRequest, GetPreferencesResponse, GetRequest, GetResponse, GetScoreHistoryRequest,
    GetScoreHistoryResponse, QueryFavoritesRequest, QueryFavoritesResponse, QueryReputationsRequest,
    QueryReputationsResponse, RemoveFavoriteEntitiesRequest, RemoveFavoriteEntitiesResponse, RemoveFavoritesRequest,
    RemoveFavoritesResponse, Reputation, ReputationLock, SetPairsRequest, SetPairsResponse, Standing,
    VersionedReputation,
};
use sc_proto::common::api::v1::{PaginationInfo, Query};
use sc_proto::internal::reputation_internal_server::ReputationInternalServer;
use sc_runtime::player_urn;
use sc_runtime::query;
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, metadata::MetadataMap, transport::Server, Request, Response, Status};
use tracing::info;

const KIND_REPUTATION: &str = "reputation";
const KIND_ENTITY: &str = "entity";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reputations (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    subject TEXT NOT NULL,
    entity TEXT NOT NULL,
    scope TEXT NOT NULL,
    standings_id TEXT NOT NULL DEFAULT '',
    score INT NOT NULL DEFAULT 0,
    lock_min INT,
    lock_max INT,
    version BIGINT NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_drifted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (subject, entity, scope)
);
CREATE UNIQUE INDEX IF NOT EXISTS reputations_id ON reputations (id);
CREATE TABLE IF NOT EXISTS reputation_standings (
    id TEXT PRIMARY KEY,
    standings_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    min INT NOT NULL,
    drift_time BIGINT NOT NULL DEFAULT 0,
    drift_amount INT NOT NULL DEFAULT 0,
    gated BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS reputation_standings_ladder ON reputation_standings (standings_id, min);
CREATE TABLE IF NOT EXISTS reputation_scopes (
    scope TEXT PRIMARY KEY,
    standings_id TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    initial_reputation INT NOT NULL DEFAULT 0,
    ceiling INT NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS reputation_ladder_standings (
    standings_id TEXT NOT NULL,
    standing_id TEXT NOT NULL,
    PRIMARY KEY (standings_id, standing_id)
);
CREATE TABLE IF NOT EXISTS reputation_scores (
    reputation_id UUID NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    score INT NOT NULL,
    PRIMARY KEY (reputation_id, recorded_at)
);
CREATE TABLE IF NOT EXISTS reputation_preferences (
    subject TEXT PRIMARY KEY,
    id UUID NOT NULL DEFAULT gen_random_uuid()
);
CREATE TABLE IF NOT EXISTS reputation_preference_pairs (
    subject TEXT NOT NULL,
    pair_key TEXT NOT NULL,
    pair_value TEXT NOT NULL,
    PRIMARY KEY (subject, pair_key)
);
CREATE TABLE IF NOT EXISTS reputation_favorites (
    subject TEXT NOT NULL,
    kind TEXT NOT NULL,
    favorite TEXT NOT NULL,
    PRIMARY KEY (subject, kind, favorite)
);";

const REPUTATION_COLUMNS: &str = "id::text AS id, subject, entity, scope, standings_id, score, lock_min, lock_max,
    version, (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms,
    (extract(epoch FROM last_drifted_at) * 1000)::bigint AS drifted_ms";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Reputations {
    state: State,
    db: Postgres,
}

fn reputation(row: &Row) -> (Reputation, u32) {
    let lock_min: Option<i32> = row.get("lock_min");
    let lock_max: Option<i32> = row.get("lock_max");
    let reputation = Reputation {
        id: row.get("id"),
        standings_id: row.get("standings_id"),
        updated_at: row.get::<_, i64>("updated_ms") as u64,
        subject: row.get("subject"),
        entity: row.get("entity"),
        scope: row.get("scope"),
        score: row.get("score"),
        last_drifted_at: row.get::<_, i64>("drifted_ms") as u64,
        lock: lock_min.zip(lock_max).map(|(min, max)| ReputationLock { min, max }),
    };
    (reputation, row.get::<_, i64>("version") as u32)
}

fn standing(row: &Row) -> (String, Standing) {
    let standing = Standing {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        min: row.get("min"),
        drift: Some(Drift { time: row.get::<_, i64>("drift_time") as u32, amount: row.get("drift_amount") }),
        gated: row.get("gated"),
    };
    (row.get("standings_id"), standing)
}

/// The highest rung of the ladder that `score` has reached.
fn rank(ladder: &[Standing], score: i32) -> Option<&Standing> {
    ladder.iter().filter(|standing| standing.min <= score).max_by_key(|standing| standing.min)
}

fn property(versioned: &VersionedReputation, name: &str) -> Option<String> {
    let reputation = versioned.reputation.as_ref()?;
    Some(match name {
        "id" => reputation.id.clone(),
        "standings_id" => reputation.standings_id.clone(),
        "subject" => reputation.subject.clone(),
        "entity" => reputation.entity.clone(),
        "scope" => reputation.scope.clone(),
        "score" => reputation.score.to_string(),
        "updated_at" => reputation.updated_at.to_string(),
        "standing" => versioned.standing.as_ref()?.name.clone(),
        _ => return None,
    })
}

impl Reputations {
    fn subject(&self, metadata: &MetadataMap) -> Result<String, Status> {
        caller(&self.state, metadata)
            .map(|account| player_urn(account.geid))
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// Pairs each stored reputation with its standing.
    async fn versioned(&self, rows: &[Row]) -> Result<Vec<VersionedReputation>, Status> {
        let reputations: Vec<(Reputation, u32)> = rows.iter().map(reputation).collect();
        let ladders: Vec<String> = reputations.iter().map(|(r, _)| r.standings_id.clone()).collect();
        let standings = self
            .db
            .query(
                "SELECT l.standings_id, s.id, s.name, s.description, s.min, s.drift_time, s.drift_amount, s.gated
                 FROM reputation_ladder_standings l JOIN reputation_standings s ON s.id = l.standing_id
                 WHERE l.standings_id = ANY($1)",
                &[&ladders],
            )
            .await
            .map_err(internal)?;
        let mut by_ladder: HashMap<String, Vec<Standing>> = HashMap::new();
        for row in &standings {
            let (ladder, rung) = standing(row);
            by_ladder.entry(ladder).or_default().push(rung);
        }
        Ok(reputations
            .into_iter()
            .map(|(reputation, version)| {
                let ladder = by_ladder.get(&reputation.standings_id).map_or(&[][..], Vec::as_slice);
                VersionedReputation { standing: rank(ladder, reputation.score).cloned(), reputation: Some(reputation), version }
            })
            .collect())
    }

    async fn all(&self, subject: &str) -> Result<Vec<VersionedReputation>, Status> {
        let rows = self
            .db
            .query(
                &format!("SELECT {REPUTATION_COLUMNS} FROM reputations WHERE subject = $1 ORDER BY entity, scope"),
                &[&subject],
            )
            .await
            .map_err(internal)?;
        self.versioned(&rows).await
    }

    async fn favorites(&self, subject: &str) -> Result<Favorites, Status> {
        let rows = self
            .db
            .query("SELECT kind, favorite FROM reputation_favorites WHERE subject = $1 ORDER BY favorite", &[&subject])
            .await
            .map_err(internal)?;
        let mut favorites = Favorites::default();
        for row in &rows {
            let list = match row.get::<_, &str>("kind") {
                KIND_ENTITY => &mut favorites.entities,
                _ => &mut favorites.reputations,
            };
            list.push(row.get("favorite"));
        }
        Ok(favorites)
    }

    async fn add_favorites(&self, subject: &str, kind: &str, favorites: &[String]) -> Result<(), Status> {
        self.db
            .execute(
                "INSERT INTO reputation_favorites (subject, kind, favorite) SELECT $1, $2, unnest($3::text[])
                 ON CONFLICT DO NOTHING",
                &[&subject, &kind, &favorites],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    async fn remove_favorites(&self, subject: &str, kind: &str, favorites: &[String]) -> Result<(), Status> {
        self.db
            .execute(
                "DELETE FROM reputation_favorites WHERE subject = $1 AND kind = $2 AND favorite = ANY($3)",
                &[&subject, &kind, &favorites],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }
}

fn select(
    mut rows: Vec<VersionedReputation>,
    query: Option<Query>,
) -> Result<(Vec<VersionedReputation>, PaginationInfo), Status> {
    let query = query.unwrap_or_default();
    rows.retain(|row| query::matches(query.filter.as_ref(), &|name| property(row, name)));
    query::sort(&mut rows, query.sort.as_ref(), property);
    query::page(rows, query.pagination.as_ref())
}

#[tonic::async_trait]
impl ReputationService for Reputations {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        let GetRequest { entity, scope } = request.into_inner();
        self.db
            .execute(
                "WITH added AS (
                     INSERT INTO reputations (subject, entity, scope, standings_id, score)
                     SELECT $1, $2, $3, COALESCE(c.standings_id, ''), COALESCE(c.initial_reputation, 0)
                     FROM (SELECT 1) AS one LEFT JOIN reputation_scopes c ON c.scope = $3
                     ON CONFLICT DO NOTHING RETURNING id, score
                 )
                 INSERT INTO reputation_scores (reputation_id, score) SELECT id, score FROM added",
                &[&subject, &entity, &scope],
            )
            .await
            .map_err(internal)?;
        // A reputation first read before the seed has no ladder yet.
        self.db
            .execute(
                "UPDATE reputations r SET standings_id = c.standings_id FROM reputation_scopes c
                 WHERE r.subject = $1 AND r.entity = $2 AND r.scope = $3 AND r.standings_id = '' AND c.scope = r.scope",
                &[&subject, &entity, &scope],
            )
            .await
            .map_err(internal)?;
        let rows = self
            .db
            .query(
                &format!("SELECT {REPUTATION_COLUMNS} FROM reputations WHERE subject = $1 AND entity = $2 AND scope = $3"),
                &[&subject, &entity, &scope],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(GetResponse { reputation: self.versioned(&rows).await?.pop() }))
    }

    async fn query_reputations(
        &self,
        request: Request<QueryReputationsRequest>,
    ) -> Result<Response<QueryReputationsResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        let (results, page_info) = select(self.all(&subject).await?, request.into_inner().query)?;
        Ok(Response::new(QueryReputationsResponse { results, page_info: Some(page_info) }))
    }

    async fn query_favorites(
        &self,
        request: Request<QueryFavoritesRequest>,
    ) -> Result<Response<QueryFavoritesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        let favorites = self.favorites(&subject).await?;
        let mut rows = self.all(&subject).await?;
        rows.retain(|row| {
            row.reputation.as_ref().is_some_and(|r| favorites.reputations.contains(&r.id) || favorites.entities.contains(&r.entity))
        });
        let (results, page_info) = select(rows, request.into_inner().query)?;
        Ok(Response::new(QueryFavoritesResponse { results, page_info: Some(page_info) }))
    }

    async fn get_preferences(
        &self,
        request: Request<GetPreferencesRequest>,
    ) -> Result<Response<GetPreferencesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        self.db
            .execute("INSERT INTO reputation_preferences (subject) VALUES ($1) ON CONFLICT DO NOTHING", &[&subject])
            .await
            .map_err(internal)?;
        let id = self
            .db
            .query_opt("SELECT id::text AS id FROM reputation_preferences WHERE subject = $1", &[&subject])
            .await
            .map_err(internal)?
            .map(|row| row.get("id"))
            .unwrap_or_default();
        let pairs = self
            .db
            .query("SELECT pair_key, pair_value FROM reputation_preference_pairs WHERE subject = $1", &[&subject])
            .await
            .map_err(internal)?;
        let favorites = self.favorites(&subject).await?;
        Ok(Response::new(GetPreferencesResponse {
            preferences: Some(PlayerPreferences {
                id,
                player: subject,
                favorites: Some(favorites),
                preference_pairs: pairs.iter().map(|row| (row.get("pair_key"), row.get("pair_value"))).collect(),
            }),
        }))
    }

    async fn set_pairs(&self, request: Request<SetPairsRequest>) -> Result<Response<SetPairsResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        let (keys, values): (Vec<String>, Vec<String>) = request.into_inner().preference_pairs.into_iter().unzip();
        self.db
            .execute(
                "INSERT INTO reputation_preference_pairs (subject, pair_key, pair_value)
                 SELECT $1, * FROM unnest($2::text[], $3::text[])
                 ON CONFLICT (subject, pair_key) DO UPDATE SET pair_value = excluded.pair_value",
                &[&subject, &keys, &values],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(SetPairsResponse::default()))
    }

    async fn add_favorites(&self, request: Request<AddFavoritesRequest>) -> Result<Response<AddFavoritesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        Reputations::add_favorites(self, &subject, KIND_REPUTATION, &request.into_inner().favorite_reputations).await?;
        Ok(Response::new(AddFavoritesResponse::default()))
    }

    async fn remove_favorites(
        &self,
        request: Request<RemoveFavoritesRequest>,
    ) -> Result<Response<RemoveFavoritesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        Reputations::remove_favorites(self, &subject, KIND_REPUTATION, &request.into_inner().favorite_reputations).await?;
        Ok(Response::new(RemoveFavoritesResponse::default()))
    }

    async fn add_favorite_entities(
        &self,
        request: Request<AddFavoriteEntitiesRequest>,
    ) -> Result<Response<AddFavoriteEntitiesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        Reputations::add_favorites(self, &subject, KIND_ENTITY, &request.into_inner().favorite_entities).await?;
        Ok(Response::new(AddFavoriteEntitiesResponse::default()))
    }

    async fn remove_favorite_entities(
        &self,
        request: Request<RemoveFavoriteEntitiesRequest>,
    ) -> Result<Response<RemoveFavoriteEntitiesResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        Reputations::remove_favorites(self, &subject, KIND_ENTITY, &request.into_inner().favorite_entities).await?;
        Ok(Response::new(RemoveFavoriteEntitiesResponse::default()))
    }

    /// Only the caller's own reputations answer.
    async fn get_score_history(
        &self,
        request: Request<GetScoreHistoryRequest>,
    ) -> Result<Response<GetScoreHistoryResponse>, Status> {
        let subject = self.subject(request.metadata())?;
        let mut reputation_scores = Vec::new();
        for query in request.into_inner().reputation_scores {
            let days = i32::try_from(query.days).unwrap_or(i32::MAX);
            let rows = self
                .db
                .query(
                    "SELECT s.score, extract(epoch FROM s.recorded_at)::bigint AS at
                     FROM reputation_scores s JOIN reputations r ON r.id = s.reputation_id
                     WHERE r.id::text = $1 AND r.subject = $2
                       AND ($3 = 0 OR s.recorded_at > now() - make_interval(days => $3))
                     ORDER BY s.recorded_at",
                    &[&query.reputation_id, &subject, &days],
                )
                .await
                .map_err(internal)?;
            let scores = rows
                .iter()
                .map(|row| Score {
                    score: row.get::<_, i32>("score").max(0) as u64,
                    timestamp: row.get::<_, i64>("at") as u32,
                })
                .collect();
            reputation_scores.push(ScoresHistory { reputation_id: query.reputation_id, scores });
        }
        Ok(Response::new(GetScoreHistoryResponse { reputation_scores }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Reputations {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    });
    let server = ReputationServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = ReputationInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "reputation listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rung(id: &str, min: i32) -> Standing {
        Standing { id: id.into(), min, ..Default::default() }
    }

    #[test]
    fn rank_is_the_highest_rung_reached() {
        let ladder = [rung("hostile", -100), rung("neutral", 0), rung("trusted", 100)];
        assert_eq!(rank(&ladder, 50).unwrap().id, "neutral");
        assert_eq!(rank(&ladder, 100).unwrap().id, "trusted");
        assert!(rank(&ladder, -200).is_none());
    }
}
