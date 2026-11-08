#!/usr/bin/env bash
# The images and charts a deploy takes from this repository (scripts/publish.sh runs it, with the game data and seed),
# pushing only those that differ from the registry's:
#   1. builds and pushes the service images (scripts/build-images.sh);
#   2. packages the charts in deploy/charts/ and pushes them as OCI artifacts to
#      oci://<registry>/scbackend/charts, versioned from each chart's Chart.yaml.
# Name services to build only those images. The registry is the deploy environment's (scripts/registry.sh).
#
# Settings (environment):
#   SC_SKIP_BUILD=1   push no images, reuse those already in the registry
#   SC_FORCE_PUSH=1   push every image and chart, also those the registry already has unchanged
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
# A chart is pushed only when it differs from the same version in the registry (or the registry has none):
# both are unpacked and compared. SC_FORCE_PUSH=1 pushes every chart.
for chart in deploy/charts/*/; do
  helm package -d "$packages" "$chart" >/dev/null
done
pushed=0 unchanged=0
for package in "$packages"/*.tgz; do
  file=$(basename "$package" .tgz)
  name=${file%-*} version=${file##*-}
  compare=$packages/compare-$file
  mkdir -p "$compare/local" "$compare/registry"
  tar -xzf "$package" -C "$compare/local"
  if [[ ${SC_FORCE_PUSH:-0} != 1 ]] &&
     helm pull "${push_args[@]}" "oci://$registry/scbackend/charts/$name" --version "$version" --untar \
       -d "$compare/registry" >/dev/null 2>&1 &&
     diff -rq "$compare/local" "$compare/registry" >/dev/null; then
    unchanged=$((unchanged + 1))
    continue
  fi
  helm push "${push_args[@]}" "$package" "oci://$registry/scbackend/charts"
  pushed=$((pushed + 1))
done
echo "build-and-push: $pushed charts pushed, $unchanged unchanged"
