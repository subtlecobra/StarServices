//! sc.external.services.login.v1.LoginService.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::{
    internal::{push_publisher_client::PushPublisherClient, PublishRequest},
    login::{
        login_service_server::{LoginService, LoginServiceServer},
        AccountLoginCharacterStatus, CancelLoginRequest, CancelLoginResponse, InitiateLoginRequest,
        InitiateLoginResponse, LoginCompleted, LoginRequestPhase, LoginRequestState, LoginRequestStatus,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS login_sessions (
    session_id TEXT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    cancelled_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS login_sessions_account ON login_sessions (account_id, started_at DESC);";
const LOGIN_PACKAGE: &str = "sc.external.services.login.v1";
/// The client registers its login subscriptions around the time it calls InitiateLogin.
const SUBSCRIBE_WAIT_MS: u32 = 5000;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of the push service.
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
}

struct Login {
    state: State,
    db: Postgres,
    push: PushPublisherClient<Channel>,
}

async fn publish(
    mut push: PushPublisherClient<Channel>,
    account: &Account,
    type_name: &str,
    value: Vec<u8>,
) -> Result<u32, Status> {
    let request = PublishRequest {
        account_id: account.account_id,
        topic: format!("account/{}/login", account.account_id),
        type_name: format!("{LOGIN_PACKAGE}.{type_name}"),
        value,
        wait_for_subscriber_ms: SUBSCRIBE_WAIT_MS,
    };
    Ok(push.publish(request).await?.into_inner().delivered)
}

/// The real service acknowledges InitiateLogin and finishes over push.
async fn finish_login(push: PushPublisherClient<Channel>, account: Account) -> Result<(), Status> {
    let characters = AccountLoginCharacterStatus { players: vec![account.player()] };
    let status = publish(push.clone(), &account, "AccountLoginCharacterStatus", characters.encode_to_vec());
    let delivered = status.await?;
    let completed = publish(push, &account, "LoginCompleted", LoginCompleted {}.encode_to_vec()).await?;
    info!(handle = %account.handle, delivered, completed, "login pushes sent");
    Ok(())
}

#[tonic::async_trait]
impl LoginService for Login {
    async fn initiate_login(
        &self,
        request: Request<InitiateLoginRequest>,
    ) -> Result<Response<InitiateLoginResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let session = request.into_inner().session_id;
        self.db
            .execute(
                "INSERT INTO login_sessions (session_id, account_id) VALUES ($1, $2)
                 ON CONFLICT (session_id) DO UPDATE SET started_at = now(), cancelled_at = NULL",
                &[&session, &i64::from(account.account_id)],
            )
            .await
            .map_err(internal)?;
        info!(handle = %account.handle, %session, "login started");

        let push = self.push.clone();
        let login = account.clone();
        tokio::spawn(async move {
            if let Err(error) = finish_login(push, login).await {
                warn!(%error, "login pushes failed");
            }
        });

        let status = LoginRequestStatus {
            login_request_id: session.clone(),
            state: LoginRequestState::Ongoing as i32,
            phase: LoginRequestPhase::Authenticating as i32,
            ..Default::default()
        };
        Ok(Response::new(InitiateLoginResponse {
            account_id: account.account_id,
            nickname: account.handle.clone(),
            displayname: account.handle,
            status: Some(status),
            tracking_metrics_id: session,
            ..Default::default()
        }))
    }

    async fn cancel_login(
        &self,
        request: Request<CancelLoginRequest>,
    ) -> Result<Response<CancelLoginResponse>, Status> {
        // The client also calls this once a login has completed, to stop its queue tracking.
        if let Some(account) = caller(&self.state, request.metadata()) {
            self.db
                .execute(
                    "UPDATE login_sessions SET cancelled_at = now()
                     WHERE account_id = $1 AND cancelled_at IS NULL",
                    &[&i64::from(account.account_id)],
                )
                .await
                .map_err(internal)?;
        }
        Ok(Response::new(CancelLoginResponse {}))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let state = State::load(&args.common.data)?;
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let endpoint = Endpoint::from_shared(args.push.clone()).map_err(|e| format!("--push: {e}"))?;
    let login = Login { state, db, push: PushPublisherClient::new(endpoint.connect_lazy()) };
    let service = LoginServiceServer::new(login)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "login listening");
    Server::builder().add_service(service).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}
