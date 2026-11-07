//! sc.external.services.discipline.v1.DisciplineService. Sanctions are added by the server operator,
//! so the table is empty unless someone has been sanctioned.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::discipline::{
    discipline_service_server::{DisciplineService, DisciplineServiceServer},
    QuerySanctionsRequest, QuerySanctionsResponse, Sanction,
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sanctions (
    id TEXT PRIMARY KEY,
    target_geid BIGINT NOT NULL,
    sanction BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sanctions_target ON sanctions (target_geid);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Discipline {
    state: State,
    db: Postgres,
}

#[tonic::async_trait]
impl DisciplineService for Discipline {
    async fn query_sanctions(
        &self,
        request: Request<QuerySanctionsRequest>,
    ) -> Result<Response<QuerySanctionsResponse>, Status> {
        let Some(account) = caller(&self.state, request.metadata()) else {
            return Ok(Response::new(QuerySanctionsResponse::default()));
        };
        let rows = self
            .db
            .query("SELECT sanction FROM sanctions WHERE target_geid = $1", &[&(account.geid as i64)])
            .await
            .map_err(internal)?;
        let results = rows
            .iter()
            .map(|row| Sanction::decode(row.get::<_, Vec<u8>>("sanction").as_slice()).map_err(internal))
            .collect::<Result<_, _>>()?;
        Ok(Response::new(QuerySanctionsResponse { results, ..Default::default() }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Discipline {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    };
    let server = DisciplineServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "discipline listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
