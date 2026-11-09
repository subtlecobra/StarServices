//! The game data seed of a region: the entity classes and the object container tree of a game build, which sc-seeder
//! (or sc-admin seed) reads from a seed bundle and streams here. sc-entitygraph is their only writer. Classes are
//! replaced by GUID; the tree is replaced whole, so a node a build no longer has goes with its edge.

use std::collections::HashSet;

use futures_util::{Stream, StreamExt};
use sc_db::{internal, Arango};
use sc_api::internal::{ContainerSeed, EntityClassSeed, LocalizedText, SeedContainersRequest, SeedEntityClassesRequest};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

/// `type` of an edge from a container to the one that holds it.
pub const EDGE_CONTAINED_IN: u32 = 1;
const DOCUMENTS_PER_QUERY: usize = 2000;
const CLASSES: &str = "entity_classes";
const NODES: &str = "containers";
const EDGES: &str = "container_edges";

/// The key of the pseudo class that carries the item banks' type filters (sc-admin's `ITEM_BANK_CLASS`). It is no entity's
/// class: its filters are kept as `itemBank`, never as `inventory`, which would give entities of class CRC 0 a container.
pub const ITEM_BANK_CLASS: &str = "itembankcontainer";

pub fn class_doc(class: &EntityClassSeed) -> Value {
    let inventory = class
        .inventory
        .as_ref()
        .map(|inventory| {
            let filters = |list: &[sc_api::internal::ItemTypeFilter]| -> Vec<Value> {
                list.iter().map(|filter| json!({ "itemType": filter.item_type, "itemSubTypes": filter.item_sub_types })).collect()
            };
            json!({
                "capacityMicroScu": inventory.capacity_micro_scu,
                "closed": inventory.closed,
                "excluded": filters(&inventory.excluded),
                "only": filters(&inventory.only),
            })
        });
    let (inventory, item_bank) = if class.guid == ITEM_BANK_CLASS { (None, inventory) } else { (inventory, None) };
    json!({
        "occupancyMicroScu": class.occupancy_micro_scu,
        "inventory": inventory,
        "itemBank": item_bank,
        "_key": class.guid,
        "guid": class.guid,
        "guidHashCrc": class.guid_hash_crc,
        "className": class.class_name,
        "itemType": class.item_type,
        "itemSubType": class.item_sub_type,
        "itemTypeEnum": class.item_type_enum,
        "itemSubTypeEnum": class.item_sub_type_enum,
        "name": text(class.name.as_ref()),
        "description": text(class.description.as_ref()),
    })
}

/// As sc-admin always wrote it: the token and the English text, empty strings for none.
fn text(text: Option<&LocalizedText>) -> Value {
    let text = text.cloned().unwrap_or_default();
    json!({ "token": text.token, "en": text.en })
}

pub fn node_doc(node: &ContainerSeed) -> Value {
    json!({
        "_key": node.geid.to_string(),
        "geid": node.geid.to_string(),
        "guid": node.guid,
        "labels": node.labels,
        "socpakContentHash": node.socpak_content_hash,
        "socpakFilePathHash": node.socpak_file_path_hash,
        "name": node.name,
        "system": node.system,
        "identifiers": {
            "startId": node.start_id.to_string(),
            "hasPlanet": node.has_planet,
            "unstreamableCount": 0,
            "streamingImmovableCount": node.streaming_immovable_count,
            "streamingMovableCount": node.streaming_movable_count,
        },
    })
}

/// A node's only edge goes to its parent, so the child's GEID is a stable key for it.
pub fn edge_doc(node: &ContainerSeed, parent: u64) -> Value {
    json!({
        "_key": node.geid.to_string(),
        "_from": format!("{NODES}/{}", node.geid),
        "_to": format!("{NODES}/{parent}"),
        "type": EDGE_CONTAINED_IN,
        "startId": node.geid.to_string(),
        "endId": parent.to_string(),
    })
}

async fn replace(db: &Arango, collection: &str, docs: &[Value]) -> Result<(), String> {
    let query = format!("FOR d IN @docs INSERT d INTO {collection} OPTIONS {{ overwriteMode: 'replace' }}");
    for chunk in docs.chunks(DOCUMENTS_PER_QUERY) {
        db.aql(&query, json!({ "docs": chunk })).await?;
    }
    Ok(())
}

/// Leaves exactly `docs` in the collection, so a build with a smaller tree keeps no stale branch.
async fn replace_all(db: &Arango, collection: &str, docs: &[Value]) -> Result<(), String> {
    replace(db, collection, docs).await?;
    let keys: Vec<&Value> = docs.iter().map(|doc| &doc["_key"]).collect();
    let query = format!("FOR d IN {collection} FILTER d._key NOT IN @keys REMOVE d IN {collection}");
    db.aql(&query, json!({ "keys": keys })).await?;
    Ok(())
}

/// Writes the classes as they arrive. Returns how many were written.
pub async fn classes<S>(db: &Arango, mut requests: S) -> Result<u64, Status>
where
    S: Stream<Item = Result<SeedEntityClassesRequest, Status>> + Unpin,
{
    let mut written = 0u64;
    while let Some(request) = requests.next().await {
        let request = request?;
        if let Some(class) = request.classes.iter().find(|class| class.guid.is_empty()) {
            return Err(Status::invalid_argument(format!("entity class {:?} has no GUID", class.class_name)));
        }
        let docs: Vec<Value> = request.classes.iter().map(class_doc).collect();
        replace(db, CLASSES, &docs).await.map_err(internal)?;
        written += docs.len() as u64;
    }
    info!(classes = written, "entity classes seeded");
    Ok(written)
}

/// Checks a whole tree: not empty, every GEID once, every parent in it.
pub fn check_tree(nodes: &[ContainerSeed]) -> Result<(), String> {
    if nodes.is_empty() {
        return Err("an empty container tree would remove every container".into());
    }
    let mut geids = HashSet::with_capacity(nodes.len());
    for node in nodes {
        if !geids.insert(node.geid) {
            return Err(format!("container {} sent twice", node.geid));
        }
    }
    match nodes.iter().find(|node| node.parent.is_some_and(|parent| !geids.contains(&parent))) {
        Some(node) => Err(format!("container {} has parent {} that is not in the tree", node.geid, node.parent.unwrap_or_default())),
        None => Ok(()),
    }
}

/// Takes the whole tree, then replaces the stored one with it. Returns the nodes written.
pub async fn containers<S>(db: &Arango, mut requests: S) -> Result<u64, Status>
where
    S: Stream<Item = Result<SeedContainersRequest, Status>> + Unpin,
{
    let mut nodes = Vec::new();
    while let Some(request) = requests.next().await {
        nodes.extend(request?.containers);
    }
    check_tree(&nodes).map_err(Status::invalid_argument)?;
    let node_docs: Vec<Value> = nodes.iter().map(node_doc).collect();
    let edge_docs: Vec<Value> = nodes.iter().filter_map(|node| Some(edge_doc(node, node.parent?))).collect();
    replace_all(db, NODES, &node_docs).await.map_err(internal)?;
    replace_all(db, EDGES, &edge_docs).await.map_err(internal)?;
    info!(nodes = node_docs.len(), edges = edge_docs.len(), "container tree seeded");
    Ok(node_docs.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(geid: u64, parent: Option<u64>) -> ContainerSeed {
        ContainerSeed { geid, parent, ..Default::default() }
    }

    #[test]
    fn a_tree_must_be_whole() {
        assert!(check_tree(&[node(1, None), node(2, Some(1))]).is_ok());
        assert!(check_tree(&[]).is_err());
        assert!(check_tree(&[node(1, None), node(1, None)]).is_err());
        assert!(check_tree(&[node(1, None), node(2, Some(3))]).is_err());
    }

    #[test]
    fn documents_keep_the_fields_sc_admin_wrote() {
        let class = EntityClassSeed {
            guid: "g".into(),
            guid_hash_crc: 7,
            class_name: "Crate".into(),
            item_type_enum: 27,
            item_sub_type_enum: 154,
            occupancy_micro_scu: None,
            inventory: Some(sc_api::internal::EntityClassInventory { capacity_micro_scu: 5, closed: true, ..Default::default() }),
            ..Default::default()
        };
        let doc = class_doc(&class);
        assert_eq!(doc["_key"], "g");
        assert_eq!(doc["occupancyMicroScu"], Value::Null);
        assert_eq!((doc["itemTypeEnum"].as_i64(), doc["itemSubTypeEnum"].as_i64()), (Some(27), Some(154)));
        assert_eq!(doc["inventory"], json!({ "capacityMicroScu": 5, "closed": true, "excluded": [], "only": [] }));
        assert_eq!(doc["name"], json!({ "token": "", "en": "" }));
        let without = class_doc(&EntityClassSeed { guid: "h".into(), occupancy_micro_scu: Some(3), ..Default::default() });
        assert_eq!(without["inventory"], Value::Null);
        assert_eq!(without["occupancyMicroScu"], 3);

        let child = ContainerSeed { geid: 200, parent: Some(100), labels: vec![3], start_id: 201, ..Default::default() };
        assert_eq!(node_doc(&child)["identifiers"]["startId"], "201");
        assert_eq!(
            edge_doc(&child, 100),
            json!({ "_key": "200", "_from": "containers/200", "_to": "containers/100", "type": 1, "startId": "200", "endId": "100" })
        );
    }
}
