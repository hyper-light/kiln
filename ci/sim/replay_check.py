#!/usr/bin/env python3
"""An independent replay of trace.json under P0 (W25 step 1), to cross-check the Rust replayer.

The week as recorded: each run arrives at its time with its own jobs; one run of a concurrency
group runs at a time, and a newer run cancels the older active ones when its workflow said
cancel-in-progress (else only the older waiting ones); jobs take the runners' pools in queue order,
an exclusive job only an empty machine, with nothing held for it; a k8s job starts after its
recorded start delay. What the trace leaves unknown (how long a job that didn't finish would have
taken, how long a pod that didn't start would have waited) is drawn from the nearest completed
runs of the same job, right-censored: longer than it had already run or waited.

    python3 ci/sim/replay_check.py ci/sim/trace.json 1 2 3
"""
import bisect
import collections
import heapq
import json
import random
import sys

TRACE = sys.argv[1] if len(sys.argv) > 1 else "ci/sim/trace.json"
SEEDS = [int(s) for s in sys.argv[2:]] or [1]
K = 4
DONE = ("succeeded", "failed", "timed_out")


def replay(t, seed):
    rng = random.Random(seed)
    end = t["end"]
    comp = collections.defaultdict(list)
    lat = collections.defaultdict(list)
    for r in t["runs"]:
        for j in r["jobs"]:
            s, f, q = j.get("started"), j.get("finished"), j.get("queued")
            if j["status"] in DONE and s is not None and f is not None:
                comp[(r["workflow"], j["key"])].append((r["at"], f - s, j["status"]))
            if "k8s" in j["labels"] and q is not None and s is not None:
                lat[(r["workflow"], j["key"])].append((r["at"], s - q))

    def near(xs, at, more_than):
        """The nearest entries to `at` that took longer than `more_than` (any, for None)."""
        i = bisect.bisect_left(xs, (at,))
        w = K
        while True:
            lo, hi = max(0, i - w // 2), min(len(xs), i + (w - w // 2))
            c = [e for e in xs[lo:hi] if more_than is None or e[1] > more_than]
            if c or (lo == 0 and hi == len(xs)):
                return c
            w *= 2

    pools = {p["name"]: dict(p, busy=0, excl=False) for p in t["pools"] if "slots" in p}
    jobs, runs = [], []
    for r in t["runs"]:
        rr = {"r": r, "status": "pending", "jobs": {}}
        for j in r["jobs"]:
            s, f, q = j.get("started"), j.get("finished"), j.get("queued")
            jb = {"run": rr, "j": j, "state": "wait", "start": None, "end": None, "why": None, "pool": None, "qt": None}
            if j["status"] in DONE and s is not None and f is not None:
                jb["dur"], jb["out"] = f - s, j["status"]
            else:
                ran = (f - s) if (s is not None and f is not None) else ((end - s) if s is not None else None)
                c = near(comp.get((r["workflow"], j["key"])) or [], r["at"], ran)
                jb["dur"], jb["out"] = (c[rng.randrange(len(c))][1:] if c else ((ran or 0) + (ran or 0) // 10 + 1, "succeeded"))
            jb["lat"] = 0
            if "k8s" in j["labels"]:
                if q is not None and s is not None:
                    jb["lat"] = s - q
                else:
                    waited = (f - q) if (q is not None and f is not None) else ((end - q) if q is not None else None)
                    c = near(lat.get((r["workflow"], j["key"])) or [], r["at"], waited)
                    jb["lat"] = c[rng.randrange(len(c))][1] if c else (waited or 0) + (waited or 0) // 10 + 1
            rr["jobs"][j["key"]] = jb
            jobs.append(jb)
        runs.append(rr)

    ev, seq = [], 0

    def push(at, kind, x):
        nonlocal seq
        seq += 1
        heapq.heappush(ev, (at, seq, kind, x))

    for rr in runs:
        push(rr["r"]["at"], "arrive", rr)
    groups = collections.defaultdict(list)
    queue = []

    def free(jb):
        p = pools.get(jb["pool"])
        if p is not None:
            p["busy"] -= 1
            if jb["j"].get("exclusive"):
                p["excl"] = False

    def cancel(rr, at):
        rr["status"] = "cancelled"
        for jb in rr["jobs"].values():
            if jb["state"] == "run":
                jb["state"], jb["end"], jb["why"] = "cancelled", at, "superseded"
                free(jb)
            elif jb["state"] in ("wait", "queued", "launching"):
                jb["state"] = "cancelled"
        if rr in groups[rr["r"].get("group")]:
            groups[rr["r"].get("group")].remove(rr)

    def ready(rr, at):
        moved = True
        while moved:
            moved = False
            for jb in rr["jobs"].values():
                if jb["state"] != "wait":
                    continue
                ns = [rr["jobs"][n] for n in jb["j"].get("needs", []) if n in rr["jobs"]]
                if any(n["state"] in ("failed", "timed_out", "skipped") for n in ns):
                    jb["state"], moved = "skipped", True
                elif all(n["state"] == "succeeded" for n in ns):
                    jb["state"], jb["qt"], moved = "queued", at, True
                    queue.append(jb)

    def concluded(rr, at):
        if rr["status"] == "cancelled":
            return
        st = [jb["state"] for jb in rr["jobs"].values()]
        if all(s in ("succeeded", "failed", "timed_out", "skipped") for s in st):
            rr["status"] = "failed" if any(s in ("failed", "timed_out") for s in st) else "succeeded"
            g = rr["r"].get("group")
            if rr in groups[g]:
                groups[g].remove(rr)

    def gated(rr):
        g = rr["r"].get("group")
        if not g:
            return False
        for o in groups[g]:
            if o is rr:
                return False
            if o["status"] in ("pending", "running"):
                return True
        return False

    def dispatch(at):
        for jb in list(queue):
            if jb["state"] != "queued":
                queue.remove(jb)
                continue
            if gated(jb["run"]):
                continue
            labels = set(jb["j"]["labels"])
            if "k8s" in labels:
                if jb["lat"] > 0 and not jb.get("launched"):
                    jb["launched"], jb["state"] = True, "launching"
                    queue.remove(jb)
                    push(at + jb["lat"], "started", jb)
                    continue
                name = None
            else:
                excl = jb["j"].get("exclusive", False)
                cands = [n for n, p in pools.items() if labels <= set(p["labels"]) and not p["excl"] and (p["busy"] == 0 if excl else p["busy"] < p["slots"])]
                if not cands:
                    continue
                name = min(cands, key=lambda n: (pools[n]["busy"] / pools[n]["slots"], n))
                pools[name]["busy"] += 1
                if excl:
                    pools[name]["excl"] = True
            queue.remove(jb)
            jb["state"], jb["start"], jb["pool"] = "run", at, name
            jb["run"]["status"] = "running"
            push(at + jb["dur"], "finish", jb)

    while ev:
        at, _, kind, x = heapq.heappop(ev)
        if at > end:
            break
        if kind == "arrive":
            g = x["r"].get("group")
            if g:
                older = [o for o in groups[g] if o["status"] in ("pending", "running")]
                for o in older[(0 if x["r"].get("cancel_in_progress") else 1):]:
                    cancel(o, at)
                groups[g].append(x)
            ready(x, at)
            if not x["jobs"]:
                x["status"] = "failed"
        elif kind == "started":
            if x["state"] == "launching":
                x["state"] = "queued"
                queue.insert(0, x)
        elif kind == "finish":
            if x["state"] != "run":
                continue
            x["state"], x["end"] = x["out"], at
            free(x)
            ready(x["run"], at)
            concluded(x["run"], at)
        dispatch(at)

    used = sup = 0
    for jb in jobs:
        if jb["start"] is not None and jb["end"] is not None and jb["end"] <= end:
            used += jb["end"] - jb["start"]
            if jb["why"] == "superseded":
                sup += jb["end"] - jb["start"]
    main = [rr for rr in runs if rr["r"]["workflow"] == "ci" and rr["r"]["branch"] == "main" and rr["r"]["event"] == "push"]
    conc = sum(1 for rr in main if rr["status"] in ("succeeded", "failed"))
    return sup / used, conc, len(main)


t = json.load(open(TRACE))
for seed in SEEDS:
    share, conc, n = replay(t, seed)
    print(f"seed {seed}: superseded {100 * share:.1f}% (measured 25.0%), ci concluded {conc} of {n} = {100 * conc / n:.1f}% (measured 50.7%)")
