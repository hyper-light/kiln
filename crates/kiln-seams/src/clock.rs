//! `Clock`: time. What kiln asks of time is three things: a monotonic now (to measure and to set
//! deadlines by), the wall clock (to stamp records), and sleeps. Code that takes them from a
//! `Clock` runs on real time in production (kiln's runtime, `kiln::rt::RtClock`) and on a
//! [`SimClock`] in tests, where time moves only when the test moves it: a test of a 15-minute
//! back-off takes microseconds and asserts every gap exactly.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt as _;
use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::conformance::{Case, Failure, ensure, same};

/// A sleep: completes once its clock's `now()` reaches its deadline. Dropping it cancels it.
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Time, as kiln takes it.
pub trait Clock: Send + Sync + 'static {
    /// Monotonic time: never goes back. What durations and deadlines are measured by.
    fn now(&self) -> Instant;
    /// The wall clock: milliseconds since the Unix epoch, what records are stamped with. A real one
    /// can step (the system's time set); durations come from `now`.
    fn unix_ms(&self) -> i64;
    /// Completes once `now()` reaches `deadline`; at once for a deadline already past.
    fn sleep_until(&self, deadline: Instant) -> Sleep;
    /// Completes once `d` has passed by this clock.
    fn sleep(&self, d: Duration) -> Sleep {
        self.sleep_until(later(self.now(), d))
    }
    /// The wall clock in seconds since the Unix epoch.
    fn unix_s(&self) -> i64 {
        self.unix_ms().div_euclid(1000)
    }
}

/// `at` plus `d`, or as far past `at` as an `Instant` goes (a sleep of `Duration::MAX` is as long
/// as the platform's time can say, not an overflow).
pub fn later(at: Instant, d: Duration) -> Instant {
    let mut d = d;
    for _ in 0..128 {
        if let Some(t) = at.checked_add(d) {
            return t;
        }
        d = d.checked_div(2).unwrap_or_default();
    }
    at
}

/// The system's wall clock, in milliseconds since the Unix epoch (0 before it, `i64::MAX` past
/// what an `i64` holds): what a real `Clock`'s `unix_ms` returns.
pub fn system_unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------------- simulator

/// Where a [`SimClock`] starts by default: 2026-01-01T00:00:00Z.
pub const SIM_EPOCH_MS: i64 = 1_767_225_600_000;

/// Most sleeps a simulated clock holds at once. One past it completes at once (a sleep is never
/// lost): `refusals()` counts them, and a test that sees one has a leak to find.
pub const SIM_SLEEPS_MAX: usize = 1 << 20;

/// A clock whose time moves only when it's moved ([`SimClock::advance`]): sleeps due by then
/// wake, in deadline order (ties in the order they slept). Clones share one time.
#[derive(Clone)]
pub struct SimClock {
    inner: Arc<SimInner>,
}

struct SimInner {
    base: Instant,
    state: Mutex<SimState>,
    /// Told whenever a sleep starts waiting (`sleeping_since`).
    slept: event_listener::Event,
}

#[derive(Default)]
struct SimState {
    offset: Duration,
    unix_base_ms: i64,
    /// Sleeps waiting, by (deadline, the order they started): each with its waker and when it
    /// started waiting (its registration's number).
    sleepers: BTreeMap<(Instant, u64), (Waker, u64)>,
    next: u64,
    registrations: u64,
    refusals: u64,
}

impl SimState {
    fn now(&self, base: Instant) -> Instant {
        later(base, self.offset)
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SimClock {
    /// A simulated clock at [`SIM_EPOCH_MS`].
    pub fn new() -> Self {
        Self::at_unix_ms(SIM_EPOCH_MS)
    }

    /// A simulated clock whose wall clock starts at `unix_ms`.
    pub fn at_unix_ms(unix_ms: i64) -> Self {
        let state = SimState { unix_base_ms: unix_ms, ..SimState::default() };
        Self { inner: Arc::new(SimInner { base: Instant::now(), state: Mutex::new(state), slept: event_listener::Event::new() }) }
    }

    /// How long this clock has run (the sum of every advance).
    pub fn elapsed(&self) -> Duration {
        self.inner.state.lock().offset
    }

    /// Moves time forward by `d`, waking every sleep due by then, earliest first.
    pub fn advance(&self, d: Duration) {
        let due = {
            let mut st = self.inner.state.lock();
            st.offset = st.offset.saturating_add(d);
            let now = st.now(self.inner.base);
            take_due(&mut st, now)
        };
        for w in due {
            w.wake();
        }
    }

    /// Moves time forward to `t` (nothing when `t` isn't ahead).
    pub fn advance_to(&self, t: Instant) {
        let d = t.saturating_duration_since(self.now());
        self.advance(d);
    }

    /// The deadline of the next sleep due, if any is waiting.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.inner.state.lock().sleepers.keys().next().map(|(t, _)| *t)
    }

    /// Moves time to the next sleep's deadline, waking what's due then. Where time went, or `None`
    /// when nothing sleeps.
    pub fn advance_to_next(&self) -> Option<Instant> {
        let t = self.next_deadline()?;
        self.advance_to(t);
        Some(t)
    }

    /// Sleeps waiting now.
    pub fn sleepers(&self) -> usize {
        self.inner.state.lock().sleepers.len()
    }

    /// Sleeps that have started waiting, ever: pass it to [`SimClock::sleeping_since`].
    pub fn registrations(&self) -> u64 {
        self.inner.state.lock().registrations
    }

    /// Sleeps that completed at once because [`SIM_SLEEPS_MAX`] were waiting.
    pub fn refusals(&self) -> u64 {
        self.inner.state.lock().refusals
    }

    /// Completes once at least `n` sleeps that started waiting after `since` (a
    /// [`SimClock::registrations`]) are waiting: the code under test has come round to its next
    /// sleep. No time passes while it waits.
    pub async fn sleeping_since(&self, since: u64, n: usize) {
        loop {
            let listener = self.inner.slept.listen();
            if self.waiting_since(since) >= n {
                return;
            }
            listener.await;
        }
    }

    fn waiting_since(&self, since: u64) -> usize {
        self.inner.state.lock().sleepers.values().filter(|(_, at)| *at > since).count()
    }

    /// Moves time forward by `d`, then waits for the code under test to come round to its next
    /// sleep (one started after the move).
    pub async fn advance_and_settle(&self, d: Duration) {
        let since = self.registrations();
        self.advance(d);
        self.sleeping_since(since, 1).await;
    }
}

fn take_due(st: &mut SimState, now: Instant) -> Vec<Waker> {
    let mut due = Vec::new();
    while let Some(entry) = st.sleepers.first_entry() {
        if entry.key().0 > now {
            break;
        }
        let (w, _) = entry.remove();
        due.push(w);
    }
    due
}

impl Clock for SimClock {
    fn now(&self) -> Instant {
        self.inner.state.lock().now(self.inner.base)
    }

    fn unix_ms(&self) -> i64 {
        let st = self.inner.state.lock();
        st.unix_base_ms.saturating_add(i64::try_from(st.offset.as_millis()).unwrap_or(i64::MAX))
    }

    fn sleep_until(&self, deadline: Instant) -> Sleep {
        Box::pin(SimSleep { clock: Arc::clone(&self.inner), deadline, key: None })
    }
}

struct SimSleep {
    clock: Arc<SimInner>,
    deadline: Instant,
    key: Option<(Instant, u64)>,
}

impl Future for SimSleep {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let registered = {
            let mut st = this.clock.state.lock();
            if st.now(this.clock.base) >= this.deadline {
                if let Some(k) = this.key.take() {
                    st.sleepers.remove(&k);
                }
                return Poll::Ready(());
            }
            match this.key {
                Some(k) => {
                    if let Some((w, _)) = st.sleepers.get_mut(&k)
                        && !w.will_wake(cx.waker())
                    {
                        *w = cx.waker().clone();
                    }
                    false
                }
                None => {
                    if st.sleepers.len() >= SIM_SLEEPS_MAX {
                        st.refusals = st.refusals.saturating_add(1);
                        return Poll::Ready(());
                    }
                    let id = st.next;
                    st.next = st.next.saturating_add(1);
                    st.registrations = st.registrations.saturating_add(1);
                    let at = st.registrations;
                    let k = (this.deadline, id);
                    st.sleepers.insert(k, (cx.waker().clone(), at));
                    this.key = Some(k);
                    true
                }
            }
        };
        if registered {
            this.clock.slept.notify(usize::MAX);
        }
        Poll::Pending
    }
}

impl Drop for SimSleep {
    fn drop(&mut self) {
        if let Some(k) = self.key.take() {
            self.clock.state.lock().sleepers.remove(&k);
        }
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a clock's conformance suite runs against.
pub trait ClockFixture: Send + Sync {
    fn clock(&self) -> Arc<dyn Clock>;
    /// Lets `d` pass by the clock: a simulated one is advanced, a real one waited out.
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()>;
    /// A span the clock can let pass quickly and still tell apart from four times it (a real
    /// clock: tens of milliseconds; a simulated one: any).
    fn quantum(&self) -> Duration;
}

/// The fixture for a [`SimClock`].
pub struct SimClockFixture(pub SimClock);

impl ClockFixture for SimClockFixture {
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.0.clone())
    }
    fn pass(&self, d: Duration) -> BoxFuture<'_, ()> {
        self.0.advance(d);
        Box::pin(async {})
    }
    fn quantum(&self) -> Duration {
        Duration::from_millis(250)
    }
}

/// Whether `s` has completed, polled once now.
fn done(s: &mut Sleep) -> bool {
    s.as_mut().now_or_never().is_some()
}

fn times(q: Duration, n: u32) -> Duration {
    q.checked_mul(n).unwrap_or(Duration::MAX)
}

fn c1<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let mut last = c.now();
        for _ in 0..1000 {
            let n = c.now();
            ensure(n >= last, || "now() went back".into())?;
            last = n;
        }
        let q = f.quantum();
        f.pass(q).await;
        ensure(c.now().saturating_duration_since(last) >= q, || "now() didn't move by the time that passed".into())
    })
}

fn c2<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let before = c.unix_ms();
        // 2020-01-01T00:00:00Z: a wall clock before it isn't one.
        ensure(before > 1_577_836_800_000, || format!("unix_ms() is {before}, before 2020"))?;
        let q = f.quantum();
        f.pass(times(q, 2)).await;
        let moved = c.unix_ms().saturating_sub(before);
        let q_ms = i64::try_from(q.as_millis()).unwrap_or(i64::MAX);
        ensure(moved >= q_ms, || format!("unix_ms() moved {moved} ms while {} ms passed", q_ms.saturating_mul(2)))?;
        same("unix_s()", c.unix_s(), c.unix_ms().div_euclid(1000))
    })
}

fn c3<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let mut now = c.sleep_until(c.now());
        ensure(done(&mut now), || "a sleep until now didn't complete at once".into())?;
        let past = c.now();
        f.pass(f.quantum()).await;
        let mut back = c.sleep_until(past);
        ensure(done(&mut back), || "a sleep until a deadline already past didn't complete at once".into())?;
        let mut zero = c.sleep(Duration::ZERO);
        ensure(done(&mut zero), || "sleep(0) didn't complete at once".into())
    })
}

fn c4<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let q = f.quantum();
        let deadline = later(c.now(), times(q, 8));
        let mut s = c.sleep_until(deadline);
        ensure(!done(&mut s), || "a sleep completed long before its deadline".into())?;
        f.pass(q).await;
        ensure(!done(&mut s), || "a sleep completed before its deadline".into())?;
        f.pass(times(q, 8)).await;
        s.await;
        ensure(c.now() >= deadline, || "a sleep completed while now() was short of its deadline".into())
    })
}

fn c5<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let q = f.quantum();
        let mut s = c.sleep(q);
        f.pass(times(q, 2)).await;
        ensure(done(&mut s), || "a sleep wasn't complete once its deadline had passed".into())
    })
}

fn c6<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let q = f.quantum();
        // Started out of order, deadlines 4q apart: each completes in its turn.
        let mut third = c.sleep(times(q, 9));
        let mut first = c.sleep(q);
        let mut second = c.sleep(times(q, 5));
        f.pass(times(q, 2)).await;
        ensure(done(&mut first), || "the first deadline passed, its sleep isn't complete".into())?;
        ensure(!done(&mut second) && !done(&mut third), || "a later sleep completed with the first".into())?;
        f.pass(times(q, 4)).await;
        ensure(done(&mut second), || "the second deadline passed, its sleep isn't complete".into())?;
        ensure(!done(&mut third), || "the last sleep completed with the second".into())?;
        f.pass(times(q, 4)).await;
        ensure(done(&mut third), || "the last deadline passed, its sleep isn't complete".into())
    })
}

fn c7<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let mut forever = c.sleep(Duration::MAX);
        ensure(!done(&mut forever), || "sleep(Duration::MAX) completed".into())?;
        let mut far = c.sleep_until(later(c.now(), Duration::MAX));
        ensure(!done(&mut far), || "a sleep until the farthest deadline completed".into())
    })
}

fn c8<F: ClockFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = f.clock();
        let q = f.quantum();
        let mut sleeps: Vec<Sleep> = (0..1000u32).map(|i| c.sleep(times(q, 1).checked_add(Duration::from_micros(u64::from(i))).unwrap_or(q))).collect();
        f.pass(times(q, 2)).await;
        let pending = sleeps.iter_mut().map(done).filter(|d| !d).count();
        same("sleeps still waiting once every deadline passed", pending, 0)
    })
}

/// The `Clock` contract, clause by clause (docs/seams.md, "Clock").
pub fn cases<F: ClockFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "CLOCK-1", says: "now() never goes back, and moves by the time that passes", check: c1::<F> },
        Case { id: "CLOCK-2", says: "unix_ms() is the wall clock, moving forward as time passes; unix_s() is it in seconds", check: c2::<F> },
        Case { id: "CLOCK-3", says: "a sleep until now or a deadline already past, or of zero, completes at once", check: c3::<F> },
        Case { id: "CLOCK-4", says: "a sleep never completes before its deadline", check: c4::<F> },
        Case { id: "CLOCK-5", says: "a sleep completes once its deadline has passed", check: c5::<F> },
        Case { id: "CLOCK-6", says: "sleeps complete in deadline order, whatever order they started in", check: c6::<F> },
        Case { id: "CLOCK-7", says: "a sleep of Duration::MAX waits as long as the platform's time can say, without overflowing", check: c7::<F> },
        Case { id: "CLOCK-8", says: "a thousand sleeps at once all complete once their deadlines pass", check: c8::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    #[test]
    fn the_simulated_clock_passes_the_clock_suite() -> Result<(), String> {
        let n = futures::executor::block_on(run("Clock", &cases::<SimClockFixture>(), || async { SimClockFixture(SimClock::new()) })).into_result()?;
        assert_eq!(n, cases::<SimClockFixture>().len());
        Ok(())
    }

    #[test]
    fn time_moves_only_when_moved_and_sleepers_wake_in_deadline_order() {
        let c = SimClock::new();
        let t0 = c.now();
        assert_eq!(c.now(), t0, "no time passes by itself");
        assert_eq!(c.unix_ms(), SIM_EPOCH_MS);
        let mut late = c.sleep(Duration::from_secs(30));
        let mut soon = c.sleep(Duration::from_secs(10));
        assert!(!done(&mut late) && !done(&mut soon));
        assert_eq!(c.sleepers(), 2);
        assert_eq!(c.next_deadline(), Some(later(t0, Duration::from_secs(10))));
        assert_eq!(c.advance_to_next(), Some(later(t0, Duration::from_secs(10))));
        assert!(done(&mut soon) && !done(&mut late));
        assert_eq!(c.elapsed(), Duration::from_secs(10));
        assert_eq!(c.unix_ms(), SIM_EPOCH_MS + 10_000);
        c.advance(Duration::from_secs(20));
        assert!(done(&mut late));
        assert_eq!(c.sleepers(), 0, "completed sleeps are forgotten");
    }

    #[test]
    fn a_dropped_sleep_is_forgotten() {
        let c = SimClock::new();
        let mut s = c.sleep(Duration::from_secs(1));
        assert!(!done(&mut s));
        assert_eq!(c.sleepers(), 1);
        drop(s);
        assert_eq!(c.sleepers(), 0);
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn a_test_waits_for_the_code_under_test_to_come_round_to_its_next_sleep() {
        let c = SimClock::new();
        let since = c.registrations();
        let mut first = c.sleep(Duration::from_secs(5));
        assert!(!done(&mut first));
        // A sleep that started after `since`: it's come round.
        assert!(futures::executor::block_on(async { c.sleeping_since(since, 1).now_or_never() }).is_some());
        let later_on = c.registrations();
        assert!(c.sleeping_since(later_on, 1).now_or_never().is_none(), "nothing has slept since");
    }
}
