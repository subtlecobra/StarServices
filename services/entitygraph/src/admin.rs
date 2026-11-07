//! The admin RPCs of `EntityGraphInternal` (see the "Admin inventories" block of internal.proto and
//! docs/INVENTORY.md). sc-admin-web asks them for a player: a mutation is a `Plan` run as that player's own command
//! would be, so ownership, capacity and loops are checked the same way, plus a refusal of anything live on a shard.
//! Each is applied once per command id, and the record of it keeps the answer, so a repeat gets the same one.

use std::collections::{HashMap, HashSet};

use sc_db::{internal, Arango};
use sc_proto::{
    internal::{
        AdminAddItemRequest, AdminAddItemResponse, AdminEntityClass, AdminInventory, AdminInventoryItem,
        AdminInventoryTreeRequest, AdminInventoryTreeResponse, AdminMoveItemRequest, AdminMoveItemResponse,
        AdminRemoveItemRequest, AdminRemoveItemResponse, AdminSearchEntityClassesRequest,
        AdminSearchEntityClassesResponse,
    },
    services::entitygraph::v1::{
        create_command, entity_graph_command::Params, erase_command, move_command, node_properties, unstack_command,
        CreateCommand, EntityGraph, EntityGraphCommand, EntityNodeProperties, EraseCommand, MoveCommand, Node,
        NodeProperties, NodeType, UnstackCommand,
    },
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    commands::{Plan, WRITES},
    doc, now_ms, write_error,
};

/// Items made here (an added item, the new half of a split) are numbered from this GEID up, by a counter in
/// `entitygraph_meta`. Far above the players (300,000,000,000 plus the account index) and the container tree (from
/// 0x2e90cf4b80, about 200,000,000,000), and below 2^53, so ArangoDB's doubles keep it whole.
pub const FIRST_ITEM_GEID: u64 = 400_000_000_000;
const DEFAULT_ITEMS: u32 = 500;
const MAX_ITEMS: u32 = 2_000;
const DEFAULT_CLASSES: u32 = 50;
const MAX_CLASSES: u32 = 200;
const MAX_STACK: u32 = 1_000_000;
const MAX_DEPTH: u32 = 64;
const MAX_TEXT: usize = 200;
/// Tries at the GEID counter when two calls write it at once.
const COUNTER_TRIES: usize = 5;

fn invalid(text: &str) -> Status {
    Status::invalid_argument(text.to_string())
}

fn need_command(command_id: &str) -> Result<(), Status> {
    if command_id.is_empty() || command_id.len() > MAX_TEXT {
        return Err(invalid("a command id of 1 to 200 characters is needed"));
    }
    Ok(())
}

fn need_owner(owner: u64) -> Result<(), Status> {
    if owner == 0 {
        return Err(invalid("an owner is needed"));
    }
    Ok(())
}

fn need_inventory(id: &str, field: &str) -> Result<(), Status> {
    if doc::parse_inventory_id(id).is_none() {
        return Err(Status::invalid_argument(format!("{field}: not an inventory id (<holder>:<context>:<subject>)")));
    }
    Ok(())
}

/// The record of an applied command: its own key space, so a client's command ids cannot meet an admin's.
fn record_key(command_id: &str) -> String {
    doc::key(&format!("admin-rpc:{command_id}"))
}

async fn recorded(db: &Arango, command_id: &str) -> Result<Option<Value>, Status> {
    let query = "FOR c IN applied_commands FILTER c._key == @id RETURN c.result";
    let rows = db.aql(query, json!({ "id": record_key(command_id) })).await.map_err(internal)?;
    Ok(rows.into_iter().next().map(|result| if result.is_null() { json!({}) } else { result }))
}

/// Runs the plan for `owner` with the record of `command_id` and its `result` in the same transaction. A twin request
/// that committed first leaves its record, which is then answered.
async fn apply_once(db: &Arango, command_id: &str, owner: u64, plan: Plan, result: Value) -> Result<(Value, bool), Status> {
    let mut statements = plan.statements(owner);
    statements.push((
        "INSERT { _key: @id, at: @at, result: @result } INTO applied_commands".to_string(),
        json!({ "id": record_key(command_id), "at": now_ms(), "result": result }),
    ));
    let borrowed: Vec<(&str, Value)> = statements.iter().map(|(query, bind)| (query.as_str(), bind.clone())).collect();
    match db.transaction(WRITES, &borrowed).await {
        Ok(()) => Ok((result, false)),
        Err(error) if error.contains("unique constraint violated") => match recorded(db, command_id).await? {
            Some(found) => Ok((found, true)),
            None => Err(write_error(error)),
        },
        Err(error) => Err(write_error(error)),
    }
}

/// The next item GEID. One that a failed transaction leaves unused is simply skipped.
async fn allocate_geid(db: &Arango) -> Result<u64, Status> {
    let query = "UPSERT { _key: 'item_geid' } INSERT { _key: 'item_geid', next: @first + 1 }
                 UPDATE { next: OLD.next + 1 } IN entitygraph_meta
                 RETURN NEW.next - 1";
    let mut last = String::new();
    for _ in 0..COUNTER_TRIES {
        match db.aql(query, json!({ "first": FIRST_ITEM_GEID })).await {
            Ok(rows) => {
                let geid = rows.first().and_then(Value::as_f64).map_or(0, |geid| geid as u64);
                if geid < FIRST_ITEM_GEID {
                    return Err(Status::internal("the item GEID counter is broken"));
                }
                return Ok(geid);
            }
            Err(error) => last = error,
        }
    }
    Err(internal(format!("item GEID: {last}")))
}

async fn entity(db: &Arango, geid: u64) -> Result<Value, Status> {
    let query = "FOR e IN entities FILTER e._key == @key RETURN e";
    let rows = db.aql(query, json!({ "key": geid.to_string() })).await.map_err(internal)?;
    rows.into_iter().next().ok_or_else(|| Status::not_found(format!("no entity {geid}")))
}

fn command(params: Params) -> EntityGraphCommand {
    EntityGraphCommand { params: Some(params) }
}

/// A `Create` of one entity stowed in `inventory_id`, which brings its container inventory when its class has one.
fn create_into(properties: EntityNodeProperties, inventory_id: &str) -> EntityGraphCommand {
    let node = Node {
        properties: Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(properties)) }),
        r#type: NodeType::Entity as i32,
    };
    let graph = EntityGraph { nodes: vec![node], edges: Vec::new() };
    command(Params::Create(CreateCommand {
        creates: vec![create_command::Body { graphs: Some(graph) }],
        inventory_id: inventory_id.to_string(),
    }))
}

/// Guards a new GEID: `Create` replaces a document with the same key.
fn refuse_taken(plan: &mut Plan, geid: u64) {
    plan.check(
        "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key LIMIT 1 RETURN 1) == 0, 'precondition: GEID in use')",
        json!({ "key": geid.to_string() }),
    );
}

/// After `Create`: the target exists, which `Create` alone does not check.
fn need_target(plan: &mut Plan, inventory_id: &str) {
    plan.check(
        "RETURN ASSERT(LENGTH(FOR i IN inventories FILTER i._key == @key LIMIT 1 RETURN 1) == 1,
                       'precondition: the target inventory does not exist')",
        json!({ "key": doc::key(inventory_id) }),
    );
}

fn entity_properties(row: &Value) -> EntityNodeProperties {
    match doc::entity_node(row).properties.and_then(|properties| properties.properties) {
        Some(node_properties::Properties::EntityProperties(entity)) => entity,
        _ => EntityNodeProperties::default(),
    }
}

pub async fn move_item(db: &Arango, request: AdminMoveItemRequest) -> Result<AdminMoveItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    need_inventory(&request.from_inventory_id, "from_inventory_id")?;
    need_inventory(&request.to_inventory_id, "to_inventory_id")?;
    if request.geid == 0 {
        return Err(invalid("an item is needed"));
    }
    if request.from_inventory_id == request.to_inventory_id {
        return Err(invalid("the item is in that inventory already"));
    }
    if let Some(found) = recorded(db, &request.command_id).await? {
        return Ok(AdminMoveItemResponse { moved_geid: doc::big(&found, "geid"), replayed: true });
    }
    let row = entity(db, request.geid).await?;
    let stack = row["stackSize"].as_u64().unwrap_or(0).max(1) as u32;
    let (owner, to) = (request.owner_id, request.to_inventory_id.as_str());
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_entity(request.geid, owner);
    plan.refuse_live_inventory(to, owner);
    let moved = if request.quantity == 0 || request.quantity >= stack {
        let body = move_command::Body {
            entity_id: request.geid,
            from_inventory_id: request.from_inventory_id.clone(),
            to_inventory_id: to.to_string(),
            ..Default::default()
        };
        plan.moves(&MoveCommand { moves: vec![body] });
        request.geid
    } else {
        // A split: the stack shrinks where it is, and a new item of the same kind takes the rest to the target.
        let geid = allocate_geid(db).await?;
        plan.takes_from(request.geid);
        plan.check(
            "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true AND e.inventoryId == @from
                                  AND e.stackSize == @stack LIMIT 1 RETURN 1) == 1,
                           'precondition: the item is not in the source inventory, or its stack changed')",
            json!({ "key": request.geid.to_string(), "from": request.from_inventory_id, "stack": row["stackSize"] }),
        );
        refuse_taken(&mut plan, geid);
        // Below the stack, which is below MAX_STACK or what the game wrote, so it fits.
        let unstack = unstack_command::Body { entity_id: request.geid, amount: request.quantity.min(i32::MAX as u32) as i32 };
        plan.command(&command(Params::Unstack(UnstackCommand { unstacks: vec![unstack], inventory_id: String::new() })))?;
        let mut split = entity_properties(&row);
        split.geid = geid;
        split.stack_size = request.quantity;
        split.parent_urn = String::new();
        split.unstowed_from_inventory_id = String::new();
        split.created_at = 0;
        split.version = 0;
        plan.command(&create_into(split, to))?;
        need_target(&mut plan, to);
        geid
    };
    let (result, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "geid": moved.to_string() })).await?;
    info!(actor = %request.actor, reason = %request.reason, geid = request.geid, to, replayed, "admin moved an item");
    Ok(AdminMoveItemResponse { moved_geid: doc::big(&result, "geid"), replayed })
}

pub async fn add_item(db: &Arango, request: AdminAddItemRequest) -> Result<AdminAddItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    need_inventory(&request.inventory_id, "inventory_id")?;
    let guid = request.entity_class_guid.trim().to_lowercase();
    if guid.is_empty() || guid.len() > 64 {
        return Err(invalid("an entity class GUID is needed"));
    }
    if request.stack_size > MAX_STACK {
        return Err(Status::invalid_argument(format!("a stack of at most {MAX_STACK}")));
    }
    if let Some(found) = recorded(db, &request.command_id).await? {
        return Ok(AdminAddItemResponse { geid: doc::big(&found, "geid"), replayed: true });
    }
    let query = "FOR c IN entity_classes FILTER c._key == @guid OR c.guid == @guid LIMIT 1 RETURN c";
    let class = db.aql(query, json!({ "guid": guid })).await.map_err(internal)?.into_iter().next();
    let class = class.ok_or_else(|| Status::not_found(format!("no entity class {guid}: run sc-admin seed")))?;
    let geid = allocate_geid(db).await?;
    let (owner, to) = (request.owner_id, request.inventory_id.as_str());
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_inventory(to, owner);
    refuse_taken(&mut plan, geid);
    let properties = EntityNodeProperties {
        geid,
        class_guid_crc: class["guidHashCrc"].as_u64().unwrap_or(0) as u32,
        stack_size: request.stack_size.max(1),
        owner_id: owner,
        scale: 1.0,
        item_type_enum: class["itemTypeEnum"].as_i64().unwrap_or(0) as i32,
        item_sub_type_enum: class["itemSubTypeEnum"].as_i64().unwrap_or(0) as i32,
        debug: HashMap::from([("createdBy".to_string(), "admin".to_string())]),
        ..Default::default()
    };
    plan.command(&create_into(properties, to))?;
    need_target(&mut plan, to);
    let (result, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "geid": geid.to_string() })).await?;
    info!(actor = %request.actor, reason = %request.reason, class = %guid, geid, to, replayed, "admin added an item");
    Ok(AdminAddItemResponse { geid: doc::big(&result, "geid"), replayed })
}

pub async fn remove_item(db: &Arango, request: AdminRemoveItemRequest) -> Result<AdminRemoveItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    if request.geid == 0 {
        return Err(invalid("an item is needed"));
    }
    if let Some(found) = recorded(db, &request.command_id).await? {
        let erased = found["erased"].as_u64().unwrap_or(0) as u32;
        return Ok(AdminRemoveItemResponse { removed: true, erased, replayed: true });
    }
    let row = entity(db, request.geid).await?;
    if request.geid == request.owner_id || row["classGuidCrc"].as_u64() == Some(u64::from(doc::PLAYER_CLASS_CRC)) {
        return Err(invalid("a player is not an item"));
    }
    let below = format!(
        "WITH entities, inventories
         FOR v IN 0..{MAX_DEPTH} INBOUND @start entity_edges
             OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
             FILTER IS_SAME_COLLECTION('entities', v)
             COLLECT WITH COUNT INTO n
             RETURN n"
    );
    let counts = db.aql(&below, json!({ "start": doc::entity_id(request.geid) })).await.map_err(internal)?;
    let erased = counts.first().and_then(Value::as_u64).unwrap_or(1);
    let owner = request.owner_id;
    let mut plan = Plan::new(None, now_ms());
    plan.takes_from(request.geid);
    plan.check(
        "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true LIMIT 1 RETURN 1) == 1,
                       'precondition: the item is not stowed in an inventory')",
        json!({ "key": request.geid.to_string() }),
    );
    plan.refuse_live_entity(request.geid, owner);
    // Nothing it holds may be out on a shard either.
    plan.check(
        format!(
            "WITH entities, inventories
             RETURN ASSERT(LENGTH(
                 FOR v IN 1..{MAX_DEPTH} INBOUND @start entity_edges
                     OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                     FILTER IS_SAME_COLLECTION('entities', v) AND v.stowed != true AND v.shardId != null AND v.shardId != ''
                     LIMIT 1
                     RETURN 1
             ) == 0, 'precondition: something the item holds is live on a shard')"
        ),
        json!({ "start": doc::entity_id(request.geid) }),
    );
    let erase = erase_command::Body { entity_id: request.geid, preconditions: Vec::new() };
    plan.command(&command(Params::Erase(EraseCommand { erasures: vec![erase], inventory_id: String::new() })))?;
    let (result, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "erased": erased })).await?;
    info!(actor = %request.actor, reason = %request.reason, geid = request.geid, erased, replayed, "admin removed an item");
    Ok(AdminRemoveItemResponse { removed: true, erased: result["erased"].as_u64().unwrap_or(0) as u32, replayed })
}

/// `%`, `_` and `\` taken literally by LIKE.
fn like_pattern(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped.push('%');
    escaped
}

fn admin_class(row: &Value) -> AdminEntityClass {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    AdminEntityClass {
        guid: text(&row["guid"]),
        guid_hash_crc: row["guidHashCrc"].as_u64().unwrap_or(0) as u32,
        class_name: text(&row["className"]),
        display_name: text(&row["name"]["en"]),
        item_type: text(&row["itemType"]),
        item_sub_type: text(&row["itemSubType"]),
        occupancy_micro_scu: row["occupancyMicroScu"].as_f64().unwrap_or(0.0) as u64,
        capacity_micro_scu: row["inventory"]["capacityMicroScu"].as_f64().unwrap_or(0.0) as u64,
        container: row["inventory"].is_object(),
    }
}

pub async fn search_classes(
    db: &Arango,
    request: AdminSearchEntityClassesRequest,
) -> Result<AdminSearchEntityClassesResponse, Status> {
    let wanted = request.query.trim();
    if wanted.len() > MAX_TEXT {
        return Err(invalid("the query is too long"));
    }
    let limit = match request.limit {
        0 => DEFAULT_CLASSES,
        limit => limit.min(MAX_CLASSES),
    };
    let query = "FOR c IN entity_classes
                     FILTER @guid == '' OR c._key == @guid OR LIKE(c.className, @pattern, true)
                         OR LIKE(NOT_NULL(c.name.en, ''), @pattern, true)
                     SORT c.className
                     LIMIT @limit
                     RETURN c";
    let bind = json!({ "guid": wanted.to_lowercase(), "pattern": like_pattern(wanted), "limit": limit });
    let rows = db.aql(query, bind).await.map_err(internal)?;
    Ok(AdminSearchEntityClassesResponse { classes: rows.iter().map(admin_class).collect() })
}

/// The inventories to show: the player's own, the containers of the entities it owns, and every inventory below those
/// and below the owned entities that are in no inventory (a ship, with its cargo grids attached), through items and
/// what holds them. With each one's holder, parent, items and whether it is live.
const TREE: &str = "
WITH entities, inventories
LET tops = UNION_DISTINCT(
    (FOR i IN inventories FILTER i.ownerId == @owner RETURN i._id),
    (FOR e IN entities FILTER e.ownerId == @owner
        FOR i IN inventories FILTER i.holderId == e._key AND i.ownerId IN ['0', @owner] RETURN i._id),
    (FOR e IN entities FILTER e.ownerId == @owner AND e.stowed != true RETURN e._id)
)
LET ids = UNIQUE(
    FOR top IN tops
        FOR v, x IN 0..@depth INBOUND top entity_edges
            PRUNE x != null AND x.type NOT IN @holding
            OPTIONS { order: 'bfs', uniqueVertices: 'global' }
            FILTER (x == null OR x.type IN @holding) AND IS_SAME_COLLECTION('inventories', v)
            RETURN v._id
)
FOR id IN ids
    LET i = DOCUMENT(id)
    FILTER i != null
    LET holder = DOCUMENT(CONCAT('entities/', i.holderId))
    LET parent = FIRST(
        FOR v, x IN 1..@depth OUTBOUND i entity_edges
            PRUNE x != null AND (x.type NOT IN @holding OR IS_SAME_COLLECTION('inventories', v))
            OPTIONS { order: 'bfs', uniqueVertices: 'global' }
            FILTER x.type IN @holding AND IS_SAME_COLLECTION('inventories', v)
            LIMIT 1
            RETURN v.id
    )
    LET live = LENGTH(
        FOR v, x IN 0..@depth OUTBOUND i entity_edges
            PRUNE x != null AND x.type NOT IN @holding
            OPTIONS { order: 'bfs', uniqueVertices: 'global' }
            FILTER (x == null OR x.type IN @holding) AND IS_SAME_COLLECTION('entities', v)
                AND v._key != @owner AND v.stowed != true AND v.shardId != null AND v.shardId != ''
            LIMIT 1
            RETURN 1
    ) > 0
    LET stowed = (FOR x IN entity_edges FILTER x._to == i._id AND x.type == @stowedIn RETURN x._from)
    LET items = (
        FOR from IN stowed
            LET e = DOCUMENT(from)
            FILTER e != null
            SORT LENGTH(e._key), e._key
            LIMIT @items
            LET c = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN c)
            LET held = (FOR h IN entity_edges FILTER h._to == e._id AND h.type == @attachedTo
                            AND IS_SAME_COLLECTION('inventories', h._from) RETURN h.startId)
            RETURN { e: KEEP(e, 'geid', 'classGuidCrc', 'stackSize', 'ownerId', 'stowed', 'shardId'),
                     className: c.className, guid: c.guid, display: c.name.en, held }
    )
    LET holderClass = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == holder.classGuidCrc RETURN c.className)
    RETURN { i: UNSET(i, 'dirty'), holderClass, parent, live, count: LENGTH(stowed), items }";

fn tree_item(row: &Value, inventory_id: &str) -> AdminInventoryItem {
    let e = &row["e"];
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let on_shard = e["stowed"].as_bool() != Some(true) && e["shardId"].as_str().is_some_and(|shard| !shard.is_empty());
    AdminInventoryItem {
        geid: doc::big(e, "geid"),
        class_guid_crc: e["classGuidCrc"].as_u64().unwrap_or(0) as u32,
        class_name: text(&row["className"]),
        stack_size: e["stackSize"].as_u64().unwrap_or(0) as u32,
        owner_id: doc::big(e, "ownerId"),
        inventory_id: inventory_id.to_string(),
        held_inventory_ids: row["held"].as_array().into_iter().flatten().filter_map(|id| id.as_str().map(str::to_string)).collect(),
        class_guid: text(&row["guid"]),
        display_name: text(&row["display"]),
        on_shard,
        shard_id: if on_shard { text(&e["shardId"]) } else { String::new() },
    }
}

/// The player's own inventories first (personal, then by id), then each one's children right after it, by id: a
/// tree read top down, parents before children.
fn ordered(mut inventories: Vec<AdminInventory>) -> Vec<AdminInventory> {
    let id = |inventory: &AdminInventory| inventory.inventory.as_ref().map_or(String::new(), |properties| properties.id.clone());
    let known: HashSet<String> = inventories.iter().map(id).collect();
    for inventory in &mut inventories {
        if !known.contains(&inventory.parent_inventory_id) {
            inventory.parent_inventory_id = String::new();
        }
    }
    let rank = |inventory: &AdminInventory| {
        let context = inventory.inventory.as_ref().map_or("", |properties| properties.context.as_str());
        (context != doc::PLAYER_INVENTORY, id(inventory))
    };
    inventories.sort_by_key(rank);
    let mut children: HashMap<String, Vec<AdminInventory>> = HashMap::new();
    for inventory in inventories {
        children.entry(inventory.parent_inventory_id.clone()).or_default().push(inventory);
    }
    let mut out = Vec::new();
    let mut stack: Vec<(AdminInventory, u32)> = children.remove("").unwrap_or_default().into_iter().rev().map(|top| (top, 0)).collect();
    while let Some((mut inventory, depth)) = stack.pop() {
        inventory.depth = depth;
        if let Some(below) = children.remove(&id(&inventory)) {
            stack.extend(below.into_iter().rev().map(|child| (child, depth + 1)));
        }
        out.push(inventory);
    }
    // A loop the walk could not place (it cannot happen, as stowing refuses loops) is shown at the end, not lost.
    out.extend(children.into_values().flatten());
    out
}

pub async fn tree(db: &Arango, request: AdminInventoryTreeRequest) -> Result<AdminInventoryTreeResponse, Status> {
    need_owner(request.owner_id)?;
    let items = match request.max_items {
        0 => DEFAULT_ITEMS,
        items => items.min(MAX_ITEMS),
    };
    let bind = json!({
        "owner": request.owner_id.to_string(),
        "depth": MAX_DEPTH,
        "items": items,
        "holding": [
            sc_proto::services::entitygraph::v1::EntityEdgeType::AttachedTo as i32,
            sc_proto::services::entitygraph::v1::EntityEdgeType::StowedIn as i32,
        ],
        "stowedIn": sc_proto::services::entitygraph::v1::EntityEdgeType::StowedIn as i32,
        "attachedTo": sc_proto::services::entitygraph::v1::EntityEdgeType::AttachedTo as i32,
    });
    let rows = db.aql(TREE, bind).await.map_err(internal)?;
    let inventories = rows
        .iter()
        .map(|row| {
            let properties = doc::inventory_properties(&row["i"]);
            let items = row["items"].as_array().into_iter().flatten().map(|item| tree_item(item, &properties.id)).collect();
            AdminInventory {
                parent_inventory_id: row["parent"].as_str().unwrap_or_default().to_string(),
                items,
                holder_id: doc::big(&row["i"], "holderId"),
                holder_class_name: row["holderClass"].as_str().unwrap_or_default().to_string(),
                item_count: row["count"].as_u64().unwrap_or(0) as u32,
                depth: 0,
                live: row["live"].as_bool().unwrap_or(false),
                inventory: Some(properties),
            }
        })
        .collect();
    let mut inventories = ordered(inventories);
    if request.max_depth > 0 {
        inventories.retain(|inventory| inventory.depth <= request.max_depth);
    }
    Ok(AdminInventoryTreeResponse { inventories })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::entitygraph::v1::InventoryNodeProperties;

    fn inventory(id: &str, context: &str, parent: &str) -> AdminInventory {
        AdminInventory {
            inventory: Some(InventoryNodeProperties { id: id.into(), context: context.into(), ..Default::default() }),
            parent_inventory_id: parent.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_tree_reads_top_down_with_the_personal_inventory_first() {
        let found = ordered(vec![
            inventory("9:Container:0", "Container", "1:Location:5"),
            inventory("1:Location:5", "Location", ""),
            inventory("8:Container:0", "Container", "9:Container:0"),
            inventory("1:PlayerInventory:0", "PlayerInventory", ""),
            inventory("7:Container:0", "Container", "gone:Container:0"),
        ]);
        let ids: Vec<(&str, u32)> =
            found.iter().map(|i| (i.inventory.as_ref().unwrap().id.as_str(), i.depth)).collect();
        assert_eq!(
            ids,
            [
                ("1:PlayerInventory:0", 0),
                ("1:Location:5", 0),
                ("9:Container:0", 1),
                ("8:Container:0", 2),
                ("7:Container:0", 0),
            ]
        );
        assert_eq!(found[4].parent_inventory_id, "", "a parent outside the tree makes a top");
    }

    #[test]
    fn like_patterns_take_wildcards_literally() {
        assert_eq!(like_pattern("a_b%c\\"), "%a\\_b\\%c\\\\%");
    }

    #[test]
    fn mutations_need_their_fields() {
        assert_eq!(need_command("").unwrap_err().code(), tonic::Code::InvalidArgument);
        assert_eq!(need_owner(0).unwrap_err().code(), tonic::Code::InvalidArgument);
        assert!(need_inventory("1:Location:5", "to").is_ok());
        assert_eq!(need_inventory("1:Location", "to").unwrap_err().code(), tonic::Code::InvalidArgument);
        assert_ne!(record_key("admin:x"), doc::key("admin:x"), "an admin record cannot meet a client's command id");
    }

    #[test]
    fn a_seeded_class_reads_its_room_and_container() {
        let row = json!({ "guid": "g", "guidHashCrc": 7, "className": "crate_1", "name": { "en": "Crate" },
                          "occupancyMicroScu": 500_000, "inventory": { "capacityMicroScu": 900_000, "closed": true } });
        let class = admin_class(&row);
        assert_eq!((class.display_name.as_str(), class.occupancy_micro_scu, class.capacity_micro_scu, class.container), ("Crate", 500_000, 900_000, true));
        assert!(!admin_class(&json!({ "guid": "h" })).container);
    }
}
