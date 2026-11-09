//! Fake owners for tests: one server that answers every seed RPC, as each owner and `sc-entitygraph` would, and keeps
//! what it was sent. Only the seed RPCs answer; the rest are UNIMPLEMENTED. Enabled for this crate's tests and, with
//! the `fakes` feature, for others'.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};

use futures_util::{Stream, StreamExt};
use sc_api::internal::{
    blueprint_library_internal_server::{BlueprintLibraryInternal, BlueprintLibraryInternalServer},
    contract_broker_internal_server::{ContractBrokerInternal, ContractBrokerInternalServer},
    entitlement_internal_server::{EntitlementInternal, EntitlementInternalServer},
    entity_graph_internal_server::{EntityGraphInternal, EntityGraphInternalServer},
    mission_scenario_internal_server::{MissionScenarioInternal, MissionScenarioInternalServer},
    reputation_internal_server::{ReputationInternal, ReputationInternalServer},
    solar_system_shop_internal_server::{SolarSystemShopInternal, SolarSystemShopInternalServer},
    AdminAddItemRequest, AdminAddItemResponse, AdminInventoryTreeRequest, AdminInventoryTreeResponse,
    AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse,
    AdminEquipItemRequest, AdminEquipItemResponse, AdminEquipmentRequest, AdminEquipmentResponse, AdminUnequipItemRequest, AdminUnequipItemResponse, AdminSearchEntityClassesRequest, AdminSearchEntityClassesResponse, BlueprintDefault, ContainerSeed,
    EntitlementHistoryRequest, EntitlementHistoryResponse, EntityClassSeed, GetOwnersRequest, GetOwnersResponse,
    GrantPurchaseRequest, GrantPurchaseResponse, LoadAreaRequest, LoadAreaResponse, MissionContractSeed,
    QueryEntitlementsRequest, QueryEntitlementsResponse, RentalPrice, RevokeEntitlementRequest,
    RevokeEntitlementResponse, RevokeExpiredRentalsRequest, RevokeExpiredRentalsResponse, ScenarioSeed,
    SeedBlueprintDefaultsRequest, SeedContainersRequest, SeedEntityClassesRequest, SeedMissionContractsRequest,
    SeedRentalPricesRequest, SeedReputationRequest, SeedResponse, SeedScenariosRequest, SeedShopItemsRequest,
    SeedStarterEntitlementsRequest, SetRentalExpiryRequest, SetRentalExpiryResponse, ShopItem, StarterEntitlement, UpdateLoadoutRequest, UpdateLoadoutResponse,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};

/// Every owner at once, keeping what each was sent.
#[derive(Clone, Default)]
pub struct Owned {
    /// Per call of SeedShopItems: its items.
    pub shop_items: Arc<Mutex<Vec<Vec<ShopItem>>>>,
    pub prices: Arc<Mutex<Vec<RentalPrice>>>,
    pub starters: Arc<Mutex<Vec<StarterEntitlement>>>,
    pub reputation: Arc<Mutex<Vec<SeedReputationRequest>>>,
    pub blueprints: Arc<Mutex<Vec<BlueprintDefault>>>,
    pub scenarios: Arc<Mutex<Vec<ScenarioSeed>>>,
    pub contracts: Arc<Mutex<Vec<Vec<MissionContractSeed>>>>,
    /// Per call of each seed RPC of the fake `sc-entitygraph`: the rows of every message of its stream.
    pub classes: Arc<Mutex<Vec<Vec<EntityClassSeed>>>>,
    pub containers: Arc<Mutex<Vec<Vec<ContainerSeed>>>>,
}

fn seeded(rows: usize) -> Result<Response<SeedResponse>, Status> {
    Ok(Response::new(SeedResponse { rows: rows as u64 }))
}

#[tonic::async_trait]
impl SolarSystemShopInternal for Owned {
    async fn seed_shop_items(&self, request: Request<SeedShopItemsRequest>) -> Result<Response<SeedResponse>, Status> {
        let items = request.into_inner().items;
        let count = items.len();
        self.shop_items.lock().unwrap().push(items);
        seeded(count)
    }
    async fn seed_rental_prices(&self, request: Request<SeedRentalPricesRequest>) -> Result<Response<SeedResponse>, Status> {
        let prices = request.into_inner().prices;
        let count = prices.len();
        self.prices.lock().unwrap().extend(prices);
        seeded(count)
    }
}

#[tonic::async_trait]
impl EntitlementInternal for Owned {
    type QueryEntitlementsStream = tokio_stream::Empty<Result<QueryEntitlementsResponse, Status>>;

    async fn grant_purchase(&self, _: Request<GrantPurchaseRequest>) -> Result<Response<GrantPurchaseResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn set_rental_expiry(&self, _: Request<SetRentalExpiryRequest>) -> Result<Response<SetRentalExpiryResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn revoke_expired_rentals(
        &self,
        _: Request<RevokeExpiredRentalsRequest>,
    ) -> Result<Response<RevokeExpiredRentalsResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn query_entitlements(
        &self,
        _: Request<QueryEntitlementsRequest>,
    ) -> Result<Response<Self::QueryEntitlementsStream>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn update_loadout(&self, _: Request<UpdateLoadoutRequest>) -> Result<Response<UpdateLoadoutResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn revoke_entitlement(
        &self,
        _: Request<RevokeEntitlementRequest>,
    ) -> Result<Response<RevokeEntitlementResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn entitlement_history(
        &self,
        _: Request<EntitlementHistoryRequest>,
    ) -> Result<Response<EntitlementHistoryResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn seed_starter_entitlements(
        &self,
        request: Request<SeedStarterEntitlementsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = request.into_inner().entitlements;
        let count = rows.len();
        self.starters.lock().unwrap().extend(rows);
        seeded(count)
    }
}

#[tonic::async_trait]
impl ReputationInternal for Owned {
    async fn seed_reputation(&self, request: Request<SeedReputationRequest>) -> Result<Response<SeedResponse>, Status> {
        self.reputation.lock().unwrap().push(request.into_inner());
        seeded(1)
    }
}

#[tonic::async_trait]
impl BlueprintLibraryInternal for Owned {
    async fn seed_blueprint_defaults(
        &self,
        request: Request<SeedBlueprintDefaultsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = request.into_inner().blueprints;
        let count = rows.len();
        self.blueprints.lock().unwrap().extend(rows);
        seeded(count)
    }
}

#[tonic::async_trait]
impl MissionScenarioInternal for Owned {
    async fn seed_scenarios(&self, request: Request<SeedScenariosRequest>) -> Result<Response<SeedResponse>, Status> {
        let rows = request.into_inner().scenarios;
        let count = rows.len();
        self.scenarios.lock().unwrap().extend(rows);
        seeded(count)
    }
}

#[tonic::async_trait]
impl ContractBrokerInternal for Owned {
    async fn seed_mission_contracts(
        &self,
        request: Request<SeedMissionContractsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = request.into_inner().contracts;
        let count = rows.len();
        self.contracts.lock().unwrap().push(rows);
        seeded(count)
    }
}


#[tonic::async_trait]
impl EntityGraphInternal for Owned {
    type LoadAreaStream = Pin<Box<dyn Stream<Item = Result<LoadAreaResponse, Status>> + Send>>;

    async fn seed_entity_classes(&self, request: Request<Streaming<SeedEntityClassesRequest>>) -> Result<Response<SeedResponse>, Status> {
        let messages: Vec<SeedEntityClassesRequest> = request.into_inner().map(|m| m.unwrap()).collect().await;
        let count = messages.iter().map(|m| m.classes.len()).sum();
        self.classes.lock().unwrap().push(messages.into_iter().flat_map(|m| m.classes).collect());
        seeded(count)
    }
    async fn seed_containers(&self, request: Request<Streaming<SeedContainersRequest>>) -> Result<Response<SeedResponse>, Status> {
        let messages: Vec<SeedContainersRequest> = request.into_inner().map(|m| m.unwrap()).collect().await;
        let count = messages.iter().map(|m| m.containers.len()).sum();
        self.containers.lock().unwrap().push(messages.into_iter().flat_map(|m| m.containers).collect());
        seeded(count)
    }
    async fn load_area(&self, _: Request<LoadAreaRequest>) -> Result<Response<Self::LoadAreaStream>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn get_owners(&self, _: Request<GetOwnersRequest>) -> Result<Response<GetOwnersResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn release_player(&self, _: Request<sc_api::internal::ReleasePlayerRequest>) -> Result<Response<sc_api::internal::ReleasePlayerResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn ensure_loadout(&self, _: Request<sc_api::internal::EnsureLoadoutRequest>) -> Result<Response<sc_api::internal::EnsureLoadoutResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn grant_items(&self, _: Request<sc_api::internal::GrantItemsRequest>) -> Result<Response<sc_api::internal::GrantItemsResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn revoke_items(&self, _: Request<sc_api::internal::RevokeItemsRequest>) -> Result<Response<sc_api::internal::RevokeItemsResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_inventory_tree(&self, _: Request<AdminInventoryTreeRequest>) -> Result<Response<AdminInventoryTreeResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_move_item(&self, _: Request<AdminMoveItemRequest>) -> Result<Response<AdminMoveItemResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_add_item(&self, _: Request<AdminAddItemRequest>) -> Result<Response<AdminAddItemResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_remove_item(&self, _: Request<AdminRemoveItemRequest>) -> Result<Response<AdminRemoveItemResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_search_entity_classes(
        &self,
        _: Request<AdminSearchEntityClassesRequest>,
    ) -> Result<Response<AdminSearchEntityClassesResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_equipment(&self, _: Request<AdminEquipmentRequest>) -> Result<Response<AdminEquipmentResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_equip_item(&self, _: Request<AdminEquipItemRequest>) -> Result<Response<AdminEquipItemResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn admin_unequip_item(&self, _: Request<AdminUnequipItemRequest>) -> Result<Response<AdminUnequipItemResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
    async fn inventory_operation(&self, _: Request<sc_api::internal::InventoryOperationRequest>) -> Result<Response<sc_api::internal::InventoryOperationResponse>, Status> {
        Err(Status::unimplemented("not a seed"))
    }
}

impl Owned {
    /// Serves every fake on a local port, and returns them with their address (`http://127.0.0.1:<port>`).
    pub async fn serve() -> (Self, String) {
        let owned = Self::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let limit = 16 << 20;
        let server = Server::builder()
            .add_service(SolarSystemShopInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(EntitlementInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(ReputationInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(BlueprintLibraryInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(MissionScenarioInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(ContractBrokerInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .add_service(EntityGraphInternalServer::new(owned.clone()).max_decoding_message_size(limit))
            .serve_with_incoming(TcpListenerStream::new(listener));
        tokio::spawn(server);
        (owned, address)
    }

    /// Every owner at the one address.
    pub fn owners(address: &str) -> crate::Owners {
        crate::Owners {
            entitlement: address.into(),
            reputation: address.into(),
            blueprint_library: address.into(),
            mission_scenario: address.into(),
            contract_broker: address.into(),
            shop: address.into(),
        }
    }
}
