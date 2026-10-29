#!/usr/bin/env python3
"""Summarize v1 runs fetched by runs_fetch.py: supersession, waste, queueing, completion on main."""
import collections
import json
import statistics
import sys
import time

d = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "/tmp/cires/runs.json"))
runs = d["runs"]
print(f"repo {d['repo']}, {len(runs)} runs, window {d['days']} days, fetched {time.ctime(d['fetched_at'])}")
R = [x["run"] for x in runs]
t0, t1 = min(r["created_at"] for r in R), max(r["created_at"] for r in R)
print(f"created from {time.ctime(t0)} to {time.ctime(t1)} ({(t1 - t0) / 86400:.2f} days)")


def pct(a, b):
    return f"{100 * a / b:.1f}%" if b else "n/a"


def q(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = min(len(xs) - 1, max(0, int(round(p / 100 * (len(xs) - 1)))))
    return xs[k]


by_pipe = collections.Counter(r["pipeline"] for r in R)
print("\nruns by pipeline:", dict(by_pipe))
by_event = collections.Counter((r["pipeline"], r["event"]) for r in R)
print("runs by (pipeline,event):", dict(by_event))
st = collections.Counter((r["pipeline"], r["status"]) for r in R)
print("runs by (pipeline,status):", dict(sorted(st.items())))

# Why cancelled: run.error, or job errors.
reasons = collections.Counter()
for x in runs:
    r = x["run"]
    if r["status"] == "cancelled":
        errs = {j.get("error") for j in x["jobs"] if j.get("status") == "cancelled"}
        reasons[(r["pipeline"], r.get("error") or "|".join(sorted(e or "-" for e in errs)))] += 1
print("\ncancelled runs by (pipeline, reason):")
for k, v in reasons.most_common(20):
    print(f"  {v:5d}  {k}")

# Runner time: jobs that started. Wasted = cancelled jobs' started→finished time.
used = collections.Counter()
wasted = collections.Counter()
wasted_super = collections.Counter()
for x in runs:
    for j in x["jobs"]:
        s, f = j.get("started_at"), j.get("finished_at")
        if s and f and f >= s:
            used[x["run"]["pipeline"]] += f - s
            if j.get("status") == "cancelled":
                wasted[x["run"]["pipeline"]] += f - s
                if "supersed" in (j.get("error") or "") or "supersed" in (x["run"].get("error") or ""):
                    wasted_super[x["run"]["pipeline"]] += f - s
print("\nrunner-seconds of started jobs by pipeline; cancelled share; superseded share:")
for p in used:
    print(f"  {p:12s} used {used[p] / 3600:8.1f} h  cancelled {wasted[p] / 3600:7.1f} h ({pct(wasted[p], used[p])})  superseded {wasted_super[p] / 3600:7.1f} h ({pct(wasted_super[p], used[p])})")
tu, tw, ts = sum(used.values()), sum(wasted.values()), sum(wasted_super.values())
print(f"  ALL          used {tu / 3600:8.1f} h  cancelled {tw / 3600:7.1f} h ({pct(tw, tu)})  superseded {ts / 3600:7.1f} h ({pct(ts, tu)})")

# Queue wait per job key (queued_at → started_at), and run duration of successful ci runs.
waits = collections.defaultdict(list)
durs = collections.defaultdict(list)
for x in runs:
    for j in x["jobs"]:
        qa, s, f = j.get("queued_at"), j.get("started_at"), j.get("finished_at")
        if qa and s and s >= qa:
            waits[(x["run"]["pipeline"], j["key"])].append(s - qa)
        if s and f and j.get("status") == "succeeded":
            durs[(x["run"]["pipeline"], j["key"])].append(f - s)
print("\njob queue wait (s) and succeeded duration (s): p50 / p90 / n")
for k in sorted(set(waits) | set(durs)):
    w, du = waits.get(k, []), durs.get(k, [])
    print(f"  {k[0]:9s} {k[1]:16s} wait p50 {q(w, 50)!s:>6} p90 {q(w, 90)!s:>6} n {len(w):4d} | ok-dur p50 {q(du, 50)!s:>6} p90 {q(du, 90)!s:>6} n {len(du):4d}")

# Run wall time (created → finished) for successful ci runs.
rw = [r["finished_at"] - r["created_at"] for r in R if r["pipeline"] == "ci" and r["status"] == "succeeded" and r.get("finished_at")]
print(f"\nci succeeded runs: created→finished p50 {q(rw, 50)} s, p90 {q(rw, 90)} s, n {len(rw)}")

# main: push cadence vs completion.
for pipe in sorted(by_pipe):
    main = sorted([r for r in R if r["pipeline"] == pipe and r["branch"] == "main" and r["event"] == "push"], key=lambda r: r["created_at"])
    if len(main) < 2:
        continue
    gaps = [b["created_at"] - a["created_at"] for a, b in zip(main, main[1:])]
    c = collections.Counter(r["status"] for r in main)
    print(f"\n{pipe} on main (push): {len(main)} runs, statuses {dict(c)}; inter-push gap p10 {q(gaps, 10)} p50 {q(gaps, 50)} p90 {q(gaps, 90)} s")
    # Longest streak of consecutive main runs without a completed (succeeded/failed) one.
    streak = best = 0
    for r in main:
        if r["status"] in ("succeeded", "failed", "timed_out"):
            streak = 0
        else:
            streak += 1
            best = max(best, streak)
    print(f"  longest streak of main runs that never concluded (cancelled/other): {best}")
    concluded = [r for r in main if r["status"] in ("succeeded", "failed", "timed_out")]
    print(f"  concluded: {len(concluded)} of {len(main)} ({pct(len(concluded), len(main))})")
    # Time on main without any concluded result: gaps between successive concluded runs' finish times.
    fin = sorted(r["finished_at"] for r in concluded if r.get("finished_at"))
    fg = [b - a for a, b in zip(fin, fin[1:])]
    if fg:
        print(f"  gap between concluded results on main: p50 {q(fg, 50)} p90 {q(fg, 90)} max {max(fg)} s")

# Branch/PR activity: how many branches, and runs per branch.
br = collections.Counter((r["pipeline"], r["event"], r["branch"]) for r in R if r["pipeline"] == "ci")
print(f"\nci: distinct (event,branch): {len(br)}; top: {br.most_common(8)}")
authors = collections.Counter(r.get("author") for r in R if r["pipeline"] == "ci")
print(f"ci authors: {authors.most_common(10)}")

# Arrival rate vs. capacity: runs of ci per hour, and runner-hours of ci per day.
hours = collections.Counter(int(r["created_at"] // 3600) for r in R if r["pipeline"] == "ci")
hv = list(hours.values())
print(f"ci runs/hour: mean {statistics.mean(hv):.1f}, p90 {q(hv, 90)}, max {max(hv)}")
