//! The tests' following waits (decision 9, the person's ruling of 2026-11-13): a fact past W* waited for while its
//! party lives, however slowly it advances, a party that ends refused by type, a call sliced at W*, and a thread read
//! as a party.

use super::*;

/// A wait that has seen one answer of `slowest`, as a test's own would after its first.
fn learned(name: &'static str, slowest: Duration) -> Patience {
    let wait = Patience::measuring(name);
    wait.answered(slowest);
    wait
}

/// Do: wait for a fact already true. Expect: one look, its answer the wait's first sample.
#[test]
fn a_fact_already_true_takes_one_look_and_is_its_waits_sample() {
    let wait = Patience::measuring("test.testkit.follow.at_once");
    assert_eq!(
        until(&wait, "a fact already true", || Party::Live, || true),
        Ok(1)
    );
    assert!(wait.slowest().is_some());
}

/// Decision 9: past W* a test's wait goes on while its party lives. Do: a wait whose W* is 1 ns, a party alive, and a
/// fact that comes at the thousandth look. Expect: the thousand looks, never refused; the run-out told on the way (the
/// wait following), then the answer its sample, W* risen to it and bounding again.
#[test]
fn past_w_star_a_wait_goes_on_while_its_party_lives() {
    let learned_at = Duration::from_nanos(1);
    let wait = learned("test.testkit.follow.living", learned_at);
    let mut looks = 0;
    let got = until(
        &wait,
        "a fact at the thousandth look",
        || Party::Live,
        || {
            looks += 1;
            looks == 1_000
        },
    );
    assert_eq!(got, Ok(1_000));
    assert!(wait.slowest().is_some_and(|slowest| slowest > learned_at));
    assert_eq!(
        wait.deadline(),
        wait.slowest(),
        "W* bounds again after the answer"
    );
}

/// A party that ends fails the wait at once, within W* or past it, once a last look hasn't found the fact. Do: a
/// measuring wait whose party ends at its third look, the fact never coming; then one whose fact comes as its party
/// ends. Expect: the first unmet, its party ended; the second answered.
#[test]
fn a_party_that_ends_ends_the_wait_unless_its_fact_came_as_it_ended() {
    let wait = Patience::measuring("test.testkit.follow.ended");
    let mut looks = 0;
    let unmet = until(
        &wait,
        "a fact its party never makes",
        || {
            looks += 1;
            if looks < 3 { Party::Live } else { Party::Ended }
        },
        || false,
    )
    .unwrap_err();
    assert!(unmet.to_string().contains("ended without it"), "{unmet}");
    let ended = std::cell::Cell::new(false);
    let got = until(
        &wait,
        "a fact made as its party ends",
        || {
            ended.set(true);
            Party::Ended
        },
        || ended.get(),
    );
    assert_eq!(got, Ok(2));
}

/// A call that takes its own deadline is given W*'s end within it, then none: past W* its own answer, or its party's
/// end as it sees it, ends it. Do: a wait whose W* is 1 ms; a call that waits out its first slice and answers at its
/// second, its party alive. Expect: the answer; the first slice W*'s end from the start, the second none, the run-out
/// told between them.
#[test]
fn a_call_is_sliced_at_w_star_and_past_it_waits_for_its_own_answer() {
    let window = Duration::from_millis(1);
    let wait = learned("test.testkit.follow.within", window);
    let began = Instant::now();
    let mut slices = Vec::new();
    let got = within(
        &wait,
        "an answer at the second slice",
        || Party::Live,
        |by| {
            slices.push(by);
            if slices.len() == 2 {
                return Some("answered");
            }
            let at = by.unwrap();
            while Instant::now() < at {
                std::thread::yield_now();
            }
            None
        },
    );
    assert_eq!(got, Ok("answered"));
    let [Some(first), None] = slices[..] else {
        panic!("not W*'s end, then none: {slices:?}");
    };
    assert!(first >= began + window, "{slices:?}");
    assert!(wait.slowest().is_some_and(|slowest| slowest > window));
}

/// A thread as a party: live while it sleeps, ended once it has gone. Do: a thread that names itself, then sleeps on a
/// channel; read it, then wake it to its end, join it, and read it until it reads ended (a joined thread's task can
/// show for a moment as it exits: run r435 read it asleep once). Expect: live; then ended.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_thread_reads_live_while_it_sleeps_and_ended_once_gone() {
    use std::sync::mpsc;
    /// Longest this test waits for the joined thread to read ended: a bound, so a reading that never comes fails the
    /// test instead of hanging it.
    const PATIENCE: Duration = Duration::from_secs(30);
    let (named, name) = mpsc::sync_channel(1);
    let (wake, woken) = mpsc::sync_channel::<()>(1);
    let sleeper = std::thread::spawn(move || {
        named.send(Thread::current().unwrap()).unwrap();
        let _ = woken.recv();
    });
    let thread = name.recv().unwrap();
    assert_eq!(thread.party(), Party::Live);
    wake.send(()).unwrap();
    sleeper.join().unwrap();
    let joined = Instant::now();
    while thread.party() != Party::Ended {
        assert!(
            joined.elapsed() < PATIENCE,
            "{thread:?} still reads {:?} after its join",
            thread.party()
        );
        std::thread::yield_now();
    }
}
