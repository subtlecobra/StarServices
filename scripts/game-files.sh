#!/usr/bin/env bash
# Generates every file of this repository that comes from the game, from your own copy of it. None of them is in the
# repository: run this once after cloning and again after a game patch. It reads Data.p4k and StarCitizen.exe and never
# writes in the game's folder.
#
#   scripts/game-files.sh [<path to Data.p4k>] [--golden]
#
# In order:
#   1. proto/sc/, the game's protocol definitions, from StarCitizen.exe (scripts/protos.sh). Every build needs them.
#   2. The game data of the archive's build, converted by tools/gamedata-build into $SC_GAMEDATA_OUT/<build>/ (as
#      scripts/gamedata-publish.sh does, without pushing). A second run of the same build converts only what is missing.
#   3. The DGS's test data, services/dgs/testdata/, copied from that build: the tables (tables/*.tsv) and the collision
#      of the hangar the tests use (collision/<socpak hash>.col), in the layout of a build directory. sc-dgs's,
#      sc-pu-frontend's and sc-admin's tests read them.
#   4. The golden files whose content derives from game data: services/pu-frontend/testdata/golden_binds.tsv, every bind
#      the shard sends (wire_tests.rs, through its SC_WRITE_GOLDEN path), written from the test data of step 3.
#
# A golden file generated here is a snapshot of this machine's game build and of the code as it is when it is written.
# Its tests then catch any later edit that changes the wire. So it is written only when it is missing, when the build
# of the test data changed (a patch), or with --golden, after a deliberate change to the wire. Otherwise it is kept.
#
# Tests and builds stop with a pointer here when these files are missing. A release build needs only proto/sc/.
#
# Settings (environment, or local.env at the repository root, which git ignores; the arguments win):
#   SC_P4K            the game archive                (none; set it in local.env)
#   SC_GAME_EXE       the game binary                 (Bin64/StarCitizen.exe in the folder of the archive)
#   SC_GAMEDATA_OUT   converted builds                (<repo>/target/gamedata)
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"

p4k=${SC_P4K:-}
golden=false
while (($#)); do
  case $1 in
    --golden) golden=true; shift ;;
    -h|--help) sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) echo "game-files: unknown option $1" >&2; exit 1 ;;
    *) p4k=$1; shift ;;
  esac
done
out=${SC_GAMEDATA_OUT:-$repo/target/gamedata}

step() { printf '\ngame-files: %s\n' "$*"; }
fail() { echo "game-files: $*" >&2; exit 1; }

[[ -n $p4k ]] || fail "no game archive: give the path of Data.p4k, or set SC_P4K (local.env)"
[[ -f $p4k ]] || fail "no game archive at '$p4k': give the path of Data.p4k, or set SC_P4K (local.env)"
exe=${SC_GAME_EXE:-$(dirname "$p4k")/Bin64/StarCitizen.exe}
[[ -f $exe ]] || fail "no game binary at '$exe': set SC_GAME_EXE (local.env) to the StarCitizen.exe of this archive"
manifest=$(dirname "$p4k")/build_manifest.id
[[ -f $manifest ]] || fail "no build_manifest.id beside $p4k, so the build is unknown"
build=$(jq -r '.Data.RequestedP4ChangeNum // empty' "$manifest")
[[ -n $build ]] || fail "no Data.RequestedP4ChangeNum in $manifest"
echo "game-files: build $build, archive $p4k, binary $exe"

step "1/4 protocol definitions (proto/sc/)"
scripts/protos.sh "$exe"

step "2/4 game data of build $build ($out/$build)"
# meshoptimizer is C++ and the repository's musl target has no g++ here; the tool runs on this machine only.
cargo build -q --release -p gamedata-build --target x86_64-unknown-linux-gnu
target/x86_64-unknown-linux-gnu/release/gamedata-build "$p4k" "$out" --build "$build"
built=$out/$build
[[ -f $built/manifest.json ]] || fail "gamedata-build left no manifest.json in $built"

step "3/4 DGS test data (services/dgs/testdata/)"
testdata=services/dgs/testdata
# `REST_STOP` in services/dgs/src/physics/world.rs: hangar_smlfrnt_002_rest_rund, Orbituary's hangar.
fixtures=(tables/home_locations.tsv tables/hangar_content.tsv tables/entity_counts.tsv tables/transit_managers.tsv
  tables/transit_gateways.tsv tables/transit_hangars.tsv tables/transit_prefab_entities.tsv collision/1200814982.col)
for file in "${fixtures[@]}"; do
  [[ -f $built/$file ]] || fail "build $build has no $file; the tests' hangar may have changed with the patch"
done
previous=$(cat "$testdata/BUILD" 2>/dev/null || true)
# Copied to a scratch folder first, so a failed run never leaves half a set.
mkdir -p target
work=$(mktemp -d "$repo/target/game-files.XXXXXX")
trap 'command rm -rf "$work"' EXIT
for file in "${fixtures[@]}"; do
  mkdir -p "$work/dgs/$(dirname "$file")"
  command cp -f "$built/$file" "$work/dgs/$file"
done
echo "$build" > "$work/dgs/BUILD"
command rm -rf "$testdata"
command mv -f "$work/dgs" "$testdata"
for file in "${fixtures[@]}"; do
  cmp -s "$built/$file" "$testdata/$file" || fail "$testdata/$file was not copied"
  echo "  $testdata/$file"
done

step "4/4 golden files (services/pu-frontend/testdata/)"
golden_file=services/pu-frontend/testdata/golden_binds.tsv
golden_build=services/pu-frontend/testdata/golden_binds.build
if [[ $golden == false && -f $golden_file && $(cat "$golden_build" 2>/dev/null || true) == "$build" && $previous == "$build" ]]; then
  echo "  $golden_file kept: it is of build $build already (--golden writes it again, after a deliberate wire change)"
else
  # The planet zone on, then off, one test at a time, in the order the file has them.
  : > "$work/golden_binds.tsv"
  SC_WRITE_GOLDEN="$work/golden_binds.tsv" cargo test -q -p sc-pu-frontend --bin sc-pu-frontend -- --exact --test-threads=1 \
    shard::wire_tests::every_bind_through_the_dgss_is_the_bytes_of_before \
    shard::wire_tests::with_the_planet_zone_off_every_bind_is_the_bytes_of_before
  [[ -s $work/golden_binds.tsv ]] || fail "the wire tests wrote no golden binds"
  mkdir -p "$(dirname "$golden_file")"
  command mv -f "$work/golden_binds.tsv" "$golden_file"
  echo "$build" > "$golden_build"
  echo "  $golden_file written: $(wc -l < "$golden_file") rows"
fi

printf '\ngame-files: done; the files above are generated and git ignores them\n'
