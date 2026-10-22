"""Prints run.sh's JSON lines (stdin) as a table: requests per second, the server's CPU time per
request, and latency."""
import json
import sys

for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    d = json.loads(line)
    if d.get("mode") == "idle":
        print(f'{d["server"]:16} idle {d["conns"]:>7} conns  {d.get("bytes_per_conn", 0):>8,.0f} B/conn  rss {d.get("rss_after_kib", 0):>9,} KiB')
        continue
    n = max(d["requests"], 1)
    secs = d["secs"]
    cpu_ns = (d["user_s"] + d["sys_s"]) * 1e9 / (n * (1 + 1 / secs))  # run.sh counts the warmup second too
    deep = f'depth {d["depth"]:2}' if d["mode"] != "h2" else f'streams {d["streams"]:3}'
    print(f'{d["server"]:16} {d["mode"]:5} {deep}  {d["rps"]:>12,.0f} req/s  cpu {cpu_ns:6.0f} ns/req (user {d["user_s"]:5.2f}s sys {d["sys_s"]:5.2f}s)  p50 {d["p50_us"]:>8} us  p99 {d["p99_us"]:>8} us  err {d["errors"]}')
