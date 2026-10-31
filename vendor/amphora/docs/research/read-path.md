# The read path: what point reads and scans cost, and what the literature does about it

Source notes for the read path of Amphora's engine as it ships (Turso's SQLite design: a page cache
per connection, pages read with `pread`, a bytecode interpreter a row at a time) and for the
choices that set what a point read and a scan cost: where pages live in memory and how a read
reaches them, how many instructions and cache misses a row costs, and how many cores a statement
uses. CLAUDE.md §5: design decisions cite these notes; each note says what its source measured and
what it implies here.

## What Amphora's read path costs today

The suite at ten million rows on adacachy (AMD Ryzen 9 9950X3D2: 16 cores, 32 threads, 48 KiB of
L1d and 1 MiB of L2 a core, 192 MiB of L3 in two 96 MiB halves), best of the v6 matrix's passes
(`bench/records/2026-10-23-adacachy-matrix-v6.md`, noisy machine, load 3.5–123.6 from other work):

- One client's point reads trail SQLite's: `point_pk` 343,467 a second against 419,712 (0.82x),
  `point_pk_one_column` 0.62x, `text_read` 0.43x, `json_field` 0.37x, `blob_read` 0.31x. SQLite
  reads with `read()` into a page of heap memory as Amphora does (its documentation, below), with a
  page cache of 2,000 KiB a connection (`PRAGMA cache_size`'s default -2000, "limited to 2048000
  bytes") against Amphora's 2,000 pages (`DEFAULT_PAGE_CACHE_SIZE_IN_PAGES`): the gap is what
  Amphora does around each page and row, not the system call.
- Whole-table statements run on one of 32 threads: `table_count` 36.8 a second at one client against
  CedarDB's 2,197; `filter_no_index` (`COUNT(*) ... WHERE qty = ? AND price < ?`, ten million rows)
  0.6 against 2,608; `table_group_by` 0.2 against 276.
- The settle's `wal_checkpoint(TRUNCATE)` was taken to empty Amphora's log before the reads, and the
  bench's own sampler (user time outside libc, no call graphs) was read that way on 2026-10-22. It
  doesn't empty it: the measurements below are the first to see where the cycles go.

## Measured: where each statement's cycles, misses and kernel time go (2026-10-23)

From `bench/records/2026-10-23-adacachy-read-path.md` (frame-pointer call graphs with no lost
samples; per-statement counters; each statement's measured window alone; Amphora at 4acff7d against
SQLite on the same ten million rows). User mode only (perf_event_paranoid 2): the kernel's share is
the process's own system time. Rates are from a shared machine at load 33–85.

**Per statement, Amphora against SQLite**, every profiled statement (instructions are the stable
measure here: three repeats within 4.9%, cycles within 17.6%; the record has cycles, dTLB and cache
misses, branch misses and the top-down split beside these):

| statement, clients | instructions/stmt | IPC | L1d misses/stmt | kernel share | bound by (top-down) | Amphora's largest parts |
|---|---|---|---|---|---|---|
| blob_read, 1 | 20,914 vs 9,713 (2.2x) | 1.60 vs 1.73 | 243 vs 141 | 28% vs 56% | frontend (both) | the log 28.3%, page cache and pager 19.2% |
| case_aggregate, 1 | 2.6 M vs 1.0 M (2.5x) | 1.67 vs 3.36 | 22.4 K vs 2,468 | 32% vs 45% | backend stalls vs frontend | the log 27.1%, statement and binding 22.2% |
| count_distinct, 1 | 1.20 G vs 591.5 M (2.0x) | 1.31 vs 1.67 | 24.2 M vs 10.6 M | 34% vs 52% | backend stalls vs frontend | the log 32.1%, B-tree search 15.6% |
| count_range, 1 | 10.2 M vs 4.2 M (2.4x) | 2.69 vs 2.97 | 37.8 K vs 13.9 K | 15% vs 24% | instructions (both) | B-tree search 43.7%, statement and binding 12.5% |
| filter_no_index, 1 | 10.74 G vs 5.71 G (1.9x) | 1.69 vs 2.65 | 77.2 M vs 27.2 M | 33% vs 61% | backend stalls vs instructions | the log 37.0%, page cache and pager 12.6% |
| group_by_range, 1 | 34.8 M vs 25.8 M (1.3x) | 1.68 vs 2.38 | 383.3 K vs 125.2 K | 33% vs 23% | backend stalls vs frontend | interpreter 24.9%, the log 23.4% |
| in_list_10, 1 | 182,496 vs 68,647 (2.7x) | 1.35 vs 1.69 | 2,373 vs 1,334 | 36% vs 60% | backend stalls vs frontend | the log 29.4%, B-tree search 23.5% |
| json_field, 1 | 28,663 vs 13,254 (2.2x) | 1.42 vs 1.64 | 448 vs 207 | 29% vs 53% | frontend (both) | the log 23.1%, page cache and pager 15.6% |
| order_by_sort, 1 | 1.17 G vs 379.0 M (3.1x) | 1.45 vs 1.98 | 16.2 M vs 4.7 M | 36% vs 65% | frontend (both) | the log 36.3%, page cache and pager 16.9% |
| point_pk, 1 | 23,319 vs 14,097 (1.7x) | 1.48 vs 1.74 | 332 vs 206 | 27% vs 48% | frontend (both) | the log 25.1%, page cache and pager 18.6% |
| point_pk_one_column, 1 | 20,995 vs 9,412 (2.2x) | 1.40 vs 1.61 | 332 vs 148 | 29% vs 55% | frontend (both) | the log 27.5%, page cache and pager 18.8% |
| point_secondary, 1 | 37,530 vs 49,889 (0.8x) | 1.42 vs 2.24 | 576 vs 641 | 28% vs 34% | backend stalls vs frontend | the log 24.9%, page cache and pager 20.9% |
| point_text, 1 | 41,814 vs 47,630 (0.9x) | 1.53 vs 2.15 | 597 vs 751 | 26% vs 34% | backend stalls vs frontend | the log 25.4%, B-tree search 23.0% |
| range_pk_100, 1 | 242,435 vs 158,544 (1.5x) | 1.79 vs 2.73 | 2,383 vs 558 | 30% vs 41% | frontend vs instructions | the log 31.1%, statement and binding 19.5% |
| table_count, 1 | 198.6 M vs 21.5 M (9.3x) | 1.31 vs 1.97 | 2.0 M vs 363.7 K | 39% vs 81% | backend stalls vs frontend | the log 51.4%, page cache and pager 16.4% |
| table_group_by, 1 | 26.35 G (SQLite answered none in its window) | 2.50 | 174.2 M | 27% | backend stalls | interpreter 25.9%, the log 24.2% |
| text_read, 1 | 21,138 vs 9,688 (2.2x) | 1.49 vs 1.97 | 276 vs 113 | 28% vs 55% | frontend (both) | the log 27.3%, page cache and pager 21.4% |
| filter_no_index, 32 | no statement finished inside the counted window | - | - | - | - | the log 90.7%, page cache and pager 3.4% |
| point_pk, 32 | 28,591 vs 15,631 (1.8x) | 0.16 vs 0.31 | 768 vs 714 | 12% vs 90% | frontend (both) | the log 66.0%, locks 14.6% |
| point_secondary, 32 | 41,508 vs 53,667 (0.8x) | 0.18 vs 0.32 | 1,131 vs 2,161 | 11% vs 85% | backend stalls vs frontend | the log 63.1%, locks 16.1% |
| range_pk_100, 32 | 289,779 vs 166,802 (1.7x) | 0.18 vs 0.68 | 3,878 vs 2,953 | 8% vs 88% | frontend (both) | the log 86.7%, page cache and pager 4.6% |
| table_count, 32 | 310.6 M vs 41.9 M (7.4x) | 0.10 vs 0.15 | 5.6 M vs 3.6 M | 8% vs 92% | backend stalls vs frontend | the log 92.3%, page cache and pager 3.5% |

`point_secondary` and `point_text` take fewer instructions in Amphora than in SQLite because the
bench's SQLite compiles them again on every call (below); every other statement takes 1.3–9.3x
SQLite's instructions.

**Where Amphora's user cycles go** (share of sampled cycles, by part, from the leaf frames):

| part | point reads at 1 client | scans at 1 client | point reads at 32 | scans at 32 |
|---|---|---|---|---|
| the write-ahead log (frame index lookups, the log's header, frame reads) | 23.1–28.3% | 11.2–51.4% | 63.1–66.0% | 86.7–92.3% |
| page cache and pager | 15.6–21.4% | 7.1–16.9% | 7.3–8.5% | 3.4–4.6% |
| B-tree search | 11.9–23.0% | 3.6–43.7% | 2.9–5.2% | 0.2–0.8% |
| the interpreter | 5.8–11.2% | <0.5–25.9% | 1.8–2.4% | <0.5–0.9% |
| checksums, atomics, I/O completion together | 7.4–9.4% | 4.8–17.8% | 0.8–1.0% | 1.0–1.2% |
| locks | 1.2–1.7% | 0.4–2.7% | 14.6–16.1% | 1.3–1.8% |

Point reads: `blob_read`, `json_field`, `point_pk`, `point_pk_one_column`, `point_secondary`,
`point_text`, `text_read`; scans: `case_aggregate`, `count_distinct`, `count_range`,
`filter_no_index`, `group_by_range`, `order_by_sort`, `range_pk_100`, `table_count`,
`table_group_by` (`in_list_10`, ten point reads in one statement, is in the record and in neither
column); at 32 clients `point_pk` and `point_secondary`, and `filter_no_index`, `range_pk_100` and
`table_count`. A part under 0.5% in both engines isn't in the record's table (`table_count`'s
interpreter).

At 32 clients `find_frame` alone is 79.2% of `table_count`'s user cycles; `point_pk` spends 26.9%
in `find_frame`, 23.4% in `wal_header` and 13.2% in `RawRwLock::lock_shared_slow`. Both functions
take the read side of one `RwLock` over the log's shared state on every page read, then a `Mutex`
each: `find_frame` the frame index's (a map from page to its frames, then a walk of that page's
frames for the latest in the snapshot), `wal_header` the header's, to copy its eight fields. At one
client `find_frame` is 14.2% of `point_pk` and 36.0% of `table_count`. The cause: Amphora's
`PRAGMA wal_checkpoint(TRUNCATE)` after the load copied all 1,834,934 frames into the file but left
the log whole (its header's checkpoint sequence 0, every frame's salts valid, 7,559,928,112 bytes),
so every page read looks its page up in a 1.8-million-frame index and reads it from the log. The v6
matrix's database is in the same state: these are the costs its read cells measured.

SQLite's settle leaves no log. Its profile is not SQLite at its best: the bench's bundled build (the
record's header) keeps every connection's pages in one page cache behind one mutex, takes a global
mutex around each allocation, and with STAT4 compiles `point_secondary` and `point_text` again on
every call. `pthread_mutex_lock` and `pthread_mutex_unlock` are 26.5–51.5% of SQLite's point reads
at one client and 32.0–48.4% of its statements at 32 (where its kernel share is 85–92%); parsing is
14.3–14.7% of `point_text` and `point_secondary`; the bench around it 5.2–25.5% (rusqlite's statement
cache, finding each statement by its text, up to 8.1%; Amphora's harness is 4.1–8.1%). Amphora
trails even this SQLite on one client's point reads; its leads over SQLite at 32 clients are over a
SQLite serialized on those mutexes, to be measured again against a fair build (fix 7).

## The fixes, ranked by predicted gain (Amdahl's law on the measured shares)

1. **An empty log after a full checkpoint, and log lookups without shared locks.** Targets every read
   statement. (a) `wal_checkpoint(TRUNCATE)` with no reader must restart the log and cut the file
   (a failing test first: frames checkpointed, the log's file empty after); (b) a page read whose
   snapshot can't have the page in the log (no frames, or a page-presence filter that says absent)
   skips the frame index, and the index and the log's header are read without the `RwLock` and
   `Mutex` every reader takes now. Predicted from the log's share: 1.30–1.39x on one client's point
   reads (23.1–28.3%), up to 2.06x on scans (`table_count` 51.4%); at 32 clients 2.7–2.9x on point
   reads from the log alone (63.1–66.0%), 4.8–5.2x with the locks' share too (`point_secondary`
   63.1% + 16.1%, `point_pk` 66.0% + 14.6%), and 7.5–13x on scans (86.7–92.3%). Falsified if, with
   the log empty after the settle, `point_pk`'s instructions per statement at one client fall by
   less than 15%, or `table_count` at 32 clients rises less than 3x.
2. **The page path SQLite takes: plain pins, recycled buffers, no completion per read, checksums
   once.** Targets point reads and scans at one client. The page cache, pager, checksums, atomics
   and completions are 23.0–30.3% of a point read's user cycles, 30–41% of what is left once (1) is
   in; SQLite's page cache and pager are 1.8–4.6% of its own. Predicted: cutting those parts to a
   third gives 1.25–1.37x on point reads after (1). Falsified if those parts together stay above 15%
   of a point read after the change.
3. **Counting without reading rows.** Targets `table_count`: 9.3x SQLite's instructions per
   statement (198,595,680 against 21,456,129). SQLite adds up each page's cell count without reading
   a row (`btree.c`'s `sqlite3BtreeCount`: `nEntry += pPage->nCell;`), on the index with the
   smallest rows (`select.c`: "indexes are almost always spread across less pages than their
   corresponding tables"; both at f879fb18). Amphora's profile is its log and page path
   (`find_frame` 36.0% at one client; after (1), the remaining cost shows what its count reads).
   Predicted: up to 9.3x, at SQLite's instructions and IPC. Falsified if `table_count`'s
   instructions per statement stay above 3x SQLite's.
4. **Scans that stall less, then scans on every core.** Targets `filter_no_index`,
   `count_distinct`, `group_by_range` and `case_aggregate` (backend-stall bound) and `order_by_sort`
   (frontend bound): IPC 1.31–1.69 against SQLite's 1.67–3.36, with 2.3–9.1x SQLite's L1d misses
   per statement. Batched evaluation of a leaf's cells (X100), read-ahead of leaf runs, then
   morsel-driven parallel scans (G5). Predicted: at SQLite's IPC on Amphora's own instructions,
   1.27–2.01x on one thread (`count_distinct` 1.31 to 1.67, `case_aggregate` 1.67 to 3.36); then,
   from Leis et al.'s "average speedup of over 30 with 32 cores", 12–15x more across this host's 16
   cores. Falsified if `filter_no_index`'s IPC stays below 2.2 after batching, or 16 workers give it
   less than 8x.
5. **No SipHash and no error values per row.** Targets `table_group_by` (std's SipHash 6.4%, errors
   made and dropped 1.3%) and `group_by_range` (4.2%, 1.0%). A hash keyed by an engine-internal id
   needs no DoS-resistant hasher (FxHash, or an index). Predicted: 5.5–8.3% on grouped statements.
   Falsified if their SipHash share stays above 1%.
6. **In-page search by key heads, and skipping the descent.** Targets point reads' B-tree search
   (11.9–23.0%) and `count_range` (43.7%): LeanStore's key heads and hints, then an adaptive hash
   index against a learned leaf locator (G3). Predicted, if it halves the B-tree's share (an
   assumption to test, not a measured gain): 1.06–1.13x on point reads and 1.28x on `count_range`
   (Amdahl's bound, the share gone: 1.14–1.30x and 1.78x). Falsified if the B-tree's share doesn't
   fall.
7. **A fair SQLite in the bench.** Not an Amphora fix, and needed before any comparison with SQLite
   is claimed again: the bench's SQLite spends 26.5–51.5% of its point reads in mutexes its build
   adds, 14.3–14.7% compiling `point_secondary` and `point_text` again, and up to 8.1% in rusqlite's
   statement cache. G2's harness builds SQLite with its recommended options (no
   `SQLITE_ENABLE_MEMORY_MANAGEMENT`, `SQLITE_DEFAULT_MEMSTATUS=0`, STAT4 off as SQLite ships it),
   keeps each session's prepared statements by handle as Amphora's session does, and measures every
   SQLite comparison again, those at 32 clients first. Predicted: SQLite's point reads at one client
   1.36–2.06x faster with its mutexes' 26.5–51.5% gone, so the gap Amphora has to close widens.
   Falsified if its mutexes stay above 5% of its point reads in the recommended build.

Where each lands in kiln's goal stack: 1(a) and 7 in G2's harness (step 4), before the corpus's
performance runs; 1(b) G3 step 2; 2 G3 step 3; 6 G3 steps 4 and 5; 3, 4 and 5 G5 steps 2 and 3.
Each step cites the shares above and states the falsifier given here.

## Sources

### Buffer managers spend their time on translation, pinning and latches

Leis, Haubenschild, Kemper, Neumann, "LeanStore: In-Memory Data Management beyond Main Memory"
(ICDE 2018). In-memory TPC-C on one thread: BerkeleyDB 10 K transactions a second, WiredTiger
16 K, LeanStore 67 K, an in-memory B-tree 69 K: a traditional buffer manager's hash-table
translation, pinning and latching cost several times the data structure's own work. Its rule:
"programs that frequently write to memory locations accessed by multiple threads do not scale."
Its three techniques: pointer swizzling (a reference to a page in memory is a pointer, so a hot
page costs one well-predicted branch instead of a hash lookup); epoch-based reclamation instead of
"incrementing per-page pinning counters" ("an epoch-based technique that avoids writing to each
accessed page"); optimistic latches (a version counter readers validate instead of acquiring
anything: "lookups on swizzled pages do not acquire any latches at all"). With them it scales 8.8x
on 10 cores. **Here:** every page Amphora reads clones and drops `Arc`s and sets atomic flags: the
pinning LeanStore removes, paid per page even by one client.

Leis, Alhomssi, Ziegler, Loeck, Dietrich, "Virtual-Memory Assisted Buffer Management" (SIGMOD
2023, vmcache). Page `p` lives at a fixed virtual address (`base + p * page_size`) in an anonymous
mapping, read in with `pread` and evicted with `MADV_DONTNEED`; the page table translates, so a
hit takes no hash lookup. A 64-bit state per page (locked, shared count, marked, evicted, and a
version) gives optimistic reads: read the state, read the page, check the state is unchanged. The
state's address and the page's are both known up front, so the CPU fetches them in parallel,
"hiding the additional overhead of the state retrieval", where a hash table's two dependent
lookups pay "two cache miss latencies". Its cost is the OS: vanilla Linux manages 1.51 M page
faults and evictions a second with 128 threads, 79% of its time in TLB shootdowns, which the
authors' kernel module (exmap) removes. **Here:** a fixed address per page needs `unsafe` (raw
memory, CLAUDE.md §12) and a page read while a writer may change it is a data race in Rust unless
read through atomics; engine.md decision 3 takes the safe form (a reader's copy of an inner page,
validated by the frame's version counter).

Crotty, Leis, Pavlo, "Are You Sure You Want to Use MMAP in Your Database Management System?"
(CIDR 2022): with `mmap` the DBMS loses control of eviction and of when I/O happens (an access
may fault and wait on storage), errors arrive as signals, and TLB shootdowns limit throughput once
data exceeds memory. SQLite's documentation, "Memory-Mapped I/O" (sqlite.org/mmap.html): without
it, "SQLite first allocates a page-size chunk of heap memory then invokes the xRead() method ...
(at a minimum) a copy of the entire page"; with it, "content need not be copied between kernel
space and user space", but "an I/O error on a memory-mapped file cannot be caught"; it is off by
default (`mmap_size` 0), as the suite runs it.

### Check a page once, when it comes into memory

PostgreSQL's documentation, "Data Checksums" (postgresql.org/docs/current/checksums.html): each data page's
checksum "is updated when the page is written and verified each time the page is read"; its
buffer manager does that where it reads a block from storage into shared buffers
(`src/backend/storage/buffer/bufmgr.c`), not at each access to a buffered page. **Here:** a page
that falls out of a connection's 2,000-page cache and is read again from the OS cache is checked
again: a whole scan of a table larger than the cache checksums every page on every scan.

### Hide the misses a group of lookups takes

Psaropoulos, Legler, May, Ailamaki, "Interleaving with Coroutines: A Practical Approach for Robust
Index Joins" (PVLDB 11(2), 2017): given a group of independent lookups, switch to another lookup at
each likely cache miss after issuing a prefetch, so the misses overlap; binary search and CSB+-tree
lookups in SAP HANA stay fast once the index outgrows the last-level cache. Earlier forms: Chen,
Ailamaki, Gibbons, Mowry, "Improving Hash Join Performance through Prefetching" (ICDE 2004: group
prefetching, software-pipelined prefetching); Kocberber, Falsafi, Grot, "Asynchronous Memory
Access Chaining" (PVLDB 9(4), 2015). **Here:** an `IN` list, an index nested-loop join and the
table lookups behind a secondary index (`order_by_sort`'s 100,000 rows of one `grp`) are groups of
independent descents, each of whose pages may be a miss, today taken one after another.

### Use every core: morsels

Leis, Boncz, Kemper, Neumann, "Morsel-Driven Parallelism: A NUMA-Aware Query Evaluation Framework
for the Many-Core Age" (SIGMOD 2014): a query's pipelines run on one worker per hardware thread,
each taking a morsel of its input (about 100,000 tuples) at a time from a dispatcher, so load
balances itself and a query's parallelism can change while it runs; on TPC-H and SSB it measured
"an average speedup of over 30 with 32 cores" (its abstract). **Here:** a whole-table
statement runs on one thread of 32; a scan split into key ranges, its partial aggregates merged, is
the first-order change for `table_count`, `filter_no_index`, `table_group_by` and the other
whole-table statements on the row store.

### Interpret less per row

Boncz, Zukowski, Nes, "MonetDB/X100: Hyper-Pipelining Query Execution" (CIDR 2005): a
tuple-at-a-time interpreter spends most of its instructions on interpretation; operators working on
vectors of values amortize it and let the compiler use SIMD. Neumann, "Efficiently Compiling
Efficient Query Plans for Modern Hardware" (PVLDB 4(9), 2011): data-centric code generation keeps a
tuple in registers through a pipeline. Kersten, Leis, Kemper, Neumann, Pavlo, Boncz, "Everything You
Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask" (PVLDB 11(13),
2018): both are efficient; vectorized execution hides cache misses better (independent accesses in
a loop), compiled execution needs fewer instructions for computation. **Here:** the VDBE dispatches
an opcode per column per row and decodes each record's header for each column it reads.

### Columns, block summaries and skipping

Lang, Mühlbauer, Funke, Boncz, Neumann, Kemper, "Data Blocks: Hybrid OLTP and OLAP on Compressed
Storage using both Vectorization and Compilation" (SIGMOD 2016): cold data in compressed columnar
blocks of up to 2^16 records (PAX-like), byte-aligned so a point access stays cheap; each block keeps
min/max summaries (SMAs, after Moerkotte, "Small Materialized Aggregates", VLDB 1998) to skip whole
blocks and Positional SMAs to narrow the scanned range inside one; scans filter early with SIMD.
Ailamaki, DeWitt, Hill, Skounakis, "Weaving Relations for Cache Performance" (VLDB 2001, PAX): a
page's values grouped by column, so a scan of one column reads only its cache lines. **Here:** what
lets CedarDB (Umbra's successor, from the same group) answer `filter_no_index` over ten million rows
in 0.4 ms at one client: two columns, compressed, scanned with SIMD on every core, small enough to
sit in this machine's L3. A row B-tree reads every row's whole record for it.

### One shared pool, scalable

Neumann, Freitag, "Umbra: A Disk-Based System with In-Memory Performance" (CIDR 2020): a buffer
manager with variable-size pages, pointer swizzling and versioned latches keeps in-memory speed for
data that fits and degrades gracefully when it doesn't. engine.md decisions 1–3 take this line for
the engine being built beside the current one.
