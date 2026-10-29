//! Distributions (`docs/observe.md` §4.4): base-2 exponential histograms, OTel's, merged exactly
//! across hosts and time and read as percentiles, the way Datadog's distribution metrics are
//! (DDSketch: Masson, Rim and Lee, VLDB 2019).
//!
//! A value `v > 0` falls in bucket `i` at scale `s` when `base^i < v ≤ base^(i+1)`, `base =
//! 2^(2^-s)` (OTel's `ExponentialHistogramDataPoint`); a negative value in the mirrored negative
//! bucket; zero (or what an SDK's `zero_threshold` holds) in the zero bucket. A bucket at scale `s`
//! is bucket `i >> k` at scale `s - k` (the floor of `i / 2^k`): histograms of different scales merge
//! exactly at the coarser one. Kiln keeps a distribution's buckets at scale [`SCALE`] at the finest.
//!
//! Each bucket is a series of its own (`<metric>.dist{dist_bucket:5:p37}`, its count an interval), summed across series and time as any count is, and a percentile is read from the
//! merged counts at query time: the bucket holding the rank, at its geometric middle,
//! `base^(i+½)`, within `(√base - 1)` of any value in it (1.09% at scale 5; Datadog's distributions
//! promise 1%).

use std::collections::BTreeMap;

use crate::batch::{BatchError, Kind, SeriesKey};

/// The finest scale kept: 32 buckets an octave, a percentile within 1.09% of the truth (scale 6
/// would halve that and double the buckets). OTel SDKs start at 20 and shrink to fit 160 buckets,
/// most often to 3 to 5 for a latency's range.
pub const SCALE: i32 = 5;
/// The coarsest scale an SDK may send that kiln keeps as it came (OTel's least is -10).
pub const MIN_SCALE: i32 = -10;

/// The tag a bucket's series carries: its scale and which bucket, `<scale>:<bucket>` (`5:p37`,
/// `5:n4`, `5:z`).
pub const BUCKET_TAG: &str = "dist_bucket";
/// The suffix of a distribution's buckets' metric.
pub const SUFFIX: &str = ".dist";

/// A bucket: positive (`Pos(i)`: `(base^i, base^(i+1)]`), negative (mirrored), or zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bucket {
    /// Ordered before `Zero` and positives: the larger the index the smaller the value, so they
    /// sort by `Reverse`d index ([`Bucket::order`]).
    Neg(i64),
    Zero,
    Pos(i64),
}

impl Bucket {
    /// The bucket's tag value: `p37`, `n4`, `z`.
    pub fn tag(self) -> String {
        match self {
            Self::Pos(i) => format!("p{i}"),
            Self::Neg(i) => format!("n{i}"),
            Self::Zero => "z".to_owned(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.split_at_checked(1)? {
            ("z", "") => Some(Self::Zero),
            ("p", i) => i.parse().ok().map(Self::Pos),
            ("n", i) => i.parse().ok().map(Self::Neg),
            _ => None,
        }
    }

    /// The same bucket `k` scales coarser.
    pub fn coarser(self, k: u32) -> Self {
        match self {
            Self::Pos(i) => Self::Pos(i >> k.min(63)),
            Self::Neg(i) => Self::Neg(i >> k.min(63)),
            Self::Zero => Self::Zero,
        }
    }

    /// Its place ascending by value: the negatives' larger magnitudes first, then zero, then the
    /// positives (the derived order is the variants' and indices', not the values').
    pub fn order(self) -> (u8, i64) {
        match self {
            Self::Neg(i) => (0, i.saturating_neg()),
            Self::Zero => (1, 0),
            Self::Pos(i) => (2, i),
        }
    }

    /// The bucket's value as a percentile reads it: its geometric middle (zero for zero).
    pub fn value(self, scale: i32) -> f64 {
        let mid = |i: i64| ((i as f64 + 0.5) * (2f64).powi(-scale)).exp2();
        match self {
            Self::Pos(i) => mid(i),
            Self::Neg(i) => -mid(i),
            Self::Zero => 0.0,
        }
    }

    /// The bucket's bounds, `(lower, upper]` (for a negative bucket, `[lower, upper)`).
    pub fn bounds(self, scale: i32) -> (f64, f64) {
        let edge = |i: i64| (i as f64 * (2f64).powi(-scale)).exp2();
        match self {
            Self::Pos(i) => (edge(i), edge(i + 1)),
            Self::Neg(i) => (-edge(i + 1), -edge(i)),
            Self::Zero => (0.0, 0.0),
        }
    }
}

/// A bucket's tag value: its scale and the bucket.
pub fn tag_value(scale: i32, b: Bucket) -> String {
    format!("{scale}:{}", b.tag())
}

/// [`tag_value`]'s inverse.
pub fn parse_tag_value(s: &str) -> Option<(i32, Bucket)> {
    let (scale, b) = s.split_once(':')?;
    let scale: i32 = scale.parse().ok().filter(|s| (MIN_SCALE..=SCALE).contains(s))?;
    Some((scale, Bucket::parse(b)?))
}

/// An exponential histogram's buckets at `scale` coarsened to at most [`SCALE`]: each with its
/// count, in order, those of none left out, and the scale they are at.
pub fn coarsened(scale: i32, zero: u64, pos_off: i64, pos: &[u64], neg_off: i64, neg: &[u64], out: &mut Vec<(Bucket, u64)>) -> i32 {
    let to = scale.clamp(MIN_SCALE, SCALE);
    let k = u32::try_from(scale.clamp(MIN_SCALE, 20) - to).unwrap_or(0);
    out.clear();
    let mut put = |b: Bucket, c: u64| {
        if c == 0 {
            return;
        }
        match out.last_mut() {
            Some((last, n)) if *last == b => *n = n.saturating_add(c),
            _ => out.push((b, c)),
        }
    };
    // Ascending by value: negatives from the largest magnitude down.
    for (j, &c) in neg.iter().enumerate().rev() {
        put(Bucket::Neg(neg_off.saturating_add(j as i64)).coarser(k), c);
    }
    put(Bucket::Zero, zero);
    for (j, &c) in pos.iter().enumerate() {
        put(Bucket::Pos(pos_off.saturating_add(j as i64)).coarser(k), c);
    }
    to
}

/// The bucket `v` falls in at `scale` (OTel's mapping: `base^i < |v| ≤ base^(i+1)`).
pub fn bucket_of(v: f64, scale: i32) -> Option<Bucket> {
    if !v.is_finite() {
        return None;
    }
    if v == 0.0 {
        return Some(Bucket::Zero);
    }
    let m = v.abs();
    // ceil(log2(m) · 2^scale) - 1, corrected at the edges where the logarithm rounds.
    let k = (2f64).powi(scale);
    let mut i = (m.log2() * k).ceil() as i64 - 1;
    let edge = |i: i64| (i as f64 / k).exp2();
    while i > i64::MIN / 2 && edge(i) >= m {
        i -= 1;
    }
    while edge(i + 1) < m {
        i += 1;
    }
    Some(if v > 0.0 { Bucket::Pos(i) } else { Bucket::Neg(i) })
}

/// Buckets' counts merged, at one scale.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Merged {
    pub scale: i32,
    pub counts: BTreeMap<(u8, i64), (Bucket, f64)>,
}

impl Merged {
    pub fn new(scale: i32) -> Self {
        Self { scale, counts: BTreeMap::new() }
    }

    /// Adds `n` to `b` (a bucket at `scale`, coarsened to this one's if finer; a coarser one makes
    /// this one coarser first).
    pub fn add(&mut self, b: Bucket, scale: i32, n: f64) {
        if !(n.is_finite() && n > 0.0) {
            return;
        }
        if scale < self.scale {
            self.coarsen(scale);
        }
        let b = b.coarser(u32::try_from(scale - self.scale).unwrap_or(0));
        self.counts.entry(b.order()).or_insert((b, 0.0)).1 += n;
    }

    /// This, at the coarser `scale`.
    pub fn coarsen(&mut self, scale: i32) {
        if scale >= self.scale {
            return;
        }
        let k = u32::try_from(self.scale - scale).unwrap_or(0);
        let old = std::mem::take(&mut self.counts);
        self.scale = scale;
        for (_, (b, n)) in old {
            let c = b.coarser(k);
            self.counts.entry(c.order()).or_insert((c, 0.0)).1 += n;
        }
    }

    pub fn total(&self) -> f64 {
        self.counts.values().map(|(_, n)| n).sum()
    }

    /// The `q` quantile (0–1), DDSketch's way: the bucket holding the value of rank `q (n - 1)`, at
    /// its middle; `bounds` (the exact least and most, where known) clamp it.
    pub fn quantile(&self, q: f64, bounds: Option<(f64, f64)>) -> Option<f64> {
        let total = self.total();
        if total <= 0.0 || !(0.0..=1.0).contains(&q) {
            return None;
        }
        let rank = q * (total - 1.0).max(0.0);
        let mut seen = 0.0;
        for (b, n) in self.counts.values() {
            seen += n;
            if rank < seen {
                let v = b.value(self.scale);
                return Some(match bounds {
                    Some((lo, hi)) if lo.is_finite() && hi.is_finite() && lo <= hi => v.clamp(lo, hi),
                    _ => v,
                });
            }
        }
        self.counts.values().next_back().map(|(b, _)| b.value(self.scale))
    }
}

/// A distribution's series of raw values (Datadog's `distribution_points`: each point a time and
/// its values): its buckets' counts at [`SCALE`] (`<metric>.dist`), and its `.count`, `.sum`,
/// `.min` and `.max`, as an exponential histogram's are.
pub fn series_of(metric: &str, tags: &[(&str, &str)], points: &[(i64, Vec<f64>)]) -> Result<Vec<(SeriesKey, Vec<(i64, f64)>)>, BatchError> {
    let mut buckets: BTreeMap<(u8, i64), (Bucket, Vec<(i64, f64)>)> = BTreeMap::new();
    let (mut count, mut sum, mut min, mut max) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (t, values) in points {
        let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        if finite.is_empty() {
            continue;
        }
        let mut here: BTreeMap<(u8, i64), (Bucket, f64)> = BTreeMap::new();
        for &v in &finite {
            if let Some(b) = bucket_of(v, SCALE) {
                here.entry(b.order()).or_insert((b, 0.0)).1 += 1.0;
            }
        }
        for (order, (b, n)) in here {
            buckets.entry(order).or_insert_with(|| (b, Vec::new())).1.push((*t, n));
        }
        count.push((*t, finite.len() as f64));
        sum.push((*t, finite.iter().sum()));
        min.push((*t, finite.iter().copied().fold(f64::INFINITY, f64::min)));
        max.push((*t, finite.iter().copied().fold(f64::NEG_INFINITY, f64::max)));
    }
    let mut out = Vec::with_capacity(buckets.len() + 4);
    let dist = format!("{metric}{SUFFIX}");
    for (b, pts) in buckets.into_values() {
        let tag = tag_value(SCALE, b);
        let with: Vec<(&str, &str)> = tags.iter().copied().chain([(BUCKET_TAG, tag.as_str())]).collect();
        out.push((SeriesKey::new(&dist, with, Kind::Count)?, pts));
    }
    for (suffix, kind, pts) in [(".count", Kind::Count, count), (".sum", Kind::Count, sum), (".min", Kind::Gauge, min), (".max", Kind::Gauge, max)] {
        if !pts.is_empty() {
            out.push((SeriesKey::new(&format!("{metric}{suffix}"), tags.iter().copied(), kind)?, pts));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    #[test]
    fn a_value_s_bucket_holds_it_at_every_scale() {
        for scale in [-2, 0, 3, 5, 8] {
            for v in [1e-9, 0.001, 0.5, 1.0, 2.0, 3.0, 1000.0, 123_456.789, 1e12] {
                for v in [v, -v] {
                    let b = bucket_of(v, scale).unwrap();
                    let (lo, hi) = b.bounds(scale);
                    if v > 0.0 {
                        assert!(lo < v && v <= hi * (1.0 + 1e-12), "{v} at {scale}: {b:?} ({lo}, {hi}]");
                    } else {
                        assert!(lo * (1.0 + 1e-12) <= v && v < hi, "{v} at {scale}: {b:?} [{lo}, {hi})");
                    }
                }
            }
        }
        // Powers of the base fall at the top of a bucket: 2 is bucket 31's at scale 5 ((2^(31/32), 2]).
        assert_eq!(bucket_of(2.0, 5), Some(Bucket::Pos(31)));
        assert_eq!(bucket_of(1.0, 0), Some(Bucket::Pos(-1)));
        assert_eq!(bucket_of(0.0, 5), Some(Bucket::Zero));
        assert_eq!(bucket_of(f64::NAN, 5), None);
        for b in [Bucket::Pos(-3), Bucket::Neg(12), Bucket::Zero] {
            assert_eq!(Bucket::parse(&b.tag()), Some(b));
        }
    }

    /// Coarsened by `k`, a value's bucket is the one it falls in at the coarser scale.
    #[test]
    fn coarsening_a_bucket_is_bucketing_coarser() {
        let mut rng = Rng::new(5);
        for _ in 0..5000 {
            let v = (rng.f64() * 40.0 - 20.0).exp2() * if rng.below(2) == 0 { 1.0 } else { -1.0 };
            let b = bucket_of(v, 8).unwrap();
            for k in 0..8u32 {
                assert_eq!(b.coarser(k), bucket_of(v, 8 - k as i32).unwrap(), "{v} by {k}");
            }
        }
    }

    /// Percentiles of merged buckets: within (√base − 1) of the exact ones, over shapes a latency
    /// takes (log-normal), heavy tails (Pareto) and values both sides of zero, merged from many
    /// parts at different scales.
    #[test]
    fn a_merged_percentile_is_within_its_relative_error() {
        let mut rng = Rng::new(9);
        let bound = ((2f64).powi(-SCALE) * 0.5).exp2() - 1.0;
        let normal = |rng: &mut Rng| -> f64 { (-2.0 * rng.f64().max(1e-300).ln()).sqrt() * (std::f64::consts::TAU * rng.f64()).cos() };
        for shape in 0..3 {
            let mut all = Vec::new();
            let mut m = Merged::new(SCALE);
            for part in 0..20 {
                let scale = [SCALE, SCALE + 2, 8][part % 3];
                for _ in 0..2000 {
                    let v = match shape {
                        0 => (normal(&mut rng) * 0.8 - 3.0).exp(),
                        1 => 0.01 / rng.f64().max(1e-12).powf(1.0 / 1.2),
                        _ => normal(&mut rng) * 50.0,
                    };
                    all.push(v);
                    m.add(bucket_of(v, scale).unwrap(), scale, 1.0);
                }
            }
            all.sort_by(f64::total_cmp);
            let (lo, hi) = (all[0], all[all.len() - 1]);
            for q in [0.0, 0.01, 0.25, 0.5, 0.9, 0.95, 0.99, 0.999, 1.0] {
                let exact = all[((all.len() - 1) as f64 * q).floor() as usize];
                let got = m.quantile(q, Some((lo, hi))).unwrap();
                let err = if exact == 0.0 { got.abs() } else { (got - exact).abs() / exact.abs() };
                assert!(err <= bound * 1.000_001, "shape {shape} q {q}: {got} for {exact} ({err} > {bound})");
            }
        }
    }

    /// Coarser parts make the whole coarser; a merge is the same whichever order the parts come in.
    #[test]
    fn merging_takes_the_coarsest_scale_and_any_order() {
        // Bucket 20 at scale 5 is bucket 5 at scale 3 (20 >> 2).
        let mut a = Merged::new(SCALE);
        a.add(Bucket::Pos(20), 5, 3.0);
        a.add(Bucket::Pos(10), 3, 2.0);
        assert_eq!(a.scale, 3);
        assert_eq!(a.counts.values().map(|(b, n)| (*b, *n)).collect::<Vec<_>>(), vec![(Bucket::Pos(5), 3.0), (Bucket::Pos(10), 2.0)]);
        let mut b = Merged::new(SCALE);
        b.add(Bucket::Pos(10), 3, 2.0);
        b.add(Bucket::Pos(20), 5, 3.0);
        assert_eq!(a, b);
        assert_eq!(a.quantile(0.5, None), Some(Bucket::Pos(5).value(3)));
        assert_eq!(Merged::new(5).quantile(0.5, None), None);
    }
}
