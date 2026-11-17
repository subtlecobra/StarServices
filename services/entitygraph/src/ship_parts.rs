//! A ship's loadout as entities of the graph: its parts (weapons, thrusters, power plant, shields, doors, seats and what
//! sits in their ports, from `tables/ship_parts.bin`) are item entities of the ship's owner, each attached to its parent's
//! port with an `ATTACHED_TO` edge, the way a player's worn items are (`loadout`). The client's Vehicle Loadout Manager
//! asks the graph for a stowed ship's tree (`EntityQuery` on the location inventory with the tree projection, once with
//! a filter on the ship's GEID and once with an item type filter) and builds its component list from the nodes and the
//! edges' port ids; a ship with no children has an empty list (client log of 2026-11-16 01:51:16Z, docs/SHIPS.md).
//!
//! sc-entitygraph owns them: it makes the tree when a ship is acquired and for the ships that have none (at start and when
//! a player's ships are read), and answers the DGS with the parts' GEIDs when the ship is retrieved, so that the DGS binds
//! the entities the player edited. The table comes from the region's `sc-gamedata`, which owns the game files.

use std::{collections::HashMap, sync::Arc, time::Duration};

use sc_api::internal::{game_data_client::GameDataClient, GetGameFileRequest, ListGameBuildsRequest, ShipPartRef};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::{
    create_command, entity_edge_vertex::Id, entity_graph_command::Params, erase_command, node_properties, AttachmentType,
    CreateCommand, EraseCommand, EntityEdge, EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityGraph, EntityNodeProperties, Node, NodeProperties,
    NodeType,
};
use sc_ship_parts::{Part, ShipParts, SHIP_PARTS_PATH};
#[cfg(test)]
use sc_ship_parts::FLAG_DEFAULT_OF_PARENT;
use serde_json::{json, Value};
use tokio::{sync::Mutex, time::timeout};
use tonic::{transport::Endpoint, Status};
use tracing::{info, warn};

use crate::{
    admin::{allocate_geids, apply_once, command, recorded, refuse_taken},
    commands::Plan,
    now_ms,
};

/// `debug.createdBy` of a part entity, and the keys of its `debug` that say whose it is and where it sits.
pub const CREATED_BY: &str = "shipPart";
const DEBUG_CREATED_BY: &str = "createdBy";
const DEBUG_SHIP: &str = "ship";
const DEBUG_INDEX: &str = "partIndex";
/// The command id of the creation of a ship's parts: once per ship.
const COMMAND_PREFIX: &str = "ship-parts:";
/// The command id of the removal of the parts the client spawns itself from a tree made with all of them: once per ship.
const TRIM_PREFIX: &str = "ship-parts-trim:";
/// How deep parts hang below a ship (a ship's port, a mount, a weapon, its magazine and attachments).
const MAX_DEPTH: u32 = 12;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// More than `tables/ship_parts.bin` of any build is (about 1 MB for 1111 vehicles).
const MAX_FILE_BYTES: u64 = 256 << 20;
/// A failed fetch of the table is not tried again for this long.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// The most parts of one ship: its tree is made by one command.
const MAX_PARTS: usize = 2_000;

/// Where the table is: the `sc-gamedata` of the region and the game build whose file is read.
#[derive(Clone, Debug)]
pub struct Source {
    pub address: String,
    /// The build's change number; `None` takes the last build the service has ready.
    pub build: Option<String>,
}

#[derive(Default)]
struct Cached {
    table: Option<Arc<ShipParts>>,
    failed_at: Option<std::time::Instant>,
}

/// The parts of the ships of this region's game build, and the ships already known to have theirs.
#[derive(Default)]
pub struct ShipLoadouts {
    source: Option<Source>,
    cached: Mutex<Cached>,
    done: std::sync::Mutex<std::collections::HashSet<u64>>,
}

fn vertex(geid: u64) -> EntityEdgeVertex {
    EntityEdgeVertex { r#type: NodeType::Entity as i32, id: Some(Id::EntityId(geid)) }
}

/// The indexes of the parts of a table that have an entity in the graph: all but the ones the client spawns itself
/// (`Part::is_default_of_parent`; docs/SHIPS.md).
fn kept(parts: &[Part]) -> Vec<usize> {
    (0..parts.len()).filter(|index| !parts[*index].is_default_of_parent()).collect()
}

/// The creation of a ship's parts with their GEIDs: each node and an `ATTACHED_TO` edge from it to its parent's port, the
/// parent being the ship for a part of the ship's own ports. `geids` has one GEID for each part of `kept(parts)`, in order;
/// the others have no entity (the `partIndex` of the rest stays its index in the table). `classes` has each class's
/// `itemTypeEnum` and `itemSubTypeEnum` by its CRC; a class the game data seed does not know gets 0 of both.
fn graph(ship: u64, owner: u64, parts: &[Part], geids: &[u64], classes: &HashMap<u32, Value>) -> EntityGraph {
    let mut graph = EntityGraph::default();
    let mut by_index: Vec<Option<u64>> = vec![None; parts.len()];
    for (index, geid) in kept(parts).into_iter().zip(geids) {
        by_index[index] = Some(*geid);
    }
    for (index, part) in parts.iter().enumerate() {
        let Some(geid) = by_index[index] else { continue };
        let class = classes.get(&part.class_crc);
        let number = |field: &str| class.and_then(|class| class[field].as_i64()).unwrap_or(0) as i32;
        let properties = EntityNodeProperties {
            geid,
            class_guid_crc: part.class_crc,
            stack_size: 1,
            owner_id: owner,
            scale: 1.0,
            item_type_enum: number("itemTypeEnum"),
            item_sub_type_enum: number("itemSubTypeEnum"),
            debug: HashMap::from([
                (DEBUG_CREATED_BY.to_string(), CREATED_BY.to_string()),
                (DEBUG_SHIP.to_string(), ship.to_string()),
                (DEBUG_INDEX.to_string(), index.to_string()),
            ]),
            ..Default::default()
        };
        graph.nodes.push(Node {
            properties: Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(properties)) }),
            r#type: NodeType::Entity as i32,
        });
        // A part's descendants are marked like it, so its parent has an entity too.
        let parent = part.parent.map_or(Some(ship), |parent| by_index[parent]);
        let Some(parent) = parent else { continue };
        graph.edges.push(EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(vertex(geid)),
            end: Some(vertex(parent)),
            properties: Some(EntityEdgeProperties {
                attachment_type: AttachmentType::ItemPort as i32,
                attachment_port_crc: part.attach_id,
                ..Default::default()
            }),
        });
    }
    graph
}

/// The table indexes of the topmost parts the client spawns itself: the ones to erase from a tree made with all the parts
/// (their descendants go with them).
fn default_roots(parts: &[Part]) -> Vec<usize> {
    (0..parts.len())
        .filter(|index| parts[*index].is_default_of_parent() && parts[*index].parent.is_none_or(|parent| !parts[parent].is_default_of_parent()))
        .collect()
}

impl ShipLoadouts {
    /// No game data: ships are made with no parts, and the DGS binds its own ids.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn from(source: Source) -> Self {
        Self { source: Some(source), ..Default::default() }
    }

    pub fn enabled(&self) -> bool {
        self.source.is_some()
    }

    /// The table of the game build, fetched once. `None` when there is no source or the file cannot be had (tried again
    /// after `RETRY_AFTER`).
    async fn table(&self) -> Option<Arc<ShipParts>> {
        let source = self.source.as_ref()?;
        let mut cached = self.cached.lock().await;
        if let Some(table) = &cached.table {
            return Some(table.clone());
        }
        if cached.failed_at.is_some_and(|at| at.elapsed() < RETRY_AFTER) {
            return None;
        }
        match timeout(FETCH_TIMEOUT, fetch(source)).await.unwrap_or_else(|_| Err(format!("{SHIP_PARTS_PATH} not fetched in time"))) {
            Ok(table) => {
                info!(vehicles = table.ships(), "ship parts loaded from gamedata");
                let table = Arc::new(table);
                cached.table = Some(table.clone());
                Some(table)
            }
            Err(error) => {
                warn!(%error, "the ship parts are not loaded: ships are made with no loadout tree");
                cached.failed_at = Some(std::time::Instant::now());
                None
            }
        }
    }

    /// Whether the table is loaded (or there is nothing to load).
    pub async fn ready(&self) -> bool {
        !self.enabled() || self.table().await.is_some()
    }

    /// The parts of ship `geid` the graph holds, in the order of the table. Empty for a ship that has none.
    pub async fn refs(&self, db: &Arango, geid: u64) -> Result<Vec<ShipPartRef>, Status> {
        let query = format!(
            "WITH entities, inventories
             FOR v, x IN 1..{MAX_DEPTH} INBOUND @ship entity_edges
                 OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                 FILTER x.type == @attached AND v.debug.{DEBUG_CREATED_BY} == @created
                 SORT TO_NUMBER(v.debug.{DEBUG_INDEX})
                 RETURN {{ geid: v._key, crc: v.classGuidCrc, port: x.attachmentPortCrc }}"
        );
        let bind = json!({ "ship": crate::doc::entity_id(geid), "attached": EntityEdgeType::AttachedTo as i32, "created": CREATED_BY });
        let rows = db.aql(&query, bind).await.map_err(internal)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(ShipPartRef {
                    geid: row["geid"].as_str()?.parse().ok()?,
                    class_crc: row["crc"].as_u64()? as u32,
                    attach_id: row["port"].as_u64()? as u32,
                })
            })
            .collect())
    }

    /// Erases from ship `geid`'s tree the parts the table says the client spawns itself, which a tree made before that mark
    /// has: their nodes (`debug.partIndex` of a root of `default_roots`) with the subtrees and edges. Once per ship.
    async fn trim(&self, db: &Arango, owner: u64, geid: u64, parts: &[Part]) -> Result<(), Status> {
        let roots = default_roots(parts);
        if roots.is_empty() {
            return Ok(());
        }
        let query = format!(
            "WITH entities, inventories
             FOR v, x IN 1..{MAX_DEPTH} INBOUND @ship entity_edges
                 OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                 FILTER x.type == @attached AND v.debug.{DEBUG_CREATED_BY} == @created AND v.debug.{DEBUG_SHIP} == @geid
                     AND TO_NUMBER(v.debug.{DEBUG_INDEX}) IN @roots
                 RETURN v._key"
        );
        let bind = json!({
            "ship": crate::doc::entity_id(geid), "attached": EntityEdgeType::AttachedTo as i32, "created": CREATED_BY,
            "geid": geid.to_string(), "roots": roots,
        });
        let found = db.aql(&query, bind).await.map_err(internal)?;
        let erasures: Vec<erase_command::Body> = found
            .iter()
            .filter_map(|key| key.as_str()?.parse::<u64>().ok())
            .map(|entity_id| erase_command::Body { entity_id, preconditions: Vec::new() })
            .collect();
        if erasures.is_empty() {
            return Ok(());
        }
        let mut plan = Plan::new(None, now_ms());
        plan.command(&command(Params::Erase(EraseCommand { erasures: erasures.clone(), inventory_id: String::new() })))?;
        let result = json!({ "ship": geid.to_string(), "erased": erasures.len() });
        let (_, replayed) = apply_once(db, &format!("{TRIM_PREFIX}{geid}"), owner, plan, result).await?;
        info!(ship = geid, owner, roots = erasures.len(), replayed, "the parts the client spawns itself are out of the ship's loadout tree");
        Ok(())
    }

    /// Makes ship `geid` of class `ship_class_crc` (owned by `owner`) have its parts in the graph, once: nothing is made for
    /// a ship that has them, one the table does not know, or without a table. Returns its parts as `refs` does.
    pub async fn ensure(&self, db: &Arango, owner: u64, geid: u64, ship_class_crc: u32) -> Result<Vec<ShipPartRef>, Status> {
        if self.done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(&geid) {
            return self.refs(db, geid).await;
        }
        let mut held = self.refs(db, geid).await?;
        let table = self.table().await;
        if !held.is_empty() {
            // A tree made with every part loses the ones the client spawns itself, once.
            let Some(parts) = table.as_ref().and_then(|table| table.parts(ship_class_crc)) else {
                return Ok(held);
            };
            if !default_roots(&parts).is_empty() {
                self.trim(db, owner, geid, &parts).await?;
                held = self.refs(db, geid).await?;
            }
            self.done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(geid);
            return Ok(held);
        }
        let Some(table) = table else {
            return Ok(Vec::new());
        };
        let Some(parts) = table.parts(ship_class_crc) else {
            warn!(ship = geid, class_crc = format_args!("{ship_class_crc:#010x}"), "the ship class has no parts in the game data: no loadout tree");
            return Ok(Vec::new());
        };
        if parts.is_empty() || parts.len() > MAX_PARTS {
            return Ok(Vec::new());
        }
        let command_id = format!("{COMMAND_PREFIX}{geid}");
        if recorded(db, &command_id).await?.is_some() {
            // Made before and since erased (the player took them off): not made again.
            self.done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(geid);
            return self.refs(db, geid).await;
        }
        let crcs: Vec<u32> = parts.iter().map(|part| part.class_crc).collect::<std::collections::HashSet<_>>().into_iter().collect();
        let rows = db
            .aql("FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs RETURN c", json!({ "crcs": crcs }))
            .await
            .map_err(internal)?;
        let classes: HashMap<u32, Value> = rows.into_iter().filter_map(|row| Some((row["guidHashCrc"].as_u64()? as u32, row))).collect();
        let unknown = crcs.iter().filter(|crc| !classes.contains_key(crc)).count();
        let count = kept(&parts).len() as u64;
        let first = allocate_geids(db, count).await?;
        let geids: Vec<u64> = (first..first + count).collect();
        let mut plan = Plan::new(None, now_ms());
        for part in &geids {
            refuse_taken(&mut plan, *part);
        }
        let body = create_command::Body { graphs: Some(graph(geid, owner, &parts, &geids, &classes)) };
        plan.command(&command(Params::Create(CreateCommand { creates: vec![body], inventory_id: String::new() })))?;
        let result = json!({ "ship": geid.to_string(), "first": first.to_string(), "count": count });
        let (_, replayed) = apply_once(db, &command_id, owner, plan, result).await?;
        info!(ship = geid, owner, parts = count, unknown_classes = unknown, replayed, "the ship's loadout tree is in the graph");
        self.done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(geid);
        self.refs(db, geid).await
    }

    /// Gives the parts to the ships of `rows` (the `ships` rows) that have none, for the ships of a player read or the
    /// backfill at start. A failure is logged: the ship is tried again at the next read.
    pub async fn backfill(&self, db: &Arango, rows: &[Value]) {
        if !self.enabled() {
            return;
        }
        for row in rows {
            let Some(geid) = row["_key"].as_str().and_then(|key| key.parse::<u64>().ok()) else {
                continue;
            };
            if self.done.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(&geid) {
                continue;
            }
            if let Err(status) = self.backfill_one(db, geid, row).await {
                warn!(ship = geid, %status, "the ship's loadout tree was not made");
            }
        }
    }

    async fn backfill_one(&self, db: &Arango, geid: u64, row: &Value) -> Result<(), Status> {
        let owner = row["ownerId"].as_str().and_then(|owner| owner.parse::<u64>().ok()).unwrap_or(0);
        let Some(record) = crate::ships::row_record(row) else {
            return Ok(());
        };
        let found = db
            .aql("RETURN DOCUMENT('entity_classes', @guid).guidHashCrc", json!({ "guid": record.entity_class_guid.trim().to_lowercase() }))
            .await
            .map_err(internal)?;
        let Some(crc) = found.first().and_then(Value::as_u64) else {
            return Ok(());
        };
        self.ensure(db, owner, geid, crc as u32).await.map(|_| ())
    }
}

/// The table from the region's `sc-gamedata`.
async fn fetch(source: &Source) -> Result<ShipParts, String> {
    let endpoint = Endpoint::from_shared(source.address.clone()).map_err(|error| format!("gamedata address: {error}"))?;
    let channel = endpoint.connect_timeout(CONNECT_TIMEOUT).connect().await.map_err(|error| format!("{}: {error}", source.address))?;
    let mut client = GameDataClient::new(channel);
    let build = match &source.build {
        Some(build) => build.clone(),
        None => {
            let builds = client.list_builds(ListGameBuildsRequest {}).await.map_err(|status| status.message().to_string())?.into_inner().builds;
            builds.into_iter().rev().find(|build| build.ready).map(|build| build.build).ok_or("the game data has no build ready")?
        }
    };
    let request = GetGameFileRequest { build: build.clone(), path: SHIP_PARTS_PATH.to_string() };
    let mut stream = client.get_file(request).await.map_err(|status| format!("{SHIP_PARTS_PATH} of build {build}: {}", status.message()))?.into_inner();
    let (mut data, mut size) = (Vec::new(), None);
    while let Some(chunk) = stream.message().await.map_err(|status| status.message().to_string())? {
        if size.is_none() {
            if chunk.size > MAX_FILE_BYTES {
                return Err(format!("{SHIP_PARTS_PATH} is {} bytes", chunk.size));
            }
            size = Some(chunk.size);
        }
        data.extend_from_slice(&chunk.data);
    }
    if size != Some(data.len() as u64) {
        return Err(format!("{SHIP_PARTS_PATH}: {} bytes of {size:?}", data.len()));
    }
    info!(build, bytes = data.len(), "the ship parts file was read");
    ShipParts::parse(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIP: u64 = 400_000_000_246;
    const OWNER: u64 = 300_000_100_001;
    const GUN_MOUNT: u32 = 0x89ff_3fee;
    const GUN: u32 = 0x50d5_0e43;

    fn parts() -> Vec<Part> {
        vec![
            Part { class_crc: 0xce22_64a8, attach_id: 0x9a33_0c61, flags: 0, parent: None },
            Part { class_crc: GUN_MOUNT, attach_id: 0x702d_23a7, flags: 0, parent: None },
            Part { class_crc: GUN, attach_id: 0x5af9_cde4, flags: 0, parent: Some(1) },
        ]
    }

    #[test]
    fn the_parts_hang_from_the_ship_and_each_other_at_their_ports() {
        let geids = [SHIP + 10, SHIP + 11, SHIP + 12];
        let classes = HashMap::from([(GUN, json!({ "itemTypeEnum": 56, "itemSubTypeEnum": 3 }))]);
        let graph = graph(SHIP, OWNER, &parts(), &geids, &classes);
        assert_eq!(graph.nodes.len(), 3);
        let property = |index: usize| match graph.nodes[index].properties.as_ref().and_then(|p| p.properties.clone()) {
            Some(node_properties::Properties::EntityProperties(properties)) => properties,
            other => panic!("not an entity: {other:?}"),
        };
        let gun = property(2);
        assert_eq!((gun.geid, gun.owner_id, gun.class_guid_crc), (SHIP + 12, OWNER, GUN));
        assert_eq!((gun.item_type_enum, gun.item_sub_type_enum), (56, 3));
        assert_eq!(gun.debug[DEBUG_INDEX], "2");
        assert_eq!(gun.debug[DEBUG_SHIP], SHIP.to_string());
        assert_eq!(property(0).item_type_enum, 0, "a class the seed lacks");
        let ends: Vec<Option<Id>> = graph.edges.iter().map(|edge| edge.end.as_ref().and_then(|end| end.id.clone())).collect();
        assert_eq!(ends, [Some(Id::EntityId(SHIP)), Some(Id::EntityId(SHIP)), Some(Id::EntityId(SHIP + 11))]);
        let ports: Vec<u32> = graph.edges.iter().filter_map(|edge| edge.properties.as_ref().map(|p| p.attachment_port_crc)).collect();
        assert_eq!(ports, [0x9a33_0c61, 0x702d_23a7, 0x5af9_cde4]);
        assert!(graph.edges.iter().all(|edge| edge.r#type == EntityEdgeType::AttachedTo as i32));
    }

    #[test]
    fn the_parts_the_client_spawns_itself_have_no_entity_and_the_others_keep_their_table_index() {
        // 0 buff, 1 mount, 2 gun, 3 and 4 flagged (the gun's default parts, 4 inside 3), 5 a door on the ship.
        let mut parts = parts();
        let flagged = |parent: usize| Part { class_crc: 7, attach_id: 70 + parent as u32, flags: FLAG_DEFAULT_OF_PARENT, parent: Some(parent) };
        parts.push(flagged(2));
        parts.push(flagged(3));
        parts.push(Part { class_crc: 8, attach_id: 80, flags: 0, parent: None });
        assert_eq!(kept(&parts), [0, 1, 2, 5]);
        assert_eq!(default_roots(&parts), [3]);
        let geids = [SHIP + 10, SHIP + 11, SHIP + 12, SHIP + 13];
        let graph = graph(SHIP, OWNER, &parts, &geids, &HashMap::new());
        let indexes: Vec<(u64, String)> = graph
            .nodes
            .iter()
            .map(|node| match node.properties.as_ref().and_then(|p| p.properties.clone()) {
                Some(node_properties::Properties::EntityProperties(p)) => (p.geid, p.debug[DEBUG_INDEX].clone()),
                other => panic!("not an entity: {other:?}"),
            })
            .collect();
        assert_eq!(indexes, [(SHIP + 10, "0".into()), (SHIP + 11, "1".into()), (SHIP + 12, "2".into()), (SHIP + 13, "5".into())]);
        assert_eq!(graph.edges.len(), 4, "no edge for a part with no entity");
    }

    #[test]
    fn without_a_source_nothing_is_made() {
        assert!(!ShipLoadouts::none().enabled());
        assert!(ShipLoadouts::from(Source { address: "http://sc-gamedata:50051".into(), build: None }).enabled());
    }
}
