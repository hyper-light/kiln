"""Prints run.sh's JSON lines (stdin) as a table: requests per second, the server's CPU time per
request, and latency. With --medians, the median of each server and workload over the rounds, as a
Markdown table, with each kiln-http figure's ratio to hyper on tokio's."""
import json
import statistics
import sys


def key(d):
    m = d["mode"]
    if m in ("idle", "idle-h2"):
        return (m, d["conns"], 0)
    return (m, d["conns"], d["depth"] if m != "h2" else d["streams"])


def cpu_ns(d):
    n = max(d["requests"], 1)
    secs = d["secs"]
    # run.sh counts the warmup second too.
    return (d["user_s"] + d["sys_s"]) * 1e9 / (n * (1 + 1 / secs))


def label(k):
    m, conns, x = k
    return {
        "h1": f"HTTP/1.1, {conns} connections, " + ("one request at a time" if x == 1 else f"pipelined {x} deep"),
        "h2": f"HTTP/2, {conns} connections x {x} streams",
        "churn": f"HTTP/1.1, a new connection per request ({conns} at once)",
        "idle": f"{conns} idle HTTP/1.1 connections",
        "idle-h2": f"{conns} idle HTTP/2 connections",
    }[m]


rows = [json.loads(l) for l in sys.stdin if l.strip().startswith("{")]
if "--medians" not in sys.argv:
    for d in rows:
        if d["mode"] in ("idle", "idle-h2"):
            print(f'{d["server"]:16} idle {d["conns"]:>7} conns  {d.get("bytes_per_conn", 0):>8,.0f} B/conn  rss {d.get("rss_after_kib", 0):>9,} KiB')
            continue
        deep = f'depth {d["depth"]:2}' if d["mode"] != "h2" else f'streams {d["streams"]:3}'
        print(f'{d["server"]:16} {d["mode"]:5} {deep}  {d["rps"]:>12,.0f} req/s  cpu {cpu_ns(d):6.0f} ns/req (user {d["user_s"]:5.2f}s sys {d["sys_s"]:5.2f}s)  p50 {d["p50_us"]:>8} us  p99 {d["p99_us"]:>8} us  err {d["errors"]}')
    sys.exit(0)

groups = {}
for d in rows:
    groups.setdefault(key(d), {}).setdefault(d["server"], []).append(d)
med = statistics.median
for k in sorted(groups, key=lambda k: (["h1", "h2", "churn", "idle", "idle-h2"].index(k[0]), k[1], k[2])):
    print(f"\n**{label(k)}**\n")
    servers = groups[k]
    base = servers.get("hyper-tokio")
    if k[0] in ("idle", "idle-h2"):
        print("| server | bytes per connection | resident (all connections) |")
        print("|---|---:|---:|")
        for s, ds in servers.items():
            b = med(x["bytes_per_conn"] for x in ds)
            rss = med(x["rss_after_kib"] for x in ds) / 1024
            ratio = f" ({med(x['bytes_per_conn'] for x in base) / b:.1f}x less)" if base and s == "kiln-http" and b else ""
            print(f"| {s} | {b:,.0f} B{ratio} | {rss:,.0f} MiB |")
        continue
    print("| server | requests/s | server CPU per request | p50 | p99 | errors |")
    print("|---|---:|---:|---:|---:|---:|")
    for s, ds in servers.items():
        rps = med(x["rps"] for x in ds)
        cpu = med(cpu_ns(x) for x in ds)
        p50 = med(x["p50_us"] for x in ds)
        p99 = med(x["p99_us"] for x in ds)
        err = sum(x["errors"] for x in ds)
        r = c = ""
        if base and s == "kiln-http":
            r = f" ({rps / med(x['rps'] for x in base):.1f}x)"
            c = f" ({med(cpu_ns(x) for x in base) / cpu:.1f}x less)"
        print(f"| {s} | {rps:,.0f}{r} | {cpu:,.0f} ns{c} | {p50:,.0f} us | {p99:,.0f} us | {err} |")
