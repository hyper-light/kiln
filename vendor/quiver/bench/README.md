# Quiver's benchmarks

Every performance claim names the run that measured it (CLAUDE.md §7): a release build, its
machine, its commit, its date and its load (load average under 2, nothing else building), best
of N, recorded here.

## The ephemeral class's fan-out (kiln's CP-T2.1)

`crates/quiver/benches/fanout.rs`, built with the release profile:

```sh
QUIVER_COMMIT=$(git rev-parse HEAD) cargo bench -p quiver --bench fanout -- \
  fanout --rate 100000 --subscribers 1000 --seconds 30 --warmup 5 --runs 3
QUIVER_COMMIT=$(git rev-parse HEAD) cargo bench -p quiver --bench fanout -- \
  soak --rate 100000 --subscribers 1000 --seconds 3600 --warmup 5
```

- **What it does.** One node, one topic of the ephemeral class, one publisher on an open loop:
  messages arrive at `--rate` a second on a Poisson schedule (hyper-measure's `open_loop`), each
  `--payload` bytes carrying its scheduled time and its number. `--subscribers` subscriptions each
  take every message, shared out over `--workers` threads (every CPU but two by default), each
  thread taking from each of its subscriptions in turn and waiting on the topic when none has
  anything.
- **What it measures.** A delivery's latency, from its message's scheduled time to its take, for
  every delivery of a message scheduled in the `--seconds` window after `--warmup` (a log-linear
  histogram, within 1.6%); deliveries, lag notices, and every subscriber's sequence check: a gap
  with no lag notice before it is a frame lost, a number taken again a frame doubled. CPU is the
  process's, and the publisher's own thread's beside it: its open loop paces by spinning, a CPU
  that isn't Quiver's work (`/proc/thread-self/schedstat`), so a delivery's CPU leaves it out.
  `--spin-us` has each worker keep looking that long before it waits on the topic.
- **Its bar** (kiln's plan `01a157fdefc37396957ea10adb51a19f`): AC-T2.1.2, publish-to-deliver p99
  at most 300 µs at 100,000 messages a second on one node; AC-T2.1.3, kept with at least 1,000
  subscribers; AC-T2.1.4, no frame lost to a subscriber that keeps up over an hour (`soak`).
- **Where.** The plan names adacachy; adacachy runs kiln's production, so kiln's coordinator moved
  the runs to fedora in a quiet window (2026-10-22): the machine each record names is the one it
  ran on.

## Records

Each run's output, as the harness prints it, under `bench/records/`, named by date and what ran.
