//! Builds the universe hierarchy the client asks for at `RequestAndWaitForOCHierarchyData`: a root, one node
//! per solar system of `MegaMap.PU_All`, then the object containers of each system. See docs/DATA.md for
//! which field comes from where and which have no source.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    rc::Rc,
};

use p4k_formats::{cryxml::Element, socpak::SocZip, xmlq};

/// The first id above the range the client keeps for local entities, 0x2e90cf4b7f.
pub const FIRST_GEID: u64 = 0x2e_90cf_4b80;
pub const UNIVERSE_ROOT_GEID: u64 = 0xffff_ffff_ffff_fffc;
const LABEL_CONTAINER: u32 = 1;
const LABEL_STAR_SYSTEM: u32 = 2;
const LABEL_HAS_PHYSICS_GRID: u32 = 3;
const LABEL_UNIVERSE_ROOT: u32 = 4;

/// One `<Child>` of a `ChildObjectContainers` list. A parent can list its child's children inline.
#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub guid: String,
    pub path: String,
    pub children: Vec<Child>,
}

/// What a socpak file adds to a node.
#[derive(Debug, Default, PartialEq)]
pub struct Socpak {
    pub contents_crc: u32,
    pub planet: bool,
    pub physics_grid: bool,
    pub immovable: u32,
    pub movable: u32,
    pub children: Vec<Child>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Node {
    pub geid: u64,
    pub parent: Option<u64>,
    pub guid: String,
    pub labels: Vec<u32>,
    pub contents_crc: u32,
    pub path_crc: u32,
    pub start_id: u64,
    pub planet: bool,
    pub immovable: u32,
    pub movable: u32,
    /// The file name of the socpak without its extension, such as `stanton1`. ShardTemplates name areas by it.
    pub name: String,
    /// The system's key, such as `stanton`. Empty on the universe root.
    pub system: String,
}

pub struct System {
    pub record_guid: String,
    pub name: String,
    pub socpaks: Vec<String>,
}

#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    /// Nodes under each solar system, itself included.
    pub per_system: Vec<(String, usize)>,
    pub missing: BTreeSet<String>,
}

const fn crc32c_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 1 { (value >> 1) ^ 0x82f6_3b78 } else { value >> 1 };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static CRC32C: [u32; 256] = crc32c_table();

/// Both socpak hashes are CRC-32C: the client's CRC routine has the polynomial 0x1edc6f41 and uses the
/// SSE4.2 `crc32` instruction, the same CRC-32C as `content::class_crc`, which keys entity classes.
pub fn crc32c(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, byte| (crc >> 8) ^ CRC32C[((crc ^ u32::from(*byte)) & 0xff) as usize])
}

/// The form the client hashes: lower case, forward slashes, from `objectcontainers` on, and only for a
/// socpak. CSCObjectContainerManager keys its table by this, so `Data/` and the case in the XML drop out.
pub fn container_path(path: &str) -> Option<String> {
    let path = path.to_ascii_lowercase().replace('\\', "/");
    if !path.ends_with("socpak") {
        return None;
    }
    path.find("objectcontainers").map(|start| path[start..].to_string())
}

fn children(element: &Element) -> Vec<Child> {
    let Some(list) = element.find("ChildObjectContainers") else { return Vec::new() };
    let items = list.children.iter().filter(|child| child.tag == "Child");
    items
        .map(|child| Child {
            guid: child.get("guid").unwrap_or_default().to_string(),
            path: child.get("name").unwrap_or_default().to_string(),
            children: children(child),
        })
        .collect()
}

fn count(root: &Element, attribute: &str) -> u32 {
    root.get(attribute).and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn socpak_name(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".socpak").unwrap_or(file)
}

/// The name ShardTemplates use for a system, from its socpak as `stantonsystem` gives `stanton`.
fn system_key(system: &System) -> String {
    let path = system.socpaks.first().and_then(|socpak| container_path(socpak)).unwrap_or_default();
    let name = socpak_name(&path);
    name.strip_suffix("system").unwrap_or(name).to_string()
}

/// Reads the metadata XML of a socpak. `contents_crc` covers the whole file, so the caller has the bytes.
pub fn parse_metadata(xml: &[u8], contents_crc: u32) -> Result<Socpak, String> {
    let root = xmlq::parse_any(xml).map_err(|e| format!("parse metadata: {e}"))?;
    Ok(Socpak {
        contents_crc,
        planet: count(&root, "entityCountPLA") != 0,
        physics_grid: root.find("PhysicsGrid").is_some(),
        immovable: count(&root, "entityCountStatic"),
        movable: count(&root, "entityCountDynamic"),
        children: children(&root),
    })
}

pub fn parse_socpak(bytes: &[u8]) -> Result<Socpak, String> {
    let zip = SocZip::open(bytes).map_err(|e| format!("open: {e}"))?;
    let is_metadata = |name: &&str| {
        let name = name.to_ascii_lowercase();
        !name.contains('/') && name.ends_with(".xml") && !name.ends_with("_editor.xml")
    };
    let name = zip.namelist().find(is_metadata).ok_or("no metadata XML")?.to_string();
    let xml = zip.read(&name).map_err(|e| format!("read {name}: {e}"))?;
    parse_metadata(&xml, crc32c(bytes))
}

struct Builder<L> {
    load: L,
    cache: HashMap<String, Option<Rc<Socpak>>>,
    next: u64,
    tree: Tree,
}

impl<L: FnMut(&str) -> Result<Option<Socpak>, String>> Builder<L> {
    fn socpak(&mut self, path: &str) -> Result<Option<Rc<Socpak>>, String> {
        if let Some(known) = self.cache.get(path) {
            return Ok(known.clone());
        }
        let loaded = (self.load)(path)?.map(Rc::new);
        if loaded.is_none() {
            self.tree.missing.insert(path.to_string());
        }
        self.cache.insert(path.to_string(), loaded.clone());
        Ok(loaded)
    }

    fn take_id(&mut self, count: u64) -> u64 {
        let id = self.next;
        self.next += count;
        id
    }

    /// The id range follows the node's own GEID, and a node with no entities holds none.
    fn add(&mut self, parent: Option<u64>, guid: &str, path: Option<&str>, file: Option<&Socpak>, label: u32) -> u64 {
        let geid = self.take_id(1);
        let name = path.map_or("", socpak_name).to_string();
        let mut node = Node { geid, parent, guid: guid.to_string(), labels: vec![label], name, ..Node::default() };
        if let Some(file) = file {
            let ids = u64::from(file.immovable) + u64::from(file.movable);
            // Only for a file in the archive: ProcessHierarchyNode (0x142831410) reads through the client's
            // entry for any hash that is not 0, with no check, and it has entries only for files it found.
            node.path_crc = path.map_or(0, |path| crc32c(path.as_bytes()));
            node.contents_crc = file.contents_crc;
            node.planet = file.planet;
            node.immovable = file.immovable;
            node.movable = file.movable;
            node.start_id = if ids == 0 { 0 } else { self.take_id(ids) };
            if file.physics_grid {
                node.labels.push(LABEL_HAS_PHYSICS_GRID);
            }
        }
        self.tree.nodes.push(node);
        geid
    }

    /// A node's children are those its parent listed inline, plus those of its own file that were not,
    /// matched by GUID as CUniverseHierarchyShardGraph::InitNetRecvGraphNode does on the client. `above`
    /// stops a file from expanding inside itself.
    fn instance(&mut self, parent: u64, child: &Child, above: &mut Vec<String>) -> Result<(), String> {
        let path = container_path(&child.path);
        let file = match &path {
            Some(path) => self.socpak(path)?,
            None => None,
        };
        let geid = self.add(Some(parent), &child.guid, path.as_deref(), file.as_deref(), LABEL_CONTAINER);
        let expand = path.as_ref().is_some_and(|path| !above.contains(path));
        above.extend(path.clone());
        for inline in &child.children {
            self.instance(geid, inline, above)?;
        }
        if let (true, Some(file)) = (expand, &file) {
            let listed: HashSet<&str> = child.children.iter().map(|inline| inline.guid.as_str()).collect();
            for own in file.children.iter().filter(|own| !listed.contains(own.guid.as_str())) {
                self.instance(geid, own, above)?;
            }
        }
        if path.is_some() {
            above.pop();
        }
        Ok(())
    }

    fn system(&mut self, root: u64, system: &System) -> Result<(), String> {
        let first = self.tree.nodes.len();
        let geid = self.add(Some(root), &system.record_guid, None, None, LABEL_STAR_SYSTEM);
        for socpak in &system.socpaks {
            let path = container_path(socpak).ok_or_else(|| format!("{socpak} is not a socpak"))?;
            let file = self.socpak(&path)?.ok_or_else(|| format!("{socpak} is not in the archive"))?;
            // The client derives this node's GUID from the system's GUID and the path, by a call this
            // tool cannot reproduce, so it stays empty.
            let top = self.add(Some(geid), "", Some(&path), Some(&file), LABEL_CONTAINER);
            let mut above = vec![path];
            for child in &file.children {
                self.instance(top, child, &mut above)?;
            }
        }
        let key = system_key(system);
        self.tree.nodes[first..].iter_mut().for_each(|node| node.system.clone_from(&key));
        self.tree.per_system.push((system.name.clone(), self.tree.nodes.len() - first));
        Ok(())
    }
}

/// Same input, same GEIDs: ids are handed out in walk order. A new game build can renumber them.
pub fn build(
    systems: &[System],
    load: impl FnMut(&str) -> Result<Option<Socpak>, String>,
) -> Result<Tree, String> {
    let mut builder = Builder { load, cache: HashMap::new(), next: FIRST_GEID, tree: Tree::default() };
    builder.add(None, "", None, None, LABEL_UNIVERSE_ROOT);
    // The client recognises the root and its star systems by this fixed id, the same id as the entity.
    let root = UNIVERSE_ROOT_GEID;
    builder.tree.nodes[0].geid = root;
    for system in systems {
        builder.system(root, system)?;
    }
    Ok(builder.tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from stantonsystem.xml in 4.10.2, GUIDs and paths as they are in the file.
    const STANTON: &str = r#"<ObjectContainer version="12" name="stantonsystem" entityCountPLA="0" entityCountStatic="0" entityCountDynamic="0" crcSOC="0">
  <ChildObjectContainers>
    <Child external="1" name="Data/objectcontainers/pu/system/stanton/stantonstar.socpak" entityName="OOC_Stanton" class="OrbitingObjectContainer" guid="4a269932-183e-362d-aacc-188e7debbea9" pos="0,0,0" rot="1,0,0,0">
      <InstanceMetadata/>
      <ChildObjectContainers>
        <Child external="1" name="Data/objectcontainers/pu/system/stanton/stanton1.socpak" entityName="OOC_Stanton_1_Hurston" guid="4c557c37-9273-c18e-8f84-cae53ad44d82"/>
      </ChildObjectContainers>
    </Child>
    <Child external="1" name="Data/objectcontainers/pu/system/stanton/stanton_skybox.socpak" entityName="ObjectContainer-Skybox" guid="4a6d5cca-6aa2-8d41-fa61-261b7a5f8ea7"/>
  </ChildObjectContainers>
  <Health EntityCount="3652593" OC_Count="3325"/>
</ObjectContainer>"#;

    const SYSTEM_PATH: &str = "objectcontainers/pu/system/stanton/stantonsystem.socpak";

    fn child(guid: &str, path: &str) -> Child {
        Child { guid: guid.into(), path: format!("Data/ObjectContainers/{path}.socpak"), children: Vec::new() }
    }

    fn systems(socpak: &str) -> Vec<System> {
        vec![System { record_guid: "f3132aed-f110-489b-a739-9f10208a033b".into(), name: "Stanton".into(), socpaks: vec![socpak.into()] }]
    }

    fn file(children: Vec<Child>) -> Socpak {
        Socpak { children, ..Socpak::default() }
    }

    fn stanton_files(path: &str) -> Result<Option<Socpak>, String> {
        Ok(match path {
            SYSTEM_PATH => Some(parse_metadata(STANTON.as_bytes(), 7)?),
            "objectcontainers/pu/system/stanton/stanton1.socpak" => {
                let mut planet = file(vec![child("11", "pu/loc/outpost")]);
                (planet.planet, planet.immovable, planet.movable) = (true, 3, 2);
                Some(planet)
            }
            "objectcontainers/pu/system/stanton/stantonstar.socpak"
            | "objectcontainers/pu/system/stanton/stanton_skybox.socpak"
            | "objectcontainers/pu/loc/outpost.socpak" => Some(file(Vec::new())),
            _ => None,
        })
    }

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn hashed_paths_drop_the_prefix_and_the_case() {
        let path = "Data\\ObjectContainers\\PU\\system\\nyx\\nyxsystem.socpak";
        assert_eq!(container_path(path).unwrap(), "objectcontainers/pu/system/nyx/nyxsystem.socpak");
        assert_eq!(container_path("objectcontainers/pu/a.soc"), None);
        assert_eq!(container_path("levels/a.socpak"), None);
    }

    #[test]
    fn reads_real_metadata() {
        let socpak = parse_metadata(STANTON.as_bytes(), 7).unwrap();
        assert_eq!(socpak.children.len(), 2);
        assert_eq!(socpak.children[0].guid, "4a269932-183e-362d-aacc-188e7debbea9");
        assert_eq!(socpak.children[0].children[0].path, "Data/objectcontainers/pu/system/stanton/stanton1.socpak");
        assert_eq!((socpak.planet, socpak.physics_grid, socpak.contents_crc), (false, false, 7));
    }

    #[test]
    fn counts_and_flags_come_from_the_root_element() {
        let xml = br#"<ObjectContainer entityCountPLA="1" entityCountStatic="598" entityCountDynamic="39"><PhysicsGrid/></ObjectContainer>"#;
        let socpak = parse_metadata(xml, 0).unwrap();
        assert_eq!((socpak.immovable, socpak.movable), (598, 39));
        assert!(socpak.planet && socpak.physics_grid);
    }

    #[test]
    fn the_top_is_root_then_system_then_system_socpak() {
        let tree = build(&systems(SYSTEM_PATH), stanton_files).unwrap();
        let (root, system, top) = (&tree.nodes[0], &tree.nodes[1], &tree.nodes[2]);
        assert_eq!((root.labels.as_slice(), root.parent, root.path_crc), (&[LABEL_UNIVERSE_ROOT][..], None, 0));
        assert_eq!(system.labels, [LABEL_STAR_SYSTEM]);
        assert_eq!(system.parent, Some(root.geid));
        assert_eq!(root.geid, UNIVERSE_ROOT_GEID);
        assert_eq!(system.guid, "f3132aed-f110-489b-a739-9f10208a033b");
        assert_eq!(system.path_crc, 0);
        assert_eq!(top.parent, Some(system.geid));
        assert_eq!(top.path_crc, crc32c(SYSTEM_PATH.as_bytes()));
        assert_eq!(top.contents_crc, 7);
    }

    #[test]
    fn inline_children_and_file_children_merge() {
        let tree = build(&systems(SYSTEM_PATH), stanton_files).unwrap();
        // root, system, stantonsystem, star, Hurston (inline in the star), the outpost in Hurston's file, skybox
        assert_eq!(tree.nodes.len(), 7);
        assert_eq!(tree.per_system, [("Stanton".to_string(), 6)]);
        assert!(tree.missing.is_empty());
        let hurston = tree.nodes.iter().find(|node| node.guid == "4c557c37-9273-c18e-8f84-cae53ad44d82").unwrap();
        let outpost = tree.nodes.iter().find(|node| node.guid == "11").unwrap();
        assert_eq!(outpost.parent, Some(hurston.geid));
    }

    #[test]
    fn a_child_listed_inline_and_in_its_file_appears_once() {
        let mut star = child("1", "star");
        star.children = vec![child("2", "planet")];
        let files = |path: &str| {
            Ok(match path {
                "objectcontainers/top.socpak" => Some(file(vec![star.clone()])),
                "objectcontainers/star.socpak" => Some(file(vec![child("2", "planet"), child("3", "moon")])),
                _ => Some(file(Vec::new())),
            })
        };
        let tree = build(&systems("ObjectContainers/top.socpak"), files).unwrap();
        let guids: Vec<&str> = tree.nodes[3..].iter().map(|node| node.guid.as_str()).collect();
        assert_eq!(guids, ["1", "2", "3"]);
    }

    #[test]
    fn a_file_inside_itself_stops_expanding() {
        let files = |path: &str| {
            assert!(path.ends_with(".socpak"));
            Ok(Some(file(vec![child("1", "loop")])))
        };
        let tree = build(&systems("ObjectContainers/loop.socpak"), files).unwrap();
        assert_eq!(tree.nodes.len(), 4);
    }

    #[test]
    fn a_missing_file_stays_a_node_and_is_reported() {
        let files = |path: &str| {
            Ok((path == "objectcontainers/top.socpak").then(|| file(vec![child("1", "gone")])))
        };
        let tree = build(&systems("ObjectContainers/top.socpak"), files).unwrap();
        assert_eq!(tree.nodes.len(), 4);
        assert_eq!((tree.nodes[3].path_crc, tree.nodes[3].contents_crc), (0, 0));
        assert_eq!(tree.nodes[3].guid, "1");
        assert!(tree.missing.contains("objectcontainers/gone.socpak"));
    }

    #[test]
    fn a_child_that_is_no_socpak_has_no_path_hash() {
        let files = |_: &str| {
            Ok(Some(file(vec![Child { guid: "1".into(), path: "group".into(), children: Vec::new() }])))
        };
        let tree = build(&systems("ObjectContainers/top.socpak"), files).unwrap();
        assert_eq!(tree.nodes[3].path_crc, 0);
        assert!(tree.missing.is_empty());
    }

    #[test]
    fn a_system_socpak_that_does_not_exist_is_an_error() {
        assert!(build(&systems("ObjectContainers/top.socpak"), |_: &str| Ok(None)).is_err());
    }

    #[test]
    fn ids_are_unique_outside_the_local_range_and_repeat() {
        let tree = build(&systems(SYSTEM_PATH), stanton_files).unwrap();
        let again = build(&systems(SYSTEM_PATH), stanton_files).unwrap();
        assert_eq!(tree.nodes, again.nodes);
        let mut taken = HashSet::new();
        for node in &tree.nodes {
            assert!(node.geid > 0x2e_90cf_4b7f);
            assert!(taken.insert(node.geid));
            let ids = u64::from(node.immovable) + u64::from(node.movable);
            for id in (0..ids).map(|offset| node.start_id + offset) {
                assert!(id > 0x2e_90cf_4b7f && taken.insert(id));
            }
        }
    }

    #[test]
    fn nodes_are_named_after_their_socpak_and_system() {
        let tree = build(&systems(SYSTEM_PATH), stanton_files).unwrap();
        let named = |guid: &str| {
            let node = tree.nodes.iter().find(|node| node.guid == guid).unwrap();
            (node.name.as_str(), node.system.as_str())
        };
        assert_eq!(named("4c557c37-9273-c18e-8f84-cae53ad44d82"), ("stanton1", "stanton"));
        assert_eq!(named("11"), ("outpost", "stanton"));
        assert_eq!((tree.nodes[2].name.as_str(), tree.nodes[2].system.as_str()), ("stantonsystem", "stanton"));
        let (root, system) = (&tree.nodes[0], &tree.nodes[1]);
        assert_eq!((root.name.as_str(), root.system.as_str()), ("", ""));
        assert_eq!((system.name.as_str(), system.system.as_str()), ("", "stanton"));
    }

    #[test]
    fn a_socpak_missing_from_the_archive_keeps_its_name() {
        let files = |path: &str| {
            Ok((path == "objectcontainers/top.socpak").then(|| file(vec![child("1", "gone")])))
        };
        let tree = build(&systems("ObjectContainers/top.socpak"), files).unwrap();
        assert_eq!(tree.nodes[3].name, "gone");
    }

    #[test]
    fn a_child_with_no_socpak_has_no_name() {
        let files = |_: &str| {
            Ok(Some(file(vec![Child { guid: "1".into(), path: "group".into(), children: Vec::new() }])))
        };
        let tree = build(&systems("ObjectContainers/top.socpak"), files).unwrap();
        assert_eq!(tree.nodes[3].name, "");
    }
}
