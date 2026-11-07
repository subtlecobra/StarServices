//! sc.external.services.entitlement.v2.ExternalEntitlementService. Snapshots are the saved layout of a
//! ship: which entitlement sits in which port, and this service owns them. Entitlements belong to
//! sc-entitlement (v1), so they are read and updated through its `EntitlementInternal`.

use std::{collections::HashMap, net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_economy::{
    command::{command_id, Push}, reply_stream, ReplyStream,
};
use sc_proto::internal::{
    entitlement_internal_client::EntitlementInternalClient, QueryEntitlementsRequest, UpdateLoadoutRequest,
};
use sc_proto::services::entitlement::v2::{
    external_entitlement_service_server::{ExternalEntitlementService, ExternalEntitlementServiceServer},
    Entitlement, EntitlementFilter, QueryEntitlementsByEntityClassStreamRequest,
    QueryEntitlementsByEntityClassStreamResponse, QueryEntitlementsByEntityGeidsStreamRequest,
    QueryEntitlementsByEntityGeidsStreamResponse, QueryEntitlementsByItemTypeStreamRequest,
    QueryEntitlementsByItemTypeStreamResponse, QueryEntitlementsBySourceTypeStreamRequest,
    QueryEntitlementsBySourceTypeStreamResponse, QueryEntitlementsStreamRequest, QueryEntitlementsStreamResponse,
    QuerySnapshotsStreamRequest, QuerySnapshotsStreamResponse, QuerySnapshotsWithEntitlementsStreamRequest,
    QuerySnapshotsWithEntitlementsStreamResponse, SnapshotFilter, SnapshotRecord, SnapshotWithEntitlement,
    UpdateArenaCommanderLoadoutRequest, UpdateArenaCommanderLoadoutResponse,
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::info;

const LOADOUT_RESPONSE: &str = "sc.external.services.entitlement.v2.UpdateArenaCommanderLoadoutResponse";
const SNAPSHOTS: &str = "
CREATE TABLE IF NOT EXISTS entitlement_snapshots (
    id TEXT PRIMARY KEY,
    player_id BIGINT NOT NULL,
    root_urn TEXT NOT NULL,
    item_urn TEXT NOT NULL,
    slot_id INT NOT NULL,
    attach_path TEXT[] NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS entitlement_snapshots_by_player ON entitlement_snapshots (player_id);";

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
    /// Address of sc-entitlement, which owns the entitlements.
    #[arg(long, env = "SCBACKEND_ENTITLEMENT", default_value = "http://sc-entitlement:50051")]
    entitlement: String,
}

struct Entitlements {
    state: State,
    db: Postgres,
    owner: EntitlementInternalClient<Channel>,
    push: Push,
}

fn snapshot(row: &Row) -> SnapshotRecord {
    SnapshotRecord {
        id: row.get("id"),
        player_id: row.get::<_, i64>("player_id") as u64,
        root_urn: row.get("root_urn"),
        item_urn: row.get("item_urn"),
        slot_id: row.get("slot_id"),
        attach_path: row.get("attach_path"),
    }
}

impl Entitlements {
    fn player(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// Asks the owner, which also grants the starter entitlements on first sight.
    async fn entitlements(
        &self,
        account: &Account,
        filter: EntitlementFilter,
        include_properties: bool,
    ) -> Result<Vec<Entitlement>, Status> {
        let request = QueryEntitlementsRequest {
            account_id: account.account_id,
            player_id: account.geid,
            filter: Some(filter),
            include_persistent_properties: include_properties,
        };
        let mut replies = self.owner.clone().query_entitlements(request).await?.into_inner();
        let mut entitlements = Vec::new();
        while let Some(reply) = replies.message().await? {
            entitlements.extend(reply.entitlements);
        }
        Ok(entitlements)
    }

    async fn snapshots(&self, geid: u64, filter: SnapshotFilter) -> Result<Vec<SnapshotRecord>, Status> {
        let slots: Vec<i32> = filter.snapshot_slot_id.iter().copied().filter(|slot| *slot != 0).collect();
        let rows = self
            .db
            .query(
                "SELECT id, player_id, root_urn, item_urn, slot_id, attach_path FROM entitlement_snapshots
                 WHERE player_id = $1
                   AND (cardinality($2::text[]) = 0 OR root_urn = ANY($2) OR item_urn = ANY($2))
                   AND (cardinality($3::int[]) = 0 OR slot_id = ANY($3))
                 ORDER BY root_urn, id",
                &[&(geid as i64), &filter.entitlement_urn, &slots],
            )
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(snapshot).collect())
    }
}

/// Pairs every record with its entitlement, when that still exists.
fn with_entitlements(records: Vec<SnapshotRecord>, entitlements: Vec<Entitlement>) -> Vec<SnapshotWithEntitlement> {
    let by_urn: HashMap<String, Entitlement> =
        entitlements.into_iter().map(|entitlement| (entitlement.urn.clone(), entitlement)).collect();
    records
        .into_iter()
        .map(|record| SnapshotWithEntitlement {
            entitlement: by_urn.get(&record.item_urn).cloned(),
            snapshot: Some(record),
        })
        .collect()
}

#[tonic::async_trait]
impl ExternalEntitlementService for Entitlements {
    type QueryEntitlementsStreamStream = ReplyStream<QueryEntitlementsStreamResponse>;
    type QuerySnapshotsStreamStream = ReplyStream<QuerySnapshotsStreamResponse>;
    type QuerySnapshotsWithEntitlementsStreamStream = ReplyStream<QuerySnapshotsWithEntitlementsStreamResponse>;
    type QueryEntitlementsByItemTypeStreamStream = ReplyStream<QueryEntitlementsByItemTypeStreamResponse>;
    type QueryEntitlementsBySourceTypeStreamStream = ReplyStream<QueryEntitlementsBySourceTypeStreamResponse>;
    type QueryEntitlementsByEntityClassStreamStream = ReplyStream<QueryEntitlementsByEntityClassStreamResponse>;
    type QueryEntitlementsByEntityGeidsStreamStream = ReplyStream<QueryEntitlementsByEntityGeidsStreamResponse>;

    async fn query_entitlements_stream(
        &self,
        request: Request<QueryEntitlementsStreamRequest>,
    ) -> Result<Response<Self::QueryEntitlementsStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let filter = request.filter.unwrap_or_default();
        let rows = self.entitlements(&account, filter, request.include_persistent_properties).await?;
        Ok(Response::new(reply_stream(rows, |results| QueryEntitlementsStreamResponse { results })))
    }

    async fn query_snapshots_stream(
        &self,
        request: Request<QuerySnapshotsStreamRequest>,
    ) -> Result<Response<Self::QuerySnapshotsStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let filter = request.into_inner().filter.unwrap_or_default();
        let rows = self.snapshots(account.geid, filter).await?;
        Ok(Response::new(reply_stream(rows, |results| QuerySnapshotsStreamResponse { results })))
    }

    async fn query_snapshots_with_entitlements_stream(
        &self,
        request: Request<QuerySnapshotsWithEntitlementsStreamRequest>,
    ) -> Result<Response<Self::QuerySnapshotsWithEntitlementsStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let records = self.snapshots(account.geid, request.filter.unwrap_or_default()).await?;
        let filter = EntitlementFilter {
            entitlement_urn: records.iter().map(|record| record.item_urn.clone()).collect(),
            ..Default::default()
        };
        let entitlements = match filter.entitlement_urn.is_empty() {
            true => Vec::new(),
            false => self.entitlements(&account, filter, request.include_persistent_properties).await?,
        };
        let rows = with_entitlements(records, entitlements);
        Ok(Response::new(reply_stream(rows, |results| QuerySnapshotsWithEntitlementsStreamResponse { results })))
    }

    async fn query_entitlements_by_item_type_stream(
        &self,
        request: Request<QueryEntitlementsByItemTypeStreamRequest>,
    ) -> Result<Response<Self::QueryEntitlementsByItemTypeStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let filter = EntitlementFilter {
            item_type: Some(request.item_type),
            item_sub_type: request.item_sub_type,
            ..Default::default()
        };
        let rows = self.entitlements(&account, filter, request.include_persistent_properties).await?;
        Ok(Response::new(reply_stream(rows, |results| QueryEntitlementsByItemTypeStreamResponse { results })))
    }

    async fn query_entitlements_by_source_type_stream(
        &self,
        request: Request<QueryEntitlementsBySourceTypeStreamRequest>,
    ) -> Result<Response<Self::QueryEntitlementsBySourceTypeStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let filter = EntitlementFilter {
            source_type: vec![request.source_type],
            entitlement_urn: request.entitlement_urns,
            ..Default::default()
        };
        let rows = self.entitlements(&account, filter, request.include_persistent_properties).await?;
        Ok(Response::new(reply_stream(rows, |results| QueryEntitlementsBySourceTypeStreamResponse { results })))
    }

    async fn query_entitlements_by_entity_class_stream(
        &self,
        request: Request<QueryEntitlementsByEntityClassStreamRequest>,
    ) -> Result<Response<Self::QueryEntitlementsByEntityClassStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let rows = match request.entity_class_guids.is_empty() {
            true => Vec::new(),
            false => {
                let filter = EntitlementFilter {
                    entity_class_guid: request.entity_class_guids,
                    source_type: request.source_types,
                    ..Default::default()
                };
                self.entitlements(&account, filter, request.include_persistent_properties).await?
            }
        };
        Ok(Response::new(reply_stream(rows, |results| QueryEntitlementsByEntityClassStreamResponse { results })))
    }

    async fn query_entitlements_by_entity_geids_stream(
        &self,
        request: Request<QueryEntitlementsByEntityGeidsStreamRequest>,
    ) -> Result<Response<Self::QueryEntitlementsByEntityGeidsStreamStream>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let rows = match request.entity_geids.is_empty() {
            true => Vec::new(),
            false => {
                let filter = EntitlementFilter { entity_geid: request.entity_geids, ..Default::default() };
                self.entitlements(&account, filter, request.include_persistent_properties).await?
            }
        };
        Ok(Response::new(reply_stream(rows, |results| QueryEntitlementsByEntityGeidsStreamResponse { results })))
    }

    async fn update_arena_commander_loadout(
        &self,
        request: Request<UpdateArenaCommanderLoadoutRequest>,
    ) -> Result<Response<UpdateArenaCommanderLoadoutResponse>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let command = command_id(request.arguments.as_ref())?;
        let loadout = request.loadout.unwrap_or_default().encode_to_vec();
        let update = UpdateLoadoutRequest {
            player_id: account.geid,
            urn: request.urn.clone(),
            loadout,
            command_id: command.to_string(),
        };
        self.owner.clone().update_loadout(update).await?;
        let response = UpdateArenaCommanderLoadoutResponse::default();
        let arguments = request.arguments.unwrap_or_default();
        self.push.complete(account.account_id, &arguments, LOADOUT_RESPONSE, &response).await;
        Ok(Response::new(response))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let owner = Endpoint::from_shared(args.entitlement.clone()).map_err(|e| format!("--entitlement: {e}"))?;
    let service = Entitlements {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SNAPSHOTS).await?,
        owner: EntitlementInternalClient::new(owner.connect_lazy()).max_decoding_message_size(MAX_MESSAGE_BYTES),
        push: Push::connect(&args.push)?,
    };
    let server = ExternalEntitlementServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "entitlement v2 listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(item_urn: &str) -> SnapshotRecord {
        SnapshotRecord { item_urn: item_urn.into(), ..Default::default() }
    }

    #[test]
    fn records_keep_their_entitlement_and_survive_without_one() {
        let owned = Entitlement { urn: "a".into(), ..Default::default() };
        let paired = with_entitlements(vec![record("a"), record("a"), record("gone")], vec![owned]);
        assert!(paired[0].entitlement.is_some());
        assert!(paired[1].entitlement.is_some());
        assert!(paired[2].entitlement.is_none());
    }
}
