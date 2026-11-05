#!/usr/bin/env python3
"""Goal 11 (plan 01a1a87b2e56 step 1): a jemalloc heap profile's live bytes by owner, each stack given to the
first owner whose pattern one of its frames matches, deepest kiln frame first.
    owners.py PROFILE.heap SYMS.txt SEGMENTS.txt"""
import bisect, math, os, re, sys, collections

OWNERS = [
    ("queues made up front", r"async_channel::bounded|concurrent_queue::bounded|std::sync::mpmc::array|sync_channel"),
    ("sessions' copies (harness)", r"kiln::harness::copies|kiln::harness::registry|kiln::harness::packed|harness::registry::Entry"),
    ("bus (live feeds' frames)", r"quiver|focal_bus|kiln::bus::|kiln::harness::live"),
    ("conversation store", r"kiln::convo"),
    ("records store (turso)", r"kiln::records"),
    ("kiln.db (SQLite)", r"kiln::store::|rusqlite|libsqlite3|sqlite3"),
    ("observe", r"kiln::observe|kiln_obs"),
    ("UI assets", r"kiln::ui::"),
    ("runtime (hyper-rt)", r"hyper_rt::"),
    ("network (QUIC, net thread)", r"kiln::net::|hyper_quic|hyper_transport|hyper_tls"),
    ("HTTP", r"kiln_http|kiln::http::"),
    ("runners, CI, CD", r"kiln::ci::|kiln_ci|kiln::cd::|kiln_cd|kiln::runner|kiln::exec"),
    ("turso (other)", r"turso_core|turso_"),
]

def load_syms(path):
    addrs, names = [], []
    for line in open(path, errors="replace"):
        parts = line.rstrip("\n").split(" ", 2)
        if len(parts) < 3 or parts[1] not in "tTwW":
            continue
        try:
            addrs.append(int(parts[0], 16)); names.append(parts[2])
        except ValueError:
            pass
    return addrs, names

def load_segments(path):
    segs = []
    for line in open(path):
        p = line.split()
        if p and p[0] == "LOAD":
            segs.append((int(p[1], 16), int(p[2], 16), int(p[4], 16)))
    return segs

def parse(path):
    period, stacks, maps, cur, section = 524288, [], [], None, "stacks"
    for line in open(path, errors="replace"):
        line = line.rstrip("\n")
        if line.startswith("heap_v2/"):
            period = int(line.split("/")[1]); continue
        if line.startswith("MAPPED_LIBRARIES"):
            section = "maps"; continue
        if section == "maps":
            maps.append(line); continue
        if line.startswith("@"):
            cur = [int(x, 16) for x in line[1:].split()]; continue
        m = re.match(r"\s*t\*: (\d+): (\d+) \[(\d+): (\d+)\]", line)
        if m and cur is not None:
            stacks.append((cur, int(m.group(1)), int(m.group(2)))); cur = None
    return period, stacks, maps

def main():
    prof, syms, segs = sys.argv[1:4]
    addrs, names = load_syms(syms)
    seg = load_segments(segs)
    period, stacks, maps = parse(prof)
    mp = []
    for l in maps:
        p = l.split()
        if len(p) >= 6:
            s, e = (int(x, 16) for x in p[0].split("-"))
            mp.append((s, e, int(p[2], 16), p[5]))
    mp.sort()
    starts = [m[0] for m in mp]
    cache = {}
    def name(a):
        if a in cache:
            return cache[a]
        r = "?"
        i = bisect.bisect_right(starts, a) - 1
        if i >= 0 and mp[i][0] <= a < mp[i][1]:
            s, e, off, path = mp[i]
            if path.endswith("/kiln") or path.endswith("kiln (deleted)") or (os.environ.get("G11_BIN") and os.environ["G11_BIN"] in path):
                fo = a - 1 - s + off
                va = next((fo - o + v for (o, v, sz) in seg if o <= fo < o + sz), None)
                if va is not None:
                    j = bisect.bisect_right(addrs, va) - 1
                    r = names[j] if j >= 0 else "kiln?"
            else:
                r = path.rsplit("/", 1)[-1]
        cache[a] = r
        return r
    pats = [(o, re.compile(p)) for o, p in OWNERS]
    by = collections.defaultdict(float)
    sites = collections.defaultdict(lambda: collections.defaultdict(float))
    total = 0.0
    for st, c, b in stacks:
        if c == 0:
            continue
        ratio = 1 - math.exp(-(b / c) / period) if period > 0 else 1
        scaled = b / ratio if ratio > 0 else b
        total += scaled
        frames = [name(a) for a in st]
        owner = "other"
        for f in frames:
            hit = next((o for o, p in pats if p.search(f)), None)
            if hit:
                owner = hit
                break
        by[owner] += scaled
        own = next((f for f in frames if f.startswith("<kiln") or f.startswith("kiln::")), frames[0] if frames else "?")
        sites[owner][re.sub(r"::h[0-9a-f]{16}$", "", own)[:140]] += scaled
    if "--total" in sys.argv:
        print(f"{total / 1e6:.1f}")
        return
    print(f"{prof}: live {total / 1e6:.1f} MB (unsampled estimate), sample period {period}")
    for o, v in sorted(by.items(), key=lambda kv: -kv[1]):
        print(f"{v / 1e6:9.1f} MB  {o}")
        for s, w in sorted(sites[o].items(), key=lambda kv: -kv[1])[:4]:
            print(f"           {w / 1e6:7.2f} MB  {s}")

main()
