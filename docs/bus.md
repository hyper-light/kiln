# The bus

kiln's events ride a bus: Quiver, the topic store (github.com/hyper-light/quiver), vendored at
`vendor/quiver`, its node in the server's process. kiln reaches it only through kiln-seams' `Bus`
(docs/seams.md, "Bus"; `crate::bus`). Quiver keeps each topic's messages in memory, the ephemeral
class of Focal's doc 33, each message a Focal batch charged to one memory budget, and a reader
behind what a topic keeps is refused `Behind`, never skipped silently. It carries one thing so far,
the live feed of every agent session (the critical path's CP-T2.1); kiln's CI and deploy events
come next, on the persisted class (CP-T2.2).

## The live feed

What the server learns of a session, from the machine it runs on, goes to every viewer watching it
on any device (`crate::harness::live`):

- **A topic for each session's copy.** The server's copy of a session (`crate::harness::copies`)
  has a feed: a topic of its own for each generation of it, `sessions/<id>/<n>.<g>` (`<n>` the
  feed's number in this process, so a copy made again never shares a topic it could delete; `<g>`
  its generation; an id outside a topic name's alphabet, or too long for the name, is its SHA-256
  in its place), keeping its last 2048 frames. The first viewer makes it, and it's let go whenever
  nobody watches: the first frame after its last viewer leaves deletes it and starts the next
  generation, whose topic the next viewer makes, so a session nobody watches holds none of the
  bus's memory and no viewer ever reads a topic that was let go.
- **One shard owns each copy.** A session's copy belongs to the shard its id hashes to: only that
  shard's owner changes it, taking runners' updates and people's commands as messages, in order,
  from a bounded inbox (as many as the hub's queue of runner events; a sender it has no room for
  waits, never dropping one, and never holding its own shard). After each change the owner
  publishes the copy as an immutable view, swapped in whole: viewers, snapshots, catch-ups and
  every read of a session load it without a lock and never wait on the owner. No lock guards a
  session's copy (goal 4, plan 01a1a59c2a9c; plan 01a17197 step 9).
- **Frames go out only while someone watches,** as before: the JSON a viewer's page reads (`open`,
  `meta`, `state`, `state_patch`, `events`, `stream`, `changes`, `changes_patch`, `files`,
  `closed`). A state goes whole and as what changed in it, and each viewer takes the form it asked
  for (`?patch=state`); so does a changeset (`?patch=changes`). With every viewer taking patches (a
  page does), a state or changeset goes as its patch alone, so a long session's changeset (megabytes
  of diffs) is never put on the bus whole; a viewer that takes them whole gets them whole again
  from its next frame on. Each frame is written straight into its bus message, its events' JSON as
  the copy keeps it, written out once: what a live frame, a snapshot and a digest take of an event
  is made one time, however many viewers take it.
- **No frame missed or seen twice.** A viewer (`GET /api/harness/sessions/:id/ws`) is sent a
  snapshot, or only what changed since the revision it says it has (`?after=`, docs/wire.md,
  "Revisions"), and reads the feed from the offset taken just before the view it was made of. It
  joins without asking the copy's owner: counted among the viewers first, then the feed's end, then
  the view. The owner publishes each view, then asks whether anyone watches, and sends the change's
  frame only then: of the two, one sees the other (a Dekker handshake on the viewers' count and the
  view, both sequentially consistent), so every change is in a viewer's view or in a frame past
  where it reads from. The frames of a snapshot are written on the blocking pool (gzip'd there too,
  for a page that asks `?z=gzip`), so a viewer starting over never holds up the copy's owner,
  another session's frames, or the sockets on the shard it came on.
  Each frame on the feed carries the copy's revision, and one a snapshot or catch-up already held
  is skipped.
- **A viewer that falls behind catches up, alone.** One further behind than the feed keeps is
  refused `Behind` by the bus (its typed lag notice), and it catches up from the last revision it
  was sent: only what changed since (`resumed`), or the session whole (`open`) when the copy can't
  tell, with `{"t":"synced"}` after it. A publish never waits for a viewer, so a stalled one holds
  up neither the session nor any other viewer. One that has to catch up again waits first until
  as long has gone by since its last catch-up as that one took to make and send: a viewer spends at
  most half its time catching up, so one slower than its session (or a feed refusing frame after
  frame) costs the server snapshots at the pace it makes them, not one a frame.
- **Nothing dropped silently.** A frame the bus refuses (over 1 MiB, or the bus's memory full)
  marks the feed broken: its next frame goes after a resync marker, which sends every viewer reading
  it back to catch up as one that fell behind does, and the refusal is counted
  (`kiln_bus_refused_total`). A copy whose events went another way than its viewers have them (a
  runner that started again, docs/wire.md) publishes the marker too, and its viewers take it whole. A full bus may
  refuse the marker too, so no viewer relies on it: each counts its feed's refusals and, with
  nothing to read for 4 seconds, starts over if there were more since its snapshot, so every viewer
  of a broken feed starts over within 5 seconds of the refusal. While the bus can't make a session's
  feed at all, its viewers are sent a snapshot every 4 seconds.
- **The end.** When the session ends its last frame is `closed`, and each viewer ends with it,
  having read whatever it hadn't yet. The topic goes with the last of them (at once if none
  watches); a viewer that finds it gone is told `closed` too.

## Settings

`[bus]` in kiln.toml:

| Setting | Default | What it is |
|---|---|---|
| `memory` | `memory = 268435456` (256 MiB) | The bytes the bus's messages may take, their framing included, until every reader has let them go (outside it: each segment's own slots, about 25 KB, and what a viewer's read copies on its way out). Past it a topic lets its oldest go to take a new message; one it can't make room for is refused, typed, and its viewers start over from snapshots. At least 2,166,784 bytes, two of Quiver's largest messages with their framing (a topic keeping its largest, held by a reader, can still take another): less, and the server doesn't start. The default is Quiver's own as CP-T2.1 set it: a ceiling, which costs nothing at rest (Quiver's budget is a counter). Goal 11 lowered it to 171 MiB from the bus lab's figures, and its audit (row C10) put it back: a lower ceiling could only cost Quiver room it would use (docs/runtime-lab.md, goal 11). |
| `topic_bytes` | `topic_bytes = 16777216` (16 MiB) | The bytes one topic keeps at most (its keys and payloads), beside the messages it retains. The default is Quiver's own as CP-T2.1 set it, as `memory`'s is. The bus lab's largest topic asked for 3,672,418 bytes (a feed's newest 2048 frames). A feed whose frames are larger keeps fewer than 2048, and its viewers fall behind sooner. |

The bus holds 65,536 topics and 65,536 subscriptions at once; a session's feed keeps 2048 frames.

What the bus can hold follows the topics it holds, whatever `memory` allows, because each topic is held to
`topic_bytes`. For each topic, it can hold:
- the messages the topic keeps, with their framing (86 bytes a message at the most);
- its oldest segment's messages let go while that segment still holds kept ones (64 KiB at the most);
- a message being taken.

For each subscription, it can hold the segment the subscription reads and a message. That bound, capped at
`memory`, is `kiln_memory_bound_bytes{owner="bus"}` on `/metrics`: about 18.1 MB a feed at the defaults. A
server watching few sessions is bound by those, not by `memory`. Quiver fixes a node's memory when the node is
made, so `memory` itself doesn't follow the topics.

## Classes

Quiver serves the ephemeral class today. A topic of the replicated, persisted or windowed class is
refused (`Unavailable`, naming the class) until its goal lands (CP-T2.2, CP-T2.4), never kept as
another class.

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
| `kiln_bus_topic_bytes_peak` | The most bytes one topic has asked the bus to keep since the server started: a feed's newest 2048 frames, their keys and payloads as `topic_bytes` counts them, whether or not `topic_bytes` lets it (past it the bus keeps fewer frames). Each feed counts its topic's as it publishes (8 KiB of frame sizes a topic), so `topic_bytes` can be derived from a measurement (docs/runtime-lab.md, goal 11). |

kiln's overview dashboard (`deploy/grafana/dashboards/kiln-overview.json`) has a "Live feed" row of
them.

## Vendoring

kiln vendors Quiver (`vendor/quiver`), Focal's bus (`vendor/focal-bus`, branch `bus-replicated` at
`9aded7cd`) and hyper-raft (`vendor/hyper-raft`, at `0856e05`, its `main` merged with Mantle's line)
at the revisions Quiver takes them, with Quiver's recorded changes (vendor/MANIFEST.toml,
docs/vendoring.md). kiln's own transport, runtime and replicated stores (sync, CD) moved with it,
from hyper-raft `767627f` to `25cf889` and then to `0856e05` (kiln's CP-T10.2, so Mantle builds on
it too), so kiln links one revision of each. In kiln's copy, hyper-durable's
`RamStore` is compiled only for tests: kiln's sync tests ask for it through a dev-dependency.
