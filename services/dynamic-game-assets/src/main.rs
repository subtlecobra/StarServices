//! sc.external.services.dynamic_game_assets.v1.DynamicGameAssetsService. A screen, such as a loading
//! screen, shows the newest assignment that has reached its `valid_from`. Rows are operator data.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost_types::Timestamp;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_proto::services::dynamic_game_assets::v1::{
    dynamic_game_assets_service_server::{DynamicGameAssetsService, DynamicGameAssetsServiceServer},
    AssetAssignment, GetAssetForScreenRequest, GetAssetForScreenResponse, ListActiveAssetsRequest,
    ListActiveAssetsResponse,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS dynamic_game_assets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    screen_id TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    valid_from TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS dynamic_game_assets_screen ON dynamic_game_assets (screen_id);";
const COLUMNS: &str = "id::text AS id, screen_id, asset_id,
    (extract(epoch FROM valid_from) * 1000)::bigint AS valid_from_ms";
const ACTIVE: &str = "valid_from IS NULL OR valid_from <= now()";
const NEWEST: &str = "COALESCE(valid_from, created_at) DESC, created_at DESC";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Assets {
    db: Postgres,
}

fn timestamp(millis: i64) -> Timestamp {
    Timestamp { seconds: millis.div_euclid(1000), nanos: millis.rem_euclid(1000) as i32 * 1_000_000 }
}

fn assignment(row: &Row) -> AssetAssignment {
    AssetAssignment {
        id: row.get("id"),
        screen_id: row.get("screen_id"),
        asset_id: row.get("asset_id"),
        valid_from: row.get::<_, Option<i64>>("valid_from_ms").map(timestamp),
    }
}

#[tonic::async_trait]
impl DynamicGameAssetsService for Assets {
    async fn get_asset_for_screen(
        &self,
        request: Request<GetAssetForScreenRequest>,
    ) -> Result<Response<GetAssetForScreenResponse>, Status> {
        let screen = request.into_inner().screen_id;
        let sql = format!(
            "SELECT {COLUMNS} FROM dynamic_game_assets WHERE screen_id = $1 AND ({ACTIVE}) ORDER BY {NEWEST} LIMIT 1"
        );
        let row = self.db.query_opt(&sql, &[&screen]).await.map_err(internal)?;
        Ok(Response::new(GetAssetForScreenResponse { asset: row.as_ref().map(assignment) }))
    }

    async fn list_active_assets(
        &self,
        _: Request<ListActiveAssetsRequest>,
    ) -> Result<Response<ListActiveAssetsResponse>, Status> {
        let sql = format!(
            "SELECT DISTINCT ON (screen_id) {COLUMNS} FROM dynamic_game_assets WHERE {ACTIVE}
             ORDER BY screen_id, {NEWEST}"
        );
        let rows = self.db.query(&sql, &[]).await.map_err(internal)?;
        Ok(Response::new(ListActiveAssetsResponse { assets: rows.iter().map(assignment).collect() }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Assets { db: Postgres::connect(&args.database.postgres, SCHEMA).await? };
    let server = DynamicGameAssetsServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "dynamic game assets listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::timestamp;

    #[test]
    fn splits_milliseconds_into_seconds_and_nanos() {
        let time = timestamp(1_500);
        assert_eq!((time.seconds, time.nanos), (1, 500_000_000));
        let before_epoch = timestamp(-1);
        assert_eq!((before_epoch.seconds, before_epoch.nanos), (-1, 999_000_000));
    }

    /// Against a real YugabyteDB: `SC_TEST_POSTGRES=<url> cargo test -p sc-dynamic-game-assets -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn the_newest_assignment_that_has_started_wins() {
        use super::*;

        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, SCHEMA).await.unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
        let screen = format!("test-screen-{stamp}");
        let insert = "INSERT INTO dynamic_game_assets (screen_id, asset_id, valid_from)
                      VALUES ($1, 'old', now() - interval '2 hours'),
                             ($1, 'current', now() - interval '1 hour'),
                             ($1, 'future', now() + interval '1 hour')";
        db.execute(insert, &[&screen]).await.unwrap();

        let service = Assets { db: db.clone() };
        let request = Request::new(GetAssetForScreenRequest { screen_id: screen.clone() });
        let asset = service.get_asset_for_screen(request).await.unwrap().into_inner().asset.unwrap();
        assert_eq!(asset.asset_id, "current");
        assert!(asset.valid_from.is_some());
        let request = Request::new(GetAssetForScreenRequest { screen_id: format!("{screen}-none") });
        assert!(service.get_asset_for_screen(request).await.unwrap().into_inner().asset.is_none());
        let assets = service.list_active_assets(Request::new(ListActiveAssetsRequest {})).await.unwrap().into_inner().assets;
        let mine: Vec<_> = assets.iter().filter(|asset| asset.screen_id == screen).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].asset_id, "current");

        db.execute("DELETE FROM dynamic_game_assets WHERE screen_id = $1", &[&screen]).await.unwrap();
    }
}
