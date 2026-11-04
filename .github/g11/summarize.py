#!/usr/bin/env python3
"""Goal 11 (plan 01a1a87b2e56 step 1), trial only: a lab run's memory, summarized as text: the lab's own numbers
(run.json), the server's RSS over the run (rss.csv), its last smaps by owner of the mapping, glibc's malloc_info,
and the newest jemalloc heap profile by call site.   summarize.py OUT SYMS SEGMENTS"""
import glob, json, os, re, subprocess, sys

out, syms, segs = sys.argv[1], sys.argv[2], sys.argv[3]
here = os.path.dirname(os.path.abspath(__file__))

def p(*a):
    print(*a, flush=True)

try:
    run = json.load(open(os.path.join(out, "run.json")))
except Exception as e:
    run = None
    p(f"no run.json: {e}")
if run:
    keys = ["server.rss_mb", "fill.s", "attach.p50_ms", "attach.max_ms", "websocket.p50_ms", "websocket.p99_ms", "api_read.p50_ms",
            "api_read.p99_ms", "api_write.p99_ms", "page.p99_ms", "server.cpu_ms_per_request", "shards.delay_p99_ms"]
    for r in run.get("rounds", []):
        n = r.get("numbers", {})
        p(f"round {r.get('index')} {r.get('build')} hogs={r.get('hogs')} shards={r.get('shards')} host={r.get('machine', {}).get('host')} cpus={r.get('machine', {}).get('cpus')}")
        p("  " + "  ".join(f"{k}={n[k]:.4g}" for k in keys if k in n))

def kb(s):
    return int(s.split()[0])

def smaps_summary(path):
    maps, cur = [], None
    for line in open(path, errors="replace"):
        if re.match(r"^[0-9a-f]+-[0-9a-f]+ ", line):
            q = line.split()
            s, e = (int(x, 16) for x in q[0].split("-"))
            cur = {"s": s, "e": e, "perm": q[1], "name": q[5] if len(q) > 5 else "", "rss": 0, "anon": 0}
            maps.append(cur)
        elif cur is not None:
            if line.startswith("Rss:"):
                cur["rss"] = kb(line.split(":", 1)[1])
            elif line.startswith("Anonymous:"):
                cur["anon"] = kb(line.split(":", 1)[1])
    cats = {}
    def add(k, m):
        c = cats.setdefault(k, [0, 0])
        c[0] += m["rss"]; c[1] += 1
    arena = 64 << 20
    for i, m in enumerate(maps):
        name, perm, size = m["name"], m["perm"], m["e"] - m["s"]
        nxt = maps[i + 1] if i + 1 < len(maps) else None
        prv = maps[i - 1] if i else None
        if name.endswith("/kiln") or name.endswith("/kiln (deleted)"):
            add(f"kiln binary {perm}" + (" (relro/data, anon)" if m["anon"] else ""), m)
        elif name.startswith("/"):
            add("libraries and files", m)
        elif name in ("[heap]", "[stack]"):
            add(name, m)
        elif name == "" and perm == "rw-p":
            if m["s"] % arena == 0 and (size == arena or (nxt and nxt["perm"] == "---p" and nxt["s"] == m["e"] and nxt["e"] - m["s"] == arena)):
                add("glibc arena heaps (non-main)", m)
            elif prv and prv["perm"] == "---p" and prv["e"] == m["s"] and prv["e"] - prv["s"] <= (64 << 10):
                add("thread stacks (after a guard)", m)
            elif size >= (1 << 20):
                add("anon mappings >= 1 MiB (big allocations, pools)", m)
            else:
                add("anon mappings < 1 MiB", m)
        elif name.startswith("["):
            add(name, m)
        else:
            add(f"other {perm}", m)
    tot = sum(v[0] for v in cats.values())
    p(f"  smaps: Rss {tot / 1024:.1f} MB")
    for k, (rss, n) in sorted(cats.items(), key=lambda kv: -kv[1][0]):
        if rss >= 512:
            p(f"    {rss / 1024:8.1f} MB  x{n:<4} {k}")
    big = sorted((m for m in maps if m["name"] == "" and m["perm"] == "rw-p"), key=lambda m: -m["rss"])[:8]
    p("    largest anonymous mappings: " + ", ".join(f"{m['rss'] / 1024:.1f}/{(m['e'] - m['s']) >> 20}MiB" for m in big))

def malloc_summary(path):
    x = open(path, errors="replace").read()
    heaps = re.findall(r'<heap nr="(\d+)">(.*?)</heap>', x, re.S)
    free = cur = 0
    for _, h in heaps:
        free += sum(int(s) for s in re.findall(r'<total type="(?:fast|rest)" count="\d+" size="(\d+)"', h))
        cur += sum(int(s) for s in re.findall(r'<system type="current" size="(\d+)"', h))
    tail = x.rsplit("</heap>", 1)[-1]
    mm = sum(int(s) for s in re.findall(r'<total type="mmap" count="\d+" size="(\d+)"', tail))
    p(f"  glibc malloc_info: {len(heaps)} arenas, {cur / 1e6:.1f} MB from the system in arenas, {free / 1e6:.1f} MB of it free; {mm / 1e6:.1f} MB in mmap'd chunks")

for d in sorted(glob.glob(os.path.join(out, "mem", "*"))):
    p(f"\n== {os.path.basename(d)}: {open(os.path.join(d, 'env.txt')).read().strip() if os.path.exists(os.path.join(d, 'env.txt')) else ''}")
    try:
        rows = [l.strip().split(",") for l in open(os.path.join(d, "rss.csv"))][1:]
        rows = [r for r in rows if len(r) >= 6]
        if rows:
            mx = max(rows, key=lambda r: int(r[1]))
            last = rows[-1]
            p(f"  rss.csv: last VmRSS {int(last[1]) / 1024:.1f} MB (anon {int(last[2]) / 1024:.1f}, file {int(last[3]) / 1024:.1f}, shmem {int(last[4]) / 1024:.1f}), threads {last[5]}; most {int(mx[1]) / 1024:.1f} MB")
    except OSError as e:
        p(f"  no rss.csv: {e}")
    idx = os.path.join(d, "smaps.index")
    if os.path.exists(idx):
        lines = [l.split() for l in open(idx) if l.strip()]
        if lines:
            n = lines[-1][0]
            for cand in [n] + [l[0] for l in reversed(lines[:-1])]:
                f = os.path.join(d, f"smaps.{cand}")
                if os.path.exists(f) and os.path.getsize(f) > 0:
                    smaps_summary(f)
                    break
    xs = sorted(glob.glob(os.path.join(d, "malloc-*.xml")), key=lambda f: int(re.findall(r"(\d+)", os.path.basename(f))[0]))
    xs = [f for f in xs if os.path.getsize(f) > 0]
    if xs:
        malloc_summary(xs[-1])
    hs = glob.glob(os.path.join(d, "jeprof.*.heap"))
    if hs:
        def seq(f):
            m = re.search(r"jeprof\.(\d+)\.(\d+)\.", os.path.basename(f))
            return (os.path.getmtime(f), int(m.group(2)) if m else 0)
        newest = max(hs, key=seq)
        p(f"  jemalloc heap profile: {os.path.basename(newest)} (of {len(hs)})")
        r = subprocess.run([sys.executable, "-I", os.path.join(here, "heapprof.py"), newest, syms, segs, "--top", "40"], capture_output=True, text=True)
        p("\n".join("  " + l for l in r.stdout.splitlines()))
        if r.returncode:
            p("  heapprof failed: " + r.stderr[-500:])
