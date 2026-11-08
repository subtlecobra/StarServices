//! `scbackend.internal.MissionScenarioInternal`: the scenarios in the game data, which `sc-admin seed`
//! reads from the archive and hands over. Only adds, as an operator may have started one since.

use sc_db::{internal, Postgres};
use sc_api::internal::{mission_scenario_internal_server::MissionScenarioInternal, SeedResponse, SeedScenariosRequest};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Scenarios;

const SEED_ROWS: usize = 500;

pub async fn seed(db: &Postgres, request: &SeedScenariosRequest) -> Result<u64, Status> {
    if request.scenarios.iter().any(|scenario| scenario.template_id.is_empty()) {
        return Err(Status::invalid_argument("a scenario with no template"));
    }
    let mut written = 0;
    for rows in request.scenarios.chunks(SEED_ROWS) {
        let templates: Vec<&str> = rows.iter().map(|row| row.template_id.as_str()).collect();
        let statuses: Vec<i32> = rows.iter().map(|row| row.status).collect();
        written += db
            .execute(
                "INSERT INTO scenarios (template_id, status) SELECT * FROM unnest($1::text[], $2::int[])
                 ON CONFLICT DO NOTHING",
                &[&templates, &statuses],
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl MissionScenarioInternal for Scenarios {
    async fn seed_scenarios(&self, request: Request<SeedScenariosRequest>) -> Result<Response<SeedResponse>, Status> {
        let rows = seed(&self.db, &request.into_inner()).await?;
        info!(rows, "scenarios seeded");
        Ok(Response::new(SeedResponse { rows }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_api::internal::ScenarioSeed;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-mission-scenario -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn seeding_keeps_a_status_an_operator_set() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let id = format!("test-scenario-{}", std::process::id());
        let request = |status| SeedScenariosRequest { scenarios: vec![ScenarioSeed { template_id: id.clone(), status }] };
        assert_eq!(seed(&db, &request(1)).await.unwrap(), 1);
        assert_eq!(seed(&db, &request(2)).await.unwrap(), 0);
        let row = db.query_opt("SELECT status FROM scenarios WHERE template_id = $1", &[&id]).await;
        assert_eq!(row.unwrap().unwrap().get::<_, i32>(0), 1);
        db.execute("DELETE FROM scenarios WHERE template_id = $1", &[&id]).await.unwrap();
    }
}
