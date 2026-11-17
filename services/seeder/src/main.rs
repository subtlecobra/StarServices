//! sc-seeder: applies the seed bundle of a game build, as a Kubernetes Job (the `sc-seed` chart), so that a cluster
//! seeds itself with no game archive. It pulls `<registry>/<repository>:<build>` (files only, under `/seed/<build>/`,
//! pushed by scripts/seed-publish.sh) over the OCI distribution API, checks the image's digests and every file against
//! the bundle's manifest, then hands the rows to their owners exactly as `sc-admin seed` does (crates/sc-seed): the
//! entity classes and the container tree to the `sc-entitygraph` of every ShardRegion, the other tables once to their
//! global owners. Every owner upserts, so running it again changes nothing. Exits 0 once everything is applied.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

use clap::Parser;
use kube::{
    api::{ApiResource, DynamicObject, GroupVersionKind, ListParams},
    Api, Client,
};
use sc_oci::Registry;
use sc_seed::{bundle::valid_build, Bundle, Owners};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const POLL: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about)]
struct Args {
    /// The game build whose bundle to pull, such as `12913470`.
    #[arg(long, env = "SC_SEED_BUILD", required_unless_present = "bundle")]
    build: Option<String>,
    /// `host:port` of the registry with the seed bundles.
    #[arg(long, env = "SC_SEED_REGISTRY", required_unless_present = "bundle")]
    registry: Option<String>,
    #[arg(long, env = "SC_SEED_REPOSITORY", default_value = "scbackend/seed")]
    repository: String,
    /// Talk to the registry over HTTP instead of HTTPS.
    #[arg(long, env = "SC_SEED_PLAIN_HTTP", default_value_t = false, action = clap::ArgAction::Set)]
    plain_http: bool,
    /// Scratch space for the pull, removed at the end.
    #[arg(long, env = "SC_SEED_WORK_DIR", default_value = "/work")]
    work_dir: PathBuf,
    /// A bundle directory (`<out>/<build>` of `sc-admin seed-export`) to apply instead of pulling one.
    #[arg(long, env = "SC_SEED_BUNDLE", conflicts_with = "registry")]
    bundle: Option<PathBuf>,
    /// The ShardRegions to seed. Default: every one, as the Kubernetes API lists them.
    #[arg(long, env = "SC_SEED_REGIONS", value_delimiter = ',')]
    regions: Vec<String>,
    /// `name=address` of each region's sc-entitygraph, instead of asking the Kubernetes API for the ShardRegions.
    #[arg(long, env = "SC_SEED_ENTITYGRAPH", value_delimiter = ',', conflicts_with = "regions")]
    entitygraph: Vec<String>,
    /// How long to wait for every region's entity database to be Ready and for each owner to answer, in seconds.
    #[arg(long, env = "SC_SEED_WAIT_SECS", default_value_t = 1800)]
    wait_secs: u64,
    /// sc-entitlement, for the starter entitlements.
    #[arg(long, env = "SCBACKEND_ENTITLEMENT", default_value = "http://sc-entitlement:50051")]
    entitlement: String,
    /// sc-reputation, for the scopes and standings.
    #[arg(long, env = "SCBACKEND_REPUTATION", default_value = "http://sc-reputation:50051")]
    reputation: String,
    /// sc-blueprint-library, for the default blueprints.
    #[arg(long, env = "SCBACKEND_BLUEPRINT_LIBRARY", default_value = "http://sc-blueprint-library:50051")]
    blueprint_library: String,
    /// sc-mission-scenario, for the scenarios.
    #[arg(long, env = "SCBACKEND_MISSION_SCENARIO", default_value = "http://sc-mission-scenario:50051")]
    mission_scenario: String,
    /// sc-contract-broker, for the mission contracts.
    #[arg(long, env = "SCBACKEND_CONTRACT_BROKER", default_value = "http://sc-contract-broker:50051")]
    contract_broker: String,
    /// sc-solar-system-shop, for the shop items and rental prices.
    #[arg(long, env = "SCBACKEND_SHOP", default_value = "http://sc-solar-system-shop:50051")]
    shop: String,
}

/// Where a region's `sc-entitygraph` answers when its status does not say: the shard operator's naming.
fn default_entitygraph(region: &str) -> String {
    format!("http://sc-entitygraph.scbackend-region-{region}.svc:50051")
}

/// `name=address`, or an address alone, which names itself.
fn named(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((name, address)) => (name.trim().to_string(), address.trim().to_string()),
        None => (entry.trim().to_string(), entry.trim().to_string()),
    }
}

/// A region's sc-entitygraph once its entity database is Ready, else what it waits for.
fn region_target(region: &DynamicObject) -> Result<String, String> {
    let name = region.metadata.name.clone().unwrap_or_default();
    let status = &region.data["status"]["entityDb"];
    match status["state"].as_str() {
        Some("Ready") => Ok(status["service"].as_str().filter(|s| !s.is_empty()).map_or_else(|| default_entitygraph(&name), str::to_string)),
        state => Err(format!("{name}: entity database {} {}", state.unwrap_or("without status"), status["message"].as_str().unwrap_or(""))),
    }
}

/// Picks the regions to seed out of those listed: all of them, or the `wanted` ones, which must all exist.
fn pick(listed: Vec<DynamicObject>, wanted: &[String]) -> Result<Vec<DynamicObject>, String> {
    let name = |region: &DynamicObject| region.metadata.name.clone().unwrap_or_default();
    if wanted.is_empty() {
        return Ok(listed);
    }
    let missing: Vec<&String> = wanted.iter().filter(|w| !listed.iter().any(|r| &name(r) == *w)).collect();
    if !missing.is_empty() {
        return Err(format!("no ShardRegion {missing:?}"));
    }
    Ok(listed.into_iter().filter(|r| wanted.contains(&name(r))).collect())
}

/// Every ShardRegion to seed and its sc-entitygraph, once all their entity databases are Ready.
async fn regions(wanted: &[String], wait: Duration) -> Result<Vec<(String, String)>, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = Client::try_default().await.map_err(|e| format!("kubernetes client: {e}"))?;
    let resource = ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk("scbackend.io", "v1alpha1", "ShardRegion"), "shardregions");
    let api: Api<DynamicObject> = Api::all_with(client, &resource);
    let deadline = Instant::now() + wait;
    loop {
        let listed = api.list(&ListParams::default()).await.map_err(|e| format!("list ShardRegions: {e}"))?.items;
        let picked = pick(listed, wanted)?;
        if picked.is_empty() {
            return Err("no ShardRegion to seed: deploy sc-shard-operator and its regions first".into());
        }
        let targets: Vec<Result<(String, String), String>> = picked
            .iter()
            .map(|region| region_target(region).map(|address| (region.metadata.name.clone().unwrap_or_default(), address)))
            .collect();
        let waiting: Vec<&String> = targets.iter().filter_map(|target| target.as_ref().err()).collect();
        if waiting.is_empty() {
            return Ok(targets.into_iter().filter_map(Result::ok).collect());
        }
        if Instant::now() + POLL >= deadline {
            return Err(format!("regions not ready: {waiting:?}"));
        }
        info!(?waiting, "waiting for the regions' entity databases");
        tokio::time::sleep(POLL).await;
    }
}

async fn run(args: Args) -> Result<(), String> {
    let wait = Duration::from_secs(args.wait_secs);
    let owners = Owners {
        entitlement: args.entitlement,
        reputation: args.reputation,
        blueprint_library: args.blueprint_library,
        mission_scenario: args.mission_scenario,
        contract_broker: args.contract_broker,
        shop: args.shop,
    };
    // Held until the end: the pulled files go with it.
    let mut pulled = None;
    let bundle = match (&args.bundle, &args.build, &args.registry) {
        (Some(dir), build, _) => Bundle::open(dir, build.as_deref())?,
        (None, Some(build), Some(registry)) => {
            if !valid_build(build) {
                return Err(format!("{build:?} is not a build"));
            }
            let client = Registry::new(registry, &args.repository, args.plain_http)?;
            let work = args.work_dir.join(format!(".pull-{build}"));
            let image = sc_oci::pull(&client, build, &work, &Path::new("seed").join(build)).await?;
            info!(registry, repository = args.repository, build, digest = image.digest, "bundle pulled");
            let bundle = Bundle::open(&image.root, Some(build))?;
            pulled = Some(image);
            bundle
        }
        _ => return Err("give --build and --registry, or --bundle".into()),
    };
    let rows: Vec<String> = bundle.manifest.files.iter().map(|file| format!("{} {}", file.path, file.rows)).collect();
    info!(build = bundle.manifest.build, ?rows, "bundle checked");
    let content = bundle.content()?;
    let regions = match args.entitygraph.is_empty() {
        false => args.entitygraph.iter().map(|entry| named(entry)).collect(),
        true => regions(&args.regions, wait).await?,
    };
    info!(?regions, "seeding");
    sc_seed::apply(content, &regions, &owners, wait).await?;
    drop(pulled);
    info!(build = bundle.manifest.build, "seeded");
    Ok(())
}

/// Not sc_runtime::run, which ends a service with success on SIGTERM: a Job stopped half way must fail, so that it runs
/// again.
fn main() -> ExitCode {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let result = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}")).and_then(|runtime| runtime.block_on(run(args)));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            error!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
