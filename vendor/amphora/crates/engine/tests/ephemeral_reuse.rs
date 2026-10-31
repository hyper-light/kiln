//! A statement keeps its small ephemeral tables from one run to the next and empties each when the
//! next run opens it (`vdbe/mod.rs`, `ProgramState::ephemeral_kept`): every run of an IN list or an
//! IN subquery reads its own keys and none of a run before, whether the table was kept (a few
//! pages) or too large to keep (AC-T1.1.1: reads as committed).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::Arc;

fn open(dir: &std::path::Path, rows: i64) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("eph.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for first in (1..=rows).step_by(500) {
        let values: Vec<String> = (first..(first + 500).min(rows + 1))
            .map(|id| format!("({id}, {})", id % 97))
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn
}

fn ints(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|row| match &row[0] {
            Value::Numeric(Numeric::Integer(i)) => *i,
            other => panic!("{other:?}"),
        })
        .collect()
}

/// Do: run one prepared `id IN (?, ?, ?)` four times with other keys: overlapping, repeated, and
/// absent. Expect: each run returns exactly its own keys that exist, once each.
#[test]
fn each_run_of_an_in_list_reads_only_its_own_keys() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path(), 1000);
    let mut stmt = conn
        .prepare("SELECT id FROM t WHERE id IN (?, ?, ?) ORDER BY id")
        .unwrap();
    for (keys, want) in [
        ([1, 2, 3], vec![1, 2, 3]),
        ([500, 501, 999], vec![500, 501, 999]),
        ([7, 7, 7], vec![7]),
        ([2000, 3000, 1], vec![1]),
    ] {
        stmt.reset().unwrap();
        for (i, k) in keys.iter().enumerate() {
            stmt.bind_at(NonZero::new(i + 1).unwrap(), Value::from_i64(*k))
                .unwrap();
        }
        assert_eq!(
            ints(stmt.run_collect_rows().unwrap()),
            want,
            "keys {keys:?}"
        );
    }
}

/// Do: run one prepared `id IN (SELECT id FROM t WHERE v = ?)` (about 1 row in 97 of 20,000:
/// a table small enough to keep) and `v <= ?` (up to every row: too large to keep) in turns with
/// other values. Expect: each run's count is the count of its own rows.
#[test]
fn each_run_of_an_in_subquery_reads_only_its_own_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path(), 20_000);
    let count = |sql: &str, v: i64| -> i64 {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(v))
            .unwrap();
        ints(stmt.run_collect_rows().unwrap())[0]
    };
    let mut small = conn
        .prepare("SELECT count(*) FROM t WHERE id IN (SELECT id FROM t WHERE v = ?)")
        .unwrap();
    let mut large = conn
        .prepare("SELECT count(*) FROM t WHERE id IN (SELECT id FROM t WHERE v <= ?)")
        .unwrap();
    for v in [3, 50, 3, 96, 0, 50] {
        small.reset().unwrap();
        small
            .bind_at(NonZero::new(1).unwrap(), Value::from_i64(v))
            .unwrap();
        assert_eq!(
            ints(small.run_collect_rows().unwrap())[0],
            count("SELECT count(*) FROM t WHERE v = ?", v),
            "v = {v}"
        );
        large.reset().unwrap();
        large
            .bind_at(NonZero::new(1).unwrap(), Value::from_i64(v))
            .unwrap();
        assert_eq!(
            ints(large.run_collect_rows().unwrap())[0],
            count("SELECT count(*) FROM t WHERE v <= ?", v),
            "v <= {v}"
        );
    }
}
