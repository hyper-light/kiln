//! Insights (docs/ci.md W19-R01, R02): what runs, legs and steps took, kept as rollups that merge.
//!
//! A [`Rollup`] holds one bucket's facts (an hour's or a day's, for one repository, workflow,
//! job, step, executor and runner): counts by conclusion, flaky and retried legs, CPU, memory,
//! runner time, queue time by wait reason, failure classes, and durations as a [`Hist`]: a
//! log-bucketed histogram whose quantiles are within 1 % of the exact ones, and which merges
//! exactly, so a day is its hours merged and a month its days. [`critical_path`] is a run's
//! longest path through `needs` by duration.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// Bucket growth: values in one bucket are within [`ACCURACY`] of its representative.
const ACCURACY: f64 = 0.01;
const GAMMA: f64 = (1.0 + ACCURACY) / (1.0 - ACCURACY);

/// Values at or below this (a millisecond, a byte) count as zero.
const TINY: f64 = 1e-3;

/// A mergeable histogram of non-negative values with relative accuracy [`ACCURACY`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hist {
    /// How many values.
    #[serde(default, rename = "n")]
    pub count: u64,
    /// Their sum.
    #[serde(default, rename = "s")]
    pub sum: f64,
    #[serde(default, rename = "lo", skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, rename = "hi", skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Values counted as zero.
    #[serde(default, rename = "z", skip_serializing_if = "is_zero")]
    zero: u64,
    /// Counts by bucket index: bucket `i` holds `(GAMMA^(i-1), GAMMA^i]`.
    #[serde(default, rename = "b", skip_serializing_if = "BTreeMap::is_empty")]
    bins: BTreeMap<i32, u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Hist {
    pub fn add(&mut self, x: f64) {
        if !x.is_finite() || x < 0.0 {
            return;
        }
        self.count += 1;
        self.sum += x;
        self.min = Some(self.min.map_or(x, |m| m.min(x)));
        self.max = Some(self.max.map_or(x, |m| m.max(x)));
        if x <= TINY {
            self.zero += 1;
        } else {
            *self.bins.entry((x.ln() / GAMMA.ln()).ceil() as i32).or_default() += 1;
        }
    }

    pub fn merge(&mut self, o: &Hist) {
        self.count += o.count;
        self.sum += o.sum;
        self.min = match (self.min, o.min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.max = match (self.max, o.max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.zero += o.zero;
        for (i, c) in &o.bins {
            *self.bins.entry(*i).or_default() += c;
        }
    }

    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// The `q` quantile (`0.5` the median), the value at rank `q × (count − 1)`.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let rank = (q.clamp(0.0, 1.0) * (self.count - 1) as f64).round() as u64;
        // The ends are known exactly.
        if rank == 0 {
            return self.min;
        }
        if rank == self.count - 1 {
            return self.max;
        }
        let clamp = |v: f64| v.clamp(self.min.unwrap_or(v), self.max.unwrap_or(v));
        let mut seen = self.zero;
        if rank < seen {
            return Some(clamp(0.0));
        }
        for (i, c) in &self.bins {
            seen += c;
            if rank < seen {
                return Some(clamp(2.0 * GAMMA.powi(*i) / (GAMMA + 1.0)));
            }
        }
        self.max
    }
}

/// How a leg, run or step ended, for counting.
fn bucket(conclusion: &str) -> Option<Bucket> {
    match conclusion {
        "success" => Some(Bucket::Success),
        "failure" | "timed_out" | "startup_failure" => Some(Bucket::Failure),
        "cancelled" => Some(Bucket::Cancelled),
        "skipped" | "neutral" => Some(Bucket::Skipped),
        _ => None,
    }
}

enum Bucket {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

/// One leg's, run's or step's facts.
#[derive(Debug, Clone, Default)]
pub struct Fact {
    pub conclusion: String,
    /// Seconds from its start to its end.
    pub duration: Option<f64>,
    /// Seconds from queued to started.
    pub queue: Option<f64>,
    pub cpu_seconds: f64,
    pub peak_rss: u64,
    /// Seconds on runners, every attempt counted.
    pub runner_seconds: f64,
    pub attempts: u32,
    /// It passed after failing on an earlier attempt.
    pub flaky: bool,
    /// Queue seconds by wait reason (W05-R13).
    pub waits: BTreeMap<String, f64>,
    /// Why it failed, when it did (W05's failure classes).
    pub failure_class: Option<String>,
}

/// One bucket's facts, mergeable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rollup {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub success: u64,
    #[serde(default)]
    pub failure: u64,
    #[serde(default)]
    pub cancelled: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub flaky: u64,
    #[serde(default)]
    pub retried: u64,
    #[serde(default)]
    pub duration: Hist,
    #[serde(default)]
    pub queue: Hist,
    #[serde(default)]
    pub cpu_seconds: f64,
    /// The largest.
    #[serde(default)]
    pub peak_rss: u64,
    #[serde(default)]
    pub runner_seconds: f64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub waits: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub failures: BTreeMap<String, u64>,
}

impl Rollup {
    pub fn add(&mut self, f: &Fact) {
        self.count += 1;
        match bucket(&f.conclusion) {
            Some(Bucket::Success) => self.success += 1,
            Some(Bucket::Failure) => self.failure += 1,
            Some(Bucket::Cancelled) => self.cancelled += 1,
            Some(Bucket::Skipped) => self.skipped += 1,
            None => {}
        }
        self.flaky += u64::from(f.flaky);
        self.retried += u64::from(f.attempts > 1);
        if let Some(d) = f.duration {
            self.duration.add(d);
        }
        if let Some(q) = f.queue {
            self.queue.add(q);
        }
        self.cpu_seconds += f.cpu_seconds;
        self.peak_rss = self.peak_rss.max(f.peak_rss);
        self.runner_seconds += f.runner_seconds;
        for (r, s) in &f.waits {
            *self.waits.entry(r.clone()).or_default() += s;
        }
        if let Some(c) = &f.failure_class {
            *self.failures.entry(c.clone()).or_default() += 1;
        }
    }

    pub fn merge(&mut self, o: &Rollup) {
        self.count += o.count;
        self.success += o.success;
        self.failure += o.failure;
        self.cancelled += o.cancelled;
        self.skipped += o.skipped;
        self.flaky += o.flaky;
        self.retried += o.retried;
        self.duration.merge(&o.duration);
        self.queue.merge(&o.queue);
        self.cpu_seconds += o.cpu_seconds;
        self.peak_rss = self.peak_rss.max(o.peak_rss);
        self.runner_seconds += o.runner_seconds;
        for (r, s) in &o.waits {
            *self.waits.entry(r.clone()).or_default() += s;
        }
        for (c, n) in &o.failures {
            *self.failures.entry(c.clone()).or_default() += n;
        }
    }

    /// Of those that passed or failed, the share that passed.
    pub fn success_rate(&self) -> Option<f64> {
        let decided = self.success + self.failure;
        (decided > 0).then(|| self.success as f64 / decided as f64)
    }

    /// Of those that passed or failed, the share that passed only on a retry.
    pub fn flaky_rate(&self) -> Option<f64> {
        let decided = self.success + self.failure;
        (decided > 0).then(|| self.flaky as f64 / decided as f64)
    }

    /// What the UI and the API show.
    pub fn summary(&self) -> Summary {
        let q = |h: &Hist, p: f64| h.quantile(p);
        Summary {
            count: self.count,
            success: self.success,
            failure: self.failure,
            cancelled: self.cancelled,
            skipped: self.skipped,
            flaky: self.flaky,
            retried: self.retried,
            success_rate: self.success_rate(),
            flaky_rate: self.flaky_rate(),
            p50: q(&self.duration, 0.5),
            p90: q(&self.duration, 0.9),
            p99: q(&self.duration, 0.99),
            mean: self.duration.mean(),
            queue_p50: q(&self.queue, 0.5),
            queue_p90: q(&self.queue, 0.9),
            queue_seconds: self.queue.sum,
            run_seconds: self.duration.sum,
            cpu_seconds: self.cpu_seconds,
            peak_rss: self.peak_rss,
            runner_minutes: self.runner_seconds / 60.0,
            waits: self.waits.clone(),
            failures: self.failures.clone(),
        }
    }
}

/// A rollup as read: rates and percentiles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub count: u64,
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
    pub skipped: u64,
    pub flaky: u64,
    pub retried: u64,
    pub success_rate: Option<f64>,
    pub flaky_rate: Option<f64>,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p99: Option<f64>,
    pub mean: Option<f64>,
    pub queue_p50: Option<f64>,
    pub queue_p90: Option<f64>,
    pub queue_seconds: f64,
    pub run_seconds: f64,
    pub cpu_seconds: f64,
    pub peak_rss: u64,
    pub runner_minutes: f64,
    pub waits: BTreeMap<String, f64>,
    pub failures: BTreeMap<String, u64>,
}

/// A job of a run, for its critical path.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub needs: Vec<String>,
    /// Seconds.
    pub duration: f64,
}

/// A run's critical path (W19-R01): the longest path through `needs` by duration, first job
/// first. Ties go to the id that sorts first. Needs that name no job are ignored; a cycle (never
/// in a valid plan) leaves its jobs on paths of their own.
pub fn critical_path(nodes: &[Node]) -> Vec<String> {
    let at: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let needs: Vec<Vec<usize>> = nodes.iter().map(|n| n.needs.iter().filter_map(|x| at.get(x.as_str()).copied()).collect()).collect();
    // Kahn's order over `needs`, so a job comes after everything it needs.
    let mut waiting: Vec<usize> = needs.iter().map(Vec::len).collect();
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, ns) in needs.iter().enumerate() {
        for &n in ns {
            if let Some(u) = users.get_mut(n) {
                u.push(i);
            }
        }
    }
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|i| waiting.get(*i) == Some(&0)).collect();
    let (mut best, mut prev): (Vec<f64>, Vec<Option<usize>>) = (nodes.iter().map(|n| n.duration.max(0.0)).collect(), vec![None; nodes.len()]);
    let id = |i: usize| nodes.get(i).map_or("", |n| n.id.as_str());
    // Whether path `a` (ending in total `ta`) beats `b`: longer, or as long with the smaller id.
    let beats = |ta: f64, a: usize, tb: f64, b: usize| ta > tb || (ta == tb && id(a) < id(b));
    while let Some(i) = ready.pop() {
        let own = nodes.get(i).map_or(0.0, |n| n.duration.max(0.0));
        let mut from: Option<(f64, usize)> = None;
        for &n in needs.get(i).map(Vec::as_slice).unwrap_or_default() {
            let t = best.get(n).copied().unwrap_or(0.0);
            if from.is_none_or(|(ft, f)| beats(t, n, ft, f)) {
                from = Some((t, n));
            }
        }
        if let (Some(b), Some(p)) = (best.get_mut(i), prev.get_mut(i)) {
            *b = own + from.map_or(0.0, |(t, _)| t);
            *p = from.map(|(_, n)| n);
        }
        for &u in users.get(i).map(Vec::as_slice).unwrap_or_default() {
            if let Some(w) = waiting.get_mut(u) {
                *w -= 1;
                if *w == 0 {
                    ready.push(u);
                }
            }
        }
    }
    let mut end: Option<usize> = None;
    for i in 0..nodes.len() {
        let t = best.get(i).copied().unwrap_or(0.0);
        if end.is_none_or(|e| beats(t, i, best.get(e).copied().unwrap_or(0.0), e)) {
            end = Some(i);
        }
    }
    let mut path = Vec::new();
    let mut cur = end;
    while let Some(i) = cur {
        if path.len() > nodes.len() {
            break;
        }
        path.push(id(i).to_owned());
        cur = prev.get(i).copied().flatten();
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests;
