//! The statistics one run's series is judged by, each from the literature rather than a picked
//! threshold:
//!
//! - **Warm-up**: MSER-5 (White 1997, "An effective truncation heuristic for bias reduction in
//!   simulation output", Simulation 69(6); White, Cobb and Spratt 2000; the best of the 42 methods
//!   Hoad, Robinson and Davies compared, JORS 61, 2010). Means of batches of 5 in send order; the
//!   truncation `d` (at most half the batches) minimises the marginal standard error of what's left,
//!   `sum((z_j - mean)^2) / (m - d)^2`.
//! - **Sustained or not**: the Mann-Kendall test for a monotonic trend (Mann 1945; Kendall 1975) on
//!   the batch means of what MSER-5 left, `floor(sqrt(n))` batches (the batch-means method's usual
//!   count, so batches grow with the run and their means lose the series' autocorrelation). An
//!   upward trend significant at the 95% level means the backlog grew: the rate wasn't sustained.
//! - **A quantile's interval**: the distribution-free one from order statistics (the binomial's
//!   normal approximation: ranks `n p -/+ z sqrt(n p (1 - p))`), at 95%.

/// The standard normal's 97.5th percentile: two-sided 95%, the level every interval here uses.
pub const Z95: f64 = 1.959_963_984_540_054;

/// MSER-5's truncation point, in observations (a multiple of 5): how many of the first `xs` to
/// drop as warm-up. Zero for a series too short to have two batches.
pub fn mser5(xs: &[f64]) -> usize {
    const BATCH: usize = 5;
    let z: Vec<f64> = xs.as_chunks::<BATCH>().0.iter().map(|c| c.iter().sum::<f64>() / BATCH as f64).collect();
    let m = z.len();
    if m < 2 {
        return 0;
    }
    // Suffix sums of z and z^2: the sum of squares about the mean of z[d..] in O(1) per d.
    let mut suf = vec![(0.0f64, 0.0f64); m + 1];
    for j in (0..m).rev() {
        let v = z.get(j).copied().unwrap_or(0.0);
        let next = suf.get(j + 1).copied().unwrap_or((0.0, 0.0));
        if let Some(s) = suf.get_mut(j) {
            *s = (next.0 + v, next.1 + v * v);
        }
    }
    let mut best = (f64::INFINITY, 0usize);
    for d in 0..=m / 2 {
        let k = (m - d) as f64;
        let (s1, s2) = suf.get(d).copied().unwrap_or((0.0, 0.0));
        let ss = (s2 - s1 * s1 / k).max(0.0);
        let mser = ss / (k * k);
        if mser < best.0 {
            best = (mser, d);
        }
    }
    best.1 * BATCH
}

/// What the Mann-Kendall test says of a series.
#[derive(Debug, Clone, Copy, Default)]
pub struct Trend {
    /// Kendall's tau between position and value: -1 (always falling) to 1 (always rising).
    pub tau: f64,
    /// The continuity-corrected normal score of S.
    pub z: f64,
    /// Two-sided p-value.
    pub p: f64,
    /// How many batch means it was computed on.
    pub batches: usize,
}

impl Trend {
    /// A rise significant at 95%: the backlog grew over the run.
    pub fn rising(&self) -> bool {
        self.z > Z95
    }
}

/// Mann-Kendall on `floor(sqrt(n))` batch means of `xs`.
pub fn trend(xs: &[f64]) -> Trend {
    let n = xs.len();
    let b = (n as f64).sqrt().floor() as usize;
    if b < 3 {
        return Trend { p: 1.0, batches: b, ..Trend::default() };
    }
    let size = n / b;
    let y: Vec<f64> = xs.chunks_exact(size).take(b).map(|c| c.iter().sum::<f64>() / size as f64).collect();
    let mut s: i64 = 0;
    for (i, a) in y.iter().enumerate() {
        for c in y.iter().skip(i + 1) {
            s += match c.partial_cmp(a) {
                Some(std::cmp::Ordering::Greater) => 1,
                Some(std::cmp::Ordering::Less) => -1,
                _ => 0,
            };
        }
    }
    let bf = b as f64;
    let var = bf * (bf - 1.0) * (2.0 * bf + 5.0) / 18.0;
    let sf = s as f64;
    let z = if s > 0 {
        (sf - 1.0) / var.sqrt()
    } else if s < 0 {
        (sf + 1.0) / var.sqrt()
    } else {
        0.0
    };
    Trend { tau: sf / (bf * (bf - 1.0) / 2.0), z, p: erfc(z.abs() / std::f64::consts::SQRT_2), batches: b }
}

/// The complementary error function (Numerical Recipes' `erfcc`, Chebyshev fitting: fractional
/// error under 1.2e-7 everywhere).
pub fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96 + t * (0.096_784_18 + t * (-0.186_288_06 + t * (0.278_868_07 + t * (-1.135_203_98 + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let r = t * poly.exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// The 95% distribution-free interval of quantile `p` of the sorted sample `xs`: the values at the
/// order statistics bounding it (the binomial's normal approximation, ranks clamped to the sample).
pub fn quantile_ci(xs: &[u64], p: f64) -> (u64, u64) {
    let n = xs.len();
    if n == 0 {
        return (0, 0);
    }
    let nf = n as f64;
    let half = Z95 * (nf * p * (1.0 - p)).sqrt();
    let lo = ((nf * p - half).floor().max(1.0) as usize).min(n);
    let hi = ((nf * p + half).ceil().max(1.0) as usize).min(n);
    (xs.get(lo - 1).copied().unwrap_or(0), xs.get(hi - 1).copied().unwrap_or(0))
}

/// The value at or below which a share `p` of the sorted sample lies (nearest rank).
pub fn quantile(xs: &[u64], p: f64) -> u64 {
    let n = xs.len();
    if n == 0 {
        return 0;
    }
    let r = ((p * n as f64).ceil() as usize).clamp(1, n);
    xs.get(r - 1).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LB14: a warm-up that decays is cut, a flat series isn't cut much.
    #[test]
    fn lb14_mser5_cuts_a_decaying_start() {
        let mut xs: Vec<f64> = (0..500).map(|i| 1000.0 * (-(i as f64) / 40.0).exp()).collect();
        xs.extend((0..4500).map(|i| 10.0 + ((i * 7919) % 13) as f64));
        let d = mser5(&xs);
        assert!((100..=1000).contains(&d), "cut {d}");
        let flat: Vec<f64> = (0..5000).map(|i| 10.0 + ((i * 7919) % 13) as f64).collect();
        assert!(mser5(&flat) < 2500, "a flat series keeps most of itself: cut {}", mser5(&flat));
    }

    /// LB14: a backlog that grows is a rising trend; a stationary series isn't.
    #[test]
    fn lb14_trend_sees_a_growing_backlog() {
        let rising: Vec<f64> = (0..10_000).map(|i| i as f64 * 0.01 + ((i * 7919) % 13) as f64).collect();
        assert!(trend(&rising).rising(), "{:?}", trend(&rising));
        let flat: Vec<f64> = (0..10_000).map(|i| ((i * 7919) % 13) as f64).collect();
        assert!(!trend(&flat).rising(), "{:?}", trend(&flat));
    }

    #[test]
    fn lb14_erfc_and_quantiles() {
        assert!((erfc(0.0) - 1.0).abs() < 1e-6);
        assert!((erfc(Z95 / std::f64::consts::SQRT_2) - 0.05).abs() < 1e-6);
        let xs: Vec<u64> = (1..=1000).collect();
        assert_eq!(quantile(&xs, 0.5), 500);
        let (lo, hi) = quantile_ci(&xs, 0.5);
        assert!(lo < 500 && hi > 500 && hi - lo < 80, "{lo} {hi}");
    }
}
