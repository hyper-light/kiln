#!/bin/bash
# Goal 7's lab (plan 01a1a59c2aea step 4; docs/runtime-lab.md, "Goal 7: placement"): every placement against
# today's (round-robin), one build, the placement set by KILN_RT_PLACEMENT alone, round by round in turn, in
# three conditions, so the machine's drift falls on each alike:
#   prod:   the production-like layout: the server alone in a scope of its own at CPUWeight=1000 (kiln.slice's
#           weight, docs/priority.md), N busy loops in another scope at CPUWeight=1, the lab and its runners in
#           the lab's unit at the default weight;
#   shared: the server, its runners, the lab and N of kiln-lab's own hogs, all in the lab's unit, at one weight;
#   calm:   the server in its scope at CPUWeight=1000, no hogs.
# Everything held to the same CPUs (taskset), the efficient cores as main's baseline. Beside this script:
# bin/kiln and bin/kiln-lab (release), kiln-server-scope.sh and hogs.sh. Each round's kiln-lab run goes to
# runs/<condition>-<placement>/<round>; merge.py makes each one's rounds one run.json, for
# `kiln-lab table` and `compare` (report.sh). Run again, it measures only the rounds not measured yet.
D=$(cd "$(dirname "$0")" && pwd)
ROUNDS=${ROUNDS:-6} N=${N:-16} CPUS=${CPUS:-16-31}
MODES=${MODES:-round-robin steer steal both}
CONDS=${CONDS:-prod shared calm}
RUNS=${RUNS:-runs}
mkdir -p "$D/$RUNS"
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$D/$RUNS/driver.log"; }
# Another lab on the machine says it holds it by keeping HOLD_FILE fresh: each round waits while it does, so the
# two never measure at once.
held_by_another() { [ -n "${HOLD_FILE:-}" ] && [ -e "$HOLD_FILE" ] && [ $(( $(date +%s) - $(stat -c %Y "$HOLD_FILE") )) -lt 120 ]; }
one() {
  cond=$1 mode=$2 i=$3
  out=$D/$RUNS/$cond-$mode/$i
  # A round already measured (a run stopped part-way, by its machine's reset, picks up after it).
  if grep -q '"numbers"' "$out/run.json" 2>/dev/null; then
    say "round $i $cond $mode: measured already"
    return
  fi
  mkdir -p "$out"
  if held_by_another; then
    say "round $i $cond $mode: waiting while $HOLD_FILE says another lab holds the machine"
    while held_by_another; do sleep 30; done
  fi
  hogs=0 kiln=$D/kiln-server-scope.sh legs=
  case "$cond" in
    prod)
      systemd-run --user --scope --quiet --collect --unit="g7-hogs-$cond-$mode-$i" -p CPUWeight=1 "$D/hogs.sh" "$N" "$CPUS" > "$out/hogs.log" 2>&1 &
      legs=$!
      sleep 2 ;;
    shared) hogs=$N kiln=$D/bin/kiln ;;
  esac
  say "round $i $cond $mode: load $(cut -d' ' -f1-3 /proc/loadavg)"
  systemd-run --user --quiet --collect --wait --unit="g7-lab-$cond-$mode-$i" -p MemoryMax=16G \
    --working-directory="$D" -E HOME="$HOME" -E PATH="$PATH" \
    taskset -c "$CPUS" "$D/bin/kiln-lab" run --kiln "$kiln" --label "$mode" --env "KILN_RT_PLACEMENT=$mode" \
    --rounds 1 --hogs "$hogs" --out "$out" > "$out/lab.log" 2>&1
  say "round $i $cond $mode: lab exited $?"
  if [ -n "$legs" ]; then
    systemctl --user stop "g7-hogs-$cond-$mode-$i.scope" 2>/dev/null
    wait "$legs" 2>/dev/null
  fi
}
say "start: $ROUNDS rounds of $CONDS x $MODES, $N hogs on CPUs $CPUS; $(uptime)"
for i in $(seq "$ROUNDS"); do
  set -- $CONDS
  for _ in $(seq $(( (i - 1) % $# ))); do set -- "${@:2}" "$1"; done
  for cond in "$@"; do
    # Each condition's placements take turns, the turn one further on each round.
    set -- $MODES
    for _ in $(seq $(( (i - 1) % $# ))); do set -- "${@:2}" "$1"; done
    for mode in "$@"; do one "$cond" "$mode" "$i"; done
    set -- $CONDS
  done
done
say "done; $(uptime)"
echo DONE > "$D/$RUNS/DONE"
