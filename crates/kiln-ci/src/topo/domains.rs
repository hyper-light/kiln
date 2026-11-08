//! Failure domains (LB08): which runners fail together, so placement can spread a gang's members
//! and a run's retries across them. Two sources:
//!
//! - **Facts**: runners on one machine (its machine id) fail together, always; runners on one IP
//!   subnet share its switch and router.
//! - **Drops seen together**: two links that drop within the time a drop takes to be noticed of
//!   each other, more often than chance allows. Each link's drops are taken as a Poisson process of
//!   its own measured rate; if two are independent, each of `a`'s `λ_a·T` drops over the time `T`
//!   both were watched finds one of `b`'s within `±w` with chance `2w·λ_b` (for `2w·λ_b` small),
//!   so `μ = 2w·λ_a·λ_b·T` coincidences are expected by chance alone, and seeing
//!   `c` has the Poisson tail `P(X ≥ c | μ)`. A pair whose tail is below the significance level
//!   (the person's setting, Fisher's 0.05 by default), divided by the pairs tested (Bonferroni, as
//!   Dunn, "Multiple Comparisons Among Means", JASA 56(293), 1961), shares a domain; domains are the
//!   connected groups of such pairs.
//!
//! `w` is the link's own: the longest its transport takes to notice a drop (QUIC's idle timeout,
//! which the shell passes in), so a drop of two links at one instant is noticed within `w` of each
//! other. Nothing here is picked.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::fixed::{self, ONE};

/// The significance level a co-dropping pair is judged at before the pairs' correction, Q32: the
/// person's setting, Fisher's conventional 5 % by default (Fisher, "Statistical Methods for
/// Research Workers", 1925).
pub const DEFAULT_ALPHA: u64 = ONE / 20;

/// Drops seen across the fleet, kept as counts: each runner's drops and its last's time, and each
/// pair's drops within `w` of each other.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoDrops {
    pub last: BTreeMap<String, i64>,
    pub pairs: BTreeMap<String, BTreeMap<String, u64>>,
}

impl CoDrops {
    /// `runner`'s link dropped at `at` (ms), noticed within `window_ms` of when it happened: each
    /// other runner whose last drop was within that of it co-dropped with it once.
    pub fn dropped(&mut self, runner: &str, at: i64, window_ms: u64) {
        let w = i64::try_from(window_ms).unwrap_or(i64::MAX);
        let near: Vec<String> = self.last.iter().filter(|(r, t)| r.as_str() != runner && at.saturating_sub(**t).abs() <= w).map(|(r, _)| r.clone()).collect();
        for other in near {
            let (a, b) = if runner < other.as_str() { (runner.to_owned(), other) } else { (other, runner.to_owned()) };
            let n = self.pairs.entry(a).or_default().entry(b).or_insert(0);
            *n = n.saturating_add(1);
        }
        self.last.insert(runner.to_owned(), at);
    }

    /// How often `a` and `b` dropped together.
    pub fn together(&self, a: &str, b: &str) -> u64 {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        self.pairs.get(a).and_then(|m| m.get(b)).copied().unwrap_or(0)
    }
}

/// One runner's link as the test needs it: its drops and its time up, ms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seen {
    pub drops: u64,
    pub up_ms: u64,
}

/// `P(X ≥ c)` for `X` Poisson with mean `mu` (Q32), Q32: one less the chance of fewer.
pub fn poisson_tail(c: u64, mu: u64) -> u64 {
    if c == 0 {
        return ONE;
    }
    // Σ_{k<c} e^(−μ) μ^k / k!, its terms kept in Q32.
    let mut term = fixed::exp_neg(mu);
    let mut below = term;
    for k in 1..c {
        term = fixed::sat(u128::from(term) * u128::from(mu) / (u128::from(k) << 32));
        below = below.saturating_add(term);
        if term == 0 {
            break;
        }
    }
    ONE.saturating_sub(below)
}

/// Whether `a` and `b` drop together past chance: their co-drops' Poisson tail below `alpha`
/// over the `tests` pairs judged.
pub fn together_past_chance(c: u64, a: Seen, b: Seen, window_ms: u64, alpha: u64, tests: u64) -> bool {
    if c == 0 || a.up_ms == 0 || b.up_ms == 0 {
        return false;
    }
    let both = a.up_ms.min(b.up_ms);
    // μ = 2w · (n_a/T_a) · (n_b/T_b) · T, Q32.
    let mu = (u128::from(2 * window_ms) * u128::from(a.drops) * u128::from(b.drops) * u128::from(both)) << 32;
    let mu = fixed::sat(mu / (u128::from(a.up_ms) * u128::from(b.up_ms)));
    let level = alpha / tests.max(1);
    #[cfg(test)]
    if super::mutant() == Some(super::Mutant::EveryCoDropCounts) {
        return true;
    }
    poisson_tail(c, mu) < level
}

/// A runner's domains, from the widest fact to the learned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Domain {
    /// Its machine (several runner processes of one machine share it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Its IP subnets, `addr/prefix`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subnets: Vec<String>,
    /// The group of runners whose links drop together, by its first member's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learned: Option<String>,
}

/// Every runner's learned domain: groups of runners joined by pairs that drop together past
/// chance, and by machine (one machine is one domain whatever its drops say). `seen` holds each
/// runner's drops and time up; `machines` its machine id.
pub fn learn(co: &CoDrops, seen: &BTreeMap<String, Seen>, machines: &BTreeMap<String, String>, window_ms: u64, alpha: u64) -> BTreeMap<String, String> {
    let names: Vec<&String> = seen.keys().collect();
    let n = u64::try_from(names.len()).unwrap_or(u64::MAX);
    let tests = n.saturating_mul(n.saturating_sub(1)) / 2;
    let mut parent: BTreeMap<String, String> = names.iter().map(|r| ((*r).clone(), (*r).clone())).collect();
    fn root(p: &BTreeMap<String, String>, mut r: String) -> String {
        for _ in 0..p.len() {
            match p.get(&r) {
                Some(up) if *up != r => r = up.clone(),
                _ => break,
            }
        }
        r
    }
    let join = |p: &mut BTreeMap<String, String>, a: &str, b: &str| {
        let (ra, rb) = (root(p, a.to_owned()), root(p, b.to_owned()));
        if ra != rb {
            // The smaller name roots the group: the same groups name alike everywhere.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            p.insert(hi, lo);
        }
    };
    for (a, others) in &co.pairs {
        for (b, c) in others {
            let (Some(sa), Some(sb)) = (seen.get(a), seen.get(b)) else { continue };
            if together_past_chance(*c, *sa, *sb, window_ms, alpha, tests) {
                join(&mut parent, a, b);
            }
        }
    }
    let mut by_machine: BTreeMap<&String, &String> = BTreeMap::new();
    for (r, m) in machines {
        if !parent.contains_key(r) {
            continue;
        }
        match by_machine.get(m) {
            Some(first) => {
                let first = (*first).clone();
                join(&mut parent, &first, r);
            }
            None => {
                by_machine.insert(m, r);
            }
        }
    }
    names.iter().map(|r| ((*r).clone(), root(&parent, (*r).clone()))).collect()
}
