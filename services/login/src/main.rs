//! sc.external.services.login.v1.LoginService.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_api::internal::{identity_internal_client::IdentityInternalClient, push_publisher_client::PushPublisherClient, GetAccountRequest, PublishRequest};
use sc_proto::{
    common::platform::v1::PlatformBadge,
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
/// How long after its InitiateLogin reply a login is finished over push. The client takes the reply's account id and
/// badges into its local session info only while it is not authenticated yet (`CDiffusionCryClient::OnConnected`,
/// 0x147404120), and the pushes lead to that state, so the reply must reach it first.
const PUSH_AFTER_REPLY: std::time::Duration = std::time::Duration::from_millis(500);

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
    /// Address of sc-identity, which owns accounts and their in-game badges.
    #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://sc-identity:50051")]
    identity: String,
}

struct Login {
    state: State,
    db: Postgres,
    push: PushPublisherClient<Channel>,
    identity: IdentityInternalClient<Channel>,
}

impl Login {
    /// The account's in-game badges, as the login reply carries them: the client reads each one's id into its account
    /// badge mask, and QA, STAFF or GM mark a developer account. None when identity cannot say, so a login never fails
    /// over them.
    async fn badges(&self, account_id: u32) -> Vec<PlatformBadge> {
        match self.identity.clone().get_account(GetAccountRequest { account_id }).await {
            Ok(reply) => {
                let badges = reply.into_inner().account.map(|account| account.game_badges).unwrap_or_default();
                badges.into_iter().map(|badge| PlatformBadge { id: badge.id, name: badge.name }).collect()
            }
            Err(status) => {
                warn!(account_id, %status, "no badges from identity, logging in without");
                Vec::new()
            }
        }
    }
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

        let badges = self.badges(account.account_id).await;
        let push = self.push.clone();
        let login = account.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PUSH_AFTER_REPLY).await;
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
        if !badges.is_empty() {
            let names: Vec<&str> = badges.iter().map(|badge| badge.name.as_str()).collect();
            info!(handle = %account.handle, ?names, "login with badges");
        }
        Ok(Response::new(InitiateLoginResponse {
            badges,
            account_id: account.account_id,
            nickname: account.handle.clone(),
            displayname: account.handle,
            status: Some(status),
            tracking_metrics_id: session,
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
    let identity = Endpoint::from_shared(args.identity.clone()).map_err(|e| format!("--identity: {e}"))?;
    let login = Login { state, db, push: PushPublisherClient::new(endpoint.connect_lazy()), identity: IdentityInternalClient::new(identity.connect_lazy()) };
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
