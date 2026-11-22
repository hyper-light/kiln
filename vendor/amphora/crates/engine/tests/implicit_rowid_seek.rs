//! AC-T1.1.1: bounded resumed reads use existing secondary-index rowid suffixes.
//! Counters prove work is bounded by the requested page, independently of the
//! number of duplicate keys skipped. No elapsed-time deadline is involved.
use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
    StatementStatusCounter as Counter,
};
use std::sync::Arc;

struct Fixture {
    _database: Arc<Database>,
    connection: Arc<Connection>,
}

impl Fixture {
    fn new() -> Self {
        let database = Database::open(
            Arc::new(MemoryIO::new()),
            "implicit_rowid.db",
            OpenOptions::new(Arc::new(SqliteDialect))
                .db_opts(DatabaseOpts::new().with_without_rowid(true)),
        )
        .unwrap();
        let connection = database.connect(ConnectOpts::new()).unwrap();
        Self {
            _database: database,
            connection,
        }
    }

    fn query(&self, sql: &str) -> (Vec<Vec<String>>, u64, u64) {
        let mut statement = self.connection.prepare(sql).unwrap();
        statement.reset().unwrap();
        let rows = statement
            .run_collect_rows()
            .unwrap()
            .iter()
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect();
        (
            rows,
            statement.stmt_status(Counter::RowsRead),
            statement.stmt_status(Counter::VmStep),
        )
    }

    fn exec(&self, sql: &str) {
        self.query(sql);
    }

    fn explain(&self, sql: &str) -> Vec<Vec<String>> {
        let plan = self.query(&format!("EXPLAIN {sql}")).0;
        println!("SQL: {sql}");
        for row in &plan {
            println!("{}", row.join(" | "));
        }
        plan
    }
}

fn seek_width(plan: &[Vec<String>], width: usize) -> bool {
    plan.iter().any(|row| {
        if !matches!(row[1].as_str(), "SeekLT" | "SeekLE" | "SeekGT" | "SeekGE") {
            return false;
        }
        let Some(key) = row[7]
            .strip_prefix("key=[")
            .and_then(|s| s.strip_suffix(']'))
        else {
            return false;
        };
        let Some((start, end)) = key.split_once("..") else {
            return false;
        };
        end.parse::<usize>().unwrap() + 1 - start.parse::<usize>().unwrap() == width
    })
}

// The same fixture is copied into an isolated frozen checkout for baseline
// capture. This switch only relaxes optimization assertions, never row results.
fn baseline() -> bool {
    std::env::var_os("AMPHORA_ROWID_BASELINE").is_some()
}

#[test]
fn duplicate_suffix_comparisons_seek_full_keys_with_page_bounded_work() {
    let f = Fixture::new();
    f.exec("CREATE TABLE events(session TEXT,seq INTEGER,at INTEGER,body TEXT)");
    f.exec("CREATE INDEX by_seq ON events(session,seq)");
    f.exec("CREATE INDEX by_time ON events(at)");
    f.exec("CREATE INDEX by_expression ON events((seq+0))");
    f.exec("WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM s WHERE n<20000) INSERT INTO events(rowid,session,seq,at,body) SELECT n,'s',7,9,'payload' FROM s");
    const PAGE: usize = 17;
    for (op, cursor, direction) in [
        ("<", 1000, "DESC"),
        ("<=", 1000, "DESC"),
        (">", 19000, "ASC"),
        (">=", 19000, "ASC"),
        ("=", 1000, "DESC"),
    ] {
        for (index, prefix, key, width) in [
            ("by_seq", "session='s' AND seq=7", "seq", 3),
            ("by_time", "at=9", "at", 2),
            ("by_expression", "(seq+0)=7", "7", 2),
        ] {
            let query = format!("SELECT rowid,{key} FROM events INDEXED BY {index} WHERE {prefix} AND rowid{op}{cursor} ORDER BY rowid {direction} LIMIT {PAGE}");
            let plan = f.explain(&query);
            let (rows, reads, steps) = f.query(&query);
            let mut expected: Vec<i64> = (1..=20000)
                .filter(|n| match op {
                    "<" => *n < cursor,
                    "<=" => *n <= cursor,
                    ">" => *n > cursor,
                    ">=" => *n >= cursor,
                    "=" => *n == cursor,
                    _ => unreachable!(),
                })
                .collect();
            if direction == "DESC" {
                expected.reverse();
            }
            expected.truncate(PAGE);
            assert_eq!(
                rows.iter()
                    .map(|row| row[0].parse::<i64>().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            println!(
                "COUNTERS {index} {op}: rows={} reads={reads} steps={steps}",
                rows.len()
            );
            if !baseline() {
                assert!(seek_width(&plan, width), "missing full key: {plan:?}");
                assert!(
                    !plan.iter().any(|row| matches!(
                        row[1].as_str(),
                        "SorterOpen" | "OpenEphemeral" | "DeferredSeek"
                    )),
                    "{plan:?}"
                );
                assert!(reads <= (PAGE * 4 + 32) as u64, "{query}: {reads}");
                assert!(steps <= (PAGE * 32 + 256) as u64, "{query}: {steps}");
            }
        }
    }
    let query = "SELECT rowid,seq FROM events INDEXED BY by_seq WHERE session='s' AND seq=7 AND rowid<1000 ORDER BY rowid DESC LIMIT 256";
    let plan = f.explain(query);
    let (rows, reads, steps) = f.query(query);
    let expected: Vec<_> = (744..1000)
        .rev()
        .map(|id| vec![id.to_string(), "7".into()])
        .collect();
    assert_eq!(rows, expected);
    println!(
        "COUNTERS by_seq page=256: rows={} reads={reads} steps={steps}",
        rows.len()
    );
    if !baseline() {
        assert!(seek_width(&plan, 3), "{plan:?}");
        assert!(reads <= 256 * 4 + 32, "{reads}");
        assert!(steps <= 256 * 32 + 256, "{steps}");
    }
}

// UNIQUE does not make a NULL equality prefix a one-row lookup.
#[test]
fn nullable_unique_prefix_keeps_duplicate_nulls_and_seeks_rowid_suffix() {
    let f = Fixture::new();
    f.exec("CREATE TABLE nullable_keys(key INTEGER)");
    f.exec("CREATE UNIQUE INDEX nullable_key ON nullable_keys(key)");
    f.exec(
        "INSERT INTO nullable_keys(rowid,key) VALUES(-9,NULL),(1,NULL),(7,NULL),(90,NULL),(100,5)",
    );
    let query = "SELECT rowid FROM nullable_keys INDEXED BY nullable_key WHERE key IS NULL AND rowid<90 ORDER BY rowid DESC LIMIT 17";
    let reference = query.replace("INDEXED BY nullable_key", "NOT INDEXED");
    let rows = f.query(query).0;
    assert_eq!(rows, vec![vec!["7"], vec!["1"], vec!["-9"]]);
    assert_eq!(rows, f.query(&reference).0);
    let plan = f.explain(query);
    if !baseline() {
        assert!(seek_width(&plan, 2), "{plan:?}");
    }
}

#[test]
fn two_probe_pages_match_reference_across_gaps_and_extreme_rowids() {
    let f = Fixture::new();
    f.exec("CREATE TABLE events(session TEXT,seq INTEGER NOT NULL,at INTEGER NOT NULL)");
    f.exec("CREATE INDEX by_seq ON events(session,seq)");
    f.exec("CREATE INDEX by_time ON events(at)");
    let mut reference = Vec::new();
    for (key, ids) in [
        (i64::MIN, vec![i64::MIN, -900, -51]),
        (-7, vec![-50, -9, -1, 0, 1, 8, 20]),
        (90, vec![35, 99, 800]),
        (i64::MAX, vec![900, i64::MAX]),
    ] {
        for id in ids {
            f.exec(&format!(
                "INSERT INTO events(rowid,session,seq,at) VALUES({id},'s',{key},{key})"
            ));
            reference.push((key, id));
        }
    }
    f.exec("INSERT INTO events(rowid,session,seq,at) VALUES(901,'other',90,90)");
    for global in [false, true] {
        let (index, key, filter) = if global {
            ("by_time", "at", "1=1")
        } else {
            ("by_seq", "seq", "session='s'")
        };
        let expected = if global {
            let mut result = reference.clone();
            // Include the other session only in the global traversal.
            let id = f.query("SELECT rowid FROM events WHERE session='other'").0[0][0]
                .parse()
                .unwrap();
            result.push((90, id));
            result
        } else {
            reference.clone()
        };
        let mut expected = expected;
        expected.sort_unstable_by(|a, b| b.cmp(a));
        for page in [1, 4, 7] {
            let mut actual = Vec::new();
            let mut cursor = None;
            for _ in 0..=expected.len() {
                let select =
                    format!("SELECT {key},rowid FROM events INDEXED BY {index} WHERE {filter}");
                let rows = if let Some((k, r)) = cursor {
                    let mut equal = f
                        .query(&format!(
                            "{select} AND {key}={k} AND rowid<{r} ORDER BY rowid DESC LIMIT {page}"
                        ))
                        .0;
                    if equal.len() < page {
                        equal.extend(
                            f.query(&format!(
                                "{select} AND {key}<{k} ORDER BY {key} DESC,rowid DESC LIMIT {}",
                                page - equal.len()
                            ))
                            .0,
                        );
                    }
                    equal
                } else {
                    f.query(&format!("{select} AND {key}<=9223372036854775807 ORDER BY {key} DESC,rowid DESC LIMIT {page}")).0
                };
                if rows.is_empty() {
                    break;
                }
                assert!(rows.len() <= page);
                let pairs: Vec<(i64, i64)> = rows
                    .iter()
                    .map(|row| (row[0].parse().unwrap(), row[1].parse().unwrap()))
                    .collect();
                cursor = pairs.last().copied();
                actual.extend(pairs);
            }
            assert_eq!(actual, expected, "global={global}, page={page}");
        }
    }
}

#[test]
fn bound_identity_and_equality_prefix_control_suffix_eligibility() {
    let f = Fixture::new();
    f.exec("CREATE TABLE aliases(id INTEGER PRIMARY KEY,rowid INTEGER,a TEXT COLLATE NOCASE,b INTEGER)");
    f.exec("CREATE INDEX ab ON aliases(a COLLATE NOCASE DESC,b ASC)");
    f.exec("INSERT INTO aliases VALUES(1,900,'A',7),(2,10,'a',7),(3,800,'B',9),(4,0,'a',8)");
    let alias = "SELECT id FROM aliases INDEXED BY ab WHERE a='a' AND b=7 AND id<2 ORDER BY id DESC LIMIT 1";
    assert_eq!(f.query(alias).0, vec![vec!["1"]]);
    let plan = f.explain(alias);
    if !baseline() {
        assert!(seek_width(&plan, 3), "{plan:?}");
    }
    // A real column named rowid is payload, not the locator suffix.
    let shadow = "SELECT id FROM aliases INDEXED BY ab WHERE a='a' AND b=7 AND rowid<20 ORDER BY id DESC LIMIT 1";
    assert_eq!(f.query(shadow).0, vec![vec!["2"]]);
    assert!(!seek_width(&f.explain(shadow), 3));
    // No prefix, a gap, and a range before the suffix cannot form a full key.
    for predicate in [
        "id<3",
        "a='a' AND id<3",
        "a='a' AND b>=7 AND id<3",
        "a COLLATE BINARY='a' AND b=7 AND id<3",
    ] {
        let query = format!(
            "SELECT id FROM aliases INDEXED BY ab WHERE {predicate} ORDER BY id DESC LIMIT 2"
        );
        assert!(!seek_width(&f.explain(&query), 3), "{query}");
        let reference = query.replace("INDEXED BY ab", "NOT INDEXED");
        assert_eq!(f.query(&query).0, f.query(&reference).0);
    }
    // Mixed declared ordering and NOCASE equality do not alter suffix ASC.
    let query = "SELECT id FROM aliases INDEXED BY ab WHERE a='a' ORDER BY b ASC,id ASC";
    assert_eq!(f.query(query).0, vec![vec!["1"], vec!["2"], vec!["4"]]);
    f.exec("CREATE TABLE wr(a TEXT,b INTEGER,PRIMARY KEY(a,b)) WITHOUT ROWID");
    f.exec("CREATE INDEX wr_b ON wr(b)");
    assert!(f
        .connection
        .prepare("SELECT rowid FROM wr INDEXED BY wr_b WHERE b=7 AND rowid<2")
        .is_err());
}
