#!/usr/bin/env python3
# Pass 1's samples against the plan (plan 01a1a832e9cc step 8): for every cell of the merged run
# (case, condition, browser, target), its rounds, and each measure's samples against what its rounds
# planned at its K (`runfile::workload`'s paces, kiln-lab browser plan's operations a round). Prints
# a Markdown table of the cells short of the plan, by how much, and the cells' count.
#   coverage.py MERGED.json
import collections, json, sys
OPS_8 = {("good", "stream"): 21600, ("good", "burst"): 6400, ("good", "request"): 4800,
         ("100k", "stream"): 6480, ("100k", "burst"): 2880, ("100k", "request"): 720,
         ("50k", "stream"): 3240, ("50k", "burst"): 1440, ("50k", "request"): 360}
ROUNDS = {("100k", "request"): 6, ("50k", "request"): 11}
jobs = json.load(open(sys.argv[1]))["jobs"]
cells = collections.defaultdict(list)
for j in jobs:
    cells[(j["case"], j["condition"], j["browser"], j["target"])].append(j)
short = []; rows_short = 0
for (case, cond, browser, target), js in sorted(cells.items()):
    prof = cond.split("/")[0]; k = js[0]["k"]
    want_rounds = 13 if (case, prof, browser) == ("stream", "50k", "webkit") else ROUNDS.get((prof, case), 5)
    want = OPS_8[(prof, case)] * k // 8 * want_rounds
    rounds = sorted({j["round"] for j in js})
    n = {m: sum(j["numbers"].get(f"{m}.n", 0) for j in js) for m in ("time", "bytes", "cpu_us")}
    dup = len(js) - len(rounds)
    if len(rounds) < want_rounds or dup or n["time"] < want * 0.99:
        short.append(f"| {case} | {cond} | {browser} | {target} | {len(rounds)} of {want_rounds}{f', {dup} repeated' if dup else ''} | {n['time']:.0f} of {want} ({100 * n['time'] / want:.0f}%) | {n['bytes']:.0f} | {n['cpu_us']:.0f} |")
# The matrix's cells by target: the bar's five, both browsers, the HTTP/3 rows in Chromium only.
want_cells = {(c, f"{p}/{l}", b, t) for c in ("stream", "burst", "request") for p in ("good", "100k", "50k") for l in (0, 1, 5)
              for b in ("chromium", "webkit") for t in ("kiln-today", "h2o-h2", "caddy-h2", "h2o-h3", "caddy-h3") if b == "chromium" or not t.endswith("-h3")}
missing = sorted(want_cells - set(cells))
print(f"{len(cells)} cells of target ({len(want_cells)} planned), {len(short)} short of the plan, {len(missing)} with no jobs\n")
for m in missing:
    print(f"- no jobs: {' '.join(m)}")
if missing:
    print()
print("| case | condition | browser | target | rounds | time's samples of the plan | bytes' | CPU's |\n| --- | --- | --- | --- | --- | --- | --- | --- |")
print("\n".join(short))
