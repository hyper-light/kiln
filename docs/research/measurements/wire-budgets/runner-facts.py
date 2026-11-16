#!/usr/bin/env python3
"""What the facts a runner's record gained from 2026-11-10 to 14 add to `GET /api/agents`, the wire budget
"runners" (crates/kiln/tests/wire_budgets.rs, API_WIRE), as the server sends it: brotli quality 4 with the
smallest window that holds the body (crates/kiln/src/http/compress.rs, BROTLI_QUALITY and brotli_window),
gzip 5 beside it. The test's two runners on one host (the server's own and busy-runner, both connected over
QUIC), each `hub::AgentLive` as serde writes it, its fields in declaration order, facts from
crates/kiln/tests/fixtures/production-runners.json: a Linux host with no GPU, one with an NVIDIA GPU, a Mac.

Each field added, in landing order, with the text it carries:
- e89613f45 (W13-R01): `gpu_unavailable`, `kiln_gpu::NoGpus` as it reads where no vendor's GPU can be read
  (crates/kiln-gpu/src/lib.rs `Absent`; NVML's names, vendor/cuda-rust/cuda-bindings/src/dyn_load.rs; the
  error is the last name's `dlerror`); c0292187e: each GPU's `id`, `busy_per_mille` and `memory_used`;
- e4064c9a5 (step 32): `virt.microvm`;
- f6cb63610 (step 17): `isolation`, `crate::sandbox::facts` of a kiln that carries no microVM runtime
  (`NO_MICROVM`; `hypervisor` KVM where Linux has /dev/kvm, else null);
- 0909253c8 (step 34): `link`, both ends of `crate::net::LINK_PROVIDER` (TLS13_AES_256_GCM_SHA384, and
  hyper-quic's node provider's first group, secp384r1MLKEM1024);
- 72d28bc0b (P2.3): `isolation.engine`, `host` where the runner can hold no microVM.

`brotli` is Google's command-line encoder, which the `brotli` crate the server uses is a port of.
Usage: runner-facts.py [production-runners.json]"""
import copy
import gzip
import json
import pathlib
import subprocess
import sys

NO_MICROVM = "this machine's kiln carries no microVM runtime yet, so it can't hold an isolated agent (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d steps 16 and 17)"
NVML = '["libnvidia-ml.so.1", "/usr/lib/wsl/lib/libnvidia-ml.so.1"]'
NO_GPU = (f"no GPU to read: NVIDIA: their library isn't installed (tried {NVML}: /usr/lib/wsl/lib/libnvidia-ml.so.1: cannot open"
          " shared object file: No such file or directory); AMD: none is present that this process can read;"
          " Apple: this platform has no interface Hex reads for them")
END = {"suite": "TLS13_AES_256_GCM_SHA384", "group": "secp384r1MLKEM1024"}
FACTS = ["os", "os_version", "os_distro", "kernel", "arch", "arch_variant", "cpu", "mem", "disk", "gpus", "gpu_unavailable", "virt",
         "executors", "binfmt", "tools", "net", "custom", "probed_at", "kiln", "workspaces", "isolation", "work", "machine_id", "storage"]


def window(n):
    w = 10
    while w < 22 and (1 << w) - 16 < n:
        w += 1
    return w


def br(data):
    return len(subprocess.run(["brotli", "-q", "4", "-w", str(window(len(data))), "-c"], input=data, capture_output=True, check=True).stdout)


def gz(data):
    return len(gzip.compress(data, compresslevel=5, mtime=0))


def record(name, f, link):
    """A connected runner's `AgentLive`; what the fixture lacks of it (work, machine id, storage) is the same
    before and after, so it moves what's measured only through what the new fields can refer back to."""
    r = {"name": name, "label": name, "labels": ["self-hosted", f["os"], "x64" if f["arch"] == "x86_64" else "arm64"], "slots": 2,
         "hostname": name, "os": f["os"], "arch": f["arch"], "version": "0.1.0", "ephemeral": False, "connected": True,
         "last_seen": 1794832011412, "enrolled_at": 1794831990117, "running": [], "busy": 0, "draining": False,
         "sample": {"ts_ms": 1794832010997, "cpu_pct": 23.84, "cpus": 16, "load1": 2.31, "mem_total": 66850000896, "mem_used": 21503877120,
                    "disk_total": 1999421571072, "disk_used": 912345067520, "net_rx": 81234567, "net_tx": 71234567, "procs": 611},
         "terminal": False, "outpost": False, "facts": {k: f[k] for k in FACTS if k in f}, "transport": "direct"}
    if link:
        r["link"] = {"server": END, "runner": END}
    r["skew"] = {"standing": "alone", "why": "a local build: its commit isn't on the release branch"}
    return r


def body(f, link):
    return json.dumps([record("kiln-host", f, link), record("busy-runner", f, link)], separators=(",", ":"), ensure_ascii=False).encode()


def landings(gpu_why, gpu_more):
    def gpus(f):
        if gpu_why:
            f["gpu_unavailable"] = gpu_why
        for g in f["gpus"]:
            g.update(gpu_more)

    def microvm(f):
        f["virt"]["microvm"] = False

    def isolation(f):
        f["isolation"] = {"available": False, "hypervisor": "KVM" if f["os"] == "linux" and f["virt"]["kvm"] else None, "why_not": NO_MICROVM}

    def engine(f):
        f["isolation"]["engine"] = "host"

    return [("e89613f45, c0292187e W13-R01 GPUs through Hex", gpus, False), ("e4064c9a5 step 32 virt.microvm", microvm, False),
            ("f6cb63610 step 17 isolation", isolation, False), ("0909253c8 step 34 link", lambda f: None, True),
            ("72d28bc0b P2.3 isolation.engine", engine, True)]


def main():
    root = pathlib.Path(__file__).resolve().parents[4]
    path = sys.argv[1] if len(sys.argv) > 1 else root / "crates/kiln/tests/fixtures/production-runners.json"
    by = {r["name"]: r["facts"] for r in json.load(open(path))}
    hosts = [("Linux x86_64 with KVM and no GPU (fedora.attlocal.net)", "fedora.attlocal.net", NO_GPU, {}),
             ("Linux x86_64 with KVM and an NVIDIA GPU (adacachy)", "adacachy", None, {"id": "0000:01:00.0", "busy_per_mille": 40, "memory_used": 1 << 30}),
             ("macOS arm64 with an Apple GPU (adas-macbook-pro-2.local)", "adas-macbook-pro-2.local", None, {"id": "metal:4294969120", "busy_per_mille": 120, "memory_used": 2 << 30})]
    for title, name, why, more in hosts:
        f = copy.deepcopy(by[name])
        f.setdefault("work", {"io_full": 0.0, "mem_full": 0.0, "memory_level": None})
        f.setdefault("machine_id", "5f0c8e1b7a2d4c39b1e6f8a0d2c4e6f8")
        f.setdefault("storage", {"fs": "ext4", "mount": "/", "source": "/dev/nvme0n1p2", "network": False,
                                 "devices": [{"name": "nvme0n1", "kind": "nvme", "interconnect": "pcie", "model": "Samsung SSD 990 PRO 2TB", "health": "ok"}]})
        before = body(f, False)
        print(f"\n{title}\n  {'two runners':<48} {'raw':>6} {'br 4':>6} {'added':>6} {'gzip 5':>7}")
        print(f"  {'as at a64856c89':<48} {len(before):>6} {br(before):>6} {'':>6} {gz(before):>7}")
        link, last = False, br(before)
        for label, change, linked in landings(why, more):
            change(f)
            link = link or linked
            now = body(f, link)
            print(f"  {'+ ' + label:<48} {len(now):>6} {br(now):>6} {br(now) - last:>+6} {gz(now):>7}")
            last = br(now)
        print(f"  {'all of them':<48} {len(now) - len(before):>+6} {last - br(before):>+6} {'':>6} {gz(now) - gz(before):>+7}")


if __name__ == "__main__":
    main()
