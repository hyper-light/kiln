#!/usr/bin/env python3
"""How far superseded jobs got before they were cancelled, against each job's median successful duration."""
import collections
import json
import statistics
import sys

d = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "/tmp/cires/runs.json"))
runs = d["runs"]
ok = collections.defaultdict(list)
for x in runs:
    for j in x["jobs"]:
        if j.get("status") == "succeeded" and j.get("started_at") and j.get("finished_at"):
            ok[(x["run"]["pipeline"], j["key"])].append(j["finished_at"] - j["started_at"])
med = {k: statistics.median(v) for k, v in ok.items() if len(v) >= 5}

prog = collections.defaultdict(list)
never_started = collections.Counter()
for x in runs:
    r = x["run"]
    if r["status"] != "cancelled" or "supersed" not in (r.get("error") or "") and not any("supersed" in (j.get("error") or "") for j in x["jobs"]):
        continue
    for j in x["jobs"]:
        k = (r["pipeline"], j["key"])
        if j.get("status") != "cancelled":
            continue
        if not j.get("started_at"):
            never_started[k] += 1
            continue
        if k in med and j.get("finished_at"):
            prog[k].append((j["finished_at"] - j["started_at"]) / med[k])

print("superseded jobs: progress at cancel = elapsed / median successful duration")
print(f"{'pipeline':9s} {'job':16s} {'n':>4s} {'p50':>5s} {'p90':>5s} {'>=50%':>6s} {'>=80%':>6s}  never-started")
for k in sorted(set(prog) | set(never_started)):
    v = sorted(prog.get(k, []))
    if v:
        p50 = v[len(v) // 2]
        p90 = v[min(len(v) - 1, int(0.9 * (len(v) - 1)))]
        h50 = sum(1 for t in v if t >= 0.5)
        h80 = sum(1 for t in v if t >= 0.8)
        print(f"{k[0]:9s} {k[1]:16s} {len(v):4d} {p50:5.2f} {p90:5.2f} {h50:6d} {h80:6d}  {never_started.get(k, 0)}")
    else:
        print(f"{k[0]:9s} {k[1]:16s} {0:4d} {'-':>5s} {'-':>5s} {'-':>6s} {'-':>6s}  {never_started.get(k, 0)}")

# Work that was running when a newer run arrived, by the newer run's diff to the older: on main it's
# always a descendant, so the older commit's result is covered by the newer only if the newer finishes.
main = sorted([x for x in runs if x["run"]["pipeline"] == "ci" and x["run"]["branch"] == "main"], key=lambda x: x["run"]["created_at"])
shas = [x["run"]["sha"] for x in main]
print(f"\nci main runs: {len(main)}, distinct shas {len(set(shas))}")
