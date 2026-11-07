//! Encrypted channel packets and the RMQ framing inside them.
//!
//! An encrypted packet is `[8-byte header][ciphertext][8-byte counter][16-byte tag]`, with no CRC.
//! Of the three session keys, the first encrypts client to server, the second server to client,
//! and the first 12 bytes of the third give the IV: 4 fixed bytes, then the counter.

use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes128Gcm,
};
use sc_proto::internal::{DgsFrames, EntityBind, EntityTransform};

pub const HEADER_LEN: usize = 8;
pub const FLAG_ENCRYPTED: u8 = 0x02;
pub const FLAG_PLAINTEXT: u8 = 0x04;
/// Set on a retransmission, whose bytes are otherwise the original's. The flag is outside the
/// authenticated header, so the sender can set it without encrypting again.
pub const FLAG_RESEND: u8 = 0x08;
const FLAG_COMPRESSED: u8 = 0x01;
const AUTHENTICATED_FLAGS: u8 = 0x07;
const COUNTER_LEN: usize = 8;
const TAG_LEN: usize = 16;
const TRAILER_LEN: usize = COUNTER_LEN + TAG_LEN;
const IV_PREFIX_LEN: usize = 4;
pub const MAX_BODY_LEN: usize = 0x570;
const DECOMPRESS_CAPACITY: usize = 0x588;
const ACK_RECEIVED: u8 = 0x01;
const ACK_RESENT: u8 = 0x02;

/// What both the CRC and the GCM tag cover of a header.
pub fn normalized_header(header: &[u8]) -> [u8; HEADER_LEN] {
    [header[0], header[1], header[2] & AUTHENTICATED_FLAGS, header[3], 0, 0, 0, 0]
}

pub struct Channel {
    receive: Aes128Gcm,
    send: Aes128Gcm,
    iv_prefix: [u8; IV_PREFIX_LEN],
    send_counter: u64,
}

impl Channel {
    pub fn new(keys: &[[u8; 16]; 3]) -> Self {
        let mut iv_prefix = [0u8; IV_PREFIX_LEN];
        iv_prefix.copy_from_slice(&keys[2][..IV_PREFIX_LEN]);
        let mut counter = [0u8; COUNTER_LEN];
        counter.copy_from_slice(&keys[2][IV_PREFIX_LEN..IV_PREFIX_LEN + COUNTER_LEN]);
        Self {
            receive: Aes128Gcm::new(&keys[0].into()),
            send: Aes128Gcm::new(&keys[1].into()),
            iv_prefix,
            send_counter: u64::from_le_bytes(counter),
        }
    }

    fn iv(&self, counter: &[u8]) -> [u8; IV_PREFIX_LEN + COUNTER_LEN] {
        let mut iv = [0u8; IV_PREFIX_LEN + COUNTER_LEN];
        iv[..IV_PREFIX_LEN].copy_from_slice(&self.iv_prefix);
        iv[IV_PREFIX_LEN..].copy_from_slice(counter);
        iv
    }

    /// The plaintext body of an encrypted packet, decompressed if it was compressed.
    pub fn open(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let end = packet.len().checked_sub(TRAILER_LEN)?;
        if end <= HEADER_LEN || end - HEADER_LEN > MAX_BODY_LEN {
            return None;
        }
        let (counter, tag) = packet[end..].split_at(COUNTER_LEN);
        let mut body = packet[HEADER_LEN..end].to_vec();
        self.receive
            .decrypt_in_place_detached(
                &self.iv(counter).into(),
                &normalized_header(packet),
                &mut body,
                <&[u8; TAG_LEN]>::try_from(tag).ok()?.into(),
            )
            .ok()?;
        if packet[2] & FLAG_COMPRESSED == 0 {
            return Some(body);
        }
        lz4_flex::block::decompress(&body, DECOMPRESS_CAPACITY).ok().filter(|body| body.len() <= MAX_BODY_LEN)
    }

    pub fn seal(&mut self, header: [u8; HEADER_LEN], body: &[u8]) -> Option<Vec<u8>> {
        let counter = self.send_counter.to_le_bytes();
        let mut packet = header.to_vec();
        packet.extend_from_slice(body);
        let tag = self
            .send
            .encrypt_in_place_detached(
                &self.iv(&counter).into(),
                &normalized_header(&header),
                &mut packet[HEADER_LEN..],
            )
            .ok()?;
        packet.extend_from_slice(&counter);
        packet.extend_from_slice(&tag);
        self.send_counter = self.send_counter.wrapping_add(1);
        Some(packet)
    }
}

/// Sequence numbers wrap, so order is the sign of the difference.
fn is_after(left: u32, right: u32) -> bool {
    (left.wrapping_sub(right) as i32) > 0
}

/// The three words that start every ReplicationData body.
#[derive(Debug, PartialEq, Eq)]
pub struct DataHeader {
    pub drop_up_to: u32,
    pub sequence: u32,
    pub blocking: u32,
}

impl DataHeader {
    pub fn parse(body: &[u8]) -> Option<Self> {
        let word = |index: usize| {
            let bytes = body.get(index * 4..index * 4 + 4)?;
            Some(u32::from_le_bytes(bytes.try_into().ok()?))
        };
        let header = Self { drop_up_to: word(0)?, sequence: word(1)?, blocking: word(2)? };
        let valid = !is_after(header.drop_up_to, header.sequence) && !is_after(header.blocking, header.sequence);
        valid.then_some(header)
    }
}

/// The receiving half of RMQ, as far as acknowledging goes. Packets are not reordered or read yet.
fn varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// `[varint id][length low byte][varint length high bits][payload]`. Every message the shard sends is framed so.
pub fn message(id: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    out.push(payload.len() as u8);
    varint(payload.len() >> 8, out);
    out.extend_from_slice(payload);
}

/// LEB128 with the sign taken from bit 6 of the last byte, not zigzag: 63 is `3f`, 64 is `c0 00`, -1 is `7f`.
fn signed_varint(mut value: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let sign = byte & 0x40 != 0;
        if (value == 0 && !sign) || (value == -1 && sign) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn text(value: &str, out: &mut Vec<u8>) {
    varint(value.len(), out);
    out.extend_from_slice(value.as_bytes());
}

const MESSAGE_DEBUG_SERVER_INFO: u8 = 2;
const MESSAGE_DEBUG_AUTHORITY_CHANGE_INFO: u8 = 3;
const MESSAGE_SHARD_ID: u8 = 5;
const MESSAGE_VIEW_CHANGE_STATE: u8 = 6;
const MESSAGE_BIND_BATCH_BEGIN: u8 = 26;
const MESSAGE_BIND_BATCH_ADD_BIND: u8 = 27;
const MESSAGE_BIND_BATCH_WAITING: u8 = 28;
const MESSAGE_BIND_BATCH_SPAWNS_COMPLETED: usize = 29;
const MESSAGE_BIND_BATCH_END: u8 = 30;
const MESSAGE_UPDATE_BUBBLE_CREATE: u8 = 42;
const MESSAGE_GAME_CONTEXT_INFO: u8 = 45;
const CONTEXT_VALUE_LEN: usize = 16;
/// `m_ownerType`, `PlayerOwned`, and `m_mode`, `Update`. The client only makes a bubble of this mode.
const BUBBLE_OWNER_PLAYER: i64 = 0;
const BUBBLE_MODE_UPDATE: i64 = 1;
const MAP_NAME: &str = "megamap";
/// The client reads `IsPersistedInGameMode` from this mode's record. Without it the client stays in
/// `SC_Frontend`, takes the standalone path, and waits for a hierarchy it never requests.
const GAME_MODE: &str = "SC_Default";
const NO_BLOCKING_SEQUENCE: u32 = u32::MAX;
const DATA_HEADER_LEN: usize = 12;

/// The first reliable packet of a channel: who this shard is, and the state its view has reached.
/// `WaitForShardInfo` needs the first and `WaitRemoteState` the second.
pub fn greeting(shard_id: &str, shard_name: &str, state: u8) -> Vec<u8> {
    let mut body = Vec::new();
    for word in [0, 0, NO_BLOCKING_SEQUENCE] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    let mut context = vec![0; CONTEXT_VALUE_LEN];
    text(MAP_NAME, &mut context);
    text(GAME_MODE, &mut context);
    message(MESSAGE_GAME_CONTEXT_INFO, &context, &mut body);
    let mut names = Vec::new();
    text(shard_id, &mut names);
    text(shard_name, &mut names);
    message(MESSAGE_SHARD_ID, &names, &mut body);
    message(MESSAGE_VIEW_CHANGE_STATE, &[state], &mut body);
    body
}

/// A later reliable packet, with the header words the client uses: both the sequence, then the one before.
pub fn later_packet(sequence: u32, messages: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for word in [sequence, sequence, sequence.wrapping_sub(1)] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    body.extend_from_slice(messages);
    body
}

/// `DebugServerInfo`, which the overlay's `Server FPS` and `Server:` line show: four signed LEB128 frame times in 100 ns
/// ticks, the fastest, slowest, average and last, then varints for the hitches and the shard's and the server's population.
/// The frames and the server's population are the DGS's, and the shard's population is this replicant's. A DGS that does
/// not answer has no frames to show, so they go as 0 and `Server FPS` shows 0.0.
pub fn debug_server_stats(frames: &DgsFrames, shard_population: u32, server_population: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    for time in [frames.fastest, frames.slowest, frames.average, frames.last] {
        signed_varint(time, &mut payload);
    }
    for count in [frames.hitches, shard_population, server_population] {
        varint(count as usize, &mut payload);
    }
    let mut out = Vec::new();
    message(MESSAGE_DEBUG_SERVER_INFO, &payload, &mut out);
    out
}

/// `DebugAuthorityChangeInfo`, the name of the `Server:` line, then its numbers.
pub fn debug_server_info(server_name: &str, frames: &DgsFrames, shard_population: u32, server_population: u32) -> Vec<u8> {
    let mut name = Vec::new();
    text(server_name, &mut name);
    let mut out = Vec::new();
    message(MESSAGE_DEBUG_AUTHORITY_CHANGE_INFO, &name, &mut out);
    out.extend_from_slice(&debug_server_stats(frames, shard_population, server_population));
    out
}

/// `UpdateBubbleCreate`, the update bubble of a player, centred on it. A client makes none of its own, and without one no
/// entity is ever in range, so its actor does not update and it takes no input. Read by 0x1414fcd70: each field starts on a
/// byte, and those are `m_parentEntityId`, `m_parentOffset` and a fourth double, two ids no client code reads, `m_uuid`, which
/// must not be zero, `m_playerOwnerId`, `m_ownerType`, `m_mode`, and a last bit that stops the client making the bubble.
pub fn update_bubble_create(player_id: u64) -> Vec<u8> {
    let mut payload = player_id.to_le_bytes().to_vec();
    payload.extend_from_slice(&[0; 32 + 16]);
    payload.extend_from_slice(&player_id.to_le_bytes());
    payload.extend_from_slice(&1u64.to_le_bytes());
    payload.extend_from_slice(&player_id.to_le_bytes());
    signed_varint(BUBBLE_OWNER_PLAYER, &mut payload);
    signed_varint(BUBBLE_MODE_UPDATE, &mut payload);
    payload.push(0);
    let mut out = Vec::new();
    message(MESSAGE_UPDATE_BUBBLE_CREATE, &payload, &mut out);
    out
}

pub fn state_change(state: u8) -> Vec<u8> {
    let mut out = Vec::new();
    message(MESSAGE_VIEW_CHANGE_STATE, &[state], &mut out);
    out
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

/// The most message bytes one replication data packet carries. The client receives a datagram into 0x590 bytes
/// (`CUDPDatagramSocket::Recv`, 0x141489d70, buffer from 0x14149fc70): one longer fails `recvfrom` with `WSAEMSGSIZE`
/// (10040), which it logs as `Message size 10040(WSAEMSGSIZE - Message too large)` and ends in disconnect 30003. With
/// the 8-byte header and the 24-byte trailer that leaves `MAX_BODY_LEN` (0x570), the 12-byte data header included. Some
/// room is left below the limit.
pub const MAX_PACKET_MESSAGES: usize = 1200;

/// The message areas of the packets that carry `messages`, in order, each at most `limit` bytes. Whole messages fill a
/// packet; one that does not fit what is left starts the next. A message longer than `limit` is fragmented as RMQ does
/// it, with no flag or header of its own: its header and the start of its payload go in the packet it is reached in, if
/// the header fits, and the rest of the payload continues at the start of the following packets. The packet that ends it
/// carries nothing after it, as the client's writer leaves it (0x1414ce280 closes the packet). The client's reader
/// (`CReader::DispatchRecvMsgs`, 0x1414cccd0) dispatches such a message only when it takes the packet after the one that
/// completes it, so a run ending with one gets an empty packet after it.
pub fn packet_runs(messages: &[u8], limit: usize) -> Vec<Vec<u8>> {
    let mut packets = vec![Vec::new()];
    let mut offset = 0;
    while offset < messages.len() {
        let start = offset;
        let Some((_, payload)) = next_message(messages, &mut offset) else {
            // Not whole messages: sent as they are, in one packet.
            packets.last_mut().unwrap().extend_from_slice(&messages[start..]);
            break;
        };
        let message = &messages[start..offset];
        let header = message.len() - payload.len();
        let current = packets.last_mut().unwrap();
        if current.len() + message.len() <= limit {
            current.extend_from_slice(message);
        } else if message.len() <= limit {
            packets.push(message.to_vec());
        } else {
            if current.len() + header > limit {
                packets.push(Vec::new());
            }
            let mut rest = message;
            while !rest.is_empty() {
                let current = packets.last_mut().unwrap();
                if current.len() == limit {
                    packets.push(Vec::new());
                    continue;
                }
                let take = rest.len().min(limit - current.len());
                current.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
            }
            // Closed after its end: what follows, or nothing, goes in the next packet.
            packets.push(Vec::new());
        }
    }
    packets
}

/// The whole messages of a data packet body, as id and payload. Stops at one that continues in the next packet, as
/// nothing the shard reads is that long.
pub fn messages(body: &[u8]) -> Vec<(usize, &[u8])> {
    let mut offset = DATA_HEADER_LEN;
    let mut found = Vec::new();
    while offset < body.len() {
        let Some((id, payload)) = next_message(body, &mut offset) else { break };
        found.push((id, payload));
    }
    found
}

fn next_message<'a>(body: &'a [u8], offset: &mut usize) -> Option<(usize, &'a [u8])> {
    let id = read_varint(body, offset)?;
    let low = usize::from(*body.get(*offset)?);
    *offset += 1;
    let length = low | read_varint(body, offset)? << 8;
    let payload = body.get(*offset..offset.checked_add(length)?)?;
    *offset += length;
    Some((id, payload))
}

/// The highest state in the `ViewChangeState` messages of a data packet body.
pub fn reported_state(body: &[u8]) -> Option<u8> {
    let states = messages(body).into_iter().filter(|(id, _)| *id == usize::from(MESSAGE_VIEW_CHANGE_STATE));
    states.filter_map(|(_, payload)| payload.first().copied()).max()
}

/// The batch a `BindBatchAsyncSpawnsCompleted` message in this body names.
pub fn completed_batch(body: &[u8]) -> Option<u32> {
    let (_, payload) = messages(body).into_iter().find(|(id, _)| *id == MESSAGE_BIND_BATCH_SPAWNS_COMPLETED)?;
    Some(u32::from_le_bytes(payload.get(..4)?.try_into().ok()?))
}

/// As `FUN_141597530` reads it. Every field but a bit starts on a byte, so each bit here takes a byte of its own. The
/// transform is a `QuatTS` of doubles, position first: the client defaults `w` and the scale to 1, and zeros there would
/// give it a degenerate transform, which is why a bind with none is at the identity.
fn encode_bind(bind: &EntityBind, batch: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&bind.entity_id.to_le_bytes());
    out.extend_from_slice(&batch.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // layout
    out.push(0); // layout final
    out.extend_from_slice(&bind.class_crc.to_le_bytes());
    varint(bind.components.len(), &mut out);
    for component in &bind.components {
        out.extend_from_slice(&component.to_le_bytes());
        out.push(0); // server only
    }
    varint(bind.flags as usize, &mut out);
    let identity = EntityTransform { qw: 1.0, ..EntityTransform::default() };
    let transform = bind.transform.as_ref().unwrap_or(&identity);
    for value in [transform.x, transform.y, transform.z, transform.qx, transform.qy, transform.qz, transform.qw, 1.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.push(0); // flag 0x800
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&bind.parent_id.to_le_bytes());
    out.extend_from_slice(&bind.parent_attach_id.to_le_bytes());
    varint(0, &mut out); // replicated tags
    out.extend_from_slice(&0u64.to_le_bytes()); // owner
    out.extend_from_slice(&0u64.to_le_bytes()); // last status change
    out.push(0);
    varint(bind.snapshots.len(), &mut out);
    for snapshot in &bind.snapshots {
        varint(snapshot.value.len(), &mut out);
        out.extend_from_slice(&snapshot.key.to_le_bytes());
        out.extend_from_slice(&snapshot.type_id.to_le_bytes());
        out.extend_from_slice(&snapshot.value);
    }
    out
}

/// Everything of a batch up to the wait for the client to spawn it. The client then answers with
/// `BindBatchAsyncSpawnsCompleted`, and refuses `BindBatchEnd` before that as `BindEndStillSpawning`.
pub fn bind_batch(batch: u32, binds: &[EntityBind]) -> Vec<u8> {
    let mut out = Vec::new();
    message(MESSAGE_BIND_BATCH_BEGIN, &batch.to_le_bytes(), &mut out);
    for bind in binds {
        message(MESSAGE_BIND_BATCH_ADD_BIND, &encode_bind(bind, batch), &mut out);
    }
    message(MESSAGE_BIND_BATCH_WAITING, &batch.to_le_bytes(), &mut out);
    out
}

pub fn bind_batch_end(batch: u32) -> Vec<u8> {
    let mut out = Vec::new();
    message(MESSAGE_BIND_BATCH_END, &batch.to_le_bytes(), &mut out);
    out
}

/// Whether a ReplicationAck body `[u32 lastConsumed][u32 base][u16 count]...` covers `sequence`.
pub fn acknowledges(ack: &[u8], sequence: u32) -> bool {
    let word = |at: usize| ack.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let (Some(consumed), Some(base)) = (word(0), word(4)) else { return false };
    let count = ack.get(8..10).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    (consumed != u32::MAX && !is_after(sequence, consumed)) || (count > 0 && sequence.wrapping_sub(base) < u32::from(count))
}

pub struct Receiver {
    last_consumed: u32,
}

impl Receiver {
    pub fn new() -> Self {
        Self { last_consumed: u32::MAX }
    }

    /// The body of a ReplicationAck: `[u32 last consumed][u32 base][u16 count]`, then per sequence
    /// from `base` a received bit and, when set, a resent bit. The sender ignores an ack with no
    /// bits, and one whose `base` is past what it has sent, so this names the packet itself.
    pub fn acknowledge(&mut self, header: &DataHeader, resent: bool) -> [u8; 11] {
        let dropped = header.drop_up_to.wrapping_sub(1);
        if is_after(dropped, self.last_consumed) {
            self.last_consumed = dropped;
        }
        if header.sequence == self.last_consumed.wrapping_add(1) {
            self.last_consumed = header.sequence;
        }
        let mut ack = [0u8; 11];
        ack[..4].copy_from_slice(&self.last_consumed.to_le_bytes());
        ack[4..8].copy_from_slice(&header.sequence.to_le_bytes());
        ack[8..10].copy_from_slice(&1u16.to_le_bytes());
        ack[10] = ACK_RECEIVED | if resent { ACK_RESENT } else { 0 };
        ack
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::internal::EntitySnapshot;

    const UNIVERSE_ROOT_ID: u64 = 0xffff_ffff_ffff_fffc;

    fn identity() -> Option<EntityTransform> {
        Some(EntityTransform { qw: 1.0, ..EntityTransform::default() })
    }

    /// The client's end of a channel: the same keys with the directions swapped.
    #[test]
    fn the_greeting_frames_shard_info_and_the_view_state() {
        let body = greeting("s1", "n", 3);
        assert_eq!(&body[..12], &[0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        let context_len = 16 + 1 + 7 + 1 + 10;
        assert_eq!(&body[12..15], &[45, context_len as u8, 0]);
        assert_eq!(&body[31..39], b"\x07megamap");
        assert_eq!(&body[39..50], b"\x0aSC_Default");
        assert_eq!(&body[50..], &[5, 5, 0, 2, b's', b'1', 1, b'n', 6, 1, 0, 3]);
    }

    fn universe_root() -> EntityBind {
        EntityBind { entity_id: UNIVERSE_ROOT_ID, class_crc: 0x42aa_0b07, flags: 0x3000, transform: identity(), parent_id: 0, parent_attach_id: 0, components: Vec::new(), snapshots: Vec::new() }
    }

    /// Both built by hand from `FUN_141597530`, not by this code. Reading across: id, batch, layout,
    /// final bit, class, components, flags, position, rotation, w, scale, bit, u64, parent, attach id,
    /// tags, owner, status time, u8, snapshots.
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
    fn a_bind_record_is_the_fields_the_client_reads_in_order() {
        assert_eq!(encode_bind(&universe_root(), 1), bytes(ROOT_RECORD));
        let child = EntityBind {
            entity_id: 200_000_000_001,
            class_crc: 2_961_494_058,
            flags: 0x130,
            transform: identity(),
            parent_id: UNIVERSE_ROOT_ID,
            parent_attach_id: 0x46c0_1eb2,
            components: Vec::new(),
            snapshots: Vec::new(),
        };
        assert_eq!(encode_bind(&child, 1752), bytes(CHILD_RECORD));
    }

    #[test]
    fn a_transform_is_position_then_rotation_then_a_scale_of_one() {
        let transform = Some(EntityTransform { x: 1.0, y: 2.0, z: 3.0, qx: 0.0, qy: 0.0, qz: 0.5, qw: 0.5 });
        let record = encode_bind(&EntityBind { transform, ..universe_root() }, 1);
        let expected = concat!(
            "000000000000f03f", "0000000000000040", "0000000000000840",
            "0000000000000000", "0000000000000000", "000000000000e03f", "000000000000e03f", "000000000000f03f",
        );
        assert_eq!(record[24..88], bytes(expected));
    }

    #[test]
    fn a_bind_with_no_transform_is_at_the_identity_and_not_degenerate() {
        assert_eq!(encode_bind(&EntityBind { transform: None, ..universe_root() }, 1), bytes(ROOT_RECORD));
    }

    #[test]
    fn a_snapshot_is_its_size_key_type_and_value_after_the_count() {
        let snapshot = EntitySnapshot { key: 0x2ff5_d193, type_id: 0x54a0_b124, value: b"\x02ab".to_vec() };
        let record = encode_bind(&EntityBind { snapshots: vec![snapshot], ..universe_root() }, 1);
        let plain = encode_bind(&universe_root(), 1);
        assert_eq!(record[..plain.len() - 1], plain[..plain.len() - 1]);
        assert_eq!(record[plain.len() - 1..], bytes("010393d1f52f24b1a054026162"));
    }

    #[test]
    fn a_batch_opens_binds_and_waits_in_one_packet() {
        let mut expected = bytes("1a040001000000");
        expected.extend_from_slice(&[27, (ROOT_RECORD.len() / 2) as u8, 0]);
        expected.extend_from_slice(&bytes(ROOT_RECORD));
        expected.extend_from_slice(&bytes("1c040001000000"));
        assert_eq!(bind_batch(1, &[universe_root()]), expected);
        assert_eq!(bind_batch_end(1), bytes("1e040001000000"));
    }

    #[test]
    fn with_no_frames_the_debug_messages_carry_the_name_then_the_populations() {
        let expected = [0x03, 0x06, 0x00, 0x05, 0x64, 0x67, 0x73, 0x2d, 0x30, 0x02, 0x07, 0x00, 0, 0, 0, 0, 0, 1, 2];
        assert_eq!(debug_server_info("dgs-0", &DgsFrames::default(), 1, 2), expected);
    }

    #[test]
    fn frame_times_are_signed_with_the_sign_in_bit_6_of_the_last_byte() {
        let encoded = |value: i64| {
            let mut out = Vec::new();
            signed_varint(value, &mut out);
            out
        };
        assert_eq!(encoded(0), [0x00]);
        assert_eq!(encoded(63), [0x3f]);
        assert_eq!(encoded(64), [0xc0, 0x00]);
        assert_eq!(encoded(-1), [0x7f]);
        assert_eq!(encoded(-64), [0x40]);
        assert_eq!(encoded(-65), [0xbf, 0x7f]);
        assert_eq!(encoded(333_333), [0x95, 0xac, 0x14], "30 FPS, by the protocol notes");
    }

    /// Fastest, slowest, average and last, as 0x1415a9400 reads them, then the plain varints.
    #[test]
    fn the_frames_come_in_the_order_the_client_reads_them() {
        let frames = DgsFrames { fastest: 63, slowest: 64, average: 333_333, last: 1, hitches: 200 };
        let expected = [0x02, 0x0c, 0x00, 0x3f, 0xc0, 0x00, 0x95, 0xac, 0x14, 0x01, 0xc8, 0x01, 0x96, 0x01, 0x03];
        assert_eq!(debug_server_stats(&frames, 150, 3), expected);
    }

    #[test]
    fn a_later_packet_repeats_its_sequence_then_names_the_one_before() {
        assert_eq!(later_packet(2, &state_change(14)), bytes("0200000002000000010000000601000e"));
    }

    #[test]
    fn messages_are_split_and_a_cut_one_is_left_out() {
        let body = bytes("0000000000000000ffffffff06010003" );
        assert_eq!(messages(&body), vec![(6, &[3u8][..])]);
        let body = bytes("0000000000000000ffffffff0601000a1d0400010000");
        assert_eq!(messages(&body), vec![(6, &[10u8][..])]);
    }

    #[test]
    fn a_completed_batch_is_read_from_its_message() {
        assert_eq!(completed_batch(&bytes("0000000000000000ffffffff060100031d040007000000")), Some(7));
        assert_eq!(completed_batch(&bytes("0000000000000000ffffffff06010003")), None);
    }

    #[test]
    fn states_are_read_from_real_message_framing() {
        let body = bytes("0200000002000000010000000601000206010003");
        assert_eq!(reported_state(&body), Some(3));
        assert_eq!(reported_state(&bytes("000000000000000000000000")), None);
    }

    #[test]
    fn a_real_ack_covers_its_sequence_and_no_later_one() {
        let ack = bytes("0000000000000000010001");
        assert!(acknowledges(&ack, 0));
        assert!(!acknowledges(&ack, 1));
        assert!(!acknowledges(&bytes("ffffffff000000000000"), 0));
    }

    #[test]
    fn long_messages_put_the_high_length_bits_in_the_varint() {
        let mut out = Vec::new();
        message(9, &[0; 300], &mut out);
        assert_eq!(&out[..3], &[9, 300u16 as u8, 1]);
    }

    fn client_of(keys: &[[u8; 16]; 3]) -> Channel {
        Channel::new(&[keys[1], keys[0], keys[2]])
    }

    fn keys() -> [[u8; 16]; 3] {
        let mut keys = [[0u8; 16]; 3];
        for (index, byte) in keys.as_flattened_mut().iter_mut().enumerate() {
            *byte = index as u8;
        }
        keys
    }

    fn bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    /// Both built by tests/nub.py with the `cryptography` package, not by this code.
    const GOLDEN_DATA: &str = "5c0702dcc5e73e61c0f512a486f0790944b7702b77ce947d2425262728292a2b846d5baeb2cd76e9651ecfbe27f56f10";
    const GOLDEN_ACK: &str = "5c0802000000000094d485a79128420af701992425262728292a2ba8afa9490a1b8cd22439d929f3e140b8";

    fn plain_data() -> Vec<u8> {
        bytes("0000000000000000ffffffff06010003")
    }

    #[test]
    fn opens_a_packet_built_by_an_independent_implementation() {
        assert_eq!(Channel::new(&keys()).open(&bytes(GOLDEN_DATA)).unwrap(), plain_data());
    }

    #[test]
    fn seals_the_same_bytes_as_an_independent_implementation() {
        let packet = Channel::new(&keys()).seal(bytes("5c08020000000000").try_into().unwrap(), &bytes("0000000000000000010001"));
        assert_eq!(packet.unwrap(), bytes(GOLDEN_ACK));
    }

    #[test]
    fn the_resend_flag_and_the_header_words_are_not_authenticated() {
        let mut packet = bytes(GOLDEN_DATA);
        packet[2] |= FLAG_RESEND;
        packet[4] ^= 0xff;
        assert_eq!(Channel::new(&keys()).open(&packet).unwrap(), plain_data());
    }

    #[test]
    fn tampering_with_what_is_authenticated_fails() {
        for index in [1, 2, 3, HEADER_LEN, bytes(GOLDEN_DATA).len() - 1] {
            let mut packet = bytes(GOLDEN_DATA);
            packet[index] ^= 0x01;
            assert!(Channel::new(&keys()).open(&packet).is_none(), "byte {index}");
        }
    }

    #[test]
    fn the_directions_use_different_keys() {
        assert!(client_of(&keys()).open(&bytes(GOLDEN_DATA)).is_none());
    }

    #[test]
    fn each_sealed_packet_advances_the_counter() {
        let mut channel = Channel::new(&keys());
        let header = [0x5c, 8, FLAG_ENCRYPTED, 0, 0, 0, 0, 0];
        let (first, second) = (channel.seal(header, b"a").unwrap(), channel.seal(header, b"a").unwrap());
        let counter = |packet: &[u8]| u64::from_le_bytes(packet[9..17].try_into().unwrap());
        assert_eq!(counter(&second), counter(&first) + 1);
        assert_ne!(first[8], second[8]);
        assert_eq!(client_of(&keys()).open(&second).unwrap(), b"a");
    }

    #[test]
    fn compressed_bodies_are_expanded() {
        let original = vec![7u8; 200];
        let compressed = lz4_flex::block::compress(&original);
        assert!(compressed.len() < original.len());
        let packet = client_of(&keys()).seal([0x5c, 7, FLAG_ENCRYPTED | FLAG_COMPRESSED, 0, 0, 0, 0, 0], &compressed);
        assert_eq!(Channel::new(&keys()).open(&packet.unwrap()).unwrap(), original);
    }

    #[test]
    fn an_update_bubble_has_the_layout_the_client_reads() {
        let id = 0x0102_0304_0506_0708u64.to_le_bytes();
        let expected = [&[42, 83, 0][..], &id, &[0; 48], &id, &1u64.to_le_bytes(), &id, &[0, 1, 0]].concat();
        assert_eq!(update_bubble_create(0x0102_0304_0506_0708), expected);
    }

    #[test]
    fn short_or_oversized_packets_are_refused() {
        let channel = Channel::new(&keys());
        assert!(channel.open(&[0x5c; HEADER_LEN + TRAILER_LEN]).is_none());
        assert!(channel.open(&[0x5c; 3]).is_none());
        assert!(channel.open(&vec![0x5c; HEADER_LEN + MAX_BODY_LEN + 1 + TRAILER_LEN]).is_none());
    }

    #[test]
    fn a_data_header_is_three_words_and_never_points_ahead() {
        let body = plain_data();
        let header = DataHeader::parse(&body).unwrap();
        assert_eq!(header, DataHeader { drop_up_to: 0, sequence: 0, blocking: u32::MAX });
        assert!(DataHeader::parse(&body[..11]).is_none());
        assert!(DataHeader::parse(&bytes("010000000000000000000000")).is_none());
        assert!(DataHeader::parse(&bytes("000000000000000001000000")).is_none());
    }

    fn data(drop_up_to: u32, sequence: u32) -> DataHeader {
        DataHeader { drop_up_to, sequence, blocking: u32::MAX }
    }

    #[test]
    fn the_first_packet_is_acknowledged_with_its_own_sequence() {
        let ack = Receiver::new().acknowledge(&data(0, 0), false);
        assert_eq!(ack, [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, ACK_RECEIVED]);
    }

    #[test]
    fn a_resend_is_marked_so_the_sender_keeps_it_out_of_its_round_trip_estimate() {
        assert_eq!(Receiver::new().acknowledge(&data(0, 0), true)[10], ACK_RECEIVED | ACK_RESENT);
    }

    #[test]
    fn a_gap_holds_the_cumulative_ack_until_the_sender_drops_past_it() {
        let mut receiver = Receiver::new();
        receiver.acknowledge(&data(0, 0), false);
        let ack = receiver.acknowledge(&data(0, 2), false);
        assert_eq!(ack[..8], [0, 0, 0, 0, 2, 0, 0, 0]);
        let ack = receiver.acknowledge(&data(3, 3), false);
        assert_eq!(ack[..8], [3, 0, 0, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn an_old_packet_does_not_move_the_cumulative_ack_back() {
        let mut receiver = Receiver::new();
        receiver.acknowledge(&data(0, 0), false);
        receiver.acknowledge(&data(0, 1), false);
        assert_eq!(receiver.acknowledge(&data(0, 0), true)[..4], [1, 0, 0, 0]);
    }

    #[test]
    fn a_long_run_of_short_messages_splits_between_messages() {
        let mut long = Vec::new();
        for n in 0..10u8 {
            message(27, &[n; 200], &mut long);
        }
        let packets = packet_runs(&long, MAX_PACKET_MESSAGES);
        // 203 bytes a message: 5 fill 1015 bytes, a sixth would pass 1200.
        assert_eq!(packets.iter().map(Vec::len).collect::<Vec<_>>(), [1015, 1015]);
        assert_eq!(packets.concat(), long);
        let header = [0u8; DATA_HEADER_LEN];
        let whole = |part: &[u8]| messages(&[&header[..], part].concat()).len();
        assert_eq!(whole(&packets[0]) + whole(&packets[1]), 10);
        assert_eq!(packet_runs(&packets[0], MAX_PACKET_MESSAGES), [packets[0].clone()]);
        assert_eq!(client_reads(&packets).unwrap().len(), 10);
    }

    /// The client's reader, `CReader::DispatchRecvMsgs` (0x1414cccd0), over the message areas of packets in sequence
    /// order: the dispatched messages, in order. A message whose header fits but whose payload runs past the end of its
    /// packet takes the rest of that packet, and each later packet gives `min(left, its bytes)` from its start. The reader
    /// tests what is left before it copies, so a message is dispatched when the packet after the one that completes it is
    /// taken, before that packet's own messages. A header that does not fit is error 0x7561.
    fn client_reads(packets: &[Vec<u8>]) -> Result<Vec<(usize, Vec<u8>)>, &'static str> {
        let mut read = Vec::new();
        let mut fragment: Option<(usize, usize, Vec<u8>)> = None;
        for packet in packets {
            let mut offset = 0;
            if let Some((id, length, mut have)) = fragment.take() {
                let left = length - have.len();
                let take = left.min(packet.len());
                have.extend_from_slice(&packet[..take]);
                offset = take;
                if left == 0 {
                    read.push((id, have));
                } else {
                    fragment = Some((id, length, have));
                }
            }
            while offset < packet.len() {
                let id = read_varint(packet, &mut offset).ok_or("RMQ failed to read message header")?;
                let low = usize::from(*packet.get(offset).ok_or("RMQ failed to read message header")?);
                offset += 1;
                let length = low | read_varint(packet, &mut offset).ok_or("RMQ failed to read message header")? << 8;
                if offset + length > packet.len() {
                    fragment = Some((id, length, packet[offset..].to_vec()));
                    break;
                }
                read.push((id, packet[offset..offset + length].to_vec()));
                offset += length;
            }
        }
        Ok(read)
    }

    /// A batch with a bind of 10,000 bytes: the header and the first bytes of the bind after `BindBatchBegin`, the rest of
    /// it from the start of the next packets with no header of their own, and `BindBatchWaitingForAsyncSpawns` alone after.
    #[test]
    fn a_message_longer_than_a_packet_continues_at_the_start_of_the_next() {
        let bind: Vec<u8> = (0..10_000u32).map(|n| n as u8).collect();
        let mut run = Vec::new();
        message(MESSAGE_BIND_BATCH_BEGIN, &3u32.to_le_bytes(), &mut run);
        message(MESSAGE_BIND_BATCH_ADD_BIND, &bind, &mut run);
        message(MESSAGE_BIND_BATCH_WAITING, &3u32.to_le_bytes(), &mut run);
        let packets = packet_runs(&run, MAX_PACKET_MESSAGES);
        // 7 + 3 + 10,000 + 7 bytes: 10,010 to the end of the bind is 8 full packets and 410, then the last message.
        let sizes: Vec<usize> = packets.iter().map(Vec::len).collect();
        assert_eq!(sizes, [1200, 1200, 1200, 1200, 1200, 1200, 1200, 1200, 410, 7]);
        // 10,000 is 0x2710: the low byte, then 0x27 as the varint of the rest.
        assert_eq!(packets[0][..10], [0x1a, 4, 0, 3, 0, 0, 0, 0x1b, 0x10, 0x27]);
        assert_eq!(packets[0][10..], bind[..1190]);
        assert_eq!(packets[1][..], bind[1190..2390]);
        assert_eq!(packets[8][..], bind[9590..]);
        assert_eq!(packets[9], [0x1c, 4, 0, 3, 0, 0, 0]);
        assert_eq!(packets.concat(), run);
        let read = client_reads(&packets).unwrap();
        let ids: Vec<usize> = read.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [26, 27, 28]);
        assert_eq!(read[1].1, bind);
        assert!(client_reads(&packets[..9]).unwrap().len() == 1, "the bind waits for the packet after its end");
    }

    #[test]
    fn a_run_that_ends_in_a_long_message_gets_an_empty_packet_to_dispatch_it() {
        let mut run = Vec::new();
        message(MESSAGE_BIND_BATCH_ADD_BIND, &[9; 2000], &mut run);
        let packets = packet_runs(&run, MAX_PACKET_MESSAGES);
        assert_eq!(packets.iter().map(Vec::len).collect::<Vec<_>>(), [1200, 803, 0]);
        assert_eq!(client_reads(&packets).unwrap(), [(27, vec![9; 2000])]);
        assert!(client_reads(&packets[..2]).unwrap().is_empty());
    }

    #[test]
    fn a_long_message_whose_header_does_not_fit_starts_the_next_packet() {
        let mut run = Vec::new();
        message(9, &[1; 1197], &mut run); // 1200 bytes: a full packet.
        message(MESSAGE_BIND_BATCH_ADD_BIND, &[2; 1500], &mut run);
        message(9, &[3; 10], &mut run);
        let packets = packet_runs(&run, MAX_PACKET_MESSAGES);
        assert_eq!(packets.iter().map(Vec::len).collect::<Vec<_>>(), [1200, 1200, 303, 13]);
        assert_eq!(packets[1][..3], [27, 0xdc, 5]);
        assert_eq!(client_reads(&packets).unwrap(), [(9, vec![1; 1197]), (27, vec![2; 1500]), (9, vec![3; 10])]);
    }

    #[test]
    fn a_run_that_fits_is_one_packet_as_it_is() {
        assert_eq!(packet_runs(&state_change(14), MAX_PACKET_MESSAGES), [state_change(14)]);
        assert_eq!(packet_runs(&[], MAX_PACKET_MESSAGES), [Vec::<u8>::new()]);
    }
}
