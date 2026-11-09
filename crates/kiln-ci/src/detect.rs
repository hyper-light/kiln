//! Failure detection with no heartbeats (LB03, W05; docs/reach.md §4.1). A peer is suspected only
//! when something is owed on its link, by one of four events, never by a probe or a tick:
//!
//! - **Its link dropped.** The relay closes a link when either end's connection to it ends, which
//!   is the transport's own verdict on data gone unanswered: QUIC declares a connection lost once
//!   its probe timeouts (RFC 9002 §6.2) and its idle timeout (RFC 9000 §10.1) pass without an
//!   acknowledgement, as TCP's user timeout does (RFC 5482). ([`Detector::dropped`])
//! - **The transport's own timeout** on unacknowledged data, the same verdict, where a node holds
//!   the connection itself.
//! - **An answer overdue.** A word its peer answers at once arms one deadline, for that one answer,
//!   at RFC 6298's retransmission timeout from the link's measured round trip ([`Rtt`], sampled
//!   from these answers themselves), doubled after each one missed (§5.5) and set again by the next
//!   answer in time (§5.7), at most §2.5's sixty seconds. Nothing else wakes it.
//!   ([`Detector::owe`], [`Detector::answered`], [`Detector::due`])
//! - **A peer's report** that it can't reach it: an in-situ observation of existing traffic, as
//!   Panorama gathers them (Huang et al., "Capturing and Enhancing In Situ System Observability for
//!   Failure Detection", OSDI 2018). ([`Detector::reported`])
//!
//! A link with nothing owed on it is never probed: where nothing is needed, nothing needs
//! detecting. Not used, and why (the federation notes, §3): φ accrual (Hayashibara et al., SRDS
//! 2004) is built around periodic heartbeats, and its window and threshold are picked; Lifeguard
//! (Dadgar et al., 2017) extends SWIM, which probes on a period, with α = 5 and β = 6 picked; Rapid
//! (Suresh et al., ATC 2018) probes each edge, and its K, H, L and 40%-of-10 rule are picked.
//!
//! Its suspicions can be wrong (a slow peer, a frame held up), and what acts on them is safe under
//! that (Chandra and Toueg, "Unreliable Failure Detectors for Reliable Distributed Systems", JACM
//! 43(2), 1996): a wrong one costs a link made again, or a hand-off's claim that wasn't needed.
//!
//! Pure: time comes in, in milliseconds; the one timer it asks for is [`Detector::timer`].

use std::collections::{BTreeMap, BTreeSet};

use crate::place::link::Rtt;
use crate::sched::units::Ms;

/// RFC 6298 §2.5: a maximum may be placed on the timer, at least sixty seconds: that least.
pub const MAX_RTO: Ms = 60_000;

/// Why a peer is suspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its link dropped, as the transport or the relay said (`why`).
    Dropped(String),
    /// The answer `what` didn't come in its timeout; `waited` ms since it was asked.
    Overdue { what: String, waited: Ms },
    /// Node `by` said it can't reach it.
    Reported { by: String },
}

/// A peer suspected, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspicion {
    pub peer: String,
    pub why: Why,
}

impl std::fmt::Display for Suspicion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.why {
            Why::Dropped(why) => write!(f, "{}'s link dropped: {why}", self.peer),
            Why::Overdue { what, waited } => write!(f, "{} didn't answer {what} in {waited} ms", self.peer),
            Why::Reported { by } => write!(f, "{by} can't reach {}", self.peer),
        }
    }
}

#[derive(Debug, Clone)]
struct Owed {
    what: String,
    sent: i64,
    deadline: i64,
}

#[derive(Debug, Clone, Default)]
struct PeerAt {
    rtt: Option<Rtt>,
    /// Timeouts missed in a row since the last answer in time (RFC 6298 §5.5's back-off).
    backoff: u32,
    owed: BTreeMap<u64, Owed>,
}

/// What a node owes and is owed on its links, and what it measured of them.
#[derive(Debug, Clone, Default)]
pub struct Detector {
    peers: BTreeMap<String, PeerAt>,
    due: BTreeSet<(i64, String, u64)>,
    next: u64,
}

impl Detector {
    pub fn new() -> Detector {
        Detector::default()
    }

    /// The timeout of a word sent to `peer` now: RFC 6298's, from its round trip (a second before
    /// any is measured, §2.1), doubled for each one missed in a row, at most [`MAX_RTO`].
    pub fn rto(&self, peer: &str) -> Ms {
        let p = self.peers.get(peer);
        let base = Rtt::rto_of(p.and_then(|p| p.rtt.as_ref()));
        let times = 1u64.checked_shl(p.map_or(0, |p| p.backoff)).unwrap_or(u64::MAX);
        base.saturating_mul(times).min(MAX_RTO)
    }

    /// The round trip measured to `peer`, if one is.
    pub fn rtt(&self, peer: &str) -> Option<Rtt> {
        self.peers.get(peer).and_then(|p| p.rtt)
    }

    /// Something was sent to `peer` that it answers at once (`what` names the answer): one deadline,
    /// for that answer.
    pub fn owe(&mut self, peer: &str, what: &str, now: i64) {
        let deadline = now.saturating_add(i64::try_from(self.rto(peer)).unwrap_or(i64::MAX));
        self.next = self.next.saturating_add(1);
        let id = self.next;
        self.peers.entry(peer.to_owned()).or_default().owed.insert(id, Owed { what: what.to_owned(), sent: now, deadline });
        self.due.insert((deadline, peer.to_owned(), id));
    }

    /// The answer `what` came from `peer`: the oldest such owed is met, its round trip measured
    /// (RFC 6298 §2.2, §2.3) and the back-off ended. The round trip, if one was owed.
    pub fn answered(&mut self, peer: &str, what: &str, now: i64) -> Option<Ms> {
        let p = self.peers.get_mut(peer)?;
        let (id, o) = p.owed.iter().find(|(_, o)| o.what == what).map(|(id, o)| (*id, o.clone()))?;
        p.owed.remove(&id);
        self.due.remove(&(o.deadline, peer.to_owned(), id));
        let r = u64::try_from(now.saturating_sub(o.sent)).unwrap_or(0);
        p.rtt = Some(match p.rtt {
            Some(mut rtt) => {
                rtt.sample(r);
                rtt
            }
            None => Rtt::first(r, 1),
        });
        p.backoff = 0;
        Some(r)
    }

    /// Whether anything is owed by `peer`.
    pub fn owes(&self, peer: &str) -> bool {
        self.peers.get(peer).is_some_and(|p| !p.owed.is_empty())
    }

    /// `peer`'s link dropped (`why`, as the transport or the relay said): nothing it owed on it will
    /// come. Its round trip and back-off stay, for the next link to it.
    pub fn dropped(&mut self, peer: &str, why: &str) -> Suspicion {
        self.clear(peer);
        Suspicion { peer: peer.to_owned(), why: Why::Dropped(why.to_owned()) }
    }

    /// Node `by` says it can't reach `peer`.
    pub fn reported(&self, peer: &str, by: &str) -> Suspicion {
        Suspicion { peer: peer.to_owned(), why: Why::Reported { by: by.to_owned() } }
    }

    /// The answers overdue by `now`, each a suspicion of its peer, whose timer backs off.
    pub fn due(&mut self, now: i64) -> Vec<Suspicion> {
        let mut out = Vec::new();
        while let Some((at, peer, id)) = self.due.first().cloned() {
            if at > now {
                break;
            }
            self.due.remove(&(at, peer.clone(), id));
            let Some(p) = self.peers.get_mut(&peer) else { continue };
            let Some(o) = p.owed.remove(&id) else { continue };
            p.backoff = p.backoff.saturating_add(1);
            out.push(Suspicion { peer, why: Why::Overdue { what: o.what, waited: u64::try_from(now.saturating_sub(o.sent)).unwrap_or(0) } });
        }
        out
    }

    /// When the next answer is due: the one timer to set, if any is owed.
    pub fn timer(&self) -> Option<i64> {
        self.due.first().map(|(at, _, _)| *at)
    }

    /// `peer` is gone for good: nothing of it is kept.
    pub fn forget(&mut self, peer: &str) {
        self.clear(peer);
        self.peers.remove(peer);
    }

    fn clear(&mut self, peer: &str) {
        let Some(p) = self.peers.get_mut(peer) else { return };
        for (id, o) in std::mem::take(&mut p.owed) {
            self.due.remove(&(o.deadline, peer.to_owned(), id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LB03: an answer in time measures the link (RFC 6298 §2.2, §2.3) and arms nothing more; one
    /// overdue is a suspicion at the timeout, and the next deadline doubles (§5.5) until an answer
    /// comes in time again (§5.7). Nothing owed, nothing is ever due: no tick, no probe.
    #[test]
    fn lb03_w05_an_answer_is_awaited_one_rto_and_the_timer_backs_off_after_a_miss() {
        let mut d = Detector::new();
        assert_eq!((d.timer(), d.due(1_000_000)), (None, Vec::new()), "nothing owed: nothing due, ever");
        // Unmeasured: RFC 6298 §2.1's second.
        d.owe("b", "welcome", 0);
        assert_eq!(d.timer(), Some(1_000));
        assert_eq!(d.answered("b", "welcome", 100), Some(100));
        assert_eq!(d.rtt("b").map(|r| (r.srtt, r.rttvar)), Some((100, 50)));
        assert_eq!((d.timer(), d.rto("b")), (None, 300), "SRTT + 4 RTTVAR");
        // Missed: suspected at the timeout, and the timer doubles.
        d.owe("b", "probe:r1", 1_000);
        assert!(d.due(1_299).is_empty());
        let s = d.due(1_300);
        assert_eq!(s, vec![Suspicion { peer: "b".into(), why: Why::Overdue { what: "probe:r1".into(), waited: 300 } }]);
        assert_eq!(d.rto("b"), 600);
        d.owe("b", "probe:r2", 2_000);
        assert_eq!(d.timer(), Some(2_600));
        assert_eq!(d.answered("b", "probe:r2", 2_100), Some(100));
        assert_eq!(d.rto("b"), 248, "an answer in time sets it again: 100 + 4 × 37");
        assert_eq!(d.timer(), None);
    }

    /// LB03: a dropped link is a suspicion at once, and nothing it owed is awaited any more; a
    /// report is one too. The back-off is capped at RFC 6298 §2.5's least maximum.
    #[test]
    fn lb03_a_drop_or_a_report_is_a_suspicion_and_the_timer_stops_at_its_most() {
        let mut d = Detector::new();
        d.owe("b", "welcome", 0);
        d.owe("c", "welcome", 0);
        let s = d.dropped("b", "b disconnected from the kiln server");
        assert_eq!(s.why, Why::Dropped("b disconnected from the kiln server".into()));
        assert!(!d.owes("b") && d.owes("c"));
        assert_eq!(d.due(5_000).len(), 1, "only c's");
        assert_eq!(d.reported("b", "c").to_string(), "c can't reach b");
        for i in 0..40 {
            d.owe("c", "x", i64::from(i) * 100_000);
            let _ = d.due(i64::from(i) * 100_000 + i64::try_from(MAX_RTO).unwrap_or(0));
        }
        assert_eq!(d.rto("c"), MAX_RTO);
    }
}
