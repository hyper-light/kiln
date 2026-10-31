# The storage engine: many writers on one database (CP-T1.3)

Amphora's engine (`crates/engine`) is Turso's as of 3e09a82: SQLite's design, one writer at a time,
each commit a set of whole pages appended to a write-ahead log, each connection with a page cache of
its own. Its SQL layer (parser, planner, bytecode executor) stays. This document records the storage
under it that lets many writers change one database at once, the choices it is made of, their
sources, the evidence and what would falsify each (CLAUDE.md §5). Until it beats the engine it
replaces on the comprehensive suite (`bench --suite`), it is built beside that engine and the
current engine stays the default.

## The evidence that asks for it

Measured on fedora (32 cores, Samsung 9100 PRO), 2026-10-21 and 2026-10-22, 200,000 rows of the
suite's `items` with four secondary indexes; the 10-22 runs are checks at load 17–50 from other
work, not recorded runs.

- **One writer is the ceiling.** `insert_batch_100` (a hundred rows a statement) at 32 clients:
  Amphora's store writes 456–463 statements a second with its writer's thread busy throughout
  (store.md, decision 23), where OrioleDB writes 2,814 and PostgreSQL 1,854, from as many backends
  as clients. A profile of the writer: 66% inserting into indexes, 16% committing, 10% building log
  frames; nothing left that a thread could give back sixfold.
- **Whole pages logged.** A commit logs every page it touched, 4 KiB each: a hundred random index
  inserts log hundreds of pages. PostgreSQL logs a row's change (and a page's image only on its
  first change after a checkpoint), OrioleDB and InnoDB the row's change.
- **The MVCC inherited from Turso doesn't serve as the way out.** Turso's MVCC
  (`crates/engine/mvcc`, after Larson et al., below) keeps every row version a commit writes in
  in-memory skiplists beside the B-tree, merges the two on every read and checkpoints the skiplists
  into the B-tree. The whole suite on it, every write in `BEGIN CONCURRENT` from a connection a
  client (the bench's `mvcc` target, 2026-10-22), against Amphora's store at the same commit: reads
  0.64 times the store's throughput at the median, point reads at 32 clients 0.19–0.25 times,
  `table_count` 30,117 → 33 a second; writes 0.87 times at the median (single-row writes at 32
  clients 0.26–0.65 times); `insert_batch_100` 1.6–2.3 times; peak resident memory 4.5 GB against
  1.3 GB. With its checkpoints on the commit path a hundred-row batch at one client waited up to
  311 ms (p99); moved to a thread of their own, 20 ms. Its costs are in its design, not its tuning:
  a second copy of every recent row that each read probes, and checkpoints that walk it.

## 1. In-place versions in one shared buffer pool

**Decision.** One copy of each page, in one buffer pool every connection of the database shares;
writers change rows in place, in the pages, under page latches, concurrently; the version a row
had before an uncommitted or recent change is kept in the changing transaction's undo record, and
readers of older snapshots reach it from there. Reads go to the B-tree alone. This is the design of
InnoDB, Oracle, HyPer, Umbra, LeanStore and OrioleDB, against the design of the engines that keep
versions apart from the base data (Hekaton, Turso's MVCC) or append them beside it (PostgreSQL's
heap).

**Why.** It is the design whose reads cost what a single-writer engine's do (one structure, the
newest version where the B-tree has it), whose memory is one copy of the data plus the undo of
changes some snapshot can still see, and whose writers change different rows of the same table at
once. Turso's MVCC measured the other choice: a second structure costs every read (above).

**Sources.** Wu, Arulraj, Lin, Xian, Pavlo, "An Empirical Evaluation of In-Memory Multi-Version
Concurrency Control" (PVLDB 10(7), 2017): delta (undo) version storage, newest version in place and
deltas newest to oldest, beats append-only and time-travel storage for updates and keeps scans on
the base data. Neumann, Mühlbauer, Kemper, "Fast Serializable Multi-Version Concurrency Control for
Main-Memory Database Systems" (SIGMOD 2015): in-place updates with undo buffers. Leis, Haubenschild,
Kemper, Neumann, "LeanStore: In-Memory Data Management beyond Main Memory" (ICDE 2018), and
Alhomssi, Leis, "Scalable and Robust Snapshot Isolation for High-Performance Storage Engines"
(PVLDB 16(6), 2023): the same for a buffer-managed engine whose data outgrows memory. OrioleDB's
architecture notes (github.com/orioledb/orioledb, `doc/arch.md`): row-level undo, no buffer mapping
for hot pages, row-level WAL. Larson, Blanas, Diaconu, Freedman, Patel, Zwilling, "High-Performance
Concurrency Control Mechanisms for Main-Memory Databases" (PVLDB 5(4), 2011): the design Turso's
MVCC follows.

**Falsified by.** At 32 clients on the suite, writes no faster than the single-writer store, or
reads slower than it, on the same hardware and data.

## 2. The buffer pool: fixed frames, a latch each, CLOCK, a bound

**Decision.** The pool holds a fixed number of frames, sized by its budget (a typed refusal when a
transaction would need more pinned frames than there are). Each frame holds one page, a latch
(shared or exclusive), a version counter bumped by every exclusive release, a pin count and a
dirty mark; a page table maps page numbers to frames, sharded by page number. Eviction is CLOCK
over unpinned clean frames; dirty frames are written by the checkpointer (decision 7) and become
clean only then. One pool serves every connection of the database: the per-connection page caches
and decision 20's shared pages give way to it.

**Why.** Today each connection caches its own copy of the pages it reads (decision 5 of store.md:
32 readers at 2,000 pages each could hold 256 MB of mostly the same pages); concurrent writers
can't each keep a private copy of a page they all change. The suite's peak resident memory fell from
1,315 to 423 MB with one shared pool beside the private caches (store.md, decision 20, checked
2026-10-22); one pool instead of them is the step after.

**Sources.** PostgreSQL's shared buffers (`src/backend/storage/buffer/README`): a buffer table, a
pin count and a content lock per buffer, clock-sweep replacement. Corbato's CLOCK as in store.md,
decision 19.

**Falsified by.** Point reads at 32 clients slower than with the per-connection caches, or memory
beyond the pool's budget.

## 3. The B-tree: right links, leaf latches, inner pages read without writing

**Decision.** B-link trees (each page knows its right sibling and its upper bound, so a reader that
arrives at a page just split follows the link). A writer latches a leaf exclusively and, when it
must split, latches the parent only after the leaf's split is published, never holding two latches
on the way down. The root is the exception: its split makes the new root before the root's latch
goes, so no writer reaches the root's new page before the level above it exists (released first, a
writer that moved right into that page could split it and find no level to post to). Inner pages
change only on splits; readers descend through them without taking their latches: each thread keeps
copies of the inner pages it has read, keyed by page and the frame's version counter, and uses its
copy while the counter it reads (an atomic load, a read of a line no reader writes) still matches.
Leaves are latched shared for reads. Rows and index keys stay the executor's SQLite records, so
comparisons and the planner's statistics stay as they are, in cells of a page format of this
storage's own (`storage/versioned/page.rs`: slotted, each page bounded above and linked right).

**Why.** A shared latch on the root at every descent is a write to one line by every reader: at 32
clients it moves between their caches at each read (the decision 22 effect, store.md). Lock-free
reads of inner pages are what LeanStore's optimistic lock coupling gives; its unsynchronized reads of
page memory are not expressible without `unsafe` (CLAUDE.md §12), so readers here read their own
copies, validated by a counter, and the copy is made once per change of the page, which is once per
split.

**Sources.** Lehman, Yao, "Efficient Locking for Concurrent Operations on B-Trees" (ACM TODS 6(4),
1981): the inserter holds the node it split until it has the parent. PostgreSQL's nbtree README
(src/backend/access/nbtree/README): a split page stays locked until its downlink, or for the root
the new root, is in place, and a page split again before its downlink exists leaves no place to
post (notes: docs/research/blink-root-split.md). Graefe, "A Survey of B-Tree Locking Techniques"
(ACM TODS 35(3), 2010). Leis, Scheibner, Kemper, Neumann, "The ART of Practical Synchronization"
(DaMoN 2016): optimistic lock coupling, version validation.

**Falsified by.** A reader that misses a committed row its snapshot sees, or sees one twice, during
splits (model test against a sorted map under a nemesis of concurrent splits); a writer that splits
the root's new page while the root's splitter is held after releasing the root's latch, and finds
no level above it (blink.rs, `a_root_split_is_whole_before_another_writer_reaches_its_new_page`);
point reads at 32 clients that stop scaling with clients while the leaves they read are distinct.

## 4. Versions: undo records, commit timestamps, a low watermark

**Decision.** A transaction takes a start timestamp when it begins and a commit timestamp when it
commits, both from one counter. A change records, in the transaction's undo log, what the row was
(its old cell, or that it didn't exist), and the frame holding the row maps it to the head of its
version chain while any version of it is newer than the oldest active snapshot (in memory only:
frames whose map is empty, nearly all of them, cost a reader one emptiness check). A reader sees,
for each row, the newest version committed at or before its start timestamp, walking the chain from
the page's version when the frame maps the row. Versions no active snapshot can see are dropped:
the low watermark is the oldest active start timestamp, and a frame's map entry goes when the row's
newest version is older than it. Writes take a row by first-writer-wins: changing a row whose newest
version is another transaction's uncommitted change aborts the later writer with a typed
`WriteWriteConflict`. Secondary-index entries are versioned the same way.

**Why.** It is what readers need to see a consistent snapshot while writers change the same pages,
and what bounds memory by what some snapshot can still see.

**Sources.** Neumann et al. (SIGMOD 2015, above): undo buffers, version chains newest to oldest,
start and commit timestamps. Alhomssi, Leis (PVLDB 2023, above): garbage collection by an oldest
active snapshot without long-running scans holding memory. The MySQL reference manual, "InnoDB
Multi-Versioning": undo logs, purge.

**Evidence.** `storage/versioned/version.rs`'s tests: a reader begun before a commit sees the rows
as before it, one begun after sees its changes, a writer its own changes before it commits; a
second writer of a row refused while the first is open and after it commits past the second's
snapshot; a rollback of changes, an insert and a removal leaves every row as it was and no
versions once collected; collection keeps what the oldest open snapshot needs and, once it closes,
drops every version and the cells of removed rows; 16 threads moving amounts between 50 accounts
(retrying when refused) while 4 threads sum every account in a snapshot: every sum the total.

**Falsified by.** An Elle-style history (AC-T1.3) with an anomaly snapshot isolation forbids; undo
memory that grows without bound under a steady workload with no long-running reader.

## 5. Serializable by precision locking

**Decision.** Snapshot isolation is the floor; a serializable transaction (the default) also logs
the predicates it read (key ranges per B-tree, and point keys), and at commit checks them against
the writes of the transactions that committed since it began. A match aborts it with a typed
`SerializationFailure`, which the caller retries.

**Why.** The design's README asks for serializable within a group. Snapshot isolation alone allows
write skew; the store's callers (kiln's records) are read-modify-write transactions where that
matters.

**Sources.** Neumann et al. (SIGMOD 2015, above): precision locking for MVCC, validation against
recently committed writes, cheap for short transactions. Jordan, Banerjee, Ma, "Precision Locks"
(SIGMOD 1981). For contrast, Cahill, Röhm, Fekete, "Serializable Isolation for Snapshot Databases"
(SIGMOD 2008) and Ports, Grittner, "Serializable Snapshot Isolation in PostgreSQL" (PVLDB 5(12),
2012): rw-antidependency tracking, more state per read.

**Falsified by.** A serializable history with a cycle (Elle, AC-T1.3); validation costing more than
a fifth of a short transaction's time on the suite's `txn_read_write`.

## 6. Durability: a row's change logged once, at commit

**Decision.** A commit appends one redo record: each row change of the transaction (the B-tree, the
key, the new row or its removal), in the order made. Records are appended in commit-timestamp order
by a leader that gathers the commits waiting for it (group commit) and flushes once for them all;
a commit is durable when a flush that began after its record was appended ends, as the store's
flusher works today (store.md, decisions 2 and 8). Uncommitted changes are never logged and never
written to the data file (decision 7), so recovery redoes and never undoes.

**Why.** Logging rows instead of pages cuts the log by what a page is to a row (4 KiB against the
~600 bytes of an `items` row, and far more for index entries), and logging at commit means aborted
work costs no I/O.

**Sources.** Johnson, Pandis, Stoica, Athanassoulis, Ailamaki, "Aether: A Scalable Approach to
Logging" (PVLDB 3(1), 2010): consolidated log buffer inserts, flush pipelining. Haubenschild, Sauer,
Neumann, Leis, "Rethinking Logging, Checkpoints, and Recovery for High-Performance Storage Engines"
(SIGMOD 2020): per-thread logs with group commit at many cores. Tu, Zheng, Kohler, Liskov, Madden,
"Speedy Transactions in Multicore In-Memory Databases" (Silo, SOSP 2013): redo-only value logging
at commit.

**Built** (`storage/versioned/redo.rs`): one record a commit (its changes, its timestamp, each
record's length and XXH3), appended under the clock's publish lock to the pending bytes of the
current one of two files; whoever next needs a commit durable takes the flush lock and writes and
flushes everything pending at once, so one flush serves every commit appended before it began, with
no timer. A flush that fails stops the log: nothing more is logged or made durable until the
database is reopened (store.md, decision 9). Recovery reads each file up to its first torn or
damaged record and cuts the file there, so an append past it can't leave a torn record's leftovers
to be read back behind it.

**Falsified by.** An acknowledged commit lost after a crash at any write (crash injection,
T-T1.3.1); log bytes per row on the suite not less than a quarter of page-a-frame's.

## 7. Checkpoints: dirty pages copied in a pause, and the rows to correct

**Decision.** A checkpoint holds a gate exclusive (each row change, commit and rollback holds it
shared for as long as it changes pages or publishes), takes the last commit's timestamp T and
switches the redo log's file at it, copies every page changed since the last checkpoint and lists
the rows whose copied version isn't their version as of T (changes of transactions still open),
each with that version. Taken under the gate, T splits no commit, and no commit after it exists
until the gate is released. The pause is the copies' memory time. The copies, the list and T go into
a checkpoint journal, flushed; then the copies and a header naming the checkpoint are written over
the data file, flushed; then the redo file switched from, which holds only commits up to T, is cut.
Journals take turns by the checkpoint's sequence number, so a checkpoint never writes over the
journal recovery may need, and making a database is checkpoint 0 (its empty trees journaled first,
like any other). A checkpoint that fails leaves the database making nothing more durable until it
is reopened: the pages it took as copied may not have reached the file. Splits wait for the pause
like any change, so the copies are one consistent tree.

**Why.** Writing each page as of T (undoing newer changes in the copy) can need more than a page:
a row whose older version is larger than its newer one no longer fits the page it was split for.
Listed instead, the rows are put back to their versions as of T at recovery by ordinary tree
changes, which split as they must. The pause, not the I/O, is what writers wait for.

**Sources.** InnoDB's doublewrite buffer (MySQL reference manual, "Doublewrite Buffer"): pages
written whole to one place, flushed, then over their homes, so a torn write is repaired from the
first copy. SQLite's write-ahead log backfill (sqlite.org/wal.html) for the same order. Mohan et
al., ARIES (ACM TODS 17(1), 1992), for what the list replaces: undo of uncommitted changes from
records logged before each page write.

**Falsified by.** A file after recovery that holds a change of a transaction that never committed,
or misses one that did; a pause longer than the copies' memory time.

## 8. Recovery: the file's pages, the last journal, its list, then the redo after T

**Decision.** Opening a database takes the newest complete journal at or after the checkpoint its
header names (a damaged header, any complete journal) and writes its pages and header over the
data file again: a power cut may keep a checkpoint's header and not every page it wrote. It then
applies the journal's list (each row back to its version as of T), replays every redo record
committed after T, in timestamp order across both files, and clears the versions' flags (no
snapshot outlives the process), removing the cells of removed rows. A data file with no complete
journal is made anew only when it is empty and no commit is logged; otherwise it is refused as
corrupt, never replaced by an empty database.

**Falsified by.** Recovery that ends in a state no prefix of acknowledged commits explains
(T-T1.3.1).

## 9. Beside the engine it replaces, behind the cursor the SQL layer already uses

**Decision.** The new storage implements the cursor interface the executor drives
(`storage::btree::CursorTrait`, which Turso's MVCC cursor implements too) and the connection's
begin, commit and rollback, chosen per database when it is created; a database the current engine
created stays on it. The store runs as many writers as it has callers with the new storage, each
transaction on its caller's thread, and keeps its tickets, flusher semantics and refusals.

**Why.** The SQL layer, the planner's work and their tests stay, and the suite can run both
engines side by side until the new one is the default.

**Evidence.** Stage 1 (`storage/versioned`): `blink.rs`'s tests: one writer's puts, replacements
and removals of 20,000 random keys against a sorted map; 32 writers with 4 readers at once, five
times, no key lost or read with another's value; with the bound on a walk to the right fixed when
the walk began (a mutant: splits add pages under it), both fail. `bench --blink`, fedora,
2026-10-22, load 10–15 from other work (a check): one tree in memory, 2,000,000 random 8-byte keys
with 64-byte values, puts and gets a second at 1, 2, 4, 8, 16 and 32 threads: 660,371 and 847,238;
1.24 and 1.73 million; 2.33 and 3.29 million; 3.63 and 4.89 million; 6.38 and 8.42 million; 8.55
and 11.64 million.

Stage 2 (`storage/versioned/{disk,redo,durable}.rs`): the files behind a seam (`Disk`), as a
directory's files and as a simulator whose writes are durable only once flushed and which crashes
after any number of operations, keeping of each unflushed write all, none or a prefix of its sectors.
`durable.rs`'s tests: commits survive a reopen and a power cut; a checkpoint taken while one
transaction is open and another rolls back keeps only what commits; a checkpoint whose header
reached the file and whose page didn't is finished from its journal; a damaged header is recovered
from its journal, and with the journals gone the file is refused, not made anew; a checkpoint that
fails refuses the commits after it; a torn redo tail is cut before appends reach it; eight writers
committing while one checkpoints every 20 commits, and 57 crash points through a workload of
commits and checkpoints, recover a prefix of the commits at least as long as the last acknowledged
durable. `redo.rs`'s: records decode up to the first torn or damaged one; one flush makes every
commit appended before it durable. Five mutants each fail the test written for them: recovery
without rewriting the newest journal, making a database whenever the header is unreadable,
checkpoints that don't stop the log when they fail, redo tails left uncut, and versions left on
recovered cells (which fails five).

## Stages, each measured

1. The buffer pool and the B-link tree, versioned rows, in memory: concurrent inserts and reads
   against a model (a sorted map), from 1 to 32 threads; a micro-bench of concurrent inserts
   against the current engine's single writer.
2. The redo log, group commit, checkpoints with their pause and list, and recovery: crash injection
   at every write; the pool still holds every page (eviction to the file follows). Built (above);
   measured with the rest of the storage through the suite once stage 3 puts it under SQL.
3. The executor on the new storage: the comprehensive suite, both engines.
4. Serializable validation, garbage collection, memory bounds; Elle histories (AC-T1.3).
5. The default, once the suite says so.
