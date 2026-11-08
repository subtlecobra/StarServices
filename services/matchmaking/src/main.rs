//! sc.external.services.matchmaking.v1.FrontendService.
//!
//! A ticket is matched by asking the shard directory for a shard straight away, because no queue of real
//! players exists to wait on. The client has no code calling this service, so the push topic is a guess.
//! GetLastAssignment always answers from the stored ticket.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_api::internal::{ push_publisher_client::PushPublisherClient, shard_directory_client::ShardDirectoryClient, AllocateShardRequest, AllocateShardResponse, PublishRequest, };
use sc_proto::{
    common::{
    game::v1::{mode, Game, Mode},
    network::v1::{Endpoint as GameEndpoint, Region},
    },
    services::matchmaking::v1::{
    frontend_service_server::{FrontendService, FrontendServiceServer},
    match_request, matchmaking_in_progress, AcceptRequest, AcceptResponse, Assignment, CancelRequest,
    CancelResponse, GetLastAssignmentRequest, GetLastAssignmentResponse, MatchRequest, MatchResponse,
    MatchmakingCancelled, MatchmakingInProgress,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Code, Request, Response, Status,
};
use tracing::{info, warn};

const PACKAGE: &str = "sc.external.services.matchmaking.v1";
const QUEUED: &str = "queued";
const ASSIGNED: &str = "assigned";
const CANCELLED: &str = "cancelled";
const FAILED: &str = "failed";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS matchmaking_tickets (
    ticket_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    geid BIGINT NOT NULL,
    state TEXT NOT NULL,
    request BYTEA NOT NULL,
    assignment BYTEA,
    accepted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS matchmaking_tickets_player ON matchmaking_tickets (geid, updated_at DESC);";

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
    /// Address of the shard directory.
    #[arg(long, env = "SCBACKEND_SHARD_DIRECTORY", default_value = "http://sc-shard-operator:50051")]
    shard_directory: String,
    /// Region to allocate in when the request names none, or one with no ShardCluster.
    #[arg(long, env = "SCBACKEND_REGION", default_value = "usa")]
    region: String,
}

#[derive(Clone)]
struct Matchmaker {
    state: std::sync::Arc<State>,
    db: Postgres,
    push: PushPublisherClient<Channel>,
    directory: ShardDirectoryClient<Channel>,
    region: String,
}

fn region_name(region: Region) -> Option<&'static str> {
    match region {
        Region::Us => Some("usa"),
        Region::Europe => Some("europe"),
        Region::Oceania => Some("oceania"),
        Region::Asia => Some("asia"),
        Region::Unspecified | Region::Expansion => None,
    }
}

fn in_progress(ticket_id: String, account: &Account, request: &MatchRequest) -> MatchmakingInProgress {
    let (game, chosen, group) = match &request.game {
        Some(match_request::Game::PersistentUniverse(pu)) => (
            Game::PersistentUniverse,
            None,
            Some(matchmaking_in_progress::Group::PartyId(pu.party_id.clone())),
        ),
        Some(match_request::Game::ArenaCommander(arena)) => (
            Game::ArenaCommander,
            Some(mode::Mode::ArenaCommanderMode(arena.mode)),
            Some(matchmaking_in_progress::Group::SquadId(arena.squad_id.clone())),
        ),
        Some(match_request::Game::StarMarine(marine)) => (
            Game::StarMarine,
            Some(mode::Mode::StarMarineMode(marine.mode)),
            Some(matchmaking_in_progress::Group::SquadId(marine.squad_id.clone())),
        ),
        _ => (Game::Unspecified, None, None),
    };
    MatchmakingInProgress {
        ticket_id,
        creator_name: account.handle.clone(),
        game: game as i32,
        mode: chosen.map(|mode| Mode { mode: Some(mode) }),
        group,
    }
}

fn assignment(ticket_id: String, shard: AllocateShardResponse) -> Assignment {
    Assignment {
        endpoint: Some(GameEndpoint { host: shard.public_address, port: shard.public_port }),
        credentials: None,
        ticket_id,
        instance_id: shard.name,
    }
}

impl Matchmaker {
    fn account(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    async fn publish(&self, account: &Account, type_name: &str, value: Vec<u8>) {
        let request = PublishRequest {
            account_id: account.account_id,
            topic: format!("account/{}/matchmaking", account.account_id),
            type_name: format!("{PACKAGE}.{type_name}"),
            value,
            wait_for_subscriber_ms: 0,
        };
        if let Err(error) = self.push.clone().publish(request).await {
            warn!(%error, type_name, "matchmaking push failed");
        }
    }

    async fn allocate(&self, region: Region) -> Result<AllocateShardResponse, Status> {
        let wanted = region_name(region).unwrap_or(&self.region).to_string();
        let first = self.directory.clone().allocate(AllocateShardRequest { region: wanted.clone() }).await;
        match first {
            Err(status) if status.code() == Code::NotFound && wanted != self.region => {
                let request = AllocateShardRequest { region: self.region.clone() };
                Ok(self.directory.clone().allocate(request).await?.into_inner())
            }
            other => Ok(other?.into_inner()),
        }
    }

    /// Moves a queued ticket on. A ticket cancelled meanwhile stays cancelled, and the call says so.
    async fn settle(&self, ticket: &str, state: &str, assignment: Option<&[u8]>) -> Result<bool, Status> {
        let changed = self
            .db
            .execute(
                "UPDATE matchmaking_tickets SET state = $2, assignment = $3, updated_at = now()
                 WHERE ticket_id::text = $1 AND state = $4",
                &[&ticket, &state, &assignment, &QUEUED],
            )
            .await
            .map_err(internal)?;
        Ok(changed > 0)
    }

    async fn run_ticket(self, account: Account, ticket: String, region: Region) {
        let result = match self.allocate(region).await {
            Ok(shard) => {
                let found = assignment(ticket.clone(), shard);
                let saved = self.settle(&ticket, ASSIGNED, Some(&found.encode_to_vec())).await;
                if matches!(saved, Ok(true)) {
                    self.publish(&account, "Assignment", found.encode_to_vec()).await;
                }
                saved.map(|_| ())
            }
            Err(status) => Err(status),
        };
        if let Err(status) = result {
            warn!(%status, %ticket, "matchmaking failed");
            if matches!(self.settle(&ticket, FAILED, None).await, Ok(true)) {
                let cancelled = MatchmakingCancelled { ticket_id: ticket };
                self.publish(&account, "MatchmakingCancelled", cancelled.encode_to_vec()).await;
            }
        }
    }

    async fn rematch(&self, geid: i64, ticket: &str) -> Result<MatchRequest, Status> {
        let row = self
            .db
            .query_opt("SELECT request FROM matchmaking_tickets WHERE ticket_id::text = $1 AND geid = $2", &[&ticket, &geid])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no ticket {ticket}")))?;
        MatchRequest::decode(row.get::<_, &[u8]>("request")).map_err(internal)
    }
}

#[tonic::async_trait]
impl FrontendService for Matchmaker {
    async fn r#match(&self, request: Request<MatchRequest>) -> Result<Response<MatchResponse>, Status> {
        let account = self.account(request.metadata())?;
        let geid = account.geid as i64;
        let mut request = request.into_inner();
        match &request.game {
            None => return Err(Status::invalid_argument("missing game")),
            Some(match_request::Game::RematchTicketId(ticket)) => {
                let region = request.region;
                request = self.rematch(geid, ticket).await?;
                if region != Region::Unspecified as i32 {
                    request.region = region;
                }
            }
            Some(_) => {}
        }
        self.db
            .execute(
                "UPDATE matchmaking_tickets SET state = $2, updated_at = now() WHERE geid = $1 AND state = $3",
                &[&geid, &CANCELLED, &QUEUED],
            )
            .await
            .map_err(internal)?;
        let row = self
            .db
            .query_opt(
                "INSERT INTO matchmaking_tickets (geid, state, request) VALUES ($1, $2, $3)
                 RETURNING ticket_id::text AS id",
                &[&geid, &QUEUED, &request.encode_to_vec()],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("ticket not created"))?;
        let ticket_id: String = row.get("id");
        info!(handle = %account.handle, ticket = %ticket_id, "matchmaking started");
        let worker = self.clone();
        let ticket = ticket_id.clone();
        let region = request.region();
        tokio::spawn(async move {
            let started = in_progress(ticket.clone(), &account, &request);
            worker.publish(&account, "MatchmakingInProgress", started.encode_to_vec()).await;
            worker.run_ticket(account, ticket, region).await;
        });
        Ok(Response::new(MatchResponse { ticket_id }))
    }

    async fn cancel(&self, request: Request<CancelRequest>) -> Result<Response<CancelResponse>, Status> {
        let account = self.account(request.metadata())?;
        let ticket_id = request.into_inner().ticket_id;
        let changed = self
            .db
            .execute(
                "UPDATE matchmaking_tickets SET state = $3, updated_at = now()
                 WHERE ticket_id::text = $1 AND geid = $2 AND state = $4",
                &[&ticket_id, &(account.geid as i64), &CANCELLED, &QUEUED],
            )
            .await
            .map_err(internal)?;
        if changed > 0 {
            let cancelled = MatchmakingCancelled { ticket_id };
            self.publish(&account, "MatchmakingCancelled", cancelled.encode_to_vec()).await;
        }
        Ok(Response::new(CancelResponse::default()))
    }

    async fn get_last_assignment(
        &self,
        request: Request<GetLastAssignmentRequest>,
    ) -> Result<Response<GetLastAssignmentResponse>, Status> {
        let account = self.account(request.metadata())?;
        let row = self
            .db
            .query_opt(
                "SELECT assignment FROM matchmaking_tickets WHERE geid = $1 AND state = $2
                 ORDER BY updated_at DESC LIMIT 1",
                &[&(account.geid as i64), &ASSIGNED],
            )
            .await
            .map_err(internal)?;
        let assignment = row
            .and_then(|row| row.get::<_, Option<Vec<u8>>>("assignment"))
            .map(|bytes| Assignment::decode(bytes.as_slice()))
            .transpose()
            .map_err(internal)?;
        Ok(Response::new(GetLastAssignmentResponse { assignment }))
    }

    /// No proposal is ever offered before assignment, so a proposal id is the id of an assigned ticket.
    async fn accept(&self, request: Request<AcceptRequest>) -> Result<Response<AcceptResponse>, Status> {
        let account = self.account(request.metadata())?;
        let proposal = request.into_inner().proposal_id;
        let changed = self
            .db
            .execute(
                "UPDATE matchmaking_tickets SET accepted_at = COALESCE(accepted_at, now())
                 WHERE ticket_id::text = $1 AND geid = $2 AND state = $3",
                &[&proposal, &(account.geid as i64), &ASSIGNED],
            )
            .await
            .map_err(internal)?;
        if changed == 0 {
            return Err(Status::not_found(format!("no proposal {proposal}")));
        }
        Ok(Response::new(AcceptResponse::default()))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let push = Endpoint::from_shared(args.push.clone()).map_err(|e| format!("--push: {e}"))?;
    let directory = Endpoint::from_shared(args.shard_directory.clone()).map_err(|e| format!("--shard-directory: {e}"))?;
    let service = Matchmaker {
        state: std::sync::Arc::new(State::load(&args.common.data)?),
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        push: PushPublisherClient::new(push.connect_lazy()),
        directory: ShardDirectoryClient::new(directory.connect_lazy()),
        region: args.region,
    };
    let server = FrontendServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "matchmaking listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use sc_proto::services::matchmaking::v1::GameStarMarineMatchRequest;

    use super::*;

    #[test]
    fn regions_without_a_name_use_the_default() {
        assert_eq!(region_name(Region::Europe), Some("europe"));
        assert_eq!(region_name(Region::Unspecified), None);
    }

    #[test]
    fn progress_describes_the_request() {
        let account = Account { account_id: 1, handle: "a".into(), geid: 2, created_at_ms: 0 };
        let game = match_request::Game::StarMarine(GameStarMarineMatchRequest { mode: 2, squad_id: "s".into(), ..Default::default() });
        let request = MatchRequest { region: 0, game: Some(game) };
        let progress = in_progress("t".into(), &account, &request);
        assert_eq!(progress.game, Game::StarMarine as i32);
        assert_eq!(progress.creator_name, "a");
        assert_eq!(progress.group, Some(matchmaking_in_progress::Group::SquadId("s".into())));
        assert_eq!(progress.mode.unwrap().mode, Some(mode::Mode::StarMarineMode(2)));
    }

    #[test]
    fn assignments_carry_the_shard_address() {
        let shard = AllocateShardResponse { name: "n".into(), public_address: "1.2.3.4".into(), public_port: 7 };
        let found = assignment("t".into(), shard);
        assert_eq!(found.endpoint.unwrap().port, 7);
        assert_eq!(found.instance_id, "n");
    }
}
