//! AC-T1.1.1, AC-T1.8.6: composite-key mutations preserve SQLite's
//! per-statement conflict policy when statement journals are enabled. Product
//! sessions deliberately poison their enclosing transaction instead of keeping
//! statement journals; their separate public-API tests cover that contract.
use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, LimboError, MemoryIO, OpenOptions,
    SqliteDialect, StepResult, IO,
};
use amphora_engine_parser::ast::ResolveType;
use std::sync::Arc;

fn sql(
    io: &Arc<MemoryIO>,
    connection: &Arc<Connection>,
    sql: &str,
) -> Result<Vec<Vec<String>>, LimboError> {
    let mut statement = connection.prepare(sql)?;
    // The product resets checked-out statements before binding, including new
    // statements. Exercise that same lifecycle independently of its no-journal policy.
    statement.reset()?;
    let mut rows = Vec::new();
    loop {
        match statement.step()? {
            StepResult::Row => rows.push(
                statement
                    .row()
                    .unwrap()
                    .get_values()
                    .map(ToString::to_string)
                    .collect(),
            ),
            StepResult::Done => return Ok(rows),
            StepResult::IO | StepResult::Yield => io.step()?,
            other => panic!("unexpected in-memory step: {other:?}"),
        }
    }
}

#[test]
fn upsert_update_aborts_without_changing_outer_fail_policy() {
    let io = Arc::new(MemoryIO::new());
    let database = Database::open(
        io.clone(),
        "without_rowid_dml.db",
        OpenOptions::new(Arc::new(SqliteDialect))
            .db_opts(DatabaseOpts::new().with_without_rowid(true)),
    )
    .unwrap();
    let connection = database.connect(ConnectOpts::new()).unwrap();
    for statement in [
        "CREATE TABLE t(k TEXT PRIMARY KEY,u TEXT UNIQUE,n TEXT NOT NULL CHECK(length(n)>0)) WITHOUT ROWID",
        "INSERT INTO t VALUES('conflict','u1','kept'),('other','u2','other')",
        "CREATE TABLE marker(value INTEGER)",
        "INSERT INTO marker VALUES(0)",
        "BEGIN IMMEDIATE",
    ] {
        sql(&io, &connection, statement).unwrap();
    }

    for assignment in ["n=NULL", "u='u2'", "n=''"] {
        let statement = format!(
            "INSERT OR FAIL INTO t VALUES('fresh','uf','new'),('conflict','u3','new') ON CONFLICT(k) DO UPDATE SET {assignment}"
        );
        let error = sql(&io, &connection, &statement).unwrap_err();
        assert!(
            matches!(error, LimboError::Raise(ResolveType::Abort, _)),
            "{assignment}: {error:?}"
        );
        assert_eq!(
            sql(&io, &connection, "SELECT k,u,n FROM t ORDER BY k").unwrap(),
            vec![vec!["conflict", "u1", "kept"], vec!["other", "u2", "other"]]
        );
        sql(&io, &connection, "UPDATE marker SET value=value+1").unwrap();
    }
    // A conflict outside DO UPDATE retains the outer FAIL policy and its
    // earlier candidate. Setting the whole program to ABORT would lose this row.
    let error = sql(&io, &connection, "INSERT OR FAIL INTO t VALUES('retained','ur','new'),('collision','u2','new') ON CONFLICT(k) DO UPDATE SET n=NULL").unwrap_err();
    assert!(
        matches!(
            error,
            LimboError::Constraint(_) | LimboError::Raise(ResolveType::Fail, _)
        ),
        "{error:?}"
    );
    sql(&io, &connection, "COMMIT").unwrap();
    assert_eq!(
        sql(&io, &connection, "SELECT k FROM t ORDER BY k").unwrap(),
        vec![vec!["conflict"], vec!["other"], vec!["retained"]]
    );
    assert_eq!(
        sql(&io, &connection, "SELECT value FROM marker").unwrap(),
        vec![vec!["3"]]
    );
}

#[test]
fn replace_refuses_unsupported_delete_trigger_and_generated_image_before_writing() {
    let io = Arc::new(MemoryIO::new());
    let database = Database::open(
        io.clone(),
        "without_rowid_replace_refusal.db",
        OpenOptions::new(Arc::new(SqliteDialect)).db_opts(
            DatabaseOpts::new()
                .with_without_rowid(true)
                .with_generated_columns(true),
        ),
    )
    .unwrap();
    let connection = database.connect(ConnectOpts::new()).unwrap();
    for statement in [
        "CREATE TABLE triggered(k TEXT PRIMARY KEY,v INTEGER) WITHOUT ROWID",
        "CREATE TABLE audit(value INTEGER)",
        "INSERT INTO triggered VALUES('a',1)",
        "CREATE TRIGGER deleted AFTER DELETE ON triggered BEGIN INSERT INTO audit VALUES(1); END",
        "CREATE TABLE generated(k TEXT PRIMARY KEY,v INTEGER,computed INTEGER GENERATED ALWAYS AS(v+1) VIRTUAL) WITHOUT ROWID",
    ] {
        sql(&io, &connection, statement).unwrap();
    }
    for table in ["triggered", "generated"] {
        let error = sql(
            &io,
            &connection,
            &format!("INSERT OR REPLACE INTO {table}(k,v) VALUES('a',2)"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("WITHOUT ROWID"), "{error:?}");
        assert_eq!(
            sql(&io, &connection, &format!("SELECT k,v FROM {table}")).unwrap(),
            if table == "triggered" {
                vec![vec!["a", "1"]]
            } else {
                Vec::<Vec<&str>>::new()
            }
        );
    }
    assert!(sql(&io, &connection, "SELECT * FROM audit")
        .unwrap()
        .is_empty());
}

// AC-T1.1.1: work ledger lookups must seek their existing primary root,
// independently of the number of unrelated weak keys. Counters, not timing,
// distinguish bounded access from a scan of this 20,000-row fixture.
#[test]
fn native_primary_root_bounds_work_ledger_point_and_prefix_reads() {
    use amphora_engine::{StatementStatusCounter as Counter, Value};
    use std::num::NonZeroUsize;
    let io = Arc::new(MemoryIO::new());
    let db = Database::open(
        io.clone(),
        "native_pk.db",
        OpenOptions::new(Arc::new(SqliteDialect))
            .db_opts(DatabaseOpts::new().with_without_rowid(true)),
    )
    .unwrap();
    let c = db.connect(ConnectOpts::new()).unwrap();
    sql(&io,&c,"CREATE TABLE work_records(family TEXT NOT NULL,key TEXT NOT NULL,value TEXT,at INTEGER NOT NULL,PRIMARY KEY(family,key)) WITHOUT ROWID").unwrap();
    sql(&io,&c,"WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM s WHERE n<19999) INSERT INTO work_records SELECT 'weak',printf('%05d|%d',n/4,n%4),'v'||n,n%4 FROM s").unwrap();
    let point = "SELECT value FROM work_records WHERE family=?1 AND key=?2";
    let mut statement = c.prepare(point).unwrap();
    for (key, expected) in [
        ("04000|2", Some("v16002")),
        ("04000|9", None),
        ("00000|0", Some("v0")),
    ] {
        statement.reset().unwrap();
        for counter in [Counter::FullscanStep, Counter::RowsRead, Counter::VmStep] {
            statement.reset_stmt_status(counter);
        }
        statement
            .bind_at(NonZeroUsize::new(1).unwrap(), Value::build_text("weak"))
            .unwrap();
        statement
            .bind_at(NonZeroUsize::new(2).unwrap(), Value::build_text(key))
            .unwrap();
        let rows = statement.run_collect_rows().unwrap();
        assert_eq!(rows.len(), usize::from(expected.is_some()));
        if let Some(value) = expected {
            assert_eq!(rows[0][0].to_string(), value);
        }
        assert_eq!(statement.stmt_status(Counter::FullscanStep), 0);
        assert!(statement.stmt_status(Counter::RowsRead) <= 256);
        assert!(statement.stmt_status(Counter::VmStep) < 2000);
    }
    let point_plan = sql(&io, &c, &format!("EXPLAIN QUERY PLAN {point}")).unwrap();
    assert!(
        point_plan
            .iter()
            .any(|r| r[3].contains("USING PRIMARY KEY (family=? AND key=?)")),
        "{point_plan:?}"
    );
    let range = "SELECT key,value,at FROM work_records WHERE family='weak' AND key>='04000|' AND key<'04000|z' ORDER BY at DESC,key";
    let mut statement = c.prepare(range).unwrap();
    let rows = statement.run_collect_rows().unwrap();
    let values: Vec<_> = rows.iter().map(|r| r[1].to_string()).collect();
    assert_eq!(values, vec!["v16003", "v16002", "v16001", "v16000"]);
    assert_eq!(statement.stmt_status(Counter::FullscanStep), 0);
    assert!(statement.stmt_status(Counter::RowsRead) <= 256);
    assert!(statement.stmt_status(Counter::VmStep) < 2000);
    let plan = sql(&io, &c, &format!("EXPLAIN QUERY PLAN {range}")).unwrap();
    assert!(
        plan.iter()
            .any(|r| r[3].contains("USING PRIMARY KEY (family=? AND key>=? AND key<?)")),
        "{plan:?}"
    );
    assert!(
        plan.iter().any(|r| r[3].contains("ORDER BY")),
        "payload order still needs sorting: {plan:?}"
    );
    let ordered = "SELECT key FROM work_records WHERE family='weak' AND key>='04000|' AND key<'04000|z' ORDER BY key DESC";
    let plan = sql(&io, &c, &format!("EXPLAIN QUERY PLAN {ordered}")).unwrap();
    assert!(!plan.iter().any(|r| r[3].contains("ORDER BY")), "{plan:?}");
    assert_eq!(
        sql(&io, &c, ordered).unwrap(),
        vec![
            vec!["04000|3"],
            vec!["04000|2"],
            vec!["04000|1"],
            vec!["04000|0"]
        ]
    );
    let bytecode = sql(&io, &c, &format!("EXPLAIN {point}")).unwrap();
    assert!(
        !bytecode
            .iter()
            .any(|r| r[1] == "IdxRowId" || r[1] == "DeferredSeek"),
        "{bytecode:?}"
    );
    // AC-T1.13.4: the same native primary root must seek a composite keyset cursor,
    // including its non-key payload, rather than reread the preceding 16,000 rows.
    // Reuse the point/prefix work bounds; both directions must return the exact next page.
    for (op, key, direction, expected) in [
        (
            ">",
            "04000|1",
            "ASC",
            (16002..16010).map(|n| format!("v{n}")).collect::<Vec<_>>(),
        ),
        (
            "<",
            "04000|2",
            "DESC",
            (15994..16002)
                .rev()
                .map(|n| format!("v{n}"))
                .collect::<Vec<_>>(),
        ),
    ] {
        let query = format!("SELECT value FROM work_records WHERE (family,key) {op} ('weak','{key}') ORDER BY family {direction},key {direction} LIMIT 8");
        let mut statement = c.prepare(&query).unwrap();
        let got: Vec<_> = statement
            .run_collect_rows()
            .unwrap()
            .iter()
            .map(|row| row[0].to_string())
            .collect();
        assert_eq!(got, expected, "{query}");
        assert_eq!(statement.stmt_status(Counter::FullscanStep), 0, "{query}");
        assert!(statement.stmt_status(Counter::RowsRead) <= 256, "{query}");
        assert!(statement.stmt_status(Counter::VmStep) < 2000, "{query}");
        let plan = sql(&io, &c, &format!("EXPLAIN QUERY PLAN {query}")).unwrap();
        assert!(
            plan.iter()
                .any(|row| row[3].contains(&format!("USING PRIMARY KEY ((family,key){op}(?,?))"))),
            "{query}: {plan:?}"
        );
    }
}

// AC-T1.1.1: adding a native query candidate must preserve SQL results in
// alternate join/IN/OR paths, including reordered physical primary keys.
#[test]
fn native_primary_root_preserves_collation_payload_and_join_fallbacks() {
    let io = Arc::new(MemoryIO::new());
    let db = Database::open(
        io.clone(),
        "native_pk_compat.db",
        OpenOptions::new(Arc::new(SqliteDialect))
            .db_opts(DatabaseOpts::new().with_without_rowid(true)),
    )
    .unwrap();
    let c = db.connect(ConnectOpts::new()).unwrap();
    for q in [
        "CREATE TABLE t(value TEXT,b INTEGER,a TEXT COLLATE NOCASE,PRIMARY KEY(a DESC,b ASC)) WITHOUT ROWID",
        "INSERT INTO t VALUES('A1',1,'Alpha'),('A2',2,'alpha'),('B1',1,'Beta'),('G3',3,'Gamma')",
        "CREATE INDEX payload ON t(value)",
        "CREATE TABLE l(id INTEGER PRIMARY KEY,a TEXT,b INTEGER)",
        "INSERT INTO l VALUES(1,'ALPHA',1),(9,'missing',1)",
        "CREATE TABLE override(value TEXT,b INTEGER,a TEXT,PRIMARY KEY(a COLLATE NOCASE DESC,b)) WITHOUT ROWID",
        "INSERT INTO override VALUES('kept',1,'Alpha')",
    ] {
        sql(&io, &c, q).unwrap();
    }
    assert_eq!(
        sql(&io, &c, "SELECT value FROM t WHERE a='ALPHA' AND b=2").unwrap(),
        vec![vec!["A2"]]
    );
    assert_eq!(
        sql(&io, &c, "SELECT value FROM t WHERE a IS NULL OR b=NULL").unwrap(),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(
        sql(&io, &c, "SELECT value FROM t WHERE a='ALPHA' AND b='2'").unwrap(),
        vec![vec!["A2"]]
    );
    assert_eq!(
        sql(
            &io,
            &c,
            "SELECT value FROM override WHERE a COLLATE NOCASE='ALPHA' AND b=1"
        )
        .unwrap(),
        vec![vec!["kept"]]
    );
    assert!(sql(
        &io,
        &c,
        "SELECT value FROM override WHERE a='ALPHA' AND b=1"
    )
    .unwrap()
    .is_empty());
    assert_eq!(
        sql(
            &io,
            &c,
            "SELECT value FROM t WHERE a>='alpha' AND a<'gamma' ORDER BY a DESC,b"
        )
        .unwrap(),
        vec![vec!["B1"], vec!["A1"], vec!["A2"]]
    );
    for join in ["RIGHT", "FULL"] {
        assert_eq!(sql(&io,&c,&format!("SELECT coalesce(l.id,-1),t.value FROM l {join} JOIN t ON l.a=t.a COLLATE NOCASE AND l.b=t.b WHERE t.a='alpha' ORDER BY t.b")).unwrap(),vec![vec!["1","A1"],vec!["-1","A2"]]);
    }
    for predicate in [
        "a IN ('ALPHA','Beta')",
        "a='ALPHA' OR a='Beta'",
        "EXISTS(SELECT 1 FROM l WHERE l.a=t.a COLLATE NOCASE AND l.b=t.b) OR a='Beta'",
    ] {
        let expected = if predicate.starts_with("EXISTS") {
            vec![vec!["A1"], vec!["B1"]]
        } else {
            vec![vec!["A1"], vec!["A2"], vec!["B1"]]
        };
        assert_eq!(
            sql(
                &io,
                &c,
                &format!("SELECT value FROM t WHERE {predicate} ORDER BY value")
            )
            .unwrap(),
            expected
        );
    }
    assert_eq!(
        sql(
            &io,
            &c,
            "SELECT a,b FROM t INDEXED BY payload WHERE value='A2'"
        )
        .unwrap(),
        vec![vec!["alpha", "2"]]
    );
}
