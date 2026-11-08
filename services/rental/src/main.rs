//! sc.external.services.rental.v1.ExternalRentalService. Owns `rentals` and `rental_history`.
//! sc-eatransaction creates and extends rentals through `RentalInternal` (internal.rs), where the admin site also lists
//! and ends them, and this service reads, starts and ends them. The rental's entitlement belongs to sc-entitlement, so its expiry is
//! moved and revoked through `EntitlementInternal`.

mod internal;

use std::{net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row, ToSql};
use sc_economy::{
    account_urn,
    command::{command_id, Push},
    db, reply_stream,
    query::{compile, Field, Kind},
    timestamp_of,
    transaction::{retrying, Failure},
    ReplyStream,
};
use sc_api::internal::{ entitlement_internal_client::EntitlementInternalClient, entity_graph_internal_client::EntityGraphInternalClient, GrantItemsRequest, GrantedItem, rental_internal_server::RentalInternalServer, RevokeExpiredRentalsRequest, SetRentalExpiryRequest, };
use sc_proto::{
    services::rental::v1::{
    external_rental_service_server::{ExternalRentalService, ExternalRentalServiceServer},
    QueryRentalsRequest, QueryRentalsResponse, QueryRentalsStreamRequest, QueryRentalsStreamResponse, Rental,
    RentalLogAction, RentalStatus, StartRequest, StartResponse,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

/// Until 2026-11 sc-eatransaction, sc-rental and the entitlement services all created these from one
/// shared bundle. The definitions are unchanged, so an existing database is left as it is.
/// `rental_commands` records each extension by its caller's key, so a repeat extends once.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rentals (
    id TEXT PRIMARY KEY,
    status INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    began_at TIMESTAMPTZ,
    expire_at TIMESTAMPTZ,
    duration INT NOT NULL,
    account_id BIGINT NOT NULL,
    player_id BIGINT NOT NULL,
    entitlement_urn TEXT NOT NULL,
    renewals INT NOT NULL DEFAULT 0,
    entity_class_guid TEXT NOT NULL,
    target_inventory TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS rentals_by_player ON rentals (player_id);
CREATE INDEX IF NOT EXISTS rentals_by_expiry ON rentals (expire_at) WHERE status = 2;
CREATE TABLE IF NOT EXISTS rental_history (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    rental_id TEXT NOT NULL,
    player_id BIGINT NOT NULL,
    log_action INT NOT NULL,
    log_message TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS rental_commands (
    key TEXT PRIMARY KEY,
    rental_id TEXT NOT NULL,
    expire_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);";
/// Rentals ended per sweep. More wait for the next one.
const EXPIRY_BATCH: i64 = 500;

const START_RESPONSE: &str = "sc.external.services.rental.v1.StartResponse";
const EXPIRY_SWEEP: Duration = Duration::from_secs(30);
const COLUMNS: &str = "id, status, duration, account_id, player_id, entitlement_urn, renewals, entity_class_guid,
    (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
    (extract(epoch FROM began_at) * 1000)::bigint AS began_ms,
    (extract(epoch FROM expire_at) * 1000)::bigint AS expire_ms";

const FIELDS: &[Field] = &[
    Field { property: "id", expression: "id", kind: Kind::Text },
    Field {
        property: "status",
        expression: "status",
        kind: Kind::Enum(|name| RentalStatus::from_str_name(name).map(|value| value as i32)),
    },
    Field { property: "entity_class_guid", expression: "entity_class_guid", kind: Kind::Text },
    Field { property: "entitlement_urn", expression: "entitlement_urn", kind: Kind::Text },
    Field { property: "duration", expression: "duration", kind: Kind::Number },
    Field { property: "renewals", expression: "renewals", kind: Kind::Number },
    Field { property: "account_urn", expression: "'urn:sc:platform:account:integer:' || account_id", kind: Kind::Text },
    Field { property: "player_id", expression: "player_id", kind: Kind::Number },
    Field {
        property: "created_at",
        expression: "(extract(epoch FROM created_at) * 1000)::bigint",
        kind: Kind::Number,
    },
    Field { property: "began_at", expression: "(extract(epoch FROM began_at) * 1000)::bigint", kind: Kind::Number },
    Field { property: "expire_at", expression: "(extract(epoch FROM expire_at) * 1000)::bigint", kind: Kind::Number },
];

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of the push service.
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
    /// Address of sc-entitlement, which owns the rentals' entitlements.
    #[arg(long, env = "SCBACKEND_ENTITLEMENT", default_value = "http://sc-entitlement:50051")]
    entitlement: String,
    /// Address of the sc-entitygraph of the players' region, which keeps their items. With none a started rental makes no
    /// item.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH")]
    entitygraph: Option<String>,
}

struct Rentals {
    state: State,
    db: Postgres,
    push: Push,
    entitlements: EntitlementInternalClient<Channel>,
    /// sc-entitygraph, which keeps the players' items. Without it a started rental makes none.
    graph: Option<EntityGraphInternalClient<Channel>>,
}

fn rental(row: &Row) -> Rental {
    Rental {
        id: row.get("id"),
        status: row.get("status"),
        created_at: timestamp_of(row, "created_ms"),
        began_at: timestamp_of(row, "began_ms"),
        expire_at: timestamp_of(row, "expire_ms"),
        duration: row.get::<_, i32>("duration") as u32,
        account_urn: account_urn(row.get::<_, i64>("account_id") as u32),
        player_id: row.get::<_, i64>("player_id") as u64,
        entitlement_urn: row.get("entitlement_urn"),
        renewals: row.get::<_, i32>("renewals") as u32,
        entity_class_guid: row.get("entity_class_guid"),
    }
}

/// Ends what has run out. The entitlements are revoked first, so a failure leaves the rental active and
/// the next sweep tries again. Revoking twice changes nothing, and a rental renewed meanwhile is kept, as
/// both its own expiry and its entitlement's have moved on.
async fn expire_rentals(db: &Postgres, entitlements: &EntitlementInternalClient<Channel>) -> Result<u64, String> {
    let due = db
        .query(
            "SELECT id, entitlement_urn FROM rentals WHERE status = $1 AND expire_at <= now() ORDER BY expire_at LIMIT $2",
            &[&(RentalStatus::Active as i32), &EXPIRY_BATCH],
        )
        .await?;
    if due.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = due.iter().map(|row| row.get("id")).collect();
    let urns: Vec<String> = due.iter().map(|row| row.get("entitlement_urn")).collect();
    entitlements
        .clone()
        .revoke_expired_rentals(RevokeExpiredRentalsRequest { urns })
        .await
        .map_err(|status| format!("revoking entitlements: {status}"))?;
    db.execute(
        "WITH ended AS (
             UPDATE rentals SET status = $1 WHERE id = ANY($3) AND status = $2 AND expire_at <= now()
             RETURNING id, player_id)
         INSERT INTO rental_history (rental_id, player_id, log_action)
         SELECT id, player_id, $4 FROM ended",
        &[&(RentalStatus::Finished as i32), &(RentalStatus::Active as i32), &ids, &(RentalLogAction::Expired as i32)],
    )
    .await
}

async fn sweep_expired(db: Postgres, entitlements: EntitlementInternalClient<Channel>) {
    let mut ticks = tokio::time::interval(EXPIRY_SWEEP);
    loop {
        ticks.tick().await;
        match expire_rentals(&db, &entitlements).await {
            Ok(0) => {}
            Ok(ended) => info!(ended, "rentals expired"),
            Err(error) => warn!(%error, "rental expiry failed"),
        }
    }
}

impl Rentals {
    fn player(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// Starting twice is fine, because a retried command must not fail or move the expiry. Returns the
    /// entitlement and the expiry in milliseconds, which the entitlement is then given.
    async fn start(
        &self,
        geid: i64,
        id: &str,
        log_message: &str,
        target_inventory: &str,
    ) -> Result<(String, i64, String), Failure> {
        let mut client = self.db.client().await.map_err(internal)?;
        let transaction = db!(client.transaction());
        let started = db!(transaction.query_opt(
            "UPDATE rentals SET status = $3, began_at = now(), expire_at = now() + duration * interval '1 day',
                 target_inventory = $4
             WHERE id = $1 AND player_id = $2 AND status = $5
             RETURNING entitlement_urn, entity_class_guid, (extract(epoch FROM expire_at) * 1000)::bigint AS expire_ms",
            &[&id, &geid, &(RentalStatus::Active as i32), &target_inventory, &(RentalStatus::Available as i32)],
        ));
        let Some(started) = started else {
            let existing = db!(transaction.query_opt(
                "SELECT status, entitlement_urn, entity_class_guid, (extract(epoch FROM expire_at) * 1000)::bigint AS expire_ms
                 FROM rentals WHERE id = $1 AND player_id = $2",
                &[&id, &geid],
            ))
            .ok_or_else(|| Status::not_found(format!("rental {id}")))?;
            if existing.get::<_, i32>("status") != RentalStatus::Active as i32 {
                return Err(Status::failed_precondition("rental has finished").into());
            }
            return Ok((
                existing.get("entitlement_urn"),
                existing.get::<_, Option<i64>>("expire_ms").unwrap_or(0),
                existing.get("entity_class_guid"),
            ));
        };
        db!(transaction.execute(
            "INSERT INTO rental_history (rental_id, player_id, log_action, log_message) VALUES ($1, $2, $3, $4)",
            &[&id, &geid, &(RentalLogAction::Started as i32), &log_message],
        ));
        db!(transaction.commit());
        Ok((started.get("entitlement_urn"), started.get("expire_ms"), started.get("entity_class_guid")))
    }

    /// The rented item, a new one of the rental's class in the inventory the player named, once per rental. A failure to
    /// reach sc-entitygraph is the client's to retry, which asks again for the same item; one it refuses (a class that
    /// is not seeded, an inventory it will not take) is logged, since a retry would be refused the same way.
    async fn make_item(&self, player: u64, rental: &str, class_guid: &str, inventory: &str) -> Result<(), Status> {
        let Some(mut graph) = self.graph.clone() else { return Ok(()) };
        if inventory.is_empty() || class_guid.is_empty() {
            return Ok(());
        }
        let request = GrantItemsRequest {
            command_id: format!("rental:{rental}"),
            owner_id: player,
            inventory_id: inventory.to_string(),
            items: vec![GrantedItem { entity_class_guid: class_guid.to_string(), stack_size: 1 }],
            reason: format!("rental {rental}"),
        };
        match graph.grant_items(request).await {
            Ok(reply) => {
                info!(rental, geids = ?reply.into_inner().geids, "rented item made");
                Ok(())
            }
            Err(status) if matches!(status.code(), tonic::Code::Unavailable | tonic::Code::DeadlineExceeded) => Err(status),
            Err(status) => {
                warn!(rental, %status, "no item for the rental");
                Ok(())
            }
        }
    }
}

#[tonic::async_trait]
impl ExternalRentalService for Rentals {
    type QueryRentalsStreamStream = ReplyStream<QueryRentalsStreamResponse>;

    async fn start(&self, request: Request<StartRequest>) -> Result<Response<StartResponse>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        command_id(request.arguments.as_ref())?;
        let body = request.body.ok_or_else(|| Status::invalid_argument("missing body"))?;
        let geid = account.geid as i64;
        let (urn, expire_ms, class_guid) =
            retrying(|| Rentals::start(self, geid, &body.id, &body.log_message, &body.target_inventory)).await?;
        // After every start, a repeated one included, so a failure here is mended by the client's retry.
        if expire_ms > 0 {
            let expiry = SetRentalExpiryRequest {
                urn,
                player_id: account.geid,
                rental_expiry_ms: expire_ms,
                reactivate: false,
                log_message: body.log_message.clone(),
                command_id: request.arguments.as_ref().map(|arguments| arguments.id.clone()).unwrap_or_default(),
            };
            self.entitlements.clone().set_rental_expiry(expiry).await?;
        }
        self.make_item(account.geid, &body.id, &class_guid, &body.target_inventory).await?;
        info!(handle = %account.handle, rental = %body.id, "rental started");
        let arguments = request.arguments.unwrap_or_default();
        self.push.complete(account.account_id, &arguments, START_RESPONSE, &StartResponse::default()).await;
        Ok(Response::new(StartResponse::default()))
    }

    async fn query_rentals(
        &self,
        request: Request<QueryRentalsRequest>,
    ) -> Result<Response<QueryRentalsResponse>, Status> {
        let account = self.player(request.metadata())?;
        let query = request.into_inner().query;
        let compiled = compile(query.as_ref(), FIELDS, 2, "created_at, id")?;
        let geid = account.geid as i64;
        let mut parameters: Vec<&(dyn ToSql + Sync)> = vec![&geid];
        parameters.extend(compiled.parameters.iter().map(|value| value as &(dyn ToSql + Sync)));
        let sql = format!("SELECT {COLUMNS} FROM rentals WHERE player_id = $1 AND {} {}", compiled.condition, compiled.tail);
        let mut rows = self.db.query(&sql, &parameters).await.map_err(internal)?;
        let page_info = compiled.page_of(&mut rows);
        Ok(Response::new(QueryRentalsResponse {
            page_info: Some(page_info),
            results: rows.iter().map(rental).collect(),
        }))
    }

    async fn query_rentals_stream(
        &self,
        request: Request<QueryRentalsStreamRequest>,
    ) -> Result<Response<Self::QueryRentalsStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let filter = request.into_inner().filter.unwrap_or_default();
        let sql = format!(
            "SELECT {COLUMNS} FROM rentals
             WHERE player_id = $1
               AND (cardinality($2::int[]) = 0 OR status = ANY($2))
               AND (cardinality($3::text[]) = 0 OR id = ANY($3))
               AND (cardinality($4::text[]) = 0 OR entity_class_guid = ANY($4))
               AND (cardinality($5::text[]) = 0 OR entitlement_urn = ANY($5))
             ORDER BY created_at, id"
        );
        let rows = self
            .db
            .query(
                &sql,
                &[
                    &(account.geid as i64),
                    &filter.statuses,
                    &filter.ids,
                    &filter.entity_class_guid,
                    &filter.entitlement_urns,
                ],
            )
            .await
            .map_err(internal)?;
        let rentals: Vec<Rental> = rows.iter().map(rental).collect();
        Ok(Response::new(reply_stream(rentals, |results| QueryRentalsStreamResponse { results })))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let owner = Endpoint::from_shared(args.entitlement.clone()).map_err(|e| format!("--entitlement: {e}"))?;
    let entitlements = EntitlementInternalClient::new(owner.connect_lazy());
    tokio::spawn(sweep_expired(db.clone(), entitlements.clone()));
    let graph = match &args.entitygraph {
        Some(address) => {
            let endpoint = Endpoint::from_shared(address.clone()).map_err(|e| format!("--entitygraph: {e}"))?;
            Some(EntityGraphInternalClient::new(endpoint.connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(10)).connect_lazy()))
        }
        None => {
            warn!("no entitygraph, a started rental makes no item");
            None
        }
    };
    let service = Arc::new(Rentals { state: State::load(&args.common.data)?, db, push: Push::connect(&args.push)?, entitlements, graph });
    let server = ExternalRentalServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = RentalInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "rental listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
