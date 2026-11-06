//! The compute pool's and the blocking pool's work on `/metrics` (goal 3 of the API runtime program, plan
//! 01a1a59c2a8e steps 3 and 4): where the heavy work went and how long its pieces held a pool thread, and what
//! each of the blocking pool's shares does, which its size is derived from (`crate::rt::Profile`).
//!
//! kiln-rt tells of each job as it goes (`hyper_rt::compute::observe`, `hyper_rt::blocking::observe`, set as
//! the runtime starts: `crate::rt`). The histograms are atomics, so a pool's thread notes what it did without
//! a lock, and `/metrics` reads them without asking a pool anything.
//!
//! - **The compute pool** (`kiln_compute_*`): its jobs (`_jobs_total`, `_refused_total`), how long each waited
//!   for a thread (`_wait_seconds`), its pieces (`_pieces_total`; `_yielded_total`, those after which their
//!   job gave its thread to a waiting one), how long each piece ran (`_piece_seconds`) and the longest yet
//!   (`_longest_piece_seconds`); and what handing it work costs (goal 9, plan 01a1a87b2cec step 5): its workers
//!   woken (`_unparks_total`, each a system call on the thread that queued the job) and gone to park
//!   (`_parks_total`).
//! - **The blocking pool's shares** (`kiln_blocking_*{share}`): how long each job waited for one of its
//!   share's places (`_wait_seconds`), how long it ran (`_run_seconds`), and the share's places (`_places`).
//!   By Little's law a share holds, on average, the rate its jobs come at times the time each takes: the lab
//!   reads both (`docs/runtime-lab.md`).

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use hyper_rt::compute::Observed;

use crate::metrics::{Histogram, STALL_BUCKETS};

/// The blocking pool's shares, as `crate::rt` names them, in the order of their histograms.
const SHARES: [&str; 3] = [hyper_rt::dns::SHARE, crate::rt::SHARE_STORE, crate::rt::SHARE_FS];

/// kiln.db's writer (`crate::store`), as its series name it beside the blocking pool's shares: a thread
/// of its own, one place, its calls taken in turn (goal 5 of the API runtime program, plan 01a1a59c2aae).
pub const STORE_WRITE: &str = "store_write";

struct Pools {
    compute_wait: Histogram,
    compute_piece: Histogram,
    blocking_wait: [Histogram; SHARES.len()],
    blocking_run: [Histogram; SHARES.len()],
    store_write_wait: Histogram,
    store_write_run: Histogram,
    /// kiln.db's group commits, and its WAL's checkpoints on their own thread and on the writer's.
    store_commit: Histogram,
    store_checkpoint: Histogram,
    store_checkpoint_writer: Histogram,
}

fn pools() -> &'static Pools {
    static P: OnceLock<Pools> = OnceLock::new();
    P.get_or_init(|| Pools {
        compute_wait: Histogram::new(STALL_BUCKETS),
        compute_piece: Histogram::new(STALL_BUCKETS),
        blocking_wait: std::array::from_fn(|_| Histogram::new(STALL_BUCKETS)),
        blocking_run: std::array::from_fn(|_| Histogram::new(STALL_BUCKETS)),
        store_write_wait: Histogram::new(STALL_BUCKETS),
        store_write_run: Histogram::new(STALL_BUCKETS),
        store_commit: Histogram::new(STALL_BUCKETS),
        store_checkpoint: Histogram::new(STALL_BUCKETS),
        store_checkpoint_writer: Histogram::new(STALL_BUCKETS),
    })
}

/// The calls kiln.db's writer committed in its groups, all told (goal 5, plan 01a1a59c2aae): over
/// `kiln_store_commit_seconds_count`, the calls a commit.
static STORE_COMMITTED_CALLS: AtomicU64 = AtomicU64::new(0);

/// A group of calls kiln.db's writer committed together, in one transaction (goal 5, plan 01a1a59c2aae):
/// how many, and how long its commit took (its number kept with the cluster group, `COMMIT`, its changes
/// handed to the log and its callers told).
pub fn store_commit(calls: usize, took: std::time::Duration) {
    STORE_COMMITTED_CALLS.fetch_add(u64::try_from(calls).unwrap_or(u64::MAX), Ordering::Relaxed);
    pools().store_commit.observe(took.as_secs_f64());
}

/// A checkpoint of kiln.db's WAL (goal 5, plan 01a1a59c2aae), how long it took: on its own thread, or
/// (`writer`) on the writer's, between two groups, the WAL past its bound (`crate::store`'s `Checkpoints`).
pub fn store_checkpoint(writer: bool, took: std::time::Duration) {
    let p = pools();
    (if writer { &p.store_checkpoint_writer } else { &p.store_checkpoint }).observe(took.as_secs_f64());
}

/// Calls sent to kiln.db's writer not yet started: what the overload detector reads of whether one waits
/// ([`queues`]), as kiln-rt's occupancy says it of a pool's.
static STORE_WRITE_WAITING: AtomicI64 = AtomicI64::new(0);

/// When kiln.db's writer last finished a call, and when its line last formed (a call sent while none waited), on
/// [`clock_ms`]'s clock (0: never): what readiness reads of whether the writer keeps up ([`store_writer_stuck`]).
static STORE_WRITE_ENDED_MS: AtomicU64 = AtomicU64::new(0);
static STORE_WRITE_FORMED_MS: AtomicU64 = AtomicU64::new(0);

/// How long kiln.db's writer may finish nothing while calls wait for it before readiness says the store is stuck
/// (goal 6, plan 01a1a59c2abe step 3: `/readyz` answered from memory): as long as the longest any prober kiln
/// configures waits for `/readyz`. KilnCD's checks give one 30 s where it names no timeout (`crate::cd::guard`;
/// `.kiln/deploy.yaml`'s `serving` names none and rolls back on one failure), and the Kubernetes probe kiln installs
/// waits 3 s (`crate::install_targets`). The `SELECT 1` `/readyz` sent through the writer before failed a prober
/// whenever the writer's line took longer than its wait to reach it, moving or not; a writer that finished nothing
/// for the longest of them would have failed every one, so no prober sees the store unready where it saw it ready.
pub const WRITER_STUCK: std::time::Duration = std::time::Duration::from_secs(30);

/// Milliseconds since this clock was first read, plus one, so 0 says never.
fn clock_ms() -> u64 {
    static BASE: OnceLock<std::time::Instant> = OnceLock::new();
    let base = *BASE.get_or_init(std::time::Instant::now);
    u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX).saturating_add(1)
}

/// A call was sent to kiln.db's writer (`1`), or the writer started one (`-1`).
pub fn store_write_queued(delta: i64) {
    let before = STORE_WRITE_WAITING.fetch_add(delta, Ordering::Relaxed);
    if delta > 0 && before <= 0 {
        STORE_WRITE_FORMED_MS.store(clock_ms(), Ordering::Relaxed);
    }
}

/// A call kiln.db's writer ran: how long it waited for its turn, from being sent, and how long it ran.
pub fn store_write(waited: std::time::Duration, ran: std::time::Duration) {
    let p = pools();
    p.store_write_wait.observe(waited.as_secs_f64());
    p.store_write_run.observe(ran.as_secs_f64());
    longest(QUEUES.len() - 1, u64::try_from(waited.as_micros()).unwrap_or(u64::MAX));
    STORE_WRITE_ENDED_MS.store(clock_ms(), Ordering::Relaxed);
}

/// Whether kiln.db's writer is stuck (goal 6, plan 01a1a59c2abe step 3; `/readyz`): calls wait for it and it has
/// finished none for [`WRITER_STUCK`] since their line formed. How many wait, and how long it has finished none; none
/// while it keeps up, however long its line, and while nothing waits.
pub fn store_writer_stuck() -> Option<(u64, std::time::Duration)> {
    stuck(STORE_WRITE_WAITING.load(Ordering::Relaxed), STORE_WRITE_FORMED_MS.load(Ordering::Relaxed), STORE_WRITE_ENDED_MS.load(Ordering::Relaxed), clock_ms(), WRITER_STUCK)
}

/// [`store_writer_stuck`] from `waiting` calls, the line `formed_ms`, the last call `ended_ms` and `now_ms`, stuck
/// once it has finished none for `after`.
fn stuck(waiting: i64, formed_ms: u64, ended_ms: u64, now_ms: u64, after: std::time::Duration) -> Option<(u64, std::time::Duration)> {
    let waiting = u64::try_from(waiting).ok().filter(|n| *n > 0)?;
    let still = std::time::Duration::from_millis(now_ms.saturating_sub(formed_ms.max(ended_ms)));
    (still >= after).then_some((waiting, still))
}

/// The longest wait each pool of [`QUEUES`] told since the overload detector last took them, microseconds (goal 6,
/// plan 01a1a59c2abe step 4): the oldest of its line's jobs served, which says whether its line stands on work
/// that came after it formed, or on one batch submitted at once being worked off (`crate::overload`).
static LONGEST_US: [AtomicU64; QUEUES.len()] = [const { AtomicU64::new(0) }; QUEUES.len()];

fn longest(pool: usize, waited_us: u64) {
    if let Some(slot) = LONGEST_US.get(pool) {
        slot.fetch_max(waited_us, Ordering::Relaxed);
    }
}

/// The longest wait each pool of [`QUEUES`] told since the last call, microseconds, each taken back to zero as it
/// is read: the overload detector's, the first shard's probe, which alone calls it, once a period.
pub fn take_longest() -> [u64; QUEUES.len()] {
    std::array::from_fn(|i| LONGEST_US.get(i).map_or(0, |slot| slot.swap(0, Ordering::Relaxed)))
}

#[allow(clippy::cast_precision_loss)]
fn secs(ns: u64) -> f64 {
    ns as f64 / 1e9
}

/// What the compute pool told of a job's wait or a piece (`hyper_rt::compute::observe`).
pub fn compute_job(observed: Observed) {
    match observed {
        Observed::Waited(ns) => {
            pools().compute_wait.observe(secs(ns));
            longest(0, ns / 1_000);
        }
        Observed::Ran(ns) => pools().compute_piece.observe(secs(ns)),
    }
}

/// What the blocking pool told of a job of `share` as it ended (`hyper_rt::blocking::observe`).
pub fn blocking_job(share: &'static str, waited_ns: u64, ran_ns: u64) {
    let Some(i) = SHARES.iter().position(|s| *s == share) else { return };
    let p = pools();
    if let (Some(wait), Some(run)) = (p.blocking_wait.get(i), p.blocking_run.get(i)) {
        wait.observe(secs(waited_ns));
        run.observe(secs(ran_ns));
    }
    // The compute pool is `QUEUES`' first; the shares follow in `SHARES`' order.
    longest(i + 1, waited_ns / 1_000);
}

/// What the overload detector reads of one pool (goal 6, plan 01a1a59c2abe step 4; `crate::overload`): the waits
/// told so far, and whether any of its jobs waits now. The waits are the histograms' own count and sum; whether
/// one waits is kiln-rt's count of the jobs a pool holds against its places (`hyper_rt::compute::occupancy`,
/// `hyper_rt::blocking::occupancy`), since a wait is told only once its job starts (the compute pool) or ends
/// (the blocking pool's shares).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    /// Jobs whose wait has been told, and their waits' sum in microseconds.
    pub waits: u64,
    pub waited_us: u64,
    /// Whether a job waits for a place now.
    pub waiting: bool,
}

/// The pools [`queues`] reads, by name, in its order: the compute pool, the blocking pool's shares, then
/// kiln.db's writer, whose backlog of writes sheds as a share's does (goal 5 moved them off the `store`
/// share, which its reads keep).
pub const QUEUES: [&str; 2 + SHARES.len()] = ["compute", SHARES[0], SHARES[1], SHARES[2], STORE_WRITE];

/// Each pool of [`QUEUES`] as it stands: lock-free reads of atomics the pools' threads write as they go.
pub fn queues() -> [Queue; QUEUES.len()] {
    let p = pools();
    let waiting = |o: Option<hyper_rt::blocking::Occupancy>| o.is_some_and(|o| o.waiting() > 0);
    let compute = Queue { waits: p.compute_wait.count(), waited_us: p.compute_wait.sum_us(), waiting: waiting(hyper_rt::compute::occupancy()) };
    let mut out = [compute; QUEUES.len()];
    for ((slot, share), wait) in out.iter_mut().skip(1).zip(SHARES).zip(&p.blocking_wait) {
        *slot = Queue { waits: wait.count(), waited_us: wait.sum_us(), waiting: waiting(hyper_rt::blocking::occupancy(share)) };
    }
    if let Some(slot) = out.last_mut() {
        *slot = Queue { waits: p.store_write_wait.count(), waited_us: p.store_write_wait.sum_us(), waiting: STORE_WRITE_WAITING.load(Ordering::Relaxed) > 0 };
    }
    out
}

/// The `kiln_compute_*` and `kiln_blocking_*` families, in the exposition.
pub fn render(o: &mut String) {
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    // The awaited jobs by where each was asked for (goal 9, plan 01a1a87b2cec step 5).
    crate::compute_sites::render(o);
    let p = pools();
    let s = hyper_rt::compute::stats();
    for (name, kind, h, v) in [
        ("kiln_compute_jobs_total", "counter", "Jobs the compute pool took: CPU-heavy work off the shards.", s.accepted),
        ("kiln_compute_refused_total", "counter", "Jobs the compute pool refused at its bound (each asked again).", s.refused),
        ("kiln_compute_pieces_total", "counter", "Pieces the compute pool's jobs ran (a job of one closure is one).", s.pieces),
        ("kiln_compute_yielded_total", "counter", "Pieces after which their job gave its thread to a waiting job, going on after it.", s.yielded),
        // What handing work to the pool costs (goal 9, plan 01a1a87b2cec step 5).
        ("kiln_compute_unparks_total", "counter", "Compute pool workers woken from their park: each a system call (a futex wake on Linux) on the thread that queued the job, a shard's most often, or on a worker that found more.", s.unparks),
        ("kiln_compute_parks_total", "counter", "Times a compute pool worker went to park with nothing to do.", s.parks),
    ] {
        help(o, name, kind, h);
        let _ = writeln!(o, "{name} {v}");
    }
    help(o, "kiln_compute_longest_piece_seconds", "gauge", "The longest piece any compute pool thread has run.");
    let _ = writeln!(o, "kiln_compute_longest_piece_seconds {}", secs(s.longest_piece_ns));
    help(o, "kiln_compute_wait_seconds", "histogram", "How long each compute job waited for a pool thread: from its submission to its first piece.");
    p.compute_wait.render(o, "kiln_compute_wait_seconds", "");
    help(o, "kiln_compute_piece_seconds", "histogram", "How long each piece of a compute job held its pool thread.");
    p.compute_piece.render(o, "kiln_compute_piece_seconds", "");
    help(o, "kiln_blocking_wait_seconds", "histogram", "How long each blocking pool job waited for one of its share's places (store_write: kiln.db's writer, a thread of its own).");
    for (share, h) in SHARES.iter().zip(&p.blocking_wait) {
        h.render(o, "kiln_blocking_wait_seconds", &format!("share=\"{share}\""));
    }
    p.store_write_wait.render(o, "kiln_blocking_wait_seconds", &format!("share=\"{STORE_WRITE}\""));
    help(o, "kiln_blocking_run_seconds", "histogram", "How long each blocking pool job ran, by share (store_write: kiln.db's writer, a thread of its own).");
    for (share, h) in SHARES.iter().zip(&p.blocking_run) {
        h.render(o, "kiln_blocking_run_seconds", &format!("share=\"{share}\""));
    }
    p.store_write_run.render(o, "kiln_blocking_run_seconds", &format!("share=\"{STORE_WRITE}\""));
    help(o, "kiln_blocking_places", "gauge", "Each of the blocking pool's shares: the most of its jobs that run at once (crate::rt::Profile); store_write, kiln.db's writer: one.");
    let (dns, store, fs) = crate::rt::profile().pool;
    for (share, places) in SHARES.iter().zip([dns, store, fs]).chain([(&STORE_WRITE, 1)]) {
        let _ = writeln!(o, "kiln_blocking_places{{share=\"{share}\"}} {places}");
    }
    help(o, "kiln_store_commit_seconds", "histogram", "How long each of kiln.db's group commits took: the calls waiting for its writer, run in one transaction, then its number kept with the cluster group, COMMIT, its changes handed to the log and its callers told.");
    p.store_commit.render(o, "kiln_store_commit_seconds", "");
    help(o, "kiln_store_committed_calls_total", "counter", "The calls kiln.db's group commits held, all told: over kiln_store_commit_seconds_count, the calls a commit.");
    let _ = writeln!(o, "kiln_store_committed_calls_total {}", STORE_COMMITTED_CALLS.load(Ordering::Relaxed));
    help(o, "kiln_store_checkpoint_seconds", "histogram", "How long each checkpoint of kiln.db's WAL took, by the thread it ran on: its own (checkpoints), or the writer's between two groups, the WAL past its bound (writer).");
    p.store_checkpoint.render(o, "kiln_store_checkpoint_seconds", "on=\"checkpoints\"");
    p.store_checkpoint_writer.render(o, "kiln_store_checkpoint_seconds", "on=\"writer\"");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each pool's jobs, as kiln-rt tells of them, land in their own series: a share's by its name, one
    /// kiln doesn't have nowhere.
    #[test]
    fn each_pools_jobs_land_in_its_series() {
        compute_job(Observed::Waited(3_000_000));
        compute_job(Observed::Ran(200_000));
        blocking_job(crate::rt::SHARE_STORE, 1_000, 40_000_000);
        blocking_job("no such share", 1, 1);
        let mut o = String::new();
        render(&mut o);
        for family in ["kiln_compute_jobs_total", "kiln_compute_unparks_total", "kiln_compute_parks_total", "kiln_compute_wait_seconds_bucket", "kiln_compute_piece_seconds_count", "kiln_blocking_places{share=\"fs\"}"] {
            assert!(o.contains(family), "{family} in {o}");
        }
        let count = |name: &str, share: &str| {
            o.lines().find(|l| l.starts_with(&format!("{name}_count{{share=\"{share}\"}}"))).and_then(|l| l.rsplit(' ').next()).and_then(|v| v.parse::<u64>().ok())
        };
        assert!(count("kiln_blocking_run_seconds", crate::rt::SHARE_STORE).is_some_and(|n| n >= 1), "the store's job: {o}");
        assert!(!o.contains("no such share"));
    }

    /// Goal 6, plan 01a1a59c2abe step 3: kiln.db's writer is stuck only where calls wait for it and it finished none for
    /// as long as kiln's longest prober waits, from the later of their line forming and its last call: never while
    /// nothing waits, nor while it keeps finishing calls however long its line.
    #[test]
    fn g6_s3_the_writer_is_stuck_only_when_it_finishes_nothing_while_calls_wait() {
        let after = WRITER_STUCK;
        let ms = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        assert_eq!(stuck(0, 1, 1, 10 * ms, after), None, "nothing waits");
        assert_eq!(stuck(-1, 1, 1, 10 * ms, after), None, "a count that went past zero waits for nothing");
        assert_eq!(stuck(40, 1_000, 2 * ms, 2 * ms + 50, after), None, "a long line it keeps working off");
        assert_eq!(stuck(3, 1_000, 900, 1_000 + ms - 1, after), None, "not yet");
        assert_eq!(stuck(3, 1_000, 900, 1_000 + ms, after), Some((3, after)), "a line of 3 nothing finished for the whole wait");
        assert_eq!(stuck(1, 5_000, 0, 5_000 + 2 * ms, after), Some((1, 2 * after)), "a writer that never finished one");
        // The writer's figures as it tells them: a call sent while none waited forms a line; one finished moves it on.
        store_write_queued(1);
        store_write_queued(-1);
        store_write(std::time::Duration::ZERO, std::time::Duration::ZERO);
        assert!(STORE_WRITE_FORMED_MS.load(Ordering::Relaxed) > 0 && STORE_WRITE_ENDED_MS.load(Ordering::Relaxed) > 0);
        assert_eq!(store_writer_stuck(), None, "just finished one");
    }
}
