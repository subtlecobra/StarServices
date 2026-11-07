#!/usr/bin/env bash
# Seeds from the game archive on this machine, after BackendDeploySC's scripts/cluster-up.sh, because the services must
# be running. `sc-admin seed` reads the archive and hands the entity classes and the container tree to the sc-entitygraph
# of each ShardRegion (in the namespace scbackend-region-<name>, which sc-shard-operator makes), and every other table's
# rows to the service that owns it, each through its seed RPC. The owners only upsert, so it is safe to repeat.
# Without the archive there is nothing to seed, and it stops quietly. A cluster with no archive beside it seeds itself
# instead: scripts/seed-publish.sh pushes the bundle, and the sc-seed chart's Job applies it (README, Seeding).
#
#   scripts/seed.sh                     every ShardRegion
#   scripts/seed.sh --region usa ...    only these; the other tables are still handed to their owners
#
# Settings (environment):
#   SC_P4K             game archive                          (none; set it in local.env)
#   SC_SEED_PORTS      local ports: the first for the regions' sc-entitygraphs, one each, then the first of six for the
#                      owner services ("18529 15051")
#   SC_RENTAL_PRICES   rental prices to import when present  (../BackendDeploySC/environments/dev/rental-prices.json)
#   SC_KUBE_CONTEXT    cluster                               (kind-scbackend)
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"
p4k=${SC_P4K:-}
context=${SC_KUBE_CONTEXT:-kind-scbackend}
regions=()
while (($#)); do
  case $1 in
    --region) regions+=("${2:?--region needs a name}"); shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -f $p4k ]] || exit 0
if ((${#regions[@]} == 0)); then
  read -r -a regions <<< "$(kubectl --context "$context" get shardregions -o jsonpath='{.items[*].metadata.name}')"
fi
if ((${#regions[@]} == 0)); then
  echo "no ShardRegion to seed: deploy sc-shard-operator first" >&2
  exit 1
fi
for region in "${regions[@]}"; do
  state=$(kubectl --context "$context" get shardregion "$region" -o jsonpath='{.status.entityDb.state}')
  if [[ $state != Ready ]]; then
    echo "the entity database of region $region is not Ready (${state:-no status}): see kubectl get shr $region -o yaml" >&2
    exit 1
  fi
done

(cd tools/sc-admin && cargo build --release)
admin=tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin

forwards=()
stop_forwards() {
  ((${#forwards[@]} == 0)) && return 0
  kill "${forwards[@]}" 2>/dev/null || true
  wait "${forwards[@]}" 2>/dev/null || true
  forwards=()
}
trap stop_forwards EXIT
read -r graph_port service_port <<< "${SC_SEED_PORTS:-18529 15051}"
ports=()
graph_args=()
for region in "${regions[@]}"; do
  kubectl --context "$context" -n "scbackend-region-$region" port-forward svc/sc-entitygraph "$graph_port:50051" >/dev/null &
  forwards+=($!)
  ports+=("$graph_port")
  graph_args+=(--entitygraph "http://127.0.0.1:$graph_port")
  graph_port=$((graph_port + 1))
done
owner_args=()
for owner in entitlement reputation blueprint-library mission-scenario contract-broker shop; do
  service=sc-$owner
  [[ $owner == shop ]] && service=sc-solar-system-shop
  kubectl --context "$context" -n scbackend port-forward "svc/$service" "$service_port:50051" >/dev/null &
  forwards+=($!)
  ports+=("$service_port")
  owner_args+=("--$owner" "http://127.0.0.1:$service_port")
  service_port=$((service_port + 1))
done
for port in "${ports[@]}"; do
  for _ in {1..30}; do
    (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
    sleep 1
  done
done
prices=()
rental_prices=${SC_RENTAL_PRICES:-$repo/../BackendDeploySC/environments/dev/rental-prices.json}
[[ -f $rental_prices ]] && prices=(--rental-prices "$rental_prices")
"$admin" seed "${graph_args[@]}" "${owner_args[@]}" --p4k "$p4k" "${prices[@]}"
