//! The player entity, which the client needs bound to pass `eCVS_WaitForAsyncSpawns(16)`.
//!
//! The client already knows its player id: it is the character GEID from login, held by the view before
//! any shard message, and `SetLocalPlayerId` copies it to `CGame+0xc18`. The shard has to bind an entity with
//! exactly that id. `CSCPlayer` then compares its own id with `CGame+0xc18` (0x142ed1f90) and, on a match,
//! registers itself as the client actor, which is what `WaitClientActor` waits for. The game channel's
//! player id is separate, see `network_node_id_snapshot`.

use crate::entity::{Bind, Snapshot, Transform};

/// The `Player` record of the game data.
pub const PLAYER_CLASS_CRC: u32 = 2_961_494_058;
/// `ENTITY_FLAG_PLAYER`. `CSCPlayer::Initialize` raises a fatal error on a `Player` without it.
pub const ENTITY_FLAG_PLAYER: u32 = 0x200;
/// Set on an entity that only exists locally. A network component on such an entity is a fatal error.
pub const ENTITY_FLAG_LOCAL_ONLY: u32 = 0x10;
/// As a real shard sends the player. Only 0x200 and 0x1000 are understood, so the rest is kept as seen.
pub const PLAYER_FLAGS: u32 = 0x84a0_1200;
/// The attachment most real joins log. Its name, and what it needs of the parent, are unknown.
pub const PLAYER_ATTACH_ID: u32 = 0x26b2_bbf1;
/// Ids from 1 to this are local to the client, and a network component there is a fatal error.
pub const LAST_LOCAL_ID: u64 = 0x2e_90cf_4b7f;

const FIRST_ACCOUNT_ID: u64 = 1_000_000;
const FIRST_GEID: u64 = 300_000_000_000;

/// CRC-32 of the lowercased `SCActorStateDataSynced::SState::m_state.m_baseData`.
const ACTOR_STATE_KEY: u32 = 0xc3f3_88bf;
/// `SStateContainerBaseData<ESCActorResultState>`. The client skips a variable whose type differs, silently.
const ACTOR_STATE_TYPE: u32 = 0xafab_c86c;
/// CRC-32 of the lowercased `SCActorStateDataSynced::SState::m_stanceState.m_baseData`.
const ACTOR_STANCE_KEY: u32 = 0xf765_92ee;
/// `SStateContainerBaseData<ESCActorResultStanceState>`.
const ACTOR_STANCE_TYPE: u32 = 0xf076_c008;
/// `ESCActorResultState::Ground`.
const STATE_GROUND: u8 = 8;
/// `ESCActorResultStanceState::Stand`.
const STANCE_STAND: u8 = 1;
/// CRC-32 of the lowercased `CSCPlayerState::m_bIsPersistentSpawning`.
const PERSISTENT_SPAWNING_KEY: u32 = 0x4414_508c;
/// The halves of FNV-1a 64 of `bool`, xored (0x14035af50).
const BOOL_TYPE: u32 = 0x0b9f_c026;

/// CRC-32 of the lowercased `SCActorStateDataSynced::SMovement::m_entityPosition`.
const ENTITY_POSITION_KEY: u32 = 0x7cf8_dd7b;
/// `Vec3_tpl<double>`: three `f64`, as 0x140618d90 reads them. Read from the running client (0x149e3b484).
const VEC3_DOUBLE_TYPE: u32 = 0xd6b6_7a95;
/// CRC-32 of the lowercased `SCActorStateDataSynced::SMovement::m_baseRotation`.
const BASE_ROTATION_KEY: u32 = 0x4c41_ab53;
/// `Quat_tpl<float>`: `x y z w` as four `f32`, as 0x140618bd0 and 0x140616f30 read them. Read from the running client
/// (0x149e3b488).
const QUAT_FLOAT_TYPE: u32 = 0x2ec4_ac23;
/// CRC-32 of the lowercased `SCActorStateDataSynced::SMovementAux::m_zoneHostEntityId`.
const ZONE_HOST_KEY: u32 = 0xf7bc_27cb;
/// CRC-32 of the lowercased `SCActorStateDataSynced::SMovementAux::m_parentEntityId`.
const PARENT_ENTITY_KEY: u32 = 0x2f3f_d8e1;
/// `EntityId`, one `u64` (0x140617300), the type `ssQuantumGridId` also has. Read from the running client (0x149e30c40).
const ENTITY_ID_TYPE: u32 = 0x728a_e36a;

/// CRC-32 of the lowercased `scplayer.rsiBadges`: the player's badges, which the client builds its account badge mask
/// from in `CSCPlayer::Initialize` (0x142eca720), the same ids the login reply carries (QA 0x67 and so on).
const RSI_BADGES_KEY: u32 = 0xfc2f_88a6;
/// A sequence of `int16`: FNV-1a 64 of `Sequence` then `int16`, halves xored (0x142a41df0). Its value is a varint count,
/// then each id as a little-endian `u16` (0x142f5c7c0).
const INT16_SEQUENCE_TYPE: u32 = 0x62bd_6940;

/// The player's in-game badges, for its bind.
pub fn badges_snapshot(badges: &[u32]) -> Snapshot {
    let mut value = Vec::new();
    let mut count = badges.len();
    while count >= 0x80 {
        value.push(count as u8 | 0x80);
        count >>= 7;
    }
    value.push(count as u8);
    value.extend(badges.iter().flat_map(|badge| (*badge as u16).to_le_bytes()));
    Snapshot { key: RSI_BADGES_KEY, type_id: INT16_SEQUENCE_TYPE, value }
}

/// CRC-32 of `networknodeid`.
const NETWORK_NODE_ID_KEY: u32 = 0xd8c4_66e5;
/// `TNetworkNodeId`, a struct of `m_val`, a `CryGUID` of `hipart` and `lopart` (0x1414d8840).
const NETWORK_NODE_ID_TYPE: u32 = 0xe7a5_117f;

/// CRC-32 of `currentlocationid`, the player's `currentLocationId`, `CSCPlayer+0xd08`. The debug overlay's `Current
/// player location` is that field: `CGame` slots `+0x90` (0x142e72d70) and `+0x98` (0x142e72e60) read it from the local
/// player's `SCPlayer` and name it through the solar system. Nothing else the shard binds reaches it.
const CURRENT_LOCATION_ID_KEY: u32 = 0x95e9_1c7e;
/// The halves of FNV-1a 64 of `uint32`, xored (0x14035a9f0), which the variable's holder (vtable 0x147ed1bd8) reports.
const UINT32_TYPE: u32 = 0x5d4e_4bea;

/// Accounts, characters and hangars are issued in step, so this is what tells them apart. `None` for an account from
/// before the first character.
pub fn player_index(account: u64) -> Option<u64> {
    account.checked_sub(FIRST_ACCOUNT_ID)
}

/// The character of an account.
pub fn player_geid(account: u64) -> Option<u64> {
    Some(FIRST_GEID + player_index(account)?)
}

/// `[u8 state][i64 m_time]`, as 0x142a29550 writes it. Spawning servers leave the time at 0.
fn state_value(state: u8) -> Vec<u8> {
    let mut value = vec![state];
    value.extend_from_slice(&0i64.to_le_bytes());
    value
}

/// What 0x142af00e0 sets on a spawning player. The local actor's control machine starts in `Staging`, which
/// `WaitForGameRulesSpawningModule` waits on. `EnableControl` leaves it for `Ground` only with persistent
/// spawning set. The normal exit needs a spawn point, a body and the server's revive.
pub fn actor_state_snapshots() -> Vec<Snapshot> {
    vec![
        Snapshot { key: PERSISTENT_SPAWNING_KEY, type_id: BOOL_TYPE, value: vec![1] },
        Snapshot { key: ACTOR_STATE_KEY, type_id: ACTOR_STATE_TYPE, value: state_value(STATE_GROUND) },
        Snapshot { key: ACTOR_STANCE_KEY, type_id: ACTOR_STANCE_TYPE, value: state_value(STANCE_STAND) },
    ]
}

/// The server's movement state of the actor: where it is, in the frame of its zone host. Without it the state is the
/// origin with no zone host, and `SCActorStateData::SMovement::Validate` (0x146033a20) resets the client's actor to that
/// ("Position mismatch ... resetting to latest server position", teleport 0x145b6d8f0): a zone host of 0 is no zone,
/// so the player lands at 0, 0, 0 of the solar system as soon as its actor updates, and again every few frames.
/// The placement's parent is the zone host: the hangar, with the zone attachment, or the system as fallback.
pub fn movement_snapshots(placement: &Placement) -> Vec<Snapshot> {
    let position = placement.transform.position.iter().flat_map(|axis| axis.to_le_bytes()).collect();
    let rotation = placement.transform.rotation.iter().flat_map(|part| (*part as f32).to_le_bytes()).collect();
    let id = placement.parent_id.to_le_bytes().to_vec();
    vec![
        Snapshot { key: ENTITY_POSITION_KEY, type_id: VEC3_DOUBLE_TYPE, value: position },
        Snapshot { key: BASE_ROTATION_KEY, type_id: QUAT_FLOAT_TYPE, value: rotation },
        Snapshot { key: ZONE_HOST_KEY, type_id: ENTITY_ID_TYPE, value: id.clone() },
        Snapshot { key: PARENT_ENTITY_KEY, type_id: ENTITY_ID_TYPE, value: id },
    ]
}

/// A player's owner, which spawning servers write beside the variables above (0x142ae36d0). The client's own
/// node id is `{0, account id}`, set by `SetLocalClientNetworkNodeId`. The only writer of the game channel's
/// player id (0x142d2b290) ignores a zero node id and any other node's, and `InitActionMap.ClientPlayer` and
/// `InitView.ClientPlayer` read that id.
pub fn network_node_id_snapshot(account: u64) -> Snapshot {
    let value = [0u64.to_le_bytes(), account.to_le_bytes()].concat();
    Snapshot { key: NETWORK_NODE_ID_KEY, type_id: NETWORK_NODE_ID_TYPE, value }
}

/// Where the player is, as a `LocationId`: a server keeps it, and the client only shows it. Four bytes, as the holder
/// writes them (0x140402850).
pub fn current_location_snapshot(location_id: u32) -> Snapshot {
    Snapshot { key: CURRENT_LOCATION_ID_KEY, type_id: UINT32_TYPE, value: location_id.to_le_bytes().to_vec() }
}

/// The fields of a `BindBatchAddBind` that differ from an entity at the origin. They match
/// `channel::Bind` one for one, but for the snapshots, which are `actor_state_snapshots`.
#[derive(Debug, PartialEq)]
pub struct PlayerBind {
    pub entity_id: u64,
    pub class_crc: u32,
    pub flags: u32,
    pub parent_id: u64,
    pub parent_attach_id: u32,
    pub transform: Transform,
}

/// Where a player goes: its parent, the attachment to it and its place there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub parent_id: u64,
    pub attach_id: u32,
    pub transform: Transform,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// The client would stop with a fatal error, or refuse the entity at spawn.
    IdInLocalScope,
    /// Only the universe root may arrive without a bound parent.
    NoParent,
}

/// The one entity the wait tasks need. `geid` must be the id the client logs as `playerGEID`, and the parent
/// an entity already bound, such as the player's hangar.
pub fn player_bind(geid: u64, placement: Placement) -> Result<PlayerBind, SpawnError> {
    if geid <= LAST_LOCAL_ID {
        return Err(SpawnError::IdInLocalScope);
    }
    if placement.parent_id == 0 {
        return Err(SpawnError::NoParent);
    }
    Ok(PlayerBind {
        entity_id: geid,
        class_crc: PLAYER_CLASS_CRC,
        flags: PLAYER_FLAGS,
        parent_id: placement.parent_id,
        parent_attach_id: placement.attach_id,
        transform: placement.transform,
    })
}

/// The whole bind of a player: its entity, the variables the client needs to take control of it, and its location,
/// `None` for a player with none, such as one waiting at the origin of a system.
pub fn player_record(geid: u64, account: u64, placement: Placement, location_id: Option<u32>) -> Result<Bind, SpawnError> {
    let player = player_bind(geid, placement)?;
    let mut snapshots = actor_state_snapshots();
    snapshots.push(network_node_id_snapshot(account));
    snapshots.extend(movement_snapshots(&placement));
    snapshots.extend(location_id.map(current_location_snapshot));
    Ok(Bind {
        entity_id: player.entity_id,
        class_crc: player.class_crc,
        flags: player.flags,
        transform: player.transform,
        parent_id: player.parent_id,
        parent_attach_id: player.parent_attach_id,
        components: Vec::new(),
        snapshots,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn badges_go_as_a_counted_sequence_of_int16() {
        let crc = !b"scplayer.rsibadges".iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        });
        let snapshot = badges_snapshot(&[0x67, 0x6b]);
        assert_eq!((snapshot.key, snapshot.type_id), (crc, 0x62bd_6940));
        assert_eq!(snapshot.value, [2, 0x67, 0, 0x6b, 0]);
    }

    use super::*;

    const GEID: u64 = 300_000_000_001;
    const PARENT: Placement = Placement { parent_id: 862_500_987_682, attach_id: PLAYER_ATTACH_ID, transform: Transform::IDENTITY };

    #[test]
    fn the_flags_are_those_of_a_real_session() {
        assert_eq!(PLAYER_FLAGS, 2_225_082_880);
        assert_eq!(PLAYER_ATTACH_ID, 649_247_729);
    }

    #[test]
    fn the_player_keeps_the_flags_the_client_checks() {
        let bind = player_bind(GEID, PARENT).unwrap();
        assert_eq!(bind.flags & ENTITY_FLAG_PLAYER, ENTITY_FLAG_PLAYER);
        assert_eq!(bind.flags & 0x1000, 0x1000);
        assert_eq!(bind.flags & ENTITY_FLAG_LOCAL_ONLY, 0);
    }

    #[test]
    fn the_entity_id_is_the_id_the_client_waits_on() {
        let bind = player_bind(GEID, PARENT).unwrap();
        assert_eq!((bind.entity_id, bind.class_crc, bind.parent_id), (GEID, 2_961_494_058, PARENT.parent_id));
    }

    #[test]
    fn ids_in_the_local_scope_are_refused() {
        assert_eq!(player_bind(LAST_LOCAL_ID, PARENT), Err(SpawnError::IdInLocalScope));
        assert_eq!(player_bind(0, PARENT), Err(SpawnError::IdInLocalScope));
        assert!(player_bind(LAST_LOCAL_ID + 1, PARENT).is_ok());
    }

    #[test]
    fn the_actor_starts_grounded_and_standing() {
        let snapshots = actor_state_snapshots();
        let fields: Vec<_> = snapshots.iter().map(|snapshot| (snapshot.key, snapshot.type_id)).collect();
        assert_eq!(fields, [(0x4414_508c, 0x0b9f_c026), (0xc3f3_88bf, 0xafab_c86c), (0xf765_92ee, 0xf076_c008)]);
        assert_eq!(snapshots[0].value, [1]);
        assert_eq!(snapshots[1].value, [8, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(snapshots[2].value, [1, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn the_node_id_is_keyed_and_typed_as_the_client_computes_it() {
        let snapshot = network_node_id_snapshot(1);
        let crc = !b"networknodeid".iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        });
        assert_eq!(snapshot.key, crc);
        let names = ["Struct", "TNetworkNodeId", "m_val", "Struct", "CryGUID", "hipart", "uint64", "lopart", "uint64"];
        let fnv = names.concat().bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3));
        assert_eq!(snapshot.type_id, (fnv >> 32) as u32 ^ fnv as u32);
    }

    #[test]
    fn the_node_id_is_zero_then_the_account_as_real_logs_print_it() {
        let value = network_node_id_snapshot(0x004f_9844).value;
        assert_eq!(value, [0, 0, 0, 0, 0, 0, 0, 0, 0x44, 0x98, 0x4f, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn the_player_takes_its_place_from_the_placement() {
        let transform = Transform { position: [1.0, 2.0, 3.0], ..Transform::IDENTITY };
        let bind = player_bind(GEID, Placement { attach_id: 0x46c0_1eb2, transform, ..PARENT }).unwrap();
        assert_eq!((bind.parent_attach_id, bind.transform), (0x46c0_1eb2, transform));
    }

    #[test]
    fn the_character_comes_from_the_account() {
        assert_eq!(player_geid(1_000_001), Some(300_000_000_001));
        assert_eq!(player_geid(5), None);
    }

    fn crc32(text: &str) -> u32 {
        !text.to_ascii_lowercase().bytes().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        })
    }

    #[test]
    fn the_movement_is_keyed_as_the_client_computes_it() {
        let names = [
            "SCActorStateDataSynced::SMovement::m_entityPosition",
            "SCActorStateDataSynced::SMovement::m_baseRotation",
            "SCActorStateDataSynced::SMovementAux::m_zoneHostEntityId",
            "SCActorStateDataSynced::SMovementAux::m_parentEntityId",
        ];
        let keys: Vec<u32> = movement_snapshots(&PARENT).iter().map(|snapshot| snapshot.key).collect();
        assert_eq!(keys, names.map(crc32));
        assert_eq!(PERSISTENT_SPAWNING_KEY, crc32("CSCPlayerState::m_bIsPersistentSpawning"));
    }

    #[test]
    fn the_movement_holds_the_spawn_point_in_the_hangar() {
        let transform = Transform { position: [5.9, -85.14, 4.51], rotation: [0.0, 0.0, 0.5, 0.5] };
        let placement = Placement { transform, ..PARENT };
        let snapshots = movement_snapshots(&placement);
        let types: Vec<u32> = snapshots.iter().map(|snapshot| snapshot.type_id).collect();
        assert_eq!(types, [0xd6b6_7a95, 0x2ec4_ac23, 0x728a_e36a, 0x728a_e36a]);
        let position: Vec<f64> = snapshots[0].value.chunks(8).map(|bytes| f64::from_le_bytes(bytes.try_into().unwrap())).collect();
        assert_eq!(position, [5.9, -85.14, 4.51]);
        let rotation: Vec<f32> = snapshots[1].value.chunks(4).map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap())).collect();
        assert_eq!(rotation, [0.0, 0.0, 0.5, 0.5]);
        assert_eq!(snapshots[2].value, PARENT.parent_id.to_le_bytes());
        assert_eq!(snapshots[3].value, PARENT.parent_id.to_le_bytes());
    }

    #[test]
    fn the_player_record_carries_the_movement_after_the_rest() {
        let record = player_record(GEID, 1_000_001, PARENT, None).unwrap();
        let keys: Vec<u32> = record.snapshots.iter().map(|snapshot| snapshot.key).collect();
        assert_eq!(keys, [0x4414_508c, 0xc3f3_88bf, 0xf765_92ee, 0xd8c4_66e5, 0x7cf8_dd7b, 0x4c41_ab53, 0xf7bc_27cb, 0x2f3f_d8e1]);
    }

    #[test]
    fn the_player_record_ends_with_its_location() {
        let record = player_record(GEID, 1_000_001, PARENT, Some(848_868_938)).unwrap();
        let location = record.snapshots.last().unwrap();
        assert_eq!((location.key, location.type_id), (crc32("currentLocationId"), 0x5d4e_4bea));
        assert_eq!(location.value, 848_868_938u32.to_le_bytes());
        assert_eq!(record.snapshots.len(), 9);
    }

    #[test]
    fn a_player_needs_a_parent() {
        assert_eq!(player_bind(GEID, Placement { parent_id: 0, ..PARENT }), Err(SpawnError::NoParent));
    }

}
