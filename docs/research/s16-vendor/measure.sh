#!/usr/bin/env bash
# kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16: what carrying Hex and Shards costs kiln's build and its
# binary on this host. Run from a kiln checkout (it makes worktrees of its own):
#
#   docs/research/s16-vendor/measure.sh <base> <head> [runs]
#
# <base>: a kiln commit before step 16 (main at 3a4b090054c5); <head>: one with it. Each run (3 unless given) is a
# clean `cargo build --release --locked -p kiln` of each of:
#   base           kiln before step 16;
#   head-unpacked  kiln with Hex and Shards' crates (shards-agentfile, shards-api, hex-node), carrying no Shards;
#   pack           ci/shards/pack.sh: Shards' binary and its VM and network processes, from vendor/shards;
#   head-carrying  kiln with them, carrying what that pack built;
# each in a target directory of its own, emptied first, under $S16_DIR (default ~/.cache/s16-measure), at
# `nice -n 10`. Its records: $S16_DIR/measure.tsv (run, what, seconds, bytes: kiln's binary; for pack, the three
# carried files together) and each carried file's size; then n, p50, p90, p99 and max of each, with the host,
# its OS and the two revisions. A machine's other work counts: say what else ran.
set -euo pipefail
base=${1:?base commit}
head=${2:?head commit}
runs=${3:-3}
src=$(git rev-parse --show-toplevel)
dir=${S16_DIR:-$HOME/.cache/s16-measure}
mkdir -p "$dir"
for w in base head; do
  rev=$base; [ "$w" = head ] && rev=$head
  if [ -d "$dir/$w" ]; then git -C "$dir/$w" checkout -q -f --detach "$rev"; else git -C "$src" worktree add -q --detach "$dir/$w" "$rev"; fi
done
host=$(rustc -vV | sed -n 's/^host: //p')
exe=""; case $host in *-windows-*) exe=.exe ;; esac
tsv="$dir/measure.tsv"
printf 'run\twhat\tseconds\tbytes\n' > "$tsv"
size() { wc -c < "$1" | tr -d ' '; }
timed() { local s; s=$(date +%s); "$@" > "$dir/last.log" 2>&1 || { tail -40 "$dir/last.log" >&2; return 1; }; echo $(( $(date +%s) - s )); }
for run in $(seq 1 "$runs"); do
  for what in base head-unpacked pack head-carrying; do
    t="$dir/target-$what"
    rm -rf "$t"
    case $what in
      base) secs=$(cd "$dir/base" && CARGO_TARGET_DIR=$t KILN_UI_SERVER=off timed nice -n 10 cargo build --release --locked -p kiln); bytes=$(size "$t/release/kiln$exe") ;;
      head-unpacked) secs=$(cd "$dir/head" && CARGO_TARGET_DIR=$t KILN_UI_SERVER=off KILN_SHARDS_DIR="$dir/none" timed nice -n 10 cargo build --release --locked -p kiln); bytes=$(size "$t/release/kiln$exe") ;;
      pack)
        secs=$(cd "$dir/head" && CARGO_TARGET_DIR=$t KILN_SHARDS_DIR="$dir/carried" timed nice -n 10 ci/shards/pack.sh)
        bytes=0
        for f in shards shards-vm shards-net; do b=$(size "$dir/carried/$f$exe"); bytes=$(( bytes + b )); printf '%s\tcarried %s\t-\t%s\n' "$run" "$f" "$b" >> "$tsv"; done ;;
      head-carrying) secs=$(cd "$dir/head" && CARGO_TARGET_DIR=$t KILN_UI_SERVER=off KILN_SHARDS_DIR="$dir/carried" timed nice -n 10 cargo build --release --locked -p kiln); bytes=$(size "$t/release/kiln$exe") ;;
    esac
    printf '%s\t%s\t%s\t%s\n' "$run" "$what" "$secs" "$bytes" >> "$tsv"
    echo "run $run: $what ${secs}s ${bytes} bytes"
  done
done
python3 - "$tsv" "$host" "$(uname -sr)" "$(git -C "$dir/base" rev-parse HEAD)" "$(git -C "$dir/head" rev-parse HEAD)" <<'PY'
import csv, sys
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
print(f"host {sys.argv[2]}, {sys.argv[3]}; base {sys.argv[4]}, head {sys.argv[5]}")
def q(xs, p):
    xs = sorted(xs); i = min(len(xs) - 1, max(0, round(p * (len(xs) - 1))))
    return xs[i]
for what in dict.fromkeys(r["what"] for r in rows):
    for key in ("seconds", "bytes"):
        xs = [int(r[key]) for r in rows if r["what"] == what and r[key] not in ("-", "")]
        if xs:
            print(f"{what:16} {key:8} n={len(xs)} p50={q(xs,.5)} p90={q(xs,.9)} p99={q(xs,.99)} max={max(xs)}")
PY
