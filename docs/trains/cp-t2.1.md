# Train notes: CP-T2.1, kiln's live feed on Quiver

CP-T2.1's part of a merge train (the critical path's CP-M4; its master plan's §6): what it
delivers, what deploying it does and how to watch it, its risks and its rollback, and what's still
open: the metric names Kiln Metrics hasn't answered and the decisions that are Ada's.

**Where it stands (2026-10-24):**
- **Landed** on kiln's main at 545e9967 (2026-10-24 08:20 PDT, a fast-forward from cc1de07a), on
  Ada's word, Kiln Distro holding main for the train. Not deployed yet: production ran 8adeadba then.
- **Gated** on fedora, on Ada's go: cp/t2-quiver at 359c8949, main 264d8cc6 merged: clippy for
  Linux, macOS and Windows, every target, `-D warnings`, clean; the workspace's tests, 2,219 passed
  and 1 failed, `w20_ac05_the_ci_skills_programs`, main's own runner-channel race ("AssignV2 ...
  unreachable: its runner is away" 60 ms after the runner connected), which passed 3 of 3 alone and
  went to its owner. Main's 4 later commits, in files the branch doesn't touch, were merged as
  545e9967 and re-checked: clippy for the three platforms clean, their modules' tests 46 passed.
  kiln's vendor check (`scripts/vendor.py check quiver focal-bus hyper-raft`): 0 differences. The
  first gate (3da6accd, 2026-10-23) failed two tests, both fixed since:
  - P2's learner test raced its compaction, and with kiln's stores on hyper-raft 25cf889 it failed
    19 of 30 runs; its owner's fix (40e54477, on main as 264d8cc6) passed 30 of 30 on 25cf889;
  - the full-bus lab's 256 KiB bus was under the 2,166,784 bytes a Quiver node takes, so its
    server didn't start; at 3 MiB its filler emptied the bus a segment at a time, so the second
    feed's frames fit. It now reads what the filler leaves free and sizes the second feed's frames
    past it.
- **Merged with the wire owner's viewer resume** (07d06775, 873d40c9, c3afe70a; docs/wire.md
  "Revisions"): each frame on a session's feed carries the copy's revision; a viewer attaches with
  `?after=` and gets only what changed, or the session whole when the copy can't tell; one that
  falls behind, is sent back by the resync marker, or sees its feed's refusals move catches up from
  the last revision it was sent; a copy whose events went another way publishes the marker, and its
  viewers take it whole. Both catch-ups are taken as parts under the mirrors' lock and made into
  frames after it. The wire owner's resume tests (3) and wire_updates with chaos (3) pass on it.
- **Reviewed.** An adversarial review found, and this branch fixed:
  - a viewer could wait on silently once a full bus refused its feed's frames and the resync marker
    after them: each viewer now counts its feed's refusals and starts over when there were more;
  - a session nobody watched kept its frames on the bus until it ended: its topic now goes at its
    first frame with no viewer, and is made again for the next;
  - an ended session's topic went at once, from under a viewer still a frame or two behind: it now
    goes with its last viewer, each ending with the session's last frame (`closed`);
  - a viewer's snapshot was made under the registry-wide lock, holding up every session's frames:
    its parts are taken under it (the events by reference) and it's made after;
  - vendored names were pinned by caret, and crates.io has unrelated crates under some
    (`quiver`, `hyper-rt`): every one is pinned exactly, and bench/kilndb patches them too;
  - the labs passed even if viewers only ever caught up by starting over: they now require one
    snapshot each, and no lag notice or refusal on the server.
  Since then, CP-T2.1's test 4 has a lab of its own: a 3 MiB bus, filled by one watched
  session's feed, refuses a second session's frames, each sized past what the filler leaves free,
  and both of that session's viewers must start over within 5 s (a feed whose resync marker is
  refused too is harness::live's a_viewer_learns_its_feed_broke_without_a_frame_saying_so). A viewer with nothing to read looked again after
  5 s, so one whose wait began just after the refusal started over a little past it; it looks
  after 4 s now.
- **Measuring** (Quiver's bench/records/):
  - The comparison with NATS, Redis and Kafka ran on adacachy on Ada's word, beside production, its
    systems interleaved load by load: 180 runs at a load average of 21 to 143, so no record of
    any bar. Under that load Quiver led the best competitor tenfold in 8 of 12 loads.
  - The hour's soak lost no frame, twice, on adacachy beside production: at Quiver 08d3dc9 (load
    36 to 62) and at 88409dd, the commit kiln vendors (load 39 to 58): each 360,529,149 published,
    359,979,959,000 deliveries measured, none lost, doubled or undelivered, all 1,000 subscribers
    kept up, no lag notice (AC-T2.1.4). At 88409dd the subscribers' threads allocated nothing in
    the hour, and a publish 2.010 times.
  - AC-T2.1.2 and AC-T2.1.3's records (p99 at most 300 µs at 100,000 messages a second to 1,000
    subscribers) and the comparison's want a quiet machine: asked of the coordinator.
- **Quiver** is at 88409dd, hyper-light/quiver's main, its gate passed in kiln CI (run 16 on the
  branch, run 17 on Quiver's main: fmt,
  clippy on Linux, macOS and Windows, the production check, the tests, the vendor check). Since
  d8e5d54: every bench run's profile (allocations, page faults, switches, the CPU's counters,
  memory, and each competitor's own); a subscriber's take and wait allocate nothing and a publish
  allocates twice, half what it did, held by a bench gate (bench/budgets.tsv); a never-offered
  message no longer counts as lost. kiln vendors 88409dd, leaving out Quiver's bench records.

## What it delivers

- **The live feed rides a bus.** What the server learns of an agent session goes to every viewer
  watching it through Quiver's ephemeral class, in the server's process (docs/bus.md):
  - each session's mirror publishes to a topic of its own, keeping its last 2048 frames;
  - a viewer reads from the offset its snapshot was taken at, so it misses no frame and sees none
    twice;
  - a viewer that falls behind gets a typed lag notice and starts over alone from a snapshot;
  - a publish never waits for a viewer, so a stalled viewer delays neither the session nor any
    other viewer (AC-T2.1.5);
  - a viewer waiting for the next frame waits on the topic's own list of waiters, which allocates
    nothing once grown (Quiver's waits were a listener allocated each time).
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
  kiln's groups state none: sync's, CD's and kiln.db's alike take `Config::new`'s `fates: None`. So kiln's logs and messages
  are written as before, and the wire format's version (1) and fixed layout are unchanged
  (crates/hyper-raft/src/wire.rs and crates/hyper-log/src/format.rs at both revisions).
- **A fleet upgrade's minutes:** a new server with old runners, or an old server with new
  runners, speak the same wire for kiln's groups (above), so sync keeps working while runners
  upgrade.
- **The live feed:** a frame over 1 MiB is refused, which sends that feed's viewers back to a
  snapshot. The mirrors' frames are JSON of a session's state and events, far smaller.
- **Quiver's waits are its own now.** A topic's waiters wait on its own list, rung under its lock
  as `event_listener` (which it replaces) rang: against it, at 100,000 messages a second to 100
  waiting subscribers, the same wake-ups a message (9.5 to 9.7 against 9.8) and a tail no worse
  (p99 127 to 135 µs against 137 to 160; Quiver's bench/records/2026-10-23-fedora-step2/). Its
  conformance suite and the stalled-subscriber test pass on it.
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

- **The coordinator:** the bench window (about 2.3 hours of fedora with nothing beside it).
- **The wire owner:** its read of the merge's resync choice (a marker each viewer pulls the copy
  whole on, not the copy pushed on the feed), and the runner-channel race above.
- **Mantle's agent:** re-vendors Mantle at hyper-raft 25cf889 after its current change. kiln's
  T10 agent takes it from there (CP-T10.2), against this branch's copy.
