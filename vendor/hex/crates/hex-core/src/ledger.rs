//! The macOS memory bound's primary layer (native plan step 2's pure core; docs/design/runtime.md). macOS
//! gives a privileged task a per-task physical-footprint ledger limit that fires a Mach `EXC_RESOURCE` the
//! instant the task crosses it — event-driven, near-overshoot-free, no polling. A box is a tree of
//! members, so to bound the box this core **apportions** the box's bound across its members as per-task
//! limits that sum to the bound: by pigeonhole the tree cannot exceed the bound without some member
//! crossing its own limit and firing (`sum(limit) == bound`, so `sum(footprint) > bound` ⟹ some
//! `footprint > limit`). On a breach it decides whether the box is over its bound (end it) or a member
//! just needs a bigger share of the headroom others aren't using (re-apportion and re-arm). This is the
//! rule; the shell that arms the kernel limits and catches the exception is the privileged helper
//! (`hex_runtime::macos`, step 2), beneath the rootless polling watcher ([`crate::watch`]) as
//! defense-in-depth.

/// A box's members and their footprints, as a read found them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    pub footprint: u64,
}

/// What to do when a member's ledger limit fired (it crossed its apportioned share).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Breach {
    /// The box is at or over its bound: end it. `footprint` is the box total read at the breach.
    Over { footprint: u64, bound: u64 },
    /// The box is still under its bound (other members hold less than their share): give each member a
    /// fresh limit and re-arm. The box total did not exceed the bound, so this is not an eviction.
    Reapportion(Vec<Limit>),
}

/// A member's apportioned ledger limit: the footprint at which the kernel fires for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub pid: u32,
    /// The limit in bytes, at least the member's current footprint and at most the box's bound.
    pub bytes: u64,
}

/// Apportions `bound` across `members` as per-task limits that sum to exactly `bound` (so the tree can't
/// exceed the bound without a member crossing its limit): each member keeps its current footprint and an
/// equal share of the headroom left, the integer remainder going to the earliest members. When the members
/// already hold `bound` or more there is no headroom, so each limit is the member's own footprint (any
/// growth fires at once) — the caller ends the box instead of arming these. An empty box gets no limits.
pub fn apportion(bound: u64, members: &[Member]) -> Vec<Limit> {
    let n = members.len();
    let Ok(count) = u64::try_from(n) else {
        return Vec::new();
    };
    if count == 0 {
        return Vec::new();
    }
    let total = members
        .iter()
        .fold(0u64, |sum, m| sum.saturating_add(m.footprint));
    // Headroom to share; 0 once the members already fill the bound.
    let headroom = bound.saturating_sub(total);
    // `count` is at least 1 (checked above), so neither divide nor remainder can fault; checked forms keep
    // the no-unchecked-arithmetic rule (CLAUDE.md §1).
    let share = headroom.checked_div(count).unwrap_or(0);
    // The remainder (headroom not divisible by the count) goes one byte each to the earliest members, so
    // the limits still sum to exactly `bound`.
    let mut remainder = headroom.checked_rem(count).unwrap_or(0);
    members
        .iter()
        .map(|m| {
            let extra = if remainder > 0 {
                remainder = remainder.saturating_sub(1);
                1
            } else {
                0
            };
            Limit {
                pid: m.pid,
                // footprint + share (+1 for the remainder) never overflows: footprint ≤ total and
                // share*count + remainder = headroom = bound - total, so the sum is ≤ bound.
                bytes: m.footprint.saturating_add(share).saturating_add(extra),
            }
        })
        .collect()
}

/// A member fired its ledger limit; `members` is the box read at the breach. Ends the box if its total is
/// at or over its bound, else re-apportions the headroom and re-arms (a member grew into room another
/// isn't using).
pub fn on_breach(bound: u64, members: &[Member]) -> Breach {
    let total = members
        .iter()
        .fold(0u64, |sum, m| sum.saturating_add(m.footprint));
    if total >= bound {
        Breach::Over {
            footprint: total,
            bound,
        }
    } else {
        Breach::Reapportion(apportion(bound, members))
    }
}

/// The total of a set of limits; a box arms limits summing to its bound.
pub fn total_limit(limits: &[Limit]) -> u64 {
    limits
        .iter()
        .fold(0u64, |sum, l| sum.saturating_add(l.bytes))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const MIB: u64 = 1 << 20;

    fn members(footprints: &[u64]) -> Vec<Member> {
        footprints
            .iter()
            .enumerate()
            .map(|(i, &f)| Member {
                pid: u32::try_from(i + 1).unwrap(),
                footprint: f,
            })
            .collect()
    }

    /// HEX-MAC-7: the apportioned limits sum to exactly the bound, and each is at least its member's
    /// footprint, so a member has room to grow but the tree can't exceed the bound without one firing.
    #[test]
    fn limits_sum_to_the_bound_and_cover_each_footprint() {
        let bound = 256 * MIB;
        let ms = members(&[10 * MIB, 40 * MIB, 0, 6 * MIB]);
        let limits = apportion(bound, &ms);
        assert_eq!(total_limit(&limits), bound);
        for (l, m) in limits.iter().zip(&ms) {
            assert!(l.bytes >= m.footprint, "{l:?} below {m:?}");
        }
        // The headroom (256 - 56 = 200 MiB) split four ways is 50 MiB each.
        assert_eq!(limits[0].bytes, 10 * MIB + 50 * MIB);
        assert_eq!(limits[2].bytes, 50 * MIB);
    }

    /// A box already at or over its bound has no headroom: each limit is its own footprint, and a breach
    /// read there ends the box with the total it reached.
    #[test]
    fn a_full_box_gets_no_headroom_and_a_breach_ends_it() {
        let bound = 256 * MIB;
        let ms = members(&[200 * MIB, 100 * MIB]);
        let limits = apportion(bound, &ms);
        assert_eq!(limits[0].bytes, 200 * MIB);
        assert_eq!(limits[1].bytes, 100 * MIB);
        assert_eq!(
            on_breach(bound, &ms),
            Breach::Over {
                footprint: 300 * MIB,
                bound
            }
        );
    }

    /// A breach while the box is under its bound re-apportions rather than ending it: a member grew into
    /// headroom another isn't using.
    #[test]
    fn a_breach_under_the_bound_reapportions() {
        let bound = 256 * MIB;
        let ms = members(&[120 * MIB, 8 * MIB]);
        match on_breach(bound, &ms) {
            Breach::Reapportion(limits) => {
                assert_eq!(total_limit(&limits), bound);
                assert!(limits[0].bytes >= 120 * MIB && limits[1].bytes >= 8 * MIB);
            }
            other => panic!("{other:?}"),
        }
    }

    /// An empty box has nothing to arm.
    #[test]
    fn an_empty_box_has_no_limits() {
        assert!(apportion(256 * MIB, &[]).is_empty());
    }

    /// Footprints that sum to at most `bound`, built (not filtered) from raw weights: each member gets less
    /// than an equal share, so the total is under the bound and the generator never rejects.
    fn under_bound(bound: u64, weights: &[u64]) -> Vec<u64> {
        let n = u64::try_from(weights.len()).unwrap_or(1).max(1);
        let cap = (bound / n).max(1);
        weights.iter().map(|&w| w % cap).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// HEX-MAC-7 (the safety guarantee): for any box under its bound, the apportioned limits sum to the
        /// bound and never sit below a member's footprint. Summing to the bound is what makes the pigeonhole
        /// hold — see `the_pigeonhole_holds`.
        #[test]
        fn apportioned_limits_sum_to_the_bound(
            bound in (1u64..(1 << 40)),
            weights in prop::collection::vec(0u64..(1 << 36), 1..40),
        ) {
            let footprints = under_bound(bound, &weights);
            let ms = members(&footprints);
            let limits = apportion(bound, &ms);
            prop_assert_eq!(total_limit(&limits), bound);
            for (l, m) in limits.iter().zip(&ms) {
                prop_assert!(l.bytes >= m.footprint);
                prop_assert!(l.bytes <= bound);
            }
        }

        /// The pigeonhole the whole scheme rests on: with limits that sum to the bound, any tree whose total
        /// footprint exceeds the bound has at least one member over its own limit — so the kernel fires for
        /// it, catching the box total, not just a single runaway process. The over-bound tree is constructed
        /// (not filtered): members grow by random amounts, then the last is nudged so the total just exceeds
        /// the bound.
        #[test]
        fn the_pigeonhole_holds(
            bound in (1u64..(1 << 40)),
            weights in prop::collection::vec(0u64..(1 << 34), 1..40),
            growth in prop::collection::vec(0u64..(1 << 34), 1..40),
        ) {
            let n = weights.len().min(growth.len());
            let base = under_bound(bound, &weights[..n]);
            let limits = apportion(bound, &members(&base));
            let mut grown: Vec<u64> = base
                .iter()
                .zip(&growth[..n])
                .map(|(&f, &g)| f.saturating_add(g))
                .collect();
            let grown_total: u128 = grown.iter().map(|&f| u128::from(f)).sum();
            // Force the tree over the bound if the random growth didn't already: add the shortfall plus one
            // to the last member.
            if grown_total <= u128::from(bound) {
                let short = bound - u64::try_from(grown_total).unwrap_or(bound);
                let last = grown.len() - 1;
                grown[last] = grown[last].saturating_add(short).saturating_add(1);
            }
            let total: u128 = grown.iter().map(|&f| u128::from(f)).sum();
            prop_assert!(total > u128::from(bound));
            // Some member is now over the limit it was armed with.
            let someone_over = grown.iter().zip(&limits).any(|(&f, l)| f > l.bytes);
            prop_assert!(someone_over, "tree over the bound but no member over its limit");
        }
    }
}
