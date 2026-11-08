//! `scbackend.internal.ReputationInternal`: the scopes, standings and ladders from the game data, which
//! `sc-admin seed` reads from the archive and hands over. Rows are replaced by key, so a repeat changes
//! nothing and a new build updates what it finds.

use sc_db::{internal, Postgres, ToSql};
use sc_api::internal::{
    reputation_internal_server::ReputationInternal, ReputationLadderStanding, ReputationScope, ReputationStanding,
    SeedReputationRequest, SeedResponse,
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Reputations;

const SEED_ROWS: usize = 500;

fn column<'a, T, V>(rows: &'a [T], field: impl Fn(&'a T) -> V) -> Vec<V> {
    rows.iter().map(field).collect()
}

async fn scopes(db: &Postgres, rows: &[ReputationScope]) -> Result<u64, String> {
    let params: [&(dyn ToSql + Sync); 6] = [
        &column(rows, |row| row.scope.as_str()),
        &column(rows, |row| row.standings_id.as_str()),
        &column(rows, |row| row.display_name.as_str()),
        &column(rows, |row| row.description.as_str()),
        &column(rows, |row| row.initial_reputation),
        &column(rows, |row| row.ceiling),
    ];
    db.execute(
        "INSERT INTO reputation_scopes (scope, standings_id, display_name, description, initial_reputation, ceiling)
         SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::int[], $6::int[])
         ON CONFLICT (scope) DO UPDATE SET standings_id = excluded.standings_id, display_name = excluded.display_name,
             description = excluded.description, initial_reputation = excluded.initial_reputation,
             ceiling = excluded.ceiling",
        &params,
    )
    .await
}

async fn standings(db: &Postgres, rows: &[ReputationStanding]) -> Result<u64, String> {
    let params: [&(dyn ToSql + Sync); 8] = [
        &column(rows, |row| row.id.as_str()),
        &column(rows, |row| row.standings_id.as_str()),
        &column(rows, |row| row.name.as_str()),
        &column(rows, |row| row.description.as_str()),
        &column(rows, |row| row.min),
        &column(rows, |row| row.drift_seconds),
        &column(rows, |row| row.drift_amount),
        &column(rows, |row| row.gated),
    ];
    db.execute(
        "INSERT INTO reputation_standings (id, standings_id, name, description, min, drift_time, drift_amount, gated)
         SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::int[], $6::bigint[], $7::int[], $8::bool[])
         ON CONFLICT (id) DO UPDATE SET standings_id = excluded.standings_id, name = excluded.name,
             description = excluded.description, min = excluded.min, drift_time = excluded.drift_time,
             drift_amount = excluded.drift_amount, gated = excluded.gated",
        &params,
    )
    .await
}

async fn ladder(db: &Postgres, rows: &[ReputationLadderStanding]) -> Result<u64, String> {
    let params: [&(dyn ToSql + Sync); 2] =
        [&column(rows, |row| row.standings_id.as_str()), &column(rows, |row| row.standing_id.as_str())];
    db.execute(
        "INSERT INTO reputation_ladder_standings (standings_id, standing_id)
         SELECT * FROM unnest($1::text[], $2::text[]) ON CONFLICT DO NOTHING",
        &params,
    )
    .await
}

pub async fn seed(db: &Postgres, request: &SeedReputationRequest) -> Result<u64, Status> {
    if request.scopes.iter().any(|scope| scope.scope.is_empty())
        || request.standings.iter().any(|standing| standing.id.is_empty())
    {
        return Err(Status::invalid_argument("a scope or standing with no id"));
    }
    let mut written = 0;
    for chunk in request.scopes.chunks(SEED_ROWS) {
        written += scopes(db, chunk).await.map_err(internal)?;
    }
    for chunk in request.standings.chunks(SEED_ROWS) {
        written += standings(db, chunk).await.map_err(internal)?;
    }
    for chunk in request.ladder.chunks(SEED_ROWS) {
        written += ladder(db, chunk).await.map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl ReputationInternal for Reputations {
    async fn seed_reputation(&self, request: Request<SeedReputationRequest>) -> Result<Response<SeedResponse>, Status> {
        let rows = seed(&self.db, &request.into_inner()).await?;
        info!(rows, "reputation seeded");
        Ok(Response::new(SeedResponse { rows }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-reputation -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn seeding_replaces_by_key() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let id = format!("test-scope-{}", std::process::id());
        let scope = |ceiling| ReputationScope { scope: id.clone(), standings_id: "l".into(), ceiling, ..Default::default() };
        let standing = ReputationStanding { id: id.clone(), standings_id: "l".into(), name: "n".into(), ..Default::default() };
        let ladder = ReputationLadderStanding { standings_id: id.clone(), standing_id: id.clone() };
        let request = SeedReputationRequest { scopes: vec![scope(5)], standings: vec![standing], ladder: vec![ladder] };
        assert_eq!(seed(&db, &request).await.unwrap(), 3);
        let again = SeedReputationRequest { scopes: vec![scope(9)], ..request.clone() };
        seed(&db, &again).await.unwrap();
        let ceiling = db.query_opt("SELECT ceiling FROM reputation_scopes WHERE scope = $1", &[&id]).await.unwrap();
        assert_eq!(ceiling.unwrap().get::<_, i32>(0), 9);
        db.execute("DELETE FROM reputation_scopes WHERE scope = $1", &[&id]).await.unwrap();
        db.execute("DELETE FROM reputation_standings WHERE id = $1", &[&id]).await.unwrap();
        db.execute("DELETE FROM reputation_ladder_standings WHERE standings_id = $1", &[&id]).await.unwrap();
        let empty = SeedReputationRequest { scopes: vec![ReputationScope::default()], ..Default::default() };
        assert_eq!(seed(&db, &empty).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    }
}
