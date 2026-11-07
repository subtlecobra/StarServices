//! sc.external.services.trace.v1.TraceService.
//!
//! The client streams trace lines without end, so lines are only counted in memory and the counts are added
//! to one row per minute. The lines themselves are not kept.

use std::{
    net::SocketAddr,
    process::ExitCode,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::Parser;
use sc_db::{DatabaseArgs, Postgres};
use sc_proto::services::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    CollectRequest, CollectResponse, CollectionStreamRequest, CollectionStreamResponse,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status, Streaming};
use tracing::{info, warn};

const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS trace_counts (
    minute TIMESTAMPTZ PRIMARY KEY,
    events BIGINT NOT NULL,
    bytes BIGINT NOT NULL
);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

#[derive(Default)]
struct Counts {
    events: AtomicI64,
    bytes: AtomicI64,
}

impl Counts {
    fn add(&self, events: i64, bytes: i64) {
        self.events.fetch_add(events, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn record(&self, events: &[String]) {
        self.add(events.len() as i64, events.iter().map(|event| event.len() as i64).sum());
    }

    fn take(&self) -> (i64, i64) {
        (self.events.swap(0, Ordering::Relaxed), self.bytes.swap(0, Ordering::Relaxed))
    }
}

async fn flush(db: Postgres, counts: Arc<Counts>) {
    loop {
        tokio::time::sleep(FLUSH_INTERVAL).await;
        let (events, bytes) = counts.take();
        if events == 0 {
            continue;
        }
        let saved = db
            .execute(
                "INSERT INTO trace_counts (minute, events, bytes) VALUES (date_trunc('minute', now()), $1, $2)
                 ON CONFLICT (minute) DO UPDATE SET events = trace_counts.events + excluded.events,
                     bytes = trace_counts.bytes + excluded.bytes",
                &[&events, &bytes],
            )
            .await;
        if let Err(error) = saved {
            warn!(%error, "trace counts not saved");
            counts.add(events, bytes);
        }
    }
}

struct Traces {
    counts: Arc<Counts>,
}

#[tonic::async_trait]
impl TraceService for Traces {
    async fn collect(&self, request: Request<CollectRequest>) -> Result<Response<CollectResponse>, Status> {
        self.counts.record(&request.into_inner().traces);
        Ok(Response::new(CollectResponse::default()))
    }

    async fn collection_stream(
        &self,
        request: Request<Streaming<CollectionStreamRequest>>,
    ) -> Result<Response<CollectionStreamResponse>, Status> {
        let mut stream = request.into_inner();
        while let Some(message) = stream.message().await? {
            self.counts.record(&message.traces);
        }
        Ok(Response::new(CollectionStreamResponse::default()))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let counts = Arc::new(Counts::default());
    tokio::spawn(flush(db, counts.clone()));
    let server = TraceServiceServer::new(Traces { counts })
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "trace listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_events_and_bytes_until_taken() {
        let counts = Counts::default();
        counts.record(&["ab".to_string(), "cde".to_string()]);
        assert_eq!(counts.take(), (2, 5));
        assert_eq!(counts.take(), (0, 0));
    }
}
