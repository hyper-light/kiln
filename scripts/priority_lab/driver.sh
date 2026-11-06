#!/bin/bash
# Goal 8's lab (plan 01a1a59c2b29 step 7, docs/priority.md "The lab"): the API with CPU hogs as legs
# beside it, round by round, the configurations taking turns so the machine's drift falls on each
# alike. Beside this script: bin/kiln and bin/kiln-lab, the builds measured (release), and legs.sh.
# Each round's kiln-lab run goes to runs/<configuration>/<round>; merge.py makes each
# configuration's rounds one run.json, for `kiln-lab table` and `compare`.
#   calm:      the server (the lab's kiln) at CPUWeight=1000, as kiln.slice gives it; no legs.
#   normal:    the server at the default weight (100), legs at normal priority: today's bare metal.
#   weight:    the server at CPUWeight=1000, legs at normal priority: kiln.slice's weight alone.
#   idle:      the server at CPUWeight=1000, legs in a job's cgroup with cpu.idle 1 inside their
#              runner's delegated unit: goal 8's first design, which this lab turned down.
#   idle-unit: the server at CPUWeight=1000, the legs' whole unit idle (CPUWeight=idle), a sibling of
#              the server's: idle where the two compete.
# The lab and the legs held to the same CPUs (the machine's efficient cores, as main's baseline).
D=$(cd "$(dirname "$0")" && pwd)
ROUNDS=${ROUNDS:-4} N=${N:-16} CPUS=${CPUS:-16-31} CONFIGS=${CONFIGS:-calm normal weight idle idle-unit}
RUNS=${RUNS:-runs}
mkdir -p "$D/$RUNS"
# Temporary files under the lab's own folder, never a shared /tmp (a RAM disk on kiln's machines).
export TMPDIR=${TMPDIR:-$D/$RUNS/tmp}
mkdir -p "$TMPDIR"
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$D/$RUNS/driver.log"; }
one() {
  cfg=$1 i=$2
  out=$D/$RUNS/$cfg/$i
  mkdir -p "$out"
  case "$cfg" in normal) weight=100 ;; *) weight=1000 ;; esac
  case "$cfg" in idle) mode=idle-group ;; *) mode=normal ;; esac
  case "$cfg" in idle-unit) unit=idle ;; *) unit=100 ;; esac
  if [ "$cfg" != calm ]; then
    systemd-run --user --scope --quiet --collect --unit="g8-legs-$cfg-$i" -p Delegate=yes -p CPUWeight=$unit "$D/legs.sh" "$mode" "$N" "$CPUS" > "$out/legs.log" 2>&1 &
    legs=$!
    sleep 3
  fi
  say "round $i $cfg: lab at CPUWeight=$weight $(head -1 "$out/legs.log" 2>/dev/null); load $(cut -d' ' -f1-3 /proc/loadavg)"
  systemd-run --user --quiet --collect --wait --unit="g8-lab-$cfg-$i" -p CPUWeight=$weight -p MemoryMax=20G \
    --working-directory="$D" -E HOME="$HOME" -E PATH="$PATH" -E TMPDIR="$TMPDIR" \
    taskset -c "$CPUS" "$D/bin/kiln-lab" run --kiln "$D/bin/kiln" --label "$cfg" --rounds 1 --out "$out" > "$out/lab.log" 2>&1
  say "round $i $cfg: lab exited $?"
  if [ "$cfg" != calm ]; then
    systemctl --user stop "g8-legs-$cfg-$i.scope" 2>/dev/null
    wait "$legs" 2>/dev/null
  fi
}
say "start: $ROUNDS rounds of $CONFIGS, $N legs on CPUs $CPUS; $(uptime)"
for i in $(seq "$ROUNDS"); do
  set -- $CONFIGS
  # Each round starts the turn one further on.
  for _ in $(seq $(( (i - 1) % $# ))); do set -- "${@:2}" "$1"; done
  for cfg in "$@"; do one "$cfg" "$i"; done
done
say "done; $(uptime)"
echo DONE > "$D/$RUNS/DONE"
