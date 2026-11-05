#!/bin/bash
# Step 7's lab (docs/priority.md, "Step 7's lab"; goal 8, plan 01a1a59c2b29 step 7; goal 9, plan 01a1a87b2cec step 4):
# the A/B of what keeps the host off the server's shards, in production's layout (the server alone in a scope of its
# own at CPUWeight=1000, as kiln.slice gives it; hogs at cpu.weight 1, as legs and agents run), the configurations
# taking turns round by round so the machine's drift falls on each alike (scripts/shard_lab/driver.sh's way).
#   s7c.sh KILN KILN_LAB OUT     (ROUNDS, N, LAB_CPUS, SERVER_CPUS, CORES, CORES_HOGS, CONFIGS, PROFILE, SHARDS, CC_SRC,
#                                 HOG_SECS from the environment)
# The lab's clients and runners run on LAB_CPUS (0-15), apart from the server's, so it measures the server.
#   calm        the server on SERVER_CPUS (16-31); no hogs.
#   hogs        that, and N busy loops on SERVER_CPUS, busy in user space, never entering the kernel: weights alone.
#   hogs-cc     that, the hogs N C compilers compiling CC_SRC at -O2 again and again, as CI legs do.
#   cores-calm  the server on cores of its own, CORES (16-23: its scope's AllowedCPUs= and `[server] cores`); no hogs.
#   cores       that, and N busy loops on CORES_HOGS (24-31), where kiln's work CPUs put its own legs and agents.
#   cores-cc    that, the hogs N C compilers.
#   fence       as cores, the hogs a kiln runner's agent (N threads of the stand-in's lab:hog) that kiln itself puts on
#               its work CPUs: a runner held to SERVER_CPUS at cpu.weight 1, its config declaring CORES the server's
#               (hogrunner.sh). Each round notes where the hog's threads may run (fence.txt, its verdict last).
#   rt          the server on SERVER_CPUS with its shards in the real-time class (KILN_RT_REALTIME=1, its
#               RLIMIT_RTPRIO raised to 1 as kiln install's unit grants), N busy loops on SERVER_CPUS.
#   rt-cc       that, the hogs N C compilers.
#   nudge       the server on SERVER_CPUS with its shards nudging their CPUs (KILN_RT_NUDGE=1, `[server] nudge`: each
#               wait in a shard's driver ends by its timer tick, no privilege asked), N busy loops on SERVER_CPUS.
#   nudge-cc    that, the hogs N C compilers.
#   calm-nudge  the nudge with no hogs: what it costs a calm server.
#               NUDGE_US sets the nudge's period for all three (KILN_RT_NUDGE_US; unset, the server's timer tick).
# rt and rt-cc run only where this machine grants a user's process RLIMIT_RTPRIO room for priority 1; elsewhere they
# are skipped and the log says so (the machine owner's call). A *-cc configuration needs CC_SRC (sqlite3.c from
# cargo's registry, unless given). Every configuration's server runs SHARDS shards (4: what it sizes itself to on
# the 16 CPUs of SERVER_CPUS, so fewer CPUs of its own don't change the count).
# Each round notes /healthz/server (healthz.json: the shards' real-time class as they took it, the work CPUs) and how
# busy other work kept the server's CPUs (foreign.json; foreign.py). Each round's kiln-lab run goes to
# OUT/runs/<configuration>/<round>; a round measured before is kept, so the driver started again goes on where it
# stopped. At the end each configuration's rounds are one run.json (merge.py), and OUT/report.md holds kiln-lab's
# table of them all and its comparisons. Linux with systemd's user manager. It stops every unit it started, ending
# or interrupted, so the CPUs are clear after it.
set -u
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$3" || exit 2
kiln=$(readlink -f "$1") lab=$(readlink -f "$2") D=$(cd "$3" && pwd -P)
ROUNDS=${ROUNDS:-8} N=${N:-16} LAB_CPUS=${LAB_CPUS:-0-15} SERVER_CPUS=${SERVER_CPUS:-16-31}
CORES=${CORES:-16-23} CORES_HOGS=${CORES_HOGS:-24-31} PROFILE=${PROFILE:-realistic} SHARDS=${SHARDS-4}
CONFIGS=${CONFIGS:-calm hogs hogs-cc cores-calm cores cores-cc fence rt rt-cc} HOG_SECS=${HOG_SECS:-3600}
CC_SRC=${CC_SRC:-$(ls "$HOME"/.cargo/registry/src/*/libsqlite3-sys-*/sqlite3/sqlite3.c 2>/dev/null | head -1)}
export TMPDIR=${TMPDIR:-$HOME/.cache/kiln-lab/tmp}
mkdir -p "$D/bin" "$D/runs" "$TMPDIR"
cp -f "$kiln" "$D/bin/kiln" && cp -f "$lab" "$D/bin/kiln-lab" && cp -f "$here/kiln-server-s7c" "$D/bin/kiln-server" &&
  cp -f "$here/hogrunner.sh" "$here/foreign.py" "$D/bin/" && chmod +x "$D/bin/kiln-server" || exit 2
pass=(-E HOME="$HOME" -E PATH="$PATH" -E TMPDIR="$TMPDIR")
[ -n "${XDG_RUNTIME_DIR:-}" ] && pass+=(-E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR")
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && pass+=(-E DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS")
say() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$D/runs/driver.log"; }
# Every unit this driver started (their names end in its pid), stopped as it ends, however it ends.
mine() { systemctl --user list-units --all --plain --no-legend 'kiln-lab-s7c-*' 2>/dev/null | awk '{print $1}' | grep -- "-$$[.-]"; }
# The scopes of the servers this driver's rounds started (each found by its config, under OUT), should a round's lab
# be stopped before it stops its server.
servers() {
  local p
  for p in /proc/[0-9]*; do
    tr '\0' ' ' < "$p/cmdline" 2>/dev/null | grep -q -- "--config $D/runs/" || continue
    sed -n 's|^0::.*/\(kiln-lab-server-[0-9]*\.scope\)$|\1|p' "$p/cgroup" 2>/dev/null
  done | sort -u
}
cleanup() {
  local d
  for d in "$D"/runs/*/*/; do touch "$d/hogrunner.stop" "$d/foreign.stop" 2>/dev/null; done
  mine | xargs -r systemctl --user stop 2>/dev/null
  servers | xargs -r systemctl --user stop 2>/dev/null
}
trap cleanup EXIT
trap 'exit 130' INT TERM
# What this machine grants: the real-time class needs RLIMIT_RTPRIO room for priority 1 (or root).
rt_room=$(bash -c 'ulimit -Sr 1 2>/dev/null; ulimit -Sr')
rt_ok() { [ "$rt_room" = unlimited ] || { [ "$rt_room" -ge 1 ] 2>/dev/null; }; }
keep=""
for cfg in $CONFIGS; do
  case $cfg in
    rt | rt-cc) rt_ok || { say "$cfg: skipped: this machine gives a user's process no real-time room (RLIMIT_RTPRIO hard $(ulimit -Hr)); the owner's call (docs/priority.md)"; continue; } ;;
  esac
  case $cfg in
    *-cc) [ -s "$CC_SRC" ] || { say "$cfg: skipped: no C source for its compilers (CC_SRC)"; continue; } ;;
  esac
  keep="$keep $cfg"
done
CONFIGS=$keep
one() {
  local cfg=$1 i=$2 out=$D/runs/$1/$2 unit=kiln-lab-s7c-$1-$2-$$ server_cpus=$SERVER_CPUS hog="" hogcpus="" senv=()
  if [ -s "$out/run.json" ] && grep -q '"numbers"' "$out/run.json"; then say "round $i $cfg: measured before"; return; fi
  rm -rf "$out"; mkdir -p "$out"
  case "$cfg" in
    hogs) hog=spin; hogcpus=$SERVER_CPUS ;;
    hogs-cc) hog=cc; hogcpus=$SERVER_CPUS ;;
    cores-calm) server_cpus=$CORES; senv+=(--env "LAB_SERVER_CORES=$CORES") ;;
    cores) server_cpus=$CORES; senv+=(--env "LAB_SERVER_CORES=$CORES"); hog=spin; hogcpus=$CORES_HOGS ;;
    cores-cc) server_cpus=$CORES; senv+=(--env "LAB_SERVER_CORES=$CORES"); hog=cc; hogcpus=$CORES_HOGS ;;
    fence) server_cpus=$CORES; senv+=(--env "LAB_SERVER_CORES=$CORES") ;;
    rt) senv+=(--env KILN_RT_REALTIME=1 --env LAB_SERVER_RTPRIO=1); hog=spin; hogcpus=$SERVER_CPUS ;;
    rt-cc) senv+=(--env KILN_RT_REALTIME=1 --env LAB_SERVER_RTPRIO=1); hog=cc; hogcpus=$SERVER_CPUS ;;
    nudge) senv+=(--env KILN_RT_NUDGE=1); hog=spin; hogcpus=$SERVER_CPUS ;;
    nudge-cc) senv+=(--env KILN_RT_NUDGE=1); hog=cc; hogcpus=$SERVER_CPUS ;;
    calm-nudge) senv+=(--env KILN_RT_NUDGE=1) ;;
  esac
  case "$cfg" in
    nudge | nudge-cc | calm-nudge) [ -n "${NUDGE_US:-}" ] && senv+=(--env "KILN_RT_NUDGE_US=$NUDGE_US") ;;
  esac
  senv+=(--env "LAB_SERVER_CPUS=$server_cpus")
  [ -n "$SHARDS" ] && senv+=(--env "KILN_RT_SHARDS=$SHARDS")
  if [ -n "$hog" ]; then
    local each="sh -c 'while :; do :; done'" props=()
    if [ "$hog" = cc ]; then
      each="sh -c 'while :; do cc -O2 -c \"$CC_SRC\" -o /dev/null; done'"
      props=(-p MemoryMax=16G)
    fi
    systemd-run --user --scope --quiet --collect --unit="$unit-hogs" -p CPUWeight=1 "${props[@]}" \
      taskset -c "$hogcpus" bash -c "for _ in \$(seq $N); do $each & done; wait" > "$out/hogs.log" 2>&1 &
    sleep 3
  fi
  if [ "$cfg" = fence ]; then
    bash "$D/bin/hogrunner.sh" "$D/bin/kiln" "$out" "$CORES" "$SERVER_CPUS" "$N" "$HOG_SECS" "$unit-runner" > "$out/hogrunner.log" 2>&1 &
  fi
  python3 -I "$D/bin/foreign.py" sample "$server_cpus" "$out" "$out/foreign.jsonl" "$out/foreign.stop" &
  local sampler=$!
  say "round $i $cfg: load $(cut -d' ' -f1-3 /proc/loadavg); $(awk '/^MemAvailable:/ {print int($2/1048576)}' /proc/meminfo) GB available"
  # A service's output goes to the journal, not to systemd-run's: the unit writes lab.log itself.
  systemd-run --user --quiet --collect --wait --unit="$unit" -p MemoryMax=16G --working-directory="$D" "${pass[@]}" \
    -p StandardOutput="append:$out/lab.log" -p StandardError="append:$out/lab.log" \
    taskset -c "$LAB_CPUS" "$D/bin/kiln-lab" run --kiln "$D/bin/kiln-server" --label "$cfg" --profile "$PROFILE" --rounds 1 "${senv[@]}" --out "$out" >> "$out/lab.log" 2>&1
  local rc=$?
  touch "$out/foreign.stop" "$out/hogrunner.stop"
  wait "$sampler" 2>/dev/null
  python3 -I "$D/bin/foreign.py" window "$out/foreign.jsonl" > "$out/foreign.json" 2>/dev/null
  say "round $i $cfg: lab exited $rc; work not the server's on its CPUs: $(head -c 200 "$out/foreign.json" 2>/dev/null)"
  [ -n "$hog" ] && systemctl --user stop "$unit-hogs.scope" 2>/dev/null
  if [ "$cfg" = fence ]; then
    sleep 3
    systemctl --user stop "$unit-runner.scope" 2>/dev/null
    say "round $i fence: $(tail -1 "$out/fence.txt" 2>/dev/null)"
  fi
  if [ -s "$out/healthz.json" ]; then
    say "round $i $cfg: the server said $(python3 -I -c 'import json,sys; p=json.load(open(sys.argv[1])).get("priority") or {}; r=p.get("realtime") or {}; n=p.get("nudge") or {}; print("realtime:", r.get("took"), "of", r.get("shards"), "took it", r.get("why") or "", "| nudge:", n.get("nudged"), "of", n.get("shards"), "every", n.get("period_us"), "us", n.get("why") or "", "| work:", p.get("work"))' "$out/healthz.json" 2>/dev/null)"
  fi
  wait 2>/dev/null
}
say "start: $ROUNDS rounds of$CONFIGS ($PROFILE), $N hogs; the lab on CPUs $LAB_CPUS, the server on $SERVER_CPUS (its own cores: $CORES, their hogs on $CORES_HOGS) with ${SHARDS:-its own count of} shards; real-time room $rt_room; $(uptime)"
for i in $(seq "$ROUNDS"); do
  set -- $CONFIGS
  # Each round starts the turn one further on.
  for _ in $(seq $(((i - 1) % $#))); do set -- "${@:2}" "$1"; done
  for cfg in "$@"; do one "$cfg" "$i"; done
done
python3 -I "$here/merge.py" "$D/runs" $CONFIGS >> "$D/runs/driver.log" 2>&1
# The report: every configuration's numbers side by side, and the comparisons the steps' done-whens read.
{
  echo "# Step 7's lab: $(date -u +%Y-%m-%dT%H:%MZ), $ROUNDS rounds of$CONFIGS"
  echo
  tab=()
  for c in $CONFIGS; do [ -s "$D/runs/$c/run.json" ] && tab+=("$c=$D/runs/$c/run.json"); done
  "$D/bin/kiln-lab" table "${tab[@]}"
  for pair in "calm hogs" "calm hogs-cc" "calm cores" "calm cores-cc" "cores-calm cores" "cores fence" "calm rt" "calm rt-cc" "hogs cores" "hogs rt" "hogs-cc cores-cc" "hogs-cc rt-cc" "calm nudge" "calm nudge-cc" "calm calm-nudge" "hogs nudge" "hogs-cc nudge-cc"; do
    set -- $pair
    [ -s "$D/runs/$1/run.json" ] && [ -s "$D/runs/$2/run.json" ] || continue
    echo
    echo "## $1 against $2"
    "$D/bin/kiln-lab" compare "$D/runs/$1/run.json" "$D/runs/$2/run.json"
  done
} > "$D/report.md" 2>&1
say "done; report in $D/report.md; $(uptime)"
echo DONE > "$D/runs/DONE"
