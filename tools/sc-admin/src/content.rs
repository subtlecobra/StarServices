//! Reads game content out of the DataCore and the archive into plain rows. What each part means is
//! taken from the game data and the client, and nothing here makes content up.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use p4k_formats::{cryxml::Element, dcb::Dcb, xmlq};
use p4k::P4k;
use prost::Message;
use sc_api::internal::LocalizedText;
use sc_proto::{
    common::mission::v1::{ContractPrerequisites, MissionContract, MissionTemplate, ReputationRequirement},
    services::blueprint_library::v1::BlueprintProcessType,
};
use serde_json::{json, Value};

use crate::containers::{self, container_path, parse_socpak, System, Tree};

const DATACORE_PATH: &str = "Data/Game2.dcb";
const TEXT_PATH: &str = "Data/Localization/english/global.ini";
/// The record the client's own tooling reads the universe from, and the one that names every system.
const UNIVERSE_MEGAMAP: &str = "PU_All";
const NULL_GUID: &str = "00000000-0000-0000-0000-000000000000";
/// Deep enough to expand a contract generator down to its prerequisites.
const CONTRACT_DEPTH: usize = 12;
const CONTRACT_LISTS: [&str; 4] = ["contracts", "introContracts", "legacyContracts", "serviceBeaconContracts"];
const SCENARIO_DISABLED: i32 = 1;
const SCENARIO_SCHEDULED: i32 = 2;

pub struct Game {
    datacore: Dcb,
    archive: P4k,
    text: HashMap<String, String>,
    by_struct: HashMap<String, Vec<usize>>,
}

struct Record {
    index: usize,
    name: String,
    guid: String,
    data: Value,
}

pub struct Class {
    pub guid: String,
    pub name: String,
    pub crc: u32,
    pub item_type: String,
    pub item_sub_type: String,
    /// The DataCore `EItemType` and `EItemSubType` numbers of those names, 0 for a name the enums do not list.
    pub item_type_enum: i32,
    pub item_sub_type_enum: i32,
    /// The record carries the tag `Item.Inventory.Stackable`, which the client checks to let an item stack.
    pub stackable: bool,
    name_token: String,
    description_token: String,
    policy: Option<String>,
    /// The room one item takes in an inventory (`inventoryOccupancyVolume`), in micro-SCU.
    pub occupancy_micro_scu: Option<i64>,
    /// The `InventoryContainer` record of an item that holds an inventory (a crate, a backpack, a cargo grid).
    pub container: Option<String>,
    /// The `AttachDef` size and tags, which an item port checks (see "Equipping" in docs/INVENTORY.md).
    pub attach_size: Option<i32>,
    pub attach_tags: String,
    pub required_tags: String,
    /// The item ports of a class that carries gear (`ports_wanted`); none for any other class.
    pub ports: Vec<Port>,
    /// The size and insurance of a vehicle class (`VehicleComponentParams`, `shipInsuranceParams`); none for any other.
    pub vehicle: Option<Vehicle>,
}

/// What a vehicle class says about its size and its insurance.
pub struct Vehicle {
    /// `maxBoundingBoxSize` in metres: width, length, height.
    pub max_bounding_box: Option<[f64; 3]>,
    pub insurance: Option<Insurance>,
}

/// `ShipInsuranceParams`: the minutes a claim takes, the minutes of it that cannot be paid away, and the fee to do so.
pub struct Insurance {
    pub base_wait_time_minutes: f64,
    pub mandatory_wait_time_minutes: f64,
    pub base_expediting_fee: u32,
}

/// The vehicle part of a class: its component's bounding box and the insurance params next to its static class data.
fn vehicle_of(vehicle: Option<&Value>, data: &Value) -> Option<Vehicle> {
    let component = vehicle?;
    let size = &component["maxBoundingBoxSize"];
    let max_bounding_box = match (size["x"].as_f64(), size["y"].as_f64(), size["z"].as_f64()) {
        (Some(x), Some(y), Some(z)) if x > 0.0 && y > 0.0 && z > 0.0 => Some([x, y, z]),
        _ => None,
    };
    let insurance = data["StaticEntityClassData"].as_array().into_iter().flatten().find_map(|entry| {
        let params = &entry["shipInsuranceParams"];
        params.is_object().then(|| Insurance {
            base_wait_time_minutes: params["baseWaitTimeMinutes"].as_f64().unwrap_or(0.0),
            mandatory_wait_time_minutes: params["mandatoryWaitTimeMinutes"].as_f64().unwrap_or(0.0),
            base_expediting_fee: params["baseExpeditingFee"].as_u64().unwrap_or(0) as u32,
        })
    });
    Some(Vehicle { max_bounding_box, insurance })
}

/// An `SItemPortDef` of an `SItemPortContainerComponentParams`.
pub struct Port {
    pub name: String,
    pub min_size: i32,
    pub max_size: i32,
    pub types: Vec<TypeFilter>,
    pub required_port_tags: String,
    pub port_tags: String,
    pub flags: String,
}

/// The classes whose item ports are seeded: the body, armor, clothing, and personal weapons and gadgets, which are what a
/// player wears. A ship's or a vehicle's ports are not, and are a different thing than equipping a character.
fn ports_wanted(item_type: &str) -> bool {
    item_type.starts_with("Char_") || matches!(item_type, "WeaponPersonal" | "MobiGlas" | "Gadget")
}

/// The depth the DataCore reader needs to give the fields of a port (its types, sizes and tags).
const PORT_DEPTH: usize = 4;

fn ports_of(component: &Value) -> Vec<Port> {
    component["Ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|port| Port {
            name: text(port, "Name").to_string(),
            min_size: port["MinSize"].as_i64().unwrap_or(0) as i32,
            max_size: port["MaxSize"].as_i64().unwrap_or(0) as i32,
            types: port["Types"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|kind| TypeFilter {
                    item_type: text(kind, "Type").to_string(),
                    item_sub_types: kind["SubTypes"].as_array().into_iter().flatten().filter_map(|sub| sub.as_str()).filter(|sub| !sub.is_empty()).map(str::to_string).collect(),
                })
                .collect(),
            required_port_tags: text(port, "RequiredPortTags").to_string(),
            port_tags: text(port, "PortTags").to_string(),
            flags: text(port, "Flags").to_string(),
        })
        .collect()
}

/// An `InventoryContainer` record: the capacity of the inventory of the items that use it.
pub struct Container {
    pub capacity_micro_scu: i64,
    pub closed: bool,
    /// `excludedItemSubTypes`: item types (or only some subtypes of them) the container refuses.
    pub excluded: Vec<TypeFilter>,
    /// `overrideStorableItemTypes`: when not empty, the only types it takes.
    pub only: Vec<TypeFilter>,
}

/// An `InventoryContainerItemTypeFilter`: a type and the subtypes of it meant, all of them when none are listed.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeFilter {
    pub item_type: String,
    pub item_sub_types: Vec<String>,
}

fn type_filters(list: &Value) -> Vec<TypeFilter> {
    list.as_array()
        .map(|filters| {
            filters
                .iter()
                .map(|filter| TypeFilter {
                    item_type: text(filter, "itemType").to_string(),
                    item_sub_types: filter["itemSubTypes"].as_array().map(|subs| subs.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub struct Starter {
    pub class_guid: String,
    pub item_type: String,
    pub item_sub_type: String,
    pub name: String,
    pub source: String,
    pub port_path: String,
}

pub struct Scope {
    pub name: String,
    pub standings_id: String,
    pub display_name: String,
    pub description: String,
    pub initial: i32,
    pub ceiling: i32,
    standings: Vec<String>,
}

pub struct Standing {
    pub id: String,
    pub ladder: String,
    pub name: String,
    pub description: String,
    pub min: i32,
    pub drift_seconds: i64,
    pub drift_amount: i32,
    pub gated: bool,
}

pub struct Blueprint {
    pub id: String,
    pub category: String,
    pub item_class: String,
    pub process_type: i32,
}

pub struct Contract {
    pub id: String,
    pub body: Vec<u8>,
}

#[derive(Default)]
pub struct ContractSkips {
    pub not_for_release: usize,
    pub needs_scenario: usize,
}

/// The value the game keys a class by, in `class_guid_crc` and on the replication wire: the CRC-32C of
/// the GUID as the game holds it, two 64-bit halves in little endian. Checked against the real `Player`
/// and solar system ids, 2961494058 and 0x479c0d1d.
pub fn class_crc(guid: &str) -> u32 {
    let digits: Vec<u8> = guid.bytes().filter(u8::is_ascii_hexdigit).collect();
    let bytes: Vec<u8> = digits
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("0"), 16).unwrap_or(0))
        .collect();
    let mut crc = !0u32;
    for half in bytes.chunks(8) {
        for byte in half.iter().rev() {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
            }
        }
    }
    !crc
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or_default()
}

/// The GUID of a record reference, which the DataCore reads as `["ref", index, guid]`.
fn reference(value: &Value) -> Option<&str> {
    value[2].as_str().filter(|guid| *guid != NULL_GUID)
}

fn references(value: &Value) -> Vec<String> {
    let list = value.as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter().filter_map(reference).map(str::to_string).collect()
}

fn walk<'a>(value: &'a Value, visit: &mut dyn FnMut(&'a Value)) {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("__struct__") {
                visit(value);
            }
            fields.values().for_each(|field| walk(field, visit));
        }
        Value::Array(items) => items.iter().for_each(|item| walk(item, visit)),
        _ => {}
    }
}

fn parse_text(ini: &str) -> HashMap<String, String> {
    let lines = ini.trim_start_matches('\u{feff}').lines();
    lines
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.split(',').next().unwrap_or(key).to_ascii_lowercase(), value.trim_end().to_string()))
        .collect()
}

fn localized(token: &str, text: &HashMap<String, String>) -> LocalizedText {
    let en = text.get(&token.trim_start_matches('@').to_ascii_lowercase());
    LocalizedText { token: token.to_string(), en: en.cloned().unwrap_or_default() }
}

fn class_of(name: &str, guid: &str, data: &Value) -> Class {
    let components = data["Components"].as_array().into_iter().flatten();
    let params = components.chain(data["StaticEntityClassData"].as_array().into_iter().flatten());
    let (mut definition, mut policy, mut container, mut occupancy, mut port_params) = (&Value::Null, None, None, None, None);
    let mut vehicle_params: Option<&Value> = None;
    for component in params {
        match text(component, "__struct__") {
            "SAttachableComponentParams" => {
                definition = &component["AttachDef"];
                // In the item definition in 4.10.2; the component's own field is the fallback.
                occupancy = cargo_micro_scu(&definition["inventoryOccupancyVolume"])
                    .or_else(|| cargo_micro_scu(&component["inventoryOccupancyVolume"]));
            }
            "SItemPortContainerComponentParams" => port_params = Some(component),
            "VehicleComponentParams" => vehicle_params = Some(component),
            "DefaultEntitlementEntityParams" => {
                policy = reference(&component["entitlementPolicy"]).map(str::to_string)
            }
            "SCItemInventoryContainerComponentParams" => {
                container = reference(&component["containerParams"]).map(str::to_string)
            }
            _ => {}
        }
    }
    let localization = &definition["Localization"];
    Class {
        guid: guid.to_string(),
        name: name.to_string(),
        crc: class_crc(guid),
        item_type: text(definition, "Type").to_string(),
        item_sub_type: text(definition, "SubType").to_string(),
        item_type_enum: 0,
        item_sub_type_enum: 0,
        stackable: false,
        name_token: text(localization, "Name").to_string(),
        description_token: text(localization, "Description").to_string(),
        policy,
        occupancy_micro_scu: occupancy,
        container,
        attach_size: definition["Size"].as_i64().map(|size| size as i32),
        attach_tags: text(definition, "Tags").to_string(),
        required_tags: text(definition, "RequiredTags").to_string(),
        ports: port_params.filter(|_| ports_wanted(text(definition, "Type"))).map(ports_of).unwrap_or_default(),
        vehicle: vehicle_of(vehicle_params, data),
    }
}

/// A DataCore cargo volume (`SStandardCargoUnit`, `SCentiCargoUnit` or `SMicroCargoUnit`) in micro-SCU.
fn cargo_micro_scu(volume: &Value) -> Option<i64> {
    let number = |field: &str| volume[field].as_f64();
    let micro = match text(volume, "__struct__") {
        "SStandardCargoUnit" => number("standardCargoUnits")? * 1_000_000.0,
        "SCentiCargoUnit" => number("centiSCU")? * 10_000.0,
        "SMicroCargoUnit" => number("microSCU")?,
        _ => return None,
    };
    Some(micro.round() as i64)
}

/// The edge of the cube the game counts an open container's interior in (1.25 m, 0x148407298).
const SCU_EDGE_METRES: f64 = 1.25;

/// An `InventoryContainer`'s capacity as `InventoryContainer::PostLoad` (0x142f054c0) computes it: a closed container's
/// own capacity, or else the whole SCU cubes that fit in the interior.
fn container_of(data: &Value) -> Container {
    let kind = &data["inventoryType"];
    if text(kind, "__struct__") == "InventoryClosedContainerType" {
        let capacity = cargo_micro_scu(&kind["capacity"]).unwrap_or(0);
        return Container { capacity_micro_scu: capacity, closed: true, ..filtered(data) };
    }
    let size = &data["interiorDimensions"];
    let cubes = |axis: &str| (size[axis].as_f64().unwrap_or(0.0) / SCU_EDGE_METRES + 1e-6).floor().max(0.0);
    let capacity = cubes("x") * cubes("y") * cubes("z") * 1_000_000.0;
    Container { capacity_micro_scu: capacity as i64, closed: false, ..filtered(data) }
}

/// The type filters of a container record, with no capacity.
fn filtered(data: &Value) -> Container {
    Container {
        capacity_micro_scu: 0,
        closed: false,
        excluded: type_filters(&data["excludedItemSubTypes"]),
        only: type_filters(&data["inventoryType"]["overrideStorableItemTypes"]),
    }
}

/// The classes every player owns: those under a policy that grants all modes to everyone.
fn is_everyone(policy: &Value) -> bool {
    !policy["subscribersOnly"].as_bool().unwrap_or(true) && policy["Entitlements"] == json!(["AllModes"])
}

/// The items of a loadout, with the chain of ports each sits on. A port with no item is only a path.
fn loadout_items(items: &[Element], path: &str, found: &mut Vec<(String, String)>) {
    for item in items.iter().filter(|item| item.tag == "Item") {
        let port = item.get("portName").unwrap_or_default();
        let port_path = if path.is_empty() { port.to_string() } else { format!("{path}/{port}") };
        if let Some(class) = item.get("itemName").filter(|class| !class.is_empty()) {
            found.push((class.to_string(), port_path.clone()));
        }
        if let Some(children) = item.find("Items") {
            loadout_items(&children.children, &port_path, found);
        }
    }
}

fn standings_between(ladder: &[(String, i32)], min: Option<&str>, max: Option<&str>) -> Vec<String> {
    let rung = |id: Option<&str>| id.and_then(|id| ladder.iter().find(|(known, _)| known == id)).map(|(_, value)| *value);
    let (low, high) = (rung(min).unwrap_or(i32::MIN), rung(max).unwrap_or(i32::MAX));
    ladder.iter().filter(|(_, value)| (low..=high).contains(value)).map(|(id, _)| id.clone()).collect()
}

struct Ladders {
    scopes: HashMap<String, (String, Vec<(String, i32)>)>,
    factions: HashMap<String, u64>,
}

impl Ladders {
    /// The client never shows how it names an organisation, so the faction's GEID stands in. A contract
    /// asking for reputation therefore stays closed until that format is known.
    fn requirement(&self, faction: Option<&str>, scope: Option<&str>, min: Option<&str>, max: Option<&str>) -> Option<ReputationRequirement> {
        let (scope_name, ladder) = self.scopes.get(scope?)?;
        let geid = self.factions.get(faction?)?;
        Some(ReputationRequirement {
            organisation_urn: format!("urn:sc:global:faction:geid:{geid}"),
            scope_name: scope_name.clone(),
            standing_ids: standings_between(ladder, min, max),
        })
    }

    fn prerequisites(&self, handler: &Value, contract: &Value) -> ContractPrerequisites {
        let mut found = ContractPrerequisites::default();
        let default = handler["defaultAvailability"]["prerequisites"].as_array().into_iter().flatten();
        for prerequisite in default.chain(contract["additionalPrerequisites"].as_array().into_iter().flatten()) {
            if text(prerequisite, "__struct__") != "ContractPrerequisite_Reputation" {
                continue;
            }
            let requirement = self.requirement(
                reference(&prerequisite["factionReputation"]),
                reference(&prerequisite["scope"]),
                reference(&prerequisite["minStanding"]),
                reference(&prerequisite["maxStanding"]),
            );
            let list = match prerequisite["exclude"].as_bool() {
                Some(true) => &mut found.excluded_reputation,
                _ => &mut found.required_reputation,
            };
            list.extend(requirement);
        }
        // A career contract is ranked on the handler's own faction and scope.
        if text(contract, "__struct__") == "CareerContract" {
            found.required_reputation.extend(self.requirement(
                reference(&handler["factionReputation"]),
                reference(&handler["reputationScope"]),
                reference(&contract["minStanding"]),
                reference(&contract["maxStanding"]),
            ));
        }
        found
    }
}

fn releasable(value: &Value) -> bool {
    value["notForRelease"].as_bool() != Some(true) && value["workInProgress"].as_bool() != Some(true)
}

fn needs_scenario(value: &Value) -> bool {
    value["required_active_scenarios"].as_array().is_some_and(|scenarios| !scenarios.is_empty())
}

fn contract_message(id: &str, contract: &Value, prerequisites: ContractPrerequisites) -> MissionContract {
    let limit = |field: &str| contract["generationParams"][field].as_i64().unwrap_or(0) as i32;
    MissionContract {
        id: id.to_string(),
        mission_template: Some(MissionTemplate {
            contract_definition_id: id.to_string(),
            max_instances_per_player: limit("maxInstancesPerPlayer"),
            max_concurrent: limit("maxInstances"),
            ..Default::default()
        }),
        prerequisites: Some(prerequisites),
        count: 1,
        ..Default::default()
    }
}

impl Game {
    pub fn open(path: &Path) -> Result<Self, String> {
        let archive = P4k::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let read = |name: &str| {
            let entry = archive.get(name).ok_or_else(|| format!("{name} not in archive"))?;
            archive.read(entry).map_err(|e| format!("read {name}: {e}"))
        };
        let datacore = Dcb::open(read(DATACORE_PATH)?).map_err(|e| format!("parse {DATACORE_PATH}: {e}"))?;
        let text = parse_text(&String::from_utf8_lossy(&read(TEXT_PATH)?));
        let total: usize = datacore.record_index().values().map(Vec::len).sum();
        let mut by_struct: HashMap<String, Vec<usize>> = HashMap::new();
        for index in 0..total {
            let record = datacore.record(index).map_err(|e| format!("record {index}: {e}"))?;
            by_struct.entry(record.struct_name).or_default().push(index);
        }
        Ok(Self { datacore, archive, text, by_struct })
    }

    fn records(&self, struct_name: &str, depth: usize) -> Result<Vec<Record>, String> {
        let indexes = self.by_struct.get(struct_name).map(Vec::as_slice).unwrap_or_default();
        indexes
            .iter()
            .map(|index| {
                let record = self.datacore.record(*index).map_err(|e| format!("record {index}: {e}"))?;
                let data = self.datacore.record_data(&record, depth).map_err(|e| format!("{}: {e}", record.name))?;
                let name = record.name.split_once('.').map_or(record.name.as_str(), |(_, name)| name).to_string();
                Ok(Record { index: *index, name, guid: record.guid, data: data.to_json() })
            })
            .collect()
    }

    /// The shop inventory files, as `(path, bytes)`.
    pub fn shop_inventories(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
        let entries: Vec<_> = self.archive.find("Scripts/ShopInventories/", &[".json"]).collect();
        entries
            .into_iter()
            .map(|entry| Ok((entry.name.to_string(), self.archive.read(entry).map_err(|e| format!("read {}: {e}", entry.name))?)))
            .collect()
    }

    /// The object container tree of the persistent universe.
    pub fn universe(&self) -> Result<Tree, String> {
        let names: HashMap<String, String> = self
            .records("SSolarSystem", 1)?
            .into_iter()
            .map(|record| (record.guid, text(&record.data, "Name").to_string()))
            .collect();
        let maps = self.records("MegaMap", 3)?;
        let universe = maps.iter().find(|map| map.name == UNIVERSE_MEGAMAP);
        let universe = universe.ok_or_else(|| format!("no MegaMap.{UNIVERSE_MEGAMAP} in {DATACORE_PATH}"))?;
        let mut systems = Vec::new();
        for system in universe.data["SolarSystems"].as_array().into_iter().flatten() {
            let guid = reference(&system["Record"]).ok_or("a solar system of the universe has no record")?;
            let name = names.get(guid).ok_or_else(|| format!("no SSolarSystem record {guid}"))?;
            let socpaks = system["ObjectContainers"].as_array().into_iter().flatten();
            systems.push(System {
                record_guid: guid.to_string(),
                name: name.clone(),
                socpaks: socpaks.filter_map(Value::as_str).map(str::to_string).collect(),
            });
        }
        let found = self.archive.find("", &[".socpak"]);
        let entries: HashMap<_, _> =
            found.filter_map(|entry| Some((container_path(&entry.name)?, entry))).collect();
        containers::build(&systems, |path| {
            let Some(entry) = entries.get(path) else { return Ok(None) };
            let bytes = self.archive.read(entry).map_err(|e| format!("read {path}: {e}"))?;
            parse_socpak(&bytes).map(Some).map_err(|e| format!("{path}: {e}"))
        })
    }

    pub fn classes(&self) -> Result<Vec<Class>, String> {
        let records = self.records("EntityClassDefinition", 1)?;
        let (types, sub_types) = (self.enum_numbers("EItemType")?, self.enum_numbers("EItemSubType")?);
        let stackable = self.stackable_tags()?;
        records
            .iter()
            .map(|record| {
                let mut class = class_of(&record.name, &record.guid, &record.data);
                class.item_type_enum = types.get(&class.item_type).copied().unwrap_or(0);
                class.item_sub_type_enum = sub_types.get(&class.item_sub_type).copied().unwrap_or(0);
                class.stackable = references(&record.data["tags"]).iter().any(|tag| stackable.contains(tag));
                // The ports are nested deeper than the rest of the record the other classes are read at.
                if !class.ports.is_empty() {
                    let entry = self.datacore.record(record.index).map_err(|e| format!("record {}: {e}", record.index))?;
                    let deep = self.datacore.record_data(&entry, PORT_DEPTH).map_err(|e| format!("{}: {e}", record.name))?;
                    class.ports = class_of(&record.name, &record.guid, &deep.to_json()).ports;
                }
                Ok(class)
            })
            .collect()
    }

    /// The GUIDs of the tag `Item.Inventory.Stackable`: a `Stackable` tag under an `Inventory` tag under `Item`.
    pub fn stackable_tags(&self) -> Result<HashSet<String>, String> {
        let tags = self.records("Tag", 1)?;
        let name = |guid: &str| tags.iter().find(|tag| tag.guid == guid).map(|tag| text(&tag.data, "tagName").to_string());
        let parents = |guid: &str| -> Vec<String> {
            tags.iter().filter(|tag| references(&tag.data["children"]).iter().any(|child| child == guid)).map(|tag| tag.guid.clone()).collect()
        };
        let named = |guids: Vec<String>, wanted: &str| -> Vec<String> { guids.into_iter().filter(|guid| name(guid).as_deref() == Some(wanted)).collect() };
        let mut found = HashSet::new();
        for tag in tags.iter().filter(|tag| text(&tag.data, "tagName") == "Stackable") {
            let inventory = named(parents(&tag.guid), "Inventory");
            if inventory.iter().any(|guid| !named(parents(guid), "Item").is_empty()) {
                found.insert(tag.guid.clone());
            }
        }
        if found.is_empty() {
            return Err("no tag Item.Inventory.Stackable in the DataCore".into());
        }
        Ok(found)
    }

    /// The numbers of a DataCore enum's options, which are their positions in its definition: the client checks an
    /// item's `EItemType` and `EItemSubType` against them.
    pub fn enum_numbers(&self, enum_name: &str) -> Result<HashMap<String, i32>, String> {
        for index in 0..self.datacore.n_enum {
            let (name, options) = self.datacore.enum_def(index).map_err(|e| format!("enum {index}: {e}"))?;
            if name == enum_name {
                return Ok(options.into_iter().enumerate().map(|(number, option)| (option, number as i32)).collect());
            }
        }
        Err(format!("no enum {enum_name} in the DataCore"))
    }

    /// The `InventoryContainer` records by GUID.
    pub fn containers(&self) -> Result<HashMap<String, Container>, String> {
        let records = self.records("InventoryContainer", 3)?;
        Ok(records.into_iter().map(|record| (record.guid, container_of(&record.data))).collect())
    }

    /// The `ItemBankContainer` record, whose filters apply to every inventory of the item banks at the locations.
    pub fn item_bank(&self) -> Result<Option<Container>, String> {
        let records = self.records("InventoryContainer", 4)?;
        Ok(records.into_iter().find(|record| record.name == "ItemBankContainer").map(|record| container_of(&record.data)))
    }

    /// A class's name and description: the localization token and its English text, which `entity_classes` keeps.
    pub fn class_text(&self, class: &Class) -> (Option<LocalizedText>, Option<LocalizedText>) {
        (Some(localized(&class.name_token, &self.text)), Some(localized(&class.description_token, &self.text)))
    }

    fn loadout(&self, path: &str) -> Result<Vec<(String, String)>, String> {
        let entry = self.archive.find_path(path, "xml").ok_or_else(|| format!("{path} not in archive"))?;
        let bytes = self.archive.read(entry).map_err(|e| format!("read {path}: {e}"))?;
        let root = xmlq::parse_any(&bytes).map_err(|e| format!("parse {path}: {e}"))?;
        let mut found = Vec::new();
        if let Some(included) = root.find("Loadouts") {
            for loadout in &included.children {
                found.extend(self.loadout(loadout.get("loadout").unwrap_or_default())?);
            }
        }
        if let Some(items) = root.find("Items") {
            loadout_items(&items.children, "", &mut found);
        }
        Ok(found)
    }

    /// What a new player owns: classes granted to everyone, and the default PU loadout. Returns the rows
    /// and the loadout items whose class the DataCore does not have.
    pub fn starter(&self, classes: &[Class]) -> Result<(Vec<Starter>, Vec<String>), String> {
        let everyone: Vec<String> = self
            .records("DefaultEntitlementRecord", 1)?
            .into_iter()
            .filter(|policy| is_everyone(&policy.data))
            .map(|policy| policy.guid)
            .collect();
        let starter = |class: &Class, source: &str, port_path: &str| Starter {
            class_guid: class.guid.clone(),
            item_type: class.item_type.clone(),
            item_sub_type: class.item_sub_type.clone(),
            name: class.name.clone(),
            source: source.to_string(),
            port_path: port_path.to_string(),
        };
        let mut rows: BTreeMap<String, Starter> = BTreeMap::new();
        for class in classes.iter().filter(|class| class.policy.as_ref().is_some_and(|policy| everyone.contains(policy))) {
            rows.insert(class.guid.clone(), starter(class, "policy", ""));
        }
        let by_name: HashMap<String, &Class> = classes.iter().map(|class| (class.name.to_ascii_lowercase(), class)).collect();
        let defaults = self.records("DefaultPlayerLoadoutEntitlementRecord", 3)?;
        let loadouts = defaults.iter().flat_map(|record| record.data["Loadouts"].as_array().into_iter().flatten());
        let path = loadouts
            .filter(|loadout| text(loadout, "Name") == "Default")
            .map(|loadout| text(&loadout["Loadout"], "loadoutPath"))
            .next()
            .ok_or("no default player loadout in the DataCore")?;
        let mut missing = Vec::new();
        for (name, port_path) in self.loadout(path)? {
            match by_name.get(&name.to_ascii_lowercase()) {
                Some(class) => {
                    rows.insert(class.guid.clone(), starter(class, "loadout", &port_path));
                }
                None => missing.push(name),
            }
        }
        Ok((rows.into_values().collect(), missing))
    }

    pub fn reputation(&self) -> Result<(Vec<Scope>, Vec<Standing>), String> {
        let scopes: Vec<Scope> = self
            .records("SReputationScopeParams", 3)?
            .iter()
            .map(|record| {
                let map = &record.data["standingMap"];
                Scope {
                    name: text(&record.data, "scopeName").to_string(),
                    standings_id: record.guid.clone(),
                    display_name: text(&record.data, "displayName").to_string(),
                    description: text(&record.data, "description").to_string(),
                    initial: map["initialReputation"].as_i64().unwrap_or(0) as i32,
                    ceiling: map["reputationCeiling"].as_i64().unwrap_or(0) as i32,
                    standings: references(&map["standings"]),
                }
            })
            .collect();
        let mut first_ladder: HashMap<&str, &str> = HashMap::new();
        for scope in &scopes {
            for standing in &scope.standings {
                first_ladder.entry(standing).or_insert(&scope.standings_id);
            }
        }
        let standings = self
            .records("SReputationStandingParams", 1)?
            .iter()
            .filter_map(|record| {
                let ladder = first_ladder.get(record.guid.as_str())?;
                Some(Standing {
                    id: record.guid.clone(),
                    ladder: ladder.to_string(),
                    name: text(&record.data, "name").to_string(),
                    description: text(&record.data, "perkDescription").to_string(),
                    min: record.data["minReputation"].as_i64().unwrap_or(0) as i32,
                    drift_seconds: (record.data["driftTimeHours"].as_f64().unwrap_or(0.0) * 3600.0) as i64,
                    drift_amount: record.data["driftReputation"].as_i64().unwrap_or(0) as i32,
                    gated: record.data["gated"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        Ok((scopes, standings))
    }

    /// The blueprints CraftingGlobalParams gives every player. Every other one is a mission reward.
    pub fn default_blueprints(&self) -> Result<Vec<Blueprint>, String> {
        let params = self.records("CraftingGlobalParams", 2)?;
        let wanted: Vec<String> = params
            .iter()
            .flat_map(|record| references(&record.data["defaultBlueprintSelection"]["blueprintRecords"]))
            .collect();
        let records = self.records("CraftingBlueprintRecord", 3)?;
        Ok(records
            .iter()
            .filter(|record| wanted.contains(&record.guid))
            .map(|record| {
                let blueprint = &record.data["blueprint"];
                let process = &blueprint["processSpecificData"];
                let process_type = match text(process, "__struct__") {
                    "GenericCraftingProcess_Dismantle" => BlueprintProcessType::Dismantle,
                    "CraftingProcess_Creation" => BlueprintProcessType::Create,
                    _ => BlueprintProcessType::Unspecified,
                };
                Blueprint {
                    id: record.guid.clone(),
                    category: reference(&blueprint["category"]).unwrap_or_default().to_string(),
                    item_class: reference(&process["entityClass"]).unwrap_or_default().to_string(),
                    process_type: process_type as i32,
                }
            })
            .collect())
    }

    /// Scenario ids and statuses. Nothing here starts one: the game data leaves that to an operator or
    /// a schedule, and this server has no scheduler.
    pub fn scenarios(&self) -> Result<Vec<(String, i32)>, String> {
        let records = self.records("MissionScenario", 2)?;
        let status = |record: &Record| match record.data["schedule"]["enabled"].as_bool() {
            Some(true) => SCENARIO_SCHEDULED,
            _ => SCENARIO_DISABLED,
        };
        Ok(records.iter().map(|record| (record.guid.clone(), status(record))).collect())
    }

    fn ladders(&self, scopes: &[Scope], standings: &[Standing]) -> Result<Ladders, String> {
        let minimum: HashMap<&str, i32> = standings.iter().map(|standing| (standing.id.as_str(), standing.min)).collect();
        let ladder = |scope: &Scope| -> Vec<(String, i32)> {
            let rungs = scope.standings.iter().filter_map(|id| Some((id.clone(), *minimum.get(id.as_str())?)));
            rungs.collect()
        };
        let factions = self.records("FactionReputation", 1)?;
        Ok(Ladders {
            scopes: scopes.iter().map(|scope| (scope.standings_id.clone(), (scope.name.clone(), ladder(scope)))).collect(),
            factions: factions.iter().filter_map(|record| Some((record.guid.clone(), record.data["GEID"].as_u64()?))).collect(),
        })
    }

    /// Contracts a release build offers, one per definition. A contract gated on a scenario is left out,
    /// since `MissionContract` cannot carry that condition and no scenario is running.
    pub fn contracts(&self, scopes: &[Scope], standings: &[Standing]) -> Result<(Vec<Contract>, ContractSkips), String> {
        let ladders = self.ladders(scopes, standings)?;
        let mut rows: BTreeMap<String, Contract> = BTreeMap::new();
        let mut skips = ContractSkips::default();
        for generator in self.records("ContractGenerator", CONTRACT_DEPTH)? {
            let mut handlers = Vec::new();
            walk(&generator.data, &mut |value| {
                if text(value, "__struct__").starts_with("ContractGeneratorHandler") {
                    handlers.push(value);
                }
            });
            for handler in handlers {
                let lists = CONTRACT_LISTS.iter().filter_map(|list| handler[*list].as_array());
                for contract in lists.flatten() {
                    let id = text(contract, "id");
                    if id.is_empty() {
                        continue;
                    }
                    if !releasable(handler) || !releasable(contract) {
                        skips.not_for_release += 1;
                    } else if needs_scenario(handler) || needs_scenario(contract) {
                        skips.needs_scenario += 1;
                    } else {
                        let message = contract_message(id, contract, ladders.prerequisites(handler, contract));
                        rows.insert(id.to_string(), Contract { id: id.to_string(), body: message.encode_to_vec() });
                    }
                }
            }
        }
        Ok((rows.into_values().collect(), skips))
    }
}

impl Scope {
    pub fn ladder_rows(&self) -> impl Iterator<Item = (&str, &str)> {
        self.standings.iter().map(|standing| (self.standings_id.as_str(), standing.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(tag: &str, attrs: &[(&str, &str)], children: Vec<Element>) -> Element {
        let attrs = attrs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
        Element { tag: tag.to_string(), attrs, children, ..Default::default() }
    }

    #[test]
    fn class_crcs_match_the_ids_the_real_game_uses() {
        assert_eq!(class_crc("5287b146-fa24-49b8-8126-326164217c4f"), 2_961_494_058);
        assert_eq!(class_crc("2a0f6bf7-730d-430c-a5d6-6005d45e9625"), 0x479c_0d1d);
    }

    #[test]
    fn a_class_reads_its_occupancy_and_container() {
        let data = json!({
            "Components": [
                {
                    "__struct__": "SAttachableComponentParams",
                    "AttachDef": {
                        "Type": "Container", "SubType": "Cargo",
                        "inventoryOccupancyVolume": { "__struct__": "SStandardCargoUnit", "standardCargoUnits": 0.5 },
                    },
                },
                {
                    "__struct__": "SCItemInventoryContainerComponentParams",
                    "containerParams": ["ref", 74, "fd3cea55-e4ec-48d4-b857-eed642f981a6"],
                },
            ],
        });
        let class = class_of("crate", "5287b146-fa24-49b8-8126-326164217c4f", &data);
        assert_eq!(class.occupancy_micro_scu, Some(500_000));
        assert_eq!(class.container.as_deref(), Some("fd3cea55-e4ec-48d4-b857-eed642f981a6"));
        let pistol = json!({ "Components": [{ "__struct__": "SAttachableComponentParams", "AttachDef": {
            "inventoryOccupancyVolume": { "__struct__": "SMicroCargoUnit", "microSCU": 1 } } }] });
        let pistol = class_of("pistol", "5287b146-fa24-49b8-8126-326164217c4f", &pistol);
        assert_eq!((pistol.occupancy_micro_scu, pistol.container), (Some(1), None));
    }

    #[test]
    fn a_container_has_its_closed_capacity_or_whole_scu_cubes() {
        let closed = json!({ "inventoryType": { "__struct__": "InventoryClosedContainerType",
            "capacity": { "__struct__": "SCentiCargoUnit", "centiSCU": 250 } } });
        let closed = container_of(&closed);
        assert_eq!((closed.capacity_micro_scu, closed.closed), (2_500_000, true));
        let open = json!({ "inventoryType": { "__struct__": "InventoryOpenContainerType" },
            "interiorDimensions": { "x": 2.5, "y": 1.25, "z": 3.0 } });
        assert_eq!(container_of(&open).capacity_micro_scu, 4_000_000);
    }

    #[test]
    fn a_class_reads_its_item_definition_and_policy() {
        let data = json!({
            "StaticEntityClassData": [{
                "__struct__": "DefaultEntitlementEntityParams",
                "entitlementPolicy": ["ref", 1, "d0722bc1-600f-4e43-be02-ee071c9fc83d"],
            }],
            "Components": [{
                "__struct__": "SAttachableComponentParams",
                "AttachDef": { "Type": "Char_Head", "SubType": "Male", "Localization": { "Name": "@n", "Description": "@d" } },
            }],
        });
        let class = class_of("head", "5287b146-fa24-49b8-8126-326164217c4f", &data);
        assert_eq!((class.item_type.as_str(), class.item_sub_type.as_str()), ("Char_Head", "Male"));
        assert_eq!(class.policy.as_deref(), Some("d0722bc1-600f-4e43-be02-ee071c9fc83d"));
        assert_eq!(class.crc, 2_961_494_058);
    }

    #[test]
    fn only_an_all_modes_policy_open_to_everyone_counts() {
        assert!(is_everyone(&json!({ "Entitlements": ["AllModes"], "subscribersOnly": false })));
        assert!(!is_everyone(&json!({ "Entitlements": ["AllModes"], "subscribersOnly": true })));
        assert!(!is_everyone(&json!({ "Entitlements": ["Offline"], "subscribersOnly": false })));
    }

    #[test]
    fn a_null_reference_is_none() {
        assert_eq!(reference(&json!(["ref", 4294967295u32, NULL_GUID])), None);
        assert_eq!(reference(&json!(["ref", 1, "a"])), Some("a"));
    }

    #[test]
    fn loadout_items_keep_their_port_path_and_skip_empty_ports() {
        let helmet = element("Item", &[("portName", "Armor_Helmet"), ("itemName", "helmet")], vec![]);
        let suit = element("Item", &[("portName", "Armor_Undersuit"), ("itemName", "suit")], vec![element("Items", &[], vec![helmet])]);
        let body = element("Item", &[("portName", "Body"), ("itemName", "")], vec![element("Items", &[], vec![suit])]);
        let mut found = Vec::new();
        loadout_items(&[body], "", &mut found);
        assert_eq!(
            found,
            [
                ("suit".to_string(), "Body/Armor_Undersuit".to_string()),
                ("helmet".to_string(), "Body/Armor_Undersuit/Armor_Helmet".to_string()),
            ]
        );
    }

    #[test]
    fn a_standing_range_is_inclusive_and_open_when_unset() {
        let ladder = [("a".to_string(), 0), ("b".to_string(), 10), ("c".to_string(), 20)];
        assert_eq!(standings_between(&ladder, Some("b"), None), ["b", "c"]);
        assert_eq!(standings_between(&ladder, Some("a"), Some("b")), ["a", "b"]);
        assert_eq!(standings_between(&ladder, None, None).len(), 3);
    }

    #[test]
    fn unreleased_and_scenario_gated_contracts_are_recognised() {
        assert!(releasable(&json!({ "notForRelease": false, "workInProgress": false })));
        assert!(!releasable(&json!({ "notForRelease": true })));
        assert!(!releasable(&json!({ "workInProgress": true })));
        assert!(needs_scenario(&json!({ "required_active_scenarios": [["ref", 1, "x"]] })));
        assert!(!needs_scenario(&json!({ "required_active_scenarios": [] })));
    }

    #[test]
    fn a_contract_names_its_definition_and_limits() {
        let contract = json!({ "generationParams": { "maxInstancesPerPlayer": 2, "maxInstances": 5 } });
        let message = contract_message("id", &contract, ContractPrerequisites::default());
        let template = message.mission_template.unwrap();
        assert_eq!(template.contract_definition_id, "id");
        assert_eq!((template.max_instances_per_player, template.max_concurrent), (2, 5));
    }

    #[test]
    fn a_contract_prerequisite_becomes_a_requirement() {
        let ladders = Ladders {
            scopes: HashMap::from([("s".to_string(), ("Scope".to_string(), vec![("a".to_string(), 0), ("b".to_string(), 10)]))]),
            factions: HashMap::from([("f".to_string(), 7)]),
        };
        let handler = json!({ "defaultAvailability": { "prerequisites": [{
            "__struct__": "ContractPrerequisite_Reputation",
            "factionReputation": ["ref", 1, "f"],
            "scope": ["ref", 1, "s"],
            "minStanding": ["ref", 1, "b"],
            "maxStanding": ["ref", 1, NULL_GUID],
            "exclude": false,
        }, { "__struct__": "ContractPrerequisite_CrimeStat" }] } });
        let found = ladders.prerequisites(&handler, &json!({}));
        assert_eq!(found.required_reputation.len(), 1);
        assert_eq!(found.required_reputation[0].scope_name, "Scope");
        assert_eq!(found.required_reputation[0].standing_ids, ["b"]);
    }

    #[test]
    fn text_keys_ignore_case_and_suffixes() {
        let text = parse_text("\u{feff}Item_Name,P=Pistol\nother=x\n");
        assert_eq!(localized("@item_name", &text).en, "Pistol");
        assert_eq!(localized("@missing", &text).en, "");
    }
}


#[cfg(test)]
mod enum_tests {
    use super::*;

    /// With the game archive (`SC_P4K`): the numbers a 4.10.2 client reports for the item types of its worn items.
    #[test]
    #[ignore]
    fn item_enums_numbers_match_what_the_client_reports() {
        let path = std::env::var("SC_P4K").expect("SC_P4K");
        let game = Game::open(Path::new(&path)).unwrap();
        let types = game.enum_numbers("EItemType").unwrap();
        for (name, number) in [("Char_Body", 27), ("Char_Head", 42), ("MobiGlas", 132), ("Char_Armor_Undersuit", 25), ("Display", 65), ("Char_Armor_Helmet", 22), ("FPS_Consumable", 85)] {
            assert_eq!(types[name], number, "{name}");
        }
        let sub_types = game.enum_numbers("EItemSubType").unwrap();
        for (name, number) in [("Fluid", 54), ("UNDEFINED", 154), ("Personal", 110), ("Male", 91), ("MedPack", 97), ("Helmet", 70)] {
            assert_eq!(sub_types[name], number, "{name}");
        }
    }

    /// With the game archive (`SC_P4K`): the classes the game tags `Item.Inventory.Stackable` include its grenades.
    #[test]
    #[ignore]
    fn grenades_are_stackable_and_rifles_are_not() {
        let path = std::env::var("SC_P4K").expect("SC_P4K");
        let game = Game::open(Path::new(&path)).unwrap();
        let classes = game.classes().unwrap();
        let grenades: Vec<&Class> = classes.iter().filter(|class| class.item_type == "WeaponPersonal" && class.item_sub_type == "Grenade").collect();
        assert!(!grenades.is_empty() && grenades.iter().all(|class| class.stackable), "every grenade stacks");
        assert!(classes.iter().filter(|class| class.item_type == "WeaponPersonal" && class.item_sub_type == "Large").all(|class| !class.stackable), "a rifle does not");
    }

    #[test]
    fn a_gear_class_keeps_its_size_tags_and_ports() {
        let data = json!({ "Components": [
            { "__struct__": "SAttachableComponentParams", "AttachDef": { "Type": "Char_Armor_Undersuit", "SubType": "UNDEFINED", "Size": 1, "Tags": "Odyssey $armor_mobi++", "RequiredTags": "" } },
            { "__struct__": "SItemPortContainerComponentParams", "Ports": [{
                "Name": "wep_sidearm", "MinSize": 1, "MaxSize": 2, "RequiredPortTags": "", "PortTags": "", "Flags": "body_attach inventory",
                "Types": [{ "Type": "WeaponPersonal", "SubTypes": ["Small", ""] }],
            }] },
        ] });
        let class = class_of("suit", "5287b146-fa24-49b8-8126-326164217c4f", &data);
        assert_eq!((class.attach_size, class.attach_tags.as_str()), (Some(1), "Odyssey $armor_mobi++"));
        assert_eq!(class.ports.len(), 1);
        let port = &class.ports[0];
        assert_eq!((port.name.as_str(), port.min_size, port.max_size), ("wep_sidearm", 1, 2));
        assert_eq!(port.types, [TypeFilter { item_type: "WeaponPersonal".into(), item_sub_types: vec!["Small".into()] }], "an empty sub type is dropped");
        // A ship part's ports are not seeded.
        let ship = json!({ "Components": [
            { "__struct__": "SAttachableComponentParams", "AttachDef": { "Type": "Turret", "SubType": "GunTurret", "Size": 3 } },
            { "__struct__": "SItemPortContainerComponentParams", "Ports": [{ "Name": "gun", "MinSize": 1, "MaxSize": 3 }] },
        ] });
        assert!(class_of("turret", "5287b146-fa24-49b8-8126-326164217c4f", &ship).ports.is_empty());
    }

    /// With the game archive (`SC_P4K`): the body and an undersuit carry the ports equipping uses.
    #[test]
    #[ignore]
    fn the_body_and_undersuits_carry_their_ports() {
        let path = std::env::var("SC_P4K").expect("SC_P4K");
        let game = Game::open(Path::new(&path)).unwrap();
        let classes = game.classes().unwrap();
        let by_name = |name: &str| classes.iter().find(|class| class.name == name).expect(name);
        let body = by_name("body_01_noMagicPocket");
        assert!(body.ports.iter().any(|port| port.name == "Armor_Undersuit" && port.types.iter().any(|kind| kind.item_type == "Char_Armor_Undersuit")));
        let suit = by_name("rsi_odyssey_undersuit_01_01_01");
        let sidearm = suit.ports.iter().find(|port| port.name == "wep_sidearm").expect("wep_sidearm");
        assert_eq!((sidearm.min_size, sidearm.max_size), (1, 1));
        let pistol = by_name("klwe_pistol_energy_01");
        assert_eq!((pistol.item_type.as_str(), pistol.item_sub_type.as_str(), pistol.attach_size), ("WeaponPersonal", "Small", Some(1)));
    }

    /// With the game archive (`SC_P4K`): a ship class carries its bounding box and its insurance, other classes neither.
    #[test]
    #[ignore]
    fn a_ship_class_keeps_its_size_and_insurance() {
        let path = std::env::var("SC_P4K").expect("SC_P4K");
        let game = Game::open(Path::new(&path)).unwrap();
        let classes = game.classes().unwrap();
        let titan = classes.iter().find(|class| class.name == "AEGS_Avenger_Titan").expect("the Avenger Titan");
        let vehicle = titan.vehicle.as_ref().expect("a vehicle");
        assert_eq!(vehicle.max_bounding_box, Some([15.0, 20.0, 6.5]));
        let insurance = vehicle.insurance.as_ref().expect("insured");
        assert_eq!((insurance.base_expediting_fee, (insurance.base_wait_time_minutes * 100.0).round()), (2343, 405.0));
        assert!(classes.iter().filter(|class| class.item_type == "WeaponPersonal").all(|class| class.vehicle.is_none()));
        let sized = classes.iter().filter(|class| class.vehicle.as_ref().is_some_and(|v| v.max_bounding_box.is_some())).count();
        let insured = classes.iter().filter(|class| class.vehicle.as_ref().is_some_and(|v| v.insurance.is_some())).count();
        println!("vehicle classes with a size: {sized}, with insurance: {insured}");
        assert!(sized > 50 && insured > 50);
    }
}
