# kiln's placement against nginx, HAProxy and Envoy (LB00, LB14)

The person's bar: "an order of magnitude faster than nginx, envoy, or any other OSS competition".
This folder holds the benches that decide whether kiln's placement (`crates/kiln-ci/src/place`)
meets it, on the method of `~/.cache/kiln-lb/notes/research-bench.md`: each competitor's own
selection code, built from its own source at a pinned commit, under one trace; quality beside
speed; bootstrap intervals; failures published beside passes. Results: `results/<date>.md`.

## The competitors, pinned

| who | version | commit | built how | algorithms |
|---|---|---|---|---|
| nginx | release-1.31.8 (mainline) | `140aa942711868a8daeb95810bc9a81f272ada72` (github.com/nginx/nginx) | `auto/configure --with-cc-opt='-O2 -g'` (gcc, as distributions build it), `make` | `round_robin` (smooth weighted RR, O(N)), `least_conn` (O(N) scan), `random_two_least_conn` (`random two least_conn`) |
| HAProxy | v3.4.7 (stable) | `8872a2604595140544774c2a6309a41186fc0f44` (git.haproxy.org haproxy-3.4.git) | `make TARGET=linux-glibc` (its default -O2, threads on) | `leastconn` (eb32 tree), `roundrobin` (fwrr), `random` (`random(2)`, chash tree) |
| Envoy | v1.40.2 | `b2d89e0ae53916f28770771fbdd49c5c99c5c5a2` (github.com/envoyproxy/envoy) | bazel 8.8.1 (bazelisk), `-c opt`, Envoy's hermetic LLVM toolchain | `least_request` (P2C, choice_count 2: its default), `least_request_full_scan`, `round_robin`, `random`; plus its stock `*_lb_benchmark` and `scheduler_benchmark` (EDF) |
| wrk2 | master | `44a94c17d8e6a0bac8559b53da76848e430cb7a7` (github.com/giltene/wrk2) | not yet built | the end-to-end comparison's generator for the proxies |

Sources live in `~/.cache/kiln-lb/bench/src/<name>`; each build script refuses a tree at another
commit. Binaries go to `~/.cache/kiln-lb/bench/bin`.

## The machine

Recorded with every sweep (`run.py` writes it as the first line of `kernels.jsonl`; the results
page repeats it). On `kiln-a094a12d` (hostname `fedora`; never adacachy, production's):

- Intel Core i9-14900HX: 8 P-cores with SMT (CPUs 0-15), 16 E-cores (16-31); one socket, one NUMA node, 36 MiB L3.
- Governor `powersave` under `intel_pstate` active (HWP), EPP `balance_performance`, turbo on (5.6 GHz max on P-cores), SMT on.
  None of these can be changed without root: they are recorded, not set.
- Every kernel runs pinned to one P-core thread (`taskset -c 14`, sibling 15), one at a time.
- **The machine is shared.** The swarm's builds and agents run beside the benches (load average ~30 on 32 CPUs while the first sweeps ran). This is the largest caveat on every number here: it inflates p99s and widens intervals, for every competitor alike (runs are interleaved, see below), but it is not a quiet machine.

## The trace (`drivers/common.h`, the same in Rust in `crates/kiln-ci/benches/lb_vs.rs`)

- N backends (runners) of C slots (CPUs) each; K = round(rho·N·C) requests in flight.
- Each step: one in-flight request, chosen uniformly at random by a seeded splitmix64, ends (the
  same departures for every competitor); then one new request is picked, and only the pick is
  timed. rho = 0 is Envoy's stock bench (every backend idle).
- N ∈ {100, 500, 4,095, 10,000} (research-bench.md: Envoy's 100-500, HAProxy's up to 4,095,
  scheduler_perf's 5,000, Nomad's up to 10,000); rho ∈ {0, 0.5, 0.9}; C = 8.
- d = 1 (one CPU per request) is the like-for-like. kiln is also run at d = 2 (and memory) and
  d = 4 (and disk and GPUs), reported apart as "no equivalent": no competitor picks on more than
  one dimension.
- Timing: `CLOCK_MONOTONIC` (Rust's `Instant`, C++'s `steady_clock`) around each pick, the median
  cost of two back-to-back reads (~15 ns here) subtracted from every interval.
- Quality, counted outside the timed region: **oversubscribed** (a pick onto a backend already
  holding C requests), **refused** (no backend returned / run not started), **cv** (coefficient
  of variation of in-flight counts across backends) and **peak** (the busiest backend over the
  mean), averaged over steps.

### What each competitor's timed region holds

- **nginx** (`drivers/nginx_pick.c`): the upstream is built by the modules' own init functions
  (`ngx_http_upstream_init_round_robin`, `_least_conn`, `_random`) from N `server` entries of
  weight 1, without a shared zone (no locks: nginx's cheapest case). Per request the module's
  `peer.init` runs untimed; the timed region is `pc->get` through the pointer nginx itself calls
  (it includes `peer->conns++`). The end is `pc->free`. The least_conn and random modules are
  `#include`d (their functions are static); every other object is linked as nginx built it.
- **HAProxy** (`drivers/haproxy_pick.c`): a real configuration (`backend`, `balance ...`, N
  `server` lines) parsed and checked by HAProxy's own startup path (initcalls, `parse_cfg`,
  `check_config_validity`, which runs the algorithm's tree init); one thread. Timed: the lookup
  (`fwlc_get_next_server`, `fwrr_get_next_server`, `get_server_rnd`) and the assignment HAProxy
  makes (`served++`, `sess_change_server`, which runs `server_take_conn`). Its real, uncontended
  locks are taken. On ties random(2) compares session rates, all 0 here (nothing connects).
- **Envoy** (`drivers/envoy/envoy_pick.cc`, built in Envoy's tree as
  `//test/extensions/load_balancing_policies/kiln_bench:envoy_pick`): Envoy's own `BaseTester`
  (N hosts, one locality) and load balancers constructed as its stock benches construct them.
  Timed: `chooseHost` and the `rq_active` increment the connection pool makes on assignment.
  Changed from the stock bench only as research-bench.md asks: no `node_hash_map` insert of the
  address string in the timed loop, an N sweep, real `rq_active` from the trace.
- **kiln** (`crates/kiln-ci/benches/lb_vs.rs`): one `Owner` and N `Admission`s (lb/core's `place`,
  merged at 10b6e8d8), messages routed in process (no network, no serialization), replies
  delivered in an order drawn from a second seeded stream (the network's). A decision is
  everything from `Owner::ask` to the chosen runner's `Started`, including every probe, offer,
  take, withdrawal and decline. Three times per decision, each the sum of the calls into `place`:
  - `place.critical`: the owner's calls and the chosen runner's. **This is what is compared**:
    the other runners answer in parallel on their own machines, so this is the CPU on the
    decision's path, as a proxy's pick is;
  - `place.owner`: the owner's alone (what bounds one asking node's rate);
  - `place.total`: every runner's too (the fleet's CPU per decision).

## Pass

Per (competitor, N, rho) at d = 1, `analyze.py`: the ratio competitor / kiln of the mean over
runs, at p50 and at p99, with a 95% percentile bootstrap interval over runs (10 runs per
configuration, 10,000 resamples; the run is the unit, not the pick). The competitor's side is its
fastest algorithm on that statistic, as measured. A configuration **passes** when both lower
bounds are ≥ 10 and kiln's quality is no worse than that algorithm's (oversubscription and
refusals per decision no higher, cv no higher).

## Building

Every heavy build runs in a capped low-priority unit under the swarm's lock
(`~/.cache/kiln-lb/build.lock`), never on adacachy:

```sh
ci/bench/build-nginx.sh                      # minutes
ci/bench/build-haproxy.sh                    # minutes
ci/bench/build-envoy.sh [slice seconds]      # hours: run again until it says "built"
~/.cache/kiln-lb/bin/lb-cargo.sh "$PWD" ~/.cache/kiln-lb/bench/logs/cargo-lb_vs.log \
    bench -p kiln-ci --bench lb_vs --no-run  # kiln's side
```

Envoy builds in slices (`envoy-bazel.sh`): each holds the lock for at most its slice
(`RuntimeMaxSec`), in a unit capped at 24 GiB, CPU weight 1, nice 19, 6 jobs, refusing below
100 GiB free, and bazel resumes from its output base (`~/.cache/kiln-lb/bench/bazel-root`).

## Running

```sh
python3 -I ci/bench/run.py --reps 10                 # every competitor built, every N, rho, d
python3 -I ci/bench/analyze.py ci/bench/results/raw/<stamp> --out ci/bench/results/<date>.md
```

Runs are interleaved (each seed runs every configuration before the next seed), so drift in the
machine's load falls on all alike. Each run's step count comes from a short calibration run
(about 2 s a run; 1,000 to 200,000 steps).

## End to end (`e2e/`)

`e2e` is kiln's open-loop generator: asks at a fixed rate to the asking runner's reach socket
(`kiln run`'s), each a detached no-op for one other runner, latency from the intended send time
in an HdrHistogram-layout histogram (< 0.1% error), warm-up discarded. Two latencies: ask →
answer (the first event) and ask → started. Its own workspace (`cd ci/bench/e2e && cargo test`),
no kiln crates linked: it speaks the socket's frames with copies of `reach::local`'s types.

It is built and tested against a fake socket; it runs for real once lb/core's M3 (commands
through placement end to end) is committed, between two runners started for the bench from that
build (never by restarting an installed runner):

```sh
kiln-bench-e2e --socket <asking runner's reach socket> --target <other runner> \
    --rate <asks/s> --seconds 60 --warmup 10 --json ci/bench/results/raw/<stamp>/e2e-<rate>.json
```

The proxies' side is wrk2 (`-R`, open loop) or Nighthawk through each proxy to N test servers,
minus the direct latency, at equal rates below both knees (research-bench.md, "Fair comparison").

## Caveats (research-bench.md's, and this machine's)

- **Not the same problem.** A proxy's pick is one-dimensional, local, with no admission: it never
  refuses and never queues, and oversubscribes when its estimate is off. kiln's decision admits:
  a runner offers only room it has, and the asker learns started, queued or never. kiln's
  in-process decision also runs the messages a distributed decision needs (N probes, N - 1
  withdrawals), which a proxy's pick has no counterpart for.
- Envoy's stock benches are artificial (every host idle, a string map insert timed); its numbers
  here come from the patched one, the stock ones are kept beside them.
- nginx's round robin and least_conn are O(N) by design; its `random two` is its scalable one.
- Vendors' published proxy numbers use one backend; none are used here.
- The machine is shared and loaded (above). Turbo and HWP are on: frequencies vary with load.
