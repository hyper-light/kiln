#!/bin/bash
# The BOLT pipeline and its A/B (goal 11, plan 01a1a87b2e56 step 5; Ada's goal 117): kiln's release laid out by a
# profile of the lab's realistic load, checked to be the same program, then measured against the release as it is.
# BOLT lays out together the code the server runs (ci/release/bolt.sh): fewer of the binary's pages touched, and fewer
# I-cache and iTLB misses on the shards, a latency and throughput gain before a memory one (Ada's rule: latency and
# throughput come first).
#
#   bolt-lab.sh tools                       where perf and llvm-bolt are on this machine, their versions, the CPU, and
#                                           whether perf records branch stacks here (nothing fetched or installed)
#   bolt-lab.sh record <kiln> <kiln-lab> <dir> <rounds>
#                                           <rounds> rounds of the realistic profile against <kiln> (the release: the
#                                           relocations build stripped), perf recording its server from its start to
#                                           its window's end (bolt-hook.sh, BOLT_RECORD=auto: LBR where this CPU has
#                                           it, else cycles:u samples with call graphs; each round's perf.log says
#                                           which) as often as the kernel lets it (BOLT_FREQ=max, below), into
#                                           <dir>/prof-<n>, the next n each time
#   bolt-lab.sh bolt <kiln.relocs> <dir>    every recording in <dir> (any machine's, matched by build id) to one
#                                           profile, <dir>/kiln.fdata, and the build laid out by it, <dir>/kiln.bolt
#                                           (bolt.sh optimize: BOLT's documented options); <dir>/kiln.yaml, the same
#                                           profile for later commits' builds; and the rounds BOLT asks for, derived
#                                           from what these rounds gave it (<dir>/rounds.txt, below)
#   bolt-lab.sh profile <kiln.relocs> <kiln> <kiln-lab> <dir>
#                                           record and bolt on one machine with both perf and BOLT: one round, then as
#                                           many more as BOLT asks for, then bolt again
#   bolt-lab.sh smoke <kiln> <kiln.bolt> <kiln-lab> <dir>
#                                           the same program: bolt.sh same (one build, one `--version`, the code
#                                           rewritten), then a round of the lab's small profile against each in turn
#                                           (kiln-lab ab), the BOLTed build with every round done and no more errors
#   bolt-lab.sh ab <kiln> <kiln.bolt> <kiln-lab> <dir>
#                                           the A/B: A the release, B it BOLTed, ROUNDS rounds each, calm, then with as
#                                           many hogs as the server's CPUs (<dir>/calm, <dir>/hogs), every round's
#                                           binary pages by mapping and section (bolt-hook.sh: pages.txt,
#                                           sections.txt), then <dir>/read.md (bolt-read.py ab)
#
# The placement, from the environment (the lab's own on fedora by default, as goal 9's lab runs: the lab's clients on
# LAB_CPUS, the server and its hogs on SERVER_CPUS, production's layout, the server's shards what it sizes itself to on
# those 16 CPUs):
#   LAB_CPUS (0-15), SERVER_CPUS (16-31), LAYOUT (production), SHARDS (4), ROUNDS (8, each side's rounds in the A/B),
#   HOGS (1x: as many as the server's CPUs), MEMORY_MAX (16G, the lab unit's cap); empty LAB_CPUS or SERVER_CPUS pin
#   nothing. UNIT_WEIGHT gives the lab's unit that CPUWeight and IOWeight (1 beside production: LAYOUT=shared then, so
#   the server isn't put at production's weight). UNIT=none runs kiln-lab in this process, no unit (the tests).
#   PERF, PERF_LIBS, BOLT_EVENT, KILN_BOLT_BIN, KILN_BOLT_ARGS as bolt.sh takes them. BOLT_FREQ, perf's frequency for
#   the profile's rounds: max by default, perf's "the currently maximum allowed frequency, i.e. the value in the
#   kernel.perf_event_max_sample_rate sysctl", which the kernel lowers itself while sampling takes more of the CPU than
#   kernel.perf_cpu_time_max_percent allows (a rate the kernel learns, machine-wide, not ours; perf's own default,
#   4000 a second, took 72 times too few samples by BOLT's own estimate on adacachy, 2026-11-06).
# Linux with systemd's user manager.
set -u
here=$(cd "$(dirname "$0")" && pwd)
bolt=${BOLT_SH:-$here/../../ci/release/bolt.sh}
LAB_CPUS=${LAB_CPUS-0-15} SERVER_CPUS=${SERVER_CPUS-16-31} LAYOUT=${LAYOUT:-production} SHARDS=${SHARDS-4}
ROUNDS=${ROUNDS:-8} HOGS=${HOGS:-1x} MEMORY_MAX=${MEMORY_MAX:-16G} UNIT_WEIGHT=${UNIT_WEIGHT:-} UNIT=${UNIT:-systemd}
export TMPDIR=${TMPDIR:-$HOME/.cache/kiln-lab/tmp}
mkdir -p "$TMPDIR"

die() {
  echo "bolt-lab: $*" >&2
  exit 1
}
say() { echo "$(date -u +%H:%M:%S) $*" >&2; }

build_id() { readelf -n "$1" 2> /dev/null | awk '$1 == "Build" && $2 == "ID:" { print $3; exit }'; }

# kiln-lab <name> <out> <record> <kiln-lab args...>: kiln-lab in a unit of its own, so it outlives whoever started it
# and its cap holds all it starts, with bolt-hook.sh as every round's hook (BOLT_RECORD=<record>); its output in
# <out>/lab.log.
lab() {
  local name=$1 out=$2 record=$3 v
  shift 3
  mkdir -p "$out"
  local env=(HOME="$HOME" PATH="$PATH" TMPDIR="$TMPDIR" KILN_LAB_HOOK="$here/bolt-hook.sh" BOLT_SH="$bolt" BOLT_RECORD="$record")
  for v in XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS PERF PERF_LIBS BOLT_EVENT BOLT_FREQ BOLT_FROM; do
    if [ -n "${!v:-}" ]; then env+=("$v=${!v}"); fi
  done
  local pin=()
  if [ -n "$LAB_CPUS" ]; then pin=(taskset -c "$LAB_CPUS"); fi
  say "kiln-lab $* (hook's record: $record) -> $out/lab.log"
  if [ "$UNIT" = none ]; then
    env "${env[@]}" ${pin[@]+"${pin[@]}"} "$klab" "$@" > "$out/lab.log" 2>&1
    return
  fi
  local props=(-p MemoryMax="$MEMORY_MAX") pass=() e
  if [ -n "$UNIT_WEIGHT" ]; then props+=(-p CPUWeight="$UNIT_WEIGHT" -p IOWeight="$UNIT_WEIGHT"); fi
  for e in "${env[@]}"; do pass+=(-E "$e"); done
  systemd-run --user --quiet --collect --wait --unit="kiln-bolt-lab-$name-$$" "${props[@]}" --working-directory="$out" "${pass[@]}" \
    ${pin[@]+"${pin[@]}"} "$klab" "$@" > "$out/lab.log" 2>&1
}

# The kiln-lab options every stage's rounds share: production's layout on the lab's CPUs, the server's shards.
placed() {
  local o=(--layout "$LAYOUT")
  if [ -n "$SERVER_CPUS" ]; then o+=(--server-cpus "$SERVER_CPUS"); fi
  printf '%s\n' "${o[@]}"
}

tools() {
  local perf
  echo "host: $(hostname) ($(uname -sr), $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo))"
  echo "glibc: $(ldd --version 2> /dev/null | awk 'NR == 1 { print $NF }')"
  if perf=$(command -v "${PERF:-perf}"); then
    echo "perf: $perf ($(LD_LIBRARY_PATH=${PERF_LIBS:-${LD_LIBRARY_PATH:-}} "$perf" --version 2>&1 | head -n 1))"
  else
    echo "perf: none (PERF names one)"
  fi
  echo "perf_event_paranoid: $(cat /proc/sys/kernel/perf_event_paranoid 2> /dev/null)"
  "$bolt" lbr 2> /dev/null || true
  local d=${KILN_BOLT_BIN:-}
  if [ -z "$d" ] && command -v llvm-bolt > /dev/null; then d=$(dirname "$(command -v llvm-bolt)"); fi
  if [ -n "$d" ] && [ -x "$d/llvm-bolt" ]; then
    echo "llvm-bolt: $d ($("$d/llvm-bolt" --version 2>&1 | awk '/LLVM version/ { print $3; exit }'); perf2bolt $([ -x "$d/perf2bolt" ] && echo yes || echo no), merge-fdata $([ -x "$d/merge-fdata" ] && echo yes || echo no))"
  else
    echo "llvm-bolt: none (KILN_BOLT_BIN names one; bolt.sh tools would fetch LLVM's release, which this doesn't)"
  fi
}

record() {
  local kiln=$1 rounds=$3 dir=$4 n=1 out BOLT_FREQ=${BOLT_FREQ:-max}
  [ -x "$kiln" ] || die "$kiln: no build to profile"
  case $rounds in '' | *[!0-9]* | 0) die "record takes a count of rounds, not '$rounds'" ;; esac
  while [ -e "$dir/prof-$n" ]; do n=$((n + 1)); done
  out=$dir/prof-$n
  local o
  mapfile -t o < <(placed)
  lab "prof-$n" "$out" "${BOLT_RECORD:-auto}" run --kiln "$kiln" --label release ${SHARDS:+--env KILN_RT_SHARDS=$SHARDS} \
    --profile realistic --rounds "$rounds" --hogs 0 "${o[@]}" --out "$out/run" || say "kiln-lab ended $? ($out/lab.log)"
  local recorded
  recorded=$(find "$out/run/rounds" -name perf.data -path '*.hook/*' -size +0 2> /dev/null | wc -l)
  [ "$recorded" -gt 0 ] || die "no round recorded a profile ($out/lab.log, $out/run/rounds/*.hook/perf.log)"
  # What each round's perf took, LBR or samples, as bolt.sh record said it.
  grep -h '^bolt.sh: recording' "$out"/run/rounds/*.hook/perf.log 2> /dev/null | sed 's/process [0-9]*/the server/' | sort | uniq -c >&2
  say "$out: $recorded round(s) recorded"
}

# The rounds a profile takes, from what the $2 rounds recorded so far gave BOLT, the rounds being alike, by BOLT's two
# measures, none of ours: its README's "We recommend the profile to cover 1B instructions as reported by BOLT
# -dyno-stats option" ($1, the instructions -dyno-stats counted), and perf2bolt's own estimate from each recording's
# samples against the code they fell in, "BOLT is estimated to optimize better with <N>x more samples" ($3, the largest
# N of a round's recording; 0 where perf2bolt said none): `readme=<rounds> bolt=<rounds> needed=<the more>`.
needed() {
  awk -v have="$1" -v rounds="$2" -v more="$3" 'BEGIN {
    if (have <= 0 || rounds <= 0) exit 1
    per = have / rounds; r = int(1e9 / per); if (r * per < 1e9) r++
    b = int(more); if (b < more) b++
    n = rounds; if (r > n) n = r; if (b > n) n = b
    printf "readme=%d bolt=%d needed=%d\n", r, b, n
  }'
}

bolt_stage() {
  local relocs=$1 dir=$2 data rounds ins need more said
  "$bolt" check "$relocs" > /dev/null || exit 1
  mapfile -t data < <(find "$dir" -name perf.data -path '*/prof-*/run/rounds/*.hook/*' -size +0 2> /dev/null | sort)
  [ ${#data[@]} -gt 0 ] || die "$dir: no recordings (bolt-lab.sh record)"
  rounds=${#data[@]}
  # The layout's profile, and the same as YAML for later commits' builds, from one pass of perf2bolt.
  BOLT_PROFILE_YAML=$dir/kiln.yaml "$bolt" profile "$relocs" "$dir/kiln.fdata" "${data[@]}" || exit 1
  "$bolt" optimize "$relocs" "$dir/kiln.fdata" "$dir/kiln.bolt" || exit 1
  cp -f "$relocs.bolt.txt" "$dir/kiln.bolt.txt"
  ins=$(awk '$1 == "instructions:" { print $2; exit }' "$dir/kiln.bolt.txt")
  # perf2bolt's estimate for each recording (bolt.sh profile keeps its log beside the profile, one a recording).
  more=$(cat "$dir"/kiln.fdata.*.log 2> /dev/null | sed -n 's/.*estimated to optimize better with \([0-9.]*\)x more samples.*/\1/p' | sort -g | tail -n 1)
  if said=$(needed "${ins:-0}" "$rounds" "${more:-0}"); then
    echo "rounds=$rounds instructions=$ins more=${more:-0} $said" > "$dir/rounds.txt"
    need=${said##*needed=}
    if [ "$need" -gt "$rounds" ]; then
      say "the profile: $ins instructions in $rounds round(s), perf2bolt asking ${more:-no}x more samples ($said): bolt-lab.sh record ... $((need - rounds)), then bolt again"
    else
      say "the profile: $ins instructions in $rounds round(s), perf2bolt asking for no more: enough by both of BOLT's measures"
    fi
  else
    echo "rounds=$rounds instructions=unknown" > "$dir/rounds.txt"
    say "llvm-bolt's log says no instructions covered ($relocs.bolt.log)"
  fi
}

profile() {
  local relocs=$1 kiln=$2 dir=$4 need have
  [ "$(build_id "$relocs")" = "$(build_id "$kiln")" ] && [ -n "$(build_id "$kiln")" ] || die "$kiln isn't $relocs stripped (build ids differ or none)"
  record "$kiln" "$klab" 1 "$dir"
  bolt_stage "$relocs" "$dir"
  need=$(awk -F'needed=' 'NF > 1 { print $2 + 0 }' "$dir/rounds.txt")
  have=$(awk -F'rounds=' 'NF > 1 { print $2 + 0 }' "$dir/rounds.txt")
  if [ -n "$need" ] && [ "$need" -gt "$have" ]; then
    record "$kiln" "$klab" $((need - have)) "$dir"
    bolt_stage "$relocs" "$dir"
  fi
  echo "$dir/kiln.bolt: $(cat "$dir/rounds.txt")"
}

smoke() {
  local a=$1 b=$2 dir=$4 o
  "$bolt" same "$a" "$b" || exit 1
  mapfile -t o < <(placed)
  # The small profile: a minute's try of every class the lab times (docs/runtime-lab.md), one round each, A then B.
  lab smoke "$dir/smoke" none ab --a "$a" --b "$b" --label-a release --label-b bolt ${SHARDS:+--env-a KILN_RT_SHARDS=$SHARDS --env-b KILN_RT_SHARDS=$SHARDS} \
    --profile small --rounds 1 --hogs 0 "${o[@]}" --out "$dir/smoke/run" || say "kiln-lab ended $? ($dir/smoke/lab.log)"
  python3 -I "$here/bolt-read.py" smoke "$dir/smoke/run" release bolt
}

ab() {
  local a=$1 b=$2 dir=$4 side hogs o worst=0 rc
  "$bolt" same "$a" "$b" || exit 1
  mapfile -t o < <(placed)
  for side in calm hogs; do
    hogs=0
    if [ "$side" = hogs ]; then hogs=$HOGS; fi
    lab "ab-$side" "$dir/$side" none ab --a "$a" --b "$b" --label-a release --label-b bolt \
      ${SHARDS:+--env-a KILN_RT_SHARDS=$SHARDS --env-b KILN_RT_SHARDS=$SHARDS} --profile realistic --rounds "$ROUNDS" --hogs "$hogs" \
      "${o[@]}" --out "$dir/$side/run"
    rc=$?
    say "ab $side: kiln-lab exit $rc; $dir/$side/run/report.md"
    if [ "$rc" != 0 ]; then worst=$rc; fi
  done
  python3 -I "$here/bolt-read.py" ab release bolt "$dir/calm/run" "$dir/hogs/run" > "$dir/read.md" || worst=1
  say "ab: $dir/read.md, $dir/{calm,hogs}/run/report.md"
  return "$worst"
}

# Paths as absolute ones (the lab's unit runs in its own folder), each there.
file() {
  readlink -e "$1" 2> /dev/null && return
  echo "bolt-lab: $1: no such file" >&2
  return 1
}
folder() { mkdir -p "$1" && realpath "$1"; }

cmd=${1:-}
[ $# -gt 0 ] && shift
case $cmd in
  tools) tools ;;
  record)
    [ $# -eq 4 ] || die "record takes <kiln> <kiln-lab> <dir> <rounds>"
    kiln=$(file "$1") && klab=$(file "$2") && dir=$(folder "$3") || exit 1
    record "$kiln" "$klab" "$4" "$dir"
    ;;
  bolt)
    [ $# -eq 2 ] || die "bolt takes <kiln.relocs> <dir>"
    relocs=$(file "$1") && dir=$(folder "$2") || exit 1
    bolt_stage "$relocs" "$dir"
    ;;
  profile)
    [ $# -eq 4 ] || die "profile takes <kiln.relocs> <kiln> <kiln-lab> <dir>"
    relocs=$(file "$1") && kiln=$(file "$2") && klab=$(file "$3") && dir=$(folder "$4") || exit 1
    profile "$relocs" "$kiln" "$klab" "$dir"
    ;;
  smoke | ab)
    [ $# -eq 4 ] || die "$cmd takes <kiln> <kiln.bolt> <kiln-lab> <dir>"
    kiln=$(file "$1") && bolted=$(file "$2") && klab=$(file "$3") && dir=$(folder "$4") || exit 1
    "$cmd" "$kiln" "$bolted" "$klab" "$dir"
    ;;
  *) sed -n '2,/^set -u/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
