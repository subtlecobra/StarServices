#!/usr/bin/env bash
# Generates proto/sc/, the game's own protocol definitions, from your copy of the game. They are recovered from
# StarCitizen.exe by tools/protodump and are not in the repository. scripts/game-files.sh runs this along with the
# other files from the game, once after cloning and again after a game patch; the build stops with a pointer there
# while proto/sc/ is missing. Run this alone to regenerate proto/sc/ only.
#
#   scripts/protos.sh [<path to StarCitizen.exe>]
#
# Settings (environment, or local.env at the repository root, which git ignores):
#   SC_GAME_EXE   the game binary   (Bin64/StarCitizen.exe in the folder of SC_P4K)
#   SC_P4K        the game archive, used only to find the game folder
set -euo pipefail
repo=$(dirname "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")")
if [[ -f $repo/local.env ]]; then source "$repo/local.env"; fi
cd "$repo"

exe=${1:-${SC_GAME_EXE:-}}
if [[ -z $exe && -n ${SC_P4K:-} ]]; then
  exe=$(dirname "$SC_P4K")/Bin64/StarCitizen.exe
fi
[[ -n $exe && -f $exe ]] || {
  echo "protos: no game binary at '${exe}': give its path, or set SC_GAME_EXE or SC_P4K (local.env)" >&2
  exit 1
}

# protodump runs on this machine only, so it builds for the host's glibc rather than the repository's musl target.
(cd tools/protodump && cargo build -q --release --target x86_64-unknown-linux-gnu)
dump=tools/protodump/target/x86_64-unknown-linux-gnu/release/protodump

# Written to a scratch folder first: a failed or partial run never replaces a good proto/sc/.
work=$(mktemp -d "$repo/target/protos.XXXXXX")
trap 'rm -rf "$work"' EXIT
"$dump" "$exe" "$work" proto/vendor
[[ -d $work/sc ]] || { echo "protos: protodump wrote no sc/ folder" >&2; exit 1; }
rm -rf proto/sc
mv "$work/sc" proto/sc
echo "protos: wrote $(find proto/sc -name '*.proto' | wc -l) files to proto/sc/ from $exe"
