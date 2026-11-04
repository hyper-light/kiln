//! The pools observed (kiln's departure, ORIGIN.md; goal 3 of kiln's API runtime program, plan 01a1a59c2a8e
//! steps 3 and 4), in one process because each pool is one per process: the blocking pool tells its observer
//! each job's share, how long it waited for one of the share's places and how long it ran, as the job ends;
//! the compute pool tells how long each job waited for a thread and how long each piece ran. Each wait and
//! run here is held open by the test for a known time, so each reading has a floor it can't be under.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cognitive_complexity
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use hyper_rt::blocking::{self, Share};
use hyper_rt::compute::{self, Observed, Rank, Step};
use hyper_rt::sync::OneshotReceiver;

/// How long the test holds a job open: what the readings must be at least.
const HOLD: Duration = Duration::from_millis(30);
/// How long a check waits for what must come before it calls the pool stuck.
const PATIENCE: Duration = Duration::from_secs(20);

/// The receiver's value, polled from this plain thread until it comes or [`PATIENCE`] passes.
fn within<T>(receiver: &mut OneshotReceiver<T>) -> Option<T> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match receiver.try_recv() {
            Ok(Some(value)) => return Some(value),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            _ => return None,
        }
    }
}

static HELD_JOBS: AtomicU64 = AtomicU64::new(0);
static HELD_MOST_WAITED_NS: AtomicU64 = AtomicU64::new(0);
static HELD_MOST_RAN_NS: AtomicU64 = AtomicU64::new(0);

fn noted(share: &'static str, waited_ns: u64, ran_ns: u64) {
    if share == "held" {
        HELD_JOBS.fetch_add(1, Ordering::Relaxed);
        HELD_MOST_WAITED_NS.fetch_max(waited_ns, Ordering::Relaxed);
        HELD_MOST_RAN_NS.fetch_max(ran_ns, Ordering::Relaxed);
    }
}

static WAITS: AtomicU64 = AtomicU64::new(0);
static MOST_WAITED_NS: AtomicU64 = AtomicU64::new(0);
static PIECES: AtomicU64 = AtomicU64::new(0);
static LONGEST_PIECE_NS: AtomicU64 = AtomicU64::new(0);

fn seen(observed: Observed) {
    match observed {
        Observed::Waited(ns) => {
            WAITS.fetch_add(1, Ordering::Relaxed);
            MOST_WAITED_NS.fetch_max(ns, Ordering::Relaxed);
        }
        Observed::Ran(ns) => {
            PIECES.fetch_add(1, Ordering::Relaxed);
            LONGEST_PIECE_NS.fetch_max(ns, Ordering::Relaxed);
        }
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap()
}

/// A share of one place: a job held open `HOLD` runs at least that long, and the job behind it waits at least
/// that long for its place; both told, with their share's name.
#[test]
fn the_blocking_pool_tells_each_jobs_wait_and_run() {
    blocking::start(blocking::Config {
        workers: 2,
        shares: vec![Share { name: "held", limit: 1, waiting: 1 }],
    })
    .unwrap();
    assert!(blocking::observe(noted));
    assert!(!blocking::observe(noted), "one observer per process");
    let (started_tx, started) = sync_channel::<()>(1);
    let (release, hold) = sync_channel::<()>(1);
    let mut first = blocking::run("held", move || {
        started_tx.send(()).unwrap();
        hold.recv().unwrap();
    })
    .unwrap();
    started.recv().unwrap();
    let mut second = blocking::run("held", || ()).unwrap();
    std::thread::sleep(HOLD);
    release.send(()).unwrap();
    assert!(within(&mut first).is_some() && within(&mut second).is_some());
    // Told as each job ends, before its result is sent: both are in by now.
    assert_eq!(HELD_JOBS.load(Ordering::Relaxed), 2);
    assert!(HELD_MOST_RAN_NS.load(Ordering::Relaxed) >= nanos(HOLD), "the held job ran at least as long as it was held");
    assert!(HELD_MOST_WAITED_NS.load(Ordering::Relaxed) >= nanos(HOLD), "the job behind it waited at least as long for its place");
}

/// A pool of one thread: a job held open `HOLD` is a piece that long, the job behind it waits at least that
/// long for the thread, and a job in pieces tells each of its pieces.
#[test]
fn the_compute_pool_tells_each_jobs_wait_and_each_piece() {
    compute::start(compute::Config { workers: 1, queued: 8, piece_ns: 0, rank: Rank::Below }).unwrap();
    assert!(compute::observe(seen));
    assert!(!compute::observe(seen), "one observer per process");
    let (started_tx, started) = sync_channel::<()>(1);
    let (release, hold) = sync_channel::<()>(1);
    let mut first = compute::run(move || {
        started_tx.send(()).unwrap();
        hold.recv().unwrap();
    })
    .unwrap();
    started.recv().unwrap();
    let mut second = compute::run(|| ()).unwrap();
    std::thread::sleep(HOLD);
    release.send(()).unwrap();
    assert!(within(&mut first).is_some() && within(&mut second).is_some());
    // Three pieces of one job: each is told.
    let pieces_before = PIECES.load(Ordering::Relaxed);
    let mut third = compute::run_pieces(0u32, |n, _turn| {
        *n += 1;
        if *n == 3 { Step::Done(*n) } else { Step::More }
    })
    .map_err(|refused| refused.error)
    .unwrap();
    assert_eq!(within(&mut third), Some(3));
    // A job's wait is told as its first piece starts, before its code runs: all three are in.
    assert_eq!(WAITS.load(Ordering::Relaxed), 3, "each job's wait, once");
    assert!(MOST_WAITED_NS.load(Ordering::Relaxed) >= nanos(HOLD), "the job behind the held one waited at least as long for the thread");
    assert!(LONGEST_PIECE_NS.load(Ordering::Relaxed) >= nanos(HOLD), "the held job was one piece at least that long");
    // A piece is told as it ends, after a last piece has sent its result: the last may come just after.
    let deadline = Instant::now() + PATIENCE;
    while PIECES.load(Ordering::Relaxed) - pieces_before < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    // (At least: the job before it may have told its one piece just after its result, too.)
    assert!(PIECES.load(Ordering::Relaxed) - pieces_before >= 3, "each piece of the job in pieces");
}
