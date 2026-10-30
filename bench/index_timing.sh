#!/usr/bin/env bash
# Index build, no-op update, incremental update and lookup timings on a corpus checkout.
#
#   bench/index_timing.sh CORPUS_DIR [N_CHANGED_FILES]
#
# The incremental step builds a synthetic commit that appends a comment to N C/C++ files, using git
# plumbing only (temporary index, commit-tree): the worktree and HEAD are never touched. It indexes
# that commit, then HEAD again, so the index ends where it started.
set -euo pipefail

corpus=$(cd "$1" && pwd)
n=${2:-200}
here=$(cd "$(dirname "$0")/.." && pwd)
bin="$here/target/release/orch-index"
oi() { "$bin" --repo "$corpus" "$@"; }
t() { local s; s=$(python3 -c 'import time;print(time.time())'); "$@" >/dev/null; python3 -c "import time;print(f'{time.time()-$s:.3f}')"; }

cargo build -q --release --manifest-path "$here/Cargo.toml"
db=$(git -C "$corpus" rev-parse --path-format=absolute --git-common-dir)/orch-index/index.db
head=$(git -C "$corpus" rev-parse HEAD)

echo "## corpus $(basename "$corpus") @ ${head:0:10}"
if [[ "${FRESH:-0}" == 1 ]]; then rm -f "$db" "$db-wal" "$db-shm"; fi
echo "full/initial index:"; /usr/bin/time -l "$bin" --repo "$corpus" index 2>&1 | grep -E 'indexed|time:|maximum resident'
oi stats
echo "no-op update: $(t oi index)s"

tmp_index=$(mktemp)
trap 'rm -f "$tmp_index"' EXIT
export GIT_INDEX_FILE="$tmp_index"
git -C "$corpus" read-tree HEAD
git -C "$corpus" ls-tree -r --name-only HEAD | grep -E '\.(cpp|h|cc)$' | awk -v n="$n" 'NR % 97 == 0 && c < n { c++; print }' |
while read -r path; do
  blob=$( { git -C "$corpus" cat-file blob "HEAD:$path"; printf '\n// orch-index bench touch\n'; } |
          git -C "$corpus" hash-object -w --stdin)
  mode=$(git -C "$corpus" ls-tree HEAD -- "$path" | cut -d' ' -f1)
  git -C "$corpus" update-index --cacheinfo "$mode,$blob,$path"
done
tree=$(git -C "$corpus" write-tree)
synthetic=$(git -C "$corpus" -c user.name=bench -c user.email=bench@localhost commit-tree "$tree" -p HEAD -m "bench: touch $n files")
unset GIT_INDEX_FILE
echo "changed files: $(git -C "$corpus" diff --name-only HEAD "$synthetic" | wc -l | tr -d ' ')"
echo "incremental ($n files) -> synthetic:"; oi index --rev "$synthetic" | sed 's/^/  /'
echo "incremental back -> HEAD:";           oi index --rev HEAD | sed 's/^/  /'

echo "lookups: see bench/index_accuracy.py (times each question's real query)"
