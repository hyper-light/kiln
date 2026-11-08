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
//! A target that comes first ([`Detector::first`]: kiln's own server's delay, docs/priority.md) is
//! in trouble once it rises past what's calm, not only toward its tolerance: a second CUSUM steps
//! each sample past calm's noise, its reference half a standard deviation of calm's log-latency
//! above calm's mean (the shift that matters is one of calm's own spread; half of it is the
//! reference for detecting it, as above), calibrated the same way on its own record. Its alarm holds
//! until the target is back within calm's noise: the mirror of that CUSUM (what samples fell short
//! of its reference, added up) passing the same threshold, which at calm takes as many samples as
//! a rise past calm's noise takes to alarm. Calm, and both calibration records, come only from
//! excursions that ended with no alarm (or with kiln quiet) and with kiln pacing nothing of its own
//! (`paced`, the engine's to set): never from a degraded regime or a paced stretch (goal 8, plan
//! 01a1a59c2b29 step 5: on 11-05 the legs' 2.5 and the agents' 4 to 6 times calm sat below the
//! half-way-to-tolerance reference and became calm).
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
    /// One excursion's height `p`, keeping the `keep` largest (and a bound on the rest).
    pub fn add(&mut self, p: f64, keep: usize) {
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
    pub fn empirical(&self, allowed: usize) -> f64 {
        match self.kept.get(allowed) {
            Some(p) => *p,
            None if self.kept.len() + usize::from(self.dropped_max.is_some()) <= allowed => 0.0,
            None => self.dropped_max.unwrap_or(0.0),
        }
    }

    /// Where the record's exponential tail puts `budget_per_day` alarms a day, over `days`
    /// recorded: excursions come at `count / days` a day, a height h is passed with chance
    /// exp(-h / mean height), so h = mean height x ln(rate / budget). Zero with nothing recorded.
    pub fn tail(&self, budget_per_day: f64, days: f64) -> f64 {
        if self.count == 0 || self.sum <= 0.0 || days <= 0.0 || budget_per_day <= 0.0 {
            return 0.0;
        }
        let rate = self.count as f64 / days;
        let mean = self.sum / self.count as f64;
        if rate <= budget_per_day { 0.0 } else { mean * (rate / budget_per_day).ln() }
    }

    /// The threshold: no lower than the record allows (`allowed` of its excursions past it) nor
    /// than its tail puts the budget's rate.
    pub fn threshold(&self, allowed: usize, budget_per_day: f64, days: f64) -> f64 {
        self.empirical(allowed).max(self.tail(budget_per_day, days))
    }

    /// How many excursions were recorded.
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// One side of a CUSUM, with its calm record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Side {
    s: f64,
    peak: f64,
    /// The excursion under way isn't noise to calibrate on: a sample reached the tolerance, or an
    /// alarm with kiln's work running had kiln cut it (how it would have ended alone, unknown).
    harm: bool,
    peaks: Peaks,
}

impl Side {
    /// One step by `step` (the sample's distance past the reference); an excursion that ended
    /// without harm goes to the record.
    fn step(&mut self, step: f64, keep: usize) {
        self.step_ended(step, keep);
    }

    /// [`Side::step`], saying whether an excursion ended with this step: `Some(true)` when it went
    /// to the record (it was calm), `Some(false)` when it didn't.
    fn step_ended(&mut self, step: f64, keep: usize) -> Option<bool> {
        self.s = (self.s + step).max(0.0);
        self.peak = self.peak.max(self.s);
        if self.s == 0.0 && self.peak > 0.0 {
            let calm = !self.harm;
            if calm {
                self.peaks.add(self.peak, keep);
            }
            self.peak = 0.0;
            self.harm = false;
            return Some(calm);
        }
        None
    }
}

/// The samples of an excursion past calm's noise under way, learned as calm until it shows it
/// isn't (an alarm with kiln working, kiln paced, harm): their log-latency's count, sum and squares'
/// sum, taken back out of calm then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Held {
    n: u64,
    sum: f64,
    sumsq: f64,
}

impl Held {
    fn add(&mut self, x: f64) {
        self.n += 1;
        self.sum += x;
        self.sumsq += x * x;
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
    /// Their squares added up: calm's spread.
    #[serde(default)]
    sumsq: f64,
    up: Side,
    down: Side,
    /// A target that comes first: the CUSUM past calm's noise, its calm record, and the time that
    /// record covers (µs of samples that could calibrate it).
    #[serde(default)]
    past: Side,
    #[serde(default)]
    past_us: f64,
    /// The samples of the excursion past calm's noise under way, learned until it shows it isn't calm.
    #[serde(default)]
    held: Held,
    /// During an alarm on a target that comes first, from the samples that can show kiln's last cut
    /// (whose time began at `counts_from_us` or later): what fell short of the reference past calm's
    /// noise, added up (`back`: back within calm's noise once it passes the threshold), and what
    /// went past it (`still`: still past calm's noise once that does).
    #[serde(default)]
    back: f64,
    #[serde(default)]
    still: f64,
    #[serde(default)]
    counts_from_us: u64,
    pub alarm: Option<Alarm>,
    /// The longest a cut of kiln's has taken to show (the engine's measure): an excursion that
    /// outlasts it under an alarm with kiln working may have been ended by kiln, so isn't noise.
    #[serde(skip)]
    pub lag_us: Option<f64>,
    /// Its target's window ([`super::Target::window_us`], the engine's to set): a probe out this
    /// long is harm, and how long one has been out says nothing of its answer.
    #[serde(skip)]
    pub window_us: Option<f64>,
    /// Its target comes first ([`super::Target::first`], the engine's to set): a rise past calm's
    /// noise is trouble, and an alarm holds until it's back within it.
    #[serde(skip)]
    pub first: bool,
    /// kiln holds some of its own work below what it wants now (the engine's to set): what's
    /// measured meanwhile isn't calm (for a target that comes first).
    #[serde(default)]
    pub paced: bool,
    /// When the last sample came: a target with a window answers for the time since.
    #[serde(default)]
    prev_us: Option<u64>,
    /// The fastest answer in this regime (µs): what a probe takes with nothing queued ahead of it,
    /// near enough, for what the target can afford of probes ([`Detector::probe_share`]).
    #[serde(default)]
    fastest_us: Option<f64>,
}

impl Detector {
    pub fn new(tolerance_us: f64, now_us: u64) -> Self {
        Self {
            tolerance_us,
            regime_since_us: now_us,
            last_us: now_us,
            n: 0,
            sum: 0.0,
            sumsq: 0.0,
            up: Side::default(),
            down: Side::default(),
            past: Side::default(),
            past_us: 0.0,
            held: Held::default(),
            back: 0.0,
            still: 0.0,
            counts_from_us: 0,
            alarm: None,
            lag_us: None,
            window_us: None,
            first: false,
            paced: false,
            prev_us: None,
            fastest_us: None,
        }
    }

    /// Calm's standard deviation of log-latency, once calm samples differ.
    pub fn spread(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let n = self.n as f64;
        let mean = self.sum / n;
        Some((self.sumsq / n - mean * mean).max(0.0).sqrt()).filter(|s| *s > 0.0)
    }

    /// The share of the target's time its probes may take (LB05, docs/protect.md "Probes"): no more
    /// than would move its latency by its own detector's reference, the shift it looks for (half
    /// calm's standard deviation for a target that comes first, half the way to the tolerance for
    /// any other), so probing can't make the trouble it looks for. As an M/M/1 queue (its mean
    /// response s/(1 - ρ): L. Kleinrock, Queueing Systems vol. 1, 1975), probes taking δ of it raise
    /// its latency by (1 - ρ)/(1 - ρ - δ), within e^k while δ ≤ (1 - ρ)(1 - e^-k); its idle share
    /// 1 - ρ is its fastest answer over its calm one (s over s/(1 - ρ)). A fastest answer from a
    /// cache, faster than the service, makes it smaller: probes slower, never faster. None until
    /// calm, its spread and its fastest answer are known.
    pub fn probe_share(&self) -> Option<f64> {
        let (calm, fastest) = (self.calm_us()?, self.fastest_us?);
        self.spread()?;
        let k = self.past_reference().or_else(|| self.reference())?;
        let idle = (fastest / calm).clamp(0.0, 1.0);
        Some(idle * (1.0 - (-k).exp())).filter(|s| s.is_finite() && *s > 0.0)
    }

    /// For a target that comes first: CUSUM's reference past calm's noise, half calm's standard
    /// deviation (the shift that matters is one of calm's own spread).
    fn past_reference(&self) -> Option<f64> {
        if !self.first {
            return None;
        }
        self.spread().map(|s| s / 2.0)
    }

    fn past_days(&self) -> f64 {
        self.past_us / DAY_US
    }

    /// The threshold for a rise past calm's noise, calibrated as [`Detector::threshold`] is on its
    /// own record, over the time that record covers.
    pub fn past_threshold(&self, budget_per_day: f64) -> f64 {
        let days = self.past_days();
        self.past.peaks.threshold((budget_per_day.max(0.0) * days).floor() as usize, budget_per_day, days)
    }

    /// Samples a rise past calm's noise takes to alarm, and, mirrored, a return within it to clear
    /// (the drift of a shift of calm's spread past its reference is the reference itself): at least
    /// the samples a shift toward the tolerance takes. As [`Detector::delay_samples`] for a target
    /// that doesn't come first.
    pub fn settle_samples(&self, budget_per_day: f64) -> f64 {
        let toward = self.delay_samples(budget_per_day);
        match self.past_reference() {
            Some(k) => toward.max((self.past_threshold(budget_per_day) / k).ceil()),
            None => toward,
        }
    }

    /// kiln just cut its own work for this alarm, and the cut can show in what's measured from
    /// `show_us`: the evidence either way starts again from samples measured from then.
    pub fn cut(&mut self, show_us: u64) {
        self.back = 0.0;
        self.still = 0.0;
        self.counts_from_us = show_us;
    }

    /// Of an alarm on a target that comes first: since kiln's last cut could show, it went on rising
    /// past calm's noise as far as the threshold. The cut wasn't enough.
    pub fn persists(&self, budget_per_day: f64) -> bool {
        self.alarm.is_some() && self.past_reference().is_some() && self.still > self.past_threshold(budget_per_day)
    }

    fn learn(&mut self, x: f64) {
        self.n += 1;
        self.sum += x;
        self.sumsq += x * x;
    }

    /// The excursion past calm's noise under way isn't calm after all: it neither calibrates, nor
    /// stays in calm, and nothing more of it is learned.
    fn not_calm(&mut self) {
        if self.past.harm {
            return;
        }
        self.past.harm = true;
        let h = std::mem::take(&mut self.held);
        self.n = self.n.saturating_sub(h.n);
        self.sum -= h.sum;
        self.sumsq -= h.sumsq;
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
    /// Whether a sample of `us` pushes the CUSUM up: past calm by more than the reference (with
    /// nothing learned yet, only the tolerance does).
    /// For a target that comes first, past calm's noise instead.
    pub fn rising(&self, us: f64) -> bool {
        match (self.mean(), self.past_reference().or_else(|| self.reference())) {
            (Some(mean), Some(k)) => us.max(f64::MIN_POSITIVE).ln() - mean > k,
            _ => us >= self.tolerance_us,
        }
    }

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

    /// Whoever watched this target is going (kiln's work was stopped with an excursion under way, or
    /// its watcher ends): how the excursion would have ended is unknown, so it calibrates nothing and
    /// what it put in calm comes back out; the next watcher starts at calm with what was learned
    /// before it (LB05, `crate::protect::guard`).
    pub fn forget_excursion(&mut self) {
        if self.past.s > 0.0 {
            self.not_calm();
        }
        for side in [&mut self.up, &mut self.down, &mut self.past] {
            side.s = 0.0;
            side.peak = 0.0;
            side.harm = false;
        }
        self.held = Held::default();
        self.back = 0.0;
        self.still = 0.0;
        self.alarm = None;
        self.prev_us = None;
    }

    /// The machine changed: a new regime from now, learning again. What the engine set (its
    /// target's window, whether it comes first, the lag, whether kiln paces) stays.
    pub fn restart(&mut self, now_us: u64) {
        let (lag, window, first, paced) = (self.lag_us, self.window_us, self.first, self.paced);
        *self = Detector::new(self.tolerance_us, now_us);
        (self.lag_us, self.window_us, self.first, self.paced) = (lag, window, first, paced);
    }

    /// A probe still out after `age_us`: harm once it's out as long as the tolerance (or the window,
    /// for a target that has one).
    pub fn inflight(&mut self, now_us: u64, age_us: f64, quiet: bool, budget_per_day: f64) -> Event {
        self.last_us = self.last_us.max(now_us);
        let (limit, of) = self.window_us.map_or((self.tolerance_us, "tolerance"), |w| (w, "window"));
        if age_us >= limit && self.alarm.is_none() {
            self.alarm = Some(Alarm { since_us: now_us, harm: true, quiet });
            self.cut(now_us);
            if self.first {
                self.not_calm();
            }
            return Event::Alarm(format!("a probe has been out {:.0} ms, its {of} {:.0} ms", age_us / 1e3, limit / 1e3));
        }
        // Out this long, it answers at least this slow: were that its answer, would the CUSUM cross
        // the threshold? Then trouble is already here (nothing is recorded until it answers). Not
        // for a target with a window: its answer is what it measured, not how long it took.
        if self.alarm.is_none()
            && self.window_us.is_none()
            && let (Some(mean), Some(k)) = (self.mean(), self.reference())
            && k > 0.0
        {
            let s = (self.up.s + age_us.max(1.0).ln() - mean - k).max(0.0);
            let h = self.threshold(budget_per_day);
            if s > h {
                self.alarm = Some(Alarm { since_us: now_us, harm: false, quiet });
                self.up.s = s;
                return Event::Alarm(format!("a probe has been out {:.1} ms, typical {:.1} ms (CUSUM {s:.2} past {h:.2})", age_us / 1e3, mean.exp() / 1e3));
            }
        }
        Event::Same
    }

    /// When what a sample answering now measured began: for a target with a window, the last
    /// sample (it answers for the time since); for any other, when its probe was sent.
    pub fn began_us(&self, now_us: u64, us: f64) -> u64 {
        match self.window_us {
            Some(_) => self.prev_us.unwrap_or(now_us),
            None => now_us.saturating_sub(us.max(0.0) as u64),
        }
    }

    /// A probe answered with a failure after `us` (a 5xx, a 429): failed service however fast it
    /// came (LB05: a fast 503 can't look healthy), so harm, as a sample at the tolerance, said as a
    /// failure.
    pub fn failed(&mut self, now_us: u64, us: f64, quiet: bool, budget_per_day: f64) -> Event {
        match self.sample(now_us, self.tolerance_us, quiet, budget_per_day) {
            Event::Alarm(_) => Event::Alarm(format!("answered with a failure in {:.0} ms: failed service, however fast", us.max(0.0) / 1e3)),
            other => other,
        }
    }

    /// A probe that answered in `us`. `quiet`: kiln's own heavy work is quiet now.
    pub fn sample(&mut self, now_us: u64, us: f64, quiet: bool, budget_per_day: f64) -> Event {
        self.last_us = self.last_us.max(now_us);
        if !us.is_finite() || us < 0.0 {
            return Event::Same;
        }
        let began = self.began_us(now_us, us);
        let since = self.prev_us.replace(now_us);
        if us < self.tolerance_us {
            self.fastest_us = Some(self.fastest_us.map_or(us, |f| f.min(us)));
        }
        let x = us.max(1.0).ln();
        if let Some(a) = self.alarm.as_mut() {
            a.quiet &= quiet;
            if !a.quiet && self.lag_us.is_some_and(|l| now_us.saturating_sub(a.since_us) as f64 > l) {
                self.up.harm = true;
            }
        }
        if us >= self.tolerance_us {
            if self.first {
                // Harm is no calm: the excursion under way neither calibrates nor stays in calm.
                self.not_calm();
            }
            if self.alarm.is_none() {
                self.alarm = Some(Alarm { since_us: now_us, harm: true, quiet });
                self.cut(now_us);
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
            self.learn(x);
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
        // For a target that comes first, what's measured while kiln paces its own work, or under an
        // alarm with kiln working, isn't calm: it neither calibrates nor joins calm.
        let calm = !self.paced && self.alarm.as_ref().is_none_or(|a| a.quiet);
        if self.first && !calm && (self.up.s > 0.0 || x - mean - k > 0.0) {
            self.up.harm = true;
        }
        self.up.step(x - mean - k, keep);
        self.down.step(mean - x - k, keep);
        let past = self.past_reference().map(|kp| mean + kp);
        if let Some(r) = past {
            self.past_step(now_us, since, began, x, r, calm, budget_per_day);
        }
        match self.alarm.clone() {
            None => {
                if !self.first || (past.is_none() && calm) {
                    self.learn(x);
                }
                if self.down.s > self.down.peaks.threshold(self.allowed(budget_per_day), budget_per_day, self.days()) {
                    self.restart(now_us);
                    return Event::Regime("its latency fell well below its calm: a new regime".into());
                }
                let h = self.threshold(budget_per_day);
                if self.up.s > h {
                    self.alarm = Some(Alarm { since_us: now_us, harm: false, quiet });
                    self.cut(now_us);
                    return Event::Alarm(format!("latency rising toward its tolerance: typical {:.1} ms, now {:.1} ms (CUSUM {:.2} past {:.2})", mean.exp() / 1e3, us / 1e3, self.up.s, h));
                }
                // Past calm's noise, once the record holds an excursion to tell it from (with none,
                // no rise can be told from noise).
                let hp = self.past_threshold(budget_per_day);
                if past.is_some() && self.past.peaks.count > 0 && self.past.s > hp {
                    self.alarm = Some(Alarm { since_us: now_us, harm: false, quiet });
                    self.cut(now_us);
                    return Event::Alarm(format!("rising past its calm: typical {:.2} ms, now {:.2} ms (CUSUM {:.2} past {:.2})", mean.exp() / 1e3, us / 1e3, self.past.s, hp));
                }
                Event::Same
            }
            Some(_) => {
                if self.up.s > 0.0 {
                    return Event::Same;
                }
                // A target that comes first: back within calm's noise only once what fell short of
                // the reference past it, since kiln's last cut could show, passes its threshold.
                if past.is_some() && self.back <= self.past_threshold(budget_per_day) {
                    return Event::Same;
                }
                self.alarm = None;
                // Back within calm's noise: the rise that alarmed is over (it calibrates nothing).
                self.past.s = 0.0;
                self.past.peak = 0.0;
                self.past.harm = false;
                Event::Clear
            }
        }
    }

    /// One sample of a target that comes first, stepped past calm's noise (`r`, the reference's
    /// log-latency): learned as calm while it is, an excursion's samples taken back out once it shows
    /// it isn't; under an alarm, the evidence either way from samples that can show kiln's cut.
    #[allow(clippy::too_many_arguments)]
    fn past_step(&mut self, now_us: u64, since: Option<u64>, began: u64, x: f64, r: f64, calm: bool, budget_per_day: f64) {
        if calm {
            self.past_us += since.map_or(0.0, |s| now_us.saturating_sub(s) as f64);
        }
        if !calm && (self.past.s > 0.0 || x > r) {
            self.not_calm();
        }
        let keep = (budget_per_day.max(0.0) * self.past_days()).floor() as usize + 1;
        let in_calm = calm && !self.past.harm;
        let ended = self.past.step_ended(x - r, keep).is_some();
        if in_calm {
            self.learn(x);
            if self.past.s > 0.0 {
                self.held.add(x);
            }
        }
        if ended {
            self.held = Held::default();
        }
        if self.alarm.is_some() && began >= self.counts_from_us {
            self.back = (self.back + r - x).max(0.0);
            self.still = (self.still + x - r).max(0.0);
        }
    }
}

/// How often to probe a target (µs): the time left to act on trouble (its tolerance less the
/// longest a cut has taken to show, `lag_us`) shared over the samples a shift takes to alarm and
/// one more to see it start. A lag at or past the tolerance leaves no time that probing faster
/// could win back, so it doesn't shorten the interval. Never more of the target's time than
/// `load_share` of it, at `cost_us` a probe: a disk, like a server, serves others besides the
/// probe (2026-10-30: a 221 s lag had every runner's disk probed back to back and production's
/// server about 270 times a second).
pub fn interval_us(tolerance_us: f64, lag_us: f64, delay_samples: f64, cost_us: f64, load_share: f64) -> f64 {
    let lag = lag_us.max(0.0);
    let lag = if lag < tolerance_us { lag } else { 0.0 };
    let window = (tolerance_us - lag).max(cost_us.max(1.0));
    let every = window / (delay_samples.max(1.0) + 1.0);
    let share = if load_share > 0.0 && load_share <= 1.0 { load_share } else { 1.0 };
    every.max(cost_us / share).max(1.0)
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
        assert_eq!(d.inflight(1, 100_000.0, false, 1.0), Event::Same);
        assert!(matches!(d.inflight(2, 500_000.0, false, 1.0), Event::Alarm(_)));
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
        let fast = interval_us(1e6, 0.0, 3.0, 5e3, 1.0);
        let tight = interval_us(2e5, 0.0, 3.0, 5e3, 1.0);
        assert!(tight < fast);
        // Never under the probe's own cost.
        assert_eq!(interval_us(1e4, 0.0, 9.0, 5e3, 1.0), 5e3);
        // A lag eats into the window.
        assert!(interval_us(1e6, 5e5, 3.0, 5e3, 1.0) < fast);
        // A lag at or past the tolerance leaves no time to win back: as with none.
        assert_eq!(interval_us(1e6, 1e6, 3.0, 5e3, 1.0), fast);
        assert_eq!(interval_us(1e6, 221e6, 3.0, 5e3, 1.0), fast);
        // Every target is held to its share: 1 ms a probe at 10% is no more than every 10 ms.
        assert!(interval_us(1e5, 0.0, 9.0, 1e3, 0.1) >= 1e4);
    }

    proptest::proptest! {
        /// However long the lag and however cheap the probe, probes take at most the load share
        /// of a target's time, and a lag at or past the tolerance probes no faster than none.
        #[test]
        fn probes_stay_within_the_load_share(tolerance in 1e3f64..1e8, lag in 0.0f64..1e9, delay in 1.0f64..100.0, cost in 1.0f64..1e6, share in 1e-4f64..=1.0) {
            let every = interval_us(tolerance, lag, delay, cost, share);
            proptest::prop_assert!(cost / every <= share * (1.0 + 1e-9), "{cost} µs a probe every {every} µs is over {share}");
            if lag >= tolerance {
                proptest::prop_assert_eq!(every, interval_us(tolerance, 0.0, delay, cost, share));
            }
        }
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
