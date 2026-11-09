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
use sc_api::internal::{ AcquireShipRequest, AcquireShipResponse, ListShipsResponse, PadSize, ReportShipRequest, ReportShipResponse, ShipClaim, ShipInsurance, ShipPolicy, ShipRecord, ShipState, ShipView, AccountSummary, GameBadge, AdminAddItemRequest, AdminEquipItemRequest, AdminEquipItemResponse, AdminEquipPort, AdminEquipmentResponse, AdminUnequipItemRequest, AdminUnequipItemResponse, AdminWornItem, AdminAddItemResponse, AdminEntityClass, AdminInventory, AdminInventoryItem, AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse, CreateRentalRequest, EndRentalRequest, EndRentalResponse, EntitlementHistoryEntry, ExtendRentalRequest, ExtendRentalResponse, GrantPurchaseRequest, LedgerAdjustRequest, LedgerAdjustResponse, LedgerBalance, LedgerOperation, PlayerPresence, PlayerShard, PutEntityDocumentRequest, PutEntityDocumentResponse, RevokeEntitlementRequest, RevokeEntitlementResponse, SearchAccountsRequest, SearchAccountsResponse, SetAccountStateRequest, SetRentalExpiryRequest, StoredEntityDocument, VerifyWebSessionResponse, };
use sc_proto::{
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
    backend::{ClassSearch, Identity, Owners},
    web::{router, App},
};

const ADMIN: u32 = 1_000_001;
const PLAYER: u32 = 1_000_002;
const PLAYER_GEID: u64 = 300_000_000_002;
/// A class that stacks (a consumable); the crate the fake finds otherwise does not.
const MEDPEN_GUID: &str = "0a0b0c0d-0000-0000-0000-000000000002";
/// An account sc-identity knows no player for.
const NO_PLAYER: u32 = 1_000_003;
/// A ship class (the fake gives it for the vehicle filter), with an `<` in its name to see it escaped.
const SHIP_GUID: &str = "5a1b0000-0000-0000-0000-000000000001";
const ORISON: u32 = 2_026_442_305;
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
    /// The ships sc-entitygraph keeps, and the commands it applied.
    ships: Mutex<Vec<ShipRecord>>,
    ship_commands: Mutex<HashMap<String, u64>>,
    /// How long every read of an owner takes, to show the pages ask the owners together.
    latency: Mutex<std::time::Duration>,
    /// Extra items in the item bank's inventory, to page.
    extra_items: Mutex<u32>,
}

impl Fake {
    async fn wait(&self) {
        let latency = *self.latency.lock().unwrap();
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
    }

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

    async fn game_badges(&self) -> Result<Vec<GameBadge>, Status> {
        Ok(vec![GameBadge { id: 1, name: "RSI_STAFF".into() }, GameBadge { id: 0x67, name: "QA".into() }, GameBadge { id: 0x68, name: "SUBSCRIBER".into() }])
    }

    async fn set_account_state(&self, request: SetAccountStateRequest) -> Result<AccountSummary, Status> {
        if request.set_game_badges {
            self.called(format!("badges {:?}", request.game_badges));
        }
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
    if let Some(properties) = crate_inventory.inventory.as_mut() {
        use sc_proto::services::entitygraph::v1::{inventory_configuration::Configuration, InventoryConfiguration, PhysicalInventoryConfiguration};
        properties.configuration = Some(InventoryConfiguration { configuration: Some(Configuration::Physical(PhysicalInventoryConfiguration { capacity: 2_000_000, occupancy: 500_000 })) });
    }
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
        self.wait().await;
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
        self.wait().await;
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
        self.wait().await;
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
        self.wait().await;
        let on = *self.on_shard.lock().unwrap();
        Ok(on.then(|| PlayerShard { shard_id: "usa-crimson-falcon".into(), location_id: 2_026_442_305, ..Default::default() }))
    }

    async fn presence(&self, _: u32) -> Result<Option<PlayerPresence>, Status> {
        self.wait().await;
        Ok(None)
    }

    async fn inventory_tree(&self, geid: u64) -> Result<Vec<AdminInventory>, Status> {
        self.wait().await;
        assert_eq!(geid, PLAYER_GEID, "the GEID comes from the account summary");
        let mut tree = fake_tree();
        let extra = *self.extra_items.lock().unwrap();
        let bank = tree[1].inventory.as_ref().unwrap().id.clone();
        for at in 0..extra {
            tree[1].items.push(AdminInventoryItem { geid: 410_000_000_000 + u64::from(at), class_name: format!("pile{at:03}"), stack_size: 1, owner_id: PLAYER_GEID, inventory_id: bank.clone(), ..Default::default() });
        }
        tree[1].item_count += extra;
        Ok(tree)
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

    async fn equipment(&self, geid: u64) -> Result<AdminEquipmentResponse, Status> {
        self.wait().await;
        assert_eq!(geid, PLAYER_GEID);
        let worn = |geid: u64, name: &str, kind: &str, parent: u64, port: &str, depth: u32, removable: bool| AdminWornItem {
            geid,
            class_name: name.into(),
            display_name: name.to_uppercase(),
            item_type: kind.into(),
            parent_geid: parent,
            port: port.into(),
            depth,
            removable,
            ..Default::default()
        };
        Ok(AdminEquipmentResponse {
            worn: vec![
                worn(700, "body", "Char_Body", PLAYER_GEID, "Body_ItemPort", 1, false),
                worn(701, "undersuit", "Char_Armor_Undersuit", 700, "Armor_Undersuit", 2, true),
                worn(702, "pistol", "WeaponPersonal", 701, "wep_sidearm", 3, true),
            ],
            ports: vec![
                AdminEquipPort { parent_geid: 701, parent_name: "UNDERSUIT".into(), port: "wep_sidearm".into(), occupant_geid: 702, occupant_name: "PISTOL".into(), candidates: vec![400_000_000_002] },
                AdminEquipPort { parent_geid: 701, parent_name: "UNDERSUIT".into(), port: "medPen_attach_1".into(), candidates: vec![400_000_000_002], ..Default::default() },
            ],
        })
    }

    async fn equip_item(&self, request: AdminEquipItemRequest) -> Result<AdminEquipItemResponse, Status> {
        self.called(format!("equip {} {} {} {} {}", request.command_id, request.owner_id, request.geid, request.parent_geid, request.port));
        let mut applied = self.inventory_commands.lock().unwrap();
        let replayed = applied.contains_key(&request.command_id);
        applied.entry(request.command_id).or_insert(request.geid);
        Ok(AdminEquipItemResponse { replayed })
    }

    async fn unequip_item(&self, request: AdminUnequipItemRequest) -> Result<AdminUnequipItemResponse, Status> {
        self.called(format!("unequip {} {} {} -> {}", request.command_id, request.owner_id, request.geid, request.to_inventory_id));
        let mut applied = self.inventory_commands.lock().unwrap();
        let replayed = applied.contains_key(&request.command_id);
        applied.entry(request.command_id).or_insert(request.geid);
        Ok(AdminUnequipItemResponse { replayed })
    }

    async fn search_classes(&self, search: ClassSearch) -> Result<Vec<AdminEntityClass>, Status> {
        self.wait().await;
        let pairs = |list: &[(String, Vec<String>)]| list.iter().map(|(item_type, subs)| if subs.is_empty() { item_type.clone() } else { format!("{item_type}:{}", subs.join("|")) }).collect::<Vec<_>>().join("+");
        self.called(format!("classes {} {} {} {} {}", search.query, pairs(&search.any_of), pairs(&search.none_of), search.item_sub_types.join("+"), search.named_only));
        if search.query.starts_with(MEDPEN_GUID) {
            return Ok(vec![AdminEntityClass {
                guid: MEDPEN_GUID.into(),
                class_name: "medpen_01".into(),
                display_name: "MedPen".into(),
                item_type: "FPS_Consumable".into(),
                item_sub_type: "MedPack".into(),
                stackable: true,
                ..Default::default()
            }]);
        }
        if search.query.starts_with("dead") {
            return Ok(Vec::new());
        }
        if search.query.starts_with(SHIP_GUID) || search.any_of.iter().any(|(item_type, _)| item_type == "NOITEM_Vehicle") {
            return Ok(vec![AdminEntityClass {
                guid: SHIP_GUID.into(),
                class_name: "AEGS_Avenger_Titan".into(),
                display_name: "Avenger <Titan>".into(),
                item_type: "NOITEM_Vehicle".into(),
                ..Default::default()
            }]);
        }
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

    async fn list_ships(&self, geid: u64) -> Result<ListShipsResponse, Status> {
        self.wait().await;
        assert_eq!(geid, PLAYER_GEID);
        let ships = self.ships.lock().unwrap().iter().map(|ship| ShipView { ship: Some(ship.clone()), ..Default::default() }).collect();
        Ok(ListShipsResponse { ships, repaired: 2, orphans: 1 })
    }

    async fn acquire_ship(&self, request: AcquireShipRequest) -> Result<AcquireShipResponse, Status> {
        let insurance = request.insurance.clone().unwrap_or_default();
        self.called(format!(
            "acquire {} {} {} at {} policy {} expires {} pad {}",
            request.command_id, request.owner_id, request.entity_class_guid, request.location_id, insurance.policy, insurance.expires_at_ms, request.pad_size
        ));
        let mut applied = self.ship_commands.lock().unwrap();
        let replayed = applied.contains_key(&request.command_id);
        let geid = 500_000_000_000 + applied.len() as u64;
        let geid = *applied.entry(request.command_id.clone()).or_insert(geid);
        let record = ShipRecord {
            ship_uid: format!("ship:{}", request.command_id),
            owner_id: request.owner_id,
            entity_class_guid: request.entity_class_guid,
            state: ShipState::Stored as i32,
            insurance: Some(insurance),
            pad_size: request.pad_size,
            location_id: request.location_id,
            geid,
            graph_synced: true,
            ..Default::default()
        };
        if !replayed {
            self.ships.lock().unwrap().push(record.clone());
        }
        Ok(AcquireShipResponse { ship: Some(ShipView { ship: Some(record), ..Default::default() }), replayed, graph_synced: true })
    }

    async fn report_ship(&self, request: ReportShipRequest) -> Result<ReportShipResponse, Status> {
        self.called(format!("report {} {} {} {} at {}", request.command_id, request.owner_id, request.geid, request.state, request.location_id));
        let mut ships = self.ships.lock().unwrap();
        let ship = ships.iter_mut().find(|ship| ship.geid == request.geid).ok_or_else(|| Status::not_found("ship"))?;
        if ship.state != ShipState::Deployed as i32 {
            return Err(Status::failed_precondition("the ship is not deployed"));
        }
        ship.state = request.state;
        Ok(ReportShipResponse { ship: Some(ShipView { ship: Some(ship.clone()), ..Default::default() }), replayed: false })
    }

    async fn funds_changed(&self, _: u32, _: u64, _: &str, _: i64, _: u64) {
        *self.pushes.lock().unwrap() += 1;
    }
}

struct Site {
    router: Router,
    fake: Arc<Fake>,
    audit: Arc<MemoryAudit>,
    app: Arc<App>,
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
        flashes: Default::default(),
        class_names: Default::default(),
    });
    Site { router: router(app.clone()), fake, audit, app }
}

fn site() -> Site {
    site_with(MemoryAudit::default())
}

struct Reply {
    status: StatusCode,
    location: String,
    cookies: Vec<String>,
    body: String,
    /// The notice an action left for the admin's next page read, as that read's query.
    flash: String,
}

impl Reply {
    /// Whether the action's notice is an error.
    fn flagged(&self) -> bool {
        self.flash.ends_with("&error=1")
    }
}

impl Site {
    async fn send(&self, request: Request<Body>) -> Reply {
        let changes = request.method() != axum::http::Method::GET;
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response.headers().get(header::LOCATION).map_or(String::new(), |v| v.to_str().unwrap().to_string());
        let cookies: Vec<String> = response.headers().get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).collect();
        let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        let flash = if changes { self.app.flashes.lock().unwrap().values().map(|(_, text)| text.clone()).next().unwrap_or_default() } else { String::new() };
        Reply { status, location, cookies, body, flash }
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
    let query = reply.flash.as_str();
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
    for tab in ["", "/badges", "/money", "/assets", "/ships", "/location", "/inventory", "/audit"] {
        let reply = site.get(&format!("/admin/accounts/{PLAYER}{tab}"), Some("admin")).await;
        assert_eq!(reply.status, StatusCode::OK, "{tab}");
        assert!(reply.body.contains(&format!(r#"value="{CSRF}""#)) || ["/ships", "/inventory", "/audit"].contains(&tab) || tab == "/badges", "{tab}");
        assert!(!reply.body.contains("/hangar"), "{tab}: the Hangar tab is part of Ships now");
    }
    // The old Hangar address leads to Ships, with its query.
    let moved = site.get(&format!("/admin/accounts/{PLAYER}/hangar?q=aven"), Some("admin")).await;
    assert_eq!((moved.status, moved.location.as_str()), (StatusCode::SEE_OTHER, format!("/admin/accounts/{PLAYER}/ships?q=aven").as_str()));
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
    assert!(reply.flagged());
    assert!(notice(&reply).contains("insufficient funds"));
    let records = site.audit.list(None, 10).await.unwrap();
    assert!(records[0].outcome.starts_with("error"));
    for bad in [[("amount", "0")], [("amount", "-3")], [("amount", "x")]] {
        let mut fields = vec![("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("reason", "r")];
        fields.extend_from_slice(&bad);
        assert!(site.act(&path, &fields).await.flagged());
    }
    let no_reason = site.act(&path, &[("ledger", "AUEC_LEDGER"), ("direction", "credit"), ("amount", "1")]).await;
    assert!(no_reason.flagged());
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
    assert!(!reply.flagged(), "{}", notice(&reply));
    let calls = site.fake.calls();
    assert_eq!(calls[0], format!("grant admin:{RID} 4 abcdef01-0000-0000-0000-000000000001"));
    assert_eq!(calls[1], format!("rent admin:{RID} urn:e:new 3"));
    let refused = site.act(&path, &[("class_guid", "x y"), ("source", "platform"), ("reason", "r")]).await;
    assert!(refused.flagged());
    let no_days = site.act(&path, &[("class_guid", "ab"), ("source", "rental"), ("reason", "r")]).await;
    assert!(no_days.flagged());
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
    assert!(unknown.flagged());
}

/// The account page offers every badge, marks the developer ones, and a change sends the ticked ones as a set.
#[tokio::test]
async fn an_admin_sets_an_accounts_in_game_badges() {
    let site = site();
    let page = site.get(&format!("/admin/accounts/{PLAYER}/badges"), Some("admin")).await;
    assert!(page.body.contains(r#"name="badge_103""#) && page.body.contains("STAFF") && page.body.contains("SUBSCRIBER"));
    assert!(page.body.contains(r#"class="tag heavy">developer"#), "the developer ones are marked");
    assert!(!site.get(&format!("/admin/accounts/{PLAYER}"), Some("admin")).await.body.contains("badge_103"), "not on the overview");
    let path = format!("/admin/accounts/{PLAYER}/state");
    let done = site.act(&path, &[("op", "badges"), ("badge_1", "1"), ("badge_103", "1"), ("badge_104", "1"), ("badge_66", "1")]).await;
    assert!(!done.flagged());
    assert_eq!(site.fake.calls()[0], "badges [1, 103, 104]", "only known badges, in the catalog's order");
    let records = site.audit.list(Some(PLAYER), 10).await.unwrap();
    assert!(records[0].entry.detail.to_string().contains("QA"));
}

#[tokio::test]
async fn an_admin_cannot_lock_themselves_out() {
    let site = site();
    let path = format!("/admin/accounts/{ADMIN}/state");
    assert!(site.act(&path, &[("op", "disable")]).await.flagged());
    assert!(site.act(&path, &[("op", "roles"), ("roles", "moderator")]).await.flagged());
    assert!(site.fake.calls().is_empty());
    let other = site.act(&format!("/admin/accounts/{PLAYER}/state"), &[("op", "password"), ("new_password", "correct horse battery")]).await;
    assert!(!other.flagged());
    let records = site.audit.list(Some(PLAYER), 10).await.unwrap();
    assert!(!records[0].entry.detail.to_string().contains("horse"), "the password stays out of the audit");
}

#[tokio::test]
async fn the_catalog_asks_for_the_classes_of_the_chosen_category_and_its_sub_filters() {
    let site = site();
    let sidearms = site.get(&format!("/admin/accounts/{PLAYER}/inventory/classes?q=cod&cat=weapons&opt=sidearms"), Some("admin")).await;
    assert_eq!(sidearms.status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&sidearms.body).unwrap();
    assert_eq!(json["classes"][0]["guid"], "c0ffee00-0000-0000-0000-000000000001");
    assert_eq!(json["classes"][0]["container"], true);
    assert_eq!(json["classes"][0]["stackLimit"], 0, "a crate does not stack");
    let calls = || site.fake.calls().into_iter().filter(|call| call.starts_with("classes ")).collect::<Vec<_>>();
    assert_eq!(calls().last().unwrap(), "classes cod WeaponPersonal:Small   true", "only sidearms, and only classes with an English name");
    async fn get(site: &Site, query: &str) -> Reply {
        site.get(&format!("/admin/accounts/{PLAYER}/inventory/classes?{query}"), Some("admin")).await
    }

    // An armor's part and weight together; a group with nothing chosen does not narrow.
    get(&site, "cat=armor&opt=core,arms,heavy").await;
    assert_eq!(calls().last().unwrap(), "classes  Char_Armor_Torso+Char_Armor_Arms  Heavy true");
    get(&site, "cat=armor&opt=light").await;
    assert_eq!(
        calls().last().unwrap(),
        "classes  Char_Armor_Undersuit+Char_Armor_Helmet+Char_Armor_Torso+Char_Armor_Arms+Char_Armor_Legs+Char_Armor_Backpack+Char_Flair  Light+LightArmor true"
    );
    // No category: any item. A choice of another category is not taken. Other leaves out what the categories hold.
    get(&site, "q=x").await;
    assert_eq!(calls().last().unwrap(), "classes x    true");
    get(&site, "cat=ammo&opt=heavy").await;
    assert_eq!(calls().last().unwrap(), "classes  WeaponAttachment:Magazine   true");
    get(&site, "cat=other").await;
    let other = calls().last().unwrap().clone();
    assert!(other.starts_with("classes   WeaponPersonal:") && other.contains("+Char_Armor_Torso+") && other.ends_with(" true"), "{other}");
    assert_eq!(get(&site, "cat=nonsense").await.status, StatusCode::BAD_REQUEST);

    // The stack limit of a class that stacks.
    let medpen = get(&site, &format!("q={MEDPEN_GUID}")).await;
    let json: serde_json::Value = serde_json::from_str(&medpen.body).unwrap();
    assert_eq!(json["classes"][0]["stackLimit"], 99);

    let anonymous = site.get(&format!("/admin/accounts/{PLAYER}/inventory/classes"), None).await;
    assert_ne!(anonymous.status, StatusCode::OK, "only for an admin");
    let script = site.get("/admin/static/inventory.js", None).await;
    assert_eq!(script.status, StatusCode::OK);
    assert!(script.body.contains("textContent") && !script.body.contains("innerHTML"), "built from text nodes");
}

#[tokio::test]
async fn a_batch_gives_every_item_under_one_reason_and_only_stackable_ones_take_an_amount() {
    let site = site();
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let crate_guid = "c0ffee00-0000-0000-0000-000000000001";
    let path = format!("/admin/accounts/{PLAYER}/inventory/add-batch");
    // Two crates (separate items), and five medpens in one stack.
    let items = format!("{crate_guid}:1:2,{MEDPEN_GUID}:5:1");
    let fields = [("inventory", bank.as_str()), ("items", items.as_str()), ("reason", "event prize")];
    let reply = site.act(&path, &fields).await;
    assert!(notice(&reply).contains("Added 3 items") && notice(&reply).contains("2x Cargo crate") && notice(&reply).contains("5 MedPen"), "{}", notice(&reply));
    let calls = site.fake.calls();
    assert_eq!(calls.iter().filter(|call| call.starts_with("add ")).count(), 3);
    assert!(calls.contains(&format!("add admin:{RID}:0 {PLAYER_GEID} {bank} {crate_guid} x1")));
    assert!(calls.contains(&format!("add admin:{RID}:1 {PLAYER_GEID} {bank} {crate_guid} x1")), "a copy is its own item");
    assert!(calls.contains(&format!("add admin:{RID}:2 {PLAYER_GEID} {bank} {MEDPEN_GUID} x5")), "a stack is one item");
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!((record.entry.action.as_str(), record.outcome.as_str()), ("inventory.add", "ok"));
    assert_eq!(record.after.unwrap()["items"].as_array().unwrap().len(), 3);

    // An amount for an item that does not stack, above a stack's limit, an unknown class, nothing at all: nothing added.
    let refused = site_with(MemoryAudit::default());
    for (items, expected) in [
        (format!("{crate_guid}:3"), "does not stack"),
        (format!("{MEDPEN_GUID}:100"), "stacks to 99"),
        ("dead0000-0000-0000-0000-000000000000".to_string(), "No class"),
        (String::new(), "Add some items"),
        (format!("{crate_guid}:1:21"), "number from 1 to 20"),
    ] {
        let fields = [("inventory", bank.as_str()), ("items", items.as_str()), ("reason", "r")];
        let reply = refused.act(&path, &fields).await;
        assert!(reply.flagged() && notice(&reply).contains(expected), "{items}: {}", notice(&reply));
    }
    assert!(refused.fake.calls().iter().all(|call| !call.starts_with("add ")), "nothing was added");
    let no_reason = [("inventory", bank.as_str()), ("items", crate_guid), ("reason", "")];
    assert!(refused.act(&path, &no_reason).await.flagged());
}

#[tokio::test]
async fn the_inventory_page_shows_the_tree_with_its_forms() {
    let site = site();
    let reply = site.get(&format!("/admin/accounts/{PLAYER}/inventory"), Some("admin")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = &reply.body;
    assert!(body.contains("Personal inventory"));
    assert!(body.contains("Item bank at RR_P6_LEO"), "a location bank is named by its place");
    let personal_id = format!("{PLAYER_GEID}:PlayerInventory:0");
    assert!(body.contains(&format!(r#"id="inv-{personal_id}" open"#)), "the personal inventory is open");
    assert!(body.contains(r#"<details class="node inventory" id="inv-400000000001:Container:0">"#), "the crate's inventory is collapsed");
    assert!(body.contains(r#"<details class="node inventory" id="inv-"#) && !body.contains(r#"id="inv-2026442305"#));
    let (holder, nested) = (body.find(r#"id="item-400000000001""#).unwrap(), body.find(r#"id="inv-400000000001:Container:0""#).unwrap());
    assert!(holder < nested && body[holder..nested].contains("<summary>"), "the crate's inventory is nested under the crate");
    assert!(body.contains(r#"<details class="node item-node" id="item-400000000001">"#), "a crate with an inventory is a collapsed branch");
    assert!(body.contains(r#"<details class="node character" id="character" open>"#) && body.contains("Character & storage".replace('&', "&amp;").as_str()));
    assert!(body.contains(r#"<span class="count">"#), "counts in the headings");
    assert!(body.contains("<meter ") || !body.contains("capacity"), "occupancy is a bar");
    assert!(body.contains(r#"data-label="crate&lt;1&gt;""#), "a container is named by the item that holds it, not by a GEID");
    assert!(body.contains(r#"<span class="name" title="c0ffee00-0000-0000-0000-000000000001">crate&lt;1&gt;</span>"#), "an item is named, with its class GUID on hover");
    assert!(body.contains(r#"<select name="inventory" data-tree>"#) && body.contains(r#"data-parent="400000000001:Location:2026442305""#.replace("400000000001", &PLAYER_GEID.to_string()).as_str()), "the picker is a tree: a container knows its parent");
    assert!(body.contains(r#"<script src="/admin/static/tree.js" defer></script>"#));
    assert!(body.contains(r#"<summary title="RR_P6_LEO (Pyro, 2026442305) › Item bank › crate&lt;1&gt;">"#), "a branch says its path from the top");
    assert!(body.contains(r#"data-path="RR_P6_LEO (Pyro, 2026442305) › Item bank""#), "the picker knows each path");
    assert!(body.contains("› crate&lt;1&gt;"), "a container's path ends with its own name, escaped");
    assert!(body.contains(r#"href="/admin/accounts/1000002/inventory/item?geid=400000000002">Move…</a>"#), "Move is on the item's page: no row carries every inventory");
    assert!(!body.contains(r#"action="/admin/accounts/1000002/inventory/move""#), "so the page stays small");
    assert!(body.contains(r#"<details class="actions"><summary>Actions</summary>"#) && body.contains(r#"action="/admin/accounts/1000002/inventory/remove""#));
    let item = site.get(&format!("/admin/accounts/{PLAYER}/inventory/item?geid=400000000002"), Some("admin")).await;
    assert_eq!(item.status, StatusCode::OK);
    assert!(item.body.contains(r#"<form method="post" action="/admin/accounts/1000002/inventory/move" class="stacked">"#) && item.body.contains(r#"<select name="to" data-tree>"#));
    assert!(item.body.contains(r#"name="from" value="400000000001:Container:0""#) && item.body.contains("Ammo box"));
    assert!(item.body.contains(r#"action="/admin/accounts/1000002/inventory/equip""#) && item.body.contains(r#"action="/admin/accounts/1000002/inventory/remove""#));
    assert!(item.body.contains(r#"<script src="/admin/static/tree.js" defer></script>"#));
    let missing = site.get(&format!("/admin/accounts/{PLAYER}/inventory/item?geid=42"), Some("admin")).await;
    assert!(missing.body.contains("has no item 42"));    let tree_js = site.get("/admin/static/tree.js", None).await;
    assert_eq!(tree_js.status, StatusCode::OK);
    assert!(tree_js.body.contains("createTextNode") && !tree_js.body.contains("innerHTML"), "built from text nodes");
    assert!(!body.contains("crate<1>"), "values are escaped");
    assert!(body.contains("Ammo box"));
    assert!(body.contains(r#"name="geid" value="400000000002""#), "a stowed item can be moved and removed");
    assert!(body.contains("Held by something live on a shard"));
    assert!(!body.contains(r#"name="geid" value="901""#), "nothing in a live hold is changed");
    assert!(!body.contains(r#"<option value="900:Container:0">"#), "nor is anything moved or added into it");
    assert!(body.contains(r#"action="/admin/accounts/1000002/inventory/add-batch""#), "one form for the whole batch");
    assert!(body.contains(r#"<div id="cart" class="cart"></div>"#) && body.contains(r#"name="items""#));
    assert!(body.contains(&format!(r#"value="{CSRF}""#)));
    assert!(body.contains(r#"<div id="catalog" data-url="/admin/accounts/1000002/inventory/classes""#), "the filtering catalog");
    assert!(body.contains(r#"<script src="/admin/static/inventory.js" defer></script>"#) && !body.contains("<script>"), "no inline script");
    // The item banks the player has none at yet can be picked: sc-entitygraph makes them on the first item.
    let places = crate::places::places();
    let fresh = places.iter().find(|place| place.id != 2_026_442_305).expect("a home location");
    assert!(body.contains(&format!(r#"<option value="{PLAYER_GEID}:Location:{}" data-parent="" data-label="Item bank at "#, fresh.id)), "a new bank is offered");
    assert!(body.contains("(new)"));
    let existing = format!(r#"<option value="{PLAYER_GEID}:Location:2026442305" "#);
    assert!(body.split(&existing).skip(1).all(|rest| !rest.split("</option>").next().unwrap().contains("(new)")), "a bank it has is not new");

    let search = site.get(&format!("/admin/accounts/{PLAYER}/inventory?class=crate"), Some("admin")).await;
    assert!(search.body.contains("Cargo crate"));
    assert!(search.body.contains("guid=c0ffee00-0000-0000-0000-000000000001#add"));
    assert_eq!(site.fake.calls(), ["classes crate    false"]);
    let picked = site.get(&format!("/admin/accounts/{PLAYER}/inventory?class=crate&guid=c0ffee00-0000-0000-0000-000000000001"), Some("admin")).await;
    assert!(picked.body.contains(r#"name="items" id="items" required size="60" placeholder="Class GUID:amount, ..." value="c0ffee00-0000-0000-0000-000000000001""#));
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
    assert!(!reply.flagged(), "{}", notice(&reply));
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
    assert!(reply.flagged());
    assert!(notice(&reply).contains("live on a shard"), "{}", notice(&reply));
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert!(record.outcome.starts_with("error"), "{}", record.outcome);
}

#[tokio::test]
async fn the_page_lists_what_is_worn_and_offers_only_fitting_free_ports() {
    let site = site();
    let page = site.get(&format!("/admin/accounts/{PLAYER}/inventory"), Some("admin")).await;
    let body = &page.body;
    assert!(body.contains(r#"id="character" open"#) && body.contains("3 worn items"), "the character is at the top, with its gear");
    let (body_at, suit_at, pistol_at) = (body.find("BODY").unwrap(), body.find("UNDERSUIT").unwrap(), body.find("PISTOL").unwrap());
    let storage_at = body.find(r#"class="storage""#).unwrap();
    assert!(body_at < suit_at && suit_at < pistol_at && pistol_at < storage_at, "worn gear nests under what it hangs from, above the storage");
    assert!(body.contains("wep_sidearm · WeaponPersonal"), "a worn item says its port and type");
    assert!(body.contains(r#"action="/admin/accounts/1000002/inventory/unequip""#));
    assert!(body.contains(&format!(r#"name="to" value="{PLAYER_GEID}:PlayerInventory:0""#)), "taken off into the personal inventory by default");
    assert!(!body.contains(r#"name="geid" value="700""#), "the body is the character: no way to take it off");
    assert!(body.contains(r#"name="geid" value="702""#), "the pistol can come off");
    // The ammo box fits the free medpen port, not the taken sidearm port.
    assert!(body.contains(r#"<option value="701:medPen_attach_1">UNDERSUIT › medPen_attach_1</option>"#));
    assert!(!body.contains(r#"<option value="701:wep_sidearm">"#), "a taken port is not offered");
    assert_eq!(body.matches(r#"inventory/equip""#).count(), 1, "one item fits somewhere");
    assert!(body.contains("</select></div><input name=\"reason\""), "the chooser sits above the reason");
    assert!(!body.contains("<h2 id=\"worn\">"), "worn and inventories are one view");
    assert!(!body.contains("<script>"));

    let fields = [("geid", "400000000002"), ("port", "701:medPen_attach_1"), ("reason", "off duty")];
    let reply = site.act(&format!("/admin/accounts/{PLAYER}/inventory/equip"), &fields).await;
    assert!(!reply.flagged(), "{}", notice(&reply));
    assert!(reply.location.ends_with("#character"), "{}", reply.location);
    let calls = site.fake.calls();
    assert!(calls.iter().any(|call| call.starts_with("equip admin:") && call.ends_with(&format!("{PLAYER_GEID} 400000000002 701 medPen_attach_1"))), "{calls:?}");
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!((record.entry.action.as_str(), record.outcome.as_str()), ("inventory.equip", "ok"));

    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let off = [("geid", "702"), ("to", bank.as_str()), ("reason", "stash")];
    let reply = site.act(&format!("/admin/accounts/{PLAYER}/inventory/unequip"), &off).await;
    assert!(!reply.flagged(), "{}", notice(&reply));
    assert!(site.fake.calls().iter().any(|call| call.ends_with(&format!("{PLAYER_GEID} 702 -> {bank}"))));
    // A port name with anything but letters, digits and _ is no port.
    for port in ["701:", "x:wep", "701:a b", "701:../x", ""] {
        let reply = site.act(&format!("/admin/accounts/{PLAYER}/inventory/equip"), &[("geid", "400000000002"), ("port", port), ("reason", "r")]).await;
        assert!(reply.flagged(), "{port}");
    }
    let no_reason = site.act(&format!("/admin/accounts/{PLAYER}/inventory/unequip"), &[("geid", "702"), ("to", bank.as_str()), ("reason", "")]).await;
    assert!(no_reason.flagged());
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
        ("equip", vec![("geid", "400000000002"), ("port", "701:medPen_attach_1"), ("reason", "r")]),
        ("unequip", vec![("geid", "702"), ("to", bank.as_str()), ("reason", "r")]),
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
        assert!(reply.flagged(), "{path} {fields:?}");
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

fn deployed_ship(geid: u64) -> ShipRecord {
    ShipRecord {
        owner_id: PLAYER_GEID,
        entity_class_guid: SHIP_GUID.into(),
        state: ShipState::Deployed as i32,
        pad_size: PadSize::Xsmall as i32,
        location_id: ORISON,
        geid,
        insurance: Some(ShipInsurance { policy: ShipPolicy::Lifetime as i32, ..Default::default() }),
        claim: Some(ShipClaim { ready_at_ms: 1_794_052_800_000, cost_auec: 1234, ..Default::default() }),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_ships_page_has_the_hangar_the_grant_the_entitlements_and_the_rentals() {
    let site = site();
    site.fake.ships.lock().unwrap().push(deployed_ship(500_000_000_100));
    let page = site.get(&format!("/admin/accounts/{PLAYER}/ships"), Some("admin")).await;
    assert_eq!(page.status, StatusCode::OK);
    let body = &page.body;
    assert!(body.contains("Avenger &lt;Titan&gt;") && !body.contains("Avenger <Titan>"), "the English name, escaped");
    assert!(body.contains("deployed") && body.contains("XSmall") && body.contains("lifetime"));
    assert!(body.contains("RR_P6_LEO (Pyro, 2026442305)"), "the place by name");
    assert!(body.contains("ready 2026-11-07 12:00:00, cost 1,234 aUEC"));
    assert!(body.contains("repaired 2 ships") && body.contains("1 orphan "), "{body}");
    assert!(body.contains(&format!(r#"action="/admin/accounts/{PLAYER}/ships/grant""#)) && body.contains(&format!(r#"value="{CSRF}""#)));
    assert!(body.contains(r#"name="location""#) && body.contains(r#"name="insurance""#) && body.contains(r#"name="pad""#) && body.contains("not re-seeded"));
    assert!(body.contains(r#"action="/admin/accounts/1000002/ships/report""#), "a deployed ship can be put away");
    assert!(!body.contains("<script>"), "no inline script");
    assert!(body.contains(r#"<script src="/admin/static/details.js" defer></script>"#));
    // One page: the hangar ships, the grant, the entitlements and the rentals are sections of it.
    for section in ["hangar", "grant", "entitlements", "rentals"] {
        assert!(body.contains(&format!(r#"<details class="section" id="{section}""#)), "{section}");
    }
    assert!(body.contains(r#"id="hangar" open"#), "the hangar ships are open");
    assert!(!body.contains(r#"id="grant" open"#), "the grant form is collapsed until used");
    assert!(body.contains("Ship entitlements") && body.contains("Rentals"));

    // The search asks for vehicles, and a link adds the class to the cart.
    let found = site.get(&format!("/admin/accounts/{PLAYER}/ships?q=aven"), Some("admin")).await;
    assert_eq!(site.fake.calls().last().unwrap(), "classes aven NOITEM_Vehicle   true");
    assert!(found.body.contains(&format!("cart={SHIP_GUID}%3A1#grant")), "the Add link");
    assert!(found.body.contains(r#"id="grant" open"#), "a search opens the grant section");
    let carted = site.get(&format!("/admin/accounts/{PLAYER}/ships?cart={SHIP_GUID}:2,bad%3C%3E"), Some("admin")).await;
    assert!(carted.body.contains("2 x Avenger &lt;Titan&gt;") && !carted.body.contains("bad<>"));
    assert!(carted.body.contains(&format!(r#"name="ships" value="{SHIP_GUID}:2""#)));
    let all = site.get(&format!("/admin/accounts/{PLAYER}/ships?q=aven&all=1"), Some("admin")).await;
    assert!(all.body.contains("checked") && site.fake.calls().last().unwrap() == "classes aven    true");
}

#[tokio::test]
async fn a_grant_acquires_every_ship_with_a_stable_command_id() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/ships/grant");
    let location = ORISON.to_string();
    let ships = format!("{SHIP_GUID}:2");
    let fields = [("ships", ships.as_str()), ("location", location.as_str()), ("insurance", "lifetime"), ("pad", "3"), ("reason", "testing")];
    let reply = site.act(&path, &fields).await;
    assert!(notice(&reply).starts_with("Granted 2 ships at RR_P6_LEO (Pyro, 2026442305): 2x Avenger <Titan>."), "{}", notice(&reply));
    let calls = site.fake.calls();
    let acquires: Vec<&String> = calls.iter().filter(|call| call.starts_with("acquire ")).collect();
    assert_eq!(acquires.len(), 2);
    assert_eq!(acquires[0], &format!("acquire admin:{RID}:0 {PLAYER_GEID} {SHIP_GUID} at {ORISON} policy 1 expires 0 pad 3"));
    assert!(acquires[1].starts_with(&format!("acquire admin:{RID}:1 ")));
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!((record.entry.action.as_str(), record.outcome.as_str()), ("ship.grant", "ok"));
    assert_eq!(record.after.unwrap()["ships"].as_array().unwrap().len(), 2);
    assert_eq!(record.entry.detail["reason"], "testing");

    // The same form again (the same request id) grants nothing new.
    let again = site.act(&path, &fields).await;
    assert!(notice(&again).contains("already") || site.fake.ships.lock().unwrap().len() == 2, "{}", notice(&again));
    assert_eq!(site.fake.ships.lock().unwrap().len(), 2, "a repeat of the command makes no third ship");

    // A duration is days from now; a class with no size and no pad chosen is told.
    let other = site_with(MemoryAudit::default());
    let fields = [("ships", ships.as_str()), ("location", location.as_str()), ("insurance", "duration"), ("days", "10"), ("reason", "r")];
    let reply = other.act(&path, &fields).await;
    assert!(notice(&reply).contains("have no pad size"), "{}", notice(&reply));
    let call = other.fake.calls().remove(1);
    let expires: i64 = call.split("expires ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(call.contains("policy 2") && call.ends_with("pad 0") && expires > 9 * 86_400_000, "{call}");
}

#[tokio::test]
async fn a_bad_grant_is_refused_before_anything_is_asked() {
    let site = site();
    let path = format!("/admin/accounts/{PLAYER}/ships/grant");
    let location = ORISON.to_string();
    let ships = format!("{SHIP_GUID}:1");
    let good = [("ships", ships.as_str()), ("location", location.as_str()), ("insurance", "none"), ("reason", "r")];
    let with = |name: &str, value: &'static str| -> Vec<(&str, &str)> {
        let mut fields: Vec<(&str, &str)> = good.iter().copied().filter(|(key, _)| *key != name).collect();
        fields.push((Box::leak(name.to_string().into_boxed_str()), value));
        fields
    };
    for (fields, expected) in [
        (with("ships", ""), "Add some ships"),
        (with("ships", "not-a-guid!"), "a class GUID"),
        (with("ships", "5a1b0000-0000-0000-0000-000000000001:11"), "from 1 to"),
        (with("reason", ""), "reason"),
        (with("location", "7"), "home hangar"),
        (with("pad", "9"), "pad size"),
        (with("insurance", "duration"), "days"),
        (with("insurance", "forever"), "insurance"),
        (with("ships", "dead0000-0000-0000-0000-000000000000"), "No class"),
    ] {
        let reply = site.act(&path, &fields).await;
        assert!(notice(&reply).contains(expected), "{expected}: {}", notice(&reply));
    }
    assert!(site.fake.calls().iter().all(|call| !call.starts_with("acquire")));
    assert!(site.audit.records.lock().unwrap().is_empty());
    assert!(site.fake.ships.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ships_are_not_granted_or_changed_while_the_player_is_on_a_shard() {
    let site = site();
    site.fake.ships.lock().unwrap().push(deployed_ship(500_000_000_100));
    *site.fake.on_shard.lock().unwrap() = true;
    let page = site.get(&format!("/admin/accounts/{PLAYER}/ships"), Some("admin")).await;
    assert!(page.body.contains("changes are refused") && !page.body.contains("Grant ships") && !page.body.contains("ships/report"));
    assert!(page.body.contains("Avenger &lt;Titan&gt;"), "the list is still shown");
    let location = ORISON.to_string();
    let ships = format!("{SHIP_GUID}:1");
    let grant = site.act(&format!("/admin/accounts/{PLAYER}/ships/grant"), &[("ships", ships.as_str()), ("location", location.as_str()), ("reason", "r")]).await;
    assert!(notice(&grant).contains("on a shard"), "{}", notice(&grant));
    let report = site.act(&format!("/admin/accounts/{PLAYER}/ships/report"), &[("geid", "500000000100"), ("to", "stored"), ("reason", "r")]).await;
    assert!(notice(&report).contains("on a shard"), "{}", notice(&report));
    assert!(site.fake.calls().iter().all(|call| !call.starts_with("acquire") && !call.starts_with("report")));
    assert!(site.audit.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_deployed_ship_left_behind_is_put_away_where_it_belongs() {
    let site = site();
    site.fake.ships.lock().unwrap().push(deployed_ship(500_000_000_100));
    let path = format!("/admin/accounts/{PLAYER}/ships/report");
    let refused = site.act(&path, &[("geid", "500000000100"), ("to", "flying"), ("reason", "r")]).await;
    assert!(notice(&refused).contains("put away or marked destroyed"));
    let missing = site.act(&path, &[("geid", "42"), ("to", "stored"), ("reason", "r")]).await;
    assert!(notice(&missing).contains("no ship 42"), "{}", notice(&missing));
    let done = site.act(&path, &[("geid", "500000000100"), ("to", "stored"), ("reason", "crashed shard")]).await;
    assert!(notice(&done).contains("is stored"), "{}", notice(&done));
    assert!(site.fake.calls().contains(&format!("report admin:{RID} {PLAYER_GEID} 500000000100 {} at {ORISON}", ShipState::Stored as i32)));
    let record = site.audit.list(Some(PLAYER), 1).await.unwrap().remove(0);
    assert_eq!((record.entry.action.as_str(), record.outcome.as_str()), ("ship.store", "ok"));
}

#[tokio::test]
async fn items_are_paged_per_inventory_and_a_redirect_opens_the_inventory_worked_on() {
    let site = site();
    *site.fake.extra_items.lock().unwrap() = 60;
    let bank = format!("{PLAYER_GEID}:Location:2026442305");
    let path = format!("/admin/accounts/{PLAYER}/inventory");
    let first = site.get(&path, Some("admin")).await;
    // 61 items in the bank: 25 on the first page, with the links to the others and to the second page.
    let items_in_bank = |body: &str| body.matches(r#"<div class="item" id="item-41"#).count();
    assert_eq!(items_in_bank(&first.body), 24, "25 per page, the crate being the first and a branch of its own");
    assert!(first.body.contains("Showing 1-25 of 61"));
    assert!(first.body.contains(&format!("pg%3A{}=2", bank.replace(':', "%3A"))), "{}", first.body.matches("pager").count());
    let second = site.get(&format!("{path}?pg%3A{}=3", bank.replace(':', "%3A")), Some("admin")).await;
    assert!(second.body.contains("Showing 51-61 of 61"));
    assert_eq!(items_in_bank(&second.body), 11);
    assert!(second.body.contains("pile059") && !second.body.contains("pile000"));
    assert!(second.body.contains("Previous"), "and back");
    // Every page of the page is small: no row carries a list of the inventories.
    assert!(first.body.len() < 120_000, "{}", first.body.len());

    let script = site.get("/admin/static/details.js", None).await;
    assert!(script.status == StatusCode::OK && script.body.contains("createTextNode") && !script.body.contains("innerHTML"), "built from text nodes");

    // A move or a remove returns to the inventory it touched, open, scrolled to.
    let moved = site
        .act(&format!("{path}/move"), &[("geid", "400000000002"), ("from", "400000000001:Container:0"), ("to", bank.as_str()), ("reason", "r")])
        .await;
    assert!(moved.location.contains(&format!("open={}", query_value_for_test(&format!("{bank},400000000001:Container:0")))), "{}", moved.location);
    assert!(moved.location.ends_with(&format!("#inv-{}", query_value_for_test(&bank))), "{}", moved.location);
    let back = site.get(&moved.location.replace("/admin", "/admin"), Some("admin")).await;
    assert!(back.body.contains(&format!(r#"id="inv-{bank}" open"#)) && back.body.contains(r#"id="inv-400000000001:Container:0" open"#), "the way to what was worked on is open");
    assert!(back.body.contains(r#"id="item-400000000001" open"#), "and the branch of the crate holding it");
}

fn query_value_for_test(text: &str) -> String {
    form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

#[tokio::test]
async fn the_ships_page_asks_the_owners_together_and_names_each_class_once() {
    let site = site();
    {
        let mut ships = site.fake.ships.lock().unwrap();
        for at in 0..4 {
            ships.push(deployed_ship(500_000_000_100 + at));
        }
    }
    let path = format!("/admin/accounts/{PLAYER}/ships");
    let classes = || site.fake.calls().iter().filter(|call| call.starts_with("classes ")).count();
    let first = site.get(&path, Some("admin")).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(classes(), 1, "four ships of one class are named by one lookup, not four");
    assert!(first.body.matches("Avenger &lt;Titan&gt;").count() >= 4);
    site.get(&path, Some("admin")).await;
    assert_eq!(classes(), 1, "a class's name is remembered");

    // Every owner takes 100 ms: asked in turn the page would take over a second.
    *site.fake.latency.lock().unwrap() = std::time::Duration::from_millis(100);
    let other = site_with(MemoryAudit::default());
    *other.fake.latency.lock().unwrap() = std::time::Duration::from_millis(100);
    other.fake.ships.lock().unwrap().push(deployed_ship(500_000_000_100));
    let started = std::time::Instant::now();
    let page = other.get(&path, Some("admin")).await;
    let took = started.elapsed();
    assert_eq!(page.status, StatusCode::OK);
    assert!(took < std::time::Duration::from_millis(450), "the owners are asked together: {took:?}");
}

#[tokio::test]
async fn an_action_notice_is_kept_in_memory_and_shown_once() {
    let site = site();
    let reply = site.act("/admin/accounts/1000002/money", &[("amount", "250"), ("reason", "test")]).await;
    assert!(!reply.location.contains("notice"), "{}", reply.location);
    assert!(reply.cookies.iter().all(|c| !c.contains("flash")), "no cookie carries it");
    assert!(!notice(&reply).is_empty());
    let shown = site.get("/admin/accounts/1000002/money", Some("admin")).await;
    assert!(shown.body.contains("class=\"notice"), "the next read shows it");
    site.app.flashes.lock().unwrap().clear();
    let again = site.get("/admin/accounts/1000002/money", Some("admin")).await;
    assert!(!again.body.contains("class=\"notice"), "and a refresh does not");
}
