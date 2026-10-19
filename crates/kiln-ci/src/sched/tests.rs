//! Scheduler tests (docs/ci.md W05-AC01 to AC06, AC10, W05-S01 to S04), on the simulator.

use super::groups::GroupConfig;
use super::pools::{ExecState, PoolSpec};
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
    assert!(matches!(sim.last_wait("w1"), Some(WaitReason::Pool { .. })), "{:?}", sim.last_wait("w1"));
    let view = sim.sched.pools();
    assert!(view.first().and_then(|p| p.last_error.clone()).is_some_and(|(h, _)| h == "bad"));
    sim.runner(20_000, docker_host("good"));
    sim.run(40_000);
    assert_eq!(sim.assigned.iter().find(|a| a.leg == "w1").map(|a| a.runner.as_str()), Some("good"));
    let _ = ExecState::Idle;
    Ok(())
}
