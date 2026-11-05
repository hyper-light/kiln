//! Liveness by bytes arriving (docs/browser-protocol.md, "Liveness"; goal 9's rule, coded here
//! for both halves and mirrored in `ui/src/api/bp/liveness.ts`). The peer promises bytes at least
//! every idle period; the receiver learns how late they come beyond it (RFC 6298's smoothed mean
//! and mean deviation, over the gaps between arrivals), and how long a round trip takes. A link is
//! live while bytes are due, suspect once they're late (a probe goes first), and dead only when a
//! suspect link stays silent through a backed-off number of round trips after the probe: the
//! carrier below may be retransmitting with doubling timers (TCP's RFC 6298 backoff; a lost
//! packet holds everything after it), so a silent link is given `2^backoff - 1` round-trip
//! windows, as TCP gives a lost segment, before it's called dead (goal 9's traces: one window gave
//! 231 false dead verdicts in 300 traces at 50 kbit/s, 5% loss and a 6 s queue). Everything is worked out from
//! the arrival times given, whenever it's asked: a page that was frozen judges from what arrived,
//! not from timers that didn't fire. Times are milliseconds on the caller's clock; nothing here
//! reads one.

/// RFC 6298's estimator, in milliseconds scaled by 8 (mean) and 4 (deviation) as TCP keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Estimate {
    /// The smoothed mean, times 8.
    srtt8: u64,
    /// The mean deviation, times 4.
    var4: u64,
    samples: u64,
}

impl Estimate {
    pub fn sample(&mut self, x: u64) {
        let x8 = x.saturating_mul(8);
        if self.samples == 0 {
            self.srtt8 = x8;
            self.var4 = x.saturating_mul(2);
        } else {
            let srtt = self.srtt8 / 8;
            let err = srtt.abs_diff(x);
            // var = 3/4 var + 1/4 |srtt - x|; srtt = 7/8 srtt + 1/8 x.
            self.var4 = self.var4.saturating_sub(self.var4 / 4).saturating_add(err);
            self.srtt8 = self.srtt8.saturating_sub(self.srtt8 / 8).saturating_add(x);
        }
        self.samples = self.samples.saturating_add(1);
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn mean(&self) -> u64 {
        self.srtt8 / 8
    }

    pub fn deviation(&self) -> u64 {
        self.var4 / 4
    }

    /// The mean plus four deviations: RFC 6298's bound.
    pub fn bound(&self) -> u64 {
        self.mean().saturating_add(self.var4)
    }
}

/// What the link is, judged now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Bytes are due no later than they've come before.
    Live,
    /// Bytes are late: a probe goes ahead of everything.
    Suspect,
    /// Still silent, past a round trip after the probe: reconnect and resume.
    Dead,
}

/// The floor on the learned spread and round trip: the clock's own grain.
pub const GRAIN_MS: u64 = 1;
/// How many doublings of the round trip a silent link is given after its probe (`Liveness::dead_at`).
pub const BACKOFF: u32 = 5;

/// One side's view of its peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Liveness {
    /// The idle period the peer declared.
    idle_ms: u64,
    /// When bytes last arrived (the link's start until then).
    last: u64,
    /// How late bytes come beyond the idle period.
    late: Estimate,
    /// Round trips.
    rtt: Estimate,
    /// When a probe went, while one is out.
    probed: Option<u64>,
    /// Round-trip windows a silent link is given: `2^backoff - 1`.
    backoff: u32,
}

impl Liveness {
    /// A link that opened at `now`, whose peer declared `idle_ms`, with a first round trip
    /// (the open's) when one is known.
    pub fn new(idle_ms: u64, now: u64) -> Self {
        Self { idle_ms: idle_ms.max(GRAIN_MS), last: now, late: Estimate::default(), rtt: Estimate::default(), probed: None, backoff: BACKOFF }
    }

    /// With another backoff (what the measurements trade: fewer windows find a dead link sooner
    /// and misjudge more stalled ones).
    pub fn with_backoff(mut self, backoff: u32) -> Self {
        self.backoff = backoff.min(20);
        self
    }

    /// The page ran again after a freeze (shown, `pageshow`, resume): what it reads now came while
    /// it slept, so the gap is the page's, not the link's. Moves the last arrival to `now` without
    /// learning from the gap.
    pub fn resumed(&mut self, now: u64) {
        self.last = self.last.max(now);
        // A probe that went before the freeze isn't a round trip of the link's any more.
        self.probed = None;
    }

    pub fn set_idle(&mut self, idle_ms: u64) {
        self.idle_ms = idle_ms.max(GRAIN_MS);
    }

    pub fn idle_ms(&self) -> u64 {
        self.idle_ms
    }

    /// Bytes arrived at `now`: the link is live, and the gap since the last is learned from.
    pub fn arrived(&mut self, now: u64) {
        let gap = now.saturating_sub(self.last);
        // Only gaps that reach toward the idle period say how late an idle frame can be; the
        // short gaps of a busy link say nothing of it.
        if gap.saturating_mul(2) >= self.idle_ms {
            self.late.sample(gap.saturating_sub(self.idle_ms));
        }
        self.last = self.last.max(now);
        self.probed = None;
    }

    /// A round trip measured: a probe's answer, or the open's.
    pub fn round_trip(&mut self, ms: u64) {
        self.rtt.sample(ms);
    }

    /// How late bytes may be beyond the idle period: the learned bound, or the idle period itself
    /// before anything is learned.
    pub fn spread(&self) -> u64 {
        if self.late.samples() == 0 { self.idle_ms } else { self.late.bound().max(GRAIN_MS) }
    }

    /// The round trip a probe is given: the learned bound, or the idle period before anything
    /// is learned.
    pub fn rtt_bound(&self) -> u64 {
        if self.rtt.samples() == 0 { self.idle_ms } else { self.rtt.bound().max(GRAIN_MS) }
    }

    pub fn rtt(&self) -> &Estimate {
        &self.rtt
    }

    /// By when bytes are expected.
    pub fn expected_by(&self) -> u64 {
        self.last.saturating_add(self.idle_ms).saturating_add(self.spread())
    }

    /// The probe went at `now`.
    pub fn probe_sent(&mut self, now: u64) {
        if self.probed.is_none() {
            self.probed = Some(now);
        }
    }

    /// Whether a probe is out.
    pub fn probing(&self) -> bool {
        self.probed.is_some()
    }

    /// When the link is dead if nothing arrives: `2^backoff - 1` round trips after the probe,
    /// which goes no sooner than the link is suspect.
    pub fn dead_at(&self) -> u64 {
        let from = self.probed.unwrap_or(self.expected_by()).max(self.expected_by());
        let windows = (1u64 << self.backoff).saturating_sub(1);
        from.saturating_add(self.rtt_bound().saturating_mul(windows))
    }

    pub fn verdict(&self, now: u64) -> Verdict {
        if now < self.expected_by() {
            Verdict::Live
        } else if self.probed.is_none() || now < self.dead_at() {
            Verdict::Suspect
        } else {
            Verdict::Dead
        }
    }

    /// When the verdict next changes if nothing arrives: what a carrier's timer waits for.
    pub fn next_change(&self, now: u64) -> u64 {
        match self.verdict(now) {
            Verdict::Live => self.expected_by(),
            Verdict::Suspect if self.probed.is_some() => self.dead_at(),
            Verdict::Suspect | Verdict::Dead => now,
        }
    }

    pub fn last_arrival(&self) -> u64 {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn a_quiet_live_link_stays_live_and_a_dark_one_is_found_within_its_bound() {
        let mut l = Liveness::new(1000, 0);
        let mut t = 0;
        let mut g = Rng::new(5);
        // Idle frames every second, each up to 300 ms late.
        for _ in 0..200 {
            t += 1000 + g.below(300);
            assert_eq!(l.verdict(t), if t < l.expected_by() { Verdict::Live } else { Verdict::Suspect });
            l.arrived(t);
            l.round_trip(100 + g.below(50));
            assert_eq!(l.verdict(t), Verdict::Live);
        }
        // Dark: suspect after the expected time, dead a round trip after the probe.
        let suspect = l.expected_by();
        assert_eq!(l.verdict(suspect), Verdict::Suspect);
        l.probe_sent(suspect);
        let dead = l.dead_at();
        assert!(dead <= suspect + l.rtt_bound() * ((1 << BACKOFF) - 1));
        assert_eq!(l.verdict(dead - 1), Verdict::Suspect);
        assert_eq!(l.verdict(dead), Verdict::Dead);
        // A page that froze, then judges: no probe went, so it's suspect, not dead.
        let mut f = l.clone();
        f.probed = None;
        assert_eq!(f.verdict(dead + 60_000), Verdict::Suspect);
        // A page that resumes judges afresh from then, learning nothing from its own freeze.
        let spread = f.spread();
        f.resumed(dead + 60_000);
        assert_eq!(f.verdict(dead + 60_001), Verdict::Live);
        assert_eq!(f.spread(), spread);
    }

    #[test]
    fn the_estimate_is_rfc_6298s() {
        let mut e = Estimate::default();
        e.sample(100);
        assert_eq!((e.mean(), e.deviation(), e.bound()), (100, 50, 300));
        e.sample(200);
        // srtt = 7/8*100 + 1/8*200 = 112; var = 3/4*50 + 1/4*100 = 62.
        assert_eq!((e.mean(), e.deviation()), (112, 62));
    }
}
