//! The learned wait's tests (decision 9; Ada's rules of 2026-11-06 and 2026-11-13, no picked constant and no record
//! baked in; AC-T7.1.1, AC-T7.1.3: every wait a node makes is bounded and learned). Against random answers, the wait is
//! the slowest seen, never shrinks, and lets through every answer no slower than that, and no other; before its first
//! answer, and after a run-out, it has no bound, its shell following its party by its life.

use std::time::Duration;

use proptest::prelude::*;

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// What the tests' waits follow, as a run-out names it.
const PARTY: &[Piece<'_>] = &[Piece::Kind("the test's party")];

proptest! {
    /// Do: record random answers on a wait with none, then ask whether a new answer would come within it. Expect:
    /// before the first, no bound (it follows its party); after each, the slowest so far, never less than before, and
    /// a new answer comes within it exactly when it is at most that.
    #[test]
    fn the_wait_is_the_slowest_answer_seen(
        answers in prop::collection::vec(0u64..86_400_000_000_000, 1..100),
        next in 0u64..200_000_000_000_000,
    ) {
        let patience = Patience::measuring("test");
        prop_assert_eq!(patience.deadline(), None);
        let mut slowest: Option<Duration> = None;
        let mut before = None;
        for &ns in &answers {
            let answer = Duration::from_nanos(ns);
            patience.answered(answer);
            slowest = Some(slowest.map_or(answer, |s| s.max(answer)));
            let got = patience.deadline();
            prop_assert_eq!(got, slowest);
            prop_assert!(got >= before, "the wait shrank from {:?} to {:?}", before, got);
            before = got;
        }
        let next = Duration::from_nanos(next);
        prop_assert_eq!(next <= patience.deadline().unwrap(), next <= slowest.unwrap());
    }
}

/// AC-T7.1.3, decision 9 (original; the person's ruling of 2026-11-13): a run-out at W* never refuses by itself. Do:
/// answer a wait in 5 ms, run the next out at 5 ms, then answer it at a minute. Expect: the run-out no sample (W* still
/// 5 ms) and the next wait unbounded, its shell following its party; the minute's answer the sample W* rises to,
/// bounding again.
#[test]
fn original_a_run_out_at_the_bound_never_refuses_by_itself() {
    let p = Patience::measuring("test");
    p.answered(ms(5));
    p.ran_out(ms(5), ms(5), PARTY);
    assert_eq!(p.slowest(), Some(ms(5)), "a run-out is no sample");
    assert_eq!(p.deadline(), None, "past W*, the wait follows its party");
    p.answered(ms(60_000));
    assert_eq!(p.deadline(), Some(ms(60_000)));
}

/// After a run-out (decision 9; Karn's point: no sample, the tail unknown, the cold start's case). Do: answer a wait
/// in 5 ms, so bounded at 5 ms; run it out there; answer it in 30 ms; then run it out again. Expect: the run-out no
/// sample (slowest still 5 ms) and the next wait unbounded; the slow answer learned, the bound 30 ms; unbounded again
/// after the second run-out; no factor anywhere.
#[test]
fn after_a_run_out_the_next_wait_follows_and_learns_its_uncensored_answer() {
    let p = Patience::measuring("test");
    p.answered(ms(5));
    assert_eq!(p.deadline(), Some(ms(5)));
    p.ran_out(ms(5), ms(5), PARTY);
    assert_eq!((p.deadline(), p.slowest()), (None, Some(ms(5))));
    p.answered(ms(30));
    assert_eq!(p.deadline(), Some(ms(30)));
    p.ran_out(ms(30), ms(30), PARTY);
    assert_eq!(p.deadline(), None);
}

/// A host slower than any answer seen. Do: answer a wait in 1 ms, then make calls that each take 25 ms, every wait
/// answered within its bound, run out at it, or followed to its answer. Expect: one run-out, the next wait following to
/// the 25 ms answer, and none after; the bound the slowest answer, 25 ms, with nothing added.
#[test]
fn a_host_slower_than_any_answer_seen_runs_out_once() {
    let p = Patience::measuring("test");
    p.answered(ms(1));
    let mut ran_out = 0u32;
    for _ in 0..100 {
        match p.deadline() {
            Some(bound) if ms(25) > bound => {
                p.ran_out(bound, bound, PARTY);
                ran_out += 1;
            }
            _ => p.answered(ms(25)),
        }
    }
    assert_eq!((ran_out, p.deadline()), (1, Some(ms(25))));
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
/// every time it runs out, reaches the installed sink by the wait's name, from the wait's own timing point; a run-out is
/// no sample (Karn's point), told apart from the answers, and raises nothing; a party taken for hung tells it nothing.
/// Do: install a sink, answer a wait named `census.test` in 7 ms and 9 ms, run it out at 20 ms, then take its party for
/// hung. Expect: the sink told the two answers and the run-out, by that name, and nothing of the hang; the slowest 9 ms.
#[test]
fn every_answer_and_run_out_reaches_the_census_by_its_waits_name() {
    static TOLD: std::sync::Mutex<Vec<(&'static str, Sample)>> = std::sync::Mutex::new(Vec::new());
    fn sink(name: &'static str, sample: Sample) {
        TOLD.lock().unwrap().push((name, sample));
    }
    assert!(census(sink), "the first install takes");
    let p = Patience::measuring("census.test");
    p.answered(ms(7));
    p.answered(ms(9));
    p.ran_out(ms(20), ms(20), PARTY);
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
    assert_eq!(p.slowest(), Some(ms(9)), "a run-out is no sample");
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

/// AC-T7.4.1: typed refusal census identity must not alter the existing online learning rule.
#[test]
fn refusal_census_preserves_learning_and_marks_only_the_optional_record() {
    let answered = Patience::measuring("outcome.answer");
    let refused = Patience::measuring("outcome.refused");
    for took in [ms(7), ms(3), ms(12)] {
        answered.answered(took);
        refused.refused(took);
        assert_eq!(answered.slowest(), refused.slowest());
        assert_eq!(answered.deadline(), refused.deadline());
    }
    assert_eq!(refused.slowest(), Some(ms(12)));
    assert_eq!(refused.deadline(), Some(ms(12)));
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

/// C-1 (decision 9; ROOT's conditions of 2026-11-14): the told line names its party in pieces, each by its kind, the
/// party a JSON string. Do: form the line for a party of every kind of piece, one a name holding a quote, a backslash,
/// a newline and a control character. Expect: the fields kiln reads, integers in nanoseconds, the party's pieces in
/// order, escaped as a JSON string, and the line one line.
#[test]
fn a_told_line_names_its_party_in_pieces_as_a_json_string() {
    let line = told_line(
        "a.wait",
        Duration::from_nanos(41),
        Duration::from_nanos(43),
        &[
            Piece::Kind("the init of container "),
            Piece::Name("a \"b\" \\ c\nd\u{1}"),
            Piece::Kind(", pid "),
            Piece::Pid(42),
            Piece::Kind(", seed "),
            Piece::Number(7),
            Piece::Kind(", at "),
            Piece::Path(std::path::Path::new("/sys/fs/cgroup/x")),
        ],
    );
    assert_eq!(
        line,
        "hex.wait event=ran_out wait=a.wait w_star_ns=41 waited_ns=43 party=\"the init of container \
         a \\\"b\\\" \\\\ c\\nd\\u0001, pid 42, seed 7, at /sys/fs/cgroup/x\"\n"
    );
    assert_eq!(line.matches('\n').count(), 1);
}

/// ROOT's conditions of 2026-11-14: the party is bounded, each piece cut at what the line leaves it, the cut told. Do:
/// form the line at the widest numbers a wait can name, for a party whose name and path are each ten times the line's
/// bound, a piece after them. Expect: the line within [`LINE_BOUND`], one line; the pieces before the cut whole; the
/// party ending with the count of every byte cut, the later piece's among them.
#[test]
fn a_party_past_what_the_line_leaves_is_cut_at_its_bound_and_the_cut_told() {
    let name = "n".repeat(LINE_BOUND * 10);
    let path = format!("/{}", "p".repeat(LINE_BOUND * 10));
    let party = [
        Piece::Kind("the init of container "),
        Piece::Name(&name),
        Piece::Kind(" at "),
        Piece::Path(std::path::Path::new(&path)),
        Piece::Kind(", by its pidfd"),
    ];
    let line = told_line("a.wait", Duration::MAX, Duration::MAX, &party);
    assert!(line.len() <= LINE_BOUND, "{} bytes", line.len());
    assert_eq!(line.matches('\n').count(), 1);
    let head = format!(
        "hex.wait event=ran_out wait=a.wait w_star_ns={0} waited_ns={0} party=\"the init of container ",
        Duration::MAX.as_nanos()
    );
    let rest = line.strip_prefix(&head).unwrap_or_else(|| panic!("{line}"));
    let after = rest.trim_start_matches('n');
    let shown = rest.len() - after.len();
    assert!(shown > 0, "the name shown up to the cut: {line}");
    let cut = name.len() - shown + " at ".len() + path.len() + ", by its pidfd".len();
    assert_eq!(after, format!(" [cut {cut} bytes]\"\n"));
}

/// The line's bound is POSIX's least PIPE_BUF, `{_POSIX_PIPE_BUF}` (IEEE Std 1003.1-2017, `<limits.h>`), cited where
/// it is declared: a write of at most that many bytes is never interleaved with another's on a pipe.
#[test]
fn the_lines_bound_is_posixs_least_pipe_buf() {
    assert_eq!(LINE_BOUND, 512);
}

/// The review's C-10 (decision 9; ROOT, 2026-11-14): a wait that starts with no W* and whose party could hang alive
/// tells that it follows it, in the run-out's own form and bound. Do: form the following line for a party of a kind and a
/// name; then a party ten times the line's bound; and ask a cold wait, then one that has its W*, to tell it. Expect: the
/// fields kiln reads, the party a JSON string, one line; the long one within [`LINE_BOUND`], its cut told; the cold
/// wait told, the one with a W* not.
#[test]
fn c10_a_wait_with_no_w_star_tells_that_it_follows_its_party() {
    assert_eq!(
        following_line(
            "a.wait",
            &[Piece::Kind("the node VM's run, by its READY and its end")]
        ),
        "hex.wait event=following wait=a.wait party=\"the node VM's run, by its READY and its end\"\n"
    );
    let name = "n".repeat(LINE_BOUND * 10);
    let long = following_line("a.wait", &[Piece::Kind("the init of "), Piece::Name(&name)]);
    assert!(long.len() <= LINE_BOUND, "{} bytes", long.len());
    assert_eq!(long.matches('\n').count(), 1);
    assert!(long.ends_with(" bytes]\"\n"), "{long}");
    let cold = Patience::measuring("c10.cold");
    assert!(cold.following(&[Piece::Kind("a party")]));
    cold.answered(Duration::from_nanos(41));
    assert!(!cold.following(&[Piece::Kind("a party")]));
}
