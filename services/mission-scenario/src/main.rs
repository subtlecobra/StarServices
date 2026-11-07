//! sc.external.services.mission_scenario.v1.ScenarioService. Scenarios and their progress are written
//! outside this API, so it only reads them. The scenarios of the game data come in through
//! `MissionScenarioInternal` (seed.rs), from `sc-admin seed`.

mod seed;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::{
    common::mission::v1::ScenarioStatus,
    internal::mission_scenario_internal_server::MissionScenarioInternalServer,
    services::mission_scenario::v1::{
        query_scenario_status_response::Body,
        scenario_service_server::{ScenarioService, ScenarioServiceServer},
        GetContributorScenarioProgressRequest, GetContributorScenarioProgressResponse, QueryScenarioStatusRequest,
        QueryScenarioStatusResponse,
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scenarios (
    template_id TEXT PRIMARY KEY,
    status INT NOT NULL
);
CREATE TABLE IF NOT EXISTS scenario_progress (
    template_id TEXT NOT NULL,
    progress_key TEXT NOT NULL,
    value BIGINT NOT NULL,
    PRIMARY KEY (template_id, progress_key)
);
CREATE TABLE IF NOT EXISTS scenario_contributor_progress (
    template_id TEXT NOT NULL,
    contributor_id BIGINT NOT NULL,
    progress_key TEXT NOT NULL,
    value BIGINT NOT NULL,
    PRIMARY KEY (template_id, contributor_id, progress_key)
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

struct Scenarios {
    db: Postgres,
}

impl Scenarios {
    /// Stored statuses by template. A template with no row counts as disabled.
    async fn statuses(&self, templates: &[String]) -> Result<HashMap<String, i32>, Status> {
        let rows = self
            .db
            .query("SELECT template_id, status FROM scenarios WHERE template_id = ANY($1)", &[&templates])
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(|row| (row.get("template_id"), row.get("status"))).collect())
    }
}

fn status_of(statuses: &HashMap<String, i32>, template: &str) -> i32 {
    statuses.get(template).copied().unwrap_or(ScenarioStatus::Disabled as i32)
}

#[tonic::async_trait]
impl ScenarioService for Scenarios {
    async fn get_contributor_scenario_progress(
        &self,
        request: Request<GetContributorScenarioProgressRequest>,
    ) -> Result<Response<GetContributorScenarioProgressResponse>, Status> {
        let request = request.into_inner();
        let template = request.template_id;
        let contributor = request.contributor_id as i64;
        let overall = self
            .db
            .query("SELECT progress_key, value FROM scenario_progress WHERE template_id = $1", &[&template])
            .await
            .map_err(internal)?;
        let own = self
            .db
            .query(
                "SELECT progress_key, value FROM scenario_contributor_progress
                 WHERE template_id = $1 AND contributor_id = $2",
                &[&template, &contributor],
            )
            .await
            .map_err(internal)?;
        let statuses = self.statuses(std::slice::from_ref(&template)).await?;
        Ok(Response::new(GetContributorScenarioProgressResponse {
            overall_progress: overall.iter().map(|row| (row.get("progress_key"), row.get("value"))).collect(),
            contributor_progress: own.iter().map(|row| (row.get("progress_key"), row.get("value"))).collect(),
            is_scenario_active: status_of(&statuses, &template) == ScenarioStatus::Running as i32,
        }))
    }

    async fn query_scenario_status(
        &self,
        request: Request<QueryScenarioStatusRequest>,
    ) -> Result<Response<QueryScenarioStatusResponse>, Status> {
        let templates = request.into_inner().template_ids;
        let statuses = self.statuses(&templates).await?;
        let results = templates
            .into_iter()
            .map(|template_id| Body { status: status_of(&statuses, &template_id), template_id })
            .collect();
        Ok(Response::new(QueryScenarioStatusResponse { results }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Scenarios { db: Postgres::connect(&args.database.postgres, SCHEMA).await? });
    let server = ScenarioServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = MissionScenarioInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "mission scenario listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_templates_are_disabled() {
        let statuses = HashMap::from([("known".to_string(), ScenarioStatus::Running as i32)]);
        assert_eq!(status_of(&statuses, "known"), ScenarioStatus::Running as i32);
        assert_eq!(status_of(&statuses, "other"), ScenarioStatus::Disabled as i32);
    }
}
