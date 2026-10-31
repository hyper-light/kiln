# Point reads: how other engines reach a row, read in their code

Source notes for the point-read rung (CLAUDE.md §5): what SQLite, PostgreSQL, InnoDB and LeanStore
do on the path from a key to a row's values, read in their source at pinned commits, beside what
Amphora's engine does on the same path, and the alternatives the literature offers for the B-tree
descent itself. Each claim names the file and lines it rests on.

Sources read (shallow clones under `~/.cache/research-src`, 2026-10-23):

| System | Repository | Commit |
|---|---|---|
| SQLite | github.com/sqlite/sqlite (mirror) | `f879fb18cd81586e8caad7cc532575525ac1bd8e` |
| PostgreSQL | github.com/postgres/postgres | `126976a8ffcfa47f40d3df09243ebf4606001912` |
| MySQL/InnoDB | github.com/mysql/mysql-server | `561753af986c3c006c4b6c7caa8359748fb55b22` |
| LeanStore | github.com/leanstore/leanstore | `fcb64bf39cf69c435173f76e062cbb305a9a998f` |

## 1. Pinning a page

- **SQLite** counts a page's references with plain integers: a connection's pager is used by one
  thread at a time (under the database mutex), so pinning writes no shared line.
  `src/pcache.c:567-572` (`sqlite3PcacheRef`): `p->nRef++;` and `p->pCache->nRefSum++;`.
- **PostgreSQL** shares one buffer pool across processes, so a pin is an atomic update of the
  buffer's state word; but each backend keeps a private count so a buffer pinned again by the same
  backend touches no shared memory. `src/backend/storage/buffer/bufmgr.c:231-245`: "Each buffer
  also has a private refcount that keeps track of the number of times the buffer is pinned in the
  current process. This is so that the shared refcount needs to be modified only once if a buffer
  is pinned more than once by an individual backend."
- **LeanStore** pins nothing on reads: a page reached through a swizzled pointer is read under an
  optimistic latch whose version is checked afterwards, and the read restarts if it changed.
  `backend/leanstore/storage/buffer-manager/Swip.hpp:20`: `// 1xxxxxxxxxxxx evicted, 01xxxxxxxxxxx
  cooling, 00xxxxxxxxxxx hot`; `backend/leanstore/sync-primitives/Latch.hpp:84-90` (`recheck`:
  a version that moved restarts the operation, `jumpmu::jump();`).
- **Amphora** (Turso's design): every page read allocates `Arc<Page>` and an `Arc` around its
  buffer, and cursors clone and drop those `Arc`s as they move: atomic read-modify-writes even with
  one client (`crates/engine/storage/pager.rs`: `let page = Arc::new(Page::new(page_idx));`). The
  2026-10-22 profile of `table_count`: an atomic decrement was the largest leaf, 19% of samples.

## 2. Reading a page that isn't cached

- **SQLite**: the page cache recycles an unpinned buffer from its LRU (`src/pcache1.c:902`, "Step 4.
  Try to recycle a page."), and the pager reads straight into it: `src/pager.c:3095`, `rc =
  sqlite3OsRead(pPager->fd, pPg->pData, pPager->pageSize, iOffset);`. No allocation, no callback, no
  checksum on the read (`pager_set_pagehash` exists only in debug builds).
- **Amphora**: each read allocates a page, a buffer `Arc`, a boxed completion callback and a
  completion, though Unix `pread` completes before returning
  (`crates/engine/storage/sqlite3_ondisk.rs:565`, `let complete = Box::new(move |res: ...`;
  `crates/engine/io/unix.rs:334`, `// Read succeeded immediately`), and verifies an XXH3 checksum of
  every page it reads, 15% of `table_count`'s samples in the 2026-10-22 profile.

## 3. Finding the key inside a page

- **SQLite** picks a comparator for the search key's first field once per seek: integers and
  binary-collated strings get specialized loops. `src/vdbeaux.c:5089`
  (`sqlite3VdbeFindCompare`), `:5118` `return vdbeRecordCompareInt;`.
- **Amphora** has the same three (`crates/engine/types.rs:2745`, `RecordCompare { Int, String,
  Generic }`), comparing cells where they lie. Parity.
- **LeanStore**'s nodes keep a 4-byte normalized "head" of each key in its slot, truncate the
  prefix every key in the node shares, and narrow the binary search with a hint array, so most
  probes compare two integers without touching the key's bytes.
  `backend/leanstore/storage/btree/core/BTreeNode.hpp:204` (`head`), `:357` `if (keyHead <
  slot[mid].head) {`.
- SQLite's format (Amphora's too) keeps a cell pointer array: each probe follows a pointer into
  the page and decodes a varint (table B-trees) or a record header (indexes).

## 4. Skipping the descent: InnoDB's adaptive hash index

InnoDB watches index searches and, for B-tree pages searched often with the same key prefix,
builds hash entries that point at records, so a later point lookup jumps to the row without
descending. `storage/innobase/btr/btr0sea.cc:812` (`btr_search_guess_on_hash`);
`storage/innobase/include/btr0sea.h:166`: "Tries to guess the right search position based on the
hash search info of the index." Its counter of how promising a hash would be is an atomic per
index (`btr0sea.h:66`, `std::atomic<uint64_t> n_hash_potential;`), and the index is partitioned
to limit latch contention.

## 5. Reading a row's values

- **SQLite** parses a row's header once and caches the offsets in the cursor for every later column
  of the same row (`src/vdbe.c:3078`, `if( pC->cacheStatus!=p->cacheCtr ){`, and the `aOffset`
  array). Strings and blobs are copied into the register's own buffer, reused across rows and grown
  only when too small: `src/vdbe.c:3338`, `if( pDest->szMalloc < len+2 ){`.
- **Amphora** reuses a register's text buffer the same way (`crates/engine/vdbe/mod.rs:4185`,
  `existing_text.replace_with_bytes(text_data)?;`) and fuses consecutive column reads into one pass
  over the header (`crates/engine/vdbe/builder.rs:1256`, `ColumnRange`). Parity, except that a
  column read out of order walks the header from its start again.
- **So the text-read gap (`text_read` 0.43x SQLite at one client) isn't in the column decoder.** It
  has to be on the page path (§1, §2) or in the store's read checkout and the bench's own work; G1's
  profiles decide which.

## 6. Alternatives to the B-tree descent (literature)

- **Learned indexes.** Kraska, Beutel, Chi, Dean, Polyzotis, "The Case for Learned Index
  Structures" (SIGMOD 2018, arXiv:1712.01208): "by using neural nets we are able to outperform
  cache-optimized B-Trees by up to 70% in speed while saving an order-of-magnitude in memory over
  several real-world data sets." The independent benchmark, Marcus et al., "Benchmarking Learned
  Indexes" (PVLDB 14(1), arXiv:2006.12804): "We find that learned index structures offer
  competitive performance." Updatable variants: ALEX (Ding et al., SIGMOD 2020,
  arXiv:1905.08898); PGM-index (Ferragina, Vinciguerra, PVLDB 13(8), arXiv:1910.06169), whose
  related work notes RMI reported "up to 1.5–3× faster and two orders of magnitude smaller in
  space" than a B+-tree. **Here:** rowid tables whose rowids are mostly dense (the suite's items,
  kiln's append-mostly records) are close to the easiest case for a learned leaf locator.
- **Adaptive radix trees.** Leis, Kemper, Neumann, "The Adaptive Radix Tree: ARTful Indexing for
  Main-Memory Databases" (ICDE 2013): a space-efficient radix tree for in-memory secondary keys.
- **Hash on hot keys.** InnoDB's adaptive hash index (§4): a point lookup of a hot key without a
  descent, built only where searches show it pays.

## 7. Storage below the pool

Haas, Leis, "What Modern NVMe Storage Can Do, And How To Exploit It" (PVLDB 16(9), 2023): "In a
heavy out-of-memory setting, where the dataset is 10 times larger than main memory, our system can
achieve more than 1 million TPC-C transactions per second," and "a single SSD can achieve more than
one million random I/O Operations Per Second (IOPS)." Haubenschild, Sauer, Neumann, Leis,
"Rethinking Logging, Checkpoints, and Recovery for High-Performance Storage Engines" (SIGMOD 2020):
"Using per-thread logging with minimal synchronization, our implementation achieves near-linear
scalability on multi-core CPUs."

## 8. What to prototype, in order (each measured on the corpus's point-read classes)

Profiled since (`read-path.md`, its measured section and ranked fixes, 2026-10-23): before any of
these, the log's frame index and the locks around it, 23.1–28.3% of a point read at one client and
63.1–66.0% at 32 with the log left whole after the load; this order follows that fix.

1. Pages a single connection holds without atomic reference counting (SQLite's plain counts;
   PostgreSQL's private counts where a page is shared), and a miss path that reads into a recycled
   buffer with no per-read allocation (SQLite's pcache1).
2. Checksums verified once per entry into the process's memory, as PostgreSQL verifies on reading a
   block into shared buffers.
3. Key heads and hints for in-page search (LeanStore), kept in an in-memory directory built when a
   page enters the cache, so the file format needn't change.
4. An adaptive hash index on hot leaves for rowid and secondary point lookups (InnoDB), against a
   learned leaf locator for rowid tables (RMI/PGM): both skip the descent; measure which wins where.
5. Optimistic, version-validated reads of shared pages in safe Rust (atomics-backed page words or
   per-reader copies as engine.md decision 3 does), for 32-client scaling.
