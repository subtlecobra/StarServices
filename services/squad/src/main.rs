//! sc.external.services.squad.v1.SquadService. A squad is a social group plus its match settings.
//! FindMatch opens a matchmaking ticket for the leader. AutoRequeue only records the wish, and
//! nothing fills in `match_instance_id` for RecoverMatch yet.
//!
//! sc-squad owns `squads`. The group is sc-social's, which it asks through SocialInternal; when
//! sc-social destroys a group it has the squad's row deleted through
//! scbackend.internal.GroupDataInternal, served on the same port.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs};
use sc_api::internal::{ group_data_internal_server::{GroupDataInternal, GroupDataInternalServer}, DeleteGroupDataRequest, DeleteGroupDataResponse, };
use sc_proto::{
    common::network::v1::Region,
    services::{
    matchmaking::v1::MatchRequest,
    social::v1::GroupType,
    squad::v1::{
    squad_service_server::{SquadService, SquadServiceServer},
    AutoRequeueRequest, AutoRequeueResponse, CreateFromPartyRequest, CreateFromPartyResponse, CreateRequest,
    CreateResponse, FindMatchRequest, FindMatchResponse, GetRequest, GetResponse, RecoverMatchRequest,
    RecoverMatchResponse, Squad, SquadSettings, SquadSettingsChanged, UpdateSettingsRequest,
    UpdateSettingsResponse,
    },
    },
};
use sc_runtime::{state::Account, CommonArgs, MAX_MESSAGE_BYTES};
use sc_social::{
    groups::{self, GroupView},
    matchmaking::{request, Matchmaking},
    modes, now_ms,
    peers::PeerArgs,
    push::{Event, SQUAD},
    tx::Queries,
    Context,
};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::{info, warn};

/// sc-squad's table, which it alone reads and writes.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS squads (
    group_id TEXT PRIMARY KEY,
    settings BYTEA NOT NULL,
    queued_at BIGINT NOT NULL DEFAULT 0,
    auto_requeue BOOLEAN NOT NULL DEFAULT false,
    match_instance_id TEXT NOT NULL DEFAULT ''
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

struct Squads {
    ctx: Context,
    matchmaking: Matchmaking,
}

/// What a squad's settings ask of matchmaking. The settings give a mode as the client's game type.
fn ticket_request(group_id: &str, settings: &SquadSettings) -> MatchRequest {
    let wanted = settings.match_settings.clone().unwrap_or_default();
    request(group_id, modes::find(&wanted.mode.to_string()), &wanted.map, Region::Unspecified)
}

impl Squads {
    /// Stores the settings of a squad. A group that sc-social destroyed meanwhile has had its row
    /// deleted already, so this one goes too.
    async fn store(&self, id: &str, settings: Option<SquadSettings>) -> Result<Squad, Status> {
        let settings = settings.unwrap_or_default();
        self.ctx
            .execute(
                "INSERT INTO squads (group_id, settings) VALUES ($1, $2)
                 ON CONFLICT (group_id) DO UPDATE SET settings = excluded.settings",
                &[&id, &settings.encode_to_vec()],
            )
            .await?;
        if self.ctx.peers.gone(id).await {
            self.delete_groups(&[id.to_string()]).await?;
            return Err(Status::not_found(format!("no group {id}")));
        }
        Ok(Squad { group_id: id.to_string(), settings: Some(settings) })
    }

    /// Stores the settings of a squad sc-social has just made, and has it end the group if that fails,
    /// so that no squad is left without settings.
    async fn store_new(&self, view: &GroupView, settings: Option<SquadSettings>) -> Result<Squad, Status> {
        let stored = self.store(&view.group.id, settings).await;
        if stored.is_err() {
            if let Err(error) = self.ctx.peers.destroy_group(&view.group.id).await {
                warn!(group = %view.group.id, %error, "squad without settings left");
            }
        }
        stored
    }

    async fn delete_groups(&self, ids: &[String]) -> Result<u64, Status> {
        self.ctx.execute("DELETE FROM squads WHERE group_id = ANY($1)", &[&ids]).await
    }

    async fn own_squad(&self, account: &Account) -> Result<GroupView, Status> {
        let squad = self.ctx.peers.one_of(account.geid, GroupType::Squad).await?;
        squad.ok_or_else(|| Status::failed_precondition("not in a squad"))
    }

    async fn led_squad(&self, account: &Account) -> Result<GroupView, Status> {
        let squad = self.own_squad(account).await?;
        groups::require_leader(&squad.group, account.geid)?;
        Ok(squad)
    }
}

#[tonic::async_trait]
impl SquadService for Squads {
    async fn update_settings(
        &self,
        request: Request<UpdateSettingsRequest>,
    ) -> Result<Response<UpdateSettingsResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let request = request.into_inner();
        let view = ctx.peers.member_of(&request.group_id, account.geid).await?;
        groups::require_leader(&view.group, account.geid)?;
        let squad = self.store(&view.group.id, request.settings).await?;
        let changed = SquadSettingsChanged { settings: squad.settings };
        groups::to_members(ctx, &view.group.id, &view.members, &Event::new(SQUAD, "SquadSettingsChanged", &changed));
        Ok(Response::new(UpdateSettingsResponse {}))
    }

    async fn create(&self, request: Request<CreateRequest>) -> Result<Response<CreateResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let settings = request.into_inner().settings;
        let view = ctx.peers.create_group(GroupType::Squad, "", "", vec![account.geid]).await?;
        let squad = self.store_new(&view, settings).await?;
        Ok(Response::new(CreateResponse { squad: Some(squad) }))
    }

    /// The whole party moves in, with its leader leading.
    async fn create_from_party(
        &self,
        request: Request<CreateFromPartyRequest>,
    ) -> Result<Response<CreateFromPartyResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let settings = request.into_inner().settings;
        let party = ctx.peers.one_of(account.geid, GroupType::Party).await?;
        let party = party.ok_or_else(|| Status::failed_precondition("not in a party"))?;
        groups::require_leader(&party.group, account.geid)?;
        let mut geids = vec![account.geid];
        for member in &party.members {
            if member.geid != account.geid && ctx.directory.by_geid(member.geid).is_some() {
                geids.push(member.geid);
            }
        }
        let view = ctx.peers.create_group(GroupType::Squad, &party.group.name, "", geids).await?;
        let squad = self.store_new(&view, settings).await?;
        Ok(Response::new(CreateFromPartyResponse { squad: Some(squad) }))
    }

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let id = request.into_inner().group_id;
        ctx.peers.member_of(&id, account.geid).await?;
        let row = ctx
            .db
            .query_opt("SELECT settings FROM squads WHERE group_id = $1", &[&id])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no squad {id}")))?;
        let settings: Vec<u8> = row.get("settings");
        let settings = SquadSettings::decode(settings.as_slice()).map_err(internal)?;
        Ok(Response::new(GetResponse { squad: Some(Squad { group_id: id, settings: Some(settings) }) }))
    }

    /// `queued_at` is only set once matchmaking has taken the ticket.
    async fn find_match(&self, request: Request<FindMatchRequest>) -> Result<Response<FindMatchResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let squad = self.led_squad(&account).await?.group;
        let row = self
            .ctx
            .db
            .query_opt("SELECT settings FROM squads WHERE group_id = $1", &[&squad.id])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no squad {}", squad.id)))?;
        let settings: Vec<u8> = row.get("settings");
        let settings = SquadSettings::decode(settings.as_slice()).map_err(internal)?;
        self.matchmaking.open_ticket(request.metadata(), ticket_request(&squad.id, &settings)).await?;
        self.ctx
            .db
            .execute("UPDATE squads SET queued_at = $2 WHERE group_id = $1", &[&squad.id, &now_ms()])
            .await
            .map_err(internal)?;
        Ok(Response::new(FindMatchResponse {}))
    }

    async fn auto_requeue(
        &self,
        request: Request<AutoRequeueRequest>,
    ) -> Result<Response<AutoRequeueResponse>, Status> {
        let account = self.ctx.caller(request.metadata())?;
        let squad = self.led_squad(&account).await?.group;
        self.ctx
            .db
            .execute("UPDATE squads SET auto_requeue = true WHERE group_id = $1", &[&squad.id])
            .await
            .map_err(internal)?;
        Ok(Response::new(AutoRequeueResponse {}))
    }

    /// Without `rejoin` the player gives the match up.
    async fn recover_match(
        &self,
        request: Request<RecoverMatchRequest>,
    ) -> Result<Response<RecoverMatchResponse>, Status> {
        let ctx = &self.ctx;
        let account = ctx.caller(request.metadata())?;
        let rejoin = request.into_inner().rejoin;
        let squad = self.own_squad(&account).await?.group;
        let row = ctx
            .db
            .query_opt("SELECT match_instance_id FROM squads WHERE group_id = $1", &[&squad.id])
            .await
            .map_err(internal)?;
        let match_instance_id: String = row.map_or_else(String::new, |row| row.get("match_instance_id"));
        if !rejoin {
            ctx.db
                .execute("UPDATE squads SET match_instance_id = '' WHERE group_id = $1", &[&squad.id])
                .await
                .map_err(internal)?;
        }
        Ok(Response::new(RecoverMatchResponse { match_instance_id }))
    }
}

/// sc-social, which destroyed these groups, has their squad rows deleted.
#[tonic::async_trait]
impl GroupDataInternal for Squads {
    async fn delete_group_data(
        &self,
        request: Request<DeleteGroupDataRequest>,
    ) -> Result<Response<DeleteGroupDataResponse>, Status> {
        let deleted = self.delete_groups(&request.into_inner().group_ids).await?;
        Ok(Response::new(DeleteGroupDataResponse { deleted }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Squads {
        ctx: Context::open(&args.common, &args.database, &args.push, &args.peers, TABLES).await?,
        matchmaking: Matchmaking::connect(&args.matchmaking)?,
    });
    let server = SquadServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = GroupDataInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, push = %args.push, "squad listening");
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
        common::game::v1::ArenaCommanderMode,
        services::{matchmaking::v1::match_request, r#match::v1::MatchSettings},
    };
    use sc_social::testing::{fake_matchmaking, nowhere, serve, SocialProcess, TestDb};
    use tonic::service::Routes;

    /// Squads on the test's schema, asking the real sc-social, which deletes a destroyed squad's row through
    /// a squad service of its own.
    async fn with_social(db: &TestDb, matchmaking: &str) -> Option<(Squads, SocialProcess)> {
        let connect = || Matchmaking::connect(matchmaking).unwrap();
        let owner = Arc::new(Squads { ctx: db.context(TABLES).await, matchmaking: connect() });
        let address = serve(Routes::new(GroupDataInternalServer::from_arc(owner))).await;
        let peers = PeerArgs { group_data: format!("squad={address}"), ..nowhere() };
        let social = SocialProcess::spawn(db, &peers).await?;
        let squads = Squads { ctx: db.context_with(TABLES, &social.peers()).await, matchmaking: connect() };
        Some((squads, social))
    }

    #[test]
    fn a_ticket_follows_the_squad_settings() {
        let settings = SquadSettings {
            match_settings: Some(MatchSettings { mode: 11, map: "EA_Kareah".to_string(), ..Default::default() }),
        };
        let request = ticket_request("squad", &settings);
        let Some(match_request::Game::ArenaCommander(arena)) = request.game else { panic!("wrong game") };
        assert_eq!(arena.mode, ArenaCommanderMode::Duel as i32);
        assert_eq!(arena.squad_id, "squad");
        assert_eq!(arena.maps, ["EA_Kareah"]);
    }

    fn signed<T>(message: T, token: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    async fn queued_at(squads: &Squads, id: &str) -> i64 {
        let row = squads.ctx.query_opt("SELECT queued_at FROM squads WHERE group_id = $1", &[&id]).await.unwrap();
        row.unwrap().get("queued_at")
    }

    #[tokio::test]
    async fn find_match_opens_a_ticket_in_the_leaders_name() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let (address, tickets) = fake_matchmaking().await;
        let Some((squads, social)) = with_social(&db, &address).await else { return db.drop_schema().await };
        let token = squads.ctx.subscription_key(&db.accounts[0]);
        let settings = SquadSettings { match_settings: Some(MatchSettings { mode: 7, ..Default::default() }) };
        let created = squads.create(signed(CreateRequest { settings: Some(settings) }, &token)).await.unwrap();
        let id = created.into_inner().squad.unwrap().group_id;

        squads.find_match(signed(FindMatchRequest {}, &token)).await.unwrap();

        let (caller, ticket) = tickets.lock().unwrap().pop().unwrap();
        assert_eq!(caller, format!("Bearer {token}"));
        let Some(match_request::Game::StarMarine(marine)) = ticket.game else { panic!("wrong game") };
        assert_eq!(marine.squad_id, id);
        assert!(queued_at(&squads, &id).await > 0);

        let stranger = squads.ctx.subscription_key(&db.accounts[1]);
        let refused = squads.find_match(signed(FindMatchRequest {}, &stranger)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(tickets.lock().unwrap().is_empty());
        drop(social);
        db.drop_schema().await;
    }

    #[tokio::test]
    async fn a_refused_ticket_leaves_the_squad_unqueued() {
        let Some((db, _)) = TestDb::open(1, TABLES).await else { return };
        let Some((squads, social)) = with_social(&db, "http://127.0.0.1:9").await else { return db.drop_schema().await };
        let token = squads.ctx.subscription_key(&db.accounts[0]);
        let created = squads.create(signed(CreateRequest { settings: None }, &token)).await.unwrap();
        let id = created.into_inner().squad.unwrap().group_id;
        assert!(squads.find_match(signed(FindMatchRequest {}, &token)).await.is_err());
        assert_eq!(queued_at(&squads, &id).await, 0);
        drop(social);
        db.drop_schema().await;
    }

    /// The real service, when `SCBACKEND_TEST_MATCHMAKING_BIN` names its binary. It gets no shard
    /// directory, so the ticket is stored and then fails without reaching a shard.
    #[tokio::test]
    async fn the_real_matchmaking_stores_the_ticket() {
        let Ok(binary) = std::env::var("SCBACKEND_TEST_MATCHMAKING_BIN") else { return };
        let Some((db, _)) = TestDb::open(1, TABLES).await else { return };
        let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut service = std::process::Command::new(binary)
            .env("SCBACKEND_POSTGRES", &db.url)
            .env("SCBACKEND_DATA", &db.data)
            .env("SCBACKEND_LISTEN", listen.to_string())
            .env("SCBACKEND_PUSH", "http://127.0.0.1:9")
            .env("SCBACKEND_SHARD_DIRECTORY", "http://127.0.0.1:9")
            .spawn()
            .unwrap();
        while tokio::net::TcpStream::connect(listen).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let Some((squads, social)) = with_social(&db, &format!("http://{listen}")).await else { return };
        let token = squads.ctx.subscription_key(&db.accounts[0]);
        squads.create(signed(CreateRequest { settings: None }, &token)).await.unwrap();

        let outcome = squads.find_match(signed(FindMatchRequest {}, &token)).await;
        let stored = squads.ctx.query("SELECT geid FROM matchmaking_tickets", &[]).await;
        service.kill().unwrap();
        service.wait().unwrap();

        outcome.unwrap();
        let stored = stored.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].get::<_, i64>("geid"), db.accounts[0].geid as i64);
        drop(social);
        db.drop_schema().await;
    }

    /// A party becomes a squad with its leader leading, and the squad's row goes with the group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_party_becomes_a_squad_and_its_row_goes_with_the_group() {
        let Some((db, _)) = TestDb::open(2, TABLES).await else { return };
        let Some((squads, social)) = with_social(&db, "http://127.0.0.1:9").await else { return db.drop_schema().await };
        let (one, two) = (&db.accounts[0], &db.accounts[1]);
        let peers = &squads.ctx.peers;
        peers.create_group(GroupType::Party, "crew", "", vec![one.geid, two.geid]).await.unwrap();
        let (token_one, token_two) = (squads.ctx.subscription_key(one), squads.ctx.subscription_key(two));
        let not_leader = squads.create_from_party(signed(CreateFromPartyRequest { settings: None }, &token_two)).await;
        assert_eq!(not_leader.unwrap_err().code(), tonic::Code::PermissionDenied);
        let made = squads.create_from_party(signed(CreateFromPartyRequest { settings: None }, &token_one)).await;
        let id = made.unwrap().into_inner().squad.unwrap().group_id;
        let squad = peers.group(&id).await.unwrap();
        assert_eq!(squad.group.name, "crew");
        assert!(groups::is_leader(&squad.group, one.geid) && groups::is_member(&squad.group, two.geid));

        let settings = SquadSettings { match_settings: Some(MatchSettings { mode: 7, ..Default::default() }) };
        let update = UpdateSettingsRequest { group_id: id.clone(), settings: Some(settings.clone()) };
        squads.update_settings(signed(update, &token_one)).await.unwrap();
        let got = squads.get(signed(GetRequest { group_id: id.clone() }, &token_two)).await.unwrap().into_inner();
        assert_eq!(got.squad.unwrap().settings, Some(settings));

        peers.destroy_group(&id).await.unwrap();
        for _ in 0..100 {
            if squads.ctx.query_opt("SELECT 1 FROM squads WHERE group_id = $1", &[&id]).await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(squads.ctx.query_opt("SELECT 1 FROM squads WHERE group_id = $1", &[&id]).await.unwrap().is_none());
        drop(social);
        db.drop_schema().await;
    }

    #[test]
    fn settings_survive_the_table() {
        let settings = SquadSettings {
            match_settings: Some(MatchSettings { map: "EA_Kareah".to_string(), ..Default::default() }),
        };
        let stored = settings.encode_to_vec();
        assert_eq!(SquadSettings::decode(stored.as_slice()).unwrap(), settings);
    }
}
