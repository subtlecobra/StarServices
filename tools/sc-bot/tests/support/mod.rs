//! A stand-in for sc-entitygraph that keeps a player's items in memory, answering the calls the DGS makes for a live
//! player's inventory requests (`services/dgs/src/equip.rs`, `GraphItems`). It owns no rules: every equip fits, every
//! owner is the player. What it proves is the request path from the bot through the replicant to the DGS and back.

use std::{pin::Pin, sync::Mutex};

use sc_api::internal::{
    entity_graph_internal_server::EntityGraphInternal, inventory_operation_request::Operation, AdminAddItemRequest, AdminAddItemResponse, AdminEquipItemRequest,
    AdminEquipItemResponse, AdminEquipmentRequest, AdminEquipmentResponse, AdminInventory, AdminInventoryItem, AdminInventoryTreeRequest, AdminInventoryTreeResponse,
    AdminMoveItemRequest, AdminMoveItemResponse, AdminRemoveItemRequest, AdminRemoveItemResponse, AdminSearchEntityClassesRequest, AdminSearchEntityClassesResponse,
    AdminUnequipItemRequest, AdminUnequipItemResponse, EnsureLoadoutRequest, EnsureLoadoutResponse, EntityOwner, GetOwnersRequest, GetOwnersResponse, GrantItemsRequest,
    GrantItemsResponse, InventoryOperationRequest, InventoryOperationResponse, LoadAreaRequest, LoadAreaResponse, ReleasePlayerRequest, ReleasePlayerResponse,
    RevokeItemsRequest, RevokeItemsResponse, SeedContainersRequest, SeedEntityClassesRequest, SeedResponse,
};

use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Clone, Debug)]
pub struct Item {
    pub geid: u64,
    pub class_crc: u32,
    /// `None` for a worn item.
    pub inventory: Option<String>,
    pub stack: u32,
}

/// The player's items: a helmet, a stack of five and a stack of three, in the item bank, and what it wears once it has
/// joined.
pub struct Graph {
    player: u64,
    bank: String,
    items: Mutex<Vec<Item>>,
    /// The ids of the loadout's items, in its order, once `EnsureLoadout` has made them.
    loadout: Mutex<Vec<u64>>,
    next: Mutex<u64>,
    pub log: Mutex<Vec<String>>,
}

pub const HELMET: u32 = 0x431a_7340;
pub const STACK: u32 = 0x1234_abcd;
pub const WEARABLE_STACK: u32 = 0x2222_aaaa;

impl Graph {
    pub fn new(player: u64, location: u64) -> Self {
        let bank = format!("{player}:Location:{location}");
        let item = |geid, class_crc, stack| Item { geid, class_crc, inventory: Some(bank.clone()), stack };
        Self {
            items: Mutex::new(vec![item(400_000_000_001, HELMET, 1), item(400_000_000_002, STACK, 5), item(400_000_000_003, WEARABLE_STACK, 3)]),
            player,
            bank,
            loadout: Mutex::default(),
            next: Mutex::new(400_000_000_100),
            log: Mutex::default(),
        }
    }

    pub fn snapshot(&self) -> Vec<Item> {
        self.items.lock().unwrap().clone()
    }

    fn note(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }

    fn with_item<T>(&self, geid: u64, change: impl FnOnce(&mut Item) -> T) -> Result<T, Status> {
        let mut items = self.items.lock().unwrap();
        let item = items.iter_mut().find(|item| item.geid == geid).ok_or_else(|| Status::not_found("no such item"))?;
        Ok(change(item))
    }

    fn new_geid(&self) -> u64 {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        *next
    }
}

type Never<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl EntityGraphInternal for Graph {
    type LoadAreaStream = Never<LoadAreaResponse>;

    async fn load_area(&self, _: Request<LoadAreaRequest>) -> Result<Response<Self::LoadAreaStream>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    /// The loadout's items get ids on the first join and keep them; one that was taken off answers 0.
    async fn ensure_loadout(&self, request: Request<EnsureLoadoutRequest>) -> Result<Response<EnsureLoadoutResponse>, Status> {
        let request = request.into_inner();
        let mut loadout = self.loadout.lock().unwrap();
        if loadout.is_empty() {
            for item in &request.items {
                let geid = self.new_geid();
                self.items.lock().unwrap().push(Item { geid, class_crc: item.class_crc, inventory: None, stack: 1 });
                loadout.push(geid);
            }
        }
        let items = self.items.lock().unwrap();
        let geids = loadout.iter().map(|geid| if items.iter().any(|item| item.geid == *geid && item.inventory.is_none()) { *geid } else { 0 }).collect();
        Ok(Response::new(EnsureLoadoutResponse { geids, replayed: false, equipped: Vec::new() }))
    }

    async fn get_owners(&self, request: Request<GetOwnersRequest>) -> Result<Response<GetOwnersResponse>, Status> {
        let items = self.items.lock().unwrap();
        let owners = request.into_inner().entities.into_iter().filter(|entity| items.iter().any(|item| item.geid == *entity)).map(|entity| EntityOwner { entity, owner: self.player }).collect();
        Ok(Response::new(GetOwnersResponse { owners }))
    }

    async fn admin_inventory_tree(&self, request: Request<AdminInventoryTreeRequest>) -> Result<Response<AdminInventoryTreeResponse>, Status> {
        let owner = request.into_inner().owner_id;
        let items = self.items.lock().unwrap();
        let listed: Vec<AdminInventoryItem> = items
            .iter()
            .filter_map(|item| {
                Some(AdminInventoryItem { geid: item.geid, class_guid_crc: item.class_crc, stack_size: item.stack, owner_id: owner, inventory_id: item.inventory.clone()?, ..Default::default() })
            })
            .collect();
        let mut inventories = Vec::new();
        for id in [self.bank.clone(), format!("{owner}:PlayerInventory:0")] {
            let items = listed.iter().filter(|item| item.inventory_id == id).cloned().collect();
            inventories.push(AdminInventory {
                inventory: Some(sc_proto::services::entitygraph::v1::InventoryNodeProperties { id, ..Default::default() }),
                items,
                holder_id: owner,
                ..Default::default()
            });
        }
        Ok(Response::new(AdminInventoryTreeResponse { inventories }))
    }

    async fn admin_move_item(&self, request: Request<AdminMoveItemRequest>) -> Result<Response<AdminMoveItemResponse>, Status> {
        let r = request.into_inner();
        self.note(format!("move {} {} -> {} ({})", r.geid, r.from_inventory_id, r.to_inventory_id, r.quantity));
        self.with_item(r.geid, |item| item.inventory = Some(r.to_inventory_id.clone()))?;
        Ok(Response::new(AdminMoveItemResponse { moved_geid: r.geid, replayed: false }))
    }

    async fn admin_add_item(&self, _: Request<AdminAddItemRequest>) -> Result<Response<AdminAddItemResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn admin_remove_item(&self, _: Request<AdminRemoveItemRequest>) -> Result<Response<AdminRemoveItemResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn admin_search_entity_classes(&self, _: Request<AdminSearchEntityClassesRequest>) -> Result<Response<AdminSearchEntityClassesResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn admin_equipment(&self, _: Request<AdminEquipmentRequest>) -> Result<Response<AdminEquipmentResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn admin_equip_item(&self, request: Request<AdminEquipItemRequest>) -> Result<Response<AdminEquipItemResponse>, Status> {
        let r = request.into_inner();
        self.note(format!("equip {} on {}:{}", r.geid, r.parent_geid, r.port));
        self.with_item(r.geid, |item| item.inventory = None)?;
        Ok(Response::new(AdminEquipItemResponse { replayed: false }))
    }

    async fn admin_unequip_item(&self, request: Request<AdminUnequipItemRequest>) -> Result<Response<AdminUnequipItemResponse>, Status> {
        let r = request.into_inner();
        self.note(format!("unequip {} -> {}", r.geid, r.to_inventory_id));
        let mut items = self.items.lock().unwrap();
        match items.iter_mut().find(|item| item.geid == r.geid) {
            Some(item) => item.inventory = Some(r.to_inventory_id),
            // A made-up id of the default gear, which the graph does not hold: it is now stowed.
            None => items.push(Item { geid: r.geid, class_crc: 0, inventory: Some(r.to_inventory_id), stack: 1 }),
        }
        Ok(Response::new(AdminUnequipItemResponse { replayed: false }))
    }

    async fn inventory_operation(&self, request: Request<InventoryOperationRequest>) -> Result<Response<InventoryOperationResponse>, Status> {
        let r = request.into_inner();
        match r.operation {
            Some(Operation::Split(split)) => {
                let (class_crc, target) = self.with_item(split.geid, |item| {
                    item.stack -= split.amount;
                    (item.class_crc, item.inventory.clone())
                })?;
                let geid = self.new_geid();
                let inventory = if split.to_inventory_id.is_empty() { target } else { Some(split.to_inventory_id) };
                self.items.lock().unwrap().push(Item { geid, class_crc, inventory, stack: split.amount });
                self.note(format!("split {} by {} -> {geid}", split.geid, split.amount));
                Ok(Response::new(InventoryOperationResponse { geid, class_crc, ..Default::default() }))
            }
            Some(Operation::Stack(stack)) => {
                let mut items = self.items.lock().unwrap();
                let from = items.iter().position(|item| item.geid == stack.geid).ok_or_else(|| Status::not_found("no such stack"))?;
                let class_crc = items[from].class_crc;
                let into = items.iter().position(|item| item.geid != stack.geid && item.class_crc == class_crc && item.inventory.is_some()).ok_or_else(|| Status::not_found("no other stack"))?;
                let amount = if stack.amount == 0 { items[from].stack } else { stack.amount };
                items[into].stack += amount;
                items[from].stack -= amount;
                let (geid, erased) = (items[into].geid, items[from].stack == 0);
                if erased {
                    items.remove(from);
                }
                drop(items);
                self.note(format!("stack {} onto {geid}", stack.geid));
                Ok(Response::new(InventoryOperationResponse { geid, class_crc, erased: if erased { vec![stack.geid] } else { vec![] }, ..Default::default() }))
            }
            _ => Err(Status::unimplemented("test graph")),
        }
    }

    async fn release_player(&self, _: Request<ReleasePlayerRequest>) -> Result<Response<ReleasePlayerResponse>, Status> {
        Ok(Response::new(ReleasePlayerResponse::default()))
    }

    async fn revoke_items(&self, _: Request<RevokeItemsRequest>) -> Result<Response<RevokeItemsResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn grant_items(&self, _: Request<GrantItemsRequest>) -> Result<Response<GrantItemsResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn seed_entity_classes(&self, _: Request<Streaming<SeedEntityClassesRequest>>) -> Result<Response<SeedResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }

    async fn seed_containers(&self, _: Request<Streaming<SeedContainersRequest>>) -> Result<Response<SeedResponse>, Status> {
        Err(Status::unimplemented("test graph"))
    }
}
