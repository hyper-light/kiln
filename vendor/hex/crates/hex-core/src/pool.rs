//! The macOS box users' pool, grown on demand (native plan step 2, the lead's call 2026-11-06 under Ada's
//! rule: no picked pool size). The privileged helper makes a box user only when a box needs one and every
//! user it has is busy. The account it makes comes from this core alone, never from its caller: the name
//! after the highest index the pool holds (`_hexbox<n>`), at the first uid in Apple's role-account range
//! that no user, group or process holds. Other installers (Nix) share that range, so the helper takes no
//! more ids than it needs. Pure (CLAUDE.md §9): the helper reads the directory and makes the account.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

/// Apple's role-account uids: `sysadminctl`'s usage on macOS 26.4, "Role accounts require name starting with
/// _ and UID in 450-499 range" (docs/research/runtime/installer-per-os.md §3.1).
pub const ROLE_UIDS: RangeInclusive<u32> = 450..=499;

/// Every pool user's name is this and its index: `_hexbox1`, `_hexbox2`, …
pub const POOL_PREFIX: &str = "_hexbox";

/// An account the helper makes: its name and uid, the rest of its attributes the helper's fixed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
}

/// The index a pool user's name holds: `_hexbox<n>`, `n` a decimal from 1 with no leading zero that fits a
/// `u32`; `None` for any other name.
pub fn pool_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix(POOL_PREFIX)?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The next account to make for a pool whose users are named `pool`: the name after the highest pool index
/// among them, at the first role uid not in `taken` (every user's uid, group's gid and running process's
/// uid the helper read just now). `None` when every role uid is taken, or the index can't grow.
pub fn next_account(pool: &[String], taken: &BTreeSet<u32>) -> Option<Account> {
    let index = pool
        .iter()
        .filter_map(|n| pool_index(n))
        .max()
        .unwrap_or(0)
        .checked_add(1)?;
    let uid = ROLE_UIDS.clone().find(|u| !taken.contains(u))?;
    Some(Account {
        name: format!("{POOL_PREFIX}{index}"),
        uid,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use proptest::prelude::*;
    use proptest::test_runner::{TestCaseError, TestRng, TestRunner};

    use super::*;
    use crate::meet::cases_to_meet;

    /// The rarest kind's share of [`an_account_is_always_in_range_untaken_and_new`]'s cases, as measured by
    /// `records_the_rarest_case_shares` (bench/native/macos/records/case-shares-pool.txt).
    const RAREST_SHARE: f64 = 0.08403;
    /// The case by which proptest's deterministic sequence has met every kind, as the recorder measured it: a
    /// fixed sequence meets each kind every run or never, so the test runs at least this many (step 2's
    /// finding, 7785ad3).
    const DETERMINISTIC_FIRST_MEET: u32 = 19;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The role range is Apple's own: `sysadminctl`'s usage on macOS 26.4, "Role accounts require name
    /// starting with _ and UID in 450-499 range" (docs/research/runtime/installer-per-os.md §3.1).
    #[test]
    fn the_role_range_is_apples() {
        assert_eq!(ROLE_UIDS, 450..=499);
        assert_eq!(POOL_PREFIX, "_hexbox");
    }

    /// HEX-MAC-16: only `_hexbox<n>`, n a decimal from 1 with no leading zero, is a pool name. Do read
    /// pool and other names; expect the index of the first, nothing for the rest.
    #[test]
    fn only_hexbox_and_an_index_is_a_pool_name() {
        assert_eq!(pool_index("_hexbox1"), Some(1));
        assert_eq!(pool_index("_hexbox42"), Some(42));
        for other in [
            "_hexbox",
            "_hexbox0",
            "_hexbox01",
            "_hexboxa",
            "_hexbox1x",
            "hexbox1",
            "_nixbld1",
            "_hexbox-1",
            "_hexbox 1",
            "_hexbox99999999999",
        ] {
            assert_eq!(pool_index(other), None, "{other}");
        }
    }

    /// HEX-MAC-16: the next account follows the pool's highest index, at the first role uid nobody holds.
    /// Do ask with gaps in the names and taken ids; expect `_hexbox4` at 452, skipping 450 and 451.
    #[test]
    fn the_next_account_follows_the_pool_at_the_first_free_role_uid() {
        let taken = BTreeSet::from([450, 451, 9000]);
        assert_eq!(
            next_account(&names(&["_hexbox1", "_hexbox3", "_nixbld9"]), &taken),
            Some(Account {
                name: "_hexbox4".to_owned(),
                uid: 452
            })
        );
        assert_eq!(
            next_account(&[], &BTreeSet::new()),
            Some(Account {
                name: "_hexbox1".to_owned(),
                uid: 450
            })
        );
    }

    /// CLAUDE.md §2, §3: a full role range is a refusal, not an id outside it. Do take every role uid;
    /// expect no account.
    #[test]
    fn a_full_role_range_makes_no_account() {
        let taken: BTreeSet<u32> = ROLE_UIDS.collect();
        assert_eq!(next_account(&names(&["_hexbox1"]), &taken), None);
    }

    /// The kinds of case [`an_account_is_always_in_range_untaken_and_new`] counts (non-vacuity, CLAUDE.md
    /// §6). A full role range, which its generator all but never makes, is pinned by
    /// [`a_full_role_range_makes_no_account`].
    #[derive(Default)]
    struct Met {
        follows_a_pool_name: Cell<u64>,
        first_of_a_pool: Cell<u64>,
        skips_a_taken_uid: Cell<u64>,
        takes_the_first_uid: Cell<u64>,
    }

    impl Met {
        fn kinds(&self) -> [(&'static str, u64); 4] {
            [
                ("a pool holding a pool name", self.follows_a_pool_name.get()),
                ("a pool holding none", self.first_of_a_pool.get()),
                ("the first role uid taken", self.skips_a_taken_uid.get()),
                ("the first role uid free", self.takes_the_first_uid.get()),
            ]
        }
    }

    fn arb_case() -> impl Strategy<Value = (Vec<String>, BTreeSet<u32>)> {
        (
            proptest::collection::vec("_hexbox[0-9]{1,3}|_[a-z]{1,8}[0-9]{0,2}|[a-z]{1,6}", 0..40),
            proptest::collection::btree_set(400u32..520, 0..80),
        )
    }

    /// One case of [`an_account_is_always_in_range_untaken_and_new`], counting the kinds it met.
    fn account_holds(
        pool: &[String],
        taken: &BTreeSet<u32>,
        met: &Met,
    ) -> Result<(), TestCaseError> {
        let bump = |c: &Cell<u64>| c.set(c.get() + 1);
        match next_account(pool, taken) {
            Some(a) => {
                prop_assert!(ROLE_UIDS.contains(&a.uid));
                prop_assert!(!taken.contains(&a.uid));
                prop_assert!(pool_index(&a.name).is_some());
                prop_assert!(!pool.contains(&a.name));
            }
            None => prop_assert!(
                ROLE_UIDS.clone().all(|u| taken.contains(&u))
                    || pool
                        .iter()
                        .filter_map(|n| pool_index(n))
                        .any(|i| i == u32::MAX)
            ),
        }
        if pool.iter().any(|n| pool_index(n).is_some()) {
            bump(&met.follows_a_pool_name);
        } else {
            bump(&met.first_of_a_pool);
        }
        if taken.contains(ROLE_UIDS.start()) {
            bump(&met.skips_a_taken_uid);
        } else {
            bump(&met.takes_the_first_uid);
        }
        Ok(())
    }

    /// HEX-MAC-16, model-based (CLAUDE.md §6): whatever the pool's names and whatever ids are taken, an
    /// account made is in the role range, untaken, a pool name, and new to the pool; and none is made only
    /// when every role uid is taken. It runs as many cases as meet each kind it counts with probability 0.95
    /// (hex_core::meet), sized from the rarest kind's share as `records_the_rarest_case_shares` measured it,
    /// drawn from proptest's deterministic RNG, so every run checks the same cases and the check that they met
    /// every kind is a fact of the code, not a draw.
    #[test]
    fn an_account_is_always_in_range_untaken_and_new() {
        let met = Met::default();
        // proptest's deterministic RNG, not a fresh seed a run: a random draw sized to meet each kind with
        // probability 0.95 misses one in up to 5% of runs, and the non-vacuity check below would fail them
        // spuriously (proptest's own note on `TestRng::deterministic_rng`).
        let config =
            ProptestConfig::with_cases(cases_to_meet(RAREST_SHARE).max(DETERMINISTIC_FIRST_MEET));
        let rng = TestRng::deterministic_rng(config.rng_algorithm);
        let mut runner = TestRunner::new_with_rng(config, rng);
        runner
            .run(&arb_case(), |(pool, taken)| {
                account_holds(&pool, &taken, &met)
            })
            .unwrap();
        for (kind, n) in met.kinds() {
            assert!(n > 0, "the cases met no {kind}");
        }
    }

    /// Records the share of each kind of case [`an_account_is_always_in_range_untaken_and_new`]'s generator
    /// makes, and the case by which proptest's deterministic sequence has met every kind
    /// (bench/native/macos/records/case-shares-pool.txt). Ignored: a measurement, not a check.
    #[test]
    #[ignore = "a measurement; run by hand and recorded in bench/native/macos/records"]
    fn records_the_rarest_case_shares() {
        // Enough to know a share near 0.02 to about 2.2% relative: its standard error, sqrt((1 - p) / (n p)).
        const RECORD_CASES: u32 = 100_000;
        let met = Met::default();
        let mut runner = TestRunner::new(ProptestConfig::with_cases(RECORD_CASES));
        runner
            .run(&arb_case(), |(pool, taken)| {
                account_holds(&pool, &taken, &met)
            })
            .unwrap();
        let total: u64 = RECORD_CASES.into();
        let test = "an_account_is_always_in_range_untaken_and_new";
        for (kind, n) in met.kinds() {
            println!(
                "{test}: {kind}: {n} of {total} ({:.5})",
                n as f64 / total as f64
            );
        }
        let rarest = met.kinds().iter().map(|(_, n)| *n).min().unwrap_or(0);
        let share = rarest as f64 / total as f64;
        println!(
            "{test}: rarest share {share:.5} -> {} cases",
            cases_to_meet(share)
        );
        let seen = Met::default();
        let case = Cell::new(0u32);
        let first_meet = Cell::new(None);
        let config = ProptestConfig::with_cases(RECORD_CASES);
        let rng = TestRng::deterministic_rng(config.rng_algorithm);
        TestRunner::new_with_rng(config, rng)
            .run(&arb_case(), |(pool, taken)| {
                case.set(case.get() + 1);
                account_holds(&pool, &taken, &seen)?;
                if first_meet.get().is_none() && seen.kinds().iter().all(|(_, n)| *n > 0) {
                    first_meet.set(Some(case.get()));
                }
                Ok(())
            })
            .unwrap();
        let first_meet = first_meet.get();
        println!("{test}: the deterministic sequence meets every kind by case {first_meet:?}");
    }
}
