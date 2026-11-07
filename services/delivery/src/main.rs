//! sc.external.services.delivery.v1.ExternalDeliveryService. A worker marks every pending delivery
//! delivered once it is due and tells the player. The update claims each row, so replicas cannot deliver
//! the same one twice. The push is best effort: a player who misses it sees the status in the next query. The account a
//! push goes to comes from sc-identity, which owns accounts (`IdentityInternal.GetAccounts`).

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, time::Duration};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_economy::{command::Push, reply_stream, timestamp_of, ReplyStream};
use sc_proto::services::delivery::v1::{
    external_delivery_service_server::{ExternalDeliveryService, ExternalDeliveryServiceServer},
    Delivery, DeliveryStatus, QueryDeliveriesStreamRequest, QueryDeliveriesStreamResponse,
};
use sc_proto::internal::{identity_internal_client::IdentityInternalClient, GetAccountsRequest};
use sc_proto::services::entitlement::v2::DelayedDeliveryCompleted;
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

const COMPLETED: &str = "sc.external.services.entitlement.v2.DelayedDeliveryCompleted";
const WORKER_PERIOD: Duration = Duration::from_secs(5);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS deliveries (
    id TEXT PRIMARY KEY,
    status INT NOT NULL,
    player_id BIGINT NOT NULL,
    entity_id BIGINT NOT NULL,
    source_inventory_id TEXT NOT NULL DEFAULT '',
    destination_inventory_id TEXT NOT NULL DEFAULT '',
    deliver_at TIMESTAMPTZ NOT NULL,
    requester_urn TEXT NOT NULL DEFAULT '',
    attempts INT NOT NULL DEFAULT 0,
    failure_reason TEXT NOT NULL DEFAULT '',
    is_expedited BOOLEAN NOT NULL DEFAULT FALSE,
    slot_id INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS deliveries_by_player ON deliveries (player_id);
CREATE INDEX IF NOT EXISTS deliveries_due ON deliveries (deliver_at) WHERE status = 1;";
const COMPLETE_DUE: &str = "
UPDATE deliveries SET status = $1, attempts = attempts + 1, updated_at = now()
WHERE status = $2 AND deliver_at <= now()
RETURNING id, player_id, entity_id, destination_inventory_id,
          (extract(epoch FROM updated_at) * 1000)::bigint AS delivered_ms";

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
    /// `sc-identity`, asked for the account of each player a delivery completes for.
    #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://sc-identity:50051")]
    identity: String,
}

struct Deliveries {
    state: State,
    db: Postgres,
}

fn delivery(row: &Row) -> Delivery {
    Delivery {
        id: row.get("id"),
        status: row.get("status"),
        player_id: row.get::<_, i64>("player_id") as u64,
        entity_id: row.get::<_, i64>("entity_id") as u64,
        source_inventory_id: row.get("source_inventory_id"),
        destination_inventory_id: row.get("destination_inventory_id"),
        deliver_at: timestamp_of(row, "deliver_ms"),
        requester_urn: row.get("requester_urn"),
        attempts: row.get("attempts"),
        failure_reason: row.get("failure_reason"),
        created_at: timestamp_of(row, "created_ms"),
        updated_at: timestamp_of(row, "updated_ms"),
        is_expedited: row.get("is_expedited"),
        slot_id: row.get("slot_id"),
    }
}

/// The client subscribes to group `player`, its GEID and `delivery` (SubscribeToPlayerDelivery).
fn topic(player: i64) -> String {
    format!("player/{}/delivery", player as u64)
}

fn completed(row: &Row) -> DelayedDeliveryCompleted {
    DelayedDeliveryCompleted {
        delivery_id: row.get("id"),
        entity_id: row.get::<_, i64>("entity_id") as u64,
        destination_inventory_id: row.get("destination_inventory_id"),
        delivered_at: row.get("delivered_ms"),
    }
}

/// The accounts of these players. Players sc-identity does not know are left out, and so is everyone when it does not
/// answer: the rows are delivered already, and the players see them in their next query.
async fn accounts(identity: &IdentityInternalClient<Channel>, players: Vec<u64>) -> HashMap<u64, u32> {
    if players.is_empty() {
        return HashMap::new();
    }
    let mut request = Request::new(GetAccountsRequest { geids: players });
    request.set_timeout(IDENTITY_TIMEOUT);
    match identity.clone().get_accounts(request).await {
        Ok(reply) => reply.into_inner().accounts.into_iter().map(|found| (found.geid, found.account_id)).collect(),
        Err(status) => {
            warn!(%status, "no accounts from identity, completed deliveries go untold");
            HashMap::new()
        }
    }
}

async fn complete_due(db: &Postgres, push: &Push, identity: &IdentityInternalClient<Channel>) -> Result<usize, String> {
    let (delivered, pending) = (DeliveryStatus::Delivered as i32, DeliveryStatus::Pending as i32);
    let rows = db.query(COMPLETE_DUE, &[&delivered, &pending]).await?;
    let mut players: Vec<u64> = rows.iter().map(|row| row.get::<_, i64>("player_id") as u64).collect();
    players.sort_unstable();
    players.dedup();
    let accounts = accounts(identity, players).await;
    for row in &rows {
        let player: i64 = row.get("player_id");
        match accounts.get(&(player as u64)) {
            Some(&account) => push.notify(account, topic(player), COMPLETED, &completed(row)).await,
            None => warn!(player, "delivery for a player with no known account"),
        }
    }
    Ok(rows.len())
}

async fn worker(db: Postgres, push: Push, identity: IdentityInternalClient<Channel>) {
    let mut ticks = tokio::time::interval(WORKER_PERIOD);
    loop {
        ticks.tick().await;
        match complete_due(&db, &push, &identity).await {
            Ok(0) => {}
            Ok(count) => info!(count, "deliveries completed"),
            Err(error) => warn!(%error, "delivery worker"),
        }
    }
}

#[tonic::async_trait]
impl ExternalDeliveryService for Deliveries {
    type QueryDeliveriesStreamStream = ReplyStream<QueryDeliveriesStreamResponse>;

    async fn query_deliveries_stream(
        &self,
        request: Request<QueryDeliveriesStreamRequest>,
    ) -> Result<Response<Self::QueryDeliveriesStreamStream>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let filter = request.into_inner().filter.unwrap_or_default();
        let entity_ids: Vec<i64> = filter.entity_ids.iter().map(|id| *id as i64).collect();
        let rows = self
            .db
            .query(
                "SELECT id, status, player_id, entity_id, source_inventory_id, destination_inventory_id,
                        requester_urn, attempts, failure_reason, is_expedited, slot_id,
                        (extract(epoch FROM deliver_at) * 1000)::bigint AS deliver_ms,
                        (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
                        (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms
                 FROM deliveries
                 WHERE player_id = $1
                   AND (cardinality($2::int[]) = 0 OR status = ANY($2))
                   AND (cardinality($3::text[]) = 0 OR id = ANY($3))
                   AND (cardinality($4::bigint[]) = 0 OR entity_id = ANY($4))
                   AND (cardinality($5::text[]) = 0 OR requester_urn = ANY($5))
                 ORDER BY deliver_at, id",
                &[&(account.geid as i64), &filter.statuses, &filter.ids, &entity_ids, &filter.requester_urns],
            )
            .await
            .map_err(internal)?;
        let deliveries: Vec<Delivery> = rows.iter().map(delivery).collect();
        Ok(Response::new(reply_stream(deliveries, |results| QueryDeliveriesStreamResponse { results })))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let db = Postgres::connect(&args.database.postgres, SCHEMA).await?;
    let endpoint = Endpoint::from_shared(args.identity.clone()).map_err(|e| format!("--identity: {e}"))?;
    let identity = IdentityInternalClient::new(endpoint.connect_lazy());
    tokio::spawn(worker(db.clone(), Push::connect(&args.push)?, identity));
    let service = Deliveries { state: State::load(&args.common.data)?, db };
    let server = ExternalDeliveryServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "delivery listening");
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
    fn the_topic_is_the_one_the_client_subscribes_to() {
        assert_eq!(topic(42), "player/42/delivery");
    }

    #[test]
    fn large_geids_stay_unsigned_in_the_topic() {
        assert_eq!(topic(-1), format!("player/{}/delivery", u64::MAX));
    }

    use sc_proto::internal::{
        identity_internal_server::{IdentityInternal, IdentityInternalServer},
        ChangePasswordRequest, ChangePasswordResponse, GetAccountRequest, GetAccountResponse, GetAccountsResponse,
        IssueGameTokenRequest, IssueGameTokenResponse, ListWebSessionsRequest, ListWebSessionsResponse,
        PasswordLoginRequest, PasswordLoginResponse, PlayerAccount, RegisterAccountRequest, RegisterAccountResponse,
        RevokeWebSessionRequest, RevokeWebSessionResponse, SearchAccountsRequest, SearchAccountsResponse,
        SetAccountStateRequest, SetAccountStateResponse, VerifyWebSessionRequest, VerifyWebSessionResponse,
    };

    /// Knows player `n` as account `n + 100`, for players below 100.
    struct FakeIdentity;

    #[tonic::async_trait]
    impl IdentityInternal for FakeIdentity {
        async fn get_accounts(&self, request: Request<GetAccountsRequest>) -> Result<Response<GetAccountsResponse>, Status> {
            let known = request.into_inner().geids.into_iter().filter(|&geid| geid < 100);
            let accounts = known.map(|geid| PlayerAccount { geid, account_id: geid as u32 + 100 }).collect();
            Ok(Response::new(GetAccountsResponse { accounts }))
        }

        // The web account calls, which delivery never makes.
        async fn register_account(&self, _: Request<RegisterAccountRequest>) -> Result<Response<RegisterAccountResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn password_login(&self, _: Request<PasswordLoginRequest>) -> Result<Response<PasswordLoginResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn verify_web_session(&self, _: Request<VerifyWebSessionRequest>) -> Result<Response<VerifyWebSessionResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn revoke_web_session(&self, _: Request<RevokeWebSessionRequest>) -> Result<Response<RevokeWebSessionResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn list_web_sessions(&self, _: Request<ListWebSessionsRequest>) -> Result<Response<ListWebSessionsResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn change_password(&self, _: Request<ChangePasswordRequest>) -> Result<Response<ChangePasswordResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn issue_game_token(&self, _: Request<IssueGameTokenRequest>) -> Result<Response<IssueGameTokenResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn get_account(&self, _: Request<GetAccountRequest>) -> Result<Response<GetAccountResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn search_accounts(&self, _: Request<SearchAccountsRequest>) -> Result<Response<SearchAccountsResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
        async fn set_account_state(&self, _: Request<SetAccountStateRequest>) -> Result<Response<SetAccountStateResponse>, Status> {
            Err(Status::unimplemented("web accounts"))
        }
    }

    async fn fake_identity() -> IdentityInternalClient<Channel> {
        let address: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tokio::spawn(Server::builder().add_service(IdentityInternalServer::new(FakeIdentity)).serve(address));
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        IdentityInternalClient::new(Endpoint::from_shared(format!("http://{address}")).unwrap().connect_lazy())
    }

    #[tokio::test]
    async fn accounts_come_from_identity_and_unknown_players_are_left_out() {
        let found = accounts(&fake_identity().await, vec![7, 500]).await;
        assert_eq!(found, HashMap::from([(7, 107)]));
    }

    #[tokio::test]
    async fn no_identity_means_no_accounts_and_no_error() {
        let identity = IdentityInternalClient::new(Endpoint::from_static("http://127.0.0.1:1").connect_lazy());
        assert!(accounts(&identity, vec![7]).await.is_empty());
    }

    /// Against a real YugabyteDB, in a private schema:
    /// `SC_TEST_POSTGRES=<url> cargo test -p sc-delivery -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn only_due_pending_rows_are_completed_and_only_once() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, SCHEMA).await.unwrap();
        let push = Push::connect("http://127.0.0.1:1").unwrap();
        let identity = fake_identity().await;
        let insert = "INSERT INTO deliveries (id, status, player_id, entity_id, deliver_at)
                      VALUES ($1, 1, 7, 9, now() + $2::int * interval '1 hour') ON CONFLICT DO NOTHING";
        db.execute(insert, &[&"due", &-1]).await.unwrap();
        db.execute(insert, &[&"later", &1]).await.unwrap();
        assert_eq!(complete_due(&db, &push, &identity).await.unwrap(), 1);
        assert_eq!(complete_due(&db, &push, &identity).await.unwrap(), 0);
        let rows = db.query("SELECT id, status FROM deliveries ORDER BY id", &[]).await.unwrap();
        let found: Vec<(String, i32)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        assert_eq!(found, [("due".to_string(), 2), ("later".to_string(), 1)]);
    }
}
