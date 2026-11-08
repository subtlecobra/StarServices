//! The "Hub" (legacy Diffusion/CRYNUB connection, `star_network` in LoginData.json).
//!
//! TLS over TCP. Frames are `[u32 LE length][u32 LE 0xDEADBEEF][envelope]`, and the envelope is a
//! protobuf `{ string key = 1; bytes payload = 2; uint64 id = 3; }` routed by key.
//!
//! The Hub owns `player_shards`, the shard each connected player was sent to. Other services ask for it over
//! `HubInternal`, on a gRPC port of its own, as the Hub's port speaks only its TLS protocol.

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
    hub_internal_server::{HubInternal, HubInternalServer},
    shard_directory_client::ShardDirectoryClient,
    AllocateShardRequest, GetEntityDocumentsRequest, GetPlayerShardRequest, GetPlayerShardResponse, PlayerShard,
    ResolveSessionRequest, ResolveSessionResponse,
};
use sc_proto::services::entity_document::v1::entity_document_body::Class;
use sc_proto::hub::{
    AccountLogin, AccountLoginResponse, AccountRecord, Envelope, GotoLocation, InstanceEntry,
    LocationServicesDestination, LocationServicesDestinationList, MatchMakerUpdate, RegionSelection, RpcGuid,
    RpcRequest, RpcResponse,
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
);";
/// A session lasts as long as its Hub connection, and a day at most.
const SESSION_MAX_AGE: &str = "1 day";
const PERSISTENT_PREFIX: &str = "var.persistent_items.";

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
}

/// How long a login waits for the home location before going on without it.
const HOME_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

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

/// How many fresh sessions a login draws before giving up, should each one collide with a live one. Two 128-bit random
/// values coincide with odds of about 2^-128 each time, so a second draw is already beyond any real chance.
const SESSION_ATTEMPTS: usize = 5;

/// Records a new session for `account` and returns it. Its primary key keeps every live session unique: a draw that
/// collides with one writes no row, and another is drawn. Expired sessions are removed first (a crash can leave some).
async fn issue_session(db: &Postgres, account: &Account, home: Option<u32>) -> Result<String, String> {
    db.execute(&format!("DELETE FROM sessions WHERE created_at < now() - interval '{SESSION_MAX_AGE}'"), &[]).await?;
    for _ in 0..SESSION_ATTEMPTS {
        let session = new_session()?;
        let written = db
            .execute(
                "INSERT INTO sessions (session, account_id, geid, nickname, home_location) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (session) DO NOTHING",
                &[&session, &i64::from(account.account_id), &(account.geid as i64), &account.handle, &home.map(i64::from)],
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

/// Parsed as a GUID by the client (FUN_14042ab20 in the smsg_ls_dest_list handler).
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
fn login_response(account: &Account, session: &str) -> AccountLoginResponse {
    AccountLoginResponse {
        result: LOGIN_RESULT_SUCCESS,
        account: Some(AccountRecord {
            session: Some(session.to_string()),
            account_id: Some(account.account_id as i32),
            display_name: Some(account.handle.clone()),
            handle: Some(account.handle.clone()),
            geid: Some(account.geid),
            ..Default::default()
        }),
        field4: None,
        queue_wait: 0,
        field6: 0,
    }
}

fn destination_list(shard: &ShardInfo) -> LocationServicesDestinationList {
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
    let mut data = Vec::new();
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
            for row in rows {
                let stored: String = row.get("node_data");
                let node: Value = serde_json::from_str(&stored).unwrap_or(Value::Null);
                let path: String = row.get("node_path");
                data.push(json!({ "key": entry["key"], "value": entry["value"], "node_path": path, "node_data": node }));
            }
        }
    }
    Ok(json!({ "data_list": data }).to_string())
}

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
        vec![frame("smsg_do_join_instance", update, envelope.id)]
    }

    /// The Hub connection lasts as long as the game session, so its end means the player left.
    async fn leave(&self) {
        if let Some(session) = &self.session {
            if let Err(error) = self.db.execute("DELETE FROM sessions WHERE session = $1", &[session]).await {
                warn!(%error, "could not end the session");
            }
        }
        let Some(account) = &self.account else { return };
        if let Err(error) = self.db.execute("DELETE FROM player_shards WHERE geid = $1", &[&(account.geid as i64)]).await {
            warn!(%error, "could not clear the player's shard");
        }
    }

    /// Returns the frames to send back for one envelope.
    async fn handle(&mut self, envelope: &Envelope) -> Vec<Vec<u8>> {
        match envelope.key.as_str() {
            "account_login" => {
                let token = AccountLogin::decode(envelope.payload.as_slice()).unwrap_or_default().token;
                let Some(account) = self
                    .state
                    .verify_token(&token)
                    .or_else(|| self.state.account_by_handle(&self.default_handle))
                else {
                    warn!("account_login with no usable account");
                    return Vec::new();
                };
                let home = home_location(&self.documents, account.geid).await;
                // A session no shard could look up is worth nothing: without one recorded, the login is refused and the
                // client tries again.
                let session = match issue_session(&self.db, &account, home).await {
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
                let reply = login_response(&account, &session);
                self.account = Some(account);
                vec![frame("account_login_response", reply, envelope.id)]
            }
            "diff.rpc.request" => {
                let Ok(request) = RpcRequest::decode(envelope.payload.as_slice()) else {
                    warn!(bytes = envelope.payload.len(), "undecodable RPC request");
                    return Vec::new();
                };
                let mut response = rpc_response(&request);
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
            "cmsg_ls_req_dests" => {
                info!(count = self.shard.destinations.len(), "destination list requested");
                vec![frame("smsg_ls_dest_list", destination_list(&self.shard), envelope.id)]
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

    async fn resolve_session(
        &self,
        request: Request<ResolveSessionRequest>,
    ) -> Result<Response<ResolveSessionResponse>, Status> {
        let session = request.into_inner().session;
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT account_id, geid, nickname, home_location FROM sessions
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
        }))
    }
}

async fn serve_internal(addr: SocketAddr, db: Postgres) -> Result<(), String> {
    info!(%addr, "hub internal gRPC listening");
    Server::builder()
        .add_service(HubInternalServer::new(Internal { db }))
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

async fn accept_loop(
    addr: SocketAddr,
    tls: Arc<ServerConfig>,
    state: Arc<State>,
    db: Postgres,
    shard: ShardInfo,
    default_handle: String,
    documents: EntityDocumentInternalClient<Channel>,
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
            documents: documents.clone(),
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
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let config = tls::plain(&args.common.data)?;
    let endpoint =
        Endpoint::from_shared(args.entity_document.clone()).map_err(|e| format!("--entity-document: {e}"))?;
    let documents = EntityDocumentInternalClient::new(endpoint.connect_lazy());
    tokio::select! {
        result = accept_loop(args.listen, config, state, db.clone(), shard, args.handle, documents) => result,
        result = serve_internal(args.internal_listen, db) => result,
    }
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
        let list = destination_list(&shard);
        assert_eq!(list.list.len(), 3, "the sentinel must not be sent");
        assert_eq!(list.list[1].instance_list[0].instance_id, "00000000-0000-4000-8000-000000000002");
        assert_eq!(shard.default_location(), 0x0002_0001_0001_0001);
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
    fn persistent_requests_split_into_entries() {
        let bulk = json!({ "get_partial_list": [{ "key": "geid", "value": 1 }, { "key": "geid", "value": 2 }] });
        assert_eq!(persistent_entries(&bulk).len(), 2);
        let single = json!({ "key": "geid", "value": 1, "node_path": "x" });
        assert_eq!(persistent_entries(&single), [single.clone()]);
    }

    #[test]
    fn frames_keep_all_required_envelope_fields_when_empty() {
        let frame = raw_frame("k", &[], 0);
        assert_eq!(frame[8..], [0x0a, 0x01, b'k', 0x12, 0x00, 0x18, 0x00]);
    }

    #[test]
    fn login_reply_writes_every_field_the_client_requires() {
        let account = Account { account_id: 7, handle: "pilot".into(), geid: 9, created_at_ms: 0 };
        let reply = login_response(&account, "s");
        assert_eq!(reply.account.as_ref().unwrap().handle(), "pilot");
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
        let db = Postgres::connect(&format!("{base}{separator}options=-c%20search_path%3D{schema}"), SCHEMA).await.unwrap();
        let insert = "INSERT INTO player_shards (geid, shard_id, public_address, public_port, location_id)
                      VALUES ($1, $2, '10.0.0.1', 7000, $3)";
        db.execute(insert, &[&1_i64, &"shard-a", &5_i64]).await.unwrap();
        db.execute(insert, &[&2_i64, &"shard-a", &6_i64]).await.unwrap();
        db.execute(insert, &[&3_i64, &"shard-b", &5_i64]).await.unwrap();

        let address: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tokio::spawn(serve_internal(address, db.clone()));
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
        let db = Postgres::connect(&url, SCHEMA).await.unwrap();
        let shard = player_shard(&db, 9).await.unwrap().unwrap();
        assert_eq!((shard.shard_id.as_str(), shard.public_address.as_str(), shard.public_port, shard.location_id), ("", "", 0, 0));
        assert_eq!((shard.players_at_location, shard.players_on_shard), (0, 0));
        admin.execute(&format!("DROP SCHEMA {schema} CASCADE"), &[]).await.unwrap();
    }
}
