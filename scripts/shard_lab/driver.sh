#!/bin/bash
# Goal 9's lab (plan 01a1a87b2cec, docs/runtime-lab.md "Goal 9"): each shard's delay to the microsecond and its busy
# share, and what held the shards past 100 µs, calm and with CPU hogs, in production's layout, the configurations taking
# turns round by round so the machine's drift falls on each alike (goal 6's scripts/overload_lab/driver.sh). The lab's
# clients and runners run on LAB_CPUS and the server on SERVER_CPUS, so the lab measures the server, not itself.
#   driver.sh <kiln> <kiln-lab> <out>     (ROUNDS, N, LAB_CPUS, SERVER_CPUS, HOG_CPUS, CORES, CORES_HOGS, CONFIGS, PROFILE,
#                                          SHARDS, SERVER_ENV, PROF, PROF_KILN, PERF, PERF_LIBS, PERF_CALL_GRAPH, PERF_FROM,
#                                          COMPARE
#                                          from the environment)
# Every configuration's server runs SHARDS shards (KILN_RT_SHARDS; 4, what the server sizes itself to on the 16 CPUs of
# SERVER_CPUS, so cores of its own, fewer CPUs, don't change the count), with SERVER_ENV's KEY=VALUE pairs besides.
#   calm:        the server alone in a scope of its own at CPUWeight=1000 (kiln.slice's weight on bare metal) on
#                SERVER_CPUS; no hogs.
#   hogs:        that, and N busy loops in a scope of their own at cpu.weight 1 (CI legs' and agents' weight) on HOG_CPUS,
#                the server's: busy in user space, never entering the kernel. Weights alone.
#   cores-calm:  the server on cores of its own, CORES (its scope's AllowedCPUs= and `[server] cores`, as goal 8's cpuset
#                partition gives it on bare metal): its shards each on one (the second on), its other threads on the
#                rest (rt::pin); no hogs.
#   cores:       that, and N busy loops at cpu.weight 1 on CORES_HOGS, the CPUs past the server's.
#   q<µs>-<cfg>: <cfg> with the shards' step quantum at <µs> (KILN_RT_QUANTUM_US): step 3's A/B of quanta on one build.
#   nudge-<mode>-<cfg>: <cfg> with `[server] nudge` at <mode>, off, on or auto (KILN_RT_NUDGE; goal 8 step 7 and goal 6
#                step 3's A/B on one build). Without it a configuration runs the build's default, off (off until
#                declared: docs/ci.md §0.3).
#   rt-<cfg>:    <cfg> with the server's shards in Linux's real-time class (goal 9 step 4: KILN_RT_REALTIME=1, the lab's
#                unit given LimitRTPRIO=1 as kiln install's unit has, the server's soft limit raised to it). Run only
#                where this user's service manager grants a unit that room (the most it gives is its own hard
#                RLIMIT_RTPRIO, which only root raises); elsewhere skipped, and the log says so. rt-hogs is the shards
#                in the class beside the hogs, rt-cores the class on cores of their own.
#   spin-<cfg>:  <cfg> with each shard spinning before its parks as long as its own idle gaps make cheapest in CPU, up to
#                16 µs (goal 9 step 4: KILN_RT_SPIN=1; hyper-rt's park_spin): it spends less CPU than parking at once
#                wherever it spins, so it's the busy-share-safe spin.
#   spin<µs>-<cfg>: <cfg> with each shard spinning <µs> before every park whatever it costs (KILN_RT_SPIN_US): what a
#                longer spin buys in delay, and costs in busy share (goal 9 step 5's bar).
#   nudge<µs>-<cfg>: <cfg> with the nudge's period at <µs> (KILN_RT_NUDGE_US) where kiln derives the step quantum less
#                the timers' tick, 166.7 µs (goal 9 step 4); with no nudge-<mode>- after it, the nudge on.
#                Prefixes combine in the order spin, nudge<µs>, nudge-<mode>, rt: spin-nudge-on-hogs, nudge100-hogs,
#                spin16-rt-hogs.
# Each round's kiln-lab run goes to <out>/runs/<configuration>/<round>; a round measured before is kept, so the driver
# started again goes on where it stopped. With PROF=1, each configuration then gets one round more (runs/prof-<cfg>)
# with PROF_KILN (an unstripped build of the same commit) and perf recording its server over the window (hook.sh),
# kept apart from the measured rounds; its report beside it (perf.txt). Every round's hook writes which CPUs each of the
# server's threads and children may run on, and each thread's scheduling policy, as its window ends
# (<round>.hook/affinity.txt); the log says how many of the shards' threads ran in the real-time class. At the end each
# configuration's rounds are one run.json (scripts/priority_lab/merge.py), for `kiln-lab table` and `kiln-lab compare`,
# and <out>/report.md holds the table of them all and COMPARE's pairs (a:b, each against the other). Linux with systemd's
# user manager.
set -u
here=$(cd "$(dirname "$0")" && pwd)
kiln=$(readlink -f "$1") lab=$(readlink -f "$2") D=$3
ROUNDS=${ROUNDS:-8} N=${N:-16} LAB_CPUS=${LAB_CPUS:-0-15} SERVER_CPUS=${SERVER_CPUS:-16-31} HOG_CPUS=${HOG_CPUS:-$SERVER_CPUS}
CORES=${CORES:-16-23} CORES_HOGS=${CORES_HOGS:-24-31} CONFIGS=${CONFIGS:-calm hogs cores-calm cores}
PROFILE=${PROFILE:-realistic} PROF=${PROF:-0} PROF_KILN=${PROF_KILN:-} PERF=${PERF:-perf} PERF_LIBS=${PERF_LIBS:-}
# How perf records the profiled rounds' call graphs (hook.sh): DWARF by default; `fp` for a PROF_KILN built with frame
# pointers (RUSTFLAGS="-C force-frame-pointers=yes"), which perf unwinds as it records, where DWARF's unwinding at report
# time didn't resolve on g9c's data (goal 9, plan 01a1a87b2cec step 5).
PERF_CALL_GRAPH=${PERF_CALL_GRAPH:-dwarf,16384}
# Where the profiled rounds' perf starts (hook.sh): `window` (the default), `fill` (sessions' Open, the owners' first
# events) or `server` (before the runners connect: their Build and ReachHello); it stops as the window ends (goal 9,
# plan 01a1a87b2cec step 3).
PERF_FROM=${PERF_FROM:-window}
SHARDS=${SHARDS-4} SERVER_ENV=${SERVER_ENV:-}
# The pairs the report compares: each configuration against weights alone with hogs, cores against the class on them,
# and calm against the class calm (goal 9 step 4's A/B).
COMPARE=${COMPARE:-calm:hogs hogs:cores hogs:rt-hogs hogs:rt-cores cores:rt-cores calm:rt-calm cores-calm:cores}
export TMPDIR=${TMPDIR:-$HOME/.cache/kiln-lab/tmp}
mkdir -p "$D/bin" "$D/runs" "$TMPDIR"
cp -f "$kiln" "$D/bin/kiln" && cp -f "$lab" "$D/bin/kiln-lab" && cp -f "$here/kiln-server" "$D/bin/kiln-server" && cp -f "$here/hook.sh" "$D/bin/hook.sh" || exit 2
if [ "$PROF" = 1 ]; then
  mkdir -p "$D/prof" && cp -f "${PROF_KILN:?PROF=1 needs PROF_KILN}" "$D/prof/kiln" && cp -f "$here/kiln-server" "$D/prof/kiln-server" || exit 2
fi
pass=(-E HOME="$HOME" -E PATH="$PATH" -E TMPDIR="$TMPDIR")
[ -n "${XDG_RUNTIME_DIR:-}" ] && pass+=(-E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR")
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && pass+=(-E DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS")
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$D/runs/driver.log"; }
# The real-time room a unit that asks for LimitRTPRIO=1 gets from this user's service manager, as an rt-* round's lab unit
# asks: 1 where the manager may give it (its own hard limit 1 or more), else what it has (0 on fedora, 2026-11-05).
rt_room=$(systemd-run --user --quiet --wait --pipe --collect -p LimitRTPRIO=1 bash -c 'ulimit -Hr' 2>/dev/null)
rt_ok() { [ "$rt_room" = unlimited ] || { [ "$rt_room" -ge 1 ] 2>/dev/null; }; }
keep=""
for cfg in $CONFIGS; do
  case $cfg in
    rt-* | *-rt-*)
      rt_ok || { say "$cfg: skipped: this user's service manager gives a unit no real-time room (LimitRTPRIO=1 got ${rt_room:-nothing}); root's call (docs/runtime-lab.md, \"Goal 8 step 7 and goal 9 step 4\")"; continue; } ;;
  esac
  keep="$keep $cfg"
done
CONFIGS=$keep
one() {
  local cfg=$1 i=$2 out=$D/runs/$1/$2 bin=$D/bin/kiln-server senv=(--env "LAB_SERVER_CPUS=$SERVER_CPUS") hogcpus="" base=$1
  local run=(-E KILN_LAB_HOOK="$D/bin/hook.sh") unit=()
  if [ -s "$out/run.json" ] && grep -q '"numbers"' "$out/run.json"; then say "round $i $cfg: measured before"; return; fi
  rm -rf "$out"; mkdir -p "$out"
  case "$cfg" in prof-*) base=${cfg#prof-}; bin=$D/prof/kiln-server; run+=(-E PERF_ON=1 -E PERF="$PERF" -E PERF_LIBS="$PERF_LIBS" -E PERF_CALL_GRAPH="$PERF_CALL_GRAPH" -E PERF_FROM="$PERF_FROM") ;; esac
  local quantum=""
  case "$base" in q[0-9]*-*) quantum=${base%%-*}; quantum=${quantum#q}; base=${base#*-} ;; esac
  # The spin before each park (goal 9 step 4), the nudge's period and its mode (goal 9 step 4; goal 8 step 7 and goal
  # 6 step 3), each a prefix before rt-; a period with no mode is the nudge on at it.
  local spin="" nudge_us="" nudge=""
  case "$base" in spin[0-9]*-*) spin=${base%%-*}; spin=${spin#spin}; base=${base#*-} ;; spin-*) spin=sized; base=${base#spin-} ;; esac
  case "$base" in nudge[0-9]*-*) nudge_us=${base%%-*}; nudge_us=${nudge_us#nudge}; base=${base#*-} ;; esac
  case "$base" in nudge-off-* | nudge-on-* | nudge-auto-*) nudge=${base#nudge-}; nudge=${nudge%%-*}; base=${base#nudge-"$nudge"-} ;; esac
  [ -n "$nudge_us" ] && [ -z "$nudge" ] && nudge=on
  local rt=""
  case "$base" in rt-*) rt=1; base=${base#rt-} ;; esac
  case "$base" in
    hogs) hogcpus=$HOG_CPUS ;;
    cores-calm) senv=(--env "LAB_SERVER_CPUS=$CORES" --env "LAB_SERVER_CORES=$CORES") ;;
    cores) senv=(--env "LAB_SERVER_CPUS=$CORES" --env "LAB_SERVER_CORES=$CORES"); hogcpus=$CORES_HOGS ;;
  esac
  [ -n "$SHARDS" ] && senv+=(--env "KILN_RT_SHARDS=$SHARDS")
  [ -n "$quantum" ] && senv+=(--env "KILN_RT_QUANTUM_US=$quantum")
  case "$spin" in "") ;; sized) senv+=(--env KILN_RT_SPIN=1) ;; *) senv+=(--env "KILN_RT_SPIN_US=$spin") ;; esac
  [ -n "$nudge" ] && senv+=(--env "KILN_RT_NUDGE=$nudge")
  [ -n "$nudge_us" ] && senv+=(--env "KILN_RT_NUDGE_US=$nudge_us")
  if [ -n "$rt" ]; then
    senv+=(--env KILN_RT_REALTIME=1 --env LAB_SERVER_RTPRIO=1)
    unit=(-p LimitRTPRIO=1)
  fi
  for kv in $SERVER_ENV; do senv+=(--env "$kv"); done
  if [ -n "$hogcpus" ]; then
    systemd-run --user --scope --quiet --collect --unit="kiln-lab-hogs-$i-$$" -p CPUWeight=1 \
      taskset -c "$hogcpus" bash -c "for _ in \$(seq $N); do sh -c 'while :; do :; done' & done; wait" > "$out/hogs.log" 2>&1 &
    sleep 3
  fi
  say "round $i $cfg: load $(cut -d' ' -f1-3 /proc/loadavg); $(awk '/^MemAvailable:/ {print int($2/1048576)}' /proc/meminfo) GB available"
  systemd-run --user --quiet --collect --wait --unit="kiln-lab-$cfg-$i-$$" -p MemoryMax=16G "${unit[@]}" --working-directory="$D" "${pass[@]}" "${run[@]}" \
    taskset -c "$LAB_CPUS" "$D/bin/kiln-lab" run --kiln "$bin" --label "$cfg" --profile "$PROFILE" --rounds 1 "${senv[@]}" --out "$out" > "$out/lab.log" 2>&1
  say "round $i $cfg: lab exited $?"
  if [ -n "$hogcpus" ]; then
    systemctl --user stop "kiln-lab-hogs-$i-$$.scope" 2>/dev/null
    wait 2>/dev/null
  fi
  # How the shards ran, as the hook saw them as the window ended: their threads (hyper-rt-shard-N, cut to 15 bytes) in
  # the real-time class (policy 2, SCHED_RR) of all of them.
  for aff in "$out"/rounds/*.hook/affinity.txt; do
    [ -s "$aff" ] || continue
    say "round $i $cfg: $(awk '$2 ~ /^hyper-rt-shard/ {n++; if ($5 == "policy=2") rr++} END {printf "%d of %d shard threads in the real-time class", rr, n}' "$aff")"
  done
  for data in "$out"/rounds/*.hook/perf.data; do
    [ -s "$data" ] || continue
    # Every thread's by symbol; then the shards' alone (their threads' names, hyper-rt-shard-N, cut to 15 bytes), by
    # symbol with who called it.
    # --no-inline: resolving inlined frames through a 1.2 GB unstripped build's line tables took perf 17+ minutes a
    # report on g9p6 (2026-11-05), with it 19 s; the symbols are the same.
    LD_LIBRARY_PATH=$PERF_LIBS "$PERF" report -i "$data" --no-inline --no-children --sort comm,dso,symbol --stdio -g none --percent-limit 0.2 > "${data%.data}.txt" 2>/dev/null
    LD_LIBRARY_PATH=$PERF_LIBS "$PERF" report -i "$data" --no-inline --children --comms hyper-rt-shard- --sort symbol --stdio -g none --percent-limit 0.5 > "${data%.data}-shards-children.txt" 2>/dev/null
    LD_LIBRARY_PATH=$PERF_LIBS "$PERF" report -i "$data" --no-inline --no-children --comms hyper-rt-shard- --sort symbol --stdio -g caller,0.5,callee,function,percent --percent-limit 0.5 > "${data%.data}-shards.txt" 2>/dev/null
  done
}
say "start: $ROUNDS rounds of$CONFIGS ($PROFILE), $N hogs; the lab on CPUs $LAB_CPUS, the server on $SERVER_CPUS with ${SHARDS:-its own count of} shards${SERVER_ENV:+ and $SERVER_ENV}, hogs on $HOG_CPUS (cores: $CORES, its hogs on $CORES_HOGS); real-time room ${rt_room:-unknown}; $(uptime)"
for i in $(seq "$ROUNDS"); do
  set -- $CONFIGS
  # Each round starts the turn one further on.
  for _ in $(seq $(( (i - 1) % $# ))); do set -- "${@:2}" "$1"; done
  for cfg in "$@"; do one "$cfg" "$i"; done
done
if [ "$PROF" = 1 ]; then
  for cfg in $CONFIGS; do one "prof-$cfg" 1; done
fi
python3 -I "$here/../priority_lab/merge.py" "$D/runs" $CONFIGS >> "$D/runs/driver.log" 2>&1
# The report: every configuration's numbers side by side, and the comparisons goal 9 step 4's A/B reads.
{
  echo "# Goal 9's lab: $(date -u +%Y-%m-%dT%H:%MZ), $ROUNDS rounds of$CONFIGS"
  echo
  tab=()
  for c in $CONFIGS; do [ -s "$D/runs/$c/run.json" ] && tab+=("$c=$D/runs/$c/run.json"); done
  [ ${#tab[@]} -gt 0 ] && "$D/bin/kiln-lab" table "${tab[@]}"
  for pair in $COMPARE; do
    a=${pair%%:*} b=${pair#*:}
    [ -s "$D/runs/$a/run.json" ] && [ -s "$D/runs/$b/run.json" ] || continue
    echo
    echo "## $a against $b"
    "$D/bin/kiln-lab" compare "$D/runs/$a/run.json" "$D/runs/$b/run.json"
  done
} > "$D/report.md" 2>&1
say "done; report in $D/report.md; $(uptime)"
echo DONE > "$D/runs/DONE"
