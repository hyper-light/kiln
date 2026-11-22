# Status

Updated 2026-10-24. A piece is done when its tests pass in the gate (`scripts/gates.sh`: Linux, with the
macOS and Windows lints) and the evidence its requirement names is recorded.

## Implemented

- **The bar** (kiln's CP-M1): a Cargo workspace whose lints deny panics, this repository's rules
  (`CLAUDE.md`), the requirement registry (`docs/requirements.toml`), the design's starting point
  (`docs/design/README.md`), the gate (`scripts/gates.sh`, run by kiln CI from `.kiln/ci.yaml`), and the
  vendoring tool (`scripts/vendor.py`, kiln's).
- **The audit log** (CP-T1.7, AC-T1.7.4; AL.1, `crates/amphora/src/audit.rs`, docs/design/audit.md):
  RFC 6962's Merkle tree over any Records store, its head checked against its subtrees on open, a
  reader's earlier checkpoint checked by a consistency proof, and each event by its leaf hash and
  inclusion proof on read; every byte of a 1,000-event log flipped is detected. Open: where kiln keeps
  checkpoints outside the store (signed heads, witnesses), and the audit_cost runs for bench/records.
- **Records in amphora-api, typed and simulated** (CP-T1.5, kiln goal a1f386fe; lane records-api;
  docs/design/records.md): the trait's change feed is documented with etcd's watch guarantees and read
  from the commits' own version rows, and gains `changed` (a wait's answer without waiting);
  `amphora_api::sim::SimRecords` keeps Amphora's layout in memory, bounded by the caller's
  `SimLimits`; `amphora_api::index` (index families) and `amphora_api::counter` (counters) type
  kiln's deploy layout; `amphora_api::conformance` holds kiln-seams' RECORDS-1 to 14, ported, and
  Amphora's own cases. Amphora (in memory, on a file, through the local owner) and the simulator
  pass it, and a seeded differential test holds the simulator's answers to Amphora's
  (`crates/amphora/tests/records_conformance.rs`).
- **The `amphora` binary** says what it is (`amphora --version`, `amphora --help`), and nothing more yet.
- **The design's index and the README** (AC-T1.2.3, T-T1.13.2; lane docs): `docs/design.md` gives every
  decision of `docs/design/` one line and a link, held to the records by
  `crates/amphora/tests/design_index.rs` (a decision without a line, or a line that points nowhere,
  fails it); it compares the client protocol with PostgreSQL's, MySQL's, SQLite's API and Hrana from
  `docs/research/client-protocol.md`, and `docs/design/client-protocol.md` proposes the decision
  (embedded first; the network listener open). `README.md` takes a fresh clone to a build, a working
  database (`cargo run -p amphora --example quickstart`) and the gates. Built and merged; a fresh
  agent following it on a clean machine (T-T1.13.2's run) is still to do.
- **The store's commit pipeline** (CP-T1.1, `crates/amphora/src/store.rs`, docs/design/store.md): one
  writer with a group commit and no timer; durable beside it by a flusher (a write is durable once a
  flush begun after its commit ends, `Store::durable` by its batch's ticket); readers of their own on
  their caller's thread, each waiting only for its key's writes; no checkpoint in a commit's way (the
  log copied until little is left before the writer starts it again). A full queue refuses rows,
  typed and counted, and `put_wait` waits for room. A commit that fails is retried batch by batch:
  a batch that can't be committed is refused to its sender (`Refused::Failed`), the rest kept. The
  writer keeps no statement journal. Its meter keeps the longest commit, flush, checkpoint, wait to
  restart and restart. A flush that fails is the last: the store refuses what waits and what comes
  (`Refused::Disk`) until it is started again. The flusher flushes the log's data, and the log keeps
  its length across restarts within twice its bound (decisions 13 and 31); when the writer ran out
  of work during a flush, the next waits for it to take in what that flush's senders send (decision
  14): at 32 writers on fedora, 7,605 writes a second at p50 4.06 ms against 5,608 at 5.12 ms
  before, and PostgreSQL 17's 6,120 at 5.06 ms in the same windows (2026-10-20, load 4–12, not yet a
  quiet machine). Tested in `crates/amphora/tests/store.rs`.
- **Groups** (`crates/amphora/src/groups.rs`, docs/design/store.md decision 10): a store in groups
  on one node, keys spread by hash, each group a writer, a log and a flusher of its own; a batch is
  one group's. 16 groups wrote 320,451 rows a second at 1,000,000 writers on fedora, against one
  writer's 22,000. `Store::put_then` tells a sender once its batch is durable (decision 11).
- **Crash injection** (T-T1.1.1, `crates/amphora/tests/crash.rs`, docs/design/store.md): the store on
  a simulated disk that loses its power at any change, unflushed writes surviving whole, torn or not
  at all, its flushes taking time and covering what was written before they began, batches left in
  flight so commits land during flushes. 10,000 seeds: no acknowledged batch lost, no refused batch
  kept, no batch torn, the integrity check passing (2026-10-20). Every gate runs 200. A test holds a
  flush and commits during it: no checkpoint copies that commit before a later flush (decision 15).
- **The versioned storage's log in lanes** (CP-T1.3's log, `crates/engine/storage/versioned`,
  docs/design/engine.md decision 11): one group-committed log or a log a writer, merged by commit
  timestamp; a commit acknowledged once every lane is durable through it; recovery replays to the
  first missing commit, reading and replaying side by side (SiloR), and cuts each lane there; on Linux
  a flush is one io_uring submission (the engine's `io_uring` feature). Checkpoints copy pages by
  copy-on-update after a pause that only lists rows with versions (decision 7). T-T1.3.1, a crash at
  every disk operation of each seed's workload: 200 seeds in every gate; the 10,000 and
  `amphora-bench --vlog` (AC-T1.4.2, AC-T1.4.3) not yet run.
- **Stores on the engine** (`crates/amphora/src/db.rs`): a database opened for the pipeline, its
  readers, its flusher, its checkpoints and its log's restart.
- **Amphora's engine** (`crates/engine*`, its `ORIGIN.md`): Turso's four crates (core, sqlite/parser,
  macros, extensions/core) as of 3e09a82, Amphora's own code since 2026-10-20 as `amphora-engine`,
  `-parser`, `-macros` and `-ext`, members of the workspace, nothing compiled from C. Its suites run
  in every gate; what is left of the inherited code that breaks CLAUDE.md §1 is counted, lint by lint,
  against `scripts/engine-ratchet.toml`, and may only fall. The bench's vanilla Turso is upstream at
  3e09a82, none of Amphora's changes.
- **The packed log** (`crates/engine/storage/packed.rs`, docs/design/store.md decisions 16 and 17),
  opt-in (`Db::pack_log`): a packed log holds records, not a page a frame: a commit's pages as whole images, or
  as the bytes that changed since their last version in the log (at most 8 deltas in a row),
  compressed together with LZ4 into the log's frame slots. Reads rebuild a page from its chain, on a
  fresh connection, in the checkpoint and in recovery alike; a record torn by a crash ends the log,
  one whose checksums hold but that doesn't decode refuses it. Crash injection, 10,000 power cuts on
  the committed tree: nothing lost (2026-10-20); the engine's tests include a model test of
  savepoints rolled back amid spills. On the bench so far it cost writes at 32 writers (6,216 and
  5,887 a second against 7,903, `bench/records/2026-10-20-fedora-packed-log.jsonl`), so it isn't the
  default until the comprehensive bench shows where it helps. The comprehensive bench shows where it
  hurts: statements that write hundreds of pages, `insert_batch_100` at 32 clients 392 → 44 a
  second, `delete_range_insert` 2,800 → 1,011 (fedora, 2026-10-22, checks); single-row writes as
  before.
- **Page reads without the log's shared locks** (G3 step 2, docs/design/store.md decision 40,
  `crates/engine/storage/log_index.rs`), a prototype behind `AMPHORA_LOG_READ_INDEX`, off by
  default: the log's header read from a seqlocked copy; a page the log doesn't hold answered from an
  exact bitmap; the frame index read from a copy published per generation (`published`) or from
  sharded latest-frame tables under version checks (`optimistic`). Built and tested (a model against
  the locked index, crashes at every write of a checkpoint and restart); not yet measured, nor a way
  chosen.
- **The page path SQLite takes** (G3 step 3, docs/design/store.md decision 42), a prototype behind
  `AMPHORA_PAGE_PATH=plain`, off by default: plain pin counts on a connection's own pages, a shared
  page's buffer counted once as it enters a connection, file misses read in line with no completion
  (Unix), the checksum checked once as a page enters the process. Built and tested (the shared-pages
  model on the plain path; each path reads today's rows; a flipped byte refused on each path); not
  yet measured. The miss still allocates the page and its buffer's `Arc`.
- **Point readers that write no shared line** (G3 step 6, docs/design/store.md decision 43), two
  prototypes, each behind its switch and off by default: `AMPHORA_READ_CHECKOUT=slots` takes a
  store's reader from a slot of its own, with no pool mutex or shared gate per read;
  `AMPHORA_SHARED_PAGE_READS=optimistic` reads shared pages from frames of atomic words under a
  version check, without the shard's lock. Built and tested (slot readers, and 32 readers and a
  writer under the model: no image past a reader's snapshot, none torn); not yet measured (60 s
  run, perf c2c and the 32-client suite are the coordinator's).
- **The mixed log's commit costs** (docs/design/store.md, Decision (lane wal-read)): publishing a
  commit's records costs a lookup each, not a pass over the records kept decoded or over the
  commit's pages, and deltas are found a word at a time. Built, not merged; whether the bulk
  statements now match the page-a-frame log (`AMPHORA_PAGE_LOG=1` in the bench) waits on the
  interleaved bench.
- **The planner where the suite found it choosing far more work** (docs/design/planner.md,
  `crates/engine/tests/planner.rs`): a closed range with unknown bounds is 0.5% of its table whether
  it drives a seek or filters one, so a join from a key range looks each row's partner up by key; a
  sort is charged to the access path that needs it, on the rows it sorts, so a page by key reads in
  key order. EXPLAIN words a range on the rowid as one. Checks on fedora, 1 client (not recorded
  runs): `join_pk` 71 → 94,710 statements a second, `join_three` 56 → 1,598, `offset_deep` 25 → 64.
- **A GLOB prefix seeks its index** (docs/design/planner.md decision 3, `crates/engine/tests/glob_range.rs`):
  `col GLOB p` on a TEXT column implies the range of `p`'s literal prefix, computed when the
  statement runs, so an index seeks it; a BLOB matches no GLOB or LIKE (SQLite's recommended
  `SQLITE_LIKE_DOESNT_MATCH_BLOBS`). The suite asks SQLite's dialect for GLOB where it asks the
  others for LIKE, as their LIKE heeds case.
- **LIKE jumps to its literal** (docs/design/executor.md decision 3): past a `%`, an ASCII pattern's
  next literal is found with `memchr2` in either case, as SQLite's `patternCompare` finds it with
  `strcspn`. `like_substring` 67 → 222 statements a second at 1 client and 867 → 2,556 at 32
  (fedora, best of 3 at load under 2, not recorded runs).
- **Table lookups cost what the cache can't hold** (docs/design/planner.md decision 5): a row read
  through an index that lacks its columns costs a page read by Mackert and Lohman's finite-buffer
  formula, with the connection's own cache (and shared pages), as PostgreSQL's
  `index_pages_fetched` counts them. Of the suite's 50 plans only `table_group_by` changes: it scans
  the table and sorts instead of looking every row up through `items_grp_created`: 4.0 → 14.2
  statements a second at 1 client and 35.2 → 167.5 at 32 (fedora, best of 2 at load under 2, not
  recorded runs).
- **Many searches of a cached index read its pages once** (docs/design/planner.md decision 6): a
  nested loop's searches of an index the connection's cache holds whole are charged its pages
  once at most (Mackert and Lohman, as PostgreSQL charges an index read over many loops). Of the
  suite's 50 plans only `cte` changes: its grouped rows drive lookups of `owners` by key, instead of
  every owner probing an index built on them. `cte` 218 → 324 statements a second at 1 client and
  2,372 → 2,904 at 32 (fedora, best of 3 at load under 2, not recorded runs).
- **Skip-scan** (docs/design/planner.md decision 4, `crates/engine/tests/skip_scan.rs`): a constraint
  on an index's second column alone seeks it once for each value of its first, where ANALYZE
  counted at least 18 rows to a value, as SQLite does. `count_range` 165 → 2,476 statements a
  second at 1 client and 2,644 → 32,892 at 32 (fedora, best of 3 at load 2–7, not recorded runs).
- **Plans without statistics** (docs/design/planner.md decision 8,
  `crates/engine/tests/records_plans.rs`): a column of a composite primary key is costed as any
  indexed column, a unique lookup being a rowid alias's or a one-column key's only. Without ANALYZE,
  kiln's records reads (its versions table) are planned on the index each needs, as SQLite plans
  them, where the newest version walked its family by revision and the change feed sorted it.
- **A subquery the output reads is priced once an output row** (docs/design/planner.md decision 9,
  `crates/engine/tests/planner.rs`): a correlated subquery that an aggregate's or a GROUP BY's
  output reads is costed at the output's rows, as it runs (once a group), not at its loop's. The
  suite's `group_correlated` keeps the correlated form, a search of its one group's items, where it
  was unnested into a scan of all 200,000 entries of `items_grp_created`: 227 → 26,895 statements a
  second at 1 client and 1,231 → 225,470 at 32, best of 3 in turns on adas-mac-studio-2, the controls
  within 2% (`bench/records/2026-10-25-adas-mac-studio-2-output-subquery-calls.md`).
- **Correlated subqueries unnested in general** (docs/design/planner.md decision 22, PL.10,
  `crates/engine/tests/unnesting.rs`): beside the fixed patterns, `NOT IN` becomes a NULL-aware
  anti-join, an `EXISTS`/`IN` of several tables a semi- or anti-join with a derived table, an
  `EXISTS` under `OR` or in the `SELECT` list a grouped left join, and nested subqueries unnest level
  by level from a worklist with no recursion; the cheapest of the written, patterns' and general
  forms is kept. Answers equal nested ones on 39 classes and 24 levels of nesting (T-PL.10.1); a
  subquery reading a semi-joined table stays nested, and Q20's `0.5 * total()` isn't unnested yet. Open:
  q21 at SF 10 and TPC-DS's ten queries measured (AC-PL.10.1, AC-PL.10.2).
- **A one-row lookup orders its table** (docs/design/planner.md decision 10,
  `crates/engine/tests/records_plans.rs`): a loop that gives at most one row for each row before it
  (a unique key's whole lookup by `=`, or the rowid's) takes its table's ORDER BY columns as ordered
  and keeps the order before it, as SQLite's one-row loop does: `SELECT v FROM q WHERE a = ? AND b
  = ? ORDER BY c` on a key (a, b), a rowid lookup ordered by another column, and a join's key lookup
  after an outer scan in rowid order no longer sort.
- **Without statistics, a unique key is searched, not hashed** (docs/design/planner.md decision 11,
  `crates/engine/tests/records_plans.rs`): a hash join whose build table has no ANALYZE statistics
  isn't weighed against a search of that table by a unique key from the other table, as SQLite
  plans the join: `p JOIN q ON q.a = p.x AND q.b = p.y` on q's key (a, b) is `SCAN p` and a search
  of q's key, not a hash join costed on the fallback's million rows. With statistics the costs
  decide; CERT, run without statistics, lost a finding and gained none.
- **A row value seeks the index its columns lead** (docs/design/planner.md decision 14,
  `crates/engine/tests/row_value_seek.rs`): `(a, b, c) > (?, ?, ?)` bounds a, and a search of an
  index on (a, b, c) takes the whole row, from the first key at or past it, the comparison filtering
  every row as SQLite's does. So a keyset page, `WHERE (family, key, rev) > (?1, ?2, ?3) ORDER BY
  family, key, rev LIMIT 8`, reads from the cursor and stops at its LIMIT, and a pass is linear in
  its rows where it was quadratic. A page reading columns the index lacks seeks once ANALYZE has
  counted the table, or under a LIMIT (decisions 16 and 17); the OR chain spelling of a keyset stays a
  scan, as in SQLite.
- **An ordered read under a LIMIT stops at its rows, a bound LIMIT too** (docs/design/planner.md
  decisions 16 and 17, `crates/engine/tests/rowid_index_order.rs`): a one-table read in an index's
  order is costed to the rows a literal LIMIT keeps, and under a LIMIT bound as a parameter, as kiln
  binds every LIMIT, to the q-middle of the fewest and all the read keeps. Keyset pages and kiln's
  conversation feed (`records_by_rev`) search the index as SQLite 3.54.0 does, reading at most
  LIMIT + 2 rows a page, where they scanned and sorted the whole table.
- **Readers keep their pages across commits** (docs/design/store.md decision 18,
  `crates/engine/storage/changes.rs`): a connection starting a read transaction forgets only the
  pages written since its last one, where SQLite and Turso forget its whole cache; it forgets
  everything when the log can't say (another generation, a gap, more pages than it caches). A model
  test of random transactions, checkpoints and restarts, both log formats, never reads a stale row,
  and catches a reader that forgets nothing. With it, readers keep 2,000 pages (decision 5): K0's
  reads are as fast as at 128, the suite's at 32 clients up to 62 times faster, for 8 MiB a reader.
- **The page cache evicts as SIEVE does** (docs/design/store.md decision 19): a new page joins the
  queue's head, so the hand reaches it after every older page; inserted after the hand, as Turso
  had it, a new working set never settled in a full cache (`order_by_index` after
  `point_secondary`, 2,000 pages: 49,044 → 147,133 a second, a check on fedora at 1 client).
- **Shared pages** (docs/design/store.md decision 20, `crates/engine/storage/shared_pages.rs`), opt-in
  (`Db::share_pages`): a page one connection read in, every connection reads from memory, keyed by
  the version it is; a writer copies a shared buffer first, a file write drops the file's version.
  Sharing 256 MiB, the suite's analytic reads run 2–2.6 times as fast at 1 and 32 clients and K0's
  reads 43% faster, for K0's resident set 340 MB against 83 and 5% fewer writes. One pool now serves
  a process's databases (`Db::share_pages_in`), each database's pages under keys of its own; run
  past the suite's first seconds, every read measured is faster shared (point reads 1.33–1.49 times
  at one client), with 32 readers for less memory. Not on by default: that, and its budget, are
  the person's call. A process's pool (decision 26, `Sharing::Process`, `Db::open_sharing`) holds a
  quarter of the memory the process may use (its cgroups' smallest limit and the machine's, on
  Linux; 256 MiB elsewhere); a database that closes takes its pages out, and one whose log on disk
  is packed refuses to share. A lookup copies its page out under its shard's read lock (decision
  29), so readers of the same pages no longer take each other's lines: at ten million rows with the
  process's pool, scans at 32 clients ran 1.5–2 times as fast as before it. Against no pool with
  the OS's cache of the database warm (the suite's settle now cuts the log, so it stays warm),
  reads still run faster with no pool (`case_aggregate` at 32 clients 43,339 a second against
  21,751; `union_all` at one client 206,733 against 17,936 while the pool fills), and the pool's
  gains are writes at 32 clients (`update_indexed` 4,696 against 3,265) and caches the OS can't
  hold. So the default stays unshared.
- **The writer reads committed pages from the pool, keeping none** (docs/design/store.md decision
  27): pages committed before its transaction began it finds in memory; what it reads it doesn't
  keep, since it is about to change it. Keeping them filled the 8 GiB pool with a 10M-row load's
  log versions, all dead once the log restarted.
- **A reader's snapshot takes no lock** (docs/design/store.md decision 28): the log's generation is
  read from a copy every header guard refreshes, and spin locks test before they take. Point reads
  at 32 clients, ten million rows, no pool: 701,613 → 1,572,331 a second.
- **Key heads for in-page search** (docs/design/store.md decision 38, `crates/engine/storage/key_heads.rs`;
  G3 step 4, PR.4), a prototype behind `AMPHORA_KEY_HEADS=1`: a directory of 4-byte key heads
  beside a cached page, built once the page's searches have paid for it, replaces a probe's decode
  and compare when the heads order the cell. Built and model-tested (T-PR.4.1); not yet measured
  (AC-PR.4.1, AC-PR.4.2).
- **Skipping the descent** (docs/design/store.md decision 41, `crates/engine/storage/descent_skip.rs`;
  G3 step 5, G3.5): two cores that name a point read's leaf without descending, each answer proven
  by the leaf's own range: InnoDB's adaptive hash index, built a leaf at a time by a ski rental
  counted in probes, and a learned rowid-to-leaf locator whose error bound is its own fit's. Built
  and model-tested against a plain descent and on the engine's pages against the cursor (T-G3.5.1-3).
  Wired into the cursor behind `AMPHORA_DESCENT_SKIP` = off | hash | locator (default off): a read
  cursor's point seek stands on a named leaf clean in the connection's cache, stamped by the
  connection's freed-page count and its snapshot; every other move descends first (T-G3.5.4). Not
  yet measured (AC-G3.5.1, AC-G3.5.2: `bench/descent_skip.sh`).
- **A shared page found in one lookup** (docs/design/store.md decision 22): the pool keeps each image
  in its map's entry and counts lookups in each shard, under the lock it holds anyway, where one
  count for the pool moved between 32 readers' cores at every lookup. Sharing 256 MiB at 32 clients:
  `order_by_sort` 8,566 → 11,660 statements a second, `exists_subquery` 11,330 → 15,483,
  `group_by_having` 8,169 → 9,851 (fedora, best of 2 at load under 2, not recorded runs).
- **A page read records itself only when it must wait** (docs/design/store.md decision 21): a page
  read from the file goes straight into the cache, recorded for a resumed call only when a spill
  holds it up; every read had taken two write locks and a hash insert and remove. Whole scans
  5–7% faster (a probe of the engine on fedora, checks).
- **A commit ends before its pages outgrow the writer's cache** (docs/design/store.md decision 23):
  the writer takes whole batches into a commit until the next might make its cache spill written
  pages into the log, and commits the rest at once after (`Engine::apply_batches`,
  `Db::transaction_while_room`); statements of a hundred rows had made commits of 3,400 pages
  through a 1,024-page cache. At 32 clients `insert_batch_100` 333 → 456 statements a second (p99
  203 → 131 ms), `delete_range_insert` 2,841 → 3,136, peak resident 102 → 59 MB (fedora, checks).
- **The checkpointer lists the pages of the frames it copies** (docs/design/store.md decision 24):
  from the frames' own list, copied out under its lock and sorted after, where every checkpoint had
  scanned every page the log holds under the lock each page read from the log takes; the scan stays
  for ranges the list doesn't hold. A model test checks the list against the scan over random
  appends, packed records, rewinds, rollbacks and restarts.
- **A checkpointer that copied everything durable waits for a flush** (docs/design/store.md
  decision 25): past its bound it had passed about 10,000 times a second copying nothing, each pass
  taking locks and lines the writer takes at every commit; a pass that copies nothing now waits for
  the next flush.
- **A transaction's spills write over its own frames** (docs/design/store.md decision 30): a page
  spilled again goes back over the frame its transaction wrote it to, and the commit chains the
  frames' checksums again before its own, as SQLite's `walFrames` does; a transaction's log holds at
  most a frame for each page it dirtied before its commit, and two after, where it took a frame for
  each change after a spill (the unify audit's 8 MiB log-search transaction: a 7 GiB log beside a
  120 MiB file; reproduced on its corpus, 7,041.7 MiB → 241.1 MiB for 30,689 dirtied pages). A
  power cut anywhere in such a transaction keeps the last commit (crash injection, 10,000 seeds).
- **A restart cuts a log past twice its bound to nothing** (docs/design/store.md decision 31):
  `Db::restart_log` keeps the log's file while it holds room for no more than twice the store's
  bound (`Db::keep_log`), and past it cuts the file to nothing once every frame is in the database
  file; a log's high water had stayed on disk for good.
- **Many writers, stage 1** (docs/design/engine.md, `crates/engine/storage/versioned`): B-link
  trees on one shared buffer pool, in memory: writers on different leaves at once, readers through
  copies of inner pages validated by version, a root split whole under the root's latch (decision
  3; its race reproduced by a test that pauses the splitter, then 50 of 50 runs of the concurrent
  test beside the whole lib suite on fedora and on the Mac, 2026-10-24); rows versioned in place
  (snapshots, first writer wins, rollback, collection by the oldest open snapshot); not yet under
  the executor. One tree,
  2,000,000 random keys, unversioned: 660,371 puts a second at one thread, 8.55 million at 32
  (fedora, a check).
- **Many writers, stage 2** (docs/design/engine.md decisions 6 to 8,
  `crates/engine/storage/versioned/{disk,redo,durable}.rs`): a redo record a commit, group-committed
  by one flush for every commit appended before it; checkpoints in a brief pause (copies of the
  pages written since the last, and the rows open transactions changed, put back as of the
  checkpoint) through alternating journals; recovery from the newest complete journal and the redo
  after it, never an empty database over an existing file. Crash points through commits and
  checkpoints recover a prefix of the commits at least as long as the last acknowledged durable;
  the pool still holds every page, and nothing runs it under SQL yet (stage 3).
- **The executor's cursor on the versioned storage** (docs/design/engine.md decision 9, NS.1;
  `crates/engine/storage/versioned/cursor.rs`): `CursorTrait` on a B-link tree's rows as a snapshot
  sees them, keyed by rowid or by an index's record; it copies a leaf at a time under its shared
  latch and finds its row again by key once the leaf's version moves, so it sees its own
  transaction's writes through other cursors. A conformance suite (`conformance.rs`) drives it and
  the current engine's B-tree cursor through the same random operation sequences on a table and two
  indexes (one descending and NOCASE) and compares every step. It puts back a large row's overflow
  chunks after each leaf's copy (`AC-NS.2.4`). Not yet: a comparison per tree in one database, the
  connection's wiring (step 2).
- **Savepoints on the versioned storage** (G7 step 2 in part, `NS.2`;
  `crates/engine/storage/versioned/{version,txn_model}.rs`): a transaction rolls back to a savepoint
  from its versions (rows first changed after it as before the transaction, rows changed before and
  after as it found them, later changes out of the redo record). The shared model test of
  transactions (`txn_model.rs`, `T-NS.2.1`) runs the same random transactions with savepoints on the
  current engine through SQL and on the versioned storage, every read and scan compared, and checks
  each snapshot sees exactly what was committed when it opened (300 seeds pass; the 10,000 are the
  batch's). Overflow rows (`AC-NS.2.4`, `overflow.rs`): a value past a cell's room keeps that many
  bytes in its cell and the rest as versioned chunk rows of the database's overflow tree, written by
  the same transaction, so snapshots, rollback, savepoints, the redo log and recovery carry them; a
  page now splits by bytes with the new cell counted (a split by count found no room once cells
  near the largest size gathered on one side). Trees made and dropped while running (`AC-NS.2.5`): a
  redo record each, a dropped tree's root listed as 0 in the checkpoint, both replayed; dropped
  trees' pages stay unreferenced (the pool has no free list yet).
- **Eviction on the versioned storage** (G7 step 4, `NS.4`, docs/design/engine.md decision 14,
  `crates/engine/storage/versioned/{pool,evict}.rs`): a database's pool holds the frames its byte
  budget pays for and evicts by CLOCK; dirty pages go to a spill file recovery never reads (the data
  file still changes only by checkpoints), pages read in are checked once against a checksum, and a
  pool whose every frame is latched refuses typed (`MemoryBudget`). Tests: a database over ten times
  its pool across a checkpoint and a reopen, a crash at every disk operation of a spilling,
  checkpointing workload losing no acknowledged commit, and the pool's model test with eviction
  under 32 threads. Not yet: the 10M-row corpus run and its resident memory (the coordinator's);
  checkpoints write and recovery reads their journal a page at a time; the shell passes the budget once the switch
  takes it from `amphora::memory::limit`.
- **The switch: SQL on the versioned storage** (G7 step 2, `AC-NS.2.1`, docs/design/engine.md
  decision 13, `crates/engine/storage/versioned/{switch,sql}.rs`): `ConnectOpts::with_versioned_storage`
  or `AMPHORA_STORAGE=versioned` puts a database's B-trees on the versioned storage; the executor's
  cursors, CREATE/DROP TABLE and INDEX, DELETE without WHERE, `sqlite_schema` (tree 1) and ANALYZE
  run on it, each SQL transaction one storage transaction, a failed statement undone alone by a
  savepoint. `sql.rs` (`T-NS.2.6`) runs the same SQL on both storages and compares. A file database
  reopens and recovers from a crash with its indexes (each tree's order in the redo log, the schema
  read from tree 1); the storage's budget is `ConnectOpts::with_memory_budget` (the shell's quarter
  of its process's memory), else a quarter of the machine's, its frames made as pages are handed
  out (`T-NS.2.10`); DDL undone by a failed statement, a savepoint or a failed commit leaves no
  orphan tree (`T-NS.2.11`). Not yet: the header (schema cookie, user version) on the versioned
  storage (still the pager's); a database past the budget's frames (G7 step 4's eviction); one
  budget shared by a process's databases. On the pager as on the switch, `nested_loops`'
  chain of hash joins, `row_counts`' hash join and `sample_filters`' outer join fail (planner
  work in flight on main), and a failed CREATE TABLE ... AS in a transaction leaves the pager's
  next statement refused `SchemaUpdated`.
- **Three ways to reach a cached page** (G7 step 3, `NS.3`; `crates/engine/storage/reach`,
  `amphora-bench --reach`): behind one trait, sharing frames, CLOCK eviction and one miss path, (a) a
  page table in shards with pins and a private pin on a client's current page (PostgreSQL), (b)
  references by frame index read optimistically under a version, atomics standing in for
  unsynchronized reads (LeanStore, Umbra), and (c) a direct array of frames by page number
  (vmcache without the page table). Shards follow the machine's threads, the array the file's pages.
  The model test with eviction under 32 threads (`T-NS.3.1`) passes on all three. Not yet: the
  harness's runs (point reads and scans at 1 and 32 clients, cached and a pool a tenth of the file)
  and the choice by their numbers, the coordinator's; misses read an in-memory stand-in for the file.
- **Serializable validation, three prototypes behind one switch** (G8 step 1, docs/design/engine.md
  decision 5, `crates/engine/storage/versioned/{version,elle}.rs`, `amphora-bench --cc`): precision
  locking (the default), SSI and Silo-style OCC validate commits on the versioned storage's commit
  path; Elle's list-append and register histories checked for G0, G1a, G1b, G1c, G-single and G2
  (snapshot isolation the control, showing G2). Not yet: the 10,000-seed runs and the three schemes'
  throughput, refusals and p99 on the suite's write classes and K0 (the batch's), then the choice.
- **Versions collected, memory bounded** (G8 step 2, docs/design/engine.md decision 10,
  `crates/engine/storage/versioned/version.rs`): collection keeps of each row only the versions some
  open snapshot sees, so a long reader holds one version a row; versions, transactions' records and
  commits kept for validation are charged to one budget (by default the pool's bytes, or the shell's
  `with_budget`) and refused past it with a typed `MemoryBudget`; commits are kept only while a
  transaction begun before them is open; reads past the budget escalate to the whole tree. Tested by
  a short soak (one reader, 32 writers). Not yet: versions moved out of memory for long readers
  (LeanStore's graveyard), the hour-long soak (`AMPHORA_G8_SOAK_SECONDS=3600`, the batch's), and the
  shell passing a budget learned from the process's memory once it opens this storage.
- **Ephemeral tables kept between runs** (docs/design/executor.md decision 1,
  `crates/engine/tests/ephemeral_reuse.rs`): a statement keeps each ephemeral table of at most 16
  pages (an IN list's keys, an IN subquery's rows) and empties it on its next run, where building
  one each run (a temp file, a pager, a transaction, a B-tree) was a third of `in_list_10`'s time.
- **`IN (SELECT ...)` searches its list once a row** (docs/design/executor.md decision 2,
  `crates/engine/tests/in_subquery.rs`): as a condition a miss fails the row at once; as a value or
  under `NOT IN`, a one-column list's first entry says whether it holds a NULL, where every miss
  read the whole list. `subquery_in` 213 → 784 statements a second at 1 client, 2,846 → 6,833 at
  32 (fedora, best of 3 in turns, not recorded runs).
- **GROUP BY in a hash table; ORDER BY ... LIMIT keeps its top rows** (docs/design/executor.md
  decisions 4 and 5, `crates/engine/tests/top_n.rs`,
  `crates/engine/vdbe/group_table.rs`, `crates/engine/tests/hash_group_by.rs`): a GROUP BY whose
  rows aren't in group order and whose aggregates keep small states aggregates each row into its
  group as it arrives, where every row was sorted first; the groups past a 64 MiB budget go to the
  sorter as before and the two merge in group order. An ORDER BY with a LIMIT (and OFFSET) of up
  to 65,536 rows keeps only those rows in its sorter, where every row was sorted (the engine's heap
  for it was chosen only where a LIMIT's registers already existed, almost never).
- **A subquery runs where it is read** (docs/design/executor.md decision 6,
  `crates/engine/tests/oracle_findings.rs`): a subquery outside FROM is a subroutine each read
  calls, an uncorrelated one's body behind its own Once, so one that no row reaches never runs
  (over an empty table, past a WHERE term no row passes, in a CASE branch none takes), as in
  SQLite. A correlated one runs at each read too, wherever what it reads of the outer row is: a
  direct-output SELECT's loop, an aggregate's or a GROUP BY's output (from the values kept for it),
  a window query's source, a RIGHT or FULL JOIN's row body, a write statement's SET, WHERE and
  RETURNING; only one in an aggregate's arguments, a GROUP BY key or a RIGHT or FULL JOIN's WHERE
  still runs at every row (`a_correlated_subquery_no_row_reads_never_runs`). A WHERE term holding a
  subquery is tested in the first loop, not before the loops. The suite's `subquery_in` and
  `exists_subquery` are no slower:
  time per statement within 1% at 1 and 32 clients, best of 3 in turns on adas-mac-studio
  (`bench/records/2026-10-25-adas-mac-studio-lazy-subqueries.md`). A correlated subquery in a grouped
  query's output runs once a group, not once an input row: `group_correlated_row` 15.8 times as fast
  at 1 client and 11.4 at 32, the suite's subquery statements within 1%
  (`bench/records/2026-10-25-adas-mac-studio-2-corr-subqueries.md`).

- **The suite counts what it measures** (`bench/src/suite.rs`): a run counts if it ends within the
  window, whenever it began (one begun in the warmup that outlasted the window used to blank a
  client's whole run: `exists_subquery` read 0/s on Amphora, SQLite and PostgreSQL at ten million
  rows), each statement records its longest run, every client draws its own parameters (clients
  0, 2, 4, ... drew the same), and every database checkpoints after its load before any run
  (`settle_s`; Amphora's loader had left the whole load, 7.5 GB, in its log). `--query` runs a
  statement on a database a run left behind.
- **The leader's write slot** (AC-T1.8.W1, Amphora goal 9c63f7d4; replication.md, "the write slot"): a member
  whose write is refused at the leader's append gets a slot for its next try, its length learned per member from
  its retries' gaps (cold start: the core's election timeout). A gap over which the host's detector suspected the
  member is not learned; no number bounds a gap (lane g-slot-close, 2026-11-21).
- **The raft log as the WAL** (G12 step 2, AC-T1.8.L1 and L2; replication.md, "The raft log as the WAL"): the engine
  applies committed entries without flushing its log per commit; compaction asks nothing of the owner's thread and
  moves the raft log's start no further than what the flushing thread made durable (T-T1.8.L1). The Records suite
  and seeded histories run on one harness at one voter and three, with identical transcripts (T-T1.8.L2; lane
  g12s2, 2026-11-21).

- **Backups and point-in-time recovery to Mantle, the mechanism** (CP-T1.12, lane pitr,
  `crates/amphora/src/backup*`, docs/design/backup.md): the object-store seam (`amphora_api::blobs`,
  its simulator and conformance suite); commits stamped by the writer (one mark a second); the log's
  durable frames shipped as segments with no timer; bases copied chunk by chunk without a reader;
  log restarts held until shipped; restore to any second by the engine's own recovery, refusing
  seconds the archive can't say exactly. Restore drills at random seconds on the simulator: every row
  equal (T-T1.12.D1). `Database::open_backed_up` backs a local writer up: its commits stamped and a
  thread that ships and copies bases as it commits, across a close and a reopen (T-T1.12.D2). Not
  yet: kiln's adapter to Mantle, and the 100 GB restore measurement (AC-T1.12.3).
  The folder store (`backup::FolderBlobs`, lane pitr-7): the owned default, a peer's folder over a
  mount the same, passing the seam's suite and answering as the simulator (AMPHORA-BLOBS-8).
  Restore-and-check (`backup::restore_and_check`): the newest whole second restored to a scratch file,
  integrity-checked and its rows compared with the source's, typed and counted, due on the archive's
  cadence (T-T1.12.C1, C2). One cut across groups (`backup::restore_cut`): every group restored to
  one second, half-done cross-group transactions finished by their records (T-T1.12.X1). Not yet:
  the source's digest taken by the shipping thread itself; cuts across nodes' clocks. Incremental
  bases (T-T1.12.I1 to I3): a new base keeps the latest's chunks the log left unchanged, due once the
  log outweighs what it would copy, after a reopen too.
  Lane pitr-8: the shipping thread checks itself (`Archive::check`): the source's own rows from a
  reader snapshot lined up with a second's stamps, checked once that second settles and the archive
  holds it, the result in `Status::last_check` and the check counters (T-T1.12.C3); a due base waits
  for the log's next restart, one generation at most (T-T1.12.I4); cuts across machines: each
  cross-group step carries a hybrid logical clock, so a decision is stamped after the writes it
  decides whatever the nodes' clocks read (T-T1.12.X2).

- **Transactions across groups** (CP-T1.10, G13 step 3; `amphora::xgroup`, docs/design/xgroup.md): two-phase commit
  over groups and Parallel Commits, behind `Protocol`, sharing one group-side step logic on the simulator's tablet and
  on a group's database (`Cluster`, each step one call to the group). A crashed coordinator's next incarnation fences
  the old one and finishes what it left. Tested: a crash at every step (T-T1.10.1) and seeded runs with lost and late
  steps and crashes, Elle-style. Left: the LAN p99 (AC-T1.10.2) on multi-node groups and the 10,000-seed run, which
  decide between the two; SQL sessions across groups (AC-T1.10.1).
- **Picked constants replaced by derived ones** (lane constants, kiln goal dfabffcc; Ada, 2026-11-11: "learned and
  stat derived values only"): a page's chain of deltas ends where its bytes reach a page (store.md decision 17); the
  store's log is copied past its bound (decision 35); a term runs on the whole sample (planner.md decision 21); a
  drained lease is probed from the probe's cost to the learned window (J1); a class entry's bound, the patience's
  voters and a change's charge follow from what they bound; an owner's own session lasts its role's raft timing; and
  an owner's connections are bounded by the process's descriptor budget, the system's limit less what is open
  (local-owner-transport.md, "Decision (lane constants): one descriptor budget"; FD.1), with a replica's local
  readers following it; the replicated owner drives once a step, in turn with its inputs (replication.md, "Decision
  (lane constants): one drive a step"; DQ.1); and each owner charges its lease, or a client its socket, to the same
  budget, no count of owners (`OWNERS_MAX` gone; AC-FD.1.4); the owner wakes on its inputs, not a poll (WK.1); the
  quorum patience's and the write slot's generations are learned from their own events (GN.1); and a generation of
  the database log's restarts ends once it alone covers its own spend at the p99.9, the next restart guessed by the
  worst of the last thousand holds (`log_bound::GENERATION` gone; replication.md, "Decision (lane constants): a
  generation of restarts ends once it covers its spend"; GN.2); and the replicated owner's tick period is learned
  from its voters' round trips, its flushes, its leader's link and its timer by hyper-timing's law, 10 ms only the
  start state before they are measured (replication.md, "Decision (lane constants): the owner's tick pace"; TP.1); and the raft log is compacted by Ongaro's
  rule at an expansion learned from the owner's own measured image and replay costs a byte, its machine reporting its
  image's size (replication.md, "Decision (lane constants): the compaction's expansion learned where it runs"; CT.1); and a
  command's and an image's envelope is what their encoding adds ("Decision (lane constants): an envelope is what its
  encoding adds"; EN.1); and the process has one memory budget, its limit read where it runs (cgroups and RSS on
  Linux, the platform's readings on macOS and Windows, a typed refusal where none can be read) and shared out by
  measured use (store.md, "Decision (lane constants): one process memory budget"; MB.1), an image bounded by its room
  for one in place of `SNAPSHOT_BYTES` (replication.md, "Decision (lane constants): an image is bounded by the memory
  budget's room for one"; SB.1).
  Left: an owner's other descriptors (endpoint, engine files, a replica's log) charged by kind, the core's tick
  counts (twenty an election, two a heartbeat: raft-rs's defaults, fixed in tick mode), the owner's queue
  and outgoing bounds, and the rest of the list (handoff/amphora-constants-list.md).
- **Region-local groups under a global root** (CP-T1.11, G14 step 1, lane regions; `crates/amphora/src/regions.rs`,
  docs/design/regions.md): members carry a region label; a regional group's voters and lead stay in its home
  region, and the root spreads its voters so losing the regions it survives leaves a quorum (CockroachDB's
  survival goals); a lost member is replaced from its region, never from another. Regional rows route to their
  home region's directory, and reads try the reader's region first. The simulated lab (`regions::lab`, latencies
  as its input) gives the same answers and rows at one node, three nodes and three regions. Open: the SQL corpus
  at three regions on the replicated shell, the 60 ms lab's p99 (the coordinator's), the region-loss drill.
- **Groups that split, merge and move themselves** (CP-T1.9, lane split-merge; `crates/amphora/src/reshard/`,
  docs/design/reshard.md): a pure core: a directory of hash spans with every position in exactly one group,
  each group's own holding (fenced, stale actions refused), a planner deciding from measured load, bytes and
  cores, and a driver doing copy, catch-up in rounds and a fenced cutover. Tested in a deterministic
  simulator (T-T1.9.S1). Connected (lane split-merge-4): the directory is a replicated group's state
  (`reshard::replicated::DirectoryMachine`, a hyper-durable state machine of directory changes; its
  leader runs the driver under its term), and `Groups` routes by the directory instead of `fnv % n`, each
  group checking every write and read against its holding (`store::Refused::Away`) (T-T1.9.R1, R2).
  Kept (lane split-merge-5): each group's holding is kept by its own engine with its rows
  (`Engine::holding`/`keep_holding`), durable before the group acts on it, and a group started again
  answers from it whatever directory it is started by; a directory member keeps the image it installed
  (`DirectoryMachine::open`), so a restart needs no catch-up (T-T1.9.R1 to R3). Copied (lane
  split-merge-6): an engine keeps a log of its commits and copies a span by position (`Engine::head`,
  `export`, `changed`, `import`, `keep_log`, `sweep`); the old group's log is held from the first copy
  until the operation ends; `Groups::carry` carries every driver action to this node's groups, so a split
  or merge moves the range's rows, through repeated actions and driver restarts (T-T1.9.R4, R5), with the cutover-pause and lab harnesses (T-T1.9.P1, L1).
  Bounded (lane split-merge-8): every copy-path batch holds no more units than the process's memory
  budget (its pool of pages, decision 26) takes at the most bytes a unit has held, so a batch doubling on
  an idle node stops there (T-T1.9.R8).
  A local database's Records move (lane split-merge-8): a row routes by its namespace, family and key
  (`reshard::row_key`; needs Ada's word), and `database::Local` carries the copy path over its Records
  rows, indexed by position from its first holding, with a log of its commits and a feed while held
  (`database/group.rs`; T-T1.9.R9).
  Moved across nodes (lane split-merge-9): a copy or catch-up batch reads its old group through a
  pull (`groups::Pull`, answered by `Groups::serve` where the group is, through `groups::Peers` from
  another node), and a group moved to a second node under writes keeps every acknowledged write
  (T-T1.9.R10). Over the transport (lane split-merge-10): a pull and its answer are appended variants of
  the owner transport (version 7), a node's endpoint answers them (`transport::Node`) and its peers
  ask through it (`transport::Nodes`); a move through loss and partition keeps every acknowledged
  write (T-T1.9.R12). Left: that transport off loopback (between machines), a `Database` over
  `Groups<Local>`, load from the store's meter, and the full AC-T1.9.2 and AC-T1.9.3 runs.

- **Online index builds by F1's states** (CP-T1.12, T-T1.12.1, docs/design/online-schema.md):
  `Database::change_schema` builds a `CREATE [UNIQUE] INDEX` absent, delete-only, write-only, then
  public, each step a transaction through the writer, with a backfill in batches sized as it runs
  (a batch holds the writer about as long as it waited for it). Reads use the index only once
  public; the cutover changes only the catalog. A power cut at every change of a build leaves the
  old schema or the new; rows that break a unique index undo the change. The 10M-row cutover p99
  (AC-T1.12.2) is not yet measured. A column (`ALTER TABLE … ADD COLUMN`) is added in one catalog
  change, its states run together, as no row is rewritten or read; one whose addition reads every
  row is refused. NOT NULL and CHECK are added to an existing column (`ALTER COLUMN c TO` its own
definition plus the constraint) write-only, then validated in learned rowid batches, then public by
a catalog change; the planner doesn't rely on an unvalidated NOT NULL, a breaking row undoes the
change, and a power cut leaves the old schema or the new. DROP COLUMN is built PostgreSQL's way: one
catalog change hides the column as a slot (no name finds it, `*` and inserts pass it, every write
stores NULL in it), learned rowid batches clear its old values, then the drop is recorded.

## Measured

- **The oracles: wrong answers and slow plans** (G2 step 3; `amphora-bench --oracle`, `bench/src/oracle`,
  `docs/research/oracles.md`; the registry `bench/oracles/findings.md`; every run in
  `bench/records/2026-10-24-adacachy-oracles/`; nightly in `.kiln/oracles.yaml`): random databases and
  queries over the inventory's features, each query checked by TLP, NoREC, PQS, DQP, EET and CERT, by
  the tables' rows, and against SQLite 3.54.0 built with the system's options and
  `SQLITE_LIKE_DOESNT_MATCH_BLOBS` (`bench/oracles/sqlite.sh`); every finding cut down to its smallest
  form and filed under its cause. Five runs on adacachy, 2026-10-24: seeds 11 and 12 (978 and 3,824
  databases, 35,104 and 154,695 checks, 70 and 84 findings), the injected bug's (1,759 databases,
  71,127 checks, 245 findings), and seeds 13 and 14 on the fixes (24,321 and 27,091 databases, 983,406
  and 1,096,012 checks, 61 and 70 findings, 37 and then 12 of them not CERT's). 49 causes fixed in
  Amphora, each from a failing test (`crates/engine/tests/oracle_findings.rs`, 59 tests; `real_text.rs`,
  4,756 REALs as SQLite writes them), among them SQLite 3.54's evaluation order, its number in text
  holding a NUL, NULL routing under NOT, comparisons that kept their conversions, and two panics of
  subqueries run after the read that needs them; the oracles' own artifacts, fixed; five not Amphora's
  (SQLite's own bugs, evaluation order, the BLOB LIKE option, ill-formed UTF-8 per
  `docs/design/semantics.md`, SQLite's names in a folded AND). CERT's estimates (cause AG, 87
  findings) are clean since the planner's goal (step 5, 2026-10-27,
  `bench/records/2026-10-27-adas-mac-studio-2-cert-clean/`): every CERT finding of the campaigns
  replays clean, CERT checks every shape the generator makes (grouped, aggregated, DISTINCT, ordered,
  limited, compound), and six runs of every oracle on its last tree (seeds 11 to 13 and three nights',
  1,800 s each) fail none of 2,607,858 CERT checks and find only SQLite's own LIKE bug (cause AH);
  the step fixed 14 more causes in Amphora from failing tests (`oracle_findings.rs`, 79 tests), among
  them a RIGHT JOIN's OFFSET, atanh's last digit (`docs/design/semantics.md`, decision 2) and a
  panic of a RIGHT JOIN's unmatched-row pass. **The test:** a wrong-result bug injected into the optimizer (`x IS
  NULL` taken as rejecting NULLs) was caught by one nightly run: 125 findings, the first at 0.47 s
  of the 1,800 s (`bench/oracles/injected.sh`).
- **The SQLite language against every engine** (G2 step 1, `bench/corpus/run.sh`; published in
  `bench/corpus/matrix.md`, every cell in `matrix.json`, the probes' answers in
  `bench/records/2026-10-24-adacachy-corpus-probes/`): SQLite's documentation (3.54.1) inventoried into
  570 entries: 71 syntax productions, 144 functions, 19 date modifiers, 74 PRAGMAs, 51 operators, 5
  parameter forms, 4 expression forms, 35 rules of affinity, comparison, sorting and collation, 13
  types and collations, 147 keywords and 7 context words. `check_inventory.py` finds nothing documented
  missing against every list the documentation keeps and against SQLite 3.54.0's own function and
  PRAGMA catalogs. 510 probes ran on nine engines, SQLite the reference; every entry has its status and
  its spelling in each (sqlglot's translation, one of 468 hand-written spellings, or one of 441 reasons
  an engine has none). Answering as SQLite does: Turso 466 of 570, Amphora 463, PostgreSQL and OrioleDB
  309, DuckDB 292, MySQL 278, CedarDB 259, ClickHouse 252. Where Amphora doesn't (57 entries): no ORDER
  BY inside aggregates; no median or percentiles; no json_array_insert, jsonb_array_insert, jsonb_each
  or jsonb_tree; ATTACH, VACUUM, WITHOUT ROWID and generated columns behind experimental flags; about
  thirty PRAGMAs that answer nothing; and two wrong answers it shares with Turso: `'a' IN ('A' COLLATE
  NOCASE)` is 1 where SQLite says 0 (the left operand's collation decides), and `PRAGMA index_xinfo`
  names the rowid's column '' where SQLite gives NULL. CedarDB's server dies on
  `SELECT 0 OR NULL, 1 OR NULL`, and again when it's tried a second time.
- **The corpus's runner** (G2 steps 4 and 7, lane corpus-ci; `crates/corpus`, in the gate):
  sqllogictest records read, run, rendered (I, R, T as slt_sqlite.c writes them, REALs through Amphora's
  port of SQLite's printf), sorted and compared (MD5 past the hash threshold) as SQLite's own runner does,
  one code path for the gate's subset and the bench's run over every engine; each query after a statement
  settles the engine first (`PRAGMA wal_checkpoint(TRUNCATE)`), and each disagreement classifies as an
  Amphora bug, a competitor's bug, a dialect difference or a translation gap. `amphora-corpus run <pool>
  --budget-s <s>` runs a ranked pool on Amphora: required files whole, the rest until the budget, timed on
  the machine as it runs, so the subset's size is what that machine runs in the budget. The gate's pool
  (`bench/corpus/subset`, chosen by `bench/corpus/subset.py` from measured times: required files reach
  every entry Amphora answers right somewhere, then query types per measured second up to the plan's 2
  minutes) runs in `scripts/gates.sh`, its times kept as CI artifacts; `.kiln/corpus.yaml`
  (`bench/corpus/nightly.sh`) builds and runs the whole corpus nightly and re-ranks the pool. No record
  runs past the budget (lane corpus-ci-4): the engine stops it (Amphora through its own query deadline,
  which its VM checks as it steps; a shell by its process ended; ClickHouse by its max_execution_time)
  and it counts as past the budget, which fails the gate for a required file. The engine seam
  (`crates/corpus/src/engines.rs`, `--engine <name>`) opens every engine of step 4: Amphora in-process,
  SQLite, DuckDB and Turso through their own shells, PostgreSQL, OrioleDB and CedarDB through `psql`,
  MySQL through `mysql`, ClickHouse through its HTTP interface, each kept open for a file and settled
  before each query; an engine a machine lacks is refused (exit 3), and the nightly runs every one,
  each on its share of the night's budget, and fails when any couldn't run. Lane corpus-ci-6: an engine's
  opening, settling and closing statements are bounded by the run's budget (a server that doesn't answer
  is unreachable, never a hung run); an untyped column is declared BLOB by SQLite's affinity rule 3
  (DuckDB's comparable records on 40 written files: 152 to 193); Turso's list output, which escapes
  nothing, is counted again by Turso when an answer disagrees, and a misread one is unreadable, not a
  disagreement; `CORPUS_BIN_<ENGINE>` may be a command (a client in its server's container). The five
  server engines ran a pool through the harness on jupiter's Docker (2026-11-21). Left: kiln's queries
  in the pool (kiln_queries.py needs sqlglot).
- **The correctness corpus, run on SQLite** (G2 step 2, `bench/corpus/corpus.sh`, sources and revisions
  in `bench/corpus/sources.json`; published in `bench/records/2026-10-24-adacachy-corpus/corpus.md`):
  114,556 distinct query types (a statement's shape with the inventory entries it exercises; 56,691
  counted coarser, by statement kind and the syntax, functions, operators and PRAGMAs it uses), and
  every one of the 570 inventoried entries exercised by at least ten distinct statements. Six sources:
  sqllogictest whole, 622 files and 5,939,879 records run as its own runner runs them (32-bit integers,
  SQLite's `%.3f`, text by bytes), every answer agreeing but one, the file's own unfinished `sum()`
  overflow record; DuckDB's, CockroachDB's and PostgreSQL's suites translated by sqlglot where SQLite
  takes them, 152,337 records read and 47,181 kept; kiln's 507 statements at the commit production
  runs (8adeadba); and 6,410 written cases, at least ten for every entry and families for window
  frames, recursive CTEs, JSON, UPSERT and RETURNING, generated columns, STRICT, collations, triggers
  and PRAGMAs. Every answer the corpus carries is SQLite's own: the translated sources' files run back
  on SQLite agree, 48,128 records, after six PostgreSQL queries reading stored 'now' were kept as
  statements. Against the source engines' own answers the kept records disagree 7,474 times, each
  under its cause: the same SQL meaning something else on SQLite 2,728, PostgreSQL refusing what
  SQLite accepts 1,734, types SQLite doesn't have 1,274, rows from data the translation couldn't carry
  654, dates and times 542, order 356, plans and catalogs 126, PostgreSQL's aborted transactions 57,
  integer division 3.
- **The v6 matrix: the suite at ten million rows and K0, eight engines, best of 3** (adacachy,
  2026-10-23; `bench/records/2026-10-23-adacachy-matrix-v6-{suite,k0}.jsonl`, scored by
  `python3 bench/best_of.py` into `2026-10-23-adacachy-matrix-v6.md`; the code measured is this
  round's, 4acff7d; load 3.5–124 from other work, so not the quiet machine CLAUDE.md §7 asks). Of
  the suite's 99 cells Amphora is first in 19 and ahead of every row store in 37. Losses first:
  whole-table and grouped statements against CedarDB and ClickHouse by two to four orders of
  magnitude (`order_by_sort` at 1 client 0.4 a second against CedarDB's 2,397, `filter_no_index`
  0.6 against 2,608, `table_group_by` 0.2 against 276, `table_count` at 32 clients 140 against
  ClickHouse's 14,127), and against the best row store too (`order_by_sort` 0.04x, `table_group_by`
  0.12x, `case_aggregate` 0.19x); one client's point reads against SQLite (`blob_read` 0.31x,
  `text_read` 0.43x, `point_pk` 0.82x); inserts at 32 clients (`insert_batch_100` 0.17x CedarDB,
  `delete_range_insert` 0.47x, `insert_one` 0.76x PostgreSQL). Ahead: updates, upserts and
  read-write transactions at 32 clients 1.65–3x the best competitor, `point_pk` at 32 clients 1.1x
  CedarDB, `in_list_10` and `like_prefix` at 32 clients 5x. K0:
  Amphora writes 7,601 events a second at p99 9.3 ms (PostgreSQL 5,272 at 10.9 ms, the rest 1,399
  and fewer), history reads at p99 0.109 ms (PostgreSQL 0.218), in 83 MB (PostgreSQL 253, CedarDB
  938, ClickHouse 1,435); search loses (p50 74 ms against CedarDB's 2.9 and SQLite's 6.0). The 50
  statements are the suite's whole reach: G2's corpus replaces it. Two caveats from the read-path
  profiles (`bench/records/2026-10-23-adacachy-read-path.md`): Amphora's reads ran with its load's
  whole log still in place (the settle's TRUNCATE checkpoint left it), and the bench's SQLite is
  rusqlite's bundled build, which serializes its connections on a shared page cache's mutex and an
  allocator mutex and recompiles `point_secondary` on every call. Five of the 37 leads over every
  row store are leads over that SQLite (`join_pk` 1.97x, `join_three` 1.55x, `like_prefix` 1.19x
  and `point_secondary` 1.02x at one client, `count_range` 8.55x at 32): none is claimed until a
  fair SQLite build is measured (G2's harness).
- **Against every competitor, 32 to 1,000,000 writers** (`bench/records/2026-10-20-fedora-matrix.jsonl`,
  `python3 bench/table.py`; fedora, one run each, load 1.4–66 with other work on the machine, so not
  yet the quiet best of 3 CP-T1.2 asks). Amphora's best (one group at 32 writers, 12 above) against
  PostgreSQL 17: 1.2× the writes at 32 writers, 3.6× at 1,000, 5.8× at 10,000, 3.9× at 100,000,
  7.4× at 1,000,000; p99 1.1×, 2.5×, 1.9×, 1.6×, 7.6×; memory 8.9× at 32 writers, 1.3–2.6× with
  12 groups. Against OrioleDB much the same; against MySQL, Vitess, ClickHouse, vanilla Turso and
  SQLite 3.4–44× the writes at 32 writers and 18–1,128× from 1,000 up. What bounds it: at low
  concurrency the disk's flush (2.8 ms on fedora's btrfs, however the log is written); at high, the
  log's volume (whole pages, about 7 KiB a 200-byte event at 10,000 writers, 1.7 GB/s, as much
  again copied into the file). CedarDB refused the schema's `COLLATE "C"` (the bench now leaves it
  out for CedarDB); MySQL's server exited at 1,000,000 writers and the bench waited on it.

## Next

- **Replication measured: the harnesses** (G12 steps 1 and 3, CP-T1.8; lane repl-measure; `bench/src/repl.rs`,
  `bench/servers/repl/`): built and unit-tested, not run at size. `amphora-bench --repl commit|failover|catchup
  --system amphora|rqlite|dqlite|litefs|libsql` gives commit latency p50/p99/p99.9 and throughput, the time from a
  leader kill to the first acknowledged write with the acknowledged events lost, and a member's time to catch up,
  every size and duration an input. Amphora's three members run in one process (a kill is a cut from the relay);
  the competitors run as three-node container clusters through their scripts (dqlite and LiteFS through the bench's
  Go shim). libSQL has no failover and dqlite no member-local read, so those are refused, typed, as is LiteFS's
  failover until its script runs a Consul lease (lane repl-measure-2: it does now; `bench/repl.sh RECORDS FLAGS...`
  runs every measure on every system, and all four clusters came up on jupiter). Closes with: the comparison set and Amphora's classes measured on
  adacachy, fedora and jupiter, best of 3, and Amphora's members across machines for the LAN p99.
- **Follower reads at a closed time and search over partitions** (G13 step 4; replication.md, "Follower reads at a
  closed time" and "Search over partitions"; lane follower-reads): built and unit-tested, not measured. Every proposed
  entry carries its proposer's clock; every member applies it at the larger of that and one past the time before it,
  so times rise along the log whoever leads. `Database::query_closed` answers at the replica's closed time once it is
  at least the time asked; `follower_read_micros` is now less the replica's measured p99 lag. `amphora::search` asks
  partitions at once and merges newest first. Closes with: AC-T1.10.3 and AC-T1.10.4 measured (`amphora-bench
  --cluster`), an idle group's time closed on demand, and the attached client's request on the wire.
- **Query speed step 6 (QT.6), started** (lane qs6; docs/design/executor.md section 8): built and unit-tested, not
  measured. `vdbe/morsel.rs` runs a scan with its filter and aggregates, and a hash join's
  probe side, in morsels on several threads behind `ParallelMode` (off by default); the worker count and morsel size
  come from costs measured where it runs, answers equal at every worker count (T-QT.6.1 to T-QT.6.5).
  `vdbe/morsel_scan.rs` (lane qs6-2) reads a table b-tree in morsels of leaf pages: the leaves come from its interior
  pages, each worker opens its own cursor over its leaves through the statement's pager (one snapshot), and the scan
  pipeline's filter and aggregates, or a hash join's probe, run over the rows; `choose_degree` sizes the morsels in
  pages. Answers equal the VDBE's serial SQL answers at every worker count (T-QT.6.7 to T-QT.6.9). The VDBE's hook:
  `Connection::set_parallel_mode` (off by default) lets a single-table scan with its filter and aggregates run as
  `Insn::MorselAggregate` ahead of its serial loop, which answers when the run declines a value (`translate/parallel.rs`;
  T-QT.6.10). Lane qs6-3: a two-table inner hash join with aggregates goes through the same hook, the build side read
  and hashed once, each worker probing over its own leaves of the probe table with its own cursor and folding the
  joined rows into its partial aggregates (T-QT.6.11). Closes with: a join's own rows and joins of more than two
  tables through the hook, expressions beyond column-against-literal terms (a program's own loop body per worker), a shared hash build, loom or shuttle on the dispatcher, worker
  and morsel metrics, and AC-QT.6.1 to AC-QT.6.4 measured.
- **PL.5's Bloom filters sized and chosen by cost** (lane qs5-9): a hash join's filter is sized when its build ends
  from the distinct keys it measured, at a false-positive rate derived from the learned build and probe weights, and
  chosen only where it costs less than the probes it saves; its hashes and bits stay within the join's memory budget
  (`docs/design/planner.md`, "Decision (lane qs5-9)"). LIP and predicate transfer, a measured filter-lookup weight,
  removing `rhs_has_selective_seek` and deriving `cpu_cost_per_where_step` are left.
- **Query speed step 5 (QT.5), started** (lane qs5): `Connection::set_join_methods` plans a connection's new
  statements by cost, with index nested loops only, or with hash joins only; `tests/join_methods.rs` (T-QT.5.1) runs
  inner, left, semi and anti joins with NULL and duplicate keys all three ways and against a model. The harness
  `bench/join_methods` times both methods at 10^2 to 10^7 rows (the check on the join costs, the coordinator's run).
  Left: the hash join's and the seek's costs and every hash, group and sort budget learned where the engine runs
  (calibrated at start and from observed operator times). A budget taken from the connection's cache was built
  but not merged: with today's join costs it flips `records_plans`' one-row-lookup case to a hash join and a sort
  (the two plans cost about 110 each), so it waits for the learned costs, then the `rhs_has_selective_seek` gate removed, builds over joined inputs, PL.5's Bloom
  filters with LIP and predicate transfer, and sort-merge and prefetched index joins (G6 step 4).
  Lane qs5-6: the planner no longer prices pages its cache holds as file reads. A tree's first reads are held at the
  share of the connection's recent fetches that found their page cached (`Pager::page_miss_ratio`, over one to two
  cache turnovers) times the part of the tree the cache has room for; reads past what the cache can keep are charged
  whatever (`cost::file_reads`). `index_bonus` (a picked half page) is removed. `records_plans`' one-row-lookup join
  now picks the scan and index searches it measured faster (1.75 ms against the hash join's 3.05 ms, jupiter, release).
  Lane qs5-7: an automatic index, and a subquery's probe index, past its own cache (the default page cache its pager
  gets) pays for the leaves it writes out and reads again, as a hash join's spill already paid; a search of an index
  bigger than the cache keeps its inner pages held while they fit. Before, the automatic index cost CPU only and,
  against a measured page read, beat a unique key's search and a hash join on tables the cache can't hold.
  `cache_reuse_factor` (a picked 0.2) is removed: a repeated whole read pays for what the cache can't keep, a
  repeated search for its path below the root; multi-index fetches go through `file_reads`. Seeks, hash builds and
  hash probes time sampled runs into the learned rates, one run in k with k set by each one's spread against the
  timing's own cost (step 3a). Left: SQLite's skip-scan factor 1.375 (Ada's call), the row visit's own observed time.
- **Query speed step 7 (QT.7), started** (lane qs7; also G5 step 2): behind `Connection::set_vectorized_scans`
  (off by default), a read-only statement's scanned tables decode each leaf once into column batches
  (`vdbe/leaf_batch.rs`, executor.md decision 9): one header parse a row, only the columns the statement reads, the
  window sized from the measured L2 cache and the statement's memory budget, each row charged its own bytes; an
  overflow row or one over the budget is read row-at-a-time. Filters and aggregates read the batch through the
  VDBE's own evaluator. COUNT(*) over a table already reads only the leaves' cell counts (`Count`).
  `vdbe/leaf_batch/tests.rs` (T-QT.7.1 to .3) checks switch on against off on random tables. `amphora-bench
  --planner` runs it as the target `amphora-vectorized`. Lane qs7-3: a scan loop whose body only filters and
  steps COUNT, SUM, TOTAL, AVG, MIN or MAX runs over the whole batch by selection vectors
  (`vdbe/leaf_batch/fused.rs`, executor.md decision 10), through the opcodes' own comparison and aggregate code
  (T-QT.7.4, .5). Left: AC-QT.7.1 to .3 measured (the coordinator's); GROUP BY bodies still run row-at-a-time
  from the batch (a batch group lookup is the next piece); compiled pipelines if G5 measures them better;
  Windows's cache size, which needs an OS call this crate makes only with unsafe code.
- **G6 step 5, plans that read less, started** (lane g6s5): an `EXISTS` may now run as a hash semi join, chosen by
  cost against the nested loop's early out: the outer rows build the hash table, the subquery's table probes it, and
  `HashMarkFirstMatch` passes each build row on at its first match in a probe pass only (its matched bit, kept in
  grace partitions when the table spills). `tests/hash_semi_join.rs` runs EXISTS queries with NULL and duplicate
  keys under all three join methods, in memory and spilled, against a model. Sorted row fetches are a prototype behind
  `Connection::set_sorted_fetches` (off by default): the chosen read of a non-covering index runs as a one-branch
  multi-index read, its rowids sorted in a RowSet and the rows fetched in rowid order, kept when the existing
  intersection cost says it is cheaper; `tests/sorted_fetch.rs` runs reads with it off and on. That cost counts the
  fetch as the share of table pages, which assumes page locality: it should be learned before the switch goes on
  by default. Index-only reads were already planned (covering indexes, expression indexes included). Left: the 2x
  measurements on order_by_sort, exists_subquery and left_join_anti, and the sorted fetch's measurement against the
  plain read (the coordinator's).
- **G6 step 3, learned plan hints, built as a prototype** (lane g6s3; planner.md, decision of lane g6s3): Bao's
  hint sets over the planner (`optimizer/plan_hints.rs`), ten on/off masks of hash joins, nested loops, automatic
  indexes (`Connection::set_automatic_indexes`, new) and sorted fetches, applied with `HintSet::apply`. A Bayesian
  linear model over sixteen plan features, its precisions learned from the runs, chooses a training run's mask by
  Thompson sampling; a guard keeps a query class on the cost model's plan until a mask's held-out runs beat it, and
  drops it when they show it slower. Memory is fixed for the model and bounded by the caller's class count for the
  guard. `tests/plan_hints.rs` runs every mask's answers against the unhinted plan's; `bench/plan_hints` trains and
  checks held-out queries, answers against SQLite's. Left: held-out JOB and corpus times (AC-G6.3.1, the
  coordinator's), and hints in the engine's own prepare path (today the caller applies them).
- **The replicated class** (docs/design/replication.md, "Fast acknowledgement"; Ada's word 2026-11-01): built on
  t1/replicated-line, not on main (fast-ack, class-metrics, class-quorum and disk-stalls, merged in that order). A
  replicated database opened with `Durability::Replicated(Fates)` answers a write once members spanning the fates
  asked hold it (hyper-raft R-8), before its leader flushes its own log. One member's power cut loses nothing
  acknowledged: the nemesis cuts and restarts a member at the end of each round of writes. Its metrics (flush
  backlog, flush seconds, commits by quorum; replication.md, "Metrics") are exported by `ReplicaHandle::counters`;
  their core count is upstream in hyper-raft (`d5d5f8f`, vendored). Also on the line:
  - the suspicion feed: the host's hyper-liveness verdicts, told through `ReplicaHandle::verdict`, leave a suspected
    member out of held quorums until trusted (vendor patch 0005, offered upstream on Ada's word);
  - the owner's identity naming its durability (transport version 4), so an attachment in another class is refused,
    typed;
  - a write made at a follower answered on its group's commit, before its own log write, kept until it applies;
  - read sets on top of the write slot (goal ceea4e0a; replication.md, "read sets"): the pager records a session's
    reads, which ride after its write (append-only); the leader moves a stale base to its last entry when nothing its
    log holds since wrote a page the write read or wrote, page 1's size merging. Release A/B against G: the coordinator's;
  - the leader's quorum patience (replication.md, "The leader's quorum patience"): on fedora's own load its own
    checks stepped members down 5-8 times in 1,800 s against 43-63 at fixed ticks (docs/research/owner-stalls.md).

  Left: the host running hyper-liveness and telling its verdicts; the class stated once for a group across its
  nodes; the quorum patience's two metrics exported (names agreed); a member's fate carried by its change; the terms
  that still move on a leader's own stalls (owner-stalls.md).
- **What the language matrix found** (`bench/corpus/matrix.md`): the IN operator's collation is
  fixed (the oracles' cause E); left, from a failing test, `index_xinfo`'s NULL; then the missing
  features by how often users reach them (ORDER BY in aggregates, median and percentiles, the JSON
  functions, the PRAGMAs), each against the probe that found it.
- **What the oracles left open** (`bench/oracles/findings.md`): no cause of Amphora's. Their last
  runs (step 5 of plan 01a16eab7f457673b4cf9933adacfc4d) find only SQLite 3.54's LIKE optimization
  dropping an outer join's rows under SQLITE_LIKE_DOESNT_MATCH_BLOBS (cause AH), where the system's
  SQLite answers as Amphora does.
- **One thread making everything durable**: on btrfs every flush is a commit of the filesystem's
  log, one at a time, so the checkpointer's flushes of the file hold up the flusher's, and with it
  every write (docs/design/store.md, decision 4: commits stalled up to 97 ms, writes at 512 writers
  lost a third of their rate). The checkpoint now copies only frames the flusher made durable and
  flushes no log of its own (decision 15); a pass still flushes the file, 10.9–12.2 ms of each at 32
  writers. Left: the file's flush goes to the flusher's thread, between its flushes of the log,
  before the log starts again. It changes when Turso's checkpoint makes what it copied durable;
  T-T1.1.1's crash injection proves it.
- **CP-T1.1**: kiln vendors Amphora and its conversation store runs on `amphora::store` in place of its one writer thread; the soak bench for drops (AC-T1.1.5) and the stalled-disk test (T-T1.1.2); the bar measured on a quiet machine, best of 3 (AC-T1.1.2–4), with vanilla Turso, SQLite and Vitess beside Amphora (`bench/`).
- **CP-T1.2**: Baselines on a quiet machine against SQLite, Postgres 17, Vitess, CockroachDB and TiDB; the research; the client protocol; `docs/design/` written to it.
- **CP-T1.3**: many writers on one database: the storage of docs/design/engine.md (in-place versions with undo in one shared buffer pool, B-link trees, row-level redo at commit, serializable by precision locking), built beside the current engine and the default once the suite says so. Turso's MVCC was measured as the way there and set aside: on the whole suite, reads 0.64 times the store's at the median, point reads at 32 clients 0.2 times, 4.5 GB resident against 1.3 (fedora, 2026-10-22, checks).
- **The planner** (docs/research/planner.md; plan 01a16eab7f457673b4cf9933adacfc4d). Today it orders joins
  left-deep, by a dynamic program over its connected subsets, a size at a time on a worklist, while
  they number 10,000 or fewer, and past that by a greedy search that adds the table of fewest joined
  rows (decision 19, built on t1/planner-search; JOB's measurement against its parent waits on the
  batch's bench); it estimates from `sqlite_stat1`'s index prefixes and, for an equality or IN list of
  constants, from each column's distinct count and each table's sample that ANALYZE keeps beside it
  (decision 20, built on t1/planner-stats; its JOB and TPC-H measurement waits on the batch's bench); a
  table's other filters run together on its sample, and combine by exponential backoff where it has
  none (decision 21, the same branch and wait); one estimate per joined set whatever the order
  (decision 13); it weighs a hash join only where the table joined has no seek, so JOB and TPC-H run
  index nested loops; it plans at prepare, and again after ANALYZE; CERT
  and the order check are clean (step 5). Step 3's baseline (`bench/records/2026-10-26-jupiter-
  planner-baseline.md`) has every JOB and TPC-H query's estimates against the rows counted, and their
  run times beside SQLite, DuckDB and PostgreSQL. What is open, each closed by its goal in kiln, in
  this order: Amphora's query time down an order of magnitude (goal 7788ef29: JOB's median best from
  784.2 to 78.4 ms, TPC-H SF 10's from 23,287.8 to 2,328.8 ms; it plans the execution the planner
  can't give: the page-miss path, per-row execution, hash joins where a seek exists, parallel
  execution); then the ranked changes of section 9, measured as `docs/requirements.toml`'s PL.1 to
  PL.13 say: join sizes from equivalence classes (PL.1, goal b4c804c3), distinct counts and a sample
  per table (PL.2, 3492da3a: built and tested, its measurement open), the join search above 12 tables (PL.3, f692c137: built and tested, its JOB measurement open), each table's filters
  estimated together (PL.4, 210578d2: built and tested, its measurement open), Bloom filters to a star join's fact table (PL.5, 96c74342), no
  index-less nested loop beneath an unbounded outer (PL.6, 5e23ff8b), the cost model against time
  (PL.7, e0e7a1df), bushy plans with CD-C (PL.8, c837cde3), statistics kept as rows change (PL.9,
  26302efe), unnesting by dependent joins (PL.10, 61c79bd1), upper bounds as a guard (PL.11,
  f4e0bd7f), custom then generic plans (PL.12, aa0541f6) and adaptive joins (PL.13, 4f237e07).
- **PL.11** (f4e0bd7f, built and tested on t1/lane-pl11, its JOB and TPC-H counts open): ANALYZE
  keeps each index prefix's degree sequence as its largest degree and sum of squared degrees, and
  each joined set's estimate is cut to the upper bound they prove (one table's rows times each
  attached table's largest degree, or two tables' ℓ2 norms); underestimates are left as they are
  (docs/design/planner.md, "Decision (lane pl11)"). Open: the bound holds for PL.9's net row
  change, not for updates or cancelled inserts between ANALYZE runs.
- **PL.7** (b6a2313e, built and tested on main, behind a switch, its JOB measurement
  open): with `Connection::set_fitted_costs` (or `AMPHORA_FITTED_COSTS=1`) on, the database times a
  share of its statements, the share learned against the timer's cost and the measurement's spread,
  and fits the planner's weights to their counted work (rows, seeks, hash rows added and looked up,
  sort work, file page reads) by non-negative least squares with Huber weights, in bounded memory;
  weights no sample determines, or none tells from nothing at 95%, keep the learned calibration's
  (docs/design/planner.md, "Decision (lane pl7)"; lane pl7b, T-PL.7.6, after a loaded run's noisy
  weights flipped a plan in `tests/planner.rs`). Lane pl7c (T-PL.7.1): ANALYZE counts each table's and index's
  B-tree pages and the planner reads rows a page from them, else from the page size and the
  sample's measured records, else from the format's smallest record; the picked 50 rows a page is
  gone (docs/design/planner.md, "Decision (lane pl7c)"). Open: the audit's 09-cost-model run, the P-error harness (AC-PL.7.2), and
  AC-PL.7.1's JOB rank correlation with the switch on and off.
- **PL.6** (5e23ff8b, built and tested on t1/lane-pl6, its measurement open): no table is read whole
  beneath loops that can give more than one row. A scan stays only beneath one-row loops (a lookup by a
  unique key) or where no equality joins the table; elsewhere a seek, an automatic index or a hash join
  reads it, and a hash
  join's build input is materialized where its probe would otherwise be read once for each outer row
  (docs/design/planner.md, "Decision (lane pl6)"). `bench/planner/loops.py` counts such plans in the
  bench's records: 30 of JOB's 113 at step 3, none of TPC-H's; the target is 0, with JOB's times within
  the passes' spread of the parent's.
  PL.12 is built (lane pl12, docs/design/planner.md "custom plans, then a generic one"): a statement
  whose estimates read a parameter's value is planned with each run's values until its own measured
  costs say the generic plan does as well; one whose estimates read none keeps its generic plan. Its
  TPC-H prepared measurement (AC-PL.12.1; its harness, `amphora-bench --planner --prepared` with
  qgen's parameters, is written: bench/planner/README.md) and the suite's (AC-PL.12.2) are open.
  PL.13 is started (lane pl13, docs/design/planner.md "Decision (lane pl13)"): its two run-time
  checks are built as pure cores with their tests (`optimizer/adaptive.rs`): a nested loop that turns
  into a hash join at the learned costs' break-even, refused over its memory budget, and a
  materialization point that asks for a new plan only past its estimate's own error. The executor's
  shell that drives them, and JOB's 20 slowest queries against the parent (AC-PL.13.1), are open.
  PL.8 is started (docs/design/planner.md "Decision (lane pl8)"): a RIGHT JOIN led by one
  table is planned as its mirrored LEFT JOIN, CD-C's commutativity rule, so the tables after it are no
  longer held behind its right table and the audit's 03-right-full-barrier reads tiny first (T-PL.8.1);
  an outer join's ON term that reads only earlier tables now counts in its join's estimate.
  Lane pl8-3 (docs/design/planner.md "Decision (lane pl8-3)"): a LEFT JOIN's right table follows only
  the tables its ON terms read (CD-C's left-asscom rule), answers held against SQLite (T-PL.8.2), and
  `JoinN` carries a bushy shape (`inner_joins`) that left-deep plans leave empty.
  Lane pl8-4 (docs/design/planner.md "Decision (lane pl8-4)"): a hash join's build side can be a
  run of tables joined among themselves while the loops before the probe stay loops; the emitter
  runs such bushy plans and refuses other shapes with a typed error.
  Not built: the search over connected complement pairs (DPccp) that would choose bushy plans, and
  semi and anti joins placed before other loops (the emitter's no-match path); JOB's AC-PL.8.1 is open.
  PL.9 is built (lane pl9, docs/design/planner.md "row counts kept as rows change"): each table's
  rows inserted less deleted since ANALYZE are kept as writes finish (one atomic add a statement),
  the planner's row count follows them, and a prepared statement plans again once a table it counted
  has moved past its statistics' own error. CH-benCHmark (AC-PL.9.1) and the suite's writes
  (AC-PL.9.2) are open.
- **G11, analytics at column-store speed** (docs/design/columnar.md, decision 1; sources in
  docs/research/columnar.md; CS.1): decided, not built. Columns are a copy derived from the rows, fed
  by each commit's redo record after the commit, exact at every snapshot by a journal of changed keys
  read from the rows, never logged, every size and time learned where it runs. Next: step 2
  prototypes it in memory and as persistent blocks, on CP-T1.3's storage once that runs under SQL.
  Step 2 (CS.2), built, not measured: `crates/engine/storage/versioned/columns.rs` keeps a tree's
  copy fed by the versioned storage's commits (`Database::keep_columns`), with prototype A (`Plain`,
  in memory) and B (`Packed`, front-coded blocks and positional deltas in a file, reused at open when
  current) behind one core; a scan at any snapshot equals the rows there under concurrent writes and
  a full queue (T-CS.2.1 to T-CS.2.3). Block size and rebuilds follow measured scan and rebuild cost.
  Open: the numbers (`amphora-bench --columns`, T-CS.2.4) and the choice they make.
  Step 3's two codecs are built (CS.3, crates/columnar, lane g11-compress): BtrBlocks' cascade
  (one value, run lengths, dictionary, decimal doubles, frame of reference with horizontal packing)
  and FastLanes' layouts (its interleaved 1,024-value packing, lane-wise DELTA, dictionary,
  decimal), each scheme chosen per column from a sample that grows until its choice stops changing,
  the cascade a loop with no fixed depth; integers, doubles, strings and nulls round-trip bit for
  bit (T-CS.3.1). Its decode benchmark (`cargo bench -p amphora-columnar --bench decode`) is not yet
  measured against [c18]'s 8 values a cycle.
  Step 4 (CS.4, lane g11s4), built, not measured: `columns/scan.rs` scans the copy granule by
  granule, a granule being one of its blocks decoded into column vectors, so its size is the block
  size the sizer learns, now charged per row of the table so skipped granules count. Each carries
  min/max and null counts per column and its key range and position; filters skip granules their
  summaries exclude and run on whole vectors, as do COUNT, SUM (exact for reals), MIN, MAX and
  group-by (each row given its group's slot in one pass, each aggregate's vector folded by slot,
  T-CS.4.5). A key range (a wide-range statement) skips blocks outside it undecoded, by the blocks'
  key bounds, and filters its boundary granules by their key column. Blocks no change touches are
  decoded once a generation. Every class equals the row store's answer, whole-table and over key
  ranges, with writes between scans, serially and on the morsel executor's workers (`Plan` is a
  `vdbe/morsel.rs` pipeline, granules its unit) (T-CS.4.1 to T-CS.4.3). Open: the planner's use
  (step 5), and the benches against CedarDB and ClickHouse (AC-CS.4.1, AC-CS.4.2).
  Step 5's choice is built, behind `AMPHORA_COLUMN_PATH=on` (CS.5, lane g11s5): the planner prices
  "scan the column copy" from the copy's measured nanoseconds per row at its block size plus each
  queued key read from the rows and merged (`translate/optimizer/column_path.rs`), against the
  row path's learned rates for a row's visit and a seek; a copy that can't answer the snapshot
  exactly is no path. `storage/versioned/columns_path.rs` reads by the choice and charges the
  copy's scan time back to it (T-CS.5.1 to T-CS.5.4). Aggregates run on the copy through one
  hook, `CopyScan`: step 4's vectorized scan as main has it (`Vectorized`, granules and `Plan`),
  which a later scan replaces by implementing the trait, the choice unchanged; on the rows row by
  row, with the same answer (T-CS.5.5, T-CS.5.6). Open: SQL tables don't keep a copy yet, so no
  SQL statement reaches it; the classes against CedarDB and ClickHouse (AC-CS.5.2) are the
  coordinator's.
- **G5 step 4, skipping rows** (docs/design/executor.md decision 11; AC-G5.4.1, T-G5.4.1; lanes
  g5s4, g5s4b): built, not measured, not wired. `crates/engine/skipping` holds zone maps over leaf
  runs and database cracking behind one switch, each kept from the write path through one hook;
  zone maps are built from the table's own leaves (`zone_map_of_table`), one zone a leaf. With
  writes between scans each names exactly what a plain scan finds, in the model and on a real table
  through SQL (T-G5.4.1). Open: the scan and write costs (`crates/engine/tests/skipping_bench.rs`,
  through the engine), the choice they make, and the VDBE's scan and write opcodes calling the
  switch.

## Open

Everything else the track's plan names (kiln plan `01a157fdefba746a94a5fcd920a80d5a`): each goal, its criteria and how each is
measured are in `docs/requirements.toml`.
