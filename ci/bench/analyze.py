#!/usr/bin/env python3
"""The load-balancer bench's verdicts (bench-spec.md, Output).

  analyze.py RUNS.jsonl [RUNS.jsonl ...] [--w W.json] [--out results/DATE.md]

Per configuration (N, rho) and implementation (impl, algo, d): the median over runs of ns per step
less the null measured in the same run (the deciding number), its 95% percentile bootstrap interval
(10,000 resamples of the runs, Efron and Tibshirani 1993; seeded, so the page is reproducible); the
release-only step the same way, and the pick as their difference; quality beside it.

Per competitor: its fastest algorithm of any kind (the person's bar, 2026-11-07: load-unaware ones
count), its fastest load-aware one, and its best-quality one (fewest oversubscribed picks, then fewest
refusals, then the lowest imbalance). The verdict, once kiln has runs: competitor / kiln with a
bootstrap interval (each side's runs resampled apart); a configuration passes when the interval's
lower bound is at least 10. Failures are printed beside passes. Without kiln's runs the page shows the
bar instead: a tenth of each competitor's fastest step.
"""

import argparse
import collections
import json
import os
import random
import statistics
import sys

RESAMPLES = 10_000
BAR = 10.0

# What the competitor study found in their code, carried with the numbers (~/.cache/kiln-lb/notes/competitors.md).
QUALITY_NOTES = {
    ("nginx", "hash"): "keeps 15 bits of crc32 ((crc32 >> 16) & 0x7fff), so 32,768 values: uneven at N = 10,000",
    ("nginx", "ip_hash"): "6,271 hash values over the address's first three octets: 37% of peers unreachable at N = 10,000",
    ("envoy", "maglev"): "default table 65,537 entries: about +-8% skew at N = 10,000",
    ("katran", "maglev"): "ring 65,537 entries (RING_SIZE): about +-8% skew at N = 10,000; no release path, no load",
    ("haproxy", "first"): "without maxconn every request goes to the first server",
    ("haproxy", "roundrobin"): "documented as limited by design to 4,095 active servers (N = 10,000 is past it; it still runs)",
}


def load(paths):
    rows, errors = [], []
    for p in paths:
        with open(p) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                j = json.loads(line)
                (errors if "error" in j else rows).append(j)
    return rows, errors


def base(algo):
    return algo.split("/")[0]


def net_step(j):
    return j["ns_step"] if j["impl"] == "null" else j["ns_step"] - j.get("ns_null", 0.0)


def release_valid(j):
    """A release-only measurement means something only if its top-up held: an admission variant's top-up past
    N x C is mostly refused, so its "releases" are mostly of empty slots; and kiln's bench prints null when it
    has none."""
    if j.get("ns_release") is None:
        return False
    if "/max" in j.get("algo", "") and j.get("f", 0) + j.get("m_r", 3) * j.get("b", 0) > j.get("n", 0) * 8:
        return False
    return True


def net_release(j):
    if not release_valid(j):
        return float("nan")
    return j["ns_release"] if j["impl"] == "null" else j["ns_release"] - (j.get("ns_null_release") or 0.0)


def boot_median(xs, rng):
    n = len(xs)
    if n == 0:
        return float("nan"), float("nan"), float("nan")
    ms = sorted(statistics.median([xs[rng.randrange(n)] for _ in range(n)]) for _ in range(RESAMPLES))
    return statistics.median(xs), ms[int(0.025 * RESAMPLES)], ms[int(0.975 * RESAMPLES) - 1]


def boot_ratio(a, b, rng):
    """median(a) / median(b), each side's runs resampled apart; the 95% percentile interval."""
    if not a or not b:
        return float("nan"), float("nan"), float("nan")
    rs = []
    for _ in range(RESAMPLES):
        ma = statistics.median([a[rng.randrange(len(a))] for _ in a])
        mb = statistics.median([b[rng.randrange(len(b))] for _ in b])
        rs.append(ma / mb if mb > 0 else float("inf"))
    rs.sort()
    mb = statistics.median(b)
    return (statistics.median(a) / mb if mb > 0 else float("inf")), rs[int(0.025 * RESAMPLES)], rs[int(0.975 * RESAMPLES) - 1]


def quality(rs):
    picks = sum(j.get("picks", 0) for j in rs) or 1
    return {"oversub": sum(j.get("oversub", 0) for j in rs) / picks, "refusals": sum(j.get("refusals", 0) for j in rs) / picks,
            "cv": statistics.mean([j.get("cv", 0.0) for j in rs])}


def qkey(q):
    return (round(q["oversub"], 6), round(q["refusals"], 6), round(q["cv"], 4))


def ns(x):
    if x != x:
        return "-"
    if x >= 100_000:
        return "%.1f us" % (x / 1000)
    if x >= 10_000:
        return "%.2f us" % (x / 1000)
    if x >= 100:
        return "%.0f ns" % x
    return "%.1f ns" % x


def ci(t):
    m, lo, hi = t
    return "%s [%s, %s]" % (ns(m), ns(lo), ns(hi))


def ratio(t):
    m, lo, hi = t
    return "-" if m != m else "%.2fx [%.2f, %.2f]" % (m, lo, hi)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("runs", nargs="+")
    ap.add_argument("--w", help="the W file run.py warmup wrote")
    ap.add_argument("--out")
    a = ap.parse_args()
    rows, errors = load(a.runs)
    rng = random.Random(20261107)
    g = collections.defaultdict(list)
    for j in rows:
        g[(j["n"], round(j["rho"], 2), j["impl"], j["algo"], j.get("d", 1))].append(j)
    configs = sorted({(k[0], k[1]) for k in g})
    out = []
    w = out.append

    machines = sorted({j.get("machine", "?") for j in rows})
    loads = sorted(x for j in rows for x in (j.get("load_before"), j.get("load_after")) if isinstance(x, (int, float)))
    w("# kiln's balancer against nginx, HAProxy, Envoy and Katran: the bench (bench-spec.md)\n")
    w("Machine: %s." % "; ".join(machines))
    if loads:
        w("Load average (1 min) before and after the runs: median %.1f, range %.1f to %.1f." % (statistics.median(loads), loads[0], loads[-1]))
    commits = sorted({(j["impl"], j.get("commit", "")) for j in rows if j["impl"] != "null"})
    w("Commits: %s. Bench harness: %s.\n" % (", ".join("%s %s" % (i, c[:12]) for i, c in commits),
                                             ", ".join(sorted({j.get("bench_commit", "")[:12] for j in rows})) or "?"))
    w("Numbers are ns per step (release + pick) less the null measured in the same run, the median over runs with its 95% "
      "bootstrap interval; release is the release-only step less its null; pick = step - release. Quality: oversubscribed "
      "and refused picks per pick, and the mean CV of in-flight counts sampled every N steps. The whole step decides; the "
      "release-only column is informational: the competitors' runs measure it on their own state topped up by 3 x B picks "
      "(so at small F' far above the step's load), \"-\" where an admission variant's top-up passed its capacity.\n")
    if a.w and os.path.exists(a.w):
        wj = json.load(open(a.w))
        w("W (warm-up steps, MSER-5 on the imbalance at rho %s, the largest over the implementations): %s.\n" % (
            ", ".join(sorted({str(v.get("rho")) for k, v in wj.items() if not k.startswith("_")})),
            ", ".join("N = %s: %s" % (k, v["w"]) for k, v in sorted(wj.items(), key=lambda kv: int(kv[0]) if kv[0].isdigit() else 0) if not k.startswith("_"))))

    verdicts = []
    summary = []
    for n, rho in configs:
        keys = sorted(k for k in g if k[0] == n and k[1] == rho)
        bs = sorted({j.get("b") for k in keys for j in g[k]})
        nulls = [j.get("ns_null", 0.0) for k in keys for j in g[k] if j["impl"] != "null"]
        w("\n## N = %d, rho = %.1f\n" % (n, rho))
        w("B = %s; the null (subtracted): median %s per step over %d runs.\n" % (", ".join(map(str, bs)), ns(statistics.median(nulls)) if nulls else "-", len(nulls)))
        w("| impl | algo | load | runs | step | release | pick | oversub/pick | refused/pick | cv | note |")
        w("|---|---|---|---|---|---|---|---|---|---|---|")
        stats = {}
        for k in keys:
            rs = g[k]
            st = boot_median([net_step(j) for j in rs], rng)
            rl = boot_median([x for x in (net_release(j) for j in rs) if x == x], rng)
            q = quality(rs)
            stats[k] = (st, rl, q, rs)
            note = QUALITY_NOTES.get((k[2], base(k[3])), "")
            prec = "" if st[0] > 0 and st[1] >= 0.95 * st[0] and st[2] <= 1.05 * st[0] else " (interval wider than 5%)"
            w("| %s | %s | %s | %d | %s%s | %s | %s | %.4f | %.4f | %.3f | %s |" % (
                k[2], k[3] + ("" if k[4] == 1 else " d=%d" % k[4]), rs[0].get("label", "?"), len(rs), ci(st), prec, ns(rl[0]),
                ns(st[0] - rl[0]), q["oversub"], q["refusals"], q["cv"], note))

        comps = sorted({k[2] for k in keys if k[2] not in ("null", "kiln")})
        kiln = {k: v for k, v in stats.items() if k[2] == "kiln" and k[4] == 1}
        w("\n| competitor | fastest (any) | its step | fastest load-aware | its step | best quality | its step | bar (fastest / 10) |")
        w("|---|---|---|---|---|---|---|---|")
        picks = {}
        for c in comps:
            ks = [k for k in stats if k[2] == c]
            fastest = min(ks, key=lambda k: stats[k][0][0])
            aware = [k for k in ks if stats[k][3][0].get("label") == "aware"]
            fastest_aware = min(aware, key=lambda k: stats[k][0][0]) if aware else None
            best_q = min(ks, key=lambda k: (qkey(stats[k][2]), stats[k][0][0]))
            picks[c] = (fastest, fastest_aware, best_q)
            w("| %s | %s | %s | %s | %s | %s | %s | %s |" % (
                c, fastest[3], ns(stats[fastest][0][0]), fastest_aware[3] if fastest_aware else "-",
                ns(stats[fastest_aware][0][0]) if fastest_aware else "-", best_q[3], ns(stats[best_q][0][0]), ns(stats[fastest][0][0] / BAR)))
        allk = [k for k in stats if k[2] not in ("null", "kiln")]
        f_any = min(allk, key=lambda k: stats[k][0][0]) if allk else None
        awk = [k for k in allk if stats[k][3][0].get("label") == "aware"]
        f_aw = min(awk, key=lambda k: stats[k][0][0]) if awk else None
        k1 = min(kiln, key=lambda k: stats[k][0][0]) if kiln else None
        srow = [n, rho, f_any, f_aw, k1]
        if k1 is not None:
            kx = [net_step(j) for j in stats[k1][3]]
            srow.append(boot_ratio([net_step(j) for j in stats[f_any][3]], kx, rng) if f_any else None)
            srow.append(boot_ratio([net_step(j) for j in stats[f_aw][3]], kx, rng) if f_aw else None)
        summary.append((srow, {k: stats[k] for k in (f_any, f_aw, k1) if k is not None}))
        if not kiln:
            continue
        w("\n| kiln | vs | competitor's algorithm | ratio (competitor / kiln) | quality: kiln vs it (oversub, refused, cv) | verdict |")
        w("|---|---|---|---|---|---|")
        for kk, kv in sorted(kiln.items()):
            kx = [net_step(j) for j in kv[3]]
            for c in comps:
                for what, k in zip(("fastest", "fastest load-aware", "best quality"), picks[c]):
                    if k is None:
                        continue
                    r = boot_ratio([net_step(j) for j in stats[k][3]], kx, rng)
                    kq, cq = kv[2], stats[k][2]
                    worse = [m for m in ("oversub", "refusals", "cv") if round(kq[m], 4) > round(cq[m], 4)]
                    verdict = "PASS" if r[1] >= BAR and not worse else "FAIL"
                    verdicts.append((n, rho, kk[3], c, what, verdict))
                    w("| %s | %s %s | %s | %s | %.4f, %.4f, %.3f vs %.4f, %.4f, %.3f%s | **%s** |" % (
                        kk[3], c, what, k[3], ratio(r), kq["oversub"], kq["refusals"], kq["cv"], cq["oversub"], cq["refusals"], cq["cv"],
                        (" (worse: %s)" % ", ".join(worse)) if worse else "", verdict))

    if summary:
        w("\n## Summary: every configuration\n")
        w("| N | rho | fastest competitor (any) | its step | fastest load-aware | its step | bar | kiln (d = 1, fastest) | its step | vs fastest | vs load-aware |")
        w("|---|---|---|---|---|---|---|---|---|---|---|")
        for srow, st in summary:
            n, rho, fa, fw, k1 = srow[:5]
            def name(k):
                return "-" if k is None else "%s %s" % (k[2], k[3])
            def step(k):
                return "-" if k is None else ns(st[k][0][0])
            r1 = ratio(srow[5]) if len(srow) > 5 and srow[5] else "-"
            r2 = ratio(srow[6]) if len(srow) > 6 and srow[6] else "-"
            pf = lambda r: "" if r == "-" else (" **PASS**" if srow[5 if r is r1 else 6][1] >= BAR else " **FAIL**")
            w("| %d | %.1f | %s | %s | %s | %s | %s | %s | %s | %s%s | %s%s |" % (
                n, rho, name(fa), step(fa), name(fw), step(fw), ns(st[fa][0][0] / BAR) if fa else "-", name(k1), step(k1),
                r1, pf(r1), r2, pf(r2)))
    if verdicts:
        w("\n## Verdict\n")
        for what in ("fastest", "fastest load-aware", "best quality"):
            vs = [v for v in verdicts if v[4] == what]
            w("- against each competitor's %s algorithm: %d of %d configurations pass." % (what, sum(v[5] == "PASS" for v in vs), len(vs)))
    else:
        w("\n## Verdict\n\nkiln has no runs here yet: the bar per configuration is the last column above.")
    if errors:
        w("\n## Runs that failed\n")
        for e in errors[:50]:
            w("- %s %s N=%s rho=%s: %s" % (e.get("impl"), e.get("algo"), e.get("n"), e.get("rho"), (e.get("error") or "").strip().splitlines()[-1:] or ""))
    text = "\n".join(out) + "\n"
    if a.out:
        with open(a.out, "w") as f:
            f.write(text)
    sys.stdout.write(text)


if __name__ == "__main__":
    main()
