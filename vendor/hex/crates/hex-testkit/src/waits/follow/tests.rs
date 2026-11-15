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
            hex_wait::rest_until(by.unwrap()).unwrap();
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

/// A thread as a party, made on the thread itself (the review's C-14; AC-T7.4.1), on every OS: live while it runs, ended
/// once it has finished, a thread that panicked too. Do: a thread that names itself, then waits on a channel; read it,
/// then wake it to its end (the second round, to a panic), and join it. Expect: live; then, as its join returns, ended.
#[test]
fn a_thread_reads_live_while_it_runs_and_ended_once_it_finished_or_panicked() {
    use std::sync::mpsc;
    for panics in [false, true] {
        let (named, name) = mpsc::sync_channel(1);
        let (wake, woken) = mpsc::sync_channel::<()>(1);
        let runner = std::thread::spawn(move || {
            named.send(Thread::current().unwrap()).unwrap();
            let _ = woken.recv();
            if panics {
                panic!("the followed thread panics, as the test asks");
            }
        });
        let thread = name.recv().unwrap();
        assert_eq!(thread.party(), Party::Live, "panics: {panics}");
        wake.send(()).unwrap();
        assert_eq!(runner.join().is_err(), panics);
        assert_eq!(thread.party(), Party::Ended, "panics: {panics}");
    }
}

/// ROOT's ask of 2026-11-14 (the review's C-5): what a look-based wait ([`until`]) costs in CPU while its fact is far
/// off, beside an event-based one ([`whole`] on a channel) for the same span. A measurement, not a verdict: run alone,
/// under `/usr/bin/time`, with `HEX_TESTKIT_MEASURE=<until|event>` and `HEX_TESTKIT_MEASURE_SPAN_MS=<the span>`, the
/// span the measurement's own input (none is picked here: unset, it measures nothing). The fact comes once a thread's
/// receive of the span ends; the process's CPU over the wall time is the wait's cost.
#[test]
#[ignore = "a measurement, run alone under /usr/bin/time (its docs)"]
fn measure_the_cpu_of_a_wait_for_a_far_off_fact() {
    let (Some(form), Some(span)) = (
        std::env::var_os("HEX_TESTKIT_MEASURE"),
        std::env::var("HEX_TESTKIT_MEASURE_SPAN_MS")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok()),
    ) else {
        return;
    };
    let span = Duration::from_millis(span);
    let made = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (keep, held) = std::sync::mpsc::channel::<()>();
    let (told, tell) = std::sync::mpsc::channel::<()>();
    let maker = {
        let made = std::sync::Arc::clone(&made);
        std::thread::spawn(move || {
            let _ = held.recv_timeout(span);
            made.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = told.send(());
        })
    };
    let began = Instant::now();
    let wait = Patience::measuring("test.testkit.measure");
    if form == "until" {
        until(
            &wait,
            "a far-off fact",
            || {
                if maker.is_finished() {
                    Party::Ended
                } else {
                    Party::Live
                }
            },
            || made.load(std::sync::atomic::Ordering::SeqCst),
        )
        .unwrap();
    } else {
        whole(&wait, "a far-off fact", || tell.recv()).unwrap();
    }
    let wall = began.elapsed();
    drop(keep);
    maker.join().unwrap();
    eprintln!(
        "MEASURE form={} span_ms={} wall_ns={}",
        form.to_string_lossy(),
        span.as_millis(),
        wall.as_nanos()
    );
}
