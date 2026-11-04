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
//!   (`_longest_piece_seconds`).
//! - **The blocking pool's shares** (`kiln_blocking_*{share}`): how long each job waited for one of its
//!   share's places (`_wait_seconds`), how long it ran (`_run_seconds`), and the share's places (`_places`).
//!   By Little's law a share holds, on average, the rate its jobs come at times the time each takes: the lab
//!   reads both (`docs/runtime-lab.md`).

use std::fmt::Write as _;
use std::sync::OnceLock;

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
    })
}

/// A call kiln.db's writer ran: how long it waited for its turn, from being sent, and how long it ran.
pub fn store_write(waited: std::time::Duration, ran: std::time::Duration) {
    let p = pools();
    p.store_write_wait.observe(waited.as_secs_f64());
    p.store_write_run.observe(ran.as_secs_f64());
}

#[allow(clippy::cast_precision_loss)]
fn secs(ns: u64) -> f64 {
    ns as f64 / 1e9
}

/// What the compute pool told of a job's wait or a piece (`hyper_rt::compute::observe`).
pub fn compute_job(observed: Observed) {
    match observed {
        Observed::Waited(ns) => pools().compute_wait.observe(secs(ns)),
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
}

/// The `kiln_compute_*` and `kiln_blocking_*` families, in the exposition.
pub fn render(o: &mut String) {
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    let p = pools();
    let s = hyper_rt::compute::stats();
    for (name, kind, h, v) in [
        ("kiln_compute_jobs_total", "counter", "Jobs the compute pool took: CPU-heavy work off the shards.", s.accepted),
        ("kiln_compute_refused_total", "counter", "Jobs the compute pool refused at its bound (each asked again).", s.refused),
        ("kiln_compute_pieces_total", "counter", "Pieces the compute pool's jobs ran (a job of one closure is one).", s.pieces),
        ("kiln_compute_yielded_total", "counter", "Pieces after which their job gave its thread to a waiting job, going on after it.", s.yielded),
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
        for family in ["kiln_compute_jobs_total", "kiln_compute_wait_seconds_bucket", "kiln_compute_piece_seconds_count", "kiln_blocking_places{share=\"fs\"}"] {
            assert!(o.contains(family), "{family} in {o}");
        }
        let count = |name: &str, share: &str| {
            o.lines().find(|l| l.starts_with(&format!("{name}_count{{share=\"{share}\"}}"))).and_then(|l| l.rsplit(' ').next()).and_then(|v| v.parse::<u64>().ok())
        };
        assert!(count("kiln_blocking_run_seconds", crate::rt::SHARE_STORE).is_some_and(|n| n >= 1), "the store's job: {o}");
        assert!(!o.contains("no such share"));
    }
}
