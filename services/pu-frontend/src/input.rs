//! A player's movement on the wire: the client's input frames in, the server's movement state out. The replicant decodes
//! and encodes; the DGS that holds the player decides where it is. See "Player movement" in the protocol notes (kept outside this repository).

use sc_api::internal::{ClientPosition, InputFrame, PlayerMovement, Rotation};

/// `EntityRemoteMethod`, `[u64 entity][varint method][varint length][length bytes]` from a client.
pub const MESSAGE_ENTITY_REMOTE_METHOD: usize = 18;
/// `EntitySnapshotUpdate`, read by 0x1415ad7c0.
const MESSAGE_ENTITY_SNAPSHOT_UPDATE: u8 = 16;
/// The remote method of the player that carries `CSCActorActionHandler`'s action frames.
const ACTION_FRAME_METHOD: usize = 7;

// Action types are the byte each `CSCActorActionHandler::Request<SCActorActionHandlerActions::...>` stores at `+0x18` of
// the action (`mov byte [reg+0x18], type`), read from all 226 of those functions. The wire gives no length per action.

/// `SDeltaRotation`, written by 0x1462710f0: `m_originAimRotation` (a quaternion, the aim at the start of the frame), then
/// the delta, start and end velocities of the aim and of the look offset (seven angle triples), then the zone's entity id,
/// 108 bytes. Field order from the RE repo's `VisitPayload` (`SCActorActionHandlerSerialization.cpp`).
const ACTION_DELTA_ROTATION: u8 = 0x36;
/// `SClientAuthoritativePosition`, sent only while the player's `m_clientAuthoritative` is set: `m_originZoneHostId`
/// (u64), `m_originPosition` (3 f64), `m_deltaPosition`, `m_startVelocity`, `m_endVelocity` (3 f32 each),
/// `m_originBaseRotation` (quaternion), `m_deltaBaseRotation` (axis times angle), `m_startBaseVelocity`,
/// `m_endBaseVelocity` (3 f32 each), `planarSpeed` (f32), `flag7c` (a bool, a byte), `m_relativeEntityId` (u64),
/// `m_relativeOriginPosition` (3 f64), then three more 3 f32: 193 bytes, by the RE repo's `VisitPayload`. Not seen yet:
/// a wrong length fails `chain`, and the frame falls back to the scan.
const ACTION_CLIENT_POSITION: u8 = 0x32;
const CLIENT_POSITION_LEN: usize = 193;
const DELTA_ROTATION_LEN: usize = 108;
/// `SMoveButtonMask`, written by 0x146272560: a `u32`, in every frame while a button is held.
const ACTION_MOVE_BUTTON_MASK: u8 = 0x58;
/// `SSprint(bool)`: one byte, bit 3 set while Shift is held (seen; bit 0 was set in every one).
const ACTION_SPRINT: u8 = 0x63;
const SPRINT_HELD: u8 = 0x08;
/// `SStance`, one byte: 1 stand, 2 crouch.
const ACTION_STANCE: u8 = 0x64;
/// `SJump`, no data.
const ACTION_JUMP_START: u8 = 0x9c;
/// `SJumpHold` (written by 0x1462a7010), no data.
const ACTION_JUMP_HOLD: u8 = 0x9d;
/// `SJumpRelease` (0x1462a7160), no data.
const ACTION_JUMP_RELEASE: u8 = 0x9e;

/// The length of an action's data after its type, where it is known: the three above from their writers, the lengths seen
/// on a running client between known actions (all of them actions of one `bool` or one enum: `SAbilityInteractionModeToggle`
/// 0x15, `SFreeLook` 0x49, `SFullBodyIdle` 0x4a, `SHoldBreath` 0x4c, `SStance` 0x64, `SThirdPerson` 0x6f, `SScanning`
/// 0x8f, `SFire` 0x45 with two enums in 2 bytes, `SBodyCarryDropStart` 0x94 and `SZoom` 0xac with none), and by that rule the other one-`bool` or one-enum actions and those with no arguments. A guess that is wrong
/// cannot pass `chain`, which needs a frame to end exactly where its actions do.
fn action_len(action: u8) -> Option<usize> {
    match action {
        ACTION_DELTA_ROTATION => Some(DELTA_ROTATION_LEN),
        ACTION_CLIENT_POSITION => Some(CLIENT_POSITION_LEN),
        ACTION_MOVE_BUTTON_MASK => Some(4),
        ACTION_SPRINT | ACTION_STANCE => Some(1),
        // Seen.
        0x15 | 0x49 | 0x4a | 0x4c | 0x6f | 0x8f => Some(1),
        // Seen: `SFire` (two enums) as `45 01 00` alone in a frame, `SBodyCarryDropStart` and `SZoom` as `94 ac`, two
        // actions in a frame of 2 bytes.
        0x45 => Some(2),
        0x94 => Some(0),
        // One bool or enum, by name: SLean, SWalk, SDodge, SProneRoll, SUnarmedCombat, SKioskSoftLock, SEVABrake,
        // SEVABoost, SWeaponZeroing, SBodyDragDrop, SInspect, SMobiGlas.
        0x54 | 0x7f | 0x38 | 0x5f | 0x5a | 0x51 | 0x3e | 0x3d | 0x81 | 0x30 | 0x50 | 0x56 => Some(1),
        // No arguments: SJump, SJumpHold, SJumpRelease, SZoom, SZoomIn, SZoomOut, SZoomStarted, SZoomFinished, SVisorWipe,
        // SFinishInteraction, SSelfTarget, SInspectFlip.
        ACTION_JUMP_START | ACTION_JUMP_HOLD | ACTION_JUMP_RELEASE => Some(0),
        0xac | 0xaf | 0xb0 | 0xad | 0xae | 0xb1 | 0x99 | 0xa3 | 0x9a => Some(0),
        // `SFixedSpeedIncrementDecrement(int)` (0x47, the mouse wheel's speed step) is left out: no frame of it has been
        // seen to tell its length, so `frame_actions` scans past it.
        _ => None,
    }
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*offset)?;
        *offset += 1;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

fn f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn f64_at(bytes: &[u8], offset: usize) -> f64 {
    f64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// `a` then `b`, Hamilton products of `x y z w`.
fn compose([ax, ay, az, aw]: [f64; 4], [bx, by, bz, bw]: [f64; 4]) -> [f64; 4] {
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// The end of a `SClientAuthoritativePosition`: the origin moved by the delta, and the origin rotation turned by the
/// delta's axis times angle.
fn client_position(data: &[u8]) -> ClientPosition {
    let origin = [f64_at(data, 8), f64_at(data, 16), f64_at(data, 24)];
    let delta = [0, 1, 2].map(|axis| f64::from(f32_at(data, 32 + axis * 4)));
    let base = [0, 1, 2, 3].map(|part| f64::from(f32_at(data, 68 + part * 4)));
    let turn = [0, 1, 2].map(|axis| f64::from(f32_at(data, 84 + axis * 4)));
    let angle = (turn[0] * turn[0] + turn[1] * turn[1] + turn[2] * turn[2]).sqrt();
    let end = if angle > 0.0 {
        let (sin, cos) = (angle / 2.0).sin_cos();
        compose([turn[0] / angle * sin, turn[1] / angle * sin, turn[2] / angle * sin, cos], base)
    } else {
        base
    };
    let [x, y, z, w] = end.map(|part| part as f32);
    ClientPosition {
        zone_host_id: u64_at(data, 0).unwrap_or(0),
        x: origin[0] + delta[0],
        y: origin[1] + delta[1],
        z: origin[2] + delta[2],
        base_rotation: Some(Rotation { x, y, z, w }),
    }
}

/// One action frame of a player.
#[derive(Debug, PartialEq)]
pub struct Decoded {
    pub entity: u64,
    pub frame: InputFrame,
    /// The first action type of unknown length, when the frame had one.
    pub unknown: Option<u8>,
}

/// The actions of `bytes`, if they are exactly `count` actions of known length or, with `count` `None`, any number of
/// them, ending exactly at the end.
fn chain(bytes: &[u8], count: Option<usize>) -> Option<Vec<(u8, &[u8])>> {
    let mut actions = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let action = bytes[at];
        let len = action_len(action)?;
        actions.push((action, bytes.get(at + 1..at + 1 + len)?));
        at += 1 + len;
    }
    count.is_none_or(|count| count == actions.len()).then_some(actions)
}

/// Whether an action found by scanning holds a value it could hold, so that a stray byte is not taken for it.
fn plausible(action: u8, data: &[u8]) -> bool {
    match action {
        ACTION_MOVE_BUTTON_MASK => u32::from_le_bytes(data.try_into().unwrap()) < 0x100,
        ACTION_DELTA_ROTATION => {
            let norm: f32 = (0..4).map(|part| f32_at(data, part * 4).powi(2)).sum();
            (norm - 1.0).abs() < 1e-3 && u64_at(data, 100).is_some_and(|zone| zone != 0)
        }
        _ => false,
    }
}

/// The actions of a frame: all of them when every type's length is known. Otherwise those before the first unknown type,
/// and a tail found by scanning past it for a button mask or a turn whose value is plausible and after which the rest
/// of the frame reads exactly to its end. A false find needs a byte `0x58` with three zero bytes after a small value, or
/// a unit quaternion, followed by bytes that read as known actions to the very end, so it is unlikely; what lay between
/// is lost either way.
fn frame_actions(bytes: &[u8], count: usize) -> (Vec<(u8, &[u8])>, Option<u8>) {
    if let Some(actions) = chain(bytes, Some(count)) {
        return (actions, None);
    }
    let mut actions = Vec::new();
    let mut at = 0;
    while let Some(len) = bytes.get(at).and_then(|action| action_len(*action)) {
        let Some(data) = bytes.get(at + 1..at + 1 + len) else { break };
        actions.push((bytes[at], data));
        at += 1 + len;
    }
    let unknown = bytes.get(at).copied();
    for start in at + 1..bytes.len() {
        let action = bytes[start];
        if action != ACTION_MOVE_BUTTON_MASK && action != ACTION_DELTA_ROTATION {
            continue;
        }
        let Some(tail) = chain(&bytes[start..], None) else { continue };
        if tail.first().is_some_and(|(action, data)| plausible(*action, data)) && actions.len() + tail.len() < count {
            actions.extend(tail);
            break;
        }
    }
    (actions, unknown)
}

/// The frame layout, after the method's length:
/// `[u64 time][u8 1][u64 entity][u64 time][8 bytes][u8 actions][u64 entity][actions][u64 time]`. Every frame seen has the
/// same eight bytes, `14 16 05 00 00 00 00 00`, which are not read. An action is `[u8 type][data]`.
pub fn action_frame(payload: &[u8]) -> Option<Decoded> {
    let entity = u64_at(payload, 0)?;
    let mut offset = 8;
    if read_varint(payload, &mut offset)? != ACTION_FRAME_METHOD {
        return None;
    }
    let length = read_varint(payload, &mut offset)?;
    let blob = payload.get(offset..)?;
    if blob.len() != length || length < 50 || blob[8] != 1 || u64_at(blob, 9)? != entity || u64_at(blob, 34)? != entity {
        return None;
    }
    let count = usize::from(blob[33]);
    let (actions, unknown) = frame_actions(&blob[42..blob.len() - 8], count);
    let mut frame = InputFrame { time: u64_at(blob, blob.len() - 8)?, partial: unknown.is_some(), ..InputFrame::default() };
    for (action, data) in actions {
        match action {
            ACTION_MOVE_BUTTON_MASK => frame.move_buttons = u32::from_le_bytes(data.try_into().ok()?),
            ACTION_SPRINT => frame.sprint = data[0] & SPRINT_HELD != 0,
            ACTION_STANCE => frame.stance = Some(u32::from(data[0])),
            ACTION_JUMP_START => frame.jump |= 1,
            ACTION_JUMP_HOLD => frame.jump |= 2,
            ACTION_JUMP_RELEASE => frame.jump |= 4,
            ACTION_DELTA_ROTATION => {
                frame.aim_rotation = Some(Rotation { x: f32_at(data, 0), y: f32_at(data, 4), z: f32_at(data, 8), w: f32_at(data, 12) });
            }
            ACTION_CLIENT_POSITION => frame.client_position = Some(client_position(data)),
            _ => {}
        }
    }
    // A partial frame whose mask was found is whole as far as the buttons go.
    if frame.move_buttons != 0 {
        frame.partial = false;
    }
    Some(Decoded { entity, frame, unknown })
}

/// Indexes of the player's variables in its `CSerializedComponent` (`+0x68`, 0x50 bytes each), which an update names.
/// Read from the variable table of a running client's player, twice, across a respawn: they are the position of the
/// variable among the 1076 of the `Player` class of this build, so another build or class needs them read again. The
/// client checks each against the entity's variable count only, and a wrong one writes another variable.
mod index {
    /// `SCActorStateDataSynced::SMovement::m_entityPosition`, `0x7cf8dd7b`.
    pub const ENTITY_POSITION: usize = 502;
    /// `SCActorStateDataSynced::SMovement::m_baseRotation`, `0x4c41ab53`.
    pub const BASE_ROTATION: usize = 303;
    /// `SCActorStateDataSynced::SMovement::m_aimRotation`, `0x2ae4ed77`, the same type as the base rotation.
    pub const AIM_ROTATION: usize = 176;
    /// `SCActorStateDataSynced::SMovementAux::m_zoneHostEntityId`, `0xf7bc27cb`.
    pub const ZONE_HOST: usize = 1009;
    /// `SCActorStateDataSynced::SMovementAux::m_parentEntityId`, `0x2f3fd8e1`.
    pub const PARENT: usize = 193;
    /// `SCActorStateDataSynced::m_actionFrameEndTimestamp.m_baseTimestamp`, `0xf45869ca`: a `u64` (the serializer of an
    /// `EntityId`), at `+0x30` of the synced data.
    pub const ACTION_FRAME_END_BASE: usize = 996;
    /// `SCActorStateDataSynced::m_actionFrameEndTimestamp.m_timestampOffset`, `0xe1442f4e`: a `u32` (0x140402850 writes
    /// 32 bits), at `+0x40`. The time is the base plus the offset (0x145848510).
    pub const ACTION_FRAME_END_OFFSET: usize = 908;
    /// `CSCActorActionHandler::m_lagCompensation.m_clientAuthoritative`, `0xe51161d2`: a `u32` (0x140402850), the flags
    /// the actor's movement mode follows (actor `+0x478`, read by 0x145bd0800).
    pub const CLIENT_AUTHORITATIVE: usize = 927;
}

/// The flag a real server always sets in `m_clientAuthoritative` (`UpdateClientAuthoritativeState`, in the RE repo's
/// `SCActorActionHandlerComponent.cpp`). Any value but 0 and 8 makes 0x145bd0800 answer 2, so `SMovement::Validate` does
/// not run on the client, which moves its actor itself and reports where in each frame (`SClientAuthoritativePosition`).
const CLIENT_AUTHORITATIVE_MOVEMENT: u32 = 0x10;

/// `EntitySnapshotUpdate` giving the player its own movement: `m_clientAuthoritative` set, once, when it is in game.
pub fn client_authority(player: u64) -> Vec<u8> {
    let mut payload = player.to_le_bytes().to_vec();
    payload.extend_from_slice(&SNAPSHOT_VARIABLES.to_le_bytes());
    varint(index::CLIENT_AUTHORITATIVE + 1, &mut payload);
    varint(4, &mut payload);
    payload.extend_from_slice(&CLIENT_AUTHORITATIVE_MOVEMENT.to_le_bytes());
    payload.push(0);
    let mut message = Vec::new();
    crate::channel::message(MESSAGE_ENTITY_SNAPSHOT_UPDATE, &payload, &mut message);
    message
}

/// The `Player` class the indexes were read for. The table is the class's: the same 1076 variables, in the same order,
/// with and without 23 loadout items in the join (read live both ways), so items are not part of it. Components added to
/// the bind would be.
const INDEXED_CLASS: u32 = 2_961_494_058;

/// Whether `index` holds for a player bound so. Any other bind gets no movement update, as a wrong index is fatal to the
/// client.
pub fn indexes_hold(player: &sc_api::internal::EntityBind) -> bool {
    player.class_crc == INDEXED_CLASS && player.components.is_empty()
}

/// The mask bit of an update that carries variables. No other field of the entity changes.
const SNAPSHOT_VARIABLES: u16 = 0x800;

fn varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn rotation_bytes(rotation: Option<&Rotation>) -> Vec<u8> {
    let Rotation { x, y, z, w } = rotation.copied().unwrap_or(Rotation { x: 0.0, y: 0.0, z: 0.0, w: 1.0 });
    [x, y, z, w].iter().flat_map(|part| part.to_le_bytes()).collect()
}

/// `EntitySnapshotUpdate` with the movement variables that differ from `sent`, the state the client last got, or all of
/// them with none: `[u64 entity][u16 0x800]`, then `[varint index + 1][varint size][value]` each, then varint 0. Values are
/// encoded as in a bind (`player::movement_snapshots` of the DGS). `None` when nothing differs.
///
/// The frame the state is of goes with any other variable, and alone only with `refresh`: a client standing still sends a
/// frame every 33 ms, and naming each of them would be an update every frame for nothing. A `Validate` that runs compares
/// the state with where the client was at the frame named, and a client that stood still since then was where it is now.
pub fn movement_update(movement: &PlayerMovement, sent: Option<&PlayerMovement>, refresh: bool) -> Option<Vec<u8>> {
    let mut variables: Vec<(usize, Vec<u8>)> = Vec::new();
    let position = |state: &PlayerMovement| [state.x, state.y, state.z];
    if sent.is_none_or(|sent| position(sent) != position(movement)) {
        variables.push((index::ENTITY_POSITION, position(movement).iter().flat_map(|axis| axis.to_le_bytes()).collect()));
    }
    if sent.is_none_or(|sent| sent.base_rotation != movement.base_rotation) {
        variables.push((index::BASE_ROTATION, rotation_bytes(movement.base_rotation.as_ref())));
    }
    if sent.is_none_or(|sent| sent.aim_rotation != movement.aim_rotation) {
        variables.push((index::AIM_ROTATION, rotation_bytes(movement.aim_rotation.as_ref())));
    }
    if sent.is_none_or(|sent| sent.zone_host_id != movement.zone_host_id) {
        variables.push((index::ZONE_HOST, movement.zone_host_id.to_le_bytes().to_vec()));
    }
    if sent.is_none_or(|sent| sent.parent_id != movement.parent_id) {
        variables.push((index::PARENT, movement.parent_id.to_le_bytes().to_vec()));
    }
    // Which of the client's own frames this state is: `Validate` then compares it with where the client was at the end of
    // that frame, not with where it is now, so the time the update takes to arrive does not count.
    let frame_changed = movement.frame_time != 0 && sent.is_none_or(|sent| sent.frame_time != movement.frame_time);
    if frame_changed && (refresh || !variables.is_empty()) {
        variables.push((index::ACTION_FRAME_END_BASE, movement.frame_time.to_le_bytes().to_vec()));
        variables.push((index::ACTION_FRAME_END_OFFSET, 0u32.to_le_bytes().to_vec()));
    }
    if variables.is_empty() {
        return None;
    }
    let mut payload = movement.entity_id.to_le_bytes().to_vec();
    payload.extend_from_slice(&SNAPSHOT_VARIABLES.to_le_bytes());
    // The reader (0x1415985d0) adds each number to the index before it, starting from -1: the first is the index + 1, every
    // later one the step from the previous index. So the variables go in rising order. Sending a later index + 1 instead
    // put the base rotation (303) on index 502 + 304 = 806, a 32-byte variable, which is fatal on the client.
    variables.sort_by_key(|(index, _)| *index);
    let mut previous = None;
    for (index, value) in variables {
        varint(previous.map_or(index + 1, |previous| index - previous), &mut payload);
        previous = Some(index);
        varint(value.len(), &mut payload);
        payload.extend_from_slice(&value);
    }
    payload.push(0);
    // Framed as every message: the length's low byte, then the rest of it as a varint. A plain varint length left that
    // second part out, and the client dropped every update.
    let mut message = Vec::new();
    crate::channel::message(MESSAGE_ENTITY_SNAPSHOT_UPDATE, &payload, &mut message);
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
    }

    /// Real `EntityRemoteMethod` payloads of a client in the hangar, from the replicant's log.
    const IDLE: &str = "01b864d9450000000732d7793e74bfbc3f000101b864d945000000d7793e74bfbc3f0014160500000000000001b864d945000000c2bd4374bfbc3f00";
    const FORWARD: &str = "01b864d9450000000737579b1ceac2bc3f000101b864d945000000579b1ceac2bc3f0014160500000000000101b864d9450000005801000000c47e22eac2bc3f00";
    const TURN_AND_LEFT: &str = concat!(
        "01b864d94500000007a401ef9206ebc2bc3f000101b864d945000000ef9206ebc2bc3f0014160500000000000201b864d9450000003690",
        "a79cbb26d519b6b962fb393eff7f3f7777bdbb00000000000000001bbd2abe00000000000000006118adbe000000000000000000000000",
        "00000000000000000000000000000000000000000000000000000000000000000000008000000080000000800098063933000000580400",
        "000031d50bebc2bc3f00"
    );
    const PLAYER: u64 = 300_000_000_001;

    fn payload(text: &str) -> Vec<u8> {
        hex(&text.replace(' ', ""))
    }

    #[test]
    fn an_idle_frame_holds_no_button() {
        let decoded = action_frame(&payload(IDLE)).unwrap();
        assert_eq!((decoded.entity, decoded.frame.move_buttons, decoded.frame.aim_rotation, decoded.unknown), (PLAYER, 0, None, None));
        assert_eq!(decoded.frame.time, u64::from_le_bytes(hex("c2bd4374bfbc3f00").try_into().unwrap()));
    }

    #[test]
    fn w_is_the_forward_bit() {
        let decoded = action_frame(&payload(FORWARD)).unwrap();
        assert_eq!(decoded.frame.move_buttons, 1);
    }

    #[test]
    fn a_turn_and_a_button_in_one_frame() {
        let decoded = action_frame(&payload(TURN_AND_LEFT)).unwrap();
        assert_eq!(decoded.frame.move_buttons, 4);
        let turn = decoded.frame.aim_rotation.unwrap();
        assert!((turn.w - 0.99999).abs() < 1e-4 && turn.x < 0.0, "{turn:?}");
        assert_eq!(decoded.unknown, None);
    }

    #[test]
    fn sprint_is_bit_3_of_its_action() {
        let frame = |actions: &str, count: u8| {
            let blob = format!("0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 {count:02x} 01b864d945000000 {actions} 0000000000000000");
            let blob = payload(&blob);
            let mut message = payload("01b864d945000000 07");
            message.push(blob.len() as u8);
            message.extend(blob);
            action_frame(&message).unwrap()
        };
        assert!(frame("5801000000 6309", 2).frame.sprint);
        assert!(!frame("5801000000 6301", 2).frame.sprint);
        let stopped = frame("4a00 5801000000 7700 5802000000", 4);
        assert_eq!((stopped.frame.move_buttons, stopped.unknown), (2, Some(0x77)), "the mask after the unknown action is found");
        let lost = frame("4a00 7700 6301", 3);
        assert_eq!((lost.frame.move_buttons, lost.unknown, lost.frame.partial), (0, Some(0x77), true));
        let jump = frame("9c 9d 6402", 3);
        assert_eq!((jump.frame.jump, jump.frame.stance, jump.frame.partial), (3, Some(2), false));
        assert_eq!(frame("9d9e", 2).frame.jump, 6);
    }

    /// Real frames of several actions, from the replicant's log of a client in the hangar.
    const REAL_FRAMES: [(&str, u32, bool, Option<u8>); 5] = [
        ("01b864d94500000007347792f4cfbfbc3f000101b864d9450000007792f4cfbfbc3f0014160500000000000201b864d94500000094ac5f7bfacfbfbc3f00", 0, false, None),
        ("01b864d9450000000735682d6ae3c3bc3f000101b864d945000000682d6ae3c3bc3f0014160500000000000101b864d945000000450100c0ce6fe3c3bc3f00", 0, false, None),
        ("01b864d9450000000738936cfcedc2bc3f000101b864d945000000936cfcedc2bc3f0014160500000000000201b864d94500000058020000009dc8ae01eec2bc3f00", 2, false, None),
        ("01b864d94500000007394be13cf2c2bc3f000101b864d9450000004be13cf2c2bc3f0014160500000000000201b864d9450000004a005804000000564142f2c2bc3f00", 4, false, None),
        ("01b864d94500000007397c5785e8c3bc3f000101b864d9450000007c5785e8c3bc3f0014160500000000000201b864d9450000005801000000630920af8ae8c3bc3f00", 1, true, None),
    ];

    #[test]
    fn real_frames_of_several_actions_read_to_their_end() {
        for (text, buttons, sprint, unknown) in REAL_FRAMES {
            let decoded = action_frame(&payload(text)).unwrap();
            assert_eq!((decoded.frame.move_buttons, decoded.frame.sprint, decoded.unknown, decoded.frame.partial), (buttons, sprint, unknown, false), "{text}");
        }
    }

    /// A real frame holding A, with an action of unknown length put before the mask, as the client sends the mouse wheel's
    /// speed step (0x47) and as the replicant logged it.
    #[test]
    fn the_mask_is_found_past_an_action_of_unknown_length() {
        let real = "01b864d94500000007394be13cf2c2bc3f000101b864d9450000004be13cf2c2bc3f0014160500000000000201b864d9450000004a005804000000564142f2c2bc3f00";
        for wheel in ["4701", "47ffffffff", "4701000000"] {
            let text = real.replacen("4a00", &format!("4a00{wheel}"), 1);
            let mut bytes = payload(&text);
            bytes[9] += (wheel.len() / 2) as u8;
            bytes[10 + 33] = 3;
            let decoded = action_frame(&bytes).unwrap();
            assert_eq!((decoded.frame.move_buttons, decoded.unknown, decoded.frame.partial), (4, Some(0x47), false), "{wheel}");
        }
    }

    #[test]
    fn a_turn_is_found_past_an_action_of_unknown_length() {
        let decoded = action_frame(&payload(TURN_AND_LEFT)).unwrap();
        let mut bytes = payload(TURN_AND_LEFT);
        // `47 05` before the turn: one more action, two more bytes. The length is the varint `a4 01`, so the frame starts at 11.
        bytes.splice(11 + 42..11 + 42, [0x47, 0x05]);
        bytes[9] = 0xa6;
        bytes[11 + 33] = 3;
        let found = action_frame(&bytes).unwrap();
        assert_eq!((found.frame.aim_rotation, found.frame.move_buttons, found.unknown), (decoded.frame.aim_rotation, 4, Some(0x47)));
    }

    #[test]
    fn other_methods_and_short_frames_are_not_action_frames() {
        assert_eq!(action_frame(&payload("01b864d945000000ff0100")), None);
        assert_eq!(action_frame(&payload("01b864d945000000b1010101")), None);
        let other = payload(IDLE).iter().enumerate().map(|(at, byte)| if at == 45 { byte ^ 1 } else { *byte }).collect::<Vec<_>>();
        assert_eq!(action_frame(&other), None, "a frame whose ids do not match");
    }

    fn movement() -> PlayerMovement {
        PlayerMovement {
            entity_id: PLAYER,
            x: 5.9,
            y: -85.14,
            z: 4.51,
            base_rotation: Some(Rotation { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }),
            aim_rotation: Some(Rotation { x: 0.0, y: 0.0, z: 0.0, w: 1.0 }),
            zone_host_id: 220_000_065_536,
            parent_id: 220_000_065_536,
            revision: 1,
            frame_time: 0,
        }
    }

    #[test]
    fn an_update_names_the_variables_that_moved() {
        let sent = movement();
        let moved = PlayerMovement { y: -84.0, revision: 2, ..movement() };
        let message = movement_update(&moved, Some(&sent), false).unwrap();
        let mut expected = vec![16, 38, 0];
        expected.extend_from_slice(&PLAYER.to_le_bytes());
        expected.extend_from_slice(&[0x00, 0x08, 0xf7, 0x03, 24]);
        for axis in [5.9f64, -84.0, 4.51] {
            expected.extend_from_slice(&axis.to_le_bytes());
        }
        expected.push(0);
        assert_eq!(message, expected);
    }

    #[test]
    fn the_first_update_carries_everything_in_the_readers_form() {
        let message = movement_update(&movement(), None, false).unwrap();
        // Framed as `channel::messages` reads every message, then the walk of 0x1415985d0: entity, mask, then numbers each
        // added to the index before, from -1, with a size in bytes, until a 0.
        let body = [&[0u8; 12][..], &message].concat();
        let read = crate::channel::messages(&body);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].0, 16);
        let payload = read[0].1;
        assert_eq!(payload.len(), message.len() - 3);
        assert_eq!(u64::from_le_bytes(payload[..8].try_into().unwrap()), PLAYER);
        assert_eq!(u16::from_le_bytes([payload[8], payload[9]]), 0x800);
        let mut offset = 10;
        let (mut index, mut steps, mut seen) = (u16::MAX, Vec::new(), Vec::new());
        loop {
            let step = read_varint(payload, &mut offset).unwrap();
            if step == 0 {
                break;
            }
            steps.push(step);
            index = index.wrapping_add(step as u16);
            let size = read_varint(payload, &mut offset).unwrap();
            seen.push((index, size, size * 8));
            offset += size;
        }
        assert_eq!(offset, payload.len());
        assert_eq!(steps, [177, 17, 110, 199, 507]);
        // Index, bytes and the bits `ReceiveDirtyValuesFromNetwork` (0x1429423b0) checks against what the variable reads:
        // the aim rotation, the parent, the base rotation, the position and the zone host, as the client's table has them.
        assert_eq!(seen, [(176, 16, 128), (193, 8, 64), (303, 16, 128), (502, 24, 192), (1009, 8, 64)]);
    }

    /// A real update of the client's own, two variables: 317 is sent as 318, then 898 as 581, the step from 317.
    #[test]
    fn the_clients_own_updates_step_their_indexes() {
        let real = payload("01b864d9450000000008be02045c8fb240c50408000000000000000000");
        let (mut offset, mut index, mut seen) = (10, u16::MAX, Vec::new());
        while let Some(step) = read_varint(&real, &mut offset).filter(|step| *step != 0) {
            index = index.wrapping_add(step as u16);
            let size = read_varint(&real, &mut offset).unwrap();
            seen.push((index, size));
            offset += size;
        }
        assert_eq!((seen, offset), (vec![(317, 4), (898, 8)], real.len()));
    }

    #[test]
    fn a_turn_and_a_step_go_in_rising_order() {
        let moved = PlayerMovement {
            y: -84.0,
            base_rotation: Some(Rotation { x: 0.0, y: 0.0, z: 0.5, w: 0.5 }),
            revision: 2,
            ..movement()
        };
        let message = movement_update(&moved, Some(&movement()), false).unwrap();
        // 303 + 1, 16 bytes, the rotation, then 502 - 303 = 199 (`c7 01`), 24 bytes, the position, then the end.
        assert_eq!(&message[3 + 10..3 + 13], [0xb0, 0x02, 16]);
        assert_eq!(&message[3 + 13 + 16..3 + 13 + 16 + 3], [0xc7, 0x01, 24]);
        assert_eq!(message.last(), Some(&0));
    }

    #[test]
    fn only_a_plain_player_bind_gets_updates() {
        let mut player = sc_api::internal::EntityBind { class_crc: 2_961_494_058, ..Default::default() };
        assert!(indexes_hold(&player));
        player.components.push(0x702b_110a);
        assert!(!indexes_hold(&player));
        assert!(!indexes_hold(&sc_api::internal::EntityBind { class_crc: 1, ..Default::default() }));
    }

    #[test]
    fn a_state_names_the_frame_it_is_of() {
        let moved = PlayerMovement { y: -84.0, frame_time: 0x003f_bcc2_ea1c_9b57, revision: 2, ..movement() };
        let message = movement_update(&moved, Some(&movement()), false).unwrap();
        // 502 + 1, the position; 908 - 502 = 406 (`96 03`), 4 bytes of offset; 996 - 908 = 88, 8 bytes of base.
        let tail = &message[3 + 10 + 2 + 1 + 24..];
        assert_eq!(&tail[..3], [0x96, 0x03, 4]);
        assert_eq!(&tail[3..7], [0, 0, 0, 0]);
        assert_eq!(&tail[7..9], [88, 8]);
        assert_eq!(&tail[9..17], 0x003f_bcc2_ea1c_9b57u64.to_le_bytes());
        assert_eq!(&tail[17..], [0]);
        let again = PlayerMovement { revision: 3, ..moved };
        assert_eq!(movement_update(&again, Some(&moved), false), None, "the same frame is not sent twice");
    }

    #[test]
    fn a_later_frame_alone_goes_only_when_refreshed() {
        let sent = PlayerMovement { frame_time: 1_000_000, ..movement() };
        let still = PlayerMovement { frame_time: 1_333_333, revision: 2, ..movement() };
        assert_eq!(movement_update(&still, Some(&sent), false), None, "standing still, frame after frame");
        let message = movement_update(&still, Some(&sent), true).unwrap();
        // 908 + 1 (`8d 07`), the offset; then 88, the base; then the end.
        assert_eq!(&message[3 + 10..3 + 13], [0x8d, 0x07, 4]);
        assert_eq!(&message[3 + 17..3 + 19], [88, 8]);
        assert_eq!(&message[3 + 19..3 + 27], 1_333_333u64.to_le_bytes());
        assert_eq!(message.len(), 3 + 28);
        assert_eq!(movement_update(&still, Some(&still), true), None, "the same frame is not refreshed");
    }

    #[test]
    fn a_client_position_is_its_origin_moved_by_its_delta() {
        let mut data = vec![0u8; CLIENT_POSITION_LEN];
        data[..8].copy_from_slice(&220_000_065_536u64.to_le_bytes());
        for (axis, value) in [5.9f64, -85.14, 4.51].iter().enumerate() {
            data[8 + axis * 8..16 + axis * 8].copy_from_slice(&value.to_le_bytes());
        }
        data[36..40].copy_from_slice(&0.5f32.to_le_bytes());
        data[68 + 12..68 + 16].copy_from_slice(&1f32.to_le_bytes());
        data[84 + 8..84 + 12].copy_from_slice(&std::f32::consts::FRAC_PI_2.to_le_bytes());
        let end = client_position(&data);
        assert_eq!((end.zone_host_id, end.x, end.y, end.z), (220_000_065_536, 5.9, -85.14 + 0.5, 4.51));
        let Rotation { x, y, z, w } = end.base_rotation.unwrap();
        assert!(x == 0.0 && y == 0.0 && (z - w).abs() < 1e-6 && z > 0.7, "a quarter turn about up: {z} {w}");
        let frame = format!("0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 02 01b864d945000000 32{} 5801000000 0000000000000000", data.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        let blob = payload(&frame);
        let mut message = payload("01b864d945000000 07");
        varint(blob.len(), &mut message);
        message.extend(blob);
        let decoded = action_frame(&message).unwrap();
        assert_eq!((decoded.frame.client_position.unwrap().y, decoded.frame.move_buttons, decoded.unknown), (-85.14 + 0.5, 1, None));
    }

    #[test]
    fn the_client_is_given_its_movement_with_flag_0x10_at_927() {
        let message = client_authority(PLAYER);
        let mut expected = vec![16, 18, 0];
        expected.extend_from_slice(&PLAYER.to_le_bytes());
        // Mask 0x800, then 927 + 1 (`a0 07`), 4 bytes (32 bits, 0x140402850), 0x10, then the end.
        expected.extend_from_slice(&[0x00, 0x08, 0xa0, 0x07, 4, 0x10, 0, 0, 0, 0]);
        assert_eq!(message, expected);
    }

    #[test]
    fn nothing_changed_sends_nothing() {
        assert_eq!(movement_update(&movement(), Some(&movement()), false), None);
        let turned = PlayerMovement { revision: 9, ..movement() };
        assert_eq!(movement_update(&turned, Some(&movement()), false), None, "the revision is not a variable");
    }

    #[test]
    fn the_rotation_is_x_y_z_w() {
        let turned = PlayerMovement { base_rotation: Some(Rotation { x: 0.0, y: 0.0, z: 0.5, w: 0.5 }), ..movement() };
        let message = movement_update(&turned, Some(&movement()), false).unwrap();
        let value = &message[3 + 8 + 2 + 3..][..16];
        assert_eq!(value, [0f32, 0.0, 0.5, 0.5].iter().flat_map(|part| part.to_le_bytes()).collect::<Vec<_>>());
    }
}
