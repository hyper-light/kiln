#!/usr/bin/env python3
"""The bus's bounds from the lab (goal 11, plan 01a1a87b2e56 step 3): [bus] memory and [bus] topic_bytes.

    python3 -I bus-summary.py <run dir> [<run dir> ...]      (each a kiln-lab run's --out: calm, hogs, ...)
    TOPICS: the topics [bus] memory is reckoned for, 134 by default (the sessions production held on 2026-11-05,
    every one watched; kiln's crate::bus::TOPICS_HELD).

Reads every round's <round>.hook/bus.tsv (bus-hook.sh's samples, one a second) and bus.marks, and says for
each round and over all of them:
  - the bus's peak bytes (kiln_bus_memory_bytes, framing included) in the fill and in the window, and its limit;
  - the most one topic asked the bus to keep (kiln_bus_topic_bytes_peak: a live feed's newest 2048 frames, their
    keys and payloads as topic_bytes counts them, whether or not topic_bytes let it);
  - the frames published, what the bus refused and the lag notices it sent over the round (their counters'
    rise): a refusal or a lag notice means a bound was reached, so the peaks are floors, not the need;
  - the process's peak resident memory (anon);
  - [bus] memory's bound for the lab's own topics: the largest peak over every round, calm and with hogs, with
    room for one more of Quiver's largest messages with their framing (1,083,392 bytes: half docs/bus.md's floor
    of two), so the busiest moment seen could have taken one more; never below the floor (2,166,784 bytes);
    rounded up to a whole MiB.
  - [bus] memory's default: the most the bus held for each topic it held at any second (its bytes over its
    topics), for TOPICS topics, with room for that message, never below the floor, rounded up to a MiB: the
    lab's few topics aren't a large server's many. Both are proposed only when no round refused, sent lag
    notices, or came within that message of its limit.
  - [bus] topic_bytes' bound: the largest topic over every round, with room for one more of the largest
    message a topic takes (1,048,576 bytes of payload; a feed's frames have no key), so the busiest topic seen
    could have taken one more; never below that message (Quiver refuses a message larger than topic_bytes);
    rounded up to a whole MiB. A topic's figure isn't held to topic_bytes, so it's the need even where the
    topic asked for more than topic_bytes (said: its viewers fell behind after fewer than 2048 frames).
"""
import glob
import os
import sys

LARGEST = 1_083_392
FLOOR = 2 * LARGEST
PAYLOAD_MAX = 1 << 20
TOPIC_BYTES = 5 << 20
MIB = 1 << 20
TOPICS = int(os.environ.get("TOPICS", "134"))


def samples(path):
    out = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if not parts:
                continue
            try:
                at = int(parts[0])
            except ValueError:
                continue
            kv = {}
            for p in parts[1:]:
                k, _, v = p.partition("=")
                try:
                    kv[k] = float(v)
                except ValueError:
                    pass
            out.append((at, kv))
    return out


def marks(path):
    m = {}
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                parts = line.split()
                if len(parts) == 2:
                    m[parts[1]] = int(parts[0])
    return m


def peak(rows, key, start=None, end=None):
    vals = [kv[key] for at, kv in rows if key in kv and (start is None or at >= start) and (end is None or at <= end)]
    return max(vals) if vals else None


def share(rows):
    """The most the bus held for each topic it held, at any sample: (bytes over topics, bytes, topics)."""
    best = (0.0, 0.0, 0.0)
    for _, kv in rows:
        topics, held = kv.get("kiln_bus_topics", 0.0), kv.get("kiln_bus_memory_bytes", 0.0)
        if topics > 0 and held / topics > best[0]:
            best = (held / topics, held, topics)
    return best


def rise(rows, key):
    vals = [kv[key] for _, kv in rows if key in kv]
    return (vals[-1] - vals[0]) if len(vals) >= 2 else 0.0


def up_to_mib(n):
    return -(-int(n) // MIB) * MIB


def main(runs):
    rounds = []
    for run in runs:
        for tsv in sorted(glob.glob(os.path.join(run, "**", "*.hook", "bus.tsv"), recursive=True)):
            hook = os.path.dirname(tsv)
            rows = samples(tsv)
            if not rows:
                continue
            m = marks(os.path.join(hook, "bus.marks"))
            rounds.append({
                "run": os.path.basename(os.path.normpath(run)),
                "round": os.path.basename(hook),
                "fill": peak(rows, "kiln_bus_memory_bytes", None, m.get("fill-end")),
                "window": peak(rows, "kiln_bus_memory_bytes", m.get("window-start"), m.get("window-end")),
                "peak": peak(rows, "kiln_bus_memory_bytes"),
                "limit": peak(rows, "kiln_bus_memory_limit_bytes"),
                "topic": peak(rows, "kiln_bus_topic_bytes_peak"),
                "published": rise(rows, "kiln_bus_published_total"),
                "refused": rise(rows, "kiln_bus_refused_total"),
                "lag": rise(rows, "kiln_bus_lag_notices_total"),
                "anon": peak(rows, "rss.anon"),
                "topics": peak(rows, "kiln_bus_topics"),
                "share": share(rows),
            })
    if not rounds:
        print("no bus.tsv under", " ".join(runs))
        return 1
    mb = lambda v: "-" if v is None else f"{v / 1e6:.2f}"
    print("| run | round | bus peak MB (fill) | bus peak MB (window) | limit MB | largest topic MB | frames | refused | lag notices | anon peak MB | topics |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for r in rounds:
        print(f"| {r['run']} | {r['round']} | {mb(r['fill'])} | {mb(r['window'])} | {mb(r['limit'])} | {mb(r['topic'])} | {r['published']:.0f} | {r['refused']:.0f} | {r['lag']:.0f} | {mb(r['anon'])} | {r['topics'] or 0:.0f} |")
    print()

    # [bus] memory.
    most = max((r["peak"] or 0) for r in rounds)
    near = [r for r in rounds if r["limit"] and r["peak"] is not None and r["peak"] + LARGEST > r["limit"]]
    reached = [r for r in rounds if r["refused"] > 0 or r["lag"] > 0]
    bound = up_to_mib(max(most + LARGEST, FLOOR))
    print(f"[bus] memory: the largest peak {most:.0f} bytes over {len(rounds)} rounds; the bound it gives {bound} bytes ({bound // MIB} MiB): the peak, one more of Quiver's largest messages, rounded up to a MiB")
    each = max(rounds, key=lambda r: r["share"][0])
    per, held, topics = each["share"]
    scaled = up_to_mib(max(int(per * TOPICS) + LARGEST, FLOOR))
    print(f"  for each topic held: at the most {per:.0f} bytes ({each['run']} {each['round']}: {held:.0f} bytes for {topics:.0f} topics); for {TOPICS} topics (TOPICS) the bound it gives {scaled} bytes ({scaled // MIB} MiB)")
    if reached or near:
        why = []
        if reached:
            why.append(f"{len(reached)} rounds refused frames or sent lag notices")
        if near:
            why.append(f"{len(near)} rounds came within one largest message of their limit")
        print(f"  NOT a measurement of the need: {', and '.join(why)}; run again with a larger [bus] memory (BUS_MEMORY)")
    else:
        print(f"  for the lab's own topics: [bus] memory = {bound}")
        print(f"  proposed, for {TOPICS} topics: [bus] memory = {scaled}")

    # [bus] topic_bytes.
    topics = [r["topic"] for r in rounds if r["topic"] is not None]
    if not topics:
        print("[bus] topic_bytes: no round said its largest topic (kiln_bus_topic_bytes_peak): a build from before it")
        return 0
    largest = max(topics)
    tbound = up_to_mib(max(largest + PAYLOAD_MAX, PAYLOAD_MAX))
    print(f"[bus] topic_bytes: the largest topic asked for {largest:.0f} bytes over {len(topics)} rounds; the bound it gives {tbound} bytes ({tbound // MIB} MiB): that, one more of the largest message, rounded up to a MiB")
    past = [r for r in rounds if r["topic"] is not None and r["topic"] > TOPIC_BYTES]
    if past:
        print(f"  {len(past)} rounds had a topic ask for more than today's topic_bytes ({TOPIC_BYTES} bytes): its viewers fell behind after fewer than 2048 frames")
    if reached:
        print("  (a round refused frames or sent lag notices: a topic's frames counted are what was published, so this stands, but its round is flagged above)")
    print(f"  proposed: [bus] topic_bytes = {tbound}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
