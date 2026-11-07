#!/usr/bin/env python3
"""End-to-end check of a running scbackend with an independent client stack.

Follows the order in a real Game.log: Hub TLS connect, gRPC Listen and subscribe, InitiateLogin,
login pushes, GetCurrentPlayer, Hub account_login, destination list, join, shard handshake.

  pip install grpcio protobuf
  tests/e2e.py <client-files-dir> [--grpc P --hub P --shard P]
"""
import argparse, base64, json, os, queue, socket, ssl, struct, sys, threading, time
import grpc
from google.protobuf import descriptor_pb2, descriptor_pool, message_factory

ap = argparse.ArgumentParser()
ap.add_argument("files")
ap.add_argument("--descriptors", required=True, help="output of scbackend dump-descriptors")
ap.add_argument("--host", default="127.0.0.2")
ap.add_argument("--grpc", type=int, default=50051); ap.add_argument("--hub", type=int, default=8000)
ap.add_argument("--shard", type=int, default=64090)
args = ap.parse_args()
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from nub import NubChannel

pool = descriptor_pool.DescriptorPool()
fds = descriptor_pb2.FileDescriptorSet(); fds.ParseFromString(open(args.descriptors, "rb").read())
for f in fds.file:
    pool.Add(f)
def M(name): return message_factory.GetMessageClass(pool.FindMessageTypeByName(name))
P, L, I = "sc.external.services.push.v1", "sc.external.services.login.v1", "sc.external.services.identity.v1"
C = "sc.external.services.configuration.v1"

login = json.load(open(args.files + "/LoginData.json"))
token = login["auth_token"]
meta = [("authorization", "Bearer " + token)]
failures = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f"  {detail}" if detail else ""))
    if not ok:
        failures.append(name)

# Hub, verified the way the client does: chain to client.crt, host name not checked.
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT); ctx.check_hostname = False
ctx.load_verify_locations(args.files + "/client.crt"); ctx.verify_mode = ssl.CERT_REQUIRED
hub = ctx.wrap_socket(socket.create_connection((args.host, args.hub), timeout=5))
check("hub TLS chains to client.crt", True, hub.version())

def varint(v):
    out = b""
    while v >= 0x80:
        out += bytes([v & 0x7f | 0x80]); v >>= 7
    return out + bytes([v])
def ld(field, data): return varint(field << 3 | 2) + varint(len(data)) + data
def hub_send(key, payload=b"", id=0):
    body = ld(1, key.encode()) + ld(2, payload) + varint(3 << 3) + varint(id)
    hub.sendall(struct.pack("<II", len(body), 0xDEADBEEF) + body)
def read_exact(n):
    data = b""
    while len(data) < n:
        chunk = hub.recv(n - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data
Env = None
def rv(b, i):
    v = s = 0
    while True:
        c = b[i]; i += 1; v |= (c & 0x7f) << s; s += 7
        if not c & 0x80:
            return v, i
def fields(b):
    i, out = 0, {}
    while i < len(b):
        t, i = rv(b, i)
        if t & 7 == 0:
            v, i = rv(b, i)
        else:
            n, i = rv(b, i); v = b[i:i + n]; i += n
        out.setdefault(t >> 3, []).append(v)
    return out
def hub_recv():
    length, magic = struct.unpack("<II", read_exact(8))
    assert magic == 0xDEADBEEF
    f = fields(read_exact(length))
    return f[1][0].decode(), f.get(2, [b""])[0], f.get(3, [0])[0]

# The gRPC client does check the name, like the game: the target is the bare IP.
roots = open(args.files + "/client.crt", "rb").read()
chan = grpc.secure_channel(f"{args.host}:{args.grpc}", grpc.ssl_channel_credentials(root_certificates=roots))
check("LoginData endpoint matches", login["star_network"]["servicesEndpoint"] == f"https://{args.host}:{args.grpc}")
def unary(service, method, req, resp):
    call = chan.unary_unary(f"/{service}/{method}", request_serializer=lambda m: m.SerializeToString(),
                            response_deserializer=resp.FromString)
    return call(req, metadata=meta, timeout=5)

Config = M(C + ".QueryConfigResponse")
q = M(C + ".QueryConfigRequest")(); q.query.config_group = "grpc"; q.query.config_key = "endpoints"
r = unary(C + ".ConfigService", "QueryConfig", q, Config)
check("QueryConfig reports not found", r.response.result == 3 or "NOT_FOUND" in str(r), str(r.response.result))

# Bidi Listen with a real subscribe.
LReq, LResp = M(P + ".ListenRequest"), M(P + ".ListenResponse")
# Topic strings as sent by the real client (scbackend.log from a live run).
claims = token.split(".")[1]
account_id = json.loads(base64.urlsafe_b64decode(claims + "=" * (-len(claims) % 4)))["account_id"]
topics = [f"account/{account_id}/login", "account/+/presence", f"account/{account_id}/#"]
outbound = queue.Queue(); inbound = queue.Queue()
sub = LReq(); sub.commands.add().subscribe.topics.extend(topics)
outbound.put(sub)
listen = chan.stream_stream(f"/{P}.PushService/Listen", request_serializer=lambda m: m.SerializeToString(),
                            response_deserializer=LResp.FromString)
stream = listen(iter(outbound.get, None), metadata=meta)
threading.Thread(target=lambda: [inbound.put(x) for x in stream], daemon=True).start()
def next_push():
    return inbound.get(timeout=8)
first = next_push().envelopes[0].messages[0]
check("first Listen message is STREAM_OPENED", first.control.command == 1)
second = next_push().envelopes[0].messages[0]
check("subscribe acknowledged", list(second.control.subscription_event.subscribed) == topics)

# Unauthenticated login must fail.
Req, Resp = M(L + ".InitiateLoginRequest"), M(L + ".InitiateLoginResponse")
try:
    chan.unary_unary(f"/{L}.LoginService/InitiateLogin", request_serializer=lambda m: m.SerializeToString(),
                     response_deserializer=Resp.FromString)(Req(session_id="x"), timeout=5)
    check("login without a token rejected", False)
except grpc.RpcError as e:
    check("login without a token rejected", e.code() == grpc.StatusCode.UNAUTHENTICATED)

r = unary(L + ".LoginService", "InitiateLogin", Req(session_id="sess-1"), Resp)
check("InitiateLogin", r.nickname == login["username"] and r.status.login_request_id == "sess-1", r.nickname)

env = next_push().envelopes[0]
status = M(L + ".AccountLoginCharacterStatus")(); env.messages[0].any.Unpack(status)
check("character status push", status.players[0].name == login["username"] and status.players[0].state == 1,
      f"geid={status.players[0].geid}")
check("pushed on the login topic", list(env.topics) == [f"account/{account_id}/login"], env.topics[0])
env = next_push().envelopes[0]
check("LoginCompleted push", env.messages[0].any.type_url.endswith(".LoginCompleted"))

g = unary(I + ".IdentityService", "GetCurrentPlayer", M(I + ".GetCurrentPlayerRequest")(), M(I + ".GetCurrentPlayerResponse"))
check("GetCurrentPlayer returns player and jwt", g.player.name == login["username"] and g.jwt.count(".") == 2)

# What the game calls right after login, through the gateway, against the real databases.
from google.protobuf import text_format
geid = g.player.geid
EG = "sc.external.services.entitygraph.v1"
query = M(EG + ".EntityQueryRequest")()
text_format.Parse(f"""body {{ scope {{ type: SCOPE_TYPE_GLOBAL }} query {{ filter {{ property_filter {{
    property: "geid" operator: COMPARISON_OPERATOR_IN values {{ unsigned_bigint_value: {geid} }} }} }} }} }}""", query)
found = unary(EG + ".EntityGraphService", "EntityQuery", query, M(EG + ".EntityQueryResponse"))
check("EntityQuery finds the player", str(geid) in str(found), f"{len(str(found))} bytes")

SOC = "sc.external.services.social.v1"
key = unary(SOC + ".SocialService", "GetSubscriptionKey", M(SOC + ".GetSubscriptionKeyRequest")(), M(SOC + ".GetSubscriptionKeyResponse"))
check("GetSubscriptionKey returns a key", bool(key.subscription_key))
CT = "sc.external.services.contacts.v1"
contacts = M(CT + ".ListContactsRequest")(); contacts.pagination.first = 250
unary(CT + ".ContactsService", "ListContacts", contacts, M(CT + ".ListContactsResponse"))
check("ListContacts answers", True)

LG = "sc.external.services.ledger.v1"
funds = unary(LG + ".LedgerService", "GetFunds", M(LG + ".GetFundsRequest")(), M(LG + ".GetFundsResponse"))
check("ledger holds aUEC", funds.ledgers[0].funds.amount > 0 and funds.ledgers[0].owner_urn.startswith("urn:sc:global:player:geid:"), funds.ledgers[0].owner_urn)

ED = "sc.external.services.entity_document.v1"
doc = M(ED + ".UpsertRequest")(); body = doc.upserts.add()
body.command.entity_id = geid; body.command.generation = 1; body.body.player_home_location.home_location_id = 848868938
unary(ED + ".ExternalEntityDocumentService", "Upsert", doc, M(ED + ".UpsertResponse"))
fetch = M(ED + ".FetchRequest")(); q = fetch.queries.add().query; q.entity_id = geid; setattr(q, "class", "player_home_location")
got = unary(ED + ".ExternalEntityDocumentService", "Fetch", fetch, M(ED + ".FetchResponse"))
check("entity document persists", got.documents[0].body.player_home_location.home_location_id == 848868938)

CS = "sc.external.services.character_customizer.v1"
saved = M(CS + ".SaveCharacterCustomizationsRequest")(); saved.character_customizations.dna_matrix = b"dna"
unary(CS + ".CharacterCustomizerService", "SaveCharacterCustomizations", saved, M(CS + ".SaveCharacterCustomizationsResponse"))
check("character customization saves", True)

SHOP = "sc.external.services.solar_system_shop.v1"
shop = unary(SHOP + ".SolarSystemShopService", "GetGlobalInventory", M(SHOP + ".GetGlobalInventoryRequest")(), M(SHOP + ".GetGlobalInventoryResponse"))
check("shop is seeded from the game data", len(str(shop)) > 100, f"{len(str(shop))} bytes")

AN = "sc.external.services.echo.v1"
gz = chan.unary_unary(f"/{AN}.EchoService/Echo", request_serializer=lambda x: x.SerializeToString(),
                      response_deserializer=M(AN + ".EchoResponse").FromString)
gz(M(AN + ".EchoRequest")(), metadata=meta, timeout=5, compression=grpc.Compression.Gzip)
check("gzip request accepted", True)

# The client rejects an envelope missing any of its three fields, including id 0 and empty payload.
hub_send("msg_ping", b"", 0)
length, magic = struct.unpack("<II", read_exact(8))
raw = fields(read_exact(length))
check("envelope carries key, payload and id even when empty", raw.get(2) == [b""] and raw.get(3) == [0])

hub_send("account_login", ld(1, login["token"].encode()) + ld(2, b"sig"), 7)
key, body, rid = hub_recv()
f = fields(body); rec = fields(f[2][0])
check("hub account_login_response", key == "account_login_response" and f[1][0] == 1 and rid == 7)
check("handle in field 5", rec[5][0].decode() == login["username"])
check("required fields 5 and 6 present, no queue wait", f.get(5) == [0] and f.get(6) == [0])

hub_send("cmsg_ls_req_dests", b"", 8)
key, body, _ = hub_recv()
dests = [fields(d) for d in fields(body)[1]]
ids = sorted(d[1][0] for d in dests)
check("destination list uses the game's location ids", key == "smsg_ls_dest_list" and
      ids == [0x0002000100010001, 0x0003000100010001, 0x0005000100010001], str([hex(i) for i in ids]))
check("destinations have name tokens", all(d[2][0].startswith(b"@") for d in dests), dests[0][2][0].decode())
# The bytes the real client sent, for the call that gates the join.
rpc = (ld(1, b"entitlement.inventory.location") + ld(2, b'{"locationId":848868938}') + varint(5 << 3) + varint(10001)
       + ld(6, varint(1 << 3) + varint(5053125871272637958) + varint(2 << 3) + varint(7898989898)))
hub_send("diff.rpc.request", rpc, 0)
key, body, _ = hub_recv()
f = fields(body)
check("RPC response carries all five required fields", key == "diff.rpc.response" and set(f) == {1, 2, 3, 4, 5})
guid = fields(f[5][0])
check("RPC response matches the call and succeeds", f[2] == [10001] and f[3] == [0] and guid[1] == [5053125871272637958])

# The exact payload the real client sent for Pyro.
hub_send("cmsg_ls_goto_loc", bytes.fromhex("08818084809080c001"), 0)
key, body, _ = hub_recv()
check("goto location routes to the shard in that system", key == "smsg_do_join_instance" and fields(body)[5] == [0x0003000100010001])

hub_send("slash.joininstance", b'{"instance_id": 1, "by_friend": false}', 9)  # format from the RE repo
key, body, _ = hub_recv()
join = fields(body); ip, port = join[3][0].decode(), join[4][0]
check("join update names the shard", key == "smsg_do_join_instance", f"{ip}:{port}")

nub = NubChannel(remote=(ip, port))
nub.handshake(session_guid=b"\x01" * 16)
check("shard handshake as the real client", nub.connected and len(nub.keys) == 3)
check("shard answers a ping", nub.ping(77) == 77)
nub.close()
print("\nALL PASSED" if not failures else f"\n{len(failures)} FAILED: {failures}")
sys.exit(1 if failures else 0)
