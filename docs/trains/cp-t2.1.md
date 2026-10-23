# Train notes: CP-T2.1, kiln's live feed on Quiver

CP-T2.1's part of a merge train (the critical path's CP-M4; its master plan's §6): what it
delivers, what deploying it does and how to watch it, its risks and its rollback, and what's still
open: the metric names Kiln Metrics hasn't answered and the decisions that are Ada's.

**Where it stands (2026-10-23):**
- **Not merged.** Branch `cp/t2-quiver`, local, has merged main at 719c954 (76b6baf). Main's new
  kiln-http took hyper-rt from git at 767627f; it now takes the vendored copy, so kiln holds one
  hyper-raft.
- **Not gated.** Its two gates, Quiver's and kiln's, wait for slots from the coordinator. What
  has run on fedora on the merged tree, as development checks:
  - clippy on kiln, kiln-cd, kiln-seams and kiln-http, every target;
  - kiln's unit tests: 1,323 passed, none failed;
  - the live-feed labs (2) and the vendored test (8);
  - kiln-http's tests (39) and kiln-cd's (153).
  The blob labs passed before the merge.
- **Not measured.** AC-T2.1.2 to AC-T2.1.4 (Quiver's fan-out bench and its hour's soak) and the
  comparison with NATS, Redis and Kafka wait for a quiet window on fedora.
- **Quiver** is at df068e4. Its gate passed in kiln CI (run 5: fmt, clippy on Linux, macOS and
  Windows, the production check, the tests, the vendor check). Its branch `cp/t2-quiver` is on
  hyper-light/quiver, whose main fast-forwards to it on Ada's word.

## What it delivers

- **The live feed rides a bus.** What the server learns of an agent session goes to every viewer
  watching it through Quiver's ephemeral class, in the server's process (docs/bus.md):
  - each session's mirror publishes to a topic of its own, keeping its last 2048 frames;
  - a viewer reads from the offset its snapshot was taken at, so it misses no frame and sees none
    twice;
  - a viewer that falls behind gets a typed lag notice and starts over alone from a snapshot;
  - a publish never waits for a viewer, so a stalled viewer delays neither the session nor any
    other viewer (AC-T2.1.5).
- **One hyper-raft, vendored.** kiln vendors Quiver, Focal's bus and hyper-raft
  (vendor/MANIFEST.toml, docs/vendoring.md). Its own transport, runtime and replicated stores
  (sync, CD) move with them from hyper-raft's git revision 767627f to the vendored 25cf889,
  hyper-raft's main. Mantle meets kiln at that revision (CP-T10.2).
- **RamStore for tests alone.** hyper-durable's in-memory store compiles only for tests, so the
  write-ahead log is the only store production code can name (T-T2.1.2).
- **kiln-seams' `Bus`** gains `delete` and its clause BUS-11.

## Deploying it

**Config: nothing to set.** `[bus]` has two settings, `memory` (256 MiB) and `topic_bytes`
(16 MiB). Production's `kiln.toml`, as the chart renders it, has no `[bus]`, so it takes those
defaults. The bus also holds at most 65,536 topics and 65,536 subscriptions.

**At its first start:** nothing to take in and nothing written to disk. The bus is in memory: its
topics are made as sessions' mirrors are and go with them.

**Memory:** the bus's messages are charged to its budget as they're kept, not reserved up front,
and let go when every reader has passed them. One session's feed keeps at most 2048 frames or
16 MiB. A full budget lets a topic's oldest frames go, then refuses the frame, which sends that
feed's viewers back to a snapshot. It's never silent: `kiln_bus_refused_total` counts it.

## Watching it

On production's `/metrics`, its logs and its dashboard:
- **Refusals:** `kiln_bus_refused_total` stays at 0. Each refusal sends a feed's viewers back to a
  snapshot.
- **Lag notices:** `kiln_bus_lag_notices_total` moves only when a viewer stalls (a slow phone, a
  backgrounded tab). Many at once means viewers can't keep up with sessions' output.
- **Memory:** `kiln_bus_memory_bytes` stays well under `kiln_bus_memory_limit_bytes`.
- **Delivery:** `kiln_bus_delivery_seconds`, a frame's publish to a viewer's read, timed in
  microseconds: under a millisecond in the main, since both are in the server's process. A tail in
  milliseconds means the server's own tasks are waiting for its CPUs.
- **Events** on target `kiln.bus`: no warnings. A viewer that lost its live feed and starts over
  warns: `event = "feed.refused"` (a refused create, publish, read or wait) or
  `event = "feed.unreadable"` (a frame the feed didn't make). An ended session's topic the bus
  wouldn't delete is info.
- **The dashboard:** the "Live feed" row of the Kiln overview dashboard
  (deploy/grafana/dashboards/kiln-overview.json).

## Metric names, agreed with Kiln Metrics

Kiln Metrics (Observe's owner, agent 01a14bf7c0ce7451a5fd7c6f0a9764d0) agreed the names on
2026-10-23: `kiln_bus_*` names the interface kiln uses, not what's behind it (a bounded `bus`
label if a second implementation ever runs), and Observe tags each node's registry with its host.
At its suggestion the delivery histogram is `kiln_bus_delivery_seconds`, its buckets from 50 µs (a
frame's delivery is timed on its own microsecond stamp), and Observe's event bridge takes target
`kiln.bus` at info and up: a warning when a viewer lost its live feed and starts over, info
otherwise. Once deployed, production exports:

| Family | Type | Labels |
|---|---|---|
| `kiln_bus_published_total` | counter | none |
| `kiln_bus_refused_total` | counter | `op` (create, publish, delete) |
| `kiln_bus_lag_notices_total` | counter | none |
| `kiln_bus_delivery_seconds` | histogram | none |
| `kiln_bus_topics` | gauge | none |
| `kiln_bus_viewers` | gauge | none |
| `kiln_bus_memory_bytes`, `kiln_bus_memory_limit_bytes` | gauge | none |

A rename Kiln Metrics asks for is one change: crates/kiln/src/metrics.rs, docs/bus.md
("Metrics") and the dashboard's Live feed row. Nothing else reads the names.

## Risks

- **hyper-raft under all of kiln.** 767627f to 25cf889 is 166 commits under kiln's runtime,
  transport and stores. Among them:
  - hyper-rt's receive ring grows with use, and its waits park;
  - hyper-log can write a growing file's zeros ahead while the device is idle;
  - hyper-block marks preallocated files on btrfs to be written in place, and on overlayfs reads
    again without `RWF_NOWAIT`;
  - hyper-durable's compaction and refused-write fixes.
  kiln's tests pass on it. kiln sets the new transport limits to the behavior it had: each
  connection's fixed stream credit, the exchange table alone, no retry budget.
- **What R-8 changed doesn't reach kiln.** R-8 (hyper-raft's commits on entries held in memory)
  changed both the log format and the wire format, but only for groups that state fates:
  - the log's new record kind (11, a hard state with a promise) and a persist row's flag 128;
  - the wire's new flags (`HELD`, `TOLD`).
  kiln's groups state none: they take `Config::new`'s `fates: None`. So kiln's logs and messages
  are written as before, and the wire format's version (1) and fixed layout are unchanged
  (crates/hyper-raft/src/wire.rs and crates/hyper-log/src/format.rs at both revisions).
- **A fleet upgrade's minutes:** a new server with old runners, or an old server with new
  runners, speak the same wire for kiln's groups (above), so sync keeps working while runners
  upgrade.
- **The live feed:** a frame over 1 MiB is refused, which sends that feed's viewers back to a
  snapshot. The mirrors' frames are JSON of a session's state and events, far smaller.
- **Measured on fedora, not on production's machine.** The bench's machine is named in each
  record (bench/README.md in Quiver).

## Rolling back

- **Back to the release before** through KilnCD. The deploy's guard does this itself when health
  fails.
- **Config:** production's `kiln.toml` has no `[bus]`, so there's nothing to take out. A server
  whose `kiln.toml` has `[bus]` must lose it first: every section refuses settings it doesn't know,
  and `kiln install` writes every default into a `kiln.toml` it makes.
- **No data:** the bus is in memory. Viewers reconnect and get the older release's feed.
- **The stores:** kiln-cd's store and sync's logs written under 25cf889 are read by the older
  release, since nothing in kiln writes R-8's records (above). Coming back, this release reads what
  the older one wrote, as it read 767627f's before.
- **Still to do (§6):** drill the rollback in the lab with the real binaries: the older release on
  a store this one wrote, then this release again.

## Ada's open decisions

- **The comparison's bar.** Ada asked for an order of magnitude over Kafka, Redis and NATS. Smoke
  runs (no record) show it where subscribers poll and at 100 or more subscribers, where every
  competitor falls behind. With one subscriber whose thread sleeps when idle, the lead is
  bounded by the kernel's wake-up from a CPU's idle state, up to 127 µs on fedora. Quiver pays that
  wake once per message and a broker pays it at least twice. The quiet window's runs say where it
  stands. Running the comparison with deep idle states disabled would take root on fedora, so it's
  hers to decide.

## Open with others

- **The coordinator:** the bench window and kiln's gate slot (Quiver's gate passed in kiln CI).
- **Mantle's agent:** re-vendors Mantle at hyper-raft 25cf889 after its current change. kiln's
  T10 agent takes it from there (CP-T10.2), against this branch's copy.
