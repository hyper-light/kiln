# Focal's bus in Quiver (kiln's CP-T2.1)

Status: 2026-10-22, decided for CP-T2.1. What Quiver takes of Focal's event bus, how its first
class is built on it, and what's offered back to Focal. CLAUDE.md §5: each decision with its
sources, its evidence and what would falsify it.

## What is taken

- **Focal's bus,** branch `bus-replicated` at `0d1fab4` (2026-10-21), into `vendor/focal-bus`: the
  crates `focal-bus` (Focal's docs 32 to 34: the batch, a part's log and streams, the node over
  QUIC, the replicated part over hyper-durable), `focal-memory` (the budget every byte is charged
  to), `focal-seal` and `focal-platform` (the node log's keys, the files a part's image is installed
  through) and `focal-timing` (the bus's tests' waits), with Focal's LICENSE and root manifest.
  `vendor/focal-bus/UPSTREAM.md` lists Quiver's one change: the shared crates named by version, so
  whoever builds the bus points them at one copy.
- **hyper-raft** at `1af65f7`, the revision this commit of the bus takes (Focal's `vendor/README.md`
  names all fifteen shared crates there), into `vendor/hyper-raft`, unchanged: the runtime, the
  transport over QUIC and TLS, the consensus core, the durable shell over the log and the block
  layer, the liveness stream, sealing at rest, and their measurement and simulation crates.
- **One revision in kiln.** Kiln holds one copy of each hyper-light product (kiln's
  docs/vendoring.md): it vendors Focal's bus and hyper-raft at these same commits, beside Quiver,
  and moves its own hyper-raft from `767627f` to `1af65f7`.

## The ephemeral class, in process

Focal's doc 33 (§4.2) defines the class: in memory, lost with its node, gaps announced. Doc 32's
step 2 built it as a node serving its part over QUIC. Kiln's first use is in one process: the
server's live feed of each session to every browser watching it, where one stalled browser must
never delay the others (AC-T2.1.5), at 100,000 messages a second fanned out to a thousand
subscribers (AC-T2.1.2, AC-T2.1.3).

Two of the node's choices don't fit that use, by its own design:

- **Its lanes are lossless.** "A subscriber whose lane is full holds the publishers feeding its
  stream at their own lanes" (doc 32 §4.3): right for the ledger's claims, where nothing owed may
  be let go, and the opposite of what a live feed needs, where a stalled viewer must not hold up a
  publish.
- **A delivery is a copy to each subscriber's lane** (doc 32 §4.3, "five user copies of each
  message"): over the network there's no other way, but in one process a thousand subscribers
  would copy each message a thousand times.

So the ephemeral class in process is Quiver's own layer (`crates/quiver/src/ephemeral.rs`), built
on the bus's format and budget:

- **Each message is a Focal batch** of one record, built by `focal_bus::batch::BatchBuilder`, its
  CRC-32C computed once as it's published: the bytes Focal's node appends, replicates and delivers,
  so the classes that come next (CP-T2.2 on) and a message forwarded over Focal's wire carry the
  same bytes. The record is Quiver's: the key's length plus one (a LEB128, 0 for none), the key,
  the payload.
- **Every byte is charged to one `focal_memory::MemoryBudget`** (the node's `memory`), held by the
  message until the last reader lets it go. A publish the budget can't take lets the topic's
  oldest go, a segment at a time, then is refused `Full`, typed.
- **A topic is a log of segments,** each up to 256 messages or 64 KiB, filled in order and never
  changed after. A publish takes the topic's lock to give the next offset and fill the next slot;
  readers take none. A subscription holds the one segment it reads and reaches the next by a weak
  link, so what it reads is shared, never copied, and a subscription that stalls keeps alive its
  own segment and none after it.
- **A reader behind what's kept is told so:** a read is refused `Behind`; a subscription takes a
  lag notice (`Next::Lag`, the offsets it missed) and goes on from the oldest kept. A publish waits
  for no reader.

The replicated and persisted classes are Focal's replicated part (`focal_bus::member`, `group`):
CP-T2.2 builds on it, and until then a topic of those classes is refused `Unsupported`, never kept
as ephemeral.

**Evidence and what would falsify it.** The conformance suite (`quiver-api`, QUIVER-1 to 19) and
the stalled-subscriber test (`crates/quiver/tests/stalled.rs`: one stalled among 100) hold the
semantics; the bench harness (`bench/`) measures AC-T2.1.2 to AC-T2.1.4. The layer is falsified if
the bench can't keep p99 publish-to-deliver under 300 µs at 100,000 a second with a thousand
subscribers, or if any subscriber that keeps up loses a frame in the hour's soak.

## GroupStore mandated

The plan's target (docs/design/README.md): the write-ahead log is mandatory, hyper-durable's
`GroupStore` on a real device with the full flush, `RamStore` test-only. Focal's bus already fixes
its replica to `GroupStore<DeviceFile>` (`member.rs`). Quiver keeps it so: a compile-fail test
fails the build where `RamStore` is named outside `cfg(test)` (T-T2.1.2).

## Offered back to Focal

Each change Quiver makes in its copy is recorded in `vendor/focal-bus/UPSTREAM.md`, offered to
Focal as a branch, and Focal's agent is told (kiln's master plan §5). The one change so far, the
shared crates by version, is a vendoring change only, not offered. The in-process host is Quiver's
own crate; whether Focal wants it in `focal-bus` is Focal's agent's to say (below).

## What Focal's agent said

Asked on 2026-10-22 (kiln message `01a161a8ffc470c5b61fa33dfb14baa6`): the commit to take, the API
to build on, the in-process host, and GroupStore. Its answers are recorded here as they come.
