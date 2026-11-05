#!/bin/bash
# The lab's second pass (2026-11-05): WebKit's pilot and matrix rows (in Playwright's image,
# KILN_LAB_PODMAN), and kiln today's request cells again in Chromium, whose write now carries an id
# of each link's own (they supersede the first pass's write cells). A unit of its own, from a
# snapshot of the lab (lab-snap2) that nothing edits while it runs; it starts once the first pass's
# unit has ended, so the two never share the machine. Each part writes into a folder of its own and
# leaves `done` there when it ends; started again, it skips what is done.
#   systemd-run --user --unit=kiln-bp-g1-lab2 -p Delegate=yes bash $G/lab-snap2/run-pass2.sh
set -u
G=$HOME/.cache/kiln-bp/g1
S=$G/lab-snap2
L=$S/kiln-lab
T=$G/tools/rpm
export KILN_LAB_TC=$T/usr/bin/tc TC_LIB_DIR=$T/usr/lib64/tc KILN_LAB_WEB=$S/web
export KILN_LAB_PODMAN=mcr.microsoft.com/playwright:v1.64.1-noble
K=$G/target-kiln/release/kiln
H2O=$G/tools/h2o/build/h2o
CADDY=$G/tools/caddy/caddy
P=$G/catalogue2/payloads
command -v node >/dev/null || { echo "no node on the PATH: start the unit with -E PATH=..."; exit 1; }
# The first pass's unit first: wait for it to end (it ends on its own when its matrix is done).
while systemctl --user is-active -q kiln-bp-g1-lab; do sleep 600; done
echo "$(date -u +%FT%TZ) the first pass's unit has ended: the second pass starts"
COMMON="--seed 7 --kiln $K --h2o $H2O --caddy $CADDY --payloads $P"
part() { # name, then run's arguments
  local name=$1; shift
  local out=$G/lab2/$name
  if [ -f $out/done ]; then echo "$(date -u +%FT%TZ) $name: done before"; return; fi
  mkdir -p $out
  echo "$(date -u +%FT%TZ) $name: starting"
  $L browser run "$@" $COMMON --out $out > $out.log 2>&1
  local code=$?
  if [ $code = 0 ]; then echo "exit 0" > $out/done; else echo "exit $code" > $out/failed; fi
  echo "$(date -u +%FT%TZ) $name: exit $code"
  return $code
}
# WebKit's pilot (step 7): the reduced matrix and the A/A, HTTP/2 rows only (WebKit on Linux has
# no HTTP/3: f30).
part pilot-good-webkit --browsers webkit --targets kiln-today,kiln-today-b,h2o-h2 --profiles good --losses 0 --rounds 5 --k 8
part pilot-50k-webkit --browsers webkit --targets kiln-today,kiln-today-b,h2o-h2 --profiles 50k --losses 1 --rounds 5 --k 8
# WebKit's matrix rows (step 8) and kiln today's request cells again in Chromium, the same rounds
# the first pass's matrix gave each condition and case.
TARGETS=kiln-today,h2o-h2,caddy-h2
for loss in 0 1 5; do
  part matrix-good-$loss-webkit --browsers webkit --targets $TARGETS --profiles good --losses $loss --rounds 5 --k 8
  part write-good-$loss-chromium --browsers chromium --targets kiln-today --profiles good --losses $loss --cases request --rounds 5 --k 8
done
for prof in 100k 50k; do
  for loss in 0 1 5; do
    part matrix-$prof-$loss-stream-webkit --browsers webkit --targets $TARGETS --profiles $prof --losses $loss --cases stream --rounds 5 --k 8
    if [ $prof = 50k ]; then R=7; else R=5; fi
    part matrix-$prof-$loss-burst-webkit --browsers webkit --targets $TARGETS --profiles $prof --losses $loss --cases burst --rounds $R --k 8
    if [ $prof = 50k ]; then R=28; else R=14; fi
    part matrix-$prof-$loss-request-webkit --browsers webkit --targets $TARGETS --profiles $prof --losses $loss --cases request --rounds $R --k 8
    part write-$prof-$loss-chromium --browsers chromium --targets kiln-today --profiles $prof --losses $loss --cases request --rounds $R --k 8
  done
done
echo "$(date -u +%FT%TZ) all parts done"
