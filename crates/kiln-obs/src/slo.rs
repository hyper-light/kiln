//! Service level objectives (`docs/observe.md` §9.2): what share of what a service does is good, and
//! how fast it spends what it may get wrong.
//!
//! Two kinds of indicator (the SLI), both read from metrics, so an SLO keeps no history of its own:
//! - **ratio**: good events over all of them, each a metric query of counts (Datadog's metric-based
//!   SLO: `sum:http.requests{status:2xx}.as_count()` over `sum:http.requests{*}.as_count()`);
//! - **time slices**: the share of slices (1 or 5 minutes) whose query's value meets a condition
//!   (Datadog's time-slice SLO: `p99:http.latency{*}` under 0.3 s), of those with data.
//!
//! An SLO's target (99.9) is the share it promises over a rolling timeframe (7, 30 or 90 days); its
//! error budget the rest (0.1%). The **burn rate** over a window is the error rate there over the
//! budget's (Beyer et al., *The Site Reliability Workbook*, ch. 5, "Alerting on SLOs"): 1 spends
//! the budget in exactly the timeframe, 14.4 a 30-day budget's 2% in an hour. A burn-rate alert
//! asks it of a long window and a short one at once (the workbook's multiwindow, multi-burn-rate
//! alerts): the long one says the rate is real, the short one that it is still so.

use crate::monitor::Op;
use crate::query::eval::{Request, Source};

/// The timeframes an SLO may be over (Datadog's: 7, 30 and 90 days).
pub const TIMEFRAMES_MS: [i64; 3] = [7 * 86_400_000, 30 * 86_400_000, 90 * 86_400_000];
/// A time slice's widths (Datadog's: one minute or five).
pub const SLICES_MS: [i64; 2] = [60_000, 300_000];

/// What an SLO measures.
#[derive(Debug, Clone, PartialEq)]
pub enum Sli {
    /// Good events over total, each a metric query of counts, every series of each summed.
    Ratio { good: String, total: String },
    /// Slices of `slice_ms` whose value `op threshold` holds, over the slices with a value: each
    /// series' own.
    Slices { query: String, op: Op, threshold: f64, slice_ms: i64 },
}

/// An SLO, as the engine reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Slo {
    pub sli: Sli,
    /// Percent: 99.9.
    pub target: f64,
    pub timeframe_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SloError {
    #[error("{0}")]
    Invalid(String),
    #[error("a query: {0}")]
    Query(String),
}

impl Slo {
    pub fn check(&self) -> Result<(), SloError> {
        if !(self.target > 0.0 && self.target < 100.0) {
            return Err(SloError::Invalid(format!("a target between 0 and 100 (percent): {}", self.target)));
        }
        if !TIMEFRAMES_MS.contains(&self.timeframe_ms) {
            return Err(SloError::Invalid("a timeframe of 7, 30 or 90 days".into()));
        }
        let parse = |q: &str| crate::query::parse::parse(q).map(|_| ()).map_err(|e| SloError::Query(format!("{q}: {e}")));
        match &self.sli {
            Sli::Ratio { good, total } => {
                parse(good)?;
                parse(total)?;
            }
            Sli::Slices { query, threshold, slice_ms, .. } => {
                parse(query)?;
                if !threshold.is_finite() {
                    return Err(SloError::Invalid("a slice's threshold: a number".into()));
                }
                if !SLICES_MS.contains(slice_ms) {
                    return Err(SloError::Invalid("a slice of 1 or 5 minutes".into()));
                }
            }
        }
        Ok(())
    }

    /// The share of the timeframe's errors the target allows: 0.001 for 99.9.
    pub fn budget(&self) -> f64 {
        1.0 - self.target / 100.0
    }
}

/// The step a range is read at: the coarsest of an hour, five minutes and a minute that keeps its
/// buckets at 2,200 or fewer, so a long range reads the stores' rollups (`docs/observe.md` §4.7)
/// and the step stays within the range's precision a thousandth or so.
pub fn step_for(range_ms: i64) -> i64 {
    for step in [60_000, 300_000, 3_600_000] {
        if range_ms / step <= 2_200 {
            return step;
        }
    }
    86_400_000
}

/// Slices a read holds at the most: under the grid's 11,000 points (`query::eval::MAX_POINTS`).
const SLICES_A_READ: i64 = 10_000;

/// `query`'s series over `[from, to)` at `step`: the grid's first bucket's start, and each series'
/// values.
fn series(src: Source<'_>, query: &str, from: i64, to: i64, step: i64) -> Result<(i64, Vec<Vec<f64>>), SloError> {
    let mut r = Request::new(query, from, to);
    r.step = Some(step);
    let a = src.ask(&r).map_err(|e| SloError::Query(format!("{query}: {e}")))?;
    Ok((a.grid.from, a.series.into_iter().map(|s| s.values).collect()))
}

/// The SLI's good and total in each bucket of `step` over `[from, to)` (the grid's: a bucket begins
/// at a multiple of `step`): `(its start, good, total)`. A ratio's counts are each bucket's sums
/// across series; a time slice's, its slices met and with a value, read a slice at a time in parts
/// of [`SLICES_A_READ`]. Of a store held throughout, or of a shared one, each query read a part at
/// a time (`eval::Source`).
pub fn buckets<'a>(store: impl Into<Source<'a>>, sli: &Sli, from: i64, to: i64, step: i64) -> Result<Vec<(i64, f64, f64)>, SloError> {
    let src = store.into();
    let first = from.div_euclid(step) * step;
    let n = usize::try_from((to - first + step - 1) / step).unwrap_or(0);
    let mut out: Vec<(i64, f64, f64)> = (0..n).map(|i| (first + i as i64 * step, 0.0, 0.0)).collect();
    let mut add = |t: i64, good: f64, total: f64| {
        let i = usize::try_from((t - first).div_euclid(step)).unwrap_or(usize::MAX);
        if let Some(b) = out.get_mut(i) {
            b.1 += good;
            b.2 += total;
        }
    };
    match sli {
        Sli::Ratio { good, total } => {
            for (q, is_good) in [(good, true), (total, false)] {
                let (g0, all) = series(src, q, from, to, step)?;
                for s in all {
                    for (i, v) in s.into_iter().enumerate().filter(|(_, v)| v.is_finite()) {
                        let t = g0 + i as i64 * step;
                        if is_good { add(t, v, 0.0) } else { add(t, 0.0, v) }
                    }
                }
            }
        }
        Sli::Slices { query, op, threshold, slice_ms } => {
            let mut at = from;
            while at < to {
                let end = (at + SLICES_A_READ * slice_ms).min(to);
                let (g0, all) = series(src, query, at, end, *slice_ms)?;
                for s in all {
                    for (i, v) in s.into_iter().enumerate().filter(|(_, v)| v.is_finite()) {
                        let t = g0 + i as i64 * slice_ms;
                        if t >= at && t < end {
                            add(t, if op.passes(v, *threshold) { 1.0 } else { 0.0 }, 1.0);
                        }
                    }
                }
                at = end;
            }
        }
    }
    Ok(out)
}

/// The SLI's good and total over `[from, to)`, read at `step` (a ratio's buckets begin at multiples
/// of it, so the range's start is read to it).
pub fn counts<'a>(store: impl Into<Source<'a>>, sli: &Sli, from: i64, to: i64, step: i64) -> Result<(f64, f64), SloError> {
    Ok(buckets(store, sli, from, to, step)?.into_iter().fold((0.0, 0.0), |(g, t), b| (g + b.1, t + b.2)))
}

/// The `window_ms` of whole steps ([`step_for`]) before `now`: a window ends where its last whole
/// step does, so it holds no part of a step either end (a minute's step that began a second ago is
/// not yet one).
fn whole(window_ms: i64, now: i64) -> (i64, i64, i64) {
    let step = step_for(window_ms);
    let to = now.div_euclid(step) * step;
    (to - window_ms, to, step)
}

/// The burn rate over the `window_ms` before `now` (its whole steps): its error rate over the
/// budget's; none without data in it.
pub fn burn_rate<'a>(store: impl Into<Source<'a>>, slo: &Slo, window_ms: i64, now: i64) -> Result<Option<f64>, SloError> {
    let (from, to, step) = whole(window_ms, now);
    let (good, total) = counts(store, &slo.sli, from, to, step)?;
    Ok((total > 0.0).then(|| ((total - good).max(0.0) / total) / slo.budget()))
}

/// An SLO's standing over its timeframe up to `now`.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Percent; none without data.
    pub sli: Option<f64>,
    pub good: f64,
    pub total: f64,
    /// The error budget left, percent of it (below 0 once spent).
    pub budget_left: Option<f64>,
    /// Each UTC day the timeframe touches: its start, its SLI (percent), and the budget left over
    /// the timeframe through its end.
    pub days: Vec<(i64, Option<f64>, Option<f64>)>,
}

/// The budget left (percent of it) of good out of total against a target.
fn left(good: f64, total: f64, budget: f64) -> Option<f64> {
    (total > 0.0).then(|| 100.0 - ((total - good).max(0.0) / total) / budget * 100.0)
}

/// An SLO's standing: its SLI over the timeframe's whole steps ([`step_for`]: five minutes for a
/// week, an hour beyond), the budget left, and day by day (one read of a day's step: rollups'
/// hours; a slice SLO's slices).
pub fn status<'a>(store: impl Into<Source<'a>>, slo: &Slo, now: i64) -> Result<Status, SloError> {
    let src = store.into();
    let (from, to, step) = whole(slo.timeframe_ms, now);
    let (good, total) = counts(src, &slo.sli, from, to, step)?;
    let (mut g, mut t) = (0.0, 0.0);
    let days = buckets(src, &slo.sli, from, now, 86_400_000)?
        .into_iter()
        .map(|(day, dg, dt)| {
            g += dg;
            t += dt;
            (day, (dt > 0.0).then(|| dg / dt * 100.0), left(g, t, slo.budget()))
        })
        .collect();
    Ok(Status { sli: (total > 0.0).then(|| good / total * 100.0), good, total, budget_left: left(good, total, slo.budget()), days })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::batch::{Builder, Kind, SeriesKey};
    use crate::hash::Keyed;
    use crate::store::Store;

    /// Ten days of requests, a hundred a minute, a tenth of a percent failing but for one hour of
    /// day 8 when half do; and a latency, its p99 under 300 ms but in that hour.
    fn store(end: i64) -> Store {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let ok = SeriesKey::new("http.requests", [("status", "ok")], Kind::Count).unwrap();
        let bad = SeriesKey::new("http.requests", [("status", "error")], Kind::Count).unwrap();
        let lat = SeriesKey::new("http.latency.p99", [("service", "web")], Kind::Gauge).unwrap();
        for k in [&ok, &bad, &lat] {
            b.define(k);
        }
        let start = end - 10 * 86_400_000;
        let minutes = (0..10 * 1440i64).map(|m| start + m * 60_000);
        let outage = |t: i64| t >= end - 2 * 86_400_000 && t < end - 2 * 86_400_000 + 3_600_000;
        b.run(ok.fp, minutes.clone().map(|t| (t, if outage(t) { 50.0 } else { 99.9 })));
        b.run(bad.fp, minutes.clone().map(|t| (t, if outage(t) { 50.0 } else { 0.1 })));
        b.run(lat.fp, minutes.map(|t| (t, if outage(t) { 0.8 } else { 0.12 })));
        s.apply(&b.finish()).unwrap();
        s
    }

    fn ratio() -> Sli {
        Sli::Ratio { good: "sum:http.requests{status:ok}.as_count()".into(), total: "sum:http.requests{*}.as_count()".into() }
    }

    #[test]
    fn a_ratio_slo_s_sli_budget_and_burn_rates_are_the_workbook_s() {
        let end = 1_800_000_000_000i64.div_euclid(86_400_000) * 86_400_000 + 43_200_000;
        let s = store(end);
        let slo = Slo { sli: ratio(), target: 99.0, timeframe_ms: 7 * 86_400_000 };
        slo.check().unwrap();
        let st = status(&s, &slo, end).unwrap();
        // 7 days of 144,000 requests a day; the outage hour 3,000 of them failing among 6,000.
        let total = 7.0 * 1440.0 * 100.0;
        let failed = (7.0 * 1440.0 - 60.0) * 0.1 + 60.0 * 50.0;
        assert!((st.total - total).abs() < 1e-6, "{} of {total}", st.total);
        assert!((st.sli.unwrap() - (1.0 - failed / total) * 100.0).abs() < 1e-9, "{:?}", st.sli);
        let left = 100.0 - (failed / total) / 0.01 * 100.0;
        assert!((st.budget_left.unwrap() - left).abs() < 1e-6, "{:?} {left}", st.budget_left);
        assert!(st.days.len() == 8 && st.days.iter().filter_map(|d| d.1).any(|x| x < 98.0), "the outage's day: {:?}", st.days);
        // The burn rate: 0.1% of errors against a 1% budget is 0.1; the outage hour's 50%, 50.
        let calm = burn_rate(&s, &slo, 3_600_000, end).unwrap().unwrap();
        assert!((calm - 0.1).abs() < 1e-9, "{calm}");
        let during = burn_rate(&s, &slo, 3_600_000, end - 2 * 86_400_000 + 3_600_000).unwrap().unwrap();
        assert!((during - 50.0).abs() < 1e-9, "{during}");
        assert!(burn_rate(&s, &slo, 3_600_000, end - 20 * 86_400_000).unwrap().is_none(), "no data, no rate");
    }

    #[test]
    fn a_time_slice_slo_counts_the_slices_its_condition_holds_in() {
        let end = 1_800_000_000_000i64.div_euclid(86_400_000) * 86_400_000 + 43_200_000;
        let s = store(end);
        let slo = Slo { sli: Sli::Slices { query: "max:http.latency.p99{service:web}".into(), op: Op::Lt, threshold: 0.3, slice_ms: 300_000 }, target: 99.5, timeframe_ms: 7 * 86_400_000 };
        slo.check().unwrap();
        let st = status(&s, &slo, end).unwrap();
        // Seven days of 288 slices; the outage hour's twelve not met.
        assert_eq!(st.total, 7.0 * 288.0);
        assert_eq!(st.good, 7.0 * 288.0 - 12.0);
        let bad = [Slo { target: 100.0, ..slo.clone() }, Slo { timeframe_ms: 86_400_000, ..slo.clone() }, Slo { sli: Sli::Slices { query: "max:x{*}".into(), op: Op::Lt, threshold: 1.0, slice_ms: 120_000 }, ..slo.clone() }, Slo { sli: Sli::Ratio { good: "sum:x{".into(), total: "sum:x{*}".into() }, ..slo }];
        for b in bad {
            assert!(b.check().is_err(), "{b:?}");
        }
    }
}
