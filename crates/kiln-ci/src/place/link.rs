//! What a link between an owner and a runner measures of itself, as the shell measures it from
//! the frames it sends and the answers it gets (docs/needs.md §21: every number a measurement or
//! from cited research): its round-trip time, kept as RFC 6298 keeps it, and the offset between the
//! two ends' clocks, as NTP's on-wire protocol takes it (RFC 5905 §8). Nothing here reads a clock.

use crate::sched::units::Ms;

/// A link's round-trip time as RFC 6298 ("Computing TCP's Retransmission Timer", Paxson, Allman,
/// Chu and Sargent, 2011) keeps it, from the shell's samples of it: the smoothed round trip, its
/// variation, and the clock's granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rtt {
    pub srtt: Ms,
    pub rttvar: Ms,
    pub granularity: Ms,
}

impl Rtt {
    /// RFC 6298 §2.1: before any round trip has been measured, the timer is 1 second.
    pub const INITIAL: Ms = 1000;

    /// RFC 6298 §2.2: the first measurement `r`: SRTT is `r`, RTTVAR half of it.
    pub fn first(r: Ms, granularity: Ms) -> Rtt {
        Rtt { srtt: r, rttvar: r / 2, granularity }
    }

    /// RFC 6298 §2.3: each later measurement `r`, with its gains of 1/8 and 1/4 (Jacobson,
    /// "Congestion Avoidance and Control", SIGCOMM 1988): RTTVAR first, from the SRTT before it.
    pub fn sample(&mut self, r: Ms) {
        let err = self.srtt.abs_diff(r);
        self.rttvar = self.rttvar.saturating_mul(3).saturating_add(err) / 4;
        self.srtt = self.srtt.saturating_mul(7).saturating_add(r) / 8;
    }

    /// RFC 6298 §2.3's retransmission timeout: SRTT plus the larger of the clock's granularity and
    /// K = 4 times RTTVAR: how long a word sent on the link waits for its answer before it's taken
    /// as lost. §2.4's floor of a second is left out: it's there for TCP's delayed
    /// acknowledgements, and an owner answers an offer at once.
    pub fn rto(&self) -> Ms {
        self.srtt.saturating_add(self.granularity.max(self.rttvar.saturating_mul(4)))
    }

    /// The timeout of a link that may not have been measured yet.
    pub fn rto_of(rtt: Option<&Rtt>) -> Ms {
        rtt.map_or(Rtt::INITIAL, Rtt::rto)
    }

    /// RFC 6298 §2.5: "A maximum value MAY be placed on RTO provided it is at least 60 seconds".
    /// The least the RFC allows: a word lost on a link that stays up is said again within a minute.
    pub const MOST: Ms = 60_000;

    /// The timeout `rto` after it ran out `n` times: RFC 6298 §5.5 doubles it each time ("back off
    /// the timer"), to at most [`Rtt::MOST`].
    pub fn backed_off(rto: Ms, n: u32) -> Ms {
        rto.saturating_mul(1u64.checked_shl(n).unwrap_or(u64::MAX)).min(Rtt::MOST)
    }
}

/// The offset between an owner's clock and a runner's, as NTP's on-wire protocol takes it from
/// one request and its answer (RFC 5905 §8): `t1` the owner sent it, `t2` the runner got it, `t3`
/// the runner answered, `t4` the owner got the answer, each by its own end's clock. `theta` is the
/// runner's clock less the owner's, within `delta / 2` of the truth; `delta` is the round trip
/// less the runner's time with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offset {
    pub theta: i64,
    pub delta: i64,
}

impl Offset {
    /// RFC 5905 §8: θ = ((t2 − t1) + (t3 − t4)) / 2 and δ = (t4 − t1) − (t3 − t2).
    pub fn of(t1: i64, t2: i64, t3: i64, t4: i64) -> Offset {
        let theta = t2.saturating_sub(t1).saturating_add(t3.saturating_sub(t4)) / 2;
        let delta = t4.saturating_sub(t1).saturating_sub(t3.saturating_sub(t2));
        Offset { theta, delta }
    }

    /// A time in the runner's clock, in the owner's.
    pub fn owner_time(&self, at: i64) -> i64 {
        at.saturating_sub(self.theta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6298's timer from its samples, and RFC 5905's offset from one exchange.
    #[test]
    fn e5_a_link_s_timeout_and_offset_are_its_measurements() {
        let mut r = Rtt::first(100, 1);
        assert_eq!((r.srtt, r.rttvar, r.rto()), (100, 50, 300));
        r.sample(100);
        assert_eq!((r.srtt, r.rttvar), (100, 37));
        assert_eq!(Rtt::rto_of(None), 1000);
        // §5.5's back-off doubles the timer each time it runs out, to §2.5's least maximum.
        assert_eq!([0, 1, 2, 6, 7, 64].map(|n| Rtt::backed_off(1000, n)), [1000, 2000, 4000, 60_000, 60_000, 60_000]);
        assert_eq!(Rtt::backed_off(300, 3), 2400);
        // The runner's clock 5 s ahead, 20 ms each way, 2 ms at the runner.
        let o = Offset::of(1_000, 6_020, 6_022, 1_042);
        assert_eq!((o.theta, o.delta), (5_000, 40));
        assert_eq!(o.owner_time(66_000), 61_000);
    }
}
