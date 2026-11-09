//! The client channel against a minimal server written here from the packet model (handshake, keys, GCM, the reliable layer):
//! the handshake and ack, in-order delivery of out-of-order and fragmented messages, resends with the resend flag, and
//! the unacknowledged packet that gives up. The real shard is exercised by `local_stack.rs`.

use std::time::Duration;

use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes128Gcm,
};
use blake2::{digest::consts::U16, Blake2s, Digest};
use sc_bot::{
    curve::{KeyPair, Point, KEY_LEN},
    nub::{Channel, Received, CONNECT_ACK, REPLICATION_ACK, REPLICATION_DATA},
    wire::{crc32c, frame_message},
};
use tokio::net::UdpSocket;

struct Server {
    socket: UdpSocket,
    send: Aes128Gcm,
    receive: Aes128Gcm,
    iv_prefix: [u8; 4],
    counter: u64,
    client: std::net::SocketAddr,
}

fn normalized(header: &[u8]) -> [u8; 8] {
    [header[0], header[1], header[2] & 7, header[3], 0, 0, 0, 0]
}

impl Server {
    /// Waits for the connect request on `socket`, answers it as the shard does and derives the keys. Returns the session
    /// the request named.
    async fn accept(socket: UdpSocket) -> (Server, Vec<u8>) {
        let mut buffer = [0u8; 2048];
        let (length, client) = socket.recv_from(&mut buffer).await.unwrap();
        let packet = &buffer[..length];
        let (rest, footer) = packet.split_at(packet.len() - 4);
        let (header, body) = rest.split_at(8);
        assert_eq!(crc32c(&[&normalized(header)[..], body].concat()).to_le_bytes(), footer, "the request's CRC");
        assert_eq!(header, [0x5c, 0, 4, 0xdc, 0xc5, 0xe7, 0x3e, 0x61], "a real connect request's header");
        assert_eq!(u32::from_le_bytes(body[..4].try_into().unwrap()), 0xdd3a_1ee5);
        let session_length = usize::from(body[4]);
        let session = body[5..5 + session_length].to_vec();
        let keys_at = 5 + session_length;
        let mut ack = Vec::new();
        let mut keys = [[0u8; 16]; 3];
        for (index, key) in keys.iter_mut().enumerate() {
            let peer = Point::parse(&body[keys_at + index * KEY_LEN..keys_at + (index + 1) * KEY_LEN]).expect("a key on the curve");
            let pair = KeyPair::generate();
            ack.extend_from_slice(&pair.public.encode());
            *key = Blake2s::<U16>::digest(pair.shared_secret(&peer).unwrap()).into();
        }
        ack.push(1);
        let mut reply = vec![0x5c, CONNECT_ACK, 4, 0xdc, 0xc5, 0xe7, 0x3e, 0x61];
        let crc = crc32c(&[&normalized(&reply)[..], &ack].concat());
        reply.extend_from_slice(&ack);
        reply.extend_from_slice(&crc.to_le_bytes());
        socket.send_to(&reply, client).await.unwrap();
        let server = Server {
            socket,
            send: Aes128Gcm::new(&keys[1].into()),
            receive: Aes128Gcm::new(&keys[0].into()),
            iv_prefix: keys[2][..4].try_into().unwrap(),
            counter: u64::from_le_bytes(keys[2][4..12].try_into().unwrap()),
            client,
        };
        (server, session)
    }

    fn iv(&self, counter: &[u8]) -> [u8; 12] {
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&self.iv_prefix);
        iv[4..].copy_from_slice(counter);
        iv
    }

    /// The next encrypted packet from the client: kind, flags and plaintext.
    async fn receive(&self) -> (u8, u8, Vec<u8>) {
        let mut buffer = [0u8; 2048];
        loop {
            let (length, _) = self.socket.recv_from(&mut buffer).await.unwrap();
            let packet = &buffer[..length];
            if packet[2] & 2 == 0 {
                continue;
            }
            let end = packet.len() - 24;
            let mut body = packet[8..end].to_vec();
            let (counter, tag) = packet[end..].split_at(8);
            self.receive.decrypt_in_place_detached(&self.iv(counter).into(), &normalized(packet), &mut body, <&[u8; 16]>::try_from(tag).unwrap().into()).expect("the client's packet opens");
            return (packet[1], packet[2], body);
        }
    }

    async fn send(&mut self, kind: u8, body: &[u8]) {
        let header = [0x5c, kind, 2, 0, 0, 0, 0, 0];
        let counter = self.counter.to_le_bytes();
        self.counter += 1;
        let mut packet = header.to_vec();
        packet.extend_from_slice(body);
        let tag = self.send.encrypt_in_place_detached(&self.iv(&counter).into(), &normalized(&header), &mut packet[8..]).unwrap();
        packet.extend_from_slice(&counter);
        packet.extend_from_slice(&tag);
        self.socket.send_to(&packet, self.client).await.unwrap();
    }

    /// A reliable packet as the shard writes one.
    async fn data(&mut self, sequence: u32, messages: &[u8]) {
        let mut body = Vec::new();
        for word in [0, sequence, sequence.wrapping_sub(1)] {
            body.extend_from_slice(&word.to_le_bytes());
        }
        body.extend_from_slice(messages);
        self.send(REPLICATION_DATA, &body).await;
    }

    /// An ack of the client's packet `sequence`, as the shard answers: base the sequence, one packet.
    async fn ack(&mut self, sequence: u32) {
        let mut ack = u32::MAX.to_le_bytes().to_vec();
        ack.extend(sequence.to_le_bytes());
        ack.extend(1u16.to_le_bytes());
        ack.push(1);
        self.send(REPLICATION_ACK, &ack).await;
    }
}

/// A server and a client connected to it.
async fn connected() -> (Server, Channel) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap().to_string();
    let client = tokio::spawn(async move { Channel::connect(&address, b"hub-test-session", Duration::from_secs(5)).await });
    let (server, session) = Server::accept(socket).await;
    assert_eq!(session, b"hub-test-session");
    (server, client.await.unwrap().expect("the client takes the ack"))
}

#[tokio::test]
async fn the_client_sends_packets_the_server_opens_and_the_ack_releases_them() {
    let (mut server, mut channel) = connected().await;
    let message = frame_message(6, &[3]);
    assert_eq!(channel.send_reliable(&message).await.unwrap(), 0);
    let (kind, flags, body) = server.receive().await;
    assert_eq!((kind, flags & 2), (REPLICATION_DATA, 2));
    // `[dropUpTo][sequence][blocking]` then the messages; the first packet has no blocking sequence.
    assert_eq!(body, [&[0u8; 4][..], &0u32.to_le_bytes(), &u32::MAX.to_le_bytes(), &message].concat());
    assert!(!channel.all_acked());
    server.ack(0).await;
    channel.poll(Duration::from_secs(2)).await.unwrap();
    assert!(channel.all_acked());
}

#[tokio::test]
async fn messages_come_out_in_order_whole_and_every_packet_is_acknowledged() {
    let (mut server, mut channel) = connected().await;
    let big = frame_message(27, &[9u8; 700]);
    let small = frame_message(6, &[14]);
    // Sequence 1 arrives first and holds the end of the big message and a small one; sequence 0 has the start.
    server.data(1, &[&big[400..], &small[..]].concat()).await;
    assert!(channel.poll(Duration::from_secs(2)).await.unwrap().is_empty(), "held until sequence 0 comes");
    server.data(0, &big[..400]).await;
    let received = channel.poll(Duration::from_secs(2)).await.unwrap();
    assert_eq!(received, vec![Received::Message(27, vec![9u8; 700]), Received::Message(6, vec![14])]);
    // Two acks: one per data packet, naming its own sequence.
    for expected in [1u32, 0] {
        let (kind, _, ack) = server.receive().await;
        assert_eq!(kind, REPLICATION_ACK);
        assert_eq!(u32::from_le_bytes(ack[4..8].try_into().unwrap()), expected);
        assert_eq!(u16::from_le_bytes(ack[8..10].try_into().unwrap()), 1);
    }
    // A repeat is acknowledged again and delivers nothing.
    server.data(0, &big[..400]).await;
    assert!(channel.poll(Duration::from_secs(2)).await.unwrap().is_empty());
    assert_eq!(server.receive().await.0, REPLICATION_ACK);
}

#[tokio::test]
async fn an_unacknowledged_packet_is_sent_again_with_the_resend_flag() {
    let (mut server, mut channel) = connected().await;
    channel.send_reliable(&frame_message(6, &[3])).await.unwrap();
    let (_, flags, first) = server.receive().await;
    assert_eq!(flags & 8, 0);
    tokio::time::sleep(Duration::from_millis(450)).await;
    channel.resend_due().await.unwrap();
    let (_, flags, again) = server.receive().await;
    assert_eq!((flags & 8, again), (8, first), "the same bytes, flagged");
    server.ack(0).await;
    channel.poll(Duration::from_secs(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    channel.resend_due().await.unwrap();
    assert!(channel.all_acked());
}
