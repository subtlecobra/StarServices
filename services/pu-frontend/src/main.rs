//! The PU frontend, the replicant: the UDP endpoint clients join. sc-shard-operator runs one per shard. It keeps the
//! protocol, and the DGSs of its shard decide what entities are. How meshing and instancing work is decided later.

mod batches;
mod channel;
mod curve;
mod dgs;
mod input;
mod link;
mod sessions;
mod shard;

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use sc_runtime::CommonArgs;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:64090")]
    listen: SocketAddr,
    /// Address for the HTTP endpoint that reports the player count to the operator.
    #[arg(long, env = "SCBACKEND_STATUS")]
    status: Option<SocketAddr>,
    /// This shard's id.
    #[arg(long, env = "SCBACKEND_SHARD_ID", default_value = "local")]
    shard_id: String,
    /// This shard's name, which the game receives alongside its id.
    #[arg(long, env = "SCBACKEND_SHARD_NAME", default_value = "local")]
    shard_name: String,
    /// The DGSs of this shard's `ShardTemplate`, space separated, each `<system>:<name>=<root>,<root>`.
    #[arg(long, env = "SCBACKEND_DGS_AREAS", default_value = "")]
    dgs_areas: String,
    /// How to reach a DGS, with `{name}` for its name. Empty on a shard whose DGSs have no Service.
    #[arg(long, env = "SCBACKEND_DGS_ADDRESS", default_value = "")]
    dgs_address: String,
    /// sc-hub's internal gRPC, which says whose a client's session is. Shards run in namespaces of their own.
    #[arg(long, env = "SCBACKEND_HUB", default_value = "http://sc-hub-internal.scbackend.svc:50051")]
    hub: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, shard::serve(args.listen, args.status, args.shard_id, args.shard_name, args.dgs_areas, args.dgs_address, args.hub))
}
