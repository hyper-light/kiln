//! Crash injection (T-T1.1.1; AC-T1.1.1: no update is dropped unsaid): the store on a simulated
//! disk that loses its power at a chosen change. A flush takes time, as a disk's does, and makes
//! durable what was written before it began; each change not made durable survives whole, torn or
//! not at all, in any combination. After the restart, every batch acknowledged durable is there,
//! every batch the engine refused isn't, every other batch is there whole or not at all, and the
//! database passes Turso's integrity check. The same disk under one transaction too large for its
//! cache, whose spills write frames over again (docs/design/store.md, decision 30): after the
//! restart it is there whole or not at all, and there once its flush was told done.

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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use amphora::db::{Db, LogFormat, Sharing};
use amphora::store::{Checkpointed, Config, Engine, Flush, Opener, Refused, Store};
use amphora_engine::io::{FileId, FileSyncType};
use amphora_engine::{
    Buffer, Clock, Completion, CompletionError, File, IO, LimboError, MonotonicInstant, OpenFlags,
    Value, WallClockInstant,
};

/// A seeded generator (xorshift64*): a seed's workload and losses are its own.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// A change made to a file since its last flush.
#[derive(Clone)]
enum Change {
    Write(u64, Vec<u8>),
    Truncate(u64),
}

fn apply(bytes: &mut Vec<u8>, change: &Change) {
    match change {
        Change::Write(pos, data) => {
            let pos = usize::try_from(*pos).unwrap();
            let end = pos + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[pos..end].copy_from_slice(data);
        }
        Change::Truncate(len) => bytes.resize(usize::try_from(*len).unwrap(), 0),
    }
}

/// What a file holds: what its flushes made durable, the changes not yet durable (in order), how
/// many changes were made durable in all, what a reader sees (both), and how many writes went over
/// bytes it already held and how many times it was cut.
#[derive(Default)]
struct Held {
    durable: Vec<u8>,
    since: Vec<Change>,
    flushed: u64,
    seen: Vec<u8>,
    rewrites: u64,
    cuts: u64,
}

/// How long a flush takes. A change made while one runs isn't part of it, so a commit can land
/// during a flush and still be unflushed after it, and a checkpoint can run meanwhile, as on a real
/// disk (fedora's takes 2.7 ms; a fifth of a millisecond keeps 10,000 seeds quick).
const FLUSH_TAKES: Duration = Duration::from_micros(200);

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

/// Gives each disk its own files: Turso keeps one database open per file a process opens, so a
/// disk restarted on what survived must not name the crashed one's.
static DISKS: AtomicU64 = AtomicU64::new(0);

/// A disk whose power goes at the `crash_at`th change made after it is armed.
struct Disk {
    me: Weak<Disk>,
    id: u64,
    files: Mutex<HashMap<String, Arc<SimFile>>>,
    /// Held by each change as it is made, and by the power cut alone: a change is wholly before the
    /// cut or after it.
    order: RwLock<()>,
    changes: AtomicU64,
    crash_at: AtomicU64,
    /// What survived the cut, file by file.
    image: Mutex<Option<HashMap<String, Vec<u8>>>>,
    rng: Mutex<Rng>,
    /// What the cut did with each change since a flush that touched the database's first page or
    /// the log's header (for a failing seed's account).
    told: Mutex<Vec<String>>,
    /// While true, a flush waits (after taking what it makes durable) until it is let go; with the
    /// number of flushes waiting.
    holding: (Mutex<(bool, u64)>, Condvar),
}

fn power_out() -> LimboError {
    LimboError::CompletionError(CompletionError::IOError(
        std::io::ErrorKind::Other,
        "the power is out",
    ))
}

impl Disk {
    fn new(seed: u64, image: HashMap<String, Vec<u8>>) -> Arc<Self> {
        Arc::new_cyclic(|me: &Weak<Disk>| {
            let files = image
                .into_iter()
                .map(|(path, bytes)| {
                    let file = Arc::new(SimFile {
                        disk: me.clone(),
                        held: Mutex::new(Held {
                            durable: bytes.clone(),
                            seen: bytes,
                            ..Held::default()
                        }),
                    });
                    (path, file)
                })
                .collect();
            Disk {
                me: me.clone(),
                id: DISKS.fetch_add(1, Ordering::Relaxed),
                files: Mutex::new(files),
                order: RwLock::new(()),
                changes: AtomicU64::new(0),
                crash_at: AtomicU64::new(u64::MAX),
                image: Mutex::new(None),
                rng: Mutex::new(Rng(seed | 1)),
                told: Mutex::new(Vec::new()),
                holding: (Mutex::new((false, 0)), Condvar::new()),
            }
        })
    }

    /// The power goes at the `at`th change from now.
    fn arm(&self, at: u64) {
        self.changes.store(0, Ordering::SeqCst);
        self.crash_at.store(at, Ordering::SeqCst);
    }

    fn crashed(&self) -> bool {
        self.image.lock().unwrap().is_some()
    }

    /// Flushes from now wait until [`Disk::let_go`].
    fn hold(&self) {
        self.holding.0.lock().unwrap().0 = true;
    }

    /// Waits until `n` flushes are held.
    fn held(&self, n: u64) {
        let mut h = self.holding.0.lock().unwrap();
        while h.1 < n {
            h = self.holding.1.wait(h).unwrap();
        }
    }

    fn let_go(&self) {
        self.holding.0.lock().unwrap().0 = false;
        self.holding.1.notify_all();
    }

    /// A flush, once it knows what it makes durable: waits while flushes are held.
    fn flushing(&self) {
        let mut h = self.holding.0.lock().unwrap();
        if !h.0 {
            return;
        }
        h.1 += 1;
        self.holding.1.notify_all();
        while h.0 {
            h = self.holding.1.wait(h).unwrap();
        }
        h.1 -= 1;
    }

    /// `f` made to `held`, unless the power is out; the change that reaches the crash point cuts it.
    fn change(&self, held: &Mutex<Held>, f: impl FnOnce(&mut Held)) -> amphora_engine::Result<()> {
        {
            let _ordered = self.order.read().unwrap();
            let n = self.changes.fetch_add(1, Ordering::SeqCst) + 1;
            if n < self.crash_at.load(Ordering::SeqCst) && !self.crashed() {
                f(&mut held.lock().unwrap());
                return Ok(());
            }
        }
        self.cut();
        Err(power_out())
    }

    /// The power cut: each file keeps what its last flush made durable, and each change since
    /// whole, torn at a 512-byte boundary, or not at all.
    fn cut(&self) {
        let _alone = self.order.write().unwrap();
        let mut image = self.image.lock().unwrap();
        if image.is_some() {
            return;
        }
        let files = self.files.lock().unwrap();
        let mut rng = self.rng.lock().unwrap();
        let mut survived = HashMap::new();
        let mut told = self.told.lock().unwrap();
        for (path, file) in files.iter() {
            let held = file.held.lock().unwrap();
            let mut bytes = held.durable.clone();
            told.push(format!(
                "{path}: {} bytes durable, {} bytes seen, {} changes since its last flush",
                held.durable.len(),
                held.seen.len(),
                held.since.len()
            ));
            for change in &held.since {
                let fate = match (rng.below(4), change) {
                    (0 | 1, change) => {
                        apply(&mut bytes, change);
                        "kept".to_owned()
                    }
                    (2, _) => "lost".to_owned(),
                    (_, Change::Write(pos, data)) => {
                        let kept =
                            usize::try_from(rng.below(data.len() as u64 / 512 + 1) * 512).unwrap();
                        let torn = Change::Write(*pos, data[..kept.min(data.len())].to_vec());
                        apply(&mut bytes, &torn);
                        format!("torn after {kept} bytes")
                    }
                    (_, change) => {
                        apply(&mut bytes, change);
                        "kept".to_owned()
                    }
                };
                let (pos, len) = match change {
                    Change::Write(pos, data) => (*pos, data.len() as u64),
                    Change::Truncate(len) => (*len, 0),
                };
                if pos < 4096 {
                    told.push(format!("  {path}: change at {pos}, {len} bytes: {fate}"));
                }
            }
            survived.insert(path.clone(), bytes);
        }
        *image = Some(survived);
    }

    /// What survived: the power is cut now if it wasn't already.
    fn image(&self) -> HashMap<String, Vec<u8>> {
        self.cut();
        self.image.lock().unwrap().clone().unwrap()
    }
}

struct SimFile {
    disk: Weak<Disk>,
    held: Mutex<Held>,
}

impl SimFile {
    fn change(&self, f: impl FnOnce(&mut Held)) -> amphora_engine::Result<()> {
        let disk = self.disk.upgrade().ok_or_else(power_out)?;
        disk.change(&self.held, f)
    }

    fn write(&self, pos: u64, data: Vec<u8>, c: Completion) -> amphora_engine::Result<Completion> {
        let n = i32::try_from(data.len()).unwrap();
        self.change(|held| {
            held.rewrites += u64::from(pos + data.len() as u64 <= held.seen.len() as u64);
            let change = Change::Write(pos, data);
            apply(&mut held.seen, &change);
            held.since.push(change);
        })?;
        c.complete(n);
        Ok(c)
    }
}

impl File for SimFile {
    fn lock_file(&self, _exclusive: bool) -> amphora_engine::Result<()> {
        Ok(())
    }

    fn unlock_file(&self) -> amphora_engine::Result<()> {
        Ok(())
    }

    fn pread(&self, pos: u64, c: Completion) -> amphora_engine::Result<Completion> {
        let n = {
            let held = self.held.lock().unwrap();
            let dst = c.as_read().buf().as_mut_slice();
            let pos = usize::try_from(pos).unwrap();
            let src = held.seen.get(pos..).unwrap_or_default();
            let n = src.len().min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
            n
        };
        c.complete(i32::try_from(n).unwrap());
        Ok(c)
    }

    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.write(pos, buffer.as_slice().to_vec(), c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        let data = buffers
            .iter()
            .flat_map(|b| b.as_slice().iter().copied())
            .collect();
        self.write(pos, data, c)
    }

    fn sync(&self, c: Completion, _sync_type: FileSyncType) -> amphora_engine::Result<Completion> {
        // What was written before the flush began is what it makes durable (a flush begun later
        // may already have made it durable).
        let target = {
            let held = self.held.lock().unwrap();
            held.flushed + held.since.len() as u64
        };
        if let Some(disk) = self.disk.upgrade() {
            disk.flushing();
        }
        pause(FLUSH_TAKES);
        self.change(|held| {
            let n = usize::try_from(target.saturating_sub(held.flushed))
                .unwrap()
                .min(held.since.len());
            for change in held.since.drain(..n) {
                apply(&mut held.durable, &change);
            }
            held.flushed += n as u64;
        })?;
        c.complete(0);
        Ok(c)
    }

    fn truncate(&self, len: u64, c: Completion) -> amphora_engine::Result<Completion> {
        self.change(|held| {
            held.cuts += 1;
            let change = Change::Truncate(len);
            apply(&mut held.seen, &change);
            held.since.push(change);
        })?;
        c.complete(0);
        Ok(c)
    }

    fn size(&self) -> amphora_engine::Result<u64> {
        Ok(self.held.lock().unwrap().seen.len() as u64)
    }
}

impl Clock for Disk {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        MonotonicInstant::now()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        WallClockInstant::now()
    }
}

impl IO for Disk {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        _direct: bool,
    ) -> amphora_engine::Result<Arc<dyn File>> {
        let mut files = self.files.lock().unwrap();
        if let Some(file) = files.get(path) {
            return Ok(file.clone());
        }
        if !flags.contains(OpenFlags::Create) {
            return Err(LimboError::CompletionError(CompletionError::IOError(
                std::io::ErrorKind::NotFound,
                "open",
            )));
        }
        let file = Arc::new(SimFile {
            disk: self.me.clone(),
            held: Mutex::new(Held::default()),
        });
        files.insert(path.to_owned(), file.clone());
        Ok(file)
    }

    fn remove_file(&self, path: &str) -> amphora_engine::Result<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn file_id(&self, path: &str) -> amphora_engine::Result<FileId> {
        Ok(FileId::from_path_hash(&format!("{}:{path}", self.id)))
    }
}

/// Keyed events, each tagged with its batch; a row of negative seq fails the table's CHECK.
struct Events {
    db: Db,
}

struct Row {
    key: String,
    seq: i64,
    batch: usize,
}

const PATH: &str = "/crash/events.db";
const INSERT: &str = "INSERT INTO events (key, seq, batch) VALUES (?1, ?2, ?3)";

impl Engine for Events {
    type Row = Row;
    type Reader = Db;

    fn key(row: &Row) -> &str {
        &row.key
    }

    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<usize> {
        self.db.transaction(|db| {
            for r in rows {
                let params = [
                    Value::from_text(r.key.clone()),
                    Value::from_i64(r.seq),
                    Value::from_i64(i64::try_from(r.batch).unwrap()),
                ];
                db.query(INSERT, params, |_| Ok(true))?;
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

/// The WAL's checksum (SQLite's format: words little-endian under magic 0x377f0682).
fn wal_checksum(data: &[u8], (mut s0, mut s1): (u32, u32), little: bool) -> (u32, u32) {
    for w in data.as_chunks::<8>().0 {
        let word = |b: &[u8]| {
            let a: [u8; 4] = b.try_into().unwrap();
            if little {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }
        };
        s0 = s0.wrapping_add(word(&w[0..4]).wrapping_add(s1));
        s1 = s1.wrapping_add(word(&w[4..8]).wrapping_add(s0));
    }
    (s0, s1)
}

/// What a recovery would find in a log image: its header, and each frame's page, whether it ends
/// a commit, and whether it holds (salts and the checksum chain), up to the first that doesn't.
fn wal_account(wal: &[u8]) -> String {
    if wal.len() < 32 {
        return format!("log: {} bytes, no header", wal.len());
    }
    let be = |at: usize| u32::from_be_bytes(wal[at..at + 4].try_into().unwrap());
    let (magic, page_size, seq, salt) = (be(0), be(8) as usize, be(12), (be(16), be(20)));
    let little = magic == 0x377f_0682;
    let mut sum = wal_checksum(&wal[0..24], (0, 0), little);
    let header_holds = sum == (be(24), be(28));
    let mut out = format!(
        "log: {} bytes, magic {magic:#x}, page size {page_size}, checkpoint {seq}, salts {salt:?}, header {}\n",
        wal.len(),
        if header_holds { "holds" } else { "BROKEN" }
    );
    let mut at = 32;
    let (mut frame, mut last_commit) = (0, 0);
    while page_size > 0 && at + 24 + page_size <= wal.len() {
        frame += 1;
        let (page, commit) = (be(at), be(at + 4));
        let holds_salt = (be(at + 8), be(at + 12)) == salt;
        sum = wal_checksum(&wal[at..at + 8], sum, little);
        sum = wal_checksum(&wal[at + 24..at + 24 + page_size], sum, little);
        let holds = holds_salt && sum == (be(at + 16), be(at + 20));
        out.push_str(&format!(
            "  frame {frame}: page {page}{}{}\n",
            if commit > 0 {
                format!(", commit (db {commit} pages)")
            } else {
                String::new()
            },
            if holds {
                ""
            } else if holds_salt {
                ", checksum BREAKS"
            } else {
                ", other salts"
            }
        ));
        if !holds {
            break;
        }
        if commit > 0 {
            last_commit = frame;
        }
        at += 24 + page_size;
    }
    out.push_str(&format!("  recovery keeps frames 1..={last_commit}"));
    out
}

/// What became of a batch: how many rows it had, whether the engine was bound to refuse it, and
/// what its sender was told.
struct Sent {
    rows: usize,
    bad: bool,
    told: Option<Result<(), Refused>>,
}

/// The crash point is drawn up to here: a whole workload makes 174-197 changes (percentiles
/// 10-90, measured by this test), so most seeds lose the power midway.
const CHANGES: u64 = 200;

/// What a seed exercised, so a run can show its checks weren't vacuous.
#[derive(Default)]
struct Seen {
    /// The power went out before the workload was done.
    cut_midway: bool,
    /// Changes made to the disk until the cut, or the workload's end.
    changes: u64,
    /// The checkpoints and restarts of the log the store made before the cut.
    checkpoints: u64,
    restarts: u64,
    /// The times a restart cut the log's file to nothing before the cut (decision 31).
    log_cuts: u64,
    acknowledged: usize,
    refused: usize,
}

/// One seed: a workload on a disk that loses its power at a change the seed draws, then the
/// restart and its checks. The failure, if any, names what was wrong.
fn one_seed(seed: u64) -> Result<Seen, String> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let disk = Disk::new(seed, HashMap::new());
    let path = std::path::Path::new(PATH);
    // Half the seeds on a mixed log, as a store opens one (docs/design/store.md, decision 37), a
    // quarter on a packed log (decisions 16 and 17), whose pages aren't shared, and a quarter a page
    // a frame; all but the packed share the process's pages (decision 26). Reopened, a packed log
    // refuses to share by itself.
    let format = match seed % 4 {
        1 => LogFormat::Packed,
        3 => LogFormat::Pages,
        _ => LogFormat::Mixed,
    };
    let sharing = if format == LogFormat::Packed {
        Sharing::Off
    } else {
        Sharing::Process
    };
    let mut db =
        Db::open_with_sharing(path, disk.clone(), &sharing).map_err(|e| format!("open: {e:#}"))?;
    db.log_format(format)
        .map_err(|e| format!("log format: {e:#}"))?;
    // Each seed's log has a bound of its own, 8 to 23 frames, so cuts land with frames left
    // uncopied across commits at every depth of a log within its bound (decision 35: nothing is
    // copied before the bound).
    let bound = 8 + seed % 16;
    // Every third seed keeps no more log than its bound across a restart, so each restart, past
    // the bound, cuts the log's file to nothing (docs/design/store.md, decision 31); the others keep
    // their logs' files (decision 13).
    if seed.is_multiple_of(3) {
        db.keep_log(bound);
    }
    db.exec("CREATE TABLE events (key TEXT, seq INTEGER CHECK (seq >= 0), batch INTEGER); CREATE INDEX by_key ON events (key, seq)")
        .map_err(|e| format!("schema: {e:#}"))?;
    // The schema durable first: the crash point is drawn among the workload's changes.
    db.flusher()().map_err(|e| format!("flush: {e}"))?;
    let config = Config {
        log_frames: bound,
        checkpoint_every: Duration::from_millis(1),
        ..Config::default()
    };
    let store = Store::start(Events { db }, config).map_err(|e| format!("start: {e:#}"))?;
    disk.arm(1 + rng.below(CHANGES));

    let soon = || Instant::now() + Duration::from_secs(10);
    let mut sent: Vec<Sent> = Vec::new();
    // Batches handed over and not yet waited for: up to a few stay in flight, so commits land
    // while flushes run and checkpoints copy.
    let mut tickets = std::collections::VecDeque::new();
    while sent.len() < 150 && !disk.crashed() {
        // A few batches handed over together: they share commits.
        for _ in 0..1 + rng.below(3) {
            let batch = sent.len();
            let n = 1 + usize::try_from(rng.below(4)).unwrap();
            let bad = rng.below(10) == 0;
            let rows = (0..n)
                .map(|i| Row {
                    key: format!("k{}", rng.below(5)),
                    seq: if bad && i + 1 == n {
                        -1
                    } else {
                        i64::try_from(batch * 10 + i).unwrap()
                    },
                    batch,
                })
                .collect();
            tickets.push_back((batch, store.put_wait(rows, soon())));
            sent.push(Sent {
                rows: n,
                bad,
                told: None,
            });
        }
        let in_flight = usize::try_from(rng.below(4)).unwrap();
        while tickets.len() > in_flight {
            let (batch, ticket) = tickets.pop_front().unwrap();
            sent[batch].told = Some(match ticket {
                Ok(t) => store.durable(&t, soon()),
                Err(e) => Err(e),
            });
        }
    }
    for (batch, ticket) in tickets {
        sent[batch].told = Some(match ticket {
            Ok(t) => store.durable(&t, soon()),
            Err(e) => Err(e),
        });
    }
    let counted = store.counters();
    drop(store);
    let mut seen = Seen {
        cut_midway: disk.crashed(),
        changes: disk.changes.load(Ordering::SeqCst),
        checkpoints: counted.checkpoints,
        restarts: counted.restarts,
        log_cuts: disk
            .files
            .lock()
            .unwrap()
            .get(&format!("{PATH}-wal"))
            .map_or(0, |f| f.held.lock().unwrap().cuts),
        ..Seen::default()
    };

    // The restart, on what survived.
    let image = disk.image();
    let account = || {
        let told = disk.told.lock().unwrap().join("\n");
        let db_file = image.get(PATH).map_or(0, Vec::len);
        let wal = image
            .get(&format!("{PATH}-wal"))
            .map_or_else(String::new, |w| wal_account(w));
        format!("\n{told}\ndatabase file: {db_file} bytes\n{wal}")
    };
    let db = Db::open_with(path, Disk::new(seed, image.clone()))
        .map_err(|e| format!("reopen: {e:#}{}", account()))?;
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}"))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let mut found: HashMap<usize, usize> = HashMap::new();
    db.query("SELECT batch FROM events", [], |r| {
        let batch = r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        *found.entry(usize::try_from(batch).unwrap()).or_default() += 1;
        Ok(true)
    })
    .map_err(|e| format!("rows: {e:#}"))?;
    for (batch, s) in sent.iter().enumerate() {
        let there = found.remove(&batch).unwrap_or(0);
        let acked = matches!(s.told, Some(Ok(())));
        if acked && there != s.rows {
            return Err(format!(
                "batch {batch} was acknowledged durable; {there} of its {} rows survived",
                s.rows
            ));
        }
        if s.bad && there != 0 {
            return Err(format!(
                "batch {batch} was refused by the engine; {there} of its rows are there"
            ));
        }
        if s.bad && acked {
            return Err(format!(
                "batch {batch} holds a row the engine refuses, and was acknowledged"
            ));
        }
        if there != 0 && there != s.rows {
            return Err(format!(
                "batch {batch} survived in part: {there} of its {} rows",
                s.rows
            ));
        }
        seen.acknowledged += usize::from(acked);
        seen.refused += usize::from(matches!(s.told, Some(Err(Refused::Failed))));
    }
    if let Some((batch, n)) = found.into_iter().next() {
        return Err(format!("{n} rows of batch {batch}, which was never sent"));
    }
    Ok(seen)
}

/// `one` on seeds `from..from + n`, side by side, each on a disk of its own (its files named apart in
/// Turso's registry): every seed passes (a seed that panics fails alone), and what each saw comes
/// back in seed order.
fn run_seeds<S: Send>(n: u64, from: u64, one: impl Fn(u64) -> Result<S, String> + Sync) -> Vec<S> {
    let next = AtomicU64::new(from);
    let results: Mutex<Vec<(u64, Result<S, String>)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= from + n {
                        return;
                    }
                    let seen = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| one(seed)))
                        .unwrap_or_else(|p| {
                            Err(format!(
                                "panicked: {}",
                                p.downcast_ref::<String>().map_or_else(
                                    || p.downcast_ref::<&str>().copied().unwrap_or("?").to_owned(),
                                    Clone::clone
                                )
                            ))
                        });
                    results.lock().unwrap().push((seed, seen));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(seed, _)| *seed);
    let mut failed = Vec::new();
    let mut passed = Vec::new();
    for (seed, seen) in results {
        match seen {
            Ok(seen) => passed.push(seen),
            Err(e) => failed.push((seed, e)),
        }
    }
    for (seed, e) in failed.iter().take(20) {
        eprintln!("seed {seed}: {e}");
    }
    assert!(
        failed.is_empty(),
        "{} of {n} seeds failed; the first: seed {} ({})",
        failed.len(),
        failed[0].0,
        failed[0].1
    );
    passed
}

/// `n` seeds from `from`: every one passes, and between them they cut the power midway in most,
/// and acknowledged and refused batches to check.
fn seeds(n: u64, from: u64) {
    let (mut midway, mut acknowledged, mut refused) = (0u64, 0usize, 0usize);
    let (mut checkpoints, mut restarts, mut log_cuts) = (0u64, 0u64, 0u64);
    // What whole workloads changed, to draw crash points from.
    let mut whole: Vec<u64> = Vec::new();
    for seen in run_seeds(n, from, one_seed) {
        if !seen.cut_midway {
            whole.push(seen.changes);
        }
        midway += u64::from(seen.cut_midway);
        acknowledged += seen.acknowledged;
        refused += seen.refused;
        checkpoints += seen.checkpoints;
        restarts += seen.restarts;
        log_cuts += seen.log_cuts;
    }
    eprintln!(
        "{n} seeds: cut midway in {midway}; {checkpoints} checkpoints, {restarts} restarts of the log \
         and {log_cuts} cuts of its file before a cut; {acknowledged} batches acknowledged, \
         {refused} refused"
    );
    whole.sort_unstable();
    assert!(
        midway * 2 > n,
        "the power went out midway in only {midway} of {n} seeds (whole workloads changed the disk {:?} times, by percentile 10/50/90)",
        [whole.len() / 10, whole.len() / 2, whole.len() * 9 / 10]
            .map(|i| whole.get(i).copied().unwrap_or(0))
    );
    assert!(
        acknowledged > 0 && refused > 0,
        "{acknowledged} acknowledged and {refused} refused batches checked"
    );
    assert!(
        checkpoints > 0 && restarts > 0 && log_cuts > 0,
        "{checkpoints} checkpoints, {restarts} restarts of the log and {log_cuts} cuts of its file \
         before a cut"
    );
}

/// The database the spilling transaction's seeds write.
const SPILL_PATH: &str = "/crash/spill.db";
const SPILL_SCHEMA: &str = "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL); \
     CREATE TABLE postings (term INTEGER NOT NULL, doc INTEGER NOT NULL, PRIMARY KEY (term, doc)); \
     CREATE INDEX postings_doc ON postings (doc, term); \
     CREATE TABLE terms (term INTEGER PRIMARY KEY, docs INTEGER NOT NULL)";
/// Documents committed and made durable before the transaction, each a page of text (a table leaf
/// of its own): 600 pages, three times the transaction's cache of 200.
const SPILL_DOCS: i64 = 600;
/// The transaction's edits: each rewrites a document drawn at random, inserts a posting into a table
/// with two indexes and upserts its term's count, so the pages it reads in outgrow its cache and
/// spill, and are changed and spilled again over the frames they spilled to (docs/design/store.md,
/// decision 30).
const SPILL_EDITS: usize = 1200;
/// The crash point is drawn up to here: a whole transaction, its commit and its flush make 319
/// changes in every seed (measured by this test), so about four seeds in five lose the power midway
/// and the rest check the transaction made durable.
const SPILL_CHANGES: u64 = 400;

/// A document's text at a version (0 as the baseline wrote it, else the edit that wrote it last).
fn spill_body(doc: i64, version: i64) -> String {
    format!("{doc:08}:{version:08}:{}", "x".repeat(3500))
}

/// The transaction's edits, (document, term), the same in every seed.
fn spill_edits() -> Vec<(i64, i64)> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    (0..SPILL_EDITS)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let doc = i64::try_from(x % SPILL_DOCS.unsigned_abs()).unwrap();
            (doc, i64::try_from((x >> 32) % 300).unwrap())
        })
        .collect()
}

/// The edits, a savepoint for each inside one for each 20, as kiln's log search nests them.
fn spill_edits_in(db: &mut Db, edits: &[(i64, i64)]) -> anyhow::Result<()> {
    for (k, &(doc, term)) in (1i64..).zip(edits) {
        if k % 20 == 1 {
            if k > 1 {
                db.exec("RELEASE step")?;
            }
            db.exec("SAVEPOINT step")?;
        }
        db.exec("SAVEPOINT edit")?;
        let body = [Value::from_i64(doc), Value::from_text(spill_body(doc, k))];
        db.query("UPDATE docs SET body = ?2 WHERE id = ?1", body, |_| {
            Ok(true)
        })?;
        let posting = [Value::from_i64(term), Value::from_i64(k)];
        db.query(
            "INSERT INTO postings (term, doc) VALUES (?1, ?2)",
            posting,
            |_| Ok(true),
        )?;
        db.query(
            "INSERT INTO terms (term, docs) VALUES (?1, 1) ON CONFLICT (term) DO UPDATE SET docs = docs + 1",
            [Value::from_i64(term)],
            |_| Ok(true),
        )?;
        db.exec("RELEASE edit")?;
    }
    db.exec("RELEASE step")
}

/// The edits in one transaction.
fn spill_transaction(db: &mut Db, edits: &[(i64, i64)]) -> anyhow::Result<()> {
    db.transaction(|db| spill_edits_in(db, edits))
}

/// A database of the baseline's documents, made durable, keeping 200 pages from here on.
/// The spilling transaction's baseline on `disk`, in a log of `format`: a mixed one (decision 37),
/// whose commits are records beside the spills' page frames, or one a page a frame.
fn spill_baseline(disk: &Arc<Disk>, format: LogFormat) -> Result<Db, String> {
    let path = std::path::Path::new(SPILL_PATH);
    let mut db = Db::open_with(path, disk.clone()).map_err(|e| format!("open: {e:#}"))?;
    db.log_format(format)
        .map_err(|e| format!("log format: {e:#}"))?;
    db.exec(SPILL_SCHEMA)
        .map_err(|e| format!("schema: {e:#}"))?;
    db.transaction(|db| {
        for doc in 0..SPILL_DOCS {
            let body = [Value::from_i64(doc), Value::from_text(spill_body(doc, 0))];
            db.query("INSERT INTO docs (id, body) VALUES (?1, ?2)", body, |_| {
                Ok(true)
            })?;
        }
        Ok(())
    })
    .map_err(|e| format!("baseline: {e:#}"))?;
    db.flusher()().map_err(|e| format!("flush: {e}"))?;
    db.keep_pages(200).map_err(|e| format!("cache: {e:#}"))?;
    Ok(db)
}

fn count(db: &mut Db, sql: &'static str, params: Vec<Value>) -> Result<i64, String> {
    let mut n = 0;
    db.query(sql, params, |r| {
        n = r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("{sql}: {e:#}"))?;
    Ok(n)
}

/// What a spilling transaction's seed exercised.
struct SpillSeen {
    cut_midway: bool,
    changes: u64,
    /// Writes over bytes the log already held before the cut: frames its spills wrote over, and
    /// frames written back with their checksums chained again.
    rewrites: u64,
    acknowledged: bool,
    kept: bool,
}

/// One seed: a durable baseline, then one transaction too large for its 200-page cache, committed
/// and flushed, on a disk that loses its power at a change the seed draws; then the restart and its
/// checks. The failure, if any, names what was wrong.
fn spill_seed(seed: u64) -> Result<SpillSeen, String> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let disk = Disk::new(seed, HashMap::new());
    let path = std::path::Path::new(SPILL_PATH);
    // Every other seed a page a frame, the others mixed (decision 37).
    let format = if seed.is_multiple_of(2) {
        LogFormat::Mixed
    } else {
        LogFormat::Pages
    };
    let mut db = spill_baseline(&disk, format)?;
    let edits = spill_edits();
    disk.arm(1 + rng.below(SPILL_CHANGES));
    let committed = spill_transaction(&mut db, &edits).is_ok();
    let acknowledged = committed && db.flusher()().is_ok();
    let rewrites = disk
        .files
        .lock()
        .unwrap()
        .get(&format!("{SPILL_PATH}-wal"))
        .map_or(0, |f| f.held.lock().unwrap().rewrites);
    drop(db);
    let (cut_midway, changes) = (disk.crashed(), disk.changes.load(Ordering::SeqCst));

    // The restart, on what survived.
    let image = disk.image();
    let account = || {
        let told = disk.told.lock().unwrap().join("\n");
        let wal = image
            .get(&format!("{SPILL_PATH}-wal"))
            .map_or_else(String::new, |w| wal_account(w));
        format!("\n{told}\n{wal}")
    };
    let mut db = Db::open_with(path, Disk::new(seed, image.clone()))
        .map_err(|e| format!("reopen: {e:#}{}", account()))?;
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}{}", account()))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}{}", account()));
    }
    let postings = count(&mut db, "SELECT count(*) FROM postings", vec![])?;
    let edited = i64::try_from(SPILL_EDITS).unwrap();
    let kept = if postings == 0 {
        false
    } else if postings == edited {
        true
    } else {
        return Err(format!(
            "{postings} of the transaction's {SPILL_EDITS} postings survived{}",
            account()
        ));
    };
    // Each document's version is the edit that wrote it last, or 0: summed, one number for the
    // baseline and one for the whole transaction.
    let mut last = vec![0i64; SPILL_DOCS.unsigned_abs().try_into().unwrap()];
    if kept {
        for (k, &(doc, _)) in (1i64..).zip(&edits) {
            last[usize::try_from(doc).unwrap()] = k;
        }
    }
    let body = Value::from_i64(i64::try_from(spill_body(0, 0).len()).unwrap());
    let whole = [
        (
            "SELECT count(*) FROM docs WHERE length(body) = ?1",
            vec![body],
            SPILL_DOCS,
        ),
        (
            "SELECT coalesce(sum(CAST(substr(body, 10, 8) AS INTEGER)), 0) FROM docs",
            vec![],
            last.iter().sum(),
        ),
        ("SELECT coalesce(sum(docs), 0) FROM terms", vec![], postings),
    ];
    for (sql, params, want) in whole {
        let n = count(&mut db, sql, params)?;
        if n != want {
            return Err(format!(
                "{sql}: {n}, where {} makes {want}{}",
                if kept {
                    "the whole transaction"
                } else {
                    "the baseline"
                },
                account()
            ));
        }
    }
    if acknowledged && !kept {
        return Err(format!(
            "the transaction's commit was flushed, and is gone{}",
            account()
        ));
    }
    Ok(SpillSeen {
        cut_midway,
        changes,
        rewrites,
        acknowledged,
        kept,
    })
}

/// `n` spilling seeds from `from`: every one passes, and between them they cut the power midway in
/// most, after writes over the log's own frames in most of those, and restart on the baseline alone
/// and on the whole transaction.
fn spill_seeds(n: u64, from: u64) {
    let seen = run_seeds(n, from, spill_seed);
    let midway = seen.iter().filter(|s| s.cut_midway).count();
    let rewritten = seen
        .iter()
        .filter(|s| s.cut_midway && s.rewrites > 0)
        .count();
    let kept = seen.iter().filter(|s| s.kept).count();
    let acknowledged = seen.iter().filter(|s| s.acknowledged).count();
    let mut whole: Vec<u64> = seen
        .iter()
        .filter(|s| !s.cut_midway)
        .map(|s| s.changes)
        .collect();
    whole.sort_unstable();
    let percentiles = [whole.len() / 10, whole.len() / 2, whole.len() * 9 / 10]
        .map(|i| whole.get(i).copied().unwrap_or(0));
    eprintln!(
        "{n} seeds: cut midway in {midway}, {rewritten} of them after writes over the log's own frames; \
         {kept} kept the transaction, {acknowledged} acknowledged it; whole workloads changed the disk \
         {percentiles:?} times (percentiles 10/50/90)"
    );
    assert!(
        midway * 2 > seen.len(),
        "the power went out midway in only {midway} of {n} seeds (whole workloads changed the disk \
         {percentiles:?} times, by percentile 10/50/90)"
    );
    assert!(
        rewritten * 2 > midway,
        "the log's own frames were written over before the cut in only {rewritten} of the {midway} \
         seeds cut midway"
    );
    assert!(
        kept > 0 && kept < seen.len() && acknowledged > 0,
        "{kept} of {n} seeds kept the transaction, {acknowledged} acknowledged it"
    );
}

/// T-T1.1.1 and decision 30 of docs/design/store.md, a sample run in every gate: a power cut at a
/// change drawn from a transaction whose spills write pages over frames it wrote before (and from
/// its commit, which chains their checksums again, and its flush): the restart holds the last
/// commit before it, whole, or the transaction too, whole, and that once its flush was told done.
#[test]
fn a_power_cut_in_a_transaction_that_wrote_its_frames_over_keeps_the_last_commit() {
    spill_seeds(100, 1);
}

/// The same in full: 10,000 seeds (`cargo test --release --test crash -- --ignored spilled`; from
/// another seed with AMPHORA_CRASH_FROM).
#[test]
#[ignore = "10,000 seeds: run on its own, in release"]
fn ten_thousand_power_cuts_in_spilled_transactions() {
    let from = std::env::var("AMPHORA_CRASH_FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    spill_seeds(10_000, from);
}

/// T-T1.1.1, the flusher's mark (docs/design/store.md, decision 15): a flush counts durable only
/// what the log held when it began. A commit that lands while a flush runs, held here, is not
/// copied by a checkpoint after it, which flushes no log of its own for what the mark covers.
/// Decision 30 of docs/design/store.md: the commit computes the checksum of every frame from the
/// first one a spill wrote over, so a spill computes none for the frames it writes over. Do: a
/// transaction spills its pages over their frames; read its log before it commits, then commit,
/// flush and restart. Expect: frames in the log with no checksum, and every edit after the restart,
/// whose recovery checks each frame's.
#[test]
fn a_frame_a_spill_writes_over_holds_no_checksum_until_its_commit() {
    let disk = Disk::new(1, HashMap::new());
    let mut db = spill_baseline(&disk, LogFormat::Mixed).unwrap();
    let edits = spill_edits();
    let unchecked = db
        .transaction(|db| {
            spill_edits_in(db, &edits)?;
            let files = disk.files.lock().unwrap();
            let held = files[&format!("{SPILL_PATH}-wal")].held.lock().unwrap();
            let frames = held.seen.get(32..).unwrap_or_default();
            let (frames, _) = frames.as_chunks::<{ 4096 + 24 }>();
            Ok(frames
                .iter()
                .filter(|frame| frame[..4] != [0; 4] && frame[16..24] == [0; 8])
                .count())
        })
        .unwrap();
    assert!(
        unchecked > 0,
        "every frame held a checksum before the commit"
    );
    db.flusher()().unwrap();
    drop(db);
    let path = std::path::Path::new(SPILL_PATH);
    let mut db = Db::open_with(path, Disk::new(1, disk.image())).unwrap();
    let postings = count(&mut db, "SELECT count(*) FROM postings", vec![]).unwrap();
    assert_eq!(postings, i64::try_from(SPILL_EDITS).unwrap());
}

#[test]
fn a_commit_that_lands_during_a_flush_is_not_counted_durable_by_it() {
    let disk = Disk::new(1, HashMap::new());
    let db = Db::open_with(std::path::Path::new(PATH), disk.clone()).unwrap();
    db.exec("CREATE TABLE t (n INTEGER)").unwrap();
    db.exec("INSERT INTO t (n) VALUES (1)").unwrap();
    let flush = db.flusher();
    disk.hold();
    let flushing = std::thread::spawn(flush);
    disk.held(1);
    db.exec("INSERT INTO t (n) VALUES (2)").unwrap();
    disk.let_go();
    flushing.join().unwrap().unwrap();
    let mut reader = db.reader().unwrap();
    let copied = reader.checkpoint().unwrap();
    assert!(
        copied.left > 0,
        "a checkpoint copied a commit no flush made durable: {copied:?}"
    );
    // A flush begun after it: now it is copied.
    db.flusher()().unwrap();
    let copied = reader.checkpoint().unwrap();
    assert_eq!(copied.left, 0, "{copied:?}");
}

/// Rows that insert their number (Some) or only read (None), each batch in one transaction.
struct Mixed {
    db: Db,
}

impl Engine for Mixed {
    type Row = Option<i64>;
    type Reader = Db;

    fn key(_: &Option<i64>) -> &str {
        "t"
    }

    fn apply(&mut self, rows: &[Option<i64>]) -> anyhow::Result<usize> {
        self.db.transaction(|db| {
            for row in rows {
                match row {
                    Some(n) => db.query(
                        "INSERT INTO t (n) VALUES (?1)",
                        [Value::from_i64(*n)],
                        |_| Ok(true),
                    )?,
                    None => db.query("SELECT count(*) FROM t", Vec::new(), |_| Ok(true))?,
                }
            }
            Ok(rows.len())
        })
    }

    fn flusher(&self) -> Option<Flush> {
        Some(self.db.flusher())
    }
}

/// AC-T1.1.1, docs/design/store.md decision 15 (kiln goal f4b650c4): do a batch that only reads with
/// every flush of the disk held; expect it told durable at once, no flush held for it. A commit
/// that wrote nothing leaves the log as the last flush made it, so the flush its ticket waits for
/// has nothing to sync. Then a write: its flush is held until let go, so the hold holds.
#[test]
fn a_batch_that_wrote_nothing_is_durable_with_every_flush_held() {
    let until = || Instant::now() + Duration::from_secs(30);
    let disk = Disk::new(1, HashMap::new());
    let db = Db::open_with(std::path::Path::new(PATH), disk.clone()).unwrap();
    db.exec("CREATE TABLE t (n INTEGER)").unwrap();
    let store = Store::start(Mixed { db }, Config::default()).unwrap();
    store
        .durable(&store.put(vec![Some(1)]).unwrap(), until())
        .unwrap();
    disk.hold();
    // The 30 s budget is a hang guard: a flush this batch needed would wait for the hold forever.
    store
        .durable(&store.put(vec![None]).unwrap(), until())
        .unwrap();
    assert_eq!(
        disk.holding.0.lock().unwrap().1,
        0,
        "a flush was held for a batch that only read"
    );
    let write = store.put(vec![Some(2)]).unwrap();
    disk.held(1);
    disk.let_go();
    store.durable(&write, until()).unwrap();
}

/// T-T1.1.1, a sample run in every gate: a power cut at a change drawn from each seed's workload,
/// and the restart keeps every acknowledged batch, no refused one, and every other whole or not at
/// all.
#[test]
fn a_power_cut_at_any_change_loses_nothing_acknowledged_and_tears_no_batch() {
    seeds(200, 1);
}

/// The seeds AMPHORA_CRASH_SEEDS names (comma-separated), each told whatever it found: to look into
/// one (`cargo test --release --test crash -- --ignored listed`).
#[test]
#[ignore = "the seeds AMPHORA_CRASH_SEEDS names"]
fn listed() {
    let seeds = std::env::var("AMPHORA_CRASH_SEEDS").unwrap_or_default();
    for seed in seeds
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
    {
        match one_seed(seed) {
            Ok(seen) => eprintln!(
                "seed {seed}: ok (cut midway: {}, acknowledged {}, refused {})",
                seen.cut_midway, seen.acknowledged, seen.refused
            ),
            Err(e) => eprintln!("seed {seed}: {e}"),
        }
    }
}

/// T-T1.1.1 in full: 10,000 seeds (`cargo test --release --test crash -- --ignored`; from another
/// seed with AMPHORA_CRASH_FROM).
#[test]
#[ignore = "10,000 seeds: run on its own, in release"]
fn ten_thousand_power_cuts() {
    let from = std::env::var("AMPHORA_CRASH_FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    seeds(10_000, from);
}

/// T-T1.12.1's database: 300 rows of `t`, made durable.
const ONLINE_PATH: &str = "online.db";
const ONLINE_ROWS: i64 = 300;
/// The backfill's batch in this test (a test's workload size, not Amphora's: its driver learns
/// its own, `online::Sizer`).
const ONLINE_BATCH: u64 = 16;

fn online_plan() -> amphora::database::online::Plan {
    amphora::database::online::Plan::parse(&amphora_api::Migration {
        name: "ix".into(),
        sql: "CREATE INDEX ix ON t (a)".into(),
    })
    .unwrap()
}

/// Each step in its own transaction, made durable as the writer's pipeline makes it before its
/// caller hears; a write between steps, and one after the last, so some cuts fall after the
/// cutover is durable. Stops at the first failure (the power going out).
fn online_run(db: &mut Db) -> anyhow::Result<()> {
    online_steps(db, &online_plan())
}

/// [`online_run`]'s steps and writes for any online change.
fn online_steps(db: &mut Db, plan: &amphora::database::online::Plan) -> anyhow::Result<()> {
    for i in 0..10_000i64 {
        let stepped =
            db.transaction(|db| amphora::database::online::step(db, plan, ONLINE_BATCH))?;
        db.flusher()().map_err(|e| anyhow::anyhow!("{e}"))?;
        let ended = matches!(
            stepped,
            amphora::database::online::Stepped::Public { .. }
                | amphora::database::online::Stepped::Already
        );
        db.transaction(|db| {
            db.query(
                "INSERT INTO t (a) VALUES (?1)",
                [Value::from_i64(i % 31)],
                |_| Ok(true),
            )?;
            db.query(
                "DELETE FROM t WHERE id = ?1",
                [Value::from_i64(i * 11 + 3)],
                |_| Ok(true),
            )
        })?;
        db.flusher()().map_err(|e| anyhow::anyhow!("{e}"))?;
        if ended {
            return Ok(());
        }
    }
    anyhow::bail!("the change never ended")
}

fn online_baseline() -> HashMap<String, Vec<u8>> {
    let disk = Disk::new(0, HashMap::new());
    let mut db = Db::open_with(std::path::Path::new(ONLINE_PATH), disk.clone()).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER); CREATE TABLE __amphora_migrations (name TEXT PRIMARY KEY, sql TEXT NOT NULL)").unwrap();
    db.transaction(|db| {
        for i in 0..ONLINE_ROWS {
            db.query(
                "INSERT INTO t (a) VALUES (?1)",
                [Value::from_i64(i % 31)],
                |_| Ok(true),
            )?;
        }
        Ok(())
    })
    .unwrap();
    db.flusher()().unwrap();
    drop(db);
    disk.image()
}

/// The schema after a restart: the old one (no `ix` readable, its change unfinished or not begun)
/// or the new (`ix` public, its migration recorded, every row's entry in it). Never both or
/// neither: a public `ix` without its migration, or a recorded migration without `ix`.
fn online_check(db: &mut Db) -> Result<bool, String> {
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}"))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let migrated = count(
        db,
        "SELECT count(*) FROM __amphora_migrations WHERE name = 'ix'",
        vec![],
    )? == 1;
    let readable = count(
        db,
        "SELECT count(*) FROM t INDEXED BY ix WHERE a >= 0",
        vec![],
    );
    match (migrated, readable) {
        (true, Ok(indexed)) => {
            let all = count(db, "SELECT count(*) FROM t NOT INDEXED", vec![])?;
            if indexed != all {
                return Err(format!("ix is public with {indexed} of {all} rows"));
            }
            Ok(true)
        }
        (false, Err(_)) => Ok(false),
        (true, Err(e)) => Err(format!("the migration is recorded, ix isn't readable: {e}")),
        (false, Ok(_)) => Err("ix is readable, its migration unrecorded".into()),
    }
}

/// T-T1.12.1: a power cut at every change the online build of `ix` makes (each step, each backfill
/// batch, each write between them, each flush), then the restart. Expect: the old schema or the
/// new, never both or neither, the integrity check passing; and the change, carried on, ends with
/// `ix` public and whole.
#[test]
fn t_t1_12_1_a_power_cut_at_every_step_of_an_online_index_leaves_old_or_new() {
    let baseline = online_baseline();
    let path = std::path::Path::new(ONLINE_PATH);
    // The whole run's changes, uncut: every crash point is one of them.
    let disk = Disk::new(0, baseline.clone());
    let mut db = Db::open_with(path, disk.clone()).unwrap();
    disk.arm(u64::MAX);
    online_run(&mut db).unwrap();
    let changes = disk.changes.load(Ordering::SeqCst);
    drop(db);
    assert!(changes > 50, "the run made only {changes} changes");
    let (mut old, mut new) = (0u64, 0u64);
    for at in 1..=changes {
        let disk = Disk::new(at, baseline.clone());
        let mut db = Db::open_with(path, disk.clone()).unwrap();
        disk.arm(at);
        let ran = online_run(&mut db);
        drop(db);
        let image = disk.image();
        let mut db = Db::open_with(path, Disk::new(at, image)).unwrap();
        match online_check(&mut db) {
            Ok(true) => new += 1,
            Ok(false) => old += 1,
            Err(e) => panic!("cut at change {at} of {changes} ({ran:?}): {e}"),
        }
        online_run(&mut db).unwrap_or_else(|e| panic!("cut at change {at}: carrying on: {e:#}"));
        assert_eq!(
            online_check(&mut db),
            Ok(true),
            "cut at change {at}: carried on"
        );
    }
    assert!(
        old > 0 && new > 0,
        "old schema after {old} cuts, new after {new}"
    );
}

/// T-T1.12.1's column: `c` added online to the same 300 rows, a write before and after, each
/// made durable, stopping at the first failure (the power going out).
fn column_run(db: &mut Db) -> anyhow::Result<()> {
    let plan = amphora::database::online::Plan::parse(&amphora_api::Migration {
        name: "c".into(),
        sql: "ALTER TABLE t ADD COLUMN c INTEGER NOT NULL DEFAULT 7 CHECK (c >= 0)".into(),
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let write = |db: &mut Db, i: i64| -> anyhow::Result<()> {
        db.transaction(|db| {
            db.query(
                "INSERT INTO t (a) VALUES (?1)",
                [Value::from_i64(i % 31)],
                |_| Ok(true),
            )?;
            db.query(
                "DELETE FROM t WHERE id = ?1",
                [Value::from_i64(i * 11 + 3)],
                |_| Ok(true),
            )
        })?;
        db.flusher()().map_err(|e| anyhow::anyhow!("{e}"))
    };
    write(db, 1)?;
    let stepped = db.transaction(|db| amphora::database::online::step(db, &plan, ONLINE_BATCH))?;
    db.flusher()().map_err(|e| anyhow::anyhow!("{e}"))?;
    write(db, 2)?;
    match stepped {
        amphora::database::online::Stepped::Public { .. }
        | amphora::database::online::Stepped::Already => Ok(()),
        other => anyhow::bail!("a column's change stepped {other:?}"),
    }
}

/// The schema after a restart: the old one (no `c`, its migration unrecorded) or the new (`c` on
/// every row, its migration recorded). Never a column without its record or a record without it.
fn column_check(db: &mut Db) -> Result<bool, String> {
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}"))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let migrated = count(
        db,
        "SELECT count(*) FROM __amphora_migrations WHERE name = 'c'",
        vec![],
    )? == 1;
    let column = count(
        db,
        "SELECT count(*) FROM pragma_table_info('t') WHERE name = 'c'",
        vec![],
    )? == 1;
    match (migrated, column) {
        (true, true) => {
            let unset = count(db, "SELECT count(*) FROM t WHERE c IS NOT 7", vec![])?;
            if unset != 0 {
                return Err(format!("{unset} rows read c other than its default"));
            }
            Ok(true)
        }
        (false, false) => Ok(false),
        (true, false) => Err("the migration is recorded, c isn't there".into()),
        (false, true) => Err("c is there, its migration unrecorded".into()),
    }
}

/// T-T1.12.1, a column: a power cut at every change adding `c` online makes (the write before,
/// the change, the write after, each flush), then the restart. Expect: the old schema or the new,
/// never both or neither, the integrity check passing; and the change, carried on, ends with `c`.
#[test]
fn t_t1_12_1_a_power_cut_at_every_step_of_an_online_column_leaves_old_or_new() {
    let baseline = online_baseline();
    let path = std::path::Path::new(ONLINE_PATH);
    let disk = Disk::new(0, baseline.clone());
    let mut db = Db::open_with(path, disk.clone()).unwrap();
    disk.arm(u64::MAX);
    column_run(&mut db).unwrap();
    let changes = disk.changes.load(Ordering::SeqCst);
    drop(db);
    assert!(changes > 2, "the run made only {changes} changes");
    let (mut old, mut new) = (0u64, 0u64);
    for at in 1..=changes {
        let disk = Disk::new(at, baseline.clone());
        let mut db = Db::open_with(path, disk.clone()).unwrap();
        disk.arm(at);
        let ran = column_run(&mut db);
        drop(db);
        let image = disk.image();
        let mut db = Db::open_with(path, Disk::new(at, image)).unwrap();
        match column_check(&mut db) {
            Ok(true) => new += 1,
            Ok(false) => old += 1,
            Err(e) => panic!("cut at change {at} of {changes} ({ran:?}): {e}"),
        }
        column_run(&mut db).unwrap_or_else(|e| panic!("cut at change {at}: carrying on: {e:#}"));
        assert_eq!(
            column_check(&mut db),
            Ok(true),
            "cut at change {at}: carried on"
        );
    }
    assert!(
        old > 0 && new > 0,
        "old schema after {old} cuts, new after {new}"
    );
}

/// T-T1.12.1's constraint: NOT NULL and CHECK added online to `a` of the same 300 rows, each
/// step and each write between them made durable, stopping at the first failure.
fn constraint_run(db: &mut Db) -> anyhow::Result<()> {
    let plan = amphora::database::online::Plan::parse(&amphora_api::Migration {
        name: "nn".into(),
        sql: "ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL CHECK (a >= 0)".into(),
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    online_steps(db, &plan)
}

/// The schema after a restart: the old one (its migration unrecorded; `a` without the constraint,
/// or with it still marked unvalidated) or the new (its migration recorded, `a` declared with it,
/// no mark, no row breaking it). Never a constraint public without its record, or a record
/// without the constraint public.
fn constraint_check(db: &mut Db) -> Result<bool, String> {
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}"))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let migrated = count(
        db,
        "SELECT count(*) FROM __amphora_migrations WHERE name = 'nn'",
        vec![],
    )? == 1;
    let declared = count(
        db,
        "SELECT count(*) FROM sqlite_schema WHERE name = 't' AND sql LIKE '%NOT NULL%' AND sql LIKE '%CHECK%'",
        vec![],
    )? == 1;
    let kept = count(
        db,
        "SELECT count(*) FROM sqlite_schema WHERE name = '__amphora_constraint_states'",
        vec![],
    )? == 1;
    let marked = kept
        && count(
            db,
            "SELECT count(*) FROM __amphora_constraint_states WHERE tbl = 't' AND col = 'a'",
            vec![],
        )? == 1;
    match (migrated, declared && !marked) {
        (true, true) => {
            let broken = count(
                db,
                "SELECT count(*) FROM t WHERE a IS NULL OR a < 0",
                vec![],
            )?;
            if broken != 0 {
                return Err(format!("{broken} rows break the public constraint"));
            }
            Ok(true)
        }
        (false, false) => Ok(false),
        (true, false) => Err(format!(
            "the migration is recorded, the constraint isn't public (declared {declared}, marked {marked})"
        )),
        (false, true) => Err("the constraint is public, its migration unrecorded".into()),
    }
}

/// T-T1.12.1, a constraint: a power cut at every change adding NOT NULL and CHECK to `a` online
/// makes (each step, each validation batch, each write between them, each flush), then the
/// restart. Expect: the old schema or the new, never both or neither, the integrity check
/// passing; and the change, carried on, ends with the constraint public.
#[test]
fn t_t1_12_1_a_power_cut_at_every_step_of_an_online_constraint_leaves_old_or_new() {
    let baseline = online_baseline();
    let path = std::path::Path::new(ONLINE_PATH);
    let disk = Disk::new(0, baseline.clone());
    let mut db = Db::open_with(path, disk.clone()).unwrap();
    disk.arm(u64::MAX);
    constraint_run(&mut db).unwrap();
    let changes = disk.changes.load(Ordering::SeqCst);
    drop(db);
    assert!(changes > 20, "the run made only {changes} changes");
    let (mut old, mut new) = (0u64, 0u64);
    for at in 1..=changes {
        let disk = Disk::new(at, baseline.clone());
        let mut db = Db::open_with(path, disk.clone()).unwrap();
        disk.arm(at);
        let ran = constraint_run(&mut db);
        drop(db);
        let image = disk.image();
        let mut db = Db::open_with(path, Disk::new(at, image)).unwrap();
        match constraint_check(&mut db) {
            Ok(true) => new += 1,
            Ok(false) => old += 1,
            Err(e) => panic!("cut at change {at} of {changes} ({ran:?}): {e}"),
        }
        constraint_run(&mut db)
            .unwrap_or_else(|e| panic!("cut at change {at}: carrying on: {e:#}"));
        assert_eq!(
            constraint_check(&mut db),
            Ok(true),
            "cut at change {at}: carried on"
        );
    }
    assert!(
        old > 0 && new > 0,
        "old schema after {old} cuts, new after {new}"
    );
}

/// T-T1.12.1's drop: 300 rows of `t` with a column `b` (its default in every row), made durable.
fn drop_baseline() -> HashMap<String, Vec<u8>> {
    let disk = Disk::new(0, HashMap::new());
    let mut db = Db::open_with(std::path::Path::new(ONLINE_PATH), disk.clone()).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT NOT NULL DEFAULT 'kept'); CREATE TABLE __amphora_migrations (name TEXT PRIMARY KEY, sql TEXT NOT NULL)").unwrap();
    db.transaction(|db| {
        for i in 0..ONLINE_ROWS {
            db.query(
                "INSERT INTO t (a) VALUES (?1)",
                [Value::from_i64(i % 31)],
                |_| Ok(true),
            )?;
        }
        Ok(())
    })
    .unwrap();
    db.flusher()().unwrap();
    drop(db);
    disk.image()
}

/// T-T1.12.1's drop: `b` dropped online from the same rows, each step and each write between
/// them made durable, stopping at the first failure.
fn drop_run(db: &mut Db) -> anyhow::Result<()> {
    let plan = amphora::database::online::Plan::parse(&amphora_api::Migration {
        name: "dc".into(),
        sql: "ALTER TABLE t DROP COLUMN b".into(),
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    online_steps(db, &plan)
}

/// The schema after a restart: the old one (`b` found by name, its migration unrecorded) or the
/// new (`b` found by no name, `*` passing it; recorded once the clear ended, unrecorded while it
/// is under way). Never `b` found by name with its drop recorded, nor a record of the drop with
/// its change still kept. Returns whether `b` is dropped, and whether the drop is recorded.
fn drop_check(db: &mut Db) -> Result<(bool, bool), String> {
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .map_err(|e| format!("integrity_check: {e:#}"))?;
    if check != "ok" {
        return Err(format!("integrity_check: {check}"));
    }
    let migrated = count(
        db,
        "SELECT count(*) FROM __amphora_migrations WHERE name = 'dc'",
        vec![],
    )? == 1;
    let named = db.column_count("SELECT b FROM t").is_ok();
    let star = db
        .column_count("SELECT * FROM t")
        .map_err(|e| format!("SELECT *: {e:#}"))?;
    if named != (star == 3) {
        return Err(format!("b found by name {named}, yet * has {star} columns"));
    }
    let kept = count(
        db,
        "SELECT count(*) FROM sqlite_schema WHERE name = '__amphora_schema_changes'",
        vec![],
    )? == 1
        && count(
            db,
            "SELECT count(*) FROM __amphora_schema_changes WHERE name = 'dc'",
            vec![],
        )? == 1;
    match (migrated, named, kept) {
        (true, true, _) => Err("the drop is recorded, b is found by name".into()),
        (true, false, true) => Err("the drop is recorded and its change still kept".into()),
        (false, false, false) => Err("b is dropped with neither its change nor its record".into()),
        (false, true, true) => Err("b is found by name while its drop is under way".into()),
        (migrated, named, _) => Ok((!named, migrated)),
    }
}

/// T-T1.12.1, a drop: a power cut at every change dropping `b` online makes (the switch-over, each
/// clearing batch, each write between them, each flush), then the restart. Expect: the old schema
/// or the new, never both or neither, the integrity check passing; and the change, carried on,
/// ends with `b` dropped and its drop recorded.
#[test]
fn t_t1_12_1_a_power_cut_at_every_step_of_an_online_drop_leaves_old_or_new() {
    let baseline = drop_baseline();
    let path = std::path::Path::new(ONLINE_PATH);
    let disk = Disk::new(0, baseline.clone());
    let mut db = Db::open_with(path, disk.clone()).unwrap();
    disk.arm(u64::MAX);
    drop_run(&mut db).unwrap();
    let changes = disk.changes.load(Ordering::SeqCst);
    drop(db);
    assert!(changes > 20, "the run made only {changes} changes");
    let (mut old, mut new) = (0u64, 0u64);
    for at in 1..=changes {
        let disk = Disk::new(at, baseline.clone());
        let mut db = Db::open_with(path, disk.clone()).unwrap();
        disk.arm(at);
        let ran = drop_run(&mut db);
        drop(db);
        let image = disk.image();
        let mut db = Db::open_with(path, Disk::new(at, image)).unwrap();
        match drop_check(&mut db) {
            Ok((true, _)) => new += 1,
            Ok((false, _)) => old += 1,
            Err(e) => panic!("cut at change {at} of {changes} ({ran:?}): {e}"),
        }
        drop_run(&mut db).unwrap_or_else(|e| panic!("cut at change {at}: carrying on: {e:#}"));
        assert_eq!(
            drop_check(&mut db),
            Ok((true, true)),
            "cut at change {at}: carried on"
        );
    }
    assert!(
        old > 0 && new > 0,
        "old schema after {old} cuts, new after {new}"
    );
}
