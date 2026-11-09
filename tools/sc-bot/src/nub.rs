//! The CryNub UDP channel from the client's side: the connect handshake, the encrypted packets, and the reliable layer
//! (ReplicationData 7 / ReplicationAck 8) with its acknowledgements and resends. The shard's side of the same code is
//! `services/pu-frontend/src/{shard,channel}.rs`; the packet model is the RE repository's `crynub_protocol.md`.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes128Gcm,
};
use blake2::{digest::consts::U16, Blake2s, Digest};
use tokio::net::UdpSocket;

use crate::{
    curve::{KeyPair, Point, KEY_LEN},
    wire::{crc32c, MessageStream},
};

pub const HEADER_LEN: usize = 8;
const MAGIC: u8 = 0x5c;
pub const CONNECT_REQUEST: u8 = 0;
pub const CONNECT_ACK: u8 = 1;
pub const PING: u8 = 3;
pub const PONG: u8 = 4;
pub const DISCONNECT: u8 = 5;
pub const DISCONNECT_ACK: u8 = 6;
pub const REPLICATION_DATA: u8 = 7;
pub const REPLICATION_ACK: u8 = 8;
const FLAG_COMPRESSED: u8 = 0x01;
const FLAG_ENCRYPTED: u8 = 0x02;
const FLAG_PLAINTEXT: u8 = 0x04;
const FLAG_RESEND: u8 = 0x08;
const REQUEST_MAGIC: u32 = 0xdd3a_1ee5;
const REQUEST_METADATA_BYTE: u8 = 0xdc;
const REQUEST_METADATA_U32: u32 = 0x613e_e7c5;
const COUNTER_LEN: usize = 8;
const TAG_LEN: usize = 16;
const MAX_BODY: usize = 0x570;
/// The most message bytes one packet of ours carries (the server's own limit for its packets).
pub const MAX_PACKET_MESSAGES: usize = 1200;
const RESEND_AFTER: Duration = Duration::from_millis(400);
const MAX_TRIES: u32 = 40;
const NO_SEQUENCE: u32 = u32::MAX;

/// The header as the CRC and the GCM tag see it: byte 2 masked to 7, byte 3 kept, bytes 4 to 7 cleared.
fn normalized(header: &[u8]) -> [u8; HEADER_LEN] {
    [header[0], header[1], header[2] & 0x07, header[3], 0, 0, 0, 0]
}

fn packet_crc(header: &[u8], body: &[u8]) -> u32 {
    crc32c(&[&normalized(header)[..], body].concat())
}

/// A plaintext packet with its CRC-32C footer.
pub fn plaintext_packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let header = [MAGIC, kind, FLAG_PLAINTEXT, 0, 0, 0, 0, 0];
    [&header[..], body, &packet_crc(&header, body).to_le_bytes()].concat()
}

/// The body of a plaintext packet whose CRC holds.
pub fn plaintext_body(packet: &[u8]) -> Option<&[u8]> {
    let end = packet.len().checked_sub(4).filter(|end| *end >= HEADER_LEN)?;
    let (rest, footer) = packet.split_at(end);
    let (header, body) = rest.split_at(HEADER_LEN);
    (header[0] == MAGIC && header[2] & FLAG_PLAINTEXT != 0 && packet_crc(header, body).to_le_bytes() == footer).then_some(body)
}

fn derive_key(secret: &[u8]) -> [u8; 16] {
    Blake2s::<U16>::digest(secret).into()
}

/// The disconnect notice a client sends (`BuildDisconnectPacket`): `[u32 reason][0x30-byte trace id][text]`, plaintext.
pub fn disconnect_notice(reason: u32, text: &str) -> Vec<u8> {
    let mut body = reason.to_le_bytes().to_vec();
    body.extend_from_slice(&[0; 0x30]);
    body.extend(text.bytes().map(|byte| match byte {
        b'"' => b'\'',
        b'%' | b'\\' => b'#',
        0x20..=0x7e => byte,
        _ => b'*',
    }));
    plaintext_packet(DISCONNECT, &body)
}

/// The keys of one channel, client side: key 0 encrypts what we send, key 1 what we receive, key 2 gives the IV.
struct Keys {
    send: Aes128Gcm,
    receive: Aes128Gcm,
    iv_prefix: [u8; 4],
    counter: u64,
}

impl Keys {
    fn new(keys: &[[u8; 16]; 3]) -> Self {
        Self {
            send: Aes128Gcm::new(&keys[0].into()),
            receive: Aes128Gcm::new(&keys[1].into()),
            iv_prefix: keys[2][..4].try_into().unwrap(),
            counter: u64::from_le_bytes(keys[2][4..12].try_into().unwrap()),
        }
    }

    fn iv(&self, counter: &[u8]) -> [u8; 12] {
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&self.iv_prefix);
        iv[4..].copy_from_slice(counter);
        iv
    }

    /// `[header][ciphertext][u64 counter][16-byte tag]`.
    fn seal(&mut self, header: [u8; HEADER_LEN], body: &[u8]) -> Vec<u8> {
        let counter = self.counter.to_le_bytes();
        self.counter = self.counter.wrapping_add(1);
        let mut packet = header.to_vec();
        packet.extend_from_slice(body);
        let tag = self.send.encrypt_in_place_detached(&self.iv(&counter).into(), &normalized(&header), &mut packet[HEADER_LEN..]).expect("GCM seals");
        packet.extend_from_slice(&counter);
        packet.extend_from_slice(&tag);
        packet
    }

    /// The plaintext of an encrypted packet from the server, decompressed when it was compressed.
    fn open(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let end = packet.len().checked_sub(COUNTER_LEN + TAG_LEN)?;
        if end <= HEADER_LEN {
            return None;
        }
        let (counter, tag) = packet[end..].split_at(COUNTER_LEN);
        let mut body = packet[HEADER_LEN..end].to_vec();
        self.receive.decrypt_in_place_detached(&self.iv(counter).into(), &normalized(packet), &mut body, <&[u8; TAG_LEN]>::try_from(tag).ok()?.into()).ok()?;
        if packet[2] & FLAG_COMPRESSED == 0 {
            return Some(body);
        }
        lz4_flex::block::decompress(&body, MAX_BODY + 0x18).ok()
    }
}

/// What a received datagram means to the session.
#[derive(Debug, PartialEq)]
pub enum Received {
    /// A whole message of the reliable layer, in order.
    Message(u32, Vec<u8>),
    /// A pong: the time we sent and the server's clock (100 ns ticks since 1970).
    Pong { sent: u64, server_clock: u64 },
    /// The answer to our disconnect notice.
    DisconnectAck,
    /// The server ended the connection: reason code and text.
    Disconnect { reason: u32, text: String },
    /// A packet the channel did not understand.
    Unknown(u8),
}

struct Pending {
    packet: Vec<u8>,
    sent: Instant,
    tries: u32,
}

/// A connected client channel with the reliable layer on top.
pub struct Channel {
    socket: UdpSocket,
    keys: Keys,
    next_sequence: u32,
    last_sequence: Option<u32>,
    unacked: BTreeMap<u32, Pending>,
    /// The packets received, kept until all before them have: `(sequence)` to the message area.
    held: BTreeMap<u32, Vec<u8>>,
    expected: u32,
    last_consumed: u32,
    stream: MessageStream,
    pub packets_sent: u64,
    pub packets_received: u64,
}

/// Whether a ReplicationAck body `[u32 lastConsumed][u32 base][u16 count]...` covers `sequence`.
fn acknowledges(ack: &[u8], sequence: u32) -> bool {
    let word = |at: usize| ack.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let (Some(consumed), Some(base)) = (word(0), word(4)) else { return false };
    let count = ack.get(8..10).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    let after = |left: u32, right: u32| (left.wrapping_sub(right) as i32) > 0;
    (consumed != u32::MAX && !after(sequence, consumed)) || (count > 0 && sequence.wrapping_sub(base) < u32::from(count))
}

impl Channel {
    /// Sends the connect request every 400 ms until the shard's ack arrives or `timeout` passes. `session` is the Hub's
    /// session id.
    pub async fn connect(remote: &str, session: &[u8], timeout: Duration) -> Result<Channel, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| format!("bind: {e}"))?;
        socket.connect(remote).await.map_err(|e| format!("connect {remote}: {e}"))?;
        let pairs: Vec<KeyPair> = (0..3).map(|_| KeyPair::generate()).collect();
        let mut body = REQUEST_MAGIC.to_le_bytes().to_vec();
        crate::wire::put_varint(&mut body, session.len() as u64);
        body.extend_from_slice(session);
        for pair in &pairs {
            body.extend_from_slice(&pair.public.encode());
        }
        let mut header = vec![MAGIC, CONNECT_REQUEST, FLAG_PLAINTEXT, REQUEST_METADATA_BYTE];
        header.extend_from_slice(&REQUEST_METADATA_U32.to_le_bytes());
        let request = [&header[..], &body, &packet_crc(&header, &body).to_le_bytes()].concat();

        let started = Instant::now();
        let mut buffer = [0u8; 2048];
        while started.elapsed() < timeout {
            socket.send(&request).await.map_err(|e| format!("send: {e}"))?;
            let wait = RESEND_AFTER.min(timeout.saturating_sub(started.elapsed()));
            let Ok(Ok(length)) = tokio::time::timeout(wait, socket.recv(&mut buffer)).await else { continue };
            let packet = &buffer[..length];
            if packet.len() < HEADER_LEN || packet[1] != CONNECT_ACK {
                continue;
            }
            let Some(body) = plaintext_body(packet) else { continue };
            if body.len() < KEY_LEN * 3 + 1 {
                return Err("the connect ack is too short".into());
            }
            let mut keys = [[0u8; 16]; 3];
            for (index, key) in keys.iter_mut().enumerate() {
                let peer = Point::parse(&body[index * KEY_LEN..(index + 1) * KEY_LEN]).ok_or("the shard's key is not on the curve")?;
                *key = derive_key(&pairs[index].shared_secret(&peer).ok_or("no shared secret")?);
            }
            return Ok(Channel {
                socket,
                keys: Keys::new(&keys),
                next_sequence: 0,
                last_sequence: None,
                unacked: BTreeMap::new(),
                held: BTreeMap::new(),
                expected: 0,
                last_consumed: u32::MAX,
                stream: MessageStream::default(),
                packets_sent: 0,
                packets_received: 0,
            });
        }
        Err(format!("no connect ack from {remote} in {:?}", timeout))
    }

    /// Sends `messages` (already framed) as the next reliable packet, kept for resending until acknowledged.
    pub async fn send_reliable(&mut self, messages: &[u8]) -> Result<u32, String> {
        if messages.len() > MAX_PACKET_MESSAGES {
            return Err(format!("{} message bytes do not fit one packet", messages.len()));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let mut body = Vec::with_capacity(12 + messages.len());
        for word in [0, sequence, self.last_sequence.unwrap_or(NO_SEQUENCE)] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        body.extend_from_slice(messages);
        self.last_sequence = Some(sequence);
        let packet = self.keys.seal([MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED, 0, 0, 0, 0, 0], &body);
        self.socket.send(&packet).await.map_err(|e| format!("send: {e}"))?;
        self.packets_sent += 1;
        self.unacked.insert(sequence, Pending { packet, sent: Instant::now(), tries: 1 });
        Ok(sequence)
    }

    /// Whether every reliable packet we sent is acknowledged.
    pub fn all_acked(&self) -> bool {
        self.unacked.is_empty()
    }

    /// Sends again, with the resend flag, what has gone unacknowledged for too long. Fails when a packet is never answered
    /// (the real client disconnects with 0x7543 then).
    pub async fn resend_due(&mut self) -> Result<(), String> {
        let now = Instant::now();
        for (sequence, pending) in &mut self.unacked {
            if now.duration_since(pending.sent) < RESEND_AFTER {
                continue;
            }
            if pending.tries >= MAX_TRIES {
                return Err(format!("reliable packet {sequence} failed to deliver after {} tries", pending.tries));
            }
            pending.tries += 1;
            pending.sent = now;
            pending.packet[2] |= FLAG_RESEND;
            self.socket.send(&pending.packet).await.map_err(|e| format!("send: {e}"))?;
        }
        Ok(())
    }

    pub async fn send_ping(&mut self, time: u64) -> Result<(), String> {
        self.socket.send(&plaintext_packet(PING, &time.to_le_bytes())).await.map_err(|e| format!("send: {e}"))?;
        Ok(())
    }

    pub async fn send_disconnect(&mut self, reason: u32, text: &str) -> Result<(), String> {
        self.socket.send(&disconnect_notice(reason, text)).await.map_err(|e| format!("send: {e}"))?;
        Ok(())
    }

    async fn send_ack(&mut self, header: (u32, u32, u32), resent: bool) -> Result<(), String> {
        let (drop_up_to, sequence, _blocking) = header;
        let dropped = drop_up_to.wrapping_sub(1);
        if (dropped.wrapping_sub(self.last_consumed) as i32) > 0 {
            self.last_consumed = dropped;
        }
        if sequence == self.last_consumed.wrapping_add(1) {
            self.last_consumed = sequence;
        }
        let mut ack = [0u8; 11];
        ack[..4].copy_from_slice(&self.last_consumed.to_le_bytes());
        ack[4..8].copy_from_slice(&sequence.to_le_bytes());
        ack[8..10].copy_from_slice(&1u16.to_le_bytes());
        ack[10] = 1 | if resent { 2 } else { 0 };
        let packet = self.keys.seal([MAGIC, REPLICATION_ACK, FLAG_ENCRYPTED, 0, 0, 0, 0, 0], &ack);
        self.socket.send(&packet).await.map_err(|e| format!("send: {e}"))?;
        Ok(())
    }

    /// Waits up to `wait` for one datagram and handles it. Returns what it meant: the messages a data packet completed,
    /// in order, or a pong, a disconnect and so on. An empty list is a datagram that carried nothing for the session
    /// (an ack, a repeat), or the wait running out.
    pub async fn poll(&mut self, wait: Duration) -> Result<Vec<Received>, String> {
        let mut buffer = [0u8; 4096];
        let length = match tokio::time::timeout(wait, self.socket.recv(&mut buffer)).await {
            Err(_) => return Ok(Vec::new()),
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Ok(Ok(length)) => length,
        };
        self.packets_received += 1;
        let packet = &buffer[..length];
        if packet.len() < HEADER_LEN || packet[0] != MAGIC {
            return Ok(Vec::new());
        }
        let (kind, flags) = (packet[1], packet[2]);
        match kind {
            PONG => {
                let Some(body) = plaintext_body(packet).filter(|body| body.len() == 16) else { return Ok(Vec::new()) };
                let sent = u64::from_le_bytes(body[..8].try_into().unwrap());
                let server_clock = u64::from_le_bytes(body[8..].try_into().unwrap());
                Ok(vec![Received::Pong { sent, server_clock }])
            }
            DISCONNECT_ACK => Ok(vec![Received::DisconnectAck]),
            DISCONNECT => {
                let Some(body) = plaintext_body(packet) else { return Ok(Vec::new()) };
                let reason = body.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
                let text = String::from_utf8_lossy(body.get(4 + 0x30..).unwrap_or_default()).into_owned();
                self.socket.send(&plaintext_packet(DISCONNECT_ACK, &[])).await.map_err(|e| format!("send: {e}"))?;
                Ok(vec![Received::Disconnect { reason, text }])
            }
            REPLICATION_ACK if flags & FLAG_ENCRYPTED != 0 => {
                if let Some(body) = self.keys.open(packet) {
                    self.unacked.retain(|sequence, _| !acknowledges(&body, *sequence));
                }
                Ok(Vec::new())
            }
            REPLICATION_DATA if flags & FLAG_ENCRYPTED != 0 => {
                let Some(body) = self.keys.open(packet) else { return Ok(Vec::new()) };
                if body.len() < 12 {
                    return Ok(Vec::new());
                }
                let word = |index: usize| u32::from_le_bytes(body[index * 4..index * 4 + 4].try_into().unwrap());
                let header = (word(0), word(1), word(2));
                self.send_ack(header, flags & FLAG_RESEND != 0).await?;
                let sequence = header.1;
                if (sequence.wrapping_sub(self.expected) as i32) < 0 {
                    return Ok(Vec::new());
                }
                self.held.entry(sequence).or_insert_with(|| body[12..].to_vec());
                let mut received = Vec::new();
                while let Some(area) = self.held.remove(&self.expected) {
                    self.expected = self.expected.wrapping_add(1);
                    self.stream.push(&area);
                    while let Some((id, payload)) = self.stream.next_message() {
                        received.push(Received::Message(id, payload));
                    }
                }
                Ok(received)
            }
            other => Ok(vec![Received::Unknown(other)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plaintext_packet_carries_a_crc_over_its_normalized_header() {
        let packet = plaintext_packet(PING, &7u64.to_le_bytes());
        assert_eq!(&packet[..8], &[0x5c, 3, 4, 0, 0, 0, 0, 0]);
        assert_eq!(plaintext_body(&packet), Some(&7u64.to_le_bytes()[..]));
        let mut broken = packet.clone();
        broken[9] ^= 1;
        assert_eq!(plaintext_body(&broken), None);
    }

    /// The real connect request of `services/pu-frontend/src/shard.rs` (`REAL_REQUEST`) is checked there; here the header
    /// rule is: the resend flag is outside what the CRC and the tag cover.
    #[test]
    fn the_resend_flag_is_not_authenticated() {
        let mut keys = Keys::new(&[[1; 16], [2; 16], [3; 16]]);
        let mut packet = keys.seal([MAGIC, REPLICATION_DATA, FLAG_ENCRYPTED, 0, 0, 0, 0, 0], b"hello");
        assert_eq!(keys.open(&packet), None, "a client cannot open what it sealed: the server's receive key is its send key");
        packet[2] |= FLAG_RESEND;
        let mirror = Keys { send: Aes128Gcm::new(&[2u8; 16].into()), receive: Aes128Gcm::new(&[1u8; 16].into()), iv_prefix: [3; 4], counter: 0 };
        assert_eq!(mirror.open(&packet).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn an_ack_covers_what_it_names_and_what_was_consumed() {
        let ack = |consumed: u32, base: u32, count: u16| {
            let mut bytes = consumed.to_le_bytes().to_vec();
            bytes.extend(base.to_le_bytes());
            bytes.extend(count.to_le_bytes());
            bytes.push(1);
            bytes
        };
        assert!(acknowledges(&ack(u32::MAX, 3, 1), 3));
        assert!(!acknowledges(&ack(u32::MAX, 3, 1), 4));
        assert!(acknowledges(&ack(5, 6, 0), 4));
    }

    #[test]
    fn the_disconnect_notice_is_the_clients_own_form() {
        let packet = disconnect_notice(0x7540, "User closed \"the\" 100% app");
        let body = plaintext_body(&packet).unwrap();
        assert_eq!(&body[..4], &0x7540u32.to_le_bytes());
        assert_eq!(&body[4 + 0x30..], b"User closed 'the' 100# app");
        assert_eq!(packet[1], DISCONNECT);
    }
}
