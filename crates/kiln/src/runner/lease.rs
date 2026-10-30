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

    /// `Renew` number `seq`, sent at `at`, naming `held`.
    pub fn renew_sent(&mut self, seq: u64, at: u64, held: Vec<(String, Fence)>) {
        self.sent.insert(seq, (at, held));
        while self.sent.len() > UNANSWERED {
            self.sent.pop_first();
        }
    }

    /// The answer to `Renew` number `seq` (`Renewed`): each grant it named that's held still under
    /// that fence lives `lease_ms` from when the `Renew` was sent, less a tenth; one it says `lost`
    /// ends now. An answer from an epoch older than the newest seen changes nothing.
    pub fn renewed(&mut self, seq: u64, lease_ms: u64, epoch: u64, lost: &[(String, Fence)]) {
        if self.stale(epoch) {
            return;
        }
        let Some((at, named)) = self.sent.remove(&seq) else { return };
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
        l.renewed(1, LEASE, 1, &[]);
        l.renew_sent(2, 20 * S, held());
        l.renewed(2, LEASE, 1, &[]);
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
        l.renewed(1, LEASE, 1, &[]);
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
        l.renewed(1, LEASE, 1, &[]);
        l.renew_sent(2, 10 * S, vec![("stop".to_owned(), fence(1))]);
        l.renewed(2, LEASE, 1, &[]);
        assert!(l.due(20 * S).is_empty());
        // Asleep from 20 s to 620 s: the next check, a second after waking, is at 621 s.
        assert_eq!(l.due(621 * S), vec!["stop".to_owned()]);
        // What `Instant` would have read then: 21 s, the grant running on.
        let mut by_instant = Leases::default();
        by_instant.connected(0);
        by_instant.granted("stop", fence(1), LEASE, true);
        by_instant.renew_sent(2, 10 * S, vec![("stop".to_owned(), fence(1))]);
        by_instant.renewed(2, LEASE, 1, &[]);
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
        l.renewed(1, LEASE, 1, &[]);
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
        l.renewed(2, LEASE, 1, &[("b".to_owned(), fence(2))]);
        assert_eq!((l.end("a"), l.end("b")), (Some(114 * S), Some(0)));
        assert_eq!(l.due(61 * S), vec!["b".to_owned()]);
        l.granted("c", fence(3), LEASE, false);
        assert_eq!(l.end("c"), Some(114 * S), "the connection's base: the last answered Renew's send");
        // An answer naming a grant the leg has moved past changes nothing.
        l.granted("a", fence(4), LEASE, true);
        l.renew_sent(3, 70 * S, vec![("a".to_owned(), fence(1))]);
        l.renewed(3, LEASE, 1, &[]);
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
        l.renewed(1, LEASE, 3, &[]);
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
}
