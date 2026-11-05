#!/usr/bin/env python3
"""Goal 11 (plan 01a1a87b2e56 step 1): a jemalloc heap profile (heap_v2) of the lab's server, its live bytes by
call site, named from the build's symbols (nm -n -C of the unstripped binary) and its PT_LOAD segments.
    heapprof.py PROFILE.heap SYMS.txt SEGMENTS.txt [--top N] [--owners]
SYMS: `nm -n -C --defined-only kiln.sym` output. SEGMENTS: `readelf -lW kiln.sym` output."""
import bisect, math, re, sys
from collections import defaultdict

def load_syms(path):
    addrs, names = [], []
    with open(path, errors="replace") as f:
        for line in f:
            parts = line.rstrip("\n").split(" ", 2)
            if len(parts) < 3 or parts[1] not in "tTwW":
                continue
            try:
                a = int(parts[0], 16)
            except ValueError:
                continue
            addrs.append(a); names.append(parts[2])
    return addrs, names

def load_segments(path):
    segs = []
    with open(path) as f:
        for line in f:
            p = line.split()
            if p and p[0] == "LOAD":
                off, vaddr, filesz = int(p[1], 16), int(p[2], 16), int(p[4], 16)
                segs.append((off, vaddr, filesz))
    return segs

def parse(path):
    period, stacks, maps, cur = 524288, [], [], None
    section = "stacks"
    with open(path, errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("heap_v2/"):
                period = int(line.split("/")[1]); continue
            if line.startswith("MAPPED_LIBRARIES"):
                section = "maps"; continue
            if section == "maps":
                maps.append(line); continue
            if line.startswith("@"):
                cur = [int(x, 16) for x in line[1:].split()]
                continue
            m = re.match(r"\s*t\*: (\d+): (\d+) \[(\d+): (\d+)\]", line)
            if m and cur is not None:
                stacks.append((cur, int(m.group(1)), int(m.group(2))))
                cur = None
    return period, stacks, maps

def mapping(maps):
    out = []
    for l in maps:
        p = l.split()
        if len(p) < 6:
            continue
        s, e = (int(x, 16) for x in p[0].split("-"))
        out.append((s, e, int(p[2], 16), p[5], p[1]))
    out.sort()
    return out

def main():
    args = sys.argv[1:]
    top = 40
    if "--top" in args:
        i = args.index("--top"); top = int(args[i + 1]); del args[i:i + 2]
    prof, syms, segs = args[0], args[1], args[2]
    addrs, names = load_syms(syms)
    seg = load_segments(segs)
    period, stacks, maps = parse(prof)
    mp = mapping(maps)
    starts = [m[0] for m in mp]
    cache = {}
    def name(a):
        if a in cache:
            return cache[a]
        r = "?"
        i = bisect.bisect_right(starts, a) - 1
        if i >= 0 and mp[i][0] <= a < mp[i][1]:
            s, e, off, path, perm = mp[i]
            if path.endswith("/kiln") or "/kiln." in path or path.endswith("kiln (deleted)"):
                fo = a - 1 - s + off
                va = None
                for (o, v, sz) in seg:
                    if o <= fo < o + sz:
                        va = fo - o + v; break
                if va is not None:
                    j = bisect.bisect_right(addrs, va) - 1
                    r = names[j] if j >= 0 else "kiln?"
                else:
                    r = "kiln?"
            else:
                r = path.rsplit("/", 1)[-1]
        cache[a] = r
        return r
    def short(n):
        # Generic arguments go (those after a name); a qualified type's path stays: `<a::B as c::D>::f`.
        for _ in range(6):
            m = re.sub(r"(?<=[A-Za-z0-9_])<[^<>]*>", "", n)
            if m == n:
                break
            n = m
        n = re.sub(r"::h[0-9a-f]{16}$", "", n)
        return n[:160]
    total = 0.0
    rows = []
    for st, c, b in stacks:
        if c == 0:
            continue
        avg = b / c
        ratio = 1 - math.exp(-avg / period) if period > 0 else 1
        scaled = b / ratio if ratio > 0 else b
        total += scaled
        frames = [short(name(a)) for a in st]
        rows.append((scaled, c / ratio if ratio > 0 else c, frames))
    print(f"profile {prof}: sample period {period}, {len(rows)} stacks, live {total/1e6:.1f} MB (unsampled estimate)")
    generic = re.compile(r"^(je_|_rjem|malloc|calloc|realloc|__rust|__rdl|alloc::|<alloc::|core::|<core::|std::|<std::|hashbrown::|<hashbrown::|"
                         r"serde_json::|<serde_json::|serde::|<serde::|bytes::|<bytes::|smallvec::|<&|<T as|\?|libjemalloc|libc\.so|ld-linux|"
                         r"<serde_json|<Vec|<Box|<Arc|<String|Vec<|Box<|Arc<|String|prof_|imalloc|ialloc|arena_|tcache_|<\[|\[)")
    by_site = defaultdict(float)
    by_stack3 = defaultdict(float)
    for scaled, cnt, frames in rows:
        own = [f for f in frames if not generic.match(f)]
        site = own[0] if own else (frames[0] if frames else "?")
        by_site[site] += scaled
        by_stack3[" <- ".join(own[:4]) if own else "?"] += scaled
    if "--grep" in sys.argv:
        pat = re.compile(sys.argv[sys.argv.index("--grep") + 1])
        depth = 14
        agg = defaultdict(float)
        for scaled, cnt, frames in rows:
            if any(pat.search(f) for f in frames):
                own = [f for f in frames if not generic.match(f)]
                agg[" <- ".join(own[:depth])] += scaled
        print(f"\n## stacks matching {pat.pattern}, {depth} deep")
        for k, v in sorted(agg.items(), key=lambda kv: -kv[1])[:top]:
            print(f"{v/1e6:9.2f} MB  {k[:1500]}")
        return
    print(f"\n## by first own frame (top {top})")
    for k, v in sorted(by_site.items(), key=lambda kv: -kv[1])[:top]:
        print(f"{v/1e6:9.2f} MB  {k}")
    print(f"\n## by own frames, four deep (top {top})")
    for k, v in sorted(by_stack3.items(), key=lambda kv: -kv[1])[:top]:
        print(f"{v/1e6:9.2f} MB  {k[:400]}")

main()
