//! A player's inventory requests: the moves, equips and stores its client asks for.
//!
//! The client queues each request (`CSCPlayerInventory::RequestInventoryManagementOperation`) and sends the next one only
//! after the first is answered. Only a query it runs itself; every other request goes to the authority of its player as the
//! remote method `pi.RmAuthorityProcessInventoryManagementOperation`, which answers the client with
//! `pi.RmTokenMoveRequestCompleted` (result, request id, item). Without that answer the request stays "pending" for ever
//! (`Inventory Mgmt Request Queued ... RequestInProgress[1]`), and so does every request behind it.
//!
//! The methods' indices are the order the player entity's components registered them in, which is not known for the
//! player (the registrar is 0x1444b4220 of 4.10.2). Within the inventory's methods the order is fixed: the process method is
//! the second and the completion the twelfth, so the answer is ten methods after the request, whatever the player's base.
//! The request is recognised by its shape instead: a request id, the type as a varint, then the player's own id.

/// Methods between `RmAuthorityProcessInventoryManagementOperation` and `RmTokenMoveRequestCompleted` in the registrar.
pub const COMPLETED_AFTER_PROCESS: u32 = 10;

/// `EInventoryContext` names, by value: the game formats an inventory id with the name (`"%llu:%s:%llu"`, 0x1406cc590).
/// Observed live: 3 is `Location` and 11 the context of an inventory that is not set.
const CONTEXTS: [&str; 12] = [
    "Container",
    "Debug",
    "Entitlement",
    "Location",
    "PlayerInventory",
    "PlayerEscrowInventory",
    "Prison",
    "PersonalEntityInventory",
    "Hangar",
    "Mission",
    "ClientOnly",
    "Undefined",
];
const CONTEXT_UNDEFINED: i32 = 11;

/// An `SInventoryId`: holder entity, context and subject, written as a `u64`, a varint and a `u64`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InventoryId {
    pub holder: u64,
    pub context: i32,
    pub subject: u64,
}

impl InventoryId {
    /// Whether the request names an inventory (the client writes `0:Undefined:0` for none).
    pub fn is_set(&self) -> bool {
        self.holder != 0 && self.context != CONTEXT_UNDEFINED && (0..CONTEXT_UNDEFINED).contains(&self.context)
    }

    /// The id sc-entitygraph keeps it under, `<holder>:<context>:<subject>`. `None` for one that is not set.
    pub fn id(&self) -> Option<String> {
        self.is_set().then(|| format!("{}:{}:{}", self.holder, CONTEXTS[self.context as usize], self.subject))
    }
}

/// The fields after the two inventories, as far as they are known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    /// The CRC of the item's `EntityClassDefinition` GUID *(inferred: the same value the bind of the class carries)*.
    pub class_id: u32,
    pub amount: u32,
    pub source_rank: String,
    pub target_rank: String,
    /// The entity the request is about, 0 for none: an attach asks by class and inventory.
    pub item: u64,
    /// *(Inferred)* the entity an unstow starts from; an equip from an inventory carries the player here.
    pub unstow_root: u64,
    /// `itemPortPath` of an equip: the ports from the player down, `Body_ItemPort:Armor_Undersuit:Armor_Helmet`.
    pub port_path: String,
}

/// An `SInventoryManagementRequest`, as its bit stream writer (0x1442be760) puts it: the request id (a `u32`), the type (a
/// signed varint), then the player's entity id (a `u64`), the source and the target inventory, each field starting on a byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub request_id: u32,
    pub kind: i32,
    pub player: u64,
    pub source: InventoryId,
    pub target: InventoryId,
    /// `None` when the rest of the request is not readable.
    pub detail: Option<Detail>,
}

/// `EInventoryManagementRequestType` values (the switch in 0x14619f350).
pub const KIND_DROP: i32 = 0;
pub const KIND_INTERACTION: i32 = 1;
pub const KIND_MOVE: i32 = 2;
pub const KIND_SORT: i32 = 3;
pub const KIND_SPLIT: i32 = 4;
pub const KIND_STACK: i32 = 5;
pub const KIND_SWAP: i32 = 6;
pub const KIND_STORE: i32 = 7;
pub const KIND_STORE_AND_HIDE: i32 = 8;
pub const KIND_OPEN_NESTED_INVENTORY: i32 = 9;
pub const KIND_QUERY_INVENTORY: i32 = 10;
pub const KIND_SWAP_OUTFIT: i32 = 11;
pub const KIND_AMMO_REPOOL: i32 = 12;
pub const KIND_STACK_ALL: i32 = 13;
pub const KIND_INVALID: i32 = 14;

/// A reader over the arguments, each read at the cursor.
struct Reader<'a> {
    args: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let found = self.args.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(found)
    }

    fn u32(&mut self) -> Option<u32> {
        self.bytes().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.bytes().map(u64::from_le_bytes)
    }

    /// A varint of 7 bits a byte, low first, with the last byte's bit 6 as the sign.
    fn varint(&mut self) -> Option<i64> {
        let (mut value, mut shift) = (0i64, 0u32);
        loop {
            let byte = *self.args.get(self.at)?;
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            self.at += 1;
            if byte & 0x80 == 0 {
                if shift < 63 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
            if shift > 28 {
                return None;
            }
        }
    }

    /// A string: a varint length, then the bytes.
    fn string(&mut self) -> Option<String> {
        let length = usize::try_from(self.varint()?).ok()?;
        let bytes = self.args.get(self.at..self.at.checked_add(length)?)?;
        self.at += length;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    fn inventory(&mut self) -> Option<InventoryId> {
        Some(InventoryId { holder: self.u64()?, context: i32::try_from(self.varint()?).ok()?, subject: self.u64()? })
    }

    /// `classId`, `amount`, `sourceRank`, `classItemRankList`, two numbers, `targetRank`, `targetRankList`, `itemEntityId`,
    /// `unstowAggregateRootId`, `itemPortPath`. What follows (the post action, data entries, a direction, the stacks to
    /// move) is not read.
    fn detail(&mut self) -> Option<Detail> {
        let class_id = self.u32()?;
        let amount = self.u32()?;
        let source_rank = self.string()?;
        // `classItemRankList`: a count, each a `u32` and a rank.
        for _ in 0..self.varint()? {
            self.u32()?;
            self.string()?;
        }
        self.u32()?;
        self.u32()?;
        let target_rank = self.string()?;
        // `targetRankList` is not read: its elements are not known, and no request seen has any.
        if self.varint()? != 0 {
            return None;
        }
        let item = self.u64()?;
        let unstow_root = self.u64()?;
        let port_path = self.string()?;
        Some(Detail { class_id, amount, source_rank, target_rank, item, unstow_root, port_path })
    }
}

impl Request {
    /// `None` for arguments that are not a request of `player`, or that are too short to be one.
    pub fn parse(args: &[u8], player: u64) -> Option<Self> {
        let mut reader = Reader { args, at: 0 };
        let request_id = reader.u32()?;
        let kind = i32::try_from(reader.varint()?).ok()?;
        let found = reader.u64()?;
        // The request is written for its own player, and has more behind it: two inventory ids at least.
        if found != player || request_id == 0 || !(0..=KIND_INVALID).contains(&kind) || args.len() < reader.at + 2 * 17 {
            return None;
        }
        let (source, target) = (reader.inventory()?, reader.inventory()?);
        let detail = reader.detail();
        Some(Self { request_id, kind, player, source, target, detail })
    }

    /// Whether the DGS completes a request it does not apply with success: any but an invalid one, or a query, which a
    /// client runs itself and never sends.
    pub fn succeeds(&self) -> bool {
        self.kind != KIND_INVALID && self.kind != KIND_QUERY_INVENTORY
    }
}

/// The arguments of `RmTokenMoveRequestCompleted(bool result, u32 request_id, EntityId item)`: a bit, then the others each
/// on the next byte (0x1445445d0).
pub fn completed_args(result: bool, request_id: u32, item: u64) -> Vec<u8> {
    let mut args = vec![u8::from(result)];
    args.extend(request_id.to_le_bytes());
    args.extend(item.to_le_bytes());
    args
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const PLAYER: u64 = 300_000_100_001;
    /// `300000100001:Location:1105811199`, the player's item bank at its home.
    pub const BANK: InventoryId = InventoryId { holder: PLAYER, context: 3, subject: 1_105_811_199 };
    const NONE: InventoryId = InventoryId { holder: 0, context: CONTEXT_UNDEFINED, subject: 0 };

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

    /// The arguments as the writer (0x1442be760) lays them out, up to the port path and the tail seen in two real requests.
    pub fn build(request_id: u32, kind: u8, player: u64, source: InventoryId, target: InventoryId, detail: &Detail) -> Vec<u8> {
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
        // The tail of the real requests, not decoded: two ids and owners, a post action, empty lists and stacks.
        args.extend(unhex("ff030000000000000000ff030000000000000000ffffffff000000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000000100000000000000000000000000"));
        args
    }

    pub fn detail(class_id: u32, item: u64, port_path: &str) -> Detail {
        Detail { class_id, amount: 1, source_rank: String::new(), target_rank: String::new(), item, unstow_root: 0, port_path: port_path.into() }
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()).collect()
    }

    /// Request 4 of the first real test, the arguments of the call on the player after the entity id, the method (185) and
    /// the argument length: a Store of the worn helmet 400000000066 into the player's item bank.
    const STORE_REAL: &str = "0400000007a13e66d94500000000000000000000000b0000000000000000a13e66d94500000003ff56e941000000000000000000000000000000000000000000000d616d736c78796c77627869796e0042a0db215d000000000000000000000000ff030000000000000000ff030000000000000000ffffffff140000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000ac0100000000000000000000000000";
    /// Request 5: an Interaction that equips the stowed `rrs_specialist_heavy_helmet_01_01_01` (class CRC 0x431a7340) on
    /// `Body_ItemPort:Armor_Undersuit:Armor_Helmet`.
    const EQUIP_REAL: &str = "0500000001a13e66d945000000a13e66d94500000003ff56e9410000000000000000000000000b000000000000000040731a43010000000000000000000000000000000000000000000000a13e66d9450000002a426f64795f4974656d506f72743a41726d6f725f556e646572737569743a41726d6f725f48656c6d6574ff030000000000000000ff030000000000000000ffffffff000000ffffffff000100000000000000000b00000000000000000000000000000000000000000b00000000000000000300000000000100000000000000000000000000";

    #[test]
    fn a_real_store_request_is_read() {
        let request = Request::parse(&unhex(STORE_REAL), PLAYER).unwrap();
        assert_eq!((request.request_id, request.kind), (4, KIND_STORE));
        assert_eq!((request.source, request.target), (NONE, BANK));
        assert_eq!(request.target.id().as_deref(), Some("300000100001:Location:1105811199"));
        assert_eq!(request.source.id(), None);
        let detail = request.detail.unwrap();
        assert_eq!((detail.item, detail.class_id, detail.target_rank.as_str(), detail.port_path.as_str()), (400_000_000_066, 0, "amslxylwbxiyn", ""));
    }

    #[test]
    fn a_real_equip_request_is_read() {
        let request = Request::parse(&unhex(EQUIP_REAL), PLAYER).unwrap();
        assert_eq!((request.request_id, request.kind), (5, KIND_INTERACTION));
        assert_eq!((request.source, request.target), (BANK, NONE));
        let detail = request.detail.unwrap();
        assert_eq!((detail.item, detail.class_id, detail.amount, detail.unstow_root), (0, 0x431a_7340, 1, PLAYER));
        assert_eq!(detail.port_path, "Body_ItemPort:Armor_Undersuit:Armor_Helmet");
    }

    #[test]
    fn what_the_writer_lays_out_reads_back() {
        let sent = detail(0x431a_7340, 0, "Body_ItemPort:Armor_Undersuit:Armor_Helmet");
        let args = build(5, KIND_INTERACTION as u8, PLAYER, BANK, NONE, &Detail { unstow_root: PLAYER, ..sent.clone() });
        assert_eq!(args, unhex(EQUIP_REAL), "the builder makes the real request byte for byte");
        let store = build(4, KIND_STORE as u8, PLAYER, NONE, BANK, &Detail { target_rank: "amslxylwbxiyn".into(), amount: 0, class_id: 0, ..detail(0, 400_000_000_066, "") });
        assert_eq!(Request::parse(&store, PLAYER).unwrap().detail.unwrap().item, 400_000_000_066);
    }

    #[test]
    fn what_is_not_a_request_of_the_player_is_not_read() {
        let args = unhex(EQUIP_REAL);
        assert!(Request::parse(&args, PLAYER).is_some());
        assert_eq!(Request::parse(&args, PLAYER + 1), None);
        assert_eq!(Request::parse(&args[..20], PLAYER), None);
        let mut zero = args.clone();
        zero[..4].fill(0);
        assert_eq!(Request::parse(&zero, PLAYER), None);
        zero[..4].copy_from_slice(&3u32.to_le_bytes());
        zero[4] = 15;
        assert_eq!(Request::parse(&zero, PLAYER), None);
    }

    #[test]
    fn a_request_cut_short_after_its_inventories_has_no_detail() {
        let args = unhex(EQUIP_REAL);
        let request = Request::parse(&args[..4 + 1 + 8 + 34 + 3], PLAYER).unwrap();
        assert_eq!(request.detail, None);
    }

    #[test]
    fn an_invalid_request_fails_and_the_rest_succeed_when_not_applied() {
        let mut args = unhex(EQUIP_REAL);
        args[4] = KIND_INVALID as u8;
        assert!(!Request::parse(&args, PLAYER).unwrap().succeeds());
        args[4] = KIND_SORT as u8;
        assert!(Request::parse(&args, PLAYER).unwrap().succeeds());
    }

    #[test]
    fn the_completion_is_a_bit_then_the_request_and_the_item() {
        let args = completed_args(true, 0x0403_0201, 0x1122_3344_5566_7788);
        assert_eq!(args, [1, 1, 2, 3, 4, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(completed_args(false, 2, 0)[0], 0);
    }
}
