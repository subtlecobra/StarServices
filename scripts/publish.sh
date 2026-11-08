#!/usr/bin/env bash
# Publishes everything a deployment of this backend pulls into an OCI registry, from this PC (the build PC):
#   1. logs in to the registry as the user push (asks for the password once, or reads SC_REGISTRY_PASSWORD_FILE), unless
#      already logged in;
#   2. the service images and the charts (scripts/build-and-push.sh): each version is pushed once, and a component that
#      changed without a new version stops it (bump the version, semver);
#   3. the game data and the seed bundle of the archive's build (scripts/gamedata-publish.sh --seed);
#   4. the release manifest: every image and chart by version and digest, as target/release.yaml and in the registry as
#      <registry>/scbackend/release:<release id> and :latest, for a deployment to pin (it pulls the manifest, then deploys
#      exactly those digests).
#
#   scripts/publish.sh [service...]
#
# With service names only those images are considered; the manifest still lists every service, at the digest the
# registry has for its version.
#
# Settings (environment, or local.env):
#   SC_P4K                      game archive; without it only images and charts are pushed
#   SC_REGISTRY_PASSWORD_FILE   file with the push password, instead of the prompt
#   SC_SKIP_BUILD=1             consider no images
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"
source scripts/registry.sh
registry_host=${registry%%:*}
p4k=${SC_P4K:-}
step() { printf '\n== %s\n' "$*"; }

step "1. registry $registry"
if [[ $registry_host != localhost ]]; then
  resolved=$(getent hosts "$registry_host" | awk '{print $1; exit}' || true)
  if [[ -z $resolved ]]; then
    echo "publish: $registry_host does not resolve here. Map it to the deploy host's address:" >&2
    echo "  echo '<deploy host address> $registry_host' | sudo tee -a /etc/hosts" >&2
    exit 1
  fi
  scheme=https
  [[ $registry_plain_http == true ]] && scheme=http
  # -q: no ~/.curlrc, whose DNS settings (such as doh-url) would skip /etc/hosts, where the registry name is.
  status=$(curl -q -s -m 10 -o /dev/null -w '%{http_code}' "$scheme://$registry/v2/" || true)
  if [[ $status != 200 && $status != 401 ]]; then
    echo "publish: no registry answers at $scheme://$registry ($registry_host is $resolved here): start it, and open" >&2
    echo "its port ${registry##*:} to this PC." >&2
    exit 1
  fi
  tls=() helm_tls=()
  [[ $registry_plain_http == true ]] && tls=(--tls-verify=false) helm_tls=(--plain-http)
  helm_config=${HELM_REGISTRY_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/helm/registry/config.json}
  if podman login --get-login "$registry" >/dev/null 2>&1 && grep -qF "\"$registry\"" "$helm_config" 2>/dev/null &&
     [[ -z ${SC_REGISTRY_PASSWORD_FILE:-} ]]; then
    echo "logged in already as $(podman login --get-login "$registry")"
  else
    if [[ -n ${SC_REGISTRY_PASSWORD_FILE:-} ]]; then
      password=$(<"$SC_REGISTRY_PASSWORD_FILE")
    else
      read -rsp "push password for $registry: " password
      echo
    fi
    printf '%s' "$password" | podman login "${tls[@]}" -u push --password-stdin "$registry" >/dev/null
    printf '%s' "$password" | helm registry login "${helm_tls[@]}" -u push --password-stdin "$registry" >/dev/null
    unset password
    echo "logged in as push"
  fi
else
  echo "the local kind-registry container: no login"
fi

step "2. images and charts"
scripts/build-and-push.sh "$@"

step "3. game data and seed"
if [[ -f $p4k ]]; then
  scripts/gamedata-publish.sh "$p4k" --seed
else
  echo "publish: no game archive (SC_P4K, local.env): no game data or seed pushed; the shards and the sc-seed Job wait" >&2
  echo "for them" >&2
fi

step "4. release manifest"
release=$(date -u +%Y%m%d-%H%M%S)
{
  echo "# A release of StarServices: every image and chart in the registry below, by version and digest."
  echo "release: $release"
  echo "commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)$(git diff --quiet HEAD 2>/dev/null || echo '-dirty')"
  echo "registry: $registry"
  echo "images:"
  awk -F'\t' '{ printf "  %s: \"%s@%s\"\n", $1, $2, $3 }' target/images.tsv
  echo "charts:"
  awk -F'\t' '{ printf "  %s: \"%s@%s\"\n", $1, $2, $3 }' target/charts.tsv
} > target/release.yaml
# The manifest as a one-file image, reproducible like the game data's.
context=$(mktemp -d)
\cp -f target/release.yaml "$context/release.yaml"
printf 'FROM scratch\nCOPY release.yaml /release.yaml\n' > "$context/Containerfile"
image=$registry/scbackend/release
podman build -q --timestamp 0 -t "$image:$release" -t "$image:latest" "$context" >/dev/null
rm -rf "$context"
tls=()
[[ $registry_plain_http == true ]] && tls=(--tls-verify=false)
podman push -q "${tls[@]}" "$image:$release"
podman push -q "${tls[@]}" "$image:latest"
echo "release $release: $(grep -c '@sha256:' target/release.yaml) images and charts, $image:$release (and :latest), target/release.yaml"
