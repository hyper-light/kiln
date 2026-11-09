//! A lease runner's own hold on its grants (docs/ci.md W05-R37, docs/needs.md E3.5): where each
//! grant's lease ends by the runner's clock, and which grants to stop once it has.
//!
//! The server renews a grant for a lease from when it takes the runner's `Renew` naming it. The
//! runner counts that lease from when it *sent* the `Renew`, since the server's began no earlier,
//! less a tenth: Chubby's conservative local lease. A grant assigned on a connection counts from
//! the last `Renew` answered on it, or from its `Hello`: the server grants on a connection only
//! once it has the hello. A new connection renews nothing by itself, so a grant from before it
//! keeps its end until a `Renew` naming it is answered. A grant whose lease ends unrenewed and that
//! stops on losing it (`on_lease_loss: stop`) is stopped then; one that runs to its end runs on,
//! and what it writes is refused once its grant is gone (W05-R36). Self-fencing narrows the
//! window, but the guarantee rests on fencing (G2): a runner that is itself paused can't stop
//! anything.
//!
//! The clock counts suspend ([`now_ms`]): a machine waking from sleep finds the lease ended,
//! where `Instant`, which leaves sleep out on Linux and macOS, would have it running on.
//!
//! Nothing here looks on a clock (goal 9). A grant is stopped at its own end ([`Leases::stop_due`]),
//! and renewed at its own due time ([`Leases::renew_due`]): two RTOs of its link before the earliest
//! end of the grants it holds, from how long its `Renew`s' answers take (RFC 6298). Each is an
//! [`Alarm`] on the lease clock, which a sleep doesn't put off: it rings on waking past it.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use kiln_ci::sched::Fence;

/// The lease clock, in ms: the host's clock that counts suspend, read through hyper-rt (Linux
/// `CLOCK_BOOTTIME`; macOS the continuous timebase `CLOCK_MONOTONIC_RAW` reads; Windows interrupt
/// time), and never back: no reading is less than one before it in this process.
pub fn now_ms() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let ms = hyper_rt::machine::clock::shard_clock_ns() / 1_000_000;
    LAST.fetch_max(ms, Ordering::Relaxed).max(ms)
}

/// How long a lease of `lease_ms` holds on the runner's side: less a tenth.
fn held_for(lease_ms: u64) -> u64 {
    lease_ms.saturating_sub(lease_ms / 10)
}

/// A grant held: its fence, whether it stops when its lease ends, and where that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Held {
    fence: Fence,
    stop: bool,
    /// By the lease clock.
    end: u64,
    /// Stopped for its lease already: it's said once.
    stopped: bool,
}

/// A runner's leases (W05-R37).
#[derive(Debug, Default)]
pub struct Leases {
    /// `Renew`s sent and not yet answered: each one's number, when it was sent and the grants it
    /// named.
    sent: BTreeMap<u64, (u64, Vec<(String, Fence)>)>,
    /// What a grant assigned on this connection counts from: the send time of the last `Renew`
    /// answered on it, or of its `Hello`. None while away.
    base: Option<u64>,
    held: HashMap<String, Held>,
    /// The newest epoch seen: a command from an older one is ignored.
    epoch: u64,
    /// How long its link takes to answer a `Renew`, from each one's send to its `Renewed` (RFC
    /// 6298; every `Renew` has a number of its own, so each answer is a sample, Karn's rule kept).
    rto: crate::harness::told::Rto,
    /// The lease the server last said, ms (its grants' and its answers').
    lease_ms: u64,
}

/// At most this many `Renew`s wait for their answers: a connection's come in order, or not at all.
const UNANSWERED: usize = 64;

impl Leases {
    /// Connected, its `Hello` sent at `at`.
    pub fn connected(&mut self, at: u64) {
        self.sent.clear();
        self.base = Some(at);
    }

    /// The connection lost: what was sent on it unanswered won't be answered.
    pub fn away(&mut self) {
        self.sent.clear();
        self.base = None;
    }

    /// `Renew` number `seq`, sent at `at`, naming `held`. One sent while the last is unanswered
    /// past its RTO says that answer is overdue: the next RTO is twice as long (RFC 6298 §5.5).
    pub fn renew_sent(&mut self, seq: u64, at: u64, held: Vec<(String, Fence)>) {
        if self.sent.last_key_value().is_some_and(|(_, (last, _))| at >= last.saturating_add(self.rto.ms())) {
            self.rto.expired();
        }
        self.sent.insert(seq, (at, held));
        while self.sent.len() > UNANSWERED {
            self.sent.pop_first();
        }
    }

    /// The answer to `Renew` number `seq` (`Renewed`), come at `now`: each grant it named that's
    /// held still under that fence lives `lease_ms` from when the `Renew` was sent, less a tenth;
    /// one it says `lost` ends now. How long it took is a sample of the link's RTO. An answer from
    /// an epoch older than the newest seen changes nothing.
    pub fn renewed(&mut self, seq: u64, lease_ms: u64, epoch: u64, lost: &[(String, Fence)], now: u64) {
        if self.stale(epoch) {
            return;
        }
        let Some((at, named)) = self.sent.remove(&seq) else { return };
        self.rto.sample(now.saturating_sub(at));
        if lease_ms > 0 {
            self.lease_ms = lease_ms;
        }
        if let Some(b) = self.base.as_mut() {
            *b = (*b).max(at);
        }
        let end = at.saturating_add(held_for(lease_ms));
        for (leg, fence) in named {
            let gone = lost.iter().any(|(l, f)| *l == leg && *f == fence);
            if let Some(h) = self.held.get_mut(&leg).filter(|h| h.fence == fence) {
                h.end = if gone { 0 } else { h.end.max(end) };
            }
        }
        for (leg, fence) in lost {
            if let Some(h) = self.held.get_mut(leg).filter(|h| h.fence == *fence) {
                h.end = 0;
            }
        }
    }

    /// Grant `fence` of `leg` taken on this connection (its `Assign`), its lease `lease_ms`:
    /// it lasts that long, less a tenth, from this connection's base, and it's stopped once that
    /// ends if `stop`. Away (no base), it's taken as ended.
    pub fn granted(&mut self, leg: &str, fence: Fence, lease_ms: u64, stop: bool) {
        if lease_ms > 0 {
            self.lease_ms = lease_ms;
        }
        let end = self.base.map_or(0, |b| b.saturating_add(held_for(lease_ms)));
        self.held.insert(leg.to_owned(), Held { fence, stop, end, stopped: false });
    }

    /// `leg`'s grant over (its job ended, or the leg's next replaced it): it isn't stopped.
    pub fn ended(&mut self, leg: &str) {
        self.held.remove(leg);
    }

    /// The grants to stop at `now` (the lease clock's): those that stop on losing their lease,
    /// whose lease has ended. Each once, by leg.
    pub fn due(&mut self, now: u64) -> Vec<String> {
        let mut due: Vec<String> = self
            .held
            .iter_mut()
            .filter(|(_, h)| h.stop && !h.stopped && h.end <= now)
            .map(|(leg, h)| {
                h.stopped = true;
                leg.clone()
            })
            .collect();
        due.sort();
        due
    }

    /// Whether `epoch` is older than the newest seen: what comes from it is ignored. A newer one
    /// becomes the newest.
    pub fn stale(&mut self, epoch: u64) -> bool {
        if epoch < self.epoch {
            return true;
        }
        self.epoch = epoch;
        false
    }

    /// Where `leg`'s grant's lease ends by the lease clock, while it's held.
    pub fn end(&self, leg: &str) -> Option<u64> {
        self.held.get(leg).map(|h| h.end)
    }

    /// When the next grant is to be stopped for its lease, by the lease clock: the earliest end of
    /// those that stop on losing it and aren't stopped yet ([`Leases::due`] then names it). None
    /// while none is held.
    pub fn stop_due(&self) -> Option<u64> {
        self.held.values().filter(|h| h.stop && !h.stopped).map(|h| h.end).min()
    }

    /// When the next `Renew` is due by the lease clock (W05-R37), on a connection, while the runner
    /// holds grants (`holding`: a leg running, or a result the server hasn't said it has): two RTOs
    /// before the earliest end of what it holds, one for the answer (RFC 6298 §2: RTO = SRTT +
    /// max(G, 4·RTTVAR) of its `Renew`s' answers, at least 1 s, §2.4) and one for a second `Renew`
    /// should that answer be overdue (§5.4, its RTO then doubled, §5.5); and never before the one
    /// unanswered is overdue. A result's grant, no longer here, ends where the last answered
    /// `Renew` left every grant it named. A lease of L renews every 0.9·L − 2·RTO: a 60 s lease on a
    /// link answering in under 250 ms every 52 s, where it was every 10 s ("three times a lease, at
    /// most every 10 s"). None away, or holding nothing: the hello's `Renew` goes as it's welcomed.
    pub fn renew_due(&self, holding: bool) -> Option<u64> {
        let base = self.base.filter(|_| holding)?;
        let rto = self.rto.ms();
        let results = (self.lease_ms > 0).then(|| base.saturating_add(held_for(self.lease_ms)));
        let end = self.held.values().filter(|h| h.end > 0).map(|h| h.end).chain(results).min()?;
        let due = end.saturating_sub(rto.saturating_mul(2));
        Some(self.sent.last_key_value().map_or(due, |(_, (at, _))| due.max(at.saturating_add(rto))))
    }

    /// The link's RTO now, ms (for its page and tests).
    pub fn rto_ms(&self) -> u64 {
        self.rto.ms()
    }
}

/// A due time on the lease clock, which counts suspend (W05-R37): a task of its own waits for it
/// and rings it, armed again only when it moves, so a loop that asks for it every turn costs a
/// compare. A sleep doesn't put it off: on Linux it's a `CLOCK_BOOTTIME` timerfd, on macOS a
/// kqueue timer on the continuous clock (`NOTE_MACH_CONTINUOUS_TIME`), each ringing on waking past
/// it; elsewhere the runtime's timer, whose deadline counts suspend as its shard's clock does.
pub struct Alarm {
    /// The time asked, ms by the lease clock; `u64::MAX` for none.
    at: AtomicU64,
    moved: crate::rt::Notify,
    rang: crate::rt::Notify,
    /// Times it rang (tests count an idle runner's).
    rings: AtomicU64,
}

impl Alarm {
    /// An alarm with nothing asked, its task ending with `stop`.
    pub fn start(stop: crate::rt::Shutdown) -> std::sync::Arc<Alarm> {
        let a = std::sync::Arc::new(Alarm { at: AtomicU64::new(u64::MAX), moved: crate::rt::Notify::new(), rang: crate::rt::Notify::new(), rings: AtomicU64::new(0) });
        let me = std::sync::Arc::clone(&a);
        crate::rt::spawn(async move {
            use futures::FutureExt as _;
            loop {
                let at = me.at.load(Ordering::SeqCst);
                let wait = async {
                    if at == u64::MAX {
                        std::future::pending::<()>().await;
                    }
                    until(at).await;
                };
                futures::select_biased! {
                    () = stop.cancelled().fuse() => return,
                    () = me.moved.notified().fuse() => continue,
                    () = wait.fuse() => {}
                }
                if me.at.load(Ordering::SeqCst) == at {
                    me.rings.fetch_add(1, Ordering::Relaxed);
                    me.rang.notify();
                }
                // Rung once for this time: the next wait is for another.
                futures::select_biased! {
                    () = stop.cancelled().fuse() => return,
                    () = me.moved.notified().fuse() => {}
                }
            }
        });
        a
    }

    /// The time to ring at (none: never), by the lease clock; the same as asked already changes
    /// nothing.
    pub fn set(&self, at: Option<u64>) {
        let v = at.unwrap_or(u64::MAX);
        if self.at.swap(v, Ordering::SeqCst) != v {
            self.moved.notify();
        }
    }

    /// It rang for the time last asked.
    pub async fn rung(&self) {
        self.rang.notified().await;
    }

    /// Times it rang so far.
    pub fn rings(&self) -> u64 {
        self.rings.load(Ordering::Relaxed)
    }
}

/// Waits until the lease clock reads `at` ms, a sleep counted: never sooner, as a stop due finds
/// what it's due for (a wait the runtime's clock ended a hair early waits the rest).
async fn until(at: u64) {
    while now_ms() < at {
        if !until_os(at).await {
            let left = std::time::Duration::from_millis(at.saturating_sub(now_ms()).max(1));
            crate::rt::sleep_until(kiln_seams::clock::later(std::time::Instant::now(), left)).await;
        }
    }
}

/// Linux: a timerfd on `CLOCK_BOOTTIME`, the lease clock's own, at `at` absolute: it turns
/// readable at once on waking past it. False when it can't be had.
#[cfg(target_os = "linux")]
async fn until_os(at: u64) -> bool {
    use nix::sys::time::TimeSpec;
    use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let Ok(t) = TimerFd::new(ClockId::CLOCK_BOOTTIME, TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC) else { return false };
    let ns = at.saturating_mul(1_000_000);
    let when = TimeSpec::new(i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX), i64::try_from(ns % 1_000_000_000).unwrap_or(0));
    if t.set(Expiration::OneShot(when), TimerSetTimeFlags::TFD_TIMER_ABSTIME).is_err() {
        return false;
    }
    let raw = t.as_fd().as_raw_fd();
    matches!(crate::rt::native(hyper_rt::readiness::readable(raw)).await, Ok(Ok(())))
}

/// macOS: a kqueue timer on the continuous clock, which counts sleep (`NOTE_MACH_CONTINUOUS_TIME`),
/// `at` less now from now: it fires at once on waking past it. False when it can't be had.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // kqueue(2)'s timer on the continuous clock: no safe wrapper takes NOTE_MACH_CONTINUOUS_TIME
async fn until_os(at: u64) -> bool {
    use nix::libc;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    // SAFETY: kqueue() takes nothing and returns a descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return false;
    }
    // SAFETY: a descriptor kqueue() just returned, owned here alone.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let ns = at.saturating_sub(now_ms()).saturating_mul(1_000_000);
    // The change made and let go before the wait: a kevent holds a pointer, which no wait may.
    let added = {
        // SAFETY: an all-zero kevent is a valid value of the C struct.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = 1;
        ev.filter = libc::EVFILT_TIMER;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_NSECONDS | libc::NOTE_MACH_CONTINUOUS_TIME;
        ev.data = isize::try_from(ns).unwrap_or(isize::MAX);
        // SAFETY: one change on a live kqueue, no events asked back, no timeout.
        let changed = unsafe { libc::kevent(kq.as_raw_fd(), &raw const ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        changed >= 0
    };
    if !added {
        return false;
    }
    matches!(crate::rt::native(hyper_rt::readiness::readable(kq.as_raw_fd())).await, Ok(Ok(())))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn until_os(_at: u64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: u64 = 60_000;
    const S: u64 = 1000;

    fn fence(n: u64) -> Fence {
        Fence { epoch: 1, n }
    }

    /// The legs due at each second from `from` to `to` (in s), with the second each first was.
    fn checks(l: &mut Leases, from: u64, to: u64) -> Vec<(u64, String)> {
        (from..=to).flat_map(|t| l.due(t * S).into_iter().map(move |leg| (t, leg))).collect()
    }

    /// W05-R37: a lease of 60 s, the last `Renew` answered the one sent at t=20 s, those after it
    /// unanswered (the link gone): the stop grant is stopped at its lease end, 20 + 54 = 74 s, at a
    /// check each second, so by 75 s; never before. The finish grant on the same runner runs on.
    #[test]
    fn w05_r37_a_stop_grant_stops_at_its_lease_end() {
        let mut l = Leases::default();
        l.connected(0);
        l.granted("stop", fence(1), LEASE, true);
        l.granted("finish", fence(2), LEASE, false);
        let held = || vec![("stop".to_owned(), fence(1)), ("finish".to_owned(), fence(2))];
        l.renew_sent(1, 10 * S, held());
        l.renewed(1, LEASE, 1, &[], 10 * S + 100);
        l.renew_sent(2, 20 * S, held());
        l.renewed(2, LEASE, 1, &[], 20 * S + 100);
        for (seq, t) in [(3, 30), (4, 40), (5, 50), (6, 60), (7, 70)] {
            l.renew_sent(seq, t * S, held());
        }
        assert_eq!(l.end("stop"), Some(74 * S));
        assert_eq!(checks(&mut l, 21, 90), vec![(74, "stop".to_owned())], "stopped at 74 s, once; the finish grant never");
        assert!(checks(&mut l, 74, 300).is_empty());
    }

    /// W05-R37: the answer comes 20 s after its `Renew` was sent: the lease counts from the send,
    /// so it ends at 0 + 54 s, not at 20 + 54 s.
    #[test]
    fn w05_r37_the_lease_counts_from_the_renews_send_time() {
        let mut l = Leases::default();
        l.connected(0);
        l.granted("stop", fence(1), LEASE, true);
        l.renew_sent(1, 0, vec![("stop".to_owned(), fence(1))]);
        assert!(l.due(19 * S).is_empty());
        // The answer, at 20 s.
        l.renewed(1, LEASE, 1, &[], 20 * S);
        assert_eq!(l.end("stop"), Some(54 * S));
        assert!(l.due(54 * S - 1).is_empty());
        assert_eq!(l.due(54 * S), vec!["stop".to_owned()]);
    }

    /// W05-R37, the example of docs/needs.md E3.5: a laptop runner holding a stop grant (lease
    /// 60 s) sleeps at t=20 s for 10 minutes. The lease clock counts the sleep, so its first check
    /// on waking, within a second, reads past the lease end and stops the grant; by `Instant`,
    /// which leaves sleep out, the check would have read 21 s and the grant would have run on.
    #[test]
    fn w05_r37_a_sleep_ends_the_lease() {
        let mut l = Leases::default();
        l.connected(0);
        l.granted("stop", fence(1), LEASE, true);
        l.renew_sent(1, 0, vec![("stop".to_owned(), fence(1))]);
        l.renewed(1, LEASE, 1, &[], 100);
        l.renew_sent(2, 10 * S, vec![("stop".to_owned(), fence(1))]);
        l.renewed(2, LEASE, 1, &[], 10 * S + 100);
        assert!(l.due(20 * S).is_empty());
        // Asleep from 20 s to 620 s: the next check, a second after waking, is at 621 s.
        assert_eq!(l.due(621 * S), vec!["stop".to_owned()]);
        // What `Instant` would have read then: 21 s, the grant running on.
        let mut by_instant = Leases::default();
        by_instant.connected(0);
        by_instant.granted("stop", fence(1), LEASE, true);
        by_instant.renew_sent(2, 10 * S, vec![("stop".to_owned(), fence(1))]);
        by_instant.renewed(2, LEASE, 1, &[], 10 * S + 100);
        assert!(by_instant.due(21 * S).is_empty());
    }

    /// W05-R37: a grant the server's answer says isn't the runner's any more ends at once; one
    /// assigned on a connection counts from its last answered `Renew` (or its `Hello`), and a
    /// reconnect's hello doesn't renew a grant from before it.
    #[test]
    fn w05_r37_a_lost_grant_ends_at_once_and_a_hello_renews_nothing_older() {
        let mut l = Leases::default();
        l.connected(0);
        l.granted("a", fence(1), LEASE, true);
        l.renew_sent(1, 20 * S, vec![("a".to_owned(), fence(1))]);
        l.renewed(1, LEASE, 1, &[], 20 * S + 100);
        assert_eq!(l.end("a"), Some(74 * S));
        // Away at 25 s, back at 50 s: a's lease isn't renewed by the hello.
        l.away();
        l.connected(50 * S);
        assert_eq!(l.end("a"), Some(74 * S));
        // A grant on the new connection counts from its hello.
        l.granted("b", fence(2), LEASE, true);
        assert_eq!(l.end("b"), Some(104 * S));
        // An answer about a's renewal after the hello moves the base, and says b is lost.
        l.renew_sent(2, 60 * S, vec![("a".to_owned(), fence(1)), ("b".to_owned(), fence(2))]);
        l.renewed(2, LEASE, 1, &[("b".to_owned(), fence(2))], 60 * S + 100);
        assert_eq!((l.end("a"), l.end("b")), (Some(114 * S), Some(0)));
        assert_eq!(l.due(61 * S), vec!["b".to_owned()]);
        l.granted("c", fence(3), LEASE, false);
        assert_eq!(l.end("c"), Some(114 * S), "the connection's base: the last answered Renew's send");
        // An answer naming a grant the leg has moved past changes nothing.
        l.granted("a", fence(4), LEASE, true);
        l.renew_sent(3, 70 * S, vec![("a".to_owned(), fence(1))]);
        l.renewed(3, LEASE, 1, &[], 70 * S + 100);
        assert_eq!(l.end("a"), Some(114 * S));
    }

    /// W05-R37: the runner ignores what comes from an epoch older than the newest it has seen,
    /// an answer about leases included.
    #[test]
    fn w05_r37_an_older_epochs_word_is_ignored() {
        let mut l = Leases::default();
        assert!(!l.stale(3) && l.stale(2) && !l.stale(3) && !l.stale(4) && l.stale(3));
        l.connected(0);
        l.granted("a", fence(1), LEASE, true);
        l.renew_sent(1, 20 * S, vec![("a".to_owned(), fence(1))]);
        l.renewed(1, LEASE, 3, &[], 20 * S + 100);
        assert_eq!(l.end("a"), Some(54 * S), "an answer from epoch 3, older than 4, renews nothing");
    }

    /// W05-R37: the lease clock counts suspend (on Linux it's `CLOCK_BOOTTIME`, which runs at least
    /// as far as `CLOCK_MONOTONIC`, the time asleep besides) and never goes back.
    #[test]
    fn w05_r37_the_lease_clock_counts_suspend_and_never_goes_back() {
        let mut last = now_ms();
        for _ in 0..10_000 {
            let t = now_ms();
            assert!(t >= last, "{t} after {last}");
            last = t;
        }
        #[cfg(target_os = "linux")]
        {
            let ms = |c| nix::time::clock_gettime(c).map(|t| u64::try_from(t.tv_sec()).unwrap_or(0) * 1000 + u64::try_from(t.tv_nsec()).unwrap_or(0) / 1_000_000);
            let (boot, mono) = (ms(nix::time::ClockId::CLOCK_BOOTTIME), ms(nix::time::ClockId::CLOCK_MONOTONIC));
            assert!(boot.is_ok_and(|b| b.abs_diff(now_ms()) < 1000), "the boot clock: {boot:?}, the lease clock: {}", now_ms());
            assert!(boot.ok() >= mono.ok());
        }
    }

    /// W05-R37: a `Renew` is due two RTOs before the earliest end of what the runner holds, and
    /// only while it holds something on a connection. No answer measured yet: RTO is RFC 6298's
    /// first, 1 s, so a 60 s lease's grant (its end 54 s) is renewed at 52 s, not every 10 s.
    #[test]
    fn w05_r37_a_renew_is_due_two_rtos_before_the_earliest_end() {
        let mut l = Leases::default();
        assert_eq!(l.renew_due(true), None, "away: the hello's Renew goes as it's welcomed");
        l.connected(0);
        assert_eq!(l.renew_due(false), None, "holding nothing, nothing to renew");
        l.granted("a", fence(1), LEASE, true);
        assert_eq!(l.renew_due(true), Some(52 * S));
        // Answered in 100 ms: SRTT 100 ms, RTTVAR 50 ms, RTO max(1 s, 300 ms) = 1 s (§2.4).
        l.renew_sent(1, 52 * S, vec![("a".to_owned(), fence(1))]);
        assert_eq!(l.renew_due(true), Some(53 * S), "not before the one unanswered is overdue");
        l.renewed(1, LEASE, 1, &[], 52 * S + 100);
        assert_eq!(l.end("a"), Some(106 * S));
        assert_eq!(l.renew_due(true), Some(104 * S));
        // A later grant, assigned on this connection, counts from the last answered Renew's send.
        l.granted("b", fence(2), LEASE, false);
        assert_eq!((l.end("b"), l.renew_due(true)), (Some(106 * S), Some(104 * S)));
        // Its job ended, its result not yet received: still held, its end the last Renew's.
        l.ended("a");
        l.ended("b");
        assert_eq!(l.renew_due(true), Some(104 * S));
        assert_eq!(l.renew_due(false), None);
    }

    /// W05-R37: slow answers make the margin wider, by the link's own measure: answers taking 3 s
    /// (SRTT 3 s, RTTVAR 1.5 s: RTO 9 s) renew 18 s before the end. One overdue doubles it.
    #[test]
    fn w05_r37_the_margin_is_the_links_measured_rto() {
        let mut l = Leases::default();
        l.connected(0);
        l.granted("a", fence(1), LEASE, false);
        l.renew_sent(1, 0, vec![("a".to_owned(), fence(1))]);
        l.renewed(1, LEASE, 1, &[], 3 * S);
        assert_eq!(l.rto_ms(), 9 * S);
        assert_eq!(l.end("a"), Some(54 * S));
        assert_eq!(l.renew_due(true), Some(36 * S));
        // Sent at 36 s, unanswered: the next waits for it to be overdue, at 45 s; sent then, the
        // RTO doubles (§5.5) to 18 s.
        l.renew_sent(2, 36 * S, vec![("a".to_owned(), fence(1))]);
        assert_eq!(l.renew_due(true), Some(45 * S));
        l.renew_sent(3, 45 * S, vec![("a".to_owned(), fence(1))]);
        assert_eq!(l.rto_ms(), 18 * S);
        // An answer resets the backoff (§5.7's new sample).
        l.renewed(3, LEASE, 1, &[], 46 * S);
        assert!(l.rto_ms() < 18 * S, "{}", l.rto_ms());
    }

    /// W05-R37: the next stop is the earliest end of the grants that stop on losing their lease and
    /// aren't stopped yet: not a grant that runs on, not one stopped already.
    #[test]
    fn w05_r37_the_next_stop_is_the_earliest_stop_grants_end() {
        let mut l = Leases::default();
        assert_eq!(l.stop_due(), None);
        l.connected(0);
        l.granted("finish", fence(1), LEASE / 2, false);
        assert_eq!(l.stop_due(), None, "a grant that runs on to its end is never stopped");
        l.granted("stop", fence(2), LEASE, true);
        assert_eq!(l.stop_due(), Some(54 * S));
        assert_eq!(l.due(54 * S), vec!["stop".to_owned()]);
        assert_eq!(l.stop_due(), None, "stopped once: due no more");
    }

    /// W05-R37: an alarm rings at its time on the lease clock, once, and not before; asked again
    /// for the same time it doesn't ring again, and with nothing asked it never wakes.
    #[test]
    fn w05_r37_an_alarm_rings_once_at_its_time() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let stop = crate::rt::Shutdown::new();
            let a = Alarm::start(stop.clone());
            let asked = now_ms() + 200;
            a.set(Some(asked));
            let t = std::time::Instant::now();
            crate::rt::timeout(std::time::Duration::from_secs(30), a.rung()).await.map_err(|_| anyhow::anyhow!("never rang"))?;
            anyhow::ensure!(now_ms() >= asked && t.elapsed() >= std::time::Duration::from_millis(150), "rang early: {:?}", t.elapsed());
            a.set(Some(asked));
            a.set(None);
            crate::rt::sleep(std::time::Duration::from_millis(400)).await;
            anyhow::ensure!(a.rings() == 1, "{} rings", a.rings());
            // A time already past rings at once.
            a.set(Some(now_ms().saturating_sub(1)));
            crate::rt::timeout(std::time::Duration::from_secs(30), a.rung()).await.map_err(|_| anyhow::anyhow!("a past time never rang"))?;
            stop.cancel();
            Ok(())
        })
    }
}
