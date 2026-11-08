//! Prepares what the services and the game need. Kept out of the service workspace because it reads
//! the game archive through the EditorSC crates, which the container images do not need.

mod accounts;
mod client_files;
mod containers;
mod content;
mod destinations;
mod pki;
mod seed;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::{Parser, Subcommand};
use sc_runtime::{destinations as stored, state::State};

/// Not 127.0.0.1: the client's endpoint override only applies when it changes the default
/// "127.0.0.1" host (ParseEndpointOverride / ApplyEndpoints), and it is ignored otherwise.
const DEFAULT_HOST: &str = "127.0.0.2";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Directory for the CA, keys, account state and destinations.
    #[arg(long, env = "SCBACKEND_DATA", default_value = "data", global = true)]
    data: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or update state.json, the certificates and destinations.json. The shop's items are not written here:
    /// they travel in the seed bundle (seed-export, then sc-seeder or `seed`).
    Init {
        /// Address the game uses to reach the backend; becomes a certificate name.
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
        #[arg(long, default_value = "pilot")]
        handle: String,
        /// Game archive to read the star systems from.
        #[arg(long, env = "SCBACKEND_P4K")]
        p4k: Option<PathBuf>,
    },
    /// Write LoginData.json and client.crt for a game directory.
    ClientFiles {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "pilot")]
        handle: String,
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
        #[arg(long, default_value_t = 50051)]
        grpc_port: u16,
        #[arg(long, default_value_t = 8000)]
        hub_port: u16,
    },
    /// Import game content: seed-export and the apply of sc-seeder in one go, from the archive or from a bundle. The
    /// entity classes and the container tree go to each region's sc-entitygraph, and every other table to the service
    /// that owns it, each through its seed RPC. Safe to repeat, and needs the services to be running.
    Seed {
        /// A region's sc-entitygraph. Repeat it, or separate with commas, for every region.
        #[arg(long, env = "SCBACKEND_ENTITYGRAPH", required = true, value_delimiter = ',')]
        entitygraph: Vec<String>,
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
        /// The game archive to read.
        #[arg(long, env = "SCBACKEND_P4K", required_unless_present = "bundle", conflicts_with = "bundle")]
        p4k: Option<PathBuf>,
        /// A seed bundle (`<out>/<build>` of seed-export) to apply instead of reading the archive.
        #[arg(long)]
        bundle: Option<PathBuf>,
        /// JSON price list for rentals, which the game data lacks, with --p4k:
        /// `[{"id": "<class guid>", "offerings": [{"days": 1, "price": 100}]}]`.
        #[arg(long, conflicts_with = "bundle")]
        rental_prices: Option<PathBuf>,
    },
    /// Write the seed bundle of the archive's build to `<out>/<build>/`: every row `seed` hands over, as files with a
    /// manifest, for scripts/seed-publish.sh to push and sc-seeder to apply. The same archive gives the same bundle.
    SeedExport {
        p4k: PathBuf,
        out: PathBuf,
        /// The build; default the change number of `build_manifest.id` beside the archive.
        #[arg(long)]
        build: Option<String>,
        /// JSON price list for rentals, as for `seed`. Without it the bundle has none.
        #[arg(long)]
        rental_prices: Option<PathBuf>,
    },
    /// Import only the object container tree, which `seed` also does, to each region's sc-entitygraph.
    SeedContainers {
        /// A region's sc-entitygraph. Repeat it, or separate with commas, for every region.
        #[arg(long, env = "SCBACKEND_ENTITYGRAPH", required = true, value_delimiter = ',')]
        entitygraph: Vec<String>,
        #[arg(long, env = "SCBACKEND_P4K")]
        p4k: PathBuf,
    },
    /// Give a web account a role, such as `admin`, through sc-identity. Bootstraps the first admin.
    GrantRole {
        handle: String,
        role: String,
        /// sc-identity; scripts/identity-admin.sh port-forwards it here.
        #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://127.0.0.1:15050")]
        identity: String,
    },
    /// Take a role from a web account.
    RevokeRole {
        handle: String,
        role: String,
        #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://127.0.0.1:15050")]
        identity: String,
    },
    /// Set a web account's password, read as one line from standard input, and end its web sessions.
    SetPassword {
        handle: String,
        #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://127.0.0.1:15050")]
        identity: String,
    },
    /// Write the protobuf descriptor set, for tooling.
    DumpDescriptors {
        #[arg(long)]
        out: PathBuf,
    },
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("runtime: {e}"))
}

/// The rental price list, checked, or none.
fn read_prices(path: Option<&Path>) -> Result<Vec<sc_api::internal::RentalPrice>, String> {
    match path {
        Some(path) => seed::rental_prices(&std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?),
        None => Ok(Vec::new()),
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::GrantRole { handle, role, identity } => {
            runtime()?.block_on(accounts::set_role(&identity, &handle, &role, true))
        }
        Command::RevokeRole { handle, role, identity } => {
            runtime()?.block_on(accounts::set_role(&identity, &handle, &role, false))
        }
        Command::SetPassword { handle, identity } => {
            let mut password = String::new();
            std::io::stdin().read_line(&mut password).map_err(|e| format!("read password: {e}"))?;
            let password = password.trim_end_matches(['\r', '\n']).to_string();
            runtime()?.block_on(accounts::set_password(&identity, &handle, password))
        }
        Command::DumpDescriptors { out } => {
            std::fs::write(&out, sc_proto::DESCRIPTORS).map_err(|e| format!("write {}: {e}", out.display()))
        }
        Command::Seed {
            entitygraph,
            entitlement,
            reputation,
            blueprint_library,
            mission_scenario,
            contract_broker,
            shop,
            p4k,
            bundle,
            rental_prices,
        } => {
            let content = match (p4k, bundle) {
                (Some(p4k), _) => seed::read(&content::Game::open(&p4k)?, read_prices(rental_prices.as_deref())?)?,
                (None, Some(dir)) => {
                    let bundle = sc_seed::Bundle::open(&dir, None)?;
                    println!("bundle of build {}", bundle.manifest.build);
                    bundle.content()?
                }
                (None, None) => return Err("give --p4k or --bundle".into()),
            };
            let owners = sc_seed::Owners { entitlement, reputation, blueprint_library, mission_scenario, contract_broker, shop };
            let regions: Vec<(String, String)> = entitygraph.into_iter().map(|address| (address.clone(), address)).collect();
            runtime()?.block_on(sc_seed::apply(content, &regions, &owners, Duration::ZERO))
        }
        Command::SeedExport { p4k, out, build, rental_prices } => {
            let build = match build {
                Some(build) => build,
                None => seed::build_of(&p4k)?,
            };
            let content = seed::read(&content::Game::open(&p4k)?, read_prices(rental_prices.as_deref())?)?;
            let manifest = sc_seed::bundle::write(&out, &build, &content)?;
            let bytes: u64 = manifest.files.iter().map(|file| file.size).sum();
            println!("seed bundle of build {build} in {}: {} files, {bytes} bytes", out.join(&build).display(), manifest.files.len());
            Ok(())
        }
        Command::SeedContainers { entitygraph, p4k } => {
            let tree = content::Game::open(&p4k)?.universe()?;
            let nodes: Vec<_> = tree.nodes.iter().map(seed::container).collect();
            runtime()?.block_on(async {
                for address in &entitygraph {
                    println!("{address}:");
                    sc_seed::apply::region_tree(address, &nodes, Duration::ZERO).await?;
                }
                Ok(())
            })
        }
        Command::Init { host, handle, p4k } => {
            let state = State::open(&cli.data)?;
            state.ensure_account(&handle)?;
            pki::ensure(&cli.data, &host)?;
            if let Some(path) = p4k {
                let list = destinations::from_p4k(&path)?;
                stored::save(&cli.data, &list)?;
                println!("{} destinations read from {}", list.len(), path.display());
            }
            println!("state ready in {}", cli.data.display());
            Ok(())
        }
        Command::ClientFiles { out, handle, host, grpc_port, hub_port } => {
            let state = State::open(&cli.data)?;
            pki::ensure(&cli.data, &host)?;
            client_files::write(&out, &cli.data, &state, &handle, &host, grpc_port, hub_port)?;
            println!("client files written to {}", out.display());
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sc-admin: {message}");
            ExitCode::FAILURE
        }
    }
}
