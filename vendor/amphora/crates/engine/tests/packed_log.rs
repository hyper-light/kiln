//! The packed log (`storage/packed.rs`; Amphora's `docs/design/store.md`, decisions 16 and 17):
//! what a packed log holds, whole images and deltas, reads back the same through each way the
//! engine reads its log (a connection with nothing cached, the checkpoint, recovery on open), whole
//! commits or none survive a cut, and a record whose checksums hold but that doesn't decode is
//! refused rather than read (AC-T1.4.4, AC-T1.3.1).

use amphora_engine::{
    CheckpointMode, ConnectOpts, Connection, Database, DatabaseOpts, LimboError, Numeric,
    OpenOptions, PlatformIO, SqliteDialect, Value,
};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The log header's file format for a packed generation, and for SQLite's one page a frame.
const PACKED_FORMAT: u32 = 0x414d_5002;
const PAGE_FORMAT: u32 = 3_007_000;
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed.db");
    let mut want = Vec::new();
    {
        let db = open(&path);
        db.wal_pack(true).unwrap();
        let conn = connect(&db);
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body BLOB)")
            .unwrap();
        want.extend(insert(&conn, 0..10, 100, false));
        want.extend(insert(&conn, 10..400, 3000, false));
        want.extend(insert(&conn, 400..1600, 3000, true));
        // Rows of 600 KB: records of nothing but their overflow pages, which don't compress.
        want.extend(insert(&conn, 1600..1602, 600_000, true));
        assert_eq!(wal_format(&path), PACKED_FORMAT);
        let wal = std::fs::read(wal_path(&path)).unwrap();
        let stored: Vec<u32> = records(&wal)
            .iter()
            .map(|(head, _)| {
                let at = head + FRAME_HEADER;
                u32::from_be_bytes(wal[at..at + 4].try_into().unwrap())
            })
            .collect();
        assert!(stored.iter().any(|n| *n > 0), "no record compressed");
        assert!(stored.contains(&0), "no record stored as it is: {stored:?}");

        // A connection with nothing cached reads every page from the log's records.
        assert_eq!(table(&connect(&db)), want);
        integrity(&connect(&db));

        // The checkpoint reads the records too, and the next generation is packed as well.
        truncate(&conn);
        assert_eq!(table(&connect(&db)), want);
        want.extend(insert(&conn, 2000..2100, 3000, false));
        assert_eq!(wal_format(&path), PACKED_FORMAT);
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed.db");
    let want = {
        let db = open(&path);
        db.wal_pack(true).unwrap();
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

/// The entries of every record in a packed log with no restart behind it, as (kind, depth): 0 an
/// image, 1 a delta.
fn entries(wal: &[u8]) -> Vec<(u8, u8)> {
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
            found.push((raw[at + 4], raw[at + 5]));
        }
    }
    found
}

/// Do: in a packed log, change a few bytes of the same rows commit after commit, more times than
/// a page's deltas may run in a row; read the rows on a fresh connection, after a checkpoint and
/// after opening the database again. Expect: deltas in the log, at depths up to the most allowed
/// and never past it, and every row as last written each time.
#[test]
fn pages_changed_again_and_again_are_logged_as_deltas_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deltas.db");
    let mut want: Vec<(i64, Vec<u8>)>;
    {
        let db = open(&path);
        db.wal_pack(true).unwrap();
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
        let deltas = entries.iter().filter(|(kind, _)| *kind == 1).count();
        let deepest = entries.iter().map(|(_, depth)| *depth).max().unwrap();
        assert!(
            deltas * 2 > entries.len(),
            "{deltas} deltas of {} entries",
            entries.len()
        );
        assert_eq!(deepest, 8, "chains run to the most allowed, and no further");
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
    for pack in [false, true] {
        for seed in 1..=8u64 {
            savepoints_amid_spills(pack, seed);
        }
    }
}

fn savepoints_amid_spills(pack: bool, seed: u64) {
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("savepoints.db");
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut model: std::collections::BTreeMap<i64, Vec<u8>> = Default::default();
        {
            let db = open(&path);
            db.wal_pack(pack).unwrap();
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
                    "pack {pack}, seed {seed}, transaction {txn}"
                );
            }
            integrity(&connect(&db));
        }
        let db = open(&path);
        let want: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
        assert_eq!(
            table(&connect(&db)),
            want,
            "pack {pack}, seed {seed}, opened again"
        );
    }
}

/// Do: in a packed log, write a transaction too large for its connection's cache, so its pages
/// spill into the log before it ends; roll it back; then commit another like it. Expect: the log
/// grew before the end (the spill happened), the rolled-back rows are nowhere, and the committed
/// rows read back, on a fresh connection and after opening the database again.
#[test]
fn a_transaction_spilled_into_a_packed_log_commits_or_rolls_back_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spill.db");
    let mut want = Vec::new();
    {
        let db = open(&path);
        db.wal_pack(true).unwrap();
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
        db.wal_pack(true).unwrap();
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

/// The first slot of each record in a packed log, with its slot count, in order.
fn records(wal: &[u8]) -> Vec<(usize, u32)> {
    let mut found = Vec::new();
    let mut at = HEADER;
    while at + FRAME_HEADER + PAGE <= wal.len() {
        let field = u32::from_be_bytes(wal[at..at + 4].try_into().unwrap());
        assert!(
            field != 0 && field != CONTINUATION,
            "a record begins at {at}"
        );
        found.push((at, field));
        at += field as usize * (FRAME_HEADER + PAGE);
    }
    found
}

/// Do: open a packed log whose last record's checksums are recomputed after its payload was
/// damaged, so they hold but it doesn't decode. Expect: the database is refused as corrupt, not
/// opened with that commit (or any) quietly dropped.
#[test]
fn a_record_whose_checksums_hold_but_that_doesnt_decode_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.db");
    {
        let db = open(&path);
        db.wal_pack(true).unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.db");
    let kept;
    {
        let db = open(&path);
        db.wal_pack(true).unwrap();
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
