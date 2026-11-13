"""The percentile report, from the raw records alone: per host, its facts and revision, then
density, start latency, control round trip and stream throughput as n, p50, p90, p99 and max, the
chaos it took, and every witness's count. Nothing is computed that a reader can't recompute from
records.jsonl with this file.
"""

from __future__ import annotations

from typing import Dict, List, Optional

from . import stats

MIB = float(1 << 20)


def _fmt(s: Optional[Dict[str, float]], unit: str, scale: float) -> str:
    if s is None:
        return "n=0"
    vals = " ".join(f"{k}={s[k] / scale:.3f}" for k in ("p50", "p90", "p99", "max"))
    note = " (p99 is the max: under 100 samples)" if s.get("p99_is_max") else ""
    return f"n={s['n']} {vals} {unit}{note}"


def by_host(records: List[dict]) -> Dict[str, List[dict]]:
    out: Dict[str, List[dict]] = {}
    for r in records:
        out.setdefault(str(r.get("host", "?")), []).append(r)
    return out


def host_summary(recs: List[dict]) -> Dict[str, object]:
    kinds: Dict[str, List[dict]] = {}
    for r in recs:
        kinds.setdefault(r["kind"], []).append(r)
    host = (kinds.get("host") or [{}])[0]
    hold_scans = [r for r in kinds.get("scan", []) if str(r.get("why", "")).startswith("after ")]
    starts = kinds.get("started", [])
    rtt = [r for r in kinds.get("rtt", []) if not r.get("floor")]
    rtt_floor = [r for r in kinds.get("rtt", []) if r.get("floor")]
    bulk = [r for r in kinds.get("bulk", []) if not r.get("floor")]
    bulk_floor = [r for r in kinds.get("bulk", []) if r.get("floor")]
    witnesses: Dict[str, int] = {}
    for w in kinds.get("witness", []):
        witnesses[w["witness"]] = witnesses.get(w["witness"], 0) + 1
    for e in kinds.get("ended_unasked", []):
        witnesses["ended_unasked"] = witnesses.get("ended_unasked", 0) + 1
    verdicts = kinds.get("verdict", [])
    after = [v for v in verdicts if v.get("at") == "after teardown"]
    left = sum(len(v.get("left") or []) for v in after)
    stray = sum(len(v.get("stray") or []) for v in verdicts if v.get("at") == "before teardown")
    first_chaos = min((c["t"] for c in kinds.get("chaos", []) if "t" in c), default=None)
    chaos: Dict[str, Dict[str, object]] = {}
    for c in kinds.get("chaos", []):
        a = chaos.setdefault(str(c.get("action")), {"n": 0, "kills": {}, "settle_ns": []})
        a["n"] = int(a["n"]) + 1
        k = str(c.get("kill", c.get("skipped", "?")))
        a["kills"][k] = a["kills"].get(k, 0) + 1
        if c.get("settle_ns") is not None:
            a["settle_ns"].append(c["settle_ns"])
        for key in ("run_after_client", "survived", "ended", "left_listed", "gone", "settled"):
            if key in c:
                a.setdefault(key, []).append(c[key])
    return {
        "host": host,
        "limit": next((r.get("limit") for r in reversed(kinds.get("scan", [])) if r.get("limit") is not None), None),
        "cost": next((r.get("cost") for r in reversed(kinds.get("scan", [])) if r.get("cost")), None),
        "density_vms": stats.summary(r["vms"] for r in hold_scans),
        "density_mem": stats.summary(m for r in hold_scans for m in r.get("per_vm_mem", [])),
        "start": stats.summary(r["start_ns"] for r in starts),
        "start_after_chaos": stats.summary(r["start_ns"] for r in starts if first_chaos is not None and r.get("t", 0) > first_chaos),
        "rtt": stats.summary(r["ns"] for r in rtt),
        "rtt_floor": stats.summary(r["ns"] for r in rtt_floor),
        "bulk_mib_s": stats.summary(r["bytes"] / r["ns"] * 1e9 / MIB for r in bulk if r.get("ns")),
        "bulk_floor_mib_s": stats.summary(r["bytes"] / r["ns"] * 1e9 / MIB for r in bulk_floor if r.get("ns")),
        "bulk_bytes": next((r.get("bytes") for r in kinds.get("bulk_size", [])), None),
        "asks": len(kinds.get("ask", [])),
        "start_failed": len(kinds.get("start_failed", [])),
        "ran_out": _count(kinds.get("ran_out", []), "wait"),
        "failed": [f"{r.get('what')}: {r.get('why')}" for r in kinds.get("failed", [])],
        "witnesses": witnesses,
        "stray": stray,
        "left": left,
        "chaos": chaos,
        "unparsed": len(kinds.get("unparsed", [])),
        "shards_version": next((r.get("version") for r in kinds.get("daemon", []) if r.get("version")), None),
    }


def _count(recs: List[dict], key: str) -> Dict[str, int]:
    out: Dict[str, int] = {}
    for r in recs:
        out[str(r.get(key))] = out.get(str(r.get(key)), 0) + 1
    return out


def verdict(s: Dict[str, object]) -> str:
    bad = []
    if s["witnesses"]:
        bad.append(f"witnesses {s['witnesses']}")
    if s["left"]:
        bad.append(f"{s['left']} processes left after teardown")
    if s["stray"]:
        bad.append(f"{s['stray']} VM or network processes before teardown")
    if s["failed"]:
        bad.append(f"{len(s['failed'])} failures")
    return "PASS" if not bad else "FAIL: " + "; ".join(bad)


def render(records: List[dict], cross: Optional[List[dict]] = None) -> str:
    lines: List[str] = []
    for name, recs in sorted(by_host(records).items()):
        s = host_summary(recs)
        h = s["host"]
        f = h.get("facts", {}) if isinstance(h, dict) else {}
        lines.append(f"== host {name}: {f.get('hostname')} {f.get('os')} {f.get('os_release')} {f.get('os_version')} {f.get('arch')}, {f.get('cpus')} CPUs ({f.get('cpu')}), {_gib(f.get('mem_total'))} memory")
        lines.append(f"   kiln {f.get('kiln_rev')}{' (dirty)' if f.get('kiln_dirty') else ''}; shards {s['shards_version']} sha256 {h.get('shards_sha256')} ({h.get('shards')}); python {f.get('python')}; load at start {f.get('load')}")
        lines.append(f"   driver {h.get('driver')}; image {h.get('image')}; ballast {h.get('ballast')} B; room {h.get('room')} (needs {h.get('needs')}); seed {h.get('seed')}; chaos {h.get('chaos')} x{h.get('rounds')}")
        lines.append(f"   density: limit {s['limit']} VMs (cost {s['cost']}); VMs at once {_fmt(s['density_vms'], 'VMs', 1)}")
        lines.append(f"   memory per VM at density: {_fmt(s['density_mem'], 'MiB', MIB)}")
        lines.append(f"   start (ask to first echo): {_fmt(s['start'], 'ms', 1e6)}; after chaos {_fmt(s['start_after_chaos'], 'ms', 1e6)}")
        lines.append(f"   control round trip: {_fmt(s['rtt'], 'us', 1e3)}; floor (local cat, same loop) {_fmt(s['rtt_floor'], 'us', 1e3)}")
        lines.append(f"   stream throughput at {s['bulk_bytes']} B: {_fmt(s['bulk_mib_s'], 'MiB/s', 1)}; floor {_fmt(s['bulk_floor_mib_s'], 'MiB/s', 1)}")
        lines.append(f"   asks {s['asks']}, start failures {s['start_failed']}, waits run out {s['ran_out'] or 0}")
        for action, a in sorted(s["chaos"].items()):
            settle = stats.summary(a.get("settle_ns", []))
            extra = {k: v for k, v in a.items() if k not in ("n", "kills", "settle_ns")}
            lines.append(f"   chaos {action}: n={a['n']} kills {a['kills']} settle {_fmt(settle, 'ms', 1e6)} {extra}")
        lines.append(f"   witnesses: {s['witnesses'] or 'none'}; stray before teardown {s['stray']}; left after teardown {s['left']}")
        for why in s["failed"][:10]:
            lines.append(f"   failed: {why}")
        if s["unparsed"]:
            lines.append(f"   unparsed lines: {s['unparsed']}")
        lines.append(f"   verdict: {verdict(s)}")
    if cross is not None:
        lines.append(f"== across hosts: doubled {len(cross)}" + (f" {cross[:5]}" if cross else ""))
    return "\n".join(lines) + "\n"


def _gib(b: object) -> str:
    try:
        return f"{int(b) / (1 << 30):.1f} GiB"
    except (TypeError, ValueError):
        return "? GiB"
