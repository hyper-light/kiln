//! Scheduler tests (docs/ci.md W05-AC01 to AC06, AC10, W05-S01 to S04), on the simulator.

use super::groups::GroupConfig;
use super::pools::PoolSpec;
use super::reserve::ReservationSpec;
use super::retry::{FailureClass, RetryPolicy};
use super::routing::{LegRoute, Preference, RunnerView, github_label_rules};
use super::sim::Sim;
use super::state::{Command, Config, Incoming, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason, room};
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

/// A machine of `cpus` CPUs and `gib` GiB, its slots `slots`: a bench's runner.
fn machine(name: &str, slots: u32, cpus: u32, gib: u64) -> RunnerIn {
    let mut r = linux(name, slots);
    r.view.facts.cpu.cores = cpus / 2;
    r.view.facts.cpu.threads = cpus;
    r.view.facts.mem.total = gib << 30;
    r
}

/// An exclusive leg asking for `cpus` CPUs and `gib` GiB: a bench.
fn bench(id: &str, cpus: f64, gib: u64) -> LegIn {
    LegIn { exclusive: true, request: Resources { cpu: cpus, mem: gib << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) }
}

fn assigned(c: &[Command]) -> Vec<String> {
    c.iter().filter_map(|x| if let Command::Assign { leg, .. } = x { Some(leg.clone()) } else { None }).collect()
}

/// An exclusive leg that asks for CPUs holds them and its memory, and ordinary legs pack in
/// beside it, in what's left: on a 32-CPU, 64 GiB runner of 4 slots, a bench holding 16 CPUs
/// leaves two slots' worth.
#[test]
fn an_exclusive_leg_holds_its_share_and_others_run_beside_it() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 0);
    let c = s.handle(Input::LegQueued(Box::new(bench("bench", 16.0, 16))), 1);
    assert_eq!(assigned(&c), vec!["bench"], "it doesn't wait for an empty runner");
    let mut on = Vec::new();
    for (i, id) in ["o1", "o2", "o3"].iter().enumerate() {
        on.extend(assigned(&s.handle(Input::LegQueued(Box::new(leg(id, "acme/x", 60))), 2 + i as i64)));
    }
    assert_eq!(on, vec!["o1", "o2"], "two ordinary legs beside it, in the half it doesn't hold");
    assert!(matches!(s.reason("o3"), Some(WaitReason::Capacity { eligible: 1, busy: 1, draining: 0 })), "{:?}", s.reason("o3"));
    // Another bench waits for its own share: of the 16 CPUs not held, the two legs there take 16.
    s.handle(Input::LegQueued(Box::new(bench("bench2", 16.0, 16))), 6);
    assert!(matches!(s.reason("bench2"), Some(WaitReason::Capacity { .. })), "{:?}", s.reason("bench2"));
    // The bench ends: all four slots are there again, and the waiting legs take them.
    let c = s.handle(Input::AttemptFinished { leg: "bench".into(), attempt: 1, outcome: Outcome::Success }, 7);
    let next = assigned(&c);
    assert!(next.contains(&"o3".to_owned()) || next.contains(&"bench2".to_owned()), "what waited goes on: {next:?}");
}

/// Asking for nothing, an exclusive leg still has its runner to itself, as before.
#[test]
fn an_exclusive_leg_asking_nothing_still_takes_the_whole_runner() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 0);
    s.handle(Input::LegQueued(Box::new(leg("busy", "acme/x", 60))), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { exclusive: true, ..leg("ex", "acme/x", 60) })), 1);
    assert_eq!(s.reason("ex"), Some(&WaitReason::Exclusive));
    let c = s.handle(Input::AttemptFinished { leg: "busy".into(), attempt: 1, outcome: Outcome::Success }, 2);
    assert_eq!(assigned(&c), vec!["ex"]);
    let c = s.handle(Input::LegQueued(Box::new(leg("after", "acme/x", 60))), 3);
    assert!(assigned(&c).is_empty(), "nothing shares the runner with it");
}

/// After a restart, a bench the runner still runs holds its share as before: nothing new lands
/// in it.
#[test]
fn a_bench_adopted_after_a_restart_keeps_its_share() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::LegRunning { leg: Box::new(bench("bench", 16.0, 16)), runner: "a".into(), attempt: 1 }, 0);
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 1);
    let mut on = Vec::new();
    for (i, id) in ["o1", "o2", "o3"].iter().enumerate() {
        on.extend(assigned(&s.handle(Input::LegQueued(Box::new(leg(id, "acme/x", 60))), 2 + i as i64)));
    }
    assert_eq!(on, vec!["o1", "o2"], "the adopted bench still holds half the runner");
    // And one asking for nothing, adopted, still holds all of it.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::LegRunning { leg: Box::new(LegIn { exclusive: true, ..leg("ex", "acme/x", 60) }), runner: "a".into(), attempt: 1 }, 0);
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 1);
    let c = s.handle(Input::LegQueued(Box::new(leg("o", "acme/x", 60))), 2);
    assert!(assigned(&c).is_empty(), "nothing beside an adopted exclusive leg");
}

/// The rule both dispatchers and the runner use, in numbers: 32 CPUs, 64 GiB, 4 slots.
#[test]
fn the_room_rule_both_dispatchers_and_the_runner_use() {
    let size = Some(Resources { cpu: 32.0, mem: 64 << 30, ..Resources::default() });
    let none = Resources::default();
    let half = Resources { cpu: 16.0, mem: 16 << 30, ..Resources::default() };
    // Nothing held: four ordinary legs, and a share of 16 beside at most two of them.
    assert!(room(4, size, &none, false, 3, 3, Incoming::Ordinary));
    assert!(!room(4, size, &none, false, 4, 4, Incoming::Ordinary));
    assert!(room(4, size, &none, false, 2, 2, Incoming::Share(half)));
    assert!(!room(4, size, &none, false, 3, 3, Incoming::Share(half)));
    // Half held: two ordinary slots are left; no second share of 16 beside two legs.
    assert!(room(4, size, &half, false, 1, 2, Incoming::Ordinary));
    assert!(!room(4, size, &half, false, 2, 3, Incoming::Ordinary));
    assert!(!room(4, size, &half, false, 2, 3, Incoming::Share(half)));
    assert!(room(4, size, &half, false, 0, 1, Incoming::Share(half)));
    // The whole runner: only an empty one, and nothing beside it.
    assert!(room(4, size, &none, false, 0, 0, Incoming::Whole) && !room(4, size, &none, false, 1, 1, Incoming::Whole));
    assert!(!room(4, size, &none, true, 0, 1, Incoming::Ordinary));
    // A runner whose size isn't known: a share waits for it to be empty, as the whole would.
    assert!(room(4, None, &none, false, 0, 0, Incoming::Share(half)) && !room(4, None, &none, false, 1, 1, Incoming::Share(half)));
    // Memory holds as CPUs do: a share asking more memory than is left waits.
    let big = Resources { cpu: 4.0, mem: 60 << 30, ..Resources::default() };
    assert!(!room(4, size, &none, false, 1, 1, Incoming::Share(big)));
    assert_eq!(Incoming::of(true, &half), Incoming::Share(half));
    assert_eq!(Incoming::of(true, &none), Incoming::Whole);
    assert_eq!(Incoming::of(false, &half), Incoming::Ordinary);
}

/// A share bigger than any runner it could run on waits, saying so.
#[test]
fn a_share_no_runner_can_hold_waits_saying_why() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 0);
    s.handle(Input::LegQueued(Box::new(bench("huge", 64.0, 16))), 1);
    assert!(matches!(s.reason("huge"), Some(WaitReason::Unroutable { failing }) if failing.iter().any(|f| f.contains("64 CPUs"))), "{:?}", s.reason("huge"));
}

/// In resource mode the share comes out of the runner before overcommit: ordinary legs
/// overcommit only what no bench holds.
#[test]
fn in_resource_mode_ordinary_legs_overcommit_only_what_isnt_held() {
    let mut s = Scheduler::new(Config { resources: true, ..Config::default() });
    let big = RunnerIn { capacity: Some(Resources { cpu: 32.0, mem: 64 << 30, disk: 500 << 30, gpu: 0 }), ..machine("a", 99, 32, 64) };
    s.handle(Input::RunnerUp(Box::new(big)), 0);
    assert_eq!(assigned(&s.handle(Input::LegQueued(Box::new(bench("bench", 16.0, 16))), 1)), vec!["bench"]);
    let cpu = |id: &str| LegIn { request: Resources { cpu: 4.0, mem: 2 << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) };
    let mut on = 0;
    for i in 0..8 {
        on += assigned(&s.handle(Input::LegQueued(Box::new(cpu(&format!("c{i}")))), 2 + i)).len();
    }
    assert_eq!(on, 6, "16 unheld CPUs × 1.5 overcommit = 24, six legs of 4");
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

// ------------------------------------------------------------------ warm placement (W24-R18)

use super::routing::Affinity;
use super::sim::Assigned;
use super::warm::{self, Held, Wants, Workspace};
use proptest::prelude::*;
use std::collections::BTreeMap;

const LANE: &str = "acme-x-rust";

fn warm_leg(id: &str, cold: f64, patience: Option<f64>) -> LegIn {
    LegIn { lane: Some(LANE.into()), cold: Some(cold), prefer: vec![Preference { warm: true, patience, ..Preference::default() }], ..leg(id, "acme/x", 60) }
}

/// A runner holding the lane's workspace: its warm runs take `warm_s`, it's `files` behind, and
/// lacks `missing` bytes of it.
fn holding(warm_s: f64, files: u32, missing: u64) -> Held {
    Held { workspaces: vec![Workspace { lane: LANE.into(), toolchain: String::new(), repo: "acme/x".into(), workflow: "ci".into(), job: "rust".into(), behind: files / 4, files, warm_s: Some(warm_s), missing }], bandwidth: None }
}

fn warm_input(runner: &str, held: Held) -> Input {
    Input::Warm { runner: runner.into(), held }
}

fn assigned_to(c: &[Command], l: &str) -> Option<String> {
    c.iter().find_map(|x| match x {
        Command::Assign { leg, runner, .. } if leg == l => Some(runner.clone()),
        _ => None,
    })
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// W24-R18: a leg with `warm` goes where it's expected to finish first, from what each runner
/// holds warm for its lane: the closest warm workspace, then one far behind and missing most of
/// its snapshot, then cold. Without the term, warm state changes nothing.
#[test]
fn w24_r18_the_warm_term_places_where_it_finishes_first() {
    let fleet = |s: &mut Scheduler| {
        for r in ["a", "b", "c"] {
            s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
        }
        s.handle(warm_input("b", holding(20.0, 0, 0)), 0);
        // Warmer runs, but 300 files behind and missing 2 GiB: 92.5 s running, 20.48 s fetching.
        s.handle(warm_input("c", holding(10.0, 300, 2 << 30)), 0);
    };
    let mut s = Scheduler::new(Config::default());
    fleet(&mut s);
    let placed: Vec<Option<String>> = ["w1", "w2", "w3"].iter().map(|id| assigned_to(&s.handle(Input::LegQueued(Box::new(warm_leg(id, 120.0, Some(0.0)))), 1), id)).collect();
    assert_eq!(placed, vec![Some("b".to_owned()), Some("c".to_owned()), Some("a".to_owned())]);
    let mut s = Scheduler::new(Config::default());
    fleet(&mut s);
    let placed: Vec<Option<String>> = ["x1", "x2", "x3"].iter().map(|id| assigned_to(&s.handle(Input::LegQueued(Box::new(LegIn { cost: Some(120.0), ..leg(id, "acme/x", 60) })), 1), id)).collect();
    assert_eq!(placed, vec![Some("a".to_owned()), Some("b".to_owned()), Some("c".to_owned())], "by name, as before");
}

/// W24-R18 explained (`ci.legs.routing`, W05-R20): each runner's workspace, warmth, expected
/// wait, run, fetch and finish, and the score they make; nothing of it for a leg without the term.
#[test]
fn w24_r18_explained() -> Result<(), String> {
    let mut s = Scheduler::new(Config::default());
    for r in ["a", "b", "c"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
    }
    s.handle(warm_input("b", holding(20.0, 0, 0)), 0);
    s.handle(warm_input("c", holding(10.0, 300, 2 << 30)), 0);
    // Every runner busy for 30 s.
    for f in ["f1", "f2", "f3"] {
        s.handle(Input::LegQueued(Box::new(LegIn { cost: Some(30.0), ..leg(f, "acme/x", 60) })), 0);
    }
    s.handle(Input::LegQueued(Box::new(warm_leg("q", 120.0, Some(0.0)))), 0);
    assert!(matches!(s.reason("q"), Some(WaitReason::Capacity { eligible: 3, busy: 3, .. })), "{:?}", s.reason("q"));
    let why = s.explain("q");
    let of = |r: &str| why.iter().find(|e| e.runner == r).cloned().ok_or(format!("no {r}"));
    let (a, b, c) = (of("a")?, of("b")?, of("c")?);
    let (wa, wb, wc) = (a.warm.clone().ok_or("a")?, b.warm.clone().ok_or("b")?, c.warm.clone().ok_or("c")?);
    assert!(wa.estimate.lane.is_none() && near(wa.finish_s, 150.0), "cold: {wa:?}");
    assert!(wb.estimate.lane.as_deref() == Some(LANE) && near(wb.estimate.run_s, 20.0) && near(wb.wait_s, 30.0) && near(wb.finish_s, 50.0), "{wb:?}");
    assert!(near(wc.estimate.run_s, 92.5) && near(wc.estimate.fetch_s, 20.48) && (wc.estimate.files, wc.estimate.behind) == (300, 75), "{wc:?}");
    assert_eq!((a.score, b.score), (Some(0.0), Some(1.0)), "the soonest finish scores 1, the latest 0");
    assert!(c.score.is_some_and(|x| x > 0.0 && x < 0.1), "{c:?}");
    let json = serde_json::to_value(&b).map_err(|e| e.to_string())?;
    assert!(["lane", "behind", "files", "warmth", "run_s", "fetch_s", "wait_s", "finish_s"].iter().all(|k| json.pointer(&format!("/warm/{k}")).is_some()), "{json}");
    // Without the term, nothing of it.
    s.handle(Input::LegQueued(Box::new(leg("plain", "acme/x", 60))), 1);
    let plain = serde_json::to_value(s.explain("plain")).map_err(|e| e.to_string())?;
    assert!(plain.as_array().is_some_and(|a| a.len() == 3 && a.iter().all(|e| e.get("warm").is_none())), "{plain}");
    // As b frees, the leg takes it.
    let c = s.handle(Input::AttemptFinished { leg: "f2".into(), attempt: 1, outcome: Outcome::Success }, 30_000);
    assert_eq!(assigned_to(&c, "q").as_deref(), Some("b"), "{c:?}");
    Ok(())
}

/// W05-R23's `affinity: { job }`, with real workspaces behind it (W24-R18): the runner holding
/// that job's workspace in the leg's repository and workflow, never another repository's or
/// another workflow's job of that name.
#[test]
fn w24_r18_affinity_goes_after_that_jobs_workspaces() {
    let build = |repo: &str| Held { workspaces: vec![Workspace { lane: format!("{repo}-build"), repo: repo.into(), workflow: "ci".into(), job: "build".into(), warm_s: Some(10.0), ..Workspace::default() }], bandwidth: None };
    let mut s = Scheduler::new(Config::default());
    for r in ["a", "b", "c"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 4))), 0);
    }
    s.handle(warm_input("b", build("acme/x")), 0);
    s.handle(warm_input("c", build("other/y")), 0);
    let test = |id: &str, repo: &str, workflow: &str| {
        let mut l = LegIn { cost: Some(100.0), prefer: vec![Preference { affinity: Some(Affinity { job: "build".into() }), ..Preference::default() }], ..leg(id, repo, 60) };
        l.route.job = "test".into();
        l.workflow_file = workflow.into();
        l
    };
    assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(test("t1", "acme/x", "ci"))), 1), "t1").as_deref(), Some("b"));
    assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(test("t2", "other/y", "ci"))), 2), "t2").as_deref(), Some("c"));
    // Another workflow's `build` is another job: no runner is warm for it, so the least loaded.
    assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(test("t3", "acme/x", "release"))), 3), "t3").as_deref(), Some("a"));
}

/// W24-R11 through R18: a runner's workspace of the leg's lane counts only where the leg would run
/// with the toolchain it was made with. Both runners hold one made in the `docker` executor's
/// container; the rules run the leg there where a runner has Docker, else on the host. It goes to
/// the one with Docker, though the other comes first by name, and the explanation says why.
#[test]
fn w24_r18_a_lanes_workspace_counts_where_the_leg_runs_with_its_toolchain() -> Result<(), String> {
    use super::routing::{Rule, RuleMatch};
    let rule = Rule { matches: RuleMatch { labels: vec!["k8s".into()], ..RuleMatch::default() }, executor: vec!["docker".into(), "host".into()], image: Some("kiln/runner-ubuntu-24.04".into()), ..Rule::default() };
    let mut s = Scheduler::new(Config { rules: vec![rule.clone()], ..Config::default() });
    let mut docker = linux("w", 1);
    for e in ["docker", "host"] {
        docker.view.facts.executors.insert(e.into(), crate::facts::Executor::default());
    }
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(docker)), 0);
    let made = |toolchain: &str| Held { workspaces: vec![Workspace { lane: LANE.into(), toolchain: toolchain.into(), warm_s: Some(20.0), ..Workspace::default() }], bandwidth: None };
    for r in ["a", "w"] {
        s.handle(warm_input(r, made("docker|kiln/runner-ubuntu-24.04|")), 0);
    }
    let c = s.handle(Input::LegQueued(Box::new(warm_leg("x", 120.0, Some(0.0)))), 1);
    assert_eq!(assigned_to(&c, "x").as_deref(), Some("w"), "{c:?}");
    let why = s.explain("x");
    let of = |r: &str| why.iter().find(|e| e.runner == r).and_then(|e| e.warm.clone()).ok_or(format!("no {r}"));
    let (a, w) = (of("a")?, of("w")?);
    assert!(a.estimate.lane.is_none() && near(a.estimate.run_s, 120.0), "on the host, cold: {a:?}");
    assert!(w.estimate.lane.as_deref() == Some(LANE) && near(w.estimate.run_s, 20.0), "{w:?}");
    // A workspace made on the host is warm on `a` and not on `w`, where the leg would run in a
    // container.
    let mut s = Scheduler::new(Config { rules: vec![rule], ..Config::default() });
    let mut docker = linux("w", 1);
    docker.view.facts.executors.insert("docker".into(), crate::facts::Executor::default());
    s.handle(Input::RunnerUp(Box::new(docker)), 0);
    s.handle(Input::RunnerUp(Box::new(linux("z", 1))), 0);
    for r in ["w", "z"] {
        s.handle(warm_input(r, made("host||")), 0);
    }
    let c = s.handle(Input::LegQueued(Box::new(warm_leg("y", 120.0, Some(0.0)))), 1);
    assert_eq!(assigned_to(&c, "y").as_deref(), Some("z"), "{c:?}");
    Ok(())
}

/// W24-R18's patience, on the simulator. Warm on `w` (20 s there, 120 s cold): a second leg waits
/// for `w` while it's busy, as `w` is expected to free up within half of the 100 s it saves, says
/// so, and starts there as it frees: both are done by 40 s, where without the term the first runs
/// cold until 120 s. A runner expected to stay busy longer isn't waited for. A wait that outlasts
/// its bound ends at its bound, the leg going where it can then.
#[test]
fn w24_r18_a_leg_waits_for_a_busy_warm_runner_within_its_patience() -> Result<(), String> {
    let fleet = |sim: &mut Sim| {
        sim.runner(0, linux("a", 1));
        sim.runner(0, linux("w", 1));
        sim.input(0, warm_input("w", holding(20.0, 0, 0)));
    };
    let legs = |sim: &mut Sim, first_on_w: i64, terms: bool| {
        for (id, t) in [("first", 1), ("second", 1_000)] {
            let l = if terms { warm_leg(id, 120.0, None) } else { LegIn { cost: Some(120.0), ..leg(id, "acme/x", 60) } };
            sim.leg(t, l, 120_000);
            sim.durations_on.insert((id.into(), "w".into()), if id == "first" { first_on_w } else { 20_000 });
        }
    };
    let on = |sim: &Sim, l: &str| sim.assigned.iter().find(|a| a.leg == l).map(|a| (a.at, a.runner.clone()));
    let waits = |sim: &Sim, l: &str| sim.waits.iter().filter(|(_, x, _)| x == l).map(|(t, _, r)| (*t, r.clone())).collect::<Vec<_>>();
    let mut sim = Sim::new(Config::default());
    fleet(&mut sim);
    legs(&mut sim, 20_000, true);
    sim.run(10 * MIN);
    assert_eq!(on(&sim, "first"), Some((1, "w".to_owned())));
    let warm = WaitReason::Warm { runner: "w".into(), saves_s: 100, until: 51_000 };
    assert_eq!(waits(&sim, "second"), vec![(1_000, warm.clone())]);
    assert_eq!(on(&sim, "second"), Some((20_001, "w".to_owned())), "as w frees");
    assert_eq!(sim.settled.iter().map(|(t, _, _)| *t).max(), Some(40_001), "{:?}", sim.settled);
    let json = serde_json::to_value(&warm).map_err(|e| e.to_string())?;
    assert_eq!(json, serde_json::json!({ "reason": "warm", "runner": "w", "saves_s": 100, "until": 51_000 }));
    // Without the term: the first by name, on a, cold until 120 s.
    let mut sim = Sim::new(Config::default());
    fleet(&mut sim);
    legs(&mut sim, 20_000, false);
    sim.run(10 * MIN);
    assert_eq!(on(&sim, "first"), Some((1, "a".to_owned())));
    assert_eq!(sim.settled.iter().map(|(t, _, _)| *t).max(), Some(120_001), "{:?}", sim.settled);
    // `w` busy for another 300 s, as expected: not waited for.
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("w", 1));
    sim.input(0, warm_input("w", holding(20.0, 0, 0)));
    sim.leg(1, LegIn { cost: Some(300.0), ..leg("slow", "acme/x", 60) }, 300_000);
    sim.runner(2, linux("a", 1));
    sim.leg(1_000, warm_leg("second", 120.0, None), 120_000);
    sim.run(10 * MIN);
    assert_eq!(on(&sim, "slow").map(|(_, r)| r).as_deref(), Some("w"));
    assert_eq!(on(&sim, "second"), Some((1_000, "a".to_owned())));
    assert!(waits(&sim, "second").is_empty());
    // `first` takes 100 s on w, not the 20 s expected: `second` waits until its bound, 51 s,
    // then runs on a.
    let mut sim = Sim::new(Config::default());
    fleet(&mut sim);
    legs(&mut sim, 100_000, true);
    sim.run(10 * MIN);
    assert_eq!(waits(&sim, "second"), vec![(1_000, warm)]);
    assert_eq!(on(&sim, "second"), Some((51_000, "a".to_owned())), "at its bound");
    Ok(())
}

/// W24-R18: a wait for a warm runner ends as that runner goes (lost or draining): the leg goes
/// where it can at once, and doesn't wait for another.
#[test]
fn w24_r18_a_wait_ends_when_its_runner_goes() {
    for gone in [Input::RunnerLost { runner: "w".into() }, Input::RunnerDraining { runner: "w".into() }] {
        let mut s = Scheduler::new(Config::default());
        s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
        s.handle(Input::RunnerUp(Box::new(linux("w", 1))), 0);
        s.handle(warm_input("w", holding(20.0, 0, 0)), 0);
        assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(warm_leg("first", 120.0, None))), 1), "first").as_deref(), Some("w"));
        let c = s.handle(Input::LegQueued(Box::new(warm_leg("second", 120.0, None))), 1_000);
        assert!(assigned_to(&c, "second").is_none() && matches!(s.reason("second"), Some(WaitReason::Warm { .. })), "{c:?}");
        let c = s.handle(gone, 5_000);
        assert_eq!(assigned_to(&c, "second").as_deref(), Some("a"), "{c:?}");
        // Its timer later finds nothing to do.
        let c = s.handle(Input::Tick, 51_000);
        assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. } | Command::Wait { .. })), "{c:?}");
    }
}

/// W24-R18: a leg waiting for one busy warm runner takes another as warm as soon as that one
/// frees, rather than wait on; nothing colder.
#[test]
fn w24_r18_a_waiting_leg_takes_a_runner_as_warm() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    for w in ["w1", "w2"] {
        sim.runner(0, linux(w, 1));
        sim.input(0, warm_input(w, holding(20.0, 0, 0)));
    }
    // Both warm runners busy, each expected for 20 s: w1 takes 100 s, w2 10 s.
    sim.leg(0, warm_leg("f1", 120.0, None), 120_000);
    sim.leg(1, warm_leg("f2", 120.0, None), 120_000);
    sim.durations_on.insert(("f1".into(), "w1".into()), 100_000);
    sim.durations_on.insert(("f2".into(), "w2".into()), 10_000);
    sim.leg(2, warm_leg("x", 120.0, None), 120_000);
    sim.durations_on.insert(("x".into(), "w2".into()), 20_000);
    sim.run(10 * MIN);
    let on = |l: &str| sim.assigned.iter().find(|a| a.leg == l).map(|a| (a.at, a.runner.clone()));
    assert_eq!((on("f1"), on("f2")), (Some((0, "w1".to_owned())), Some((1, "w2".to_owned()))));
    assert!(sim.waits.iter().any(|(t, l, r)| *t == 2 && l == "x" && matches!(r, WaitReason::Warm { runner, .. } if runner == "w1")), "{:?}", sim.waits);
    assert_eq!(on("x"), Some((10_001, "w2".to_owned())), "w2 is as warm, and free first");
}

#[derive(Debug, Clone)]
struct Box1 {
    /// Its workspace of the lane: warm runs' seconds, files behind, bytes missing.
    ws: Option<(f64, u32, u64)>,
    bandwidth: Option<f64>,
    /// Busy for this many seconds from 0, as expected.
    busy: Option<u32>,
}

/// A runner for the rule's property: as often warm as not, near or far, busy or free.
fn box1() -> impl Strategy<Value = Box1> {
    let ws = prop_oneof![1 => Just(None), 2 => (1.0f64..150.0, prop_oneof![Just(0u32), 0u32..400], prop_oneof![Just(0u64), 0u64..(4u64 << 30)]).prop_map(Some)];
    (ws, prop::option::of(10.0f64 * 1024.0 * 1024.0..1e9), prop::option::of(prop_oneof![1u32..60, 1u32..600])).prop_map(|(ws, bandwidth, busy)| Box1 { ws, bandwidth, busy })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// W24-R18: placement and waits follow the expected-finish-time rule. Of the runners with
    /// room, the leg goes to the one where queue wait + cold × (1 − warmth) + missing ÷ bandwidth
    /// is least (ties by name); unless a busy runner that would take it finishes it sooner, wait
    /// included, saves it at least a second, and is expected to have room within its share of what
    /// it saves: then it waits for the soonest such, until that share at most.
    #[test]
    fn w24_r18_placement_and_waits_follow_the_expected_finish_rule(fleet in prop::collection::vec(box1(), 2..=5), cold in 30.0f64..600.0, share in prop_oneof![1 => Just(0.0), 1 => Just(1.0), 4 => 0.0f64..=1.0]) {
        let mut s = Scheduler::new(Config::default());
        let held = |b: &Box1| b.ws.map(|(warm_s, files, missing)| Held { workspaces: vec![Workspace { lane: LANE.into(), warm_s: Some(warm_s), files, missing, ..Workspace::default() }], bandwidth: b.bandwidth });
        for (i, b) in fleet.iter().enumerate() {
            let name = format!("r{i}");
            let mut r = linux(&name, 1);
            r.view.labels.push(name.clone());
            s.handle(Input::RunnerUp(Box::new(r)), 0);
            if let Some(h) = held(b) {
                s.handle(warm_input(&name, h), 0);
            }
            if let Some(secs) = b.busy {
                let f = format!("f{i}");
                let mut l = LegIn { cost: Some(f64::from(secs)), ..leg(&f, "acme/x", 60) };
                l.route.labels.push(name.clone());
                prop_assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(l)), 0), &f), Some(name));
            }
        }
        let t0 = 500;
        let c = s.handle(Input::LegQueued(Box::new(warm_leg("x", cold, Some(share)))), t0);
        // The rule, from each runner's estimate.
        let m = warm::Model::default();
        let busy_s: Vec<f64> = fleet.iter().map(|b| warm::estimate(held(b).as_ref(), &[Wants::Lane(LANE)], cold, &m, None).busy_s()).collect();
        let at = |i: usize| busy_s.get(i).copied().unwrap_or(f64::INFINITY);
        let free: Vec<usize> = (0..fleet.len()).filter(|i| fleet.get(*i).is_some_and(|b| b.busy.is_none())).collect();
        let Some(first) = free.iter().copied().min_by(|i, j| at(*i).total_cmp(&at(*j)).then(i.cmp(j))) else {
            prop_assert!(assigned_to(&c, "x").is_none());
            prop_assert!(matches!(s.reason("x"), Some(WaitReason::Capacity { .. })), "{:?}", s.reason("x"));
            return Ok(());
        };
        let here = at(first);
        let mut wait: Option<(f64, usize, u64, i64)> = None;
        for (i, b) in fleet.iter().enumerate() {
            let Some(secs) = b.busy else { continue };
            let wait_s = (i64::from(secs) * 1000 - t0) as f64 / 1000.0;
            let there = at(i);
            let saves = here - there;
            if saves < 1.0 {
                continue;
            }
            let saves_s = saves.floor() as u64;
            let bound = (share * saves_s as f64 * 1000.0).floor();
            if bound <= 0.0 || (wait_s * 1000.0).ceil() > bound || wait_s + there >= here {
                continue;
            }
            if wait.is_none_or(|(f, ..)| wait_s + there < f) {
                wait = Some((wait_s + there, i, saves_s, t0 + bound as i64));
            }
        }
        match wait {
            Some((_, i, saves_s, until)) => {
                let want = WaitReason::Warm { runner: format!("r{i}"), saves_s, until };
                prop_assert!(assigned_to(&c, "x").is_none(), "{:?}", c);
                prop_assert!(c.iter().any(|x| *x == Command::Wait { leg: "x".into(), reason: want.clone() }), "{:?} in {:?}", want, c);
                let timer = Command::Timer { at: until };
                prop_assert!(c.contains(&timer), "no timer for {}: {:?}", until, c);
            }
            None => prop_assert_eq!(assigned_to(&c, "x"), Some(format!("r{first}"))),
        }
    }
}

/// One simulated leg: queued at, its cold seconds, its patience, how long it takes on each
/// runner.
fn sim_leg() -> impl Strategy<Value = (i64, f64, f64, Vec<i64>)> {
    (0i64..120_000, 10.0f64..300.0, prop_oneof![Just(0.5), 0.0f64..=1.0], prop::collection::vec(1_000i64..300_000, 5))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// W24-R18: patience never exceeds its bound. Legs that take other times than expected, on
    /// warm and cold runners: every wait for a warm runner is for one that saves a second or
    /// more, lasts at most its share of what that saves, ends by then (the leg assigned, or
    /// waiting for something else), and comes once per leg; and every leg runs.
    #[test]
    fn w24_r18_patience_never_exceeds_its_bound(fleet in prop::collection::vec((prop::option::of((1.0f64..200.0, 0u32..300, 0u64..(2u64 << 30))), 1u32..3), 2..5), legs in prop::collection::vec(sim_leg(), 1..25)) {
        let mut sim = Sim::new(Config::default());
        for (i, (ws, slots)) in fleet.iter().enumerate() {
            let name = format!("r{i}");
            sim.runner(0, linux(&name, *slots));
            if let Some((warm_s, files, missing)) = ws {
                sim.input(0, warm_input(&name, Held { workspaces: vec![Workspace { lane: LANE.into(), warm_s: Some(*warm_s), files: *files, missing: *missing, ..Workspace::default() }], bandwidth: None }));
            }
        }
        let mut share: BTreeMap<String, f64> = BTreeMap::new();
        for (j, (at, cold, patience, took)) in legs.iter().enumerate() {
            let id = format!("x{j}");
            sim.leg(*at, warm_leg(&id, *cold, Some(*patience)), 60_000);
            for (i, d) in took.iter().enumerate() {
                sim.durations_on.insert((id.clone(), format!("r{i}")), *d);
            }
            share.insert(id, *patience);
        }
        sim.run(10_000 * MIN);
        prop_assert_eq!(sim.settled.len(), legs.len(), "every leg ran: {:?}", sim.settled);
        for (t0, l, r) in &sim.waits {
            let WaitReason::Warm { saves_s, until, .. } = r else { continue };
            let p = share.get(l).copied().unwrap_or(0.0);
            prop_assert!(*saves_s >= 1);
            prop_assert!(until - t0 <= (p * *saves_s as f64 * 1000.0).floor() as i64, "{} waited {} ms for {} s saved at {}", l, until - t0, saves_s, p);
            let next = sim.assigned.iter().filter(|a| a.leg == *l && a.at >= *t0).map(|a| a.at).chain(sim.waits.iter().filter(|(t, x, w)| x == l && t >= t0 && !matches!(w, WaitReason::Warm { .. })).map(|(t, _, _)| *t)).min();
            prop_assert!(next.is_some_and(|t| t <= *until), "{} still waits past {}: {:?}", l, until, next);
        }
        for l in share.keys() {
            prop_assert!(sim.waits.iter().filter(|(_, x, w)| x == l && matches!(w, WaitReason::Warm { .. })).count() <= 1, "{} waited twice", l);
        }
    }

    /// W24-R18: cold-only fleets behave exactly as before. The same legs (priorities,
    /// repositories, costs, durations, a runner lost and back) assign, settle and wait alike with
    /// `warm` and `affinity` terms as without, whether no runner holds anything warm or only
    /// workspaces of other lanes and repositories.
    #[test]
    fn w24_r18_cold_only_fleets_behave_as_before(fleet in prop::collection::vec(1u32..3, 1..5), legs in prop::collection::vec((0i64..60_000, 0u8..=100, 0usize..3, 1i64..200_000, prop::option::of(1.0f64..300.0)), 1..30), lost in prop::option::of((0usize..5, 0i64..60_000, 1i64..60_000)), share in 0.0f64..=1.0) {
        let run = |terms: bool, others: bool| -> (Vec<Assigned>, Vec<(i64, String, Outcome)>, Vec<String>) {
            let mut sim = Sim::new(Config::default());
            for (i, slots) in fleet.iter().enumerate() {
                let name = format!("r{i}");
                sim.runner(0, linux(&name, *slots));
                if others {
                    let ws = Workspace { lane: "another-lane".into(), repo: "other/repo".into(), workflow: "ci".into(), job: "job0".into(), warm_s: Some(1.0), ..Workspace::default() };
                    sim.input(0, warm_input(&name, Held { workspaces: vec![ws], bandwidth: Some(1.0) }));
                    sim.input(1, warm_input(&format!("gone{i}"), Held::default()));
                }
            }
            if let Some((r, at, back)) = lost
                && let Some(slots) = fleet.get(r)
            {
                sim.input(at, Input::RunnerLost { runner: format!("r{r}") });
                sim.runner(at + back, linux(&format!("r{r}"), *slots));
            }
            for (j, (at, priority, repo, took, cost)) in legs.iter().enumerate() {
                let mut l = LegIn { cost: *cost, ..leg(&format!("l{j}"), &format!("acme/r{repo}"), *priority) };
                l.route.job = format!("job{}", j % 3);
                l.route.workflow = "ci".into();
                if terms {
                    l.lane = Some(format!("lane{}", j % 3));
                    l.prefer = vec![if j % 2 == 0 { Preference { warm: true, patience: Some(share), ..Preference::default() } } else { Preference { affinity: Some(Affinity { job: "job0".into() }), weight: Some(3.0), ..Preference::default() } }];
                }
                sim.leg(*at, l, *took);
            }
            sim.run(10_000 * MIN);
            let mut waits: Vec<String> = sim.waits.iter().map(|(t, l, w)| format!("{t:>12} {l} {w:?}")).collect();
            waits.sort();
            (sim.assigned, sim.settled, waits)
        };
        let before = run(false, false);
        prop_assert_eq!(&run(true, false), &before);
        prop_assert_eq!(&run(true, true), &before);
    }
}
