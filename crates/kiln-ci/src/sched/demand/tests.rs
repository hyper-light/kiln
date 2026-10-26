//! docs/ci.md W28-R40 to R44 and W28-AC40: demands, domains, the work in flight, the fate of work
//! no one wants, and endings; then 10,000 random sequences of arrivals, asks, starts, endings,
//! fates, departures, checks and restarts.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::*;

fn demand(run: &str, lane: &str, at: i64) -> Demand {
    Demand { run: run.into(), lane: lane.into(), commit: format!("c-{run}"), intent: Intent::Trunk, actor: Some("agent".into()), domain: Domain::Trusted, at }
}

fn work(key: &str, domain: Domain) -> Work {
    Work { key: Some(key.into()), domain, lane: format!("acme/ci/{key}"), observed: false }
}

fn trusted(key: &str) -> Work {
    work(key, Domain::Trusted)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

fn next(v: &[(&str, Next)]) -> Vec<(String, Next)> {
    v.iter().map(|(l, n)| ((*l).to_owned(), n.clone())).collect()
}

fn adopt(exec: &str, run: &str) -> Ask {
    Ask::Adopt { exec: exec.into(), run: run.into() }
}

#[test]
fn w28_r40_a_newer_push_withdraws_older_demands_on_its_lane() {
    let mut d = Demands::default();
    assert!(d.arrive(demand("A", "main", 1), true).is_empty());
    assert!(d.arrive(demand("X", "dev", 2), true).is_empty(), "another lane");
    assert_eq!(d.arrive(demand("B", "main", 3), true), s(&["A"]));
    assert_eq!(d.withdrawn_by("A"), Some("B"));
    assert!(d.live("B") && d.live("X") && !d.live("A"));
    assert_eq!(d.arrive(demand("C", "main", 4), true), s(&["B"]), "A was withdrawn already");
    // A rerun of A: its demand is live again, and withdraws nothing.
    assert!(d.arrive(demand("A", "main", 1), false).is_empty());
    assert!(d.live("A") && d.live("C"));
    // A run approved late (W18-R06), created before C, withdraws only what's older than it.
    assert_eq!(d.arrive(demand("L", "main", 2), true), s(&["A"]));
    assert!(d.live("L") && d.live("C"));
    // Without `supersede: adopt`, or on a branch in `keep`, nothing is withdrawn.
    assert!(d.arrive(demand("D", "main", 5), false).is_empty());
    assert!(d.live("C") && d.live("D"));
    // A run without a demand wants its own work; one that ended has none.
    assert!(d.live("unknown"));
    d.leave("A");
    assert!(d.demand("A").is_none() && d.live("A"));
    assert_eq!(d.demand("C").map(|x| (x.commit.as_str(), x.intent, x.domain.clone())), Some(("c-C", Intent::Trunk, Domain::Trusted)));
}

#[test]
fn w28_r41_domains() {
    use Domain::{Person, Protected, Trusted, Untrusted};
    let p = |id: &str| Person(id.into());
    let table = [
        (Trusted, Trusted, true),
        (Trusted, Untrusted, false),
        (Trusted, p("a"), false),
        (Trusted, Protected, false),
        (Untrusted, Trusted, true),
        (Untrusted, Untrusted, true),
        (Untrusted, p("a"), false),
        (Untrusted, Protected, false),
        (p("a"), Trusted, true),
        (p("a"), p("a"), true),
        (p("a"), p("b"), false),
        (p("a"), Untrusted, false),
        (Protected, Trusted, false),
        (Protected, Protected, false),
        (Protected, Untrusted, false),
    ];
    for (reader, writer, may) in table {
        assert_eq!(reader.may_adopt(&writer), may, "{reader} adopting {writer}");
    }
}

#[test]
fn w28_r41_adoption_follows_the_domains() {
    let mut d = Demands::default();
    assert_eq!(d.ask("T", "r1", &trusted("K"), 50), Ask::Execute);
    // Untrusted and person scopes adopt the trusted execution.
    assert_eq!(d.ask("U", "r2", &work("K", Domain::Untrusted), 50), adopt("T", "r1"));
    assert_eq!(d.ask("P", "r3", &work("K", Domain::Person("ada".into())), 50), adopt("T", "r1"));
    // Protected adopts nothing, not even another protected execution: one runner's result in
    // flight isn't two trusted runners' (W24 Decision 4).
    assert_eq!(d.ask("R1", "r4", &work("K", Domain::Protected), 50), Ask::Execute);
    assert_eq!(d.ask("R2", "r5", &work("K", Domain::Protected), 50), Ask::Execute);
    // An untrusted execution is the untrusted domain's: a trusted leg never adopts it.
    assert_eq!(d.ask("U2", "r6", &work("J", Domain::Untrusted), 50), Ask::Execute);
    assert_eq!(d.ask("T2", "r7", &trusted("J"), 50), Ask::Execute);
    assert_eq!(d.ask("U3", "r8", &work("J", Domain::Untrusted), 50), adopt("U2", "r6"), "its own domain's first");
    // A leg without a key adopts nothing and is never adopted.
    let keyless = Work { key: None, ..trusted("K") };
    assert_eq!(d.ask("N1", "r9", &keyless, 50), Ask::Execute);
    assert_eq!(d.ask("N2", "r9", &keyless, 50), Ask::Execute);
}

#[test]
fn w28_r42_b_adopts_a_with_the_same_key() {
    let mut d = Demands::default();
    d.arrive(demand("A", "main", 1), true);
    assert_eq!(d.ask("A/rust", "A", &trusted("K1"), 60), Ask::Execute);
    d.started("A/rust", 1_000);
    assert_eq!(d.arrive(demand("B", "main", 2), true), s(&["A"]));
    assert_eq!(d.ask("B/rust", "B", &trusted("K1"), 60), adopt("A/rust", "A"));
    assert_eq!(d.adopts("B/rust"), Some("A/rust"));
    assert_eq!(d.adopters("A/rust"), ["B/rust"]);
    assert!(d.is_execution("A/rust") && !d.is_execution("B/rust"));
    // Asked again, it gets what it got.
    assert_eq!(d.ask("B/rust", "B", &trusted("K1"), 60), adopt("A/rust", "A"));
    // A's demand is withdrawn, but B wants the execution: it's kept, and not cancelled.
    assert_eq!(d.affected(&s(&["A"])), s(&["A/rust"]));
    let kept = d.fate("A/rust", Some(900), true, FINISH_WITHIN);
    assert_eq!(kept, Some(Fate::Kept { priority: 60, by: s(&["B"]) }));
    assert_eq!(kept.map(|f| f.why()).as_deref(), Some("kept: run B still wants it, so it goes on at priority 60"));
    assert!(d.unwanted_running().is_empty());
    // It ends: B finishes reused from it.
    assert_eq!(d.ended("A/rust", End::Success), next(&[("B/rust", Next::Reuse { from: "A/rust".into() })]));
    assert!(d.is_empty());
}

#[test]
fn w28_r42_an_execution_takes_the_highest_priority_still_waiting() {
    let mut d = Demands::default();
    d.arrive(demand("A", "main", 1), true);
    d.arrive(demand("B", "dev", 2), true);
    assert_eq!(d.ask("A/test", "A", &trusted("K"), 20), Ask::Execute);
    assert_eq!(d.priority("A/test"), Some(20));
    assert_eq!(d.ask("B/test", "B", &trusted("K"), 80), adopt("A/test", "A"));
    assert_eq!(d.priority("A/test"), Some(80));
    // A's lane gets a newer push: A is withdrawn, and B's priority is what's left.
    assert_eq!(d.arrive(demand("A2", "main", 3), true), s(&["A"]));
    assert_eq!(d.fate("A/test", None, false, FINISH_WITHIN), Some(Fate::Kept { priority: 80, by: s(&["B"]) }));
    // B is withdrawn too: no live demand wants it, and it hasn't started.
    assert_eq!(d.arrive(demand("B2", "dev", 4), true), s(&["B"]));
    assert_eq!(d.priority("A/test"), None);
    let dropped = d.fate("A/test", None, false, FINISH_WITHIN);
    assert_eq!(dropped, Some(Fate::Drop { by: s(&["A2", "B2"]) }));
    assert_eq!(dropped.map(|f| f.why()).as_deref(), Some("dropped before it started: no live demand wants it (withdrawn by runs A2, B2)"));
    // B leaves before A's execution is dropped: it's withdrawn by A2 alone.
    assert_eq!(d.forget("B/test").as_deref(), Some("A/test"));
    assert_eq!(d.fate("A/test", None, false, FINISH_WITHIN), Some(Fate::Drop { by: s(&["A2"]) }));
}

#[test]
fn w28_r42_a_withdrawn_run_only_takes_what_is_in_flight() {
    let mut d = Demands::default();
    d.arrive(demand("A", "main", 1), true);
    d.arrive(demand("B", "main", 2), true);
    assert_eq!(d.ask("B/ui", "B", &trusted("U"), 60), Ask::Execute);
    // A was withdrawn before its legs were queued: it adopts B's ui, and runs nothing new.
    assert_eq!(d.ask("A/ui", "A", &trusted("U"), 60), adopt("B/ui", "B"));
    assert_eq!(d.ask("A/rust", "A", &trusted("K1"), 60), Ask::Drop);
    assert!(!d.is_execution("A/rust"));
}

#[test]
fn w28_r42_a_restart_turns_a_queued_duplicate_into_an_adopter() {
    let mut d = Demands::default();
    // After a restart, B's leg (queued with A's key) and C's are asked for before A's running leg
    // is announced.
    assert_eq!(d.ask("B/rust", "B", &trusted("K1"), 60), Ask::Execute);
    assert_eq!(d.ask("C/rust", "C", &trusted("K1"), 50), adopt("B/rust", "B"));
    assert_eq!(d.running("A/rust", "A", &trusted("K1"), 40, 5_000), Running::Took { queued: "B/rust".into() });
    assert_eq!((d.adopts("B/rust"), d.adopts("C/rust")), (Some("A/rust"), Some("A/rust")));
    assert!(d.is_execution("A/rust") && !d.is_execution("B/rust"));
    assert_eq!(d.priority("A/rust"), Some(60));
    // Announced again, it's still the execution.
    assert_eq!(d.running("A/rust", "A", &trusted("K1"), 40, 5_000), Running::Executes);
    // A running leg of other work executes it; two running at once can't be undone.
    assert_eq!(d.running("D/ui", "D", &trusted("U"), 40, 5_000), Running::Executes);
    assert_eq!(d.running("E/ui", "E", &trusted("U"), 40, 6_000), Running::Alone { exec: "D/ui".into() });
    assert_eq!(d.ask("F/ui", "F", &trusted("U"), 40), adopt("D/ui", "D"), "the one the table knew is the one adopted");
    assert_eq!(
        d.ended("A/rust", End::Success),
        next(&[("B/rust", Next::Reuse { from: "A/rust".into() }), ("C/rust", Next::Reuse { from: "A/rust".into() })])
    );
}

/// A, the older push, executes the rust leg (key K1) and the ui leg (U1); B touches crates/: its
/// rust leg has another key.
fn pushes() -> Demands {
    let mut d = Demands::default();
    d.arrive(demand("A", "main", 1), true);
    assert_eq!(d.ask("A/rust", "A", &trusted("K1"), 60), Ask::Execute);
    assert_eq!(d.ask("A/ui", "A", &trusted("U1"), 60), Ask::Execute);
    d.started("A/rust", 0);
    d.started("A/ui", 0);
    assert_eq!(d.arrive(demand("B", "main", 2), true), s(&["A"]));
    assert_eq!(d.ask("B/ui", "B", &trusted("U1"), 60), adopt("A/ui", "A"));
    assert_eq!(d.ask("B/rust", "B", &trusted("K2"), 60), Ask::Execute);
    d
}

#[test]
fn w28_r43_work_no_one_wants_runs_on_until_live_work_contends_for_it() {
    let d = pushes();
    assert_eq!(d.affected(&s(&["A"])), s(&["A/rust", "A/ui"]));
    // B adopted A's ui: it's kept.
    assert_eq!(d.fate("A/ui", Some(600), true, FINISH_WITHIN), Some(Fate::Kept { priority: 60, by: s(&["B"]) }));
    // No one wants A's rust leg: it runs on while nothing contends for its capacity.
    assert_eq!(d.unwanted_running(), s(&["A/rust"]));
    let runs = d.fate("A/rust", Some(900), false, FINISH_WITHIN);
    assert_eq!(runs, Some(Fate::Run { by: s(&["B"]) }));
    assert_eq!(runs.map(|f| f.why()).as_deref(), Some("runs on: no live demand wants it (withdrawn by run B), but nothing waits for its capacity"));
    // Contended, it finishes within finish_within, seeding trusted entries, and is cancelled past it.
    let finishes = d.fate("A/rust", Some(200), true, FINISH_WITHIN);
    assert_eq!(finishes, Some(Fate::Finish { by: s(&["B"]), left: 200, finish_within: 300 }));
    assert_eq!(
        finishes.map(|f| f.why()).as_deref(),
        Some("finishes: no live demand wants it (withdrawn by run B), but it has about 200 s left, within finish_within (300 s), and it seeds trusted entries")
    );
    let cancelled = d.fate("A/rust", Some(900), true, FINISH_WITHIN);
    assert_eq!(cancelled, Some(Fate::Cancel { by: s(&["B"]), left: Some(900), finish_within: 300, seeds: true, lane: "acme/ci/K1".into() }));
    assert_eq!(
        cancelled.map(|f| f.why()).as_deref(),
        Some("cancelled for live work: no live demand wants it (withdrawn by run B), and it has about 900 s left, more than finish_within (300 s); its warm workspace goes back to lane acme/ci/K1")
    );
    // Exactly finish_within left still finishes; how long is left unknown, it's cancelled.
    assert!(matches!(d.fate("A/rust", Some(300), true, FINISH_WITHIN), Some(Fate::Finish { .. })));
    assert!(matches!(d.fate("A/rust", None, true, FINISH_WITHIN), Some(Fate::Cancel { left: None, seeds: true, .. })));
    assert!(d.fate("nothing", Some(1), true, FINISH_WITHIN).is_none());
}

#[test]
fn w28_r43_work_that_seeds_no_trusted_entries_is_cancelled_however_near_its_end() {
    for w in [work("K", Domain::Untrusted), work("K", Domain::Person("ada".into())), Work { key: None, ..trusted("K") }] {
        let mut d = Demands::default();
        d.arrive(demand("A", "main", 1), true);
        assert_eq!(d.ask("A/x", "A", &w, 60), Ask::Execute);
        d.started("A/x", 0);
        d.arrive(demand("B", "main", 2), true);
        let cancelled = d.fate("A/x", Some(100), true, FINISH_WITHIN);
        assert!(matches!(cancelled, Some(Fate::Cancel { left: Some(100), seeds: false, .. })), "{w:?}: {cancelled:?}");
        assert!(cancelled.is_some_and(|f| f.why().contains("it seeds no trusted entries")));
    }
}

#[test]
fn w28_r43_an_attempt_that_stopped_is_dropped_like_one_that_never_started() {
    let mut d = Demands::default();
    d.arrive(demand("A", "main", 1), true);
    assert_eq!(d.ask("A/x", "A", &trusted("K"), 60), Ask::Execute);
    d.started("A/x", 0);
    d.arrive(demand("B", "main", 2), true);
    assert!(matches!(d.fate("A/x", Some(900), false, FINISH_WITHIN), Some(Fate::Run { .. })));
    // Its attempt failed and it waits for a retry: no one wants it, so it's dropped.
    d.stopped("A/x");
    assert!(d.unwanted_running().is_empty());
    assert_eq!(d.fate("A/x", Some(900), false, FINISH_WITHIN), Some(Fate::Drop { by: s(&["B"]) }));
}

/// A executes K (trusted); B (another lane) and C (a third) adopt it, then U, untrusted.
fn shared() -> Demands {
    let mut d = Demands::default();
    for (run, lane) in [("A", "main"), ("B", "dev"), ("C", "qa"), ("U", "fork")] {
        d.arrive(demand(run, lane, 0), true);
    }
    assert_eq!(d.ask("A/k", "A", &trusted("K"), 50), Ask::Execute);
    assert_eq!(d.ask("B/k", "B", &trusted("K"), 50), adopt("A/k", "A"));
    assert_eq!(d.ask("C/k", "C", &trusted("K"), 50), adopt("A/k", "A"));
    assert_eq!(d.ask("U/k", "U", &work("K", Domain::Untrusted), 50), adopt("A/k", "A"));
    d
}

#[test]
fn w28_r44_only_successes_and_failures_of_class_failure_are_shared() {
    use FailureClass::{DiskFull, Failure, ImagePull, Infra, Oom, RunnerLost, Timeout};
    for end in [End::Success, End::Failed(Failure)] {
        let mut d = shared();
        let reused = Next::Reuse { from: "A/k".into() };
        assert_eq!(d.ended("A/k", end), next(&[("B/k", reused.clone()), ("C/k", reused.clone()), ("U/k", reused)]), "{end:?}");
        assert!(d.is_empty());
    }
    for end in [End::Failed(RunnerLost), End::Failed(Infra), End::Failed(ImagePull), End::Failed(Oom), End::Failed(DiskFull), End::Failed(Timeout), End::Cancelled] {
        let mut d = shared();
        // The first adopter executes, the second adopts it; the untrusted one can adopt it too.
        assert_eq!(d.ended("A/k", end), next(&[("B/k", Next::Execute), ("C/k", Next::Adopt { exec: "B/k".into(), run: "B".into() }), ("U/k", Next::Adopt { exec: "B/k".into(), run: "B".into() })]), "{end:?}");
        assert!(d.is_execution("B/k") && d.adopts("C/k") == Some("B/k"));
    }
}

#[test]
fn w28_r44_the_rest_adopt_only_what_their_domain_may() {
    let mut d = Demands::default();
    assert_eq!(d.ask("A/k", "A", &trusted("K"), 50), Ask::Execute);
    assert_eq!(d.ask("U/k", "U", &work("K", Domain::Untrusted), 50), adopt("A/k", "A"));
    assert_eq!(d.ask("B/k", "B", &trusted("K"), 50), adopt("A/k", "A"));
    // The untrusted adopter executes first, in its own domain: the trusted one may not adopt that,
    // so it executes too.
    assert_eq!(d.ended("A/k", End::Failed(FailureClass::RunnerLost)), next(&[("U/k", Next::Execute), ("B/k", Next::Execute)]));
}

#[test]
fn w28_r44_withdrawn_adopters_take_what_is_in_flight_or_are_dropped() {
    let mut d = shared();
    // B and C are withdrawn; U is live, and executes first even though it adopted last.
    d.arrive(demand("B2", "dev", 1), true);
    d.arrive(demand("C2", "qa", 1), true);
    assert_eq!(d.ended("A/k", End::Failed(FailureClass::Timeout)), next(&[("U/k", Next::Execute), ("B/k", Next::Drop), ("C/k", Next::Drop)]), "trusted legs may not adopt U's untrusted execution");
    let mut d = shared();
    d.arrive(demand("U2", "fork", 1), true);
    d.arrive(demand("B2", "dev", 1), true);
    assert_eq!(d.ended("A/k", End::Cancelled), next(&[("C/k", Next::Execute), ("B/k", Next::Adopt { exec: "C/k".into(), run: "C".into() }), ("U/k", Next::Adopt { exec: "C/k".into(), run: "C".into() })]));
}

#[test]
fn w28_r44_an_adopter_of_observed_reads_is_checked_and_runs_itself_on_a_mismatch() {
    let observed = Work { observed: true, ..trusted("K") };
    let mut d = Demands::default();
    assert_eq!(d.ask("A/k", "A", &observed, 50), Ask::Execute);
    assert_eq!(d.ask("B/k", "B", &observed, 50), adopt("A/k", "A"));
    assert_eq!(d.ask("C/k", "C", &observed, 50), adopt("A/k", "A"));
    let check = Next::Check { from: "A/k".into() };
    assert_eq!(d.ended("A/k", End::Success), next(&[("B/k", check.clone()), ("C/k", check)]));
    assert!(!d.is_empty(), "B and C are checking");
    assert_eq!(d.checked("B/k", true), Some(Next::Reuse { from: "A/k".into() }));
    assert_eq!(d.checked("C/k", false), Some(Next::Execute));
    assert_eq!(d.checked("C/k", true), None, "checked once");
    assert!(d.is_execution("C/k"));
    assert!(d.ended("C/k", End::Success).is_empty());
    assert!(d.is_empty());
}

#[test]
fn w28_r44_every_decision_says_why() {
    assert_eq!(adopt("A/k", "A").why(), "adopts A/k of run A: the same work, in flight in a domain it may read");
    assert_eq!(Ask::Drop.why(), "dropped: its run's demand was withdrawn, and nothing in flight has its work");
    assert_eq!(Next::Reuse { from: "A/k".into() }.why(), "reused from A/k, the execution it adopted");
    assert_eq!(Next::Execute.why(), "executes: the execution it adopted ended in a way it may not take over");
    assert_eq!(Fate::Kept { priority: 90, by: s(&["B", "C"]) }.why(), "kept: runs B, C still want it, so it goes on at priority 90");
    assert_eq!(Fate::Drop { by: Vec::new() }.why(), "dropped before it started: no live demand wants it (its demands are gone)");
}

// ------------------------------------------------------------------ W28-AC40

/// One step of a random sequence; numbers pick among what's there, so every sequence is valid.
#[derive(Debug, Clone)]
enum Op {
    Arrive { run: u8, withdraw: bool },
    Ask { run: u8, key: u8, domain: u8, priority: u8 },
    Start { pick: u8 },
    End { pick: u8, end: u8 },
    Fate { pick: u8, left: u16, contended: bool },
    /// Live work can't be placed: one of the running executions no one wants is decided, as the
    /// scheduler does.
    Contend { pick: u8, left: u16 },
    Forget { pick: u8 },
    Check { pick: u8, matched: bool },
    Restart { seed: u64 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u8..6, prop::bool::weighted(0.8)).prop_map(|(run, withdraw)| Op::Arrive { run, withdraw }),
        6 => (0u8..6, 0u8..4, 0u8..8, 0u8..=100).prop_map(|(run, key, domain, priority)| Op::Ask { run, key, domain, priority }),
        3 => any::<u8>().prop_map(|pick| Op::Start { pick }),
        4 => (any::<u8>(), 0u8..12).prop_map(|(pick, end)| Op::End { pick, end }),
        3 => (any::<u8>(), 0u16..600, any::<bool>()).prop_map(|(pick, left, contended)| Op::Fate { pick, left, contended }),
        3 => (any::<u8>(), 0u16..450).prop_map(|(pick, left)| Op::Contend { pick, left }),
        1 => any::<u8>().prop_map(|pick| Op::Forget { pick }),
        5 => (any::<u8>(), prop::bool::weighted(0.35)).prop_map(|(pick, matched)| Op::Check { pick, matched }),
        1 => any::<u64>().prop_map(|seed| Op::Restart { seed }),
    ]
}

/// Where a leg is, as the scheduler driving the table keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum St {
    Executes { started: bool },
    Waits(String),
    /// Checking what the execution it adopted read (W24-R04).
    Checks(String),
    Ended,
}

#[derive(Debug, Clone)]
struct Leg {
    run: String,
    work: Work,
    priority: u8,
    st: St,
}

/// What the property test saw, across its cases.
#[derive(Default)]
struct Seen {
    cases: Cell<usize>,
    adopted: Cell<usize>,
    took: Cell<usize>,
    reexecuted: Cell<usize>,
    cancelled: Cell<usize>,
    finished: Cell<usize>,
    mismatched: Cell<usize>,
}

fn bump(c: &Cell<usize>) {
    c.set(c.get() + 1);
}

struct Model<'a> {
    d: Demands,
    legs: BTreeMap<String, Leg>,
    /// Each run's first `at`, and every arrival in order: a restart replays them.
    at: BTreeMap<String, i64>,
    arrivals: Vec<(Demand, bool)>,
    seen: &'a Seen,
}

fn key_of(k: u8) -> Option<String> {
    (k < 3).then(|| format!("k{k}"))
}

fn domain_of(d: u8) -> Domain {
    match d {
        0..=3 => Domain::Trusted,
        4 => Domain::Untrusted,
        5 => Domain::Person("a".into()),
        6 => Domain::Person("b".into()),
        _ => Domain::Protected,
    }
}

fn end_of(e: u8) -> End {
    use FailureClass::{DiskFull, Failure, ImagePull, Infra, Oom, RunnerLost, Timeout};
    match e {
        0..=3 => End::Success,
        4 => End::Failed(Failure),
        5 => End::Failed(RunnerLost),
        6 => End::Failed(Infra),
        7 => End::Failed(ImagePull),
        8 => End::Failed(Oom),
        9 => End::Failed(DiskFull),
        10 => End::Failed(Timeout),
        _ => End::Cancelled,
    }
}

/// The item `pick` chooses among `v`, if there's any.
fn choose<T: Clone>(v: &[T], pick: u8) -> Option<T> {
    if v.is_empty() { None } else { v.get(usize::from(pick) % v.len()).cloned() }
}

/// A mix of a seed and a leg, to put legs in an order of the seed's.
fn mix(seed: u64, leg: &str) -> u64 {
    leg.bytes().fold(seed ^ 0x9e37_79b9_7f4a_7c15, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3).rotate_left(17))
}

impl Model<'_> {
    fn legs_in(&self, f: impl Fn(&St) -> bool) -> Vec<String> {
        self.legs.iter().filter(|(_, l)| f(&l.st)).map(|(k, _)| k.clone()).collect()
    }

    fn set(&mut self, leg: &str, st: St) -> Result<(), TestCaseError> {
        let l = self.legs.get_mut(leg).ok_or_else(|| TestCaseError::fail(format!("no leg {leg}")))?;
        prop_assert!(l.st != St::Ended, "{leg} ended twice");
        l.st = st;
        Ok(())
    }

    /// What a leg's ask got: only a live run's leg executes, and only a withdrawn one's is dropped.
    fn asked(&mut self, leg: &str, ask: Ask) -> Result<(), TestCaseError> {
        let run = self.legs.get(leg).map(|l| l.run.clone()).unwrap_or_default();
        match ask {
            Ask::Execute => {
                prop_assert!(self.d.live(&run), "{leg} executes, but its run {run} was withdrawn");
                self.set(leg, St::Executes { started: false })
            }
            Ask::Adopt { exec, .. } => {
                bump(&self.seen.adopted);
                self.set(leg, St::Waits(exec))
            }
            Ask::Drop => {
                prop_assert!(!self.d.live(&run), "{leg} dropped, but its run {run} is live");
                self.set(leg, St::Ended)
            }
        }
    }

    /// An execution ended: it, and what each of its adopters does next.
    fn ended(&mut self, exec: &str, end: End) -> Result<(), TestCaseError> {
        let waiting: BTreeSet<String> = self.legs_in(|st| *st == St::Waits(exec.to_owned())).into_iter().collect();
        let next = self.d.ended(exec, end);
        self.set(exec, St::Ended)?;
        let told: BTreeSet<String> = next.iter().map(|(l, _)| l.clone()).collect();
        prop_assert_eq!(&told, &waiting, "every adopter of {} hears of its end", exec);
        for (leg, n) in next {
            match n {
                Next::Reuse { from } => {
                    prop_assert!(end.shared() && from == exec, "{leg} reused {from} after {end:?}");
                    self.set(&leg, St::Ended)?;
                }
                Next::Check { from } => {
                    prop_assert!(end.shared() && from == exec);
                    self.set(&leg, St::Checks(from))?;
                }
                Next::Execute => {
                    prop_assert!(!end.shared());
                    bump(&self.seen.reexecuted);
                    self.asked(&leg, Ask::Execute)?;
                }
                Next::Adopt { exec, run } => self.asked(&leg, Ask::Adopt { exec, run })?,
                Next::Drop => self.asked(&leg, Ask::Drop)?,
            }
        }
        Ok(())
    }

    fn apply(&mut self, i: usize, op: &Op) -> Result<(), TestCaseError> {
        match *op {
            Op::Arrive { run, withdraw } => {
                let run = format!("r{run}");
                // A run keeps the `at` of its first arrival (a rerun's is its run's); no two runs share one.
                let at = *self.at.entry(run.clone()).or_insert(i64::try_from(i).unwrap_or(i64::MAX));
                let lane = if run.ends_with(['0', '2', '4']) { "lane0" } else { "lane1" };
                // What it withdraws: the live demands on its lane created before it.
                let older: Vec<String> = self.at.iter().filter(|(r, a)| **r != run && **a < at && self.d.live(r) && self.d.demand(r).is_some_and(|d| d.lane == lane)).map(|(r, _)| r.clone()).collect();
                let dm = demand(&run, lane, at);
                self.arrivals.push((dm.clone(), withdraw));
                let mut got = self.d.arrive(dm, withdraw);
                got.sort();
                prop_assert_eq!(&got, &if withdraw { older } else { Vec::new() }, "withdrawn by {}", run);
                prop_assert!(self.d.live(&run));
                for r in &got {
                    prop_assert_eq!(self.d.withdrawn_by(r), Some(run.as_str()));
                }
            }
            Op::Ask { run, key, domain, priority } => {
                let leg = format!("l{}", self.legs.len());
                let k = key_of(key);
                let work = Work { observed: matches!(k.as_deref(), Some("k1" | "k2")), lane: format!("lane/{k:?}"), key: k, domain: domain_of(domain) };
                let run = format!("r{run}");
                self.legs.insert(leg.clone(), Leg { run: run.clone(), work: work.clone(), priority, st: St::Executes { started: false } });
                let ask = self.d.ask(&leg, &run, &work, priority);
                self.asked(&leg, ask)?;
            }
            Op::Start { pick } => {
                if let Some(leg) = choose(&self.legs_in(|st| *st == St::Executes { started: false }), pick) {
                    self.d.started(&leg, i64::try_from(i).unwrap_or(0));
                    self.set(&leg, St::Executes { started: true })?;
                }
            }
            Op::End { pick, end } => {
                if let Some(leg) = choose(&self.legs_in(|st| matches!(st, St::Executes { .. })), pick) {
                    let started = self.legs.get(&leg).is_some_and(|l| l.st == St::Executes { started: true });
                    // Only an attempt that ran can succeed or fail; a queued one is cancelled.
                    self.ended(&leg, if started { end_of(end) } else { End::Cancelled })?;
                }
            }
            Op::Fate { pick, left, contended } => {
                let Some(exec) = choose(&self.legs_in(|st| matches!(st, St::Executes { .. })), pick) else { return Ok(()) };
                let Some(l) = self.legs.get(&exec).cloned() else { return Ok(()) };
                let started = l.st == St::Executes { started: true };
                let left = started.then_some(i64::from(left));
                let fate = self.d.fate(&exec, left, contended, FINISH_WITHIN).ok_or_else(|| TestCaseError::fail(format!("no fate for {exec}")))?;
                let waiters: Vec<Leg> = self.legs.values().filter(|w| w.st == St::Waits(exec.clone())).cloned().collect();
                let live: Vec<u8> = std::iter::once(&l).chain(&waiters).filter(|w| self.d.live(&w.run)).map(|w| w.priority).collect();
                match &fate {
                    Fate::Kept { priority, .. } => prop_assert_eq!(Some(*priority), live.iter().copied().max()),
                    Fate::Drop { .. } => prop_assert!(live.is_empty() && !started),
                    Fate::Run { .. } => prop_assert!(live.is_empty() && started && !contended),
                    Fate::Finish { left: f, .. } => {
                        prop_assert!(live.is_empty() && started && contended && l.work.seeds_trusted() && *f <= FINISH_WITHIN);
                        bump(&self.seen.finished);
                    }
                    Fate::Cancel { lane, .. } => {
                        prop_assert!(live.is_empty() && started && contended && !(l.work.seeds_trusted() && left.is_some_and(|x| x <= FINISH_WITHIN)));
                        prop_assert_eq!(lane, &l.work.lane);
                        bump(&self.seen.cancelled);
                    }
                }
                // The scheduler carries out a drop or a cancel: the execution ends cancelled.
                if matches!(fate, Fate::Drop { .. } | Fate::Cancel { .. }) {
                    self.ended(&exec, End::Cancelled)?;
                }
            }
            Op::Contend { pick, left } => {
                let unwanted = self.d.unwanted_running();
                let running = self.legs_in(|st| *st == St::Executes { started: true });
                prop_assert!(unwanted.iter().all(|x| running.contains(x)), "unwanted_running {:?} names a leg that doesn't run", unwanted);
                let Some(exec) = choose(&unwanted, pick) else { return Ok(()) };
                let seeds = self.legs.get(&exec).is_some_and(|l| l.work.seeds_trusted());
                match self.d.fate(&exec, Some(i64::from(left)), true, FINISH_WITHIN) {
                    Some(Fate::Finish { left: f, finish_within, .. }) => {
                        prop_assert!(seeds && f == i64::from(left) && f <= finish_within);
                        bump(&self.seen.finished);
                    }
                    Some(Fate::Cancel { seeds: s, .. }) => {
                        prop_assert!(s == seeds && !(seeds && i64::from(left) <= FINISH_WITHIN));
                        bump(&self.seen.cancelled);
                        self.ended(&exec, End::Cancelled)?;
                    }
                    other => return Err(TestCaseError::fail(format!("unwanted running {exec}, contended: {other:?}"))),
                }
            }
            Op::Forget { pick } => {
                if let Some(leg) = choose(&self.legs_in(|st| matches!(st, St::Waits(_) | St::Checks(_))), pick) {
                    let was = self.legs.get(&leg).map(|l| l.st.clone());
                    let got = self.d.forget(&leg);
                    match was {
                        Some(St::Waits(exec)) => prop_assert_eq!(got, Some(exec)),
                        _ => prop_assert_eq!(got, None),
                    }
                    self.set(&leg, St::Ended)?;
                }
            }
            Op::Check { pick, matched } => {
                if let Some(leg) = choose(&self.legs_in(|st| matches!(st, St::Checks(_))), pick) {
                    let n = self.d.checked(&leg, matched).ok_or_else(|| TestCaseError::fail(format!("{leg} wasn't checking")))?;
                    match n {
                        Next::Reuse { from } => {
                            prop_assert!(matched);
                            prop_assert_eq!(self.legs.get(&leg).map(|l| &l.st), Some(&St::Checks(from)));
                            self.set(&leg, St::Ended)?;
                        }
                        // A mismatch: it runs itself, or adopts another execution, or (withdrawn) is dropped.
                        Next::Execute => {
                            prop_assert!(!matched);
                            bump(&self.seen.mismatched);
                            self.asked(&leg, Ask::Execute)?;
                        }
                        Next::Adopt { exec, run } => {
                            prop_assert!(!matched);
                            bump(&self.seen.mismatched);
                            self.asked(&leg, Ask::Adopt { exec, run })?;
                        }
                        Next::Drop => {
                            prop_assert!(!matched);
                            bump(&self.seen.mismatched);
                            self.asked(&leg, Ask::Drop)?;
                        }
                        Next::Check { .. } => return Err(TestCaseError::fail(format!("{leg} checks twice"))),
                    }
                }
            }
            Op::Restart { seed } => {
                // The server restarts: demands arrive again, then the legs come back in an order of
                // the seed's, the running ones announced and the others asked for again.
                self.d = Demands::default();
                for (dm, withdraw) in self.arrivals.clone() {
                    self.d.arrive(dm, withdraw);
                }
                let mut back: Vec<(u64, String)> = self.legs.iter().filter(|(_, l)| l.st != St::Ended).map(|(k, _)| (mix(seed, k), k.clone())).collect();
                back.sort();
                for (_, leg) in back {
                    let Some(l) = self.legs.get(&leg).cloned() else { continue };
                    if l.st == (St::Executes { started: true }) {
                        match self.d.running(&leg, &l.run, &l.work, l.priority, 0) {
                            Running::Executes => {}
                            Running::Took { queued } => {
                                bump(&self.seen.took);
                                // The queued duplicate and its adopters wait on this one now.
                                for w in self.legs.values_mut() {
                                    if w.st == St::Waits(queued.clone()) {
                                        w.st = St::Waits(leg.clone());
                                    }
                                }
                                self.set(&queued, St::Waits(leg.clone()))?;
                            }
                            Running::Alone { exec } => return Err(TestCaseError::fail(format!("{leg} and {exec} ran at once"))),
                        }
                    } else {
                        let ask = self.d.ask(&leg, &l.run, &l.work, l.priority);
                        self.set(&leg, St::Executes { started: false })?;
                        self.asked(&leg, ask)?;
                    }
                }
            }
        }
        self.invariants()
    }

    /// Single-flight and the domains, after every step (W28-AC40).
    fn invariants(&self) -> Result<(), TestCaseError> {
        let mut slots: BTreeMap<(String, Domain), Vec<&str>> = BTreeMap::new();
        for (id, l) in &self.legs {
            match &l.st {
                St::Executes { .. } => {
                    prop_assert!(self.d.is_execution(id), "{id} executes, but the table doesn't say so");
                    if let Some(k) = &l.work.key
                        && l.work.domain != Domain::Protected
                    {
                        slots.entry((k.clone(), l.work.domain.clone())).or_default().push(id);
                    }
                }
                St::Waits(exec) => {
                    prop_assert_eq!(self.d.adopts(id), Some(exec.as_str()));
                    let x = self.legs.get(exec).ok_or_else(|| TestCaseError::fail(format!("{id} waits on unknown {exec}")))?;
                    prop_assert!(matches!(x.st, St::Executes { .. }), "{id} waits on {exec}, which doesn't execute");
                    prop_assert!(l.work.key.is_some() && l.work.key == x.work.key, "{id} adopted other work");
                    prop_assert!(l.work.domain.may_adopt(&x.work.domain), "{} adopted {}'s {} execution", l.work.domain, exec, x.work.domain);
                }
                St::Checks(_) | St::Ended => prop_assert!(!self.d.is_execution(id) && self.d.adopts(id).is_none()),
            }
        }
        for (slot, legs) in slots {
            prop_assert!(legs.len() <= 1, "{:?} executes twice at once: {:?}", slot, legs);
        }
        Ok(())
    }

    /// Everything ends: checks match, and every execution succeeds, until nothing's left. Every leg
    /// has then ended exactly once (`set` refuses a second end), and the table holds nothing.
    fn drain(&mut self) -> Result<(), TestCaseError> {
        loop {
            let checking: Vec<(String, String)> = self.legs.iter().filter_map(|(k, l)| if let St::Checks(from) = &l.st { Some((k.clone(), from.clone())) } else { None }).collect();
            if let Some((leg, from)) = checking.first() {
                prop_assert_eq!(self.d.checked(leg, true), Some(Next::Reuse { from: from.clone() }));
                self.set(leg, St::Ended)?;
                continue;
            }
            if let Some(leg) = self.legs_in(|st| matches!(st, St::Executes { .. })).first() {
                self.ended(leg, End::Success)?;
                self.invariants()?;
                continue;
            }
            break;
        }
        for (id, l) in &self.legs {
            prop_assert_eq!(&l.st, &St::Ended, "{} never ended", id);
        }
        prop_assert!(self.d.is_empty(), "the table kept something: {:?}", self.d);
        Ok(())
    }
}

fn run_ops<'a>(ops: &[Op], seen: &'a Seen) -> Result<Model<'a>, TestCaseError> {
    let mut m = Model { d: Demands::default(), legs: BTreeMap::new(), at: BTreeMap::new(), arrivals: Vec::new(), seen };
    for (i, op) in ops.iter().enumerate() {
        m.apply(i, op)?;
    }
    Ok(m)
}

fn case(ops: &[Op], seen: &Seen) -> Result<(), TestCaseError> {
    bump(&seen.cases);
    let mut m = run_ops(ops, seen)?;
    // Pure: the same steps give the same table and the same legs, byte for byte.
    let unseen = Seen::default();
    let again = run_ops(ops, &unseen)?;
    prop_assert_eq!(format!("{:?}", m.d), format!("{:?}", again.d));
    prop_assert_eq!(format!("{:?}", m.legs), format!("{:?}", again.legs));
    m.drain()
}

#[test]
fn w28_ac40_single_flight_within_domains() -> Result<(), String> {
    let seen = Seen::default();
    let mut runner = TestRunner::new(Config { cases: 10_000, failure_persistence: None, ..Config::default() });
    runner.run(&prop::collection::vec(op(), 1..64), |ops| case(&ops, &seen)).map_err(|e| e.to_string())?;
    let (cases, adopted, took, reexecuted, cancelled, finished, mismatched) =
        (seen.cases.get(), seen.adopted.get(), seen.took.get(), seen.reexecuted.get(), seen.cancelled.get(), seen.finished.get(), seen.mismatched.get());
    println!("W28-AC40: {cases} cases; {adopted} adoptions, {took} queued duplicates taken over after a restart, {reexecuted} adopters executing after an end they couldn't share, {cancelled} cancels and {finished} finishes for live work, {mismatched} checks that didn't match");
    assert!(cases >= 10_000, "{cases} cases run");
    // Every path is taken, many times over: over 10,000 cases the rarest (finishes, checks that
    // didn't match) come out between 100 and 200.
    for (what, n) in [("adoptions", adopted), ("restart take-overs", took), ("re-executions", reexecuted), ("cancels", cancelled), ("finishes", finished), ("mismatched checks", mismatched)] {
        assert!(n >= 50, "only {n} {what}");
    }
    Ok(())
}

#[test]
fn w28_r46_supersede_reads() -> Result<(), String> {
    use crate::wf::ir::Tpl;
    let s = |x: &str| YVal::Str(Tpl::new(x, Default::default()));
    let map = |kv: Vec<(&str, YVal)>| YVal::Map(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
    let adopt = map(vec![("when", YVal::Seq(vec![s("branch"), s("pull_request")])), ("action", s("adopt")), ("keep", YVal::Seq(vec![s("release")])), ("finish-within", s("0s"))]);
    assert_eq!(
        Supersession::from_yval(&adopt)?,
        Some(Supersession { when: vec![When::Branch, When::PullRequest], action: Supersede::Adopt, keep: vec!["release".into()], finish_within: 0 })
    );
    assert_eq!(Supersession::from_yval(&map(Vec::new()))?, Some(Supersession { when: vec![When::Branch], action: Supersede::Cancel, keep: Vec::new(), finish_within: FINISH_WITHIN }));
    assert_eq!(Supersession::from_yval(&map(vec![("finish-within", s("5m"))]))?.map(|x| x.finish_within), Some(300));
    assert_eq!(Supersession::from_yval(&YVal::Null)?, None);
    for (bad, says) in [
        (map(vec![("action", s("sometimes"))]), "takes cancel, deprioritize or adopt"),
        (map(vec![("when", YVal::Seq(vec![s("tag")]))]), "takes branch, stack and pull_request"),
        (map(vec![("colour", s("red"))]), "isn't one of"),
        (map(vec![("finish-within", s("soon"))]), "is a duration"),
        (s("adopt"), "is a map"),
    ] {
        assert!(Supersession::from_yval(&bad).is_err_and(|e| e.contains(says)), "{bad:?}");
    }
    Ok(())
}
