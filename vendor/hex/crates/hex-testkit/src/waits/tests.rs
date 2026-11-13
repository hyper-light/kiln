//! The tests' waits (decision 9; the lead's rule of 2026-11-08: every wait in the tests uses its own record's
//! patience): a fact true at once, one that never comes within a seeded patience, one a measuring patience waits for
//! however long it takes, and the census of the tests' own waits, which records them and disarms nothing.

use super::*;
use std::process::Command;

/// Do: wait for a fact already true. Expect: one look, its answer seen by the wait.
#[test]
fn a_fact_already_true_takes_one_look_and_is_recorded() {
    let wait = Patience::measuring("test.testkit.at_once");
    assert_eq!(until(&wait, "a fact already true", || true), Ok(1));
    assert!(wait.slowest().is_some(), "the answer was recorded");
}

/// Do: wait for a fact that never comes, with a patience seeded from a record of a nanosecond. Expect: refused once
/// its deadline, twice the record, has passed, naming the fact and that patience; no answer recorded (Karn's point),
/// but the fact is slower than that patience, so the wait's slowest is the patience and its next deadline twice it
/// (AC-T7.4.1: `Patience::ran_out` backs off in process, RFC 6298 §5.5).
#[test]
fn a_fact_that_never_comes_is_refused_at_twice_its_record() {
    let wait = Patience::seeded("test.testkit.never", Duration::from_nanos(1));
    let patience = wait.deadline().unwrap();
    assert_eq!(
        until(&wait, "a fact that never comes", || false),
        Err(Waited {
            what: "a fact that never comes".to_owned(),
            patience,
        })
    );
    assert_eq!(
        wait.slowest(),
        Some(patience),
        "no answer recorded, only the least the run-out proved"
    );
    assert_eq!(wait.deadline(), patience.checked_mul(2), "backed off");
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

/// AC-T7.4.1. Do: wait through a call that takes its own deadline, with a measuring patience, and with a patience
/// seeded from a record of a nanosecond for a call that answers only once its deadline has passed. Expect: the
/// measuring call given no deadline and its answer recorded; the seeded call given its deadline, twice the record from
/// its start, and no answer recorded (Karn's point), only the least the run-out proved, so its next deadline doubles.
#[test]
fn a_wait_that_takes_its_own_deadline_is_given_its_patience() {
    let measured = Patience::measuring("test.testkit.within");
    assert_eq!(within(&measured, |deadline| deadline), None);
    assert!(measured.slowest().is_some(), "the answer was recorded");
    let seeded = Patience::seeded("test.testkit.within_bounded", Duration::from_nanos(1));
    let patience = seeded.deadline().unwrap();
    let before = Instant::now();
    let given = within(&seeded, |deadline| {
        let at = deadline.unwrap();
        while Instant::now() < at {
            std::thread::yield_now();
        }
        at
    });
    assert!(given >= before + patience, "given {given:?}");
    assert_eq!(
        seeded.slowest(),
        Some(patience),
        "no answer recorded, only the least the run-out proved"
    );
    assert_eq!(seeded.deadline(), patience.checked_mul(2), "backed off");
}

/// The census's child: run only by [`the_census_records_a_tests_waits_and_disarms_none`], in a process of its own,
/// with the census's variables set. It waits once on a measuring fact, and once on a fact that comes only at the
/// millionth look with a patience of a nanosecond's record: refused at its bound, unless the census disarmed it.
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
    assert!(past.is_err(), "the bounded wait was disarmed: {past:?}");
}

/// Do: run [`census_child`] with `HEX_WAITS_CENSUS` naming a file and `HEX_WAITS_CENSUS_MAX` set. Expect: the file
/// holds the measured wait's answer as the census keeps it, `name nanos`, and the seeded wait's run-out at its bound,
/// `name >nanos`: the seeded wait still refused past its bound, so the tests' census disarms none of their waits, and
/// what ran out is told, a censored sample the census's seed rule takes (decision 9; the lead, 2026-11-09).
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
    assert_eq!(lines.len(), 2, "{text:?}");
    let (name, nanos) = lines[0].split_once(' ').unwrap();
    assert_eq!(name, "test.testkit.census");
    assert!(nanos.parse::<u64>().is_ok(), "{text:?}");
    // Its bound, twice a record of a nanosecond.
    assert_eq!(lines[1], "test.testkit.bounded >2", "{text:?}");
}
