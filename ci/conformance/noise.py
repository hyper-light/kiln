#!/usr/bin/env python3
"""How busy the machine is with work other than the lab's (the quiet runs' meter, 10-28): CPU
cores and disk MB/s used outside the lab's cgroups (w28.slice: its runners, driver and guard;
the containers of its minikube node, servers, git daemon and image builder), sampled every
`every` seconds on a thread of its own. The lab's own legs load the machine during a wave, so
pressure (PSI) alone can't tell a quiet machine from a busy lab; this can.

    noise.py [seconds]   prints a line a sample for that long (default 60)
"""
import os
import subprocess
import sys
import threading
import time

# The lab's own: its containers by name, its systemd user slice (the W28 lab's by default, as its
# lab-local.sh names them), and the whole disks its machine's filesystems are on (adacachy's btrfs
# spans four NVMe disks). Each may be given by the environment, comma-separated.
LAB_CONTAINERS = tuple(os.environ.get("NOISE_LAB_CONTAINERS", "w28lab,w28-srv-a,w28-srv-b,w28-gitd,buildx_buildkit_w28lab0").split(","))
LAB_SLICE = os.environ.get("NOISE_LAB_SLICE", f"/sys/fs/cgroup/user.slice/user-{os.getuid()}.slice/user@{os.getuid()}.service/w28.slice")
DISKS = tuple(os.environ.get("NOISE_DISKS", "nvme0n1,nvme1n1,nvme2n1,nvme3n1").split(","))


def lab_cgroups():
    out = [LAB_SLICE]
    for c in LAB_CONTAINERS:
        r = subprocess.run(["docker", "inspect", "-f", "{{.Id}}", c], capture_output=True, text=True)
        cid = r.stdout.strip()
        if r.returncode == 0 and cid:
            out.append(f"/sys/fs/cgroup/system.slice/docker-{cid}.scope")
    return [p for p in out if os.path.isdir(p)]


def cpu_usec(cg):
    try:
        with open(f"{cg}/cpu.stat") as f:
            for line in f:
                k, v = line.split()
                if k == "usage_usec":
                    return int(v)
    except OSError:
        pass
    return 0


def disk_devices():
    """The major:minor of each whole disk in DISKS."""
    devs = {}
    with open("/proc/diskstats") as f:
        for line in f:
            p = line.split()
            if p[2] in DISKS:
                devs[f"{p[0]}:{p[1]}"] = p[2]
    return devs


def disk_bytes_total(devs):
    total = 0
    with open("/proc/diskstats") as f:
        for line in f:
            p = line.split()
            if f"{p[0]}:{p[1]}" in devs:
                total += (int(p[5]) + int(p[9])) * 512
    return total


def disk_bytes_of(cg, devs):
    total = 0
    try:
        with open(f"{cg}/io.stat") as f:
            for line in f:
                p = line.split()
                if p and p[0] in devs:
                    kv = dict(x.split("=", 1) for x in p[1:] if "=" in x)
                    total += int(kv.get("rbytes", 0)) + int(kv.get("wbytes", 0))
    except OSError:
        pass
    return total


class Meter:
    """Samples others' cores and MB/s every `every` seconds; `window(t0, t1)` summarizes a span."""

    def __init__(self, every=30):
        self.every = every
        self.devs = disk_devices()
        self.samples = []  # (t, others_cores, others_mbps, lab_cores, lab_mbps)
        self.lock = threading.Lock()
        self.cgs = lab_cgroups()
        self._last = self._read()
        threading.Thread(target=self._run, daemon=True).start()

    def _read(self):
        return (time.time(), cpu_usec("/sys/fs/cgroup"), sum(cpu_usec(c) for c in self.cgs), disk_bytes_total(self.devs), sum(disk_bytes_of(c, self.devs) for c in self.cgs))

    def _run(self):
        n = 0
        while True:
            time.sleep(self.every)
            n += 1
            if n % 20 == 0:
                # Containers the lab starts again get new cgroups.
                self.cgs = lab_cgroups()
                self._last = self._read()
                continue
            now = self._read()
            t0, c0, l0, d0, ld0 = self._last
            t1, c1, l1, d1, ld1 = now
            dt = max(t1 - t0, 1e-3)
            lab_cores = max(l1 - l0, 0) / dt / 1e6
            all_cores = max(c1 - c0, 0) / dt / 1e6
            lab_mbps = max(ld1 - ld0, 0) / dt / 1e6
            all_mbps = max(d1 - d0, 0) / dt / 1e6
            with self.lock:
                self.samples.append((t1, max(all_cores - lab_cores, 0.0), max(all_mbps - lab_mbps, 0.0), lab_cores, lab_mbps))
                del self.samples[:-20000]
            self._last = now

    def window(self, t0, t1=None):
        t1 = t1 or time.time()
        with self.lock:
            s = [x for x in self.samples if t0 <= x[0] <= t1]
        if not s:
            return None
        oc, om = [x[1] for x in s], [x[2] for x in s]
        return {"from": round(t0), "to": round(t1), "samples": len(s), "others_cores_mean": round(sum(oc) / len(oc), 2), "others_cores_max": round(max(oc), 2),
                "others_mbps_mean": round(sum(om) / len(om), 1), "others_mbps_max": round(max(om), 1)}

    def last(self, seconds):
        return self.window(time.time() - seconds)


if __name__ == "__main__":
    span = int(sys.argv[1]) if len(sys.argv) > 1 else 60
    m = Meter(every=10)
    print(f"lab cgroups: {len(m.cgs)}; disks: {', '.join(m.devs.values())}", flush=True)
    end = time.time() + span
    seen = 0
    while time.time() < end:
        time.sleep(1)
        with m.lock:
            new = m.samples[seen:]
            seen = len(m.samples)
        for t, oc, om, lc, lm in new:
            print(f"{time.strftime('%H:%M:%S', time.localtime(t))} others {oc:5.1f} cores {om:7.1f} MB/s | lab {lc:5.1f} cores {lm:7.1f} MB/s", flush=True)
