//! The rule a macOS box's memory is bounded by when a privileged helper is present (the native
//! plan's step 2, docs/design/runtime.md). Where step 1's rootless box is bounded by polling each
//! member's footprint and ending the box past a bound ([`crate::watch`]), a box with a helper is
//! bounded by the kernel: the helper arms a per-task physical-footprint ledger limit on each member
//! (`task_set_phys_footprint_limit`), and this core says what each member's limit must be.
//!
//! The limits are apportioned so they **sum to the box's bound less a reserve**. By pigeonhole the
//! armed members' footprints then sum to at most `bound - reserve`, so the whole tree stays under
//! the bound even while a member that has just forked is still unarmed (its own footprint has the
//! reserve to grow into before the helper arms it; `fork` resets the kernel's limit to none,
//! `bsd/kern/kern_fork.c:1199-1209`, so a new member starts unbounded until armed, and that window
//! is bench/native/macos's to measure and the reserve's to cover). A member that reaches its share
//! raises `EXC_RESOURCE` to the helper; the helper re-apportions (this core, with the member's new
//! footprint) and, if headroom remains, the member's share grows, else the box is ended. So a
//! crossing "rebalances or ends the box" (Ada's call, 2026-11-01) and both are this one decision.
//!
//! Pure (CLAUDE.md §9): the shell (`hex_runtime::macos`) sets the kernel limits, catches the
//! exception, kills and reaps; this only does the arithmetic. Memory alone: a box's process count
//! is the kernel's, `RLIMIT_NPROC` of the box's own user
//! (docs/research/runtime/native-platforms.md §3), not this core's.

use std::collections::BTreeSet;

/// A box's memory bound and the reserve held back from apportionment.
///
/// `reserve` is the headroom the sum of the members' limits is kept below the bound by, so a member
/// that has forked but is not yet armed has room to grow into before the box's total reaches the
/// bound. It is measured against the arming window in bench/native/macos (CLAUDE.md §5): the reserve
/// must cover what a new member allocates between its fork and the helper arming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// The box's memory bound, in bytes: its members' footprints, summed, may not pass it.
    pub limit: u64,
    /// Held back from apportionment, in bytes: the arming window's headroom.
    pub reserve: u64,
}

impl Bound {
    /// What the members' limits may sum to: the bound less the reserve, saturating at 0 (a reserve
    /// past the bound leaves nothing to apportion, so every member is ended at once).
    pub fn apportionable(&self) -> u64 {
        self.limit.saturating_sub(self.reserve)
    }
}

/// One member's footprint now, in bytes, by pid: what the helper read for it (`proc_pid_rusage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub pid: u32,
    pub footprint: u64,
}

/// The ledger limit the helper must arm member `pid` at, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Share {
    pub pid: u32,
    pub limit: u64,
}

/// What apportioning a box's bound across its members decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Arm each member at its limit. The limits sum to at most [`Bound::apportionable`], and each is
    /// at least the member's own footprint (a limit below it would end the member at once). Empty
    /// when the box has no members: nothing to arm.
    Arm(Vec<Share>),
    /// The members' footprints already reach what may be apportioned, so no limits can be set that
    /// both cover the members and sum within the bound: end the box, reporting the footprint it
    /// reached and the bound it reached.
    End { footprint: u64, bound: u64 },
}

/// The members' footprints, summed, saturating (a sum past 2^64 bytes is past any bound).
fn total(members: &[Use]) -> u64 {
    members
        .iter()
        .fold(0u64, |sum, m| sum.saturating_add(m.footprint))
}

/// Apportions `bound` across `members`: each member's ledger limit is its own footprint plus an
/// equal share of what headroom is left, so the limits sum to [`Bound::apportionable`] and none is
/// below its member's footprint. If the members' footprints already reach the apportionable bound,
/// no such limits exist and the box is ended instead (HEX-MAC-8).
///
/// The equal share's integer remainder (apportionable bytes modulo the member count) is given one
/// byte each to the members the remainder covers, so the limits sum to exactly the apportionable
/// bound, never over it. A member listed twice (the same pid) would be apportioned twice; the shell
/// lists each member once (`proc_listpids`), so the caller's slice holds distinct pids.
pub fn apportion(bound: Bound, members: &[Use]) -> Plan {
    if members.is_empty() {
        return Plan::Arm(Vec::new());
    }
    let apportionable = bound.apportionable();
    let used = total(members);
    // A member can't be armed below its own footprint (the kernel would end it at once). If the
    // footprints already reach what may be apportioned, there's no headroom to cover them within the
    // bound: end the box. `>=` not `>`, so a box exactly at the apportionable bound is ended, since
    // arming at the footprint leaves no room to grow and the next read would end it regardless.
    if used >= apportionable {
        return Plan::End {
            footprint: used,
            bound: bound.limit,
        };
    }
    // The count fits a u32: a slice holds at most isize::MAX members, and a box's members are capped
    // far below that by RLIMIT_NPROC. Non-empty, so the division's divisor isn't 0.
    let n = u64::try_from(members.len()).unwrap_or(u64::MAX);
    // `used < apportionable` (ended above otherwise), and `n >= 1` (non-empty), so neither the subtraction
    // nor the division can fail; the checked forms satisfy CLAUDE.md §1 without a panic path.
    let headroom = apportionable.saturating_sub(used);
    let base = headroom.checked_div(n).unwrap_or(0);
    let remainder = headroom.checked_rem(n).unwrap_or(0);
    let shares = members
        .iter()
        .enumerate()
        .map(|(i, m)| {
            // The first `remainder` members get one extra byte, so every byte of headroom is handed
            // out and the limits sum to exactly `apportionable`.
            let extra = u64::from(u64::try_from(i).unwrap_or(u64::MAX) < remainder);
            Share {
                pid: m.pid,
                // Each addend is within the bound and their sum is `used + headroom = apportionable`,
                // so no add overflows; saturating guards the types all the same.
                limit: m.footprint.saturating_add(base).saturating_add(extra),
            }
        })
        .collect();
    Plan::Arm(shares)
}

/// How a box's members changed between two of the helper's listings, so the helper knows which pids
/// to arm (the added) and which limits to forget (the gone). A member is its pid; the helper reads
/// the box's user's processes (`proc_listpids(PROC_UID_ONLY)`), which keeps a daemonized member
/// (one that called `setsid`) that step 1's process-group listing would lose, since a user id is
/// inherited and can't be dropped unprivileged (native-platforms.md §3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Pids the new listing adds: each a member to arm.
    pub added: Vec<u32>,
    /// Pids the new listing drops: each a member that ended, whose limit the helper forgets.
    pub gone: Vec<u32>,
}

/// What changed from listing `before` to listing `now`, by pid (HEX-MAC-9).
pub fn changes(before: &[u32], now: &[u32]) -> Changes {
    let was: BTreeSet<u32> = before.iter().copied().collect();
    let is: BTreeSet<u32> = now.iter().copied().collect();
    Changes {
        added: now.iter().copied().filter(|p| !was.contains(p)).collect(),
        gone: before.iter().copied().filter(|p| !is.contains(p)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;
    use crate::meet::{cases_to_meet, sized_runner};

    const MIB: u64 = 1 << 20;

    fn use_(pid: u32, footprint: u64) -> Use {
        Use { pid, footprint }
    }

    fn bound(limit: u64, reserve: u64) -> Bound {
        Bound { limit, reserve }
    }

    /// The limits a plan arms, by pid; empty for an end.
    fn limits(plan: &Plan) -> Vec<Share> {
        match plan {
            Plan::Arm(s) => s.clone(),
            Plan::End { .. } => Vec::new(),
        }
    }

    /// HEX-MAC-7: a box of equal, idle members splits its apportionable bound equally, and the
    /// limits sum to exactly the apportionable bound (the remainder handed out, not dropped).
    #[test]
    fn an_idle_box_splits_its_bound_equally() {
        // 256 MiB bound, 16 MiB reserve: 240 MiB apportioned across 4 members at 0 footprint.
        let plan = apportion(
            bound(256 * MIB, 16 * MIB),
            &[use_(2, 0), use_(3, 0), use_(4, 0), use_(5, 0)],
        );
        let shares = limits(&plan);
        assert_eq!(shares.len(), 4);
        assert!(shares.iter().all(|s| s.limit == 60 * MIB));
        assert_eq!(shares.iter().map(|s| s.limit).sum::<u64>(), 240 * MIB);
    }

    /// HEX-MAC-7: a member already using memory keeps its footprint and shares only the headroom,
    /// so no member is armed below what it already holds.
    #[test]
    fn a_members_limit_is_never_below_its_footprint() {
        // 200 MiB apportionable; one member at 150 MiB, one at 0: headroom 50 MiB, 25 MiB each.
        let plan = apportion(bound(200 * MIB, 0), &[use_(7, 150 * MIB), use_(8, 0)]);
        let shares = limits(&plan);
        assert_eq!(
            shares[0],
            Share {
                pid: 7,
                limit: 175 * MIB
            }
        );
        assert_eq!(
            shares[1],
            Share {
                pid: 8,
                limit: 25 * MIB
            }
        );
        assert_eq!(shares.iter().map(|s| s.limit).sum::<u64>(), 200 * MIB);
    }

    /// HEX-MAC-8: when the members' footprints already reach the apportionable bound, the box is
    /// ended, reporting the footprint and the real bound (not the apportionable one).
    #[test]
    fn a_box_at_its_apportionable_bound_is_ended() {
        let plan = apportion(
            bound(256 * MIB, 16 * MIB),
            &[use_(2, 120 * MIB), use_(3, 120 * MIB)],
        );
        assert_eq!(
            plan,
            Plan::End {
                footprint: 240 * MIB,
                bound: 256 * MIB,
            }
        );
    }

    /// An empty box has nothing to arm.
    #[test]
    fn an_empty_box_arms_nothing() {
        assert_eq!(
            apportion(bound(256 * MIB, 16 * MIB), &[]),
            Plan::Arm(Vec::new())
        );
    }

    /// A reserve at or past the bound leaves nothing to apportion, so any member is ended at once.
    #[test]
    fn a_reserve_past_the_bound_ends_every_box() {
        assert_eq!(
            apportion(bound(64 * MIB, 64 * MIB), &[use_(9, 0)]),
            Plan::End {
                footprint: 0,
                bound: 64 * MIB
            }
        );
    }

    /// HEX-MAC-9: the listing's diff names the members added and gone, by pid.
    #[test]
    fn a_listing_diff_names_added_and_gone() {
        let c = changes(&[10, 11, 12], &[11, 12, 13, 14]);
        assert_eq!(c.added, vec![13, 14]);
        assert_eq!(c.gone, vec![10]);
    }

    fn arb_members() -> impl Strategy<Value = Vec<Use>> {
        // Distinct pids, so a member isn't apportioned twice (the shell lists each once).
        prop::collection::vec((1u32..256, 0u64..(512 * MIB)), 0..24).prop_map(|raw| {
            let mut seen = BTreeSet::new();
            raw.into_iter()
                .filter(|(pid, _)| seen.insert(*pid))
                .map(|(pid, footprint)| use_(pid, footprint))
                .collect()
        })
    }

    fn arb_bound() -> impl Strategy<Value = Bound> {
        (MIB..(4096 * MIB), 0u64..(128 * MIB)).prop_map(|(limit, reserve)| bound(limit, reserve))
    }

    /// The kinds of plan the model-based test met, so it can't pass by never meeting one
    /// (CLAUDE.md §6's non-vacuity counters).
    #[derive(Default)]
    struct Met {
        arm_empty: Cell<u64>,
        arm_one: Cell<u64>,
        arm_many: Cell<u64>,
        arm_with_remainder: Cell<u64>,
        end: Cell<u64>,
    }

    impl Met {
        fn kinds(&self) -> [(&'static str, u64); 5] {
            [
                ("an empty arm", self.arm_empty.get()),
                ("a one-member arm", self.arm_one.get()),
                ("a many-member arm", self.arm_many.get()),
                ("an arm with a remainder", self.arm_with_remainder.get()),
                ("an end", self.end.get()),
            ]
        }
    }

    /// The outcomes the armed-tree test met (non-vacuity, CLAUDE.md §6).
    #[derive(Default)]
    struct Grown {
        ended_at_start: Cell<u64>,
        ended_growing: Cell<u64>,
        held: Cell<u64>,
    }

    impl Grown {
        fn kinds(&self) -> [(&'static str, u64); 3] {
            [
                ("a box ended at its start", self.ended_at_start.get()),
                ("a box ended as it grew", self.ended_growing.get()),
                ("a box held through its growth", self.held.get()),
            ]
        }
    }

    /// The rarest kind's share of [`apportioning_holds_its_invariants`]'s cases, and of
    /// [`the_armed_tree_never_passes_the_bound`]'s, as measured by `records_the_rarest_case_shares`
    /// (bench/native/macos/records/case-shares-boxuser.txt).
    const APPORTION_RAREST_SHARE: f64 = 0.03897;
    const ARMED_TREE_RAREST_SHARE: f64 = 0.01562;
    /// The case by which proptest's deterministic sequence, the one these tests draw from
    /// ([`sized_runner`]), has met every kind, by the same recorder: a fixed sequence meets each kind or
    /// never does, so each test runs at least this many cases, as well as the count its rarest share asks.
    const APPORTION_DETERMINISTIC_MEETS_ALL: u32 = 93;
    const ARMED_TREE_DETERMINISTIC_MEETS_ALL: u32 = 27;

    /// HEX-MAC-7/8 (model-based, CLAUDE.md §6): for any bound and any members, the plan holds the
    /// apportionment's invariants —
    /// - an Arm's limits sum to at most the apportionable bound (so the armed tree can't pass the
    ///   bound, leaving the reserve for an unarmed fork), and to exactly it when the box isn't
    ///   empty (every byte handed out, none dropped);
    /// - no member is armed below its own footprint;
    /// - the box is ended exactly when the members' footprints reach the apportionable bound.
    #[test]
    fn apportioning_holds_its_invariants() {
        let met = Met::default();
        let mut runner = sized_runner(
            cases_to_meet(APPORTION_RAREST_SHARE).max(APPORTION_DETERMINISTIC_MEETS_ALL),
        );
        runner
            .run(&(arb_bound(), arb_members()), |(b, members)| {
                apportion_holds(b, &members, &met)
            })
            .unwrap();
        for (kind, n) in met.kinds() {
            assert!(n > 0, "the cases met no {kind}");
        }
    }

    /// One case of [`apportioning_holds_its_invariants`], counting the kind of plan it met.
    fn apportion_holds(b: Bound, members: &[Use], met: &Met) -> Result<(), TestCaseError> {
        let bump = |c: &Cell<u64>| c.set(c.get() + 1);
        let members = members.to_vec();
        let used = total(&members);
        let apportionable = b.apportionable();
        let plan = apportion(b, &members);
        match &plan {
            Plan::Arm(shares) => {
                prop_assert!(members.is_empty() || used < apportionable);
                prop_assert_eq!(shares.len(), members.len());
                let sum: u64 = shares.iter().map(|s| s.limit).sum();
                prop_assert!(sum <= apportionable, "sum {} > {}", sum, apportionable);
                for (s, m) in shares.iter().zip(&members) {
                    prop_assert_eq!(s.pid, m.pid);
                    prop_assert!(s.limit >= m.footprint);
                }
                if members.is_empty() {
                    bump(&met.arm_empty);
                } else {
                    // Non-empty: every byte of headroom handed out.
                    prop_assert_eq!(sum, apportionable);
                    match members.len() {
                        1 => bump(&met.arm_one),
                        _ => bump(&met.arm_many),
                    }
                    if !(apportionable - used).is_multiple_of(u64::try_from(members.len()).unwrap())
                    {
                        bump(&met.arm_with_remainder);
                    }
                }
            }
            Plan::End { footprint, bound } => {
                prop_assert_eq!(*footprint, used);
                prop_assert_eq!(*bound, b.limit);
                prop_assert!(!members.is_empty());
                prop_assert!(used >= apportionable);
                bump(&met.end);
            }
        }
        Ok(())
    }

    /// HEX-MAC-7 (the bound holds): members grow, and as long as each stays within the limit the
    /// core last armed it at, the box's footprint never passes the bound. A member that reaches its
    /// limit is re-apportioned (its new, higher footprint fed back); when no headroom is left the
    /// box is ended, and that happens before the footprint passes the bound. This is the property
    /// the kernel's per-task ledger limits turn into a tree-wide bound.
    #[test]
    fn the_armed_tree_never_passes_the_bound() {
        let grown = Grown::default();
        let mut runner = sized_runner(
            cases_to_meet(ARMED_TREE_RAREST_SHARE).max(ARMED_TREE_DETERMINISTIC_MEETS_ALL),
        );
        runner
            .run(&armed_tree_scenario(), |(b, growths)| {
                armed_tree_holds(b, growths, &grown)
            })
            .unwrap();
        for (kind, n) in grown.kinds() {
            assert!(n > 0, "the cases met no {kind}");
        }
    }

    fn armed_tree_scenario() -> impl Strategy<Value = (Bound, Vec<(u32, Vec<u64>)>)> {
        (
            arb_bound(),
            prop::collection::vec(
                (1u32..64, prop::collection::vec(0u64..(64 * MIB), 1..16)),
                1..8,
            ),
        )
    }

    /// One case of [`the_armed_tree_never_passes_the_bound`], counting its outcome.
    fn armed_tree_holds(
        b: Bound,
        growths: Vec<(u32, Vec<u64>)>,
        grown: &Grown,
    ) -> Result<(), TestCaseError> {
        let bump = |c: &Cell<u64>| c.set(c.get() + 1);
        // Distinct pids with a series of growth steps each.
        let mut seen = BTreeSet::new();
        let members: Vec<(u32, Vec<u64>)> = growths
            .into_iter()
            .filter(|(pid, _)| seen.insert(*pid))
            .collect();
        // Each member's footprint and the limit it is armed at now.
        let mut footprint: Vec<u64> = members.iter().map(|_| 0).collect();
        let mut armed: Vec<u64> = members.iter().map(|_| 0).collect();
        let rearm = |footprint: &[u64], armed: &mut [u64]| -> Option<(u64, u64)> {
            let uses: Vec<Use> = footprint
                .iter()
                .enumerate()
                .map(|(i, &f)| use_(u32::try_from(i).unwrap(), f))
                .collect();
            match apportion(b, &uses) {
                Plan::Arm(shares) => {
                    for (i, s) in shares.iter().enumerate() {
                        armed[i] = s.limit;
                    }
                    None
                }
                Plan::End { footprint, bound } => Some((footprint, bound)),
            }
        };
        // Arm once at the start.
        if let Some((fp, bnd)) = rearm(&footprint, &mut armed) {
            // Ended before anything grew: the start already reached the bound.
            prop_assert!(fp >= b.apportionable());
            prop_assert_eq!(bnd, b.limit);
            bump(&grown.ended_at_start);
            return Ok(());
        }
        let steps = members.iter().map(|(_, g)| g.len()).max().unwrap_or(0);
        for step in 0..steps {
            for (i, (_, growth)) in members.iter().enumerate() {
                if let Some(&g) = growth.get(step) {
                    // A member grows, but the kernel holds it at the limit it was armed at:
                    // it can't pass `armed[i]`.
                    footprint[i] = (footprint[i].saturating_add(g)).min(armed[i]);
                }
            }
            // The armed tree never passes the bound, whatever the members did.
            let sum: u64 = footprint.iter().copied().fold(0, u64::saturating_add);
            prop_assert!(sum <= b.limit, "armed sum {} > bound {}", sum, b.limit);
            // A member at its limit asks to be re-apportioned.
            if footprint.iter().zip(&armed).any(|(f, a)| f >= a)
                && let Some((fp, _)) = rearm(&footprint, &mut armed)
            {
                // Ended: the footprint reached the apportionable bound, still under the real
                // bound (the reserve is the gap).
                prop_assert!(fp <= b.limit);
                bump(&grown.ended_growing);
                return Ok(());
            }
        }
        bump(&grown.held);
        Ok(())
    }

    /// Records the rarest kind's share of each model test's cases, which sizes its case count
    /// ([`cases_to_meet`]): run with `--ignored --nocapture`, its output is
    /// bench/native/macos/records/case-shares-boxuser.txt.
    #[test]
    #[ignore = "a recorder: its output sizes the model tests' case counts"]
    fn records_the_rarest_case_shares() {
        // Enough to know a share near the rarest (about 0.016) to about 2.5% relative: its standard error,
        // sqrt((1 - p) / (n p)).
        const RECORD_CASES: u32 = 100_000;
        let met = Met::default();
        let mut runner = TestRunner::new(ProptestConfig::with_cases(RECORD_CASES));
        runner
            .run(&(arb_bound(), arb_members()), |(b, members)| {
                apportion_holds(b, &members, &met)
            })
            .unwrap();
        let grown = Grown::default();
        let mut runner = TestRunner::new(ProptestConfig::with_cases(RECORD_CASES));
        runner
            .run(&armed_tree_scenario(), |(b, growths)| {
                armed_tree_holds(b, growths, &grown)
            })
            .unwrap();
        for (test, kinds) in [
            ("apportioning_holds_its_invariants", met.kinds().to_vec()),
            (
                "the_armed_tree_never_passes_the_bound",
                grown.kinds().to_vec(),
            ),
        ] {
            let total: u64 = RECORD_CASES.into();
            for (kind, n) in &kinds {
                println!(
                    "{test}: {kind}: {n} of {total} ({:.5})",
                    *n as f64 / total as f64
                );
            }
            let rarest = kinds.iter().map(|(_, n)| *n).min().unwrap_or(0);
            let share = rarest as f64 / total as f64;
            println!(
                "{test}: rarest share {share:.5} -> {} cases",
                cases_to_meet(share)
            );
        }
        // Where proptest's deterministic sequence, the one the tests draw from, first meets every kind.
        let (met, at, first) = (Met::default(), Cell::new(0u32), Cell::new(None));
        sized_runner(RECORD_CASES)
            .run(&(arb_bound(), arb_members()), |(b, members)| {
                let held = apportion_holds(b, &members, &met);
                at.set(at.get() + 1);
                if first.get().is_none() && met.kinds().iter().all(|(_, n)| *n > 0) {
                    first.set(Some(at.get()));
                }
                held
            })
            .unwrap();
        println!(
            "apportioning_holds_its_invariants: the deterministic sequence meets every kind by case {:?}",
            first.get()
        );
        let (grown, at, first) = (Grown::default(), Cell::new(0u32), Cell::new(None));
        sized_runner(RECORD_CASES)
            .run(&armed_tree_scenario(), |(b, growths)| {
                let held = armed_tree_holds(b, growths, &grown);
                at.set(at.get() + 1);
                if first.get().is_none() && grown.kinds().iter().all(|(_, n)| *n > 0) {
                    first.set(Some(at.get()));
                }
                held
            })
            .unwrap();
        println!(
            "the_armed_tree_never_passes_the_bound: the deterministic sequence meets every kind by case {:?}",
            first.get()
        );
    }
}
