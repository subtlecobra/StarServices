//! What the client needs bound to leave `eCVS_ReplicateSolarSystem`: a `SolarSystem` entity per star system.
//!
//! `WaitForSolarSystem` looks at replication records, not spawned entities: it walks down from the universe
//! root through children flagged unstreamable until it meets the class below. A record exists as soon as its
//! `BindBatchAddBind` arrives, so one bind under an already bound root ends the wait. States 12 and 13 need no
//! further bind, as `CacheSolarSystemStreamingGroupRoots` accepts a solar system with no group roots.

use crate::entity::{Bind, Snapshot, Transform, UNIVERSE_ROOT_ID};

/// GUID 2a0f6bf7-730d-430c-a5d6-6005d45e9625, and hard coded in the client's walk.
const SOLAR_SYSTEM_CLASS_CRC: u32 = 0x479c_0d1d;
/// `SeedMegaMap` spawns with 0x3000, replicated and unstreamable, and the class's spawn hook adds 0x400,
/// which marks a zone host. The client's walk needs 0x2000.
const SOLAR_SYSTEM_FLAGS: u32 = 0x3400;
/// The attachment type `SeedMegaMap` gets by giving a parent to the spawn: attached to its zone.
pub const ZONE_ATTACHMENT: u32 = 0x46c0_1eb2;
/// CRC of the `Default` class, which a seed spawns the quantum grid of a solar system with (0x142932aa0).
const DEFAULT_CLASS_CRC: u32 = 0xeed1_4dba;
/// As the seed spawns it, without the `0x400` of a zone host.
const QUANTUM_GRID_FLAGS: u32 = 0x3000;
/// FNV-1a of `EntityComponentQuantumGrid`, as `OC_PHYSICS_CONTROLLER` in `location`.
const QUANTUM_GRID_COMPONENT: u32 = 0x2bac_ecea;
/// CRC-32 of `ssquantumgridid`, the field the solar system finds its grid by.
const QUANTUM_GRID_ID_KEY: u32 = 0x116e_09ed;
/// The halves of FNV-1a 64 of `Struct`, `EntityId`, `m_value` and `uint64`, xored (0x1406189a0).
const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
/// The attachment of a plain child, which `SetParent` (0x1403b7500) writes for type 0, as the seed spawns the grid. The
/// zone attachment leaves the entity's `+0x120` link empty, and the grid reads its parent through it as its batch
/// completes (0x1429836f0), which is a null read at 0x142983817.
const CHILD_ATTACHMENT: u32 = 0x26b2_bbf1;
/// Above the tree, and below the first hangar.
const FIRST_QUANTUM_GRID_ID: u64 = 219_000_000_000;

pub struct SolarSystem {
    pub name: &'static str,
    pub geid: u64,
}

/// The `STAR_SYSTEM` nodes of `containers` for 4.10.2, which a new build can renumber. The entity must have
/// its node's id: `GetSolarSystemIdForLocationId` answers with the id of the node, not by asking the entity.
pub const SOLAR_SYSTEMS: [SolarSystem; 3] = [
    SolarSystem { name: "Stanton", geid: 199_998_000_001 },
    SolarSystem { name: "Pyro", geid: 200_001_275_310 },
    SolarSystem { name: "Nyx", geid: 200_002_429_787 },
];

/// A planet's id, by the `name` of its node in `containers` for 4.10.2, which a new build can renumber. The entity
/// must have this id: the client looks the graph node up by it (0x1470e8790) and only then reuses the tree's node, with
/// its children and location id. The planets of the rows of the home locations table (`location`).
const PLANETS: [(&str, u64); 6] = [
    ("stanton1", 199_998_000_004),
    ("stanton2", 200_000_314_300),
    ("stanton3", 199_999_082_807),
    ("stanton4", 199_999_559_856),
    ("pyro3", 200_002_185_940),
    ("pyro6", 200_001_891_234),
];

pub fn planet_id(name: &str) -> Option<u64> {
    PLANETS.iter().find(|planet| planet.0 == name).map(|planet| planet.1)
}

/// The binds of one batch for `systems`, all of them as `ViewBindSolarSystemRoot` does, then their quantum grids. The
/// batch needs the universe root bound first, or the client ends the connection with `Entity missing bound parent`.
///
/// A planet asks its system for the grid while it sets up (0x143afcb30), and a system with none crashes the client at
/// 0x1428cc398, so the grid goes before any planet.
pub fn solar_system_binds(systems: &[SolarSystem]) -> Vec<Bind> {
    let entries = || systems.iter().enumerate();
    let grids = entries().map(|(index, system)| quantum_grid_bind(index, system));
    entries().map(|(index, system)| solar_system_bind(index, system)).chain(grids).collect()
}

fn quantum_grid_id(index: usize) -> u64 {
    FIRST_QUANTUM_GRID_ID + index as u64
}

/// `index` is the system's place in `SOLAR_SYSTEMS`, which numbers its grid.
pub fn solar_system_bind(index: usize, system: &SolarSystem) -> Bind {
    Bind {
        entity_id: system.geid,
        class_crc: SOLAR_SYSTEM_CLASS_CRC,
        flags: SOLAR_SYSTEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: UNIVERSE_ROOT_ID,
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots: vec![Snapshot { key: QUANTUM_GRID_ID_KEY, type_id: ENTITY_ID_TYPE, value: quantum_grid_id(index).to_le_bytes().to_vec() }],
    }
}

pub fn quantum_grid_bind(index: usize, system: &SolarSystem) -> Bind {
    Bind {
        entity_id: quantum_grid_id(index),
        class_crc: DEFAULT_CLASS_CRC,
        flags: QUANTUM_GRID_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: system.geid,
        parent_attach_id: CHILD_ATTACHMENT,
        components: vec![QUANTUM_GRID_COMPONENT],
        snapshots: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_outside_the_range_the_client_keeps_for_itself() {
        assert!(SOLAR_SYSTEMS.iter().all(|system| system.geid > 0x2e_90cf_4b7f));
    }

    #[test]
    fn a_planet_has_the_id_of_its_tree_node() {
        assert_eq!(planet_id("pyro3"), Some(200_002_185_940));
        assert_eq!(planet_id("pyro"), None);
    }

    #[test]
    fn a_subset_binds_only_those_systems() {
        let pyro = [SolarSystem { name: "Pyro", geid: 200_001_275_310 }];
        let binds = solar_system_binds(&pyro);
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].entity_id, 200_001_275_310);
    }

    #[test]
    fn a_system_names_its_quantum_grid_which_hangs_from_it() {
        let binds = solar_system_binds(&SOLAR_SYSTEMS);
        let (systems, grids) = binds.split_at(SOLAR_SYSTEMS.len());
        for (system, grid) in systems.iter().zip(grids) {
            assert_eq!(system.snapshots[0].value, grid.entity_id.to_le_bytes());
            assert_eq!((grid.parent_id, grid.parent_attach_id), (system.entity_id, CHILD_ATTACHMENT));
            assert_eq!(grid.components, [QUANTUM_GRID_COMPONENT]);
            assert_eq!((grid.class_crc, grid.flags), (DEFAULT_CLASS_CRC, 0x3000));
        }
    }

    #[test]
    fn the_grid_ids_sit_between_the_tree_and_the_hangars() {
        assert!(FIRST_QUANTUM_GRID_ID > SOLAR_SYSTEMS[2].geid);
        assert!(FIRST_QUANTUM_GRID_ID + 3 < crate::location::hangar_id(0));
    }
}
