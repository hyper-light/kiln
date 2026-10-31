# Bounded Records compaction

Records GC runs as ordinary work on the existing namespace writer. It has no background
controller, client cursor, reserved SQL interface or separate database. `compact_step(before,
until)` commits a recoverable prefix and returns `CompactionProgress`; `compact` repeats those
steps under the same absolute deadline until the first response's accepted horizon is complete.
A concurrently raised horizon does not make the convenience call chase a growing head.

A successful step can advance the logical snapshot horizon while physical cleanup remains.
Snapshots below that horizon are refused immediately. The feed horizon is at least
`horizon - 1` (saturating at zero), preserving events exactly at the horizon for an allowed
`changes(after = horizon - 1)`. A later timeout/refusal does not undo prior steps. After mutation
submission, response loss is an unknown outcome: callers reconcile durable progress, rather than
assuming the horizon or cursor stayed unchanged.

## Durable progress and retention

One product-owned `__amphora_compaction` row per namespace records a fixed active target, completed
target and nullable `(family, key, revision)` cursor. Its table is created lazily inside the same
writer transaction/prepared replicated command. It is not unreplicated startup metadata. The
existing namespace horizon is the wanted target; no second wanted-target registry is needed.
Higher requests can raise that horizon without resetting an active cycle. At cycle completion,
the next target is the then-current horizon.

Each step visits at most `TX_MAX` versions at or below its fixed target, in primary-key order,
using separate prefix/range seeks. It selects the next version from the working transaction;
there is no preselected cross-key deletion batch. The cursor advances across retained versions
as well as deleted versions. New writes have higher revisions and cannot create work below the
captured target. Deadline checks also bound SQL VM work; a visit count does not promise a fixed
read-I/O budget in a tree containing later revisions.

For each key, keep its latest live baseline at or below the target, even when its revision is
older than the snapshot horizon. Current `Cond::At` still accepts that live revision. Delete
superseded versions first. Delete a latest tombstone only strictly below the target and only
when no older version for that key remains in the working transaction. Thus a partially completed
cycle cannot resurrect an old value, and the boundary feed event remains observable. The cursor,
deletions and horizon changes commit atomically. Recovery resumes from that cursor.

## Dirty-page and serialized bounds

A row limit is not a resolved-page limit: one indexed deletion can touch overflow, freelist,
index and ancestor pages. During compaction the existing pager therefore admits at most a fixed
number of distinct dirty page IDs. It refuses a new ID before unsharing, subjournaling, inserting
it into the dirty bitmap or modifying its contents. The bitmap includes spilled pages. The fence
covers metadata/catalog/progress pages as well as record pages.

Prepared commands use 4096-byte pages. For postcard encoding, the worst-case fixed overhead is
81 bytes: the optional 17-byte baseline prefix, a 10-byte source-node ID, 32-byte source identity,
two 10-byte base-point integers and a 2-byte page-vector length. Each page costs at most 4103
bytes: a 5-byte page ID, 2-byte vector length and 4096-byte payload. Let `R` be the smaller of
`REQUEST_MAX` and the existing replica log's available entry bytes after its own/context overhead.
The page cap is `floor((R - 81) / 4103)`; a cap below one is an explicit refusal. At `REQUEST_MAX`
(33,554,432), 8178 pages require at most 33,554,415 bytes; 8179 would exceed the bound. The final
exact `Prepared::encode().len() <= R` check remains before proposal. Local compaction uses the
same conservative page cap. Other existing SQL operations retain their preparation policy.
Non-4096-byte databases explicitly refuse this capability; they do not use an incorrect bound.

Only the pager's typed page-admission error permits retrying deterministic GC. Each failed
attempt disables the fence before whole-transaction rollback, verifies autocommit and an empty
dirty bitmap, then halves its visit budget from 1000 through 1 (at most ten attempts). Savepoint
rollback is insufficient: restored preimages can remain dirty. No arbitrary IO/SQL/deadline error
or adaptive `FnOnce` session is replayed. If one visited version and its progress metadata cannot
fit, return typed `TooLarge`, without committing that attempt's cursor/horizon. This is an explicit
unsupported atom, not a guarantee that every theoretical valid tree shape fits. For atoms that
fit, successful steps advance a finite fixed-domain cursor or initialize/complete a cycle.

Failed rollback/reset permanently fences that owner's `Db`, shared with existing readers.
SQL batch/query/prepare, raw-page application, reader creation and checkpoint/restart entry paths
check that fence. A replicated cleanup fence stops the existing actor. Reopening after recovery
is the existing owner lifecycle; there is no automatic repair loop. Flushing an already committed
prefix remains allowed and cannot publish a speculative transaction.

## Embedded owner compatibility

`Hello.version` remains 2 and all existing request/answer/record discriminants stay fixed. The
new SDK probes an appended read-only `Capabilities` request, bit 0, on the same authenticated
stream used for one subsequent bounded-compaction step. The owner answers before acquiring a
Database/SQL/Raft session, accepts at most one small restricted continuation and never caches a
capability across owner connections. Probe replies and the continuation are capped at 256 bytes.
Existing clients' other operations remain interoperable.

An old version-2 owner closes when it cannot decode the appended probe. Missing/invalid capability
replies or read-only transport loss explicitly report support unverified and send no mutation;
a probe deadline stays `Timeout`. Once mutation transmission begins, loss or malformed response
is conservatively `UnknownOutcome`; typed owner refusals remain typed. There is no fallback to
legacy unbounded compaction. A legacy `Record::Compact` accepted by a new owner invokes this
bounded convenience path. The old whole-namespace DELETE implementation is removed.

The existing CAS/snapshot/feed/reopen, import/storage-oracle, deadline/cleanup and transport
behavior cases cover these invariants. Native acceptance is still required; source bounds and
syntax checks alone are not a performance or runtime acceptance claim.
