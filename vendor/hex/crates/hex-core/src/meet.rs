//! How many cases a property test runs: the fewest that meet each kind of case it counts at least once with
//! probability [`MEET`], from the kind's share of the generated cases, `(1 - share)^n <= 1 - MEET`, as step
//! 7's suite sizes its runs (0.95^60). The shares are measured (a recorder test) or follow from the
//! strategy exactly; neither count is picked.

/// The chance a property test meets each kind of case it counts.
pub(crate) const MEET: f64 = 0.95;

/// The fewest cases that meet a kind of case of share `share` with probability [`MEET`].
pub(crate) fn cases_to_meet(share: f64) -> u32 {
    // A kind never met can't be sized for: the most cases a run can name.
    if share <= 0.0 {
        return u32::MAX;
    }
    let (mut n, mut missed) = (0u32, 1.0_f64);
    while missed > 1.0 - MEET {
        missed *= 1.0 - share;
        n = n.saturating_add(1);
    }
    n
}

/// A runner for `cases` cases drawn from proptest's deterministic RNG, so every run checks the same cases. A
/// run sized to meet each kind with probability [`MEET`] under a fresh seed would miss one in up to
/// `1 - MEET` of runs, and the test's non-vacuity check would fail them spuriously (proptest names this case
/// for `TestRng::deterministic_rng`); drawn deterministically, the derived count meets each kind every run.
pub(crate) fn sized_runner(cases: u32) -> proptest::test_runner::TestRunner {
    let config = proptest::test_runner::Config::with_cases(cases);
    let rng = proptest::test_runner::TestRng::deterministic_rng(config.rng_algorithm);
    proptest::test_runner::TestRunner::new_with_rng(config, rng)
}

/// Do size a run for a share of 5%; expect 59 cases (0.95^59 = 4.8% <= 5% < 0.95^58), the arithmetic step 7
/// cites for its sixty runs.
#[test]
fn a_five_percent_kind_takes_fifty_nine_cases() {
    assert_eq!(cases_to_meet(0.05), 59);
}
