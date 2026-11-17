//! sc.external.services.echo.v1.EchoService, the test service behind the client's grpc_test_* commands.

use std::{
    future::{pending, Future},
    net::SocketAddr,
    pin::Pin,
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use futures_util::{Stream, StreamExt};
use prost::Message;
use sc_proto::{
    google::rpc::{bad_request::FieldViolation, BadRequest, Status as RichStatus},
    services::echo::v1::{
        echo_service_server::{EchoService, EchoServiceServer},
        CloseAllStreamsRequest, CloseAllStreamsResponse, EchoClientStreamRequest, EchoClientStreamResponse,
        EchoDuplexStreamRequest, EchoDuplexStreamResponse, EchoErrorRequest, EchoErrorResponse, EchoRequest,
        EchoResponse, EchoServerStreamRequest, EchoServerStreamResponse, EchoSimpleDuplexStreamRequest,
        EchoSimpleDuplexStreamResponse, RandomMessagesStreamSettings,
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tokio::{
    sync::{broadcast, mpsc},
    time::{sleep, sleep_until, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    codec::CompressionEncoding,
    metadata::{MetadataMap, MetadataValue},
    transport::Server,
    Code, Request, Response, Status, Streaming,
};
use tracing::info;

/// Delays come straight from the caller, so they are bounded to keep a call from pinning a task for days.
const MAX_DELAY_MS: u32 = 60_000;
const STREAM_BUFFER: usize = 32;
const BAD_REQUEST_TYPE: &str = "type.googleapis.com/google.rpc.BadRequest";

type Reply<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Echo {
    state: sc_runtime::state::State,
    /// Tells every open stream to end. The value is CloseAllStreamsRequest.cancel.
    close: broadcast::Sender<bool>,
}

fn delay(ms: u32) -> Duration {
    Duration::from_millis(u64::from(ms.min(MAX_DELAY_MS)))
}

impl Echo {
    /// Runs `body` as a response stream that CloseAllStreams can end.
    fn stream<T, F, B>(&self, body: B) -> Reply<T>
    where
        T: Send + 'static,
        F: Future<Output = ()> + Send + 'static,
        B: FnOnce(mpsc::Sender<Result<T, Status>>) -> F,
    {
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let mut close = self.close.subscribe();
        let closer = tx.clone();
        let work = body(tx);
        tokio::spawn(async move {
            tokio::select! {
                () = work => {}
                closed = close.recv() => {
                    if matches!(closed, Ok(true)) {
                        let _ = closer.send(Err(Status::cancelled("streams closed"))).await;
                    }
                }
            }
        });
        Box::pin(ReceiverStream::new(rx))
    }
}

/// A xorshift step. Random test messages need no real entropy, only spread.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn random_seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |now| now.subsec_nanos().into()) | 1
}

/// The time until the next random message, within the settings' period.
fn random_period(settings: &RandomMessagesStreamSettings, seed: &mut u64) -> Duration {
    let low = settings.period_min.min(settings.period_max);
    let high = settings.period_min.max(settings.period_max);
    let span = u64::from(high - low) + 1;
    delay(low + (next_random(seed) % span) as u32)
}

/// The rich error a call gets. The client's grpc_rich_error sends only a message with code 0 and looks for
/// a BadRequest detail, so a message without a code is reported as INVALID_ARGUMENT.
fn rich_error(request: EchoErrorRequest) -> Option<Status> {
    let code = match request.code {
        0 if request.msg.is_empty() => return None,
        0 => Code::InvalidArgument,
        code => Code::from_i32(code as i32),
    };
    let violation = FieldViolation { field: "msg".into(), description: request.msg.clone(), ..Default::default() };
    let detail = BadRequest { field_violations: vec![violation] };
    let details = RichStatus {
        code: code as i32,
        message: request.msg.clone(),
        details: vec![prost_types::Any { type_url: BAD_REQUEST_TYPE.into(), value: detail.encode_to_vec() }],
    };
    let mut metadata = MetadataMap::new();
    if let Ok(pushback) = MetadataValue::try_from(request.retry_pushback_ms.as_str()) {
        if !request.retry_pushback_ms.is_empty() {
            metadata.insert("grpc-retry-pushback-ms", pushback);
        }
    }
    Some(Status::with_details_and_metadata(code, request.msg, details.encode_to_vec().into(), metadata))
}

#[tonic::async_trait]
impl EchoService for Echo {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let request = request.into_inner();
        sleep(delay(request.delay)).await;
        Ok(Response::new(EchoResponse { msg: request.msg }))
    }

    async fn echo_client_stream(
        &self,
        request: Request<Streaming<EchoClientStreamRequest>>,
    ) -> Result<Response<EchoClientStreamResponse>, Status> {
        let mut incoming = request.into_inner();
        let mut close = self.close.subscribe();
        let mut msgs = Vec::new();
        loop {
            tokio::select! {
                message = incoming.message() => match message? {
                    Some(message) => msgs.push(message.msg),
                    None => break,
                },
                closed = close.recv() => {
                    if matches!(closed, Ok(true)) {
                        return Err(Status::cancelled("streams closed"));
                    }
                    break;
                }
            }
        }
        Ok(Response::new(EchoClientStreamResponse { msgs }))
    }

    type EchoServerStreamStream = Reply<EchoServerStreamResponse>;

    /// `max` 0 streams until the client leaves or CloseAllStreams.
    async fn echo_server_stream(
        &self,
        request: Request<EchoServerStreamRequest>,
    ) -> Result<Response<Self::EchoServerStreamStream>, Status> {
        let request = request.into_inner();
        let period = delay(request.period.max(1));
        Ok(Response::new(self.stream(|tx| async move {
            let mut count = 0;
            while request.max == 0 || count < request.max {
                count += 1;
                let reply = EchoServerStreamResponse { msg: request.msg.clone(), count };
                if tx.send(Ok(reply)).await.is_err() {
                    return;
                }
                sleep(period).await;
            }
        })))
    }

    type EchoDuplexStreamStream = Reply<EchoDuplexStreamResponse>;

    /// Echoes every message. The first request with enabled random settings also starts random messages,
    /// and the stream stays open after the client's half closes until they have all gone out.
    async fn echo_duplex_stream(
        &self,
        request: Request<Streaming<EchoDuplexStreamRequest>>,
    ) -> Result<Response<Self::EchoDuplexStreamStream>, Status> {
        let mut incoming = request.into_inner();
        Ok(Response::new(self.stream(|tx| async move {
            let mut count = 0;
            let mut sent_random = 0;
            let mut seed = random_seed();
            let mut random: Option<RandomMessagesStreamSettings> = None;
            let mut next: Option<Instant> = None;
            let mut open = true;
            while open || next.is_some() {
                let timer = async {
                    match next {
                        Some(at) => sleep_until(at).await,
                        None => pending().await,
                    }
                };
                let reply = tokio::select! {
                    message = incoming.message(), if open => match message {
                        Ok(Some(message)) => {
                            if let Some(settings) = message.random_message_settings.filter(|s| s.enabled) {
                                if random.is_none() {
                                    next = Some(Instant::now() + random_period(&settings, &mut seed));
                                    random = Some(settings);
                                }
                            }
                            message.msg
                        }
                        Ok(None) => {
                            open = false;
                            continue;
                        }
                        Err(status) => {
                            let _ = tx.send(Err(status)).await;
                            return;
                        }
                    },
                    () = timer => {
                        sent_random += 1;
                        let settings = random.as_ref().expect("the timer only runs with settings");
                        next = (settings.max == 0 || sent_random < settings.max)
                            .then(|| Instant::now() + random_period(settings, &mut seed));
                        format!("random message {sent_random}")
                    }
                };
                count += 1;
                if tx.send(Ok(EchoDuplexStreamResponse { msg: reply, count })).await.is_err() {
                    return;
                }
            }
        })))
    }

    type EchoSimpleDuplexStreamStream = Reply<EchoSimpleDuplexStreamResponse>;

    async fn echo_simple_duplex_stream(
        &self,
        request: Request<Streaming<EchoSimpleDuplexStreamRequest>>,
    ) -> Result<Response<Self::EchoSimpleDuplexStreamStream>, Status> {
        let mut incoming = request.into_inner();
        Ok(Response::new(self.stream(|tx| async move {
            while let Some(message) = incoming.next().await {
                let reply = match message {
                    Ok(message) => {
                        sleep(delay(message.delay)).await;
                        Ok(EchoSimpleDuplexStreamResponse { msg: message.msg })
                    }
                    Err(status) => Err(status),
                };
                if tx.send(reply).await.is_err() {
                    return;
                }
            }
        })))
    }

    async fn close_all_streams(
        &self,
        request: Request<CloseAllStreamsRequest>,
    ) -> Result<Response<CloseAllStreamsResponse>, Status> {
        sc_runtime::caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let request = request.into_inner();
        let close = self.close.clone();
        tokio::spawn(async move {
            sleep(delay(request.delay)).await;
            let _ = close.send(request.cancel);
        });
        Ok(Response::new(CloseAllStreamsResponse::default()))
    }

    async fn echo_error(&self, request: Request<EchoErrorRequest>) -> Result<Response<EchoErrorResponse>, Status> {
        match rich_error(request.into_inner()) {
            Some(status) => Err(status),
            None => Ok(Response::new(EchoErrorResponse::default())),
        }
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let (close, _) = broadcast::channel(8);
    let server = EchoServiceServer::new(Echo { state: sc_runtime::state::State::load(&args.common.data)?, close })
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "echo listening");
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
    fn random_period_stays_inside_the_settings() {
        let settings = RandomMessagesStreamSettings { period_min: 10, period_max: 20, ..Default::default() };
        let mut seed = 7;
        for _ in 0..100 {
            let period = random_period(&settings, &mut seed);
            assert!(period >= Duration::from_millis(10) && period <= Duration::from_millis(20));
        }
    }

    #[test]
    fn delays_are_bounded() {
        assert_eq!(delay(u32::MAX), Duration::from_millis(u64::from(MAX_DELAY_MS)));
    }

    #[test]
    fn a_bare_message_becomes_a_rich_invalid_argument() {
        let status = rich_error(EchoErrorRequest { msg: "boom".into(), ..Default::default() }).unwrap();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "boom");
        let details = RichStatus::decode(status.details()).unwrap();
        assert_eq!(details.details[0].type_url, BAD_REQUEST_TYPE);
    }

    #[test]
    fn nothing_to_report_is_no_error() {
        assert!(rich_error(EchoErrorRequest::default()).is_none());
    }

    #[test]
    fn an_explicit_code_carries_the_pushback() {
        let request = EchoErrorRequest { msg: "busy".into(), code: 14, retry_pushback_ms: "250".into() };
        let status = rich_error(request).unwrap();
        assert_eq!(status.code(), Code::Unavailable);
        assert_eq!(status.metadata().get("grpc-retry-pushback-ms").unwrap(), "250");
    }
}
