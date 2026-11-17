#!/usr/bin/env bash
# Checks the services' internal API (proto/internal/) with Buf (https://buf.build), as proto/buf.yaml configures it:
#   1. the protos build, the game's included (proto/sc/, from scripts/protos.sh);
#   2. the lint passes;
#   3. no breaking change against a git ref (the last commit by default): a removed or renumbered field, a changed type,
#      a removed RPC, ... For a breaking change, bump the version of the owner and of every service that calls it (on 0.x
#      the minor version, then the major), and confirm it with SC_ALLOW_BREAKING=1. scripts/publish.sh runs this first.
#
#   scripts/proto-check.sh [git ref]
#
# Needs buf on the PATH (a single binary from its GitHub releases, with sha256.txt to check it).
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
cd "$repo/proto"
against=${1:-HEAD}
command -v buf >/dev/null || { echo "proto-check: buf is not installed (https://github.com/bufbuild/buf/releases)" >&2; exit 1; }
[[ -d sc ]] || { echo "proto-check: proto/sc/ is missing: generate it with scripts/protos.sh" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
buf build -o "$work/now.binpb"
buf lint internal
echo "proto-check: builds and lints"

# The base: the ref's proto/ with this machine's game protos, which git does not hold.
if ! git -C "$repo" cat-file -e "$against:proto/internal" 2>/dev/null; then
  echo "proto-check: $against has no proto/internal: nothing to compare"
  exit 0
fi
mkdir -p "$work/base"
git -C "$repo" archive "$against" proto | tar -x -C "$work/base"
cp -r sc "$work/base/proto/"
(cd "$work/base/proto" && buf build -o "$work/base.binpb")
if ! buf breaking "$work/now.binpb" --against "$work/base.binpb" --path scbackend/internal \
       --config '{"version":"v2","breaking":{"use":["FILE"]}}'; then
  if [[ ${SC_ALLOW_BREAKING:-0} == 1 ]]; then
    echo "proto-check: breaking changes against $against, allowed (SC_ALLOW_BREAKING=1)" >&2
  else
    echo "proto-check: breaking changes against $against (above). Bump the owner's and its callers' versions, then run" >&2
    echo "again with SC_ALLOW_BREAKING=1" >&2
    exit 1
  fi
else
  echo "proto-check: no breaking change against $against"
fi
