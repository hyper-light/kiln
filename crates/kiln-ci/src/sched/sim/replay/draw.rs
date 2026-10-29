//! What the trace leaves unknown, drawn from the seed: how long a job that didn't finish would have
//! taken and how it would have ended, and how long a k8s pod that didn't start would have waited.
//!
//! Each unknown is drawn from the nearest completed runs of the same job (by when their run was
//! created), right-censored: a job cancelled after running `t` seconds takes longer than `t`, and a
//! pod that hadn't started after `w` seconds waits longer than `w` (ci/sim/README.md; without it the
//! replay finishes far more runs than the week did). Every job draws once per seed from a stream
//! of its own, so every policy meets the same draws (common random numbers).

use std::collections::BTreeMap;

use super::rng::Rng;
use super::trace::{End, Trace};

/// How many neighbours a draw starts from: half before the job's run, half after.
const NEAR: usize = 4;

/// A job's duration, outcome and pod start delay, as recorded or drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Draw {
    pub duration: i64,
    pub end: End,
    pub failed_step: Option<String>,
    /// Durations of its steps another pipeline's work could replace (P5).
    pub shared: BTreeMap<String, i64>,
    /// A k8s job's wait from its pod's launch to its start.
    pub delay: i64,
    /// Whether the duration and outcome were drawn rather than recorded.
    pub drawn: bool,
}

/// A completed job of the trace: where it is, and how long it took.
#[derive(Debug, Clone, Copy)]
struct Done {
    run: usize,
    job: usize,
    took: i64,
}

/// The nearest entries to `at` of `xs` (sorted by `at`) worth more than `more_than` (any, for
/// `None`): starting from [`NEAR`] around it and widening until some qualify.
fn near<T: Copy>(xs: &[(i64, T)], at: i64, more_than: Option<i64>, worth: impl Fn(&T) -> i64) -> Vec<T> {
    let i = xs.partition_point(|(t, _)| *t < at);
    let mut w = NEAR;
    loop {
        let lo = i.saturating_sub(w / 2);
        let hi = xs.len().min(i.saturating_add(w - w / 2));
        let found: Vec<T> = xs.get(lo..hi).unwrap_or(&[]).iter().filter(|(_, x)| more_than.is_none_or(|m| worth(x) > m)).map(|(_, x)| *x).collect();
        if !found.is_empty() || (lo == 0 && hi == xs.len()) {
            return found;
        }
        w = w.saturating_mul(2);
    }
}

/// Every job's draw, by run and job, for `seed`.
pub fn draws(trace: &Trace, seed: u64) -> Vec<Vec<Draw>> {
    let mut done: BTreeMap<(&str, &str), Vec<(i64, Done)>> = BTreeMap::new();
    let mut delays: BTreeMap<(&str, &str), Vec<(i64, i64)>> = BTreeMap::new();
    for (ri, r) in trace.runs.iter().enumerate() {
        for (ji, j) in r.jobs.iter().enumerate() {
            if let Some((took, _)) = j.completed() {
                done.entry((&r.workflow, &j.key)).or_default().push((r.at, Done { run: ri, job: ji, took }));
            }
            if let (true, Some(q), Some(s)) = (j.is_k8s(), j.queued, j.started) {
                delays.entry((&r.workflow, &j.key)).or_default().push((r.at, s - q));
            }
        }
    }
    let job_at = |d: &Done| trace.runs.get(d.run).and_then(|r| r.jobs.get(d.job));
    let mut out = Vec::with_capacity(trace.runs.len());
    for (ri, r) in trace.runs.iter().enumerate() {
        let mut row = Vec::with_capacity(r.jobs.len());
        for (ji, j) in r.jobs.iter().enumerate() {
            let (duration, end, failed_step, shared, drawn) = match j.completed() {
                Some((took, e)) => (took, e, j.failed_step.clone(), j.shared.clone(), false),
                None => {
                    // How long it had run when it was cancelled, or when the trace ends.
                    let ran = match (j.started, j.finished) {
                        (Some(s), Some(f)) => Some((f - s).max(0)),
                        (Some(s), None) => Some((trace.end - s).max(0)),
                        _ => None,
                    };
                    let xs = done.get(&(r.workflow.as_str(), j.key.as_str())).map(Vec::as_slice).unwrap_or(&[]);
                    let found = near(xs, r.at, ran, |d| d.took);
                    let mut rng = Rng::stream(seed, &format!("duration/{ri}/{ji}"));
                    match found.get(rng.below(found.len())).and_then(|d| job_at(d).map(|n| (d, n))) {
                        Some((d, n)) => (d.took, n.completed().map_or(End::Succeeded, |(_, e)| e), n.failed_step.clone(), n.shared.clone(), true),
                        // No neighbour ran longer: a little longer than it had.
                        None => {
                            let ran = ran.unwrap_or(0);
                            (ran + ran / 10 + 1, End::Succeeded, None, BTreeMap::new(), true)
                        }
                    }
                }
            };
            let mut delay = 0;
            if j.is_k8s() {
                delay = match (j.queued, j.started) {
                    (Some(q), Some(s)) => (s - q).max(0),
                    (q, None) => {
                        // How long its pod had waited when its run was cancelled, or the trace ends.
                        let waited = match (q, j.finished) {
                            (Some(q), Some(f)) => Some((f - q).max(0)),
                            (Some(q), None) => Some((trace.end - q).max(0)),
                            _ => None,
                        };
                        let xs = delays.get(&(r.workflow.as_str(), j.key.as_str())).map(Vec::as_slice).unwrap_or(&[]);
                        let found = near(xs, r.at, waited, |w| *w);
                        let mut rng = Rng::stream(seed, &format!("delay/{ri}/{ji}"));
                        let waited = waited.unwrap_or(0);
                        found.get(rng.below(found.len())).copied().unwrap_or(waited + waited / 10 + 1)
                    }
                    (None, Some(_)) => 0,
                };
            }
            row.push(Draw { duration, end, failed_step, shared, delay, drawn });
        }
        out.push(row);
    }
    out
}
