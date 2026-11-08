//! sc.external.services.entitygraph.v1.EntityGraphService: entities, inventories, containers and the
//! edges between them, in ArangoDB. See docs/DATA.md.

mod admin;
mod area;
mod commands;
mod doc;
mod filter;
mod grant;
mod loadout;
mod read;
mod seed;

use std::{
    collections::HashSet,
    net::SocketAddr,
    pin::Pin,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use commands::{Plan, WRITES};
use futures_util::{stream, Stream, StreamExt};
use prost::Message;
use sc_db::{internal, Arango, DatabaseArgs};
use sc_api::internal::{ entity_graph_internal_server::{EntityGraphInternal, EntityGraphInternalServer}, push_publisher_client::PushPublisherClient, AdminAddItemRequest, AdminAddItemResponse, AdminInventoryTreeRequest, AdminInventoryTreeResponse, AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse, AdminSearchEntityClassesRequest, AdminSearchEntityClassesResponse, EnsureLoadoutRequest, EnsureLoadoutResponse, GrantItemsRequest, GrantItemsResponse, RevokeItemsRequest, RevokeItemsResponse, EntityOwner, GetOwnersRequest, GetOwnersResponse, LoadAreaRequest, PublishRequest, SeedContainersRequest, SeedEntityClassesRequest, SeedResponse, };
use sc_proto::{
    common::command::v1::{notify_destination, CommandResponseBody},
    services::entitygraph::v1::{
    entity_graph_service_server::{EntityGraphService, EntityGraphServiceServer},
    ContainerQueryRequest, ContainerQueryResponse, ContainerQueryStreamRequest, ContainerQueryStreamResponse,
    ContainerQueryStreamResponseBody, EntityQueryRequest, EntityQueryResponse, GetDatabaseVersionRequest,
    GetDatabaseVersionResponse, GetDatabaseVersionResponseBody, GetEntityStowContextsByGeidsRequest,
    GetEntityStowContextsByGeidsResponse, GetEntityStowContextsByGeidsResponseBody,
    GetEntityStowContextsByParentUrnsRequest, GetEntityStowContextsByParentUrnsResponse,
    GetEntityStowContextsByParentUrnsResponseBody, GetInventoriesRequest, GetInventoriesResponse,
    GetInventoriesResponseBody, InventoryQueryRequest, InventoryQueryResponse, MoveRequest, MoveResponse,
    MoveResponseBody, MutateEntitiesRequest, MutateEntitiesResponse, MutateEntitiesResponseBody,
    MutationArguments, StreamStowContextsByOwnerRequest, StreamStowContextsByOwnerResponse,
    StreamStowContextsByOwnerResponseBody,
    },
};
use sc_runtime::{
    caller,
    state::{Account, State},
    CommonArgs, MAX_MESSAGE_BYTES,
};
use serde_json::json;
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

const COLLECTIONS: &[&str] = &[
    "entities",
    "inventories",
    "containers",
    "entity_snapshots",
    "entity_classes",
    "lt_records",
    "entitygraph_meta",
    "applied_commands",
    "edge:entity_edges",
    "edge:container_edges",
];
const DATABASE_VERSION: &str = "1";
const STOW_CONTEXTS_PER_MESSAGE: usize = 500;
const TYPE_URL_PREFIX: &str = "type.googleapis.com/";
const RESPONSE_TYPE: &str = "sc.external.common.command.v1.CommandResponseBody";
const MUTATE_RESPONSE: &str = "sc.external.services.entitygraph.v1.MutateEntitiesResponse";
const MOVE_RESPONSE: &str = "sc.external.services.entitygraph.v1.MoveResponse";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// The internal publisher of sc-push.
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
}

struct Graph {
    state: State,
    db: Arango,
    push: PushPublisherClient<Channel>,
    known_players: Mutex<HashSet<u64>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A failed transaction as the caller should see it. The text comes from our own ASSERT statements.
fn write_error(error: String) -> Status {
    if let Some((_, reason)) = error.split_once("precondition: ") {
        return Status::failed_precondition(reason.trim_end_matches('"').to_string());
    }
    if error.contains("unique constraint violated") {
        return Status::already_exists("already exists");
    }
    internal(error)
}

/// The result of a command as the client waits for it: on the topics it named in the command, with
/// the response in `context`.
fn command_result(arguments: &MutationArguments, kind: &str, response: &impl Message) -> (Vec<String>, Vec<u8>) {
    let notifications = arguments.notifications.clone().unwrap_or_default();
    if notifications.only_on_failure {
        return (Vec::new(), Vec::new());
    }
    let destinations = notifications.destinations.into_iter().filter_map(|destination| destination.destination);
    let topics = destinations
        .filter_map(|destination| match destination {
            notify_destination::Destination::Push(push) => Some(push.topics),
            notify_destination::Destination::Kafka(_) => None,
        })
        .flatten()
        .collect();
    let context = prost_types::Any {
        type_url: format!("{TYPE_URL_PREFIX}{kind}"),
        value: response.encode_to_vec(),
    };
    let body = CommandResponseBody { id: arguments.id.clone(), status: None, context: Some(context) };
    (topics, body.encode_to_vec())
}

impl Graph {
    fn account(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// Creates the account's player entity and inventory the first time they are needed.
    async fn ensure_player(&self, account: &Account) -> Result<(), Status> {
        if self.known_players.lock().unwrap().contains(&account.geid) {
            return Ok(());
        }
        let now = now_ms();
        let inventory_id = doc::player_inventory_id(account.geid);
        let player = doc::player_doc(account.geid, &account.handle, &inventory_id, now);
        let inventory = doc::player_inventory_doc(account.geid, now);
        let edge = doc::holder_edge(&inventory_id, account.geid);
        let statements = [
            // Players created before classes and item types were known have 0 for them.
            (
                "UPSERT { _key: @doc._key } INSERT @doc UPDATE { classGuidCrc: @doc.classGuidCrc, itemTypeEnum: @doc.itemTypeEnum, itemSubTypeEnum: @doc.itemSubTypeEnum } IN entities",
                json!({ "doc": player }),
            ),
            ("INSERT @doc INTO inventories OPTIONS { overwriteMode: 'ignore' }", json!({ "doc": inventory })),
            ("INSERT @doc INTO entity_edges OPTIONS { overwriteMode: 'ignore' }", json!({ "doc": edge })),
        ];
        self.db.transaction(WRITES, &statements).await.map_err(internal)?;
        self.known_players.lock().unwrap().insert(account.geid);
        info!(handle = %account.handle, geid = account.geid, "player entity ready");
        Ok(())
    }

    async fn ensure_caller(&self, metadata: &MetadataMap) -> Result<(), Status> {
        match caller(&self.state, metadata) {
            Some(account) => self.ensure_player(&account).await,
            None => Ok(()),
        }
    }

    /// Applies a plan once per command id. The id is recorded in the same transaction, so a retry after
    /// a lost reply finds it and succeeds without repeating a change such as a `Stack`.
    async fn apply(&self, plan: Plan, account: &Account, arguments: &MutationArguments) -> Result<(), Status> {
        let mut statements = plan.statements(account.geid);
        if !arguments.id.is_empty() {
            let id = doc::key(&arguments.id);
            let seen = "FOR c IN applied_commands FILTER c._key == @id RETURN 1";
            if !self.db.aql(seen, json!({ "id": id })).await.map_err(internal)?.is_empty() {
                return Ok(());
            }
            let record = "INSERT { _key: @id, at: @at } INTO applied_commands".to_string();
            statements.push((record, json!({ "id": id, "at": now_ms() })));
        }
        let borrowed: Vec<(&str, serde_json::Value)> =
            statements.iter().map(|(query, bind)| (query.as_str(), bind.clone())).collect();
        self.db.transaction(WRITES, &borrowed).await.map_err(write_error)
    }

    /// A push that fails leaves the client waiting until its deadline, but the command stands.
    async fn notify(&self, account: &Account, arguments: &MutationArguments, kind: &str, response: &impl Message) {
        let (topics, value) = command_result(arguments, kind, response);
        for topic in topics {
            let request = PublishRequest {
                account_id: account.account_id,
                topic,
                type_name: RESPONSE_TYPE.to_string(),
                value: value.clone(),
                wait_for_subscriber_ms: 0,
            };
            if let Err(error) = self.push.clone().publish(request).await {
                warn!(%error, command = %arguments.id, "command result not pushed");
            }
        }
    }
}

type Reply<T> = Result<Response<T>, Status>;

/// Owners as `entities` stores them: a decimal string, '0' for none.
fn owners(rows: &[serde_json::Value]) -> Vec<EntityOwner> {
    let id = |value: &serde_json::Value| value.as_str().and_then(|id| id.parse::<u64>().ok());
    let owner = |row: &serde_json::Value| Some(EntityOwner { entity: id(&row["entity"])?, owner: id(&row["owner"])? });
    rows.iter().filter_map(owner).filter(|found| found.owner != 0).collect()
}

/// Calls from other backend services, on the same port: see `EntityGraphInternal` in internal.proto.
#[tonic::async_trait]
impl EntityGraphInternal for Graph {
    type LoadAreaStream = area::AreaStream;

    async fn load_area(&self, request: Request<LoadAreaRequest>) -> Reply<Self::LoadAreaStream> {
        Ok(Response::new(area::load(&self.db, request.into_inner()).await?))
    }

    async fn get_owners(&self, request: Request<GetOwnersRequest>) -> Reply<GetOwnersResponse> {
        let keys: Vec<String> = request.into_inner().entities.iter().map(u64::to_string).collect();
        if keys.is_empty() {
            return Ok(Response::new(GetOwnersResponse::default()));
        }
        let query = "FOR e IN entities FILTER e._key IN @keys RETURN { entity: e._key, owner: e.ownerId }";
        let rows = self.db.aql(query, json!({ "keys": keys })).await.map_err(internal)?;
        Ok(Response::new(GetOwnersResponse { owners: owners(&rows) }))
    }

    async fn admin_inventory_tree(&self, request: Request<AdminInventoryTreeRequest>) -> Reply<AdminInventoryTreeResponse> {
        Ok(Response::new(admin::tree(&self.db, request.into_inner()).await?))
    }

    async fn admin_move_item(&self, request: Request<AdminMoveItemRequest>) -> Reply<AdminMoveItemResponse> {
        Ok(Response::new(admin::move_item(&self.db, request.into_inner()).await?))
    }

    async fn revoke_items(&self, request: Request<RevokeItemsRequest>) -> Reply<RevokeItemsResponse> {
        Ok(Response::new(grant::revoke(&self.db, request.into_inner()).await?))
    }

    async fn grant_items(&self, request: Request<GrantItemsRequest>) -> Reply<GrantItemsResponse> {
        Ok(Response::new(grant::grant(&self.db, request.into_inner()).await?))
    }

    async fn ensure_loadout(&self, request: Request<EnsureLoadoutRequest>) -> Reply<EnsureLoadoutResponse> {
        Ok(Response::new(loadout::ensure(&self.db, request.into_inner()).await?))
    }

    async fn admin_add_item(&self, request: Request<AdminAddItemRequest>) -> Reply<AdminAddItemResponse> {
        Ok(Response::new(admin::add_item(&self.db, request.into_inner()).await?))
    }

    async fn admin_remove_item(&self, request: Request<AdminRemoveItemRequest>) -> Reply<AdminRemoveItemResponse> {
        Ok(Response::new(admin::remove_item(&self.db, request.into_inner()).await?))
    }

    async fn admin_search_entity_classes(
        &self,
        request: Request<AdminSearchEntityClassesRequest>,
    ) -> Reply<AdminSearchEntityClassesResponse> {
        Ok(Response::new(admin::search_classes(&self.db, request.into_inner()).await?))
    }

    async fn seed_entity_classes(&self, request: Request<tonic::Streaming<SeedEntityClassesRequest>>) -> Reply<SeedResponse> {
        Ok(Response::new(SeedResponse { rows: seed::classes(&self.db, request.into_inner()).await? }))
    }

    async fn seed_containers(&self, request: Request<tonic::Streaming<SeedContainersRequest>>) -> Reply<SeedResponse> {
        Ok(Response::new(SeedResponse { rows: seed::containers(&self.db, request.into_inner()).await? }))
    }
}

#[tonic::async_trait]
impl EntityGraphService for Graph {
    type ContainerQueryStreamStream = Pin<Box<dyn Stream<Item = Result<ContainerQueryStreamResponse, Status>> + Send>>;
    type StreamStowContextsByOwnerStream =
        Pin<Box<dyn Stream<Item = Result<StreamStowContextsByOwnerResponse, Status>> + Send>>;

    async fn entity_query(&self, request: Request<EntityQueryRequest>) -> Reply<EntityQueryResponse> {
        self.ensure_caller(request.metadata()).await?;
        let body = request.into_inner().body.unwrap_or_default();
        let result = read::entity_query(&self.db, body.scope, body.query.unwrap_or_default()).await?;
        Ok(Response::new(EntityQueryResponse { body: Some(result) }))
    }

    async fn container_query(&self, request: Request<ContainerQueryRequest>) -> Reply<ContainerQueryResponse> {
        let body = request.into_inner().body.unwrap_or_default();
        let query = body.query.unwrap_or_default();
        info!(?query, "container query");
        let started = Instant::now();
        let result = read::container_query(&self.db, query).await.inspect_err(|error| warn!(%error, "container query failed"))?;
        info!(bytes = result.encoded_len(), elapsed = ?started.elapsed(), "container query answered");
        Ok(Response::new(ContainerQueryResponse { body: Some(result) }))
    }

    async fn container_query_stream(
        &self,
        request: Request<ContainerQueryStreamRequest>,
    ) -> Reply<Self::ContainerQueryStreamStream> {
        let body = request.into_inner().body.unwrap_or_default();
        let batch = if body.batch_size > 0 { body.batch_size as usize } else { read::DEFAULT_BATCH };
        let query = body.query.unwrap_or_default();
        info!(?query, batch, "container stream");
        let started = Instant::now();
        let graphs = read::container_stream(&self.db, query, batch)
            .await
            .inspect_err(|error| warn!(%error, "container stream failed"))?;
        let count = graphs.len();
        let bytes: usize = graphs.iter().map(Message::encoded_len).sum();
        info!(count, bytes, elapsed = ?started.elapsed(), "container stream read");
        let messages = graphs.into_iter().map(|graph| {
            let body = ContainerQueryStreamResponseBody { results: Some(graph) };
            Ok(ContainerQueryStreamResponse { body: Some(body) })
        });
        // Runs only once the client has taken every message, which tells a slow reader from a lost one.
        let finished = stream::once(async move {
            info!(count, elapsed = ?started.elapsed(), "container stream delivered");
        })
        .filter_map(|()| async { None::<Result<ContainerQueryStreamResponse, Status>> });
        Ok(Response::new(Box::pin(stream::iter(messages).chain(finished))))
    }

    async fn inventory_query(&self, request: Request<InventoryQueryRequest>) -> Reply<InventoryQueryResponse> {
        self.ensure_caller(request.metadata()).await?;
        let body = request.into_inner().body.unwrap_or_default();
        let result = read::inventory_query(&self.db, body.query.unwrap_or_default()).await?;
        Ok(Response::new(InventoryQueryResponse { body: Some(result) }))
    }

    async fn get_inventories(&self, request: Request<GetInventoriesRequest>) -> Reply<GetInventoriesResponse> {
        let account = caller(&self.state, request.metadata());
        if let Some(account) = &account {
            self.ensure_player(account).await?;
        }
        let owner = request.into_inner().body.unwrap_or_default().owner_id;
        let owner = match (owner.is_empty(), &account) {
            (false, _) => owner,
            (true, Some(account)) => account.geid.to_string(),
            (true, None) => return Err(Status::unauthenticated("missing or invalid token")),
        };
        // Another player's inventories are shown only where they are shared.
        let everything = account.as_ref().is_none_or(|account| account.geid.to_string() == owner);
        let inventories = read::inventories_of(&self.db, &owner, everything).await?;
        Ok(Response::new(GetInventoriesResponse { body: Some(GetInventoriesResponseBody { inventories }) }))
    }

    async fn get_entity_stow_contexts_by_geids(
        &self,
        request: Request<GetEntityStowContextsByGeidsRequest>,
    ) -> Reply<GetEntityStowContextsByGeidsResponse> {
        self.ensure_caller(request.metadata()).await?;
        let body = request.into_inner().body.unwrap_or_default();
        let results = read::stow_by_geids(&self.db, &body.geids, body.owner_id, &body.entity_class_crcs).await?;
        let body = GetEntityStowContextsByGeidsResponseBody { results };
        Ok(Response::new(GetEntityStowContextsByGeidsResponse { body: Some(body) }))
    }

    async fn get_entity_stow_contexts_by_parent_urns(
        &self,
        request: Request<GetEntityStowContextsByParentUrnsRequest>,
    ) -> Reply<GetEntityStowContextsByParentUrnsResponse> {
        let body = request.into_inner().body.unwrap_or_default();
        let results = read::stow_by_parent_urns(&self.db, &body.parent_urns, body.owner_id, &body.entity_class_crcs).await?;
        let body = GetEntityStowContextsByParentUrnsResponseBody { results };
        Ok(Response::new(GetEntityStowContextsByParentUrnsResponse { body: Some(body) }))
    }

    /// The client sends this with an empty body: the owner is whoever the token says.
    async fn stream_stow_contexts_by_owner(
        &self,
        request: Request<StreamStowContextsByOwnerRequest>,
    ) -> Reply<Self::StreamStowContextsByOwnerStream> {
        let account = self.account(request.metadata())?;
        self.ensure_player(&account).await?;
        let contexts = read::stow_by_owner(&self.db, account.geid).await?;
        let messages: Vec<_> = contexts
            .chunks(STOW_CONTEXTS_PER_MESSAGE)
            .map(|chunk| {
                let body = StreamStowContextsByOwnerResponseBody { results: chunk.to_vec() };
                Ok(StreamStowContextsByOwnerResponse { body: Some(body) })
            })
            .collect();
        Ok(Response::new(Box::pin(stream::iter(messages))))
    }

    async fn r#move(&self, request: Request<MoveRequest>) -> Reply<MoveResponse> {
        let account = self.account(request.metadata())?;
        self.ensure_player(&account).await?;
        let body = request.into_inner().body.unwrap_or_default();
        let arguments = body.arguments.unwrap_or_default();
        let mut plan = Plan::new(Some(&arguments), now_ms());
        plan.moves(&body.r#move.unwrap_or_default());
        self.apply(plan, &account, &arguments).await?;
        let response = MoveResponse { body: Some(MoveResponseBody {}) };
        self.notify(&account, &arguments, MOVE_RESPONSE, &response).await;
        Ok(Response::new(response))
    }

    async fn mutate_entities(&self, request: Request<MutateEntitiesRequest>) -> Reply<MutateEntitiesResponse> {
        let account = self.account(request.metadata())?;
        self.ensure_player(&account).await?;
        let body = request.into_inner().body.unwrap_or_default();
        let arguments = body.arguments.unwrap_or_default();
        let mut plan = Plan::new(Some(&arguments), now_ms());
        for command in &body.commands {
            plan.command(command)?;
        }
        self.apply(plan, &account, &arguments).await?;
        let response = MutateEntitiesResponse { body: Some(MutateEntitiesResponseBody {}) };
        self.notify(&account, &arguments, MUTATE_RESPONSE, &response).await;
        Ok(Response::new(response))
    }

    async fn get_database_version(&self, _: Request<GetDatabaseVersionRequest>) -> Reply<GetDatabaseVersionResponse> {
        let query = "FOR m IN entitygraph_meta FILTER m._key == 'version' RETURN m.version";
        let rows = self.db.aql(query, json!({})).await.map_err(internal)?;
        let version = rows.first().and_then(|version| version.as_str()).unwrap_or(DATABASE_VERSION).to_string();
        Ok(Response::new(GetDatabaseVersionResponse { body: Some(GetDatabaseVersionResponseBody { version }) }))
    }
}

/// The indexes the reads and the inventory checks filter on. `dirty` is sparse: only inventories waiting for their
/// count have it.
const INDEXES: &[(&str, &[&str], bool)] = &[
    ("inventories", &["id"], false),
    ("inventories", &["ownerId", "context"], false),
    ("inventories", &["holderId"], false),
    ("inventories", &["dirty"], true),
    ("entities", &["ownerId"], false),
    ("entities", &["inventoryId"], true),
    ("entities", &["parentUrn"], false),
    ("entity_classes", &["guidHashCrc"], false),
];

async fn open(arango: &str) -> Result<Arango, String> {
    let db = Arango::connect(arango, COLLECTIONS).await?;
    for (collection, fields, sparse) in INDEXES {
        db.ensure_index(collection, fields, *sparse).await?;
    }
    let seed = "INSERT { _key: 'version', version: @version } INTO entitygraph_meta OPTIONS { overwriteMode: 'ignore' }";
    db.aql(seed, json!({ "version": DATABASE_VERSION })).await?;
    // Inventories written before the context and subject were taken from the id: fill them in, and count the
    // occupancy of each physical one again on its next change.
    let migrate = "FOR i IN inventories FILTER i.context IN [null, ''] OR i.subjectId == null
                       LET parts = SPLIT(i.id, ':')
                       FILTER LENGTH(parts) == 3
                       UPDATE i WITH { context: parts[1], subjectId: parts[2], holderId: parts[0], dirty: true } IN inventories";
    db.aql(migrate, json!({})).await?;
    Ok(db)
}

async fn serve(args: Args) -> Result<(), String> {
    let endpoint = Endpoint::from_shared(args.push.clone()).map_err(|e| format!("--push: {e}"))?;
    let service = Arc::new(Graph {
        state: State::load(&args.common.data)?,
        db: open(&args.database.arango).await?,
        push: PushPublisherClient::new(endpoint.connect_lazy()),
        known_players: Mutex::default(),
    });
    let server = EntityGraphServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = EntityGraphInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "entity graph listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests;
