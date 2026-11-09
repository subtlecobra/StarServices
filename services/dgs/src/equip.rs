//! Applies a live player's inventory requests that change what it wears or where its items are: an equip (type
//! Interaction with a port path), a store (a worn item into an inventory) and a move. The DGS has authority over the
//! player's worn items (`worn`), so it decides here, changes its tree, and has the client shown the change the way the
//! loadout is: the item bound under its new parent in the port's key, or unbound with what hangs from it.
//!
//! What a client asks for is checked against the DGS's own state first (the item is a known stowed item or a worn one of
//! this player, the port path leads to a worn parent, the port is free). The item classes' rules (the port takes the item's
//! type, size and tags) and the items' ownership are sc-entitygraph's, which owns that data: the DGS asks it to make the
//! change (`ItemStore`) before it changes its own state, so what a rejection says (a gadget in a helmet port) fails the
//! request, and what is accepted is what the player has after a rejoin (see "Persisting live equips" in docs/INVENTORY.md).

use std::time::Duration;

use sc_api::internal::{
    entity_graph_internal_client::EntityGraphInternalClient, inventory_operation_request::Operation, AdminEquipItemRequest,
    AdminInventoryTreeRequest, AdminMoveItemRequest, AdminUnequipItemRequest, DropOperation, InventoryOperationRequest,
    InventoryOperationResponse, PickupOperation, RepoolAmmoOperation, SplitOperation, StackAllOperation, StackOperation, SwapOperation,
};
use tonic::{transport::Channel, Code, Status};
use tracing::{info, warn};

use crate::{
    entity::{Bind, Transform},
    inventory::{
        Request, KIND_AMMO_REPOOL, KIND_DROP, KIND_INTERACTION, KIND_MOVE, KIND_SORT, KIND_SPLIT, KIND_STACK, KIND_STACK_ALL, KIND_STORE,
        KIND_STORE_AND_HIDE, KIND_SWAP, KIND_SWAP_OUTFIT,
    },
    loadout::{port_attach_id, ITEM_FLAGS},
    worn::{PathError, Worn, WornItem},
};

/// How long the DGS waits for sc-entitygraph before it fails the request.
const STORE_TIMEOUT: Duration = Duration::from_secs(3);

/// A stowed item of the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stowed {
    pub geid: u64,
    pub class_crc: u32,
}

/// An item now worn that was not where it is (sc-entitygraph's `PlacedItem`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub geid: u64,
    pub class_crc: u32,
    pub parent: u64,
    pub port_crc: u32,
}

/// What sc-entitygraph answers an operation with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Done {
    /// The item the request ends in.
    pub geid: u64,
    pub class_crc: u32,
    pub erased: Vec<u64>,
    pub placed: Vec<Placed>,
}

impl From<InventoryOperationResponse> for Done {
    fn from(reply: InventoryOperationResponse) -> Self {
        Self {
            geid: reply.geid,
            class_crc: reply.class_crc,
            erased: reply.erased,
            placed: reply
                .placed
                .into_iter()
                .map(|item| Placed { geid: item.geid, class_crc: item.class_crc, parent: item.parent_geid, port_crc: item.port_crc })
                .collect(),
        }
    }
}

/// Where the player stands: where a dropped item is put.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    /// The zone entity the position is relative to.
    pub zone: u64,
    pub position: [f64; 3],
    pub rotation: [f64; 4],
}

/// Where the items of a player are kept, and the rules of their classes. Every call is for the player `owner`.
#[allow(async_fn_in_trait)]
pub trait ItemStore {
    /// The player's stowed item of class `class_crc` in `inventory`: a single item rather than a stack, when `single`.
    async fn find_stowed(&self, owner: u64, inventory: &str, class_crc: u32, single: bool) -> Result<Option<Stowed>, Status>;
    /// How many a stowed item of `inventory` stands for (1 for a single item, 0 when it is not there).
    async fn stack_size(&self, owner: u64, inventory: &str, geid: u64) -> Result<u32, Status>;
    /// A stowed item goes into a free port of a worn one. FAILED_PRECONDITION when the class does not fit it.
    async fn equip(&self, command: &str, owner: u64, geid: u64, parent: u64, port: &str) -> Result<(), Status>;
    /// A worn item goes into an inventory, with what hangs from it.
    async fn unequip(&self, command: &str, owner: u64, geid: u64, inventory: &str) -> Result<(), Status>;
    /// A stowed item goes from one inventory to another.
    async fn move_item(&self, command: &str, owner: u64, geid: u64, from: &str, to: &str, quantity: u32) -> Result<(), Status>;
    /// A stack goes onto another stack of its class in `inventory` (empty: its own), `amount` of it (0: all).
    async fn stack(&self, command: &str, owner: u64, geid: u64, inventory: &str, amount: u32) -> Result<Done, Status>;
    /// The stacks of an inventory are merged, of one class or (0) of every class.
    async fn stack_all(&self, command: &str, owner: u64, inventory: &str, class_crc: u32) -> Result<Done, Status>;
    /// A new item takes `amount` from a stack, into `to` (empty: the stack's inventory).
    async fn split(&self, command: &str, owner: u64, geid: u64, amount: u32, to: &str) -> Result<Done, Status>;
    /// Two items trade places.
    async fn swap(&self, command: &str, owner: u64, geid: u64, other: u64) -> Result<Done, Status>;
    /// The rounds of the magazines of a class in an inventory are poured into as few as possible.
    async fn repool_ammo(&self, command: &str, owner: u64, inventory: &str, class_crc: u32) -> Result<Done, Status>;
    /// A stowed item (or `amount` of a stack) is put in the world at `spot`.
    async fn drop_item(&self, command: &str, owner: u64, geid: u64, amount: u32, shard: &str, spot: &Spot) -> Result<Done, Status>;
    /// An item lying in the world of `shard` is stowed in `to` (empty: the personal inventory). NOT_FOUND: it is none.
    async fn pickup(&self, command: &str, owner: u64, geid: u64, shard: &str, to: &str) -> Result<Done, Status>;
}

/// `ItemStore` over sc-entitygraph, `None` without one.
pub struct GraphItems(pub Option<EntityGraphInternalClient<Channel>>);

impl GraphItems {
    fn client(&self) -> Result<EntityGraphInternalClient<Channel>, Status> {
        self.0.clone().ok_or_else(|| Status::unavailable("this DGS has no sc-entitygraph"))
    }
}

async fn timed<T>(call: impl std::future::Future<Output = Result<tonic::Response<T>, Status>>) -> Result<T, Status> {
    match tokio::time::timeout(STORE_TIMEOUT, call).await {
        Ok(reply) => reply.map(tonic::Response::into_inner),
        Err(_) => Err(Status::deadline_exceeded("sc-entitygraph did not answer")),
    }
}

impl ItemStore for GraphItems {
    async fn find_stowed(&self, owner: u64, inventory: &str, class_crc: u32, single: bool) -> Result<Option<Stowed>, Status> {
        let mut graph = self.client()?;
        let tree = timed(graph.admin_inventory_tree(AdminInventoryTreeRequest { owner_id: owner, max_depth: 1, max_items: 2000 })).await?;
        let mut found: Vec<_> = tree
            .inventories
            .iter()
            .filter(|listed| listed.inventory.as_ref().is_some_and(|inventory_node| inventory_node.id == inventory))
            .flat_map(|listed| listed.items.iter())
            .filter(|item| item.class_guid_crc == class_crc && !item.on_shard)
            .collect();
        found.sort_by_key(|item| (single && item.stack_size > 1, item.geid));
        Ok(found.first().map(|item| Stowed { geid: item.geid, class_crc: item.class_guid_crc }))
    }

    async fn stack_size(&self, owner: u64, inventory: &str, geid: u64) -> Result<u32, Status> {
        let mut graph = self.client()?;
        let tree = timed(graph.admin_inventory_tree(AdminInventoryTreeRequest { owner_id: owner, max_depth: 1, max_items: 2000 })).await?;
        Ok(tree
            .inventories
            .iter()
            .filter(|listed| listed.inventory.as_ref().is_some_and(|node| node.id == inventory))
            .flat_map(|listed| listed.items.iter())
            .find(|item| item.geid == geid)
            .map_or(0, |item| item.stack_size.max(1)))
    }

    async fn equip(&self, command: &str, owner: u64, geid: u64, parent: u64, port: &str) -> Result<(), Status> {
        let mut graph = self.client()?;
        let request = AdminEquipItemRequest {
            command_id: command.into(),
            owner_id: owner,
            geid,
            parent_geid: parent,
            port: port.into(),
            actor: "dgs".into(),
            reason: "inventory request of the live player".into(),
        };
        timed(graph.admin_equip_item(request)).await.map(|_| ())
    }

    async fn unequip(&self, command: &str, owner: u64, geid: u64, inventory: &str) -> Result<(), Status> {
        let mut graph = self.client()?;
        let request = AdminUnequipItemRequest {
            command_id: command.into(),
            owner_id: owner,
            geid,
            to_inventory_id: inventory.into(),
            actor: "dgs".into(),
            reason: "inventory request of the live player".into(),
        };
        timed(graph.admin_unequip_item(request)).await.map(|_| ())
    }

    async fn move_item(&self, command: &str, owner: u64, geid: u64, from: &str, to: &str, quantity: u32) -> Result<(), Status> {
        let mut graph = self.client()?;
        let request = AdminMoveItemRequest {
            command_id: command.into(),
            owner_id: owner,
            geid,
            from_inventory_id: from.into(),
            to_inventory_id: to.into(),
            quantity,
            actor: "dgs".into(),
            reason: "inventory request of the live player".into(),
        };
        timed(graph.admin_move_item(request)).await.map(|_| ())
    }

    async fn stack(&self, command: &str, owner: u64, geid: u64, inventory: &str, amount: u32) -> Result<Done, Status> {
        self.operate(command, owner, Operation::Stack(StackOperation { geid, inventory_id: inventory.into(), amount })).await
    }

    async fn stack_all(&self, command: &str, owner: u64, inventory: &str, class_crc: u32) -> Result<Done, Status> {
        self.operate(command, owner, Operation::StackAll(StackAllOperation { inventory_id: inventory.into(), class_crc })).await
    }

    async fn split(&self, command: &str, owner: u64, geid: u64, amount: u32, to: &str) -> Result<Done, Status> {
        self.operate(command, owner, Operation::Split(SplitOperation { geid, amount, to_inventory_id: to.into() })).await
    }

    async fn swap(&self, command: &str, owner: u64, geid: u64, other: u64) -> Result<Done, Status> {
        self.operate(command, owner, Operation::Swap(SwapOperation { geid, other_geid: other })).await
    }

    async fn repool_ammo(&self, command: &str, owner: u64, inventory: &str, class_crc: u32) -> Result<Done, Status> {
        self.operate(command, owner, Operation::RepoolAmmo(RepoolAmmoOperation { inventory_id: inventory.into(), class_crc })).await
    }

    async fn drop_item(&self, command: &str, owner: u64, geid: u64, amount: u32, shard: &str, spot: &Spot) -> Result<Done, Status> {
        let ([x, y, z], [qx, qy, qz, qw]) = (spot.position, spot.rotation);
        let drop = DropOperation { geid, amount, shard_id: shard.into(), zone_id: spot.zone, x, y, z, qx, qy, qz, qw };
        self.operate(command, owner, Operation::Drop(drop)).await
    }

    async fn pickup(&self, command: &str, owner: u64, geid: u64, shard: &str, to: &str) -> Result<Done, Status> {
        self.operate(command, owner, Operation::Pickup(PickupOperation { geid, shard_id: shard.into(), to_inventory_id: to.into() })).await
    }
}

impl GraphItems {
    async fn operate(&self, command: &str, owner: u64, operation: Operation) -> Result<Done, Status> {
        let mut graph = self.client()?;
        let request = InventoryOperationRequest {
            command_id: command.into(),
            owner_id: owner,
            actor: "dgs".into(),
            reason: "inventory request of the live player".into(),
            operation: Some(operation),
        };
        timed(graph.inventory_operation(request)).await.map(Done::from)
    }
}

/// Which request this is, for the command ids sc-entitygraph applies once.
#[derive(Clone, Copy, Debug)]
pub struct Origin<'a> {
    pub replicant: u64,
    pub session: u64,
    /// The shard the DGS serves: the one dropped items lie on.
    pub shard: &'a str,
    /// Where the player is, when known: where a drop puts the item.
    pub spot: Option<Spot>,
}

impl Origin<'_> {
    fn command(&self, request: &Request) -> String {
        format!("live:{}:{}:{}:{}", self.replicant, self.session, request.player, request.request_id)
    }
}

/// What a request came to.
#[derive(Default)]
pub struct Outcome {
    /// The answer's result.
    pub result: bool,
    /// The answer's item: the item equipped, 0 for none.
    pub item: u64,
    /// The player's tree after the request, when it changed.
    pub worn: Option<Worn>,
    /// What the client is shown: binds (parents first) and unbinds (children first).
    pub binds: Vec<Bind>,
    pub unbinds: Vec<u64>,
}

impl Outcome {
    fn failed(reason: &str, request: &Request) -> Self {
        warn!(player = request.player, request_id = request.request_id, kind = request.kind, reason, "inventory request failed");
        Self::default()
    }

    fn done() -> Self {
        Self { result: true, ..Self::default() }
    }
}

fn refused(status: &Status) -> &'static str {
    match status.code() {
        Code::FailedPrecondition => "sc-entitygraph refused it (the class does not fit the port, or the item cannot be moved now)",
        Code::NotFound => "sc-entitygraph does not know the item",
        Code::PermissionDenied => "the item is not the player's",
        Code::Unavailable | Code::DeadlineExceeded => "sc-entitygraph did not answer",
        _ => "sc-entitygraph failed",
    }
}

/// Applies `request`, a request of a player wearing `worn`. Requests that change nothing the DGS holds are done as they
/// were before this existed: answered as completed.
pub async fn apply<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    match request.kind {
        KIND_INTERACTION => equip(store, worn, origin, request).await,
        KIND_STORE => store_item(store, worn, origin, request).await,
        KIND_MOVE | KIND_STORE_AND_HIDE => move_between(store, worn, origin, request).await,
        KIND_STACK => stack(store, origin, request).await,
        KIND_STACK_ALL => stack_all(store, origin, request).await,
        KIND_SPLIT => split(store, origin, request).await,
        KIND_SWAP | KIND_SWAP_OUTFIT => swap(store, worn, origin, request).await,
        KIND_AMMO_REPOOL => repool_ammo(store, origin, request).await,
        KIND_DROP => drop_item(store, worn, origin, request).await,
        KIND_SORT => {
            // The order is the client's display; nothing is kept.
            info!(player = request.player, request_id = request.request_id, "sort confirmed, nothing is kept");
            Outcome::done()
        }
        _ if request.succeeds() => {
            info!(player = request.player, request_id = request.request_id, kind = request.kind, "inventory request completed without a change");
            Outcome::done()
        }
        _ => Outcome::failed("an invalid request", request),
    }
}

async fn equip<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(detail) = request.detail.as_ref().filter(|detail| !detail.port_path.is_empty()) else {
        // An interaction that names no port is not an equip: with an entity, it is a pickup of an item lying in the world.
        return pickup(store, worn, origin, request).await;
    };
    let Some(inventory) = request.source.id() else { return Outcome::failed("an equip from no inventory", request) };
    let owner = request.player;
    let (parent, port) = match worn.resolve(&detail.port_path) {
        Ok(found) => found,
        Err(PathError::TooShort) => return Outcome::failed("the port path is too short", request),
        Err(PathError::NothingThere) => return Outcome::failed("the port path leads through a port with nothing worn in it", request),
    };
    // The body's `inventory_pocket` ports are the client's way to put an item into the personal storage, not to wear it.
    if is_pocket(port) {
        return stow_in_pocket(store, worn, origin, request, &inventory).await;
    }
    let port_crc = port_attach_id(port);
    if let Some(taken) = worn.child_at(parent, port_crc) {
        return Outcome::failed(&format!("the port is taken by {}: take it off first", taken.geid), request);
    }
    let (geid, class_crc) = if detail.item != 0 {
        (detail.item, detail.class_id)
    } else {
        match store.find_stowed(owner, &inventory, detail.class_id, true).await {
            Ok(Some(stowed)) => (stowed.geid, stowed.class_crc),
            Ok(None) => return Outcome::failed(&format!("no stowed item of class {:#010x} in {inventory}", detail.class_id), request),
            Err(status) => return Outcome::failed(&format!("the stowed item could not be looked up: {status}"), request),
        }
    };
    if worn.contains(geid) {
        return Outcome::failed(&format!("item {geid} is worn already"), request);
    }
    // A stack is worn one at a time: one is split off (a new item next to the stack) and that one is equipped, and put back
    // onto the stack when equipping it fails.
    let (mut geid, command) = (geid, origin.command(request));
    let mut split_off = false;
    if store.stack_size(owner, &inventory, geid).await.is_ok_and(|size| size > 1) {
        match store.split(&format!("{command}:split"), owner, geid, 1, &inventory).await {
            Ok(done) if done.geid != 0 => {
                geid = done.geid;
                split_off = true;
            }
            Ok(_) => return Outcome::failed("splitting one off a stack answered no item", request),
            Err(status) => return Outcome::failed(&format!("one could not be split off the stack: {}: {status}", refused(&status)), request),
        }
    }
    let equip_command = if split_off { format!("{command}:equip") } else { command.clone() };
    if let Err(status) = store.equip(&equip_command, owner, geid, parent, port).await {
        if split_off {
            if let Err(back) = store.stack(&format!("{command}:undo"), owner, geid, &inventory, 0).await {
                warn!(player = owner, request_id = request.request_id, item = geid, %back, "the item split off could not be put back on its stack");
            }
        }
        return Outcome::failed(&format!("{}: {status}", refused(&status)), request);
    }
    let item = WornItem { geid, class_crc, parent, port_crc };
    let mut after = worn.clone();
    after.attach(item.clone());
    let bind = Bind {
        entity_id: geid,
        class_crc,
        flags: ITEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: parent,
        parent_attach_id: port_crc,
        components: Vec::new(),
        snapshots: Vec::new(),
    };
    info!(player = owner, request_id = request.request_id, item = geid, class = format_args!("{class_crc:#010x}"), parent, port, "item equipped");
    Outcome { result: true, item: geid, worn: Some(after), binds: vec![bind], unbinds: Vec::new() }
}

/// Whether `port` is one of the body's inventory pockets (`inventory_pocket`, `inventory_pocket_2`).
fn is_pocket(port: &str) -> bool {
    port.to_ascii_lowercase().starts_with("inventory_pocket")
}

/// An interaction with a pocket port of the body: the item goes from the source inventory into the player's personal
/// inventory (a worn item is taken off into it). Nothing is worn, so nothing is bound; the answer's item is the item.
async fn stow_in_pocket<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request, from: &str) -> Outcome {
    let personal = format!("{}:PlayerInventory:0", request.player);
    let geid = match item_of(store, request, Some(from)).await {
        Ok(geid) => geid,
        Err(outcome) => return outcome,
    };
    if worn.contains(geid) {
        let outcome = take_off(store, worn, origin, request, geid, &personal).await;
        return Outcome { item: if outcome.result { geid } else { 0 }, ..outcome };
    }
    if from != personal {
        let amount = request.detail.as_ref().map_or(0, |detail| detail.amount);
        if let Err(status) = store.move_item(&origin.command(request), request.player, geid, from, &personal, amount).await {
            return failed_with(&status, request);
        }
    }
    info!(player = request.player, request_id = request.request_id, item = geid, from, to = %personal, "item put in a pocket");
    Outcome { item: geid, ..Outcome::done() }
}

/// A worn item goes into `inventory`: unbound with what hangs from it.
async fn take_off<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request, geid: u64, inventory: &str) -> Outcome {
    if !worn.contains(geid) {
        return Outcome::failed(&format!("item {geid} is not worn"), request);
    }
    if let Err(status) = store.unequip(&origin.command(request), request.player, geid, inventory).await {
        return Outcome::failed(&format!("{}: {status}", refused(&status)), request);
    }
    let mut after = worn.clone();
    let mut gone = after.detach(geid);
    // Children before parents.
    gone.reverse();
    info!(player = request.player, request_id = request.request_id, item = geid, inventory, unbound = ?gone, "item taken off");
    Outcome { result: true, item: 0, worn: Some(after), binds: Vec::new(), unbinds: gone }
}

async fn store_item<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let (Some(detail), Some(inventory)) = (request.detail.as_ref(), request.target.id()) else {
        return Outcome::failed("a store into no inventory, or one that is not readable", request);
    };
    if detail.item == 0 {
        return Outcome::failed("a store of no entity", request);
    }
    take_off(store, worn, origin, request, detail.item, &inventory).await
}

async fn move_between<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let (Some(detail), Some(to)) = (request.detail.as_ref(), request.target.id()) else {
        return Outcome::failed("a move to no inventory, or one that is not readable", request);
    };
    if detail.item != 0 && worn.contains(detail.item) {
        return take_off(store, worn, origin, request, detail.item, &to).await;
    }
    let Some(from) = request.source.id() else { return Outcome::failed("a move from no inventory", request) };
    let geid = if detail.item != 0 {
        detail.item
    } else {
        match store.find_stowed(request.player, &from, detail.class_id, false).await {
            Ok(Some(stowed)) => stowed.geid,
            Ok(None) => return Outcome::failed(&format!("no stowed item of class {:#010x} in {from}", detail.class_id), request),
            Err(status) => return Outcome::failed(&format!("the stowed item could not be looked up: {status}"), request),
        }
    };
    if let Err(status) = store.move_item(&origin.command(request), request.player, geid, &from, &to, detail.amount).await {
        return Outcome::failed(&format!("{}: {status}", refused(&status)), request);
    }
    info!(player = request.player, request_id = request.request_id, item = geid, from, to, amount = detail.amount, "item moved");
    Outcome { item: geid, ..Outcome::done() }
}

fn failed_with(status: &Status, request: &Request) -> Outcome {
    Outcome::failed(&format!("{}: {status}", refused(status)), request)
}

/// The item a request is about: the entity it names, else the player's stowed item of its class in `inventory`.
async fn item_of<S: ItemStore>(store: &S, request: &Request, inventory: Option<&str>) -> Result<u64, Outcome> {
    let Some(detail) = request.detail.as_ref() else { return Err(Outcome::failed("a request with no readable detail", request)) };
    if detail.item != 0 {
        return Ok(detail.item);
    }
    let Some(inventory) = inventory else { return Err(Outcome::failed("an item named by class in no inventory", request)) };
    match store.find_stowed(request.player, inventory, detail.class_id, false).await {
        Ok(Some(stowed)) => Ok(stowed.geid),
        Ok(None) => Err(Outcome::failed(&format!("no stowed item of class {:#010x} in {inventory}", detail.class_id), request)),
        Err(status) => Err(Outcome::failed(&format!("the stowed item could not be looked up: {status}"), request)),
    }
}

/// Stack: the item goes onto another stack of its class, in the target inventory or else its own.
async fn stack<S: ItemStore>(store: &S, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(detail) = request.detail.as_ref() else { return Outcome::failed("a stack with no readable detail", request) };
    let geid = match item_of(store, request, request.source.id().as_deref()).await {
        Ok(geid) => geid,
        Err(outcome) => return outcome,
    };
    let target = request.target.id().unwrap_or_default();
    match store.stack(&origin.command(request), request.player, geid, &target, detail.amount).await {
        Ok(done) => {
            info!(player = request.player, request_id = request.request_id, item = done.geid, erased = ?done.erased, "stacked");
            Outcome { item: done.geid, ..Outcome::done() }
        }
        Err(status) => failed_with(&status, request),
    }
}

/// StackAll: the stacks of the source inventory (else the target) are merged, of the class named or of every class.
async fn stack_all<S: ItemStore>(store: &S, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(inventory) = request.source.id().or_else(|| request.target.id()) else { return Outcome::failed("a stack all of no inventory", request) };
    let class = request.detail.as_ref().map_or(0, |detail| detail.class_id);
    match store.stack_all(&origin.command(request), request.player, &inventory, class).await {
        Ok(done) => {
            info!(player = request.player, request_id = request.request_id, inventory, erased = ?done.erased, "stacks merged");
            Outcome { item: done.geid, ..Outcome::done() }
        }
        Err(status) => failed_with(&status, request),
    }
}

/// Split: `amount` of a stack becomes a new item, in the target inventory or else the stack's own.
async fn split<S: ItemStore>(store: &S, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(detail) = request.detail.as_ref() else { return Outcome::failed("a split with no readable detail", request) };
    let geid = match item_of(store, request, request.source.id().as_deref()).await {
        Ok(geid) => geid,
        Err(outcome) => return outcome,
    };
    let to = request.target.id().unwrap_or_default();
    match store.split(&origin.command(request), request.player, geid, detail.amount, &to).await {
        Ok(done) => {
            info!(player = request.player, request_id = request.request_id, item = done.geid, from = geid, "stack split");
            Outcome { item: done.geid, ..Outcome::done() }
        }
        Err(status) => failed_with(&status, request),
    }
}

/// Swap and SwapOutfit *(the fields are inferred)*: the item named (by entity, or by class in the source inventory) trades
/// places with the item worn at the port path, else with the entity in `unstowAggregateRootId`. What hangs from an item goes
/// with it: the swapped worn items are unbound with it and bound again where they are now.
async fn swap<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(detail) = request.detail.as_ref() else { return Outcome::failed("a swap with no readable detail", request) };
    let geid = match item_of(store, request, request.source.id().as_deref()).await {
        Ok(geid) => geid,
        Err(outcome) => return outcome,
    };
    let other = if !detail.port_path.is_empty() {
        match worn.resolve(&detail.port_path) {
            Ok((parent, port)) => worn.child_at(parent, port_attach_id(port)).map(|item| item.geid),
            Err(_) => None,
        }
    } else if detail.unstow_root != 0 && detail.unstow_root != request.player {
        Some(detail.unstow_root)
    } else {
        None
    };
    let Some(other) = other else { return Outcome::failed("a swap that names no second item", request) };
    let done = match store.swap(&origin.command(request), request.player, geid, other).await {
        Ok(done) => done,
        Err(status) => return failed_with(&status, request),
    };
    let mut after = worn.clone();
    let mut unbinds = Vec::new();
    for moved in [geid, other] {
        let mut gone = after.detach(moved);
        gone.reverse();
        unbinds.extend(gone);
    }
    let mut binds = Vec::new();
    for item in &done.placed {
        after.attach(WornItem { geid: item.geid, class_crc: item.class_crc, parent: item.parent, port_crc: item.port_crc });
        binds.push(worn_bind(item));
    }
    info!(player = request.player, request_id = request.request_id, geid, other, unbound = ?unbinds, bound = binds.len(), "items swapped");
    Outcome { result: true, item: geid, worn: Some(after), binds, unbinds }
}

fn worn_bind(item: &Placed) -> Bind {
    Bind {
        entity_id: item.geid,
        class_crc: item.class_crc,
        flags: ITEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: item.parent,
        parent_attach_id: item.port_crc,
        components: Vec::new(),
        snapshots: Vec::new(),
    }
}

/// AmmoRepool: the rounds of the magazines of the class in the source inventory (else the target) are poured into as few
/// magazines as possible.
async fn repool_ammo<S: ItemStore>(store: &S, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(inventory) = request.source.id().or_else(|| request.target.id()) else { return Outcome::failed("an ammo repool in no inventory", request) };
    let class = request.detail.as_ref().map_or(0, |detail| detail.class_id);
    match store.repool_ammo(&origin.command(request), request.player, &inventory, class).await {
        Ok(done) => {
            info!(player = request.player, request_id = request.request_id, inventory, item = done.geid, "ammo repooled");
            Outcome { item: done.geid, ..Outcome::done() }
        }
        Err(status) => failed_with(&status, request),
    }
}

/// Drop: a stowed item (or `amount` of a stack) lies in the world where the player stands, and is bound for the client
/// under the player's zone. A worn item is not dropped: it is taken off first.
async fn drop_item<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let Some(detail) = request.detail.as_ref() else { return Outcome::failed("a drop with no readable detail", request) };
    let Some(spot) = origin.spot else { return Outcome::failed("a drop of a player whose position is not known", request) };
    let geid = match item_of(store, request, request.source.id().as_deref()).await {
        Ok(geid) => geid,
        Err(outcome) => return outcome,
    };
    if worn.contains(geid) {
        return Outcome::failed(&format!("item {geid} is worn: take it off before dropping it"), request);
    }
    match store.drop_item(&origin.command(request), request.player, geid, detail.amount, origin.shard, &spot).await {
        Ok(done) => {
            let bind = Bind {
                entity_id: done.geid,
                class_crc: done.class_crc,
                flags: ITEM_FLAGS,
                transform: Transform { position: spot.position, rotation: spot.rotation },
                parent_id: spot.zone,
                parent_attach_id: 0,
                components: Vec::new(),
                snapshots: Vec::new(),
            };
            info!(player = request.player, request_id = request.request_id, item = done.geid, from = geid, zone = spot.zone, "item dropped");
            Outcome { result: true, item: done.geid, worn: None, binds: vec![bind], unbinds: Vec::new() }
        }
        Err(status) => failed_with(&status, request),
    }
}

/// An interaction with an entity and no port: the pickup of an item lying in the world, which is unbound from the client
/// and stowed in the target inventory (else the personal one). An entity that is no such item leaves the request completed
/// without a change, as every interaction was.
async fn pickup<S: ItemStore>(store: &S, worn: &Worn, origin: Origin<'_>, request: &Request) -> Outcome {
    let item = request.detail.as_ref().map_or(0, |detail| detail.item);
    if item == 0 || worn.contains(item) {
        info!(player = request.player, request_id = request.request_id, "interaction without a port path completed without a change");
        return Outcome::done();
    }
    let to = request.target.id().unwrap_or_default();
    match store.pickup(&origin.command(request), request.player, item, origin.shard, &to).await {
        Ok(done) => {
            info!(player = request.player, request_id = request.request_id, item, "item picked up");
            Outcome { result: true, item: done.geid, worn: None, binds: Vec::new(), unbinds: vec![item] }
        }
        Err(status) if status.code() == Code::NotFound => {
            info!(player = request.player, request_id = request.request_id, item, "interaction with an entity that is no dropped item completed without a change");
            Outcome::done()
        }
        Err(status) => failed_with(&status, request),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{
        inventory::{tests::{build, detail, BANK, PLAYER}, InventoryId},
        loadout::{loadout_binds, DEFAULT_LOADOUT},
    };

    const HELMET_CLASS: u32 = 0x431a_7340;
    const NEW_HELMET: u64 = 400_000_000_900;
    const NONE: InventoryId = InventoryId { holder: 0, context: 11, subject: 0 };
    const BANK_ID: &str = "300000100001:Location:1105811199";
    const SPOT: Spot = Spot { zone: 5, position: [1.0, 2.0, 3.0], rotation: [0.0, 0.0, 0.0, 1.0] };
    const ORIGIN: Origin<'static> = Origin { replicant: 1, session: 2, shard: "shard-a", spot: Some(SPOT) };

    /// A store that keeps what it is told, and answers from a table of what is stowed.
    #[derive(Default)]
    struct Fake {
        stowed: Vec<(String, Stowed, u32)>,
        refuse_equip: bool,
        /// What an operation answers with; a missing one is NOT_FOUND.
        done: Option<Done>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn operated(&self, call: String) -> Result<Done, Status> {
            self.calls.lock().unwrap().push(call);
            self.done.clone().ok_or_else(|| Status::not_found("nothing"))
        }
    }

    impl ItemStore for Fake {
        async fn find_stowed(&self, _: u64, inventory: &str, class_crc: u32, single: bool) -> Result<Option<Stowed>, Status> {
            let mut found: Vec<_> = self.stowed.iter().filter(|(id, item, _)| id == inventory && item.class_crc == class_crc).collect();
            found.sort_by_key(|(_, item, stack)| (single && *stack > 1, item.geid));
            Ok(found.first().map(|(_, item, _)| *item))
        }

        async fn stack_size(&self, _: u64, inventory: &str, geid: u64) -> Result<u32, Status> {
            Ok(self.stowed.iter().find(|(id, item, _)| id == inventory && item.geid == geid).map_or(0, |(_, _, stack)| *stack))
        }

        async fn equip(&self, command: &str, _: u64, geid: u64, parent: u64, port: &str) -> Result<(), Status> {
            self.calls.lock().unwrap().push(format!("equip {command} {geid} {parent} {port}"));
            if self.refuse_equip { Err(Status::failed_precondition("does not fit")) } else { Ok(()) }
        }

        async fn unequip(&self, command: &str, _: u64, geid: u64, inventory: &str) -> Result<(), Status> {
            self.calls.lock().unwrap().push(format!("unequip {command} {geid} {inventory}"));
            Ok(())
        }

        async fn move_item(&self, command: &str, _: u64, geid: u64, from: &str, to: &str, quantity: u32) -> Result<(), Status> {
            self.calls.lock().unwrap().push(format!("move {command} {geid} {from} {to} {quantity}"));
            Ok(())
        }

        async fn stack(&self, command: &str, _: u64, geid: u64, inventory: &str, amount: u32) -> Result<Done, Status> {
            self.operated(format!("stack {command} {geid} {inventory} {amount}"))
        }

        async fn stack_all(&self, command: &str, _: u64, inventory: &str, class_crc: u32) -> Result<Done, Status> {
            self.operated(format!("stack_all {command} {inventory} {class_crc}"))
        }

        async fn split(&self, command: &str, _: u64, geid: u64, amount: u32, to: &str) -> Result<Done, Status> {
            self.operated(format!("split {command} {geid} {amount} {to}"))
        }

        async fn swap(&self, command: &str, _: u64, geid: u64, other: u64) -> Result<Done, Status> {
            self.operated(format!("swap {command} {geid} {other}"))
        }

        async fn repool_ammo(&self, command: &str, _: u64, inventory: &str, class_crc: u32) -> Result<Done, Status> {
            self.operated(format!("repool {command} {inventory} {class_crc}"))
        }

        async fn drop_item(&self, command: &str, _: u64, geid: u64, amount: u32, shard: &str, spot: &Spot) -> Result<Done, Status> {
            self.operated(format!("drop {command} {geid} {amount} {shard} {}", spot.zone))
        }

        async fn pickup(&self, command: &str, _: u64, geid: u64, shard: &str, to: &str) -> Result<Done, Status> {
            self.operated(format!("pickup {command} {geid} {shard} {to}"))
        }
    }

    fn fake() -> Fake {
        Fake { stowed: vec![(BANK_ID.into(), Stowed { geid: NEW_HELMET, class_crc: HELMET_CLASS }, 1)], ..Fake::default() }
    }

    fn worn() -> (Worn, Vec<u64>) {
        let binds = loadout_binds(PLAYER, 0, &DEFAULT_LOADOUT);
        let ids = binds.iter().map(|bind| bind.entity_id).collect();
        (Worn::from_binds(PLAYER, &binds), ids)
    }

    fn equip_request(class: u32, item: u64, path: &str) -> Request {
        Request::parse(&build(5, 1, PLAYER, BANK, NONE, &detail(class, item, path)), PLAYER).unwrap()
    }

    fn store_request(item: u64) -> Request {
        Request::parse(&build(4, 7, PLAYER, NONE, BANK, &detail(0, item, "")), PLAYER).unwrap()
    }

    const HELMET_PATH: &str = "Body_ItemPort:Armor_Undersuit:Armor_Helmet";

    async fn run(store: &Fake, worn: &Worn, request: &Request) -> Outcome {
        apply(store, worn, ORIGIN, request).await
    }

    #[tokio::test]
    async fn a_stowed_item_is_equipped_into_a_free_port_and_bound_under_it() {
        let (mut worn, ids) = worn();
        let store = fake();
        // The old helmet is taken off first, as the client asks.
        let off = run(&store, &worn, &store_request(ids[12])).await;
        assert!(off.result);
        worn = off.worn.unwrap();
        let outcome = run(&store, &worn, &equip_request(HELMET_CLASS, 0, HELMET_PATH)).await;
        assert!(outcome.result);
        assert_eq!(outcome.item, NEW_HELMET);
        assert_eq!(outcome.binds.len(), 1);
        let bind = &outcome.binds[0];
        assert_eq!((bind.entity_id, bind.class_crc, bind.parent_id, bind.parent_attach_id), (NEW_HELMET, HELMET_CLASS, ids[11], port_attach_id("Armor_Helmet")));
        assert_eq!(bind.flags, ITEM_FLAGS);
        let after = outcome.worn.unwrap();
        assert_eq!(after.child_at(ids[11], port_attach_id("Armor_Helmet")).map(|item| item.geid), Some(NEW_HELMET));
        assert_eq!(store.calls.lock().unwrap().last().unwrap(), &format!("equip live:1:2:{PLAYER}:5 {NEW_HELMET} {} Armor_Helmet", ids[11]));
    }

    #[tokio::test]
    async fn an_item_the_player_has_not_stowed_there_fails() {
        let (worn, ids) = worn();
        let store = fake();
        let off = run(&store, &worn, &store_request(ids[12])).await.worn.unwrap();
        let unknown = run(&store, &off, &equip_request(0xdead_beef, 0, HELMET_PATH)).await;
        assert!(!unknown.result && unknown.binds.is_empty() && unknown.worn.is_none());
        assert_eq!(unknown.item, 0);
    }

    #[tokio::test]
    async fn a_port_that_is_taken_fails_without_asking_the_store() {
        let (worn, _) = worn();
        let store = fake();
        let outcome = run(&store, &worn, &equip_request(HELMET_CLASS, 0, HELMET_PATH)).await;
        assert!(!outcome.result);
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_class_that_does_not_fit_the_port_fails_and_changes_nothing() {
        let (worn, ids) = worn();
        let store = Fake { refuse_equip: true, ..fake() };
        let off = run(&store, &worn, &store_request(ids[12])).await.worn.unwrap();
        let outcome = run(&store, &off, &equip_request(HELMET_CLASS, 0, HELMET_PATH)).await;
        assert!(!outcome.result && outcome.worn.is_none() && outcome.binds.is_empty());
    }

    #[tokio::test]
    async fn a_pocket_port_stows_the_item_in_the_personal_inventory() {
        let (worn, _) = worn();
        let store = fake();
        let personal = format!("{PLAYER}:PlayerInventory:0");
        let outcome = run(&store, &worn, &equip_request(HELMET_CLASS, NEW_HELMET, "Body_ItemPort:inventory_pocket")).await;
        assert!(outcome.result && outcome.item == NEW_HELMET && outcome.binds.is_empty() && outcome.worn.is_none());
        assert_eq!(store.calls.lock().unwrap()[0], format!("move live:1:2:{PLAYER}:5 {NEW_HELMET} {BANK_ID} {personal} 1"));
        // Named by class only, and the second pocket.
        let by_class = run(&store, &worn, &equip_request(HELMET_CLASS, 0, "Body_ItemPort:inventory_pocket_2")).await;
        assert!(by_class.result && by_class.item == NEW_HELMET);
    }

    #[tokio::test]
    async fn a_worn_item_put_in_a_pocket_is_taken_off_into_the_personal_inventory() {
        let (worn, ids) = worn();
        let store = fake();
        let outcome = run(&store, &worn, &equip_request(0, ids[12], "Body_ItemPort:inventory_pocket")).await;
        assert!(outcome.result && outcome.item == ids[12] && outcome.unbinds.contains(&ids[12]));
        assert!(store.calls.lock().unwrap()[0].starts_with("unequip") && store.calls.lock().unwrap()[0].ends_with("PlayerInventory:0"));
    }

    #[tokio::test]
    async fn a_stack_is_worn_one_at_a_time_through_a_split() {
        let (worn, ids) = worn();
        let split_off = 400_000_000_901;
        let store = Fake {
            stowed: vec![(BANK_ID.into(), Stowed { geid: NEW_HELMET, class_crc: HELMET_CLASS }, 3)],
            done: Some(Done { geid: split_off, class_crc: HELMET_CLASS, ..Done::default() }),
            ..Fake::default()
        };
        let off = run(&store, &worn, &store_request(ids[12])).await.worn.unwrap();
        let outcome = run(&store, &off, &equip_request(HELMET_CLASS, NEW_HELMET, HELMET_PATH)).await;
        assert!(outcome.result);
        assert_eq!(outcome.item, split_off, "the answer is the new item");
        assert_eq!(outcome.binds[0].entity_id, split_off);
        assert_eq!(outcome.worn.unwrap().child_at(ids[11], port_attach_id("Armor_Helmet")).map(|item| item.geid), Some(split_off));
        let calls = store.calls.lock().unwrap();
        assert_eq!(calls[1], format!("split live:1:2:{PLAYER}:5:split {NEW_HELMET} 1 {BANK_ID}"));
        assert_eq!(calls[2], format!("equip live:1:2:{PLAYER}:5:equip {split_off} {} Armor_Helmet", ids[11]));
    }

    #[tokio::test]
    async fn a_split_off_item_that_does_not_fit_goes_back_on_its_stack() {
        let (worn, ids) = worn();
        let split_off = 400_000_000_901;
        let store = Fake {
            stowed: vec![(BANK_ID.into(), Stowed { geid: NEW_HELMET, class_crc: HELMET_CLASS }, 3)],
            done: Some(Done { geid: split_off, class_crc: HELMET_CLASS, ..Done::default() }),
            refuse_equip: true,
            ..Fake::default()
        };
        let off = run(&store, &worn, &store_request(ids[12])).await.worn.unwrap();
        let outcome = run(&store, &off, &equip_request(HELMET_CLASS, NEW_HELMET, HELMET_PATH)).await;
        assert!(!outcome.result && outcome.item == 0);
        assert_eq!(store.calls.lock().unwrap().last().unwrap(), &format!("stack live:1:2:{PLAYER}:5:undo {split_off} {BANK_ID} 0"));
    }

    #[tokio::test]
    async fn a_path_through_an_empty_port_or_to_nowhere_fails() {
        let (worn, _) = worn();
        let store = fake();
        for path in ["Body_ItemPort:Armor_Undersuit:Armor_Torso:backpack", "Armor_Helmet", "Body_ItemPort:nothing:Armor_Helmet"] {
            assert!(!run(&store, &worn, &equip_request(HELMET_CLASS, 0, path)).await.result, "{path}");
        }
    }

    #[tokio::test]
    async fn an_entity_named_by_the_request_is_equipped_without_a_lookup() {
        let (worn, ids) = worn();
        let store = Fake::default();
        let off = run(&store, &worn, &store_request(ids[12])).await.worn.unwrap();
        let outcome = run(&store, &off, &equip_request(HELMET_CLASS, NEW_HELMET, HELMET_PATH)).await;
        assert!(outcome.result);
        assert_eq!(outcome.item, NEW_HELMET);
        let worn_again = run(&store, &outcome.worn.unwrap(), &equip_request(HELMET_CLASS, NEW_HELMET, "Body_ItemPort:Armor_Undersuit:Armor_Torso")).await;
        assert!(!worn_again.result, "an item that is worn cannot be equipped again");
    }

    #[tokio::test]
    async fn a_worn_item_is_taken_off_with_what_hangs_from_it() {
        let (worn, ids) = worn();
        let store = fake();
        let outcome = run(&store, &worn, &store_request(ids[12])).await;
        assert!(outcome.result);
        assert_eq!(outcome.unbinds, vec![ids[14], ids[13], ids[12]], "children before the helmet");
        assert!(!outcome.worn.as_ref().unwrap().contains(ids[13]));
        assert_eq!(store.calls.lock().unwrap().last().unwrap(), &format!("unequip live:1:2:{PLAYER}:4 {} {BANK_ID}", ids[12]));
    }

    #[tokio::test]
    async fn a_store_of_something_not_worn_or_into_no_inventory_fails() {
        let (worn, ids) = worn();
        let store = fake();
        assert!(!run(&store, &worn, &store_request(NEW_HELMET)).await.result);
        assert!(!run(&store, &worn, &store_request(0)).await.result);
        let nowhere = Request::parse(&build(4, 7, PLAYER, NONE, NONE, &detail(0, ids[12], "")), PLAYER).unwrap();
        assert!(!run(&store, &worn, &nowhere).await.result);
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_move_of_a_stowed_item_goes_through_the_store_and_binds_nothing() {
        let (worn, _) = worn();
        let store = fake();
        let to = InventoryId { holder: PLAYER, context: 4, subject: 0 };
        let request = Request::parse(&build(6, 2, PLAYER, BANK, to, &detail(HELMET_CLASS, 0, "")), PLAYER).unwrap();
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result && outcome.binds.is_empty() && outcome.unbinds.is_empty() && outcome.worn.is_none());
        assert_eq!(outcome.item, NEW_HELMET);
        assert_eq!(store.calls.lock().unwrap().last().unwrap(), &format!("move live:1:2:{PLAYER}:6 {NEW_HELMET} {BANK_ID} {PLAYER}:PlayerInventory:0 1"));
    }

    #[tokio::test]
    async fn requests_that_change_nothing_are_completed_as_before() {
        let (worn, _) = worn();
        let store = fake();
        let sort = Request::parse(&build(7, 3, PLAYER, BANK, NONE, &detail(0, 0, "")), PLAYER).unwrap();
        assert!(run(&store, &worn, &sort).await.result);
        let invalid = Request::parse(&build(8, 14, PLAYER, BANK, NONE, &detail(0, 0, "")), PLAYER).unwrap();
        assert!(!run(&store, &worn, &invalid).await.result);
        assert!(store.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unavailable_store_fails_the_request_instead_of_hanging_it() {
        let (worn, ids) = worn();
        let store = GraphItems(None);
        assert!(!apply(&store, &worn, ORIGIN, &store_request(ids[12])).await.result);
        assert!(!apply(&store, &worn, ORIGIN, &equip_request(HELMET_CLASS, 0, HELMET_PATH)).await.result);
    }

    fn personal() -> InventoryId {
        InventoryId { holder: PLAYER, context: 4, subject: 0 }
    }

    fn with_amount(kind: u8, id: u32, source: InventoryId, target: InventoryId, class: u32, item: u64, amount: u32, path: &str) -> Request {
        let mut sent = detail(class, item, path);
        sent.amount = amount;
        Request::parse(&build(id, kind, PLAYER, source, target, &sent), PLAYER).unwrap()
    }

    fn done(geid: u64) -> Done {
        Done { geid, class_crc: HELMET_CLASS, ..Done::default() }
    }

    #[tokio::test]
    async fn a_stack_a_split_and_a_stack_all_are_asked_of_the_store_and_answer_with_its_item() {
        let (worn, _) = worn();
        let store = Fake { done: Some(done(77)), ..fake() };
        let stack = with_amount(5, 11, BANK, personal(), 0, 1234, 3, "");
        let outcome = run(&store, &worn, &stack).await;
        assert!(outcome.result && outcome.item == 77 && outcome.binds.is_empty() && outcome.worn.is_none());
        let split = with_amount(4, 12, BANK, NONE, 0, 1234, 4, "");
        assert_eq!(run(&store, &worn, &split).await.item, 77);
        let all = with_amount(13, 13, BANK, NONE, HELMET_CLASS, 0, 0, "");
        assert!(run(&store, &worn, &all).await.result);
        let calls = store.calls.lock().unwrap().clone();
        assert_eq!(calls[0], format!("stack live:1:2:{PLAYER}:11 1234 {PLAYER}:PlayerInventory:0 3"));
        assert_eq!(calls[1], format!("split live:1:2:{PLAYER}:12 1234 4 "));
        assert_eq!(calls[2], format!("stack_all live:1:2:{PLAYER}:13 {BANK_ID} {HELMET_CLASS}"));
    }

    #[tokio::test]
    async fn a_stack_by_class_finds_the_item_in_the_source_inventory_and_a_refusal_fails_the_request() {
        let (worn, _) = worn();
        let store = Fake { done: Some(done(77)), ..fake() };
        let by_class = with_amount(5, 14, BANK, NONE, HELMET_CLASS, 0, 0, "");
        assert!(run(&store, &worn, &by_class).await.result);
        assert!(store.calls.lock().unwrap()[0].contains(&format!(" {NEW_HELMET} ")));
        let nothing = with_amount(5, 15, BANK, NONE, 0xdead_beef, 0, 0, "");
        assert!(!run(&store, &worn, &nothing).await.result);
        let refusing = fake();
        assert!(!run(&refusing, &worn, &with_amount(13, 16, BANK, NONE, 0, 0, 0, "")).await.result, "the store has no answer");
    }

    #[tokio::test]
    async fn a_swap_with_a_worn_item_unbinds_it_and_binds_the_placed_ones() {
        let (worn, ids) = worn();
        let placed = Placed { geid: NEW_HELMET, class_crc: HELMET_CLASS, parent: ids[11], port_crc: port_attach_id("Armor_Helmet") };
        let store = Fake { done: Some(Done { geid: NEW_HELMET, placed: vec![placed], ..Done::default() }), ..fake() };
        let request = with_amount(6, 17, BANK, NONE, HELMET_CLASS, 0, 1, HELMET_PATH);
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result);
        assert_eq!(outcome.unbinds, vec![ids[14], ids[13], ids[12]], "the old helmet and what hangs from it, children first");
        assert_eq!(outcome.binds.len(), 1);
        assert_eq!((outcome.binds[0].entity_id, outcome.binds[0].parent_id), (NEW_HELMET, ids[11]));
        let after = outcome.worn.unwrap();
        assert!(after.contains(NEW_HELMET) && !after.contains(ids[12]) && !after.contains(ids[13]));
        assert_eq!(store.calls.lock().unwrap()[0], format!("swap live:1:2:{PLAYER}:17 {NEW_HELMET} {}", ids[12]));
        // The outfit swap is the same request.
        let outfit = with_amount(11, 18, BANK, NONE, HELMET_CLASS, 0, 1, HELMET_PATH);
        assert!(run(&store, &worn, &outfit).await.result);
        // A swap with nothing to swap with fails.
        let alone = with_amount(6, 19, BANK, NONE, HELMET_CLASS, 0, 1, "");
        assert!(!run(&store, &worn, &alone).await.result);
    }

    #[tokio::test]
    async fn store_and_hide_takes_a_worn_item_off_like_a_store() {
        let (worn, ids) = worn();
        let store = fake();
        let request = Request::parse(&build(20, 8, PLAYER, NONE, BANK, &detail(0, ids[12], "")), PLAYER).unwrap();
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result);
        assert_eq!(outcome.unbinds, vec![ids[14], ids[13], ids[12]]);
    }

    #[tokio::test]
    async fn a_drop_binds_the_world_item_under_the_players_zone_and_a_worn_item_is_not_dropped() {
        let (worn, ids) = worn();
        let store = Fake { done: Some(done(NEW_HELMET)), ..fake() };
        let request = with_amount(0, 21, BANK, NONE, 0, NEW_HELMET, 1, "");
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result && outcome.item == NEW_HELMET && outcome.unbinds.is_empty());
        let bind = &outcome.binds[0];
        assert_eq!((bind.entity_id, bind.class_crc, bind.parent_id, bind.transform.position), (NEW_HELMET, HELMET_CLASS, 5, [1.0, 2.0, 3.0]));
        assert_eq!(store.calls.lock().unwrap()[0], format!("drop live:1:2:{PLAYER}:21 {NEW_HELMET} 1 shard-a 5"));
        let worn_one = with_amount(0, 22, BANK, NONE, 0, ids[12], 1, "");
        assert!(!run(&store, &worn, &worn_one).await.result);
        let nowhere = Origin { spot: None, ..ORIGIN };
        assert!(!apply(&store, &worn, nowhere, &request).await.result, "a player whose position is not known drops nothing");
    }

    #[tokio::test]
    async fn an_interaction_with_a_dropped_item_picks_it_up_and_other_entities_change_nothing() {
        let (worn, _) = worn();
        let store = Fake { done: Some(done(NEW_HELMET)), ..fake() };
        let request = with_amount(1, 23, NONE, personal(), 0, NEW_HELMET, 1, "");
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result && outcome.unbinds == vec![NEW_HELMET] && outcome.binds.is_empty());
        assert_eq!(store.calls.lock().unwrap()[0], format!("pickup live:1:2:{PLAYER}:23 {NEW_HELMET} shard-a {PLAYER}:PlayerInventory:0"));
        // An entity the graph does not know as a dropped item (a door, a seat) is completed as before.
        let door = Fake::default();
        let outcome = run(&door, &worn, &request).await;
        assert!(outcome.result && outcome.unbinds.is_empty());
        // No entity at all: no call.
        let none = with_amount(1, 24, NONE, NONE, 0, 0, 1, "");
        assert!(run(&door, &worn, &none).await.result);
        assert_eq!(door.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_ammo_repool_is_asked_of_the_store() {
        let (worn, _) = worn();
        let store = Fake { done: Some(done(9)), ..fake() };
        let request = with_amount(12, 25, BANK, NONE, HELMET_CLASS, 0, 0, "");
        let outcome = run(&store, &worn, &request).await;
        assert!(outcome.result && outcome.item == 9);
        assert_eq!(store.calls.lock().unwrap()[0], format!("repool live:1:2:{PLAYER}:25 {BANK_ID} {HELMET_CLASS}"));
    }
}
