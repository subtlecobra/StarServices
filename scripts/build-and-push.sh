#!/usr/bin/env bash
# The images and charts a deploy takes from this repository (scripts/publish.sh runs it, with the game data and seed).
# Every image and chart is pushed under its own version, once, and a version never changes:
#   1. the service images (scripts/build-images.sh), tagged with each service's Cargo.toml version;
#   2. the charts in deploy/charts/, as OCI artifacts in oci://<registry>/scbackend/charts, at their Chart.yaml version.
#      A version the registry lacks is pushed; one it has with the same content is left; one it has with other content
#      stops this: bump the chart's version (semver) and run it again.
# Name services to consider only those images. It writes target/images.tsv and target/charts.tsv (name, version and
# digest), which scripts/publish.sh turns into the release manifest.
#
# Settings (environment):
#   SC_SKIP_BUILD=1   consider no images (target/images.tsv is left as it is)
#   SC_REGISTRY, SC_REGISTRY_PLAIN_HTTP   see scripts/registry.sh
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
# Machine-specific settings, such as SC_P4K, go in local.env at the repository root, which git ignores.
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"
source scripts/registry.sh
[[ -d proto/sc ]] || { echo "proto/sc/ is missing: generate it from your game with scripts/game-files.sh" >&2; exit 1; }
[[ ${SC_SKIP_BUILD:-0} == 1 ]] || scripts/build-images.sh "$@"

packages=$(mktemp -d)
trap 'rm -rf "$packages"' EXIT
push_args=()
[[ $registry_plain_http == true ]] && push_args=(--plain-http)
scheme=https
[[ $registry_plain_http == true ]] && scheme=http
# The digest of <repository>:<tag> in the registry; empty when it has none. -q: no ~/.curlrc (its DNS settings would
# skip /etc/hosts, where the registry name is).
manifest_digest() {
  curl -q -sfI -m 10 -H 'Accept: application/vnd.oci.image.manifest.v1+json' "$scheme://$registry/v2/$1/manifests/$2" |
    tr -d '\r' | awk -F': ' 'tolower($1) == "docker-content-digest" {print $2}'
}

# 2. Our charts, each against the same version in the registry, both unpacked. The name and version come from each
# Chart.yaml (a pre-release version has dashes, so the package's file name cannot be split).
pushed=0 unchanged=0 stale=() charts=()
for chart in deploy/charts/*/; do
  name=$(sed -n 's/^name:[[:space:]]*//p' "$chart/Chart.yaml" | tr -d '"')
  version=$(sed -n 's/^version:[[:space:]]*//p' "$chart/Chart.yaml" | tr -d '"')
  helm package -d "$packages" "$chart" >/dev/null
  package=$packages/$name-$version.tgz
  file=$name-$version
  charts+=("$name $version")
  compare=$packages/compare-$file
  mkdir -p "$compare/local" "$compare/registry"
  tar -xzf "$package" -C "$compare/local"
  if ! helm pull "${push_args[@]}" "oci://$registry/scbackend/charts/$name" --version "$version" --untar \
       -d "$compare/registry" >/dev/null 2>&1; then
    continue
  fi
  if diff -rq "$compare/local" "$compare/registry" >/dev/null; then
    unchanged=$((unchanged + 1))
    rm "$package"
  else
    stale+=("$name $version")
  fi
done
# Uncommitted work is published as release candidates only (CLAUDE.md).
if [[ -n $(git status --porcelain 2>/dev/null) ]]; then
  plain=()
  for package in "$packages"/*.tgz; do
    [[ -e $package ]] || continue
    for entry in "${charts[@]}"; do
      read -r name version <<<"$entry"
      [[ $package == "$packages/$name-$version.tgz" && $version != *-* ]] && plain+=("$name $version")
    done
  done
  if ((${#plain[@]} > 0)); then
    echo "build-and-push: the working tree has uncommitted changes, so these charts would be published as plain versions:" >&2
    printf '  %s\n' "${plain[@]}" >&2
    echo "Give each a release candidate version (X.Y.Z-rc.N) to publish uncommitted work, or commit first." >&2
    exit 1
  fi
fi
if ((${#stale[@]} > 0)); then
  echo "build-and-push: these charts changed but their version is already in $registry with other content:" >&2
  printf '  %s\n' "${stale[@]}" >&2
  echo "Bump each one's version in its Chart.yaml (semver) and run this again. No chart was pushed." >&2
  exit 1
fi
for package in "$packages"/*.tgz; do
  [[ -e $package ]] || continue
  helm push "${push_args[@]}" "$package" "oci://$registry/scbackend/charts" >/dev/null
  echo "pushed $(basename "$package" .tgz)"
  pushed=$((pushed + 1))
done

# Every chart with its digest in the registry, for the release manifest.
for entry in "${charts[@]}"; do
  read -r name version <<<"$entry"
  printf '%s\t%s\t%s\n' "$name" "$version" "$(manifest_digest "scbackend/charts/$name" "$version")"
done > target/charts.tsv
if awk -F'\t' '$3 == ""' target/charts.tsv | grep -q .; then
  echo "build-and-push: the registry has no digest for some charts (target/charts.tsv)" >&2
  exit 1
fi
echo "build-and-push: $pushed charts pushed, $unchanged unchanged (list: target/charts.tsv)"
