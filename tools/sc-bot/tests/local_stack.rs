//! The bot against the real replicant binary and a real DGS (in process, on its test data), with a stand-in for the Hub's
//! internal gRPC. No database and no sc-entitygraph: what needs them (equips, stacks, owners) is answered by the DGS as
//! it answers when sc-entitygraph is away, which is part of what this checks.
//!
//! Ignored by default because it needs the replicant binary and the DGS's test data (scripts/game-files.sh):
//!
//!   cargo build -p sc-pu-frontend
//!   cargo test -p sc-bot --test local_stack -- --ignored --nocapture
//!
//! `SC_BOT_REPLICANT` names the binary when it is not under `target/`.

mod support;

use std::{
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use sc_api::internal::{
    dgs_control_server::DgsControlServer,
    entity_graph_internal_client::EntityGraphInternalClient,
    entity_graph_internal_server::EntityGraphInternalServer,
    hub_internal_server::{HubInternal, HubInternalServer},
    GetPlayerShardRequest, GetPlayerShardResponse, PlayerDisconnect, ReportDisconnectResponse, ResolveSessionRequest, ResolveSessionResponse,
};
use sc_bot::{
    scenario::{self, Options, Outcome},
    scenarios,
    session::Target,
};
use sc_dgs::{
    control::{Control, Settings},
    frames,
};
use tokio::net::TcpListener;
use tonic::{transport::{server::TcpIncoming, Server}, Request, Response, Status};

const SESSION: &str = "bot-session-1";
const ACCOUNT: u32 = 1_000_001;
/// Orison's hangar on Stanton 2, a home location of the test data whose hangar has the ASOP kiosks (and so the ATC).
const HOME_LOCATION: u32 = 1_105_811_199;
const SHARD: &str = "pub_usa_12913470_010";
const AREAS: &str = "stanton:stanton-3=stanton2,stantonsystem";

/// The Hub's internal gRPC, knowing one session.
struct FakeHub;

#[tonic::async_trait]
impl HubInternal for FakeHub {
    async fn get_player_shard(&self, _: Request<GetPlayerShardRequest>) -> Result<Response<GetPlayerShardResponse>, Status> {
        Ok(Response::new(GetPlayerShardResponse::default()))
    }

    async fn resolve_session(&self, request: Request<ResolveSessionRequest>) -> Result<Response<ResolveSessionResponse>, Status> {
        let found = request.into_inner().session == SESSION;
        Ok(Response::new(ResolveSessionResponse { found, account_id: ACCOUNT, geid: 300_000_000_000 + u64::from(ACCOUNT), nickname: "bot".into(), home_location: HOME_LOCATION }))
    }

    async fn report_disconnect(&self, _: Request<PlayerDisconnect>) -> Result<Response<ReportDisconnectResponse>, Status> {
        Ok(Response::new(ReportDisconnectResponse { recorded: true, ..Default::default() }))
    }
}

async fn free_tcp() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap()
}

fn free_udp() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

fn replicant_binary() -> PathBuf {
    if let Ok(path) = std::env::var("SC_BOT_REPLICANT") {
        return PathBuf::from(path);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    ["x86_64-unknown-linux-musl/debug", "debug", "x86_64-unknown-linux-musl/release", "release"]
        .iter()
        .map(|dir| target.join(dir).join("sc-pu-frontend"))
        .find(|path| path.exists())
        .expect("no sc-pu-frontend binary: cargo build -p sc-pu-frontend (or set SC_BOT_REPLICANT)")
}

struct Replicant(Child);

impl Drop for Replicant {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the sc-pu-frontend binary (cargo build -p sc-pu-frontend) and the DGS test data (scripts/game-files.sh)"]
async fn the_bot_joins_and_plays_against_the_real_replicant_and_dgs() {
    // sc-entitygraph's stand-in, which the DGS reaches as it reaches the real one.
    let player = sc_dgs::player::player_geid(u64::from(ACCOUNT)).unwrap();
    let graph = std::sync::Arc::new(support::Graph::new(player, u64::from(HOME_LOCATION)));
    let graph_address = free_tcp().await;
    let listener = TcpListener::bind(graph_address).await.unwrap();
    let served = graph.clone();
    tokio::spawn(async move { Server::builder().add_service(EntityGraphInternalServer::from_arc(served)).serve_with_incoming(TcpIncoming::from(listener)).await });

    // The DGS, serving its gRPC as `sc-dgs` does.
    let dgs_address = free_tcp().await;
    let settings = Settings {
        shard_id: SHARD.into(),
        name: "stanton-3".into(),
        system: "stanton".into(),
        roots: AREAS.split_once('=').unwrap().1.split(',').map(str::to_string).collect(),
        planet_zone: true,
        ..Settings::default()
    };
    let control = Control::new(settings).with_graph(EntityGraphInternalClient::connect(format!("http://{graph_address}")).await.unwrap());
    let movements = control.movements();
    let tick = tokio::spawn(frames::run(control.frame_meter(), move |elapsed| movements.lock().unwrap().step(elapsed)));
    let listener = TcpListener::bind(dgs_address).await.unwrap();
    let dgs = Server::builder().add_service(DgsControlServer::new(control));
    tokio::spawn(async move { dgs.serve_with_incoming(TcpIncoming::from(listener)).await });

    // The Hub's internal gRPC.
    let hub_address = free_tcp().await;
    let listener = TcpListener::bind(hub_address).await.unwrap();
    let hub = Server::builder().add_service(HubInternalServer::new(FakeHub));
    tokio::spawn(async move { hub.serve_with_incoming(TcpIncoming::from(listener)).await });

    // The replicant, the real binary.
    let shard = free_udp();
    let log_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/sc-bot-test");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = std::fs::File::create(log_dir.join("replicant.log")).unwrap();
    let child = Command::new(replicant_binary())
        .args(["--listen", &shard.to_string(), "--shard-id", SHARD, "--shard-name", "local", "--dgs-areas", AREAS])
        .args(["--dgs-address", &format!("http://{dgs_address}"), "--hub", &format!("http://{hub_address}")])
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("the replicant starts");
    let _replicant = Replicant(child);

    let opts = Options {
        target: Target { session: Some(SESSION.into()), shard: Some(shard.to_string()), join_timeout: Duration::from_secs(60), ..Target::default() },
        dgs_url: Some(format!("http://{dgs_address}")),
        elevator_wait: Duration::from_secs(20),
        bank_location: Some(u64::from(HOME_LOCATION)),
        helmet_class: support::HELMET,
        stack_class: Some(support::STACK),
        stack_equip_class: Some(support::WEARABLE_STACK),
        stack_equip_port: "Body_ItemPort:Armor_Undersuit:oxyPen_attach_1".into(),
        foreign_item: None,
        expect_atc: true,
        ..Options::default()
    };
    let chosen: Vec<_> = scenarios::ALL.iter().collect();
    let report = scenario::run(&opts, &chosen, Duration::from_secs(180)).await;
    tick.abort();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!("entity graph calls: {:#?}", graph.log.lock().unwrap());

    let outcome = |name: &str| report.scenarios.iter().find(|s| s.name == name).unwrap();
    for scenario in &report.scenarios {
        let failed: Vec<_> = scenario.checks.iter().filter(|check| !check.ok).collect();
        println!("{}: {:?} {failed:?} {:?}", scenario.name, scenario.outcome, scenario.reason);
    }
    // What the scenarios moved is where it began: the stowed items in the bank, the gear worn again.
    let items = graph.snapshot();
    let total = |class| items.iter().filter(|item| item.class_crc == class && item.inventory.is_some()).map(|item| item.stack).sum::<u32>();
    assert!(items.iter().filter(|item| item.class_crc == support::HELMET).all(|item| item.inventory.as_deref().is_some_and(|inventory| inventory.contains(":Location:"))), "{items:?}");
    assert_eq!((total(support::STACK), total(support::WEARABLE_STACK)), (5, 3), "{items:?}");
    assert!(items.iter().filter(|item| item.inventory.is_none()).count() >= 23, "the default gear is worn again: {items:?}");
    for name in ["join", "access-token", "inventory-equip", "inventory-move", "inventory-stack", "inventory-pocket", "inventory-equip-stack", "elevator-call", "disconnect"] {
        assert_eq!(outcome(name).outcome, Outcome::Passed, "{name}: {:#?} {:?}", outcome(name).checks, outcome(name).reason);
    }
}
