//! `EnsureLoadout` of `EntityGraphInternal`: the items a player wears as entities of their own. A DGS asks for them
//! when a player joins, and binds the GEIDs it gets. The command id is the record of it, as for the admin RPCs, so a
//! repeat answers with the GEIDs of the first and creates nothing.

use std::collections::{HashMap, HashSet};

use sc_api::internal::{EnsureLoadoutRequest, EnsureLoadoutResponse, LoadoutItem};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::{
    create_command, entity_edge_vertex::Id, entity_graph_command::Params, node_properties, AttachmentType, CreateCommand,
    EntityEdge, EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityGraph, EntityNodeProperties, Node,
    NodeProperties, NodeType,
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

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
        return Ok(EnsureLoadoutResponse { geids: geids_of(&found).unwrap_or_default(), replayed: true });
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
    Ok(EnsureLoadoutResponse { geids: geids_of(&result).unwrap_or_default(), replayed })
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
