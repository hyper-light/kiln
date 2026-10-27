//! How much kiln's own work may use of a resource (docs/headroom.md, "The knee"): a budget on its
//! write rate, or on its CPU cores, that its work is admitted and paced to, found by bisection
//! between the highest level it held calm and the lowest level an alarm began at. With no alarm seen it doubles from the highest calm
//! rate; a level counts as held once it stayed calm as long as a shift takes to be detected; a new
//! regime forgets both bounds. No gain, step or window: halving the gap is all it does.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Knee {
    /// The highest write rate (bytes a second) held calm in this regime.
    pub safe: Option<f64>,
    /// The lowest write rate an alarm began at in this regime.
    pub unsafe_at: Option<f64>,
    /// What kiln's work may write a second now (none: no limit, nothing has gone wrong).
    pub budget: Option<f64>,
    /// Since when the budget has held calm, and the most delivered meanwhile.
    held_since: Option<u64>,
    held_max: f64,
    /// When the last cut was made, to time how long it takes to show.
    cut_at: Option<u64>,
    /// The longest a cut has taken to clear an alarm (µs).
    pub lag_us: Option<f64>,
}

impl Knee {
    /// An alarm began with `delivered` bytes a second being written: that rate is unsafe; the
    /// budget drops to the highest rate held calm below it, or to half of it when there's none.
    pub fn alarm(&mut self, now_us: u64, delivered: f64, unit: Unit) -> String {
        let r = delivered.max(0.0);
        self.unsafe_at = Some(self.unsafe_at.map_or(r, |u| u.min(r)));
        if self.safe.is_some_and(|s| s >= r) {
            // What held calm before doesn't now: the machine got slower under us.
            self.safe = None;
        }
        let to = self.safe.unwrap_or(r / 2.0);
        self.budget = Some(to);
        self.held_since = None;
        self.held_max = 0.0;
        self.cut_at = Some(now_us);
        format!("{} cut to {}: trouble began at {}{}", unit.what(), unit.fmt(to), unit.fmt(r), self.safe.map(|s| format!(", {} held calm before", unit.fmt(s))).unwrap_or_default())
    }

    /// Still in trouble after a cut has had `hold_us` to show: the rate now is unsafe too, and it
    /// is cut again.
    pub fn persisting(&mut self, now_us: u64, delivered: f64, hold_us: f64, unit: Unit) -> Option<String> {
        let since = self.cut_at?;
        if (now_us.saturating_sub(since) as f64) < hold_us {
            return None;
        }
        Some(self.alarm(now_us, delivered, unit))
    }

    /// The alarm cleared: how long the cut took to show is noted.
    pub fn clear(&mut self, now_us: u64) {
        if let Some(c) = self.cut_at.take() {
            let took = now_us.saturating_sub(c) as f64;
            self.lag_us = Some(self.lag_us.map_or(took, |l| l.max(took)));
        }
    }

    /// Calm now, `delivered` bytes a second being written; `limited`: kiln's work wants more than
    /// the budget; `hold_us`: how long a level must stay calm to count as held. Says what it
    /// raised the budget to, when it did.
    pub fn calm(&mut self, now_us: u64, delivered: f64, limited: bool, hold_us: f64, unit: Unit) -> Option<String> {
        let since = *self.held_since.get_or_insert(now_us);
        self.held_max = self.held_max.max(delivered.max(0.0));
        if (now_us.saturating_sub(since) as f64) < hold_us {
            return None;
        }
        // Held: the most written meanwhile was safe.
        let held = self.held_max;
        self.safe = Some(self.safe.map_or(held, |s| s.max(held)));
        if self.unsafe_at.is_some_and(|u| self.safe.is_some_and(|s| s >= u)) {
            self.unsafe_at = None;
        }
        self.held_since = Some(now_us);
        self.held_max = 0.0;
        let budget = self.budget?;
        if !limited {
            return None;
        }
        let safe = self.safe.unwrap_or(budget).max(budget);
        let to = match self.unsafe_at {
            Some(u) => (safe + u) / 2.0,
            None => safe * 2.0,
        };
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
    }
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
                    k.persisting(now, delivered, 1e6, Unit::Writes);
                } else {
                    k.alarm(now, delivered, Unit::Writes);
                    alarmed = true;
                }
            } else {
                if alarmed {
                    k.clear(now);
                    alarmed = false;
                }
                k.calm(now, delivered, true, 1e6, Unit::Writes);
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
        k.alarm(0, 400e6, Unit::Writes);
        assert_eq!(k.budget, Some(200e6));
        assert_eq!(k.calm(5_000_000, 50e6, false, 1e6, Unit::Writes), None);
        assert_eq!(k.calm(7_000_000, 50e6, false, 1e6, Unit::Writes), None);
        assert_eq!(k.budget, Some(200e6));
        k.clear(1_500_000);
        assert_eq!(k.lag_us, Some(1.5e6));
        k.regime();
        assert_eq!((k.budget, k.safe, k.unsafe_at, k.lag_us), (None, None, None, Some(1.5e6)));
    }

    #[test]
    fn nothing_wrong_yet_means_no_limit() {
        let mut k = Knee::default();
        assert_eq!(k.calm(10_000_000, 900e6, true, 1e6, Unit::Writes), None);
        assert_eq!(k.calm(20_000_000, 900e6, true, 1e6, Unit::Writes), None);
        assert_eq!(k.budget, None);
    }
}
