//! A player's movement on the wire: the client's input frames in, the server's movement state out. The replicant decodes
//! and encodes; the DGS that holds the player decides where it is.

use sc_api::internal::{ClientPosition, InputFrame, ItemAction, PlayerMovement, Rotation};

/// `EntityRemoteMethod`, `[u64 entity][varint method][varint length][length bytes]` from a client.
pub const MESSAGE_ENTITY_REMOTE_METHOD: usize = 18;
/// `EntitySnapshotUpdate`.
const MESSAGE_ENTITY_SNAPSHOT_UPDATE: u8 = 16;
/// The remote method of the player that carries the action handler's action frames.
pub const ACTION_FRAME_METHOD: usize = 7;

// Action types are the byte each the action handler's requests stores as the action's
// type. The wire gives no length per action.

/// `SDeltaRotation`: `m_originAimRotation` (a quaternion, the aim at the start of the frame), then
/// the delta, start and end velocities of the aim and of the look offset (seven angle triples), then the zone's entity id,
/// 108 bytes. Field order from `VisitPayload`.
const ACTION_DELTA_ROTATION: u8 = 0x36;
/// `SClientAuthoritativePosition`, sent only while the player's `m_clientAuthoritative` is set: `m_originZoneHostId`
/// (u64), `m_originPosition` (3 f64), `m_deltaPosition`, `m_startVelocity`, `m_endVelocity` (3 f32 each),
/// `m_originBaseRotation` (quaternion), `m_deltaBaseRotation` (axis times angle), `m_startBaseVelocity`,
/// `m_endBaseVelocity` (3 f32 each), `planarSpeed` (f32), `flag7c` (a bool, a byte), `m_relativeEntityId` (u64),
/// `m_relativeOriginPosition` (3 f64), then three more 3 f32: 193 bytes, by `VisitPayload`. Not verified:
/// a wrong length fails `chain`, and the frame falls back to the scan.
const ACTION_CLIENT_POSITION: u8 = 0x32;
const CLIENT_POSITION_LEN: usize = 193;
const DELTA_ROTATION_LEN: usize = 108;
/// `SMoveButtonMask`: a `u32`, in every frame while a button is held.
const ACTION_MOVE_BUTTON_MASK: u8 = 0x58;
/// `SSprint(bool)`: one byte, bit 3 set while Shift is held (bit 0 is set in every one).
const ACTION_SPRINT: u8 = 0x63;
const SPRINT_HELD: u8 = 0x08;
/// `SStance`, one byte: 1 stand, 2 crouch.
const ACTION_STANCE: u8 = 0x64;
/// `SJump`, no data.
const ACTION_JUMP_START: u8 = 0x9c;
/// `SJumpHold`, no data.
const ACTION_JUMP_HOLD: u8 = 0x9d;
/// `SJumpRelease`, no data.
const ACTION_JUMP_RELEASE: u8 = 0x9e;
/// The item actions `SDrop`, one bit
/// (drop all); `SSwap`, 47 bytes with an empty rank string; `STake`, 87 bytes. Only the item, the action and the hand
/// are read; the item action's lengths are those of the defaults, so a longer one breaks the chain and is scanned past.
pub const ACTION_DROP: u8 = 0x3a;
/// `Request<SMobiGlas>`: one byte, 0 asks to open the mobiGlas and 1 to close it.
const ACTION_MOBIGLAS: u8 = 0x56;
/// `SFlyMode` (the fly request), the dev fly request: `ESCActorFlyMode` as one byte (0 none, 1 default,
/// 2 no clip), a signed 7-bit varint of one byte for those values (assumed).
const ACTION_FLY_MODE: u8 = 0x48;
/// `SUnholster` (23 bytes: two item ports of a `u16` and an `EntityId` each, a bit apiece, then the pose as a byte) and
/// `SHolster` (20 bytes: the held `EntityId`, a bit, the destination port, a varint post-select action). Types and sizes
/// are those of `EActionType`.
const ACTION_UNHOLSTER: u8 = 0x76;
const ACTION_HOLSTER: u8 = 0x4d;
/// Where `SUnholster` keeps the port index (u16) and the owner entity (u64).
const UNHOLSTER_PORT_AT: usize = 11;
const UNHOLSTER_OWNER_AT: usize = 13;
const UNHOLSTER_LEN: usize = 23;
const HOLSTER_LEN: usize = 20;
/// `ESCActorResultPoseState`: `NoWeapon` and `Weapon`.
const POSE_NO_WEAPON: u32 = 1;
const POSE_WEAPON: u32 = 2;
/// `SReload` (0x60, `Request<SReload>` of `CPlayerInput::OnActionReload`): the target byte (`EReloadTarget`, 0 the main
/// magazine, 1 the secondary), then `SItemReloadInfo`, whose first field is the magazine the client chose (`m_suitMagId`,
/// an `EntityId`). What follows (an inventory id, two class ids, a string, two counts and a bit) has no length known, so
/// the action is read for those first nine bytes and the rest of the frame is found by the scan.
const ACTION_RELOAD: u8 = 0x60;
const RELOAD_MAGAZINE_AT: usize = 1;
const RELOAD_PREFIX_LEN: usize = RELOAD_MAGAZINE_AT + 8;
/// The fixed parts of an `SReload`: the target (1), the magazine
/// (8), the inventory id's holder (8), its context as a varuint, its subject (8), the two class ids (4 each), the item name
/// as a varuint length and its bytes, the two counts (4 each) and the byte of `nothingToLoad`.
const RELOAD_HOLDER_LEN: usize = 8;
const RELOAD_SUBJECT_LEN: usize = 8;
const RELOAD_CLASS_IDS_LEN: usize = 8;
const RELOAD_COUNT_LEN: usize = 4;
const RELOAD_FLAG_LEN: usize = 1;
/// `SFire` (0x45): the `EInputIntention` as a byte, then the `EFireMethod` as a varint, e.g.
/// `45 01 00` (stop, primary).
const ACTION_FIRE: u8 = 0x45;
const FIRE_INTENTION_AT: usize = 0;
const FIRE_METHOD_AT: usize = 1;
const FIRE_LEN: usize = 2;
/// `SChangeMode` (0x31, `CPlayerInput::OnActionWeaponChangeFiremode`): the activation mode as one byte.
const ACTION_CHANGE_MODE: u8 = 0x31;
/// `SConsume` (0x7b) and `SEatAndDrink` (0x7c), `CPlayerInput::OnActionConsume`: the consumable, the user it is used on,
/// the `EConsumeActivationState` as a varint, a flag as one padded byte. The length of 18 is from that field list and
/// unconfirmed on the wire: a wrong one fails `chain`, and the frame falls back to the scan.
const ACTION_CONSUME: u8 = 0x7b;
const ACTION_EAT_AND_DRINK: u8 = 0x7c;
const CONSUME_TARGET_AT: usize = 8;
const CONSUME_STATE_AT: usize = 16;
const CONSUME_LEN: usize = 18;

/// Where the pieces of an `SReload` are, read from its data (after the kind byte): the length of the whole and the
/// offset of the client's `ammoCount`. `None` when the data does not hold a whole one.
fn reload_layout(data: &[u8]) -> Option<(usize, usize)> {
    let mut at = RELOAD_PREFIX_LEN + RELOAD_HOLDER_LEN;
    read_varint(data, &mut at)?;
    at += RELOAD_SUBJECT_LEN + RELOAD_CLASS_IDS_LEN;
    let name = read_varint(data, &mut at)?;
    at += name;
    let counts = at;
    let end = counts + 2 * RELOAD_COUNT_LEN + RELOAD_FLAG_LEN;
    (end <= data.len()).then_some((end, counts))
}

/// The length of the data of an action given what follows its kind byte: fixed for most, read from the data for `SReload`.
fn action_span(action: u8, rest: &[u8]) -> Option<usize> {
    match action {
        ACTION_RELOAD => reload_layout(rest).map(|(end, _)| end),
        _ => action_len(action),
    }
}
pub const ACTION_SWAP: u8 = 0x68;
pub const ACTION_TAKE: u8 = 0x6c;
const DROP_LEN: usize = 1;
const SWAP_LEN: usize = 47;
const TAKE_LEN: usize = 87;
/// Where the fields are in the data of a `STake` (item, action, preferred hand) and of a `SSwap` (item, action).
const TAKE_ACTION_AT: usize = 8;
const TAKE_HAND_AT: usize = 10;
const SWAP_ITEM_AT: usize = 8;
const SWAP_ACTION_AT: usize = 16;
/// `SDrop`'s one bit: drop all.
const DROP_ALL_BIT: u8 = 1;

/// The length of an action's data after its type, where it is known: the three above from their writers, the lengths
/// between known actions (all of them actions of one `bool` or one enum: `SAbilityInteractionModeToggle`
/// 0x15, `SFreeLook` 0x49, `SFullBodyIdle` 0x4a, `SHoldBreath` 0x4c, `SStance` 0x64, `SThirdPerson` 0x6f, `SScanning`
/// 0x8f, `SFire` 0x45 with two enums in 2 bytes, `SBodyCarryDropStart` 0x94 and `SZoom` 0xac with none), and by that rule the other one-`bool` or one-enum actions and those with no arguments. An assumption that is wrong
/// cannot pass `chain`, which needs a frame to end exactly where its actions do.
fn action_len(action: u8) -> Option<usize> {
    match action {
        ACTION_DELTA_ROTATION => Some(DELTA_ROTATION_LEN),
        ACTION_CLIENT_POSITION => Some(CLIENT_POSITION_LEN),
        ACTION_MOVE_BUTTON_MASK => Some(4),
        ACTION_SPRINT | ACTION_STANCE | ACTION_FLY_MODE => Some(1),
        ACTION_DROP => Some(DROP_LEN),
        ACTION_SWAP => Some(SWAP_LEN),
        ACTION_TAKE => Some(TAKE_LEN),
        ACTION_UNHOLSTER => Some(UNHOLSTER_LEN),
        ACTION_HOLSTER => Some(HOLSTER_LEN),
        // Seen.
        0x15 | 0x49 | 0x4a | 0x4c | 0x6f | 0x8f => Some(1),
        // Seen: `SFire` (two enums) as `45 01 00` alone in a frame, `SBodyCarryDropStart` and `SZoom` as `94 ac`, two
        // actions in a frame of 2 bytes.
        ACTION_FIRE => Some(FIRE_LEN),
        ACTION_CHANGE_MODE => Some(1),
        ACTION_CONSUME | ACTION_EAT_AND_DRINK => Some(CONSUME_LEN),
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
        let len = action_span(action, &bytes[at + 1..])?;
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
    while let Some(len) = bytes.get(at).and_then(|action| action_span(*action, &bytes[at + 1..])) {
        let Some(data) = bytes.get(at + 1..at + 1 + len) else { break };
        actions.push((bytes[at], data));
        at += 1 + len;
    }
    let unknown = bytes.get(at).copied();
    // The reload is the unknown action itself: it counts as one of the frame's, with its first fields read.
    let mut reload = 0;
    if unknown == Some(ACTION_RELOAD) {
        if let Some(prefix) = bytes.get(at + 1..at + 1 + RELOAD_PREFIX_LEN) {
            actions.push((ACTION_RELOAD, prefix));
            reload = 1;
        }
    }
    for start in at + 1..bytes.len() {
        let action = bytes[start];
        if action != ACTION_MOVE_BUTTON_MASK && action != ACTION_DELTA_ROTATION {
            continue;
        }
        let Some(tail) = chain(&bytes[start..], None) else { continue };
        if tail.first().is_some_and(|(action, data)| plausible(*action, data)) && actions.len() - reload + tail.len() < count {
            actions.extend(tail);
            break;
        }
    }
    (actions, unknown)
}

/// A client's `EntityRemoteMethod`: the entity, the method's index and its arguments. `None` when the length does not
/// match the payload.
pub fn remote_method(payload: &[u8]) -> Option<(u64, u32, &[u8])> {
    let entity = u64_at(payload, 0)?;
    let mut offset = 8;
    let method = u32::try_from(read_varint(payload, &mut offset)?).ok()?;
    let length = read_varint(payload, &mut offset)?;
    let args = payload.get(offset..)?;
    (args.len() == length).then_some((entity, method, args))
}

/// An `EntityRemoteMethod` to a client, framed, as the client sends its own.
pub fn remote_method_message(entity: u64, method: u32, args: &[u8]) -> Vec<u8> {
    let mut payload = entity.to_le_bytes().to_vec();
    varint(method as usize, &mut payload);
    varint(args.len(), &mut payload);
    payload.extend_from_slice(args);
    let mut message = Vec::new();
    crate::channel::message(MESSAGE_ENTITY_REMOTE_METHOD as u8, &payload, &mut message);
    message
}

/// `EntityTokenRequest`, `[u64 entity][varint token][varint request][u8 flags]` from a client: the token is an index into the
/// entity's serialized components, the request is the client's own number for it. `flags` was 0xce for the tokens a client
/// asks for as an entity binds and 0xcf for the inventory container's user token.
pub const MESSAGE_ENTITY_TOKEN_REQUEST: usize = 21;
/// `EntityTokenEvent`, `[u64 entity][varint token][varint request][u8 type]` to a client.
const MESSAGE_ENTITY_TOKEN_EVENT: u8 = 23;
/// The `EntityTokenEvent` type that denies a request, and the one that grants it.
const TOKEN_DENIED: u8 = 0;
const TOKEN_GRANTED: u8 = 1;

/// `(entity, token, request, flags)` of an `EntityTokenRequest` payload.
pub fn token_request(payload: &[u8]) -> Option<(u64, u32, u32, u8)> {
    let entity = u64_at(payload, 0)?;
    let mut offset = 8;
    let token = u32::try_from(read_varint(payload, &mut offset)?).ok()?;
    let request = u32::try_from(read_varint(payload, &mut offset)?).ok()?;
    let flags = *payload.get(offset)?;
    (payload.len() == offset + 1).then_some((entity, token, request, flags))
}

/// An `EntityTokenEvent` that grants or denies a client's request, framed.
pub fn token_event_message(entity: u64, token: u32, request: u32, granted: bool) -> Vec<u8> {
    let mut payload = entity.to_le_bytes().to_vec();
    varint(token as usize, &mut payload);
    varint(request as usize, &mut payload);
    payload.push(if granted { TOKEN_GRANTED } else { TOKEN_DENIED });
    let mut message = Vec::new();
    crate::channel::message(MESSAGE_ENTITY_TOKEN_EVENT, &payload, &mut message);
    message
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
            ACTION_MOBIGLAS => frame.mobiglas = Some(u32::from(data[0])),
            ACTION_FLY_MODE => frame.fly_mode = Some(u32::from(data[0])),
            ACTION_UNHOLSTER => {
                // The pose is the last byte; the draw is only believed with the pose it asks for.
                frame.pose = Some(u32::from(data[UNHOLSTER_LEN - 1]).max(POSE_WEAPON));
                frame.unholster_port = u32::from(u16::from_le_bytes([data[UNHOLSTER_PORT_AT], data[UNHOLSTER_PORT_AT + 1]]));
                frame.unholster_owner = u64_at(data, UNHOLSTER_OWNER_AT).unwrap_or(0);
                tracing::info!(data = %data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), "unholster action");
            }
            ACTION_HOLSTER => {
                frame.pose = Some(POSE_NO_WEAPON);
                frame.holstered_item = u64_at(data, 0).unwrap_or(0);
                tracing::info!(data = %data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), "holster action");
            }
            ACTION_RELOAD => {
                frame.reload = Some(u32::from(data[0]));
                frame.reload_magazine = u64_at(data, RELOAD_MAGAZINE_AT).unwrap_or(0);
                // The counts only when the whole action was read; a reload cut short has just its first fields.
                if let Some((_, counts)) = reload_layout(data) {
                    let count = |at: usize| i32::from_le_bytes(data[at..at + RELOAD_COUNT_LEN].try_into().unwrap());
                    frame.reload_ammo_count = Some(count(counts));
                    frame.reload_ammo_capacity = count(counts + RELOAD_COUNT_LEN);
                }
                tracing::info!(data = %data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), whole = reload_layout(data).is_some(), "reload action");
            }
            ACTION_FIRE => {
                frame.fire = Some(u32::from(data[FIRE_INTENTION_AT]));
                frame.fire_method = u32::from(data[FIRE_METHOD_AT]);
            }
            ACTION_CHANGE_MODE => frame.change_mode = Some(u32::from(data[0])),
            ACTION_CONSUME => {
                frame.consume_item = u64_at(data, 0).unwrap_or(0);
                frame.consume_target = u64_at(data, CONSUME_TARGET_AT).unwrap_or(0);
                frame.consume_state = Some(u32::from(data[CONSUME_STATE_AT]));
            }
            ACTION_JUMP_START => frame.jump |= 1,
            ACTION_JUMP_HOLD => frame.jump |= 2,
            ACTION_JUMP_RELEASE => frame.jump |= 4,
            ACTION_DELTA_ROTATION => {
                frame.aim_rotation = Some(Rotation { x: f32_at(data, 0), y: f32_at(data, 4), z: f32_at(data, 8), w: f32_at(data, 12) });
            }
            ACTION_CLIENT_POSITION => frame.client_position = Some(client_position(data)),
            ACTION_TAKE => frame.item_actions.push(ItemAction {
                kind: u32::from(ACTION_TAKE),
                item: u64_at(data, 0)?,
                action: u32::from(data[TAKE_ACTION_AT]),
                hand: u32::from(data[TAKE_HAND_AT]),
            }),
            ACTION_SWAP => frame.item_actions.push(ItemAction {
                kind: u32::from(ACTION_SWAP),
                item: u64_at(data, SWAP_ITEM_AT)?,
                action: u32::from(data[SWAP_ACTION_AT]),
                hand: 0,
            }),
            ACTION_DROP => {
                frame.item_actions.push(ItemAction { kind: u32::from(ACTION_DROP), item: 0, action: u32::from(data[0] & DROP_ALL_BIT), hand: 0 });
            }
            _ => {}
        }
    }
    // A partial frame whose mask was found is whole as far as the buttons go.
    if frame.move_buttons != 0 {
        frame.partial = false;
    }
    Some(Decoded { entity, frame, unknown })
}

/// Indexes of the player's variables in its `CSerializedComponent`, which an update names.
/// They are the position of the
/// variable among the 1076 of the `Player` class of this build, so another build or class needs them found again. The
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
    /// `EntityId`).
    pub const ACTION_FRAME_END_BASE: usize = 996;
    /// `SCActorStateDataSynced::m_actionFrameEndTimestamp.m_timestampOffset`, `0xe1442f4e`: a `u32` (32 bits). The time is the base plus the offset.
    pub const ACTION_FRAME_END_OFFSET: usize = 908;
    /// `m_lagCompensation.m_clientAuthoritative`, `0xe51161d2`: a `u32`, the flags
    /// the actor's movement mode follows.
    pub const CLIENT_AUTHORITATIVE: usize = 927;
    /// `SCActorStateDataSynced::SState::m_additiveState.m_baseData`, `0x647be629`: one byte of `ESCActorResultAdditiveState` then
    /// an 8-byte `m_time`.
    pub const ADDITIVE_STATE: usize = 411;
    /// `SCActorStateDataSynced::SMobiGlas::m_state`, `0x75699ad2`: one byte of `EMobiGlasState`.
    pub const MOBIGLAS_STATE: usize = 471;
    /// `SCActorStateDataSynced::SState::m_poseState.m_baseData`, `0x287c85c1`: one byte of `ESCActorResultPoseState` then an
    /// 8-byte `m_time`.
    pub const POSE_STATE: usize = 164;
    /// `SCActorStateDataSynced::SFly::m_flyMode`, `0x8b2c162d`: one byte of `ESCActorFlyMode` (the numbering is the
    /// sort class, then the key, not the registration order).
    pub const FLY_MODE: usize = 563;
    /// `SCActorStateDataSynced::SState::m_state.m_baseData`, `0xc3f388bf`: the result state byte, then its 8-byte time.
    pub const RESULT_STATE: usize = 796;
    /// `SCActorStateDataSynced::SState::m_stanceState.m_baseData`, `0xf76592ee`: the stance byte, then its 8-byte time.
    pub const STANCE_STATE: usize = 1006;
}
/// `ESCActorResultState::Fly` and `Ground`, and `ESCActorResultStanceState::Stand`, as the client's fly state enters and leaves.
const RESULT_STATE_FLY: u8 = 6;
const RESULT_STATE_GROUND: u8 = 8;
const STANCE_STAND: u8 = 1;

/// The flag the official server always sets in `m_clientAuthoritative`. Any value but 0 and 8 makes the client's movement
/// mode 2, so the client's movement check does not run on the client, which moves its actor itself and reports where in each frame (`SClientAuthoritativePosition`).
const CLIENT_AUTHORITATIVE_MOVEMENT: u32 = 0x10;
/// `ESCActorResultAdditiveState::MobiGlas`, and `Inactive` for no additive state.
const ADDITIVE_MOBIGLAS: u8 = 0x13;
const ADDITIVE_INACTIVE: u8 = 0;
/// `EMobiGlasState::Open` and `Closed`.
const MOBIGLAS_OPEN: u8 = 2;
const MOBIGLAS_CLOSED: u8 = 0;
/// The `m_time` after the additive state byte.
const ADDITIVE_TIME_LEN: usize = 8;

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
/// with and without 23 loadout items in the join, so items are not part of it. Components added to
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
/// frame every 33 ms, and naming each of them would be an update every frame for nothing. A the client's check that runs compares
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
    // The mobiGlas the client asked for: its action is validated against these two, and a state that is not open closes
    // it. Nothing is sent before it was first opened, so the additive state stays whatever the client has.
    if sent.map_or(movement.mobiglas_open, |sent| sent.mobiglas_open != movement.mobiglas_open) {
        let (additive, state) = if movement.mobiglas_open { (ADDITIVE_MOBIGLAS, MOBIGLAS_OPEN) } else { (ADDITIVE_INACTIVE, MOBIGLAS_CLOSED) };
        let mut value = vec![additive];
        value.extend_from_slice(&[0; ADDITIVE_TIME_LEN]);
        variables.push((index::ADDITIVE_STATE, value));
        variables.push((index::MOBIGLAS_STATE, vec![state]));
    }
    // The pose the client asked for (a drawn weapon is `Weapon`): a pose the replicated state does not share is put back by
    // the client after its state timeout, which holsters the weapon. The time is the frame the pose is of.
    if movement.pose != 0 && sent.is_none_or(|sent| sent.pose != movement.pose) {
        let mut value = vec![movement.pose as u8];
        value.extend_from_slice(&movement.frame_time.to_le_bytes());
        variables.push((index::POSE_STATE, value));
    }
    // The fly the client asked for (and the DGS allowed): the result state is `Fly` with the mode and the standing stance, as
    // the client's own fly state enters it, or the client's check puts the client back. Leaving it goes back to `Ground`. Nothing is
    // sent before the first request, so the state stays whatever the client has.
    if sent.map_or(movement.fly_mode != 0, |sent| sent.fly_mode != movement.fly_mode) {
        let timed = |state: u8| {
            let mut value = vec![state];
            value.extend_from_slice(&movement.frame_time.to_le_bytes());
            value
        };
        let flying = movement.fly_mode != 0;
        variables.push((index::RESULT_STATE, timed(if flying { RESULT_STATE_FLY } else { RESULT_STATE_GROUND })));
        if flying {
            variables.push((index::STANCE_STATE, timed(STANCE_STAND)));
        }
        variables.push((index::FLY_MODE, vec![movement.fly_mode as u8]));
    }
    // Which of the client's own frames this state is: the client's check then compares it with where the client was at the end of
    // that frame, not with where it is now, so the time the update takes to arrive does not count.
    let frame_changed = movement.frame_time != 0 && sent.is_none_or(|sent| sent.frame_time != movement.frame_time);
    if frame_changed && (refresh || !variables.is_empty()) {
        variables.push((index::ACTION_FRAME_END_BASE, movement.frame_time.to_le_bytes().to_vec()));
        variables.push((index::ACTION_FRAME_END_OFFSET, 0u32.to_le_bytes().to_vec()));
    }
    if variables.is_empty() {
        return None;
    }
    Some(variables_update(movement.entity_id, variables))
}

/// `EntitySnapshotUpdate` of `variables` of an entity, each by its index in the entity's layout, with its value.
pub fn variables_update(entity_id: u64, mut variables: Vec<(usize, Vec<u8>)>) -> Vec<u8> {
    let mut payload = entity_id.to_le_bytes().to_vec();
    payload.extend_from_slice(&SNAPSHOT_VARIABLES.to_le_bytes());
    // The reader adds each number to the index before it, starting from -1: the first is the index + 1, every
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
    message
}

/// `EntitySnapshotUpdate` mask bits position, three f64 (x y z), and
/// rotation, four f64 (x y z w), in wire order, each field byte aligned as the bind's transform is.
const SNAPSHOT_POSITION: u16 = 0x2;
const SNAPSHOT_ROTATION: u16 = 0x4;

/// `EntitySnapshotUpdate` that moves an already bound entity: `[u64 entity][u16 mask]`, then the position and the rotation that
/// are given, f64 little endian each. The entity's parent and attachment, with no bit `0x10`, stay. Not verified on a client:
/// the layout is the bind's transform (`encode_bind`) behind the mask.
pub fn moved_update(entity_id: u64, position: Option<[f64; 3]>, rotation: Option<[f64; 4]>) -> Vec<u8> {
    let mut payload = entity_id.to_le_bytes().to_vec();
    let mask = if position.is_some() { SNAPSHOT_POSITION } else { 0 } | if rotation.is_some() { SNAPSHOT_ROTATION } else { 0 };
    payload.extend_from_slice(&mask.to_le_bytes());
    for value in position.iter().flatten().chain(rotation.iter().flatten()) {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    let mut message = Vec::new();
    crate::channel::message(MESSAGE_ENTITY_SNAPSHOT_UPDATE, &payload, &mut message);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moved_entity_is_its_id_a_position_mask_and_three_doubles() {
        let message = moved_update(0x5d_21db_a08c, Some([1.0, -2.5, 3.0]), None);
        let expected = concat!(
            "10", "22", "00", // message 16, length 34, no more length
            "8ca0db215d000000", // entity id
            "0200", // mask: position
            "000000000000f03f", "00000000000004c0", "0000000000000840", // 1.0 -2.5 3.0
        );
        assert_eq!(message, hex(expected));
    }

    #[test]
    fn a_rotation_follows_the_position_in_the_mask_order() {
        let message = moved_update(7, Some([0.0; 3]), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(message.len(), 3 + 8 + 2 + 24 + 32);
        assert_eq!(&message[11..13], &[0x06, 0x00]);
        assert_eq!(&message[message.len() - 8..], &1.0f64.to_le_bytes());
        assert_eq!(moved_update(7, None, None).len(), 3 + 10);
    }

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
    fn a_reload_names_its_magazine_whatever_follows_it() {
        let frame = |actions: &str, count: u8| {
            let blob = format!("0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 {count:02x} 01b864d945000000 {actions} 0000000000000000");
            let blob = payload(&blob);
            let mut message = payload("01b864d945000000 07");
            message.push(blob.len() as u8);
            message.extend(blob);
            action_frame(&message).unwrap()
        };
        // The target, the magazine 0x3f00_00a3 and a tail of unknown length (inventory id, classes, string, counts), then
        // the button mask that ends the frame.
        let reload = "60 00 a300003f00000000 0000000000000000 00 0000000000000000 00000000 00000000 7f 00000000 00000000 00";
        let decoded = frame(&format!("{reload} 5802000000"), 2);
        assert_eq!((decoded.frame.reload, decoded.frame.reload_magazine), (Some(0), 0x3f00_00a3));
        assert_eq!((decoded.unknown, decoded.frame.move_buttons), (Some(ACTION_RELOAD), 2), "the mask after it is found");
        assert_eq!(frame("5802000000", 1).frame.reload, None);
    }

    #[test]
    fn a_whole_reload_is_read_by_the_layout_of_the_game() {
        let frame = |actions: &str, count: u8| {
            let blob = format!("0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 {count:02x} 01b864d945000000 {actions} 0000000000000000");
            let blob = payload(&blob);
            let mut message = payload("01b864d945000000 07");
            message.push(blob.len() as u8);
            message.extend(blob);
            action_frame(&message).unwrap()
        };
        // Context 300 takes two bytes; the item name is "ab"; 7 rounds of 30; nothingToLoad clear.
        let reload = "60 00 a300003f00000000 0100000000000000 ac02 0200000000000000 11000000 22000000 02 6162 07000000 1e000000 00";
        let decoded = frame(&format!("{reload} 5802000000"), 2);
        assert_eq!((decoded.unknown, decoded.frame.move_buttons, decoded.frame.partial), (None, 2, false), "read to its end, nothing scanned");
        assert_eq!((decoded.frame.reload_magazine, decoded.frame.reload_ammo_count, decoded.frame.reload_ammo_capacity), (0x3f00_00a3, Some(7), 30));
        let empty = "60 01 0000000000000000 0000000000000000 00 0000000000000000 00000000 00000000 00 00000000 00000000 01";
        assert_eq!(frame(&format!("{empty} 5801000000"), 2).frame.move_buttons, 1, "the 44 byte form");
    }

    #[test]
    fn a_fire_action_carries_its_intention_and_method() {
        let frame = |actions: &str, count: u8| {
            let blob = format!("0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 {count:02x} 01b864d945000000 {actions} 0000000000000000");
            let blob = payload(&blob);
            let mut message = payload("01b864d945000000 07");
            message.push(blob.len() as u8);
            message.extend(blob);
            action_frame(&message).unwrap()
        };
        // The stop of the primary fire, then a start of the secondary.
        let stop = frame("450100", 1);
        assert_eq!((stop.frame.fire, stop.frame.fire_method, stop.unknown), (Some(1), 0, None));
        let start = frame("450001 5801000000", 2);
        assert_eq!((start.frame.fire, start.frame.fire_method, start.frame.move_buttons), (Some(0), 1, 1));
        assert_eq!(frame("5801000000", 1).frame.fire, None);
        let mode = frame("3101", 1);
        assert_eq!((mode.frame.change_mode, mode.unknown), (Some(1), None));
    }

    #[test]
    fn a_consume_action_names_the_item_the_target_and_the_state() {
        let blob = "0000000000000000 01 01b864d945000000 0000000000000000 1416050000000000 02 01b864d945000000 \
                    7b 2a00000000000000 0100000000000000 01 01  5801000000 0000000000000000";
        let blob = payload(blob);
        let mut message = payload("01b864d945000000 07");
        message.push(blob.len() as u8);
        message.extend(blob);
        let decoded = action_frame(&message).unwrap();
        assert_eq!((decoded.frame.consume_item, decoded.frame.consume_target, decoded.frame.consume_state), (0x2a, 1, Some(1)));
        assert_eq!((decoded.frame.move_buttons, decoded.unknown), (1, None));
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
            mobiglas_open: false,
            fly_mode: 0,
            pose: 0,
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
        // Framed as `channel::messages` reads every message, then the reader's walk: entity, mask, then numbers each
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
        // Index, bytes and the bits `ReceiveDirtyValuesFromNetwork` checks against what the variable reads:
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
        // Mask 0x800, then 927 + 1 (`a0 07`), 4 bytes (32 bits), 0x10, then the end.
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

    #[test]
    fn a_real_token_request_is_read() {
        // The access token of the worn core's container (token 2, request 1), and an arbitrated token (0) of the same
        // entity, from the 2026-11-09 session.
        assert_eq!(token_request(&hex("1ca0db215d0000000201cf")), Some((400_000_000_028, 2, 1, 0xcf)));
        assert_eq!(token_request(&hex("1ca0db215d0000000001ce")), Some((400_000_000_028, 0, 1, 0xce)));
        assert_eq!(token_request(&hex("1da0db215d0000000009ce")), Some((400_000_000_029, 0, 9, 0xce)));
        assert_eq!(token_request(&hex("1ca0db215d00000002")), None, "cut short");
        assert_eq!(token_request(&hex("1ca0db215d0000000201cf00")), None, "longer than a request");
    }

    #[test]
    fn a_token_event_names_the_request_and_the_decision() {
        // Message 23, length 11 (low byte, then the high bits): the entity, token 2, request 1, granted.
        assert_eq!(token_event_message(400_000_000_028, 2, 1, true), hex("170b001ca0db215d000000020101"));
        assert_eq!(token_event_message(400_000_000_028, 2, 1, false), hex("170b001ca0db215d000000020100"));
    }

    #[test]
    fn a_swap_of_a_stored_item_is_read() {
        // The client swapping a MedPen (400000000161) with action 4, in a frame of a client-position and a swap.
        let frame = action_frame(&hex("053f66d94500000007a402be8b7a5dc7be3f0001053f66d945000000be8b7a5dc7be3f00141605000000000002053f66d9450000003200980ac034000000300cfeb511e252409a2a1c3154aa24c00010a015014048c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003d08573f0bea0a3f00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000680000000000000000a1a0db215d000000040100000000000000000b000000000000000000ff03000000000000000001ed97805dc7be3f00")).unwrap();
        assert_eq!(frame.unknown, None);
        assert_eq!(frame.frame.item_actions, vec![ItemAction { kind: 0x68, item: 400_000_000_161, action: 4, hand: 0 }]);
        assert!(frame.frame.client_position.is_some());
    }

    #[test]
    fn an_opened_mobiglas_is_replicated_as_additive_state_and_open() {
        let opened = PlayerMovement { mobiglas_open: true, revision: 2, ..movement() };
        let update = movement_update(&opened, Some(&movement()), false).expect("the mobiGlas changed");
        let at = |bytes: &[u8]| update.windows(bytes.len()).any(|window| window == bytes);
        // [varint size 9][0x13 and 8 zero bytes] and [varint size 1][2].
        assert!(at(&[9, 0x13, 0, 0, 0, 0, 0, 0, 0, 0]), "{update:02x?}");
        assert!(at(&[1, 2]), "{update:02x?}");
        assert_eq!(movement_update(&opened, Some(&opened), false), None, "nothing more to say");
        let closed = movement_update(&movement(), Some(&opened), false).expect("closed again");
        assert!(closed.windows(10).any(|window| window == [9, 0, 0, 0, 0, 0, 0, 0, 0, 0]), "{closed:02x?}");
    }

    #[test]
    fn a_first_update_with_the_mobiglas_closed_leaves_its_state_alone() {
        let update = movement_update(&movement(), None, false).unwrap();
        assert!(!update.windows(10).any(|window| window == [9, 0x13, 0, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn a_fly_request_is_replicated_as_the_fly_state_with_its_mode() {
        let flying = PlayerMovement { fly_mode: 2, revision: 2, frame_time: 9, ..movement() };
        let update = movement_update(&flying, Some(&movement()), false).expect("the fly mode changed");
        let timed = |state: u8| [vec![state], 9u64.to_le_bytes().to_vec()].concat();
        // The movement is unchanged but for the fly state, the standing stance, the mode and the frame it is of.
        let expected = variables_update(
            flying.entity_id,
            vec![
                (index::RESULT_STATE, timed(RESULT_STATE_FLY)),
                (index::STANCE_STATE, timed(STANCE_STAND)),
                (index::FLY_MODE, vec![2]),
                (index::ACTION_FRAME_END_BASE, 9u64.to_le_bytes().to_vec()),
                (index::ACTION_FRAME_END_OFFSET, 0u32.to_le_bytes().to_vec()),
            ],
        );
        assert_eq!(update, expected);
        let ground = movement_update(&movement(), Some(&flying), false);
        assert!(ground.is_some(), "leaving fly goes back to ground");
        // Nothing before the first request: a first update with no fly leaves the client's state alone.
        let first = movement_update(&movement(), None, false).expect("a first update");
        assert!(!first.windows(2).any(|pair| pair == [(index::FLY_MODE + 1) as u8 & 0x7f | 0x80, 4]));
        assert_eq!(index::FLY_MODE, 563);
    }

    #[test]
    fn the_mobiglas_action_is_read_from_a_frame() {
        assert_eq!(super::ACTION_MOBIGLAS, 0x56);
    }

    #[test]
    fn a_drawn_weapon_is_replicated_as_the_weapon_pose_with_the_frame_time() {
        let drawn = PlayerMovement { pose: 2, frame_time: 0x1122_3344_5566_7788, revision: 2, ..movement() };
        let update = movement_update(&drawn, Some(&movement()), false).expect("the pose changed");
        let mut value = vec![9, 2];
        value.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        assert!(update.windows(value.len()).any(|window| window == value), "{update:02x?}");
        assert_eq!(movement_update(&drawn, Some(&drawn), false), None);
    }

    #[test]
    fn unholster_and_holster_have_the_lengths_of_their_writers() {
        assert_eq!((action_len(0x76), action_len(0x4d)), (Some(23), Some(20)));
    }
}
