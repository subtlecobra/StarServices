"""The CryNub connect handshake as the real client performs it: secp128r1 keys as `04 || X || Y`, and a
CRC-32C over the header with bytes 4 to 7 cleared. Checked against packets captured from StarCitizen.exe."""
import hashlib, secrets, socket, struct

P = 0xFFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFF
A = 0xFFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFC
B = 0xE87579C11079F43DD824993C2CEE5ED3
G = (0x161FF7528B899B2D0C28607CA52C5B86, 0xCF5AC8395BAFEB13C02DA292DDED7A83)
HEADER = bytes.fromhex("5c0004dcc5e73e61")
REQUEST_MAGIC = 0xDD3A1EE5
KEY_LEN = 33


def add(p, q):
    if p is None or q is None:
        return p or q
    if p[0] == q[0]:
        if (p[1] + q[1]) % P == 0:
            return None
        slope = (3 * p[0] * p[0] + A) * pow(2 * p[1], -1, P) % P
    else:
        slope = (q[1] - p[1]) * pow(q[0] - p[0], -1, P) % P
    x = (slope * slope - p[0] - q[0]) % P
    return x, (slope * (p[0] - x) - p[1]) % P


def multiply(k, point):
    result = None
    while k:
        if k & 1:
            result = add(result, point)
        point = add(point, point)
        k >>= 1
    return result


def encode(point):
    return b"\x04" + point[0].to_bytes(16, "big") + point[1].to_bytes(16, "big")


def decode(key):
    x, y = int.from_bytes(key[1:17], "big"), int.from_bytes(key[17:33], "big")
    if key[0] != 4 or (y * y - (x ** 3 + A * x + B)) % P:
        raise ValueError("not a secp128r1 point")
    return x, y


def crc32c(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def sealed(header, body):
    covered = header[:4] + bytes(4) + body
    return header + body + struct.pack("<I", crc32c(covered))


def authenticated(header):
    return header[:2] + bytes([header[2] & 7, header[3]]) + bytes(4)


def encrypt(key, iv_prefix, counter, header, body):
    """`[header][ciphertext][counter][tag]`: AES-128-GCM over the body, IV = prefix || counter."""
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM

    counter_bytes = struct.pack("<Q", counter)
    sealed_body = AESGCM(key).encrypt(iv_prefix + counter_bytes, body, authenticated(header))
    return header + sealed_body[:-16] + counter_bytes + sealed_body[-16:]


def decrypt(key, iv_prefix, packet):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM

    body, counter, tag = packet[8:-24], packet[-24:-16], packet[-16:]
    return AESGCM(key).decrypt(iv_prefix + counter, body + tag, authenticated(packet[:8]))


def read_varint(data, offset):
    value = shift = 0
    while True:
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if byte < 0x80:
            return value, offset


def split_messages(data):
    """`[varint id][u8 length low][varint length high][payload]`, as (id, payload) pairs."""
    messages, offset = [], 0
    while offset < len(data):
        message_id, offset = read_varint(data, offset)
        low = data[offset]
        high, offset = read_varint(data, offset + 1)
        length = high << 8 | low
        messages.append((message_id, data[offset:offset + length]))
        offset += length
    return messages


def message(message_id, payload):
    assert message_id < 0x80 and len(payload) < 0x8000
    return bytes([message_id, len(payload) & 0xFF, len(payload) >> 8]) + payload


PING_HEADER = bytes.fromhex("5c03040000000000")
DATA_HEADER = bytes.fromhex("5c07020000000000")
RESEND_FLAG = 0x08
NO_SEQUENCE = 0xFFFFFFFF


class NubChannel:
    def __init__(self, remote, local_port=0):
        self.remote, self.keys, self.connected = remote, [], False
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        if local_port:
            self._sock.bind(("", local_port))
        self._sock.settimeout(10)

    def settimeout(self, seconds):
        self._sock.settimeout(seconds)

    def close(self):
        self._sock.close()

    def handshake(self, session_guid=b""):
        pairs = [secrets.randbits(127) | 2 for _ in range(3)]
        body = struct.pack("<I", REQUEST_MAGIC) + bytes([len(session_guid)]) + session_guid
        body += b"".join(encode(multiply(k, G)) for k in pairs)
        self._sock.sendto(sealed(HEADER, body), self.remote)
        ack, _ = self._sock.recvfrom(2048)
        if ack[1] != 1 or sealed(ack[:8], ack[8:-4]) != ack:
            raise ValueError("bad connect ack")
        peers = [decode(ack[8 + KEY_LEN * i:8 + KEY_LEN * (i + 1)]) for i in range(3)]
        secrets_x = [multiply(k, peer)[0].to_bytes(16, "big") for k, peer in zip(pairs, peers)]
        self.keys = [hashlib.blake2s(secret, digest_size=16).digest() for secret in secrets_x]
        self.connected = True

    def ping(self, time=0x1122334455):
        """Returns the time the shard's pong echoes."""
        self._sock.sendto(sealed(PING_HEADER, struct.pack("<Q", time)), self.remote)
        pong, _ = self._sock.recvfrom(2048)
        if pong[1] != 4 or sealed(pong[:8], pong[8:-4]) != pong:
            raise ValueError("bad pong")
        return struct.unpack("<Q", pong[8:16])[0]

    def replicate(self, sequence, messages=b"", resend=False):
        """Sends a ReplicationData packet as a client does. Returns the ack's four fields."""
        iv_prefix = self.keys[2][:4]
        counter = struct.unpack("<Q", self.keys[2][4:12])[0] + sequence
        header = bytearray(DATA_HEADER)
        header[2] |= RESEND_FLAG if resend else 0
        body = struct.pack("<III", 0, sequence, NO_SEQUENCE) + messages
        self._sock.sendto(encrypt(self.keys[0], iv_prefix, counter, bytes(header), body), self.remote)
        ack, _ = self._sock.recvfrom(2048)
        if ack[1] != 8:
            raise ValueError("bad replication ack")
        body = decrypt(self.keys[1], iv_prefix, ack)
        return (*struct.unpack("<IIH", body[:10]), body[10])

    def receive_reliable(self):
        """Waits for the shard's next reliable packet and acknowledges it. Returns its sequence and messages."""
        while True:
            packet, _ = self._sock.recvfrom(2048)
            if packet[1] == 7:
                break
        iv_prefix = self.keys[2][:4]
        body = decrypt(self.keys[1], iv_prefix, packet)
        sequence = struct.unpack("<I", body[4:8])[0]
        self._acks = getattr(self, "_acks", 0) + 1
        counter = struct.unpack("<Q", self.keys[2][4:12])[0] + 0x10000 + self._acks
        ack = struct.pack("<IIHB", sequence, sequence, 1, 1)
        header = bytes.fromhex("5c08020000000000")
        self._sock.sendto(encrypt(self.keys[0], iv_prefix, counter, header, ack), self.remote)
        return sequence, split_messages(body[12:])
