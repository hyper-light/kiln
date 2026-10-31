//! Flake-aware culprit finding over a new failure's suspects (docs/ci.md W28-R56), the pure part:
//! which commit to try next, and when to stop. After Google's Flake Aware Culprit Finding: a
//! posterior over "suspect i broke it" and "nothing did, it was a flake", from a prior and every
//! try's outcome. A check that is broken at a commit fails there every time; a check that isn't
//! fails with the check's flake rate. So a pass rules out every suspect at or before it, and a
//! failure counts against the later ones by the flake rate. With no flakiness it is a binary
//! search: at most ceil(log2 n) + 1 tries for n suspects. The server, and step 1's replayer, run
//! each try and say how it went; this decides.

use serde::{Deserialize, Serialize};

/// The posterior a culprit, or no culprit, needs to be named.
pub const CONFIDENCE: f64 = 0.99;
/// The tries flakiness may add to the search's bound before it stops, inconclusive.
pub const RETRIES: usize = 3;
/// The least no-culprit share a test W16 marks flaky starts with.
pub const FLAKY_FLOOR: f64 = 0.1;

/// Where a try runs: one of the suspects (by place, oldest first) or the failing commit again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum At {
    Suspect(usize),
    Failing,
}

/// What to do next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "next", rename_all = "kebab-case")]
pub enum Next {
    /// Run the check there.
    Try { at: At },
    /// This suspect broke it, with this posterior.
    Culprit { suspect: usize, confidence: f64 },
    /// Nothing did: the failure was a flake.
    NoCulprit { confidence: f64 },
    /// It stops without knowing, and says why.
    Inconclusive { why: String },
}

/// One new failure's search (W28-R56).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Search {
    /// How many suspects there are; the failing commit is at or after the last.
    pub suspects: usize,
    /// The check's flake rate: how often it fails where it isn't broken (W16's history).
    pub flake: f64,
    /// The prior share of "no culprit".
    pub no_culprit: f64,
    /// Whether W16 marks the check flaky.
    pub marked: bool,
    /// The tries so far, and whether each failed.
    pub tries: Vec<(At, bool)>,
}

impl Search {
    /// A search over `suspects` suspects of a failure of a check whose flake rate is `flake`; one
    /// W16 marks flaky starts with a no-culprit share of at least [`FLAKY_FLOOR`].
    pub fn new(suspects: usize, flake: f64, marked: bool) -> Search {
        let flake = if flake.is_finite() { flake.clamp(0.0, 0.5) } else { 0.0 };
        let no_culprit = if marked { flake.max(FLAKY_FLOOR) } else { flake };
        Search { suspects, flake, no_culprit, marked, tries: Vec::new() }
    }

    /// At most how many tries it takes: ceil(log2 n) + 1 without flakiness, and [`RETRIES`] more.
    pub fn cap(&self) -> usize {
        bound(self.suspects) + RETRIES
    }

    /// Adds a try's outcome.
    pub fn observe(&mut self, at: At, failed: bool) {
        self.tries.push((at, failed));
    }

    /// The posterior: each suspect's, then no culprit's. All zero when the outcomes contradict
    /// every hypothesis.
    pub fn posterior(&self) -> (Vec<f64>, f64) {
        let n = self.suspects;
        let f = self.flake;
        let each = if n == 0 { 0.0 } else { (1.0 - self.no_culprit) / n as f64 };
        // The failure that started it: at the failing commit, broken under every suspect.
        let observed = std::iter::once((At::Failing, true)).chain(self.tries.iter().copied());
        let mut culprit = vec![each; n];
        let mut none = self.no_culprit;
        for (at, failed) in observed {
            let flaky = if failed { f } else { 1.0 - f };
            none *= flaky;
            for (i, p) in culprit.iter_mut().enumerate() {
                let broken = match at {
                    At::Failing => true,
                    At::Suspect(j) => j >= i,
                };
                *p *= match (broken, failed) {
                    (true, true) => 1.0,
                    (true, false) => 0.0,
                    (false, _) => flaky,
                };
            }
        }
        let total: f64 = culprit.iter().sum::<f64>() + none;
        if total <= 0.0 || !total.is_finite() {
            return (vec![0.0; n], 0.0);
        }
        (culprit.into_iter().map(|p| p / total).collect(), none / total)
    }

    /// What to do next (W28-R56): name a culprit, or no culprit, once one's posterior reaches
    /// [`CONFIDENCE`]; stop inconclusive past [`Search::cap`] tries or when the outcomes contradict
    /// each other; a flaky check's first try runs the failing commit again; else the first suspect
    /// whose cumulative posterior reaches one half, or the one before it when that one already
    /// failed.
    pub fn next(&self) -> Next {
        let (culprit, none) = self.posterior();
        if culprit.iter().all(|p| *p == 0.0) && none == 0.0 {
            return Next::Inconclusive { why: "the tries contradict each other: no suspect, and no flake, explains them all".into() };
        }
        let best = culprit.iter().enumerate().fold(None, |b: Option<(usize, f64)>, (i, p)| if b.is_none_or(|(_, q)| *p > q) { Some((i, *p)) } else { b });
        if let Some((i, p)) = best
            && p >= CONFIDENCE
        {
            return Next::Culprit { suspect: i, confidence: p };
        }
        if none >= CONFIDENCE {
            return Next::NoCulprit { confidence: none };
        }
        if self.tries.len() >= self.cap() {
            let (i, p) = best.unwrap_or((0, 0.0));
            return Next::Inconclusive { why: format!("{} tries without {:.0}% confidence: suspect {} at {:.0}%, no culprit at {:.0}%", self.tries.len(), CONFIDENCE * 100.0, i + 1, p * 100.0, none * 100.0) };
        }
        if self.tries.is_empty() && self.no_culprit >= FLAKY_FLOOR {
            return Next::Try { at: At::Failing };
        }
        let mass: f64 = culprit.iter().sum();
        let mut sum = 0.0;
        let mut k = culprit.len().saturating_sub(1);
        for (i, p) in culprit.iter().enumerate() {
            sum += p;
            if mass > 0.0 && sum / mass >= 0.5 {
                k = i;
                break;
            }
        }
        // A suspect that already failed says nothing more by failing again: try the one before it.
        if k > 0 && self.tries.iter().any(|(at, failed)| *failed && *at == At::Suspect(k)) {
            k -= 1;
        }
        Next::Try { at: At::Suspect(k) }
    }
}

/// ceil(log2 n) + 1: the most tries a search over `n` suspects takes without flakiness.
pub fn bound(n: usize) -> usize {
    let n = n.max(1);
    (usize::BITS - (n - 1).leading_zeros()) as usize + 1
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestCaseError, TestRunner};

    use super::*;

    /// Runs a search to its end, each try's outcome from `outcome`.
    fn run(mut s: Search, mut outcome: impl FnMut(At, usize) -> bool) -> (Next, usize) {
        loop {
            match s.next() {
                Next::Try { at } => {
                    let n = s.tries.len();
                    let failed = outcome(at, n);
                    s.observe(at, failed);
                }
                done => return (done, s.tries.len()),
            }
        }
    }

    /// W28-R56: with no flakiness it is a binary search, naming the culprit in at most
    /// ceil(log2 n) + 1 tries, wherever the culprit is, for up to 64 suspects.
    #[test]
    fn w28_r56_no_flakiness_is_a_binary_search() {
        for n in 1..=64usize {
            for c in 0..n {
                let (done, tries) = run(Search::new(n, 0.0, false), |at, _| match at {
                    At::Suspect(j) => j >= c,
                    At::Failing => true,
                });
                assert_eq!(done, Next::Culprit { suspect: c, confidence: 1.0 }, "{n} suspects, culprit {c}");
                assert!(tries <= bound(n), "{n} suspects, culprit {c}: {tries} tries, more than {}", bound(n));
            }
        }
        assert_eq!(run(Search::new(1, 0.0, false), |_, _| true), (Next::Culprit { suspect: 0, confidence: 1.0 }, 0), "one suspect is the culprit at once");
        assert_eq!((bound(1), bound(2), bound(3), bound(4), bound(5)), (1, 2, 3, 3, 4));
    }

    /// W28-R56, the plan's example: suspects c1, c3 and c5 of a failure at c5, c3 the culprit: c3
    /// fails, then c1 passes, and c3 is named in 2 tries, within ceil(log2 3) + 1 = 3.
    #[test]
    fn w28_r56_three_suspects() {
        let mut s = Search::new(3, 0.0, false);
        assert_eq!(s.next(), Next::Try { at: At::Suspect(1) });
        s.observe(At::Suspect(1), true);
        assert_eq!(s.next(), Next::Try { at: At::Suspect(0) });
        s.observe(At::Suspect(0), false);
        assert_eq!(s.next(), Next::Culprit { suspect: 1, confidence: 1.0 });
    }

    /// W28-R56: a test W16 marks flaky is tried again at the failing commit first: passing there,
    /// the failure was a flake; failing again, the search goes on among the suspects.
    #[test]
    fn w28_r56_a_flaky_check_reruns_the_failing_commit_first() {
        let mut s = Search::new(4, 0.05, true);
        assert_eq!(s.next(), Next::Try { at: At::Failing });
        let mut passed = s.clone();
        passed.observe(At::Failing, false);
        assert!(matches!(passed.next(), Next::NoCulprit { confidence } if confidence >= CONFIDENCE), "{:?}", passed.next());
        s.observe(At::Failing, true);
        assert!(matches!(s.next(), Next::Try { at: At::Suspect(_) }), "{:?}", s.next());
        // Outcomes no hypothesis explains stop it, saying so.
        let mut never = Search::new(2, 0.0, false);
        never.observe(At::Failing, false);
        assert!(matches!(never.next(), Next::Inconclusive { .. }), "{:?}", never.next());
    }

    /// W28-R57: result reuse behind `Ledger`. A search that asks a ledger before each try, and
    /// runs only what it doesn't know, names the culprit the search that runs every try names,
    /// with the same tries, running fewer; `NoLedger` knows nothing, so every try runs.
    #[test]
    fn w28_r57_a_ledger_answers_tries_and_the_rest_run() {
        use std::collections::BTreeMap;

        use crate::verdict::{Check, Known, Ledger, NoLedger, Outcome};

        /// What runs of the same inputs concluded, by commit: whether the check failed.
        struct Kept(BTreeMap<String, bool>);
        impl Ledger for Kept {
            fn outcome(&self, _check: &Check, commit: &str) -> Option<Known> {
                let outcome = |failed: bool| if failed { Outcome::Failed { signature: "t".into() } } else { Outcome::Passed };
                self.0.get(commit).map(|failed| Known { outcome: outcome(*failed), from: format!("the run at {commit}") })
            }
        }
        let check = Check::unit("ci.yml", "test");
        let commit = |at: At| match at {
            At::Suspect(j) => format!("c{j}"),
            At::Failing => "bad".to_owned(),
        };
        // A search over `n` suspects, `c` the culprit, asking `ledger` first: how it ends, its
        // tries, and how many of them ran.
        let search = |n: usize, c: usize, ledger: &dyn Ledger| -> (Next, Vec<At>, usize) {
            let mut s = Search::new(n, 0.0, false);
            let (mut tries, mut ran) = (Vec::new(), 0);
            loop {
                match s.next() {
                    Next::Try { at } => {
                        let failed = match ledger.outcome(&check, &commit(at)) {
                            Some(k) => k.failed(),
                            None => {
                                ran += 1;
                                match at {
                                    At::Suspect(j) => j >= c,
                                    At::Failing => true,
                                }
                            }
                        };
                        tries.push(at);
                        s.observe(at, failed);
                    }
                    done => return (done, tries, ran),
                }
            }
        };
        let (mut runs_alone, mut runs_with) = (0, 0);
        for n in 2..=16usize {
            for c in 0..n {
                let (alone, tries, every) = search(n, c, &NoLedger);
                assert_eq!(alone, Next::Culprit { suspect: c, confidence: 1.0 }, "{n} suspects, culprit {c}");
                assert_eq!(every, tries.len(), "with no ledger every try runs");
                // A ledger that knows every other suspect's outcome.
                let kept = Kept((0..n).filter(|j| j % 2 == 0).map(|j| (format!("c{j}"), j >= c)).collect());
                let (with, same, ran) = search(n, c, &kept);
                assert_eq!((&with, &same), (&alone, &tries), "{n} suspects, culprit {c}: the ledger's outcomes are the runs'");
                let known = tries.iter().filter(|at| matches!(at, At::Suspect(j) if j % 2 == 0)).count();
                assert_eq!(ran, every - known, "{n} suspects, culprit {c}: only what the ledger doesn't know runs");
                runs_alone += every;
                runs_with += ran;
            }
        }
        assert!(runs_with < runs_alone, "{runs_with} runs with the ledger, {runs_alone} without");
    }

    /// W28-R56, property test: over 10,000 random searches (up to 40 suspects, a culprit or none,
    /// flake rates up to 0.3, flaky outcomes drawn from the case): every search ends within its cap
    /// (ceil(log2 n) + 1 + RETRIES tries), and the same outcomes give the same tries; without
    /// flakiness it names the culprit within the bound.
    #[test]
    fn w28_r56_searches_end_within_their_cap() -> Result<(), String> {
        let strategy = (1usize..=40, any::<u64>(), 0u32..=30, any::<bool>(), any::<bool>());
        let mut runner = TestRunner::new(Config { cases: 10_000, failure_persistence: None, ..Config::default() });
        runner
            .run(&strategy, |(n, seed, rate, has_culprit, marked)| {
                let f = f64::from(rate) / 100.0;
                let culprit = has_culprit.then_some((seed % n as u64) as usize);
                // A broken commit fails every time; elsewhere the check fails at the flake rate,
                // decided by a hash of the seed and the try's number.
                let outcome = |at: At, k: usize| -> bool {
                    let broken = match (at, culprit) {
                        (_, None) => false,
                        (At::Failing, Some(_)) => true,
                        (At::Suspect(j), Some(c)) => j >= c,
                    };
                    let x = seed.rotate_left(k as u32 % 64) ^ (k as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    broken || (x % 1000) < (f * 1000.0) as u64
                };
                let s = Search::new(n, f, marked);
                let cap = s.cap();
                let (done, tries) = run(s.clone(), outcome);
                prop_assert!(tries <= cap, "{} tries past the cap {}", tries, cap);
                prop_assert_eq!(run(s, outcome), (done.clone(), tries));
                if rate == 0
                    && let Some(c) = culprit
                {
                    prop_assert_eq!(done, Next::Culprit { suspect: c, confidence: 1.0 });
                    prop_assert!(tries <= bound(n) + usize::from(marked));
                }
                Ok::<(), TestCaseError>(())
            })
            .map_err(|e| e.to_string())
    }
}
