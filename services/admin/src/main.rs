//! sc-admin-web: the admin site, served by the portal's site under `/admin`.
//!
//! Admins look up accounts and act on what each owner service keeps for them: money (sc-ledger), entitlements
//! (sc-entitlement), rentals (sc-rental), home and spawn locations (sc-entity-document) and inventories
//! (sc-entitygraph). This service keeps none of that data. It owns only `admin_audit`, the record of every
//! action. Sessions and roles come from sc-identity; see web.rs for the guard.

mod audit;
mod backend;
mod catalog;
mod html;
mod places;
mod web;

#[cfg(test)]
mod tests;

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::DatabaseArgs;
use sc_runtime::CommonArgs;
use tracing::{info, warn};

use audit::PostgresAudit;
use backend::{Addresses, Grpc};
use web::App;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// The path the site is served under.
    #[arg(long, env = "SCBACKEND_ADMIN_PREFIX", default_value = "/admin")]
    prefix: String,
    /// The portal's login page, where a request without a session goes.
    #[arg(long, env = "SCBACKEND_LOGIN_PATH", default_value = "/login")]
    login: String,
    /// Marks the CSRF cookie Secure, for a site served over HTTPS.
    #[arg(long, env = "SCBACKEND_SECURE_COOKIES", default_value_t = false)]
    secure_cookies: bool,
    #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://sc-identity:50051")]
    identity: String,
    #[arg(long, env = "SCBACKEND_LEDGER", default_value = "http://sc-ledger:50051")]
    ledger: String,
    #[arg(long, env = "SCBACKEND_ENTITLEMENT", default_value = "http://sc-entitlement:50051")]
    entitlement: String,
    #[arg(long, env = "SCBACKEND_RENTAL", default_value = "http://sc-rental:50051")]
    rental: String,
    #[arg(long, env = "SCBACKEND_ENTITY_DOCUMENT", default_value = "http://sc-entity-document:50051")]
    entity_document: String,
    /// `HubInternal`, on the Hub's internal Service.
    #[arg(long, env = "SCBACKEND_HUB", default_value = "http://sc-hub-internal:50051")]
    hub: String,
    #[arg(long, env = "SCBACKEND_PRESENCE", default_value = "http://sc-presence:50051")]
    presence: String,
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH", default_value = "http://sc-entitygraph:50051")]
    entitygraph: String,
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
    /// The `sc-gamedata` of the region that takes the entity calls, for the home locations of the game build. With
    /// `SC_GAME_BUILD`; without either the site lists no home locations.
    #[arg(long, env = "SC_GAMEDATA_ADDR")]
    gamedata: Option<String>,
    /// The game build the environment runs, its change number, e.g. `12913470`.
    #[arg(long, env = "SC_GAME_BUILD")]
    game_build: Option<String>,
}

async fn serve(args: Args) -> Result<(), String> {
    let prefix = args.prefix.trim_end_matches('/').to_string();
    if !prefix.starts_with('/') {
        return Err(format!("--prefix {prefix}: must start with /"));
    }
    let set = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    match (set(args.gamedata), set(args.game_build)) {
        (Some(address), Some(build)) => {
            info!(%address, build, "home locations from gamedata");
            tokio::spawn(places::keep(address, build));
        }
        _ => warn!("no SC_GAMEDATA_ADDR or SC_GAME_BUILD: no home locations to list or set"),
    }
    let backend = Arc::new(Grpc::connect(&Addresses {
        identity: args.identity,
        ledger: args.ledger,
        entitlement: args.entitlement,
        rental: args.rental,
        entity_document: args.entity_document,
        hub: args.hub,
        presence: args.presence,
        entitygraph: args.entitygraph,
        push: args.push,
    })?);
    let audit = Arc::new(PostgresAudit::connect(&args.database.postgres).await?);
    let app = Arc::new(App {
        identity: backend.clone(),
        owners: backend,
        audit,
        prefix,
        login: args.login,
        secure_cookies: args.secure_cookies,
    });
    let listener = tokio::net::TcpListener::bind(args.listen).await.map_err(|e| format!("bind {}: {e}", args.listen))?;
    info!(listen = %args.listen, prefix = %app.prefix, "admin site listening");
    axum::serve(listener, web::router(app)).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
