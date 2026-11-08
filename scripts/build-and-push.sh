#!/usr/bin/env bash
# The images and charts a deploy takes from this repository (scripts/publish.sh runs it, with the game data and seed):
#   1. builds and pushes the service images (scripts/build-images.sh);
#   2. packages the charts in deploy/charts/ and pushes them as OCI artifacts to
#      oci://<registry>/scbackend/charts, versioned from each chart's Chart.yaml.
# Name services to build only those images. The registry is the deploy environment's (scripts/registry.sh).
#
# Settings (environment):
#   SC_SKIP_BUILD=1   push no images, reuse those already in the registry
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"
source scripts/registry.sh
[[ -d proto/sc ]] || { echo "proto/sc/ is missing: generate it from your game with scripts/game-files.sh" >&2; exit 1; }
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
