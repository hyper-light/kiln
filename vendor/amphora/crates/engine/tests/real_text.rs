//! A REAL in text is the text SQLite 3.54 writes for it (crates/engine/numeric/fp.rs, a port of its
//! `sqlite3FpDecode` and `%!.17g`): CAST(x AS TEXT), which is also what TEXT affinity stores, and
//! quote(x), for every double of a fixture SQLite itself answered (fixtures/real_text.rs: each power
//! of ten across the range, the edges of 15, 16 and 17 digits, subnormals, the integers about 2^53,
//! random doubles). The oracles found Amphora writing 1e15 as '1.0e+15' where SQLite writes
//! '1000000000000000.0', so rows stored through a TEXT column differed from SQLite's (bench/oracles,
//! G2 step 3; AC-T1.1.1: reads as committed; AC-T1.8.6: the differential suite).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
    StepResult, Value, IO,
};
use std::num::NonZero;
use std::sync::Arc;

// Generated data, one double to a line.
#[rustfmt::skip]
#[path = "fixtures/real_text.rs"]
mod fixture;

fn open() -> (Arc<MemoryIO>, Arc<Connection>) {
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io.clone(), "real_text.db", options).unwrap();
    (io, db.connect(ConnectOpts::new()).unwrap())
}

/// Do: CAST(?1 AS TEXT) and quote(?1) of each double, bound as a REAL. Expect: SQLite's own text
/// for each, every one of the fixture's.
#[test]
fn reals_render_as_sqlite_renders_them() {
    let (io, conn) = open();
    let mut wrong = Vec::new();
    for (bits, text, quoted) in fixture::CASES {
        let r = f64::from_bits(*bits);
        let mut stmt = conn.prepare("SELECT CAST(?1 AS TEXT), quote(?1)").unwrap();
        stmt.bind_at(NonZero::new(1).unwrap(), Value::from_f64(r))
            .unwrap();
        loop {
            match stmt.step().unwrap() {
                StepResult::Row => {
                    let row = stmt.row().unwrap();
                    let got: Vec<String> = row.get_values().map(|v| v.to_string()).collect();
                    if got != [*text, *quoted] {
                        wrong.push(format!(
                            "{r:e} ({bits:#018x}): {got:?}, SQLite {:?}",
                            [text, quoted]
                        ));
                    }
                }
                StepResult::Done => break,
                StepResult::IO => io.step().unwrap(),
                other => panic!("step: {other:?}"),
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} doubles render otherwise than SQLite's:\n{}",
        wrong.len(),
        fixture::CASES.len(),
        wrong
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Do: store doubles in a TEXT column, read them back. Expect: the text SQLite stores (the
/// oracles' first finding, a REAL into TEXT affinity).
#[test]
fn text_affinity_stores_sqlites_text() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(x TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1e15), (1e17), (0.1), (9007199254740993.0), (-1e-308)")
        .unwrap();
    let mut stmt = conn.prepare("SELECT x FROM t ORDER BY rowid").unwrap();
    let mut got = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                got.push(stmt.row().unwrap().get_values().next().unwrap().to_string())
            }
            StepResult::Done => break,
            StepResult::IO => io.step().unwrap(),
            other => panic!("step: {other:?}"),
        }
    }
    assert_eq!(
        got,
        [
            "1000000000000000.0",
            "1.0e+17",
            "0.1",
            "9007199254740992.0",
            "-1.0e-308"
        ]
    );
}
