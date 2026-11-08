#!/usr/bin/env python3
"""The load-balancer bench's runner (bench-spec.md, 2026-11-07).

  run.py prepare [nginx|haproxy|envoy]  fetch a competitor at its pin and build it
  run.py build [nginx|haproxy|null]     compile the drivers against the competitors' objects
  run.py smoke                          every driver and algorithm once, small, checked
  run.py warmup                         W per N: when the imbalance stops trending (MSER-5)
  run.py measure                        the interleaved rounds: one JSON line per run

Competitor sources, builds and driver binaries live under ~/.cache/kiln-lb/bench
(src/, build/, bin/), never in the repository. Standard library only.
"""

import argparse
import contextlib
import datetime
import fcntl
import json
import os
import platform
import random
import shlex
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
HOME = os.path.expanduser("~")
BENCH = os.path.join(HOME, ".cache", "kiln-lb", "bench")
SRC = os.path.join(BENCH, "src")
BUILD = os.path.join(BENCH, "build")
BIN = os.path.join(BENCH, "bin")
KILN_WT = os.path.join(HOME, ".cache", "kiln-lb", "wt-kernel")
KILN_TARGET = os.path.join(HOME, ".cache", "kiln-lb", "target-kernel")
# Timed runs hold this exclusively, so no other bench on the machine (the end-to-end one) measures beside them.
MEASURE_LOCK = os.path.join(HOME, ".cache", "kiln-lb", "bench-measure.lock")

# The pins (bench-spec.md): never move them without the spec.
PINS = {
    "nginx": ("https://github.com/nginx/nginx.git", "140aa942711868a8daeb95810bc9a81f272ada72", "release-1.31.8"),
    "haproxy": ("https://github.com/haproxy/haproxy.git", "8872a2604595140544774c2a6309a41186fc0f44", "v3.4.7"),
    "envoy": ("https://github.com/envoyproxy/envoy.git", "b2d89e0ae53916f28770771fbdd49c5c99c5c5a2", "v1.40.2"),
    # Katran has no tags: main at the commit the competitor study read.
    "katran": ("https://github.com/facebookincubator/katran.git", "042d08df6677dbde86ed7b127218a8e80445ce41", "main@042d08d"),
}
# Katran's lookup, copied verbatim from katran/lib/bpf/balancer.bpf.c (the new-flow path of get_packet_dst).
KATRAN_LOOKUP = ["get_packet_hash", "is_under_flood", "increment_ch_drop_no_real", "increment_ch_drop_real_0", "get_packet_dst"]
# Katran's BPF build compiles balancer.bpf.c at -O2 (katran/lib/Makefile-bpf); its ring code is built the same.
KATRAN_CFLAGS = ["-O2", "-g"]

# nginx: its own configure, its default -O for clang plus -O2 (spec: their default, plus -O2 if lower).
NGINX_CONFIGURE = ["--without-pcre", "--without-http_rewrite_module", "--without-http_gzip_module", "--with-cc-opt=-O2"]
NGINX_CFLAGS = ["-pipe", "-O", "-Wall", "-Wextra", "-Wpointer-arith", "-Wconditional-uninitialized", "-Wno-unused-parameter",
                "-Wno-deprecated-declarations", "-g", "-O2"]
# HAProxy: its Makefile's own defaults for TARGET=osx (OPT_CFLAGS = -O2 already).
HAPROXY_MAKE = []  # TARGET= is added per platform (haproxy_target)

# Every algorithm each competitor ships that this workload can drive (bench-spec.md: Algorithms), each run
# twice where the competitor has admission: as configured by default, and with it at C (the
# equal-quality comparison: nginx max_conns, HAProxy maxconn; kiln never oversubscribes).
# Load-aware: the pick reads the backends' in-flight counts. The person's decision (2026-11-07): every
# algorithm counts toward the bar; the load-aware-only comparison is shown beside it.
C = 8
ALGOS = {
    "null": [("null", "null")],
    "nginx": [("round_robin", "unaware"), ("least_conn", "aware"), ("random", "unaware"), ("random_two_least_conn", "aware"),
              ("hash", "unaware"), ("hash_consistent", "unaware"), ("ip_hash", "unaware"), ("least_time", "aware")],
    "haproxy": [("roundrobin", "unaware"), ("static-rr", "unaware"), ("leastconn", "aware"), ("first", "unaware"),
                ("random", "aware"), ("source", "unaware"), ("source-consistent", "unaware")],
    "envoy": [("round_robin", "unaware"), ("least_request", "aware"), ("least_request_full_scan", "aware"), ("random", "unaware"),
              ("ring_hash", "unaware"), ("maglev", "unaware")],
    "katran": [("maglev", "unaware")],
}
ADMISSION = {"nginx": "/max_conns=%d" % C, "haproxy": "/maxconn=%d" % C}
NS = [100, 500, 4095, 10000]
RHOS = [0.0, 0.5, 0.9]
KILN_DS = [1, 2, 4]
KILN_BATCHES = []  # --kiln-batch K: kiln's batched decision (ask_many over K), reported apart


def implementations(which):
    """[(impl, algo, d, label)] for the impls named, admission variants included."""
    out = []
    for impl in which:
        if impl == "kiln":
            for d in KILN_DS:
                out.append(("kiln", "kiln", d, "aware"))
            for k in KILN_BATCHES:
                out.append(("kiln", "kiln-batch%d" % k, 1, "aware"))
            continue
        for algo, label in ALGOS[impl]:
            out.append((impl, algo, 1, label))
            if impl in ADMISSION:
                out.append((impl, algo + ADMISSION[impl], 1, label))
    return out


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def run(cmd, cwd=None, env=None, check=True, capture=False, nice=True, timeout=None):
    """Runs <cmd> (a list), at nice 10 for builds; output to stderr unless captured."""
    if nice:
        cmd = ["nice", "-n", "10"] + cmd
    if capture:
        p = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout)
    else:
        p = subprocess.run(cmd, cwd=cwd, env=env, stdout=sys.stderr, stderr=sys.stderr, timeout=timeout)
    if check and p.returncode != 0:
        tail = (p.stderr or "")[-2000:] if capture else ""
        raise SystemExit("failed (%d): %s\n%s" % (p.returncode, " ".join(cmd), tail))
    return p


# ---- prepare: fetch at the pin, build as the project builds itself ---------------------------------------


def fetch(name):
    url, sha, tag = PINS[name]
    d = os.path.join(SRC, name)
    if not os.path.isdir(os.path.join(d, ".git")):
        os.makedirs(d, exist_ok=True)
        run(["git", "-C", d, "init", "-q"], nice=False)
        run(["git", "-C", d, "remote", "add", "origin", url], nice=False)
    head = run(["git", "-C", d, "rev-parse", "HEAD"], capture=True, check=False, nice=False).stdout.strip()
    if head != sha:
        run(["git", "-C", d, "fetch", "-q", "--depth", "1", "origin", sha], nice=False)
        run(["git", "-C", d, "checkout", "-q", "--detach", sha], nice=False)
    head = run(["git", "-C", d, "rev-parse", "HEAD"], capture=True, nice=False).stdout.strip()
    if head != sha:
        raise SystemExit("%s is at %s, not its pin %s (%s)" % (name, head, sha, tag))
    log(name, tag, sha)
    return d


def prepare_nginx():
    src = fetch("nginx")
    b = os.path.join(BUILD, "nginx")
    if not os.path.exists(os.path.join(b, "Makefile")):
        run(["./auto/configure", "--builddir=" + b, "--prefix=" + os.path.join(BUILD, "nginx-prefix")] + NGINX_CONFIGURE, cwd=src)
    run(["make", "-j4"], cwd=src)


def prepare_haproxy():
    src = fetch("haproxy")
    b = os.path.join(BUILD, "haproxy")
    if not os.path.isdir(b):
        # HAProxy builds in its tree: build in a worktree of the source, so src/ stays as fetched.
        run(["git", "-C", src, "worktree", "add", "-q", "--detach", b, PINS["haproxy"][1]], nice=False)
    run(["make", "-j4", "TARGET=" + haproxy_target()] + HAPROXY_MAKE, cwd=b)


def prepare_katran():
    fetch("katran")


# Envoy: its own bazel (the version in its .bazelversion, through bazelisk), -c opt, output under BENCH/bazel.
BAZELISK = ("v1.25.0", "https://github.com/bazelbuild/bazelisk/releases/download/v1.25.0/bazelisk-%s-%s")
ENVOY_PKG = "test/extensions/load_balancing_policies/kiln_bench"
ENVOY_DRIVER = "//%s:envoy_driver" % ENVOY_PKG
# Envoy's own LB benches, unpatched, for reference (random_lb_benchmark.cc is empty at this pin).
ENVOY_STOCK = ["//test/extensions/load_balancing_policies/least_request:least_request_lb_benchmark",
               "//test/extensions/load_balancing_policies/round_robin:round_robin_lb_benchmark",
               "//test/extensions/load_balancing_policies/ring_hash:ring_hash_lb_benchmark",
               "//test/extensions/load_balancing_policies/maglev:maglev_lb_benchmark"]
DISK_FLOOR = 100 * 2 ** 30  # the brief: keep 100 GiB free while Envoy builds


def free_bytes():
    st = os.statvfs(HOME)
    return st.f_bavail * st.f_frsize


def bazelisk():
    tools = os.path.join(BENCH, "tools")
    os.makedirs(tools, exist_ok=True)
    out = os.path.join(tools, "bazelisk")
    if not os.access(out, os.X_OK):
        osn = "darwin" if platform.system() == "Darwin" else "linux"
        arch = "arm64" if platform.machine() in ("arm64", "aarch64") else "amd64"
        url = BAZELISK[1] % (osn, arch)
        run(["curl", "-fsSL", "-o", out + ".part", url], nice=False)
        os.chmod(out + ".part", 0o755)
        os.rename(out + ".part", out)
    return out


def prepare_envoy(jobs):
    src = fetch("envoy")
    pkg = os.path.join(src, ENVOY_PKG)
    os.makedirs(pkg, exist_ok=True)
    for f in [os.path.join(HERE, "drivers", "envoy", "BUILD"), os.path.join(HERE, "drivers", "envoy", "envoy.cc"), os.path.join(HERE, "common.h")]:
        shutil.copy(f, pkg)
    if free_bytes() < DISK_FLOOR:
        raise SystemExit("envoy: %.0f GiB free, under the 100 GiB floor: not building" % (free_bytes() / 2 ** 30))
    bz = bazelisk()
    logf = os.path.join(BENCH, "envoy-build.log")
    # --batch: no server. On a machine whose process sweeps end daemons a command leaves running (this Mac's
    # did: "Server terminated abruptly"), bazel's server dies at its first command; in batch mode it is the client.
    root = ["--output_user_root=" + os.path.join(BENCH, "bazel"), "--batch"]
    cmd = ["nice", "-n", "10", bz] + root + ["build", "-c", "opt", "--jobs=%d" % jobs,
           "--color=no", "--curses=no", "--show_progress_rate_limit=60", ENVOY_DRIVER] + ENVOY_STOCK
    log("envoy:", " ".join(cmd), "->", logf)
    with open(logf, "a") as lf:
        p = subprocess.Popen(cmd, cwd=src, stdout=lf, stderr=lf)
        while p.poll() is None:
            if free_bytes() < DISK_FLOOR:
                log("envoy: under 100 GiB free (%.0f GiB): stopping the build" % (free_bytes() / 2 ** 30))
                p.terminate()
                p.wait()
                raise SystemExit("envoy: stopped at the disk floor")
            time.sleep(20)
    if p.returncode != 0:
        raise SystemExit("envoy: bazel failed (%d); see %s" % (p.returncode, logf))
    os.makedirs(BIN, exist_ok=True)
    bazel_bin = run([bz] + root + ["info", "-c", "opt", "bazel-bin"], cwd=src, capture=True, nice=False).stdout.strip()
    for t in [ENVOY_DRIVER] + ENVOY_STOCK:
        path, name = t[2:].split(":")
        dst = os.path.join(BIN, "envoy-driver" if t == ENVOY_DRIVER else "envoy-stock-" + name)
        shutil.copy(os.path.join(bazel_bin, path, name), dst)
        log("envoy: built", dst)
    log("envoy: %.0f GiB free after the build" % (free_bytes() / 2 ** 30))


# ---- build: the drivers --------------------------------------------------------------------------------


def newer(target, deps):
    if not os.path.exists(target):
        return True
    t = os.path.getmtime(target)
    return any(os.path.getmtime(d) > t for d in deps if os.path.exists(d))


def build_null():
    os.makedirs(BIN, exist_ok=True)
    out = os.path.join(BIN, "null-driver")
    deps = [os.path.join(HERE, "drivers", "null.c"), os.path.join(HERE, "common.h")]
    if newer(out, deps):
        run(["cc", "-O2", "-g", "-Wall", "-Wextra", "-o", out, deps[0], "-lm"])
    return out


def build_nginx():
    src = os.path.join(SRC, "nginx")
    b = os.path.join(BUILD, "nginx")
    lb = os.path.join(b, "lb")
    os.makedirs(lb, exist_ok=True)
    os.makedirs(BIN, exist_ok=True)
    incs = []
    for d in ["src/core", "src/event", "src/event/modules", "src/event/quic", "src/os/unix", b, "src/http", "src/http/modules"]:
        incs += ["-I", d if os.path.isabs(d) else os.path.join(src, d)]
    nomain = os.path.join(lb, "nginx_nomain.o")
    if newer(nomain, [os.path.join(src, "src/core/nginx.c")]):
        # nginx.c, as nginx compiles it, with its main() renamed: the driver's main starts nginx instead.
        run(["cc", "-c"] + NGINX_CFLAGS + incs + ["-Dmain=lb_nginx_unused_main", "-o", nomain, os.path.join(src, "src/core/nginx.c")], cwd=src)
    drv = os.path.join(lb, "driver.o")
    dsrc = os.path.join(HERE, "drivers", "nginx.c")
    if newer(drv, [dsrc, os.path.join(HERE, "common.h")]):
        run(["cc", "-c"] + NGINX_CFLAGS + incs + ["-o", drv, dsrc], cwd=src)
    objs = []
    for root, _, files in os.walk(os.path.join(b, "src")):
        for f in files:
            if f.endswith(".o"):
                objs.append(os.path.join(root, f))
    objs = sorted(o for o in objs if not o.endswith(os.path.join("src", "core", "nginx.o")))
    # The libraries on nginx's own link line (its Makefile's recipe for the nginx binary).
    mk = open(os.path.join(b, "Makefile")).read()
    recipe = mk[mk.index("$(LINK) -o"):]
    recipe = recipe[:recipe.index("\n\n")] if "\n\n" in recipe else recipe
    libs = [t for t in recipe.split() if t.startswith("-l") or t.startswith("-Wl,")]
    out = os.path.join(BIN, "nginx-driver")
    if newer(out, [drv, nomain] + objs):
        run(["cc", "-o", out, drv, nomain] + objs + [os.path.join(b, "ngx_modules.o")] + libs + ["-lm"])
    return out


def haproxy_target():
    return "osx" if platform.system() == "Darwin" else "linux-glibc"


def haproxy_opts(b):
    """HAProxy's own compile options, link options and objects (its Makefile's `make opts`)."""
    out = run(["make", "TARGET=" + haproxy_target(), "-s", "opts"], cwd=b, capture=True, nice=False).stdout
    opts = {}
    for line in out.splitlines():
        for k in ("COPTS", "LDOPTS", "OPTIONS_OBJS", "OBJS"):
            if line.startswith(k + "="):
                # One shell word: the value's own words keep the quotes the macros need (-DCONFIG_HAPROXY_VERSION="3.4.7").
                opts[k] = shlex.split(line)[0][len(k) + 1:].split()
    return opts


def build_haproxy():
    b = os.path.join(BUILD, "haproxy")
    lb = os.path.join(b, "lb")
    os.makedirs(lb, exist_ok=True)
    os.makedirs(BIN, exist_ok=True)
    o = haproxy_opts(b)
    nomain = os.path.join(lb, "haproxy_nomain.o")
    if newer(nomain, [os.path.join(b, "src", "haproxy.c")]):
        # haproxy.c, as HAProxy compiles it, with main() renamed: the driver's main starts HAProxy through it.
        run(["cc"] + o["COPTS"] + ["-Dmain=lb_haproxy_main", "-c", "-o", nomain, "src/haproxy.c"], cwd=b)
    drv = os.path.join(lb, "driver.o")
    dsrc = os.path.join(HERE, "drivers", "haproxy.c")
    if newer(drv, [dsrc, os.path.join(HERE, "common.h")]):
        run(["cc"] + o["COPTS"] + ["-Wno-declaration-after-statement", "-c", "-o", drv, dsrc], cwd=b)
    objs = [nomain if x == "src/haproxy.o" else os.path.join(b, x) for x in o["OPTIONS_OBJS"] + o["OBJS"]]
    out = os.path.join(BIN, "haproxy-driver")
    if newer(out, [drv] + objs):
        run(["cc", "-o", out, drv] + objs + o["LDOPTS"] + ["-lm"])
    return out


# ---- the machine, and each run's load ---------------------------------------------------------------------


def sh(cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def machine():
    if platform.system() == "Darwin":
        cpu = sh(["sysctl", "-n", "machdep.cpu.brand_string"])
        p, e = sh(["sysctl", "-n", "hw.perflevel0.physicalcpu"]), sh(["sysctl", "-n", "hw.perflevel1.physicalcpu"])
        osv = "macOS " + sh(["sw_vers", "-productVersion"])
        cores = "%s P + %s E cores" % (p, e)
    else:
        cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "?")
        osv, cores = "Linux " + platform.release(), "%d CPUs" % os.cpu_count()
    return "%s (%s, %s, %s)" % (platform.node().split(".")[0], cpu, cores, osv)


def load1():
    return round(os.getloadavg()[0], 2)


def git_head(d):
    return sh(["git", "-C", d, "rev-parse", "HEAD"])


def bench_commit():
    """This harness's commit: BENCH_COMMIT beside run.py when the harness was copied out of its repository."""
    f = os.path.join(HERE, "BENCH_COMMIT")
    if os.path.exists(f):
        return open(f).read().strip()
    return git_head(HERE)


# ---- one run of one implementation -----------------------------------------------------------------------


KILN_BIN = os.environ.get("KILN_LB_BENCH_BIN", "")


def kiln_bench_binary():
    """kiln's bench: --kiln-bin (or KILN_LB_BENCH_BIN), else the newest `lb-*` that `cargo bench -p kiln-ci --bench lb
    --no-run` left in the kernel worktree's target dir (CARGO_TARGET_DIR=~/.cache/kiln-lb/target-kernel)."""
    if KILN_BIN:
        return KILN_BIN if os.access(KILN_BIN, os.X_OK) else None
    d = os.path.join(KILN_TARGET, "release", "deps")
    try:
        c = [os.path.join(d, f) for f in os.listdir(d) if f.startswith("lb-") and "." not in f and os.access(os.path.join(d, f), os.X_OK)]
    except OSError:
        return None
    return max(c, key=os.path.getmtime) if c else None


def kiln_choice():
    """The kiln bench to time, as the kernel agent names it in ~/.cache/kiln-lb/notes/kiln-bench-bin.txt (mirrored
    beside run.py as KILN_BENCH_BIN): line 1 its path on this machine, line 2 its commit or variant, line 3 the batch K.
    Re-read before every round. (path or None, variant, [K])."""
    f = os.path.join(HERE, "KILN_BENCH_BIN")
    if not os.path.exists(f):
        return None, "", []
    import re
    lines = [l.strip() for l in open(f).read().splitlines()]
    path = os.path.expanduser(lines[0].split()[0]) if lines and lines[0] else None
    variant = lines[1] if len(lines) > 1 else ""
    # Line 3 names batch sizes in prose ("1 (single ...; --batch 64 for the batched variant)"): every integer
    # above 1 is a batched decision to time beside the single one.
    ks = sorted({int(x) for x in re.findall(r"\b\d+\b", lines[2])} - {0, 1}) if len(lines) > 2 else []
    return path, variant, ks


def kiln_commit_of(variant):
    """The commit a variant line names (its first hex word of 7 to 40 digits), else the line."""
    import re
    m = re.search(r"\b[0-9a-f]{7,40}\b", variant)
    return m.group(0) if m else variant


def command(impl, algo, d, n, rho, extra):
    if impl == "kiln":
        b = kiln_bench_binary()
        if not b:
            return None
        batch = ["--batch", algo[len("kiln-batch"):]] if algo.startswith("kiln-batch") else []
        return [b, "--bench", "--json", "--n", str(n), "--rho", str(rho), "--d", str(d), "--runs", "1"] + batch + extra
    exe = os.path.join(BIN, ("null" if impl == "null" else impl) + "-driver")
    if not os.access(exe, os.X_OK):
        return None
    return [exe, algo, "--n", str(n), "--rho", str(rho)] + extra


@contextlib.contextmanager
def measuring(timed):
    """An exclusive flock on MEASURE_LOCK for the length of a timed run (none for untimed ones)."""
    if not timed:
        yield None
        return
    os.makedirs(os.path.dirname(MEASURE_LOCK), exist_ok=True)
    with open(MEASURE_LOCK, "a") as fh:
        t0 = time.time()
        fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
        try:
            yield time.time() - t0
        finally:
            fcntl.flock(fh.fileno(), fcntl.LOCK_UN)


def once(impl, algo, d, n, rho, extra, timeout, timed=True):
    """One run: its JSON lines, with the load before and after; or an error line."""
    cmd = command(impl, algo, d, n, rho, extra)
    if cmd is None:
        return [{"impl": impl, "algo": algo, "d": d, "n": n, "rho": rho, "error": "not built"}]
    with measuring(timed) as waited:
        before = load1()
        try:
            p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            return [{"impl": impl, "algo": algo, "d": d, "n": n, "rho": rho, "error": "timeout after %ds" % timeout, "cmd": cmd}]
        after = load1()
    lines = []
    for l in p.stdout.splitlines():
        try:
            j = json.loads(l)
        except ValueError:
            continue
        if isinstance(j, dict):
            j.setdefault("d", d)
            j.update({"load_before": before, "load_after": after})
            if waited is not None:
                j["lock_wait_s"] = round(waited, 3)
            lines.append(j)
    if p.returncode != 0 or not lines:
        return [{"impl": impl, "algo": algo, "d": d, "n": n, "rho": rho, "error": (p.stderr or "")[-1500:], "exit": p.returncode, "cmd": cmd}]
    return lines


# ---- W: when the imbalance stops trending (MSER-5) --------------------------------------------------------


def mser5(series):
    """MSER-5 (White 1997; the warm-up rule Hoad, Robinson and Davies 2010 found best): the batch count d*
    to cut that minimises the marginal standard error of the 5-sample batch means left. Returns (d*, batches)."""
    m = len(series) // 5
    if m < 2:
        return 0, m
    b = [sum(series[5 * j:5 * j + 5]) / 5.0 for j in range(m)]
    best, arg = None, 0
    for d in range(m - 1):
        tail = b[d:]
        mu = sum(tail) / len(tail)
        v = sum((x - mu) ** 2 for x in tail) / (len(tail) ** 2)
        if best is None or v < best - 1e-18:
            best, arg = v, d
    return arg, m


def w_of(per):
    """W: the largest MSER-5 cut that MSER's own rule accepts (in the first half of its series). A series still
    trending at the longest horizon (too_short) has no valid cut: it is reported as not at steady state instead."""
    ws = [v["w"] for v in per.values() if "w" in v and not v.get("too_short")]
    return max(ws) if ws else 0


def save_w(wfile, n, entry):
    old = json.load(open(wfile)) if os.path.exists(wfile) else {}
    old[str(n)] = entry
    old["_machine"] = machine()
    with open(wfile + ".part", "w") as fh:
        json.dump(old, fh, indent=1, sort_keys=True)
    os.replace(wfile + ".part", wfile)


def warmup(impls, ns, rho, timeout, wfile):
    """W per N: the largest MSER-5 cut over every implementation's imbalance series at rho (the highest load:
    the most in flight, the slowest to forget the fill). The series starts after the fill and samples the CV
    every N steps; the horizon starts at 4 residence times (4 F': the fill's share left is e^-4) and 100 samples,
    and doubles (at most twice) while the cut falls in the series' second half (MSER's rule: the run was too
    short); a cut still there is recorded as too_short."""
    out = {}
    for n in ns:
        f = max(1, int(rho * n * C))
        per = {}
        for impl, algo, d, label in impls:
            if impl in ("null", "kiln"):
                continue
            h = max(4 * f, 100 * n)
            h = ((h + 5 * n - 1) // (5 * n)) * 5 * n
            for attempt in range(3):
                lines = once(impl, algo, d, n, rho, ["--mode", "trend", "--horizon", str(h)], timeout, timed=False)
                j = lines[0]
                if "error" in j:
                    per["%s %s" % (impl, algo)] = {"error": j["error"][-300:]}
                    break
                cut, m = mser5(j["cv_series"])
                if cut <= m // 2 or attempt == 2:
                    per["%s %s" % (impl, algo)] = {"w": cut * 5 * n, "cut_batches": cut, "batches": m, "horizon": h,
                                                   "too_short": cut > m // 2}
                    break
                h *= 2
            log("warmup", n, impl, algo, per.get("%s %s" % (impl, algo)))
        out[str(n)] = {"w": w_of(per), "rho": rho, "per": per,
                       "not_steady": sorted(k for k, v in per.items() if v.get("too_short"))}
        save_w(wfile, n, out[str(n)])
    return out


# ---- B: from the null ------------------------------------------------------------------------------------


def calibrate(n, rho, w, timeout):
    """The smallest power of two whose block of null steps takes 1,000 counter ticks: the largest of three
    calibrations, so noise can only lengthen the block, never shorten it below the counter's resolution."""
    bs = []
    for _ in range(3):
        j = once("null", "null", 1, n, rho, ["--mode", "calibrate", "--w", str(w)], timeout)[0]
        if "error" in j:
            raise SystemExit("calibration failed: %s" % j["error"])
        bs.append(j["b"])
    return max(bs)


# ---- the rounds ------------------------------------------------------------------------------------------


def median(xs):
    return statistics.median(xs) if xs else float("nan")


def boot_median_ci(xs, rng, resamples):
    """The 95% percentile bootstrap interval of the median (Efron and Tibshirani 1993, ch. 13)."""
    n = len(xs)
    ms = sorted(statistics.median([xs[rng.randrange(n)] for _ in range(n)]) for _ in range(resamples))
    return ms[int(0.025 * resamples)], ms[int(0.975 * resamples) - 1]


def net(j):
    """The deciding number: ns per step less the null's (the null itself: its own)."""
    return j["ns_step"] if j["impl"] == "null" else j["ns_step"] - j.get("ns_null", 0.0)


def precise(rows, precision, rng):
    xs = [net(j) for j in rows]
    if len(xs) < 2:
        return False
    m = median(xs)
    lo, hi = boot_median_ci(xs, rng, 2000)
    return m > 0 and lo >= m * (1 - precision) and hi <= m * (1 + precision)


def measure(impls, ns, rhos, wfile, out, min_runs, max_runs, precision, timeout):
    global KILN_BIN
    ws = json.load(open(wfile)) if wfile and os.path.exists(wfile) else {}
    mach = machine()
    harness = bench_commit()
    kiln_commit = git_head(KILN_WT) if any(i[0] == "kiln" for i in impls) else ""
    rng = random.Random(20261107)
    with open(out, "a") as f:
        for n in ns:
            e = ws.get(str(n), {})
            w = w_of(e["per"]) if "per" in e else e.get("w")
            if w is None:
                raise SystemExit("no W for N = %d in %s: run.py warmup first" % (n, wfile))
            for rho in rhos:
                b = calibrate(n, rho, w, timeout)
                log("N", n, "rho", rho, "W", w, "B", b)
                rows = {k: [] for k in impls}
                live = list(impls)
                for r in range(max_runs):
                    kpath, kvariant, kks = kiln_choice()
                    if any(i[0] == "kiln" for i in impls):
                        if kpath:
                            KILN_BIN = kpath
                        for kk in kks:
                            k = ("kiln", "kiln-batch%d" % kk, 1, "aware")
                            if k not in rows:
                                rows[k] = []
                                live.append(k)
                    order = live[r % len(live):] + live[:r % len(live)]
                    for k in order:
                        impl, algo, d, label = k
                        extra = ["--b", str(b), "--w", str(w), "--run", str(r)]
                        for j in once(impl, algo, d, n, rho, extra, timeout):
                            j.update({"machine": mach, "label": label, "round": r, "bench_commit": harness, "w": w, "b": j.get("b", b)})
                            if impl == "kiln":
                                # kiln's bench prints the cwd's HEAD as its commit: the variant line says which it is.
                                j["commit"] = kiln_commit_of(kvariant) if kvariant else (j.get("commit") or kiln_commit)
                                j["kiln_bin"] = kiln_bench_binary() or ""
                                j["kiln_variant"] = kvariant
                            f.write(json.dumps(j) + "\n")
                            f.flush()
                            if "error" in j:
                                log("error", impl, algo, n, rho, j["error"][-200:])
                                if k in live:
                                    live.remove(k)
                            elif j.get("d", d) == d:
                                rows[k].append(j)
                    if not live:
                        break
                    if r + 1 >= min_runs:
                        # Each median's R grows until its own interval is within the precision (bench-spec.md):
                        # one that is retires from this configuration's later rounds; the rest stay interleaved.
                        done = [k for k in live if precise(rows[k], precision, rng)]
                        for k in done:
                            live.remove(k)
                        if done:
                            log("N", n, "rho", rho, "after", r + 1, "runs, precise:", [k[0] + " " + k[1] + ("" if k[2] == 1 else " d=%d" % k[2]) for k in done])
                    if not live:
                        log("N", n, "rho", rho, "every median precise after", r + 1, "rounds")
                        break
                    log("N", n, "rho", rho, "round", r + 1, "done;", len(live), "still imprecise:", [k[0] + " " + k[1] for k in live][:8])


def smoke(impls, timeout):
    bad = 0
    for impl, algo, d, label in impls:
        j = once(impl, algo, d, 100, 0.5, ["--b", "1024", "--w", "1000"], timeout, timed=False)[0]
        if "error" in j:
            bad += 1
            log("FAIL", impl, algo, j["error"][-300:])
        else:
            log("ok", impl, algo, "ns_step %.1f" % j["ns_step"], "oversub", j["oversub"], "refusals", j["refusals"], "cv %.3f" % j["cv"])
    return bad


def extract_c_functions(path, names):
    """The named functions' text, verbatim from a C file (from their attributes to their closing brace)."""
    import re
    text = open(path).read()
    out = []
    for name in names:
        m = re.search(r"(__attribute__\(\(__always_inline__\)\)\s+)?static\s+inline\s+[\w\s\*]*?\b%s\s*\(" % re.escape(name), text)
        if not m:
            raise SystemExit("katran: %s not found in %s" % (name, path))
        i = text.index("{", m.end())
        depth, j = 0, i
        while True:
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        first, last = text.count("\n", 0, m.start()) + 1, text.count("\n", 0, j) + 1
        out.append("/* %s: balancer.bpf.c:%d-%d */\n%s\n" % (name, first, last, text[m.start():j + 1]))
    return out


def build_katran():
    src = os.path.join(SRC, "katran")
    lb = os.path.join(BUILD, "katran")
    os.makedirs(lb, exist_ok=True)
    os.makedirs(BIN, exist_ok=True)
    here = os.path.join(HERE, "drivers", "katran")
    bpf = os.path.join(src, "katran", "lib", "bpf", "balancer.bpf.c")
    inc = os.path.join(lb, "lookup.inc")
    body = extract_c_functions(bpf, KATRAN_LOOKUP)
    with open(inc, "w") as f:
        f.write("/* Katran %s, katran/lib/bpf/balancer.bpf.c: copied verbatim by ci/bench/run.py. */\n\n" % PINS["katran"][1])
        f.write("\n".join(body))
    objs = []
    for cc in ["CHHelpers.cpp", "MaglevBase.cpp", "MaglevHash.cpp", "MaglevHashV2.cpp", "MurmurHash3.cpp"]:
        o = os.path.join(lb, cc.replace(".cpp", ".o"))
        run(["c++", "-std=c++17"] + KATRAN_CFLAGS + ["-I", src, "-c", "-o", o, os.path.join(src, "katran", "lib", cc)])
        objs.append(o)
    ring = os.path.join(lb, "ring.o")
    run(["c++", "-std=c++17"] + KATRAN_CFLAGS + ["-I", src, "-c", "-o", ring, os.path.join(here, "ring.cc")])
    # N = 10,000 is above Katran's default MAX_REALS (4096): balancer_consts.h lets a build raise it.
    drv = os.path.join(lb, "katran.o")
    run(["cc"] + KATRAN_CFLAGS + ["-DMAX_REALS=16384", "-I", os.path.join(here, "shim"), "-I", src, "-I", lb, "-c", "-o", drv,
                                   os.path.join(here, "katran.c")])
    out = os.path.join(BIN, "katran-driver")
    run(["c++", "-o", out, drv, ring] + objs + ["-lm"])
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("what", nargs="*", default=["nginx", "haproxy", "katran"])
    p.add_argument("--jobs", type=int, default=8, help="bazel's jobs, for Envoy")
    p = sub.add_parser("build")
    p.add_argument("what", nargs="*", default=["null", "nginx", "haproxy"])
    for name in ("smoke", "warmup", "measure"):
        p = sub.add_parser(name)
        p.add_argument("--impls", default="null,nginx,haproxy")
        p.add_argument("--n", default=",".join(map(str, NS)))
        p.add_argument("--rho", default=",".join(map(str, RHOS)))
        p.add_argument("--only", default="", help="comma-separated algorithms to keep (default: all)")
        p.add_argument("--timeout", type=int, default=1800)
        p.add_argument("--w-file", default=os.path.join(HERE, "results", "raw", "w-%s.json" % platform.node().split(".")[0]))
        p.add_argument("--out", default=os.path.join(HERE, "results", "raw", "runs-%s-%s.jsonl" % (
            platform.node().split(".")[0], datetime.datetime.now().strftime("%Y-%m-%dT%H%M"))))
        p.add_argument("--min-runs", type=int, default=6, help="the fewest runs with a distribution-free 95%% interval of a median")
        p.add_argument("--max-runs", type=int, default=30)
        p.add_argument("--precision", type=float, default=0.05)
        p.add_argument("--kiln-bin", default="", help="kiln's bench binary (default: the kernel worktree's target dir)")
        p.add_argument("--kiln-batch", default="", help="comma-separated batch sizes for kiln's batched decision")
    a = ap.parse_args()
    if a.cmd == "prepare":
        for w in a.what:
            if w == "envoy":
                prepare_envoy(a.jobs)
            else:
                {"nginx": prepare_nginx, "haproxy": prepare_haproxy, "katran": prepare_katran}[w]()
        return
    if a.cmd == "build":
        for w in a.what:
            log("built", {"null": build_null, "nginx": build_nginx, "haproxy": build_haproxy, "katran": build_katran}[w]())
        return
    global KILN_BIN
    if getattr(a, "kiln_bin", ""):
        KILN_BIN = a.kiln_bin
    if getattr(a, "kiln_batch", ""):
        KILN_BATCHES.extend(int(x) for x in a.kiln_batch.split(","))
    impls = implementations(a.impls.split(","))
    if a.only:
        keep = set(a.only.split(","))
        impls = [i for i in impls if i[1] in keep or i[0] in ("null", "kiln")]
    ns = [int(x) for x in a.n.split(",")]
    rhos = [float(x) for x in a.rho.split(",")]
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    if a.cmd == "smoke":
        sys.exit(1 if smoke(impls, a.timeout) else 0)
    if a.cmd == "warmup":
        res = warmup(impls, ns, max(rhos), a.timeout, a.w_file)
        log("W", {k: v["w"] for k, v in res.items()}, "->", a.w_file)
        return
    if a.cmd == "measure":
        log("measuring", len(impls), "implementations ->", a.out)
        measure(impls, ns, rhos, a.w_file, a.out, a.min_runs, a.max_runs, a.precision, a.timeout)


if __name__ == "__main__":
    main()
