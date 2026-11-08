//! How the elevators' entities are written for a client: a carriage's bind and state, a gateway door's state. See the
//! RE repository's `docs/transit/phase3_carriage.md` and `phase3_doors_unbind.md`.
//!
//! A change of an entity's variable after its bind goes in `EntitySnapshotUpdate` by the variable's index in the
//! entity's whole layout, which counts the variables of every component registered before it. A wrong index writes
//! another variable and is fatal to the client, so where an index has not been read from a running client the function
//! says so with `None`, and the caller binds the entity again with the new value as a snapshot, which is keyed by name
//! and safe: the client compares the new entity's state with what it has made and animates the difference.

use std::time::Duration;

use sc_api::internal::{EntityBind, EntityVariables};

use crate::entity::{Bind, Snapshot, Transform};
use crate::solar_system::ZONE_ATTACHMENT;

/// `ENTITY_FLAG_REPLICATION_ROOT`, and `0x400` as a zone host, as the hangar: the carriage's class has a physics grid,
/// and its riders are zone-hosted inside it (inferred; the official spawn, `TrySpawnCarriage` 0x1437237d0, sets 0x1000).
const CARRIAGE_FLAGS: u32 = 0x1400;
/// CRC-32 of each variable's lowercased name, and the type hashes of their serializers (FNV-1a 64 of the type name,
/// halves xored), from the carriage's PostInit (0x1436d4e70).
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
/// doors added and closes those removed (`ReconcileDoorState`, 0x1436e0e50), once the cab interior is loaded.
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

/// `SCItemDoor`'s `Door_OpenState` (registered by 0x143de6f70), CRC-32 of its lowercased name, and the type hash of
/// `EDoorOpenState` (computed by the algorithm of 0x1403e1350; inferred). Its value is a varint.
const DOOR_OPEN_STATE_KEY: u32 = 0x7895_447b;
const DOOR_OPEN_STATE_TYPE: u32 = 0x2cf9_9cab;
/// `EDoorOpenState`: 0 Closed, 3 Opened, 5 Opening.
const DOOR_CLOSED: u8 = 0;
const DOOR_OPENING: u8 = 5;
/// The index of `Door_OpenState` in an `ElevatorDoorDefaultTemplate`'s layout: not read yet.
const DOOR_OPEN_STATE_INDEX: Option<u32> = None;

fn snapshot(key: u32, type_id: u32, value: Vec<u8>) -> Snapshot {
    Snapshot { key, type_id, value }
}

/// A `CTimeValue`: the time since the server started in ticks. A carriage's samples must rise in time, or the client
/// drops them.
fn time_value(time: Duration) -> Vec<u8> {
    ((time.as_secs_f64() * TICKS_PER_SECOND) as i64).to_le_bytes().to_vec()
}

fn f32s(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|value| (*value as f32).to_le_bytes()).collect()
}

/// A carriage as the official spawn makes it (`TrySpawnCarriage`, 0x1437237d0), bound under the hangar with the zone
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

pub fn carriage_bind(entity_id: u64, kind: &CarriageKind, slot: usize, at: &CarriageAt) -> Bind {
    let CarriageAt { hangar_id, transform, time, .. } = at.clone();
    let CarriageKind { class_crc, cab_socpak_hash, manager_name } = kind;
    let (class_crc, cab_socpak_hash) = (*class_crc, *cab_socpak_hash);
    let id = hangar_id.to_le_bytes().to_vec();
    let mut snapshots = vec![
        snapshot(OC_FILENAME_KEY, UINT32_TYPE, cab_socpak_hash.to_le_bytes().to_vec()),
        snapshot(ASSIGNED_ID_KEY, UINT32_TYPE, (slot as u32).to_le_bytes().to_vec()),
        snapshot(NETWORK_UPDATES_KEY, BOOL_TYPE, vec![1]),
        snapshot(DEFAULT_ACCELERATION_KEY, F32_TYPE, DEFAULT_ACCELERATION.to_le_bytes().to_vec()),
        snapshot(MANAGER_NAME_KEY, STRING_TYPE, crate::universe::string_value(manager_name)),
        snapshot(PATH_ZONE_HOST_KEY, ENTITY_ID_TYPE, id.clone()),
        snapshot(ZONE_HOST_KEY, ENTITY_ID_TYPE, id),
        snapshot(IS_AUTOMATED_KEY, BOOL_TYPE, vec![0]),
        snapshot(IN_TRANSIT_KEY, BOOL_TYPE, vec![0]),
    ];
    snapshots.extend(position_snapshots(transform, time));
    snapshots.push(snapshot(OPENED_DOORS_KEY, ENTITY_ID_VECTOR_TYPE, opened_doors(&at.opened_doors)));
    // No `ocIdRange`: the client makes none of the cab's statics, which would be its own and keep its panel's presses
    // to itself. The cab's brushes still load; its entities are bound by the server (`cab_entity_bind`).
    let _ = cab_socpak_hash;
    let components = vec![OBJECT_CONTAINER, crate::location::OC_PHYSICS_CONTROLLER];
    Bind { entity_id, class_crc, flags: CARRIAGE_FLAGS, transform, parent_id: hangar_id, parent_attach_id: ZONE_ATTACHMENT, components, snapshots }
}

/// An entity of the cab's `.soc`, bound by the server under its carriage with the zone attachment, at its pose in the
/// cab: the class's defaults with the instance's geometry and material as overrides, and `extra` snapshots.
pub fn cab_entity_bind(entity_id: u64, carriage_id: u64, entity: &crate::transit::PrefabEntity, extra: Vec<Snapshot>) -> Bind {
    let mut snapshots = crate::location::geometry_overrides(entity);
    snapshots.extend(extra);
    Bind {
        entity_id,
        class_crc: entity.class_crc,
        flags: 0x1000,
        transform: entity.pose,
        parent_id: carriage_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots,
    }
}

/// `panelInteractions` of a transit interaction panel (PostInit 0x1436d7930): what the client lists and lets a player
/// press, each entry an interaction point it makes (`SVSetInteractions`, 0x1436ca250). Its value is a varint count,
/// then per entry the label's localization key (varint length and bytes), the point's name hash as a little-endian
/// `u32`, and a byte of flags: 1 primary, 2 important, 4 enabled.
const PANEL_INTERACTIONS_KEY: u32 = 0x2156_4e7d;
const PANEL_INTERACTIONS_TYPE: u32 = 0xa47d_475c;
/// The point every panel's "open doors" is registered on: CRC-32 of `open_doors`.
pub const OPEN_DOORS_POINT: u32 = 0x15e6_562c;
pub const PANEL_ENABLED: u8 = 4;

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

/// The point of a panel's `index`th stop, as official servers name them: CRC-32 of `dynamic_interaction_<index>`.
pub fn stop_point(index: usize) -> u32 {
    let text = format!("dynamic_interaction_{index}");
    !text.bytes().fold(!0u32, |crc, byte| (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 }))
}

/// The `tcCarriagePositionData` group at rest: time, position, rotation and no velocity, in the zone host's frame.
fn position_snapshots(transform: Transform, time: Duration) -> Vec<Snapshot> {
    vec![
        snapshot(TIME_STAMP_KEY, TIME_VALUE_TYPE, time_value(time)),
        snapshot(POSITION_KEY, VEC3D_TYPE, transform.position.iter().flat_map(|axis| axis.to_le_bytes()).collect()),
        snapshot(ROTATION_KEY, QUAT_TYPE, f32s(&transform.rotation)),
        snapshot(VELOCITY_KEY, VEC3_TYPE, f32s(&[0.0; 3])),
        snapshot(ANGULAR_VELOCITY_KEY, VEC3_TYPE, f32s(&[0.0; 3])),
    ]
}

/// A bound carriage moved to `transform`, `time` after the server started. `None` while the position's index is not
/// known: the caller binds it again.
pub fn carriage_moved(entity_id: u64, hangar_id: u64, transform: Transform, time: Duration) -> Option<EntityVariables> {
    let _ = (entity_id, hangar_id, transform, time);
    None
}

/// A carriage leaving for or arriving from limbo (`tcInTransit`). `None` while its index is not known; the client
/// then sees the move alone.
pub fn carriage_state(entity_id: u64, in_transit: bool) -> Option<EntityVariables> {
    let _ = (entity_id, in_transit);
    None
}

/// A carriage's open doors (`tcOpenedDoors`). `None` while its index is not known.
pub fn carriage_opened_doors(entity_id: u64, doors: &[u64]) -> Option<EntityVariables> {
    let _ = (entity_id, doors);
    None
}

/// A door to open or close by an update, `None` while the index of its state is not known.
pub fn door(entity_id: u64, open: bool) -> Option<EntityVariables> {
    let index = DOOR_OPEN_STATE_INDEX?;
    let value = vec![if open { DOOR_OPENING } else { DOOR_CLOSED }];
    Some(EntityVariables { entity_id, variables: vec![sc_api::internal::EntityVariable { index, value }] })
}

/// A door's bind with its state as a snapshot: a fresh door starts closed on the client, which animates to this state.
pub fn door_rebind(bind: &EntityBind, open: bool) -> EntityBind {
    let mut bind = bind.clone();
    bind.snapshots.retain(|snapshot| snapshot.key != DOOR_OPEN_STATE_KEY);
    let state = Snapshot { key: DOOR_OPEN_STATE_KEY, type_id: DOOR_OPEN_STATE_TYPE, value: vec![if open { DOOR_OPENING } else { DOOR_CLOSED }] };
    bind.snapshots.push(state.into());
    bind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(text: &[u8]) -> u32 {
        !text.iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        })
    }

    /// Two stops and the doors, as the research's checked example has them.
    #[test]
    fn a_panel_lists_its_stops_and_the_doors() {
        let entries = [
            PanelEntry { label: "@ui_interactor_ST2_spaceport".into(), point: 0x2062_7c32, flags: PANEL_ENABLED },
            PanelEntry { label: "@ui_interactor_hangar_small".into(), point: 0x5765_4ca4, flags: PANEL_ENABLED },
            PanelEntry { label: "@innerthought_open".into(), point: OPEN_DOORS_POINT, flags: PANEL_ENABLED },
        ];
        let snapshot = panel_interactions(&entries);
        assert_eq!((snapshot.key, snapshot.type_id, snapshot.value.len()), (crc32(b"panelinteractions"), 0xa47d_475c, 92));
        assert_eq!(&snapshot.value[..2], [3, 28]);
        assert_eq!(&snapshot.value[30..35], [0x32, 0x7c, 0x62, 0x20, 4]);
        assert_eq!((stop_point(0), stop_point(1)), (0x2062_7c32, 0x5765_4ca4));
        assert_eq!(crc32(b"open_doors"), OPEN_DOORS_POINT);
    }

    #[test]
    fn the_door_state_is_keyed_by_its_name() {
        assert_eq!(crc32(b"door_openstate"), DOOR_OPEN_STATE_KEY);
    }

    #[test]
    fn the_carriage_variables_are_keyed_by_their_names() {
        let names = [
            ("ocfilename", OC_FILENAME_KEY), ("tcassignedid", ASSIGNED_ID_KEY), ("tcusecarriagenetworkupdates", NETWORK_UPDATES_KEY),
            ("tcdefaultacceleration", DEFAULT_ACCELERATION_KEY), ("tcmanagername", MANAGER_NAME_KEY), ("tcpathzonehost", PATH_ZONE_HOST_KEY),
            ("tczonehostentityid", ZONE_HOST_KEY), ("tcisautomated", IS_AUTOMATED_KEY), ("tctimestamp", TIME_STAMP_KEY),
            ("tccarriageposition", POSITION_KEY), ("tccarriagerotation", ROTATION_KEY), ("tccarriagevelocity", VELOCITY_KEY),
            ("tccarriageangularvelocity", ANGULAR_VELOCITY_KEY), ("tcintransit", IN_TRANSIT_KEY), ("tcopeneddoors", OPENED_DOORS_KEY),
        ];
        for (name, key) in names {
            assert_eq!(crc32(name.as_bytes()), key, "{name}");
        }
    }

    #[test]
    fn a_carriage_is_bound_where_it_stands_with_its_cab_and_line() {
        let at = Transform { position: [20.0, -88.0, 4.25], rotation: [0.0, 0.0, 0.0, 1.0] };
        let kind = CarriageKind { class_crc: 2_423_595_438, cab_socpak_hash: 3_194_931_106, manager_name: "TransitManager_Spaceport_to_Hangars".into() };
        let bind = carriage_bind(9, &kind, 2, &CarriageAt { hangar_id: 5, transform: at, time: Duration::from_secs(3), opened_doors: vec![20, 7] });
        assert_eq!(bind.snapshots.iter().find(|snapshot| snapshot.key == OPENED_DOORS_KEY).unwrap().value, [&[2u8][..], &20u64.to_le_bytes(), &7u64.to_le_bytes()].concat());
        let value = |key: u32| bind.snapshots.iter().find(|snapshot| snapshot.key == key).unwrap().value.clone();
        assert_eq!(value(OC_FILENAME_KEY), 3_194_931_106u32.to_le_bytes());
        assert_eq!(value(ASSIGNED_ID_KEY), 2u32.to_le_bytes());
        assert_eq!(value(MANAGER_NAME_KEY)[0], 35);
        assert_eq!(value(TIME_STAMP_KEY), 30_000_000i64.to_le_bytes());
        assert_eq!(value(POSITION_KEY).len(), 24);
        assert_eq!((bind.parent_id, bind.parent_attach_id, bind.flags), (5, ZONE_ATTACHMENT, 0x1400));
        assert_eq!(bind.components, [OBJECT_CONTAINER, crate::location::OC_PHYSICS_CONTROLLER]);
        assert!(bind.snapshots.iter().all(|snapshot| snapshot.key != 0xe359_14ad), "no ocIdRange: the server binds the cab's entities");
    }

    #[test]
    fn a_door_rebound_open_carries_its_state_once() {
        let bind = EntityBind { entity_id: 7, ..EntityBind::default() };
        let open = door_rebind(&door_rebind(&bind, false), true);
        let states: Vec<&[u8]> = open.snapshots.iter().filter(|snapshot| snapshot.key == DOOR_OPEN_STATE_KEY).map(|snapshot| snapshot.value.as_slice()).collect();
        assert_eq!(states, [[DOOR_OPENING]]);
        assert_eq!(door(7, true), None, "no index read yet");
    }
}
