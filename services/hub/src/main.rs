//! The "Hub" (legacy Diffusion/CRYNUB connection, `star_network` in LoginData.json).
//!
//! TLS over TCP. Frames are `[u32 LE length][u32 LE 0xDEADBEEF][envelope]`, and the envelope is a
//! protobuf `{ string key = 1; bytes payload = 2; uint64 id = 3; }` routed by key.
//!
//! The Hub owns `player_shards`, the shard each connected player was sent to. Other services ask for it over
//! `HubInternal`, on a gRPC port of its own, as the Hub's port speaks only its TLS protocol.

mod claim;
mod disconnect;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use prost::Message;
use rustls::ServerConfig;
use sc_db::{DatabaseArgs, Postgres};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;
use tonic::{
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{debug, info, warn};

use sc_db::internal;
use sc_api::internal::{
    entity_document_internal_client::EntityDocumentInternalClient,
    identity_internal_client::IdentityInternalClient, GetAccountsRequest,
    hub_internal_server::{HubInternal, HubInternalServer},
    shard_directory_client::ShardDirectoryClient,
    entity_graph_internal_client::EntityGraphInternalClient, AllocateShardRequest, DisconnectReason, GetEntityDocumentsRequest, GetResidenceRequest, HangarSize,
    GetPlayerShardRequest, GetPlayerShardResponse, PlayerDisconnect, PlayerShard, ReportDisconnectResponse,
    ResolveSessionRequest, ResolveSessionResponse,
};
use sc_proto::services::entity_document::v1::entity_document_body::Class;
use sc_proto::hub::{
    AccountLogin, AccountLoginResponse, AccountRecord, Envelope, GotoLocation, InstanceEntry,
    LocationServicesDestination, LocationServicesDestinationList, MatchMakerUpdate, PlayerStatusRequest, PlayerUpdated,
    PlayerUpdatedGroup, RegionSelection, RpcGuid, RpcRequest, RpcResponse, SysNotify,
};
use sc_runtime::{
    destinations::{self, Destination},
    state::{Account, State},
    tls, CommonArgs,
};

const FRAME_MAGIC: u32 = 0xDEAD_BEEF;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOGGED_PAYLOAD: usize = 512;
const LOGIN_RESULT_SUCCESS: i32 = 1;
const JOIN_STATUS_READY: i32 = 1;
/// `smsg_ag_plr_updated` as the official Hub answers `cmsg_ag_update_plr_status`: fields 3 and 7 are 0 and field 5 is
/// 0x67, whose meaning is not known, so it is repeated as is.
const PLAYER_UPDATED_ZERO: i32 = 0;
const PLAYER_UPDATED_FIELD5: i32 = 0x67;
/// `msg_sys_notify` of the official join: field 1 is 1 in all five frames; the codes follow the join in this order (the
/// first, a bare `08 01 10 02`, carries nothing else), field 5 is 1 with the second code and the port with the last.
const SYS_NOTIFY_FIELD1: i32 = 1;
const SYS_NOTIFY_CODE_START: i32 = 2;
const SYS_NOTIFY_CODE_ALLOCATED: i32 = 27;
const SYS_NOTIFY_CODE_ROUTING: i32 = 3;
const SYS_NOTIFY_CODE_READY: i32 = 4;
const SYS_NOTIFY_ALLOCATED_FLAG: i32 = 1;
/// `SC_HUB_JOIN_NOTIFY`: `on`, `1`, `true` or `yes` sends the official `msg_sys_notify` sequence before `smsg_do_join_instance`. Off
/// by default: the codes' meaning and what the client does with them is not verified.
const JOIN_NOTIFY_SETTING: &str = "SC_HUB_JOIN_NOTIFY";
const JOIN_NOTIFY_ON: [&str; 4] = ["on", "1", "true", "yes"];
const RPC_SUCCESS: i32 = 0;
const RPC_FAILED: i32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS persistent_items (
    item_key TEXT NOT NULL,
    item_value TEXT NOT NULL,
    node_path TEXT NOT NULL DEFAULT '',
    node_data TEXT NOT NULL,
    PRIMARY KEY (item_key, item_value, node_path)
);
CREATE TABLE IF NOT EXISTS player_shards (
    geid BIGINT PRIMARY KEY,
    shard_id TEXT NOT NULL,
    public_address TEXT NOT NULL,
    public_port INT NOT NULL,
    location_id BIGINT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS player_shards_shard ON player_shards (shard_id);
CREATE TABLE IF NOT EXISTS sessions (
    session TEXT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    geid BIGINT NOT NULL,
    nickname TEXT NOT NULL,
    home_location BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS hangar_size INT NOT NULL DEFAULT 0;";
/// A session lasts as long as its Hub connection, and a day at most.
const SESSION_MAX_AGE: &str = "1 day";
const PERSISTENT_PREFIX: &str = "var.persistent_items.";
const PERSISTENT_GET: &str = "get";
/// The member naming the variable's owner in a request and its reply (`geid`).
const PERSISTENT_DEFAULT_KEY: &str = "geid";
/// `result` of a free form variable the service holds, and of one it does not (live).
const VARIABLE_FOUND: i32 = 0;
const VARIABLE_NOT_FOUND: i32 = -5;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:8000")]
    listen: SocketAddr,
    /// gRPC for other backend services: `HubInternal`.
    #[arg(long, env = "SCBACKEND_INTERNAL_LISTEN", default_value = "0.0.0.0:50051")]
    internal_listen: SocketAddr,
    /// Address clients use to reach the PU frontend.
    #[arg(long, env = "SCBACKEND_PUBLIC_IP", default_value = "127.0.0.2")]
    public_ip: String,
    /// Port clients use to reach the PU frontend.
    #[arg(long, env = "SCBACKEND_SHARD_PORT", default_value_t = 64090)]
    shard_port: u16,
    #[arg(long, env = "SCBACKEND_SHARD_NAME", default_value = "private_shard_001")]
    shard_name: String,
    /// Account used when the login token cannot be matched to one.
    #[arg(long, env = "SCBACKEND_HANDLE", default_value = "pilot")]
    handle: String,
    /// sc-shard-operator's address. Without it, every join goes to the one shard given above.
    #[arg(long, env = "SCBACKEND_SHARD_DIRECTORY")]
    shard_directory: Option<String>,
    /// Region to allocate shards in when the client names none the map knows.
    #[arg(long, env = "SCBACKEND_REGION", default_value = "usa")]
    region: String,
    /// `<client region id>=<ShardCluster region>`, comma separated.
    #[arg(long, env = "SCBACKEND_REGION_MAP", value_delimiter = ',')]
    region_map: Vec<String>,
    /// sc-entity-document, which holds each player's chosen home location (`player_home_location`).
    #[arg(long, env = "SCBACKEND_ENTITY_DOCUMENT", default_value = "http://sc-entity-document:50051")]
    entity_document: String,
    /// sc-identity, which owns the handle and the name the game shows for each player.
    #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://sc-identity:50051")]
    identity: String,
    /// sc-entitlement and sc-entitygraph, for `entitlement.inventory.location`: the entitlements that are items become items
    /// in the player's bank at the joined location. Both are needed; without them the call is only acknowledged.
    #[arg(long, env = "SCBACKEND_ENTITLEMENT")]
    entitlement: Option<String>,
    /// See `entitlement`. The sc-entitygraph of the players' region.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH")]
    entitygraph: Option<String>,
}

/// How long a login waits for the home location before going on without it.
const HOME_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// The size of the player's home hangar, `player_residence` in sc-entity-document, which the session carries to the shard.
/// Unspecified when none was chosen, or sc-entity-document fails or does not answer in time; the DGS then gives the default.
async fn hangar_size(documents: &EntityDocumentInternalClient<Channel>, geid: u64) -> i32 {
    let mut documents = documents.clone();
    match tokio::time::timeout(HOME_LOOKUP_TIMEOUT, documents.get_residence(GetResidenceRequest { entity_id: geid })).await {
        Ok(Ok(response)) => response.into_inner().hangar_size,
        Ok(Err(status)) => {
            warn!(geid, error = status.message(), "residence lookup failed");
            HangarSize::Unspecified as i32
        }
        Err(_) => {
            warn!(geid, "residence lookup timed out");
            HangarSize::Unspecified as i32
        }
    }
}

/// The player's chosen home location, `player_home_location` in sc-entity-document, which the session id carries to the
/// shard: the DGS spawns the player in that location's hangar. None when the player chose none, or sc-entity-document
/// fails or does not answer in time; the DGS then uses its default.
async fn home_location(documents: &EntityDocumentInternalClient<Channel>, geid: u64) -> Option<u32> {
    let request = GetEntityDocumentsRequest { entity_id: geid, classes: vec!["player_home_location".into()] };
    let mut documents = documents.clone();
    let response = match tokio::time::timeout(HOME_LOOKUP_TIMEOUT, documents.get_documents(request)).await {
        Ok(Ok(response)) => response.into_inner(),
        Ok(Err(status)) => {
            warn!(geid, error = status.message(), "home location lookup failed");
            return None;
        }
        Err(_) => {
            warn!(geid, "home location lookup timed out");
            return None;
        }
    };
    response.documents.into_iter().find_map(|stored| match stored.document?.body?.class? {
        Class::PlayerHomeLocation(home) if home.home_location_id != 0 => Some(home.home_location_id),
        _ => None,
    })
}

/// The handle and the name sc-identity has for a player now. The game token's handle can predate a rename, so the Hub
/// never takes the player's names from it. None when identity cannot say; the token's handle then stands in.
async fn live_names(identity: &IdentityInternalClient<Channel>, geid: u64) -> Option<(String, String)> {
    match identity.clone().get_accounts(GetAccountsRequest { geids: vec![geid] }).await {
        Ok(reply) => reply.into_inner().accounts.into_iter().find(|a| a.geid == geid).map(|a| (a.handle, a.display_name)),
        Err(status) => {
            warn!(geid, %status, "no names from identity, using the token's handle");
            None
        }
    }
}

/// How many fresh sessions a login draws before giving up, should each one collide with a live one. Two 128-bit random
/// values coincide with odds of about 2^-128 each time, so a second draw is already beyond any real chance.
const SESSION_ATTEMPTS: usize = 5;

/// Records a new session for `account` and returns it. Its primary key keeps every live session unique: a draw that
/// collides with one writes no row, and another is drawn. Expired sessions are removed first (a crash can leave some).
async fn issue_session(db: &Postgres, account: &Account, home: Option<u32>, hangar_size: i32) -> Result<String, String> {
    db.execute(&format!("DELETE FROM sessions WHERE created_at < now() - interval '{SESSION_MAX_AGE}'"), &[]).await?;
    for _ in 0..SESSION_ATTEMPTS {
        let session = new_session()?;
        let written = db
            .execute(
                "INSERT INTO sessions (session, account_id, geid, nickname, home_location, hangar_size) VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (session) DO NOTHING",
                &[&session, &i64::from(account.account_id), &(account.geid as i64), &account.handle, &home.map(i64::from), &hangar_size],
            )
            .await?;
        if written == 1 {
            return Ok(session);
        }
        warn!("a new session collided with a live one: drawing another");
    }
    Err(format!("{SESSION_ATTEMPTS} new sessions all collided with live ones"))
}

/// A new session: 16 random bytes as 32 hex digits, as the real backend's look. It says nothing about the player: a shard
/// learns who it is only by asking the Hub (`ResolveSession`), so a client cannot name itself someone else.
fn new_session() -> Result<String, String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| "no system randomness".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Clone)]
pub struct ShardInfo {
    pub directory: Option<ShardDirectoryClient<Channel>>,
    pub region: String,
    pub region_map: HashMap<u64, String>,
    pub public_ip: String,
    pub port: u16,
    pub name: String,
    pub destinations: Vec<Destination>,
    pub max_players: i32,
}

impl ShardInfo {
    /// The location a join lands in. One shard serves every destination for now.
    fn default_location(&self) -> u64 {
        self.destinations.iter().find(|d| d.joinable()).map_or(0, |d| d.location_id)
    }
}

/// Parsed as a GUID by the client (in the smsg_ls_dest_list handler).
fn instance_id(index: usize) -> String {
    format!("00000000-0000-4000-8000-{:012x}", index + 1)
}

/// Payloads of messages without a handler, for working out their format from a real run.
fn hex_preview(bytes: &[u8]) -> String {
    bytes.iter().take(MAX_LOGGED_PAYLOAD).map(|b| format!("{b:02x}")).collect()
}

fn frame(key: &str, payload: impl Message, id: u64) -> Vec<u8> {
    raw_frame(key, &payload.encode_to_vec(), id)
}

fn raw_frame(key: &str, payload: &[u8], id: u64) -> Vec<u8> {
    let envelope = Envelope { key: key.to_string(), payload: payload.to_vec(), id };
    let body = envelope.encode_to_vec();
    let mut frame = Vec::with_capacity(body.len() + 8);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// A zero `queue_wait` completes the login at once; a positive one waits for login_queue_status.
fn login_response(account: &Account, display_name: &str, session: &str) -> AccountLoginResponse {
    AccountLoginResponse {
        result: LOGIN_RESULT_SUCCESS,
        account: Some(AccountRecord {
            session: Some(session.to_string()),
            account_id: Some(account.account_id as i32),
            display_name: Some(display_name.to_string()),
            handle: Some(account.handle.clone()),
            geid: Some(account.geid),
            ..Default::default()
        }),
        field4: None,
        queue_wait: 0,
        field6: 0,
    }
}

/// How many players this shard has at each location, from `player_shards`; a destination's instance reports its count.
/// Empty when the table cannot be read: the list is still sent, with zeros, rather than none.
async fn players_by_location(db: &Postgres, shard_name: &str) -> HashMap<u64, i32> {
    match db.query("SELECT location_id, count(*) AS players FROM player_shards WHERE shard_id = $1 GROUP BY location_id", &[&shard_name]).await {
        Ok(rows) => rows
            .iter()
            .map(|row| (row.get::<_, i64>("location_id") as u64, i32::try_from(row.get::<_, i64>("players")).unwrap_or(i32::MAX)))
            .collect(),
        Err(error) => {
            warn!(%error, "player counts for the destination list unreadable, sent as 0");
            HashMap::new()
        }
    }
}

fn destination_list(shard: &ShardInfo, players: &HashMap<u64, i32>) -> LocationServicesDestinationList {
    // Real sessions also list the 0xffff sentinel, but the client skips it, and it shares Stanton's name
    // here, which made the client's location table lose Stanton.
    let list = shard
        .destinations
        .iter()
        .filter(|destination| destination.joinable())
        .enumerate()
        .map(|(index, destination)| LocationServicesDestination {
            location_id: destination.location_id,
            location_name: destination.name.clone(),
            instance_list: vec![InstanceEntry {
                instance_id: instance_id(index),
                max_players: shard.max_players,
                num_players: Some(players.get(&destination.location_id).copied().unwrap_or(0)),
                ..Default::default()
            }],
            instance_owner: 0,
        })
        .collect();
    LocationServicesDestinationList { list }
}

/// One entry of a persistent item request: `{key, value, node_path, node_data}`. Bulk requests carry a
/// list of them under a `*_list` member, and the others are a single entry.
fn persistent_entries(body: &Value) -> Vec<Value> {
    body.as_object()
        .and_then(|members| members.values().find_map(|value| value.as_array().cloned()))
        .unwrap_or_else(|| vec![body.clone()])
}

fn text(value: &Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_string)
}

/// The free form variable store. The client sends JSON and reads JSON back, and the reply format comes
/// from the names in the client (`data_list`, `node_path`, `node_data`).
async fn persistent_items(db: &Postgres, method: &str, body: &str) -> Result<String, String> {
    let request: Value = serde_json::from_str(body).map_err(|e| format!("bad body: {e}"))?;
    let mut looked_up = Vec::new();
    for entry in persistent_entries(&request) {
        let (key, value) = (text(&entry["key"]), text(&entry["value"]));
        let path = entry["node_path"].as_str().unwrap_or_default().to_string();
        if method.starts_with("set") {
            let node = entry["node_data"].to_string();
            db.execute(
                "INSERT INTO persistent_items (item_key, item_value, node_path, node_data) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (item_key, item_value, node_path) DO UPDATE SET node_data = excluded.node_data",
                &[&key, &value, &path, &node],
            )
            .await?;
        } else if method.starts_with("del") {
            db.execute(
                "DELETE FROM persistent_items WHERE item_key = $1 AND item_value = $2 AND ($3 = '' OR node_path = $3)",
                &[&key, &value, &path],
            )
            .await?;
        } else {
            let rows = db
                .query(
                    "SELECT node_path, node_data FROM persistent_items
                     WHERE item_key = $1 AND item_value = $2 AND ($3 = '' OR node_path = $3)",
                    &[&key, &value, &path],
                )
                .await?;
            let nodes = rows
                .iter()
                .map(|row| {
                    let stored: String = row.get("node_data");
                    (row.get::<_, String>("node_path"), serde_json::from_str(&stored).unwrap_or(Value::Null))
                })
                .collect();
            looked_up.push((entry, nodes));
        }
    }
    if method.starts_with(PERSISTENT_GET) {
        return Ok(variables_reply(&looked_up).to_string());
    }
    Ok(json!({ "data_list": [] }).to_string())
}

/// The reply of a get: the counters and one `{key, result, data}` per variable asked, as the official
/// service answers it (`result` `VARIABLE_NOT_FOUND` and null `data` for one it holds nothing of;
/// the shape of a found one is assumed: its nodes by path).
fn variables_reply(looked_up: &[(Value, Vec<(String, Value)>)]) -> Value {
    let mut found = 0;
    let mut missing = 0;
    let data_list: Vec<Value> = looked_up
        .iter()
        .map(|(entry, nodes)| {
            let key = entry["key"].as_str().unwrap_or(PERSISTENT_DEFAULT_KEY);
            if nodes.is_empty() {
                missing += 1;
                json!({ key: entry["value"], "result": VARIABLE_NOT_FOUND, "data": null })
            } else {
                found += nodes.len();
                let data: serde_json::Map<String, Value> = nodes.iter().cloned().collect();
                json!({ key: entry["value"], "result": VARIABLE_FOUND, "data": data })
            }
        })
        .collect();
    json!({
        "success": true,
        "result": VARIABLE_FOUND,
        "msg": "",
        "num_in_cache": 0,
        "num_from_db": found,
        "num_nodes_found": found,
        "num_nodes_not_found": 0,
        "num_variables_not_found": missing,
        "data_list": data_list,
    })
}

/// The call a join makes to have the player's entitlements at its location.
const CLAIM_METHOD: &str = "entitlement.inventory.location";

/// Calls without a store behind them succeed with an empty result. `entitlement.inventory.location`
/// needs that: the join only continues after a success code, and it times out without a response.
fn rpc_response(request: &RpcRequest) -> RpcResponse {
    let source = request.source.unwrap_or(RpcGuid { high: 0, low: 0 });
    RpcResponse {
        body: "{}".to_string(),
        call_id: request.call_id(),
        code: RPC_SUCCESS,
        message: String::new(),
        destination: source,
    }
}

fn join_update(address: &str, port: i32, shard: &str, location: u64) -> MatchMakerUpdate {
    MatchMakerUpdate {
        group_id: instance_id(0),
        status: JOIN_STATUS_READY,
        game_ip: address.to_string(),
        game_port: port,
        game_location: location,
        target_shard_id: shard.to_string(),
        ..Default::default()
    }
}

/// The `smsg_ag_plr_updated` that answers a status update: the player's names, the status echoed and the recorded constants.
fn player_updated(handle: &str, name: &str, status: i32) -> PlayerUpdated {
    PlayerUpdated {
        handle: Some(handle.to_string()),
        name: Some(name.to_string()),
        field3: Some(PLAYER_UPDATED_ZERO),
        status: Some(status),
        field5: Some(PLAYER_UPDATED_FIELD5),
        group: Some(PlayerUpdatedGroup { first: Some(String::new()), second: Some(String::new()) }),
        field7: Some(PLAYER_UPDATED_ZERO),
    }
}

/// The `msg_sys_notify` sequence the official Hub sends around a join, in its order, for the shard `update` routes to.
fn join_notifications(update: &MatchMakerUpdate) -> Vec<SysNotify> {
    let shard = |code, field5: Option<i32>| SysNotify {
        field1: Some(SYS_NOTIFY_FIELD1),
        code: Some(code),
        field5,
        shard: Some(update.target_shard_id.clone()),
        ..Default::default()
    };
    vec![
        SysNotify { field1: Some(SYS_NOTIFY_FIELD1), code: Some(SYS_NOTIFY_CODE_START), ..Default::default() },
        shard(SYS_NOTIFY_CODE_ALLOCATED, Some(SYS_NOTIFY_ALLOCATED_FLAG)),
        shard(SYS_NOTIFY_CODE_ROUTING, None),
        SysNotify {
            address: Some(update.game_ip.clone()),
            field5: Some(update.game_port),
            location: Some(update.game_location),
            ..shard(SYS_NOTIFY_CODE_READY, None)
        },
    ]
}

fn join_notify_enabled() -> bool {
    std::env::var(JOIN_NOTIFY_SETTING).is_ok_and(|value| JOIN_NOTIFY_ON.contains(&value.to_ascii_lowercase().as_str()))
}

/// Picks the shard for a join: from the operator when there is one, or the configured shard.
async fn route_to_shard(shard: &ShardInfo, region: &str, location: u64) -> Option<MatchMakerUpdate> {
    let Some(directory) = &shard.directory else {
        return Some(join_update(&shard.public_ip, i32::from(shard.port), &shard.name, location));
    };
    let request = AllocateShardRequest { region: region.to_string() };
    match directory.clone().allocate(request).await {
        Ok(reply) => {
            let reply = reply.into_inner();
            info!(shard = %reply.name, address = %reply.public_address, port = reply.public_port, "routing client to shard");
            Some(join_update(&reply.public_address, reply.public_port as i32, &reply.name, location))
        }
        Err(error) => {
            warn!(region, %error, "no shard for this join");
            None
        }
    }
}

struct Session {
    state: Arc<State>,
    db: Postgres,
    shard: ShardInfo,
    region: String,
    account: Option<Account>,
    default_handle: String,
    documents: EntityDocumentInternalClient<Channel>,
    identity: IdentityInternalClient<Channel>,
    claims: Option<claim::Claims>,
    graph: Option<EntityGraphInternalClient<Channel>>,
    /// The session this connection issued, which ends with it.
    session: Option<String>,
}

impl Session {
    /// Routes a join and remembers the shard, which the social services report as the player's instance.
    async fn join(&self, location: u64, envelope: &Envelope) -> Vec<Vec<u8>> {
        let Some(update) = route_to_shard(&self.shard, &self.region, location).await else { return Vec::new() };
        if let Some(account) = &self.account {
            let stored = self
                .db
                .execute(
                    "INSERT INTO player_shards (geid, shard_id, public_address, public_port, location_id)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (geid) DO UPDATE SET shard_id = $2, public_address = $3, public_port = $4,
                         location_id = $5, joined_at = now()",
                    &[&(account.geid as i64), &update.target_shard_id, &update.game_ip, &update.game_port, &(location as i64)],
                )
                .await;
            if let Err(error) = stored {
                warn!(%error, "could not record the player's shard");
            }
        }
        let mut frames: Vec<Vec<u8>> = if join_notify_enabled() {
            join_notifications(&update).into_iter().map(|notice| frame("msg_sys_notify", notice, 0)).collect()
        } else {
            Vec::new()
        };
        frames.push(frame("smsg_do_join_instance", update, envelope.id));
        frames
    }

    /// The Hub connection lasts as long as the game session, so its end means the player left: the session ends, the shard slot
    /// is freed, and sc-entitygraph is told as for any disconnect (`disconnect`). A replicant that reports the same player
    /// afterwards finds the slot already gone and the event recorded.
    async fn leave(&self) {
        if let Some(session) = &self.session {
            if let Err(error) = self.db.execute("DELETE FROM sessions WHERE session = $1", &[session]).await {
                warn!(%error, "could not end the session");
            }
        }
        let Some(account) = &self.account else { return };
        let geid = account.geid as i64;
        let shard = match self.db.query_opt("SELECT shard_id FROM player_shards WHERE geid = $1", &[&geid]).await {
            Ok(row) => row.map(|row| row.get::<_, String>(0)),
            Err(error) => {
                warn!(%error, "could not read the player's shard");
                None
            }
        };
        if let Err(error) = self.db.execute("DELETE FROM player_shards WHERE geid = $1", &[&geid]).await {
            warn!(%error, "could not clear the player's shard");
        }
        let Some(session) = &self.session else { return };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        // Without a shard row the player was in the menu, with nothing live: the event still reaches sc-entitygraph, for the
        // markers a lost report may have left on any shard.
        let event = PlayerDisconnect {
            event_id: format!("hub:{session}"),
            reason: DisconnectReason::ConnectionLost as i32,
            account_id: account.account_id,
            geid: account.geid,
            shard_id: shard.unwrap_or_default(),
            client_code: 0,
            connected_at_ms: 0,
            at_ms: now,
        };
        if let Err(error) = disconnect::handle(&self.db, self.graph.clone(), event).await {
            warn!(%error, "could not record the disconnect");
        }
    }

    /// The claim of the player's entitled items at the location of the call. Only logged when it cannot be done.
    async fn claim_entitlements(&self, body: &str) {
        let (Some(claims), Some(account)) = (&self.claims, &self.account) else { return };
        match claim::location_of(body) {
            Some(location) => claims.claim(account.account_id, account.geid, location).await,
            None => warn!(body, "a claim with no location"),
        }
    }

    /// Returns the frames to send back for one envelope.
    async fn handle(&mut self, envelope: &Envelope) -> Vec<Vec<u8>> {
        match envelope.key.as_str() {
            "account_login" => {
                let token = AccountLogin::decode(envelope.payload.as_slice()).unwrap_or_default().token;
                let Some(mut account) = self
                    .state
                    .verify_token(&token)
                    .or_else(|| self.state.account_by_handle(&self.default_handle))
                else {
                    warn!("account_login with no usable account");
                    return Vec::new();
                };
                let (handle, display_name) =
                    live_names(&self.identity, account.geid).await.unwrap_or_else(|| (account.handle.clone(), account.handle.clone()));
                account.handle = handle;
                let home = home_location(&self.documents, account.geid).await;
                let size = hangar_size(&self.documents, account.geid).await;
                // A session no shard could look up is worth nothing: without one recorded, the login is refused and the
                // client tries again.
                let session = match issue_session(&self.db, &account, home, size).await {
                    Ok(session) => session,
                    Err(error) => {
                        warn!(%error, handle = %account.handle, "no session for the login");
                        return Vec::new();
                    }
                };
                info!(handle = %account.handle, home, "hub login");
                if let Some(previous) = self.session.replace(session.clone()) {
                    let _ = self.db.execute("DELETE FROM sessions WHERE session = $1", &[&previous]).await;
                }
                let reply = login_response(&account, &display_name, &session);
                self.account = Some(account);
                vec![frame("account_login_response", reply, envelope.id)]
            }
            "diff.rpc.request" => {
                let Ok(request) = RpcRequest::decode(envelope.payload.as_slice()) else {
                    warn!(bytes = envelope.payload.len(), "undecodable RPC request");
                    return Vec::new();
                };
                let mut response = rpc_response(&request);
                if request.method() == CLAIM_METHOD {
                    self.claim_entitlements(request.body()).await;
                }
                match request.method().strip_prefix(PERSISTENT_PREFIX) {
                    Some(method) => match persistent_items(&self.db, method, request.body()).await {
                        Ok(body) => response.body = body,
                        Err(error) => {
                            warn!(method, %error, "persistent item call failed");
                            response.code = RPC_FAILED;
                            response.message = error;
                        }
                    },
                    None => debug!(method = request.method(), body = request.body(), "RPC answered with an empty success"),
                }
                vec![frame("diff.rpc.response", response, envelope.id)]
            }
            "cmsg_set_region_id" => {
                let id = RegionSelection::decode(envelope.payload.as_slice()).unwrap_or_default().region_id;
                if let Some(region) = self.shard.region_map.get(&id) {
                    self.region = region.clone();
                }
                debug!(id, region = %self.region, "client region");
                Vec::new()
            }
            // The client announcing itself. There is nothing to answer.
            "diff.service.online" => Vec::new(),
            "cmsg_ls_goto_loc" => {
                let requested = GotoLocation::decode(envelope.payload.as_slice()).unwrap_or_default().location_id;
                let location = if requested == 0 { self.shard.default_location() } else { requested };
                info!(location = format!("{location:#x}"), "join requested");
                self.join(location, envelope).await
            }
            "msg_ping" => vec![raw_frame("msg_pong", &envelope.payload, envelope.id)],
            "heartbeat" => Vec::new(),
            // The client's arena-group status after a join (4). The official Hub answers with the player's names and the status echoed.
            "cmsg_ag_update_plr_status" => {
                let status = PlayerStatusRequest::decode(envelope.payload.as_slice()).unwrap_or_default().status();
                let Some(account) = &self.account else { return Vec::new() };
                debug!(status, "player status update");
                vec![frame("smsg_ag_plr_updated", player_updated(&account.handle, &account.handle, status), envelope.id)]
            }
            "cmsg_ls_req_dests" => {
                info!(count = self.shard.destinations.len(), "destination list requested");
                let players = players_by_location(&self.db, &self.shard.name).await;
                vec![frame("smsg_ls_dest_list", destination_list(&self.shard, &players), envelope.id)]
            }
            "slash.joininstance" | "cmsg_ls_goto_inst" => {
                let payload = hex_preview(&envelope.payload);
                info!(key = %envelope.key, %payload, "join requested");
                let location = self.shard.default_location();
                self.join(location, envelope).await
            }
            key => {
                let payload = hex_preview(&envelope.payload);
                warn!(key, id = envelope.id, bytes = envelope.payload.len(), %payload, "unimplemented hub message");
                Vec::new()
            }
        }
    }
}

/// Where a player is, with the players around it. The columns are read as nullable: an older sc-social created this
/// table too, without NOT NULL.
async fn player_shard(db: &Postgres, geid: u64) -> Result<Option<PlayerShard>, String> {
    let row = db
        .query_opt(
            "SELECT p.shard_id, p.public_address, p.public_port, p.location_id,
                 (SELECT count(*) FROM player_shards o
                  WHERE o.shard_id = p.shard_id AND o.location_id = p.location_id) AS players_at_location,
                 (SELECT count(*) FROM player_shards o WHERE o.shard_id = p.shard_id) AS players_on_shard
             FROM player_shards p WHERE p.geid = $1",
            &[&(geid as i64)],
        )
        .await?;
    Ok(row.map(|row| PlayerShard {
        shard_id: row.get::<_, Option<String>>("shard_id").unwrap_or_default(),
        public_address: row.get::<_, Option<String>>("public_address").unwrap_or_default(),
        public_port: row.get::<_, Option<i32>>("public_port").unwrap_or_default() as u32,
        location_id: row.get::<_, Option<i64>>("location_id").unwrap_or_default() as u64,
        players_at_location: row.get::<_, i64>("players_at_location") as u32,
        players_on_shard: row.get::<_, i64>("players_on_shard") as u32,
    }))
}

struct Internal {
    db: Postgres,
    /// sc-entitygraph, told when a player's connection ends.
    graph: Option<EntityGraphInternalClient<Channel>>,
}

#[tonic::async_trait]
impl HubInternal for Internal {
    async fn get_player_shard(
        &self,
        request: Request<GetPlayerShardRequest>,
    ) -> Result<Response<GetPlayerShardResponse>, Status> {
        let shard = player_shard(&self.db, request.into_inner().geid).await.map_err(internal)?;
        Ok(Response::new(GetPlayerShardResponse { shard }))
    }

    async fn report_disconnect(
        &self,
        request: Request<PlayerDisconnect>,
    ) -> Result<Response<ReportDisconnectResponse>, Status> {
        let event = request.into_inner();
        let (recorded, slot_freed) = disconnect::handle(&self.db, self.graph.clone(), event).await.map_err(|error| {
            if error.contains("needed") { Status::invalid_argument(error) } else { internal(error) }
        })?;
        Ok(Response::new(ReportDisconnectResponse { recorded, slot_freed }))
    }

    async fn resolve_session(
        &self,
        request: Request<ResolveSessionRequest>,
    ) -> Result<Response<ResolveSessionResponse>, Status> {
        let session = request.into_inner().session;
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT account_id, geid, nickname, home_location, hangar_size FROM sessions
                     WHERE session = $1 AND created_at >= now() - interval '{SESSION_MAX_AGE}'"
                ),
                &[&session.to_ascii_lowercase()],
            )
            .await
            .map_err(internal)?;
        let Some(row) = rows.first() else { return Ok(Response::new(ResolveSessionResponse::default())) };
        Ok(Response::new(ResolveSessionResponse {
            found: true,
            account_id: u32::try_from(row.get::<_, i64>(0)).unwrap_or_default(),
            geid: row.get::<_, i64>(1) as u64,
            nickname: row.get(2),
            home_location: row.get::<_, Option<i64>>(3).and_then(|location| u32::try_from(location).ok()).unwrap_or_default(),
            hangar_size: row.get::<_, i32>(4),
        }))
    }
}

async fn serve_internal(addr: SocketAddr, db: Postgres, graph: Option<EntityGraphInternalClient<Channel>>) -> Result<(), String> {
    info!(%addr, "hub internal gRPC listening");
    Server::builder()
        .add_service(HubInternalServer::new(Internal { db, graph }))
        .serve(addr)
        .await
        .map_err(|e| format!("hub internal {addr}: {e}"))
}

async fn run_session<S>(mut stream: S, session: &mut Session) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut header = [0u8; 8];
    loop {
        stream.read_exact(&mut header).await?;
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        if u32::from_le_bytes(header[4..].try_into().unwrap()) != FRAME_MAGIC || length > MAX_FRAME_BYTES {
            warn!(length, "bad hub frame header, closing");
            return Ok(());
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await?;
        let Ok(envelope) = Envelope::decode(body.as_slice()) else {
            warn!(length, "undecodable hub envelope");
            continue;
        };
        debug!(key = %envelope.key, id = envelope.id, "hub message");
        for frame in session.handle(&envelope).await {
            stream.write_all(&frame).await?;
        }
        stream.flush().await?;
    }
}

/// The services a connection asks.
#[derive(Clone)]
struct Backends {
    documents: EntityDocumentInternalClient<Channel>,
    identity: IdentityInternalClient<Channel>,
    claims: Option<claim::Claims>,
    graph: Option<EntityGraphInternalClient<Channel>>,
}

async fn accept_loop(
    addr: SocketAddr,
    tls: Arc<ServerConfig>,
    state: Arc<State>,
    db: Postgres,
    shard: ShardInfo,
    default_handle: String,
    backends: Backends,
) -> Result<(), String> {
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("hub bind {addr}: {e}"))?;
    let acceptor = TlsAcceptor::from(tls);
    info!(%addr, "hub listening (TLS)");
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(%error, "hub accept failed");
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let acceptor = acceptor.clone();
        let mut session = Session {
            state: state.clone(),
            db: db.clone(),
            region: shard.region.clone(),
            shard: shard.clone(),
            account: None,
            default_handle: default_handle.clone(),
            documents: backends.documents.clone(),
            identity: backends.identity.clone(),
            claims: backends.claims.clone(),
            graph: backends.graph.clone(),
            session: None,
        };
        tokio::spawn(async move {
            match acceptor.accept(socket).await {
                Ok(stream) => {
                    info!(%peer, "hub connected");
                    let result = run_session(stream, &mut session).await;
                    session.leave().await;
                    info!(%peer, ?result, "hub disconnected");
                }
                Err(error) => warn!(%peer, %error, "hub TLS handshake failed"),
            }
        });
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let state = Arc::new(State::load(&args.common.data)?);
    let directory = match &args.shard_directory {
        Some(url) => {
            let endpoint = Endpoint::from_shared(url.clone()).map_err(|e| format!("--shard-directory: {e}"))?;
            Some(ShardDirectoryClient::new(endpoint.connect_lazy()))
        }
        None => None,
    };
    let region_map = args
        .region_map
        .iter()
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (id, region) = entry.split_once('=').ok_or_else(|| format!("bad region map entry '{entry}'"))?;
            Ok((id.parse().map_err(|e| format!("bad region id '{id}': {e}"))?, region.to_string()))
        })
        .collect::<Result<_, String>>()?;
    let shard = ShardInfo {
        directory,
        region: args.region,
        region_map,
        public_ip: args.public_ip,
        port: args.shard_port,
        name: args.shard_name,
        destinations: destinations::load(&args.common.data)?,
        max_players: 100,
    };
    let db = Postgres::connect(&args.database.postgres, &schema()).await?;
    let config = tls::plain(&args.common.data)?;
    let endpoint =
        Endpoint::from_shared(args.entity_document.clone()).map_err(|e| format!("--entity-document: {e}"))?;
    let documents = EntityDocumentInternalClient::new(endpoint.connect_lazy());
    let identity = IdentityInternalClient::new(
        Endpoint::from_shared(args.identity.clone()).map_err(|e| format!("--identity: {e}"))?.connect_lazy(),
    );
    let claims = match (&args.entitlement, &args.entitygraph) {
        (Some(entitlement), Some(entitygraph)) => {
            let channel = |address: &String, flag: &str| {
                Endpoint::from_shared(address.clone())
                    .map(|endpoint| endpoint.connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(10)).connect_lazy())
                    .map_err(|e| format!("--{flag}: {e}"))
            };
            info!(%entitlement, %entitygraph, "entitlements are claimed as items at the joined location");
            Some(claim::Claims {
                entitlements: sc_api::internal::entitlement_internal_client::EntitlementInternalClient::new(channel(entitlement, "entitlement")?),
                graph: sc_api::internal::entity_graph_internal_client::EntityGraphInternalClient::new(channel(entitygraph, "entitygraph")?),
            })
        }
        _ => {
            warn!("no --entitlement and --entitygraph, joins claim no items");
            None
        }
    };
    // sc-entitygraph is told when a player's connection ends, whether or not entitlements are claimed.
    let graph = match &args.entitygraph {
        Some(address) => {
            let endpoint = Endpoint::from_shared(address.clone()).map_err(|e| format!("--entitygraph: {e}"))?;
            Some(EntityGraphInternalClient::new(endpoint.connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(10)).connect_lazy()))
        }
        None => {
            warn!("no --entitygraph, a player's live markers are not cleared when they leave");
            None
        }
    };
    tokio::spawn(disconnect::retry_loop(db.clone(), graph.clone()));
    tokio::select! {
        result = accept_loop(args.listen, config, state, db.clone(), shard, args.handle, Backends { documents, identity, claims, graph: graph.clone() }) => result,
        result = serve_internal(args.internal_listen, db, graph) => result,
    }
}

/// The Hub's tables: its own and those of the disconnects it records.
fn schema() -> String {
    format!("{SCHEMA}{}", disconnect::SCHEMA)
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_32_random_hex_digits() {
        let (a, b) = (new_session().unwrap(), new_session().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn frame_matches_the_documented_layout() {
        let frame = raw_frame("heartbeat", &[1, 2], 0);
        assert_eq!(&frame[4..8], &[0xef, 0xbe, 0xad, 0xde]);
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize, frame.len() - 8);
        // Byte-exact against the envelope the client's serializer produces.
        assert_eq!(&frame[8..19], b"\x0a\x09heartbeat");
        let envelope = Envelope::decode(&frame[8..]).unwrap();
        assert_eq!((envelope.key.as_str(), envelope.payload.as_slice()), ("heartbeat", &[1u8, 2][..]));
    }

    #[test]
    fn every_destination_gets_a_guid_instance() {
        let shard = ShardInfo {
            directory: None,
            region: "usa".into(),
            region_map: HashMap::new(),
            public_ip: "127.0.0.2".into(),
            port: 1,
            name: "s".into(),
            destinations: destinations::builtin(),
            max_players: 10,
        };
        let list = destination_list(&shard, &HashMap::from([(0x0002_0001_0001_0001, 4)]));
        assert_eq!(list.list.len(), 3, "the sentinel must not be sent");
        assert_eq!(list.list[1].instance_list[0].instance_id, "00000000-0000-4000-8000-000000000002");
        assert_eq!(shard.default_location(), 0x0002_0001_0001_0001);
        let counts: Vec<i32> = list.list.iter().map(|d| d.instance_list[0].num_players.unwrap()).collect();
        assert_eq!(counts.iter().sum::<i32>(), 4, "only the destination with players reports them");
    }

    /// The game declares `num_players` required, and proto3 leaves out a zero, which made the client refuse the whole
    /// destination list ("missing required fields: list[0].instance_list[0].num_players").
    #[test]
    fn an_instance_always_carries_num_players() {
        let entry = InstanceEntry { instance_id: "i".into(), max_players: 10, num_players: Some(0), ..Default::default() };
        // instance_id, max_players 10, then field 3 (tag 0x18) with its zero.
        assert_eq!(entry.encode_to_vec(), [0x0a, 1, b'i', 0x10, 10, 0x18, 0]);
    }

    #[test]
    fn rpc_response_writes_every_required_field_and_echoes_the_call() {
        let request = RpcRequest {
            method: Some("entitlement.inventory.location".into()),
            call_id: Some(10001),
            source: Some(RpcGuid { high: 5, low: 0 }),
            ..Default::default()
        };
        let bytes = rpc_response(&request).encode_to_vec();
        // body "{}", call id 10001, code 0, empty message, destination { 5, 0 }
        assert_eq!(bytes, [0x0a, 2, b'{', b'}', 0x10, 0x91, 0x4e, 0x18, 0, 0x22, 0, 0x2a, 4, 8, 5, 0x10, 0]);
    }

    #[test]
    fn goto_location_decodes_the_bytes_the_client_sends() {
        let payload = [0x08, 0x81, 0x80, 0x84, 0x80, 0x90, 0x80, 0xc0, 0x01];
        assert_eq!(GotoLocation::decode(&payload[..]).unwrap().location_id, 0x0003_0001_0001_0001);
    }

    #[test]
    fn a_get_of_an_unknown_variable_is_answered_as_the_live_service_does() {
        let asked = json!({ "key": "geid", "value": 924458801418u64, "node_path": "lrc.impoundingData" });
        let reply = variables_reply(&[(asked, Vec::new())]);
        let live: Value = serde_json::from_str(
            r#"{"success":true,"result":0,"msg":"","num_in_cache":0,"num_from_db":0,"num_nodes_found":0,"num_nodes_not_found":0,"num_variables_not_found":1,"data_list":[{"geid":924458801418,"result":-5,"data":null}]}"#,
        )
        .unwrap();
        assert_eq!(reply, live);
    }

    #[test]
    fn persistent_requests_split_into_entries() {
        let bulk = json!({ "get_partial_list": [{ "key": "geid", "value": 1 }, { "key": "geid", "value": 2 }] });
        assert_eq!(persistent_entries(&bulk).len(), 2);
        let single = json!({ "key": "geid", "value": 1, "node_path": "x" });
        assert_eq!(persistent_entries(&single), [single.clone()]);
    }

    fn bytes_of(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
    }

    /// The official Hub's `smsg_ag_plr_updated` for the status 4.
    const LIVE_PLAYER_UPDATED: &str = "0a0f4578616d706c6570696c6f74303031120e4578616d706c6570696c6f74303118002004286732040a0012003800";
    const LIVE_SHARD: &str = "insightful_propulsion";
    const LIVE_ADDRESS: &str = "198.51.100.58";
    const LIVE_PORT: i32 = 64311;
    const LIVE_LOCATION: u64 = 18_446_462_603_027_873_793;

    #[test]
    fn a_status_update_is_answered_as_the_official_hub_does() {
        let request = PlayerStatusRequest::decode(bytes_of("0804").as_slice()).unwrap();
        assert_eq!(request.status(), 4);
        let reply = player_updated("Examplepilot001", "Examplepilot01", request.status());
        assert_eq!(reply.encode_to_vec(), bytes_of(LIVE_PLAYER_UPDATED));
    }

    #[test]
    fn the_join_notifications_are_the_five_official_frames_but_the_repeated_one() {
        let update = join_update(LIVE_ADDRESS, LIVE_PORT, LIVE_SHARD, LIVE_LOCATION);
        let notices: Vec<Vec<u8>> = join_notifications(&update).iter().map(Message::encode_to_vec).collect();
        assert_eq!(notices[0], bytes_of("08011002"));
        assert_eq!(notices[1], bytes_of("0801101b28013a15696e736967687466756c5f70726f70756c73696f6e"));
        assert_eq!(notices[2], bytes_of("080110033a15696e736967687466756c5f70726f70756c73696f6e"));
        assert_eq!(
            notices[3],
            bytes_of("08011004220d3139382e35312e3130302e353828b7f60330818084809080c0ffff013a15696e736967687466756c5f70726f70756c73696f6e")
        );
    }

    #[test]
    fn frames_keep_all_required_envelope_fields_when_empty() {
        let frame = raw_frame("k", &[], 0);
        assert_eq!(frame[8..], [0x0a, 0x01, b'k', 0x12, 0x00, 0x18, 0x00]);
    }

    #[test]
    fn login_reply_writes_every_field_the_client_requires() {
        let account = Account { account_id: 7, handle: "pilot".into(), geid: 9, created_at_ms: 0 };
        let reply = login_response(&account, "Pilot Name", "s");
        assert_eq!(reply.account.as_ref().unwrap().handle(), "pilot");
        assert_eq!(reply.account.as_ref().unwrap().display_name(), "Pilot Name");
        let bytes = reply.encode_to_vec();
        assert_eq!(bytes[..2], [0x08, LOGIN_RESULT_SUCCESS as u8]);
        // Fields 5 and 6 must be on the wire even as zero, or IsInitialized fails in the client.
        assert_eq!(bytes[bytes.len() - 4..], [0x28, 0x00, 0x30, 0x00]);
    }

    /// Against a real YugabyteDB, in a private schema of the server in `SCBACKEND_TEST_POSTGRES`. Without it the test
    /// does nothing, so `cargo test` stays offline.
    #[tokio::test]
    async fn a_player_shard_comes_with_the_players_around_it() {
        use sc_api::internal::hub_internal_client::HubInternalClient;

        let Ok(base) = std::env::var("SCBACKEND_TEST_POSTGRES") else { return };
        let schema = format!("hub_test_{}", std::process::id());
        let admin = Postgres::connect(&base, &format!("CREATE SCHEMA {schema}")).await.unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let db = Postgres::connect(&format!("{base}{separator}options=-c%20search_path%3D{schema}"), &super::schema()).await.unwrap();
        let insert = "INSERT INTO player_shards (geid, shard_id, public_address, public_port, location_id)
                      VALUES ($1, $2, '10.0.0.1', 7000, $3)";
        db.execute(insert, &[&1_i64, &"shard-a", &5_i64]).await.unwrap();
        db.execute(insert, &[&2_i64, &"shard-a", &6_i64]).await.unwrap();
        db.execute(insert, &[&3_i64, &"shard-b", &5_i64]).await.unwrap();

        let address: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tokio::spawn(serve_internal(address, db.clone(), None));
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut client = HubInternalClient::connect(format!("http://{address}")).await.unwrap();
        let shard = client.get_player_shard(GetPlayerShardRequest { geid: 1 }).await.unwrap().into_inner().shard.unwrap();
        assert_eq!((shard.shard_id.as_str(), shard.public_address.as_str(), shard.public_port), ("shard-a", "10.0.0.1", 7000));
        assert_eq!((shard.location_id, shard.players_at_location, shard.players_on_shard), (5, 1, 2));
        let nowhere = client.get_player_shard(GetPlayerShardRequest { geid: 4 }).await.unwrap().into_inner();
        assert!(nowhere.shard.is_none());

        admin.execute(&format!("DROP SCHEMA {schema} CASCADE"), &[]).await.unwrap();
    }

    /// The table as an older sc-social created it, with no NOT NULL and the index already there: the Hub's schema
    /// applies over it, and a row of nulls reads as empty.
    #[tokio::test]
    async fn the_table_sc_social_created_still_reads() {
        let Ok(base) = std::env::var("SCBACKEND_TEST_POSTGRES") else { return };
        let schema = format!("hub_legacy_test_{}", std::process::id());
        let admin = Postgres::connect(&base, &format!("CREATE SCHEMA {schema}")).await.unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{schema}");
        let legacy = "CREATE TABLE player_shards (geid BIGINT PRIMARY KEY, shard_id TEXT, public_address TEXT,
                      public_port INT, location_id BIGINT, joined_at TIMESTAMPTZ);
                      CREATE INDEX player_shards_shard ON player_shards (shard_id);";
        let db = Postgres::connect(&url, legacy).await.unwrap();
        db.execute("INSERT INTO player_shards (geid) VALUES (9)", &[]).await.unwrap();
        let db = Postgres::connect(&url, &super::schema()).await.unwrap();
        let shard = player_shard(&db, 9).await.unwrap().unwrap();
        assert_eq!((shard.shard_id.as_str(), shard.public_address.as_str(), shard.public_port, shard.location_id), ("", "", 0, 0));
        assert_eq!((shard.players_at_location, shard.players_on_shard), (0, 0));
        admin.execute(&format!("DROP SCHEMA {schema} CASCADE"), &[]).await.unwrap();
    }
}
