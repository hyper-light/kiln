#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]
//! CP-T1.5 / U05/U06: independent processes share the same writer and Records revisions.
use amphora::{database::Database, transport::Client};
use amphora_api::{self as api, Records};
use anyhow::Context;
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn until() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
fn replica_config() -> api::replication::ReplicaConfig {
    api::replication::ReplicaConfig {
        node_id: 1,
        voters: vec![1],
        learners: vec![],
        bootstrap: None,
    }
}
fn known_leader(database: &Database) {
    let deadline = until();
    loop {
        let status = database.replica_status(deadline).unwrap().unwrap();
        assert!(
            status.fenced.is_none(),
            "replica fenced: {:?}",
            status.fenced
        );
        if status.leader != 0 && !status.bootstrapping {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "replica did not become ready: {status:?}"
        );
        std::thread::park_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1)),
        );
    }
}
fn child(path: &Path, mode: &str) -> std::process::ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "transport_child", "--nocapture"])
        .env("AMPHORA_TRANSPORT_TEST_PATH", path)
        .env("AMPHORA_TRANSPORT_TEST_MODE", mode)
        .status()
        .unwrap()
}
fn migration_diagnostics(database: &Database) {
    // U05/U16: caller migration context and the engine cause survive either SQL session path.
    let failure = database
        .call::<(), _>(until(), |db| {
            db.execute_batch("CREATE INDEX diagnostic_index ON missing_diagnostic_table(value)")
                .context("migration 20261023-1300_p6_records.sql")
                .context("apply migrations")?;
            Ok(())
        })
        .unwrap_err();
    let api::Refusal::Unavailable(message) = failure else {
        panic!("untyped SQL error changed refusal: {failure:?}");
    };
    for expected in [
        "apply migrations",
        "20261023-1300_p6_records.sql",
        "missing_diagnostic_table",
    ] {
        assert!(
            message.contains(expected),
            "lost error context {expected}: {message}"
        );
    }
    let refusal = database.call::<(), _>(until(), |_| {
        Err(
            anyhow::Error::new(api::Refusal::NotLeader { leader: Some(7) })
                .context("apply migrations"),
        )
    });
    assert!(matches!(
        refusal,
        Err(api::Refusal::NotLeader { leader: Some(7) })
    ));
    let refusal = database.call::<(), _>(until(), |_| {
        Err(
            anyhow::Error::new(api::sql::Error::Refusal(api::Refusal::Fenced(
                "writer fenced".into(),
            )))
            .context("apply migrations"),
        )
    });
    assert!(matches!(refusal, Err(api::Refusal::Fenced(reason)) if reason == "writer fenced"));
}
// The disconnect oracle must kill the isolated child before Commit or any Rust cleanup.
// Returning from the closure would commit; unwinding would exercise explicit Abort instead.
#[allow(clippy::exit)]
fn exit_disconnected_child() -> ! {
    std::process::exit(0)
}
#[test]
fn transport_child() {
    let Some(path) = std::env::var_os("AMPHORA_TRANSPORT_TEST_PATH") else {
        return;
    };
    let path = Path::new(&path);
    match std::env::var("AMPHORA_TRANSPORT_TEST_MODE")
        .unwrap()
        .as_str()
    {
        mode @ ("hold_owner" | "hold_replica") => {
            let database = if mode == "hold_replica" {
                let database = Database::open_replicated(
                    path,
                    &path.with_extension("log"),
                    replica_config(),
                    until(),
                )
                .unwrap();
                known_leader(&database);
                database
            } else {
                Database::open(path, until()).unwrap()
            };
            database
                .call(until(), |db| {
                    db.execute_batch(
                        "CREATE TABLE items(value INTEGER); INSERT INTO items VALUES(1)",
                    )?;
                    Ok(())
                })
                .unwrap();
            std::io::stdout().write_all(b"\nowner-ready\n").unwrap();
            std::io::stdout().flush().unwrap();
            let _ = std::io::stdin().read_exact(&mut [0]);
            drop(database);
        }
        "owner" => {
            let database = Database::open(path, until()).unwrap();
            migration_diagnostics(&database);
            let n = database
                .call(until(), |db| {
                    Ok(db.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))?)
                })
                .unwrap();
            assert_eq!(n, 1);
        }
        "cas" => {
            let client = Database::open(path, until()).unwrap();
            let ns = client.namespace("ledger").unwrap();
            let tx = api::Tx {
                conds: vec![api::Cond::Absent {
                    family: "work".into(),
                    key: b"one".to_vec(),
                }],
                writes: vec![api::Write::Put {
                    family: "work".into(),
                    key: b"one".to_vec(),
                    value: b"claimed".to_vec(),
                }],
            };
            assert!(matches!(
                ns.commit(tx, until()),
                Err(api::Refusal::Conflict { .. })
            ));
            client
                .call(until(), |db| {
                    let n: i64 = db.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?;
                    assert_eq!(n, 1);
                    db.execute("INSERT INTO items VALUES (?1)", [n + 1])?;
                    let row = db.records_get("ledger", "work", b"one")?.unwrap();
                    assert_eq!(row.value, b"claimed");
                    db.records_commit(
                        "ledger",
                        api::Tx {
                            conds: vec![api::Cond::At {
                                family: "work".into(),
                                key: b"one".to_vec(),
                                rev: row.rev,
                            }],
                            writes: vec![api::Write::Put {
                                family: "work".into(),
                                key: b"one".to_vec(),
                                value: b"child_claimed".to_vec(),
                            }],
                        },
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        "disconnect" => {
            let client = Database::open(path, until()).unwrap();
            let _: Result<(), _> = client.call(until(), |db| {
                db.execute("INSERT INTO items VALUES (99)", [])?;
                db.records_commit(
                    "ledger",
                    api::Tx {
                        conds: vec![],
                        writes: vec![api::Write::Put {
                            family: "work".into(),
                            key: b"disconnect".to_vec(),
                            value: b"discarded".to_vec(),
                        }],
                    },
                )?;
                exit_disconnected_child()
            });
            panic!("disconnect child returned")
        }
        _ => panic!("unknown child mode"),
    }
}

#[test]
fn sdk_client_keeps_ledger_and_becomes_owner_after_predecessor_exits() {
    // CP-T1.5/U16: blue/green overlap retains the same durable SQL and Records after old exits.
    for replicated in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut predecessor = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "transport_child", "--nocapture"])
            .env("AMPHORA_TRANSPORT_TEST_PATH", &path)
            .env(
                "AMPHORA_TRANSPORT_TEST_MODE",
                if replicated {
                    "hold_replica"
                } else {
                    "hold_owner"
                },
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // The test harness writes a prefix first. Read through the explicit owner-ready line.
        let mut stdout = std::io::BufReader::new(predecessor.stdout.take().unwrap());
        let mut ready = String::new();
        use std::io::BufRead;
        loop {
            ready.clear();
            assert_ne!(stdout.read_line(&mut ready).unwrap(), 0);
            if ready.trim() == "owner-ready" {
                break;
            }
        }
        let successor = if replicated {
            // Attachment must happen before the host tries any exclusive bootstrap inspection.
            let database = Database::attach_replicated(
                &path,
                &path.with_extension("log"),
                replica_config(),
                until(),
            )
            .unwrap()
            .unwrap();
            assert!(database.replica().is_none());
            assert!(database.ensure_owner(until()).unwrap().is_none());
            database
        } else {
            Database::open(&path, until()).unwrap()
        };
        assert!(matches!(
            successor.replica_status(Instant::now()),
            Err(api::Refusal::Timeout)
        ));
        let status = successor.replica_status(until()).unwrap();
        if replicated {
            let status = status.unwrap();
            assert_eq!(status.node_id, 1);
            assert_eq!(status.voters, vec![1]);
            assert_ne!(status.leader, 0);
            assert!(!status.bootstrapping);
            assert!(status.fenced.is_none());
        } else {
            assert!(status.is_none());
        }
        assert!(
            successor.replica().is_none(),
            "status must not promote an attached client"
        );
        let ns = successor.namespace("ledger").unwrap();
        let write = |key: u8| api::Tx {
            conds: vec![],
            writes: vec![api::Write::Put {
                family: "work".into(),
                key: vec![key],
                value: b"retained".to_vec(),
            }],
        };
        assert_eq!(ns.commit(write(1), until()).unwrap(), 1);
        predecessor.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(predecessor.wait().unwrap().success());
        assert!(matches!(
            successor.replica_status(until()),
            Err(api::Refusal::Unavailable(_) | api::Refusal::Timeout)
        ));
        assert!(
            successor.replica().is_none(),
            "an absent owner status must not acquire ownership"
        );
        if replicated {
            assert!(successor.ensure_owner(until()).unwrap().is_some());
            assert!(successor.replica().is_some());
            known_leader(&successor);
        }
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        successor
            .call(until(), move |db| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(
                    db.query_row("SELECT value FROM items", [], |r| r.get::<_, i64>(0))?,
                    1
                );
                db.execute("INSERT INTO items VALUES(2)", [])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(ns.head(until()).unwrap(), 1);
        assert_eq!(ns.commit(write(2), until()).unwrap(), 2);
        drop(successor);
        assert_eq!(
            Client::connect(&path, until())
                .unwrap()
                .namespace("ledger")
                .unwrap()
                .head(until())
                .unwrap(),
            2
        );
    }
}

#[test]
fn separate_memory_databases_do_not_share_sql_or_records() {
    // CP-T1.5/U05: live MemoryIO instances never share a native registry entry by filename.
    let first = Database::in_memory(until()).unwrap();
    let second = Database::in_memory(until()).unwrap();
    for database in [&first, &second] {
        database
            .call(until(), |db| {
                db.execute_batch("CREATE TABLE isolated(value INTEGER)")?;
                Ok(())
            })
            .unwrap();
    }
    first
        .call(until(), |db| {
            db.execute("INSERT INTO isolated VALUES(1)", [])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        second
            .call(until(), |db| Ok(db.query_row(
                "SELECT count(*) FROM isolated",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        0
    );
    first
        .namespace("ledger")
        .unwrap()
        .commit(
            api::Tx {
                conds: vec![],
                writes: vec![api::Write::Put {
                    family: "work".into(),
                    key: b"one".to_vec(),
                    value: b"one".to_vec(),
                }],
            },
            until(),
        )
        .unwrap();
    assert_eq!(
        second.namespace("ledger").unwrap().head(until()).unwrap(),
        0
    );
}
#[test]
fn independent_processes_share_cas_and_adaptive_sql_and_abort_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let database = Database::open(&path, until()).unwrap();
    migration_diagnostics(&database);
    let client = Client::connect(&path, until()).unwrap();
    client
        .call(until(), |db| {
            db.execute_batch("CREATE TABLE items(value INTEGER); INSERT INTO items VALUES(1)")?;
            Ok(())
        })
        .unwrap();
    let ns = client.namespace("ledger").unwrap();
    ns.commit(
        api::Tx {
            conds: vec![],
            writes: vec![api::Write::Put {
                family: "work".into(),
                key: b"one".to_vec(),
                value: b"claimed".to_vec(),
            }],
        },
        until(),
    )
    .unwrap();
    assert_eq!(ns.try_wait("work", 0, until()).unwrap(), Some(1));
    let restored = client.namespace("restored").unwrap();
    restored
        .import_page(
            api::import::ImportPage {
                source: "fixed-snapshot".into(),
                expected_cursor: vec![],
                next_cursor: vec![1],
                versions: vec![api::import::ImportVersion {
                    family: "work".into(),
                    key: b"old".to_vec(),
                    rev: 7,
                    seq: 0,
                    value: Some(b"retained".to_vec()),
                }],
                state: api::import::ImportState {
                    head: 7,
                    horizon: 0,
                    feed: 0,
                    newest: vec![("work".into(), 7)],
                },
                done: true,
            },
            until(),
        )
        .unwrap();
    assert!(restored.import_state(until()).unwrap().unwrap().done);
    assert_eq!(
        restored
            .get("work", b"old", api::Consistency::Linearizable, until())
            .unwrap()
            .unwrap()
            .rev,
        7
    );
    assert!(child(&path, "owner").success());
    assert!(child(&path, "cas").success());
    let result: Result<(), _> = client.call(until(), |db| {
        let n: i64 = db.query_row("SELECT count(*) FROM items", [], |r| r.get(0))?;
        assert_eq!(n, 2);
        db.execute("INSERT INTO items VALUES(3)", [])?;
        let row = db.records_get("ledger", "work", b"one")?.unwrap();
        assert_eq!(row.value, b"child_claimed");
        db.records_commit(
            "ledger",
            api::Tx {
                conds: vec![api::Cond::At {
                    family: "work".into(),
                    key: b"one".to_vec(),
                    rev: row.rev,
                }],
                writes: vec![api::Write::Put {
                    family: "work".into(),
                    key: b"one".to_vec(),
                    value: b"discarded".to_vec(),
                }],
            },
        )?;
        anyhow::bail!("application rollback")
    });
    assert!(result.is_err());
    assert!(child(&path, "disconnect").success());
    // The next writer operation waits behind disconnect cleanup, without a guessed sleep.
    let n = client
        .call(until(), |db| {
            db.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                .map_err(Into::into)
        })
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        ns.get("work", b"one", api::Consistency::Linearizable, until())
            .unwrap()
            .unwrap()
            .value,
        b"child_claimed"
    );
    assert!(
        ns.get(
            "work",
            b"disconnect",
            api::Consistency::Linearizable,
            until()
        )
        .unwrap()
        .is_none()
    );
    let conflict: Result<(), _> = client.call(until(), |db| {
        db.execute("INSERT INTO items VALUES(100)", [])?;
        db.records_commit(
            "ledger",
            api::Tx {
                conds: vec![api::Cond::Absent {
                    family: "work".into(),
                    key: b"one".to_vec(),
                }],
                writes: vec![],
            },
        )?;
        Ok(())
    });
    assert!(matches!(
        conflict,
        Err(amphora::transport::Error::Refused(
            api::Refusal::Conflict { .. }
        ))
    ));
    let clone = database.clone();
    drop(database);
    assert_eq!(
        client.namespace("ledger").unwrap().head(until()).unwrap(),
        2
    );
    drop(clone);
}
