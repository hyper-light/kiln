#!/usr/bin/env python3
# Step 8's test (plan 01a1a832e9cc): five cells of the merged matrix, drawn after the run on a fixed
# seed, measured again in a session of their own (a unit started after the matrix ended), each with
# the matrix's rounds (the same round numbers, so the same netem seeds) and K, then each against
# the matrix's own (`kiln-lab browser compare MATRIX T T --b-run RERUN`, Bonferroni over the cell's
# nine numbers).
#   rerun5.py parts MATRIX.json SEED PARTS_OUT   the draw, as a run-pass-mem.sh list
#   rerun5.py compare MATRIX.json PARTS KILN_LAB OUT_DIR   each rerun against the matrix
import collections, json, random, subprocess, sys

def cells(path):
    jobs = json.load(open(path))["jobs"]
    by = collections.defaultdict(list)
    for j in jobs:
        by[(j["case"], j["condition"], j["browser"], j["target"])].append(j)
    return by

if sys.argv[1] == "parts":
    _, _, matrix, seed, out = sys.argv
    by = cells(matrix)
    keys = sorted(by)
    drawn = random.Random(int(seed)).sample(keys, 5)
    with open(out, "w") as f:
        f.write(f"# step 8's test: five cells of {matrix} drawn with random.Random({seed}).sample over its {len(keys)} cells\n")
        for case, cond, browser, target in drawn:
            js = by[(case, cond, browser, target)]
            rounds = sorted({j["round"] for j in js}); k = js[0]["k"]
            prof, loss = cond.split("/")
            f.write(f"r5-{case}-{prof}-{loss}-{browser}-{target} --targets {target} --profiles {prof} --losses {loss} --cases {case} "
                    f"--first-round {rounds[0]} --rounds {rounds[-1] - rounds[0] + 1} --browsers {browser} --k {k}\n")
    print(open(out).read())
else:
    _, _, matrix, parts, lab, outdir = sys.argv
    for line in open(parts):
        if line.startswith("#") or not line.strip():
            continue
        name = line.split()[0]; target = line.split("--targets ")[1].split()[0]
        r = subprocess.run([lab, "browser", "compare", matrix, target, target, "--b-run", f"{outdir}/{name}/run.json"], capture_output=True, text=True)
        print(f"{name}: {(r.stdout + r.stderr).strip()}")
