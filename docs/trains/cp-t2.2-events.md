# Train notes: CP-T2.2's kiln events on Quiver's persisted class

CP-T2.2's part of a merge train (the critical path's CP-M4; its master plan's §6): step 4, the
persisted class in kiln's bus, and step 8, kiln's CI and deploy events on it (AC-T2.2.1,
T-T2.2.1): what it delivers, what deploying it does and how to watch it, its risks and its
rollback, and what's still open.

**Where it stands (2026-11-11):** on branch `cp/t2.2-land` (step 8's `cp/t2.2-events` merged with
main b2dfa3f2cbc), not on main, not deployed; Ada's word to land and deploy given 2026-11-09. Its gates and labs are below; its landing waits on
Ada's word, through Kiln Distro's train.

## What rides with it

- **Quiver at 1e8363b** (hyper-light/quiver main, hyper-raft 0004 recorded as kiln records it): Quiver's CP-T2.2 line on its main (the
  persisted class, its simulation, step 8's changes), on hyper-raft f45fc563 and Focal's bus
  9aded7cd. Before: 8a882b5, which held none of it.
- **Focal's bus with Quiver's four recorded changes** (0002-0005: a lone member's liveness room,
  a member's log on the simulated device, the simulation's diagnosis, Ed25519 identities), recorded
  byte for byte as Quiver records them. Focal's agent reviewed them with Ada; landing them in
  `bus-replicated` is Ada's call, and until then they stay recorded changes.
- **hyper-raft at f45fc563** (from 0856e05): a member's catch-up read off the log's device and its
  owner's thread (8203466), its next run read ahead (f45fc563); its 0001 refreshed as the runtime
  program records it, 0002 and 0003 unchanged. hyper-log reads on one more thread
  (`hyper-log-fetch`).
- **The runtime program's catch-up, part 2** (a41c4a25332, goal 11's audit, row B21), at Ada's
  word: the learned log cache goes; a runner back from away is caught up from the file while the
  server writes.
- **A test hub's bus serves the ephemeral class unless its test asks** (`persisted_hub_on`):
  the server's opens the persisted class as before.
- **The webhook queues' memory owner goes** (`webhook_queues`): the store-backed deliveries
  replaced the queues it counted.
- **Step 8's two migrations are dated 2026-11-07** (`20261107-1500`, `20261107-1501`), after
  every migration main had then.
- **A leg's first wait for reviewers records `approval.waiting` with it**, through the same
  transaction main's recovered waits share (`ci::runs::write_waits`); a recovered wait records none.

## What it delivers

- **The persisted class in kiln's bus.** The server opens its bus as the one node of a cluster of
  one (docs/bus.md, "The persisted class"): each persisted topic a part of Focal's replicated by a
  Raft group of one, durable on the server's disk, a publish answered once the group's log flushed
  it. Its data is in the data directory's `bus` (`[bus] data`); its node listens on loopback at
  ports the system picks (Quiver 4edf13c). A persisted class that can't open leaves the server
  running with the live feeds' ephemeral class alone, saying why.
- **kiln's events, recorded with the change** (docs/bus.md, "kiln's events"): a row of
  `event_outbox` in the SQLite transaction that makes each change. Triggers record every run's
  and job's change of status or conclusion and every step's; a run's end, a deployment's end, a run
  waiting for reviewers and a land queue's ejection are recorded by the code that makes them, in
  its transaction; what isn't a change of kiln.db (KilnCD's apps, runners, agents) is recorded as
  it's said.
- **Relayed in order** to the persisted topic `kiln.events`, in batches of up to 128 events a
  message, each event numbered by its outbox row; a restart publishes again only the last row the
  topic holds.
- **Delivered from the topic**: outgoing webhooks (W19-R07) and the notification rules' channels
  (W19-R06) each read `kiln.events` from a position of their own, logging each event's deliveries
  in the transaction that moves it; a sender per subscription or channel sends them one at a time,
  in order, retried with exponential backoff (2 s doubling to 5 minutes) for a day.
  `X-Kiln-Delivery` stays the same for an event and a subscriber however often it's sent, for the
  receiver to drop a second.
- **Kept from before:** the delivery logs and redelivery, `throttle`, the hub's events for the UI
  and kiln's own listeners; a release's chat messages, brought up to date as it moves.

## What shows it

On the branch as landed (ba13521227f, main b2dfa3f2cbc merged), the checks of what it changes
(Ada, 11-01: "we don't need the full gate"), each on a machine kiln picked by its needs:
- **Clippy** with `-D warnings`, the workspace's every target: Linux (adacachy2) and macOS (a Mac
  Studio) pass. Windows (`x86_64-pc-windows-gnu` by cargo-zigbuild: no Windows runner takes reach
  runs) fails on 8 `dead_code` errors in main's W20 legacy custody (`harness/`,
  `runner/custody.rs`), the same on main's own tree; this branch changes none of those files.
- **kiln's lib tests** of the modules it changes (`ci`, `events`, `hub`, `bus`, `deploy`, `memory`,
  `metrics`, `observe`, `store`, `sync`, `api`, `server`), on adacachy under its caps: 1,227 passed,
  5 failed. Four pass alone (load: `g9_s5`, `w24_ac10`, `g11_s3_b3`, `g11_b21`);
  `w28_kilns_own_workflows_check_plan_and_route` fails on main too (`.kiln/windows-recovery.yaml`
  isn't in its list). Step 8's own pass: T-T2.2.1, a rules' channel down 10 minutes, the relay's
  kill points, the persisted class's three, the group's replay.
- **kiln-obs and kiln-seams**: 414 passed; `bus_persisted`, `notifications` and `vendored` pass.
- **`deploy_chat`'s `p5_chat_slack…`** failed on this branch, and not for want of `helm`, as was
  said of it before: its second release was rejected before its message went out (below, "Risks").
  It now waits for that message, then rejects it, and passes (adacachy2, which has no `helm`), as
  it passes on main.
- **The kill -9 lab**: 100 kills over 184 s, 6,348 changes committed, each delivered at least once
  and in order, 43 twice with the same delivery id.

Earlier, on fedora (kiln-a094a12d), at e0989666f: the full gate, 6,312 passed, 5 of the 6 that
failed passing alone (load), and `p5_chat_slack…` (above); the kill -9 lab, 100 kills over 112 s,
7,635 changes committed, each delivered at least once and in order, 19 twice.

Earlier, on adacachy under its caps:
- **A persisted topic across kill -9** (`tests/bus_persisted.rs`): 28,000 messages acknowledged in
  8.7 s, the process killed -9, every one at its offset after the restart (28,032 held, 39 MB of
  the topic's 64 MiB); the restarted bus caught up with its catalog in 929 ms and served the topic
  55 ms later.
- **Kills at each point of the relay** (`events::tests`): stopped before it publishes, and after it
  publishes but before it records so, then started again on the same kiln.db and bus: every
  committed event on the topic in commit order, only the last published row again, with its
  numbers. The cluster group's replay brings a write's events back once (`store::group`).
- **T-T2.2.1** (`ci::webhooks`): a subscriber down for 10 minutes (simulated time) while runs are
  made and moved: tried again and again with backoff, then every change delivered once it was
  back, in the order made. A rule's channel down for 10 minutes, the same (`ci::notify`).
- **The kill -9 lab** (`tests/events_lab.rs`, chaos build): a server with a subscriber, a writer
  committing runs' changes into its kiln.db, the server killed -9 at random a hundred times: 9,477
  changes over 148 s, every one delivered at least once and in order, 18 twice with the same
  delivery id. Its starts found where the kills fell: 48 with rows published and not recorded, 101
  with rows committed and not published, 53 with deliveries logged and not sent. An earlier run
  (one event a message) delivered 8,985 of 8,985 so.
- **The UI**: the notifications sent list shows a pending delivery as waiting to go (its tries and
  error), checked in Chromium and WebKit at desktop and phone widths.

## Deploying it

**Config: nothing to set.** `[bus]` gains `data` (empty: the data directory's `bus`),
`part_memory` and `persisted_bytes` (64 MiB each). Production's `kiln.toml` has no `[bus]`, so it
takes the defaults. **Set none of them while a rollback may be wanted**: the release before denies
unknown keys and wouldn't start.

**At its first start:**
- two migrations: `event_outbox`, `event_relay` and the triggers on `runs`, `jobs` and `steps`;
  `event_positions`, and `webhook_deliveries` and `notify_deliveries` gain their queue's columns;
- the bus makes `<data>/bus`: identities (two ECDSA keys, sealed under `identities.key`), the
  catalog's log (about 3 MB at once), and `kiln.events`'s part and log as it's made;
- the readers start at the topic's end: deliveries are of what happens after.

**Memory:** each persisted part may hold `part_memory` (the catalog's and `kiln.events`'s: up to
128 MiB together), beside the live feeds' `[bus] memory`; `kiln.events` keeps up to 64 MiB of
events for its readers. Kiln's events are a few hundred bytes each.

**Disk:** each part's log is about twice its memory at most (about 256 MiB for both); a pending
delivery keeps its body in kiln.db until it's sent or given up (a day).

## Watching it

On production's `/metrics` and the overview dashboard's "Events" row (docs/bus.md, "Metrics"):
- `kiln_events_outbox_pending` stays near 0: rows waiting to be relayed. Growing: the relay can't publish
  (a warning `relay_stopped` on target `kiln.events` says why).
- `kiln_events_delivery_attempts_total{kind,result}`: `retry` and `failed` move when a subscriber
  or channel is down; `kiln_events_deliveries_pending{kind}` shows the backlog it holds.
- `kiln_events_missed_total`, `kiln_events_unrecorded_total`, `kiln_events_unknown_kind_total` stay at 0.
- A start logs `relay_start` (rows published and not recorded, rows to publish) and
  `senders_start` (deliveries waiting) on target `kiln.events`; `kiln.bus`'s `persisted_refused`
  means the server runs without the persisted class.

## Risks

- **A subscriber that's down holds its own deliveries** for up to a day, then they're `failed`
  (redeliverable). Before, they were dropped after 77 s (webhooks) or 17 s (rules).
- **A notification goes twice** when the server is killed between sending it and recording that it
  went: the same `X-Kiln-Delivery` for webhooks; chat channels show it twice.
- **A run deleted before its event is read** makes no `run` or `job` delivery (its body is made from
  the run as it is).
- **A release's first chat message may show a later state:** its messages go out from kiln.events,
  a move after it changes, so a release that moves on before then (asked for and rejected at once)
  gets one message, posted as it is by then, and no edit. Before, its first message went out at once.
- **Throughput:** one relay publishing up to 128 events a message, a flush a message; on adacachy's
  loaded disk it kept up with the lab's writer (100 changes a second) between kills.
- **Runners on the release before** that hold kiln.db's replica (P2's learners) replay the
  server's writes with their triggers on, so a replayed run or job change may add an outbox row of
  their own beside the server's: never read on a runner, and overwritten as the server's rows
  arrive; gone once the runner takes the release and a fresh copy.

## Rollback

The release before runs on what this one leaves:
- it skips the migrations it doesn't know; its webhooks send the pending deliveries this one
  logged, on their old schedule;
- **the triggers stay** and go on filling `event_outbox`, which nothing relays: a few hundred
  bytes a change. Rolled forward again, they're relayed and delivered late. For a rollback meant to
  last, drop them: `DROP TRIGGER event_run_made; DROP TRIGGER event_run_changed; DROP TRIGGER
  event_job_made; DROP TRIGGER event_job_changed; DROP TRIGGER event_step_made; DROP TRIGGER
  event_step_changed;` (and `DELETE FROM event_outbox`), and a later release puts them back by a
  migration of its own;
- `<data>/bus` stays, unused; the release after takes it up again.

## Open

- **hyper-raft main is at fb74b59**: kiln's recorded change 0003 conflicts with its driver fix,
  and the runtime program, whose change it is, refreshes it; kiln and Quiver move there together
  after.
- **Windows clippy and `w28_kilns_own_workflows` fail on main's own tree** (main's W20 legacy
  custody: code only Unix calls, and `.kiln/windows-recovery.yaml`); the agent that pushed it is told.
- **`scripts/locks.py check` fails on main's own code** (async locks and channels in
  `ci/actions.rs`, `ci/dispatch.rs`, `offers/mod.rs`, `reach/told.rs` not listed in
  `scripts/locks/hot.toml`); this branch changes none of them.
- **CP-T2.1's live-feed fix (04854b2ec1b) is left out**: main replaced the code it fixed (lockless
  session copies, a topic per generation); its three bugs are to be checked on main's feed.
- **The relay's and senders' looks are periodic** (`LOOK_EVERY`, `SENDER_LOOK_EVERY`, the hourly
  prune), on the injected clock, which kiln's no-clock-without-a-due-item lint doesn't see: to be
  made event-driven (a trigger's row told on commit).
- **An idle persisted node's CPU** is unmeasured in a release build: in a debug test binary,
  three of them took about 40% of a core each.
