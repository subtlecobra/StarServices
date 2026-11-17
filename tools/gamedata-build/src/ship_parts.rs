//! The parts of every vehicle of the build, `tables/ship_parts.bin`: what a server binds with a ship, in the same batch, when
//! it spawns one (docs/SHIPS.md, "A ship's parts"; the format is in docs/DATA.md). The DGS reads it to bind a retrieved
//! ship whole instead of as a bare hull.
//!
//! A vehicle is an `EntityClassDefinition` with a `VehicleComponentParams`. Its parts are its default loadout, as a tree:
//!
//! - every entry of the class's `SEntityComponentDefaultLoadoutParams` loadout that names a class (by
//!   `entityClassReference`, else by `entityClassName`) is a part, in the port `itemPortName` of its parent;
//! - its own parts are the entry's `loadout` when it has one, else the default loadout of the part's class, at any depth;
//! - a loadout is either a list of entries (`SItemPortLoadoutManualParams`) or an XML file (`SItemPortLoadoutXMLParams`,
//!   `<Item portName itemName>`, nested in `<Items>`), which is in the encrypted `Data/Scripts`: it is read only when the
//!   archive's key is at hand (`gamedata-build --exe`), and counted as unread otherwise;
//! - a port of the class (`SItemPortContainerComponentParams`) that the loadout leaves empty and that has a
//!   `defaultItem` gets that item, with the item's own default loadout.
//!
//! A part is marked `FLAG_DEFAULT_OF_PARENT` (kept in the table, which the DGS binds whole, but with no graph entity and
//! the bit masked out of its bind) when the client spawns it itself, which makes its own loadout setup
//! refuse it as `DuplicatedItemPorts`: it comes only from the default loadout of the class of a part (not of the ship)
//! whose own entry has no inline loadout, never from a port's `defaultItem` nor an inline loadout, or it descends from such
//! a part. This holds on the DataCore for the Marauder (6 of its 104 parts); whether the client also spawns the
//! sub-parts the rule flags on other ships (the gun parts of the Gladius's and Lightning's guns) is not verified.
//!
//! The attach id of a part is CRC-32C of its port name in lower case, its class the class CRC of its GUID.
//!
//! Checked against the reference table of 16 vehicles (`--ship-parts-reference`): every part this rule gives that
//! the table has is in the same port with the same class; what differs is the state of those ships (players in seats,
//! cargo, missiles fired, components a player swapped, a paint).
//!
//! A vehicle's object containers are its `VehicleComponentParams.objectContainers`, each a socpak bound as an entity of
//! class `VehicleObjectContainer` under the ship: attach id CRC-32C of `<boneName>-<socpak name without directory and
//! extension>` in lower case (`NoBone` for none), at its `Offset`, with the socpak's hash,
//! `entityCountStatic`, `entityCountDynamic` and `radius` from its metadata. All 55 containers of the reference vehicles
//! have this attach id, class and flags `0x101000`, and its `uhGuid`, `ocFilename`,
//! `socRadius` to the bit, position and rotation, `ocIdRange` of exactly `entityCountStatic` ids and an empty
//! `ocIdRangeMov`. The entities of a container's `.soc` are not in the file: the client makes them from `ocIdRange`.
//!
//! The flags of a part are those of its class, from its components: in the reference table every class always has the same flags,
//! and these rules give them for most of its 617 classes (the report of `--ship-parts-reference` counts them). They are
//! empirical.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
};

use p4k_formats::{
    cryxml::Element,
    dcb::{Dcb, Value, NULL_PTR},
};

use crate::crc_byte;

/// The path of the file in a build, which `sc-gamedata` serves under the same name.
pub const SHIP_PARTS: &str = "tables/ship_parts.bin";
/// The first bytes of the file.
pub const MAGIC: [u8; 4] = *b"SCSP";
/// The version of the format; a reader refuses another.
pub const VERSION: u32 = 2;
/// The parent of a part that is in a port of the ship itself.
pub const PARENT_IS_SHIP: i32 = -1;
/// Bytes of the header (magic, version, ship count, part count, container count), of a ship row, of a part row and of a
/// container row.
pub const HEADER_BYTES: usize = 20;
pub const SHIP_BYTES: usize = 20;
pub const PART_BYTES: usize = 16;
pub const CONTAINER_BYTES: usize = 68;
/// The class of a vehicle's object containers.
const CONTAINER_CLASS: &str = "VehicleObjectContainer";
/// The bone name of a container that names none, in its attach id.
const NO_BONE: &str = "NoBone";
/// Between the bone name and the socpak's name in the attach id.
const BONE_SEPARATOR: &str = "-";
const SOCPAK_EXTENSION: &str = ".socpak";
/// The archive's root, which the DataCore's socpak paths leave out.
const DATA_PREFIX: &str = "Data/";
const STATIC_COUNT: &str = "entityCountStatic";
const DYNAMIC_COUNT: &str = "entityCountDynamic";
const RADIUS: &str = "radius";

/// The replicated spawn flag: every part of the reference table has it.
const FLAG_REPLICATED: u32 = 0x1000;
/// A class with an `SGeometryResourceParams` (exact for the 617 classes of the reference table).
const FLAG_GEOMETRY: u32 = 0x0020_0000;
/// A class whose physics is static, or rigid with no mass.
const FLAG_STATIC: u32 = 0x0010_0000;
/// A part the client spawns itself, from its parent's class default loadout (`sc_ship_parts::FLAG_DEFAULT_OF_PARENT`); the
/// rule is `default_of_parent`.
pub const FLAG_DEFAULT_OF_PARENT: u32 = 0x2000_0000;
/// A class with an `EntityComponentRoomOccupantParams`.
const FLAG_ROOM_OCCUPANT: u32 = 0x0400_0000;
/// A class with geometry and a physics controller, but for fuel intakes.
const FLAG_PHYSICAL: u32 = 0x8000_0000;

const ENTITY_CLASS: &str = "EntityClassDefinition";
const VEHICLE: &str = "VehicleComponentParams";
const DEFAULT_LOADOUT: &str = "SEntityComponentDefaultLoadoutParams";
const MANUAL_LOADOUT: &str = "SItemPortLoadoutManualParams";
const XML_LOADOUT: &str = "SItemPortLoadoutXMLParams";
const PORT_CONTAINER: &str = "SItemPortContainerComponentParams";
const GEOMETRY: &str = "SGeometryResourceParams";
const PHYSICS: &str = "SEntityPhysicsControllerParams";
const STATIC_PHYSICS: &str = "SEntityStaticPhysicsControllerParams";
const RIGID_PHYSICS: &str = "SEntityRigidPhysicsControllerParams";
const ROOM_OCCUPANT: &str = "EntityComponentRoomOccupantParams";
const FUEL_INTAKE: &str = "SCItemFuelIntakeParams";
const NULL_GUID: &str = "00000000-0000-0000-0000-000000000000";
/// A loadout deeper than this is a loop of classes, not a ship.
const MAX_DEPTH: usize = 16;
const XML_ITEMS: &str = "Items";
const XML_ITEM: &str = "Item";
const XML_PORT: &str = "portName";
const XML_CLASS: &str = "itemName";

/// One part: the index of its parent among the ship's parts (`PARENT_IS_SHIP` for the ship), the attach id of its port on
/// that parent, its class and its flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub parent: i32,
    pub attach_id: u32,
    pub class_crc: u32,
    pub flags: u32,
}

/// An object container of a vehicle (`VehicleComponentParams.objectContainers`), bound under the ship as an entity of
/// class `VehicleObjectContainer`: the client loads its socpak and makes the entities of its `.soc` from the ids it is
/// given.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Container {
    /// CRC-32C of `<boneName>-<socpak name without directory and extension>` in lower case (`NoBone` for no bone).
    pub attach_id: u32,
    pub class_crc: u32,
    /// `ocFilename`: the socpak's hash, CRC-32C of its path in lower case from `objectcontainers` on.
    pub socpak_hash: u32,
    /// `entityCountStatic` and `entityCountDynamic` of the socpak's metadata.
    pub statics: u32,
    pub dynamics: u32,
    /// `socRadius`: the metadata's `radius`.
    pub radius: f32,
    /// `uhGuid`, as it goes on the wire (each half of eight bytes reversed).
    pub guid: [u8; 16],
    /// The container's `Offset` in the ship's frame: position, and rotation as `x y z w`.
    pub position: [f32; 3],
    pub rotation: [f32; 4],
}

/// A vehicle class and its parts, parents before children (breadth first), and its object containers.
#[derive(Clone, Debug, PartialEq)]
pub struct Ship {
    pub class_crc: u32,
    pub name: String,
    pub parts: Vec<Part>,
    pub containers: Vec<Container>,
}

/// What the walk could not follow, for the report.
#[derive(Debug, Default)]
pub struct Unread {
    /// XML loadouts that could not be read (no key) or parsed.
    pub xml_loadouts: BTreeMap<String, usize>,
    /// Classes named by a loadout that are not in the DataCore.
    pub missing_classes: BTreeMap<String, usize>,
    /// Socpaks of object containers that are not in the archive or have no metadata.
    pub missing_socpaks: BTreeMap<String, usize>,
}

/// The attach id of an object container on its vehicle: CRC-32C of `<boneName>-<socpak name>` in lower case, the name
/// without directory and extension, `NoBone` for an empty bone name (it gives the attach ids
/// of all 57 containers of the reference table).
pub fn container_attach_id(bone: &str, file_name: &str) -> u32 {
    let bone = if bone.is_empty() { NO_BONE } else { bone };
    let name = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = name.strip_suffix(SOCPAK_EXTENSION).or_else(|| name.rsplit_once('.').map(|(stem, _)| stem)).unwrap_or(name);
    port_attach_id(&format!("{bone}{BONE_SEPARATOR}{stem}"))
}

/// The key of a port: CRC-32C of its name in lower case, as the DGS's `loadout::port_attach_id`.
pub fn port_attach_id(port: &str) -> u32 {
    !port.bytes().map(|byte| byte.to_ascii_lowercase()).fold(!0, crc_byte)
}

/// One entry of a loadout: the port and the class GUID in it, and the entry's own loadout if it has one.
struct Entry {
    port: String,
    class: String,
    loadout: Option<Vec<Entry>>,
}

/// A child of a part: its port, its class GUID, its loadout when the parent's entry gives one, and whether it comes from
/// the default loadout of the parent's class (not from an inline loadout nor a port's `defaultItem`).
type Child = (String, String, Option<Rc<Vec<Entry>>>, bool);

/// Whether the client spawns a part itself (`DuplicatedItemPorts` when the table's binds fill its port again): it comes
/// only from the default loadout of the class of a part that is not the ship (`parent_is_part`), whose own entry has no
/// inline loadout (that is `from_class_default`), or it is a descendant of such a part (`parent_flagged`: the client's
/// default loadout recursion spawns it too). Holds on the DataCore for the Marauder's 6; not verified for the gun sub-parts of
/// other ships.
fn default_of_parent(parent_is_part: bool, parent_flagged: bool, from_class_default: bool) -> bool {
    parent_flagged || (parent_is_part && from_class_default)
}

/// What the walk needs of a class.
struct Class {
    crc: u32,
    flags: u32,
    /// The default loadout; `None` when it has none.
    loadout: Option<Vec<Entry>>,
    /// The ports with a default item: port name, class GUID.
    default_items: Vec<(String, String)>,
}

struct Reader<'a> {
    datacore: &'a Dcb,
    by_guid: HashMap<String, usize>,
    by_name: HashMap<String, String>,
    xml: &'a dyn Fn(&str) -> Option<Element>,
    socpak: &'a dyn Fn(&str) -> Option<Element>,
    container_class: u32,
    classes: RefCell<HashMap<String, Option<Rc<Class>>>>,
    unread: RefCell<Unread>,
}

impl Reader<'_> {
    /// A pointer, an unexpanded class or an object, as an object of its own fields; `None` for a null pointer.
    fn resolve(&self, value: &Value) -> Option<Value> {
        match value {
            Value::Strong(structure, _) | Value::Weak(structure, _) if *structure == NULL_PTR => None,
            Value::Strong(structure, instance) | Value::Weak(structure, instance) | Value::ClassRef(structure, instance) => {
                self.datacore.read_instance(*structure as usize, *instance as usize, 0, 0).ok()
            }
            Value::Obj(_) => Some(value.clone()),
            _ => None,
        }
    }

    fn struct_name(value: &Value) -> &str {
        value.as_obj().map_or("", |object| object.struct_name.as_str())
    }

    /// The components of a record, resolved.
    fn components(&self, index: usize) -> Vec<Value> {
        let Ok(record) = self.datacore.record(index) else { return Vec::new() };
        let Ok(data) = self.datacore.record_data(&record, 0) else { return Vec::new() };
        let list = data.get("Components").and_then(Value::as_list).unwrap_or_default();
        list.iter().filter_map(|component| self.resolve(component)).collect()
    }

    /// The GUID of a reference, `None` for a null one.
    fn reference(value: Option<&Value>) -> Option<String> {
        match value {
            Some(Value::Ref(_, guid)) | Some(Value::Guid(guid)) if guid != NULL_GUID => Some(guid.clone()),
            _ => None,
        }
    }

    /// The entries of a loadout object (manual or XML).
    fn loadout(&self, loadout: &Value) -> Option<Vec<Entry>> {
        let loadout = self.resolve(loadout)?;
        match Self::struct_name(&loadout) {
            MANUAL_LOADOUT => {
                let entries = loadout.get("entries").and_then(Value::as_list).unwrap_or_default();
                Some(entries.iter().filter_map(|entry| self.entry(entry)).collect())
            }
            XML_LOADOUT => {
                let path = loadout.get("loadoutPath").and_then(Value::as_str).unwrap_or_default();
                if path.is_empty() {
                    return Some(Vec::new());
                }
                match (self.xml)(path) {
                    Some(root) => Some(self.xml_entries(&root)),
                    None => {
                        *self.unread.borrow_mut().xml_loadouts.entry(path.to_ascii_lowercase()).or_default() += 1;
                        Some(Vec::new())
                    }
                }
            }
            _ => None,
        }
    }

    fn entry(&self, entry: &Value) -> Option<Entry> {
        let entry = self.resolve(entry)?;
        let port = entry.get("itemPortName").and_then(Value::as_str).unwrap_or_default().to_string();
        let class = match Self::reference(entry.get("entityClassReference")) {
            Some(guid) => guid,
            None => self.class_by_name(entry.get("entityClassName").and_then(Value::as_str).unwrap_or_default())?,
        };
        let loadout = entry.get("loadout").and_then(|loadout| self.loadout(loadout));
        Some(Entry { port, class, loadout })
    }

    fn class_by_name(&self, name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        let guid = self.by_name.get(&name.to_ascii_lowercase()).cloned();
        if guid.is_none() {
            *self.unread.borrow_mut().missing_classes.entry(name.to_string()).or_default() += 1;
        }
        guid
    }

    /// `<Items><Item portName itemName><Items>…` of an XML loadout.
    fn xml_entries(&self, element: &Element) -> Vec<Entry> {
        let Some(items) = element.find(XML_ITEMS) else { return Vec::new() };
        items
            .children
            .iter()
            .filter(|item| item.tag == XML_ITEM)
            .filter_map(|item| {
                let class = self.class_by_name(item.get(XML_CLASS).unwrap_or_default())?;
                let loadout = item.find(XML_ITEMS).map(|_| self.xml_entries(item));
                Some(Entry { port: item.get(XML_PORT).unwrap_or_default().to_string(), class, loadout })
            })
            .collect()
    }

    /// A class by its GUID, read once; `None` when the DataCore has no such record.
    fn class(&self, guid: &str) -> Option<Rc<Class>> {
        if let Some(class) = self.classes.borrow().get(guid) {
            return class.clone();
        }
        let class = self.read_class(guid).map(Rc::new);
        if class.is_none() {
            *self.unread.borrow_mut().missing_classes.entry(guid.to_string()).or_default() += 1;
        }
        self.classes.borrow_mut().insert(guid.to_string(), class.clone());
        class
    }

    fn read_class(&self, guid: &str) -> Option<Class> {
        let index = *self.by_guid.get(guid)?;
        let components = self.components(index);
        let find = |name: &str| components.iter().find(|component| Self::struct_name(component) == name);
        let loadout = find(DEFAULT_LOADOUT).and_then(|params| params.get("loadout")).and_then(|loadout| self.loadout(loadout));
        let ports = find(PORT_CONTAINER).and_then(|params| params.get("Ports")).and_then(Value::as_list).unwrap_or_default();
        let default_items = ports
            .iter()
            .filter_map(|port| self.resolve(port))
            .filter_map(|port| {
                let name = port.get("Name").and_then(Value::as_str)?.to_string();
                let item = self.resolve(port.get("defaultItem")?)?;
                Some((name, Self::reference(item.get("entityClass"))?))
            })
            .collect();
        let physics = find(PHYSICS).and_then(|params| params.get("PhysType")).and_then(|kind| self.resolve(kind));
        let geometry = find(GEOMETRY).is_some();
        let fixed = physics.as_ref().is_some_and(|physics| match Self::struct_name(physics) {
            STATIC_PHYSICS => true,
            RIGID_PHYSICS => physics.get("Mass").and_then(Value::as_f64) == Some(0.0),
            _ => false,
        });
        let mut flags = FLAG_REPLICATED;
        for (set, flag) in [
            (geometry, FLAG_GEOMETRY),
            (fixed, FLAG_STATIC),
            (find(ROOM_OCCUPANT).is_some(), FLAG_ROOM_OCCUPANT),
            (geometry && physics.is_some() && find(FUEL_INTAKE).is_none(), FLAG_PHYSICAL),
        ] {
            if set {
                flags |= flag;
            }
        }
        Some(Class { crc: crate::tables::guid_crc(guid), flags, loadout, default_items })
    }

    /// The parts of vehicle `guid`, breadth first.
    fn ship(&self, guid: &str) -> Vec<Part> {
        let Some(root) = self.class(guid) else { return Vec::new() };
        let mut parts = Vec::new();
        // (index of the parent, its class, its loadout override, depth, whether it is flagged)
        let mut queue = std::collections::VecDeque::from([(PARENT_IS_SHIP, root, None::<Rc<Vec<Entry>>>, 0usize, false)]);
        while let Some((parent, class, loadout, depth, flagged)) = queue.pop_front() {
            if depth >= MAX_DEPTH {
                continue;
            }
            for (port, child, child_loadout, from_class_default) in children_of_class(&class, loadout.as_deref().map(Vec::as_slice)) {
                let Some(child_class) = self.class(&child) else { continue };
                let index = parts.len() as i32;
                let child_flagged = default_of_parent(parent != PARENT_IS_SHIP, flagged, from_class_default);
                let flags = child_class.flags | if child_flagged { FLAG_DEFAULT_OF_PARENT } else { 0 };
                parts.push(Part { parent, attach_id: port_attach_id(&port), class_crc: child_class.crc, flags });
                queue.push_back((index, child_class, child_loadout, depth + 1, child_flagged));
            }
        }
        parts
    }
}

/// The children of a part of class `class` whose loadout is `loadout` (its class's when `None`): port, class, loadout.
fn children_of_class(class: &Class, loadout: Option<&[Entry]>) -> Vec<Child> {
    let from_class_default = loadout.is_none();
    let entries = loadout.or(class.loadout.as_deref()).unwrap_or_default();
    let mut children: Vec<Child> = entries
        .iter()
        .map(|entry| (entry.port.clone(), entry.class.clone(), entry.loadout.as_ref().map(|loadout| Rc::new(copy(loadout))), from_class_default))
        .collect();
    let filled: HashSet<String> = children.iter().map(|(port, _, _, _)| port.to_ascii_lowercase()).collect();
    for (port, guid) in &class.default_items {
        if !filled.contains(&port.to_ascii_lowercase()) {
            children.push((port.clone(), guid.clone(), None, false));
        }
    }
    children
}

fn copy(entries: &[Entry]) -> Vec<Entry> {
    entries
        .iter()
        .map(|entry| Entry { port: entry.port.clone(), class: entry.class.clone(), loadout: entry.loadout.as_deref().map(copy) })
        .collect()
}

impl Reader<'_> {
    /// The object containers of a vehicle's `VehicleComponentParams`.
    fn containers(&self, vehicle: &Value) -> Vec<Container> {
        let list = vehicle.get("objectContainers").and_then(Value::as_list).unwrap_or_default();
        list.iter()
            .filter_map(|container| self.resolve(container))
            .filter_map(|container| {
                let file_name = container.get("fileName").and_then(Value::as_str).unwrap_or_default().replace('\\', "/");
                let bone = container.get("boneName").and_then(Value::as_str).unwrap_or_default();
                // A text property, not a reference.
                let guid = container.get("guid").and_then(Value::as_str).filter(|guid| *guid != NULL_GUID)?.to_string();
                let Some(metadata) = (self.socpak)(&format!("{DATA_PREFIX}{file_name}")) else {
                    *self.unread.borrow_mut().missing_socpaks.entry(file_name.to_ascii_lowercase()).or_default() += 1;
                    return None;
                };
                let number = |name: &str| metadata.get(name).and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.0);
                let offset = container.get("Offset");
                let vector = |value: Option<&Value>, axis: &str| value.and_then(|value| value.get(axis)).and_then(Value::as_f64).unwrap_or(0.0);
                let position = offset.and_then(|offset| offset.get("Position"));
                let angles = offset.and_then(|offset| offset.get("Rotation"));
                let rotation = crate::tables::rotation_xyz([vector(angles, "x"), vector(angles, "y"), vector(angles, "z")]);
                let guid_bytes = crate::tables::guid_wire(&guid);
                Some(Container {
                    attach_id: container_attach_id(bone, &file_name),
                    class_crc: self.container_class,
                    socpak_hash: crate::socpak_hash(&file_name)?,
                    statics: number(STATIC_COUNT) as u32,
                    dynamics: number(DYNAMIC_COUNT) as u32,
                    radius: number(RADIUS) as f32,
                    guid: guid_bytes.try_into().ok()?,
                    position: [vector(position, "x"), vector(position, "y"), vector(position, "z")].map(|part| part as f32),
                    rotation: rotation.map(|part| part as f32),
                })
            })
            .collect()
    }
}

/// Every vehicle of the DataCore with its parts and object containers, sorted by class CRC, and what could not be read.
/// `xml` reads an XML file of the archive by its path from `Data/` on, as a loadout names it; `socpak` the metadata of a
/// socpak by its path from `Data/` on.
pub fn generate(datacore: &Dcb, xml: &dyn Fn(&str) -> Option<Element>, socpak: &dyn Fn(&str) -> Option<Element>) -> (Vec<Ship>, Unread) {
    let total: usize = datacore.record_index().values().map(Vec::len).sum();
    let (mut by_guid, mut by_name, mut entity_classes) = (HashMap::new(), HashMap::new(), Vec::new());
    for index in 0..total {
        let Ok(record) = datacore.record(index) else { continue };
        if record.struct_name != ENTITY_CLASS {
            continue;
        }
        let name = record.name.rsplit('.').next().unwrap_or(&record.name).to_string();
        by_name.entry(name.to_ascii_lowercase()).or_insert_with(|| record.guid.clone());
        by_guid.insert(record.guid.clone(), index);
        entity_classes.push((index, name, record.guid));
    }
    let container_class = by_name.get(&CONTAINER_CLASS.to_ascii_lowercase()).map_or(0, |guid| crate::tables::guid_crc(guid));
    let reader = Reader {
        datacore,
        by_guid,
        by_name,
        xml,
        socpak,
        container_class,
        classes: RefCell::default(),
        unread: RefCell::default(),
    };
    let mut ships: Vec<Ship> = entity_classes
        .into_iter()
        .filter_map(|(index, name, guid)| {
            let components = reader.components(index);
            let vehicle = components.iter().find(|component| Reader::struct_name(component) == VEHICLE)?;
            let containers = reader.containers(vehicle);
            Some(Ship { class_crc: crate::tables::guid_crc(&guid), parts: reader.ship(&guid), containers, name })
        })
        .collect();
    ships.sort_by_key(|ship| ship.class_crc);
    ships.dedup_by_key(|ship| ship.class_crc);
    (ships, reader.unread.into_inner())
}

/// The attribute of a vehicle's XML that holds `landingOffset`: where its keel touches the ground with the gear out, `x,y,z` from the
/// ship's origin, so `z` is minus the distance from the origin down to the pad it lands on.
const LANDING_OFFSET: &str = "landingOffset";
/// The `VehicleComponentParams` field that names the vehicle's XML.
const VEHICLE_DEFINITION: &str = "vehicleDefinition";

/// Every vehicle's origin-to-keel distance in metres (`landingOffset`'s `z`, made positive), by class CRC, sorted, for the ships
/// whose XML gives one: 0 is how a ship with no gear says it has none, and is left out.
pub fn keels(datacore: &Dcb, xml: &dyn Fn(&str) -> Option<Element>) -> Vec<(u32, f64)> {
    let total: usize = datacore.record_index().values().map(Vec::len).sum();
    let mut rows = Vec::new();
    for index in 0..total {
        let Ok(record) = datacore.record(index) else { continue };
        if record.struct_name != ENTITY_CLASS {
            continue;
        }
        let Ok(data) = datacore.record_data(&record, 0) else { continue };
        let list = data.get("Components").and_then(Value::as_list).unwrap_or_default();
        for component in list {
            let Some(component) = (match component {
                Value::Strong(structure, instance) | Value::Weak(structure, instance) => {
                    datacore.read_instance(*structure as usize, *instance as usize, 0, 0).ok()
                }
                Value::Obj(_) => Some(component.clone()),
                _ => None,
            }) else {
                continue;
            };
            if Reader::struct_name(&component) != VEHICLE {
                continue;
            }
            let path = component.get(VEHICLE_DEFINITION).and_then(Value::as_str).unwrap_or_default();
            let offset = xml(&format!("Data/{path}")).and_then(|root| root.get(LANDING_OFFSET).map(str::to_string));
            let z = offset.and_then(|text| text.split(',').nth(2)?.trim().parse::<f64>().ok()).map(f64::abs);
            if let Some(z) = z.filter(|z| *z > 0.0) {
                rows.push((crate::tables::guid_crc(&record.guid), z));
            }
        }
    }
    rows.sort_by_key(|(class_crc, _)| *class_crc);
    rows.dedup_by_key(|(class_crc, _)| *class_crc);
    rows
}

/// The file, all little endian, every field 4 bytes: a header (`MAGIC`, `VERSION`, the ship, part and container counts),
/// the ships sorted by class CRC (class CRC, index of its first part, its part count, index of its first container, its
/// container count), the parts of every ship one after the other (parent as an i32, attach id, class CRC, flags) and the
/// containers of every ship one after the other (attach id, class CRC, socpak hash, static and dynamic entity counts, radius
/// as an f32, the GUID's 16 wire bytes, position and rotation `x y z w` as f32s).
pub fn encode(ships: &[Ship]) -> Vec<u8> {
    let parts: usize = ships.iter().map(|ship| ship.parts.len()).sum();
    let containers: usize = ships.iter().map(|ship| ship.containers.len()).sum();
    let mut out = Vec::with_capacity(HEADER_BYTES + ships.len() * SHIP_BYTES + parts * PART_BYTES + containers * CONTAINER_BYTES);
    out.extend_from_slice(&MAGIC);
    for word in [VERSION, ships.len() as u32, parts as u32, containers as u32] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let (mut first_part, mut first_container) = (0u32, 0u32);
    for ship in ships {
        for word in [ship.class_crc, first_part, ship.parts.len() as u32, first_container, ship.containers.len() as u32] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        first_part += ship.parts.len() as u32;
        first_container += ship.containers.len() as u32;
    }
    for part in ships.iter().flat_map(|ship| &ship.parts) {
        out.extend_from_slice(&part.parent.to_le_bytes());
        for word in [part.attach_id, part.class_crc, part.flags] {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    for container in ships.iter().flat_map(|ship| &ship.containers) {
        for word in [container.attach_id, container.class_crc, container.socpak_hash, container.statics, container.dynamics] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&container.radius.to_le_bytes());
        out.extend_from_slice(&container.guid);
        for number in container.position.iter().chain(&container.rotation) {
            out.extend_from_slice(&number.to_le_bytes());
        }
    }
    out
}

/// How the parts of the reference ships (`testdata/retrieve_loadout_ships.tsv`) compare with the generated ones, matched
/// by the chain of attach ids from the ship down.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Reference ships the file has.
    pub ships: usize,
    /// Reference ships it does not have.
    pub missing_ships: Vec<u32>,
    /// Parts in the same port of the same parent, with the same class.
    pub same: usize,
    /// Of those, the ones with other flags.
    pub other_flags: usize,
    /// Parts in the same port with another class.
    pub other_class: usize,
    /// Parts only the reference table has, and their descendants.
    pub reference_only: usize,
    /// Parts only the file has, and their descendants.
    pub generated_only: usize,
    /// Flags of classes that differ: class CRC, reference, generated.
    pub flag_differences: BTreeMap<u32, (u32, u32)>,
    /// Reference children of a ship that are one of its object containers (same attach id and class), and of those the
    /// ones with flags other than `CONTAINER_FLAGS`.
    pub containers: usize,
    pub container_other_flags: usize,
    /// The reference entities under those containers (made by the client from the container's `ocIdRange`).
    pub container_content: usize,
    /// Object containers of the file the reference table does not have.
    pub generated_only_containers: usize,
}

/// The parent the comparison gives a reference container it has matched, which no part has.
const DETACHED: i32 = i32::MIN;

/// The flags of every object container of the reference table (`0x1000` replicated, `0x100000`).
pub const CONTAINER_FLAGS: u32 = 0x0010_1000;

/// Reads the rows of the reference table (ship class, index, parent, class, attach id, flags; hex but the indexes).
pub fn parse_reference_table(text: &str) -> Result<BTreeMap<u32, Vec<Part>>, String> {
    const COLUMNS: usize = 6;
    const HEX: u32 = 16;
    let mut ships: BTreeMap<u32, Vec<Part>> = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let fail = || format!("reference line {}", number + 1);
        if fields.len() != COLUMNS {
            return Err(fail());
        }
        let hex = |text: &str| u32::from_str_radix(text, HEX).map_err(|_| fail());
        let part = Part {
            parent: fields[2].parse().map_err(|_| fail())?,
            class_crc: hex(fields[3])?,
            attach_id: hex(fields[4])?,
            flags: hex(fields[5])?,
        };
        ships.entry(hex(fields[0])?).or_default().push(part);
    }
    Ok(ships)
}

/// Compares the generated ships with the reference ones.
pub fn compare(ships: &[Ship], table: &BTreeMap<u32, Vec<Part>>) -> Comparison {
    let mut comparison = Comparison::default();
    for (class_crc, reference) in table {
        let Some(ship) = ships.iter().find(|ship| ship.class_crc == *class_crc) else {
            comparison.missing_ships.push(*class_crc);
            continue;
        };
        comparison.ships += 1;
        // The ship's object containers first, so that what is left at the top is compared as parts.
        let mut reference = reference.clone();
        for container in &ship.containers {
            let found = reference.iter().position(|part| {
                part.parent == PARENT_IS_SHIP && part.attach_id == container.attach_id && part.class_crc == container.class_crc
            });
            let Some(index) = found else {
                comparison.generated_only_containers += 1;
                continue;
            };
            comparison.containers += 1;
            comparison.container_other_flags += usize::from(reference[index].flags != CONTAINER_FLAGS);
            comparison.container_content += descendants(&reference, index);
            // Out of the comparison of parts: no part has this parent.
            reference[index].parent = DETACHED;
        }
        compare_children(&ship.parts, PARENT_IS_SHIP, &reference, PARENT_IS_SHIP, &mut comparison);
    }
    comparison
}

fn children_of(parts: &[Part], parent: i32) -> Vec<usize> {
    (0..parts.len()).filter(|index| parts[*index].parent == parent).collect()
}

fn descendants(parts: &[Part], index: usize) -> usize {
    children_of(parts, index as i32).iter().map(|child| 1 + descendants(parts, *child)).sum()
}

fn compare_children(generated: &[Part], at: i32, reference: &[Part], recorded_at: i32, comparison: &mut Comparison) {
    let mut unmatched: Vec<usize> = children_of(reference, recorded_at);
    for child in children_of(generated, at) {
        let part = generated[child];
        let found = unmatched.iter().position(|other| reference[*other].attach_id == part.attach_id && reference[*other].class_crc == part.class_crc);
        let found = found.or_else(|| unmatched.iter().position(|other| reference[*other].attach_id == part.attach_id));
        let Some(position) = found else {
            comparison.generated_only += 1 + descendants(generated, child);
            continue;
        };
        let other = unmatched.remove(position);
        if reference[other].class_crc != part.class_crc {
            comparison.other_class += 1;
            continue;
        }
        comparison.same += 1;
        // The mark is not a flag of the class: the reference table has none.
        let flags = part.flags & !FLAG_DEFAULT_OF_PARENT;
        if reference[other].flags != flags {
            comparison.other_flags += 1;
            comparison.flag_differences.insert(part.class_crc, (reference[other].flags, flags));
        }
        compare_children(generated, child as i32, reference, other as i32, comparison);
    }
    comparison.reference_only += unmatched.iter().map(|other| 1 + descendants(reference, *other)).sum::<usize>();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIP: u32 = 0x8edc_05e2;
    const SEAT: u32 = 0x1111_1111;
    const SCREEN: u32 = 0x2222_2222;
    const FLAGS: u32 = 0x8020_1000;

    fn part(parent: i32, attach_id: u32, class_crc: u32) -> Part {
        Part { parent, attach_id, class_crc, flags: FLAGS }
    }

    fn class(loadout: Option<Vec<Entry>>, default_items: Vec<(String, String)>) -> Class {
        Class { crc: 0, flags: 0, loadout, default_items }
    }

    fn entry(port: &str, loadout: Option<Vec<Entry>>) -> Entry {
        Entry { port: port.into(), class: format!("class-{port}"), loadout }
    }

    /// The flag of each child of `class` as `ship()` gives it, children of the ship or of a part (`parent_is_part`).
    fn flags_of(class: &Class, loadout: Option<&[Entry]>, parent_is_part: bool, parent_flagged: bool) -> Vec<(String, bool)> {
        children_of_class(class, loadout)
            .into_iter()
            .map(|(port, _, _, from_default)| (port, default_of_parent(parent_is_part, parent_flagged, from_default)))
            .collect()
    }

    #[test]
    fn only_class_default_children_of_a_nested_part_are_flagged() {
        // The ship's own default loadout and its ports' default items: not flagged (depth 1).
        let ship = class(Some(vec![entry("seat", None)]), vec![("door".into(), "class-door".into())]);
        assert_eq!(flags_of(&ship, None, false, false), [("seat".into(), false), ("door".into(), false)]);
        // A nested part with no inline loadout: its class's default loadout is flagged, its defaultItem is not.
        let nested = class(Some(vec![entry("barrel", None)]), vec![("magazine".into(), "class-magazine".into())]);
        assert_eq!(flags_of(&nested, None, true, false), [("barrel".into(), true), ("magazine".into(), false)]);
        // A nested part with an inline loadout on its own entry (`Mount_Gimbal`): not flagged, nor are its defaultItems.
        let inline = [entry("gun", None)];
        assert_eq!(flags_of(&nested, Some(&inline), true, false), [("gun".into(), false), ("magazine".into(), false)]);
        // The descendants of a flagged part are flagged, whatever their source.
        assert_eq!(flags_of(&nested, Some(&inline), true, true), [("gun".into(), true), ("magazine".into(), true)]);
    }

    #[test]
    fn the_attach_id_is_the_crc_32c_of_the_lowercase_port_name() {
        // `Body_ItemPort` of the player's item port container (loadout.rs).
        assert_eq!(port_attach_id("Body_ItemPort"), 0xab63_41ac);
        assert_eq!(port_attach_id("engineeringBuff"), port_attach_id("ENGINEERINGBUFF"));
    }

    /// The Cutlass Red's exterior lights container, as the reference table lists it.
    const LIGHTS_ATTACH: u32 = 0xbab3_e001;
    const CONTAINER_CLASS_CRC: u32 = 0x5f1a_1634;

    fn container() -> Container {
        Container {
            attach_id: LIGHTS_ATTACH,
            class_crc: CONTAINER_CLASS_CRC,
            socpak_hash: 0xf290_58e8,
            statics: 15,
            dynamics: 0,
            radius: f32::from_bits(0x4190_c1e2),
            guid: [0xa4, 0x41, 0x4e, 0x8b, 0x00, 0x86, 0x2d, 0xb3, 0x9f, 0xd6, 0x7a, 0xa2, 0x20, 0x25, 0xc4, 0xa3],
            position: [0.118_868, -4.730_933, 2.720_086],
            rotation: [0.0, 0.0, -1.51e-7, 1.0],
        }
    }

    #[test]
    fn a_containers_attach_id_is_its_bone_and_socpak_name() {
        // The five object containers of the Cutlass Red, as the reference table lists them.
        let cutlass = [
            ("hardpoint_lights_ext", "objectcontainers/ships/drak/cutlass/red_ext_lg.socpak", 0xbab3_e001),
            ("hardpoint_Cutlass_Red_Int_Cockpit", "ObjectContainers\\Ships\\DRAK\\Cutlass\\red_int_cockpit_main.socpak", 0x0bd5_1741),
            ("hardpoint_Cutlass_Red_Int_Body", "objectcontainers/ships/drak/cutlass/red_int_body_main.socpak", 0xb5c7_dac2),
            ("hardpoint_Cutlass_Red_Int_Medbay", "ObjectContainers\\Ships\\DRAK\\Cutlass\\red_int_medbay_main.socpak", 0x285c_0134),
            ("hardpoint_Cutlass_Red_Int_Airlock", "ObjectContainers\\Ships\\DRAK\\Cutlass\\red_int_airlock_main.socpak", 0x29cb_9dba),
        ];
        for (bone, file, attach_id) in cutlass {
            assert_eq!(container_attach_id(bone, file), attach_id, "{bone}");
        }
        assert_eq!(container_attach_id("", "a/b.socpak"), port_attach_id("NoBone-b"));
    }

    #[test]
    fn the_file_has_a_header_the_ships_their_parts_and_containers() {
        let ships = [Ship { class_crc: SHIP, name: "DRAK_Cutlass_Red".into(), parts: vec![part(-1, 1, SEAT), part(0, 2, SCREEN)], containers: vec![container()] }];
        let bytes = encode(&ships);
        assert_eq!(bytes.len(), HEADER_BYTES + SHIP_BYTES + 2 * PART_BYTES + CONTAINER_BYTES);
        assert_eq!(&bytes[..4], &MAGIC);
        assert_eq!(bytes[4..8], VERSION.to_le_bytes());
        assert_eq!(bytes[16..20], 1u32.to_le_bytes(), "one container");
        let ship = HEADER_BYTES;
        assert_eq!(bytes[ship..ship + 4], SHIP.to_le_bytes());
        assert_eq!(bytes[ship + 8..ship + 12], 2u32.to_le_bytes());
        assert_eq!(bytes[ship + 16..ship + 20], 1u32.to_le_bytes());
        let second = HEADER_BYTES + SHIP_BYTES + PART_BYTES;
        assert_eq!(bytes[second..second + 4], 0i32.to_le_bytes());
        assert_eq!(bytes[second + 8..second + 12], SCREEN.to_le_bytes());
        let row = HEADER_BYTES + SHIP_BYTES + 2 * PART_BYTES;
        assert_eq!(bytes[row..row + 4], LIGHTS_ATTACH.to_le_bytes());
        assert_eq!(bytes[row + 12..row + 16], 15u32.to_le_bytes());
        assert_eq!(bytes[row + 24..row + 40], container().guid);
    }

    #[test]
    fn the_comparison_matches_by_port_and_counts_what_differs() {
        let table = parse_reference_table(&format!(
            "# header\n{SHIP:08x}\t0\t-1\t{SEAT:08x}\t00000001\t{FLAGS:08x}\n{SHIP:08x}\t1\t0\t{SCREEN:08x}\t00000002\t00001000\n\
             {SHIP:08x}\t2\t-1\t{SCREEN:08x}\t00000009\t{FLAGS:08x}\n"
        ))
        .unwrap();
        let ships = [Ship { class_crc: SHIP, name: String::new(), parts: vec![part(-1, 1, SEAT), part(0, 2, SCREEN), part(-1, 3, SEAT)], containers: Vec::new() }];
        let comparison = compare(&ships, &table);
        assert_eq!(comparison.ships, 1);
        assert_eq!(comparison.same, 2);
        assert_eq!(comparison.other_flags, 1);
        assert_eq!(comparison.flag_differences[&SCREEN], (0x1000, FLAGS));
        assert_eq!((comparison.generated_only, comparison.reference_only, comparison.other_class), (1, 1, 0));
    }

    /// The reference ship parts, the check of the rule. They come from the game and stay out of the repository: the maintainer
    /// puts them in `testdata/` of this crate, and the check is skipped without them.
    const REFERENCE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/retrieve_loadout_ships.tsv");
    /// The archive of build 13165946 to check against, by this variable; the test passes without it.
    const ARCHIVE_VARIABLE: &str = "GAMEDATA_TEST_P4K";
    const REFERENCE_BUILD_SAME: usize = 1635;
    const REFERENCE_BUILD_OTHER_CLASS: usize = 22;
    const REFERENCE_BUILD_GENERATED_ONLY: usize = 45;
    const REFERENCE_BUILD_OTHER_FLAGS: usize = 13;
    /// The reference ships' object containers, and the entities under them.
    const REFERENCE_BUILD_CONTAINERS: usize = 55;
    const REFERENCE_BUILD_CONTAINER_CONTENT: usize = 1490;
    const MARAUDER: u32 = 0x34c5_df90;
    const MARAUDER_DEFAULT_OF_PARENT: usize = 6;
    const LIGHTNING_F8C: u32 = 0x7f0d_f83d;

    #[test]
    fn the_reference_ships_are_what_the_rule_gives_for_build_13165946() {
        let Some(path) = std::env::var_os(ARCHIVE_VARIABLE) else {
            eprintln!("{ARCHIVE_VARIABLE} is not set: the reference ships are not checked");
            return;
        };
        let Ok(reference) = std::fs::read_to_string(REFERENCE_PATH) else {
            eprintln!("{REFERENCE_PATH} is not there: the reference ships are not checked");
            return;
        };
        let archive = p4k::P4k::open(path).expect("open archive");
        let entry = archive.get("Data/Game2.dcb").expect("no DataCore");
        let datacore = Dcb::open(archive.read(entry).expect("read DataCore")).expect("parse DataCore");
        let xml = |path: &str| p4k_formats::xmlq::parse_any(&archive.read(archive.find_path(path, ".xml")?).ok()?).ok();
        let socpaks = crate::tables::Socpaks::of(&archive);
        let socpak = |path: &str| crate::tables::metadata(&socpaks, path);
        let (ships, unread) = generate(&datacore, &xml, &socpak);
        assert!(unread.xml_loadouts.is_empty(), "the archive's key is next to it: {:?}", unread.xml_loadouts);
        let comparison = compare(&ships, &parse_reference_table(&reference).unwrap());
        // Every port of the reference table the rule fills has the class the rule gives, but where a player swapped a component
        // (16 on the Constellation Mk5, 6 on the Hornet F7CS); what only the rule has are missiles the reference ships had
        // fired (43) and two MFDs of the Starfarer's pilot.
        assert_eq!(comparison.same, REFERENCE_BUILD_SAME);
        assert_eq!(comparison.other_class, REFERENCE_BUILD_OTHER_CLASS);
        assert_eq!(comparison.generated_only, REFERENCE_BUILD_GENERATED_ONLY);
        assert_eq!(comparison.other_flags, REFERENCE_BUILD_OTHER_FLAGS);
        // Every object container of the reference ships has the attach id and class of one of the file's, and flags
        // `CONTAINER_FLAGS`; the file has none they lack.
        assert_eq!((comparison.containers, comparison.container_content), (REFERENCE_BUILD_CONTAINERS, REFERENCE_BUILD_CONTAINER_CONTENT));
        assert_eq!((comparison.container_other_flags, comparison.generated_only_containers), (0, 0));
        // The six parts of the Marauder that the client spawns itself (`DuplicatedItemPorts`).
        let marauder = ships.iter().find(|vehicle| vehicle.class_crc == MARAUDER).expect("the Marauder");
        assert_eq!(marauder.parts.iter().filter(|part| part.flags & FLAG_DEFAULT_OF_PARENT != 0).count(), MARAUDER_DEFAULT_OF_PARENT);
        for ship in [MARAUDER, LIGHTNING_F8C] {
            assert!(ships.iter().any(|vehicle| vehicle.class_crc == ship && !vehicle.parts.is_empty()), "{ship:#010x}");
        }
    }
}
