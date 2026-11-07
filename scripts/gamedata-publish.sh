#!/usr/bin/env bash
# Populates the region caches with a game build, from the archive to the registry in one command: converts Data.p4k with
# tools/gamedata-build and pushes the result with scripts/gamedata-push.sh as <registry>/scbackend/gamedata:<build>.
#   scripts/gamedata-publish.sh [Data.p4k] [--build <n>] [--out <dir>] [--jobs <n>] [--report] [--seed]
# Run it once per game build, on a workstation that has the game: no cluster ever sees the archive. A second run of the
# same build converts only what is missing and pushes no new layers. --seed also publishes the build's seed bundle with
# scripts/seed-publish.sh (same archive and build), so one command publishes everything a cluster needs from the archive.
#
# Settings (environment; the arguments win):
#   SC_P4K            game archive              (none; set it in local.env)
#   SC_GAMEDATA_OUT   converted builds          (<repo>/target/gamedata)
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
# The build is the change number of build_manifest.id beside the archive, as gamedata-build reads it.
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
p4k=${SC_P4K:-}
out=${SC_GAMEDATA_OUT:-$repo/target/gamedata}
build=
tool_args=()
seed=false
while (($#)); do
  case $1 in
    --build) build=${2:?--build needs a value}; shift 2 ;;
    --out) out=${2:?--out needs a value}; shift 2 ;;
    --jobs) tool_args+=(--jobs "${2:?--jobs needs a value}"); shift 2 ;;
    --report) tool_args+=(--report); shift ;;
    --seed) seed=true; shift ;;
    -*) echo "unknown option $1" >&2; exit 1 ;;
    *) p4k=$1; shift ;;
  esac
done
[[ -n $p4k && -f $p4k ]] || { echo "no game archive at '$p4k': give its path, or set SC_P4K (local.env)" >&2; exit 1; }
if [[ -z $build ]]; then
  build=$(jq -r '.Data.RequestedP4ChangeNum // empty' "$(dirname "$p4k")/build_manifest.id")
  [[ -n $build ]] || { echo "no build in build_manifest.id beside $p4k; give --build" >&2; exit 1; }
fi

# meshoptimizer is C++ and the repository's musl target has no g++ here; the tool runs on this machine only.
(cd "$repo/tools/gamedata-build" && cargo build -q --release --target x86_64-unknown-linux-gnu)
"$repo/tools/gamedata-build/target/x86_64-unknown-linux-gnu/release/gamedata-build" "$p4k" "$out" --build "$build" "${tool_args[@]}"
"$repo/scripts/gamedata-push.sh" "$out" "$build"
if [[ $seed == true ]]; then
  "$repo/scripts/seed-publish.sh" "$p4k" --build "$build"
fi
