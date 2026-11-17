//! sc.external.services.match.v1.MatchService. Matches are stored as the protobuf message and written
//! outside this API, so this binary only reads them.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::services::r#match::v1::{
    match_service_server::{MatchService, MatchServiceServer},
    GetRequest, GetResponse, Match,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS matches (
    group_id TEXT PRIMARY KEY,
    body BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
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

struct Matches {
    db: Postgres,
}

#[tonic::async_trait]
impl MatchService for Matches {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let group = request.into_inner().group_id;
        let row = self
            .db
            .query_opt("SELECT body FROM matches WHERE group_id = $1", &[&group])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no match for group {group}")))?;
        let body: Vec<u8> = row.get("body");
        let found = Match::decode(body.as_slice()).map_err(internal)?;
        Ok(Response::new(GetResponse { r#match: Some(found) }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Matches { db: Postgres::connect(&args.database.postgres, SCHEMA).await? };
    let server = MatchServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "match listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
