#!/usr/bin/env python3
# Step 8 (plan 01a1a832e9cc): the rounds the merged matrix is missing, as a run-pass-mem.sh list.
# A round is missing where a cell (case, condition, browser, target) has no job for one of the round
# numbers step 6's plan gives it: a job that failed by name (a server that didn't come up, drivers
# that left no result), or one left out (a memory freeze). Each runs again on its own seeds under its
# own number (`--first-round R --rounds 1`), the targets missing the same round together.
#   fill.py MATRIX.json TAG PARTS_OUT
import collections, json, sys

TARGETS = ("kiln-today", "h2o-h2", "caddy-h2", "h2o-h3", "caddy-h3")


def plan(case, prof, browser):
    """(rounds, K) step 6's plan gives a cell, as pass 1 runs it."""
    if (case, prof, browser) == ("stream", "50k", "webkit"):
        return 13, 2
    if case == "request":
        return {"good": 5, "100k": 6, "50k": 11}[prof], 8
    return 5, 8


matrix, tag, out = sys.argv[1:]
have = collections.defaultdict(set)
for j in json.load(open(matrix))["jobs"]:
    have[(j["case"], j["condition"], j["browser"], j["target"])].add(j["round"])
groups = collections.defaultdict(list)
for case in ("stream", "burst", "request"):
    for prof in ("good", "100k", "50k"):
        for loss in (0, 1, 5):
            for browser in ("chromium", "webkit"):
                rounds, k = plan(case, prof, browser)
                for t in TARGETS:
                    if browser == "webkit" and t.endswith("-h3"):
                        continue
                    for r in sorted(set(range(rounds)) - have[(case, f"{prof}/{loss}", browser, t)]):
                        groups[(prof, loss, case, browser, r, k)].append(t)
with open(out, "w") as f:
    f.write(f"# step 8: the rounds {matrix} is missing, run again on their seeds; list {tag}\n")
    for (p, l, c, b, r, k), ts in sorted(groups.items()):
        f.write(f"g{tag}-{p}-{l}-{c}-{b}-r{r} --targets {','.join(ts)} --profiles {p} --losses {l} --cases {c} --first-round {r} --rounds 1 --browsers {b} --k {k}\n")
print(f"{sum(len(t) for t in groups.values())} cell-rounds missing, {len(groups)} parts", file=sys.stderr)
