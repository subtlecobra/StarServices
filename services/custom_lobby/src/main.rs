//! sc.external.services.custom_lobby.v1.CustomLobbyService. A lobby is a social group, and each of its
//! teams a child group, so team chat and voice work like any other group's. FindMatch opens a
//! matchmaking ticket for the leader and marks the lobby as started.
//!
//! sc-custom-lobby owns `custom_lobbies`. The lobby and its teams are sc-social's groups, which it reads
//! and changes through SocialInternal; when sc-social destroys a lobby it has its row deleted through
//! scbackend.internal.GroupDataInternal, served on the same port.

mod password;

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs};
use sc_proto::{
    internal::{
        group_data_internal_server::{GroupDataInternal, GroupDataInternalServer},
        DeleteGroupDataRequest, DeleteGroupDataResponse,
    },
    services::{
        custom_lobby::v1::{
            custom_lobby_service_server::{CustomLobbyService, CustomLobbyServiceServer},
            ChangeTeamRequest, ChangeTeamResponse, ChangeTeammateTeamRequest, ChangeTeammateTeamResponse,
            CreateRequest, CreateResponse, CustomLobby, CustomLobbyMemberJoinedTeam, CustomLobbySettings,
            CustomLobbySettingsChanged, CustomLobbyTeam, FindMatchRequest, FindMatchResponse, GetRequest, GetResponse,
            JoinRequest, JoinResponse, UpdateSettingsRequest, UpdateSettingsResponse,
        },
        matchmaking::v1::MatchRequest,
        social::v1::{Group, GroupType},
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups::{self, GroupView},
    matchmaking::{self, Matchmaking},
    modes,
    peers::PeerArgs,
    push::{Event, CUSTOM_LOBBY},
    tx::Queries,
    urn, Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::{info, warn};

/// sc-custom-lobby's table, which it alone reads and writes.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS custom_lobbies (
    group_id TEXT PRIMARY KEY,
    settings BYTEA NOT NULL,
    password_hash BYTEA,
    started BOOLEAN NOT NULL DEFAULT false
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
    /// Address of the matchmaking service.
    #[arg(long, env = "SCBACKEND_MATCHMAKING", default_value = "http://sc-matchmaking:50051")]
    matchmaking: String,
    #[command(flatten)]
    peers: PeerArgs,
}

struct Lobbies {
    ctx: Context,
    matchmaking: Matchmaking,
}

fn teams_wanted(settings: &CustomLobbySettings) -> usize {
    modes::find(&settings.mode).teams.max(1) as usize
}

const SET_LOBBY: &str = "
INSERT INTO custom_lobbies (group_id, settings, password_hash) VALUES ($1, $2, $3)
ON CONFLICT (group_id) DO UPDATE SET settings = excluded.settings, password_hash = excluded.password_hash";

fn ticket_request(lobby: &Group, settings: &CustomLobbySettings) -> MatchRequest {
    let region = matchmaking::region(&settings.region);
    matchmaking::request(&lobby.id, modes::find(&settings.mode), &settings.map, region)
}

async fn lobby_settings(db: &impl Queries, id: &str) -> Result<(CustomLobbySettings, bool), Status> {
    let row = db
        .query_opt("SELECT settings, started FROM custom_lobbies WHERE group_id = $1", &[&id])
        .await?
        .ok_or_else(|| Status::not_found(format!("no lobby {id}")))?;
    let settings: Vec<u8> = row.get("settings");
    Ok((CustomLobbySettings::decode(settings.as_slice()).map_err(internal)?, row.get("started")))
}

impl Lobbies {
    async fn delete_groups(&self, ids: &[String]) -> Result<u64, Status> {
        self.ctx.execute("DELETE FROM custom_lobbies WHERE group_id = ANY($1)", &[&ids]).await
    }

    /// After writing a lobby's row: a lobby that sc-social destroyed meanwhile has had its row deleted
    /// already, so this one goes too.
    async fn check_lobby(&self, id: &str) -> Result<(), Status> {
        if self.ctx.peers.gone(id).await {
            self.delete_groups(&[id.to_string()]).await?;
            return Err(Status::not_found(format!("no lobby {id}")));
        }
        Ok(())
    }

    /// The lobby that a team belongs to.
    async fn lobby_of_team(&self, team_id: &str) -> Result<GroupView, Status> {
        let team = self.ctx.peers.group(team_id).await?;
        let lobby = urn::group_id(&team.group.parent_urn).ok_or_else(|| Status::invalid_argument("not a team"))?;
        self.ctx.peers.group(lobby).await
    }

    /// Moves a member of the lobby into one of its teams, and tells the lobby.
    async fn move_to_team(&self, destination: &str, geid: u64) -> Result<(), Status> {
        let (lobby, previous) = self.ctx.peers.move_to_team(destination, geid).await?;
        if previous == destination {
            return Ok(());
        }
        let joined = CustomLobbyMemberJoinedTeam {
            group_id: lobby.group.id.clone(),
            team_id: destination.to_string(),
            member_urn: urn::player(geid),
            previous_team_id: previous,
        };
        let event = Event::new(CUSTOM_LOBBY, "CustomLobbyMemberJoinedTeam", &joined);
        groups::to_members(&self.ctx, &lobby.group.id, &lobby.members, &event);
        Ok(())
    }

    async fn teams(&self, id: &str) -> Result<Vec<CustomLobbyTeam>, Status> {
        let teams = self.ctx.peers.teams(id).await?;
        Ok(teams
            .into_iter()
            .map(|team| CustomLobbyTeam { group_id: team.group.id, members: team.group.members })
            .collect())
    }

    async fn view(&self, id: &str) -> Result<CustomLobby, Status> {
        let (settings, started) = lobby_settings(&self.ctx, id).await?;
        let map_game_rules = modes::rules(modes::find(&settings.mode));
        Ok(CustomLobby {
            group_id: id.to_string(),
            started,
            settings: Some(settings),
            map_game_rules: Some(map_game_rules),
            teams: self.teams(id).await?,
        })
    }

    async fn publish_settings(&self, lobby: &Group) -> Result<(), Status> {
        let view = self.view(&lobby.id).await?;
        let changed = CustomLobbySettingsChanged {
            group_id: view.group_id,
            settings: view.settings,
            map_game_rules: view.map_game_rules,
            teams: view.teams,
        };
        let members = self.ctx.peers.group(&lobby.id).await?.members;
        let event = Event::new(CUSTOM_LOBBY, "CustomLobbySettingsChanged", &changed);
        groups::to_members(&self.ctx, &lobby.id, &members, &event);
        Ok(())
    }

    async fn own_lobby(&self, account: &Account) -> Result<Group, Status> {
        let lobby = self.ctx.peers.one_of(account.geid, GroupType::CustomLobby).await?;
        lobby.map(|view| view.group).ok_or_else(|| Status::failed_precondition("not in a lobby"))
    }

    /// Checks a join's password. A hash of the old kind is replaced now that the password is known.
    async fn check_password(&self, id: &str, given: &str) -> Result<(), Status> {
        let row = self.ctx.query_opt("SELECT password_hash FROM custom_lobbies WHERE group_id = $1", &[&id]).await?;
        let stored: Option<Vec<u8>> = row.and_then(|row| row.get("password_hash"));
        let stored = stored.ok_or_else(|| Status::permission_denied("wrong password"))?;
        if !password::verify(id, given, &stored).await? {
            return Err(Status::permission_denied("wrong password"));
        }
        if password::is_outdated(&stored) {
            let hash = password::hash(given).await?;
            self.ctx
                .execute(
                    "UPDATE custom_lobbies SET password_hash = $2 WHERE group_id = $1 AND password_hash = $3",
                    &[&id, &hash, &stored],
                )
                .await?;
        }
        Ok(())
    }

    /// The row and teams of a lobby sc-social has just made, with its creator in the first team.
    async fn set_up(&self, lobby: &Group, account: &Account, settings: &CustomLobbySettings, hash: Option<Vec<u8>>) -> Result<(), Status> {
        self.ctx.execute(SET_LOBBY, &[&lobby.id, &settings.encode_to_vec(), &hash]).await?;
        self.check_lobby(&lobby.id).await?;
        self.ctx.peers.sync_teams(&lobby.id, teams_wanted(settings), GroupType::CustomLobbyTeam).await?;
        if let Some(team) = self.ctx.peers.teams(&lobby.id).await?.first() {
            self.ctx.peers.join_group(&team.group.id, account.geid).await?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl CustomLobbyService for Lobbies {
    async fn change_team(&self, request: Request<ChangeTeamRequest>) -> Result<Response<ChangeTeamResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let destination = request.into_inner().destination_team_id;
        let lobby = self.lobby_of_team(&destination).await?.group;
        if !groups::is_member(&lobby, account.geid) {
            return Err(Status::permission_denied("not in this lobby"));
        }
        let (settings, _) = lobby_settings(ctx, &lobby.id).await?;
        if !settings.player_can_change_team && !groups::can_manage(&lobby, account.geid) {
            return Err(Status::permission_denied("players cannot change team in this lobby"));
        }
        self.move_to_team(&destination, account.geid).await?;
        Ok(Response::new(ChangeTeamResponse {}))
    }

    async fn change_teammate_team(
        &self,
        request: Request<ChangeTeammateTeamRequest>,
    ) -> Result<Response<ChangeTeammateTeamResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let lobby = self.lobby_of_team(&request.destination_team_id).await?.group;
        if !groups::can_manage(&lobby, account.geid) {
            return Err(Status::permission_denied("only the lobby leader can move players"));
        }
        let teammate = ctx.directory.by_urn(&request.member_urn)?;
        if !groups::is_member(&lobby, teammate.geid) {
            return Err(Status::failed_precondition(format!("{} is not in this lobby", teammate.handle)));
        }
        self.move_to_team(&request.destination_team_id, teammate.geid).await?;
        Ok(Response::new(ChangeTeammateTeamResponse {}))
    }

    /// Anyone may look at a public lobby. A private one is for its members.
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().group_id;
        let group = ctx.peers.group(&id).await?.group;
        let lobby = self.view(&id).await?;
        let public = lobby.settings.as_ref().is_some_and(|s| s.is_public);
        if !public && !groups::is_member(&group, account.geid) {
            return Err(Status::permission_denied("this lobby is private"));
        }
        Ok(Response::new(GetResponse { custom_lobby: Some(lobby) }))
    }

    /// A lobby that cannot be set up completely is ended again.
    async fn create(&self, request: Request<CreateRequest>) -> Result<Response<CreateResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let mut settings = request.settings.unwrap_or_default();
        settings.is_password_enabled = !request.password.is_empty();
        let hash = match request.password.as_str() {
            "" => None,
            given => Some(password::hash(given).await?),
        };
        let lobby = ctx.peers.create_group(GroupType::CustomLobby, "", "", vec![account.geid]).await?.group;
        if let Err(status) = self.set_up(&lobby, &account, &settings, hash).await {
            if let Err(error) = ctx.peers.destroy_group(&lobby.id).await {
                warn!(lobby = %lobby.id, %error, "lobby left half set up");
            }
            return Err(status);
        }
        let custom_lobby = self.view(&lobby.id).await?;
        Ok(Response::new(CreateResponse { custom_lobby: Some(custom_lobby) }))
    }

    /// An empty password keeps the current one while the lobby stays protected.
    async fn update_settings(
        &self,
        request: Request<UpdateSettingsRequest>,
    ) -> Result<Response<UpdateSettingsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let lobby = self.own_lobby(&account).await?;
        groups::require_leader(&lobby, account.geid)?;
        let settings = request.settings.unwrap_or_default();
        let keep_password = request.password.is_empty() && settings.is_password_enabled;
        let hash = if settings.is_password_enabled && !keep_password {
            Some(password::hash(&request.password).await?)
        } else {
            None
        };
        if keep_password {
            ctx.execute("UPDATE custom_lobbies SET settings = $2 WHERE group_id = $1", &[&lobby.id, &settings.encode_to_vec()])
                .await?;
        } else {
            ctx.execute(SET_LOBBY, &[&lobby.id, &settings.encode_to_vec(), &hash]).await?;
        }
        self.check_lobby(&lobby.id).await?;
        ctx.peers.sync_teams(&lobby.id, teams_wanted(&settings), GroupType::CustomLobbyTeam).await?;
        self.publish_settings(&lobby).await?;
        Ok(Response::new(UpdateSettingsResponse {}))
    }

    /// The lobby only counts as started once matchmaking has taken the ticket.
    async fn find_match(&self, request: Request<FindMatchRequest>) -> Result<Response<FindMatchResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let lobby = self.own_lobby(&account).await?;
        groups::require_leader(&lobby, account.geid)?;
        let (settings, _) = lobby_settings(ctx, &lobby.id).await?;
        if settings.enforce_ready && !lobby.is_ready {
            return Err(Status::failed_precondition("not everyone is ready"));
        }
        self.matchmaking.open_ticket(request.metadata(), ticket_request(&lobby, &settings)).await?;
        ctx.db
            .execute("UPDATE custom_lobbies SET started = true WHERE group_id = $1", &[&lobby.id])
            .await
            .map_err(internal)?;
        self.publish_settings(&lobby).await?;
        Ok(Response::new(FindMatchResponse {}))
    }

    async fn join(&self, request: Request<JoinRequest>) -> Result<Response<JoinResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let group = ctx.peers.group(&request.group_id).await?.group;
        let (settings, _) = lobby_settings(ctx, &group.id).await?;
        if settings.is_password_enabled {
            self.check_password(&group.id, &request.password).await?;
        } else if !settings.is_public {
            return Err(Status::permission_denied("this lobby takes invited players only"));
        }
        ctx.peers.join_group(&group.id, account.geid).await?;
        Ok(Response::new(JoinResponse {}))
    }
}

/// sc-social, which destroyed these lobbies, has their rows deleted.
#[tonic::async_trait]
impl GroupDataInternal for Lobbies {
    async fn delete_group_data(
        &self,
        request: Request<DeleteGroupDataRequest>,
    ) -> Result<Response<DeleteGroupDataResponse>, Status> {
        let deleted = self.delete_groups(&request.into_inner().group_ids).await?;
        Ok(Response::new(DeleteGroupDataResponse { deleted }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Lobbies {
        ctx: Context::open(&args.common, &args.database, &args.push, &args.peers, TABLES).await?,
        matchmaking: Matchmaking::connect(&args.matchmaking)?,
    });
    let server = CustomLobbyServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = GroupDataInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "custom lobby listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::{
        common::{game::v1::StarMarineMode, network::v1::Region},
        services::matchmaking::v1::match_request,
    };
    use sc_social::testing::{fake_matchmaking, nowhere, serve, SocialProcess, TestDb};
    use tonic::{service::Routes, Code};

    const NO_MATCHMAKING: &str = "http://127.0.0.1:9";

    /// Lobbies on the test's schema, asking the real sc-social, which deletes a destroyed lobby's row
    /// through a lobby service of its own.
    async fn with_social(db: &TestDb) -> Option<(Lobbies, SocialProcess)> {
        let connect = || Matchmaking::connect(NO_MATCHMAKING).unwrap();
        let owner = Arc::new(Lobbies { ctx: db.context(TABLES).await, matchmaking: connect() });
        let address = serve(Routes::new(GroupDataInternalServer::from_arc(owner))).await;
        let peers = PeerArgs { group_data: format!("custom_lobby={address}"), ..nowhere() };
        let social = SocialProcess::spawn(db, &peers).await?;
        let lobbies = Lobbies { ctx: db.context_with(TABLES, &social.peers()).await, matchmaking: connect() };
        Some((lobbies, social))
    }

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    async fn stored_hash(lobbies: &Lobbies, id: &str) -> Vec<u8> {
        let query = "SELECT password_hash FROM custom_lobbies WHERE group_id = $1";
        lobbies.ctx.query_opt(query, &[&id]).await.unwrap().unwrap().get("password_hash")
    }

    async fn new_lobby(lobbies: &Lobbies, token: &str, password: &str) -> String {
        let settings = CustomLobbySettings { mode: "EA_Duel".to_string(), ..Default::default() };
        let create = CreateRequest { settings: Some(settings), password: password.to_string() };
        lobbies.create(signed(create, token)).await.unwrap().into_inner().custom_lobby.unwrap().group_id
    }

    fn join(id: &str, password: &str, token: &str) -> Request<JoinRequest> {
        signed(JoinRequest { group_id: id.to_string(), password: password.to_string() }, token)
    }

    #[tokio::test]
    async fn a_password_gates_the_lobby_and_only_its_hash_is_stored() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let Some((lobbies, social)) = with_social(&db).await else { return db.drop_schema().await };
        let leader = lobbies.ctx.subscription_key(&db.accounts[0]);
        let guest = lobbies.ctx.subscription_key(&db.accounts[1]);
        let id = new_lobby(&lobbies, &leader, "letmein").await;

        let hash = stored_hash(&lobbies, &id).await;
        assert!(!password::is_outdated(&hash));
        assert!(!hash.windows(7).any(|window| window == b"letmein"));

        let refused = lobbies.join(join(&id, "wrong", &guest)).await.unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied);
        lobbies.join(join(&id, "letmein", &guest)).await.unwrap();

        let group = lobbies.ctx.peers.group(&id).await.unwrap().group;
        assert!(groups::is_member(&group, db.accounts[1].geid));
        let teams = lobbies.ctx.peers.teams(&id).await.unwrap();
        assert_eq!(teams.len(), 2);
        assert_eq!(teams.iter().map(|team| team.members.len()).sum::<usize>(), 2);
        drop(social);
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn a_hash_from_before_works_once_and_is_replaced() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let Some((lobbies, social)) = with_social(&db).await else { return db.drop_schema().await };
        let leader = lobbies.ctx.subscription_key(&db.accounts[0]);
        let guest = lobbies.ctx.subscription_key(&db.accounts[1]);
        let id = new_lobby(&lobbies, &leader, "first").await;
        lobbies
            .ctx
            .execute(
                "UPDATE custom_lobbies SET password_hash = sha256(convert_to($1 || $2, 'UTF8')) WHERE group_id = $1",
                &[&id, &"old-secret"],
            )
            .await
            .unwrap();
        assert!(password::is_outdated(&stored_hash(&lobbies, &id).await));

        assert!(lobbies.join(join(&id, "first", &guest)).await.is_err());
        lobbies.join(join(&id, "old-secret", &guest)).await.unwrap();

        let upgraded = stored_hash(&lobbies, &id).await;
        assert!(!password::is_outdated(&upgraded));
        assert!(password::verify(&id, "old-secret", &upgraded).await.unwrap());
        drop(social);
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn find_match_starts_the_lobby_only_once_the_ticket_exists() {
        let Some((db, _)) = TestDb::open(1, TABLES).await else { return };
        let Some((mut lobbies, social)) = with_social(&db).await else { return db.drop_schema().await };
        let token = lobbies.ctx.subscription_key(&db.accounts[0]);
        let id = new_lobby(&lobbies, &token, "").await;

        assert!(lobbies.find_match(signed(FindMatchRequest {}, &token)).await.is_err());
        assert!(!lobby_settings(&lobbies.ctx, &id).await.unwrap().1);

        let (address, tickets) = fake_matchmaking().await;
        lobbies.matchmaking = Matchmaking::connect(&address).unwrap();
        lobbies.find_match(signed(FindMatchRequest {}, &token)).await.unwrap();
        assert!(lobby_settings(&lobbies.ctx, &id).await.unwrap().1);
        let (caller, ticket) = tickets.lock().unwrap().pop().unwrap();
        assert_eq!(caller, format!("Bearer {token}"));
        let Some(match_request::Game::ArenaCommander(arena)) = ticket.game else { panic!("wrong game") };
        assert_eq!(arena.squad_id, id);
        drop(social);
        db.drop_schema().await;
    }

    /// Teams follow the mode, players move between them as the settings allow, and the lobby's row goes
    /// with the group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn teams_follow_the_settings_and_the_row_goes_with_the_lobby() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let Some((lobbies, social)) = with_social(&db).await else { return db.drop_schema().await };
        let (one, two) = (&db.accounts[0], &db.accounts[1]);
        let (leader, guest) = (lobbies.ctx.subscription_key(one), lobbies.ctx.subscription_key(two));
        let id = new_lobby(&lobbies, &leader, "").await;
        let settings = CustomLobbySettings { mode: "EA_Duel".to_string(), is_public: true, ..Default::default() };
        lobbies.join(join(&id, "", &guest)).await.unwrap_err();
        let update = UpdateSettingsRequest { settings: Some(settings.clone()), password: String::new() };
        lobbies.update_settings(signed(update, &leader)).await.unwrap();
        lobbies.join(join(&id, "", &guest)).await.unwrap();

        let teams = lobbies.ctx.peers.teams(&id).await.unwrap();
        let first = teams[0].group.id.clone();
        let refused = lobbies.change_team(signed(ChangeTeamRequest { destination_team_id: first.clone() }, &guest)).await;
        assert_eq!(refused.unwrap_err().code(), Code::PermissionDenied, "players cannot change team");
        let moved = ChangeTeammateTeamRequest { destination_team_id: first.clone(), member_urn: urn::player(two.geid) };
        lobbies.change_teammate_team(signed(moved, &leader)).await.unwrap();
        let view = lobbies.get(signed(GetRequest { group_id: id.clone() }, &guest)).await.unwrap().into_inner();
        let teams = view.custom_lobby.unwrap().teams;
        assert_eq!(teams.iter().map(|team| team.members.len()).collect::<Vec<_>>(), [2, 0]);

        let free = CustomLobbySettings { mode: "EA_FreeFlight".to_string(), ..settings };
        let update = UpdateSettingsRequest { settings: Some(free), password: String::new() };
        lobbies.update_settings(signed(update, &leader)).await.unwrap();
        assert_eq!(lobbies.ctx.peers.teams(&id).await.unwrap().len(), 1);

        lobbies.ctx.peers.destroy_group(&id).await.unwrap();
        for _ in 0..100 {
            if lobby_settings(&lobbies.ctx, &id).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(lobby_settings(&lobbies.ctx, &id).await.unwrap_err().code(), Code::NotFound);
        drop(social);
        db.drop_schema().await;
    }

    #[test]
    fn team_count_follows_the_mode() {
        let settings = |mode: &str| CustomLobbySettings { mode: mode.to_string(), ..Default::default() };
        assert_eq!(teams_wanted(&settings("EA_Duel")), 2);
        assert_eq!(teams_wanted(&settings("EA_FreeFlight")), 1);
    }

    #[test]
    fn a_ticket_follows_the_lobby_settings() {
        let lobby = Group { id: "lobby".to_string(), ..Default::default() };
        let settings = CustomLobbySettings {
            mode: "EA_TeamElimination".to_string(),
            map: "EA_Echo11".to_string(),
            region: "europe".to_string(),
            ..Default::default()
        };
        let request = ticket_request(&lobby, &settings);
        assert_eq!(request.region, Region::Europe as i32);
        let Some(match_request::Game::StarMarine(marine)) = request.game else { panic!("wrong game") };
        assert_eq!(marine.mode, StarMarineMode::TeamElimination as i32);
        assert_eq!(marine.squad_id, "lobby");
        assert_eq!(marine.maps, ["EA_Echo11"]);
    }
}
