//! The capacity view under the nemesis (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1).
//! A bounded set of seeds here; ROOT runs the sweeps (`run` over more seeds and runners).

use super::*;

fn total(all: Report, r: Report) -> Report {
    Report {
        steps: all.steps + r.steps,
        hex_started: all.hex_started + r.hex_started,
        hex_refused: all.hex_refused + r.hex_refused,
        runs_granted: all.runs_granted + r.runs_granted,
        runs_refused: all.runs_refused + r.runs_refused,
        runs_unanswered: all.runs_unanswered + r.runs_unanswered,
        link_drops: all.link_drops + r.link_drops,
        keeper_crashes: all.keeper_crashes + r.keeper_crashes,
        crashes_at_write: all.crashes_at_write + r.crashes_at_write,
        runner_crashes: all.runner_crashes + r.runner_crashes,
        released_at_sync: all.released_at_sync + r.released_at_sync,
        adopted_at_sync: all.adopted_at_sync + r.adopted_at_sync,
        forgotten: all.forgotten + r.forgotten,
        wrongly_forgotten: all.wrongly_forgotten + r.wrongly_forgotten,
    }
}

fn seeds(runners: usize, seeds: u64, steps: u64) -> Report {
    (0..seeds).fold(Report::default(), |all, seed| {
        let r = run(seed, runners, steps, None)
            .unwrap_or_else(|v| panic!("{runners} runners, seed {seed}: {v:?}"));
        total(all, r)
    })
}

/// AC-T7.4.1, joint plan step 13: do run one machine, Hex's own boxes beside two kiln runners on its
/// keeper, for 64 seeds of 2,000 steps under the nemesis (the keeper crashing between messages and at
/// its writes, before and after they land; runners crashing, their runs outliving them or not; links
/// dropped with what was on them, partitioned and healed; waits run out; a runner retired and released
/// by the person); expect the work running never past the room and every running run in the keeper's
/// books at every step, and once settled the keeper's books, each runner's and what it runs the same;
/// and (non-vacuity) each fault struck, both sides refused for room, syncs released what ended, and a sync
/// adopted work the person had released while its runner was away (CV6).
#[test]
fn no_oversubscription_and_the_books_agree_across_crash_seeds() {
    let all = seeds(2, 64, 2_000);
    eprintln!("2 runners, 64 seeds of 2,000 steps: {all:?}");
    assert!(
        all.hex_started > 0
            && all.hex_refused > 0
            && all.runs_granted > 0
            && all.runs_refused > 0
            && all.runs_unanswered > 0
            && all.link_drops > 0
            && all.keeper_crashes > 0
            && all.crashes_at_write > 0
            && all.runner_crashes > 0
            && all.released_at_sync > 0
            && all.forgotten > 0
            && all.wrongly_forgotten > 0
            && all.adopted_at_sync > 0,
        "{all:?}"
    );
}

/// AC-T7.4.1, joint plan step 13: do the same with one runner and with three, 32 seeds each; expect the
/// same.
#[test]
fn one_runner_or_three_keep_the_same() {
    for runners in [1, 3] {
        let all = seeds(runners, 32, 2_000);
        assert!(
            all.runs_granted > 0 && all.keeper_crashes > 0 && all.link_drops > 0,
            "{runners}: {all:?}"
        );
    }
}

/// Runs 64 seeds with `mutant` planted: the first violation.
fn caught(mutant: Mutant) -> Option<Violation> {
    let found = (0..64).find_map(|seed| run(seed, 2, 2_000, Some(mutant)).err().map(|v| (seed, v)));
    eprintln!("{mutant:?}: {found:?}");
    found.map(|(_, v)| v)
}

/// Non-vacuity, CV5: do plant a keeper's shell that releases a runner's reservations when its link
/// drops; expect some seed to run a run the keeper's books don't hold.
#[test]
fn releasing_on_silence_is_caught() {
    let found = caught(Mutant::ReleaseOnSilence);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV5 (a holder's first obligation): do plant a runner that starts a run when it asks;
/// expect some seed to run an unbooked run or past the room.
#[test]
fn starting_before_the_grant_is_caught() {
    let found = caught(Mutant::StartBeforeGrant);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV5 (a holder's third obligation): do plant a runner whose next process lists none of
/// the runs that outlived it; expect some seed to run a run the keeper released.
#[test]
fn forgetting_survivors_is_caught() {
    let found = caught(Mutant::ForgetSurvivors);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV2: do plant a keeper that answers before its write lands, its crash losing the write;
/// expect some seed to run a run the restarted keeper doesn't hold.
#[test]
fn answering_before_the_write_is_caught() {
    let found = caught(Mutant::AnswerBeforeWrite);
    assert!(found.is_some(), "no seed caught it");
}

/// Non-vacuity, CV3: do plant Hex admitting against the room less its own boxes alone; expect some seed
/// past the room.
#[test]
fn hex_ignoring_the_holders_is_caught() {
    let found = caught(Mutant::LocalIgnoresForeign);
    assert!(
        matches!(found, Some(Violation::Oversubscribed { .. })),
        "{found:?}"
    );
}

/// Non-vacuity, CV5: do plant a keeper's shell that drops from a sync what the keeper doesn't hold, which
/// its sync would adopt after the person's wrong release; expect some seed to run a run the keeper's books
/// lack once its runner has synced, or that runner told the counts disagree.
#[test]
fn dropping_what_the_keeper_lacks_at_a_sync_is_caught() {
    let found = caught(Mutant::DropUnknownAtSync);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::SyncDisagreed { .. })
        ),
        "{found:?}"
    );
}

/// T-T7.4.1's determinism: one seed, one run.
#[test]
fn a_seed_names_one_run() {
    assert_eq!(run(7, 2, 2_000, None), run(7, 2, 2_000, None));
}

/// A sweep's size from the environment, for the lead's record runs: `name`'s count, else the bounded suite's.
fn sized(name: &str, bounded: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(bounded)
}

/// AC-T7.4.1, joint plan step 13: the record sweep, the lead's, in release: `HEX_CAPACITY_SEEDS` seeds (the bounded
/// suite's 64 unless set) of `HEX_CAPACITY_STEPS` steps (2,000) with one, two and three runners, then each planted
/// mistake over the same seeds with two. Expect every seed to hold, and each mistake caught, with how many seeds of
/// how many caught it and the first.
#[test]
#[ignore = "the record sweep, sized by HEX_CAPACITY_SEEDS and HEX_CAPACITY_STEPS, in release (the lead's)"]
fn the_record_sweep() {
    let (n, steps) = (
        sized("HEX_CAPACITY_SEEDS", 64),
        sized("HEX_CAPACITY_STEPS", 2_000),
    );
    for runners in [1, 2, 3] {
        let all = seeds(runners, n, steps);
        eprintln!("{runners} runners, {n} seeds of {steps} steps: {all:?}");
    }
    for mutant in [
        Mutant::ReleaseOnSilence,
        Mutant::StartBeforeGrant,
        Mutant::ForgetSurvivors,
        Mutant::AnswerBeforeWrite,
        Mutant::LocalIgnoresForeign,
        Mutant::DropUnknownAtSync,
    ] {
        let caught: Vec<u64> = (0..n)
            .filter(|seed| run(*seed, 2, steps, Some(mutant)).is_err())
            .collect();
        eprintln!(
            "{mutant:?}: caught on {} of {n} seeds, first {:?}",
            caught.len(),
            caught.first()
        );
        assert!(!caught.is_empty(), "{mutant:?} caught on no seed of {n}");
    }
}
