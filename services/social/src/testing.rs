//! For tests that need a database: a private schema on the server in `SCBACKEND_TEST_POSTGRES`, with
//! only the tables of the service under test, so that a query on another service's table fails as it
//! would in production. Also stand-ins for matchmaking and for the services that own data, and the
//! real sc-social binary for what needs real groups. Without that variable such a test does nothing,
//! so `cargo test` stays offline. Not used outside tests.

use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    process::{self, Child, Command},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use sc_db::{DatabaseArgs, Postgres};
use sc_proto::{
    internal::{
        contacts_internal_server::{ContactsInternal, ContactsInternalServer},
        group_data_internal_server::{GroupDataInternal, GroupDataInternalServer},
        presence_internal_server::{PresenceInternal, PresenceInternalServer},
        DeleteGroupDataRequest, DeleteGroupDataResponse, GetPresencesRequest, GetPresencesResponse, IsBlockedRequest,
        IsBlockedResponse, ListFriendsRequest, ListFriendsResponse, PlayerPresence,
    },
    services::matchmaking::v1::{
        frontend_service_server::{FrontendService, FrontendServiceServer},
        AcceptRequest, AcceptResponse, CancelRequest, CancelResponse, GetLastAssignmentRequest,
        GetLastAssignmentResponse, MatchRequest, MatchResponse,
    },
};
use sc_runtime::{state::Account, CommonArgs};
use tokio::net::TcpStream;
use tonic::{metadata::MetadataMap, service::Routes, transport::Server, Request, Response, Status};

use crate::{peers::PeerArgs, Context};

/// Nothing listens here, so a call fails at once.
pub const NOWHERE: &str = "http://127.0.0.1:9";

/// Peers that all fail, for a test that does not reach them.
pub fn nowhere() -> PeerArgs {
    PeerArgs {
        social: NOWHERE.to_string(),
        presence: NOWHERE.to_string(),
        contacts: NOWHERE.to_string(),
        group_data: format!("chat={NOWHERE},squad={NOWHERE},custom_lobby={NOWHERE},rtc={NOWHERE}"),
    }
}

const FIRST_ACCOUNT: u32 = 2_000_000;
const FIRST_GEID: u64 = 700_000_000_000;
const DROP_ATTEMPTS: u32 = 120;

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct TestDb {
    pub accounts: Vec<Account>,
    pub url: String,
    pub data: std::path::PathBuf,
    admin: Postgres,
    schema: String,
}

impl TestDb {
    /// A fresh schema holding only `tables`, and this many made-up accounts.
    pub async fn open(accounts: u32, tables: &str) -> Option<(Self, Context)> {
        let base = std::env::var("SCBACKEND_TEST_POSTGRES").ok()?;
        let schema = format!("social_test_{}_{}", process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let admin = Postgres::connect(&base, &format!("CREATE SCHEMA {schema}")).await.unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{schema},public");
        let accounts: Vec<Account> = (0..accounts)
            .map(|n| Account {
                account_id: FIRST_ACCOUNT + n,
                handle: format!("Tester{n}"),
                geid: FIRST_GEID + u64::from(n),
                created_at_ms: 0,
            })
            .collect();
        let data = std::env::temp_dir().join(&schema);
        std::fs::create_dir_all(&data).unwrap();
        let state = serde_json::json!({ "jwt_secret": "dGVzdA", "accounts": accounts });
        std::fs::write(data.join("state.json"), state.to_string()).unwrap();
        let db = Self { accounts, url, data, admin, schema };
        let context = db.context(tables).await;
        Some((db, context))
    }

    /// A context of its own, with its own pool, as another replica would have. Its peers fail.
    pub async fn context(&self, tables: &str) -> Context {
        self.context_with(tables, &nowhere()).await
    }

    /// A context that reaches these peers.
    pub async fn context_with(&self, tables: &str, peers: &PeerArgs) -> Context {
        let common = CommonArgs { data: self.data.clone(), health: None };
        let database = DatabaseArgs { postgres: self.url.clone(), arango: String::new() };
        Context::open(&common, &database, NOWHERE, peers, tables).await.unwrap()
    }

    pub async fn drop_schema(self) {
        let drop = format!("DROP SCHEMA {} CASCADE", self.schema);
        // Tests running side by side change the catalog, which fails this now and then.
        for attempt in 1..=DROP_ATTEMPTS {
            match self.admin.execute(&drop, &[]).await {
                Ok(_) => break,
                Err(error) if attempt == DROP_ATTEMPTS => panic!("{drop}: {error}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        std::fs::remove_dir_all(&self.data).unwrap();
    }
}

/// What the stand-in was asked, in order: the caller's authorization header and the request.
pub type Tickets = Arc<Mutex<Vec<(String, MatchRequest)>>>;

#[derive(Clone, Default)]
struct FakeMatchmaking {
    tickets: Tickets,
}

fn header(metadata: &MetadataMap) -> String {
    metadata.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or_default().to_string()
}

#[tonic::async_trait]
impl FrontendService for FakeMatchmaking {
    async fn r#match(&self, request: Request<MatchRequest>) -> Result<Response<MatchResponse>, Status> {
        let caller = header(request.metadata());
        self.tickets.lock().unwrap().push((caller, request.into_inner()));
        Ok(Response::new(MatchResponse { ticket_id: "ticket".to_string() }))
    }

    async fn cancel(&self, _: Request<CancelRequest>) -> Result<Response<CancelResponse>, Status> {
        Err(Status::unimplemented("cancel"))
    }

    async fn get_last_assignment(
        &self,
        _: Request<GetLastAssignmentRequest>,
    ) -> Result<Response<GetLastAssignmentResponse>, Status> {
        Err(Status::unimplemented("get_last_assignment"))
    }

    async fn accept(&self, _: Request<AcceptRequest>) -> Result<Response<AcceptResponse>, Status> {
        Err(Status::unimplemented("accept"))
    }
}

/// Starts a stand-in for matchmaking, and returns its address and what it receives.
pub async fn fake_matchmaking() -> (String, Tickets) {
    let address: SocketAddr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let service = FakeMatchmaking::default();
    let tickets = service.tickets.clone();
    tokio::spawn(Server::builder().add_service(FrontendServiceServer::new(service)).serve(address));
    while TcpStream::connect(address).await.is_err() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (format!("http://{address}"), tickets)
}

/// Serves these services on a free local port until the test ends, and returns its address.
pub async fn serve(routes: Routes) -> String {
    let address: SocketAddr = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    tokio::spawn(Server::builder().add_routes(routes).serve(address));
    while TcpStream::connect(address).await.is_err() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    format!("http://{address}")
}

/// A service with rows per group, which records the groups it was asked to clear. It refuses while
/// `failing` is set.
#[derive(Clone, Default)]
pub struct FakeGroupData {
    pub deleted: Arc<Mutex<Vec<String>>>,
    pub failing: Arc<AtomicBool>,
}

#[tonic::async_trait]
impl GroupDataInternal for FakeGroupData {
    async fn delete_group_data(
        &self,
        request: Request<DeleteGroupDataRequest>,
    ) -> Result<Response<DeleteGroupDataResponse>, Status> {
        if self.failing.load(Ordering::Relaxed) {
            return Err(Status::unavailable("down"));
        }
        let ids = request.into_inner().group_ids;
        let deleted = ids.len() as u64;
        self.deleted.lock().unwrap().extend(ids);
        Ok(Response::new(DeleteGroupDataResponse { deleted }))
    }
}

impl FakeGroupData {
    pub async fn serve(&self) -> String {
        serve(Routes::new(GroupDataInternalServer::new(self.clone()))).await
    }

    pub fn deleted(&self) -> Vec<String> {
        let mut deleted = self.deleted.lock().unwrap().clone();
        deleted.sort();
        deleted
    }
}

/// sc-presence, answering with what the test put in `presences`.
#[derive(Clone, Default)]
pub struct FakePresence {
    pub presences: Arc<Mutex<HashMap<u32, PlayerPresence>>>,
}

#[tonic::async_trait]
impl PresenceInternal for FakePresence {
    async fn get_presences(
        &self,
        request: Request<GetPresencesRequest>,
    ) -> Result<Response<GetPresencesResponse>, Status> {
        let known = self.presences.lock().unwrap();
        let presences = request.into_inner().account_ids.iter().filter_map(|id| known.get(id).cloned()).collect();
        Ok(Response::new(GetPresencesResponse { presences }))
    }
}

impl FakePresence {
    pub async fn serve(&self) -> String {
        serve(Routes::new(PresenceInternalServer::new(self.clone()))).await
    }

    pub fn set(&self, presence: PlayerPresence) {
        self.presences.lock().unwrap().insert(presence.account_id, presence);
    }
}

/// sc-contacts, with the blocks as (owner, target) and friends by account.
#[derive(Clone, Default)]
pub struct FakeContacts {
    pub blocked: Arc<Mutex<HashSet<(u32, u32)>>>,
    pub friends: Arc<Mutex<HashMap<u32, Vec<u32>>>>,
}

#[tonic::async_trait]
impl ContactsInternal for FakeContacts {
    async fn is_blocked(&self, request: Request<IsBlockedRequest>) -> Result<Response<IsBlockedResponse>, Status> {
        let request = request.into_inner();
        let pair = (request.owner_account_id, request.target_account_id);
        Ok(Response::new(IsBlockedResponse { blocked: self.blocked.lock().unwrap().contains(&pair) }))
    }

    async fn list_friends(&self, request: Request<ListFriendsRequest>) -> Result<Response<ListFriendsResponse>, Status> {
        let account_ids = self.friends.lock().unwrap().get(&request.into_inner().account_id).cloned().unwrap_or_default();
        Ok(Response::new(ListFriendsResponse { account_ids }))
    }
}

impl FakeContacts {
    pub async fn serve(&self) -> String {
        serve(Routes::new(ContactsInternalServer::new(self.clone()))).await
    }
}

/// The sc-social binary of this build, or the one `SCBACKEND_TEST_SOCIAL_BIN` names. It is built
/// along with the workspace's tests; a test run without it skips what needs it.
fn social_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SCBACKEND_TEST_SOCIAL_BIN") {
        return Some(PathBuf::from(path));
    }
    let deps = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let binary = deps.parent()?.join("sc-social");
    binary.exists().then_some(binary)
}

/// The real sc-social, on the test's schema, for tests of the services that ask it about groups. It
/// stops when dropped.
pub struct SocialProcess {
    pub address: String,
    child: Child,
}

impl SocialProcess {
    /// `peers` are what sc-social itself reaches: presence, contacts and the services with rows per
    /// group.
    pub async fn spawn(db: &TestDb, peers: &PeerArgs) -> Option<Self> {
        let Some(binary) = social_binary() else {
            eprintln!("no sc-social binary: build it, or set SCBACKEND_TEST_SOCIAL_BIN");
            return None;
        };
        let listen = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let child = Command::new(binary)
            .env("SCBACKEND_POSTGRES", &db.url)
            .env("SCBACKEND_DATA", &db.data)
            .env("SCBACKEND_LISTEN", listen.to_string())
            .env("SCBACKEND_PUSH", NOWHERE)
            .env("SCBACKEND_PRESENCE", &peers.presence)
            .env("SCBACKEND_CONTACTS", &peers.contacts)
            .env("SCBACKEND_GROUP_DATA", &peers.group_data)
            .env_remove("SCBACKEND_HEALTH")
            .spawn()
            .unwrap();
        let mut social = Self { address: format!("http://{listen}"), child };
        for _ in 0..600 {
            if TcpStream::connect(listen).await.is_ok() {
                return Some(social);
            }
            if let Ok(Some(status)) = social.child.try_wait() {
                panic!("sc-social exited: {status}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("sc-social did not start");
    }

    /// Peers for the service under test: this sc-social, and nothing else.
    pub fn peers(&self) -> PeerArgs {
        PeerArgs { social: self.address.clone(), ..nowhere() }
    }
}

impl Drop for SocialProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
