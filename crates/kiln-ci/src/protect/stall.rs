//! A CUSUM on a stall the kernel counts (LB05 step 2, docs/protect.md "Stalls"): PSI's "full", the
//! time every task of a cgroup waited on a resource at once, of the most stalled of the services a
//! watcher protects, in the window its kernel trigger uses.
//!
//! - **Harm** is a whole deadline of it in one window: every request of that service needing the
//!   resource then waited a deadline at least once in the window, or could have (a share of
//!   stall over a span holds a deadline in some window of it once it's a deadline a window, by
//!   averaging). Harm needs no record to vouch for it.
//! - **Reference.** Page's CUSUM ("Continuous inspection schemes", Biometrika 1954) for a shift
//!   from calm to harm steps by each window's stall past the halfway level between them, as
//!   headroom's detector does (`crate::headroom::detect`): calm is the exact mean of the stall per
//!   window measured while nothing alarmed.
//! - **Threshold** as headroom's, calibrated on the record of excursions that ended without harm or a
//!   stop, against the person's false-alarm budget over the time watched (`detect::Peaks`): its
//!   (allowed + 1)-th largest, held above where the record's exponential tail puts the budget. With
//!   no excursion recorded, nothing vouches for any level short of harm: only harm stops.
//! - **Woken by the kernel.** The watcher asks the kernel to wake it once a window's stall passes
//!   the reference ([`Stall::trigger_us`]); windows that don't pass it only bring the CUSUM down, and
//!   falls clamped at zero add up exactly as their sum does, so the windows nobody looked at cost
//!   nothing and lose nothing. The window that woke it holds at most a window's stall, and is taken
//!   to hold all it could of the span's: the CUSUM comes out at least what looking at every window
//!   would give, and at most a window's stall above it ([`advance`]).

use serde::{Deserialize, Serialize};

use crate::headroom::detect::{DAY_US, Peaks};

/// A watch's window (the kernel trigger's, `super::budget::psi_window_us`) and the stall in it that
/// is harm (a deadline's), µs.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub window_us: f64,
    pub harm_us: f64,
}

/// What a span said.
#[derive(Clone, Debug, PartialEq)]
pub enum Read {
    /// Short of the threshold (`s`: the CUSUM now, µs of stall past the reference).
    Calm { s: f64 },
    /// The CUSUM passed its threshold: a rise toward harm the record says isn't noise.
    Alarm { s: f64, threshold: f64, per_window_us: f64 },
    /// At least a deadline's stall a window over the span.
    Harm { per_window_us: f64 },
}

/// One resource's watch: its calm, its CUSUM and its record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stall {
    /// Stall (µs) and windows measured while nothing alarmed: calm's exact mean.
    calm_us: f64,
    calm_windows: f64,
    /// The CUSUM, its excursion's peak, and whether the excursion is no calm to record (harm in it).
    s: f64,
    peak: f64,
    spoiled: bool,
    peaks: Peaks,
    /// The time watched (µs), over which the false-alarm budget is spread.
    watched_us: f64,
}

/// The CUSUM `s` after a span of `windows` windows that stalled `stall_us` in all, against a
/// `reference_us` a window: one step when the span's windows were all short of the reference or
/// nothing can be said of its last (`woke` false: the kernel didn't wake the reading); else the
/// windows before the last first, then the last, given all the stall a window holds. Windows short
/// of the reference step the CUSUM down only, and steps down clamped at zero add up exactly as
/// their sum does: so this is exact for spans the kernel didn't wake, and for woken ones at least
/// the window-by-window value and at most a window's stall above it.
pub fn advance(s: f64, stall_us: f64, windows: f64, woke: bool, reference_us: f64, window_us: f64) -> f64 {
    if woke && windows > 1.0 {
        let last = stall_us.min(window_us);
        let before = (s + (stall_us - last) - reference_us * (windows - 1.0)).max(0.0);
        (before + last - reference_us).max(0.0)
    } else {
        (s + stall_us - reference_us * windows).max(0.0)
    }
}

impl Stall {
    /// Calm's stall a window (µs): none measured yet, none.
    pub fn calm_us(&self) -> f64 {
        if self.calm_windows > 0.0 { self.calm_us / self.calm_windows } else { 0.0 }
    }

    /// The reference a window (µs): halfway between calm and harm, within what a trigger takes
    /// (more than nothing, no more than the window).
    pub fn reference_us(&self, l: Limits) -> f64 {
        ((self.calm_us() + l.harm_us) / 2.0).clamp(1.0, l.window_us.max(1.0))
    }

    /// What the kernel should wake the watcher at: the reference, within the window.
    pub fn trigger_us(&self, l: Limits) -> f64 {
        self.reference_us(l).round()
    }

    fn days(&self) -> f64 {
        self.watched_us / DAY_US
    }

    /// The threshold, calibrated on the record against `budget_per_day` false alarms a day; with no
    /// excursion recorded, none short of harm.
    pub fn threshold(&self, budget_per_day: f64) -> f64 {
        if self.peaks.count() == 0 {
            return f64::INFINITY;
        }
        let days = self.days();
        self.peaks.threshold((budget_per_day.max(0.0) * days).floor() as usize, budget_per_day, days)
    }

    /// The CUSUM now.
    pub fn level(&self) -> f64 {
        self.s
    }

    /// A span of `span_us` in which the most stalled service stalled `stall_us`, woken by the
    /// kernel's trigger or not (`woke`), against `reference_us`: the reference the kernel's trigger
    /// was armed with over the span (what [`Stall::trigger_us`] said before it), so the windows it
    /// didn't wake for are the ones short of it.
    pub fn span(&mut self, stall_us: f64, span_us: f64, woke: bool, reference_us: f64, l: Limits, budget_per_day: f64) -> Read {
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !positive(span_us) || !positive(l.window_us) || !stall_us.is_finite() || !reference_us.is_finite() {
            return Read::Calm { s: self.s };
        }
        let windows = span_us / l.window_us;
        let stall = stall_us.clamp(0.0, span_us);
        let per_window = stall / windows.max(1.0);
        let harm = stall >= l.harm_us * windows.max(1.0);
        self.s = advance(self.s, stall, windows, woke, reference_us, l.window_us);
        self.peak = self.peak.max(self.s);
        self.spoiled |= harm;
        self.watched_us += span_us;
        let keep = (budget_per_day.max(0.0) * self.days()).floor() as usize + 1;
        let threshold = self.threshold(budget_per_day);
        if !harm && self.s <= threshold {
            self.calm_us += stall;
            self.calm_windows += windows;
        }
        if self.s == 0.0 && self.peak > 0.0 {
            if !self.spoiled {
                self.peaks.add(self.peak, keep);
            }
            self.peak = 0.0;
            self.spoiled = false;
        }
        if harm {
            Read::Harm { per_window_us: per_window }
        } else if self.s > threshold {
            Read::Alarm { s: self.s, threshold, per_window_us: per_window }
        } else {
            Read::Calm { s: self.s }
        }
    }

    /// The excursion under way ends unseen (its work was stopped, or its watcher goes): it records
    /// nothing, and the next watcher starts at calm.
    pub fn forget_excursion(&mut self) {
        self.s = 0.0;
        self.peak = 0.0;
        self.spoiled = false;
    }

    /// Excursions recorded.
    pub fn recorded(&self) -> u64 {
        self.peaks.count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const L: Limits = Limits { window_us: 2e6, harm_us: 1e6 };

    /// A span read as the guard reads it: against the reference armed before it.
    fn go(st: &mut Stall, stall_us: f64, span_us: f64, woke: bool) -> Read {
        let k = st.trigger_us(L);
        st.span(stall_us, span_us, woke, k, L, 1.0)
    }

    /// The CUSUM window by window, for comparison.
    fn by_window(s: f64, windows: &[f64], k: f64) -> f64 {
        windows.iter().fold(s, |s, x| (s + x - k).max(0.0))
    }

    proptest! {
        /// LB05 step 2: woken only when a window passes the reference, the CUSUM is exact over the
        /// windows it didn't see, and over a woken span at least the window-by-window value and at
        /// most a window's stall above it. Giving the woken span's stall to its quiet windows
        /// instead, or reading the span as one step, would fall short of it.
        #[test]
        fn lb05_the_kernel_woken_cusum_misses_nothing(s0 in 0.0f64..5e6, k in 1e5f64..2e6, quiet in prop::collection::vec(0.0f64..1.0, 0..40), last in 0.0f64..2e6) {
            let w = L.window_us;
            let quiet: Vec<f64> = quiet.iter().map(|q| q * k).collect();
            // Nobody woken: exact.
            let sum: f64 = quiet.iter().sum();
            let n = quiet.len() as f64;
            if n > 0.0 {
                let exact = by_window(s0, &quiet, k);
                prop_assert!((advance(s0, sum, n, false, k, w) - exact).abs() <= 1e-6 * (1.0 + exact));
            }
            // Woken by the last window.
            let mut all = quiet.clone();
            all.push(last);
            let truth = by_window(s0, &all, k);
            let got = advance(s0, sum + last, n + 1.0, true, k, w);
            prop_assert!(got + 1e-6 * (1.0 + truth) >= truth, "{got} short of {truth}");
            prop_assert!(got <= truth + (w - last) + 1e-6 * (1.0 + truth), "{got} past {truth} by more than a window");
        }
    }

    /// With nothing recorded, no level short of harm stops anything: a rise to 90% of harm a window,
    /// for an hour, never alarms (a threshold of zero, as an empty record gives the empirical rule,
    /// would stop the first window past halfway). A deadline in a window is harm at once.
    #[test]
    fn lb05_with_no_record_only_harm_stops() {
        let mut st = Stall::default();
        for _ in 0..1800 {
            let r = go(&mut st, 0.9e6, 2e6, true);
            assert!(matches!(r, Read::Calm { .. }), "{r:?}");
        }
        assert!(matches!(go(&mut st, 1e6, 2e6, true), Read::Harm { .. }));
    }

    /// A span shorter than a window holds a deadline only if its stall is one; a longer span holds one
    /// in some window once it averages a deadline a window. Prorating harm to a short span, or not
    /// averaging over a long one, would each get one of these wrong.
    #[test]
    fn lb05_harm_is_a_deadline_in_a_window() {
        let mut st = Stall::default();
        assert!(matches!(go(&mut st, 0.6e6, 0.6e6, false), Read::Calm { .. }), "0.6 s of stall in 0.6 s isn't a deadline");
        assert!(matches!(go(&mut st, 2.2e6, 4e6, true), Read::Harm { .. }), "2.2 s in two windows holds a deadline in one");
        let mut st = Stall::default();
        assert!(matches!(go(&mut st, 1.5e6, 4e6, true), Read::Calm { .. }), "1.5 s over two windows needn't");
    }

    /// Excursions that end without harm calibrate the threshold; one with harm in it, or one ended by
    /// a stop, never does.
    #[test]
    fn lb05_harmless_excursions_calibrate_and_harm_or_a_stop_doesnt() {
        let mut st = Stall::default();
        // Calm at 0.1 s a window, an excursion to 0.7 s for two windows every five minutes, a day.
        let mut t = 0.0;
        while t < DAY_US {
            for _ in 0..148 {
                go(&mut st, 0.1e6, 2e6, false);
            }
            go(&mut st, 0.7e6, 2e6, true);
            go(&mut st, 0.7e6, 2e6, true);
            t += 300e6;
        }
        let h = st.threshold(1.0);
        assert!(h.is_finite() && h > 0.0, "a day of harmless excursions calibrates: {h}");
        let recorded = st.recorded();
        // Harm: never recorded.
        let mut harmed = st.clone();
        go(&mut harmed, 1.2e6, 2e6, true);
        for _ in 0..10 {
            go(&mut harmed, 0.0, 2e6, false);
        }
        assert_eq!(harmed.recorded(), recorded, "an excursion with harm in it was recorded");
        // A stop mid-excursion: never recorded.
        let mut stopped = st.clone();
        go(&mut stopped, 0.8e6, 2e6, true);
        stopped.forget_excursion();
        for _ in 0..10 {
            go(&mut stopped, 0.0, 2e6, false);
        }
        assert_eq!(stopped.recorded(), recorded, "an excursion ended by a stop was recorded");
        // And past the record's excursions, it alarms short of harm.
        let mut rising = st.clone();
        let mut alarmed = false;
        for _ in 0..30 {
            if let Read::Alarm { .. } = go(&mut rising, 0.9e6, 2e6, true) {
                alarmed = true;
                break;
            }
        }
        assert!(alarmed, "a sustained rise past what the record holds never alarmed (threshold {h})");
    }

    /// Calm is the exact mean a window, whatever the spans it was measured over: averaging the
    /// spans' own rates, or their totals, would put calm at 0.3 s here, not 0.06 s.
    #[test]
    fn lb05_calm_is_the_mean_a_window_and_the_trigger_sits_halfway_to_harm() {
        let mut st = Stall::default();
        assert_eq!(st.trigger_us(L), 0.5e6, "nothing measured: halfway from no stall to harm");
        go(&mut st, 0.6e6, 2e6, false);
        go(&mut st, 0.0, 18e6, false);
        assert!((st.calm_us() - 0.06e6).abs() < 1e-6, "{}", st.calm_us());
        assert!((st.trigger_us(L) - 0.53e6).abs() < 1.0, "{}", st.trigger_us(L));
        // Within what the kernel takes.
        let tiny = Limits { window_us: 2e6, harm_us: 0.0 };
        assert!(Stall::default().trigger_us(tiny) >= 1.0);
    }
}
