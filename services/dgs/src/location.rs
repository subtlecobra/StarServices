//! Where a player spawns: the hangar of its home location.
//!
//! A home location is a `LocationId`, the class CRC of a `StarMapObject` GUID, as `player_home_location` stores it.
//! The home locations table maps it to the location's first default hangar. That table, the hangars' content and the
//! entity counts come from Data.p4k through `tools/gamedata-build`, per game build, and reach the DGS through the
//! region's `sc-gamedata` at startup (`crate::gamedata`), which `install`s them before anything here is asked. Tests
//! read the copies in `testdata/` instead, which `scripts/game-files.sh` generates from the game (the `fixtures` feature
//! lets other crates' tests do so too).
//!
//! A real shard parents the player to an instance of the hangar class, with the zone attachment. The hangar is a
//! dynamic entity, not a node of the tree the client downloads. The client finds the socpak locally by the hash in
//! `ocFilename` and makes a graph node for it, but no ids for its content: those come from the DGS as `ocIdRange`, in
//! one block above the hangar's id, as a real server's do.
//!
//! The hangar's zone hangs from the planet's, which hangs from the solar system's, see the protocol notes (kept outside this repository). A planet is
//! the entity of its tree node, so unlike the hangar it reuses a node the client already has. With the planet zone off
//! the hangar goes to the origin of its system, the placement that has joined on the real game.
//!
//! Nothing here knows of connections.

use std::{collections::HashMap, sync::OnceLock};

use crate::entity::{Bind, Snapshot, Transform};
use crate::player::{player_index, Placement, PLAYER_ATTACH_ID};
use crate::solar_system::{planet_id, SOLAR_SYSTEMS, ZONE_ATTACHMENT};
use tracing::warn;

/// `StarMapObject.RR_P3_LEO`, Orbituary. Real sessions log it as the location of every route from there.
pub const ORBITUARY: u32 = 848_868_938;
/// The residence of the only character so far, for a session that names none.
const DEFAULT_HOME_LOCATION: u32 = ORBITUARY;
/// CRC-32 of `ocfilename`.
const OC_FILENAME_KEY: u32 = 0xffa1_bce6;
/// CRC-32 of `entsocpak`: the socpak that holds an entity's XML, by the hash of `ocFilename`.
const ENT_SOCPAK_KEY: u32 = 0x608b_db8c;
/// CRC-32 of `entcrc`: the name of an entity's XML in that socpak's `entdata`.
const ENT_CRC_KEY: u32 = 0xa887_9f42;
/// The halves of FNV-1a 64 of `uint32`, xored (0x14035a9f0).
const UINT32_TYPE: u32 = 0x5d4e_4bea;
/// `0x1000` is `ENTITY_FLAG_REPLICATION_ROOT`, needed as a streamable child of an unstreamable system, and a real
/// hangar is named `StreamingSOC_`, which 0x142925680 skips for 0x2000. `0x400` is a zone host: a server's own
/// `PrepareObjectContainer` adds it for a socpak with a `<PhysicsGrid>`, and 0x1415efb20 wants it on the record of
/// any parent with a zone attached child. The client adds it to its spawn alone, not to the record.
const HANGAR_FLAGS: u32 = 0x1400;
/// FNV-1a of `ObjectContainerPhysicsController`, as registry slot +0x58 hashes it. A running client's hangar lacked
/// this component, and with it the grid that makes the hangar a zone host.
pub(crate) const OC_PHYSICS_CONTROLLER: u32 = 0x702b_110a;
/// `AddChildrenToBatch` (0x1470c85e0) spawns an unstreamable container with `0x3000`, and `0x400` for a node with a
/// planet (0x14058e0a0). A client's spawn hook takes the `0x2000` path to `PrepareObjectContainer` (0x1429322f0).
const PLANET_FLAGS: u32 = 0x3400;
/// Above the tree and the game mode, and below the GEIDs issued to players.
const FIRST_HANGAR_ID: u64 = 220_000_000_000;
/// Each player's hangar keeps room, as a real hangar's content takes ids just after its own.
const HANGAR_ID_STRIDE: u64 = 0x1_0000;
/// With no hangar to go to, a player waits at the origin of this system.
const FALLBACK_SYSTEM: &str = "Pyro";
/// The paths of the tables in a game build, as `tools/gamedata-build` writes them and `sc-gamedata` serves them.
/// The home locations, one per row.
pub const HOME_LOCATIONS_PATH: &str = "tables/home_locations.tsv";
/// The containers inside each hangar socpak, at any depth, in the hangar's frame.
pub const HANGAR_CONTENT_PATH: &str = "tables/hangar_content.tsv";
/// Per socpak, by hash: `entityCountStatic`, `entityCountDynamic` and `entityCountALE` of its metadata, then its
/// number of `<TileXmlEntry><TileItemPortEntries><ItemPort>`.
pub const ENTITY_COUNTS_PATH: &str = "tables/entity_counts.tsv";
/// The copies of the tables in `testdata/`, for tests.
#[cfg(test)]
const TABLE: crate::testdata::Text = crate::testdata::Text(HOME_LOCATIONS_PATH);
#[cfg(test)]
const CONTENT: crate::testdata::Text = crate::testdata::Text(HANGAR_CONTENT_PATH);
/// What `AddChildrenToBatch` (0x1470c85e0) gives a child container that is no tree node and no zone host:
/// `ENTITY_FLAG_REPLICATION_ROOT` alone. No `0x2000`, as the client makes the graph node of a container bound without
/// it (0x1467b5750), as for the hangar.
const CONTENT_FLAGS: u32 = 0x1000;
/// CRC-32 of `ocidrange`: the ids of a container's static entities, the `.soc`'s entity chunk.
const OC_ID_RANGE_KEY: u32 = 0xe359_14ad;
/// CRC-32 of `ocidrangemov`: the ids of its dynamic entities.
const OC_ID_RANGE_MOV_KEY: u32 = 0x09f4_d2d5;
/// The serializer of `CEntityIdRange` (0x140edf9d0), read from a running client's cache of it (0x149ff13a0).
const ENTITY_ID_RANGE_TYPE: u32 = 0xfb3e_0faa;
#[cfg(test)]
const ENTITY_COUNTS: crate::testdata::Text = crate::testdata::Text(ENTITY_COUNTS_PATH);
/// FNV-1a of `ItemPortContainer` and of `Tile`, hashed as `OC_PHYSICS_CONTROLLER`. A static of a container's `.soc`
/// with `TileAttachmentParams`, such as a fire extinguisher cabinet, attaches to the container's entity at the port
/// `ocTile<ItemPortName>` (0x146827030, key CRC-32C of the lowercased name). Those ports are made by the `TileXmlEntry`
/// handler (0x1449ea440, 0x143d51fa0, 0x143d4b690), which needs a `Tile` component on the container's entity and
/// adds each port to its `ItemPortContainer` without a null check. No class in `Game2.dcb` has `TileParams`, and
/// `ObjectContainerModifier` has no item port container, so the bind lists both, the container first.
const ITEM_PORT_CONTAINER: u32 = 0xc523_11dc;
const TILE: u32 = 0xb272_936b;
/// CRC-32 of `sochasmetadata`. The handler only runs from the container component's load (0x142925680), which calls
/// the metadata stream-in callbacks (0x1428f9290) with the socpak's xml only when the component read this snapshot
/// as true (0x1428e5f40). A server's `CreateStreamingBatch` (0x1470df4d2) writes it as true for a node whose xml has a
/// node with a callback (`+0x25e`, 0x1470ffe2a).
const SOC_HAS_METADATA_KEY: u32 = 0x06d0_5294;
/// CRC-32 of `basegeometryresource.screengeometryoverride.geometry` and `...material`, `CBaseEntityGeometryResource`'s
/// string variables (0x1428e4960). A non-empty geometry wins over the class's and the instance's (`RefreshGeometryInt`,
/// 0x142943dc0), so it gives a bound gateway entity the model its `.soc` gives it, which no `entdata` holds.
const GEOMETRY_OVERRIDE_KEY: u32 = 0xedfc_cdfc;
const MATERIAL_OVERRIDE_KEY: u32 = 0x0410_b6c2;
/// The halves of FNV-1a 64 of `bool`, xored (0x14035af50), as `player.rs`'s.
const BOOL_TYPE: u32 = 0x0b9f_c026;

/// The entities of a container that the DGS binds itself instead of letting the client make them from `ocIdRange`:
/// those of a gateway prefab (`crate::transit::PrefabEntity`). The client makes a `.soc`'s statics in a local batch
/// with flag `0x10` (`CreateImmovableEntities`, 0x1470d5b80), so it has authority over them and runs their
/// interactions locally: a press on such a call panel never reaches the network. Bound as replicated entities, as
/// official servers bind an instanced hangar's content, the panel sends `RmAuthorityRequestInteract` to the server
/// (seen on a running client) and the door is the server's to open. Such a container gets no id ranges, so the client
/// makes none of its statics; their ids stay reserved and each entity takes the one the client would have given it.
/// Empty for any other container, and for a prefab whose table does not list exactly its statics.
fn server_bound(socpak_hash: u32) -> &'static [crate::transit::PrefabEntity] {
    let entities = crate::transit::tables().prefab_entities(socpak_hash);
    let (statics, _) = entity_counts(socpak_hash);
    if entities.len() as u64 != statics {
        if !entities.is_empty() {
            warn!(socpak = socpak_hash, statics, listed = entities.len(), "a gateway prefab's entities do not match its statics: the client makes them");
        }
        return &[];
    }
    entities
}

/// The planet of a location. `class_crc` is its `<Child>`'s `OrbitingObjectContainer`.
#[derive(Clone, Debug, PartialEq)]
pub struct Planet {
    /// The `name` of its node in `containers`.
    pub name: String,
    pub class_crc: u32,
    pub socpak_hash: u32,
    /// The socpak that lists the planet's `<Child>`, usually its system's, which holds the entity's XML.
    pub entity_socpak_hash: u32,
    /// CRC-32C of the `<Child>`'s `guid` as text, the name of that XML.
    pub entity_crc: u32,
    /// Relative to the solar system, composed through the star, whose `rot` turns its children.
    pub transform: Transform,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HomeLocation {
    pub location_id: u32,
    pub name: String,
    /// The `name` of an entry of `SOLAR_SYSTEMS`.
    pub system: String,
    pub hangar_class: String,
    pub hangar_class_crc: u32,
    pub socpak_hash: u32,
    /// `InstancedInteriorComponentParams.spawnTransform`. Taken to be relative to the hangar, which is unconfirmed.
    pub spawn: Transform,
    /// `None` for a location with no planet above it in the tree, such as a Lagrange station.
    pub planet: Option<Planet>,
    /// The station's `<Child>`, relative to the planet or, with none, to the solar system. A live hangar is about
    /// 1150 m from its station's origin, which nothing read explains, so this is near the real place, not at it.
    pub hangar: Transform,
}

fn transform(numbers: &[f64]) -> Option<Transform> {
    let [x, y, z, qx, qy, qz, qw] = numbers else { return None };
    Some(Transform { position: [*x, *y, *z], rotation: [*qx, *qy, *qz, *qw] })
}

fn parse(row: &str) -> Option<HomeLocation> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [location_id, name, system, hangar_class, hangar_class_crc, socpak_hash, planet, planet_class_crc, planet_socpak_hash, planet_entity_socpak_hash, planet_entity_crc, numbers @ ..] = fields.as_slice() else {
        return None;
    };
    let numbers: Vec<f64> = numbers.iter().map(|number| number.parse().ok()).collect::<Option<_>>()?;
    let [spawn, planet_transform, hangar] = [numbers.get(..7)?, numbers.get(7..14)?, numbers.get(14..)?];
    let planet_row = Planet {
        name: planet.to_string(),
        class_crc: planet_class_crc.parse().ok()?,
        socpak_hash: planet_socpak_hash.parse().ok()?,
        entity_socpak_hash: planet_entity_socpak_hash.parse().ok()?,
        entity_crc: planet_entity_crc.parse().ok()?,
        transform: transform(planet_transform)?,
    };
    Some(HomeLocation {
        location_id: location_id.parse().ok()?,
        name: name.to_string(),
        system: system.to_string(),
        hangar_class: hangar_class.to_string(),
        hangar_class_crc: hangar_class_crc.parse().ok()?,
        socpak_hash: socpak_hash.parse().ok()?,
        spawn: transform(spawn)?,
        planet: (!planet.is_empty()).then_some(planet_row),
        hangar: transform(hangar)?,
    })
}

/// The hangar socpaks of the home locations of `system`, each once.
pub fn hangar_socpaks(system: &str) -> Vec<u32> {
    let locations = tables().locations.iter();
    let mut hashes: Vec<u32> = locations.filter(|location| location.system.eq_ignore_ascii_case(system)).map(|location| location.socpak_hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

pub fn home_location(location_id: u32) -> Option<HomeLocation> {
    tables().locations.iter().find(|location| location.location_id == location_id).cloned()
}

/// One per player, as a real hangar is an instance of its owner.
pub fn hangar_id(player_index: u64) -> u64 {
    FIRST_HANGAR_ID + player_index * HANGAR_ID_STRIDE
}

/// A container inside a hangar socpak, such as `hangar_smlfrnt_002` in `hangar_smlfrnt_002_rest_rund`, which holds the
/// shell and the floor of the hangar as brushes of its `.soc`.
#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub hangar_socpak_hash: u32,
    pub socpak_hash: u32,
    pub class_crc: u32,
    /// The socpak that holds its entity XML, and that XML's name, or 0 for a `<Child>` with no `hasAdditionalData`.
    pub entity_socpak_hash: u32,
    pub entity_crc: u32,
    pub name: String,
    /// In the hangar's frame.
    pub transform: Transform,
}

fn parse_content(row: &str) -> Option<Content> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [hangar, socpak, class_crc, entity_socpak, entity_crc, name, numbers @ ..] = fields.as_slice() else { return None };
    let numbers: Vec<f64> = numbers.iter().map(|number| number.parse().ok()).collect::<Option<_>>()?;
    Some(Content {
        hangar_socpak_hash: hangar.parse().ok()?,
        socpak_hash: socpak.parse().ok()?,
        class_crc: class_crc.parse().ok()?,
        entity_socpak_hash: entity_socpak.parse().ok()?,
        entity_crc: entity_crc.parse().ok()?,
        name: name.to_string(),
        transform: transform(&numbers)?,
    })
}

/// The containers of a hangar socpak, in the order of its XML.
pub fn hangar_content(socpak_hash: u32) -> Vec<Content> {
    tables().content.iter().filter(|content| content.hangar_socpak_hash == socpak_hash).cloned().collect()
}

/// A row of the entity counts: the socpak's hash, then statics, dynamics, ALEs and tile item ports.
fn parse_counts(row: &str) -> Option<(u32, [u64; 4])> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [hash, statics, dynamics, ale, tile_ports] = fields.as_slice() else { return None };
    Some((hash.parse().ok()?, [statics.parse().ok()?, dynamics.parse().ok()?, ale.parse().ok()?, tile_ports.parse().ok()?]))
}

/// The row of a socpak in the entity counts: statics, dynamics, ALEs and tile item ports.
fn counts_row(socpak_hash: u32) -> Option<[u64; 4]> {
    tables().counts.get(&socpak_hash).copied()
}

/// The three tables of the shard's game build, parsed.
#[derive(Debug, Default)]
pub struct Tables {
    locations: Vec<HomeLocation>,
    content: Vec<Content>,
    counts: HashMap<u32, [u64; 4]>,
}

/// Every non-empty line of `text`, parsed; a line that does not parse fails the table, as it means a broken build.
fn rows<T>(name: &str, text: &str, parse: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    let lines = text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty());
    lines.map(|(index, line)| parse(line).ok_or_else(|| format!("{name}: line {} does not parse", index + 1))).collect()
}

impl Tables {
    /// The tables from their text, as `tools/gamedata-build` writes them.
    pub fn parse(home_locations: &str, hangar_content: &str, entity_counts: &str) -> Result<Self, String> {
        let locations = rows(HOME_LOCATIONS_PATH, home_locations, parse)?;
        if locations.is_empty() {
            return Err(format!("{HOME_LOCATIONS_PATH}: no home locations"));
        }
        let content = rows(HANGAR_CONTENT_PATH, hangar_content, parse_content)?;
        let counts = rows(ENTITY_COUNTS_PATH, entity_counts, parse_counts)?.into_iter().collect();
        Ok(Self { locations, content, counts })
    }

    pub fn locations(&self) -> usize {
        self.locations.len()
    }

    /// The copies in `testdata/`, generated from the game by `scripts/game-files.sh`, for tests.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn fixtures() -> Self {
        use crate::testdata::text;
        Self::parse(text(HOME_LOCATIONS_PATH), text(HANGAR_CONTENT_PATH), text(ENTITY_COUNTS_PATH)).expect("the fixture tables parse")
    }
}

static TABLES: OnceLock<Tables> = OnceLock::new();

/// Sets the tables for the life of the process, once, at startup. `false` when they were already set.
pub fn install(tables: Tables) -> bool {
    TABLES.set(tables).is_ok()
}

#[cfg(any(test, feature = "fixtures"))]
fn tables() -> &'static Tables {
    TABLES.get_or_init(Tables::fixtures)
}

#[cfg(not(any(test, feature = "fixtures")))]
fn tables() -> &'static Tables {
    TABLES.get().expect("the game data tables are installed at startup, before anything asks for a location")
}

/// The static and dynamic entity counts of a socpak, 0 for one not in the table.
fn entity_counts(socpak_hash: u32) -> (u64, u64) {
    counts_row(socpak_hash).map(|[statics, dynamics, ..]| (statics, dynamics)).unwrap_or_default()
}

/// The components a container's bind lists: the item port container and tile that its tile ports need, if it has any.
fn content_components(socpak_hash: u32) -> Vec<u32> {
    match counts_row(socpak_hash) {
        Some([.., tile_ports]) if tile_ports > 0 => vec![ITEM_PORT_CONTAINER, TILE],
        _ => Vec::new(),
    }
}

/// A `CEntityIdRange`: the first and the last id, both included. `(0, 0)` holds none, as a server leaves it for a
/// count of 0 (0x1470feb50).
pub type IdRange = (u64, u64);

/// The ids of one container's content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentIds {
    pub statics: IdRange,
    pub dynamics: IdRange,
}

impl ContentIds {
    /// Takes `statics` then `dynamics` ids from `next`, in the order a server's parser splits its block (0x1470feb50).
    fn take(next: &mut u64, (statics, dynamics): (u64, u64)) -> Self {
        let mut range = |count: u64| {
            let range = if count == 0 { (0, 0) } else { (*next, *next + count - 1) };
            *next += count;
            range
        };
        let statics = range(statics);
        Self { statics, dynamics: range(dynamics) }
    }

    fn is_empty(&self) -> bool {
        self.statics == (0, 0) && self.dynamics == (0, 0)
    }

    /// `ocIdRange` and `ocIdRangeMov`, as a server's `PrepareObjectContainer` writes them (0x1470b82d0): `m_firstId`
    /// then `m_lastId`, each a little endian `u64` (reader 0x1427c2820). None for a container with no content, which
    /// leaves the client's own preparation as it was.
    fn snapshots(&self) -> Vec<Snapshot> {
        if self.is_empty() {
            return Vec::new();
        }
        let range = |key: u32, (first, last): IdRange| Snapshot {
            key,
            type_id: ENTITY_ID_RANGE_TYPE,
            value: [first.to_le_bytes(), last.to_le_bytes()].concat(),
        };
        vec![range(OC_ID_RANGE_KEY, self.statics), range(OC_ID_RANGE_MOV_KEY, self.dynamics)]
    }
}

/// Where the ids of a hangar's content go, all inside its stride: the hangar's own statics and dynamics just above its
/// id, then the ids of the containers inside it, then the content of each container in the order of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HangarIds {
    pub hangar: ContentIds,
    /// One per row of `hangar_content`: the container's entity and its content.
    pub containers: Vec<(u64, ContentIds)>,
    /// The first id after them all.
    pub end: u64,
}

impl HangarIds {
    fn new(hangar_id: u64, socpak_hash: u32, content: &[Content]) -> Self {
        let mut next = hangar_id + 1;
        let hangar = ContentIds::take(&mut next, entity_counts(socpak_hash));
        let first_container = next;
        next += content.len() as u64;
        let containers = content
            .iter()
            .zip(first_container..)
            .map(|(content, entity_id)| (entity_id, ContentIds::take(&mut next, entity_counts(content.socpak_hash))))
            .collect();
        Self { hangar, containers, end: next }
    }
}

/// One player's hangar: the entity to bind, and where that puts the player.
#[derive(Clone, Debug, PartialEq)]
pub struct HomeHangar {
    pub location: HomeLocation,
    pub entity_id: u64,
    system_id: u64,
    /// The planet's entity, when the location has one and the planet zone is on.
    planet_id: Option<u64>,
    transform: Transform,
}

fn uint32_snapshot(key: u32, value: u32) -> Snapshot {
    Snapshot { key, type_id: UINT32_TYPE, value: value.to_le_bytes().to_vec() }
}

fn filename_snapshot(socpak_hash: u32) -> Snapshot {
    uint32_snapshot(OC_FILENAME_KEY, socpak_hash)
}

impl HomeHangar {
    /// `None` for a location with no hangar, in a system that is not bound, or on a planet with no known id.
    pub fn new(location_id: u32, player_index: u64, planet_zone: bool) -> Option<Self> {
        let location = home_location(location_id)?;
        let system_id = SOLAR_SYSTEMS.iter().find(|system| system.name == location.system)?.geid;
        let planet_id = match &location.planet {
            Some(planet) if planet_zone => Some(planet_id(&planet.name)?),
            _ => None,
        };
        let transform = if planet_zone { location.hangar } else { Transform::IDENTITY };
        Some(Self { location, entity_id: hangar_id(player_index), system_id, planet_id, transform })
    }

    /// The tree node's own entity. With `ocFilename` alone the client runs the whole preparation on the reused node,
    /// and `CreatePlanetEntity` (0x1470dd970) adds the components of the socpak's `pivot.entxml`, `ProceduralEntity`,
    /// whose class hook (0x142715420) makes it a zone host. It takes no `ObjectContainerPhysicsController`: a planet's
    /// socpak has no `<PhysicsGrid>`, and only that sets node `+0x25d` (0x1470feb50). It is unstreamable, so it goes with
    /// the solar systems, well before the hangar and the player.
    ///
    /// A bind has no name. `entSocpak` and `entCrc` point the spawn (0x146847dc0) at the entity's XML, whose `Name` it
    /// takes, and without them it names the entity `<class>_<id>`. The zone copies the name when it is made.
    pub fn planet_bind(&self) -> Option<Bind> {
        let planet = self.location.planet.as_ref()?;
        Some(Bind {
            entity_id: self.planet_id?,
            class_crc: planet.class_crc,
            flags: PLANET_FLAGS,
            transform: planet.transform,
            parent_id: self.system_id,
            parent_attach_id: ZONE_ATTACHMENT,
            components: Vec::new(),
            snapshots: vec![
                filename_snapshot(planet.socpak_hash),
                uint32_snapshot(ENT_SOCPAK_KEY, planet.entity_socpak_hash),
                uint32_snapshot(ENT_CRC_KEY, planet.entity_crc),
            ],
        })
    }

    /// The ids of the hangar's content, which the client takes from the binds, see `content_ids`.
    pub fn ids(&self) -> HangarIds {
        HangarIds::new(self.entity_id, self.location.socpak_hash, &hangar_content(self.location.socpak_hash))
    }

    /// Under the planet, or the solar system with no planet.
    ///
    /// The client makes the graph node of a container bound as a dynamic entity itself, with no id ranges
    /// (`CEntitySpawnBatch::AddEntityInternal`, 0x1467b5750), and its `PrepareObjectContainer` reuses that node and
    /// parses nothing, so it would write empty ranges. With `ocIdRange` the class hook (0x1429322f0) skips that
    /// preparation, finds the same node, and the component hands the range to `ExecuteAllLoads` (0x1470e5390), which
    /// creates the `.soc`'s static entities, the lights among them. As that preparation no longer runs, the bind lists
    /// the physics controller it would add.
    pub fn bind(&self) -> Bind {
        let mut snapshots = vec![filename_snapshot(self.location.socpak_hash)];
        snapshots.extend(self.ids().hangar.snapshots());
        Bind {
            entity_id: self.entity_id,
            class_crc: self.location.hangar_class_crc,
            flags: HANGAR_FLAGS,
            transform: self.transform,
            parent_id: self.planet_id.unwrap_or(self.system_id),
            parent_attach_id: ZONE_ATTACHMENT,
            components: vec![OC_PHYSICS_CONTROLLER],
            snapshots,
        }
    }

    /// The containers inside the hangar, each its own entity under the hangar with the zone attachment, as a server's
    /// `AddChildToBatch` (0x1470c85e0) spawns them: `ocFilename`, and `entSocpak` with `entCrc` for one whose entity XML
    /// the listing socpak holds, then the ranges of its content as for the hangar. Their ids follow the hangar's
    /// statics, as on live, see `HangarIds`. A deeper container goes under the hangar as well, in the hangar's frame,
    /// as the hangar is the nearest zone host (0x1406bfee0).
    ///
    /// The client skips these in its own `AddChildrenToBatch`, as their graph nodes have no ids (see the protocol notes (kept outside this repository)),
    /// and the hangar's shell and floor are brushes of the `.soc` of one of them, so without them there is no floor.
    /// One with tile item ports lists the components they need, see `TILE`.
    ///
    /// The entities of the gateway prefabs follow (`server_bound`), under the hangar as well.
    pub fn content_binds(&self) -> Vec<Bind> {
        let content = hangar_content(self.location.socpak_hash);
        let ids = HangarIds::new(self.entity_id, self.location.socpak_hash, &content).containers;
        let mut binds: Vec<Bind> = content
            .iter()
            .zip(&ids)
            .map(|(content, &(entity_id, content_ids))| {
                let mut snapshots = vec![filename_snapshot(content.socpak_hash)];
                if content.entity_crc != 0 {
                    snapshots.push(uint32_snapshot(ENT_SOCPAK_KEY, content.entity_socpak_hash));
                    snapshots.push(uint32_snapshot(ENT_CRC_KEY, content.entity_crc));
                }
                if server_bound(content.socpak_hash).is_empty() {
                    snapshots.extend(content_ids.snapshots());
                }
                let components = content_components(content.socpak_hash);
                if !components.is_empty() {
                    snapshots.push(Snapshot { key: SOC_HAS_METADATA_KEY, type_id: BOOL_TYPE, value: vec![1] });
                }
                Bind {
                    entity_id,
                    class_crc: content.class_crc,
                    flags: CONTENT_FLAGS,
                    transform: content.transform,
                    parent_id: self.entity_id,
                    parent_attach_id: ZONE_ATTACHMENT,
                    components,
                    snapshots,
                }
            })
            .collect();
        binds.extend(self.server_static_binds(&content, &ids));
        binds
    }

    /// The binds of the gateway prefabs' entities (`server_bound`): class defaults, and the instance's geometry and
    /// material as overrides.
    fn server_static_binds(&self, content: &[Content], ids: &[(u64, ContentIds)]) -> Vec<Bind> {
        let mut binds = Vec::new();
        for (content, (_, content_ids)) in content.iter().zip(ids) {
            let (first, _) = content_ids.statics;
            for entity in server_bound(content.socpak_hash) {
                binds.push(Bind {
                    entity_id: first + entity.index,
                    class_crc: entity.class_crc,
                    flags: CONTENT_FLAGS,
                    transform: content.transform.then(&entity.pose),
                    parent_id: self.entity_id,
                    parent_attach_id: ZONE_ATTACHMENT,
                    components: Vec::new(),
                    snapshots: geometry_overrides(entity),
                });
            }
        }
        binds
    }

    /// The gateway prefabs among the containers of `content_binds`, with their entities' ids and places.
    pub fn gateway_prefabs(&self) -> Vec<GatewayPrefab> {
        let content = hangar_content(self.location.socpak_hash);
        let ids = HangarIds::new(self.entity_id, self.location.socpak_hash, &content).containers;
        let mut prefabs = Vec::new();
        for (content, (_, content_ids)) in content.iter().zip(&ids) {
            let entities = server_bound(content.socpak_hash);
            if entities.is_empty() {
                continue;
            }
            let mut prefab = GatewayPrefab { name: content.name.clone(), gateway: None, panel: None, door: None, limbo: None };
            for entity in entities {
                let placed = Some(Placed { entity_id: content_ids.statics.0 + entity.index, transform: content.transform.then(&entity.pose) });
                match entity.class_crc {
                    GATEWAY_CLASS | TRANSPORT_GATEWAY_CLASS => prefab.gateway = placed,
                    CALL_PANEL_CLASS | TRANSPORT_PANEL_CLASS => prefab.panel = placed,
                    LIMBO_NODE_CLASS => prefab.limbo = placed,
                    _ => prefab.door = placed,
                }
            }
            prefabs.push(prefab);
        }
        prefabs
    }

    /// The call panels among the entities of `content_binds`.
    pub fn call_panels(&self) -> Vec<CallPanel> {
        let prefabs = self.gateway_prefabs().into_iter();
        // A Transport control panel's interactions are not a Transit panel's, and are not handled yet.
        let transit = prefabs.filter(|prefab| prefab.limbo.is_some());
        let panels = transit.filter_map(|prefab| Some(CallPanel { entity_id: prefab.panel?.entity_id, hangar_id: self.entity_id, gateway: prefab.name }));
        panels.collect()
    }

    pub fn placement(&self) -> Placement {
        Placement { parent_id: self.entity_id, attach_id: ZONE_ATTACHMENT, transform: self.location.spawn }
    }
}

/// `TransitInteractionUIPanel_Elevator_Ext`, the class of every hangar elevator's call panel.
pub const CALL_PANEL_CLASS: u32 = 3_016_690_202;

/// `TransitGateway` and `TransitLimboNode`, the classes of a gateway prefab's gateway and limbo node.
pub const GATEWAY_CLASS: u32 = 3_160_160_724;
pub const LIMBO_NODE_CLASS: u32 = 157_523_774;
/// The door classes of the gateway prefabs and the cab interiors: `ElevatorDoorDefaultTemplate` and its lowtech and
/// Levski kin, and Area18's Transport door.
pub const DOOR_CLASSES: [u32; 4] = [257_190_916, 621_302_575, 415_121_244, 4_092_652_084];
/// The gateway and the control panel of Area18's Transport access point, which has no limbo node.
const TRANSPORT_GATEWAY_CLASS: u32 = 2_791_990_256;
const TRANSPORT_PANEL_CLASS: u32 = 3_633_828_005;

/// An entity of a gateway prefab that the DGS binds, with where it is in the hangar's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub entity_id: u64,
    pub transform: Transform,
}

/// The entities of one gateway prefab in a hangar, by role: the gateway, the call panel, the door and the limbo node.
/// The door is the entity of none of the other classes.
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayPrefab {
    /// The name of the prefab's `<Child>` in the hangar.
    pub name: String,
    pub gateway: Option<Placed>,
    pub panel: Option<Placed>,
    pub door: Option<Placed>,
    pub limbo: Option<Placed>,
}

/// A hangar elevator's call panel that the DGS binds, see `server_bound`.
#[derive(Clone, Debug, PartialEq)]
pub struct CallPanel {
    pub entity_id: u64,
    /// The hangar's entity.
    pub hangar_id: u64,
    /// The name of the gateway prefab's `<Child>` in the hangar, such as `playerhangar_elev_hangar_ht_gateway-014`.
    pub gateway: String,
}

/// The geometry and material of a prefab entity as override snapshots, each only when the instance has its own.
pub(crate) fn geometry_overrides(entity: &crate::transit::PrefabEntity) -> Vec<Snapshot> {
    use crate::universe::{string_value, STRING_TYPE};
    [(GEOMETRY_OVERRIDE_KEY, &entity.geometry), (MATERIAL_OVERRIDE_KEY, &entity.material)]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| Snapshot { key, type_id: STRING_TYPE, value: string_value(value) })
        .collect()
}

/// A player's hangar: `location` as the session names it, or the default. `None` for one with no usable row, or an
/// account from before the first character.
pub fn player_home(account: u64, location: Option<u32>, planet_zone: bool) -> Option<HomeHangar> {
    let location = location.unwrap_or(DEFAULT_HOME_LOCATION);
    let home = HomeHangar::new(location, player_index(account)?, planet_zone);
    if home.is_none() {
        warn!(location, "home location has no hangar in the table");
    }
    home
}

/// What governs a place: its system and planet, the fallback system's root for a location with no hangar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Governed {
    pub system: String,
    pub planet: Option<String>,
}

impl Governed {
    /// The root that claims this place. Only a planet with a root of its own is told apart, as in the replicant's
    /// `Directory`.
    pub fn root(&self) -> String {
        self.planet.clone().unwrap_or_else(|| format!("{}system", self.system.to_ascii_lowercase()))
    }
}

/// Depends on the location alone, so any DGS can answer it before a player has one.
pub fn governed_place(location: Option<u32>) -> Governed {
    let row = HomeHangar::new(location.unwrap_or(DEFAULT_HOME_LOCATION), 0, true).map(|home| home.location);
    match row {
        Some(location) => Governed { system: location.system, planet: location.planet.map(|planet| planet.name) },
        None => Governed { system: FALLBACK_SYSTEM.to_string(), planet: None },
    }
}

/// The planets of the table, once each, for what a DGS holds whoever joins.
pub fn planets() -> Vec<Planet> {
    let mut planets: Vec<Planet> = tables().locations.iter().filter_map(|location| location.planet.clone()).collect();
    planets.sort_by(|left, right| left.name.cmp(&right.name));
    planets.dedup_by(|left, right| left.name == right.name);
    planets
}

/// Where a player goes with no hangar.
pub fn fallback_placement() -> Option<Placement> {
    let parent_id = SOLAR_SYSTEMS.iter().find(|system| system.name == FALLBACK_SYSTEM)?.geid;
    Some(Placement { parent_id, attach_id: PLAYER_ATTACH_ID, transform: Transform::IDENTITY })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(text: &[u8]) -> u32 {
        !text.iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        })
    }

    #[test]
    fn the_snapshot_is_keyed_and_typed_as_the_client_computes_it() {
        assert_eq!(OC_FILENAME_KEY, crc32(b"ocfilename"));
        assert_eq!(ENT_SOCPAK_KEY, crc32(b"entsocpak"));
        assert_eq!(ENT_CRC_KEY, crc32(b"entcrc"));
        assert_eq!(OC_ID_RANGE_KEY, crc32(b"ocidrange"));
        assert_eq!(OC_ID_RANGE_MOV_KEY, crc32(b"ocidrangemov"));
        assert_eq!(SOC_HAS_METADATA_KEY, crc32(b"sochasmetadata"));
        let fnv = b"uint32".iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3));
        assert_eq!(UINT32_TYPE, (fnv >> 32) as u32 ^ fnv as u32);
    }

    #[test]
    fn orbituary_is_the_small_rest_stop_hangar_of_pyro() {
        let location = home_location(ORBITUARY).unwrap();
        assert_eq!((location.name.as_str(), location.system.as_str()), ("RR_P3_LEO", "Pyro"));
        assert_eq!(location.hangar_class, "Hangar_SmallFront_RestStop_Pyro");
        assert_eq!((location.hangar_class_crc, location.socpak_hash), (1_011_559_220, 1_200_814_982));
        assert_eq!(location.spawn.position, [5.900000095367432, -85.13999938964844, 4.510000228881836]);
        assert_eq!(location.spawn.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(location.hangar.position, [379_849.562_5, -718_724.312_5, 226_043.640_625]);
    }

    #[test]
    fn every_row_parses_and_names_a_bound_system() {
        let rows = TABLE.lines().count();
        assert_eq!(TABLE.lines().filter_map(parse).count(), rows);
        assert!(rows >= 8);
        assert!(TABLE.lines().filter_map(parse).all(|location| HomeHangar::new(location.location_id, 0, true).is_some()));
    }

    #[test]
    fn an_unknown_location_has_no_hangar() {
        assert_eq!(home_location(1), None);
    }

    #[test]
    fn the_hangar_bind_carries_its_socpak_and_the_ids_of_its_statics() {
        let bind = HomeHangar::new(ORBITUARY, 1, true).unwrap().bind();
        assert_eq!((bind.entity_id, bind.class_crc, bind.flags), (220_000_065_536, 1_011_559_220, 0x1400));
        assert_eq!((bind.parent_id, bind.parent_attach_id), (200_002_185_940, 0x46c0_1eb2));
        assert_eq!(bind.components, [OC_PHYSICS_CONTROLLER]);
        let [filename, statics, dynamics] = bind.snapshots.as_slice() else { panic!("{} snapshots", bind.snapshots.len()) };
        assert_eq!(filename.value, [0x86, 0xfb, 0x92, 0x47]);
        assert_eq!((statics.key, statics.type_id), (OC_ID_RANGE_KEY, ENTITY_ID_RANGE_TYPE));
        let (first, last) = (220_000_065_537u64, 220_000_065_580u64);
        assert_eq!(statics.value, [first.to_le_bytes(), last.to_le_bytes()].concat(), "the 44 statics, just above the hangar");
        assert_eq!((dynamics.key, dynamics.value.as_slice()), (OC_ID_RANGE_MOV_KEY, [0u8; 16].as_slice()));
    }

    #[test]
    fn entity_counts_come_from_the_socpaks_metadata() {
        assert_eq!(entity_counts(1_200_814_982), (44, 0));
        assert_eq!(entity_counts(2_136_596_536), (181, 0));
        assert_eq!(entity_counts(2_066_649_194), (209, 3));
        assert_eq!(entity_counts(1), (0, 0));
        let rows = ENTITY_COUNTS.lines().count();
        assert!(rows > 20);
        let all = TABLE.lines().filter_map(parse).map(|location| location.socpak_hash);
        let content = CONTENT.lines().filter_map(parse_content).map(|content| content.socpak_hash);
        assert!(all.chain(content).all(|hash| ENTITY_COUNTS.lines().any(|row| row.starts_with(&format!("{hash}\t")))));
    }

    /// A live log has `LoadingPlatformManager_ShipElevator_HangarSmallFront_Rund`, the 65th entity of
    /// `hangar_smlfrnt_002`'s `.soc`, at its hangar's id + 133, and its platform, the 63rd, at + 131.
    #[test]
    fn orbituarys_content_ids_follow_the_hangars_statics() {
        let ids = HomeHangar::new(ORBITUARY, 1, true).unwrap().ids();
        assert_eq!(ids.hangar, ContentIds { statics: (220_000_065_537, 220_000_065_580), dynamics: (0, 0) });
        let entities: Vec<u64> = ids.containers.iter().map(|(entity_id, _)| *entity_id).collect();
        assert_eq!(entities, (220_000_065_581..220_000_065_588).collect::<Vec<_>>());
        let closet = ids.containers[0].1;
        assert_eq!(closet.statics, (220_000_065_588, 220_000_065_608));
        let shell = ids.containers[2].1;
        assert_eq!(shell.statics.1 - shell.statics.0 + 1, 181);
        assert!(ids.containers.windows(2).all(|pair| pair[0].1.statics.1 < pair[1].1.statics.0));
    }

    #[test]
    fn the_planet_is_bound_under_the_system() {
        let planet = HomeHangar::new(ORBITUARY, 1, true).unwrap().planet_bind().unwrap();
        assert_eq!(planet.entity_id, 200_002_185_940);
        assert_eq!((planet.class_crc, planet.flags, planet.parent_id), (2_556_206_422, 0x3400, 200_001_275_310));
        assert_eq!(planet.parent_attach_id, 0x46c0_1eb2);
        assert!(planet.components.is_empty());
        assert_eq!(planet.snapshots[0].value, 1_660_264_064u32.to_le_bytes());
    }

    /// `pyrosystem/entdata/3848707807.entxml` has `Name="pyro3"`, and the name is the client's to read, so the shard
    /// sends only where that file is.
    #[test]
    fn the_planet_names_its_entity_xml() {
        let planet = HomeHangar::new(ORBITUARY, 1, true).unwrap().planet_bind().unwrap();
        let [_, socpak, crc] = planet.snapshots.as_slice() else { panic!("{} snapshots", planet.snapshots.len()) };
        assert_eq!((socpak.key, socpak.type_id), (ENT_SOCPAK_KEY, UINT32_TYPE));
        assert_eq!((crc.key, crc.type_id), (ENT_CRC_KEY, UINT32_TYPE));
        assert_eq!(socpak.value, 3_790_982_489u32.to_le_bytes());
        assert_eq!(crc.value, 3_848_707_807u32.to_le_bytes());
    }

    #[test]
    fn each_home_binds_the_entity_of_its_own_planet() {
        let entity = |location_id| {
            let planet = HomeHangar::new(location_id, 1, true).unwrap().planet_bind().unwrap();
            (planet.entity_id, planet.snapshots[1].value.clone(), planet.snapshots[2].value.clone())
        };
        let (pyro6, pyro3, stanton1) = (entity(2_026_442_305), entity(ORBITUARY), entity(4_005_457_614));
        assert_eq!(pyro6, (200_001_891_234, 3_790_982_489u32.to_le_bytes().to_vec(), 3_484_202_360u32.to_le_bytes().to_vec()));
        assert_eq!(pyro3.1, pyro6.1);
        assert_ne!(pyro3.2, pyro6.2);
        assert_eq!(stanton1, (199_998_000_004, 3_772_184_237u32.to_le_bytes().to_vec(), 1_238_500_217u32.to_le_bytes().to_vec()));
    }

    #[test]
    fn every_planet_has_an_entity_xml() {
        let planets: Vec<Planet> = TABLE.lines().filter_map(parse).filter_map(|location| location.planet).collect();
        assert!(planets.len() >= 6);
        assert!(planets.iter().all(|planet| planet.entity_socpak_hash != 0 && planet.entity_crc != 0));
    }

    #[test]
    fn the_class_crc_is_that_of_orbiting_object_container() {
        let guid = "2d71be5a11f049909bb60d8a1ba4efe3";
        let bytes: Vec<u8> = (0..32).step_by(2).map(|at| u8::from_str_radix(&guid[at..at + 2], 16).unwrap()).collect();
        let reversed = bytes.chunks(8).flat_map(|half| half.iter().rev().copied());
        let crc = !reversed.fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
        });
        assert_eq!(crc, home_location(ORBITUARY).unwrap().planet.unwrap().class_crc);
    }

    /// The star's `rot` of pyrosystem.xml turned on pyro6's `pos`. The planet spins on z under it, so a live camera
    /// agrees on the length of the xy offset and on z, to the eight digits of that `rot`, but not on the direction.
    #[test]
    fn pyro6_is_where_a_live_client_puts_it() {
        let location = home_location(2_026_442_305).unwrap();
        let planet = location.planet.unwrap();
        assert_eq!(planet.name, "pyro6");
        let [x, y, z] = planet.transform.position;
        let (camera_in_system, camera_in_planet) = ([-48_117_844_418.3, 48_503_056_439.7, -99_832.6], [-314_523.235, 301_932.203, -99_832.591]);
        let offset = [camera_in_system[0] - x, camera_in_system[1] - y, camera_in_system[2] - z];
        assert!((offset[0].hypot(offset[1]) - f64::hypot(camera_in_planet[0], camera_in_planet[1])).abs() < 700.0);
        assert!((offset[2] - camera_in_planet[2]).abs() < 0.1);
        assert_eq!(location.hangar.position, [-315_346.452214, 302_409.057066, -100_477.857041]);
    }

    #[test]
    fn a_lagrange_station_has_no_planet_and_stays_under_its_system() {
        let hangar = HomeHangar::new(810_966_700, 1, true).unwrap();
        assert_eq!(hangar.location.planet, None);
        assert!(hangar.planet_bind().is_none());
        let bind = hangar.bind();
        assert_eq!(bind.parent_id, 200_001_275_310);
        assert_eq!(bind.transform, hangar.location.hangar);
    }

    #[test]
    fn with_the_planet_zone_off_the_hangar_has_no_planet_and_no_place() {
        let hangar = HomeHangar::new(ORBITUARY, 1, false).unwrap();
        assert!(hangar.planet_bind().is_none());
        let bind = hangar.bind();
        assert_eq!(bind.transform, Transform::IDENTITY);
        assert_eq!(bind.parent_id, 200_001_275_310);
    }

    #[test]
    fn the_default_home_is_the_residence_and_a_session_may_name_another() {
        assert_eq!(player_home(1_000_001, None, true).unwrap().location.location_id, ORBITUARY);
        assert_eq!(player_home(1_000_001, Some(2_026_442_305), true).unwrap().location.name, "RR_P6_LEO");
        assert_eq!(player_home(1_000_001, Some(1), true), None);
        assert_eq!(player_home(5, None, true), None);
    }

    #[test]
    fn a_place_is_governed_by_its_planet_or_else_its_system() {
        let governed = governed_place(None);
        assert_eq!((governed.system.as_str(), governed.planet.as_deref()), ("Pyro", Some("pyro3")));
        assert_eq!(governed.root(), "pyro3");
        let station = governed_place(Some(810_966_700));
        assert_eq!((station.planet.clone(), station.root()), (None, format!("{}system", station.system.to_ascii_lowercase())));
    }

    #[test]
    fn a_location_with_no_hangar_is_governed_by_where_its_player_goes() {
        assert_eq!(governed_place(Some(1)), Governed { system: "Pyro".to_string(), planet: None });
        assert_eq!(fallback_placement().unwrap().parent_id, 200_001_275_310);
    }

    #[test]
    fn each_planet_is_listed_once() {
        let names: Vec<String> = planets().into_iter().map(|planet| planet.name).collect();
        assert_eq!(names, ["pyro3", "pyro6", "stanton1", "stanton2", "stanton3", "stanton4"]);
    }

    #[test]
    fn the_player_stands_in_its_own_hangar() {
        let hangar = HomeHangar::new(ORBITUARY, 1, true).unwrap();
        let placement = hangar.placement();
        assert_eq!((placement.parent_id, placement.attach_id), (220_000_065_536, 0x46c0_1eb2));
        assert_eq!(placement.transform, hangar.location.spawn);
    }

    #[test]
    fn every_content_row_parses_and_each_hangar_has_some() {
        assert_eq!(CONTENT.lines().filter_map(parse_content).count(), CONTENT.lines().count());
        assert!(TABLE.lines().filter_map(parse).all(|location| !hangar_content(location.socpak_hash).is_empty()));
    }

    /// `hangar_smlfrnt_002`, a `<Child>` of the socpak at 48, -1, -2, holds `hd_hangar_floor_a.cgf` and the walls.
    #[test]
    fn orbituarys_hangar_binds_its_shell_after_its_spawn_closets() {
        let hangar = HomeHangar::new(ORBITUARY, 1, true).unwrap();
        let mut binds = hangar.content_binds();
        assert_eq!(binds.len(), 11, "7 containers, then the 4 entities of the elevator's gateway prefab");
        let gateway = binds.split_off(7);
        assert!(gateway.iter().all(|bind| bind.entity_id > 220_000_065_588));
        let ids: Vec<u64> = binds.iter().map(|bind| bind.entity_id).collect();
        assert_eq!(ids, (220_000_065_581..220_000_065_588).collect::<Vec<_>>());
        assert!(binds.iter().all(|bind| (bind.parent_id, bind.parent_attach_id, bind.flags) == (220_000_065_536, 0x46c0_1eb2, 0x1000)));
        let shell = &binds[2];
        assert_eq!(shell.class_crc, 1_281_446_769);
        assert_eq!(shell.transform, Transform { position: [48.0, -1.0, -2.0], rotation: [0.0, 0.0, 0.0, 1.0] });
        let [filename, socpak, crc, statics, dynamics, has_metadata] = shell.snapshots.as_slice() else { panic!("{} snapshots", shell.snapshots.len()) };
        assert_eq!((has_metadata.key, has_metadata.type_id, has_metadata.value.as_slice()), (SOC_HAS_METADATA_KEY, BOOL_TYPE, [1u8].as_slice()));
        assert_eq!(filename.value, 2_136_596_536u32.to_le_bytes());
        assert_eq!((socpak.key, socpak.value.as_slice()), (ENT_SOCPAK_KEY, 1_200_814_982u32.to_le_bytes().as_slice()));
        assert_eq!((crc.key, crc.value.as_slice()), (ENT_CRC_KEY, 963_357_624u32.to_le_bytes().as_slice()));
        assert_eq!((statics.key, dynamics.key), (OC_ID_RANGE_KEY, OC_ID_RANGE_MOV_KEY));
        let keys: Vec<u32> = binds[3].snapshots.iter().map(|snapshot| snapshot.key).collect();
        assert_eq!(keys, [OC_FILENAME_KEY, OC_ID_RANGE_KEY, OC_ID_RANGE_MOV_KEY, SOC_HAS_METADATA_KEY], "no entity XML: the socpak, the ranges and its tile port");
    }

    /// Orison's small front hangar has four elevator gateways, `playerhangar_elev_hangar_ht_gateway-012` to `-015`, 4
    /// statics each from H+235: limbo node, gateway, door and panel, all bound by the DGS, and the gateways get no
    /// ranges.
    #[test]
    fn orisons_call_panels_are_bound_by_the_server_at_the_ids_the_client_would_give_them() {
        const ORISON: u32 = 1_105_811_199;
        let hangar = HomeHangar::new(ORISON, 1, true).unwrap();
        let h = hangar.entity_id;
        let binds = hangar.content_binds();
        let panels: Vec<&Bind> = binds.iter().filter(|bind| bind.class_crc == 3_016_690_202).collect();
        let ids: Vec<u64> = panels.iter().map(|bind| bind.entity_id - h).collect();
        assert_eq!(ids, [238, 242, 246, 250]);
        let listed: Vec<(u64, String)> = hangar.call_panels().into_iter().map(|panel| (panel.entity_id - h, panel.gateway)).collect();
        assert_eq!(listed[2], (246, "playerhangar_elev_hangar_ht_gateway-014".to_string()));
        assert_eq!(listed.len(), 4);
        let bound: Vec<u64> = binds.iter().map(|bind| bind.entity_id - h).filter(|id| *id >= 235).collect();
        assert_eq!(bound, (235..=250).collect::<Vec<_>>());
        assert!(panels.iter().all(|bind| (bind.parent_id, bind.parent_attach_id, bind.flags) == (h, 0x46c0_1eb2, 0x1000)));
        // The panel and the door take their `.soc`'s models as overrides; the limbo node and the gateway have none.
        let panel_geometry = crate::universe::string_value("Objects/buildingsets/human/hightech/prop/interactive/console/console_elevator_1_panel_a.cgf");
        assert!(panels.iter().all(|bind| matches!(bind.snapshots.as_slice(), [geometry] if (geometry.key, geometry.type_id, &geometry.value) == (GEOMETRY_OVERRIDE_KEY, crate::universe::STRING_TYPE, &panel_geometry))));
        let door = binds.iter().find(|bind| bind.entity_id == h + 245).unwrap();
        let keys: Vec<u32> = door.snapshots.iter().map(|snapshot| snapshot.key).collect();
        assert_eq!(keys, [GEOMETRY_OVERRIDE_KEY, MATERIAL_OVERRIDE_KEY]);
        assert!(binds.iter().filter(|bind| [h + 243, h + 244].contains(&bind.entity_id)).all(|bind| bind.snapshots.is_empty()));
        let gateways = binds.iter().filter(|bind| bind.snapshots.first().is_some_and(|snapshot| snapshot.value == 2_432_330_252u32.to_le_bytes()));
        assert!(gateways.clone().count() == 4 && gateways.into_iter().all(|bind| bind.snapshots.len() == 1), "the gateways get no id ranges");
        let gateway = hangar_content(hangar.location.socpak_hash).into_iter().find(|content| content.name.ends_with("gateway-014")).unwrap();
        assert_eq!(panels[2].transform, gateway.transform.then(&crate::transit::tables().prefab_entities(2_432_330_252)[3].pose));
    }

    /// The fire extinguisher cabinets of `hangar_smlfrnt_002` (4) and `cargo_elevator_sml_001_rund_a` (1) attach to
    /// tile ports of their container, which need an item port container and a tile on its entity.
    #[test]
    fn containers_with_tile_ports_list_their_components() {
        assert_eq!(counts_row(2_136_596_536), Some([181, 0, 0, 4]));
        assert_eq!(counts_row(1_594_620_259), Some([13, 0, 0, 1]));
        let hangar = HomeHangar::new(ORBITUARY, 1, true).unwrap();
        let components: Vec<Vec<u32>> = hangar.content_binds().into_iter().map(|bind| bind.components).collect();
        let tiles = vec![ITEM_PORT_CONTAINER, TILE];
        assert_eq!(components[..7], [vec![], vec![], tiles.clone(), tiles, vec![], vec![], vec![]]);
        let flagged: Vec<bool> = hangar.content_binds().iter().map(|bind| bind.snapshots.iter().any(|snapshot| snapshot.key == SOC_HAS_METADATA_KEY)).collect();
        assert_eq!(flagged[..7], [false, false, true, true, false, false, false], "socHasMetadata, which runs the TileXmlEntry handler");
        assert_eq!(ENTITY_COUNTS.lines().filter_map(|row| counts_row(row.split('\t').next()?.parse().ok()?)).count(), ENTITY_COUNTS.lines().count());
    }

    /// 32-bit FNV-1a of the upper-cased name with its final zero (registry slot +0x58, 0x146762510).
    #[test]
    fn component_hashes_are_fnv_of_the_upper_cased_name() {
        let fnv = |name: &str| {
            name.to_ascii_uppercase().bytes().chain([0]).fold(0x811c_9dc5u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
        };
        assert_eq!(fnv("ObjectContainerPhysicsController"), OC_PHYSICS_CONTROLLER);
        assert_eq!(fnv("ItemPortContainer"), ITEM_PORT_CONTAINER);
        assert_eq!(fnv("Tile"), TILE);
    }

    #[test]
    fn a_hangars_content_stays_inside_its_id_stride() {
        for location in TABLE.lines().filter_map(parse) {
            let hangar = HomeHangar::new(location.location_id, 3, true).unwrap();
            assert!(hangar.ids().end <= hangar_id(4), "{} outgrows its stride", location.name);
        }
    }

    #[test]
    fn hangar_ids_stay_between_the_tree_and_the_players() {
        assert!(hangar_id(0) > 210_000_000_001);
        assert!(hangar_id(1_000_000) < 300_000_000_000);
    }
}
