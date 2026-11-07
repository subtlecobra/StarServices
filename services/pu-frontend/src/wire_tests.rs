//! The whole path of an entity, from a real DGS over gRPC to the bytes a client gets, and how a join behaves when a DGS is
//! not there. Part of `shard`, for its peers.
//!
//! `testdata/golden_binds.tsv` is every bind the shard sends, for every session form and home location, with the planet
//! zone on and off. The binds must stay these bytes. The binds carry the game's data, so the file is not in the
//! repository: `scripts/game-files.sh` writes it, after the DGS's test data, from what the code sends then. It is a
//! snapshot of this machine's game build and of the code at the time it was written, so a later edit that changes the
//! wire shows up here. Write it again (run the script, or `SC_WRITE_GOLDEN`, below) only after a deliberate change or a
//! game patch.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use sc_dgs::{
    control::{player_entities, Control, Settings},
    frames,
    solar_system::{solar_system_binds, SOLAR_SYSTEMS},
    universe::first_binds,
};
use sc_proto::internal::{
    dgs_control_client::DgsControlClient, dgs_control_server::DgsControlServer, DgsStatusRequest, DgsStatusResponse, EntityBind,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc::UnboundedReceiver, oneshot},
    time::{sleep, timeout},
};
use tonic::transport::{server::TcpIncoming, Server};

use super::*;
use crate::channel::messages;
use crate::dgs::Directory;
use crate::link::{Event, Identity, Joined, Link, PlayerEntities, World};

/// `testdata/golden_binds.tsv`, read at run time so that a checkout without it says how to make it.
fn golden() -> &'static str {
    static GOLDEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/golden_binds.tsv");
    GOLDEN.get_or_init(|| std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}. The golden binds come from the game and are not in the repository: generate them from your copy \
             of the game with scripts/game-files.sh (see README, \"Game files\")",
            path.display()
        )
    }))
}
/// The roots of the `default` template that the home locations touch.
const AREAS: &str = "stanton:stanton-1=stanton1 stanton:stanton-3=stanton2 stanton:stanton-5=stanton4 \
    stanton:stanton-7=stanton3,stantonsystem pyro:pyro-1=pyro1,pyro2,pyro3,pyrosystem pyro:pyro-2=pyro5,pyro6 \
    nyx:nyx-1=nyxsystem";
const SHARD: &str = "pub_usa_12913470_010";
const RETRY: Duration = Duration::from_millis(20);
const PATIENCE: Duration = Duration::from_secs(10);
/// Long enough for many retries, for a test that a join stays held.
const HELD: Duration = Duration::from_millis(300);

/// What the DGSs send, built in this process, for tests of the stages that do not need the network.
pub fn world() -> World {
    let solar_systems = solar_system_binds(&SOLAR_SYSTEMS).into_iter().map(EntityBind::from).collect();
    World { universe: first_binds().into_iter().map(EntityBind::from).collect(), solar_systems }
}

/// As `world`, for a player.
pub fn joined(account: u64, location: Option<u32>, planet_zone: bool) -> PlayerEntities {
    let entities = player_entities(account, location, planet_zone).unwrap();
    PlayerEntities::Joined(Box::new(Joined {
        dgs: "pyro-1".to_string(),
        server_name: "pyro-1".to_string(),
        planet: entities.planet.map(EntityBind::from),
        hangar: entities.hangar.map(EntityBind::from),
        hangar_content: entities.hangar_content.into_iter().map(EntityBind::from).collect(),
        player: entities.player.into(),
        player_items: entities.items.into_iter().map(EntityBind::from).collect(),
        status: DgsStatusResponse::default(),
    }))
}

struct Dgs {
    settings: Settings,
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
}

impl Dgs {
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Serves on this DGS's address as a new process would, having forgotten everything.
    async fn start(&mut self) {
        // The port of a server just stopped takes a moment to come back.
        let listener = loop {
            match TcpListener::bind(self.address).await {
                Ok(listener) => break listener,
                Err(_) => sleep(RETRY).await,
            }
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let control = Control::new(self.settings.clone());
        let movements = control.movements();
        let tick = tokio::spawn(frames::run(control.frame_meter(), move |elapsed| movements.lock().unwrap().step(elapsed)));
        let server = Server::builder().add_service(DgsControlServer::new(control));
        tokio::spawn(async move {
            let _ = server.serve_with_incoming_shutdown(TcpIncoming::from(listener), async { drop(stopped.await) }).await;
            tick.abort();
        });
        self.stop = Some(stop);
    }

    async fn stop(&mut self) {
        drop(self.stop.take());
        // A connection left open by a graceful stop would keep serving the old process.
        sleep(RETRY * 3).await;
    }

    async fn status(&self) -> DgsStatusResponse {
        let mut client = DgsControlClient::connect(self.url()).await.unwrap();
        client.status(DgsStatusRequest {}).await.unwrap().into_inner()
    }

    async fn players(&self) -> u32 {
        self.status().await.players
    }
}

/// The DGSs of `areas`, none of them started.
struct Fleet {
    dgs: HashMap<String, Dgs>,
    link: Arc<Link>,
    answers: UnboundedReceiver<Event>,
}

async fn free_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap()
}

impl Fleet {
    async fn prepared(areas: &str, planet_zone: bool) -> Self {
        let mut directory = Directory::parse(SHARD, areas, "").unwrap();
        let mut dgs = HashMap::new();
        for entry in areas.split_whitespace() {
            let (owner, roots) = entry.split_once('=').unwrap();
            let (system, name) = owner.split_once(':').unwrap();
            let roots = roots.split(',').map(str::to_string).collect();
            let settings = Settings { shard_id: SHARD.into(), name: name.into(), system: system.into(), roots, planet_zone, ..Settings::default() };
            let one = Dgs { settings, address: free_address().await, stop: None };
            directory.set_endpoint(name, one.url());
            dgs.insert(name.to_string(), one);
        }
        let (events, answers) = unbounded_channel();
        Self { dgs, link: Arc::new(Link::new(directory, events).retrying_every(RETRY)), answers }
    }

    async fn running(areas: &str, planet_zone: bool) -> Self {
        let mut fleet = Self::prepared(areas, planet_zone).await;
        for one in fleet.dgs.values_mut() {
            one.start().await;
        }
        fleet
    }

    fn dgs(&mut self, name: &str) -> &mut Dgs {
        self.dgs.get_mut(name).unwrap()
    }

    /// Hands over answers until `done`.
    async fn wait(&mut self, peers: &mut HashMap<SocketAddr, Peer>, done: impl Fn(&HashMap<SocketAddr, Peer>) -> bool) {
        let started = Instant::now();
        while !done(peers) {
            let left = PATIENCE.saturating_sub(started.elapsed());
            let event = timeout(left, self.answers.recv()).await.expect("the DGSs did not answer in time").unwrap();
            receive_event(event, peers);
        }
    }

    /// Hands over what comes in `time`, for a test that nothing does.
    async fn wait_for(&mut self, peers: &mut HashMap<SocketAddr, Peer>, time: Duration) {
        let started = Instant::now();
        while let Ok(Some(event)) = timeout(time.saturating_sub(started.elapsed()), self.answers.recv()).await {
            receive_event(event, peers);
        }
    }

    /// Opens a connection, as the packet loop does after a handshake.
    fn connect(&self, peers: &mut HashMap<SocketAddr, Peer>, session: &str, id: u64) -> SocketAddr {
        let from = SocketAddr::from(([127, 0, 0, 1], 1000 + id as u16));
        let mut peer = peer_of(session);
        peer.open(&self.link, from, id);
        peers.insert(from, peer);
        from
    }

    /// Opens a connection and waits for all its answers.
    async fn join(&mut self, peers: &mut HashMap<SocketAddr, Peer>, session: &str, id: u64) -> SocketAddr {
        let from = self.connect(peers, session, id);
        self.wait(peers, |peers| answered(&peers[&from])).await;
        from
    }
}

fn answered(peer: &Peer) -> bool {
    peer.world.is_some() && peer.player != PlayerEntities::Waiting
}

fn peer_of(session: &str) -> Peer {
    let request = ConnectRequest { session_guid: session.as_bytes().to_vec(), pubkeys: [[0; PUBKEY_LEN]; 3] };
    Peer::new(&request, Vec::new(), &[[0; 16]; 3])
}

/// Takes a peer through every stage, with a client that has done everything, and writes what it sends as the golden
/// file has it. The last stage is the overlay, which is no bind and has real numbers now.
fn walk(peer: &mut Peer, session: &str, planet_zone: bool) -> Vec<String> {
    peer.reliable_acked = true;
    peer.client_state = 17;
    // The bind batches, then the player's items a depth at a time (6 to 10).
    peer.batches_completed = (1..=10).collect();
    let mut rows = Vec::new();
    loop {
        let before = peer.stage;
        peer.plan();
        if peer.stage == before {
            break;
        }
        let messages = peer.pending.pop_back().unwrap();
        if peer.stage != Stage::InGame {
            rows.push(format!("{session}\t{planet_zone}\t{:?}\t{}", peer.stage, hex(&messages)));
        }
    }
    rows.push(format!("{session}\t{planet_zone}\tEnd\t{:?}", peer.stage));
    rows
}

/// The session forms the golden file holds: those of no character or of a broken one, then a character at each home
/// location of the DGS's test table, in its order.
fn sessions() -> Vec<String> {
    let mut sessions: Vec<String> =
        ["check", "hub-5", "hub-1000000", "hub-1000001", "hub-1000002", "hub-1000007-1", "hub-1000001-abc"].map(String::from).into();
    let table = sc_dgs::testdata::text(sc_dgs::location::HOME_LOCATIONS_PATH);
    for (index, row) in table.lines().enumerate() {
        let location = row.split('\t').next().unwrap();
        sessions.push(format!("hub-{}-{location}", 1_000_010 + index));
    }
    sessions
}

fn golden_sessions(planet_zone: bool) -> Vec<&'static str> {
    let mut sessions = Vec::new();
    for row in golden().lines() {
        let [session, zone, ..] = row.split('\t').collect::<Vec<_>>()[..] else { panic!("{row}") };
        if zone == planet_zone.to_string() && !sessions.contains(&session) {
            sessions.push(session);
        }
    }
    sessions
}

async fn assert_golden(planet_zone: bool) {
    let mut fleet = Fleet::running(AREAS, planet_zone).await;
    let mut peers = HashMap::new();
    let sessions = sessions();
    let writing = std::env::var("SC_WRITE_GOLDEN").ok();
    if writing.is_none() {
        assert_eq!(golden_sessions(planet_zone), sessions, "the sessions of the golden file; write it again with scripts/game-files.sh");
    }
    for (index, session) in sessions.iter().map(String::as_str).enumerate() {
        let from = fleet.join(&mut peers, session, index as u64 + 1).await;
        let rows = walk(peers.get_mut(&from).unwrap(), session, planet_zone);
        // `SC_WRITE_GOLDEN=<file>` appends what is sent now, to regenerate the golden file after a deliberate change.
        // `scripts/game-files.sh` runs the planet zone on, then off, one test at a time, into an empty file.
        if let Some(path) = &writing {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
            file.write_all(format!("{}\n", rows.join("\n")).as_bytes()).unwrap();
            peers.clear();
            continue;
        }
        let expected: Vec<&str> = golden().lines().filter(|row| row.starts_with(&format!("{session}\t{planet_zone}\t"))).collect();
        assert_eq!(rows.len(), expected.len(), "{session}");
        for (row, expected) in rows.iter().zip(expected) {
            assert_eq!(row, expected);
        }
        peers.clear();
    }
}

#[tokio::test]
async fn every_bind_through_the_dgss_is_the_bytes_of_before() {
    assert_golden(true).await;
}

#[tokio::test]
async fn with_the_planet_zone_off_every_bind_is_the_bytes_of_before() {
    assert_golden(false).await;
}

#[tokio::test]
async fn the_order_of_the_template_does_not_change_the_wire() {
    let reversed: Vec<&str> = AREAS.split_whitespace().rev().collect();
    let mut fleet = Fleet::running(&reversed.join(" "), true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    assert_eq!(peers[&from].world.as_ref(), Some(&world()));
}

#[tokio::test]
async fn a_player_goes_to_the_dgs_that_governs_its_home_and_the_system_to_its_own() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    // pyro6 is pyro-2's, while the system is pyro-1's.
    let at_pyro6 = fleet.join(&mut peers, "hub-1000001-2026442305", 1).await;
    // stanton1 is stanton-1's, while the system is stanton-7's.
    let at_stanton1 = fleet.join(&mut peers, "hub-1000002-4005457614", 2).await;
    // Orbituary, on pyro3, and a Lagrange station under no planet, both pyro-1's.
    let at_pyro3 = fleet.join(&mut peers, "hub-1000003", 3).await;
    let at_lagrange = fleet.join(&mut peers, "hub-1000004-810966700", 4).await;
    assert_eq!(peers[&at_pyro6].dgs(), Some("pyro-2"));
    assert_eq!(peers[&at_stanton1].dgs(), Some("stanton-1"));
    assert_eq!(peers[&at_pyro3].dgs(), Some("pyro-1"));
    assert_eq!(peers[&at_lagrange].dgs(), Some("pyro-1"));
    assert_eq!(peers[&at_pyro6].server_name(), Some("pub-usa-sc-4102-12913470-dgs-pyro-2"));
    let held = [("pyro-1", 2), ("pyro-2", 1), ("stanton-1", 1), ("stanton-7", 0), ("nyx-1", 0)];
    for (name, players) in held {
        assert_eq!(fleet.dgs(name).players().await, players, "{name}");
    }
    let status = fleet.dgs("pyro-2").status().await;
    assert_eq!(status.player_entities, 32, "a hangar, its 7 containers, a player and its 23 items");
}

#[tokio::test]
async fn a_session_with_no_account_or_character_gets_the_solar_systems_and_no_player() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    for (id, session) in [(1, "check"), (2, "hub-5")] {
        let from = fleet.join(&mut peers, session, id).await;
        assert_eq!(peers[&from].player, PlayerEntities::NoCharacter);
    }
    assert_eq!(fleet.dgs("pyro-1").players().await, 0);
}

/// Waits for the player count of a DGS to reach `players`.
async fn settle(fleet: &mut Fleet, dgs: &str, players: u32) {
    let started = Instant::now();
    while fleet.dgs(dgs).players().await != players {
        assert!(started.elapsed() < PATIENCE, "{dgs} never got to {players} players");
        sleep(RETRY).await;
    }
}

#[tokio::test]
async fn a_dropped_peer_frees_what_its_dgs_holds() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    fleet.join(&mut peers, "hub-1000002", 2).await;
    // Each: the hangar, its 7 containers, the player and its 23 items.
    assert_eq!(fleet.dgs("pyro-1").status().await.player_entities, 64);
    peers.remove(&from);
    settle(&mut fleet, "pyro-1", 1).await;
    let status = fleet.dgs("pyro-1").status().await;
    assert_eq!((status.player_entities, status.shard_entities), (32, 3));
}

#[tokio::test]
async fn a_client_that_timed_out_leaves() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    peers.get_mut(&from).unwrap().last_seen -= PEER_TIMEOUT;
    prune(&mut peers);
    assert!(peers.is_empty());
    settle(&mut fleet, "pyro-1", 0).await;
}

#[tokio::test]
async fn a_client_that_reconnects_keeps_its_entities_when_the_old_connection_leaves() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let old = fleet.join(&mut peers, "hub-1000001", 1).await;
    let new = fleet.join(&mut peers, "hub-1000001", 2).await;
    peers.remove(&old);
    fleet.wait_for(&mut peers, HELD).await;
    assert_eq!(fleet.dgs("pyro-1").players().await, 1);
    peers.remove(&new);
    settle(&mut fleet, "pyro-1", 0).await;
}

#[tokio::test]
async fn a_connection_replaced_at_its_address_leaves_too() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    // What `handle_packet` does for a new handshake from the same address.
    let mut peer = peer_of("hub-1000002");
    peer.open(&fleet.link, from, 2);
    peers.insert(from, peer);
    fleet.wait(&mut peers, |peers| answered(&peers[&from])).await;
    settle(&mut fleet, "pyro-1", 1).await;
    assert_eq!(peers[&from].joined().unwrap().player.entity_id, 300_000_000_002);
}

#[tokio::test]
async fn a_peer_gone_while_its_dgs_is_down_asks_no_more() {
    let mut fleet = Fleet::prepared("pyro:pyro-1=pyrosystem,pyro3", true).await;
    let mut peers = HashMap::new();
    let from = fleet.connect(&mut peers, "hub-1000001", 1);
    fleet.wait_for(&mut peers, HELD).await;
    peers.remove(&from);
    fleet.dgs("pyro-1").start().await;
    fleet.wait_for(&mut peers, HELD).await;
    assert_eq!(fleet.dgs("pyro-1").players().await, 0);
}

#[tokio::test]
async fn an_unreachable_dgs_holds_the_join_and_nothing_is_built_here_until_it_comes() {
    let mut fleet = Fleet::prepared("pyro:pyro-1=pyrosystem,pyro3", true).await;
    let mut peers = HashMap::new();
    let from = fleet.connect(&mut peers, "hub-1000001", 1);
    let peer = peers.get_mut(&from).unwrap();
    peer.stage = Stage::Unstowing;
    peer.client_state = 17;
    peer.batches_completed = vec![1, 2, 3, 4];
    fleet.wait_for(&mut peers, HELD).await;
    let peer = peers.get_mut(&from).unwrap();
    peer.plan();
    assert_eq!((peer.stage, peer.world.as_ref(), peer.pending.len()), (Stage::Unstowing, None, 0));

    fleet.dgs("pyro-1").start().await;
    fleet.wait(&mut peers, |peers| answered(&peers[&from])).await;
    let peer = peers.get_mut(&from).unwrap();
    peer.plan();
    assert_eq!(peer.stage, Stage::BatchSent);
    assert_eq!(fleet.dgs("pyro-1").players().await, 1);
}

#[tokio::test]
async fn only_the_governing_dgs_being_down_holds_the_solar_systems_batch() {
    let mut fleet = Fleet::prepared(AREAS, true).await;
    for one in fleet.dgs.values_mut().filter(|one| one.settings.name != "pyro-2") {
        one.start().await;
    }
    let mut peers = HashMap::new();
    let from = fleet.connect(&mut peers, "hub-1000001-2026442305", 1);
    fleet.wait(&mut peers, |peers| peers[&from].world.is_some()).await;
    fleet.wait_for(&mut peers, HELD).await;
    let peer = peers.get_mut(&from).unwrap();
    peer.stage = Stage::BatchEnded;
    peer.client_state = 17;
    peer.plan();
    assert_eq!((peer.stage, &peer.player), (Stage::BatchEnded, &PlayerEntities::Waiting));

    fleet.dgs("pyro-2").start().await;
    fleet.wait(&mut peers, |peers| answered(&peers[&from])).await;
    let peer = peers.get_mut(&from).unwrap();
    peer.plan();
    assert_eq!(peer.stage, Stage::SolarSystemsSent);
}

#[tokio::test]
async fn a_shard_with_no_dgs_address_holds_every_join() {
    let (events, mut answers) = unbounded_channel();
    let link = Arc::new(Link::new(Directory::parse(SHARD, "pyro:pyro-1=pyrosystem", "").unwrap(), events).retrying_every(RETRY));
    let session = link.open("127.0.0.1:1".parse().unwrap(), 1, Identity { account: Some(1_000_001), location: None });
    assert!(timeout(HELD, answers.recv()).await.is_err());
    drop(session);
}

#[tokio::test]
async fn a_dgs_restart_loses_no_player_and_the_overlay_tells_the_truth_meanwhile() {
    let mut fleet = Fleet::running("pyro:pyro-1=pyrosystem,pyro3", true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    let first_boot = peers[&from].boot_id;
    peers.get_mut(&from).unwrap().stage = Stage::InGame;

    fleet.dgs("pyro-1").stop().await;
    fleet.link.poll("pyro-1".to_string());
    fleet.wait(&mut peers, |peers| peers[&from].server.is_none()).await;
    assert_eq!(peers.get_mut(&from).unwrap().pending.pop_back(), Some(vec![2, 7, 0, 0, 0, 0, 0, 0, 1, 0]));

    fleet.dgs("pyro-1").start().await;
    let started = Instant::now();
    while peers[&from].boot_id == first_boot {
        assert!(started.elapsed() < PATIENCE, "the restart was not seen");
        fleet.link.poll("pyro-1".to_string());
        fleet.wait_for(&mut peers, RETRY * 5).await;
    }
    settle(&mut fleet, "pyro-1", 1).await;
    peers.clear();
    settle(&mut fleet, "pyro-1", 0).await;
}

#[tokio::test]
async fn the_overlay_shows_what_the_dgs_measured() {
    let mut fleet = Fleet::running("pyro:pyro-1=pyrosystem,pyro3", true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    peers.get_mut(&from).unwrap().stage = Stage::InGame;
    sleep(frames::FRAME_PERIOD * 4).await;
    fleet.link.poll("pyro-1".to_string());
    fleet.wait(&mut peers, |peers| !peers[&from].pending.is_empty()).await;
    let peer = peers.get_mut(&from).unwrap();
    let frames = peer.server.as_ref().unwrap().frames.unwrap();
    assert!(frames.average >= 300_000 && frames.fastest > 0 && frames.slowest >= frames.fastest, "{frames:?}");
    let message = peer.pending.pop_back().unwrap();
    let body = [&[0u8; 12][..], &message].concat();
    let (id, payload) = messages(&body)[0];
    assert_eq!(id, 2);
    assert!(payload.len() > 7, "real frame times take more than a byte each");
    assert_eq!(payload[payload.len() - 2..], [1, 1], "one connection here, one player on the DGS");
}

/// What the first batch binds, as the client's walk needs it.
const DATA_HEADER_LEN: usize = 12;
const MESSAGE_ADD_BIND: usize = 27;
const UNSTREAMABLE: u32 = 0x2000;
const STREAMING_ROOT_MASK: u32 = 0x1800;
const OWN_STREAMING_ROOT: u32 = 0x1000;
const UNIVERSE_ROOT_ID: u64 = 0xffff_ffff_ffff_fffc;

/// The fields of a record the client's walk reads, at their offsets in `BindBatchAddBind`.
#[derive(Debug, PartialEq)]
struct Record {
    id: u64,
    batch: u32,
    class_crc: u32,
    flags: u32,
    parent: u64,
    attachment: u32,
    rotation_w: f64,
    scale: f64,
}

fn record(payload: &[u8]) -> Record {
    let u32_at = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(payload[at..at + 8].try_into().unwrap());
    assert_eq!(payload[21], 0, "no components");
    let flags = u32::from(payload[22] & 0x7f) | u32::from(payload[23]) << 7;
    assert!(payload[22] & 0x80 != 0 && payload[23] & 0x80 == 0, "flags fill two varint bytes");
    // pos and the rotation's vector take 6 doubles from 24, then w and the scale.
    Record {
        id: u64_at(0),
        batch: u32_at(8),
        class_crc: u32_at(17),
        flags,
        parent: u64_at(97),
        attachment: u32_at(105),
        rotation_w: f64::from_bits(u64_at(72)),
        scale: f64::from_bits(u64_at(80)),
    }
}

fn sent_records(batch: u32) -> Vec<Record> {
    let batch_messages = bind_batch(batch, &world().solar_systems);
    let body = [&[0u8; DATA_HEADER_LEN][..], &batch_messages].concat();
    let adds = messages(&body).into_iter().filter(|(id, _)| *id == MESSAGE_ADD_BIND);
    let systems = adds.filter(|(_, payload)| payload[17..21] == 0x479c_0d1d_u32.to_le_bytes());
    systems.map(|(_, payload)| record(payload)).collect()
}

/// `GatherSolarSystemRoots` (0x141575510) and `FUN_1414f7b70`, and the `SetParent` guard before it.
fn gather(records: &[Record], parent: u64) -> Vec<u64> {
    let children = records.iter().filter(|record| record.parent == parent);
    let linked = children.filter(|record| record.flags & STREAMING_ROOT_MASK == OWN_STREAMING_ROOT);
    let unstreamable = linked.filter(|record| record.flags & UNSTREAMABLE != 0);
    let found = unstreamable.map(|record| {
        if record.class_crc == 0x479c_0d1d {
            vec![record.id]
        } else {
            gather(records, record.id)
        }
    });
    found.flatten().collect()
}

#[test]
fn the_client_walk_finds_every_system_under_the_root() {
    let ids: Vec<u64> = SOLAR_SYSTEMS.iter().map(|system| system.geid).collect();
    assert_eq!(gather(&sent_records(7), UNIVERSE_ROOT_ID), ids);
}

#[test]
fn nothing_is_found_under_any_other_parent() {
    assert!(gather(&sent_records(7), 0).is_empty());
}

#[test]
fn a_record_is_what_the_seed_spawns() {
    let first = &sent_records(7)[0];
    let expected = Record {
        id: 199_998_000_001,
        batch: 7,
        class_crc: 0x479c_0d1d,
        flags: 0x3400,
        parent: 0xffff_ffff_ffff_fffc,
        attachment: 1_186_995_890,
        rotation_w: 1.0,
        scale: 1.0,
    };
    assert_eq!(*first, expected);
}

fn hangar_record(planet_zone: bool) -> Vec<u8> {
    let PlayerEntities::Joined(joined) = joined(1_000_001, None, planet_zone) else { panic!("not joined") };
    let batch = bind_batch(3, std::slice::from_ref(joined.hangar.as_ref().unwrap()));
    let body = [&[0u8; DATA_HEADER_LEN][..], &batch].concat();
    messages(&body).into_iter().find(|(id, _)| *id == MESSAGE_ADD_BIND).unwrap().1.to_vec()
}

#[test]
fn the_hangar_record_is_a_streamable_zone_host_with_these_bytes() {
    let payload = hangar_record(true);
    assert_eq!(payload[17..21], 1_011_559_220u32.to_le_bytes());
    // One component and its server only bit, then flags as a varint.
    assert_eq!(payload[21..29], [0x01, 0x0a, 0x11, 0x2b, 0x70, 0x00, 0x80, 0x28]);
    assert_eq!(payload[102..114], [0xd4, 0x2a, 0x0f, 0x91, 0x2e, 0, 0, 0, 0xb2, 0x1e, 0xc0, 0x46]);
}

#[test]
fn with_the_planet_zone_off_the_hangar_is_at_the_origin_of_its_system() {
    let payload = hangar_record(false);
    let identity = [[0u8; 8]; 6].concat();
    assert_eq!(payload[29..77], identity);
    assert_eq!(payload[77..93], [[0, 0, 0, 0, 0, 0, 0xf0, 0x3f]; 2].concat());
    assert_eq!(payload[102..114], [0xae, 0x45, 0x01, 0x91, 0x2e, 0, 0, 0, 0xb2, 0x1e, 0xc0, 0x46]);
}

/// A frame of the client's input, with `buttons` held, at `frame` thirtieths of a second.
fn input_frame(frame: u64, buttons: u32) -> sc_proto::internal::InputFrame {
    sc_proto::internal::InputFrame { time: 10_000_000 + frame * 333_333, move_buttons: buttons, ..Default::default() }
}

#[tokio::test]
async fn the_governing_dgs_moves_the_player_from_its_input() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    let player = peers[&from].joined().unwrap().player.entity_id;
    let frames = (0..=30).map(|frame| input_frame(frame, 1)).collect();
    fleet.link.input(from, 1, "pyro-1".to_string(), player, frames);
    fleet.wait(&mut peers, |peers| peers[&from].movement.is_some()).await;
    let movement = peers[&from].movement.unwrap();
    assert_eq!((movement.entity_id, movement.zone_host_id, movement.parent_id), (player, 220_000_065_536, 220_000_065_536));
    let moved = movement.y - peers[&from].movement_sent.as_ref().unwrap().y;
    assert!((moved - sc_dgs::movement::RUN_SPEED).abs() < 1e-3, "{moved}");
    // A DGS that does not hold the player says so, and the replicant keeps what it had.
    fleet.link.input(from, 1, "pyro-2".to_string(), player, Vec::new());
    peers.get_mut(&from).unwrap().movement = None;
    fleet.wait_for(&mut peers, HELD).await;
    assert_eq!(peers[&from].movement, None);
}

/// Real frames of a client holding W, through the replicant's decoding, a real DGS and back to an update for the client.
#[tokio::test]
async fn frames_of_a_player_holding_w_come_back_as_updates_that_move_it() {
    let mut fleet = Fleet::running(AREAS, true).await;
    let mut peers = HashMap::new();
    let from = fleet.join(&mut peers, "hub-1000001", 1).await;
    {
        let peer = peers.get_mut(&from).unwrap();
        peer.stage = Stage::InGame;
        peer.reliable = Some(Vec::new());
        peer.reliable_acked = true;
    }
    let base = "01b864d9450000000737579b1ceac2bc3f000101b864d945000000579b1ceac2bc3f0014160500000000000101b864d9450000005801000000c47e22eac2bc3f00";
    let mut bytes: Vec<u8> = (0..base.len()).step_by(2).map(|at| u8::from_str_radix(&base[at..at + 2], 16).unwrap()).collect();
    let n = bytes.len();
    for frame in 0..40u64 {
        let time = 0x003f_bcc2_ea22_7ec4u64 + frame * 333_333;
        bytes[n - 8..].copy_from_slice(&time.to_le_bytes());
        let peer = peers.get_mut(&from).unwrap();
        assert!(peer.receive_input(&bytes));
        peer.forward_input(&fleet.link, from);
        if peer.input_in_flight {
            fleet.wait(&mut peers, |peers| !peers[&from].input_in_flight).await;
        }
        let peer = peers.get_mut(&from).unwrap();
        let sent = peer.advance();
        // Every frame that moved, with the frame it is of. The first has no length to move for, and its frame alone is
        // not sent.
        assert_eq!(sent.is_some(), frame > 0, "frame {frame}");
        if frame > 0 {
            assert_eq!(peer.movement_sent.unwrap().frame_time, time);
        }
        peer.reliable_acked = true;
    }
    let moved = peers[&from].movement_sent.unwrap().y - -85.14;
    assert!((moved - sc_dgs::movement::RUN_SPEED * 39.0 / 30.0).abs() < 1e-3, "{moved}");
}
