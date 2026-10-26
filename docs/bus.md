# The bus

kiln's events ride a bus: Quiver, the topic store (github.com/hyper-light/quiver), vendored at
`vendor/quiver`, its node in the server's process. kiln reaches it only through kiln-seams' `Bus`
(docs/seams.md, "Bus"; `crate::bus`). Quiver keeps each topic's messages in memory, the ephemeral
class of Focal's doc 33, each message a Focal batch charged to one memory budget, and a reader
behind what a topic keeps is refused `Behind`, never skipped silently. It carries the live feed of
every agent session (the critical path's CP-T2.1) on the ephemeral class, and kiln's CI and deploy
events on the persisted class (CP-T2.2): a topic's messages on the server's disk before a publish
is answered.

## The live feed

What the server learns of a session, from the machine it runs on, goes to every viewer watching it
on any device (`crate::harness::live`):

- **A topic for each session's mirror.** The server's view of a session (`crate::harness::registry`)
  has a feed: a topic of its own, `sessions/<id>/<n>` (`<n>` the feed's number in this process, so a
  mirror made again never shares a topic it could delete; an id outside a topic name's alphabet is
  its SHA-256 in its place), keeping its last 2048 frames. It's made with the mirror, and let go
  whenever nobody watches: as its last viewer leaves (and at the session's next change, should a
  viewer go without saying so), and the next viewer makes it again, so a session nobody watches
  holds none of the bus's memory.
- **Frames go out only while someone watches,** as before: the JSON a viewer's page reads (`open`,
  `meta`, `state`, `state_patch`, `events`, `stream`, `changes`, `files`, `closed`). A state goes
  whole and as what changed in it, and each viewer takes the form it asked for (`?patch=state`).
- **No frame missed or seen twice.** A viewer (`GET /api/harness/sessions/:id/ws`) is sent a
  snapshot, or only what changed since the revision it says it has (`?after=`, docs/wire.md,
  "Revisions"), and reads the feed from the offset that was taken at: both are taken under the
  mirrors' lock, which every publish takes too. The lock holds only while the parts are taken (the
  events by reference); the frames are made after it's let go, so a viewer starting over never holds
  up another session's frames. Each frame on the feed carries the copy's revision, and one a
  catch-up already held is skipped; what changes here rather than on the machine (a name given, the
  machine gone) takes a revision of its own, so a viewer that has everything never skips it.
- **A viewer that falls behind catches up, alone.** One further behind than the feed keeps is
  refused `Behind` by the bus (its typed lag notice), and it catches up from the last revision it
  was sent: only what changed since (`resumed`), or the session whole (`open`) when the copy can't
  tell, with `{"t":"synced"}` after it. A publish never waits for a viewer, so a stalled one holds
  up neither the session nor any other viewer.
- **Nothing dropped silently.** A frame the bus refuses (over 1 MiB, or the bus's memory full)
  marks the feed broken: its next frame goes after a resync marker, which sends every viewer reading
  it back to catch up as one that fell behind does, and the refusal is counted
  (`kiln_bus_refused_total`). A copy whose events went another way than its viewers have them (a
  runner that started again, docs/wire.md) publishes the marker too, and its viewers take it whole. A full bus may
  refuse the marker too, so no viewer relies on it: each counts its feed's refusals and, with
  nothing to read for 4 seconds, starts over if there were more since its snapshot, so every viewer
  of a broken feed starts over within 5 seconds of the refusal. While the bus can't make a session's
  feed at all, its viewers are sent a snapshot every 4 seconds.
- **The end.** When the session ends its last frame is `closed`, the bare marker whether its machine
  or this server ended it, and each viewer ends with it, having read whatever it hadn't yet: the
  server closes its socket. The topic goes with the last of them (at once if none
  watches); a viewer that finds it gone is told `closed` too.

## The persisted class

The server opens its bus as the one node of a cluster of one (`QuiverBus::open_server`): each
persisted topic a part of Focal's replicated by a Raft group of one, durable on the server's disk,
a publish answered once the group's log flushed it (Quiver's `docs/design/persisted.md`). Its data
is in `[bus] data`, else the data directory's `bus`: `node/` (the catalog's and each topic's log,
part and port), `identities/` and `identities.key` (the identities its node and publishers speak
as, made the first time). Its node listens on loopback at ports the system picks, since no peer
dials it.

- **What a topic holds.** Reads come from the node's view of what its group committed, under the
  topic's retention (`[bus] persisted_bytes`); after a restart the view holds what the part's
  window kept (`[bus] part_memory`), so the defaults keep the two alike: a restart holds what the
  topic held.
- **When it can't open** (its directory damaged, unreadable or not a directory), the server starts
  with the ephemeral class alone and says why (an error on target `kiln.bus`, event
  `persisted_refused`): the live feeds go on, and what would ride a persisted topic waits where it
  was made.

## kiln's events

kiln's CI and deploy events, and everything else its outgoing webhooks (docs/ci.md W19-R07) and
notification rules (W19-R06) take, go one way (`crate::events`, CP-T2.2 step 8): recorded with the
state they report, published to the persisted topic `kiln.events` in the order they were recorded,
and delivered from it in that order, at least once, so a kill -9 of the server loses none.

- **Recorded with the change (the outbox).** An event is a row of `event_outbox`, written in the
  SQLite transaction that changes what it reports, so the change and its event commit together or
  not at all (the transactional outbox). A run's or a job's change of status or conclusion is
  written by triggers on `runs` and `jobs`, whatever code made it; a run's end (`run.*`, `leg.flaky`),
  a deployment's end (`deploy.*`), a run waiting for its reviewers (`approval.waiting`) and a land
  queue's ejection (`land.ejected`) by the code that makes the change, in its transaction. What
  isn't a change of kiln.db (KilnCD's apps, a runner's comings and goings, an agent's turn) is
  recorded as it's said. The cluster group's replay writes a write's rows as they were, its
  triggers off, so a replica holds the same events, not twice them.
- **Relayed in order.** One relay publishes the outbox to `kiln.events` oldest first, each row's
  events with the row's number in them, up to 128 events (256 KiB) to a message so that one flush
  of the topic's log carries them, one message after another; then it records how far it got and
  lets the rows go. A restart goes on from the first row not recorded: of rows published before a
  kill and not recorded, the last goes again (its events with the numbers they had), the rest not.
- **Delivered from the topic.** Each kind of delivery reads `kiln.events` from a position of its
  own, kept in kiln.db: for each event it logs the deliveries the event makes (each subscription
  that takes it; each channel a rule sends it to), each with its body and a delivery id that is the
  same however often it's made (`X-Kiln-Delivery`), and moves its position past the event, in one
  transaction. An event it meets again, with a number it has passed, it skips. A worker for each
  target sends that target's logged deliveries one at a time, in order, retrying with exponential
  backoff for a day before it gives one up as failed (logged, counted, sent again on asking), so a
  target that's down holds up only itself, and its backlog waits in kiln.db, not on the topic.
- **Behind.** A position the topic has let go (`Refusal::Behind`, a reader stopped longer than the
  topic keeps) moves to the topic's oldest, saying how many events were missed (a warning on target
  `kiln.events`, counted): never skipped silently.

## Settings

`[bus]` in kiln.toml:

| Setting | Default | What it is |
|---|---|---|
| `memory` | `memory = 268435456` (256 MiB) | The bytes the bus's messages may take, their framing included, until every reader has let them go (outside it: each segment's own slots, about 25 KB, and what a viewer's read copies on its way out). Past it a topic lets its oldest go to take a new message; one it can't make room for is refused, typed, and its viewers start over from snapshots. At least 2,166,784 bytes, two of Quiver's largest messages with their framing (a topic keeping its largest, held by a reader, can still take another): less, and the server doesn't start. |
| `topic_bytes` | `topic_bytes = 16777216` (16 MiB) | The bytes one topic keeps at most (its keys and payloads), beside the messages it retains. |
| `data` | `data = ""` | Where the persisted class keeps its logs, parts and identities. Empty: the data directory's `bus`. |
| `part_memory` | `part_memory = 67108864` (64 MiB) | Each persisted part's memory: what its window keeps of its messages, and so what its topic holds again after a restart; its log on disk is about twice it. At least 25,165,824 (24 MiB), the least a part of Focal's takes. |
| `persisted_bytes` | `persisted_bytes = 67108864` (64 MiB) | The bytes a persisted topic keeps for its readers (its keys and payloads): for kiln's events, what a delivery target away the longest may still read. Its messages are charged to `memory` with the live feeds'. |

The bus holds 65,536 topics and 65,536 subscriptions at once; a session's feed keeps 2048 frames.

## Classes

Quiver serves the ephemeral class, and on the server the persisted class (above). A topic of the
replicated or windowed class is refused (`Unavailable`, naming the class) until its goal lands
(the windowed class's is CP-T2.4), and so is a persisted one on a bus whose persisted class didn't
open: never kept as another class.

## Metrics

| Metric | What it counts |
|---|---|
| `kiln_bus_published_total` | Frames sessions' live feeds published. |
| `kiln_bus_refused_total{op}` | What the bus refused a live feed, by operation (`create`, `publish`, `delete`): a refused create or publish sends the feed's viewers back to a snapshot (a warning on target `kiln.bus`); a refused delete leaves an ended session's topic behind (info). |
| `kiln_bus_lag_notices_total` | Viewers told they fell behind what a feed keeps, and sent back to a snapshot. |
| `kiln_bus_delivery_seconds` | How long a frame took from its publish to a viewer's read. |
| `kiln_bus_topics` | Topics the bus holds. |
| `kiln_bus_viewers` | Viewers reading sessions' live feeds. |
| `kiln_bus_memory_bytes`, `kiln_bus_memory_limit_bytes` | Bytes the bus's messages take, and may take. |

kiln's events (above):

| Metric | What it counts |
|---|---|
| `kiln_events_relayed_total` | Outbox rows the relay published to `kiln.events` and let go. |
| `kiln_events_published_total` | Events published to `kiln.events` (a row makes none, one or several). |
| `kiln_events_outbox` | Outbox rows waiting to be relayed, as the relay last saw. |
| `kiln_events_unrecorded_total` | Events said that couldn't be recorded, so won't be delivered (a warning on target `kiln.events`). |
| `kiln_events_unknown_total` | Outbox rows of a kind this build doesn't know (a later build's, after a rollback): they make no event. |
| `kiln_events_deliveries_logged_total{kind}` | Deliveries the topic's readers logged, by kind (`webhook`, `notify`). |
| `kiln_events_delivery_attempts_total{kind,result}` | Tries of those deliveries, by kind and result: `sent`, `retry` (tried again later), `failed` (given up after a day, or never able to go), `skipped`. |
| `kiln_events_delivery_seconds{kind}` | How long a delivery waited from its logging to its sending. |
| `kiln_events_deliveries_pending{kind}` | Deliveries waiting to be sent, as their senders last counted. |
| `kiln_events_missed_total{reader}` | Events a reader never took, the topic having let them go first (a warning on target `kiln.events`). |

kiln's overview dashboard (`deploy/grafana/dashboards/kiln-overview.json`) has a "Live feed" row and
an "Events" row of them.

## Vendoring

kiln vendors Quiver (`vendor/quiver`, at `44bae34`, whose simulation of the persisted class ran
10,000 seeds alike in two passes and lost no acknowledged publish), Focal's bus (`vendor/focal-bus`,
branch `bus-replicated` at `b6cf8e3`) and hyper-raft (`vendor/hyper-raft`, at `c6baec2`, its `main`
and the revision Focal's bus takes) at the revisions Quiver takes them, with Quiver's recorded
changes: the bus's five and hyper-raft's six (vendor/MANIFEST.toml, docs/vendoring.md, each copy's
`UPSTREAM.md`). kiln's own transport, runtime and replicated stores (sync, CD) moved with it, from
hyper-raft `767627f` to `25cf889` (CP-T2.1) and on to `c6baec2` (CP-T2.2), so kiln links one
revision of each. In kiln's copy, hyper-durable's `RamStore` is compiled only for tests: kiln's sync
tests ask for it through a dev-dependency.
