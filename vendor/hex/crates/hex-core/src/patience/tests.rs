//! The learned wait's tests (decision 9; Ada's 2026-11-06 rule and the lead's failure-bound rule; AC-T7.1.1,
//! AC-T7.1.3: every wait a node makes is bounded and learned). Against random records and answers, the wait is twice
//! the larger of the record's slowest and the slowest seen, never shrinks, and lets through every answer no more
//! than twice slower than those, and no other; a measuring wait is never armed, its answers only recorded.

use std::time::Duration;

use proptest::prelude::*;

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

proptest! {
    /// Do: seed a wait from a random record (or none, measuring), record random answers, then ask whether a new
    /// answer would come within it. Expect: a seeded wait, after each answer, is twice the larger of the record's
    /// slowest and the slowest so far, never less than before, and a new answer comes within it exactly when it is at
    /// most that; a measuring one is never armed, while it keeps the slowest it has seen.
    #[test]
    fn the_wait_is_twice_the_slowest_recorded_or_seen(
        record in proptest::option::of(0u64..86_400_000_000_000),
        answers in prop::collection::vec(0u64..86_400_000_000_000, 1..100),
        next in 0u64..200_000_000_000_000,
    ) {
        let patience = match record {
            Some(ns) => Patience::seeded("test", Duration::from_nanos(ns)),
            None => Patience::measuring("test"),
        };
        let mut slowest = record.map(Duration::from_nanos);
        let mut before = patience.deadline();
        for &ns in &answers {
            let answer = Duration::from_nanos(ns);
            patience.answered(answer);
            slowest = Some(slowest.map_or(answer, |s| s.max(answer)));
            let got = patience.deadline();
            prop_assert_eq!(patience.slowest(), slowest);
            if record.is_none() {
                prop_assert_eq!(got, None);
                continue;
            }
            prop_assert_eq!(got, slowest.map(|s| s * 2));
            prop_assert!(got >= before, "the wait shrank from {:?} to {:?}", before, got);
            before = got;
        }
        if record.is_some() {
            let next = Duration::from_nanos(next);
            prop_assert_eq!(next <= patience.deadline().unwrap(), next <= slowest.unwrap() * 2);
        }
    }
}

/// The seed. Do: seed a wait from a record whose slowest answer is 1.918 ms, and ask before any answer. Expect:
/// the bound armed at 3.836 ms, with no calibration read.
#[test]
fn before_any_answer_the_bound_is_twice_the_records_slowest() {
    let p = Patience::seeded("test", Duration::from_micros(1_918));
    assert_eq!(p.deadline(), Some(Duration::from_micros(3_836)));
}

/// The record is a floor the node's answers only raise. Do: seed at 40 ms, answer 200 times in 1 ms, then once in
/// 50 ms. Expect: the wait stays 80 ms through the fast answers, then 100 ms.
#[test]
fn fast_answers_never_shrink_the_wait_and_a_slower_one_raises_it() {
    let p = Patience::seeded("test", ms(40));
    for _ in 0..200 {
        p.answered(ms(1));
    }
    assert_eq!(p.deadline(), Some(ms(80)));
    p.answered(ms(50));
    assert_eq!(p.deadline(), Some(ms(100)));
}

/// Measuring (the waits census, run 2026-11-08: a listing armed by its first fast answer refused the next one, so
/// the census could not measure). Do: ask a wait with no record and no answer, then answer it in 3 ms and in 1 ms.
/// Expect: never armed, while its slowest, 3 ms, is kept for the census.
#[test]
fn a_measuring_wait_is_never_armed() {
    let p = Patience::measuring("test");
    assert_eq!((p.deadline(), p.slowest()), (None, None));
    p.answered(ms(3));
    p.answered(ms(1));
    assert_eq!((p.deadline(), p.slowest()), (None, Some(ms(3))));
}

/// Shared by threads. Do: from eight threads, answer 1 to 800 ms between them. Expect: the slowest, 800 ms, kept.
#[test]
fn answers_from_many_threads_keep_the_slowest() {
    let p = Patience::measuring("test");
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let p = &p;
            s.spawn(move || {
                for n in 0..100u64 {
                    p.answered(ms(t * 100 + n + 1));
                }
            });
        }
    });
    assert_eq!(p.slowest(), Some(ms(800)));
}

/// The census (the waits census probe, `bench/competitive/probes/waits_census.md`): every answer a wait records, and
/// every time it runs out, reaches the installed sink by the wait's name, from the wait's own timing point; a run-out
/// is no sample (Karn's point), told apart from the answers, but the least the slow answer took, which the wait takes
/// as its slowest from then (decision 9; the lead, 2026-11-09; RFC 6298 §5.5). Do: install a sink, answer a wait named
/// `census.test` in 7 ms and 9 ms, and say it ran out at 20 ms. Expect: the sink told the two answers and the
/// run-out, by that name, and the wait's slowest 20 ms.
#[test]
fn every_answer_and_run_out_reaches_the_census_by_its_waits_name() {
    static TOLD: std::sync::Mutex<Vec<(&'static str, Sample)>> = std::sync::Mutex::new(Vec::new());
    fn sink(name: &'static str, sample: Sample) {
        TOLD.lock().unwrap().push((name, sample));
    }
    assert!(census(sink), "the first install takes");
    let p = Patience::seeded("census.test", ms(1));
    p.answered(ms(7));
    p.answered(ms(9));
    p.ran_out(ms(20));
    let told: Vec<Sample> = TOLD
        .lock()
        .unwrap()
        .iter()
        .filter(|(name, _)| *name == "census.test")
        .map(|(_, sample)| *sample)
        .collect();
    assert_eq!(
        told,
        [
            Sample::Answered(ms(7)),
            Sample::Answered(ms(9)),
            Sample::RanOut(ms(20))
        ]
    );
    assert_eq!(
        p.slowest(),
        Some(ms(20)),
        "a run-out is no sample, but its answer is slower than its bound"
    );
}

/// The census's file form, which `waits_census.py` reads: an answer `name nanos`, a run-out `name >nanos`.
#[test]
fn an_answer_and_a_run_out_are_census_lines_of_their_own() {
    assert_eq!(
        census_line("a.wait", Sample::Answered(Duration::from_nanos(41))),
        "a.wait 41\n"
    );
    assert_eq!(
        census_line("a.wait", Sample::RanOut(ms(440))),
        "a.wait >440000000\n"
    );
}

/// Seeds by build profile (the lead, 2026-11-08: a debug build reads the debug record; production builds carry only
/// release seeds). Do: in this build, a debug one, make a wait whose release record is 5 ms and whose debug record
/// is 8 ms, and one with no debug record. Expect: the first armed at twice the debug record, 16 ms; the second
/// measuring, never armed.
#[cfg(debug_assertions)]
#[test]
fn a_debug_build_reads_the_debug_record() {
    let p = Patience::by_profile("test", ms(5), Some(ms(8)));
    assert_eq!(p.deadline(), Some(ms(16)));
    let q = Patience::by_profile("test", ms(5), None);
    q.answered(ms(1));
    assert_eq!(q.deadline(), None);
}

/// AC-T7.4.1: typed refusal census identity must not alter the existing online learning rule.
#[test]
fn refusal_census_preserves_learning_and_marks_only_the_optional_record() {
    let answered = Patience::seeded("outcome.answer", ms(1));
    let refused = Patience::seeded("outcome.refused", ms(1));
    for took in [ms(7), ms(3), ms(12)] {
        answered.answered(took);
        refused.refused(took);
        assert_eq!(answered.slowest(), refused.slowest());
        assert_eq!(answered.deadline(), refused.deadline());
    }
    assert_eq!(refused.slowest(), Some(ms(12)));
    assert_eq!(refused.deadline(), Some(ms(24)));
    let measuring = Patience::measuring("outcome.measuring");
    measuring.refused(ms(9));
    assert_eq!(measuring.slowest(), Some(ms(9)));
    assert_eq!(measuring.deadline(), None);
    assert_eq!(
        census_line("a.wait", Sample::Refused(Duration::from_nanos(41))),
        "a.wait !41\n"
    );
    assert_eq!(
        census_line("a.wait", Sample::Answered(Duration::from_nanos(41))),
        "a.wait 41\n"
    );
    assert_eq!(
        census_line("a.wait", Sample::RanOut(Duration::from_nanos(41))),
        "a.wait >41\n"
    );
}

/// AC-T7.4.1 (RFC 6298 §5.5, Karn's algorithm: a run-out is no sample, and the timer backs off): a wait that ran
/// out at its bound proves its answer slower than that bound, so this process's next wait is longer, not the same
/// bound failing every slow call until a new record lands. Do: seed a wait at 5 ms, so bounded at 10 ms, and say
/// it ran out at that bound; then again at the next. Expect: each next bound past the one that ran out, twice it
/// (20 ms, then 40 ms); a measuring wait's run-out arms nothing.
#[test]
fn a_wait_that_ran_out_waits_longer_next() {
    let p = Patience::seeded("test", ms(5));
    let bound = p.deadline().unwrap();
    p.ran_out(bound);
    let next = p.deadline().unwrap();
    p.ran_out(next);
    let after = p.deadline().unwrap();
    assert!(
        next > bound,
        "the next wait {next:?} after one ran out at {bound:?}"
    );
    assert_eq!((bound, next, after), (ms(10), ms(20), ms(40)));
    let measuring = Patience::measuring("test");
    measuring.ran_out(ms(7));
    assert_eq!(measuring.deadline(), None);
}

/// AC-T7.4.1 (decision 9, by build profile): a debug build's answers are no faster than a release build's under the
/// same conditions, so the release record's cohorts bound a debug build's slowest from below, and a debug seed is never
/// under its release one. Do: in this build, a debug one, make a wait whose release record is 3.022291 ms and whose
/// debug record is 295.291 us (macOS's `client.guest.answer.hello`: the release campaign met cold first hellos the debug
/// one never did, and a suite's hello ran out at the debug seed's 590.582 us bound), and one whose debug record is the
/// larger. Expect: the first armed at twice the release record, 6.044582 ms; the second at twice its debug record.
#[cfg(debug_assertions)]
#[test]
fn a_debug_seed_is_never_under_its_release_record() {
    let hello = Patience::by_profile(
        "test",
        Duration::from_nanos(3_022_291),
        Some(Duration::from_nanos(295_291)),
    );
    assert_eq!(hello.deadline(), Some(Duration::from_nanos(6_044_582)));
    let slower = Patience::by_profile("test", ms(5), Some(ms(7)));
    assert_eq!(slower.deadline(), Some(ms(14)));
}
