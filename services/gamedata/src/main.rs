//! sc-gamedata: a region's game data, which the DGSs ask for the files of the game build their shard runs (the
//! collision of a hangar, by its socpak hash). It is the only reader and writer of the `gd:` keys in the region's Valkey
//! cluster, and stateless itself: for every build it is given, the replica that takes the build's lock pulls the image
//! `<registry>/<repository>:<build>` (files only, under `/gamedata/<build>/`) into its work directory, checks it, and
//! copies the files into Valkey, unless Valkey already has the build complete. The other replicas wait for it, and
//! all serve every complete build. gRPC health reports "" SERVING once Valkey answers and every build is complete in
//! it, which is what the pod's readiness probe asks. See cache.rs for the keys, the locks and pruning.

mod cache;
mod kv;
mod store;
#[cfg(test)]
mod tests;

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use sc_proto::internal::game_data_server::GameDataServer;
use sc_runtime::CommonArgs;
use tonic::transport::Server;
use tracing::{info, warn};

use cache::{Cache, Settings};
use sc_oci::Registry;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// The game builds (change numbers) to hold, e.g. `12913470`.
    #[arg(long, env = "SC_GAMEDATA_BUILDS", value_delimiter = ',', required = true)]
    builds: Vec<String>,
    /// Scratch space for a pull: the image's layers and files, removed once they are in Valkey.
    #[arg(long, env = "SC_GAMEDATA_WORK_DIR", default_value = "/work")]
    work_dir: PathBuf,
    /// Valkey nodes, `host:port` or `redis://host:port`. In cluster mode any one is enough to find the others.
    #[arg(long, env = "SC_GAMEDATA_VALKEY", value_delimiter = ',', required = true)]
    valkey: Vec<String>,
    /// Valkey runs in cluster mode; false for a single server.
    #[arg(long, env = "SC_GAMEDATA_VALKEY_CLUSTER", default_value_t = true, action = clap::ArgAction::Set)]
    valkey_cluster: bool,
    /// Files larger than this are stored in parts of this size, at most Valkey's 512 MiB per value.
    #[arg(long, env = "SC_GAMEDATA_CHUNK_BYTES", default_value_t = 64 << 20)]
    chunk_bytes: u64,
    /// Who holds the locks this replica takes: its pod's name. Default: the host name.
    #[arg(long, env = "SC_GAMEDATA_OWNER", default_value = "")]
    owner: String,
    /// `host:port` of the registry with the game data images.
    #[arg(long, env = "SC_GAMEDATA_REGISTRY")]
    registry: String,
    #[arg(long, env = "SC_GAMEDATA_REPOSITORY", default_value = "scbackend/gamedata")]
    repository: String,
    /// How often to ask the registry whether a loaded build's tag still names the same image, in seconds. A build pushed
    /// again under its tag is loaded again, see cache.rs. Asked at start too.
    #[arg(long, env = "SC_GAMEDATA_RECHECK_SECS", default_value_t = 300)]
    recheck_secs: u64,
    /// Talk to the registry over HTTP instead of HTTPS.
    #[arg(long, env = "SC_GAMEDATA_PLAIN_HTTP", default_value_t = false, action = clap::ArgAction::Set)]
    plain_http: bool,
}

fn node_url(node: &str) -> String {
    match node.contains("://") {
        true => node.to_string(),
        false => format!("redis://{node}"),
    }
}

/// Connects to Valkey, retrying until it answers.
async fn connect(cache: Arc<Cache>, nodes: Vec<String>, cluster: bool) {
    let mut wait = Duration::from_secs(1);
    loop {
        let connected: Result<Arc<dyn kv::Kv>, String> = match cluster {
            true => kv::cluster(&nodes).await.map(|kv| Arc::new(kv) as Arc<dyn kv::Kv>),
            false => kv::single(&nodes[0]).await.map(|kv| Arc::new(kv) as Arc<dyn kv::Kv>),
        };
        match connected {
            Ok(kv) => {
                info!(?nodes, cluster, "valkey connected");
                cache.attach(kv);
                return;
            }
            Err(error) => warn!(%error, retry = ?wait, "valkey not reached"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let registry = Registry::new(&args.registry, &args.repository, args.plain_http)?;
    let owner = match args.owner.trim() {
        "" => std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("sc-gamedata-{}", std::process::id())),
        owner => owner.to_string(),
    };
    let settings = Settings::new(owner.clone(), args.chunk_bytes, Duration::from_secs(args.recheck_secs.max(1)))?;
    let nodes: Vec<String> = args.valkey.iter().map(|node| node.trim()).filter(|node| !node.is_empty()).map(node_url).collect();
    if nodes.is_empty() {
        return Err("no valkey nodes".into());
    }
    let (health, health_service) = tonic_health::server::health_reporter();
    let cache = Cache::new(args.work_dir.clone(), args.builds, settings, health).await?;
    info!(
        listen = %args.listen, work = %args.work_dir.display(), registry = args.registry, repository = args.repository,
        builds = ?cache.builds, owner, "game data listening",
    );
    tokio::spawn(connect(cache.clone(), nodes, args.valkey_cluster));
    tokio::spawn({
        let cache = cache.clone();
        async move { cache.run(registry).await }
    });
    Server::builder()
        .add_service(health_service)
        .add_service(GameDataServer::from_arc(cache))
        .serve(args.listen)
        .await
        .map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
