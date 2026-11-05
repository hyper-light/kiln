#!/usr/bin/env python3
"""The selection-kernel sweep (ci/bench/README.md): every competitor's kernels and kiln's, on one
pinned core, the same trace (ci/bench/drivers/common.h), the declared N, rho and d.

Runs are interleaved: each repetition (seed) runs every configuration once before the next seed,
so a change in the machine's load over the sweep falls on every competitor alike instead of on
whichever ran last. Each run's line is the driver's JSON plus the machine's state around it
(load averages, the pinned CPU's frequency); the machine itself (CPU, governor, SMT, turbo,
kernel, compilers) is recorded once at the top of the output.

Usage: python3 -I ci/bench/run.py [--cpu 14] [--reps 10] [--n 100,500,4095,10000]
                                  [--rho 0,0.5,0.9] [--only nginx,haproxy,envoy,kiln] [--out DIR]
"""

import argparse
import datetime
import json
import os
import platform
import subprocess
import sys

HOME = os.path.expanduser("~")
BIN = os.path.join(HOME, ".cache/kiln-lb/bench/bin")
HERE = os.path.dirname(os.path.abspath(__file__))

# Each competitor's algorithms, as its driver names them. Envoy's FULL_SCAN and HAProxy's and
# nginx's round robins are run too: "best" is decided from the measurements, not assumed.
COMPETITORS = {
    "nginx": ("nginx_pick", ["round_robin", "least_conn", "random_two_least_conn"]),
    "haproxy": ("haproxy_pick", ["roundrobin", "leastconn", "random"]),
    "envoy": ("envoy_pick", ["least_request", "least_request_full_scan", "round_robin", "random"]),
}
# The time one run aims at, so a slow kernel at large N takes as long as a fast one, not 1000x.
TARGET_NS = 2e9
MIN_STEPS, MAX_STEPS = 1000, 200_000


def read(path):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def sh(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return None


def machine(cpu):
    sib = read(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
    return {
        "host": platform.node(),
        "cpu_model": next((l.split(":", 1)[1].strip() for l in (read("/proc/cpuinfo") or "").splitlines() if l.startswith("model name")), None),
        "kernel": platform.release(),
        "pinned_cpu": cpu,
        "pinned_siblings": sib,
        "p_cores": read("/sys/devices/cpu_core/cpus"),
        "e_cores": read("/sys/devices/cpu_atom/cpus"),
        "governor": read(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor"),
        "epp": read(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/energy_performance_preference"),
        "max_khz": read(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq"),
        "smt_active": read("/sys/devices/system/cpu/smt/active"),
        "no_turbo": read("/sys/devices/system/cpu/intel_pstate/no_turbo"),
        "gcc": (sh(["gcc", "--version"]) or "").splitlines()[:1],
        "rustc": sh(["rustc", "--version"]),
        "nproc": os.cpu_count(),
    }


def state(cpu):
    return {"load": read("/proc/loadavg"), "cur_khz": read(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq")}


def kiln_bin():
    """kiln's bench binary, as cargo bench --no-run left it in the shared target dir."""
    d = os.path.join(HOME, ".cache/kiln-lb/target-shared/release/deps")
    try:
        cands = [os.path.join(d, f) for f in os.listdir(d) if f.startswith("lb_vs-") and os.access(os.path.join(d, f), os.X_OK) and "." not in f]
    except OSError:
        return None
    return max(cands, key=os.path.getmtime) if cands else None


def runs(args):
    """Every configuration: (who, binary, algo, n, rho, d, metric lines it prints)."""
    out = []
    for who in args.only:
        if who == "kiln":
            b = kiln_bin()
            if not b:
                print("run: kiln's lb_vs isn't built (ci/bench/README.md, Building)", file=sys.stderr)
                continue
            for d in args.d:
                out.append(("kiln", b, "place", d))
            continue
        exe, algos = COMPETITORS[who]
        b = os.path.join(BIN, exe)
        if not os.access(b, os.X_OK):
            print(f"run: {b} isn't built", file=sys.stderr)
            continue
        for a in algos:
            out.append((who, b, a, 1))
    return out


def once(cpu, b, algo, n, rho, d, steps, seed, c):
    cmd = ["taskset", "-c", str(cpu), b, "-a", algo, "-n", str(n), "-c", str(c), "-r", str(rho), "-s", str(steps), "-S", str(seed)]
    if d != 1:
        cmd += ["-d", str(d)]
    before = state(cpu)
    p = subprocess.run(cmd, capture_output=True, text=True)
    after = state(cpu)
    lines = []
    for l in p.stdout.splitlines():
        try:
            j = json.loads(l)
        except ValueError:
            continue
        j.update({"exit": p.returncode, "before": before, "after": after})
        lines.append(j)
    if p.returncode != 0 or not lines:
        lines.append({"error": p.stderr[-2000:], "exit": p.returncode, "cmd": cmd})
    return lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cpu", type=int, default=14)
    ap.add_argument("--reps", type=int, default=10)
    ap.add_argument("--n", default="100,500,4095,10000")
    ap.add_argument("--rho", default="0,0.5,0.9")
    ap.add_argument("--d", default="1,2,4")
    ap.add_argument("--c", type=int, default=8)
    ap.add_argument("--only", default="nginx,haproxy,envoy,kiln")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    args.only = args.only.split(",")
    args.d = [int(x) for x in args.d.split(",")]
    ns = [int(x) for x in args.n.split(",")]
    rhos = [float(x) for x in args.rho.split(",")]
    stamp = datetime.datetime.now().strftime("%Y-%m-%dT%H%M")
    out = args.out or os.path.join(HERE, "results", "raw", stamp)
    os.makedirs(out, exist_ok=True)
    path = os.path.join(out, "kernels.jsonl")
    configs = runs(args)
    with open(path, "a") as f:
        f.write(json.dumps({"machine": machine(args.cpu), "started": stamp, "reps": args.reps, "n": ns, "rho": rhos, "c": args.c}) + "\n")
        # Steps per configuration, from one short calibration run each.
        steps = {}
        for (who, b, algo, d) in configs:
            for n in ns:
                for rho in rhos:
                    # Calibrated idle (rho 0): no warm-up to pay, which at N = 10,000 and rho 0.9
                    # is 72,000 of kiln's decisions; a loaded pick costs about what an idle one does.
                    cal = once(args.cpu, b, algo, n, 0, d, 200, 0, args.c)
                    mean = max((l.get("mean_ns", 0) for l in cal), default=0)
                    s = int(TARGET_NS / mean) if mean > 0 else MIN_STEPS
                    steps[(who, algo, d, n, rho)] = max(MIN_STEPS, min(MAX_STEPS, s))
        for seed in range(1, args.reps + 1):
            for (who, b, algo, d) in configs:
                for n in ns:
                    for rho in rhos:
                        s = steps[(who, algo, d, n, rho)]
                        for l in once(args.cpu, b, algo, n, rho, d, s, seed, args.c):
                            l["rep"] = seed
                            f.write(json.dumps(l) + "\n")
                        f.flush()
            print(f"run: rep {seed}/{args.reps} done", file=sys.stderr)
    print(path)


if __name__ == "__main__":
    main()
