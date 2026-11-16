//! A deployment experiment's live traffic split (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, the person's
//! decision 7): which variant each new session of its target gets, by the variants' weights; and what those sessions
//! say of each variant, read at any time without inflating a false winner.
//!
//! - **The draw** for the `n`-th session is a point of [0, 1) from the experiment's seed and `n` (splitmix64), laid
//!   on the variants' weights in their order: reproducible, and each variant's share is its weight's.
//! - **The measures** of each variant come from its sessions, each with its confidence sequence
//!   (`crate::stats::sequence`), so a reading after every session is valid at that moment:
//!   - its failures (a start refused or that never ran, or a session that ended in error) as a rate;
//!   - its start, from launch to running, compared only within each start kind (`cold`, `template`, `warm`), never
//!     pooled: warm pools are sized by demand, so a variant of low weight starts cold more often, and a pooled start
//!     time would be that, not the variant (ROOT, 2026-11-14);
//!   - its start-kind mix, each kind's share, as a measure of its own;
//!   - its footprint, peak and steady, and its host CPU;
//!   - its sessions' outcomes, counted.
//! - **A winner** on a measure is a variant whose sequence lies wholly below every other's. Each variant's sequences
//!   are held at `α / k` for its `k` variants (Bonferroni), so a winner holds at the stated 95 % whenever it's read.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::stats::sequence::{self, Sequence};

/// The level a winner holds at.
pub const LEVEL: f64 = 0.95;

/// The start kinds start times are compared within.
pub const KINDS: [&str; 3] = ["cold", "template", "warm"];

/// A point of [0, 1) from `seed` and `n` (splitmix64).
fn unit(seed: u64, n: u64) -> f64 {
    let mut z = seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
}

/// The variant the `n`-th session of a split gets, by `weights` (each above 0) in their order; none without one.
pub fn draw(weights: &[f64], seed: u64, n: u64) -> Option<usize> {
    let total: f64 = weights.iter().sum();
    if weights.is_empty() || weights.iter().any(|w| !(w.is_finite() && *w > 0.0)) {
        return None;
    }
    let at = unit(seed, n) * total;
    let mut sum = 0.0;
    for (k, w) in weights.iter().enumerate() {
        sum += w;
        if at < sum {
            return Some(k);
        }
    }
    weights.len().checked_sub(1)
}

/// How Hex's word for a microVM's start is kept: booted or saving its template is cold, restored from one is from a
/// template, a warm host's is warm.
pub fn kind_of(started_from: &str) -> Option<&'static str> {
    match started_from {
        "booted" | "saved" | "replaced" => Some("cold"),
        "restored" => Some("template"),
        "warm" => Some("warm"),
        _ => None,
    }
}

/// One session of a split, as its experiment read it, in the order sessions were assigned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub arm: String,
    /// Whether it failed, once that's known: none while it runs without having failed.
    pub failed: Option<bool>,
    /// How it stands: `running`, `ended`, `failed`.
    pub outcome: String,
    /// Its first start's kind and time, in milliseconds.
    pub start: Option<(&'static str, f64)>,
    /// Its other measures, each lower being better: `footprint_peak_bytes`, `footprint_steady_bytes`, `cpu_ms`.
    pub measures: BTreeMap<String, f64>,
}

/// A variant's failures: how many of its sessions are known to have failed, of those known, and the rate's sequence.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failures {
    pub failed: usize,
    pub known: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<Sequence>,
}

/// What a variant's sessions say.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Arm {
    pub name: String,
    pub assigned: usize,
    pub failures: Failures,
    /// Each start kind's share of its starts.
    pub mix: BTreeMap<String, Sequence>,
    /// Its start time's median within each start kind, in milliseconds.
    pub start_ms: BTreeMap<String, Sequence>,
    /// Its other measures' medians.
    pub measures: BTreeMap<String, Sequence>,
    pub outcomes: BTreeMap<String, usize>,
}

/// A measure on which one variant is clearly best: its sequence lies wholly below every other variant's.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Winner {
    pub measure: String,
    pub arm: String,
}

/// What a split's sessions say.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Results {
    pub arms: Vec<Arm>,
    pub winners: Vec<Winner>,
    /// The level a winner holds at, and the level each variant's sequences are held at for it.
    pub level: f64,
    pub per_arm_level: f64,
}

/// What `sessions` say of `arms` (module docs).
pub fn analyze(arms: &[String], sessions: &[Session]) -> Results {
    let k = arms.len().max(1) as f64;
    let alpha = (1.0 - LEVEL) / k;
    let mut out = Vec::new();
    for name in arms {
        let mine: Vec<&Session> = sessions.iter().filter(|s| &s.arm == name).collect();
        let known: Vec<f64> = mine.iter().filter_map(|s| s.failed).map(|f| if f { 1.0 } else { 0.0 }).collect();
        let failures = Failures { failed: known.iter().filter(|f| **f > 0.5).count(), known: known.len(), rate: sequence::rate(&known, alpha) };
        let started: Vec<(&'static str, f64)> = mine.iter().filter_map(|s| s.start).collect();
        let mut mix = BTreeMap::new();
        let mut start_ms = BTreeMap::new();
        for kind in KINDS {
            let share: Vec<f64> = started.iter().map(|(k, _)| if *k == kind { 1.0 } else { 0.0 }).collect();
            if let Some(s) = sequence::rate(&share, alpha) {
                mix.insert(kind.to_owned(), s);
            }
            let ms: Vec<f64> = started.iter().filter(|(k, _)| *k == kind).map(|(_, ms)| *ms).collect();
            if let Some(s) = sequence::median(&ms, alpha) {
                start_ms.insert(kind.to_owned(), s);
            }
        }
        let mut names: Vec<&String> = mine.iter().flat_map(|s| s.measures.keys()).collect();
        names.sort();
        names.dedup();
        let mut measures = BTreeMap::new();
        for m in names {
            let v: Vec<f64> = mine.iter().filter_map(|s| s.measures.get(m).copied()).collect();
            if let Some(s) = sequence::median(&v, alpha) {
                measures.insert(m.clone(), s);
            }
        }
        let mut outcomes = BTreeMap::new();
        for s in &mine {
            *outcomes.entry(s.outcome.clone()).or_insert(0) += 1;
        }
        out.push(Arm { name: name.clone(), assigned: mine.len(), failures, mix, start_ms, measures, outcomes });
    }
    // Each measure lower being better: a variant whose sequence lies wholly below every other's that has one.
    let mut winners = Vec::new();
    let mut readings: Vec<(String, Vec<(&str, Sequence)>)> = vec![("failure_rate".to_owned(), out.iter().filter_map(|a| Some((a.name.as_str(), a.failures.rate?))).collect())];
    for kind in KINDS {
        readings.push((format!("start_ms.{kind}"), out.iter().filter_map(|a| Some((a.name.as_str(), *a.start_ms.get(kind)?))).collect()));
    }
    let mut measure_names: Vec<&String> = out.iter().flat_map(|a| a.measures.keys()).collect();
    measure_names.sort();
    measure_names.dedup();
    for m in measure_names {
        readings.push((m.clone(), out.iter().filter_map(|a| Some((a.name.as_str(), *a.measures.get(m)?))).collect()));
    }
    for (measure, seqs) in readings {
        if seqs.len() < 2 {
            continue;
        }
        if let Some((arm, _)) = seqs.iter().find(|(n, s)| seqs.iter().filter(|(o, _)| o != n).all(|(_, t)| s.below(t))) {
            winners.push(Winner { measure, arm: (*arm).to_owned() });
        }
    }
    Results { arms: out, winners, level: LEVEL, per_arm_level: 1.0 - alpha }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P7.1 ("with weights 70/30 over N starts, the assignment proportions fall within their binomial interval"):
    /// 2,000 draws at 70/30 give the first variant a share within 3.29 standard errors of 0.7 (99.9 %), for each of
    /// several seeds; weights that aren't all above 0 draw nothing.
    #[test]
    fn p7_1_draws_fall_within_their_binomial_interval() {
        let n = 2000u64;
        for seed in [1u64, 42, 0xDEAD_BEEF] {
            let first = (0..n).filter(|i| draw(&[70.0, 30.0], seed, *i) == Some(0)).count() as f64;
            let share = first / n as f64;
            let se = (0.7f64 * 0.3 / n as f64).sqrt();
            assert!((share - 0.7).abs() <= 3.29 * se, "seed {seed}: {share}");
        }
        assert_eq!(draw(&[70.0, 0.0], 1, 0), None);
        assert_eq!(draw(&[], 1, 0), None);
        assert_eq!((0..100).map(|i| draw(&[1.0, 1.0, 1.0], 9, i)).collect::<Vec<_>>(), (0..100).map(|i| draw(&[1.0, 1.0, 1.0], 9, i)).collect::<Vec<_>>(), "the same experiment draws the same again");
    }

    /// splitmix64 units for the simulated sessions.
    fn sim(seed: u64) -> impl FnMut() -> f64 {
        let mut n = 0u64;
        move || {
            n += 1;
            unit(seed, n)
        }
    }

    /// P7.1 ("a 90/10 split of identical variants shows no start-time difference within each kind"; ROOT, "no
    /// warm-pool confound"): the variants start alike within each kind, but the 90 % one finds warm hosts far more
    /// often, as demand-sized pools give it. Within each kind no start-time winner shows; the mix tells them apart.
    #[test]
    fn p7_1_start_times_compare_within_each_kind_and_the_mix_is_its_own_measure() {
        let mut r = sim(11);
        let mut sessions = Vec::new();
        for _ in 0..600 {
            let a = r() < 0.9;
            let warm = r() < if a { 0.8 } else { 0.2 };
            let (kind, ms) = if warm { ("warm", 5.0 + 10.0 * r()) } else { ("cold", 250.0 + 100.0 * r()) };
            sessions.push(Session { arm: if a { "a".into() } else { "b".into() }, failed: Some(false), outcome: "ended".into(), start: Some((kind, ms)), ..Session::default() });
        }
        let res = analyze(&["a".to_owned(), "b".to_owned()], &sessions);
        assert!(!res.winners.iter().any(|w| w.measure.starts_with("start_ms")), "{:?}", res.winners);
        let warm = |n: &str| res.arms.iter().find(|x| x.name == n).and_then(|x| x.mix.get("warm").copied());
        assert!(matches!((warm("a"), warm("b")), (Some(a), Some(b)) if b.below(&a)), "{:?} {:?}", warm("a"), warm("b"));
        assert!(res.arms.iter().all(|a| a.start_ms.contains_key("cold") && a.start_ms.contains_key("warm")), "{res:?}");
        assert!((res.per_arm_level - 0.975).abs() < 1e-12 && res.level == 0.95);
    }

    /// P7.1 ("a failing variant's failures are counted"): every failed session counts, running ones aren't yet known,
    /// and a variant failing far more often is the other's winner on failures.
    #[test]
    fn p7_1_failures_are_counted_and_a_clearly_failing_variant_loses() {
        let mut sessions = Vec::new();
        for i in 0..300 {
            let arm = if i % 2 == 0 { "ok" } else { "broken" };
            let failed = arm == "broken" || i % 50 == 0;
            sessions.push(Session { arm: arm.into(), failed: Some(failed), outcome: if failed { "failed".into() } else { "ended".into() }, ..Session::default() });
        }
        sessions.push(Session { arm: "ok".into(), failed: None, outcome: "running".into(), ..Session::default() });
        let res = analyze(&["ok".to_owned(), "broken".to_owned()], &sessions);
        let of = |n: &str| res.arms.iter().find(|a| a.name == n).cloned();
        assert!(of("broken").is_some_and(|a| a.failures.failed == 150 && a.failures.known == 150 && a.outcomes.get("failed") == Some(&150)), "{res:?}");
        assert!(of("ok").is_some_and(|a| a.assigned == 151 && a.failures.known == 150 && a.failures.failed == 6 && a.outcomes.get("running") == Some(&1)), "{res:?}");
        assert!(res.winners.iter().any(|w| w.measure == "failure_rate" && w.arm == "ok"), "{:?}", res.winners);
    }
}
