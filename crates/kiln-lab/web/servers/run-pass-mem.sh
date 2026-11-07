#!/bin/bash
# The rest of a pass on a shared, tight machine (plan 01a1a832e9cc step 8): the parts in PARTS,
# at most WORKERS at a time, each in a systemd unit of its own (`kiln-bp-g1-p-<part>`, so the memory
# guard, `guard-mem.sh`, can freeze one part and no other). No part starts while less than
# KILN_LAB_MIN_AVAIL_GB (10) is available, nor within KILN_LAB_START_GAP_S (180 s) of the last
# start (a part takes a minute or two to reach its size). A part is claimed atomically
# (`<part>.mem`); one done, started (its folder there) or claimed is passed by. It ignores
# `<part>.skip`: the parts it takes from running run-pass.sh units are marked so they pass them by.
#   run-pass-mem.sh PARTS WORKERS     (in a unit of its own; `$H3` and `after=` as in run-pass.sh)
set -u
PARTS=$1; WORKERS=$2
G=$HOME/.cache/kiln-bp/g1
S=${KILN_LAB_SNAP:-$G/lab-snap1}
OUT=${KILN_LAB_OUT:-$G/lab}
export S OUT G
export MIN_GB=${KILN_LAB_MIN_AVAIL_GB:-10} GAP=${KILN_LAB_START_GAP_S:-180}
export COMMON="--seed 7 --kiln $S/kiln --h2o $G/tools/h2o/build/h2o --caddy $G/tools/caddy/caddy --payloads $G/catalogue2/payloads"
h3() {
  python3 - $OUT/pilot-50k-*/run.json <<'PY'
import json, statistics, sys
jobs = [j for f in sys.argv[1:] for j in json.load(open(f))["jobs"]]
def p99(t):
    v = [j["numbers"].get("time.p99_ms") for j in jobs if j["target"] == t and j["numbers"].get("time.p99_ms") is not None]
    return statistics.median(v) if v else float("inf")
print("h2o-h3-reno" if p99("h2o-h3-reno") < p99("h2o-h3") else "h2o-h3")
PY
}
avail_gb() { awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo; }
part() {
  set -- $1
  local name=$1; shift
  local out=$OUT/$name
  if [ -f $out/done ] || [ -e $out ] || ! mkdir $OUT/$name.mem 2>/dev/null; then return 0; fi
  if [[ ${1:-} == after=* ]]; then
    local deps=${1#after=}; shift
    for dep in ${deps//,/ }; do until [ -f $OUT/$dep/done ]; do sleep 60; done; done
  fi
  local args="$*"
  if [[ $args == *'$H3'* ]]; then local c; c=$(h3); args=${args//'$H3'/$c}; echo "$(date -u +%FT%TZ) $name: h2o's QUIC controller $c"; fi
  # One start at a time: memory first, then the gap since the last start.
  exec 9>$OUT/.start.lock
  flock 9
  local said=0
  while [ $(avail_gb) -lt $MIN_GB ]; do
    [ $said = 0 ] && echo "$(date -u +%FT%TZ) $name: waits for $MIN_GB GB available ($(avail_gb) now)" && said=1
    sleep 60
  done
  local last; last=$(cat $OUT/.last-start 2>/dev/null || echo 0)
  local wait=$(( last + GAP - $(date +%s) )); [ $wait -gt 0 ] && sleep $wait
  if [ -e $out ]; then flock -u 9; return 0; fi
  mkdir -p $out; date +%s > $OUT/.last-start
  flock -u 9; exec 9>&-
  echo "$(date -u +%FT%TZ) $name: starting, in unit kiln-bp-g1-p-$name, $(avail_gb) GB available ($args)"
  systemd-run --user --wait --quiet --collect --unit=kiln-bp-g1-p-$name -E PATH=$PATH -E HOME=$HOME -E KILN_LAB_WEB=$S/web \
    -E KILN_LAB_TC=${KILN_LAB_TC:-} -E TC_LIB_DIR=${TC_LIB_DIR:-} -E KILN_LAB_PODMAN=${KILN_LAB_PODMAN:-} \
    -p Delegate=yes -p Nice=19 -p CPUWeight=10 -p IOWeight=10 -p MemoryMax=15032385536 \
    -p StandardOutput=file:$out.log -p StandardError=file:$out.log \
    $S/kiln-lab browser run $args $COMMON --out $out
  local code=$?
  if [ $code = 0 ]; then echo "exit 0" > $out/done; else echo "exit $code" > $out/failed; fi
  echo "$(date -u +%FT%TZ) $name: exit $code"
}
export -f part h3 avail_gb
mkdir -p $OUT
# The parts that wait on others last (their workers would only wait).
{ grep -v '^#' $PARTS | grep . | grep -v ' after='; grep -v '^#' $PARTS | grep ' after='; } | xargs -P $WORKERS -I{} bash -c 'part "$@"' _ {}
echo "$(date -u +%FT%TZ) every part of $PARTS has run"
