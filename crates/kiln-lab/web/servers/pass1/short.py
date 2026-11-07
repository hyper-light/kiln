#!/usr/bin/env python3
# Step 8 (plan 01a1a832e9cc): each cell of the merged matrix against step 6's plan, for the doc.
# Step 6's floor is 10,000 samples a cell, and 3,688 where the budget put it there (pass 1: the slow
# links' requests and 50k/5's bursts). A cell's samples are its time's (operations completed, plus
# requests missed by the job's end, counted with their wait); its operations that failed outright
# (a request that errored) are counted apart, as are its rounds against those planned.
#   short.py MATRIX.json
import collections, json, sys

FLOOR, LOW = 10_000, 3_688


def plan(case, prof, loss, browser):
    """(rounds, floor) step 6's plan gives a cell, as pass 1 runs it."""
    if (case, prof, browser) == ("stream", "50k", "webkit"):
        return 13, FLOOR
    if case == "request":
        return {"good": 5, "100k": 6, "50k": 11}[prof], (FLOOR if prof == "good" else LOW)
    if (case, prof, loss) == ("burst", "50k", "5"):
        return 5, LOW
    return 5, FLOOR


jobs = json.load(open(sys.argv[1]))["jobs"]
cells = collections.defaultdict(list)
for j in jobs:
    cells[(j["case"], j["condition"], j["browser"], j["target"])].append(j)
under, failing, rounds_short = [], [], []
for (case, cond, browser, target), js in sorted(cells.items()):
    prof, loss = cond.split("/")
    want_rounds, floor = plan(case, prof, loss, browser)
    s = lambda k: sum(j["numbers"].get(k, 0) for j in js)
    n, ops, failed, missed = s("time.n"), s("ops"), s("failed"), s("missed")
    rounds = len({j["round"] for j in js})
    row = (case, cond, browser, target)
    if n < floor:
        under.append((row, n, floor, s("bytes.n"), s("cpu_us.n")))
    if failed > 0.01 * max(ops, 1):
        failing.append((row, failed, ops))
    if rounds < want_rounds:
        rounds_short.append((row, rounds, want_rounds))
print(f"{len(cells)} cells of target. Under step 6's floor (time's samples): {len(under)}; with more than 1% of their operations failed: {len(failing)}; short of their planned rounds: {len(rounds_short)}.\n")
if under:
    print("| case | condition | browser | target | time's samples | floor | short by | bytes' samples | CPU's samples |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    for (c, co, b, t), n, f, bn, cn in under:
        print(f"| {c} | {co} | {b} | {t} | {n:,.0f} | {f:,} | {100 * (f - n) / f:.0f}% | {bn:,.0f} | {cn:,.0f} |")
    print()
if failing:
    print("Operations that failed outright (not in the time's samples):\n\n| case | condition | browser | target | failed | of operations |\n| --- | --- | --- | --- | --- | --- |")
    for (c, co, b, t), fl, ops in failing:
        print(f"| {c} | {co} | {b} | {t} | {fl:,.0f} | {ops:,.0f} ({100 * fl / ops:.0f}%) |")
    print()
if rounds_short:
    print("Cells short of their planned rounds:\n")
    for (c, co, b, t), r, w in rounds_short:
        print(f"- {c} {co} {b} {t}: {r} of {w}")
    print()
# Streams and bursts have no misses: an event still queued at the job's end has no time. A target
# whose cell holds far fewer events than the baselines' (the same schedule) left that many behind,
# so its time's percentiles are of what arrived: a lower bound.
by_cell = collections.defaultdict(dict)
for (case, cond, browser, target), js in cells.items():
    if case in ("stream", "burst"):
        by_cell[(case, cond, browser)][target] = sum(j["numbers"].get("ops", 0) / max(1, len({x["round"] for x in js})) for j in js)
behind = []
for (case, cond, browser), per in sorted(by_cell.items()):
    base = sorted(v for t, v in per.items() if t != "kiln-today")
    if not base:
        continue
    ref = base[len(base) // 2]
    for t, v in per.items():
        if v < 0.9 * ref:
            behind.append((case, cond, browser, t, v, ref))
if behind:
    print("Events left behind at the job's end (a round's events against the baselines' median in the cell; under 90%):\n\n| case | condition | browser | target | events a round | baselines' | delivered |\n| --- | --- | --- | --- | --- | --- | --- |")
    for c, co, b, t, v, ref in behind:
        print(f"| {c} | {co} | {b} | {t} | {v:,.0f} | {ref:,.0f} | {100 * v / ref:.0f}% |")
