//! U05: restartable SQL bootstrap, FK validation, and one canonical writer owner.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]
use amphora::database::{Database, Restore};
use amphora_api::restore::RestorePage;
use amphora_api::{Records, Statement, Tx, Write};
use std::time::{Duration, Instant};
fn until() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
fn statement(sql: &str) -> Statement {
    Statement {
        sql: sql.into(),
        params: vec![],
    }
}

#[test]
fn pages_resume_without_exposing_partial_state_and_validate_foreign_keys_before_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let restore = Restore::open(&path, "sha256:fixed-snapshot", until()).unwrap();
    restore.apply_page(RestorePage {
        expected_cursor:vec![],next_cursor:vec![1],
        schema:Some("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES parent(id))".into()),
        statements:vec![statement("INSERT INTO child VALUES (1, 7)")],
    }, until()).unwrap();
    assert!(Database::open(&path, until()).is_err());
    drop(restore);
    assert!(Database::open(&path, until()).is_err());
    assert!(Restore::open(&path, "sha256:wrong-snapshot", until()).is_err());
    let restore = Restore::open(&path, "sha256:fixed-snapshot", until()).unwrap();
    assert_eq!(restore.progress(until()).unwrap().cursor, vec![1]);
    assert!(
        restore.finish(vec![1], until()).is_err(),
        "dangling FKs refuse activation"
    );
    let restore = Restore::open(&path, "sha256:fixed-snapshot", until()).unwrap();
    restore
        .apply_page(
            RestorePage {
                expected_cursor: vec![1],
                next_cursor: vec![2],
                schema: None,
                statements: vec![statement("INSERT INTO parent VALUES (7)")],
            },
            until(),
        )
        .unwrap();
    let db = restore.finish(vec![2], until()).unwrap();
    assert!(db.restore_progress(until()).unwrap().unwrap().done);
    let count = db
        .call(until(), |c| {
            Ok(c.query_row("SELECT count(*) FROM child", [], |r| {
                r.get::<_, i64>(0usize)
            })?)
        })
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        db.call(until(), |c| {
            c.execute("INSERT INTO child VALUES (2,99)", [])?;
            Ok(())
        })
        .is_err(),
        "normal sessions enforce FKs"
    );
}

#[test]
fn namespaces_keep_the_canonical_owner_alive_and_memory_uses_the_same_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let first = Database::open(&path, until()).unwrap();
    let namespace = first.namespace("records").unwrap();
    drop(first);
    let reopened = Database::open(&path, until()).unwrap();
    let at = reopened
        .namespace("records")
        .unwrap()
        .commit(
            Tx {
                conds: vec![],
                writes: vec![Write::Put {
                    family: "items".into(),
                    key: b"x".into(),
                    value: b"y".into(),
                }],
            },
            until(),
        )
        .unwrap();
    assert_eq!(namespace.try_wait("items", 0).unwrap(), Some(at));
    assert!(Restore::open(&path, "new-source", until()).is_err());
    drop(reopened);
    drop(namespace);
    let memory = Database::in_memory(until()).unwrap();
    memory
        .call(until(), |c| {
            c.execute_batch("CREATE TABLE test(n); INSERT INTO test VALUES(42)")?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        memory
            .call(until(), |c| Ok(c.query_row(
                "SELECT n FROM test",
                [],
                |r| r.get::<_, i64>(0usize)
            )?))
            .unwrap(),
        42
    );
}

#[test]
fn unknown_outcome_keeps_the_owner_lease_until_the_writer_actually_drains() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Database::open(&path, until()).unwrap();
    let (started, begun) = std::sync::mpsc::sync_channel(1);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let (observed, outcome) = std::sync::mpsc::sync_channel(1);
    let caller = std::thread::spawn(move || {
        db.call(Instant::now() + Duration::from_millis(25), move |c| {
        started.send(())?;
        released.recv()?;
        let result = c.execute_batch("CREATE TABLE committed_after_timeout(n); INSERT INTO committed_after_timeout VALUES (42)");
        observed.send(result.as_ref().map(|_| ()).map_err(ToString::to_string))?;
        result?;
        Ok(())
    })
    });
    begun.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(
        caller.join().unwrap(),
        Err(amphora_api::Refusal::UnknownOutcome)
    ));
    assert!(Database::open(&path, Instant::now() + Duration::from_millis(5)).is_err());
    release.send(()).unwrap();
    outcome
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let reopened = Database::open(&path, until()).unwrap();
    assert_eq!(
        reopened
            .call(until(), |c| Ok(c.query_row(
                "SELECT n FROM committed_after_timeout",
                [],
                |r| r.get::<_, i64>(0usize)
            )?))
            .unwrap(),
        42
    );
}

/// U05/CP-T1.5: distinct live memory databases must not share the native file registry.
#[test]
fn independent_memory_databases_keep_sql_and_records_isolated() {
    use amphora_api::{Consistency, Records, Tx, Write};
    let first = Database::in_memory(until()).unwrap();
    let second = Database::in_memory(until()).unwrap();
    first
        .call(until(), |connection| {
            connection
                .execute_batch("CREATE TABLE private_row(n); INSERT INTO private_row VALUES (7)")?;
            connection.records_commit(
                "tenant",
                Tx {
                    conds: vec![],
                    writes: vec![Write::Put {
                        family: "items".into(),
                        key: b"private".to_vec(),
                        value: b"first".to_vec(),
                    }],
                },
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        second
            .call(until(), |connection| Ok(connection.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'private_row'",
                [],
                |row| row.get::<_, i64>(0usize)
            )?))
            .unwrap(),
        0
    );
    let namespace = second.namespace("tenant").unwrap();
    assert_eq!(namespace.head(until()).unwrap(), 0);
    assert!(
        namespace
            .get("items", b"private", Consistency::Linearizable, until())
            .unwrap()
            .is_none()
    );
}
