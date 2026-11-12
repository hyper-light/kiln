#!/usr/bin/env python3
"""AC-T2.2.5's sweep collected (bench/sim-sweep.sh, kiln's CP-T2.2 step 6): each pass's seed lines
(OUT/pass-<P>-*.log) as a TSV a seed a line, the totals, the seeds that broke, every process's
exit, and each seed's digest in pass A against pass B. Writes OUT/seeds-<P>.tsv and prints the
summary.
    bench/sim-collect.py OUT"""
import re, sys, glob, os
OUT = sys.argv[1]
FIELDS = ["publishes", "acknowledged", "lost", "kills", "planned", "await", "before", "during",
          "after", "leader", "leader_after", "other_ends", "kept", "random", "lost_all", "one_way",
          "both", "stalls", "stalls_all", "resent", "recognised", "checks", "reordering", "bursty",
          "twice", "dropped"]
LINE = re.compile(
    r"seed (\d+): (\d+) publishes, (\d+) acknowledged, (\d+) lost; kills (\d+) of (\d+) planned "
    r"\(await (\d+), before a cut (\d+), during (\d+), after (\d+); the leader (\d+), after its flush (\d+)\), "
    r"other ends (\d+); devices kept (\d+), random (\d+), lost (\d+); partitions (\d+) one way, (\d+) both; "
    r"stalls (\d+) of a disk, (\d+) of all; resent (\d+), recognised (\d+); views checked (\d+); "
    r"reordering (\d+), bursty (\d+); datagrams twice (\d+), lost (\d+); digest ([0-9a-f]{16}), (ok|BROKEN)")
passes = {}
for P in ("A", "B"):
    seeds, exits, other = {}, [], []
    for log in sorted(glob.glob(os.path.join(OUT, f"pass-{P}-*.log"))):
        text = open(log, errors="replace").read()
        for m in LINE.finditer(text):
            seed = int(m.group(1))
            if seed in seeds:
                other.append(f"seed {seed} twice in pass {P}")
            seeds[seed] = (list(map(int, m.groups()[1:27])), m.group(28), m.group(29))
        e = re.findall(r"^EXIT (\d+)$", text, re.M)
        exits.append((os.path.basename(log), e[-1] if e else "none"))
        other += [l for l in text.splitlines() if "DIFFERENT" in l or "run again" in l]
    if not seeds:
        continue
    passes[P] = seeds
    with open(os.path.join(OUT, f"seeds-{P}.tsv"), "w") as f:
        f.write("seed\t" + "\t".join(FIELDS) + "\tdigest\tverdict\n")
        for s in sorted(seeds):
            v, d, ok = seeds[s]
            f.write(f"{s}\t" + "\t".join(map(str, v)) + f"\t{d}\t{ok}\n")
    tot = [sum(seeds[s][0][i] for s in seeds) for i in range(len(FIELDS))]
    broken = sorted(s for s in seeds if seeds[s][2] != "ok")
    missing = sorted(set(range(1, 10001)) - set(seeds))
    print(f"== pass {P}: {len(seeds)} seeds, {len(missing)} missing{(' ' + str(missing[:20])) if missing else ''}")
    print("   " + ", ".join(f"{k} {v}" for k, v in zip(FIELDS, tot)))
    print(f"   broken {len(broken)}: {broken[:50]}")
    print(f"   exits: {exits}")
    for l in other[:20]:
        print("   " + l[:300])
if "A" in passes and "B" in passes:
    A, B = passes["A"], passes["B"]
    both = sorted(set(A) & set(B))
    differ = [s for s in both if A[s][1] != B[s][1]]
    print(f"== digests: {len(both)} seeds in both passes, {len(both) - len(differ)} alike, {len(differ)} apart: {differ[:50]}")
