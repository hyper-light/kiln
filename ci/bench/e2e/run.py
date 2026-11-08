#!/usr/bin/env python3
"""LB14: kiln's end-to-end load-balancer bench against nginx, HAProxy and Envoy.

What it measures: what each system adds between a request (an ask) arriving and the work reaching
the machine that will do it (~/.cache/kiln-lb/research-bench.md, "Fair comparison" and plan step 5).

- Proxies (nginx, HAProxy, Envoy at their pins; their fastest and their load-aware algorithms):
  wrk2 open loop through the proxy to N test backends (e2e-backend: 204 at once), and the same wrk2
  straight to one backend. Added = proxy path - direct path, per percentile, at the same rate.
- kiln (as it is today, and with lb/wire's kernel merged): the generator (src/main.rs) asks a
  runner of a fleet of its own (a kiln server and N + 1 runners in temp dirs on this machine, as
  kiln's tests start them) over its reach socket to run a no-op on one of the N others, open loop,
  timed to Started; against the same no-op spawned directly. Added = kiln path - direct path.

Every latency is taken from each request's intended send time (wrk2 -R; the generator's schedule)
into HdrHistogram-layout histograms; warm-up is discarded (wrk2's calibration period; MSER-5 in the
generator); rates are matched and below both systems' knees, and each system's highest sustained
rate is measured apart. How each number is derived is said where it is computed (DERIVATIONS).

  run.py prepare WHAT...   fetch at the pins and build (wrk2 luajit openssl nginx haproxy envoy e2e kiln)
  run.py smoke             every path once at a low rate, checked
  run.py knee              each system's highest sustained rate (doubling, then bisection)
  run.py sweep             interleaved rounds at matched rates (rotated order per round)

Builds run under `locked.py --shared`, measurements under `locked.py --exclusive` (the lock file
the kernel bench takes too). Results: ~/.cache/kiln-lb/bench-e2e/results/*.jsonl, one JSON object
per run. Standard library only.
"""

import argparse
import contextlib
import datetime
import hashlib
import http.client
import json
import math
import os
import platform
import resource
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
HOME = os.path.expanduser("~")
BENCH = os.path.join(HOME, ".cache", "kiln-lb", "bench-e2e")
SRC = os.path.join(BENCH, "src")
BUILD = os.path.join(BENCH, "build")
BIN = os.path.join(BENCH, "bin")
LOGS = os.path.join(BENCH, "logs")
RESULTS = os.path.join(BENCH, "results")
RUN = os.path.join(BENCH, "run")
KILN_TARGET = os.path.join(BENCH, "target")
LOCK = os.path.join(HOME, ".cache", "kiln-lb", "bench-measure.lock")

# The pins. Competitors as bench-spec.md pins them (the kernel bench builds the same commits);
# wrk2 as the task names it; LuaJIT 2.1 and OpenSSL only to build wrk2 on arm64 (its bundled
# LuaJIT 2.0 has no arm64 port; TLS is never used).
PINS = {
    "wrk2": ("https://github.com/giltene/wrk2.git", "44a94c17d8e6a0bac8559b53da76848e430cb7a7"),
    "luajit": ("https://github.com/LuaJIT/LuaJIT.git", "ea9eb7edb5eac4a9d175d74523db34fac0fa5180"),
    "openssl": ("https://github.com/openssl/openssl.git", "45e844fa2a14ec92d146bd8f5778ac130b6625fb"),
    "nginx": ("https://github.com/nginx/nginx.git", "140aa942711868a8daeb95810bc9a81f272ada72"),
    "haproxy": ("https://github.com/haproxy/haproxy.git", "8872a2604595140544774c2a6309a41186fc0f44"),
}
# Envoy v1.40.2 (b2d89e0): Homebrew's bottle of that release, built with `bazel -c opt`
# (bench-spec.md's build for Envoy); its version string is checked against the pin.
ENVOY = {"version": "1.40.2", "commit": "b2d89e0ae53916f28770771fbdd49c5c99c5c5a2", "tag": "arm64_tahoe"}
# nginx configured as the kernel bench configures it (ci/bench/run.py on lb/bench2): its default -O
# plus -O2, and the modules a proxy doesn't need left out.
NGINX_CONFIGURE = ["--without-pcre", "--without-http_rewrite_module", "--without-http_gzip_module", "--with-cc-opt=-O2"]

# Each competitor's fastest algorithm by its code (notes/competitors.md section 0: HAProxy static-rr,
# nginx random, Envoy random) and its scalable load-aware one (research-bench.md: nginx least_conn's
# scan and random two least_conn, HAProxy leastconn, Envoy least_request's P2C).
ALGOS = {
    "nginx": {"random": "random;", "least_conn": "least_conn;", "random_two_least_conn": "random two least_conn;", "round_robin": ""},
    "haproxy": {"static-rr": "static-rr", "leastconn": "leastconn", "roundrobin": "roundrobin"},
    "envoy": {"random": "RANDOM", "least_request": "LEAST_REQUEST", "round_robin": "ROUND_ROBIN"},
}
DEFAULT_SYSTEMS = ["nginx:random", "nginx:least_conn", "haproxy:static-rr", "haproxy:leastconn", "envoy:random", "envoy:least_request"]

# DERIVATIONS (no picked numbers; each from the user's contract, a cited method or this machine):
# - 95% intervals and the 5% precision target: bench-spec.md ("R grows until the 95% bootstrap
#   interval of each median is within 5% of it, or 30 runs").
# - Load levels rho = 0.5 and 0.9 of the lower knee: bench-spec.md's loads (its rho = 0 is a kernel's
#   empty step, which an open loop can't hold exactly).
# - Samples per run for p99: the median over R runs of a per-run quantile has a rank spread of
#   1.2533 * sqrt(p (1 - p) / n) / sqrt(R); to bring the 95% half-width within 5% of the tail mass
#   (1 - p) with R at its 30-run cap: n >= p / (1 - p) * (1.96 * 1.2533 / (0.05 * sqrt(30)))^2.
# - wrk2's warm-up: its own calibration (CALIBRATE_DELAY_MS = 10 s + 5 ms per connection of a
#   thread), after which it resets its histograms; the generator's: MSER-5 (stats.rs).
# - Connections (wrk2 -c) and generator workers: Little's law on the pilot's slowest answer, so each
#   connection is idle again before its next due time: ceil(rate * max latency) + 1.
# - Sustained (generator): no upward Mann-Kendall trend at 95% and nothing lost; (wrk2) its corrected
#   p50 within its own timer slop of its uncorrected p50, the slop measured on the direct path at a
#   light load (its p100 there).
# - Keep-alive limits, timeouts and pools: each set to outlast a run (a run's requests, its length),
#   so no proxy closes or purges a connection mid-run; file limits: the process's hard limit.
# - Process waits: kiln's own tests' (60 s for a server, 120 s for runners to connect).
Z95 = 1.959963984540054
TARGET = 0.05
RMAX = 30
RHOS = [0.5, 0.9]


def n_per_run(p=0.99):
    return math.ceil(p / (1 - p) * (Z95 * 1.2533 / (TARGET * math.sqrt(RMAX))) ** 2)


def log(*a):
    print(datetime.datetime.now().strftime("%H:%M:%S"), *a, file=sys.stderr, flush=True)


def ncpu():
    return os.cpu_count() or 1


def sh(argv, cwd=None, env=None, logname=None, nice=True, check=True):
    """Runs a build step, niced, its output to logs/<logname>.log (or inherited)."""
    if nice:
        argv = ["nice", "-n", "10"] + list(argv)
    out = None
    if logname:
        os.makedirs(LOGS, exist_ok=True)
        out = open(os.path.join(LOGS, logname + ".log"), "ab")
        out.write(("\n$ (cd %s) %s\n" % (cwd or os.getcwd(), " ".join(argv))).encode())
        out.flush()
    r = subprocess.run(argv, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT if out else None)
    if out:
        out.close()
    if check and r.returncode != 0:
        raise SystemExit("failed (%d): %s; see %s" % (r.returncode, " ".join(argv), os.path.join(LOGS, (logname or "?") + ".log")))
    return r.returncode


def out_of(argv, cwd=None, env=None):
    return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True).stdout.strip()


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


# ---- prepare: sources at their pins, builds -------------------------------------------------------


def fetch(name):
    url, sha = PINS[name]
    d = os.path.join(SRC, name)
    if os.path.isdir(os.path.join(d, ".git")) and out_of(["git", "-C", d, "rev-parse", "HEAD"]) == sha:
        return d
    if os.path.exists(d):
        shutil.rmtree(d)
    os.makedirs(d)
    for argv in (["git", "init", "-q"], ["git", "remote", "add", "origin", url], ["git", "fetch", "-q", "--depth", "1", "origin", sha], ["git", "checkout", "-q", "--detach", "FETCH_HEAD"]):
        sh(argv, cwd=d, logname="fetch-" + name, nice=False)
    got = out_of(["git", "-C", d, "rev-parse", "HEAD"])
    if got != sha:
        raise SystemExit("%s: fetched %s, not the pin %s" % (name, got, sha))
    return d


def fresh_copy(src, dst):
    if os.path.exists(dst):
        shutil.rmtree(dst)
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".git"), symlinks=True)


def macos_version():
    return out_of(["sw_vers", "-productVersion"]) if platform.system() == "Darwin" else ""


def prepare_openssl():
    prefix = os.path.join(BUILD, "openssl")
    if os.path.exists(os.path.join(prefix, "lib", "libcrypto.a")):
        return prefix
    b = os.path.join(BUILD, "openssl-src")
    fresh_copy(fetch("openssl"), b)
    target = "darwin64-arm64-cc" if platform.system() == "Darwin" else "linux-x86_64"
    sh(["./Configure", target, "no-shared", "no-tests", "no-docs", "--prefix=" + prefix, "--libdir=lib"], cwd=b, logname="openssl")
    sh(["make", "-j%d" % ncpu(), "build_libs"], cwd=b, logname="openssl")
    sh(["make", "install_dev"], cwd=b, logname="openssl")
    return prefix


def prepare_wrk2():
    osl = prepare_openssl()
    b = os.path.join(BUILD, "wrk2")
    fresh_copy(fetch("wrk2"), b)
    # LuaJIT 2.1 in place of the bundled 2.0 (no arm64 port), built first on its own so wrk2's
    # variables below never reach its make.
    lj = os.path.join(b, "deps", "luajit")
    fresh_copy(fetch("luajit"), lj)
    env = dict(os.environ)
    if platform.system() == "Darwin":
        env["MACOSX_DEPLOYMENT_TARGET"] = macos_version()
    sh(["make", "-j%d" % ncpu(), "BUILDMODE=static"], cwd=os.path.join(lj, "src"), env=env, logname="wrk2")
    # wrk2's own flags (its Makefile), with OpenSSL's and LuaJIT's paths for this machine.
    cflags = "-std=c99 -Wall -O2 -D_REENTRANT -I%s/include -Ideps/luajit/src" % osl
    libs = "-lluajit -lpthread -lm -L%s/lib -lssl -lcrypto" % osl
    sh(["make", "-j%d" % ncpu(), "CFLAGS=" + cflags, "LIBS=" + libs, "LDFLAGS=-Ldeps/luajit/src"], cwd=b, env=env, logname="wrk2")
    os.makedirs(BIN, exist_ok=True)
    shutil.copy2(os.path.join(b, "wrk"), os.path.join(BIN, "wrk2"))
    return os.path.join(BIN, "wrk2")


def prepare_nginx():
    src = fetch("nginx")
    b = os.path.join(BUILD, "nginx")
    sh(["./auto/configure", "--builddir=" + b, "--prefix=" + os.path.join(BUILD, "nginx-prefix")] + NGINX_CONFIGURE, cwd=src, logname="nginx")
    sh(["make", "-j%d" % ncpu()], cwd=src, logname="nginx")
    os.makedirs(BIN, exist_ok=True)
    shutil.copy2(os.path.join(b, "nginx"), os.path.join(BIN, "nginx"))


def prepare_haproxy():
    b = os.path.join(BUILD, "haproxy")
    fresh_copy(fetch("haproxy"), b)
    target = "osx" if platform.system() == "Darwin" else "linux-glibc"
    sh(["make", "-j%d" % ncpu(), "TARGET=" + target], cwd=b, logname="haproxy")
    os.makedirs(BIN, exist_ok=True)
    shutil.copy2(os.path.join(b, "haproxy"), os.path.join(BIN, "haproxy"))


def prepare_envoy():
    env = dict(os.environ, HOMEBREW_NO_AUTO_UPDATE="1", HOMEBREW_NO_ANALYTICS="1", HOMEBREW_NO_INSTALL_CLEANUP="1")
    brew = shutil.which("brew") or "/opt/homebrew/bin/brew"
    info = json.loads(out_of([brew, "info", "--json=v2", "envoy"], env=env))
    f = info["formulae"][0]
    if f["versions"]["stable"] != ENVOY["version"]:
        raise SystemExit("Homebrew's envoy is %s, not the pin %s" % (f["versions"]["stable"], ENVOY["version"]))
    want = f["bottle"]["stable"]["files"][ENVOY["tag"]]["sha256"]
    sh([brew, "fetch", "--bottle-tag=" + ENVOY["tag"], "envoy"], env=env, logname="envoy", nice=False)
    path = out_of([brew, "--cache", "--bottle-tag=" + ENVOY["tag"], "envoy"], env=env)
    if sha256(path) != want:
        raise SystemExit("envoy bottle %s: sha256 is not Homebrew's %s" % (path, want))
    d = os.path.join(BUILD, "envoy-bottle")
    if os.path.exists(d):
        shutil.rmtree(d)
    os.makedirs(d)
    sh(["tar", "-xzf", path, "-C", d], logname="envoy", nice=False)
    exe = os.path.join(d, "envoy", ENVOY["version"], "bin", "envoy")
    os.makedirs(BIN, exist_ok=True)
    shutil.copy2(exe, os.path.join(BIN, "envoy"))
    v = out_of([os.path.join(BIN, "envoy"), "--version"])
    if ENVOY["commit"] not in v or ENVOY["version"] not in v:
        raise SystemExit("envoy --version says %r, not the pin %s/%s" % (v, ENVOY["commit"], ENVOY["version"]))
    with open(os.path.join(BIN, "envoy.json"), "w") as w:
        json.dump({"bottle": path, "sha256": want, "version": v}, w)


def prepare_e2e():
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(BUILD, "e2e-target"))
    sh(["cargo", "build", "--release"], cwd=HERE, env=env, logname="e2e")
    os.makedirs(BIN, exist_ok=True)
    for b in ("kiln-bench-e2e", "e2e-backend"):
        shutil.copy2(os.path.join(BUILD, "e2e-target", "release", b), os.path.join(BIN, b))


def prepare_kiln(tag):
    """kiln from this tree, as a release build without its UI, kept as bin/kiln-<tag>."""
    env = dict(os.environ, CARGO_TARGET_DIR=KILN_TARGET, KILN_UI_SERVER="off")
    sh(["cargo", "build", "--release", "-p", "kiln"], cwd=REPO, env=env, logname="kiln-" + tag)
    os.makedirs(BIN, exist_ok=True)
    dst = os.path.join(BIN, "kiln-" + tag)
    shutil.copy2(os.path.join(KILN_TARGET, "release", "kiln"), dst)
    meta = {"tag": tag, "commit": out_of(["git", "-C", REPO, "rev-parse", "HEAD"]), "dirty": out_of(["git", "-C", REPO, "status", "--porcelain", "--", "crates"]) != "", "rustc": out_of(["rustc", "--version"]), "sha256": sha256(dst)}
    with open(dst + ".json", "w") as w:
        json.dump(meta, w)
    return dst


def versions():
    v = {}
    for name, argv in (("nginx", [os.path.join(BIN, "nginx"), "-V"]), ("haproxy", [os.path.join(BIN, "haproxy"), "-v"]), ("envoy", [os.path.join(BIN, "envoy"), "--version"]), ("wrk2", [os.path.join(BIN, "wrk2"), "-v"])):
        if os.path.exists(argv[0]):
            r = subprocess.run(argv, capture_output=True, text=True)
            v[name] = (r.stdout + r.stderr).strip().splitlines()[:2]
    for k in sorted(os.listdir(BIN)) if os.path.isdir(BIN) else []:
        if k.startswith("kiln-") and k.endswith(".json"):
            with open(os.path.join(BIN, k)) as f:
                v[k[:-5]] = json.load(f)
    v["pins"] = PINS
    v["envoy_pin"] = ENVOY
    return v


# ---- processes -------------------------------------------------------------------------------------


def raise_nofile():
    """The soft file limit raised to what the process may have: this machine's limit, derived."""
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    cap = hard
    if platform.system() == "Darwin":
        per = int(out_of(["sysctl", "-n", "kern.maxfilesperproc"]) or 0)
        cap = per if hard == resource.RLIM_INFINITY else min(hard, per)
    if cap and cap != resource.RLIM_INFINITY and cap > soft:
        resource.setrlimit(resource.RLIMIT_NOFILE, (cap, hard))
    return resource.getrlimit(resource.RLIMIT_NOFILE)[0]


class Procs:
    """What this run started, each in a process group of its own (its leader's pid recorded), and
    stopped by that pid alone: SIGTERM to its group, then SIGKILL after the grace."""

    def __init__(self, d):
        self.dir = d
        os.makedirs(d, exist_ok=True)
        self.live = []

    def start(self, name, argv, env, cwd=None, pipe=False):
        logf = open(os.path.join(self.dir, name + ".log"), "ab")
        p = subprocess.Popen(argv, env=env, cwd=cwd, stdout=subprocess.PIPE if pipe else logf, stderr=logf, stdin=subprocess.DEVNULL, start_new_session=True)
        with open(os.path.join(self.dir, "pids"), "a") as w:
            w.write("%d %s\n" % (p.pid, name))
        self.live.append((name, p, logf))
        return p

    def stop(self, p, grace=60.0):
        for name, q, logf in list(self.live):
            if q is p:
                self.live.remove((name, q, logf))
                if q.poll() is None:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(q.pid, signal.SIGTERM)
                    try:
                        q.wait(grace)
                    except subprocess.TimeoutExpired:
                        with contextlib.suppress(ProcessLookupError):
                            os.killpg(q.pid, signal.SIGKILL)
                        q.wait()
                logf.close()

    def stop_all(self):
        for _, p, _ in reversed(list(self.live)):
            self.stop(p)


def cpu_of_groups(pgids):
    """CPU seconds (user + system) so far of every process in these process groups."""
    if not pgids:
        return {}
    out = out_of(["ps", "-A", "-o", "pgid=,time="])
    want = set(pgids)
    tot = {g: 0.0 for g in pgids}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        try:
            g = int(parts[0])
        except ValueError:
            continue
        if g in want:
            tot[g] += parse_cputime(parts[1])
    return tot


def parse_cputime(t):
    # ps time: [[dd-]hh:]mm:ss.cc
    days = 0
    if "-" in t:
        d, t = t.split("-", 1)
        days = int(d)
    parts = [float(x) for x in t.split(":")]
    s = 0.0
    for x in parts:
        s = s * 60 + x
    return s + days * 86400


def free_port(udp=False):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_for(what, check, within):
    t0 = time.monotonic()
    while True:
        try:
            got = check()
        except (OSError, ValueError, http.client.HTTPException):
            got = None
        if got:
            return got
        if time.monotonic() - t0 > within:
            raise SystemExit("%s: not within %.0f s" % (what, within))
        time.sleep(0.1)


def http_get(port, path, token=None, timeout=10.0):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    h = {"Authorization": "Bearer " + token} if token else {}
    c.request("GET", path, headers=h)
    r = c.getresponse()
    body = r.read()
    c.close()
    return r.status, body


# ---- the backends and the proxies ------------------------------------------------------------------


def start_backends(procs, n):
    p = procs.start("backends-%d" % n, [os.path.join(BIN, "e2e-backend"), "--n", str(n)], env=dict(os.environ), pipe=True)
    info = json.loads(p.stdout.readline())
    info["proc"] = p
    wait_for("backends", lambda: http_get(info["control"], "/counts")[0] == 200, 60)
    return info


def backend_counts(b):
    st, body = http_get(b["control"], "/counts")
    return json.loads(body)["counts"] if st == 200 else []


def reset_counts(b):
    http_get(b["control"], "/reset")


def cv(xs):
    if not xs:
        return None
    m = sum(xs) / len(xs)
    if m == 0:
        return None
    return math.sqrt(sum((x - m) ** 2 for x in xs) / len(xs)) / m


def proxy_files(impl, algo, n, ports, listen, workers, nofile, conns, keep_s, keep_requests, d):
    """The proxy's configuration and command line. Pools, keep-alives and timeouts are set to
    outlast a run (`keep_s`, `keep_requests`), so a proxy never closes or purges a connection
    mid-run; its file and connection limits are this process's (`nofile`: each proxied request
    holds two descriptors, its client's and its backend's). nginx's cache of idle backend
    connections holds one per backend plus one per client connection (each carries at most one
    request at a time): nothing is evicted in a run."""
    os.makedirs(d, exist_ok=True)
    if impl == "nginx":
        # nginx opens <prefix>/logs/error.log before it reads its configuration.
        os.makedirs(os.path.join(d, "logs"), exist_ok=True)
        servers = "".join("    server 127.0.0.1:%d;\n" % p for p in ports)
        cfg = (
            "worker_processes %d;\nworker_rlimit_nofile %d;\ndaemon off;\nmaster_process on;\npid %s/nginx.pid;\nerror_log %s/nginx-error.log warn;\n"
            "events { worker_connections %d; }\n"
            "http {\n  access_log off;\n  keepalive_timeout %ds;\n  keepalive_requests %d;\n"
            "  upstream be {\n    %s\n%s    keepalive %d;\n    keepalive_timeout %ds;\n    keepalive_requests %d;\n  }\n"
            "  server {\n    listen 127.0.0.1:%d;\n    location / {\n      proxy_pass http://be;\n      proxy_http_version 1.1;\n      proxy_set_header Connection \"\";\n    }\n  }\n}\n"
        ) % (workers, nofile, d, d, nofile, keep_s, keep_requests, ALGOS["nginx"][algo], servers, n + conns, keep_s, keep_requests, listen)
        path = os.path.join(d, "nginx.conf")
        with open(path, "w") as w:
            w.write(cfg)
        return [os.path.join(BIN, "nginx"), "-p", d, "-c", path], cfg
    if impl == "haproxy":
        servers = "".join("  server s%d 127.0.0.1:%d\n" % (i, p) for i, p in enumerate(ports))
        cfg = (
            "global\n  nbthread %d\n  maxconn %d\n\n"
            "defaults\n  mode http\n  option http-keep-alive\n  http-reuse always\n"
            "  timeout connect %ds\n  timeout client %ds\n  timeout server %ds\n  timeout http-keep-alive %ds\n\n"
            "frontend fe\n  bind 127.0.0.1:%d\n  default_backend be\n\n"
            "backend be\n  balance %s\n  default-server pool-purge-delay %ds\n%s"
        ) % (workers, nofile // 2, keep_s, keep_s, keep_s, keep_s, listen, ALGOS["haproxy"][algo], keep_s, servers)
        path = os.path.join(d, "haproxy.cfg")
        with open(path, "w") as w:
            w.write(cfg)
        return [os.path.join(BIN, "haproxy"), "-db", "-f", path], cfg
    if impl == "envoy":
        admin = free_port()
        eps = "".join("            - endpoint: {address: {socket_address: {address: 127.0.0.1, port_value: %d}}}\n" % p for p in ports)
        big = nofile
        cfg = (
            "admin:\n  address: {socket_address: {address: 127.0.0.1, port_value: %d}}\n"
            "static_resources:\n  listeners:\n  - name: l\n    address: {socket_address: {address: 127.0.0.1, port_value: %d}}\n"
            "    filter_chains:\n    - filters:\n      - name: envoy.filters.network.http_connection_manager\n        typed_config:\n"
            "          \"@type\": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager\n"
            "          stat_prefix: e2e\n          common_http_protocol_options: {idle_timeout: %ds, max_requests_per_connection: %d}\n"
            "          route_config:\n            virtual_hosts:\n            - name: all\n              domains: [\"*\"]\n              routes:\n"
            "              - match: {prefix: \"/\"}\n                route: {cluster: be, timeout: %ds}\n"
            "          http_filters:\n          - name: envoy.filters.http.router\n            typed_config:\n"
            "              \"@type\": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router\n"
            "  clusters:\n  - name: be\n    type: STATIC\n    lb_policy: %s\n    connect_timeout: %ds\n"
            "    typed_extension_protocol_options:\n      envoy.extensions.upstreams.http.v3.HttpProtocolOptions:\n"
            "        \"@type\": type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions\n"
            "        common_http_protocol_options: {idle_timeout: %ds, max_requests_per_connection: %d}\n"
            "        explicit_http_config: {http_protocol_options: {}}\n"
            "    circuit_breakers:\n      thresholds:\n      - {max_connections: %d, max_pending_requests: %d, max_requests: %d, max_retries: %d}\n"
            "    load_assignment:\n      cluster_name: be\n      endpoints:\n      - lb_endpoints:\n%s"
        ) % (admin, listen, keep_s, keep_requests, keep_s, ALGOS["envoy"][algo], keep_s, keep_s, keep_requests, big, big, big, big, eps)
        path = os.path.join(d, "envoy.yaml")
        with open(path, "w") as w:
            w.write(cfg)
        return [os.path.join(BIN, "envoy"), "-c", path, "--concurrency", str(workers), "--log-level", "warn", "--disable-hot-restart"], cfg
    raise SystemExit("no proxy " + impl)


def start_proxy(procs, impl, algo, n, backends, workers, nofile, conns, keep_s, keep_requests, tag):
    listen = free_port()
    d = os.path.join(procs.dir, "%s-%s-%d-%s" % (impl, algo, n, tag))
    argv, _ = proxy_files(impl, algo, n, backends["ports"][:n], listen, workers, nofile, conns, keep_s, keep_requests, d)
    p = procs.start("%s-%s-%d-%s" % (impl, algo, n, tag), argv, env=dict(os.environ), cwd=d)

    def up():
        if p.poll() is not None:
            raise SystemExit("%s exited (%s); see %s" % (impl, p.returncode, procs.dir))
        return http_get(listen, "/", timeout=2.0)[0] == 204

    wait_for(impl + " up", up, 60)
    return p, listen


# ---- wrk2 ------------------------------------------------------------------------------------------

DONE_LUA = """
done = function(summary, latency, requests)
  io.write(string.format("E2E-CORRECTED n=%d min=%d max=%d mean=%.3f stdev=%.3f", #latency, latency.min, latency.max, latency.mean, latency.stdev))
  for _, p in ipairs({50, 90, 99, 99.9, 99.99}) do
    io.write(string.format(" p%s=%d", tostring(p), latency:percentile(p)))
  end
  local e = summary.errors
  io.write(string.format(" total=%d duration_us=%d connect=%d read=%d write=%d status=%d timeout=%d\\n", summary.requests, summary.duration, e.connect, e.read, e.write, e.status, e.timeout))
end
"""


def parse_units_us(s):
    for unit, k in (("us", 1.0), ("ms", 1e3), ("s", 1e6), ("m", 60e6), ("h", 3600e6)):
        if s.endswith(unit):
            return float(s[: -len(unit)]) * k
    return float(s)


def parse_wrk2(text):
    out = {"corrected": {}, "uncorrected": {}}
    for line in text.splitlines():
        if line.startswith("E2E-CORRECTED"):
            for kv in line.split()[1:]:
                k, v = kv.split("=")
                out["corrected"][k] = float(v)
    # The uncorrected section: its headline percentiles (exact, printed in us/ms) and total count.
    sec = text.split("Uncorrected Latency", 1)
    if len(sec) == 2:
        body = sec[1]
        for line in body.splitlines():
            line = line.strip()
            if line.endswith(("us", "ms", "s")) and "%" in line:
                p, v = line.split("%", 1)
                out["uncorrected"]["p" + ("%g" % float(p))] = parse_units_us(v.strip())
            if line.startswith("#[Max"):
                parts = line.replace("#[", "").replace("]", "").split(",")
                for part in parts:
                    k, v = [x.strip() for x in part.split("=")]
                    if k == "Max":
                        out["uncorrected"]["max"] = float(v) * 1000.0
                    if k == "Total count":
                        out["uncorrected"]["n"] = float(v)
            if line.startswith("#[Mean"):
                parts = line.replace("#[", "").replace("]", "").split(",")
                for part in parts:
                    k, v = [x.strip() for x in part.split("=")]
                    if k == "Mean":
                        out["uncorrected"]["mean"] = float(v) * 1000.0
            if line.startswith("----"):
                break
    for line in text.splitlines():
        if line.startswith("Requests/sec:"):
            out["requests_per_s"] = float(line.split()[1])
    return out


def wrk2_calibration(conns, threads):
    """wrk2's warm-up, discarded: CALIBRATE_DELAY_MS (10 s) plus 5 ms per connection of a thread
    (src/wrk.c thread_main), after which it resets its histograms."""
    return 10.0 + 0.005 * math.ceil(max(conns, threads) / threads)


def wrk2_span(window, conns, threads):
    """A wrk2 run's whole length: -d takes whole seconds."""
    return math.ceil(wrk2_calibration(conns, threads) + window)


def wrk2_run(url, rate, window, conns, threads, logname):
    conns = max(conns, threads)
    calibrate = wrk2_calibration(conns, threads)
    d = wrk2_span(window, conns, threads)
    lua = os.path.join(RUN, "done.lua")
    os.makedirs(RUN, exist_ok=True)
    with open(lua, "w") as w:
        w.write(DONE_LUA)
    argv = [os.path.join(BIN, "wrk2"), "-t%d" % threads, "-c%d" % conns, "-d%ds" % d, "-R%d" % round(rate), "-L", "-U", "-s", lua, url]
    r = subprocess.run(argv, capture_output=True, text=True)
    with open(os.path.join(LOGS, logname + ".log"), "a") as w:
        w.write("$ %s\n%s\n%s\n" % (" ".join(argv), r.stdout, r.stderr))
    got = parse_wrk2(r.stdout)
    c, u = got["corrected"], got["uncorrected"]
    got["window_s"] = d - calibrate
    got["threads"], got["conns"], got["duration_s"] = threads, conns, d
    got["achieved_per_s"] = c.get("n", 0) / got["window_s"] if got["window_s"] > 0 else 0
    got["ok"] = r.returncode == 0 and "p50" in c and "p50" in u
    got["errors"] = {k: c.get(k, 0) for k in ("connect", "read", "write", "status", "timeout")}
    got["lat"] = {"p50_us": c.get("p50"), "p90_us": c.get("p90"), "p99_us": c.get("p99"), "p999_us": c.get("p99.9"), "max_us": c.get("max"), "mean_us": c.get("mean"), "n": c.get("n")}
    got["lat_u"] = {"p50_us": u.get("p50"), "p90_us": u.get("p90"), "p99_us": u.get("p99"), "p999_us": u.get("p99.9"), "max_us": u.get("max"), "mean_us": u.get("mean"), "n": u.get("n")}
    return got


# ---- the generator -----------------------------------------------------------------------------------


def gen_run(args, rate, seconds, workers, label):
    argv = [os.path.join(BIN, "kiln-bench-e2e")] + args + ["--rate", "%g" % rate, "--seconds", "%g" % seconds, "--workers", str(workers), "--label", label]
    r = subprocess.run(argv, capture_output=True, text=True)
    lines = [l for l in r.stdout.splitlines() if l.startswith("{")]
    if r.returncode != 0 or not lines:
        raise SystemExit("generator failed (%d): %s %s" % (r.returncode, " ".join(argv), r.stderr[-2000:]))
    g = json.loads(lines[-1])
    us = lambda v: None if v is None else v / 1000.0
    s, ss = g["started"], g["since_sent"]
    g["lat"] = {"p50_us": us(s["p50_ns"]), "p90_us": us(s["p90_ns"]), "p99_us": us(s["p99_ns"]), "p999_us": us(s["p999_ns"]), "max_us": us(s["max_ns"]), "mean_us": us(s["mean_ns"]), "n": s["n"], "p50_ci_us": [us(x) for x in s["p50_ci_ns"]], "p99_ci_us": [us(x) for x in s["p99_ci_ns"]]}
    g["lat_u"] = {"p50_us": us(ss["p50_ns"]), "p90_us": us(ss["p90_ns"]), "p99_us": us(ss["p99_ns"]), "p999_us": us(ss["p999_ns"]), "max_us": us(ss["max_ns"]), "mean_us": us(ss["mean_ns"]), "n": ss["n"]}
    g["ok"] = g["ok"] > 0
    return g


# ---- the kiln fleet ----------------------------------------------------------------------------------


def machine_id(name):
    """kiln's key for a runner started with KILN_MACHINE_ID=name (crates/kiln/src/os/mod.rs, machine_id_of)."""
    return hashlib.sha256(("kiln machine id\n" + name.strip().lower()).encode()).hexdigest()[:32]


def reach_socket(name, state_dir, tmpdir):
    """Its reach socket: `reach::local::path` (the harness socket's name with reach- for harness-,
    tagged by its state directory's SHA-256) in the runtime directory under TMPDIR."""
    safe = "".join(c if (c.isalnum() and c.isascii()) or c in "-_" else "_" for c in name)[:32]
    tag = hashlib.sha256(state_dir.encode()).hexdigest()[:8]
    return os.path.join(tmpdir, "kiln-%d" % os.getuid(), "reach-%s-%s.sock" % (safe, tag))


def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def read_varint(b, i):
    n, shift = 0, 0
    while True:
        c = b[i]
        i += 1
        n |= (c & 0x7F) << shift
        shift += 7
        if not c & 0x80:
            return n, i


def reach_runners(sock):
    """`Req::Runners { want: None }` on a reach socket (postcard: variant 1, None): the JSON of its
    `Ev::Runners`, or None."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(30)
    s.connect(sock)
    body = varint(1) + b"\x00"
    s.sendall(struct.pack(">I", len(body)) + body)
    buf = b""
    while True:
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
        if len(buf) >= 4:
            (ln,) = struct.unpack(">I", buf[:4])
            if len(buf) >= 4 + ln:
                break
    s.close()
    (ln,) = struct.unpack(">I", buf[:4])
    msg = buf[4 : 4 + ln]
    v, i = read_varint(msg, 0)
    if v != 1:
        return None
    n, i = read_varint(msg, i)
    return json.loads(msg[i : i + n].decode())


def kiln_env(home, tmpdir, machine):
    """A kiln process's environment: clean (none of this machine's KILN_* variables, as
    `tests/common::kiln()` strips them), its own home, temp dir and machine id, and no receiver on
    the machine's OTLP or Datadog ports (the installed runner's)."""
    os.makedirs(home, exist_ok=True)
    os.makedirs(tmpdir, exist_ok=True)
    return {
        "HOME": home,
        "TMPDIR": tmpdir,
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "LANG": "C.UTF-8",
        "NO_COLOR": "1",
        "KILN_MACHINE_ID": machine,
        "KILN_WARM_FLOOR": "0",
        "KILN_OTLP_ADDR": "off",
        "KILN_OTLP_GRPC_ADDR": "off",
        "KILN_DD_TRACE_ADDR": "off",
    }


SERVER_TOML = """[server]
http_addr = "127.0.0.1:{http}"
quic_addr = "127.0.0.1:{quic}"
public_url = "http://127.0.0.1:{http}"
quic_advertise = "127.0.0.1:{quic}"
data_dir = "{data}"
https = false

[gitops]
auto_pipelines = false

[local_agent]
enabled = false

[kubernetes]
enabled = false

[terminal]
enabled = false

[discovery]
enabled = false
announce = false
browse = false
join_requests = false
tailscale = false

[cd]
enabled = false

[ci]
enabled = true
signatures = "warn"

[ci.scheduler]
headroom = false
"""


class Fleet:
    """A kiln server and N + 1 runners on this machine, each a machine of its own (KILN_MACHINE_ID),
    with homes, state and a temp dir of their own under `d`, as kiln's tests start them
    (crates/kiln/tests/common/mod.rs `start_once`'s configuration; tests/common/agents.rs's runners).
    The runner `asker` takes the asks; `d000`... are the N that may do them."""

    def __init__(self, procs, kiln, n, d):
        self.procs, self.kiln, self.n, self.d = procs, kiln, n, d
        # Sockets live under TMPDIR, and a unix socket's path holds 104 bytes on macOS: a short one.
        self.tmp = os.path.join(BENCH, "t", hashlib.sha256(d.encode()).hexdigest()[:8])
        os.makedirs(self.tmp, exist_ok=True)
        self.http, self.quic = free_port(), free_port(udp=True)
        data = os.path.join(d, "server", "data")
        os.makedirs(data, exist_ok=True)
        cfg = os.path.join(d, "server", "kiln.toml")
        with open(cfg, "w") as w:
            w.write(SERVER_TOML.format(http=self.http, quic=self.quic, data=data))
        env = kiln_env(os.path.join(d, "server", "home"), self.tmp, "server " + d)
        self.server = procs.start("kiln-server-n%d" % n, [kiln, "--config", cfg, "server"], env=env, cwd=os.path.join(d, "server"))
        tok = os.path.join(data, "admin.token")
        wait_for("kiln server", lambda: os.path.exists(tok) and http_get(self.http, "/healthz")[0] == 200, 60)
        self.admin = open(tok).read().strip()
        join = open(os.path.join(data, "join.token")).read().strip()
        self.names = ["asker"] + ["d%03d" % i for i in range(n)]
        self.runners = {}
        for name in self.names:
            r = os.path.join(d, name)
            env = kiln_env(os.path.join(r, "home"), self.tmp, name)
            argv = [kiln, "agent", "--server", "http://127.0.0.1:%d" % self.http, "--token", join, "--name", name, "--work-dir", os.path.join(r, "work"), "--state-dir", os.path.join(r, "state")]
            self.runners[name] = procs.start("kiln-%s-n%d" % (name, n), argv, env=env, cwd=r)
        ids = {machine_id(x): x for x in self.names}

        def connected():
            st, body = http_get(self.http, "/api/agents", self.admin)
            v = json.loads(body)
            lst = v if isinstance(v, list) else v.get("agents", [])
            on = {a.get("name") for a in lst if a.get("connected")}
            return all(i in on for i in ids)

        wait_for("%d runners connected" % len(self.names), connected, 120 + 2 * n)
        self.sock = reach_socket("asker", os.path.join(d, "asker", "state"), self.tmp)
        wait_for("the asker's reach socket " + self.sock, lambda: os.path.exists(self.sock), 60)
        doers = {machine_id(x) for x in self.names[1:]}

        def ready():
            v = reach_runners(self.sock)
            if not v:
                return False
            seen = {c["runner"]["name"] for c in v.get("runners", [])}
            return doers <= seen

        wait_for("the asker's directory listing every doer", ready, 120 + 2 * n)
        self.target = "-" + machine_id("asker")

    def pgids(self):
        return [self.server.pid] + [p.pid for p in self.runners.values()]

    def stop(self):
        for p in list(self.runners.values()):
            self.procs.stop(p, grace=60.0)
        self.procs.stop(self.server, grace=60.0)


# ---- one measured run ------------------------------------------------------------------------------


def machine():
    m = {"host": socket.gethostname(), "os": platform.platform(), "cpus": ncpu()}
    if platform.system() == "Darwin":
        m["model"] = out_of(["sysctl", "-n", "machdep.cpu.brand_string"])
        m["perf_cores"] = out_of(["sysctl", "-n", "hw.perflevel0.physicalcpu"])
        m["eff_cores"] = out_of(["sysctl", "-n", "hw.perflevel1.physicalcpu"])
        m["mem"] = out_of(["sysctl", "-n", "hw.memsize"])
    return m


def record(fh, row):
    fh.write(json.dumps(row, sort_keys=True) + "\n")
    fh.flush()


def measure_one(ctx, system, n, rate, window, rnd, order):
    """One run, or what kept it from going (recorded, and the sweep goes on)."""
    try:
        return measure(ctx, system, n, rate, window, rnd, order)
    except (Exception, SystemExit) as e:
        impl, _, algo = system.partition(":")
        log("run failed:", system, "n=%d rate=%g:" % (n, rate), e)
        return {"kind": "run", "ok": False, "sweep": ctx["sweep"], "round": rnd, "order": order, "impl": impl, "algo": algo or None, "n": n, "rate": rate, "error": str(e), "lat": {}, "lat_u": {}, "load_after": list(os.getloadavg()), "t_end": time.time()}


def measure(ctx, system, n, rate, window, rnd, order):
    """One run of `system` (impl:algo, or direct, kiln, spawn) at `rate` over `window` seconds."""
    impl, _, algo = system.partition(":")
    row = {"kind": "run", "sweep": ctx["sweep"], "round": rnd, "order": order, "impl": impl, "algo": algo or None, "n": n, "rate": rate, "window_s": window, "load_before": list(os.getloadavg()), "t_start": time.time()}
    b = ctx["backends"]
    if impl in ("nginx", "haproxy", "envoy", "direct"):
        conns = ctx["conns"](rate)
        # The run's whole length (wrk2's calibration and the window): no keep-alive or pool ends in it.
        span = wrk2_span(window, conns, ctx["wrk2_threads"])
        keep_s = math.ceil(span) + 1
        keep_requests = math.ceil(rate * span) + 1
        proc, port = (None, b["ports"][0])
        if impl != "direct":
            proc, port = start_proxy(ctx["procs"], impl, algo, n, b, ctx["proxy_workers"], ctx["nofile"], conns, keep_s, keep_requests, "r%d" % rnd)
        groups = [b["proc"].pid] + ([proc.pid] if proc else [])
        reset_counts(b)
        cpu0 = cpu_of_groups(groups)
        got = wrk2_run("http://127.0.0.1:%d/" % port, rate, window, conns, ctx["wrk2_threads"], "wrk2-%s" % ctx["sweep"])
        cpu1 = cpu_of_groups(groups)
        counts = backend_counts(b)[: (n if impl != "direct" else 1)]
        if proc:
            ctx["procs"].stop(proc, grace=60.0)
        row.update({"tool": "wrk2", "path": "direct" if impl == "direct" else "proxy", "workers": ctx["proxy_workers"], "lat": got["lat"], "lat_u": got["lat_u"], "achieved_per_s": got["achieved_per_s"], "errors": got["errors"], "ok": got["ok"], "threads": got["threads"], "conns": got["conns"], "duration_s": got["duration_s"], "window_s": got["window_s"]})
        row["quality"] = {"counts": counts, "cv": cv(counts), "backends": len(counts)}
        row["cpu_s"] = {"proxy": (cpu1.get(proc.pid, 0) - cpu0.get(proc.pid, 0)) if proc else 0.0, "backend": cpu1.get(b["proc"].pid, 0) - cpu0.get(b["proc"].pid, 0)}
    elif impl in ("kiln", "spawn"):
        fleet = ctx["fleets"].get(n) if impl == "kiln" else None
        workers = ctx["workers"](rate)
        groups = fleet.pgids() if fleet else []
        cpu0 = cpu_of_groups(groups)
        if impl == "kiln":
            got = gen_run(["reach", "--socket", fleet.sock, "--target", fleet.target], rate, window, workers, "%s-n%d" % (ctx["kiln_tag"], n))
        else:
            got = gen_run(["spawn"], rate, window, workers, "spawn")
        cpu1 = cpu_of_groups(groups)
        picks = got.get("picks", {})
        row.update({"tool": "gen", "path": "kiln" if impl == "kiln" else "direct", "kiln": ctx.get("kiln_tag") if impl == "kiln" else None, "workers": workers, "lat": got["lat"], "lat_u": got["lat_u"], "answer": got["answer"], "late": got["late"], "achieved_per_s": got["achieved_per_s"], "ok": got["ok"], "sustained": got["sustained"], "trend": got["trend"], "warmup": got["warmup"], "refused": got["refused"], "failed": got["failed"], "why": got["why"], "window_s": got["window_s"]})
        counts = list(picks.values())
        row["quality"] = {"picks": picks, "cv": cv(counts + [0] * max(0, (n - len(counts)) if impl == "kiln" else 0)), "runners_used": len(counts)}
        if fleet:
            server = fleet.server.pid
            row["cpu_s"] = {"server": cpu1.get(server, 0) - cpu0.get(server, 0), "runners": sum(cpu1.get(g, 0) - cpu0.get(g, 0) for g in groups if g != server)}
    else:
        raise SystemExit("no system " + system)
    row["load_after"] = list(os.getloadavg())
    row["t_end"] = time.time()
    return row


# ---- commands -------------------------------------------------------------------------------------


def cmd_prepare(a):
    os.makedirs(BENCH, exist_ok=True)
    for what in a.what:
        log("prepare", what)
        if what == "openssl":
            prepare_openssl()
        elif what == "wrk2":
            prepare_wrk2()
        elif what == "nginx":
            prepare_nginx()
        elif what == "haproxy":
            prepare_haproxy()
        elif what == "envoy":
            prepare_envoy()
        elif what == "e2e":
            prepare_e2e()
        elif what.startswith("kiln-"):
            prepare_kiln(what[len("kiln-") :])
        else:
            raise SystemExit("prepare what? " + what)
    print(json.dumps(versions(), indent=1, default=str))


def context(a, sweep):
    nofile = raise_nofile()
    procs = Procs(os.path.join(RUN, sweep))
    ctx = {"sweep": sweep, "procs": procs, "nofile": nofile, "proxy_workers": a.proxy_workers, "wrk2_threads": a.wrk2_threads, "fleets": {}, "kiln_tag": a.kiln}
    # Little's law on the slowest answer seen in the pilot (`--max-latency-ms`): enough connections or
    # workers that each is idle again before its next due time.
    ctx["conns"] = lambda rate: max(a.wrk2_threads, math.ceil(rate * a.max_latency_ms / 1000.0) + 1)
    ctx["workers"] = lambda rate: math.ceil(rate * a.max_latency_ms / 1000.0) + 1
    return ctx


def setup(ctx, a, ns, need_kiln):
    ctx["backends"] = start_backends(ctx["procs"], max(ns))
    if need_kiln:
        kiln = os.path.join(BIN, "kiln-" + a.kiln)
        for n in ns:
            log("fleet: a server and %d runners" % (n + 1))
            ctx["fleets"][n] = Fleet(ctx["procs"], kiln, n, os.path.join(ctx["procs"].dir, "fleet-n%d" % n))


def cmd_smoke(a):
    sweep = "smoke-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    ctx = context(a, sweep)
    systems = a.systems.split(",")
    ns = [int(x) for x in a.n.split(",")]
    os.makedirs(RESULTS, exist_ok=True)
    try:
        setup(ctx, a, ns, any(s.startswith("kiln") for s in systems))
        with open(os.path.join(RESULTS, sweep + ".jsonl"), "a") as fh:
            record(fh, {"kind": "meta", "sweep": sweep, "machine": machine(), "versions": versions(), "args": vars(a)})
            for n in ns:
                for k, s in enumerate(systems):
                    row = measure_one(ctx, s, n, a.rate, a.window, 0, k)
                    record(fh, row)
                    log(s, "n=%d" % n, "p50=%s p99=%s max=%s (from send p50=%s max=%s) ok=%s achieved=%.1f %s" % (row["lat"].get("p50_us"), row["lat"].get("p99_us"), row["lat"].get("max_us"), row["lat_u"].get("p50_us"), row["lat_u"].get("max_us"), row.get("ok"), row.get("achieved_per_s") or 0, row.get("error", "")))
    finally:
        for f in ctx["fleets"].values():
            f.stop()
        ctx["procs"].stop_all()


def sustained(row, slop_us):
    if not row.get("ok"):
        return False
    if row.get("tool") == "gen":
        return bool(row.get("sustained"))
    c, u = row["lat"].get("p50_us"), row["lat_u"].get("p50_us")
    if c is None or u is None or not row.get("ok"):
        return False
    lost = sum(row.get("errors", {}).values())
    return lost == 0 and (c - u) <= slop_us


def cmd_knee(a):
    """Each system's highest sustained rate: double from `--from` until a run isn't sustained, then
    bisect between the last sustained rate and the first that wasn't until they are within the 5%
    precision target. Each run long enough for `n_per_run()` samples at its rate."""
    sweep = "knee-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    ctx = context(a, sweep)
    systems = a.systems.split(",")
    ns = [int(x) for x in a.n.split(",")]
    os.makedirs(RESULTS, exist_ok=True)
    try:
        setup(ctx, a, ns, any(s.startswith("kiln") for s in systems))
        with open(os.path.join(RESULTS, sweep + ".jsonl"), "a") as fh:
            record(fh, {"kind": "meta", "sweep": sweep, "machine": machine(), "versions": versions(), "args": vars(a), "n_per_run": n_per_run()})
            for n in ns:
                for s in systems:
                    lo, hi, rate = None, None, a.start
                    while True:
                        window = max(a.min_window, n_per_run() / rate)
                        row = measure_one(ctx, s, n, rate, window, 0, 0)
                        row["kind"] = "knee-step"
                        ok = sustained(row, a.slop_us)
                        row["sustained_judged"] = ok
                        record(fh, row)
                        log(s, "n=%d rate=%.0f sustained=%s p50=%s achieved=%.0f" % (n, rate, ok, row["lat"].get("p50_us"), row.get("achieved_per_s") or 0))
                        if ok:
                            lo = rate
                        else:
                            hi = rate
                        if hi is None:
                            rate *= 2
                        elif lo is None:
                            rate /= 2
                            if rate < 1:
                                break
                        elif (hi - lo) / lo <= TARGET:
                            break
                        else:
                            rate = (lo + hi) / 2
                    record(fh, {"kind": "knee", "sweep": sweep, "impl": s, "n": n, "sustained": lo, "not_sustained": hi})
                    log("KNEE", s, "n=%d" % n, "sustained", lo, "not", hi)
    finally:
        for f in ctx["fleets"].values():
            f.stop()
        ctx["procs"].stop_all()


def cmd_sweep(a):
    """Interleaved rounds at matched rates: round r runs every system once at each (N, rate), in an
    order rotated by r, so drift falls on all alike; rounds go on until every system's median p50
    and p99 has a 95% bootstrap interval within 5% of it (analyze.py's rule), or `--rounds`."""
    sweep = a.name or ("sweep-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S"))
    ctx = context(a, sweep)
    systems = a.systems.split(",")
    plan = json.loads(a.plan)  # {"n": {"rate": [...]}} or [[n, rate], ...]
    cells = [(int(n), float(r)) for n, r in plan]
    ns = sorted({n for n, _ in cells})
    os.makedirs(RESULTS, exist_ok=True)
    try:
        setup(ctx, a, ns, any(s.startswith("kiln") for s in systems))
        with open(os.path.join(RESULTS, sweep + ".jsonl"), "a") as fh:
            record(fh, {"kind": "meta", "sweep": sweep, "machine": machine(), "versions": versions(), "args": vars(a), "n_per_run": n_per_run(), "cells": cells, "systems": systems})
            for rnd in range(a.first_round, a.first_round + a.rounds):
                for n, rate in cells:
                    order = systems[rnd % len(systems) :] + systems[: rnd % len(systems)]
                    window = max(a.min_window, n_per_run() / rate)
                    for k, s in enumerate(order):
                        row = measure_one(ctx, s, n, rate, window, rnd, k)
                        record(fh, row)
                        log("round %d" % rnd, s, "n=%d rate=%g" % (n, rate), "p50=%.1f p99=%.1f us" % (row["lat"].get("p50_us") or -1, row["lat"].get("p99_us") or -1))
                # bench-spec.md's rule: rounds stop once every median is within 5% at 95%.
                sys.path.insert(0, HERE)
                import analyze

                done, worst = analyze.converged(analyze.load([os.path.join(RESULTS, sweep + ".jsonl")]))
                log("round %d: converged=%s; widest %s" % (rnd, done, sorted([w for w in worst if w[3] is not None], key=lambda w: -w[3])[:3]))
                if done:
                    break
    finally:
        for f in ctx["fleets"].values():
            f.stop()
        ctx["procs"].stop_all()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("what", nargs="+")
    for name in ("smoke", "knee", "sweep"):
        q = sub.add_parser(name)
        q.add_argument("--systems", default="direct," + ",".join(DEFAULT_SYSTEMS) + ",spawn,kiln")
        q.add_argument("--n", default="10")
        q.add_argument("--kiln", default="before", help="which bin/kiln-<tag> the fleet runs")
        q.add_argument("--proxy-workers", type=int, default=1)
        q.add_argument("--wrk2-threads", type=int, default=1)
        # smoke checks the paths; knee and sweep size connections and workers from the smoke's slowest answer.
        q.add_argument("--max-latency-ms", type=float, required=name != "smoke", default=1000.0 if name == "smoke" else None, help="the pilot's slowest answer (Little's law for connections and workers)")
        q.add_argument("--min-window", type=float, default=0.0, help="a floor under each run's measured window, in seconds (0: n_per_run() / rate alone)")
        q.add_argument("--slop-us", type=float, default=0.0, help="wrk2's timer slop, measured on the direct path (its corrected minus uncorrected p100)")
        if name == "smoke":
            q.add_argument("--rate", type=float, default=100.0)
            q.add_argument("--window", type=float, default=10.0)
        if name == "knee":
            q.add_argument("--start", type=float, required=True)
        if name == "sweep":
            q.add_argument("--plan", required=True, help='JSON [[n, rate], ...]')
            q.add_argument("--rounds", type=int, default=RMAX)
            q.add_argument("--first-round", type=int, default=0)
            q.add_argument("--name", default="")
    a = ap.parse_args()
    {"prepare": cmd_prepare, "smoke": cmd_smoke, "knee": cmd_knee, "sweep": cmd_sweep}[a.cmd](a)


if __name__ == "__main__":
    main()
