//! Scheduler tests (docs/ci.md W05-AC01 to AC06, AC10, W05-S01 to S04), on the simulator.

use super::groups::GroupConfig;
use super::pools::PoolSpec;
use super::reserve::ReservationSpec;
use super::retry::{FailureClass, RetryPolicy};
use super::routing::{LegRoute, Preference, RunnerView, github_label_rules};
use super::sim::Sim;
use super::state::{Command, Config, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason};
use crate::facts::{Disk, RunnerFacts};

const MIN: i64 = 60_000;

fn linux(name: &str, slots: u32) -> RunnerIn {
    RunnerIn {
        id: name.into(),
        view: RunnerView { name: name.into(), labels: vec!["k8s".into()], facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 1 << 10, pool: None },
        slots,
        capacity: None,
        executor: None,
    }
}

fn leg(id: &str, repo: &str, priority: u8) -> LegIn {
    LegIn { id: id.into(), run: format!("run-{id}"), repo: repo.into(), route: LegRoute { labels: vec!["k8s".into()], ..LegRoute::default() }, priority, ..LegIn::default() }
}

#[test]
fn assigns_to_the_least_loaded_and_queues_the_rest() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 2));
    sim.runner(0, linux("b", 2));
    for i in 0..5 {
        sim.leg(1, leg(&format!("l{i}"), "acme/x", 60), 10 * MIN);
    }
    sim.run(2);
    let on: Vec<&str> = sim.assigned.iter().map(|a| a.runner.as_str()).collect();
    assert_eq!(on, vec!["a", "b", "a", "b"], "least loaded first, then most free, then name");
    assert!(matches!(sim.last_wait("l4"), Some(WaitReason::Capacity { eligible: 2, busy: 2, draining: 0 })));
    sim.run(11 * MIN);
    assert_eq!(sim.started("l4"), Some(10 * MIN + 1));
}

/// W05-AC03: a low-priority leg under a constant stream of high-priority legs starts within the
/// bound aging gives: (100 − 20) / 1 per minute = 80 minutes.
#[test]
fn w05_ac03_aging_bounds_starvation() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, leg("busy", "acme/x", 100), MIN / 2);
    sim.leg(1, leg("low", "acme/y", 20), MIN / 2);
    for i in 0..400 {
        sim.leg(i * MIN / 2 + 2, leg(&format!("high{i}"), "acme/x", 100), MIN / 2);
    }
    sim.run(200 * MIN);
    let start = sim.started("low").unwrap_or(i64::MAX);
    assert!((79 * MIN..=82 * MIN).contains(&start), "the low-priority leg started at {} min", start / MIN);
}

/// W05-AC04: two repositories, 1,000 one-minute legs each on 20 slots: within 10 % of an equal
/// share in every 10-minute window; 3:1 weights give 75 % ± 10 %.
#[test]
fn w05_ac04_fair_share() {
    for (wa, wb, want) in [(1.0, 1.0, 0.5), (3.0, 1.0, 0.75)] {
        let cfg = Config { weights: [("a/a".to_owned(), wa), ("b/b".to_owned(), wb)].into_iter().collect(), ..Config::default() };
        let mut sim = Sim::new(cfg);
        for r in 0..4 {
            sim.runner(0, linux(&format!("r{r}"), 5));
        }
        for i in 0..1_000 {
            sim.leg(1, leg(&format!("a{i}"), "a/a", 60), MIN);
        }
        for i in 0..1_000 {
            sim.leg(1, leg(&format!("b{i}"), "b/b", 60), MIN);
        }
        sim.run(200 * MIN);
        // While both still have work queued.
        let both_busy_until = if wa > wb { 60 * MIN } else { 90 * MIN };
        let mut w = 0;
        while (w + 10) * MIN <= both_busy_until {
            let share = sim.shares(w * MIN, (w + 10) * MIN).get("a/a").copied().unwrap_or(0.0);
            assert!((share - want).abs() <= 0.10, "weights {wa}:{wb}, window {w}–{}: a got {share:.2}", w + 10);
            w += 10;
        }
        assert_eq!(sim.settled.len(), 2_000);
    }
}

/// W05-AC01 and W05-S03 through the scheduler: a workflow-level group of runs.
#[test]
fn w05_s03_run_groups() {
    let mut s = Scheduler::new(Config::default());
    let g = || Some(("acme/x\0deploy-prod".to_owned(), "deploy-prod".to_owned(), GroupConfig::default()));
    let starts = |c: &[Command]| c.iter().filter_map(|x| if let Command::StartRun { run } = x { Some(run.clone()) } else { None }).collect::<Vec<_>>();
    assert_eq!(starts(&s.handle(Input::RunQueued { run: "r1".into(), group: g(), supersede: None }, 0)), vec!["r1"]);
    assert!(s.handle(Input::RunQueued { run: "r2".into(), group: g(), supersede: None }, 1).is_empty());
    let c = s.handle(Input::RunQueued { run: "r3".into(), group: g(), supersede: None }, 2);
    assert_eq!(c, vec![Command::CancelRun { run: "r2".into(), reason: "cancelled: superseded in group deploy-prod".into() }]);
    assert_eq!(starts(&s.handle(Input::RunFinished { run: "r1".into() }, 3)), vec!["r3"]);
}

/// W05-AC02: supersede on a branch cancels the older runs; without it all run.
#[test]
fn w05_ac02_supersede() {
    let mut s = Scheduler::new(Config::default());
    let sup = || Some(("ci.yml@refs/heads/main".to_owned(), Supersede::Cancel));
    let mut cancelled = Vec::new();
    for r in ["p1", "p2", "p3"] {
        for c in s.handle(Input::RunQueued { run: r.into(), group: None, supersede: sup() }, 0) {
            if let Command::CancelRun { run, reason } = c {
                cancelled.push((run, reason));
            }
        }
    }
    assert_eq!(cancelled, vec![("p1".to_owned(), "superseded by a newer run".to_owned()), ("p2".to_owned(), "superseded by a newer run".to_owned())]);
    let mut s = Scheduler::new(Config::default());
    let all: Vec<Command> = ["p1", "p2", "p3"].iter().flat_map(|r| s.handle(Input::RunQueued { run: (*r).into(), group: None, supersede: None }, 0)).collect();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|c| matches!(c, Command::StartRun { .. })));
}

#[test]
fn w05_ac01_job_level_groups_and_cancel_in_progress() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 4));
    let grouped = |id: &str, cancel: bool| LegIn { group: Some(("acme/x\0deploy".into(), "deploy".into(), GroupConfig { cancel_in_progress: cancel, ..GroupConfig::default() })), ..leg(id, "acme/x", 60) };
    sim.leg(0, grouped("d1", false), 10 * MIN);
    sim.leg(1, grouped("d2", false), 10 * MIN);
    sim.run(2);
    assert_eq!(sim.started("d1"), Some(0));
    assert!(matches!(sim.last_wait("d2"), Some(WaitReason::Concurrency { group, holders }) if group == "deploy" && holders == &vec!["d1".to_owned()]));
    sim.run(10 * MIN + 5);
    assert_eq!(sim.started("d2"), Some(10 * MIN), "d2 starts when d1 finishes");
    // cancel-in-progress stops d2 for d3.
    sim.leg(11 * MIN, grouped("d3", true), MIN);
    sim.run(11 * MIN + 1);
    assert!(sim.settled.iter().any(|(_, l, o)| l == "d2" && *o == Outcome::Cancelled));
    assert_eq!(sim.started("d3"), Some(11 * MIN));
}

/// W05-S01: a declared preference picks the runner with more disk, and says so; without one,
/// the less loaded runner.
#[test]
fn w05_s01_preferences() {
    let with_disk = |name: &str, free: u64| RunnerIn { view: RunnerView { facts: RunnerFacts { disk: Disk { total: 1 << 40, free }, ..linux(name, 4).view.facts }, ..linux(name, 4).view }, ..linux(name, 4) };
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(with_disk("a", 500 << 30))), 0);
    s.handle(Input::RunnerUp(Box::new(with_disk("b", 20 << 30))), 0);
    // B is busier? No: give A one leg first, so "least loaded" would pick B.
    s.handle(Input::LegQueued(Box::new(LegIn { route: LegRoute { labels: vec!["k8s".into()], requires: vec!["disk.free > 400Gi".into()], ..LegRoute::default() }, ..leg("warm", "acme/x", 60) })), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { prefer: vec![Preference { fact: Some("disk.free".into()), higher: true, ..Preference::default() }], ..leg("p", "acme/x", 60) })), 1);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "p" && runner == "a")), "{c:?}");
    let why = s.explain("p");
    assert!(why.iter().any(|e| e.runner == "a" && e.score == Some(1.0)) && why.iter().any(|e| e.runner == "b" && e.score == Some(0.0)), "{why:?}");
    let c = s.handle(Input::LegQueued(Box::new(leg("q", "acme/x", 60))), 2);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "q" && runner == "b")), "the less loaded: {c:?}");
}

/// W05-S02: GitHub labels with no runner that could carry them → unroutable; a runner that does
/// joins → assigned at once.
#[test]
fn w05_s02_unroutable_then_assigned() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("x86", 2))), 0);
    let l = LegIn { route: LegRoute { labels: vec!["self-hosted".into(), "linux".into(), "arm64".into()], ..LegRoute::default() }, ..leg("arm", "acme/x", 60) };
    s.handle(Input::LegQueued(Box::new(l)), 0);
    assert!(matches!(s.reason("arm"), Some(WaitReason::Unroutable { failing }) if failing.iter().any(|f| f.contains("arm64"))), "{:?}", s.reason("arm"));
    let arm = RunnerIn { view: RunnerView { facts: RunnerFacts { os: "linux".into(), arch: "aarch64".into(), ..RunnerFacts::default() }, ..linux("pi", 1).view }, ..linux("pi", 1) };
    let c = s.handle(Input::RunnerUp(Box::new(arm)), 5);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "arm" && runner == "pi")), "{c:?}");
}

#[test]
fn github_label_rules_are_off_until_enabled() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let l = LegIn { route: LegRoute { labels: vec!["ubuntu-latest".into()], ..LegRoute::default() }, ..leg("u", "acme/x", 60) };
    let c = s.handle(Input::LegQueued(Box::new(l.clone())), 0);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "the label must be on the runner");
    let mut rules = github_label_rules();
    rules.push(super::routing::native_rule());
    let c = s.handle(Input::RulesChanged(rules), 1);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "u")), "{c:?}");
}

/// W05-S04 and W05-AC06: retries by class, with backoff, on another runner after a lost one.
#[test]
fn w05_ac06_retries() {
    // A lost runner: retried once, elsewhere, at once.
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.runner(0, linux("b", 1));
    sim.leg(0, leg("x", "acme/x", 60), 10 * MIN);
    sim.run(1);
    let first = sim.assigned.first().map(|a| a.runner.clone()).unwrap_or_default();
    sim.input(MIN, Input::RunnerLost { runner: first.clone() });
    sim.run(MIN + 1);
    let second = sim.assigned.get(1).cloned();
    assert!(second.as_ref().is_some_and(|a| a.runner != first && a.attempt == 2 && a.at == MIN), "{second:?}");
    // OOM with a policy: a second attempt after the backoff; a plain failure isn't retried.
    let policy = RetryPolicy { max: 2, on: vec![super::retry::RetryOn::Class(FailureClass::Oom)], initial_ms: 10_000, factor: 2.0, max_ms: 60_000, runner: super::retry::RunnerChoice::Any };
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, LegIn { retry: Some(policy.clone()), request: Resources { mem: 4 << 30, ..Resources::default() }, ..leg("m", "acme/x", 60) }, MIN);
    sim.fail("m", &[FailureClass::Oom, FailureClass::Oom]);
    sim.run(10 * MIN);
    let at: Vec<(i64, u32)> = sim.assigned.iter().map(|a| (a.at, a.attempt)).collect();
    assert_eq!(at, vec![(0, 1), (MIN + 10_000, 2), (2 * MIN + 10_000 + 20_000, 3)]);
    assert_eq!(sim.settled.last().map(|(_, _, o)| o.clone()), Some(Outcome::Success));
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, LegIn { retry: Some(policy), ..leg("f", "acme/x", 60) }, MIN);
    sim.fail("f", &[FailureClass::Failure]);
    sim.run(10 * MIN);
    assert_eq!(sim.assigned.len(), 1);
    assert!(matches!(sim.settled.last(), Some((_, _, Outcome::Failed(f))) if f.class == FailureClass::Failure));
}

/// W05-AC10: the wait reasons, each from a scenario.
#[test]
fn w05_ac10_wait_reasons() {
    let mut s = Scheduler::new(Config { quotas: [("q/q".to_owned(), 1)].into_iter().collect(), ..Config::default() });
    // capability: the runner would do, but its build lacks CAP_CI2.
    let old = RunnerIn { view: RunnerView { caps: 0, ..linux("old", 1).view }, ..linux("old", 1) };
    s.handle(Input::RunnerUp(Box::new(old)), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { route: LegRoute { labels: vec!["k8s".into()], caps: 1 << 10, ..LegRoute::default() }, ..leg("c", "acme/x", 60) })), 0);
    assert!(matches!(s.reason("c"), Some(WaitReason::Capability { runners }) if runners == &vec!["old".to_owned()]));
    // no-runner: the eligible runner went offline.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("n", 1))), 1);
    s.handle(Input::RunnerLost { runner: "n".into() }, 2);
    s.handle(Input::LegQueued(Box::new(LegIn { route: LegRoute { labels: vec!["k8s".into()], requires: vec!["os == linux".into()], ..LegRoute::default() }, ..leg("o", "acme/x", 60) })), 3);
    assert!(matches!(s.reason("o"), Some(WaitReason::NoRunner { offline }) if offline.contains(&"n".to_owned())), "{:?}", s.reason("o"));
    // exclusive and quota.
    let mut s = Scheduler::new(Config { quotas: [("q/q".to_owned(), 1)].into_iter().collect(), ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 4))), 0);
    s.handle(Input::LegQueued(Box::new(leg("busy", "acme/x", 60))), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { exclusive: true, ..leg("ex", "acme/x", 60) })), 1);
    assert_eq!(s.reason("ex"), Some(&WaitReason::Exclusive));
    let c = s.handle(Input::AttemptFinished { leg: "busy".into(), attempt: 1, outcome: Outcome::Success }, 2);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "ex")));
    let c = s.handle(Input::LegQueued(Box::new(leg("after", "acme/x", 60))), 3);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "nothing shares the runner with an exclusive leg");
    s.handle(Input::RunnerUp(Box::new(linux("b", 4))), 4);
    s.handle(Input::LegQueued(Box::new(leg("q1", "q/q", 60))), 5);
    s.handle(Input::LegQueued(Box::new(leg("q2", "q/q", 60))), 6);
    assert_eq!(s.reason("q2"), Some(&WaitReason::Quota { limit: 1 }));
    // retry.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let policy = RetryPolicy { max: 1, on: vec![super::retry::RetryOn::Class(FailureClass::Infra)], initial_ms: 30_000, factor: 1.0, max_ms: 30_000, runner: super::retry::RunnerChoice::Any };
    s.handle(Input::LegQueued(Box::new(LegIn { retry: Some(policy), ..leg("r", "acme/x", 60) })), 0);
    s.handle(Input::AttemptFinished { leg: "r".into(), attempt: 1, outcome: Outcome::Failed(super::retry::Failed { class: FailureClass::Infra, exit_code: None, log_hits: Vec::new() }) }, 10);
    assert_eq!(s.reason("r"), Some(&WaitReason::Retry { at: 30_010 }));
}

/// A leg cancelled while it runs ends cancelled whatever its attempt reports, its runner lost
/// with it included, and isn't retried.
#[test]
fn a_cancelled_leg_is_never_retried() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("l", "acme/x", 60))), 0);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "l")));
    let c = s.handle(Input::LegCancelled { leg: "l".into(), reason: "the run was cancelled".into() }, 1);
    assert!(c.iter().any(|x| matches!(x, Command::Cancel { leg, .. } if leg == "l")));
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Retry { .. })), "{c:?}");
    assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, outcome: Outcome::Cancelled, .. } if leg == "l")), "{c:?}");
}

/// A leg gone with its repository leaves no trace: its runner is told to stop it, its slot goes
/// to the next leg at once, and nothing is settled or retried (its records went with it).
#[test]
fn a_leg_gone_with_its_repository_frees_its_runner() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("gone", "acme/gone", 60))), 0);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "gone")), "{c:?}");
    let c = s.handle(Input::LegQueued(Box::new(leg("next", "acme/kept", 60))), 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "the runner is full: {c:?}");
    let c = s.handle(Input::LegQueued(Box::new(leg("queued", "acme/gone", 60))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    // The queued one first: out of the queue, nothing said.
    let c = s.handle(Input::LegGone { leg: "queued".into() }, 3);
    assert!(c.iter().all(|x| !matches!(x, Command::Settled { .. } | Command::Assign { .. })), "{c:?}");
    // The running one: its runner stops it, and the slot waits for the runner to say so.
    let c = s.handle(Input::LegGone { leg: "gone".into() }, 4);
    assert!(c.iter().any(|x| matches!(x, Command::Cancel { leg, .. } if leg == "gone")), "its runner stops it: {c:?}");
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. } | Command::Settled { .. })), "{c:?}");
    // Its attempt's end, killed or lost with its runner, frees the slot for the next leg, and
    // nothing is settled or retried (its records went with it).
    let c = s.handle(Input::AttemptFinished { leg: "gone".into(), attempt: 1, outcome: Outcome::Cancelled }, 5);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "next" && runner == "a")), "the slot is the next leg's: {c:?}");
    assert!(!c.iter().any(|x| matches!(x, Command::Settled { .. } | Command::Retry { .. })), "{c:?}");
    let c = s.handle(Input::AttemptFinished { leg: "gone".into(), attempt: 1, outcome: Outcome::Cancelled }, 6);
    assert!(c.is_empty(), "once: {c:?}");
}

#[test]
fn resources_mode_and_cancellation() {
    let cfg = Config { resources: true, ..Config::default() };
    let mut s = Scheduler::new(cfg);
    let big = RunnerIn { capacity: Some(Resources { cpu: 4.0, mem: 8 << 30, disk: 100 << 30, gpu: 0 }), ..linux("a", 99) };
    s.handle(Input::RunnerUp(Box::new(big)), 0);
    let cpu = |id: &str, c: f64| LegIn { request: Resources { cpu: c, mem: 1 << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) };
    let mut assigned = 0;
    for i in 0..4 {
        assigned += s.handle(Input::LegQueued(Box::new(cpu(&format!("c{i}"), 2.0))), i).iter().filter(|c| matches!(c, Command::Assign { .. })).count();
    }
    assert_eq!(assigned, 3, "6 CPUs fit 4 × 1.5 overcommit; the fourth waits");
    // Cancelling a queued leg settles it; a running one is stopped on its runner.
    let c = s.handle(Input::LegCancelled { leg: "c3".into(), reason: "fail-fast".into() }, 10);
    assert_eq!(c, vec![Command::Settled { leg: "c3".into(), outcome: Outcome::Cancelled, reason: Some("fail-fast".into()) }]);
    let c = s.handle(Input::LegCancelled { leg: "c0".into(), reason: "run cancelled".into() }, 11);
    assert_eq!(c, vec![Command::Cancel { leg: "c0".into(), attempt: 1, reason: "run cancelled".into() }]);
}

/// W05-AC09's core: after a restart, running legs are adopted (not assigned again) and their
/// runner's capacity counts them.
#[test]
fn restart_adopts_running_legs() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::LegRunning { leg: Box::new(leg("run1", "acme/x", 60)), runner: "a".into(), attempt: 2 }, 0);
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 1);
    assert!(c.is_empty(), "nothing assigned twice: {c:?}");
    let c = s.handle(Input::LegQueued(Box::new(leg("next", "acme/x", 60))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "the adopted leg holds the slot");
    let c = s.handle(Input::AttemptFinished { leg: "run1".into(), attempt: 2, outcome: Outcome::Success }, 3);
    assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, .. } if leg == "run1")));
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "next")));
}

#[test]
fn a_rejected_assignment_goes_elsewhere_on_the_same_attempt() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { runner, attempt: 1, .. } if runner == "a")));
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 1);
    let c = s.handle(Input::Rejected { leg: "x".into() }, 2);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { runner, attempt: 1, .. } if runner == "b")), "{c:?}");
    // With no other runner, it waits for one, and says so.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let c = s.handle(Input::Rejected { leg: "x".into() }, 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("x"), Some(&WaitReason::OtherRunner { avoided: "a".into(), refused: true }));
}

/// W05-R16: the default retry after `runner-lost` goes to another runner when one could take it,
/// else to the same one; a policy's `runner: different` waits for another, and says so.
#[test]
fn w05_r16_a_lost_runners_retry() {
    let assigned = |c: &[Command], l: &str, r: &str, n: u32| c.iter().any(|x| matches!(x, Command::Assign { leg, runner, attempt, .. } if leg == l && runner == r && *attempt == n));
    // The only runner: lost, then back, it takes the retry.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    assert!(assigned(&c, "x", "a", 1), "{c:?}");
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 1);
    assert!(c.iter().any(|x| matches!(x, Command::Retry { leg, attempt: 2, .. } if leg == "x")) && !c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 2);
    assert!(assigned(&c, "x", "a", 2), "no other could take it: {c:?}");
    // Two runners: the retry waits for the other while it's busy, and takes the lost one once
    // the other goes too, before the other's own leg.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let first = if assigned(&c, "x", "a", 1) { "a" } else { "b" };
    let other = if first == "a" { "b" } else { "a" };
    let c = s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 0);
    assert!(assigned(&c, "y", other, 1), "{c:?}");
    s.handle(Input::RunnerLost { runner: first.into() }, 1);
    let c = s.handle(Input::RunnerUp(Box::new(linux(first, 1))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "x")), "another could take it: {c:?}");
    assert!(matches!(s.reason("x"), Some(WaitReason::Capacity { .. })), "{:?}", s.reason("x"));
    let c = s.handle(Input::RunnerLost { runner: other.into() }, 3);
    assert!(assigned(&c, "x", first, 2), "{c:?}");
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "y")), "the runner is x's: {c:?}");
    // A draining other is no other.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let first = if assigned(&c, "x", "a", 1) { "a" } else { "b" };
    let other = if first == "a" { "b" } else { "a" };
    s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 0);
    s.handle(Input::RunnerLost { runner: first.into() }, 1);
    s.handle(Input::RunnerUp(Box::new(linux(first, 1))), 2);
    let c = s.handle(Input::RunnerDraining { runner: other.into() }, 3);
    assert!(assigned(&c, "x", first, 2), "{c:?}");
    // `runner: different`: the only runner back, the retry waits for another.
    let strict = RetryPolicy { max: 1, on: vec![super::retry::RetryOn::Class(FailureClass::RunnerLost)], initial_ms: 0, factor: 1.0, max_ms: 0, runner: super::retry::RunnerChoice::Different };
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { retry: Some(strict), ..leg("z", "acme/x", 60) })), 0);
    assert!(assigned(&c, "z", "a", 1), "{c:?}");
    s.handle(Input::RunnerLost { runner: "a".into() }, 1);
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("z"), Some(&WaitReason::OtherRunner { avoided: "a".into(), refused: false }));
    let c = s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 3);
    assert!(assigned(&c, "z", "b", 2), "{c:?}");
}

// ------------------------------------------------------------------ pools (W13-R09, W05-R27)

fn docker_host(name: &str) -> RunnerIn {
    let mut r = linux(name, 2);
    r.view.labels = vec!["linux".into()];
    r.view.facts.executors.insert("docker".into(), crate::facts::Executor::default());
    r
}

fn pool(min_idle: usize, max: usize, per_host_max: usize, idle: &str) -> Result<super::pools::Pool, String> {
    PoolSpec {
        name: "burst".into(),
        executor: "docker".into(),
        hosts: Some("executors contains docker".into()),
        image: Some("kiln/runner-ubuntu-24.04".into()),
        labels: vec!["burst".into(), "linux".into()],
        min_idle,
        max: Some(max),
        per_host_max: Some(per_host_max),
        idle_timeout: Some(idle.into()),
        ..PoolSpec::default()
    }
    .pool()
}

fn pool_leg(id: &str) -> LegIn {
    LegIn { id: id.into(), run: "run-burst".into(), repo: "acme/x".into(), route: LegRoute { labels: vec!["burst".into()], ..LegRoute::default() }, priority: 50, ..LegIn::default() }
}

fn counts(sim: &Sim) -> (usize, usize, usize) {
    sim.sched.pools().first().map_or((0, 0, 0), |p| (p.starting, p.idle, p.busy))
}

/// A pool keeps `min_idle` executors ready, a leg of its labels runs in one (which is then
/// discarded), on the hosts its `hosts` names only, and the host's own slots stay its own.
#[test]
fn w13_r09_pool_runs_legs_in_its_executors() -> Result<(), String> {
    let mut sim = Sim::new(Config { pools: vec![pool(2, 4, 4, "10m")?], ..Config::default() });
    sim.runner(0, docker_host("d1"));
    sim.runner(0, linux("plain", 2));
    sim.run(MIN);
    assert_eq!(counts(&sim), (0, 2, 0), "min_idle ready");
    assert!(sim.executors_started.iter().all(|(_, _, host)| host == "d1"), "only hosts with Docker: {:?}", sim.executors_started);
    sim.leg(MIN, pool_leg("p1"), 30_000);
    sim.leg(MIN, leg("own", "acme/x", 50), 30_000);
    sim.run(MIN + 1);
    let p1 = sim.assigned.iter().find(|a| a.leg == "p1").ok_or("p1 never ran")?;
    assert_eq!(p1.runner, "d1");
    let slot = sim.runs.is_empty() && sim.started("own").is_some();
    assert!(slot, "the host's own leg ran in its slots beside the pool's");
    // Its executor was busy, then discarded; min_idle is kept.
    sim.run(2 * MIN);
    assert_eq!(counts(&sim).2, 0, "nothing busy");
    assert_eq!(counts(&sim).1, 2, "min_idle kept after the leg's executor was discarded");
    Ok(())
}

/// W13-AC04, on the simulator (the E2E runs it with Docker): a burst of 200 legs on a pool with
/// `min_idle = 2` and `max = 32` scales its executors to 32, drains the queue, and is back to
/// 2 within `idle_timeout`.
#[test]
fn w13_ac04_pool_burst_on_the_simulator() -> Result<(), String> {
    let mut sim = Sim::new(Config { pools: vec![pool(2, 32, 16, "2m")?], ..Config::default() });
    sim.runner(0, docker_host("h1"));
    sim.runner(0, docker_host("h2"));
    sim.run(MIN);
    assert_eq!(counts(&sim), (0, 2, 0));
    for i in 0..200 {
        sim.leg(MIN, pool_leg(&format!("b{i}")), 20_000);
    }
    let (mut peak, mut t) = (0, MIN);
    while sim.settled.len() < 200 && t < 60 * MIN {
        t += 250;
        sim.run(t);
        let (s, i, b) = counts(&sim);
        peak = peak.max(s + i + b);
        assert!(s + i + b <= 32, "never more than max: {}", s + i + b);
    }
    assert_eq!(sim.settled.len(), 200, "the queue drained");
    assert!(sim.settled.iter().all(|(_, _, o)| *o == Outcome::Success));
    assert_eq!(peak, 32, "scaled to max");
    // Each host held at most per_host_max at once.
    for host in ["h1", "h2"] {
        let on: usize = sim.sched.pools().first().map_or(0, |p| p.executors.iter().filter(|e| e.runner == host).count());
        assert!(on <= 16, "{host}: {on}");
    }
    let drained = t;
    sim.run(drained + 2 * MIN + 1_000);
    assert_eq!(counts(&sim), (0, 2, 0), "back to min_idle within idle_timeout");
    Ok(())
}

/// A host that can't start the pool's executors is left alone, longer each time, and the legs
/// say why they wait; another host takes them.
#[test]
fn w13_r09_failing_host_backs_off() -> Result<(), String> {
    let mut sim = Sim::new(Config { pools: vec![pool(0, 4, 4, "10m")?], ..Config::default() });
    sim.failing_hosts.insert("bad".into());
    sim.runner(0, docker_host("bad"));
    sim.leg(1, pool_leg("w1"), 10_000);
    sim.run(20_000);
    let tries = sim.executors_started.iter().filter(|(_, _, h)| h == "bad").count();
    assert!((2..=6).contains(&tries), "backing off: {tries} tries in 20 s");
    assert!(sim.waits.iter().any(|(_, l, r)| l == "w1" && matches!(r, WaitReason::Scaling { pool, starting: 1, .. } if pool == "burst")), "{:?}", sim.waits);
    let view = sim.sched.pools();
    assert!(view.first().and_then(|p| p.last_error.clone()).is_some_and(|(h, _)| h == "bad"));
    sim.runner(20_000, docker_host("good"));
    sim.run(40_000);
    assert_eq!(sim.assigned.iter().find(|a| a.leg == "w1").map(|a| a.runner.as_str()), Some("good"));
    Ok(())
}

fn pod_pool(min_idle: usize, max: usize) -> Result<super::pools::Pool, String> {
    PoolSpec { name: "pods".into(), executor: "k8s".into(), labels: vec!["pod".into()], min_idle, max: Some(max), idle_timeout: Some("1m".into()), ..PoolSpec::default() }.pool()
}

fn pod_leg(id: &str) -> LegIn {
    LegIn { id: id.into(), run: "run-pods".into(), repo: "acme/x".into(), route: LegRoute { labels: vec!["pod".into()], ..LegRoute::default() }, priority: 50, ..LegIn::default() }
}

/// W13-R07: a `k8s` pool's executors are the server's, each a pod whose own runner, bound to it
/// as it connects, takes one of the pool's legs and nothing else, `max` of them at once. A runner
/// with the pool's labels takes none of its legs; a pod the scheduler doesn't know is told to go.
#[test]
fn w13_r07_k8s_pool_pods_are_their_own_runners() -> Result<(), String> {
    let mut sim = Sim::new(Config { pools: vec![pod_pool(0, 3)?], ..Config::default() });
    let mut lookalike = linux("lookalike", 4);
    lookalike.view.labels = vec!["pod".into()];
    sim.runner(0, lookalike);
    for i in 0..5 {
        sim.leg(1, pod_leg(&format!("p{i}")), 10_000);
    }
    let mut anywhere = leg("anywhere", "acme/x", 50);
    anywhere.route.labels = vec!["self-hosted".into()];
    sim.leg(1, anywhere, 10_000);
    let (mut t, mut peak) = (1, 0);
    while sim.settled.len() < 6 && t < 5 * MIN {
        t += 250;
        sim.run(t);
        let (s, i, b) = counts(&sim);
        peak = peak.max(s + i + b);
    }
    assert_eq!(sim.settled.len(), 6, "every leg ran: {:?}", sim.settled);
    assert!(sim.executors_started.iter().all(|(_, _, host)| host.is_empty()), "the server starts them: {:?}", sim.executors_started);
    assert_eq!(peak, 3, "max at once");
    let pods: Vec<&str> = sim.assigned.iter().filter(|a| a.leg.starts_with('p')).map(|a| a.runner.as_str()).collect();
    assert_eq!(pods.len(), 5);
    assert!(pods.iter().all(|r| r.starts_with("pod-pool-pods-")), "each in a pod: {pods:?}");
    let mut once = pods.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(once.len(), 5, "a pod takes one leg");
    assert_eq!(sim.assigned.iter().find(|a| a.leg == "anywhere").map(|a| a.runner.as_str()), Some("lookalike"), "no pod takes another leg");
    sim.run(t + 2 * MIN);
    assert_eq!(counts(&sim), (0, 0, 0), "each pod went with its leg");
    // A pod from before a restart: told to go, and given nothing.
    sim.runner(t + 3 * MIN, RunnerIn { id: "pod-old".into(), slots: 1, executor: Some("pool-pods-old".into()), ..RunnerIn::default() });
    sim.leg(t + 3 * MIN, pod_leg("late"), 10_000);
    sim.run(t + 4 * MIN);
    assert!(sim.executors_stopped.iter().any(|(_, e)| e == "pool-pods-old"), "{:?}", sim.executors_stopped);
    assert_ne!(sim.assigned.iter().find(|a| a.leg == "late").map(|a| a.runner.as_str()), Some("pod-old"));
    Ok(())
}

/// W13-R07: a leg with containers gets a pod made for it on a `k8s` pool (its job container and
/// services in it), and only that one; the pool's other legs share pods made for none.
#[test]
fn w13_r07_a_leg_with_containers_gets_a_pod_of_its_own() -> Result<(), String> {
    let mut sim = Sim::new(Config { pools: vec![pod_pool(0, 4)?], ..Config::default() });
    let mut db = pod_leg("with-db");
    db.own_executor = true;
    sim.leg(1, db, 10_000);
    sim.leg(1, pod_leg("plain"), 10_000);
    sim.run(2 * MIN);
    assert_eq!(sim.settled.len(), 2, "{:?}", sim.settled);
    let made: Vec<Option<String>> = sim.sched.pools().first().map(|p| p.executors.iter().map(|e| e.leg.clone()).collect()).unwrap_or_default();
    assert!(made.is_empty(), "each pod went with its leg: {made:?}");
    // The leg with containers ran on the pod started for it, the other on one started for none.
    let pod_of = |leg: &str| sim.assigned.iter().find(|a| a.leg == leg).map(|a| a.runner.clone()).unwrap_or_default();
    assert_ne!(pod_of("with-db"), pod_of("plain"));
    assert_eq!(sim.executors_started.len(), 2, "{:?}", sim.executors_started);
    // Cancelled while its pod starts, the pod made for it goes too.
    let mut late = pod_leg("late");
    late.own_executor = true;
    sim.leg(3 * MIN, late, 10_000);
    sim.input(3 * MIN + 1, Input::LegCancelled { leg: "late".into(), reason: "cancelled".into() });
    sim.run(4 * MIN);
    assert_eq!(counts(&sim), (0, 0, 0), "{:?}", sim.sched.pools());
    Ok(())
}

/// W13-R07: a `k8s` pool may have a Docker sidecar in its pods (Podman's by default), which
/// `pool_for` says of the pool a leg goes to; no other kind of pool has one, nor another engine.
#[test]
fn w13_r07_a_pools_docker_sidecar() -> Result<(), String> {
    let mut spec = PoolSpec { name: "pods".into(), executor: "k8s".into(), labels: vec!["pod".into()], docker: Some(super::pools::PodDocker::default()), ..PoolSpec::default() };
    let docker = spec.docker.clone().unwrap_or_default();
    assert_eq!((docker.engine(), docker.image(), docker.cli_image()), ("podman", "quay.io/podman/stable".to_owned(), "docker:cli".to_owned()));
    let sched = Scheduler::new(Config { pools: vec![spec.pool()?, pod_pool(0, 1).map(|p| super::pools::Pool { name: "plain".into(), labels: vec!["plain".into()], ..p })?], ..Config::default() });
    let route = |label: &str| LegRoute { labels: vec![label.into()], ..LegRoute::default() };
    assert_eq!(sched.pool_for(&route("pod")).map(|p| (p.name.as_str(), p.docker)), Some(("pods", true)));
    assert_eq!(sched.pool_for(&route("plain")).map(|p| (p.name.as_str(), p.docker)), Some(("plain", false)));
    spec.docker = Some(super::pools::PodDocker { engine: Some("containerd".into()), ..super::pools::PodDocker::default() });
    assert!(spec.pool().is_err_and(|e| e.contains("`podman` or `docker`")));
    let on_docker = PoolSpec { executor: "docker".into(), image: Some("ubuntu".into()), docker: Some(super::pools::PodDocker::default()), ..spec };
    assert!(on_docker.pool().is_err_and(|e| e.contains("`k8s` pool's pods")));
    Ok(())
}

// ------------------------------------------------------------------ reservations (W05-R12)

const H: i64 = 60 * MIN;

fn wf_leg(id: &str, workflow: &str) -> LegIn {
    let mut l = leg(id, "acme/x", 50);
    l.route.workflow = workflow.into();
    l
}

/// W05-R12: a runner kept for `bench` legs from 02:00 for two hours takes only those in the
/// window; another waits with reason `reservation` until it ends; one already running finishes.
#[test]
fn w05_r12_reservation_keeps_a_runner() -> Result<(), String> {
    let res = ReservationSpec { name: "nightly".into(), runners: Some("custom.rack == bench".into()), matches: "workflow == 'bench'".into(), cron: "0 2 * * *".into(), duration: "2h".into(), ..ReservationSpec::default() }.reservation()?;
    let mut sim = Sim::new(Config { reservations: vec![res], ..Config::default() });
    let mut bench = linux("bench", 1);
    bench.view.facts.custom.insert("rack".into(), "bench".into());
    sim.runner(0, bench);
    sim.leg(H + 50 * MIN, wf_leg("early", "build"), 20 * MIN);
    sim.leg(2 * H + 30 * MIN, wf_leg("late", "build"), MIN);
    sim.leg(2 * H + 35 * MIN, wf_leg("kept", "bench"), MIN);
    sim.run(5 * H);
    assert_eq!(sim.started("early"), Some(H + 50 * MIN), "before the window, anything");
    assert!(sim.settled.iter().any(|(t, l, o)| l == "early" && *o == Outcome::Success && *t == 2 * H + 10 * MIN), "running into the window, it finishes: {:?}", sim.settled);
    assert_eq!(sim.started("kept"), Some(2 * H + 35 * MIN), "the leg it's kept for");
    assert!(sim.waits.iter().any(|(_, l, r)| l == "late" && matches!(r, WaitReason::Reservation { reservation, until } if reservation == "nightly" && *until == 4 * H)), "{:?}", sim.waits);
    assert_eq!(sim.started("late"), Some(4 * H), "once the window ends");
    // A runner it doesn't keep takes anything.
    let res = ReservationSpec { runners: Some("custom.rack == bench".into()), matches: "workflow == 'bench'".into(), cron: "0 2 * * *".into(), duration: "2h".into(), ..ReservationSpec::default() }.reservation()?;
    let mut sim = Sim::new(Config { reservations: vec![res], ..Config::default() });
    sim.runner(0, linux("other", 1));
    sim.leg(2 * H + 30 * MIN, wf_leg("late", "build"), MIN);
    sim.run(3 * H);
    assert_eq!(sim.started("late"), Some(2 * H + 30 * MIN));
    Ok(())
}

/// A pool's `reserve`: its executors take only the legs it's kept for in the window.
#[test]
fn w05_r12_reserved_pool() -> Result<(), String> {
    let mut spec = PoolSpec {
        name: "burst".into(),
        executor: "docker".into(),
        image: Some("kiln/runner-ubuntu-24.04".into()),
        labels: vec!["burst".into()],
        min_idle: 1,
        max: Some(2),
        ..PoolSpec::default()
    };
    spec.reserve = Some(ReservationSpec { matches: "workflow == 'bench'".into(), cron: "0 2 * * *".into(), duration: "1h".into(), ..ReservationSpec::default() });
    let res = spec.reservation().ok_or("no reservation")??;
    assert_eq!((res.name.as_str(), res.pool.as_deref()), ("pool burst", Some("burst")));
    let mut sim = Sim::new(Config { pools: vec![spec.pool()?], reservations: vec![res], ..Config::default() });
    sim.runner(0, docker_host("d1"));
    let mut other = pool_leg("other");
    other.route.workflow = "build".into();
    let mut kept = pool_leg("kept");
    kept.route.workflow = "bench".into();
    sim.leg(2 * H + 10 * MIN, other, MIN);
    sim.leg(2 * H + 20 * MIN, kept, MIN);
    sim.run(4 * H);
    assert_eq!(sim.started("kept"), Some(2 * H + 20 * MIN));
    assert_eq!(sim.started("other"), Some(3 * H), "after the window");
    Ok(())
}

/// W05-R03: a request as written. CPUs as Kubernetes writes them (`500m` is half of one, not 500
/// minutes), memory and disk in bytes with their units, GPUs; a pool's the same way.
#[test]
fn w05_r03_requests_as_written() -> Result<(), String> {
    use super::pools::{ResourceSpec, cpus};
    assert_eq!(cpus("2"), Some(2.0));
    assert_eq!(cpus("500m"), Some(0.5));
    assert_eq!(cpus("0.25"), Some(0.25));
    assert_eq!(cpus("-1"), None);
    assert_eq!(cpus("lots"), None);
    let r = ResourceSpec { cpu: Some("1500m".into()), memory: Some("2Gi".into()), disk: Some("10G".into()), gpu: Some(1) }.resources()?;
    assert_eq!(r, Resources { cpu: 1.5, mem: 2 << 30, disk: 10_000_000_000, gpu: 1 });
    assert_eq!(ResourceSpec::default().resources()?, Resources::default(), "nothing asked");
    assert!(ResourceSpec { memory: Some("much".into()), ..ResourceSpec::default() }.resources().is_err());
    let pool = PoolSpec {
        name: "p".into(),
        executor: "docker".into(),
        image: Some("alpine:3.20".into()),
        labels: vec!["p".into()],
        resources: Some(ResourceSpec { cpu: Some("100m".into()), memory: Some("64Mi".into()), ..ResourceSpec::default() }),
        ..PoolSpec::default()
    }
    .pool()?;
    assert_eq!((pool.request.cpu, pool.request.mem), (0.1, 64 << 20), "a pool's executor asks a tenth of a CPU");
    Ok(())
}

// ------------------------------------------------------------------ demands (W28-R45, W28-AC41)

use super::demand::{Ask, Demand, Domain, Fate, Next, Work};
use super::retry::Failed;
use super::state::Decision;

const LANE: &str = "acme/ci@refs/heads/main";

/// A push's run arrives: its demand, then its `RunQueued`, which withdraws older demands on its
/// lane (`supersede: { action: adopt }`).
fn push(s: &mut Scheduler, run: &str, at: i64) -> Vec<Command> {
    let demand = Demand { run: run.into(), lane: LANE.into(), commit: format!("c-{run}"), domain: Domain::Trusted, at, ..Demand::default() };
    let mut c = s.handle(Input::Demand(Box::new(demand)), at);
    c.extend(s.handle(Input::RunQueued { run: run.into(), group: None, supersede: Some((LANE.into(), Supersede::Adopt)) }, at));
    c
}

/// `run`'s leg of job `job`, its work's key `key`, expected to take `cost` seconds.
fn job(run: &str, job: &str, key: &str, cost: f64) -> LegIn {
    LegIn {
        id: format!("{run}/{job}"),
        run: run.into(),
        repo: "acme/x".into(),
        route: LegRoute { labels: vec!["k8s".into()], ..LegRoute::default() },
        priority: 60,
        cost: Some(cost),
        work: Some(Work { key: Some(key.into()), domain: Domain::Trusted, lane: format!("acme/x/{job}"), observed: false }),
        ..LegIn::default()
    }
}

fn assigns(c: &[Command]) -> Vec<(String, String)> {
    c.iter().filter_map(|x| if let Command::Assign { leg, runner, .. } = x { Some((leg.clone(), runner.clone())) } else { None }).collect()
}

/// A leg's last decision in demands among `c`.
fn decision(c: &[Command], leg: &str) -> Option<Decision> {
    c.iter().rev().find_map(|x| match x {
        Command::Demand { leg: l, decision } if l == leg => Some(decision.clone()),
        _ => None,
    })
}

fn endings(c: &[Command]) -> Vec<Command> {
    c.iter().filter(|x| matches!(x, Command::Settled { .. } | Command::Adopted { .. })).cloned().collect()
}

fn adopted(leg: &str, exec: &str, run: &str) -> Command {
    Command::Wait { leg: leg.into(), reason: WaitReason::Adopted { leg: exec.into(), run: run.into() } }
}

/// W28-AC41: push A, then push B touching only docs/ while A's rust leg runs. B's rust leg has
/// A's key and adopts A's execution: one execution, both legs green, A never cancelled.
#[test]
fn w28_ac41_a_docs_only_push_adopts_the_rust_leg() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 0);
    let mut all = push(&mut s, "A", 0);
    let c = s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0);
    assert_eq!(assigns(&c), [("A/rust".to_owned(), "a".to_owned())]);
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Asked(Ask::Execute)));
    all.extend(c);
    // B arrives while it runs: A's demand is withdrawn, and its work runs on.
    let c = push(&mut s, "B", 60_000);
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Fate(Fate::Run { by: vec!["B".into()] })));
    all.extend(c);
    let c = s.handle(Input::LegQueued(Box::new(job("B", "rust", "K1", 200.0))), 60_000);
    assert!(c.contains(&adopted("B/rust", "A/rust", "A")), "{c:?}");
    assert_eq!(decision(&c, "B/rust"), Some(Decision::Asked(Ask::Adopt { exec: "A/rust".into(), run: "A".into() })));
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Fate(Fate::Kept { priority: 60, by: vec!["B".into()] })), "wanted again");
    all.extend(c);
    let c = s.handle(Input::AttemptFinished { leg: "A/rust".into(), attempt: 1, outcome: Outcome::Success }, 200_000);
    assert_eq!(
        endings(&c),
        [Command::Settled { leg: "A/rust".into(), outcome: Outcome::Success, reason: None }, Command::Adopted { leg: "B/rust".into(), from: "A/rust".into(), outcome: Outcome::Success }]
    );
    all.extend(c);
    assert_eq!(assigns(&all).len(), 1, "one execution: {all:?}");
    assert!(!all.iter().any(|x| matches!(x, Command::Cancel { .. } | Command::CancelRun { .. })), "A is never cancelled");
}

/// W28-AC41: push B touching crates/ while A's rust leg is 30% done and both slots are busy. B's
/// ui leg adopts A's (the same key). B's rust leg contends for a slot: A's rust leg, which no live
/// demand wants, is cancelled for it, saying how long it had left, and B's runs once it stopped.
#[test]
fn w28_ac41_a_crates_push_takes_the_unwanted_rust_legs_slot() {
    let mut s = Scheduler::new(Config { finish_within: 60, ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 0);
    push(&mut s, "A", 0);
    let mut c = s.handle(Input::LegQueued(Box::new(job("A", "ui", "U1", 300.0))), 0);
    c.extend(s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0));
    assert_eq!(assigns(&c).len(), 2, "both slots busy");
    // 60 s into A's rust leg's 200, B arrives.
    let c = push(&mut s, "B", 60_000);
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Fate(Fate::Run { by: vec!["B".into()] })));
    assert_eq!(decision(&c, "A/ui"), Some(Decision::Fate(Fate::Run { by: vec!["B".into()] })));
    let c = s.handle(Input::LegQueued(Box::new(job("B", "ui", "U1", 300.0))), 60_000);
    assert!(c.contains(&adopted("B/ui", "A/ui", "A")), "{c:?}");
    assert_eq!(decision(&c, "A/ui"), Some(Decision::Fate(Fate::Kept { priority: 60, by: vec!["B".into()] })));
    let c = s.handle(Input::LegQueued(Box::new(job("B", "rust", "K2", 200.0))), 60_000);
    let cancelled = Fate::Cancel { by: vec!["B".into()], left: Some(140), finish_within: 60, seeds: true, lane: "acme/x/rust".into() };
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Fate(cancelled.clone())));
    assert!(c.contains(&Command::Cancel { leg: "A/rust".into(), attempt: 1, reason: cancelled.why() }), "{c:?}");
    assert!(cancelled.why().contains("no live demand wants it") && cancelled.why().contains("about 140 s left"), "{}", cancelled.why());
    assert!(assigns(&c).is_empty(), "B's rust leg waits for A's to stop");
    let c = s.handle(Input::AttemptFinished { leg: "A/rust".into(), attempt: 1, outcome: Outcome::Cancelled }, 61_000);
    assert_eq!(endings(&c), [Command::Settled { leg: "A/rust".into(), outcome: Outcome::Cancelled, reason: None }]);
    assert_eq!(assigns(&c), [("B/rust".to_owned(), "a".to_owned())]);
    let c = s.handle(Input::AttemptFinished { leg: "A/ui".into(), attempt: 1, outcome: Outcome::Success }, 300_000);
    assert_eq!(endings(&c), [Command::Settled { leg: "A/ui".into(), outcome: Outcome::Success, reason: None }, Command::Adopted { leg: "B/ui".into(), from: "A/ui".into(), outcome: Outcome::Success }]);
}

/// W28-AC41: contended, work no one wants that is within finish_within of its end and seeds
/// trusted entries finishes; live work waits for it.
#[test]
fn w28_r45_contended_work_near_its_end_finishes() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    push(&mut s, "A", 0);
    s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0);
    push(&mut s, "B", 60_000);
    let c = s.handle(Input::LegQueued(Box::new(job("B", "rust", "K2", 200.0))), 60_000);
    assert_eq!(decision(&c, "A/rust"), Some(Decision::Fate(Fate::Finish { by: vec!["B".into()], left: 140, finish_within: 300 })));
    assert!(!c.iter().any(|x| matches!(x, Command::Cancel { .. })), "{c:?}");
    let c = s.handle(Input::AttemptFinished { leg: "A/rust".into(), attempt: 1, outcome: Outcome::Success }, 200_000);
    assert_eq!(assigns(&c), [("B/rust".to_owned(), "a".to_owned())]);
}

/// W28-AC41: after a restart, B's leg, queued with A's key, is asked for before A's running leg is
/// announced. It becomes A's adopter, and the work runs once.
#[test]
fn w28_ac41_a_restart_ends_with_one_execution() {
    let mut s = Scheduler::new(Config::default());
    let c = s.handle(Input::LegQueued(Box::new(job("B", "rust", "K1", 200.0))), 0);
    assert_eq!(decision(&c, "B/rust"), Some(Decision::Asked(Ask::Execute)));
    let running = LegIn { started: Some(-30_000), ..job("A", "rust", "K1", 200.0) };
    let c = s.handle(Input::LegRunning { leg: Box::new(running), runner: "a".into(), attempt: 1 }, 0);
    assert!(c.contains(&adopted("B/rust", "A/rust", "A")), "{c:?}");
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 1);
    assert!(assigns(&c).is_empty(), "nothing runs twice: {c:?}");
    let c = s.handle(Input::AttemptFinished { leg: "A/rust".into(), attempt: 1, outcome: Outcome::Success }, 170_000);
    assert_eq!(endings(&c), [Command::Settled { leg: "A/rust".into(), outcome: Outcome::Success, reason: None }, Command::Adopted { leg: "B/rust".into(), from: "A/rust".into(), outcome: Outcome::Success }]);
}

/// W28-R44 through the scheduler: an execution that fails in class `failure` fails its adopter too.
/// One whose runner is lost twice (its retry too) leaves its adopter to run the work itself.
#[test]
fn w28_r45_failures_and_lost_runners() {
    let failed = |class| Outcome::Failed(Failed { class, exit_code: Some(1), log_hits: Vec::new() });
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0);
    s.handle(Input::LegQueued(Box::new(job("B", "rust", "K1", 200.0))), 1);
    let c = s.handle(Input::AttemptFinished { leg: "A/rust".into(), attempt: 1, outcome: failed(FailureClass::Failure) }, 100_000);
    assert!(c.contains(&Command::Adopted { leg: "B/rust".into(), from: "A/rust".into(), outcome: failed(FailureClass::Failure) }), "{c:?}");

    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    assert_eq!(assigns(&s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0)), [("A/rust".to_owned(), "a".to_owned())]);
    assert!(s.handle(Input::LegQueued(Box::new(job("B", "rust", "K1", 200.0))), 1).contains(&adopted("B/rust", "A/rust", "A")));
    // A's runner is lost: its retry runs elsewhere, and B still waits for it.
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 10_000);
    assert!(c.iter().any(|x| matches!(x, Command::Retry { leg, attempt: 2, .. } if leg == "A/rust")), "{c:?}");
    assert_eq!(assigns(&c), [("A/rust".to_owned(), "b".to_owned())]);
    assert!(endings(&c).is_empty());
    // Lost again, A has failed for good, in a way B may not take over: B executes the work.
    let c = s.handle(Input::RunnerLost { runner: "b".into() }, 20_000);
    assert!(endings(&c).iter().all(|x| matches!(x, Command::Settled { leg, .. } if leg == "A/rust")) && !endings(&c).is_empty(), "{c:?}");
    assert_eq!(decision(&c, "B/rust"), Some(Decision::Next(Next::Execute)));
    let c = s.handle(Input::RunnerUp(Box::new(linux("c", 1))), 30_000);
    assert_eq!(assigns(&c), [("B/rust".to_owned(), "c".to_owned())]);
}

/// W28-R45: B's push withdraws A. A's queued rust leg, which no live demand wants, settles
/// cancelled, saying why; its test leg, which D wants, is kept at the highest priority left (D's 50,
/// not A's 90), so X's leg at 70 goes first.
#[test]
fn w28_r45_withdrawn_queued_work_is_dropped_or_kept_at_the_highest_priority_left() {
    let mut s = Scheduler::new(Config::default());
    push(&mut s, "A", 0);
    s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { priority: 90, ..job("A", "test", "T1", 100.0) })), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { priority: 70, ..job("X", "lint", "L1", 100.0) })), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { priority: 50, ..job("D", "test", "T1", 100.0) })), 0);
    assert!(c.contains(&adopted("D/test", "A/test", "A")), "{c:?}");
    let c = push(&mut s, "B", 1_000);
    let dropped = Fate::Drop { by: vec!["B".into()] };
    assert!(c.contains(&Command::Settled { leg: "A/rust".into(), outcome: Outcome::Cancelled, reason: Some(dropped.why()) }), "{c:?}");
    assert_eq!(decision(&c, "A/test"), Some(Decision::Fate(Fate::Kept { priority: 50, by: vec!["D".into()] })));
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 2_000);
    assert_eq!(assigns(&c), [("X/lint".to_owned(), "a".to_owned())]);
}

/// W28-R45: a withdrawn run's leg queued later only takes what's in flight: with nothing there, it
/// settles cancelled, saying why. An adopter cancelled with its run stops waiting, and the
/// execution it adopted is decided again: its own run still wants it, so it's kept.
#[test]
fn w28_r45_withdrawn_runs_take_only_what_is_in_flight() {
    let mut s = Scheduler::new(Config::default());
    push(&mut s, "A", 0);
    push(&mut s, "B", 1_000);
    let c = s.handle(Input::LegQueued(Box::new(job("A", "rust", "K1", 200.0))), 2_000);
    assert!(c.contains(&Command::Settled { leg: "A/rust".into(), outcome: Outcome::Cancelled, reason: Some(Ask::Drop.why()) }), "{c:?}");
    // C's leg (no demand: its workflow doesn't adopt) executes K2, queued; B's leg adopts it. B
    // cancelled, C's work is still C's own, so it stays.
    s.handle(Input::LegQueued(Box::new(job("C", "rust", "K2", 200.0))), 3_000);
    assert!(s.handle(Input::LegQueued(Box::new(job("B", "rust", "K2", 200.0))), 3_000).contains(&adopted("B/rust", "C/rust", "C")));
    let c = s.handle(Input::LegCancelled { leg: "B/rust".into(), reason: "run cancelled".into() }, 4_000);
    assert_eq!(endings(&c), [Command::Settled { leg: "B/rust".into(), outcome: Outcome::Cancelled, reason: Some("run cancelled".into()) }]);
    assert_eq!(decision(&c, "C/rust"), Some(Decision::Fate(Fate::Kept { priority: 60, by: vec!["C".into()] })));
}
