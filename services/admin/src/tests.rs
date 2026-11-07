//! The site against fake owners: what reaches each owner, what is audited, and what an outsider sees.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use sc_proto::{
    internal::{
        AccountSummary, AdminAddItemRequest, AdminAddItemResponse, AdminEntityClass, AdminInventory, AdminInventoryItem,
        AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse,
        CreateRentalRequest, EndRentalRequest, EndRentalResponse, EntitlementHistoryEntry,
        ExtendRentalRequest, ExtendRentalResponse, GrantPurchaseRequest, LedgerAdjustRequest, LedgerAdjustResponse,
        LedgerBalance, LedgerOperation, PlayerPresence, PlayerShard, PutEntityDocumentRequest,
        PutEntityDocumentResponse, RevokeEntitlementRequest, RevokeEntitlementResponse, SearchAccountsRequest,
        SearchAccountsResponse, SetAccountStateRequest, SetRentalExpiryRequest, StoredEntityDocument,
        VerifyWebSessionResponse,
    },
    services::{
        entitlement::v2::{Entitlement, EntitlementFilter, EntitlementStatus},
        entity_document::v1::{entity_document_body::Class, DocumentHeader, EntityDocument},
        entitygraph::v1::InventoryNodeProperties,
        rental::v1::{Rental, RentalStatus},
    },
};
use tonic::Status;
use tower::ServiceExt;

use crate::{
    audit::{AuditLog, MemoryAudit},
    backend::{Identity, Owners},
    web::{router, App},
};

const ADMIN: u32 = 1_000_001;
const PLAYER: u32 = 1_000_002;
const PLAYER_GEID: u64 = 300_000_000_002;
/// An account sc-identity knows no player for.
const NO_PLAYER: u32 = 1_000_003;
const CSRF: &str = "c5rf-token-value";
const RID: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct Fake {
    balance: Mutex<i64>,
    adjustments: Mutex<HashMap<String, LedgerAdjustResponse>>,
    calls: Mutex<Vec<String>>,
    on_shard: Mutex<bool>,
    documents: Mutex<HashMap<String, (u64, PutEntityDocumentRequest)>>,
    rentals: Mutex<Vec<Rental>>,
    pushes: Mutex<u32>,
    /// Applied inventory commands, as sc-entitygraph keeps them: a command id applies once.
    inventory_commands: Mutex<HashMap<String, u64>>,
}

impl Fake {
    fn called(&self, what: impl Into<String>) {
        self.calls.lock().unwrap().push(what.into());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn summary(account_id: u32) -> Option<AccountSummary> {
        let (handle, geid) = match account_id {
            ADMIN => ("root", 300_000_000_001),
            PLAYER => ("pilot<script>", PLAYER_GEID),
            NO_PLAYER => ("ghost", 0),
            _ => return None,
        };
        Some(AccountSummary { account_id, handle: handle.into(), email: format!("{account_id}@example.org"), geid, ..Default::default() })
    }
}

#[tonic::async_trait]
impl Identity for Fake {
    async fn verify_session(&self, token: &str) -> Result<VerifyWebSessionResponse, Status> {
        Ok(match token {
            "admin" => VerifyWebSessionResponse { valid: true, account_id: ADMIN, handle: "root".into(), roles: vec!["admin".into()] },
            "player" => VerifyWebSessionResponse { valid: true, account_id: PLAYER, handle: "pilot".into(), roles: vec![] },
            _ => VerifyWebSessionResponse::default(),
        })
    }

    async fn search_accounts(&self, request: SearchAccountsRequest) -> Result<SearchAccountsResponse, Status> {
        self.called(format!("search {}", request.query));
        Ok(SearchAccountsResponse { accounts: [ADMIN, PLAYER].into_iter().filter_map(Fake::summary).collect(), ..Default::default() })
    }

    async fn account(&self, account_id: u32) -> Result<Option<AccountSummary>, Status> {
        Ok(Fake::summary(account_id))
    }

    async fn set_account_state(&self, request: SetAccountStateRequest) -> Result<AccountSummary, Status> {
        self.called(format!("state {:?} {:?} {:?}", request.disabled, request.roles, request.new_password.is_some()));
        let mut account = Fake::summary(request.account_id).ok_or_else(|| Status::not_found("account"))?;
        account.disabled = request.disabled.unwrap_or(false);
        Ok(account)
    }
}

/// The player's inventories as sc-entitygraph answers: a location bank holding a crate, the crate's inventory with
/// a stack of five, and a ship's hold that is live on a shard.
fn fake_tree() -> Vec<AdminInventory> {
    let inventory = |id: &str, context: &str, subject: u64, parent: &str, depth: u32| AdminInventory {
        inventory: Some(InventoryNodeProperties { id: id.into(), context: context.into(), subject_id: subject, owner_id: PLAYER_GEID, ..Default::default() }),
        parent_inventory_id: parent.into(),
        depth,
        ..Default::default()
    };
    let personal = format!("{PLAYER_GEID}:PlayerInventory:0");
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let mut location = inventory(&bank, "Location", 2_026_442_305, "", 0);
    location.items = vec![AdminInventoryItem {
        geid: 400_000_000_001,
        class_name: "crate<1>".into(),
        class_guid: "c0ffee00-0000-0000-0000-000000000001".into(),
        stack_size: 1,
        owner_id: PLAYER_GEID,
        inventory_id: bank.clone(),
        held_inventory_ids: vec!["400000000001:Container:0".into()],
        ..Default::default()
    }];
    location.item_count = 1;
    let mut crate_inventory = inventory("400000000001:Container:0", "Container", 0, &bank, 1);
    crate_inventory.holder_id = 400_000_000_001;
    crate_inventory.holder_class_name = "crate<1>".into();
    crate_inventory.items = vec![AdminInventoryItem {
        geid: 400_000_000_002,
        class_name: "ammo".into(),
        display_name: "Ammo box".into(),
        stack_size: 5,
        owner_id: PLAYER_GEID,
        inventory_id: "400000000001:Container:0".into(),
        ..Default::default()
    }];
    crate_inventory.item_count = 1;
    let mut hold = inventory("900:Container:0", "Container", 0, "", 0);
    hold.live = true;
    hold.items = vec![AdminInventoryItem { geid: 901, class_name: "spaceship_cargo".into(), inventory_id: "900:Container:0".into(), ..Default::default() }];
    vec![inventory(&personal, "PlayerInventory", 0, "", 0), location, crate_inventory, hold]
}

#[tonic::async_trait]
impl Owners for Fake {
    async fn balances(&self, _: u64) -> Result<Vec<LedgerBalance>, Status> {
        let amount = *self.balance.lock().unwrap();
        Ok(vec![LedgerBalance { ledger_name: "AUEC_LEDGER".into(), amount, currency: 3, version: 1, updated_at_ms: 0 }])
    }

    /// Keyed as sc-ledger is: a key applies once.
    async fn adjust(&self, request: LedgerAdjustRequest) -> Result<LedgerAdjustResponse, Status> {
        self.called(format!("adjust {} {} {}", request.key, request.delta, request.owner_geid));
        if let Some(done) = self.adjustments.lock().unwrap().get(&request.key) {
            return Ok(LedgerAdjustResponse { replayed: true, ..*done });
        }
        let mut balance = self.balance.lock().unwrap();
        if *balance + request.delta < 0 {
            return Err(Status::failed_precondition("insufficient funds"));
        }
        let before = *balance;
        *balance += request.delta;
        let done = LedgerAdjustResponse { balance_before: before, balance_after: *balance, version: 2, replayed: false };
        self.adjustments.lock().unwrap().insert(request.key, done);
        Ok(done)
    }

    async fn operations(&self, _: u64, _: i64) -> Result<Vec<LedgerOperation>, Status> {
        Ok(vec![])
    }

    async fn entitlements(&self, _: u32, geid: u64, _: EntitlementFilter) -> Result<Vec<Entitlement>, Status> {
        Ok(vec![Entitlement {
            urn: "urn:e:1".into(),
            entity_class_guid: "abc".into(),
            player_id: geid,
            status: EntitlementStatus::Packaged as i32,
            ..Default::default()
        }])
    }

    async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status> {
        self.called(format!("grant {} {} {}", request.key, request.source_type, request.entity_class_guid));
        Ok("urn:e:new".into())
    }

    async fn revoke(&self, request: RevokeEntitlementRequest) -> Result<RevokeEntitlementResponse, Status> {
        self.called(format!("revoke {} {}", request.urn, request.command_id));
        Ok(RevokeEntitlementResponse { previous_status: EntitlementStatus::Packaged as i32, revoked: true })
    }

    async fn entitlement_history(&self, _: u64) -> Result<Vec<EntitlementHistoryEntry>, Status> {
        Ok(vec![])
    }

    async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status> {
        self.called(format!("expiry {} {}", request.urn, request.reactivate));
        Ok(())
    }

    async fn rentals(&self, _: u64) -> Result<Vec<Rental>, Status> {
        Ok(self.rentals.lock().unwrap().clone())
    }

    async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status> {
        self.called(format!("rent {} {} {}", request.id, request.entitlement_urn, request.days));
        Ok(())
    }

    async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status> {
        self.called(format!("extend {} {} {}", request.key, request.id, request.days));
        Ok(ExtendRentalResponse { expire_at_ms: 5, entitlement_urn: "urn:e:1".into() })
    }

    async fn end_rental(&self, request: EndRentalRequest) -> Result<EndRentalResponse, Status> {
        self.called(format!("end {}", request.id));
        Ok(EndRentalResponse { ended: true, previous_expire_at_ms: 9 })
    }

    async fn documents(&self, entity_id: u64, classes: Vec<String>) -> Result<Vec<StoredEntityDocument>, Status> {
        let documents = self.documents.lock().unwrap();
        Ok(classes
            .iter()
            .filter_map(|class| {
                let (revision, put) = documents.get(class)?;
                Some(StoredEntityDocument {
                    class: class.clone(),
                    document: Some(EntityDocument {
                        header: Some(DocumentHeader { entity_id, generation: 1, revision: revision.to_string() }),
                        body: put.body.clone(),
                    }),
                    owner_id: entity_id,
                    prepared: false,
                })
            })
            .collect())
    }

    /// Checks the expected revision as sc-entity-document does.
    async fn put_document(&self, request: PutEntityDocumentRequest) -> Result<PutEntityDocumentResponse, Status> {
        let class = match request.body.as_ref().and_then(|body| body.class.as_ref()) {
            Some(Class::PlayerHomeLocation(_)) => "player_home_location",
            Some(Class::PlayerSpawnLocation(_)) => "player_spawn_location",
            _ => return Err(Status::invalid_argument("class")),
        };
        self.called(format!("put {class} {} {:?}", request.entity_id, request.expected_revision));
        let mut documents = self.documents.lock().unwrap();
        let found = documents.get(class).map_or(0, |(revision, _)| *revision);
        if request.expected_revision.is_some_and(|expected| expected != found) {
            return Err(Status::aborted("changed meanwhile"));
        }
        documents.insert(class.into(), (found + 1, request));
        Ok(PutEntityDocumentResponse { previous: None, revision: found + 1 })
    }

    async fn player_shard(&self, _: u64) -> Result<Option<PlayerShard>, Status> {
        let on = *self.on_shard.lock().unwrap();
        Ok(on.then(|| PlayerShard { shard_id: "usa-crimson-falcon".into(), location_id: 2_026_442_305, ..Default::default() }))
    }

    async fn presence(&self, _: u32) -> Result<Option<PlayerPresence>, Status> {
        Ok(None)
    }

    async fn inventory_tree(&self, geid: u64) -> Result<Vec<AdminInventory>, Status> {
        assert_eq!(geid, PLAYER_GEID, "the GEID comes from the account summary");
        Ok(fake_tree())
    }

    async fn move_item(&self, request: AdminMoveItemRequest) -> Result<AdminMoveItemResponse, Status> {
        self.called(format!(
            "move {} {} {} {} -> {} x{}",
            request.command_id, request.owner_id, request.geid, request.from_inventory_id, request.to_inventory_id, request.quantity
        ));
        let moved = if request.quantity > 0 { 400_000_000_009 } else { request.geid };
        let mut applied = self.inventory_commands.lock().unwrap();
        let replayed = applied.contains_key(&request.command_id);
        let moved = *applied.entry(request.command_id).or_insert(moved);
        Ok(AdminMoveItemResponse { moved_geid: moved, replayed })
    }

    async fn add_item(&self, request: AdminAddItemRequest) -> Result<AdminAddItemResponse, Status> {
        self.called(format!("add {} {} {} {} x{}", request.command_id, request.owner_id, request.inventory_id, request.entity_class_guid, request.stack_size));
        if request.inventory_id.starts_with("900:") {
            return Err(Status::failed_precondition("the inventory is held by something live on a shard"));
        }
        let mut applied = self.inventory_commands.lock().unwrap();
        let replayed = applied.contains_key(&request.command_id);
        let geid = *applied.entry(request.command_id).or_insert(400_000_000_010);
        Ok(AdminAddItemResponse { geid, replayed })
    }

    async fn remove_item(&self, request: AdminRemoveItemRequest) -> Result<AdminRemoveItemResponse, Status> {
        self.called(format!("remove {} {} {}", request.command_id, request.owner_id, request.geid));
        Ok(AdminRemoveItemResponse { removed: true, erased: 2, replayed: false })
    }

    async fn search_classes(&self, query: &str) -> Result<Vec<AdminEntityClass>, Status> {
        self.called(format!("classes {query}"));
        Ok(vec![AdminEntityClass {
            guid: "c0ffee00-0000-0000-0000-000000000001".into(),
            class_name: "crate<1>".into(),
            display_name: "Cargo crate".into(),
            occupancy_micro_scu: 500_000,
            capacity_micro_scu: 1_000_000,
            container: true,
            ..Default::default()
        }])
    }

    async fn funds_changed(&self, _: u32, _: u64, _: &str, _: i64, _: u64) {
        *self.pushes.lock().unwrap() += 1;
    }
}

struct Site {
    router: Router,
    fake: Arc<Fake>,
    audit: Arc<MemoryAudit>,
}

fn site_with(audit: MemoryAudit) -> Site {
    let fake = Arc::new(Fake { balance: Mutex::new(1_000), ..Default::default() });
    let audit = Arc::new(audit);
    let app = Arc::new(App {
        identity: fake.clone(),
        owners: fake.clone(),
        audit: audit.clone(),
        prefix: "/admin".into(),
        login: "/login".into(),
        secure_cookies: false,
    });
    Site { router: router(app), fake, audit }
}

fn site() -> Site {
    site_with(MemoryAudit::default())
}

struct Reply {
    status: StatusCode,
    location: String,
    cookies: Vec<String>,
    body: String,
}

impl Site {
    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response.headers().get(header::LOCATION).map_or(String::new(), |v| v.to_str().unwrap().to_string());
        let cookies = response.headers().get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).collect();
        let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        Reply { status, location, cookies, body }
    }

    async fn get(&self, path: &str, session: Option<&str>) -> Reply {
        let mut request = Request::get(path);
        if let Some(session) = session {
            request = request.header(header::COOKIE, format!("sc_session={session}; sc_csrf={CSRF}"));
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn post(&self, path: &str, session: &str, fields: &[(&str, &str)]) -> Reply {
        let body: String = form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
        let request = Request::post(path)
            .header(header::COOKIE, format!("sc_session={session}; sc_csrf={CSRF}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        self.send(request).await
    }

    /// Posts as the admin, with the CSRF token and a request id.
    async fn act(&self, path: &str, fields: &[(&str, &str)]) -> Reply {
        let mut all = vec![("x-csrf-token", CSRF), ("request_id", RID)];
        all.extend_from_slice(fields);
        self.post(path, "admin", &all).await
    }
}

fn notice(reply: &Reply) -> String {
    let query = reply.location.split_once('?').map_or("", |(_, query)| query);
    form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "notice").map(|(_, value)| value.into_owned()).unwrap_or_default()
}

#[tokio::test]
async fn without_a_session_the_browser_goes_to_the_login() {
    let site = site();
    let reply = site.get("/admin/accounts/1000002/money", None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location, "/login?next=%2Fadmin%2Faccounts%2F1000002%2Fmoney");
    let reply = site.get("/admin/", Some("expired")).await;
    assert!(reply.location.starts_with("/login?next="));
}

#[tokio::test]
async fn a_player_who_is_not_an_admin_sees_nothing() {
    let site = site();
    for path in ["/admin/", "/admin/accounts/1000002", "/admin/audit"] {
        let reply = site.get(path, Some("player")).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.body, "Forbidden");
    }
    let reply = site.post("/admin/accounts/1000002/money", "player", &[("x-csrf-token", CSRF)]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(site.fake.calls().is_empty());
}

#[tokio::test]
async fn pages_render_with_values_escaped() {
    let site = site();
    let reply = site.get("/admin/?q=pilot", Some("admin")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("pilot&lt;script&gt;"));
    assert!(!reply.body.contains("pilot<script>"));
    for tab in ["", "/money", "/assets", "/ships", "/location", "/inventory", "/audit"] {
        let reply = site.get(&format!("/admin/accounts/{PLAYER}{tab}"), Some("admin")).await;
        assert_eq!(reply.status, StatusCode::OK, "{tab}");
        assert!(reply.body.contains(&format!(r#"value="{CSRF}""#)) || ["/ships", "/inventory", "/audit"].contains(&tab), "{tab}");
    }
    let missing = site.get("/admin/accounts/42", Some("admin")).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let css = site.get("/admin/static/admin.css", None).await;
    assert_eq!(css.status, StatusCode::OK);
}

#[tokio::test]
async fn a_first_visit_gets_a_csrf_cookie() {
    let site = site();
    let request = Request::get("/admin/").header(header::COOKIE, "sc_session=admin").body(Body::empty()).unwrap();
    let reply = site.send(request).await;
    assert_eq!(reply.status, StatusCode::OK);
    let cookie = reply.cookies.iter().find(|cookie| cookie.starts_with("sc_csrf=")).expect("a csrf cookie");
    let token = cookie.trim_start_matches("sc_csrf=").split(';').next().unwrap();
    assert_eq!(token.len(), 64);
    assert!(reply.body.contains(token) || !reply.body.contains("<form method=\"post\""));
}

#[tokio::test]
async fn a_change_without_the_csrf_token_is_refused() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/money");
    let fields = [("request_id", RID), ("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "5"), ("reason", "r")];
    let reply = site.post(&path, "admin", &fields).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let mut wrong = fields.to_vec();
    wrong.push(("x-csrf-token", "other"));
    assert_eq!(site.post(&path, "admin", &wrong).await.status, StatusCode::FORBIDDEN);
    assert!(site.fake.calls().is_empty());
    assert!(site.audit.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn money_changes_apply_once_and_are_audited() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/money");
    let fields = [("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "250"), ("reason", "compensation")];
    let reply = site.act(&path, &fields).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(notice(&reply).contains("1,000 → 1,250"), "{}", notice(&reply));
    let again = site.act(&path, &fields).await;
    assert!(notice(&again).contains("already applied"));
    assert_eq!(*site.fake.balance.lock().unwrap(), 1_250);
    assert_eq!(*site.fake.pushes.lock().unwrap(), 1, "the client is told once");
    assert_eq!(site.fake.calls()[0], format!("adjust admin:{RID} 250 {PLAYER_GEID}"));

    let records = site.audit.list(Some(PLAYER), 10).await.unwrap();
    assert_eq!(records.len(), 1, "one action, one row");
    let record = &records[0];
    assert_eq!((record.entry.action.as_str(), record.entry.actor_handle.as_str(), record.outcome.as_str()), ("money.adjust", "root", "ok"));
    assert_eq!(record.entry.detail["delta"], 250);
    assert_eq!(record.before.as_ref().unwrap()["balance"], 1_000);
    assert_eq!(record.after.as_ref().unwrap()["balance"], 1_250);
}

#[tokio::test]
async fn a_debit_beyond_the_balance_is_refused_and_recorded() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/money");
    let reply = site.act(&path, &[("ledger", "AUEC_LEDGER"), ("direction", "debit"), ("amount", "5000"), ("reason", "r")]).await;
    assert!(reply.location.contains("error=1"));
    assert!(notice(&reply).contains("insufficient funds"));
    let records = site.audit.list(None, 10).await.unwrap();
    assert!(records[0].outcome.starts_with("error"));
    for bad in [[("amount", "0")], [("amount", "-3")], [("amount", "x")]] {
        let mut fields = vec![("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("reason", "r")];
        fields.extend_from_slice(&bad);
        assert!(site.act(&path, &fields).await.location.contains("error=1"));
    }
    let no_reason = site.act(&path, &[("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "1")]).await;
    assert!(no_reason.location.contains("error=1"));
    assert_eq!(site.fake.calls().len(), 1, "only the first reached the ledger");
}

#[tokio::test]
async fn nothing_is_done_when_the_audit_cannot_be_written() {
    let site = site_with(MemoryAudit { broken: true, ..Default::default() });
    let reply = site
        .act(&format!("/admin/accounts/{PLAYER}/money"), &[("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "1"), ("reason", "r")])
        .await;
    assert!(notice(&reply).contains("nothing was done"));
    assert!(site.fake.calls().is_empty());
}

#[tokio::test]
async fn a_rental_grant_is_granted_and_rented_with_one_key() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/assets/grant");
    let fields = [("class_guid", "ABCDEF01-0000-0000-0000-000000000001"), ("source", "rental"), ("days", "3"), ("reason", "event")];
    let reply = site.act(&path, &fields).await;
    assert!(!reply.location.contains("error=1"), "{}", notice(&reply));
    let calls = site.fake.calls();
    assert_eq!(calls[0], format!("grant admin:{RID} 4 abcdef01-0000-0000-0000-000000000001"));
    assert_eq!(calls[1], format!("rent admin:{RID} urn:e:new 3"));
    let refused = site.act(&path, &[("class_guid", "x y"), ("source", "platform"), ("reason", "r")]).await;
    assert!(refused.location.contains("error=1"));
    let no_days = site.act(&path, &[("class_guid", "ab"), ("source", "rental"), ("reason", "r")]).await;
    assert!(no_days.location.contains("error=1"));
}

#[tokio::test]
async fn revoking_ends_the_entitlements_active_rental() {
    let site = site();
    site.fake.rentals.lock().unwrap().push(Rental {
        id: "r1".into(),
        entitlement_urn: "urn:e:1".into(),
        status: RentalStatus::Active as i32,
        ..Default::default()
    });
    let reply = site.act(&format!("/admin/accounts/{PLAYER}/assets/revoke"), &[("urn", "urn:e:1"), ("reason", "chargeback")]).await;
    assert!(notice(&reply).contains("Revoked"));
    assert_eq!(site.fake.calls(), [format!("revoke urn:e:1 admin:{RID}"), "end r1".to_string()]);
}

#[tokio::test]
async fn extending_a_rental_renews_it_without_a_charge() {
    let site = site();
    site.act(&format!("/admin/accounts/{PLAYER}/ships/extend"), &[("rental", "r1"), ("days", "7"), ("reason", "gift")]).await;
    assert_eq!(site.fake.calls(), [format!("extend admin:{RID} r1 7"), "expiry urn:e:1 true".to_string()]);
}

#[tokio::test]
async fn locations_change_only_while_the_player_is_off_a_shard() {
    let site = site();
    *site.fake.on_shard.lock().unwrap() = true;
    let page = site.get(&format!("/admin/accounts/{PLAYER}/location"), Some("admin")).await;
    assert!(page.body.contains("changes are refused"));
    assert!(!page.body.contains("Set home"));
    let path = format!("/admin/accounts/{PLAYER}/location/home");
    let fields = [("location", "2026442305"), ("revision", "0"), ("reason", "stuck")];
    let refused = site.act(&path, &fields).await;
    assert!(notice(&refused).contains("on a shard"));
    assert!(site.fake.calls().is_empty());
    assert!(site.audit.records.lock().unwrap().is_empty());

    *site.fake.on_shard.lock().unwrap() = false;
    let page = site.get(&format!("/admin/accounts/{PLAYER}/location"), Some("admin")).await;
    assert!(page.body.contains("take effect when the player next joins"));
    let done = site.act(&path, &fields).await;
    assert!(notice(&done).contains("next joins"), "{}", notice(&done));
    assert_eq!(site.fake.calls(), [format!("put player_home_location {PLAYER_GEID} Some(0)")]);
    // The form still says revision 0, which is no longer so.
    let stale = site
        .post(&path, "admin", &[("x-csrf-token", CSRF), ("request_id", "fedcba9876543210fedcba9876543210"), ("location", "2026442305"), ("revision", "0"), ("reason", "r")])
        .await;
    assert!(notice(&stale).contains("changed meanwhile"));
    let unknown = site.act(&path, &[("location", "12345"), ("revision", "1"), ("reason", "r")]).await;
    assert!(unknown.location.contains("error=1"));
}

#[tokio::test]
async fn an_admin_cannot_lock_themselves_out() {
    let site = site();
    let path = format!("/admin/accounts/{ADMIN}/state");
    assert!(site.act(&path, &[("op", "disable")]).await.location.contains("error=1"));
    assert!(site.act(&path, &[("op", "roles"), ("roles", "moderator")]).await.location.contains("error=1"));
    assert!(site.fake.calls().is_empty());
    let other = site.act(&format!("/admin/accounts/{PLAYER}/state"), &[("op", "password"), ("new_password", "correct horse battery")]).await;
    assert!(!other.location.contains("error=1"));
    let records = site.audit.list(Some(PLAYER), 10).await.unwrap();
    assert!(!records[0].entry.detail.to_string().contains("horse"), "the password stays out of the audit");
}

#[tokio::test]
async fn the_inventory_page_shows_the_tree_with_its_forms() {
    let site = site();
    let reply = site.get(&format!("/admin/accounts/{PLAYER}/inventory"), Some("admin")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = &reply.body;
    assert!(body.contains("Personal inventory"));
    assert!(body.contains("Item bank at RR_P6_LEO"), "a location bank is named by its place");
    assert!(body.contains(r#"class="inventory depth-1" id="inv-400000000001:Container:0""#), "the crate's inventory nests");
    assert!(body.contains("Container of crate&lt;1&gt; 400000000001"));
    assert!(!body.contains("crate<1>"), "values are escaped");
    assert!(body.contains("Ammo box"));
    assert!(body.contains(r##"href="#inv-400000000001:Container:0""##), "an item links the inventory it holds");
    assert!(body.contains(r#"name="geid" value="400000000002""#), "a stowed item can be moved and removed");
    assert!(body.contains("Held by something live on a shard"));
    assert!(!body.contains(r#"name="geid" value="901""#), "nothing in a live hold is changed");
    assert!(!body.contains(r#"<option value="900:Container:0">"#), "nor is anything moved or added into it");
    assert!(body.contains(r#"action="/admin/accounts/1000002/inventory/add""#));
    assert!(body.contains(&format!(r#"value="{CSRF}""#)));

    let search = site.get(&format!("/admin/accounts/{PLAYER}/inventory?class=crate"), Some("admin")).await;
    assert!(search.body.contains("Cargo crate"));
    assert!(search.body.contains("guid=c0ffee00-0000-0000-0000-000000000001#add"));
    assert_eq!(site.fake.calls(), ["classes crate"]);
    let picked = site.get(&format!("/admin/accounts/{PLAYER}/inventory?class=crate&guid=c0ffee00-0000-0000-0000-000000000001"), Some("admin")).await;
    assert!(picked.body.contains(r#"name="class_guid" required size="38" value="c0ffee00-0000-0000-0000-000000000001""#));
}

#[tokio::test]
async fn a_split_move_applies_once_and_is_audited_with_before_and_after() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/inventory/move");
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let fields = [
        ("geid", "400000000002"),
        ("from", "400000000001:Container:0"),
        ("to", bank.as_str()),
        ("quantity", "2"),
        ("reason", "stuck item"),
    ];
    let reply = site.act(&path, &fields).await;
    assert!(!reply.location.contains("error=1"), "{}", notice(&reply));
    assert!(notice(&reply).contains("Moved 2 of 400000000002 as 400000000009"), "{}", notice(&reply));
    let again = site.act(&path, &fields).await;
    assert!(notice(&again).contains("already applied"));
    let call = format!("move admin:{RID} {PLAYER_GEID} 400000000002 400000000001:Container:0 -> {bank} x2");
    assert_eq!(site.fake.calls(), [call.clone(), call]);

    let records = site.audit.list(Some(PLAYER), 10).await.unwrap();
    assert_eq!(records.len(), 1, "one action, one row");
    let record = &records[0];
    assert_eq!((record.entry.action.as_str(), record.entry.owner.as_str(), record.outcome.as_str()), ("inventory.move", "sc-entitygraph", "ok"));
    assert_eq!(record.entry.target_geid, Some(PLAYER_GEID));
    assert_eq!(record.entry.detail["quantity"], 2);
    let before = record.before.as_ref().unwrap();
    assert_eq!((before["stack"].clone(), before["inventory"].clone()), (serde_json::json!(5), serde_json::json!("400000000001:Container:0")));
    let after = record.after.as_ref().unwrap();
    assert_eq!((after["geid"].clone(), after["split_from"].clone()), (serde_json::json!(400_000_000_009_u64), serde_json::json!(400_000_000_002_u64)));
}

#[tokio::test]
async fn adding_and_removing_reach_entitygraph_for_the_player() {
    let site = site();
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let add = [("inventory", bank.as_str()), ("class_guid", "C0FFEE00-0000-0000-0000-000000000001"), ("stack", "3"), ("reason", "event prize")];
    let reply = site.act(&format!("/admin/accounts/{PLAYER}/inventory/add"), &add).await;
    assert!(notice(&reply).contains("Added 400000000010"), "{}", notice(&reply));
    assert_eq!(site.fake.calls(), [format!("add admin:{RID} {PLAYER_GEID} {bank} c0ffee00-0000-0000-0000-000000000001 x3")]);
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!((record.entry.action.as_str(), record.outcome.as_str()), ("inventory.add", "ok"));
    assert_eq!(record.after.unwrap()["geid"], 400_000_000_010_u64);

    let other = site_with(MemoryAudit::default());
    let remove = [("geid", "400000000001"), ("from", bank.as_str()), ("reason", "duplicate")];
    let reply = other.act(&format!("/admin/accounts/{PLAYER}/inventory/remove"), &remove).await;
    assert!(notice(&reply).contains("2 entities erased"), "{}", notice(&reply));
    assert_eq!(other.fake.calls(), [format!("remove admin:{RID} {PLAYER_GEID} 400000000001")]);
    let record = other.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!(record.entry.action, "inventory.remove");
    assert_eq!(record.before.unwrap()["holds"], serde_json::json!(["400000000001:Container:0"]), "before: what it held");
    assert_eq!(record.after.unwrap()["erased"], 2);
}

#[tokio::test]
async fn an_owners_refusal_is_shown_and_recorded() {
    let site = site();
    let add = [("inventory", "900:Container:0"), ("class_guid", "c0ffee00"), ("stack", "1"), ("reason", "r")];
    let reply = site.act(&format!("/admin/accounts/{PLAYER}/inventory/add"), &add).await;
    assert!(reply.location.contains("error=1"));
    assert!(notice(&reply).contains("live on a shard"), "{}", notice(&reply));
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert!(record.outcome.starts_with("error"), "{}", record.outcome);
}

#[tokio::test]
async fn inventory_changes_wait_until_the_player_is_off_a_shard() {
    let site = site();
    *site.fake.on_shard.lock().unwrap() = true;
    let page = site.get(&format!("/admin/accounts/{PLAYER}/inventory"), Some("admin")).await;
    assert!(page.body.contains("changes are refused"));
    assert!(!page.body.contains("Add an item"));
    assert!(!page.body.contains(r#"name="geid""#), "no item forms");
    assert!(page.body.contains("Ammo box"), "the inventories are still shown");
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let posts = [
        ("move", vec![("geid", "400000000002"), ("from", "400000000001:Container:0"), ("to", bank.as_str()), ("reason", "r")]),
        ("add", vec![("inventory", bank.as_str()), ("class_guid", "c0ffee00"), ("reason", "r")]),
        ("remove", vec![("geid", "400000000002"), ("reason", "r")]),
    ];
    for (action, fields) in &posts {
        let refused = site.act(&format!("/admin/accounts/{PLAYER}/inventory/{action}"), fields).await;
        assert!(notice(&refused).contains("on a shard"), "{action}: {}", notice(&refused));
    }
    assert!(site.fake.calls().is_empty());
    assert!(site.audit.records.lock().unwrap().is_empty(), "nothing was asked, so nothing is recorded");
}

#[tokio::test]
async fn bad_inventory_forms_are_refused_before_anything_is_asked() {
    let site = site();
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let path = |action: &str| format!("/admin/accounts/{PLAYER}/inventory/{action}");
    let refused = [
        (path("move"), vec![("geid", "400000000002"), ("from", bank.as_str()), ("to", bank.as_str()), ("reason", "r")]),
        (path("move"), vec![("geid", "400000000002"), ("from", bank.as_str()), ("to", "not:an inventory"), ("reason", "r")]),
        (path("move"), vec![("geid", "x"), ("from", bank.as_str()), ("to", "1:Location:2"), ("reason", "r")]),
        (path("move"), vec![("geid", "400000000002"), ("from", bank.as_str()), ("to", "1:Location:2"), ("quantity", "-1"), ("reason", "r")]),
        (path("move"), vec![("geid", "400000000002"), ("from", bank.as_str()), ("to", "1:Location:2")]),
        (path("add"), vec![("inventory", bank.as_str()), ("class_guid", "not a guid"), ("reason", "r")]),
        (path("add"), vec![("inventory", bank.as_str()), ("class_guid", "c0ffee00"), ("stack", "0"), ("reason", "r")]),
        (path("add"), vec![("inventory", ""), ("class_guid", "c0ffee00"), ("reason", "r")]),
        (path("remove"), vec![("geid", "0"), ("reason", "r")]),
    ];
    for (path, fields) in &refused {
        let reply = site.act(path, fields).await;
        assert!(reply.location.contains("error=1"), "{path} {fields:?}");
    }
    assert!(site.fake.calls().is_empty());
    let no_csrf = site.post(&path("remove"), "admin", &[("request_id", RID), ("geid", "400000000002"), ("reason", "r")]).await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_player_is_the_one_sc_identity_names() {
    let site = site();
    let page = site.get(&format!("/admin/accounts/{NO_PLAYER}/inventory"), Some("admin")).await;
    assert!(page.body.contains("knows no player"));
    let refused = site.act(&format!("/admin/accounts/{NO_PLAYER}/money"), &[("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "5"), ("reason", "r")]).await;
    assert!(notice(&refused).contains("No player"));
    let overview = site.get(&format!("/admin/accounts/{PLAYER}"), Some("admin")).await;
    assert!(overview.body.contains(&format!("player <span class=\"mono\">{PLAYER_GEID}</span>")));
    assert!(site.fake.calls().is_empty());
}
