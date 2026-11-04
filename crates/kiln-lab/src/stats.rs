//! The noise the lab measures, and the differences it calls. A round is the unit: every number a
//! round reports is one draw, and a run's number is the median of its rounds'. Its noise is how far
//! that median moves when the rounds are drawn again (a bootstrap of the rounds); a difference
//! between two runs is called only where its interval, at a level that holds across every number
//! compared at once (Bonferroni's), leaves out none.

use serde::{Deserialize, Serialize};

use crate::standin::Lcg;

/// Bootstrap draws: enough that the extreme quantiles a family-wide level asks for (for 40 numbers
/// at 95%, the 0.06th percentile) are still a few draws in.
const DRAWS: usize = 8000;

fn median(sorted: &[f64]) -> Option<f64> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let (a, b) = (sorted.get((n - 1) / 2)?, sorted.get(n / 2)?);
    Some(f64::midpoint(*a, *b))
}

fn sorted(v: &[f64]) -> Vec<f64> {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    s.sort_by(f64::total_cmp);
    s
}

/// `q` of sorted `v`, by the nearest rank.
fn at(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// The median of a draw (with replacement) from `v`.
fn draw_median(v: &[f64], r: &mut Lcg, scratch: &mut Vec<f64>) -> Option<f64> {
    scratch.clear();
    for _ in 0..v.len() {
        let i = usize::try_from(r.below(v.len() as u64)).unwrap_or(0);
        scratch.push(v.get(i).copied()?);
    }
    scratch.sort_by(f64::total_cmp);
    median(scratch)
}

/// A seed of a number's own, from its name: the same rounds give the same interval.
pub fn seed_of(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// A number across a run's rounds: its median, and the interval the median moves in when the
/// rounds are drawn again (`level`, e.g. 0.95).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub median: f64,
    pub lo: f64,
    pub hi: f64,
    pub rounds: usize,
}

pub fn summarize(values: &[f64], level: f64, seed: u64) -> Option<Summary> {
    let v = sorted(values);
    let m = median(&v)?;
    let mut r = Lcg(seed);
    let mut scratch = Vec::with_capacity(v.len());
    let mut meds: Vec<f64> = (0..DRAWS).filter_map(|_| draw_median(&v, &mut r, &mut scratch)).collect();
    meds.sort_by(f64::total_cmp);
    let tail = (1.0 - level) / 2.0;
    Some(Summary { median: m, lo: at(&meds, tail).unwrap_or(m), hi: at(&meds, 1.0 - tail).unwrap_or(m), rounds: v.len() })
}

/// Which way a number is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    Lower,
    Higher,
}

/// What a difference came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Within the noise.
    Same,
    Better,
    Worse,
}

/// B's median less A's, with the interval it moves in when both runs' rounds are drawn again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub a: f64,
    pub b: f64,
    pub delta: f64,
    pub lo: f64,
    pub hi: f64,
    /// The difference as a share of A's median (none where A's is 0).
    pub rel: Option<f64>,
    /// The interval's half-width as a share of A's median: the noise of the comparison.
    pub noise: Option<f64>,
    pub verdict: Verdict,
}

/// B against A at `level` (each number's own; a caller comparing many lowers it for all of them).
pub fn compare(a: &[f64], b: &[f64], level: f64, better: Better, seed: u64) -> Option<Diff> {
    let (va, vb) = (sorted(a), sorted(b));
    let (ma, mb) = (median(&va)?, median(&vb)?);
    let mut r = Lcg(seed);
    let (mut sa, mut sb) = (Vec::with_capacity(va.len()), Vec::with_capacity(vb.len()));
    let mut deltas: Vec<f64> = (0..DRAWS).filter_map(|_| Some(draw_median(&vb, &mut r, &mut sb)? - draw_median(&va, &mut r, &mut sa)?)).collect();
    deltas.sort_by(f64::total_cmp);
    let tail = (1.0 - level) / 2.0;
    let delta = mb - ma;
    let (lo, hi) = (at(&deltas, tail).unwrap_or(delta), at(&deltas, 1.0 - tail).unwrap_or(delta));
    let verdict = if lo > 0.0 || hi < 0.0 {
        match (delta < 0.0, better) {
            (true, Better::Lower) | (false, Better::Higher) => Verdict::Better,
            _ => Verdict::Worse,
        }
    } else {
        Verdict::Same
    };
    let rel = (ma != 0.0).then(|| delta / ma.abs());
    let noise = (ma != 0.0).then(|| (hi - lo) / 2.0 / ma.abs());
    Some(Diff { a: ma, b: mb, delta, lo, hi, rel, noise, verdict })
}

/// The level each of `m` numbers is held to, so all `m` together hold at `family` (Bonferroni).
pub fn each_level(family: f64, m: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let m = m.max(1) as f64;
    1.0 - (1.0 - family) / m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runs_noise_holds_its_median() {
        let s = summarize(&[10.0, 11.0, 9.0, 10.5, 9.5, 10.2, 9.8, 10.1], 0.95, 1).unwrap_or(Summary { median: 0.0, lo: 0.0, hi: 0.0, rounds: 0 });
        assert!((s.median - 10.05).abs() < 1e-9, "{s:?}");
        assert!(s.lo <= s.median && s.median <= s.hi && s.hi - s.lo < 2.0, "{s:?}");
        assert_eq!(s.rounds, 8);
    }

    /// The same draws are within the noise; a shift well past it is called, which way it's better.
    #[test]
    fn a_difference_is_called_only_past_the_noise() {
        let a = [10.0, 11.0, 9.0, 10.5, 9.5, 10.2, 9.8, 10.1];
        let same = [10.1, 10.9, 9.2, 10.4, 9.6, 10.0, 9.9, 10.3];
        let level = each_level(0.95, 40);
        assert_eq!(compare(&a, &same, level, Better::Lower, 7).map(|d| d.verdict), Some(Verdict::Same));
        let slower: Vec<f64> = a.iter().map(|x| x * 2.0).collect();
        let d = compare(&a, &slower, level, Better::Lower, 7);
        assert_eq!(d.as_ref().map(|d| d.verdict), Some(Verdict::Worse), "{d:?}");
        assert_eq!(compare(&a, &slower, level, Better::Higher, 7).map(|d| d.verdict), Some(Verdict::Better));
        assert!(d.and_then(|d| d.rel).is_some_and(|r| (r - 1.0).abs() < 0.05));
    }
}
