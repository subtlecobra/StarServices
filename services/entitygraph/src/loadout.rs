//! `EnsureLoadout` of `EntityGraphInternal`: the items a player wears as entities of their own. A DGS asks for them
//! when a player joins, and binds the GEIDs it gets. The command id is the record of it, as for the admin RPCs, so a
//! repeat answers with the GEIDs of the first and creates nothing.

use std::collections::{HashMap, HashSet};

use sc_api::internal::{EnsureLoadoutRequest, EnsureLoadoutResponse, LoadoutItem};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::{
    create_command, entity_edge_vertex::Id, entity_graph_command::Params, erase_command, node_properties, AttachmentType, CreateCommand, EraseCommand,
    EntityEdge, EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityGraph, EntityNodeProperties, Node,
    NodeProperties, NodeType,
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::{info, warn};

use crate::{
    admin::{allocate_geids, apply_once, command, invalid, need_command, need_owner, recorded, refuse_taken},
    commands::Plan,
    now_ms,
};

const MAX_ITEMS: usize = 200;

fn vertex(geid: u64) -> EntityEdgeVertex {
    EntityEdgeVertex { r#type: NodeType::Entity as i32, id: Some(Id::EntityId(geid)) }
}

/// The request's shape: parents come before their children, so no item is its own ancestor.
fn check(items: &[LoadoutItem]) -> Result<(), Status> {
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Err(invalid("a loadout of 1 to 200 items is needed"));
    }
    for (index, item) in items.iter().enumerate() {
        if item.parent < -1 || item.parent >= index as i32 {
            return Err(Status::invalid_argument(format!("item {index}: its parent must be -1 or an item before it")));
        }
    }
    Ok(())
}

/// The creation of the items with their GEIDs: each node, and an `ATTACHED_TO` edge from it to its parent's port.
fn graph(player: u64, items: &[LoadoutItem], geids: &[u64], classes: &HashMap<u32, Value>) -> EntityGraph {
    let mut graph = EntityGraph::default();
    for (index, item) in items.iter().enumerate() {
        let class = &classes[&item.class_crc];
        let properties = EntityNodeProperties {
            geid: geids[index],
            class_guid_crc: item.class_crc,
            stack_size: 1,
            owner_id: player,
            scale: 1.0,
            item_type_enum: class["itemTypeEnum"].as_i64().unwrap_or(0) as i32,
            item_sub_type_enum: class["itemSubTypeEnum"].as_i64().unwrap_or(0) as i32,
            debug: HashMap::from([("createdBy".to_string(), "loadout".to_string())]),
            ..Default::default()
        };
        graph.nodes.push(Node {
            properties: Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(properties)) }),
            r#type: NodeType::Entity as i32,
        });
        let parent = if item.parent < 0 { player } else { geids[item.parent as usize] };
        graph.edges.push(EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(vertex(geids[index])),
            end: Some(vertex(parent)),
            properties: Some(EntityEdgeProperties {
                attachment_type: AttachmentType::ItemPort as i32,
                attachment_port_crc: item.port_crc,
                ..Default::default()
            }),
        });
    }
    graph
}

pub async fn ensure(db: &Arango, request: EnsureLoadoutRequest) -> Result<EnsureLoadoutResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.player_id)?;
    check(&request.items)?;
    let geids_of = |found: &Value| found["geids"].as_array().map(|list| list.iter().filter_map(|geid| geid.as_str()?.parse().ok()).collect::<Vec<u64>>());
    if let Some(found) = recorded(db, &request.command_id).await? {
        let made = geids_of(&found).unwrap_or_default();
        let (geids, equipped) = worn_now(db, request.player_id, made).await?;
        return Ok(EnsureLoadoutResponse { geids, replayed: true, equipped });
    }
    let wanted: Vec<u32> = request.items.iter().map(|item| item.class_crc).collect::<HashSet<_>>().into_iter().collect();
    let rows = db
        .aql("FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs RETURN c", json!({ "crcs": wanted }))
        .await
        .map_err(internal)?;
    let classes: HashMap<u32, Value> = rows.into_iter().filter_map(|row| Some((row["guidHashCrc"].as_u64()? as u32, row))).collect();
    let mut missing: Vec<u32> = wanted.iter().copied().filter(|crc| !classes.contains_key(crc)).collect();
    if !missing.is_empty() {
        missing.sort_unstable();
        let names: Vec<String> = missing.iter().map(|crc| format!("{crc:#010x}")).collect();
        return Err(Status::not_found(format!("no entity class {}: run sc-admin seed", names.join(", "))));
    }
    let first = allocate_geids(db, request.items.len() as u64).await?;
    let geids: Vec<u64> = (first..first + request.items.len() as u64).collect();
    let mut plan = Plan::new(None, now_ms());
    for geid in &geids {
        refuse_taken(&mut plan, *geid);
    }
    let body = create_command::Body { graphs: Some(graph(request.player_id, &request.items, &geids, &classes)) };
    plan.command(&command(Params::Create(CreateCommand { creates: vec![body], inventory_id: String::new() })))?;
    let result = json!({ "geids": geids.iter().map(u64::to_string).collect::<Vec<_>>() });
    let (result, replayed) = apply_once(db, &request.command_id, request.player_id, plan, result).await?;
    info!(player = request.player_id, items = geids.len(), replayed, "loadout ensured");
    if !replayed {
        retire_old(db, request.player_id, &geids, &request.command_id).await;
    }
    let (geids, equipped) = worn_now(db, request.player_id, geids_of(&result).unwrap_or_default()).await?;
    Ok(EnsureLoadoutResponse { geids, replayed, equipped })
}

/// The loadout's items as they are worn now (0 for one taken off), and what else is worn on them.
async fn worn_now(db: &Arango, player: u64, made: Vec<u64>) -> Result<(Vec<u64>, Vec<sc_api::internal::EquippedItem>), Status> {
    crate::equip::loadout_state(db, player, &made).await
}

/// Erases the loadouts the player wore before this one: the items `EnsureLoadout` made for them that are still attached to
/// the player, with what hangs from them. An item the player took off and stowed somewhere is theirs and stays. A failure
/// is logged and leaves the old items for the next change.
async fn retire_old(db: &Arango, player: u64, current: &[u64], command_id: &str) {
    let query = "FOR e IN entities
                     FILTER TO_STRING(e.ownerId) == TO_STRING(@player) AND e.debug.createdBy == 'loadout'
                         AND e.stowed != true AND e._key NOT IN @current
                     FOR x IN entity_edges FILTER x.startId == e._key AND x.type == @attached AND x.endId == TO_STRING(@player)
                     RETURN e._key";
    let bind = json!({
        "player": player,
        "current": current.iter().map(u64::to_string).collect::<Vec<_>>(),
        "attached": EntityEdgeType::AttachedTo as i32,
    });
    let old: Vec<u64> = match db.aql(query, bind).await {
        Ok(rows) => rows.iter().filter_map(|row| row.as_str()?.parse().ok()).collect(),
        Err(error) => {
            warn!(player, %error, "the old loadout was not looked up");
            return;
        }
    };
    if old.is_empty() {
        return;
    }
    let mut plan = Plan::new(None, now_ms());
    // What the player equipped on the old items is theirs: it goes to their personal inventory, not with the old loadout.
    let personal = format!("{player}:PlayerInventory:0");
    for geid in equipped_on(db, &old).await {
        plan.unequip(geid, &personal);
    }
    let erasures = old.iter().map(|geid| erase_command::Body { entity_id: *geid, preconditions: Vec::new() }).collect();
    if let Err(status) = plan.command(&command(Params::Erase(EraseCommand { erasures, inventory_id: String::new() }))) {
        warn!(player, %status, "the old loadout was not erased");
        return;
    }
    match apply_once(db, &format!("{command_id}:retire"), player, plan, json!({ "retired": old.len() })).await {
        Ok(_) => info!(player, roots = old.len(), "old loadout erased"),
        Err(status) => warn!(player, %status, "the old loadout was not erased"),
    }
}

/// The items attached below `roots` that no loadout made (equipped by the player or an admin) and that hang directly from
/// an item a loadout made, which are the ones to rescue: what hangs from them goes with them.
async fn equipped_on(db: &Arango, roots: &[u64]) -> Vec<u64> {
    let query = "WITH entities, inventories
                 FOR r IN @roots
                     FOR v, x IN 1..8 INBOUND CONCAT('entities/', r) entity_edges
                         PRUNE IS_SAME_COLLECTION('inventories', v)
                         OPTIONS { order: 'bfs', uniqueVertices: 'global' }
                         FILTER x.type == @attached AND IS_SAME_COLLECTION('entities', v) AND v.debug.createdBy != 'loadout'
                         LET parent = DOCUMENT('entities', x.endId)
                         FILTER parent.debug.createdBy == 'loadout'
                         RETURN v._key";
    let bind = json!({ "roots": old_keys(roots), "attached": EntityEdgeType::AttachedTo as i32 });
    match db.aql(query, bind).await {
        Ok(rows) => rows.iter().filter_map(|row| row.as_str()?.parse().ok()).collect(),
        Err(error) => {
            warn!(%error, "what was equipped on the old loadout was not looked up");
            Vec::new()
        }
    }
}

fn old_keys(roots: &[u64]) -> Vec<String> {
    roots.iter().map(u64::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(parent: i32) -> LoadoutItem {
        LoadoutItem { class_crc: 1, port_crc: 2, parent }
    }

    #[test]
    fn parents_come_before_children() {
        assert!(check(&[item(-1), item(0), item(0), item(2)]).is_ok());
        assert!(check(&[item(0)]).is_err(), "its own parent");
        assert!(check(&[item(-1), item(2), item(0)]).is_err(), "a parent after it");
        assert!(check(&[item(-2)]).is_err());
        assert!(check(&[]).is_err());
    }

    #[test]
    fn items_attach_to_their_parents_ports() {
        let items = [item(-1), item(0)];
        let classes = HashMap::from([(1u32, json!({ "itemTypeEnum": 3 }))]);
        let graph = graph(300_000_000_001, &items, &[400_000_000_000, 400_000_000_001], &classes);
        assert_eq!(graph.nodes.len(), 2);
        let ends: Vec<Option<Id>> = graph.edges.iter().map(|edge| edge.end.as_ref().and_then(|end| end.id.clone())).collect();
        assert_eq!(ends, [Some(Id::EntityId(300_000_000_001)), Some(Id::EntityId(400_000_000_000))]);
        assert!(graph.edges.iter().all(|edge| edge.properties.as_ref().is_some_and(|p| p.attachment_port_crc == 2)));
    }
}
