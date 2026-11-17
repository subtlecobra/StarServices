//! sc.external.services.land_claim.v1.LandClaimService.

mod area;

use std::{collections::BTreeMap, net::SocketAddr, process::ExitCode};

use area::{overlaps, Point, Rectangle};
use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row, ToSql};
use sc_economy::{player_urn, timestamp_of};
use sc_proto::{
    google::r#type::LatLng,
    services::land_claim::v1::{
        land_claim::Tax,
        land_claim_service_server::{LandClaimService, LandClaimServiceServer},
        LandClaim, Plot, QueryAreaClaimsRequest, QueryAreaClaimsResponse, QueryOwnedClaimsRequest,
        QueryOwnedClaimsResponse,
    },
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS land_claims (
    id TEXT PRIMARY KEY,
    location_id BIGINT NOT NULL,
    owner_urn TEXT NOT NULL,
    claim_type INT NOT NULL,
    last_paid_at TIMESTAMPTZ,
    last_processed_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS land_claims_by_location ON land_claims (location_id);
CREATE INDEX IF NOT EXISTS land_claims_by_owner ON land_claims (owner_urn);
CREATE TABLE IF NOT EXISTS land_claim_plots (
    id TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL,
    bottom DOUBLE PRECISION NOT NULL,
    left_edge DOUBLE PRECISION NOT NULL,
    top DOUBLE PRECISION NOT NULL,
    right_edge DOUBLE PRECISION NOT NULL
);
CREATE INDEX IF NOT EXISTS land_claim_plots_by_claim ON land_claim_plots (claim_id);";
const SELECT: &str = "
SELECT c.id, c.location_id, c.owner_urn, c.claim_type,
       (extract(epoch FROM c.last_paid_at) * 1000)::bigint AS paid_ms,
       (extract(epoch FROM c.last_processed_at) * 1000)::bigint AS processed_ms,
       p.id AS plot_id, p.bottom, p.left_edge, p.top, p.right_edge
FROM land_claims c LEFT JOIN land_claim_plots p ON p.claim_id = c.id";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Claims {
    state: State,
    db: Postgres,
}

fn plot(row: &Row) -> Option<Plot> {
    Some(Plot {
        id: row.get::<_, Option<String>>("plot_id")?,
        bottom_left: Some(LatLng { latitude: row.get("bottom"), longitude: row.get("left_edge") }),
        top_right: Some(LatLng { latitude: row.get("top"), longitude: row.get("right_edge") }),
    })
}

/// One row per plot, so claims are put back together here.
fn claims(rows: &[Row]) -> Vec<LandClaim> {
    let mut claims: BTreeMap<String, LandClaim> = BTreeMap::new();
    for row in rows {
        let id: String = row.get("id");
        let claim = claims.entry(id.clone()).or_insert_with(|| LandClaim {
            id,
            location_id: row.get::<_, i64>("location_id") as u32,
            owner_urn: row.get("owner_urn"),
            plots: Vec::new(),
            r#type: row.get("claim_type"),
            tax_history: Some(Tax {
                last_paid_at: timestamp_of(row, "paid_ms"),
                last_processed_at: timestamp_of(row, "processed_ms"),
            }),
        });
        claim.plots.extend(plot(row));
    }
    claims.into_values().collect()
}

fn rectangle(plot: &Plot) -> Rectangle {
    let (bottom_left, top_right) = (plot.bottom_left.unwrap_or_default(), plot.top_right.unwrap_or_default());
    Rectangle {
        bottom_left: Point { latitude: bottom_left.latitude, longitude: bottom_left.longitude },
        top_right: Point { latitude: top_right.latitude, longitude: top_right.longitude },
    }
}

impl Claims {
    async fn load(&self, condition: &str, parameter: &(dyn ToSql + Sync)) -> Result<Vec<LandClaim>, Status> {
        let sql = format!("{SELECT} WHERE {condition} ORDER BY c.id, p.id");
        Ok(claims(&self.db.query(&sql, &[parameter]).await.map_err(internal)?))
    }
}

#[tonic::async_trait]
impl LandClaimService for Claims {
    async fn query_area_claims(
        &self,
        request: Request<QueryAreaClaimsRequest>,
    ) -> Result<Response<QueryAreaClaimsResponse>, Status> {
        let filter = request.into_inner().area_filter.ok_or_else(|| Status::invalid_argument("missing area"))?;
        let area: Vec<Point> = filter
            .area
            .iter()
            .map(|point| Point { latitude: point.latitude, longitude: point.longitude })
            .collect();
        let mut claims = self.load("c.location_id = $1", &i64::from(filter.location_id)).await?;
        if !area.is_empty() {
            claims.retain(|claim| claim.plots.iter().any(|plot| overlaps(&area, &rectangle(plot))));
        }
        Ok(Response::new(QueryAreaClaimsResponse { claims }))
    }

    async fn query_owned_claims(
        &self,
        request: Request<QueryOwnedClaimsRequest>,
    ) -> Result<Response<QueryOwnedClaimsResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let claims = self.load("c.owner_urn = $1", &player_urn(account.geid)).await?;
        Ok(Response::new(QueryOwnedClaimsResponse { claims }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Claims {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    };
    let server = LandClaimServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "land claim listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
