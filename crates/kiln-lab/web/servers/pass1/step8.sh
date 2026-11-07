#!/bin/bash
# Step 8 (plan 01a1a832e9cc): pass 1's matrix as one run, on the lab machine after its parts end.
# The parts that count: the pilot's, the matrix's (`m-`, `m2-`: WebKit's 50 kbit/s streams at K = 2)
# and the freezes' rounds run again (`f<list>-`, `g<list>-`), and the baselines' streams and bursts
# again (`s2-`); not the K check's (`k1-`, `k2-`, `k4-`, `xm-`).
# Left out: the K = 8 WebKit streams at 50k/1 (step 7), the jobs a memory freeze overlapped
# (frozen.py's list), jobs whose page clock is past step 4's limit, the baselines' streams and bursts from before `s2-` (their origin counted
# frames, not events: about 65% of kiln's events a burst), and the pilot's extra rows (kiln-today-b, the A/A; h2o-h3-reno, the
# controller not chosen), so the best baseline is among the bar's five targets.
#   step8.sh KILN_LAB    writes $OUT/step8/{matrix.json,matrix-slim.json,coverage.md,tables.md}
set -euo pipefail
LAB=$1
G=$HOME/.cache/kiln-bp/g1; OUT=${KILN_LAB_OUT:-$G/lab}; A=$OUT/step8; HERE=$(dirname "$0")
mkdir -p $A
runs=()
for d in $OUT/pilot-* $OUT/m-* $OUT/m2-* $OUT/f?-* $OUT/g?-* $OUT/s2-*; do
  b=$(basename $d)
  [[ $b == *.* ]] && continue
  [ -f $d/done ] && [ -f $d/run.json ] && runs+=($d/run.json)
done
# The jobs left out by when each ended: the freezes', and the baselines' streams and bursts before s2.
python3 - $A/drop-jobs.txt $OUT/step7/frozen-jobs.txt "${runs[@]}" <<'PY'
import json, os, sys
out, frozen, runs = sys.argv[1], sys.argv[2], sys.argv[3:]
lines = [l for l in open(frozen) if l.strip() and not l.startswith("#")]
n = c = 0
for f in runs:
    part = os.path.basename(os.path.dirname(f))
    after = part.startswith("s2-") or (part[0] == "g" and part[2:3] == "-")  # run on lab-snap2's origin
    for j in json.load(open(f))["jobs"]:
        # A job whose page clock is past step 4's limit (200 µs in Chromium, the timer's grain in WebKit).
        cal, limit = j.get("calibration_p99_us"), max({"chromium": 200.0, "webkit": 1000.0}.get(j["browser"], 200.0), float(j.get("timer_grain_us") or 0))
        if cal is not None and cal > limit:
            lines.append(f"{j['started_us']} # {part} {j['condition']} {j['case']} {j['browser']} {j['target']} r{j['round']}: clock residual {cal:.0f} µs\n"); c += 1
        elif not after and j["target"] != "kiln-today" and j["case"] in ("stream", "burst"):
            lines.append(f"{j['started_us']} # {part} {j['condition']} {j['case']} {j['browser']} {j['target']} r{j['round']}: before s2\n"); n += 1
open(out, "w").write("# step 8's left-out jobs: the freezes' (frozen.py), those past the clock's limit, and the baselines' streams and bursts before s2\n" + "".join(lines))
print(f"{len(lines)} jobs left out: {n} baseline streams and bursts before s2, {c} past the clock's limit", file=sys.stderr)
PY
drops=(--drop stream,webkit,50k/1,8 --drop-jobs $A/drop-jobs.txt --drop-target kiln-today-b --drop-target h2o-h3-reno)
$LAB browser merge --out $A/matrix.json "${drops[@]}" "${runs[@]}"
$LAB browser merge --out $A/matrix-slim.json --slim "${drops[@]}" "${runs[@]}"
python3 $HERE/coverage.py $A/matrix.json > $A/coverage.md
$LAB browser report $A/matrix-slim.json > $A/tables.md
echo "${#runs[@]} parts; $(wc -c < $A/matrix-slim.json) bytes slim"
head -3 $A/coverage.md
