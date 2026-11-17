//! Equipping: what a player wears and the admin calls that change it (see "Equipping" in docs/INVENTORY.md).
//!
//! A worn item is an entity with an `ATTACHED_TO` edge to a port of another worn item, the roots to the player (the body at
//! `Body_ItemPort`). The ports of a class come from the seed (`entity_classes.ports`, read from the item's
//! `SItemPortContainerComponentParams`); an edge keeps only the CRC of the port's name. Whether an item fits a port is
//! `fits`: its type and sub type, its size and its tags against the port's.

use std::collections::{HashMap, HashSet};

use sc_api::internal::{
    AdminEquipItemRequest, AdminEquipItemResponse, AdminEquipPort, AdminRelocateItemRequest, AdminRelocateItemResponse, AdminEquipmentRequest, AdminEquipmentResponse, AdminUnequipItemRequest,
    AdminUnequipItemResponse, AdminWornItem, EquippedItem, FindStowedItemRequest, FindStowedItemResponse,
};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::EntityEdgeType;
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    admin::{apply_once, invalid, need_command, need_inventory, need_owner, recorded},
    commands::Plan,
    doc, now_ms,
};

/// Levels of worn items below the player (body, undersuit, armor, backpack, weapon, magazine, optic ...).
const WORN_DEPTH: u32 = 8;
/// Stowed items looked at for the ones that fit a port.
const CANDIDATES: u32 = 2000;

/// The key of a port: CRC-32C of the lowercased name, which is what the client keys the ports of an item port container by
/// (the DGS has the same in `loadout::port_attach_id`).
pub fn port_crc(name: &str) -> u32 {
    !name.bytes().map(|byte| byte.to_ascii_lowercase()).fold(!0u32, |crc, byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
    })
}

pub(crate) fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or_default()
}

fn tags(list: &str) -> HashSet<String> {
    list.split_whitespace().map(|tag| tag.trim_start_matches('$').to_lowercase()).collect()
}

/// Ports a person never equips to: the head and lens (the character), the MobiGlas, what is held in a hand, the internal
/// inventory pockets and the restraints. Assumed from the port names and flags of the body in 4.10.2.
pub fn equippable(port: &Value) -> bool {
    let name = text(port, "name").to_lowercase();
    let flags = text(port, "flags").to_lowercase();
    !(name.starts_with("weapon_attach_hand")
        || matches!(name.as_str(), "head_itemport" | "lens_attach" | "mobiglas_attach")
        || flags.split_whitespace().any(|flag| matches!(flag, "inventoryinternalonly" | "restraint_attach")))
}

/// The hand ports of the body: what is attached to one is what the character carries. They are held in, not equipped to: the
/// game lists no types for them (`flags` `selfphys select usegripdata`), so `fits` takes whatever is carried.
pub fn holdable(port: &Value) -> bool {
    text(port, "name").to_lowercase().starts_with("weapon_attach_hand")
}

/// Whether an item can be put on `port` or taken off it: equipment and what is carried.
pub fn takes_item(port: &Value) -> bool {
    equippable(port) || holdable(port)
}

/// Why `class` does not fit `port`, if it does not. A sub type of '' in the game's data was dropped by the seed.
pub fn fits(port: &Value, class: &Value) -> Result<(), String> {
    if holdable(port) && port["types"].as_array().is_none_or(Vec::is_empty) {
        return Ok(());
    }
    let (kind, sub) = (text(class, "itemType"), text(class, "itemSubType"));
    let kind_fits = port["types"].as_array().into_iter().flatten().any(|allowed| {
        text(allowed, "itemType") == kind && {
            let subs = allowed["itemSubTypes"].as_array().map(Vec::as_slice).unwrap_or_default();
            subs.is_empty() || subs.iter().any(|allowed| allowed.as_str() == Some(sub))
        }
    });
    if !kind_fits {
        return Err(format!("{kind} {sub} does not go in that port"));
    }
    if let Some(size) = class["attachSize"].as_i64() {
        let (min, max) = (port["minSize"].as_i64().unwrap_or(0), port["maxSize"].as_i64().unwrap_or(0));
        if size < min || size > max {
            return Err(format!("size {size} does not fit a port of sizes {min} to {max}"));
        }
    }
    let item_tags = tags(text(class, "attachTags"));
    if let Some(missing) = tags(text(port, "requiredPortTags")).into_iter().find(|tag| !item_tags.contains(tag)) {
        return Err(format!("the port needs the tag {missing}"));
    }
    let port_tags = tags(text(port, "portTags"));
    if let Some(missing) = tags(text(class, "requiredTags")).into_iter().find(|tag| !port_tags.contains(tag)) {
        return Err(format!("the item needs a port with the tag {missing}"));
    }
    Ok(())
}

/// The port of `class` called `name`, case ignored.
pub(crate) fn port_named<'a>(class: &'a Value, name: &str) -> Option<&'a Value> {
    class["ports"].as_array()?.iter().find(|port| text(port, "name").eq_ignore_ascii_case(name))
}

/// An item the player wears now.
#[derive(Clone)]
pub struct Worn {
    pub geid: u64,
    pub class: Value,
    pub parent: u64,
    pub port_crc: u32,
    pub depth: usize,
}

impl Worn {
    pub fn class_name(&self) -> &str {
        text(&self.class, "className")
    }

    fn display_name(&self) -> &str {
        text(&self.class["name"], "en")
    }

    /// What it is called in words.
    fn label(&self) -> String {
        let name = if self.display_name().is_empty() { self.class_name() } else { self.display_name() };
        if name.is_empty() { format!("item {}", self.geid) } else { name.to_string() }
    }
}

/// Everything attached, at any depth, to the player, parents before children.
pub async fn worn(db: &Arango, player: u64) -> Result<Vec<Worn>, Status> {
    // One query per level, not a traversal: a traversal crosses the shards of the cluster for every vertex (0.5 s for 55 items, and
    // a join waits for this), a level is one indexed query for its edges and one for its items.
    let mut rows: Vec<Value> = Vec::new();
    let mut level = vec![player.to_string()];
    let mut seen: HashSet<String> = level.iter().cloned().collect();
    for depth in 1..=WORN_DEPTH {
        let aql = "FOR x IN entity_edges FILTER x.endId IN @ids FILTER x.type == @attached FILTER x.startId != null RETURN x";
        let edges = db.aql(aql, json!({ "ids": level, "attached": EntityEdgeType::AttachedTo as i32 })).await.map_err(internal)?;
        let edges: Vec<Value> = edges
            .into_iter()
            .filter(|edge| edge["_from"].as_str().is_some_and(|from| from.starts_with("entities/")))
            .filter(|edge| edge["startId"].as_str().is_some_and(|key| seen.insert(key.to_string())))
            .collect();
        if edges.is_empty() {
            break;
        }
        let keys: Vec<&str> = edges.iter().filter_map(|edge| edge["startId"].as_str()).collect();
        let items = db.aql("FOR e IN entities FILTER e._key IN @keys RETURN { k: e._key, crc: e.classGuidCrc }", json!({ "keys": keys })).await.map_err(internal)?;
        let crc_of: HashMap<&str, &Value> = items.iter().filter_map(|item| Some((item["k"].as_str()?, &item["crc"]))).collect();
        for edge in &edges {
            let key = edge["startId"].as_str().unwrap_or_default();
            let crc = crc_of.get(key).map(|crc| (*crc).clone()).unwrap_or(Value::Null);
            rows.push(json!({ "geid": key, "parent": edge["endId"], "port": edge["attachmentPortCrc"], "depth": depth, "crc": crc }));
        }
        level = edges.iter().filter_map(|edge| edge["startId"].as_str().map(str::to_string)).collect();
    }
    let crcs: Vec<Value> = rows.iter().map(|row| row["crc"].clone()).filter(|crc| !crc.is_null()).collect::<HashSet<_>>().into_iter().collect();
    let classes = db
        .aql("FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs RETURN c", json!({ "crcs": crcs }))
        .await
        .map_err(internal)?;
    let class_of = |crc: &Value| classes.iter().find(|class| class["guidHashCrc"] == *crc).cloned().unwrap_or(Value::Null);
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Worn {
                geid: row["geid"].as_str()?.parse().ok()?,
                class: class_of(&row["crc"]),
                parent: row["parent"].as_str()?.parse().ok()?,
                port_crc: row["port"].as_u64().unwrap_or(0) as u32,
                depth: row["depth"].as_u64().unwrap_or(1) as usize,
            })
        })
        .collect())
}

/// Whether `item` sits in a port equipping reaches: not the body or the head, which are the character.
pub(crate) fn removable(items: &[Worn], item: &Worn) -> bool {
    items
        .iter()
        .find(|parent| parent.geid == item.parent)
        .and_then(|parent| parent.class["ports"].as_array())
        .is_some_and(|ports| ports.iter().any(|port| port_crc(text(port, "name")) == item.port_crc && takes_item(port)))
}

/// The name of a worn item's port on its parent, from the parent's class, else the CRC in hex.
fn port_label(items: &[Worn], item: &Worn) -> String {
    items
        .iter()
        .find(|parent| parent.geid == item.parent)
        .and_then(|parent| parent.class["ports"].as_array())
        .and_then(|ports| ports.iter().find(|port| port_crc(text(port, "name")) == item.port_crc))
        .map_or_else(|| format!("{:#010x}", item.port_crc), |port| text(port, "name").to_string())
}

struct Stowed {
    geid: u64,
    class: Value,
}

/// The player's stowed items of single pieces (a stack is moved out one at a time first) of the types any port takes.
async fn stowed(db: &Arango, player: u64, types: &[String]) -> Result<Vec<Stowed>, Status> {
    let query = "FOR e IN entities FILTER e.ownerId == @owner AND e.stowed == true AND NOT_NULL(e.stackSize, 1) <= 1
                     LIMIT @limit
                     LET c = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN UNSET(c, 'ports', 'description'))
                     FILTER c != null AND c.itemType IN @types
                     RETURN { geid: e._key, class: c }";
    let rows = db.aql(query, json!({ "owner": player.to_string(), "limit": CANDIDATES, "types": types })).await.map_err(internal)?;
    Ok(rows.into_iter().filter_map(|row| Some(Stowed { geid: row["geid"].as_str()?.parse().ok()?, class: row["class"].clone() })).collect())
}

pub async fn equipment(db: &Arango, request: AdminEquipmentRequest) -> Result<AdminEquipmentResponse, Status> {
    need_owner(request.owner_id)?;
    let items = worn(db, request.owner_id).await?;
    let mut types: HashSet<String> = HashSet::new();
    for item in &items {
        for port in item.class["ports"].as_array().into_iter().flatten().filter(|port| equippable(port)) {
            types.extend(port["types"].as_array().into_iter().flatten().map(|kind| text(kind, "itemType").to_string()));
        }
    }
    let types: Vec<String> = types.into_iter().collect();
    let candidates = if types.is_empty() { Vec::new() } else { stowed(db, request.owner_id, &types).await? };
    let mut ports = Vec::new();
    for item in &items {
        for port in item.class["ports"].as_array().into_iter().flatten().filter(|port| equippable(port)) {
            let crc = port_crc(text(port, "name"));
            let occupant = items.iter().find(|other| other.parent == item.geid && other.port_crc == crc);
            ports.push(AdminEquipPort {
                parent_geid: item.geid,
                parent_name: item.label(),
                port: text(port, "name").to_string(),
                occupant_geid: occupant.map_or(0, |other| other.geid),
                occupant_name: occupant.map(Worn::label).unwrap_or_default(),
                candidates: candidates.iter().filter(|stowed| fits(port, &stowed.class).is_ok()).map(|stowed| stowed.geid).collect(),
            });
        }
    }
    let worn = items
        .iter()
        .map(|item| AdminWornItem {
            geid: item.geid,
            class_name: item.class_name().to_string(),
            display_name: item.display_name().to_string(),
            item_type: text(&item.class, "itemType").to_string(),
            item_sub_type: text(&item.class, "itemSubType").to_string(),
            parent_geid: item.parent,
            port: port_label(&items, item),
            depth: item.depth as u32,
            removable: removable(&items, item),
        })
        .collect();
    Ok(AdminEquipmentResponse { worn, ports })
}

pub(crate) async fn class_of(db: &Arango, crc: Value) -> Result<Value, Status> {
    let cached = crc.as_u64().and_then(|key| CLASSES.lock().unwrap().get(&key).cloned());
    if let Some(class) = cached {
        return Ok(class);
    }
    let rows = db.aql("FOR c IN entity_classes FILTER c.guidHashCrc == @crc LIMIT 1 RETURN c", json!({ "crc": crc })).await.map_err(internal)?;
    let class = rows.into_iter().next().ok_or_else(|| Status::not_found("the item's class is not seeded: run sc-admin seed"))?;
    if let Some(key) = crc.as_u64() {
        CLASSES.lock().unwrap().insert(key, class.clone());
    }
    Ok(class)
}

/// The classes read so far, by CRC: they change only when the classes are seeded again (`forget_classes`), and an equip
/// would otherwise spend a database round trip on each.
static CLASSES: std::sync::LazyLock<std::sync::Mutex<HashMap<u64, Value>>> = std::sync::LazyLock::new(Default::default);

/// Drops the classes kept by `class_of`, after the classes were seeded.
pub(crate) fn forget_classes() {
    CLASSES.lock().unwrap().clear();
}

pub async fn equip_item(db: &Arango, request: AdminEquipItemRequest) -> Result<AdminEquipItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    if request.geid == 0 || request.parent_geid == 0 || request.port.is_empty() || request.port.len() > 100 {
        return Err(invalid("an item, a worn item and a port are needed"));
    }
    let owner = request.owner_id;
    // The three reads do not depend on each other: one round trip's time, not three.
    let (done, items, rows) = tokio::try_join!(
        recorded(db, &request.command_id),
        worn(db, owner),
        async {
            db.aql("FOR e IN entities FILTER e._key == @key RETURN e", json!({ "key": request.geid.to_string() })).await.map_err(internal)
        },
    )?;
    if done.is_some() {
        return Ok(AdminEquipItemResponse { replayed: true, attached: attached_items(db, request.geid).await? });
    }
    let parent = items
        .iter()
        .find(|item| item.geid == request.parent_geid)
        .ok_or_else(|| Status::failed_precondition("the parent is not worn by the player"))?;
    if items.iter().any(|item| item.geid == request.geid) {
        return Err(Status::failed_precondition("the item is worn already"));
    }
    let item = rows.into_iter().next().ok_or_else(|| Status::not_found(format!("no entity {}", request.geid)))?;
    if item["ownerId"].as_str() != Some(owner.to_string().as_str()) {
        return Err(Status::permission_denied("the item belongs to someone else"));
    }
    if item["stowed"].as_bool() != Some(true) {
        return Err(Status::failed_precondition("the item is not stowed in an inventory"));
    }
    if item["stackSize"].as_u64().unwrap_or(1) > 1 {
        return Err(Status::failed_precondition("a stack cannot be equipped: move one out first"));
    }
    let class = class_of(db, item["classGuidCrc"].clone()).await?;
    let port = port_named(&parent.class, &request.port)
        .filter(|port| takes_item(port))
        .ok_or_else(|| Status::failed_precondition(format!("{} has no port {} to equip to", parent.label(), request.port)))?;
    fits(port, &class).map_err(Status::failed_precondition)?;
    let crc = port_crc(text(port, "name"));
    if let Some(occupant) = items.iter().find(|other| other.parent == parent.geid && other.port_crc == crc) {
        return Err(Status::failed_precondition(format!("the port is taken by {}: take it off first", occupant.label())));
    }
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_entity(request.geid, owner);
    // The port is still free and the item still stowed when the transaction runs.
    plan.check(
        "RETURN ASSERT(LENGTH(FOR x IN entity_edges FILTER x.type == @attached AND x.endId == @parent AND x.attachmentPortCrc == @port
                              LIMIT 1 RETURN 1) == 0, 'precondition: the port is taken')",
        json!({ "attached": EntityEdgeType::AttachedTo as i32, "parent": parent.geid.to_string(), "port": crc }),
    );
    plan.check(
        "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true LIMIT 1 RETURN 1) == 1,
                       'precondition: the item is not stowed in an inventory')",
        json!({ "key": request.geid.to_string() }),
    );
    plan.equip(request.geid, parent.geid, crc);
    let (_, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "geid": request.geid.to_string() })).await?;
    info!(actor = %request.actor, reason = %request.reason, geid = request.geid, parent = parent.geid, port = %request.port, replayed, "admin equipped an item");
    Ok(AdminEquipItemResponse { replayed, attached: attached_items(db, request.geid).await? })
}

/// What hangs from the equipped item `geid`, parents before children, for the DGS to bind with it.
async fn attached_items(db: &Arango, geid: u64) -> Result<Vec<EquippedItem>, Status> {
    let placed = crate::operations::attached_below(db, geid).await?;
    Ok(placed.into_iter().map(|item| EquippedItem { geid: item.geid, class_crc: item.class_crc, port_crc: item.port_crc, parent_geid: item.parent }).collect())
}

/// One stowed item of an inventory by GEID, or by class (an item that is not a stack first, then the lowest GEID): a
/// lookup through the inventory's edges, not a read of the player's whole tree. An item lying in the world is none.
pub async fn find_stowed(db: &Arango, request: FindStowedItemRequest) -> Result<FindStowedItemResponse, Status> {
    need_owner(request.owner_id)?;
    need_inventory(&request.inventory_id, "inventory_id")?;
    let query = "
        LET inv = FIRST(FOR i IN inventories FILTER i._key == @inventory RETURN i._id)
        FOR x IN entity_edges FILTER x._to == inv AND x.type == @stowedIn
            LET e = DOCUMENT(x._from)
            FILTER e != null AND e.ownerId == @owner AND (e.stowed == true OR NOT_NULL(e.shardId, '') == '')
            FILTER (@geid != '' AND e._key == @geid) OR (@geid == '' AND e.classGuidCrc == @class)
            SORT (@single AND NOT_NULL(e.stackSize, 1) > 1), LENGTH(e._key), e._key
            LIMIT 1
            RETURN { geid: e._key, class: e.classGuidCrc, stack: e.stackSize }";
    let bind = json!({
        "inventory": doc::key(&request.inventory_id),
        "owner": request.owner_id.to_string(),
        "stowedIn": EntityEdgeType::StowedIn as i32,
        "geid": if request.geid == 0 { String::new() } else { request.geid.to_string() },
        "class": request.class_guid_crc,
        "single": request.prefer_single,
    });
    let rows = db.aql(query, bind).await.map_err(internal)?;
    Ok(match rows.first() {
        Some(row) => FindStowedItemResponse {
            found: true,
            geid: row["geid"].as_str().and_then(|geid| geid.parse().ok()).unwrap_or(0),
            class_guid_crc: row["class"].as_u64().unwrap_or(0) as u32,
            stack_size: row["stack"].as_u64().unwrap_or(0) as u32,
        },
        None => FindStowedItemResponse::default(),
    })
}

pub async fn unequip_item(db: &Arango, request: AdminUnequipItemRequest) -> Result<AdminUnequipItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    need_inventory(&request.to_inventory_id, "to_inventory_id")?;
    if request.geid == 0 {
        return Err(invalid("an item is needed"));
    }
    let owner = request.owner_id;
    let (done, items) = tokio::try_join!(recorded(db, &request.command_id), worn(db, owner))?;
    if done.is_some() {
        return Ok(AdminUnequipItemResponse { replayed: true });
    }
    let item = items.iter().find(|item| item.geid == request.geid).ok_or_else(|| Status::failed_precondition("the item is not worn"))?;
    // Only what equipping can put on: not the body or the head, which are the character.
    if !removable(&items, item) {
        return Err(Status::failed_precondition("that item is part of the character and cannot be taken off"));
    }
    let to = request.to_inventory_id.as_str();
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_inventory(to, owner);
    plan.unequip(request.geid, to);
    let (_, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "geid": request.geid.to_string() })).await?;
    info!(actor = %request.actor, reason = %request.reason, geid = request.geid, to, replayed, "admin took an item off");
    Ok(AdminUnequipItemResponse { replayed })
}

/// A part of the name of the ports that hold an item for later (`helmethook_attach`): the item is carried there, not worn.
/// Assumed from the names of the ports in 4.10.2.
pub const HOOK_PORT_MARKER: &str = "hook";

fn on_hook(port: &str) -> bool {
    port.to_ascii_lowercase().contains(HOOK_PORT_MARKER)
}

/// Where a worn item goes when the player toggles it (the helmet on its hook goes on the head, and back): the free port, of
/// an item that does not hang from it, that takes it and is of the other kind than its own (a hook, or not), on the item
/// closest to the player, then by name. `None` when there is none.
pub(crate) fn relocation_target(items: &[Worn], item: &Worn) -> Option<(u64, String)> {
    let hooked = on_hook(&port_label(items, item));
    let hanging: HashSet<u64> = crate::operations::below(items, item.geid).iter().map(|part| part.geid).collect();
    items
        .iter()
        .filter(|parent| !hanging.contains(&parent.geid))
        .flat_map(|parent| {
            let ports = parent.class["ports"].as_array().map(Vec::as_slice).unwrap_or_default();
            ports.iter().filter(|port| equippable(port)).map(move |port| (parent, port))
        })
        .filter(|(parent, port)| {
            let name = text(port, "name");
            let crc = port_crc(name);
            on_hook(name) != hooked && fits(port, &item.class).is_ok() && !items.iter().any(|other| other.parent == parent.geid && other.port_crc == crc)
        })
        .map(|(parent, port)| (parent.depth, text(port, "name").to_string(), parent.geid))
        .min()
        .map(|(_, name, parent)| (parent, name))
}

/// The worn item `geid` goes to the port `relocation_target` finds, with what hangs from it.
pub async fn relocate_item(db: &Arango, request: AdminRelocateItemRequest) -> Result<AdminRelocateItemResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    if request.geid == 0 {
        return Err(invalid("an item is needed"));
    }
    let owner = request.owner_id;
    let (done, items) = tokio::try_join!(recorded(db, &request.command_id), worn(db, owner))?;
    let item = items.iter().find(|item| item.geid == request.geid).ok_or_else(|| Status::failed_precondition("the item is not worn"))?;
    if done.is_some() {
        return Ok(AdminRelocateItemResponse { replayed: true, parent_geid: item.parent, port_crc: item.port_crc, port: port_label(&items, item) });
    }
    let held_in_a_hand = items
        .iter()
        .find(|parent| parent.geid == item.parent)
        .and_then(|parent| parent.class["ports"].as_array())
        .is_some_and(|ports| ports.iter().any(|port| port_crc(text(port, "name")) == item.port_crc && !equippable(port)));
    if !removable(&items, item) || held_in_a_hand {
        return Err(Status::failed_precondition("that item is not worn in a port that can change"));
    }
    let (parent, port) = relocation_target(&items, item).ok_or_else(|| Status::failed_precondition("no other free port takes that item"))?;
    let crc = port_crc(&port);
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_entity(request.geid, owner);
    plan.check(
        "RETURN ASSERT(LENGTH(FOR x IN entity_edges FILTER x.type == @attached AND x.endId == @parent AND x.attachmentPortCrc == @port
                              LIMIT 1 RETURN 1) == 0, 'precondition: the port is taken')",
        json!({ "attached": EntityEdgeType::AttachedTo as i32, "parent": parent.to_string(), "port": crc }),
    );
    plan.reattach(request.geid, parent, crc);
    let (_, replayed) = apply_once(db, &request.command_id, owner, plan, json!({ "geid": request.geid.to_string() })).await?;
    info!(actor = %request.actor, reason = %request.reason, geid = request.geid, parent, port = %port, replayed, "a worn item moved to another port");
    Ok(AdminRelocateItemResponse { replayed, parent_geid: parent, port_crc: crc, port })
}

/// What the player wears that `known` (the loadout's own items) does not account for, parents before children, and which of
/// `known` are no longer worn (0 in their place).
pub async fn loadout_state(db: &Arango, player: u64, known: &[u64]) -> Result<(Vec<u64>, Vec<EquippedItem>), Status> {
    let items = worn(db, player).await?;
    let attached: HashSet<u64> = items.iter().map(|item| item.geid).collect();
    let geids = known.iter().map(|geid| if attached.contains(geid) { *geid } else { 0 }).collect();
    let in_loadout: HashSet<u64> = known.iter().copied().collect();
    let by_geid: HashMap<u64, &Worn> = items.iter().map(|item| (item.geid, item)).collect();
    let equipped = items
        .iter()
        .filter(|item| !in_loadout.contains(&item.geid) && by_geid.contains_key(&item.parent))
        .map(|item| EquippedItem {
            geid: item.geid,
            class_crc: item.class["guidHashCrc"].as_u64().unwrap_or(0) as u32,
            port_crc: item.port_crc,
            parent_geid: item.parent,
        })
        .collect();
    Ok((geids, equipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, min: i64, max: i64, types: Value, required: &str, offered: &str) -> Value {
        json!({ "name": name, "minSize": min, "maxSize": max, "types": types, "requiredPortTags": required, "portTags": offered, "flags": "" })
    }

    fn class(kind: &str, sub: &str, size: i64, tags: &str) -> Value {
        json!({ "itemType": kind, "itemSubType": sub, "attachSize": size, "attachTags": tags, "requiredTags": "" })
    }

    fn worn_item(geid: u64, parent: u64, port: &str, depth: usize, ports: Value, class: Value) -> Worn {
        let mut class = class;
        class["ports"] = ports;
        Worn { geid, class, parent, port_crc: port_crc(port), depth }
    }

    fn open_port(name: &str) -> Value {
        port(name, 0, 9, json!([{ "itemType": "Char_Armor_Helmet", "itemSubTypes": [] }]), "", "")
    }

    #[test]
    fn a_helmet_on_its_hook_goes_to_the_head_port_and_back() {
        let suit = worn_item(2, 1, "Armor_Undersuit", 1, json!([open_port("Armor_Helmet")]), class("Char_Armor_Undersuit", "", 0, ""));
        let armor = worn_item(3, 1, "Armor_Core", 1, json!([open_port("helmethook_attach")]), class("Char_Armor_Torso", "", 0, ""));
        let helmet = |parent: u64, port: &str| worn_item(4, parent, port, 2, json!([]), class("Char_Armor_Helmet", "", 0, ""));
        let on_hook_now = vec![suit.clone(), armor.clone(), helmet(3, "helmethook_attach")];
        assert_eq!(relocation_target(&on_hook_now, &on_hook_now[2]), Some((2, "Armor_Helmet".to_string())));
        let on_head = vec![suit.clone(), armor.clone(), helmet(2, "Armor_Helmet")];
        assert_eq!(relocation_target(&on_head, &on_head[2]), Some((3, "helmethook_attach".to_string())));
        // A taken port is no target.
        let taken = vec![suit.clone(), armor.clone(), helmet(3, "helmethook_attach"), worn_item(9, 2, "Armor_Helmet", 2, json!([]), class("Char_Armor_Helmet", "", 0, ""))];
        assert_eq!(relocation_target(&taken, &taken[2]), None);
    }

    #[test]
    fn port_names_hash_as_the_client_keys_them() {
        // Read live from a client in the loadout notes: the player's one port.
        assert_eq!(port_crc("Body_ItemPort"), 0xab63_41ac);
        assert_eq!(port_crc("BODY_ITEMPORT"), port_crc("body_itemport"));
    }

    #[test]
    fn a_port_takes_the_types_and_sizes_the_game_lists() {
        let sidearm = port("wep_sidearm", 1, 1, json!([{ "itemType": "WeaponPersonal", "itemSubTypes": ["Small"] }]), "", "");
        let stocked = port("wep_stocked_2", 2, 4, json!([{ "itemType": "WeaponPersonal", "itemSubTypes": ["Medium"] }]), "", "");
        let pistol = class("WeaponPersonal", "Small", 1, "pistol");
        let rifle = class("WeaponPersonal", "Medium", 2, "stocked rifle");
        assert!(fits(&sidearm, &pistol).is_ok() && fits(&stocked, &rifle).is_ok());
        assert!(fits(&sidearm, &rifle).unwrap_err().contains("does not go in"), "a rifle is not a sidearm");
        assert!(fits(&stocked, &pistol).is_err());
        let big = class("WeaponPersonal", "Medium", 5, "");
        assert!(fits(&stocked, &big).unwrap_err().contains("size 5"));
        let without_size = json!({ "itemType": "WeaponPersonal", "itemSubType": "Medium" });
        assert!(fits(&stocked, &without_size).is_ok(), "no size seeded, none checked");
    }

    #[test]
    fn a_hand_carries_whatever_it_is_given() {
        let hand = json!({ "name": "weapon_attach_hand_left", "flags": "selfphys select usegripdata", "minSize": 0, "maxSize": 0 });
        let crate_class = class("Container", "Cargo", 3, "");
        assert!(fits(&hand, &crate_class).is_ok(), "no types, no size: the hand takes it");
        let listed = json!({ "name": "weapon_attach_hand_left", "types": [{ "itemType": "WeaponPersonal", "itemSubTypes": [] }] });
        assert!(fits(&listed, &crate_class).is_err(), "types the game lists still count");
    }

    #[test]
    fn a_whole_type_port_takes_every_sub_type() {
        let torso = port("Armor_Torso", 1, 1, json!([{ "itemType": "Char_Armor_Torso", "itemSubTypes": [] }]), "", "");
        assert!(fits(&torso, &class("Char_Armor_Torso", "Heavy", 1, "")).is_ok());
        assert!(fits(&torso, &class("Char_Armor_Arms", "Heavy", 1, "")).is_err());
    }

    #[test]
    fn tags_are_matched_both_ways() {
        let backpack = port("backpack", 1, 2, json!([{ "itemType": "Char_Armor_Backpack", "itemSubTypes": [] }]), "$backpack", "utility combat");
        assert!(fits(&backpack, &class("Char_Armor_Backpack", "Personal", 1, "rsi Set_01 backpack")).is_ok());
        assert!(fits(&backpack, &class("Char_Armor_Backpack", "Personal", 1, "rsi Set_01")).unwrap_err().contains("backpack"));
        let mut needy = class("Char_Armor_Backpack", "Personal", 1, "backpack");
        needy["requiredTags"] = json!("heavy");
        assert!(fits(&backpack, &needy).unwrap_err().contains("heavy"));
        needy["requiredTags"] = json!("Combat");
        assert!(fits(&backpack, &needy).is_ok(), "tags ignore case");
    }

    #[test]
    fn the_character_and_the_pockets_are_not_equipment() {
        let flagged = |name: &str, flags: &str| json!({ "name": name, "flags": flags });
        assert!(equippable(&flagged("Armor_Torso", "body_attach nontransferable inventory")));
        assert!(!equippable(&flagged("Head_ItemPort", "nontransferable")));
        assert!(!equippable(&flagged("weapon_attach_hand_right", "selfphys")));
        assert!(takes_item(&flagged("weapon_attach_hand_right", "selfphys")), "but a hand carries");
        assert!(!takes_item(&flagged("inventory_pocket", "body_attach inventoryinternalonly")));
        assert!(!equippable(&flagged("inventory_pocket", "body_attach inventoryinternalonly")));
        assert!(!equippable(&flagged("restraint_attach", "restraint_attach")));
    }
}
