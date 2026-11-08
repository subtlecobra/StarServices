#!/usr/bin/env bash
# Populates a deploy host's registry from this PC (the build PC) with everything its cluster pulls:
#   1. logs in to the registry as the user push (asks for the password once; the deploy host's cluster-up.sh made it
#      and says where it keeps it), unless already logged in;
#   2. the service images and the charts (scripts/build-and-push.sh; name services to build only those images);
#   3. the game data and the seed bundle of the archive's build (scripts/gamedata-publish.sh --seed).
#
#   scripts/publish.sh [service...]
#
# The deploy host runs StarServices-CN's scripts/cluster-up.sh itself (with --wait-for-push, it deploys as soon as this
# is done; otherwise run it again after). Every step is cached: a repeat with nothing changed pushes nothing new.
#
# Settings (environment):
#   SC_P4K                      game archive (local.env); without it only images and charts are pushed
#   SC_REGISTRY_PASSWORD_FILE   file with the push password, instead of the prompt
#   SC_SKIP_BUILD=1             push no images, reuse those already in the registry
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP, SC_DEPLOY_REPO, SC_ENVIRONMENT   see scripts/registry.sh
# The registry name (registry.yaml's address) must lead to the deploy host here: /etc/hosts or DNS.
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
    echo "publish: no registry answers at $scheme://$registry ($registry_host is $resolved here). Run StarServices-CN's" >&2
    echo "scripts/cluster-up.sh on the deploy host first, and open its port ${registry##*:} to this PC." >&2
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
      read -rsp "push password for $registry (registry.password in the deploy host's backend state): " password
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
