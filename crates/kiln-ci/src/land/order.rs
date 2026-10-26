//! Order and admission inside the land queue (docs/ci.md W28-R35 to R37), the pure part: which of
//! the pooled queue's waiting work runs first, how many candidates each component tests at once,
//! and when admission defers a speculative one. The replayer
//! ([`crate::sched::sim::replay::land`]) carries out what it says, as the server's queue will.
//!
//! - One pooled queue, over every runner that can do the work, holds every component's work side
//!   by side. Within a component the critical path's units come first, then the shortest expected
//!   service; W05-R09's aging lifts what waits, so once a unit has waited [`Order::aging_bound`]
//!   nothing that arrives after passes it (R35).
//! - Each component keeps an AIMD speculation window (R36): one more candidate on a pass, halved on
//!   a failure, as Zuul's window does, never past its cap nor below one.
//! - Admission holds the bottleneck's committed utilization below a cap by deferring speculative
//!   candidates, shortest expected service first when it frees: a component's head always starts,
//!   and so does a candidate deferred for the aging bound (R37).

use std::fmt;

/// What a candidate is for (W28-R37). A component's head is the next change to land, so its check
/// is needed now; a speculative candidate is tested on its expected stack before the changes ahead
/// of it have passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Intent {
    /// The component's head: nothing in the component lands before it.
    Head,
    /// A candidate behind the head, on its expected stack.
    Speculative,
}

/// The order and admission policy's knobs (W28-R35 to R37), with their defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Order {
    /// W05-R09: effective priority gained per minute of waiting (default 1).
    pub aging_per_minute: f64,
    /// The most candidates one component tests at once, the head's included (default 8).
    pub window_cap: u32,
    /// The share of the bottleneck's capacity committed at which speculative candidates defer
    /// (default 0.8).
    pub util_cap: f64,
}

/// Priority a unit on its component's critical path has over one that isn't (W28-R35).
pub const CRITICAL: i64 = 30;
/// Priority an instant unit has over one of [`SJF_SPAN`] or longer: shortest expected service
/// first, linearly between.
pub const SJF_RANGE: i64 = 30;
/// Expected service at or past this long scores no shortest-first priority (seconds).
pub const SJF_SPAN: i64 = 1800;

impl Default for Order {
    fn default() -> Order {
        Order { aging_per_minute: 1.0, window_cap: 8, util_cap: 0.8 }
    }
}

/// A unit of work waiting in the pooled queue, as the order sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting {
    /// It lies on its component's critical path.
    pub critical: bool,
    /// Its expected service: the median of its job's runs (seconds).
    pub service_s: i64,
    /// How long it has waited (seconds).
    pub waited_s: i64,
}

impl Order {
    /// A unit's effective priority (W28-R35): [`CRITICAL`] on the critical path, up to
    /// [`SJF_RANGE`] for the shortest expected service, plus W05-R09's aging, `aging_per_minute`
    /// a minute waited.
    pub fn score(&self, w: &Waiting) -> i64 {
        let sjf = SJF_RANGE - (w.service_s.max(0).saturating_mul(SJF_RANGE) / SJF_SPAN).min(SJF_RANGE);
        let aged = (self.aging_per_minute.max(0.0) * w.waited_s.max(0) as f64 / 60.0).floor() as i64;
        let critical = if w.critical { CRITICAL } else { 0 };
        critical.saturating_add(sjf).saturating_add(aged)
    }

    /// How long a unit can wait while work that arrived after it goes first (W28-R35): the whole
    /// range of base priority, aged away (seconds). A unit that has waited this long scores at
    /// least what any newcomer can, and ties go to the longer wait, so nothing that arrives from
    /// then on passes it.
    pub fn aging_bound(&self) -> i64 {
        if self.aging_per_minute <= 0.0 {
            return i64::MAX;
        }
        ((CRITICAL + SJF_RANGE) as f64 * 60.0 / self.aging_per_minute).ceil() as i64
    }

    /// The order the pooled queue takes `waiting` in (W28-R35): highest score first, then the
    /// longest wait, then the order given (places into `waiting`).
    pub fn rank(&self, waiting: &[Waiting]) -> Vec<usize> {
        let mut by: Vec<(i64, i64, usize)> = waiting.iter().enumerate().map(|(i, w)| (self.score(w), w.waited_s, i)).collect();
        by.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        by.into_iter().map(|(_, _, i)| i).collect()
    }

    /// Whether a candidate for `intent` may start while the bottleneck its check needs has `util`
    /// of its capacity committed, having been deferred `waited_s` (W28-R37): the head always, one
    /// deferred for the aging bound always, a speculative one only below the cap.
    pub fn admit(&self, intent: Intent, util: f64, waited_s: i64) -> Admission {
        if intent == Intent::Head {
            Admission::Head
        } else if waited_s >= self.aging_bound() {
            Admission::Aged { waited_s, bound_s: self.aging_bound() }
        } else if util < self.util_cap {
            Admission::Below { util, cap: self.util_cap }
        } else {
            Admission::Defer { util, cap: self.util_cap }
        }
    }
}

/// What admission decided for one candidate, and why (W28-R37).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Admission {
    /// A component's head, never deferred.
    Head,
    /// A speculative candidate deferred for the aging bound.
    Aged { waited_s: i64, bound_s: i64 },
    /// A speculative candidate while the bottleneck is below the cap.
    Below { util: f64, cap: f64 },
    /// A speculative candidate deferred: the bottleneck its check needs is at or past the cap.
    Defer { util: f64, cap: f64 },
}

impl Admission {
    /// Whether the candidate may start now.
    pub fn runs(&self) -> bool {
        !matches!(self, Admission::Defer { .. })
    }
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

impl fmt::Display for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Admission::Head => write!(f, "starts: the component's head, never deferred"),
            Admission::Aged { waited_s, bound_s } => write!(f, "starts: deferred {} min, the aging bound of {} min", waited_s / 60, bound_s / 60),
            Admission::Below { util, cap } => write!(f, "starts: speculative, and the bottleneck its check needs is at {} of its capacity, below the {} cap", pct(*util), pct(*cap)),
            Admission::Defer { util, cap } => write!(f, "deferred: speculative, and the bottleneck its check needs is at {} of its capacity, at or past the {} cap", pct(*util), pct(*cap)),
        }
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::Head => write!(f, "the component's head"),
            Intent::Speculative => write!(f, "a speculative candidate"),
        }
    }
}

/// One component's AIMD speculation window (W28-R36): how many of its candidates are tested at
/// once, the head's included. It starts open to its cap, as Zuul's starts at its size; a pass
/// admits one more and a failure halves it, as Zuul's window does; it never passes its cap nor
/// falls below one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    n: u32,
    cap: u32,
    /// The widest it has been.
    pub max: u32,
}

impl Window {
    pub fn new(cap: u32) -> Window {
        let cap = cap.max(1);
        Window { n: cap, cap, max: cap }
    }

    /// A candidate passed: one more may be tested.
    pub fn pass(&mut self) {
        self.n = self.n.saturating_add(1).min(self.cap);
        self.max = self.max.max(self.n);
    }

    /// A candidate failed: the window halves, never below one.
    pub fn fail(&mut self) {
        self.n = (self.n / 2).max(1);
    }

    /// How many candidates the window tests at once.
    pub fn open(&self) -> u32 {
        self.n
    }

    pub fn cap(&self) -> u32 {
        self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(critical: bool, service_s: i64, waited_s: i64) -> Waiting {
        Waiting { critical, service_s, waited_s }
    }

    /// W28-R35: the critical path's units first, then the shortest expected service; aging lifts
    /// what waits.
    #[test]
    fn w28_r35_critical_path_then_shortest_then_aged() {
        let o = Order::default();
        let waiting = [w(false, 600, 0), w(false, 60, 0), w(true, 1200, 0), w(false, 300, 0), w(false, 60, 30)];
        assert_eq!(o.rank(&waiting), vec![2, 4, 1, 3, 0], "critical first, then shortest, the longer wait breaking the tie");
        // A long unit that has waited outranks fresh short work.
        assert!(o.score(&w(false, SJF_SPAN, 31 * 60)) > o.score(&w(false, 0, 0)));
    }

    /// W28-R35: once a unit has waited the aging bound, nothing that arrives from then on outranks
    /// it, whatever either's place on the critical path or expected service.
    #[test]
    fn w28_r35_nothing_newer_passes_a_unit_past_the_aging_bound() {
        for aging in [0.5, 0.7, 1.0, 2.0, 3.0] {
            let o = Order { aging_per_minute: aging, ..Order::default() };
            let bound = o.aging_bound();
            for (crit, service) in [(false, 0), (false, 600), (false, SJF_SPAN), (false, 10 * SJF_SPAN), (true, 0), (true, SJF_SPAN)] {
                for later in [0, 1, 59, 600, 3599, 86_400] {
                    let old = w(crit, service, bound + later);
                    for (crit2, service2) in [(false, 0), (true, 0), (true, 1), (false, 30)] {
                        let new = w(crit2, service2, later);
                        assert_eq!(o.rank(&[new, old]).first(), Some(&1), "aging {aging}: waited {} s ({crit}, {service} s) against ({crit2}, {service2} s, waited {later} s)", bound + later);
                    }
                }
            }
        }
        assert_eq!(Order::default().aging_bound(), 3600, "(30 + 30) points at one a minute");
        assert_eq!(Order { aging_per_minute: 0.0, ..Order::default() }.aging_bound(), i64::MAX, "no aging, no bound");
    }

    /// W28-R36: the window starts at its cap, halves on a failure, opens by one on a pass, and
    /// never passes its cap or falls below one.
    #[test]
    fn w28_r36_aimd_window() {
        let mut win = Window::new(5);
        assert_eq!((win.open(), win.cap(), win.max), (5, 5, 5));
        win.fail();
        assert_eq!(win.open(), 2);
        win.fail();
        win.fail();
        assert_eq!(win.open(), 1, "never below one");
        for _ in 0..10 {
            win.pass();
            assert!(win.open() <= win.cap());
        }
        assert_eq!((win.open(), win.max), (5, 5), "capped");
        assert_eq!(Window::new(0).open(), 1, "a cap of none is one");
    }

    /// W28-R37: a speculative candidate defers at the cap, a head never does, and one deferred for
    /// the aging bound starts; each decision says why.
    #[test]
    fn w28_r37_admission_defers_speculative_candidates_at_the_cap() {
        let o = Order::default();
        assert!((o.util_cap - 0.8).abs() < f64::EPSILON, "the cap defaults to 0.8");
        assert!(o.admit(Intent::Speculative, 0.79, 0).runs());
        let d = o.admit(Intent::Speculative, 0.8, 0);
        assert!(!d.runs());
        assert_eq!(d.to_string(), "deferred: speculative, and the bottleneck its check needs is at 80% of its capacity, at or past the 80% cap");
        assert_eq!(o.admit(Intent::Head, 1.0, 0), Admission::Head);
        assert_eq!(o.admit(Intent::Speculative, 1.0, 3600).to_string(), "starts: deferred 60 min, the aging bound of 60 min");
        assert_eq!(o.admit(Intent::Speculative, 0.5, 0).to_string(), "starts: speculative, and the bottleneck its check needs is at 50% of its capacity, below the 80% cap");
    }
}
