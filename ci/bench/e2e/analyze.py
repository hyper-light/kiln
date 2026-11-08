#!/usr/bin/env python3
"""LB14's analysis: run.py's runs into added latencies, ratios and verdicts.

    analyze.py RESULTS.jsonl... [--json OUT] [--check]

For each matched-rate cell (sweep, N, rate) and round:
- a proxy's added latency at percentile p is its path's p minus the direct path's p (wrk2 straight
  to one backend; or, for gen:<proxy>, the generator's HTTP path straight to one) in the same round;
  kiln's is its reach path's p minus the direct spawn's p in the same round. Both from intended send
  time (wrk2's corrected histogram, the generator's `started`), and again from the actual send (wrk2
  -U, the generator's `since_sent`), shown beside. wrk2's from-intended figure carries its own timer
  (it sends on whole milliseconds, up to about 1.5 ms after a request was due), which is the same on
  a proxy's path and the direct one only to first order; the generator's dispatcher spins to each due
  time, so its from-intended figure is the fine one, and is the verdict's.
- the ratio for (competitor, algorithm, N, rate, p) is the median over rounds of the competitor's
  added latency over the median over rounds of kiln's. Its 95% interval: the percentile bootstrap
  (Efron and Tibshirani 1993, ch. 13) resampling rounds, each round's runs together (the rounds
  interleave every system, so a resampled round keeps each system's drift with the others').
- a cell passes when the interval's lower bound is at least 10 (the person's bar: an order of
  magnitude faster). Failures are printed beside passes.

Bootstrap resamples B: enough that the interval's ends (the 2.5th and 97.5th percentiles of the
resampled ratios) are themselves within the 5% precision target in rank: the end's rank spread is
sqrt(B q (1 - q)), so B >= (1 - q) / q * (1.96 / 0.05)^2 with q = 0.025 (59,928). `--check` (the
sweep's stopping rule, bench-spec.md) says whether every system's median p50 and p99 over rounds
has a 95% bootstrap interval within 5% of itself. Standard library only.
"""

import argparse
import json
import math
import random
import statistics
import sys
from collections import defaultdict

Z95 = 1.959963984540054
TARGET = 0.05
Q = 0.025
B = math.ceil((1 - Q) / Q * (Z95 / TARGET) ** 2)
BAR = 10.0
PCTS = ("p50_us", "p99_us")
# The bootstrap's own generator: seeded, so the same runs give the same intervals.
SEED = 0x4C423134


def load(paths):
    rows = []
    for p in paths:
        with open(p) as f:
            for line in f:
                line = line.strip()
                if line.startswith("{"):
                    rows.append(json.loads(line))
    return rows


def system_of(r):
    return r["impl"] + (":" + r["algo"] if r.get("algo") else "")


def cells_of(rows):
    """{(sweep, n, rate): {round: {system: row}}} for the runs that went through."""
    cells = defaultdict(lambda: defaultdict(dict))
    for r in rows:
        if r.get("kind") != "run" or not r.get("ok"):
            continue
        cells[(r["sweep"], r["n"], r["rate"])][r["round"]][system_of(r)] = r
    return cells


def boot(values_by_round, stat, rng):
    """The percentile bootstrap's 95% interval of stat(rounds), resampling rounds."""
    rounds = list(values_by_round)
    k = len(rounds)
    if k < 2:
        return None
    reps = []
    for _ in range(B):
        sample = [rounds[rng.randrange(k)] for _ in range(k)]
        v = stat(sample)
        if v is not None:
            reps.append(v)
    if not reps:
        return None
    reps.sort()
    lo = reps[max(0, math.floor(Q * len(reps)) - 1)]
    hi = reps[min(len(reps) - 1, math.ceil((1 - Q) * len(reps)) - 1)]
    return lo, hi


def added(rounds, system, base, key, p):
    """{round: system's p minus base's p}, for rounds that have both."""
    out = {}
    for rnd, by in rounds.items():
        s, b = by.get(system), by.get(base)
        if s and b and s[key].get(p) is not None and b[key].get(p) is not None:
            out[rnd] = s[key][p] - b[key][p]
    return out


def med(xs):
    return statistics.median(xs) if xs else None


def base_of(comp):
    """The path a competitor's added latency is taken over: the same tool, straight to a backend."""
    return "gen:direct" if comp.startswith("gen:") else "direct"


def ratio_cell(rounds, comp, kiln_sys, key, p, rng):
    c = added(rounds, comp, base_of(comp), key, p)
    k = added(rounds, kiln_sys, "spawn", key, p)
    both = sorted(set(c) & set(k))
    if not both:
        return None
    mc, mk = med([c[r] for r in both]), med([k[r] for r in both])

    def stat(sample):
        a, b = med([c[r] for r in sample]), med([k[r] for r in sample])
        return None if not b or b <= 0 else a / b

    ci = boot(both, stat, rng)
    point = None if not mk or mk <= 0 else mc / mk
    return {"rounds": len(both), "competitor_added_us": mc, "kiln_added_us": mk, "ratio": point, "ci": ci, "pass": bool(ci and ci[0] >= BAR)}


def converged(rows, rng=None):
    """Whether every system's median p50 and p99 over rounds, in every cell, has a 95% bootstrap
    interval within 5% of itself (bench-spec.md's rule for when rounds may stop)."""
    rng = rng or random.Random(SEED)
    worst = []
    for cell, rounds in cells_of(rows).items():
        systems = {s for by in rounds.values() for s in by}
        for s in systems:
            for p in PCTS:
                vals = {rnd: by[s]["lat"][p] for rnd, by in rounds.items() if s in by and by[s]["lat"].get(p) is not None}
                m = med(list(vals.values()))
                ci = boot(list(vals), lambda sample: med([vals[r] for r in sample]), rng)
                if not m or not ci:
                    worst.append((cell, s, p, None))
                    continue
                half = max(m - ci[0], ci[1] - m) / m
                if half > TARGET:
                    worst.append((cell, s, p, half))
    return not worst, worst


def quality(rounds, system):
    """The spread of the work (the coefficient of variation of requests per backend, or of asks per
    runner), what was lost, the system's own CPU per request, and how many cores the idle kiln
    fleets used meanwhile (load on the machine the system didn't cause)."""
    rs = [by[system] for by in rounds.values() if system in by]
    cvs = [r["quality"].get("cv") for r in rs if r.get("quality", {}).get("cv") is not None]
    lost = sum(sum(r.get("errors", {}).values()) + r.get("refused", 0) + r.get("failed", 0) for r in rs)
    per_req = []
    for r in rs:
        cpu = r.get("cpu_s") or {}
        own = cpu.get("proxy", 0) + cpu.get("server", 0) + cpu.get("runners", 0)
        n = (r.get("achieved_per_s") or 0) * (r.get("window_s") or 0)
        if own and n:
            per_req.append(own / n * 1e6)
    fleet = [r["fleets_cpu_s"] / r["wall_s"] for r in rs if r.get("wall_s")]
    pressure = [r["cpu_pressure"] for r in rs if r.get("cpu_pressure") is not None]
    return {"cv_median": med(cvs), "lost": lost, "cpu_us_per_request": med(per_req), "idle_fleet_cores": med(fleet), "cpu_pressure": med(pressure)}


def summary(rows, rng, out):
    """One line per (N, rate, competitor, algorithm): the verdict's measure (the generator's added
    latency from intended send time, over kiln's), wrk2's added latency from the send beside it,
    and how much of kiln's latency is its own decision (ask to Picked) rather than the path."""
    print("== summary (ratio = competitor added / kiln added; pass = 95% lower bound >= 10)")
    tally = defaultdict(lambda: [0, 0])
    for (sweep, n, rate), rounds in sorted(cells_of(rows).items(), key=lambda x: (x[0][1], x[0][2], x[0][0])):
        systems = {s for by in rounds.values() for s in by}
        kilns = sorted(s for s in systems if s.startswith("kiln"))
        comps = sorted(s for s in systems if s.startswith("gen:") and s != "gen:direct")
        for k in kilns:
            picked = [by[k]["answer"]["p50_ns"] / 1e3 for by in rounds.values() if k in by and by[k].get("answer")]
            print("N=%d rate=%g/s: %s: added p50/p99 from the pick alone (ask to Picked, median p50 over %d rounds): %s us" % (n, rate, k, len(picked), fmt(med(picked))))
            for c in comps:
                wrk = c[len("gen:") :]
                parts = []
                for p in PCTS:
                    r = ratio_cell(rounds, c, k, "lat", p, rng)
                    if not r:
                        continue
                    w = added(rounds, wrk, "direct", "lat_u", p)
                    verdict = "PASS" if r["pass"] else "FAIL"
                    tally[(k, p)][0 if r["pass"] else 1] += 1
                    ci = r["ci"] or (None, None)
                    parts.append("%s adds %s us (wrk2 from send %s) kiln %s us ratio %s [%s, %s] %s" % (p[:3], fmt(r["competitor_added_us"]), fmt(med(list(w.values()))), fmt(r["kiln_added_us"]), fmt(r["ratio"], 3), fmt(ci[0], 3), fmt(ci[1], 3), verdict))
                print("   %-26s %s" % (c, " | ".join(parts)))
    for (k, p), (ok, no) in sorted(tally.items()):
        print("  %s %s: %d pass, %d fail" % (k, p[:3], ok, no))
    out["summary"] = {"%s %s" % key: {"pass": v[0], "fail": v[1]} for key, v in tally.items()}
    print()


def fmt(v, nd=1):
    return "-" if v is None else ("%.*f" % (nd, v))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--json")
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    rows = load(a.paths)
    rng = random.Random(SEED)
    if a.check:
        ok, worst = converged(rows, rng)
        print(json.dumps({"converged": ok, "unconverged": [[list(map(str, c)), s, p, h] for c, s, p, h in worst]}))
        return 0
    out = {"bootstrap_resamples": B, "bar": BAR, "cells": [], "knees": [r for r in rows if r.get("kind") == "knee"]}
    print("LB14 end to end: added latency (path minus direct, same round), ratio = competitor / kiln; pass = 95%% lower bound >= %g" % BAR)
    print("B = %d bootstrap resamples over rounds\n" % B)
    for (sweep, n, rate), rounds in sorted(cells_of(rows).items(), key=lambda x: (x[0][0], x[0][1], x[0][2])):
        systems = sorted({s for by in rounds.values() for s in by})
        kilns = [s for s in systems if s.startswith("kiln")]
        # Competitors through each tool: the generator's first (the verdict's), then wrk2's beside it.
        comps = [s for s in systems if s.startswith("gen:") and s != "gen:direct"] + [s for s in systems if s.split(":")[0] in ("nginx", "haproxy", "envoy")]
        print("== %s  N=%d  rate=%g/s  rounds=%d" % (sweep, n, rate, len(rounds)))
        for s in systems:
            lat = [by[s]["lat"] for by in rounds.values() if s in by]
            latu = [by[s]["lat_u"] for by in rounds.values() if s in by]
            q = quality(rounds, s)
            print("   %-28s p50 %9s us  p99 %9s us  | from send p50 %9s p99 %9s | cv %5s lost %d | cpu/req %7s us | idle fleets %5s cores | pressure %5s" % (s, fmt(med([x["p50_us"] for x in lat])), fmt(med([x["p99_us"] for x in lat])), fmt(med([x["p50_us"] for x in latu if x.get("p50_us") is not None])), fmt(med([x["p99_us"] for x in latu if x.get("p99_us") is not None])), fmt(q["cv_median"], 3), q["lost"], fmt(q["cpu_us_per_request"]), fmt(q["idle_fleet_cores"], 2), fmt(q["cpu_pressure"], 3)))
        for k in kilns:
            for c in comps:
                for key, label in (("lat", "from intended"), ("lat_u", "from send")):
                    for p in PCTS:
                        r = ratio_cell(rounds, c, k, key, p, rng)
                        if not r:
                            continue
                        verdict = "PASS" if r["pass"] else "FAIL"
                        ci = r["ci"]
                        print("   %s %-26s vs %-8s %-13s %s: competitor adds %8s us, kiln adds %10s us, ratio %8s [%s, %s]" % (verdict, c, k, label, p[:3], fmt(r["competitor_added_us"]), fmt(r["kiln_added_us"]), fmt(r["ratio"], 4), fmt(ci[0] if ci else None, 4), fmt(ci[1] if ci else None, 4)))
                        out["cells"].append({"sweep": sweep, "n": n, "rate": rate, "competitor": c, "kiln": k, "from": key, "p": p, **r})
        print()
    summary(rows, rng, out)
    if out["knees"]:
        print("== highest sustained rates")
        for kn in out["knees"]:
            print("   %-28s N=%-4d sustained %s/s, not sustained at %s/s" % (kn["impl"], kn["n"], fmt(kn.get("sustained"), 0), fmt(kn.get("not_sustained"), 0)))
    if a.json:
        with open(a.json, "w") as w:
            json.dump(out, w, indent=1, default=str)
    return 0


if __name__ == "__main__":
    sys.exit(main())
