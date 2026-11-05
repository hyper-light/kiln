#!/bin/bash
# The pilot (step 7) and then the matrix (step 8), in both browsers (WebKit in Playwright's image:
# KILN_LAB_PODMAN), in a unit of its own, from a snapshot of the lab that nothing edits while it
# runs. Each part writes into a folder of its own and leaves `done` there when it ends; started
# again, it skips what is done (so it resumes where it stood).
#   systemd-run --user --unit=kiln-bp-g1-lab -p Delegate=yes bash $G/lab-snap1/run-all.sh
set -u
G=$HOME/.cache/kiln-bp/g1
S=$G/lab-snap1
L=$S/kiln-lab
T=$G/tools/rpm
export KILN_LAB_TC=$T/usr/bin/tc TC_LIB_DIR=$T/usr/lib64/tc KILN_LAB_WEB=$S/web
export KILN_LAB_PODMAN=mcr.microsoft.com/playwright:v1.64.1-noble
K=$G/target-kiln/release/kiln
H2O=$G/tools/h2o/build/h2o
CADDY=$G/tools/caddy/caddy
P=$G/catalogue2/payloads
command -v node >/dev/null || { echo "no node on the PATH: start the unit with -E PATH=..."; exit 1; }
COMMON="--seed 7 --browsers chromium,webkit --kiln $K --h2o $H2O --caddy $CADDY --payloads $P"
part() { # name, then run's arguments
  local name=$1; shift
  local out=$G/lab/$name
  if [ -f $out/done ]; then echo "$(date -u +%FT%TZ) $name: done before"; return; fi
  mkdir -p $out
  echo "$(date -u +%FT%TZ) $name: starting"
  $L browser run "$@" $COMMON --out $out > $out.log 2>&1
  local code=$?
  # Only a part that ended well is done; one that didn't runs again when this is started again.
  if [ $code = 0 ]; then echo "exit 0" > $out/done; else echo "exit $code" > $out/failed; fi
  echo "$(date -u +%FT%TZ) $name: exit $code"
  return $code
}
mkdir -p $G/lab
# Step 7: the pilot and its A/A, the QUIC controller's choice, and K links against one.
part pilot-good --targets kiln-today,kiln-today-b,h2o-h2,h2o-h3,h2o-h3-reno --profiles good --losses 0 --rounds 5 --k 8
part pilot-50k --targets kiln-today,kiln-today-b,h2o-h2,h2o-h3,h2o-h3-reno --profiles 50k --losses 1 --rounds 5 --k 8
part pilot-50k-k1 --targets kiln-today --profiles 50k --losses 1 --cases stream --rounds 5 --k 1
[ -f $G/lab/pilot-good/done ] && [ -f $G/lab/pilot-50k/done ] || { echo "$(date -u +%FT%TZ) the pilot isn't done: no matrix"; exit 1; }
# h2o's QUIC controller for the matrix: the lower median p99 at 50 kbit/s with 1% loss.
H3=$(python3 - $G/lab/pilot-50k/run.json <<'PY'
import json, statistics, sys
r = json.load(open(sys.argv[1]))
def p99(t):
    v = [j["numbers"].get("time.p99_ms") for j in r["jobs"] if j["target"] == t and j["numbers"].get("time.p99_ms") is not None]
    return statistics.median(v) if v else float("inf")
print("h2o-h3-reno" if p99("h2o-h3-reno") < p99("h2o-h3") else "h2o-h3")
PY
)
echo "$(date -u +%FT%TZ) h2o's QUIC controller: $H3"
TARGETS=kiln-today,h2o-h2,caddy-h2,$H3,caddy-h3
# Step 8: every condition and case, the rounds the run plan gives it at K = 8.
for loss in 0 1 5; do
  part matrix-good-$loss --targets $TARGETS --profiles good --losses $loss --rounds 5 --k 8
done
for prof in 100k 50k; do
  for loss in 0 1 5; do
    part matrix-$prof-$loss-stream --targets $TARGETS --profiles $prof --losses $loss --cases stream --rounds 5 --k 8
    if [ $prof = 50k ]; then R=7; else R=5; fi
    part matrix-$prof-$loss-burst --targets $TARGETS --profiles $prof --losses $loss --cases burst --rounds $R --k 8
    if [ $prof = 50k ]; then R=28; else R=14; fi
    part matrix-$prof-$loss-request --targets $TARGETS --profiles $prof --losses $loss --cases request --rounds $R --k 8
  done
done
echo "$(date -u +%FT%TZ) all parts done"
