//! The tests' waits (decision 9; the person's ruling of 2026-11-13, R03 for tests: a wait for a fact waits for the
//! answer, and fails at its party's end, the review's C-5): a call that can't be cut given no deadline, a wait refused
//! once its party has ended, a call sliced at W*'s end, and the census of the tests' own waits, which records them and
//! disarms nothing.

use super::follow::{self, Party};
use super::*;
use std::process::Command;
use std::time::Instant;

/// AC-T7.4.1, R03 for the tests' waits (decision 9, the person's ruling of 2026-11-13): a call that can't be cut and
/// made again ([`follow::whole`]) is given no deadline, so it waits for its answer, and a W* it passes refuses nothing.
/// Do: wait through such a call with a measuring patience, then with one that has seen a nanosecond's answer, the call
/// answering past it. Expect: each answer the call's own, recorded, W* risen to the slower one; the run-out told.
#[test]
fn original_a_call_that_cant_be_cut_is_given_no_deadline() {
    let measured = Patience::measuring("test.testkit.whole");
    assert_eq!(follow::whole(&measured, "a call's answer", || 7), 7);
    assert!(measured.slowest().is_some(), "the answer was recorded");
    let learned = Duration::from_nanos(1);
    let past = Patience::measuring("test.testkit.whole_past");
    past.answered(learned);
    let began = Instant::now();
    let got = follow::whole(&past, "an answer past W*", || {
        while began.elapsed() <= learned {
            std::thread::yield_now();
        }
        "answered"
    });
    assert_eq!(got, "answered");
    assert!(past.slowest().is_some_and(|slowest| slowest > learned));
    assert_eq!(
        past.deadline(),
        past.slowest(),
        "W* bounds again after the answer"
    );
}

/// The census's child: run only by [`the_census_records_a_tests_waits_and_disarms_none`], in a process of its own,
/// with the census's variables set. It waits once on a measuring fact, and once on a fact that comes only at the third
/// look with a patience that has seen a nanosecond's answer: waited for past its bound, its run-out told. Each fact is
/// the test's own count, so its party is the test itself, alive while it looks.
#[test]
#[ignore = "the census test's child, run in a process of its own"]
fn census_child() {
    let measured = Patience::measuring("test.testkit.census");
    assert_eq!(
        follow::until(&measured, "the census's fact", || Party::Live, || true),
        Ok(1)
    );
    let bounded = Patience::measuring("test.testkit.bounded");
    bounded.answered(Duration::from_nanos(1));
    let mut looks = 0;
    let past = follow::until(
        &bounded,
        "a fact past its bound",
        || Party::Live,
        || {
            looks += 1;
            looks == 3
        },
    );
    assert_eq!(past, Ok(3));
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
    assert_eq!(lines.len(), 4, "{text:?}");
    let (name, nanos) = lines[0].split_once(' ').unwrap();
    assert_eq!(name, "test.testkit.census");
    assert!(nanos.parse::<u64>().is_ok(), "{text:?}");
    // The answer of a nanosecond that bounds it; that bound, no margin (decision 9), run out and told; then its answer,
    // waited for.
    assert_eq!(lines[1], "test.testkit.bounded 1", "{text:?}");
    assert_eq!(lines[2], "test.testkit.bounded >1", "{text:?}");
    let (name, nanos) = lines[3].split_once(' ').unwrap();
    assert_eq!(name, "test.testkit.bounded");
    assert!(nanos.parse::<u64>().is_ok(), "{text:?}");
}

/// C-5 (decision 9; AC-T7.4.1): a test's wait for a fact fails once the party that would make it ends without it,
/// rather than looking on for good. Do: wait, through the kit's wait for a fact, for one a thread would make at the
/// thousandth look, the thread having ended before the first. Expect: refused, naming the fact, at its party's end:
/// never the thousand looks.
#[test]
fn c5_a_wait_fails_once_its_party_ends_without_its_fact() {
    // The party: a thread whose end drops the channel's one sender.
    let (alive, life) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || drop(alive)).join().unwrap();
    let wait = Patience::measuring("test.testkit.c5.ended");
    let mut looks = 0;
    let got = follow::until(
        &wait,
        "a fact its ended party never made",
        || match life.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Party::Ended,
            _ => Party::Live,
        },
        || {
            looks += 1;
            looks == 1_000
        },
    );
    assert!(
        got.is_err(),
        "the wait looked on past its party's end: {got:?} after {looks} looks"
    );
    assert!(
        looks <= 2,
        "a last look as its party ended, then refused: {looks}"
    );
}

/// C-5 (decision 9; AC-T7.4.1): a test's wait through a call is sliced at its W*'s end, so a party that ended within W*
/// is seen between slices, rather than the call given no deadline and waiting on for good. Do: a wait whose W* is
/// learned, its call keeping each deadline it is given. Expect: its first call given W*'s end.
#[test]
fn c5_a_calls_wait_is_given_its_w_stars_end() {
    let wait = Patience::measuring("test.testkit.c5.sliced");
    wait.answered(Duration::from_millis(1));
    let mut given = Vec::new();
    follow::within(
        &wait,
        "the call's answer",
        || Party::Live,
        |by| {
            given.push(by);
            Some(())
        },
    )
    .unwrap();
    assert!(
        matches!(given[..], [Some(_)]),
        "its call was given no deadline: {given:?}"
    );
}
