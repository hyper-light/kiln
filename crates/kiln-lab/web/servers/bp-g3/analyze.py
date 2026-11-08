#!/usr/bin/env python3
"""Goal 3 step 3's measures from a flood.sh run (plan 01a1a832eb2f711f9c6f51d88effe89d):

- the kernel's unsent bytes on the page's socket during the flood, every 10 ms sample (diag.txt), against
  the mark and the chunk in use then (the server's numbers a second before: sockets.jsonl): p50, p99,
  p99.9 and the most, and p99 over (mark + chunk), which the step holds to at most 1;
- the most each layer below the scheduler held (tungstenite, TLS; the server's own counters), against
  two chunks;
- the TLS records on the socket (records.json: the capture decrypted with the page's keys), against the
  chunk;
- and what shows the flood was one: the scheduler's queue over the run, the bytes sent.

    analyze.py OUT
"""
import bisect
import json
import sys


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, max(0, int(-(-p * len(xs) // 100)) - 1))]


def main():
    o = sys.argv[1]
    start, end = (int(l) for l in open(f"{o}/flood.times").read().split())
    polls = []
    for line in open(f"{o}/sockets.jsonl"):
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        ss = r["v"].get("sockets", []) if isinstance(r.get("v"), dict) else []
        if ss:
            polls.append((r["t_us"], ss[0]))
    times = [t for t, _ in polls]
    samples, intervals, last = [], [], None
    for line in open(f"{o}/diag.txt"):
        w = line.split()
        t, notsent = int(w[0]), int(w[3])
        if not start <= t <= end:
            continue
        if last is not None:
            intervals.append((t - last) / 1000)
        last = t
        i = bisect.bisect_right(times, t) - 1
        if i < 0:
            continue
        s = polls[i][1]
        samples.append((notsent, s["mark"], s["chunk"], int(w[4]) * int(w[5])))
    unsent = [u for u, _, _, _ in samples]
    ratio = [u / (m + c) for u, m, c, _ in samples if m + c > 0]
    end_s = json.load(open(f"{o}/sockets.end.json"))["sockets"][0]
    chunk = max(s["chunk"] for _, s in polls)
    records = json.load(open(f"{o}/records.json"))["connections"]
    ws = [r for r in records if r.get("upgraded")]
    queued = [sum(s["queued"]) for t, s in polls if start <= t <= end]
    out = {
        "flood_s": round((end - start) / 1e6, 1),
        "kernel_unsent": {
            "samples": len(unsent),
            "interval_ms_p50": pct(intervals, 50),
            "interval_ms_p99": pct(intervals, 99),
            "bytes": {"p50": pct(unsent, 50), "p99": pct(unsent, 99), "p99.9": pct(unsent, 99.9), "max": max(unsent) if unsent else None},
            "mark_in_use": sorted({m for _, m, _, _ in samples}),
            "chunk_in_use": sorted({c for _, _, c, _ in samples}),
            "over_mark_plus_chunk": {"p50": pct(ratio, 50), "p99": pct(ratio, 99), "p99.9": pct(ratio, 99.9), "max": max(ratio) if ratio else None},
            "bar": "p99 of unsent / (mark + chunk) <= 1",
            "met": bool(ratio) and pct(ratio, 99) <= 1,
        },
        "in_flight_bytes": {"p50": pct([f for *_, f in samples], 50), "p99": pct([f for *_, f in samples], 99)},
        "user_space": {
            "held_max_message": end_s["held_max"],
            "held_ws_max": end_s["held_ws_max"],
            "held_tls_max": end_s["held_tls_max"],
            "chunk": chunk,
            "held_most_chunks": round((max(end_s["held_max"], end_s["held_ws_max"]) + end_s["held_tls_max"]) / chunk, 3) if chunk else None,
            "bar": "(tungstenite's most + TLS's most) <= 2 chunks",
            "met": bool(chunk) and max(end_s["held_max"], end_s["held_ws_max"]) + end_s["held_tls_max"] <= 2 * chunk,
        },
        "tls_records": {
            "connections": len(ws),
            "ws_records": sum(r["ws_records"] for r in ws),
            "largest_plaintext": max((r["largest_plaintext"] for r in ws), default=None),
            "over_chunk": sum(r["over_chunk"] for r in ws),
            "undecrypted": sum(r["undecrypted"] for r in ws),
            "chunk": chunk,
            "bar": "no record's plaintext past the chunk",
            "met": bool(ws) and sum(r["ws_records"] for r in ws) > 0 and sum(r["over_chunk"] for r in ws) == 0 and sum(r["undecrypted"] for r in ws) == 0,
        },
        "flood": {
            "sent_bytes": end_s["sent"],
            "scheduler_queue_bytes": {"first": queued[0] if queued else None, "most": max(queued) if queued else None, "last": queued[-1] if queued else None},
            "window_end": end_s["window"],
            "congestion": end_s.get("congestion"),
            "retransmitted": end_s["retransmitted"],
        },
    }
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
