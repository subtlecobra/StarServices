# shellcheck shell=bash
# Sourced by the build scripts: the OCI registry they push images, charts and artifacts to. Settings, from the environment
# or this repository's local.env (which git ignores):
#   SC_REGISTRY              host[:port] of the registry           (required)
#   SC_REGISTRY_PLAIN_HTTP   true: plain HTTP, false: HTTPS         (false)
# A name such as registry.example.lan must resolve on this machine (/etc/hosts or DNS).
registry_repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# shellcheck source=/dev/null
if [[ -f $registry_repo/local.env ]]; then source "$registry_repo/local.env"; fi
registry=${SC_REGISTRY:-}
if [[ -z $registry ]]; then
  echo "SC_REGISTRY is not set: the registry to push to, host[:port] (set it in local.env)" >&2
  exit 1
fi
registry_plain_http=${SC_REGISTRY_PLAIN_HTTP:-false}
