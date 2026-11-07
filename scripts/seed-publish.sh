#!/usr/bin/env bash
# Publishes the seed of a game build, from the archive to the registry in one command: `sc-admin seed-export` writes the
# seed bundle (the entity classes, the container tree and every owner's rows) and scripts/seed-push.sh pushes it as
# <registry>/scbackend/seed:<build>. The sc-seed chart's Job then applies it in the cluster, which needs no archive:
#   scripts/seed-publish.sh [Data.p4k] [--build <n>] [--out <dir>] [--rental-prices <file>]
# Run it once per game build on a machine that has the game, as scripts/gamedata-publish.sh (whose --seed runs this
# too). The same archive gives the same bundle and the same image digest, so a second run pushes nothing new.
#
# Settings (environment; the arguments win):
#   SC_P4K             game archive                  (none; set it in local.env)
#   SC_SEED_OUT        exported bundles              (<repo>/target/seed)
#   SC_RENTAL_PRICES   rental prices to put in the bundle when present
#                      (<deploy repo>/environments/<environment>/rental-prices.json)
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP, SC_DEPLOY_REPO, SC_ENVIRONMENT   see scripts/registry.sh
# The build is the change number of build_manifest.id beside the archive, as for gamedata-publish.
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
p4k=${SC_P4K:-}
out=${SC_SEED_OUT:-$repo/target/seed}
prices=${SC_RENTAL_PRICES:-${SC_DEPLOY_REPO:-$repo/../StarServices-CN}/environments/${SC_ENVIRONMENT:-dev}/rental-prices.json}
build=
while (($#)); do
  case $1 in
    --build) build=${2:?--build needs a value}; shift 2 ;;
    --out) out=${2:?--out needs a value}; shift 2 ;;
    --rental-prices) prices=${2:?--rental-prices needs a file}; shift 2 ;;
    -*) echo "unknown option $1" >&2; exit 1 ;;
    *) p4k=$1; shift ;;
  esac
done
[[ -n $p4k && -f $p4k ]] || { echo "no game archive at '$p4k': give its path, or set SC_P4K (local.env)" >&2; exit 1; }
if [[ -z $build ]]; then
  build=$(jq -r '.Data.RequestedP4ChangeNum // empty' "$(dirname "$p4k")/build_manifest.id")
  [[ -n $build ]] || { echo "no build in build_manifest.id beside $p4k; give --build" >&2; exit 1; }
fi
export_args=(--build "$build")
[[ -f $prices ]] && export_args+=(--rental-prices "$prices")

(cd "$repo/tools/sc-admin" && cargo build -q --release)
"$repo/tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin" seed-export "$p4k" "$out" "${export_args[@]}"
"$repo/scripts/seed-push.sh" "$out" "$build"
