//! sc.external.services.mission_service.v1.MissionService.
//!
//! Join, Leave and Share are commands: the client completes them when a CommandResponseBody arrives on the
//! push topic in their arguments, so the RPC reply alone leaves it waiting. Nothing in this API creates a
//! mission, so the first JoinMission of an unknown id does. Objectives and phases are stored as protobuf
//! and written outside this API.

use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use prost_types::{Any, Timestamp};
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_proto::{
    common::{
        command::v1::{notify_destination::Destination, CommandArguments, CommandResponseBody},
        mission::v1::{
            ActiveMissionInfo, MissionObjective, MissionParticipant, MissionPhase, MissionReward, MissionState,
            MissionTemplate,
        },
    },
    google::rpc::Status as RpcStatus,
    internal::{push_publisher_client::PushPublisherClient, PublishRequest},
    services::mission_service::v1::{
        check_mission_sharing_eligibility_response::NotEligiblePlayer,
        mission_service_server::{MissionService, MissionServiceServer},
        query_active_missions_response, CheckMissionSharingEligibilityRequest, CheckMissionSharingEligibilityResponse,
        JoinMissionRequest, JoinMissionResponse, LeaveMissionRequest, LeaveMissionResponse, QueryActiveMissionsRequest,
        QueryActiveMissionsResponse, QueryMissionObjectivesRequest, QueryMissionObjectivesResponse,
        QueryMissionPhasesRequest, QueryMissionPhasesResponse, QueryMissionsRequest, QueryMissionsResponse,
        ShareMissionRequest, ShareMissionResponse,
    },
};
use sc_runtime::player_urn;
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    metadata::MetadataMap,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

const COMMAND_RESPONSE: &str = "sc.external.common.command.v1.CommandResponseBody";
const PLAYER_URN_PREFIX: &str = "urn:sc:global:player:geid:";
/// A mission in one of these states can still be joined and shows as active.
const OPEN_STATES: [i32; 3] =
    [MissionState::Pending as i32, MissionState::Active as i32, MissionState::Suspended as i32];
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS missions (
    mission_id TEXT PRIMARY KEY,
    contract_id TEXT NOT NULL DEFAULT '',
    owner_urn TEXT NOT NULL,
    state INT NOT NULL,
    active_mission_key TEXT NOT NULL,
    template BYTEA,
    reward BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS missions_owner ON missions (owner_urn);
CREATE TABLE IF NOT EXISTS mission_participants (
    mission_id TEXT NOT NULL,
    player_urn TEXT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    left_at TIMESTAMPTZ,
    reason TEXT NOT NULL DEFAULT '',
    end_state INT NOT NULL DEFAULT 0,
    end_reason BIGINT NOT NULL DEFAULT 0,
    analytics_reason TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (mission_id, player_urn)
);
CREATE INDEX IF NOT EXISTS mission_participants_player ON mission_participants (player_urn);
CREATE TABLE IF NOT EXISTS mission_shares (
    mission_id TEXT NOT NULL,
    player_urn TEXT NOT NULL,
    shared_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (mission_id, player_urn)
);
CREATE TABLE IF NOT EXISTS mission_objectives (
    mission_id TEXT NOT NULL,
    objective_id TEXT NOT NULL,
    body BYTEA NOT NULL,
    PRIMARY KEY (mission_id, objective_id)
);
CREATE TABLE IF NOT EXISTS mission_phases (
    mission_id TEXT NOT NULL,
    phase_id TEXT NOT NULL,
    body BYTEA NOT NULL,
    PRIMARY KEY (mission_id, phase_id)
);";

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

struct Missions {
    state: State,
    db: Postgres,
    push: PushPublisherClient<Channel>,
}

struct Mission {
    owner: String,
    state: i32,
}

fn is_player_urn(urn: &str) -> bool {
    urn.strip_prefix(PLAYER_URN_PREFIX).is_some_and(|geid| geid.parse::<u64>().is_ok())
}

fn is_open(state: i32) -> bool {
    OPEN_STATES.contains(&state)
}

/// The owner may add anyone. Anyone else may only add themselves, to a mission shared with them.
fn may_join(caller: &str, owner: &str, players: &[String], shared_with_caller: bool) -> bool {
    caller == owner || (shared_with_caller && players.iter().all(|player| player == caller))
}

fn may_remove(caller: &str, owner: &str, player: &str) -> bool {
    caller == owner || caller == player
}

/// Why `player` cannot be offered the mission, if there is a reason.
fn share_refusal(player: &str, open: bool, participating: bool) -> Option<&'static str> {
    if !is_player_urn(player) {
        Some("invalid player")
    } else if !open {
        Some("mission is not active")
    } else if participating {
        Some("already in the mission")
    } else {
        None
    }
}

/// The state the mission ends in when an owner leaves or the last player does.
fn end_state(requested: i32) -> i32 {
    if requested == MissionState::Unspecified as i32 || is_open(requested) {
        MissionState::Ended as i32
    } else {
        requested
    }
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp { seconds: ms.div_euclid(1000), nanos: (ms.rem_euclid(1000) * 1_000_000) as i32 }
}

fn decode_opt<T: Message + Default>(row: &Row, column: &str) -> Result<Option<T>, Status> {
    row.get::<_, Option<&[u8]>>(column).map(T::decode).transpose().map_err(internal)
}

fn rpc_status(result: &Result<(), Status>) -> RpcStatus {
    match result {
        Ok(()) => RpcStatus::default(),
        Err(status) => RpcStatus { code: status.code() as i32, message: status.message().into(), details: Vec::new() },
    }
}

impl Missions {
    fn account(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    async fn mission(&self, mission_id: &str) -> Result<Option<Mission>, Status> {
        let row = self
            .db
            .query_opt("SELECT owner_urn, state FROM missions WHERE mission_id = $1", &[&mission_id])
            .await
            .map_err(internal)?;
        Ok(row.map(|row| Mission { owner: row.get("owner_urn"), state: row.get("state") }))
    }

    async fn is_shared_with(&self, mission_id: &str, player: &str) -> Result<bool, Status> {
        let row = self
            .db
            .query_opt("SELECT 1 AS found FROM mission_shares WHERE mission_id = $1 AND player_urn = $2", &[&mission_id, &player])
            .await
            .map_err(internal)?;
        Ok(row.is_some())
    }

    async fn participants(&self, mission_id: &str) -> Result<Vec<String>, Status> {
        let rows = self
            .db
            .query(
                "SELECT player_urn FROM mission_participants WHERE mission_id = $1 AND left_at IS NULL ORDER BY joined_at",
                &[&mission_id],
            )
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(|row| row.get("player_urn")).collect())
    }

    async fn objectives(&self, mission_id: &str, ids: &[String]) -> Result<Vec<MissionObjective>, Status> {
        let rows = self
            .db
            .query(
                "SELECT body FROM mission_objectives
                 WHERE mission_id = $1 AND (cardinality($2::text[]) = 0 OR objective_id = ANY($2))
                 ORDER BY objective_id",
                &[&mission_id, &ids],
            )
            .await
            .map_err(internal)?;
        rows.iter().map(|row| MissionObjective::decode(row.get::<_, &[u8]>("body")).map_err(internal)).collect()
    }

    /// Tells the caller's client how a command ended, on every push topic its arguments name. The account
    /// is always the caller's, so a client can only ever address itself.
    fn notify(&self, account: &Account, arguments: Option<CommandArguments>, context: Option<Any>, status: RpcStatus) {
        let Some(arguments) = arguments else {
            return;
        };
        let body = CommandResponseBody { id: arguments.id, status: Some(status), context };
        let value = body.encode_to_vec();
        let topics = arguments.destinations.into_iter().filter_map(|destination| match destination.destination {
            Some(Destination::Push(push)) => Some(push.topics),
            _ => None,
        });
        for topic in topics.flatten() {
            let request = PublishRequest {
                account_id: account.account_id,
                topic,
                type_name: COMMAND_RESPONSE.into(),
                value: value.clone(),
                wait_for_subscriber_ms: 0,
            };
            let mut push = self.push.clone();
            tokio::spawn(async move {
                if let Err(error) = push.publish(request).await {
                    warn!(%error, "command response not published");
                }
            });
        }
    }

    async fn join(&self, me: &str, request: &JoinMissionRequest) -> Result<(), Status> {
        let id = &request.mission_id;
        if id.is_empty() {
            return Err(Status::invalid_argument("missing mission id"));
        }
        let owner = if request.owner_urn.is_empty() { me } else { &request.owner_urn };
        let mission = match self.mission(id).await? {
            Some(mission) => mission,
            None if owner == me => {
                self.db
                    .execute(
                        "INSERT INTO missions (mission_id, owner_urn, state, active_mission_key)
                         VALUES ($1, $2, $3, gen_random_uuid()::text) ON CONFLICT DO NOTHING",
                        &[id, &owner, &(MissionState::Active as i32)],
                    )
                    .await
                    .map_err(internal)?;
                self.mission(id).await?.ok_or_else(|| Status::internal("mission row disappeared"))?
            }
            None => return Err(Status::not_found(format!("no mission {id}"))),
        };
        if !is_open(mission.state) {
            return Err(Status::failed_precondition("mission is not active"));
        }
        if !may_join(me, &mission.owner, &request.player_urns, self.is_shared_with(id, me).await?) {
            return Err(Status::permission_denied("not invited to this mission"));
        }
        let players = if request.player_urns.is_empty() { vec![me.to_string()] } else { request.player_urns.clone() };
        self.db
            .execute(
                "INSERT INTO mission_participants (mission_id, player_urn) SELECT $1, unnest($2::text[])
                 ON CONFLICT (mission_id, player_urn) DO UPDATE SET left_at = NULL, joined_at = now()",
                &[id, &players],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    async fn leave(&self, me: &str, request: &LeaveMissionRequest) -> Result<(), Status> {
        let id = &request.mission_id;
        let mission = self.mission(id).await?.ok_or_else(|| Status::not_found(format!("no mission {id}")))?;
        let mut requested = MissionState::Unspecified as i32;
        let mut owner_left = request.missions.is_empty() && me == mission.owner;
        if request.missions.iter().any(|leave| !may_remove(me, &mission.owner, &leave.player_urn)) {
            return Err(Status::permission_denied("cannot remove that player"));
        }
        for leave in &request.missions {
            self.db
                .execute(
                    "UPDATE mission_participants SET left_at = now(), reason = $3, end_state = $4, end_reason = $5,
                         analytics_reason = $6
                     WHERE mission_id = $1 AND player_urn = $2 AND left_at IS NULL",
                    &[id, &leave.player_urn, &leave.reason, &leave.end_state, &i64::from(leave.end_reason), &leave.analytics_reason],
                )
                .await
                .map_err(internal)?;
            requested = leave.end_state;
            owner_left |= leave.player_urn == mission.owner;
        }
        if request.missions.is_empty() {
            self.db
                .execute(
                    "UPDATE mission_participants SET left_at = now()
                     WHERE mission_id = $1 AND player_urn = $2 AND left_at IS NULL",
                    &[id, &me],
                )
                .await
                .map_err(internal)?;
        }
        self.db
            .execute(
                "UPDATE missions SET state = $2 WHERE mission_id = $1 AND state = ANY($3)
                   AND ($4 OR NOT EXISTS (
                       SELECT 1 FROM mission_participants WHERE mission_id = $1 AND left_at IS NULL))",
                &[id, &end_state(requested), &&OPEN_STATES[..], &owner_left],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    async fn share(&self, me: &str, request: &ShareMissionRequest) -> Result<(), Status> {
        let id = &request.mission_id;
        let mission = self.mission(id).await?.ok_or_else(|| Status::not_found(format!("no mission {id}")))?;
        if mission.owner != me {
            return Err(Status::permission_denied("not your mission"));
        }
        if !is_open(mission.state) {
            return Err(Status::failed_precondition("mission is not active"));
        }
        self.db
            .execute(
                "INSERT INTO mission_shares (mission_id, player_urn) SELECT $1, unnest($2::text[])
                 ON CONFLICT DO NOTHING",
                &[id, &request.player_urns],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }
}

#[tonic::async_trait]
impl MissionService for Missions {
    async fn query_active_missions(
        &self,
        request: Request<QueryActiveMissionsRequest>,
    ) -> Result<Response<QueryActiveMissionsResponse>, Status> {
        self.account(request.metadata())?;
        let urns: Vec<String> = request.into_inner().queries.into_iter().map(|query| query.urn).collect();
        let rows = self
            .db
            .query(
                "SELECT DISTINCT m.mission_id, m.active_mission_key FROM missions m
                 LEFT JOIN mission_participants p ON p.mission_id = m.mission_id AND p.left_at IS NULL
                 WHERE (m.owner_urn = ANY($1) OR p.player_urn = ANY($1)) AND m.state = ANY($2)
                   AND (m.expires_at IS NULL OR m.expires_at > now())",
                &[&urns, &&OPEN_STATES[..]],
            )
            .await
            .map_err(internal)?;
        let mission_ids = rows
            .iter()
            .map(|row| query_active_missions_response::Body {
                mission_id: row.get("mission_id"),
                active_mission_key: row.get("active_mission_key"),
            })
            .collect();
        Ok(Response::new(QueryActiveMissionsResponse { mission_ids }))
    }

    async fn query_missions(&self, request: Request<QueryMissionsRequest>) -> Result<Response<QueryMissionsResponse>, Status> {
        self.account(request.metadata())?;
        let ids: Vec<String> = request.into_inner().queries.into_iter().map(|query| query.mission_id).collect();
        let rows = self
            .db
            .query(
                "SELECT mission_id, contract_id, owner_urn, state, template, reward,
                        (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
                        (extract(epoch FROM expires_at) * 1000)::bigint AS expiry_ms
                 FROM missions WHERE mission_id = ANY($1)",
                &[&ids],
            )
            .await
            .map_err(internal)?;
        let mut missions = Vec::with_capacity(rows.len());
        for row in &rows {
            let mission_id: String = row.get("mission_id");
            let participants = self.participants(&mission_id).await?;
            let mission_objectives = self.objectives(&mission_id, &[]).await?;
            missions.push(ActiveMissionInfo {
                contract_id: row.get("contract_id"),
                mission_template: decode_opt::<MissionTemplate>(row, "template")?,
                mission_owner_urn: row.get("owner_urn"),
                participants: participants
                    .into_iter()
                    .map(|player_urn| MissionParticipant { player_urn, share: Vec::new() })
                    .collect(),
                creation: Some(timestamp(row.get("created_ms"))),
                expiry: row.get::<_, Option<i64>>("expiry_ms").map(timestamp),
                mission_state: row.get("state"),
                mission_objectives,
                reward: decode_opt::<MissionReward>(row, "reward")?,
                mission_id,
            });
        }
        Ok(Response::new(QueryMissionsResponse { missions }))
    }

    async fn join_mission(&self, request: Request<JoinMissionRequest>) -> Result<Response<JoinMissionResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let result = self.join(&player_urn(account.geid), &request).await;
        self.notify(&account, request.arguments, request.context, rpc_status(&result));
        result.map(|()| Response::new(JoinMissionResponse::default()))
    }

    async fn leave_mission(&self, request: Request<LeaveMissionRequest>) -> Result<Response<LeaveMissionResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let result = self.leave(&player_urn(account.geid), &request).await;
        self.notify(&account, request.arguments, request.context, rpc_status(&result));
        result.map(|()| Response::new(LeaveMissionResponse::default()))
    }

    async fn share_mission(&self, request: Request<ShareMissionRequest>) -> Result<Response<ShareMissionResponse>, Status> {
        let account = self.account(request.metadata())?;
        let request = request.into_inner();
        let result = self.share(&player_urn(account.geid), &request).await;
        self.notify(&account, request.arguments, request.context, rpc_status(&result));
        result.map(|()| Response::new(ShareMissionResponse::default()))
    }

    async fn query_mission_objectives(
        &self,
        request: Request<QueryMissionObjectivesRequest>,
    ) -> Result<Response<QueryMissionObjectivesResponse>, Status> {
        self.account(request.metadata())?;
        let mut objectives = Vec::new();
        for query in request.into_inner().queries {
            objectives.extend(self.objectives(&query.mission_id, &query.mission_objective_ids).await?);
        }
        Ok(Response::new(QueryMissionObjectivesResponse { objectives }))
    }

    async fn query_mission_phases(
        &self,
        request: Request<QueryMissionPhasesRequest>,
    ) -> Result<Response<QueryMissionPhasesResponse>, Status> {
        self.account(request.metadata())?;
        let mut phases = Vec::new();
        for query in request.into_inner().queries {
            let rows = self
                .db
                .query(
                    "SELECT body FROM mission_phases
                     WHERE mission_id = $1 AND (cardinality($2::text[]) = 0 OR phase_id = ANY($2))
                     ORDER BY phase_id",
                    &[&query.mission_id, &query.mission_phase_ids],
                )
                .await
                .map_err(internal)?;
            for row in &rows {
                phases.push(MissionPhase::decode(row.get::<_, &[u8]>("body")).map_err(internal)?);
            }
        }
        Ok(Response::new(QueryMissionPhasesResponse { phases }))
    }

    async fn check_mission_sharing_eligibility(
        &self,
        request: Request<CheckMissionSharingEligibilityRequest>,
    ) -> Result<Response<CheckMissionSharingEligibilityResponse>, Status> {
        self.account(request.metadata())?;
        let request = request.into_inner();
        let open = self.mission(&request.mission_id).await?.is_some_and(|mission| is_open(mission.state));
        let participants = self.participants(&request.mission_id).await?;
        let mut reply = CheckMissionSharingEligibilityResponse::default();
        for player_urn in request.player_urns {
            match share_refusal(&player_urn, open, participants.contains(&player_urn)) {
                Some(reason) => reply.not_eligible_players.push(NotEligiblePlayer { player_urn, reason: reason.into() }),
                None => reply.eligible_player_urns.push(player_urn),
            }
        }
        Ok(Response::new(reply))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let endpoint = Endpoint::from_shared(args.push.clone()).map_err(|e| format!("--push: {e}"))?;
    let service = Missions {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        push: PushPublisherClient::new(endpoint.connect_lazy()),
    };
    let server = MissionServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "mission service listening");
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
    fn urns_round_trip() {
        assert!(is_player_urn(&player_urn(7)));
        assert!(!is_player_urn("player:7"));
    }

    #[test]
    fn joins_need_the_owner_or_a_share() {
        let me = vec!["me".to_string()];
        let other = vec!["other".to_string()];
        assert!(may_join("owner", "owner", &other, false));
        assert!(may_join("me", "owner", &me, true));
        assert!(!may_join("me", "owner", &me, false));
        assert!(!may_join("me", "owner", &other, true));
    }

    #[test]
    fn players_leave_for_themselves() {
        assert!(may_remove("me", "owner", "me"));
        assert!(may_remove("owner", "owner", "me"));
        assert!(!may_remove("me", "owner", "other"));
    }

    #[test]
    fn share_refusals() {
        let player = player_urn(1);
        assert_eq!(share_refusal("nonsense", true, false), Some("invalid player"));
        assert_eq!(share_refusal(&player, false, false), Some("mission is not active"));
        assert_eq!(share_refusal(&player, true, true), Some("already in the mission"));
        assert_eq!(share_refusal(&player, true, false), None);
    }

    #[test]
    fn a_mission_never_ends_in_an_open_state() {
        assert_eq!(end_state(MissionState::Active as i32), MissionState::Ended as i32);
        assert_eq!(end_state(MissionState::Unspecified as i32), MissionState::Ended as i32);
        assert_eq!(end_state(MissionState::Failed as i32), MissionState::Failed as i32);
    }

    #[test]
    fn command_status_mirrors_the_result() {
        assert_eq!(rpc_status(&Ok(())).code, 0);
        let failed = rpc_status(&Err(Status::not_found("gone")));
        assert_eq!((failed.code, failed.message.as_str()), (5, "gone"));
    }
}
