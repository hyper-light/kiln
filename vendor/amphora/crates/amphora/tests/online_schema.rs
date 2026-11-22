//! Online schema changes by F1's states (CP-T1.12, T-T1.12.1; docs/design/online-schema.md): an
//! index built while writes go on is public only once whole, reads never use it before, and rows
//! that break a unique one undo the change.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora::database::online::{self, Plan, Stepped};
use amphora::db::Db;
use amphora_api::{Migration, Refusal};
use amphora_engine::Value;

fn plan(sql: &str) -> Plan {
    Plan::parse(&Migration {
        name: "ix".into(),
        sql: sql.into(),
    })
    .unwrap()
}

fn count(db: &Db, sql: &str, params: Vec<Value>) -> anyhow::Result<i64> {
    let mut n = 0;
    db.query_owned(sql, params, |r| {
        n = r.get::<i64>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })?;
    Ok(n)
}

fn plan_text(db: &Db, sql: &str) -> String {
    let mut out = String::new();
    db.query_owned(&format!("EXPLAIN QUERY PLAN {sql}"), [], |r| {
        out.push_str(&r.get::<String>(3).map_err(|e| anyhow::anyhow!("{e}"))?);
        out.push('\n');
        Ok(true)
    })
    .unwrap();
    out
}

fn integrity(db: &Db) -> String {
    let mut check = String::new();
    db.query("PRAGMA integrity_check", [], |r| {
        check = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    })
    .unwrap();
    check
}

fn table(rows: i64) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("t.db")).unwrap();
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT); CREATE TABLE __amphora_migrations (name TEXT PRIMARY KEY, sql TEXT NOT NULL)")
        .unwrap();
    db.transaction(|db| {
        for i in 0..rows {
            db.query(
                "INSERT INTO t (a, b) VALUES (?1, ?2)",
                [Value::from_i64(i % 97), Value::from_text(format!("r{i}"))],
                |_| Ok(true),
            )?;
        }
        Ok(())
    })
    .unwrap();
    (dir, db)
}

fn write(db: &mut Db, i: i64) {
    db.transaction(|db| {
        db.query(
            "INSERT INTO t (a, b) VALUES (?1, 'w')",
            [Value::from_i64(i % 97)],
            |_| Ok(true),
        )?;
        db.query(
            "UPDATE t SET a = ?1 WHERE id = ?2",
            [Value::from_i64((i * 7) % 97), Value::from_i64(i * 13 + 1)],
            |_| Ok(true),
        )?;
        db.query(
            "DELETE FROM t WHERE id = ?1",
            [Value::from_i64(i * 17 + 2)],
            |_| Ok(true),
        )
    })
    .unwrap();
}

/// Every value's rows read through the index equal those read without it.
fn whole(db: &Db) {
    assert_eq!(integrity(db), "ok");
    let all = count(db, "SELECT count(*) FROM t NOT INDEXED", vec![]).unwrap();
    assert_eq!(
        count(
            db,
            "SELECT count(*) FROM t INDEXED BY ix WHERE a >= 0",
            vec![]
        )
        .unwrap(),
        all
    );
    for v in 0..97 {
        assert_eq!(
            count(
                db,
                "SELECT count(*) FROM t INDEXED BY ix WHERE a = ?1",
                vec![Value::from_i64(v)]
            )
            .unwrap(),
            count(
                db,
                "SELECT count(*) FROM t NOT INDEXED WHERE a = ?1",
                vec![Value::from_i64(v)]
            )
            .unwrap(),
            "value {v}"
        );
    }
}

#[test]
fn t_t1_12_1_writes_during_the_change_keep_the_index_and_reads_wait_for_public() {
    // CP-T1.12: build ix on 1,000 rows in batches of 7 while each step's gap inserts, updates and
    // deletes rows. Expect: no plan, the writer's or a reader's, names ix before its cutover; after
    // it, ix has every row's entry and only theirs, and the integrity check passes.
    let (_dir, mut db) = table(1000);
    let reader = db.reader().unwrap();
    let ix = plan("CREATE INDEX ix ON t (a)");
    let query = "SELECT b FROM t WHERE a = 5";
    let mut steps = 0;
    loop {
        let stepped = db.transaction(|db| online::step(db, &ix, 7)).unwrap();
        steps += 1;
        if let Stepped::Public { .. } = stepped {
            break;
        }
        assert!(
            !plan_text(&db, query).contains("ix"),
            "the writer read ix before public"
        );
        // The reader reads first: a read prepares under the schema the writer published, where a
        // cached EXPLAIN, which opens no transaction, keeps the plan of the schema it was made in.
        assert!(count(&reader, "SELECT count(*) FROM t INDEXED BY ix", vec![]).is_err());
        assert!(
            !plan_text(&reader, query).contains("ix"),
            "a reader read ix before public"
        );
        assert!(count(&db, "SELECT count(*) FROM t INDEXED BY ix", vec![]).is_err());
        assert_eq!(integrity(&db), "ok");
        write(&mut db, steps);
        assert!(steps < 1000, "the change never ended");
    }
    assert!(
        steps > 100,
        "the backfill ran in {steps} steps, not batches of 7"
    );
    let writer_plan = plan_text(&db, query);
    assert!(
        writer_plan.contains("ix"),
        "the writer's plan after public: {writer_plan}"
    );
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM t INDEXED BY ix WHERE a = 5",
            vec![]
        )
        .unwrap(),
        count(
            &reader,
            "SELECT count(*) FROM t NOT INDEXED WHERE a = 5",
            vec![]
        )
        .unwrap(),
        "a reader's read through ix after public"
    );
    let reader_plan = plan_text(&reader, query);
    assert!(
        reader_plan.contains("ix"),
        "a reader's plan after public: {reader_plan}"
    );
    whole(&db);
    assert_eq!(
        db.transaction(|db| online::step(db, &ix, 7)).unwrap(),
        Stepped::Already
    );
}

#[test]
fn t_t1_12_1_a_write_only_unique_index_refuses_a_duplicate_write() {
    // CP-T1.12, F1's write-only state enforces the constraint it adds: once ix is write-only, two
    // rows written with the same a are refused; rows from before it aren't checked until backfill.
    let (_dir, mut db) = table(10);
    let ix = plan("CREATE UNIQUE INDEX ix ON t (a)");
    assert_eq!(
        db.transaction(|db| online::step(db, &ix, 7)).unwrap(),
        Stepped::DeleteOnly
    );
    assert_eq!(
        db.transaction(|db| online::step(db, &ix, 7)).unwrap(),
        Stepped::WriteOnly
    );
    db.exec("INSERT INTO t (a, b) VALUES (500, 'x')").unwrap();
    assert!(db.exec("INSERT INTO t (a, b) VALUES (500, 'y')").is_err());
    // An index or table in a change is the change's: neither drops under it.
    assert!(db.exec("DROP INDEX ix").is_err());
    assert!(db.exec("DROP TABLE t").is_err());
}

#[test]
fn t_t1_12_1_rows_that_break_a_unique_index_leave_the_old_schema() {
    // CP-T1.12, T-T1.12.1: a unique index over 300 rows whose a repeats. Expect: the change is
    // refused as BROKEN, ix is gone with every record of the change, the rows are untouched, and
    // the same index without UNIQUE then builds.
    let dir = tempfile::tempdir().unwrap();
    let until = || Instant::now() + Duration::from_secs(60);
    let database = Database::open(&dir.path().join("d.db"), until()).unwrap();
    database
        .migrate(
            Migration {
                name: "t".into(),
                sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) INSERT INTO t (a) SELECT i % 50 FROM n".into(),
            },
            until(),
        )
        .unwrap();
    let broken = database
        .change_schema(
            Migration {
                name: "ux".into(),
                sql: "CREATE UNIQUE INDEX ix ON t (a)".into(),
            },
            until(),
        )
        .unwrap_err();
    assert!(
        matches!(&broken, Refusal::Invalid { what, .. } if what == online::BROKEN),
        "{broken:?}"
    );
    // The index is gone; its state and change rows too, or the same name couldn't stage again.
    let left = database
        .call(until(), |c| {
            let index: i64 = c.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'ix'",
                [],
                |r| r.get(0usize),
            )?;
            let rows: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0usize))?;
            Ok((index, rows))
        })
        .unwrap();
    assert_eq!(left, (0, 300));
    let built = database
        .change_schema(
            Migration {
                name: "nx".into(),
                sql: "CREATE INDEX ix ON t (a)".into(),
            },
            until(),
        )
        .unwrap();
    assert!(
        built.applied && built.cutover.is_some() && built.rows >= 300,
        "{built:?}"
    );
    let again = database
        .change_schema(
            Migration {
                name: "nx".into(),
                sql: "CREATE INDEX ix ON t (a)".into(),
            },
            until(),
        )
        .unwrap();
    assert!(!again.applied);
}

#[test]
fn t_t1_12_1_a_column_is_added_in_one_catalog_change_and_its_constraints_hold_from_it() {
    // CP-T1.12: add c NOT NULL DEFAULT 7 CHECK (c >= 0) to a 300-row table online. Expect: public
    // in one step that backfills no row, each row written before reads 7, a write breaking the
    // CHECK or NOT NULL is refused from then on, and the same migration again is already applied;
    // a generated column checked against every row is refused and leaves the table as it was.
    let dir = tempfile::tempdir().unwrap();
    let until = || Instant::now() + Duration::from_secs(60);
    let database = Database::open(&dir.path().join("d.db"), until()).unwrap();
    database
        .migrate(
            Migration {
                name: "t".into(),
                sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) INSERT INTO t (a) SELECT i % 50 FROM n".into(),
            },
            until(),
        )
        .unwrap();
    let column = || Migration {
        name: "c".into(),
        sql: "ALTER TABLE t ADD COLUMN c INTEGER NOT NULL DEFAULT 7 CHECK (c >= 0)".into(),
    };
    let added = database.change_schema(column(), until()).unwrap();
    assert!(
        added.applied && added.cutover.is_some() && added.batches == 0 && added.rows == 0,
        "{added:?}"
    );
    let read = |sql: &'static str| {
        database
            .call(until(), move |c| {
                let n: i64 = c.query_row(sql, [], |r| r.get(0usize))?;
                Ok(n)
            })
            .unwrap()
    };
    assert_eq!(read("SELECT count(*) FROM t WHERE c = 7"), 300);
    let write = |sql: &'static str| {
        database.call(until(), move |c| {
            c.execute(sql, [])?;
            Ok(())
        })
    };
    assert!(write("INSERT INTO t (a, c) VALUES (1, -1)").is_err());
    assert!(write("INSERT INTO t (a, c) VALUES (1, NULL)").is_err());
    write("INSERT INTO t (a, c) VALUES (1, 3)").unwrap();
    assert_eq!(read("SELECT count(*) FROM t"), 301);
    assert!(!database.change_schema(column(), until()).unwrap().applied);
    let other = database
        .change_schema(
            Migration {
                name: "c".into(),
                sql: "ALTER TABLE t ADD COLUMN d INTEGER".into(),
            },
            until(),
        )
        .unwrap_err();
    assert!(matches!(other, Refusal::Invalid { .. }), "{other:?}");
    let generated = database
        .change_schema(
            Migration {
                name: "g".into(),
                sql: "ALTER TABLE t ADD COLUMN g AS (a + 1) NOT NULL".into(),
            },
            until(),
        )
        .unwrap_err();
    assert!(
        matches!(generated, Refusal::Invalid { .. }),
        "{generated:?}"
    );
    assert_eq!(
        read("SELECT count(*) FROM pragma_table_info('t')"),
        3,
        "the table's columns: id, a, c"
    );
}

fn sql_of_t(db: &Db) -> String {
    let mut sql = String::new();
    db.query_owned("SELECT sql FROM sqlite_schema WHERE name = 't'", [], |r| {
        sql = r.get::<String>(0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(false)
    })
    .unwrap();
    sql
}

#[test]
fn t_t1_12_1_a_constraint_is_enforced_on_writes_while_rows_are_validated_then_public() {
    // CP-T1.12: add NOT NULL and CHECK (a >= 0) to a of 1,000 rows in batches of 7 while each
    // step's gap inserts, updates and deletes rows. Expect: from the first step a write breaking
    // either is refused; the batches visit the rows before it; a NULL count reads the truth on the
    // writer and a reader throughout; after the cutover the mark is gone, the migration recorded,
    // the integrity check passes, and the same migration again is already applied.
    let (_dir, mut db) = table(1000);
    let reader = db.reader().unwrap();
    let nn = plan("ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL CHECK (a >= 0)");
    assert_eq!(
        db.transaction(|db| online::step(db, &nn, 7)).unwrap(),
        Stepped::WriteOnly
    );
    assert!(sql_of_t(&db).contains("NOT NULL"));
    assert!(db.exec("INSERT INTO t (a, b) VALUES (NULL, 'x')").is_err());
    assert!(db.exec("INSERT INTO t (a, b) VALUES (-1, 'x')").is_err());
    assert!(db.exec("UPDATE t SET a = NULL WHERE id = 3").is_err());
    db.exec("INSERT INTO t (a, b) VALUES (5, 'x')").unwrap();
    // A table with a column in a change is the change's: it neither drops nor alters otherwise.
    assert!(db.exec("DROP TABLE t").is_err());
    assert!(db.exec("ALTER TABLE t DROP COLUMN b").is_err());
    assert!(db.exec("ALTER TABLE t ADD COLUMN z INTEGER").is_err());
    let mut steps = 0;
    let mut visited = 0;
    loop {
        let stepped = db.transaction(|db| online::step(db, &nn, 7)).unwrap();
        steps += 1;
        match stepped {
            Stepped::Public { .. } => break,
            Stepped::Batch { visited: v, .. } => visited += v,
            other => panic!("stepped {other:?}"),
        }
        assert_eq!(
            count(&db, "SELECT count(*) FROM t WHERE a IS NULL", vec![]).unwrap(),
            0
        );
        assert_eq!(
            count(&db, "SELECT count(*) FROM __amphora_migrations", vec![]).unwrap(),
            0,
            "migrated before public"
        );
        write(&mut db, steps);
        assert!(steps < 1000, "the change never ended");
    }
    // Each step's write deletes at most one row, perhaps before its batch reached it.
    assert!(
        visited >= 1000 - u64::try_from(steps).unwrap(),
        "the batches visited {visited} rows in {steps} steps"
    );
    assert!(steps > 100, "validated in {steps} steps, not batches of 7");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM __amphora_constraint_states",
            vec![]
        )
        .unwrap(),
        0
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM __amphora_migrations WHERE name = 'ix'",
            vec![]
        )
        .unwrap(),
        1
    );
    assert_eq!(integrity(&db), "ok");
    assert_eq!(integrity(&reader), "ok");
    assert!(db.exec("INSERT INTO t (a, b) VALUES (NULL, 'x')").is_err());
    assert_eq!(
        db.transaction(|db| online::step(db, &nn, 7)).unwrap(),
        Stepped::Already
    );
}

#[test]
fn t_t1_12_1_an_unvalidated_not_null_is_not_relied_on_and_a_breaking_row_undoes_the_change() {
    // CP-T1.12: 50 rows, one with a NULL a and one with a = -3. Expect: while the constraint is
    // write-only the planner reads the NULL and negative rows (it doesn't trust a NOT NULL not yet
    // validated), on the writer and a reader, and the integrity check doesn't call them corrupt;
    // validation then stops at a breaking row, refused as BROKEN, and undoing the change gives a
    // its old definition back, the mark and record gone, NULL writable again.
    let (_dir, mut db) = table(50);
    db.exec("UPDATE t SET a = NULL WHERE id = 20; UPDATE t SET a = -3 WHERE id = 30")
        .unwrap();
    let reader = db.reader().unwrap();
    let nn = plan("ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL CHECK (a >= 0)");
    assert_eq!(
        db.transaction(|db| online::step(db, &nn, 7)).unwrap(),
        Stepped::WriteOnly
    );
    for d in [&db, &reader] {
        assert_eq!(
            count(d, "SELECT count(*) FROM t WHERE a IS NULL", vec![]).unwrap(),
            1
        );
        assert_eq!(
            count(d, "SELECT count(*) FROM t WHERE a IS NOT NULL", vec![]).unwrap(),
            49
        );
        assert_eq!(integrity(d), "ok");
    }
    let broken = loop {
        match db.transaction(|db| online::step(db, &nn, 7)) {
            Ok(Stepped::Batch { .. }) => {}
            Ok(other) => panic!("stepped {other:?} past a breaking row"),
            Err(e) => break e,
        }
    };
    assert!(
        matches!(broken.downcast_ref::<Refusal>(), Some(Refusal::Invalid { what, .. }) if what == online::BROKEN),
        "{broken:?}"
    );
    db.transaction(|db| online::abandon(db, &nn)).unwrap();
    assert!(!sql_of_t(&db).contains("NOT NULL"), "{}", sql_of_t(&db));
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM __amphora_constraint_states",
            vec![]
        )
        .unwrap(),
        0
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM __amphora_schema_changes", vec![]).unwrap(),
        0
    );
    db.exec("INSERT INTO t (a, b) VALUES (NULL, 'x')").unwrap();
    assert_eq!(integrity(&db), "ok");
}

#[test]
fn t_t1_12_1_a_constraint_change_is_refused_unless_it_only_adds_the_constraint() {
    // CP-T1.12: the new definition must be the column's own plus NOT NULL or CHECK, so the change
    // can be undone. Expect: a rename, a definition adding neither, a changed type, a dropped
    // CHECK and NOT NULL added twice are refused, leaving the table as it was; through
    // change_schema a breaking row undoes the change (BROKEN) and a clean column takes it.
    let refused = |sql: &str| {
        Plan::parse(&Migration {
            name: "c".into(),
            sql: sql.into(),
        })
        .is_err()
    };
    assert!(refused(
        "ALTER TABLE t ALTER COLUMN a TO z INTEGER NOT NULL"
    ));
    assert!(refused("ALTER TABLE t ALTER COLUMN a TO a TEXT"));
    let dir = tempfile::tempdir().unwrap();
    let until = || Instant::now() + Duration::from_secs(60);
    let database = Database::open(&dir.path().join("d.db"), until()).unwrap();
    database
        .migrate(
            Migration {
                name: "t".into(),
                sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT CHECK (length(b) > 0), c INTEGER NOT NULL DEFAULT 0); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) INSERT INTO t (a, b) SELECT i % 50, 'r' FROM n".into(),
            },
            until(),
        )
        .unwrap();
    let change = |name: &str, sql: &str| {
        database.change_schema(
            Migration {
                name: name.into(),
                sql: sql.into(),
            },
            until(),
        )
    };
    let table_sql = || {
        database
            .call(until(), |c| {
                let sql: String =
                    c.query_row("SELECT sql FROM sqlite_schema WHERE name = 't'", [], |r| {
                        r.get(0usize)
                    })?;
                Ok(sql)
            })
            .unwrap()
    };
    let before = table_sql();
    for (name, sql) in [
        ("ty", "ALTER TABLE t ALTER COLUMN a TO a TEXT NOT NULL"),
        ("dr", "ALTER TABLE t ALTER COLUMN b TO b TEXT NOT NULL"),
        (
            "tw",
            "ALTER TABLE t ALTER COLUMN c TO c INTEGER NOT NULL DEFAULT 0 NOT NULL",
        ),
    ] {
        let refusal = change(name, sql).unwrap_err();
        assert!(
            matches!(&refusal, Refusal::Invalid { what, .. } if what != online::BROKEN),
            "{name}: {refusal:?}"
        );
        assert_eq!(table_sql(), before, "{name} changed the table");
    }
    database
        .call(until(), |c| {
            c.execute("UPDATE t SET a = NULL WHERE id = 150", [])?;
            Ok(())
        })
        .unwrap();
    let broken = change("nn", "ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL").unwrap_err();
    assert!(
        matches!(&broken, Refusal::Invalid { what, .. } if what == online::BROKEN),
        "{broken:?}"
    );
    let a_not_null = database
        .call(until(), |c| {
            let n: i64 = c.query_row(
                "SELECT \"notnull\" FROM pragma_table_info('t') WHERE name = 'a'",
                [],
                |r| r.get(0usize),
            )?;
            Ok(n)
        })
        .unwrap();
    assert_eq!(a_not_null, 0, "a breaking row left the constraint");
    let added = change(
        "bc",
        "ALTER TABLE t ALTER COLUMN b TO b TEXT CHECK (length(b) > 0) NOT NULL CHECK (b <> '')",
    )
    .unwrap();
    assert!(
        added.applied && added.cutover.is_some() && added.rows >= 300,
        "{added:?}"
    );
    let refused_write = database.call(until(), |c| {
        c.execute("INSERT INTO t (a, b) VALUES (1, NULL)", [])?;
        Ok(())
    });
    assert!(refused_write.is_err());
}

fn rows_of(db: &Db, sql: &str) -> Vec<String> {
    let mut rows = Vec::new();
    db.query_owned(sql, [], |r| {
        let row: Vec<String> = r.get_values().map(|v| format!("{v:?}")).collect();
        rows.push(row.join("|"));
        Ok(true)
    })
    .unwrap();
    rows
}

/// [`write`]'s insert, update and delete, naming no `b`.
fn write_without_b(db: &mut Db, i: i64) {
    db.transaction(|db| {
        db.query(
            "INSERT INTO t (a) VALUES (?1)",
            [Value::from_i64(i % 97)],
            |_| Ok(true),
        )?;
        db.query(
            "UPDATE t SET a = ?1 WHERE id = ?2",
            [Value::from_i64((i * 7) % 97), Value::from_i64(i * 13 + 1)],
            |_| Ok(true),
        )?;
        db.query(
            "DELETE FROM t WHERE id = ?1",
            [Value::from_i64(i * 17 + 2)],
            |_| Ok(true),
        )
    })
    .unwrap();
}

#[test]
fn t_t1_12_1_a_dropped_column_is_hidden_while_its_values_are_cleared_then_recorded() {
    // CP-T1.12: drop b of 1,000 rows online in batches of 7 while each step's gap inserts,
    // updates and deletes rows. Expect: from the first step no query, insert or update finds b
    // (`*`, `PRAGMA table_info` and an insert without a column list pass it), on the writer and a
    // reader, and `SELECT *` answers what `SELECT id, a` answered before; the migration is
    // recorded only at the end; reopened without the slot's mark, every row's b is NULL (new
    // writes stored NULL, the batches cleared the old values) and the integrity check passes.
    let (dir, mut db) = table(1000);
    let reader = db.reader().unwrap();
    let before = rows_of(&db, "SELECT id, a FROM t ORDER BY id");
    let dc = plan("ALTER TABLE t DROP COLUMN b");
    assert_eq!(
        db.transaction(|db| online::step(db, &dc, 7)).unwrap(),
        Stepped::WriteOnly
    );
    let hidden = |db: &Db| {
        assert!(db.column_count("SELECT b FROM t").is_err());
        assert!(db.column_count("SELECT t.b FROM t").is_err());
        assert!(db.column_count("SELECT * FROM t WHERE b = 'r1'").is_err());
        assert_eq!(db.column_count("SELECT * FROM t").unwrap(), 2);
        assert_eq!(
            count(db, "SELECT count(*) FROM pragma_table_info('t')", vec![]).unwrap(),
            2
        );
    };
    hidden(&db);
    hidden(&reader);
    assert_eq!(rows_of(&db, "SELECT * FROM t ORDER BY id"), before);
    assert_eq!(rows_of(&reader, "SELECT * FROM t ORDER BY id"), before);
    assert!(db.exec("INSERT INTO t (a, b) VALUES (1, 'x')").is_err());
    assert!(db.exec("INSERT INTO t VALUES (NULL, 1, 'x')").is_err());
    assert!(db.exec("UPDATE t SET b = 'x'").is_err());
    db.exec("INSERT INTO t VALUES (NULL, 1)").unwrap();
    db.exec("INSERT INTO t DEFAULT VALUES").unwrap();
    let mut steps = 0;
    let mut visited = 0;
    loop {
        let stepped = db.transaction(|db| online::step(db, &dc, 7)).unwrap();
        steps += 1;
        match stepped {
            Stepped::Public { .. } => break,
            Stepped::Batch { visited: v, .. } => visited += v,
            other => panic!("stepped {other:?}"),
        }
        hidden(&db);
        assert_eq!(
            count(&db, "SELECT count(*) FROM __amphora_migrations", vec![]).unwrap(),
            0,
            "migrated before the clear ended"
        );
        write_without_b(&mut db, steps);
        assert!(steps < 1000, "the change never ended");
    }
    assert!(
        visited >= 1000 - u64::try_from(steps).unwrap(),
        "the batches visited {visited} rows in {steps} steps"
    );
    assert!(steps > 100, "cleared in {steps} steps, not batches of 7");
    hidden(&db);
    hidden(&reader);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM __amphora_migrations WHERE name = 'ix'",
            vec![]
        )
        .unwrap(),
        1
    );
    assert_eq!(integrity(&db), "ok");
    assert_eq!(
        db.transaction(|db| online::step(db, &dc, 7)).unwrap(),
        Stepped::Already
    );
    let rows = count(&db, "SELECT count(*) FROM t", vec![]).unwrap();
    // A reopened database keeps the slot hidden.
    drop(reader);
    drop(db);
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    hidden(&db);
    db.exec("DELETE FROM __amphora_dropped_columns").unwrap();
    drop(db);
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    assert_eq!(db.column_count("SELECT * FROM t").unwrap(), 3);
    assert_eq!(
        count(&db, "SELECT count(*) FROM t WHERE b IS NULL", vec![]).unwrap(),
        rows
    );
    assert_eq!(integrity(&db), "ok");
}

#[test]
fn t_t1_12_1_a_drop_is_refused_where_the_engine_refuses_it_and_its_slot_keeps_the_name() {
    // CP-T1.12: through change_schema, dropping a key, an indexed column, a column a view or a
    // CHECK names, the only other column, a column of a table with a trigger, or of a WITHOUT
    // ROWID table is refused before anything changes. A NOT NULL column with a default and its
    // own CHECK is dropped: inserts store NULL in its slot; adding a column of its name is
    // refused; a reopened database keeps it hidden.
    let dir = tempfile::tempdir().unwrap();
    let until = || Instant::now() + Duration::from_secs(60);
    let path = dir.path().join("d.db");
    let database = Database::open(&path, until()).unwrap();
    database
        .migrate(
            Migration {
                name: "t".into(),
                sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c INTEGER NOT NULL DEFAULT 7 CHECK (c > 0), d INTEGER CHECK (d <> a)); CREATE INDEX t_a ON t (b); CREATE VIEW v AS SELECT a FROM t; CREATE TABLE g (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER); CREATE TRIGGER gt AFTER INSERT ON g BEGIN SELECT 1; END; CREATE TABLE w (k INTEGER PRIMARY KEY, x INTEGER, y INTEGER) WITHOUT ROWID; CREATE TABLE o (id INTEGER PRIMARY KEY, x INTEGER); CREATE TABLE u (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER CHECK (y <> x)); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) INSERT INTO t (a, b) SELECT i % 50, 'r' FROM n".into(),
            },
            until(),
        )
        .unwrap();
    let change = |name: &str, sql: &str| {
        database.change_schema(
            Migration {
                name: name.into(),
                sql: sql.into(),
            },
            until(),
        )
    };
    let schema = || {
        database
            .call(until(), |c| {
                let sql: String = c.query_row(
                    "SELECT group_concat(sql, ';') FROM sqlite_schema WHERE name NOT LIKE '__amphora%'",
                    [],
                    |r| r.get(0usize),
                )?;
                Ok(sql)
            })
            .unwrap()
    };
    let star = |database: &Database| {
        database
            .call(until(), |c| {
                let n: i64 =
                    c.query_row("SELECT count(*) FROM pragma_table_info('t')", [], |r| {
                        r.get(0usize)
                    })?;
                Ok(n)
            })
            .unwrap()
    };
    let before = schema();
    for (name, sql) in [
        ("pk", "ALTER TABLE t DROP COLUMN id"),
        ("ix", "ALTER TABLE t DROP COLUMN b"),
        ("vw", "ALTER TABLE t DROP COLUMN a"),
        ("ck", "ALTER TABLE u DROP COLUMN x"),
        ("tg", "ALTER TABLE g DROP COLUMN x"),
        ("wr", "ALTER TABLE w DROP COLUMN x"),
        ("on", "ALTER TABLE o DROP COLUMN x"),
        ("no", "ALTER TABLE t DROP COLUMN z"),
    ] {
        let refusal = match change(name, sql) {
            Ok(done) => panic!("{name}: {sql} went on: {done:?}"),
            Err(refusal) => refusal,
        };
        assert!(
            matches!(&refusal, Refusal::Invalid { what, .. } if what != online::BROKEN),
            "{name}: {refusal:?}"
        );
        assert_eq!(schema(), before, "{name} changed the schema");
        assert_eq!(star(&database), 5, "{name} left t's columns changed");
    }
    let dropped = change("c", "ALTER TABLE t DROP COLUMN c").unwrap();
    assert!(dropped.applied && dropped.rows >= 300, "{dropped:?}");
    database
        .call(until(), |c| {
            c.execute("INSERT INTO t (a, b, d) VALUES (1, 'x', 2)", [])?;
            Ok(())
        })
        .unwrap();
    let readd = database.call(until(), |c| {
        c.execute("ALTER TABLE t ADD COLUMN c INTEGER", [])?;
        Ok(())
    });
    assert!(readd.is_err(), "the slot's name was taken");
    assert_eq!(star(&database), 4);
    drop(database);
    let database = Database::open(&path, until()).unwrap();
    assert_eq!(star(&database), 4);
    let added = database
        .change_schema(
            Migration {
                name: "e".into(),
                sql: "ALTER TABLE t ADD COLUMN e INTEGER DEFAULT 1".into(),
            },
            until(),
        )
        .unwrap();
    assert!(added.applied);
    assert_eq!(star(&database), 5);
}

#[test]
fn t_t1_12_1_a_persons_alter_column_of_a_held_column_is_refused_and_the_changes_own_goes_on() {
    // CP-T1.12: while NOT NULL is validated on a, a person's ALTER COLUMN of a (the change's own
    // SQL, the old definition, or another) is refused and leaves the table as it was; the change's
    // own steps redefine it, and a breaking row's undo puts the old definition back.
    let (_dir, mut db) = table(50);
    let nn = plan("ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL");
    assert_eq!(
        db.transaction(|db| online::step(db, &nn, 7)).unwrap(),
        Stepped::WriteOnly
    );
    let held = sql_of_t(&db);
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL",
        "ALTER TABLE t ALTER COLUMN a TO a INTEGER",
        "ALTER TABLE t ALTER COLUMN a TO a INTEGER NOT NULL CHECK (a < 1000)",
    ] {
        assert!(db.exec(sql).is_err(), "{sql} went on");
        assert_eq!(sql_of_t(&db), held, "{sql} changed the table");
    }
    db.transaction(|db| online::abandon(db, &nn)).unwrap();
    assert!(!sql_of_t(&db).contains("NOT NULL"), "{}", sql_of_t(&db));
    db.exec("ALTER TABLE t ALTER COLUMN a TO a INTEGER CHECK (a >= 0)")
        .unwrap();
}
