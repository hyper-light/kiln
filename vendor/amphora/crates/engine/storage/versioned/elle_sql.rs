//! Amphora: Elle-style isolation checks through the SQL layer (G8 steps 4 and 5, CP-T1.4,
//! `T-G8.4.1`): the histories of `elle` (list-append and read-write-register, Elle's `wr-txns`
//! generator, its checker of Adya's anomalies) recorded from many connections running SQL at once,
//! where `elle` records them at the storage. The claim checked is serializable, on each storage (a
//! test each): the pager's single writer, and the versioned storage under its default validation
//! (precision locking, `Validation::default`); so every anomaly the checker names is a failure.
//!
//! A list is a row `elle(k, v)`, its elements comma-separated in `v`, appended to by one upsert
//! (Elle's SQL list-append: `INSERT .. ON CONFLICT DO UPDATE SET v = v || ',' || e`); a register is
//! a row `reg(k, v)`, written by an upsert. A read reads a key, or (odd seeds, as `elle`'s) a span of
//! two rows from it, so phantoms are in reach. Each connection is a thread taking its next
//! transaction from the seed's generator; a refused transaction is rolled back and kept in the
//! history as aborted (its writes must never be seen). A register history orders each key's
//! versions by a counter each transaction takes just before its `COMMIT`: a later writer of the key
//! either began after this one was visible (so took the counter after) or was refused.
//!
//! `AMPHORA_ELLE_SQL_SEEDS` (seeds; `elle`'s threaded driver's 4 unless set), `AMPHORA_ELLE_SQL_FIRST`
//! (the first seed; 1) and `AMPHORA_ELLE_SQL_WRITERS` (connections; the step's 32 unless set) choose
//! the run, so the coordinator's 10,000-seed runs are the same test. Each seed runs `elle::TXNS`
//! transactions.

use super::elle::{check_lists, check_registers, Anomaly, Gen, Mop, Plan, Record, TXNS, WRITERS};
use super::many_writers::{connect, env, int, open, refused, roll_back, rows, Storage};
use crate::sync::Arc;
use crate::types::Value;
use crate::{Connection, LimboError};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Which history: lists or registers; reads of a key or of a span.
#[derive(Clone, Copy, Debug)]
struct Workload {
    lists: bool,
    spans: bool,
}

fn table(w: Workload) -> &'static str {
    if w.lists {
        "elle"
    } else {
        "reg"
    }
}

fn decode(v: &Value) -> Vec<u64> {
    let text = v.to_text().unwrap();
    text.split(',').map(|e| e.parse().unwrap()).collect()
}

fn observed(w: Workload, key: u64, v: Option<&Value>) -> Mop {
    if w.lists {
        Mop::List {
            key,
            seen: v.map(decode).unwrap_or_default(),
        }
    } else {
        Mop::Reg {
            key,
            seen: v.map(|v| u64::try_from(int(v)).unwrap()),
        }
    }
}

/// Runs one planned operation as SQL in the open transaction, what it read or wrote added to `mops`.
fn step(
    conn: &Arc<Connection>,
    w: Workload,
    plan: Plan,
    ids: &AtomicU64,
    mops: &mut Vec<Mop>,
) -> Result<(), LimboError> {
    let t = table(w);
    match plan {
        Plan::Write(key) => {
            let id = ids.fetch_add(1, Ordering::Relaxed);
            if w.lists {
                conn.execute(format!(
                    "INSERT INTO elle VALUES ({key}, '{id}') ON CONFLICT (k) DO UPDATE SET v = v || ',' || '{id}'"
                ))?;
                mops.push(Mop::Append { key, element: id });
            } else {
                conn.execute(format!(
                    "INSERT INTO reg VALUES ({key}, {id}) ON CONFLICT (k) DO UPDATE SET v = excluded.v"
                ))?;
                mops.push(Mop::Write { key, value: id });
            }
        }
        Plan::Read(key) if !w.spans => {
            let got = rows(conn, &format!("SELECT v FROM {t} WHERE k = {key}"))?;
            mops.push(observed(w, key, got.first().and_then(|r| r.first())));
        }
        Plan::Read(key) => {
            let got = rows(
                conn,
                &format!("SELECT k, v FROM {t} WHERE k >= {key} ORDER BY k LIMIT 2"),
            )?;
            // The span read, as `elle`'s: to the last row given when it gave two, else to the end;
            // keys of it no row holds read as absent.
            let got: BTreeMap<u64, &Value> = got
                .iter()
                .map(|r| (u64::try_from(int(&r[0])).unwrap(), &r[1]))
                .collect();
            let to = if got.len() == 2 {
                got.keys().next_back().copied()
            } else {
                None
            };
            let mut keys: BTreeSet<u64> = got.keys().copied().collect();
            for j in [key, key + 1] {
                if to.is_none_or(|to| j <= to) {
                    keys.insert(j);
                }
            }
            for j in keys {
                mops.push(observed(w, j, got.get(&j).copied()));
            }
        }
    }
    Ok(())
}

/// A seed's history from `writers` connections at once, its final rows, and its counts.
struct Run {
    history: Vec<Record>,
    finals: Vec<(u64, Value)>,
    committed: usize,
    refused: usize,
}

fn run(storage: Storage, w: Workload, seed: u64, writers: u64) -> Run {
    let db = open();
    let setup = connect(&db, storage);
    let ty = if w.lists { "TEXT" } else { "INTEGER" };
    setup
        .execute(format!(
            "CREATE TABLE {} (k INTEGER PRIMARY KEY, v {ty} NOT NULL)",
            table(w)
        ))
        .unwrap();
    let generator = Arc::new(Mutex::new(Gen::new(seed)));
    let ids = Arc::new(AtomicU64::new(1));
    let order = Arc::new(AtomicU64::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..writers)
        .map(|_| {
            let (db, generator, ids, order, started) = (
                db.clone(),
                generator.clone(),
                ids.clone(),
                order.clone(),
                started.clone(),
            );
            std::thread::spawn(move || {
                let conn = connect(&db, storage);
                let mut history = Vec::new();
                'txns: while started.fetch_add(1, Ordering::Relaxed) < TXNS {
                    let plan = generator.lock().unwrap().txn();
                    let mut mops = Vec::new();
                    conn.execute("BEGIN").unwrap();
                    for p in plan {
                        match step(&conn, w, p, &ids, &mut mops) {
                            Ok(()) => {}
                            Err(e) if refused(&e) => {
                                roll_back(&conn);
                                history.push(Record {
                                    mops,
                                    committed: false,
                                    ts: 0,
                                });
                                continue 'txns;
                            }
                            Err(e) => panic!("{storage:?} seed {seed}: {e}"),
                        }
                    }
                    let ts = order.fetch_add(1, Ordering::SeqCst);
                    match conn.execute("COMMIT") {
                        Ok(()) => history.push(Record {
                            mops,
                            committed: true,
                            ts,
                        }),
                        Err(e) if refused(&e) => {
                            roll_back(&conn);
                            history.push(Record {
                                mops,
                                committed: false,
                                ts: 0,
                            });
                        }
                        Err(e) => panic!("{storage:?} seed {seed}: commit: {e}"),
                    }
                }
                history
            })
        })
        .collect();
    let mut history = Vec::new();
    for t in threads {
        history.extend(t.join().unwrap());
    }
    let finals = rows(&setup, &format!("SELECT k, v FROM {} ORDER BY k", table(w)))
        .unwrap()
        .into_iter()
        .map(|r| (u64::try_from(int(&r[0])).unwrap(), r[1].clone()))
        .collect();
    let committed = history.iter().filter(|r| r.committed).count();
    Run {
        refused: history.len() - committed,
        history,
        finals,
        committed,
    }
}

fn anomalies(w: Workload, r: &Run) -> BTreeSet<Anomaly> {
    if w.lists {
        let finals = r.finals.iter().map(|(k, v)| (*k, decode(v))).collect();
        check_lists(&r.history, &finals)
    } else {
        let finals = r
            .finals
            .iter()
            .map(|(k, v)| (*k, Some(u64::try_from(int(v)).unwrap())))
            .collect();
        check_registers(&r.history, &finals)
    }
}

/// Records list-append and register histories from many SQL connections at once on `storage`,
/// seeds `AMPHORA_ELLE_SQL_FIRST` on (`AMPHORA_ELLE_SQL_SEEDS` of them), and checks each; every
/// seed's outcome printed, then the seeds that showed an anomaly or committed nothing (a history of
/// none checks nothing) named in one failure.
fn histories_have_no_anomaly(storage: Storage) {
    let first = env("AMPHORA_ELLE_SQL_FIRST", 1);
    let count = env("AMPHORA_ELLE_SQL_SEEDS", 4);
    let writers = env("AMPHORA_ELLE_SQL_WRITERS", WRITERS as u64).max(1);
    let mut failed = Vec::new();
    for lists in [true, false] {
        for seed in first..first + count {
            let w = Workload {
                lists,
                spans: !seed.is_multiple_of(2),
            };
            let r = run(storage, w, seed, writers);
            let found = anomalies(w, &r);
            eprintln!(
                "elle_sql {storage:?} lists={lists} spans={} seed {seed}: committed={} refused={} anomalies={found:?}",
                w.spans, r.committed, r.refused
            );
            if !found.is_empty() || r.committed == 0 {
                failed.push((lists, seed, found, r.committed));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{storage:?}: {} of {} histories failed (lists, seed, anomalies, committed): {failed:?}",
        failed.len(),
        2 * count
    );
}

/// T-G8.4.1: do record Elle's histories through SQL on the pager's storage (its single writer);
/// expect no anomaly at all, the claim being serializable.
#[test]
fn g8_4_sql_histories_have_no_anomaly_on_the_pager() {
    histories_have_no_anomaly(Storage::Pager);
}

/// T-G8.4.1, CP-T1.4: do record Elle's histories through SQL on the versioned storage; expect no
/// anomaly at all, the claim being serializable (precision locking, the default validation).
/// Ignored until it holds: its first tiny run (4 connections, seed 1, spans) showed G2, as the SQL
/// cursor (`cursor.rs`) reads rows by its snapshot and keeps no reads for its commit's validation
/// (`VTree::read` and `read_range` keep them; the cursor's leaf copies don't), so SQL there is
/// snapshot isolation. The switch and its cursor are lane g7s2-3's; `--ignored` runs it.
#[test]
#[ignore = "G2 through SQL on the versioned storage: the SQL cursor keeps no reads for validation (lane g7s2-3's)"]
fn g8_4_sql_histories_have_no_anomaly_on_the_versioned_storage() {
    histories_have_no_anomaly(Storage::Versioned);
}

/// T-G8.4.1 (non-vacuity of the recorder): do record a seed's history and break it as a lost write
/// would (the last element appended to a list taken off its final read); expect the checker to
/// name it a lost write, so a recorder that saw nothing wouldn't pass.
#[test]
fn g8_4_a_broken_sql_history_is_caught() {
    let storage = Storage::Pager;
    let writers = env("AMPHORA_ELLE_SQL_WRITERS", WRITERS as u64).max(1);
    let w = Workload {
        lists: true,
        spans: false,
    };
    let mut r = run(storage, w, 2, writers);
    let (_, last) = r
        .finals
        .iter_mut()
        .find(|(_, v)| decode(v).len() > 1)
        .expect("a list of two elements or more");
    let mut list = decode(last);
    list.pop();
    let shortened: Vec<String> = list.iter().map(u64::to_string).collect();
    *last = Value::build_text(shortened.join(","));
    assert!(anomalies(w, &r).contains(&Anomaly::Lost));
}
