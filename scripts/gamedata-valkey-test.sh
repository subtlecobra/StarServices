#!/usr/bin/env bash
# Runs sc-gamedata against a real Valkey cluster in podman, made the way a ShardRegion's is: six nodes (three primaries,
# a replica each) of docker.io/valkey/valkey with the shard operator's valkey.conf and start.sh, formed by its init.sh.
#   scripts/gamedata-valkey-test.sh                       the ignored integration test of sc-gamedata
#   scripts/gamedata-valkey-test.sh --image <build> --expect <file> [--path <path>] [--registry <host:port>]
#                                   [--network <podman network>]
# With --image it then empties the cluster, runs the sc-gamedata binary on it, which pulls <registry>/scbackend/gamedata:<build>
# (default kind-registry:5000 on the network kind, the local registry of StarServices-CN), and checks that <path>
# (default collision/1200814982.col) comes back byte for byte as <file>.
# The tests and the service run in containers (Alpine, as the service images) on the cluster's network, where the nodes' host names resolve; everything
# started here is removed at the end.
set -euo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."

image="" expect="" path=collision/1200814982.col registry=kind-registry:5000 registry_network=kind
while [[ $# -gt 0 ]]; do
  case $1 in
    --image) image=$2; shift 2 ;;
    --expect) expect=$(readlink -f "$2"); shift 2 ;;
    --path) path=$2; shift 2 ;;
    --registry) registry=$2; shift 2 ;;
    --network) registry_network=$2; shift 2 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
if [[ -n $image && ! -f $expect ]]; then
  echo "--image needs --expect <file>" >&2
  exit 2
fi

valkey=${SC_VALKEY_IMAGE:-docker.io/valkey/valkey:9.0}
# What the service images run on, with the CA roots the registry client loads.
runtime=docker.io/library/alpine:3
network=sc-gamedata-test
prefix=sc-gamedata-test
nodes=6
containers=()
cleanup() {
  podman rm -f "${containers[@]}" >/dev/null 2>&1 || true
  podman network rm "$network" >/dev/null 2>&1 || true
}
trap cleanup EXIT

work=target/gamedata-valkey-test
mkdir -p "$work"
{ cat services/shard-operator/valkey/valkey.conf; echo "maxmemory 200mb"; } > "$work/valkey.conf"

executable() {
  python3 -c '
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message["target"]["name"] == "sc-gamedata" and message.get("executable"):
        if message["profile"]["test"] == (sys.argv[1] == "test"):
            print(message["executable"])
' "$1"
}
tests=$(cargo test -p sc-gamedata --no-run --message-format=json 2>/dev/null | executable test | tail -1)
service=$(cargo build -p sc-gamedata --message-format=json 2>/dev/null | executable bin | tail -1)

podman network create "$network" >/dev/null
names=""
for i in $(seq 0 $((nodes - 1))); do
  name=$prefix-valkey-$i
  containers+=("$name")
  names="$names $name"
  podman run -d --name "$name" --network "$network" --user 999:999 \
    -v "$PWD/$work/valkey.conf:/etc/valkey/valkey.conf:ro,z" \
    -v "$PWD/services/shard-operator/valkey/start.sh:/start.sh:ro,z" \
    -e POD_HOSTNAME="$name" "$valkey" sh -c 'mkdir -p /data && POD_IP=$(hostname -i | cut -d" " -f1) exec sh /start.sh' >/dev/null
done
containers+=("$prefix-init")
podman run --rm --name "$prefix-init" --network "$network" --user 999:999 \
  -v "$PWD/services/shard-operator/valkey/init.sh:/init.sh:ro,z" -e NODES="${names# }" -e REPLICAS=1 "$valkey" sh /init.sh |
  grep -v '^#\|^Moving slot'
addresses=$(for name in $names; do printf 'redis://%s:6379,' "$name"; done)
addresses=${addresses%,}

echo "--- integration test"
podman run --rm --name "$prefix-tests" --network "$network" -v "$tests:/tests:ro,z" \
  -e SC_GAMEDATA_TEST_VALKEY="$addresses" "$runtime" /tests --ignored --nocapture real_valkey_cluster

if [[ -n $image ]]; then
  echo "--- sc-gamedata with $registry/scbackend/gamedata:$image"
  podman run --rm --network "$network" "$valkey" valkey-cli -h "$prefix-valkey-0" --cluster call "$prefix-valkey-0:6379" FLUSHALL \
    --cluster-only-masters >/dev/null
  containers+=("$prefix-service")
  podman run -d --name "$prefix-service" --network "$network,$registry_network" -v "$service:/sc-gamedata:ro,z" \
    -e SC_GAMEDATA_BUILDS="$image" -e SC_GAMEDATA_VALKEY="$addresses" -e SC_GAMEDATA_REGISTRY="$registry" \
    -e SC_GAMEDATA_PLAIN_HTTP=true -e SC_GAMEDATA_WORK_DIR=/work -e SC_GAMEDATA_OWNER="$prefix-service" \
    "$runtime" /sc-gamedata >/dev/null
  podman run --rm --network "$network" -v "$tests:/tests:ro,z" -v "$expect:/expected:ro,z" \
    -e SC_GAMEDATA_TEST_ADDR="http://$prefix-service:50051" -e SC_GAMEDATA_TEST_BUILD="$image" \
    -e SC_GAMEDATA_TEST_PATH="$path" -e SC_GAMEDATA_TEST_EXPECT=/expected \
    "$runtime" /tests --ignored --nocapture running_service_serves_the_file_byte_identical
  podman logs "$prefix-service" 2>&1 | tail -5
  podman run --rm --network "$network" "$valkey" valkey-cli -c -h "$prefix-valkey-0" get "gd:$image:complete"
fi
