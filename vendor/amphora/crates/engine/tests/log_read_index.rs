//! The log's frame index and header read without the shared locks (G3 step 2; Amphora's
//! `docs/design/store.md`, decision 40), under crashes: a crash at every write of a checkpoint and
//! of a log restart, then the database opened again and read through each lock-free way.

use amphora_engine::io::{FileId, FileSyncType};
use amphora_engine::storage::log_index::LogReadIndex;
use amphora_engine::{
    Buffer, CheckpointMode, Clock, Completion, ConnectOpts, Connection, Database, DatabaseOpts,
    File, LimboError, MonotonicInstant, Numeric, OpenFlags, OpenOptions, PlatformIO, SqliteDialect,
    Value, WallClockInstant, IO,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Writes counted once armed; every one past `budget` fails, as if the process ended there.
#[derive(Default)]
struct Crash {
    armed: AtomicBool,
    budget: AtomicU64,
    writes: AtomicU64,
}

impl Crash {
    fn write(&self) -> amphora_engine::Result<()> {
        if !self.armed.load(Ordering::Acquire) {
            return Ok(());
        }
        let done = self.writes.fetch_add(1, Ordering::AcqRel);
        if done >= self.budget.load(Ordering::Acquire) {
            return Err(LimboError::InternalError("crashed here".into()));
        }
        Ok(())
    }
}

struct Crashing {
    inner: Arc<PlatformIO>,
    crash: Arc<Crash>,
}

struct CrashingFile {
    inner: Arc<dyn File>,
    crash: Arc<Crash>,
}

impl File for CrashingFile {
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
        self.crash.write()?;
        self.inner.pwrite(pos, buffer, c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.crash.write()?;
        self.inner.pwritev(pos, buffers, c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> amphora_engine::Result<Completion> {
        self.crash.write()?;
        self.inner.sync(c, sync_type)
    }

    fn size(&self) -> amphora_engine::Result<u64> {
        self.inner.size()
    }

    fn truncate(&self, len: u64, c: Completion) -> amphora_engine::Result<Completion> {
        self.crash.write()?;
        self.inner.truncate(len, c)
    }
}

impl Clock for Crashing {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for Crashing {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        direct: bool,
    ) -> amphora_engine::Result<Arc<dyn File>> {
        Ok(Arc::new(CrashingFile {
            inner: self.inner.open_file(path, flags, direct)?,
            crash: self.crash.clone(),
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

fn open_with(io: Arc<dyn IO>, path: &Path) -> Arc<Database> {
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    Database::open(io, path.to_str().unwrap(), options).unwrap()
}

fn connect(db: &Arc<Database>) -> Arc<Connection> {
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn
}

type Rows = BTreeMap<i64, Vec<u8>>;

fn body(id: i64, version: u8) -> Vec<u8> {
    (0..600)
        .map(|i| (id as u8).wrapping_mul(31) ^ (i as u8) ^ version)
        .collect()
}

fn put(
    conn: &Arc<Connection>,
    rows: &mut Rows,
    ids: &[i64],
    version: u8,
) -> amphora_engine::Result<()> {
    conn.execute("BEGIN")?;
    for &id in ids {
        let hex: String = body(id, version)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        conn.execute(format!("INSERT OR REPLACE INTO t VALUES ({id}, x'{hex}')"))?;
    }
    conn.execute("COMMIT")?;
    for &id in ids {
        rows.insert(id, body(id, version));
    }
    Ok(())
}

fn table(conn: &Arc<Connection>) -> Rows {
    let mut stmt = conn.prepare("SELECT id, body FROM t ORDER BY id").unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| {
            let id = match &row[0] {
                Value::Numeric(Numeric::Integer(id)) => *id,
                other => panic!("id {other:?}"),
            };
            (id, row[1].to_blob().unwrap().to_vec())
        })
        .collect()
}

fn integrity(conn: &Arc<Connection>) {
    let rows = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(
        matches!(&rows[..], [row] if matches!(&row[0], Value::Text(t) if t.as_str() == "ok")),
        "integrity_check: {rows:?}"
    );
}

/// The database's commits before the crash point is armed, then the checkpoints and log restarts
/// it is armed over: each step's rows as committed, and how many steps returned.
fn run(path: &Path, crash: &Arc<Crash>) -> (Vec<Rows>, usize) {
    let io = Arc::new(Crashing {
        inner: Arc::new(PlatformIO::new().unwrap()),
        crash: crash.clone(),
    });
    let db = open_with(io, path);
    let conn = connect(&db);
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
        .unwrap();
    let mut rows = Rows::new();
    for round in 0..4u8 {
        let ids: Vec<i64> = (0..30).map(|i| i64::from(round) * 7 + i).collect();
        put(&conn, &mut rows, &ids, round).unwrap();
    }
    let mut states = vec![rows.clone()];
    crash.armed.store(true, Ordering::Release);
    let mut steps = 0;
    let truncate = CheckpointMode::Truncate {
        upper_bound_inclusive: None,
    };
    let passive = CheckpointMode::Passive {
        upper_bound_inclusive: None,
    };
    // A checkpoint that restarts the log, a commit into the restarted log, a checkpoint that leaves
    // it whole, and a commit that restarts it from its first frame.
    let outcome = (|| -> amphora_engine::Result<()> {
        conn.checkpoint(truncate)?;
        steps += 1;
        states.push(rows.clone());
        put(&conn, &mut rows, &[3, 50, 200], 9)?;
        steps += 1;
        states.push(rows.clone());
        conn.checkpoint(passive)?;
        steps += 1;
        states.push(rows.clone());
        put(&conn, &mut rows, &[4, 51, 201], 10)?;
        steps += 1;
        states.push(rows.clone());
        Ok(())
    })();
    // A step that failed may have left its commit in the file whole: the state after it counts.
    if outcome.is_err() {
        let mut after = rows.clone();
        match steps {
            1 => put_model(&mut after, &[3, 50, 200], 9),
            3 => put_model(&mut after, &[4, 51, 201], 10),
            _ => {}
        }
        states.push(after);
    }
    // The crash: the files as the writes before it left them, copied before anything else runs.
    let copy = path.with_extension("crashed");
    for (from, to) in [(path.to_path_buf(), copy.clone()), (wal(path), wal(&copy))] {
        if from.exists() {
            std::fs::copy(&from, &to).unwrap();
        }
    }
    // The versioned storage's files, with its switch on (G7 step 2, `AC-NS.2.1`): its directory
    // beside the database's.
    let (from, to) = (versioned(path), versioned(&copy));
    if from.is_dir() {
        std::fs::create_dir_all(&to).unwrap();
        for entry in std::fs::read_dir(&from).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
    std::mem::forget(conn);
    std::mem::forget(db);
    (states, steps)
}

fn put_model(rows: &mut Rows, ids: &[i64], version: u8) {
    for &id in ids {
        rows.insert(id, body(id, version));
    }
}

fn wal(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-wal", path.display()))
}

fn versioned(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-versioned", path.display()))
}

/// Do (T-G3.2.2): for each lock-free way, crash at every write of a TRUNCATE checkpoint (which
/// restarts the log), of a commit into the restarted log, of a PASSIVE checkpoint and of a commit
/// that restarts the log from its first frame; open the files as the crash left them and read every
/// row, by a scan and one at a time. Expect: the rows as some state from the last step that
/// returned on, every row its last committed version, and a database that checks whole.
#[test]
fn g3_2_reads_after_a_crash_in_a_checkpoint_or_log_restart_find_the_last_commit() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [
        LogReadIndex::Filtered,
        LogReadIndex::Published,
        LogReadIndex::Optimistic,
    ] {
        LogReadIndex::set_process_default(mode);
        // The writes the steps take when nothing crashes.
        let whole = Arc::new(Crash::default());
        whole.budget.store(u64::MAX, Ordering::Release);
        let (_, steps) = run(&dir.path().join(format!("{mode:?}-whole.db")), &whole);
        assert_eq!(steps, 4);
        let writes = whole.writes.load(Ordering::Acquire);
        assert!(writes > 4, "{mode:?}: {writes} writes");
        for at in 0..writes {
            let path = dir.path().join(format!("{mode:?}-{at}.db"));
            let crash = Arc::new(Crash::default());
            crash.budget.store(at, Ordering::Release);
            let (states, steps) = run(&path, &crash);
            let db = open_with(
                Arc::new(PlatformIO::new().unwrap()),
                &path.with_extension("crashed"),
            );
            let conn = connect(&db);
            let read = table(&conn);
            assert!(
                states[steps..].contains(&read),
                "{mode:?}, crashed at write {at} of {writes}, {steps} steps returned: read {} rows",
                read.len()
            );
            for (&id, expected) in &read {
                let mut stmt = conn
                    .prepare(format!("SELECT body FROM t WHERE id = {id}"))
                    .unwrap();
                let one = stmt.run_collect_rows().unwrap();
                assert_eq!(one.len(), 1, "{mode:?} at {at}: row {id}");
                assert_eq!(
                    &one[0][0].to_blob().unwrap().to_vec(),
                    expected,
                    "{mode:?} at {at}: row {id}"
                );
            }
            integrity(&conn);
        }
    }
}
