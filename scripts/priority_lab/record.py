#!/usr/bin/env python3
"""Goal 8's headroom lab (plan 01a1a59c2b29 step 5, docs/priority.md "headroom listens to the
server"): what the lab's server and the runner beside it say over time, as JSON lines in OUT:

- delay.jsonl: the server's delay (`/healthz/delay`), four times a second: its worst shard's now,
  and, where the build says them, its probes' lateness added up and counted (from two reads, how
  late its ready work ran on average between them: what headroom reads);
- shards.jsonl: its shards' delay histogram (`/metrics`' `kiln_shard_delay_seconds`, every shard's
  buckets added up), every two seconds;
- headroom.jsonl: the runner's headroom status as the server has it (`/api/runners/<key>/headroom`:
  its targets, budgets, groups' shares and why), every second;
- groups.jsonl: each of the runner's groups (its cgroups under SCOPE: `kiln-agents-*/*`,
  `kiln-jobs-*/*`), its freezer and the CPU it has used, every second.

    record.py PORT ADMIN_TOKEN_FILE RUNNER_KEY SCOPE_CGROUP OUT

It runs until OUT/STOP exists.
"""
import glob
import json
import os
import sys
import threading
import time
import urllib.request

port, admin_file, key, scope, out = sys.argv[1:6]
admin = open(admin_file).read().strip()
base = f"http://127.0.0.1:{port}"
stop = threading.Event()


def get(path, auth=True, timeout=5.0):
    req = urllib.request.Request(base + path)
    if auth:
        req.add_header("Authorization", f"Bearer {admin}")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode()


def every(name, period, take):
    def loop():
        with open(os.path.join(out, name), "a") as f:
            nxt = time.time()
            while not stop.is_set():
                t = time.time()
                try:
                    v = take()
                except Exception as e:  # a slow or refused answer is a record too
                    v = {"error": str(e)[:200]}
                f.write(json.dumps({"t": round(t, 3), **v}) + "\n")
                f.flush()
                nxt += period
                time.sleep(max(0.0, nxt - time.time()))

    th = threading.Thread(target=loop, daemon=True)
    th.start()
    return th


def delay():
    t0 = time.time()
    body = get("/healthz/delay", auth=False).strip()
    took = round(time.time() - t0, 6)
    v = json.loads(body)
    if isinstance(v, dict):
        # The server's figures: its worst shard now, and its probes' lateness added up and counted.
        return {"d": v.get("delay_s"), "late": v.get("late_s"), "probes": v.get("probes"), "held": v.get("held_s"), "took": took}
    return {"d": float(v), "took": took}


def shards():
    buckets, total, count = {}, 0.0, 0.0
    for line in get("/metrics").splitlines():
        if not line.startswith("kiln_shard_delay_seconds"):
            continue
        name, _, value = line.rpartition(" ")
        if name.startswith("kiln_shard_delay_seconds_bucket"):
            le = name.split('le="', 1)[1].split('"', 1)[0]
            buckets[le] = buckets.get(le, 0.0) + float(value)
        elif name.startswith("kiln_shard_delay_seconds_sum"):
            total += float(value)
        elif name.startswith("kiln_shard_delay_seconds_count"):
            count += float(value)
    return {"buckets": buckets, "sum": total, "count": count}


def headroom():
    v = json.loads(get(f"/api/runners/{key}/headroom"))
    return {"status": v.get("status"), "applied": v.get("applied"), "enabled": (v.get("settings") or {}).get("enabled")}


def read(path):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return ""


def groups():
    out_groups = {}
    for d in sorted(glob.glob(os.path.join(scope, "kiln-agents-*", "*")) + glob.glob(os.path.join(scope, "kiln-jobs-*", "*"))):
        if not os.path.isdir(d):
            continue
        usage = next((int(l.split()[1]) for l in read(os.path.join(d, "cpu.stat")).splitlines() if l.startswith("usage_usec ")), None)
        events = read(os.path.join(d, "cgroup.events"))
        out_groups[os.path.relpath(d, scope)] = {
            "freeze": read(os.path.join(d, "cgroup.freeze")).strip(),
            "frozen": "frozen 1" in events,
            "usage_us": usage,
            "procs": len(read(os.path.join(d, "cgroup.procs")).split()),
        }
    return {"groups": out_groups}


threads = [every("delay.jsonl", 0.25, delay), every("shards.jsonl", 2.0, shards), every("headroom.jsonl", 1.0, headroom), every("groups.jsonl", 1.0, groups)]
while not os.path.exists(os.path.join(out, "STOP")):
    time.sleep(1)
stop.set()
for th in threads:
    th.join(timeout=10)
