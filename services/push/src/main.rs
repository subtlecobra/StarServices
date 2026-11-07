//! sc.external.services.push.v1.PushService, plus the internal publisher other services call.

mod bus;

use std::{net::SocketAddr, pin::Pin, process::ExitCode, sync::Arc, time::Duration};

use bus::PushBus;
use clap::Parser;
use futures_util::{Stream, StreamExt};
use prost::Message;
use sc_proto::{
    internal::{
        push_publisher_client::PushPublisherClient,
        push_publisher_server::{PushPublisher, PushPublisherServer},
        PublishRequest, PublishResponse,
    },
    push::{
        push_service_server::{PushService, PushServiceServer},
        ListenRequest, ListenResponse,
    },
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status, Streaming};
use tokio::{
    net::lookup_host,
    time::{sleep, Instant},
};
use tracing::{info, warn};

const SUBSCRIBER_RETRY: Duration = Duration::from_millis(100);

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// `host:port` that resolves to every replica, from a headless Service. Without it this replica
    /// only reaches its own streams.
    #[arg(long, env = "SCBACKEND_PUSH_PEERS")]
    peers: Option<String>,
}

#[derive(Clone)]
struct Push {
    state: Arc<State>,
    bus: Arc<PushBus>,
    peers: Option<String>,
}

impl Push {
    async fn deliver(&self, request: &PublishRequest) -> u32 {
        let value = request.value.clone().into();
        self.bus.publish(request.account_id, &request.topic, &request.type_name, value).await as u32
    }

    async fn deliver_everywhere(&self, request: &PublishRequest) -> Result<u32, Status> {
        match &self.peers {
            Some(peers) => self.fan_out(peers, request).await,
            None => Ok(self.deliver(request).await),
        }
    }

    /// Calls every replica at once. A stream lives on one replica, and only that one knows about it.
    async fn fan_out(&self, peers: &str, request: &PublishRequest) -> Result<u32, Status> {
        let addresses = lookup_host(peers).await.map_err(|e| Status::unavailable(format!("{peers}: {e}")))?;
        let calls: Vec<_> = addresses
            .map(|address| {
                let request = request.clone();
                tokio::spawn(async move {
                    let mut peer = PushPublisherClient::connect(format!("http://{address}")).await.ok()?;
                    Some(peer.publish_local(request).await.ok()?.into_inner().delivered)
                })
            })
            .collect();
        let mut delivered = 0;
        for call in calls {
            match call.await {
                Ok(Some(count)) => delivered += count,
                _ => warn!("a push replica did not answer"),
            }
        }
        Ok(delivered)
    }
}

#[tonic::async_trait]
impl PushService for Push {
    type ListenStream = Pin<Box<dyn Stream<Item = Result<ListenResponse, Status>> + Send>>;

    async fn listen(
        &self,
        request: Request<Streaming<ListenRequest>>,
    ) -> Result<Response<Self::ListenStream>, Status> {
        let account = caller(&self.state, request.metadata()).map(|a| a.account_id);
        let mut incoming = request.into_inner();
        let bus = self.bus.clone();
        let state = self.state.clone();
        let (id, rx) = bus.register(account).await;
        info!(stream = id, ?account, "push stream opened");
        tokio::spawn(async move {
            while let Some(Ok(request)) = incoming.next().await {
                let verify_key = |key: &str| state.verify_token(key).map(|account| account.account_id);
                let added = bus.handle_request(id, &request.encode_to_vec(), verify_key).await;
                if !added.is_empty() {
                    info!(stream = id, topics = ?added, "push subscribe");
                }
            }
            bus.unregister(id);
            info!(stream = id, "push stream closed");
        });
        let stream = ReceiverStream::new(rx).map(|bytes| {
            ListenResponse::decode(bytes).map_err(|e| Status::internal(e.to_string()))
        });
        Ok(Response::new(Box::pin(stream)))
    }
}

#[tonic::async_trait]
impl PushPublisher for Push {
    /// Tries at once, and only retries while nobody has subscribed yet. Waiting on each replica
    /// instead would hold every publish up on the replicas without the player.
    async fn publish(&self, request: Request<PublishRequest>) -> Result<Response<PublishResponse>, Status> {
        let request = request.into_inner();
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.wait_for_subscriber_ms));
        loop {
            let delivered = self.deliver_everywhere(&request).await?;
            if delivered > 0 || Instant::now() >= deadline {
                if delivered == 0 {
                    warn!(topic = %request.topic, "nobody subscribed");
                }
                return Ok(Response::new(PublishResponse { delivered }));
            }
            sleep(SUBSCRIBER_RETRY).await;
        }
    }

    async fn publish_local(&self, request: Request<PublishRequest>) -> Result<Response<PublishResponse>, Status> {
        Ok(Response::new(PublishResponse { delivered: self.deliver(request.get_ref()).await }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let push = Push { state: Arc::new(State::load(&args.common.data)?), bus: Arc::default(), peers: args.peers };
    let client_facing = PushServiceServer::new(push.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "push listening");
    Server::builder()
        .add_service(client_facing)
        .add_service(PushPublisherServer::new(push))
        .serve(args.listen)
        .await
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
