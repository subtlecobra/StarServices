//! sc.external.services.mission_token.v1.MissionTokenService. Tokens are granted outside this API, so this
//! binary only reads them. Contract prerequisites are checked against what it returns.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost_types::Timestamp;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_proto::{
    common::mission::v1::MissionToken,
    services::mission_token::v1::{
        mission_token_service_server::{MissionTokenService, MissionTokenServiceServer},
        QueryTokensRequest, QueryTokensResponse,
    },
};
use sc_runtime::player_urn;
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mission_tokens (
    id TEXT NOT NULL,
    owner_urn TEXT NOT NULL,
    count INT NOT NULL DEFAULT 1,
    reason TEXT NOT NULL DEFAULT '',
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_urn, id)
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

struct Tokens {
    state: State,
    db: Postgres,
}

fn token(row: &Row) -> MissionToken {
    let expiry: Option<i64> = row.get("expiry_ms");
    MissionToken {
        id: row.get("id"),
        expiry: expiry.map(|ms| Timestamp { seconds: ms.div_euclid(1000), nanos: (ms.rem_euclid(1000) * 1_000_000) as i32 }),
        created_at: row.get::<_, i64>("created_ms") as u64,
        reason: row.get("reason"),
        owner_urn: row.get("owner_urn"),
        count: row.get("count"),
    }
}

#[tonic::async_trait]
impl MissionTokenService for Tokens {
    async fn query_tokens(&self, request: Request<QueryTokensRequest>) -> Result<Response<QueryTokensResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let rows = self
            .db
            .query(
                "SELECT id, owner_urn, count, reason,
                        (extract(epoch FROM expires_at) * 1000)::bigint AS expiry_ms,
                        (extract(epoch FROM created_at) * 1000)::bigint AS created_ms
                 FROM mission_tokens
                 WHERE owner_urn = $1 AND (expires_at IS NULL OR expires_at > now())",
                &[&player_urn(account.geid)],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(QueryTokensResponse { tokens: rows.iter().map(token).collect() }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Tokens {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    };
    let server = MissionTokenServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "mission token listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_urns_follow_the_client_format() {
        assert_eq!(player_urn(42), "urn:sc:global:player:geid:42");
    }
}
