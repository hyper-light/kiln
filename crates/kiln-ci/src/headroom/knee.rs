//! How much kiln's own work may use of a resource (docs/headroom.md, "The knee"): a budget on its
//! write rate, or on its CPU cores, that its work is admitted and paced to, found by bisection
//! between the highest level it held calm and the lowest level an alarm began at. With no alarm seen it doubles from the highest calm
//! rate; a level counts as held once it stayed calm as long as a shift takes to be detected; a new
//! regime forgets both bounds. No gain, step or window: halving the gap is all it does. Once the
//! gap is finer than the pacer can apply, the bound itself is tried: a bound that holds calm was
//! wrong (set by trouble that had another cause, or before a cut had shown), and is forgotten.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Knee {
    /// The highest write rate (bytes a second) held calm in this regime.
    pub safe: Option<f64>,
    /// The lowest write rate an alarm began at in this regime.
    pub unsafe_at: Option<f64>,
    /// Higher rates alarms began at: the next bound once a lower one is shown wrong.
    #[serde(default)]
    pub above: Vec<f64>,
    /// What kiln's work may write a second now (none: no limit, nothing has gone wrong).
    pub budget: Option<f64>,
    /// Since when the budget has held calm, and what was delivered meanwhile (its integral over
    /// time, and when it was last added to): a level is held only as sustained, so the mean.
    held_since: Option<u64>,
    #[serde(default)]
    held_area: f64,
    #[serde(default)]
    held_last: Option<u64>,
    /// When the last cut was made, to time how long it takes to show.
    cut_at: Option<u64>,
    /// The longest a cut has taken to show (µs): from the cut to the first probe after it that
    /// answered faster than the one before, however long the rest takes to drain.
    pub lag_us: Option<f64>,
}

impl Knee {
    /// An alarm began with `delivered` bytes a second being written: that rate is unsafe. With
    /// `drain` (the level that empties what's queued in time, where the machine's capacity is
    /// known) below it, the budget drops to that; else to the highest rate held calm below it, or
    /// to half of it when there's none.
    pub fn alarm(&mut self, now_us: u64, delivered: f64, drain: Option<f64>, unit: Unit) -> String {
        let r = delivered.max(0.0);
        if r <= 0.0 {
            // Trouble with none of it in use: nothing of kiln's to cut, and no level learned.
            return format!("{} not cut: none in use when trouble began", unit.what());
        }
        if let Some(u) = self.unsafe_at
            && r < u
        {
            self.above.push(u);
        }
        self.unsafe_at = Some(self.unsafe_at.map_or(r, |u| u.min(r)));
        if self.safe.is_some_and(|s| s >= r) {
            // What held calm before doesn't now: the machine got slower under us.
            self.safe = None;
        }
        let drained = drain.filter(|d| *d < r).map(|d| d.max(0.0));
        let to = drained.unwrap_or_else(|| self.safe.unwrap_or(r / 2.0));
        self.budget = Some(to);
        self.held_since = None;
        self.held_area = 0.0;
        self.held_last = None;
        self.cut_at = Some(now_us);
        let why = if drained.is_some() { ", what drains the queue within the tolerance".to_owned() } else { self.safe.map(|s| format!(", {} held calm before", unit.fmt(s))).unwrap_or_default() };
        format!("{} cut to {}: trouble began at {}{why}", unit.what(), unit.fmt(to), unit.fmt(r))
    }

    /// Still in trouble, and `worse`: of the probes sent since the last cut, a later one came back
    /// (or is out) no faster than an earlier one, so the cut has shown and isn't draining it. The rate now
    /// is unsafe too, and it is cut again. Probes already out when it was cut can't show it, so they
    /// never cut again: cuts follow evidence, not time.
    pub fn persisting(&mut self, now_us: u64, delivered: f64, drain: Option<f64>, worse: bool, unit: Unit) -> Option<String> {
        self.cut_at?;
        if !worse {
            return None;
        }
        Some(self.alarm(now_us, delivered, drain, unit))
    }

    /// The last cut showed (trouble began to drain): how long that took is noted.
    pub fn shown(&mut self, now_us: u64) {
        if let Some(c) = self.cut_at {
            let took = now_us.saturating_sub(c) as f64;
            self.lag_us = Some(self.lag_us.map_or(took, |l| l.max(took)));
        }
    }

    /// The alarm cleared; a cut not yet shown shows now at the latest.
    pub fn clear(&mut self, now_us: u64) {
        if self.cut_at.is_some() && self.lag_us.is_none() {
            self.shown(now_us);
        }
        self.cut_at = None;
    }

    /// Calm now (see [`Calm`]). Says what it raised the budget to, when it did.
    pub fn calm(&mut self, now_us: u64, c: Calm, unit: Unit) -> Option<String> {
        let Calm { delivered, limited, hold_us, resolution, cap } = c;
        let since = *self.held_since.get_or_insert(now_us);
        if let Some(last) = self.held_last {
            self.held_area += delivered.max(0.0) * now_us.saturating_sub(last) as f64;
        }
        self.held_last = Some(now_us);
        let span = now_us.saturating_sub(since) as f64;
        if span < hold_us || span <= 0.0 {
            return None;
        }
        // Held: what was written meanwhile, on average, was safe (a moment's burst a buffer took
        // isn't a rate the machine sustains).
        let held = self.held_area / span;
        self.safe = Some(self.safe.map_or(held, |s| s.max(held)));
        // Held calm at the bound (as near it as the pacer goes): the bound was wrong.
        let disproved = self.unsafe_at.is_some_and(|u| self.safe.is_some_and(|s| s + resolution >= u));
        if disproved {
            // The next bound up, if an alarm ever began above what held.
            let held = self.safe.unwrap_or_default() + resolution;
            self.above.retain(|u| *u > held);
            self.above.sort_by(f64::total_cmp);
            self.unsafe_at = (!self.above.is_empty()).then(|| self.above.remove(0));
        }
        self.held_since = Some(now_us);
        self.held_area = 0.0;
        let budget = self.budget?;
        if !limited {
            return None;
        }
        let safe = self.safe.unwrap_or(budget).max(budget);
        let to = match self.unsafe_at {
            // Finer than the pacer can apply: try the bound itself.
            Some(u) if u - safe <= 2.0 * resolution => u,
            Some(u) => (safe + u) / 2.0,
            None => safe * 2.0,
        };
        let to = cap.map_or(to, |c| to.min(c));
        if to <= budget {
            return None;
        }
        self.budget = Some(to);
        Some(format!("{} raised to {}: {} held calm{}", unit.what(), unit.fmt(to), unit.fmt(safe), self.unsafe_at.map(|u| format!(", trouble at {}", unit.fmt(u))).unwrap_or_default()))
    }

    /// A new regime: what was learned about this machine's rates no longer holds.
    pub fn regime(&mut self) {
        let lag = self.lag_us;
        *self = Knee { lag_us: lag, ..Knee::default() };
        self.above.clear();
    }
}

/// What a calm moment says to a knee.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calm {
    /// What kiln's work uses now (bytes written a second, or cores).
    pub delivered: f64,
    /// kiln's work wants more than the budget (running, or waiting to start).
    pub limited: bool,
    /// How long a level must stay calm to count as held.
    pub hold_us: f64,
    /// The smallest change in the budget the pacer can apply.
    pub resolution: f64,
    /// What the machine was measured to take, when known: never raised past it.
    pub cap: Option<f64>,
}

/// What a knee budgets, for what it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Bytes written a second.
    Writes,
    /// CPU cores.
    Cpu,
}

impl Unit {
    fn what(self) -> &'static str {
        match self {
            Unit::Writes => "kiln's writes",
            Unit::Cpu => "kiln's CPU",
        }
    }

    pub fn fmt(self, v: f64) -> String {
        match self {
            Unit::Writes => format!("{:.0} MB/s", v / 1e6),
            Unit::Cpu => format!("{v:.1} cores"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk that's in trouble whenever more than `knee` bytes a second are written, under work
    /// that always wants more than it gets; one step a second, a level held after one step.
    fn converge(knee: f64, start: f64) -> Knee {
        let mut k = Knee::default();
        let mut alarmed = false;
        let mut want = start;
        for t in 0..200u64 {
            let now = t * 1_000_000;
            let delivered = k.budget.map_or(want, |b| want.min(b));
            if delivered > knee {
                if alarmed {
                    k.persisting(now, delivered, None, true, Unit::Writes);
                } else {
                    k.alarm(now, delivered, None, Unit::Writes);
                    alarmed = true;
                }
            } else {
                if alarmed {
                    k.clear(now);
                    alarmed = false;
                }
                k.calm(now, Calm { delivered, limited: true, hold_us: 1e6, resolution: delivered / 100.0, cap: None }, Unit::Writes);
            }
            want = want.max(delivered * 2.0);
        }
        k
    }

    #[test]
    fn the_budget_converges_to_within_a_tenth_of_a_hidden_knee() {
        for (knee, start) in [(800e6, 100e6), (50e6, 3_000e6), (1_234e6, 1e6)] {
            let k = converge(knee, start);
            let b = k.budget.unwrap_or_default();
            assert!((b / knee - 1.0).abs() <= 0.1, "knee {knee}: budget {b}");
            assert!(k.safe.unwrap_or_default() <= knee && k.unsafe_at.unwrap_or(f64::INFINITY) > knee, "{k:?}");
        }
    }

    #[test]
    fn work_under_the_budget_never_raises_it_and_a_new_regime_forgets() {
        let mut k = Knee::default();
        k.alarm(0, 400e6, None, Unit::Writes);
        assert_eq!(k.budget, Some(200e6));
        assert_eq!(k.calm(5_000_000, Calm { delivered: 50e6, limited: false, hold_us: 1e6, resolution: 0.5e6, cap: None }, Unit::Writes), None);
        assert_eq!(k.calm(7_000_000, Calm { delivered: 50e6, limited: false, hold_us: 1e6, resolution: 0.5e6, cap: None }, Unit::Writes), None);
        assert_eq!(k.budget, Some(200e6));
        k.clear(1_500_000);
        assert_eq!(k.lag_us, Some(1.5e6));
        k.regime();
        assert_eq!((k.budget, k.safe, k.unsafe_at, k.lag_us), (None, None, None, Some(1.5e6)));
    }

    #[test]
    fn a_wrong_bound_is_tried_and_forgotten_and_trouble_with_nothing_in_use_cuts_nothing() {
        let mut k = Knee::default();
        // An alarm at 400 MB/s that the disk could in fact take (its cause was elsewhere).
        k.alarm(0, 400e6, None, Unit::Writes);
        let mut t = 0;
        for _ in 0..64 {
            t += 1_000_000;
            let at = k.budget.unwrap_or_default();
            k.calm(t, Calm { delivered: at, limited: true, hold_us: 1e6, resolution: at / 100.0, cap: None }, Unit::Writes);
        }
        assert_eq!(k.unsafe_at, None, "{k:?}");
        assert!(k.budget.unwrap_or_default() > 400e6, "{k:?}");
        let mut k = Knee::default();
        k.alarm(0, 0.0, None, Unit::Writes);
        assert_eq!((k.budget, k.unsafe_at), (None, None));
    }

    #[test]
    fn nothing_wrong_yet_means_no_limit() {
        let mut k = Knee::default();
        assert_eq!(k.calm(10_000_000, Calm { delivered: 900e6, limited: true, hold_us: 1e6, resolution: 9e6, cap: None }, Unit::Writes), None);
        assert_eq!(k.calm(20_000_000, Calm { delivered: 900e6, limited: true, hold_us: 1e6, resolution: 9e6, cap: None }, Unit::Writes), None);
        assert_eq!(k.budget, None);
    }
}
