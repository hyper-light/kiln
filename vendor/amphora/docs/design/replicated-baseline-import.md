# Importing a local baseline into the product group (U15 / CP-T1.8)

A resolved page transaction is a delta, not a complete database. Applied index zero does not
identify the pages at that index. Opening an existing local database as index zero while opening
its followers empty violates Raft state-machine safety before any transaction runs.

The initial group therefore uses an explicit declaration naming one source node and the SHA-256
identity of its original, ordered, postcard-encoded full page image. Every initial member uses the
same declaration. `amphora_api::replication::ReplicaConfig::bootstrap` is an optional
`Bootstrap { source_node: u64, source_id: [u8; 32] }`. The product helper
`amphora::replication::bootstrap::inspect_source(path, source_node)` authors that declaration for
an explicitly chosen source; callers persist and distribute the same result. `is_replica(path)`
checks both pending-baseline and replica ownership before startup considers local legacy restore.
Both helpers hold the product lease and open the engine with native read-only flags, without
initialization or write PRAGMAs. An existing live owner must be attached through its transport
before attempting file-based migration inspection.
Connection order, election results and most recent activity never select a
source. A non-source member with an unmarked nonempty database refuses adoption, even if it might
contain an equivalent SQL dataset. A conflicting durable declaration also refuses reopening.

Before Raft opens, the source captures the complete original image, verifies the declared identity,
and retains that exact image in a product-owned baseline row using a real flushed WAL transaction.
An empty follower persists the same declaration with no image. This pending marker is not evidence
of quorum commitment. Elections can write replica metadata, so retry must use the retained original
image rather than recapture the changing physical database. Pending adoption blocks every canonical
SQL operation, including reads. It also blocks resolved page preparation and application.

After ReadIndex, the declared source leader proposes successive bounded image chunks through the
existing hyper-durable Replica. Each chunk carries the declaration, total size and prefix offset.
Every member durably stages the same contiguous prefix and cursor with its actual applied point.
The final chunk validates the full SHA-256 identity and complete contiguous page sequence before
importing all pages and writing the completed baseline marker and actual applied point in the
same engine transaction, then flushing. Only the durable application result releases admission to
normal operations. A subsequent ReadIndex is required before evaluating a waiting SQL closure.
A lost answer is an unknown outcome: reopening the same declaration and retrying converges on the
same committed image; it must not select a new source or overwrite an already completed baseline.

The image obeys the existing 256 MiB snapshot page ceiling and snapshot encoding allowance. Each
chunk obeys the normal command ceiling and hyper-log's actual `entry_room()` after subtracting
GroupStore framing and context. Framing reserves 1 KiB, larger than the fixed prefix, declaration
and three postcard length/cursor fields; this is an encoding bound, not a tuning constant. Oversize
input is a typed refusal with no raised limit. Prefix staging never publishes partial application
data. Original image retention and follower prefix buffers are bounded by the snapshot encoding
budget and are removed after durable seed application. A chunk transition may temporarily hold
the old and new prefix, each charged to that same finite image bound.

Snapshot creation and actor compaction refuse while adoption is pending, so a source snapshot
cannot recursively capture its retained image. Completed snapshots carry an explicit declaration
envelope and durable completion marker. Installing a snapshot of a pending source cannot count
as a committed baseline. A conflicting declaration must be refused
before any imported page changes the database. A completed replica cannot be replaced by a second
seed: repeated committed proposals of the same declaration only advance their applied point.

The product Machine owns baseline staging, typed SQL gates, delta identity fencing and snapshot
identity fencing. Its actor owns ReadIndex and quorum proposals; no host Raft controller is added.
Startup integration must finish
legacy SQL adoption before exposing canonical operations; it must not silently skip a legacy
restore when a replicated handle exists.

Evidence: the pinned [hyper-durable StateMachine contract](../../vendor/hyper-raft/crates/hyper-durable/src/machine.rs)
and [Replica implementation](../../vendor/hyper-raft/crates/hyper-durable/src/replica.rs) own
commit/application and snapshot ordering; the [GroupStore entry framing](../../vendor/hyper-raft/crates/hyper-durable/src/hyperlog.rs)
and [hyper-log entry-room API](../../vendor/hyper-raft/crates/hyper-log/src/lib.rs) define existing
admission limits. Raft's identical committed state and snapshot requirements are described in
[Ongaro's dissertation, chapters 3 and 5](https://raft.github.io/raft.pdf).

Falsified by: a source's unchanged base row missing on a follower after a later small delta; SQL
being evaluated before seed application; a different declaration accepted on restart; a nonempty
follower overwritten; a second seed changing completed state; or a lost seed answer causing a
new image to be captured. Source mismatch and retained-image restart fixtures live in
`replication/bootstrap.rs`; real quorum coverage belongs to the product actor fixture.

The actor retains at most 512 MiB for this one group inside its existing 1 GiB / sixteen-segment
log ring. A maximal encoded seed is at most
257 MiB and uses at most nine normal-sized chunks; chunk/context framing is below another MiB.
Nine 64 MiB segment slots leave seven slots, including the mandatory sweep reserve. This derives
retention from the supported input rather than raising the largest-entry or snapshot ceiling.
Pending adoption disables compaction; a stalled bootstrap fences before user publication.
