//! The parts of a ship the DGS binds with it when a retrieval spawns it on the platform (see `retrieve`).
//!
//! An official server binds a ship as a tree: the ship entity and, in the same bind batch, every item in its ports (thrusters,
//! seats, doors, cargo grids, weapon mounts, the quantum drive, the interior modules...) and its object containers (the
//! socpaks of its interior and lights). The Cutlass Red has 190
//! descendants under its root, all in the root's batch with the root first. A ship bound alone (as the first try did) has no
//! such parts, so the client shows a hull with no internals.
//!
//! The parts and containers of every vehicle of the build come from the game data, `tables/ship_parts.bin`, which
//! `tools/gamedata-build` generates from the DataCore and the socpaks (its `ship_parts.rs` has the rules and
//! docs/DATA.md has the format). `main` installs it at startup (`install`); a build whose
//! game data has no such file binds ships alone, as does a class it does not have, and the log says so.
//!
//! A part is bound at the identity pose, placed by its port. An object container (`VehicleObjectContainer`, flags
//! `0x101000`) is bound at its offset in the ship, with the variables the official ones have: `uhGuid`, `ocFilename`,
//! `socRadius`, `socHasMetadata`, `uhParentId` and `m_tileManagerId` (the ship), and `ocIdRange`/`ocIdRangeMov`, the ids
//! from which the client makes the entities of the container's `.soc` itself (lights, rooms, switches, doors): the official ones'
//! give exactly `entityCountStatic` ids. The DGS binds none of those entities.
//!
//! What is not sent: the parts' snapshot variables (the official ones carry 5 to 100 each: health, state, ...), their tags, the
//! root's owner, the containers' life time variables, and the entities of the containers the official server binds over
//! the client's own (some of each `ocIdRange`). Bare binds work for the player's items (`loadout`), and the client fills a
//! part's variables with its class's defaults, but that was not tried on a ship.
//!
//! The parts' ids are the entity graph's when it holds the ship's loadout tree (`remember`, from the answer to
//! `RetrieveShip`; sc-entitygraph's `ship_parts` makes it, and the Vehicle Loadout Manager reads and edits it), so that the
//! entities the player sees in the manager are the ones that spawn. Otherwise, and for the containers and their content, which
//! the backend has no entities for, the ids are made up, one block per ship above every other id the DGS makes (see
//! `part_id`): the parts, then the containers, then each container's content. `SC_DGS_SHIP_PARTS=off` binds the
//! ship alone, `SC_DGS_SHIP_CONTAINERS=off` binds its parts but no containers, and `SC_DGS_SHIP_CONTAINER_CONTENT=off`
//! binds the containers with no `ocIdRange`, so that the client makes none of their entities.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, OnceLock},
};

use crate::entity::{Bind, Snapshot, Transform};
use crate::location::ContentIds;

/// The first GEID sc-entitygraph gives an entity (the ships' among them).
pub const GEID_BASE: u64 = 400_000_000_000;

pub use sc_ship_parts::{Container, Part, ShipParts, FLAG_DEFAULT_OF_PARENT, MAGIC, SHIP_PARTS_PATH, VERSION};


/// `off`, `0`, `false` or `no` bind the ship without its parts.
pub const SETTING: &str = "SC_DGS_SHIP_PARTS";
/// The same for the ship's object containers, and for their content (their `ocIdRange`).
pub const CONTAINERS_SETTING: &str = "SC_DGS_SHIP_CONTAINERS";
pub const CONTENT_SETTING: &str = "SC_DGS_SHIP_CONTAINER_CONTENT";
pub const OFF_SETTINGS: [&str; 4] = ["off", "0", "false", "no"];
/// The first made up part id, above the players' (300,000,000,000), the hangars' (220,000,000,000), the items' (310,000,000,000)
/// and the backend's entities' (400,000,000,000 + the sequence) ids.
pub const FIRST_PART_ID: u64 = 600_000_000_000;
/// Ids a ship has for its parts, containers and their content: more than the largest vehicle of build 13165946 needs
/// (6149).
pub const PART_ID_STRIDE: u64 = 0x2000;
/// The ships of a build that can be spawned one after the other, before the made up ids would pass into other ranges.
const SHIP_SEQUENCE_LIMIT: u64 = 1_000_000;

/// The flags of every object container of the official ship binds.
const CONTAINER_FLAGS: u32 = 0x0010_1000;
/// CRC-32 of each variable's lowercased name and the type hash of its serializer.
const UH_GUID_KEY: u32 = 0x4944_11f3;
const OC_FILENAME_KEY: u32 = 0xffa1_bce6;
const SOC_RADIUS_KEY: u32 = 0x160c_c028;
const SOC_HAS_METADATA_KEY: u32 = 0x06d0_5294;
const TILE_MANAGER_KEY: u32 = 0x9df9_884f;
const UH_PARENT_KEY: u32 = 0xf419_b233;
const GUID_TYPE: u32 = 0x7009_3a01;
const UINT32_TYPE: u32 = 0x5d4e_4bea;
const F32_TYPE: u32 = 0x2213_655c;
const BOOL_TYPE: u32 = 0x0b9f_c026;
const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
const TRUE: u8 = 1;


static INSTALLED: OnceLock<ShipParts> = OnceLock::new();

/// Sets the parts for the life of the process, once, at startup. `false` when they were already set.
pub fn install(parts: ShipParts) -> bool {
    INSTALLED.set(parts).is_ok()
}

/// The installed parts. Tests get the fixture: a Cutlass Red of three parts and one container.
fn installed() -> &'static ShipParts {
    #[cfg(any(test, feature = "fixtures"))]
    return INSTALLED.get_or_init(fixture);
    #[cfg(not(any(test, feature = "fixtures")))]
    static NONE: LazyLock<ShipParts> = LazyLock::new(ShipParts::default);
    #[cfg(not(any(test, feature = "fixtures")))]
    INSTALLED.get().unwrap_or(&NONE)
}

/// The Cutlass Red of the fixture.
#[cfg(any(test, feature = "fixtures"))]
pub const FIXTURE_SHIP: u32 = 0x8edc_05e2;

/// Three parts of the Cutlass Red as the official service has them: its engineering buff modifier
/// (`Engineering_Buff_Modifier_DRAK_Cutlass` in port `engineeringBuff`) and a `Mount_Gimbal_S3` with its
/// `GATS_BallisticGatling_S3`; and its exterior lights container, `red_ext_lg.socpak` on `hardpoint_lights_ext`.
#[cfg(any(test, feature = "fixtures"))]
fn fixture() -> ShipParts {
    let parts = vec![
        Part { class_crc: 0xce22_64a8, attach_id: 0x9a33_0c61, flags: 0x1000, parent: None },
        Part { class_crc: 0x89ff_3fee, attach_id: 0x702d_23a7, flags: 0x8020_1000, parent: None },
        Part { class_crc: 0x50d5_0e43, attach_id: 0x5af9_cde4, flags: 0x8020_1000, parent: Some(1) },
    ];
    let lights = Container {
        attach_id: 0xbab3_e001,
        class_crc: 0x5f1a_1634,
        socpak_hash: 0xf290_58e8,
        statics: 15,
        dynamics: 0,
        radius: f32::from_bits(0x4190_c1e2),
        guid: [0xa4, 0x41, 0x4e, 0x8b, 0x00, 0x86, 0x2d, 0xb3, 0x9f, 0xd6, 0x7a, 0xa2, 0x20, 0x25, 0xc4, 0xa3],
        position: [0.118_868_2, -4.730_933, 2.720_086],
        rotation: [0.0, 0.0, -1.509_958e-7, 1.0],
    };
    ShipParts::parse(ShipParts::encode(&[(FIXTURE_SHIP, parts, vec![lights])])).expect("the fixture parses")
}

fn switched_on(setting: &str) -> bool {
    #[cfg(test)]
    return {
        let _ = setting;
        true
    };
    #[cfg(not(test))]
    !std::env::var(setting).is_ok_and(|value| OFF_SETTINGS.contains(&value.to_ascii_lowercase().as_str()))
}

/// Whether the parts are bound with a ship.
pub fn enabled() -> bool {
    switched_on(SETTING)
}

/// The id of part `index` of ship `geid` (the containers and their content follow the parts).
pub fn part_id(ship_geid: u64, index: usize) -> u64 {
    let sequence = ship_geid.saturating_sub(GEID_BASE) % SHIP_SEQUENCE_LIMIT;
    FIRST_PART_ID + sequence * PART_ID_STRIDE + index as u64
}

/// The graph's entities for a ship's parts, in the order of the game data's, from the answer that retrieved it.
static GRAPH_PARTS: LazyLock<Mutex<HashMap<u64, Vec<sc_api::internal::ShipPartRef>>>> = LazyLock::new(Default::default);

/// Keeps the graph's parts of ship `ship_geid` for its spawn. Nothing is kept for none.
pub fn remember(ship_geid: u64, parts: &[sc_api::internal::ShipPartRef]) {
    let mut kept = GRAPH_PARTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if parts.is_empty() {
        kept.remove(&ship_geid);
    } else {
        kept.insert(ship_geid, parts.to_vec());
    }
}

/// The ids of the parts of ship `ship_geid` in the order of `parts`: the graph has no entity for the parts the client spawns
/// itself (`Part::is_default_of_parent`), so its parts are those of the others, in table order, at the same ports (the class
/// and the attach id of each) and they get the graph's ids; every other part, and all of them when the graph is not the
/// table's, get the made up ones (by their index in the table).
fn part_ids(ship_geid: u64, parts: &[Part]) -> Vec<u64> {
    let mut ids: Vec<u64> = (0..parts.len()).map(|index| part_id(ship_geid, index)).collect();
    let graph = GRAPH_PARTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&ship_geid);
    if let Some(graph) = graph {
        let kept: Vec<usize> = (0..parts.len()).filter(|index| !parts[*index].is_default_of_parent()).collect();
        let same = graph.len() == kept.len()
            && graph.iter().zip(&kept).all(|(held, index)| held.class_crc == parts[*index].class_crc && held.attach_id == parts[*index].attach_id);
        if same {
            for (held, index) in graph.iter().zip(&kept) {
                ids[*index] = held.geid;
            }
        } else {
            tracing::warn!(ship = ship_geid, graph = graph.len(), table = kept.len(), "the graph's parts are not the game data's: made up ids");
        }
    }
    ids
}

/// The parts that were bound with each ship, so that they leave with it.
static SPAWNED: LazyLock<Mutex<HashMap<u64, Vec<u64>>>> = LazyLock::new(Default::default);

/// The binds of the parts and object containers of ship `geid` of class `ship_class_crc`, parents first, to go in the
/// ship's own batch right after it. Empty when the switch is off or the class has no parts in the game data.
pub fn part_binds(ship_geid: u64, ship_class_crc: u32) -> Vec<Bind> {
    let settings = (switched_on(CONTAINERS_SETTING), switched_on(CONTENT_SETTING));
    part_binds_of(installed(), ship_geid, ship_class_crc, settings)
}

fn part_binds_of(table: &ShipParts, ship_geid: u64, ship_class_crc: u32, (containers_on, content_on): (bool, bool)) -> Vec<Bind> {
    if !enabled() {
        return Vec::new();
    }
    let Some(parts) = table.parts(ship_class_crc) else {
        tracing::warn!(
            ship = ship_geid,
            class_crc = format_args!("{ship_class_crc:#010x}"),
            vehicles = table.ships(),
            "the ship class has no parts in the game data: bound alone, with no internals"
        );
        return Vec::new();
    };
    let containers = if containers_on { table.containers(ship_class_crc) } else { Vec::new() };
    let content: u64 = containers.iter().map(|container| u64::from(container.statics) + u64::from(container.dynamics)).sum();
    if (parts.len() + containers.len()) as u64 + content > PART_ID_STRIDE {
        tracing::warn!(ship = ship_geid, parts = parts.len(), "the ship has more parts than ids: bound alone");
        return Vec::new();
    }
    let ids = part_ids(ship_geid, &parts);
    let mut binds: Vec<Bind> = parts
        .iter()
        .enumerate()
        .map(|(index, part)| Bind {
            entity_id: ids[index],
            class_crc: part.class_crc,
            flags: part.bound_flags(),
            transform: Transform::IDENTITY,
            parent_id: part.parent.map_or(ship_geid, |parent| ids[parent]),
            parent_attach_id: part.attach_id,
            components: Vec::new(),
            snapshots: Vec::new(),
        })
        .collect();
    let mut next = part_id(ship_geid, parts.len() + containers.len());
    for (index, container) in containers.iter().enumerate() {
        let entity_id = part_id(ship_geid, parts.len() + index);
        let ids = if content_on {
            ContentIds::take(&mut next, (u64::from(container.statics), u64::from(container.dynamics)))
        } else {
            ContentIds::default()
        };
        binds.push(container_bind(entity_id, ship_geid, container, &ids));
    }
    SPAWNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(ship_geid, binds.iter().map(|bind| bind.entity_id).collect());
    tracing::info!(ship = ship_geid, parts = parts.len(), containers = containers.len(), "the ship's parts are bound with it");
    binds
}

/// An object container of a ship, as the official service binds it.
fn container_bind(entity_id: u64, ship_geid: u64, container: &Container, ids: &ContentIds) -> Bind {
    let snapshot = |key: u32, type_id: u32, value: Vec<u8>| Snapshot { key, type_id, value };
    let mut snapshots = vec![
        snapshot(SOC_HAS_METADATA_KEY, BOOL_TYPE, vec![TRUE]),
        snapshot(SOC_RADIUS_KEY, F32_TYPE, container.radius.to_le_bytes().to_vec()),
        snapshot(UH_GUID_KEY, GUID_TYPE, container.guid.to_vec()),
        snapshot(TILE_MANAGER_KEY, ENTITY_ID_TYPE, ship_geid.to_le_bytes().to_vec()),
        snapshot(UH_PARENT_KEY, ENTITY_ID_TYPE, ship_geid.to_le_bytes().to_vec()),
        snapshot(OC_FILENAME_KEY, UINT32_TYPE, container.socpak_hash.to_le_bytes().to_vec()),
    ];
    snapshots.extend(ids.snapshots());
    Bind {
        entity_id,
        class_crc: container.class_crc,
        flags: CONTAINER_FLAGS,
        transform: Transform {
            position: container.position.map(f64::from),
            rotation: container.rotation.map(f64::from),
        },
        parent_id: ship_geid,
        parent_attach_id: container.attach_id,
        components: Vec::new(),
        snapshots,
    }
}

/// The ids of the parts bound with ship `geid`, forgotten: they leave the client's world with the ship.
pub fn take_part_ids(ship_geid: u64) -> Vec<u64> {
    SPAWNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&ship_geid)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Marauder, which the data has no bind of.
    const MARAUDER: u32 = 0x34c5_df90;
    const ALL_ON: (bool, bool) = (true, true);

    #[test]
    fn the_file_reads_back_by_class() {
        let table = installed();
        assert_eq!(table.ships(), 1);
        let parts = table.parts(FIXTURE_SHIP).expect("the Cutlass Red");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], Part { class_crc: 0xce22_64a8, attach_id: 0x9a33_0c61, flags: 0x1000, parent: None });
        assert_eq!(parts[2].parent, Some(1));
        let containers = table.containers(FIXTURE_SHIP);
        assert_eq!(containers.len(), 1);
        assert_eq!((containers[0].attach_id, containers[0].statics, containers[0].radius), (0xbab3_e001, 15, f32::from_bits(0x4190_c1e2)));
        assert_eq!(containers[0].guid[0], 0xa4);
        assert_eq!(table.parts(MARAUDER), None);
        assert!(table.containers(MARAUDER).is_empty());
    }

    #[test]
    fn the_parts_are_bound_under_the_ship_and_each_other_with_made_up_ids() {
        let ship = GEID_BASE + 246;
        let binds = part_binds(ship, FIXTURE_SHIP);
        assert_eq!(binds.len(), 4);
        assert_eq!(binds[0].parent_id, ship);
        assert_eq!(binds[0].parent_attach_id, 0x9a33_0c61);
        assert_eq!(binds[0].entity_id, FIRST_PART_ID + 246 * PART_ID_STRIDE);
        assert_eq!(binds[2].parent_id, binds[1].entity_id);
        assert_eq!((binds[2].class_crc, binds[2].parent_attach_id), (0x50d5_0e43, 0x5af9_cde4));
        assert_eq!(take_part_ids(ship).len(), 4);
        assert!(take_part_ids(ship).is_empty());
    }

    #[test]
    fn the_graphs_parts_are_bound_with_their_ids_when_they_are_the_game_datas() {
        let ship = GEID_BASE + 11;
        let graph = |geid: u64, part: &Part| sc_api::internal::ShipPartRef { geid, class_crc: part.class_crc, attach_id: part.attach_id };
        let parts = installed().parts(FIXTURE_SHIP).unwrap();
        let held: Vec<_> = parts.iter().enumerate().map(|(index, part)| graph(GEID_BASE + 5000 + index as u64, part)).collect();
        remember(ship, &held);
        let binds = part_binds(ship, FIXTURE_SHIP);
        assert_eq!(binds.iter().take(3).map(|bind| bind.entity_id).collect::<Vec<_>>(), [GEID_BASE + 5000, GEID_BASE + 5001, GEID_BASE + 5002]);
        assert_eq!((binds[0].parent_id, binds[2].parent_id), (ship, GEID_BASE + 5001), "the tree keeps its shape");
        assert_eq!(binds[3].entity_id, part_id(ship, 3), "the container has a made up id after the parts");
        assert_eq!(take_part_ids(ship).len(), 4, "they leave with the ship");
        // Another port: the graph's tree is not this build's, so the ids are made up.
        let mut other = held.clone();
        other[1].attach_id ^= 1;
        remember(ship, &other);
        assert_eq!(part_binds(ship, FIXTURE_SHIP)[0].entity_id, part_id(ship, 0));
        take_part_ids(ship);
        // Used once: the next spawn of the ship asks again.
        assert_eq!(part_binds(ship, FIXTURE_SHIP)[0].entity_id, part_id(ship, 0));
        take_part_ids(ship);
    }

    #[test]
    fn the_parts_the_client_spawns_itself_are_bound_with_made_up_ids_and_without_the_mark() {
        const SHIP_CLASS: u32 = 0x1234_5678;
        let flags = 0x8020_1000;
        let part = |class_crc: u32, attach_id: u32, parent: Option<usize>, mark: u32| Part { class_crc, attach_id, flags: flags | mark, parent };
        // 0 seat, 1 mount, 2 gun (of the mount), 3 and 4 flagged (children of the gun, spawned by the client), 5 door.
        let parts = vec![
            part(10, 1, None, 0),
            part(11, 2, None, 0),
            part(12, 3, Some(1), 0),
            part(13, 4, Some(2), FLAG_DEFAULT_OF_PARENT),
            part(14, 5, Some(3), FLAG_DEFAULT_OF_PARENT),
            part(15, 6, None, 0),
        ];
        let table = ShipParts::parse(ShipParts::encode(&[(SHIP_CLASS, parts.clone(), Vec::new())])).unwrap();
        let ship = GEID_BASE + 12;
        let graph = |geid: u64, part: &Part| sc_api::internal::ShipPartRef { geid, class_crc: part.class_crc, attach_id: part.attach_id };
        let held: Vec<_> = [0, 1, 2, 5].iter().map(|index| graph(GEID_BASE + 7000 + *index as u64, &parts[*index])).collect();
        remember(ship, &held);
        let binds = part_binds_of(&table, ship, SHIP_CLASS, ALL_ON);
        let ids: Vec<u64> = binds.iter().map(|bind| bind.entity_id).collect();
        assert_eq!(ids, [GEID_BASE + 7000, GEID_BASE + 7001, GEID_BASE + 7002, part_id(ship, 3), part_id(ship, 4), GEID_BASE + 7005]);
        assert!(binds.iter().all(|bind| bind.flags == flags), "the mark never reaches the wire");
        assert_eq!(binds[4].parent_id, part_id(ship, 3));
        take_part_ids(ship);
        // A graph with every part (a tree made before the mark) is not the table's: made up ids, as for any mismatch.
        let all: Vec<_> = parts.iter().enumerate().map(|(index, part)| graph(GEID_BASE + 8000 + index as u64, part)).collect();
        remember(ship, &all);
        assert_eq!(part_binds_of(&table, ship, SHIP_CLASS, ALL_ON)[0].entity_id, part_id(ship, 0));
        take_part_ids(ship);
    }

    #[test]
    fn a_container_is_bound_as_the_official_service_binds_it() {
        let ship = GEID_BASE + 9;
        let binds = part_binds_of(installed(), ship, FIXTURE_SHIP, ALL_ON);
        let container = &binds[3];
        assert_eq!((container.entity_id, container.parent_id), (part_id(ship, 3), ship));
        assert_eq!((container.class_crc, container.flags, container.parent_attach_id), (0x5f1a_1634, 0x10_1000, 0xbab3_e001));
        assert!((container.transform.position[1] - -4.730_933).abs() < 1e-5);
        let value = |key: u32| container.snapshots.iter().find(|snapshot| snapshot.key == key).map(|snapshot| snapshot.value.clone());
        // As `red_ext_lg`: its GUID, socpak, radius (`e2c19041`) and the ship as parent and tile manager.
        assert_eq!(value(UH_GUID_KEY).unwrap()[..4], [0xa4, 0x41, 0x4e, 0x8b]);
        assert_eq!(value(OC_FILENAME_KEY).unwrap(), [0xe8, 0x58, 0x90, 0xf2]);
        assert_eq!(value(SOC_RADIUS_KEY).unwrap(), [0xe2, 0xc1, 0x90, 0x41]);
        assert_eq!(value(UH_PARENT_KEY).unwrap(), ship.to_le_bytes());
        assert_eq!(value(TILE_MANAGER_KEY).unwrap(), ship.to_le_bytes());
        // Its 15 statics take the ids after the container, and it has no dynamics.
        let first = part_id(ship, 4);
        let range = [first.to_le_bytes(), (first + 14).to_le_bytes()].concat();
        assert_eq!(value(0xe359_14ad).unwrap(), range);
        assert_eq!(value(0x09f4_d2d5).unwrap(), [0; 16]);
        take_part_ids(ship);
    }

    #[test]
    fn the_switches_leave_out_the_containers_or_their_content() {
        let ship = GEID_BASE + 10;
        assert_eq!(part_binds_of(installed(), ship, FIXTURE_SHIP, (false, true)).len(), 3);
        let binds = part_binds_of(installed(), ship, FIXTURE_SHIP, (true, false));
        assert_eq!(binds.len(), 4);
        assert!(binds[3].snapshots.iter().all(|snapshot| snapshot.key != 0xe359_14ad), "no ocIdRange");
        take_part_ids(ship);
    }

    #[test]
    fn a_ship_not_in_the_file_is_bound_alone() {
        assert!(part_binds(GEID_BASE + 7, MARAUDER).is_empty());
        assert!(part_binds_of(&ShipParts::default(), GEID_BASE + 8, FIXTURE_SHIP, ALL_ON).is_empty());
    }

    /// A `ship_parts.bin` that `tools/gamedata-build` wrote, to check by this variable; the test passes without it.
    const FILE_VARIABLE: &str = "SC_DGS_TEST_SHIP_PARTS";

    #[test]
    fn a_generated_file_has_the_marauder_and_the_f8c() {
        let Some(path) = std::env::var_os(FILE_VARIABLE) else {
            eprintln!("{FILE_VARIABLE} is not set: no generated file is checked");
            return;
        };
        let table = ShipParts::parse(std::fs::read(path).expect("the file")).expect("it parses");
        // Build 13165946: the Marauder and the Lightning F8C, with their interior and lights containers.
        for (class_crc, parts, containers) in [(MARAUDER, 104, 2), (0x7f0d_f83d, 106, 2)] {
            assert_eq!(table.parts(class_crc).map(|parts| parts.len()), Some(parts), "{class_crc:#010x}");
            assert_eq!(table.containers(class_crc).len(), containers, "{class_crc:#010x}");
            let binds = part_binds_of(&table, GEID_BASE + 3, class_crc, ALL_ON);
            assert_eq!(binds.len(), parts + containers);
            take_part_ids(GEID_BASE + 3);
        }
    }

    #[test]
    fn two_ships_have_blocks_that_do_not_overlap() {
        assert!(part_id(GEID_BASE + 1, PART_ID_STRIDE as usize - 1) < part_id(GEID_BASE + 2, 0));
    }
}
