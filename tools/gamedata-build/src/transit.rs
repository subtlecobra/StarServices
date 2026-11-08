//! The hangar elevators of the home locations, which `tables.rs` writes with the other tables: the DGS's data to run
//! rides and spawn carriages (docs/DGS_TRANSIT.md). docs/DATA.md lists the columns.
//!
//! - `transit_managers.tsv`: one row per Transit manager that serves a hangar of a home location, sorted.
//! - `transit_gateways.tsv`: one row per gateway (a stop's door) of those managers, and of each hangar socpak.
//! - `transit_hangars.tsv`: one row per home location and hangar class, every size (`defaultHangars`), with the system
//!   and the manager that serves it.
//!
//! The game has two elevator systems. Transit (`SCTransit*`) serves the hangars of Orison, New Babbage, Lorville, Levski
//! and the rest stops; Transport (`SCTransport*`) those of Area18. A hangar's system is the kind of gateway its tree
//! holds. Transport is recorded only as which manager and which access point; its carriages are the manager's state.
//!
//! **A manager** lives in the location's containers, never in a hangar, as a `TransitManager` entity. Its parameters
//! are the class's `TransitManagerParams` with the entity's `PropertiesDataCore/SCTransitManager` over them. A second,
//! editor-baked `SCTransitManager` element lists its stops: `TransitDestinations/Destination`, each with `Gateway`s
//! (`gatewayPos`/`gatewayQuat` in the manager's frame, `Entity` the chain of `<Child>` GUIDs from the manager's
//! container down to the gateway entity) and their `Peripheral`s (`eType` 0 the exterior call panel, 2 the gateway's
//! door, 3 the limbo node). `CarriageSpawnLocations` has one `SpawnLocation` per carriage of the fleet, whose
//! `gatewayIndex` is hexadecimal and counts the gateways of every destination in document order.
//!
//! **A hangar** holds no manager. A Transit hangar's top socpak holds a `TransitDynamicDestination` entity, and its tree
//! holds the gateway prefabs: socpaks whose `metadata/sctransitgateway.xml` and `metadata/sctransitperipheral.xml` give
//! the gateway, its peripherals and their tags, in the prefab's frame. Where the hangar also has
//! `metadata/sctransitdynamicdestination.xml` (Lorville, Levski but its XL, the rest stops), its baked list says which
//! gateways the destination has; without one (Orison, New Babbage) every Transit gateway of the hangar is taken, as the
//! game gathers them at run time within a radius the client data does not hold. A gateway's transform is where its
//! prefab places it (the `<Child>` placements composed down from the hangar). Levski's baked lists put their gateways up
//! to 13 cm from there, which the run prints; the others agree within a centimetre. A Transport hangar holds an access
//! point instead (`metadata/sctransportgateway.xml`, `...door.xml`, `...controlpanel.xml`).
//!
//! **Which manager serves a hangar** is not baked: the hangar is instanced at run time and no manager lists it. It is
//! resolved per location, among the managers in the containers below the location's `<Child>` (its `starMapRecord`):
//!
//! - `link`: the manager has an `EntityLinks` link to the location's `InstancedInteriorManager` entity, the one the game
//!   asks for a rider's hangars. At each home location of build 12913470 exactly one manager has one.
//! - `tags`: the manager's `tagFilter` accepts the tags of every gateway of the hangar: all its positive tags are on the
//!   gateway and none of its negative ones, the rule that holds for the lobby and hangar gateways of Orison, New
//!   Babbage, Lorville and Levski.
//!
//! The `resolution` column says which held: `link+tags` when both name the same manager, `link` when only the link
//! does, `tags` when only the tag rule does. At the Pyro rest stops (Orbituary among them) it is `link`: the link names
//! `TransitManager_Hangar-to-Lobby`, the research's candidate, while no manager there accepts the hangar gateway's
//! `Spaceport_Hangar` tag (its filter is `Station_ATC`), so that manager is likely but not proven. A Transport hangar's
//! manager is the location's `TransportManager` whose `permissionsSource` is a `TransportIIMHangarInterface`
//! (`iim-interface`). A hangar no rule resolves has `none` and empty manager columns, and the run says so.
//!
//! Every transform is `x y z qx qy qz qw` in the frame of the owner's container: the socpak that holds the manager, or
//! the hangar socpak. The XML gives rotations as `w,x,y,z`. A manager's gateways are where its baked list puts them.
//! A gateway's containers are given twice, as socpak hashes and as `<Child>` GUIDs, as the same prefab is placed many
//! times (a lobby's doors) and only the GUIDs tell its instances apart.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write,
};

use p4k_formats::{
    cryxml::{self, Element},
    socpak::SocZip,
    xmlq,
};
use serde_json::Value;

use crate::socpak_hash;
use crate::tables::{children, exists, HangarClass, HomeHangars, folded, guid_crc, metadata, normalized, numbers, reference, Frame, Records, Socpaks, IDENTITY};

/// Paths of the tables in a build, as `tables.rs`'s.
pub const MANAGERS: &str = "tables/transit_managers.tsv";
pub const GATEWAYS: &str = "tables/transit_gateways.tsv";
pub const HANGARS: &str = "tables/transit_hangars.tsv";

const MAX_DEPTH: usize = 16;
/// How far a gateway taken from a hangar's prefabs may be from where its baked list puts it, in metres.
const BAKED_TOLERANCE: f64 = 0.01;

/// The three tables, each ending in a newline.
pub struct Tables {
    pub managers: String,
    pub gateways: String,
    pub hangars: String,
}

/// `w,x,y,z` as a normalized `x y z w`.
fn rotation(text: Option<&str>) -> Option<[f64; 4]> {
    let [w, x, y, z] = numbers(text)?;
    Some(normalized([x, y, z, w]))
}

fn frame(element: &Element, position: &str, turn: &str) -> Frame {
    Frame {
        position: numbers(element.get(position)).unwrap_or(IDENTITY.position),
        rotation: rotation(element.get(turn)).unwrap_or(IDENTITY.rotation),
    }
}

/// An entity of a prefab's metadata in the prefab's frame: `PivotRelativePos` and `PivotRelativeRot`, or
/// `PivotRelativeTM="Q : w,x,y,z | T : x,y,z"` (Transport).
fn pivot(element: &Element) -> Frame {
    let Some(matrix) = element.get("PivotRelativeTM") else { return frame(element, "PivotRelativePos", "PivotRelativeRot") };
    let part = |key: &str| matrix.split('|').find_map(|part| part.trim().strip_prefix(key)).map(|value| value.trim_start_matches([' ', ':']).trim());
    Frame { position: numbers(part("T")).unwrap_or(IDENTITY.position), rotation: rotation(part("Q")).unwrap_or(IDENTITY.rotation) }
}

fn distance(a: &Frame, b: &Frame) -> f64 {
    a.position.iter().zip(b.position).map(|(a, b)| (a - b) * (a - b)).sum::<f64>().sqrt()
}

/// A member of a socpak's `metadata/` by its file name, parsed.
fn member(zip: &SocZip, file: &str) -> Option<Element> {
    let suffix = format!("/metadata/{file}");
    let name = zip.namelist().find(|name| name.to_ascii_lowercase().ends_with(&suffix))?.to_string();
    xmlq::parse_any(&zip.read(&name).ok()?).ok()
}

/// The entities of a socpak: those of the `<Entities>` document of each `.soc`, then the `entdata` files.
fn entities(zip: &SocZip) -> Vec<Element> {
    let names: Vec<String> = zip.namelist().map(str::to_string).collect();
    let mut found = Vec::new();
    for name in names.iter().filter(|name| name.ends_with(".soc")) {
        let Ok(soc) = zip.read(name) else { continue };
        let Some(start) = soc.windows(7).position(|window| window == b"CryXmlB") else { continue };
        let Ok(root) = cryxml::parse(&soc[start..]) else { continue };
        found.extend(xmlq::iter(&root, "Entity").into_iter().filter(|entity| entity.get("EntityClass").is_some()).cloned());
    }
    for name in names.iter().filter(|name| name.ends_with(".entxml")) {
        if let Some(root) = zip.read(name).ok().and_then(|data| xmlq::parse_any(&data).ok()) {
            found.push(root);
        }
    }
    found
}

fn attribute(element: &Element, key: &str) -> String {
    element.get(key).unwrap_or_default().to_string()
}

fn same_guid(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The `TagId`s of an element's `Tags`.
fn tags(element: &Element) -> Vec<String> {
    xmlq::findall(element, "Tags/Tag").into_iter().filter_map(|tag| tag.get("TagId")).map(str::to_ascii_lowercase).collect()
}

/// A peripheral of a gateway: its entity's GUID and transform. An empty GUID is one the gateway does not have.
#[derive(Clone)]
struct Peripheral {
    guid: String,
    frame: Frame,
}

impl Peripheral {
    const NONE: Peripheral = Peripheral { guid: String::new(), frame: IDENTITY };

    fn row(&self) -> String {
        format!("{}\t{}", self.guid, self.frame.row())
    }
}

/// A stop's door, of a manager or of a hangar socpak.
struct Gateway {
    destination: usize,
    name: String,
    important: bool,
    enabled: bool,
    /// Carriages of the fleet that start here.
    spawns: usize,
    /// The socpaks from the owner's container down to the one that holds the gateway entity.
    containers: Vec<u32>,
    /// The `<Child>` GUIDs of those containers, then the gateway entity's: what a baked list names it by.
    chain: Vec<String>,
    guid: String,
    frame: Frame,
    door: Peripheral,
    panel: Peripheral,
    limbo: Peripheral,
    tags: Vec<String>,
}

impl Gateway {
    fn row(&self, owner_hash: u32, owner_guid: &str, index: usize) -> String {
        let containers: Vec<String> = self.containers.iter().map(u32::to_string).collect();
        let guids = &self.chain[..self.chain.len().saturating_sub(1)];
        let mut row = format!("{owner_hash}\t{owner_guid}\t{index}\t{}\t{}\t", self.destination, self.name);
        let _ = write!(row, "{}\t{}\t{}\t", u8::from(self.important), u8::from(self.enabled), self.spawns);
        let _ = write!(row, "{}\t{}\t", containers.join(","), guids.join(","));
        let _ = write!(row, "{}\t{}\t{}\t{}\t{}", self.guid, self.frame.row(), self.door.row(), self.panel.row(), self.limbo.row());
        row
    }
}

/// The socpaks of the `<Child>` chain `guids` below `start`: each GUID is a `<Child>` of the socpak reached so far, listed
/// in its metadata or inline under the `<Child>` that reached it. `None` when one is not found.
fn resolve(socpaks: &Socpaks, start: &str, guids: &[&str]) -> Option<Vec<u32>> {
    let (mut path, mut inline, mut hashes) = (start.to_string(), Vec::<Element>::new(), Vec::new());
    for guid in guids {
        let root = metadata(socpaks, &path);
        let own = root.as_ref().map(children).unwrap_or_default();
        let child = inline.iter().chain(own).find(|child| child.get("guid").is_some_and(|g| same_guid(g, guid)))?.clone();
        path = child.get("name")?.to_string();
        hashes.push(socpak_hash(&path)?);
        inline = children(&child).into_iter().cloned().collect();
    }
    Some(hashes)
}

/// A `TransitManager` entity and the parameters of the DGS's rides.
struct Manager {
    hash: u32,
    guid: String,
    name: String,
    frame: Frame,
    carriage_class: String,
    carriage_crc: u32,
    cab_hash: u32,
    carriages: usize,
    numbers: [f64; 6],
    flags: [bool; 5],
    positive: Vec<String>,
    negative: Vec<String>,
    /// The entities its `EntityLinks` point at.
    links: Vec<String>,
    gateways: Vec<Gateway>,
}

/// `TransitManagerParams` fields given as numbers, then as flags, in the order of the table's columns.
const NUMBERS: [&str; 6] = ["carriageWaitTime", "carriageDoorTimeout", "idealTimeBetweenArrivals", "pauseDistance", "carriageAcceleration", "doorFindDistance"];
const FLAGS: [&str; 5] = ["allowOneDestination", "sortDestinationsByName", "sortDestinationsByPriority", "forceStreamableTransit", "carriageDoorOpenByDefault"];

impl Manager {
    fn read(socpaks: &Socpaks, records: &Records, path: &str, entity: &Element) -> Manager {
        let name = attribute(entity, "Name");
        let class = entity.get("EntityClassGUID").and_then(|guid| records.by_guid.get(guid)).map(|index| records.data(*index, 3).2);
        let defaults = class
            .as_ref()
            .and_then(|class| class["Components"].as_array()?.iter().find(|component| component["__struct__"] == "TransitManagerParams").cloned())
            .unwrap_or_else(|| panic!("{path}: the class of {name} has no TransitManagerParams"));
        let empty = Element::default();
        let own = xmlq::find(entity, "PropertiesDataCore/SCTransitManager").unwrap_or(&empty);
        let number = |key: &str| own.get(key).and_then(|value| value.parse().ok()).or_else(|| defaults[key].as_f64()).unwrap_or(0.0);
        let flag = |key: &str| own.get(key).map_or_else(|| defaults[key].as_bool().unwrap_or(false), |value| value == "1" || value.eq_ignore_ascii_case("true"));
        let carriage = own.get("carriageType").map_or_else(|| reference(&defaults["carriageType"]).to_string(), str::to_string);
        let carriage_class = records.by_guid.get(&carriage).map(|index| records.data(*index, 1).0).unwrap_or_default();
        let interior = own.find("carriageInterior").and_then(|interior| interior.get("path")).map(str::to_string);
        let interior = interior.or_else(|| defaults["carriageInterior"]["path"].as_str().map(str::to_string)).unwrap_or_default();
        // The entity's list replaces the class's, even when it is empty.
        let filter = |sign: &str| -> Vec<String> {
            match xmlq::find(own, &format!("tagFilter/{sign}Tags")) {
                Some(list) => xmlq::iter(list, "record").into_iter().filter_map(|record| record.get("value")).map(str::to_ascii_lowercase).collect(),
                None => {
                    let default = defaults["tagFilter"][format!("{sign}Tags")].as_array().cloned().unwrap_or_default();
                    let guid = |tag: &Value| tag.as_str().unwrap_or_else(|| reference(tag)).to_ascii_lowercase();
                    default.iter().map(guid).filter(|tag| !tag.is_empty()).collect()
                }
            }
        };
        let (positive, negative) = (filter("positive"), filter("negative"));
        let links = xmlq::findall(entity, "EntityLinks/Link").into_iter().filter_map(|link| link.get("TargetCryGUID")).map(str::to_ascii_lowercase).collect();
        let frame = frame(entity, "Pos", "Rotate");
        let mut manager = Manager {
            hash: socpak_hash(path).unwrap_or(0),
            guid: attribute(entity, "EntityCryGUID").to_ascii_lowercase(),
            name,
            frame,
            carriage_class: carriage_class.rsplit('.').next().unwrap_or_default().to_string(),
            carriage_crc: if carriage.is_empty() { 0 } else { guid_crc(&carriage) },
            cab_hash: socpak_hash(&interior).unwrap_or(0),
            carriages: 0,
            numbers: NUMBERS.map(number),
            flags: FLAGS.map(flag),
            positive,
            negative,
            links,
            gateways: Vec::new(),
        };
        manager.read_stops(socpaks, path, entity);
        manager
    }

    /// The baked stops and the fleet.
    fn read_stops(&mut self, socpaks: &Socpaks, path: &str, entity: &Element) {
        let Some(baked) = entity.children.iter().find(|child| child.tag == "SCTransitManager") else { return };
        // A carriage of a line with splines starts on a spline point instead, with the index all `F`.
        let starts: Vec<u64> = xmlq::findall(baked, "CarriageSpawnLocations/SpawnLocation")
            .into_iter()
            .map(|spawn| {
                let index = spawn.get("gatewayIndex").unwrap_or_default();
                u64::from_str_radix(index.trim(), 16).unwrap_or_else(|_| panic!("{}: gatewayIndex {index:?}", self.name))
            })
            .collect();
        self.carriages = starts.len();
        let destinations = xmlq::findall(baked, "TransitDestinations/Destination");
        for (destination, stop) in destinations.into_iter().enumerate() {
            for gateway in stop.children.iter().filter(|child| child.tag == "Gateway") {
                let chain: Vec<&str> = gateway.get("Entity").unwrap_or_default().split(',').map(str::trim).filter(|g| !g.is_empty()).collect();
                let Some((guid, above)) = chain.split_last() else { continue };
                let containers = resolve(socpaks, path, above).unwrap_or_else(|| {
                    eprintln!("transit: {} gateway {guid}: the chain {above:?} is not below {path}", self.name);
                    Vec::new()
                });
                let mut row = Gateway {
                    destination,
                    name: attribute(stop, "nameIdentifier"),
                    important: stop.get("importantLocation") == Some("true"),
                    enabled: stop.get("enabled") != Some("false"),
                    spawns: starts.iter().filter(|start| **start == self.gateways.len() as u64).count(),
                    containers,
                    chain: chain.iter().map(|g| g.to_ascii_lowercase()).collect(),
                    guid: guid.to_ascii_lowercase(),
                    frame: self.frame.compose(&frame(gateway, "gatewayPos", "gatewayQuat")),
                    door: Peripheral::NONE,
                    panel: Peripheral::NONE,
                    limbo: Peripheral::NONE,
                    tags: Vec::new(),
                };
                for peripheral in gateway.children.iter().filter(|child| child.tag == "Peripheral") {
                    let chain = peripheral.get("Entity").or(peripheral.get("CryGUID")).unwrap_or_default();
                    let found = Peripheral {
                        guid: chain.rsplit(',').next().unwrap_or_default().trim().to_ascii_lowercase(),
                        frame: self.frame.compose(&frame(peripheral, "peripheralPos", "peripheralQuat")),
                    };
                    match peripheral.get("eType") {
                        Some("0") => row.panel = found,
                        Some("2") => row.door = found,
                        Some("3") => row.limbo = found,
                        _ => {}
                    }
                }
                self.gateways.push(row);
            }
        }
        let count = self.gateways.len() as u64;
        if let Some(start) = starts.iter().find(|start| **start >= count && **start != u64::MAX) {
            eprintln!("transit: {}: a carriage starts at gateway {start} of {count}", self.name);
        }
    }

    /// Whether its tag filter accepts a gateway with these tags.
    fn accepts(&self, tags: &[String]) -> bool {
        self.positive.iter().all(|tag| tags.contains(tag)) && !self.negative.iter().any(|tag| tags.contains(tag))
    }

    fn row(&self) -> String {
        let mut row = format!("{}\t{}\t{}\t{}\t", self.hash, self.guid, self.name, self.frame.row());
        let _ = write!(row, "{}\t{}\t{}\t{}", self.carriage_class, self.carriage_crc, self.cab_hash, self.carriages);
        for number in self.numbers {
            let _ = write!(row, "\t{number}");
        }
        for flag in self.flags {
            let _ = write!(row, "\t{}", u8::from(flag));
        }
        row
    }
}

/// What the containers below a location's `<Child>` hold for its hangar elevators.
#[derive(Default)]
struct Site {
    /// The GUIDs of its `InstancedInteriorManager` entities.
    iims: Vec<String>,
    transit: Vec<Manager>,
    /// `TransportManager`s whose `permissionsSource` is a `TransportIIMHangarInterface`: container hash, GUID, name.
    transport: Vec<(u32, String, String)>,
    containers: usize,
}

/// The socpak at `root` and every one below it: the `<Child>`ren of each one's metadata at any depth, inline ones
/// included, each socpak once.
fn subtree(socpaks: &Socpaks, root: &str) -> Vec<String> {
    let (mut seen, mut order, mut stack) = (HashSet::new(), Vec::new(), vec![root.to_string()]);
    while let Some(path) = stack.pop() {
        if !exists(socpaks, &path) || !seen.insert(folded(&path)) {
            continue;
        }
        if let Some(list) = metadata(socpaks, &path).as_ref().and_then(|root| root.find("ChildObjectContainers")) {
            let mut below: Vec<String> = xmlq::iter(list, "Child").into_iter().filter_map(|child| child.get("name")).map(str::to_string).collect();
            below.reverse();
            stack.extend(below);
        }
        order.push(path);
    }
    order
}

/// The `Entity CryGUID`s a component's metadata lists.
fn listed(zip: &SocZip, file: &str) -> Vec<String> {
    let Some(root) = member(zip, file) else { return Vec::new() };
    xmlq::iter(&root, "Entity").into_iter().filter_map(|entity| entity.get("CryGUID")).map(str::to_ascii_lowercase).collect()
}

fn site(socpaks: &Socpaks, records: &Records, root: &str) -> Site {
    let mut site = Site::default();
    for path in subtree(socpaks, root) {
        site.containers += 1;
        let Some(bytes) = socpaks.bytes(&path) else { continue };
        let Ok(zip) = SocZip::open(&bytes) else { continue };
        site.iims.extend(listed(&zip, "entitycomponentinstancedinteriormanager.xml"));
        let (transit, transport) = (listed(&zip, "sctransitmanager.xml"), listed(&zip, "sctransportmanager.xml"));
        if transit.is_empty() && transport.is_empty() {
            continue;
        }
        for entity in entities(&zip) {
            let guid = attribute(&entity, "EntityCryGUID").to_ascii_lowercase();
            if transit.contains(&guid) {
                site.transit.push(Manager::read(socpaks, records, &path, &entity));
            } else if transport.contains(&guid) {
                let source = xmlq::find(&entity, "PropertiesDataCore/SCTransportManager/permissionsSource");
                if source.and_then(|source| source.get("__type")) == Some("TransportIIMHangarInterface") {
                    site.transport.push((socpak_hash(&path).unwrap_or(0), guid, attribute(&entity, "Name")));
                }
            }
        }
    }
    site
}

/// A hangar socpak's elevator stop.
struct Hangar {
    system: &'static str,
    /// The `TransitDynamicDestination` entity, or for Transport the access point's `<Child>`: the owner of the
    /// hangar's rows of the gateways table.
    owner_guid: String,
    owner_name: String,
    /// The dynamic destination's `radius`, 0 where the entity has none (the client data has no default).
    radius: f64,
    frame: Frame,
    /// `baked` when the hangar's `sctransitdynamicdestination.xml` lists the gateways, `gathered` when every gateway of
    /// the hangar is taken.
    source: &'static str,
    gateways: Vec<Gateway>,
}

/// A container found in a hangar: its path, its frame in the hangar's, the `<Child>` GUIDs and socpak hashes from the
/// hangar down to it, and its entity name.
type Found<'f> = dyn FnMut(&str, &Frame, &[String], &[u32], &str) + 'f;

/// Every container below a hangar, as `tables.rs`'s `content`: the `<Child>`ren of a socpak's metadata, those listed
/// inline under a `<Child>`, then those of each child's own socpak that were not.
fn visit(socpaks: &Socpaks, list: &[(&Element, &str)], anchor: Frame, chain: &mut Vec<String>, hashes: &mut Vec<u32>, above: &mut Vec<String>, found: &mut Found) {
    for &(child, owner) in list {
        let frame = anchor.compose(&crate::tables::child_frame(child));
        let path = child.get("name").unwrap_or_default();
        let (Some(hash), true) = (socpak_hash(path), exists(socpaks, path)) else { continue };
        if above.len() > MAX_DEPTH {
            continue;
        }
        let name = child.get("entityName").unwrap_or_else(|| crate::tables::file_stem(path));
        chain.push(child.get("guid").unwrap_or_default().to_ascii_lowercase());
        hashes.push(hash);
        found(path, &frame, chain, hashes, name);
        let mut below: Vec<(&Element, &str)> = children(child).into_iter().map(|inline| (inline, owner)).collect();
        let listed: Vec<Option<&str>> = below.iter().map(|(inline, _)| inline.get("guid")).collect();
        let file = (!above.contains(&folded(path))).then(|| metadata(socpaks, path)).flatten();
        if let Some(file) = &file {
            below.extend(children(file).into_iter().filter(|own| !listed.contains(&own.get("guid"))).map(|own| (own, path)));
        }
        above.push(folded(path));
        visit(socpaks, &below, frame, chain, hashes, above, found);
        above.pop();
        chain.pop();
        hashes.pop();
    }
}

/// The gateways of a prefab that holds a Transit gateway, in the hangar's frame.
fn transit_gateways(zip: &SocZip, frame: &Frame, chain: &[String], hashes: &[u32]) -> Vec<Gateway> {
    let Some(doc) = member(zip, "sctransitgateway.xml") else { return Vec::new() };
    let peripherals = member(zip, "sctransitperipheral.xml");
    let peripherals: Vec<&Element> = peripherals.as_ref().map(|doc| xmlq::iter(doc, "Entity")).unwrap_or_default();
    let gateways: Vec<&Element> = xmlq::iter(&doc, "Entity");
    if gateways.len() > 1 {
        eprintln!("transit: a prefab with {} gateways; its peripherals go to the nearest", gateways.len());
    }
    let placed: Vec<Frame> = gateways.iter().map(|gateway| frame.compose(&pivot(gateway))).collect();
    let mut rows: Vec<Gateway> = gateways
        .iter()
        .zip(&placed)
        .map(|(gateway, placed)| {
            let guid = attribute(gateway, "CryGUID").to_ascii_lowercase();
            Gateway {
                destination: 0,
                name: String::new(),
                important: false,
                enabled: true,
                spawns: 0,
                containers: hashes.to_vec(),
                chain: chain.iter().cloned().chain([guid.clone()]).collect(),
                guid,
                frame: *placed,
                door: Peripheral::NONE,
                panel: Peripheral::NONE,
                limbo: Peripheral::NONE,
                tags: tags(gateway),
            }
        })
        .collect();
    for peripheral in peripherals {
        let found = Peripheral { guid: attribute(peripheral, "CryGUID").to_ascii_lowercase(), frame: frame.compose(&pivot(peripheral)) };
        let nearest = (0..rows.len()).min_by(|a, b| distance(&placed[*a], &found.frame).total_cmp(&distance(&placed[*b], &found.frame)));
        let Some(row) = nearest.map(|index| &mut rows[index]) else { continue };
        match peripheral.get("eType") {
            Some("0") => row.panel = found,
            Some("2") => row.door = found,
            Some("3") => row.limbo = found,
            _ => {}
        }
    }
    rows
}

/// The gateway of a Transport access point, in the hangar's frame, with its door and control panel.
fn transport_gateway(zip: &SocZip, frame: &Frame, chain: &[String], hashes: &[u32]) -> Option<Gateway> {
    let doc = member(zip, "sctransportgateway.xml")?;
    let gateway = xmlq::iter(&doc, "Entity").into_iter().next()?.clone();
    let peripheral = |file: &str| {
        let doc = member(zip, file)?;
        let entity = xmlq::iter(&doc, "Entity").into_iter().next()?.clone();
        Some(Peripheral { guid: attribute(&entity, "CryGUID").to_ascii_lowercase(), frame: frame.compose(&pivot(&entity)) })
    };
    let guid = attribute(&gateway, "CryGUID").to_ascii_lowercase();
    Some(Gateway {
        destination: 0,
        name: String::new(),
        important: false,
        enabled: true,
        spawns: 0,
        containers: hashes.to_vec(),
        chain: chain.iter().cloned().chain([guid.clone()]).collect(),
        guid,
        frame: frame.compose(&pivot(&gateway)),
        door: peripheral("sctransportgatewaydoor.xml").unwrap_or(Peripheral::NONE),
        panel: peripheral("sctransportgatewaycontrolpanel.xml").unwrap_or(Peripheral::NONE),
        limbo: Peripheral::NONE,
        tags: Vec::new(),
    })
}

fn hangar(socpaks: &Socpaks, path: &str) -> Hangar {
    let mut transit: Vec<Gateway> = Vec::new();
    let mut transport: Vec<(Gateway, String, String, Frame)> = Vec::new();
    let mut found = |container: &str, frame: &Frame, chain: &[String], hashes: &[u32], name: &str| {
        let Some(bytes) = socpaks.bytes(container) else { return };
        let Ok(zip) = SocZip::open(&bytes) else { return };
        transit.extend(transit_gateways(&zip, frame, chain, hashes));
        if let Some(gateway) = transport_gateway(&zip, frame, chain, hashes) {
            transport.push((gateway, chain.last().cloned().unwrap_or_default(), name.to_string(), *frame));
        }
    };
    found(path, &IDENTITY, &[], &[], "");
    if let Some(root) = metadata(socpaks, path) {
        let list: Vec<(&Element, &str)> = children(&root).into_iter().map(|child| (child, path)).collect();
        visit(socpaks, &list, IDENTITY, &mut Vec::new(), &mut Vec::new(), &mut vec![folded(path)], &mut found);
    }
    let bytes = socpaks.bytes(path).unwrap_or_else(|| panic!("{path} is not in the archive"));
    let zip = SocZip::open(&bytes).unwrap_or_else(|error| panic!("{path}: {error}"));
    if transit.is_empty() {
        let mut hangar = Hangar { system: "none", owner_guid: String::new(), owner_name: String::new(), radius: 0.0, frame: IDENTITY, source: "", gateways: Vec::new() };
        if let Some((gateway, guid, name, frame)) = transport.into_iter().next() {
            hangar = Hangar { system: "transport", owner_guid: guid, owner_name: name, frame, source: "gathered", gateways: vec![gateway], ..hangar };
        }
        return hangar;
    }
    let destination = entities(&zip).into_iter().find(|entity| entity.get("EntityClass") == Some("TransitDynamicDestination"));
    let destination = destination.unwrap_or_else(|| {
        eprintln!("transit: {path} has Transit gateways and no TransitDynamicDestination");
        Element::default()
    });
    let radius = xmlq::find(&destination, "PropertiesDataCore/SCTransitDynamicDestination").and_then(|params| params.get("radius"));
    let mut hangar = Hangar {
        system: "transit",
        owner_guid: attribute(&destination, "EntityCryGUID").to_ascii_lowercase(),
        owner_name: attribute(&destination, "Name"),
        radius: radius.and_then(|radius| radius.parse().ok()).unwrap_or(0.0),
        frame: frame(&destination, "Pos", "Rotate"),
        source: "gathered",
        gateways: Vec::new(),
    };
    let Some(baked) = member(&zip, "sctransitdynamicdestination.xml") else {
        hangar.gateways = transit;
        return hangar;
    };
    hangar.source = "baked";
    let entity = xmlq::iter(&baked, "Entity").into_iter().next().cloned().unwrap_or_default();
    if !same_guid(entity.get("CryGUID").unwrap_or_default(), &hangar.owner_guid) {
        eprintln!("transit: {path}: the baked destination is {:?}, the entity {}", entity.get("CryGUID"), hangar.owner_guid);
    }
    for listed in entity.children.iter().filter(|child| child.tag == "Gateway") {
        let chain: Vec<String> = listed.get("CryGUID").unwrap_or_default().split(',').map(|guid| guid.trim().to_ascii_lowercase()).collect();
        let Some(index) = transit.iter().position(|gateway| gateway.chain == chain) else {
            eprintln!("transit: {path}: the baked gateway {chain:?} is not in the hangar");
            continue;
        };
        let gateway = transit.remove(index);
        let at = frame(listed, "gatewayPos", "gatewayQuat");
        if distance(&at, &gateway.frame) > BAKED_TOLERANCE {
            eprintln!("transit: {path}: gateway {} is at {:?}, baked at {:?}", gateway.guid, gateway.frame.position, at.position);
        }
        hangar.gateways.push(gateway);
    }
    for left in &transit {
        eprintln!("transit: {path}: gateway {} {:?} is not in the baked list", left.guid, left.frame.position);
    }
    hangar
}

/// The manager serving a Transit hangar of `site`, and which rule found it (see above).
fn serving<'a>(site: &'a Site, hangar: &Hangar) -> (Option<&'a Manager>, &'static str) {
    let linked: Vec<&Manager> = site.transit.iter().filter(|manager| manager.links.iter().any(|link| site.iims.contains(link))).collect();
    let accepts = |manager: &&Manager| !hangar.gateways.is_empty() && hangar.gateways.iter().all(|gateway| manager.accepts(&gateway.tags));
    let tagged: Vec<&Manager> = site.transit.iter().filter(accepts).collect();
    let both: Vec<&Manager> = linked.iter().copied().filter(accepts).collect();
    match (both.as_slice(), linked.as_slice(), tagged.as_slice()) {
        ([one], _, _) => (Some(one), "link+tags"),
        (_, [one], _) => (Some(one), "link"),
        (_, _, [one]) => (Some(one), "tags"),
        _ => (None, "none"),
    }
}

/// The tables for the home locations and their hangar classes.
pub(crate) fn generate(socpaks: &Socpaks, records: &Records, locations: &[HomeHangars]) -> Tables {
    let mut sites: HashMap<String, Site> = HashMap::new();
    let mut hangars: HashMap<u32, Hangar> = HashMap::new();
    let mut managers: BTreeMap<(u32, String), String> = BTreeMap::new();
    let mut gateways: BTreeMap<(u32, String, usize), String> = BTreeMap::new();
    let mut rows: Vec<((u32, usize), String)> = Vec::new();
    for HomeHangars { location_id: location, guid, classes } in locations {
        let Some(place) = records.places.get(&guid.to_ascii_lowercase()) else {
            eprintln!("transit: location {location} is in no container");
            continue;
        };
        let site = sites.entry(folded(&place.path)).or_insert_with(|| site(socpaks, records, &place.path));
        for (order, HangarClass { name: class, crc, path }) in classes.iter().enumerate() {
            let hash = socpak_hash(path).unwrap_or_else(|| panic!("{path} is not under objectcontainers"));
            let hangar = hangars.entry(hash).or_insert_with(|| hangar(socpaks, path));
            for (index, gateway) in hangar.gateways.iter().enumerate() {
                gateways.insert((hash, hangar.owner_guid.clone(), index), gateway.row(hash, &hangar.owner_guid, index));
            }
            let (manager, resolution) = match hangar.system {
                "transit" => {
                    let (manager, resolution) = serving(site, hangar);
                    let manager = manager.map(|manager| {
                        managers.insert((manager.hash, manager.guid.clone()), manager.row());
                        for (index, gateway) in manager.gateways.iter().enumerate() {
                            gateways.insert((manager.hash, manager.guid.clone(), index), gateway.row(manager.hash, &manager.guid, index));
                        }
                        (manager.hash, manager.guid.clone(), manager.name.clone())
                    });
                    (manager, resolution)
                }
                "transport" => match site.transport.as_slice() {
                    [one] => (Some(one.clone()), "iim-interface"),
                    _ => (None, "none"),
                },
                _ => (None, "none"),
            };
            let (manager_hash, manager_guid, manager_name) = manager.unwrap_or_default();
            if resolution == "none" && hangar.system != "none" {
                eprintln!("transit: no manager for {class} at location {location} ({} containers below {})", site.containers, place.path);
            }
            let mut row = format!("{location}\t{class}\t{crc}\t{hash}\t{}\t{manager_hash}\t{manager_guid}\t{manager_name}\t{resolution}\t", hangar.system);
            let _ = write!(row, "{}\t{}\t{}\t{}\t{}", hangar.owner_guid, hangar.owner_name, hangar.radius, hangar.frame.row(), hangar.source);
            println!("transit: {class} at {location}: {} hangar {hash}, {} gateways ({}), manager {manager_name} ({resolution})", hangar.system, hangar.gateways.len(), hangar.source);
            rows.push(((*location, order), row));
        }
    }
    rows.sort_by_key(|(key, _)| *key);
    let text = |rows: Vec<String>| rows.join("\n") + "\n";
    Tables {
        managers: text(managers.into_values().collect()),
        gateways: text(gateways.into_values().collect()),
        hangars: text(rows.into_iter().map(|(_, row)| row).collect()),
    }
}
