//! T-T1.3.1 on the versioned storage's log (Amphora's docs/design/engine.md, decisions 6 to 8 and
//! 10): crash at every write, then recovery. Each seed draws a workload: a log of one lane (one
//! group-committed log) or of two to four (per-thread logs), commits of puts and removes on lanes
//! taken in turn, some durable, checkpoints, and transactions left open across checkpoints, then
//! committed or rolled back. The workload runs once to count the disk operations it makes, then
//! again on a fresh disk for each of them, crashing there; the power is cut (each unflushed write
//! kept whole, torn at a sector or lost, as the seed draws) and the database reopened. Passes when
//! the rows after each reopening are exactly a prefix of the commits in commit order, at least as
//! long as the last commit acknowledged durable: acknowledged commits lost is exactly 0.
//!
//! Every gate runs 200 seeds; the 10,000 are `cargo test --release -p amphora-engine --test
//! versioned_crash -- --ignored` (from another seed with AMPHORA_CRASH_FROM).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use amphora_engine::storage::versioned::blink::{Comparator, Descender};
use amphora_engine::storage::versioned::disk::{Disk, Simulated};
use amphora_engine::storage::versioned::durable::Database;
use amphora_engine::storage::versioned::version::Txn;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A seeded generator (xorshift64*): a seed's workload is its own.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn cmp() -> Comparator {
    Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
}

/// Frames for a workload's rows: they fit a few pages, and splits make a few more.
const FRAMES: usize = 256;

type Rows = BTreeMap<Vec<u8>, Vec<u8>>;
type Changes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// What a run of a seed's workload did, up to its crash.
struct Run {
    /// Each commit's changes, in commit order, as far as the workload got.
    commits: Vec<Changes>,
    /// Commits acknowledged durable: the shortest prefix recovery may return.
    acknowledged: usize,
    checkpoints: u64,
}

/// The seed's workload on `disk`, stopping at the first refusal (the crash).
fn workload(seed: u64, disk: &Arc<Simulated>, crash: Option<u64>) -> Run {
    let mut rng = Rng::new(seed);
    let db = Database::open(disk.clone(), FRAMES, 1, cmp).unwrap();
    let taken: Vec<_> = (0..disk.lanes()).map(|_| db.lane()).collect();
    if let Some(at) = crash {
        disk.crash_after(at);
    }
    let mut run = Run {
        commits: Vec::new(),
        acknowledged: 0,
        checkpoints: 0,
    };
    let mut d = Descender::new();
    let commits = 10 + rng.below(30);
    // A transaction left open across checkpoints, its keys apart from the others'.
    let mut open: Option<(Txn, Changes)> = None;
    for i in 0..commits {
        let lane = taken[usize::try_from(rng.below(taken.len() as u64)).unwrap()];
        if open.is_none() && rng.below(6) == 0 {
            let Ok(mut t) = db.begin() else { return run };
            let mut changes = Changes::new();
            for _ in 0..1 + rng.below(3) {
                let k = vec![200, u8::try_from(rng.below(8)).unwrap()];
                let v = vec![u8::try_from(i % 256).unwrap(); 1 + rng.below(40) as usize];
                if db.put(&mut d, &mut t, 0, &k, &v).is_err() {
                    return run;
                }
                changes.insert(k, Some(v));
            }
            open = Some((t, changes));
        }
        let Ok(mut t) = db.begin() else { return run };
        let mut changes = Changes::new();
        for _ in 0..1 + rng.below(5) {
            let k = vec![u8::try_from(rng.below(48)).unwrap()];
            if rng.below(4) == 0 {
                if db.remove(&mut d, &mut t, 0, &k).is_err() {
                    return run;
                }
                changes.insert(k, None);
            } else {
                let v = vec![u8::try_from(i % 256).unwrap(); 1 + rng.below(60) as usize];
                if db.put(&mut d, &mut t, 0, &k, &v).is_err() {
                    return run;
                }
                changes.insert(k, Some(v));
            }
        }
        let durable = rng.below(3) != 0;
        run.commits.push(changes);
        match db.commit(t, &mut d, lane, durable) {
            Ok(_) if durable => run.acknowledged = run.commits.len(),
            Ok(_) => {}
            Err(_) => return run,
        }
        if rng.below(8) == 0 {
            if db.checkpoint().is_err() {
                return run;
            }
            run.checkpoints += 1;
        }
        if open.is_some() && rng.below(4) == 0 {
            let (t, changes) = open.take().unwrap();
            if rng.below(2) == 0 {
                run.commits.push(changes);
                if db.commit(t, &mut d, lane, false).is_err() {
                    return run;
                }
            } else if db.abort(t, &mut d).is_err() {
                return run;
            }
        }
    }
    if let Some((t, _)) = open {
        if db.abort(t, &mut d).is_err() {
            return run;
        }
    }
    if db.sync().is_ok() {
        run.acknowledged = run.commits.len();
    }
    run
}

fn rows(db: &Database) -> Rows {
    let mut d = Descender::new();
    let snap = db.snapshot().unwrap();
    let all = db.scan(&mut d, &snap, 0, None, usize::MAX).unwrap();
    db.close_snapshot(snap);
    all.into_iter().collect()
}

/// What the seeds saw, so a run can show its checks weren't vacuous.
#[derive(Default)]
struct Seen {
    recoveries: AtomicU64,
    /// Recoveries with more than one lane.
    laned: AtomicU64,
    /// Recoveries that lost commits never acknowledged (the power cut kept less than was logged).
    short: AtomicU64,
    checkpoints: AtomicU64,
    acknowledged: AtomicU64,
    lost: AtomicU64,
}

/// Seed `seed`: its workload crashed at every operation it makes, each recovered and checked.
fn one_seed(seed: u64, seen: &Seen) -> Result<(), String> {
    let mut rng = Rng::new(seed ^ 0x5eed);
    let lanes = 1 + usize::try_from(rng.below(4)).unwrap();
    let dry = Arc::new(Simulated::new(seed, lanes).unwrap());
    workload(seed, &dry, None);
    let ops = dry.operations();
    for at in 0..=ops {
        let disk =
            Arc::new(Simulated::new(seed.wrapping_mul(ops + 1).wrapping_add(at), lanes).unwrap());
        let run = workload(seed, &disk, Some(at));
        disk.power_cut().unwrap();
        let db = Database::open(disk.clone(), FRAMES, 1, cmp).map_err(|e| {
            format!("crash at {at} of {ops}, {lanes} lanes: reopening refused: {e}")
        })?;
        let got = rows(&db);
        let mut state = Rows::new();
        let mut n = if got == state { Some(0) } else { None };
        for (i, c) in run.commits.iter().enumerate() {
            for (k, v) in c {
                match v {
                    Some(v) => state.insert(k.clone(), v.clone()),
                    None => state.remove(k),
                };
            }
            if got == state {
                n = Some(i + 1);
            }
        }
        let Some(n) = n else {
            return Err(format!(
                "crash at {at} of {ops}, {lanes} lanes: the rows match no prefix of the commits"
            ));
        };
        seen.recoveries.fetch_add(1, Ordering::Relaxed);
        seen.checkpoints
            .fetch_add(run.checkpoints, Ordering::Relaxed);
        seen.acknowledged
            .fetch_add(run.acknowledged as u64, Ordering::Relaxed);
        if lanes > 1 {
            seen.laned.fetch_add(1, Ordering::Relaxed);
        }
        if n < run.commits.len() {
            seen.short.fetch_add(1, Ordering::Relaxed);
        }
        if n < run.acknowledged {
            seen.lost
                .fetch_add((run.acknowledged - n) as u64, Ordering::Relaxed);
            return Err(format!(
                "crash at {at} of {ops}, {lanes} lanes: {n} commits recovered, {} acknowledged",
                run.acknowledged
            ));
        }
    }
    Ok(())
}

/// `n` seeds from `from`, side by side on as many threads as the machine runs: every one passes,
/// and between them they recover from crashes on one lane and on several, mid-checkpoint and with
/// commits never acknowledged lost.
fn seeds(n: u64, from: u64) {
    let seen = Seen::default();
    let next = AtomicU64::new(from);
    let failed = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let seed = next.fetch_add(1, Ordering::Relaxed);
                if seed >= from + n {
                    return;
                }
                if let Err(e) = one_seed(seed, &seen) {
                    failed.lock().unwrap().push((seed, e));
                }
            });
        }
    });
    let mut failed = failed.into_inner().unwrap();
    failed.sort();
    let recoveries = seen.recoveries.load(Ordering::Relaxed);
    let lost = seen.lost.load(Ordering::Relaxed);
    println!(
        "{n} seeds: {recoveries} crashes recovered ({} on several lanes, {} short of what was \
         logged); {} checkpoints; {} acknowledged commits checked; acknowledged commits lost: {lost}",
        seen.laned.load(Ordering::Relaxed),
        seen.short.load(Ordering::Relaxed),
        seen.checkpoints.load(Ordering::Relaxed),
        seen.acknowledged.load(Ordering::Relaxed),
    );
    assert!(
        failed.is_empty(),
        "{} of {n} seeds failed; the first: seed {} ({})",
        failed.len(),
        failed[0].0,
        failed[0].1
    );
    assert_eq!(lost, 0);
    assert!(
        seen.laned.load(Ordering::Relaxed) > 0
            && seen.laned.load(Ordering::Relaxed) < recoveries
            && seen.short.load(Ordering::Relaxed) > 0
            && seen.checkpoints.load(Ordering::Relaxed) > 0
            && seen.acknowledged.load(Ordering::Relaxed) > 0,
        "a vacuous run: no lanes, no losses, no checkpoints or nothing acknowledged"
    );
}

/// T-T1.3.1, a sample in every gate: 200 seeds, each crashed at every operation it makes.
#[test]
fn t_t1_3_1_a_crash_at_every_write_loses_no_acknowledged_commit() {
    seeds(200, 1);
}

/// T-T1.3.1 in full: 10,000 seeds (`cargo test --release -p amphora-engine --test versioned_crash
/// -- --ignored`; from another seed with AMPHORA_CRASH_FROM).
#[test]
#[ignore = "10,000 seeds: run on its own, in release"]
fn t_t1_3_1_ten_thousand_seeds() {
    let from = std::env::var("AMPHORA_CRASH_FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    seeds(10_000, from);
}
