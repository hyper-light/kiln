//! Column reads the place of its value from the cursor's parse of the row's record header, kept
//! while the cursor stays on the row (`vdbe/row_header.rs`, docs/design/executor.md decision 7):
//! every column of every row a scan, an index seek's deferred seek and a join reach is the row's
//! own value, in any order the select list asks for them (T-QT.3.1, T-QT.3.2).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect, Value,
};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::num::NonZero;
use std::sync::Arc;

const ROWS: i64 = 600;
/// The columns after `id` and `k`: a, b, c, d, then e, added by ALTER TABLE with a default.
const COLUMNS: [&str; 5] = ["a", "b", "c", "d", "e"];
const DEFAULT_E: &str = "dflt";

fn random_value(rng: &mut ChaCha8Rng) -> Value {
    match rng.random_range(0..8) {
        0 => Value::Null,
        1 => Value::from_i64(rng.random_range(0..2)),
        2 => Value::from_i64(rng.random_range(-200..200)),
        3 => Value::from_i64(rng.random()),
        4 => Value::from_f64(rng.random_range(-1e9..1e9)),
        5 => Value::build_text("x".repeat(rng.random_range(0..60))),
        // Past a page: the record spills to overflow pages.
        6 => Value::build_text("y".repeat(rng.random_range(5_000..9_000))),
        _ => Value::from_blob((0..rng.random_range(0..80)).map(|_| rng.random()).collect()),
    }
}

/// The table and the model of its rows: `rows[id - 1]` holds k and the five columns' values.
fn open(dir: &std::path::Path, seed: u64) -> (Arc<Connection>, Vec<(i64, Vec<Value>)>) {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("rh.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, a, b, c, d)")
        .unwrap();
    conn.execute("CREATE INDEX t_k ON t (k)").unwrap();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut rows = Vec::new();
    conn.execute("BEGIN").unwrap();
    let mut insert = |conn: &Arc<Connection>, id: i64, with_e: bool, rng: &mut ChaCha8Rng| {
        let k = rng.random_range(1..=ROWS);
        let mut values: Vec<Value> = (0..4).map(|_| random_value(rng)).collect();
        let sql = if with_e {
            "INSERT INTO t (id, k, a, b, c, d, e) VALUES (?, ?, ?, ?, ?, ?, ?)"
        } else {
            "INSERT INTO t (id, k, a, b, c, d) VALUES (?, ?, ?, ?, ?, ?)"
        };
        if with_e {
            values.push(random_value(rng));
        }
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id))
            .unwrap();
        stmt.bind_at(NonZero::new(2).unwrap(), Value::from_i64(k))
            .unwrap();
        for (i, v) in values.iter().enumerate() {
            stmt.bind_at(NonZero::new(i + 3).unwrap(), v.clone())
                .unwrap();
        }
        stmt.run_collect_rows().unwrap();
        if !with_e {
            // A row written before the column existed reads its default.
            values.push(Value::build_text(DEFAULT_E));
        }
        rows.push((k, values));
    };
    for id in 1..=ROWS / 2 {
        insert(&conn, id, false, &mut rng);
    }
    conn.execute("COMMIT").unwrap();
    conn.execute(format!("ALTER TABLE t ADD COLUMN e DEFAULT '{DEFAULT_E}'"))
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for id in ROWS / 2 + 1..=ROWS {
        insert(&conn, id, true, &mut rng);
    }
    conn.execute("COMMIT").unwrap();
    (conn, rows)
}

/// A select list of `width` columns in a random order, some repeated.
fn random_list(rng: &mut ChaCha8Rng, width: usize) -> Vec<usize> {
    (0..width)
        .map(|_| rng.random_range(0..COLUMNS.len()))
        .collect()
}

fn names(list: &[usize], table: &str) -> String {
    list.iter()
        .map(|&c| format!("{table}.{}", COLUMNS[c]))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Do: scan the table with select lists in random column orders. Expect: each row's values are
/// the model's, the rows written before `e` existed reading its default (T-QT.3.1).
#[test]
fn a_scan_reads_every_column_in_any_order() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, rows) = open(dir.path(), 1);
    let mut rng = ChaCha8Rng::seed_from_u64(2);
    for _ in 0..20 {
        let width = rng.random_range(1..9);
        let list = random_list(&mut rng, width);
        let sql = format!("SELECT t.id, {} FROM t ORDER BY t.id", names(&list, "t"));
        let got = conn.prepare(&sql).unwrap().run_collect_rows().unwrap();
        assert_eq!(got.len(), rows.len(), "{sql}");
        for (row, (_, values)) in got.iter().zip(&rows) {
            for (at, &column) in list.iter().enumerate() {
                assert_eq!(row[at + 1], values[column], "{sql}, row {:?}", row[0]);
            }
        }
    }
}

/// Do: look rows up through the index on k, which reads k from the index and seeks the table
/// only when a column of its own is read (a deferred seek), and join the table to itself on
/// `t2.id = t1.k`. Expect: every row's values are the model's (T-QT.3.2).
#[test]
fn a_seek_and_a_join_read_each_row_they_reach() {
    let dir = tempfile::tempdir().unwrap();
    let (conn, rows) = open(dir.path(), 3);
    let mut rng = ChaCha8Rng::seed_from_u64(4);
    for _ in 0..20 {
        let width = rng.random_range(1..7);
        let list = random_list(&mut rng, width);
        let sql = format!(
            "SELECT t.id, {} FROM t WHERE t.k = ? ORDER BY t.id",
            names(&list, "t")
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        for _ in 0..10 {
            let k = rng.random_range(1..=ROWS);
            stmt.reset().unwrap();
            stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(k))
                .unwrap();
            let got = stmt.run_collect_rows().unwrap();
            let want: Vec<(i64, &Vec<Value>)> = rows
                .iter()
                .enumerate()
                .filter(|(_, (row_k, _))| *row_k == k)
                .map(|(i, (_, values))| (i as i64 + 1, values))
                .collect();
            assert_eq!(got.len(), want.len(), "{sql}, k = {k}");
            for (row, (id, values)) in got.iter().zip(want) {
                assert_eq!(row[0], Value::from_i64(id), "{sql}, k = {k}");
                for (at, &column) in list.iter().enumerate() {
                    assert_eq!(row[at + 1], values[column], "{sql}, id {id}");
                }
            }
        }
    }
    for _ in 0..10 {
        let left = random_list(&mut rng, 3);
        let right = random_list(&mut rng, 3);
        let sql = format!(
            "SELECT t1.id, {}, {} FROM t AS t1 JOIN t AS t2 ON t2.id = t1.k ORDER BY t1.id",
            names(&left, "t1"),
            names(&right, "t2")
        );
        let got = conn.prepare(&sql).unwrap().run_collect_rows().unwrap();
        assert_eq!(got.len(), rows.len(), "{sql}");
        for (row, (k, values)) in got.iter().zip(&rows) {
            let joined = &rows[usize::try_from(*k - 1).unwrap()].1;
            for (at, &column) in left.iter().enumerate() {
                assert_eq!(row[at + 1], values[column], "{sql}");
            }
            for (at, &column) in right.iter().enumerate() {
                assert_eq!(row[at + 4], joined[column], "{sql}");
            }
        }
    }
}
