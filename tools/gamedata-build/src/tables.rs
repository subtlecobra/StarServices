//! The DGS's tables of a build, from Data.p4k, which `gamedata-build` writes under `<out>/<build>/tables/` (see main.rs),
//! as no service holds the archive. Each is tab separated, one row per line, no header; docs/DATA.md lists the columns.
//!
//! - `home_locations.tsv`: one row per home location that is not `devOnly`, sorted.
//! - `hangar_content.tsv`: the containers inside each hangar socpak of those rows, at any depth, in the order of its XML.
//! - `entity_counts.tsv`: the entity counts of each of those socpaks and every container inside them, sorted.
//!
//! A location id is the class CRC of the `StarMapObject` GUID. `InstancedInteriorLocationParams` names the
//! hangar classes of a location, and each class holds its socpak and spawn transform.
//!
//! Where a location is comes from the socpaks: its `<Child>` names the record as `starMapRecord`. The walk composes
//! `pos` and `rot` from the system down, as the client does for a node whose parent is no zone host (0x1406bfee0), and
//! starts again at each planet, as a hangar's zone is the nearest one.
//!
//! A planet's entity XML is in the socpak that lists its `<Child>`, named by the CRC-32C of the GUID as text, not in
//! the planet's own socpak. Its `Name` is the client's name for the entity, see the protocol notes (outside this repository).

use std::{borrow::Cow, collections::HashMap, fmt::Write, rc::Rc};

use p4k_formats::{cryxml::Element, dcb::Dcb, socpak::SocZip, xmlq};
use p4k::P4k;
use serde_json::Value;

use crate::{crc_byte, socpak_hash, transit};

const STAR_SUFFIX: &str = "Star";
/// The class whose socpaks hold the system's layout: stars, planets, moons and Lagrange points.
const ORBITING_CLASS: &str = "OrbitingObjectContainer";
const SYSTEMS: [&str; 3] = ["stanton", "pyro", "nyx"];

/// A position and a rotation as `x y z w`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Frame {
    pub(crate) position: [f64; 3],
    pub(crate) rotation: [f64; 4],
}

pub(crate) const IDENTITY: Frame = Frame { position: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0] };

fn multiply([ax, ay, az, aw]: [f64; 4], [bx, by, bz, bw]: [f64; 4]) -> [f64; 4] {
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

pub(crate) fn normalized(rotation: [f64; 4]) -> [f64; 4] {
    let length = rotation.iter().map(|part| part * part).sum::<f64>().sqrt();
    rotation.map(|part| part / length)
}

impl Frame {
    /// `child`, given in this frame, in the frame this one is given in.
    pub(crate) fn compose(&self, child: &Frame) -> Frame {
        let [x, y, z] = child.position;
        let [qx, qy, qz, qw] = self.rotation;
        let turned = multiply(multiply(self.rotation, [x, y, z, 0.0]), [-qx, -qy, -qz, qw]);
        let position = [self.position[0] + turned[0], self.position[1] + turned[1], self.position[2] + turned[2]];
        Frame { position, rotation: normalized(multiply(self.rotation, child.rotation)) }
    }

    pub(crate) fn row(&self) -> String {
        let [x, y, z] = self.position;
        let [qx, qy, qz, qw] = self.rotation;
        format!("{x}\t{y}\t{z}\t{qx}\t{qy}\t{qz}\t{qw}")
    }
}

pub(crate) fn numbers<const N: usize>(text: Option<&str>) -> Option<[f64; N]> {
    let parsed: Vec<f64> = text?.split(',').map(|number| number.parse().ok()).collect::<Option<_>>()?;
    parsed.try_into().ok()
}

/// `pos` and `rot` of a `<Child>`, whose rotation is `w x y z`.
pub(crate) fn child_frame(child: &Element) -> Frame {
    let position = numbers(child.get("pos")).unwrap_or(IDENTITY.position);
    let [w, x, y, z] = numbers(child.get("rot")).unwrap_or([1.0, 0.0, 0.0, 0.0]);
    Frame { position, rotation: normalized([x, y, z, w]) }
}

/// The planet a location is on, in the frame of its solar system.
struct Planet {
    name: String,
    class_crc: u32,
    socpak_hash: u32,
    /// The socpak that lists the planet, which holds `entdata/<entity_crc>.entxml`.
    entity_socpak_hash: u32,
    entity_crc: u32,
    frame: Frame,
}

/// `frame` is in the frame of the planet, or of the solar system when there is no planet.
pub(crate) struct Place {
    planet: Option<Rc<Planet>>,
    frame: Frame,
    /// The socpak of the `<Child>` that names the location, from `Data/` on.
    pub(crate) path: String,
}

/// The paths in the xml differ from the archive's in case.
pub(crate) struct Socpaks<'a> {
    pub(crate) archive: &'a P4k,
    pub(crate) names: HashMap<String, String>,
}

impl<'a> Socpaks<'a> {
    /// The bytes of a socpak by its path in any case, borrowed from the archive when it is stored uncompressed, as every
    /// socpak is: reading the members a socpak is opened for then costs no copy of the rest.
    pub(crate) fn bytes(&self, path: &str) -> Option<Cow<'a, [u8]>> {
        let entry = self.archive.get(self.names.get(&folded(path))?)?;
        if entry.method == 0 && !entry.encrypted() {
            return self.archive.raw(entry).ok().map(Cow::Borrowed);
        }
        self.archive.read(entry).ok().map(Cow::Owned)
    }
}

pub(crate) fn folded(path: &str) -> String {
    path.to_ascii_lowercase().replace('\\', "/")
}

/// A socpak's metadata, as `sc-admin` finds it: the xml at its top that is not the editor's.
pub(crate) fn metadata(socpaks: &Socpaks, path: &str) -> Option<Element> {
    let bytes = socpaks.bytes(path)?;
    let zip = SocZip::open(&bytes).ok()?;
    let is_metadata = |name: &&str| {
        let name = name.to_ascii_lowercase();
        !name.contains('/') && name.ends_with(".xml") && !name.ends_with("_editor.xml")
    };
    let name = zip.namelist().find(is_metadata)?.to_string();
    xmlq::parse_any(&zip.read(&name).ok()?).ok()
}

pub(crate) fn children(element: &Element) -> Vec<&Element> {
    let list = element.find("ChildObjectContainers");
    list.map_or(Vec::new(), |list| list.children.iter().filter(|child| child.tag == "Child").collect())
}

pub(crate) fn file_stem(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.split('.').next().unwrap_or(file)
}

/// The `entdata` file of a child of `owner`, by the name the client builds (0x1470f4c50).
fn entity_xml(socpaks: &Socpaks, owner: &str, crc: u32) -> Option<Element> {
    let bytes = socpaks.bytes(owner)?;
    let zip = SocZip::open(&bytes).ok()?;
    xmlq::parse_any(&zip.read(&format!("{}/entdata/{crc}.entxml", file_stem(owner))).ok()?).ok()
}

/// Every `<Child>` with a `starMapRecord`, by that GUID. Each child comes with the socpak that lists it.
fn walk(socpaks: &Socpaks, list: &[(&Element, &str)], anchor: Frame, planet: &Option<Rc<Planet>>, places: &mut HashMap<String, Place>) {
    for &(child, owner) in list {
        let frame = anchor.compose(&child_frame(child));
        let path = child.get("name").unwrap_or_default();
        let orbiting = child.get("class") == Some(ORBITING_CLASS);
        let file = orbiting.then(|| metadata(socpaks, path)).flatten();
        let has_planet = file.as_ref().and_then(|root| root.get("entityCountPLA")).is_some_and(|count| count != "0");
        let own_planet = has_planet.then(|| {
            let class_crc = guid_crc(child.get("classGuid").unwrap_or_default());
            let guid = child.get("guid").unwrap_or_default();
            let entity_crc = !guid.bytes().fold(!0, crc_byte);
            let entity = entity_xml(socpaks, owner, entity_crc).unwrap_or_else(|| panic!("{owner} has no entity for {path}"));
            assert_eq!(entity.get("EntityCryGUID"), Some(guid), "the entity of {path} is another one");
            let entity_socpak_hash = socpak_hash(owner).expect("a socpak under objectcontainers");
            Rc::new(Planet { name: file_stem(path).to_ascii_lowercase(), class_crc, socpak_hash: socpak_hash(path).unwrap_or(0), entity_socpak_hash, entity_crc, frame })
        });
        let (below, below_anchor) = match &own_planet {
            Some(_) => (&own_planet, IDENTITY),
            None => (planet, frame),
        };
        if let Some(record) = child.get("starMapRecord") {
            places.insert(record.to_ascii_lowercase(), Place { planet: planet.clone(), frame, path: path.to_string() });
        }
        let mut below_list: Vec<(&Element, &str)> = children(child).into_iter().map(|inline| (inline, owner)).collect();
        let listed: Vec<Option<&str>> = below_list.iter().map(|(inline, _)| inline.get("guid")).collect();
        if let Some(file) = &file {
            below_list.extend(children(file).into_iter().filter(|own| !listed.contains(&own.get("guid"))).map(|own| (own, path)));
        }
        walk(socpaks, &below_list, below_anchor, below, places);
    }
}

/// The bytes of a GUID as the game holds it: two 64-bit halves in little endian, then CRC-32C with no final xor
/// removed. `sc-admin` computes class CRCs the same way.
pub(crate) fn guid_crc(guid: &str) -> u32 {
    let digits: Vec<u8> = guid.bytes().filter(u8::is_ascii_hexdigit).collect();
    let hex = |pair: &[u8]| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    let bytes: Vec<u8> = digits.chunks_exact(2).map(hex).collect();
    let reversed = bytes.chunks(8).flat_map(|half| half.iter().rev().copied());
    !reversed.fold(!0, crc_byte)
}

/// Whether the archive holds this socpak. A `<Child>` can name one that is not there.
pub(crate) fn exists(socpaks: &Socpaks, path: &str) -> bool {
    socpaks.names.contains_key(&folded(path))
}

/// The containers inside a hangar, at any depth, each in the frame of the hangar: `<ChildObjectContainers>` of its
/// socpak, those listed inline under a `<Child>`, then those of each child's own socpak that were not. Every one is
/// bound under the hangar, the nearest zone host, as 0x1406bfee0 makes a zone host the parent of the nodes below it.
/// A child with `hasAdditionalData` has its entity XML in the socpak that lists it, as for a planet.
fn content(socpaks: &Socpaks, hangar_hash: u32, list: &[(&Element, &str)], anchor: Frame, above: &mut Vec<String>, rows: &mut Vec<String>) {
    for &(child, owner) in list {
        let frame = anchor.compose(&child_frame(child));
        let path = child.get("name").unwrap_or_default();
        let (Some(hash), true) = (socpak_hash(path), exists(socpaks, path)) else { continue };
        let class_crc = guid_crc(child.get("classGuid").unwrap_or_default());
        let guid = child.get("guid").unwrap_or_default();
        let entity_crc = !guid.bytes().fold(!0, crc_byte);
        let has_data = child.get("hasAdditionalData") == Some("1") && entity_xml(socpaks, owner, entity_crc).is_some();
        let (entity_socpak, entity_crc) = if has_data { (socpak_hash(owner).unwrap_or(0), entity_crc) } else { (0, 0) };
        let name = child.get("entityName").unwrap_or_else(|| file_stem(path));
        rows.push(format!("{hangar_hash}\t{hash}\t{class_crc}\t{entity_socpak}\t{entity_crc}\t{name}\t{}", frame.row()));
        let mut below: Vec<(&Element, &str)> = children(child).into_iter().map(|inline| (inline, owner)).collect();
        let listed: Vec<Option<&str>> = below.iter().map(|(inline, _)| inline.get("guid")).collect();
        let file = (!above.contains(&folded(path))).then(|| metadata(socpaks, path)).flatten();
        if let Some(file) = &file {
            below.extend(children(file).into_iter().filter(|own| !listed.contains(&own.get("guid"))).map(|own| (own, path)));
        }
        above.push(folded(path));
        content(socpaks, hangar_hash, &below, frame, above, rows);
        above.pop();
    }
}

/// The rows of `content` for the hangar socpak at `path` (from `Data/` on).
fn hangar_content(socpaks: &Socpaks, path: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let (Some(root), Some(hash)) = (metadata(socpaks, path), socpak_hash(path)) else { return rows };
    let list: Vec<(&Element, &str)> = children(&root).into_iter().map(|child| (child, path)).collect();
    content(socpaks, hash, &list, IDENTITY, &mut vec![folded(path)], &mut rows);
    rows
}

/// The entity counts of a socpak's metadata, as the parser (0x1470feb50) reads them, by its hash:
/// `entityCountStatic`, `entityCountDynamic` and `entityCountALE`. A server takes the ranges of a container's content
/// from these. Then the number of `<TileXmlEntry><TileItemPortEntries><ItemPort>`: the ports `ocTile<name>` that the
/// container's entity must hold for statics with `TileAttachmentParams`, such as fire extinguisher cabinets.
fn entity_counts(socpaks: &Socpaks, path: &str) -> Option<String> {
    let root = metadata(socpaks, path)?;
    let count = |name: &str| root.get(name).and_then(|count| count.parse::<u32>().ok()).unwrap_or(0);
    let counts = (count("entityCountStatic"), count("entityCountDynamic"), count("entityCountALE"));
    let entries = root.find("TileXmlEntry").and_then(|tile| tile.find("TileItemPortEntries"));
    let tile_ports = entries.map_or(0, |entries| entries.children.iter().filter(|port| port.tag == "ItemPort").count());
    Some(format!("{}\t{}\t{}\t{}\t{tile_ports}", socpak_hash(path)?, counts.0, counts.1, counts.2))
}

/// The paths of the socpaks of a hangar and of every container inside it.
fn content_paths(socpaks: &Socpaks, list: &[(&Element, &str)], above: &mut Vec<String>, paths: &mut Vec<String>) {
    for &(child, owner) in list {
        let path = child.get("name").unwrap_or_default();
        if socpak_hash(path).is_none() || !exists(socpaks, path) {
            continue;
        }
        paths.push(path.to_string());
        let mut below: Vec<(&Element, &str)> = children(child).into_iter().map(|inline| (inline, owner)).collect();
        let listed: Vec<Option<&str>> = below.iter().map(|(inline, _)| inline.get("guid")).collect();
        let file = (!above.contains(&folded(path))).then(|| metadata(socpaks, path)).flatten();
        if let Some(file) = &file {
            below.extend(children(file).into_iter().filter(|own| !listed.contains(&own.get("guid"))).map(|own| (own, path)));
        }
        above.push(folded(path));
        content_paths(socpaks, &below, above, paths);
        above.pop();
    }
}

pub(crate) fn reference(value: &Value) -> &str {
    value.get(2).and_then(Value::as_str).unwrap_or_default()
}

/// `Quat::CreateRotationXYZ` of CryEngine, from degrees, as `x y z w`.
fn quaternion(angles: &Value) -> [f64; 4] {
    let half = |axis: &str| angles[axis].as_f64().unwrap_or(0.0).to_radians() / 2.0;
    let ((sx, cx), (sy, cy), (sz, cz)) = (half("x").sin_cos(), half("y").sin_cos(), half("z").sin_cos());
    [cz * sx * cy - sz * cx * sy, cz * cx * sy + sz * sx * cy, sz * cx * cy - cz * sx * sy, cz * cx * cy + sz * sx * sy]
}

/// A home location's hangar classes.
pub(crate) struct HomeHangars {
    pub(crate) location_id: u32,
    /// The location's `StarMapObject` record.
    pub(crate) guid: String,
    pub(crate) classes: Vec<HangarClass>,
}

pub(crate) struct HangarClass {
    pub(crate) name: String,
    pub(crate) crc: u32,
    /// The socpak, from `Data/` on.
    pub(crate) path: String,
}

pub(crate) struct Records {
    datacore: Dcb,
    pub(crate) by_guid: HashMap<String, usize>,
    by_struct: HashMap<String, Vec<usize>>,
    pub(crate) places: HashMap<String, Place>,
}

impl Records {
    pub(crate) fn data(&self, index: usize, depth: usize) -> (String, String, Value) {
        let record = self.datacore.record(index).unwrap();
        let data = self.datacore.record_data(&record, depth).unwrap().to_json();
        (record.name, record.guid, data)
    }

    /// The solar system of a `StarMapObject`: its chain of parents ends at the star, such as `PyroStar`.
    fn system(&self, mut guid: String) -> Option<String> {
        loop {
            let (name, _, data) = self.data(*self.by_guid.get(&guid)?, 2);
            let parent = reference(&data["parent"]).to_string();
            if !self.by_guid.contains_key(&parent) {
                return name.rsplit('.').next()?.strip_suffix(STAR_SUFFIX).map(str::to_string);
            }
            guid = parent;
        }
    }

    /// The location of a home location row and every hangar class it lists (`defaultHangars`, small to XL, not only the
    /// first that `row` takes).
    pub(crate) fn hangar_classes(&self, index: usize) -> Option<HomeHangars> {
        let (_, _, params) = self.data(index, 3);
        let location = reference(&params["location"]).to_string();
        let mut classes = Vec::new();
        for hangar in params["defaultHangars"].as_array()? {
            let guid = reference(hangar).to_string();
            let (class, _, data) = self.data(*self.by_guid.get(&guid)?, 3);
            let components = data["Components"].as_array()?;
            let container = components.iter().find(|component| component["__struct__"] == "SObjectContainerComponentParams");
            let path = container?["objectContainer"].as_str()?;
            classes.push(HangarClass { name: class.rsplit('.').next()?.to_string(), crc: guid_crc(&guid), path: format!("Data/{path}") });
        }
        Some(HomeHangars { location_id: guid_crc(&location), guid: location, classes })
    }

    /// The row of a location, and the path of its hangar's socpak from `Data/` on.
    fn row(&self, index: usize, archive: &P4k) -> Option<(String, String)> {
        let (_, _, params) = self.data(index, 3);
        if params["devOnly"].as_bool().unwrap_or(false) {
            return None;
        }
        let location = reference(&params["location"]).to_string();
        let (name, _, _) = self.data(*self.by_guid.get(&location)?, 1);
        let hangar = reference(params["defaultHangars"].get(0)?).to_string();
        let (class, _, data) = self.data(*self.by_guid.get(&hangar)?, 3);
        let components = data["Components"].as_array()?;
        let find = |kind: &str| components.iter().find(|component| component["__struct__"] == kind);
        let path = find("SObjectContainerComponentParams")?["objectContainer"].as_str()?;
        assert!(archive.get(&format!("Data/{path}")).is_some(), "{path} is not in the archive");
        let spawn = &find("InstancedInteriorComponentParams")?["spawnTransform"];
        let position = &spawn["Position"];
        let [qx, qy, qz, qw] = quaternion(&spawn["Rotation"]);
        let place = self.places.get(&location)?;
        let mut row = format!("{}\t{}\t{}\t", guid_crc(&location), name.rsplit('.').next()?, self.system(location)?);
        let (x, y, z) = (position["x"].as_f64()?, position["y"].as_f64()?, position["z"].as_f64()?);
        write!(row, "{}\t{}\t{}\t", class.rsplit('.').next()?, guid_crc(&hangar), socpak_hash(path)?).ok()?;
        let no_planet = Planet { name: String::new(), class_crc: 0, socpak_hash: 0, entity_socpak_hash: 0, entity_crc: 0, frame: IDENTITY };
        let planet = place.planet.as_deref().unwrap_or(&no_planet);
        write!(row, "{}\t{}\t{}\t{}\t{}\t", planet.name, planet.class_crc, planet.socpak_hash, planet.entity_socpak_hash, planet.entity_crc).ok()?;
        write!(row, "{x}\t{y}\t{z}\t{qx}\t{qy}\t{qz}\t{qw}\t{}\t{}", planet.frame.row(), place.frame.row()).ok()?;
        Some((row, format!("Data/{path}")))
    }
}

/// Paths of the tables in a build, relative to `<out>/<build>/`, which `sc-gamedata` serves under the same names.
pub const HOME_LOCATIONS: &str = "tables/home_locations.tsv";
pub const HANGAR_CONTENT: &str = "tables/hangar_content.tsv";
pub const ENTITY_COUNTS: &str = "tables/entity_counts.tsv";

/// The tables, each ending in a newline.
pub struct Tables {
    pub home_locations: String,
    pub hangar_content: String,
    pub entity_counts: String,
    pub transit: transit::Tables,
}

impl Tables {
    /// Each table with its path in the build.
    pub fn files(&self) -> [(&'static str, &str); 6] {
        [
            (HOME_LOCATIONS, &self.home_locations),
            (HANGAR_CONTENT, &self.hangar_content),
            (ENTITY_COUNTS, &self.entity_counts),
            (transit::MANAGERS, &self.transit.managers),
            (transit::GATEWAYS, &self.transit.gateways),
            (transit::HANGARS, &self.transit.hangars),
        ]
    }
}

/// Reads the tables of the build `archive` is.
pub fn generate(archive: &P4k) -> Tables {
    let entry = archive.get("Data/Game2.dcb").expect("no DataCore");
    let datacore = Dcb::open(archive.read(entry).expect("read DataCore")).expect("parse DataCore");
    let total: usize = datacore.record_index().values().map(Vec::len).sum();
    let (mut by_guid, mut by_struct) = (HashMap::new(), HashMap::<String, Vec<usize>>::new());
    for index in 0..total {
        let record = datacore.record(index).expect("record");
        by_guid.insert(record.guid, index);
        by_struct.entry(record.struct_name).or_default().push(index);
    }
    let names = archive.find("", &[".socpak"]).map(|entry| (folded(&entry.name), entry.name.to_string())).collect();
    let socpaks = Socpaks { archive, names };
    let mut places = HashMap::new();
    for system in SYSTEMS {
        let path = format!("data/objectcontainers/pu/system/{system}/{system}system.socpak");
        let root = metadata(&socpaks, &path).expect("system socpak");
        let list: Vec<(&Element, &str)> = children(&root).into_iter().map(|child| (child, path.as_str())).collect();
        walk(&socpaks, &list, IDENTITY, &None, &mut places);
    }
    let records = Records { datacore, by_guid, by_struct, places };
    let indexes = &records.by_struct["InstancedInteriorLocationParams"];
    let homes: Vec<usize> = indexes.iter().copied().filter(|index| records.row(*index, archive).is_some()).collect();
    let found: Vec<(String, String)> = homes.iter().filter_map(|index| records.row(*index, archive)).collect();
    let mut rows: Vec<String> = found.iter().map(|(row, _)| row.clone()).collect();
    rows.sort();
    let hangars: std::collections::BTreeSet<&str> = found.iter().map(|(_, path)| path.as_str()).collect();
    let content_rows: Vec<String> = hangars.iter().flat_map(|path| hangar_content(&socpaks, path)).collect();
    let mut paths: Vec<String> = Vec::new();
    for path in &hangars {
        paths.push(path.to_string());
        let Some(root) = metadata(&socpaks, path) else { continue };
        let list: Vec<(&Element, &str)> = children(&root).into_iter().map(|child| (child, *path)).collect();
        content_paths(&socpaks, &list, &mut vec![folded(path)], &mut paths);
    }
    let mut count_rows: Vec<String> = paths.iter().filter_map(|path| entity_counts(&socpaks, path)).collect();
    count_rows.sort();
    count_rows.dedup();
    let locations: Vec<_> = homes.iter().filter_map(|index| records.hangar_classes(*index)).collect();
    let transit = transit::generate(&socpaks, &records, &locations);
    Tables {
        home_locations: rows.join("\n") + "\n",
        hangar_content: content_rows.join("\n") + "\n",
        entity_counts: count_rows.join("\n") + "\n",
        transit,
    }
}
