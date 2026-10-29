//! Forecasts (`docs/observe.md` §9.1, §12.15): Datadog's `forecast()`, what a series will do next
//! and how sure of it to be: an expected value each step ahead, and the spread of the errors made
//! that far ahead, so that `deviations` of it either side are its bounds.
//!
//! Datadog says what its algorithms are for, not how they work ("linear" for steady trends,
//! "seasonal" for repeating patterns; docs.datadoghq.com, "Forecasts"). kiln's were chosen by the
//! M4 competition's data, scored as M4 scores them (Makridakis, Spiliotis and Assimakopoulos, IJF
//! 36(1), 2020; the scoring checked by M4's own benchmarks, which it scores to the published
//! figure), and each choice held on the half of the series it was not chosen on (`benches/m4.rs`):
//!
//! - **seasonal**: additive Holt-Winters with a damped trend (Gardner and McKenzie 1985), its
//!   smoothing chosen to err least over the whole horizon from every season's start, learned from
//!   the last [`SEASONS`] seasons; beside it the seasonal naive (the last season again).
//! - **linear, default**: Holt's linear method, damped or not as it errs least one step ahead, from
//!   the last [`LINEAR_HORIZONS`] horizons of points; beside it the last value.
//! - **linear, reactive**: Holt's linear method undamped, from the last [`REACTIVE_HORIZONS`]
//!   horizons, alone: the recent trend carried on, noise and all.
//! - **linear, simple**: Theil and Sen's line through the whole history (the median of every pair's
//!   slope: robust to 29% of its points being wild; Sen 1968).
//!
//! Each of the first two forecasts one of three ways, chosen per series on its own history: the
//! smoothing, the naive, or the two averaged (M4's lesson that a combination beats its parts). The
//! average unless another erred less by [`CHOICE_MARGIN`]: a steady trend is carried on, a random
//! walk kept near its last value. On M4: hourly series OWA 0.556 (the seasonal naive's 0.628, M4's
//! Naive2's 1), daily 0.976 (M4's best entry's 0.977).
//!
//! A forecast's spread at each step ahead is its own errors at that step, made from every earlier
//! origin in its history as it will be made from the last (one pass, the state at each point
//! projected): their root mean square, never less than the step before's. At 1.96 of it either
//! side, M4's hourly outcomes fall inside 94.3% of the time (MSIS 7.30; M4's ETS 17.49, ARIMA 7.54),
//! its daily ones 93.3% (MSIS 28.9; ETS 29.7, Naive 32.6). Theil-Sen's spread is its residuals'
//! (1.4826 times their median absolute deviation) widened as a line's prediction is with distance.

/// Seasons a seasonal forecast learns from: M4's hourly series forecast best (OWA, MSIS, coverage)
/// from their last 10 days of the 6, 8, 10, 12, 15, 20 and all tried, on either half of the series.
pub const SEASONS: usize = 10;
/// Seasons a seasonal forecast needs at the least (Datadog's: "at least two seasons of history").
pub const MIN_SEASONS: usize = 2;
/// Horizons of points a default linear forecast learns from: on M4's daily series, 30 of the 10, 15,
/// 20, 30 and 40 tried, on either half.
pub const LINEAR_HORIZONS: usize = 30;
/// Horizons of points a reactive linear forecast learns from: its recent past alone.
pub const REACTIVE_HORIZONS: usize = 4;
/// Points Theil-Sen reads at the most (evenly spaced, the last always among them): its pairs grow as
/// the square of its points.
pub const SIMPLE_POINTS: usize = 512;
/// How much less another way of forecasting (the smoothing alone, or the naive alone) must have
/// erred on a series' own history than their average for it to be chosen instead. On M4, of 0,
/// 0.1, 0.2, 0.3, 0.5 and 1 (never), 0.2 forecast best over both its hourly series (OWA 0.556) and
/// its daily ones (0.976); on halves of each, best or tied on three, 0.006 behind 0.1's on the
/// fourth (`benches/m4.rs`). A steady trend errs far less carried on than averaged with its last
/// value (which lags it by half), so it is carried on.
pub const CHOICE_MARGIN: f64 = 0.2;
/// The most steps ahead a forecast makes.
pub const MAX_AHEAD: usize = 4096;

/// How a linear forecast follows its series (Datadog's `model`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Default,
    Simple,
    Reactive,
}

impl Model {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "default" => Self::Default,
            "simple" => Self::Simple,
            "reactive" => Self::Reactive,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Simple => "simple",
            Self::Reactive => "reactive",
        }
    }
}

/// Datadog's two algorithms: a linear one, or a seasonal one whose season is `period` points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Linear(Model),
    Seasonal { period: usize },
}

/// A forecast: each step ahead's expected value and its errors' spread there (one deviation).
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub expected: Vec<f64>,
    pub deviation: Vec<f64>,
}

impl Forecast {
    /// Step `k`'s (0: the next) bounds at `deviations` either side.
    pub fn bounds(&self, k: usize, deviations: f64) -> Option<(f64, f64)> {
        let (e, d) = (self.expected.get(k)?, self.deviation.get(k)?);
        Some((e - deviations * d, e + deviations * d))
    }
}

/// The next `h` points of a series on a regular grid (`history` oldest first; gaps NaN, closed by a
/// line between the points either side; the forecast follows its last finite point). None for a
/// history too short for the algorithm.
pub fn forecast(history: &[f64], algo: Algorithm, h: usize) -> Option<Forecast> {
    forecast_with(history, algo, h, CHOICE_MARGIN)
}

/// [`forecast`] with another [`CHOICE_MARGIN`]: the study's (`benches/m4.rs`).
pub fn forecast_with(history: &[f64], algo: Algorithm, h: usize, margin: f64) -> Option<Forecast> {
    let h = h.min(MAX_AHEAD);
    if h == 0 {
        return None;
    }
    let x = closed(history)?;
    match algo {
        Algorithm::Seasonal { period: m } => {
            if m < 2 || x.len() < MIN_SEASONS * m {
                return None;
            }
            let w = tail(&x, SEASONS * m);
            Some(smoothed(w, m, h, Fit::Horizon, Blend::SeasonalNaive, &GRID_PHI, margin))
        }
        Algorithm::Linear(Model::Default) => {
            let w = tail(&x, LINEAR_HORIZONS.saturating_mul(h).max(8));
            (w.len() >= 3).then(|| smoothed(w, 1, h, Fit::OneStep, Blend::Naive, &GRID_PHI, margin))
        }
        Algorithm::Linear(Model::Reactive) => {
            let w = tail(&x, REACTIVE_HORIZONS.saturating_mul(h).max(8));
            (w.len() >= 3).then(|| smoothed(w, 1, h, Fit::OneStep, Blend::None, &[1.0], margin))
        }
        Algorithm::Linear(Model::Simple) => (x.len() >= 3).then(|| theil_sen(&x, h)),
    }
}

/// The history from its first finite point to its last, gaps closed by a straight line.
fn closed(history: &[f64]) -> Option<Vec<f64>> {
    let first = history.iter().position(|v| v.is_finite())?;
    let last = history.iter().rposition(|v| v.is_finite())?;
    let x = history.get(first..=last)?;
    let mut out: Vec<f64> = x.to_vec();
    let mut prev: Option<usize> = None;
    for i in 0..out.len() {
        if !out.get(i).is_some_and(|v| v.is_finite()) {
            continue;
        }
        if let Some(p) = prev
            && i > p + 1
        {
            let (a, b) = (out.get(p).copied().unwrap_or(0.0), out.get(i).copied().unwrap_or(0.0));
            let span = (i - p) as f64;
            for j in p + 1..i {
                if let Some(v) = out.get_mut(j) {
                    *v = a + (b - a) * (j - p) as f64 / span;
                }
            }
        }
        prev = Some(i);
    }
    Some(out)
}

fn tail(x: &[f64], n: usize) -> &[f64] {
    x.get(x.len().saturating_sub(n)..).unwrap_or(x)
}

/// What a smoothing is chosen to err least at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    OneStep,
    /// Every step to the horizon, from the start of every season.
    Horizon,
}

/// What a smoothing's forecast is averaged with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blend {
    None,
    /// The last value.
    Naive,
    /// The last season again.
    SeasonalNaive,
}

/// The dampings tried: the usual ranges (Hyndman et al., "Forecasting with Exponential
/// Smoothing", 2008), 1 undamped.
const GRID_PHI: [f64; 4] = [0.8, 0.9, 0.98, 1.0];
const GRID_ALPHA: [f64; 8] = [0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9];
const GRID_BETA: [f64; 5] = [0.0, 0.01, 0.05, 0.1, 0.3];
const GRID_GAMMA: [f64; 5] = [0.01, 0.05, 0.1, 0.2, 0.4];

/// Additive Holt-Winters with a damped trend, in error-correction form (ETS(A,Ad,A)); no season
/// when its period is 1. Its buffers are kept from one smoothing tried to the next.
struct Hw {
    alpha: f64,
    beta: f64,
    gamma: f64,
    phi: f64,
    level: f64,
    trend: f64,
    season: Vec<f64>,
    /// The next point's place.
    t: usize,
    /// phi + phi^2 + ... + phi^k, by k (k from 0).
    damp: Vec<f64>,
}

impl Hw {
    fn new(m: usize, h: usize) -> Self {
        Self { alpha: 0.0, beta: 0.0, gamma: 0.0, phi: 1.0, level: 0.0, trend: 0.0, season: Vec::with_capacity(m), t: 0, damp: vec![0.0; h + 1] }
    }

    /// Starts again with these smoothings, from `x`'s first season (its mean the level, each phase's
    /// offset from it the season), or its first point without one.
    fn reset(&mut self, x: &[f64], m: usize, (alpha, beta, gamma, phi): (f64, f64, f64, f64)) {
        (self.alpha, self.beta, self.gamma) = (alpha, beta, gamma);
        if phi != self.phi || self.damp.get(1).copied() != Some(phi) {
            self.phi = phi;
            let mut p = 1.0;
            let mut sum = 0.0;
            for d in self.damp.iter_mut().skip(1) {
                p *= phi;
                sum += p;
                *d = sum;
            }
        }
        let first = x.get(..m.max(1).min(x.len())).unwrap_or(&[]);
        self.level = first.iter().sum::<f64>() / first.len().max(1) as f64;
        self.trend = 0.0;
        self.season.clear();
        if m > 1 {
            self.season.extend(first.iter().map(|v| v - self.level));
        }
        self.t = first.len();
    }

    /// `k` steps ahead of the last point taken (1: the next).
    fn ahead(&self, k: usize) -> f64 {
        let s = if self.season.is_empty() { 0.0 } else { self.season.get((self.t + k - 1) % self.season.len()).copied().unwrap_or(0.0) };
        self.level + self.damp.get(k).copied().unwrap_or(0.0) * self.trend + s
    }

    fn update(&mut self, x: f64) {
        let e = x - self.ahead(1);
        self.level += self.phi * self.trend + self.alpha * e;
        self.trend = self.phi * self.trend + self.alpha * self.beta * e;
        if !self.season.is_empty() {
            let i = self.t % self.season.len();
            if let Some(s) = self.season.get_mut(i) {
                *s += self.gamma * (1.0 - self.alpha) * e;
            }
        }
        self.t += 1;
    }
}

/// What `blend` says `k` steps after origin `t` (the point there is `x[t + k - 1]`): the last value
/// before `t`, or the last season's value at that phase.
fn blended(x: &[f64], blend: Blend, m: usize, t: usize, k: usize) -> Option<f64> {
    match blend {
        Blend::None => None,
        Blend::Naive => x.get(t.checked_sub(1)?).copied(),
        Blend::SeasonalNaive => x.get((t + (k - 1) % m).checked_sub(m)?).copied(),
    }
}

/// A smoothing chosen from the grid, then its forecast of `h` and its spread at each step from its
/// own errors at every origin of one pass.
fn smoothed(x: &[f64], m: usize, h: usize, fit: Fit, blend: Blend, phis: &[f64], margin: f64) -> Forecast {
    let mut hw = Hw::new(m, h);
    let gammas: &[f64] = if m > 1 { &GRID_GAMMA } else { &[0.0] };
    // The fit reads the horizon only from each season's start (each point's, without one).
    let (fit_h, stride) = match fit {
        Fit::OneStep => (1, 1),
        Fit::Horizon => (h, m.max(1)),
    };
    let mut best = (f64::INFINITY, (0.3, 0.0, 0.1, 0.98));
    for &phi in phis {
        for &alpha in &GRID_ALPHA {
            for &beta in &GRID_BETA {
                for &gamma in gammas {
                    let p = (alpha, beta, gamma, phi);
                    hw.reset(x, m, p);
                    let start = hw.t;
                    let mut err = 0.0;
                    for (t, &v) in x.iter().enumerate().skip(start) {
                        if (t - start).is_multiple_of(stride) {
                            for k in 1..=fit_h.min(x.len() - t) {
                                err += (x.get(t + k - 1).copied().unwrap_or(0.0) - hw.ahead(k)).abs();
                            }
                            if err >= best.0 {
                                break;
                            }
                        }
                        hw.update(v);
                    }
                    if err < best.0 {
                        best = (err, p);
                    }
                }
            }
        }
    }
    // One pass with the smoothing chosen: from each origin past two seasons (two points without
    // one), every step ahead projected and its error kept, as the forecast from the end will be;
    // for the smoothing alone, the (seasonal) naive alone and the two averaged. The one that erred
    // least on the series' own history forecasts it (a steady trend carries on; a random walk is
    // averaged with its last value), its spread its own errors'.
    hw.reset(x, m, best.1);
    let warm = 2 * m.max(1);
    let ways = if blend == Blend::None { 1 } else { 3 };
    let (mut sq, mut n) = (vec![0.0; h * ways], vec![0u32; h * ways]);
    let mut total = [0.0_f64; 3];
    let way = |w: usize, hw_k: f64, other: Option<f64>| match (w, other) {
        (1, Some(o)) => o,
        (2, Some(o)) => (hw_k + o) / 2.0,
        _ => hw_k,
    };
    for t in hw.t..x.len() {
        if t >= warm {
            for k in 1..=h.min(x.len() - t) {
                let (hw_k, other) = (hw.ahead(k), blended(x, blend, m, t, k));
                let y = x.get(t + k - 1).copied().unwrap_or(hw_k);
                for (w, tot) in total.iter_mut().enumerate().take(ways) {
                    let e = y - way(w, hw_k, other);
                    *tot += e * e;
                    let i = w * h + k - 1;
                    if let (Some(s), Some(c)) = (sq.get_mut(i), n.get_mut(i)) {
                        *s += e * e;
                        *c += 1;
                    }
                }
            }
        }
        if let Some(&v) = x.get(t) {
            hw.update(v);
        }
    }
    // The average (M4's best) unless another erred clearly less: by `margin` of it.
    let chosen = if ways == 1 {
        0
    } else {
        let [smoothing, naive, average] = total;
        let bar = (1.0 - margin) * average;
        match (smoothing < bar, naive < bar) {
            (true, true) if naive < smoothing => 1,
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        }
    };
    let end = x.len();
    let expected: Vec<f64> = (1..=h).map(|k| way(chosen, hw.ahead(k), blended(x, blend, m, end, k))).collect();
    let at = chosen * h;
    Forecast { expected, deviation: spread(sq.get(at..at + h).unwrap_or(&[]), n.get(at..at + h).unwrap_or(&[])) }
}

/// Each step's root mean square error, a step with none taking the step before's, and none less
/// than the step before's (a later step is no surer than an earlier one).
fn spread(sq: &[f64], n: &[u32]) -> Vec<f64> {
    let mut out = Vec::with_capacity(sq.len());
    let (mut last, mut most) = (0.0_f64, 0.0_f64);
    for (s, &c) in sq.iter().zip(n) {
        let v = if c > 0 { (s / f64::from(c)).sqrt() } else { last };
        last = v;
        most = most.max(v);
        out.push(most);
    }
    out
}

/// Theil and Sen's line through the history (at most [`SIMPLE_POINTS`] of its points, evenly
/// spaced), carried on; its spread its residuals' robust scale, widened with distance as a line's
/// prediction is (`s √(1 + 1/n + (x - x̄)² / Sxx)`).
fn theil_sen(x: &[f64], h: usize) -> Forecast {
    let n = x.len();
    let stride = n.div_ceil(SIMPLE_POINTS).max(1);
    // From the last point back, so the last is always read.
    let mut pts: Vec<(f64, f64)> = (0..n).rev().step_by(stride).filter_map(|i| Some((i as f64, *x.get(i)?))).collect();
    pts.reverse();
    let mut slopes = Vec::with_capacity(pts.len() * pts.len().saturating_sub(1) / 2);
    for (i, &(xi, yi)) in pts.iter().enumerate() {
        for &(xj, yj) in pts.iter().skip(i + 1) {
            slopes.push((yj - yi) / (xj - xi));
        }
    }
    let b = median_of(&mut slopes).unwrap_or(0.0);
    let mut resid: Vec<f64> = pts.iter().map(|&(xi, yi)| yi - b * xi).collect();
    let a = median_of(&mut resid).unwrap_or(0.0);
    let mut dev: Vec<f64> = pts.iter().map(|&(xi, yi)| (yi - a - b * xi).abs()).collect();
    let s = crate::anomaly::MAD_NORMAL * median_of(&mut dev).unwrap_or(0.0);
    let m = pts.len() as f64;
    let mean = pts.iter().map(|p| p.0).sum::<f64>() / m;
    let sxx = pts.iter().map(|p| (p.0 - mean) * (p.0 - mean)).sum::<f64>().max(f64::MIN_POSITIVE);
    let mut expected = Vec::with_capacity(h);
    let mut deviation = Vec::with_capacity(h);
    for k in 0..h {
        let at = (n + k) as f64;
        expected.push(a + b * at);
        deviation.push(s * (1.0 + 1.0 / m + (at - mean) * (at - mean) / sxx).sqrt());
    }
    Forecast { expected, deviation }
}

fn median_of(v: &mut [f64]) -> Option<f64> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((*seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    }

    /// A daily cycle every hour, its noise known: the seasonal forecast follows the cycle two days
    /// ahead, and its bounds at 1.96 hold the outcomes about as often as they say (95%).
    #[test]
    fn a_season_is_followed_ahead_and_its_bounds_hold_what_they_say() {
        let (m, days, h) = (24usize, 12, 48);
        let mut seed = 7;
        let total = m * days + h;
        let y: Vec<f64> = (0..total).map(|t| 100.0 + 0.05 * t as f64 + 20.0 * (t as f64 / m as f64 * std::f64::consts::TAU).sin() + 4.0 * noise(&mut seed)).collect();
        let (hist, ahead) = y.split_at(m * days);
        let f = forecast(hist, Algorithm::Seasonal { period: m }, h).unwrap();
        let mae = f.expected.iter().zip(ahead).map(|(e, a)| (e - a).abs()).sum::<f64>() / h as f64;
        // The noise alone errs by a uniform's mean absolute deviation, 1 (of 4 wide).
        assert!(mae < 2.5, "{mae}");
        let inside = (0..h).filter(|&k| f.bounds(k, 1.96).is_some_and(|(lo, hi)| (lo..=hi).contains(&ahead[k]))).count();
        assert!(inside as f64 / h as f64 >= 0.85, "{inside} of {h}");
        assert!(f.deviation.windows(2).all(|w| w[0] <= w[1]));
    }

    /// A line with noise: every model carries the trend on, and the line's own value ten steps
    /// ahead is within the model's bounds there (each says how sure it is: the reactive one chases
    /// the noise, and says so).
    #[test]
    fn a_trend_is_carried_on_within_its_own_bounds() {
        let mut seed = 3;
        let y: Vec<f64> = (0..400).map(|t| 10.0 + 0.5 * t as f64 + noise(&mut seed)).collect();
        for model in [Model::Default, Model::Reactive, Model::Simple] {
            let f = forecast(&y, Algorithm::Linear(model), 10).unwrap();
            let truth = 10.0 + 0.5 * 409.0;
            let (lo, hi) = f.bounds(9, 1.96).unwrap();
            assert!((lo..=hi).contains(&truth), "{model:?}: {truth} outside [{lo}, {hi}] (expected {})", f.expected[9]);
            assert!(f.expected[9] > f.expected[0] + 2.0, "{model:?} rises: {:?}", f.expected);
        }
    }

    #[test]
    fn gaps_are_closed_and_a_short_history_has_no_forecast() {
        let mut y: Vec<f64> = (0..100).map(|t| t as f64).collect();
        for i in [10, 11, 12, 50] {
            y[i] = f64::NAN;
        }
        y.push(f64::NAN);
        let f = forecast(&y, Algorithm::Linear(Model::Simple), 3).unwrap();
        assert!((f.expected[0] - 100.0).abs() < 1e-9, "{:?}", f.expected);
        assert!(forecast(&[1.0, 2.0], Algorithm::Linear(Model::Default), 3).is_none());
        assert!(forecast(&vec![1.0; 30], Algorithm::Seasonal { period: 24 }, 3).is_none());
        assert!(forecast(&[f64::NAN; 10], Algorithm::Linear(Model::Default), 3).is_none());
        assert!(forecast(&y, Algorithm::Linear(Model::Default), 0).is_none());
    }

    #[test]
    fn the_same_history_forecasts_the_same_to_the_bit() {
        let mut seed = 11;
        let y: Vec<f64> = (0..300).map(|t| (t as f64 / 24.0 * std::f64::consts::TAU).cos() + noise(&mut seed)).collect();
        let a = forecast(&y, Algorithm::Seasonal { period: 24 }, 30).unwrap();
        let b = forecast(&y, Algorithm::Seasonal { period: 24 }, 30).unwrap();
        assert!(a.expected.iter().zip(&b.expected).all(|(x, y)| x.to_bits() == y.to_bits()));
        assert!(a.deviation.iter().zip(&b.deviation).all(|(x, y)| x.to_bits() == y.to_bits()));
    }

    #[test]
    fn medians_are_medians() {
        assert_eq!(median_of(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median_of(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median_of(&mut []), None);
    }
}
