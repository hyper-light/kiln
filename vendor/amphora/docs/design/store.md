# The store's commit pipeline and readers (CP-T1.1)

`crates/amphora/src/store.rs`, `crates/amphora/src/db.rs`. Its decisions, each with its sources,
its evidence and what would falsify it (CLAUDE.md §5). The evidence is kiln's K0 workload (1,000
sessions of 100 events, then 30 s of 32 writers appending one event per durable write and 4 readers
reading a session's latest 50 events), run on fedora (32 cores, Samsung SSD 9100 PRO 8 TB under
btrfs with zstd) through kiln's conversation store, which these files take from. Those runs were
at load 8–35 with other builds running: CP-T1.2 measures them again on a quiet machine, best of 3,
into `bench/`.

## 1. A group commit with no timer

**Decision.** One writer thread owns the engine and commits as soon as nothing more waits in its
queue (or a commit reaches its row bound); what came while it committed shares the next commit.
Nothing waits for a timer. (The flusher may wait, bounded by a flush's own time, before it begins a
flush: decision 14.)

**Sources.** Group commit in DeWitt et al., "Implementation Techniques for Main Memory Database
Systems" (SIGMOD 1984); Helland et al., "Group Commit Timers and High Volume Transaction Systems"
(HPTS 1987), on why a fixed timer adds latency without throughput once commits arrive faster than
they complete.

**Evidence.** kiln's store committed every 50 ms and its writes' syncs each forced a commit of their
own: 195 durable writes/s at p50 159 ms (2026-10-19). With the timer gone and a sync joining the
commit that covers it: 3,613 writes/s at p50 7.3 ms (2026-10-20).

**Falsified by.** A workload where a short timer gives more throughput at no more latency than
committing on an empty queue.

## 2. Durable beside the writer: a flusher

**Decision.** A commit writes the log and doesn't flush it (Turso's `NORMAL`); a flusher thread
flushes the log (`fdatasync`) over and over, and a write is durable, and acknowledged by
`Store::sync`, once a flush that began after its commit ends. A checkpoint copies only what a flush
has made durable (decision 15) and flushes the file after (Turso's checkpoint in `NORMAL`), so
nothing the flusher made durable is lost to a checkpoint. This replaces `SyncMode::Full` (a flush inside every
commit), which the design's README names until CP-T1.8: the guarantee to whoever is told a write is
durable is the same; the flush leaves the commit, so commits no longer wait for the disk one by one.

**Sources.** The flush-pipelining argument in Johnson et al., "Aether: A Scalable Approach to
Logging" (VLDB 2010): commits acknowledge once the log is durable, without holding the log's
writer through the flush. `fdatasync(2)` and `fsync(2)` (Linux man-pages): a flush makes durable
what was written to the file before it began, through any descriptor of the file. Turso's commit
path (`crates/engine/storage/pager.rs`, `CommitState::WaitSync`): it flushes only in `FULL`.

**Evidence.** On fedora's disk a durable 4 KiB write takes 2.6–3.3 ms (`dd oflag=dsync`; no
copy-on-write and direct I/O with `O_DSYNC` measured no faster), the floor of any durable write
there. `bench/disk.py` (2026-10-20, btrfs with zstd on the Samsung 9100 PRO): one thread, 2.72 ms a
write, appended or in place, flushed or written through; two to sixteen threads, on one file or a
file each, 4.9–5.0 ms a write at 200 writes a second a thread. btrfs commits its log one commit at a
time, so a flush that arrives during another's waits for it and then for its own: under
concurrency a durable write there costs about two of the disk's flushes, whoever asks. With the flusher, the writer is busy about a third of the time and a write's p50 is
5.4–6.1 ms, under two of that disk's flushes. Turso's `OFF` mode was measured and refused: a
checkpoint under `OFF` never records what it copied as safe, so the log never starts again and every
checkpoint copies all of it again (130 writes/s).

The flusher flushes through the database's own IO (`Db::flusher`), so a simulator's disk sees every
flush the store counts on.

**Falsified by.** An acknowledged write missing after a crash or power cut (T-T1.1.1: crash
injection at every write, 10,000 seeds, `crates/amphora/tests/crash.rs`; CP-T1.4's power-cut
harness). 2026-10-20: 10,000 seeds, none lost; with the flusher made to acknowledge without
flushing, 157 of 200 seeds lost acknowledged batches, so the test sees a loss when there is one.

## 3. Readers of their own, on the caller's thread, waiting only for their key's writes

**Decision.** A read takes a connection from a pool (opened as reads want them, up to a bound) and
runs on its caller's thread: no thread between a read and its answer. It waits until the writes of
its key handed over before it began are committed, counted per key in shards (handed and committed
counts, each entry numbered so a reader can tell when its entry was settled and replaced), and no
longer: not for other keys' writes, and not behind the writer.

**Sources.** Snapshot reads on a write-ahead log: SQLite's WAL (sqlite.org/wal.html) and Turso's
(`crates/engine/storage/wal.rs`, `begin_read_tx`): a reader sees what was committed when its
read began, and never blocks the writer.

**Evidence.** kiln's store ran every read on its writer thread: reads p50 159 ms under K0's writes
(2026-10-19). On connections of their own: history reads p50 0.11–0.15 ms, p99 0.9–1.0 ms. With no
writes, Amphora's reads (p50 0.050 ms, p99 0.081 ms) beat vanilla Turso's (0.055, 0.091) on the
same reads; with readers on sessions no writer writes (`--disjoint`), p50 0.057 ms against vanilla
Turso's 0.068 while writing 14 times as many rows. What reads of written sessions pay beyond that
is waiting for their own session's commit and reading the pages it changed.

**Falsified by.** A read that misses a write of its key handed over before it began (the store's
tests), or a read that waits for another key's write.

## 4. No checkpoint in a commit's way

**Decision.** The writer never checkpoints (`wal_autocheckpoint = 0`). A checkpointer with a
connection of its own copies the log into the file every `Config::checkpoint_every` (25 ms) without
waiting for anyone (passive). A log only starts again once nothing reads from it, which readers that
never pause prevent: so once the log holds more than `Config::log_frames` (131,072 frames, 512 MiB
of 4 KiB pages), the checkpointer copies without pausing until less than a 64th of the bound is left,
and the writer then starts the log again between commits, holding the readers and the
checkpointer back for the moment it takes (what little is left, and a flush of the file). The
checkpointer starts no pass while a restart is pending. At twice its bound the log is started
again however much is left.

**Sources.** Checkpoint starvation under continuous readers: sqlite.org/wal.html §"Avoiding
Excessively Large WAL Files". Turso's checkpoint modes and the writer's restart
(`crates/engine/storage/wal.rs`, `CheckpointMode`, `try_restart_log_before_write`).

**Evidence.** Checkpoints inline on the writer: write p99 23 ms, p99.9 28 ms. Off the writer with
passive checkpoints alone: write p99 18 ms, but the log grew past 2 GB in 30 s, and past 700 MB with
restarts asked of a reader-busy log. With the writer starting it again and the readers held back:
the log under ~90 MB, write p99 18 ms; with the checkpointer every 25 ms instead of 250, write p99
11–12 ms and p99.9 25–26 ms (the restart has little left to copy).

Copying until little is left before a restart (2026-10-20, fedora, 512 writers): the longest restart
went from 69–150 ms to 30–34 ms. The checkpointer starting no pass while a restart waits: the
writer's longest wait for the gate went from 57–61 ms to under 0.5 ms.

What checkpoints still cost (2026-10-20, fedora, 4,000 sessions, 20 s, load 3–7, one run each):
with no checkpoints at all (the log left to grow) 512 writers write 38,178 a second at p99 21 ms,
against 25,500–25,900 at p99 86–89 ms with them; 32 writers write 5,600 at p99 7.9 ms against
4,900–5,100 at 10.7–12.7. Checkpointing every second with a 262,144-frame log takes back most of
that (512 writers: 33,200–36,400 a second, p99 41–47 ms) but raises the longest write at 32 writers
from about 40–90 ms to 97–127 ms, and keeps 10 MB more of log index in memory. The cause, measured
by sampling where the writer and the flusher wait in the kernel (`bench/wchan.py`): a checkpoint's
flush of the file, and of the log before it, are btrfs log commits; the flusher's flush of the log
waits behind them (`wait_log_commit`, once 118 ms), and the writer's next append to the log waits for
the flusher's hold on the log's inode (`btrfs_inode_lock`), so a commit stalls (Turso's COMMIT, up to
97 ms). Starting the file's writeback while the checkpoint copies (`sync_file_range`, as
PostgreSQL's `checkpoint_flush_after` does) didn't shorten the stalls: the copy comes in one burst.
The checkpoints stay at 25 ms until they stop competing with the flusher for the disk's log
(STATUS.md: one thread making everything durable).

The bound, alone (2026-10-20, fedora, 32 writers, one group, checkpoints every 25 ms, three runs
each, load 4–9.5; `bench/records/2026-10-20-fedora-32-writers-log-bound-*.jsonl`): each restart
holds the writer 10–14 ms, and a commit at 32 writers writes about 73 frames, so a 16,384-frame log
restarted 27 times in 20 s: p99.9 12.0–15.6 ms, max 27–37 ms. At 131,072 frames it restarted 3–4
times: p99.9 9.1–10.6 ms, max 20–29 ms, 1–2 MB more memory. With no checkpoint at all: p99.9
8.8–11.8 ms, max 12–45 ms (the longest is the disk's: no checkpoint ran). So the default is now
131,072. A log keeps up to twice that many frames of disk (decision 13 keeps its length), and a
restart copies up to a 64th of it: at 1,000 writers and more, the matrix's restarts are to be read
against the 16,384-frame runs before.

## 5. Page budgets by what a connection does

**Decision.** The writer keeps 1,024 pages of the database in memory, each reader 2,000 (8 MiB,
Turso's default for every connection), the checkpointer next to nothing. A reader's miss is a read of
the file through the system's page cache; the writer keeps its indexes' inner pages. Readers kept 128
until decision 18: a reader that cleared its cache whole at every commit gained nothing from a larger
one under writes.

**Sources.** Turso's page cache (`crates/engine/storage/page_cache.rs`): one per connection, 2,000
pages by default, kept consistent with the log by the connection itself.

**Evidence.** Amphora's bench, fedora, 2026-10-20, K0's load, before decision 18: at 2,000 pages a
connection, 78–86 MB resident and reads p99 0.74 ms; at 1,024/128, 47–49 MB and reads p99 0.27 ms.
With decision 18 and 19 (commit f597f5a), 2026-10-21, K0's load (32 writers, 4 readers, 20 s), three runs
each in turn (`bench/records/2026-10-21-fedora-k0-reader-pages-{128,2000}.jsonl`): at 128 pages
reads p99 0.128–0.144 ms, 1.21–1.22 million reads, writes p99 6.5–7.0 ms, 47–51 MB resident; at
2,000 pages reads p99 0.122–0.138 ms, 1.24–1.27 million reads, writes p99 6.3–7.4 ms, 80–90 MB.
The comprehensive suite, each statement on its own, 200,000 rows
(`bench/records/2026-10-21-fedora-suite.jsonl` and `-suite-amphora-2000-pages.jsonl`): at 32
clients, 2,000 pages read `order_by_index` at 1,049,700 a second against 172,600 at 128,
`filter_is_null` 74,100 against 10,500, `range_pk_100` 394,700 against 139,200 and `table_count`
196,300 against 3,162; at 1 client the gains are smaller (`order_by_index` 142,000 against 63,100,
`point_secondary` 319,600 against 271,500). The suite's peak resident set at 32 readers: 494 MB
against 248 MB. Loads before the runs 1.3–6.4 (the K0 runs' own load included).

**Falsified by.** Reads under writes slower with the larger budget; a working set that fits in it
read slower than from the system's cache. Its memory, 8 MiB a reader, is the case for one cache all
readers share.

## 6. One flusher

**Decision.** One flusher thread (`Config::flushers`), though the store can run more.

**Evidence.** With two or four flushers a commit landing during a flush starts one of its own; on
fedora's disk the flushes serialize anyway: at 300 writes/s the write p50 stayed at 6.6–6.7 ms with
one, two and four (2026-10-20), and four once held a write 1.1 s.

**Falsified by.** A disk whose flushes overlap (a write's latency then falls toward one flush).

## 7. The writer keeps no statement journal; a commit that fails is retried batch by batch

**Decision.** The writer's connection keeps no statement journal (Turso's
`Connection::set_statement_journals(false)`, Amphora's change to its engine): a write statement in
a transaction that fails, or is abandoned unfinished, poisons the transaction, and COMMIT then rolls
it back whole and says so. `Db::transaction` rolls back on any failure, and after a COMMIT that
fails, so nothing a failed statement half-wrote is ever committed, and the next transaction can
begin. A commit that fails is tried again batch by batch, each batch its own transaction: the
batches that commit alone are kept, and one that can't is refused to its sender
(`Refused::Failed`), nothing of it written.

**Sources.** SQLite's statement journal and when it is skipped (`usesStmtJournal = isMultiWrite &&
mayAbort`, `vdbe.c`, `sqlite3VdbeHalt`), which Turso mirrors
(`crates/engine/translate/stmt_journal.rs`): an UPSERT whose DO UPDATE can fail is both, so every
event and session written keeps one, a 4 KiB copy of each page it first touches.

**Evidence.** Amphora's bench (2026-10-20, fedora, 128 writers): the statement journal was 13.6% of
the writer's time applying rows; without it, the writer's µs per row went from 30.6 to 25.4 and
writes per second from 13,580 to 14,405. The store's tests: a batch with a row that fails a CHECK is
refused and writes nothing, the batches committed with it are kept and made durable; a transaction
holding a failed statement is never committed, whether its failure was returned or ignored.

**Falsified by.** A row of a failed batch found after its sender was refused, a row of a kept batch
missing, or a statement's half-write committed (the store's tests; T-T1.1.1's crash injection).

## 8. Durable by ticket

**Decision.** `Store::put` and `Store::put_wait` return a `Ticket`; `Store::durable(ticket)` waits
until that batch's commit is followed by a flush that began after it, and refuses it if its commit
failed or the writer stopped before committing it. `Store::sync` still waits for everything handed
over so far, without saying which batches failed.

**Sources.** Group commit with per-transaction acknowledgement (DeWitt et al., 1984; Helland et
al., 1987): each committer learns its own outcome, not the group's.

**Evidence.** Before tickets, a batch whose commit failed was let go and `Store::sync` reported it
durable: a silent drop, against AC-T1.1.1. The store's tests cover a ticket made durable only after
a flush that began after its commit, and a failed batch's ticket refused.

**Falsified by.** A ticket reported durable whose batch isn't in the database after a crash
(T-T1.1.1), or a failed batch's ticket reported durable.

## 9. A failed flush is the last

**Decision.** When a flush fails, the store makes nothing more durable: the flusher stops, every
sender waiting for a batch to be durable is refused (`Refused::Disk`), and new batches are refused,
until the store is started again on what its disk holds.

**Sources.** Rebello et al., "Can Applications Recover from fsync Failures?" (USENIX ATC 2020):
after a failed `fsync`, Linux file systems may mark the pages they couldn't write clean, so a later
`fsync` succeeds without them. PostgreSQL stops on a failed `fsync` since 2018 for this
(`data_sync_retry`, off by default).

**Evidence.** The store retried a failed flush every 10 ms: a retry that then succeeded would have
acknowledged writes the disk never took. The crash test cuts a disk's power mid-flush: the senders
waiting are refused at once, where before they waited out their deadline.

**Falsified by.** A file system that keeps failed pages dirty and reports a later flush truthfully,
on every platform Amphora runs on (then a bounded retry could be safe).

## Crash injection (T-T1.1.1)

`crates/amphora/tests/crash.rs` runs the store on a simulated disk whose power goes at a change
drawn from each seed's workload (writes, flushes and truncations to the database file and the log).
A flush takes time (0.2 ms) and makes durable what was written before it began; each change not
made durable survives whole, torn at a 512-byte boundary, or not at all, in any combination. The
workload hands over 150 batches of 1 to 4 rows, a tenth of them holding a row the engine refuses, a
few at a time so they share commits, with up to three left in flight so commits land while flushes
run, the log checkpointed every millisecond and restarted past 16 frames. After the restart on what survived:
every batch acknowledged durable is there, every batch the engine refused isn't, every other batch is
there whole or not at all, and Turso's integrity check passes. Each run checks it isn't vacuous: the
power goes out midway in most seeds, acknowledged and refused batches are checked, and checkpoints
and restarts happened. Seeds run side by side, each on a disk of its own; one that panics fails
alone. Every gate runs 200 seeds; the full 10,000 run with
`cargo test --release --test crash -- --ignored` (4.7 s on fedora's 32 cores). Before flushes took
time, a flush followed each commit at once, so no checkpoint ever met an unflushed commit, and a
flusher claiming the whole log durable passed 10,000 seeds; it now fails them (decision 15).

## 10. Groups: writes on as many cores

**Decision.** A store can run in groups (`crates/amphora/src/groups.rs`): stores on one node, each
a database file with a writer, a log, a flusher, a checkpointer and readers of its own, keys spread
over them by FNV-1a, so writes go on in parallel. A batch is one group's: a batch with rows in more
than one group is refused (`Refused::Groups`); keeping operations across groups atomic is
CP-T1.10's. A read of a key goes to its group; a read of every key to each group. A database's
number of groups is its own for good. The commit's row bound comes down to 512 (`Config::commit_rows`).

**Sources.** Partitioned single-writer engines: H-Store (Kallman et al., "H-Store: a
High-Performance, Distributed Main Memory Transaction Processing System", VLDB 2008) and VoltDB, one
thread a partition, no latches between them.

**Evidence.** One writer was the ceiling: at 1,000 to 1,000,000 writers it wrote 21,000-23,000 rows a
second, its thread busy throughout (fedora, 2026-10-20). With commits of 512 rows instead of 2,048 (a
commit's pages then fit its writer's cache): 35,000-41,000. In groups, the files without copy-on-write
or compression (`chattr +C`; on btrfs with both, a checkpoint's flush held every log's for 2-4 s),
10,000 writers, one run each: 1 group 34,918 a second (p99 401 ms), 4 groups 105,377 (223 ms), 8
groups 161,789 (207 ms), 12 groups 254,430 (110 ms). At 1,000,000 writers 12 groups wrote 221,417 a
second and 16 groups 320,451 (p50 3.08 s, max 3.82 s: a million writers ÷ the rate), none failed and
none unanswered, the machine's 32 cores busy (load 48). Groups cost at low concurrency on btrfs: at
32 writers 8 groups wrote 3,640 a second at p50 8.7 ms where one group wrote 5,249 at 5.7 ms, eight
logs flushed at once costing two of btrfs's log commits each.

**Falsified by.** A key whose rows are found in another group than its own, a batch spanning groups
written (`crates/amphora/tests/groups.rs`), or a node where groups write no faster than one writer.

## 11. Told when durable

**Decision.** `Store::put_then` hands a batch over with what to tell once it is durable (or why it
won't be). The writer files each committed batch with a sender to tell by its commit, and a notifier
thread tells them once a flush covers their commit; refusals come the same way. No sender's code runs
on the flusher, and a client needs no thread waiting per write: the bench drives a million writers
from 16 threads.

**Evidence.** The store's test: a sender told only after a flush begun after its commit, and a
failing batch's sender told it failed. The bench at 1,000,000 writers on 16 submitter threads, every
write answered.

**Falsified by.** A sender told durable before a flush covering its commit ended, or told twice, or
never.

## 12. Written in place, every page checksummed

**Decision.** On Linux, a new database's file and log are made with btrfs's no-copy-on-write flag
before anything is written to them (`db.rs`, `in_place`; best effort: a file that exists keeps its
way, a file system without the flag declines it). The flag turns off btrfs's own data checksums for
those files, and its compression; Turso's page checksums (its `checksum` feature: an XXH3-64 of each
4 KiB page in the page's last 8 bytes, written with the page and checked whenever the page is read
from the file) take their place, so a page corrupted on disk is refused, never read as data.

**Sources.** btrfs(5), `nodatacow` and the `C` attribute: no copy-on-write, no compression, no data
checksums, for files written in place such as databases and disk images (as systemd-journald does
for its journals on btrfs). Turso's `storage/checksum.rs`.

**Evidence.** Amphora's bench (2026-10-20, fedora, btrfs with zstd, 10,000 writers, commits of 512
rows), the same runs in a copy-on-write directory and one with the flag: 1 group 25,051 against
28,774 rows a second, longest write 1.16 s against 0.57 s; 12 groups 97,150 against 194,941. The
store's tests: a new database's file and log carry the flag where the file system takes it; a byte
flipped in a page of the file is refused with a checksum error when read. The checksums' own cost
is to be measured on a quiet machine.

Crash injection found what checksums cost recovery: Turso reads page 1 from the file to open a
database before attaching its log, and after a power cut during a checkpoint that page can be torn
in the file while the log holds it whole: with checksums 3-12 of 10,000 seeds couldn't reopen
(without them the torn page was read, its header's bytes whole). Opening now checks page checksums
from when the log is attached (Turso, `Pager::defer_checksum`), when what it read before is dropped:
10,000 seeds twice, none failing (2026-10-20).

**Falsified by.** A page corrupted on disk read as data (`tests/files.rs`), a database that a power
cut leaves unopenable (`tests/crash.rs`), or, on btrfs, the flag making writes slower or their tails
longer than copy-on-write.

## 13. The log flushed by its data, and kept at its length

**Decision.** The flusher flushes the log's data (`FileSyncType::DataSync`, added to Turso:
`fdatasync` on Linux and the BSDs, `F_FULLFSYNC` on Apple's systems, whose `fsync` leaves the disk's
cache, `FlushFileBuffers` on Windows). And the log keeps its length when it starts again: the frames
from before the restart stay, to be overwritten in place, as SQLite's are. Turso cut the file back
to its header whenever it wrote a new one, so a log grew by appending after every restart; it now
does that only under its multi-process authority, which checks its snapshot against the file's
length and which Amphora doesn't use. What makes the frames left behind safe is unchanged: the new
header changes the salts and is synced before any frame follows it (`prepare_wal_finish`), and a
recovery stops at the first frame whose salts or checksum don't hold.

**Sources.** fdatasync(2): metadata not needed to read the data back (times) is not flushed.
SQLite's `os_unix.c` (`unixSync` uses `fdatasync` where the system has it) and `wal.c` (a restart's
header is synced before frames follow, ticket ff5be73dee; the log is cut only to
`journal_size_limit`). PostgreSQL's `wal_sync_method` (`fdatasync` by default on Linux), with
segments written once and recycled (`wal_init_zero`, `wal_recycle`), so a commit's flush finds no
new extents or length.

**Evidence.** On fedora's disk (Samsung 9100 PRO, btrfs with zstd) the change is neutral: a
durable write costs the disk's flush however it is made (`bench/disk.py`, decision 2; again
2026-10-20 with 16 KiB writes, load 3: p50 2.74–3.56 ms appended or overwritten, written first
or only allocated, `fsync` or `fdatasync`, with copy-on-write or without). It is the case file
systems that journal metadata reward (ext4, XFS: an `fsync` after an append commits the journal, an
`fdatasync` after an overwrite doesn't), to be measured on such a disk. Crash injection
(`tests/crash.rs`, 2026-10-20): 10,000 power cuts with the frames kept, none losing an acknowledged
batch; with the new header not synced (a mutant), 3 of 10,000 lost acknowledged batches, so the
test reaches frames left from before a restart.

**Falsified by.** An acknowledged batch lost to a power cut (`tests/crash.rs`); on ext4 or XFS, the
log's `fdatasync` no faster than its `fsync`.

## 14. Gathering before a flush at low load

**Decision.** When the writer ran out of work during a flush, the flusher doesn't begin the next
flush at the first commit after it. It waits until the writer has been idle for a 32nd of a flush,
or half a flush has gone by, whichever comes first (a flush's time smoothed: the first, then an
eighth of each new one), and then flushes everything committed. When the writer never ran out of
work, flushes follow one another as before.

**Why.** Writers that each wait for their write to be durable send their next write when told. A
flush begun at the first commit after the last flush covers the first few of those writes and
leaves the rest to the flush after; the writers settle into two halves, each committing during the
other's flush, and each write waits two flushes. At 32 writers on a 2.8 ms flush: 5.6 ms a write
and 5,700 writes a second, predicted; 5.1–5.8 ms and 5,134–5,608 a second, measured.

**Sources.** Helland et al. (HPTS 1987; decision 1) on group commit timers: a fixed delay adds
latency once commits arrive faster than they complete, which is why this one is taken only when the
writer ran out of work, and is bounded by the flush's own time. PostgreSQL's `commit_delay` and
`commit_siblings`: a delay before the flush when other transactions are active.

**Evidence.** fedora, 2026-10-20, 32 writers, one group, three runs each alternating with
PostgreSQL 17 in the same windows, at load 4–12 with other work on the machine
(`bench/records/2026-10-20-fedora-32-writers-flush-at-first-commit.jsonl`,
`...-gathering.jsonl`). Flushing at the first commit: 5,134–5,608 writes a second, p50 5.12–5.76 ms,
p99 9.3–9.7 ms, p99.9 14.6–20.7 ms, max 26–54 ms. Gathering: 7,390–7,605 writes a second, p50
4.06–4.16 ms, p99 7.2–8.1 ms, p99.9 11.3–17.2 ms, max 21–38 ms, with about 5,000 flushes a run
against 7,000. PostgreSQL: 5,803–6,164 writes a second, p50 5.06–5.44 ms, p99 7.4–8.1 ms, p99.9
9.5–20.2 ms, max 10–29 ms. Crash injection: 10,000 power cuts, none lost. The two fractions are
chosen, not yet tuned: a 16th and a 64th of a flush are to be measured against the 32nd on a quiet
machine, and gathering at 1,000 writers and more against flushing at once.

**Falsified by.** A workload where gathering lengthens a write's p50 or tails against flushing at
the first commit (an open-loop trickle pays up to a 32nd of a flush a write), or more writers where
flushes stop following one another.

## 15. The checkpoint copies what the flusher made durable

**Decision.** Before a flush the flusher captures the log's header-object identity, generation
(its header's `checkpoint_seq`) and last committed frame together with any completed-flush proof,
under one shared-log guard (`Database::wal_prefix`). If that exact prefix is already known durable,
it owes no additional log sync. Otherwise it syncs the log and publishes the captured prefix only
after success (`Database::wal_prefix_durable`); a stale or foreign token is ignored. The checkpointer
copies only up to the proved frame (`Database::wal_durable_frame`, a passive checkpoint's upper
bound), and its own flush is omitted only for a covered prefix (`WalCoordination::durable_through`).
With no mark yet, a checkpoint copies everything and flushes the log first, as before. No local
proof is accepted or reused under the multi-process authority, whose log other processes write.

A normal log restart clears its proof. Reusing the 32-bit generation after wrap also renews header
identity. External replacement always renews header identity and clears proof, even if restored
bytes reuse the same generation and frame numbers; a pre-replacement sync cannot then publish a
proof for the replacement. The old `wal_end`/`wal_durable` tuple API has no object identity: after
external replacement or generation wrap, its proof publications are conservatively ignored for
that database instance, so legacy callers may incur extra checkpoint syncs. Typed publication
continues to work. External file replacement itself requires writers, flushers and checkpoints to
remain quiescent until reload completes; proof invalidation does not make file replacement atomic.

**Sources.** Turso's checkpoint (`CheckpointState::SyncWal`): a frame copied into the file must
be durable in the log first, or a crash could keep the copy and lose the frame, leaving a file that
matches no committed prefix. fdatasync(2): a flush makes durable what was written before it began.
Within a generation a committed frame's slot is never written again (`frame_cache_high_water`).

**Evidence.** fedora, 2026-10-20, 32 writers, one group, three runs each of one binary with the
mark on and off, alternating, load 5.6–8.5 (`bench/records/2026-10-20-fedora-32-writers-mark-*.jsonl`):
a checkpoint pass took 10.9–12.2 ms against 12.6–14.1 ms (the log's flush gone; the file's flush
is most of what remains), and the checkpointer flushed the log 27 fewer times a second. Writes
didn't move beyond the noise: 7,116–8,095 a second against 7,096–7,877, p99 6.5–8.3 ms against
6.5–8.2. A test holds a flush and commits while it runs: the checkpoint after it leaves that commit
uncopied (`tests/crash.rs`, `a_commit_that_lands_during_a_flush_is_not_counted_durable_by_it`). It
fails with the flusher noting the log's end after its flush instead of before, and with a flusher
claiming the whole log durable, which also fails crash injection (integrity check and checksum
failures in 10,000 seeds); the real flusher passes both.

**Candidate validation (UNRUN).** AC-T1.4.4:
`restored_wal_requires_a_fresh_proof_before_checkpoint_can_omit_sync` replaces a WAL with different
contents at the same numeric prefix, attempts stale, foreign and legacy proof publications, and
requires a refused WAL sync to stop checkpoint before backfill. A fresh successful sync and typed
publication must then permit backfill without another WAL sync. This proposed regression and the
candidate changes have not been compiled or run; the historical measurements above establish no
latency claim for this optimization.

**Falsified by.** A required WAL sync omitted without a matching completed-flush proof, a checkpoint
that copies a frame no flush made durable (the tests above), or an acknowledged batch lost or torn
by a power cut during checkpoints (`tests/crash.rs`).

## 16. The log packed: a commit's pages compressed into records

**Decision.** A log can be packed (`Db::pack_log`, `Database::wal_pack`; the format in
`crates/engine/storage/packed.rs`). Amphora doesn't pack by default: on the one bench run so far it
cost writes at 32 writers (Evidence), so it waits on the comprehensive bench to show where it helps
before it is turned on. In a packed log a commit's pages, or a
spill's, become entries (decision 17 says which are whole images and which are deltas), cut into
records of up to 256 KiB, each record's entries compressed together with LZ4, or kept as they are
where that doesn't make them smaller, and laid into as many of the log's fixed-size frame slots as
it takes. A record's first slot carries its slot count where a frame carries its page number, the
slots after it a continuation mark, and its last slot the commit's database size when it ends a
commit; the slots' checksums chain as frames' do, and every page of a record maps to its first
slot, so frame numbers keep their meaning for read marks, backfill, the durable mark (decision 15)
and restarts. The log header's file format says which a generation is, and a generation keeps the
format it began with, so a log is always read as it was written and a page-a-frame log from before
reads as ever. Records are kept decoded (4 MiB a database, the oldest dropped first past that; the
writer keeps those it writes), the checkpoint reads a record once for all the pages it copies from
it, and recovery reads record by record: a record whose slots don't all hold ends the log there, as
a torn frame does; one whose slots all hold but that doesn't decode refuses the log, since no torn
write leaves that. The raw frame interface (Turso's sync's) and the multi-process authority refuse
a packed log.

**Why.** At 10,000 writers in 12 groups a 200-byte row cost about 1.8 page frames (7 KiB) of log,
about 1.7 GB/s, and the checkpoint copies besides: the disk's write bandwidth bounded the writes.
Those pages compress: in a sample of 30,000 frames of that run, 123.6 MB of whole pages took 41.2
MB compressed (3.0x, zlib at level 1 standing in for LZ4).

**Sources.** The LZ4 block format (`lz4_Block_format.md`, lz4/lz4 on GitHub): a match reaches at
most 65,535 bytes back, so records much larger than 64 KiB compress little better while a read
decodes all of one. PostgreSQL's `wal_compression` (its documentation, "Write Ahead Log",
`runtime-config-wal`): the full-page images in its log compressed with pglz, LZ4 or zstd, for the
same reason. SQLite's WAL format (sqlite.org/fileformat2.html, "The WAL File Format"): the frame
header, the checksum chain from the header's salts, and recovery stopping at the first frame that
doesn't hold, all of which a record's slots keep.

**Evidence.** The bench, fedora, 2026-10-20, 32 writers in one group, one run each at load 2.9–3.0
(`bench/records/2026-10-20-fedora-packed-log.jsonl`): page-a-frame 7,903 writes a second, p50 3.97
ms, p99 6.5 ms, p99.9 9.6 ms; packed and compressed whole (this decision alone) 6,216, 4.93, 8.8,
10.4; with deltas (decision 17) 5,887, 5.12, 9.0, 14.2. The process wrote 23.4, 18.6 and 18.2 KB a
row: most of it the checkpoint's copies into the file, which packing doesn't shrink. It used 0.58,
0.73 and 0.78 ms of CPU a row. A profile of the deltas' run (load 18) finds LZ4's compression 90%
of what preparing records costs the writer, yet the writer is on a CPU 3% of the run, while its
commits took 2.3 s longer in all over 20 s: most of the cost is a wait not yet found. At 10,000
writers only page-a-frame ran (234,071 writes a second, 21 KB written a row) before other work
loaded the machine. The engine's tests (`crates/engine/tests/packed_log.rs`): commits of
one page, of many records and of more than one batch (1,024 pages), compressed and stored records,
read back on a connection with nothing cached, after a checkpoint, in the next generation and after
recovery; a transaction spilled into the log and rolled back, then one committed; a generation
keeping its format across a change of the setting; a record cut short taking only its commit; and a
record whose checksums hold but that doesn't decode refusing the log. Crash injection
(`tests/crash.rs`, 10,000 power cuts, 2026-10-20): none lost an acknowledged batch on packed
records; with a commit's size on a record's first slot instead of its last (a mutant), 4,318 of
10,000 did, and with continuation slots' checksums unchecked in recovery, 60 left a log refused as
corrupt (so the cuts land inside records).

The comprehensive suite's writes, fedora, 2026-10-22, packed (with deltas) against page-a-frame,
one binary (4a60ada), each run started at load under 2, one round each (checks, not recorded runs):
at 32 clients `insert_batch_100` 392 → 44 statements a second (p99 172 → 705 ms),
`delete_range_insert` 2,800 → 1,011, `update_plain` 7,787 → 6,134, `upsert` 7,531 → 6,288,
`update_indexed`, `update_text` and `txn_read_write` 12% fewer; at one client `insert_batch_100`
159 → 96, the single-row writes as before. By the falsifier below it is not the default: packed,
a statement that writes hundreds of pages waits on something page-a-frame doesn't, the wait not yet
found above.

**Falsified by.** A row read back other than as written, or an acknowledged batch lost or torn by a
power cut (`tests/crash.rs`, every other seed packed; `crates/engine/tests/packed_log.rs`); as a
default, fewer writes a second or longer tails packed than page-a-frame on the comprehensive bench.

## 17. Only the bytes that changed: deltas in the packed log

**Decision.** In a packed log (opt-in, decision 16) a page whose last version is in the log's
current generation is written as the bytes that changed since (a delta: runs of offset, length and
bytes, runs closer than a run's header merged), at most 8 deltas in a row (`packed::MOST_DELTAS`)
before its next whole image, and only where the delta takes less than half a page; otherwise its
image. The base
is what the page's buffer held when the page was last made dirty: `Pager::add_dirty` keeps a copy
while the buffer holds a version as written to the log (`Page::logged`, set when a commit or a
spill is published), and the delta is made only while that version is still the page's latest
frame in the log's current generation. The page's checksum (its last 8 bytes, which its buffer
doesn't keep up to date) is carried in every delta. A read rebuilds a page from its chain, its
entry at the frame read back through its deltas (each the page's previous frame) to its image,
every record read once and kept decoded, and checks that the depths count down to the image; the
checkpoint reads the same way. Under a page codec (encryption) pages are written whole.

**Why.** In the sample of decision 16, 83% of the frames were later versions of pages already in
the log, changed by 311 bytes on average (p50 48, p90 326, p99 3,735). Deltas merged and compressed
took 9.7 MB where whole pages took 123.6 MB and compressed whole pages 41.2 MB (12.8x against
3.0x). Deltas against the database file's copy of a page's first version in a generation (10.1 MB)
gained nothing over compressing that version whole, so first versions stay whole, and no chain
reaches outside the log.

**Sources.** ARIES (Mohan, Haderle, Lindsay, Pirahesh and Schwarz, ACM TODS 17(1), 1992): redo
records describe the change made to a page, not the page. PostgreSQL's `full_page_writes` (its
documentation, `runtime-config-wal`): a page's first change after a checkpoint logs the whole page,
later ones only the change, the split used here at a generation's start.

**Evidence.** The engine's tests: a delta turns a page into its next version, the checksum's bytes
are carried whole even where the buffer's stale copy matches the new one (a test found that case:
a run merged with the carried tail had dropped its first byte, and the page's checksum refused the
rebuilt page), chains of up to 8 deltas rebuild every version, damaged records are refused, and
(`packed_log.rs`) rows changed commit after commit are logged mostly as deltas, at depths up to 8
and never past it, and read back on fresh connections, after a checkpoint and after recovery. A
model test there (random inserts, changes and deletes with savepoints rolled back amid spills, 8
seeds, both formats) found that a balance renumbers its siblings' page objects, so one could carry
another page's logged version: every page of a record shares its frame, so the check that the
base is the page's latest frame passed, and a delta was made against the wrong page. A renumbered
page now holds no logged version (`PageInner::set_id`); the test fails without it.

Crash injection (`tests/crash.rs`, 10,000 power cuts, 2026-10-20): none lost an acknowledged batch
with deltas. Mutants: the checksum's bytes not carried whole, 1,553 of 10,000 failed (checksum
mismatches on rebuilt pages); chains never ending in an image, 29 (a chain past what a read looks
back through); deltas applied newest first, 9,552. With the check that a base is the page's latest
frame removed, no test fails: no path found leaves a page holding a version that isn't its latest
(a savepoint's rollback restores pages as new objects, and a spilled page is journaled first), so
the check stays as a guard. The log's bytes a row and what reads pay for chains: to be measured
beside decision 16's.

**Falsified by.** A page read back other than as written; a chain longer than 8; an acknowledged
batch lost or torn by a power cut; writes or reads slower with deltas than with compressed whole
pages at any writer count.

## 18. A reader forgets only what was written since it last read

**Decision.** A connection starting a read transaction after others committed forgets only the
pages those commits wrote (`Pager::begin_read_tx`, `Wal::begin_read_tx` answering a `ReadChange`),
not its whole page cache. The log lists the pages its recent frames hold, in frame order, beside
its frame index and under its lock (`crates/engine/storage/changes.rs`: at most 65,536 entries, 1
MiB, the oldest half dropped past that). The list vouches only for frames it saw appended one after
another in the current generation: a reader whose last read was in another generation (the log
restarted), or before what the list holds (dropped for the bound, or appended past a gap, as frames
recovered or restored whole are), or behind more changed pages than its cache holds, forgets
everything, as before. A checkpoint's backfill alone changes no page's contents and forgets
nothing. A page a cursor still pins, or one being read, can't be dropped alone, so then the whole
cache is cleared. Under another process's authority (the shared-memory index) no list is kept, and
readers forget everything.

**Why.** SQLite's pager, and Turso's, cleared a connection's page cache whenever the database had
changed since its last read (the comment it came with: "we can probably do better with more
granular invalidation"). Under writes every read began cold, and a larger cache only cost more to
clear and fill: that is why decision 5's readers keep 128 pages. The comprehensive suite
(`bench --suite`) shows what a small cache costs reads. A reader caching 20,000 pages reads the
suite's `order_by_index` at 139,710 a second, against 45,822 at 2,000 pages and 43,783 at 128;
`point_secondary` 378,174, against 306,489 and 229,422 (fedora, 1 client, load 8: a check, not a
recorded run). With this decision a reader's cache outlives commits, so decision 5's budget can be
chosen for what reads need.

**Sources.** SQLite's pager (`src/pager.c`): `pagerBeginReadTransaction` resets the cache when the
log's read reports a change (`sqlite3WalBeginReadTransaction`), which this replaces. PostgreSQL's
shared buffers (its documentation, "Resource Consumption", `shared_buffers`): pages stay cached
across transactions, and a page changed is replaced alone.

**Evidence.** `crates/engine/storage/pager.rs`,
`a_reader_forgets_only_the_pages_written_since_its_last_read`: a reader caching a 400-row table
keeps every page but the one a one-row update wrote and reads the new row; it forgets everything,
another table's pages too, when a commit writes more pages than it caches, and after a checkpoint
that restarted the log.
`crates/engine/tests/reader_cache.rs`: 400 random steps of transactions (one row to hundreds, rows
growing into overflow pages and shrinking, inserts, deletes and rollbacks), checkpoints of every
mode and reads, 12 seeds, page-a-frame and packed logs, readers caching 200 pages (the least a cache
keeps) and 2,000: every
read is the model's. With a listed change forgotten not at all (a mutant) both fail, the model at
seed 0, step 4; with a range's last frame left out (a mutant), the model fails at seed 0, step 11.

**Falsified by.** A read returning a row as it was before a commit its read transaction started
after; reads under writes slower than when every commit cleared the cache.

## 19. The page cache evicts the oldest page not looked at again

**Decision.** A page new to a connection's cache joins at the head of its queue, and the clock
hand sweeps from the tail toward the head (`crates/engine/storage/page_cache.rs`), as SIEVE has
it: the hand evicts the first page it finds unmarked, taking a mark off each marked page it
passes, so a page new to the cache is examined after every older one. Turso's cache inserted a new
page just after the hand, where the sweep looks next.

**Why.** Inserted there, a page read once was the next the sweep examined: with the cache full,
each miss evicted the page the previous miss brought in, the pages cached before stayed, and a new
working set never settled unless each of its pages was looked at again before the next miss. The
suite found it: the same reader running `point_secondary` and then `order_by_index`, whose working
set is about 600 pages, read `order_by_index` at 49,044 a second with 2,000 pages cached, against
151,719 from an empty cache (fedora, 1 client, load 6: checks, not recorded runs). Readers live
across statements in a store, where Turso's bench target opens connections afresh.

**Sources.** Zhang, Yang, Yue, Vigfusson and Rashmi, "SIEVE is Simpler than LRU: an Efficient
Turn-Key Eviction Algorithm for Web Caches" (NSDI 2024): new objects are inserted at the head, and
the hand moves from the tail toward the head, evicting the first object not visited since it last
passed.

**Evidence.** `page_cache.rs`, `a_new_working_set_settles_in_a_full_cache`: a cache of 100 pages
full of pages read once, then a working set of 40 read round after round: every read after the
first round a hit; with the insertion after the hand, 360 misses in the 9 rounds after the first,
every read. Two of Turso's tests asserted the old order (the newest page evicted first) and now
assert SIEVE's. With it, the same reader reads `order_by_index` after `point_secondary` at 147,133
a second with 2,000 pages, and at 72,402 with 128 (45,128 before).

**Falsified by.** A working set that settles in the cache yet misses; a hot page evicted before a
page read once.

## 20. Pages one connection read in, every connection reads from memory

**Decision.** A database can share the pages its connections read (`Database::share_pages`,
`crates/engine/storage/shared_pages.rs`). A page a connection misses in its own cache is looked up
there before it is read: under the version it is (a frame of the log in the log's generation, or
the file's), its image as committed, kept by whichever connection read it in first. The images
never change: only a connection in a read transaction and holding no write lock shares, so no frame
of a write in progress is kept, and a file version is dropped before the file's page is written or
the file cut. A connection about to write a shared buffer copies it first (`Pager::add_dirty`, which
every write goes through before it touches a page: it keeps the page's contents for savepoints).
At most the pages it was made for are kept, in 64 shards by page number with a lock each, evicted
by CLOCK. Refused under another process's authority (its writes are unseen), with MVCC and with a
packed log. One pool can serve a process's databases, a budget for them all
(`Database::share_pages_in`, `Db::share_pages_in`): each database file that joins it gets a number
of its own, part of every key its pages take (`SharedFile`), so two databases' pages never meet, and
forgetting one's file versions leaves the others'. Not on by default (`Db::share_pages`, the bench's
`AMPHORA_SHARED_PAGES`, one pool for all of K0's groups).

**Why.** Each connection's cache is its own: 32 readers each missing a working set larger than its
cache read the same pages from the file 32 times, each a `pread`, a 4 KiB copy and a checksum, and
evicted for them. A profile of the suite's `order_by_sort` at 32 clients (2,000 pages a reader):
reading pages in 27% of the time and evicting 19%, the CPU a statement took 4.4 times what it took
at 1 client.

**Sources.** PostgreSQL's shared buffers (its documentation, "Resource Consumption",
`shared_buffers`) and InnoDB's buffer pool (MySQL's reference manual, "Buffer Pool"): one cache all
sessions read, a page read once for all. CLOCK (Corbató, "A Paging Experiment with the Multics
System", MIT Project MAC, 1968): a reference bit a page, cleared as the hand passes, the first page
found unreferenced evicted.

**Evidence.** `crates/engine/tests/shared_pages.rs`: a second reader reads every page a first read
in from memory, from the log and from the file; a writer that holds a shared page updates it, and
the other readers read the old row while the write is open and after it is rolled back; a reader
after a checkpoint that wrote the file reads every new row; a writer whose spilled frames a rolled
back write used again reads, and leaves, only what committed; 300 random steps for 16 seeds, four
readers, a writer of up to hundreds of rows a transaction, rollbacks, checkpoints of every mode:
every read the model's. With no copy before a write (a mutant) the writer's test and the model fail
(a reader saw the write in progress); with a file write forgetting nothing, the checkpoint's test
and the model fail (a row read from a version written over); with a writer sharing its own frames,
the rolled-back write's test fails (it read the rolled-back value). The suite, fedora, one client
and 32, 200,000 rows, 2,000 pages a reader, sharing 65,536 pages (256 MiB) against none (checks at
loads 6–30, not recorded runs): at 32 clients `order_by_sort` 7,664 a second against 3,183,
`group_by_having` 7,137 against 2,907, `count_distinct` 7,277 against 2,844, `join_group` 1,366
against 687, `exists_subquery` 10,562 against 5,185, `table_group_by` 79 against 36, the peak
resident set 410 MB against 414; at one client the same gains, 2.2–2.4 times, the resident set 249
MB against 51 (the shared pages filled). Sharing 16,384 pages (64 MiB), smaller than the working
set, gains half as much (`order_by_sort` 5,314 at 32 clients) for more memory at 32 clients (611
MB: buffers the shared pages evicted live on in readers' caches beside their next copies). K0's
load (32 writers, 4 readers, two runs each): without sharing 163,682 and 163,620 writes (p99 6.3
ms), reads 1.36 and 1.32 million (p50 0.054 ms, p99 0.12 ms), 82–84 MB; sharing 65,536 pages,
153,178 and 157,216 writes (p99 7.0 ms), reads 1.94 and 1.92 million (p50 0.037 ms, p99 0.082–0.086
ms), 339–341 MB; sharing 16,384, writes 154,464 and 154,744 (p99 6.8–8.2 ms), reads unchanged,
150 MB. The recorded suite at 39549dc, without sharing and with 256 MiB shared
(`bench/records/2026-10-21-fedora-suite-amphora-39549dc.jsonl`, `-shared-256mib.jsonl`, loads
1.7 and 3.4): the analytic reads gain as above, but reads whose pages each reader's cache mostly
holds ran slower shared, `point_pk` 263,000 a second against 426,000 at one client,
`point_pk_one_column` 561,000 against 1,064,000 and `join_three` 10,500 against 23,400 at 32. Run
alone for 5 s after a second's warmup, `point_pk` at one client is faster shared (567,847 and
665,306 a second against 421,239 and 458,198, no read from the file left in its profile): the
suite's slowdown is likely its first statements' 2.5 s right after the load, while the
checkpointer drains the log and restarts it (every frame version kept then is a key no reader asks
for again), to be confirmed. Confirmed: at f2105d2, the statements that read slower shared run
after a 3 s warmup, sharing 65,536 pages against none, in turns, best of 2 (fedora, load 2.8–2.9,
checks): `point_pk` 578,820 a second against 426,838 at one client (1,118,528 against 1,080,926 at
32), `point_pk_one_column` 667,829 against 449,580 (1,132,324 against 1,078,408),
`point_secondary` 429,388 against 322,838 (1,123,615 against 1,065,232), `join_three` 2,109
against 1,729 (31,325 against 24,631), `order_by_sort` 766 against 384 (8,704 against 3,798),
`exists_subquery` 947 against 484 (11,450 against 5,932), `table_count` at 32 clients 255,748
against 76,860 (at one, 20,837 and 29,699 against 4,732 and 38,656: it varies eightfold unshared);
the peak resident set 265–297 MB against 333 (32 readers' caches hold one copy of a page between
them, not one each). `crates/engine/tests/shared_pages.rs`: two databases whose tables have the
same pages, frame for frame, share one pool and each reads its own rows alone, from the log and
after a checkpoint of one, the second time from memory; with every file given the same number (a
mutant) one reads the other's rows. So every read measured is faster shared, at 32 readers for less
memory; what is left before it is on by default is the default budget, and readers' own caches
smaller while sharing, for the process with few readers.

**Falsified by.** A read of a version the reader's snapshot doesn't see; reads slower shared than
not; more memory shared than the budget and the readers' own caches together.

## 21. A page read records itself only when it must wait for the cache

**Decision.** A page read from the file goes straight into the connection's cache; only a read
whose page the cache can't take yet (full of pages it can't evict, a spill in flight) is recorded in
`pending_reads`, so the call that resumes it reuses its page and its read rather than reading again
(`Pager::read_page_into`). Every read used to record itself before the cache took it and remove the
record after: two write locks, a hash map insert and remove, and two reference counts, for each page
read from the file.

**Why.** A profile of the suite's `filter_no_index` (a whole scan of 200,000 rows, 1 client,
2,000 pages cached): reading pages from the file 60% of its time, and removing the read's record
alone 11.6%, nearly all of it the record's last reference counts, on lines cold by then.

**Sources.** SQLite's pager reads a page it misses into its cache and keeps no record of the read
(`getPageNormal`, `src/pager.c`): its reads are synchronous, so nothing resumes them. Amphora's
reads stay resumable, as they are when the cache must spill first; only that case needs the record.

**Evidence.** `storage/pager.rs`, `a_read_waiting_on_a_spill_is_recorded_and_reused`: a read whose
insert into the cache must wait (a test hook makes it) is recorded, and the next call reuses its
page and its read and clears the record; with the record left out on that path (a mutant) it fails.
`read_page_nonblock_reentry_reuses_pending_entry` still holds. A probe of the engine on fedora (a
200,000-row copy of the suite's table, release, 2,000 pages cached, best of 3 in turns against the
binary before, load 2.6–2.7; checks, not recorded runs): a whole scan counted 33.64 → 31.35 ms, a
scan to row 90,000 30.21 → 28.58 ms. With 65,536 pages shared they take 16.88 and 14.24 ms: a page
found shared costs about a fifth of one read from the file.

**Falsified by.** A read resumed after a spill that reads its page twice, or another's.

## 22. A shared page found in one lookup, and counted where it was found

**Decision.** A shard of the shared pages keeps each image in its map's entry, beside the bit CLOCK
reads, and its ring holds only the keys, in the order the hand passes them
(`storage/shared_pages.rs`): a page found is one lookup, where it was the slot's number from the map
and then the slot. The pool's counts of lookups found, lookups missed and images added are kept in
each shard, written under the lock the lookup holds anyway, and summed when asked; they were one
count for the pool, written by every reader's every lookup.

**Why.** A profile of the suite's `group_by_having` sharing pages (1 client): finding pages in the
pool 21% of its time, half of it the slot reached after the map's entry, a second line from memory.
And a line every reader writes at every lookup moves between their cores each time: point reads
gained 1.36 times shared at one client, 1.03–1.05 times at 32 (decision 20).

**Sources.** CLOCK as in decision 20. Intel's optimization reference manual on false sharing: a line
written by many cores moves between their caches at each write. Counters split by CPU or by shard
and summed when read are the usual remedy (the Linux kernel's `percpu_counter`).

**Evidence.** `storage/shared_pages.rs` and `crates/engine/tests/shared_pages.rs` pass unchanged:
each version found under its own key and the counts summed, eviction by CLOCK under the bound, two
files' pages kept apart, readers never reading a version their snapshot doesn't see. Release
binaries on fedora sharing 65,536 pages (200,000 items, best of 2 in turns, each run started at
load under 2; checks, not recorded runs): at 32 clients `order_by_sort` 8,566 → 11,660 statements a
second, `exists_subquery` 11,330 → 15,483, `group_by_having` 8,169 → 9,851, `table_count` 266,452
→ 274,682; point reads unchanged at the bench's own ceiling (about 1.13 million a second); at one
client within the runs' spread (each statement varied 10–25% between rounds of one binary, on a
machine with heavy I/O pressure from other work).

**Falsified by.** Counts that differ from one count for the pool; reads slower at 32 clients than
before, or at one client beyond the spread of runs of one binary.

## 23. A commit ends before its pages outgrow the writer's cache

**Decision.** The writer commits what waits for it in as many commits as its cache holds the pages
of: a commit takes whole batches, in order, and ends before the next once the pages written so far,
and as many again as the last batch wrote, would pass the point where the cache spills written pages
into the log (90% of it); the rest go into the next commit at once (`Engine::apply_batches`,
`Db::transaction_while_room`). The first batch always goes in, however large. Decision 10's row
bound stays.

**Why.** A profile of the suite's `insert_batch_100` at 32 clients (a hundred rows of `items`, which
has four secondary indexes, to a statement): about 24 statements a commit, each commit writing about
3,400 pages through a writer that keeps 1,024; 13% of the writer's time went to spilling written
pages into the log mid-commit (to make room for a page a split allocates) and 21% to reading pages
back. Decision 10's bound counts rows, which for an engine of SQL statements are statements of any
size: it was meant to keep a commit's pages in the writer's cache, and with statements of a hundred
rows it doesn't.

**Sources.** SQLite's pager spills dirty pages into the log mid-transaction when its cache is full
(`pagerStress`, `src/pager.c`; `PRAGMA cache_spill`), and its pages are read back when the
transaction needs them again; Amphora's engine spills at 90% of its cache
(`storage/page_cache.rs`). Group commit as in decision 1: what a commit takes is a trade between
its fixed cost, paid fewer times by larger commits, and the work it causes itself.

**Evidence.** `crates/amphora/tests/store.rs`: ten batches of about 45 pages each handed to a held
writer that keeps 200 pages are committed in more than one commit and fewer than ten, each batch
durable and whole; with Amphora's 1,024 pages, in one;
`a_transaction_over_batches_ends_before_one_its_cache_might_not_hold`: a transaction over such
batches takes more than one and fewer than ten and commits exactly those, and a first batch larger
than the cache goes in whole, alone. With the bound left out (a mutant), both fail. Before the
bound, release binaries of 4a60ada on fedora, `insert_batch_100`, 200,000 items, one round each,
each run started at load under 2 (checks, not recorded runs): at 32 clients 351 statements a second
(p99 184 ms, 100 MB resident at most); at two statements to a commit (`AMPHORA_COMMIT_ROWS=2`, a
stand-in for the bound) 526 (p99 100 ms, 57 MB); keeping 4,096 pages instead, 512 (p99 139 ms,
114 MB); 16,384 pages, 685 (p99 131 ms, 209 MB), the larger caches slower at one client (196, 159
and 125 a second). With the bound, against the binary before it, the suite's writes in turns
(one round each, the binary before twice): at 32 clients `insert_batch_100` 333 and 325 → 456
statements a second (p99 203 and 197 → 131 ms), `delete_range_insert` 2,841 and 2,764 → 3,136
(p99 32 and 36 → 19 ms), `update_indexed` 7,145 and 7,170 → 8,486, `insert_one` and
`txn_read_write` within the runs' spread; peak resident 102 MB → 59 MB, the heap's peak 61 → 21
MB. With the bound and 4,096 pages kept, `insert_batch_100` 528 at 97 MB resident; the writer
keeps decision 5's 1,024 pages.

**Falsified by.** A batch committed in parts, or lost; fewer statements a second or longer tails
with the bound than without on the suite's writes.

## 24. The checkpointer lists the pages of the frames it copies

**Decision.** A checkpoint finds the pages it copies, each at its latest frame in the checkpoint's
range, in the frames' own list (decision 18's, in frame order: `storage/changes.rs`), copied out
under the list's lock and sorted by page after it (`InProcessWalCoordination::iter_latest_frames`,
`changes::latest_by_page`). Only when the list doesn't hold the whole range (past its bound, after
a gap a recovery or a restore left) does the checkpoint scan every page the log holds, as before. A
record cut into by a rewind leaves the list as it leaves the frame cache (`cache_record`).

**Why.** The scan walked every page with a frame in the log, at every checkpoint, holding the frame
cache's spin lock: the lock every page read from the log takes (`find_frame`). Past its bound the
checkpointer copies without pausing (decision 4), and a log of 131,072 frames holds tens of
thousands of pages. In a profile of the suite's `insert_batch_100` at 32 clients the writer spent
5.4% of its time spinning on that lock in `find_frame`.

**Sources.** SQLite's checkpoint iterates only the frames past the last backfill, sorted by page
from its WAL index (`walIteratorInit`, `src/wal.c`): its work is the frames it copies, not the pages
the log holds.

**Evidence.** `storage/wal.rs`, `the_checkpointers_list_is_what_a_scan_of_every_page_finds`: 20
seeds of 400 random appends of frames and packed records, rewinds by frames and by records, rollbacks
and restarts, each followed by three ranges: every range's list is the scan's, more than 10,000 of
them answered by the frames' list; with the cut record left in the list (a mutant) it fails at the
first seed. `storage/changes.rs`, `lists_each_pages_latest_frame_in_a_range`. A checkpoint with
frames to copy now searches no page's frames
(`in_process_checkpoint_with_no_frames_to_backfill_compares_no_frames`).

**Falsified by.** A checkpoint that copies a page at other than its latest frame in its range, or
misses one; checkpoints that list their pages slower than the scan did.

## 25. A checkpointer that copied everything durable waits for a flush

**Decision.** Past its bound the checkpointer passes without pausing only while its passes copy
something. A pass that copies nothing new (it copies only what a flush made durable, decision 15)
makes the next wait until a flush makes more durable, or a checkpoint period passes, or the store
stops (`Sends::flushed_past`, on the condition variable the flusher already signals at every
flush).

**Why.** With decisions 23 and 24, a profile of the suite's `insert_batch_100` at 32 clients: the
checkpointer passed about 10,000 times a second (784,505 times in a 70-second run of five
statements), nearly every pass copying nothing, each taking the gate, the log's coordination locks
and its file's reference count, lines the writer takes at every commit and page read. The
checkpointer's samples rose to 923, a third of the writer's, 18% of them on the log file's reference
count alone; and with decision 24's spinning gone (0 samples, from 5.4% of the writer's time), the
writer wrote no faster (`insert_batch_100` 448 and 399 statements a second against 456–463).

**Sources.** Decision 15: nothing that isn't durable is copied, so until a flush ends a pass has
nothing new to copy. Decision 4: past its bound, passes without pause keep what a restart must
copy small; a pass that copies something is still followed by the next at once.

**Evidence.** `crates/amphora/tests/store.rs`,
`a_checkpointer_with_nothing_durable_to_copy_waits_for_a_flush`: with flushes held and the log
past its bound and short of twice it, at most ten passes in 500 ms (the test's checkpoint period
100 ms), every row durable and read back once the flushes go; with the wait left out (a mutant),
135,526 passes in those 500 ms.

**Falsified by.** A log that stays longer past its bound (its restarts later or fewer) or writes
slower than with passes without pause.

## 26. One pool of pages a process's databases can share, a quarter of its memory

**Decision.** A database opened with `Sharing::Process` (`Db::open_sharing`) joins the process's
pool of shared pages (decision 20): one pool for every database the process opens that way, made
when the first opens, holding at most a quarter of the memory the process may use, in 4 KiB pages
(`db::process_pages`, `db::default_pages`, `memory::limit`): on Linux the
smallest memory limit of the control groups it runs in (cgroup v2, else v1, each group up to the
root) and the machine's memory; where the system doesn't say (macOS, Windows: their interfaces need
`unsafe`, which the crate denies), 65,536 pages (256 MiB, decision 20's measured size); never under
4,096 pages. `Sharing::Pool` gives a database a pool of the caller's, `Sharing::Off` none (a packed
log, decision 16, needs `Off`). `Db::open` still shares nothing: whether it should share the
process's pool by default, and that budget, are the person's call (STATUS.md). A storage that refuses to
share (another process's authority over the file, MVCC, a packed log: one this process packs or,
new with this decision, one whose generation on disk is packed, which recovery reads as written
without anything asking for packing again) leaves each connection its own cache and says so
(`amphora.store` event `pages_unshared`). The pool only holds pages connections have read:
a cap, not a reservation. A database that closes takes its pages out of the pool
(`SharedFile`'s membership, `SharedPages::forget_all`), so a pool that outlives its databases keeps
none of theirs.

**Why.** At ten million rows (6.7 GB of pages) each reader's 2,000 pages hold neither the table's
leaves nor even its inner pages (about 5,500): a lookup by rowid misses twice, each miss a log
lookup, a `pread`, a 4 KiB copy, a checksum and an eviction, about 20 µs a lookup against SQLite's
~1 µs from a warm OS cache. One pool for the process, sized from its memory, is the budget a
process has to give it, where a pool a database each multiplies with the databases (K0 runs one a
group).

**Sources.** PostgreSQL's documentation ("Resource Consumption", `shared_buffers`): on a dedicated
server with 1 GB or more, "a reasonable starting value for `shared_buffers` is 25% of the memory in
your system" (its shipped default is 128 MB). CedarDB's configuration reference: its buffer pool
(`buffersize`) defaults to 45% of available memory. DuckDB's configuration reference:
`memory_limit` defaults to 80% of RAM. A quarter is the cautious end of what databases that size
their caches from memory take. Control groups v2 (the kernel's `Documentation/admin-guide/
cgroup-v2.rst`, `memory.max`): a group's limit bounds every group under it, so the smallest along
the path is the process's.

**Evidence.** `crates/amphora/src/memory.rs`' tests: a process finds its memory group in cgroup
v2, v1 and hybrid hierarchies; limits read as bytes, "max" and v1's no-limit value as none; every
group above a group bounds it; this process's limit is no larger than its machine.
`db.rs`, `the_process_pool_is_a_quarter_of_its_memory`: 32 GiB gives 2,097,152 pages, 16 MiB the
floor, none said the fallback, no overflow. `storage/shared_pages.rs`,
`a_file_that_leaves_the_pool_takes_its_pages_with_it`: a file's pages stay while a handle is left
and all go with the last, the other file's untouched; with the membership's drop left empty (a
mutant) it fails. `crates/engine/tests/packed_log.rs`,
`a_database_opened_on_a_packed_log_doesnt_share_its_pages`: reopened on a packed generation, the
database refuses to share and reads every row; with the check of the log's format on disk left
out (a mutant) it fails. `crates/amphora/tests/crash.rs`: the power-cut seeds that don't pack their
log now share the process's pages through every crash and recovery, and lose nothing acknowledged.
The suite at ten million rows (adacachy, 2026-10-23, loads 65 to 117, checks rather than recorded
runs; `ab-v4-10m.jsonl`), the process's pool (8 GiB in the bench's 32 GiB unit; 98.8% of 628
million lookups found their page) against none: `insert_one` 3,023 and 1,302 a second at 32
clients, `update_indexed` 3,182 and 1,921, `txn_read_write` 2,469 and 1,521, `exists_subquery`
8.6 and 0.6 at one client, `join_pk` 430,752 and 272,128 at 32 clients; slower where many readers
scan the same pages at once (`table_group_by` at 32 clients 50 s a query against 15 s,
`window_row_number` 7,300 a second against 13,977), and for the statements right after the load
(`point_pk` at one client 8,968 against 172,322: the pool and the OS's cache of 14 GB of database
and log no longer both fit the unit). With decisions 27 to 29 and planner decision 7 in, and the
suite's settle cutting the log so the OS's cache of the database stays warm (`ab-v6-10m.jsonl`):
no pool reads faster still (`case_aggregate` at 32 clients 43,339 a second against 21,751,
`join_pk` 547,680 against 165,541, `union_all` at one client 206,733 against 17,936 while the pool
fills), the pool ahead on writes at 32 clients (`update_indexed` 4,696 against 3,265,
`txn_read_write` 2,497 against 2,025). A pool beside a warm OS cache holds every page twice and pays
a lookup and an insert on every miss; it pays where the OS can't keep the pages.

**Falsified by.** A process sharing the pool whose reads run slower than with none once the pool
is warm (the scans above, until the pool's lookups stop contending); a process whose other memory
the pool crowds out (its memory pressure rising with the pool); a closed database's pages found in
the pool.

## 27. A writer reads committed pages through the pool, and keeps none of what it reads

**Decision.** A connection holding the write lock looks pages up in the shared pool like a reader,
but only versions committed before its write transaction began: the log's committed end is
recorded when the transaction begins (`WalFile::write_base`, after any restart it makes), and a
page whose latest frame for the writer lies past it (a frame the transaction itself wrote, spilled
from its cache) isn't looked up. What the writer reads on a miss it doesn't keep in the pool: it
reads the pages it is about to change, whose committed images its commit supersedes. Pages it
changes it copies first, as decision 20's writers did (`Pager::add_dirty`).

**Why.** Decision 20 kept the writer out of the pool while it held the write lock. At ten million
rows the writer's 1,024 pages (decision 5) hold none of six B-trees' leaves and few of their inner
pages, so each commit read pages from the file that readers had already brought into memory. The
first version of this decision let the writer keep what it read too: the suite's 10M load then
filled the 8 GiB pool with log versions that all died when the settle restarted the log, and with
the pool on the suite's reads ran slower than with none (pool off against on at 32 clients:
`date_bucket` 9.1x, `string_functions` 8x, `case_aggregate` 5.6x; at one client `string_functions`
3.4x; `ab-v3-10m.jsonl`). A pool hit itself costs less than a read from the OS cache (one query of
340 pages through a 200-page reader cache, one client: 0.34 ms from the pool, 0.6 ms without, with
reads up to 2.9 ms), so the loss was the pool's contents, not its lookups.

**Sources.** Decision 20's invariant: only committed images are kept. SQLite's WAL (its
documentation, "Write-Ahead Logging", and `wal.c`): a writer's snapshot is the latest commit plus
its own uncommitted frames, appended past the committed end; frames up to that end are committed.

**Evidence.** `crates/engine/tests/shared_pages.rs`:
`a_writer_in_a_transaction_reads_committed_pages_from_memory` (a writer caching 200 pages that
updates 200 rows on 100 leaves in a transaction finds at least 50 of its pages in the pool, from
the log and after a checkpoint from the file, while a reader reads the old rows until it commits);
`a_writer_keeps_nothing_it_reads_in_the_shared_pages` (with nothing read in, the writer's
transaction adds at most its schema read to the pool, a reader after it the table's pages); the
model (300 random steps for 16 seeds) and `a_write_rolled_back_leaves_no_shared_page` still pass.
Mutants: a writer keeping what it reads fails the second; one keeping even its own spilled frames
fails `a_write_rolled_back_leaves_no_shared_page` (a reader read a rolled-back value); one whose
transaction's base is 0 fails the first. With the process's pool at ten million rows, writes run as
decision 26's evidence gives (`insert_one` at 32 clients 3,023 a second against 1,302 with no pool,
`update_indexed` 3,182 against 1,921).

**Falsified by.** A reader or a writer reading a version its snapshot doesn't see (the model's
check), or a write workload running slower with the writer reading the pool.

## 28. A reader's snapshot takes the log's generation without a lock; spin locks test first

**Decision.** The log's header keeps a copy of its generation (`checkpoint_seq`) that reads take
without the header's lock (`wal::HeaderCell`): every guard of the header refreshes the copy as it
is dropped, so no write to the header leaves it stale, and every write to the header holds the
shared log's write lock, so a reader under its read lock sees the copy and the other snapshot fields
together, as it saw them under the header's lock. A reader's snapshot (`snapshot_of`, taken at
every transaction a reader begins, and again to check it) reads the copy. And the engine's spin
lock (`fast_lock::SpinLock`) tests before it takes: a waiter spins on a plain load and swaps only
once the lock looks free.

**Why.** A profile of the suite's `point_pk` at 32 clients, ten million rows (v3, 2026-10-23):
19.5% of the time beginning read transactions, 18% loading the snapshot, 17.6% in the header
lock's swap alone. Every point read is a read transaction, so 32 readers took one lock line from
each other at every query; the spin lock swapped in its loop, so each waiter wrote the line at
every turn.

**Sources.** Anderson, "The Performance of Spin Lock Alternatives for Shared-Memory
Multiprocessors" (IEEE TPDS 1, 1990): test-and-test-and-set spins in the cache and goes to the
interconnect only when the lock is released. SQLite's WAL index header (`wal.c`,
`walIndexTryHdr`): readers copy the header without a lock and check the copy.

**Evidence.** `storage/wal.rs`, `a_header_cells_generation_follows_every_write`: the copy is the
header's after a field write and after the whole header is replaced. `fast_lock.rs`'s test: 1,000
threads incrementing under the lock lose no increment. The suite at ten million rows with no pool
(adacachy, checks rather than recorded runs, loads near 100 and 80): `point_pk` at 32 clients
701,613 a second before (`ab-v3-10m.jsonl`) and 1,572,331 after (`ab-v4-10m.jsonl`),
`point_pk_one_column` 589,372 and 1,713,766.

**Falsified by.** A reader's snapshot naming a generation the log isn't in (the WAL tests, the
model, the crash injection), or point reads at 32 clients no faster.

## 29. A pool lookup reads under its shard's read lock and copies the page out

**Decision.** A lookup in the shared pages (`SharedPages::copy_into`) takes its shard's read lock,
copies the page's image into a buffer of the reader's own, and lets the lock go: readers of the
same shard proceed together, and no reader holds a count on a buffer every other reader of that
page shares. The mark CLOCK reads (read since the hand last passed) is an atomic each entry, set by
lookups only when not already set; the shard's hits and misses are atomics beside its lock.
Additions, evictions and forgetting take the shard's write lock. A page read on a miss still keeps
the buffer it publishes (one reader, one count).

**Why.** With decision 27, the process's pool at ten million rows hit 98.8% of 628 million lookups
and still lost to no pool where many readers scanned the same pages at once (`table_group_by` at 32
clients 50 s a query against 15 s, `window_row_number` 7,300 a second against 13,977): each hit took
its shard's mutex and a count on the page's shared buffer, and released both, so 32 readers in step
took the same two lines from each other at every page.

**Sources.** The kernel's page cache serves the same reads without a lock a page (`filemap_read`,
the page cache's xarray read under RCU): what the pool's lookups lose to. Corbató's CLOCK (decision
20): only the reference bit is written on a hit.

**Evidence.** `storage/shared_pages.rs`, `a_lookup_copies_the_image_out_and_holds_nothing`: a hit
copies the image's bytes and adds no holder of the pool's buffer; a buffer of another size gets
nothing. The suite at ten million rows with the process's pool (adacachy, 2026-10-23, checks
rather than recorded runs; `ab-v4-10m.jsonl` before, `ab-v5-10m.jsonl` after, 98.8% of lookups
found both times), 32 clients: `window_row_number` 7,300 → 15,146 a second, `string_functions`
134,293 → 214,902, `group_by_range` 1,545 → 2,340, `date_bucket` 1,530 → 2,470, `aggregates_range`
21,591 → 24,428; against no pool in the same run, the pool now ahead or level on those, and
behind on `case_aggregate` (26,274 against 36,065) and on `union_all` at one client (53,790
against 75,565).

**Falsified by.** Scans many readers make of the same pages at once still slower with the pool than
without, or single readers slower (a 4 KiB copy a hit costing more than the count it saves).

## Virtual filesystem registry ownership (AC-T1.1.1)

A synthetic file identity belongs to its virtual filesystem. Independent MemoryIO and MemoryYieldIO
instances can contain different bytes under the same path; their registry keys therefore combine
FileId with an opaque, retained Arc identity. Reopening through the same backend or a transparent
wrapper retains that scope and shares the existing Database. Native files keep the unscoped OS
identity, including hard-link sharing. FileId's public dev/ino representation is unchanged.
Within each memory backend, file creation assigns a monotonically increasing checked file ID
under the existing files mutex. Removing and recreating a path gets a fresh ID while old handles
retain the old bytes. Exhausting the counter returns a typed refusal; IDs never wrap or repeat.
A missing path has no file identity and returns NotFound.

A scoped registration is owned by its Database and removed in one key lookup when its final alias
closes. The cleanup compares the exact weak Database identity, so an old destructor cannot remove a
successor or an Opening reservation. Failed and cancelled opens keep the existing state-owned
reservation cleanup. Registry locks are released before validating or dropping an upgraded Database.
This prevents both an unbounded accumulation of dead virtual scopes and destructor lock reentrancy.

Evidence: the engine's implicit_rowid_seek suite opens independent MemoryIO backends at the same
path; the previous path-only keys reused one Database and three parallel cases returned Busy. The
parameterized ownership regression is a sequential witness: independent backends keep different
rows, aliases share writes, unlink/recreate keeps old readers separate from new bytes, and the
final alias releases its scoped entry. It covers both memory backends and their mixed use when
io_memory_yield is enabled. These behaviors, and the unchanged implicit-rowid suite, would falsify
this ownership model if they fail.

Sources: Rust's [`Arc::ptr_eq`](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.ptr_eq)
and [`Weak::ptr_eq`](https://doc.rust-lang.org/std/sync/struct.Weak.html#method.ptr_eq) specify
allocation identity. Unlike value equality, these preserve separate scopes with equal payloads;
the retained Arc and exact Weak owner keep those allocations alive until cleanup.

## 30. A transaction's spills write over the frames it already wrote

**Decision.** A page a write transaction already wrote into the log past its committed end is
written back over that frame when it spills again, or a cache flush writes it again, not appended
(`WalFile::append_frames_vectored`, `write_over`; frames next to each other in one write). The
frame keeps its page, so the frame cache, the list of changed pages (decision 18) and readers, who
see frames only up to the committed end, are untouched. The first frame written over is noted: its
checksum, and every later frame's, no longer chain. Before a commit prepares its frames,
`Wal::rechain` (the commit's `Rechain` step) reads the transaction's frames back from that frame to
its last, 1,024 at a time (as many as a spill writes at once, 4 MiB: what it holds in memory),
chains their checksums again from the frame before it (the log's header for frame 1, the last
commit's checksum, or the checksum that frame holds, never written over) and writes them back; the
commit's frames then chain on from them, as they always did. A savepoint rolled back past the first
frame written over forgets it. A raw frame (`write_frame_raw`, the sync engine's) after frames
written over is refused (`TxError`): it could commit them before they chain. A packed log (decision
16, opt-in) keeps appending its records, whose lengths follow their contents.

So the log a transaction writes holds at most a frame for each page it dirtied before its commit,
and at most two after it, whatever its statement count: the engine keeps a spilled page dirty, and
its commit writes every dirty page again, where SQLite's pager marks a spilled page clean and its
commit writes only what changed since (left as it was here: one frame a page is that change).

**Why.** The unify audit (kiln-unify, finding data-1, Amphora 542f5a9): one 8 MiB log-search
transaction (for each trigram a row into a table with two indexes and an upsert of its term's
count, in nested savepoints) left a 7,068.5 MiB log beside a 119.9 MiB file, and a 12 MiB one,
rolled back, 9,010.7 MiB. Each page changed again after it spilled was appended again: 1.8 million
4 KiB frames for 1,790,521 postings. A log that grows with a transaction's statements instead of its
pages costs every byte written, the disk it keeps (decision 13) and the checkpoints that copy it.

**Sources.** SQLite 3.54.0's `walFrames` (`src/wal.c`): "Check if this page has already been
written into the wal file by the current transaction. If so, overwrite the existing frame";
`iReCksum` ("On commit, recalculate checksums from here") and `walRewriteChecksums`, which reads the
frames back from the earliest written over and writes their headers again;
`sqlite3WalSavepointUndo` forgets `iReCksum` past the frame it rolls back to. Its pager's spill,
`pagerStress` (`src/pager.c`): a single frame for the page, the page then clean.
sqlite.org/fileformat2.html, "The Write-Ahead
Log": a frame is valid only if its checksum matches the one "computed consecutively on the first 24
bytes of the WAL header and the first 8 bytes and the content of all frames up to and including the
current frame", and a read takes "the last valid instance of page P that is followed by a commit
frame or is a commit frame itself"; sqlite.org/walformat.html: mxFrame "is set to the index of the
last valid commit frame in WAL". So frames past the last commit stand for nothing until a commit
frame follows them: a crash before the commit's frames, written over or chained again or not, keeps
the last commit.

**Evidence.** `crates/engine/tests/spilled_log.rs`,
`a_transactions_log_is_bounded_by_the_pages_it_dirtied_not_its_statements`: 60,000 statements of
kiln's log search on a 200-page cache, the log cut first, dirty 473 pages; on c87e332 the log held
1,892 frames past its last commit before the commit and 2,365 after (the test failed); with this
decision 462 and 935, every row read back on a fresh connection and after opening the database
again (its recovery reads the commit's chain whole, from the log's header), the integrity check
whole. The audit's legs, its corpus and SQL on a connection as Amphora's writer keeps one (1,024
pages; `the_audits_log_search_legs_log_within_their_pages`, release builds without LTO as the audit
built them, adacachy 2026-10-29 at load 46-64, sizes, not times): on c87e332 the 1, 2, 4 and 8 MiB
transactions dirtied 3,405, 7,390, 15,162 and 30,689 pages and left 22,554, 207,360, 688,238 and
1,792,166 frames past the last commit (88.6, 814.7, 2,704.2 and 7,041.7 MiB: the audit's 7,068.5
MiB at 8 MiB, growing faster than the pages); with this decision 6,780, 14,758, 30,307 and 61,359
(26.6, 58.0, 119.1 and 241.1 MiB), about two frames a page at every size. Crash injection
(`crates/amphora/tests/crash.rs`,
`a_power_cut_in_a_transaction_that_wrote_its_frames_over_keeps_the_last_commit`): 1,200 edits of 600
one-page documents on a 200-page cache, a posting and an upsert each, in nested savepoints, the
commit flushed, the power cut at a change drawn from the 319 the transaction makes: in every gate
100 seeds, 79 cut midway, 78 of them after writes over the log's own frames, 21 restarted with the
whole transaction, the rest with what was there before; in 10,000 seeds (release) 7,964 cut midway,
7,913 of them after writes over the log's own frames, 2,044 restarted with the whole transaction
(2,036 of them acknowledged), never one with a part of it. The engine's suites pass unchanged: the
log's and pager's unit tests (116 in `storage::wal` and `storage::pager`, among them the contract
that a commit chains after its transaction's spilled frames), the model that rolls savepoints back
amid spills in both log formats (`packed_log.rs`), and the reader-cache and shared-pages models.
What it costs, the four legs in turn with c87e332's (release binaries, adacachy 2026-10-29 at load
17-90, best of 2; checks rather than recorded runs): on files written in place, as decision 12
makes Amphora's, 50.3 s against c87e332's 76.9 s (frames written one at a time, not together: 64.4
s); in a copy-on-write directory with compression, 59.6 s against 39.5 s, every frame written over
there a new extent.

**Falsified by.** A transaction whose log holds more frames past the last commit than the pages it
dirtied before its commit, or more than twice them after (`spilled_log.rs`); a row read back other
than as committed, on a fresh connection or after opening again (the models); a power cut that
leaves part of a transaction whose frames were written over, or loses one whose commit was flushed
(`tests/crash.rs`); on files written in place, a transaction that spills taking longer than it did
when its spills appended.

## 31. A restart cuts a log past twice its bound to nothing

**Decision.** `Db::restart_log` starts the log again with its file kept (decision 13) while the
file holds room for no more frames than `Db::keep_log` keeps: by default twice the store's default
bound (decision 4), 262,144 frames, 1.0 GiB of 4 KiB pages, the most a log takes between restarts. A
file past that is cut to nothing in the same step (the engine's TRUNCATE checkpoint). When: only at
a restart, which the store already makes between commits with its readers and checkpointer held
back, and only once every frame is in the file and the file flushed (the checkpoint copies what is
left and flushes the file first, then cuts the log and flushes it). Not cut to the bound: until the
next commit writes a new header the file holds the old generation's header and frames, all copied,
and a file cut short among them would keep a valid prefix of old frames that a recovery would read
over the newer pages the checkpoint wrote, which is why SQLite cuts a log to its limit only after
the new header is synced, and to zero bytes on close. A log within the bound pays nothing new.

**Why.** The unify audit (finding data-1): the restart kept the log's file at its high water for
good: the 8 MiB transaction's 7 GiB log stayed, and the rolled-back 12 MiB one left 9 GiB. Decision
30 bounds what one transaction writes by its pages, not by the store's bound: a transaction of a
million pages still grows the log past it, and nothing gave the space back.

**Sources.** sqlite.org/pragma.html, `journal_size_limit`: "in WAL mode, the write-ahead log file is
not truncated following a checkpoint. Instead, SQLite reuses the existing file for subsequent WAL
entries since overwriting is faster than appending", and "Each time a transaction is committed or a
WAL file resets, SQLite compares the size of the rollback journal file or WAL file left in the
file-system to the size limit set by this pragma and if the journal or WAL file is larger it is
truncated to the limit"; its default is no limit. sqlite.org/c3ref/wal_checkpoint_v2.html:
`SQLITE_CHECKPOINT_TRUNCATE` "works the same way as SQLITE_CHECKPOINT_RESTART with the addition that
it also truncates the log file to zero bytes just prior to a successful return". SQLite 3.54.0's
`src/wal.c`: the limit is applied at the first commit after a reset, once its header is written
(`truncateOnCommit`), and `sqlite3WalClose` cuts "to zero bytes as truncating to the
journal_size_limit might leave a corrupt WAL file on disk". sqlite.org/fileformat2.html:
"Performance is usually a little better if the WAL is not truncated, since filesystems generally
will overwrite an existing file faster than they will grow a file."

**Evidence.** `crates/amphora/tests/files.rs`,
`a_restart_cuts_a_log_past_its_bound_to_nothing_and_keeps_one_within_it`: with 64 frames kept, 500
rows of a page grew the log to 2,076,512 bytes; on c87e332 (with the setter alone) the restart left
the file at that length (the test failed); with this decision it cut it to 0 bytes, a row's commit
grew it to a few frames, a restart within the bound kept that length, and all 501 rows read back
after opening again. Crash injection (`tests/crash.rs`): every third seed keeps 16 frames, so each
of its restarts cuts the log; in 10,000 seeds 4,365 cuts came before a power cut (40 in the gate's
200), and nothing acknowledged was lost, nothing refused found, nothing torn. Its cost to writers
(`what_cutting_the_log_costs_its_writer`, release without LTO, adacachy 2026-10-29 at load 66-100,
files in place as decision 12 makes them; a check rather than a recorded run): on a 1 GiB log a
restart keeping the file held the writer 42.4 ms (best of 6) and one cutting it 66.0 ms (best of 2),
and the commit of 4 MiB and the flush after it took 36.0 ms after a restart that kept the file and
79.7 ms after one that cut it (its writes extend the file again). A log is cut only once it has
outgrown twice its bound, so that is paid once for each such log, not at every restart.

**Falsified by.** A log's file longer than its bound after a restart that copied every frame
(`files.rs`); an acknowledged write lost, or a database refused, after a power cut around a restart
that cut the log (`tests/crash.rs`); a restart that cuts the log holding the writer much longer than
one that keeps it, or the commits after it slower than after a restart that keeps the file.
