#!/usr/bin/env bash
# Builds one image per service and pushes them to the deploy registry (scripts/registry.sh). The build
# stage runs once and is cached for the rest. Name services to build only those:
#   scripts/build-images.sh sc-pu-frontend
set -euo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."
source scripts/registry.sh
[[ -d proto/sc ]] || { echo "proto/sc/ is missing: generate it from your game with scripts/game-files.sh" >&2; exit 1; }
tag=${SC_IMAGE_TAG:-dev}
packages=--workspace
services=("$@")
if ((${#services[@]})); then
  packages=${services[*]/#/-p }
else
  for manifest in services/*/Cargo.toml; do
    services+=("$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)")
  done
fi
for service in "${services[@]}"; do
  image=$registry/scbackend/$service:$tag
  podman build -q --build-arg "SERVICE=$service" --build-arg "PACKAGES=$packages" -t "$image" .
  if [[ $registry_plain_http == true ]]; then
    podman push -q --tls-verify=false "$image"
  else
    podman push -q "$image"
  fi
done
