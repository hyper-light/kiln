//! Running seeds: how many (a budget, `KILN_SIM_SEEDS`), from where (`KILN_SIM_FROM`, so a
//! nightly run tries seeds no run before it tried), one alone (`KILN_SIM_SEED`, to replay a
//! failure), how fast (seeds a minute, on the one core a run uses), and twice (a seed that runs
//! two ways is a failure of its own: the simulator lost its determinism).
//!
//! A model's run of one seed is a function from the seed to its outcome: its trace's digest, or
//! what its checker found wrong.

use std::time::{Duration, Instant};

/// How a run of one seed ended: the digest of what it did, or what went wrong.
pub type Outcome = Result<u64, String>;

/// Seeds to run: `KILN_SIM_SEEDS` when set, else `default`.
pub fn budget(default: u64) -> u64 {
    env_u64("KILN_SIM_SEEDS").unwrap_or(default)
}

/// The first seed to run: `KILN_SIM_FROM` when set, else 0.
pub fn from() -> u64 {
    env_u64("KILN_SIM_FROM").unwrap_or(0)
}

/// The one seed to replay, when `KILN_SIM_SEED` names it.
pub fn replay() -> Option<u64> {
    env_u64("KILN_SIM_SEED")
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

/// What a run of many seeds found.
#[derive(Debug, Clone)]
pub struct Report {
    pub model: &'static str,
    pub first: u64,
    pub seeds: u64,
    /// Each seed that failed, and why.
    pub failures: Vec<(u64, String)>,
    pub elapsed: Duration,
}

/// Most failures a report keeps: past it, they're counted only.
pub const FAILURES_KEPT: usize = 20;

impl Report {
    /// Seeds a minute, on the one thread the run took.
    pub fn per_minute(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        (self.seeds as f64) * 60.0 / secs
    }

    /// Every seed passed.
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// One line: what ran, how fast, and what failed.
    pub fn line(&self) -> String {
        let last = self.first.saturating_add(self.seeds).saturating_sub(1);
        let mut s = format!(
            "kiln-sim {}: seeds {}..={last} ({}) in {:.2} s, {:.0} seeds/min on one thread, {} failed",
            self.model,
            self.first,
            self.seeds,
            self.elapsed.as_secs_f64(),
            self.per_minute(),
            self.failures.len()
        );
        for (seed, why) in &self.failures {
            s.push_str(&format!("\n  seed {seed}: {why}\n    replay: KILN_SIM_SEED={seed}"));
        }
        s
    }

    /// Ok with its line, or the line as the error.
    pub fn into_result(self) -> Result<String, String> {
        let line = self.line();
        if self.passed() { Ok(line) } else { Err(line) }
    }
}

/// Runs `model` on seeds `first..first + seeds`, one after another on this thread.
pub fn run(model: &'static str, first: u64, seeds: u64, mut run_one: impl FnMut(u64) -> Outcome) -> Report {
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut failed = 0u64;
    for seed in first..first.saturating_add(seeds) {
        if let Err(why) = run_one(seed) {
            failed = failed.saturating_add(1);
            if failures.len() < FAILURES_KEPT {
                failures.push((seed, why));
            }
        }
    }
    if failed > u64::try_from(failures.len()).unwrap_or(u64::MAX) {
        failures.push((u64::MAX, format!("{failed} seeds failed in all; the first {FAILURES_KEPT} are above")));
    }
    Report { model, first, seeds, failures, elapsed: started.elapsed() }
}

/// Runs the budget a test asks for (or the one seed `KILN_SIM_SEED` names): the report.
pub fn run_budget(model: &'static str, default: u64, run_one: impl FnMut(u64) -> Outcome) -> Report {
    match replay() {
        Some(seed) => run(model, seed, 1, run_one),
        None => run(model, from(), budget(default), run_one),
    }
}

/// Runs `seed` twice: its digest when both runs agree, else what differed (the simulator, or
/// the model, isn't deterministic: a failure of its own).
pub fn twice(seed: u64, mut run_one: impl FnMut(u64) -> Outcome) -> Outcome {
    let a = run_one(seed);
    let b = run_one(seed);
    if a == b {
        return a;
    }
    Err(format!("seed {seed} ran two ways: {a:?}, then {b:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T6.1: a report says how fast, what failed and how to replay it; twice catches a seed
    /// that runs two ways.
    #[test]
    fn a_report_names_each_failed_seed_and_twice_catches_one_that_runs_two_ways() {
        let r = run("toy", 10, 5, |s| if s == 12 { Err("a planted failure".into()) } else { Ok(s) });
        assert_eq!(r.failures, [(12, "a planted failure".to_owned())]);
        let line = r.line();
        assert!(line.contains("seeds 10..=14 (5)") && line.contains("KILN_SIM_SEED=12"), "{line}");
        assert_eq!(twice(3, Ok), Ok(3));
        let mut n = 0;
        let two_ways = twice(3, |_| {
            n += 1;
            Ok(n)
        });
        assert!(two_ways.is_err_and(|e| e.contains("ran two ways")));
    }
}
