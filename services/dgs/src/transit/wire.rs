//! How the elevators' entities are written for a client: a carriage's bind and state, a gateway door's state. 
//!
//! A change of an entity's variable after its bind goes in `EntitySnapshotUpdate` by the variable's index in the
//! entity's whole layout, which counts the variables of every component registered before it. A wrong index writes
//! another variable and is fatal to the client, so where an index is not known the function
//! says so with `None`, and the caller binds the entity again with the new value as a snapshot, which is keyed by name
//! and safe: the client compares the new entity's state with what it has made and animates the difference.

use std::time::Duration;

use sc_api::internal::{EntityBind, EntityVariables};

use crate::entity::{Bind, Snapshot, Transform};
use crate::solar_system::ZONE_ATTACHMENT;

/// `ENTITY_FLAG_REPLICATION_ROOT`, and `0x400` as a zone host, as the hangar: the carriage's class has a physics grid,
/// and its riders are zone-hosted inside it (assumed; the official spawn of a carriage sets 0x1000).
const CARRIAGE_FLAGS: u32 = 0x1400;
/// CRC-32 of each variable's lowercased name, and the type hashes of their serializers (FNV-1a 64 of the type name,
/// halves xored).
const OC_FILENAME_KEY: u32 = 0xffa1_bce6;
const ASSIGNED_ID_KEY: u32 = 0xd060_646a;
const NETWORK_UPDATES_KEY: u32 = 0x3375_c405;
const DEFAULT_ACCELERATION_KEY: u32 = 0xd1ad_db7e;
const MANAGER_NAME_KEY: u32 = 0xed2d_5e3d;
const PATH_ZONE_HOST_KEY: u32 = 0xf169_d6f9;
const ZONE_HOST_KEY: u32 = 0xb790_ba5b;
const IS_AUTOMATED_KEY: u32 = 0xb1aa_e25f;
const TIME_STAMP_KEY: u32 = 0x5c73_dd7e;
const POSITION_KEY: u32 = 0x9d21_66cf;
const ROTATION_KEY: u32 = 0xf271_1acb;
const VELOCITY_KEY: u32 = 0x65d5_5493;
const ANGULAR_VELOCITY_KEY: u32 = 0x93d3_5163;
const IN_TRANSIT_KEY: u32 = 0xbe8b_9f65;
/// `tcOpenedDoors`: a varint count, then the doors' ids (writer of type 0x40a7daa4). On a client, a change opens the
/// doors added and closes those removed (`ReconcileDoorState`), once the cab interior is loaded.
const OPENED_DOORS_KEY: u32 = 0x1e97_d055;
const ENTITY_ID_VECTOR_TYPE: u32 = 0x40a7_daa4;
const UINT32_TYPE: u32 = 0x5d4e_4bea;
const BOOL_TYPE: u32 = 0x0b9f_c026;
const F32_TYPE: u32 = 0x2213_655c;
const STRING_TYPE: u32 = 0x54a0_b124;
const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
const TIME_VALUE_TYPE: u32 = 0xfee2_9f0d;
const VEC3D_TYPE: u32 = 0xd6b6_7a95;
const QUAT_TYPE: u32 = 0x2ec4_ac23;
const VEC3_TYPE: u32 = 0x36a1_5d07;
/// The carriage's default acceleration, metres per second squared, as the official spawn writes it.
const DEFAULT_ACCELERATION: f32 = 40.0;
/// `CTimeValue` counts 100 ns ticks.
const TICKS_PER_SECOND: f64 = 1e7;

/// door's `Door_OpenState`, CRC-32 of its lowercased name, and the type hash of
/// `EDoorOpenState` (computed by the same algorithm as the other type hashes; assumed). Its value is a varint.
const DOOR_OPEN_STATE_KEY: u32 = 0x7895_447b;
/// `Door_OpenState_timestamp`, a `CTimeValue` (CRC-32 of the lowercased name): the client fast-forwards a door's animation
/// by the network time minus this, so a door bound with none (0) jumps to its end, with no animation. Written as the server's time now, in ticks since the Unix epoch, which is what the
/// client's network time turned out to count in; a time ahead of the client's makes no fast-forward.
const DOOR_OPEN_STATE_TIMESTAMP_KEY: u32 = 0xdd7a_4f9e;
const DOOR_OPEN_STATE_TYPE: u32 = 0x2cf9_9cab;
/// `EDoorOpenState`: 0 Closed, 2 Closing, 3 Opened, 5 Opening.
pub const DOOR_CLOSED: u8 = 0;
pub const DOOR_CLOSING: u8 = 2;
pub const DOOR_OPENED: u8 = 3;
pub const DOOR_OPENING: u8 = 5;
/// `autoSerializeState` of door: with 0 a client's door ignores `Door_OpenState` and follows only the opening
/// and closing its carriage's `tcOpenedDoors` asks of it. The official manager sets it on a gateway's doors while a
/// carriage is there.
const AUTO_SERIALIZE_STATE_KEY: u32 = 0xba05_30e2;
/// `useAutoCloseDelay` of door (a bool like `autoSerializeState`): with 0 the door never closes itself after its
/// `autoCloseDelay`, so it stays open as long as its carriage's `tcOpenedDoors` lists it.
const USE_AUTO_CLOSE_DELAY_KEY: u32 = 0xf4c6_0a71;

/// The absolute variable indices of an in-place `EntitySnapshotUpdate`, which are **assumed** from the counts of
/// the game's registrations (+- a few) and not verified.
/// A wrong one writes another variable of the entity and a size mismatch is fatal to the client, so none is used unless
/// `SC_DGS_TRANSIT_INDICES` sets it, and without one the same changes are made by binding the entity again.
///
/// `SC_DGS_TRANSIT_INDICES=door_state,opened_doors,panel,current_stop`, each a number or empty, or `assumed` for
/// `10,6,6`: `Door_OpenState` of a cab door (a `Newbab_Gateway_Door`), `tcOpenedDoors` of a carriage and
/// `panelInteractions` of a cab panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Indices {
    pub door_state: Option<u32>,
    pub opened_doors: Option<u32>,
    pub panel: Option<u32>,
    /// `tdpCurrentStop` of a cab panel. The client writes a u32 equal to the new stop's index after an arrival,
    /// so no number is assumed: it is set by the fourth value only.
    pub current_stop: Option<u32>,
}

impl Indices {
    pub const ASSUMED: Self = Self {
        door_state: Some(10),
        opened_doors: Some(6),
        panel: Some(6),
        current_stop: None,
    };

    pub fn from_environment() -> Self {
        Self::parse(std::env::var("SC_DGS_TRANSIT_INDICES").ok().as_deref())
    }

    pub fn parse(setting: Option<&str>) -> Self {
        if setting.map(str::trim) == Some("assumed") {
            return Self::ASSUMED;
        }
        let mut parts = setting
            .unwrap_or_default()
            .split(',')
            .map(|part| part.trim().parse().ok());
        Self {
            door_state: parts.next().flatten(),
            opened_doors: parts.next().flatten(),
            panel: parts.next().flatten(),
            current_stop: parts.next().flatten(),
        }
    }
}

fn snapshot(key: u32, type_id: u32, value: Vec<u8>) -> Snapshot {
    Snapshot {
        key,
        type_id,
        value,
    }
}

/// A `CTimeValue`: the time since the server started in ticks. A carriage's samples must rise in time, or the client
/// drops them.
fn time_value(time: Duration) -> Vec<u8> {
    ((time.as_secs_f64() * TICKS_PER_SECOND) as i64)
        .to_le_bytes()
        .to_vec()
}

fn f32s(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| (*value as f32).to_le_bytes())
        .collect()
}

/// Who makes the entities of a carriage's cab (`SC_DGS_TRANSIT_CAB_STATICS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CabStatics {
    /// The DGS binds them with class defaults: the cab's lights stay dark, but its panel's presses reach the server.
    Server,
    /// The client makes them from the cab's `ocIdRange`, so the `.soc` instance XML loads: the cab's lights are on (seen
    /// in game). They are local and have authority on the client: a press on the cab panel never reaches the server, and
    /// neither does a press on a server panel bound beside it (tried in game): no stop can be picked. The cab
    /// panel did not work in game with the server's cab either, so this is the default.
    #[default]
    Client,
    /// As `Client`, and the cab panel is also bound at its own static's id inside the range (carriage + 1 + index) with
    /// `0x1000 | 0x800` and the carriage as parent: the client's `.soc` load merges the panel's XML into that record instead
    /// of making a local copy, so the lights stay on and the panel's presses reach the server.
    /// Opt-in, not verified in game: `SC_DGS_TRANSIT_CAB_STATICS=merge`.
    Merge,
}

const CAB_STATICS_ENV: &str = "SC_DGS_TRANSIT_CAB_STATICS";
const CAB_STATICS_DEFAULT: &str = "client";

impl CabStatics {
    /// `SC_DGS_TRANSIT_CAB_STATICS`; **client** when unset, `server` or `merge` to opt in.
    pub fn from_environment() -> Self {
        let setting = std::env::var(CAB_STATICS_ENV).ok();
        Self::parse(Some(setting.as_deref().unwrap_or(CAB_STATICS_DEFAULT)))
    }

    pub fn parse(setting: Option<&str>) -> Self {
        match setting
            .map(|setting| setting.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("server") => Self::Server,
            Some("merge") => Self::Merge,
            _ => Self::Client,
        }
    }
}

/// A carriage as the official spawn makes it (`TrySpawnCarriage`), bound under the hangar with the zone
/// attachment at `transform`, in the hangar's frame: the cab interior is the carriage's own object container
/// (`ocFilename`), it is the line's `slot`, and the server streams its position (`tcUseCarriageNetworkUpdates`), which
/// it gives here at rest at `time`. The manager is not bound: a client never registers `tcTransitManagerId`.
/// What every carriage of a line is: its class, its cab interior's socpak, and its manager's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarriageKind {
    pub class_crc: u32,
    pub cab_socpak_hash: u32,
    pub manager_name: String,
}

/// FNV-1a of `EntityComponentObjectContainer`, as component names are hashed (see `location::OC_PHYSICS_CONTROLLER`). The
/// official spawn adds it by name to the carriage, which is the object container of its cab.
const OBJECT_CONTAINER: u32 = 0x0001_71ef;
/// The ids a carriage keeps after its own for its cab's content: the largest cab has 15 statics.
pub const CAB_IDS: u64 = 63;

/// A carriage's place and open doors as a client is told them at its bind.
#[derive(Clone, Debug, PartialEq)]
pub struct CarriageAt {
    pub hangar_id: u64,
    pub transform: Transform,
    pub time: Duration,
    pub opened_doors: Vec<u64>,
}

fn opened_doors(doors: &[u64]) -> Vec<u8> {
    let mut value = Vec::new();
    let mut count = doors.len();
    while count >= 0x80 {
        value.push(count as u8 | 0x80);
        count >>= 7;
    }
    value.push(count as u8);
    value.extend(doors.iter().flat_map(|door| door.to_le_bytes()));
    value
}

pub fn carriage_bind(
    entity_id: u64,
    kind: &CarriageKind,
    slot: usize,
    at: &CarriageAt,
    client_statics: u64,
) -> Bind {
    let CarriageAt {
        hangar_id,
        transform,
        time,
        ..
    } = at.clone();
    let CarriageKind {
        class_crc,
        cab_socpak_hash,
        manager_name,
    } = kind;
    let (class_crc, cab_socpak_hash) = (*class_crc, *cab_socpak_hash);
    let id = hangar_id.to_le_bytes().to_vec();
    let mut snapshots = vec![
        snapshot(
            OC_FILENAME_KEY,
            UINT32_TYPE,
            cab_socpak_hash.to_le_bytes().to_vec(),
        ),
        snapshot(
            ASSIGNED_ID_KEY,
            UINT32_TYPE,
            (slot as u32).to_le_bytes().to_vec(),
        ),
        snapshot(NETWORK_UPDATES_KEY, BOOL_TYPE, vec![1]),
        snapshot(
            DEFAULT_ACCELERATION_KEY,
            F32_TYPE,
            DEFAULT_ACCELERATION.to_le_bytes().to_vec(),
        ),
        snapshot(
            MANAGER_NAME_KEY,
            STRING_TYPE,
            crate::universe::string_value(manager_name),
        ),
        snapshot(PATH_ZONE_HOST_KEY, ENTITY_ID_TYPE, id.clone()),
        snapshot(ZONE_HOST_KEY, ENTITY_ID_TYPE, id),
        snapshot(IS_AUTOMATED_KEY, BOOL_TYPE, vec![0]),
        snapshot(IN_TRANSIT_KEY, BOOL_TYPE, vec![0]),
    ];
    snapshots.extend(position_snapshots(transform, time));
    snapshots.push(snapshot(
        OPENED_DOORS_KEY,
        ENTITY_ID_VECTOR_TYPE,
        opened_doors(&at.opened_doors),
    ));
    // By default no `ocIdRange`: the client makes none of the cab's statics, which would be its own and keep its panel's
    // presses to itself. The cab's brushes still load; its entities are bound by the server (`cab_entity_bind`), with the
    // class defaults, so the cab's light group has no lights (they are in its `.soc` instance XML only).
    // With `client_statics` (`CabStatics::Client`) the client makes the cab's statics from `ocIdRange`, which begins right
    // after the carriage's id as the server-bound ones do, so the `.soc` loads locally: lights, group, doors, panel.
    let _ = cab_socpak_hash;
    snapshots.extend(crate::location::static_range_snapshots(
        entity_id + 1,
        client_statics,
    ));
    let components = vec![OBJECT_CONTAINER, crate::location::OC_PHYSICS_CONTROLLER];
    Bind {
        entity_id,
        class_crc,
        flags: CARRIAGE_FLAGS,
        transform,
        parent_id: hangar_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components,
        snapshots,
    }
}

/// An entity of the cab's `.soc`, bound by the server under its carriage with the zone attachment, at its pose in the
/// cab: the class's defaults with the instance's geometry and material as overrides, and `extra` snapshots.
pub fn cab_entity_bind(
    entity_id: u64,
    carriage_id: u64,
    entity: &crate::transit::PrefabEntity,
    extra: Vec<Snapshot>,
) -> Bind {
    cab_entity_bind_with(entity_id, carriage_id, entity, extra, CAB_ENTITY_FLAGS)
}

/// The flags of a cab entity the DGS binds on its own.
const CAB_ENTITY_FLAGS: u32 = 0x1000;

/// A cab entity merged into the client's statics (`CabStatics::Merge`): the same bind with `MERGED_STATIC_FLAGS`.
pub fn cab_merged_bind(
    entity_id: u64,
    carriage_id: u64,
    entity: &crate::transit::PrefabEntity,
    extra: Vec<Snapshot>,
) -> Bind {
    cab_entity_bind_with(
        entity_id,
        carriage_id,
        entity,
        extra,
        crate::location::MERGED_STATIC_FLAGS,
    )
}

fn cab_entity_bind_with(
    entity_id: u64,
    carriage_id: u64,
    entity: &crate::transit::PrefabEntity,
    extra: Vec<Snapshot>,
    flags: u32,
) -> Bind {
    let mut snapshots = crate::location::geometry_overrides(entity);
    snapshots.extend(extra);
    Bind {
        entity_id,
        class_crc: entity.class_crc,
        flags,
        transform: entity.pose,
        parent_id: carriage_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots,
    }
}

/// `panelInteractions` of a transit interaction panel: what the client lists and lets a player
/// press, each entry an interaction point it makes (`SVSetInteractions`). Its value is a varint count,
/// then per entry the label's localization key (varint length and bytes), the point's name hash as a little-endian
/// `u32`, and a byte of flags: 1 primary, 2 important, 4 enabled.
const PANEL_INTERACTIONS_KEY: u32 = 0x2156_4e7d;
const PANEL_INTERACTIONS_TYPE: u32 = 0xa47d_475c;
/// The point every panel's "open doors" is registered on: CRC-32 of `open_doors`.
pub const OPEN_DOORS_POINT: u32 = 0x15e6_562c;
pub const PANEL_ENABLED: u8 = 4;
/// The other two flags of a panel entry. The official service sends 1 and 4 together on a hangar's entry (the instanced stops
/// of the panel), 2 and 4 on the stops its data marks as important, and 4 alone on the rest.
pub const PANEL_PRIMARY: u8 = 1;
pub const PANEL_IMPORTANT: u8 = 2;
/// `tdpStopNames` of a panel's display (`TransitDisplayPanelParams`): per stop, in the order of `panelInteractions` without
/// the doors' button, the label's CRC-32 (lowercase, without its `@`), the name of the stop's owner (empty for a fixed stop)
/// and a byte of flags, 1 unlocked and 2 important. The official service sends one entry per stop, `Unknown` for an owner it
/// has no name for.
const STOP_NAMES_KEY: u32 = 0xe924_f32c;
const STOP_NAMES_TYPE: u32 = 0x282e_b0c2;
const STOP_UNLOCKED: u8 = 1;
const STOP_IMPORTANT: u8 = 2;
/// What the panel shows for a stop whose owner has no name.
pub const UNKNOWN_OWNER: &str = "Unknown";
/// The prefix a localization key has in `panelInteractions` and not in the name hash of `tdpStopNames`.
const LABEL_PREFIX: char = '@';
/// The point of a fixed stop: CRC-32 of `destination_<index>`, `index` the destination's place in its manager (the official
/// service's panels, from `destination_0` on).
const DESTINATION_POINT_PREFIX: &str = "destination_";
const DYNAMIC_POINT_PREFIX: &str = "dynamic_interaction_";

/// One entry of a panel's list: a label's localization key, the point's name hash, flags.
pub struct PanelEntry {
    pub label: String,
    pub point: u32,
    pub flags: u8,
}

pub fn panel_interactions(entries: &[PanelEntry]) -> Snapshot {
    let mut value = Vec::new();
    let varint = |mut number: usize, value: &mut Vec<u8>| {
        while number >= 0x80 {
            value.push(number as u8 | 0x80);
            number >>= 7;
        }
        value.push(number as u8);
    };
    varint(entries.len(), &mut value);
    for entry in entries {
        varint(entry.label.len(), &mut value);
        value.extend(entry.label.as_bytes());
        value.extend(entry.point.to_le_bytes());
        value.push(entry.flags);
    }
    snapshot(PANEL_INTERACTIONS_KEY, PANEL_INTERACTIONS_TYPE, value)
}

/// The game's name hash: CRC-32 of the text, lowercased.
pub fn name_hash(text: &str) -> u32 {
    !text
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        })
}

/// The point of a panel's `index`th instanced stop (a hangar), as official servers name them: CRC-32 of
/// `dynamic_interaction_<index>`.
pub fn stop_point(index: usize) -> u32 {
    name_hash(&format!("{DYNAMIC_POINT_PREFIX}{index}"))
}

/// The point of the manager's destination number `index`, a fixed stop of its location: CRC-32 of `destination_<index>`.
pub fn destination_point(index: usize) -> u32 {
    name_hash(&format!("{DESTINATION_POINT_PREFIX}{index}"))
}

/// One stop of a panel's display: its label, its owner's name and whether the data marks it important.
pub struct StopName {
    pub label: String,
    pub owner: String,
    pub important: bool,
}

fn push_varint(value: &mut Vec<u8>, mut number: usize) {
    while number >= 0x80 {
        value.push(number as u8 | 0x80);
        number >>= 7;
    }
    value.push(number as u8);
}

/// `tdpStopNames` for the panel's stops, in their order.
pub fn stop_names(names: &[StopName]) -> Snapshot {
    let mut value = Vec::new();
    push_varint(&mut value, names.len());
    for name in names {
        value.extend(name_hash(name.label.trim_start_matches(LABEL_PREFIX)).to_le_bytes());
        value.extend(crate::universe::string_value(&name.owner));
        value.push(STOP_UNLOCKED | if name.important { STOP_IMPORTANT } else { 0 });
    }
    snapshot(STOP_NAMES_KEY, STOP_NAMES_TYPE, value)
}

/// The display variables a panel is bound with besides its list (`TransitDisplayPanelParams`): `tdpCarriageDisplayType`, `tdpDisplayOffset`, `tdpCurrentStop`, `tdpOverrideStyleGUID`,
/// `tdpNumUnlockLocs`, `tdpPanelLocationIndex` and `tdpReverseDisplay`.
const DISPLAY_TYPE_KEY: u32 = 0x1139_eb4d;
const DISPLAY_TYPE_TYPE: u32 = 0xecb0_5fca;
const DISPLAY_OFFSET_KEY: u32 = 0x18d7_5c2b;
const DISPLAY_OFFSET_TYPE: u32 = 0x8f29_9ff7;
const CURRENT_STOP_KEY: u32 = 0x5e3e_3533;
const STOP_INDEX_TYPE: u32 = 0x5d4e_4bea;
const STYLE_KEY: u32 = 0x82c5_7d34;
const STYLE_TYPE: u32 = 0x7009_3a01;
const NUM_UNLOCK_KEY: u32 = 0xbd23_a6c5;
const PANEL_LOCATION_KEY: u32 = 0xc348_731a;
const REVERSE_DISPLAY_KEY: u32 = 0xf572_aad1;
const REVERSE_DISPLAY_TYPE: u32 = 0x0b9f_c026;
/// The display type of the interior panels: the Habs line's, and the others' (the Hangar-to-Lobby's).
pub const DISPLAY_TYPE_HABS: [u8; 4] = [0xdb, 0xf8, 0xf3, 0xe0];
pub const DISPLAY_TYPE_DEFAULT: [u8; 4] = [0xc6, 0x06, 0x75, 0x26];
/// The style every interior panel carries (`tdpOverrideStyleGUID`, as sent).
pub const PANEL_STYLE: [u8; 16] = [
    0xa0, 0x44, 0x36, 0x25, 0x79, 0x36, 0x61, 0x86, 0x25, 0x57, 0x42, 0x3e, 0xcd, 0xa0, 0x85, 0xa1,
];
/// `SC_DGS_PANEL_STYLE=live` sends `PANEL_STYLE` as `tdpOverrideStyleGUID`; unset, no style is sent. In Orison the GUID resolves through `GetDataCoreRecordInstance` and the official one is not a
/// DataCore record (it resolves to null), and no stock data gives Orison an override, so the canvas default
/// (`I_Trn_Elevator_Interior`) is what shows when none is sent.
pub const PANEL_STYLE_SETTING: &str = "SC_DGS_PANEL_STYLE";
pub const PANEL_STYLE_OFFICIAL: &str = "live";

fn sends_panel_style() -> bool {
    std::env::var(PANEL_STYLE_SETTING).is_ok_and(|value| value.eq_ignore_ascii_case(PANEL_STYLE_OFFICIAL))
}
/// The name prefix of the manager whose panels have `DISPLAY_TYPE_HABS`.
pub const HABS_MANAGER_PREFIX: &str = "TransitManager_Habs";

/// The display a panel shows besides its list: `current` is the index (in the order of `tdpStopNames`) of the stop the
/// carriage is at, which the panel names as the current floor, and `total` how many stops it lists.
pub fn panel_display(manager_name: &str, current: u32, total: u32) -> Vec<Snapshot> {
    let display_type = if manager_name.starts_with(HABS_MANAGER_PREFIX) {
        DISPLAY_TYPE_HABS
    } else {
        DISPLAY_TYPE_DEFAULT
    };
    let mut display = vec![
        snapshot(DISPLAY_TYPE_KEY, DISPLAY_TYPE_TYPE, display_type.to_vec()),
        snapshot(DISPLAY_OFFSET_KEY, DISPLAY_OFFSET_TYPE, vec![0]),
        snapshot(
            CURRENT_STOP_KEY,
            STOP_INDEX_TYPE,
            current.to_le_bytes().to_vec(),
        ),
        snapshot(
            NUM_UNLOCK_KEY,
            STOP_INDEX_TYPE,
            total.to_le_bytes().to_vec(),
        ),
        snapshot(
            PANEL_LOCATION_KEY,
            STOP_INDEX_TYPE,
            0u32.to_le_bytes().to_vec(),
        ),
        snapshot(REVERSE_DISPLAY_KEY, REVERSE_DISPLAY_TYPE, vec![0]),
    ];
    if sends_panel_style() {
        display.push(snapshot(STYLE_KEY, STYLE_TYPE, PANEL_STYLE.to_vec()));
    }
    display
}

/// `tcManagerName` of a panel: the name of the manager it belongs to, as the carriage has it.
pub fn panel_manager_name(manager_name: &str) -> Snapshot {
    snapshot(
        MANAGER_NAME_KEY,
        STRING_TYPE,
        crate::universe::string_value(manager_name),
    )
}

/// The `tcCarriagePositionData` group at rest: time, position, rotation and no velocity, in the zone host's frame.
fn position_snapshots(transform: Transform, time: Duration) -> Vec<Snapshot> {
    vec![
        snapshot(TIME_STAMP_KEY, TIME_VALUE_TYPE, time_value(time)),
        snapshot(
            POSITION_KEY,
            VEC3D_TYPE,
            transform
                .position
                .iter()
                .flat_map(|axis| axis.to_le_bytes())
                .collect(),
        ),
        snapshot(ROTATION_KEY, QUAT_TYPE, f32s(&transform.rotation)),
        snapshot(VELOCITY_KEY, VEC3_TYPE, f32s(&[0.0; 3])),
        snapshot(ANGULAR_VELOCITY_KEY, VEC3_TYPE, f32s(&[0.0; 3])),
    ]
}

/// A bound carriage moved to `transform`, `time` after the server started. `None` while the position's index is not
/// known: the caller binds it again.
pub fn carriage_moved(
    entity_id: u64,
    hangar_id: u64,
    transform: Transform,
    time: Duration,
) -> Option<EntityVariables> {
    let _ = (entity_id, hangar_id, transform, time);
    None
}

/// A carriage leaving for or arriving from limbo (`tcInTransit`). `None` while its index is not known; the client
/// then sees the move alone.
pub fn carriage_state(entity_id: u64, in_transit: bool) -> Option<EntityVariables> {
    let _ = (entity_id, in_transit);
    None
}

/// A carriage's open doors (`tcOpenedDoors`) in place, ascending as the client's set difference needs them. `None`
/// while its index is not set.
pub fn carriage_opened_doors(
    entity_id: u64,
    doors: &[u64],
    indices: &Indices,
) -> Option<EntityVariables> {
    let mut doors = doors.to_vec();
    doors.sort_unstable();
    variable(entity_id, indices.opened_doors?, opened_doors(&doors))
}

/// A door's `Door_OpenState` in place. A client's door animates only from the state before: 5 (opening) while closed
/// then 3, 2 (closing) while opened then 0; 3 or 0 first snaps it. `None` while its index is not set.
pub fn door_state(entity_id: u64, state: u8, indices: &Indices) -> Option<EntityVariables> {
    variable(entity_id, indices.door_state?, vec![state])
}

/// A cab panel's `panelInteractions` in place. `None` while its index is not set.
pub fn panel_update(
    entity_id: u64,
    entries: &[PanelEntry],
    indices: &Indices,
) -> Option<EntityVariables> {
    variable(entity_id, indices.panel?, panel_interactions(entries).value)
}

/// A cab panel's `tdpCurrentStop` in place: the index of the stop the carriage arrived at, in the order of `tdpStopNames`
/// (the client writes the new stop as a u32). `None` while its index is not set.
pub fn current_stop_update(
    entity_id: u64,
    stop: u32,
    indices: &Indices,
) -> Option<EntityVariables> {
    variable(entity_id, indices.current_stop?, stop.to_le_bytes().to_vec())
}

fn variable(entity_id: u64, index: u32, value: Vec<u8>) -> Option<EntityVariables> {
    Some(EntityVariables {
        entity_id,
        variables: vec![sc_api::internal::EntityVariable { index, value }],
    })
}

/// `autoSerializeState` off, for the doors that follow their carriage's `tcOpenedDoors` only.
pub fn auto_serialize_off() -> Snapshot {
    snapshot(AUTO_SERIALIZE_STATE_KEY, BOOL_TYPE, vec![0])
}

/// `useAutoCloseDelay` off, for the doors the carriage opens and closes: they close only when it says so.
pub fn auto_close_off() -> Snapshot {
    snapshot(USE_AUTO_CLOSE_DELAY_KEY, BOOL_TYPE, vec![0])
}

/// A door's bind with its state as a snapshot: a fresh door starts closed on the client, which animates to this state.
pub fn door_rebind(bind: &EntityBind, open: bool) -> EntityBind {
    let mut bind = bind.clone();
    bind.snapshots
        .retain(|snapshot| snapshot.key != DOOR_OPEN_STATE_KEY);
    let state = Snapshot {
        key: DOOR_OPEN_STATE_KEY,
        type_id: DOOR_OPEN_STATE_TYPE,
        value: vec![if open { DOOR_OPENING } else { DOOR_CLOSED }],
    };
    bind.snapshots.push(state.into());
    bind.snapshots
        .retain(|snapshot| snapshot.key != DOOR_OPEN_STATE_TIMESTAMP_KEY);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    bind.snapshots.push(
        snapshot(
            DOOR_OPEN_STATE_TIMESTAMP_KEY,
            TIME_VALUE_TYPE,
            time_value(now),
        )
        .into(),
    );
    bind
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

    /// Two stops and the doors, as the official example has them.
    #[test]
    fn a_panel_lists_its_stops_and_the_doors() {
        let entries = [
            PanelEntry {
                label: "@ui_interactor_ST2_spaceport".into(),
                point: 0x2062_7c32,
                flags: PANEL_ENABLED,
            },
            PanelEntry {
                label: "@ui_interactor_hangar_small".into(),
                point: 0x5765_4ca4,
                flags: PANEL_ENABLED,
            },
            PanelEntry {
                label: "@innerthought_open".into(),
                point: OPEN_DOORS_POINT,
                flags: PANEL_ENABLED,
            },
        ];
        let snapshot = panel_interactions(&entries);
        assert_eq!(
            (snapshot.key, snapshot.type_id, snapshot.value.len()),
            (crc32(b"panelinteractions"), 0xa47d_475c, 92)
        );
        assert_eq!(&snapshot.value[..2], [3, 28]);
        assert_eq!(&snapshot.value[30..35], [0x32, 0x7c, 0x62, 0x20, 4]);
        assert_eq!((stop_point(0), stop_point(1)), (0x2062_7c32, 0x5765_4ca4));
        assert_eq!(crc32(b"open_doors"), OPEN_DOORS_POINT);
    }

    /// The panel of the cab of the Pyro hangar elevator: fifteen fixed stops at `destination_0`.., a second important `ui_dest_entry_pyro` at `destination_15`,
    /// the player's large hangar at `dynamic_interaction_0` and the doors.
    const PANEL_INTERACTIONS: &str = "12134075695f646573745f656e7472795f7079726fc614aacf061040617265615f6e616d655f636172676f5024adb8041340617265615f6e616d655f726566696e657279ea75a421041c4073686f705f6e616d655f6c61776c657373736869706465616c65727c45a356041d4075695f696e7465726163746f725f646f636b696e67706f72745f3031dfd0c7c8040f40646f636b696e67706f72745f303249e0c0bf04154075695f696e7465726163746f725f7061645f3031f3b1c92604154075695f696e7465726163746f725f7061645f30326581ce5104154075695f696e7465726163746f725f7061645f3033f49c71c1040d40636172676f6465636b5f303162ac76b6040d40636172676f6465636b5f3032f123089f040d40636172676f6465636b5f303367130fe8040d40636172676f6465636b5f3034dd420671040d40636172676f6465636b5f30354b720106040d40636172676f6465636b5f3036e8e7659804134075695f646573745f656e7472795f7079726f7ed762ef061b4075695f696e7465726163746f725f68616e6761725f6c61726765327c6220051240696e6e657274686f756768745f6f70656e2c56e61504";
    /// Its display's `tdpStopNames`, whose flag bytes carry stray high bits that are not part of the value.
    const OFFICIAL_STOP_NAMES: &str = "110f5f968300038ee7387a00015f658e8c0001e011e40200014a29c07200590ae2f9200001b67878a6001d0c29713f00019a197648000974ee3b8a0001cebf32130001588f35640001fb1a51fa00016d2a568d0001d77b5f1400010f5f9683000358c2acc109456464795f4162726101";

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn official_stops() -> Vec<(&'static str, u32, u8, &'static str)> {
        let mut stops: Vec<(&str, u32, u8, &str)> = [
            "@ui_dest_entry_pyro",
            "@area_name_cargo",
            "@area_name_refinery",
            "@shop_name_lawlessshipdealer",
            "@ui_interactor_dockingport_01",
            "@dockingport_02",
            "@ui_interactor_pad_01",
            "@ui_interactor_pad_02",
            "@ui_interactor_pad_03",
            "@cargodeck_01",
            "@cargodeck_02",
            "@cargodeck_03",
            "@cargodeck_04",
            "@cargodeck_05",
            "@cargodeck_06",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            (
                label,
                destination_point(index),
                if index == 0 {
                    PANEL_IMPORTANT | PANEL_ENABLED
                } else {
                    PANEL_ENABLED
                },
                "",
            )
        })
        .collect();
        stops.push((
            "@ui_dest_entry_pyro",
            destination_point(15),
            PANEL_IMPORTANT | PANEL_ENABLED,
            "",
        ));
        stops.push((
            "@ui_interactor_hangar_large",
            stop_point(0),
            PANEL_PRIMARY | PANEL_ENABLED,
            "Eddy_Abra",
        ));
        stops
    }

    #[test]
    fn a_panel_lists_its_stops_as_the_official_service_does() {
        let mut entries: Vec<PanelEntry> = official_stops()
            .into_iter()
            .map(|(label, point, flags, _)| PanelEntry {
                label: label.into(),
                point,
                flags,
            })
            .collect();
        entries.push(PanelEntry {
            label: "@innerthought_open".into(),
            point: OPEN_DOORS_POINT,
            flags: PANEL_ENABLED,
        });
        assert_eq!(
            panel_interactions(&entries).value,
            hex_bytes(PANEL_INTERACTIONS)
        );
        assert_eq!(
            (
                destination_point(0),
                destination_point(1),
                destination_point(15)
            ),
            (0xcfaa_14c6, 0xb8ad_2450, 0xef62_d77e)
        );
    }

    #[test]
    fn a_displays_stop_names_are_the_official_ones_but_for_stray_bits() {
        let names: Vec<StopName> = official_stops()
            .into_iter()
            .map(|(label, _, flags, owner)| StopName {
                label: label.into(),
                owner: owner.into(),
                important: flags & PANEL_IMPORTANT != 0,
            })
            .collect();
        let ours = stop_names(&names);
        assert_eq!((ours.key, ours.type_id), (0xe924_f32c, 0x282e_b0c2));
        // Per entry: the label's hash, the owner's name, and a flags byte whose low two bits are the value.
        let clean = |value: &[u8]| {
            let mut out = Vec::new();
            let (count, mut at) = (value[0] as usize, 1);
            for _ in 0..count {
                let hash = u32::from_le_bytes(value[at..at + 4].try_into().unwrap());
                let length = value[at + 4] as usize;
                let owner = String::from_utf8(value[at + 5..at + 5 + length].to_vec()).unwrap();
                out.push((hash, owner, value[at + 5 + length] & 3));
                at += 6 + length;
            }
            assert_eq!(at, value.len());
            out
        };
        assert_eq!(clean(&ours.value), clean(&hex_bytes(OFFICIAL_STOP_NAMES)));
    }

    #[test]
    fn the_door_state_is_keyed_by_its_name() {
        assert_eq!(crc32(b"door_openstate"), DOOR_OPEN_STATE_KEY);
        assert_eq!(crc32(b"autoserializestate"), AUTO_SERIALIZE_STATE_KEY);
        let off = auto_serialize_off();
        assert_eq!(
            (off.key, off.type_id, off.value),
            (0xba05_30e2, 0x0b9f_c026, vec![0])
        );
        assert_eq!(crc32(b"useautoclosedelay"), USE_AUTO_CLOSE_DELAY_KEY);
        let close = auto_close_off();
        assert_eq!(
            (close.key, close.type_id, close.value),
            (0xf4c6_0a71, 0x0b9f_c026, vec![0])
        );
    }

    #[test]
    fn indices_are_set_only_by_the_setting() {
        assert_eq!(Indices::parse(None), Indices::default());
        assert_eq!(
            Indices::parse(Some("10,,3")),
            Indices {
                door_state: Some(10),
                opened_doors: None,
                panel: Some(3),
                current_stop: None
            }
        );
        assert_eq!(Indices::parse(Some("assumed")), Indices::ASSUMED);
        let off = Indices::default();
        assert_eq!(
            (
                door_state(7, DOOR_OPENING, &off),
                carriage_opened_doors(7, &[1], &off),
                panel_update(7, &[], &off)
            ),
            (None, None, None)
        );
    }

    #[test]
    fn in_place_updates_are_byte_exact() {
        let indices = Indices::ASSUMED;
        let state = door_state(7, DOOR_OPENING, &indices).unwrap();
        assert_eq!(
            (
                state.entity_id,
                state.variables[0].index,
                state.variables[0].value.clone()
            ),
            (7, 10, vec![5])
        );
        let opened = carriage_opened_doors(9, &[300, 20, 7], &indices).unwrap();
        let expect = [
            &[3u8][..],
            &7u64.to_le_bytes(),
            &20u64.to_le_bytes(),
            &300u64.to_le_bytes(),
        ]
        .concat();
        assert_eq!(
            (opened.variables[0].index, opened.variables[0].value.clone()),
            (6, expect)
        );
        assert_eq!(
            carriage_opened_doors(9, &[], &indices).unwrap().variables[0].value,
            [0]
        );
    }

    #[test]
    fn the_carriage_variables_are_keyed_by_their_names() {
        let names = [
            ("ocfilename", OC_FILENAME_KEY),
            ("tcassignedid", ASSIGNED_ID_KEY),
            ("tcusecarriagenetworkupdates", NETWORK_UPDATES_KEY),
            ("tcdefaultacceleration", DEFAULT_ACCELERATION_KEY),
            ("tcmanagername", MANAGER_NAME_KEY),
            ("tcpathzonehost", PATH_ZONE_HOST_KEY),
            ("tczonehostentityid", ZONE_HOST_KEY),
            ("tcisautomated", IS_AUTOMATED_KEY),
            ("tctimestamp", TIME_STAMP_KEY),
            ("tccarriageposition", POSITION_KEY),
            ("tccarriagerotation", ROTATION_KEY),
            ("tccarriagevelocity", VELOCITY_KEY),
            ("tccarriageangularvelocity", ANGULAR_VELOCITY_KEY),
            ("tcintransit", IN_TRANSIT_KEY),
            ("tcopeneddoors", OPENED_DOORS_KEY),
        ];
        for (name, key) in names {
            assert_eq!(crc32(name.as_bytes()), key, "{name}");
        }
    }

    #[test]
    fn a_carriage_is_bound_where_it_stands_with_its_cab_and_line() {
        let at = Transform {
            position: [20.0, -88.0, 4.25],
            rotation: [0.0, 0.0, 0.0, 1.0],
        };
        let kind = CarriageKind {
            class_crc: 2_423_595_438,
            cab_socpak_hash: 3_194_931_106,
            manager_name: "TransitManager_Spaceport_to_Hangars".into(),
        };
        let bind = carriage_bind(
            9,
            &kind,
            2,
            &CarriageAt {
                hangar_id: 5,
                transform: at,
                time: Duration::from_secs(3),
                opened_doors: vec![20, 7],
            },
            0,
        );
        assert_eq!(
            bind.snapshots
                .iter()
                .find(|snapshot| snapshot.key == OPENED_DOORS_KEY)
                .unwrap()
                .value,
            [&[2u8][..], &20u64.to_le_bytes(), &7u64.to_le_bytes()].concat()
        );
        let value = |key: u32| {
            bind.snapshots
                .iter()
                .find(|snapshot| snapshot.key == key)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value(OC_FILENAME_KEY), 3_194_931_106u32.to_le_bytes());
        assert_eq!(value(ASSIGNED_ID_KEY), 2u32.to_le_bytes());
        assert_eq!(value(MANAGER_NAME_KEY)[0], 35);
        assert_eq!(value(TIME_STAMP_KEY), 30_000_000i64.to_le_bytes());
        assert_eq!(value(POSITION_KEY).len(), 24);
        assert_eq!(
            (bind.parent_id, bind.parent_attach_id, bind.flags),
            (5, ZONE_ATTACHMENT, 0x1400)
        );
        assert_eq!(
            bind.components,
            [OBJECT_CONTAINER, crate::location::OC_PHYSICS_CONTROLLER]
        );
        assert!(
            bind.snapshots
                .iter()
                .all(|snapshot| snapshot.key != 0xe359_14ad),
            "no ocIdRange: the server binds the cab's entities"
        );
    }

    #[test]
    fn a_carriage_gives_the_client_its_cabs_id_range_only_when_asked() {
        let at = Transform {
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
        };
        let kind = CarriageKind {
            class_crc: 2_423_595_438,
            cab_socpak_hash: 3_194_931_106,
            manager_name: "m".into(),
        };
        let at = CarriageAt {
            hangar_id: 5,
            transform: at,
            time: Duration::from_secs(3),
            opened_doors: Vec::new(),
        };
        let bind = carriage_bind(100, &kind, 0, &at, 12);
        let range = bind
            .snapshots
            .iter()
            .find(|snapshot| snapshot.key == 0xe359_14ad)
            .expect("ocIdRange");
        assert_eq!(
            range.value,
            [101u64.to_le_bytes(), 112u64.to_le_bytes()].concat()
        );
        assert!(
            bind.snapshots
                .iter()
                .any(|snapshot| snapshot.key == 0x09f4_d2d5),
            "ocIdRangeMov, empty"
        );
        assert_eq!(CabStatics::parse(Some(" Client ")), CabStatics::Client);
        assert_eq!(CabStatics::parse(Some("server")), CabStatics::Server);
        assert_eq!(CabStatics::parse(None), CabStatics::Client);
    }

    #[test]
    fn a_door_rebound_open_carries_its_state_once() {
        let bind = EntityBind {
            entity_id: 7,
            ..EntityBind::default()
        };
        let open = door_rebind(&door_rebind(&bind, false), true);
        let states: Vec<&[u8]> = open
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.key == DOOR_OPEN_STATE_KEY)
            .map(|snapshot| snapshot.value.as_slice())
            .collect();
        assert_eq!(states, [[DOOR_OPENING]]);
    }
}
