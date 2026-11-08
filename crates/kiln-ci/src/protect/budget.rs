//! How fast protection must act (LB05 step 1, docs/protect.md "The reaction budget"): from the
//! moment a service's requests start to miss, kiln has its deadline less their calm to stop the
//! harm, and its reaction takes four parts, each measured: sensing (until a watcher sees it),
//! deciding, acting (until what kiln stopped stops) and the drain of what kiln already issued (its
//! dirty pages still to be written, its requests already queued). What doesn't fit can't be fixed by
//! reacting, only by not starting what would cause it. Every time is µs.

use serde::{Deserialize, Serialize};

/// The longest window a PSI trigger takes (psi.c, v6.12: `WINDOW_MAX_US`, "Max window size is 10s").
pub const PSI_WINDOW_MAX_US: f64 = 10_000_000.0;
/// What an unprivileged watcher's window must be a whole number of (psi.c, v6.12: "Unprivileged users
/// can only use 2s windows so that averages aggregation work is used"; since Linux 6.4).
pub const PSI_UNPRIVILEGED_STEP_US: f64 = 2_000_000.0;
/// How often the kernel looks at an unprivileged trigger: the averages work's period, `PSI_FREQ`,
/// "(2*HZ+1)", 2 s and a tick (psi.c, v6.12).
pub const PSI_AVERAGES_PERIOD_US: f64 = 2_000_000.0;
/// How many times a window a privileged trigger is looked at (psi.c, v6.12: `UPDATES_PER_WINDOW`).
pub const PSI_UPDATES_PER_WINDOW: f64 = 10.0;
/// The longest a kernel tick is: Linux's coarsest timer frequency, 100 Hz (kernel/Kconfig.hz, v6.12,
/// offers 100, 250, 300 and 1000). A bound where the running kernel's own isn't known.
pub const TICK_MAX_US: f64 = 10_000.0;

/// What a reaction takes, each part as measured (µs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    /// From trouble beginning until a watcher sees it.
    pub sense_us: f64,
    /// Deciding.
    pub decide_us: f64,
    /// Until what was stopped stops.
    pub act_us: f64,
    /// What was already issued, draining at the rate it drains ([`drain_us`]).
    pub drain_us: f64,
}

impl Reaction {
    pub fn total_us(&self) -> f64 {
        [self.sense_us, self.decide_us, self.act_us, self.drain_us].iter().map(|t| t.max(0.0)).sum()
    }

    /// What's left of a `deadline_us` for requests whose calm is `calm_us` once the reaction has
    /// run: below zero, reacting can't keep the deadline, only not starting what would break it.
    pub fn slack_us(&self, deadline_us: f64, calm_us: f64) -> f64 {
        deadline_us - calm_us.max(0.0) - self.total_us()
    }

    /// The parts, in words, for the reasons decisions give.
    pub fn said(&self) -> String {
        let s = |us: f64| format!("{:.2} s", us.max(0.0) / 1e6);
        format!("sensing {}, deciding {}, acting {}, draining {}", s(self.sense_us), s(self.decide_us), s(self.act_us), s(self.drain_us))
    }
}

/// How long what's already queued takes to drain (µs): what's queued over the rate it drains at.
/// Little's law (L = λW) says the wait behind a queue of L is L/λ; a sync waits behind every dirty
/// page of its file system (2026-10-26: `ovl_sync_fs`, docs/headroom.md). None while nothing drains.
pub fn drain_us(queued: f64, rate_per_s: f64) -> Option<f64> {
    (queued.is_finite() && rate_per_s.is_finite() && rate_per_s > 0.0).then(|| queued.max(0.0) / rate_per_s * 1e6)
}

/// The PSI window for a `deadline_us` (µs): the shortest the kernel takes from this watcher that is
/// no shorter than the deadline, so a whole deadline's stall fits in it. A watcher with
/// `CAP_SYS_RESOURCE` (`privileged`) may use the deadline itself; any other a whole number of 2 s.
/// None past the kernel's longest window, or for no deadline.
pub fn psi_window_us(deadline_us: f64, privileged: bool) -> Option<f64> {
    if !deadline_us.is_finite() || deadline_us <= 0.0 {
        return None;
    }
    let w = if privileged { deadline_us.ceil() } else { (deadline_us / PSI_UNPRIVILEGED_STEP_US).ceil() * PSI_UNPRIVILEGED_STEP_US };
    (w <= PSI_WINDOW_MAX_US).then_some(w)
}

/// The longest a PSI trigger can take to tell of a stall once its window holds the threshold (µs):
/// the period the kernel looks at it, the averages work's for an unprivileged watcher (and its
/// tick), a tenth of the window for a privileged one.
pub fn psi_sense_us(window_us: f64, privileged: bool) -> f64 {
    if privileged { window_us / PSI_UPDATES_PER_WINDOW } else { PSI_AVERAGES_PERIOD_US + TICK_MAX_US }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rounding the window down would leave a deadline's stall no room in it; rounding a privileged
    /// watcher's to 2 s would slow it for nothing.
    #[test]
    fn lb05_the_psi_window_is_the_shortest_the_kernel_takes_that_holds_a_deadline() {
        assert_eq!(psi_window_us(1e6, false), Some(2e6));
        assert_eq!(psi_window_us(2e6, false), Some(2e6));
        assert_eq!(psi_window_us(2.5e6, false), Some(4e6));
        assert_eq!(psi_window_us(9.9e6, false), Some(10e6));
        assert_eq!(psi_window_us(1e6, true), Some(1e6));
        assert_eq!(psi_window_us(10.5e6, true), None);
        assert_eq!(psi_window_us(10.5e6, false), None);
        assert_eq!(psi_window_us(0.0, false), None);
        assert_eq!(psi_window_us(f64::NAN, true), None);
    }

    #[test]
    fn lb05_sensing_is_the_kernels_period_for_the_watcher() {
        // Unprivileged: the averages work's 2 s, whatever the window.
        assert_eq!(psi_sense_us(2e6, false), psi_sense_us(10e6, false));
        assert!((psi_sense_us(4e6, false) - 2.01e6).abs() < 1.0);
        // Privileged: ten looks a window.
        assert!((psi_sense_us(1e6, true) - 1e5).abs() < 1e-9);
    }

    #[test]
    fn lb05_draining_is_whats_queued_over_its_rate_and_the_reaction_adds_up() {
        assert_eq!(drain_us(1e9, 1e8), Some(10e6));
        assert_eq!(drain_us(1e9, 0.0), None);
        assert_eq!(drain_us(-5.0, 1e8), Some(0.0));
        let r = Reaction { sense_us: 2.01e6, decide_us: 50.0, act_us: 0.2e6, drain_us: 3e6 };
        assert!((r.total_us() - 5.21005e6).abs() < 1e-3);
        // A 1 s deadline with a 0.1 s calm can't be kept by a 5.2 s reaction.
        assert!(r.slack_us(1e6, 1e5) < 0.0);
        let quick = Reaction { sense_us: 0.1e6, decide_us: 0.0, act_us: 0.2e6, drain_us: 0.1e6 };
        assert!((quick.slack_us(1e6, 1e5) - 0.5e6).abs() < 1e-6);
        assert!(r.said().contains("draining 3.00 s"), "{}", r.said());
    }
}
