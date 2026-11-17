//! Every personal weapon comes with a magazine of its own class in its `magazine_attach` port, as the game's manual
//! loadout of the weapon spawns it. The seed gives each weapon class its magazine (`defaultMagazineGuid`); this adds one
//! to each weapon of a player that has none. It runs wherever a service gives a player a weapon (a loadout, a grant, an
//! admin addition) and again at each join, which is what repairs the weapons made before it existed. sc-entitygraph owns
//! the items, so no other service writes them.

use std::collections::HashMap;

use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::{
    create_command, entity_edge_vertex::Id, entity_graph_command::Params, node_properties, AttachmentType, CreateCommand, EntityEdge,
    EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityGraph, EntityNodeProperties, Node, NodeProperties, NodeType,
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    admin::{allocate_geids, apply_once, command, refuse_taken},
    commands::Plan,
    now_ms,
};

/// The key of the port a weapon's magazine sits in: CRC-32C of the lowercased `magazine_attach`.
pub const MAGAZINE_PORT_CRC: u32 = 0x5276_3c6e;
/// The most weapons one pass completes, so a player with a huge armory is finished over several joins.
const MAX_PER_PASS: usize = 200;
/// `debug.createdBy` of a magazine whose weapon has none. (The parts of a ship are left out of the pass: their loadout is the ship's.)
const CREATED_BY: &str = "magazines";

/// A weapon with no magazine, and what its magazine is.
#[derive(Clone, Debug, PartialEq)]
pub struct Missing {
    pub weapon: u64,
    pub magazine_crc: u32,
    pub item_type_enum: i32,
    pub item_sub_type_enum: i32,
    /// How the weapon was made, which its magazine shares so that it goes when the weapon does.
    pub created_by: String,
}

fn vertex(geid: u64) -> EntityEdgeVertex {
    EntityEdgeVertex { r#type: NodeType::Entity as i32, id: Some(Id::EntityId(geid)) }
}

/// The magazines to create, one per `missing` weapon, with the GEIDs from `first` on: each a node and an `ATTACHED_TO`
/// edge to the weapon's magazine port.
pub fn graph(owner: u64, missing: &[Missing], first: u64) -> EntityGraph {
    let mut graph = EntityGraph::default();
    for (index, weapon) in missing.iter().enumerate() {
        let geid = first + index as u64;
        let properties = EntityNodeProperties {
            geid,
            class_guid_crc: weapon.magazine_crc,
            stack_size: 1,
            owner_id: owner,
            scale: 1.0,
            item_type_enum: weapon.item_type_enum,
            item_sub_type_enum: weapon.item_sub_type_enum,
            debug: HashMap::from([("createdBy".to_string(), weapon.created_by.clone())]),
            ..Default::default()
        };
        graph.nodes.push(Node {
            properties: Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(properties)) }),
            r#type: NodeType::Entity as i32,
        });
        graph.edges.push(EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(vertex(geid)),
            end: Some(vertex(weapon.weapon)),
            properties: Some(EntityEdgeProperties {
                attachment_type: AttachmentType::ItemPort as i32,
                attachment_port_crc: MAGAZINE_PORT_CRC,
                ..Default::default()
            }),
        });
    }
    graph
}

/// The weapons of `owner` whose class has a default magazine and that hold none in the magazine port.
fn missing_of(rows: &[Value]) -> Vec<Missing> {
    rows.iter()
        .filter_map(|row| {
            Some(Missing {
                weapon: row["weapon"].as_str()?.parse().ok()?,
                magazine_crc: u32::try_from(row["magazineCrc"].as_u64()?).ok()?,
                item_type_enum: row["itemTypeEnum"].as_i64().unwrap_or(0) as i32,
                item_sub_type_enum: row["itemSubTypeEnum"].as_i64().unwrap_or(0) as i32,
                created_by: row["createdBy"].as_str().filter(|by| !by.is_empty()).unwrap_or(CREATED_BY).to_string(),
            })
        })
        .collect()
}

/// Gives each weapon of `owner` that has no magazine its class's default one. Returns how many it made.
pub async fn complete(db: &Arango, owner: u64) -> Result<usize, Status> {
    // Every set is asked once and joined here: a subquery that names each weapon runs once per weapon, and in a cluster each run is
    // a round trip (3.2 s at a join, which waits 3 s for the loadout).
    let query = "LET weapons = (FOR w IN entities FILTER w.ownerId IN [TO_STRING(@owner), @owner] FILTER w.debug.createdBy != @ship_part RETURN w)
                 LET classes = (FOR c IN entity_classes FILTER c.guidHashCrc IN weapons[*].classGuidCrc
                                    FILTER c.defaultMagazineGuid != null AND c.defaultMagazineGuid != '' RETURN c)
                 LET held = (FOR x IN entity_edges FILTER x.endId IN weapons[*]._key AND x.type == @attached
                                 AND x.attachmentPortCrc == @port RETURN x.endId)
                 LET magazines = (FOR m IN entity_classes FILTER m._key IN classes[*].defaultMagazineGuid RETURN m)
                 FOR w IN weapons
                     LET class = FIRST(FOR c IN classes FILTER c.guidHashCrc == w.classGuidCrc RETURN c)
                     FILTER class != null
                     FILTER w._key NOT IN held
                     LET magazine = FIRST(FOR m IN magazines FILTER m._key == class.defaultMagazineGuid RETURN m)
                     FILTER magazine != null
                     SORT w._key
                     LIMIT @limit
                     RETURN { weapon: w._key, magazineCrc: magazine.guidHashCrc, itemTypeEnum: magazine.itemTypeEnum,
                              itemSubTypeEnum: magazine.itemSubTypeEnum, createdBy: w.debug.createdBy }";
    let bind = json!({ "owner": owner, "attached": EntityEdgeType::AttachedTo as i32, "port": MAGAZINE_PORT_CRC, "limit": MAX_PER_PASS, "ship_part": crate::ship_parts::CREATED_BY });
    let rows = db.aql(query, bind).await.map_err(internal)?;
    let missing = missing_of(&rows);
    if missing.is_empty() {
        return Ok(0);
    }
    let first = allocate_geids(db, missing.len() as u64).await?;
    let mut plan = Plan::new(None, now_ms());
    for index in 0..missing.len() as u64 {
        refuse_taken(&mut plan, first + index);
    }
    let body = create_command::Body { graphs: Some(graph(owner, &missing, first)) };
    plan.command(&command(Params::Create(CreateCommand { creates: vec![body], inventory_id: String::new() })))?;
    let weapons: Vec<String> = missing.iter().map(|weapon| weapon.weapon.to_string()).collect();
    let command_id = format!("magazines:{owner}:{}", weapons.join(","));
    let result = json!({ "made": missing.len() });
    apply_once(db, &command_id, owner, plan, result).await?;
    info!(owner, weapons = missing.len(), "weapons given their magazines");
    Ok(missing.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_is_the_crc_of_its_name() {
        let crc = !"magazine_attach".bytes().fold(!0u32, |crc, byte| (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 }));
        assert_eq!(crc, MAGAZINE_PORT_CRC);
    }

    #[test]
    fn a_weapon_without_a_magazine_gets_one_in_the_magazine_port() {
        let rows = vec![
            json!({ "weapon": "400000000003", "magazineCrc": 3830248767u64, "itemTypeEnum": 7, "itemSubTypeEnum": 2, "createdBy": "loadout" }),
            json!({ "weapon": "400000000009", "magazineCrc": 11, "createdBy": null }),
            json!({ "weapon": "not a geid", "magazineCrc": 11 }),
        ];
        let missing = missing_of(&rows);
        assert_eq!(missing.len(), 2, "a row that is no weapon is left out");
        let made = graph(300_000_000_000, &missing, 500);
        assert_eq!(made.nodes.len(), 2);
        let edge = made.edges[0].properties.as_ref().unwrap();
        assert_eq!((edge.attachment_port_crc, edge.attachment_type), (MAGAZINE_PORT_CRC, AttachmentType::ItemPort as i32));
        assert_eq!(made.edges[1].start.as_ref().unwrap().id, Some(Id::EntityId(501)));
        assert_eq!(made.edges[1].end.as_ref().unwrap().id, Some(Id::EntityId(400_000_000_009)));
        let node = |index: usize| match made.nodes[index].properties.as_ref().unwrap().properties.as_ref().unwrap() {
            node_properties::Properties::EntityProperties(properties) => properties.clone(),
            _ => panic!("an entity"),
        };
        assert_eq!((node(0).class_guid_crc, node(0).debug["createdBy"].as_str()), (3_830_248_767, "loadout"));
        assert_eq!(node(1).debug["createdBy"], CREATED_BY, "a weapon of no origin gives its magazine ours");
    }
}
