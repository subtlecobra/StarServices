//! sc.external.services.configuration.v1.ConfigService.
//!
//! Settings live in the `config_values` table, which starts empty so the client keeps its defaults.
//! That includes the channel table a ("grpc", "endpoints") value would replace.

use std::{net::SocketAddr, pin::Pin, process::ExitCode, time::Duration};

use clap::Parser;
use futures_util::{stream, Stream};
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::config::{
    config_response,
    config_service_server::{ConfigService, ConfigServiceServer},
    ConfigQuery, ConfigResponse, QueryConfigRequest, QueryConfigResponse, WatchConfigRequest, WatchConfigResponse,
    WatchMultiConfigurationsRequest, WatchMultiConfigurationsResponse, WatchMultiGroupKeyPair,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const WATCH_PERIOD: Duration = Duration::from_secs(5);
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS config_values (
    config_group TEXT NOT NULL,
    config_key TEXT NOT NULL,
    value TEXT NOT NULL,
    node TEXT NOT NULL DEFAULT '',
    version BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (config_group, config_key)
);";

type Watch<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Config {
    db: Postgres,
}

async fn lookup(db: &Postgres, group: &str, key: &str) -> Result<ConfigResponse, String> {
    let row = db
        .query_opt(
            "SELECT value, node, version FROM config_values WHERE config_group = $1 AND config_key = $2",
            &[&group, &key],
        )
        .await?;
    let base = ConfigResponse { config_group: group.to_string(), config_key: key.to_string(), ..Default::default() };
    Ok(match row {
        Some(row) => ConfigResponse {
            result: config_response::Result::Found as i32,
            value: row.get("value"),
            curr_node: row.get("node"),
            version: row.get::<_, i64>("version") as u64,
            ..base
        },
        None => ConfigResponse { result: config_response::Result::NotFound as i32, ..base },
    })
}

/// Yields each key's response whenever its version moves past the one last seen.
fn watch(db: Postgres, mut keys: Vec<(String, String, u64)>) -> impl Stream<Item = Result<ConfigResponse, Status>> {
    stream::unfold((db, Vec::<ConfigResponse>::new()), move |(db, mut pending)| {
        let mut keys = std::mem::take(&mut keys);
        async move {
            loop {
                if let Some(next) = pending.pop() {
                    return Some((Ok(next), (db, pending)));
                }
                for (group, key, seen) in &mut keys {
                    match lookup(&db, group, key).await {
                        Ok(response) if response.version > *seen => {
                            *seen = response.version;
                            pending.push(response);
                        }
                        Ok(_) => {}
                        Err(error) => return Some((Err(internal(error)), (db, pending))),
                    }
                }
                if pending.is_empty() {
                    tokio::time::sleep(WATCH_PERIOD).await;
                }
            }
        }
    })
}

fn watched(query: ConfigQuery) -> (String, String, u64) {
    (query.config_group, query.config_key, query.curr_version.max(0) as u64)
}

#[tonic::async_trait]
impl ConfigService for Config {
    async fn query_config(
        &self,
        request: Request<QueryConfigRequest>,
    ) -> Result<Response<QueryConfigResponse>, Status> {
        let query = request.into_inner().query.unwrap_or_default();
        let response = lookup(&self.db, &query.config_group, &query.config_key).await.map_err(internal)?;
        Ok(Response::new(QueryConfigResponse { response: Some(response) }))
    }

    type WatchConfigStream = Watch<WatchConfigResponse>;

    async fn watch_config(
        &self,
        request: Request<WatchConfigRequest>,
    ) -> Result<Response<Self::WatchConfigStream>, Status> {
        let query = request.into_inner().request.unwrap_or_default();
        let updates = watch(self.db.clone(), vec![watched(query)]);
        let responses = futures_util::StreamExt::map(updates, |update| {
            update.map(|response| WatchConfigResponse { response: Some(response) })
        });
        Ok(Response::new(Box::pin(responses)))
    }

    type WatchMultiConfigurationsStream = Watch<WatchMultiConfigurationsResponse>;

    async fn watch_multi_configurations(
        &self,
        request: Request<WatchMultiConfigurationsRequest>,
    ) -> Result<Response<Self::WatchMultiConfigurationsStream>, Status> {
        let pairs = request.into_inner().request.unwrap_or_default().group_key_pairs;
        let keys = pairs
            .into_iter()
            .map(|WatchMultiGroupKeyPair { config_group, config_key }| (config_group, config_key, 0))
            .collect();
        let updates = watch(self.db.clone(), keys);
        let responses = futures_util::StreamExt::map(updates, |update| {
            update.map(|response| WatchMultiConfigurationsResponse { response: Some(response) })
        });
        Ok(Response::new(Box::pin(responses)))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let service = ConfigServiceServer::new(Config { db })
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "configuration listening");
    Server::builder().add_service(service).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
