#!/usr/bin/env bash
# Packs one converted game build as an OCI image and pushes it to the deploy registry (scripts/registry.sh), as
# <registry>/scbackend/gamedata:<build>, which is what sc-gamedata pulls into each region's cache:
#   scripts/gamedata-push.sh <out dir> [build]
# <out dir> is gamedata-build's (tools/gamedata-build); the build defaults to the only one in it that has a manifest.
# scripts/gamedata-publish.sh runs both from Data.p4k, and is the way to populate the caches.
#
# The image is FROM scratch with the files under /gamedata/<build>/ (manifest.json, tables/ and collision/) and nothing else, so
# any OCI registry takes it: ECR, Artifact Registry, Harbor, Alexandria. It is built with --timestamp 0, so the same files
# give the same digest and a push of an unchanged build uploads nothing.
set -euo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."
source scripts/registry.sh
out=${1:?usage: scripts/gamedata-push.sh <out dir> [build]}
build=${2:-}
if [[ -z $build ]]; then
  builds=("$out"/*/manifest.json)
  [[ ${#builds[@]} == 1 && -f ${builds[0]} ]] || { echo "give the build: $out holds ${#builds[@]} with a manifest" >&2; exit 1; }
  build=$(basename "$(dirname "${builds[0]}")")
fi
dir=$out/$build
manifest=$dir/manifest.json
# gamedata-build writes the manifest last, so without one the build is partial.
[[ -f $manifest ]] || { echo "$manifest: missing, the build is not complete; run gamedata-build" >&2; exit 1; }
[[ $(jq -r .build "$manifest") == "$build" ]] || { echo "$manifest: not build $build" >&2; exit 1; }
# The image holds the whole directory, so it must be exactly what the manifest lists, with those digests.
listed=$(jq -r '.files[].path' "$manifest" | sort)
present=$(cd "$dir" && find . -type f ! -name manifest.json | sed 's|^\./||' | sort)
[[ $listed == "$present" ]] || { echo "$dir: files differ from the manifest:" >&2; diff <(echo "$listed") <(echo "$present") >&2; exit 1; }
(cd "$dir" && jq -r '.files[] | "\(.sha256)  \(.path)"' manifest.json | sha256sum -c --quiet)

image=$registry/scbackend/gamedata:$build
containerfile=$(mktemp)
trap 'rm -f "$containerfile"' EXIT
printf 'FROM scratch\nCOPY . /gamedata/%s/\n' "$build" >"$containerfile"
podman build -q --timestamp 0 -f "$containerfile" -t "$image" "$dir"
if [[ $registry_plain_http == true ]]; then
  podman push -q --tls-verify=false "$image"
else
  podman push -q "$image"
fi
echo "$image"
