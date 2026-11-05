#!/usr/bin/env python3
"""Step 7's lab (docs/priority.md, "Step 7's lab"; goal 8, plan 01a1a59c2b29 step 7): how busy work that isn't the
round's server kept the server's CPUs while the round ran, and what the server says of its place on its machine.

On a kernel with lazy preemption, anything busy in user space on a CPU a shard wakes on holds it up to a tick
(docs/priority.md, "The kernel's preemption"), so a round's tail on cores of the server's own depends on what else
ran there: other sessions' work, which no setting of kiln's keeps out (only a cpuset partition does).

    foreign.py sample <cpus> <lab out> <out.jsonl> <stop file>
        once a second until the stop file is there: the server's CPUs' busy time (/proc/stat, all but idle and
        iowait) and the round's server's own CPU time (its /proc/<pid>/stat), the server found by its config,
        <lab out>/rounds/*/kiln.toml; once it has answered for 15 s, its /healthz/server, to <lab out>/healthz.json.
    foreign.py window <out.jsonl>
        the run as JSON: the share of the server's CPUs' time that wasn't the server's, each second (mean, p50,
        p90, max), and the server's own share.
"""
import glob
import json
import os
import statistics
import sys
import time
import urllib.request

TICK = os.sysconf("SC_CLK_TCK")


def cpus_of(text):
    out = []
    for part in text.split(","):
        part = part.strip()
        if not part:
            continue
        a, _, b = part.partition("-")
        out.extend(range(int(a), int(b or a) + 1))
    return out


def busy(cpus):
    """Ticks the CPUs `cpus` spent on anything but idle and iowait (guest time is in user already)."""
    total = 0
    with open("/proc/stat") as f:
        for line in f:
            name, *vals = line.split()
            if name.startswith("cpu") and name[3:].isdigit() and int(name[3:]) in cpus:
                v = [int(x) for x in vals[:8]]
                total += sum(v) - v[3] - v[4]
    return total


def server(lab_out):
    """The round's server: its pid and port, from the config the lab wrote for it."""
    for config in glob.glob(os.path.join(lab_out, "rounds", "*", "kiln.toml")):
        needle = b"--config\0" + config.encode() + b"\0server"
        port = None
        with open(config) as f:
            for line in f:
                if line.startswith("http_addr"):
                    port = line.split(":")[-1].strip().strip('"')
        for d in os.listdir("/proc"):
            if not d.isdigit():
                continue
            try:
                with open(f"/proc/{d}/cmdline", "rb") as f:
                    if needle in f.read():
                        return int(d), port
            except OSError:
                continue
    return None, None


def ticks_of(pid):
    """The process's own user and system ticks, every thread's."""
    with open(f"/proc/{pid}/stat") as f:
        after = f.read().rsplit(")", 1)[1].split()
    return int(after[11]) + int(after[12])


def sample(cpus_text, lab_out, out_path, stop):
    cpus = set(cpus_of(cpus_text))
    pid, port, found_at, noted = None, None, None, False
    with open(out_path, "a") as out:
        while not os.path.exists(stop):
            if pid is None:
                pid, port = server(lab_out)
                found_at = time.monotonic() if pid else None
            mine = None
            if pid:
                try:
                    mine = ticks_of(pid)
                except (OSError, IndexError, ValueError):
                    pid = None
            out.write(json.dumps({"t": time.monotonic(), "busy": busy(cpus), "server": mine, "cpus": len(cpus)}) + "\n")
            out.flush()
            if pid and port and not noted and time.monotonic() - found_at >= 15:
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz/server", timeout=10) as r:
                        health = json.loads(r.read())
                    with open(os.path.join(lab_out, "healthz.json"), "w") as f:
                        json.dump(health, f, indent=1)
                    noted = True
                except (OSError, ValueError):
                    pass
            time.sleep(1)


def window(path):
    rows = [json.loads(line) for line in open(path) if line.strip()]
    foreign, own = [], []
    for a, b in zip(rows, rows[1:]):
        if a["server"] is None or b["server"] is None:
            continue
        room = (b["t"] - a["t"]) * b["cpus"] * TICK
        if room <= 0:
            continue
        theirs = (b["busy"] - a["busy"]) - (b["server"] - a["server"])
        foreign.append(min(max(theirs / room, 0.0), 1.0))
        own.append(min(max((b["server"] - a["server"]) / room, 0.0), 1.0))
    if not foreign:
        print(json.dumps({"seconds": 0}))
        return
    q = statistics.quantiles(foreign, n=10) if len(foreign) > 1 else [foreign[0]] * 9
    print(json.dumps({
        "seconds": len(foreign),
        "foreign_share": {"mean": statistics.fmean(foreign), "p50": statistics.median(foreign), "p90": q[8], "max": max(foreign)},
        "server_share": {"mean": statistics.fmean(own), "max": max(own)},
    }))


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "sample":
        sample(*sys.argv[2:])
    elif len(sys.argv) == 3 and sys.argv[1] == "window":
        window(sys.argv[2])
    else:
        sys.exit(__doc__)
