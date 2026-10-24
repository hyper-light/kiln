# Quiver's benchmarks

Every performance claim names the run that measured it (CLAUDE.md §7): a release build, its
machine, its commit, its date and its load (load average under 2, nothing else building), best
of N, recorded here.

One program measures them all: `crates/quiver-bench/benches/fanout/`, built with the release
profile, its mode its first argument (`fanout`, `soak` or `report`).

## The ephemeral class's fan-out (kiln's CP-T2.1)

```sh
QUIVER_COMMIT=$(git rev-parse HEAD) cargo bench -p quiver-bench --bench fanout -- \
  fanout --rate 100000 --subscribers 1000 --seconds 30 --warmup 5 --runs 3
QUIVER_COMMIT=$(git rev-parse HEAD) cargo bench -p quiver-bench --bench fanout -- \
  soak --rate 100000 --subscribers 1000 --seconds 3600 --warmup 5
```

- **What it does.** One node, one topic of the ephemeral class, one publisher on an open loop:
  messages arrive at `--rate` a second on a Poisson schedule (hyper-measure's `open_loop`), each
  `--payload` bytes carrying its scheduled time and its number. `--subscribers` subscriptions each
  take every message, shared out over `--workers` threads (every CPU but two by default), each
  thread taking from each of its subscriptions in turn and, when none has anything, waiting on the
  topic, or with `--busy` looking again.
- **What it measures.** A delivery's latency, from its message's scheduled time to its take, for
  every delivery of a message scheduled in the `--seconds` window after `--warmup` (a log-linear
  histogram, within 1.6%); deliveries, lag notices, and every subscriber's sequence check. A message
  of the window a subscriber never took is undelivered and counts in the percentiles past every
  delivery (shown as `undelivered`), so losses can't flatter a tail; one never taken with no lag
  notice before it is a frame lost silently. CPU is the process's, and the publisher's own thread's
  beside it: its open loop paces by spinning, a CPU that isn't Quiver's work
  (`/proc/thread-self/schedstat`).
- **Which path.** A delivery here is a subscription's take, Quiver's shared, zero-copy path. kiln's
  live feed reads by offset through kiln-seams' `Bus`, which copies each frame out and takes the
  topic's lock to find its segment; kiln measures that path itself (`kiln_bus_delivery_seconds`).
- **Its bar** (kiln's plan `01a157fdefc37396957ea10adb51a19f`): AC-T2.1.2, publish-to-deliver p99
  at most 300 µs at 100,000 messages a second on one node; AC-T2.1.3, kept with at least 1,000
  subscribers; AC-T2.1.4, no frame lost to a subscriber that keeps up over an hour (`soak`).
- **Where.** The plan names adacachy; adacachy runs kiln's production, so kiln's coordinator moved
  the runs to fedora in a quiet window (2026-10-22): the machine each record names is the one it
  ran on.

## The comparison: NATS, Redis and Kafka, class for class

`bench/compare.sh OUT` runs every system on the same schedule, payloads and fan-out, measured by
the same harness, and writes `OUT/report.md`: each system's best run beside Quiver's, load by load,
and Quiver's lead over the best competitor at p99 and at p99.9. The bar is an order of magnitude
at both (Q-1.1, docs/requirements.toml).

- **The class.** Quiver's ephemeral class keeps messages in memory, at most once across a node's
  loss, a gap announced. Its peers are the brokers' own in-memory delivery: NATS core (no
  JetStream) and Redis Pub/Sub, each as shipped; Kafka has no in-memory class, so it runs as a
  latency-minded user sets it, as Focal's comparison sets it (Focal's `tools/compare`): no linger
  before a send (`linger.ms` 0), a fetch answered as soon as a byte is there (`fetch.min.bytes` 1),
  a topic of one partition on one broker.
- **The deployment.** Quiver runs in the generator's process, as kiln runs it: its node is a
  library. Each broker runs as the server it is, one node in Docker (`--network host`, on loopback
  alone), at the image digests Focal's comparison pins, reached by its own client: async-nats,
  redis-rs (Nagle off, as a latency-minded user sets it: the others' clients turn it off
  themselves) and librdkafka through rdkafka. Each subscriber has a connection of its own (for
  Kafka, a consumer of its own), shared out over the generator's threads, each a tokio runtime of
  its own. What a broker costs over Quiver, a hop to a server and back over TCP, is what it costs
  the user who runs it.
- **The machine.** The broker gets CPUs of its own (`BROKER_CPUS`, by default fedora's P-cores 4 to
  7, both threads each) beside the generator's (`CLIENT_CPUS`, the other 24), which Quiver's node
  shares with its publisher and subscribers: every broker has eight CPUs more than Quiver.
- **How subscribers wait.** Two ways, the same for every system. `wait`: a subscriber's thread
  with nothing to take sleeps until there is (Quiver's workers on the topic, the brokers' clients'
  runtimes in epoll). `busy`: it polls without sleeping, as the lowest-latency consumers do. A
  sleeping thread is woken by the kernel, and a CPU that went idle takes as long as its idle state
  to wake: on fedora up to 127 µs from C2 and 1,048 µs from C3 (`/sys/devices/system/cpu/cpu0/
  cpuidle`, the menu governor). Quiver's subscriber pays that once per message at most; a broker's
  path pays it at its server and again at its client.
- **The floor.** The report sets beside the systems the kernel's own wake-up (`--system wake`):
  the same open loop with no system around it, its publisher waking each waiter thread
  (`Thread::unpark`, the futex Quiver's waits come down to) and each waiter taking what was
  written. It's measured on the machine as it runs, idle states and all: what a system's waiting
  subscriber takes above it is the system's own, and no system's waiting subscriber goes under it.
- **Before each run** the publisher sends priming messages until every subscriber has taken one,
  so every subscription stands at its broker before the first measured message; Kafka's run has a
  topic of its own, read from its beginning. A broker's subscriber that hasn't taken everything
  `--drain` seconds (30) after the schedule closed has what it missed counted undelivered.
- **Where.** fedora, in a quiet window, like the fan-out. `LOAD_LIMIT` lets a smoke test of the
  harness start on a busy machine; what it measures is no record.

```sh
bench/compare.sh ~/.cache/quiver-compare/$(date +%F)    # every system, every load
SYSTEMS="quiver nats" SUBSCRIBERS=100 WAITS=busy bench/compare.sh OUT    # one cell
```

The comparison needs Docker, and librdkafka's build needs `make` and a C and a C++ compiler: where
there's no `c++` on the PATH, zig's serves (`zig c++`, as Quiver's gate installs zig). Kafka's
profile needs a JDK's `jcmd` on the host.

## The profile

Every run carries a profile of its measured window (`profile/`): what its threads and its broker
spent, in all, per message and per delivery. The table is printed under the run's rows, every
figure goes into the runs file (`--out`), and `report` sets each system's best run's beside the
others', load by load. A figure the machine won't give says why: never a zero in its place.

- **This process** is Quiver itself for Quiver (its node is a library in the generator's process,
  as kiln runs it), and a broker's clients for a broker: the broker's own side is its own column.
- **Allocations.** Every thread's allocations, reallocations (and those that moved their block),
  frees and bytes asked for, each thread counting into a slot of its own (no lock, no contended
  atomic). On Linux with glibc the count sits under `malloc` itself, this binary defining C's
  allocator over glibc's own, so librdkafka's C allocations count beside Rust's; elsewhere a
  counting global allocator counts Rust's. The slots also count each thread's publishes and
  deliveries: a profile divides by what they counted between the same two readings.
- **The kernel's account.** The process's CPU, minor and major page faults and voluntary and
  involuntary context switches (getrusage), less the profiler's own thread's; the publisher's and
  the subscribers' threads' own (`/proc/self/task/<tid>/`), with the time each spent runnable but
  waiting for a CPU; resident memory at its peak in the window (`VmHWM`, reset as the window opens)
  and at its end; the process's storage I/O (`/proc/self/io`'s bytes; its call counts leave out
  a socket's `recv` and `send`, so they aren't shown).
- **The CPU's counters** on the publisher's and the subscribers' threads (perf_event_open, user
  space only, as `perf_event_paranoid` 2 allows): cycles, instructions, cache references and misses,
  and L1 data, last-level and data-TLB read misses, scaled where the PMU multiplexed them, and on
  a hybrid CPU summed over its core types. The publisher's include its pacing spin, and with
  `--busy` a subscriber's thread polls, so its cycles are its time rather than its work. A counter
  the kernel refuses is shown refused with its reason (AMD's cores count no last-level event per
  thread: their L3 sits outside the core's PMU).
- **Memory held.** Quiver's node's accounted memory, framing included, at its peak in the window,
  and per message it held then.
- **The broker**, read a quarter second before the window and after it, so no reading lands in
  what's measured: its container's cgroup (CPU, page faults, memory charged at its peak and at the
  end, storage bytes and operations from `io.stat`), and its runtime's own, each counting what it
  counts. NATS: Go's `runtime.MemStats` from its profiling port (`--profile 16060`: objects
  allocated and freed, bytes, collections and their pauses, the heap in use), the window's
  allocation profile and the heap profile after it saved for `go tool pprof`. Redis: jemalloc's
  allocation requests (`MEMORY MALLOC-STATS`, every arena merged) and `used_memory`, `MEMORY STATS`
  and the allocator's statistics saved. Kafka: a Java Flight Recorder recording over the run
  (`jcmd` from the host into its container's JVM): the bytes its threads allocated in the window
  (their counters every 100 ms), its collections and pauses, the heap after the last; the recording
  saved for JDK Mission Control or `jfr print`, its allocation samples with their stacks. Starting
  the recording lands in the warm-up, and it costs Kafka the JDK's profiling settings' overhead
  through the window (`profile.jfc`: "typically around 2 % overhead").
- **Its check.** `bench/plant.sh OUT` runs Quiver and the floor briefly from the bench as built and
  from a test build that plants one allocation in every delivery (`--features plant`): the profiles
  must differ by one allocation and one free a delivery, all on the subscribers' threads, so the
  count sees every thread. Their subscribers poll (`--busy`): a waiting Quiver subscriber allocates
  once a wait, and how often it waits moves with the machine's load between two runs.
  `compare.sh` runs it before its brokers start (`OUT/plant.md`).

## The budgets

`bench/budgets.tsv` holds what one run of a load may spend, figure by figure, a message or a
delivery (kiln's CP-T2.1, step 2): a subscriber's take allocates nothing, waiting or polling; a
publish allocates twice (the future `Topics::publish` answers with, and the block its batch is kept
in) and reallocates nothing; at most 100 minor page faults a million messages after the warm-up;
and on fedora the subscribers' last-level cache misses a delivery and a waiting one's CPU a
delivery (a polling thread's CPU is its time, not its work). `--budgets FILE`
holds each run to the budgets for its load and machine, printed under its profile, and fails the
run's exit status if it went over one. `bench/budget.sh OUT` is the bench gate: each budgeted load
for 60 s after 5 s of warm-up, failing where a run is over a budget by more than a tenth (over one of
zero at all) or doesn't count a figure it's held to. Like every measure here, on a quiet machine.

## Records

Each run's output, as the harness prints it, under `bench/records/`, named by date and what ran.
