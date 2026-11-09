//! The framing inside a channel's packets: varints, the messages of the reliable layer, and the records the server
//! sends in them (binds, unbinds, remote methods, token events). The server side of all of it is
//! `services/pu-frontend/src/channel.rs` and `input.rs`; the layouts are the RE repository's `crynub_protocol.md`.

/// `ViewChangeState`, one byte: the sender's connection state.
pub const MSG_DEBUG_SERVER_INFO: u32 = 2;
pub const MSG_DEBUG_AUTHORITY_CHANGE_INFO: u32 = 3;
pub const MSG_SHARD_ID: u32 = 5;
pub const MSG_VIEW_CHANGE_STATE: u32 = 6;
pub const MSG_ENTITY_SNAPSHOT_UPDATE: u32 = 16;
pub const MSG_ENTITY_REMOTE_METHOD: u32 = 18;
pub const MSG_ENTITY_TOKEN_REQUEST: u32 = 21;
pub const MSG_ENTITY_TOKEN_RELEASE: u32 = 22;
pub const MSG_ENTITY_TOKEN_EVENT: u32 = 23;
pub const MSG_BIND_BATCH_BEGIN: u32 = 26;
pub const MSG_BIND_BATCH_ADD_BIND: u32 = 27;
pub const MSG_BIND_BATCH_WAITING: u32 = 28;
pub const MSG_BIND_BATCH_SPAWNS_COMPLETED: u32 = 29;
pub const MSG_BIND_BATCH_END: u32 = 30;
pub const MSG_UNBIND_BATCH: u32 = 31;
pub const MSG_UNBIND_BATCH_BEGIN: u32 = 32;
pub const MSG_UNBIND_BATCH_ADD: u32 = 33;
pub const MSG_UNBIND_BATCH_END: u32 = 34;
pub const MSG_UPDATE_BUBBLE_CREATE: u32 = 42;
pub const MSG_GAME_CONTEXT_INFO: u32 = 45;

/// Appends `value` as an unsigned LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A cursor over a byte slice, reading what the server writes.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pub at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    pub fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let found = self.bytes.get(self.at..self.at.checked_add(count)?)?;
        self.at += count;
        Some(found)
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Option<f64> {
        self.take(8).map(|b| f64::from_le_bytes(b.try_into().unwrap()))
    }

    /// Unsigned LEB128, at most 10 bytes.
    pub fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    /// `[varint length][bytes]`.
    pub fn string(&mut self) -> Option<String> {
        let length = usize::try_from(self.varint()?).ok()?;
        self.take(length).map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

/// `[varint id][u8 length low][varint length high][payload]`, the framing of one message of the reliable layer.
pub fn frame_message(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, u64::from(id));
    out.push(payload.len() as u8);
    put_varint(&mut out, (payload.len() >> 8) as u64);
    out.extend_from_slice(payload);
    out
}

/// The byte stream the message areas of the server's reliable packets make, in sequence order. A message can start in one
/// packet and end in a later one, so whole messages are taken off the front as they complete.
#[derive(Default)]
pub struct MessageStream {
    buffer: Vec<u8>,
}

impl MessageStream {
    pub fn push(&mut self, area: &[u8]) {
        self.buffer.extend_from_slice(area);
    }

    /// The next whole message, or `None` while it is incomplete. Bytes that cannot be a message header are an error.
    pub fn next_message(&mut self) -> Option<(u32, Vec<u8>)> {
        let mut reader = Reader::new(&self.buffer);
        let id = reader.varint()?;
        let low = reader.u8()?;
        let high = reader.varint()?;
        let length = usize::try_from(high << 8).ok()? | usize::from(low);
        let payload = reader.take(length)?.to_vec();
        let used = reader.at;
        self.buffer.drain(..used);
        Some((u32::try_from(id).ok()?, payload))
    }
}

/// A snapshot (variable) in a bind: key, type and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub key: u32,
    pub type_id: u32,
    pub value: Vec<u8>,
}

/// One `BindBatchAddBind`, as `channel.rs::encode_bind` writes it (reader `FUN_141597530`).
#[derive(Clone, Debug, PartialEq)]
pub struct Bind {
    pub entity_id: u64,
    pub batch: u32,
    pub class_crc: u32,
    pub components: Vec<u32>,
    pub flags: u64,
    pub position: [f64; 3],
    /// `x y z w`.
    pub rotation: [f64; 4],
    pub scale: f64,
    pub parent_id: u64,
    /// The CRC-32C of the lowercased name of the port the entity hangs from, `0x46c01eb2` for a zone.
    pub parent_attach_id: u32,
    pub snapshots: Vec<Snapshot>,
}

impl Bind {
    pub fn parse(payload: &[u8]) -> Option<Bind> {
        let mut r = Reader::new(payload);
        let entity_id = r.u64()?;
        let batch = r.u32()?;
        let _layout = r.u32()?;
        let _layout_final = r.u8()?;
        let class_crc = r.u32()?;
        let mut components = Vec::new();
        for _ in 0..r.varint()? {
            components.push(r.u32()?);
            r.u8()?;
        }
        let flags = r.varint()?;
        let mut numbers = [0f64; 8];
        for number in &mut numbers {
            *number = r.f64()?;
        }
        let _bit = r.u8()?;
        let _unnamed = r.u64()?;
        let parent_id = r.u64()?;
        let parent_attach_id = r.u32()?;
        for _ in 0..r.varint()? {
            r.u32()?;
        }
        let _owner = r.u64()?;
        let _last_status_change = r.u64()?;
        let _unnamed = r.u8()?;
        let mut snapshots = Vec::new();
        for _ in 0..r.varint()? {
            let length = usize::try_from(r.varint()?).ok()?;
            let key = r.u32()?;
            let type_id = r.u32()?;
            snapshots.push(Snapshot { key, type_id, value: r.take(length)?.to_vec() });
        }
        Some(Bind {
            entity_id,
            batch,
            class_crc,
            components,
            flags,
            position: [numbers[0], numbers[1], numbers[2]],
            rotation: [numbers[3], numbers[4], numbers[5], numbers[6]],
            scale: numbers[7],
            parent_id,
            parent_attach_id,
            snapshots,
        })
    }

    /// The bytes the server writes for this bind (`channel.rs::encode_bind`), for the tests' fake shard.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.entity_id.to_le_bytes());
        out.extend_from_slice(&self.batch.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(0);
        out.extend_from_slice(&self.class_crc.to_le_bytes());
        put_varint(&mut out, self.components.len() as u64);
        for component in &self.components {
            out.extend_from_slice(&component.to_le_bytes());
            out.push(0);
        }
        put_varint(&mut out, self.flags);
        for value in self.position.iter().chain(&self.rotation).chain([&self.scale]) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.push(0);
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&self.parent_id.to_le_bytes());
        out.extend_from_slice(&self.parent_attach_id.to_le_bytes());
        put_varint(&mut out, 0);
        out.extend_from_slice(&[0; 16]);
        out.push(0);
        put_varint(&mut out, self.snapshots.len() as u64);
        for snapshot in &self.snapshots {
            put_varint(&mut out, snapshot.value.len() as u64);
            out.extend_from_slice(&snapshot.key.to_le_bytes());
            out.extend_from_slice(&snapshot.type_id.to_le_bytes());
            out.extend_from_slice(&snapshot.value);
        }
        out
    }

    pub fn snapshot(&self, key: u32) -> Option<&Snapshot> {
        self.snapshots.iter().find(|snapshot| snapshot.key == key)
    }
}

/// `[varint batch][varint count][count x u64 entity]`, the three forms of an unbind.
pub fn parse_unbind(payload: &[u8]) -> Option<(u64, Vec<u64>)> {
    let mut r = Reader::new(payload);
    let batch = r.varint()?;
    let count = r.varint()?;
    let mut entities = Vec::new();
    for _ in 0..count {
        entities.push(r.u64()?);
    }
    Some((batch, entities))
}

/// An `EntityRemoteMethod` in either direction: `[u64 entity][varint method][varint length][args]`.
pub fn parse_remote_method(payload: &[u8]) -> Option<(u64, u32, Vec<u8>)> {
    let mut r = Reader::new(payload);
    let entity = r.u64()?;
    let method = u32::try_from(r.varint()?).ok()?;
    let length = usize::try_from(r.varint()?).ok()?;
    let args = r.take(length)?.to_vec();
    (r.left() == 0).then_some((entity, method, args))
}

pub fn remote_method(entity: u64, method: u32, args: &[u8]) -> Vec<u8> {
    let mut payload = entity.to_le_bytes().to_vec();
    put_varint(&mut payload, u64::from(method));
    put_varint(&mut payload, args.len() as u64);
    payload.extend_from_slice(args);
    frame_message(MSG_ENTITY_REMOTE_METHOD, &payload)
}

/// `EntityTokenRequest`: `[u64 entity][varint token][varint request][u8 flags]`.
pub fn token_request(entity: u64, token: u32, request: u32, flags: u8) -> Vec<u8> {
    let mut payload = entity.to_le_bytes().to_vec();
    put_varint(&mut payload, u64::from(token));
    put_varint(&mut payload, u64::from(request));
    payload.push(flags);
    frame_message(MSG_ENTITY_TOKEN_REQUEST, &payload)
}

/// `EntityTokenEvent`: `(entity, token, request, type)`; type 1 granted, 0 denied, 2 grant for kind 1, 3 revoke.
pub fn parse_token_event(payload: &[u8]) -> Option<(u64, u32, u32, u8)> {
    let mut r = Reader::new(payload);
    let entity = r.u64()?;
    let token = u32::try_from(r.varint()?).ok()?;
    let request = u32::try_from(r.varint()?).ok()?;
    let kind = r.u8()?;
    Some((entity, token, request, kind))
}

/// A `BindBatchAsyncSpawnsCompleted`: the batch id alone.
pub fn spawns_completed(batch: u32) -> Vec<u8> {
    frame_message(MSG_BIND_BATCH_SPAWNS_COMPLETED, &batch.to_le_bytes())
}

pub fn view_change_state(state: u8) -> Vec<u8> {
    frame_message(MSG_VIEW_CHANGE_STATE, &[state])
}

/// CRC-32C (Castagnoli), as packets and class and port keys use it.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The key of an item port: the CRC-32C of its lowercased name, which is a bind's `parent_attach_id`.
pub fn port_crc(name: &str) -> u32 {
    crc32c(name.to_ascii_lowercase().as_bytes())
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).filter_map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both from `services/pu-frontend/src/channel.rs`, built there by hand from the client's reader.
    const ROOT_RECORD: &str = concat!(
        "fcffffffffffffff", "01000000", "00000000", "00", "070baa42", "00", "8060",
        "0000000000000000", "0000000000000000", "0000000000000000",
        "0000000000000000", "0000000000000000", "0000000000000000", "000000000000f03f", "000000000000f03f",
        "00", "0000000000000000", "0000000000000000", "00000000", "00",
        "0000000000000000", "0000000000000000", "00", "00",
    );
    const CHILD_RECORD: &str = concat!(
        "01d0ed902e000000", "d8060000", "00000000", "00", "2ad084b0", "00", "b002",
        "0000000000000000", "0000000000000000", "0000000000000000",
        "0000000000000000", "0000000000000000", "0000000000000000", "000000000000f03f", "000000000000f03f",
        "00", "0000000000000000", "fcffffffffffffff", "b21ec046", "00",
        "0000000000000000", "0000000000000000", "00", "00",
    );

    #[test]
    fn the_universe_root_bind_reads_back() {
        let bind = Bind::parse(&unhex(ROOT_RECORD)).unwrap();
        assert_eq!((bind.entity_id, bind.batch, bind.class_crc, bind.flags), (0xffff_ffff_ffff_fffc, 1, 0x42aa_0b07, 0x3000));
        assert_eq!((bind.parent_id, bind.rotation[3], bind.scale), (0, 1.0, 1.0));
    }

    #[test]
    fn a_child_bind_names_its_parent_and_zone_attachment() {
        let bind = Bind::parse(&unhex(CHILD_RECORD)).unwrap();
        assert_eq!((bind.entity_id, bind.batch, bind.class_crc, bind.flags), (0x002e_90ed_d001, 0x6d8, 0xb084_d02a, 0x130));
        assert_eq!((bind.parent_id, bind.parent_attach_id), (0xffff_ffff_ffff_fffc, 0x46c0_1eb2));
    }

    #[test]
    fn a_bind_with_snapshots_reads_back_what_the_server_writes() {
        let bind = Bind {
            entity_id: 42,
            batch: 3,
            class_crc: 0x1234,
            components: vec![9],
            flags: 0x1000,
            position: [1.0, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: 1.0,
            parent_id: 7,
            parent_attach_id: 0x46c0_1eb2,
            snapshots: vec![Snapshot { key: 0xf9df_8d76, type_id: 0x728a_e36a, value: 99u64.to_le_bytes().to_vec() }],
        };
        assert_eq!(Bind::parse(&bind.encode()), Some(bind));
        assert_eq!(unhex(ROOT_RECORD), Bind::parse(&unhex(ROOT_RECORD)).unwrap().encode(), "the writer is the server's, byte for byte");
    }

    #[test]
    fn messages_split_across_packets_are_put_together() {
        let whole = frame_message(27, &[7u8; 700]);
        let mut stream = MessageStream::default();
        stream.push(&whole[..300]);
        assert_eq!(stream.next_message(), None);
        stream.push(&whole[300..]);
        stream.push(&frame_message(6, &[3]));
        assert_eq!(stream.next_message(), Some((27, vec![7u8; 700])));
        assert_eq!(stream.next_message(), Some((6, vec![3])));
        assert_eq!(stream.next_message(), None);
    }

    #[test]
    fn the_greeting_of_the_shard_reads_as_three_messages() {
        // channel.rs: the data body after its 12-byte header.
        let mut stream = MessageStream::default();
        stream.push(&[5, 5, 0, 2, b's', b'1', 1, b'n', 6, 1, 0, 3]);
        assert_eq!(stream.next_message(), Some((5, vec![2, b's', b'1', 1, b'n'])));
        assert_eq!(stream.next_message(), Some((6, vec![3])));
    }

    #[test]
    fn unbinds_read_the_server_form() {
        // channel.rs: unbind_batch(7, [0x34bfa698f6]).
        assert_eq!(parse_unbind(&[7, 1, 0xf6, 0x98, 0xa6, 0xbf, 0x34, 0, 0, 0]), Some((7, vec![0x34_bfa6_98f6])));
    }

    #[test]
    fn remote_methods_and_tokens_round_trip() {
        let framed = remote_method(0x45_d966_3ea1, 185, &[1, 2, 3]);
        let mut stream = MessageStream::default();
        stream.push(&framed);
        let (id, payload) = stream.next_message().unwrap();
        assert_eq!((id, parse_remote_method(&payload)), (MSG_ENTITY_REMOTE_METHOD, Some((0x45_d966_3ea1, 185, vec![1, 2, 3]))));
        // `1ca0db215d000000 02 01 cf`, a real request (docs): entity 400000000028, token 2, request 1, flags 0xcf.
        assert_eq!(token_request(400_000_000_028, 2, 1, 0xcf)[3..], unhex("1ca0db215d0000000201cf"));
        assert_eq!(parse_token_event(&unhex("1ca0db215d000000020101")), Some((400_000_000_028, 2, 1, 1)));
    }

    #[test]
    fn a_port_key_is_the_crc_of_its_lowercased_name() {
        // docs: `Body_ItemPort` is 0xab6341ac.
        assert_eq!(port_crc("Body_ItemPort"), 0xab63_41ac);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }
}
