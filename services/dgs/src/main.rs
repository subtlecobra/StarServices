//! The DGS binary: loads the tables of its game build (the home locations and the hangar elevators), then its area from sc-entitygraph, then the collision of its
//! system's hangars, and only then opens its port and answers the replicant. It has no database of its own. See the library for what a DGS is. sc-shard-operator runs
//! one pod per DGS of a shard's ShardTemplate.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, time::Duration};

use clap::Parser;
use sc_dgs::{
    area::{self, Area},
    control::{Control, Settings},
    frames,
    gamedata::{self, Backoff, GameData},
    location, transit,
};
use sc_api::internal::{
    character_customizer_internal_client::CharacterCustomizerInternalClient, dgs_control_server::DgsControlServer,
    entity_graph_internal_client::EntityGraphInternalClient, identity_internal_client::IdentityInternalClient, LoadAreaRequest,
};
use sc_runtime::CommonArgs;
use tokio::time::{sleep, timeout};
use tonic::transport::{Channel, Endpoint, Server};
use tracing::{info, warn};

/// The tree can be seeded after this pod starts, and it would then find no roots. sc-entitygraph may be away too.
const LOAD_RETRY: Duration = Duration::from_secs(15);
/// A load that takes longer is given up and tried again. The largest area takes seconds.
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    /// Opened once the area is loaded, so the pod is ready when this port accepts connections.
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    #[arg(long, env = "SCBACKEND_SHARD_ID")]
    shard_id: String,
    #[arg(long, env = "SCBACKEND_DGS_NAME")]
    name: String,
    /// The system this DGS is in, as `system` in `containers`.
    #[arg(long, env = "SCBACKEND_DGS_SYSTEM")]
    system: String,
    /// Names of the containers this DGS has authority over, with everything under them.
    #[arg(long, env = "SCBACKEND_DGS_ROOTS", value_delimiter = ',', required = true)]
    roots: Vec<String>,
    /// The roots of the other DGSs of the system.
    #[arg(long, env = "SCBACKEND_DGS_EXCLUDED_ROOTS", value_delimiter = ',')]
    excluded: Vec<String>,
    /// `sc-entitygraph`, which owns the container tree and walks the area for this DGS.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH", default_value = "http://sc-entitygraph.scbackend.svc:50051")]
    entitygraph: String,
    /// Put before the container collection names by sc-entitygraph, to read a test copy.
    #[arg(long, env = "SCBACKEND_COLLECTION_PREFIX", default_value = "")]
    collection_prefix: String,
    /// Bind the home planet and hang the hangar from it. `false` puts the hangar at the origin of its system.
    #[arg(long, env = "SCBACKEND_PLANET_ZONE", default_value_t = true, action = clap::ArgAction::Set)]
    planet_zone: bool,
    /// `sc-character-customizer`, asked for each joining player's saved character. Unset: every player wears the default.
    #[arg(long, env = "SCBACKEND_CUSTOMIZER")]
    customizer: Option<String>,
    /// `sc-identity`, asked for each joining player's in-game badges. Unset: no player has any.
    #[arg(long, env = "SCBACKEND_IDENTITY")]
    identity: Option<String>,
    /// The region's `sc-gamedata`, asked for the tables of the game build and the collision of the hangars this DGS
    /// loads, with `SC_GAME_BUILD`. The shard operator sets both.
    #[arg(long, env = "SC_GAMEDATA_ADDR")]
    gamedata: Option<String>,
    /// The game build the shard runs, its change number, e.g. `12913470`.
    #[arg(long, env = "SC_GAME_BUILD")]
    game_build: Option<String>,
    /// For a local run without sc-gamedata: a build directory that `tools/gamedata-build` wrote, `<out>/<build>`, read
    /// instead. Used only when `SC_GAMEDATA_ADDR` and `SC_GAME_BUILD` are not both set.
    #[arg(long, env = "SC_GAMEDATA_DIR")]
    gamedata_dir: Option<PathBuf>,
}

/// Where the game data comes from: sc-gamedata, else the local build directory. Neither is an error, as a DGS has no
/// tables and no walls without it.
fn game_data(args: &Args) -> Result<GameData, String> {
    let set = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    match (set(&args.gamedata), set(&args.game_build), &args.gamedata_dir) {
        (Some(address), Some(build), _) => GameData::service(&address, &build),
        (_, _, Some(directory)) => Ok(GameData::directory(directory)),
        _ => Err("no game data: set SC_GAMEDATA_ADDR and SC_GAME_BUILD (the shard operator does), or SC_GAMEDATA_DIR to a \
                  build directory of tools/gamedata-build for a local run"
            .into()),
    }
}

/// How long a join waits for a saved character before it gives the default.
const CUSTOMIZER_TIMEOUT: Duration = Duration::from_secs(2);

async fn governed(mut graph: EntityGraphInternalClient<Channel>, args: &Args) -> Area {
    let request = LoadAreaRequest {
        system: args.system.clone(),
        roots: args.roots.clone(),
        excluded_roots: args.excluded.clone(),
        collection_prefix: args.collection_prefix.clone(),
    };
    loop {
        match timeout(LOAD_TIMEOUT, area::load(&mut graph, request.clone())).await {
            Ok(Ok(area)) => return area,
            Ok(Err(error)) => warn!(%error, "area not loaded"),
            Err(_) => warn!(timeout = ?LOAD_TIMEOUT, "area not loaded in time"),
        }
        sleep(LOAD_RETRY).await;
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let gamedata = game_data(&args)?;
    let source = gamedata.describe();
    info!(source, "loading the game data tables");
    let tables = gamedata::retrying("tables", Backoff::default(), || gamedata.tables()).await;
    info!(source, locations = tables.locations(), "game data tables loaded");
    location::install(tables);
    let elevators = gamedata::retrying("transit tables", Backoff::default(), || gamedata.transit_tables()).await;
    info!(source, managers = elevators.managers().len(), hangars = elevators.hangars().len(), "hangar elevator tables loaded");
    transit::install(elevators);
    let endpoint = Endpoint::from_shared(args.entitygraph.clone()).map_err(|e| format!("entitygraph address: {e}"))?;
    let graph = EntityGraphInternalClient::new(endpoint.connect_timeout(CONNECT_TIMEOUT).connect_lazy());
    let started = std::time::Instant::now();
    let area = governed(graph.clone(), &args).await;
    info!(
        shard = %args.shard_id, dgs = %args.name, system = %args.system, roots = args.roots.len(),
        containers = area.containers(), entities = area.entities(), elapsed = ?started.elapsed(), "area loaded",
    );
    let hangars = location::hangar_socpaks(&args.system);
    let control = Control::new(Settings {
        shard_id: args.shard_id,
        name: args.name,
        system: args.system,
        roots: args.roots,
        governed_containers: area.containers(),
        governed_entities: area.entities(),
        planet_zone: args.planet_zone,
    });
    // The items players wear are kept by sc-entitygraph, as asked on each join.
    let control = control.with_graph(graph);
    // Connected on first use, so a DGS serves while the customizer is away; its players then wear the default.
    let control = match &args.customizer {
        Some(address) => {
            let endpoint = Endpoint::from_shared(address.clone()).map_err(|e| format!("customizer address: {e}"))?;
            let channel = endpoint.connect_timeout(CUSTOMIZER_TIMEOUT).timeout(CUSTOMIZER_TIMEOUT).connect_lazy();
            info!(%address, "saved characters from the customizer");
            control.with_customizer(CharacterCustomizerInternalClient::new(channel))
        }
        None => {
            warn!("no customizer, every player wears the default");
            control
        }
    };
    // As the customizer: a DGS serves while identity is away, and its players then have no badges.
    let control = match &args.identity {
        Some(address) => {
            let endpoint = Endpoint::from_shared(address.clone()).map_err(|e| format!("identity address: {e}"))?;
            let channel = endpoint.connect_timeout(CUSTOMIZER_TIMEOUT).timeout(CUSTOMIZER_TIMEOUT).connect_lazy();
            info!(%address, "in-game badges from identity");
            control.with_identity(IdentityInternalClient::new(channel))
        }
        None => {
            warn!("no identity, no player has in-game badges");
            control
        }
    };
    // The collision of the hangars players of this system spawn in, built off the runtime: a third of a second each.
    // The port opens only once every one is loaded, so no player is ever placed in a hangar without its walls.
    for hash in hangars {
        let started = std::time::Instant::now();
        let world = gamedata::retrying("hangar collision", Backoff::default(), || gamedata.hangar_collision(hash)).await;
        info!(hash, source, triangles = world.triangles(), elapsed = ?started.elapsed(), "hangar collision loaded");
    }
    let (movements, elevators) = (control.movements(), control.elevators());
    tokio::spawn(frames::run(control.frame_meter(), move |elapsed| {
        let mut movements = movements.lock().unwrap();
        let mut elevators = elevators.lock().unwrap();
        movements.set_vehicles(&elevators.carriage_zones());
        movements.step(elapsed);
        elevators.observe(&movements.zones());
        elevators.step(std::time::Instant::now());
    }));
    Server::builder()
        .add_service(DgsControlServer::new(control))
        .serve(args.listen)
        .await
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
