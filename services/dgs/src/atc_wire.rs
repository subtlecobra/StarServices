//! The air traffic controller's calls on the wire.
//! A client asks the ATC for a landing or a take off (a ship from the ASOP kiosk is a take off) with
//! `RmAuthorityRequestPermission`, and the authority answers with `RmMulticastNotifyPlayer` and
//! `RmMulticastChangePermissionToken`. This is the codec only: the absolute method indices of the ATC entity are not known yet, and the state machine that answers is not here yet.
//!
//! The stream: the position is counted in bits; a scalar (a byte, a number, a string, a variable length integer) starts at
//! the next whole byte; a bool is one bit at the current position, least significant first; an enum is a signed LEB128.

/// `EATCActionRequestType`.
pub const REQUEST_LOAD: i64 = 0;
pub const REQUEST_LAND: i64 = 1;
pub const REQUEST_TAKE_OFF: i64 = 2;
pub const REQUEST_NOTIFY: i64 = 3;
/// The default `EATCNotificationType` of a request, the count of the enum.
pub const NOTIFICATION_NONE: i64 = 45;
/// `EATCNotificationType`: what the kiosk takes as success, and as a failure with a text.
pub const NOTIFICATION_TAKE_OFF_AREA_GRANTED: i64 = 3;
pub const NOTIFICATION_NO_PERMISSION_TOKENS: i64 = 13;
pub const NOTIFICATION_NO_VALID_AREA_TYPE: i64 = 14;
pub const NOTIFICATION_NO_VALID_AREA_SIZE: i64 = 15;
pub const NOTIFICATION_WAITING_IN_QUEUE: i64 = 16;
/// `EATCTokenState`.
pub const TOKEN_FREE: i64 = 0;
pub const TOKEN_WAITING_FOR_LANDING: i64 = 1;
pub const TOKEN_WAITING_FOR_TAKING_OFF: i64 = 2;
pub const TOKEN_PLAYER_CHANGING_SHIP: i64 = 3;
pub const TOKEN_OCCUPIED: i64 = 4;
pub const TOKEN_RESERVED: i64 = 6;

/// The tags of the stowage variant: a name, or an inventory.
const STOWAGE_NAME: u8 = 0;
const STOWAGE_INVENTORY: u8 = 1;
/// The longest string read, so that a length cannot ask for more than the call holds.
const MAX_STRING_BYTES: usize = 4096;
/// The longest variable length integer, in bytes.
const MAX_VARINT_BYTES: usize = 10;
const BITS_PER_BYTE: usize = 8;
const VARINT_PAYLOAD_BITS: u32 = 7;
const VARINT_PAYLOAD_MASK: u8 = 0x7f;
const VARINT_CONTINUES: u8 = 0x80;
const VARINT_SIGN_BIT: u8 = 0x40;

struct Reader<'a> {
    bytes: &'a [u8],
    /// The position in bits.
    at: usize,
}

impl<'a> Reader<'a> {
    fn align(&mut self) {
        self.at = self.at.div_ceil(BITS_PER_BYTE) * BITS_PER_BYTE;
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        self.align();
        let start = self.at / BITS_PER_BYTE;
        let found = self.bytes.get(start..start.checked_add(len)?)?;
        self.at += len * BITS_PER_BYTE;
        Some(found)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.bytes.get(self.at / BITS_PER_BYTE)?;
        let bit = byte >> (self.at % BITS_PER_BYTE) & 1 == 1;
        self.at += 1;
        Some(bit)
    }

    /// A signed LEB128.
    fn signed(&mut self) -> Option<i64> {
        let (mut value, mut shift) = (0i64, 0u32);
        for _ in 0..MAX_VARINT_BYTES {
            let byte = self.u8()?;
            value |= i64::from(byte & VARINT_PAYLOAD_MASK) << shift;
            shift += VARINT_PAYLOAD_BITS;
            if byte & VARINT_CONTINUES == 0 {
                if shift < i64::BITS && byte & VARINT_SIGN_BIT != 0 {
                    value |= -1i64 << shift;
                }
                return Some(value);
            }
        }
        None
    }

    fn string(&mut self) -> Option<String> {
        let len = usize::try_from(self.signed()?)
            .ok()
            .filter(|len| *len <= MAX_STRING_BYTES)?;
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    /// The position in bits.
    at: usize,
}

impl Writer {
    fn align(&mut self) {
        self.at = self.at.div_ceil(BITS_PER_BYTE) * BITS_PER_BYTE;
        self.bytes.resize(self.at / BITS_PER_BYTE, 0);
    }

    fn put(&mut self, bytes: &[u8]) {
        self.align();
        self.bytes.extend_from_slice(bytes);
        self.at += bytes.len() * BITS_PER_BYTE;
    }

    fn u8(&mut self, value: u8) {
        self.put(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.put(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.put(&value.to_le_bytes());
    }

    fn bit(&mut self, value: bool) {
        if self.at % BITS_PER_BYTE == 0 {
            self.bytes.push(0);
        }
        if value {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.at % BITS_PER_BYTE);
        }
        self.at += 1;
    }

    fn signed(&mut self, mut value: i64) {
        loop {
            let byte = (value & i64::from(VARINT_PAYLOAD_MASK)) as u8;
            value >>= VARINT_PAYLOAD_BITS;
            let done = (value == 0 && byte & VARINT_SIGN_BIT == 0)
                || (value == -1 && byte & VARINT_SIGN_BIT != 0);
            self.u8(if done { byte } else { byte | VARINT_CONTINUES });
            if done {
                return;
            }
        }
    }

    fn string(&mut self, text: &str) {
        self.signed(text.len() as i64);
        self.put(text.as_bytes());
    }
}

/// An `SInventoryId`: holder, context, subject.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InventoryRef {
    pub holder: u64,
    pub context: i64,
    pub subject: u64,
}

impl InventoryRef {
    fn read(reader: &mut Reader) -> Option<Self> {
        Some(Self {
            holder: reader.u64()?,
            context: reader.signed()?,
            subject: reader.u64()?,
        })
    }

    fn write(&self, writer: &mut Writer) {
        writer.u64(self.holder);
        writer.signed(self.context);
        writer.u64(self.subject);
    }
}

/// Where a ship is stowed: by a name, or in an inventory (a ship with no record is the empty name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stowage {
    Name(String),
    Inventory(InventoryRef),
}

impl Default for Stowage {
    fn default() -> Self {
        Self::Name(String::new())
    }
}

/// An `SATCActionRequest`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionRequest {
    pub player: u64,
    pub vehicle: u64,
    /// The landing area, 0 from the kiosk.
    pub token: u64,
    pub mode: i64,
    pub notification: i64,
    pub force: u8,
    pub time: u64,
    pub extra: u32,
    pub inventory: InventoryRef,
    pub class_name: String,
    pub name_filter: String,
    /// The ASOP spawn of a ship.
    pub is_spawn: bool,
    pub bit92: bool,
    pub stowage: Stowage,
}

impl ActionRequest {
    fn read(reader: &mut Reader) -> Option<Self> {
        let (player, vehicle, token) = (reader.u64()?, reader.u64()?, reader.u64()?);
        let (mode, notification, force) = (reader.signed()?, reader.signed()?, reader.u8()?);
        let (time, extra) = (reader.u64()?, reader.u32()?);
        let inventory = InventoryRef::read(reader)?;
        let (class_name, name_filter) = (reader.string()?, reader.string()?);
        let (is_spawn, bit92) = (reader.bit()?, reader.bit()?);
        let stowage = match reader.u8()? {
            STOWAGE_NAME => Stowage::Name(reader.string()?),
            STOWAGE_INVENTORY => Stowage::Inventory(InventoryRef::read(reader)?),
            _ => Stowage::default(),
        };
        Some(Self {
            player,
            vehicle,
            token,
            mode,
            notification,
            force,
            time,
            extra,
            inventory,
            class_name,
            name_filter,
            is_spawn,
            bit92,
            stowage,
        })
    }

    fn write(&self, writer: &mut Writer) {
        writer.u64(self.player);
        writer.u64(self.vehicle);
        writer.u64(self.token);
        writer.signed(self.mode);
        writer.signed(self.notification);
        writer.u8(self.force);
        writer.u64(self.time);
        writer.u32(self.extra);
        self.inventory.write(writer);
        writer.string(&self.class_name);
        writer.string(&self.name_filter);
        writer.bit(self.is_spawn);
        writer.bit(self.bit92);
        match &self.stowage {
            Stowage::Name(name) => {
                writer.u8(STOWAGE_NAME);
                writer.string(name);
            }
            Stowage::Inventory(inventory) => {
                writer.u8(STOWAGE_INVENTORY);
                inventory.write(writer);
            }
        }
    }
}

/// The arguments of `RmAuthorityRequestPermission`: the request, then a bool.
pub fn read_request_permission(args: &[u8]) -> Option<(ActionRequest, bool)> {
    let mut reader = Reader { bytes: args, at: 0 };
    let request = ActionRequest::read(&mut reader)?;
    Some((request, reader.bit()?))
}

/// What `RmMulticastNotifyPlayer` tells a player about its request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// The `EATCNotificationType`.
    pub kind: i64,
    pub operator: u64,
    /// The landing area the player is told about, 0 for the request's.
    pub token: u64,
    pub queue_position: u32,
    pub generation_time: u64,
    /// The token's HUD identifier, empty for none.
    pub identifier: String,
}

/// The arguments of `RmMulticastNotifyPlayer`: the request, then the notification.
pub fn notify_player_args(request: &ActionRequest, notification: &Notification) -> Vec<u8> {
    let mut writer = Writer::default();
    request.write(&mut writer);
    writer.signed(notification.kind);
    writer.u64(notification.operator);
    writer.u64(notification.token);
    writer.u32(notification.queue_position);
    writer.u64(notification.generation_time);
    writer.string(&notification.identifier);
    writer.bytes
}

/// The arguments of `RmMulticastChangePermissionToken`.
pub fn change_permission_token_args(token: u64, state: i64, owner: u64, vehicle: u64) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.u64(token);
    writer.signed(state);
    writer.u64(owner);
    writer.u64(vehicle);
    writer.bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER: u64 = 0x45_d966_3f05;
    const SHIP: u64 = 0x5d_21db_a08c;
    /// A worked example: a take off of an Arrow from the ASOP kiosk, 102 bytes.
    const REQUEST_PERMISSION: &str = concat!(
        "053f66d945000000",
        "8ca0db215d000000",
        "0000000000000000",
        "02",
        "2d",
        "00",
        "8877665544332211",
        "00000000",
        "053f66d945000000",
        "03",
        "d204000000000000",
        "0a414e564c5f4172726f77",
        "0e48616e6761725f536d6c46726e74",
        "01",
        "01",
        "053f66d945000000",
        "03",
        "d204000000000000",
        "00",
    );

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn inventory() -> InventoryRef {
        InventoryRef {
            holder: PLAYER,
            context: 3,
            subject: 1234,
        }
    }

    fn request() -> ActionRequest {
        ActionRequest {
            player: PLAYER,
            vehicle: SHIP,
            mode: REQUEST_TAKE_OFF,
            notification: NOTIFICATION_NONE,
            time: 0x1122_3344_5566_7788,
            inventory: inventory(),
            class_name: "ANVL_Arrow".into(),
            name_filter: "Hangar_SmlFrnt".into(),
            is_spawn: true,
            stowage: Stowage::Inventory(inventory()),
            ..ActionRequest::default()
        }
    }

    #[test]
    fn the_worked_request_is_read() {
        let args = hex(REQUEST_PERMISSION);
        assert_eq!(args.len(), 102);
        let (read, trailing) = read_request_permission(&args).unwrap();
        assert_eq!(read, request());
        assert!(!trailing);
    }

    #[test]
    fn a_request_writes_back_as_it_was_read() {
        let mut writer = Writer::default();
        request().write(&mut writer);
        writer.bit(false);
        assert_eq!(writer.bytes, hex(REQUEST_PERMISSION));
    }

    #[test]
    fn a_cut_request_is_not_read() {
        let args = hex(REQUEST_PERMISSION);
        assert!(read_request_permission(&args[..args.len() - 1]).is_none());
        assert!(read_request_permission(&[]).is_none());
    }

    #[test]
    fn a_stowage_by_name_is_read() {
        let mut named = request();
        named.stowage = Stowage::Name("pad".into());
        let mut writer = Writer::default();
        named.write(&mut writer);
        writer.bit(true);
        let (read, trailing) = read_request_permission(&writer.bytes).unwrap();
        assert_eq!((read, trailing), (named, true));
    }

    #[test]
    fn signed_numbers_round_trip() {
        for value in [
            0,
            1,
            63,
            64,
            -1,
            -64,
            -65,
            300,
            -300,
            i64::from(i32::MAX),
            i64::MIN + 1,
        ] {
            let mut writer = Writer::default();
            writer.signed(value);
            assert_eq!(
                Reader {
                    bytes: &writer.bytes,
                    at: 0
                }
                .signed(),
                Some(value),
                "{value}"
            );
        }
        // The default notification type is the single byte 0x2d, and 64 needs two bytes.
        let mut writer = Writer::default();
        writer.signed(NOTIFICATION_NONE);
        writer.signed(64);
        assert_eq!(writer.bytes, [0x2d, 0xc0, 0x00]);
    }

    #[test]
    fn a_notification_is_the_request_and_then_its_fields() {
        let notification = Notification {
            kind: NOTIFICATION_TAKE_OFF_AREA_GRANTED,
            operator: 7,
            token: 0x5d_21db_a0aa,
            queue_position: 2,
            generation_time: 9,
            identifier: "pad".into(),
        };
        let args = notify_player_args(&request(), &notification);
        let mut reader = Reader {
            bytes: &args,
            at: 0,
        };
        assert_eq!(ActionRequest::read(&mut reader), Some(request()));
        assert_eq!(reader.signed(), Some(NOTIFICATION_TAKE_OFF_AREA_GRANTED));
        assert_eq!(
            (reader.u64(), reader.u64(), reader.u32(), reader.u64()),
            (Some(7), Some(0x5d_21db_a0aa), Some(2), Some(9))
        );
        assert_eq!(reader.string().as_deref(), Some("pad"));
        assert_eq!(reader.at / BITS_PER_BYTE, args.len());
    }

    #[test]
    fn a_token_change_is_four_fields() {
        let args = change_permission_token_args(5, TOKEN_OCCUPIED, PLAYER, SHIP);
        assert_eq!(args.len(), 8 + 1 + 8 + 8);
        assert_eq!(&args[..8], &5u64.to_le_bytes());
        assert_eq!(args[8], 4);
    }
}
