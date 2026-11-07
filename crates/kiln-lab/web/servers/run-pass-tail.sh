#!/bin/bash
# More workers beside a running run-pass.sh, without restarting it: this takes the parts from the
# end of the same list, each one it takes marked `<part>.skip` first (run-pass.sh then passes it by),
# and one already started, done or taken is passed by here. A part that waits on another
# (`after=`) waits here the same way.
#   run-pass-tail.sh PARTS WORKERS
set -u
PARTS=$1; WORKERS=$2
G=$HOME/.cache/kiln-bp/g1
S=${KILN_LAB_SNAP:-$G/lab-snap1}
OUT=${KILN_LAB_OUT:-$G/lab}
export L=$S/kiln-lab KILN_LAB_WEB=$S/web OUT G
export KILN_LAB_TC=${KILN_LAB_TC:-$(command -v tc)}
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
take() {
  set -- $1
  local name=$1; shift
  local out=$OUT/$name
  # Taken atomically: the first to make `<part>.take` has it.
  if [ -f $out/done ] || [ -e $out ] || [ -f $OUT/$name.skip ] || ! mkdir $OUT/$name.take 2>/dev/null; then return 0; fi
  touch $OUT/$name.skip
  if [[ ${1:-} == after=* ]]; then
    local deps=${1#after=}; shift
    for dep in ${deps//,/ }; do until [ -f $OUT/$dep/done ]; do sleep 60; done; done
  fi
  local args="$*"
  if [[ $args == *'$H3'* ]]; then local c; c=$(h3); args=${args//'$H3'/$c}; echo "$(date -u +%FT%TZ) $name: h2o's QUIC controller $c"; fi
  mkdir -p $out
  echo "$(date -u +%FT%TZ) $name: starting, from the list's end ($args)"
  $L browser run $args $COMMON --out $out > $out.log 2>&1
  local code=$?
  if [ $code = 0 ]; then echo "exit 0" > $out/done; else echo "exit $code" > $out/failed; fi
  echo "$(date -u +%FT%TZ) $name: exit $code"
}
export -f take h3
mkdir -p $OUT
# From the end, the parts that wait on others last (their workers would only wait).
{ grep -v '^#' $PARTS | grep . | grep -v ' after=' | tac; grep -v '^#' $PARTS | grep ' after='; } | xargs -P $WORKERS -I{} bash -c 'take "$@"' _ {}
echo "$(date -u +%FT%TZ) the tail of $PARTS has run"
