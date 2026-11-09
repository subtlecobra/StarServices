//! sc.external.services.identity.v1.IdentityService, and `IdentityInternal` for other backend services on the same port.
//! The service owns `accounts`, and for the web (sc-portal, sc-admin-web) passwords, emails, roles, web sessions and the
//! login audit (`web.rs`).

mod password;
mod web;

use std::{net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_proto::identity::{
    identity_service_server::{IdentityService, IdentityServiceServer},
    get_players_names_response::PlayerName, GetCurrentPlayerRequest, GetCurrentPlayerResponse,
    GetPlayersNamesRequest, GetPlayersNamesResponse, PreallocatePlayerRequest, PreallocatePlayerResponse,
};
use sc_api::internal::{
    identity_internal_server::{IdentityInternal, IdentityInternalServer},
    ChangePasswordRequest, ChangePasswordResponse, GetAccountRequest, GetAccountResponse, GetAccountsRequest,
    GetAccountsResponse, IssueGameTokenRequest, IssueGameTokenResponse, ListWebSessionsRequest, ListWebSessionsResponse,
    PasswordLoginRequest, PasswordLoginResponse, PlayerAccount, RegisterAccountRequest, RegisterAccountResponse,
    RevokeWebSessionRequest, RevokeWebSessionResponse, SearchAccountsRequest, SearchAccountsResponse,
    GameBadgesRequest, GameBadgesResponse, SetAccountStateRequest, SetAccountStateResponse, VerifyWebSessionRequest, VerifyWebSessionResponse,
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::{info, warn};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    geid BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL UNIQUE,
    handle TEXT NOT NULL UNIQUE,
    created_at_ms BIGINT NOT NULL
);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// argon2id memory per password hash, in KiB. Raising it, or the passes, rehashes each password at its next login.
    #[arg(long, env = "SCBACKEND_ARGON2_MEMORY_KIB", default_value_t = 19_456)]
    argon2_memory_kib: u32,
    #[arg(long, env = "SCBACKEND_ARGON2_ITERATIONS", default_value_t = 2)]
    argon2_iterations: u32,
    #[arg(long, env = "SCBACKEND_ARGON2_PARALLELISM", default_value_t = 1)]
    argon2_parallelism: u32,
}

const CLEAN_UP_EVERY: Duration = Duration::from_secs(3600);

struct Identity {
    state: Arc<State>,
    db: Postgres,
    web: web::Web,
}

#[tonic::async_trait]
impl IdentityService for Identity {
    /// Claims a name for the calling account, which fails if another account holds it.
    async fn preallocate_player(
        &self,
        request: Request<PreallocatePlayerRequest>,
    ) -> Result<Response<PreallocatePlayerResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let name = request.into_inner().name.ok_or_else(|| Status::invalid_argument("missing name"))?;
        let owner = self
            .db
            .query_opt("SELECT geid FROM accounts WHERE lower(handle) = lower($1)", &[&name])
            .await
            .map_err(internal)?;
        if owner.is_some_and(|row| row.get::<_, i64>("geid") != account.geid as i64) {
            return Err(Status::already_exists(format!("name '{name}' is taken")));
        }
        // Handles are unique whatever their case, which the index enforces.
        self.db
            .execute("UPDATE accounts SET handle = $2 WHERE geid = $1", &[&(account.geid as i64), &name])
            .await
            .map_err(|error| match error.contains("duplicate key") {
                true => Status::already_exists(format!("name '{name}' is taken")),
                false => internal(error),
            })?;
        let mut player = account.player();
        player.name = name;
        Ok(Response::new(PreallocatePlayerResponse { player: Some(player) }))
    }

    async fn get_current_player(
        &self,
        request: Request<GetCurrentPlayerRequest>,
    ) -> Result<Response<GetCurrentPlayerResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let jwt = self.state.issue_token(&account);
        Ok(Response::new(GetCurrentPlayerResponse { player: Some(account.player()), jwt }))
    }

    async fn get_players_names(
        &self,
        request: Request<GetPlayersNamesRequest>,
    ) -> Result<Response<GetPlayersNamesResponse>, Status> {
        let geids: Vec<i64> = request.into_inner().player_geids.iter().map(|geid| *geid as i64).collect();
        let rows = self
            .db
            .query("SELECT geid, handle FROM accounts WHERE geid = ANY($1)", &[&geids])
            .await
            .map_err(internal)?;
        let names = rows
            .iter()
            .map(|row| PlayerName { geid: row.get::<_, i64>("geid") as u64, name: row.get("handle") })
            .collect();
        Ok(Response::new(GetPlayersNamesResponse { names }))
    }
}

/// Turns players into the accounts push routes by: sc-delivery asks this for whom to tell.
#[tonic::async_trait]
impl IdentityInternal for Identity {
    async fn get_accounts(&self, request: Request<GetAccountsRequest>) -> Result<Response<GetAccountsResponse>, Status> {
        let geids: Vec<i64> = request.into_inner().geids.iter().map(|geid| *geid as i64).collect();
        if geids.is_empty() {
            return Ok(Response::new(GetAccountsResponse::default()));
        }
        let rows = self
            .db
            .query("SELECT geid, account_id FROM accounts WHERE geid = ANY($1)", &[&geids])
            .await
            .map_err(internal)?;
        let accounts = rows
            .iter()
            .map(|row| PlayerAccount {
                geid: row.get::<_, i64>("geid") as u64,
                account_id: row.get::<_, i64>("account_id") as u32,
            })
            .collect();
        Ok(Response::new(GetAccountsResponse { accounts }))
    }

    async fn register_account(
        &self,
        request: Request<RegisterAccountRequest>,
    ) -> Result<Response<RegisterAccountResponse>, Status> {
        self.web.register(request.into_inner()).await.map(Response::new)
    }

    async fn password_login(
        &self,
        request: Request<PasswordLoginRequest>,
    ) -> Result<Response<PasswordLoginResponse>, Status> {
        self.web.login(request.into_inner()).await.map(Response::new)
    }

    async fn verify_web_session(
        &self,
        request: Request<VerifyWebSessionRequest>,
    ) -> Result<Response<VerifyWebSessionResponse>, Status> {
        self.web.verify_session(request.into_inner()).await.map(Response::new)
    }

    async fn revoke_web_session(
        &self,
        request: Request<RevokeWebSessionRequest>,
    ) -> Result<Response<RevokeWebSessionResponse>, Status> {
        self.web.revoke_session(request.into_inner()).await.map(Response::new)
    }

    async fn list_web_sessions(
        &self,
        request: Request<ListWebSessionsRequest>,
    ) -> Result<Response<ListWebSessionsResponse>, Status> {
        self.web.list_sessions(request.into_inner()).await.map(Response::new)
    }

    async fn change_password(
        &self,
        request: Request<ChangePasswordRequest>,
    ) -> Result<Response<ChangePasswordResponse>, Status> {
        self.web.change_password(request.into_inner()).await.map(Response::new)
    }

    async fn issue_game_token(
        &self,
        request: Request<IssueGameTokenRequest>,
    ) -> Result<Response<IssueGameTokenResponse>, Status> {
        self.web.issue_game_token(request.into_inner()).await.map(Response::new)
    }

    async fn get_account(&self, request: Request<GetAccountRequest>) -> Result<Response<GetAccountResponse>, Status> {
        self.web.get_account(request.into_inner()).await.map(Response::new)
    }

    async fn search_accounts(
        &self,
        request: Request<SearchAccountsRequest>,
    ) -> Result<Response<SearchAccountsResponse>, Status> {
        self.web.search(request.into_inner()).await.map(Response::new)
    }

    async fn set_account_state(
        &self,
        request: Request<SetAccountStateRequest>,
    ) -> Result<Response<SetAccountStateResponse>, Status> {
        self.web.set_state(request.into_inner()).await.map(Response::new)
    }

    async fn game_badges(&self, _: Request<GameBadgesRequest>) -> Result<Response<GameBadgesResponse>, Status> {
        let badges = web::GAME_BADGES.iter().filter_map(|(id, _)| web::game_badge(*id)).collect();
        Ok(Response::new(GameBadgesResponse { badges }))
    }
}

/// Accounts are issued by sc-admin, so each start copies new ones into the database.
async fn seed_accounts(db: &Postgres, state: &State) -> Result<(), String> {
    for account in state.accounts() {
        db.execute(
            "INSERT INTO accounts (geid, account_id, handle, created_at_ms) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
            &[&(account.geid as i64), &i64::from(account.account_id), &account.handle, &(account.created_at_ms as i64)],
        )
        .await?;
    }
    Ok(())
}

async fn serve(args: Args) -> Result<(), String> {
    let state = Arc::new(State::load(&args.common.data)?);
    let db = Postgres::connect(&args.database.postgres, &format!("{SCHEMA}{}", web::SCHEMA)).await?;
    seed_accounts(&db, &state).await?;
    let hasher = password::Hasher::new(args.argon2_memory_kib, args.argon2_iterations, args.argon2_parallelism)?;
    let identity = Arc::new(Identity { web: web::Web::new(db.clone(), state.clone(), hasher), state, db });
    let cleaner = identity.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = cleaner.web.clean_up().await {
                warn!(%error, "web session clean-up failed");
            }
            tokio::time::sleep(CLEAN_UP_EVERY).await;
        }
    });
    let service = IdentityServiceServer::from_arc(identity.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = IdentityInternalServer::from_arc(identity).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "identity listening");
    Server::builder().add_service(service).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_api::internal::{WebClient, WebSession};

    #[tokio::test]
    async fn the_badge_catalog_has_the_developer_badges() {
        let ids: Vec<u32> = web::GAME_BADGES.iter().map(|(id, _)| *id).collect();
        assert!(ids.len() == 7, "the game's seven badges, once each");
        assert!([1, 14, 126].iter().all(|id| ids.contains(id)), "the console's developer badges");
        assert!(web::game_badge(0x42).is_none());
    }

    /// A private schema of the server in `SCBACKEND_TEST_POSTGRES`, with identity's tables. Without it the tests do
    /// nothing, so `cargo test` stays offline.
    struct TestDb {
        admin: Postgres,
        schema: String,
        dir: std::path::PathBuf,
        identity: Identity,
    }

    async fn test_db(name: &str) -> Option<TestDb> {
        let base = std::env::var("SCBACKEND_TEST_POSTGRES").ok()?;
        let schema = format!("identity_{name}_{}", std::process::id());
        let admin = Postgres::connect(&base, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{schema}");
        let db = Postgres::connect(&url, &format!("{SCHEMA}{}", web::SCHEMA)).await.unwrap();
        let dir = std::env::temp_dir().join(&schema);
        let state = Arc::new(State::open(&dir).unwrap());
        let hasher = password::Hasher::new(1024, 1, 1).unwrap();
        let identity = Identity { web: web::Web::new(db.clone(), state.clone(), hasher), state, db };
        Some(TestDb { admin, schema, dir, identity })
    }

    impl TestDb {
        async fn drop(self) {
            self.admin.execute(&format!("DROP SCHEMA {} CASCADE", self.schema), &[]).await.unwrap();
            let _ = std::fs::remove_dir_all(self.dir);
        }
    }

    fn client(ip: &str) -> Option<WebClient> {
        Some(WebClient { ip: ip.into(), user_agent: "test".into() })
    }

    fn register(handle: &str, email: &str, password: &str) -> Request<RegisterAccountRequest> {
        Request::new(RegisterAccountRequest {
            handle: handle.into(),
            email: email.into(),
            password: password.into(),
            client: client("10.0.0.1"),
        })
    }

    fn login(login: &str, password: &str, ip: &str) -> Request<PasswordLoginRequest> {
        Request::new(PasswordLoginRequest { login: login.into(), password: password.into(), client: client(ip) })
    }

    async fn verify(identity: &Identity, token: &str) -> VerifyWebSessionResponse {
        let request = Request::new(VerifyWebSessionRequest { token: token.into() });
        identity.verify_web_session(request).await.unwrap().into_inner()
    }

    async fn sessions(identity: &Identity, account_id: u32, current: &str) -> Vec<WebSession> {
        let request = Request::new(ListWebSessionsRequest { account_id, current_token: current.into() });
        identity.list_web_sessions(request).await.unwrap().into_inner().sessions
    }

    #[tokio::test]
    async fn accounts_are_found_by_geid_and_unknown_geids_left_out() {
        let Some(test) = test_db("geids").await else { return };
        let insert = "INSERT INTO accounts (geid, account_id, handle, created_at_ms) VALUES ($1, $2, $3, 0)";
        test.identity.db.execute(insert, &[&-1_i64, &7_i64, &"big"]).await.unwrap();
        test.identity.db.execute(insert, &[&5_i64, &8_i64, &"small"]).await.unwrap();
        let request = Request::new(GetAccountsRequest { geids: vec![u64::MAX, 5, 6] });
        let mut found = test.identity.get_accounts(request).await.unwrap().into_inner().accounts;
        found.sort_by_key(|account| account.account_id);
        let pairs: Vec<_> = found.iter().map(|account| (account.geid, account.account_id)).collect();
        assert_eq!(pairs, [(u64::MAX, 7), (5, 8)]);
        let empty = test.identity.get_accounts(Request::new(GetAccountsRequest::default())).await.unwrap().into_inner();
        assert!(empty.accounts.is_empty());
        test.drop().await;
    }

    #[tokio::test]
    async fn register_login_verify_list_and_logout() {
        let Some(test) = test_db("web").await else { return };
        let identity = &test.identity;
        let password = "violet tangerine orbit";
        let made = identity.register_account(register("Nova_1", "Nova@Example.com", password)).await.unwrap().into_inner();
        assert!(made.account_id > 1_100_000, "registered accounts take indexes from 100,001");
        let accounts = identity
            .get_accounts(Request::new(GetAccountsRequest { geids: vec![300_000_000_000 + u64::from(made.account_id - 1_000_000)] }))
            .await
            .unwrap()
            .into_inner()
            .accounts;
        assert_eq!(accounts.len(), 1, "the GEID follows the account id as for issued accounts");

        // The handle and email are taken whatever their case, with one answer for both.
        for (handle, email) in [("nova_1", "other@example.com"), ("Other", "NOVA@example.com")] {
            let refused = identity.register_account(register(handle, email, password)).await.unwrap_err();
            assert_eq!(refused.code(), tonic::Code::AlreadyExists);
        }
        let weak = identity.register_account(register("Weak", "weak@example.com", "password123")).await.unwrap_err();
        assert_eq!(weak.code(), tonic::Code::InvalidArgument);

        // Login by handle or email, case ignored.
        let first = identity.password_login(login("NOVA_1", password, "10.0.0.2")).await.unwrap().into_inner();
        let second = identity.password_login(login("nova@example.com", password, "10.0.0.3")).await.unwrap().into_inner();
        assert_eq!((first.account_id, first.handle.as_str()), (made.account_id, "Nova_1"));
        let checked = verify(identity, &first.session_token).await;
        assert!(checked.valid && checked.account_id == made.account_id && checked.roles.is_empty());

        // A wrong password and an unknown login get the same answer.
        let wrong = identity.password_login(login("nova_1", "not the password", "10.0.0.2")).await.unwrap_err();
        let unknown = identity.password_login(login("nobody", password, "10.0.0.2")).await.unwrap_err();
        assert_eq!((wrong.code(), wrong.message()), (unknown.code(), unknown.message()));
        assert_eq!(wrong.code(), tonic::Code::Unauthenticated);

        let listed = sessions(identity, made.account_id, &first.session_token).await;
        assert_eq!(listed.len(), 2);
        assert_eq!(listed.iter().filter(|session| session.current).count(), 1);

        // Revoking the other session by id, then logging out by token.
        let other = listed.iter().find(|session| !session.current).unwrap();
        let revoke = RevokeWebSessionRequest { account_id: made.account_id, session_id: other.session_id.clone(), ..Default::default() };
        assert!(identity.revoke_web_session(Request::new(revoke)).await.unwrap().into_inner().revoked);
        assert!(!verify(identity, &second.session_token).await.valid);
        let logout = RevokeWebSessionRequest { token: first.session_token.clone(), ..Default::default() };
        identity.revoke_web_session(Request::new(logout)).await.unwrap();
        assert!(!verify(identity, &first.session_token).await.valid);
        assert!(!verify(identity, "garbage").await.valid);
        test.drop().await;
    }

    #[tokio::test]
    async fn password_change_keeps_this_session_and_ends_the_others() {
        let Some(test) = test_db("change").await else { return };
        let identity = &test.identity;
        let old = "violet tangerine orbit";
        let new = "amber walrus lantern";
        let made = identity.register_account(register("Changer", "changer@example.com", old)).await.unwrap().into_inner();
        let kept = identity.password_login(login("changer", old, "10.0.0.2")).await.unwrap().into_inner();
        let ended = identity.password_login(login("changer", old, "10.0.0.3")).await.unwrap().into_inner();
        let change = |current: &str| ChangePasswordRequest {
            account_id: made.account_id,
            current_password: current.into(),
            new_password: new.into(),
            keep_session_token: kept.session_token.clone(),
            client: client("10.0.0.2"),
        };
        let refused = identity.change_password(Request::new(change("wrong password"))).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        identity.change_password(Request::new(change(old))).await.unwrap();
        assert!(verify(identity, &kept.session_token).await.valid);
        assert!(!verify(identity, &ended.session_token).await.valid);
        assert!(identity.password_login(login("changer", old, "10.0.0.2")).await.is_err());
        assert!(identity.password_login(login("changer", new, "10.0.0.2")).await.is_ok());
        test.drop().await;
    }

    #[tokio::test]
    async fn failed_logins_are_throttled_per_login_and_address() {
        let Some(test) = test_db("throttle").await else { return };
        let identity = &test.identity;
        let password = "violet tangerine orbit";
        identity.register_account(register("Target", "target@example.com", password)).await.unwrap();
        for _ in 0..10 {
            let failed = identity.password_login(login("target", "guess guess guess", "10.9.9.9")).await.unwrap_err();
            assert_eq!(failed.code(), tonic::Code::Unauthenticated);
        }
        // Now even the right password is refused from that address, and the same happens for a name with no account.
        let throttled = identity.password_login(login("target", password, "10.9.9.9")).await.unwrap_err();
        assert_eq!(throttled.code(), tonic::Code::ResourceExhausted);
        assert!(identity.password_login(login("target", password, "10.0.0.5")).await.is_ok());
        for _ in 0..10 {
            identity.password_login(login("ghost", "guess guess guess", "10.9.9.8")).await.unwrap_err();
        }
        let ghost = identity.password_login(login("ghost", "guess guess guess", "10.9.9.8")).await.unwrap_err();
        assert_eq!(ghost.code(), tonic::Code::ResourceExhausted);
        test.drop().await;
    }

    #[tokio::test]
    async fn admins_search_disable_reset_and_grant_roles() {
        let Some(test) = test_db("admin").await else { return };
        let identity = &test.identity;
        let password = "violet tangerine orbit";
        let insert = "INSERT INTO accounts (geid, account_id, handle, created_at_ms) VALUES ($1, $2, $3, 0)";
        identity.db.execute(insert, &[&300_000_000_001_i64, &1_000_001_i64, &"pilot"]).await.unwrap();
        let made = identity.register_account(register("Wingman", "wing@example.com", password)).await.unwrap().into_inner();
        let session = identity.password_login(login("wingman", password, "10.0.0.2")).await.unwrap().into_inner();

        let search = |query: &str, limit: u32, page_token: &str| SearchAccountsRequest {
            query: query.into(),
            limit,
            page_token: page_token.into(),
        };
        let all = identity.search_accounts(Request::new(search("", 1, ""))).await.unwrap().into_inner();
        assert_eq!(all.accounts[0].handle, "pilot");
        assert_eq!(all.accounts[0].geid, 300_000_000_001, "the summary names the account's player");
        let rest = identity.search_accounts(Request::new(search("", 1, &all.next_page_token))).await.unwrap().into_inner();
        assert_eq!((rest.accounts[0].handle.as_str(), rest.next_page_token.as_str()), ("Wingman", ""));
        let by_email = identity.search_accounts(Request::new(search("WING@", 0, ""))).await.unwrap().into_inner();
        assert_eq!(by_email.accounts.len(), 1);
        let literal = identity.search_accounts(Request::new(search("%", 0, ""))).await.unwrap().into_inner();
        assert!(literal.accounts.is_empty(), "LIKE wildcards in the query are literal");

        let grant = SetAccountStateRequest { account_id: made.account_id, roles: vec!["admin".into()], set_roles: true, ..Default::default() };
        let granted = identity.set_account_state(Request::new(grant)).await.unwrap().into_inner().account.unwrap();
        assert_eq!(granted.roles, ["admin"]);
        assert_eq!(granted.geid, 300_000_000_000 + u64::from(made.account_id - 1_000_000));
        assert_eq!(verify(identity, &session.session_token).await.roles, ["admin"]);
        let bad_role = SetAccountStateRequest { account_id: made.account_id, roles: vec!["god".into()], set_roles: true, ..Default::default() };
        assert!(identity.set_account_state(Request::new(bad_role)).await.is_err());

        // In-game badges: replaced as a set, kept apart from the roles, and only known ids.
        let badges = SetAccountStateRequest { account_id: made.account_id, game_badges: vec![126, 14], set_game_badges: true, ..Default::default() };
        let badged = identity.set_account_state(Request::new(badges)).await.unwrap().into_inner().account.unwrap();
        let ids: Vec<u32> = badged.game_badges.iter().map(|badge| badge.id).collect();
        assert_eq!(ids, [14, 126]);
        assert_eq!(badged.game_badges[0].name, "RSI_QA");
        assert_eq!(badged.roles, ["admin"]);
        let unknown = SetAccountStateRequest { account_id: made.account_id, game_badges: vec![0x42], set_game_badges: true, ..Default::default() };
        assert!(identity.set_account_state(Request::new(unknown)).await.is_err());
        let fetched = identity.get_account(Request::new(GetAccountRequest { account_id: made.account_id })).await.unwrap().into_inner().account.unwrap();
        assert_eq!(fetched.game_badges.len(), 2);

        let disable = SetAccountStateRequest { account_id: made.account_id, disabled: Some(true), ..Default::default() };
        assert!(identity.set_account_state(Request::new(disable)).await.unwrap().into_inner().account.unwrap().disabled);
        assert!(!verify(identity, &session.session_token).await.valid);
        let refused = identity.password_login(login("wingman", password, "10.0.0.2")).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::PermissionDenied);
        let token = identity.issue_game_token(Request::new(IssueGameTokenRequest { account_id: made.account_id })).await;
        assert_eq!(token.unwrap_err().code(), tonic::Code::PermissionDenied);

        // An account sc-admin issued has no password until an admin sets one.
        let reset = SetAccountStateRequest { account_id: 1_000_001, new_password: Some("amber walrus lantern".into()), ..Default::default() };
        identity.set_account_state(Request::new(reset)).await.unwrap();
        let pilot = identity.password_login(login("pilot", "amber walrus lantern", "10.0.0.2")).await.unwrap().into_inner();
        let game = identity.issue_game_token(Request::new(IssueGameTokenRequest { account_id: pilot.account_id })).await;
        let game = game.unwrap().into_inner();
        let account = identity.state.verify_token(&game.token).unwrap();
        assert_eq!((account.account_id, account.geid, account.handle.as_str()), (1_000_001, 300_000_000_001, "pilot"));
        test.drop().await;
    }
}
