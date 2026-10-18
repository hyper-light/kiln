//! Forecasts on the M4 competition's data (`docs/observe.md` §12.15): its hourly series (414, 48
//! points ahead, a day's season) and daily ones (4,227, 14 ahead), scored as M4 scores them (the
//! organisers' `Benchmarks and Evaluation.R`; Makridakis, Spiliotis and Assimakopoulos, "The M4
//! Competition: 100,000 time series and 61 forecasting methods", IJF 36(1), 2020): sMAPE, MASE and
//! OWA against Naive2 for the forecasts, MSIS and coverage of 95% intervals for the bounds.
//!
//! First M4's own benchmarks are made again (Naive, seasonal naive, Naive2 and Naive's intervals),
//! and must score what M4 published for them: that is the check of the scoring. Then kiln's
//! forecasts are scored beside them.
//!
//! `M4=<a folder of Hourly-train.csv, Hourly-test.csv, Daily-train.csv, Daily-test.csv> cargo bench
//! -p kiln-obs --bench m4` (from github.com/Mcompetitions/M4-methods, `Dataset/`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::time::Instant;

use hyper_measure::alloc;
use kiln_obs::forecast::{self, Algorithm, Model};

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

/// kiln's forecast of a series (the library's), as an answer: its 95% interval at 1.96 deviations.
fn kiln(x: &Series, f: &Freq, algo: Algorithm) -> Answer {
    kiln_with(x, f, algo, forecast::CHOICE_MARGIN)
}

fn kiln_with(x: &Series, f: &Freq, algo: Algorithm, margin: f64) -> Answer {
    let z = 1.959_963_984_540_054;
    let Some(fc) = forecast::forecast_with(&x.train, algo, f.h, margin) else { return Answer { point: vec![*x.train.last().unwrap(); f.h], interval: None } };
    let lo = (0..f.h).map(|k| fc.bounds(k, z).unwrap().0).collect();
    let hi = (0..f.h).map(|k| fc.bounds(k, z).unwrap().1).collect();
    Answer { point: fc.expected, interval: Some((lo, hi)) }
}

/// A frequency of M4's: its files, its period (`m`, M4-info's "Frequency") and horizon.
struct Freq {
    name: &'static str,
    m: usize,
    h: usize,
}

const HOURLY: Freq = Freq { name: "Hourly", m: 24, h: 48 };
const DAILY: Freq = Freq { name: "Daily", m: 1, h: 14 };

/// What M4 published for its benchmarks (`Evaluation and Ranks.xlsx`, "Point Forecasts-Frequency"
/// and "Prediction Intervals-Frequency"): sMAPE, MASE, OWA; and Naive's intervals' MSIS and ACD.
struct Published {
    naive: (f64, f64, f64),
    snaive: (f64, f64, f64),
    naive2: (f64, f64, f64),
    naive_pi: (f64, f64),
}

const HOURLY_PUBLISHED: Published = Published { naive: (43.003, 11.608, 3.593), snaive: (13.912, 1.193, 0.627), naive2: (18.383, 2.395, 1.000), naive_pi: (71.245, 0.011) };
const DAILY_PUBLISHED: Published = Published { naive: (3.045, 3.278, 1.000), snaive: (3.045, 3.278, 1.000), naive2: (3.045, 3.278, 1.000), naive_pi: (32.552, 0.021) };

/// One series: its history and what came after.
struct Series {
    train: Vec<f64>,
    test: Vec<f64>,
}

fn read_csv(path: &str) -> Vec<(String, Vec<f64>)> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut cells = l.split(',').map(|c| c.trim().trim_matches('"'));
            let id = cells.next().unwrap_or("").to_owned();
            let values: Vec<f64> = cells.filter(|c| !c.is_empty()).map(|c| c.parse().unwrap_or_else(|_| panic!("{path}: {c}"))).collect();
            (id, values)
        })
        .collect()
}

fn load(dir: &str, f: &Freq) -> Vec<Series> {
    let train = read_csv(&format!("{dir}/{}-train.csv", f.name));
    let test = read_csv(&format!("{dir}/{}-test.csv", f.name));
    assert_eq!(train.len(), test.len());
    train
        .into_iter()
        .zip(test)
        .map(|((a, train), (b, test))| {
            assert_eq!(a, b);
            assert_eq!(test.len(), f.h, "{a}");
            Series { train, test }
        })
        .collect()
}

/// A method's forecast of a series: the points, and its 95% interval if it gives one.
#[derive(Clone)]
struct Answer {
    point: Vec<f64>,
    interval: Option<(Vec<f64>, Vec<f64>)>,
}

/// Scores over a frequency: M4's sums, kept so totals are means over every point of every series.
#[derive(Default, Clone, Copy)]
struct Score {
    smape: f64,
    mase: f64,
    points: f64,
    msis: f64,
    covered: f64,
    pi_points: f64,
}

/// MASE's scale: the in-sample mean absolute seasonal difference (`m` the period; 1: the naive's).
fn mase_scale(train: &[f64], m: usize) -> f64 {
    let d: Vec<f64> = (m..train.len()).map(|j| (train[j] - train[j - m]).abs()).collect();
    d.iter().sum::<f64>() / d.len() as f64
}

fn score(series: &[Series], f: &Freq, answers: &[Answer]) -> Score {
    let mut s = Score::default();
    for (x, a) in series.iter().zip(answers) {
        let scale = mase_scale(&x.train, f.m);
        for (j, (&y, &p)) in x.test.iter().zip(&a.point).enumerate() {
            s.smape += 200.0 * (y - p).abs() / (y.abs() + p.abs());
            s.mase += (y - p).abs() / scale;
            s.points += 1.0;
            if let Some((lo, hi)) = &a.interval {
                let (l, u) = (lo[j], hi[j]);
                // Gneiting and Raftery's interval score at alpha = 0.05, scaled as MASE is.
                let alpha = 0.05;
                let is = (u - l) + if y < l { 2.0 / alpha * (l - y) } else { 0.0 } + if y > u { 2.0 / alpha * (y - u) } else { 0.0 };
                s.msis += is / scale;
                s.covered += f64::from(u8::from(y >= l && y <= u));
                s.pi_points += 1.0;
            }
        }
    }
    s
}

impl Score {
    fn smape(&self) -> f64 {
        self.smape / self.points
    }
    fn mase(&self) -> f64 {
        self.mase / self.points
    }
    fn owa(&self, naive2: &Score) -> f64 {
        (self.mase() / naive2.mase() + self.smape() / naive2.smape()) / 2.0
    }
    fn msis(&self) -> f64 {
        self.msis / self.pi_points
    }
    fn coverage(&self) -> f64 {
        self.covered / self.pi_points
    }
}

// ------------------------------------------------------------------ M4's benchmarks, made again

/// R's `acf`: autocorrelations at lags 1 to `max`, about the mean, over `n` (not `n - lag`).
fn acf(x: &[f64], max: usize) -> Vec<f64> {
    let n = x.len();
    let mean = x.iter().sum::<f64>() / n as f64;
    let c0: f64 = x.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
    (1..=max).map(|k| (0..n - k).map(|t| (x[t] - mean) * (x[t + k] - mean)).sum::<f64>() / n as f64 / c0).collect()
}

/// M4's seasonality test (`SeasonalityTest`): 90% (1.645), lag `m`'s autocorrelation against the
/// band its lower lags' widen (Bartlett).
fn seasonal(x: &[f64], m: usize) -> bool {
    if m <= 1 || x.len() < 3 * m {
        return false;
    }
    // R's default lag.max: 10 log10(n).
    let max = ((10.0 * (x.len() as f64).log10()).floor() as usize).max(m);
    let r = acf(x, max);
    let mut cum = 1.0;
    for r_k in r.iter().take(m - 1) {
        cum += 2.0 * r_k * r_k;
    }
    let clim = 1.645 / (x.len() as f64).sqrt() * cum.sqrt();
    r[m - 1].abs() > clim
}

/// R's `decompose(type = "multiplicative")$seasonal`'s figure: each phase's mean ratio to the
/// centred moving average, normalised to a mean of one; phase 0 is the series' first point.
fn seasonal_figure(x: &[f64], m: usize) -> Vec<f64> {
    let n = x.len();
    let half = m / 2;
    let mut sums = vec![0.0; m];
    let mut counts = vec![0usize; m];
    for t in half..n.saturating_sub(half) {
        // A 2 x m moving average for even m (its ends at half weight), m for odd.
        let trend = if m.is_multiple_of(2) {
            let mut s = 0.5 * x[t - half] + 0.5 * x[t + half];
            for v in &x[t - half + 1..t + half] {
                s += v;
            }
            s / m as f64
        } else {
            x[t - half..=t + half].iter().sum::<f64>() / m as f64
        };
        sums[t % m] += x[t] / trend;
        counts[t % m] += 1;
    }
    let figure: Vec<f64> = sums.iter().zip(&counts).map(|(s, &c)| s / c as f64).collect();
    let mean = figure.iter().sum::<f64>() / m as f64;
    figure.iter().map(|v| v / mean).collect()
}

fn naive(x: &Series, f: &Freq) -> Answer {
    let last = *x.train.last().unwrap();
    // The random walk's intervals: one-step errors' root mean square, times the root of the steps.
    let res: Vec<f64> = x.train.windows(2).map(|w| w[1] - w[0]).collect();
    let sigma = (res.iter().map(|e| e * e).sum::<f64>() / res.len() as f64).sqrt();
    let z = 1.959_963_984_540_054;
    let point = vec![last; f.h];
    let lo = (1..=f.h).map(|h| last - z * sigma * (h as f64).sqrt()).collect();
    let hi = (1..=f.h).map(|h| last + z * sigma * (h as f64).sqrt()).collect();
    Answer { point, interval: Some((lo, hi)) }
}

fn snaive(x: &Series, f: &Freq) -> Answer {
    if f.m <= 1 {
        return Answer { point: vec![*x.train.last().unwrap(); f.h], interval: None };
    }
    let last = &x.train[x.train.len() - f.m..];
    Answer { point: (0..f.h).map(|i| last[i % f.m]).collect(), interval: None }
}

fn naive2(x: &Series, f: &Freq) -> Answer {
    let n = x.train.len();
    if !seasonal(&x.train, f.m) {
        return Answer { point: vec![x.train[n - 1]; f.h], interval: None };
    }
    let fig = seasonal_figure(&x.train, f.m);
    let si = |t: usize| fig[t % f.m];
    let last = x.train[n - 1] / si(n - 1);
    Answer { point: (0..f.h).map(|i| last * si(n + i)).collect(), interval: None }
}

// ------------------------------------------------------------------ candidates (exploration)

/// A point forecast of `h` from a history.
type Point = dyn Fn(&[f64], usize) -> Vec<f64>;

/// Each horizon's error spread for `method`, from forecasts made at origins in the history (every
/// `stride` points, from `warm` on): the root mean square of errors at each horizon, made
/// non-decreasing (a later step is no surer than an earlier).
fn calibrate(train: &[f64], h: usize, warm: usize, stride: usize, method: &Point) -> Vec<f64> {
    let n = train.len();
    let mut sq = vec![0.0; h];
    let mut cnt = vec![0usize; h];
    let mut t = warm;
    while t + 1 < n {
        let p = method(&train[..t], h);
        for k in 0..h.min(n - t) {
            let e = train[t + k] - p[k];
            if e.is_finite() {
                sq[k] += e * e;
                cnt[k] += 1;
            }
        }
        t += stride;
    }
    let mut out: Vec<f64> = sq.iter().zip(&cnt).map(|(s, &c)| if c > 0 { (s / c as f64).sqrt() } else { f64::NAN }).collect();
    // Fill horizons with no errors from the last known, then make it non-decreasing.
    let mut last = f64::NAN;
    for v in &mut out {
        if v.is_nan() {
            *v = last;
        }
        last = *v;
    }
    let mut most = 0.0_f64;
    for v in &mut out {
        most = most.max(*v);
        *v = most;
    }
    out
}

fn with_bounds(train: &[f64], h: usize, warm: usize, stride: usize, method: &Point, z: f64) -> Answer {
    let point = method(train, h);
    let sigma = calibrate(train, h, warm, stride, method);
    let lo = point.iter().zip(&sigma).map(|(p, s)| p - z * s).collect();
    let hi = point.iter().zip(&sigma).map(|(p, s)| p + z * s).collect();
    Answer { point, interval: Some((lo, hi)) }
}

/// The same phase over the last `k` seasons: its mean (or median).
fn season_avg(x: &[f64], h: usize, m: usize, k: usize, median: bool) -> Vec<f64> {
    let n = x.len();
    let k = k.min(n / m).max(1);
    (0..h)
        .map(|i| {
            let mut v: Vec<f64> = (1..=k).filter_map(|j| (n + i % m).checked_sub(j * m).map(|t| x[t])).collect();
            if median {
                v.sort_by(f64::total_cmp);
                let l = v.len();
                if l % 2 == 1 { v[l / 2] } else { (v[l / 2 - 1] + v[l / 2]) / 2.0 }
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        })
        .collect()
}

/// A season's profile from the last `k` seasons (each season less its own mean), added to the last
/// season's level, and with `trend`, the seasons' means' slope (Theil-Sen) carried on.
fn season_level(x: &[f64], h: usize, m: usize, k: usize, trend: bool) -> Vec<f64> {
    let n = x.len();
    let k = k.min(n / m).max(1);
    let means: Vec<f64> = (1..=k).map(|j| x[n - j * m..n - (j - 1) * m].iter().sum::<f64>() / m as f64).collect();
    let mut profile = vec![0.0; m];
    for j in 1..=k {
        for p in 0..m {
            let t = n - j * m + p;
            profile[(t) % m] += (x[t] - means[j - 1]) / k as f64;
        }
    }
    let slope = if trend && k >= 2 {
        let mut slopes = Vec::new();
        for a in 0..k {
            for b in a + 1..k {
                // Seasons a and b back: means[a] is later.
                slopes.push((means[a] - means[b]) / ((b - a) * m) as f64);
            }
        }
        slopes.sort_by(f64::total_cmp);
        slopes[slopes.len() / 2]
    } else {
        0.0
    };
    let level = means[0];
    // The last season's mean stands at its middle.
    (0..h).map(|i| level + slope * ((m as f64 - 1.0) / 2.0 + 1.0 + i as f64) + profile[(n + i) % m]).collect()
}

/// Additive Holt-Winters with a damped trend (ETS(A,Ad,A) in error-correction form).
#[derive(Clone)]
struct Hw {
    a: f64,
    b: f64,
    g: f64,
    phi: f64,
    level: f64,
    trend: f64,
    season: Vec<f64>,
    t: usize,
    /// The damped trend's sums, phi + ... + phi^k, by k (k from 0).
    damp: Vec<f64>,
}

impl Hw {
    fn start(x: &[f64], m: usize, (a, b, g, phi): (f64, f64, f64, f64)) -> Self {
        let first = &x[..m.min(x.len())];
        let level = first.iter().sum::<f64>() / first.len() as f64;
        let season = if m > 1 { first.iter().map(|v| v - level).collect() } else { Vec::new() };
        let mut damp = vec![0.0; 400];
        let mut p = 1.0;
        for k in 1..damp.len() {
            p *= phi;
            damp[k] = damp[k - 1] + p;
        }
        // Its first season is its start: it runs from there.
        Self { a, b, g, phi, level, trend: 0.0, season, t: if m > 1 { m } else { 1 }, damp }
    }
    fn s(&self, t: usize) -> f64 {
        if self.season.is_empty() { 0.0 } else { self.season[t % self.season.len()] }
    }
    /// `k` steps ahead (1: the next).
    fn ahead(&self, k: usize) -> f64 {
        self.level + self.damp[k] * self.trend + self.s(self.t + k - 1)
    }
    fn update(&mut self, x: f64) {
        let e = x - self.ahead(1);
        self.level += self.phi * self.trend + self.a * e;
        self.trend = self.phi * self.trend + self.a * self.b * e;
        if !self.season.is_empty() {
            let i = self.t % self.season.len();
            self.season[i] += self.g * (1.0 - self.a) * e;
        }
        self.t += 1;
    }
}

/// The grid of smoothings tried (Hyndman et al. 2008's usual ranges).
fn grid(m: usize) -> Vec<(f64, f64, f64, f64)> {
    let gammas: &[f64] = if m > 1 { &[0.01, 0.05, 0.1, 0.2, 0.4] } else { &[0.0] };
    let mut out = Vec::new();
    for &a in &[0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9] {
        for &b in &[0.0, 0.01, 0.05, 0.1, 0.3] {
            for &g in gammas {
                for &phi in &[0.8, 0.9, 0.98, 1.0] {
                    out.push((a, b, g, phi));
                }
            }
        }
    }
    out
}

/// The smoothing that errs least on `x`: at one step (`h` 1), or over every step to `h` from
/// origins every `stride` points.
fn hw_fit(x: &[f64], m: usize, h: usize, stride: usize) -> (f64, f64, f64, f64) {
    let mut best = (f64::INFINITY, (0.3, 0.0, 0.1, 0.98));
    for p in grid(m) {
        let mut hw = Hw::start(x, m, p);
        let mut err = 0.0;
        for t in hw.t..x.len() {
            if h == 1 || (t - hw.t).is_multiple_of(stride) {
                for k in 1..=h.min(x.len() - t) {
                    err += (x[t + k - 1] - hw.ahead(k)).abs();
                }
            }
            hw.update(x[t]);
        }
        if err < best.0 {
            best = (err, p);
        }
    }
    best.1
}

/// A Holt-Winters forecast with its bounds: fitted once, its errors at each horizon from every
/// origin of one pass (the state at each point projected ahead and compared as the points come).
fn hw_answer(x: &[f64], h: usize, m: usize, p: (f64, f64, f64, f64), z: f64, warm: usize, blend: Option<&Point>) -> Answer {
    let mut hw = Hw::start(x, m, p);
    let n = x.len();
    let (mut sq, mut cnt) = (vec![0.0; h], vec![0usize; h]);
    for t in hw.t..n {
        if t >= warm {
            let other = blend.map(|b| b(&x[..t], h));
            for k in 1..=h.min(n - t) {
                let mut f = hw.ahead(k);
                if let Some(o) = &other {
                    f = (f + o[k - 1]) / 2.0;
                }
                let e = x[t + k - 1] - f;
                sq[k - 1] += e * e;
                cnt[k - 1] += 1;
            }
        }
        hw.update(x[t]);
    }
    let mut point: Vec<f64> = (1..=h).map(|k| hw.ahead(k)).collect();
    if let Some(b) = blend {
        let o = b(x, h);
        for (p, q) in point.iter_mut().zip(o) {
            *p = (*p + q) / 2.0;
        }
    }
    let mut sigma: Vec<f64> = sq.iter().zip(&cnt).map(|(s, &c)| if c > 0 { (s / c as f64).sqrt() } else { f64::NAN }).collect();
    let mut last = f64::NAN;
    let mut most = 0.0_f64;
    for v in &mut sigma {
        if v.is_nan() {
            *v = last;
        }
        last = *v;
        most = most.max(*v);
        *v = most;
    }
    let lo = point.iter().zip(&sigma).map(|(p, s)| p - z * s).collect();
    let hi = point.iter().zip(&sigma).map(|(p, s)| p + z * s).collect();
    Answer { point, interval: Some((lo, hi)) }
}

/// Theil and Sen's line through the last `w` points (the median of every pair's slope, its
/// intercept the median residual), carried on.
fn theil_sen(x: &[f64], h: usize, w: usize) -> Vec<f64> {
    let tail = &x[x.len().saturating_sub(w)..];
    let n = tail.len();
    if n < 2 {
        return vec![*x.last().unwrap(); h];
    }
    let mut slopes = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        for j in i + 1..n {
            slopes.push((tail[j] - tail[i]) / (j - i) as f64);
        }
    }
    let mid = slopes.len() / 2;
    let b = *slopes.select_nth_unstable_by(mid, f64::total_cmp).1;
    let mut r: Vec<f64> = tail.iter().enumerate().map(|(i, v)| v - b * i as f64).collect();
    let mid = r.len() / 2;
    let a = *r.select_nth_unstable_by(mid, f64::total_cmp).1;
    (0..h).map(|k| a + b * (n + k) as f64).collect()
}

fn row(name: &str, s: &Score, naive2: &Score, published: Option<(f64, f64, f64)>) {
    let pi = if s.pi_points > 0.0 { format!("{:>9.3} {:>8.3}", s.msis(), s.coverage()) } else { format!("{:>9} {:>8}", "", "") };
    let p = published.map(|(a, b, c)| format!("   M4: {a:.3} {b:.3} {c:.3}")).unwrap_or_default();
    println!("{:<34} {:>8.3} {:>8.3} {:>7.3} {pi}{p}", name, s.smape(), s.mase(), s.owa(naive2));
}

fn main() {
    let Ok(dir) = std::env::var("M4") else {
        eprintln!("set M4 to a folder of M4's Hourly and Daily train and test files (docs/observe.md §12.15)");
        return;
    };
    for (f, published) in [(&HOURLY, &HOURLY_PUBLISHED), (&DAILY, &DAILY_PUBLISHED)] {
        let series = load(&dir, f);
        println!("\n{} ({} series, {} ahead, m = {})", f.name, series.len(), f.h, f.m);
        println!("{:<34} {:>8} {:>8} {:>7} {:>9} {:>8}", "method", "sMAPE", "MASE", "OWA", "MSIS", "covered");
        let run = |method: &dyn Fn(&Series, &Freq) -> Answer| -> (Score, f64) {
            let t = Instant::now();
            let answers: Vec<Answer> = series.iter().map(|x| method(x, f)).collect();
            let secs = t.elapsed().as_secs_f64();
            (score(&series, f, &answers), secs)
        };
        let (n2, _) = run(&naive2);
        let (n1, _) = run(&naive);
        let (sn, _) = run(&snaive);
        row("Naive (and its intervals)", &n1, &n2, Some(published.naive));
        row("sNaive", &sn, &n2, Some(published.snaive));
        row("Naive2", &n2, &n2, Some(published.naive2));
        println!("{:<34} Naive's intervals, M4: MSIS {:.3}, ACD {:.3}; here MSIS {:.3}, ACD {:.3}", "", published.naive_pi.0, published.naive_pi.1, n1.msis(), (n1.coverage() - 0.95).abs());
        // kiln's, as shipped: time and allocations a series besides.
        let algos: Vec<(&str, Algorithm)> = if f.m > 1 {
            vec![("kiln seasonal", Algorithm::Seasonal { period: f.m })]
        } else {
            vec![("kiln linear, default", Algorithm::Linear(Model::Default)), ("kiln linear, reactive", Algorithm::Linear(Model::Reactive)), ("kiln linear, simple", Algorithm::Linear(Model::Simple))]
        };
        // The choice's margin, on either half of the series (odd and even places).
        if std::env::var("M4_MARGIN").is_ok() {
            let algo = algos[0].1;
            for margin in [0.0, 0.1, 0.2, 0.3, 0.5, 1.0] {
                let answers: Vec<Answer> = series.iter().map(|x| kiln_with(x, f, algo, margin)).collect();
                let half = |parity: usize| -> (Score, Score) {
                    let own: Vec<Series> = series.iter().enumerate().filter(|(i, _)| i % 2 == parity).map(|(_, x)| Series { train: x.train.clone(), test: x.test.clone() }).collect();
                    let a: Vec<Answer> = answers.iter().enumerate().filter(|(i, _)| i % 2 == parity).map(|(_, a)| a.clone()).collect();
                    let n2: Vec<Answer> = own.iter().map(|x| naive2(x, f)).collect();
                    (score(&own, f, &a), score(&own, f, &n2))
                };
                let ((o, o2), (e, e2)) = (half(1), half(0));
                println!("margin {margin:.1}: odd OWA {:.3} MSIS {:.3} | even OWA {:.3} MSIS {:.3}", o.owa(&o2), o.msis(), e.owa(&e2), e.msis());
            }
        }
        for (name, algo) in algos {
            alloc::begin();
            let (s, secs) = run(&|x: &Series, f: &Freq| kiln(x, f, algo));
            let a = alloc::end();
            row(&format!("{name} ({:.2} ms, {:.0} allocs a series)", secs * 1000.0 / series.len() as f64, a.calls() as f64 / series.len() as f64), &s, &n2, None);
        }
        if std::env::var("M4_EXPLORE").as_deref() == Ok("windows") {
            let z = 1.959_963_984_540_054;
            let half = |parity: usize| -> Vec<usize> { (0..series.len()).filter(|i| i % 2 == parity).collect() };
            let sub = |idx: &[usize], answers: &[Answer]| -> (Score, Score) {
                let xs: Vec<&Series> = idx.iter().map(|&i| &series[i]).collect();
                let s2: Vec<Answer> = idx.iter().map(|&i| naive2(&series[i], f)).collect();
                let a: Vec<Answer> = idx.iter().map(|&i| Answer { point: answers[i].point.clone(), interval: answers[i].interval.clone() }).collect();
                let own: Vec<Series> = xs.iter().map(|x| Series { train: x.train.clone(), test: x.test.clone() }).collect();
                (score(&own, f, &a), score(&own, f, &s2))
            };
            let windows: &[usize] = if f.m > 1 { &[6, 8, 10, 12, 15, 20] } else { &[10, 15, 20, 30, 40] };
            for &w in windows {
                let t = Instant::now();
                let answers: Vec<Answer> = series
                    .iter()
                    .map(|x| {
                        if f.m > 1 {
                            let tail = &x.train[x.train.len().saturating_sub(w * f.m)..];
                            let p = hw_fit(tail, f.m, f.h, f.m);
                            let m = f.m;
                            hw_answer(tail, f.h, f.m, p, z, 2 * f.m, Some(&move |x: &[f64], h: usize| season_avg(x, h, m, 1, false)))
                        } else {
                            let tail = &x.train[x.train.len().saturating_sub(w * f.h)..];
                            let p = hw_fit(tail, 1, 1, 1);
                            hw_answer(tail, f.h, 1, p, z, 2, Some(&|x: &[f64], h: usize| vec![*x.last().unwrap(); h]))
                        }
                    })
                    .collect();
                let secs = t.elapsed().as_secs_f64();
                let (odd, odd2) = sub(&half(1), &answers);
                let (even, even2) = sub(&half(0), &answers);
                println!(
                    "window {w:>2} ({secs:>5.1}s): odd OWA {:.3} MSIS {:.3} cov {:.3} | even OWA {:.3} MSIS {:.3} cov {:.3}",
                    odd.owa(&odd2),
                    odd.msis(),
                    odd.coverage(),
                    even.owa(&even2),
                    even.msis(),
                    even.coverage()
                );
            }
            continue;
        }
        if std::env::var("M4_EXPLORE").is_err() {
            continue;
        }
        if f.m <= 1 {
            let z = 1.959_963_984_540_054;
            let naive_point = |x: &[f64], h: usize| vec![*x.last().unwrap(); h];
            for w in [4usize, 10, 20] {
                let win = w * f.h;
                for (name, fit_h) in [("one step", 1usize), ("to the horizon", f.h)] {
                    let (s, secs) = run(&|x: &Series, f: &Freq| {
                        let tail = &x.train[x.train.len().saturating_sub(win)..];
                        let p = hw_fit(tail, 1, fit_h, 1);
                        hw_answer(tail, f.h, 1, p, z, 2, None)
                    });
                    row(&format!("Holt (damped or not), {name}, last {w}h ({secs:.1}s)"), &s, &n2, None);
                    let (s, secs) = run(&|x: &Series, f: &Freq| {
                        let tail = &x.train[x.train.len().saturating_sub(win)..];
                        let p = hw_fit(tail, 1, fit_h, 1);
                        hw_answer(tail, f.h, 1, p, z, 2, Some(&naive_point))
                    });
                    row(&format!("  ..., with Naive ({secs:.1}s)"), &s, &n2, None);
                }
            }
            for w in [2usize, 4, 8] {
                let win = w * f.h;
                let method = move |x: &[f64], h: usize| theil_sen(x, h, win);
                let (s, secs) = run(&|x: &Series, f: &Freq| with_bounds(&x.train[x.train.len().saturating_sub(win * 4)..], f.h, win, 1, &method, z));
                row(&format!("Theil-Sen over the last {w}h ({secs:.1}s)"), &s, &n2, None);
            }
            continue;
        }
        let z = 1.959_963_984_540_054;
        let m = f.m;
        let snaive_point = move |x: &[f64], h: usize| season_avg(x, h, m, 1, false);
        for seasons in [4usize, 6, 10] {
            let (s, secs) = run(&|x: &Series, f: &Freq| {
                let tail = &x.train[x.train.len().saturating_sub(seasons * m)..];
                let p = hw_fit(tail, m, f.h, m);
                hw_answer(tail, f.h, m, p, z, 2 * m, Some(&snaive_point))
            });
            row(&format!("HW on the last {seasons} seasons, with sNaive ({secs:.1}s)"), &s, &n2, None);
        }
        for (name, fit_h) in [("Holt-Winters, one step", 1usize), ("Holt-Winters, to the horizon", f.h)] {
            let (s, secs) = run(&|x: &Series, f: &Freq| {
                let p = hw_fit(&x.train, m, fit_h, m);
                hw_answer(&x.train, f.h, m, p, z, 2 * m, None)
            });
            row(&format!("{name} ({secs:.1}s)"), &s, &n2, None);
            let (s, secs) = run(&|x: &Series, f: &Freq| {
                let p = hw_fit(&x.train, m, fit_h, m);
                hw_answer(&x.train, f.h, m, p, z, 2 * m, Some(&snaive_point))
            });
            row(&format!("{name}, with sNaive ({secs:.1}s)"), &s, &n2, None);
        }
        if std::env::var("M4_EXPLORE").as_deref() != Ok("all") {
            continue;
        }
        for k in 1..=6 {
            for median in [false, true] {
                let method = move |x: &[f64], h: usize| season_avg(x, h, m, k, median);
                let (s, secs) = run(&|x: &Series, f: &Freq| with_bounds(&x.train, f.h, 2 * m * k.max(2), 1, &method, z));
                row(&format!("season {} of {k} ({secs:.1}s)", if median { "median" } else { "mean" }), &s, &n2, None);
            }
        }
        for k in 1..=6 {
            for trend in [false, true] {
                let method = move |x: &[f64], h: usize| season_level(x, h, m, k, trend);
                let (s, secs) = run(&|x: &Series, f: &Freq| with_bounds(&x.train, f.h, 2 * m * k.max(2), 1, &method, z));
                row(&format!("profile of {k}, level{} ({secs:.1}s)", if trend { ", trend" } else { "" }), &s, &n2, None);
            }
        }
    }
}
