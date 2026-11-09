//! The services the admin site asks. It owns no data but its audit: every account, balance, entitlement, rental,
//! document and inventory belongs to another service, reached over gRPC through that service's internal API.

use std::time::Duration;

use futures_util::TryStreamExt;
use sc_economy::command::Push;
use sc_api::internal::{ entitlement_internal_client::EntitlementInternalClient, entity_document_internal_client::EntityDocumentInternalClient, hub_internal_client::HubInternalClient, entity_graph_internal_client::EntityGraphInternalClient, identity_internal_client::IdentityInternalClient, ledger_internal_client::LedgerInternalClient, presence_internal_client::PresenceInternalClient, rental_internal_client::RentalInternalClient, AccountSummary, AcquireShipRequest, AcquireShipResponse, ListShipsRequest, ListShipsResponse, ReportShipRequest, ReportShipResponse, ships_internal_client::ShipsInternalClient, AdminAddItemRequest, AdminAddItemResponse, AdminEntityClass, AdminEquipItemRequest, AdminEquipItemResponse, AdminEquipmentRequest, AdminEquipmentResponse, AdminUnequipItemRequest, AdminUnequipItemResponse, AdminInventory, AdminInventoryTreeRequest, AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse, AdminSearchEntityClassesRequest, CreateRentalRequest, EndRentalRequest, EndRentalResponse, EntitlementHistoryEntry, EntitlementHistoryRequest, ExtendRentalRequest, ExtendRentalResponse, GameBadge, GameBadgesRequest, GetAccountRequest, GetEntityDocumentsRequest, GetPlayerShardRequest, GetPresencesRequest, GrantPurchaseRequest, LedgerAdjustRequest, LedgerAdjustResponse, LedgerBalance, LedgerBalancesRequest, LedgerOperation, LedgerOperationsRequest, ListRentalsRequest, PlayerPresence, PlayerShard, PutEntityDocumentRequest, PutEntityDocumentResponse, QueryEntitlementsRequest, RevokeEntitlementRequest, RevokeEntitlementResponse, SearchAccountsRequest, SearchAccountsResponse, SetAccountStateRequest, SetRentalExpiryRequest, StoredEntityDocument, VerifyWebSessionRequest, VerifyWebSessionResponse, };
use sc_proto::{
    services::{
    entitlement::v2::{Entitlement, EntitlementFilter},
    ledger::v1::{varied_funds::Variation, Currency, Funds, VariedFunds},
    rental::v1::Rental,
    },
};
use sc_runtime::MAX_MESSAGE_BYTES;
use tonic::{
    transport::{Channel, Endpoint},
    Status,
};

/// A slow owner makes the page fail rather than hang.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A search of the entity classes: a text, and the item types and sub types the classes may have (none: any).
#[derive(Clone, Debug, Default)]
pub struct ClassSearch {
    pub query: String,
    /// (item type, sub types) pairs a class must match one of, a type with no sub types taking any.
    pub any_of: Vec<(String, Vec<String>)>,
    /// Pairs a class must match none of.
    pub none_of: Vec<(String, Vec<String>)>,
    /// Sub types a class must have, whatever its type.
    pub item_sub_types: Vec<String>,
    pub named_only: bool,
    pub limit: u32,
}

fn filters(pairs: Vec<(String, Vec<String>)>) -> Vec<sc_api::internal::ItemTypeFilter> {
    pairs.into_iter().map(|(item_type, item_sub_types)| sc_api::internal::ItemTypeFilter { item_type, item_sub_types }).collect()
}

const VARIED_FUNDS: &str = "sc.external.services.ledger.v1.VariedFunds";
/// The most items listed per inventory on the inventory page.
pub const ITEMS_PER_INVENTORY: u32 = 500;

/// sc-identity: web sessions and accounts. An account's player (GEID) is `AccountSummary.geid`.
#[tonic::async_trait]
pub trait Identity: Send + Sync {
    async fn verify_session(&self, token: &str) -> Result<VerifyWebSessionResponse, Status>;
    async fn search_accounts(&self, request: SearchAccountsRequest) -> Result<SearchAccountsResponse, Status>;
    async fn account(&self, account_id: u32) -> Result<Option<AccountSummary>, Status>;
    async fn set_account_state(&self, request: SetAccountStateRequest) -> Result<AccountSummary, Status>;
    /// The in-game badges an account can be given.
    async fn game_badges(&self) -> Result<Vec<GameBadge>, Status>;
}

/// Every other owner.
#[tonic::async_trait]
pub trait Owners: Send + Sync {
    // sc-ledger
    async fn balances(&self, geid: u64) -> Result<Vec<LedgerBalance>, Status>;
    async fn adjust(&self, request: LedgerAdjustRequest) -> Result<LedgerAdjustResponse, Status>;
    async fn operations(&self, geid: u64, before_ms: i64) -> Result<Vec<LedgerOperation>, Status>;
    // sc-entitlement
    async fn entitlements(&self, account_id: u32, geid: u64, filter: EntitlementFilter) -> Result<Vec<Entitlement>, Status>;
    async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status>;
    async fn revoke(&self, request: RevokeEntitlementRequest) -> Result<RevokeEntitlementResponse, Status>;
    async fn entitlement_history(&self, geid: u64) -> Result<Vec<EntitlementHistoryEntry>, Status>;
    async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status>;
    // sc-rental
    async fn rentals(&self, geid: u64) -> Result<Vec<Rental>, Status>;
    async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status>;
    async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status>;
    async fn end_rental(&self, request: EndRentalRequest) -> Result<EndRentalResponse, Status>;
    // sc-entity-document
    async fn documents(&self, entity_id: u64, classes: Vec<String>) -> Result<Vec<StoredEntityDocument>, Status>;
    async fn put_document(&self, request: PutEntityDocumentRequest) -> Result<PutEntityDocumentResponse, Status>;
    // sc-hub and sc-presence: whether the player is playing.
    async fn player_shard(&self, geid: u64) -> Result<Option<PlayerShard>, Status>;
    async fn presence(&self, account_id: u32) -> Result<Option<PlayerPresence>, Status>;
    // sc-entitygraph, `EntityGraphInternal`'s admin calls: it acts for the player and checks as for the player.
    /// Every inventory of the player, parents before children, with at most `ITEMS_PER_INVENTORY` items each.
    async fn inventory_tree(&self, geid: u64) -> Result<Vec<AdminInventory>, Status>;
    async fn move_item(&self, request: AdminMoveItemRequest) -> Result<AdminMoveItemResponse, Status>;
    async fn add_item(&self, request: AdminAddItemRequest) -> Result<AdminAddItemResponse, Status>;
    async fn remove_item(&self, request: AdminRemoveItemRequest) -> Result<AdminRemoveItemResponse, Status>;
    /// What the player wears and the ports items can go to, each with the stowed items that fit.
    async fn equipment(&self, geid: u64) -> Result<AdminEquipmentResponse, Status>;
    async fn equip_item(&self, request: AdminEquipItemRequest) -> Result<AdminEquipItemResponse, Status>;
    async fn unequip_item(&self, request: AdminUnequipItemRequest) -> Result<AdminUnequipItemResponse, Status>;
    async fn search_classes(&self, search: ClassSearch) -> Result<Vec<AdminEntityClass>, Status>;
    // sc-entitygraph, `ShipsInternal`: the ships a player keeps in hangars.
    /// Every ship of the player, at every location.
    async fn list_ships(&self, geid: u64) -> Result<ListShipsResponse, Status>;
    async fn acquire_ship(&self, request: AcquireShipRequest) -> Result<AcquireShipResponse, Status>;
    async fn report_ship(&self, request: ReportShipRequest) -> Result<ReportShipResponse, Status>;
    /// Tells a playing client its balance changed, through sc-push. Best effort.
    async fn funds_changed(&self, account_id: u32, geid: u64, ledger_name: &str, delta: i64, version: u64);
}

pub struct Addresses {
    pub identity: String,
    pub ledger: String,
    pub entitlement: String,
    pub rental: String,
    pub entity_document: String,
    pub hub: String,
    pub presence: String,
    pub entitygraph: String,
    pub push: String,
}

fn channel(name: &str, address: &str) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("--{name}: {e}"))?;
    Ok(endpoint.timeout(CALL_TIMEOUT).connect_timeout(CALL_TIMEOUT).connect_lazy())
}

pub struct Grpc {
    identity: IdentityInternalClient<Channel>,
    ledger: LedgerInternalClient<Channel>,
    entitlement: EntitlementInternalClient<Channel>,
    rental: RentalInternalClient<Channel>,
    documents: EntityDocumentInternalClient<Channel>,
    hub: HubInternalClient<Channel>,
    presence: PresenceInternalClient<Channel>,
    graph: EntityGraphInternalClient<Channel>,
    ships: ShipsInternalClient<Channel>,
    push: Push,
}

impl Grpc {
    pub fn connect(addresses: &Addresses) -> Result<Self, String> {
        Ok(Self {
            identity: IdentityInternalClient::new(channel("identity", &addresses.identity)?),
            ledger: LedgerInternalClient::new(channel("ledger", &addresses.ledger)?),
            entitlement: EntitlementInternalClient::new(channel("entitlement", &addresses.entitlement)?)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            rental: RentalInternalClient::new(channel("rental", &addresses.rental)?),
            documents: EntityDocumentInternalClient::new(channel("entity-document", &addresses.entity_document)?)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            hub: HubInternalClient::new(channel("hub", &addresses.hub)?),
            presence: PresenceInternalClient::new(channel("presence", &addresses.presence)?),
            graph: EntityGraphInternalClient::new(channel("entitygraph", &addresses.entitygraph)?)
                .max_decoding_message_size(MAX_MESSAGE_BYTES),
            ships: ShipsInternalClient::new(channel("entitygraph", &addresses.entitygraph)?).max_decoding_message_size(MAX_MESSAGE_BYTES),
            push: Push::connect(&addresses.push)?,
        })
    }
}

#[tonic::async_trait]
impl Identity for Grpc {
    async fn verify_session(&self, token: &str) -> Result<VerifyWebSessionResponse, Status> {
        let request = VerifyWebSessionRequest { token: token.to_string() };
        Ok(self.identity.clone().verify_web_session(request).await?.into_inner())
    }

    async fn search_accounts(&self, request: SearchAccountsRequest) -> Result<SearchAccountsResponse, Status> {
        Ok(self.identity.clone().search_accounts(request).await?.into_inner())
    }

    async fn account(&self, account_id: u32) -> Result<Option<AccountSummary>, Status> {
        match self.identity.clone().get_account(GetAccountRequest { account_id }).await {
            Ok(response) => Ok(response.into_inner().account),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(status) => Err(status),
        }
    }

    async fn set_account_state(&self, request: SetAccountStateRequest) -> Result<AccountSummary, Status> {
        let response = self.identity.clone().set_account_state(request).await?.into_inner();
        response.account.ok_or_else(|| Status::internal("sc-identity returned no account"))
    }

    async fn game_badges(&self) -> Result<Vec<GameBadge>, Status> {
        Ok(self.identity.clone().game_badges(GameBadgesRequest {}).await?.into_inner().badges)
    }
}

#[tonic::async_trait]
impl Owners for Grpc {
    async fn balances(&self, geid: u64) -> Result<Vec<LedgerBalance>, Status> {
        let request = LedgerBalancesRequest { owner_geid: geid };
        Ok(self.ledger.clone().balances(request).await?.into_inner().balances)
    }

    async fn adjust(&self, request: LedgerAdjustRequest) -> Result<LedgerAdjustResponse, Status> {
        Ok(self.ledger.clone().adjust(request).await?.into_inner())
    }

    async fn operations(&self, geid: u64, before_ms: i64) -> Result<Vec<LedgerOperation>, Status> {
        let request = LedgerOperationsRequest { owner_geid: geid, limit: 100, before_ms };
        Ok(self.ledger.clone().operations(request).await?.into_inner().operations)
    }

    async fn entitlements(&self, account_id: u32, geid: u64, filter: EntitlementFilter) -> Result<Vec<Entitlement>, Status> {
        let request = QueryEntitlementsRequest {
            account_id,
            player_id: geid,
            filter: Some(filter),
            include_persistent_properties: false,
        };
        let stream = self.entitlement.clone().query_entitlements(request).await?.into_inner();
        let batches: Vec<_> = stream.try_collect().await?;
        Ok(batches.into_iter().flat_map(|batch| batch.entitlements).collect())
    }

    async fn grant(&self, request: GrantPurchaseRequest) -> Result<String, Status> {
        Ok(self.entitlement.clone().grant_purchase(request).await?.into_inner().urn)
    }

    async fn revoke(&self, request: RevokeEntitlementRequest) -> Result<RevokeEntitlementResponse, Status> {
        Ok(self.entitlement.clone().revoke_entitlement(request).await?.into_inner())
    }

    async fn entitlement_history(&self, geid: u64) -> Result<Vec<EntitlementHistoryEntry>, Status> {
        let request = EntitlementHistoryRequest { player_id: geid, urn: String::new(), limit: 100 };
        Ok(self.entitlement.clone().entitlement_history(request).await?.into_inner().entries)
    }

    async fn set_rental_expiry(&self, request: SetRentalExpiryRequest) -> Result<(), Status> {
        self.entitlement.clone().set_rental_expiry(request).await?;
        Ok(())
    }

    async fn rentals(&self, geid: u64) -> Result<Vec<Rental>, Status> {
        let request = ListRentalsRequest { player_id: geid, include_finished: true };
        Ok(self.rental.clone().list_rentals(request).await?.into_inner().rentals)
    }

    async fn create_rental(&self, request: CreateRentalRequest) -> Result<(), Status> {
        self.rental.clone().create_rental(request).await?;
        Ok(())
    }

    async fn extend_rental(&self, request: ExtendRentalRequest) -> Result<ExtendRentalResponse, Status> {
        Ok(self.rental.clone().extend_rental(request).await?.into_inner())
    }

    async fn end_rental(&self, request: EndRentalRequest) -> Result<EndRentalResponse, Status> {
        Ok(self.rental.clone().end_rental(request).await?.into_inner())
    }

    async fn documents(&self, entity_id: u64, classes: Vec<String>) -> Result<Vec<StoredEntityDocument>, Status> {
        let request = GetEntityDocumentsRequest { entity_id, classes };
        Ok(self.documents.clone().get_documents(request).await?.into_inner().documents)
    }

    async fn put_document(&self, request: PutEntityDocumentRequest) -> Result<PutEntityDocumentResponse, Status> {
        Ok(self.documents.clone().put_document(request).await?.into_inner())
    }

    async fn player_shard(&self, geid: u64) -> Result<Option<PlayerShard>, Status> {
        Ok(self.hub.clone().get_player_shard(GetPlayerShardRequest { geid }).await?.into_inner().shard)
    }

    async fn presence(&self, account_id: u32) -> Result<Option<PlayerPresence>, Status> {
        let request = GetPresencesRequest { account_ids: vec![account_id] };
        let found = self.presence.clone().get_presences(request).await?.into_inner().presences;
        Ok(found.into_iter().find(|presence| presence.account_id == account_id))
    }

    async fn inventory_tree(&self, geid: u64) -> Result<Vec<AdminInventory>, Status> {
        let request = AdminInventoryTreeRequest { owner_id: geid, max_depth: 0, max_items: ITEMS_PER_INVENTORY };
        Ok(self.graph.clone().admin_inventory_tree(request).await?.into_inner().inventories)
    }

    async fn move_item(&self, request: AdminMoveItemRequest) -> Result<AdminMoveItemResponse, Status> {
        Ok(self.graph.clone().admin_move_item(request).await?.into_inner())
    }

    async fn add_item(&self, request: AdminAddItemRequest) -> Result<AdminAddItemResponse, Status> {
        Ok(self.graph.clone().admin_add_item(request).await?.into_inner())
    }

    async fn remove_item(&self, request: AdminRemoveItemRequest) -> Result<AdminRemoveItemResponse, Status> {
        Ok(self.graph.clone().admin_remove_item(request).await?.into_inner())
    }

    async fn equipment(&self, geid: u64) -> Result<AdminEquipmentResponse, Status> {
        Ok(self.graph.clone().admin_equipment(AdminEquipmentRequest { owner_id: geid }).await?.into_inner())
    }

    async fn equip_item(&self, request: AdminEquipItemRequest) -> Result<AdminEquipItemResponse, Status> {
        Ok(self.graph.clone().admin_equip_item(request).await?.into_inner())
    }

    async fn unequip_item(&self, request: AdminUnequipItemRequest) -> Result<AdminUnequipItemResponse, Status> {
        Ok(self.graph.clone().admin_unequip_item(request).await?.into_inner())
    }

    async fn search_classes(&self, search: ClassSearch) -> Result<Vec<AdminEntityClass>, Status> {
        let request = AdminSearchEntityClassesRequest {
            query: search.query,
            limit: search.limit,
            any_of: filters(search.any_of),
            none_of: filters(search.none_of),
            item_sub_types: search.item_sub_types,
            named_only: search.named_only,
        };
        Ok(self.graph.clone().admin_search_entity_classes(request).await?.into_inner().classes)
    }

    async fn list_ships(&self, geid: u64) -> Result<ListShipsResponse, Status> {
        let request = ListShipsRequest { owner_id: geid, location_id: 0, hangar_size: 0, exact_size: false, now_ms: 0 };
        Ok(self.ships.clone().list_ships(request).await?.into_inner())
    }

    async fn acquire_ship(&self, request: AcquireShipRequest) -> Result<AcquireShipResponse, Status> {
        Ok(self.ships.clone().acquire_ship(request).await?.into_inner())
    }

    async fn report_ship(&self, request: ReportShipRequest) -> Result<ReportShipResponse, Status> {
        Ok(self.ships.clone().report_ship(request).await?.into_inner())
    }

    /// As sc-eatransaction tells a buyer: topics under `player/<geid>/` reach the ledger subscription.
    async fn funds_changed(&self, account_id: u32, geid: u64, ledger_name: &str, delta: i64, version: u64) {
        let currency = if ledger_name == sc_economy::ledger::AUEC_LEDGER { Currency::Auec } else { Currency::Rec };
        let funds = VariedFunds {
            funds: Some(Funds { amount: delta.unsigned_abs(), currency: currency as i32 }),
            ledger_name: ledger_name.to_string(),
            log_message: "AdminAdjustment".to_string(),
            variation: if delta > 0 { Variation::Deposited } else { Variation::Withdrew } as i32,
            version: version as u32,
        };
        self.push.notify(account_id, format!("player/{geid}/ledger"), VARIED_FUNDS, &funds).await;
    }
}
