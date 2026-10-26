# Train notes: CP-T2.2's kiln events on Quiver's persisted class

CP-T2.2's part of a merge train (the critical path's CP-M4; its master plan's §6): step 4, the
persisted class in kiln's bus, and step 8, kiln's CI and deploy events on it (AC-T2.2.1,
T-T2.2.1): what it delivers, what deploying it does and how to watch it, its risks and its
rollback, and what's still open.

**Where it stands (2026-10-26):** on branch `cp/t2.2-events`, not on main, not deployed. Its
gates and labs are below; its landing waits on Ada's word, through Kiln Distro's train.

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

All on adacachy under its caps (a debug build, the machine beside production):
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
- `kiln_events_outbox` stays near 0: rows waiting to be relayed. Growing: the relay can't publish
  (a warning `relay_stopped` on target `kiln.events` says why).
- `kiln_events_delivery_attempts_total{kind,result}`: `retry` and `failed` move when a subscriber
  or channel is down; `kiln_events_deliveries_pending{kind}` shows the backlog it holds.
- `kiln_events_missed_total`, `kiln_events_unrecorded_total`, `kiln_events_unknown_total` stay at 0.
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

- Kiln Metrics' word on the `kiln_events_*` names.
- The gate's 12 container tests, failing on adacachy's Docker (its address pools taken by another
  gate's leftover containers), to run again once they're gone.
- Focal's and Mantle's agents told of the re-vendor (their Mac's disk was full).
