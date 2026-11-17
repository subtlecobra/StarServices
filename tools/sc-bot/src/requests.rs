//! The requests a client sends that the bot emits: inventory operations (the argument layout is
//! `services/dgs/src/inventory.rs::build`), the press of a transit
//! call panel (`services/dgs/src/interaction.rs`) and the answers to read.
//!
//! What is not decoded cannot be emitted: the tail of an inventory request is the bytes two real requests ended with, and
//! the per-type use of the common fields is assumed for every type but Interaction (equip), Store and Move (see
//! docs/BOT.md).

/// `EInventoryManagementRequestType`.
pub mod kind {
    pub const DROP: u8 = 0;
    pub const INTERACTION: u8 = 1;
    pub const MOVE: u8 = 2;
    pub const SORT: u8 = 3;
    pub const SPLIT: u8 = 4;
    pub const STACK: u8 = 5;
    pub const SWAP: u8 = 6;
    pub const STORE: u8 = 7;
    pub const STORE_AND_HIDE: u8 = 8;
    pub const OPEN_NESTED_INVENTORY: u8 = 9;
    pub const SWAP_OUTFIT: u8 = 11;
    pub const AMMO_REPOOL: u8 = 12;
    pub const STACK_ALL: u8 = 13;
}

/// `EInventoryContext`.
pub mod context {
    pub const CONTAINER: i32 = 0;
    pub const LOCATION: i32 = 3;
    pub const PLAYER_INVENTORY: i32 = 4;
    pub const UNDEFINED: i32 = 11;
}

/// Methods between `pi.RmAuthorityProcessInventoryManagementOperation` and `pi.RmTokenMoveRequestCompleted`.
pub const COMPLETED_AFTER_PROCESS: u32 = 10;
/// The player's method index of `RmAuthorityProcessInventoryManagementOperation` in build 4.10.2: the player's methods start at 184 and it is the second. Not derivable from the wire, so configurable.
pub const DEFAULT_PROCESS_METHOD: u32 = 185;

/// An `SInventoryId`: holder entity, context and subject.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InventoryId {
    pub holder: u64,
    pub context: i32,
    pub subject: u64,
}

impl InventoryId {
    /// `0:Undefined:0`, which the client writes for none.
    pub const NONE: InventoryId = InventoryId { holder: 0, context: context::UNDEFINED, subject: 0 };

    /// A player's item bank at a location: `<player>:Location:<location>`.
    pub fn bank(player: u64, location: u64) -> Self {
        Self { holder: player, context: context::LOCATION, subject: location }
    }

    /// The player's personal inventory, `<player>:PlayerInventory:0`.
    pub fn personal(player: u64) -> Self {
        Self { holder: player, context: context::PLAYER_INVENTORY, subject: 0 }
    }

    /// The inventory of an item that holds others, `<entity>:Container:0`.
    pub fn container(entity: u64) -> Self {
        Self { holder: entity, context: context::CONTAINER, subject: 0 }
    }
}

/// The fields after the two inventories.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detail {
    /// The CRC of the item's class GUID, as binds carry it.
    pub class_id: u32,
    pub amount: u32,
    pub source_rank: String,
    pub target_rank: String,
    /// The entity the request is about; 0 for an item the client has no entity for (a stowed one).
    pub item: u64,
    /// An equip from an inventory carries the player here.
    pub unstow_root: u64,
    /// `Body_ItemPort:Armor_Undersuit:Armor_Helmet`: the ports from the player down, the target port last.
    pub port_path: String,
}

fn varint(out: &mut Vec<u8>, value: usize) {
    let mut value = value;
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn string(out: &mut Vec<u8>, text: &str) {
    varint(out, text.len());
    out.extend(text.as_bytes());
}

fn inventory(out: &mut Vec<u8>, id: InventoryId) {
    out.extend(id.holder.to_le_bytes());
    out.push(id.context as u8);
    out.extend(id.subject.to_le_bytes());
}

/// The tail of the two real requests: two ids and owners, a post action, empty lists and stacks. Not decoded.
const TAIL: &str = "ff030000000000000000ff030000000000000000ffffffff000000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000000100000000000000000000000000";

/// The arguments of `RmAuthorityProcessInventoryManagementOperation`.
pub fn inventory_request(request_id: u32, kind: u8, player: u64, source: InventoryId, target: InventoryId, detail: &Detail) -> Vec<u8> {
    let mut args = request_id.to_le_bytes().to_vec();
    args.push(kind);
    args.extend(player.to_le_bytes());
    inventory(&mut args, source);
    inventory(&mut args, target);
    args.extend(detail.class_id.to_le_bytes());
    args.extend(detail.amount.to_le_bytes());
    string(&mut args, &detail.source_rank);
    args.push(0);
    args.extend([0; 8]);
    string(&mut args, &detail.target_rank);
    args.push(0);
    args.extend(detail.item.to_le_bytes());
    args.extend(detail.unstow_root.to_le_bytes());
    string(&mut args, &detail.port_path);
    args.extend(crate::wire::unhex(TAIL));
    args
}

/// The arguments of `RmTokenMoveRequestCompleted(bool result, u32 request_id, EntityId item)`.
pub fn parse_completed(args: &[u8]) -> Option<(bool, u32, u64)> {
    if args.len() != 13 {
        return None;
    }
    Some((args[0] & 1 != 0, u32::from_le_bytes(args[1..5].try_into().ok()?), u64::from_le_bytes(args[5..13].try_into().ok()?)))
}

/// `CRC-32` of `callelevator`, the interaction of a transit call panel.
pub const CALL_ELEVATOR: u32 = 0xb275_937b;
/// `SvRequestInteract` and `ClOnInteract` on a transit call panel.
pub const PANEL_REQUEST_INTERACT: u32 = 10;
pub const PANEL_ON_INTERACT: u32 = 11;
/// The interaction point hash and time value of the press the layout was taken from (a panel).
pub const SAMPLE_POINT: u32 = 0x42bf_347e;
const SAMPLE_TIME: [u8; 8] = [0x0d, 0xa5, 0x2c, 0x4d, 0xc1, 0xbd, 0x3f, 0x00];

/// The arguments of `SvRequestInteract`: a time value, the request id, the point's and the interaction's name hashes,
/// the user, an interaction metadata block (11 bytes, all default) and the interactable.
pub fn interaction(request_id: u64, point: u32, interaction: u32, user: u64, interactable: u64) -> Vec<u8> {
    let mut args = SAMPLE_TIME.to_vec();
    args.extend(request_id.to_le_bytes());
    args.extend(point.to_le_bytes());
    args.extend(interaction.to_le_bytes());
    args.extend(user.to_le_bytes());
    args.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    args.extend(interactable.to_le_bytes());
    args
}

/// The player's remote method that carries the action frames (`RmAuthoritySendActions`).
pub const ACTION_FRAME_METHOD: u32 = 7;
/// `Request<SMobiGlas>`: one byte, 0 opens and 1 closes.
pub const ACTION_MOBIGLAS: u8 = 0x56;
pub const MOBIGLAS_OPEN_REQUEST: u8 = 0;
pub const MOBIGLAS_CLOSE_REQUEST: u8 = 1;
/// `SUnholster` (23 data bytes) and `SHolster` (20 data bytes).
pub const ACTION_UNHOLSTER: u8 = 0x76;
pub const ACTION_HOLSTER: u8 = 0x4d;
/// The eight bytes every real frame carries after the first time, which the replicant does not read.
const FRAME_UNKNOWN: [u8; 8] = [0x14, 0x16, 0x05, 0, 0, 0, 0, 0];
/// The empty item port a real `SUnholster` starts with: `u16 0x03ff` and a `u64` 0.
const EMPTY_PORT: [u8; 10] = [0xff, 0x03, 0, 0, 0, 0, 0, 0, 0, 0];
/// The entity of a real `SUnholster` (its owner field).
pub const SAMPLE_HOLSTER_OWNER: u64 = 0x5d_21db_a08e;
/// A real `SUnholster` payload: an empty port, a byte, a port index, a byte, the owner, a bit and the pose (2, Weapon).
pub const SAMPLE_UNHOLSTER: &str = "ff0300000000000000002806008ea0db215d0000000102";

/// One action of a frame: its type byte and the data after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub kind: u8,
    pub data: Vec<u8>,
}

impl Action {
    pub fn new(kind: u8, data: &[u8]) -> Self {
        Self { kind, data: data.to_vec() }
    }

    /// `Request<SMobiGlas>`.
    pub fn mobiglas(request: u8) -> Self {
        Self::new(ACTION_MOBIGLAS, &[request])
    }

    /// `SUnholster` with a real payload.
    pub fn unholster() -> Self {
        Self::new(ACTION_UNHOLSTER, &crate::wire::unhex(SAMPLE_UNHOLSTER))
    }

    /// `SHolster` of `held`: the entity, a bit, the destination port (empty) and a post-select action of 0 (20 bytes).
    pub fn holster(held: u64) -> Self {
        let mut data = held.to_le_bytes().to_vec();
        data.push(0);
        data.extend(EMPTY_PORT);
        data.push(0);
        Self::new(ACTION_HOLSTER, &data)
    }
}

/// The arguments of the player's method 7 (the action frame): `[u64 start][u8 1][u64 player][u64 start][8 bytes]
/// [u8 count][u64 player][actions][u64 end]`, as `services/pu-frontend/src/input.rs::action_frame` reads it.
pub fn action_frame(player: u64, start: u64, actions: &[Action], end: u64) -> Vec<u8> {
    let mut blob = start.to_le_bytes().to_vec();
    blob.push(1);
    blob.extend(player.to_le_bytes());
    blob.extend(start.to_le_bytes());
    blob.extend(FRAME_UNKNOWN);
    blob.push(actions.len() as u8);
    blob.extend(player.to_le_bytes());
    for action in actions {
        blob.push(action.kind);
        blob.extend(&action.data);
    }
    blob.extend(end.to_le_bytes());
    blob
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::unhex;

    const PLAYER: u64 = 300_000_100_001;
    const BANK: InventoryId = InventoryId { holder: PLAYER, context: 3, subject: 1_105_811_199 };

    /// The two real requests of `services/dgs/src/inventory.rs` (a Store of the worn helmet, an Interaction that equips one).
    const STORE_REAL: &str = "0400000007a13e66d94500000000000000000000000b0000000000000000a13e66d94500000003ff56e941000000000000000000000000000000000000000000000d616d736c78796c77627869796e0042a0db215d000000000000000000000000ff030000000000000000ff030000000000000000ffffffff140000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000ac0100000000000000000000000000";
    const EQUIP_REAL: &str = "0500000001a13e66d945000000a13e66d94500000003ff56e9410000000000000000000000000b000000000000000040731a43010000000000000000000000000000000000000000000000a13e66d9450000002a426f64795f4974656d506f72743a41726d6f725f556e646572737569743a41726d6f725f48656c6d6574ff030000000000000000ff030000000000000000ffffffff000000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000000100000000000000000000000000";

    #[test]
    fn the_equip_request_is_the_real_one_byte_for_byte() {
        let detail = Detail { class_id: 0x431a_7340, amount: 1, item: 0, unstow_root: PLAYER, port_path: "Body_ItemPort:Armor_Undersuit:Armor_Helmet".into(), ..Detail::default() };
        assert_eq!(inventory_request(5, kind::INTERACTION, PLAYER, BANK, InventoryId::NONE, &detail), unhex(EQUIP_REAL));
    }

    /// The real Store ends in two bytes the builder does not write (`0x14` and `0xac 01`: the tail is not decoded), so the
    /// check is on everything before the tail and on its length.
    #[test]
    fn the_store_request_matches_the_real_one_up_to_its_tail() {
        let detail = Detail { target_rank: "amslxylwbxiyn".into(), item: 400_000_000_066, ..Detail::default() };
        let built = inventory_request(4, kind::STORE, PLAYER, InventoryId::NONE, BANK, &detail);
        let real = unhex(STORE_REAL);
        assert_eq!(built.len(), real.len());
        let head = real.len() - TAIL.len() / 2;
        assert_eq!(built[..head], real[..head]);
    }

    #[test]
    fn the_completion_is_a_bit_then_the_request_and_the_item() {
        assert_eq!(parse_completed(&[1, 1, 2, 3, 4, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]), Some((true, 0x0403_0201, 0x1122_3344_5566_7788)));
        assert_eq!(parse_completed(&[0; 12]), None);
    }

    /// The press the DGS test reads (`interaction.rs`): panel 0x34bfa698f6, request 1.
    #[test]
    fn the_call_panel_press_is_the_real_one_byte_for_byte() {
        let real = "0da52c4dc1bd3f0001000000000000007e34bf427b9375b2a13e66d9450000000100000000000000000000f698a6bf34000000";
        assert_eq!(interaction(1, SAMPLE_POINT, CALL_ELEVATOR, 0x45_d966_3ea1, 0x34_bfa6_98f6), unhex(real));
    }

    /// The real frames of the replicant's tests (`input.rs`): the method's arguments, after its length.
    #[test]
    fn an_idle_frame_is_the_real_one_byte_for_byte() {
        let real = "d7793e74bfbc3f000101b864d945000000d7793e74bfbc3f0014160500000000000001b864d945000000c2bd4374bfbc3f00";
        assert_eq!(action_frame(0x45_d964_b801, 0x003f_bcbf_743e_79d7, &[], 0x003f_bcbf_7443_bdc2), unhex(real));
    }

    #[test]
    fn a_frame_with_the_move_button_mask_is_the_real_one_byte_for_byte() {
        let real = "579b1ceac2bc3f000101b864d945000000579b1ceac2bc3f0014160500000000000101b864d9450000005801000000c47e22eac2bc3f00";
        let frame = action_frame(0x45_d964_b801, 0x003f_bcc2_ea1c_9b57, &[Action::new(0x58, &1u32.to_le_bytes())], 0x003f_bcc2_ea22_7ec4);
        assert_eq!(frame, unhex(real));
    }

    #[test]
    fn the_holster_actions_have_the_lengths_the_replicant_reads() {
        assert_eq!(Action::unholster().data.len(), 23);
        assert_eq!(Action::holster(1).data.len(), 20);
        assert_eq!(*Action::unholster().data.last().unwrap(), 2);
    }
}
