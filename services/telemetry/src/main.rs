//! sc.game.telemetry.v1.GameTelemetryService.
//!
//! In the real game the client hosts this service (CGameTelemetryServer) and sends frames to local
//! tools. No frames reach the backend, so a stream here stays open and silent, and connections are
//! counted.

use std::{net::SocketAddr, pin::Pin, process::ExitCode};

use clap::Parser;
use futures_util::{stream, Stream};
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::sc::game::telemetry::v1::{
    game_telemetry_service_server::{GameTelemetryService, GameTelemetryServiceServer},
    TelemetryStreamRequest, TelemetryStreamResponse,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS telemetry_connections (
    day DATE PRIMARY KEY,
    connections BIGINT NOT NULL
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

struct Telemetry {
    db: Postgres,
}

#[tonic::async_trait]
impl GameTelemetryService for Telemetry {
    type TelemetryStreamStream = Pin<Box<dyn Stream<Item = Result<TelemetryStreamResponse, Status>> + Send>>;

    async fn telemetry_stream(
        &self,
        _request: Request<TelemetryStreamRequest>,
    ) -> Result<Response<Self::TelemetryStreamStream>, Status> {
        self.db
            .execute(
                "INSERT INTO telemetry_connections (day, connections) VALUES (current_date, 1)
                 ON CONFLICT (day) DO UPDATE SET connections = telemetry_connections.connections + 1",
                &[],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(Box::pin(stream::pending())))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let server = GameTelemetryServiceServer::new(Telemetry { db })
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "telemetry listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
