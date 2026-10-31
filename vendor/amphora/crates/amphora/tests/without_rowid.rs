//! AC-T1.1.1, AC-T1.8.6: SQLite-compatible WITHOUT ROWID behavior through
//! Amphora's durable public SQL boundary. Expectations are SQL results, not engine layout.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use amphora::database::Database;
use amphora_api::{Refusal, Statement, Value};
use std::time::{Duration, Instant};

fn until() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
fn exec(db: &Database, sql: &str) {
    let sql = sql.to_owned();
    db.call(until(), move |c| {
        c.execute_batch(&sql)?;
        Ok(())
    })
    .unwrap();
}
fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.execute(
        vec![Statement {
            sql: sql.into(),
            params: vec![],
        }],
        until(),
    )
    .unwrap()
    .remove(0)
    .rows
}
fn refused(db: &Database, sql: &str) {
    let owned_sql = sql.to_owned();
    let result: Result<(), Refusal> = db.call(until(), move |c| {
        c.execute_batch(&owned_sql)?;
        Ok(())
    });
    assert!(result.is_err(), "must refuse: {sql}");
}
fn text(s: &str) -> Value {
    Value::Text(s.into())
}
fn int(n: i64) -> Value {
    Value::Integer(n)
}

// Exact table definitions from Kiln's shipped P6/W24/W28/U03/U05 migrations.
const APP_SCHEMA: &str = r#"
CREATE TABLE jobs(id TEXT PRIMARY KEY);
CREATE TABLE records_versions(family TEXT NOT NULL,key BLOB NOT NULL,rev INTEGER NOT NULL,
 seq INTEGER NOT NULL,value BLOB,PRIMARY KEY(family,key,rev)) WITHOUT ROWID;
CREATE INDEX records_versions_feed ON records_versions(family,rev,seq);
CREATE TABLE records_families(family TEXT PRIMARY KEY,newest INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE work_records(family TEXT NOT NULL,key TEXT NOT NULL,value BLOB NOT NULL,
 at INTEGER NOT NULL,PRIMARY KEY(family,key)) WITHOUT ROWID;
CREATE TABLE work_flight(id TEXT PRIMARY KEY,lease TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE ci_completion_reports(job TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
 attempt INTEGER NOT NULL,agent TEXT NOT NULL,payload TEXT NOT NULL,received_at INTEGER NOT NULL,
 PRIMARY KEY(job,attempt,agent)) WITHOUT ROWID;
CREATE TABLE runner_work_holds(owner TEXT NOT NULL,repo TEXT NOT NULL,domain TEXT NOT NULL,
 digest TEXT NOT NULL,PRIMARY KEY(owner,repo,domain,digest)) WITHOUT ROWID;
CREATE TABLE runner_work_imports(source TEXT PRIMARY KEY,identity TEXT NOT NULL,cursor BLOB NOT NULL,
 done INTEGER NOT NULL) WITHOUT ROWID;
"#;

#[test]
fn without_rowid_app_migrations_feed_and_implicit_notnull() {
    let db = Database::in_memory(until()).unwrap();
    exec(&db, APP_SCHEMA);
    exec(&db, "INSERT INTO records_versions VALUES('f',x'02',3,1,x'bb'),('f',x'01',3,0,x'aa');
        INSERT INTO records_families VALUES('f',3); INSERT INTO work_records VALUES('f','k',x'01',3);
        INSERT INTO work_flight VALUES('step','lease'); INSERT INTO jobs VALUES('job');
        INSERT INTO ci_completion_reports VALUES('job',1,'runner','payload',3);
        INSERT INTO runner_work_holds VALUES('o','r','d','hash');
        INSERT INTO runner_work_imports VALUES('old','identity',x'00',0)");
    assert_eq!(
        rows(
            &db,
            "SELECT hex(key),seq FROM records_versions INDEXED BY records_versions_feed WHERE family='f' AND rev=3 ORDER BY seq"
        ),
        vec![vec![text("01"), int(0)], vec![text("02"), int(1)]]
    );
    for sql in [
        "INSERT INTO records_families VALUES(NULL,1)",
        "INSERT INTO work_flight VALUES(NULL,'lease')",
        "INSERT INTO runner_work_imports VALUES(NULL,'id',x'00',0)",
    ] {
        refused(&db, sql);
    }
    assert_eq!(
        rows(&db, "SELECT count(*) FROM runner_work_imports"),
        vec![vec![int(1)]]
    );
}

#[test]
fn without_rowid_secondary_unique_nulls_and_collation() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE populated(k INTEGER PRIMARY KEY,u TEXT) WITHOUT ROWID;
        INSERT INTO populated VALUES(1,'Alpha'),(2,'alpha'),(3,NULL),(4,NULL)",
    );
    refused(
        &db,
        "CREATE UNIQUE INDEX populated_u ON populated(u COLLATE NOCASE)",
    );
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM sqlite_schema WHERE name='populated_u'"
        ),
        vec![vec![int(0)]]
    );
    exec(
        &db,
        "DELETE FROM populated WHERE k=2;
        CREATE UNIQUE INDEX populated_u ON populated(u COLLATE NOCASE)",
    );
    refused(&db, "INSERT INTO populated VALUES(5,'ALPHA')");
    assert_eq!(
        rows(
            &db,
            "SELECT k FROM populated INDEXED BY populated_u WHERE u IS NULL ORDER BY k"
        ),
        vec![vec![int(3)], vec![int(4)]]
    );
    exec(
        &db,
        "CREATE TABLE t(k TEXT PRIMARY KEY,u TEXT COLLATE NOCASE UNIQUE,v INTEGER) WITHOUT ROWID;
        INSERT INTO t VALUES('a',NULL,1),('b',NULL,2),('c','Alpha',3)",
    );
    refused(&db, "INSERT INTO t VALUES('d','alpha',4)");
    assert_eq!(
        rows(&db, "SELECT k FROM t WHERE u IS NULL ORDER BY k"),
        vec![vec![text("a")], vec![text("b")]]
    );
    assert_eq!(
        rows(&db, "SELECT k,v FROM t WHERE u='ALPHA'"),
        vec![vec![text("c"), int(3)]]
    );
    exec(
        &db,
        "UPDATE t SET u='Beta' WHERE k='c'; DELETE FROM t WHERE k='a'",
    );
    exec(&db, "INSERT INTO t VALUES('d','alpha',4)");
    assert_eq!(
        rows(&db, "SELECT k FROM t WHERE u='ALPHA'"),
        vec![vec![text("d")]]
    );
}

#[test]
fn without_rowid_populated_index_composite_collation_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wr.db");
    {
        let db = Database::open(&path, until()).unwrap();
        exec(&db,"CREATE TABLE t(a TEXT,b INTEGER,v TEXT,u TEXT UNIQUE,
            PRIMARY KEY(a COLLATE NOCASE DESC,b ASC)) WITHOUT ROWID;
            INSERT INTO t VALUES('Alpha',2,'shared','u2'),('Beta',1,'shared','u1'),('Alpha',1,'other','u3');
            CREATE INDEX feed ON t(v,a COLLATE BINARY)");
        refused(&db, "INSERT INTO t VALUES('ALPHA',2,'duplicate','u4')");
        assert_eq!(
            rows(&db, "SELECT a,b FROM t ORDER BY a COLLATE NOCASE DESC,b"),
            vec![
                vec![text("Beta"), int(1)],
                vec![text("Alpha"), int(1)],
                vec![text("Alpha"), int(2)]
            ]
        );
    }
    let db = Database::open(&path, until()).unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT a,b,u FROM t INDEXED BY feed WHERE v='shared' ORDER BY a COLLATE NOCASE,b"
        ),
        vec![
            vec![text("Alpha"), int(2), text("u2")],
            vec![text("Beta"), int(1), text("u1")]
        ]
    );
    refused(&db, "INSERT INTO t VALUES('Gamma',1,'x','u1')");
    exec(&db, "REINDEX feed");
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM t INDEXED BY feed WHERE v='shared'"
        ),
        vec![vec![int(2)]]
    );
}

#[test]
fn without_rowid_dml_indexes_and_transaction_rollback() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE t(a TEXT,b INTEGER,u TEXT UNIQUE,v INTEGER,PRIMARY KEY(a,b)) WITHOUT ROWID;
        CREATE INDEX by_v ON t(v); INSERT INTO t VALUES('a',1,'first',10),('b',2,'second',20);
        UPDATE t SET a='c',b=3,v=30 WHERE a='a';
        INSERT INTO t VALUES('c',3,'first',31) ON CONFLICT(a,b) DO UPDATE SET v=excluded.v;
        INSERT OR REPLACE INTO t VALUES('d',4,'second',40)",
    );
    assert_eq!(
        rows(&db, "SELECT a,b,v FROM t ORDER BY a"),
        vec![
            vec![text("c"), int(3), int(31)],
            vec![text("d"), int(4), int(40)]
        ]
    );
    assert_eq!(
        rows(&db, "SELECT a FROM t INDEXED BY by_v WHERE v=20 OR v=30"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE u='second'"),
        vec![vec![text("d")]]
    );
    let result: Result<(), Refusal> = db.call(until(), |c| {
        c.execute("UPDATE t SET v=99 WHERE a='c'", [])?;
        c.execute("DELETE FROM t WHERE a='d'", [])?;
        anyhow::bail!("application abort")
    });
    assert!(result.is_err());
    assert_eq!(
        rows(&db, "SELECT a FROM t INDEXED BY by_v WHERE v=31"),
        vec![vec![text("c")]]
    );
    assert_eq!(
        rows(&db, "SELECT a FROM t WHERE u='second'"),
        vec![vec![text("d")]]
    );
    exec(&db, "DELETE FROM t WHERE a='c'");
    assert!(rows(&db, "SELECT a FROM t INDEXED BY by_v WHERE v=31").is_empty());
    exec(&db, "INSERT INTO t VALUES('e',5,'first',50)");
}

#[test]
fn without_rowid_reordered_child_cascade_and_no_action() {
    let db = Database::in_memory(until()).unwrap();
    exec(&db,"CREATE TABLE jobs(id TEXT PRIMARY KEY); INSERT INTO jobs VALUES('cascade'),('keep');
        CREATE TABLE child(payload TEXT,job TEXT REFERENCES jobs(id) ON DELETE CASCADE,attempt INTEGER,
          PRIMARY KEY(job,attempt)) WITHOUT ROWID;
        CREATE TABLE guarded(payload TEXT,job TEXT REFERENCES jobs(id),attempt INTEGER,
          PRIMARY KEY(job,attempt)) WITHOUT ROWID;
        INSERT INTO child VALUES('one','cascade',1),('two','cascade',2);
        INSERT INTO guarded VALUES('guard','keep',1)");
    exec(&db, "DELETE FROM jobs WHERE id='cascade'");
    assert_eq!(rows(&db, "SELECT count(*) FROM child"), vec![vec![int(0)]]);
    refused(&db, "DELETE FROM jobs WHERE id='keep'");
    assert_eq!(
        rows(&db, "SELECT payload,job,attempt FROM guarded"),
        vec![vec![text("guard"), text("keep"), int(1)]]
    );
    assert_eq!(rows(&db, "SELECT id FROM jobs"), vec![vec![text("keep")]]);
    refused(&db, "INSERT INTO child VALUES('orphan','missing',3)");
}

#[test]
fn without_rowid_parent_composite_foreign_key_mapping() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE parent(payload TEXT,a TEXT COLLATE NOCASE,b INTEGER,
        PRIMARY KEY(a,b)) WITHOUT ROWID;
        CREATE TABLE child(payload TEXT,x TEXT,y INTEGER,PRIMARY KEY(x,y),
        FOREIGN KEY(x,y) REFERENCES parent(a,b) ON DELETE CASCADE) WITHOUT ROWID;
        INSERT INTO parent VALUES('parent','Alpha',7);
        INSERT INTO child VALUES('child','ALPHA',7)",
    );
    refused(&db, "INSERT INTO child VALUES('bad','other',7)");
    assert_eq!(
        rows(&db, "SELECT payload FROM child"),
        vec![vec![text("child")]]
    );
    exec(&db, "DELETE FROM parent WHERE a='alpha' AND b=7");
    assert_eq!(rows(&db, "SELECT count(*) FROM child"), vec![vec![int(0)]]);
}

#[test]
fn without_rowid_self_reference_primary_key_mutation() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE t(id TEXT PRIMARY KEY,parent TEXT REFERENCES t(id)) WITHOUT ROWID;
        INSERT INTO t VALUES('a','a'); UPDATE t SET id='b',parent='b' WHERE id='a'",
    );
    assert_eq!(
        rows(&db, "SELECT id,parent FROM t"),
        vec![vec![text("b"), text("b")]]
    );
    refused(&db, "UPDATE t SET id='c' WHERE id='b'");
    assert_eq!(
        rows(&db, "SELECT id,parent FROM t"),
        vec![vec![text("b"), text("b")]]
    );
    exec(
        &db,
        "DELETE FROM t WHERE id='b'; INSERT INTO t VALUES('d','d'); DROP TABLE t",
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM sqlite_schema WHERE name='t'"),
        vec![vec![int(0)]]
    );
}

#[test]
fn without_rowid_parent_drop_cascade_and_no_action() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE p(payload TEXT,id TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TABLE c(payload TEXT,parent TEXT REFERENCES p(id) ON DELETE CASCADE,
          n INTEGER,PRIMARY KEY(parent,n)) WITHOUT ROWID;
        INSERT INTO p VALUES('parent','a'); INSERT INTO c VALUES('child','a',1);
        DROP TABLE p",
    );
    assert_eq!(rows(&db, "SELECT count(*) FROM c"), vec![vec![int(0)]]);
    exec(
        &db,
        "CREATE TABLE guarded_parent(payload TEXT,id TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TABLE guard(payload TEXT,parent TEXT REFERENCES guarded_parent(id),
          n INTEGER,PRIMARY KEY(parent,n)) WITHOUT ROWID;
        INSERT INTO guarded_parent VALUES('parent','b'); INSERT INTO guard VALUES('child','b',1)",
    );
    refused(&db, "DROP TABLE guarded_parent");
    assert_eq!(
        rows(&db, "SELECT payload,id FROM guarded_parent"),
        vec![vec![text("parent"), text("b")]]
    );
    assert_eq!(
        rows(&db, "SELECT payload,parent,n FROM guard"),
        vec![vec![text("child"), text("b"), int(1)]]
    );
}

#[test]
fn without_rowid_primary_conflict_defaults_and_statement_overrides() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE ignored(a TEXT,b INTEGER,v TEXT,
        PRIMARY KEY(a,b) ON CONFLICT IGNORE) WITHOUT ROWID;
        INSERT INTO ignored VALUES('a',1,'original');
        INSERT INTO ignored VALUES('a',1,'ignored')",
    );
    assert_eq!(
        rows(&db, "SELECT v FROM ignored"),
        vec![vec![text("original")]]
    );
    refused(&db, "INSERT OR ABORT INTO ignored VALUES('a',1,'abort')");
    exec(
        &db,
        "INSERT OR REPLACE INTO ignored VALUES('a',1,'replaced')",
    );
    assert_eq!(
        rows(&db, "SELECT v FROM ignored"),
        vec![vec![text("replaced")]]
    );
    exec(
        &db,
        "CREATE TABLE replaced(a TEXT,b INTEGER,v TEXT,
        PRIMARY KEY(a,b) ON CONFLICT REPLACE) WITHOUT ROWID;
        INSERT INTO replaced VALUES('a',1,'original');
        INSERT INTO replaced VALUES('a',1,'default replacement');
        INSERT OR IGNORE INTO replaced VALUES('a',1,'ignored')",
    );
    assert_eq!(
        rows(&db, "SELECT v FROM replaced"),
        vec![vec![text("default replacement")]]
    );
    refused(&db, "INSERT OR ABORT INTO replaced VALUES('a',1,'abort')");
    assert_eq!(
        rows(&db, "SELECT v FROM replaced"),
        vec![vec![text("default replacement")]]
    );
}

#[test]
fn without_rowid_ignore_conflict_preserves_earlier_replace_candidate() {
    let db = Database::in_memory(until()).unwrap();
    // SQLite's result: the IGNORE prevents the whole insertion, including
    // deleting a row that conflicted with the preceding REPLACE constraint.
    exec(
        &db,
        "CREATE TABLE mixed(k TEXT PRIMARY KEY,
        a TEXT UNIQUE ON CONFLICT REPLACE,b TEXT UNIQUE ON CONFLICT IGNORE) WITHOUT ROWID;
        INSERT INTO mixed VALUES('1','a','x'),('2','y','b');
        INSERT INTO mixed VALUES('3','a','b')",
    );
    let expected = vec![
        vec![text("1"), text("a"), text("x")],
        vec![text("2"), text("y"), text("b")],
    ];
    assert_eq!(rows(&db, "SELECT k,a,b FROM mixed ORDER BY k"), expected);
    exec(&db, "UPDATE mixed SET a='a',b='b' WHERE k='2'");
    assert_eq!(
        rows(&db, "SELECT k,a,b FROM mixed ORDER BY k"),
        vec![vec![text("2"), text("a"), text("b")]]
    );
}

#[test]
fn without_rowid_notnull_replace_default_and_multirow_abort() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE nn(id TEXT PRIMARY KEY,
        v TEXT NOT NULL ON CONFLICT REPLACE DEFAULT 'fallback') WITHOUT ROWID;
        INSERT INTO nn VALUES('a','first'),('b','second');
        UPDATE nn SET v=NULL WHERE id='a'",
    );
    let expected = vec![
        vec![text("a"), text("fallback")],
        vec![text("b"), text("second")],
    ];
    assert_eq!(rows(&db, "SELECT id,v FROM nn ORDER BY id"), expected);
    refused(
        &db,
        "UPDATE OR ABORT nn SET v=CASE WHEN id='b' THEN NULL ELSE 'changed' END",
    );
    assert_eq!(rows(&db, "SELECT id,v FROM nn ORDER BY id"), expected);
}

#[test]
fn without_rowid_partial_expression_index_mutations_and_reindex() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE expression_rows(id TEXT PRIMARY KEY,label TEXT,active INTEGER) WITHOUT ROWID;
        INSERT INTO expression_rows VALUES('a','Alpha',1),('b','Beta',0),('c','Gamma',1);
        CREATE INDEX active_lower ON expression_rows(lower(label)) WHERE active=1",
    );
    assert_eq!(
        rows(
            &db,
            "SELECT id FROM expression_rows INDEXED BY active_lower
        WHERE active=1 AND lower(label)='alpha'"
        ),
        vec![vec![text("a")]]
    );
    exec(
        &db,
        "UPDATE expression_rows SET label='Delta' WHERE id='a';
        UPDATE expression_rows SET active=1 WHERE id='b';
        UPDATE expression_rows SET active=0 WHERE id='c';
        INSERT INTO expression_rows VALUES('d','Echo',1);
        DELETE FROM expression_rows WHERE id='d'; REINDEX active_lower",
    );
    assert!(
        rows(
            &db,
            "SELECT id FROM expression_rows INDEXED BY active_lower
        WHERE active=1 AND lower(label) IN ('alpha','gamma','echo')"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &db,
            "SELECT id,lower(label) FROM expression_rows INDEXED BY active_lower
        WHERE active=1 ORDER BY lower(label)"
        ),
        vec![
            vec![text("b"), text("beta")],
            vec![text("a"), text("delta")]
        ]
    );
}

#[test]
fn without_rowid_correlated_set_observes_prior_updates_and_original_row_assignments() {
    let db = Database::in_memory(until()).unwrap();
    // SQLite oracle: row 3 sees row 2's updated 30, so its sum is 70.
    // Computing all SET expressions before updating any row incorrectly yields 60.
    exec(
        &db,
        "CREATE TABLE t(id INTEGER PRIMARY KEY,v INTEGER) WITHOUT ROWID;
        INSERT INTO t VALUES(1,10),(2,20),(3,30);
        UPDATE t SET v=(SELECT sum(p.v) FROM t AS p WHERE p.id<=t.id)",
    );
    assert_eq!(
        rows(&db, "SELECT id,v FROM t ORDER BY id"),
        vec![
            vec![int(1), int(10)],
            vec![int(2), int(30)],
            vec![int(3), int(70)]
        ]
    );
    // Within each row every right-hand side uses that row's old values.
    exec(
        &db,
        "CREATE TABLE simultaneous(id INTEGER PRIMARY KEY,a INTEGER,b INTEGER) WITHOUT ROWID;
        INSERT INTO simultaneous VALUES(1,10,100),(2,20,200);
        UPDATE simultaneous SET a=b,b=a",
    );
    assert_eq!(
        rows(&db, "SELECT id,a,b FROM simultaneous ORDER BY id"),
        vec![
            vec![int(1), int(100), int(10)],
            vec![int(2), int(200), int(20)]
        ]
    );
}

#[test]
fn without_rowid_update_delete_returning_metadata_and_values() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE returned(id INTEGER PRIMARY KEY,v TEXT) WITHOUT ROWID;
        INSERT INTO returned VALUES(1,'one'),(2,'two'),(3,'three')",
    );
    let updated = db
        .call(until(), |c| {
            let mut statement = c.prepare(
                "UPDATE returned SET v=v || '-new' WHERE id<=2
            RETURNING id AS changed_id,v AS new_value",
            )?;
            assert_eq!(statement.column_count(), 2);
            let result = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0usize)?, row.get::<_, String>(1usize)?))
                })?
                .collect::<amphora::sql::Result<Vec<_>>>()?;
            Ok(result)
        })
        .unwrap();
    assert_eq!(updated, vec![(1, "one-new".into()), (2, "two-new".into())]);
    let deleted = db
        .call(until(), |c| {
            let mut statement = c.prepare(
                "DELETE FROM returned WHERE id<=2
            RETURNING id AS deleted_id,v AS old_value",
            )?;
            assert_eq!(statement.column_count(), 2);
            let result = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0usize)?, row.get::<_, String>(1usize)?))
                })?
                .collect::<amphora::sql::Result<Vec<_>>>()?;
            Ok(result)
        })
        .unwrap();
    assert_eq!(deleted, updated);
    assert_eq!(
        rows(&db, "SELECT id,v FROM returned"),
        vec![vec![int(3), text("three")]]
    );
}

#[test]
fn without_rowid_returning_foreign_key_failure_never_acknowledges_results() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE parents(id TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TABLE children(payload TEXT,parent TEXT REFERENCES parents(id),
            PRIMARY KEY(parent)) WITHOUT ROWID;
        CREATE TABLE marker(value INTEGER); INSERT INTO marker VALUES(0);
        INSERT INTO parents VALUES('kept'); INSERT INTO children VALUES('child','kept')",
    );
    let result = db.execute(
        vec![
            Statement {
                sql: "UPDATE marker SET value=1".into(),
                params: vec![],
            },
            Statement {
                sql: "DELETE FROM parents RETURNING id".into(),
                params: vec![],
            },
        ],
        until(),
    );
    assert!(
        result.is_err(),
        "FK failure must return an error instead of RETURNING rows"
    );
    assert_eq!(rows(&db, "SELECT value FROM marker"), vec![vec![int(0)]]);
    assert_eq!(
        rows(&db, "SELECT id FROM parents"),
        vec![vec![text("kept")]]
    );
    assert_eq!(
        rows(&db, "SELECT payload,parent FROM children"),
        vec![vec![text("child"), text("kept")]]
    );
}

#[test]
fn without_rowid_upsert_update_aborts_statement_despite_outer_fail_policy() {
    let db = Database::in_memory(until()).unwrap();
    exec(
        &db,
        "CREATE TABLE upsert_abort(k TEXT PRIMARY KEY,u TEXT UNIQUE,n TEXT NOT NULL) WITHOUT ROWID;
        INSERT INTO upsert_abort VALUES('conflict','u1','kept'),('other','u2','other');
        CREATE TABLE survived(value INTEGER); INSERT INTO survived VALUES(0)",
    );
    // Product calls disable per-statement journaling: a caught SQL error
    // poisons the enclosing call, whose earlier writes must not become durable.
    for sql in [
        "INSERT OR FAIL INTO upsert_abort VALUES('fresh','uf','new'),('conflict','u3','new')
         ON CONFLICT(k) DO UPDATE SET n=NULL",
        "INSERT OR FAIL INTO upsert_abort VALUES('fresh','uf','new'),('conflict','u3','new')
         ON CONFLICT(k) DO UPDATE SET u='u2'",
    ] {
        let result: Result<(), Refusal> = db.call(until(), move |c| {
            c.execute("UPDATE survived SET value=value+1", [])?;
            assert!(c.execute(sql, []).is_err());
            Ok(())
        });
        assert!(
            result.is_err(),
            "a caught SQL error must refuse the enclosing commit"
        );
        assert_eq!(
            rows(&db, "SELECT count(*) FROM upsert_abort WHERE k='fresh'"),
            vec![vec![int(0)]]
        );
        assert_eq!(rows(&db, "SELECT value FROM survived"), vec![vec![int(0)]]);
    }
    assert_eq!(
        rows(&db, "SELECT k,u,n FROM upsert_abort ORDER BY k"),
        vec![
            vec![text("conflict"), text("u1"), text("kept")],
            vec![text("other"), text("u2"), text("other")]
        ]
    );
    assert_eq!(rows(&db, "SELECT value FROM survived"), vec![vec![int(0)]]);
}

#[test]
fn native_primary_root_payload_and_mutations_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native-pk.db");
    {
        let db = Database::open(&path, until()).unwrap();
        exec(&db,"CREATE TABLE native_payload(value TEXT,b INTEGER,a TEXT COLLATE NOCASE,PRIMARY KEY(a DESC,b ASC)) WITHOUT ROWID;
            INSERT INTO native_payload VALUES('one',1,'Alpha'),('two',2,'alpha'),('three',1,'Beta');
            CREATE INDEX native_value ON native_payload(value);
            INSERT INTO native_payload VALUES('replace',1,'ALPHA') ON CONFLICT(a,b) DO UPDATE SET value=excluded.value;
            UPDATE native_payload SET value='updated' WHERE a='ALPHA' AND b=2;
            DELETE FROM native_payload WHERE a='BETA' AND b=1");
        refused(&db, "INSERT INTO native_payload VALUES('bad',1,NULL)");
    }
    let db = Database::open(&path, until()).unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT value FROM native_payload WHERE a='alpha' AND b=1"
        ),
        vec![vec![text("replace")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT value FROM native_payload WHERE a='alpha' AND b>=1 AND b<3 ORDER BY b DESC"
        ),
        vec![vec![text("updated")], vec![text("replace")]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT a,b FROM native_payload INDEXED BY native_value WHERE value='updated'"
        ),
        vec![vec![text("alpha"), int(2)]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM native_payload"),
        vec![vec![int(2)]]
    );
}
