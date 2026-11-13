//! How long a group of processes gets to empty after `SIGKILL`: a failure bound, derived from measurement
//! (Ada's rule, 2026-11-06: no picked numbers). A kill ends its members in milliseconds; past this bound a
//! drain is a fault reported by name, never a limit raised (CLAUDE.md §4).

use std::time::Duration;

/// The worst kill-to-empty of a one-member group, in nanoseconds: the record's n=1 maximum
/// (bench/native/macos/records/killscale-a84e094-20261106T160339Z.txt).
pub const FIXED_NS: u64 = 883_000;
/// The worst cost of each member past the first, in nanoseconds: the record's largest slope over its
/// one-member maximum (n=32: (10.125 - 0.883) ms / 31).
pub const PER_MEMBER_NS: u64 = 298_000;
/// The lead's margin over the measured envelope.
pub const MARGIN: u64 = 2;

/// How long a group of `members` processes gets to empty after `SIGKILL`: the measured envelope times the
/// margin. Saturates at the largest duration a `u64` of nanoseconds holds (its stated meaning: never
/// overflows); a group listed empty gets the one-member bound.
pub fn bound(members: usize) -> Duration {
    let past_first = u64::try_from(members.saturating_sub(1)).unwrap_or(u64::MAX);
    Duration::from_nanos(
        PER_MEMBER_NS
            .saturating_mul(past_first)
            .saturating_add(FIXED_NS)
            .saturating_mul(MARGIN),
    )
}

/// The drain bound as this node learns it (the learned-waits rule, 2026-11-06): the record's to start, then
/// raised by the slowest drain this node has seen, for every group size in proportion, never lowered. It keeps
/// that slowest drain as a ratio to the record's envelope for its size, in parts per million (at least one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Learned {
    slowest_ppm: u64,
}

const PPM: u64 = 1_000_000;

impl Default for Learned {
    fn default() -> Self {
        Self { slowest_ppm: PPM }
    }
}

impl Learned {
    /// How long a group of `members` gets to empty: [`bound`] scaled by the slowest drain seen, never below it.
    pub fn bound(&self, members: usize) -> Duration {
        let scaled = bound(members)
            .as_nanos()
            .saturating_mul(u128::from(self.slowest_ppm))
            .checked_div(u128::from(PPM))
            .unwrap_or(u128::MAX);
        Duration::from_nanos(u64::try_from(scaled).unwrap_or(u64::MAX))
    }

    /// Notes a drain of `members` that took `elapsed`: raises the bound if it was slower, relative to the
    /// record's envelope for its size, than any seen.
    pub fn observe(&mut self, members: usize, elapsed: Duration) {
        let Some(envelope) = bound(members).as_nanos().checked_div(u128::from(MARGIN)) else {
            return;
        };
        let Some(ratio) = elapsed
            .as_nanos()
            .saturating_mul(u128::from(PPM))
            .checked_div(envelope)
        else {
            return;
        };
        let ratio = u64::try_from(ratio).unwrap_or(u64::MAX);
        if ratio > self.slowest_ppm {
            self.slowest_ppm = ratio;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record's envelope, doubled: 2 x (0.883 ms + 0.298 ms per member past the first)
    /// (bench/native/macos/records/killscale-a84e094-20261106T160339Z.txt). Do ask for 1, 32, 512 and 6,000
    /// members; expect 1.766 ms, 20.242 ms, 306.322 ms and 3.577 s.
    #[test]
    fn the_bound_is_the_measured_envelope_doubled() {
        assert_eq!(bound(1), Duration::from_nanos(1_766_000));
        assert_eq!(bound(32), Duration::from_nanos(20_242_000));
        assert_eq!(bound(512), Duration::from_nanos(306_322_000));
        assert_eq!(bound(6_000), Duration::from_nanos(3_577_170_000));
    }

    /// A group listed empty still gets the one-member bound (the kill may race the listing), and the bound
    /// never shrinks as a group grows, nor overflows (CLAUDE.md §1). Do ask for 0 and for the most a usize
    /// holds; expect the one-member bound and a finite bound.
    #[test]
    fn the_bound_grows_with_the_group_and_never_overflows() {
        assert_eq!(bound(0), bound(1));
        let mut last = Duration::ZERO;
        for n in [1, 2, 31, 32, 33, 1000, 100_000] {
            assert!(bound(n) >= last);
            last = bound(n);
        }
        assert!(bound(usize::MAX) >= last);
    }

    /// The learned-waits rule (2026-11-06): before any sample the bound is the record's, 2 x its envelope; a
    /// slower drain on this node raises it, for every size in proportion; a faster one leaves it; it never
    /// falls. Do read a fresh learner's bound for 8 members, then observe an 8-member drain at 3 x the
    /// record's envelope, then one at half of it; expect the record's bound, then 3 x it, then still 3 x it,
    /// and 32 members' bound 3 x the record's too.
    #[test]
    fn a_learned_bound_starts_at_the_record_and_only_rises() {
        let mut learned = Learned::default();
        assert_eq!(learned.bound(8), bound(8));
        let envelope = bound(8) / 2;
        learned.observe(8, envelope * 3);
        assert_eq!(learned.bound(8), bound(8) * 3);
        learned.observe(8, envelope / 2);
        assert_eq!(learned.bound(8), bound(8) * 3);
        assert_eq!(learned.bound(32), bound(32) * 3);
    }

    /// The learned bound never overflows, whatever is observed (CLAUDE.md §1). Do observe the longest
    /// duration there is; expect a finite bound no lower than the record's.
    #[test]
    fn a_learned_bound_never_overflows() {
        let mut learned = Learned::default();
        learned.observe(1, Duration::MAX);
        assert!(learned.bound(1_000) >= bound(1_000));
    }
}
