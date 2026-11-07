//! What every backend service shares: options, logging, health, shutdown, tokens and TLS.

pub mod query;
pub mod destinations;
pub mod state;
pub mod tls;

use std::{future::Future, net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Args;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    signal::unix::{signal, SignalKind},
};
use tonic::metadata::MetadataMap;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use state::{Account, State};

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Args, Clone, Debug)]
pub struct CommonArgs {
    /// Directory with state.json, certificates and destinations.json.
    #[arg(long, env = "SCBACKEND_DATA", default_value = "data")]
    pub data: PathBuf,
    /// Address for the HTTP health endpoint used by Kubernetes probes.
    #[arg(long, env = "SCBACKEND_HEALTH")]
    pub health: Option<SocketAddr>,
}

/// Runs a service until it fails or receives SIGTERM or SIGINT, which matters when it is PID 1.
pub fn run<F>(health: Option<SocketAddr>, service: F) -> ExitCode
where
    F: Future<Output = Result<(), String>>,
{
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        if let Some(address) = health {
            tokio::spawn(serve_health(address));
        }
        tokio::select! {
            result = service => result,
            result = shutdown_signal() => {
                info!("shutting down");
                result
            }
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            error!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() -> Result<(), String> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM handler: {e}"))?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT handler: {e}"))?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

async fn serve_health(address: SocketAddr) {
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            warn!(%address, %error, "health endpoint not available");
            return;
        }
    };
    while let Ok((mut socket, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut request = [0u8; 512];
            let _ = socket.read(&mut request).await;
            let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
            let _ = socket.write_all(reply).await;
        });
    }
}

/// The account of the call's bearer token, if it has a valid one.
/// The player URN in the client's format.
pub fn player_urn(geid: u64) -> String {
    format!("urn:sc:global:player:geid:{geid}")
}

pub fn caller(state: &State, metadata: &MetadataMap) -> Option<Account> {
    let header = metadata.get("authorization")?.to_str().ok()?;
    state.verify_token(header.strip_prefix("Bearer ")?)
}
