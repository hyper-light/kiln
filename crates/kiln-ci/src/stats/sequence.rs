//! Anytime-valid confidence sequences (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1): what a live experiment's
//! measures say at every look, so reading them after every session inflates no false winner.
//!
//! - **A rate** (a mean of observations in [0, 1]: failures, a start kind's share) has the hedged betting confidence
//!   sequence of Waudby-Smith and Ramdas, "Estimating means of bounded random variables by betting" (JRSS B, 2024;
//!   arXiv:2010.09686, §4, Theorem 3), with its predictable plug-in bets (their eq. (26)–(27)): for each candidate
//!   mean `m`, two capital processes bet that the mean is above and below `m`, each bet `λ_i = sqrt(2 log(2/α) /
//!   (σ̂²_{i-1} i log(1 + i)))` truncated at `c/m` and `c/(1 - m)` with their `c = 1/2`, `σ̂²` and `μ̂` their
//!   regularised running variance and mean (priors 1/4 and 1/2). `m` is in the sequence at time `t` while their
//!   average stays below `1/α`: by Ville's inequality the true mean is outside at any time with probability at most
//!   `α`, however often it's looked at.
//! - **A median** of any measure has the quantile construction of Howard and Ramdas, "Sequential estimation of
//!   quantiles with applications to A/B-testing and best-arm identification" (Bernoulli, 2022; arXiv:1906.09712,
//!   §2): at the true median `q`, `1{X_i ≤ q}` are Bernoulli(1/2), so `x` is in the sequence while the betting test of
//!   mean 1/2 on `1{X_i ≤ x}` keeps it. Those indicators change only at the observed values, so the sequence is read
//!   off the regions between them: its ends are observed values, or unbounded while too few say otherwise.
//!
//! Every observation is read in the order it came (a session's, as its experiment saw it). Nothing is picked but
//! `α`, the caller's.

use serde::Serialize;

/// A confidence sequence's reading at the latest time: its point estimate and its ends, either of which may be
/// unbounded (`None`) while too few observations bound it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Sequence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<f64>,
    pub mid: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<f64>,
}

impl Sequence {
    /// Whether this lies wholly below `other`: each end bounded, and its high under the other's low.
    pub fn below(&self, other: &Sequence) -> bool {
        matches!((self.high, other.low), (Some(h), Some(l)) if h < l)
    }
}

/// The bets' truncation (Waudby-Smith and Ramdas, §4: `c = 1/2`).
const TRUNCATE: f64 = 0.5;

/// Whether the hedged capital betting against mean `m` of `ys` (each in [0, 1], in order) reached `1/α` by its last
/// observation: `m` is out of the sequence.
fn rejects(ys: &[f64], m: f64, alpha: f64) -> bool {
    let log_term = 2.0 * (2.0 / alpha).ln();
    let (mut up, mut down) = (1.0f64, 1.0f64);
    let (mut sum, mut squares) = (0.0f64, 0.0f64);
    let mut var_before = 0.25;
    for (k, y) in ys.iter().enumerate() {
        let i = (k + 1) as f64;
        let bet = (log_term / (var_before * i * (1.0 + i).ln())).sqrt();
        let up_bet = if m > 0.0 { bet.min(TRUNCATE / m) } else { bet };
        let down_bet = if m < 1.0 { bet.min(TRUNCATE / (1.0 - m)) } else { bet };
        up *= 1.0 + up_bet * (y - m);
        down *= 1.0 - down_bet * (y - m);
        sum += y;
        let mean = (0.5 + sum) / (i + 1.0);
        squares += (y - mean) * (y - mean);
        var_before = (0.25 + squares) / (i + 1.0);
    }
    0.5 * up + 0.5 * down >= 1.0 / alpha
}

/// Bisection to the edge between `inside` (in the sequence) and `outside`, to f64's resolution.
fn edge(ys: &[f64], alpha: f64, mut inside: f64, mut outside: f64) -> f64 {
    for _ in 0..f64::MANTISSA_DIGITS {
        let mid = 0.5 * (inside + outside);
        if rejects(ys, mid, alpha) {
            outside = mid;
        } else {
            inside = mid;
        }
    }
    inside
}

/// The confidence sequence for the mean of `ys` (each in [0, 1], in the order they came) at level `1 - alpha`; none
/// without an observation, or with one outside [0, 1].
pub fn rate(ys: &[f64], alpha: f64) -> Option<Sequence> {
    if ys.is_empty() || ys.iter().any(|y| !(0.0..=1.0).contains(y)) || !(alpha > 0.0 && alpha < 1.0) {
        return None;
    }
    let mid = ys.iter().sum::<f64>() / ys.len() as f64;
    let low = if rejects(ys, 0.0, alpha) { edge(ys, alpha, mid, 0.0) } else { 0.0 };
    let high = if rejects(ys, 1.0, alpha) { edge(ys, alpha, mid, 1.0) } else { 1.0 };
    Some(Sequence { low: Some(low), mid, high: Some(high) })
}

/// The confidence sequence for the median of `xs` (finite values, in the order they came) at level `1 - alpha`.
pub fn median(xs: &[f64], alpha: f64) -> Option<Sequence> {
    if xs.is_empty() || xs.iter().any(|x| !x.is_finite()) || !(alpha > 0.0 && alpha < 1.0) {
        return None;
    }
    let mut sorted = xs.to_vec();
    let mid = super::median(&mut sorted)?;
    sorted.dedup();
    // Region 0 is below every value; region j (1..=d) runs from the j-th distinct value up to the next.
    let kept = |j: usize| -> bool {
        let at = if j == 0 { None } else { sorted.get(j - 1).copied() };
        let ys: Vec<f64> = xs.iter().map(|x| if at.is_some_and(|a| *x <= a) { 1.0 } else { 0.0 }).collect();
        !rejects(&ys, 0.5, alpha)
    };
    let d = sorted.len();
    let first = (0..=d).find(|j| kept(*j));
    let last = (0..=d).rev().find(|j| kept(*j));
    let (low, high) = match (first, last) {
        (Some(f), Some(l)) => (if f == 0 { None } else { sorted.get(f - 1).copied() }, if l == d { None } else { sorted.get(l).copied() }),
        // No region keeps it (the sequence's test can, rarely, refuse every one): the point alone.
        _ => (Some(mid), Some(mid)),
    };
    Some(Sequence { low, mid, high })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64, from a seed.
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// P7.1: a rate's sequence holds its truth, narrows as it learns, and stays within [0, 1]; a median's holds its
    /// truth and is unbounded on a side too few observations bound.
    #[test]
    fn p7_1_sequences_hold_their_truth_and_narrow() {
        let mut r = Rng(5);
        let ys: Vec<f64> = (0..400).map(|_| if r.unit() < 0.3 { 1.0 } else { 0.0 }).collect();
        let few = rate(ys.get(..40).unwrap_or_default(), 0.05).unwrap_or(Sequence { low: None, mid: 0.0, high: None });
        let many = rate(&ys, 0.05).unwrap_or(Sequence { low: None, mid: 0.0, high: None });
        for s in [few, many] {
            assert!(s.low.is_some_and(|l| (0.0..0.3).contains(&l)) && s.high.is_some_and(|h| h > 0.3 && h <= 1.0), "{s:?}");
        }
        let width = |s: Sequence| s.high.unwrap_or(1.0) - s.low.unwrap_or(0.0);
        assert!(width(many) < width(few), "{many:?} {few:?}");
        let xs: Vec<f64> = (0..300).map(|_| 100.0 + 50.0 * r.unit()).collect();
        let m = median(&xs, 0.05).unwrap_or(Sequence { low: None, mid: 0.0, high: None });
        assert!(m.low.is_some_and(|l| l < 125.0) && m.high.is_some_and(|h| h > 125.0) && m.high.unwrap_or(0.0) - m.low.unwrap_or(0.0) < 25.0, "{m:?}");
        let two = median(&[1.0, 2.0], 0.05).unwrap_or(Sequence { low: Some(0.0), mid: 0.0, high: Some(0.0) });
        assert!(two.low.is_none() && two.high.is_none(), "two values bound nothing: {two:?}");
        assert_eq!(rate(&[], 0.05), None);
        assert_eq!(rate(&[2.0], 0.05), None);
        assert_eq!(median(&[f64::NAN], 0.05), None);
    }

    /// P7.1's A/A check (ROOT, 2026-11-14, "no peeking errors"): two identical variants, 70/30, looked at after every
    /// session, over many seeded experiments. A winner is declared when one's sequence lies wholly below the other's,
    /// each held at α/2 so a winner holds at 95 %. Across 200 experiments of 150 sessions each, a failure rate and a
    /// measure's median, at most 5 % ever declare one at any look.
    #[test]
    fn p7_1_an_aa_experiment_looked_at_after_every_session_declares_no_winner_at_its_rate() {
        let alpha = 0.05 / 2.0;
        let (runs, sessions) = (200usize, 150usize);
        let (mut rate_winners, mut median_winners) = (0usize, 0usize);
        for seed in 0..runs as u64 {
            let mut r = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 7);
            let (mut a_fail, mut b_fail, mut a_ms, mut b_ms) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let (mut rate_won, mut median_won) = (false, false);
            for n in 0..sessions {
                let to_a = r.unit() < 0.7;
                let failed = if r.unit() < 0.2 { 1.0 } else { 0.0 };
                let ms = 200.0 + 100.0 * r.unit();
                if to_a {
                    a_fail.push(failed);
                    a_ms.push(ms);
                } else {
                    b_fail.push(failed);
                    b_ms.push(ms);
                }
                if !rate_won && let (Some(a), Some(b)) = (rate(&a_fail, alpha), rate(&b_fail, alpha)) {
                    rate_won = a.below(&b) || b.below(&a);
                }
                // The medians' sequences cost a pass a region: every fifth session's look, and the last.
                if !median_won && (n % 5 == 4 || n + 1 == sessions) && let (Some(a), Some(b)) = (median(&a_ms, alpha), median(&b_ms, alpha)) {
                    median_won = a.below(&b) || b.below(&a);
                }
            }
            rate_winners += usize::from(rate_won);
            median_winners += usize::from(median_won);
        }
        assert!(rate_winners * 20 <= runs, "{rate_winners} of {runs} declared a failure-rate winner");
        assert!(median_winners * 20 <= runs, "{median_winners} of {runs} declared a median winner");
    }
}
