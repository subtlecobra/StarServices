#!/usr/bin/env bash
# Replaces the DGS pods of the shards that have players with the sc-dgs image of a release manifest, in place, to test a
# change that only affects the DGS without a new shard: the pods keep their names and addresses, the shards are not
# drained or recreated, and no other service is touched. A pod whose DGS restarts under a player in game makes its
# replicant pause the client with a server error dialog and bind the new DGS's entities to it (see docs/DISCONNECT.md),
# so the player stays in game. With SC_REPLICANT_DGS_RESTART=kick on the replicant the client returns to the menu instead
# and the player joins the same shard again.
#
#   scripts/dgs-roll.sh [--context C] [--release FILE] [--shards-namespace NS] [--dry-run] [shard...]
#
# Without shard names, every shard that has players. A shard whose DGS pods already run the release's image is
# refused (nothing to do). Publish first (scripts/publish.sh sc-dgs) so the manifest names the new image.
#
# What it writes to the cluster: the image of the container `dgs` of pods that carry the label scbackend.io/dgs, in the
# namespace of a shard. Nothing else. `--dry-run` writes nothing and prints the same plan.
#
# A shard created later gets the image the shard operator is set to (a normal deployment changes that); a pod rolled here
# stays on its image until its shard is replaced, as the operator does not compare a pod's live image, only the manifest
# it made it from.
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")

DGS_IMAGE_KEY=sc-dgs
DGS_CONTAINER=dgs
DGS_LABEL=scbackend.io/dgs
SHARD_RESOURCE=shards.scbackend.io
READY_TIMEOUT_SECONDS=300
POLL_SECONDS=2

context=""
release="$repo/target/release.yaml"
shards_namespace=scbackend
dry_run=0
names=()
while (($#)); do
  case $1 in
    --context) context=$2; shift 2 ;;
    --release) release=$2; shift 2 ;;
    --shards-namespace) shards_namespace=$2; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "dgs-roll: unknown option $1" >&2; exit 2 ;;
    *) names+=("$1"); shift ;;
  esac
done
kubectl_args=()
if [[ -n $context ]]; then kubectl_args=(--context "$context"); fi
k() { kubectl "${kubectl_args[@]}" "$@"; }

# The tag of the DGS image in the manifest: `<version>@sha256:<digest>`.
tag=$(sed -n "s/^  $DGS_IMAGE_KEY: \"\\(.*\\)\"\$/\\1/p" "$release")
if [[ -z $tag ]]; then
  echo "dgs-roll: no $DGS_IMAGE_KEY in $release (run scripts/publish.sh $DGS_IMAGE_KEY first)" >&2
  exit 1
fi

# The shards to roll: `name namespace players` per line.
mapfile -t rows < <(k -n "$shards_namespace" get "$SHARD_RESOURCE" \
  -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.status.namespace}{" "}{.status.players}{"\n"}{end}')
selected=()
for row in "${rows[@]}"; do
  read -r name namespace players <<<"$row"
  [[ -n $namespace ]] || continue
  if ((${#names[@]})); then
    for wanted in "${names[@]}"; do [[ $wanted == "$name" ]] && selected+=("$name $namespace"); done
  elif ((${players:-0} > 0)); then
    selected+=("$name $namespace")
  fi
done
if ((${#selected[@]} == 0)); then
  echo "dgs-roll: no shard to roll (none has players, or none of the names exists)" >&2
  exit 1
fi

# The pods to change, as `namespace pod image`, per shard; a shard with none to change is refused.
plan=()
for entry in "${selected[@]}"; do
  read -r name namespace <<<"$entry"
  changes=0
  while read -r pod current; do
    [[ -n $pod ]] || continue
    # The registry and repository the pod already pulls from, with the release's tag: no deployment's address is named here.
    repository=${current%@*}
    repository=${repository%:*}
    wanted="$repository:$tag"
    if [[ $current == "$wanted" ]]; then
      echo "shard $name: $pod already runs $tag"
    else
      shown=${current%@*}
      echo "shard $name: $pod  ${shown##*:} -> $tag"
      plan+=("$namespace $pod $wanted")
      changes=$((changes + 1))
    fi
  done < <(k -n "$namespace" get pods -l "$DGS_LABEL" -o jsonpath="{range .items[*]}{.metadata.name}{\" \"}{.spec.containers[?(@.name==\"$DGS_CONTAINER\")].image}{\"\\n\"}{end}")
  if ((changes == 0)); then
    echo "dgs-roll: shard $name has nothing to change: its DGS pods run $tag already (or it has none)" >&2
  fi
done
if ((${#plan[@]} == 0)); then
  exit 1
fi
if ((dry_run)); then
  echo "dgs-roll: dry run, ${#plan[@]} pod(s) would change; nothing written"
  exit 0
fi

for item in "${plan[@]}"; do
  read -r namespace pod wanted <<<"$item"
  k -n "$namespace" set image "pod/$pod" "$DGS_CONTAINER=$wanted"
done

# Ready again on the new image: the restarted container reports the new digest and the pod is ready.
digest=${tag#*@}
deadline=$((SECONDS + READY_TIMEOUT_SECONDS))
for item in "${plan[@]}"; do
  read -r namespace pod _ <<<"$item"
  while :; do
    state=$(k -n "$namespace" get pod "$pod" \
      -o jsonpath="{.status.containerStatuses[?(@.name==\"$DGS_CONTAINER\")].imageID}{\" \"}{.status.containerStatuses[?(@.name==\"$DGS_CONTAINER\")].ready}" 2>/dev/null || true)
    if [[ $state == *"$digest"*" true" ]]; then
      echo "$namespace/$pod ready on $tag"
      break
    fi
    if ((SECONDS > deadline)); then
      echo "dgs-roll: $namespace/$pod is not ready on the new image after ${READY_TIMEOUT_SECONDS}s" >&2
      exit 1
    fi
    sleep "$POLL_SECONDS"
  done
done
echo "dgs-roll: ${#plan[@]} DGS pod(s) replaced; the clients on them get a server error dialog and recover on the new DGS (a replicant set to kick sends them to the menu: join again)"
