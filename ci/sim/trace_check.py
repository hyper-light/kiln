#!/usr/bin/env python3
"""Checks trace.json (W25 step 1): it still gives runs_analyze.py's figures, and it is anonymous.

Recomputes, from trace.json alone, the share of runner time on jobs cancelled as superseded and
how many ci pushes to main concluded, as runs_analyze.py computes them from runs.json. Given the
runs.json it was exported from, it also fails if any sha, author, runner name, run or job id of
that file appears in trace.json.

    python3 ci/sim/trace_check.py ci/sim/trace.json [/tmp/cires/runs.json]
"""
import json
import re
import sys

TRACE = sys.argv[1] if len(sys.argv) > 1 else "ci/sim/trace.json"
RUNS = sys.argv[2] if len(sys.argv) > 2 else None
DONE = ("succeeded", "failed", "timed_out")

text = open(TRACE, encoding="utf-8").read()
t = json.loads(text)
ok = True

used = superseded = 0
for r in t["runs"]:
    for j in r["jobs"]:
        s, f = j.get("started"), j.get("finished")
        if s is not None and f is not None and f >= s:
            used += f - s
            if j["status"] == "cancelled" and j.get("why") == "superseded":
                superseded += f - s
share = superseded / used
print(f"runner time: used {used / 3600:.1f} h, superseded {superseded / 3600:.1f} h ({100 * share:.1f}%)")
main = [r for r in t["runs"] if r["workflow"] == "ci" and r["branch"] == "main" and r["event"] == "push"]
concluded = sum(1 for r in main if r["status"] in DONE)
print(f"ci pushes to main concluded: {concluded} of {len(main)} ({100 * concluded / len(main):.1f}%)")
m = t["measured"]
if round(100 * share, 1) != round(100 * m["superseded_share"], 1) or concluded != m["ci_concluded"] or len(main) != m["ci_pushes"]:
    print(f"MISMATCH with the measured figures: {m}")
    ok = False

if re.search(r"\b[0-9a-f]{40}\b", text):
    print("a 40-hex sha is in the trace")
    ok = False
if RUNS:
    d = json.load(open(RUNS))
    tokens = set()
    for x in d["runs"]:
        r = x["run"]
        tokens |= {r["sha"], r["sha"][:12], r["id"], r.get("repo_id") or "", r.get("author") or ""}
        for j in x["jobs"]:
            tokens |= {j["id"], j.get("agent") or ""}
    tokens = {tok for tok in tokens if len(tok) >= 6}
    found = sorted(tok for tok in tokens if tok in text)
    print(f"identifying tokens checked: {len(tokens)}; found in the trace: {len(found)}")
    if found:
        print("  " + ", ".join(found[:10]))
        ok = False
print("ok" if ok else "FAILED")
sys.exit(0 if ok else 1)
