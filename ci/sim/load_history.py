#!/usr/bin/env python3
"""Read-only: each runner's CPU and load over the last hour (5 s samples) from /api/metrics/history."""
import json
import statistics
import subprocess
import urllib.request

BASE = "http://adacachy:7070"
tok = subprocess.run(["kiln", "token"], capture_output=True, text=True, check=True).stdout.strip().splitlines()[-1].strip()
req = urllib.request.Request(BASE + "/api/metrics/history", headers={"Authorization": "Bearer " + tok})
h = json.loads(urllib.request.urlopen(req, timeout=60).read())
print(f"{'runner':28s} {'n':>4s} {'cpus':>4s} {'cpu% mean':>9s} {'cpu% p90':>8s} {'load1 mean':>10s} {'load1/cpus':>10s}")
for name, xs in sorted(h.items()):
    xs = [x for x in xs if x]
    if not xs:
        continue
    cpu = [x["cpu_pct"] for x in xs]
    ld = [x["load1"] for x in xs]
    n = xs[-1].get("cpus") or 1
    p90 = sorted(cpu)[int(0.9 * (len(cpu) - 1))]
    print(f"{name:28s} {len(xs):4d} {n:4d} {statistics.mean(cpu):9.1f} {p90:8.1f} {statistics.mean(ld):10.1f} {statistics.mean(ld) / n:10.2f}")
