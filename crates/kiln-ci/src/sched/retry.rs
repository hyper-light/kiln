//! Retries (docs/ci.md W05-R14 to W05-R16): a policy at leg level (`retry:`), failure classes the
//! runner reports, and the one default: a runner lost mid-leg is retried once, on another runner.

use serde::{Deserialize, Serialize};

use super::units::{self, Ms};
use crate::wf::ir::YVal;

/// Why an attempt failed (W05-R15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    RunnerLost,
    Infra,
    ImagePull,
    Oom,
    DiskFull,
    Timeout,
    Failure,
}

impl FailureClass {
    pub fn parse(s: &str) -> Option<FailureClass> {
        Some(match s {
            "runner-lost" => FailureClass::RunnerLost,
            "infra" => FailureClass::Infra,
            "image-pull" => FailureClass::ImagePull,
            "oom" => FailureClass::Oom,
            "disk-full" => FailureClass::DiskFull,
            "timeout" => FailureClass::Timeout,
            "failure" => FailureClass::Failure,
            _ => return None,
        })
    }

    /// The name [`FailureClass::parse`] reads.
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::RunnerLost => "runner-lost",
            FailureClass::Infra => "infra",
            FailureClass::ImagePull => "image-pull",
            FailureClass::Oom => "oom",
            FailureClass::DiskFull => "disk-full",
            FailureClass::Timeout => "timeout",
            FailureClass::Failure => "failure",
        }
    }
}

/// A failed attempt, as the dispatcher reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub class: FailureClass,
    pub exit_code: Option<i32>,
    /// The policy's `log:` patterns found in the attempt's log.
    pub log_hits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryOn {
    Class(FailureClass),
    ExitCode(Vec<i32>),
    Log(String),
}

/// Where a retry runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunnerChoice {
    /// [`RunnerChoice::Another`] after `runner-lost`, any otherwise.
    #[default]
    Default,
    Any,
    Same,
    /// Another runner, waiting for one when none other can take it (`runner: different`).
    Different,
    /// Another runner when one can take it, else the same one (W05-R16's default after
    /// `runner-lost`): where only that runner could, the retry doesn't wait for good.
    Another,
}

/// A `retry:` policy.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    pub max: u32,
    pub on: Vec<RetryOn>,
    pub initial_ms: Ms,
    /// What each wait multiplies the last by, in thousandths (`2` is `2000`), at least one.
    pub factor: super::state::PerMille,
    pub max_ms: Ms,
    pub runner: RunnerChoice,
}

fn duration_ms(v: &YVal) -> Option<Ms> {
    match v {
        YVal::Num(n) if *n >= 0.0 => units::ms_of_secs(*n),
        YVal::Str(t) => super::routing::number(&t.src).and_then(units::ms_of_secs),
        _ => None,
    }
}

impl RetryPolicy {
    /// From the kiln dialect's `retry:` (W05-R14); v1's `retries: n` arrives as `{ max: n, on:
    /// [runner-lost] }` (W02-R06). Unknown entries are ignored here: W02 reports them.
    pub fn from_yval(v: &YVal) -> Option<RetryPolicy> {
        let max = match v.get("max") {
            Some(YVal::Num(n)) if *n >= 0.0 => *n as u32,
            _ => return None,
        };
        let mut on = Vec::new();
        if let Some(YVal::Seq(items)) = v.get("on") {
            for i in items {
                match i {
                    YVal::Str(t) => on.extend(FailureClass::parse(&t.src).map(RetryOn::Class)),
                    YVal::Map(_) => {
                        if let Some(codes) = i.get("exit-code") {
                            let list: Vec<i32> = match codes {
                                YVal::Seq(s) => s.iter().filter_map(|x| if let YVal::Num(n) = x { Some(*n as i32) } else { None }).collect(),
                                YVal::Num(n) => vec![*n as i32],
                                _ => Vec::new(),
                            };
                            on.push(RetryOn::ExitCode(list));
                        }
                        if let Some(l) = i.get("log").and_then(YVal::as_str) {
                            on.push(RetryOn::Log(l.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
        } else {
            on.push(RetryOn::Class(FailureClass::RunnerLost));
            on.push(RetryOn::Class(FailureClass::Infra));
        }
        let b = v.get("backoff");
        let initial_ms = b.and_then(|b| b.get("initial")).and_then(duration_ms).unwrap_or(10_000);
        let factor = match b.and_then(|b| b.get("factor")) {
            Some(YVal::Num(f)) if *f >= 1.0 => super::state::per_mille(*f).max(1000),
            _ => 2000,
        };
        let max_ms = b.and_then(|b| b.get("max")).and_then(duration_ms).unwrap_or(300_000);
        let runner = match v.get("runner").and_then(YVal::as_str) {
            Some("any") => RunnerChoice::Any,
            Some("same") => RunnerChoice::Same,
            Some("different") => RunnerChoice::Different,
            _ => RunnerChoice::Default,
        };
        Some(RetryPolicy { max, on, initial_ms, factor, max_ms, runner })
    }

    fn covers(&self, f: &Failed) -> bool {
        self.on.iter().any(|o| match o {
            RetryOn::Class(c) => *c == f.class,
            RetryOn::ExitCode(codes) => f.exit_code.is_some_and(|c| codes.contains(&c)),
            RetryOn::Log(p) => f.log_hits.iter().any(|h| h == p),
        })
    }

    /// The wait before retry `n` (1 for the first): initial × factorⁿ⁻¹, at most `max`, in
    /// integers (E1): the wait in billionths of a millisecond, multiplied a step at a time until
    /// it reaches `max`, so it's the same on every machine. A factor of one never grows, and at
    /// above one it reaches any `max` within some 45,000 steps.
    pub fn backoff_ms(&self, n: u32) -> Ms {
        const NANO: u128 = 1_000_000_000;
        let max = u128::from(self.max_ms).saturating_mul(NANO);
        let mut wait = u128::from(self.initial_ms).saturating_mul(NANO);
        if self.factor > 1000 && wait > 0 {
            for _ in 1..n {
                if wait >= max {
                    break;
                }
                wait = wait.saturating_mul(u128::from(self.factor)) / 1000;
            }
        }
        u64::try_from(wait.min(max) / NANO).unwrap_or(self.max_ms)
    }
}

/// A retry: after how long, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub delay_ms: u64,
    pub runner: RunnerChoice,
}

/// Whether attempt `attempt` (1 for the first) of a leg that failed as `f` is retried.
pub fn decide(policy: Option<&RetryPolicy>, attempt: u32, f: &Failed) -> Option<Decision> {
    let resolve = |c: RunnerChoice| match c {
        RunnerChoice::Default if f.class == FailureClass::RunnerLost => RunnerChoice::Another,
        RunnerChoice::Default => RunnerChoice::Any,
        other => other,
    };
    match policy {
        // The default: a lost runner is retried once, elsewhere when it can be (W05-R16).
        None => (f.class == FailureClass::RunnerLost && attempt < 2).then_some(Decision { delay_ms: 0, runner: RunnerChoice::Another }),
        Some(p) => (attempt <= p.max && p.covers(f)).then(|| Decision { delay_ms: p.backoff_ms(attempt), runner: resolve(p.runner) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::ir::Tpl;

    fn failed(class: FailureClass) -> Failed {
        Failed { class, exit_code: None, log_hits: Vec::new() }
    }

    fn s(x: &str) -> YVal {
        YVal::Str(Tpl::new(x, Default::default()))
    }

    #[test]
    fn the_default() {
        assert_eq!(decide(None, 1, &failed(FailureClass::RunnerLost)), Some(Decision { delay_ms: 0, runner: RunnerChoice::Another }));
        assert_eq!(decide(None, 2, &failed(FailureClass::RunnerLost)), None, "once");
        assert_eq!(decide(None, 1, &failed(FailureClass::Failure)), None, "nothing else");
    }

    #[test]
    fn a_policy() {
        let v = YVal::Map(vec![
            ("max".into(), YVal::Num(3.0)),
            ("on".into(), YVal::Seq(vec![s("oom"), s("timeout"), YVal::Map(vec![("exit-code".into(), YVal::Seq(vec![YVal::Num(137.0), YVal::Num(143.0)]))]), YVal::Map(vec![("log".into(), s("connection reset by peer"))])])),
            ("backoff".into(), YVal::Map(vec![("initial".into(), s("10s")), ("factor".into(), YVal::Num(2.0)), ("max".into(), s("30s"))])),
            ("runner".into(), s("same")),
        ]);
        let p = RetryPolicy::from_yval(&v);
        assert!(p.is_some());
        let p = p.unwrap_or_else(|| RetryPolicy { max: 0, on: Vec::new(), initial_ms: 0, factor: 1000, max_ms: 0, runner: RunnerChoice::Any });
        assert_eq!((p.backoff_ms(1), p.backoff_ms(2), p.backoff_ms(3)), (10_000, 20_000, 30_000));
        assert_eq!(decide(Some(&p), 1, &failed(FailureClass::Oom)).map(|d| (d.delay_ms, d.runner)), Some((10_000, RunnerChoice::Same)));
        assert!(decide(Some(&p), 4, &failed(FailureClass::Oom)).is_none(), "max reached");
        assert!(decide(Some(&p), 1, &failed(FailureClass::Failure)).is_none());
        assert!(decide(Some(&p), 1, &Failed { class: FailureClass::Failure, exit_code: Some(137), log_hits: Vec::new() }).is_some());
        assert!(decide(Some(&p), 1, &Failed { class: FailureClass::Failure, exit_code: None, log_hits: vec!["connection reset by peer".into()] }).is_some());
        // v1's retries: n.
        let v1 = YVal::Map(vec![("max".into(), YVal::Num(2.0)), ("on".into(), YVal::Seq(vec![s("runner-lost")]))]);
        let p = RetryPolicy::from_yval(&v1);
        assert!(p.as_ref().is_some_and(|p| decide(Some(p), 2, &failed(FailureClass::RunnerLost)).is_some_and(|d| d.runner == RunnerChoice::Another)));
        // An explicit `runner: different` stays strict.
        let strict = YVal::Map(vec![("max".into(), YVal::Num(1.0)), ("on".into(), YVal::Seq(vec![s("runner-lost")])), ("runner".into(), s("different"))]);
        let p = RetryPolicy::from_yval(&strict);
        assert!(p.as_ref().is_some_and(|p| decide(Some(p), 1, &failed(FailureClass::RunnerLost)).is_some_and(|d| d.runner == RunnerChoice::Different)));
    }

    /// E1 (docs/needs.md): the backoff in integers is the float formula's to within a millisecond,
    /// for factors 1.5, 2 and 3 over ten attempts, stops at its max, and doesn't grow at a factor
    /// of one.
    #[test]
    fn e1_2_backoff_in_integers_is_the_float_formulas() {
        for factor in [1.5f64, 2.0, 3.0] {
            let p = RetryPolicy { max: 10, on: Vec::new(), initial_ms: 10_000, factor: crate::sched::state::per_mille(factor), max_ms: 100_000_000, runner: RunnerChoice::Any };
            for n in 1..=10u32 {
                let before = (10_000f64 * factor.powi(i32::try_from(n - 1).unwrap_or(0))).min(100_000_000.0) as u64;
                assert!(p.backoff_ms(n).abs_diff(before) <= 1, "factor {factor}, attempt {n}: {} against {before}", p.backoff_ms(n));
            }
        }
        let capped = RetryPolicy { max: 50, on: Vec::new(), initial_ms: 10_000, factor: 2000, max_ms: 300_000, runner: RunnerChoice::Any };
        assert_eq!((capped.backoff_ms(5), capped.backoff_ms(6), capped.backoff_ms(50)), (160_000, 300_000, 300_000));
        assert_eq!(RetryPolicy { factor: 1000, ..capped.clone() }.backoff_ms(40), 10_000);
        assert_eq!(RetryPolicy { initial_ms: 0, ..capped }.backoff_ms(9), 0);
    }
}
