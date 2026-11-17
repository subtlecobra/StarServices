//! The ASOP kiosk's link to an air traffic controller (ATC), which the kiosk needs before its UI opens.
//!
//! Behaviour of the client (4.10.2, CL 12913470): at the first press the kiosk logs
//! `OnRequestOpen ... [NO ATC] ShipSelectorKiosk_Hangar_HighTech_1_a-032
//! [226553665713] - NO LINK TO ATC - Verify and export OC` and opens nothing. The component keeps the
//! ATC's entity id in its entity variable `OwnerATCId` ; 0 is "no link",
//! and a set id must name an entity with the `SCAirTrafficControllerDataManager` component that is streamed in on the
//! client (else `ATC [..] not streamed in`). The variable is only written by the kiosk's handler of the component event
//! the ATC link event, which an ATC entity sends from its `Register*` functions.
//! The hangar's own object containers hold no ATC, and the kiosks have no entity link to one, so on a client that makes
//! the hangar's statics itself (local entities, flag `0x10`) nothing ever sends the event. A official server spawns an
//! instanced hangar's content as replicated entities and the location's ATC links them.
//!
//! How the link works (4.10.2; see docs/SHIPS.md, "The kiosk's ATC link"):
//!
//! - A official server's ATC (authority) runs `LoadFromXml` (only with authority) at its `PostInitialize`,
//!   resolves the exported super GUIDs of its `ShipInsuranceProviders` and calls `RegisterInsuranceProvider`,
//!   which sends `LinkWithATC` to the kiosk. The kiosk's handler writes `OwnerATCId`, a replicated
//!   variable (flags 5, serializer `Struct EntityId m_value uint64` = 0x728ae36a).
//! - An `EntitySnapshotUpdate` does not reach a client-made static: the client looks the id up in the replication view's
//!   bind table and logs `Snapshot Update For Missing Entity` for a local entity. So the kiosk the client
//!   makes from `ocIdRange` cannot be given the variable, and no variable index is sent.
//! - A replicated kiosk is not an authority on the client, and a bind carries variables by key and type, so it can carry
//!   `OwnerATCId` with no index. A first try bound a replicated copy of each kiosk beside the client's own, and the press
//!   reached the client's own kiosk. So the container `hangar_smlfrnt_001` gets no `ocIdRange` now: the client makes
//!   none of its 209 statics, `location.rs` binds each of them as a replicated entity at the id the client would have
//!   given it (the table `transit_prefab_entities.tsv`, as for the gateway prefabs), and the three kiosks carry
//!   `OwnerATCId`. This module binds the ATC entity they name.
//!
//! Switch: the environment variable `SC_DGS_KIOSK_ATC`, `merge` unless set (the official way: the kiosks are bound over the
//! client's own statics with the flags the official server uses); `1`, `true`, `on`, `yes` and `bind` bind (the DGS then binds the
//! kiosk container's entities itself, which costs that container its lights, doors and other per-instance data), `overlay` and `off` too.

use crate::entity::{Bind, Snapshot, Transform};
use crate::location::{hangar_content, HomeHangar};
use crate::solar_system::ZONE_ATTACHMENT;
use sc_api::internal::HangarSize;
use tracing::info;

/// CRC-32 of the lowercased variable name, as every entity variable is keyed.
pub const OWNER_ATC_ID_KEY: u32 = 0xf9df_8d76;
/// CRC-32 of `locationid`: the kiosk's `LocationId` variable (a u32), which the ship
/// list fetch reads as the location of the player's `Location` and `Hangar` inventories. At 0 the client skips both
/// queries and lists no ship.
pub const LOCATION_ID_KEY: u32 = 0x0353_0ccf;
/// FNV-1a 64 halves xored of the `uint32` serializer type (as `location`'s `UINT32_TYPE`).
pub const UINT32_TYPE: u32 = 0x5d4e_4bea;
/// FNV-1a 64 halves xored of the entity id's serializer type (as `transit::wire`'s `ENTITY_ID_TYPE`).
pub const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
/// `EntityClassDefinition.ATC_DataManager` (GUID 467cdeca-a5bc-4b29-ad21-44aed9bfce35), CRC-32C of its GUID: the class
/// of the generic ATC entity, with `SCAirTrafficControllerDataManagerParams`, comms and a restricted-area ATC component.
pub const ATC_CLASS_CRC: u32 = 1_225_794_389;
/// As the hangar's content: `AddChildToBatch`, replicated.
const ATC_FLAGS: u32 = 0x1000;

/// The ATC entity of a hangar: the first id after the hangar's content, inside its stride (`HangarIds::end`).
pub fn atc_id(hangar: &HomeHangar) -> u64 {
    hangar.ids().end
}

/// The ATC as a replicated entity under the location's own entity (the planet, else the solar system), at its origin: a
/// non-vehicle ATC caches the location id of where it sits in the universe hierarchy once, when the client registers it
/// so it is the location's ATC (Orison's in Orison) and not one under the hangar.
/// The location's entity is bound before the hangar, so before this ATC.
pub fn atc_bind(hangar: &HomeHangar) -> Bind {
    Bind {
        entity_id: atc_id(hangar),
        class_crc: ATC_CLASS_CRC,
        flags: ATC_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: hangar.location_node_id(),
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots: Vec::new(),
    }
}

/// The variable that links a kiosk (or a landing area) to the ATC `atc_id`.
pub fn owner_atc_snapshot(atc_id: u64) -> Snapshot {
    Snapshot {
        key: OWNER_ATC_ID_KEY,
        type_id: ENTITY_ID_TYPE,
        value: atc_id.to_le_bytes().to_vec(),
    }
}

/// The variable that gives a kiosk the location id of the inventories its ship list reads (the home location's id).
pub fn kiosk_location_snapshot(location_id: u32) -> Snapshot {
    Snapshot {
        key: LOCATION_ID_KEY,
        type_id: UINT32_TYPE,
        value: location_id.to_le_bytes().to_vec(),
    }
}

/// CRC-32 of `maxshipdockingclass` and `maxgrounddockingclass`: the kiosk's two `int32` variables of the largest pad class
/// of ship, and of ground vehicle, it takes. Every kiosk bind of the official service carries both.
pub const MAX_SHIP_DOCKING_CLASS_KEY: u32 = 0xc9df_7699;
pub const MAX_GROUND_DOCKING_CLASS_KEY: u32 = 0x2aca_e4a9;
/// FNV-1a 64 halves xored of the `int32` serializer type.
pub const INT32_TYPE: u32 = 0xbb70_6850;
/// The ground class of every kiosk: always 5.
pub const GROUND_DOCKING_CLASS: i32 = 5;
/// The ship class the kiosks of each hangar size carry: 2, 4, 5 and 6 in the order of the sizes
/// (assumed for all but the small hangar).
pub const SHIP_DOCKING_CLASSES: [(HangarSize, i32); 4] = [
    (HangarSize::Small, 2),
    (HangarSize::Medium, 4),
    (HangarSize::Large, 5),
    (HangarSize::Xlarge, 6),
];

/// The `int32` variable `key` with `value`.
fn int32_snapshot(key: u32, value: i32) -> Snapshot {
    Snapshot {
        key,
        type_id: INT32_TYPE,
        value: value.to_le_bytes().to_vec(),
    }
}

/// The variables of a hangar's ASOP kiosk, as the official service binds them: the link to the ATC, the location of the
/// inventories the ship list reads, and the docking classes by the hangar's size (none when the size is not known).
pub fn kiosk_snapshots(hangar: &HomeHangar) -> Vec<Snapshot> {
    let mut snapshots = vec![
        owner_atc_snapshot(atc_id(hangar)),
        kiosk_location_snapshot(hangar.location.location_id),
    ];
    let size = crate::ships::hangar_class::hangar_size_of(&hangar.location.hangar_class);
    if let Some((_, class)) = SHIP_DOCKING_CLASSES
        .iter()
        .find(|(known, _)| *known == size)
    {
        snapshots.push(int32_snapshot(MAX_SHIP_DOCKING_CLASS_KEY, *class));
        snapshots.push(int32_snapshot(
            MAX_GROUND_DOCKING_CLASS_KEY,
            GROUND_DOCKING_CLASS,
        ));
    }
    snapshots
}

/// Raw bytes as hex, for logging the arguments of a call whose layout is not read yet.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// How the ASOP kiosks of a hangar are given their ATC link (`SC_DGS_KIOSK_ATC`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The client makes the kiosk container's statics from `ocIdRange`: lights, doors and the loading platform load, but
    /// the kiosk is a local entity with no `OwnerATCId`, so it logs `[NO ATC]` and opens nothing. Opt-in (`off`).
    Off,
    /// The DGS binds every entity of the kiosk container itself (`binds_container`): the kiosk is linked and calls the DGS
    /// but the container loses its `.soc` instance XML: lights, doors, loading platform.
    /// Opt-in: `SC_DGS_KIOSK_ATC=bind`.
    Bind,
    /// The container keeps its `ocIdRange` and the DGS binds the ATC and only the kiosks, each at the id the client gives
    /// its static, with `0x1000 | 0x800` and the container as parent: the client's `.soc` load finds the kiosk already a
    /// record of the container's content batch and merges the `.soc` XML into it, without making it local. With the flags of the official service's statics (`0x80300840`, the quantum grid
    /// attachment, see `location::MERGED_STATIC_FLAGS`): the first try used a assumed `0x1800` and the kiosk was
    /// gone in game. The default, as the official service binds its hangars this way.
    #[default]
    Merge,
    /// The container keeps its `ocIdRange` (the client makes the lights, doors and loading platform from the `.soc`) and the
    /// DGS binds the ATC and only the kiosks as plain replicated entities (`0x1000`, no `0x800`) at the ids the client
    /// gives their statics, with the pose and parent the `Bind` mode gives them (works in game). If the bind spawns before
    /// the `.soc` load, the client's own static at that id is the one `SpawnEntityImpl` rejects (`id collision`) and the kiosk is bound with the hangar intact; if the static comes first, the bound
    /// kiosk is rejected and the kiosk is the local `[NO ATC]` one. Opt-in: `SC_DGS_KIOSK_ATC=overlay`.
    Overlay,
}

/// The settings of `SC_DGS_KIOSK_ATC` that choose each mode (case-insensitive); any other value is `Off`.
pub const BIND_SETTINGS: [&str; 5] = ["1", "true", "on", "yes", "bind"];
pub const MERGE_SETTINGS: [&str; 1] = ["merge"];
pub const OVERLAY_SETTINGS: [&str; 1] = ["overlay"];

impl Mode {
    pub fn parse(setting: Option<&str>) -> Self {
        let Some(setting) = setting.map(str::to_ascii_lowercase) else {
            return Self::Off;
        };
        let is = |names: &[&str]| names.contains(&setting.as_str());
        if is(&BIND_SETTINGS) {
            Self::Bind
        } else if is(&MERGE_SETTINGS) {
            Self::Merge
        } else if is(&OVERLAY_SETTINGS) {
            Self::Overlay
        } else {
            Self::Off
        }
    }
}

/// The mode set by `SC_DGS_KIOSK_ATC`; **merge by default**, see `Mode`.
pub fn mode() -> Mode {
    #[cfg(test)]
    if let Some(forced) = FORCED_MODE.with(std::cell::Cell::get) {
        return forced;
    }
    let default = KIOSK_ATC_DEFAULT;
    let setting = std::env::var(KIOSK_ATC_ENV).ok();
    Mode::parse(Some(setting.as_deref().unwrap_or(default)))
}

const KIOSK_ATC_ENV: &str = "SC_DGS_KIOSK_ATC";
/// Merge when unset: the official service binds its hangars' statics this way (flags `0x80300840`), which keeps the lights, doors
/// and the loading platform the client makes from the `.soc` and links the kiosk. `bind`, `overlay` and `off` opt in.
const KIOSK_ATC_DEFAULT: &str = "merge";

/// Whether the DGS binds the ATC (`SC_DGS_KIOSK_ATC` set to any mode).
pub fn enabled() -> bool {
    mode() != Mode::Off
}

#[cfg(test)]
thread_local! {
    static FORCED_MODE: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
}

/// For a test of the switched-on behaviour: this thread's `mode()` is `Bind`.
#[cfg(test)]
pub fn force_on() {
    force_mode(Mode::Bind);
}

#[cfg(test)]
pub fn force_mode(mode: Mode) {
    FORCED_MODE.with(|forced| forced.set(Some(mode)));
}

/// The hangar ASOP kiosk classes (`EntityClassDefinition.ShipSelectorKiosk_Hangar_*`), as the tables compute class CRCs
/// (CRC-32C of the class GUID): `HighTech_1_a` (Orison, New Babbage), `LowTech_1_a`, `LowTech_1_b`, `LowTech_2_a`,
/// `LowTech_2_b` (Lorville and the others). `tools/gamedata-build` lists the same set.
pub const KIOSK_CLASS_CRCS: [u32; 5] = [
    1_689_243_590,
    3_570_166_718,
    276_579_384,
    4_185_193_948,
    2_068_243_638,
];

/// Whether an entity class is one of the hangar ASOP kiosks.
pub fn is_kiosk(class_crc: u32) -> bool {
    KIOSK_CLASS_CRCS.contains(&class_crc)
}

/// Containers with kiosks that are also a hangar's shell (`hangar_smlfrnt_002`, the rest stops': its `.soc` holds the
/// floor and the walls as brushes): they keep their id ranges, as the client needs them to load the `.soc`.
const SHELL_CONTAINERS: [u32; 1] = [2_136_596_536];

/// Whether the container holds ASOP kiosks (its entities are in `transit_prefab_entities.tsv`) and the DGS binds its
/// entities itself (`SC_DGS_KIOSK_ATC` on).
pub fn binds_container(socpak_hash: u32) -> bool {
    mode() == Mode::Bind
        && !SHELL_CONTAINERS.contains(&socpak_hash)
        && crate::transit::tables()
            .prefab_entities(socpak_hash)
            .iter()
            .any(|entity| is_kiosk(entity.class_crc))
}

/// Whether the container keeps its `ocIdRange` and only its kiosks are bound, merged into the client's statics (`Mode::Merge`).
pub fn merges_container(socpak_hash: u32) -> bool {
    mode() == Mode::Merge
        && crate::transit::tables()
            .prefab_entities(socpak_hash)
            .iter()
            .any(|entity| is_kiosk(entity.class_crc))
}

/// Whether the container keeps its `ocIdRange` and its kiosks are bound over the client's statics (`Mode::Overlay`).
pub fn overlays_container(socpak_hash: u32) -> bool {
    mode() == Mode::Overlay
        && crate::transit::tables()
            .prefab_entities(socpak_hash)
            .iter()
            .any(|entity| is_kiosk(entity.class_crc))
}

/// The ATC of a hangar that has `hangar_smlfrnt_001`; empty for any other, and when the switch is off. The kiosks that name
/// it are bound with the container's other entities (`location.rs`). Logs what it binds.
pub fn binds(hangar: &HomeHangar) -> Vec<Bind> {
    if !enabled() {
        return Vec::new();
    }
    binds_for(hangar)
}

fn binds_for(hangar: &HomeHangar) -> Vec<Bind> {
    if !hangar_content(hangar.location.socpak_hash)
        .iter()
        .any(|content| {
            binds_container(content.socpak_hash)
                || merges_container(content.socpak_hash)
                || overlays_container(content.socpak_hash)
        })
    {
        return Vec::new();
    }
    info!(
        hangar = hangar.entity_id, atc = atc_id(hangar), atc_class = ATC_CLASS_CRC, owner_atc_key = format_args!("{OWNER_ATC_ID_KEY:#010x}"),
        "binding the hangar's ATC; its kiosks are bound with the container's entities (SC_DGS_KIOSK_ATC is on)",
    );
    vec![atc_bind(hangar)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(text: &[u8]) -> u32 {
        !text.iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        })
    }

    #[test]
    fn the_link_variable_is_keyed_by_its_lowercased_name() {
        assert_eq!(OWNER_ATC_ID_KEY, crc32(b"owneratcid"));
        assert_eq!(LOCATION_ID_KEY, crc32(b"locationid"));
        let snapshot = kiosk_location_snapshot(4_005_457_614);
        assert_eq!(
            (snapshot.key, snapshot.type_id, snapshot.value),
            (
                LOCATION_ID_KEY,
                UINT32_TYPE,
                4_005_457_614u32.to_le_bytes().to_vec()
            )
        );
    }

    #[test]
    fn the_link_carries_the_atc_id_as_an_entity_id() {
        let snapshot = owner_atc_snapshot(0x0102_0304_0506_0708);
        assert_eq!(
            (snapshot.key, snapshot.type_id),
            (OWNER_ATC_ID_KEY, ENTITY_ID_TYPE)
        );
        assert_eq!(snapshot.value, 0x0102_0304_0506_0708u64.to_le_bytes());
    }

    #[test]
    fn the_atc_is_bound_under_the_location_node_inside_its_stride() {
        let hangar = HomeHangar::new(crate::location::ORBITUARY, 0, false).expect("a home hangar");
        let bind = atc_bind(&hangar);
        assert_eq!(
            (bind.class_crc, bind.parent_id),
            (ATC_CLASS_CRC, hangar.location_node_id())
        );
        assert!(bind.entity_id > hangar.ids().containers.last().map_or(0, |(id, _)| *id));
        assert!(
            bind.entity_id < crate::location::hangar_id(1),
            "the ATC is outside the hangar's block"
        );
    }

    /// `Stanton2_Orison`, whose hangar socpak is 427479476 with `hangar_smlfrnt_001` as its first container.
    #[test]
    fn a_kiosk_carries_the_variables_the_official_kiosk_bind_has() {
        let hangar = orison();
        let keys: Vec<u32> = kiosk_snapshots(&hangar)
            .iter()
            .map(|snapshot| snapshot.key)
            .collect();
        assert_eq!(
            keys,
            [
                OWNER_ATC_ID_KEY,
                LOCATION_ID_KEY,
                MAX_SHIP_DOCKING_CLASS_KEY,
                MAX_GROUND_DOCKING_CLASS_KEY
            ]
        );
        // The bytes of the two classes: key, int32 type, then the value (5 and 2).
        let ground = kiosk_snapshots(&hangar).pop().unwrap();
        assert_eq!(
            (ground.type_id, ground.value),
            (INT32_TYPE, 5i32.to_le_bytes().to_vec())
        );
    }

    fn orison() -> HomeHangar {
        HomeHangar::new(1_105_811_199, 0, false).expect("Orison's home hangar")
    }

    #[test]
    fn bytes_log_as_hex() {
        assert_eq!(hex(&[0x01, 0xab, 0x00]), "01ab00");
    }

    #[test]
    fn the_switch_merges_unless_set() {
        assert_eq!(KIOSK_ATC_DEFAULT, "merge");
        assert_eq!(Mode::default(), Mode::Merge);
        let off = [None, Some("0"), Some(""), Some("off")];
        assert!(off.iter().all(|setting| Mode::parse(*setting) == Mode::Off));
        assert!(["1", "On", "true", "YES", "bind"]
            .iter()
            .all(|setting| Mode::parse(Some(setting)) == Mode::Bind));
        assert_eq!(Mode::parse(Some("Merge")), Mode::Merge);
        assert_eq!(Mode::parse(Some("overlay")), Mode::Overlay);
    }

    #[test]
    fn orisons_hangar_gets_the_atc_and_binds_the_kiosk_container_itself() {
        force_on();
        let hangar = orison();
        let binds = binds_for(&hangar);
        assert_eq!(binds.len(), 1);
        assert_eq!(
            (binds[0].entity_id, binds[0].class_crc),
            (atc_id(&hangar), ATC_CLASS_CRC)
        );
        assert!(
            binds[0].entity_id < crate::location::hangar_id(1),
            "inside the hangar's stride"
        );
        assert!(binds_container(2_066_649_194) == enabled());
        assert!(!binds_container(1));
    }

    #[test]
    fn a_rest_stops_hangar_whose_kiosks_are_in_its_shell_binds_its_atc_for_them() {
        // The official service links the kiosks of a Pyro rest stop's shell container too: they are merged into the shell's
        // own statics, which it keeps making from the `.soc`.
        crate::atc::force_mode(Mode::Merge);
        let rest_stop =
            HomeHangar::new(crate::location::ORBITUARY, 0, false).expect("a home hangar");
        assert_eq!(binds_for(&rest_stop).len(), 1);
    }
}
