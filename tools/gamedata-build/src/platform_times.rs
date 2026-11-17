//! How long each hangar's loading platform takes, which `tables.rs` writes with the other tables: the DGS's data to time
//! the platform's cycle per hangar class (docs/DATA.md lists the columns).
//!
//! `platform_times.tsv` has one row per hangar class of the home locations, every size (`defaultHangars`), sorted by
//! class name: the class, then in milliseconds the length of the opening and of the closing sequence, of the pad door's
//! opening and closing, and of the gate door's opening and closing.
//!
//! The manager holds no durations. A hangar's `LoadingPlatformManager` entity names its pad (`LoadingPlatform`), its gate
//! (`LoadingGate`) and two `SequenceObjectItem` entities, the opening and the closing track, by entity GUID. Those
//! entities are in the hangar's socpak tree (the manager is in a container below the hangar's class socpak).
//!
//! - **The sequences** hold the light, hologram and sound timeline of the manager's cycle: a track view whose `Sequence`
//!   has an `EndTime` in seconds.
//! - **The doors** (the pad and the gate are doors) play an animation clip of their geometry: the clip of
//!   `<geometry path without .cga>_door_open.caf` and `..._door_close.caf` under `Animations/`, whose length is its frame
//!   count at 30 frames per second. The entity's `DoorAnimationSpeed` (1 when absent) divides it: that is how long the door
//!   moves. A clip the archive lacks for one direction falls back to the other's.
//!
//! A column is 0 where the tree has no such entity or clip.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
};

use p4k::P4k;
use p4k_formats::{
    cryxml::Element,
    dba,
    socpak::SocZip,
    xmlq,
};

use crate::tables::{children, content_paths, folded, metadata, HangarClass, HomeHangars, Socpaks};
use crate::transit;

/// Path of the table in a build, as `tables.rs`'s.
pub const PLATFORM_TIMES: &str = "tables/platform_times.tsv";

const MANAGER_CLASS: &str = "LoadingPlatformManager";
/// The entity classes the table reads: the manager and what it names.
const READ_CLASSES: [&str; 4] = [MANAGER_CLASS, "LoadingPlatform", "LoadingPlatformLoadingGate", "SequenceObjectItem"];
const CLASS_ATTRIBUTE: &str = "EntityClass";
const GUID_ATTRIBUTE: &str = "EntityCryGUID";
/// The manager's `<SCLoadingPlatformManager>` and the elements that name its entities by `entitySuperGUID`.
const MANAGER_ELEMENT: &str = "SCLoadingPlatformManager";
const PAD_ELEMENT: &str = "LoadingPlatform";
const GATE_ELEMENT: &str = "LoadingGate";
const OPEN_TRACK_ELEMENT: &str = "OpenTrackview";
const CLOSE_TRACK_ELEMENT: &str = "CloseTrackview";
const SUPER_GUID_ATTRIBUTE: &str = "entitySuperGUID";
const SEQUENCE_ELEMENT: &str = "Sequence";
const END_TIME_ATTRIBUTE: &str = "EndTime";
const DOOR_ELEMENT: &str = "SCItemDoor";
const SPEED_ATTRIBUTE: &str = "DoorAnimationSpeed";
/// The speed of a door whose entity does not set one (the class's).
const DEFAULT_DOOR_SPEED: f64 = 1.0;
const GEOMETRY_ELEMENT: &str = "Geometry";
const PATH_ATTRIBUTE: &str = "path";
const GEOMETRY_EXTENSION: &str = ".cga";
const CLIP_EXTENSION: &str = ".caf";
const CLIP_ROOT: &str = "animations/";
const CLIP_OPEN_SUFFIX: &str = "_door_open.caf";
const CLIP_CLOSE_SUFFIX: &str = "_door_close.caf";
/// The variant letter that ends a geometry's name.
const VARIANT_SUFFIX: &str = "_a";
const MILLISECONDS: f64 = 1000.0;

/// The timings of one hangar's platform, in milliseconds, (opening, closing) each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Timing {
    pub(crate) sequence: (u64, u64),
    pub(crate) pad: (u64, u64),
    pub(crate) gate: (u64, u64),
}

impl Timing {
    fn row(&self, class: &str) -> String {
        let (so, sc, po, pc, go, gc) = (self.sequence.0, self.sequence.1, self.pad.0, self.pad.1, self.gate.0, self.gate.1);
        format!("{class}\t{so}\t{sc}\t{po}\t{pc}\t{go}\t{gc}")
    }
}

fn milliseconds(seconds: f64) -> u64 {
    (seconds * MILLISECONDS).round().max(0.0) as u64
}

fn entity_by_guid<'a>(entities: &'a [Element], guid: &str) -> Option<&'a Element> {
    entities.iter().find(|entity| entity.get(GUID_ATTRIBUTE).is_some_and(|own| own.eq_ignore_ascii_case(guid)))
}

/// The GUID a manager's `<SCLoadingPlatformManager>` names under `element`.
fn named<'a>(manager: &'a Element, element: &str) -> Option<&'a str> {
    xmlq::find(manager, &format!("{MANAGER_ELEMENT}/{element}"))?.get(SUPER_GUID_ATTRIBUTE)
}

/// The end of a sequence entity's track view, in milliseconds.
fn sequence_time(entity: &Element) -> u64 {
    let end = xmlq::iter(entity, SEQUENCE_ELEMENT).into_iter().find_map(|sequence| sequence.get(END_TIME_ATTRIBUTE));
    milliseconds(end.and_then(|end| end.parse().ok()).unwrap_or(0.0))
}

/// The geometry path of a door entity, from `objects/` on, in lower case.
fn geometry(entity: &Element) -> Option<String> {
    let found = xmlq::iter(entity, GEOMETRY_ELEMENT).into_iter().find_map(|node| node.get(PATH_ATTRIBUTE))?;
    Some(found.to_ascii_lowercase().replace('\\', "/"))
}

/// How long a door moves each way, in milliseconds: its clip's length over its animation speed. `clip` gives the clip's
/// length in seconds for the path of a clip under `Animations/`.
fn door_time(entity: &Element, clip: &dyn Fn(&str) -> Option<f64>) -> (u64, u64) {
    let speed = xmlq::iter(entity, DOOR_ELEMENT)
        .into_iter()
        .find_map(|door| door.get(SPEED_ATTRIBUTE))
        .and_then(|speed| speed.parse::<f64>().ok())
        .filter(|speed| *speed > 0.0)
        .unwrap_or(DEFAULT_DOOR_SPEED);
    let Some(path) = geometry(entity) else { return (0, 0) };
    let stem = path.strip_suffix(GEOMETRY_EXTENSION).unwrap_or(&path);
    // Some clips are named without the geometry's variant letter (`..._sml_a.cga` plays `..._sml_door_open.caf`).
    let plain = stem.strip_suffix(VARIANT_SUFFIX).unwrap_or(stem);
    let seconds = |suffix: &str| [stem, plain].into_iter().find_map(|name| clip(&format!("{CLIP_ROOT}{name}{suffix}")));
    let (open, close) = (seconds(CLIP_OPEN_SUFFIX), seconds(CLIP_CLOSE_SUFFIX));
    let time = |way: Option<f64>, other: Option<f64>| milliseconds(way.or(other).unwrap_or(0.0) / speed);
    (time(open, close), time(close, open))
}

/// The timing of the platform of the manager among `entities` (the entities of a hangar's tree), or `None` when there
/// is no manager. The first manager is the one.
pub(crate) fn timing(entities: &[Element], clip: &dyn Fn(&str) -> Option<f64>) -> Option<Timing> {
    let manager = entities.iter().find(|entity| entity.get(CLASS_ATTRIBUTE) == Some(MANAGER_CLASS))?;
    let entity = |element: &str| named(manager, element).and_then(|guid| entity_by_guid(entities, guid));
    let sequence = |element: &str| entity(element).map_or(0, sequence_time);
    let door = |element: &str| entity(element).map_or((0, 0), |door| door_time(door, clip));
    Some(Timing {
        sequence: (sequence(OPEN_TRACK_ELEMENT), sequence(CLOSE_TRACK_ELEMENT)),
        pad: door(PAD_ELEMENT),
        gate: door(GATE_ELEMENT),
    })
}

/// The entities of the classes the table reads in the socpak at `path`.
fn read_entities(socpaks: &Socpaks, path: &str) -> Vec<Element> {
    let Some(bytes) = socpaks.bytes(path) else { return Vec::new() };
    let Ok(zip) = SocZip::open(&bytes) else { return Vec::new() };
    let wanted = |entity: &Element| entity.get(CLASS_ATTRIBUTE).is_some_and(|class| READ_CLASSES.contains(&class));
    transit::entities(&zip).into_iter().filter(wanted).collect()
}

/// The length in seconds of a clip of the archive, by its path under `Data/` (any case).
fn clip_seconds(archive: &P4k, relative: &str) -> Option<f64> {
    let entry = archive.find_path(relative, CLIP_EXTENSION)?;
    let data = archive.read(entry).ok()?;
    dba::parse_caf(&data, relative).ok().map(|clip| clip.seconds)
}

/// The table: a row for each hangar class of the home locations, sorted by class name.
pub fn generate(socpaks: &Socpaks, locations: &[HomeHangars]) -> String {
    let mut by_path: HashMap<String, Vec<Element>> = HashMap::new();
    let clips: RefCell<HashMap<String, Option<f64>>> = RefCell::new(HashMap::new());
    let mut rows: BTreeMap<String, String> = BTreeMap::new();
    let classes: BTreeMap<&str, &HangarClass> =
        locations.iter().flat_map(|location| &location.classes).map(|class| (class.name.as_str(), class)).collect();
    for (name, class) in classes {
        let mut paths = vec![class.path.clone()];
        if let Some(root) = metadata(socpaks, &class.path) {
            let list: Vec<_> = children(&root).into_iter().map(|child| (child, class.path.as_str())).collect();
            content_paths(socpaks, &list, &mut vec![folded(&class.path)], &mut paths);
        }
        let mut entities = Vec::new();
        for path in paths {
            let found = by_path.entry(path.clone()).or_insert_with(|| read_entities(socpaks, &path));
            entities.extend(found.iter().cloned());
        }
        let clip = |relative: &str| {
            let known = clips.borrow().get(relative).copied();
            known.unwrap_or_else(|| {
                let seconds = clip_seconds(socpaks.archive, relative);
                clips.borrow_mut().insert(relative.to_string(), seconds);
                seconds
            })
        };
        match timing(&entities, &clip) {
            Some(timing) => {
                rows.insert(name.to_string(), timing.row(name));
            }
            None => eprintln!("{name}: no loading platform manager in the hangar's tree"),
        }
    }
    rows.values().map(|row| format!("{row}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGER_GUID: &str = "00000000-0000-0000-0000-00000000000a";
    const PAD_GUID: &str = "00000000-0000-0000-0000-00000000000b";
    const GATE_GUID: &str = "00000000-0000-0000-0000-00000000000c";
    const OPEN_GUID: &str = "00000000-0000-0000-0000-00000000000d";
    const CLOSE_GUID: &str = "00000000-0000-0000-0000-00000000000e";
    const PAD_PATH: &str = "objects/Test/Pad_A.cga";
    const GATE_PATH: &str = "objects/test/gate_a.cga";
    const PAD_CLIP_SECONDS: f64 = 15.0;
    const GATE_CLIP_SECONDS: f64 = 18.0;

    fn element(tag: &str, attributes: &[(&str, &str)], children: Vec<Element>) -> Element {
        let attrs = attributes.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
        Element { tag: tag.to_string(), attrs, text: String::new(), children }
    }

    fn named_entity(class: &str, guid: &str, inside: Vec<Element>) -> Element {
        element("Entity", &[(CLASS_ATTRIBUTE, class), (GUID_ATTRIBUTE, guid)], inside)
    }

    fn door(class: &str, guid: &str, path: &str, speed: Option<&str>) -> Element {
        let geometry = element(GEOMETRY_ELEMENT, &[(PATH_ATTRIBUTE, path)], Vec::new());
        let mut inside = vec![element("EntityGeometryResource", &[], vec![geometry])];
        match speed {
            Some(speed) => inside.push(element(DOOR_ELEMENT, &[(SPEED_ATTRIBUTE, speed)], Vec::new())),
            None => inside.push(element(DOOR_ELEMENT, &[], Vec::new())),
        }
        named_entity(class, guid, inside)
    }

    fn sequence(guid: &str, end: &str) -> Element {
        let track = element(SEQUENCE_ELEMENT, &[(END_TIME_ATTRIBUTE, end)], Vec::new());
        named_entity("SequenceObjectItem", guid, vec![element("EntityComponentTrackView", &[], vec![track])])
    }

    fn manager() -> Element {
        let named = |tag: &str, guid: &str| element(tag, &[(SUPER_GUID_ATTRIBUTE, guid)], Vec::new());
        let inside = element(
            MANAGER_ELEMENT,
            &[],
            vec![
                named(PAD_ELEMENT, PAD_GUID),
                named(GATE_ELEMENT, GATE_GUID),
                named(OPEN_TRACK_ELEMENT, OPEN_GUID),
                named(CLOSE_TRACK_ELEMENT, CLOSE_GUID),
            ],
        );
        named_entity(MANAGER_CLASS, MANAGER_GUID, vec![inside])
    }

    fn tree(pad_speed: Option<&str>) -> Vec<Element> {
        vec![
            manager(),
            door("LoadingPlatform", PAD_GUID, PAD_PATH, pad_speed),
            door("LoadingPlatformLoadingGate", GATE_GUID, GATE_PATH, Some("1.2")),
            sequence(OPEN_GUID, "33"),
            sequence(CLOSE_GUID, "31.6667"),
        ]
    }

    /// The clips of the synthetic archive: the pad's in both ways, the gate's open only.
    fn clips(relative: &str) -> Option<f64> {
        match relative {
            "animations/objects/test/pad_a_door_open.caf" | "animations/objects/test/pad_a_door_close.caf" => Some(PAD_CLIP_SECONDS),
            "animations/objects/test/gate_a_door_open.caf" => Some(GATE_CLIP_SECONDS),
            _ => None,
        }
    }

    #[test]
    fn the_sequences_and_the_doors_are_read_from_the_manager_s_entities() {
        let found = timing(&tree(Some("1.2")), &clips).unwrap();
        // 15 s over 1.2 both ways for the pad; the gate has only its opening clip, which serves both ways: 18 s over 1.2.
        assert_eq!(found, Timing { sequence: (33_000, 31_667), pad: (12_500, 12_500), gate: (15_000, 15_000) });
    }

    #[test]
    fn a_clip_may_be_named_without_the_variant_letter() {
        let plain = |relative: &str| (relative == "animations/objects/test/pad_door_open.caf").then_some(PAD_CLIP_SECONDS);
        let found = timing(&tree(Some("1.2")), &plain).unwrap();
        assert_eq!(found.pad, (12_500, 12_500));
        assert_eq!(found.gate, (0, 0));
    }

    #[test]
    fn a_door_without_a_speed_plays_at_the_class_speed() {
        assert_eq!(timing(&tree(None), &clips).unwrap().pad, (15_000, 15_000));
    }

    #[test]
    fn what_the_tree_lacks_is_zero_and_no_manager_is_no_row() {
        let lacking: Vec<Element> = tree(None).into_iter().filter(|entity| entity.get(GUID_ATTRIBUTE) != Some(GATE_GUID)).collect();
        assert_eq!(timing(&lacking, &clips).unwrap().gate, (0, 0));
        let no_clips = timing(&tree(None), &|_| None).unwrap();
        assert_eq!((no_clips.pad, no_clips.gate), ((0, 0), (0, 0)));
        assert!(timing(&tree(None)[1..], &clips).is_none());
    }

    #[test]
    fn a_row_is_the_class_and_six_milliseconds() {
        let row = Timing { sequence: (1, 2), pad: (3, 4), gate: (5, 6) }.row("Hangar_Test");
        assert_eq!(row, "Hangar_Test\t1\t2\t3\t4\t5\t6");
    }
}
