#!/usr/bin/env bash
# Everything a deploy takes from this repository, run before BackendDeploySC's scripts/cluster-up.sh:
#   1. builds sc-admin and runs `sc-admin init`, which creates or updates the backend state in data/
#      (accounts, certificates, destinations), which cluster-up.sh hands to the pods as Secrets;
#   2. builds and pushes the service images (scripts/build-images.sh);
#   3. packages the charts in deploy/charts/ and pushes them as OCI artifacts to
#      oci://<registry>/scbackend/charts, versioned from each chart's Chart.yaml.
# Name services to build only those images. The registry is the deploy environment's (scripts/registry.sh).
#
# Settings (environment):
#   SC_BACKEND_HOST   address the game uses              (127.0.0.2)
#   SC_BACKEND_DATA   backend state directory            (<repo>/data)
#   SC_P4K            game archive, for the star systems (none; set it in local.env)
#   SC_SKIP_BUILD=1   push no images, reuse those already in the registry
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"
source scripts/registry.sh
[[ -d proto/sc ]] || { echo "proto/sc/ is missing: generate it from your game with scripts/game-files.sh" >&2; exit 1; }
host=${SC_BACKEND_HOST:-127.0.0.2}
data_dir=${SC_BACKEND_DATA:-$repo/data}
p4k=${SC_P4K:-}

(cd tools/sc-admin && cargo build --release)
admin=tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin
p4k_args=()
[[ -f $p4k ]] && p4k_args=(--p4k "$p4k")
"$admin" --data "$data_dir" init --host "$host" "${p4k_args[@]}"

[[ ${SC_SKIP_BUILD:-0} == 1 ]] || scripts/build-images.sh "$@"

packages=$(mktemp -d)
trap 'rm -rf "$packages"' EXIT
push_args=()
[[ $registry_plain_http == true ]] && push_args=(--plain-http)
for chart in deploy/charts/*/; do
  helm package -d "$packages" "$chart" >/dev/null
done
for package in "$packages"/*.tgz; do
  helm push "${push_args[@]}" "$package" "oci://$registry/scbackend/charts"
done
