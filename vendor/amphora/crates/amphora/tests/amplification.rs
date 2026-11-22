//! Small durable commits' write amplification (kiln goal 60805ec4, docs/design/store.md decisions
//! 35 and 37; AC-T1.4.4): the store on a disk that counts what is written to the log and to the file, under
//! the unify audit's batched leg (kiln's log index posting 128 grams a durable call, its
//! statements and its shape), and the bytes written per byte of the rows its commits wrote.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use amphora::db::Db;
use amphora::store::{Checkpointed, Config, Engine, Flush, Opener, Store};
use amphora_engine::io::{FileId, FileSyncType};
use amphora_engine::{
    Buffer, Clock, Completion, File, IO, MemoryIO, MonotonicInstant, OpenFlags, Value,
    WallClockInstant,
};

/// What a flush takes: fedora's disk's p50 (docs/design/store.md, decision 13: 2.74-3.56 ms), so
/// commits land between checkpoints as they do on a disk.
const FLUSH_TAKES: Duration = Duration::from_millis(3);

/// Parks this thread for `d` (a park ends early for no reason sometimes: the rest is parked again).
fn pause(d: Duration) {
    let until = Instant::now() + d;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::park_timeout(left);
    }
}

/// Bytes written to and flushes of the log and of the file.
#[derive(Default)]
struct Counts {
    log: AtomicU64,
    file: AtomicU64,
    log_flushes: AtomicU64,
    file_flushes: AtomicU64,
}

/// Memory's files, each write and flush counted.
struct Counting {
    inner: Arc<MemoryIO>,
    counts: Arc<Counts>,
}

struct CountedFile {
    inner: Arc<dyn File>,
    log: bool,
    counts: Arc<Counts>,
}

impl CountedFile {
    fn wrote(&self, n: usize) {
        let to = if self.log {
            &self.counts.log
        } else {
            &self.counts.file
        };
        to.fetch_add(n as u64, Ordering::Relaxed);
    }
}

impl File for CountedFile {
    fn lock_file(&self, exclusive: bool) -> amphora_engine::Result<()> {
        self.inner.lock_file(exclusive)
    }

    fn unlock_file(&self) -> amphora_engine::Result<()> {
        self.inner.unlock_file()
    }

    fn pread(&self, pos: u64, c: Completion) -> amphora_engine::Result<Completion> {
        self.inner.pread(pos, c)
    }

    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.wrote(buffer.len());
        self.inner.pwrite(pos, buffer, c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.wrote(buffers.iter().map(|b| b.len()).sum());
        self.inner.pwritev(pos, buffers, c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> amphora_engine::Result<Completion> {
        let to = if self.log {
            &self.counts.log_flushes
        } else {
            &self.counts.file_flushes
        };
        to.fetch_add(1, Ordering::Relaxed);
        pause(FLUSH_TAKES);
        self.inner.sync(c, sync_type)
    }

    fn size(&self) -> amphora_engine::Result<u64> {
        self.inner.size()
    }

    fn truncate(&self, len: u64, c: Completion) -> amphora_engine::Result<Completion> {
        self.inner.truncate(len, c)
    }
}

impl Clock for Counting {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for Counting {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        direct: bool,
    ) -> amphora_engine::Result<Arc<dyn File>> {
        Ok(Arc::new(CountedFile {
            inner: self.inner.open_file(path, flags, direct)?,
            log: path.ends_with("-wal"),
            counts: self.counts.clone(),
        }))
    }

    fn remove_file(&self, path: &str) -> amphora_engine::Result<()> {
        self.inner.remove_file(path)
    }

    fn step(&self) -> amphora_engine::Result<()> {
        self.inner.step()
    }

    fn file_id(&self, path: &str) -> amphora_engine::Result<FileId> {
        self.inner.file_id(path)
    }
}

/// A seeded generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Kiln's log index (the audit's harness: migration 20261030-0200_u05_log_index_progress.sql),
/// a posting's bitmap an integer here.
const SCHEMA: &str = "
CREATE TABLE ci_log_grams (gram INTEGER NOT NULL, finished INTEGER NOT NULL, leg_order INTEGER NOT NULL, leg TEXT NOT NULL, block INTEGER NOT NULL, bits INTEGER NOT NULL, PRIMARY KEY (gram, finished, leg_order, leg, block));
CREATE TABLE ci_log_terms (gram INTEGER PRIMARY KEY, documents INTEGER NOT NULL);
CREATE TABLE ci_log_search_ready (doc INTEGER PRIMARY KEY, version TEXT NOT NULL);
CREATE TABLE ci_log_search_pending (doc INTEGER PRIMARY KEY, after_gram INTEGER, bytes INTEGER);
CREATE TABLE ci_log_doc_gram_pages (doc INTEGER NOT NULL, version TEXT NOT NULL, part INTEGER NOT NULL, grams BLOB NOT NULL, PRIMARY KEY (doc, version, part));
";

const GRAMS_PER_CALL: usize = 128;
const BLOCK_DOCS: i64 = 64;

/// The audit's 1 MiB of kiln's logs: 157 documents of about 1,290 distinct trigrams each, 15,639
/// distinct in all, 27,914 postings, 1,665 calls. Drawn here: 92% of a document's grams from 4,000
/// common ones, the rest from 17,000 others (about 15,000 distinct, 27,000 postings, 1,650 calls).
fn documents() -> Vec<Vec<i64>> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..157)
        .map(|_| {
            let mut grams = std::collections::BTreeSet::new();
            while grams.len() < 1290 {
                let r = rng.next();
                let rank = if r % 100 < 92 {
                    (r >> 32) % 4000
                } else {
                    4000 + (r >> 32) % 17_000
                };
                // A trigram's code spreads its rank over the keys as characters do.
                grams.insert(i64::try_from(rank.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 2).unwrap());
            }
            grams.into_iter().collect()
        })
        .collect()
}

enum Call {
    Begin(i64),
    Post {
        doc: i64,
        grams: Vec<i64>,
        done: bool,
    },
}

/// The bytes of the rows a call writes: 8 a number, a text's or blob's length.
fn row_bytes(call: &Call) -> u64 {
    match call {
        Call::Begin(_) => 8,
        Call::Post { grams, done, .. } => {
            let n = grams.len() as u64;
            // A posting (5 numbers and the leg's 8 bytes), a term (2), the page of grams (doc,
            // version, part and the grams), the pending row's update; done, the ready row and the
            // pending row's removal.
            n * (48 + 16) + (8 + 2 + 8 + 8 * n) + 16 + if *done { 10 + 8 } else { 0 }
        }
    }
}

struct Postings {
    db: Db,
}

fn post(db: &Db, doc: i64, grams: &[i64], done: bool) -> anyhow::Result<()> {
    db.query(
        "SELECT after_gram FROM ci_log_search_pending WHERE doc = ?1",
        [Value::from_i64(doc)],
        |_| Ok(false),
    )?;
    let (block, mask) = (doc / BLOCK_DOCS, 1i64 << (doc % BLOCK_DOCS).min(62));
    for &gram in grams {
        db.query(
            "INSERT INTO ci_log_grams (gram, finished, leg_order, leg, block, bits) VALUES (?1, 1, 1, 'leg-0001', ?2, ?3) ON CONFLICT (gram, finished, leg_order, leg, block) DO UPDATE SET bits = bits | excluded.bits",
            [Value::from_i64(gram), Value::from_i64(block), Value::from_i64(mask)],
            |_| Ok(true),
        )?;
        db.query(
            "INSERT INTO ci_log_terms (gram, documents) VALUES (?1, 1) ON CONFLICT (gram) DO UPDATE SET documents = documents + 1",
            [Value::from_i64(gram)],
            |_| Ok(true),
        )?;
    }
    if let Some(&last) = grams.last() {
        db.query(
            "INSERT INTO ci_log_doc_gram_pages (doc, version, part, grams) VALUES (?1, 'v1', ?2, randomblob(?3))",
            [
                Value::from_i64(doc),
                Value::from_i64(last),
                Value::from_i64(i64::try_from(grams.len() * 8).unwrap()),
            ],
            |_| Ok(true),
        )?;
        db.query(
            "UPDATE ci_log_search_pending SET after_gram = ?2 WHERE doc = ?1",
            [Value::from_i64(doc), Value::from_i64(last)],
            |_| Ok(true),
        )?;
    }
    if done {
        db.query(
            "INSERT INTO ci_log_search_ready (doc, version) VALUES (?1, 'v1')",
            [Value::from_i64(doc)],
            |_| Ok(true),
        )?;
        db.query(
            "DELETE FROM ci_log_search_pending WHERE doc = ?1",
            [Value::from_i64(doc)],
            |_| Ok(true),
        )?;
    }
    Ok(())
}

impl Engine for Postings {
    type Row = Call;
    type Reader = Db;

    fn key(_: &Call) -> &str {
        "postings"
    }

    fn apply(&mut self, rows: &[Call]) -> anyhow::Result<usize> {
        self.db.transaction(|db| {
            for call in rows {
                match call {
                    Call::Begin(doc) => db.query(
                        "INSERT INTO ci_log_search_pending (doc) VALUES (?1)",
                        [Value::from_i64(*doc)],
                        |_| Ok(true),
                    )?,
                    Call::Post { doc, grams, done } => post(db, *doc, grams, *done)?,
                }
            }
            Ok(rows.len())
        })
    }

    fn flusher(&self) -> Option<Flush> {
        Some(self.db.flusher())
    }

    fn opener(&self) -> Option<Opener<Db>> {
        Some(self.db.opener())
    }

    fn checkpoint(reader: &mut Db) -> anyhow::Result<Checkpointed> {
        reader.checkpoint()
    }

    fn uncopied(reader: &mut Db) -> anyhow::Result<Option<Checkpointed>> {
        Ok(Some(reader.uncopied()))
    }

    fn restart_log(&mut self) -> anyhow::Result<bool> {
        self.db.restart_log()
    }
}

/// The documents ready, the postings their terms count and the documents still pending.
fn counts_of(db: &Db) -> anyhow::Result<(u64, u64, u64)> {
    let mut counted = (0, 0, 0);
    db.query(
        "SELECT (SELECT count(*) FROM ci_log_search_ready), (SELECT sum(documents) FROM ci_log_terms), (SELECT count(*) FROM ci_log_search_pending)",
        [],
        |r| {
            counted = (
                u64::try_from(r.get::<i64>(0)?)?,
                u64::try_from(r.get::<i64>(1)?)?,
                u64::try_from(r.get::<i64>(2)?)?,
            );
            Ok(false)
        },
    )?;
    Ok(counted)
}

/// What the batched leg wrote.
struct Wrote {
    rows: u64,
    log: u64,
    file: u64,
    log_flushes: u64,
    file_flushes: u64,
    calls: u64,
}

fn batched_leg(config: Config) -> Wrote {
    let counts = Arc::new(Counts::default());
    let io = Arc::new(Counting {
        inner: Arc::new(MemoryIO::new()),
        counts: counts.clone(),
    });
    let db = Db::open_with(std::path::Path::new("/amplification/postings.db"), io).unwrap();
    db.exec(SCHEMA).unwrap();
    db.flusher()().unwrap();
    let store = Store::start(Postings { db }, config).unwrap();
    let base = (
        counts.log.load(Ordering::Relaxed),
        counts.file.load(Ordering::Relaxed),
        counts.log_flushes.load(Ordering::Relaxed),
        counts.file_flushes.load(Ordering::Relaxed),
    );
    let (mut rows, mut calls) = (0u64, 0u64);
    let soon = || Instant::now() + Duration::from_secs(30);
    let documents = documents();
    let postings: u64 = documents.iter().map(|g| g.len() as u64).sum();
    for (doc, grams) in documents.iter().enumerate() {
        let doc = i64::try_from(doc).unwrap();
        let mut each = vec![Call::Begin(doc)];
        let parts: Vec<&[i64]> = grams.chunks(GRAMS_PER_CALL).collect();
        let last = parts.len() - 1;
        each.extend(parts.into_iter().enumerate().map(|(i, g)| Call::Post {
            doc,
            grams: g.to_vec(),
            done: i == last,
        }));
        // One durable call at a time, as kiln's indexer waits for each.
        for call in each {
            rows += row_bytes(&call);
            calls += 1;
            let ticket = store.put_wait(vec![call], soon()).unwrap();
            store.durable(&ticket, soon()).unwrap();
        }
    }
    // Every document posted whole, read back beside the writer.
    let counted = store
        .read(
            None,
            soon(),
            |db| counts_of(db),
            |engine: &mut Postings| counts_of(&engine.db),
        )
        .unwrap();
    assert_eq!(counted, (documents.len() as u64, postings, 0));
    Wrote {
        rows,
        log: counts.log.load(Ordering::Relaxed) - base.0,
        file: counts.file.load(Ordering::Relaxed) - base.1,
        log_flushes: counts.log_flushes.load(Ordering::Relaxed) - base.2,
        file_flushes: counts.file_flushes.load(Ordering::Relaxed) - base.3,
        calls,
    }
}

/// The batched leg on the store's defaults, run once for the tests that read it.
fn default_leg() -> &'static Wrote {
    static LEG: std::sync::OnceLock<Wrote> = std::sync::OnceLock::new();
    LEG.get_or_init(|| {
        let w = batched_leg(Config::default());
        eprintln!("{}", account(&w));
        w
    })
}

/// What a leg wrote, for a failing assertion.
fn account(w: &Wrote) -> String {
    let per_row = |b: u64| b as f64 / w.rows as f64;
    format!(
        "{} calls committed {} bytes of rows and wrote {} into the log ({:.1} a byte of rows, {} flushes) and {} into the file ({:.1}, {} flushes)",
        w.calls,
        w.rows,
        w.log,
        per_row(w.log),
        w.log_flushes,
        w.file,
        per_row(w.file),
        w.file_flushes,
    )
}

/// Decision 37: the batched leg logs its commits' changes, not their pages: at most 4 bytes of log
/// for each byte of the rows its calls commit, where a page a frame wrote 16.6.
#[test]
fn small_commits_log_their_changes_not_their_pages() {
    let w = default_leg();
    assert!(w.log <= w.rows.saturating_mul(4), "{}", account(w));
}

/// Decision 35: the batched leg, each call durable before the next as kiln's indexer waits for it,
/// writes into the file at most a quarter of what it writes into the log. The checkpoint copies a
/// page that many small commits change once for them all, not once for every period's commits.
#[test]
fn small_commits_copy_their_pages_once_for_many_commits() {
    let w = default_leg();
    assert!(w.file.saturating_mul(4) <= w.log, "{}", account(w));
}
