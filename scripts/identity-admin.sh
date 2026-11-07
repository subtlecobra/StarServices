#!/usr/bin/env bash
# Runs an sc-admin account command against sc-identity in the cluster, through a port-forward, as seed.sh does for
# the seeding owners. Bootstraps the first admin of the portal and the admin pages:
#   scripts/identity-admin.sh grant-role <handle> admin
#   scripts/identity-admin.sh revoke-role <handle> admin
#   scripts/identity-admin.sh set-password <handle>      # reads the password from standard input
#
# Settings (environment):
#   SC_IDENTITY_PORT   local port for the forward   (15050)
#   SC_KUBE_CONTEXT    cluster                      (kind-scbackend)
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
cd "$repo"
context=${SC_KUBE_CONTEXT:-kind-scbackend}
port=${SC_IDENTITY_PORT:-15050}
(($# > 0)) || { sed -n '2,10p' "$0"; exit 1; }

(cd tools/sc-admin && cargo build --release -q)
admin=tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin

kubectl --context "$context" -n scbackend port-forward svc/sc-identity "$port:50051" >/dev/null &
forward=$!
trap 'kill "$forward" 2>/dev/null || true; wait "$forward" 2>/dev/null || true' EXIT
for _ in {1..30}; do
  (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
  sleep 1
done
"$admin" "$@" --identity "http://127.0.0.1:$port"
