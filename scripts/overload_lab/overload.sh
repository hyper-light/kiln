#!/bin/bash
# Goal 6's lab, plan 01a1a59c2abe step 4 (docs/runtime-lab.md "Goal 6"): the server overloaded on purpose (the
# overload profile, one shard), shedding off (A) against on (B, KILN_SHED=1), interleaved: kiln-lab ab of one build,
# its server alone in a scope at CPUWeight=1000 (kiln-scoped), the lab and everything it starts on CPUs $CPUS.
#   overload.sh <kiln> <kiln-lab> <out>     (ROUNDS, CPUS, PROFILE, SHARDS from the environment; SHARDS=default:
#                                            the server's own sizing)
set -u
here=$(cd "$(dirname "$0")" && pwd)
kiln=$(readlink -f "$1") lab=$(readlink -f "$2") D=$3
ROUNDS=${ROUNDS:-6} CPUS=${CPUS:-16-31} PROFILE=${PROFILE:-overload} SHARDS=${SHARDS:-1}
export TMPDIR=${TMPDIR:-$HOME/.cache/kiln-lab/tmp}
mkdir -p "$D/bin" "$TMPDIR"
cp -f "$kiln" "$D/bin/kiln" && cp -f "$lab" "$D/bin/kiln-lab" && cp -f "$here/kiln-scoped" "$D/bin/kiln-scoped" || exit 2
shards=()
[ "$SHARDS" != default ] && shards=(--env-a "KILN_RT_SHARDS=$SHARDS" --env-b "KILN_RT_SHARDS=$SHARDS")
echo "$(date -u +%FT%TZ) start: $PROFILE, shards $SHARDS, $ROUNDS rounds each; $(uptime)" >> "$D/driver.log"
taskset -c "$CPUS" "$D/bin/kiln-lab" ab --a "$D/bin/kiln-scoped" --b "$D/bin/kiln-scoped" --label-a shed-off --label-b shed-on \
  "${shards[@]}" --env-b KILN_SHED=1 --profile "$PROFILE" --rounds "$ROUNDS" --out "$D/run" > "$D/lab.log" 2>&1
echo "$(date -u +%FT%TZ) lab exited $?; $(uptime)" >> "$D/driver.log"
echo DONE > "$D/DONE"
