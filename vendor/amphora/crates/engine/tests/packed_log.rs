//! The packed log (`storage/packed.rs`; Amphora's `docs/design/store.md`, decisions 16 and 17):
//! what a packed log holds, whole images and deltas, reads back the same through each way the
//! engine reads its log (a connection with nothing cached, the checkpoint, recovery on open), whole
//! commits or none survive a cut, and a record whose checksums hold but that doesn't decode is
//! refused rather than read (AC-T1.4.4, AC-T1.3.1). The same of a mixed log (decision 37), whose
//! spills are page frames written over and whose commits are records; and of decision 39's, whose
//! images of pages new to a chain are page frames and whose chains' entries are records of one
//! slot, so a read past the cache reads one frame for each entry of its page's chain.

use amphora_engine::io::{FileId, FileSyncType};
use amphora_engine::{
    Buffer, CheckpointMode, Clock, Completion, ConnectOpts, Connection, Database, DatabaseOpts,
    File, LimboError, LogFormat, MonotonicInstant, Numeric, OpenFlags, OpenOptions, PlatformIO,
    SqliteDialect, Value, WallClockInstant, IO,
};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// These tests exercise the pager's log format (its page frames, records and deltas), which the
/// versioned storage doesn't write: with its switch on (`AMPHORA_STORAGE=versioned`, G7 step 2,
/// `AC-NS.2.1`) they say so and pass over, and aren't ported. The versioned storage's redo log is
/// tested in `storage::versioned`.
fn pager_only() -> bool {
    let on = amphora_engine::storage::versioned::switch::asked();
    if on {
        eprintln!("pager-only: this test exercises the pager's log format (its page frames, records and deltas); the versioned storage writes none");
    }
    on
}

/// The log header's file format for a packed generation begun now (chains ended by their bytes,
/// decision 17), a mixed one begun now (decision 39, its chains ended so too), a mixed one begun
/// under decision 37 and SQLite's one page a frame.
const PACKED_FORMAT: u32 = 0x414d_5004;
const MIXED_FORMAT: u32 = 0x414d_5007;
const MIXED_RECORDS_FORMAT: u32 = 0x414d_5003;
/// The packed format whose chains end at 8 deltas in a row, as binaries before 2026-11-11 begin
/// generations (decision 17).
const PACKED_EIGHT_FORMAT: u32 = 0x414d_5002;
const PAGE_FORMAT: u32 = 3_007_000;
/// A mixed generation's mark of a record's first slot.
const RECORD_HEAD: u32 = 0x8000_0000;
const HEADER: usize = 32;
const FRAME_HEADER: usize = 24;
const PAGE: usize = 4096;
const CONTINUATION: u32 = u32::MAX;

fn open(path: &Path) -> Arc<Database> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    Database::open(io, path.to_str().unwrap(), options).unwrap()
}

fn connect(db: &Arc<Database>) -> Arc<Connection> {
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

fn wal_format(path: &Path) -> u32 {
    let wal = std::fs::read(wal_path(path)).unwrap();
    u32::from_be_bytes(wal[4..8].try_into().unwrap())
}

/// A row's body: `len` bytes from `id`, patterned (compressible) or a xorshift's (not).
fn body(id: i64, len: usize, noise: bool) -> Vec<u8> {
    let mut x = (id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|i| {
            if noise {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            } else {
                (id as u8).wrapping_add((i % 13) as u8)
            }
        })
        .collect()
}

/// Inserts rows `ids` with bodies of `len` bytes in one transaction, returning them.
fn insert(
    conn: &Arc<Connection>,
    ids: std::ops::Range<i64>,
    len: usize,
    noise: bool,
) -> Vec<(i64, Vec<u8>)> {
    conn.execute("BEGIN").unwrap();
    let rows = insert_in(conn, ids, len, noise);
    conn.execute("COMMIT").unwrap();
    rows
}

/// Inserts rows in the transaction that is open.
fn insert_in(
    conn: &Arc<Connection>,
    ids: std::ops::Range<i64>,
    len: usize,
    noise: bool,
) -> Vec<(i64, Vec<u8>)> {
    let mut stmt = conn
        .prepare("INSERT INTO t (id, body) VALUES (?, ?)")
        .unwrap();
    let mut rows = Vec::new();
    for id in ids {
        let b = body(id, len, noise);
        stmt.reset().unwrap();
        stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id))
            .unwrap();
        stmt.bind_at(NonZero::new(2).unwrap(), Value::from_slice(&b).unwrap())
            .unwrap();
        stmt.run_ignore_rows().unwrap();
        rows.push((id, b));
    }
    rows
}

fn table(conn: &Arc<Connection>) -> Vec<(i64, Vec<u8>)> {
    let mut stmt = conn.prepare("SELECT id, body FROM t ORDER BY id").unwrap();
    stmt.run_collect_rows()
        .unwrap_or_else(|e| panic!("reading the table: {e}"))
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
    assert_eq!(rows.len(), 1, "integrity_check: {rows:?}");
    assert!(
        matches!(&rows[0][0], Value::Text(t) if t.as_str() == "ok"),
        "integrity_check: {rows:?}"
    );
}

fn truncate(conn: &Arc<Connection>) {
    conn.checkpoint(CheckpointMode::Truncate {
        upper_bound_inclusive: None,
    })
    .unwrap();
}

/// Do: commit to a packed log commits of one page, of a few records, and of more pages than one
/// prepared batch takes (1,024), bodies that compress and overflow pages of noise that don't; read
/// them on a fresh connection, after a checkpoint, in the log's next generation, and after opening
/// the database again. Expect: records both compressed and stored as they are, every row as
/// written each time, and a database that checks whole.
#[test]
fn what_a_packed_log_holds_reads_back_through_every_path() {
    if pager_only() {
        return;
    }
    reads_back_through_every_path(LogFormat::Packed, PACKED_FORMAT);
}

/// The same of a mixed log (decision 37), whose commits are records too.
#[test]
fn what_a_mixed_log_holds_reads_back_through_every_path() {
    if pager_only() {
        return;
    }
    reads_back_through_every_path(LogFormat::Mixed, MIXED_FORMAT);
}

fn reads_back_through_every_path(format: LogFormat, file_format: u32) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.db");
    let mut want = Vec::new();
    {
        let db = open(&path);
        db.wal_format(format).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want.extend(insert(&conn, 0..10, 100, false));
        want.extend(insert(&conn, 10..400, 3000, false));
        want.extend(insert(&conn, 400..1600, 3000, true));
        // Rows of 600 KB: records of nothing but their overflow pages, which don't compress.
        want.extend(insert(&conn, 1600..1602, 600_000, true));
        assert_eq!(wal_format(&path), file_format);
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let stored: Vec<u32> = records(&wal)
            .iter()
            .map(|(head, _)| {
                let at = head + FRAME_HEADER;
                u32::from_be_bytes(wal[at..at + 4].try_into().unwrap())
            })
            .collect();
        if format == LogFormat::Packed {
            assert!(stored.iter().any(|n| *n > 0), "no record compressed");
            assert!(stored.contains(&0), "no record stored as it is: {stored:?}");
        } else {
            // A mixed generation writes new rows' pages, new to the log's chains, as page frames,
            // and the changes to pages logged before (the schema's, the tree's inner pages) as
            // records of one slot (decision 39).
            let (records, frames) = records_and_frames(&wal);
            assert!(frames > 1600, "{frames} page frames for 1,602 rows' pages");
            assert!(
                records.iter().all(|&(_, slots)| slots == 1),
                "records of more than one slot: {records:?}"
            );
        }

        // A connection with nothing cached reads every page from the log's records.
        assert_eq!(table(&connect(&db)), want);
        integrity(&connect(&db));

        // The checkpoint reads the records too, and the next generation takes the format as well.
        truncate(&conn);
        assert_eq!(table(&connect(&db)), want);
        want.extend(insert(&conn, 2000..2100, 3000, false));
        assert_eq!(wal_format(&path), file_format);
        assert_eq!(table(&connect(&db)), want);
    }
    // Opened again: recovery reads the log record by record.
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    integrity(&conn);
}

/// Do: commit to a packed log, close the database, open it again (its log's generation packed on
/// disk, though this process never asked for packing) and ask it to share its pages. Expect: the
/// sharing refused, as it is for a database that packs its log now, and every row read back
/// (decisions 20 and 26: a process's databases share pages by default, a packed log's don't).
#[test]
fn a_database_opened_on_a_packed_log_doesnt_share_its_pages() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed.db");
    let want = {
        let db = open(&path);
        db.wal_format(LogFormat::Packed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        let want = insert(&conn, 0..200, 3000, false);
        assert_eq!(wal_format(&path), PACKED_FORMAT);
        assert!(db.share_pages(1024).is_err(), "shared while packing");
        want
    };
    assert_eq!(wal_format(&path), PACKED_FORMAT);
    let db = open(&path);
    assert!(
        db.share_pages(1024).is_err(),
        "a database whose log on disk is packed shared its pages"
    );
    assert_eq!(table(&connect(&db)), want);
}

/// The entries of every record in a packed log with no restart behind it, in log order, as (page,
/// kind, depth, body bytes): kind 0 an image, 1 a delta.
fn entries(wal: &[u8]) -> Vec<(u32, u8, u8, usize)> {
    let frame = FRAME_HEADER + PAGE;
    let mut found = Vec::new();
    for (head, slots) in records(wal) {
        let payload: Vec<u8> = (0..slots as usize)
            .flat_map(|i| {
                let at = head + i * frame + FRAME_HEADER;
                wal[at..at + PAGE].iter().copied()
            })
            .collect();
        let stored = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
        let raw_len = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
        let raw = if stored == 0 {
            payload[8..8 + raw_len].to_vec()
        } else {
            let mut raw = vec![0u8; raw_len];
            lz4_flex::block::decompress_into(&payload[8..8 + stored], &mut raw).unwrap();
            raw
        };
        let count = u32::from_be_bytes(raw[0..4].try_into().unwrap()) as usize;
        for e in 0..count {
            let at = 4 + e * 12;
            let page = u32::from_be_bytes(raw[at..at + 4].try_into().unwrap());
            let len = u32::from_be_bytes(raw[at + 8..at + 12].try_into().unwrap()) as usize;
            found.push((page, raw[at + 4], raw[at + 5], len));
        }
    }
    found
}

/// Do: in a packed log, change a few bytes of the same rows commit after commit, thirty times;
/// read the rows on a fresh connection, after a checkpoint and after opening the database again.
/// Expect: deltas in the log, a page's chain running past eight deltas while its deltas' bytes stay
/// under a page and never reaching one (decision 17: the chain ends where its bytes would pass an
/// image's, no count of deltas), and every row as last written each time (AC-T1.4.4).
#[test]
fn pages_changed_again_and_again_are_logged_as_deltas_and_read_back() {
    if pager_only() {
        return;
    }
    deltas_read_back(LogFormat::Packed);
}

/// The same in a mixed log (decision 37).
#[test]
fn pages_changed_again_and_again_in_a_mixed_log_are_logged_as_deltas() {
    if pager_only() {
        return;
    }
    deltas_read_back(LogFormat::Mixed);
}

fn deltas_read_back(format: LogFormat) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deltas.db");
    let mut want: Vec<(i64, Vec<u8>)>;
    {
        let db = open(&path);
        db.wal_format(format).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want = insert(&conn, 0..300, 500, false);
        let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
        for round in 0..30u8 {
            conn.execute("BEGIN").unwrap();
            for row in want
                .iter_mut()
                .filter(|(id, _)| (*id as u8) % 3 == round % 3)
            {
                row.1[usize::from(round) * 7 % 500] = round;
                update.reset().unwrap();
                update
                    .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                    .unwrap();
                update
                    .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                    .unwrap();
                update.run_ignore_rows().unwrap();
            }
            conn.execute("COMMIT").unwrap();
            if round % 10 == 9 {
                assert_eq!(table(&connect(&db)), want, "after round {round}");
            }
        }
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let entries = entries(&wal);
        let deltas = entries.iter().filter(|(_, kind, _, _)| *kind == 1).count();
        let deepest = entries.iter().map(|(_, _, depth, _)| *depth).max().unwrap();
        assert!(
            deltas * 2 > entries.len(),
            "{deltas} deltas of {} entries",
            entries.len()
        );
        assert!(deepest > 8, "chains of small deltas end at {deepest}");
        // Each page's chain, in log order: its depths count up from its image, its deltas' bytes
        // stay under a page.
        let mut chains: std::collections::HashMap<u32, (u8, usize)> = Default::default();
        for (page, kind, depth, len) in &entries {
            let chain = chains.entry(*page).or_default();
            *chain = match kind {
                0 => (0, 0),
                _ => {
                    assert_eq!(*depth, chain.0 + 1, "page {page}'s depths");
                    (*depth, chain.1 + len)
                }
            };
            assert!(
                chain.1 < PAGE,
                "page {page}'s deltas took {} bytes",
                chain.1
            );
        }
        // Records that carry deltas are compressed, in either format.
        let compressed = records(&wal)
            .iter()
            .filter(|(head, _)| {
                let at = head + FRAME_HEADER;
                u32::from_be_bytes(wal[at..at + 4].try_into().unwrap()) > 0
            })
            .count();
        assert!(compressed > 0, "no record of deltas compressed");
        integrity(&connect(&db));
        truncate(&conn);
        assert_eq!(table(&connect(&db)), want);
        let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
        for row in want.iter_mut().take(50) {
            row.1[499] ^= 0xff;
            update.reset().unwrap();
            update
                .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                .unwrap();
            update
                .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                .unwrap();
            update.run_ignore_rows().unwrap();
        }
    }
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    integrity(&conn);
}

/// A xorshift's next number.
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn run(conn: &Arc<Connection>, sql: &str, params: Vec<Value>) {
    let mut stmt = conn.prepare(sql).unwrap();
    for (i, p) in params.into_iter().enumerate() {
        stmt.bind_at(NonZero::new(i + 1).unwrap(), p).unwrap();
    }
    stmt.run_ignore_rows().unwrap();
}

/// Do: in a packed log with a connection cache of 16 pages (so pages spill into the log and come
/// back), run transactions of random inserts, small changes to rows, deletes, savepoints, and
/// rollbacks to savepoints, checking a model of the table: a seed per run, eight runs. Expect:
/// after every commit and after opening the database again, exactly the rows the model kept (a
/// page whose last version in the log was rolled back away must not be the base of its next
/// delta).
#[test]
fn savepoints_rolled_back_amid_spills_keep_exactly_what_was_kept() {
    if pager_only() {
        return;
    }
    for format in [LogFormat::Pages, LogFormat::Packed, LogFormat::Mixed] {
        for seed in 1..=8u64 {
            savepoints_amid_spills(format, seed);
        }
    }
}

fn savepoints_amid_spills(format: LogFormat, seed: u64) {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("savepoints.db");
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut model: std::collections::BTreeMap<i64, Vec<u8>> = Default::default();
        {
            let db = open(&path);
            db.wal_format(format).unwrap();
            let conn = connect(&db);
            conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
                .unwrap();
            conn.execute("PRAGMA cache_size = 16").unwrap();
            for txn in 0..6 {
                conn.execute("BEGIN").unwrap();
                let mut working = model.clone();
                let mut saved: Vec<std::collections::BTreeMap<i64, Vec<u8>>> = Vec::new();
                for _ in 0..80 {
                    let id = (next(&mut x) % 300) as i64;
                    match next(&mut x) % 10 {
                        0..=3 => {
                            let b: Vec<u8> = (0..1500).map(|_| next(&mut x) as u8).collect();
                            run(
                                &conn,
                                "INSERT OR REPLACE INTO t (id, body) VALUES (?, ?)",
                                vec![Value::from_i64(id), Value::from_slice(&b).unwrap()],
                            );
                            working.insert(id, b);
                        }
                        4..=6 => {
                            if let Some(b) = working.get_mut(&id) {
                                let at = (next(&mut x) % 1500) as usize;
                                b[at] = b[at].wrapping_add(1);
                                run(
                                    &conn,
                                    "UPDATE t SET body = ? WHERE id = ?",
                                    vec![Value::from_slice(b).unwrap(), Value::from_i64(id)],
                                );
                            }
                        }
                        7 => {
                            conn.execute(format!("SAVEPOINT s{}", saved.len())).unwrap();
                            saved.push(working.clone());
                        }
                        8 => {
                            if let Some(kept) = saved.pop() {
                                let name = format!("s{}", saved.len());
                                conn.execute(format!("ROLLBACK TO {name}")).unwrap();
                                conn.execute(format!("RELEASE {name}")).unwrap();
                                working = kept;
                            }
                        }
                        _ => {
                            run(
                                &conn,
                                "DELETE FROM t WHERE id = ?",
                                vec![Value::from_i64(id)],
                            );
                            working.remove(&id);
                        }
                    }
                }
                conn.execute("COMMIT").unwrap();
                model = working;
                let want: Vec<(i64, Vec<u8>)> = model.clone().into_iter().collect();
                assert_eq!(
                    table(&connect(&db)),
                    want,
                    "{format:?}, seed {seed}, transaction {txn}"
                );
            }
            integrity(&connect(&db));
        }
        let db = open(&path);
        let want: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
        assert_eq!(
            table(&connect(&db)),
            want,
            "{format:?}, seed {seed}, opened again"
        );
    }
}

/// Do: in a packed log, write a transaction too large for its connection's cache, so its pages
/// spill into the log before it ends; roll it back; then commit another like it. Expect: the log
/// grew before the end (the spill happened), the rolled-back rows are nowhere, and the committed
/// rows read back, on a fresh connection and after opening the database again.
#[test]
fn a_transaction_spilled_into_a_packed_log_commits_or_rolls_back_whole() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spill.db");
    let mut want = Vec::new();
    {
        let db = open(&path);
        db.wal_format(LogFormat::Packed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want.extend(insert(&conn, 0..50, 3000, false));
        conn.execute("PRAGMA cache_size = 20").unwrap();
        let before = std::fs::metadata(wal_path(&path)).unwrap().len();

        conn.execute("BEGIN").unwrap();
        insert_in(&conn, 50..800, 3000, false);
        let during = std::fs::metadata(wal_path(&path)).unwrap().len();
        assert!(during > before, "nothing spilled: {before} then {during}");
        conn.execute("ROLLBACK").unwrap();
        assert_eq!(table(&connect(&db)), want);

        conn.execute("BEGIN").unwrap();
        want.extend(insert_in(&conn, 800..1500, 3000, true));
        conn.execute("COMMIT").unwrap();
        assert_eq!(table(&connect(&db)), want);
        assert_eq!(table(&conn), want);
    }
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    integrity(&conn);
}

/// Do: commit to a log whose generation began with one page a frame, ask for packing, commit
/// more, start the log again (a truncating checkpoint), commit more, and open it again without
/// asking for packing. Expect: the generation keeps the format it began with until the log starts
/// again, the next is packed, and every row reads back after each step.
#[test]
fn a_log_keeps_the_format_its_generation_began_with() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("formats.db");
    let mut want = Vec::new();
    {
        let db = open(&path);
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want.extend(insert(&conn, 0..100, 3000, false));
        assert_eq!(wal_format(&path), PAGE_FORMAT);
        db.wal_format(LogFormat::Packed).unwrap();
        want.extend(insert(&conn, 100..200, 3000, false));
        assert_eq!(
            wal_format(&path),
            PAGE_FORMAT,
            "a generation keeps its format"
        );
        assert_eq!(table(&connect(&db)), want);
        truncate(&conn);
        want.extend(insert(&conn, 200..300, 3000, false));
        assert_eq!(wal_format(&path), PACKED_FORMAT);
        assert_eq!(table(&connect(&db)), want);
    }
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    want.extend(insert(&conn, 300..350, 3000, false));
    assert_eq!(
        wal_format(&path),
        PACKED_FORMAT,
        "the recovered generation is still packed"
    );
    truncate(&conn);
    want.extend(insert(&conn, 350..400, 3000, false));
    assert_eq!(wal_format(&path), PAGE_FORMAT);
    assert_eq!(table(&connect(&db)), want);
    integrity(&conn);
}

/// SQLite's log checksum (sqlite3 `walChecksumBytes`) over `bytes`, from `seed`, its words read
/// little-endian as the header's magic says for this machine's logs.
fn checksum(bytes: &[u8], seed: (u32, u32), big_endian: bool) -> (u32, u32) {
    let (mut s0, mut s1) = seed;
    for pair in bytes.as_chunks::<8>().0 {
        let word = |b: &[u8]| {
            let w: [u8; 4] = b.try_into().unwrap();
            if big_endian {
                u32::from_be_bytes(w)
            } else {
                u32::from_le_bytes(w)
            }
        };
        s0 = s0.wrapping_add(word(&pair[0..4])).wrapping_add(s1);
        s1 = s1.wrapping_add(word(&pair[4..8])).wrapping_add(s0);
    }
    (s0, s1)
}

/// The first slot of each record in a packed or mixed log with no restart behind it, with its slot
/// count, in order; a mixed log's page frames are passed over.
fn records(wal: &[u8]) -> Vec<(usize, u32)> {
    records_and_frames(wal).0
}

/// A packed or mixed log's records, as [`records`], and how many page frames it holds.
fn records_and_frames(wal: &[u8]) -> (Vec<(usize, u32)>, usize) {
    let format = u32::from_be_bytes(wal[4..8].try_into().unwrap());
    let mixed = format == MIXED_FORMAT || format == MIXED_RECORDS_FORMAT;
    let mut found = Vec::new();
    let mut frames = 0;
    let mut at = HEADER;
    while at + FRAME_HEADER + PAGE <= wal.len() {
        let field = u32::from_be_bytes(wal[at..at + 4].try_into().unwrap());
        assert!(
            field != 0 && field != CONTINUATION,
            "a record or page frame begins at {at}"
        );
        if mixed && field & RECORD_HEAD == 0 {
            frames += 1;
            at += FRAME_HEADER + PAGE;
            continue;
        }
        let slots = if mixed { field & !RECORD_HEAD } else { field };
        found.push((at, slots));
        at += slots as usize * (FRAME_HEADER + PAGE);
    }
    (found, frames)
}

/// Stamps the log at `path` with `format` in its header, and chains the header's and every
/// frame's checksums again from it, as a binary that began the generation so would have written
/// them.
fn restamp(path: &Path, format: u32) {
    let mut wal = std::fs::read(wal_path(path)).unwrap();
    let big_endian = u32::from_be_bytes(wal[0..4].try_into().unwrap()) & 1 == 1;
    wal[4..8].copy_from_slice(&format.to_be_bytes());
    let mut seed = checksum(&wal[0..24], (0, 0), big_endian);
    wal[24..28].copy_from_slice(&seed.0.to_be_bytes());
    wal[28..32].copy_from_slice(&seed.1.to_be_bytes());
    let frame = FRAME_HEADER + PAGE;
    let mut at = HEADER;
    while at + frame <= wal.len() {
        let header = checksum(&wal[at..at + 8], seed, big_endian);
        seed = checksum(&wal[at + FRAME_HEADER..at + frame], header, big_endian);
        wal[at + 16..at + 20].copy_from_slice(&seed.0.to_be_bytes());
        wal[at + 20..at + 24].copy_from_slice(&seed.1.to_be_bytes());
        at += frame;
    }
    std::fs::write(wal_path(path), &wal).unwrap();
}

/// Do: begin a packed generation; then stamp a log's header with the format of 8 deltas in a row
/// (0x414d_5002, as a binary before 2026-11-11 began it), reopen it, change a byte of the same
/// rows thirty times, and read them back. Expect: a generation begun now takes 0x414d_5004, so an
/// older binary refuses it as a format it doesn't know, naming the value, never as corrupt; the
/// restamped generation keeps its format and its chains end at 8, so an older binary still reads
/// it; every row as last written (decisions 16 and 17, 2026-11-11; AC-T1.4.4).
#[test]
fn a_generation_keeps_the_chain_bound_of_the_format_it_began_with() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eight.db");
    let mut want;
    {
        let db = open(&path);
        db.wal_format(LogFormat::Packed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want = insert(&conn, 0..300, 500, false);
        assert_eq!(wal_format(&path), PACKED_FORMAT);
    }
    restamp(&path, PACKED_EIGHT_FORMAT);
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
    for round in 0..30u8 {
        conn.execute("BEGIN").unwrap();
        for row in want
            .iter_mut()
            .filter(|(id, _)| *id % 3 == i64::from(round % 3))
        {
            row.1[usize::from(round) * 7 % 500] = round;
            update.reset().unwrap();
            update
                .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                .unwrap();
            update
                .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                .unwrap();
            update.run_ignore_rows().unwrap();
        }
        conn.execute("COMMIT").unwrap();
    }
    assert_eq!(
        wal_format(&path),
        PACKED_EIGHT_FORMAT,
        "the generation kept its format"
    );
    let entries = entries(&std::fs::read(wal_path(&path)).unwrap());
    let deepest = entries.iter().map(|(_, _, depth, _)| *depth).max().unwrap();
    assert!(deepest >= 2, "deltas at all: {deepest}");
    assert!(
        deepest <= 8,
        "a chain of {deepest} in a generation of 8 at most"
    );
    assert_eq!(table(&connect(&db)), want);
}

/// Do: open a packed log whose last record's checksums are recomputed after its payload was
/// damaged, so they hold but it doesn't decode. Expect: the database is refused as corrupt, not
/// opened with that commit (or any) quietly dropped.
#[test]
fn a_record_whose_checksums_hold_but_that_doesnt_decode_is_refused() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.db");
    {
        let db = open(&path);
        db.wal_format(LogFormat::Packed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        insert(&conn, 0..20, 3000, false);
        insert(&conn, 20..40, 3000, false);
    }
    let mut wal = std::fs::read(wal_path(&path)).unwrap();
    let magic = u32::from_be_bytes(wal[0..4].try_into().unwrap());
    let big_endian = magic & 1 == 1;
    let (head, _) = *records(&wal).last().unwrap();
    // The stored length, past any record's: the payload no longer decodes.
    let payload = head + FRAME_HEADER;
    wal[payload..payload + 4].copy_from_slice(&0x7fff_fff0u32.to_be_bytes());
    // Chain the checksums again from the slot before it, through the end.
    let frame = FRAME_HEADER + PAGE;
    let mut seed = if head == HEADER {
        let c = &wal[24..32];
        (
            u32::from_be_bytes(c[0..4].try_into().unwrap()),
            u32::from_be_bytes(c[4..8].try_into().unwrap()),
        )
    } else {
        let c = &wal[head - frame + 16..head - frame + 24];
        (
            u32::from_be_bytes(c[0..4].try_into().unwrap()),
            u32::from_be_bytes(c[4..8].try_into().unwrap()),
        )
    };
    let mut at = head;
    while at + frame <= wal.len() {
        let header = checksum(&wal[at..at + 8], seed, big_endian);
        seed = checksum(&wal[at + FRAME_HEADER..at + frame], header, big_endian);
        wal[at + 16..at + 20].copy_from_slice(&seed.0.to_be_bytes());
        wal[at + 20..at + 24].copy_from_slice(&seed.1.to_be_bytes());
        at += frame;
    }
    std::fs::write(wal_path(&path), &wal).unwrap();

    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    match Database::open(io, path.to_str().unwrap(), options) {
        Err(LimboError::Corrupt(why)) => assert!(why.contains("packed log"), "{why}"),
        Err(other) => panic!("refused, but not as corrupt: {other}"),
        Ok(_) => panic!("a log with a record that doesn't decode was opened"),
    }
}

/// Do: cut a packed log short in the middle of its last record, as a crash during that commit's
/// write leaves it. Expect: the database opens with every commit before it, and none of that one.
#[test]
fn a_record_cut_short_takes_only_its_own_commit() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.db");
    let kept;
    {
        let db = open(&path);
        db.wal_format(LogFormat::Packed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        kept = insert(&conn, 0..200, 3000, true);
        insert(&conn, 200..400, 3000, true);
    }
    let wal = std::fs::read(wal_path(&path)).unwrap();
    let (head, slots) = *records(&wal).last().unwrap();
    assert!(slots > 1, "the last record takes a slot: {slots}");
    let cut = head + (FRAME_HEADER + PAGE) * (slots as usize / 2).max(1);
    std::fs::write(wal_path(&path), &wal[..cut]).unwrap();

    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(table(&conn), kept);
    integrity(&conn);
}

/// Do: in a mixed log (decision 37), with a connection cache of 16 pages, insert rows of a page each
/// and change them again in one transaction, so its pages spill into the log and spill again, then
/// commit; then change a byte of some rows in a transaction of their own; read the rows on a fresh
/// connection, after opening the database again, and through the shared pages. Expect: the log
/// holds page frames (the spills, each page's written over, and the commit's pages new to the
/// log's chains: at most two frames a page dirtied, decision 39) and, for the changes, records of
/// one slot, marked apart from them, that end the log; every row as written each time; and a
/// database opened on a mixed log shares its pages, where one on a packed log refuses (decisions
/// 20 and 26).
#[test]
fn a_mixed_log_holds_its_spills_as_pages_and_its_commits_as_records() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.db");
    let mut want;
    {
        let db = open(&path);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        conn.execute("PRAGMA cache_size = 16").unwrap();
        truncate(&conn);
        conn.execute("BEGIN").unwrap();
        want = insert_in(&conn, 0..200, 3000, false);
        let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
        for row in want.iter_mut().step_by(3) {
            row.1[0] ^= 0xff;
            update.reset().unwrap();
            update
                .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                .unwrap();
            update
                .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                .unwrap();
            update.run_ignore_rows().unwrap();
        }
        drop(update);
        let dirtied = conn.dirty_pages();
        conn.execute("COMMIT").unwrap();
        assert_eq!(wal_format(&path), MIXED_FORMAT);
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let (_, frames) = records_and_frames(&wal);
        assert!(frames > 0, "no page frame");
        assert!(
            frames <= 2 * dirtied,
            "{frames} page frames for the {dirtied} pages the transaction dirtied"
        );
        // Pages a commit logged and left in the connection's cache change as deltas against what
        // it logged: the first round's pages come back from the log (new to the chains, page
        // frames), the second round's are deltas against the first's.
        conn.execute("PRAGMA cache_size = 2000").unwrap();
        for round in 1..=2usize {
            conn.execute("BEGIN").unwrap();
            let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
            for row in want.iter_mut().step_by(7) {
                row.1[round] ^= 0xff;
                update.reset().unwrap();
                update
                    .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                    .unwrap();
                update
                    .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                    .unwrap();
                update.run_ignore_rows().unwrap();
            }
            drop(update);
            conn.execute("COMMIT").unwrap();
        }
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let (records, _) = records_and_frames(&wal);
        assert!(!records.is_empty(), "the changes wrote no record");
        assert!(
            records.iter().all(|&(_, slots)| slots == 1),
            "records of more than one slot: {records:?}"
        );
        let last = records.last().unwrap();
        let end = last.0 + last.1 as usize * (FRAME_HEADER + PAGE);
        assert_eq!(end, wal.len(), "the changes' records end the log");
        assert_eq!(table(&connect(&db)), want);
        integrity(&connect(&db));
    }
    // Opened again: recovery reads page frames and records side by side, and the pages are shared.
    let db = open(&path);
    db.share_pages(1024).unwrap();
    let readers: Vec<_> = (0..2).map(|_| connect(&db)).collect();
    for reader in &readers {
        assert_eq!(table(reader), want);
    }
    let shared = db.shared_page_counts().unwrap();
    assert!(
        shared.hits > 0,
        "no page was read from the shared pages: {shared:?}"
    );
    integrity(&readers[0]);
}

/// Do: open a database whose log's header names a format this engine doesn't write (its
/// checksum made to hold). Expect: the database is refused as corrupt, not its log read as page
/// frames (decision 37: an older binary reading a newer generation would map records' slots to pages
/// that aren't there).
#[test]
fn a_log_of_a_format_not_known_is_refused() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unknown.db");
    {
        let db = open(&path);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        insert(&conn, 0..20, 3000, false);
    }
    let mut wal = std::fs::read(wal_path(&path)).unwrap();
    let big_endian = u32::from_be_bytes(wal[0..4].try_into().unwrap()) & 1 == 1;
    wal[4..8].copy_from_slice(&0x414d_5006u32.to_be_bytes());
    let (c1, c2) = checksum(&wal[0..24], (0, 0), big_endian);
    wal[24..28].copy_from_slice(&c1.to_be_bytes());
    wal[28..32].copy_from_slice(&c2.to_be_bytes());
    std::fs::write(wal_path(&path), &wal).unwrap();

    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    match Database::open(io, path.to_str().unwrap(), options) {
        Err(LimboError::Corrupt(why)) => assert!(why.contains("isn't known"), "{why}"),
        Err(other) => panic!("refused, but not as corrupt: {other}"),
        Ok(_) => panic!("a log of a format not known was opened"),
    }
}

/// What a database's connections read from its log: the reads, their bytes, and the most one read
/// took.
#[derive(Default)]
struct LogReads {
    reads: AtomicU64,
    bytes: AtomicU64,
    largest: AtomicU64,
}

impl LogReads {
    fn take(&self) -> (u64, u64, u64) {
        (
            self.reads.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
            self.largest.swap(0, Ordering::Relaxed),
        )
    }
}

/// The system's files, each read of a log counted.
struct Counting {
    inner: Arc<PlatformIO>,
    reads: Arc<LogReads>,
}

struct CountedFile {
    inner: Arc<dyn File>,
    log: bool,
    reads: Arc<LogReads>,
}

impl File for CountedFile {
    fn lock_file(&self, exclusive: bool) -> amphora_engine::Result<()> {
        self.inner.lock_file(exclusive)
    }

    fn unlock_file(&self) -> amphora_engine::Result<()> {
        self.inner.unlock_file()
    }

    fn pread(&self, pos: u64, c: Completion) -> amphora_engine::Result<Completion> {
        if self.log {
            let n = c.as_read().buf().len() as u64;
            self.reads.reads.fetch_add(1, Ordering::Relaxed);
            self.reads.bytes.fetch_add(n, Ordering::Relaxed);
            self.reads.largest.fetch_max(n, Ordering::Relaxed);
        }
        self.inner.pread(pos, c)
    }

    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.inner.pwrite(pos, buffer, c)
    }

    fn pwritev(
        &self,
        pos: u64,
        buffers: Vec<Arc<Buffer>>,
        c: Completion,
    ) -> amphora_engine::Result<Completion> {
        self.inner.pwritev(pos, buffers, c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> amphora_engine::Result<Completion> {
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
            reads: self.reads.clone(),
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

/// The database at `path`, its reads of its log counted in `reads`.
fn open_counted(path: &Path, reads: &Arc<LogReads>) -> Arc<Database> {
    let io = Arc::new(Counting {
        inner: Arc::new(PlatformIO::new().unwrap()),
        reads: reads.clone(),
    });
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    Database::open(io, path.to_str().unwrap(), options).unwrap()
}

/// Row `id`'s body, looked up by its id.
fn row(conn: &Arc<Connection>, id: i64) -> Vec<u8> {
    let mut stmt = conn.prepare("SELECT body FROM t WHERE id = ?").unwrap();
    stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id))
        .unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    assert_eq!(rows.len(), 1, "row {id}");
    rows[0][0].to_blob().unwrap().to_vec()
}

/// A log's frame: its header and a page.
const FRAME: u64 = (FRAME_HEADER + PAGE) as u64;

/// Do: in a mixed log, commit 300 rows of 3,000 bytes (a leaf page each, new to the log) in one
/// transaction, open the database again (nothing decoded, nothing cached) and look up rows by
/// their ids, one at a time on fresh connections. Expect: every read of the log takes at most one
/// frame (decision 39: a page new to the log's chains is a page frame, read as page-a-frame reads
/// it), where decision 37's log read the whole record of up to 64 pages that held the page; each
/// row as written; and the lookups read the log (the test isn't vacuous).
#[test]
fn a_page_new_to_the_log_is_read_in_one_frame() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cold.db");
    let reads = Arc::new(LogReads::default());
    let want = {
        let db = open_counted(&path, &reads);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        insert(&conn, 0..300, 3000, false)
    };
    let db = open_counted(&path, &reads);
    reads.take();
    let mut read = 0;
    for &(id, ref body) in want.iter().step_by(37) {
        assert_eq!(&row(&connect(&db), id), body, "row {id}");
        let (n, bytes, largest) = reads.take();
        assert!(
            largest <= FRAME,
            "looking up row {id} read {bytes} bytes of the log in {n} reads, {largest} at once"
        );
        read += n;
    }
    assert!(read > 0, "no lookup read the log");
}

/// Do: in a mixed log, commit 300 rows, then change two bytes of every third row in each of 12
/// transactions (the rows' pages stay in the writer's cache, so each change is a delta against
/// the page's last version, decision 17, its chain growing while its deltas take less than the
/// page), open the database again and read every row, then look rows up one at a time. Expect:
/// every read of the
/// log takes at most one frame (decision 39: a chain's entries are records of one slot, and a
/// chain may begin at a page frame), where decision 37's log read a whole record for each entry of
/// a page's chain; every row as last written; deltas in the log, and reads of it.
#[test]
fn a_pages_chain_is_read_one_frame_an_entry() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hot.db");
    let reads = Arc::new(LogReads::default());
    let mut want;
    {
        let db = open_counted(&path, &reads);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want = insert(&conn, 0..300, 3000, false);
        let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
        for round in 0..12usize {
            conn.execute("BEGIN").unwrap();
            for row in want.iter_mut().step_by(3) {
                row.1[round * 11] ^= 0x5a;
                row.1[2000 + round] ^= 0xa5;
                update.reset().unwrap();
                update
                    .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                    .unwrap();
                update
                    .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                    .unwrap();
                update.run_ignore_rows().unwrap();
            }
            conn.execute("COMMIT").unwrap();
        }
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let deltas = entries(&wal)
            .iter()
            .filter(|(_, kind, _, _)| *kind == 1)
            .count();
        // Eleven rounds of twelve at least: two bytes a round keep a chain's deltas far under a
        // page (decision 17), so a chain ends, its page's image beginning it again, at most once.
        assert!(
            deltas >= 100 * 11,
            "{deltas} deltas for 100 rows changed 12 times"
        );
    }
    let db = open_counted(&path, &reads);
    reads.take();
    assert_eq!(table(&connect(&db)), want);
    let (n, bytes, largest) = reads.take();
    assert!(n > 0, "reading the table read nothing of the log");
    assert!(
        largest <= FRAME,
        "reading the table read {bytes} bytes of the log in {n} reads, {largest} at once"
    );
    for &(id, ref body) in want.iter().step_by(31) {
        assert_eq!(&row(&connect(&db), id), body, "row {id}");
        let (n, bytes, largest) = reads.take();
        assert!(
            largest <= FRAME,
            "looking up row {id} read {bytes} bytes of the log in {n} reads, {largest} at once"
        );
    }
}

/// Do: begin a mixed log's generation as a binary of decision 37 began it (its header restamped
/// 0x414d_5003 over a first commit of new pages, its checksums chained again), open it, commit
/// rows and change them, read them back, then start the log again (a truncating checkpoint) and
/// commit more. Expect: the generation keeps 0x414d_5003, its commits written as decision 37 wrote
/// them (records, none past its first commit's page frames: no chain begins at a page frame, which
/// a binary of 0x414d_5003 couldn't read); every row reads back, also after opening it again; and
/// the next generation begins 0x414d_5007.
#[test]
fn a_mixed_generation_begun_before_keeps_its_records() {
    if pager_only() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("before.db");
    {
        let db = open(&path);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
    }
    let (records, first) = records_and_frames(&std::fs::read(wal_path(&path)).unwrap());
    assert!(
        records.is_empty() && first > 0,
        "a first commit of page frames"
    );
    restamp(&path, MIXED_RECORDS_FORMAT);
    let mut want;
    {
        let db = open(&path);
        db.wal_format(LogFormat::Mixed).unwrap();
        let conn = connect(&db);
        want = insert(&conn, 0..200, 3000, false);
        let mut update = conn.prepare("UPDATE t SET body = ? WHERE id = ?").unwrap();
        for round in 0..4usize {
            conn.execute("BEGIN").unwrap();
            for row in want.iter_mut().step_by(5) {
                row.1[round] ^= 0xff;
                update.reset().unwrap();
                update
                    .bind_at(NonZero::new(1).unwrap(), Value::from_slice(&row.1).unwrap())
                    .unwrap();
                update
                    .bind_at(NonZero::new(2).unwrap(), Value::from_i64(row.0))
                    .unwrap();
                update.run_ignore_rows().unwrap();
            }
            conn.execute("COMMIT").unwrap();
        }
        drop(update);
        assert_eq!(wal_format(&path), MIXED_RECORDS_FORMAT);
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let (records, frames) = records_and_frames(&wal);
        assert_eq!(frames, first, "page frames past the first commit");
        assert!(
            records.iter().any(|&(_, slots)| slots > 1),
            "no record of many slots: {records:?}"
        );
        assert_eq!(table(&connect(&db)), want);
    }
    let db = open(&path);
    db.wal_format(LogFormat::Mixed).unwrap();
    let conn = connect(&db);
    assert_eq!(table(&conn), want);
    integrity(&conn);
    truncate(&conn);
    want.extend(insert(&conn, 200..250, 3000, false));
    assert_eq!(wal_format(&path), MIXED_FORMAT);
    assert_eq!(table(&connect(&db)), want);
}
