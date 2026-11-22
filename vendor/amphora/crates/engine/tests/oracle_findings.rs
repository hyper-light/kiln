//! The oracles' findings (bench/oracles, docs/research/oracles.md), each cut down to its minimal case
//! and answered as SQLite 3.54.0 answers it: a wrong row, a refusal SQLite doesn't make or a panic
//! found by TLP, NoREC, PQS, DQP, EET or the differential check stays found here once it is fixed
//! (G2 step 3; AC-T1.1.1: reads as committed; AC-T1.8.6: the differential suite).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, Numeric, OpenOptions, SqliteDialect,
    StepResult, Value, IO,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own: one opened by the same name is shared across a process's tests.
fn open() -> (Arc<MemoryIO>, Arc<Connection>) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let name = format!("oracle_findings_{}.db", N.fetch_add(1, Ordering::Relaxed));
    let db = Database::open(io.clone(), &name, options).unwrap();
    (io, db.connect(ConnectOpts::new()).unwrap())
}

/// The rows of `sql`, columns joined by `|`, each value as SQL writes it: NULL, an integer, a real,
/// 'text' or x'blob'.
fn rows(io: &MemoryIO, conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    written_rows(io, conn, sql, false)
}

/// The rows of `sql` as rows() writes them, but each REAL as Rust writes an f64, which keeps the sign
/// of a negative zero that SQL's text of a REAL leaves off: -0.0 is "-0.0" here and 0.0 to CAST.
fn signed_rows(io: &MemoryIO, conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    written_rows(io, conn, sql, true)
}

fn written_rows(io: &MemoryIO, conn: &Arc<Connection>, sql: &str, signed: bool) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let cols: Vec<String> = row
                    .get_values()
                    .map(|v| match v {
                        Value::Null => "NULL".to_owned(),
                        Value::Numeric(Numeric::Float(f)) if signed => {
                            format!("{:?}", f64::from(*f))
                        }
                        Value::Text(t) => format!("'{}'", t.as_str()),
                        Value::Blob(b) => format!(
                            "x'{}'",
                            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                        ),
                        v => v.to_string(),
                    })
                    .collect();
                out.push(cols.join("|"));
            }
            StepResult::Done => return out,
            StepResult::IO => io.step().unwrap(),
            other => panic!("step: {other:?}"),
        }
    }
}

/// Do: LEFT JOIN on a comparison of the right table's column with a constant that holds a BETWEEN,
/// which the planner answers with an automatic index on the right table filtered by that ON term.
/// Expect: SQLite's rows. Amphora panicked ("selected partial index predicate must be implied by
/// query"): an ON term keeps its BETWEEN where a WHERE term has it rewritten to two comparisons, and
/// the index's filter, copied from the ON term, was compared with it only after that rewrite
/// (finding tlp-4ff8ea015744079b, seed 11).
#[test]
fn a_left_join_on_a_constant_between_returns_sqlites_rows() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 INTEGER UNIQUE, c1 INTEGER COLLATE RTRIM PRIMARY KEY, c2 BOOLEAN COLLATE RTRIM, c3 INT NOT NULL)").unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT 1 FROM t0 AS a0 LEFT JOIN t0 AS a1 ON ((1.5 NOT BETWEEN x'' AND 3) < a1.c3)"
        ),
        Vec::<String>::new()
    );
    conn.execute("INSERT INTO t0 VALUES (1, 1, 0, 0), (2, 2, 1, 2), (3, 3, 0, 5)")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT a0.c1, a1.c1 FROM t0 AS a0 LEFT JOIN t0 AS a1 ON ((1.5 BETWEEN 0 AND 3) < a1.c3) ORDER BY 1, 2"),
        ["1|2", "1|3", "2|2", "2|3", "3|2", "3|3"]
    );
    assert_eq!(rows(&io, &conn, "SELECT a0.c1, a1.c1 FROM t0 AS a0 LEFT JOIN t0 AS a1 ON ((1.5 NOT BETWEEN 0 AND 3) + 5 < a1.c3) ORDER BY 1, 2"), ["1|NULL", "2|NULL", "3|NULL"]);
}

/// Do: JSON of REALs: json_array, json_object, json_set and JSONB read back as text. Expect: each REAL
/// as quote() writes it (SQLite 3.54's "%!0.17g", shortened while it reads back the same), not the
/// shortest digits (finding diff-af5ef200abdb39e2: 9.223372036854776e+18 for SQLite's
/// 9.2233720368547758e+18).
#[test]
fn json_writes_a_real_as_quote_does() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT json_array(NULL, 255, CAST(9223372036854775807 AS REAL)), json_array(1e-7, 0.1, 100.0, 123456789012345678.0), json_object('a', 0.1 + 0.2)"),
        ["'[null,255,9.2233720368547758e+18]'|'[1.0e-07,0.1,100.0,1.2345678901234568e+17]'|'{\"a\":0.30000000000000004}'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT json_set('{}', '$.a', 9223372036854775807.0), json(jsonb_array(9223372036854775807.0)), json_quote(1e300 * 1e10), json_array(-1e300 * 1e10)"),
        ["'{\"a\":9.2233720368547758e+18}'|'[9.2233720368547758e+18]'|'9.0e+999'|'[-9.0e+999]'"]
    );
}

/// Do: format() and printf() of an empty format, as TEXT and as a BLOB. Expect: '' as SQLite 3.54
/// gives (3.53 gave NULL), and NULL for a NULL format (findings diff-40dd229adb385868,
/// diff-654a3482133a8839, pqs-d3c0f00ee68ce1b7).
#[test]
fn format_of_an_empty_format_is_empty_text() {
    let (io, conn) = open();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT format(''), format(x''), printf(''), format('', 1), format(NULL)"
        ),
        ["''|''|''|''|NULL"]
    );
    conn.execute("CREATE TABLE t0(c0 INT, c1 DOUBLE, c2 VARCHAR(10) PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES ('', 'x', '0')")
        .unwrap();
    assert_eq!(rows(&io, &conn, "SELECT (format(a2.c0, '10') COLLATE RTRIM) FROM t0 AS a0 LEFT JOIN t0 AS a1 ON (('a ' <= 255) AND (a0.c1 ISNULL)), t0 AS a2"), ["''"]);
}

/// Do: printf's %j and %J (SQLite 3.54): escapes, NULL, precision in bytes and in characters,
/// width. Expect: SQLite's text. Amphora took them for unknown conversions and stopped there (found
/// beside the oracles' format('') findings: SQLite 3.54's printf documents both).
#[test]
fn printf_writes_json_strings_with_j() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT printf('%J', 'a\"b\\c' || char(10) || char(1) || char(127)), printf('%j', char(8, 9, 10, 12, 13, 31)), printf('%.2J', 'abcdef'), printf('%!.2J', 'éàü'), printf('%.2J', 'éàü')"),
        ["'\"a\\\"b\\\\c\\n\\u0001\u{7f}\"'|'\\b\\t\\n\\f\\r\\u001f'|'\"ab\"'|'\"éà\"'|'\"é\"'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT printf('%10J', 'ab'), printf('%-10J|', 'ab'), printf('%J', NULL), printf('[%j]', NULL), printf('%J', 12), printf('%J', 1.5), printf('%j', x'41')"),
        ["'      \"ab\"'|'\"ab\"      |'|'null'|'[]'|'\"12\"'|'\"1.5\"'|'A'"]
    );
}

/// Do: unhex(X, Y) with Y a number, and with Y's characters among X's digits. Expect: Y read as
/// text, its characters skipped only between pairs of digits, as SQLite does (findings
/// diff-88786185ab21e328, diff-e08cfad77a60e714: Amphora gave NULL for a number).
#[test]
fn unhex_reads_its_second_argument_as_text() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT unhex('10', 255), unhex(9007199254740993, 9007199254740993), unhex('1 0', ' '), unhex('10', NULL), unhex('-10-', '-')"),
        ["x'10'|x'9007199254740993'|NULL|NULL|x'10'"]
    );
}

/// Do: concatenate BLOBs, and a BLOB with a number. Expect: TEXT, as SQLite's || always gives
/// (finding diff-91e350f55dad4c41: Amphora gave a BLOB).
#[test]
fn concatenation_is_text() {
    let (io, conn) = open();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT x'61' || x'62', typeof(x'61' || x'62'), x'61' || 1, typeof(x'00' || x'00')"
        ),
        ["'ab'|'text'|'a1'|'text'"]
    );
    conn.execute("CREATE TABLE t1(c0 REAL COLLATE NOCASE PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (x'6162')").unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT NULL, (a0.c0 || a0.c0), NULL FROM t1 AS a0 GROUP BY NULL, a0.c0"
        ),
        ["NULL|'abab'|NULL"]
    );
}

/// Do: LIKE and GLOB with a BLOB on either side, beside a NULL. Expect: 0 every time, as SQLite
/// built with SQLITE_LIKE_DOESNT_MATCH_BLOBS answers (the option Amphora adopts,
/// docs/design/planner.md decision 3; bench/oracles/sqlite.sh builds the oracles' SQLite so): it
/// tests for a BLOB before a NULL, and the pattern as well as the text (finding
/// diff-d8adeec90811268d: Amphora gave NULL for glob(NULL, x'0102')).
#[test]
fn a_blob_matches_no_like_or_glob_even_beside_a_null() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT glob(NULL, x'01'), glob(x'01', NULL), x'41' LIKE NULL, NULL LIKE x'41', like('a', x'61', NULL), x'41' GLOB 'A', 'A' GLOB x'41', 'A' LIKE x'41', x'41' NOT LIKE 'a'"),
        ["0|0|0|0|0|0|0|0|1"]
    );
}

/// Do: the JSON functions that edit or read a document, given NULL for it, with paths SQLite would
/// refuse. Expect: NULL, as SQLite answers a NULL document before reading a path (findings
/// error-327aa6bdaaacbc20: Amphora refused jsonb_set(NULL, 0, 3); pqs-273539be46d477be:
/// jsonb_remove(NULL) was not NULL).
#[test]
fn json_of_a_null_document_is_null() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT jsonb_set(NULL, 0, 3), json_set(NULL, 'bad', 1), json_insert(NULL, 'bad', 1), json_replace(NULL, 'bad', 1), json_remove(NULL, 'bad'), jsonb_remove(NULL), json_remove(NULL), json_extract(NULL, 'bad'), jsonb_extract(NULL, 'bad'), json_type(NULL, 'bad'), json_array_length(NULL, 'bad'), json_patch(NULL, '{}')"),
        ["NULL|NULL|NULL|NULL|NULL|NULL|NULL|NULL|NULL|NULL|NULL|NULL"]
    );
}

/// Do: `CASE x WHEN y` with x a column of TEXT, INTEGER, NOCASE, RTRIM or no affinity, with y a
/// column, with x behind a unary +. Expect: each WHEN compared as `x = y` is, by the operands'
/// affinity and collation, as SQLite compares them (findings diff-6043d55fb3d98918,
/// pqs-547601b066301724: Amphora compared without affinity, so '1' never matched 1).
#[test]
fn case_compares_as_equality_does() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(a TEXT COLLATE RTRIM, b INTEGER, c TEXT COLLATE NOCASE, d)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('1', '5', 'abc', '7')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT CASE a WHEN 1 THEN 'one' ELSE 'other' END, CASE b WHEN '5' THEN 'five' ELSE 'other' END, CASE c WHEN 'ABC' THEN 'nocase' ELSE 'binary' END, CASE 'ABC' WHEN c THEN 'nocase' ELSE 'binary' END, CASE d WHEN 7 THEN 'seven' ELSE 'other' END, CASE 7 WHEN d THEN 'seven' ELSE 'other' END, CASE 1 WHEN a THEN 'one' ELSE 'other' END, CASE a WHEN '1  ' THEN 'rtrim' ELSE 'binary' END, CASE +a WHEN 1 THEN 'one' ELSE 'other' END FROM t"
        ),
        ["'one'|'five'|'nocase'|'nocase'|'other'|'other'|'one'|'rtrim'|'other'"]
    );
    conn.execute(
        "CREATE TABLE t1(c0 COLLATE NOCASE, c1 INTEGER, c2 TEXT COLLATE RTRIM, c3 BLOB NOT NULL)",
    )
    .unwrap();
    conn.execute("INSERT INTO t1 VALUES ('0x10', 7, 1, -1)")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT (CASE a0.c2 WHEN 1 THEN NULL WHEN 2.0 THEN NULL ELSE 0 END) FROM t1 AS a0"
        ),
        ["NULL"]
    );
}

/// Do: `x IN (y, ...)` with x or a y a column of some collation, a y with COLLATE, and x and y of
/// different affinities. Expect: x's affinity and x's collation alone, as SQLite's IN takes them
/// (finding diff-125ad13d60da0c20: Amphora compared each y by the collation of the y before it).
#[test]
fn in_compares_by_its_left_operand() {
    let (io, conn) = open();
    conn.execute(
        "CREATE TABLE t(a TEXT COLLATE RTRIM, b INTEGER, c TEXT COLLATE NOCASE, d, e REAL)",
    )
    .unwrap();
    conn.execute("INSERT INTO t VALUES ('1', '5', 'abc', '7', 'ABC')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT c IN ('ABC'), c IN ('x', d, 'ABC'), 'ABC' IN (c), a IN (1), b IN ('5'), d IN (7), '5' IN (b), c IN (e), e IN (c), c IN ('x', NULL), 'ABC' IN ('abc' COLLATE NOCASE), 'ABC' COLLATE NOCASE IN ('abc') FROM t"
        ),
        ["1|1|0|1|1|0|0|1|0|NULL|0|1"]
    );
    conn.execute(
        "CREATE TABLE t0(c0 NUMERIC COLLATE RTRIM, c1 REAL, c2 INT COLLATE NOCASE PRIMARY KEY)",
    )
    .unwrap();
    conn.execute("INSERT INTO t0 VALUES (-9223372036854775808, 'ABC', 'abc')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT NULL FROM t0 AS a0 JOIN t0 AS a1 ON (a0.c2 IN ('10', a0.c1, NULL))"
        ),
        ["NULL"]
    );
}

/// Do: a WHERE term that is a COLLATE alone. Expect: its operand's truth, as SQLite takes it
/// (finding error-40f23311aa753214: Amphora refused "Collate in WHERE clause is not supported").
#[test]
fn a_collate_alone_is_its_operands_truth() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 BLOB PRIMARY KEY, c1 TEXT COLLATE NOCASE)")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT NULL FROM t0 AS a0 WHERE (0.1 COLLATE RTRIM)"
        ),
        Vec::<String>::new()
    );
    conn.execute("INSERT INTO t0 VALUES (1, 'a')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT c1 FROM t0 WHERE (0.1 COLLATE RTRIM)"),
        ["'a'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT c1 FROM t0 WHERE (0 COLLATE NOCASE)"),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(&io, &conn, "SELECT c1 FROM t0 WHERE c1 COLLATE BINARY"),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT c1 FROM t0 WHERE NOT (c1 COLLATE BINARY)"
        ),
        ["'a'"]
    );
}

/// Do: HAVING with subqueries that read the grouped query's columns, as outer references or as
/// IN's left side, where no other clause names them. Expect: each subquery sees the group's row,
/// as SQLite carries those columns with it (findings diff-8dff79b31f30b7a0,
/// diff-abc98f0b19b0bf2e: Amphora read NULL from the scan's spent cursor and dropped the group).
#[test]
fn having_subqueries_read_the_groups_row() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c3 TEXT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES ('x')").unwrap();
    conn.execute("CREATE TABLE t1(c1 TEXT)").unwrap();
    conn.execute("INSERT INTO t1 VALUES ('y')").unwrap();
    for sql in [
        "SELECT count(*) FROM t0 AS a0 GROUP BY a0.c3 HAVING a0.c3 IN (SELECT 'x' FROM t1)",
        "SELECT count(*) FROM t0 AS a0 GROUP BY a0.c3 HAVING EXISTS (SELECT 1 FROM t1 WHERE a0.c3 = 'x')",
        "SELECT count(*) FROM t0 AS a0 GROUP BY a0.c3 HAVING (SELECT 1 FROM t1 AS s WHERE a0.c3 IS NOT NULL) IS NOT NULL",
        "SELECT count(*) FROM t0 AS a0 GROUP BY a0.c3 HAVING (SELECT a0.c3 FROM t1 AS s) IS NOT NULL",
        "SELECT count(*) FROM t0 AS a0 GROUP BY 'g' HAVING EXISTS (SELECT 1 FROM t1 AS s WHERE a0.c3 = 'x')",
        "SELECT count(*) FROM t0 AS a0, t1 AS a2 GROUP BY 'g' HAVING EXISTS (SELECT 1 FROM t1 AS s WHERE a2.c1 <> a0.c3)",
        "SELECT count(*) FROM t0 AS a0 GROUP BY a0.c3 HAVING EXISTS (SELECT 1 WHERE EXISTS (SELECT 1 FROM t1 WHERE a0.c3 = 'x'))",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["1"], "{sql}");
    }
}

/// Do: AND with a literal 0 or FALSE on either side, OR with a literal nonzero integer or TRUE,
/// beside an operand that raises an error; and the same with a REAL or a negated 0, which SQLite
/// does not fold. Expect: 0 and 1 without the error where SQLite folds them as it parses (finding
/// error-493c667725cb13ff: Amphora evaluated json('x') in `0 AND json('x')`).
#[test]
fn a_literal_false_and_or_true_or_skips_the_other_side() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT 0 AND json('x'), json('x') AND 0, FALSE AND json('x'), (0) AND json('x'), 1 OR json('x'), json('x') OR 2, TRUE OR json('x'), NOT (0 AND json('x')), 1 AND 5, 1 AND 'abc', 0 OR 5"),
        ["0|0|0|0|1|1|1|1|1|0|1"]
    );
    conn.execute("CREATE TABLE t0(c1 TEXT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES ('a')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT c1 FROM t0 WHERE NOT (0 AND json('x'))"),
        ["'a'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT CASE WHEN json('x') AND 0 THEN 1 ELSE 2 END FROM t0"
        ),
        ["2"]
    );
    for sql in [
        "SELECT 0.0 AND json('x')",
        "SELECT -0 AND json('x')",
        "SELECT 1 AND json('x')",
        "SELECT 0 OR json('x')",
    ] {
        assert!(
            conn.prepare(sql)
                .and_then(|mut stmt| loop {
                    match stmt.step()? {
                        StepResult::IO => io.step()?,
                        StepResult::Done => break Ok(()),
                        _ => {}
                    }
                })
                .is_err(),
            "{sql} raises json's error, as in SQLite"
        );
    }
}

/// Do: json_array_insert and jsonb_array_insert (SQLite 3.54) at an index within the array, at its
/// end, past it, from its end (#-1), through a key that is missing, with no path past `$`, with
/// a NULL path or document, and with a path that ends in a key. Expect: SQLite's answers, an
/// error for the key where SQLite raises one (findings error-033d5f13426f5305,
/// error-f64f26a38065868e: Amphora had no such function).
#[test]
fn json_array_insert_inserts_as_sqlite_does() {
    let (io, conn) = open();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT json_array_insert('[1,2,3]', '$[1]', 'new'), json_array_insert('{\"a\":[1,2,3]}', '$.a[0]', 'new'), json_array_insert('[1,2,3]', '$[3]', 9), json_array_insert('[1,2,3]', '$[7]', 9), json_array_insert('[1,2,3]', '$[#]', 9), json_array_insert('[1,2,3]', '$[#-1]', 9), json_array_insert('[1,2,3]', '$[#-9]', 9)"
        ),
        ["'[1,\"new\",2,3]'|'{\"a\":[\"new\",1,2,3]}'|'[1,2,3,9]'|'[1,2,3]'|'[1,2,3,9]'|'[1,2,9,3]'|'[1,2,3]'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT json_array_insert('[1,2,3]', '$.a', 9), json_array_insert('{\"a\":1}', '$.a[0]', 9), json_array_insert('{\"a\":1}', '$.b[0]', 9), json_array_insert('[1,[2]]', '$[1][0]', 9), json_array_insert('[1,2]', '$[0]', 9, '$[0]', 8), json_array_insert(NULL, '$[0]', 1), json_array_insert('[1]', NULL, 1), json_array_insert('[1]', '$[0]', NULL), json_array_insert('[1]', '$', 1), json_array_insert('[1]'), json_array_insert('[[1]]', '$[0][#]', 9)"
        ),
        ["'[1,2,3]'|'{\"a\":1}'|'{\"a\":1,\"b\":[9]}'|'[1,[9,2]]'|'[8,9,1,2]'|NULL|'[1]'|'[null,1]'|'[1]'|'[1]'|'[[1,9]]'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT hex(jsonb_array_insert('[1]', '$[0]', 2))"
        ),
        ["'4B13321331'"]
    );
    for sql in [
        "SELECT json_array_insert('{\"x\":1}', '$.y', 2)",
        "SELECT json_array_insert('{\"x\":1}', '$.x', 2)",
        "SELECT json_array_insert('[1,2]', '$[0]')",
        "SELECT json_array_insert('[1,2,3]', '$[3].x', 9)",
    ] {
        assert!(
            conn.prepare(sql)
                .and_then(|mut stmt| loop {
                    match stmt.step()? {
                        StepResult::IO => io.step()?,
                        StepResult::Done => break Ok(()),
                        _ => {}
                    }
                })
                .is_err(),
            "{sql} is refused, as SQLite refuses it"
        );
    }
}

/// Do: json_object and jsonb_object with an odd number of arguments where the query never
/// evaluates them. Expect: no error, as SQLite counts their arguments when it runs them (the
/// oracles' refusals "json_object function requires an even number of arguments", 19 in
/// campaign1); evaluated, the error.
#[test]
fn json_object_counts_its_arguments_when_it_runs() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(x)").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT json_object('a') FROM t"),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT CASE WHEN 0 THEN jsonb_object('a', 1, 'b') END"
        ),
        ["NULL"]
    );
    assert!(conn
        .prepare("SELECT json_object('a')")
        .and_then(|mut stmt| loop {
            match stmt.step()? {
                StepResult::IO => io.step()?,
                StepResult::Done => break Ok(()),
                _ => {}
            }
        })
        .is_err());
}

/// The rows the plan of `sql` estimates it returns, as the oracles' CERT check reads them: the
/// plan's own `output_rows` (the join's).
fn estimate(io: &MemoryIO, conn: &Arc<Connection>, sql: &str) -> f64 {
    let plan = rows(io, conn, &format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"));
    let text = plan.first().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&text[1..text.len() - 1].replace("''", "'")).unwrap();
    json["output_rows"].as_f64().unwrap()
}

/// Whether `more` is no more than `base`, beyond the rounding of floating point (the oracles' CERT
/// tolerance: relative 1e-9).
fn no_more(more: f64, base: f64) -> bool {
    more <= base * (1.0 + 1e-9) + 1e-9
}

/// Do: estimate a query, then the same query with one more conjunct, which the planner answers
/// with a multi-index OR over its disjuncts. Expect: no more rows estimated than without it (CERT;
/// finding cert-d07da3204b082fa0: 400000.6 rows against 5000, as the OR's branch terms, numbered
/// in the branch, marked the query's first term consumed).
#[test]
fn a_multi_index_or_keeps_the_other_terms_selectivity() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 BLOB PRIMARY KEY)")
        .unwrap();
    let base = estimate(
        &io,
        &conn,
        "SELECT NULL FROM t1 AS a0 WHERE (a0.c0 < a0.c0)",
    );
    let more = estimate(&io, &conn, "SELECT NULL FROM t1 AS a0 WHERE ((a0.c0 < a0.c0) AND (('1' == a0.c0) OR (NULL BETWEEN 9223372036854775807 AND a0.c0)))");
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: estimate joins whose terms are registered on one table and completed by another, with an
/// inner join in place of a LEFT JOIN, and with a conjunct that changes the join order. Expect: the
/// more restrictive query estimated at no more rows (CERT; findings cert-773031135e2103ca:
/// 2e11 rows against 1.458e11, and cert-d7adf5887d0edbc2: 1e12 against 4e11, as a term was
/// counted only when its own table was read last).
#[test]
fn a_term_counts_where_the_join_completes_it() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 INTEGER PRIMARY KEY NOT NULL)")
        .unwrap();
    let left = estimate(&io, &conn, "SELECT NULL FROM t0 AS a0 LEFT JOIN t0 AS a1 ON ((a1.c0 NOT IN (NULL, 1, a0.c0)) AND (a1.c0 GLOB '_'))");
    let inner = estimate(&io, &conn, "SELECT NULL FROM t0 AS a0 JOIN t0 AS a1 ON ((a1.c0 NOT IN (NULL, 1, a0.c0)) AND (a1.c0 GLOB '_'))");
    assert!(
        no_more(inner, left),
        "{inner} rows estimated for the inner join, {left} for the left join"
    );
    conn.execute("CREATE TABLE t1(c0 DECIMAL(10,2), c1 VARCHAR(10) PRIMARY KEY NOT NULL)")
        .unwrap();
    let from = "SELECT abs((+ a1.c0)) FROM t1 AS a0, t1 AS a1 JOIN t1 AS a2 ON (a2.c1 IN (json_extract('{\"a\":null}', '$[0]'))) WHERE ";
    let term = "((a2.c1 == a1.c0) < ((a0.c0 IS DISTINCT FROM 'abc') COLLATE BINARY))";
    let base = estimate(&io, &conn, &format!("{from}{term}"));
    let more = estimate(&io, &conn, &format!("{from}({term} AND ((a0.c1 NOT IN ('')) IN ((2.0 IS NOT DISTINCT FROM a2.c0), json_extract('{{\"a\":1,\"b\":[2,3]}}', '$'), (a2.c1 << 7))))"));
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: estimate a RIGHT JOIN whose WHERE bounds a column on both sides, then the same with a
/// conjunct that makes it an inner join read in the other order, where one bound is ready at the
/// first table and the other only at the second. Expect: no more rows estimated (CERT; finding
/// cert-183ef92e38891107: 0.16 rows against 0.0068, as the two bounds counted as two ranges
/// when read apart and as one closed range when read together).
#[test]
fn bounds_read_apart_make_the_same_range() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 DECIMAL(10,2) UNIQUE, c1 , c2 TEXT COLLATE RTRIM NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES ('a ', 255, ' ')")
        .unwrap();
    conn.execute("ANALYZE").unwrap();
    let from = "SELECT NULL FROM t0 AS a0 RIGHT JOIN t0 AS a1 ON NULL WHERE ";
    let term = "(a1.c2 BETWEEN unixepoch(NULL, 9007199254740993, a0.c0) AND NULL)";
    let base = estimate(&io, &conn, &format!("{from}{term}"));
    let more = estimate(&io, &conn, &format!("{from}({term} AND a0.c1)"));
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: substr with starts far below the first character, with no length and with the smallest
/// length. Expect: SQLite 3.54's text, which takes the largest integer for a missing length
/// (findings diff-db01a8bc85316606: Amphora gave '' for substr(10, -3.7e18), and
/// diff-af5b1131b3704f85).
#[test]
fn substr_takes_the_largest_length_when_none_is_given() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT substr('-9223372036854775808', -9223372036854775808), substr(10, -3.6893488147419103e18), substr('abc', -5), substr('abc', -2), substr('abc', 0), substr('abc', 9223372036854775807), substr('abc', -9223372036854775807), substr('abcdef', 2, -9223372036854775808)"),
        ["'-922337203685477580'|'10'|'abc'|'bc'|'abc'|''|'abc'|'a'"]
    );
    conn.execute("CREATE TABLE t0(c0 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (-2.5)").unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT 100 FROM t0 AS a0 WHERE substr(10, (9223372036854775807 / a0.c0))"
        ),
        ["100"]
    );
}

/// Do: zeroblob of text and a BLOB that begin with an integer, and of a REAL. Expect: as many
/// zero bytes as the integer they begin with, as SQLite reads them (finding
/// diff-c34e3acbde539eab: Amphora gave x'' for zeroblob('3.0')).
#[test]
fn zeroblob_reads_the_integer_text_begins_with() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT length(zeroblob('3.0')), length(zeroblob('3.9')), length(zeroblob(2.9)), length(zeroblob('x')), length(zeroblob('2e0')), length(zeroblob('1e1')), length(zeroblob(' 3')), length(zeroblob('+4x')), length(zeroblob(x'35'))"),
        ["3|3|2|0|2|1|3|4|5"]
    );
}

/// Do: the JSON functions that give a whole document, given the JSON null. Expect: the document
/// `null` (text, or the JSONB x'00'), not SQL NULL, as SQLite gives it (finding
/// pqs-9ecd014772c7388d: NOT json_insert(x'00') was NULL, so its pivot went missing).
#[test]
fn a_json_null_document_is_the_text_null() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT json_insert(x'00'), typeof(json_insert(x'00')), json_set(x'00'), json_remove(x'00'), json_patch('{\"a\":1}', 'null'), hex(jsonb_insert('null')), json_replace('null'), json_extract('null', '$')"),
        ["'null'|'text'|'null'|'null'|'null'|'00'|'null'|NULL"]
    );
}

/// Do: NOT over an OR (a NOT BETWEEN) and over an AND as a WHERE condition, where the first
/// operand decides some rows, TRUE or NULL, and the second would fail on them. Expect: SQLite's
/// count, without the failure: NOT keeps its operand's short circuits, and where NULL and TRUE
/// go to one place an OR's NULL left side settles it (finding error-493c667725cb13ff in the second
/// campaign: Amphora evaluated NOT's operand as a value and read jsonb('A')).
#[test]
fn not_keeps_its_operands_short_circuits() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(c0, c2)").unwrap();
    conn.execute("INSERT INTO t VALUES ('A', 5), ('[1]', -1), ('A', NULL), ('3', 0.0)")
        .unwrap();
    for (sql, count) in [
        (
            "SELECT count(*) FROM t WHERE NOT (0 NOT BETWEEN c2 AND jsonb(c0))",
            "2",
        ),
        (
            "SELECT count(*) FROM t WHERE NOT (c2 > 0 OR json(c0) IS NULL)",
            "2",
        ),
        (
            "SELECT count(*) FROM t WHERE NOT NOT (c2 < 0 AND json(c0) IS NOT NULL)",
            "1",
        ),
    ] {
        assert_eq!(rows(&io, &conn, sql), [count], "{sql}");
    }
}

/// Do: replace() with an empty pattern beside a NULL replacement, and with an empty BLOB as the
/// pattern. Expect: the text unchanged, as SQLite looks at the pattern before the replacement
/// (findings diff-9dd429c4533ccdda, diff-80c10ac4f2640ff6, diff-af2d686b405d659d: Amphora gave
/// NULL).
#[test]
fn replace_with_an_empty_pattern_keeps_the_text() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT replace(255, x'', NULL), replace('abc', '', NULL), replace(NULL, '', 'x'), replace('abc', NULL, 'x'), replace('abc', 'b', NULL), replace('abc', 'b', 1)"),
        ["'255'|'abc'|NULL|NULL|NULL|'a1c'"]
    );
}

/// Do: AND and OR with an operand that SQLite's parser makes a literal, `'1.5' NOTNULL` or
/// `x'' IS NULL`, beside one that fails. Expect: SQLite's answer without the failure (findings
/// error-327aa6bdaaacbc20 and error-9c5ff875d92402a3 in the second campaign).
#[test]
fn a_literals_null_test_folds_like_a_literal() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT json_array_length(10, 0) OR ('1.5' NOTNULL), json_set('x', NULL) OR (1.5 NOTNULL), (x'' IS NULL) AND json('x'), (- 2 IS NOT NULL) OR json('x'), 'a' ISNULL"),
        ["1|1|0|1|0"]
    );
}

/// Do: a searched CASE whose WHEN is an AND that a NULL settles, beside an operand that fails.
/// Expect: SQLite's answer, the WHEN a condition with its short circuits (finding
/// error-95a30b3d763ff2ad in the second campaign: Amphora read jsonb_patch(x'', 0)).
#[test]
fn a_case_when_is_a_condition() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 NUMERIC NOT NULL, c1 BOOLEAN)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES (x'', 3)").unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT (CASE WHEN (NULL AND jsonb_patch(a0.c0, 0)) THEN 1 ELSE 0 END) FROM t0 AS a0"
        ),
        ["0"]
    );
    assert_eq!(rows(&io, &conn, "SELECT CASE WHEN c1 > 2 OR json(c0) THEN 'a' ELSE 'b' END, CASE WHEN c1 < 2 THEN 'c' WHEN c1 = 3 THEN 'd' END FROM t0"), ["'a'|'d'"]);
}

/// Do: jsonb_array and jsonb_object of text holding control characters. Expect: SQLite's JSONB,
/// which escapes them (\u0001, TEXTJ) where Amphora kept them raw (finding diff-beeb2de1b1ede67c
/// in the second campaign).
#[test]
fn jsonb_escapes_control_characters_in_text() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT hex(jsonb_array('a' || char(1))), hex(jsonb_array((x'00' || 0), -1e-308)), json_array('a' || char(1) || 'b'), hex(jsonb_object('k', char(9)))"),
        ["'8B78615C7530303031'|'CB12785C753030303030952D312E30652D333038'|'[\"a\\u0001b\"]'|'5C176B285C74'"]
    );
}

/// Do: a constant function call as the left side of a BETWEEN in a join's ON, read twice (one
/// comparison each side). Expect: SQLite's count. Computing such a call where first reached (as
/// SQLite does) had left the copy for its second read at the program's start, before anything was
/// kept to copy, so no row matched (finding diff-b87f0f152642840a in the second campaign).
#[test]
fn a_constant_call_read_twice_is_copied_where_it_is_read() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 NOT NULL, c1 INT, c2 TEXT COLLATE RTRIM PRIMARY KEY, c3 INT COLLATE NOCASE)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (0.1, 3, '1.5', '_'), (0, '%', 'ABC', -2.5)")
        .unwrap();
    for sql in [
        "SELECT count(*) FROM t1 AS a0 JOIN t1 AS a1 ON (abs(0) BETWEEN 0.0 AND a1.c1)",
        "SELECT count(*) FROM t1 AS a0 LEFT JOIN t1 AS a1 ON (json_valid(' ', '1') BETWEEN 0.0 AND a1.c1)",
        "SELECT count(*) FROM t1 AS a0 WHERE abs(-1) = 1 AND abs(-1) < a0.c1 + 1",
    ] {
        assert_eq!(rows(&io, &conn, sql), [if sql.contains("WHERE") { "1" } else { "4" }], "{sql}");
    }
}

/// Do: estimate a join the planner answers with a hash join, then one more restrictive that it
/// answers with an index. Expect: the plan's own estimate (`output_rows`, the join's) no more for
/// the second; read off the last node listed, a hash join's build input, it had seemed 1e6 rows
/// against the index plan's 1e9 (finding cert-d591d139515a7b98 in the second campaign).
#[test]
fn a_hash_joins_estimate_is_the_joins() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 INTEGER, c1 INTEGER COLLATE BINARY)")
        .unwrap();
    conn.execute("CREATE TABLE t2(c0 VARCHAR(10) COLLATE BINARY, c1 NUMERIC UNIQUE, c2 INTEGER NOT NULL, c3 DOUBLE)").unwrap();
    let from = "SELECT ('A' AND (NOT a1.c0)) FROM t2 AS a0, t1 AS a1 WHERE ";
    let term = "(a0.c3 = (CAST(0.5 AS INTEGER) << (a1.c1 <= a1.c1)))";
    let base = estimate(&io, &conn, &format!("{from}{term}"));
    let more = estimate(
        &io,
        &conn,
        &format!("{from}({term} AND (json_extract('{{\"a\":null}}', '$.b[1]') IS a0.c1))"),
    );
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: compare a function of a NOCASE column, a concatenation of it, the column behind + and
/// CAST, and the same with an explicit COLLATE inside. Expect: SQLite's answers: a function's
/// value and an operator's carry an explicit COLLATE of their operands but not a column's
/// collation, which + and CAST keep (finding pqs-c624714a4abc1e56 in the second campaign:
/// format('abc', c) IN (NULL, c) compared by NOCASE).
#[test]
fn a_columns_collation_stops_at_a_function() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(c TEXT COLLATE NOCASE)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('ABC')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT upper(c) = 'abc', c = 'abc', (c || '') = 'abc', upper(c COLLATE NOCASE) = 'abc', format('abc', c) IN (NULL, c), (+c) = 'abc', CAST(c AS TEXT) = 'abc', (c COLLATE NOCASE || '') = 'abc', (c + 0) = 'abc' FROM t"),
        ["0|1|0|1|NULL|1|1|1|0"]
    );
}

/// Do: text and BLOBs that hold a NUL as printf's and format's format and arguments, and as
/// unhex's text; strftime with a REAL as its format. Expect: SQLite's answers: its functions read
/// text to its first NUL, and a REAL as its own text (findings diff-ef836c597f80f233,
/// diff-5d93535ab30e1a1f, diff-0fd2c9fd1e1b32fd, pqs-b90da0b3526a0c3b, diff-0085db196704f7c6 in
/// the second campaign).
#[test]
fn functions_read_text_to_its_first_nul() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT hex(printf(jsonb_array(NULL), NULL, NULL)), hex(format(x'00', 'a ')), hex(format('a' || char(0) || 'b')), hex(printf('%s|%q|%w', 'a' || char(0) || 'b', 'c' || char(0) || 'd', 'e' || char(0) || 'f')), hex(unhex(x'00', 0)), hex(unhex('10' || char(0) || 'zz', 0)), strftime(-9223372036854775808.0, 0), strftime(0.5, 0)"),
        ["'1B'|''|'61'|'617C637C65'|''|'10'|'-9.2233720368547758e+18'|'0.5'"]
    );
}

/// Do: json_remove and jsonb_remove with a NULL among their paths. Expect: NULL, as SQLite gives
/// (finding pqs-75e02faf1f64380f in the second campaign: Amphora skipped the NULL path).
#[test]
fn json_remove_of_a_null_path_is_null() {
    let (io, conn) = open();
    assert_eq!(rows(&io, &conn, "SELECT json_remove(1e308, NULL), json_remove('[1,2]', '$[0]', NULL), jsonb_remove('[1]', NULL)"), ["NULL|NULL|NULL"]);
}

/// Do: sum() over text holding integers that overflow, then the same with text that is no number
/// after them. Expect: SQLite's answers: the overflow is an error, unless a value that is no
/// integer follows it and the floating-point total stands (finding error-99fb1658e34f78cb in the
/// second campaign).
#[test]
fn sum_forgets_an_overflow_a_non_integer_follows() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t2(c3 VARCHAR(10) NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO t2 VALUES (10), (9223372036854775807), ('ABC')")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT sum(c3), typeof(sum(c3)) FROM t2"),
        ["9.2233720368547758e+18|'real'"]
    );
    assert!(
        conn.prepare("SELECT sum(c3) FROM t2 WHERE rowid < 3")
            .and_then(|mut stmt| loop {
                match stmt.step()? {
                    StepResult::IO => io.step()?,
                    StepResult::Done => break Ok(()),
                    _ => {}
                }
            })
            .is_err(),
        "the overflow alone is an error"
    );
}

/// Do: `x IN (SELECT y ...)` with x a column of no collation and y one of NOCASE or RTRIM, with x
/// a literal, and with COLLATE on either side. Expect: SQLite's answers: x and y compare as
/// `x = y` does, a column on the left (BINARY too) before one on the right (finding
/// diff-6f48f87d917c2ec9 in the second campaign: '' matched ' ' by the subquery column's RTRIM).
#[test]
fn in_subquery_collates_as_equality_does() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE a(x)").unwrap();
    conn.execute("CREATE TABLE b(y TEXT COLLATE NOCASE)")
        .unwrap();
    conn.execute("INSERT INTO a VALUES ('ABC')").unwrap();
    conn.execute("INSERT INTO b VALUES ('abc')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT x IN (SELECT y FROM b), 'ABC' IN (SELECT y FROM b), x IN (SELECT y COLLATE BINARY FROM b), (x COLLATE NOCASE) IN (SELECT y FROM b), x IN (SELECT +y FROM b) FROM a"),
        ["0|1|0|1|0"]
    );
    conn.execute("CREATE TABLE t2(c0 DOUBLE, c1 DECIMAL(10,2) COLLATE RTRIM UNIQUE)")
        .unwrap();
    conn.execute("INSERT INTO t2 VALUES (0.0, NULL), ('', 'ABC'), (NULL, ' ')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT a1.c0 IN (SELECT s.c1 FROM t2 AS s WHERE s.c1 IS NOT NULL) FROM t2 AS a1"
        ),
        ["0", "0", "NULL"]
    );
}

/// Do: DISTINCT and UNION over a function of an RTRIM column and over a concatenation of it, and
/// DISTINCT over the column behind +. Expect: SQLite's rows: a function or an operator carries no
/// column collation, so '' and ' ' stay apart (BINARY), while +c is the column and dedups by RTRIM
/// (finding tlp-fe2fef30016eedb6 in the second campaign: a UNION of TLP's partitions merged them).
#[test]
fn distinct_and_union_carry_no_collation_through_a_function() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(c TEXT COLLATE RTRIM)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (''), (' '), ('x')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT DISTINCT printf('%s', c) FROM t ORDER BY 1"
        ),
        ["''", "' '", "'x'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT DISTINCT c || '' FROM t ORDER BY 1"),
        ["''", "' '", "'x'"]
    );
    assert_eq!(rows(&io, &conn, "SELECT printf('%s', c) FROM t WHERE c = '' UNION SELECT printf('%s', c) FROM t WHERE c <> '' ORDER BY 1"), ["''", "' '", "'x'"]);
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT count(*) FROM (SELECT DISTINCT +c FROM t)"
        ),
        ["2"]
    );
}

/// Do: a RIGHT JOIN followed by a LEFT JOIN whose ON holds a subquery that reads the RIGHT JOIN's
/// left table. Expect: SQLite's row: the unmatched row sees that table's NULL row in the subquery
/// too (finding diff-47f77ee9651e9862 in the second campaign: the subquery ran in the left table's
/// loop, which the unmatched pass skips, and kept the value of its last row).
#[test]
fn a_right_joins_unmatched_rows_rerun_their_subqueries() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 BOOLEAN, c1 REAL PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (x'', '1')").unwrap();
    conn.execute(
        "CREATE TABLE t2(c0 INTEGER PRIMARY KEY, c1 INT, c2 DECIMAL(10,2) COLLATE NOCASE, c3 TEXT)",
    )
    .unwrap();
    conn.execute("INSERT INTO t2 VALUES (3, 0, -2.5, -1e-308)")
        .unwrap();
    for sql in [
        "SELECT a2.c0 FROM t2 AS a0 RIGHT JOIN t1 AS a1 ON NULL LEFT JOIN t1 AS a2 ON (SELECT 1 WHERE a0.c2 IS NOT NULL)",
        "SELECT a2.c0 FROM t2 AS a0 RIGHT JOIN t1 AS a1 ON NULL LEFT JOIN t1 AS a2 ON (SELECT sum(s.c1) FROM t1 AS s WHERE (255 > a0.c2))",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["NULL"], "{sql}");
    }
}

/// Do: conditions whose NULL lands under a NOT: NOT LIKE, arithmetic and an AND's or OR's left side.
/// Expect: SQLite's rows. NOT swaps where TRUE and FALSE go and leaves NULL where it went, so a NULL
/// can go where TRUE does; NOT LIKE and an arithmetic condition sent their NULL where FALSE goes
/// whatever the condition said, and an AND's or OR's NULL left side went to the NULL's place even
/// where it doesn't settle the result (NULL AND 0 is FALSE, NULL OR 0 is NULL), where SQLite reads
/// the right side (findings diff-52a31f909cba9309, diff-8b2bec437888edcb, diff-a62d3a04048b3976 and
/// more of seed 20261024).
#[test]
fn a_null_under_not_goes_where_its_condition_sends_it() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0, c1)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (0, NULL)").unwrap();
    for (sql, expected) in [
        ("SELECT 1 FROM t0 WHERE NOT (NULL NOT GLOB '*')", 0),
        ("SELECT 1 FROM t0 WHERE NOT (c1 NOT LIKE 'a')", 0),
        ("SELECT 1 FROM t0 WHERE (NOT (c1 NOT LIKE 'a')) OR c0", 0),
        ("SELECT 1 FROM t0 WHERE NOT (c0 + c1)", 0),
        ("SELECT 1 FROM t0 WHERE NOT (c1 & 1)", 0),
        ("SELECT 1 FROM t0 WHERE NOT (NOT (c0 + c1))", 0),
        ("SELECT 1 FROM t0 WHERE NOT (NULL AND c0)", 1),
        ("SELECT 1 FROM t0 WHERE NOT (c1 AND c0)", 1),
        ("SELECT 1 FROM t0 WHERE NOT (c1 AND NOT c0)", 0),
        ("SELECT 1 FROM t0 WHERE (NOT (NULL OR c0)) OR c0", 0),
        ("SELECT 1 FROM t0 WHERE (NOT (c1 OR c0)) OR NOT c0", 1),
        ("SELECT 1 FROM t0 WHERE NOT ((c1 AND c0) OR c1)", 0),
        ("SELECT 1 FROM t0 WHERE NOT (NOT (c1 AND c0) AND c1)", 0),
    ] {
        assert_eq!(rows(&io, &conn, sql).len(), expected, "{sql}");
    }
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT a0.c0, a1.c0 FROM t0 AS a0 LEFT JOIN t0 AS a1 ON (NOT (NULL NOT LIKE 'a%'))"
        ),
        ["0|NULL"]
    );
}

/// Do: char() of integers past U+10FFFF and past 32 bits, of NULL and of text. Expect: SQLite's
/// characters: each argument read whole as an integer, and anything negative or past U+10FFFF is
/// U+FFFD (finding diff-eec070d6e2186c8f of seed 20261024: 9007199254740993 was cut to its low 32
/// bits, 1, before its range was checked).
#[test]
fn char_reads_its_integers_whole() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT hex(char(9007199254740993, NULL, 'a')), hex(char(-1, 1114112, 65, 2.9, '66')), hex(char(4294967361)), hex(char(1114111))"),
        ["'EFBFBD0000'|'EFBFBDEFBFBD410242'|'EFBFBD'|'F48FBFBF'"]
    );
    // A surrogate is U+FFFD here; SQLite writes its ill-formed bytes ED A0 80, and TEXT is
    // well-formed UTF-8 (docs/design/semantics.md, decision 1).
    assert_eq!(
        rows(&io, &conn, "SELECT hex(char(55296, 57343))"),
        ["'EFBFBDEFBFBD'"]
    );
}

/// Do: log(B, X) of an X that is text not wholly a number, a BLOB or NULL. Expect: SQLite's answers:
/// with a base, X is read as sqlite3_value_double reads it whatever its type (SQLite's logFunc checks
/// the base's numeric type twice and never X's), so '1e30.0' is 1e30 and x'3237' is 27; alone, X
/// must be a number (finding diff-fe7880981df3bca1 of seed 20261024).
#[test]
fn log_with_a_base_reads_its_operand_as_a_number() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT log(3, '1e30.0'), log(3, x'3237'), log(3, 'abc'), log(3, NULL), log(2, '8abc'), log('3', 9), log(' 9', 81), log('3x', 9)"),
        ["62.877098228681533|3.0|NULL|NULL|3.0|2.0|2.0|NULL"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT log('8abc'), log(100), log(2, -8), log(x'38')"
        ),
        ["NULL|2.0|NULL|NULL"]
    );
}

/// Do: a RIGHT or FULL JOIN after an inner join whose ON holds an EXISTS subquery, a WHERE term on
/// the right table, and ANALYZE's statistics. Expect: SQLite's row, the right table's unmatched one.
/// The EXISTS, a filter of the inner join's tables, was moved to the right table's loop, where fewer
/// rows were estimated; that loop runs again for the unmatched rows with the tables before it NULL,
/// so the filter rejected them (finding dqp-f2037376ef2b5b64 of the second campaign).
#[test]
fn a_filter_is_never_moved_into_a_right_joins_unmatched_pass() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 DOUBLE, c1 TEXT COLLATE NOCASE UNIQUE, c2 REAL, c3)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES ('1e3', NULL, 0, '')")
        .unwrap();
    conn.execute("ANALYZE").unwrap();
    for sql in [
        "SELECT a2.c3 FROM t0 AS a0 JOIN t0 AS a1 ON (EXISTS (SELECT 1 FROM t0 AS s WHERE a1.c3 = ' ')) FULL JOIN t0 AS a2 ON NULL WHERE a2.c3 = ''",
        "SELECT a2.c3 FROM t0 AS a0 JOIN t0 AS a1 ON (EXISTS (SELECT 1 FROM t0 AS s WHERE a1.c3 = ' ')) RIGHT JOIN t0 AS a2 ON NULL WHERE a2.c3 = ''",
        "SELECT a2.c3 FROM t0 AS a0 JOIN t0 AS a1 ON (EXISTS (SELECT 1 FROM t0 AS s WHERE (SELECT sum(s3.c3) FROM t0 AS s3 WHERE (a1.c3 = ' ')))) FULL JOIN t0 AS a2 ON NULL WHERE (a2.c3 GLOB '*') GROUP BY NULL",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["''"], "{sql}");
    }
}

/// Do: JSONB edits that put a shorter value in an old one's place: jsonb_patch of a scalar,
/// jsonb_set and jsonb_replace of a key or an element. Expect: SQLite's bytes. SQLite's jsonBlobEdit
/// writes a value one to eight bytes shorter in the old one's place with its header widened by the
/// difference where JSONB's header sizes allow it (jsonBlobOverwrite), so nothing after it moves and
/// no container's size changes; Amphora wrote the shorter value and resized the containers (finding
/// diff-fd2b5dce5b864501 of the second campaign: the same value in other bytes, and BLOBs compare
/// by their bytes).
#[test]
fn a_jsonb_edit_keeps_a_shorter_values_place_as_sqlite_does() {
    let (io, conn) = open();
    for (sql, expected) in [
        ("SELECT hex(jsonb_patch(-1.0, 0.0))", "'C503302E30'"),
        (
            "SELECT hex(jsonb_set('{\"a\":12345}', '$.a', 1))",
            "'8C1761E30000000131'",
        ),
        (
            "SELECT hex(jsonb_replace('[1,\"abcdef\"]', '$[1]', 2))",
            "'4B13311332'",
        ),
        (
            "SELECT hex(jsonb_set('{\"a\":\"abcdefghi\"}', '$.a', 'x'))",
            "'CC0C1761FA000000000000000178'",
        ),
        (
            "SELECT hex(jsonb_set('{\"a\":\"abcd\"}', '$.a', 'x'))",
            "'4C17611A78'",
        ),
        (
            "SELECT hex(jsonb_set('{\"a\":\"ab\"}', '$.a', json('null')))",
            "'3C176100'",
        ),
        (
            "SELECT hex(jsonb_patch('{\"a\":12345,\"b\":[1]}', '{\"a\":7}'))",
            "'CC0D1761E3000000013717622B1331'",
        ),
        (
            "SELECT json(jsonb_set('{\"a\":\"abcdefghi\"}', '$.a', 'x'))",
            "'{\"a\":\"x\"}'",
        ),
        (
            "SELECT hex(jsonb_set(jsonb_set('{\"a\":\"abcdefghi\"}', '$.a', 'x'), '$.b', 1))",
            "'CC101761FA0000000000000001781A621331'",
        ),
        // A key the target lacks is the patch's label and value as the patch holds them; an
        // object merged into a value that isn't one empties it first.
        (
            "SELECT hex(jsonb_patch('{\"a\":\"abcdefghi\",\"b\":2}', '{\"a\":\"x\",\"c\":3}'))",
            "'CC141761F70000000000000001781762133217631333'",
        ),
        (
            "SELECT hex(jsonb_patch('[1]', '{\"a\":1}'))",
            "'4C17611331'",
        ),
        (
            "SELECT hex(jsonb_patch('{\"a\":1}', '{\"b\":{\"c\":2,\"d\":null}}'))",
            "'BC1761133117624C17631332'",
        ),
        (
            "SELECT hex(jsonb_patch('{\"a\":\"abcdefghi\"}', '{\"a\":{\"b\":1}}'))",
            "'7C17614C17621331'",
        ),
        (
            "SELECT hex(jsonb_patch('{\"a\":\"abcd\"}', '{\"a\":{\"b\":1,\"z\":null}}'))",
            "'7C17614C17621331'",
        ),
        (
            "SELECT json_patch('{\"a\":1}', '{\"b\":{\"c\":2,\"d\":null}}')",
            "'{\"a\":1,\"b\":{\"c\":2}}'",
        ),
    ] {
        assert_eq!(rows(&io, &conn, sql), [expected], "{sql}");
    }
}

/// Do: text holding a NUL after a number (`'5' || char(0)`, `0 || x'00'`) in arithmetic, in math
/// functions, compared with a NUMERIC column and stored in one. Expect: SQLite 3.54's answers: it
/// reads the number from the text before the NUL (sqlite3AtoF on the C string) and takes it for an
/// integer only as the whole text allows: arithmetic makes `'5' || char(0)` 5.0 (sqlite3Atoi64 sees
/// the NUL), affinity makes it 5 (exact below 2^51), CAST AS NUMERIC 5. Amphora took such text for
/// no number at all, or for an integer (findings diff-26a9e52888119082, diff-8b40d84db739acc7 and
/// more of seed 13).
#[test]
fn text_with_a_nul_is_a_number_up_to_the_nul() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT atanh(0 || x'00'), ('5' || char(0)) + 1, typeof(('5' || char(0)) + 1), abs('5' || char(0)), CAST('5' || char(0) AS NUMERIC), typeof(CAST('5' || char(0) AS NUMERIC)), ('1.5' || char(0)) + 0, ('5x' || char(0)) + 0, typeof(('5x' || char(0)) + 0), ('9007199254740993' || char(0)) + 0, (' 5 ' || char(0)) + 0"),
        ["0.0|6.0|'real'|5.0|5|'integer'|1.5|5|'integer'|9007199254740992.0|5.0"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT CAST('5' || char(0) AS INTEGER), CAST('5' || char(0) AS REAL), sqrt('4' || char(0)), char(0) + 1, typeof(char(0) + 1)"),
        ["5|5.0|2.0|1|'integer'"]
    );
    conn.execute("CREATE TABLE t(n NUMERIC, b)").unwrap();
    conn.execute("INSERT INTO t VALUES (0, x'00')").unwrap();
    conn.execute("INSERT INTO t(n) VALUES ('7' || char(0))")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT n = ('0' || b), n < ('1' || b), typeof(n), n FROM t ORDER BY rowid"
        ),
        ["1|1|'integer'|0", "NULL|NULL|'integer'|7"]
    );
}

/// Do: a CASE whose base is compared under one WHEN's affinity and then another's, a constant base
/// in a loop, and a column read by two comparisons. Expect: SQLite's answers: a comparison applies
/// affinity to its operands for itself only (OP_Eq undoes it: "Undo any changes made by
/// applyAffinity() to the input registers"); Amphora kept the converted value in the register, so
/// `'1.5'` turned 1.5 by `WHEN c2` (NUMERIC) no longer matched `WHEN c3`, and a constant `'1'`
/// turned 1 by one row matched `WHEN 1` in the next (findings diff-2edb8c43e1d7b91f,
/// diff-aeb8f8fe124448e9, tlp-ff624de831af03cc and more of seed 13).
#[test]
fn a_comparison_leaves_its_operands_as_they_were() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0, c1 INTEGER, c2 NUMERIC, c3 TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES ('1', -2.5, 0.5, '1.5')")
        .unwrap();
    conn.execute("CREATE TABLE t1(c2 NUMERIC)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (2), ('1.5')").unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT CASE c3 WHEN c2 THEN 'a' WHEN c3 THEN 'b' END FROM t0"
        ),
        ["'b'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT count(*) FROM t0 AS a0 JOIN t0 AS a1 ON (CASE a0.c0 WHEN a1.c1 THEN -1 WHEN 1 THEN 1 ELSE 'x' END)"),
        ["0"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT DISTINCT (CASE '1' WHEN 1 THEN 0 WHEN c2 THEN 'true' END) FROM t1"
        ),
        ["NULL"]
    );
}

/// Do: trim, ltrim and rtrim with a character set holding a NUL. Expect: SQLite's answers: the set
/// ends at its first NUL (trimFunc reads it as a C string), the text trimmed keeps its NULs
/// (findings diff-6af6e289de59c2e8, diff-374f0331b4ad3353, diff-a0b957cc387446a1 and more of seed
/// 13).
#[test]
fn trim_reads_its_characters_to_the_first_nul() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT trim('a0', 'b' || char(0) || '0'), hex(trim(x'00', x'00')), ltrim('00x', x'3000'), rtrim('x00', x'3000'), hex(rtrim(x'00', x'00'))"),
        ["'a0'|'00'|'x'|'x'|'00'"]
    );
}

/// Do: round with a precision that is text, a BLOB or a REAL's text. Expect: SQLite's answers: the
/// precision is read as sqlite3_value_int64 reads it, the integer the text begins with
/// (`CAST(1e308 AS BLOB)` is '1.0e+308', so 1), where Amphora read the number in it (finding
/// diff-f1b80ebd741926b1 of seed 13).
#[test]
fn round_reads_its_precision_as_an_integer() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT round(5.298292365610485, CAST(1e308 AS BLOB)), round(2.567, '2x'), round(2.567, x'31'), round(1.25, '1.9'), round(2.5, ' 1')"),
        ["5.3|2.57|2.6|1.3|2.5"]
    );
}

/// Do: json_error_position of text that is one whole JSON value and then more, and of numbers whose
/// exponent has no digits. Expect: SQLite's positions: input after a whole top-level value is
/// position 1 (jsonConvertTextToBlob fails it without an error offset), and an exponent without
/// digits is reported at its `e` or its sign (finding diff-2d016618aab4d19a of seed 13).
#[test]
fn json_error_position_counts_as_sqlite_does() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT json_error_position('1x'), json_error_position('12x'), json_error_position('1 x'), json_error_position('[1,2x]'), json_error_position('{\"a\":1}x'), json_error_position('x'), json_error_position(' 1'), json_error_position('1' || char(0) || 'x'), json_error_position('[1] ]')"),
        ["1|1|1|5|1|1|0|0|1"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT json_error_position('1.5e'), json_error_position('1e'), json_error_position('1.5ex'), json_error_position('1e+'), json_error_position('1e5x'), json_error_position('truex')"),
        ["4|2|4|3|1|1"]
    );
}

/// Do: `x IN (SELECT realcol)` with integers that round to the column's REAL, and the same as `=`.
/// Expect: SQLite's answers: the IN compares by the column's own affinity, REAL, which makes the
/// integer a REAL (OP_Affinity), so 9223372036854775806 finds 2^63; `=` compares the integer
/// exactly. Amphora collapsed the IN's affinity to NUMERIC (findings diff-cb87365674757e9b,
/// pqs-5969aa20a7e66f03, pqs-b585f7f8224591b6 of seed 13).
#[test]
fn in_a_real_column_finds_an_integer_as_a_real() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE r(c0 REAL, c3 REAL)").unwrap();
    conn.execute("INSERT INTO r VALUES (9223372036854775807, 9007199254740993)")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT 9223372036854775807 IN (SELECT c0 FROM r), 9223372036854775806 IN (SELECT c0 FROM r), 9007199254740993 IN (SELECT c3 FROM r), 9007199254740993 = (SELECT c3 FROM r), 9007199254740993 NOT IN (SELECT c3 FROM r), 3 IN (SELECT c0 FROM r)"),
        ["1|1|1|0|0|0"]
    );
}

/// Do: ANDs and ORs an operand settles once its children are simplified, ANDs and ORs with a
/// subquery on one side, and comparisons with a subquery on one side and NULL on the other, each
/// where reading the other side would raise an error. Expect: SQLite 3.54's answers, no error:
/// its sqlite3ExprSimplifiedAndOr drops what a settled AND or OR never needs (`x OR (1 OR y)` is
/// 1), exprEvalRhsFirst computes the side without a subquery first and skips the other when that
/// settles it, and exprComputeOperands never computes a comparison's subquery side beside a NULL
/// (findings error-c1d490f4d6fdfe5a, error-2ca8ffc19f3a6362, error-3dae39fe7eb153bb and more of
/// seed 13).
#[test]
fn a_settled_and_or_and_a_null_beside_a_subquery_read_no_further() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (1)").unwrap();
    for sql in [
        "SELECT count(*) FROM t0 WHERE json_object(NULL, NULL) OR (1 OR 0.5)",
        "SELECT count(*) FROM t0 WHERE NOT (jsonb_replace(100, 7, NULL) AND (NULL AND 0))",
        "SELECT count(*) FROM t0 WHERE (json_pretty('x', 10) IN (SELECT 1)) OR (NULL ISNULL)",
        "SELECT count(*) FROM t0 WHERE (NULL == (CASE WHEN (1 IN (SELECT 1)) THEN json('x') END)) ISNULL",
        "SELECT count(*) FROM t0 WHERE ((CASE WHEN (1 IN (SELECT 1)) THEN json('x') END) <= NULL) ISNULL",
        "SELECT json('x') OR (1 OR 0)",
        "SELECT (json('x') IN (SELECT 1)) OR 1 FROM t0",
        "SELECT (json('x') IN (SELECT 1)) OR c0 FROM t0",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["1"], "{sql}");
    }
    for sql in [
        "SELECT NULL = (CASE WHEN (1 IN (SELECT 1)) THEN json('x') END) FROM t0",
        "SELECT (CASE WHEN (1 IN (SELECT 1)) THEN json('x') END) < NULL FROM t0",
        "SELECT (CASE WHEN (1 IN (SELECT 1)) THEN json('x') END) + NULL FROM t0",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["NULL"], "{sql}");
    }
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT 5 AND (1 OR 0), 0.5 OR (0 AND x'00'), (2 AND 1) + 0"
        ),
        ["1|1|1"]
    );
}

/// Do: a FULL JOIN, then a table an index seeks by a term holding a NOT IN subquery correlated with
/// the FULL JOIN's right table. Expect: SQLite's rows and no failure: the subquery had been moved
/// into the sought table's loop (to run where its term does, for the FULL JOIN's unmatched rows),
/// after the seek whose key reads it, so the key's search met a list never opened ("cursor id 0 is
/// None", a panic; finding eet-65681da1e85d73a2 of seed 13).
#[test]
fn a_seeks_subquery_runs_before_the_seek() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 INT, c1 TEXT)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1, 100)").unwrap();
    conn.execute("CREATE TABLE t2(c0 TEXT, c2 INT)").unwrap();
    conn.execute("CREATE TABLE t3(c0 TEXT, c2 INT)").unwrap();
    conn.execute("INSERT INTO t3 VALUES ('x', 1), ('y', 0), ('z', 1)")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT 2 FROM t1 AS a0 FULL JOIN t2 AS a1 ON 0, t2 AS a2 WHERE ((10 NOT IN (SELECT s.c0 FROM t2 AS s WHERE a1.c2)) = a2.c2)"),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(&io, &conn, "SELECT a0.c0, a2.c0 FROM t1 AS a0 FULL JOIN t2 AS a1 ON 0, t3 AS a2 WHERE ((10 NOT IN (SELECT s.c0 FROM t3 AS s WHERE a1.c2)) = a2.c2) ORDER BY 2"),
        ["1|'x'", "1|'z'"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT a0.c0, a2.c0 FROM t1 AS a0 FULL JOIN t3 AS a1 ON 0, t3 AS a2 WHERE ((10 NOT IN (SELECT s.c0 FROM t3 AS s WHERE a1.c2)) = a2.c2) ORDER BY 1, 2"),
        ["NULL|'x'", "NULL|'x'", "NULL|'x'", "NULL|'z'", "NULL|'z'", "NULL|'z'", "1|'x'", "1|'z'"]
    );
    // An EXISTS moved to a later loop where fewer rows were estimated, which was the loop an
    // index seeks by its term (finding dqp-d8ffafd446315714 of seed 13).
    conn.execute("CREATE TABLE t4(c0 NUMERIC NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE t5(c0 INTEGER)").unwrap();
    conn.execute("INSERT INTO t5 VALUES ('0')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT ('1e3' COLLATE BINARY) FROM t4 AS a0 RIGHT JOIN t5 AS a1 ON (NOT ((a0.c0 ISNULL) OR a0.c0)), t5 AS a2 WHERE (a2.c0 == (EXISTS (SELECT 1 FROM t5 AS s306 WHERE (a0.c0 IS NOT DISTINCT FROM a1.c0))))"),
        ["'1e3'"]
    );
}

/// Do: compare with, and deduplicate by, a CASE whose branches read a NOCASE column, with and
/// without an explicit COLLATE in it. Expect: SQLite's answers: a CASE takes the first explicit
/// COLLATE in it, its base's before its branches', and never a column's collation (it is no column,
/// sqlite3ExprCollSeq); Amphora compared by the column's NOCASE, so a FULL JOIN's ON matched where
/// it does not (finding diff-eeaf7d8d7fe8d558 of seed 13, campaign 2's AC through a CASE).
#[test]
fn a_case_carries_no_columns_collation() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 NUMERIC COLLATE NOCASE)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES ('A')").unwrap();
    conn.execute("CREATE TABLE t2(c0 TEXT COLLATE NOCASE)")
        .unwrap();
    conn.execute("INSERT INTO t2 VALUES ('a'), ('A')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT 'a' <= (CASE WHEN 1 THEN c0 END), 'a' = (CASE c0 WHEN 'x' THEN 1 ELSE c0 END), 'a' = (CASE WHEN 1 THEN c0 COLLATE NOCASE END), (CASE c0 COLLATE NOCASE WHEN 'a' THEN 'm' END), 'x' = (CASE 'X' COLLATE NOCASE WHEN 'q' THEN 1 ELSE 'X' END) FROM t1"),
        ["0|0|1|'m'|1"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT count(*) FROM (SELECT DISTINCT CASE WHEN 1 THEN c0 END FROM t2)"
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT count(*) FROM (SELECT DISTINCT CASE WHEN 1 THEN c0 COLLATE NOCASE END FROM t2)"
        ),
        ["1"]
    );
}

/// Do: `'' IN (SELECT c0 FROM t2)` and its NOT IN with t2 holding ' ', in a query whose WHERE is a
/// CASE ending on a comparison under COLLATE RTRIM; `('x' COLLATE RTRIM, '') IN (SELECT 'x', c0
/// FROM t2)`; `('', 1) IN (SELECT r, NULL FROM t3)` with r an RTRIM column holding ' ', under a
/// WHERE ending on COLLATE NOCASE; and controls with the collation explicit. Expect: SQLite's
/// answers: a miss is told from a NULL in the list by each column compared as `x = y` compares it
/// (`sqlite3ExprCodeIN`, step 6: sqlite3BinaryCompareCollSeq of each left field and the
/// subquery's column), the collation the list's index has: BINARY where neither side has one, so
/// '' is not ' ' and IN is 0, NOT IN 1; RTRIM for r, so ('', 1) matches (' ', NULL) but for the
/// NULL and IN is NULL. Amphora compared every column by the collation the builder held last, the
/// WHERE's or the first column's: NULL where SQLite answers 0 or 1, and 0 and 1 where it answers
/// NULL (finding eet-5d073f0af8f98282 of seed 20261030).
#[test]
fn an_in_lists_null_check_compares_each_column_as_equality_does() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 NUMERIC)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (9007199254740993)")
        .unwrap();
    conn.execute("CREATE TABLE t2(c0 VARCHAR(10))").unwrap();
    conn.execute("INSERT INTO t2 VALUES (' ')").unwrap();
    conn.execute("CREATE TABLE t3(r TEXT COLLATE RTRIM)")
        .unwrap();
    conn.execute("INSERT INTO t3 VALUES (' ')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT '' IN (SELECT c0 FROM t2), '' NOT IN (SELECT c0 FROM t2) FROM t1 WHERE CASE WHEN 0 THEN 1 ELSE c0 != ('a ' COLLATE RTRIM) END"),
        ["0|1"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT ('x' COLLATE RTRIM, '') IN (SELECT 'x', c0 FROM t2), ('' COLLATE RTRIM) IN (SELECT c0 FROM t2), ('x', '' COLLATE RTRIM) IN (SELECT 'x', c0 FROM t2) FROM t1"),
        ["0|1|1"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT ('', 1) IN (SELECT r, NULL FROM t3), ('', 1) NOT IN (SELECT r, NULL FROM t3), ('x', 1) IN (SELECT r, NULL FROM t3) FROM t1 WHERE CASE WHEN 0 THEN 1 ELSE c0 != ('a ' COLLATE NOCASE) END"),
        ["NULL|NULL|0"]
    );
}

/// Do: count(DISTINCT x) and group_concat(DISTINCT x) of NOCASE and RTRIM columns, of text holding
/// a NUL, and of expressions over them. Expect: SQLite's answers: the set is keyed by the
/// argument's collation (KeyInfo over the aggregate's arguments), so 'a' and 'A' count once under
/// NOCASE, as do two texts beginning with a NUL of one length (sqlite3StrNICmp stops at the NUL);
/// an expression over the column carries no collation. Amphora keyed it by the whole aggregate's,
/// which is none (found beside finding tlp-dabb1a3f92b77bab of seed 13).
#[test]
fn a_distinct_aggregate_dedupes_by_its_arguments_collation() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(c TEXT COLLATE NOCASE, r TEXT COLLATE RTRIM, b TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('a', 'x', 'p'), ('A', 'x ', 'P'), (char(0) || 'a', 'y', 'p'), (char(0) || 'b', 'y  ', 'q')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT count(DISTINCT c), count(DISTINCT r), hex(group_concat(DISTINCT c)), count(DISTINCT b), count(DISTINCT b COLLATE NOCASE), count(DISTINCT c COLLATE BINARY), count(DISTINCT c || ''), count(DISTINCT lower(c)) FROM t"),
        ["2|2|'612C0061'|3|2|4|4|3"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT b, count(DISTINCT c) FROM t GROUP BY b ORDER BY b"
        ),
        ["'P'|1", "'p'|2", "'q'|1"]
    );
}

/// Do: a LEFT JOIN the planner answers with a hash join, its WHERE term an IN subquery correlated
/// with an earlier table, where the probe side is empty so only the unmatched build rows reach the
/// term. Expect: SQLite's rows and no failure: the subquery was run where its term is first
/// checked, in the probe's path, and the unmatched rows' own path searched a list never opened
/// ("cursor id 0 is None", a panic; finding eet-e419ed8b8cc2a867 of seed 14).
#[test]
fn a_hash_joins_unmatched_rows_see_their_subqueries() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 VARCHAR(10) NOT NULL, c1 NUMERIC, c2 INT, c3 VARCHAR(10) COLLATE BINARY PRIMARY KEY)").unwrap();
    conn.execute("CREATE TABLE t1(c0 BOOLEAN, c1 TEXT, c2 BOOLEAN)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (-2.5, 3, '_')")
        .unwrap();
    conn.execute("CREATE TABLE t2(c0 INTEGER UNIQUE, c1 VARCHAR(10) COLLATE RTRIM)")
        .unwrap();
    let query = "SELECT a0.c1, a2.c1 FROM t1 AS a0 LEFT JOIN t2 AS a1 ON ((+ a1.c1) = (a0.c0 | 1e308)) JOIN t1 AS a2 ON (CAST(3.0 AS INTEGER) IS NOT a2.c2) WHERE (((CASE WHEN ((1 OR (a1.c1 NOTNULL)) NOTNULL) THEN '10' ELSE 7 END) - a2.c1) IN (SELECT s.c2 FROM t0 AS s WHERE (NOT (9223372036854775807 = a2.c0)))) ORDER BY 1, 2";
    assert_eq!(rows(&io, &conn, query), Vec::<String>::new());
    conn.execute("INSERT INTO t0 VALUES ('x', NULL, 7, 'k')")
        .unwrap();
    assert_eq!(rows(&io, &conn, query), ["'3'|'3'"]);
    conn.execute("INSERT INTO t1 VALUES (1, 5, 'z')").unwrap();
    assert_eq!(rows(&io, &conn, query), ["'3'|'3'", "'5'|'3'"]);
}

/// Do: nullif, and min and max of two or more, over NOCASE and RTRIM columns and an explicit
/// COLLATE. Expect: SQLite's answers: they compare by the first argument's collation that has one
/// (SQLITE_FUNC_NEEDCOLL; a plain column gives BINARY), without affinity; of equal values min takes
/// the later and max keeps the earlier. Amphora compared by BINARY (finding diff-83a685b59204ffff
/// of seed 14).
#[test]
fn nullif_min_and_max_compare_by_their_arguments_collation() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(r TEXT COLLATE RTRIM, n TEXT COLLATE NOCASE, b TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('', 'a', 'A')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT nullif(r, ' '), nullif(' ', r), nullif(n, 'A'), nullif(b, n), nullif(b COLLATE NOCASE, 'a'), max(n, 'B'), min(b, n), min('a', 'A' COLLATE NOCASE), max('a', 'A' COLLATE NOCASE), max(n, 'A'), min(n, 'A'), nullif(1, '1'), max(1, '0'), min(x'00', 'z', 3) FROM t"),
        ["NULL|NULL|NULL|'A'|NULL|'B'|'A'|'A'|'a'|'a'|'A'|1|'0'|3"]
    );
}

/// Do: ANDs and ORs settled by a NULL test of a parenthesized constant or of another such test,
/// each beside a call that would raise an error. Expect: SQLite's answers, no error: its parser
/// folds `(- 1.5) NOTNULL` and `(2.0 NOTNULL) NOTNULL` to 1 (sqlite3PExprIsNull, the inner test
/// already the integer it makes), so the settled AND or OR never reads the call; Amphora's fold did
/// not look inside the parentheses (findings error-dfd3551986fdeacf, error-dcaca8f9a487ab22,
/// error-493c667725cb13ff of seed 14).
#[test]
fn a_parenthesized_null_test_folds_like_a_literal() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (1)").unwrap();
    for sql in [
        "SELECT count(*) FROM t0 WHERE (jsonb_object(NULL, 0.0, NULL) OR ((- 1.5) NOTNULL))",
        "SELECT count(*) FROM t0 WHERE (NOT (jsonb_array_insert(NULL, NULL) AND ((+ x'') ISNULL)))",
        "SELECT count(*) FROM t0 WHERE (json_patch('x', 0) OR ((2.0 NOTNULL) NOTNULL))",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["1"], "{sql}");
    }
    assert_eq!(
        rows(&io, &conn, "SELECT ((2.0 NOTNULL) NOTNULL), ((NULL ISNULL) NOTNULL), (('a' ISNULL) ISNULL), ((c0 NOTNULL) NOTNULL) FROM t0"),
        ["1|1|0|1"]
    );
}

/// Do: compare with a function whose arguments hold an explicit COLLATE, the COLLATE in any of
/// them. Expect: SQLite's answers: the function's value takes the first explicit COLLATE among its
/// arguments (sqlite3ExprCollSeq), wherever it stands; Amphora kept what its last argument left
/// (finding diff-a67a6d3509fcca74 of seed 14).
#[test]
fn a_functions_value_takes_its_arguments_first_collate() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 DOUBLE, c1 COLLATE RTRIM PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES (' ', NULL)").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT char('1e3', (9007199254740993 COLLATE RTRIM), (NOT a0.c1)) < a1.c0 FROM t0 AS a0, t0 AS a1"),
        ["0"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT char(65, 66 COLLATE NOCASE, 67) = 'abc', printf('%s%s', 'A' COLLATE NOCASE, 'B') = 'ab', coalesce(NULL, 'A' COLLATE NOCASE) = 'a', replace('ABC', 'B' COLLATE NOCASE, 'X') = 'axc', substr('ABC' COLLATE RTRIM, 1) = 'ABC  '"),
        ["1|1|1|1|1"]
    );
}

/// Do: settle an AND or OR with a literal (`1 OR`, `0 AND`, on either side) beside a subquery that
/// raises an error (sum's integer overflow): in WHERE, in the select list, under EXISTS and IN,
/// beside an aggregate, shared by GROUP BY and the select list, in INSERT VALUES, UPDATE SET, its
/// WHERE and RETURNING, and DELETE's WHERE; and with a WHERE term a literal 0 settles beside it.
/// Expect: SQLite's answers. Its parser makes an AND with a literal 0 and no function on either side
/// the 0 (sqlite3ExprAnd), so names there are never resolved and an ORDER BY term so made is a
/// column number; its code leaves out what sqlite3ExprSimplifiedAndOr drops, so a subquery there
/// never runs, though an aggregate beside it still makes one row and a name it misses is still an
/// error; and a WHERE term that is a literal 0 jumps past the loops before any other term runs, but
/// not one from an ON clause. Amphora ran every uncorrelated subquery before the loops, read or
/// not, and resolved every name (finding error-99fb1658e34f78cb of seed 14).
#[test]
fn a_subquery_a_settled_branch_drops_never_runs() {
    let (io, conn) = open();
    let error = |sql: &str| match conn.prepare(sql).and_then(|mut stmt| loop {
        match stmt.step()? {
            StepResult::IO => io.step()?,
            StepResult::Done => break Ok(()),
            _ => {}
        }
    }) {
        Ok(()) => "no error".to_owned(),
        Err(e) => e.to_string(),
    };
    conn.execute("CREATE TABLE t0(c0 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (10), (9223372036854775807), (7)")
        .unwrap();
    let overflow = "(SELECT sum(c0) FROM t0 WHERE c0)";
    let exists = "EXISTS (SELECT sum(c0) FROM t0 WHERE c0 HAVING sum(c0))";
    assert!(error(&format!("SELECT 1 AND {overflow}")).contains("integer overflow"));
    for (sql, want) in [
        (format!("SELECT a0.rowid FROM t0 AS a0 WHERE (1 OR {overflow})"), vec!["1", "2", "3"]),
        (format!("SELECT (1 OR {overflow}) IS TRUE, (1 OR {overflow}) IS FALSE FROM t0 WHERE rowid = 3"), vec!["1|0"]),
        (format!("SELECT 0 AND {overflow}, {overflow} AND 0, {overflow} OR 1, NOT (0 AND {overflow})"), vec!["0|0|1|1"]),
        (format!("SELECT count(*) FROM t0 WHERE 0 AND {exists}"), vec!["0"]),
        (format!("SELECT 1 OR c0 IN {overflow} FROM t0"), vec!["1", "1", "1"]),
        (format!("SELECT 1 OR max(c0) IN {overflow} FROM t0"), vec!["1"]),
        ("SELECT 1 OR (SELECT count(*) FROM t0), (SELECT count(*) FROM t0) FROM t0 GROUP BY (SELECT count(*) FROM t0)".to_owned(), vec!["1|3"]),
        (format!("SELECT c0 FROM t0 WHERE rowid = 1 AND (0 AND {overflow}) IS NOT NULL"), vec!["10"]),
        ("SELECT 0 AND nosuch, (0) AND nosuch, (5 ISNULL) AND nosuch, 0 AND (SELECT nosuch FROM t0), 0 AND nosuch AND abs(1), (0 AND nosuch) OR 1".to_owned(), vec!["0|0|0|0|0|1"]),
        ("SELECT count(*) FROM t0 WHERE nosuch AND 0".to_owned(), vec!["0"]),
        ("SELECT count(*) FROM t0 JOIN t0 AS b ON 0 AND nosuch".to_owned(), vec!["0"]),
        ("SELECT count(*) FROM t0 GROUP BY c0 HAVING 0 AND nosuch".to_owned(), vec![]),
        (format!("SELECT count(*) FROM t0 WHERE 0 AND abs(c0) AND {exists}"), vec!["0"]),
        (format!("SELECT count(*) FROM t0 WHERE abs(c0) AND {exists} AND 0"), vec!["0"]),
        (format!("SELECT count(*) FROM t0 WHERE {exists} AND 0 AND random()"), vec!["0"]),
        (format!("SELECT c0 FROM t0 WHERE {exists} AND (0 AND abs(c0))"), vec![]),
    ] {
        assert_eq!(rows(&io, &conn, &sql), want, "{sql}");
    }
    for (sql, want) in [
        (
            "SELECT 1 OR (SELECT nosuch FROM t0)".to_owned(),
            "no such column",
        ),
        ("SELECT FALSE AND nosuch".to_owned(), "no such column"),
        ("SELECT 0.0 AND nosuch".to_owned(), "no such column"),
        ("SELECT -0 AND nosuch".to_owned(), "no such column"),
        (
            "SELECT 0 AND (nosuch LIKE 'a')".to_owned(),
            "no such column",
        ),
        ("SELECT 0 AND abs(nosuch)".to_owned(), "no such column"),
        (
            "SELECT c0 FROM t0 ORDER BY 0 AND nosuch".to_owned(),
            "ORDER BY term out of range",
        ),
        (
            format!("SELECT count(*) FROM t0 LEFT JOIN t0 AS b ON 0 WHERE {exists}"),
            "integer overflow",
        ),
    ] {
        assert!(error(&sql).contains(want), "{sql}: {}", error(&sql));
    }
    conn.execute("CREATE TABLE t1(a INT)").unwrap();
    conn.execute(format!(
        "INSERT INTO t1 VALUES (0 AND {overflow}), (1 OR {overflow})"
    ))
    .unwrap();
    assert_eq!(rows(&io, &conn, "SELECT a FROM t1"), ["0", "1"]);
    assert_eq!(
        rows(&io, &conn, &format!("UPDATE t1 SET a = a + (0 AND {overflow}) WHERE (1 OR {overflow}) RETURNING a, 1 OR {overflow}")),
        ["0|1", "1|1"]
    );
    conn.execute(format!("DELETE FROM t1 WHERE 0 AND {overflow}"))
        .unwrap();
    assert_eq!(rows(&io, &conn, "SELECT count(*) FROM t1"), ["2"]);
}

/// Do: read an uncorrelated subquery that raises an error (sum's integer overflow) only where no row
/// reaches it: over an empty table, past a WHERE term no row passes, in a CASE branch no row takes,
/// beside `WHERE 0`, as an IN filter past a failing term, and compared in the WHERE of an empty
/// table. Expect: SQLite's rows. SQLite codes a
/// subquery where it is read (sqlite3CodeSubselect, behind OP_Once when uncorrelated) and keeps a
/// WHERE term holding one out of the terms it tests before its loops, so a subquery no row reaches
/// never runs; one read where a loop is set up (an IN that drives a seek, a seek key, LIMIT) or
/// in an aggregate's output still runs and still raises its error there. Amphora ran each
/// uncorrelated subquery before the loops, read or not.
#[test]
fn an_unread_subquery_never_runs() {
    let (io, conn) = open();
    let error = |sql: &str| match conn.prepare(sql).and_then(|mut stmt| loop {
        match stmt.step()? {
            StepResult::IO => io.step()?,
            StepResult::Done => break Ok(()),
            _ => {}
        }
    }) {
        Ok(()) => "no error".to_owned(),
        Err(e) => e.to_string(),
    };
    conn.execute("CREATE TABLE t0(c0 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (10), (9223372036854775807), (7)")
        .unwrap();
    conn.execute("CREATE TABLE e(x INTEGER PRIMARY KEY, y INT)")
        .unwrap();
    conn.execute("CREATE INDEX e_y ON e(y)").unwrap();
    let s = "(SELECT sum(c0) FROM t0 WHERE c0)";
    for (sql, want) in [
        (format!("SELECT {s} FROM e"), vec![]),
        (format!("SELECT {s} FROM t0 WHERE c0 = 99"), vec![]),
        ("SELECT c0 FROM t0 WHERE c0 = 99 AND EXISTS (SELECT sum(c0) FROM t0 WHERE c0 HAVING sum(c0))".to_owned(), vec![]),
        (format!("SELECT CASE WHEN c0 = 99 THEN {s} END FROM t0"), vec!["NULL", "NULL", "NULL"]),
        (format!("SELECT {s} FROM t0 WHERE 0"), vec![]),
        (format!("SELECT c0 FROM t0 WHERE c0 = 99 AND c0 IN {s}"), vec![]),
        (format!("SELECT x FROM e WHERE y > 0 AND {s} > 0"), vec![]),
    ] {
        assert_eq!(error(&sql), "no error", "{sql}");
        assert_eq!(rows(&io, &conn, &sql), want, "{sql}");
    }
    for sql in [
        format!("SELECT {s}"),
        format!("SELECT 1 AND {s}"),
        format!("SELECT count(*), {s} FROM t0 WHERE 0"),
        format!("SELECT x FROM e WHERE y IN {s}"),
        format!("SELECT x FROM e WHERE x IN {s}"),
        format!("SELECT x FROM e WHERE y = {s}"),
        format!("SELECT x FROM e LIMIT {s}"),
    ] {
        assert!(
            error(&sql).contains("integer overflow"),
            "{sql}: {}",
            error(&sql)
        );
    }
}

/// Do: read a correlated subquery that raises an error (sum's integer overflow) only in a CASE
/// branch no row takes. Expect: SQLite's three NULL rows: it codes the subquery inside the branch,
/// so no row runs it. Amphora ran a correlated subquery for every row of its loop, read or not.
#[test]
fn an_unread_correlated_subquery_never_runs() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (10), (9223372036854775807), (7)")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT CASE WHEN a.c0 = 99 THEN (SELECT sum(b.c0) FROM t0 AS b WHERE b.c0 >= a.c0 - a.c0) END FROM t0 AS a"),
        ["NULL", "NULL", "NULL"]
    );
}

/// Do: read one uncorrelated subquery in two places: in GROUP BY and the select list (planned once
/// for both), and as the row value an UPDATE sets two columns from, a column at a time. Expect:
/// SQLite's rows, EXPLAIN QUERY PLAN listing the subquery once and EXPLAIN coding its body once
/// (one OpenRead of the table only it reads): each read calls that one body, which runs once.
#[test]
fn a_subquery_read_in_two_places_is_coded_once() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 INT, c1 INT)").unwrap();
    conn.execute("CREATE TABLE t1(c0 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (1, 2), (3, 4)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (5), (6)").unwrap();
    let root = rows(
        &io,
        &conn,
        "SELECT rootpage FROM sqlite_schema WHERE name = 't1'",
    );
    for (sql, want) in [
        (
            "SELECT (SELECT max(c0) FROM t1), count(*) FROM t0 GROUP BY (SELECT max(c0) FROM t1)",
            ["6|2"].as_slice(),
        ),
        (
            "UPDATE t0 SET (c0, c1) = (SELECT max(c0), min(c0) FROM t1) RETURNING c0, c1",
            ["6|5", "6|5"].as_slice(),
        ),
    ] {
        let listed = rows(&io, &conn, &format!("EXPLAIN QUERY PLAN {sql}"));
        assert_eq!(
            listed
                .iter()
                .filter(|step| step.contains("SUBQUERY"))
                .count(),
            1,
            "{sql}: {listed:?}"
        );
        let coded = rows(&io, &conn, &format!("EXPLAIN {sql}"));
        let opens = coded
            .iter()
            .filter(|insn| {
                let fields: Vec<&str> = insn.split('|').collect();
                fields[1] == "'OpenRead'" && fields[3] == root[0]
            })
            .count();
        assert_eq!(opens, 1, "{sql}: {coded:#?}");
        assert_eq!(rows(&io, &conn, sql), want, "{sql}");
    }
}

/// Do: fire a trigger three times whose body reads uncorrelated subqueries: in an INSERT's values
/// (one reading NEW) and in an UPDATE's SET. Expect: SQLite's rows: each firing runs each subquery
/// afresh where it reads it, as SQLite's own run of the trigger program does.
#[test]
fn a_triggers_subqueries_run_afresh_at_each_firing() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(a INT)").unwrap();
    conn.execute("CREATE TABLE log(n INT, m INT)").unwrap();
    conn.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES ((SELECT count(*) FROM t), (SELECT max(a) FROM t WHERE a < new.a)); UPDATE log SET m = (SELECT sum(a) FROM t) WHERE n = 1; END").unwrap();
    conn.execute("INSERT INTO t VALUES (10), (20), (30)")
        .unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT n, m FROM log ORDER BY n"),
        ["1|60", "2|10", "3|20"]
    );
}

/// Do: put a correlated subquery that raises an error (sum's integer overflow) in a CASE branch no
/// row takes where Amphora ran it at every row of its loop: an ungrouped aggregate's output, a
/// GROUP BY's select list, a window query's select list, a RIGHT JOIN query's select list and an
/// UPDATE's SET; then let a row take the branch. Expect: SQLite's rows where no row reads it, as
/// SQLite runs a correlated subquery where its expression is coded (an aggregate's from the values
/// kept for its output), and SQLite's error where one does. HAVING, a DELETE's WHERE behind a
/// failing term and a SELECT whose rows go straight out already answered so, and still do.
#[test]
fn a_correlated_subquery_no_row_reads_never_runs() {
    let (io, conn) = open();
    let error = |sql: &str| match conn.prepare(sql).and_then(|mut stmt| loop {
        match stmt.step()? {
            StepResult::IO => io.step()?,
            StepResult::Done => break Ok(()),
            _ => {}
        }
    }) {
        Ok(()) => "no error".to_owned(),
        Err(e) => e.to_string(),
    };
    conn.execute("CREATE TABLE t0(c0 INT, c1 INT)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (10, 0), (9223372036854775807, 0), (7, 0)")
        .unwrap();
    conn.execute("CREATE TABLE t1(x INT)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1)").unwrap();
    let s = "(SELECT sum(b.c0) FROM t0 AS b WHERE b.c0 >= a.c0 - a.c0)";
    let u = "(SELECT sum(b.c0) FROM t0 AS b WHERE b.c0 >= t0.c0 - t0.c0)";
    // Each place: a query no row of which reads the subquery, its answer, and one a row reads it in.
    for (place, unread, want, read) in [
        (
            "an ungrouped aggregate's output",
            format!("SELECT count(*), CASE WHEN max(a.c0) = 99 THEN {s} END FROM t0 AS a"),
            vec!["3|NULL"],
            format!("SELECT count(*), CASE WHEN max(a.c0) > 0 THEN {s} END FROM t0 AS a"),
        ),
        (
            "a GROUP BY's select list",
            format!("SELECT a.c0, CASE WHEN a.c0 = 99 THEN {s} END FROM t0 AS a GROUP BY a.c0 ORDER BY a.c0"),
            vec!["7|NULL", "10|NULL", "9223372036854775807|NULL"],
            format!("SELECT a.c0, CASE WHEN a.c0 = 7 THEN {s} END FROM t0 AS a GROUP BY a.c0 ORDER BY a.c0"),
        ),
        (
            "HAVING",
            format!("SELECT a.c0 FROM t0 AS a GROUP BY a.c0 HAVING a.c0 = 99 AND {s} > 0"),
            vec![],
            format!("SELECT a.c0 FROM t0 AS a GROUP BY a.c0 HAVING a.c0 = 7 AND {s} > 0"),
        ),
        (
            "a window query's select list",
            format!("SELECT CASE WHEN a.c0 = 99 THEN {s} END, row_number() OVER () FROM t0 AS a"),
            vec!["NULL|1", "NULL|2", "NULL|3"],
            format!("SELECT CASE WHEN a.c0 = 7 THEN {s} END, row_number() OVER () FROM t0 AS a"),
        ),
        (
            "a window query a LIMIT cuts short of the row that reads it",
            format!("SELECT CASE WHEN a.c0 = 7 THEN {s} END, row_number() OVER () FROM t0 AS a LIMIT 1"),
            vec!["NULL|1"],
            format!("SELECT CASE WHEN a.c0 = 7 THEN {s} END, row_number() OVER () FROM t0 AS a LIMIT 3"),
        ),
        (
            "a RIGHT JOIN query's select list",
            format!("SELECT CASE WHEN a.c0 = 99 THEN {s} END FROM t1 RIGHT JOIN t0 AS a ON t1.x = a.c0"),
            vec!["NULL", "NULL", "NULL"],
            format!("SELECT CASE WHEN a.c0 = 7 THEN {s} END FROM t1 RIGHT JOIN t0 AS a ON t1.x = a.c0"),
        ),
        (
            "a SELECT whose rows go straight out",
            format!("SELECT CASE WHEN a.c0 = 99 THEN {s} END FROM t0 AS a ORDER BY a.c1"),
            vec!["NULL", "NULL", "NULL"],
            format!("SELECT CASE WHEN a.c0 = 7 THEN {s} END FROM t0 AS a ORDER BY a.c1"),
        ),
        (
            "an UPDATE's SET",
            format!("UPDATE t0 SET c1 = CASE WHEN c0 = 99 THEN {u} END RETURNING c1"),
            vec!["NULL", "NULL", "NULL"],
            format!("UPDATE t0 SET c1 = CASE WHEN c0 = 7 THEN {u} END RETURNING c1"),
        ),
        (
            "a DELETE's WHERE",
            format!("DELETE FROM t0 WHERE c0 = 99 AND {u} > 0 RETURNING c0"),
            vec![],
            format!("DELETE FROM t0 WHERE c0 = 7 AND {u} > 0 RETURNING c0"),
        ),
    ] {
        assert_eq!(error(&unread), "no error", "{place}: {unread}");
        assert_eq!(rows(&io, &conn, &unread), want, "{place}: {unread}");
        assert!(
            error(&read).contains("integer overflow"),
            "{place}: {read}: {}",
            error(&read)
        );
    }
}

/// Do: estimate a FULL JOIN filtered on its right table's column, then the same with a conjunct on
/// its left table's, which makes it an inner join. Expect: no more rows estimated for the inner
/// join (CERT; finding cert-70d3e0baf8df5914 in the second campaign, cause AG: 320,000 rows against
/// the FULL JOIN's 10,000, as the FULL JOIN's estimate came from its access path and left out the
/// rows its inner join counted).
#[test]
fn a_full_join_is_estimated_at_least_its_inner_join() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 REAL PRIMARY KEY NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE t1(c0 VARCHAR(10), c1 TEXT PRIMARY KEY)")
        .unwrap();
    let from = "SELECT NULL FROM t1 AS a0 FULL JOIN t0 AS a1 ON ((a1.c0 > 1000000000000000.0) AND (a0.c1 >= a1.c0)) WHERE ";
    let term = "(a1.c0 IN (NULL, 1.5))";
    let base = estimate(&io, &conn, &format!("{from}{term}"));
    let more = estimate(&io, &conn, &format!("{from}({term} AND a0.c1)"));
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: estimate a join whose WHERE bounds a column by a value of its own table and one of the other
/// table's, then the same with a conjunct that is always true (`1 NOTNULL OR ...`), which moves the
/// join's order. Expect: no more rows estimated (CERT; finding cert-bae9c4dcf94932e8 in the second
/// campaign, cause AG: 6.4e10 rows against 2e9, as an index seek's estimate counted the two bounds
/// as one closed range in one order and two filters counted them apart in the other).
#[test]
fn an_always_true_conjunct_raises_no_estimate() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 TEXT PRIMARY KEY, c1 INTEGER UNIQUE)")
        .unwrap();
    let from = "SELECT ((x'41' = 9007199254740993) AND (a1.c1 NOT BETWEEN 'x' AND a0.c1)) FROM t0 AS a0, t0 AS a1 WHERE ";
    let term = "(a0.c0 BETWEEN (a0.c1 >= a0.c1) AND ('1.5' IS DISTINCT FROM a1.c1))";
    let base = estimate(&io, &conn, &format!("{from}{term}"));
    let more = estimate(
        &io,
        &conn,
        &format!("{from}({term} AND ((1 NOTNULL) OR (a1.c1 BETWEEN -1 AND -2.5)))"),
    );
    assert!(
        no_more(more, base),
        "{more} rows estimated for the more restrictive query, {base} for the query"
    );
}

/// Do: estimate a LEFT JOIN whose WHERE, or a later join's ON, tests a column of its right table
/// that no row of it holds NULL in (a NOT NULL column, an INTEGER PRIMARY KEY) with IS NULL, written
/// each way SQLite reads it (`NULL IS c`, `c IS NOT DISTINCT FROM NULL`), then the same with the
/// join made inner, by INNER JOIN or by a conjunct that rejects the rows it null-extends. Expect: no
/// more rows estimated for the inner join. The term is true only of a row the LEFT JOIN
/// null-extended, so the LEFT JOIN keeps about none and a table no join null-extends keeps none
/// (CERT; findings cert-e21949a38a9b16d7, cert-2732f12b21087328, cert-2c3a9d2455aef93d and
/// cert-d03ea211f1fab06d on step 4's d762dc2, cause AG: 1e10 to 1e17 rows against 0, the term
/// counted on the inner table as IS NULL's tenth of its rows). An inner join's ON term reads its
/// tables before a RIGHT JOIN after it null-extends them, so there too it keeps none
/// (cert-60e65956aed5c593: 1e17 rows against the LEFT JOIN form's 1e12).
#[test]
fn an_is_null_of_a_column_never_null_keeps_no_inner_row() {
    for (tables, base, more) in [
        (
            &["CREATE TABLE t0(c0 BLOB PRIMARY KEY)", "CREATE TABLE t2(c0 BOOLEAN, c1 DOUBLE PRIMARY KEY NOT NULL, c2 TEXT NOT NULL)"][..],
            "SELECT NULL FROM t0 AS a0 LEFT JOIN t2 AS a1 ON 255 WHERE (NULL IS a1.c1)",
            "SELECT NULL FROM t0 AS a0 JOIN t2 AS a1 ON 255 WHERE (NULL IS a1.c1)",
        ),
        (
            &["CREATE TABLE t0(c0 INT, c1 BLOB NOT NULL, c2 DOUBLE COLLATE BINARY PRIMARY KEY NOT NULL)"][..],
            "SELECT x'00' FROM t0 AS a0 LEFT JOIN t0 AS a1 ON '[1,2,3]' WHERE (a1.c2 IS NOT DISTINCT FROM NULL)",
            "SELECT x'00' FROM t0 AS a0 LEFT JOIN t0 AS a1 ON '[1,2,3]' WHERE ((a1.c2 IS NOT DISTINCT FROM NULL) AND a1.c2)",
        ),
        (
            &["CREATE TABLE t0(c0 DOUBLE PRIMARY KEY)", "CREATE TABLE t1(c0 REAL, c1 INTEGER PRIMARY KEY, c2 VARCHAR(10))"][..],
            "SELECT NULL FROM t0 AS a0 LEFT JOIN t1 AS a1 ON 1 JOIN t1 AS a2 ON (NULL IS NOT DISTINCT FROM a1.c1)",
            "SELECT NULL FROM t0 AS a0 JOIN t1 AS a1 ON 1 JOIN t1 AS a2 ON (NULL IS NOT DISTINCT FROM a1.c1)",
        ),
        (
            &["CREATE TABLE t1(c0 TEXT, c1  COLLATE NOCASE PRIMARY KEY NOT NULL, c2  COLLATE BINARY)", "CREATE TABLE t2(c0 BLOB NOT NULL, c1 INT PRIMARY KEY, c2 REAL COLLATE RTRIM, c3 BOOLEAN)"][..],
            "SELECT (a0.c3 <= a0.c3) FROM t2 AS a0 LEFT JOIN t1 AS a1 ON ((a1.c1 NOT GLOB 'abc') OR (a0.c3 AND (100 NOTNULL))), t2 AS a2 WHERE (((NULL IS NOT DISTINCT FROM a1.c1) AND (-9223372036854775808 = a0.c3)) AND (a2.c1 == 1000000000000000.0))",
            "SELECT (a0.c3 <= a0.c3) FROM t2 AS a0 JOIN t1 AS a1 ON ((a1.c1 NOT GLOB 'abc') OR (a0.c3 AND (100 NOTNULL))), t2 AS a2 WHERE (((NULL IS NOT DISTINCT FROM a1.c1) AND (-9223372036854775808 = a0.c3)) AND (a2.c1 == 1000000000000000.0))",
        ),
        (
            &["CREATE TABLE t0(c0 TEXT COLLATE RTRIM NOT NULL, c1 BOOLEAN)", "CREATE TABLE t1(c0 REAL COLLATE RTRIM, c1 BLOB PRIMARY KEY, c2 BLOB COLLATE RTRIM UNIQUE NOT NULL, c3 INT)"][..],
            "SELECT '\"x\"' FROM t1 AS a0 LEFT JOIN t1 AS a1 ON (a1.c2 IS NOT DISTINCT FROM NULL) RIGHT JOIN t0 AS a2 ON 1",
            "SELECT '\"x\"' FROM t1 AS a0 JOIN t1 AS a1 ON (a1.c2 IS NOT DISTINCT FROM NULL) RIGHT JOIN t0 AS a2 ON 1",
        ),
    ] {
        let (io, conn) = open();
        for table in tables {
            conn.execute(table).unwrap();
        }
        let (b, m) = (estimate(&io, &conn, base), estimate(&io, &conn, more));
        assert!(
            no_more(m, b),
            "{m} rows estimated for the more restrictive query, {b} for the query: {more}"
        );
    }
}

/// Do: EXPLAIN QUERY PLAN FORMAT=JSON a query of every top-level shape: a table read, a join,
/// grouped, aggregated, DISTINCT, cut by LIMIT, VALUES, a SELECT of no table, a compound select of
/// each operator (of VALUES and of tables, with a LIMIT of its own), a FROM-clause subquery and a
/// CTE. Expect: `output_rows` for each, the rows the statement returns: VALUES its rows, a SELECT
/// of no table one (none under a WHERE never true or LIMIT 0), an aggregate without GROUP BY one,
/// a literal LIMIT at most its rows, UNION ALL and UNION its arms' sum, INTERSECT the smaller arm,
/// EXCEPT its left arm; so CERT has an estimate for every query it checks (campaign 2 skipped 2,049
/// of its checks as "no estimate", every one a query Amphora refused, not one planned without an
/// estimate; bench/oracles/findings.md, cause AG).
#[test]
fn every_top_level_plan_shape_has_an_estimate() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE u(c, d)").unwrap();
    let table = estimate(&io, &conn, "SELECT * FROM t");
    assert!(table >= 1.0, "a table read: {table}");
    for (sql, want) in [
        ("SELECT count(*) FROM t", Some(1.0)),
        (
            "SELECT count(*) FROM t JOIN u ON t.b = u.c WHERE u.d > 3",
            Some(1.0),
        ),
        ("SELECT * FROM t LIMIT 5", Some(5.0)),
        (
            "SELECT * FROM t JOIN u ON t.b = u.c LIMIT 7 OFFSET 2",
            Some(7.0),
        ),
        ("SELECT * FROM t LIMIT 0", Some(0.0)),
        ("VALUES (1), (2), (3)", Some(3.0)),
        ("SELECT 1", Some(1.0)),
        ("SELECT 1 WHERE 0", Some(0.0)),
        ("SELECT 1 LIMIT 0", Some(0.0)),
        ("SELECT 1 UNION ALL VALUES (2), (3)", Some(3.0)),
        ("VALUES (1), (2), (3) UNION VALUES (4)", Some(4.0)),
        ("VALUES (1), (2), (3) INTERSECT VALUES (1)", Some(1.0)),
        ("VALUES (1), (2), (3) EXCEPT VALUES (1)", Some(3.0)),
        (
            "VALUES (1), (2), (3) EXCEPT VALUES (1) UNION ALL SELECT 4",
            Some(4.0),
        ),
        ("SELECT a FROM t UNION SELECT c FROM u LIMIT 9", Some(9.0)),
        ("SELECT * FROM (SELECT * FROM t LIMIT 4)", Some(4.0)),
        (
            "WITH c AS (SELECT * FROM t LIMIT 4) SELECT * FROM c",
            Some(4.0),
        ),
        ("SELECT b, count(*) FROM t GROUP BY b", None),
        ("SELECT DISTINCT b FROM t", None),
        ("SELECT * FROM t JOIN u ON t.b = u.c", None),
        ("SELECT a FROM t UNION ALL SELECT c FROM u", None),
        ("SELECT a FROM t INTERSECT SELECT c FROM u", None),
        ("SELECT a FROM t EXCEPT SELECT c FROM u", None),
        ("SELECT * FROM t WHERE a IN (SELECT c FROM u)", None),
        (
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.c = t.b)",
            None,
        ),
    ] {
        let got = estimate(&io, &conn, sql);
        match want {
            Some(want) => assert_eq!(got, want, "{sql}"),
            None => assert!(got >= 0.0, "{sql}: {got}"),
        }
    }
    // A compound's arms combine as their operator reads them.
    let (a, c) = (
        estimate(&io, &conn, "SELECT a FROM t"),
        estimate(&io, &conn, "SELECT c FROM u"),
    );
    for (sql, want) in [
        ("SELECT a FROM t UNION ALL SELECT c FROM u", a + c),
        ("SELECT a FROM t UNION SELECT c FROM u", a + c),
        ("SELECT a FROM t INTERSECT SELECT c FROM u", a.min(c)),
        ("SELECT a FROM t EXCEPT SELECT c FROM u", a),
    ] {
        assert_eq!(estimate(&io, &conn, sql), want, "{sql}");
    }
}

/// Do: concat_ws with BLOB separators (empty, a NUL, bytes), BLOB, empty and NULL arguments, and
/// numbers. Expect: SQLite 3.54's text: the separator and each argument as text, a BLOB its bytes,
/// every argument but a NULL one written, an empty one too, with the separator between; a NULL
/// separator NULL (`concatFuncCore` and `concatwsFunc`, release 3.54.0's func.c). Amphora gave NULL
/// for a BLOB separator and left BLOB arguments out (findings diff-218e78aa0f42cff2,
/// diff-24fa95590277913e, diff-3b05e239bef86a20, diff-6337522718bb9cef, diff-86205179ecc78c03,
/// diff-a0400a5512b948ae, diff-a128a508204fbdd4, diff-c448c8dc357e58ba, diff-e518f9fb47a71f6c and
/// diff-f0b1746622d11f4b, step 5's first run once the oracles called concat_ws at an arity SQLite
/// takes).
#[test]
fn concat_ws_writes_every_argument_but_null_a_blob_as_its_text() {
    let (io, conn) = open();
    for (sql, want) in [
        (
            "SELECT concat_ws(x'', 0, 3, 9007199254740993)",
            "'039007199254740993'",
        ),
        ("SELECT concat_ws(x'0102', NULL)", "''"),
        ("SELECT concat_ws(x'00', 1)", "'1'"),
        ("SELECT concat_ws('1e3', x'c3bf')", "'ÿ'"),
        ("SELECT concat_ws(0, 2, 100, x'c3bf')", "'201000ÿ'"),
        ("SELECT concat_ws(',', 'a', '', NULL, x'', 'b')", "'a,,,b'"),
        ("SELECT concat_ws(NULL, 'a')", "NULL"),
        (
            "SELECT concat_ws('-', 1.5, -2.0, 1e15)",
            "'1.5--2.0-1000000000000000.0'",
        ),
        ("SELECT typeof(concat_ws(',', NULL, NULL))", "'text'"),
        ("SELECT hex(concat_ws(x'00', 'a', 'b'))", "'610062'"),
    ] {
        assert_eq!(rows(&io, &conn, sql), [want], "{sql}");
    }
}

/// Do: a RIGHT JOIN whose ON no row passes, so every right row comes from its unmatched-row pass,
/// with a WHERE reading an IN subquery whose own WHERE is an OR of two index seeks (a multi-index
/// OR); the pass runs a copy of the subquery. Expect: SQLite's rows. Amphora panicked ("cursor id 2
/// is None"): the copy opened cursors of its own for the OR's index branches but sought on the
/// original's, which never ran, the main loop matching no row (finding norec-db0d4a055d27c23c,
/// step 5's nightly run of 2026-10-28; and the panics of its other runs).
#[test]
fn an_unmatched_row_pass_seeks_its_own_subquerys_cursors() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 REAL, c1 DOUBLE PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT OR IGNORE INTO t0 VALUES (255, '%')")
        .unwrap();
    let sub = "(SELECT s.c1 FROM t0 AS s WHERE ((s.c1 < -1) OR (-2.5 BETWEEN s.c1 AND s.c1)))";
    for (wh, want) in [
        (format!("(a0.c1 IN {sub})"), 0),
        (format!("(NOT (a0.c1 IN {sub}))"), 1),
        (format!("((a0.c1 IN {sub}) ISNULL)"), 0),
    ] {
        let sql = format!("SELECT NULL FROM t0 AS a0 RIGHT JOIN t0 AS a1 ON NULL WHERE {wh}");
        assert_eq!(rows(&io, &conn, &sql).len(), want, "{sql}");
    }
}

/// Do: an OR of a comparison with NULL and a GLOB whose pattern begins with a character class
/// (`[a-z]*`), on an indexed text column, which the planner answers with a multi-index OR: the GLOB's
/// branch seeks the range its pattern implies. Expect: SQLite's rows. The range holds every match but
/// not only matches (' a', '10' and 'b' lie within the bounds `[a-z]*` gives), so the GLOB must
/// still filter what the seek finds, as it does after a seek of its own (decision 3 of
/// docs/design/planner.md); the branch took its range as the GLOB itself and kept every row in it
/// (findings dqp-720239a34e22196d, eet-614fea129bc7ea47 and eet-ce1e661cf06a6492, step 5's first
/// runs).
#[test]
fn a_multi_index_or_still_filters_a_globs_range() {
    for (tables, sql, want) in [
        (
            &["CREATE TABLE t1(c0 TEXT)", "INSERT INTO t1 VALUES (' a'), ('b'), (NULL), ('10')", "CREATE INDEX i0 ON t1(c0)"][..],
            "SELECT count(*) FROM t1 WHERE (NULL < c0) OR (c0 GLOB '[a-z]*')",
            "1",
        ),
        (
            &["CREATE TABLE t1(c0 TEXT COLLATE BINARY)", "INSERT INTO t1 VALUES (' a')", "CREATE INDEX i0 ON t1(c0 DESC)"][..],
            "SELECT count(*) FROM t1 AS a0 WHERE ((NULL < a0.c0) OR (a0.c0 GLOB '[a-z]*'))",
            "0",
        ),
        (
            &["CREATE TABLE t0(c0 DECIMAL(10,2))", "INSERT OR IGNORE INTO t0 VALUES ('1e3')", "CREATE TABLE t2(c0 DECIMAL(10,2) PRIMARY KEY, c1 VARCHAR(10) NOT NULL, c2 REAL COLLATE BINARY UNIQUE)", "INSERT INTO t2 VALUES (-2.5, 10, -1)", "CREATE INDEX i0 ON t2(c1)"][..],
            "SELECT count(*) FROM t2 AS a0 JOIN t0 AS a1 ON ((a0.c2 == NULL) OR (a0.c1 GLOB '[a-z]*'))",
            "0",
        ),
        (
            &["CREATE TABLE t0(c0 VARCHAR(10) PRIMARY KEY)", "INSERT OR IGNORE INTO t0 VALUES (2.0)", "CREATE TABLE t2(c0 INTEGER COLLATE BINARY NOT NULL)", "INSERT INTO t2 VALUES ('1')"][..],
            "SELECT count(*) FROM t2 AS a0 JOIN t0 AS a1 ON ((a1.c0 <= NULL) OR (a1.c0 GLOB '[a-z]*'))",
            "0",
        ),
    ] {
        let (io, conn) = open();
        for table in tables {
            conn.execute(table).unwrap();
        }
        assert_eq!(rows(&io, &conn, sql), [want], "{sql}");
    }
}

/// Do: compare a numeric column with text holding a NUL after a decimal number (`'1.5' || x'00'`)
/// where the comparison's affinity is applied to a seek key or an IN subquery's probe (an index
/// seek, a join's seek, a range scan, `x IN (SELECT n ...)`). Expect: SQLite's rows: SQLite 3.54
/// reads the number before the NUL as the text's, here the REAL 1.5. A comparison answered so
/// already (AR); the Affinity instruction, which seeks and probes take, converted only text whose
/// number was a whole integer, leaving `'1.5\0'` text, so nothing numeric matched (findings
/// diff-b72da64068c12b5e, diff-ecbb9d89531a62f7, diff-a51fd77b72b31d7a, norec-7490c5b68dd496e1,
/// diff-66a81e6f5988230c and diff-ba2d9989f045f65f, step 5's first runs).
#[test]
fn a_seek_or_probe_reads_the_number_before_a_nul() {
    for (tables, sql, want) in [
        (
            &["CREATE TABLE t0(c0 BOOLEAN PRIMARY KEY, c1 REAL)", "INSERT INTO t0 VALUES ('1.5', -9223372036854775808)"][..],
            "SELECT 'a' FROM t0 AS a0 JOIN t0 AS a1 ON (a0.c0 IS NOT DISTINCT FROM (a1.c0 || x'00'))",
            1,
        ),
        (
            &["CREATE TABLE t0(c0 BOOLEAN PRIMARY KEY, c1 REAL)", "INSERT INTO t0 VALUES ('1.5', 0)"][..],
            "SELECT c0 FROM t0 WHERE c0 = ('1.5' || x'00')",
            1,
        ),
        (
            &["CREATE TABLE t0(c0 DECIMAL(10,2) PRIMARY KEY)", "INSERT INTO t0 VALUES (0.5)", "CREATE TABLE t1(c0 VARCHAR(10), c1 INT, c2 BOOLEAN PRIMARY KEY NOT NULL)", "INSERT INTO t1 VALUES (1000000000000000.0, x'00', 'x')"][..],
            "SELECT NULL FROM t1 AS a0 JOIN t0 AS a2 ON ((a2.c0 || a0.c1) IN (SELECT s96.c0 FROM t0 AS s96))",
            1,
        ),
        (
            &["CREATE TABLE t0(c0 VARCHAR(10), c1 NUMERIC COLLATE RTRIM, c2 NUMERIC)", "INSERT OR IGNORE INTO t0 VALUES (2.0, NULL, -2.5)", "CREATE TABLE t1(c0 DOUBLE PRIMARY KEY, c1 DOUBLE)", "INSERT OR IGNORE INTO t1 VALUES ('é', 1), (9223372036854775807, x'41')", "CREATE TABLE t2(c0 TEXT, c1 VARCHAR(10))", "INSERT INTO t2 VALUES (-2.5, x'00')"][..],
            "SELECT 'true' FROM t1 AS a0 RIGHT JOIN t2 AS a1 ON ((a1.c0 || a1.c1) IN (SELECT s32.c2 FROM t0 AS s32))",
            2,
        ),
        (
            &["CREATE TABLE t0(c0 BOOLEAN, c1 NUMERIC COLLATE NOCASE PRIMARY KEY, c2 BOOLEAN)", "INSERT OR IGNORE INTO t0 VALUES ('-1', -2.5, x'00')"][..],
            "SELECT 1 FROM t0 AS a0, t0 AS a1 WHERE (a1.c1 >= (a0.c1 || substr(a0.c2, x'')))",
            1,
        ),
        (
            &["CREATE TABLE t0(c0 DECIMAL(10,2) UNIQUE, c1 TEXT)", "INSERT INTO t0 VALUES (x'00', -9223372036854775808), (255, 9223372036854775807)", "INSERT OR IGNORE INTO t0 VALUES (0.5, '1e3')"][..],
            "SELECT (a1.c1 COLLATE RTRIM) FROM t0 AS a0, t0 AS a1 JOIN t0 AS a2 ON ((a0.c0 || a2.c0) < a1.c0)",
            11,
        ),
        (
            &["CREATE TABLE t1(c0 REAL, c1 TEXT, c2 INT PRIMARY KEY)", "INSERT INTO t1 VALUES (x'00', x'0102', ' a')", "CREATE TABLE t2(c0 BOOLEAN COLLATE NOCASE, c1  COLLATE RTRIM, c2 BOOLEAN COLLATE BINARY, c3 DECIMAL(10,2))", "INSERT OR IGNORE INTO t2 VALUES (3.0, 1.5, 10, NULL), (-9223372036854775808, NULL, 1.5, 255)"][..],
            "SELECT json_extract('\"x\"', '$[0]') FROM t2 AS a0, t2 AS a1 JOIN t1 AS a2 ON (a1.c0 <= (a0.c2 || a2.c0))",
            3,
        ),
    ] {
        let (io, conn) = open();
        for table in tables {
            conn.execute(table).unwrap();
        }
        assert_eq!(rows(&io, &conn, sql).len(), want, "{sql}");
    }
}

/// Do: seek the smallest rowid, -9223372036854775808, by a REAL that is exactly it (a literal, a
/// REAL column, an IN list's value, a join's key) and by text that reads as it. Expect: SQLite's
/// row: its SeekRowid takes a REAL that is an integer exactly within the rowid range, the smallest
/// among them (the exclusion of ticket #3922 is its column affinity's, not the seek's). Amphora's
/// seek converted the key by NUMERIC affinity, which keeps -2^63 a REAL, and found no row (findings
/// diff-f254967d5150ea85 and eet-56b728bcbf4c5a59, step 5's first runs).
#[test]
fn a_rowid_seek_finds_the_smallest_rowid_by_a_real() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (-9223372036854775808, 1)")
        .unwrap();
    conn.execute("CREATE TABLE r(x REAL)").unwrap();
    conn.execute("INSERT INTO r VALUES (-9223372036854775808)")
        .unwrap();
    for sql in [
        "SELECT count(*) FROM t WHERE a = -9223372036854775808.0",
        "SELECT count(*) FROM t WHERE a IN (-9223372036854775808.0, 5)",
        "SELECT count(*) FROM t, r WHERE t.a = r.x",
        "SELECT count(*) FROM t, r WHERE t.a IN (r.x, 5)",
        "SELECT count(*) FROM t WHERE a = '-9.2233720368547758e+18'",
        "SELECT count(*) FROM t AS a0 CROSS JOIN t AS a1, t AS a2 WHERE (CAST(CAST(a0.a AS REAL) AS TEXT) = a2.a)",
    ] {
        assert_eq!(rows(&io, &conn, sql), ["1"], "{sql}");
    }
    // Beyond the range, or not exactly an integer, a REAL names no rowid.
    conn.execute("CREATE TABLE u(a INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO u VALUES (1)").unwrap();
    for (sql, want) in [
        (
            "SELECT count(*) FROM t WHERE a = -9223372036854777856.0",
            "0",
        ),
        ("SELECT count(*) FROM u WHERE a = 1.5", "0"),
        ("SELECT count(*) FROM u WHERE a = 1.0", "1"),
    ] {
        assert_eq!(rows(&io, &conn, sql), [want], "{sql}");
    }
}

/// Do: AND two comparisons, each on a column with an index of its own, which the planner may answer
/// with a multi-index AND (an intersection of the two seeks): one comparison whose affinity the
/// index's column can't serve (`CAST(1e15 AS INTEGER) <= c0`, NUMERIC against a TEXT column: index
/// order is no numeric order of text), another whose collation isn't the index's (`a = 'abc'
/// COLLATE NOCASE` against a BINARY index). Expect: SQLite's rows: such an index can't serve such a
/// comparison (sqlite3IndexAffinityOk; a seek's collation is its index's), as Amphora's single-index
/// seeks already held; the intersection used them anyway (finding norec-bb8e0c1b5a66c950, step 5's
/// first runs: 1 row for SQLite's 0).
#[test]
fn a_multi_index_and_seeks_only_indexes_its_comparisons_fit() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 VARCHAR(10) UNIQUE, c1 INT UNIQUE, c2 DOUBLE COLLATE BINARY PRIMARY KEY NOT NULL, c3 VARCHAR(10) COLLATE RTRIM NOT NULL)").unwrap();
    conn.execute("INSERT OR IGNORE INTO t1 VALUES (0.0, 3, 1e308, 3)")
        .unwrap();
    conn.execute("CREATE TABLE t(a TEXT, b INT)").unwrap();
    conn.execute("CREATE INDEX ia ON t(a)").unwrap();
    conn.execute("CREATE INDEX ib ON t(b)").unwrap();
    conn.execute("INSERT INTO t VALUES ('ABC', 1), ('abd', 2)")
        .unwrap();
    for (sql, want) in [
        ("SELECT count(*) FROM t1 AS a0 WHERE (CAST(1000000000000000.0 AS INTEGER) BETWEEN a0.c1 AND a0.c0)", "0"),
        ("SELECT count(*) FROM t WHERE a = 'abc' COLLATE NOCASE AND b = 1", "1"),
        ("SELECT count(*) FROM t WHERE a > 'abc' COLLATE NOCASE AND b >= 1", "1"),
        ("SELECT count(*) FROM t WHERE a = 5 AND b = 1", "0"),
    ] {
        assert_eq!(rows(&io, &conn, sql), [want], "{sql}");
    }
}

/// Do: a correlated `x IN (SELECT y ...)` where y is a lone REAL column and x an integer with no
/// affinity of its own, which the planner unnests to a semi-join on `x = y`. Expect: SQLite's rows:
/// IN gives the probe y's REAL affinity, so -9223372036854775807 finds the REAL -2^63 (cause AV),
/// where `=` compares the integer with the REAL exactly and finds nothing. The semi-join's term
/// compared as `=` does (finding diff-6a0a824a042f7356, step 5's first runs: 0 rows for 1).
#[test]
fn an_unnested_in_compares_as_in_does() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 REAL COLLATE NOCASE, c1 NUMERIC COLLATE BINARY PRIMARY KEY, c2 NUMERIC, c3 DECIMAL(10,2) NOT NULL)").unwrap();
    conn.execute("INSERT OR IGNORE INTO t0 VALUES (-9223372036854775808, 10, 'A', ' ')")
        .unwrap();
    conn.execute("CREATE TABLE t1(c0 BOOLEAN, c1 BLOB PRIMARY KEY, c2 BLOB COLLATE RTRIM)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (9223372036854775807, 2, 0)")
        .unwrap();
    for (sql, want) in [
        ("SELECT count(*) FROM t1 AS a0 WHERE ((- a0.c0) IN (SELECT s593.c0 FROM t0 AS s593 WHERE ((s593.c2 == -2.5) != (100 + a0.c0))))", "1"),
        ("SELECT count(*) FROM t1 AS a0 WHERE ((- a0.c0) IN (SELECT s.c0 FROM t0 AS s WHERE s.c1 > a0.c0 - a0.c0))", "1"),
        ("SELECT count(*) FROM t1 AS a0 WHERE ((- a0.c0) IN (SELECT s.c0 FROM t0 AS s))", "1"),
        ("SELECT count(*) FROM t1 AS a0 WHERE ((- a0.c0) = (SELECT s.c0 FROM t0 AS s WHERE s.c1 > a0.c0 - a0.c0))", "0"),
    ] {
        assert_eq!(rows(&io, &conn, sql), [want], "{sql}");
    }
}

/// Do: sum(), total() and avg() over integers past 2^52 with text and BLOBs among them, where sum()'s
/// exact integer total turns floating-point at a REAL, a BLOB or text that is no integer. Expect:
/// SQLite 3.54's answers: the floating-point total starts from the integer one split at 2^14
/// (kahanBabuskaNeumaierInit), so a large total keeps its low bits, and a BLOB or text that is no
/// integer adds its REAL (sumStep, func.c). Amphora turned the integer total into a REAL whole when
/// text or a BLOB came, losing its low bits: 9223372036854775807, '1.5' and -9223372036854775808
/// summed to 1.5 where SQLite sums 0.5 (findings diff-6e3bfa5d3e0906f5, diff-6708219ff6f6337e,
/// diff-31fe0233802a6ff0, diff-93b25a1abd23f665, pqs-7a3d84a82e07dc63 and pqs-05f1e48d4354286b).
/// total() and avg() sum in floating point from the first value and kept SQLite's answers. A sum
/// that is TRUE keeps every row of the PQS findings' pivot query, a FALSE one none.
#[test]
fn sum_keeps_a_large_integer_totals_low_bits_where_it_turns_real() {
    let (io, conn) = open();
    for (n, (column, values, want, kept)) in [
        (
            "VARCHAR(10)",
            "(9223372036854775807), ('1.5'), (-9223372036854775808)",
            "0.5|0.5|0.16666666666666666",
            3,
        ),
        (
            "BOOLEAN",
            "(9223372036854775807), (x''), (-9223372036854775808)",
            "-1.0|-1.0|-0.33333333333333332",
            3,
        ),
        (
            "TEXT",
            "(10), (-9223372036854775808), ('a'), (9223372036854775807)",
            "9.0|9.0|2.25",
            4,
        ),
        (
            "TEXT",
            "(9223372036854775807), (-2.5), (1), ('1.5'), (-9223372036854775808)",
            "-1.0|-1.0|-0.2",
            5,
        ),
        (
            "BLOB",
            "(9223372036854775807), (x'41'), (-9223372036854775808), (2), (-1)",
            "0.0|0.0|0.0",
            0,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        conn.execute(format!("CREATE TABLE t{n}(c0 {column})"))
            .unwrap();
        conn.execute(format!("INSERT INTO t{n} VALUES {values}"))
            .unwrap();
        assert_eq!(
            rows(
                &io,
                &conn,
                &format!("SELECT sum(c0), total(c0), avg(c0) FROM t{n}")
            ),
            [want],
            "{values}"
        );
        assert_eq!(
            rows(
                &io,
                &conn,
                &format!("SELECT rowid FROM t{n} WHERE (SELECT sum(c0) FROM t{n})")
            )
            .len(),
            kept,
            "{values}"
        );
    }
}

/// Do: abs() of -0.0, of a negative zero as text and as a BLOB, and of 0 divided by a negative REAL;
/// arithmetic between a REAL and text spelling a negative zero ('-0', timediff's '-0000-08-10 ...');
/// `(- 0.0) * x` beside `-(CASE ... ELSE 0.0 END) * x`. Expect: SQLite 3.54's signs. abs negates
/// only a value below zero (absFunc), so -0.0 stays -0.0. Text whose numeric type is the integer 0
/// is that integer in integer arithmetic, but floating-point arithmetic reads each operand's REAL
/// afresh (fp_math's sqlite3VdbeRealValue), -0.0 for '-0'. `- 0.0` is the literal -0.0, while the
/// minus of any other expression is 0 - x, 0.0 for 0.0 (TK_UMINUS in sqlite3ExprCodeTarget).
/// Amphora's abs dropped the sign (findings diff-d6a7bedd446e5469, diff-5113ce99737b9e06 and
/// diff-f93f0363c5e4f173), and it multiplied a REAL by the integer a text read as
/// (diff-cfa8b811932383e5). The two minus forms give opposite signs in SQLite too, as Amphora gave
/// them: eet-49998c30035cd4e6 compared forms that aren't equivalent.
#[test]
fn a_negative_zero_keeps_its_sign_through_abs_and_text_arithmetic() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 INT, c1 BOOLEAN, c2 VARCHAR(10))")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES (3.0, 2, 9223372036854775807), (-1e-308, -1e-308, -2.5)")
        .unwrap();
    assert_eq!(
        signed_rows(
            &io,
            &conn,
            "SELECT abs(0 / c0), abs('%' / c1), abs('%' / c2) FROM t0"
        ),
        ["0|0|0", "-0.0|-0.0|-0.0"]
    );
    assert_eq!(
        signed_rows(&io, &conn, "SELECT abs(-0.0), abs('-0.0'), abs('-0'), abs(x'2d30'), abs(-2.5), abs('-2.5x'), abs(-3), abs(0.0)"),
        ["-0.0|-0.0|-0.0|-0.0|2.5|2.5|3|0.0"]
    );
    assert_eq!(
        signed_rows(&io, &conn, "SELECT '-0' * 0.1, 0.1 * '-0', '-0' / 1.0, '-0' - 0.0, '-0' * '0.5', x'2d30' * 0.5, '-0' * 1, typeof('-0' * 1), '-0' + 0.0, '-0.0' * 1"),
        ["-0.0|-0.0|-0.0|-0.0|-0.0|-0.0|0|'integer'|0.0|-0.0"]
    );
    assert_eq!(
        signed_rows(
            &io,
            &conn,
            "SELECT timediff(1, 255), timediff(1, 255) * 0.1, timediff(1, 255) * 255"
        ),
        ["'-0000-08-10 00:00:00.000'|-0.0|0"]
    );
    conn.execute("CREATE TABLE t1(c0 DOUBLE PRIMARY KEY NOT NULL, c1 REAL)")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (100, 'abc'), (3, -9223372036854775808)")
        .unwrap();
    assert_eq!(
        signed_rows(&io, &conn, "SELECT (- 0.0) * c1, -(CASE WHEN c0 > 1000 THEN '1' ELSE 0.0 END) * c1 FROM t1 ORDER BY c0"),
        ["0.0|-0.0", "-0.0|0.0"]
    );
}

/// Do: unicode() of BLOBs that aren't well-formed UTF-8: a continuation byte first (unhex() and
/// JSONB headers make them), a lead byte followed by more continuation bytes than it calls for,
/// overlong and surrogate forms, a NUL first; and of text and numbers. Expect: SQLite 3.54's
/// integers: unicodeFunc reads the first character with sqlite3Utf8Read, which takes a byte below
/// 0xC0 as its own value (0x90 is 144), lets a lead byte take in every continuation byte after it in
/// a u32 that wraps (x'C3BFBFBFBFBF' is -1 as a C int) and gives U+FFFD only for a value below 0x80
/// or a surrogate; an empty BLOB or one starting with a NUL is NULL. Amphora read the BLOB as lossy
/// UTF-8 text and gave 65533 for every ill-formed start (findings diff-ec071f3026c035f9,
/// diff-d94be5be23c74a1a, diff-fe8301cf10f64b0b and diff-7bf9cd99a17e6672).
#[test]
fn unicode_reads_a_blobs_first_character_as_sqlite_does() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT unicode(unhex('9007199254740993')), unicode(jsonb_set('1.0e+308')), unicode(jsonb_array(-1e-308))"),
        ["144|133|171"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT unicode(x'90'), unicode(x'C3BF'), unicode(x'C3BFBF'), unicode(x'C3BFBFBFBFBF'), unicode(x'F8BFBFBFBFBF'), unicode(x'FF'), unicode(x'C080'), unicode(x'EDA080'), unicode(x'E0828041'), unicode(x''), unicode(x'00'), unicode(x'0041'), unicode(x'41')"),
        ["144|255|16383|-1|1073741823|65533|65533|65533|128|NULL|NULL|NULL|65"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT unicode('é'), unicode(char(65534)), unicode(12), unicode(-1.5)"
        ),
        ["233|65534|49|45"]
    );
}

/// Do: timediff() with a date before Julian day 0 on either side, as date(0)'s '-4713-11-24' is (its
/// midnight, half a day before), and with dates at the range's two ends. Expect: SQLite 3.54's
/// answers: each argument is held to isDate's rule, no error and a Julian day from 0 to
/// 9999-12-31 23:59:59.999 (validJulianDay, date.c), so a date before -4713-11-24 12:00:00 makes
/// timediff NULL, as it makes date() and julianday() NULL. Amphora's timediff checked only for an
/// error and answered '-0000-00-00 12:00:00.000' (finding diff-1ac238359d74b8a1).
#[test]
fn timediff_of_a_date_before_julian_day_zero_is_null() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t0(c0 DECIMAL(10,2), c1 BLOB NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES (0.5, x'00')").unwrap();
    assert_eq!(
        rows(&io, &conn, "SELECT timediff(date(0), (c0 = c1)) FROM t0"),
        ["NULL"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT date(0), timediff(date(0), 0), timediff(0, date(0)), timediff('-4713-11-24 11:59:59', 0), timediff('-4713-11-24 12:00', '-4713-11-24 11:00'), julianday('-4713-11-24')"),
        ["'-4713-11-24'|NULL|NULL|NULL|NULL|NULL"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT timediff('-4713-11-24 12:00:00', 0), timediff('9999-12-31 23:59:59.999', '-4713-11-24 12:00:00'), timediff('-4713-11-24 12:00:00', '0000-01-01')"),
        ["'+0000-00-00 00:00:00.000'|'+14712-01-07 11:59:59.999'|'-4712-01-06 12:00:00.000'"]
    );
}

/// Do: atanh() of the REAL the finding multiplies out, `-1e-308 * '1.0e+308'` (-0.99999999999999989,
/// the same double in both), of its opposite, and of opposite pairs. Expect: SQLite 3.54's answers,
/// which are its C library's atanh: computed on |x| and given x's sign, so it is odd and keeps its
/// digits near -1. Amphora used Rust's f64::atanh, ln_1p(2x / (1 - x)) / 2 of x signed, which near
/// -1 rounds 2x / (1 - x) and lost half the digits: -18.36840028483855 where SQLite gives
/// -18.714973875118524 (finding diff-827d8a2d59c98538); atanh(-0.75) wasn't -atanh(0.75) either.
#[test]
fn atanh_is_odd_and_keeps_its_digits_near_minus_one() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t1(c0 REAL COLLATE BINARY, c1 TEXT PRIMARY KEY, c2 )")
        .unwrap();
    conn.execute("INSERT INTO t1 VALUES (0.5, 1e308, ' ')")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT atanh((-1e-308 * a0.c1)), -1e-308 * a0.c1 FROM t1 AS a0"
        ),
        ["-18.714973875118524|-0.99999999999999989"]
    );
    assert_eq!(
        rows(&io, &conn, "SELECT atanh(0.9999999999999999), atanh(-0.75) = -atanh(0.75), atanh(-0.5) = -atanh(0.5), atanh(-0.25) = -atanh(0.25), atanh(-1e-10) = -atanh(1e-10), atanh(2), atanh(-2)"),
        ["18.714973875118524|1|1|1|1|NULL|NULL"]
    );
}

/// Do: RIGHT and FULL JOINs whose rows come from the main loop, from the unmatched-row pass or from
/// both, LIMIT 2 at every OFFSET from 0 to past the last row. Expect: SQLite's rows: the OFFSET
/// skips rows across both passes and the LIMIT counts the rest. The pass enters the main loop's
/// body as a subroutine; a row the OFFSET skipped jumped to the right table's next row in the main
/// loop, past the subroutine's Return (SQLite resolves that level's continue label at the Return,
/// sqlite3WhereEnd), so the pass ran on as the main loop and kept or lost rows by the tables' sizes
/// (findings diff-60e05c03077b9681, diff-0cdfabdc3c24bfc3, diff-149f30f47877187c,
/// diff-ce9dc54f20fa07db, eet-010828432e21524e and four more of step 5's second runs).
#[test]
fn an_offset_skips_a_right_joins_rows_in_either_pass() {
    let (io, conn) = open();
    conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)")
        .unwrap();
    conn.execute("CREATE TABLE s(a INTEGER)").unwrap();
    conn.execute("INSERT INTO s VALUES (2), (4)").unwrap();
    for offset in 0..=7usize {
        for (from, all) in [
            ("s RIGHT JOIN t ON NULL", 5),
            ("s RIGHT JOIN t ON s.a = t.a", 5),
            ("s RIGHT JOIN t ON NULL WHERE t.a <> 3", 4),
            ("s FULL JOIN t ON s.a = t.a", 5),
            ("s FULL JOIN t ON NULL", 7),
            ("s FULL JOIN t ON NULL WHERE s.a IS NULL", 5),
        ] {
            let sql = format!("SELECT 1 FROM {from} LIMIT 2 OFFSET {offset}");
            let want = 2usize.min(all - offset.min(all));
            assert_eq!(rows(&io, &conn, &sql).len(), want, "{sql}");
        }
    }
    conn.execute("CREATE TABLE t0(c0 INTEGER UNIQUE)").unwrap();
    conn.execute("INSERT INTO t0 VALUES (2.0), ('x')").unwrap();
    conn.execute("CREATE TABLE t1(c0 TEXT COLLATE BINARY, c1 DECIMAL(10,2))")
        .unwrap();
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT -100 FROM t1 AS a0 RIGHT JOIN t0 AS a1 ON 10 LIMIT 4 OFFSET 1"
        ),
        ["-100"]
    );
}

/// Do: atanh() of REALs the generator makes, and of 0.25, which the platforms' C libraries round
/// apart. Expect: SQLite 3.54's answers, which are its C library's atanh (math1Func, func.c):
/// glibc 2.44 rounds atanh correctly, and macOS 26's libm is x below 2^-27 and
/// ln_1p(2|x| / (1 - |x|)) / 2 given x's sign above, an ulp below the correctly rounded value at
/// 0.25 (each the platform's answer for 3,000,019 arguments). The libm crate's atanh gave
/// 0.54930614433405478 for 0.5, where both give 0.54930614433405489 (finding diff-2074283afc468d4a
/// and sixteen more of step 5's second runs, on macOS).
#[test]
fn atanh_has_its_platforms_c_library_digits() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT atanh(0.5), atanh(-0.5), atanh(0.1), atanh(0.75), atanh(0.9999999999999999)"),
        ["0.54930614433405489|-0.54930614433405489|0.10033534773107558|0.97295507452765662|18.714973875118524"]
    );
    let quarter = if cfg!(target_vendor = "apple") {
        "0.25541281188299531"
    } else {
        "0.25541281188299536"
    };
    assert_eq!(
        rows(&io, &conn, "SELECT atanh(0.25), -atanh(-0.25)"),
        [format!("{quarter}|{quarter}")]
    );
}

/// Do: json_error_position() of numbers whose leading zero a digit follows, signed and not, alone
/// and inside an array or object, and of timediff(0, 0), '+0000-00-00 00:00:00.000'. Expect: SQLite
/// 3.54's positions: one past the number's start (jsonTranslateTextToBlob), the digit after an
/// unsigned zero but the zero after a sign. Amphora reported the digit after the zero after a sign
/// too, 3 for timediff(0, 0) where SQLite gives 2 (finding diff-9bcd332bd8ccd930, step 5's
/// nightly run of 2026-10-27).
#[test]
fn json_error_position_reports_a_signed_leading_zero_at_the_zero() {
    let (io, conn) = open();
    assert_eq!(
        rows(&io, &conn, "SELECT json_error_position('+00'), json_error_position('-00'), json_error_position('00'), json_error_position('-01'), json_error_position('[01]'), json_error_position('[-01]'), json_error_position('[+012]'), json_error_position('{\"a\":-00}'), json_error_position(timediff(0, 0)), json_error_position('-0x1'), json_error_position('-0.5'), json_error_position('-0')"),
        ["2|2|2|2|3|3|3|7|2|0|0|0"]
    );
}

/// Do: a WHERE ANDing a NULL test SQLite's parser folds to 0 (`1e308 ISNULL`) with
/// likelihood(NULL, NULL), which SQLite refuses wherever it resolves it: the call under a COLLATE,
/// under NOT, IS NULL, + 1, BETWEEN or IN over that COLLATE; and bare, under unary plus, inside
/// abs() or LIKE, or with the COLLATE on the NULL test instead. Expect: SQLite 3.54's answers. Its
/// parser makes such an AND the integer 0 unless a side carries EP_HasFunc (sqlite3ExprAnd,
/// sqlite3.c line 113984), and a COLLATE node takes no flag from its operand
/// (sqlite3ExprAddCollateToken, line 112995, sets only EP_Collate and EP_Skip): with the call under
/// a COLLATE the AND folds and the call is never resolved, so the query runs; in the other forms the
/// call shows, or the NULL test's 0 doesn't, and SQLite refuses it. Amphora looked through the
/// COLLATE for a call, kept the AND, and refused the folded forms too (finding
/// error-a2539ea61c26b70f, whose TLP partitions are the last query here).
#[test]
fn a_call_under_a_collate_folds_away_with_a_false_and() {
    let (io, conn) = open();
    let error = |sql: &str| match conn.prepare(sql).and_then(|mut stmt| loop {
        match stmt.step()? {
            StepResult::IO => io.step()?,
            StepResult::Done => break Ok(()),
            _ => {}
        }
    }) {
        Ok(()) => "no error".to_owned(),
        Err(e) => e.to_string(),
    };
    conn.execute(
        "CREATE TABLE t1(c0 REAL PRIMARY KEY, c1 INT, c2 VARCHAR(10), c3 NUMERIC NOT NULL)",
    )
    .unwrap();
    let call = "likelihood(NULL, NULL)";
    for folded in [
        format!("(1e308 ISNULL) AND ({call} COLLATE RTRIM)"),
        format!("({call} COLLATE RTRIM) AND (1e308 ISNULL)"),
        format!("(1e308 ISNULL) AND (NOT ({call} COLLATE RTRIM))"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) ISNULL)"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) + 1)"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) BETWEEN 1 AND 2)"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) IN (1, 2))"),
        format!("(1e308 ISNULL) AND (CAST({call} AS INT) COLLATE RTRIM)"),
    ] {
        assert_eq!(
            error(&format!("SELECT * FROM t1 WHERE {folded}")),
            "no error",
            "{folded}"
        );
    }
    for refused in [
        format!("(1e308 ISNULL) AND {call}"),
        format!("(1e308 ISNULL) AND (+{call})"),
        format!("(1e308 ISNULL) AND (abs({call} COLLATE RTRIM))"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) LIKE 'a')"),
        format!("(1e308 ISNULL) AND (({call} COLLATE RTRIM) IN (1, abs(2)))"),
        format!("((1e308 ISNULL) COLLATE RTRIM) AND ({call} COLLATE RTRIM)"),
    ] {
        assert!(
            error(&format!("SELECT * FROM t1 WHERE {refused}"))
                .contains("second argument to likelihood() must be a constant"),
            "{refused}"
        );
    }
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT min(m COLLATE BINARY) FROM (SELECT NULL AS m WHERE 0 UNION ALL SELECT min((NULL COLLATE BINARY)) FROM t1 AS a0 WHERE ((1e308 ISNULL) AND (likelihood(NULL, NULL) COLLATE RTRIM)) UNION ALL SELECT min((NULL COLLATE BINARY)) FROM t1 AS a0 WHERE ((1e308 ISNULL) AND (NOT (likelihood(NULL, NULL) COLLATE RTRIM))) UNION ALL SELECT min((NULL COLLATE BINARY)) FROM t1 AS a0 WHERE ((1e308 ISNULL) AND ((likelihood(NULL, NULL) COLLATE RTRIM) ISNULL)))"
        ),
        ["NULL"]
    );
}

/// Do: jsonb_array(), jsonb_object(), jsonb_group_array() and jsonb_group_object() of a JSONB
/// argument whose header is wider than it needs (jsonb_patch('1e3', 0) is x'D3000130', SQLite's
/// jsonBlobOverwrite widening the shorter 0 into the place of 1e3), of one nested in an array, and
/// of one holding JSON5 forms (.5, 0x1F, a \u escape in single quotes); and jsonb_set(),
/// jsonb_insert(), jsonb_replace() and jsonb_array_insert() of the same. Expect: SQLite 3.54's
/// bytes. Its jsonb_array, jsonb_object and their aggregates write every argument as JSON text, a
/// JSONB BLOB as jsonTranslateBlobToText renders it (jsonAppendSqlValue), and make the whole text
/// JSONB again (jsonReturnStringAsBlob), so the argument comes back with each header at its
/// smallest and its JSON5 forms as JSON (13 30 for 0, 0.5, 31); the edits put the argument's bytes
/// in as they are. Amphora copied a JSONB argument's bytes into the array or object (finding
/// diff-0d96c0783d6fbbce: x'5b00d3000130' for SQLite's x'3b001330').
#[test]
fn jsonb_containers_write_a_jsonb_argument_as_its_text_would() {
    let (io, conn) = open();
    let patch = "jsonb_patch('1e3', 0)";
    assert_eq!(
        rows(&io, &conn, &format!("SELECT {patch}")),
        ["x'd3000130'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            &format!("SELECT jsonb_array(NULL, {patch}), jsonb_array({patch}), jsonb_array(x'D3000130', x'D3000130'), jsonb_object('a', {patch}), jsonb_array(jsonb_set('[1]', '$[0]', {patch}))")
        ),
        ["x'3b001330'|x'2b1330'|x'4b13301330'|x'4c17611330'|x'3b2b1330'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            &format!("SELECT jsonb_group_array(x), jsonb_group_object('k', x) FROM (SELECT {patch} AS x)")
        ),
        ["x'2b1330'|x'4c176b1330'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            "SELECT jsonb_array(jsonb('[.5, 0x1F, ''\\u0041'']'))"
        ),
        ["x'cb10cb0e35302e35233331685c7530303431'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            &format!("SELECT jsonb_set('[1]', '$[0]', {patch}), jsonb_insert('[]', '$[#]', {patch}), jsonb_replace('[1]', '$[0]', {patch}), jsonb_array_insert('[1]', '$[0]', {patch})")
        ),
        ["x'4bd3000130'|x'4bd3000130'|x'4bd3000130'|x'6bd30001301331'"]
    );
    assert_eq!(
        rows(
            &io,
            &conn,
            &format!("SELECT json_array({patch}), json_object('a', {patch})")
        ),
        ["'[0]'|'{\"a\":0}'"]
    );
}

/// Do: jsonb_insert() and jsonb_set() appending to an array, by '$[#]' and by the index one past
/// its end, values of one, two and four bytes (NULL, 0, 'abc', and x'D3000130', a 0 behind a
/// header wider than it needs), into the root array ('[]', '[1]'), an object's array ('$.a[#]')
/// and a nested one ('$[0][#]'). Expect: SQLite 3.54's bytes, each array's header counting every
/// byte it holds. Amphora's root array header left out the one-byte placeholder its path
/// traversal puts in first, which an object's array counts there, so jsonb_insert('[]', '$[#]', 0)
/// was x'1b1330', a header one byte short of what it holds (found by the controls of
/// jsonb_containers_write_a_jsonb_argument_as_its_text_would).
#[test]
fn an_element_appended_to_the_root_array_counts_in_its_header() {
    let (io, conn) = open();
    let values = ["NULL", "0", "x'D3000130'", "'abc'"];
    for (doc, path, want) in [
        (
            "[]",
            "$[#]",
            "x'1b00'|x'2b1330'|x'4bd3000130'|x'4b3a616263'",
        ),
        (
            "[]",
            "$[0]",
            "x'1b00'|x'2b1330'|x'4bd3000130'|x'4b3a616263'",
        ),
        (
            "[1]",
            "$[1]",
            "x'3b133100'|x'4b13311330'|x'6b1331d3000130'|x'6b13313a616263'",
        ),
        (
            "[1]",
            "$[#]",
            "x'3b133100'|x'4b13311330'|x'6b1331d3000130'|x'6b13313a616263'",
        ),
        (
            "{\"a\":[]}",
            "$.a[#]",
            "x'4c17611b00'|x'5c17612b1330'|x'7c17614bd3000130'|x'7c17614b3a616263'",
        ),
        (
            "[[]]",
            "$[0][#]",
            "x'2b1b00'|x'3b2b1330'|x'5b4bd3000130'|x'5b4b3a616263'",
        ),
    ] {
        for function in ["jsonb_insert", "jsonb_set"] {
            let columns: Vec<String> = values
                .iter()
                .map(|value| format!("{function}('{doc}', '{path}', {value})"))
                .collect();
            assert_eq!(
                rows(&io, &conn, &format!("SELECT {}", columns.join(", "))),
                [want],
                "{function} {doc} {path}"
            );
        }
    }
}
