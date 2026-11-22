//! G7 step 4's tests of the pool's eviction on a database (plan "G7 The executor on the new
//! storage", step 4): a database many times its pool read and written through evictions and
//! checkpoints, crash injection at each write of an eviction and of a checkpoint, and the model
//! test of the pool with eviction under many threads.

use super::blink::{Comparator, Descender};
use super::disk::{Disk, Simulated};
use super::durable::Database;
use super::pool::Pool;
use crate::error::LimboError;
use crate::sync::Arc;
use std::collections::BTreeMap;

fn cmp() -> Comparator {
    Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
}

/// A commit's changes: each key's new value, or None for a removal.
type Changes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

fn rows(db: &Database) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut d = Descender::new();
    let snap = db.snapshot().unwrap();
    let all = db.scan(&mut d, &snap, 0, None, usize::MAX).unwrap();
    db.close_snapshot(snap);
    all.into_iter().collect()
}

/// Frames for the tests' pools: enough for a B-link change's latches at once, and the version
/// records' budget the pool's pages also bound (decision 10); the databases are many times more.
const FRAMES: usize = 24;

fn key(i: u64) -> Vec<u8> {
    (i.wrapping_mul(0x9e37_79b9) % 1_000_003)
        .to_be_bytes()
        .to_vec()
}

fn value(i: u64, round: u64) -> Vec<u8> {
    let len = 200 + usize::try_from((i * 31 + round) % 300).unwrap();
    vec![u8::try_from((i + round) % 251).unwrap(); len]
}

/// Do: in a pool of 24 frames, commit 2,000 rows of 200 to 500 bytes, checkpoint halfway, rewrite
/// a third of them; read them all; reopen in a pool as small and read them again. Expect: every
/// row as last committed both times; the database's pages at least ten times the pool's frames,
/// pages spilled and read back (T-NS.4.2, AC-NS.4.1).
#[test]
fn g7s4_a_database_ten_times_its_pool_reads_and_writes_correctly() {
    let disk = Arc::new(Simulated::new(5, 1).unwrap());
    let mut want = BTreeMap::new();
    {
        let db = Database::open(disk.clone(), FRAMES, 1, cmp).unwrap();
        let mut d = Descender::new();
        for chunk in 0..100u64 {
            let mut t = db.begin().unwrap();
            for i in chunk * 20..(chunk + 1) * 20 {
                db.put(&mut d, &mut t, 0, &key(i), &value(i, 0)).unwrap();
                want.insert(key(i), value(i, 0));
            }
            db.commit(t, &mut d, db.lane(), true).unwrap();
            db.collect(&mut d).unwrap();
            if chunk == 50 {
                db.checkpoint().unwrap();
            }
        }
        for chunk in (0..2_000u64).step_by(60) {
            let mut t = db.begin().unwrap();
            for i in (chunk..chunk + 60).step_by(3) {
                db.put(&mut d, &mut t, 0, &key(i), &value(i, 1)).unwrap();
                want.insert(key(i), value(i, 1));
            }
            db.commit(t, &mut d, db.lane(), true).unwrap();
            db.collect(&mut d).unwrap();
        }
        assert_eq!(rows(&db), want);
        let snap = db.snapshot().unwrap();
        for (k, v) in want.iter().step_by(7) {
            assert_eq!(db.get(&mut d, &snap, 0, k).unwrap().as_ref(), Some(v));
        }
        db.close_snapshot(snap);
        let pages = db.v.pool.allocated();
        let c = db.v.pool.counts();
        assert!(
            pages >= 10 * u64::try_from(FRAMES).unwrap(),
            "{pages} pages in {FRAMES} frames"
        );
        assert!(c.spilled > 0 && c.misses > 0 && c.evictions > 0, "{c:?}");
    }
    let db = Database::open(disk, FRAMES, 1, cmp).unwrap();
    assert_eq!(rows(&db), want);
    assert!(db.v.pool.counts().misses > 0);
}

/// The crash workload on a pool of [`FRAMES`]: commits of puts and removes over a database many
/// times the pool, some durable, checkpoints every few commits, on `disk` crashing where it was
/// told. The commits as made, and how many were acknowledged durable.
fn crash_workload(disk: &Arc<Simulated>, crash: Option<u64>) -> (Vec<Changes>, usize, u64) {
    let mut commits = Vec::new();
    let mut acknowledged = 0usize;
    let db = Database::open(disk.clone(), FRAMES, 1, cmp).unwrap();
    if let Some(c) = crash {
        disk.crash_after(c);
    }
    let mut d = Descender::new();
    let mut seed = crash.unwrap_or(0) | 1;
    'run: for i in 0..40u64 {
        let mut t = db.begin().unwrap();
        let mut changes = BTreeMap::new();
        for _ in 0..12 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let k = key((seed >> 33) % 300);
            if (seed >> 60).is_multiple_of(5) {
                if db.remove(&mut d, &mut t, 0, &k).is_err() {
                    break 'run;
                }
                changes.insert(k, None);
            } else {
                let v = value(seed >> 40, i);
                if db.put(&mut d, &mut t, 0, &k, &v).is_err() {
                    break 'run;
                }
                changes.insert(k, Some(v));
            }
        }
        commits.push(changes);
        match db.commit(t, &mut d, db.lane(), !i.is_multiple_of(3)) {
            Ok(_) if !i.is_multiple_of(3) => acknowledged = commits.len(),
            Ok(_) => {}
            Err(_) => break 'run,
        }
        if i % 8 == 7 && db.checkpoint().is_err() {
            break 'run;
        }
    }
    let spilled = db.v.pool.counts().spilled;
    (commits, acknowledged, spilled)
}

/// Do: the crash workload once to count its disk operations (its evictions' spill writes among
/// them), then again on a fresh disk crashing at each operation in turn: each write of an
/// eviction and of a checkpoint; cut the power and reopen. Expect: the rows after each reopen
/// exactly a prefix of the commits, at least as long as the last acknowledged durable: no
/// acknowledged write lost (T-NS.4.1, T-T1.3.1).
#[test]
fn g7s4_a_crash_at_each_write_of_evictions_and_checkpoints_loses_no_acknowledged_write() {
    let probe = Arc::new(Simulated::new(1, 1).unwrap());
    let (_, _, spilled) = crash_workload(&probe, None);
    let ops = probe.operations();
    assert!(spilled > 0, "the workload evicted nothing dirty");
    let mut checked = 0;
    for crash in 1..ops {
        let disk = Arc::new(Simulated::new(crash, 1).unwrap());
        let (commits, acknowledged, _) = crash_workload(&disk, Some(crash));
        disk.power_cut().unwrap();
        let db = Database::open(disk.clone(), FRAMES, 1, cmp)
            .unwrap_or_else(|e| panic!("crash at {crash} of {ops}: reopen: {e}"));
        let got = rows(&db);
        let mut state: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut prefixes = vec![state.clone()];
        for c in &commits {
            for (k, v) in c {
                match v {
                    Some(v) => state.insert(k.clone(), v.clone()),
                    None => state.remove(k),
                };
            }
            prefixes.push(state.clone());
        }
        let n = prefixes
            .iter()
            .position(|p| p == &got)
            .unwrap_or_else(|| panic!("crash at {crash} of {ops}: rows match no prefix"));
        assert!(
            n >= acknowledged,
            "crash at {crash} of {ops}: {n} commits recovered, {acknowledged} acknowledged"
        );
        checked += 1;
    }
    assert!(checked > 100, "{checked} crash points");
}

fn pattern(page: u64, round: u64, j: usize) -> u8 {
    u8::try_from((page.wrapping_mul(131) ^ round.wrapping_mul(7) ^ j as u64) & 0xff).unwrap()
}

/// Do: 32 threads on a pool of 8 frames over 64 pages, each thread owning two pages it rewrites
/// with its round's pattern and reads back, and reading others' pages; a refusal because every
/// frame was latched is retried. Expect: an owner always reads its last round; every page read
/// is whole, one round's pattern from end to end (no stale or torn page, no read of a frame
/// after its page left); evictions happened (T-NS.4.3).
#[test]
fn g7s4_the_pool_model_test_with_eviction_under_32_threads() {
    let disk: Arc<dyn Disk> = Arc::new(Simulated::new(9, 1).unwrap());
    let pool = Pool::backed(8, disk, 0).unwrap();
    let pages: Vec<u64> = (0..64).map(|_| pool.allocate().unwrap()).collect();
    for &p in &pages {
        let mut w = pool.write(p).unwrap();
        for (j, b) in w.iter_mut().enumerate() {
            *b = pattern(p, 0, j);
        }
    }
    let whole = |p: u64, bytes: &[u8]| -> Option<u64> {
        (0..64u64).find(|&r| {
            bytes
                .iter()
                .enumerate()
                .all(|(j, b)| *b == pattern(p, r, j))
        })
    };
    std::thread::scope(|s| {
        for t in 0..32usize {
            let (pool, pages, whole) = (&pool, &pages, &whole);
            s.spawn(move || {
                let mine = [pages[t * 2], pages[t * 2 + 1]];
                let mut seed = u64::try_from(t).unwrap() + 1;
                for round in 1..40u64 {
                    for &p in &mine {
                        loop {
                            match pool.write(p) {
                                Ok(mut w) => {
                                    for (j, b) in w.iter_mut().enumerate() {
                                        *b = pattern(p, round, j);
                                    }
                                    break;
                                }
                                Err(LimboError::MemoryBudget { .. }) => std::thread::yield_now(),
                                Err(e) => panic!("write {p}: {e}"),
                            }
                        }
                    }
                    for &p in &mine {
                        loop {
                            match pool.read(p) {
                                Ok(r) => {
                                    assert_eq!(whole(p, &r), Some(round), "own page {p}");
                                    break;
                                }
                                Err(LimboError::MemoryBudget { .. }) => std::thread::yield_now(),
                                Err(e) => panic!("read {p}: {e}"),
                            }
                        }
                    }
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let other = pages[usize::try_from((seed >> 33) % 64).unwrap()];
                    match pool.read(other) {
                        Ok(r) => assert!(whole(other, &r).is_some(), "torn page {other}"),
                        Err(LimboError::MemoryBudget { .. }) => {}
                        Err(e) => panic!("read {other}: {e}"),
                    }
                }
            });
        }
    });
    for (t, pair) in pages.chunks(2).enumerate() {
        for &p in pair {
            assert_eq!(
                whole(p, &pool.read(p).unwrap()),
                Some(39),
                "thread {t} page {p}"
            );
        }
    }
    let c = pool.counts();
    assert!(c.evictions > 64 && c.spilled > 0, "{c:?}");
}
