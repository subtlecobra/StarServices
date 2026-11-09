//! Conversion between protobuf messages and ArangoDB documents. Property names follow the client's
//! column names, such as `ownerId`, because query filters arrive with those names. A 64-bit id is
//! kept as a decimal string, since ArangoDB computes with doubles and would round it.

use std::collections::HashMap;

use prost_types::Timestamp;
use sc_proto::{
    common::types::v1::{LocalizedString, Quatd, Vec3d},
    services::entitygraph::v1::{
        container_node_properties::Identifiers, entity_edge_properties, entity_edge_vertex,
        inventory_configuration, node_properties, ContainerEdge, ContainerEdgeProperties, ContainerNode,
        ContainerNodeProperties, EntityClass, EntityClassProperties, EntityComponent, EntityEdge,
        EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityNodeLabel, EntityNodeProperties,
        EntitySnapshot, EntityStowContext, EntityVariable, InventoryConfiguration, InventoryNodeProperties,
        InventoryType, Node, NodeProperties, NodeType, PhysicalInventoryConfiguration,
        PhysicalInventoryEdgeConfiguration,
    },
};
use serde_json::{json, Value};

pub const ENTITIES: &str = "entities";
pub const INVENTORIES: &str = "inventories";
/// The context names of the client's inventory ids, `<holder>:<context>:<subject>` (the game formats them with
/// `"%llu:%s:%llu"`, 0x1406cc590, from `EInventoryContext`'s names).
pub const PLAYER_INVENTORY: &str = "PlayerInventory";
/// A player's item bank at a location: holder the player, subject the location id.
pub const LOCATION: &str = "Location";
/// The inventory of an entity, such as a crate, a backpack or a ship's cargo grid: holder the entity, subject 0.
pub const CONTAINER: &str = "Container";
/// A player's ship bank at a location (the client's `EInventoryContext` 8): holder the player, subject the location id.
pub const HANGAR: &str = "Hangar";
/// A player's own inventory inside an entity: owner the subject.
pub const PERSONAL_ENTITY_INVENTORY: &str = "PersonalEntityInventory";
/// Contexts whose owner is the holder, as `SInventoryConfiguration::Create` (0x140649510) sets it.
const HOLDER_OWNED: &[&str] = &["Entitlement", LOCATION, PLAYER_INVENTORY, "PlayerEscrowInventory", "Prison", HANGAR];
/// The `Player` record of the game data (5287b146-fa24-49b8-8126-326164217c4f).
pub const PLAYER_CLASS_CRC: u32 = 2_961_494_058;
/// `NOITEM_Player` and its `UNDEFINED` subtype, which the client expects of a player and reports otherwise as
/// `Entity Item Types Mismatch`.
pub const PLAYER_ITEM_TYPE: i32 = 135;
pub const PLAYER_ITEM_SUB_TYPE: i32 = 154;
const LOW_32_BITS: u64 = 0xffff_ffff;
const OWNERSHIP_STATUS_NONE: i32 = 1;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    let digit = |pair: &[u8]| std::str::from_utf8(pair).ok().and_then(|s| u8::from_str_radix(s, 16).ok());
    text.as_bytes().chunks(2).filter_map(digit).collect()
}

/// An ArangoDB key for any string: bytes outside its allowed set are percent escaped.
pub fn key(id: &str) -> String {
    let mut escaped = String::with_capacity(id.len());
    for byte in id.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b':' | b'.' | b'@' => escaped.push(byte as char),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

pub fn entity_id(geid: u64) -> String {
    format!("{ENTITIES}/{geid}")
}

pub fn inventory_handle(id: &str) -> String {
    format!("{INVENTORIES}/{}", key(id))
}

/// The entity that holds an inventory, from the id's first field.
pub fn inventory_holder(id: &str) -> u64 {
    id.split(':').next().and_then(|holder| holder.parse().ok()).unwrap_or(0)
}

/// The three parts of an inventory id, `<holder>:<context>:<subject>`.
#[derive(Debug, PartialEq, Eq)]
pub struct InventoryId<'a> {
    pub holder: u64,
    pub context: &'a str,
    pub subject: u64,
}

pub fn parse_inventory_id(id: &str) -> Option<InventoryId<'_>> {
    let mut parts = id.split(':');
    let holder = parts.next()?.parse().ok()?;
    let context = parts.next().filter(|context| !context.is_empty())?;
    let subject = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(InventoryId { holder, context, subject })
}

/// Who owns an inventory by its id alone, the game's rule: the holder for a player's inventories (location,
/// personal, hangar, ...), the subject for a personal entity inventory, nobody (0) for a container, whose owner is
/// then whoever owns what holds it.
pub fn implied_owner(id: &InventoryId) -> u64 {
    match id.context {
        context if HOLDER_OWNED.contains(&context) => id.holder,
        PERSONAL_ENTITY_INVENTORY => id.subject,
        _ => 0,
    }
}

pub fn container_inventory_id(holder: u64) -> String {
    format!("{holder}:{CONTAINER}:0")
}

/// An inventory created the first time something is stowed into it, from its id alone: infinite, owned as the id
/// says. A container's capacity is added from its holder's class where the statement creates it.
pub fn implied_inventory_doc(id: &str, now: u64) -> Option<Value> {
    let parsed = parse_inventory_id(id)?;
    let properties = InventoryNodeProperties {
        id: id.to_string(),
        inventory_type: InventoryType::Infinite as i32,
        owner_id: implied_owner(&parsed),
        ..Default::default()
    };
    Some(inventory_doc(&properties, parsed.holder, now))
}

fn low_bits(value: u64) -> u64 {
    value & LOW_32_BITS
}

fn text(doc: &Value, field: &str) -> String {
    doc[field].as_str().unwrap_or_default().to_string()
}

fn optional_text(doc: &Value, field: &str) -> Option<String> {
    doc[field].as_str().map(str::to_string)
}

pub fn big(doc: &Value, field: &str) -> u64 {
    doc[field].as_str().and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn int(doc: &Value, field: &str) -> u64 {
    doc[field].as_u64().unwrap_or(0)
}

fn signed(doc: &Value, field: &str) -> i32 {
    doc[field].as_i64().unwrap_or(0) as i32
}

/// A volume in micro-SCU, which the database keeps whole and the message carries in 32 bits: a hold above about
/// 2147 SCU reads as full scale.
fn clamped(doc: &Value, field: &str) -> i32 {
    doc[field].as_f64().unwrap_or(0.0).clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn float(doc: &Value, field: &str) -> f64 {
    doc[field].as_f64().unwrap_or(0.0)
}

fn numbers(doc: &Value, field: &str) -> Vec<u64> {
    doc[field].as_array().map(|list| list.iter().filter_map(Value::as_u64).collect()).unwrap_or_default()
}

fn strings(doc: &Value, field: &str) -> HashMap<String, String> {
    let fields = doc[field].as_object().into_iter().flatten();
    fields.filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string()))).collect()
}

/// The stow state of a new entity.
#[derive(Clone, Default)]
pub struct Stow {
    pub inventory_id: Option<String>,
    pub shard_id: Option<String>,
}

pub fn stow_fields(stow: &Stow) -> Value {
    json!({
        "stowed": stow.inventory_id.is_some(),
        "inventoryId": stow.inventory_id,
        "shardId": stow.shard_id,
    })
}

pub fn entity_doc(properties: &EntityNodeProperties, stow: &Stow, now: u64) -> Value {
    let orientation = properties.orientation.unwrap_or(Quatd { w: 1.0, ..Default::default() });
    let position = properties.position.unwrap_or_default();
    let created = if properties.created_at == 0 { now } else { properties.created_at };
    let components: Vec<Value> = properties
        .components
        .iter()
        .map(|component| json!({ "hashId": component.hash_id, "serverOnly": component.server_only }))
        .collect();
    // The client logs an error for every entity whose status it reads as unspecified.
    let status = if properties.ownership_status == 0 { OWNERSHIP_STATUS_NONE } else { properties.ownership_status };
    let mut doc = json!({
        "_key": properties.geid.to_string(),
        "geid": properties.geid.to_string(),
        "classGuidCrc": properties.class_guid_crc,
        "ownershipType": properties.ownership_type.to_string(),
        "entityFlags": properties.entity_flags.to_string(),
        "entityFlagsLow": low_bits(properties.entity_flags),
        "gameFlags": properties.game_flags.to_string(),
        "gameFlagsLow": low_bits(properties.game_flags),
        "orientation": { "x": orientation.x, "y": orientation.y, "z": orientation.z, "w": orientation.w },
        "position": { "x": position.x, "y": position.y, "z": position.z },
        "scale": properties.scale,
        "radius": properties.radius,
        "stackSize": properties.stack_size,
        "ownerId": properties.owner_id.to_string(),
        "parentUrn": properties.parent_urn,
        "labels": properties.labels,
        "generation": properties.generation,
        "tags": properties.tags,
        "components": components,
        "locationId": properties.location_id,
        "debug": properties.debug,
        "version": properties.version,
        "createdAt": created,
        "updatedAt": now,
        "unstowedFromInventoryId": properties.unstowed_from_inventory_id,
        "itemTypeEnum": properties.item_type_enum,
        "itemSubTypeEnum": properties.item_sub_type_enum,
        "ownershipStatus": status,
        "ownershipTimestamp": properties.ownership_timestamp.as_ref().map(timestamp_doc),
        "moving": false,
    });
    merge(&mut doc, stow_fields(stow));
    doc
}

pub fn merge(doc: &mut Value, extra: Value) {
    if let (Some(target), Value::Object(fields)) = (doc.as_object_mut(), extra) {
        target.extend(fields);
    }
}

pub fn timestamp_doc(timestamp: &Timestamp) -> Value {
    json!({ "seconds": timestamp.seconds, "nanos": timestamp.nanos })
}

fn entity_properties(doc: &Value) -> EntityNodeProperties {
    let orientation = &doc["orientation"];
    let position = &doc["position"];
    let timestamp = &doc["ownershipTimestamp"];
    EntityNodeProperties {
        geid: big(doc, "geid"),
        class_guid_crc: int(doc, "classGuidCrc") as u32,
        ownership_type: big(doc, "ownershipType"),
        entity_flags: big(doc, "entityFlags"),
        game_flags: big(doc, "gameFlags"),
        orientation: Some(Quatd {
            x: float(orientation, "x"),
            y: float(orientation, "y"),
            z: float(orientation, "z"),
            w: float(orientation, "w"),
        }),
        position: Some(Vec3d { x: float(position, "x"), y: float(position, "y"), z: float(position, "z") }),
        scale: float(doc, "scale"),
        radius: float(doc, "radius"),
        stack_size: int(doc, "stackSize") as u32,
        owner_id: big(doc, "ownerId"),
        parent_urn: text(doc, "parentUrn"),
        labels: numbers(doc, "labels").into_iter().map(|label| label as i32).collect(),
        generation: int(doc, "generation") as u32,
        tags: numbers(doc, "tags").into_iter().map(|tag| tag as u32).collect(),
        components: doc["components"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|component| EntityComponent {
                hash_id: int(component, "hashId") as u32,
                server_only: component["serverOnly"].as_bool().unwrap_or(false),
            })
            .collect(),
        location_id: int(doc, "locationId") as u32,
        debug: strings(doc, "debug"),
        version: int(doc, "version") as u32,
        created_at: int(doc, "createdAt"),
        updated_at: int(doc, "updatedAt"),
        unstowed_from_inventory_id: text(doc, "unstowedFromInventoryId"),
        item_type_enum: signed(doc, "itemTypeEnum"),
        item_sub_type_enum: signed(doc, "itemSubTypeEnum"),
        ownership_status: signed(doc, "ownershipStatus"),
        ownership_timestamp: timestamp.is_object().then(|| Timestamp {
            seconds: timestamp["seconds"].as_i64().unwrap_or(0),
            nanos: signed(timestamp, "nanos"),
        }),
    }
}

pub fn entity_node(doc: &Value) -> Node {
    Node {
        properties: Some(NodeProperties {
            properties: Some(node_properties::Properties::EntityProperties(entity_properties(doc))),
        }),
        r#type: NodeType::Entity as i32,
    }
}

/// The context, subject and owner a caller leaves out are taken from the id, as the game fills them (the context's
/// name, the subject, and the owner by `implied_owner`).
pub fn inventory_doc(properties: &InventoryNodeProperties, holder: u64, now: u64) -> Value {
    let physical = properties.configuration.as_ref().and_then(physical_configuration);
    let created = if properties.created_at == 0 { now } else { properties.created_at };
    let parsed = parse_inventory_id(&properties.id);
    let context = match (&parsed, properties.context.is_empty()) {
        (Some(parsed), true) => parsed.context.to_string(),
        _ => properties.context.clone(),
    };
    let subject = match &parsed {
        Some(parsed) if properties.subject_id == 0 => parsed.subject,
        _ => properties.subject_id,
    };
    let owner = match &parsed {
        Some(parsed) if properties.owner_id == 0 => implied_owner(parsed),
        _ => properties.owner_id,
    };
    // An inventory that holds by volume always has a configuration, so its occupancy can be counted.
    let physical = physical.or_else(|| {
        (properties.inventory_type == InventoryType::Physical as i32).then(PhysicalInventoryConfiguration::default)
    });
    json!({
        "_key": key(&properties.id),
        "id": properties.id,
        "inventoryType": properties.inventory_type,
        "ownerId": owner.to_string(),
        "holderId": holder.to_string(),
        "unstowedIn": properties.unstowed_in,
        "generation": properties.generation,
        "shared": properties.shared,
        "configuration": physical_doc(physical.as_ref()),
        "name": if properties.name.is_empty() { properties.id.clone() } else { properties.name.clone() },
        "debug": properties.debug,
        "context": context,
        "subjectId": subject.to_string(),
        "createdAt": created,
        "updatedAt": now,
    })
}

pub fn physical_configuration(config: &InventoryConfiguration) -> Option<PhysicalInventoryConfiguration> {
    config.configuration.map(|inventory_configuration::Configuration::Physical(physical)| physical)
}

pub fn physical_doc(physical: Option<&PhysicalInventoryConfiguration>) -> Value {
    physical.map_or(Value::Null, |physical| {
        json!({ "physical": { "capacity": physical.capacity, "occupancy": physical.occupancy } })
    })
}

pub fn inventory_properties(doc: &Value) -> InventoryNodeProperties {
    let physical = &doc["configuration"]["physical"];
    InventoryNodeProperties {
        inventory_type: signed(doc, "inventoryType"),
        owner_id: big(doc, "ownerId"),
        unstowed_in: text(doc, "unstowedIn"),
        generation: int(doc, "generation") as u32,
        shared: doc["shared"].as_bool().unwrap_or(false),
        configuration: physical.is_object().then(|| InventoryConfiguration {
            configuration: Some(inventory_configuration::Configuration::Physical(PhysicalInventoryConfiguration {
                capacity: clamped(physical, "capacity"),
                occupancy: clamped(physical, "occupancy"),
            })),
        }),
        name: text(doc, "name"),
        debug: strings(doc, "debug"),
        id: text(doc, "id"),
        context: text(doc, "context"),
        subject_id: big(doc, "subjectId"),
        created_at: int(doc, "createdAt"),
        updated_at: int(doc, "updatedAt"),
    }
}

pub fn inventory_node(doc: &Value) -> Node {
    Node {
        properties: Some(NodeProperties {
            properties: Some(node_properties::Properties::InventoryProperties(inventory_properties(doc))),
        }),
        r#type: NodeType::Inventory as i32,
    }
}

/// A node of either kind, told apart by its collection.
pub fn any_node(doc: &Value) -> Node {
    if doc["_id"].as_str().is_some_and(|id| id.starts_with(INVENTORIES)) {
        inventory_node(doc)
    } else {
        entity_node(doc)
    }
}

/// The handle of a vertex, or None for a shard, which is not a node of this graph.
pub fn vertex_handle(vertex: &EntityEdgeVertex) -> Option<String> {
    match vertex.id.as_ref()? {
        entity_edge_vertex::Id::EntityId(geid) => Some(entity_id(*geid)),
        entity_edge_vertex::Id::InventoryId(id) => Some(inventory_handle(id)),
        entity_edge_vertex::Id::ShardId(_) => None,
    }
}

fn vertex_id(vertex: &EntityEdgeVertex) -> String {
    match &vertex.id {
        Some(entity_edge_vertex::Id::EntityId(geid)) => geid.to_string(),
        Some(entity_edge_vertex::Id::InventoryId(id)) => id.clone(),
        Some(entity_edge_vertex::Id::ShardId(id)) => id.clone(),
        None => String::new(),
    }
}

/// An entity has one parent of each kind, so the key of such an edge names only the child: writing a
/// new one replaces the old one.
pub fn edge_key(kind: i32, start: &str, end: &str) -> String {
    let start = key(start);
    if kind == EntityEdgeType::References as i32 {
        return format!("r_{start}_{}", key(end));
    }
    format!("{kind}_{start}")
}

pub fn edge_doc(edge: &EntityEdge) -> Option<Value> {
    let start = edge.start.as_ref()?;
    let end = edge.end.as_ref()?;
    let properties = edge.properties.clone().unwrap_or_default();
    let occupancy = match properties.inventory {
        Some(entity_edge_properties::Inventory::Physical(physical)) => json!({ "occupancy": physical.occupancy }),
        None => Value::Null,
    };
    let (start_id, end_id) = (vertex_id(start), vertex_id(end));
    Some(json!({
        "_key": edge_key(edge.r#type, &start_id, &end_id),
        "_from": vertex_handle(start)?,
        "_to": vertex_handle(end)?,
        "type": edge.r#type,
        "startId": start_id,
        "startType": start.r#type,
        "endId": end_id,
        "endType": end.r#type,
        "attachmentType": properties.attachment_type,
        "attachmentPortCrc": properties.attachment_port_crc,
        "rank": properties.rank,
        "context": properties.context,
        "subjectId": properties.subject_id.to_string(),
        "version": properties.version,
        "physical": occupancy,
    }))
}

fn vertex(kind: i32, id: String) -> EntityEdgeVertex {
    let parsed = if kind == NodeType::Inventory as i32 {
        entity_edge_vertex::Id::InventoryId(id)
    } else {
        entity_edge_vertex::Id::EntityId(id.parse().unwrap_or(0))
    };
    EntityEdgeVertex { r#type: kind, id: Some(parsed) }
}

pub fn edge_message(doc: &Value) -> EntityEdge {
    let physical = doc["physical"].is_object().then(|| {
        entity_edge_properties::Inventory::Physical(PhysicalInventoryEdgeConfiguration {
            occupancy: int(&doc["physical"], "occupancy") as u32,
        })
    });
    EntityEdge {
        r#type: signed(doc, "type"),
        start: Some(vertex(signed(doc, "startType"), text(doc, "startId"))),
        end: Some(vertex(signed(doc, "endType"), text(doc, "endId"))),
        properties: Some(EntityEdgeProperties {
            attachment_type: signed(doc, "attachmentType"),
            attachment_port_crc: int(doc, "attachmentPortCrc") as u32,
            rank: text(doc, "rank"),
            context: text(doc, "context"),
            subject_id: big(doc, "subjectId"),
            version: int(doc, "version") as u32,
            inventory: physical,
        }),
    }
}

/// The edge from an inventory to the entity that holds it, which makes the items in a ship's locker
/// reachable from the ship by walking edges.
pub fn holder_edge(inventory_id: &str, holder: u64) -> Value {
    let kind = EntityEdgeType::AttachedTo as i32;
    json!({
        "_key": edge_key(kind, inventory_id, ""),
        "_from": inventory_handle(inventory_id),
        "_to": entity_id(holder),
        "type": kind,
        "startId": inventory_id,
        "startType": NodeType::Inventory as i32,
        "endId": holder.to_string(),
        "endType": NodeType::Entity as i32,
        "attachmentType": 0,
        "attachmentPortCrc": 0,
        "rank": "",
        "context": "",
        "subjectId": "0",
        "version": 0,
        "physical": Value::Null,
    })
}

pub fn snapshot_variables(snapshot: &EntitySnapshot) -> Vec<Value> {
    snapshot
        .variables
        .iter()
        .map(|variable| {
            json!({
                "nameCrc": variable.name_crc,
                "type": variable.r#type,
                "flags": variable.flags,
                "snapshot": hex(&variable.snapshot),
            })
        })
        .collect()
}

/// `include` keeps a variable with any of its bits, when not zero; `exclude` drops one with any.
pub fn snapshot_message(doc: &Value, include: u32, exclude: u32) -> EntitySnapshot {
    let variables = doc["variables"].as_array().into_iter().flatten();
    EntitySnapshot {
        entity_id: doc["_key"].as_str().and_then(|geid| geid.parse().ok()).unwrap_or(0),
        version: int(doc, "version") as u32,
        variables: variables
            .map(|variable| EntityVariable {
                name_crc: int(variable, "nameCrc") as u32,
                r#type: int(variable, "type") as u32,
                flags: int(variable, "flags") as u32,
                snapshot: unhex(variable["snapshot"].as_str().unwrap_or_default()),
            })
            .filter(|variable| (include == 0 || variable.flags & include != 0) && variable.flags & exclude == 0)
            .collect(),
    }
}

pub fn stow_context(doc: &Value) -> EntityStowContext {
    EntityStowContext {
        entity_id: big(doc, "geid"),
        parent_urn: text(doc, "parentUrn"),
        owner_id: big(doc, "ownerId"),
        is_stowed: doc["stowed"].as_bool().unwrap_or(false),
        shard_id: optional_text(doc, "shardId"),
        inventory_id: optional_text(doc, "inventoryId"),
        class_guid_crc: int(doc, "classGuidCrc") as u32,
        unstowed_from_inventory_id: text(doc, "unstowedFromInventoryId"),
        moving: doc["moving"].as_bool(),
    }
}

pub fn container_node(doc: &Value) -> ContainerNode {
    let identifiers = &doc["identifiers"];
    ContainerNode {
        labels: numbers(doc, "labels").into_iter().map(|label| label as i32).collect(),
        properties: Some(ContainerNodeProperties {
            guid: text(doc, "guid"),
            geid: big(doc, "geid"),
            socpak_content_hash: int(doc, "socpakContentHash") as u32,
            socpak_file_path_hash: int(doc, "socpakFilePathHash") as u32,
            identifiers: identifiers.is_object().then(|| Identifiers {
                start_id: big(identifiers, "startId"),
                has_planet: identifiers["hasPlanet"].as_bool().unwrap_or(false),
                unstreamable_count: int(identifiers, "unstreamableCount") as u32,
                streaming_immovable_count: int(identifiers, "streamingImmovableCount") as u32,
                streaming_movable_count: int(identifiers, "streamingMovableCount") as u32,
            }),
            generation: int(doc, "generation") as u32,
            created_at: int(doc, "createdAt"),
            updated_at: int(doc, "updatedAt"),
        }),
    }
}

pub fn container_edge(doc: &Value) -> ContainerEdge {
    ContainerEdge {
        r#type: signed(doc, "type"),
        start: big(doc, "startId"),
        end: big(doc, "endId"),
        properties: Some(ContainerEdgeProperties {}),
    }
}

pub fn entity_class(doc: &Value) -> EntityClass {
    EntityClass {
        properties: Some(EntityClassProperties {
            guid: text(doc, "guid"),
            guid_hash_crc: int(doc, "guidHashCrc") as u32,
            class_name: text(doc, "className"),
            item_type: text(doc, "itemType"),
            item_sub_type: text(doc, "itemSubType"),
            name: localized(&doc["name"]),
            description: localized(&doc["description"]),
            tags: numbers(doc, "tags").into_iter().map(|tag| tag as u32).collect(),
            generation: int(doc, "generation") as u32,
            item_type_enum: signed(doc, "itemTypeEnum"),
            item_sub_type_enum: signed(doc, "itemSubTypeEnum"),
            components: numbers(doc, "components").into_iter().map(|hash| hash as u32).collect(),
        }),
    }
}

fn localized(doc: &Value) -> Option<LocalizedString> {
    doc.is_object().then(|| LocalizedString {
        token: text(doc, "token"),
        en: text(doc, "en"),
        ..Default::default()
    })
}

/// A player entity, created the first time the account is seen. The client looks the player up by
/// GEID and needs the entity to exist; it holds no state of its own yet.
pub fn player_doc(geid: u64, handle: &str, inventory_id: &str, now: u64) -> Value {
    let labels = [EntityNodeLabel::AggregateRoot, EntityNodeLabel::StreamingGroup, EntityNodeLabel::Player];
    let properties = EntityNodeProperties {
        geid,
        class_guid_crc: PLAYER_CLASS_CRC,
        item_type_enum: PLAYER_ITEM_TYPE,
        item_sub_type_enum: PLAYER_ITEM_SUB_TYPE,
        owner_id: geid,
        scale: 1.0,
        labels: labels.iter().map(|label| *label as i32).collect(),
        debug: HashMap::from([("handle".to_string(), handle.to_string())]),
        created_at: now,
        ..Default::default()
    };
    entity_doc(&properties, &Stow { inventory_id: Some(inventory_id.to_string()), shard_id: None }, now)
}

pub fn player_inventory_id(geid: u64) -> String {
    format!("{geid}:{PLAYER_INVENTORY}:0")
}

pub fn player_inventory_doc(geid: u64, now: u64) -> Value {
    let properties = InventoryNodeProperties {
        id: player_inventory_id(geid),
        inventory_type: InventoryType::Infinite as i32,
        owner_id: geid,
        context: PLAYER_INVENTORY.to_string(),
        ..Default::default()
    };
    inventory_doc(&properties, geid, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_class_keeps_its_localized_name() {
        let doc = json!({ "guid": "g", "guidHashCrc": 7, "name": { "token": "@n", "en": "Pistol" } });
        let properties = entity_class(&doc).properties.unwrap();
        assert_eq!(properties.guid_hash_crc, 7);
        assert_eq!(properties.name.unwrap().en, "Pistol");
        assert!(properties.description.is_none());
    }

    #[test]
    fn a_geid_above_the_double_range_survives_the_round_trip() {
        let properties = EntityNodeProperties { geid: u64::MAX - 1, owner_id: u64::MAX, ..Default::default() };
        let doc = entity_doc(&properties, &Stow::default(), 5);
        let back = entity_properties(&doc);
        assert_eq!((back.geid, back.owner_id), (u64::MAX - 1, u64::MAX));
    }

    #[test]
    fn a_new_entity_never_has_an_unspecified_ownership_status() {
        let doc = entity_doc(&EntityNodeProperties::default(), &Stow::default(), 5);
        assert_eq!(entity_properties(&doc).ownership_status, OWNERSHIP_STATUS_NONE);
    }

    #[test]
    fn the_player_is_a_stowed_player_node_owning_itself() {
        let doc = player_doc(300_000_000_001, "Citizen", &player_inventory_id(300_000_000_001), 5);
        let context = stow_context(&doc);
        assert!(context.is_stowed);
        assert_eq!(context.owner_id, 300_000_000_001);
        assert_eq!(context.inventory_id.as_deref(), Some("300000000001:PlayerInventory:0"));
        assert!(entity_properties(&doc).labels.contains(&(EntityNodeLabel::Player as i32)));
        assert_eq!(entity_properties(&doc).class_guid_crc, PLAYER_CLASS_CRC);
        let properties = entity_properties(&doc);
        assert_eq!((properties.item_type_enum, properties.item_sub_type_enum), (PLAYER_ITEM_TYPE, PLAYER_ITEM_SUB_TYPE));
    }

    #[test]
    fn edges_keep_their_vertices_and_properties() {
        let edge = EntityEdge {
            r#type: EntityEdgeType::StowedIn as i32,
            start: Some(EntityEdgeVertex { r#type: 1, id: Some(entity_edge_vertex::Id::EntityId(7)) }),
            end: Some(EntityEdgeVertex { r#type: 2, id: Some(entity_edge_vertex::Id::InventoryId("7:Container:0".into())) }),
            properties: Some(EntityEdgeProperties { rank: "a".into(), ..Default::default() }),
        };
        assert_eq!(edge_message(&edge_doc(&edge).unwrap()), edge);
    }

    #[test]
    fn an_inventory_id_names_its_holder_context_and_subject() {
        let parsed = parse_inventory_id("300000000001:Location:848868938").unwrap();
        assert_eq!(parsed, InventoryId { holder: 300_000_000_001, context: LOCATION, subject: 848_868_938 });
        assert_eq!(implied_owner(&parsed), 300_000_000_001);
        assert_eq!(implied_owner(&parse_inventory_id("9:PersonalEntityInventory:7").unwrap()), 7);
        assert_eq!(implied_owner(&parse_inventory_id(&container_inventory_id(9)).unwrap()), 0);
        for bad in ["", "9:Container", "x:Container:0", "9::0", "9:Container:0:1"] {
            assert!(parse_inventory_id(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_new_inventory_takes_what_it_lacks_from_its_id() {
        let properties = InventoryNodeProperties { id: "5:Location:42".into(), ..Default::default() };
        let props = inventory_properties(&inventory_doc(&properties, 5, 1));
        assert_eq!((props.owner_id, props.context.as_str(), props.subject_id, props.name.as_str()), (5, "Location", 42, "5:Location:42"));
        let given = InventoryNodeProperties { owner_id: 8, context: "Other".into(), ..properties };
        let props = inventory_properties(&inventory_doc(&given, 5, 1));
        assert_eq!((props.owner_id, props.context.as_str()), (8, "Other"));
    }

    #[test]
    fn a_volume_beyond_32_bits_reads_as_full_scale() {
        let doc = json!({ "configuration": { "physical": { "capacity": 5_000_000_000_i64, "occupancy": 3 } } });
        let physical = physical_configuration(&inventory_properties(&doc).configuration.unwrap()).unwrap();
        assert_eq!((physical.capacity, physical.occupancy), (i32::MAX, 3));
    }

    #[test]
    fn keys_escape_what_arango_rejects() {
        assert_eq!(key("1:Container:0"), "1:Container:0");
        assert_eq!(key("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn snapshot_flags_select_variables() {
        let doc = json!({ "_key": "9", "version": 2, "variables": [
            { "nameCrc": 1, "type": 0, "flags": 4, "snapshot": "ff00" },
            { "nameCrc": 2, "type": 0, "flags": 2, "snapshot": "" },
        ]});
        let snapshot = snapshot_message(&doc, 4, 0);
        assert_eq!(snapshot.entity_id, 9);
        assert_eq!(snapshot.variables.len(), 1);
        assert_eq!(snapshot.variables[0].snapshot, vec![0xff, 0]);
        assert_eq!(snapshot_message(&doc, 0, 4).variables[0].name_crc, 2);
    }
}
