#!/bin/bash
# The checks that print what they find and keep no run file, run again for the repository's record
# (plan 01a1a832e9cc step 10: every number in docs/browser-lab.md traces to a record): the probe,
# the origin's frames on both carriers, the reference servers' starts and stops, how each server
# compresses a stream, the two recordings compared, and the snapshot's size. On the lab machine:
#   records.sh SNAP OUT     (SNAP: a lab snapshot, its kiln-lab and web; OUT: the records' folder)
set -u
S=$1; R=$2; G=$HOME/.cache/kiln-bp/g1; mkdir -p $R
P=$G/catalogue2/payloads; C=$G/catalogue2/catalogue.json
args=(--kiln $G/lab-snap1/kiln --h2o $G/tools/h2o/build/h2o --caddy $G/tools/caddy/caddy --payloads $P --seed 7)
rec() { local name=$1; shift; { echo "\$ $*"; "$@"; echo "exit $?"; } > $R/$name.txt 2>&1; echo "$name: $(tail -1 $R/$name.txt)"; }
rec probe $S/kiln-lab browser probe --web $S/web
rec check-origin bash -c "cd $S/web && node check-origin.mjs $P $C"
rec start-stop $S/kiln-lab browser start-stop --times 20 "${args[@]}" --web $S/web --targets h2o-h2,caddy-h2,h2o-h3,caddy-h3 --browsers chromium --out $G/lab/records-start-stop
rec compress-probe bash $(dirname "$0")/compress-probe.sh
rec recordings bash -c "cd $S/web && node catalogue.mjs compare $G/record-b/recordings $G/record-c/recordings"
rec snapshot ls -l $P/docroot/api/snapshots/lab
