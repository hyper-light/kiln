#!/bin/bash
# Step 7's checks on pass 1's pilot (plan 01a1a832e9cc): the A/A, the K-links-against-one check
# (under the pass's own load, with K lowered where K = 8 differs), h2o's QUIC controller, the
# pilot's tables, and report and compare reading the run files back. Run on the lab machine after
# the pilot's parts and the K check's (`parts-kcheck`: K = 4, 2 and 1 again at 50k/1 stream).
set -u
G=$HOME/.cache/kiln-bp/g1; O=${KILN_LAB_OUT:-$G/lab}; L=${KILN_LAB:-$G/lab-snap1/kiln-lab}; A=$O/step7; mkdir -p $A
$L browser merge --out $A/pilot.json $O/pilot-good-*/run.json $O/pilot-50k-*/run.json
echo "== A/A: kiln today against itself, interleaved on the same seeds"
$L browser compare $A/pilot.json kiln-today kiln-today-b | head -40; echo AA_EXIT=${PIPESTATUS[0]}
echo "== K links against one (the lab machine, while the pass's other parts ran)"
for k in xm k4 k2 k1b; do
  [ -f $O/$k-50k-stream/run.json ] || continue
  echo "-- 50k/1 stream: $k against k1"
  $L browser compare $O/$k-50k-stream/run.json kiln-today kiln-today --b-run $O/k1-50k-stream/run.json | head -30
done
echo "-- good/0 request: K = 8 (the pilot) against k1"
$L browser compare $A/pilot.json kiln-today kiln-today --b-run $O/k1-good-request/run.json | head -30
echo "== h2o's QUIC controller at 50k/1 (median p99 of every job)"
python3 - $O/pilot-50k-*/run.json <<'PY'
import json, statistics, sys
jobs = [j for f in sys.argv[1:] for j in json.load(open(f))["jobs"]]
for t in ["h2o-h3", "h2o-h3-reno"]:
    v = [j["numbers"].get("time.p99_ms") for j in jobs if j["target"] == t and j["numbers"].get("time.p99_ms") is not None]
    print(t, "median p99", round(statistics.median(v), 1) if v else None, "ms over", len(v), "jobs")
PY
echo "== the machine during the pilot and the K check (each job's load and CPU pressure)"
python3 - $A/pilot.json $O/k*-*/run.json $O/xm-50k-stream/run.json <<'PY'
import json, sys, statistics
for f in sys.argv[1:]:
    jobs = json.load(open(f))["jobs"]
    load = [float(j["machine"]["load"].split()[0]) for j in jobs if j.get("machine", {}).get("load")]
    psi = [float(j["machine"]["cpu_pressure"].split()[1].split("=")[1]) for j in jobs if "avg10=" in j.get("machine", {}).get("cpu_pressure", "")]
    host = {j.get("machine", {}).get("host") for j in jobs}
    print(f.split("/")[-2], "jobs", len(jobs), "host", host, "load1 median", statistics.median(load) if load else None, "max", max(load) if load else None, "cpu some avg10 median", statistics.median(psi) if psi else None)
PY
echo "== report and compare read the run files back: the tables each part printed"
# Each part's folder only: `<part>.take` (a claim) and `<part>.skip` are markers beside them.
for p in $O/pilot-good-* $O/pilot-50k-*; do
  [ -f $p/run.json ] || continue
  diff <(grep -E '^(####|\|)' $p.log) <($L browser report $p/run.json | grep -E '^(####|\|)') > /dev/null && echo "$(basename $p): the same" || echo "$(basename $p): DIFFER"
done
$L browser report $A/pilot.json > $A/pilot-tables.md; echo "pilot tables: $(grep -c '^|' $A/pilot-tables.md) rows, $A/pilot-tables.md"
