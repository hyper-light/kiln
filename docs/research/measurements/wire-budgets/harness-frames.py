#!/usr/bin/env python3
"""The `harness` frames harness-frames.sh wrote: their decoded sizes, what each top-level field of the entry takes, and which
fields changed between one agent's consecutive frames (what made each frame). Usage: harness-frames.py <frames file>"""
import collections, json, sys
frames, wires = [], []
for line in open(sys.argv[1], encoding="utf-8"):
    line = line.rstrip("\n")
    if line.startswith("#wire "):
        wires.append(int(line.split()[1]))
    elif line:
        frames.append(json.loads(line))
print(f"{len(frames)} harness frames, decoded mean {sum(len(json.dumps(f, separators=(',', ':'))) for f in frames) / max(len(frames), 1):.0f} B, wire mean {sum(wires) / max(len(wires), 1):.0f} B")
def entry(f):
    for k in ("entry", "data", "info", "session", "agent"):
        if isinstance(f.get(k), dict):
            return k, f[k]
    return None, f
k0, e0 = entry(frames[0]) if frames else (None, {})
print("frame keys:", sorted(frames[0].keys()) if frames else [], "| entry under:", k0)
sizes = collections.defaultdict(list)
for f in frames:
    _, e = entry(f)
    for k, v in e.items():
        sizes[k].append(len(json.dumps(v, separators=(",", ":"))) + len(k) + 4)
print("field                      mean B  max B  present")
for k, v in sorted(sizes.items(), key=lambda kv: -sum(kv[1]) / len(kv[1])):
    print(f"  {k:<24} {sum(v) / len(v):>6.0f} {max(v):>6} {len(v):>6}")
by_agent = collections.defaultdict(list)
for f in frames:
    _, e = entry(f)
    by_agent[e.get("id") or e.get("session") or f.get("id")].append(e)
changed = collections.Counter()
alone = collections.Counter()
for a, es in by_agent.items():
    for prev, cur in zip(es, es[1:]):
        diff = sorted(k for k in set(prev) | set(cur) if prev.get(k) != cur.get(k))
        for k in diff:
            changed[k] += 1
        alone[tuple(diff)] += 1
print(f"agents {len(by_agent)}; frames per agent {[len(v) for v in by_agent.values()]}")
print("fields changed between an agent's consecutive frames:", changed.most_common(20))
print("the sets of changed fields, most common:")
for d, n in alone.most_common(12):
    print(f"  {n:>4}  {list(d)}")
