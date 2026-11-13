//! Finite aggregate CPU credit for the existing native watcher. The shell supplies the same
//! Allocation's rate, the kernel hardware ceiling, measured response and exact watcher CPU.
//! No period, sampler, admission row or device ownership is introduced here.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Failure {
    Envelope,
    Arithmetic,
    Transition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Prepared,
    Running,
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    Prepared,
    Run(Option<Duration>),
    Hold(Duration),
}

/// Enforcement credit, in milli-CPU nanoseconds. Positive credit is bounded; debt is never
/// forgiven by another observation, a late wake, or a change to learned reaction measurements.
#[derive(Debug)]
pub(super) struct Budget {
    rate: i128,
    maximum: i128,
    burst: i128,
    credit: i128,
    mode: Mode,
}

/// The same native watcher's monotonic credit clock. Whole-helper CPU is conservatively charged
/// to each active scope, including RPC and waiter work; shared work is never silently free.
#[derive(Debug)]
pub(super) struct Control {
    budget: Budget,
    clock: Option<(Instant, Duration)>,
    baseline: Duration,
}
impl Control {
    pub(super) fn new(
        rate: u64,
        maximum: u64,
        response: Duration,
        baseline: Duration,
    ) -> Result<Self, Failure> {
        Ok(Self {
            budget: Budget::new(rate, maximum, response)?,
            clock: None,
            baseline,
        })
    }
    pub(super) fn start(&mut self, now: Instant, helper_cpu: Duration) -> Result<(), Failure> {
        self.budget.start()?;
        // No uncharged startup interval: the first scope uses a zero baseline; siblings capture
        // the actual counter before their preparation. Previously charged shared work is not reset.
        self.budget.observe(
            Duration::ZERO,
            helper_cpu
                .checked_sub(self.baseline)
                .ok_or(Failure::Transition)?,
        )?;
        self.clock = Some((now, helper_cpu));
        Ok(())
    }
    pub(super) fn observe(
        &mut self,
        now: Instant,
        helper_cpu: Duration,
    ) -> Result<Decision, Failure> {
        if let Some((before, cpu)) = self.clock {
            self.budget.observe(
                now.checked_duration_since(before)
                    .ok_or(Failure::Transition)?,
                helper_cpu.checked_sub(cpu).ok_or(Failure::Transition)?,
            )?;
            self.clock = Some((now, helper_cpu));
        }
        self.budget.decision()
    }
    pub(super) fn held(&mut self) -> Result<(), Failure> {
        self.budget.held()
    }

    /// Its burst, the most its scope runs past a stop, in milli-CPU nanoseconds (2c's tests).
    #[cfg(test)]
    pub(super) fn burst(&self) -> i128 {
        self.budget.burst
    }

    /// A hold's measured response (joint plan step 14, 2c): [`Budget::learn`].
    pub(super) fn learn(&mut self, response: Duration) -> Result<(), Failure> {
        self.budget.learn(response)
    }

    /// When the scope's credit ran out (joint plan step 14, 2c; ROOT, 2026-11-13), off its last observation: its
    /// credit's overrun there, spent at the rate it runs past its earning (its maximum less its rate), rounded up so a
    /// response measured from it is never short. None with credit left, held, before its first observation, or at a
    /// rate it is never held at.
    pub(super) fn ran_out(&self) -> Option<Instant> {
        let (at, _) = self.clock?;
        at.checked_sub(self.budget.overrun()?)
    }
    pub(super) fn permit(&mut self) -> Result<(), Failure> {
        match self.budget.mode {
            Mode::Prepared => Err(Failure::Transition),
            Mode::Held => self.budget.resume(),
            Mode::Running => Ok(()),
        }
    }

    /// Joint plan step 14 (C4 on macOS): the scope's rate becomes `rate`, its water-filled share. What it used up to
    /// `now` is charged at the rate it had first, so no interval is charged at the other; then the new rate's decision.
    pub(super) fn set_rate(
        &mut self,
        now: Instant,
        helper_cpu: Duration,
        rate: u64,
    ) -> Result<Decision, Failure> {
        self.observe(now, helper_cpu)?;
        self.budget.set_rate(rate)?;
        self.budget.decision()
    }
}

/// AC-T7.4.1: the cycle this control holds a scope that would use every core to, at `rate` milli-CPU of
/// `maximum` with `response`: how long it runs from full credit, then how long its hold lasts until it may
/// run again (burst/(maximum−rate), then 2·burst/rate, burst = maximum×response), taken from the budget's
/// own decisions. A scope at the maximum is never held: no cycle.
pub(super) fn cycle(
    rate: u64,
    maximum: u64,
    response: Duration,
) -> Result<(Duration, Duration), Failure> {
    let mut budget = Budget::new(rate, maximum, response)?;
    budget.start()?;
    let Decision::Run(Some(run)) = budget.decision()? else {
        return Err(Failure::Envelope);
    };
    budget.observe(run, Duration::ZERO)?;
    budget.held()?;
    let Decision::Hold(hold) = budget.decision()? else {
        return Err(Failure::Transition);
    };
    Ok((run, hold))
}

fn nanos(duration: Duration) -> Result<i128, Failure> {
    i128::try_from(duration.as_nanos()).map_err(|_| Failure::Arithmetic)
}
fn product(rate: i128, duration: Duration) -> Result<i128, Failure> {
    rate.checked_mul(nanos(duration)?)
        .ok_or(Failure::Arithmetic)
}
fn until(amount: i128, rate: i128) -> Result<Duration, Failure> {
    if amount <= 0 || rate <= 0 {
        return Err(Failure::Arithmetic);
    }
    let quotient = amount.checked_div(rate).ok_or(Failure::Arithmetic)?;
    let remainder = amount.checked_rem(rate).ok_or(Failure::Arithmetic)?;
    let ceil = if remainder == 0 {
        quotient
    } else {
        quotient.checked_add(1).ok_or(Failure::Arithmetic)?
    };
    let ns = u64::try_from(ceil).map_err(|_| Failure::Arithmetic)?;
    Ok(Duration::from_nanos(ns))
}

impl Budget {
    pub(super) fn new(rate: u64, maximum: u64, response: Duration) -> Result<Self, Failure> {
        if rate == 0 || maximum == 0 || rate > maximum || response.is_zero() {
            return Err(Failure::Envelope);
        }
        let rate = i128::from(rate);
        let maximum = i128::from(maximum);
        let burst = product(maximum, response)?;
        Ok(Self {
            rate,
            maximum,
            burst,
            credit: burst,
            mode: Mode::Prepared,
        })
    }

    /// A hold's measured response (joint plan step 14, 2c; ROOT, 2026-11-13): the control's is the slowest seen, only
    /// rising, and its burst, the most a scope runs past its stop, the maximum over it. A hold under way paid the burst
    /// before it, so what its scope ran past its stop beyond that is owed too: its credit tops up by the difference.
    /// Refused for a response of no time, as [`Budget::new`] refuses one.
    pub(super) fn learn(&mut self, response: Duration) -> Result<(), Failure> {
        if response.is_zero() {
            return Err(Failure::Envelope);
        }
        let burst = product(self.maximum, response)?;
        if burst <= self.burst {
            return Ok(());
        }
        if self.mode == Mode::Held {
            let owed = burst.checked_sub(self.burst).ok_or(Failure::Arithmetic)?;
            self.credit = self.credit.checked_sub(owed).ok_or(Failure::Arithmetic)?;
        }
        self.burst = burst;
        Ok(())
    }

    /// How long before its last observation its credit ran out: its overrun, spent at the rate it runs past its earning,
    /// rounded up; none with credit left, held or prepared, or at its maximum, where it is never held.
    fn overrun(&self) -> Option<Duration> {
        if self.mode != Mode::Running || self.credit > 0 {
            return None;
        }
        let depletion = self.maximum.checked_sub(self.rate).filter(|d| *d > 0)?;
        let over = self.credit.checked_neg()?;
        let ns = over
            .checked_add(depletion.checked_sub(1)?)?
            .checked_div(depletion)?;
        Some(Duration::from_nanos(u64::try_from(ns).ok()?))
    }

    /// A new rate for the same scope (joint plan step 14, C4 on macOS): its credit and any debt stay, and its burst,
    /// which its maximum and response make; only what it earns from now changes. Refused outside 1 to its maximum, as
    /// [`Budget::new`] refuses.
    pub(super) fn set_rate(&mut self, rate: u64) -> Result<(), Failure> {
        let rate = i128::from(rate);
        if rate == 0 || rate > self.maximum {
            return Err(Failure::Envelope);
        }
        self.rate = rate;
        Ok(())
    }

    /// Called before the first native CONT, not at a later first watcher observation.
    pub(super) fn start(&mut self) -> Result<(), Failure> {
        if self.mode != Mode::Prepared {
            return Err(Failure::Transition);
        }
        self.mode = Mode::Running;
        Ok(())
    }

    /// Running scope consumption is conservatively bounded by all hardware CPUs, covering short
    /// helpers even if they fork and are reaped between snapshots. The observer's own measured
    /// thread CPU is also debited; conservative overlap cannot create extra credit.
    pub(super) fn observe(
        &mut self,
        elapsed: Duration,
        watcher_cpu: Duration,
    ) -> Result<(), Failure> {
        if self.mode == Mode::Prepared {
            return Ok(());
        }
        let granted = product(self.rate, elapsed)?;
        let used = if self.mode == Mode::Running {
            product(self.maximum, elapsed)?
        } else {
            0
        };
        // One CPU is 1000 milli-CPU, the same unit as Allocation.resources.cpu_milli.
        let observer = product(1000, watcher_cpu)?;
        self.credit = self
            .credit
            .checked_add(granted)
            .and_then(|credit| credit.checked_sub(used))
            .and_then(|credit| credit.checked_sub(observer))
            .ok_or(Failure::Arithmetic)?
            .min(self.burst);
        Ok(())
    }

    /// Called after the existing scope hold has been issued successfully. The response allowance
    /// is paid once for this stop transition; accepted delivery alone is not immediate cessation.
    pub(super) fn held(&mut self) -> Result<(), Failure> {
        match self.mode {
            Mode::Prepared => return Err(Failure::Transition),
            Mode::Held => return Ok(()),
            Mode::Running => {}
        }
        self.credit = self
            .credit
            .checked_sub(self.burst)
            .ok_or(Failure::Arithmetic)?;
        self.mode = Mode::Held;
        Ok(())
    }

    /// Only full earned credit permits another pulse. Called before CONT/probing a held member.
    pub(super) fn resume(&mut self) -> Result<(), Failure> {
        if self.mode != Mode::Held || self.credit < self.burst {
            return Err(Failure::Transition);
        }
        self.mode = Mode::Running;
        Ok(())
    }

    pub(super) fn decision(&self) -> Result<Decision, Failure> {
        match self.mode {
            Mode::Prepared => Ok(Decision::Prepared),
            Mode::Held if self.credit < self.burst => Ok(Decision::Hold(until(
                self.burst
                    .checked_sub(self.credit)
                    .ok_or(Failure::Arithmetic)?,
                self.rate,
            )?)),
            Mode::Held | Mode::Running if self.credit > 0 => {
                let depletion = self
                    .maximum
                    .checked_sub(self.rate)
                    .ok_or(Failure::Arithmetic)?;
                let wait = if depletion == 0 {
                    None
                } else {
                    Some(until(self.credit, depletion)?)
                };
                Ok(Decision::Run(wait))
            }
            Mode::Running => Ok(Decision::Hold(until(
                self.burst
                    .checked_sub(self.credit)
                    .ok_or(Failure::Arithmetic)?,
                self.rate,
            )?)),
            Mode::Held => Err(Failure::Transition),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: actual helper startup cannot run free before the first VMM pulse. A sibling uses
    /// its measured pre-prepare baseline rather than charging an unrelated historical lifetime.
    #[test]
    fn helper_startup_debt_can_hold_the_initial_native_controller() {
        let at = Instant::now();
        let mut first = Control::new(1000, 4000, Duration::from_nanos(10), Duration::ZERO).unwrap();
        first.start(at, Duration::from_nanos(100)).unwrap();
        assert!(matches!(
            first.observe(at, Duration::from_nanos(100)).unwrap(),
            Decision::Hold(_)
        ));
        first.held().unwrap();
        assert!(first.permit().is_err());
        let mut sibling = Control::new(
            1000,
            4000,
            Duration::from_nanos(10),
            Duration::from_nanos(100),
        )
        .unwrap();
        sibling.start(at, Duration::from_nanos(101)).unwrap();
        assert!(matches!(
            sibling.observe(at, Duration::from_nanos(101)).unwrap(),
            Decision::Run(_)
        ));
        assert!(sibling.observe(at, Duration::from_nanos(100)).is_err());
    }

    /// AC-T7.4.1: a late watcher cannot forgive consumed aggregate CPU or enlarge its response credit.
    #[test]
    fn late_wake_keeps_all_debt_before_another_pulse() {
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        budget.start().unwrap();
        assert_eq!(
            budget.decision().unwrap(),
            Decision::Run(Some(Duration::from_nanos(14)))
        );
        budget
            .observe(Duration::from_nanos(20), Duration::ZERO)
            .unwrap();
        budget.held().unwrap();
        assert!(budget.resume().is_err());
        assert_eq!(
            budget.decision().unwrap(),
            Decision::Hold(Duration::from_nanos(100))
        );
        budget
            .observe(Duration::from_nanos(99), Duration::ZERO)
            .unwrap();
        assert!(budget.resume().is_err());
        budget
            .observe(Duration::from_nanos(1), Duration::ZERO)
            .unwrap();
        budget.resume().unwrap();
    }

    /// AC-T7.4.1: the watcher consumes the same admitted credit while the VM members are held.
    #[test]
    fn observer_cpu_is_not_free_or_an_independent_allowance() {
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        budget.start().unwrap();
        budget.held().unwrap();
        budget
            .observe(Duration::from_nanos(40), Duration::from_nanos(10))
            .unwrap();
        assert_eq!(
            budget.decision().unwrap(),
            Decision::Hold(Duration::from_nanos(10))
        );
        assert!(budget.resume().is_err());
        budget
            .observe(Duration::from_nanos(10), Duration::ZERO)
            .unwrap();
        budget.resume().unwrap();
    }

    /// AC-T7.4.1: idle time cannot bank an unbounded burst, and invalid control envelopes refuse.
    #[test]
    fn response_credit_is_fixed_and_invalid_rates_do_not_start() {
        assert!(Budget::new(0, 1000, Duration::from_nanos(1)).is_err());
        assert!(Budget::new(1001, 1000, Duration::from_nanos(1)).is_err());
        assert!(Budget::new(1000, 1000, Duration::ZERO).is_err());
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        assert_eq!(budget.decision().unwrap(), Decision::Prepared);
        budget.start().unwrap();
        assert!(budget.start().is_err());
        budget.held().unwrap();
        budget
            .observe(Duration::from_secs(3600), Duration::ZERO)
            .unwrap();
        budget.resume().unwrap();
        assert_eq!(
            budget.decision().unwrap(),
            Decision::Run(Some(Duration::from_nanos(14)))
        );
    }

    /// AC-T7.4.1 (guest_cost's macOS window): the cycle of a scope that would use every core, taken from the
    /// budget's own decisions: expect it to run burst/(maximum−rate) from full credit, rounded up, and its
    /// hold to pay that run's overshoot and refill a whole burst at the rate (2·burst/rate and the
    /// overshoot's), after which it may run again; a scope at the maximum is never held, so has none.
    #[test]
    fn the_cycle_is_the_budgets_run_then_its_hold() {
        let (run, hold) = cycle(1000, 4000, Duration::from_nanos(10)).unwrap();
        assert_eq!(
            (run, hold),
            (Duration::from_nanos(14), Duration::from_nanos(82))
        );
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        budget.start().unwrap();
        budget.observe(run, Duration::ZERO).unwrap();
        budget.held().unwrap();
        budget
            .observe(hold - Duration::from_nanos(1), Duration::ZERO)
            .unwrap();
        assert!(budget.resume().is_err());
        budget
            .observe(Duration::from_nanos(1), Duration::ZERO)
            .unwrap();
        budget.resume().unwrap();
        assert!(cycle(4000, 4000, Duration::from_nanos(10)).is_err());
    }

    /// Joint plan step 14 (2c; ROOT, 2026-11-13): the control's response is learned from its holds, the slowest seen,
    /// only rising (decision 9: no census row seeds it), and its burst, the most a scope runs past its stop, with it. Do
    /// make a scope's budget at 1 CPU of 4 with a 10 ns response, then learn a hold's 20 ns response, then a 5 ns one,
    /// then one of no time. Expect the burst 4 CPUs over 20 ns once the slower is learned, unchanged by the faster, and
    /// the response of no time refused, the burst as it was.
    #[test]
    fn the_control_learns_its_response_from_its_holds_the_slowest_seen() {
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        assert_eq!(budget.burst, 4000 * 10);
        budget.learn(Duration::from_nanos(20)).unwrap();
        assert_eq!(budget.burst, 4000 * 20);
        budget.learn(Duration::from_nanos(5)).unwrap();
        assert_eq!(budget.burst, 4000 * 20, "only rising");
        assert_eq!(budget.learn(Duration::ZERO), Err(Failure::Envelope));
        assert_eq!(budget.burst, 4000 * 20);
    }

    /// Joint plan step 14 (2c; ROOT, 2026-11-13): a hold's response, measured once the hold is issued, tops up that
    /// hold's own allowance where it is slower than the one it paid, so what a scope ran past its stop is charged; a
    /// faster one changes nothing. Do run a scope at 1 CPU of 4 with a 10 ns response until it is held, then learn that
    /// its stop took 30 ns, then 20 ns. Expect its debt 4 CPUs over 20 ns more than the hold paid, and no more.
    #[test]
    fn a_held_scopes_slower_stop_tops_up_its_hold() {
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        budget.start().unwrap();
        budget
            .observe(Duration::from_nanos(14), Duration::ZERO)
            .unwrap();
        budget.held().unwrap();
        let paid = budget.credit;
        budget.learn(Duration::from_nanos(30)).unwrap();
        assert_eq!(budget.credit, paid - 4000 * 20);
        budget.learn(Duration::from_nanos(20)).unwrap();
        assert_eq!(budget.credit, paid - 4000 * 20);
    }

    /// Joint plan step 14 (2c): a hold's response is from when its scope's credit ran out, which the control reads off
    /// its own last observation, whatever woke its watcher: its credit's overrun at the rate it is spent past its earning
    /// (the maximum less the rate), rounded up so a response is never short. Do run a scope at 1 CPU of 4 with a 10 ns
    /// response from full credit, then observe it 14 ns on. Expect its credit (40,000, spent at 3,000 a ns) to have run
    /// out 13.33 ns after its start, so 13 ns rounded up from the observation's 0.67 ns overrun; and none before, with
    /// credit left.
    #[test]
    fn a_scopes_credit_ran_out_when_its_overrun_says() {
        let at = Instant::now();
        let mut control =
            Control::new(1000, 4000, Duration::from_nanos(10), Duration::ZERO).unwrap();
        control.start(at, Duration::ZERO).unwrap();
        assert_eq!(control.ran_out(), None, "credit left");
        let later = at + Duration::from_nanos(14);
        assert!(matches!(
            control.observe(later, Duration::ZERO).unwrap(),
            Decision::Hold(_)
        ));
        let ran_out = control.ran_out().unwrap();
        assert_eq!(
            ran_out,
            later - Duration::from_nanos(1),
            "{:?} after the start",
            ran_out.saturating_duration_since(at)
        );
    }

    /// Joint plan step 14 (AC-T7.4.1), C4 on macOS: a scope's rate becomes its water-filled share. Do run a scope at 1
    /// CPU of 4 until it is held, then raise its rate to 2 CPUs, and lower another's to 0.5; then ask for 0 and for more
    /// than the maximum. Expect the debt kept across the change and its hold the new rate's (the debt refilled twice as
    /// fast, and half as fast), and the two rates refused, the rate as it was.
    #[test]
    fn a_rate_change_keeps_the_debt_and_refills_at_the_new_rate() {
        let mut budget = Budget::new(1000, 4000, Duration::from_nanos(10)).unwrap();
        budget.start().unwrap();
        budget
            .observe(Duration::from_nanos(14), Duration::ZERO)
            .unwrap();
        budget.held().unwrap();
        let Decision::Hold(at_one) = budget.decision().unwrap() else {
            panic!("not held")
        };
        let owed = budget.credit;
        budget.set_rate(2000).unwrap();
        assert_eq!(budget.credit, owed, "no debt forgiven");
        let Decision::Hold(at_two) = budget.decision().unwrap() else {
            panic!("not held")
        };
        assert_eq!(at_two, at_one.div_f64(2.0).max(Duration::from_nanos(1)));
        budget.set_rate(500).unwrap();
        let Decision::Hold(at_half) = budget.decision().unwrap() else {
            panic!("not held")
        };
        assert_eq!(at_half, at_one * 2);
        assert_eq!(budget.set_rate(0), Err(Failure::Envelope));
        assert_eq!(budget.set_rate(4001), Err(Failure::Envelope));
        assert_eq!(budget.decision().unwrap(), Decision::Hold(at_half));
    }

    /// Joint plan step 14 (AC-T7.4.1), C4 on macOS: a control's rate change charges what came before at the rate it
    /// had. Do run two controls at 1 CPU of 4 for 4 ns, then change one to 3 CPUs and observe both 4 ns later. Expect
    /// the changed one's credit the same as the other's up to the change, then its earning at 3 CPUs.
    #[test]
    fn a_rate_change_charges_what_came_before_at_the_old_rate() {
        let at = Instant::now();
        let mut kept = Control::new(1000, 4000, Duration::from_nanos(10), Duration::ZERO).unwrap();
        let mut raised =
            Control::new(1000, 4000, Duration::from_nanos(10), Duration::ZERO).unwrap();
        for control in [&mut kept, &mut raised] {
            control.start(at, Duration::ZERO).unwrap();
            control
                .observe(at + Duration::from_nanos(4), Duration::ZERO)
                .unwrap();
        }
        let before = raised.budget.credit;
        raised
            .set_rate(at + Duration::from_nanos(4), Duration::ZERO, 3000)
            .unwrap();
        assert_eq!(
            raised.budget.credit, before,
            "nothing charged at the new rate"
        );
        kept.observe(at + Duration::from_nanos(8), Duration::ZERO)
            .unwrap();
        raised
            .observe(at + Duration::from_nanos(8), Duration::ZERO)
            .unwrap();
        // Running uses all four CPUs: at 1 CPU it loses 3 per ns, at 3 CPUs 1 per ns.
        assert_eq!(kept.budget.credit, before - 4 * 3000);
        assert_eq!(raised.budget.credit, before - 4 * 1000);
    }

    /// AC-T7.4.1: two long-lived members plus a short helper consume one aggregate allocation.
    /// A late wake cannot reset their debt; held-time helper work delays every member's next pulse.
    #[test]
    fn aggregate_members_and_short_helpers_cannot_spend_independent_credit() {
        let at = Instant::now();
        let mut control =
            Control::new(1000, 4000, Duration::from_nanos(10), Duration::ZERO).unwrap();
        control.start(at, Duration::ZERO).unwrap();
        let members_cpu_ns: i128 = [20i128, 20, 10].into_iter().sum();
        let before = control.budget.credit;
        assert!(matches!(
            control
                .observe(at + Duration::from_nanos(20), Duration::ZERO)
                .unwrap(),
            Decision::Hold(_)
        ));
        let charged = before + 20 * 1000 - control.budget.credit;
        assert!(
            charged >= members_cpu_ns * 1000,
            "short helpers must not disappear from aggregate credit"
        );
        control.held().unwrap();
        assert!(control.permit().is_err());
        // A stale notification causes another observation, not a fresh budget/clock/identity.
        assert!(matches!(
            control
                .observe(at + Duration::from_nanos(20), Duration::ZERO)
                .unwrap(),
            Decision::Hold(_)
        ));
        assert!(matches!(
            control
                .observe(at + Duration::from_nanos(120), Duration::from_nanos(1))
                .unwrap(),
            Decision::Hold(_)
        ));
        assert!(control.permit().is_err());
        assert!(matches!(
            control
                .observe(at + Duration::from_nanos(121), Duration::from_nanos(1))
                .unwrap(),
            Decision::Run(_)
        ));
        control.permit().unwrap();
    }
}
