#!/usr/bin/env python3
"""Checks that a ShardCluster opens shards as players join and closes them when they leave.

Simulated players join through the Hub and hold a real shard connection. Lowers the thresholds of the
`usa` ShardCluster for the test and restores them afterwards.

  tests/scale.py <client-files-dir> [--host H] [--players N]
"""
import argparse, json, math, os, socket, ssl, struct, subprocess, sys, threading, time

ap = argparse.ArgumentParser()
ap.add_argument("files")
ap.add_argument("--host", default="127.0.0.2"); ap.add_argument("--hub", type=int, default=8000)
ap.add_argument("--players", type=int, default=5)
ap.add_argument("--context", default="kind-scbackend")
args = ap.parse_args()
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from nub import NubChannel

PER_SHARD, WARM = 2, 1
RECONCILE_WAIT = 7  # two operator passes, so joins see the patched limit
KUBECTL = ["kubectl", "--context", args.context, "-n", "scbackend"]
failures = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f"  {detail}" if detail else ""), flush=True)
    if not ok:
        failures.append(name)


def cluster():
    out = subprocess.check_output(KUBECTL + ["get", "shc", "usa", "-o", "json"], text=True)
    return json.loads(out)


def patch(spec):
    subprocess.check_output(KUBECTL + ["patch", "shc", "usa", "--type=merge", "-p", json.dumps({"spec": spec})])


def wait_for(name, predicate, timeout):
    start = time.time()
    while time.time() - start < timeout:
        status = cluster().get("status", {})
        if predicate(status):
            check(name, True, f"after {time.time() - start:.0f}s: {status.get('readyShards')} shards, {status.get('players')} players")
            return status
        time.sleep(2)
    check(name, False, json.dumps(cluster().get("status")))
    return cluster().get("status", {})


def varint(v):
    out = b""
    while v >= 0x80:
        out += bytes([v & 0x7f | 0x80]); v >>= 7
    return out + bytes([v])


def ld(field, data):
    return varint(field << 3 | 2) + varint(len(data)) + data


def fields(b):
    i, out = 0, {}
    def rv(i):
        v = s = 0
        while True:
            c = b[i]; i += 1; v |= (c & 0x7f) << s; s += 7
            if not c & 0x80:
                return v, i
    while i < len(b):
        t, i = rv(i)
        if t & 7 == 0:
            v, i = rv(i)
        else:
            n, i = rv(i); v = b[i:i + n]; i += n
        out.setdefault(t >> 3, []).append(v)
    return out


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT); ctx.check_hostname = False
ctx.load_verify_locations(args.files + "/client.crt")


def join():
    """Asks the Hub for a shard, as the game does."""
    hub = ctx.wrap_socket(socket.create_connection((args.host, args.hub), timeout=10))
    body = ld(1, b"cmsg_ls_goto_loc") + ld(2, bytes.fromhex("08818084809080c001")) + varint(3 << 3) + varint(0)
    hub.sendall(struct.pack("<II", len(body), 0xDEADBEEF) + body)
    def exact(n):
        data = b""
        while len(data) < n:
            data += hub.recv(n - len(data))
        return data
    length, _ = struct.unpack("<II", exact(8))
    envelope = fields(exact(length))
    hub.close()
    update = fields(envelope[2][0])
    return update[3][0].decode(), update[4][0], update[7][0].decode()


def connect(ip, port, guid, shard):
    """Repeats the connect from one source port, as the game does: it is a single UDP packet."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind(("", 0))
        local_port = probe.getsockname()[1]
    start = time.time()
    for attempt in range(1, 16):
        channel = NubChannel(remote=(ip, port), local_port=local_port)
        channel.settimeout(2)
        try:
            channel.handshake(session_guid=guid)
            print(f"connected to {shard} on attempt {attempt} after {time.time() - start:.1f}s", flush=True)
            return channel
        except TimeoutError:
            channel.close()
    raise TimeoutError(f"{shard} at {ip}:{port} never answered")


stop = threading.Event()
channels = []


def keepalive():
    ping = bytes([0x5C, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    while not stop.wait(5):
        for channel in channels:
            channel._sock.sendto(ping, channel.remote)


original = cluster()["spec"]
restore = {k: original[k] for k in ("playersPerShard", "scaleDownDelaySeconds", "warmShards")}
try:
    patch({"playersPerShard": PER_SHARD, "scaleDownDelaySeconds": 10, "warmShards": WARM})
    time.sleep(RECONCILE_WAIT)
    wait_for("starts with one warm shard", lambda s: s.get("readyShards") == WARM and s.get("players") == 0, 60)

    assigned = []
    threading.Thread(target=keepalive, daemon=True).start()
    for n in range(args.players):
        # A join can arrive before a new shard is ready, as the game's retry would handle.
        for _ in range(30):
            try:
                ip, port, shard = join()
                break
            except (KeyError, IndexError, OSError):
                time.sleep(1)
        channels.append(connect(ip, port, bytes([n + 1]) * 16, shard))
        assigned.append(shard)
    print("assigned:", assigned, flush=True)
    check("no shard holds more than its limit", all(assigned.count(s) <= PER_SHARD for s in assigned))

    want = math.ceil(args.players / PER_SHARD) + WARM
    status = wait_for(f"scales up to {want} shards", lambda s: s.get("readyShards") == want and s.get("players") == args.players, 60)
    ids = [s["id"] for s in status.get("shards", [])]
    check("shard ids are unique and well formed", len(set(ids)) == len(ids) and all(i.startswith("pub_usa_") for i in ids), str(ids))

    stop.set()
    wait_for("players time out", lambda s: s.get("players") == 0, 90)
    wait_for("scales back to the warm shard", lambda s: s.get("readyShards") == WARM, 90)
finally:
    stop.set()
    patch(restore)
print("\nALL PASSED" if not failures else f"\n{len(failures)} FAILED: {failures}")
sys.exit(1 if failures else 0)
