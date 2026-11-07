# shellcheck shell=bash
# Sourced by the build scripts: the registry images and charts are pushed to. It is the deploy
# environment's setting, environments/<env>/registry.yaml in StarServices-CN, so the cluster pulls from
# where the build pushed. The environment wins over the file:
#   SC_REGISTRY              host[:port]                   (registry.alexandria.local:5000 without the file)
#   SC_REGISTRY_PLAIN_HTTP   true: plain HTTP, false: HTTPS (true without the file)
#   SC_DEPLOY_REPO           the StarServices-CN checkout   (../StarServices-CN)
#   SC_ENVIRONMENT           the deploy environment         (dev)
# SC_REGISTRY=localhost:5001 is the local kind-registry container that StarServices-CN's cluster-up.sh runs.
registry_file=${SC_DEPLOY_REPO:-$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")/../StarServices-CN}/environments/${SC_ENVIRONMENT:-dev}/registry.yaml
registry_setting() {
  [[ -f $registry_file ]] || return 0
  sed -n "s/^$1:[[:space:]]*\([^#]*\).*/\1/p" "$registry_file" | sed 's/[[:space:]]*$//; s/^"\(.*\)"$/\1/'
}
registry=${SC_REGISTRY:-$(registry_setting address)}
registry=${registry:-registry.alexandria.local:5000}
registry_plain_http=${SC_REGISTRY_PLAIN_HTTP:-$(registry_setting plainHttp)}
registry_plain_http=${registry_plain_http:-true}
