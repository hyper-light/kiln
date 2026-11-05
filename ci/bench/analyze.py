#!/usr/bin/env python3
"""The sweep's verdicts (ci/bench/README.md, "Pass"): ratios with bootstrap confidence intervals
per (competitor, N, rho) at d = 1, and kiln alone at d = 2 and 4 ("no equivalent").

The unit resampled is the run (one seed's whole trace), not the pick: picks within a run are
correlated (the same machine moment, the same caches), runs are independent. For each statistic
(p50, p99, mean per decision) the ratio is competitor / kiln, both the mean over runs, and its 95%
interval the 2.5th and 97.5th percentiles of 10,000 resamples of each side's runs (Efron's
percentile bootstrap), seeded so the report is reproducible.

A configuration passes when, against the competitor's best algorithm (the one fastest on that
statistic, as measured, not as assumed), the interval's lower bound is at least 10 for p50 and
for p99, and kiln's quality is no worse than that algorithm's: oversubscription and refusals per
decision no higher, imbalance (cv of runs in flight per backend) no higher.

Usage: python3 -I ci/bench/analyze.py RAW_DIR [--out results/DATE.md] [--kiln place.critical]
"""

import argparse
import json
import os
import random
import statistics
import sys
from collections import defaultdict

RESAMPLES = 10_000
TIMES = ("p50_ns", "p99_ns", "mean_ns")


def load(raw):
    head, rows = None, []
    with open(os.path.join(raw, "kernels.jsonl")) as f:
        for line in f:
            j = json.loads(line)
            if "machine" in j:
                head = head or j
            elif "who" in j and j.get("exit", 0) == 0:
                rows.append(j)
            else:
                print(f"analyze: a failed run: {j.get('cmd')} {j.get('error', '')[:300]}", file=sys.stderr)
    return head, rows


def family(who):
    return who.split("-")[0]


def group(rows):
    g = defaultdict(list)
    for r in rows:
        g[(family(r["who"]), r["algo"], r["n"], r["rho"], r.get("d", 1))].append(r)
    return g


def mean(xs):
    return sum(xs) / len(xs) if xs else float("nan")


def boot_ratio(a, b, rng):
    """a / b of the means over runs, and its 95% percentile interval."""
    if not a or not b or mean(b) == 0:
        return float("nan"), float("nan"), float("nan")
    rs = []
    for _ in range(RESAMPLES):
        ma = mean([a[rng.randrange(len(a))] for _ in a])
        mb = mean([b[rng.randrange(len(b))] for _ in b])
        rs.append(ma / mb if mb else float("inf"))
    rs.sort()
    return mean(a) / mean(b), rs[int(0.025 * RESAMPLES)], rs[int(0.975 * RESAMPLES) - 1]


def boot_mean(a, rng):
    if not a:
        return float("nan"), float("nan"), float("nan")
    ms = sorted(mean([a[rng.randrange(len(a))] for _ in a]) for _ in range(RESAMPLES))
    return mean(a), ms[int(0.025 * RESAMPLES)], ms[int(0.975 * RESAMPLES) - 1]


def quality(runs):
    steps = sum(r["steps"] for r in runs) or 1
    return {"over": sum(r["over"] for r in runs) / steps, "refused": sum(r["refused"] for r in runs) / steps, "cv": mean([r["cv"] for r in runs]), "peak": mean([r["peak"] for r in runs])}


def no_worse(k, c):
    """kiln's quality against a competitor's: each measure no higher (the cv to the 3 decimals it is reported to)."""
    why = []
    if k["over"] > c["over"]:
        why.append(f"oversubscribed {k['over']:.4f}/decision vs {c['over']:.4f}")
    if k["refused"] > c["refused"]:
        why.append(f"refused {k['refused']:.4f}/decision vs {c['refused']:.4f}")
    if round(k["cv"], 3) > round(c["cv"], 3):
        why.append(f"cv {k['cv']:.3f} vs {c['cv']:.3f}")
    return why


def ns(x):
    if x != x:
        return "-"
    if x >= 10_000_000:
        return f"{x / 1e6:.2f} ms"
    return f"{x / 1000:.2f} µs" if x >= 10_000 else f"{x:.0f} ns"


def fmt_ci(t):
    v, lo, hi = t
    return f"{ns(v)} [{ns(lo)}, {ns(hi)}]"


def sig(x):
    return f"{x:.3g}"


def fmt_ratio(t):
    """The ratio to 3 significant digits; below 1, also how many times slower kiln is."""
    v, lo, hi = t
    if v != v:
        return "-"
    text = f"{sig(v)}× [{sig(lo)}, {sig(hi)}]"
    return text + (f" (kiln {sig(1 / v)}× slower)" if 0 < v < 1 else "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("raw")
    ap.add_argument("--out")
    ap.add_argument("--kiln", default="place.critical")
    args = ap.parse_args()
    head, rows = load(args.raw)
    g = group(rows)
    rng = random.Random(20261104)
    ns_ = sorted({k[2] for k in g})
    rhos = sorted({k[3] for k in g})
    comps = sorted({k[0] for k in g if k[0] != "kiln"})
    out = []
    w = out.append
    m = (head or {}).get("machine", {})
    w(f"# kiln's placement vs nginx, HAProxy and Envoy: selection kernels ({(head or {}).get('started', '?')})\n")
    w(f"Machine: {m.get('cpu_model')} ({m.get('host')}), kernel {m.get('kernel')}; pinned to CPU {m.get('pinned_cpu')} "
      f"(SMT siblings {m.get('pinned_siblings')}, P-cores {m.get('p_cores')}); governor {m.get('governor')}, EPP {m.get('epp')}, "
      f"turbo {'off' if m.get('no_turbo') == '1' else 'on'}, SMT {'on' if m.get('smt_active') == '1' else 'off'}; {m.get('gcc')}, {m.get('rustc')}.")
    loads = [r["before"]["load"].split()[0] for r in rows if r.get("before", {}).get("load")]
    if loads:
        lf = sorted(float(x) for x in loads)
        w(f"Load average over the runs (1 min): median {statistics.median(lf):.1f}, range {lf[0]:.1f} to {lf[-1]:.1f}, on {m.get('nproc')} CPUs.\n")
    w(f"Trace: C = {(head or {}).get('c', 8)} slots per backend, K = rho·N·C requests in flight, one random departure then one timed pick per step; "
      f"{(head or {}).get('reps', '?')} runs (seeds) per configuration, interleaved. kiln's time is `{args.kiln}`. "
      f"Ratio = competitor / kiln (above 1: kiln faster), 95% bootstrap interval over runs. Pass: lower bound ≥ 10 at p50 and p99 against the competitor's fastest algorithm, quality no worse.\n")

    verdicts = []
    for n in ns_:
        w(f"\n## N = {n}, d = 1\n")
        w("| competitor | rho | its fastest (p50) | its p50 | kiln p50 | p50 ratio | its fastest (p99) | p99 ratio | mean ratio | quality (kiln vs it) | verdict |")
        w("|---|---|---|---|---|---|---|---|---|---|---|")
        for rho in rhos:
            k = g.get(("kiln", args.kiln, n, rho, 1), [])
            if not k:
                continue
            kq = quality(k)
            for c in comps:
                algos = {a: g[(c, a, n, rho, 1)] for (cc, a, nn, rr, d) in g if cc == c and nn == n and rr == rho and d == 1}
                if not algos:
                    continue
                best = {t: min(algos, key=lambda a: mean([r[t] for r in algos[a]])) for t in TIMES}
                r50 = boot_ratio([r["p50_ns"] for r in algos[best["p50_ns"]]], [r["p50_ns"] for r in k], rng)
                r99 = boot_ratio([r["p99_ns"] for r in algos[best["p99_ns"]]], [r["p99_ns"] for r in k], rng)
                rmean = boot_ratio([r["mean_ns"] for r in algos[best["mean_ns"]]], [r["mean_ns"] for r in k], rng)
                cq = quality(algos[best["p50_ns"]])
                # One request in flight at a time (rho 0): no balance to compare.
                bad = no_worse(kq, cq) if rho > 0 else []
                speed = r50[1] >= 10 and r99[1] >= 10
                verdict = "PASS" if speed and not bad else "FAIL"
                verdicts.append((c, n, rho, verdict, r50, r99, bad))
                qtxt = "n/a (one request in flight)" if rho == 0 else (f"over {kq['over']:.3f} vs {cq['over']:.3f}; cv {kq['cv']:.3f} vs {cq['cv']:.3f}; refused {kq['refused']:.3f} vs {cq['refused']:.3f}"
                        + (f" — worse: {', '.join(bad)}" if bad else ""))
                w(f"| {c} | {rho} | {best['p50_ns']} | {fmt_ci(boot_mean([r['p50_ns'] for r in algos[best['p50_ns']]], rng))} | {fmt_ci(boot_mean([r['p50_ns'] for r in k], rng))} | "
                  f"{fmt_ratio(r50)} | {best['p99_ns']} | {fmt_ratio(r99)} | {fmt_ratio(rmean)} | {qtxt} | **{verdict}** |")

    w("\n## Every algorithm measured, d = 1 (p50 / p99 / decisions a second on one core; quality)\n")
    w("| who | algo | N | rho | p50 | p99 | per s | over/decision | cv | peak | runs |")
    w("|---|---|---|---|---|---|---|---|---|---|---|")
    for key in sorted(g, key=lambda k: (k[2], k[3], k[0], k[1])):
        who, algo, n, rho, d = key
        if d != 1:
            continue
        rs = g[key]
        q = quality(rs)
        w(f"| {who} | {algo} | {n} | {rho} | {fmt_ci(boot_mean([r['p50_ns'] for r in rs], rng))} | {fmt_ci(boot_mean([r['p99_ns'] for r in rs], rng))} | "
          f"{mean([r['per_s'] for r in rs]):,.0f} | {q['over']:.4f} | {q['cv']:.3f} | {q['peak']:.3f} | {len(rs)} |")

    kd = sorted(k for k in g if k[0] == "kiln" and k[4] != 1)
    if kd:
        w("\n## kiln at d = 2 and 4 (no equivalent in any competitor: they pick on one dimension)\n")
        w("| metric | N | rho | d | p50 | p99 | per s | over/decision | refused/decision | cv | messages/decision |")
        w("|---|---|---|---|---|---|---|---|---|---|---|")
        for key in kd:
            rs = g[key]
            q = quality(rs)
            w(f"| {key[1]} | {key[2]} | {key[3]} | {key[4]} | {fmt_ci(boot_mean([r['p50_ns'] for r in rs], rng))} | {fmt_ci(boot_mean([r['p99_ns'] for r in rs], rng))} | "
              f"{mean([r['per_s'] for r in rs]):,.0f} | {q['over']:.4f} | {q['refused']:.4f} | {q['cv']:.3f} | {mean([r.get('messages', 0) for r in rs]):.1f} |")

    passed = sum(1 for v in verdicts if v[3] == "PASS")
    w(f"\n## Verdict\n\n{passed} of {len(verdicts)} configurations pass (d = 1, against each competitor's fastest algorithm).\n")
    text = "\n".join(out) + "\n"
    if args.out:
        with open(args.out, "w") as f:
            f.write(text)
    sys.stdout.write(text)


if __name__ == "__main__":
    main()
