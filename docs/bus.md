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

- **A topic for each session's mirror.** The server's view of a session (`crate::harness::registry`)
  has a feed: a topic of its own, `sessions/<id>/<n>` (`<n>` the feed's number in this process, so a
  mirror made again never shares a topic it could delete; an id outside a topic name's alphabet is
  its SHA-256 in its place), keeping its last 2048 frames. It's made with the mirror, and let go
  whenever nobody watches: the first frame after its last viewer leaves deletes it, and the next
  viewer makes it again, so a session nobody watches holds none of the bus's memory.
- **Frames go out only while someone watches,** as before: the JSON a viewer's page reads (`open`,
  `meta`, `state`, `state_patch`, `events`, `stream`, `changes`, `files`, `closed`). A state goes
  whole and as what changed in it, and each viewer takes the form it asked for (`?patch=state`).
- **No frame missed or seen twice.** A viewer (`GET /api/harness/sessions/:id/ws`) is sent a
  snapshot and reads the feed from the offset the snapshot was taken at: both are taken under the
  mirrors' lock, which every publish takes too. The lock holds only while the snapshot's parts are
  taken (its events by reference); the snapshot is made after it's let go, so a viewer starting
  over never holds up another session's frames.
- **A viewer that falls behind starts over, alone.** One further behind than the feed keeps is
  refused `Behind` by the bus (its typed lag notice), and it starts over from a fresh snapshot (with
  `{"t":"synced"}` after it), as a viewer that fell behind always did. A publish never waits for a
  viewer, so a stalled one holds up neither the session nor any other viewer.
- **Nothing dropped silently.** A frame the bus refuses (over 1 MiB, or the bus's memory full)
  marks the feed broken: its next frame goes after a resync marker, which sends every viewer reading
  it back to a snapshot, and the refusal is counted (`kiln_bus_refused_total`). A full bus may
  refuse the marker too, so no viewer relies on it: each counts its feed's refusals and, with
  nothing to read for 5 seconds, starts over if there were more since its snapshot. While the bus
  can't make a session's feed at all, its viewers are sent a snapshot every 5 seconds.
- **The end.** When the session ends its last frame is `closed`, and each viewer ends with it,
  having read whatever it hadn't yet. The topic goes with the last of them (at once if none
  watches); a viewer that finds it gone is told `closed` too.

## Settings

`[bus]` in kiln.toml:

| Setting | Default | What it is |
|---|---|---|
| `memory` | `memory = 268435456` (256 MiB) | The bytes the bus's messages may take, their framing included, until every reader has let them go (outside it: each segment's own slots, about 25 KB, and what a viewer's read copies on its way out). Past it a topic lets its oldest go to take a new message; one it can't make room for is refused, typed, and its viewers start over from snapshots. |
| `topic_bytes` | `topic_bytes = 16777216` (16 MiB) | The bytes one topic keeps at most (its keys and payloads), beside the messages it retains. |

The bus holds 65,536 topics and 65,536 subscriptions at once; a session's feed keeps 2048 frames.

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

kiln's overview dashboard (`deploy/grafana/dashboards/kiln-overview.json`) has a "Live feed" row of
them.

## Vendoring

kiln vendors Quiver (`vendor/quiver`), Focal's bus (`vendor/focal-bus`, branch `bus-replicated` at
`f8052a5`) and hyper-raft (`vendor/hyper-raft`, at `25cf889`, its `main` and the revision Focal's bus
takes) at the revisions Quiver takes them, with Quiver's recorded changes (vendor/MANIFEST.toml,
docs/vendoring.md). kiln's own transport, runtime and
replicated stores (sync, CD) moved with it from hyper-raft `767627f` to `25cf889`, so kiln links one
revision of each, the one Mantle takes too. In kiln's copy, hyper-durable's
`RamStore` is compiled only for tests: kiln's sync tests ask for it through a dev-dependency.
