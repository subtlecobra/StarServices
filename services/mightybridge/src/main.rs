//! sc.game.editor.mightybridge.v1.MightyBridgeService.
//!
//! In the real tools the editor hosts this bridge and no other code in the client calls it, so its routing
//! here is a guess: this binary relays between the streams of one account. A command goes to that account's
//! other streams, and a result goes back to the stream that sent the command with the same ticket. Every
//! message is logged. The relay lives in one process, so this service must run as one replica.

use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    process::ExitCode,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use clap::Parser;
use futures_util::Stream;
use sc_db::{DatabaseArgs, Postgres};
use sc_proto::sc::game::editor::mightybridge::v1::{
    bridge_message,
    mighty_bridge_service_server::{MightyBridgeService, MightyBridgeServiceServer},
    BridgeMessage, BridgeMessageResult, MightyStreamRequest, MightyStreamResponse,
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status, Streaming};
use tracing::{info, warn};

const STREAM_BUFFER: usize = 256;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mightybridge_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id BIGINT NOT NULL,
    ticket TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_type TEXT NOT NULL DEFAULT '',
    payload BYTEA,
    sent_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS mightybridge_messages_ticket ON mightybridge_messages (ticket);";

type Outgoing = mpsc::Sender<Result<MightyStreamResponse, Status>>;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Peer {
    account: u32,
    tx: Outgoing,
}

/// The open streams, and which stream is waiting for each ticket.
#[derive(Default)]
struct Routes {
    peers: HashMap<u64, Peer>,
    tickets: HashMap<String, u64>,
}

#[derive(Debug, PartialEq)]
enum Route {
    /// Deliver to these streams.
    To(Vec<u64>),
    /// A command nobody can run.
    NoPeer,
    /// A result nobody waits for.
    Drop,
}

impl Routes {
    fn route(&mut self, from: u64, message: &BridgeMessage) -> Route {
        match message.message {
            Some(bridge_message::Message::MessageResult(_)) => match self.tickets.get(&message.ticket) {
                Some(origin) if self.same_account(*origin, from) => {
                    let origin = *origin;
                    self.tickets.remove(&message.ticket);
                    Route::To(vec![origin])
                }
                _ => Route::Drop,
            },
            _ => {
                let Some(account) = self.peers.get(&from).map(|peer| peer.account) else {
                    return Route::Drop;
                };
                let others: Vec<u64> =
                    self.peers.iter().filter(|(id, peer)| **id != from && peer.account == account).map(|(id, _)| *id).collect();
                if others.is_empty() {
                    return Route::NoPeer;
                }
                self.tickets.insert(message.ticket.clone(), from);
                Route::To(others)
            }
        }
    }

    fn same_account(&self, left: u64, right: u64) -> bool {
        match (self.peers.get(&left), self.peers.get(&right)) {
            (Some(left), Some(right)) => left.account == right.account,
            _ => false,
        }
    }

    fn remove(&mut self, id: u64) {
        self.peers.remove(&id);
        self.tickets.retain(|_, origin| *origin != id);
    }
}

fn kind(message: &BridgeMessage) -> &'static str {
    match message.message {
        Some(bridge_message::Message::MessageResult(_)) => "result",
        Some(bridge_message::Message::MessageCommand(_)) => "command",
        None => "unknown",
    }
}

fn no_peer(ticket: String) -> BridgeMessage {
    let result = BridgeMessageResult { error: true, additional_info: "no peer connected".into() };
    BridgeMessage { ticket, payload: None, message: Some(bridge_message::Message::MessageResult(result)) }
}

struct Bridge {
    state: State,
    db: Postgres,
    routes: Arc<Mutex<Routes>>,
    next_id: AtomicU64,
}

async fn log(db: &Postgres, account: u32, message: &BridgeMessage) {
    let (payload_type, payload) = message
        .payload
        .as_ref()
        .map_or((String::new(), None), |any| (any.type_url.clone(), Some(any.value.clone())));
    let saved = db
        .execute(
            "INSERT INTO mightybridge_messages (account_id, ticket, kind, payload_type, payload)
             VALUES ($1, $2, $3, $4, $5)",
            &[&i64::from(account), &message.ticket, &kind(message), &payload_type, &payload],
        )
        .await;
    if let Err(error) = saved {
        warn!(%error, "bridge message not logged");
    }
}

async fn relay(id: u64, account: u32, mut incoming: Streaming<MightyStreamRequest>, routes: Arc<Mutex<Routes>>, db: Postgres, own: Outgoing) {
    while let Ok(Some(request)) = incoming.message().await {
        for message in request.messages {
            log(&db, account, &message).await;
            let route = routes.lock().unwrap().route(id, &message);
            match route {
                Route::To(targets) => {
                    let routes = routes.lock().unwrap();
                    for target in targets.iter().filter_map(|target| routes.peers.get(target)) {
                        let reply = MightyStreamResponse { messages: vec![message.clone()] };
                        // A tool that stops reading must not stall the others.
                        if target.tx.try_send(Ok(reply)).is_err() {
                            warn!(ticket = %message.ticket, "bridge peer is not keeping up");
                        }
                    }
                }
                Route::NoPeer => {
                    let reply = MightyStreamResponse { messages: vec![no_peer(message.ticket)] };
                    if own.send(Ok(reply)).await.is_err() {
                        break;
                    }
                }
                Route::Drop => warn!(ticket = %message.ticket, "bridge result for no one"),
            }
        }
    }
    routes.lock().unwrap().remove(id);
    info!(stream = id, "bridge stream closed");
}

#[tonic::async_trait]
impl MightyBridgeService for Bridge {
    type MightyStreamStream = Pin<Box<dyn Stream<Item = Result<MightyStreamResponse, Status>> + Send>>;

    async fn mighty_stream(
        &self,
        request: Request<Streaming<MightyStreamRequest>>,
    ) -> Result<Response<Self::MightyStreamStream>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?
            .account_id;
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.routes.lock().unwrap().peers.insert(id, Peer { account, tx: tx.clone() });
        info!(stream = id, account, "bridge stream opened");
        tokio::spawn(relay(id, account, request.into_inner(), self.routes.clone(), self.db.clone(), tx));
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Bridge {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        routes: Arc::default(),
        next_id: AtomicU64::new(1),
    };
    let server = MightyBridgeServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "mightybridge listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use sc_proto::sc::game::editor::mightybridge::v1::BridgeMessageCommand;

    use super::*;

    fn routes(peers: &[(u64, u32)]) -> Routes {
        let mut routes = Routes::default();
        for (id, account) in peers {
            routes.peers.insert(*id, Peer { account: *account, tx: mpsc::channel(1).0 });
        }
        routes
    }

    fn command(ticket: &str) -> BridgeMessage {
        BridgeMessage {
            ticket: ticket.into(),
            payload: None,
            message: Some(bridge_message::Message::MessageCommand(BridgeMessageCommand {})),
        }
    }

    fn result(ticket: &str) -> BridgeMessage {
        BridgeMessage {
            ticket: ticket.into(),
            payload: None,
            message: Some(bridge_message::Message::MessageResult(BridgeMessageResult::default())),
        }
    }

    #[test]
    fn commands_reach_only_the_same_account() {
        let mut routes = routes(&[(1, 10), (2, 10), (3, 20)]);
        assert_eq!(routes.route(1, &command("t")), Route::To(vec![2]));
        assert_eq!(routes.route(3, &command("u")), Route::NoPeer);
    }

    #[test]
    fn a_result_returns_once_to_the_origin() {
        let mut routes = routes(&[(1, 10), (2, 10)]);
        routes.route(1, &command("t"));
        assert_eq!(routes.route(2, &result("t")), Route::To(vec![1]));
        assert_eq!(routes.route(2, &result("t")), Route::Drop);
    }

    #[test]
    fn another_account_cannot_answer_a_ticket() {
        let mut routes = routes(&[(1, 10), (2, 10), (3, 20)]);
        routes.route(1, &command("t"));
        assert_eq!(routes.route(3, &result("t")), Route::Drop);
        assert_eq!(routes.route(2, &result("t")), Route::To(vec![1]));
    }

    #[test]
    fn leaving_forgets_the_stream_and_its_tickets() {
        let mut routes = routes(&[(1, 10), (2, 10)]);
        routes.route(1, &command("t"));
        routes.remove(1);
        assert_eq!(routes.route(2, &result("t")), Route::Drop);
        assert_eq!(routes.route(2, &command("u")), Route::NoPeer);
    }
}
