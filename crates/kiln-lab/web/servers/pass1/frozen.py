#!/usr/bin/env python3
# Pass 1's jobs a memory freeze overlapped (plan 01a1a832e9cc step 8; guard-mem.sh's log). A part's
# unit frozen while a job ran stops its browsers and its server mid-measure, so the job is left out
# (`kiln-lab browser merge --drop-jobs`) and its round runs again on the same seeds under its own
# number (`--first-round`). A job spans from the end of the one before it in its run (or the run's
# start) to its own end (`started_us`, written as it ends).
#   frozen.py PARTS_OUT   writes step7/frozen-jobs.txt (every such job so far), and in PARTS_OUT
#                         a part for each round not yet asked for (from step7/frozen-asked.txt)
import collections, datetime, json, os, re, sys
G = os.path.expanduser("~/.cache/kiln-bp/g1"); O = G + "/lab"; A = O + "/step7"
now = datetime.datetime.now(datetime.timezone.utc).timestamp()
def ts(s): return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc).timestamp()
windows = collections.defaultdict(list); parts_of = collections.defaultdict(set); open_at = {}
for line in open(G + "/lab-freeze.log"):
    m = re.match(r"(\S+) (FREEZE|THAW) (\S+) \((.*?)\s*\):", line)
    if not m: continue
    t, kind, unit, parts = m.groups()
    parts_of[unit] |= set(parts.split())
    if kind == "FREEZE": open_at.setdefault(unit, ts(t))
    elif unit in open_at: windows[unit].append((open_at.pop(unit), ts(t)))
for u, t in open_at.items(): windows[u].append((t, now))
rows = []
for unit, ws in sorted(windows.items()):
    for part in sorted(parts_of[unit]):
        f = f"{O}/{part}/run.json"
        if not os.path.exists(f):
            print("no run file:", part, file=sys.stderr); continue
        r = json.load(open(f)); prev = r["started_us"] / 1e6; n = 0
        for j in sorted(r["jobs"], key=lambda j: j["started_us"]):
            end = j["started_us"] / 1e6
            if any(a < end and b > prev for a, b in ws):
                rows.append((part, j)); n += 1
            prev = end
        print(f"{part}: {n} of {len(r['jobs'])} jobs, {len(ws)} freezes, {sum(b - a for a, b in ws) / 60:.0f} min", file=sys.stderr)
with open(f"{A}/frozen-jobs.txt", "w") as out:
    out.write("# jobs a memory freeze overlapped, by when each ended (started_us)\n")
    for part, j in rows:
        out.write(f"{j['started_us']} # {part} {j['condition']} {j['case']} {j['browser']} {j['target']} r{j['round']}\n")
asked_f = f"{A}/frozen-asked.txt"
asked = set(open(asked_f).read().split()) if os.path.exists(asked_f) else set()
groups = collections.defaultdict(set)
new = []
for part, j in rows:
    if str(j["started_us"]) in asked: continue
    new.append(str(j["started_us"]))
    groups[(j["profile"], j["loss"], j["case"], j["browser"], j["round"], j["k"])].add(j["target"])
tag = sys.argv[1].rsplit("-", 1)[-1]
with open(sys.argv[1], "w") as out:
    out.write(f"# step 8: rounds a memory freeze overlapped, run again on their seeds; pass 1's list {tag}\n")
    for (p, l, c, b, rnd, k), ts_ in sorted(groups.items()):
        out.write(f"f{tag}-{p}-{l}-{c}-{b}-r{rnd} --targets {','.join(sorted(ts_))} --profiles {p} --losses {l} --cases {c} --first-round {rnd} --rounds 1 --browsers {b} --k {k}\n")
open(asked_f, "a").write("".join(s + "\n" for s in new))
print(f"{len(rows)} jobs in all, {len(new)} new, {len(groups)} rounds to run", file=sys.stderr)
