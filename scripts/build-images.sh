#!/usr/bin/env bash
# Builds one image per service and pushes them to the deploy registry (scripts/registry.sh), only those that changed.
# Name services to build only those:
#   scripts/build-images.sh sc-pu-frontend
#
# The build stage (the whole workspace, or the named services) compiles once, incrementally. Each service's image is then
# keyed by the SHA-256 of its binary and of this repository's Dockerfile, kept in the image's label
# io.scbackend.build-key: an image whose key matches the one in the registry is neither built nor pushed. A binary that
# did not change is byte-identical, so a change rebuilds and pushes only the services it reaches.
#
# Settings (environment):
#   SC_FORCE_PUSH=1   build and push every image anyway, e.g. for a new Alpine base image
#   SC_IMAGE_TAG      the tag (dev)
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
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
tls=()
[[ $registry_plain_http == true ]] && tls=(--tls-verify=false)
scheme=https
[[ $registry_plain_http == true ]] && scheme=http

# 1. The build stage, once: every binary, and its hash.
build_image=localhost/scbackend-build:$tag
podman build -q --target build --build-arg "PACKAGES=$packages" -t "$build_image" . >/dev/null
declare -A binary_sha
while read -r sum name; do
  binary_sha[$name]=$sum
done < <(podman run --rm --entrypoint sh "$build_image" -c 'cd /out && sha256sum *')
dockerfile_sha=$(sha256sum Dockerfile | cut -d' ' -f1)

# The key of the image in the registry: its config's label, read anonymously. Empty when there is none or it cannot be read.
accept='application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json'
registry_key() {
  local base=$scheme://$registry/v2/scbackend/$1 config
  # -q: no ~/.curlrc, whose DNS settings (such as doh-url) would skip /etc/hosts, where the registry name is.
  config=$(curl -q -sf -m 10 -H "Accept: $accept" "$base/manifests/$tag" | jq -r '.config.digest // empty' 2>/dev/null) || return 0
  [[ -n $config ]] || return 0
  curl -q -sfL -m 10 "$base/blobs/$config" | jq -r '.config.Labels["io.scbackend.build-key"] // empty' 2>/dev/null || true
}

# 2. Each service: built and pushed only when its key changed.
built=0 skipped=0
for service in "${services[@]}"; do
  [[ -n ${binary_sha[$service]:-} ]] || { echo "build-images: the build made no binary named $service" >&2; exit 1; }
  key=$(printf '%s %s' "${binary_sha[$service]}" "$dockerfile_sha" | sha256sum | cut -d' ' -f1)
  image=$registry/scbackend/$service:$tag
  if [[ ${SC_FORCE_PUSH:-0} != 1 && $(registry_key "$service") == "$key" ]]; then
    skipped=$((skipped + 1))
    continue
  fi
  podman build -q --build-arg "SERVICE=$service" --build-arg "PACKAGES=$packages" \
    --label "io.scbackend.build-key=$key" -t "$image" . >/dev/null
  podman push -q "${tls[@]}" "$image"
  echo "pushed $image"
  built=$((built + 1))
done
echo "build-images: $built built and pushed, $skipped unchanged"
