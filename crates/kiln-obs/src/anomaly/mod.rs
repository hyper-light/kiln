//! Anomalies (`docs/observe.md` §9.1): what a series should be now, and how far from it is
//! unusual, learned from the series' own history; the three algorithms Datadog's anomaly monitors
//! name, so a monitor written for Datadog reads here:
//! - `basic`: the median of the recent past; no season; follows a change of level in half its
//!   window.
//! - `robust`: the season's (learned) median of the same phase in the last seasons, moved by the
//!   level's offset over the last season: steady through a long anomaly, slow to take a new level.
//! - `agile`: additive Holt-Winters (Winters 1960), its smoothing fitted to the history by least
//!   absolute one-step error, its updates Huberised (Gelper, Fried and Croux, "Robust forecasting
//!   with exponential and Holt-Winters smoothing", J. Forecasting 2010) so an outlier does not move
//!   it: follows a change of level within a few points.
//!
//! Each gives, before it sees a point, its expected value and its scale: the robust spread of its
//! own one-step errors (1.4826 × their median absolute value, the MAD's consistency constant for a
//! normal law; 1.2533 × their mean absolute value when that median is 0, Iglewicz and Hoaglin's
//! fallback). A point's `z` is its error in scales; a band of `k` deviations is expected ± k scales.
//!
//! The season is learned, not configured: among a day, a week and the strongest peak of the
//! differenced series' autocorrelation, the period whose seasonal prediction (the value a season
//! ago, moved by the level's change since) errs least, if it errs less than the last value's by
//! more than twice the MAD's standard error (1.166 σ/√n for a normal law).

pub mod nab;

use std::collections::VecDeque;

/// The MAD's consistency constant for a normal law: 1/Φ⁻¹(3/4).
pub const MAD_NORMAL: f64 = 1.482_602_218_505_602;
/// The mean absolute deviation's: √(π/2).
pub const MEAN_AD_NORMAL: f64 = 1.253_314_137_315_500_3;
/// The MAD's standard error, in σ/√n, for a normal law.
const MAD_SE: f64 = 1.166;
/// Huber's ψ cut-off for the robust updates, in scales (Gelper et al.'s k = 2).
const HUBER: f64 = 2.0;
/// Seasons the robust algorithm's seasonal median reads at the most: two outlying ones are outvoted.
const SEASONS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Basic,
    Agile,
    Robust,
}

impl Algorithm {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().trim_matches(['\'', '"']) {
            "basic" => Some(Self::Basic),
            "agile" => Some(Self::Agile),
            "robust" => Some(Self::Robust),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Agile => "agile",
            Self::Robust => "robust",
        }
    }
}

/// The last `cap` values, also kept sorted: a median in O(1), a push in O(cap) (a binary search and
/// a move).
#[derive(Debug, Clone)]
pub struct Window {
    cap: usize,
    order: VecDeque<f64>,
    sorted: Vec<f64>,
}

impl Window {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self { cap, order: VecDeque::with_capacity(cap), sorted: Vec::with_capacity(cap) }
    }

    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    pub fn push(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        if self.order.len() == self.cap
            && let Some(old) = self.order.pop_front()
        {
            let at = self.sorted.partition_point(|v| v.total_cmp(&old).is_lt());
            if self.sorted.get(at).is_some_and(|v| v.total_cmp(&old).is_eq()) {
                self.sorted.remove(at);
            }
        }
        self.order.push_back(x);
        let at = self.sorted.partition_point(|v| v.total_cmp(&x).is_lt());
        self.sorted.insert(at, x);
    }

    pub fn median(&self) -> Option<f64> {
        let n = self.sorted.len();
        if n == 0 {
            return None;
        }
        let hi = self.sorted.get(n / 2).copied()?;
        if n % 2 == 1 { Some(hi) } else { Some((self.sorted.get(n / 2 - 1).copied()? + hi) / 2.0) }
    }

    fn mean(&self) -> Option<f64> {
        (!self.sorted.is_empty()).then(|| self.sorted.iter().sum::<f64>() / self.sorted.len() as f64)
    }
}

/// The scale of errors whose absolute values `w` holds: 1.4826 × their median, else 1.2533 × their
/// mean, else none (every error 0).
fn scale(w: &Window) -> Option<f64> {
    let m = w.median()?;
    if m > 0.0 {
        return Some(MAD_NORMAL * m);
    }
    let mean = w.mean()?;
    (mean > 0.0).then_some(MEAN_AD_NORMAL * mean)
}

/// What a detector said of a point before it saw it, and the point's error in scales.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub expected: f64,
    pub scale: f64,
    pub z: f64,
}

impl Step {
    /// The band of `k` deviations.
    pub fn band(&self, k: f64) -> (f64, f64) {
        (self.expected - k * self.scale, self.expected + k * self.scale)
    }
}

/// The median of `xs` (sorted in place).
fn median(xs: &mut [f64]) -> Option<f64> {
    xs.sort_unstable_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 {
        return None;
    }
    let hi = xs.get(n / 2).copied()?;
    if n % 2 == 1 { Some(hi) } else { Some((xs.get(n / 2 - 1).copied()? + hi) / 2.0) }
}

/// The period (in points) the history's season repeats at, if it has one: see the module's notes.
/// `spacing` is the points' usual distance (seconds); `window` the detector's.
pub fn season(values: &[f64], spacing: f64, window: usize) -> Option<usize> {
    let n = values.len();
    if n < 8 || spacing.is_nan() || spacing <= 0.0 {
        return None;
    }
    let mut candidates: Vec<usize> = [86_400.0, 604_800.0].iter().map(|s| (s / spacing).round() as usize).collect();
    candidates.extend(acf_peak(values));
    candidates.retain(|&p| p >= 2 && p * 2 <= n);
    candidates.sort_unstable();
    candidates.dedup();
    // Each candidate against none, on the points both predict (after two of its seasons): the robust
    // algorithm's own one-step errors, as it will make them.
    let errors = |period: Option<usize>| -> Vec<f64> {
        let mut d = Detector::with(Algorithm::Robust, period, window);
        values.iter().map(|&x| d.step(x).map_or(f64::NAN, |s| (x - s.expected).abs())).collect()
    };
    let none = errors(None);
    let mut best: Option<(usize, f64)> = None;
    for p in candidates {
        let with = errors(Some(p));
        let (mut a, mut b): (Vec<f64>, Vec<f64>) = with.iter().zip(&none).skip(2 * p).filter(|(x, y)| x.is_finite() && y.is_finite()).map(|(x, y)| (*x, *y)).unzip();
        let m = a.len();
        let (Some(e), Some(base)) = (median(&mut a), median(&mut b)) else { continue };
        let ratio = if base > 0.0 { e / base } else if e == 0.0 { 1.0 } else { f64::INFINITY };
        let margin = 2.0 * MAD_SE / (m as f64).sqrt();
        if ratio < 1.0 - margin && best.is_none_or(|(_, r)| ratio < r) {
            best = Some((p, ratio));
        }
    }
    best.map(|(p, _)| p)
}

/// The lag (2 to n/2) of the highest local peak of the differenced series' autocorrelation, if one
/// stands above the white-noise band (3/√n).
fn acf_peak(values: &[f64]) -> Option<usize> {
    let d: Vec<f64> = values.windows(2).filter_map(|w| Some(w.get(1)? - w.first()?)).collect();
    let n = d.len();
    if n < 8 {
        return None;
    }
    let mean = d.iter().sum::<f64>() / n as f64;
    let var: f64 = d.iter().map(|x| (x - mean) * (x - mean)).sum();
    if var <= 0.0 {
        return None;
    }
    let acf = |lag: usize| -> f64 { d.iter().zip(d.iter().skip(lag)).map(|(a, b)| (a - mean) * (b - mean)).sum::<f64>() / var };
    let max_lag = n / 2;
    let r: Vec<f64> = (0..=max_lag + 1).map(acf).collect();
    let band = 3.0 / (n as f64).sqrt();
    (2..=max_lag)
        .filter(|&l| {
            let (a, b, c) = (r.get(l - 1).copied().unwrap_or(0.0), r.get(l).copied().unwrap_or(0.0), r.get(l + 1).copied().unwrap_or(0.0));
            b > band && b >= a && b >= c
        })
        .max_by(|&a, &b| r.get(a).copied().unwrap_or(0.0).total_cmp(&r.get(b).copied().unwrap_or(0.0)))
}

/// Holt-Winters' smoothing: level, damped trend, season.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoothing {
    pub alpha: f64,
    pub beta: f64,
    pub gamma: f64,
    pub phi: f64,
}

/// Additive Holt-Winters in error-correction form, its updates Huberised.
#[derive(Debug, Clone)]
struct HoltWinters {
    s: Smoothing,
    level: f64,
    trend: f64,
    season: Vec<f64>,
    t: usize,
}

impl HoltWinters {
    /// Started from `history`'s first season (its median the level; each phase's offset from it the
    /// season), or its first value without one.
    fn start(s: Smoothing, history: &[f64], period: Option<usize>) -> Option<Self> {
        let p = period.unwrap_or(0);
        let first: Vec<f64> = history.iter().take(p.max(1)).copied().collect();
        let level = median(&mut first.clone())?;
        let season = if p > 0 { first.iter().map(|x| x - level).collect() } else { Vec::new() };
        Some(Self { s, level, trend: 0.0, season, t: 0 })
    }

    fn forecast(&self) -> f64 {
        let seasonal = if self.season.is_empty() { 0.0 } else { self.season.get(self.t % self.season.len()).copied().unwrap_or(0.0) };
        self.level + self.s.phi * self.trend + seasonal
    }

    /// Takes `x`, its error clipped at `clip` (Huber's ψ; none: as it is).
    fn update(&mut self, x: f64, clip: Option<f64>) {
        let e = x - self.forecast();
        let e = clip.map_or(e, |c| e.clamp(-c, c));
        let s = self.s;
        self.level += s.phi * self.trend + s.alpha * e;
        self.trend = s.phi * self.trend + s.alpha * s.beta * e;
        if !self.season.is_empty() {
            let i = self.t % self.season.len();
            if let Some(v) = self.season.get_mut(i) {
                *v += s.gamma * (1.0 - s.alpha) * e;
            }
        }
        self.t += 1;
    }
}

/// The smoothing that errs least (absolute one-step error) on `history`, of a grid over the usual
/// ranges (Hyndman et al., "Forecasting with Exponential Smoothing", 2008).
pub fn fit(history: &[f64], period: Option<usize>) -> Smoothing {
    let mut best = (f64::INFINITY, Smoothing { alpha: 0.3, beta: 0.0, gamma: 0.1, phi: 0.98 });
    let gammas: &[f64] = if period.is_some() { &[0.01, 0.05, 0.1, 0.2, 0.4] } else { &[0.0] };
    for &alpha in &[0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9] {
        for &beta in &[0.0, 0.01, 0.05, 0.1, 0.3] {
            for &gamma in gammas {
                for &phi in &[0.9, 0.98] {
                    let s = Smoothing { alpha, beta, gamma, phi };
                    let Some(mut hw) = HoltWinters::start(s, history, period) else { continue };
                    let skip = period.unwrap_or(1);
                    let mut err = 0.0;
                    for (i, &x) in history.iter().enumerate() {
                        if i >= skip {
                            err += (x - hw.forecast()).abs();
                        }
                        hw.update(x, None);
                    }
                    if err < best.0 {
                        best = (err, s);
                    }
                }
            }
        }
    }
    best.1
}

/// A series' detector: it learns from a history, then says of each next point what it expected.
#[derive(Debug, Clone)]
pub struct Detector {
    algo: Algorithm,
    period: Option<usize>,
    /// The robust algorithm's and basic's recent values; the robust one's seasons.
    recent: Window,
    history: VecDeque<f64>,
    offsets: Window,
    hw: Option<HoltWinters>,
    /// The absolute one-step errors, for the scale.
    errors: Window,
}

impl Detector {
    /// A detector of `algo` started on `history` (its points `spacing` seconds apart): its season
    /// learned (none for basic), agile's smoothing fitted, its error scale the history's.
    pub fn new(algo: Algorithm, history: &[f64], spacing: f64, window: usize) -> Self {
        let period = if algo == Algorithm::Basic { None } else { season(history, spacing, window) };
        let mut d = Self::with(algo, period, window);
        if algo == Algorithm::Agile {
            d.hw = HoltWinters::start(fit(history, period), history, period);
        }
        for &x in history {
            d.step(x);
        }
        d
    }

    /// A detector of `algo` with season `period`, nothing seen yet (agile: started by `new`).
    fn with(algo: Algorithm, period: Option<usize>, window: usize) -> Self {
        let keep = period.map_or(window, |p| p * SEASONS + 1).max(window);
        Self { algo, period, recent: Window::new(window), history: VecDeque::with_capacity(keep + 1), offsets: Window::new(period.unwrap_or(window)), hw: None, errors: Window::new(window) }
    }

    pub fn period(&self) -> Option<usize> {
        self.period
    }

    /// What it expects next, and its scale; none until it has seen enough.
    pub fn expect(&self) -> Option<(f64, f64)> {
        let expected = match self.algo {
            Algorithm::Basic => self.recent.median()?,
            Algorithm::Agile => self.hw.as_ref()?.forecast(),
            Algorithm::Robust => match self.period {
                Some(p) => self.seasonal(p)? + self.offsets.median().unwrap_or(0.0),
                None => self.recent.median()?,
            },
        };
        let scale = scale(&self.errors).unwrap_or(0.0);
        Some((expected, scale))
    }

    /// The median of the same phase in the last seasons held (the next point's phase).
    fn seasonal(&self, p: usize) -> Option<f64> {
        let n = self.history.len();
        let mut same: Vec<f64> = (1..=SEASONS).filter_map(|k| n.checked_sub(k * p).and_then(|i| self.history.get(i).copied())).collect();
        median(&mut same)
    }

    /// Takes `x`: what it expected of it (none while learning), and its error in scales (0 when
    /// every past error was 0 and so is this one; infinite when only this one is not).
    pub fn step(&mut self, x: f64) -> Option<Step> {
        if !x.is_finite() {
            return None;
        }
        let said = self.expect();
        let step = said.map(|(expected, scale)| {
            let e = x - expected;
            let z = if scale > 0.0 {
                e / scale
            } else if e == 0.0 {
                0.0
            } else {
                e.signum() * f64::INFINITY
            };
            Step { expected, scale, z }
        });
        if let Some(s) = step {
            self.errors.push((x - s.expected).abs());
        }
        // The robust algorithm's offset of this point from its season's median.
        if let (Some(p), Algorithm::Robust) = (self.period, self.algo)
            && let Some(seasonal) = self.seasonal(p)
        {
            self.offsets.push(x - seasonal);
        }
        if let Some(hw) = &mut self.hw {
            let clip = step.filter(|s| s.scale > 0.0).map(|s| HUBER * s.scale);
            hw.update(x, clip);
        }
        self.recent.push(x);
        self.history.push_back(x);
        let keep = self.period.map_or(self.recent.cap, |p| p * SEASONS + 1).max(self.recent.cap);
        while self.history.len() > keep {
            self.history.pop_front();
        }
        step
    }
}

/// The quantile of a series' own past |z| that counts as [`NORMAL_AT_LEVEL`] deviations: its 1-in-500
/// deviation. Measured on NAB (§12.12): of 99.5% to 99.9%, the best, on either calibration split.
pub const LEVEL: f64 = 0.998;
/// A normal law's |z| at [`LEVEL`]: Φ⁻¹(1 - 0.002/2).
pub const NORMAL_AT_LEVEL: f64 = 3.090_232_306_167_813;
/// History a scorer reads at the most: more steadies its baseline and scale (§12.12), this many
/// bound its memory (two `f64` a point, twice).
pub const HISTORY: usize = 20_000;
/// Of a scorer's history, the share its calibration's detector learns before scoring the rest.
const CALIBRATION_LEARNS: usize = 4;

/// A point as a [`Scorer`] reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub expected: f64,
    /// A deviation, calibrated: the series' own 1-in-500 error counts as 3.09 of them (a normal
    /// law's), so a band of `k` deviations means alike in a calm series and a wild one.
    pub deviation: f64,
    /// The error in deviations.
    pub score: f64,
    /// The first point of an episode (from a point past [`NORMAL_AT_LEVEL`] deviations until one
    /// back under them): an anomaly begins.
    pub onset: bool,
}

impl Point {
    /// The band of `k` deviations.
    pub fn band(&self, k: f64) -> (f64, f64) {
        (self.expected - k * self.deviation, self.expected + k * self.deviation)
    }
}

/// A series' anomaly scorer: a [`Detector`] whose errors are calibrated against the series' own
/// past ones, and whose episodes are told by their onsets. On the Numenta Anomaly Benchmark
/// (§12.12) its robust algorithm scores 52.6 (standard), 41.0 (low false positives), 58.3 (low
/// false negatives), its threshold [`NORMAL_AT_LEVEL`] deviations.
#[derive(Debug, Clone)]
pub struct Scorer {
    detector: Detector,
    /// The series' past |z|, sorted (the last [`HISTORY`]).
    past: Window,
    episode: bool,
}

impl Scorer {
    /// A scorer of `algo` started on `history` (points `spacing` seconds apart): the detector learns
    /// all of it; its calibration, the |z| of a detector that learned the first quarter and read
    /// the rest. None for too short a history (8 points).
    pub fn new(algo: Algorithm, history: &[f64], spacing: f64) -> Option<Self> {
        let h = history.get(history.len().saturating_sub(HISTORY)..)?;
        if h.len() < 8 {
            return None;
        }
        let window = h.len();
        let first = (h.len() / CALIBRATION_LEARNS).max(2);
        let mut early = Detector::new(algo, h.get(..first)?, spacing, window);
        let mut past = Window::new(HISTORY);
        for &x in h.get(first..)? {
            if let Some(s) = early.step(x)
                && s.z.is_finite()
            {
                past.push(s.z.abs());
            }
        }
        Some(Self { detector: Detector::new(algo, h, spacing, window), past, episode: false })
    }

    pub fn period(&self) -> Option<usize> {
        self.detector.period()
    }

    /// The series' |z| at [`LEVEL`], so far.
    fn reference(&self) -> f64 {
        let n = self.past.len();
        if n == 0 {
            return 0.0;
        }
        let at = ((n as f64 * LEVEL) as usize).min(n - 1);
        self.past.sorted.get(at).copied().unwrap_or(0.0)
    }

    /// What it expects next and its deviation; none while learning.
    pub fn expect(&self) -> Option<(f64, f64)> {
        let (expected, scale) = self.detector.expect()?;
        Some((expected, scale * self.reference() / NORMAL_AT_LEVEL))
    }

    /// Takes `x`: how it reads (none while learning).
    pub fn step(&mut self, x: f64) -> Option<Point> {
        let reference = self.reference();
        let s = self.detector.step(x)?;
        let z = s.z.abs();
        let ratio = if reference > 0.0 {
            z / reference
        } else if z == 0.0 {
            0.0
        } else {
            f64::INFINITY
        };
        if z.is_finite() {
            self.past.push(z);
        }
        let out = ratio * NORMAL_AT_LEVEL >= NORMAL_AT_LEVEL;
        let onset = out && !self.episode;
        self.episode = out;
        Some(Point { expected: s.expected, deviation: s.scale * reference / NORMAL_AT_LEVEL, score: ratio * NORMAL_AT_LEVEL * s.z.signum(), onset })
    }
}

/// The quantile of the past |z| over which excesses are modelled (SPOT's initial level, 98%).
const TAIL_LEVEL: f64 = 0.98;
/// A |z| less likely than this is an anomaly, not a peak: it does not refit the law (SPOT's q).
const TAIL_RISK: f64 = 1e-4;
/// Peaks a law is fitted from at the least.
const TAIL_PEAKS: usize = 5;

/// How unusual a |z| is for its series: the probability of one at least as large, from the series'
/// own past (Siffer, Fouque, Termier and Largouët, "Anomaly Detection in Streams with Extreme Value
/// Theory", KDD 2017): below a high quantile `t` of its past |z|, their empirical share; above it, a
/// generalised Pareto law fitted to the excesses over `t` (Pickands–Balkema–de Haan), by
/// probability-weighted moments (Hosking and Wallis, Technometrics 1987, sound for small samples
/// and shapes below 1; its shape kept at 0 or above, no tail lighter than exponential, since a few
/// peaks do not show a bound), refitted as excesses come, those already anomalous left out.
#[derive(Debug, Clone)]
pub struct Tail {
    /// The calibration's values below `t`, sorted.
    below: Vec<f64>,
    t: f64,
    /// Values seen, and the excesses over `t` kept.
    n: usize,
    peaks: Vec<f64>,
    shape: f64,
    scale: f64,
}

impl Tail {
    /// Calibrated on `history` (the series' |z|, finite ones); none with too few.
    pub fn new(history: &[f64]) -> Option<Self> {
        let mut xs: Vec<f64> = history.iter().copied().filter(|x| x.is_finite()).collect();
        if xs.len() < TAIL_PEAKS * 10 {
            return None;
        }
        xs.sort_unstable_by(f64::total_cmp);
        let at = ((xs.len() as f64 * TAIL_LEVEL) as usize).min(xs.len() - 1);
        let t = xs.get(at).copied()?;
        let split = xs.partition_point(|&x| x <= t);
        let peaks: Vec<f64> = xs.get(split..)?.iter().map(|x| x - t).collect();
        let below = xs.get(..split)?.to_vec();
        let mut tail = Self { below, t, n: xs.len(), peaks, shape: 0.0, scale: 0.0 };
        tail.fit();
        Some(tail)
    }

    /// The law of the excesses, by probability-weighted moments: a0 = E[Y], a1 = E[Y (1 - F(Y))];
    /// k = a0 / (a0 - 2 a1) - 2, σ = 2 a0 a1 / (a0 - 2 a1); the shape is -k.
    fn fit(&mut self) {
        let mut y = self.peaks.clone();
        let m = y.len();
        if m < TAIL_PEAKS {
            // Too few: an exponential tail at their mean (or a narrow one).
            self.shape = 0.0;
            self.scale = if m > 0 { y.iter().sum::<f64>() / m as f64 } else { 0.0 };
            return;
        }
        y.sort_unstable_by(f64::total_cmp);
        let a0 = y.iter().sum::<f64>() / m as f64;
        let b1 = y.iter().enumerate().map(|(j, v)| j as f64 / (m as f64 - 1.0) * v).sum::<f64>() / m as f64;
        let a1 = a0 - b1;
        let d = a0 - 2.0 * a1;
        if d <= 0.0 || a1 <= 0.0 {
            self.shape = 0.0;
            self.scale = a0;
            return;
        }
        let k = a0 / d - 2.0;
        // No lighter than exponential: a bounded tail fitted from a few peaks would call anything
        // past its bound impossible.
        self.shape = (-k).clamp(0.0, 0.95);
        self.scale = 2.0 * a0 * a1 / d;
    }

    /// The probability of a |z| at least `z`.
    pub fn probability(&self, z: f64) -> f64 {
        if z.is_nan() {
            return 1.0;
        }
        if z <= self.t {
            // Of the calibration, those at or above it (the peaks are all above).
            let above = self.below.len() - self.below.partition_point(|&x| x < z);
            return ((above + self.peaks.len()) as f64 / self.n.max(1) as f64).min(1.0);
        }
        if z.is_infinite() || self.scale <= 0.0 {
            return 0.0;
        }
        let share = self.peaks.len() as f64 / self.n.max(1) as f64;
        let y = z - self.t;
        let p = if self.shape.abs() < 1e-9 {
            (-y / self.scale).exp()
        } else {
            let base = 1.0 + self.shape * y / self.scale;
            if base <= 0.0 { 0.0 } else { base.powf(-1.0 / self.shape) }
        };
        share * p
    }

    /// Takes the next |z|: its probability, as it stood before it.
    pub fn observe(&mut self, z: f64) -> f64 {
        let p = self.probability(z);
        self.n += 1;
        if z > self.t && z.is_finite() && p > TAIL_RISK {
            self.peaks.push(z - self.t);
            self.fit();
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |z| of a normal law: the tail's probabilities follow the law's (within sampling), in the
    /// body empirically, past the 98% quantile by the fitted law.
    #[test]
    fn the_tail_s_probabilities_follow_the_law_it_learned() {
        // |N(0,1)| by the Box-Muller transform of a fixed generator.
        let mut seed = 3u64;
        let mut u = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let z: Vec<f64> = (0..20_000).map(|_| ((-2.0 * u().ln()).sqrt() * (std::f64::consts::TAU * u()).cos()).abs()).collect();
        let tail = Tail::new(&z).unwrap_or_else(|| Tail { below: Vec::new(), t: 0.0, n: 0, peaks: Vec::new(), shape: 0.0, scale: 0.0 });
        // P(|Z| >= 1) = 0.3173 (the body: empirical); past the 98% quantile, a normal law's excesses
        // are a little lighter than exponential, which the tail never assumes: P(|Z| >= 3) = 0.0027
        // and P(|Z| >= 4) = 6.3e-5 come out at most twice and eight times as likely, never less.
        let p1 = tail.probability(1.0);
        assert!((p1 - 0.3173).abs() < 0.02, "P(|z| >= 1) = {p1}");
        for (at, want, most) in [(3.0, 0.0027, 2.0), (4.0, 6.3e-5, 8.0)] {
            let p = tail.probability(at);
            assert!(p >= want * 0.9 && p <= want * most, "P(|z| >= {at}) = {p}, the law's {want}");
        }
        assert!(tail.probability(50.0) < 1e-9 && tail.probability(f64::INFINITY) == 0.0 && tail.probability(0.0) == 1.0);
    }

    #[test]
    fn a_window_keeps_its_median_as_values_come_and_go() {
        let mut w = Window::new(3);
        for x in [5.0, 1.0, 3.0] {
            w.push(x);
        }
        assert_eq!(w.median(), Some(3.0));
        w.push(10.0); // 5 leaves: 1, 3, 10
        assert_eq!(w.median(), Some(3.0));
        w.push(0.0); // 1 leaves: 3, 10, 0
        assert_eq!(w.median(), Some(3.0));
        w.push(f64::NAN);
        assert_eq!(w.len(), 3);
    }

    /// A daily cycle sampled every 5 minutes, noisy: its season is found, a day; white noise has
    /// none; a weekly cycle over a daily one, hourly, its week.
    #[test]
    fn a_daily_cycle_s_season_is_learned_and_noise_has_none() {
        let mut seed = 7u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        let daily: Vec<f64> = (0..288 * 4).map(|i| 50.0 + 20.0 * (i as f64 * std::f64::consts::TAU / 288.0).sin() + noise()).collect();
        assert_eq!(season(&daily, 300.0, 288), Some(288));
        let white: Vec<f64> = (0..288 * 4).map(|_| noise()).collect();
        assert_eq!(season(&white, 300.0, 288), None);
        // Hourly points, a weekly cycle besides the daily one: the week is found (168 points).
        let weekly: Vec<f64> = (0..168 * 4).map(|i| 10.0 * (i as f64 * std::f64::consts::TAU / 24.0).sin() + 30.0 * (i as f64 * std::f64::consts::TAU / 168.0).sin() + noise()).collect();
        assert_eq!(season(&weekly, 3600.0, 24), Some(168));
    }

    /// Each algorithm, after a quiet seasonal history, expects the cycle's next value within a few
    /// scales, and finds a spike tens of scales out.
    #[test]
    fn each_algorithm_expects_the_cycle_and_finds_a_spike() {
        let mut seed = 11u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        let value = |i: usize, n: f64| 50.0 + 20.0 * (i as f64 * std::f64::consts::TAU / 288.0).sin() + n;
        let history: Vec<f64> = (0..288 * 3).map(|i| value(i, noise())).collect();
        for algo in [Algorithm::Basic, Algorithm::Agile, Algorithm::Robust] {
            let mut d = Detector::new(algo, &history, 300.0, 288);
            let mut quiet = Vec::new();
            for i in 288 * 3..288 * 3 + 100 {
                if let Some(s) = d.step(value(i, noise())) {
                    quiet.push(s.z.abs());
                }
            }
            let worst = quiet.iter().copied().fold(0.0, f64::max);
            let spike = d.step(value(288 * 3 + 100, 0.0) + 40.0).map(|s| s.z).unwrap_or(0.0);
            // Basic has no season: a cycle of ±20 is part of its spread, a spike of 40 still stands out.
            let limit = if algo == Algorithm::Basic { 3.5 } else { 6.0 };
            assert!(worst < limit, "{algo:?}: quiet points scored up to {worst}");
            assert!(spike > 2.0 * worst.max(1.0), "{algo:?}: the spike's z {spike}, the quiet's up to {worst}");
        }
    }
}
