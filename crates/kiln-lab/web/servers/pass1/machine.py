#!/usr/bin/env python3
# Step 8 (plan 01a1a832e9cc): what the lab machine was doing while the matrix ran, from what each job
# recorded: its one-minute load and CPU pressure (some, avg10) as it started, and its page's clock
# (the calibration's residual p99, against step 4's limits: 200 µs in Chromium, the 1 ms timer grain
# in WebKit). Prints a Markdown table by browser, and the jobs past the clock's limit.
#   machine.py MATRIX.json
import collections, json, statistics, sys

LIMIT_US = {"chromium": 200.0, "webkit": 1000.0}
jobs = json.load(open(sys.argv[1]))["jobs"]
by = collections.defaultdict(list)
over = []
for j in jobs:
    m = j.get("machine") or {}
    try:
        load = float(str(m.get("load", "")).split()[0])
    except (ValueError, IndexError):
        load = None
    psi = None
    for part in str(m.get("cpu_pressure", "")).split():
        if part.startswith("avg10="):
            psi = float(part.split("=")[1])
    cal = j.get("calibration_p99_us")
    by[j["browser"]].append((load, psi, cal, m.get("host")))
    limit = max(LIMIT_US.get(j["browser"], 200.0), float(j.get("timer_grain_us") or 0))
    if cal is not None and cal > limit:
        over.append(f"{j['case']} {j['condition']} {j['browser']} {j['target']} r{j['round']}: {cal:.0f} µs")


def q(v, p):
    v = sorted(x for x in v if x is not None)
    return v[min(len(v) - 1, int(p * len(v)))] if v else float("nan")


print("| browser | jobs | hosts | load (1 min): median, p90, max | CPU pressure (avg10, %): median, p90, max | clock residual p99 (µs): median, max |")
print("| --- | --- | --- | --- | --- | --- |")
for b, rows in sorted(by.items()):
    loads = [r[0] for r in rows]; psis = [r[1] for r in rows]; cals = [r[2] for r in rows]
    hosts = ", ".join(sorted({str(r[3]) for r in rows}))
    print(f"| {b} | {len(rows)} | {hosts} | {statistics.median(x for x in loads if x is not None):.1f}, {q(loads, 0.9):.1f}, {max(x for x in loads if x is not None):.1f} | "
          f"{statistics.median(x for x in psis if x is not None):.1f}, {q(psis, 0.9):.1f}, {max(x for x in psis if x is not None):.1f} | "
          f"{statistics.median(x for x in cals if x is not None):.0f}, {max(x for x in cals if x is not None):.0f} |")
print()
print(f"Jobs whose clock residual is past its browser's limit: {len(over)} of {len(jobs)}.")
for o in over[:40]:
    print(f"- {o}")
