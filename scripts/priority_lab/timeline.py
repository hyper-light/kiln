#!/usr/bin/env python3
"""Goal 8's headroom lab (plan 01a1a59c2b29 step 5): what record.py noted, read back as a timeline
in bins of BIN seconds: the server's delay as headroom reads it (`/healthz/delay`: median and most),
its shards' delay (`kiln_shard_delay_seconds`, every probe of every shard: p99 and p99.9), headroom's
view of that target (calm, CUSUM level and threshold, alarm), kiln's CPU budget, how many of the
runner's groups were paced and the least share, the CPU its groups used and how much of the time they
were frozen; then every step hogs.sh took and every decision headroom said, with their times.

    timeline.py OUT [BIN] [FROM_S] [TO_S]
"""
import bisect
import json
import math
import os
import sys
import time

out = sys.argv[1]
binw = float(sys.argv[2]) if len(sys.argv) > 2 else 15.0
TARGET = "kiln server delay"


def lines(name):
    p = os.path.join(out, name)
    if not os.path.exists(p):
        return []
    rows = []
    for l in open(p):
        try:
            rows.append(json.loads(l))
        except ValueError:
            pass
    return rows


delay, shards, head, groups = lines("delay.jsonl"), lines("shards.jsonl"), lines("headroom.jsonl"), lines("groups.jsonl")
if not delay:
    sys.exit("nothing recorded")
t0 = delay[0]["t"]
lo = t0 + (float(sys.argv[3]) if len(sys.argv) > 3 else 0.0)
hi = t0 + float(sys.argv[4]) if len(sys.argv) > 4 else max(r["t"] for r in delay)


def q(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(math.ceil(p * len(xs))) - 1)]


def hist_q(a, b, p):
    """The p-quantile of the probes between two scrapes, interpolated within its bucket."""
    def le(k):
        return math.inf if k == "+Inf" else float(k)
    keys = sorted(set(a["buckets"]) | set(b["buckets"]), key=le)
    cum = [(le(k), b["buckets"].get(k, 0.0) - a["buckets"].get(k, 0.0)) for k in keys]
    total = cum[-1][1] if cum else 0.0
    if total <= 0:
        return None
    want = p * total
    prev_le, prev_c = 0.0, 0.0
    for upper, c in cum:
        if c >= want:
            if math.isinf(upper):
                return prev_le
            return prev_le + (upper - prev_le) * ((want - prev_c) / max(c - prev_c, 1e-12))
        prev_le, prev_c = upper, c
    return prev_le


def ms(v, nd=1):
    return "-" if v is None else f"{v * 1000:.{nd}f}"


def fmt(v, nd=2):
    return "-" if v is None else f"{v:.{nd}f}"


# The driver's steps and headroom's decisions, with their times.
steps = []
if os.path.exists(os.path.join(out, "driver.log")):
    day = None
    for l in open(os.path.join(out, "driver.log")):
        hhmmss, _, what = l.strip().partition(" ")
        steps.append((hhmmss, what))
decisions, last = [], None
for r in head:
    ev = (r.get("status") or {}).get("events") or []
    if ev and ev != last:
        decisions.append((r["t"], ev))
    last = ev

print(f"t 0 s is {time.strftime('%H:%M:%S', time.gmtime(t0))} UTC\n")
# How late the server's ready work ran on average between two of the recorder's reads, as headroom
# reads it (the server's totals), or a hold under way where no probe ran at all.
means = []
for a, b in zip(delay, delay[1:]):
    if not all(k in a and k in b and a[k] is not None and b[k] is not None for k in ("late", "probes")):
        continue
    if b["probes"] > a["probes"] and b["late"] >= a["late"]:
        means.append((b["t"], (b["late"] - a["late"]) / (b["probes"] - a["probes"])))
    elif b["probes"] == a["probes"] and (b.get("held") or 0.0) > 0.0:
        means.append((b["t"], b["held"]))
print(f"| t s | delay med ms | delay most ms | mean since read med ms | mean since read most ms | shards p99 ms | shards p99.9 ms | calm ms | CUSUM | threshold | alarm | CPU budget | paced | least share | hogs' cores | frozen |")
print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
ts = [r["t"] for r in shards]
gts = [r["t"] for r in groups]
hts = [r["t"] for r in head]
t = lo
while t < hi:
    e = t + binw
    d = [r["d"] for r in delay if t <= r["t"] < e and r.get("d") is not None]
    m = [v for (at, v) in means if t <= at < e]
    i, j = bisect.bisect_left(ts, t), bisect.bisect_left(ts, e) - 1
    sp99 = sp999 = None
    if 0 <= i < j < len(shards) and "buckets" in shards[i] and "buckets" in shards[j]:
        sp99, sp999 = hist_q(shards[i], shards[j], 0.99), hist_q(shards[i], shards[j], 0.999)
    k = bisect.bisect_left(hts, e) - 1
    st = (head[k].get("status") or {}) if 0 <= k < len(head) else {}
    tg = next((x for x in st.get("targets") or [] if x.get("name") == TARGET), {})
    paced = [g for g in st.get("groups") or [] if g.get("share", 100) < 100]
    gi, gj = bisect.bisect_left(gts, t), bisect.bisect_left(gts, e) - 1
    cores = frozen = None
    if 0 <= gi < gj < len(groups):
        a, b = groups[gi].get("groups") or {}, groups[gj].get("groups") or {}
        used = sum((b[n]["usage_us"] or 0) - (a[n]["usage_us"] or 0) for n in b if n in a and b[n].get("usage_us") is not None and a[n].get("usage_us") is not None)
        cores = used / 1e6 / max(groups[gj]["t"] - groups[gi]["t"], 1e-9)
        samples = [g for r in groups[gi:gj + 1] for g in (r.get("groups") or {}).values() if g.get("procs")]
        frozen = sum(1 for g in samples if g.get("frozen")) / len(samples) if samples else None
    alarm = tg.get("alarm")
    print(
        f"| {t - t0:.0f} | {ms(q(d, 0.5), 2)} | {ms(max(d) if d else None)} | {ms(q(m, 0.5), 2)} | {ms(max(m) if m else None, 2)} | {ms(sp99, 2)} | {ms(sp999)} | {fmt((tg.get('calm_us') or 0) / 1e3 if tg.get('calm_us') else None, 3)} | {fmt(tg.get('level'))} | {fmt(tg.get('threshold'))} | {'harm' if alarm and alarm.get('harm') else ('yes' if alarm else '')} | {fmt(st.get('cpu_budget'), 1)} | {len(paced)} | {min((g['share'] for g in paced), default='')} | {fmt(cores, 1)} | {fmt(frozen)} |"
    )
    t = e

print("\nSteps:")
for hhmmss, what in steps:
    print(f"- {hhmmss} {what}")
print("\nheadroom's decisions:")
for at, ev in decisions:
    if lo <= at <= hi:
        print(f"- {at - t0:.1f} s: " + "; ".join(ev))
