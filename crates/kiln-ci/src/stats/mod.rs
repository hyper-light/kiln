//! Bootstrap confidence intervals for benchmarks and experiments (docs/ci.md W16-R08, W20).
//!
//! A metric is compared with its baseline by the ratio of their medians: resampling both sides
//! with replacement many times gives the ratio's distribution, and its 2.5th and 97.5th
//! percentiles are the 95 % interval. A change is reported only when the whole interval clears
//! the threshold; otherwise there is "no clear difference". Randomness comes from a seed the
//! caller passes, so a verdict is reproducible.

/// A 95 % interval of `current / baseline` (the medians' ratio), and its point estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub low: f64,
    pub mid: f64,
    pub high: f64,
}

/// Which way is better for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Better {
    /// Times, sizes, allocations.
    #[default]
    Lower,
    /// Throughput, scores.
    Higher,
}

/// What the interval says against a threshold (a fraction: `0.05` for 5 %).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Worse by at least `threshold`, all the way across the interval: `change` is the point
    /// estimate (`0.1` = 10 % worse).
    Regressed { change: f64 },
    /// Better by at least `threshold` across the interval.
    Improved { change: f64 },
    NoClearDifference,
}

/// splitmix64: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// The median of `v` (sorted in place).
pub fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let (a, b) = (v.get((n - 1) / 2).copied()?, v.get(n / 2).copied()?);
    Some((a + b) / 2.0)
}

/// The bootstrap 95 % interval of `median(current) / median(baseline)`, from `rounds` resamples
/// of each side. `None` without values on both sides, or a baseline whose median is zero.
pub fn ratio_interval(baseline: &[f64], current: &[f64], rounds: usize, seed: u64) -> Option<Interval> {
    let finite = |v: &[f64]| v.iter().copied().filter(|x| x.is_finite()).collect::<Vec<f64>>();
    let (base, cur) = (finite(baseline), finite(current));
    let mid = median(&mut cur.clone())? / median(&mut base.clone()).filter(|m| *m != 0.0)?;
    let mut rng = Rng(seed);
    let mut ratios = Vec::with_capacity(rounds);
    let (mut b, mut c) = (vec![0.0; base.len()], vec![0.0; cur.len()]);
    for _ in 0..rounds.max(1) {
        for x in &mut b {
            *x = base.get(rng.below(base.len())).copied().unwrap_or(0.0);
        }
        for x in &mut c {
            *x = cur.get(rng.below(cur.len())).copied().unwrap_or(0.0);
        }
        let (mb, mc) = (median(&mut b)?, median(&mut c)?);
        if mb != 0.0 {
            ratios.push(mc / mb);
        }
    }
    ratios.sort_by(f64::total_cmp);
    let at = |p: f64| ratios.get(((ratios.len() - 1) as f64 * p).round() as usize).copied();
    Some(Interval { low: at(0.025)?, mid, high: at(0.975)? })
}

/// Judges an interval against `threshold` for a metric where `better` is better.
pub fn judge(i: Interval, threshold: f64, better: Better) -> Verdict {
    let (worse_low, worse_mid, better_high) = match better {
        // Lower is better: a ratio above 1 + threshold everywhere is a regression.
        Better::Lower => (i.low - 1.0, i.mid - 1.0, 1.0 - i.high),
        Better::Higher => (1.0 - i.high, 1.0 - i.mid, i.low - 1.0),
    };
    if worse_low >= threshold {
        Verdict::Regressed { change: worse_mid }
    } else if better_high >= threshold {
        Verdict::Improved { change: -worse_mid }
    } else {
        Verdict::NoClearDifference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples around `center` with relative noise `noise`, from a seed.
    fn samples(center: f64, noise: f64, n: usize, seed: u64) -> Vec<f64> {
        let mut r = Rng(seed);
        (0..n).map(|_| center * (1.0 + noise * ((r.next() % 2001) as f64 / 1000.0 - 1.0))).collect()
    }

    /// W16-AC04: a 10 % slowdown with little noise is flagged, a 2 % one with much noise is "no
    /// clear difference", and a gate at 5 % fails the first and passes the second.
    #[test]
    fn w16_ac04_regressions() {
        let base = samples(100.0, 0.01, 30, 1);
        let slow = samples(110.0, 0.01, 30, 2);
        let i = ratio_interval(&base, &slow, 2000, 7).unwrap_or(Interval { low: 0.0, mid: 0.0, high: 0.0 });
        assert!(i.low > 1.07 && i.high < 1.13, "{i:?}");
        assert!(matches!(judge(i, 0.05, Better::Lower), Verdict::Regressed { change } if (change - 0.10).abs() < 0.02), "{:?}", judge(i, 0.05, Better::Lower));
        let noisy_base = samples(100.0, 0.15, 12, 3);
        let noisy = samples(102.0, 0.15, 12, 4);
        let j = ratio_interval(&noisy_base, &noisy, 2000, 7).unwrap_or(Interval { low: 0.0, mid: 0.0, high: 0.0 });
        assert_eq!(judge(j, 0.05, Better::Lower), Verdict::NoClearDifference, "{j:?}");
        // The same seed, the same interval.
        assert_eq!(ratio_interval(&base, &slow, 2000, 7), Some(i));
        // Higher is better: the slowdown of a throughput is an improvement read the other way.
        assert!(matches!(judge(i, 0.05, Better::Higher), Verdict::Improved { .. }));
        assert_eq!(ratio_interval(&[], &slow, 100, 1), None);
        assert_eq!(ratio_interval(&[0.0, 0.0], &slow, 100, 1), None);
    }
}
