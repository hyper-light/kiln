# The load-balancer bench: kiln against nginx, HAProxy, Envoy and Katran

The bar (the person's, 2026-11-07): kiln's balancer an order of magnitude faster than every
algorithm nginx, HAProxy, Envoy and Katran ship, load-unaware ones included, at no worse
quality. The contract is `~/.cache/kiln-lb/notes/bench-spec.md`; this folder implements it on
Apple silicon, where the 24 MHz counter can't time one pick, so blocks of steps are timed.
Results: `results/<date>.md`, from the raw runs in `results/raw/`.

## The competitors, pinned

| who | version | commit | built how | algorithms (load-aware in bold) |
|---|---|---|---|---|
| nginx | release-1.31.8 | `140aa942711868a8daeb95810bc9a81f272ada72` | `auto/configure --without-pcre --without-http_rewrite_module --without-http_gzip_module --with-cc-opt=-O2`; nginx's clang default is `-O`, so `-pipe -O -Wall -Wextra ... -g -O2` | round_robin, **least_conn**, random, **random_two_least_conn**, hash, hash_consistent, ip_hash, **least_time** |
| HAProxy | v3.4.7 | `8872a2604595140544774c2a6309a41186fc0f44` | `make TARGET=osx` (Linux: `linux-glibc`): its own `-O2 -g -fwrapv`, threads on (`USE_THREAD`), no OpenSSL | roundrobin, static-rr, **leastconn**, first, **random** (`random(2)`, its default), source, source-consistent |
| Envoy | v1.40.2 | `b2d89e0ae53916f28770771fbdd49c5c99c5c5a2` | bazel 8.8.1 (its `.bazelversion`, through bazelisk v1.25.0), `-c opt`, Xcode's clang | round_robin, **least_request** (P2C), **least_request_full_scan**, random, ring_hash, maglev |
| Katran | main (no tags) | `042d08df6677dbde86ed7b127218a8e80445ce41` | its lookup copied verbatim from `katran/lib/bpf/balancer.bpf.c`, `-O2` as its BPF build (`katran/lib/Makefile-bpf`); its ring code (`CHHelpers`, `MaglevHash`, ...) `-O2` | maglev |

Every nginx and HAProxy algorithm runs twice: as configured by default (no limit: they
oversubscribe freely), and with their admission at C = 8 (`/max_conns=8`: every server
`max_conns=8`; `/maxconn=8`: every server `maxconn 8`), the equal-quality comparison, since kiln
never oversubscribes. A pick their admission turns away counts as a refusal (nginx: NGX_BUSY;
HAProxy: the request would queue). Envoy and Katran have no admission.

## Where things are (stable: the end-to-end bench reuses them)

On whatever machine runs `run.py` (here `adas-mac-studio.local-2`):

| path | what |
|---|---|
| `~/.cache/kiln-lb/bench/src/{nginx,haproxy,envoy,katran}` | the sources, at their pins (`run.py prepare` refuses another commit) |
| `~/.cache/kiln-lb/bench/build/nginx` | nginx's objects (`--builddir`) and `lb/` (the driver's) |
| `~/.cache/kiln-lb/bench/build/haproxy` | HAProxy, built in a worktree of `src/haproxy` (its Makefile builds in-tree) |
| `~/.cache/kiln-lb/bench/build/katran` | Katran's ring objects and `lookup.inc` (the copied functions, with their line numbers) |
| `~/.cache/kiln-lb/bench/bazel`, `tools/bazelisk` | Envoy's bazel output root and the bazelisk that ran it |
| `~/.cache/kiln-lb/bench/bin/{null,nginx,haproxy,katran,envoy}-driver` | the drivers |
| `~/.cache/kiln-lb/bench/bin/envoy-stock-*` | Envoy's own LB benches, unpatched, for reference |
| `~/.cache/kiln-lb/bench/build/{nginx,haproxy}-run/` | the configurations the drivers generate per (algorithm, admission, N) |
| `~/.cache/kiln-lb/bench-measure.lock` | held (exclusive flock) by every timed run, so no other bench measures beside one |

## Running

```sh
python3 ci/bench/run.py prepare nginx haproxy katran   # fetch at the pins, build (minutes)
python3 ci/bench/run.py prepare envoy --jobs 8         # hours; stops below 100 GiB free
python3 ci/bench/run.py build null nginx haproxy katran
python3 ci/bench/run.py smoke --impls null,nginx,haproxy,katran,envoy
python3 ci/bench/run.py warmup  --impls nginx,haproxy,katran,envoy --w-file W.json     # W per N
python3 ci/bench/run.py measure --impls null,nginx,haproxy,katran,envoy,kiln --w-file W.json --out RUNS.jsonl
python3 ci/bench/analyze.py RUNS.jsonl --w W.json --out ci/bench/results/<date>.md
```

Builds run at `nice -n 10`. A driver alone: `bin/nginx-driver least_conn --n 4095 --rho 0.9 --b 16384 --w 200000`
prints one JSON line; `--mode trend --horizon H` prints the imbalance series; the null driver's
`--mode calibrate` prints B.

## The workload, exactly (`common.h`; kiln's bench must match it)

- N backends of C = 8; F' = max(1, floor(rho N C)) requests in flight, one per slot 0..F'-1.
- One SplitMix64 stream, seed 0 (`z = (s += 0x9e3779b97f4a7c15)`, then the two xor-shift-multiplies
  `0xbf58476d1ce4e5b9`, `0x94d049bb133111eb`). A draw x maps to [0, k) as the high 64 bits of x*k.
- Fill: F' picks into slots 0..F'-1, each taking one draw as its key.
- A step: slot s = draw mapped to [0, F'); key = the next draw; release the request in s (if it
  holds one); pick into s with that key. Hashing algorithms read the key as the client: its high
  32 bits are the IPv4 address (nginx `ip_hash`, `hash $binary_remote_addr`, HAProxy `source`,
  Katran's flow source), its low 16 bits the source port (Katran); Envoy's ring_hash and maglev get
  the 64 bits as the context's `computeHashKey()`.
- Run phases: fill; W untimed steps; M timed blocks of B steps; M*B untimed quality steps; T =
  M_R*B untimed top-up picks into slots F'..F'+T-1 (one key draw each); M_R timed blocks of B
  release-only steps (slot = draw mapped to [0, live), swap-removed from the live list); then every
  request still held is released and the implementation is checked to hold none.
- M = max(3, ceil(F'/B)) (three: the fewest blocks whose median survives one disturbed block; F'
  steps: a request's mean stay, by Little's law, so the window sees the population turn over once),
  M_R = 3. A run's ns per step is the median block over B.
- The null (no-op release, pick returning backend 0, called through the same pointers) runs the
  same phases first in the same process: `ns_null`, `ns_null_release`. Every comparison is
  `ns_step - ns_null`.
- B: the smallest power of two whose every one of 3 null blocks takes 1,000 counter ticks (the
  largest of three calibrations), per (N, rho). W: per N, at rho = 0.9 (the most in flight), the
  largest MSER-5 cut over every implementation's CV series (sampled every N steps after the fill,
  horizon 4 F' and 100 samples, doubled at most twice while the cut falls in the second half).
- Rounds: per (N, rho), round r runs every implementation once, rotated by r; at least 6 rounds
  (the fewest with a distribution-free 95% interval of a median), until each median's 95%
  bootstrap interval is within 5% of it, or 30. Each run holds the measure lock and records the
  1-minute load average before and after.
- QoS user-interactive (`pthread_set_qos_class_self_np`); macOS can't pin a thread to a core.

## What each driver's step holds

- **nginx** (`drivers/nginx.c`): nginx's own startup, as its `main()` runs it up to `ngx_init_cycle()`,
  on a generated `nginx.conf` (one `upstream` of N `server` lines and the algorithm's directive; no
  `zone`, nginx's default: the worker's own peers, no lock). Pick: `us->peer.init(r, us)` (the
  per-request setup: it allocates and zeroes the `tried` bitmap, N/64 words once N > 64, and for
  `hash` evaluates the key) then `pc->get`; release: `pc->free(pc, data, 0)` and the request's pool
  reset. 1.31.8 turns upstream keepalive on by default (32 connections, `keepalive_init_main_conf`),
  so init, get and free are keepalive's wrappers around the algorithm's: the real default path (its
  cache stays empty). `least_time` (open source in this release) sees response times of 0.
- **HAProxy** (`drivers/haproxy.c`): HAProxy's own `main()` parses a generated configuration and
  starts; the bench runs from a per-thread init hook (`REGISTER_PER_THREAD_INIT`) in thread 1 of
  `nbthread 1`, after the whole startup and before the poll loop: no patch to `haproxy.c`. Pick:
  the algorithm's get function, each taking the backend's lbprm lock (`fwrr_get_next_server`,
  `map_get_server_rr`, `fwlc_get_next_server`, `fas_get_next_server`, `get_server_rnd`,
  `get_server_sh`), then `assign_server_and_queue`'s slot (`served` + 1, or its CAS below `maxconn`)
  and `sess_change_server`'s take: the backend's `served` + 1, a store barrier,
  `lbprm.ops->server_take_conn`. Release: `sess_change_server(s, NULL)`'s `served` - 1 on both, the
  barrier, `server_drop_conn`. Only leastconn and first define take and drop at v3.4.7: they
  reposition every step. Left out, as not balancing: the stream-list links, the `cum_lbconn` and
  session-rate counters (so `random(2)`'s tie-break by session rate reads 0), `process_srv_queue`.
- **Envoy** (`drivers/envoy/`): a package built in Envoy's tree. Hosts and LBs set up as Envoy's own
  `*_lb_benchmark.cc` set them up (`BaseTester`); ring_hash and maglev `initialize()`d, then
  `factory()->create()`, the worker's LB. Pick: `ClusterEntry::chooseHost`'s override check, then
  `chooseHost(&context)` with a `LoadBalancerContextBase`, then `rq_active` + 1 (the pool's take);
  the router's `HostConstSharedPtr` held for the request. Release: `rq_active` - 1. Left out: the
  cluster counters behind `BaseTester`'s gmock `MockClusterInfo`.
- **Katran** (`drivers/katran/`): `get_packet_hash`, `is_under_flood`, `increment_ch_drop_no_real`,
  `increment_ch_drop_real_0` and `get_packet_dst` copied verbatim from `balancer.bpf.c` (the
  new-flow path: jhash % RING_SIZE, `ch_rings`, `reals`), with `shim/katran_shim.h` for the kernel
  (integer types, three array maps as bounds checks and indexes, `bpf_ktime_get_ns`) and an empty
  `common/bpf/bpf_net_helpers.h` (included by `balancer_consts.h`, missing from the tree). The ring is
  Katran's `MaglevHash` (`ring.cc`); one shim: the endpoints' `folly::IPAddress::hash()` is SplitMix64
  of the address. No LRU map (the connection table is the kernel's BPF LRU, not Katran's code) and
  no release: Katran keeps nothing per backend, so the release is a no-op and the pick alone is
  `ns_step - ns_release`. N = 10,000 builds with `MAX_REALS = 16384` (Katran's default is 4096;
  `balancer_consts.h` allows raising it).

### Katran's XDP datapath (not run: it needs root on Linux)

What a `BPF_PROG_TEST_RUN` run of the real program takes, for when a Linux machine with root is
available: build `balancer.bpf.o` with Katran's `build_bpf_modules_opensource.sh` (clang -O2
-target bpf, kernel headers); load it with libbpf (`bpf_object__open_file`, `bpf_object__load`);
fill `vip_map` (one VIP), `reals` (N entries), `ch_rings` (`MaglevHash::generateHashRing`, the
same as `ring.cc`) and `ctl_array` (the default router's MAC); build one IPv4/TCP SYN frame per
request from the same SplitMix64 keys (source address and port); time `bpf_prog_test_run_opts`
with `repeat = B` per frame batch (the kernel times the repeats: `opts.duration` is ns per run);
it includes parsing, the LRU insert and the IPIP encapsulation the userspace driver leaves out.
`sudo` (CAP_BPF and CAP_NET_ADMIN) and kernel 5.10 or later.

## Quality (untimed, beside speed)

Oversubscription: picks of a backend already holding C, as a count with its denominator (`picks`);
refusals: picks that found nothing; imbalance: the mean CV of in-flight counts across backends,
sampled every N steps of the quality pass. At rho = 0 one request is in flight, so CV is sqrt(N-1)
for everyone. Notes the code dictates (`analyze.py` prints them): nginx `hash` keeps 15 bits of
crc32; `ip_hash` has 6,271 values (37% of peers unreachable at N = 10,000); Envoy's and Katran's
65,537-entry Maglev tables skew about +-8% at N = 10,000.

## Caveats

- HAProxy documents roundrobin as "limited by design to 4095 active servers per backend"; at
  N = 10,000 it starts and balances (CV as round robin's), and is measured, past that limit.
- One thread on a shared machine: other agents' builds and CI jobs run beside the bench (the load
  is on every line). Runs are interleaved, so drift falls on every implementation alike.
- Where this departs from bench-spec.md, and why: the release-only blocks run on the run's own
  state topped up by T picks (a copy of nginx's pools or HAProxy's trees can't be taken, and B
  releases would drain F' < B); oversubscription and refusals are counts, with `picks`.
- Load-aware and load-unaware algorithms are labelled; the verdict is against each competitor's
  fastest of any kind, the load-aware-only comparison beside it.
