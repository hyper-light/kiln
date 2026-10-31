//! Column affinity on WITHOUT ROWID tables (the unify audit's E1 and E2, open at e2e67247): a
//! WITHOUT ROWID table's row is its primary key's record, the key's columns first, but each value
//! still takes the affinity its own column declares, as SQLite 3.54.0 stores it ("Type Affinity",
//! sqlite.org/datatype3.html; T-T1.7.1: every record read back as written).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect, Value,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own, WITHOUT ROWID enabled as Amphora's Db opens its databases.
fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect))
        .db_opts(DatabaseOpts::new().with_without_rowid(true));
    let name = format!(
        "without_rowid_affinity_{}.db",
        N.fetch_add(1, Ordering::Relaxed)
    );
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Each row of `columns` from `table` (ordered by `order`) as each column's SQL literal and type,
/// `quote:type` joined by `|`.
fn typed(conn: &Arc<Connection>, columns: &[&str], table: &str, order: &str) -> Vec<String> {
    let each: Vec<String> = columns
        .iter()
        .map(|c| format!("quote({c}) || ':' || typeof({c})"))
        .collect();
    let sql = format!("SELECT {} FROM {table} {order}", each.join(", "));
    conn.prepare(&sql)
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| {
            row.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Do: store the audit's values in WITHOUT ROWID tables whose key isn't their first column, and in
/// one whose key column declares no type. Expect: each column keeps its own affinity, as SQLite
/// stores them: '1.0', '007', '1e3' and ' 12 ' stay text in TEXT columns, an untyped key keeps 5 an
/// integer, which `b = 5` finds and which '5' doesn't collide with (E1, E2; T-T1.7.1).
#[test]
fn a_without_rowid_column_keeps_its_own_affinity_after_the_key() {
    let conn = open();
    conn.execute("CREATE TABLE tail2(note TEXT, id INTEGER, PRIMARY KEY(id)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO tail2 VALUES('1.0', 1)").unwrap();
    assert_eq!(
        typed(&conn, &["note", "id"], "tail2", ""),
        ["'1.0':text|1:integer"]
    );
    conn.execute("CREATE TABLE w(a TEXT, b INTEGER, PRIMARY KEY(b)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO w VALUES('007',3),('1e3',4),(' 12 ',5)")
        .unwrap();
    assert_eq!(
        typed(&conn, &["a", "b"], "w", "ORDER BY b"),
        [
            "'007':text|3:integer",
            "'1e3':text|4:integer",
            "' 12 ':text|5:integer"
        ]
    );
    conn.execute("CREATE TABLE untyped(a TEXT, b, PRIMARY KEY(b)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO untyped VALUES('x', 5)").unwrap();
    assert_eq!(
        typed(&conn, &["a", "b"], "untyped", ""),
        ["'x':text|5:integer"]
    );
    assert_eq!(typed(&conn, &["a"], "untyped", "WHERE b = 5"), ["'x':text"]);
    conn.execute("INSERT INTO untyped VALUES('y', '5')")
        .unwrap();
    assert_eq!(
        typed(&conn, &["a", "b"], "untyped", "ORDER BY a"),
        ["'x':text|5:integer", "'y':text|'5':text"]
    );
}

/// Do: for tables of five columns, one of each affinity in several orders, keyed on one or two
/// columns that don't lead, insert and then update the same values into the table WITHOUT ROWID
/// and into its rowid twin. Expect: every column's stored value and type the same in both, the
/// rowid table's affinity being SQLite's (E1, E2; T-T1.7.1).
#[test]
fn a_without_rowid_table_stores_what_its_rowid_twin_stores() {
    let conn = open();
    let kinds = [
        ("t", "TEXT"),
        ("i", "INTEGER"),
        ("r", "REAL"),
        ("n", "NUMERIC"),
        ("u", ""),
    ];
    let orders: [[usize; 5]; 4] = [
        [0, 1, 2, 3, 4],
        [4, 3, 2, 1, 0],
        [2, 0, 4, 1, 3],
        [3, 4, 0, 2, 1],
    ];
    // The values, written in each column's turn: text that looks numeric, numbers, a blob.
    let values = [
        "'007'", "'1.0'", "'1e3'", "' 12 '", "5", "1.5", "x'00'", "'x'",
    ];
    let mut table = 0usize;
    for order in orders {
        for key in [vec![2usize], vec![4], vec![1, 3], vec![4, 2]] {
            let columns: Vec<String> = order
                .iter()
                .map(|&k| {
                    let (name, kind) = kinds[k];
                    format!("{name} {kind}").trim_end().to_owned()
                })
                .collect();
            let key_names: Vec<&str> = key.iter().map(|&p| kinds[order[p]].0).collect();
            let names: Vec<&str> = order.iter().map(|&k| kinds[k].0).collect();
            let (wr, rowid) = (format!("wr{table}"), format!("rw{table}"));
            table += 1;
            let body = format!(
                "{}, PRIMARY KEY({})",
                columns.join(", "),
                key_names.join(", ")
            );
            conn.execute(format!("CREATE TABLE {wr}({body}) WITHOUT ROWID"))
                .unwrap();
            conn.execute(format!("CREATE TABLE {rowid}({body})"))
                .unwrap();
            for (row, _) in values.iter().enumerate() {
                // Row `row` gives column c the value values[(row + c) % len]; its key columns get a
                // distinct integer as well, so keys never collide.
                let cells: Vec<String> = (0..names.len())
                    .map(|c| {
                        if key.contains(&c) {
                            format!("{}", row * 10 + c)
                        } else {
                            values[(row + c) % values.len()].to_owned()
                        }
                    })
                    .collect();
                for target in [&wr, &rowid] {
                    conn.execute(format!("INSERT INTO {target} VALUES({})", cells.join(", ")))
                        .unwrap();
                }
            }
            let order_by = format!("ORDER BY {}", key_names.join(", "));
            let select = |target: &str| typed(&conn, &names, target, &order_by);
            assert_eq!(select(&wr), select(&rowid), "inserted into {body}");
            // An update rewrites every non-key column with another value of the list.
            let sets: Vec<String> = (0..names.len())
                .filter(|c| !key.contains(c))
                .map(|c| format!("{} = {}", names[c], values[(c * 3 + 1) % values.len()]))
                .collect();
            for target in [&wr, &rowid] {
                conn.execute(format!("UPDATE {target} SET {}", sets.join(", ")))
                    .unwrap();
            }
            assert_eq!(select(&wr), select(&rowid), "updated in {body}");
        }
    }
}
