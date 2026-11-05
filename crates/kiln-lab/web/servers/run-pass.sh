#!/bin/bash
# One machine's share of a pass (plan 01a1a832e9cc steps 7 and 8): the parts in PARTS, WORKERS at a
# time, each part a `kiln-lab browser run` in a lab of its own (its own namespaces, servers and
# browsers), from a snapshot of the lab ($S) that nothing edits while it runs. A part writes
# $OUT/<name>/run.json as its jobs end and `done` when it ends well (`failed` otherwise); started
# again, this skips what is done and runs the rest. A part whose line names `after=<part>` waits for
# those parts' `done` first (comma-separated); `$H3` in a line is h2o's QUIC controller as the pilot chose it.
#   run-pass.sh PARTS WORKERS     (in a systemd unit of its own: systemctl --user stop/freeze it)
set -u
PARTS=$1; WORKERS=$2
G=$HOME/.cache/kiln-bp/g1
S=${KILN_LAB_SNAP:-$G/lab-snap1}
OUT=${KILN_LAB_OUT:-$G/lab}
export L=$S/kiln-lab KILN_LAB_WEB=$S/web OUT G
export KILN_LAB_TC=${KILN_LAB_TC:-$(command -v tc)}
export COMMON="--seed 7 --kiln $S/kiln --h2o $G/tools/h2o/build/h2o --caddy $G/tools/caddy/caddy --payloads $G/catalogue2/payloads"
command -v node >/dev/null || { echo "no node on the PATH"; exit 1; }
mkdir -p $OUT
# h2o's QUIC controller: the lower median p99 of h2o-h3 (Cubic) and h2o-h3-reno at 50k/1 in the
# pilot (its request, stream and burst parts).
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
part() { # one line: name [after=part] run's arguments
  set -- $1
  local name=$1; shift
  local out=$OUT/$name
  if [ -f $out/done ]; then echo "$(date -u +%FT%TZ) $name: done before"; return 0; fi
  # A part another machine took (`<part>.skip` beside the parts' folders) isn't run here.
  if [ -f $OUT/$name.skip ]; then echo "$(date -u +%FT%TZ) $name: taken by another machine"; return 0; fi
  if [[ ${1:-} == after=* ]]; then
    local deps=${1#after=}; shift
    for dep in ${deps//,/ }; do until [ -f $OUT/$dep/done ]; do sleep 60; done; done
  fi
  local args="$*"
  if [[ $args == *'$H3'* ]]; then local c; c=$(h3); args=${args//'$H3'/$c}; echo "$(date -u +%FT%TZ) $name: h2o's QUIC controller $c"; fi
  mkdir -p $out
  echo "$(date -u +%FT%TZ) $name: starting ($args)"
  $L browser run $args $COMMON --out $out > $out.log 2>&1
  local code=$?
  if [ $code = 0 ]; then echo "exit 0" > $out/done; else echo "exit $code" > $out/failed; fi
  echo "$(date -u +%FT%TZ) $name: exit $code"
}
export -f part h3
grep -v '^#' $PARTS | grep . | xargs -P $WORKERS -I{} bash -c 'part "$@"' _ {}
echo "$(date -u +%FT%TZ) every part of $PARTS has run"
