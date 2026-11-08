#!/bin/bash
# docs/browser-lab.md's audit (plan 01a1a832e9cc step 10): every number in its prose against the
# records it rests on. This builds each section's sources from the repository's records and what
# kiln-lab prints from them, then audit.py looks for each number in them (derived.tsv: the few
# derived from one, with how).
#   sources.sh KILN_LAB OUT && python3 audit.py REPO OUT
set -u
B=$1; O=$2; R=$(git rev-parse --show-toplevel); D=$R/docs/browser-lab; P=$R/crates/kiln-lab/web/servers/pass1
mkdir -p $O; cd $R || exit 9
$B browser profiles > $O/profiles.txt
for k in 1 8 16; do $B browser plan --k $k > $O/plan-$k.txt; done
for f in $D/2026-11-04/check4.json $D/2026-11-05/{grain,check4b,check4c,check5c}.json; do
  n=$(basename $f .json); $B browser check-probe --catalogue $D/catalogue.json $f > $O/$n.check.txt 2>&1; $B browser report $f > $O/$n.report.txt 2>&1
done
X=$D/2026-11-05
$B browser report $X/pilot.json > $O/pilot.report.txt
for f in $X/k*.json $X/xm-50k-stream.json; do $B browser report $f; done > $O/kcheck.report.txt 2>&1
{ $B browser compare $X/pilot.json kiln-today kiln-today-b
  for p in "pilot k1-good-request" "pilot k1-50k-stream" "xm-50k-stream k1-50k-stream" "k4-50k-stream k1-50k-stream" "k2-50k-stream k1-50k-stream" "k2-50k-stream k1b-50k-stream" "k1-50k-stream k1b-50k-stream"; do
    set -- $p; $B browser compare $X/$1.json kiln-today kiln-today --b-run $X/$2.json; done; } > $O/step7.compare.txt 2>&1
for f in $X/pass0-pilot-good.json $X/pass0-pilot-50k.json $D/2026-11-06/before-s2-50k-1-burst.json $D/2026-11-06/rerun5/*.json; do $B browser report $f; done > $O/more.report.txt 2>&1
M=$D/2026-11-06/matrix.json
$B browser report $M > $O/tables.md
python3 $P/say.py $O/tables.md > $O/say.txt
python3 $P/short.py $M > $O/short.md
python3 $P/machine.py $M > $O/machine.md
python3 $P/coverage.py $M > $O/coverage.md
{ for f in $D/2026-11-07/fresh-good-1-request.json $D/2026-11-07/quiet-good-1-request.json; do
    $B browser report $f; for t in kiln-today h2o-h2; do $B browser compare $M $t $t --b-run $f; done; done; } > $O/running.txt 2>&1
cat $D/2026-11-07/*.txt > $O/records.txt
cat $D/2026-11-06/freeze.log $D/2026-11-06/drop-jobs.txt > $O/step8.extra.txt
cp $P/genparts.py $O/ && python3 $O/genparts.py > $O/genparts.txt 2>&1   # a copy: it writes its parts lists beside itself
python3 $P/audit/figures.py $D > $O/pilot.extra.txt
cat crates/kiln-lab/src/browser/*.rs crates/kiln-lab/src/*.rs crates/kiln-lab/web/*.mjs crates/kiln-lab/web/probe/*.js crates/kiln-lab/web/servers/*.sh $P/* 2>/dev/null > $O/code.txt
echo "sources in $O"
