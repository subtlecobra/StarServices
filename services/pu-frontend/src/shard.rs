//! Shard UDP endpoint: the CryNub connect handshake.
//!
//! Plaintext packets are `[8-byte header][body][CRC-32C footer]`. The handshake exchanges three
//! secp128r1 keys per side; each ECDH secret goes through BLAKE2s-128 to give one key of the
//! channel. ReplicationData packets are decrypted, logged and acknowledged. Beyond that the shard
//! moves a client through its remote states and binds the entities the DGSs give it, which is what ends
//! the client's `WaitForGameRules`. Which entities those are is the DGSs' to say, see `link`: a stage
//! that needs what a DGS has not sent yet waits for it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use blake2::{digest::consts::U16, Blake2s, Digest};
use sc_proto::internal::{DgsFrames, DgsStatusResponse, EntityBind, InputFrame, PlayerMovement, Rotation};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::mpsc::unbounded_channel,
};
use tracing::{debug, info, warn};

use crate::dgs::Directory;
use crate::input::{action_frame, client_authority, indexes_hold, movement_update, MESSAGE_ENTITY_REMOTE_METHOD};
use crate::link::{Event, Identity, Link, PlayerEntities, Session, World};
use crate::{
    channel::{
        acknowledges, bind_batch, bind_batch_end, completed_batch, debug_server_info, debug_server_stats, greeting, later_packet, messages, normalized_header, packet_runs, reported_state, state_change,
        update_bubble_create, MAX_PACKET_MESSAGES, Channel, DataHeader, Receiver, FLAG_ENCRYPTED, FLAG_PLAINTEXT, FLAG_RESEND, HEADER_LEN,
    },
    curve::{KeyPair, Point, KEY_LEN as PUBKEY_LEN},
};

const HEADER_MAGIC: u8 = 0x5c;
const CONNECT_REQUEST: u8 = 0;
const CONNECT_ACK: u8 = 1;
const PING: u8 = 3;
const PONG: u8 = 4;
const REPLICATION_DATA: u8 = 7;
const REPLICATION_ACK: u8 = 8;
/// `eCVS_ChangeServer`, the state the client waits for first.
const VIEW_STATE: u8 = 3;
/// `eCVS_UnstowPlayer`, which the client waits for once its model has reached `VIEW_STATE`.
const UNSTOW_STATE: u8 = 14;
/// `eCVS_ReplicateUniverseRoot`, where the client blocks in `WaitForGameRules`.
const UNIVERSE_ROOT_STATE: u8 = 10;
/// `eCVS_ReplicateSolarSystem`. A client reporting this state is polling for the solar systems' binds.
const SOLAR_SYSTEM_STATE: u8 = 11;
const STATE_REPLICATE_STREAMABLES: u8 = 15;
const STATE_WAIT_FOR_ASYNC_SPAWNS: u8 = 16;
const STATE_IN_GAME: u8 = 17;
/// Any id works, as the client only matches it across the messages of a batch.
const FIRST_BATCH: u32 = 1;
const KEEP_ALIVE: u8 = 2;
const GREETING_RESEND_PERIOD: Duration = Duration::from_secs(1);
const PING_BODY_LEN: usize = 8;
const CONNECT_METADATA_BYTE: u8 = 0xdc;
const CONNECT_METADATA_U32: u32 = 0x613e_e7c5;
/// The bit after the keys of an ack: the sender's flag at 0x149e40d6c (0x1414a5a90), which the client keeps at `CNetNub+0x234`
/// (0x1414a2be0). The overlay draws its `Server:` line only with it set. Clear on a client and on a game server, by the
/// code that reads it, so this is what a replicant sends.
const ACK_FROM_REPLICANT: u8 = 1;
const REQUEST_MAGIC_NORMAL: u32 = 0xdd3a_1ee5;
const REQUEST_MAGIC_EXTENDED: u32 = 0x794f_de53;
const MAX_LOGGED_PACKETS: u64 = 16;
const MAX_LOGGED_DATA: u64 = 64;
/// A player counts as gone after this long without a packet.
const PEER_TIMEOUT: Duration = Duration::from_secs(60);
/// The client resets itself to the server's position when it is 0.5 m away for 0.1 s, which at the run speed of 5.7 m/s
/// leaves about 0.19 s for the whole round: frames go to the DGS as they come, one call at a time, and its answer goes to
/// the client at once. That is at most a call and an update per client frame, 30 a second, and none while it stands still.
const PRUNE_PERIOD: Duration = Duration::from_secs(5);
/// How often a player standing still is told the frame its state is of, which otherwise goes only with a move or a turn.
/// The client's `ValidateLastActionFrameEndTimestamp` (0x145b13de0) warns of a frame time unchanged for 60 s; a real
/// server names a new frame in every one of its frames.
const FRAME_TIME_REFRESH: Duration = Duration::from_secs(10);

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The CRC covers the header with byte 2 reduced to its low flag bits and bytes 4 to 7 cleared.
fn packet_crc(header: &[u8], body: &[u8]) -> u32 {
    crc32c(&[&normalized_header(header), body].concat())
}

fn read_varint(buf: &[u8], offset: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    for shift in (0..35).step_by(7) {
        let byte = *buf.get(*offset)?;
        *offset += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn read_blob<'a>(buf: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    let length = read_varint(buf, offset)?;
    let end = offset.checked_add(length).filter(|end| *end <= buf.len())?;
    let blob = &buf[*offset..end];
    *offset = end;
    Some(blob)
}

#[derive(Debug, PartialEq, Eq)]
struct ConnectRequest {
    session_guid: Vec<u8>,
    pubkeys: [[u8; PUBKEY_LEN]; 3],
}

fn parse_connect_request(body: &[u8]) -> Option<ConnectRequest> {
    let magic = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
    let mut offset = 4;
    let session_guid = read_blob(body, &mut offset)?.to_vec();
    match magic {
        REQUEST_MAGIC_NORMAL => {}
        REQUEST_MAGIC_EXTENDED => {
            read_blob(body, &mut offset)?; // service_id
            offset = offset.checked_add(16)?; // m_val
        }
        _ => return None,
    }
    let keys = body.get(offset..offset.checked_add(PUBKEY_LEN * 3)?)?;
    let mut pubkeys = [[0u8; PUBKEY_LEN]; 3];
    for (slot, chunk) in pubkeys.iter_mut().zip(keys.chunks_exact(PUBKEY_LEN)) {
        slot.copy_from_slice(chunk);
    }
    Some(ConnectRequest { session_guid, pubkeys })
}

fn derive_key(secret: &[u8]) -> [u8; 16] {
    Blake2s::<U16>::digest(secret).into()
}

/// Builds the CONNECT_ACK and the three session keys.
fn accept_connect(request: &ConnectRequest) -> Option<(Vec<u8>, [[u8; 16]; 3])> {
    let mut body = Vec::with_capacity(PUBKEY_LEN * 3 + 1);
    let mut keys = [[0u8; 16]; 3];
    for (key, peer) in keys.iter_mut().zip(&request.pubkeys) {
        let peer = Point::parse(peer)?;
        let pair = KeyPair::generate();
        body.extend_from_slice(&pair.public.encode());
        *key = derive_key(&pair.shared_secret(&peer)?);
    }
    body.push(ACK_FROM_REPLICANT);

    let mut packet = vec![HEADER_MAGIC, CONNECT_ACK, FLAG_PLAINTEXT, CONNECT_METADATA_BYTE];
    packet.extend_from_slice(&CONNECT_METADATA_U32.to_le_bytes());
    let crc = packet_crc(&packet, &body);
    packet.extend_from_slice(&body);
    packet.extend_from_slice(&crc.to_le_bytes());
    Some((packet, keys))
}

const SOLAR_SYSTEM_BATCH: u32 = 2;
const PLAYER_BATCH: u32 = 3;
const HANGAR_BATCH: u32 = 4;
/// The containers inside the hangar, once the hangar has spawned, as their parent.
const CONTENT_BATCH: u32 = 5;
/// What the player wears, one batch per depth from this id on: the body, then what hangs from it, and so on. An item's
/// parent must be bound and enabled when the item finalizes (`CAttachableComponent::DoAttachDuringSpawn`, see Body and
/// loadout), which it is only once the parent's own batch has ended.
const ITEMS_BATCH: u32 = 6;

/// `<account id>` and an optional `<home location>` of a session named `hub-<account id>[-<home location>]`.
fn session_fields(session: &[u8]) -> Option<(&str, Option<&str>)> {
    let fields = std::str::from_utf8(session).ok()?.strip_prefix("hub-")?;
    Some(fields.split_once('-').map_or((fields, None), |(account, home)| (account, Some(home))))
}

/// The Hub names a session `hub-<account id>`.
fn session_account(session: &[u8]) -> Option<u64> {
    session_fields(session)?.0.parse().ok()
}

/// The Hub does not send this yet, so every session gets the DGS's default.
fn session_home_location(session: &[u8]) -> Option<u32> {
    session_fields(session)?.1?.parse().ok()
}

/// How far the shard has taken a client. Each step is one reliable packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Greeted,
    Unstowing,
    BatchSent,
    BatchEnded,
    SolarSystemsSent,
    SolarSystemsEnded,
    Streamables,
    AsyncSpawns,
    HangarSent,
    HangarEnded,
    ContentSent,
    ContentEnded,
    PlayerSent,
    PlayerEnded,
    /// The player's items at this depth, 0 being the body.
    ItemsSent(u32),
    ItemsEnded(u32),
    InGame,
}

struct Peer {
    pubkeys: [[u8; PUBKEY_LEN]; 3],
    ack: Vec<u8>,
    channel: Channel,
    receiver: Receiver,
    packets: u64,
    data_packets: u64,
    /// Sealed once, as a resend is the same packet with the resend flag set.
    reliable: Option<Vec<u8>>,
    /// The messages of later reliable packets, which go one at a time as each is acknowledged.
    pending: VecDeque<Vec<u8>>,
    stage: Stage,
    /// The batches the client has reported spawned.
    batches_completed: Vec<u32>,
    /// Who this is, from the Hub session id.
    identity: Identity,
    /// What ties this connection to its DGS. Dropped with the peer, which frees what the DGS holds for it.
    session: Option<Session>,
    session_id: u64,
    /// What every client gets, once the DGSs have sent it.
    world: Option<World>,
    player: PlayerEntities,
    /// The last the governing DGS said of itself. `None` when it stopped answering.
    server: Option<DgsStatusResponse>,
    /// Which run of the DGS holds this player.
    boot_id: u64,
    /// The connections of this shard, as last counted.
    shard_population: u32,
    sequence: u32,
    reliable_acked: bool,
    reliable_sent: Instant,
    /// The highest state the client has reported.
    client_state: u8,
    last_seen: Instant,
    /// The player's input frames not yet given to its DGS.
    input: Vec<InputFrame>,
    input_in_flight: bool,
    /// The last frame that held nothing, which starts the next frame that does: the DGS moves a player over the time
    /// between frames.
    input_idle: Option<InputFrame>,
    /// Whether the last frame given to the DGS held anything. Frames that hold nothing go once, to stop the player.
    input_active: bool,
    /// The DGS's last word on where the player is.
    movement: Option<PlayerMovement>,
    /// What the client has been sent of it, starting from the bind.
    movement_sent: Option<PlayerMovement>,
    /// When the client was last sent the frame its state is of.
    frame_time_sent: Instant,
    /// What the movement path did since it was last logged.
    movement_counts: MovementCounts,
}

/// Counts of a player's movement path, logged every `PRUNE_PERIOD` while anything happens: they tell where a player that
/// does not move is stopped.
#[derive(Debug, Default, PartialEq)]
struct MovementCounts {
    /// Action frames of the player read.
    frames: u32,
    /// Of those, frames with a button held.
    moving: u32,
    /// Frames read past an action of unknown length.
    partial: u32,
    calls: u32,
    answers: u32,
    /// Calls the DGS did not answer with a state.
    failed: u32,
    updates: u32,
}

/// The movement state the player's bind gives the client, as `player::movement_snapshots` writes it: the aim rotation is
/// the variable's default.
fn bound_movement(player: &EntityBind) -> PlayerMovement {
    let transform = player.transform.unwrap_or_default();
    let rotation = |x: f64, y: f64, z: f64, w: f64| Rotation { x: x as f32, y: y as f32, z: z as f32, w: w as f32 };
    PlayerMovement {
        entity_id: player.entity_id,
        x: transform.x,
        y: transform.y,
        z: transform.z,
        base_rotation: Some(rotation(transform.qx, transform.qy, transform.qz, transform.qw)),
        aim_rotation: Some(rotation(0.0, 0.0, 0.0, 1.0)),
        zone_host_id: player.parent_id,
        parent_id: player.parent_id,
        revision: 0,
        frame_time: 0,
    }
}

/// How far below the player each item hangs: 0 for one whose parent is the player, `None` for one whose parent is neither
/// the player nor an earlier item. The DGS lists parents first.
fn item_depths(player: u64, items: &[EntityBind]) -> Vec<Option<u32>> {
    let mut depths: Vec<Option<u32>> = Vec::with_capacity(items.len());
    for item in items {
        let depth = if item.parent_id == player {
            Some(0)
        } else {
            let parent = items.iter().position(|other| other.entity_id == item.parent_id).filter(|at| *at < depths.len());
            parent.and_then(|at| depths[at]).map(|depth| depth + 1)
        };
        depths.push(depth);
    }
    depths
}

fn holds_input(frame: &InputFrame) -> bool {
    frame.move_buttons != 0 || frame.sprint || frame.aim_rotation.is_some() || frame.client_position.is_some() || frame.partial || frame.jump != 0 || frame.stance.is_some()
}

impl Peer {
    /// Greets a client once it has started talking on the reliable channel.
    fn greet(&mut self, shard_id: &str, shard_name: &str) -> Option<Vec<u8>> {
        if self.data_packets == 0 || self.reliable.is_some() {
            return None;
        }
        let header = [HEADER_MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED, 0, 0, 0, 0, 0];
        self.reliable = self.channel.seal(header, &greeting(shard_id, shard_name, VIEW_STATE));
        self.reliable_sent = Instant::now();
        self.reliable.clone()
    }

    /// Queues what the client has become ready for.
    fn plan(&mut self) {
        let next = match self.stage {
            Stage::Greeted if self.reliable_acked && self.client_state >= VIEW_STATE => {
                Some((Stage::Unstowing, state_change(UNSTOW_STATE)))
            }
            Stage::Unstowing if self.client_state >= UNIVERSE_ROOT_STATE => self.world.as_ref().map(|world| (Stage::BatchSent, bind_batch(FIRST_BATCH, &world.universe))),
            Stage::BatchSent if self.batches_completed.contains(&FIRST_BATCH) => Some((Stage::BatchEnded, bind_batch_end(FIRST_BATCH))),
            Stage::BatchEnded if self.client_state >= SOLAR_SYSTEM_STATE => self.solar_system_batch().map(|batch| (Stage::SolarSystemsSent, batch)),
            Stage::SolarSystemsSent if self.batches_completed.contains(&SOLAR_SYSTEM_BATCH) => {
                Some((Stage::SolarSystemsEnded, bind_batch_end(SOLAR_SYSTEM_BATCH)))
            }
            Stage::SolarSystemsEnded if self.client_state >= UNSTOW_STATE => {
                Some((Stage::Streamables, state_change(STATE_REPLICATE_STREAMABLES)))
            }
            Stage::Streamables if self.client_state >= STATE_REPLICATE_STREAMABLES => {
                Some((Stage::AsyncSpawns, state_change(STATE_WAIT_FOR_ASYNC_SPAWNS)))
            }
            Stage::AsyncSpawns if self.client_state >= STATE_WAIT_FOR_ASYNC_SPAWNS => self.hangar_batch().or_else(|| self.player_batch()),
            Stage::HangarSent if self.batches_completed.contains(&HANGAR_BATCH) => Some((Stage::HangarEnded, bind_batch_end(HANGAR_BATCH))),
            Stage::HangarEnded => self.content_batch().or_else(|| self.player_batch()),
            Stage::ContentSent if self.batches_completed.contains(&CONTENT_BATCH) => {
                Some((Stage::ContentEnded, bind_batch_end(CONTENT_BATCH)))
            }
            Stage::ContentEnded => self.player_batch(),
            Stage::PlayerSent if self.batches_completed.contains(&PLAYER_BATCH) => Some((Stage::PlayerEnded, bind_batch_end(PLAYER_BATCH))),
            Stage::PlayerEnded => Some(self.item_batch(0).unwrap_or_else(|| self.in_game())),
            Stage::ItemsSent(depth) if self.batches_completed.contains(&(ITEMS_BATCH + depth)) => {
                Some((Stage::ItemsEnded(depth), bind_batch_end(ITEMS_BATCH + depth)))
            }
            Stage::ItemsEnded(depth) => Some(self.item_batch(depth + 1).unwrap_or_else(|| self.in_game())),
            _ => None,
        };
        if let Some((stage, messages)) = next {
            info!(?stage, client_state = self.client_state, "shard stage reached");
            self.stage = stage;
            self.pending.push_back(messages);
        }
    }

    /// A real view binds unstreamables in state 12, before it lets the client on to the player, so a planet bound with
    /// the hangar was the only one to spawn beside a player. The client does not stop at 12 for this shard, so the planet
    /// goes with its parent, and the batch's end holds back everything after it until the planet has spawned. `None` until
    /// the DGSs have said what goes in it.
    fn solar_system_batch(&self) -> Option<Vec<u8>> {
        let mut binds = self.world.as_ref()?.solar_systems.clone();
        match &self.player {
            PlayerEntities::Waiting => return None,
            PlayerEntities::NoCharacter => {}
            PlayerEntities::Joined(joined) => {
                if let Some(planet) = &joined.planet {
                    info!(id = planet.entity_id, "binding home planet");
                    binds.push(planet.clone());
                }
            }
        }
        Some(bind_batch(SOLAR_SYSTEM_BATCH, &binds))
    }

    fn joined(&self) -> Option<&crate::link::Joined> {
        match &self.player {
            PlayerEntities::Joined(joined) => Some(joined),
            _ => None,
        }
    }

    fn hangar_batch(&self) -> Option<(Stage, Vec<u8>)> {
        let hangar = self.joined()?.hangar.as_ref()?;
        info!(id = hangar.entity_id, class = hangar.class_crc, "binding home hangar");
        Some((Stage::HangarSent, bind_batch(HANGAR_BATCH, std::slice::from_ref(hangar))))
    }

    fn content_batch(&self) -> Option<(Stage, Vec<u8>)> {
        let content = &self.joined()?.hangar_content;
        if content.is_empty() {
            return None;
        }
        info!(count = content.len(), first = content[0].entity_id, "binding the hangar's containers");
        Some((Stage::ContentSent, bind_batch(CONTENT_BATCH, content)))
    }

    /// The player alone. What it wears follows in `item_batch`, once this batch has ended.
    fn player_batch(&self) -> Option<(Stage, Vec<u8>)> {
        let player = &self.joined()?.player;
        Some((Stage::PlayerSent, bind_batch(PLAYER_BATCH, std::slice::from_ref(player))))
    }

    /// The items `depth` steps below the player: 0 is what hangs from the player itself. `None` past the deepest.
    fn item_batch(&self, depth: u32) -> Option<(Stage, Vec<u8>)> {
        let joined = self.joined()?;
        let binds: Vec<EntityBind> = item_depths(joined.player.entity_id, &joined.player_items)
            .into_iter()
            .zip(&joined.player_items)
            .filter(|(at, _)| *at == Some(depth))
            .map(|(_, item)| item.clone())
            .collect();
        if binds.is_empty() {
            return None;
        }
        info!(depth, count = binds.len(), first = binds[0].entity_id, "binding the player's items");
        Some((Stage::ItemsSent(depth), bind_batch(ITEMS_BATCH + depth, &binds)))
    }

    /// Not held for the client's 17: its model only reports a state once this view has reached it.
    fn in_game(&self) -> (Stage, Vec<u8>) {
        let debug = self.server_name().map(|name| {
            let (frames, server_population) = self.server_numbers();
            debug_server_info(name, &frames, self.shard_population, server_population)
        });
        let bubble = self.joined().map(|joined| update_bubble_create(joined.player.entity_id));
        // The client moves its actor itself, as with a real server, and the DGS follows what it reports.
        let authority = self.joined().filter(|joined| indexes_hold(&joined.player)).map(|joined| client_authority(joined.player.entity_id));
        let messages = [state_change(STATE_IN_GAME), debug.unwrap_or_default(), bubble.unwrap_or_default(), authority.unwrap_or_default()];
        (Stage::InGame, messages.concat())
    }

    /// The name of the governing DGS, for the overlay.
    fn server_name(&self) -> Option<&str> {
        Some(&self.joined()?.server_name)
    }

    /// What the overlay shows of the DGS. Nothing is known of one that stopped answering.
    fn server_numbers(&self) -> (DgsFrames, u32) {
        let frames = self.server.as_ref().and_then(|status| status.frames);
        (frames.unwrap_or_default(), self.server.as_ref().map_or(0, |status| status.players))
    }

    fn dgs(&self) -> Option<&str> {
        Some(&self.joined()?.dgs)
    }

    /// Starts asking for this connection's entities. `Peer::new` cannot, as tests of the packet path have no runtime.
    fn open(&mut self, link: &Arc<Link>, from: SocketAddr, session_id: u64) {
        self.session_id = session_id;
        self.session = Some(link.open(from, session_id, self.identity));
    }

    fn receive_player(&mut self, player: PlayerEntities) {
        if let PlayerEntities::Joined(joined) = &player {
            self.boot_id = joined.status.boot_id;
            self.server = Some(joined.status.clone());
            // A join, and a rejoin after the DGS restarted, puts the player back at its spawn point.
            self.movement = None;
            if self.movement_sent.is_none() {
                self.movement_sent = Some(bound_movement(&joined.player));
            }
        }
        self.player = player;
    }

    /// What the governing DGS said on the last poll. A real game server sends these numbers on a timer. A DGS that
    /// restarted has lost its record of this player, so it is told again.
    fn receive_status(&mut self, status: Option<DgsStatusResponse>, shard_population: u32) {
        if let Some(status) = status.as_ref().filter(|status| status.boot_id != self.boot_id) {
            self.boot_id = status.boot_id;
            info!(dgs = ?self.dgs(), "the DGS restarted, joining this player again");
            if let Some(session) = &self.session {
                session.rejoin();
            }
        }
        self.server = status;
        self.shard_population = shard_population;
        if self.stage == Stage::InGame && self.pending.is_empty() {
            let (frames, server_population) = self.server_numbers();
            self.pending.push_back(debug_server_stats(&frames, shard_population, server_population));
        }
    }

    /// Keeps a player's action frames for its DGS. Only once in game, and only the player's own.
    fn receive_input(&mut self, payload: &[u8]) -> bool {
        let Some(decoded) = action_frame(payload) else { return false };
        let player = self.joined().map(|joined| joined.player.entity_id);
        if self.stage != Stage::InGame || player != Some(decoded.entity) {
            return true;
        }
        if let Some(action) = decoded.unknown {
            debug!(action, buttons = decoded.frame.move_buttons, "input frame read past an action of unknown length");
            self.movement_counts.partial += 1;
        }
        self.movement_counts.frames += 1;
        self.movement_counts.moving += u32::from(decoded.frame.move_buttons != 0);
        self.input.push(decoded.frame);
        true
    }

    /// The frames the DGS should have now, if any: one call at a time, and after the frame that stopped the player nothing
    /// until it moves again.
    fn input_to_forward(&mut self) -> Option<Vec<InputFrame>> {
        if self.input_in_flight || self.input.is_empty() {
            return None;
        }
        let active = self.input.iter().any(holds_input);
        if !active && !self.input_active {
            self.input_idle = self.input.pop();
            self.input.clear();
            return None;
        }
        let mut frames = std::mem::take(&mut self.input);
        if let Some(idle) = self.input_idle.take() {
            frames.insert(0, idle);
        }
        self.input_active = active;
        self.input_in_flight = true;
        Some(frames)
    }

    fn forward_input(&mut self, link: &Arc<Link>, from: SocketAddr) {
        let Some(joined) = self.joined() else { return };
        if !indexes_hold(&joined.player) {
            self.input.clear();
            return;
        }
        let (dgs, player) = (joined.dgs.clone(), joined.player.entity_id);
        if let Some(frames) = self.input_to_forward() {
            self.movement_counts.calls += 1;
            link.input(from, self.session_id, dgs, player, frames);
        }
    }

    fn receive_movement(&mut self, movement: Option<PlayerMovement>) {
        self.input_in_flight = false;
        self.movement_counts.answers += 1;
        self.movement_counts.failed += u32::from(movement.is_none());
        if movement.is_some() {
            self.movement = movement;
        }
    }

    /// The movement the client has not been sent yet, as one update.
    fn movement_message(&mut self) -> Option<Vec<u8>> {
        let movement = self.movement.as_ref()?;
        let refresh = self.frame_time_sent.elapsed() >= FRAME_TIME_REFRESH;
        let message = movement_update(movement, self.movement_sent.as_ref(), refresh)?;
        if self.movement_sent.is_none_or(|sent| sent.frame_time != movement.frame_time) {
            self.frame_time_sent = Instant::now();
        }
        if self.movement_counts.updates == 0 {
            info!(x = movement.x, y = movement.y, z = movement.z, revision = movement.revision, "player movement update sent");
        }
        self.movement_counts.updates += 1;
        self.movement_sent = Some(*movement);
        Some(message)
    }

    /// One reliable packet is in flight at a time, so a later one waits for the ack of the one before. Movement goes when
    /// nothing else waits, so the client only ever gets the latest.
    fn advance(&mut self) -> Option<Vec<u8>> {
        self.plan();
        if self.reliable.is_none() || !self.reliable_acked {
            return None;
        }
        let mut messages = match self.pending.pop_front() {
            Some(messages) => messages,
            None if self.stage == Stage::InGame => self.movement_message()?,
            None => return None,
        };
        // A run too long for one packet goes as several, back to back: what is queued next, the overlay's numbers or
        // movement, waits for the queue to empty, so nothing comes between the fragments of a message.
        if messages.len() > MAX_PACKET_MESSAGES {
            let mut packets = packet_runs(&messages, MAX_PACKET_MESSAGES).into_iter();
            messages = packets.next().unwrap_or_default();
            for packet in packets.rev() {
                self.pending.push_front(packet);
            }
        }
        self.sequence += 1;
        let header = [HEADER_MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED, 0, 0, 0, 0, 0];
        self.reliable = self.channel.seal(header, &later_packet(self.sequence, &messages));
        self.reliable_acked = false;
        self.reliable_sent = Instant::now();
        self.reliable.clone()
    }

    /// Pings arrive every 100 ms, so checking on each packet needs no timer of its own.
    fn reliable_resend(&mut self) -> Option<Vec<u8>> {
        if self.reliable_acked || self.reliable_sent.elapsed() < GREETING_RESEND_PERIOD {
            return None;
        }
        let mut packet = self.reliable.clone()?;
        packet[2] |= FLAG_RESEND;
        self.reliable_sent = Instant::now();
        Some(packet)
    }

    fn new(request: &ConnectRequest, ack: Vec<u8>, keys: &[[u8; 16]; 3]) -> Self {
        Self {
            pubkeys: request.pubkeys,
            ack,
            channel: Channel::new(keys),
            receiver: Receiver::new(),
            packets: 0,
            data_packets: 0,
            reliable: None,
            pending: VecDeque::new(),
            stage: Stage::Greeted,
            batches_completed: Vec::new(),
            identity: Identity { account: session_account(&request.session_guid), location: session_home_location(&request.session_guid) },
            session: None,
            session_id: 0,
            world: None,
            player: PlayerEntities::Waiting,
            server: None,
            boot_id: 0,
            shard_population: 0,
            sequence: 0,
            reliable_acked: false,
            reliable_sent: Instant::now(),
            client_state: 0,
            last_seen: Instant::now(),
            input: Vec::new(),
            input_in_flight: false,
            input_idle: None,
            input_active: false,
            movement: None,
            movement_sent: None,
            frame_time_sent: Instant::now(),
            movement_counts: MovementCounts::default(),
        }
    }
}

/// Logs what each player's movement path did since the last time, if anything.
fn log_movement(peers: &mut HashMap<SocketAddr, Peer>) {
    for (from, peer) in peers.iter_mut() {
        let counts = std::mem::take(&mut peer.movement_counts);
        if counts != MovementCounts::default() {
            let at = peer.movement_sent.map(|sent| (sent.x, sent.y, sent.z));
            info!(%from, ?counts, ?at, acked = peer.reliable_acked, "player movement");
        }
    }
}

/// Dropping a peer ends its session, and with it what its DGS holds.
fn prune(peers: &mut HashMap<SocketAddr, Peer>) {
    peers.retain(|from, peer| {
        let alive = peer.last_seen.elapsed() < PEER_TIMEOUT;
        if !alive {
            info!(%from, "player timed out");
        }
        alive
    });
}

/// Hands a DGS's answer to the connection it is for. One that has gone, or been replaced at its address, gets nothing.
/// Returns a connection that has something to send now.
fn receive_event(event: Event, peers: &mut HashMap<SocketAddr, Peer>) -> Option<SocketAddr> {
    let population = peers.len() as u32;
    match event {
        Event::World { from, session, world } => {
            if let Some(peer) = peers.get_mut(&from).filter(|peer| peer.session_id == session) {
                peer.world = Some(world);
            }
        }
        Event::Player { from, session, player } => {
            if let Some(peer) = peers.get_mut(&from).filter(|peer| peer.session_id == session) {
                peer.receive_player(player);
            }
        }
        Event::Status { dgs, status } => {
            for peer in peers.values_mut().filter(|peer| peer.dgs() == Some(&dgs)) {
                peer.receive_status(status.clone(), population);
            }
        }
        Event::Movement { from, session, movement } => {
            if let Some(peer) = peers.get_mut(&from).filter(|peer| peer.session_id == session) {
                peer.receive_movement(movement);
                return Some(from);
            }
        }
    }
    None
}

/// Asks each DGS that holds a player here how it is doing.
fn poll_servers(link: &Arc<Link>, peers: &HashMap<SocketAddr, Peer>) {
    let servers: HashSet<&str> = peers.values().filter_map(Peer::dgs).collect();
    for dgs in servers {
        link.poll(dgs.to_string());
    }
}

/// Reports the player count to sc-shard-operator.
async fn serve_status(address: SocketAddr, players: Arc<AtomicU32>) {
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            warn!(%address, %error, "status endpoint not available");
            return;
        }
    };
    while let Ok((mut socket, _)) = listener.accept().await {
        let players = players.clone();
        tokio::spawn(async move {
            let mut request = [0u8; 512];
            let _ = socket.read(&mut request).await;
            let body = format!("{{\"players\":{}}}", players.load(Ordering::Relaxed));
            let reply = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = socket.write_all(reply.as_bytes()).await;
        });
    }
}

/// 100 ns ticks since 1970, the form `ServerTime` prints as a date.
fn server_clock() -> u64 {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (elapsed.as_nanos() / 100) as u64
}

/// The client drops a channel after 30 seconds without a packet. A pong carries `pingTime` and this server's clock,
/// which feeds `CTimer::UpdateServerTimeSync` (0x147263950).
fn pong(ping: &[u8], clock: u64) -> Option<Vec<u8>> {
    if ping.len() != HEADER_LEN + PING_BODY_LEN + 4 {
        return None;
    }
    let (rest, footer) = ping.split_at(ping.len() - 4);
    let (header, time) = rest.split_at(HEADER_LEN);
    if packet_crc(header, time).to_le_bytes() != footer {
        return None;
    }
    let packet_header = [HEADER_MAGIC, PONG, FLAG_PLAINTEXT, 0, 0, 0, 0, 0];
    let body = [time, &clock.to_le_bytes()].concat();
    let crc = packet_crc(&packet_header, &body);
    Some([&packet_header[..], &body, &crc.to_le_bytes()].concat())
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Every packet gets its own ack, resends included: the earlier ack may be what was lost.
fn replication_data(data: &[u8], from: SocketAddr, peer: &mut Peer) -> Option<Vec<u8>> {
    let Some(body) = peer.channel.open(data) else {
        peer.packets += 1;
        if peer.packets <= MAX_LOGGED_PACKETS {
            warn!(%from, flags = data[2], len = data.len(), "replication data failed to decrypt");
        }
        return None;
    };
    let header = DataHeader::parse(&body);
    peer.data_packets += 1;
    peer.client_state = peer.client_state.max(reported_state(&body).unwrap_or(0));
    peer.batches_completed.extend(completed_batch(&body));
    if peer.data_packets <= MAX_LOGGED_DATA {
        info!(%from, flags = data[2], ?header, body = %hex(&body), "replication data");
    }
    for (id, payload) in messages(&body) {
        // Action frames come 30 times a second.
        if id == MESSAGE_ENTITY_REMOTE_METHOD && peer.receive_input(payload) {
            debug!(%from, id, payload = %hex(payload), "client message");
        } else {
            info!(%from, id, payload = %hex(payload), "client message");
        }
    }
    let resent = data[2] & FLAG_RESEND != 0;
    let ack = peer.receiver.acknowledge(&header?, resent);
    peer.channel.seal([HEADER_MAGIC, REPLICATION_ACK, FLAG_ENCRYPTED, 0, 0, 0, 0, 0], &ack)
}

fn handle_packet(data: &[u8], from: SocketAddr, peers: &mut HashMap<SocketAddr, Peer>) -> Option<Vec<u8>> {
    if data.len() < HEADER_LEN || data[0] != HEADER_MAGIC {
        debug!(%from, len = data.len(), "non-nub datagram");
        return None;
    }
    let (kind, flags) = (data[1], data[2]);
    if kind == CONNECT_REQUEST && flags & FLAG_PLAINTEXT != 0 {
        if data.len() < HEADER_LEN + 4 {
            return None;
        }
        let (rest, footer) = data.split_at(data.len() - 4);
        let (header, body) = rest.split_at(HEADER_LEN);
        if packet_crc(header, body).to_le_bytes() != footer {
            warn!(%from, "connect request failed CRC");
            return None;
        }
        let Some(request) = parse_connect_request(body) else {
            warn!(%from, len = body.len(), "unparsable connect request");
            return None;
        };
        // The client resends until an ack arrives and keeps the first, so new keys here would not
        // be the ones it uses.
        if let Some(peer) = peers.get_mut(&from).filter(|peer| peer.pubkeys == request.pubkeys) {
            peer.last_seen = Instant::now();
            return Some(peer.ack.clone());
        }
        let (ack, keys) = accept_connect(&request)?;
        info!(%from, guid_bytes = request.session_guid.len(), "shard handshake complete");
        peers.insert(from, Peer::new(&request, ack.clone(), &keys));
        return Some(ack);
    }
    match peers.get_mut(&from) {
        Some(peer) => {
            peer.last_seen = Instant::now();
            if kind == PING && flags & FLAG_PLAINTEXT != 0 {
                return pong(data, server_clock());
            }
            if kind == KEEP_ALIVE {
                return None;
            }
            if kind == REPLICATION_DATA && flags & FLAG_ENCRYPTED != 0 {
                return replication_data(data, from, peer);
            }
            if kind == REPLICATION_ACK && flags & FLAG_ENCRYPTED != 0 {
                match peer.channel.open(data) {
                    Some(body) => {
                        // In game, movement makes these up to 30 a second.
                        if peer.stage == Stage::InGame {
                            debug!(%from, body = %hex(&body), "replication ack");
                        } else {
                            info!(%from, body = %hex(&body), "replication ack");
                        }
                        peer.reliable_acked |= acknowledges(&body, peer.sequence);
                    }
                    None => warn!(%from, "replication ack failed to decrypt"),
                }
                return None;
            }
            peer.packets += 1;
            if peer.packets <= MAX_LOGGED_PACKETS {
                warn!(%from, kind, encrypted = flags & FLAG_ENCRYPTED != 0, len = data.len(), "unhandled shard packet");
            }
        }
        None => debug!(%from, kind, "packet from unknown peer"),
    }
    None
}

pub async fn serve(
    addr: SocketAddr,
    status: Option<SocketAddr>,
    shard_id: String,
    shard_name: String,
    dgs_areas: String,
    dgs_address: String,
) -> Result<(), String> {
    let directory = Directory::parse(&shard_id, &dgs_areas, &dgs_address)?;
    info!(dgs = directory.dgs_count(), address = %dgs_address, "DGS areas read");
    let (events, mut answers) = unbounded_channel();
    let link = Arc::new(Link::new(directory, events));
    let socket = Arc::new(UdpSocket::bind(addr).await.map_err(|e| format!("shard bind {addr}: {e}"))?);
    info!(%addr, shard = %shard_id, "shard listening (UDP)");
    let players = Arc::new(AtomicU32::new(0));
    if let Some(status) = status {
        tokio::spawn(serve_status(status, players.clone()));
    }
    let mut peers = HashMap::new();
    let mut last_session = 0;
    let mut buffer = vec![0u8; 65536];
    let mut tick = tokio::time::interval(PRUNE_PERIOD);
    loop {
        let received = tokio::select! {
            received = socket.recv_from(&mut buffer) => received,
            Some(event) = answers.recv() => {
                let ready = receive_event(event, &mut peers);
                if let Some((from, packet)) = ready.and_then(|from| Some((from, peers.get_mut(&from)?.advance()?))) {
                    if let Err(error) = socket.send_to(&packet, from).await {
                        warn!(%from, %error, "shard send failed");
                    }
                }
                continue;
            }
            _ = tick.tick() => {
                prune(&mut peers);
                log_movement(&mut peers);
                players.store(peers.len() as u32, Ordering::Relaxed);
                poll_servers(&link, &peers);
                continue;
            }
        };
        let (length, from) = match received {
            Ok(received) => received,
            Err(error) => {
                warn!(%error, "shard receive failed");
                continue;
            }
        };
        if let Some(reply) = handle_packet(&buffer[..length], from, &mut peers) {
            if let Err(error) = socket.send_to(&reply, from).await {
                warn!(%from, %error, "shard send failed");
            }
        }
        let population = peers.len() as u32;
        if let Some(peer) = peers.get_mut(&from) {
            if peer.session.is_none() {
                last_session += 1;
                peer.open(&link, from, last_session);
            }
            peer.shard_population = population;
            peer.forward_input(&link, from);
            if let Some(packet) = peer.greet(&shard_id, &shard_name).or_else(|| peer.advance()).or_else(|| peer.reliable_resend()) {
                if peer.stage == Stage::InGame {
                    debug!(%from, resend = packet[2] & FLAG_RESEND != 0, "shard reliable packet sent");
                } else {
                    info!(%from, resend = packet[2] & FLAG_RESEND != 0, "shard reliable packet sent");
                }
                if let Err(error) = socket.send_to(&packet, from).await {
                    warn!(%from, %error, "shard send failed");
                }
            }
        }
        players.store(peers.len() as u32, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;

#[cfg(test)]
mod tests {
    use super::wire_tests::{joined, world};
    use super::*;

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    /// A connect request captured from StarCitizen.exe 4.10.2, so these tests answer to the real client.
    const REAL_REQUEST: &str = "5c0004dcc5e73e61e51e3add0b6875622d31303030303031048edeb33049f517bb8cfe240f27eb7e5893cfd5500c20966423433d096d43f2d50445cfc80a3ab028ef5502ae75f0fb5b4c511f427a739454e8178e1349b4a761d504be893e3ea82b4f0951ea107f779a206519fe9f8c7706f21bac7bc3defc2f26b01bf9b5fa";

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    fn real_request() -> Vec<u8> {
        hex_bytes(REAL_REQUEST)
    }

    #[test]
    fn a_real_connect_request_is_accepted() {
        let packet = real_request();
        let mut peers = HashMap::new();
        let from = "127.0.0.1:1".parse().unwrap();
        let ack = handle_packet(&packet, from, &mut peers).expect("the shard answers");
        assert_eq!(peers.len(), 1);
        assert_eq!(ack[1], CONNECT_ACK);
        assert_eq!(ack.len(), HEADER_LEN + PUBKEY_LEN * 3 + 1 + 4);
    }

    #[test]
    fn the_ack_says_a_replicant_sent_it() {
        let request = parse_connect_request(&real_request()[HEADER_LEN..real_request().len() - 4]).unwrap();
        let (ack, _) = accept_connect(&request).unwrap();
        assert_eq!(ack[ack.len() - 5], 1);
    }

    #[test]
    fn stages_run_from_game_rules_to_the_player_in_step_with_the_client() {
        let mut peer = connected_peer();
        peer.stage = Stage::BatchEnded;
        let mut walk = |state: u8, batch: u32| {
            peer.client_state = state;
            peer.batches_completed.push(batch);
            peer.plan();
            peer.stage
        };
        assert_eq!(walk(10, 1), Stage::BatchEnded);
        assert_eq!(walk(11, 1), Stage::SolarSystemsSent);
        assert_eq!(walk(11, 1), Stage::SolarSystemsSent);
        assert_eq!(walk(11, 2), Stage::SolarSystemsEnded);
        assert_eq!(walk(13, 2), Stage::SolarSystemsEnded);
        assert_eq!(walk(14, 2), Stage::Streamables);
        assert_eq!(walk(15, 2), Stage::AsyncSpawns);
        assert_eq!(walk(16, 2), Stage::HangarSent);
        assert_eq!(walk(16, 2), Stage::HangarSent);
        assert_eq!(walk(16, 4), Stage::HangarEnded);
        assert_eq!(walk(16, 4), Stage::ContentSent);
        assert_eq!(walk(16, 4), Stage::ContentSent);
        assert_eq!(walk(16, 5), Stage::ContentEnded);
        assert_eq!(walk(16, 5), Stage::PlayerSent);
        assert_eq!(walk(16, 3), Stage::PlayerEnded);
        assert_eq!(walk(16, 3), Stage::ItemsSent(0));
        assert_eq!(walk(16, 3), Stage::ItemsSent(0));
        assert_eq!(walk(16, 6), Stage::ItemsEnded(0));
        assert_eq!(walk(16, 6), Stage::ItemsSent(1));
        for depth in 1..4 {
            assert_eq!(walk(16, 6 + depth), Stage::ItemsEnded(depth));
            assert_eq!(walk(16, 6 + depth), Stage::ItemsSent(depth + 1));
        }
        assert_eq!(walk(16, 10), Stage::ItemsEnded(4));
        assert_eq!(walk(16, 10), Stage::InGame);
    }

    #[test]
    fn the_planet_follows_the_solar_systems_in_their_batch() {
        let mut peer = connected_peer();
        peer.stage = Stage::BatchEnded;
        peer.client_state = 11;
        peer.plan();
        let systems = bind_batch(2, &world().solar_systems);
        let planet = hex_bytes(concat!(
            "1ba700",
            "d42a0f912e000000", "02000000", "00000000", "00", "569d5c98", "00", "8068",
            "b21df470effe0dc2", "4e26250f6c44fc41", "0000000000000000",
            "0000000000000000", "0000000000000000", "a006affd8aaae5bf", "8eb04b9fa88ce73f", "000000000000f03f",
            "00", "0000000000000000", "ae4501912e000000", "b21ec046", "00",
            "0000000000000000", "0000000000000000", "00",
            "03", "04", "e6bca1ff", "ea4b4e5d", "809ef562", "04", "8cdb8b60", "ea4b4e5d", "59cdf5e1", "04", "429f87a8", "ea4b4e5d", "df9e66e5",
        ));
        let expected = [&systems[..systems.len() - 7], &planet, &systems[systems.len() - 7..]].concat();
        assert_eq!(systems[systems.len() - 7..], hex_bytes("1c040002000000"));
        assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::SolarSystemsSent, Some(expected)));
    }

    #[test]
    fn the_hangar_batch_binds_the_hangar_under_the_planet() {
        let mut peer = connected_peer();
        peer.stage = Stage::AsyncSpawns;
        peer.client_state = 16;
        peer.plan();
        let expected = hex_bytes(concat!(
            "1a040004000000", "1bc400",
            "0098063933000000", "04000000", "00000000", "00", "342b4b3c", "01", "0a112b70", "00", "8028",
            "00000040262f1741", "000000a008ef25c1", "00000020dd970b41",
            "3f58398338a2abbf", "1b2703a20fb7c53f", "0a6781e6c3b83b3f", "e9d8d8e1227def3f", "000000000000f03f",
            "00", "0000000000000000", "d42a0f912e000000", "b21ec046", "00",
            "0000000000000000", "0000000000000000", "00",
            "03", "04", "e6bca1ff", "ea4b4e5d", "86fb9247",
            "10", "ad1459e3", "aa0f3efb", "0198063933000000", "2c98063933000000",
            "10", "d5d2f409", "aa0f3efb", "0000000000000000", "0000000000000000",
            "1c040004000000",
        ));
        assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::HangarSent, Some(expected)));
    }

    #[test]
    fn with_the_planet_zone_off_a_join_binds_what_joined_on_the_real_game() {
        let mut peer = connected_peer();
        peer.player = joined(1_000_001, None, false);
        peer.stage = Stage::BatchEnded;
        peer.client_state = 11;
        peer.plan();
        assert_eq!(peer.pending.pop_back(), Some(bind_batch(2, &world().solar_systems)));
        peer.stage = Stage::AsyncSpawns;
        peer.client_state = 16;
        peer.plan();
        let expected = hex_bytes(concat!(
            "1a040004000000", "1bc400",
            "0098063933000000", "04000000", "00000000", "00", "342b4b3c", "01", "0a112b70", "00", "8028",
            "0000000000000000", "0000000000000000", "0000000000000000",
            "0000000000000000", "0000000000000000", "0000000000000000", "000000000000f03f", "000000000000f03f",
            "00", "0000000000000000", "ae4501912e000000", "b21ec046", "00",
            "0000000000000000", "0000000000000000", "00",
            "03", "04", "e6bca1ff", "ea4b4e5d", "86fb9247",
            "10", "ad1459e3", "aa0f3efb", "0198063933000000", "2c98063933000000",
            "10", "d5d2f409", "aa0f3efb", "0000000000000000", "0000000000000000",
            "1c040004000000",
        ));
        assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::HangarSent, Some(expected)));
    }

    #[test]
    fn the_last_state_brings_the_overlay_its_server_and_its_real_numbers() {
        let mut peer = connected_peer();
        peer.stage = Stage::ItemsEnded(4);
        peer.shard_population = 5;
        peer.server = Some(status(1, 3));
        let PlayerEntities::Joined(joined) = &mut peer.player else { panic!("not joined") };
        joined.server_name = "pub-usa-sc-4102-12913470-dgs-pyro-2".to_string();
        peer.plan();
        let name = b"pub-usa-sc-4102-12913470-dgs-pyro-2";
        let numbers = [2, 10, 0, 0x3f, 0xc0, 0x00, 0x95, 0xac, 0x14, 0x01, 0x02, 0x05, 0x03];
        let bubble = update_bubble_create(peer.joined().unwrap().player.entity_id);
        // Last, the player's own movement: `m_clientAuthoritative` 0x10.
        let authority = client_authority(peer.joined().unwrap().player.entity_id);
        let expected = [&[6, 1, 0, 17, 3, 36, 0, 35][..], name, &numbers, &bubble, &authority].concat();
        assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::InGame, Some(expected)));
    }

    fn status(boot_id: u64, players: u32) -> DgsStatusResponse {
        let frames = DgsFrames { fastest: 63, slowest: 64, average: 333_333, last: 1, hitches: 2 };
        DgsStatusResponse { frames: Some(frames), players, boot_id, ..DgsStatusResponse::default() }
    }

    fn in_game_peer() -> Peer {
        let mut peer = connected_peer();
        peer.stage = Stage::InGame;
        peer.boot_id = 1;
        peer
    }

    /// A real action frame of the player holding W, then one of it standing still.
    const FORWARD_FRAME: &str = "01b864d9450000000737579b1ceac2bc3f000101b864d945000000579b1ceac2bc3f0014160500000000000101b864d9450000005801000000c47e22eac2bc3f00";
    const IDLE_FRAME: &str = "01b864d9450000000732d7793e74bfbc3f000101b864d945000000d7793e74bfbc3f0014160500000000000001b864d945000000c2bd4374bfbc3f00";

    fn frame(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
    }

    fn moving_peer() -> Peer {
        let mut peer = in_game_peer();
        peer.receive_player(joined(1_000_001, None, true));
        peer
    }

    #[test]
    fn the_players_frames_go_to_its_dgs_one_call_at_a_time() {
        let mut peer = moving_peer();
        assert!(peer.receive_input(&frame(FORWARD_FRAME)));
        let frames = peer.input_to_forward().unwrap();
        assert_eq!((frames.len(), frames[0].move_buttons), (1, 1));
        peer.receive_input(&frame(FORWARD_FRAME));
        assert_eq!(peer.input_to_forward(), None, "a call is out");
        peer.receive_movement(None);
        assert_eq!(peer.input_to_forward().map(|frames| frames.len()), Some(1));
    }

    #[test]
    fn standing_still_calls_once_and_the_last_still_frame_starts_the_next_move() {
        let mut peer = moving_peer();
        peer.receive_input(&frame(FORWARD_FRAME));
        peer.input_to_forward().unwrap();
        peer.receive_movement(None);
        peer.receive_input(&frame(IDLE_FRAME));
        assert_eq!(peer.input_to_forward().map(|frames| frames[0].move_buttons), Some(0), "the frame that stops the player");
        peer.receive_movement(None);
        peer.receive_input(&frame(IDLE_FRAME));
        assert_eq!(peer.input_to_forward(), None);
        peer.receive_input(&frame(FORWARD_FRAME));
        let frames = peer.input_to_forward().unwrap();
        assert_eq!(frames.iter().map(|frame| frame.move_buttons).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn frames_before_the_game_or_of_another_entity_are_dropped() {
        let mut peer = connected_peer();
        peer.receive_input(&frame(FORWARD_FRAME));
        assert!(peer.input.is_empty());
        let mut peer = moving_peer();
        let other = frame(&FORWARD_FRAME.replace("01b864d945000000", "02b864d945000000"));
        peer.receive_input(&other);
        assert!(peer.input.is_empty());
    }

    #[test]
    fn the_client_gets_what_moved_since_its_bind_and_only_once() {
        let mut peer = moving_peer();
        let bound = peer.movement_sent.unwrap();
        assert_eq!(peer.movement_message(), None, "nothing from the DGS yet");
        peer.receive_movement(Some(PlayerMovement { y: bound.y + 1.0, revision: 3, ..bound }));
        let message = peer.movement_message().unwrap();
        assert_eq!(message[0], 16);
        assert_eq!(&message[3 + 8..3 + 8 + 4], [0x00, 0x08, 0xf7, 0x03], "the position alone");
        assert_eq!(peer.movement_message(), None);
        peer.reliable = Some(Vec::new());
        acknowledged(&mut peer);
        peer.receive_movement(Some(PlayerMovement { y: bound.y + 2.0, revision: 4, ..bound }));
        assert!(peer.advance().is_some());
        assert_eq!(peer.movement_message(), None, "the packet carried it");
    }

    #[test]
    fn a_player_standing_still_is_sent_its_frame_only_now_and_then() {
        let mut peer = moving_peer();
        let bound = peer.movement_sent.unwrap();
        let moved = PlayerMovement { y: bound.y + 1.0, revision: 3, frame_time: 1_000_000, ..bound };
        peer.receive_movement(Some(moved));
        assert!(peer.movement_message().is_some());
        // The DGS answers every frame of a client standing still, each a frame later and nothing else.
        for frame in 1..=150 {
            peer.receive_movement(Some(PlayerMovement { frame_time: 1_000_000 + frame * 333_333, ..moved }));
            assert_eq!(peer.movement_message(), None, "frame {frame}");
        }
        assert_eq!(peer.movement_counts.updates, 1);
        peer.frame_time_sent -= FRAME_TIME_REFRESH;
        let message = peer.movement_message().unwrap();
        assert_eq!(&message[3 + 8..3 + 8 + 4], [0x00, 0x08, 0x8d, 0x07], "the frame alone");
        assert_eq!(peer.movement_sent.unwrap().frame_time, 1_000_000 + 150 * 333_333);
        assert_eq!(peer.movement_message(), None);
    }

    #[test]
    fn the_overlay_numbers_are_sent_again_as_they_change() {
        let mut peer = in_game_peer();
        peer.receive_status(Some(status(1, 3)), 5);
        let expected = vec![2, 10, 0, 0x3f, 0xc0, 0x00, 0x95, 0xac, 0x14, 0x01, 0x02, 0x05, 0x03];
        assert_eq!(peer.pending.pop_back(), Some(expected));
    }

    #[test]
    fn a_dgs_that_stopped_answering_shows_no_frames_and_no_population() {
        let mut peer = in_game_peer();
        peer.receive_status(None, 5);
        assert_eq!(peer.pending.pop_back(), Some(vec![2, 7, 0, 0, 0, 0, 0, 0, 5, 0]));
        assert_eq!(peer.boot_id, 1, "the same run may come back");
    }

    #[test]
    fn numbers_do_not_pile_up_behind_a_client_that_stopped_acknowledging() {
        let mut peer = in_game_peer();
        peer.receive_status(Some(status(1, 1)), 1);
        peer.receive_status(Some(status(1, 2)), 1);
        assert_eq!(peer.pending.len(), 1);
    }

    #[test]
    fn numbers_wait_for_the_end_of_the_join() {
        let mut peer = connected_peer();
        peer.stage = Stage::AsyncSpawns;
        peer.receive_status(Some(status(1, 1)), 1);
        assert!(peer.pending.is_empty());
    }

    #[test]
    fn a_stage_holds_until_the_dgss_have_sent_what_it_binds() {
        let mut peer = connected_peer();
        peer.world = None;
        peer.stage = Stage::Unstowing;
        peer.client_state = UNIVERSE_ROOT_STATE;
        peer.plan();
        assert_eq!((peer.stage, peer.pending.len()), (Stage::Unstowing, 0));
        peer.world = Some(world());
        peer.plan();
        assert_eq!(peer.stage, Stage::BatchSent);
    }

    #[test]
    fn the_solar_systems_wait_for_the_dgs_that_holds_the_planet() {
        let mut peer = connected_peer();
        peer.player = PlayerEntities::Waiting;
        peer.stage = Stage::BatchEnded;
        peer.client_state = SOLAR_SYSTEM_STATE;
        peer.plan();
        assert_eq!((peer.stage, peer.pending.len()), (Stage::BatchEnded, 0));
        peer.player = joined(1_000_001, None, true);
        peer.plan();
        assert_eq!(peer.stage, Stage::SolarSystemsSent);
    }

    #[test]
    fn a_held_client_still_gets_its_pongs() {
        let from = "127.0.0.1:1".parse().unwrap();
        let mut peers = HashMap::new();
        handle_packet(&real_request(), from, &mut peers).unwrap();
        let peer = peers.get_mut(&from).unwrap();
        assert_eq!((peer.world.as_ref(), &peer.player), (None, &PlayerEntities::Waiting));
        assert!(handle_packet(&ping(1), from, &mut peers).is_some());
    }

    #[test]
    fn an_answer_for_a_connection_that_was_replaced_is_dropped() {
        let from: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut peer = connected_peer();
        peer.session_id = 2;
        peer.world = None;
        let mut peers = HashMap::from([(from, peer)]);
        receive_event(Event::World { from, session: 1, world: world() }, &mut peers);
        assert_eq!(peers[&from].world, None);
        receive_event(Event::World { from, session: 2, world: world() }, &mut peers);
        assert!(peers[&from].world.is_some());
    }

    #[test]
    fn with_no_server_to_name_the_last_state_goes_alone() {
        let mut peer = connected_peer();
        peer.stage = Stage::PlayerEnded;
        peer.player = PlayerEntities::NoCharacter;
        peer.plan();
        assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::InGame, Some(vec![6, 1, 0, 17])));
    }

    #[test]
    fn the_player_stands_in_the_hangar_at_its_spawn_point() {
        let mut peer = connected_peer();
        peer.stage = Stage::ContentEnded;
        peer.plan();
        let messages = peer.pending.pop_back().unwrap();
        let transform = hex_bytes(concat!(
            "000000a099991740", "000000c0f54855c0", "000000803d0a1240",
            "0000000000000000", "0000000000000000", "0000000000000000", "000000000000f03f", "000000000000f03f",
        ));
        let parent = hex_bytes(concat!("0098063933000000", "b21ec046"));
        let at = |needle: &[u8]| messages.windows(needle.len()).position(|window| window == needle);
        assert!(at(&transform).unwrap() < at(&parent).unwrap());
    }

    /// The floor of Orbituary's hangar is in `hangar_smlfrnt_002`, the third of its containers, at 48, -1, -2.
    #[test]
    fn after_the_hangar_its_containers_go_in_a_batch_of_their_own() {
        let mut peer = connected_peer();
        peer.stage = Stage::HangarEnded;
        peer.plan();
        assert_eq!(peer.stage, Stage::ContentSent);
        let messages = peer.pending.pop_back().unwrap();
        assert_eq!(&messages[..7], hex_bytes("1a040005000000").as_slice());
        let under_hangar = hex_bytes(concat!("0098063933000000", "b21ec046"));
        assert_eq!(messages.windows(under_hangar.len()).filter(|window| *window == under_hangar).count(), 7);
        let shell_socpak = hex_bytes(concat!("e6bca1ff", "ea4b4e5d", "38e0597f"));
        assert!(messages.windows(shell_socpak.len()).any(|window| window == shell_socpak));
    }

    #[test]
    fn a_home_with_no_hangar_leaves_the_player_in_its_system() {
        let mut peer = connected_peer();
        peer.player = joined(1_000_001, Some(1), true);
        peer.stage = Stage::AsyncSpawns;
        peer.client_state = 16;
        peer.plan();
        let messages = peer.pending.pop_back().unwrap();
        let parent = hex_bytes(concat!("ae4501912e000000", "f1bbb226"));
        assert_eq!(peer.stage, Stage::PlayerSent);
        assert!(messages.windows(parent.len()).any(|window| window == parent));
    }

    #[test]
    fn a_session_may_name_the_home_location() {
        assert_eq!(session_account(b"hub-1000001-2026442305"), Some(1_000_001));
        assert_eq!(session_home_location(b"hub-1000001-2026442305"), Some(2_026_442_305));
        assert_eq!(session_home_location(b"hub-1000001"), None);
    }

    #[test]
    fn the_player_batch_ends_with_nothing_but_its_end() {
        let mut peer = connected_peer();
        peer.stage = Stage::PlayerSent;
        peer.batches_completed.push(PLAYER_BATCH);
        peer.plan();
        let expected = hex_bytes("1e040003000000");
        assert_eq!(peer.pending.pop_back(), Some(expected));
    }

    #[test]
    fn the_player_bind_carries_the_actor_state() {
        let mut peer = connected_peer();
        peer.stage = Stage::ContentEnded;
        peer.plan();
        let messages = peer.pending.pop_back().unwrap();
        let snapshots = hex_bytes(concat!(
            "09", "01", "8c501444", "26c09f0b", "01",
            "09", "bf88f3c3", "6cc8abaf", "080000000000000000",
            "09", "ee9265f7", "08c076f0", "010000000000000000",
            "10", "e566c4d8", "7f11a5e7", "0000000000000000", "41420f0000000000",
        ));
        let batch_waiting = hex_bytes("1c040003000000");
        assert_eq!(messages[messages.len() - batch_waiting.len()..], batch_waiting);
        let body = &messages[..messages.len() - batch_waiting.len()];
        let start = body.windows(snapshots.len()).position(|window| window == snapshots).expect("the actor state");
        // Then the movement: position, rotation, zone host and parent, each with its length, key and type.
        let movement = &body[start + snapshots.len()..];
        assert_eq!(movement[0], 0x18);
        assert_eq!(movement[1..9], hex_bytes("7bddf87c957ab6d6")[..]);
        let rest = &movement[9 + 0x18..];
        assert_eq!(rest[..9], hex_bytes("1053ab414c23acc42e")[..]);
        let rest = &rest[9 + 0x10..];
        assert_eq!(rest[..9], hex_bytes("08cb27bcf76ae38a72")[..]);
        let rest = &rest[9 + 8..];
        assert_eq!(rest[..9], hex_bytes("08e1d83f2f6ae38a72")[..]);
        // Last, `currentLocationId`: Orbituary, which the overlay shows as `Current player location`.
        assert_eq!(rest[9 + 8..], hex_bytes("047e1ce995ea4b4e5d4ab69832")[..]);
    }

    #[test]
    fn the_items_follow_the_player_a_depth_at_a_time_each_after_its_parents_batch_ended() {
        let mut peer = connected_peer();
        let joined = peer.joined().unwrap().clone();
        let depths = item_depths(joined.player.entity_id, &joined.player_items);
        assert!(depths.iter().all(Option::is_some));
        let deepest = depths.iter().flatten().max().copied().unwrap();
        assert_eq!(deepest, 4, "player > body > head > eyes > lens > radar");
        peer.stage = Stage::PlayerEnded;
        let mut sent = 0;
        for depth in 0..=deepest {
            peer.plan();
            assert_eq!(peer.stage, Stage::ItemsSent(depth));
            let binds: Vec<EntityBind> =
                joined.player_items.iter().zip(&depths).filter(|(_, at)| **at == Some(depth)).map(|(item, _)| item.clone()).collect();
            assert!(binds.iter().all(|item| item.parent_id == joined.player.entity_id || depth > 0));
            assert_eq!(peer.pending.pop_back(), Some(bind_batch(ITEMS_BATCH + depth, &binds)));
            sent += binds.len();
            peer.plan();
            assert_eq!(peer.stage, Stage::ItemsSent(depth), "held until the client has spawned the batch");
            peer.batches_completed.push(ITEMS_BATCH + depth);
            peer.plan();
            assert_eq!((peer.stage, peer.pending.pop_back()), (Stage::ItemsEnded(depth), Some(bind_batch_end(ITEMS_BATCH + depth))));
        }
        assert_eq!(sent, 23);
        peer.plan();
        assert_eq!(peer.stage, Stage::InGame);
    }

    #[test]
    fn a_player_with_no_items_goes_straight_in() {
        let mut peer = connected_peer();
        let PlayerEntities::Joined(joined) = &mut peer.player else { panic!("not joined") };
        joined.player_items.clear();
        peer.stage = Stage::PlayerEnded;
        peer.plan();
        assert_eq!(peer.stage, Stage::InGame);
    }

    #[test]
    fn an_item_whose_parent_is_unknown_is_not_sent() {
        let item = |id: u64, parent: u64| EntityBind { entity_id: id, parent_id: parent, ..EntityBind::default() };
        let items = [item(10, 1), item(11, 10), item(12, 99), item(13, 11)];
        assert_eq!(item_depths(1, &items), [Some(0), Some(1), None, Some(2)]);
    }

    #[test]
    fn the_account_comes_from_the_hub_session() {
        assert_eq!(session_account(b"hub-1000001"), Some(1_000_001));
        assert_eq!(session_account(b"check"), None);
    }

    #[test]
    fn a_session_with_no_character_gets_no_player() {
        let mut peer = connected_peer();
        peer.player = PlayerEntities::NoCharacter;
        peer.stage = Stage::AsyncSpawns;
        peer.client_state = 16;
        peer.plan();
        assert_eq!(peer.stage, Stage::AsyncSpawns);
    }

    #[test]
    fn a_real_request_names_its_session_and_three_keys() {
        let packet = real_request();
        let request = parse_connect_request(&packet[HEADER_LEN..packet.len() - 4]).unwrap();
        assert_eq!(request.session_guid, b"hub-1000001");
        assert!(request.pubkeys.iter().all(|key| Point::parse(key).is_some()));
    }

    #[test]
    fn a_corrupted_request_gets_no_answer() {
        let mut packet = real_request();
        packet[20] ^= 1;
        assert!(handle_packet(&packet, "127.0.0.1:1".parse().unwrap(), &mut HashMap::new()).is_none());
    }

    fn connected_peer() -> Peer {
        let packet = real_request();
        let request = parse_connect_request(&packet[HEADER_LEN..packet.len() - 4]).unwrap();
        let (ack, keys) = accept_connect(&request).unwrap();
        let mut peer = Peer::new(&request, ack, &keys);
        peer.world = Some(world());
        peer.player = joined(1_000_001, None, true);
        peer
    }

    #[test]
    fn a_client_is_greeted_once_after_its_first_data_packet() {
        let mut peer = connected_peer();
        assert!(peer.greet("s", "n").is_none());
        peer.data_packets = 1;
        let packet = peer.greet("s", "n").expect("a greeting");
        assert_eq!(&packet[..3], &[HEADER_MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED]);
        assert!(peer.greet("s", "n").is_none());
    }

    #[test]
    fn a_greeting_is_resent_unchanged_but_for_the_flag_until_acknowledged() {
        let mut peer = connected_peer();
        peer.data_packets = 1;
        let packet = peer.greet("s", "n").unwrap();
        assert!(peer.reliable_resend().is_none());
        peer.reliable_sent -= GREETING_RESEND_PERIOD;
        let resend = peer.reliable_resend().expect("a resend");
        assert_eq!(resend[2], FLAG_ENCRYPTED | FLAG_RESEND);
        assert_eq!((&resend[..2], &resend[3..]), (&packet[..2], &packet[3..]));
        peer.reliable_acked = true;
        peer.reliable_sent -= GREETING_RESEND_PERIOD;
        assert!(peer.reliable_resend().is_none());
    }

    #[test]
    fn the_next_state_waits_for_the_ack_and_for_the_client_to_catch_up() {
        let mut peer = connected_peer();
        peer.data_packets = 1;
        peer.greet("s", "n").unwrap();
        peer.client_state = VIEW_STATE;
        assert!(peer.advance().is_none());
        peer.reliable_acked = true;
        peer.client_state = VIEW_STATE - 1;
        assert!(peer.advance().is_none());
        peer.client_state = VIEW_STATE;
        assert!(peer.advance().is_some());
        assert_eq!((peer.sequence, peer.reliable_acked), (1, false));
        assert!(peer.advance().is_none());
    }

    fn acknowledged(peer: &mut Peer) {
        peer.reliable_acked = true;
    }

    /// A peer that has been told to unstow, with that packet acknowledged.
    fn unstowing_peer() -> Peer {
        let mut peer = connected_peer();
        peer.data_packets = 1;
        peer.greet("s", "n").unwrap();
        acknowledged(&mut peer);
        peer.client_state = VIEW_STATE;
        peer.advance().unwrap();
        acknowledged(&mut peer);
        peer
    }

    #[test]
    fn the_batch_waits_for_the_client_to_reach_the_universe_root_state() {
        let mut peer = unstowing_peer();
        peer.client_state = UNIVERSE_ROOT_STATE - 1;
        assert!(peer.advance().is_none());
        peer.client_state = UNIVERSE_ROOT_STATE;
        assert!(peer.advance().is_some());
        assert_eq!((peer.sequence, peer.stage), (2, Stage::BatchSent));
    }

    #[test]
    fn the_end_of_a_batch_waits_for_its_ack_and_for_the_spawns_to_complete() {
        let mut peer = unstowing_peer();
        peer.client_state = UNIVERSE_ROOT_STATE;
        peer.advance().unwrap();
        peer.batches_completed.push(FIRST_BATCH);
        assert!(peer.advance().is_none());
        acknowledged(&mut peer);
        assert!(peer.advance().is_some());
        assert_eq!((peer.sequence, peer.stage), (3, Stage::BatchEnded));
        acknowledged(&mut peer);
        assert!(peer.advance().is_none());
    }

    #[test]
    fn a_later_packet_is_resent_until_acknowledged() {
        let mut peer = unstowing_peer();
        peer.client_state = UNIVERSE_ROOT_STATE;
        let packet = peer.advance().unwrap();
        peer.reliable_sent -= GREETING_RESEND_PERIOD;
        assert_eq!(peer.reliable_resend().unwrap()[3..], packet[3..]);
    }

    const GAME_MODE_RECORD: &str = concat!(
        "01b4f9e430000000", "01000000", "00000000", "00", "c11ce884", "00", "8060",
        "0000000000000000", "0000000000000000", "0000000000000000",
        "0000000000000000", "0000000000000000", "0000000000000000", "000000000000f03f", "000000000000f03f",
        "00", "0000000000000000", "fcffffffffffffff", "b21ec046", "00",
        "0000000000000000", "0000000000000000", "00",
        "01", "0f", "93d1f52f", "24b1a054", "0e", "4d6567614d61702e50555f416c6c",
    );

    #[test]
    fn the_first_batch_binds_the_root_then_the_game_mode_under_it() {
        let batch = bind_batch(FIRST_BATCH, &world().universe);
        let body = [&[0u8; 12][..], &batch].concat();
        let records: Vec<&[u8]> = messages(&body).into_iter().filter(|(id, _)| *id == 27).map(|(_, payload)| payload).collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0][..8], 0xffff_ffff_ffff_fffcu64.to_le_bytes());
        assert_eq!(records[1], hex_bytes(GAME_MODE_RECORD));
    }

    /// The client's end of a whole batch, through the real packet path.
    #[test]
    fn a_completed_message_from_the_client_ends_the_batch_once() {
        let from = "127.0.0.1:1".parse().unwrap();
        let clients: Vec<KeyPair> = (0..3).map(|_| KeyPair::generate()).collect();
        let mut peers = HashMap::new();
        let keys = connect(&clients, from, &mut peers);
        let mut client = Channel::new(&[keys[1], keys[0], keys[2]]);
        let mut send = |peers: &mut HashMap<SocketAddr, Peer>, sequence: u32, message: &str| {
            let body = hex_bytes(&format!("00000000{:02x}000000ffffffff{message}", sequence));
            let packet = client.seal([HEADER_MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED, 0, 0, 0, 0, 0], &body).unwrap();
            handle_packet(&packet, from, peers).expect("an ack");
        };
        send(&mut peers, 0, "06010003");
        let peer = peers.get_mut(&from).unwrap();
        peer.world = Some(world());
        peer.greet("s", "n").unwrap();
        acknowledged(peer);
        peer.advance().unwrap();
        acknowledged(peer);
        send(&mut peers, 1, "0601000a");
        let peer = peers.get_mut(&from).unwrap();
        peer.advance().unwrap();
        acknowledged(peer);
        assert!(peer.advance().is_none());
        send(&mut peers, 2, "1d040002000000");
        assert!(peers.get_mut(&from).unwrap().advance().is_none(), "another batch");
        send(&mut peers, 3, "1d040001000000");
        send(&mut peers, 3, "1d040001000000");
        let peer = peers.get_mut(&from).unwrap();
        assert!(peer.advance().is_some());
        acknowledged(peer);
        assert!(peer.advance().is_none());
    }

    /// A run of the size of the player's batch with a saved character (1633 bytes: the bind is 1619) goes as packets in a
    /// row, one per ack, with nothing between them, each within what the client receives.
    #[test]
    fn a_run_longer_than_a_packet_goes_as_consecutive_packets() {
        let packet = real_request();
        let request = parse_connect_request(&packet[HEADER_LEN..packet.len() - 4]).unwrap();
        let (ack, keys) = accept_connect(&request).unwrap();
        let mut peer = Peer::new(&request, ack, &keys);
        let client = Channel::new(&[keys[1], keys[0], keys[2]]);
        peer.stage = Stage::InGame;
        peer.reliable = Some(Vec::new());
        peer.reliable_acked = true;
        peer.sequence = 6;
        let mut run = Vec::new();
        crate::channel::message(26, &3u32.to_le_bytes(), &mut run);
        crate::channel::message(27, &[5; 1616], &mut run);
        crate::channel::message(28, &3u32.to_le_bytes(), &mut run);
        peer.pending.push_back(run.clone());
        peer.pending.push_back(state_change(17));
        let mut areas = Vec::new();
        while let Some(packet) = peer.advance() {
            assert!(packet.len() <= 0x590, "the client's receive buffer");
            assert!(peer.advance().is_none(), "one in flight");
            let body = client.open(&packet).unwrap();
            let sequence = 7 + areas.len() as u32;
            assert_eq!(body[..12], later_packet(sequence, &[])[..]);
            areas.push(body[12..].to_vec());
            peer.reliable_acked = true;
            if areas.len() == 4 {
                break;
            }
        }
        assert_eq!(areas.iter().map(Vec::len).collect::<Vec<_>>(), [MAX_PACKET_MESSAGES, 426, 7, 4]);
        assert_eq!(areas[..3].concat(), run);
        assert_eq!(areas[3], state_change(17));
    }

    fn ping(time: u64) -> Vec<u8> {
        let header = [HEADER_MAGIC, PING, FLAG_PLAINTEXT, 0, 0, 0, 0, 0];
        let body = time.to_le_bytes();
        [&header[..], &body, &packet_crc(&header, &body).to_le_bytes()].concat()
    }

    #[test]
    fn a_connected_peer_gets_a_pong_with_its_ping_time() {
        let from = "127.0.0.1:1".parse().unwrap();
        let mut peers = HashMap::new();
        handle_packet(&real_request(), from, &mut peers).unwrap();
        let reply = handle_packet(&ping(0x1122), from, &mut peers).expect("a pong");
        assert_eq!(reply.len(), 28);
        assert_eq!(&reply[..3], &[HEADER_MAGIC, PONG, FLAG_PLAINTEXT]);
        assert_eq!(&reply[8..16], &0x1122u64.to_le_bytes());
        let clock = u64::from_le_bytes(reply[16..24].try_into().unwrap());
        assert!(clock.abs_diff(server_clock()) < 600_000_000, "a minute of ticks either way");
        assert_eq!(reply[24..], packet_crc(&reply[..8], &reply[8..24]).to_le_bytes());
    }

    #[test]
    fn strangers_and_corrupt_pings_get_nothing() {
        let from = "127.0.0.1:1".parse().unwrap();
        assert!(handle_packet(&ping(1), from, &mut HashMap::new()).is_none());
        let mut peers = HashMap::new();
        handle_packet(&real_request(), from, &mut peers).unwrap();
        let mut corrupt = ping(1);
        corrupt[9] ^= 1;
        assert!(handle_packet(&corrupt, from, &mut peers).is_none());
    }

    #[test]
    fn both_sides_derive_the_same_keys() {
        let clients: Vec<KeyPair> = (0..3).map(|_| KeyPair::generate()).collect();
        let mut pubkeys = [[0u8; PUBKEY_LEN]; 3];
        for (slot, client) in pubkeys.iter_mut().zip(&clients) {
            *slot = client.public.encode();
        }
        let request = ConnectRequest { session_guid: vec![1], pubkeys };
        let (ack, keys) = accept_connect(&request).unwrap();
        for (index, client) in clients.iter().enumerate() {
            let start = HEADER_LEN + index * PUBKEY_LEN;
            let server = Point::parse(&ack[start..start + PUBKEY_LEN]).unwrap();
            assert_eq!(derive_key(&client.shared_secret(&server).unwrap()), keys[index]);
        }
    }

    fn connect(clients: &[KeyPair], from: SocketAddr, peers: &mut HashMap<SocketAddr, Peer>) -> [[u8; 16]; 3] {
        let header = hex_bytes("5c0004dcc5e73e61");
        let mut body = REQUEST_MAGIC_NORMAL.to_le_bytes().to_vec();
        body.push(0);
        for client in clients {
            body.extend_from_slice(&client.public.encode());
        }
        let crc = packet_crc(&header, &body);
        let ack = handle_packet(&[&header[..], &body, &crc.to_le_bytes()].concat(), from, peers).unwrap();
        let mut keys = [[0u8; 16]; 3];
        for (index, client) in clients.iter().enumerate() {
            let start = HEADER_LEN + index * PUBKEY_LEN;
            let server = Point::parse(&ack[start..start + PUBKEY_LEN]).unwrap();
            keys[index] = derive_key(&client.shared_secret(&server).unwrap());
        }
        keys
    }

    fn data_packet(client: &mut Channel, flags: u8) -> Vec<u8> {
        let body = hex_bytes("0000000000000000ffffffff06010003");
        client.seal([HEADER_MAGIC, REPLICATION_DATA, flags, 0xdc, 1, 2, 3, 4], &body).unwrap()
    }

    #[test]
    fn replication_data_is_acknowledged_under_the_client_receive_key() {
        let from = "127.0.0.1:1".parse().unwrap();
        let clients: Vec<KeyPair> = (0..3).map(|_| KeyPair::generate()).collect();
        let mut peers = HashMap::new();
        let keys = connect(&clients, from, &mut peers);
        let mut sender = Channel::new(&[keys[1], keys[0], keys[2]]);
        let reply = handle_packet(&data_packet(&mut sender, FLAG_ENCRYPTED), from, &mut peers).expect("an ack");
        assert_eq!(&reply[..3], &[HEADER_MAGIC, REPLICATION_ACK, FLAG_ENCRYPTED]);
        assert_eq!(sender.open(&reply).unwrap(), [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
        assert_eq!(peers[&from].data_packets, 1);
    }

    #[test]
    fn a_repeated_request_keeps_the_keys_the_client_already_has() {
        let from = "127.0.0.1:1".parse().unwrap();
        let mut peers = HashMap::new();
        let first = handle_packet(&real_request(), from, &mut peers).unwrap();
        let second = handle_packet(&real_request(), from, &mut peers).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn replication_data_under_the_wrong_keys_gets_no_answer() {
        let from = "127.0.0.1:1".parse().unwrap();
        let mut peers = HashMap::new();
        handle_packet(&real_request(), from, &mut peers).unwrap();
        let mut stranger = Channel::new(&[[1; 16], [2; 16], [3; 16]]);
        assert!(handle_packet(&data_packet(&mut stranger, FLAG_ENCRYPTED), from, &mut peers).is_none());
    }

    #[test]
    fn truncated_requests_are_rejected() {
        assert!(parse_connect_request(&REQUEST_MAGIC_NORMAL.to_le_bytes()).is_none());
        let mut body = REQUEST_MAGIC_NORMAL.to_le_bytes().to_vec();
        body.extend_from_slice(&[0, 1, 2, 3]);
        assert!(parse_connect_request(&body).is_none());
    }
}
