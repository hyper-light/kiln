//! The tests' waits (decision 9; the person's ruling of 2026-11-13, R03 for tests: a wait for a fact waits for the
//! answer): a fact true at once, one past its learned W* waited for, a call that takes its own deadline given none, one
//! a measuring patience waits for however long it takes, and the census of the tests' own waits, which records them
//! and disarms nothing.

use super::*;
use std::process::Command;

/// Do: wait for a fact already true. Expect: one look, its answer seen by the wait.
#[test]
fn a_fact_already_true_takes_one_look_and_is_recorded() {
    let wait = Patience::measuring("test.testkit.at_once");
    assert_eq!(until(&wait, "a fact already true", || true), Ok(1));
    assert!(wait.slowest().is_some(), "the answer was recorded");
}

/// R03 for the tests' waits (decision 9, the person's ruling of 2026-11-13; ROOT's gate, 2026-11-13): a wait for a fact
/// waits for the answer, refusing nothing at its learned W*. Do: wait with a patience that has seen a nanosecond's
/// answer for a fact that comes at the thousandth look. Expect: the thousand looks, never refused; the run-out told on
/// the way, then the answer its sample, W* risen to it.
#[test]
fn original_a_fact_past_its_learned_w_star_is_waited_for() {
    let learned = Duration::from_nanos(1);
    let wait = Patience::measuring("test.testkit.past");
    wait.answered(learned);
    let mut looks = 0;
    assert_eq!(
        until(&wait, "a fact at the thousandth look", || {
            looks += 1;
            looks == 1_000
        }),
        Ok(1_000)
    );
    assert!(wait.slowest().is_some_and(|slowest| slowest > learned));
}

/// Do: wait with a measuring patience for a fact that comes at the hundredth look. Expect: the hundred looks, never
/// refused: a measuring wait has no deadline, only its fact.
#[test]
fn a_measuring_wait_waits_for_its_fact_alone() {
    let wait = Patience::measuring("test.testkit.late");
    let mut looks = 0;
    assert_eq!(
        until(&wait, "a fact at the hundredth look", || {
            looks += 1;
            looks == 100
        }),
        Ok(100)
    );
}

/// AC-T7.4.1, R03 for the tests' waits (decision 9, the person's ruling of 2026-11-13): a call that takes its own
/// deadline is given none, so it waits for its answer, and a W* it passes refuses nothing. Do: wait through such a
/// call with a measuring patience, then with one that has seen a nanosecond's answer, the call answering past it.
/// Expect: each call given no deadline; each answer recorded, W* risen to the slower one; the run-out past W* told.
#[test]
fn original_a_call_that_takes_its_own_deadline_is_given_none() {
    let measured = Patience::measuring("test.testkit.within");
    assert_eq!(within(&measured, |deadline| deadline), None);
    assert!(measured.slowest().is_some(), "the answer was recorded");
    let learned = Duration::from_nanos(1);
    let past = Patience::measuring("test.testkit.within_past");
    past.answered(learned);
    let given = within(&past, |deadline| {
        let began = Instant::now();
        while began.elapsed() <= learned {
            std::thread::yield_now();
        }
        deadline
    });
    assert_eq!(
        given, None,
        "a deadline was given past which the call would end"
    );
    assert!(past.slowest().is_some_and(|slowest| slowest > learned));
}

/// The census's child: run only by [`the_census_records_a_tests_waits_and_disarms_none`], in a process of its own,
/// with the census's variables set. It waits once on a measuring fact, and once on a fact that comes only at the
/// millionth look with a patience of a nanosecond's record: waited for past its bound, its run-out told.
#[test]
#[ignore = "the census test's child, run in a process of its own"]
fn census_child() {
    let measured = Patience::measuring("test.testkit.census");
    assert_eq!(until(&measured, "the census's fact", || true), Ok(1));
    let bounded = Patience::seeded("test.testkit.bounded", Duration::from_nanos(1));
    let mut looks = 0;
    let past = until(&bounded, "a fact past its bound", || {
        looks += 1;
        looks == 1_000_000
    });
    assert_eq!(past, Ok(1_000_000));
}

/// Do: run [`census_child`] with `HEX_WAITS_CENSUS` naming a file and `HEX_WAITS_CENSUS_MAX` set. Expect: the file
/// holds the measured wait's answer as the census keeps it, `name nanos`, the seeded wait's run-out at its bound,
/// `name >nanos`, and then its answer, waited for past the bound (R03 for the tests' waits, decision 9): what ran out
/// is told, a censored sample, and its answer comes in uncensored.
#[test]
fn the_census_records_a_tests_waits_and_disarms_none() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("census-suites.txt");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "waits::tests::census_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CENSUS_FILE, &file)
        .env(CENSUS_MAX, "10")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the child failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text:?}");
    let (name, nanos) = lines[0].split_once(' ').unwrap();
    assert_eq!(name, "test.testkit.census");
    assert!(nanos.parse::<u64>().is_ok(), "{text:?}");
    // Its bound, a record of a nanosecond itself (decision 9: no margin), run out and told; then its answer, waited for.
    assert_eq!(lines[1], "test.testkit.bounded >1", "{text:?}");
    let (name, nanos) = lines[2].split_once(' ').unwrap();
    assert_eq!(name, "test.testkit.bounded");
    assert!(nanos.parse::<u64>().is_ok(), "{text:?}");
}
