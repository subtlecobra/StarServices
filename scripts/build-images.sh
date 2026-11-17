#!/usr/bin/env bash
# Builds one image per service, tagged with the service's version (its Cargo.toml), and pushes the versions the deploy
# registry (scripts/registry.sh) does not have yet. Name services to consider only those:
#   scripts/build-images.sh sc-pu-frontend
#
# A version is pushed once and never changes. The build stage compiles every service once, incrementally; each image is
# then keyed by the SHA-256 of its binary and of the Dockerfile (label io.scbackend.build-key), and for each service:
#   - its version is not in the registry: built and pushed;
#   - it is, with the same key: nothing to do (an unchanged binary is byte-identical);
#   - it is, with another key: the service changed without a new version. Nothing at all is pushed then: bump the version
#     of every service named (semver: patch, minor or major for the kind of change) and run this again.
# From a working tree with uncommitted changes, only release candidates (X.Y.Z-rc.N) are pushed: a plain version comes
# from a clean commit (CLAUDE.md).
# Every service ends up in $SC_IMAGES_OUT (target/images.tsv): name, version and the digest of its image in the registry,
# for scripts/publish.sh's release manifest. Those not considered are listed at the digest the registry has for their
# version, and left out (with a warning) when it has none.
#
# The registry checks and the images' builds and pushes run SC_JOBS at a time.
#
# Settings (environment):
#   SC_IMAGES_OUT                         the list written     (target/images.tsv)
#   SC_JOBS                               images at a time     (8)
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
set -euo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."
source scripts/registry.sh
[[ -d proto/sc ]] || { echo "proto/sc/ is missing: generate it from your game with scripts/game-files.sh" >&2; exit 1; }
out=${SC_IMAGES_OUT:-target/images.tsv}
jobs=${SC_JOBS:-8}
[[ $jobs =~ ^[1-9][0-9]*$ ]] || { echo "build-images: SC_JOBS must be a positive number" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Runs `$1 <item>` for each further argument, `jobs` at a time, and fails when any of them failed.
parallel() {
  local task=$1 failed=0 running=0
  shift
  for item in "$@"; do
    if ((running >= jobs)); then
      wait -n || failed=1
      running=$((running - 1))
    fi
    "$task" "$item" &
    running=$((running + 1))
  done
  while ((running > 0)); do
    wait -n || failed=1
    running=$((running - 1))
  done
  return "$failed"
}
mkdir -p "$(dirname "$out")"
tls=()
[[ $registry_plain_http == true ]] && tls=(--tls-verify=false)
scheme=https
[[ $registry_plain_http == true ]] && scheme=http

# The services and their versions: every package under services/ with a binary.
declare -A version
while IFS=$'\t' read -r name ver; do
  version[$name]=$ver
done < <(cargo metadata --no-deps --format-version 1 | jq -r --arg root "$PWD/services/" '
  .packages[] | select(.manifest_path | startswith($root)) | select(any(.targets[]; .kind | index("bin")))
  | [.name, .version] | @tsv')
services=("$@")
if ((${#services[@]} == 0)); then
  mapfile -t services < <(printf '%s\n' "${!version[@]}" | sort)
fi
for service in "${services[@]}"; do
  [[ -n ${version[$service]:-} ]] || { echo "build-images: no service named $service under services/" >&2; exit 1; }
done
packages=${*/#/-p }

# 1. The build stage, once: every binary, and its hash.
build_image=localhost/scbackend-build:latest
podman build -q --target build --build-arg "PACKAGES=$packages" -t "$build_image" . >/dev/null
declare -A binary_sha
while read -r sum name; do
  binary_sha[$name]=$sum
done < <(podman run --rm --entrypoint sh "$build_image" -c 'cd /out && sha256sum *')
dockerfile_sha=$(sha256sum Dockerfile | cut -d' ' -f1)

# The registry's manifest digest and build key of a tag; both empty when it has no such tag. Read anonymously.
# -q: no ~/.curlrc, whose DNS settings (such as doh-url) would skip /etc/hosts, where the registry name is.
accept='application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json'
registry_state() {
  local base=$scheme://$registry/v2/scbackend/$1 headers manifest config key digest
  headers=$(mktemp)
  manifest=$(curl -q -sf -m 10 -D "$headers" -H "Accept: $accept" "$base/manifests/$2") || { rm -f "$headers"; echo " "; return; }
  digest=$(tr -d '\r' < "$headers" | awk -F': ' 'tolower($1) == "docker-content-digest" {print $2}')
  rm -f "$headers"
  config=$(jq -r '.config.digest // empty' <<<"$manifest")
  key=$(curl -q -sfL -m 10 "$base/blobs/$config" | jq -r '.config.Labels["io.scbackend.build-key"] // empty')
  echo "$digest ${key:--}"
}

# 2. What each service needs, before anything is pushed.
declare -A key digest
to_push=() stale=()
check() { registry_state "$1" "${version[$1]}" > "$work/$1.state"; }
parallel check "${!version[@]}"
for service in "${services[@]}"; do
  [[ -n ${binary_sha[$service]:-} ]] || { echo "build-images: the build made no binary named $service" >&2; exit 1; }
  key[$service]=$(printf '%s %s' "${binary_sha[$service]}" "$dockerfile_sha" | sha256sum | cut -d' ' -f1)
  read -r remote_digest remote_key < "$work/$service.state"
  if [[ -z $remote_digest ]]; then
    to_push+=("$service")
  elif [[ $remote_key == "${key[$service]}" ]]; then
    digest[$service]=$remote_digest
  else
    stale+=("$service ${version[$service]}")
  fi
done
if ((${#stale[@]} > 0)); then
  echo "build-images: these services changed but their version is already in $registry with other content:" >&2
  printf '  %s\n' "${stale[@]}" >&2
  echo "Bump each one's version in its Cargo.toml (semver: patch for a fix, minor for a compatible feature, major for a" >&2
  echo "breaking change) and run this again. Nothing was pushed." >&2
  exit 1
fi

# Uncommitted work is published as release candidates only.
if [[ -n $(git status --porcelain 2>/dev/null) ]]; then
  plain=()
  for service in "${to_push[@]}"; do
    [[ ${version[$service]} == *-* ]] || plain+=("$service ${version[$service]}")
  done
  if ((${#plain[@]} > 0)); then
    echo "build-images: the working tree has uncommitted changes, so these would be published as plain versions:" >&2
    printf '  %s\n' "${plain[@]}" >&2
    echo "Give each a release candidate version (X.Y.Z-rc.N) to publish uncommitted work, or commit first. Nothing was pushed." >&2
    exit 1
  fi
fi

# 3. The new versions, each built and pushed on its own. A failed one does not stop the others; the run fails after.
publish() {
  local service=$1 image=$registry/scbackend/$1:${version[$1]}
  podman build -q --timestamp 0 --build-arg "SERVICE=$service" --build-arg "VERSION=${version[$service]}" \
    --build-arg "PACKAGES=$packages" --label "io.scbackend.build-key=${key[$service]}" -t "$image" . >/dev/null &&
    podman push -q "${tls[@]}" --digestfile "$work/$service.digest" "$image" &&
    echo "pushed $image" || { echo "build-images: $service failed" >&2; return 1; }
}
publish_failed=0
((${#to_push[@]} == 0)) || parallel publish "${to_push[@]}" || publish_failed=1
for service in "${to_push[@]}"; do
  [[ -s $work/$service.digest ]] && digest[$service]=$(<"$work/$service.digest")
done
((publish_failed == 0)) || { echo "build-images: some images were not pushed, see above" >&2; exit 1; }

for service in $(printf '%s\n' "${!version[@]}" | sort); do
  if [[ -z ${digest[$service]:-} ]]; then
    read -r remote_digest _ < "$work/$service.state"
    if [[ -z $remote_digest ]]; then
      echo "build-images: $registry has no $service ${version[$service]}: left out of $out" >&2
      continue
    fi
    digest[$service]=$remote_digest
  fi
  printf '%s\t%s\t%s\n' "$service" "${version[$service]}" "${digest[$service]}"
done > "$out"
echo "build-images: ${#to_push[@]} pushed, $((${#services[@]} - ${#to_push[@]})) unchanged (list: $out)"
