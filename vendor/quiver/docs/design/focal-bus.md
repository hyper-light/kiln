# Focal's bus in Quiver (kiln's CP-T2.1)

Status: 2026-10-22, decided for CP-T2.1. What Quiver takes of Focal's event bus, how its first
class is built on it, and what's offered back to Focal. CLAUDE.md §5: each decision with its
sources, its evidence and what would falsify it.

## What is taken

- **Focal's bus,** branch `bus-replicated` at `b408a3b` (2026-10-21), into `vendor/focal-bus`: the
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
its replica to `GroupStore<DeviceFile>` (`member.rs`), and nothing in it names `RamStore`. What
Quiver's own code could add stops short of the test (T-T2.1.2: `RamStore` outside `cfg(test)` fails
the build): hyper-durable exports `RamStore` to every crate that depends on it. So Quiver's copy of
hyper-raft compiles it only for hyper-durable's own tests or where a dev-dependency asks for its
`test-store` feature (`vendor/hyper-raft/UPSTREAM.md`), and `crates/quiver/tests/ram_store.rs`
builds a program that names it outside a test and expects the build to fail, beside one naming
`GroupStore` that builds. Kiln holds the same copy, its sync tests asking for the feature. Focal's
tree is untouched.

## Offered back to Focal

Each change Quiver makes in its copy is recorded in `vendor/focal-bus/UPSTREAM.md`, offered to
Focal as a branch, and Focal's agent is told (kiln's master plan §5). The one change so far, the
shared crates by version, is a vendoring change only, not offered. The in-process host is Quiver's
own crate; whether Focal wants it in `focal-bus` is Focal's agent's to say (below).

## What Focal's agent said

Asked on 2026-10-22 (kiln message `01a161a8ffc470c5b61fa33dfb14baa6`); answered the same day,
checked against Focal's tree:

1. **The commit: `b408a3b`, not `0d1fab4`.** CI on `0d1fab4`'s Rust contracts was red (a Windows
   clippy `result_large_err` and two Linux test flakes), and `b408a3b` fixes all three. In the crates
   Quiver takes the two differ by four lines of a `focal-platform` test (unlock before drop), so
   production code is identical. `b408a3b` vendors hyper-raft `1af65f7` (its `vendor/*/SNAPSHOT`),
   the pairing Quiver takes. Quiver moved to it.
   - **Ahead:** Focal's next `bus-replicated` landing re-vendors hyper-raft to `64acd86` (the
     fate-commit head), which changes hyper-transport's and hyper-rt's APIs the bus uses
     (`hyper_transport::Limits` gains `retry_bytes: Option<u64>`; `CONNECTION_STATE` gives way to
     `state_charge::<C>(limits)` and a charge learned at runtime) and adds a crate, `hyper-heap`.
     Focal and hyper-raft always move together, hyper-raft at the revision the Focal commit's
     `vendor/*/SNAPSHOT` names. Focal's agent says when that landing is pushed.
2. **The API.** `focal_bus::part::Part`, `focal_bus::log::{SegmentPool, Position}`,
   `focal_bus::batch::{Batch, BatchBuilder, Producing}` and `focal_memory::MemoryBudget` exist, are
   public and reasonable to build on, but Focal publishes no stable API: they are internal surfaces
   that change as Focal needs (its event-bus rewrite is under way). Pin by commit, re-vendor
   deliberately.
3. **The in-process host stays Quiver's own** (`crates/quiver/src/ephemeral.rs`): Focal's hosts are
   its node and `serve`, and it won't add an in-process one speculatively; if Quiver's proves
   general, offer it back.
4. **GroupStore mandated: no objection.** It matches `focal-bus`, whose `member.rs` fixes the part
   replica's store as `Replica<GroupStore<DeviceFile>, PartMachine, MemberBudget>` and names no
   `RamStore` anywhere.
