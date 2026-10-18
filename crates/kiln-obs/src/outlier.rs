//! Outliers (`docs/observe.md` §9.1): Datadog's `outliers()`, the members of a group that behave
//! unlike their peers over a window ("Trigger a separate alert for each outlier", for a group of
//! "three or more members, which exhibit uniform behavior"; docs.datadoghq.com, "Outlier Monitor").
//!
//! Datadog's algorithms, as its engineers described them ("Introducing Outlier Detection in
//! Datadog", 2015):
//!
//! - **DBSCAN**: each member is a point with a coordinate a bucket; "we create a new median
//!   timeseries by taking the median of the values from the existing timeseries" at each bucket,
//!   take "the (Euclidean) distance between each host and the median series", and "the threshold
//!   is the median of those distances, multiplied by a normalizing constant", then by `tolerance`:
//!   DBSCAN's ε. "Any point can agglomerate" (a cluster is what is within ε of a member, again and
//!   again), and "any point that is not in the largest cluster will be considered an outlier".
//! - **MAD**: the median and the median absolute deviation of every point of every member in the
//!   window, "multiplied by a normalizing constant" (1.4826, the MAD's consistency constant: a
//!   normal law's deviation) "and a tolerance"; a point further than that from the median is an
//!   outlier, and "if more than pct% of a particular series' points are considered outliers, then
//!   the whole series is marked as an outlier".
//! - **scaledDBSCAN, scaledMAD**: as the two, each bucket first divided by the group's median
//!   magnitude there, so that a level every member shares (a daily rise, a deploy's step) moves no
//!   one: outliers "scale with the overall metric magnitude" (Datadog's "Outlier Monitor").
//!
//! Datadog leaves DBSCAN's normalizing constant unsaid; kiln's is √2: with noise independent across
//! d buckets, a member is some σ√d from the median series and two members σ√(2d) from each other, so
//! at a tolerance of 1, ε is the distance between two ordinary members (they join), and a member off
//! by two deviations everywhere stands apart (the tests).

/// DBSCAN's normalizing constant: the distance between two members over that from each to the
/// median series, √2 for noise independent across buckets.
pub const DBSCAN_NORMALIZER: f64 = std::f64::consts::SQRT_2;
/// The MAD's consistency constant: a normal law's standard deviation over its MAD.
pub const MAD_NORMALIZER: f64 = crate::anomaly::MAD_NORMAL;
/// Members a group needs for any to be an outlier (Datadog's "three or more").
pub const MIN_MEMBERS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Dbscan,
    Mad,
    ScaledDbscan,
    ScaledMad,
}

impl Algorithm {
    /// Datadog's names, as written in any case (`'DBSCAN'`, `'mad'`, `'scaledMAD'`).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "dbscan" => Self::Dbscan,
            "mad" => Self::Mad,
            "scaleddbscan" => Self::ScaledDbscan,
            "scaledmad" => Self::ScaledMad,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dbscan => "dbscan",
            Self::Mad => "mad",
            Self::ScaledDbscan => "scaleddbscan",
            Self::ScaledMad => "scaledmad",
        }
    }

    fn scaled(self) -> bool {
        matches!(self, Self::ScaledDbscan | Self::ScaledMad)
    }
}

/// Which of a group's members are outliers over the window: `members` each a member's values on one
/// grid (NaN where it has none), `tolerance` how far is too far, `pct` (the MADs only) how many of
/// its points must be for a member to be. A member with no point at all is none; a group of fewer
/// than [`MIN_MEMBERS`] with points has none.
pub fn outliers(members: &[&[f64]], algo: Algorithm, tolerance: f64, pct: f64) -> Vec<bool> {
    let mut out = vec![false; members.len()];
    let present: Vec<usize> = (0..members.len()).filter(|&i| members.get(i).is_some_and(|m| m.iter().any(|v| v.is_finite()))).collect();
    if present.len() < MIN_MEMBERS {
        return out;
    }
    let d = members.iter().map(|m| m.len()).max().unwrap_or(0);
    // The values as compared: as they are, or each bucket over the group's median magnitude there.
    let rows: Vec<Vec<f64>> = present.iter().map(|&i| members.get(i).map_or_else(Vec::new, |m| m.to_vec())).collect();
    let rows = if algo.scaled() { scaled(&rows, d) } else { rows };
    let flags = match algo {
        Algorithm::Dbscan | Algorithm::ScaledDbscan => dbscan(&rows, d, tolerance),
        Algorithm::Mad | Algorithm::ScaledMad => mad(&rows, tolerance, pct),
    };
    for (&i, f) in present.iter().zip(flags) {
        if let Some(o) = out.get_mut(i) {
            *o = f;
        }
    }
    out
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    let (_, hi, _) = v.select_nth_unstable_by(mid, f64::total_cmp);
    let hi = *hi;
    if v.len() % 2 == 1 {
        return Some(hi);
    }
    let lo = v.get(..mid)?.iter().copied().max_by(f64::total_cmp)?;
    Some((lo + hi) / 2.0)
}

/// Each bucket over the group's median magnitude there (left as it is where that is 0).
fn scaled(rows: &[Vec<f64>], d: usize) -> Vec<Vec<f64>> {
    let mut col = Vec::with_capacity(rows.len());
    let scale: Vec<f64> = (0..d)
        .map(|t| {
            col.clear();
            col.extend(rows.iter().filter_map(|r| r.get(t)).copied().filter(|v| v.is_finite()).map(f64::abs));
            median(&mut col).filter(|m| *m > 0.0).unwrap_or(1.0)
        })
        .collect();
    rows.iter().map(|r| r.iter().zip(&scale).map(|(v, s)| v / s).collect()).collect()
}

/// The distance between two rows over the buckets both have, as if over all `d` of them.
fn distance(a: &[f64], b: &[f64], d: usize) -> Option<f64> {
    let (mut sum, mut n) = (0.0, 0usize);
    for (x, y) in a.iter().zip(b) {
        if x.is_finite() && y.is_finite() {
            sum += (x - y) * (x - y);
            n += 1;
        }
    }
    (n > 0).then(|| (sum * d as f64 / n as f64).sqrt())
}

fn dbscan(rows: &[Vec<f64>], d: usize, tolerance: f64) -> Vec<bool> {
    let n = rows.len();
    // The median series: each bucket's median over the members that have it.
    let mut col = Vec::with_capacity(n);
    let med: Vec<f64> = (0..d)
        .map(|t| {
            col.clear();
            col.extend(rows.iter().filter_map(|r| r.get(t)).copied().filter(|v| v.is_finite()));
            median(&mut col).unwrap_or(f64::NAN)
        })
        .collect();
    let mut to_median: Vec<f64> = rows.iter().filter_map(|r| distance(r, &med, d)).collect();
    let Some(typical) = median(&mut to_median) else { return vec![false; n] };
    let eps = typical * DBSCAN_NORMALIZER * tolerance;
    // Clusters: what is within ε of a member, again and again (every point a core point).
    let mut cluster = vec![usize::MAX; n];
    let mut sizes: Vec<usize> = Vec::new();
    let mut stack = Vec::new();
    for start in 0..n {
        if cluster.get(start).is_some_and(|&c| c != usize::MAX) {
            continue;
        }
        let id = sizes.len();
        sizes.push(0);
        stack.push(start);
        if let Some(c) = cluster.get_mut(start) {
            *c = id;
        }
        while let Some(i) = stack.pop() {
            if let Some(s) = sizes.get_mut(id) {
                *s += 1;
            }
            let Some(ri) = rows.get(i) else { continue };
            for (j, rj) in rows.iter().enumerate() {
                if cluster.get(j).is_some_and(|&c| c == usize::MAX) && distance(ri, rj, d).is_some_and(|x| x <= eps) {
                    if let Some(c) = cluster.get_mut(j) {
                        *c = id;
                    }
                    stack.push(j);
                }
            }
        }
    }
    // The largest cluster; of clusters as large, the one holding the member nearest the median.
    let nearest = rows.iter().enumerate().filter_map(|(i, r)| Some((i, distance(r, &med, d)?))).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(i, _)| i);
    let most = sizes.iter().copied().max().unwrap_or(0);
    let main = nearest.and_then(|i| cluster.get(i).copied()).filter(|&c| sizes.get(c).copied() == Some(most)).or_else(|| sizes.iter().position(|&s| s == most));
    cluster.iter().map(|&c| Some(c) != main).collect()
}

fn mad(rows: &[Vec<f64>], tolerance: f64, pct: f64) -> Vec<bool> {
    let mut all: Vec<f64> = rows.iter().flatten().copied().filter(|v| v.is_finite()).collect();
    let Some(m) = median(&mut all) else { return vec![false; rows.len()] };
    for v in &mut all {
        *v = (*v - m).abs();
    }
    let mad = median(&mut all).unwrap_or(0.0);
    // A MAD of 0 (most points alike): the mean absolute deviation's normal scale instead.
    let scale = if mad > 0.0 { MAD_NORMALIZER * mad } else { crate::anomaly::MEAN_AD_NORMAL * all.iter().sum::<f64>() / all.len().max(1) as f64 };
    let limit = tolerance * scale;
    rows.iter()
        .map(|r| {
            let (mut far, mut seen) = (0usize, 0usize);
            for v in r.iter().filter(|v| v.is_finite()) {
                seen += 1;
                if (v - m).abs() > limit {
                    far += 1;
                }
            }
            seen > 0 && far as f64 * 100.0 > pct * seen as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u64) -> f64 {
        // A normal deviate by Box and Muller.
        let mut u = || {
            *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((*seed >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let (a, b) = (u(), u());
        (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }

    /// `n` members of noise about a shared level (`rising`: one all of them rise along, by half over
    /// the window) over `d` buckets; member 0 off by `off` deviations everywhere.
    fn group(n: usize, d: usize, off: f64, seed: u64, rising: bool) -> Vec<Vec<f64>> {
        let mut s = seed;
        let level = |t: usize| if rising { 100.0 + 50.0 * (t as f64 / d as f64) } else { 100.0 };
        (0..n).map(|i| (0..d).map(|t| level(t) + noise(&mut s) + if i == 0 { off } else { 0.0 }).collect()).collect()
    }

    fn run(g: &[Vec<f64>], algo: Algorithm, tol: f64, pct: f64) -> Vec<bool> {
        let refs: Vec<&[f64]> = g.iter().map(Vec::as_slice).collect();
        outliers(&refs, algo, tol, pct)
    }

    /// The member unlike its peers is the outlier, by each algorithm (DBSCAN's and scaled ones over a
    /// level all members rise along: the median series and the scaling take it away; plain MAD,
    /// pooled, over a steady one); none is, in groups of noise alone, over a hundred seeds at 3.
    #[test]
    fn the_member_unlike_its_peers_is_the_outlier_and_noise_alone_has_none() {
        for (algo, rising, off, tol, pct) in [
            (Algorithm::Dbscan, true, 2.0, 1.0, 0.0),
            (Algorithm::ScaledDbscan, true, 2.0, 1.0, 0.0),
            (Algorithm::Mad, false, 3.0, 1.5, 50.0),
            (Algorithm::ScaledMad, true, 3.0, 1.5, 50.0),
        ] {
            let flagged = run(&group(20, 60, off, 1, rising), algo, tol, pct);
            assert_eq!(flagged.iter().position(|&o| o), Some(0), "{algo:?}: {flagged:?}");
            assert_eq!(flagged.iter().filter(|&&o| o).count(), 1, "{algo:?}: {flagged:?}");
            let mut false_flags = 0;
            for seed in 0..100 {
                false_flags += run(&group(20, 60, 0.0, 100 + seed, rising), algo, 3.0, 20.0).iter().filter(|&&o| o).count();
            }
            assert_eq!(false_flags, 0, "{algo:?}: {false_flags} members of noise alone flagged at 3");
        }
    }

    /// More tolerance lets more deviation pass: DBSCAN at 3 keeps a member two deviations off, and
    /// still flags one ten off.
    #[test]
    fn tolerance_says_how_far_is_too_far() {
        assert!(!run(&group(20, 60, 2.0, 7, true), Algorithm::Dbscan, 3.0, 0.0)[0]);
        assert!(run(&group(20, 60, 10.0, 7, true), Algorithm::Dbscan, 3.0, 0.0)[0]);
    }

    /// MAD's pct: a member off for a fifth of the window is an outlier under 10% and not under 30%.
    #[test]
    fn pct_says_how_much_of_a_member_must_be_off() {
        let mut g = group(10, 50, 0.0, 3, false);
        for v in g[0].iter_mut().take(10) {
            *v += 20.0;
        }
        assert!(run(&g, Algorithm::Mad, 3.0, 10.0)[0]);
        assert!(!run(&g, Algorithm::Mad, 3.0, 30.0)[0]);
    }

    /// A level every member shares hides an outlier from pooled MAD when it varies widely over the
    /// window; scaled MAD divides it away.
    #[test]
    fn scaled_mad_sees_through_a_level_all_members_share() {
        let mut s = 5;
        let g: Vec<Vec<f64>> = (0..10).map(|i| (0..48).map(|t| (1000.0 + 900.0 * (t as f64 / 48.0 * std::f64::consts::TAU).sin()) * (1.0 + 0.01 * noise(&mut s)) * if i == 0 { 1.3 } else { 1.0 }).collect()).collect();
        assert!(!run(&g, Algorithm::Mad, 3.0, 20.0)[0], "plain MAD is blinded by the cycle");
        assert!(run(&g, Algorithm::ScaledMad, 3.0, 20.0)[0], "scaled MAD sees the member 30% above");
    }

    #[test]
    fn small_groups_gaps_and_empty_members() {
        assert_eq!(run(&group(2, 10, 50.0, 1, false), Algorithm::Dbscan, 1.0, 0.0), [false, false]);
        let mut g = group(6, 30, 8.0, 2, true);
        for t in (0..30).step_by(3) {
            g[0][t] = f64::NAN;
            g[2][t + 1] = f64::NAN;
        }
        g.push(vec![f64::NAN; 30]);
        let flagged = run(&g, Algorithm::Dbscan, 1.0, 0.0);
        assert_eq!(flagged, [true, false, false, false, false, false, false]);
    }
}
