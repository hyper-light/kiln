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
at both.

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
there's no `c++` on the PATH, zig's serves (`zig c++`, as Quiver's gate installs zig).

## Records

Each run's output, as the harness prints it, under `bench/records/`, named by date and what ran.
