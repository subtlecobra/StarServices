#!/usr/bin/env bash
# Packs one seed bundle as an OCI image and pushes it to the deploy registry (scripts/registry.sh), as
# <registry>/scbackend/seed:<build>, which the sc-seed chart's Job (sc-seeder) pulls and applies in the cluster:
#   scripts/seed-push.sh <out dir> [build]
# <out dir> is `sc-admin seed-export`'s; the build defaults to the only one in it that has a manifest.
# scripts/seed-publish.sh runs both from Data.p4k.
#
# The image is FROM scratch with the files under /seed/<build>/ (manifest.json and one directory per owner) and nothing
# else, so any OCI registry takes it. It is built with --timestamp 0, so the same bundle gives the same digest and a push
# of an unchanged build uploads nothing.
set -euo pipefail
cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.."
source scripts/registry.sh
out=${1:?usage: scripts/seed-push.sh <out dir> [build]}
build=${2:-}
if [[ -z $build ]]; then
  builds=("$out"/*/manifest.json)
  [[ ${#builds[@]} == 1 && -f ${builds[0]} ]] || { echo "give the build: $out holds ${#builds[@]} with a manifest" >&2; exit 1; }
  build=$(basename "$(dirname "${builds[0]}")")
fi
dir=$out/$build
manifest=$dir/manifest.json
# seed-export writes the manifest last, so without one the bundle is partial.
[[ -f $manifest ]] || { echo "$manifest: missing, the bundle is not complete; run sc-admin seed-export" >&2; exit 1; }
[[ $(jq -r .build "$manifest") == "$build" ]] || { echo "$manifest: not build $build" >&2; exit 1; }
# The image holds the whole directory, so it must be exactly what the manifest lists, with those digests.
listed=$(jq -r '.files[].path' "$manifest" | sort)
present=$(cd "$dir" && find . -type f ! -name manifest.json | sed 's|^\./||' | sort)
[[ $listed == "$present" ]] || { echo "$dir: files differ from the manifest:" >&2; diff <(echo "$listed") <(echo "$present") >&2; exit 1; }
(cd "$dir" && jq -r '.files[] | "\(.sha256)  \(.path)"' manifest.json | sha256sum -c --quiet)

image=$registry/scbackend/seed:$build
containerfile=$(mktemp)
trap 'rm -f "$containerfile"' EXIT
printf 'FROM scratch\nCOPY . /seed/%s/\n' "$build" >"$containerfile"
podman build -q --timestamp 0 -f "$containerfile" -t "$image" "$dir" >/dev/null
if [[ $registry_plain_http == true ]]; then
  podman push -q --tls-verify=false "$image"
else
  podman push -q "$image"
fi
echo "$image"
