//! `InventoryOperation` of `EntityGraphInternal`: what a live player's client asks of its DGS besides equipping, taking off
//! and moving (see "Live inventory operations" in docs/INVENTORY.md). Each operation is one plan in one transaction, for
//! the player `owner_id`, applied once per command id with its answer recorded, so the checks of every other command
//! (ownership, capacity, loops) apply unchanged. A stack that changed between the read and the write fails the plan
//! (`FAILED_PRECONDITION`) instead of being overwritten.

use std::collections::BTreeMap;

use sc_api::internal::{
    inventory_operation_request::Operation, DropOperation, InventoryOperationRequest, InventoryOperationResponse, PickupOperation,
    PlacedItem, RepoolAmmoOperation, SplitOperation, StackAllOperation, StackOperation, SwapOperation,
};
use sc_db::{internal, Arango};
use sc_proto::{
    common::types::v1::{Quatd, Vec3d},
    services::entitygraph::v1::{
        entity_graph_command::Params, erase_command, move_command, set_stack_command, EraseCommand, MoveCommand, MutationArguments,
        Scope, ScopeType, SetStackCommand,
    },
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    admin::{allocate_geid, apply_once, command, create_into, entity, entity_properties, invalid, need_command, need_inventory, need_owner, recorded, refuse_taken, need_target},
    commands::Plan,
    doc,
    equip::{class_of, equippable, fits, port_crc, removable, text, worn, Worn},
    now_ms,
};

/// Stacks read from one inventory.
const MAX_ROWS: u32 = 5_000;
/// What the game lets stack (`PersonalThoughtInventoryParams`): 99, and 999 for currency; every other item is single.
const STACK_LIMIT: u32 = 99;
const CURRENCY_STACK_LIMIT: u32 = 999;
/// Levels of items attached to one another below an item (weapon, magazine, optic ...).
const ATTACHED_DEPTH: u32 = 8;

/// How many of a class one stack holds, 1 for one that does not stack.
pub fn stack_limit(class: &Value) -> u32 {
    if text(class, "itemType") == "Currency" {
        CURRENCY_STACK_LIMIT
    } else if class["stackable"].as_bool() == Some(true) {
        STACK_LIMIT
    } else {
        1
    }
}

fn stack_of(row: &Value) -> u32 {
    row["stackSize"].as_u64().unwrap_or(0).max(1) as u32
}

fn geid_of(row: &Value) -> u64 {
    doc::big(row, "geid")
}

fn class_crc_of(row: &Value) -> u32 {
    row["classGuidCrc"].as_u64().unwrap_or(0) as u32
}

/// An item now worn that was not where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub geid: u64,
    pub class_crc: u32,
    pub parent: u64,
    pub port_crc: u32,
}

/// What an operation came to: the answer, and what the record of it keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Done {
    pub geid: u64,
    pub class_crc: u32,
    pub erased: Vec<u64>,
    pub placed: Vec<Placed>,
}

impl Done {
    fn json(&self) -> Value {
        json!({
            "geid": self.geid.to_string(),
            "classCrc": self.class_crc,
            "erased": self.erased.iter().map(u64::to_string).collect::<Vec<_>>(),
            "placed": self.placed.iter().map(|item| json!({
                "geid": item.geid.to_string(), "classCrc": item.class_crc, "parent": item.parent.to_string(), "port": item.port_crc,
            })).collect::<Vec<_>>(),
        })
    }

    fn read(found: &Value) -> Self {
        Self {
            geid: doc::big(found, "geid"),
            class_crc: found["classCrc"].as_u64().unwrap_or(0) as u32,
            erased: found["erased"].as_array().into_iter().flatten().filter_map(|geid| geid.as_str()?.parse().ok()).collect(),
            placed: found["placed"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| Placed {
                    geid: doc::big(item, "geid"),
                    class_crc: item["classCrc"].as_u64().unwrap_or(0) as u32,
                    parent: doc::big(item, "parent"),
                    port_crc: item["port"].as_u64().unwrap_or(0) as u32,
                })
                .collect(),
        }
    }

    fn response(self, replayed: bool) -> InventoryOperationResponse {
        InventoryOperationResponse {
            geid: self.geid,
            class_crc: self.class_crc,
            replayed,
            erased: self.erased,
            placed: self
                .placed
                .into_iter()
                .map(|item| PlacedItem { geid: item.geid, class_crc: item.class_crc, parent_geid: item.parent, port_crc: item.port_crc })
                .collect(),
        }
    }
}

pub async fn run(db: &Arango, request: InventoryOperationRequest) -> Result<InventoryOperationResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    if let Some(found) = recorded(db, &request.command_id).await? {
        return Ok(Done::read(&found).response(true));
    }
    let owner = request.owner_id;
    let (kind, (plan, done)) = match request.operation {
        Some(Operation::Stack(op)) => ("stack", stack(db, owner, op).await?),
        Some(Operation::StackAll(op)) => ("stack all", stack_all(db, owner, op).await?),
        Some(Operation::Split(op)) => ("split", split(db, owner, op).await?),
        Some(Operation::Swap(op)) => ("swap", swap(db, owner, op).await?),
        Some(Operation::RepoolAmmo(op)) => ("ammo repool", repool_ammo(db, owner, op).await?),
        Some(Operation::Drop(op)) => ("drop", drop_item(db, owner, op).await?),
        Some(Operation::Pickup(op)) => ("pickup", pickup(db, owner, op).await?),
        None => return Err(invalid("an operation is needed")),
    };
    let (result, replayed) = apply_once(db, &request.command_id, owner, plan, done.json()).await?;
    let done = Done::read(&result);
    info!(actor = %request.actor, reason = %request.reason, owner, kind, geid = done.geid, erased = ?done.erased, placed = done.placed.len(), replayed, "inventory operation");
    Ok(done.response(replayed))
}

/// The item `geid`, which the player owns and has stowed.
async fn owned_stowed(db: &Arango, owner: u64, geid: u64) -> Result<Value, Status> {
    if geid == 0 {
        return Err(invalid("an item is needed"));
    }
    let row = entity(db, geid).await?;
    if row["ownerId"].as_str() != Some(owner.to_string().as_str()) {
        return Err(Status::permission_denied("the item belongs to someone else"));
    }
    if row["stowed"].as_bool() != Some(true) {
        return Err(Status::failed_precondition("the item is not stowed in an inventory"));
    }
    Ok(row)
}

/// The player's items stowed in `inventory`, by GEID.
async fn items_in(db: &Arango, owner: u64, inventory: &str) -> Result<Vec<Value>, Status> {
    let query = "FOR e IN entities FILTER e.stowed == true AND e.inventoryId == @inventory AND e.ownerId == @owner
                     SORT LENGTH(e._key), e._key LIMIT @limit RETURN e";
    db.aql(query, json!({ "inventory": inventory, "owner": owner.to_string(), "limit": MAX_ROWS })).await.map_err(internal)
}

/// Stops the transaction when one of `rows` is not stowed with the stack it had when it was read.
fn guard_stacks(plan: &mut Plan, rows: &[&Value]) {
    let wanted: Vec<Value> = rows.iter().map(|row| json!({ "key": geid_of(row).to_string(), "stack": row["stackSize"] })).collect();
    plan.check(
        "RETURN ASSERT(LENGTH(FOR t IN @rows FOR e IN entities FILTER e._key == t.key AND e.stowed == true AND e.stackSize == t.stack
                              RETURN 1) == LENGTH(@rows), 'precondition: a stack changed or is not stowed any more')",
        json!({ "rows": wanted }),
    );
}

fn set_stack(plan: &mut Plan, geid: u64, amount: u32) -> Result<(), Status> {
    let change = set_stack_command::Body { entity_id: geid, amount: amount.min(i32::MAX as u32) as i32 };
    plan.command(&command(Params::Setstack(SetStackCommand { changes: vec![change], inventory_id: String::new() })))
}

fn erase(plan: &mut Plan, geid: u64) -> Result<(), Status> {
    let body = erase_command::Body { entity_id: geid, preconditions: Vec::new() };
    plan.command(&command(Params::Erase(EraseCommand { erasures: vec![body], inventory_id: String::new() })))
}

async fn stack(db: &Arango, owner: u64, op: StackOperation) -> Result<(Plan, Done), Status> {
    let source = owned_stowed(db, owner, op.geid).await?;
    let class = class_of(db, source["classGuidCrc"].clone()).await?;
    let limit = stack_limit(&class);
    if limit < 2 {
        return Err(Status::failed_precondition("that item does not stack"));
    }
    let inventory = if op.inventory_id.is_empty() {
        source["inventoryId"].as_str().unwrap_or_default().to_string()
    } else {
        need_inventory(&op.inventory_id, "inventory_id")?;
        op.inventory_id.clone()
    };
    let rows = items_in(db, owner, &inventory).await?;
    // The fullest stack with room takes it, so that stacks fill up one after the other.
    let target = rows
        .iter()
        .filter(|row| geid_of(row) != op.geid && row["classGuidCrc"] == source["classGuidCrc"] && stack_of(row) < limit)
        .max_by_key(|row| (stack_of(row), std::cmp::Reverse(geid_of(row))))
        .ok_or_else(|| Status::failed_precondition("there is no stack of that item with room to stack onto"))?;
    let have = stack_of(&source);
    let wanted = if op.amount == 0 { have } else { op.amount.min(have) };
    let moved = wanted.min(limit - stack_of(target));
    let mut plan = Plan::new(None, now_ms());
    plan.takes_from(op.geid);
    plan.refuse_live_entity(op.geid, owner);
    plan.refuse_live_entity(geid_of(target), owner);
    guard_stacks(&mut plan, &[&source, target]);
    let mut erased = Vec::new();
    if moved == have {
        erase(&mut plan, op.geid)?;
        erased.push(op.geid);
    } else {
        set_stack(&mut plan, op.geid, have - moved)?;
    }
    set_stack(&mut plan, geid_of(target), stack_of(target) + moved)?;
    Ok((plan, Done { geid: geid_of(target), class_crc: class_crc_of(target), erased, placed: Vec::new() }))
}

/// The stacks of one class made as few as the limit allows: the sizes after, in the order given (by size, the largest
/// first); 0 is a stack that goes.
pub fn merged(sizes: &[u32], limit: u32) -> Vec<u32> {
    let mut left: u64 = sizes.iter().map(|size| u64::from(*size)).sum();
    sizes
        .iter()
        .map(|_| {
            let now = left.min(u64::from(limit));
            left -= now;
            now as u32
        })
        .collect()
}

async fn stack_all(db: &Arango, owner: u64, op: StackAllOperation) -> Result<(Plan, Done), Status> {
    need_inventory(&op.inventory_id, "inventory_id")?;
    let rows = items_in(db, owner, &op.inventory_id).await?;
    let mut groups: BTreeMap<u32, Vec<&Value>> = BTreeMap::new();
    for row in &rows {
        let crc = class_crc_of(row);
        if op.class_crc == 0 || crc == op.class_crc {
            groups.entry(crc).or_default().push(row);
        }
    }
    let mut plan = Plan::new(None, now_ms());
    let (mut touched, mut changes, mut erased, mut first) = (Vec::new(), Vec::new(), Vec::new(), None);
    for (crc, mut stacks) in groups {
        if stacks.len() < 2 {
            continue;
        }
        let Ok(class) = class_of(db, json!(crc)).await else { continue };
        let limit = stack_limit(&class);
        if limit < 2 {
            continue;
        }
        stacks.sort_by_key(|row| (std::cmp::Reverse(stack_of(row)), geid_of(row)));
        let sizes: Vec<u32> = stacks.iter().map(|row| stack_of(row)).collect();
        for (row, size) in stacks.iter().zip(merged(&sizes, limit)) {
            if size != stack_of(row) {
                touched.push(*row);
                changes.push((geid_of(row), size));
            }
        }
        if first.is_none() || crc == op.class_crc {
            first = stacks.first().map(|row| (geid_of(row), crc));
        }
    }
    // The guard comes before the changes it guards.
    guard_stacks(&mut plan, &touched);
    for (geid, size) in changes {
        plan.takes_from(geid);
        if size == 0 {
            erase(&mut plan, geid)?;
            erased.push(geid);
        } else {
            set_stack(&mut plan, geid, size)?;
        }
    }
    let (geid, class_crc) = first.unwrap_or_default();
    Ok((plan, Done { geid, class_crc, erased, placed: Vec::new() }))
}

async fn split(db: &Arango, owner: u64, op: SplitOperation) -> Result<(Plan, Done), Status> {
    let source = owned_stowed(db, owner, op.geid).await?;
    let have = stack_of(&source);
    if op.amount == 0 || op.amount >= have {
        return Err(Status::failed_precondition(format!("a split takes 1 to {} from a stack of {have}", have.saturating_sub(1))));
    }
    let to = if op.to_inventory_id.is_empty() {
        source["inventoryId"].as_str().unwrap_or_default().to_string()
    } else {
        need_inventory(&op.to_inventory_id, "to_inventory_id")?;
        op.to_inventory_id.clone()
    };
    let geid = allocate_geid(db).await?;
    let mut plan = Plan::new(None, now_ms());
    plan.takes_from(op.geid);
    plan.refuse_live_entity(op.geid, owner);
    plan.refuse_live_inventory(&to, owner);
    guard_stacks(&mut plan, &[&source]);
    refuse_taken(&mut plan, geid);
    set_stack(&mut plan, op.geid, have - op.amount)?;
    let mut new = entity_properties(&source);
    new.geid = geid;
    new.stack_size = op.amount;
    new.parent_urn = String::new();
    new.unstowed_from_inventory_id = String::new();
    new.created_at = 0;
    new.version = 0;
    plan.command(&create_into(new, &to))?;
    need_target(&mut plan, &to);
    Ok((plan, Done { geid, class_crc: class_crc_of(&source), ..Done::default() }))
}

/// Where an item is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Port { parent: u64, port_crc: u32 },
    Inventory(String),
}

fn port_by_crc<'a>(class: &'a Value, crc: u32) -> Option<&'a Value> {
    class["ports"].as_array()?.iter().find(|port| port_crc(text(port, "name")) == crc && equippable(port))
}

/// `geid` and what hangs from it, parents first.
fn below(items: &[Worn], geid: u64) -> Vec<&Worn> {
    let mut found: Vec<&Worn> = items.iter().filter(|item| item.geid == geid).collect();
    let mut next = 0;
    while next < found.len() {
        let parent = found[next].geid;
        found.extend(items.iter().filter(|item| item.parent == parent));
        next += 1;
    }
    found
}

fn placed_of(item: &Worn) -> Placed {
    Placed { geid: item.geid, class_crc: item.class["guidHashCrc"].as_u64().unwrap_or(0) as u32, parent: item.parent, port_crc: item.port_crc }
}

/// What hangs from the stowed item `geid`, parents first (its own attached children, which stay on it).
async fn attached_below(db: &Arango, geid: u64) -> Result<Vec<Placed>, Status> {
    let query = format!(
        "WITH entities, inventories
         FOR v, x IN 1..{ATTACHED_DEPTH} INBOUND @start entity_edges
             PRUNE IS_SAME_COLLECTION('inventories', v)
             OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
             FILTER x.type == @attached AND IS_SAME_COLLECTION('entities', v)
             RETURN {{ geid: v._key, classCrc: v.classGuidCrc, parent: x.endId, port: x.attachmentPortCrc }}"
    );
    let bind = json!({ "start": doc::entity_id(geid), "attached": sc_proto::services::entitygraph::v1::EntityEdgeType::AttachedTo as i32 });
    let rows = db.aql(&query, bind).await.map_err(internal)?;
    Ok(rows
        .iter()
        .map(|row| Placed {
            geid: doc::big(row, "geid"),
            class_crc: row["classCrc"].as_u64().unwrap_or(0) as u32,
            parent: doc::big(row, "parent"),
            port_crc: row["port"].as_u64().unwrap_or(0) as u32,
        })
        .collect())
}

/// Why `class` cannot go to the port `crc` of the worn item `parent`.
fn check_fit(items: &[Worn], parent: u64, crc: u32, class: &Value) -> Result<(), Status> {
    let parent = items.iter().find(|item| item.geid == parent).ok_or_else(|| Status::failed_precondition("the port's item is not worn"))?;
    let port = port_by_crc(&parent.class, crc).ok_or_else(|| Status::failed_precondition("that is not a port to equip to"))?;
    fits(port, class).map_err(Status::failed_precondition)
}

async fn swap(db: &Arango, owner: u64, op: SwapOperation) -> Result<(Plan, Done), Status> {
    if op.geid == 0 || op.other_geid == 0 || op.geid == op.other_geid {
        return Err(invalid("two different items are needed"));
    }
    let items = worn(db, owner).await?;
    let mut places = Vec::new();
    let mut rows = Vec::new();
    for geid in [op.geid, op.other_geid] {
        let row = entity(db, geid).await?;
        if row["ownerId"].as_str() != Some(owner.to_string().as_str()) {
            return Err(Status::permission_denied("an item belongs to someone else"));
        }
        let place = match items.iter().find(|item| item.geid == geid) {
            Some(found) => {
                if !removable(&items, found) {
                    return Err(Status::failed_precondition("that item is part of the character and cannot be swapped"));
                }
                Place::Port { parent: found.parent, port_crc: found.port_crc }
            }
            None if row["stowed"].as_bool() == Some(true) => Place::Inventory(row["inventoryId"].as_str().unwrap_or_default().to_string()),
            None => return Err(Status::failed_precondition("an item is neither worn nor stowed")),
        };
        places.push(place);
        rows.push(row);
    }
    let mut plan = Plan::new(None, now_ms());
    for geid in [op.geid, op.other_geid] {
        plan.refuse_live_entity(geid, owner);
    }
    let mut placed = Vec::new();
    match (&places[0], &places[1]) {
        (Place::Inventory(a), Place::Inventory(b)) => {
            if a != b {
                let body = |geid: u64, from: &str, to: &str| move_command::Body {
                    entity_id: geid,
                    from_inventory_id: from.to_string(),
                    to_inventory_id: to.to_string(),
                    ..Default::default()
                };
                plan.refuse_live_inventory(a, owner);
                plan.refuse_live_inventory(b, owner);
                plan.moves(&MoveCommand { moves: vec![body(op.geid, a, b), body(op.other_geid, b, a)] });
            }
        }
        (Place::Port { .. }, Place::Port { .. }) => {
            let (first, second) = (items.iter().find(|item| item.geid == op.geid).unwrap(), items.iter().find(|item| item.geid == op.other_geid).unwrap());
            // An item cannot be put into a port of what hangs from it.
            if below(&items, first.geid).iter().any(|item| item.geid == second.geid) || below(&items, second.geid).iter().any(|item| item.geid == first.geid) {
                return Err(Status::failed_precondition("one of the items hangs from the other"));
            }
            check_fit(&items, second.parent, second.port_crc, &class_of(db, rows[0]["classGuidCrc"].clone()).await?)?;
            check_fit(&items, first.parent, first.port_crc, &class_of(db, rows[1]["classGuidCrc"].clone()).await?)?;
            plan.reattach(first.geid, second.parent, second.port_crc);
            plan.reattach(second.geid, first.parent, first.port_crc);
            for (item, parent, port) in [(first, second.parent, second.port_crc), (second, first.parent, first.port_crc)] {
                let mut moved = placed_of(item);
                (moved.parent, moved.port_crc) = (parent, port);
                placed.push(moved);
                placed.extend(below(&items, item.geid).into_iter().skip(1).map(placed_of));
            }
        }
        (Place::Inventory(_), Place::Port { .. }) | (Place::Port { .. }, Place::Inventory(_)) => {
            // `stowed` goes to the place of `on`, which goes to where `stowed` was.
            let (stowed_at, on_at) = if matches!(places[0], Place::Inventory(_)) { (0, 1) } else { (1, 0) };
            let (stowed_geid, on_geid) = if stowed_at == 0 { (op.geid, op.other_geid) } else { (op.other_geid, op.geid) };
            let (Place::Inventory(inventory), Place::Port { parent, port_crc }) = (&places[stowed_at], &places[on_at]) else { unreachable!() };
            if stack_of(&rows[stowed_at]) > 1 {
                return Err(Status::failed_precondition("a stack cannot be equipped: split one off first"));
            }
            let class = class_of(db, rows[stowed_at]["classGuidCrc"].clone()).await?;
            check_fit(&items, *parent, *port_crc, &class)?;
            plan.refuse_live_inventory(inventory, owner);
            plan.check(
                "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true AND e.inventoryId == @inventory
                                      LIMIT 1 RETURN 1) == 1, 'precondition: the item is not stowed there any more')",
                json!({ "key": stowed_geid.to_string(), "inventory": inventory }),
            );
            plan.unequip(on_geid, inventory);
            plan.equip(stowed_geid, *parent, *port_crc);
            placed.push(Placed { geid: stowed_geid, class_crc: class_crc_of(&rows[stowed_at]), parent: *parent, port_crc: *port_crc });
            placed.extend(attached_below(db, stowed_geid).await?);
        }
    }
    Ok((plan, Done { geid: op.geid, class_crc: class_crc_of(&rows[0]), erased: Vec::new(), placed }))
}

/// The rounds each magazine holds after the rounds of all were poured into as few as possible, `capacity` each, in the
/// order given (fullest first).
pub fn repooled(rounds: &[u32], capacity: u32) -> Vec<u32> {
    let mut left: u64 = rounds.iter().map(|count| u64::from(*count)).sum();
    rounds
        .iter()
        .map(|_| {
            let now = left.min(u64::from(capacity));
            left -= now;
            now as u32
        })
        .collect()
}

async fn repool_ammo(db: &Arango, owner: u64, op: RepoolAmmoOperation) -> Result<(Plan, Done), Status> {
    need_inventory(&op.inventory_id, "inventory_id")?;
    let rows = items_in(db, owner, &op.inventory_id).await?;
    let mut groups: BTreeMap<u32, Vec<&Value>> = BTreeMap::new();
    for row in rows.iter().filter(|row| row["ammoCount"].is_u64()) {
        if op.class_crc == 0 || class_crc_of(row) == op.class_crc {
            groups.entry(class_crc_of(row)).or_default().push(row);
        }
    }
    let mut plan = Plan::new(None, now_ms());
    let (mut touched, mut changes, mut first) = (Vec::new(), Vec::new(), None);
    for (crc, mut magazines) in groups {
        let count = |row: &Value| row["ammoCount"].as_u64().unwrap_or(0) as u32;
        magazines.sort_by_key(|row| (std::cmp::Reverse(count(row)), geid_of(row)));
        // The class's magazine size when the seed has it, else the fullest magazine is full.
        let class = class_of(db, json!(crc)).await.ok();
        let capacity = class
            .as_ref()
            .and_then(|class| class["ammoCapacity"].as_u64())
            .map_or_else(|| magazines.iter().map(|row| count(row)).max().unwrap_or(0), |capacity| capacity as u32);
        let rounds: Vec<u32> = magazines.iter().map(|row| count(row)).collect();
        for (row, after) in magazines.iter().zip(repooled(&rounds, capacity)) {
            if after != count(row) {
                touched.push(json!({ "key": geid_of(row).to_string(), "rounds": row["ammoCount"] }));
                changes.push((geid_of(row), after));
            }
        }
        if first.is_none() || crc == op.class_crc {
            first = magazines.first().map(|row| (geid_of(row), crc));
        }
    }
    plan.check(
        "RETURN ASSERT(LENGTH(FOR t IN @rows FOR e IN entities FILTER e._key == t.key AND e.stowed == true AND e.ammoCount == t.rounds
                              RETURN 1) == LENGTH(@rows), 'precondition: a magazine changed or is not stowed any more')",
        json!({ "rows": touched }),
    );
    for (geid, rounds) in changes {
        plan.takes_from(geid);
        plan.set_ammo(geid, rounds);
    }
    let (geid, class_crc) = first.unwrap_or_default();
    Ok((plan, Done { geid, class_crc, ..Done::default() }))
}

async fn drop_item(db: &Arango, owner: u64, op: DropOperation) -> Result<(Plan, Done), Status> {
    if op.shard_id.is_empty() {
        return Err(invalid("a shard is needed"));
    }
    let source = owned_stowed(db, owner, op.geid).await?;
    let have = stack_of(&source);
    let (position, orientation) = ([op.x, op.y, op.z], [op.qx, op.qy, op.qz, op.qw]);
    let class_crc = class_crc_of(&source);
    if op.amount == 0 || op.amount >= have {
        let mut plan = Plan::new(None, now_ms());
        plan.takes_from(op.geid);
        plan.refuse_live_entity(op.geid, owner);
        plan.check(
            "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true LIMIT 1 RETURN 1) == 1,
                           'precondition: the item is not stowed in an inventory')",
            json!({ "key": op.geid.to_string() }),
        );
        plan.unstow(op.geid, &op.shard_id);
        plan.place_in_world(op.geid, op.zone_id, position, orientation, owner);
        return Ok((plan, Done { geid: op.geid, class_crc, ..Done::default() }));
    }
    // Part of a stack: a new item of the same kind lies in the world.
    let geid = allocate_geid(db).await?;
    let scope = Scope { r#type: ScopeType::Shard as i32, shard_id: op.shard_id.clone() };
    let mut plan = Plan::new(Some(&MutationArguments { scope: Some(scope), ..Default::default() }), now_ms());
    plan.takes_from(op.geid);
    plan.refuse_live_entity(op.geid, owner);
    guard_stacks(&mut plan, &[&source]);
    refuse_taken(&mut plan, geid);
    set_stack(&mut plan, op.geid, have - op.amount)?;
    let mut new = entity_properties(&source);
    new.geid = geid;
    new.stack_size = op.amount;
    new.owner_id = 0;
    new.parent_urn = String::new();
    new.unstowed_from_inventory_id = source["inventoryId"].as_str().unwrap_or_default().to_string();
    new.created_at = 0;
    new.version = 0;
    new.position = Some(Vec3d { x: op.x, y: op.y, z: op.z });
    new.orientation = Some(Quatd { x: op.qx, y: op.qy, z: op.qz, w: op.qw });
    // A `Create` with no inventory is live on the plan's shard.
    plan.command(&command(Params::Create(sc_proto::services::entitygraph::v1::CreateCommand {
        creates: vec![sc_proto::services::entitygraph::v1::create_command::Body {
            graphs: Some(sc_proto::services::entitygraph::v1::EntityGraph {
                nodes: vec![sc_proto::services::entitygraph::v1::Node {
                    properties: Some(sc_proto::services::entitygraph::v1::NodeProperties {
                        properties: Some(sc_proto::services::entitygraph::v1::node_properties::Properties::EntityProperties(new)),
                    }),
                    r#type: sc_proto::services::entitygraph::v1::NodeType::Entity as i32,
                }],
                edges: Vec::new(),
            }),
        }],
        inventory_id: String::new(),
    })))?;
    plan.place_in_world(geid, op.zone_id, position, orientation, owner);
    Ok((plan, Done { geid, class_crc, ..Done::default() }))
}

async fn pickup(db: &Arango, owner: u64, op: PickupOperation) -> Result<(Plan, Done), Status> {
    if op.shard_id.is_empty() || op.geid == 0 {
        return Err(invalid("an item and a shard are needed"));
    }
    let row = entity(db, op.geid).await?;
    let mine = [owner.to_string(), "0".to_string()];
    let lying_here = row["stowed"].as_bool() != Some(true)
        && row["shardId"].as_str() == Some(op.shard_id.as_str())
        && !row["droppedBy"].is_null()
        && row["ownerId"].as_str().is_some_and(|found| mine.iter().any(|mine| mine == found));
    if !lying_here {
        return Err(Status::not_found(format!("{} is not an item lying in the world of {}", op.geid, op.shard_id)));
    }
    let to = if op.to_inventory_id.is_empty() {
        format!("{owner}:{}:0", doc::PLAYER_INVENTORY)
    } else {
        need_inventory(&op.to_inventory_id, "to_inventory_id")?;
        op.to_inventory_id.clone()
    };
    let mut plan = Plan::new(None, now_ms());
    plan.check(
        "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed != true AND e.shardId == @shard AND e.droppedBy != null
                              LIMIT 1 RETURN 1) == 1, 'precondition: the item is not lying in the world any more')",
        json!({ "key": op.geid.to_string(), "shard": op.shard_id }),
    );
    plan.pick_up(op.geid, &to, owner);
    Ok((plan, Done { geid: op.geid, class_crc: class_crc_of(&row), ..Done::default() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_merge_into_as_few_as_the_limit_allows() {
        assert_eq!(merged(&[60, 50, 5], 99), [99, 16, 0]);
        assert_eq!(merged(&[99, 99], 99), [99, 99]);
        assert_eq!(merged(&[1, 1, 1], 99), [3, 0, 0]);
        assert_eq!(merged(&[500, 500], 999), [999, 1]);
        assert_eq!(merged(&[], 99), Vec::<u32>::new());
    }

    #[test]
    fn rounds_are_poured_into_the_fullest_magazines_first() {
        assert_eq!(repooled(&[20, 15, 5, 0], 30), [30, 10, 0, 0]);
        assert_eq!(repooled(&[30, 30], 30), [30, 30]);
        assert_eq!(repooled(&[7], 30), [7]);
    }

    #[test]
    fn what_may_stack_is_the_games_limit() {
        assert_eq!(stack_limit(&json!({ "itemType": "Currency" })), 999);
        assert_eq!(stack_limit(&json!({ "itemType": "FPS_Consumable", "stackable": true })), 99);
        assert_eq!(stack_limit(&json!({ "itemType": "WeaponPersonal" })), 1);
    }

    #[test]
    fn a_recorded_answer_reads_back() {
        let done = Done {
            geid: 400_000_000_007,
            class_crc: 9,
            erased: vec![400_000_000_008],
            placed: vec![Placed { geid: 1, class_crc: 2, parent: 3, port_crc: 4 }],
        };
        assert_eq!(Done::read(&done.json()), done);
    }

    #[test]
    fn what_hangs_from_an_item_is_found_parents_first() {
        let worn = |geid: u64, parent: u64| Worn { geid, class: json!({ "guidHashCrc": geid }), parent, port_crc: geid as u32, depth: 1 };
        let items = [worn(1, 100), worn(2, 1), worn(3, 2), worn(4, 1), worn(5, 100)];
        assert_eq!(below(&items, 1).iter().map(|item| item.geid).collect::<Vec<_>>(), [1, 2, 4, 3]);
        assert_eq!(below(&items, 5).len(), 1);
        assert!(below(&items, 9).is_empty());
    }
}
