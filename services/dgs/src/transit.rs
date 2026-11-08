//! The hangar elevators of the home locations, as the game data describes them: which system serves each hangar, the
//! Transit managers (their parameters and their stops) and the gateways (a stop's door, with its call panel and limbo
//! node). See docs/DGS_TRANSIT.md for what the DGS does with them: `sim` runs the rides, not yet wired to the network.
//!
//! The tables come from Data.p4k through `tools/gamedata-build` (its `transit.rs` says how each column is read) and reach
//! the DGS through `crate::gamedata` at startup, which `install`s them as `crate::location`'s. docs/DATA.md lists the
//! columns. Tests read the copies in `testdata/`.
//!
//! A manager is known by the socpak that holds it and its entity's GUID together, as a station copied into several
//! places keeps its GUIDs. A gateway belongs to an owner, known the same way: a manager, or a hangar socpak with the
//! GUID of its `TransitDynamicDestination` (Transit) or of its access point's container (Transport). Every transform is
//! in the frame of the owner's socpak.
//!
//! What is not certain is said where it is held: `Resolution::Link` marks a manager found by its link to the location's
//! instanced interior manager while no tag rule confirms it, as at the Pyro rest stops (Orbituary's hangar elevator).

use std::{collections::HashMap, sync::OnceLock};

use crate::entity::Transform;

pub mod elevators;
pub mod sim;
pub mod wire;

/// The paths of the tables in a game build, as `tools/gamedata-build` writes them and `sc-gamedata` serves them.
/// The Transit managers that serve a hangar, one per row.
pub const MANAGERS_PATH: &str = "tables/transit_managers.tsv";
/// The gateways of those managers and of every hangar socpak of the home locations.
pub const GATEWAYS_PATH: &str = "tables/transit_gateways.tsv";
/// Per home location and hangar class, the system and the manager that serve the hangar.
pub const HANGARS_PATH: &str = "tables/transit_hangars.tsv";
/// The entities of each prefab inside a hangar that holds a gateway: its call panel, door, gateway and limbo node.
pub const PREFAB_ENTITIES_PATH: &str = "tables/transit_prefab_entities.tsv";

/// A manager or the owner of a hangar's gateways: the socpak that holds it, by hash, and its entity's GUID, in lower
/// case.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner {
    pub socpak_hash: u32,
    pub guid: String,
}

/// An entity of a stop: its GUID, in lower case, and its transform.
#[derive(Clone, Debug, PartialEq)]
pub struct Peripheral {
    pub guid: String,
    pub transform: Transform,
}

/// A stop's door: where a carriage arrives.
#[derive(Clone, Debug, PartialEq)]
pub struct Gateway {
    /// Its place among its owner's gateways. A manager's counts those of every destination in order, as its
    /// `gatewayIndex` does.
    pub index: usize,
    /// The manager's destination it belongs to, 0 for a hangar's.
    pub destination: usize,
    /// The destination's `nameIdentifier`, a localization key, empty for a hangar's.
    pub name: String,
    /// The main stop, such as the spaceport's lobby.
    pub important: bool,
    pub enabled: bool,
    /// How many carriages of the fleet start here.
    pub carriages: usize,
    /// The socpaks from the owner's down to the one that holds the gateway, by hash, and their `<Child>` GUIDs.
    pub containers: Vec<u32>,
    pub container_guids: Vec<String>,
    pub guid: String,
    pub transform: Transform,
    /// The `SCItemDoor` that opens for the carriage.
    pub door: Option<Peripheral>,
    /// The exterior call panel.
    pub panel: Option<Peripheral>,
    /// Where a carriage waits between stops, below the door. A Transport access point has none.
    pub limbo: Option<Peripheral>,
}

/// A Transit manager and the parameters of its rides, `TransitManagerParams` with the entity's values over the class's.
#[derive(Clone, Debug, PartialEq)]
pub struct Manager {
    pub owner: Owner,
    pub name: String,
    pub transform: Transform,
    pub carriage_class: String,
    pub carriage_class_crc: u32,
    /// The cab's interior, `carriageInterior`.
    pub cab_socpak_hash: u32,
    /// The fleet: one carriage per `SpawnLocation` of its data.
    pub carriages: usize,
    /// Seconds a carriage stays at a stop with its doors open, `carriageWaitTime`.
    pub wait_time: f64,
    /// Seconds allowed for the doors, `carriageDoorTimeout`.
    pub door_timeout: f64,
    /// `idealTimeBetweenArrivals`, seconds.
    pub ideal_time_between_arrivals: f64,
    /// `pauseDistance`, metres.
    pub pause_distance: f64,
    /// `carriageAcceleration`, metres per second squared.
    pub acceleration: f64,
    /// `doorFindDistance`, metres.
    pub door_find_distance: f64,
    pub allow_one_destination: bool,
    pub sort_destinations_by_name: bool,
    pub sort_destinations_by_priority: bool,
    pub force_streamable_transit: bool,
    pub doors_open_by_default: bool,
    /// Its stops, in order of `Gateway::index`.
    pub gateways: Vec<Gateway>,
}

/// Which elevator system a hangar's stop belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    /// `SCTransit*`: carriages the server creates when needed.
    Transit,
    /// `SCTransport*` (Area18): a fixed fleet that is the manager's state.
    Transport,
}

/// How the manager of a hangar was found, as no data names it (see `tools/gamedata-build`'s `transit.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Linked to the location's instanced interior manager, and its tag filter accepts the hangar's gateways.
    LinkAndTags,
    /// Linked to the location's instanced interior manager, while its tag filter does not accept the hangar's
    /// gateways: likely, not proven.
    Link,
    /// Only its tag filter accepts the hangar's gateways.
    Tags,
    /// The location's Transport manager whose permissions come from its instanced interior manager.
    IimInterface,
}

impl Resolution {
    /// Whether two independent rules agree, or the data says it outright.
    pub fn certain(self) -> bool {
        matches!(self, Self::LinkAndTags | Self::IimInterface)
    }
}

/// A hangar class of a home location and its elevator.
#[derive(Clone, Debug, PartialEq)]
pub struct Hangar {
    pub location_id: u32,
    pub class: String,
    pub class_crc: u32,
    pub socpak_hash: u32,
    pub system: System,
    /// The manager that serves it. For Transit it is in `Tables::manager`; a Transport one is known by name only.
    pub manager: Owner,
    pub manager_name: String,
    pub resolution: Resolution,
    /// The owner of its gateways: its `TransitDynamicDestination`, or for Transport its access point's container.
    pub stop: Owner,
    pub stop_name: String,
    /// The dynamic destination's `radius`, metres; 0 where its entity has none.
    pub radius: f64,
    /// Where `stop` is, in the hangar's frame.
    pub stop_transform: Transform,
    /// Whether the hangar's data lists the dynamic destination's gateways. Otherwise they are every Transit gateway of
    /// the hangar, as the game gathers them at run time.
    pub baked: bool,
}

/// An entity of a gateway prefab's `.soc`. The client would make it from the prefab's `ocIdRange`, local and with
/// authority over it, so a press on its call panel would never reach the server; the DGS binds these itself.
#[derive(Clone, Debug, PartialEq)]
pub struct PrefabEntity {
    /// Its place among the `.soc`'s entities, in document order: the client gives it the range's first id plus this.
    pub index: u64,
    /// CRC-32C of its `EntityClassGUID`, as every class CRC of the tables.
    pub class_crc: u32,
    pub name: String,
    /// The instance's own geometry and material, empty where it takes its class's.
    pub geometry: String,
    pub material: String,
    /// In the prefab's frame.
    pub pose: Transform,
}

/// The four tables of the shard's game build, parsed and checked against each other.
#[derive(Debug, Default)]
pub struct Tables {
    managers: Vec<Manager>,
    hangars: Vec<Hangar>,
    /// The gateways of each hangar socpak's stop.
    stops: HashMap<Owner, Vec<Gateway>>,
    /// By the prefab's socpak hash, in index order.
    prefabs: HashMap<u32, Vec<PrefabEntity>>,
}

fn transform(numbers: &[&str]) -> Option<Transform> {
    let numbers: Vec<f64> = numbers.iter().map(|number| number.parse().ok()).collect::<Option<_>>()?;
    let [x, y, z, qx, qy, qz, qw] = numbers.as_slice() else { return None };
    Some(Transform { position: [*x, *y, *z], rotation: [*qx, *qy, *qz, *qw] })
}

fn flag(text: &str) -> Option<bool> {
    match text {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn owner(hash: &str, guid: &str) -> Option<Owner> {
    Some(Owner { socpak_hash: hash.parse().ok()?, guid: guid.to_string() })
}

/// A GUID and its transform, `None` for an empty GUID: a peripheral the gateway does not have.
fn peripheral(fields: &[&str]) -> Option<Option<Peripheral>> {
    let [guid, numbers @ ..] = fields else { return None };
    let transform = transform(numbers)?;
    Some((!guid.is_empty()).then(|| Peripheral { guid: guid.to_string(), transform }))
}

fn list<T: std::str::FromStr>(text: &str) -> Option<Vec<T>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(',').map(|item| item.parse().ok()).collect()
}

fn parse_manager(row: &str) -> Option<Manager> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [hash, guid, name, rest @ ..] = fields.as_slice() else { return None };
    let [x, y, z, qx, qy, qz, qw, class, class_crc, cab, carriages, numbers @ ..] = rest else { return None };
    let [wait, door, ideal, pause, acceleration, find, one, by_name, by_priority, streamable, open] = numbers else { return None };
    let number = |text: &str| text.parse::<f64>().ok();
    Some(Manager {
        owner: owner(hash, guid)?,
        name: name.to_string(),
        transform: transform(&[x, y, z, qx, qy, qz, qw])?,
        carriage_class: class.to_string(),
        carriage_class_crc: class_crc.parse().ok()?,
        cab_socpak_hash: cab.parse().ok()?,
        carriages: carriages.parse().ok()?,
        wait_time: number(wait)?,
        door_timeout: number(door)?,
        ideal_time_between_arrivals: number(ideal)?,
        pause_distance: number(pause)?,
        acceleration: number(acceleration)?,
        door_find_distance: number(find)?,
        allow_one_destination: flag(one)?,
        sort_destinations_by_name: flag(by_name)?,
        sort_destinations_by_priority: flag(by_priority)?,
        force_streamable_transit: flag(streamable)?,
        doors_open_by_default: flag(open)?,
        gateways: Vec::new(),
    })
}

/// A gateway and its owner.
fn parse_gateway(row: &str) -> Option<(Owner, Gateway)> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [hash, owner_guid, index, destination, name, important, enabled, carriages, containers, guids, guid, rest @ ..] = fields.as_slice() else {
        return None;
    };
    // The gateway's transform, then the door, the panel and the limbo node: each a GUID and a transform.
    if rest.len() != 7 + 3 * 8 {
        return None;
    }
    let gateway = transform(&rest[..7])?;
    let containers: Vec<u32> = list(containers)?;
    let container_guids: Vec<String> = list(guids)?;
    if containers.len() != container_guids.len() {
        return None;
    }
    Some((
        owner(hash, owner_guid)?,
        Gateway {
            index: index.parse().ok()?,
            destination: destination.parse().ok()?,
            name: name.to_string(),
            important: flag(important)?,
            enabled: flag(enabled)?,
            carriages: carriages.parse().ok()?,
            containers,
            container_guids,
            guid: guid.to_string(),
            transform: gateway,
            door: peripheral(&rest[7..15])?,
            panel: peripheral(&rest[15..23])?,
            limbo: peripheral(&rest[23..31])?,
        },
    ))
}

fn parse_hangar(row: &str) -> Option<Hangar> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [location, class, class_crc, hash, system, manager_hash, manager_guid, manager_name, resolution, stop_guid, stop_name, radius, rest @ ..] = fields.as_slice() else {
        return None;
    };
    let [x, y, z, qx, qy, qz, qw, source] = rest else { return None };
    let socpak_hash = hash.parse().ok()?;
    Some(Hangar {
        location_id: location.parse().ok()?,
        class: class.to_string(),
        class_crc: class_crc.parse().ok()?,
        socpak_hash,
        system: match *system {
            "transit" => System::Transit,
            "transport" => System::Transport,
            _ => return None,
        },
        manager: owner(manager_hash, manager_guid)?,
        manager_name: manager_name.to_string(),
        resolution: match *resolution {
            "link+tags" => Resolution::LinkAndTags,
            "link" => Resolution::Link,
            "tags" => Resolution::Tags,
            "iim-interface" => Resolution::IimInterface,
            _ => return None,
        },
        stop: Owner { socpak_hash, guid: stop_guid.to_string() },
        stop_name: stop_name.to_string(),
        radius: radius.parse().ok()?,
        stop_transform: transform(&[x, y, z, qx, qy, qz, qw])?,
        baked: match *source {
            "baked" => true,
            "gathered" => false,
            _ => return None,
        },
    })
}

fn parse_prefab_entity(row: &str) -> Option<(u32, PrefabEntity)> {
    let fields: Vec<&str> = row.split('\t').collect();
    let [socpak_hash, index, class_crc, name, geometry, material, numbers @ ..] = fields.as_slice() else { return None };
    let entity = PrefabEntity {
        index: index.parse().ok()?,
        class_crc: class_crc.parse().ok()?,
        name: name.to_string(),
        geometry: geometry.to_string(),
        material: material.to_string(),
        pose: transform(numbers)?,
    };
    Some((socpak_hash.parse().ok()?, entity))
}

/// Every non-empty line of `text`, parsed; a line that does not parse fails the table, as it means a broken build.
fn rows<T>(name: &str, text: &str, parse: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    let lines = text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty());
    lines.map(|(index, line)| parse(line).ok_or_else(|| format!("{name}: line {} does not parse", index + 1))).collect()
}

impl Tables {
    /// The tables from their text, as `tools/gamedata-build` writes them. Every gateway must have its owner, every
    /// Transit hangar its manager, and every owner its gateways numbered from 0 without a gap, as every prefab its
    /// entities.
    pub fn parse(managers: &str, gateways: &str, hangars: &str, prefab_entities: &str) -> Result<Self, String> {
        let mut managers = rows(MANAGERS_PATH, managers, parse_manager)?;
        let hangars = rows(HANGARS_PATH, hangars, parse_hangar)?;
        if hangars.is_empty() {
            return Err(format!("{HANGARS_PATH}: no hangars"));
        }
        let mut owned: HashMap<Owner, Vec<Gateway>> = HashMap::new();
        for (owner, gateway) in rows(GATEWAYS_PATH, gateways, parse_gateway)? {
            owned.entry(owner).or_default().push(gateway);
        }
        for (owner, gateways) in &mut owned {
            gateways.sort_by_key(|gateway| gateway.index);
            if gateways.iter().enumerate().any(|(index, gateway)| gateway.index != index) {
                return Err(format!("{GATEWAYS_PATH}: the gateways of {owner:?} are not numbered from 0 without a gap"));
            }
        }
        for manager in &mut managers {
            manager.gateways = owned.remove(&manager.owner).unwrap_or_default();
            if manager.gateways.is_empty() {
                return Err(format!("{MANAGERS_PATH}: {} has no gateways", manager.name));
            }
        }
        for hangar in &hangars {
            if hangar.system == System::Transit && !managers.iter().any(|manager| manager.owner == hangar.manager) {
                return Err(format!("{HANGARS_PATH}: {} at {} names a manager that is not in {MANAGERS_PATH}", hangar.class, hangar.location_id));
            }
            if !owned.contains_key(&hangar.stop) {
                return Err(format!("{HANGARS_PATH}: {} at {} has no gateways in {GATEWAYS_PATH}", hangar.class, hangar.location_id));
            }
        }
        if let Some(owner) = owned.keys().find(|owner| !hangars.iter().any(|hangar| &hangar.stop == *owner)) {
            return Err(format!("{GATEWAYS_PATH}: gateways of {owner:?}, which is no manager and no hangar"));
        }
        let mut prefabs: HashMap<u32, Vec<PrefabEntity>> = HashMap::new();
        for (hash, entity) in rows(PREFAB_ENTITIES_PATH, prefab_entities, parse_prefab_entity)? {
            prefabs.entry(hash).or_default().push(entity);
        }
        for (hash, entities) in &mut prefabs {
            entities.sort_by_key(|entity| entity.index);
            if entities.iter().enumerate().any(|(index, entity)| entity.index != index as u64) {
                return Err(format!("{PREFAB_ENTITIES_PATH}: the entities of {hash} are not numbered from 0 without a gap"));
            }
        }
        Ok(Self { managers, hangars, stops: owned, prefabs })
    }

    pub fn managers(&self) -> &[Manager] {
        &self.managers
    }

    pub fn hangars(&self) -> &[Hangar] {
        &self.hangars
    }

    pub fn manager(&self, owner: &Owner) -> Option<&Manager> {
        self.managers.iter().find(|manager| &manager.owner == owner)
    }

    /// The hangar class of a home location, by the class's CRC.
    pub fn hangar(&self, location_id: u32, class_crc: u32) -> Option<&Hangar> {
        self.hangars.iter().find(|hangar| hangar.location_id == location_id && hangar.class_crc == class_crc)
    }

    /// The gateways of a hangar's stop, in its socpak's frame.
    pub fn stop_gateways(&self, hangar: &Hangar) -> &[Gateway] {
        self.stops.get(&hangar.stop).map_or(&[], Vec::as_slice)
    }

    /// The entities of a gateway prefab, by its socpak's hash, in index order; empty for any other socpak.
    pub fn prefab_entities(&self, socpak_hash: u32) -> &[PrefabEntity] {
        self.prefabs.get(&socpak_hash).map_or(&[], Vec::as_slice)
    }

    /// The Transit manager that serves a hangar class of a home location: `None` for a Transport hangar.
    pub fn serving(&self, location_id: u32, class_crc: u32) -> Option<&Manager> {
        let hangar = self.hangar(location_id, class_crc).filter(|hangar| hangar.system == System::Transit)?;
        self.manager(&hangar.manager)
    }

    /// The copies in `testdata/`, generated from the game by `scripts/game-files.sh`, for tests.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn fixtures() -> Self {
        use crate::testdata::text;
        Self::parse(text(MANAGERS_PATH), text(GATEWAYS_PATH), text(HANGARS_PATH), text(PREFAB_ENTITIES_PATH)).expect("the fixture transit tables parse")
    }
}

static TABLES: OnceLock<Tables> = OnceLock::new();

/// Sets the tables for the life of the process, once, at startup. `false` when they were already set.
pub fn install(tables: Tables) -> bool {
    TABLES.set(tables).is_ok()
}

/// The installed tables. Tests get the fixtures.
#[cfg(any(test, feature = "fixtures"))]
pub fn tables() -> &'static Tables {
    TABLES.get_or_init(Tables::fixtures)
}

/// The installed tables.
#[cfg(not(any(test, feature = "fixtures")))]
pub fn tables() -> &'static Tables {
    TABLES.get().expect("the transit tables are installed at startup, before anything asks for an elevator")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ORBITUARY;

    /// `Hangar_SmallFront_RestStop_Pyro`, the class CRC of `home_locations.tsv`.
    const REST_STOP_SMALL: u32 = 1_011_559_220;
    /// `StarMapObject.Stanton2_Orison` and `Hangar_SmallFront_Orison`.
    const ORISON: u32 = 1_105_811_199;
    const ORISON_SMALL: u32 = 4_155_985_664;
    /// `StarMapObject.Stanton3_Area18` and `Hangar_SmallFront_Area18`.
    const AREA18: u32 = 2_273_540_638;
    const AREA18_SMALL: u32 = 3_882_666_864;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-3)
    }

    #[test]
    fn every_hangar_size_of_every_home_location_has_its_elevator() {
        let tables = tables();
        let locations = crate::location::Tables::fixtures();
        assert_eq!(tables.hangars().len(), 4 * locations.locations(), "small, medium, large and XL of each location");
        for hangar in tables.hangars() {
            assert!(!tables.stop_gateways(hangar).is_empty(), "{} at {}", hangar.class, hangar.location_id);
            match hangar.system {
                System::Transit => assert!(tables.manager(&hangar.manager).is_some()),
                System::Transport => assert_eq!(hangar.resolution, Resolution::IimInterface),
            }
        }
        // Every manager serves some hangar, and is written once.
        assert_eq!(tables.managers().len(), 7);
        for manager in tables.managers() {
            assert!(tables.hangars().iter().any(|hangar| hangar.manager == manager.owner), "{}", manager.name);
        }
    }

    #[test]
    fn orisons_hangar_elevator_is_the_spaceport_line() {
        let tables = tables();
        let manager = tables.serving(ORISON, ORISON_SMALL).expect("Orison's small hangar has a manager");
        assert_eq!(manager.name, "TransitManager_Spaceport_to_Hangars");
        assert_eq!(manager.owner.guid, "4d421e24-d10c-6957-9c73-0d276e61d6b4");
        assert_eq!((manager.carriages, manager.carriage_class.as_str()), (8, "TransitCarriage_Elev_Util_SecurityClearance"));
        assert_eq!((manager.wait_time, manager.door_timeout, manager.ideal_time_between_arrivals), (5.0, 2.5, 60.0));
        assert_eq!((manager.acceleration, manager.pause_distance), (40.0, 100.0));
        assert!(manager.allow_one_destination && manager.sort_destinations_by_name && !manager.sort_destinations_by_priority);
        // 22 gateways: 2 of the disabled Javelin tour, 10 of the lobby, 2 for each of 5 docking ports.
        assert_eq!(manager.gateways.len(), 22);
        assert_eq!(manager.gateways.iter().map(|gateway| gateway.carriages).sum::<usize>(), manager.carriages);
        assert!(!manager.gateways[0].enabled);
        let lobby: Vec<&Gateway> = manager.gateways.iter().filter(|gateway| gateway.important).collect();
        assert_eq!(lobby.len(), 10);
        assert!(lobby.iter().all(|gateway| gateway.name == "@ui_interactor_ST2_spaceport" && gateway.containers == [1_589_736_448, 1_079_967_928]));
        // Gateway 2, in the frame of `orison_sp_ext`: the lobby prefab `-006` at (-16.125, 47, -0.75), its gateway 2 m in.
        let second = &manager.gateways[2];
        assert!(close(second.transform.position, [-16.125, 45.0, -0.75]), "{:?}", second.transform.position);
        assert_eq!(second.door.as_ref().unwrap().transform.position, second.transform.position);
        let limbo = second.limbo.as_ref().unwrap();
        assert!((second.transform.position[2] - limbo.transform.position[2] - 6.0).abs() < 1e-3, "the limbo node is 6 m below");
        assert_eq!(second.panel.as_ref().unwrap().guid, "48579f5c-ed1f-2d67-30bb-5d38dc7f62a6");
        // The hangar's side: the four HT gateways of the base hangar, gathered as no list is baked.
        let hangar = tables.hangar(ORISON, ORISON_SMALL).unwrap();
        assert_eq!((hangar.system, hangar.resolution, hangar.baked), (System::Transit, Resolution::LinkAndTags, false));
        assert_eq!(hangar.stop.guid, "48aa5ff6-7f27-4829-53e7-0e0544acd8aa");
        let gateways = tables.stop_gateways(hangar);
        assert_eq!(gateways.len(), 4);
        assert!(gateways.iter().all(|gateway| gateway.guid == "47b317ea-b725-fd0c-e65c-5fb4340dffb2" && gateway.containers == [2_066_649_194, 2_432_330_252]));
        assert!(close(gateways[0].transform.position, [-21.0, -88.0, 0.0]), "{:?}", gateways[0].transform.position);
    }

    #[test]
    fn orbituarys_manager_is_the_linked_candidate_and_says_so() {
        let tables = tables();
        let hangar = tables.hangar(ORBITUARY, REST_STOP_SMALL).expect("Orbituary's small hangar");
        assert_eq!((hangar.socpak_hash, hangar.system), (crate::physics::world::REST_STOP, System::Transit));
        assert_eq!(hangar.resolution, Resolution::Link);
        assert!(!hangar.resolution.certain(), "the tag rule does not confirm it");
        let manager = tables.manager(&hangar.manager).unwrap();
        assert_eq!((manager.name.as_str(), manager.owner.socpak_hash), ("TransitManager_Hangar-to-Lobby", 3_264_072_028));
        assert_eq!((manager.carriages, manager.acceleration, manager.pause_distance), (9, 250.0, 50.0));
        // The rest stop hangar's baked destination: one gateway, its door at the gateway and its limbo node 6 m below.
        assert!(hangar.baked);
        assert_eq!(hangar.radius, 16.0);
        let [gateway] = tables.stop_gateways(hangar) else { panic!("one gateway") };
        assert!(close(gateway.transform.position, [20.0, -88.0, 4.25]));
        assert!(close(gateway.limbo.as_ref().unwrap().transform.position, [19.75, -88.0, -1.75]));
        assert_eq!(gateway.door.as_ref().unwrap().guid, "417b04e1-9581-f527-43b1-fa9462e84885");
        // The other Pyro rest stops share the hangar socpak but have their own station's manager.
        let others: Vec<&Hangar> = tables.hangars().iter().filter(|other| other.socpak_hash == hangar.socpak_hash).collect();
        assert_eq!(others.len(), 3);
        let mut stations: Vec<u32> = others.iter().map(|other| other.manager.socpak_hash).collect();
        stations.sort_unstable();
        stations.dedup();
        assert_eq!(stations.len(), 3);
    }

    #[test]
    fn area18_is_transport_with_an_access_point() {
        let tables = tables();
        let hangar = tables.hangar(AREA18, AREA18_SMALL).unwrap();
        assert_eq!((hangar.system, hangar.manager_name.as_str()), (System::Transport, "TransportManager_Area18_Spaceport_Hangars"));
        assert!(tables.serving(AREA18, AREA18_SMALL).is_none(), "no Transit manager");
        assert_eq!(hangar.stop_name, "transport_accesspoint_generic_hangar_elev_util-007");
        let [gateway] = tables.stop_gateways(hangar) else { panic!("one access point") };
        assert!(close(gateway.transform.position, [20.0, -88.0, 4.25]));
        assert!(gateway.door.is_some() && gateway.panel.is_some() && gateway.limbo.is_none());
    }

    #[test]
    fn broken_tables_are_refused() {
        use crate::testdata::text;
        let (managers, gateways, hangars, prefabs) = (text(MANAGERS_PATH), text(GATEWAYS_PATH), text(HANGARS_PATH), text(PREFAB_ENTITIES_PATH));
        assert!(Tables::parse(managers, gateways, hangars, prefabs).is_ok());
        assert!(Tables::parse(managers, gateways, "1\tbroken", prefabs).unwrap_err().contains("line 1"));
        assert!(Tables::parse(managers, gateways, "", prefabs).unwrap_err().contains("no hangars"));
        // Without the managers, the Transit hangars name nothing.
        assert!(Tables::parse("", gateways, hangars, prefabs).unwrap_err().contains("names a manager"));
        // A gateway missing in the middle of an owner's.
        let gap: Vec<&str> = gateways.lines().filter(|line| line.split('\t').nth(2).is_none_or(|index| index != "1")).collect();
        assert!(Tables::parse(managers, &gap.join("\n"), hangars, prefabs).unwrap_err().contains("without a gap"));
        // A prefab entity missing in the middle of a prefab's.
        let gap: Vec<&str> = prefabs.lines().filter(|line| line.split('\t').nth(1) != Some("1")).collect();
        assert!(Tables::parse(managers, gateways, hangars, &gap.join("\n")).unwrap_err().contains(PREFAB_ENTITIES_PATH));
    }

    /// `playerhangar_elev_hangar_ht_gateway.socpak` (Orison, New Babbage): limbo node, gateway, door, then the call panel.
    #[test]
    fn a_gateway_prefab_lists_its_entities_in_soc_order() {
        let entities = tables().prefab_entities(2_432_330_252);
        let names: Vec<&str> = entities.iter().map(|entity| entity.name.as_str()).collect();
        assert_eq!(names, ["TransitLimboNode_Orison_Elevator_Hangar-HT", "TransitGateway_HT_Elevator_Hangar", "Elevator_Door_HT_Hangar-001", "TransitInteractionUIPanel_Ext_HT_Elevator_Hangar"]);
        assert_eq!(entities[3].class_crc, 3_016_690_202, "TransitInteractionUIPanel_Elevator_Ext");
        assert!(close(entities[3].pose.position, [0.753_401, 0.324_909, 1.425_001]));
        assert!(tables().prefab_entities(1).is_empty());
    }
}
