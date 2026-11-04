#!/bin/bash
# Goal 6's lab, plan 01a1a59c2abe step 3 (docs/runtime-lab.md "Goal 6"): the Critical class's p99 (pages) calm and
# with CPU hogs, in production's layout and in one shared cgroup, round by round, the configurations taking turns
# so the machine's drift falls on each alike (goal 8's scripts/priority_lab/driver.sh).
#   driver.sh <kiln> <kiln-lab> <out>     (ROUNDS, N hogs, CPUS, CONFIGS, PROFILE from the environment)
#   calm:        the server alone in a scope of its own at CPUWeight=1000 (kiln.slice's weight on bare metal), the
#                lab's runners and clients in the lab's unit; no hogs.
#   hogs:        that, and N busy loops in a scope of their own at cpu.weight 1 (CI legs' and agents' weight) on
#                the lab's CPUs.
#   shared-calm: the server in the lab's unit with its runners and clients; no hogs.
#   shared-hogs: that, and the lab's own N hogs (--hogs) in the same unit: one cgroup for all.
# Each round's kiln-lab run goes to <out>/runs/<configuration>/<round>; a round already measured is kept, so the
# driver started again goes on where it stopped. At the end each configuration's rounds are one run.json
# (scripts/priority_lab/merge.py), for `kiln-lab table` and `kiln-lab compare`. Linux with systemd's user manager.
set -u
here=$(cd "$(dirname "$0")" && pwd)
kiln=$(readlink -f "$1") lab=$(readlink -f "$2") D=$3
ROUNDS=${ROUNDS:-8} N=${N:-16} CPUS=${CPUS:-16-31} CONFIGS=${CONFIGS:-calm hogs shared-calm shared-hogs}
PROFILE=${PROFILE:-realistic}
export TMPDIR=${TMPDIR:-$HOME/.cache/kiln-lab/tmp}
mkdir -p "$D/bin" "$D/runs" "$TMPDIR"
cp -f "$kiln" "$D/bin/kiln" && cp -f "$lab" "$D/bin/kiln-lab" && cp -f "$here/kiln-scoped" "$D/bin/kiln-scoped" || exit 2
pass=(-E HOME="$HOME" -E PATH="$PATH" -E TMPDIR="$TMPDIR")
[ -n "${XDG_RUNTIME_DIR:-}" ] && pass+=(-E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR")
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && pass+=(-E DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS")
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$D/runs/driver.log"; }
one() {
  local cfg=$1 i=$2 out=$D/runs/$1/$2 bin extra=()
  if [ -s "$out/run.json" ] && grep -q '"numbers"' "$out/run.json"; then say "round $i $cfg: measured before"; return; fi
  rm -rf "$out"; mkdir -p "$out"
  case "$cfg" in shared-*) bin=$D/bin/kiln ;; *) bin=$D/bin/kiln-scoped ;; esac
  [ "$cfg" = shared-hogs ] && extra=(--hogs "$N")
  if [ "$cfg" = hogs ]; then
    systemd-run --user --scope --quiet --collect --unit="kiln-lab-hogs-$i-$$" -p CPUWeight=1 \
      taskset -c "$CPUS" bash -c "for _ in \$(seq $N); do sh -c 'while :; do :; done' & done; wait" > "$out/hogs.log" 2>&1 &
    sleep 3
  fi
  say "round $i $cfg: load $(cut -d' ' -f1-3 /proc/loadavg); $(awk '/^MemAvailable:/ {print int($2/1048576)}' /proc/meminfo) GB available"
  systemd-run --user --quiet --collect --wait --unit="kiln-lab-$cfg-$i-$$" -p MemoryMax=16G --working-directory="$D" "${pass[@]}" \
    taskset -c "$CPUS" "$D/bin/kiln-lab" run --kiln "$bin" --label "$cfg" --profile "$PROFILE" --rounds 1 "${extra[@]}" --out "$out" > "$out/lab.log" 2>&1
  say "round $i $cfg: lab exited $?"
  if [ "$cfg" = hogs ]; then
    systemctl --user stop "kiln-lab-hogs-$i-$$.scope" 2>/dev/null
    wait 2>/dev/null
  fi
}
say "start: $ROUNDS rounds of $CONFIGS ($PROFILE), $N hogs on CPUs $CPUS; $(uptime)"
for i in $(seq "$ROUNDS"); do
  set -- $CONFIGS
  # Each round starts the turn one further on.
  for _ in $(seq $(( (i - 1) % $# ))); do set -- "${@:2}" "$1"; done
  for cfg in "$@"; do one "$cfg" "$i"; done
done
python3 "$here/../priority_lab/merge.py" "$D/runs" $CONFIGS >> "$D/runs/driver.log" 2>&1
say "done; $(uptime)"
echo DONE > "$D/runs/DONE"
