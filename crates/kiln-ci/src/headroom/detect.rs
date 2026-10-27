//! Whether a target is in trouble (docs/headroom.md, "Detection"): a CUSUM on the logarithm of its
//! latency, against the mean of the calm regime it's in, looking for a shift toward its tolerance.
//! CUSUM detects a shift fastest for a given false-alarm rate (Lorden; Moustakides), but its usual
//! threshold assumes independent samples and latency comes in bursts; so the threshold is
//! calibrated on the machine's own record: the excursions the statistic made that ended without
//! harm (a false alarm is an alarm no harm followed), against the false alarms the budget allows
//! over the time recorded. A short record can't vouch for rare events (a new excursion beats the
//! largest of E with chance 1/(E+1)), so the threshold is also held above where the record's own
//! exponential tail (CUSUM's excursions are near-exponential: Siegmund) puts the budget's rate.
//! A sample (or a probe in flight) at the tolerance is harm, alarmed at
//! once, so calibration can never raise the threshold past what matters. A shift down (a faster
//! disk) starts a new regime; so does the engine when an alarm outlasts anything kiln's own cuts
//! ever took to clear (the machine itself changed), and everything learned starts again.
//!
//! Every estimate is exact (counts, sums, extremes); nothing here is a tuning constant.

use serde::{Deserialize, Serialize};

/// Microseconds in a day: budgets are per day.
pub const DAY_US: f64 = 86_400e6;

/// What a detector saw that changes what it says.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Event {
    /// Nothing changed.
    Same,
    /// In trouble from now, and why.
    Alarm(String),
    /// Out of trouble.
    Clear,
    /// The machine changed: what was learned starts again.
    Regime(String),
}

/// The largest excursions a CUSUM made while calm, as many as calibration can need, and a bound on
/// the ones let go (all of them at most `dropped_max`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Peaks {
    /// Largest first.
    kept: Vec<f64>,
    dropped_max: Option<f64>,
    /// Every excursion recorded: how many, and their heights' sum (the exponential tail's fit).
    count: u64,
    sum: f64,
}

impl Peaks {
    fn add(&mut self, p: f64, keep: usize) {
        self.count += 1;
        self.sum += p;
        let at = self.kept.partition_point(|q| *q >= p);
        self.kept.insert(at, p);
        while self.kept.len() > keep.max(1) {
            if let Some(d) = self.kept.pop() {
                self.dropped_max = Some(self.dropped_max.map_or(d, |m| m.max(d)));
            }
        }
    }

    /// The threshold that lets at most `allowed` of the excursions seen pass it: just at the
    /// (allowed + 1)-th largest. With fewer than that seen, any rise passes no more than allowed:
    /// zero. When the one needed was let go, the largest let go bounds it from above (fewer
    /// alarms than allowed, never more).
    fn empirical(&self, allowed: usize) -> f64 {
        match self.kept.get(allowed) {
            Some(p) => *p,
            None if self.kept.len() + usize::from(self.dropped_max.is_some()) <= allowed => 0.0,
            None => self.dropped_max.unwrap_or(0.0),
        }
    }

    /// Where the record's exponential tail puts `budget_per_day` alarms a day, over `days`
    /// recorded: excursions come at `count / days` a day, a height h is passed with chance
    /// exp(-h / mean height), so h = mean height x ln(rate / budget). Zero with nothing recorded.
    fn tail(&self, budget_per_day: f64, days: f64) -> f64 {
        if self.count == 0 || self.sum <= 0.0 || days <= 0.0 || budget_per_day <= 0.0 {
            return 0.0;
        }
        let rate = self.count as f64 / days;
        let mean = self.sum / self.count as f64;
        if rate <= budget_per_day { 0.0 } else { mean * (rate / budget_per_day).ln() }
    }

    /// The threshold: no lower than the record allows (`allowed` of its excursions past it) nor
    /// than its tail puts the budget's rate.
    fn threshold(&self, allowed: usize, budget_per_day: f64, days: f64) -> f64 {
        self.empirical(allowed).max(self.tail(budget_per_day, days))
    }
}

/// One side of a CUSUM, with its calm record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Side {
    s: f64,
    peak: f64,
    /// A sample reached the tolerance during the excursion under way.
    harm: bool,
    peaks: Peaks,
}

impl Side {
    /// One step by `step` (the sample's distance past the reference); an excursion that ended
    /// without harm goes to the record.
    fn step(&mut self, step: f64, keep: usize) {
        self.s = (self.s + step).max(0.0);
        self.peak = self.peak.max(self.s);
        if self.s == 0.0 && self.peak > 0.0 {
            if !self.harm {
                self.peaks.add(self.peak, keep);
            }
            self.peak = 0.0;
            self.harm = false;
        }
    }
}

/// A target's alarm under way.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alarm {
    pub since_us: u64,
    /// A sample reached the tolerance (not only a shift toward it).
    pub harm: bool,
    /// kiln's own heavy work was quiet for all of it.
    pub quiet: bool,
}

/// A target's detector: its regime's calm statistics and the CUSUMs on them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detector {
    pub tolerance_us: f64,
    pub regime_since_us: u64,
    last_us: u64,
    /// Calm samples (their log-latency) in this regime.
    n: u64,
    sum: f64,
    up: Side,
    down: Side,
    pub alarm: Option<Alarm>,
}

impl Detector {
    pub fn new(tolerance_us: f64, now_us: u64) -> Self {
        Self { tolerance_us, regime_since_us: now_us, last_us: now_us, n: 0, sum: 0.0, up: Side::default(), down: Side::default(), alarm: None }
    }

    /// The calm mean of log-latency, once there's a sample.
    pub fn mean(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }

    /// The calm typical latency (µs), once known: the geometric mean.
    pub fn calm_us(&self) -> Option<f64> {
        self.mean().map(f64::exp)
    }

    /// Half the shift from calm to the tolerance: CUSUM's reference for detecting it.
    fn reference(&self) -> Option<f64> {
        self.mean().map(|m| (self.tolerance_us.max(1.0).ln() - m) / 2.0)
    }

    fn days(&self) -> f64 {
        self.last_us.saturating_sub(self.regime_since_us) as f64 / DAY_US
    }

    /// How many false alarms the budget allows over this regime so far.
    fn allowed(&self, budget_per_day: f64) -> usize {
        (budget_per_day.max(0.0) * self.days()).floor() as usize
    }

    /// The threshold for alarming, calibrated on this regime's calm record.
    pub fn threshold(&self, budget_per_day: f64) -> f64 {
        self.up.peaks.threshold(self.allowed(budget_per_day), budget_per_day, self.days())
    }

    /// Samples a shift to the tolerance takes to alarm, on average (the CUSUM's drift past its
    /// reference is the reference itself): at least one.
    pub fn delay_samples(&self, budget_per_day: f64) -> f64 {
        match self.reference() {
            Some(k) if k > 0.0 => (self.threshold(budget_per_day) / k).ceil().max(1.0),
            _ => 1.0,
        }
    }

    /// The statistic now (for the status).
    pub fn level(&self) -> f64 {
        self.up.s
    }

    /// The machine changed: a new regime from now, learning again.
    pub fn restart(&mut self, now_us: u64) {
        let tolerance = self.tolerance_us;
        *self = Detector::new(tolerance, now_us);
    }

    /// A probe still out after `age_us`: harm once it's out as long as the tolerance.
    pub fn inflight(&mut self, now_us: u64, age_us: f64, quiet: bool) -> Event {
        self.last_us = self.last_us.max(now_us);
        if age_us >= self.tolerance_us && self.alarm.is_none() {
            self.alarm = Some(Alarm { since_us: now_us, harm: true, quiet });
            return Event::Alarm(format!("a probe has been out {:.0} ms, its tolerance {:.0} ms", age_us / 1e3, self.tolerance_us / 1e3));
        }
        Event::Same
    }

    /// A probe that answered in `us`. `quiet`: kiln's own heavy work is quiet now.
    pub fn sample(&mut self, now_us: u64, us: f64, quiet: bool, budget_per_day: f64) -> Event {
        self.last_us = self.last_us.max(now_us);
        if !us.is_finite() || us < 0.0 {
            return Event::Same;
        }
        let x = us.max(1.0).ln();
        if let Some(a) = self.alarm.as_mut() {
            a.quiet &= quiet;
        }
        if us >= self.tolerance_us {
            if self.alarm.is_none() {
                self.alarm = Some(Alarm { since_us: now_us, harm: true, quiet });
                self.up.harm = true;
                // Held up until it's well under again: the excursion counts from here.
                self.up.s = self.up.s.max(self.threshold(budget_per_day)).max(f64::MIN_POSITIVE);
                return Event::Alarm(format!("answered in {:.0} ms, its tolerance {:.0} ms", us / 1e3, self.tolerance_us / 1e3));
            }
            if let Some(a) = self.alarm.as_mut() {
                a.harm = true;
            }
            self.up.harm = true;
            self.up.s = self.up.s.max(self.threshold(budget_per_day));
            return Event::Same;
        }
        let (Some(mean), Some(k)) = (self.mean(), self.reference()) else {
            // Nothing learned yet: this is the first calm sample.
            self.n += 1;
            self.sum += x;
            return Event::Same;
        };
        if k <= 0.0 {
            // The calm mean is at the tolerance already: the regime itself is trouble.
            if self.alarm.is_none() {
                self.alarm = Some(Alarm { since_us: now_us, harm: true, quiet });
                return Event::Alarm("its calm latency has reached its tolerance".into());
            }
            return Event::Same;
        }
        let keep = self.allowed(budget_per_day) + 1;
        self.up.step(x - mean - k, keep);
        self.down.step(mean - x - k, keep);
        match self.alarm.clone() {
            None => {
                self.n += 1;
                self.sum += x;
                if self.down.s > self.down.peaks.threshold(self.allowed(budget_per_day), budget_per_day, self.days()) {
                    self.restart(now_us);
                    return Event::Regime("its latency fell well below its calm: a new regime".into());
                }
                let h = self.threshold(budget_per_day);
                if self.up.s > h {
                    self.alarm = Some(Alarm { since_us: now_us, harm: false, quiet });
                    return Event::Alarm(format!("latency rising toward its tolerance: typical {:.1} ms, now {:.1} ms (CUSUM {:.2} past {:.2})", mean.exp() / 1e3, us / 1e3, self.up.s, h));
                }
                Event::Same
            }
            Some(_) => {
                if self.up.s > 0.0 {
                    return Event::Same;
                }
                self.alarm = None;
                Event::Clear
            }
        }
    }
}

/// How often to probe a target (µs): the time left to act on trouble (its tolerance less the
/// longest a cut has taken to show, `lag_us`) shared over the samples a shift takes to alarm and
/// one more to see it start; never faster than the probe itself takes (`cost_us`), and for a target
/// that serves others, never more of its time than `load_share` of it (`service_us` a request).
pub fn interval_us(tolerance_us: f64, lag_us: f64, delay_samples: f64, cost_us: f64, service: Option<(f64, f64)>) -> f64 {
    let window = (tolerance_us - lag_us.max(0.0)).max(cost_us.max(1.0));
    let every = window / (delay_samples.max(1.0) + 1.0);
    let floor = match service {
        Some((service_us, share)) if share > 0.0 => cost_us.max(service_us / share),
        _ => cost_us,
    };
    every.max(floor).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic generator for the tests (xorshift64*), so traces are the same every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        /// A standard normal (Box-Muller).
        fn normal(&mut self) -> f64 {
            let (a, b) = (self.next().max(1e-12), self.next());
            (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
        }
    }

    /// Calm latency around 4 ms with bursty (AR(1)) noise and a periodic stall well under the
    /// tolerance, one sample every `every` µs, for `days`.
    fn calm_trace(seed: u64, days: f64, every: u64) -> Vec<(u64, f64)> {
        let mut r = Rng(seed);
        let mut e = 0.0;
        let n = (days * DAY_US / every as f64) as u64;
        (0..n)
            .map(|i| {
                e = 0.9 * e + 0.2 * r.normal();
                let t = i * every;
                // A 0.6 s writeback stall every ~25 s raises a sample or two to ~200 ms.
                let stall = if (t / 1_000_000).is_multiple_of(25) && t % 1_000_000 < every { 50.0 } else { 1.0 };
                (t, 4_000.0 * e.exp() * stall)
            })
            .collect()
    }

    #[test]
    fn a_calm_bursty_week_stays_within_a_one_a_day_budget() {
        let mut d = Detector::new(1_000_000.0, 0);
        let mut alarms = 0;
        let days = 7.0;
        for (t, us) in calm_trace(7, days, 2_000_000) {
            if let Event::Alarm(_) = d.sample(t, us, false, 1.0) {
                alarms += 1;
            }
        }
        let per_day = f64::from(alarms) / days;
        assert!(per_day <= 1.0, "{alarms} alarms in {days} days");
    }

    #[test]
    fn a_shift_toward_the_tolerance_alarms_before_any_sample_reaches_it() {
        let mut d = Detector::new(1_000_000.0, 0);
        let mut t = 0;
        for (tt, us) in calm_trace(3, 1.0, 1_000_000) {
            t = tt;
            d.sample(t, us, false, 1.0);
        }
        d.alarm = None;
        // Latency climbs geometrically from calm toward 1 s; no sample reaches it before the alarm.
        let mut us = 4_000.0;
        let mut alarmed_at = None;
        while us < 1_000_000.0 {
            t += 1_000_000;
            if let Event::Alarm(why) = d.sample(t, us, false, 1.0) {
                alarmed_at = Some((us, why));
                break;
            }
            us *= 1.25;
        }
        let (at, why) = alarmed_at.unwrap_or_default();
        assert!(at > 0.0 && at < 1_000_000.0, "alarm at {at} µs: {why}");
        assert!(why.contains("rising"), "{why}");
    }

    #[test]
    fn a_sample_or_a_probe_out_at_the_tolerance_is_harm_at_once() {
        let mut d = Detector::new(500_000.0, 0);
        assert!(matches!(d.sample(1, 600_000.0, false, 1.0), Event::Alarm(w) if w.contains("tolerance")));
        let mut d = Detector::new(500_000.0, 0);
        assert_eq!(d.inflight(1, 100_000.0, false), Event::Same);
        assert!(matches!(d.inflight(2, 500_000.0, false), Event::Alarm(_)));
        assert!(d.alarm.as_ref().is_some_and(|a| a.harm));
    }

    #[test]
    fn a_faster_disk_starts_a_new_regime() {
        let mut d = Detector::new(1_000_000.0, 0);
        let mut t = 0;
        for (tt, us) in calm_trace(5, 2.0, 1_000_000) {
            t = tt;
            d.sample(t, us, false, 1.0);
        }
        let before = d.calm_us().unwrap_or_default();
        let mut changed = false;
        for i in 0..100_000u64 {
            t += 1_000_000;
            if let Event::Regime(_) = d.sample(t, 100.0 + (i % 7) as f64, false, 1.0) {
                changed = true;
                break;
            }
        }
        assert!(changed, "no new regime");
        for _ in 0..10 {
            t += 1_000_000;
            d.sample(t, 100.0, false, 1.0);
        }
        let after = d.calm_us().unwrap_or_default();
        assert!(after < before / 5.0, "before {before} after {after}");
    }

    #[test]
    fn excursions_that_end_without_harm_raise_the_threshold_and_harm_never_does() {
        let mut d = Detector::new(1_000_000.0, 0);
        let mut t = 0;
        for (tt, us) in calm_trace(9, 1.0, 1_000_000) {
            t = tt;
            d.sample(t, us, false, 1.0);
        }
        let learned = d.threshold(1.0);
        assert!(learned > 0.0, "a day of benign stalls calibrates the threshold");
        // An excursion that reaches the tolerance is never learned as benign.
        let mut h = d.clone();
        h.sample(t + 1, 2_000_000.0, false, 1.0);
        for i in 0..60u64 {
            h.sample(t + 2 + i * 1_000_000, 4_000.0, false, 1.0);
        }
        assert!(h.threshold(1.0) <= learned + f64::EPSILON);
    }

    #[test]
    fn probe_intervals_follow_tolerance_lag_and_cost() {
        let fast = interval_us(1e6, 0.0, 3.0, 5e3, None);
        let tight = interval_us(2e5, 0.0, 3.0, 5e3, None);
        assert!(tight < fast);
        // Never under the probe's own cost.
        assert_eq!(interval_us(1e4, 0.0, 9.0, 5e3, None), 5e3);
        // A lag eats into the window.
        assert!(interval_us(1e6, 5e5, 3.0, 5e3, None) < fast);
        // A served target is held to its share: 1 ms a request at 10% is no more than every 10 ms.
        assert!(interval_us(1e5, 0.0, 9.0, 1e3, Some((1e3, 0.1))) >= 1e4);
    }

    #[test]
    fn peaks_keep_what_calibration_needs_and_bound_the_rest() {
        let mut p = Peaks::default();
        for v in [1.0, 5.0, 3.0, 4.0, 2.0] {
            p.add(v, 2);
        }
        assert_eq!(p.kept, vec![5.0, 4.0]);
        assert_eq!(p.dropped_max, Some(3.0));
        assert_eq!(p.empirical(0), 5.0);
        assert_eq!(p.empirical(1), 4.0);
        // Needing the third: the largest let go bounds it from above.
        assert_eq!(p.empirical(2), 3.0);
        assert_eq!(Peaks::default().empirical(0), 0.0);
        // Five excursions of mean height 3 in a day, budget one a day: 3 x ln 5.
        assert!((p.tail(1.0, 1.0) - 3.0 * 5f64.ln()).abs() < 1e-12);
        assert_eq!(p.tail(10.0, 1.0), 0.0);
    }
}
