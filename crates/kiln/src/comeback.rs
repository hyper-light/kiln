//! How long a runner is given to come back (docs/machines.md, "The grace to come back"): once the server has lost it, or has started
//! again without it, its jobs wait that long for it before they're requeued, an attach to a session it holds waits that
//! long for it as the server starts (`crate::harness::returning`), and a job pod's record is kept that long. Learned from
//! how long runners take to come back holding their work (Ada, 2026-11-06: no arbitrary constants), in place of
//! `AGENT_GRACE`, 45 s whatever the runners did.
//!
//! RFC 6298's estimator, over comebacks rather than round trips. Each runner that comes back still running jobs is a
//! sample: the time since the server lost it, or, for one this server hasn't seen since it started (it ran them through
//! the restart), since the start. The grace is the smoothed time and four times its variation (the RFC's RTO; its `K`,
//! 4, its `alpha`, 1/8, and its `beta`, 1/4; the first sample the smoothed time, half of it the variation). A runner back
//! later than twice the grace counts as twice the grace, as the RFC backs its timer off on a timeout (5.5): a runner gone
//! for hours (a machine that slept with a job) moves the grace once as far as the grace itself, not by hours. Only
//! runners holding work count: they're the ones the grace is for, and a machine that slept idle or was turned off comes
//! back holding none. What's learned is kept in kiln.db (`kv`, [`KEY`]) as it changes, so the attaches and jobs a restart
//! finds wait as long as runners took to come back before it.
//!
//! Until a first runner has come back holding work there's nothing to learn from, and the grace is [`PRIOR`]: the deploy
//! that brings this in waits as kiln did before it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Where what's learned is kept, in kiln.db's `kv`: `<smoothed ms> <variation ms>`.
pub const KEY: &str = "runners/comeback";

/// The grace before any runner has come back holding work: what kiln gave every runner before it learned
/// (`AGENT_GRACE`), the one value here not learned (nothing has come back yet to learn it from).
pub const PRIOR: Duration = Duration::from_secs(45);

/// What's learned of comebacks: the smoothed time and its variation, in milliseconds, each half of one word (49 days at
/// the most), swapped whole by compare-and-swap; none (0) until a first comeback.
#[derive(Default)]
pub struct Comeback(AtomicU64);

impl Comeback {
    fn halves(word: u64) -> Option<(u64, u64)> {
        (word != 0).then_some((word >> 32, word & u64::from(u32::MAX)))
    }

    fn word(smoothed: u64, variation: u64) -> u64 {
        let half = |v: u64| v.min(u64::from(u32::MAX));
        // A smoothed time of none still says something was learned.
        (half(smoothed).max(1) << 32) | half(variation)
    }

    fn grace_of(word: u64) -> Duration {
        Self::halves(word).map_or(PRIOR, |(smoothed, variation)| Duration::from_millis(smoothed.saturating_add(variation.saturating_mul(4))))
    }

    /// How long a runner is given to come back, now.
    pub fn grace(&self) -> Duration {
        Self::grace_of(self.0.load(Ordering::Relaxed))
    }

    /// A runner came back holding its work `after` the server lost it (or started without it): learned. Gives what to
    /// keep ([`KEY`]).
    pub fn came_back(&self, after: Duration) -> String {
        let took = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        let mut seen = self.0.load(Ordering::Relaxed);
        loop {
            let backed_off = u64::try_from(Self::grace_of(seen).as_millis()).unwrap_or(u64::MAX).saturating_mul(2);
            let r = took.min(backed_off);
            let next = match Self::halves(seen) {
                None => Self::word(r, r / 2),
                Some((smoothed, variation)) => {
                    let variation = variation.saturating_mul(3).saturating_add(smoothed.abs_diff(r)) / 4;
                    let smoothed = smoothed.saturating_mul(7).saturating_add(r) / 8;
                    Self::word(smoothed, variation)
                }
            };
            match self.0.compare_exchange_weak(seen, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Self::text(next),
                Err(now) => seen = now,
            }
        }
    }

    fn text(word: u64) -> String {
        let (smoothed, variation) = Self::halves(word).unwrap_or_default();
        format!("{smoothed} {variation}")
    }

    /// Takes what a run before kept ([`KEY`]); a text that doesn't read leaves nothing learned.
    pub fn take_kept(&self, kept: &str) {
        let mut parts = kept.split_whitespace().map(str::parse::<u64>);
        if let (Some(Ok(smoothed)), Some(Ok(variation)), None) = (parts.next(), parts.next(), parts.next()) {
            self.0.store(Self::word(smoothed, variation), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6298's estimator over comebacks: the first the smoothed time with half of it its variation, each after it
    /// an eighth of the way for the time and a quarter for the variation, the grace their sum with four variations; a
    /// comeback past twice the grace counts as twice the grace; what's kept reads back as what was learned.
    #[test]
    fn comeback_the_grace_is_rfc_6298s_bound_over_how_long_runners_took() {
        let c = Comeback::default();
        assert_eq!(c.grace(), PRIOR, "nothing learned");
        assert_eq!(c.came_back(Duration::from_secs(4)), "4000 2000");
        assert_eq!(c.grace(), Duration::from_secs(12));
        // 2 s: variation (3 x 2000 + 2000) / 4 = 2000; time (7 x 4000 + 2000) / 8 = 3750.
        assert_eq!(c.came_back(Duration::from_secs(2)), "3750 2000");
        assert_eq!(c.grace(), Duration::from_millis(11_750));
        // Gone for an hour: counted as twice the grace, 23.5 s.
        assert_eq!(c.came_back(Duration::from_secs(3600)), "6218 6437");
        assert_eq!(c.grace(), Duration::from_millis(6218 + 4 * 6437));
        let again = Comeback::default();
        again.take_kept(&Comeback::text(c.0.load(Ordering::Relaxed)));
        assert_eq!(again.grace(), c.grace(), "a restart takes what was learned");
        for bad in ["", "12", "a b", "1 2 3"] {
            let fresh = Comeback::default();
            fresh.take_kept(bad);
            assert_eq!(fresh.grace(), PRIOR, "{bad:?} doesn't read");
        }
        // The first comeback past twice the prior counts as twice the prior.
        let slow = Comeback::default();
        assert_eq!(slow.came_back(Duration::from_secs(1000)), "90000 45000");
    }
}
