//! Scheduler tests (docs/ci.md W05-AC01 to AC06, AC10, W05-S01 to S04), on the simulator.

use super::groups::GroupConfig;
use super::pools::PoolSpec;
use super::reserve::ReservationSpec;
use super::retry::{FailureClass, RetryPolicy};
use super::routing::{LegRoute, Preference, RunnerView, github_label_rules};
use super::sim::Sim;
use super::state::{Command, Config, Cpus, Fence, Incoming, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason, room};
use crate::facts::{Disk, RunnerFacts};

const MIN: i64 = 60_000;

fn linux(name: &str, slots: u32) -> RunnerIn {
    RunnerIn {
        id: name.into(),
        view: RunnerView { name: name.into(), labels: vec!["k8s".into()], facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 1 << 10, pool: None },
        slots,
        capacity: None,
        executor: None,
        draining: false,
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

/// W05-AC03, within a tenant (docs/needs.md E2.3): a low-priority leg under its own repository's
/// constant stream of high-priority legs starts within the bound aging gives: (100 − 20) / 1 per
/// minute = 80 minutes.
#[test]
fn w05_ac03_aging_bounds_starvation() {
    let start = low_under_a_stream("acme/x");
    assert!((79 * MIN..=82 * MIN).contains(&start), "the low-priority leg started at {} min", start / MIN);
}

/// W05-AC03's leg in another repository than the stream (docs/needs.md E2.3): aging orders only a
/// tenant's own legs, fair share orders tenants, so it starts at its tenant's first turn.
#[test]
fn w05_ac03_another_tenants_low_leg_waits_for_its_turn_alone() {
    let start = low_under_a_stream("acme/y");
    assert!(start - 1 <= MIN, "the low-priority leg waited {} s", (start - 1) / 1000);
}

/// When a priority-20 leg of `repo` starts, queued under a constant stream of acme/x's priority-100
/// legs on one slot.
fn low_under_a_stream(repo: &str) -> i64 {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, leg("busy", "acme/x", 100), MIN / 2);
    sim.leg(1, leg("low", repo, 20), MIN / 2);
    for i in 0..400 {
        sim.leg(i * MIN / 2 + 2, leg(&format!("high{i}"), "acme/x", 100), MIN / 2);
    }
    sim.run(200 * MIN);
    sim.started("low").unwrap_or(i64::MAX)
}

/// W05-AC04: two repositories, 1,000 one-minute legs each on 20 slots: within 10 % of an equal
/// share in every 10-minute window; 3:1 weights give 75 % ± 10 %.
#[test]
fn w05_ac04_fair_share() {
    for (wa, wb, want) in [(1000, 1000, 0.5), (3000, 1000, 0.75)] {
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
    let policy = RetryPolicy { max: 2, on: vec![super::retry::RetryOn::Class(FailureClass::Oom)], initial_ms: 10_000, factor: 2000, max_ms: 60_000, runner: super::retry::RunnerChoice::Any };
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
    let policy = RetryPolicy { max: 1, on: vec![super::retry::RetryOn::Class(FailureClass::Infra)], initial_ms: 30_000, factor: 1000, max_ms: 30_000, runner: super::retry::RunnerChoice::Any };
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
fn bench(id: &str, cpus: u64, gib: u64) -> LegIn {
    LegIn { exclusive: true, request: Resources { cpu: Cpus::whole(cpus), mem: gib << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) }
}

/// The fence of `leg`'s grant now (W05-R34); the default when it holds none.
fn fence(s: &Scheduler, leg: &str) -> Fence {
    s.grants().get(leg).map(|g| g.2).unwrap_or_default()
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
    let c = s.handle(Input::LegQueued(Box::new(bench("bench", 16, 16))), 1);
    assert_eq!(assigned(&c), vec!["bench"], "it doesn't wait for an empty runner");
    let mut on = Vec::new();
    for (i, id) in ["o1", "o2", "o3"].iter().enumerate() {
        on.extend(assigned(&s.handle(Input::LegQueued(Box::new(leg(id, "acme/x", 60))), 2 + i as i64)));
    }
    assert_eq!(on, vec!["o1", "o2"], "two ordinary legs beside it, in the half it doesn't hold");
    assert!(matches!(s.reason("o3"), Some(WaitReason::Capacity { eligible: 1, busy: 1, draining: 0 })), "{:?}", s.reason("o3"));
    // Another bench waits for its own share: of the 16 CPUs not held, the two legs there take 16.
    s.handle(Input::LegQueued(Box::new(bench("bench2", 16, 16))), 6);
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
    s.handle(Input::LegRunning { leg: Box::new(bench("bench", 16, 16)), runner: "a".into(), attempt: 1, fence: Fence::default() }, 0);
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 1);
    let mut on = Vec::new();
    for (i, id) in ["o1", "o2", "o3"].iter().enumerate() {
        on.extend(assigned(&s.handle(Input::LegQueued(Box::new(leg(id, "acme/x", 60))), 2 + i as i64)));
    }
    assert_eq!(on, vec!["o1", "o2"], "the adopted bench still holds half the runner");
    // And one asking for nothing, adopted, still holds all of it.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::LegRunning { leg: Box::new(LegIn { exclusive: true, ..leg("ex", "acme/x", 60) }), runner: "a".into(), attempt: 1, fence: Fence::default() }, 0);
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 1);
    let c = s.handle(Input::LegQueued(Box::new(leg("o", "acme/x", 60))), 2);
    assert!(assigned(&c).is_empty(), "nothing beside an adopted exclusive leg");
}

/// The rule both dispatchers and the runner use, in numbers: 32 CPUs, 64 GiB, 4 slots.
#[test]
fn the_room_rule_both_dispatchers_and_the_runner_use() {
    let size = Some(Resources { cpu: Cpus::whole(32), mem: 64 << 30, ..Resources::default() });
    let none = Resources::default();
    let half = Resources { cpu: Cpus::whole(16), mem: 16 << 30, ..Resources::default() };
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
    let big = Resources { cpu: Cpus::whole(4), mem: 60 << 30, ..Resources::default() };
    assert!(!room(4, size, &none, false, 1, 1, Incoming::Share(big)));
    assert_eq!(Incoming::of(true, &half), Incoming::Share(half));
    assert_eq!(Incoming::of(true, &none), Incoming::Whole);
    assert_eq!(Incoming::of(false, &half), Incoming::Ordinary);
}

/// SplitMix64, as kiln-sim draws: a seed's numbers, the same on every machine.
pub(super) struct Draw(pub(super) u64);

impl Draw {
    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub(super) fn pick<'a>(&mut self, of: &'a [String]) -> Option<&'a String> {
        of.get(usize::try_from(self.below(of.len() as u64)).unwrap_or(0))
    }
}

/// One seed's inputs, in order with their times: runners coming, going and back, legs queued at
/// mixed priorities over runs, repositories, actors and lanes (some exclusive, some with work other
/// runs adopt, some coalescing in a lane, some stopping on losing their lease), attempts finishing
/// and failing, refused, legs cancelled, runs superseded with deprioritize and pushes' demands
/// adopting, routing rules changed, grants renewed (some at attempts they don't hold), ticks.
fn inputs(seed: u64) -> Vec<(Input, i64)> {
    let mut d = Draw(seed);
    let (mut out, mut now, mut legs) = (Vec::new(), 0i64, Vec::<String>::new());
    let runners = 1 + d.below(4);
    let runner = |d: &mut Draw| format!("r{}", d.below(runners));
    for r in 0..runners {
        out.push((Input::RunnerUp(Box::new(linux(&format!("r{r}"), 1 + u32::try_from(d.below(3)).unwrap_or(0)))), now));
    }
    for _ in 0..60 {
        now += i64::try_from(d.below(5_000)).unwrap_or(0);
        let input = match d.below(100) {
            0..=39 => {
                let id = format!("l{}", legs.len());
                legs.push(id.clone());
                let mut l = leg(&id, &format!("acme/r{}", d.below(3)), u8::try_from(d.below(5) * 20).unwrap_or(60));
                l.run = format!("run{}", d.below(4));
                l.exclusive = d.below(10) == 0;
                // Who pushed it and its lane (fair share's levels, W28-R66), and its class (W05-R30);
                // work another run may adopt (W28-R42); a lane admission coalesces (W28-R61).
                (l.actor, l.lane) = (format!("a{}", d.below(3)), format!("ln{}", d.below(2)));
                l.class = super::state::Class::ALL.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default();
                if d.below(4) == 0 {
                    l.work = Some(super::demand::Work { key: Some(format!("K{}", d.below(3))), domain: super::demand::Domain::Trusted, lane: "acme/x".into(), observed: false });
                }
                if d.below(4) == 0 {
                    l.on_lease_loss = super::state::LeaseLoss::Stop;
                }
                // A member of its run's gang of two (W05-R40).
                if d.below(6) == 0 {
                    l.gang = Some(super::state::Gang { id: format!("{}-gang", l.run), size: 2 });
                }
                if d.below(5) == 0 {
                    let coalesce = (d.below(2) == 0).then_some(super::admit::Coalesce { quiet: 5_000, max_wait: 60_000, always: d.below(2) == 0 });
                    l.admission = Some(super::admit::Demand { lane: "acme/x\0ci\0main".into(), unit: "test".into(), order: u64::try_from(legs.len()).unwrap_or(0), coalesce, cadence: None, speculative: false });
                }
                Input::LegQueued(Box::new(l))
            }
            40..=59 => match d.pick(&legs) {
                Some(id) => {
                    let outcome = if d.below(4) == 0 { Outcome::Failed(super::retry::Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }) } else { Outcome::Success };
                    Input::AttemptFinished { leg: id.clone(), attempt: 1, outcome }
                }
                None => Input::Tick,
            },
            60..=66 => Input::RunQueued { run: format!("run{}", d.below(4)), group: None, supersede: Some(("acme/r0\0main".into(), Supersede::Deprioritize)) },
            67..=71 => Input::RulesChanged(vec![super::routing::native_rule()]),
            72..=77 => Input::RunnerLost { runner: runner(&mut d) },
            78..=84 => Input::RunnerUp(Box::new(linux(&runner(&mut d), 1 + u32::try_from(d.below(3)).unwrap_or(0)))),
            85..=89 => match d.pick(&legs) {
                Some(id) => Input::LegCancelled { leg: id.clone(), reason: "cancelled".into() },
                None => Input::Tick,
            },
            // A runner renews some legs under fences of this epoch, its own or not; a refusal
            // (W05-R34).
            95..=97 => {
                let mut held = Vec::new();
                for _ in 0..d.below(4) {
                    if let Some(id) = d.pick(&legs).cloned() {
                        held.push((id, Fence { epoch: seed, n: 1 + d.below(20) }));
                    }
                }
                Input::Renewed { runner: runner(&mut d), held }
            }
            98 => match d.pick(&legs) {
                Some(id) => Input::Rejected { leg: id.clone(), fence: Fence { epoch: seed, n: 1 + d.below(20) } },
                None => Input::Tick,
            },
            // A push's demand, then its run, which adopts what an older one has in flight (W28-R46).
            90..=94 => {
                let run = format!("run{}", d.below(4));
                let demand = super::demand::Demand { run: run.clone(), lane: "acme/x".into(), commit: format!("c{}", d.below(8)), domain: super::demand::Domain::Trusted, at: now, ..super::demand::Demand::default() };
                out.push((Input::Demand(Box::new(demand)), now));
                Input::RunQueued { run, group: None, supersede: Some(("acme/x".into(), Supersede::Adopt)) }
            }
            _ => Input::Tick,
        };
        out.push((input, now));
    }
    out
}

/// E1 (docs/needs.md), W05: the engine decides in a fixed order. The same inputs given to two
/// schedulers in one process, whose hash maps hash apart (each map draws its own keys), give the
/// same commands in the same order: 1,000 seeds of runners coming and going, legs of every class
/// queued, finished and cancelled, runs deprioritized and routing rules changed.
#[test]
fn e1_3_two_schedulers_given_the_same_inputs_decide_alike() {
    use super::state::{Class, ClassConfig, FairBy};
    let mut seen = [0u64; 5];
    for seed in 0..1_000u64 {
        // Fair share across repositories, or within them by actor and lane too (W28-R66); the
        // classes' floors and bounds as they come, or, every other seed, higher floors and bounds
        // a seed's minutes pass (W05-R31).
        let fair_by = match seed % 3 {
            0 => vec![FairBy::Repo],
            1 => vec![FairBy::Repo, FairBy::Actor],
            _ => vec![FairBy::Repo, FairBy::Actor, FairBy::Lane],
        };
        // Grants as leases short enough to run out in a seed's minutes, in a seed of every four
        // (W05-R34); backfilling in a third of the seeds, room kept for exclusive legs (W05-R39).
        let lease = (seed % 4 == 3).then_some(20_000);
        let mut cfg = Config { fair_by, lease, backfill: (seed / 3) % 3 == 0, ..Config::default() };
        if seed % 2 == 1 {
            cfg.classes = [(Class::Batch, ClassConfig { floor: 300, bound: Some(40_000) }), (Class::Background, ClassConfig { floor: 200, bound: Some(60_000) })].into_iter().collect();
        }
        let (mut a, mut b) = (Scheduler::with_epoch(cfg.clone(), seed), Scheduler::with_epoch(cfg, seed));
        for (step, ((x, now), (y, _))) in inputs(seed).into_iter().zip(inputs(seed)).enumerate() {
            let (ca, cb) = (a.handle(x, now), b.handle(y, now));
            for c in &ca {
                match c {
                    Command::Adopted { .. } => seen[0] += 1,
                    Command::Demand { .. } => seen[1] += 1,
                    Command::Wait { reason: WaitReason::Coalescing { .. }, .. } => seen[2] += 1,
                    Command::Lost { why: super::state::Loss::Expired, .. } => seen[3] += 1,
                    Command::Wait { reason: WaitReason::Reserved { .. }, .. } => seen[4] += 1,
                    _ => {}
                }
            }
            assert_eq!(ca, cb, "seed {seed}, input {step} at {now}");
            let grants = a.check_grants();
            assert!(grants.is_ok(), "seed {seed}, input {step} at {now}: {grants:?}");
        }
    }
    // What the seeds cover of the work in flight, admission, leases and reservations: adoptions,
    // demands' decisions, legs coalescing, grants run out, room kept.
    assert!(seen.iter().all(|n| *n > 0), "adopted, demand decisions, coalescing, expired, reserved: {seen:?}");
}

/// E1 (docs/needs.md), W05-R03: the rule counts in thousandths and compares without dividing, so
/// a share that fills a runner to the last thousandth fits, and one thousandth more doesn't, where
/// thirds made the float version lean on an epsilon.
#[test]
fn e1_1_the_room_rule_counts_exactly() {
    use super::state::slots_left;
    let three = Some(Resources { cpu: Cpus::whole(3), mem: 3 << 30, ..Resources::default() });
    let none = Resources::default();
    // One ordinary leg of three slots takes a third: two CPUs are left, exactly.
    let two = Resources { cpu: Cpus::whole(2), ..Resources::default() };
    assert!(room(3, three, &none, false, 1, 1, Incoming::Share(two)));
    let more = Resources { cpu: Cpus::milli(2001), ..Resources::default() };
    assert!(!room(3, three, &none, false, 1, 1, Incoming::Share(more)));
    // A third held leaves two of three slots, not 1.9999999999999998 of them.
    let third = Resources { cpu: Cpus::whole(1), mem: 1 << 30, ..Resources::default() };
    assert_eq!(slots_left(3, three, &third), 2);
    assert_eq!(slots_left(3, three, &Resources { cpu: Cpus::milli(1001), ..Resources::default() }), 1);
    // Fractions add without drift: ten tenths held are one CPU.
    let mut held = Resources::default();
    for _ in 0..10 {
        held.cpu = held.cpu.saturating_add(Cpus::milli(100));
    }
    assert_eq!(held.cpu, Cpus::whole(1));
    assert_eq!(slots_left(4, Some(Resources { cpu: Cpus::whole(4), ..Resources::default() }), &held), 3);
}

/// A share bigger than any runner it could run on waits, saying so.
#[test]
fn a_share_no_runner_can_hold_waits_saying_why() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(machine("a", 4, 32, 64))), 0);
    s.handle(Input::LegQueued(Box::new(bench("huge", 64, 16))), 1);
    assert!(matches!(s.reason("huge"), Some(WaitReason::Unroutable { failing }) if failing.iter().any(|f| f.contains("64 CPUs"))), "{:?}", s.reason("huge"));
}

/// In resource mode the share comes out of the runner before overcommit: ordinary legs
/// overcommit only what no bench holds.
#[test]
fn in_resource_mode_ordinary_legs_overcommit_only_what_isnt_held() {
    let mut s = Scheduler::new(Config { resources: true, ..Config::default() });
    let big = RunnerIn { capacity: Some(Resources { cpu: Cpus::whole(32), mem: 64 << 30, disk: 500 << 30, gpu: 0 }), ..machine("a", 99, 32, 64) };
    s.handle(Input::RunnerUp(Box::new(big)), 0);
    assert_eq!(assigned(&s.handle(Input::LegQueued(Box::new(bench("bench", 16, 16))), 1)), vec!["bench"]);
    let cpu = |id: &str| LegIn { request: Resources { cpu: Cpus::whole(4), mem: 2 << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) };
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
    let big = RunnerIn { capacity: Some(Resources { cpu: Cpus::whole(4), mem: 8 << 30, disk: 100 << 30, gpu: 0 }), ..linux("a", 99) };
    s.handle(Input::RunnerUp(Box::new(big)), 0);
    let cpu = |id: &str, c: u64| LegIn { request: Resources { cpu: Cpus::whole(c), mem: 1 << 30, ..Resources::default() }, ..leg(id, "acme/x", 60) };
    let mut assigned = 0;
    for i in 0..4 {
        assigned += s.handle(Input::LegQueued(Box::new(cpu(&format!("c{i}"), 2))), i).iter().filter(|c| matches!(c, Command::Assign { .. })).count();
    }
    assert_eq!(assigned, 3, "6 CPUs fit 4 × 1.5 overcommit; the fourth waits");
    // Cancelling a queued leg settles it; a running one is stopped on its runner.
    let c = s.handle(Input::LegCancelled { leg: "c3".into(), reason: "fail-fast".into() }, 10);
    assert_eq!(c, vec![Command::Settled { leg: "c3".into(), outcome: Outcome::Cancelled, reason: Some("fail-fast".into()) }]);
    let c = s.handle(Input::LegCancelled { leg: "c0".into(), reason: "run cancelled".into() }, 11);
    assert_eq!(c, vec![Command::Cancel { leg: "c0".into(), attempt: 1, reason: "run cancelled".into() }]);
}

/// W05-R03 with needs (`sched::needs`): a leg runs only where the runner's facts meet its
/// constraints and its request has room. One asking for more than any runner that could take it
/// has waits, saying what it asks and what each has; one only out of room for now waits for room.
#[test]
fn w05_r03_needs_place_a_leg_and_one_too_big_waits_saying_so() -> Result<(), String> {
    const GIB: u64 = 1 << 30;
    let mut s = Scheduler::new(Config { resources: true, ..Config::default() });
    let runner = |name: &str, os: &str, arch: &str, cpu: u64, mem: u64| RunnerIn {
        view: RunnerView { facts: RunnerFacts { os: os.into(), arch: arch.into(), ..RunnerFacts::default() }, ..linux(name, 99).view },
        capacity: Some(Resources { cpu: Cpus::whole(cpu), mem: mem * GIB, disk: 500 * GIB, gpu: 0 }),
        ..linux(name, 99)
    };
    s.handle(Input::RunnerUp(Box::new(runner("small", "linux", "x86_64", 8, 16))), 0);
    s.handle(Input::RunnerUp(Box::new(runner("large", "linux", "x86_64", 16, 64))), 0);
    s.handle(Input::RunnerUp(Box::new(runner("mac", "macos", "aarch64", 24, 192))), 0);
    let asking = |id: &str, spec: &str| -> Result<LegIn, String> {
        let n = super::needs::parse(spec)?;
        Ok(LegIn { request: n.request, exclusive: n.exclusive, route: LegRoute { labels: vec!["k8s".into()], requires: n.requires, ..LegRoute::default() }, ..leg(id, "acme/x", 60) })
    };
    let to = |c: &[Command], id: &str| {
        c.iter().find_map(|x| match x {
            Command::Assign { leg, runner, .. } if leg == id => Some(runner.clone()),
            _ => None,
        })
    };
    // The Mac has more of everything but isn't linux/amd64, and the small runner hasn't 32 GiB.
    let c = s.handle(Input::LegQueued(Box::new(asking("fits", "cpus=8,mem=32g,linux/amd64")?)), 1);
    assert_eq!(to(&c, "fits").as_deref(), Some("large"), "{c:?}");
    // Without the constraint, 128 GiB goes to the one runner with it.
    let c = s.handle(Input::LegQueued(Box::new(asking("big", "mem=128g")?)), 2);
    assert_eq!(to(&c, "big").as_deref(), Some("mac"), "{c:?}");
    // More than any linux/amd64 runner has even idle: it waits saying so, the Mac not named.
    s.handle(Input::LegQueued(Box::new(asking("huge", "mem=128g,linux/amd64")?)), 3);
    assert_eq!(s.reason("huge"), Some(&WaitReason::Oversized { asked: "128 GiB of memory".into(), short: vec!["large has 64 GiB of memory".into(), "small has 16 GiB of memory".into()] }));
    // 40 GiB fits the large runner idle: it waits for room there, not for a larger runner.
    s.handle(Input::LegQueued(Box::new(asking("second", "cpus=8,mem=40g,linux/amd64")?)), 4);
    assert!(matches!(s.reason("second"), Some(WaitReason::Capacity { eligible: 2, busy: 2, draining: 0 })), "{:?}", s.reason("second"));
    Ok(())
}

/// W05-AC09's core: after a restart, running legs are adopted (not assigned again) and their
/// runner's capacity counts them.
#[test]
fn restart_adopts_running_legs() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::LegRunning { leg: Box::new(leg("run1", "acme/x", 60)), runner: "a".into(), attempt: 2, fence: Fence::default() }, 0);
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
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: fence(&s, "x") }, 2);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { runner, attempt: 1, .. } if runner == "b")), "{c:?}");
    // With no other runner, it waits for one, and says so.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: fence(&s, "x") }, 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("x"), Some(&WaitReason::OtherRunner { avoided: "a".into(), refused: true, asked: false }));
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
    let strict = RetryPolicy { max: 1, on: vec![super::retry::RetryOn::Class(FailureClass::RunnerLost)], initial_ms: 0, factor: 1000, max_ms: 0, runner: super::retry::RunnerChoice::Different };
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { retry: Some(strict), ..leg("z", "acme/x", 60) })), 0);
    assert!(assigned(&c, "z", "a", 1), "{c:?}");
    s.handle(Input::RunnerLost { runner: "a".into() }, 1);
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("z"), Some(&WaitReason::OtherRunner { avoided: "a".into(), refused: false, asked: false }));
    let c = s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 3);
    assert!(assigned(&c, "z", "b", 2), "{c:?}");
}

/// W05-R28: a retry a person placed. `runner` runs it only there, waiting for it busy, draining,
/// offline or unable, and saying which; `avoid` runs it anywhere else, waiting for another when
/// only that one could. Either holds through the attempt's own retries, and a refusal by the
/// runner asked for doesn't send it away for good.
#[test]
fn w05_r28_a_placed_retry() {
    use super::state::{PinnedState, Placement};
    let assigned = |c: &[Command], l: &str, r: &str| c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == l && runner == r));
    let on = |r: &str| Placement { runner: Some(r.into()), avoid: None };
    let off = |r: &str| Placement { runner: None, avoid: Some(r.into()) };
    let pinned = |r: &str, state: PinnedState| Some(WaitReason::Pinned { runner: r.into(), state });

    // Pinned to b while a is free: it waits for b, busy, and runs there once b has room.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(leg("busy", "acme/x", 60))), 0);
    let first = if assigned(&c, "busy", "a") { "a" } else { "b" };
    let other = if first == "a" { "b" } else { "a" };
    let c = s.handle(Input::LegQueued(Box::new(LegIn { placement: on(first), ..leg("x", "acme/x", 60) })), 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{other} is free, and not the one asked for: {c:?}");
    assert_eq!(s.reason("x"), pinned(first, PinnedState::Busy).as_ref());
    // The explanation says why the free one can't take it.
    let free = s.explain("x").into_iter().find(|e| e.runner == other);
    assert!(free.as_ref().is_some_and(|e| !e.eligible && e.checks.iter().any(|c| !c.pass && c.requirement == format!("runner == {first} (asked by its retry)"))), "{free:?}");
    let c = s.handle(Input::AttemptFinished { leg: "busy".into(), attempt: 1, outcome: Outcome::Success }, 2);
    assert!(assigned(&c, "x", first), "{c:?}");
    // Lost with its runner, its retry waits for that runner, offline, and goes back to it.
    let c = s.handle(Input::RunnerLost { runner: first.into() }, 3);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "x")), "{c:?}");
    assert_eq!(s.reason("x"), pinned(first, PinnedState::Offline).as_ref());
    let c = s.handle(Input::RunnerUp(Box::new(linux(first, 1))), 4);
    assert!(assigned(&c, "x", first), "{c:?}");

    // Draining, or no longer able to run it (its labels changed): said so.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerDraining { runner: "a".into() }, 0);
    s.handle(Input::LegQueued(Box::new(LegIn { placement: on("a"), ..leg("d", "acme/x", 60) })), 1);
    assert_eq!(s.reason("d"), pinned("a", PinnedState::Draining).as_ref());
    let mut gpu = linux("a", 1);
    gpu.view.labels = vec!["gpu".into()];
    s.handle(Input::RunnerChanged(Box::new(gpu)), 2);
    assert_eq!(s.reason("d"), pinned("a", PinnedState::Unable).as_ref());

    // Refused by the runner asked for: it stays that runner's, asked again a while later, on the
    // same attempt.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { placement: on("a"), ..leg("r", "acme/x", 60) })), 0);
    assert!(assigned(&c, "r", "a"), "{c:?}");
    let c = s.handle(Input::Rejected { leg: "r".into(), fence: fence(&s, "r") }, 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. } | Command::Retry { .. })), "neither the other nor at once: {c:?}");
    assert!(c.contains(&Command::Timer { at: 1 + super::state::REFUSED_AGAIN_MS }), "{c:?}");
    assert_eq!(s.reason("r"), pinned("a", PinnedState::Busy).as_ref());
    let c = s.handle(Input::Tick, 1 + super::state::REFUSED_AGAIN_MS);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, attempt: 1, .. } if leg == "r" && runner == "a")), "{c:?}");

    // Anywhere but a: b takes it though a is free; with b busy it waits for room, and with only a
    // left, for another runner, as its retry asked.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    let c = s.handle(Input::LegQueued(Box::new(LegIn { placement: off("a"), ..leg("e1", "acme/x", 60) })), 1);
    assert!(assigned(&c, "e1", "b"), "{c:?}");
    let c = s.handle(Input::LegQueued(Box::new(LegIn { placement: off("a"), ..leg("e2", "acme/x", 60) })), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "e2")), "{c:?}");
    assert_eq!(s.reason("e2"), Some(&WaitReason::Capacity { eligible: 1, busy: 1, draining: 0 }));
    s.handle(Input::RunnerLost { runner: "b".into() }, 3);
    assert_eq!(s.reason("e2"), Some(&WaitReason::OtherRunner { avoided: "a".into(), refused: false, asked: true }));
    // e1's retry after b was lost doesn't go to a either, though nothing else can take it.
    assert!(matches!(s.reason("e1"), Some(WaitReason::OtherRunner { asked: true, .. })), "{:?}", s.reason("e1"));
    let c = s.handle(Input::RunnerUp(Box::new(linux("c", 2))), 4);
    assert!(assigned(&c, "e1", "c") && assigned(&c, "e2", "c"), "{c:?}");
}

/// W05-R16: "when one can take it" is one big enough to hold it. A leg lost with its runner goes
/// back to it once it's back when the only other could never hold the leg: a share of 4 CPUs where
/// the other has 2, and in resource mode, 6 CPUs asked where the other offers 2 (× 1.5). Found by
/// kiln-sim's scheduler model (docs/sim.md): the leg waited for good for a runner too small for it.
#[test]
fn w05_r16_a_lost_leg_goes_back_when_no_other_runner_could_hold_it() {
    let assigned = |c: &[Command], l: &str, r: &str, n: u32| c.iter().any(|x| matches!(x, Command::Assign { leg, runner, attempt, .. } if leg == l && runner == r && *attempt == n));
    let sized = |name: &str, cpus: u32, gib: u64| {
        let mut r = linux(name, 2);
        r.view.facts.cpu.threads = cpus;
        r.view.facts.mem.total = gib << 30;
        r
    };
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(sized("big", 8, 16))), 0);
    s.handle(Input::RunnerUp(Box::new(sized("small", 2, 4))), 0);
    let share = LegIn { exclusive: true, request: Resources { cpu: Cpus::whole(4), mem: 4 << 30, ..Resources::default() }, ..leg("x", "acme/x", 60) };
    let c = s.handle(Input::LegQueued(Box::new(share)), 0);
    assert!(assigned(&c, "x", "big", 1), "{c:?}");
    s.handle(Input::RunnerLost { runner: "big".into() }, 1);
    let c = s.handle(Input::RunnerUp(Box::new(sized("big", 8, 16))), 2);
    assert!(assigned(&c, "x", "big", 2), "small could never hold its share: {c:?}");
    let offers = |name: &str, cpus: u64| {
        let mut r = linux(name, 4);
        r.capacity = Some(Resources { cpu: Cpus::whole(cpus), mem: 16 << 30, ..Resources::default() });
        r
    };
    let mut s = Scheduler::new(Config { resources: true, ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(offers("big", 8))), 0);
    s.handle(Input::RunnerUp(Box::new(offers("small", 2))), 0);
    let heavy = LegIn { request: Resources { cpu: Cpus::whole(6), mem: 1 << 30, ..Resources::default() }, ..leg("y", "acme/x", 60) };
    let c = s.handle(Input::LegQueued(Box::new(heavy)), 0);
    assert!(assigned(&c, "y", "big", 1), "{c:?}");
    s.handle(Input::RunnerLost { runner: "big".into() }, 1);
    let c = s.handle(Input::RunnerUp(Box::new(offers("big", 8))), 2);
    assert!(assigned(&c, "y", "big", 2), "small could never hold what it asks: {c:?}");
}

/// W05-R13: a leg the only runner big enough to hold it refused waits for another, and says so
/// (`other-runner`), though a smaller runner is eligible: it's no capacity that it waits for.
#[test]
fn w05_r13_a_leg_refused_by_the_only_runner_big_enough_says_so() {
    let sized = |name: &str, cpus: u32| {
        let mut r = linux(name, 2);
        r.view.facts.cpu.threads = cpus;
        r.view.facts.mem.total = 16 << 30;
        r
    };
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(sized("big", 8))), 0);
    s.handle(Input::RunnerUp(Box::new(sized("small", 2))), 0);
    let share = LegIn { exclusive: true, request: Resources { cpu: Cpus::whole(4), mem: 4 << 30, ..Resources::default() }, ..leg("x", "acme/x", 60) };
    s.handle(Input::LegQueued(Box::new(share)), 0);
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: fence(&s, "x") }, 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("x"), Some(&WaitReason::OtherRunner { avoided: "big".into(), refused: true, asked: false }));
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
    assert_eq!(cpus("2"), Ok(Cpus::whole(2)));
    assert_eq!(cpus("500m"), Ok(Cpus::milli(500)));
    assert_eq!(cpus("0.25"), Ok(Cpus::milli(250)));
    assert_eq!(cpus("-1"), Err(super::pools::NOT_CPUS));
    assert_eq!(cpus("lots"), Err(super::pools::NOT_CPUS));
    let r = ResourceSpec { cpu: Some("1500m".into()), memory: Some("2Gi".into()), disk: Some("10G".into()), gpu: Some(1) }.resources()?;
    assert_eq!(r, Resources { cpu: Cpus::milli(1500), mem: 2 << 30, disk: 10_000_000_000, gpu: 1 });
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
    assert_eq!((pool.request.cpu, pool.request.mem), (Cpus::milli(100), 64 << 20), "a pool's executor asks a tenth of a CPU");
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
        cost: crate::sched::units::ms_of_secs(cost),
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
    let c = s.handle(Input::LegRunning { leg: Box::new(running), runner: "a".into(), attempt: 1, fence: Fence::default() }, 0);
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

// ---------------------------------------------------------------------- admission (W28-R60 to R63)

const SEC: i64 = 1_000;

fn lane_leg(id: &str, order: u64, coalesce: Option<super::admit::Coalesce>, cadence: Option<super::admit::Cadence>) -> LegIn {
    let admission = super::admit::Demand { lane: "acme/x\0ci\0main".into(), unit: "test".into(), order, coalesce, cadence, speculative: false };
    LegIn { admission: Some(admission), ..leg(id, "acme/x", 60) }
}

/// W28-R61, W28-AC60: pushes every 30 s from 10 s, quiet 45 s, max_wait 5 min, the one runner busy
/// with another workflow's leg until 290 s: each push skips the one held (coalesced, never
/// started), and the newest is let go as the runner frees, before 300 s.
#[test]
fn w28_ac60_coalescing_runs_the_newest_once_the_runner_frees() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, leg("blocker", "acme/y", 60), 290 * SEC);
    let busy = super::admit::Coalesce { quiet: 45 * SEC, max_wait: 300 * SEC, always: false };
    for i in 0..10 {
        sim.leg(10 * SEC + i * 30 * SEC, lane_leg(&format!("p{i}"), i as u64, Some(busy), None), 60 * SEC);
    }
    sim.run(400 * SEC);
    let ran: Vec<&str> = sim.assigned.iter().map(|a| a.leg.as_str()).filter(|l| l.starts_with('p')).collect();
    assert_eq!(ran, vec!["p9"], "only the newest ran");
    assert_eq!(sim.started("p9"), Some(290 * SEC), "as the runner freed");
    let coalesced = sim.settled.iter().filter(|(_, l, o)| l.starts_with('p') && *o == Outcome::Cancelled).count();
    assert_eq!(coalesced, 9);
    assert!(sim.waits.iter().any(|(_, l, r)| l == "p0" && matches!(r, WaitReason::Coalescing { until, deadline, .. } if *until == 55 * SEC && *deadline == 310 * SEC)));
}

/// W28-R61: on an idle system a push starts at once: admission delay 0 s.
#[test]
fn w28_r61_an_idle_system_starts_a_push_at_once() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    let busy = super::admit::Coalesce { quiet: 45 * SEC, max_wait: 300 * SEC, always: false };
    sim.leg(5 * SEC, lane_leg("p0", 0, Some(busy), None), 60 * SEC);
    sim.run(10 * SEC);
    assert_eq!(sim.started("p0"), Some(5 * SEC));
}

/// W28-R61: under a stream that keeps the runner busy past max_wait, the burst's newest is let go at
/// max_wait (it then waits for capacity like any leg), and later pushes start a burst of their own.
#[test]
fn w28_r61_max_wait_bounds_a_burst() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, leg("blocker", "acme/y", 60), 2_000 * SEC);
    let busy = super::admit::Coalesce { quiet: 45 * SEC, max_wait: 300 * SEC, always: false };
    for i in 0..20 {
        sim.leg(10 * SEC + i * 30 * SEC, lane_leg(&format!("p{i}"), i as u64, Some(busy), None), 60 * SEC);
    }
    sim.run(700 * SEC);
    // Let go at 310 s (10 s + max_wait): p10, the newest then (it arrived at the deadline), waits
    // for the runner; the next burst's newest, p19, is let go at its own quiet's end.
    for id in ["p10", "p19"] {
        assert!(matches!(sim.last_wait(id), Some(WaitReason::Capacity { .. })), "{id}: {:?}", sim.last_wait(id));
        assert!(!sim.settled.iter().any(|(_, l, _)| l == id), "{id} never skipped once let go");
    }
    assert_eq!(sim.settled.iter().filter(|(_, l, o)| l.starts_with('p') && *o == Outcome::Cancelled).count(), 18);
}

/// W28-R62: on a cadence an exclusive leg's runner takes no new work while it waits, so it starts
/// once the work there ends; without, later work keeps filling the runner.
#[test]
fn w28_r62_cadence_drains_for_an_exclusive_leg() {
    for (cadence, want) in [(Some(super::admit::Cadence::Free), vec![Some(0), Some(100 * SEC), Some(110 * SEC)]), (None, vec![Some(0), Some(120 * SEC), Some(20 * SEC)])] {
        let mut sim = Sim::new(Config::default());
        sim.runner(0, linux("bench", 2));
        sim.leg(0, leg("a", "acme/x", 60), 100 * SEC);
        let mut b = lane_leg("b", 1, None, cadence);
        b.exclusive = true;
        sim.leg(10 * SEC, b, 10 * SEC);
        sim.leg(20 * SEC, leg("c", "acme/x", 60), 100 * SEC);
        sim.run(500 * SEC);
        assert_eq!(vec![sim.started("a"), sim.started("b"), sim.started("c")], want, "cadence {cadence:?}");
    }
}

/// W28-R62: a newer leg on the cadence takes the place of the one waiting to start.
#[test]
fn w28_r62_cadence_runs_the_newest() {
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("bench", 1));
    sim.leg(0, leg("busy", "acme/y", 60), 100 * SEC);
    for i in 0..3 {
        sim.leg(10 * SEC + i * 10 * SEC, lane_leg(&format!("b{i}"), i as u64, None, Some(super::admit::Cadence::Free)), 10 * SEC);
    }
    sim.run(500 * SEC);
    assert_eq!(sim.started("b2"), Some(100 * SEC));
    assert_eq!(sim.started("b0"), None);
    assert!(sim.settled.iter().any(|(_, l, o)| l == "b1" && *o == Outcome::Cancelled));
}

/// W28-R68: a lane's speculative legs beyond its window wait, saying so, until one passes.
#[test]
fn w28_r68_speculation_window_holds_and_lets_go() {
    let cfg = Config { window: super::admit::Window { initial: 2, floor: 1, cap: 4 }, ..Config::default() };
    let mut sim = Sim::new(cfg);
    sim.runner(0, linux("a", 4));
    for i in 0..3 {
        let mut l = lane_leg(&format!("s{i}"), i as u64, None, None);
        if let Some(a) = l.admission.as_mut() {
            a.speculative = true;
            a.unit = format!("bisect{i}");
        }
        sim.leg(0, l, 30 * SEC);
    }
    sim.run(SEC);
    assert!(matches!(sim.last_wait("s2"), Some(WaitReason::Speculation { size: 2, in_flight: 2, .. })));
    assert_eq!(sim.started("s2"), None);
    sim.run(40 * SEC);
    assert_eq!(sim.started("s2"), Some(30 * SEC), "the first pass widened the window");
}

// ---------------------------------------------------------------------- priority by intent (W28-R64, R65)

/// W28-R64, W28-AC62: one saturated runner; gate, waiting, trunk and WIP legs queued together
/// start in that order.
#[test]
fn w28_ac62_intent_classes_go_in_order() {
    use super::intent::Intent;
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, leg("busy", "acme/x", 60), MIN);
    for (id, i) in [("wip", Intent::Wip), ("trunk", Intent::Trunk), ("gate", Intent::Gate), ("waiting", Intent::Waiting)] {
        sim.leg(1, leg(id, "acme/x", i.priority()), MIN);
    }
    sim.run(10 * MIN);
    let order: Vec<&str> = sim.assigned.iter().map(|a| a.leg.as_str()).filter(|l| *l != "busy").collect();
    assert_eq!(order, vec!["gate", "waiting", "trunk", "wip"]);
}

/// W28-R65, grown into W05-R31 (docs/needs.md E2.3): a gate leg is `land` and work in progress
/// `batch`, and a WIP leg behind a steady stream of gate legs (one a minute, each a minute long)
/// starts through batch's floor as soon as the runner frees, where aging between priorities let it
/// through only after 50 minutes.
#[test]
fn w28_r65_a_wip_leg_behind_gate_starts_through_batchs_floor() {
    use super::intent::Intent;
    use super::state::Class;
    let gate = |id: &str| LegIn { class: Class::Land, ..leg(id, "acme/x", Intent::Gate.priority()) };
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    sim.leg(0, gate("gate-busy"), MIN);
    sim.leg(1, LegIn { class: Class::Batch, ..leg("wip", "acme/x", Intent::Wip.priority()) }, MIN);
    for i in 0..120 {
        sim.leg(i * MIN + 2, gate(&format!("gate{i}")), MIN);
    }
    sim.run(150 * MIN);
    assert_eq!(sim.started("wip"), Some(MIN), "the WIP leg starts as the gate leg ahead of it ends");
}

// ---------------------------------------------------------------------- fair share by actor and lane (W28-R66)

fn by_actor(id: &str, actor: &str, lane: &str) -> LegIn {
    LegIn { actor: actor.into(), lane: lane.into(), ..leg(id, "acme/x", 60) }
}

/// W28-R66, W28-AC63: agent A pushes ten times a minute, agent B once, in one repository, each push
/// a 40-second leg on one slot: with fair_by = [repo, actor] each gets half the capacity within 10
/// points over every 10-minute window, where by repository alone B gets its arrival share.
#[test]
fn w28_ac63_two_agents_at_ten_to_one_share_alike() {
    use super::state::FairBy;
    for (fair_by, fair) in [(vec![FairBy::Repo, FairBy::Actor], true), (vec![FairBy::Repo], false)] {
        let mut sim = Sim::new(Config { fair_by, default_cost: 40_000, ..Config::default() });
        sim.runner(0, linux("a", 1));
        for m in 0..80 {
            for k in 0..10 {
                sim.leg(m * MIN + k * 6 * SEC, by_actor(&format!("a{m}-{k}"), "agent:A", "acme/x\0ci\0agents/a"), 40 * SEC);
            }
            sim.leg(m * MIN + 3 * SEC, by_actor(&format!("b{m}"), "agent:B", "acme/x\0ci\0agents/b"), 40 * SEC);
        }
        sim.run(80 * MIN);
        let mut w = 10;
        while w + 10 <= 70 {
            let share = sim.shares_by(w * MIN, (w + 10) * MIN, |l| l.get(..1).unwrap_or_default().to_owned()).get("b").copied().unwrap_or(0.0);
            if fair {
                assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: B got {share:.2}", w + 10);
            } else {
                assert!(share < 0.25, "by repository alone, window {w}–{}: B got {share:.2}", w + 10);
            }
            w += 10;
        }
    }
}

/// W28-R66: fair_by = [repo, actor, lane]: actor A's two lanes get about a quarter each, actor B's
/// one lane about half.
#[test]
fn w28_r66_nested_levels_split_their_parents_share() {
    use super::state::FairBy;
    let mut sim = Sim::new(Config { fair_by: vec![FairBy::Repo, FairBy::Actor, FairBy::Lane], default_cost: 60_000, ..Config::default() });
    sim.runner(0, linux("a", 2));
    for i in 0..200 {
        sim.leg(1, by_actor(&format!("x{i}"), "agent:A", "x"), MIN);
        sim.leg(1, by_actor(&format!("y{i}"), "agent:A", "y"), MIN);
        sim.leg(1, by_actor(&format!("z{i}"), "agent:B", "z"), MIN);
    }
    sim.run(100 * MIN);
    let shares = sim.shares_by(0, 60 * MIN, |l| l.get(..1).unwrap_or_default().to_owned());
    for (lane, want) in [("x", 0.25), ("y", 0.25), ("z", 0.5)] {
        let got = shares.get(lane).copied().unwrap_or(0.0);
        assert!((got - want).abs() <= 0.10, "lane {lane}: {got:.2}, want {want}");
    }
}

// ------------------------------------------------------------------ warm placement (W24-R18)

mod warm_placement {
use super::*;

use crate::sched::routing::Affinity;
use crate::sched::sim::Assigned;
use crate::sched::warm::{self, Held, Wants, Workspace};
use proptest::prelude::*;
use std::collections::BTreeMap;

const LANE: &str = "acme-x-rust";

fn warm_leg(id: &str, cold_s: f64, patience: Option<f64>) -> LegIn {
    LegIn { warm_lane: Some(LANE.into()), cold: crate::sched::units::ms_of_secs(cold_s), prefer: vec![Preference { warm: true, patience, ..Preference::default() }], ..leg(id, "acme/x", 60) }
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
    let placed: Vec<Option<String>> = ["x1", "x2", "x3"].iter().map(|id| assigned_to(&s.handle(Input::LegQueued(Box::new(LegIn { cost: Some(120_000), ..leg(id, "acme/x", 60) })), 1), id)).collect();
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
        s.handle(Input::LegQueued(Box::new(LegIn { cost: Some(30_000), ..leg(f, "acme/x", 60) })), 0);
    }
    s.handle(Input::LegQueued(Box::new(warm_leg("q", 120.0, Some(0.0)))), 0);
    assert!(matches!(s.reason("q"), Some(WaitReason::Capacity { eligible: 3, busy: 3, .. })), "{:?}", s.reason("q"));
    let why = s.explain("q");
    let of = |r: &str| why.iter().find(|e| e.runner == r).cloned().ok_or(format!("no {r}"));
    let (a, b, c) = (of("a")?, of("b")?, of("c")?);
    let (wa, wb, wc) = (a.warm.clone().ok_or("a")?, b.warm.clone().ok_or("b")?, c.warm.clone().ok_or("c")?);
    assert!(wa.estimate.lane.is_none() && near(wa.finish_s, 150.0), "cold: {wa:?}");
    assert!(wb.estimate.lane.as_deref() == Some(LANE) && wb.estimate.run_ms == 20_000 && near(wb.wait_s, 30.0) && near(wb.finish_s, 50.0), "{wb:?}");
    assert!(wc.estimate.run_ms == 92_500 && wc.estimate.fetch_ms == 20_480 && (wc.estimate.files, wc.estimate.behind) == (300, 75), "{wc:?}");
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
        let mut l = LegIn { cost: Some(100_000), prefer: vec![Preference { affinity: Some(Affinity { job: "build".into() }), ..Preference::default() }], ..leg(id, repo, 60) };
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
    use crate::sched::routing::{Rule, RuleMatch};
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
    assert!(a.estimate.lane.is_none() && a.estimate.run_ms == 120_000, "on the host, cold: {a:?}");
    assert!(w.estimate.lane.as_deref() == Some(LANE) && w.estimate.run_ms == 20_000, "{w:?}");
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
            let l = if terms { warm_leg(id, 120.0, None) } else { LegIn { cost: Some(120_000), ..leg(id, "acme/x", 60) } };
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
    sim.leg(1, LegIn { cost: Some(300_000), ..leg("slow", "acme/x", 60) }, 300_000);
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
                let mut l = LegIn { cost: Some(u64::from(secs) * 1000), ..leg(&f, "acme/x", 60) };
                l.route.labels.push(name.clone());
                prop_assert_eq!(assigned_to(&s.handle(Input::LegQueued(Box::new(l)), 0), &f), Some(name));
            }
        }
        let t0 = 500;
        let c = s.handle(Input::LegQueued(Box::new(warm_leg("x", cold, Some(share)))), t0);
        // The rule, from each runner's estimate, in the engine's units (E1): what's written in
        // seconds and shares, converted at the edge as the engine converts it.
        let m = warm::Model::default();
        let (cold_ms, share_ppm) = (crate::sched::units::ms_of_secs(cold).unwrap_or(0), crate::sched::units::ppm_of(share));
        let busy: Vec<u64> = fleet.iter().map(|b| warm::estimate(held(b).as_ref(), &[Wants::Lane(LANE)], cold_ms, &m, None).busy_ms()).collect();
        let at = |i: usize| busy.get(i).copied().unwrap_or(u64::MAX);
        let free: Vec<usize> = (0..fleet.len()).filter(|i| fleet.get(*i).is_some_and(|b| b.busy.is_none())).collect();
        let Some(first) = free.iter().copied().min_by(|i, j| at(*i).cmp(&at(*j)).then(i.cmp(j))) else {
            prop_assert!(assigned_to(&c, "x").is_none());
            prop_assert!(matches!(s.reason("x"), Some(WaitReason::Capacity { .. })), "{:?}", s.reason("x"));
            return Ok(());
        };
        let here = at(first);
        let mut wait: Option<(u64, usize, u64, i64)> = None;
        for (i, b) in fleet.iter().enumerate() {
            let Some(secs) = b.busy else { continue };
            let wait_ms = u64::try_from(i64::from(secs) * 1000 - t0).unwrap_or(0);
            let there = at(i);
            let Some(saves_ms) = here.checked_sub(there).filter(|s| *s >= 1000) else { continue };
            let saves_s = saves_ms / 1000;
            let bound = u64::from(share_ppm) * saves_s * 1000 / 1_000_000;
            if bound == 0 || wait_ms > bound || wait_ms + there >= here {
                continue;
            }
            if wait.is_none_or(|(f, ..)| wait_ms + there < f) {
                wait = Some((wait_ms + there, i, saves_s, t0 + i64::try_from(bound).unwrap_or(i64::MAX)));
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
        let mut share: BTreeMap<String, u32> = BTreeMap::new();
        for (j, (at, cold, patience, took)) in legs.iter().enumerate() {
            let id = format!("x{j}");
            sim.leg(*at, warm_leg(&id, *cold, Some(*patience)), 60_000);
            for (i, d) in took.iter().enumerate() {
                sim.durations_on.insert((id.clone(), format!("r{i}")), *d);
            }
            share.insert(id, crate::sched::units::ppm_of(*patience));
        }
        sim.run(10_000 * MIN);
        prop_assert_eq!(sim.settled.len(), legs.len(), "every leg ran: {:?}", sim.settled);
        for (t0, l, r) in &sim.waits {
            let WaitReason::Warm { saves_s, until, .. } = r else { continue };
            let p = share.get(l).copied().unwrap_or(0);
            prop_assert!(*saves_s >= 1);
            prop_assert!(until - t0 <= i64::try_from(u64::from(p) * *saves_s * 1000 / 1_000_000).unwrap_or(i64::MAX), "{} waited {} ms for {} s saved at {} millionths", l, until - t0, saves_s, p);
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
                let mut l = LegIn { cost: cost.and_then(crate::sched::units::ms_of_secs), ..leg(&format!("l{j}"), &format!("acme/r{repo}"), *priority) };
                l.route.job = format!("job{}", j % 3);
                l.route.workflow = "ci".into();
                if terms {
                    l.warm_lane = Some(format!("lane{}", j % 3));
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
}

/// W05-R33 (docs/needs.md E2.1): an item's share of each dimension of the fleet, in millionths
/// rounded up, and what running charges its flow.
#[test]
fn e2_1_shares_and_charges() {
    use super::state::{charges, offers, shares};
    let gib = 1u64 << 30;
    let sized = |slots: u32, cpus: u64, mem: u64, gpu: u32| RunnerIn { capacity: Some(Resources { cpu: Cpus::whole(cpus), mem: mem * gib, gpu, ..Resources::default() }), ..linux("r", slots) };
    let hour = 3_600_000;
    // A fleet of 64 CPUs and 128 GiB, in resource mode; in slots mode only its slots count.
    let fleet = offers(&sized(16, 64, 128, 0), true);
    assert_eq!(fleet, [16, 64_000, 128 * gib, 0]);
    assert_eq!(offers(&sized(16, 64, 128, 0), false), [16, 0, 0, 0]);
    // 32 CPUs for an hour cost 32 times one CPU for an hour.
    let (one, big) = (shares(&[1, 1_000, 0, 0], &fleet), shares(&[1, 32_000, 0, 0], &fleet));
    assert_eq!(one, [62_500, 15_625, 0, 0]);
    assert_eq!(big, [62_500, 500_000, 0, 0]);
    let ([_, one_cpu, _, _], [_, big_cpu, _, _]) = (charges(&one, hour, 1000), charges(&big, hour, 1000));
    assert_eq!(big_cpu, 32 * one_cpu);
    // A memory-heavy test costs by its memory.
    assert_eq!(shares(&[1, 1_000, 32 * gib, 0], &fleet), [62_500, 15_625, 250_000, 0]);
    // A GPU, of eight.
    assert_eq!(shares(&[1, 0, 0, 1], &offers(&sized(8, 32, 64, 8), true)), [125_000, 0, 0, 125_000]);
    // A slot of 20 on a fleet in slots mode; of three, rounded up.
    assert_eq!(shares(&[1, 0, 0, 0], &offers(&linux("s", 20), true)), [50_000, 0, 0, 0]);
    assert_eq!(shares(&[1, 0, 0, 0], &[3, 0, 0, 0]), [333_334, 0, 0, 0]);
    // An exclusive leg holding its whole runner, the fleet's only one: all of it.
    assert_eq!(shares(&fleet, &fleet), [1_000_000, 1_000_000, 1_000_000, 0]);
    // Asking a dimension the fleet lacks costs nothing there.
    assert_eq!(shares(&[1, 0, 0, 1], &fleet), [62_500, 0, 0, 0]);
    // The charge: share × milliseconds × 1000 / weight (thousandths).
    assert_eq!(charges(&[50_000, 0, 0, 0], 60_000, 1000), [3_000_000_000, 0, 0, 0]);
    assert_eq!(charges(&[50_000, 0, 0, 0], 60_000, 3000), [1_000_000_000, 0, 0, 0]);
}

/// W05-R33 (docs/needs.md E2.1, H-DRF's case): a repository whose GPU legs hold every GPU, which no
/// one else asks for, still gets its share of the runners without GPUs for its other legs: a runner
/// picking goes by the dimensions it offers, so GPU time doesn't count there.
#[test]
fn e2_1_a_repository_holding_every_gpu_keeps_its_share_of_the_cpu_runners() {
    let gib = 1u64 << 30;
    let mut sim = Sim::new(Config { resources: true, ..Config::default() });
    let sized = |name: &str, label: &str, slots: u32, mem: u64, gpu: u32| {
        let base = linux(name, slots);
        RunnerIn { view: RunnerView { labels: vec![label.into()], ..base.view.clone() }, capacity: Some(Resources { cpu: Cpus::whole(8), mem: mem * gib, gpu, ..Resources::default() }), ..base }
    };
    for n in 0..4 {
        sim.runner(0, sized(&format!("c{n}"), "cpu", 8, 32, 0));
    }
    for n in 0..2 {
        sim.runner(0, sized(&format!("g{n}"), "gpu", 32, 64, 4));
    }
    let task = |id: String, repo: &str, label: &str, request: Resources| LegIn {
        run: format!("run-{id}"),
        id,
        repo: repo.into(),
        route: LegRoute { labels: vec![label.into()], ..LegRoute::default() },
        priority: 60,
        request,
        cost: Some(60_000),
        ..LegIn::default()
    };
    let (cpu, gpu) = (Resources { cpu: Cpus::whole(1), mem: 2 * gib, ..Resources::default() }, Resources { gpu: 1, ..Resources::default() });
    for i in 0..600 {
        sim.leg(1, task(format!("gg{i}"), "acme/gpu", "gpu", gpu), MIN);
        sim.leg(1, task(format!("gc{i}"), "acme/gpu", "cpu", cpu), MIN);
        sim.leg(1, task(format!("cc{i}"), "acme/cpu", "cpu", cpu), MIN);
    }
    sim.run(60 * MIN);
    let repo = |leg: &str| if leg.starts_with("cc") { "acme/cpu".to_owned() } else { "acme/gpu".to_owned() };
    // While both have CPU legs waiting (32 CPU slots, 1,200 one-minute legs: 37 minutes).
    for w in [0, 10, 20] {
        let share = sim.dominant_by(w * MIN, (w + 10) * MIN, repo, |r| r.starts_with('c')).get("acme/gpu").copied().unwrap_or(0.0);
        assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: the GPU repository got {share:.2} of the CPU runners", w + 10);
    }
    // Its GPU legs held every GPU meanwhile.
    let gpus = sim.busy_on.iter().filter(|(l, r, s, e)| l.starts_with("gg") && r.starts_with('g') && *s < 30 * MIN && *e > 0).count();
    assert!(gpus >= 8 * 29, "only {gpus} GPU legs ran in the first 30 minutes");
}

/// W05-R32 (docs/needs.md E2.2): an actor that comes to a saturated repository gets its equal part
/// of it from its first window: tags taken at dispatch give it no credit, and no debt, from before
/// it came.
#[test]
fn e2_2_a_late_actor_gets_its_part_at_once() {
    use super::state::FairBy;
    let mut sim = Sim::new(Config { fair_by: vec![FairBy::Repo, FairBy::Actor], ..Config::default() });
    for r in 0..4 {
        sim.runner(0, linux(&format!("r{r}"), 5));
    }
    let by = |id: String, actor: &str| LegIn { actor: actor.into(), ..leg(&id, "acme/x", 60) };
    for i in 0..1_500 {
        sim.leg(1, by(format!("a{i}"), "alice"), MIN);
    }
    for i in 0..600 {
        sim.leg(30 * MIN, by(format!("b{i}"), "bob"), MIN);
    }
    sim.run(150 * MIN);
    let who = |l: &str| if l.starts_with('b') { "bob".to_owned() } else { "alice".to_owned() };
    assert!(sim.shares_by(20 * MIN, 30 * MIN, who).get("alice").is_some_and(|s| *s > 0.99), "alice had it all before bob came");
    for w in [30, 40, 50] {
        let share = sim.shares_by(w * MIN, (w + 10) * MIN, who).get("bob").copied().unwrap_or(0.0);
        assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: bob got {share:.2} of the repository", w + 10);
    }
}

/// W05-R32, R33 (docs/needs.md E2.2, H-DRF's example inside the tenant tree): a repository whose
/// GPU lane holds every GPU keeps its CPU lane's share of the runners without GPUs, against a
/// repository with only CPU work.
#[test]
fn e2_2_a_gpu_lane_doesnt_cost_its_repositorys_cpu_lane() {
    use super::state::FairBy;
    let gib = 1u64 << 30;
    let mut sim = Sim::new(Config { resources: true, fair_by: vec![FairBy::Repo, FairBy::Lane], ..Config::default() });
    let sized = |name: &str, label: &str, slots: u32, mem: u64, gpu: u32| {
        let base = linux(name, slots);
        RunnerIn { view: RunnerView { labels: vec![label.into()], ..base.view.clone() }, capacity: Some(Resources { cpu: Cpus::whole(8), mem: mem * gib, gpu, ..Resources::default() }), ..base }
    };
    for n in 0..4 {
        sim.runner(0, sized(&format!("c{n}"), "cpu", 8, 32, 0));
    }
    for n in 0..2 {
        sim.runner(0, sized(&format!("g{n}"), "gpu", 32, 64, 4));
    }
    let task = |id: String, repo: &str, lane: &str, label: &str, request: Resources| LegIn {
        run: format!("run-{id}"),
        id,
        repo: repo.into(),
        lane: lane.into(),
        route: LegRoute { labels: vec![label.into()], ..LegRoute::default() },
        priority: 60,
        request,
        cost: Some(60_000),
        ..LegIn::default()
    };
    let (cpu, gpu) = (Resources { cpu: Cpus::whole(1), mem: 2 * gib, ..Resources::default() }, Resources { gpu: 1, ..Resources::default() });
    for i in 0..600 {
        sim.leg(1, task(format!("gg{i}"), "acme/gpu", "train", "gpu", gpu), MIN);
        sim.leg(1, task(format!("gc{i}"), "acme/gpu", "test", "cpu", cpu), MIN);
        sim.leg(1, task(format!("cc{i}"), "acme/cpu", "test", "cpu", cpu), MIN);
    }
    sim.run(60 * MIN);
    let repo = |leg: &str| if leg.starts_with("cc") { "acme/cpu".to_owned() } else { "acme/gpu".to_owned() };
    for w in [0, 10, 20] {
        let share = sim.dominant_by(w * MIN, (w + 10) * MIN, repo, |r| r.starts_with('c')).get("acme/gpu").copied().unwrap_or(0.0);
        assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: the GPU repository's CPU lane got {share:.2} of the CPU runners", w + 10);
    }
}

/// W05-R10, R32 (docs/needs.md E2.2): a tenant whose legs run three times their estimate still
/// gets its share, each charge settled to the time it ran (as DRFQ needs no time beforehand).
#[test]
fn e2_2_a_tenant_running_past_its_estimates_keeps_its_share() {
    let mut sim = Sim::new(Config::default());
    for r in 0..4 {
        sim.runner(0, linux(&format!("r{r}"), 5));
    }
    for i in 0..1_000 {
        sim.leg(1, LegIn { cost: Some(60_000), ..leg(&format!("a{i}"), "a/a", 60) }, 3 * MIN);
        sim.leg(1, LegIn { cost: Some(60_000), ..leg(&format!("b{i}"), "b/b", 60) }, MIN);
    }
    sim.run(100 * MIN);
    for w in (0..60).step_by(10) {
        let share = sim.shares(w * MIN, (w + 10) * MIN).get("a/a").copied().unwrap_or(0.0);
        assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: the tenant running past its estimates got {share:.2}", w + 10);
    }
}

// ---------------------------------------------------------------------- classes, floors and promotion (W05-R30, R31)

fn classed(id: &str, repo: &str, class: super::state::Class) -> LegIn {
    LegIn { class, ..leg(id, repo, 60) }
}

/// The class of a test's leg, by its id's first word.
fn class_of(leg: &str) -> String {
    leg.split(|c: char| c.is_ascii_digit() || c == '-').next().unwrap_or_default().to_owned()
}

/// W05-R31, docs/needs.md E2.3: under a saturating stream of land legs, batch keeps its 15 % floor
/// of 20 slots in every 10-minute window while it has work, its 15 % floor less two points for a
/// leg's granularity, and no more once it's there: nothing is preempted, it takes room as land
/// legs end.
#[test]
fn w05_r31_batch_keeps_its_floor_under_a_land_stream() {
    use super::state::Class;
    let mut sim = Sim::new(Config::default());
    for r in 0..4 {
        sim.runner(0, linux(&format!("r{r}"), 5));
    }
    for i in 0..400 {
        sim.leg(0, classed(&format!("batch{i}"), "acme/y", Class::Batch), MIN);
    }
    for m in 0..70 {
        for k in 0..25 {
            sim.leg(m * MIN + 1, classed(&format!("land{m}-{k}"), "acme/x", Class::Land), MIN);
        }
    }
    sim.run(70 * MIN);
    for w in (0..60).step_by(10) {
        let share = sim.shares_by(w * MIN, (w + 10) * MIN, class_of).get("batch").copied().unwrap_or(0.0);
        assert!(share >= 0.13, "window {w}–{}: batch got {share:.3}", w + 10);
        if w > 0 {
            assert!(share <= 0.17, "window {w}–{}: batch got {share:.3}, past its floor", w + 10);
        }
    }
}

/// W05-R31: a floor smaller than a leg holds over time: on one slot under a saturating land stream,
/// batch's 15 % is the slot 15 % of the time, not every time it frees.
#[test]
fn w05_r31_a_floor_less_than_a_leg_holds_over_time() {
    use super::state::Class;
    let mut sim = Sim::new(Config::default());
    sim.runner(0, linux("a", 1));
    for m in 0..70 {
        sim.leg(m * MIN, classed(&format!("land{m}"), "acme/x", Class::Land), MIN);
    }
    for i in 0..100 {
        sim.leg(1, classed(&format!("batch{i}"), "acme/x", Class::Batch), MIN);
    }
    sim.run(70 * MIN);
    for w in (0..60).step_by(10) {
        let share = sim.shares_by(w * MIN, (w + 10) * MIN, class_of).get("batch").copied().unwrap_or(0.0);
        assert!((0.09..=0.21).contains(&share), "window {w}–{}: batch got {share:.3}", w + 10);
    }
    let hour = sim.shares_by(0, 60 * MIN, class_of).get("batch").copied().unwrap_or(0.0);
    assert!((0.12..=0.17).contains(&hour), "over the hour batch got {hour:.3}");
}

/// W05-R30, R31, docs/needs.md E2.3: interactive and batch work together, batch at its floor:
/// interactive legs go first every time.
#[test]
fn w05_r31_interactive_goes_first_while_batch_holds_its_floor() {
    use super::state::Class;
    let mut sim = Sim::new(Config::default());
    for r in 0..4 {
        sim.runner(0, linux(&format!("r{r}"), 5));
    }
    for i in 0..3 {
        sim.leg(0, classed(&format!("long{i}"), "acme/x", Class::Batch), 120 * MIN);
    }
    for i in 0..17 {
        sim.leg(0, classed(&format!("busy{i}"), "acme/x", Class::Interactive), MIN);
    }
    for i in 0..40 {
        sim.leg(1, classed(&format!("interactive{i}"), "acme/x", Class::Interactive), MIN);
        sim.leg(1, classed(&format!("batch{i}"), "acme/y", Class::Batch), MIN);
    }
    sim.run(10 * MIN);
    let starts = |class: &str| sim.assigned.iter().filter(|a| class_of(&a.leg) == class).map(|a| a.at).collect::<Vec<_>>();
    let (interactive, batch) = (starts("interactive"), starts("batch"));
    assert_eq!(interactive.len(), 40);
    assert!(interactive.iter().max() <= batch.iter().min(), "a batch leg started before an interactive one");
}

/// W05-AC15, docs/needs.md E2.3: a background leg under a steady stream of one-minute batch legs on
/// 20 slots starts when a runner next frees, through its 5 % floor; with every floor at zero, at
/// its 60-minute bound, promoted to batch, and a leg's time. Its standing names its class and the
/// one it was promoted from.
#[test]
fn w05_ac15_a_background_leg_under_a_batch_stream_starts_within_its_bound() {
    use super::state::{Class, ClassConfig, default_classes};
    for floors in [true, false] {
        let classes = if floors { default_classes() } else { default_classes().into_iter().map(|(c, cc)| (c, ClassConfig { floor: 0, ..cc })).collect() };
        let mut sim = Sim::new(Config { classes, ..Config::default() });
        for r in 0..4 {
            sim.runner(0, linux(&format!("r{r}"), 5));
        }
        for i in 0..20 {
            sim.leg(0, classed(&format!("batch0-{i}"), "acme/x", Class::Batch), MIN);
        }
        sim.leg(1, classed("bg", "acme/x", Class::Background), MIN);
        for m in 1..90 {
            for i in 0..20 {
                sim.leg(m * MIN - 1, classed(&format!("batch{m}-{i}"), "acme/x", Class::Batch), MIN);
            }
        }
        sim.run(60 * MIN + MIN / 2);
        if !floors {
            let standing = sim.sched.standing("bg").map(|s| (s.class, s.promoted_from, s.tenant));
            assert_eq!(standing, Some((Class::Batch, Some(Class::Background), vec!["acme/x".to_owned()])), "promoted at its bound");
        }
        sim.run(90 * MIN);
        let waited = sim.started("bg").unwrap_or(i64::MAX) - 1;
        if floors {
            assert!(waited <= 2 * MIN, "with floors it waited {} s", waited / 1000);
        } else {
            assert!((60 * MIN..=61 * MIN).contains(&waited), "without floors it waited {} s", waited / 1000);
        }
    }
}

/// W05-R31: a class is promoted where it's passed over: batch legs starting all the while on CPU
/// runners don't keep a batch leg waiting behind interactive GPU work from going up at its bound,
/// where it takes the GPU runner at its tenant's turn.
#[test]
fn w05_r31_a_class_served_at_one_signature_is_still_promoted_at_another() {
    use super::state::Class;
    let labelled = |name: &str, label: &str, slots: u32| {
        let base = linux(name, slots);
        RunnerIn { view: RunnerView { labels: vec![label.into()], ..base.view.clone() }, ..base }
    };
    let on = |l: LegIn, label: &str| LegIn { route: LegRoute { labels: vec![label.into()], ..LegRoute::default() }, ..l };
    let mut sim = Sim::new(Config::default());
    sim.runner(0, labelled("c0", "cpu", 2));
    sim.runner(0, labelled("g0", "gpu", 1));
    for m in 0..60 {
        sim.leg((m * MIN - 1).max(0), on(classed(&format!("interactive{m}"), "acme/x", Class::Interactive), "gpu"), MIN);
        for i in 0..2 {
            sim.leg((m * MIN - 1).max(0), on(classed(&format!("batch{m}-{i}"), "acme/x", Class::Batch), "cpu"), MIN);
        }
    }
    sim.leg(1, on(classed("gpu-batch", "acme/y", Class::Batch), "gpu"), MIN);
    sim.run(60 * MIN);
    let start = sim.started("gpu-batch").unwrap_or(i64::MAX);
    assert!((30 * MIN..=31 * MIN).contains(&start), "the GPU batch leg started at {} s", start / 1000);
}

/// W05-R30, R31, docs/needs.md E2.3: the routing explanation's standing names a leg's class, its
/// tenant in fair share's tree and, once promoted, the class it was queued in.
#[test]
fn w05_r31_the_explanation_names_the_class_tenant_and_promotion() -> Result<(), String> {
    use super::state::{Class, ClassConfig, FairBy};
    let classes = [(Class::Background, ClassConfig { floor: 0, bound: Some(60_000) })].into_iter().collect();
    let mut s = Scheduler::new(Config { classes, fair_by: vec![FairBy::Repo, FairBy::Actor], ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { actor: "agent:A".into(), ..leg("busy", "acme/x", 60) })), 0);
    let queued = s.handle(Input::LegQueued(Box::new(LegIn { class: Class::Background, actor: "agent:A".into(), ..leg("bg", "acme/x", 20) })), 1);
    let json = |s: &Scheduler| serde_json::to_value(s.standing("bg")).map_err(|e| e.to_string());
    let before = json(&s)?;
    assert_eq!(before.get("class"), Some(&serde_json::json!("background")));
    assert_eq!(before.get("tenant"), Some(&serde_json::json!(["acme/x", "agent:A"])));
    assert_eq!(before.get("promoted_from"), None);
    // Passed over for its bound (a minute), it's promoted to batch, on the timer the engine asked
    // for as it was queued.
    assert!(queued.contains(&Command::Timer { at: MIN + 1 }), "{queued:?}");
    s.handle(Input::Tick, MIN + 1);
    let after = json(&s)?;
    assert_eq!(after.get("class"), Some(&serde_json::json!("batch")));
    assert_eq!(after.get("promoted_from"), Some(&serde_json::json!("background")));
    Ok(())
}

// ---------------------------------------------------------------------- G7, grown from W05-AC04 (W05-AC14)

/// The G7 scenario's legs (docs/needs.md E2.4): one CPU and `gib` GiB each, in `repo`, by `actor`,
/// expected to take `ms` (their history).
fn shaped(id: String, repo: &str, actor: &str, gib: u64, ms: i64) -> LegIn {
    LegIn {
        run: format!("run-{id}"),
        id,
        repo: repo.into(),
        actor: actor.into(),
        route: LegRoute { labels: vec!["k8s".into()], ..LegRoute::default() },
        priority: 60,
        request: Resources { cpu: Cpus::whole(1), mem: gib << 30, ..Resources::default() },
        cost: u64::try_from(ms).ok(),
        ..LegIn::default()
    }
}

/// 4 runners of 16 CPUs and 64 GiB in resource mode, their CPUs not overcommitted, so CPUs run out
/// first and a leg that ends frees room either tenant's next leg fits. Where memory runs out first
/// (CPUs overcommitted 1.5 times, or B weighted 3:1), A's 1-GiB legs refill the room B's 8-GiB legs
/// need as soon as it frees, and B holds a third rather than half: the limit E4's reservations
/// lift (docs/ci.md W05-AC14).
fn g7_fleet(cfg: Config) -> Sim {
    let mut sim = Sim::new(Config { resources: true, overcommit_cpu: 1000, ..cfg });
    for n in 0..4 {
        let base = linux(&format!("r{n}"), 16);
        sim.runner(0, RunnerIn { capacity: Some(Resources { cpu: Cpus::whole(16), mem: 64 << 30, ..Resources::default() }), ..base });
    }
    sim
}

/// W05-AC14 (G7, docs/needs.md §4, E2.4): under saturation each tenant gets its weighted share of
/// dominant-resource time within 10 points over every 10-minute window while both are backlogged,
/// whatever order the work came in and whatever its shape. Tenant A queues 1-CPU, 1-GiB,
/// one-minute legs from minute 0 (CPU-heavy); tenant B 1-CPU, 8-GiB, two-minute legs from minute
/// 20 (memory-heavy, DRF's case), each a batch every 10 minutes, more than it runs. From B's first
/// window each holds half, or A, weighted 3:1, three quarters.
#[test]
fn w05_ac14_g7_staggered_arrivals_and_mixed_shapes() {
    for (weight, want) in [(1000, 0.5), (3000, 0.75)] {
        let mut sim = g7_fleet(Config { weights: [("a/a".to_owned(), weight)].into_iter().collect(), ..Config::default() });
        for m in (0..70).step_by(10) {
            for i in 0..700 {
                sim.leg(m * MIN, shaped(format!("a{m}-{i}"), "a/a", "a", 1, MIN), MIN);
            }
            for i in 0..150 {
                if m >= 20 {
                    sim.leg(m * MIN, shaped(format!("b{m}-{i}"), "b/b", "b", 8, 2 * MIN), 2 * MIN);
                }
            }
        }
        sim.run(75 * MIN);
        let tenant = |l: &str| l.get(..1).unwrap_or_default().to_owned();
        for w in (20..70).step_by(10) {
            let share = sim.dominant_by(w * MIN, (w + 10) * MIN, tenant, |_| true).get("a").copied().unwrap_or(0.0);
            assert!((share - want).abs() <= 0.10, "A weighted {weight}, window {w}–{}: A held {share:.3} of the dominant-resource time", w + 10);
        }
        // Both were backlogged in every window: each had legs from an earlier batch still waiting
        // when the last one ended.
        for t in ["a", "b"] {
            assert!(sim.assigned.iter().any(|a| a.at >= 70 * MIN && tenant(&a.leg) == t && !a.leg.starts_with(&format!("{t}70-"))), "A weighted {weight}: {t} ran out of work");
        }
    }
}

/// W05-AC14 (G7, W28-R66): a repository whose two actors split its share, the second arriving 10
/// minutes in, against a repository with one actor: each repository holds half in every
/// 10-minute window, its actors a quarter each once both are there.
#[test]
fn w05_ac14_g7_a_repositorys_actors_split_its_share() {
    use super::state::FairBy;
    let mut sim = Sim::new(Config { fair_by: vec![FairBy::Repo, FairBy::Actor], ..Config::default() });
    for r in 0..4 {
        sim.runner(0, linux(&format!("r{r}"), 5));
    }
    for i in 0..1_000 {
        sim.leg(0, LegIn { actor: "x1".into(), ..leg(&format!("x1-{i}"), "acme/x", 60) }, MIN);
        sim.leg(10 * MIN, LegIn { actor: "x2".into(), ..leg(&format!("x2-{i}"), "acme/x", 60) }, MIN);
        sim.leg(0, LegIn { actor: "y".into(), ..leg(&format!("y-{i}"), "acme/y", 60) }, MIN);
    }
    sim.run(70 * MIN);
    let repo = |l: &str| l.get(..1).unwrap_or_default().to_owned();
    let actor = |l: &str| l.split('-').next().unwrap_or_default().to_owned();
    for w in (0..60).step_by(10) {
        let share = sim.shares_by(w * MIN, (w + 10) * MIN, repo).get("x").copied().unwrap_or(0.0);
        assert!((share - 0.5).abs() <= 0.10, "window {w}–{}: acme/x held {share:.3}", w + 10);
        if w >= 10 {
            let actors = sim.shares_by(w * MIN, (w + 10) * MIN, actor);
            for a in ["x1", "x2"] {
                let s = actors.get(a).copied().unwrap_or(0.0);
                assert!((s - 0.25).abs() <= 0.10, "window {w}–{}: {a} held {s:.3}", w + 10);
            }
        }
    }
}

/// W05-R34: a grant lives a lease unless its runner renews it. Renewed at 10 s and 20 s and then
/// not, it's lost at 80 s, not a millisecond before, and the engine asked for a timer at its
/// expiry. A finish leg's retry is queued then, back on the only runner; a stop leg's waits the
/// lock-delay past the expiry, until 95 s.
#[test]
fn w05_r34_a_grant_not_renewed_is_lost_at_its_expiry() {
    use super::state::{LeaseLoss, Loss};
    for (loss, retry_at) in [(LeaseLoss::Finish, 80_000), (LeaseLoss::Stop, 95_000)] {
        let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 7);
        s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
        let c = s.handle(Input::LegQueued(Box::new(LegIn { on_lease_loss: loss, ..leg("x", "acme/x", 60) })), 0);
        let first = Fence { epoch: 7, n: 1 };
        assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, fence, expires: Some(60_000), lease_loss, .. } if leg == "x" && *fence == first && *lease_loss == loss)), "{c:?}");
        assert!(c.contains(&Command::Timer { at: 60_000 }), "a timer at the expiry: {c:?}");
        for t in [10_000, 20_000] {
            s.handle(Input::Renewed { runner: "a".into(), held: vec![("x".into(), first)] }, t);
        }
        assert_eq!(s.grants().get("x").and_then(|g| g.3), Some(80_000));
        let c = s.handle(Input::Tick, 79_999);
        assert!(!c.iter().any(|x| matches!(x, Command::Lost { .. })) && s.grants().contains_key("x"), "live a millisecond before: {c:?}");
        let c = s.handle(Input::Tick, 80_000);
        assert!(c.iter().any(|x| matches!(x, Command::Lost { leg, attempt: 1, runner, fence, why: Loss::Expired } if leg == "x" && runner == "a" && *fence == first)), "{c:?}");
        assert!(c.iter().any(|x| matches!(x, Command::Retry { leg, attempt: 2, at } if leg == "x" && *at == retry_at)), "{c:?}");
        let again = |c: &[Command]| c.iter().any(|x| matches!(x, Command::Assign { leg, runner, attempt: 2, fence, .. } if leg == "x" && runner == "a" && *fence == Fence { epoch: 7, n: 2 }));
        if loss == LeaseLoss::Stop {
            assert!(!again(&c) && c.contains(&Command::Timer { at: 95_000 }), "{c:?}");
            assert!(!again(&s.handle(Input::Tick, 94_999)));
            let c = s.handle(Input::Tick, 95_000);
            assert!(again(&c), "{c:?}");
        } else {
            assert!(again(&c), "{c:?}");
        }
        assert_eq!(s.check_grants(), Ok(()));
    }
}

/// W05-R34: a renewal names the grants it renews by their fences. One for attempt 1's grant, come
/// after the leg was lost and granted again on the same runner, renews nothing; nor does one
/// naming attempt 2's grant from another runner; attempt 2's own does.
#[test]
fn w05_r34_a_renewal_names_its_grant() {
    let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 3);
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    s.handle(Input::Tick, 60_000);
    let (first, second) = (Fence { epoch: 3, n: 1 }, Fence { epoch: 3, n: 2 });
    assert_eq!(s.grants().get("x"), Some(&("a".to_owned(), 2, second, Some(120_000))), "lost, and granted again on the only runner");
    s.handle(Input::Renewed { runner: "a".into(), held: vec![("x".into(), first)] }, 100_000);
    s.handle(Input::Renewed { runner: "b".into(), held: vec![("x".into(), second)] }, 100_000);
    assert_eq!(s.grants().get("x").and_then(|g| g.3), Some(120_000), "neither renews it");
    s.handle(Input::Renewed { runner: "a".into(), held: vec![("x".into(), second)] }, 100_000);
    assert_eq!(s.grants().get("x").and_then(|g| g.3), Some(160_000));
}

/// W05-R34: a refusal or an end about a grant that isn't the leg's now changes nothing. Attempt 1
/// lost and attempt 2 on the same runner, attempt 1's refusal and end come late. And an attempt
/// refused by one runner, then another, and granted again to the first: that first refusal, come
/// again, refuses nothing (the attempt is the same, its fence isn't).
#[test]
fn w05_r34_a_late_answer_about_an_old_grant_changes_nothing() {
    let mut s = Scheduler::new(Config { lease: Some(60_000), ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    s.handle(Input::Tick, 60_000);
    let now = s.grants();
    assert_eq!(now.get("x").map(|g| (g.1, g.2)), Some((2, Fence { epoch: 0, n: 2 })));
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: Fence { epoch: 0, n: 1 } }, 61_000);
    let c2 = s.handle(Input::AttemptFinished { leg: "x".into(), attempt: 1, outcome: Outcome::Success }, 62_000);
    assert!(c.is_empty() && c2.is_empty() && s.grants() == now, "{c:?} {c2:?}");
    // Refused by a, then b, and back to a: a's first refusal again refuses nothing.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let to = |s: &Scheduler| s.grants().get("x").map(|g| (g.0.clone(), g.2.n));
    assert_eq!(to(&s), Some(("a".into(), 1)));
    s.handle(Input::Rejected { leg: "x".into(), fence: Fence { epoch: 0, n: 1 } }, 1);
    assert_eq!(to(&s), Some(("b".into(), 2)));
    s.handle(Input::Rejected { leg: "x".into(), fence: Fence { epoch: 0, n: 2 } }, 2);
    assert_eq!(to(&s), Some(("a".into(), 3)));
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: Fence { epoch: 0, n: 1 } }, 3);
    assert!(c.is_empty() && to(&s) == Some(("a".into(), 3)), "{c:?}");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R34, W05-R25: after a restart, a leg its runner still runs keeps its grant's fence and gets
/// a lease from then; its runner renews it by that fence, and the engine's own grants are of its
/// own epoch. Not renewed again, it's lost at its lease's end.
#[test]
fn w05_r34_an_adopted_grant_keeps_its_fence_and_gets_a_lease() {
    use super::state::Loss;
    let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 9);
    let old = Fence { epoch: 8, n: 41 };
    s.handle(Input::LegRunning { leg: Box::new(leg("x", "acme/x", 60)), runner: "a".into(), attempt: 1, fence: old }, 1_000);
    assert_eq!(s.grants().get("x"), Some(&("a".to_owned(), 1, old, Some(61_000))));
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 2_000);
    s.handle(Input::Renewed { runner: "a".into(), held: vec![("x".into(), old)] }, 30_000);
    assert_eq!(s.grants().get("x").and_then(|g| g.3), Some(90_000));
    let c = s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 31_000);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, fence, .. } if leg == "y" && *fence == Fence { epoch: 9, n: 1 })), "{c:?}");
    assert_eq!(s.check_grants(), Ok(()));
    let c = s.handle(Input::Tick, 90_000);
    assert!(c.iter().any(|x| matches!(x, Command::Lost { leg, fence, why: Loss::Expired, .. } if leg == "x" && *fence == old)), "{c:?}");
}

/// W05-R35, the engine's half: a draining runner gets no new grant while its grants run to their
/// end, and gets work again once undrained; a runner lost loses its grants, each told (`Lost`), and
/// one lost holding nothing leaves the fleet too, getting nothing until it's back.
#[test]
fn w05_r35_draining_and_lost_runners() {
    use super::state::Loss;
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("a", 3))), 0);
    for id in ["p", "q"] {
        s.handle(Input::LegQueued(Box::new(leg(id, "acme/x", 60))), 0);
    }
    s.handle(Input::RunnerDraining { runner: "a".into() }, 1);
    let c = s.handle(Input::LegQueued(Box::new(leg("r", "acme/x", 60))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "no third while it drains: {c:?}");
    for (id, t) in [("p", 3), ("q", 4)] {
        let c = s.handle(Input::AttemptFinished { leg: id.into(), attempt: 1, outcome: Outcome::Success }, t);
        assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, outcome: Outcome::Success, .. } if leg == id)), "its grants run to their end: {c:?}");
        assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    }
    let c = s.handle(Input::RunnerChanged(Box::new(linux("a", 3))), 5);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if leg == "r")), "undrained, it takes work: {c:?}");
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 6);
    assert!(c.iter().any(|x| matches!(x, Command::Lost { leg, attempt: 1, runner, why: Loss::RunnerLost, .. } if leg == "r" && runner == "a")), "{c:?}");
    // Lost holding nothing.
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 0);
    s.handle(Input::RunnerLost { runner: "b".into() }, 1);
    let c = s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    let c = s.handle(Input::RunnerUp(Box::new(linux("b", 1))), 3);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "y" && runner == "b")), "{c:?}");
}

/// W05-R35: a runner away or draining that refuses a leg (an assignment that couldn't reach it)
/// doesn't keep the leg off it: undrained, it takes the leg back, where a refusal from a runner at
/// work keeps the leg off it for another.
#[test]
fn w05_r35_a_draining_runners_refusal_doesnt_keep_the_leg_off_it() {
    let mut s = Scheduler::new(Config { lease: Some(60_000), ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let first = fence(&s, "x");
    s.handle(Input::RunnerDraining { runner: "a".into() }, 1);
    let c = s.handle(Input::Rejected { leg: "x".into(), fence: first }, 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "it takes nothing new while away: {c:?}");
    assert!(!matches!(s.reason("x"), Some(WaitReason::OtherRunner { .. })), "{:?}", s.reason("x"));
    let c = s.handle(Input::RunnerChanged(Box::new(linux("a", 1))), 3);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, attempt: 1, .. } if leg == "x" && runner == "a")), "back, it takes the leg again: {c:?}");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R05, G10: a grant the server couldn't write before its Assign goes back to its queue, the
/// same attempt, after REFUSED_AGAIN_MS, kept off no runner: the only runner there is takes it
/// again under a new grant. A late word about the grant that wasn't changes nothing.
#[test]
fn w05_r05_a_grant_not_written_goes_back_to_its_queue() {
    let mut s = Scheduler::new(Config { lease: Some(60_000), ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 1))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let first = fence(&s, "x");
    let c = s.handle(Input::Ungranted { leg: "x".into(), fence: first }, 1);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "not at once: {c:?}");
    assert!(c.iter().any(|x| matches!(x, Command::Timer { at } if *at == 1 + super::state::REFUSED_AGAIN_MS)), "{c:?}");
    assert_eq!(s.check_grants(), Ok(()));
    let c = s.handle(Input::Tick, super::state::REFUSED_AGAIN_MS);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "nor before its time: {c:?}");
    let c = s.handle(Input::Tick, 1 + super::state::REFUSED_AGAIN_MS);
    let again = c.iter().find_map(|x| match x {
        Command::Assign { leg, runner, attempt: 1, fence, .. } if leg == "x" && runner == "a" => Some(*fence),
        _ => None,
    });
    assert!(again.is_some_and(|f| f != first), "the same attempt, to the same runner, under a new grant: {c:?}");
    let c = s.handle(Input::Ungranted { leg: "x".into(), fence: first }, 2 + super::state::REFUSED_AGAIN_MS);
    assert!(c.is_empty() && again == Some(fence(&s, "x")), "a late word about the grant that wasn't changes nothing: {c:?}");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R35: a runner's drain comes with what it is: a `RunnerChanged` (a probe's report) or a
/// `RunnerUp` (its hello) that says it draining keeps it so, with no grant made in between; one
/// that says it isn't is its undrain. Draining, what it runs runs on.
#[test]
fn w05_r35_a_runner_said_draining_stays_drained_through_its_changes() {
    let mut s = Scheduler::new(Config { lease: Some(60_000), ..Config::default() });
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let held = fence(&s, "x");
    let drained = || RunnerIn { draining: true, ..linux("a", 2) };
    s.handle(Input::RunnerChanged(Box::new(drained())), 1);
    let c = s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 2);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "drained, nothing new: {c:?}");
    let c = s.handle(Input::RunnerUp(Box::new(drained())), 3);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "back, still drained: {c:?}");
    assert_eq!(fence(&s, "x"), held, "x runs on under its grant");
    let c = s.handle(Input::RunnerChanged(Box::new(linux("a", 2))), 4);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, runner, .. } if leg == "y" && runner == "a")), "undrained, it takes y: {c:?}");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R25: an engine made in epoch 7 grants under it; taking epoch 8 (its node leading again),
/// the next grant is 8's first, greater than any of 7's, while a grant made under 7 keeps its
/// fence and is renewed by it. An older epoch than its own is never taken.
#[test]
fn w05_r25_a_new_epoch_numbers_grants_afresh_above_the_old() {
    let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 7);
    s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 0);
    s.handle(Input::LegQueued(Box::new(leg("x", "acme/x", 60))), 0);
    let old = fence(&s, "x");
    assert_eq!(old.epoch, 7);
    s.new_epoch(8);
    s.new_epoch(6);
    assert_eq!(s.epoch(), 8);
    s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 1);
    assert_eq!(fence(&s, "y"), Fence { epoch: 8, n: 1 });
    assert!(fence(&s, "y") > old && fence(&s, "x") == old, "x keeps 7's fence");
    s.handle(Input::Renewed { runner: "a".into(), held: vec![("x".into(), old)] }, 50_000);
    let c = s.handle(Input::Tick, 61_000);
    assert!(!c.iter().any(|x| matches!(x, Command::Lost { leg, .. } if leg == "x")), "renewed by its old fence: {c:?}");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R03, docs/sim.md's bug 12: a share held on a runner whose size isn't known holds it whole,
/// as the engine claims one there, so nothing has room beside it, whoever asks (a runner's own
/// admission, the v1 dispatcher). Before, an ordinary leg had every slot beside it: found by
/// kiln-sim's scheduler model, a share running on there after its grant was lost and the runner
/// given another leg.
#[test]
fn w05_r03_a_share_on_a_runner_of_no_known_size_holds_it_whole() {
    let share = Resources { cpu: Cpus::whole(4), mem: 2 << 30, ..Resources::default() };
    let none = Resources::default();
    assert!(room(4, None, &none, false, 0, 0, Incoming::Share(share)), "alone");
    assert!(!room(4, None, &none, false, 1, 1, Incoming::Share(share)), "not beside another");
    assert!(!room(4, None, &share, false, 0, 1, Incoming::Ordinary), "nothing beside it");
    assert!(room(4, None, &none, false, 3, 3, Incoming::Ordinary), "without one, slots as ever");
    assert!(room(4, Some(Resources { cpu: Cpus::whole(8), mem: 16 << 30, ..Resources::default() }), &share, false, 1, 2, Incoming::Ordinary), "a known size keeps the rest");
}

/// W05-R34, R25, docs/sim.md's bug 13: a stop item the store has queued at its second attempt
/// after a restart (its first lost with its runner, cut off, which may still hold it until its lease
/// runs out by its own clock) waits out a whole lease and the lock-delay before it's granted again.
/// A finish item at its second attempt, or a stop item at its first, goes at once.
#[test]
fn w05_r34_a_stop_item_queued_again_after_a_restart_waits_out_a_lease() {
    use super::state::LeaseLoss;
    let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), lock_delay: 15_000, ..Config::default() }, 2);
    s.handle(Input::RunnerUp(Box::new(linux("a", 4))), 0);
    let mut stop = leg("stop", "acme/x", 60);
    (stop.attempt, stop.on_lease_loss) = (2, LeaseLoss::Stop);
    let c = s.handle(Input::LegQueued(Box::new(stop)), 1_000);
    assert!(!c.iter().any(|x| matches!(x, Command::Assign { .. })), "{c:?}");
    assert_eq!(s.reason("stop"), Some(&WaitReason::Retry { at: 76_000 }));
    let mut first = leg("first", "acme/x", 60);
    first.on_lease_loss = LeaseLoss::Stop;
    let mut finish = leg("finish", "acme/x", 60);
    finish.attempt = 2;
    for l in [first, finish] {
        let id = l.id.clone();
        let c = s.handle(Input::LegQueued(Box::new(l)), 1_000);
        assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, .. } if *leg == id)), "{id} at once: {c:?}");
    }
    assert!(!s.handle(Input::Tick, 75_999).iter().any(|x| matches!(x, Command::Assign { .. })));
    let c = s.handle(Input::Tick, 76_000);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, attempt: 2, .. } if leg == "stop")), "{c:?}");
}

/// W05-R25, found reading the dispatcher's rebuild for kiln-sim's model (docs/needs.md E3.8): a leg
/// the store has queued at its second attempt (its first lost with its runner) is queued again
/// after a restart at that attempt, and granted as it: never as its first again, which ended. Its
/// next retry is its third, and its policy counts from there.
#[test]
fn w05_r25_a_leg_queued_after_a_restart_keeps_its_attempt() {
    let mut s = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 2);
    let mut l = leg("x", "acme/x", 60);
    l.attempt = 2;
    l.retry = Some(RetryPolicy { max: 2, on: vec![super::retry::RetryOn::Class(FailureClass::RunnerLost)], initial_ms: 0, factor: 1000, max_ms: 0, runner: super::retry::RunnerChoice::Any });
    s.handle(Input::LegQueued(Box::new(l)), 0);
    assert_eq!(s.grant("x"), Some((2, None)));
    let c = s.handle(Input::RunnerUp(Box::new(linux("a", 2))), 1);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, attempt: 2, .. } if leg == "x")), "granted as its second attempt: {c:?}");
    assert_eq!(s.grant("x"), Some((2, Some(("a", Fence { epoch: 2, n: 1 })))));
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 2);
    assert!(c.iter().any(|x| matches!(x, Command::Retry { leg, attempt: 3, .. } if leg == "x")), "its third next: {c:?}");
    let c = s.handle(Input::RunnerUp(Box::new(linux("b", 2))), 3);
    assert!(c.iter().any(|x| matches!(x, Command::Assign { leg, attempt: 3, .. } if leg == "x")), "{c:?}");
    // Its policy allows two retries, the first and second attempts' failures: the third's is its last.
    let c = s.handle(Input::RunnerLost { runner: "b".into() }, 4);
    assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, .. } if leg == "x")), "{c:?}");
    // A new leg says nothing of its attempt: its first.
    s.handle(Input::LegQueued(Box::new(leg("y", "acme/x", 60))), 5);
    assert_eq!(s.grant("y").map(|g| g.0), Some(1));
}

/// W05-R38, docs/needs.md E4.1: a grant's expected end is its start plus its p90, moved on once
/// it has passed to its start plus twice the time it has run, never past its hard end, its start
/// plus its timeout. Its grant over, it has neither.
#[test]
fn w05_r38_a_grant_s_ends_follow_its_p90_and_its_timeout() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("r", 1))), 0);
    let at = |h: i64, m: i64| (h * 60 + m) * MIN;
    let l = LegIn { p90: Some(4 * 60_000), timeout: Some(30 * 60_000), ..leg("a", "acme/x", 60) };
    s.handle(Input::LegQueued(Box::new(l)), at(10, 0));
    let looked: Vec<(i64, i64)> = [at(10, 3), at(10, 5), at(10, 11), at(10, 23)].into_iter().filter_map(|t| s.due("a", t)).collect();
    assert_eq!(looked, vec![(at(10, 4), at(10, 30)), (at(10, 10), at(10, 30)), (at(10, 22), at(10, 30)), (at(10, 30), at(10, 30))]);
    assert_eq!(s.check_grants(), Ok(()));
    s.handle(Input::AttemptFinished { leg: "a".into(), attempt: 1, outcome: Outcome::Success }, at(10, 24));
    assert_eq!(s.due("a", at(10, 24)), None);
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R38: without a p90, a grant's expected end is its cost's; with neither, its timeout's,
/// 360 minutes when its job says none. A grant a restart rebuilds (W05-R25) goes from when the
/// store says it started, and may be past its expected end already.
#[test]
fn w05_r38_ends_without_history() {
    let mut s = Scheduler::new(Config::default());
    s.handle(Input::RunnerUp(Box::new(linux("r", 3))), 0);
    s.handle(Input::LegQueued(Box::new(LegIn { cost: Some(2 * 60_000), ..leg("costed", "acme/x", 60) })), 1_000);
    s.handle(Input::LegQueued(Box::new(leg("unknown", "acme/x", 60))), 1_000);
    assert_eq!(s.due("costed", 1_000), Some((1_000 + 2 * MIN, 1_000 + 360 * MIN)));
    assert_eq!(s.due("unknown", 1_000), Some((1_000 + 360 * MIN, 1_000 + 360 * MIN)));
    assert_eq!(s.check_grants(), Ok(()));
    let mut t = Scheduler::with_epoch(Config::default(), 2);
    t.handle(Input::RunnerUp(Box::new(linux("r", 3))), 0);
    let adopted = LegIn { p90: Some(10 * 60_000), timeout: Some(60 * 60_000), started: Some(5 * MIN), ..leg("adopted", "acme/x", 60) };
    t.handle(Input::LegRunning { leg: Box::new(adopted), runner: "r".into(), attempt: 1, fence: Fence { epoch: 1, n: 1 } }, 8 * MIN);
    assert_eq!(t.due("adopted", 8 * MIN), Some((15 * MIN, 65 * MIN)));
    // Looked at at 16 minutes, past its expected end: 5 + 2 x 11.
    assert_eq!(t.due("adopted", 16 * MIN), Some((27 * MIN, 65 * MIN)));
    assert_eq!(t.check_grants(), Ok(()));
}

/// A runner of `cpus` CPUs in resource mode, for reservations' tests (W05-R39).
fn sized(name: &str, cpus: u64) -> RunnerIn {
    RunnerIn { capacity: Some(Resources { cpu: Cpus::whole(cpus), mem: 256 << 30, disk: 1 << 40, gpu: 0 }), ..linux(name, 64) }
}

/// A leg asking `cpus` CPUs, expected to take `p90` ms (W05-R38).
fn asking(id: &str, cpus: u64, p90: i64) -> LegIn {
    LegIn { request: Resources { cpu: Cpus::whole(cpus), mem: 1 << 30, ..Resources::default() }, p90: u64::try_from(p90).ok(), ..leg(id, "acme/x", 60) }
}

/// Reservations' tests run in resource mode, CPUs not overcommitted, backfilling or not.
fn backfilling(backfill: bool) -> Config {
    Config { resources: true, overcommit_cpu: 1000, backfill, ..Config::default() }
}

/// When a leg's reservation was first said, and from when (W05-R39).
fn reserved_at(sim: &Sim, leg: &str) -> Option<(i64, i64)> {
    sim.waits.iter().find_map(|(t, l, w)| match w {
        WaitReason::Reserved { at, .. } if l == leg => Some((*t, *at)),
        _ => None,
    })
}

/// W05-R39, docs/needs.md E4.2: a 12-CPU leg behind a steady stream of 1-CPU legs on one 16-CPU
/// runner (one a 4 s, a minute each). With backfill, it reserves the runner from when 11 more of
/// them are expected to have ended and starts then, within a minute of coming: no leg granted
/// meanwhile runs past that time beside it unless it fits. Without, it hasn't started by three
/// hours, as today's engine (docs/needs.md E4's probe).
#[test]
fn w05_r39_a_big_leg_reserves_the_runner_that_frees_first() {
    let run = |backfill: bool| {
        let mut sim = Sim::new(backfilling(backfill));
        sim.runner(0, sized("r", 16));
        for (i, t) in (0..3 * 60 * MIN).step_by(4_000).enumerate() {
            sim.leg(t, asking(&format!("s{i}"), 1, MIN), MIN);
        }
        sim.leg(5 * MIN, asking("big", 12, 10 * MIN), 10 * MIN);
        sim.run(3 * 60 * MIN);
        sim
    };
    let on = run(true);
    let (said, at) = reserved_at(&on, "big").unwrap_or_default();
    let started = on.started("big").unwrap_or(i64::MAX);
    assert_eq!(started, at, "it starts at its reservation's time, said at {said}");
    assert!(started - 5 * MIN <= MIN, "it waited {} ms", started - 5 * MIN);
    // What ran beside it as it started fit beside it: 4 CPUs.
    let beside = on.busy_legs.iter().filter(|(l, from, to)| l != "big" && *from <= started && *to > started).count();
    assert!(beside <= 4, "{beside} legs ran beside it");
    assert_eq!(run(false).started("big"), None);
}

/// W05-R39: on a 32-CPU runner, a 16-CPU leg reserves it from 10 minutes, when 12 CPUs free;
/// 8 are kept running past then. A 4-CPU leg that would run past it fits beside it then and
/// starts at once; an 8-CPU one doesn't, and waits behind it; a 2-CPU one that ends before it
/// starts at once.
#[test]
fn w05_r39_backfill_ends_before_or_fits_beside() {
    let mut sim = Sim::new(backfilling(true));
    sim.runner(0, sized("r", 32));
    sim.leg(0, asking("a", 12, 10 * MIN), 10 * MIN);
    sim.leg(0, asking("b", 8, 60 * MIN), 60 * MIN);
    sim.leg(MIN, asking("big", 16, 30 * MIN), 30 * MIN);
    sim.leg(MIN, asking("four", 4, 30 * MIN), 30 * MIN);
    sim.leg(MIN, asking("eight", 8, 30 * MIN), 30 * MIN);
    sim.leg(MIN, asking("two", 2, 5 * MIN), 5 * MIN);
    sim.run(5 * MIN);
    assert_eq!(reserved_at(&sim, "big").map(|(_, at)| at), Some(10 * MIN));
    assert_eq!((sim.started("four"), sim.started("two"), sim.started("eight")), (Some(MIN), Some(MIN), None));
    assert_eq!(sim.last_wait("eight"), Some(&WaitReason::Behind { leg: "big".into(), runner: "r".into(), at: 10 * MIN }));
    assert_eq!(sim.sched.check_grants(), Ok(()));
    sim.run(60 * MIN);
    assert_eq!(sim.started("big"), Some(10 * MIN));
    assert!(sim.started("eight").is_some_and(|t| t > 10 * MIN));
}

/// W05-R39: a grant runs past its p90. Once the reservation's time has passed, nothing new goes
/// into the room kept on that runner; the next time the reservation matters it's worked out again
/// from the grant's moved-on end (W05-R38), and the reserved leg starts as that grant ends.
#[test]
fn w05_r39_a_reservation_whose_time_passes_is_worked_out_again() {
    let mut sim = Sim::new(backfilling(true));
    sim.runner(0, sized("r", 16));
    sim.runner(0, sized("small", 4));
    // Expected to end at 5 minutes, it runs 8.
    sim.leg(0, asking("a", 12, 5 * MIN), 8 * MIN);
    sim.leg(MIN, asking("big", 12, 10 * MIN), 10 * MIN);
    sim.leg(MIN, asking("one", 1, 10 * MIN), 10 * MIN);
    sim.run(2 * MIN);
    assert_eq!(reserved_at(&sim, "big").map(|(_, at)| at), Some(5 * MIN));
    // Past its time: a leg that would run into the kept room doesn't start on r, but on small,
    // and the reservation is worked out again: a at 0 + 2 x 6 minutes.
    sim.leg(6 * MIN, asking("late", 1, 10 * MIN), 10 * MIN);
    sim.run(7 * MIN);
    assert_eq!(sim.assigned.iter().find(|a| a.leg == "late").map(|a| a.runner.as_str()), Some("small"));
    assert_eq!(sim.last_wait("big"), Some(&WaitReason::Reserved { runner: "r".into(), at: 12 * MIN }));
    sim.run(20 * MIN);
    assert_eq!(sim.started("big"), Some(8 * MIN));
}

/// W05-R39: two classes' first items each fit no runner: the critical one's reservation comes
/// first, the batch one's after the critical item is expected to end; and the critical item's
/// grant doesn't wait for the batch one's reservation.
#[test]
fn w05_r39_classes_reserve_in_order() {
    use super::state::Class;
    let mut sim = Sim::new(backfilling(true));
    sim.runner(0, sized("r", 16));
    sim.leg(0, asking("a", 14, 10 * MIN), 10 * MIN);
    sim.leg(MIN, LegIn { class: Class::Batch, ..asking("batch", 12, 20 * MIN) }, 20 * MIN);
    sim.leg(MIN, LegIn { class: Class::Critical, ..asking("critical", 12, 20 * MIN) }, 20 * MIN);
    sim.leg(MIN, asking("small", 2, 5 * MIN), 5 * MIN);
    sim.run(2 * MIN);
    assert_eq!(reserved_at(&sim, "critical").map(|(_, at)| at), Some(10 * MIN));
    assert_eq!(reserved_at(&sim, "batch").map(|(_, at)| at), Some(30 * MIN));
    assert_eq!(sim.sched.check_grants(), Ok(()));
    sim.run(60 * MIN);
    assert_eq!((sim.started("critical"), sim.started("batch")), (Some(10 * MIN), Some(30 * MIN)));
}

/// W05-R39: an exclusive leg asking for no CPUs reserves its runner whole: it starts as the
/// runner's grants end, and no leg granted there meanwhile runs past that.
#[test]
fn w05_r39_an_exclusive_leg_reserves_its_runner_whole() {
    let mut sim = Sim::new(Config { backfill: true, ..Config::default() });
    sim.runner(0, linux("r", 4));
    for i in 0..3 {
        sim.leg(0, LegIn { p90: Some(10 * 60_000), ..leg(&format!("held{i}"), "acme/x", 60) }, 10 * MIN);
    }
    sim.leg(MIN, LegIn { exclusive: true, p90: Some(10 * 60_000), ..leg("whole", "acme/x", 60) }, 10 * MIN);
    sim.leg(MIN, LegIn { p90: Some(10 * 60_000), ..leg("long", "acme/x", 60) }, 10 * MIN);
    sim.leg(2 * MIN, LegIn { p90: Some(5 * 60_000), ..leg("short", "acme/x", 60) }, 5 * MIN);
    sim.run(30 * MIN);
    assert_eq!(reserved_at(&sim, "whole").map(|(_, at)| at), Some(10 * MIN));
    assert_eq!(sim.started("short"), Some(2 * MIN));
    assert_eq!(sim.started("whole"), Some(10 * MIN));
    assert!(sim.started("long").is_some_and(|t| t >= 20 * MIN), "{:?}", sim.started("long"));
}

/// W05-R39, R16 (docs/sim.md's bug 16): a leg lost with its runner while the only other was away
/// has its room kept on its own runner, where it may go back; once the other is back it stays off
/// its own, and the room kept there goes with that: a leg behind it takes that runner as room
/// frees, rather than waiting behind room kept for one that won't come.
#[test]
fn w05_r39_room_kept_where_its_leg_now_stays_off_is_dropped() {
    use super::state::Class;
    // No floors: the interactive legs go first.
    let mut sim = Sim::new(Config { classes: Default::default(), ..backfilling(true) });
    sim.runner(0, sized("a", 16));
    sim.runner(0, sized("b", 16));
    sim.leg(0, asking("z", 12, 10 * MIN), 10 * MIN);
    sim.input(1_000, Input::RunnerLost { runner: "b".into() });
    sim.input(2_000, Input::RunnerLost { runner: "a".into() });
    sim.leg(3_000, LegIn { class: Class::Interactive, ..asking("y", 8, 10 * MIN) }, 10 * MIN);
    sim.leg(3_500, asking("u", 2, MIN), MIN);
    sim.runner(4_000, sized("a", 16));
    sim.leg(5_000, LegIn { class: Class::Interactive, ..asking("w", 12, 30 * MIN) }, 30 * MIN);
    sim.runner(6_000, sized("b", 16));
    sim.leg(7_000, asking("v", 8, 20 * MIN), 20 * MIN);
    sim.run(5 * MIN);
    // z's room kept on a from y's end, as a came back; then b, taken by w, and z stays off a.
    assert_eq!(reserved_at(&sim, "z").map(|(_, at)| at), Some(4_000 + 10 * MIN));
    assert_eq!(sim.started("w"), Some(6_000));
    assert_eq!(sim.started("v"), Some(4_000 + MIN), "v takes a as u ends");
    assert_eq!(sim.sched.check_grants(), Ok(()));
}

/// A leg of the gang `g` of `size` (W05-R40).
fn member(id: &str, g: &str, size: u32) -> LegIn {
    LegIn { gang: Some(super::state::Gang { id: g.into(), size }), ..leg(id, "acme/x", 60) }
}

/// The legs a handle's commands take back, and why.
fn taken_back(c: &[Command]) -> Vec<(String, super::state::Loss)> {
    c.iter().filter_map(|x| if let Command::Lost { leg, why, .. } = x { Some((leg.clone(), *why)) } else { None }).collect()
}

/// A gang of three on three one-slot runners, granted whole: the scheduler and each member's
/// grant (its runner and fence).
fn gang_of_three() -> (Scheduler, std::collections::BTreeMap<String, (String, Fence)>) {
    let mut s = Scheduler::new(Config::default());
    for r in ["a", "b", "c"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
    }
    for m in ["m0", "m1", "m2"] {
        s.handle(Input::LegQueued(Box::new(member(m, "sim", 3))), 1);
    }
    let grants = ["m0", "m1", "m2"].into_iter().filter_map(|m| s.grant(m).and_then(|(_, g)| g.map(|(r, f)| (m.to_owned(), (r.to_owned(), f))))).collect();
    (s, grants)
}

/// W05-R40, docs/needs.md E4.3: no member of a gang is granted before all its members are
/// queued, each saying how many of how many are; then all are granted in one handle.
#[test]
fn w05_r40_a_gang_waits_for_all_its_members() {
    let mut s = Scheduler::new(Config::default());
    for r in ["a", "b", "c"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
    }
    for m in ["m0", "m1"] {
        let c = s.handle(Input::LegQueued(Box::new(member(m, "sim", 3))), 1);
        assert_eq!(assigned(&c), Vec::<String>::new());
        assert_eq!(s.check_grants(), Ok(()));
    }
    assert_eq!(s.reason("m0"), Some(&WaitReason::Gang { gang: "sim".into(), have: 2, of: 3 }));
    let c = s.handle(Input::LegQueued(Box::new(member("m2", "sim", 3))), 2);
    assert_eq!(assigned(&c), vec!["m0", "m1", "m2"]);
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R40: room for two of three now, the third's runner busy until 10 minutes: with
/// backfilling, none starts before then, each member's runner is kept for it from then, a leg
/// that would run past then waits behind it, and the three start together then.
#[test]
fn w05_r40_a_gang_starts_whole_or_reserves_whole() {
    let mut sim = Sim::new(Config { backfill: true, ..Config::default() });
    for r in ["a", "b", "c"] {
        sim.runner(0, linux(r, 1));
    }
    sim.leg(0, LegIn { p90: Some(10 * 60_000), ..leg("x", "acme/y", 60) }, 10 * MIN);
    // x takes a; the gang's members come once it runs.
    for m in ["m0", "m1", "m2"] {
        sim.leg(MIN, LegIn { p90: Some(20 * 60_000), ..member(m, "sim", 3) }, 20 * MIN);
    }
    sim.leg(MIN, LegIn { p90: Some(30 * 60_000), ..leg("late", "acme/x", 60) }, 30 * MIN);
    sim.run(5 * MIN);
    assert_eq!(["m0", "m1", "m2"].map(|m| sim.started(m)), [None, None, None]);
    for m in ["m0", "m1", "m2"] {
        assert!(matches!(sim.last_wait(m), Some(WaitReason::Reserved { at, .. }) if *at == 10 * MIN), "{m}: {:?}", sim.last_wait(m));
    }
    assert!(matches!(sim.last_wait("late"), Some(WaitReason::Behind { at, .. }) if *at == 10 * MIN), "{:?}", sim.last_wait("late"));
    assert_eq!(sim.sched.check_grants(), Ok(()));
    sim.run(60 * MIN);
    assert_eq!(["m0", "m1", "m2"].map(|m| sim.started(m)), [Some(10 * MIN); 3]);
    assert!(sim.started("late").is_some_and(|t| t >= 30 * MIN), "{:?}", sim.started("late"));
}

/// W05-R40: a member refused by its runner takes its gang's other grants back (`Loss::Gang`), and
/// the gang is queued again at the same attempts and granted whole, the refused member on another
/// runner, once the lock-delay has passed (E4.4: by then the others' runners have stopped them).
#[test]
fn w05_r40_a_refused_member_takes_its_gang_back() {
    let (mut s, grants) = gang_of_three();
    assert_eq!(grants.len(), 3);
    let (runner, fence) = grants.get("m1").cloned().unwrap_or_default();
    let c = s.handle(Input::Rejected { leg: "m1".into(), fence }, 2);
    let mut back = taken_back(&c);
    back.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(back, vec![("m0".to_owned(), super::state::Loss::Gang), ("m2".to_owned(), super::state::Loss::Gang)]);
    // Their runners stop the other two meanwhile: the gang goes again once the lock-delay has passed.
    assert_eq!(assigned(&c), Vec::<String>::new(), "{c:?}");
    assert_eq!(s.check_grants(), Ok(()));
    assert_eq!(assigned(&s.handle(Input::Tick, 15_001)), Vec::<String>::new());
    let c = s.handle(Input::Tick, 15_002);
    let mut again = assigned(&c);
    again.sort();
    assert_eq!(again, vec!["m0", "m1", "m2"]);
    assert_eq!(["m0", "m1", "m2"].map(|m| s.grant(m).map(|g| g.0)), [Some(1); 3], "the same attempts");
    assert!(s.grant("m1").and_then(|g| g.1).is_some_and(|(r, _)| r != runner), "m1 on another runner");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R40: a member lost with its runner before any member's attempt has ended takes the others
/// back, and the three go again together at their next attempts, once there's room for all three
/// and the others' runners have stopped them (the lock-delay, E4.4).
#[test]
fn w05_r40_a_member_lost_before_any_ended_takes_its_gang_to_the_next_attempts() {
    let (mut s, grants) = gang_of_three();
    let (runner, _) = grants.get("m1").cloned().unwrap_or_default();
    let c = s.handle(Input::RunnerLost { runner: runner.clone() }, 2);
    let mut back = taken_back(&c);
    back.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(back.iter().filter(|(_, why)| *why == super::state::Loss::Gang).map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["m0", "m2"]);
    assert_eq!(["m0", "m1", "m2"].map(|m| s.grant(m)), [Some((2, None)); 3], "each at its next attempt, none running");
    assert_eq!(s.check_grants(), Ok(()));
    // Two runners can't hold three: they wait whole until the third is back, and the other two
    // until their runners have stopped them, the lock-delay after.
    let c = s.handle(Input::RunnerUp(Box::new(linux(&runner, 1))), 3);
    assert_eq!(assigned(&c), Vec::<String>::new(), "{c:?}");
    let c = s.handle(Input::Tick, 15_002);
    let mut again = assigned(&c);
    again.sort();
    assert_eq!(again, vec!["m0", "m1", "m2"]);
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R40: once a member's attempt has ended, a member lost isn't tried again alone (its peers
/// aren't there to meet it): it ends failed, runner-lost, and the rest run on.
#[test]
fn w05_r40_a_member_lost_after_another_ended_is_not_tried_alone() {
    let (mut s, grants) = gang_of_three();
    s.handle(Input::AttemptFinished { leg: "m0".into(), attempt: 1, outcome: Outcome::Success }, 2);
    let (runner, _) = grants.get("m1").cloned().unwrap_or_default();
    let c = s.handle(Input::RunnerLost { runner }, 3);
    assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, outcome: Outcome::Failed(f), .. } if leg == "m1" && f.class == FailureClass::RunnerLost)), "{c:?}");
    assert!(!taken_back(&c).iter().any(|(l, _)| l == "m2"), "{c:?}");
    assert!(s.grant("m2").and_then(|g| g.1).is_some(), "m2 runs on");
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R40: a gang running when the server restarts comes back as it ran, its members started
/// together (W05-R25): one lost then takes the other back with it.
#[test]
fn w05_r40_a_restart_rebuilds_a_gang_as_it_ran() {
    let mut s = Scheduler::with_epoch(Config::default(), 2);
    for r in ["a", "b"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
    }
    for (m, r, n) in [("m0", "a", 1), ("m1", "b", 2)] {
        s.handle(Input::LegRunning { leg: Box::new(member(m, "pair", 2)), runner: r.into(), attempt: 1, fence: Fence { epoch: 1, n } }, 1);
    }
    assert_eq!(s.check_grants(), Ok(()));
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 2);
    assert_eq!(taken_back(&c).into_iter().filter(|(_, why)| *why == super::state::Loss::Gang).map(|(l, _)| l).collect::<Vec<_>>(), vec!["m1"]);
    assert_eq!(["m0", "m1"].map(|m| s.grant(m).map(|g| g.0)), [Some(2); 2]);
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R40, R25 (docs/sim.md's bug 15): a gang of two whose one member settled before the server
/// restarted: the rebuild brings back only the other, its start one ended already, so lost then it
/// ends failed, runner-lost, as it would have without the restart, not tried again alone to wait
/// for good for a mate that won't come.
#[test]
fn w05_r40_a_member_lost_after_a_restart_its_mate_ended_before_is_not_tried_alone() {
    let mut s = Scheduler::with_epoch(Config::default(), 2);
    for r in ["a", "b"] {
        s.handle(Input::RunnerUp(Box::new(linux(r, 1))), 0);
    }
    s.handle(Input::LegRunning { leg: Box::new(member("m0", "pair", 2)), runner: "a".into(), attempt: 1, fence: Fence { epoch: 1, n: 1 } }, 1);
    assert_eq!(s.check_grants(), Ok(()));
    let c = s.handle(Input::RunnerLost { runner: "a".into() }, 2);
    assert!(c.iter().any(|x| matches!(x, Command::Settled { leg, outcome: Outcome::Failed(f), .. } if leg == "m0" && f.class == FailureClass::RunnerLost)), "{c:?}");
    assert!(!c.iter().any(|x| matches!(x, Command::Retry { .. })), "{c:?}");
    assert_eq!(s.grant("m0"), None);
    assert_eq!(s.check_grants(), Ok(()));
}

/// W05-R39, R40 (docs/sim.md's bug 14): two whole gangs of a class, the second's room kept from
/// when an exclusive leg ends, the first whole only after: when it ends, the first, with room by
/// room alone, isn't the first of the class waiting for room, so the class's room is worked out
/// again without it; it starts then, and the second, its room kept from the first's end, after.
/// Each counted as waiting while the other's room held it off, they took that room from each other
/// for good, the engine never done with the input.
#[test]
fn w05_r40_two_whole_gangs_of_a_class_start_in_turn_not_taking_its_room_from_each_other() {
    let mut sim = Sim::new(Config { backfill: true, ..Config::default() });
    sim.runner(0, linux("a", 3));
    sim.runner(0, linux("b", 1));
    sim.leg(0, LegIn { exclusive: true, p90: Some(10 * 60_000), ..leg("x", "acme/y", 60) }, 10 * MIN);
    sim.leg(0, leg("q", "acme/y", 60), 3_000);
    let gang = |id: &str, g: &str, size: u32| LegIn { p90: Some(5 * 60_000), ..member(id, g, size) };
    sim.leg(1_000, gang("g0m0", "first", 2), 5 * MIN);
    for m in ["g1m0", "g1m1", "g1m2"] {
        sim.leg(2_000, gang(m, "second", 3), 5 * MIN);
    }
    sim.leg(4_000, gang("g0m1", "first", 2), 5 * MIN);
    sim.run(5 * MIN);
    // q's end has b pass the second over: its room is kept from x's end.
    let second = ["g1m0", "g1m1", "g1m2"];
    assert!(second.iter().all(|m| matches!(sim.last_wait(m), Some(WaitReason::Reserved { at, .. }) if *at == 10 * MIN)), "{:?}", second.map(|m| sim.last_wait(m)));
    sim.run(30 * MIN);
    assert_eq!(["g0m0", "g0m1"].map(|m| sim.started(m)), [Some(10 * MIN); 2]);
    assert_eq!(second.map(|m| sim.started(m)), [Some(15 * MIN); 3]);
    assert_eq!(sim.sched.check_grants(), Ok(()));
}

/// A stream of 1-CPU legs on the simulation's runners, one each `every` ms until `until`, each
/// expected to take a minute (its p90) and taking from half a minute to a fifth past it, within its
/// 10-minute timeout (docs/needs.md E4.6).
fn stream(sim: &mut Sim, d: &mut Draw, every: usize, until: i64, cpus: impl Fn(&mut Draw) -> u64) {
    for (i, t) in (0..until).step_by(every).enumerate() {
        let ran = MIN / 2 + i64::try_from(d.below(42_000)).unwrap_or(0);
        let n = cpus(d);
        sim.leg(t, LegIn { timeout: u64::try_from(10 * MIN).ok(), ..asking(&format!("s{i}"), n, MIN) }, ran);
    }
}

/// W05-AC17, docs/needs.md E4.6 (G6): a 12-CPU leg among a steady stream of 1-CPU legs on one
/// 16-CPU runner, one every 4 s, each taking from half its p90 to a fifth past it: in each of 1,000
/// seeds room is kept for it, and it starts by its bound, its room's first time plus how far the
/// grants it waited for ran past their expected ends. Without backfilling it doesn't start within
/// three hours, as E4's probe found.
#[test]
fn w05_ac17_a_12_cpu_leg_among_a_stream_of_1_cpu_legs_starts_by_its_bound() {
    let run = |seed: u64, backfill: bool, until: i64| {
        let mut d = Draw(seed);
        let mut sim = Sim::new(backfilling(backfill));
        sim.runner(0, sized("r", 16));
        stream(&mut sim, &mut d, 4_000, until, |_| 1);
        sim.leg(5 * MIN, LegIn { timeout: u64::try_from(60 * MIN).ok(), ..asking("big", 12, 10 * MIN) }, 10 * MIN);
        sim.run(until);
        sim
    };
    let mut late = Vec::new();
    for seed in 0..1000 {
        let sim = run(seed, true, 20 * MIN);
        match sim.held.iter().find(|h| h.leg == "big" && h.started.is_some()) {
            Some(h) if h.started <= h.bound => {}
            other => late.push((seed, other.cloned())),
        }
    }
    assert!(late.is_empty(), "{} of 1,000 seeds didn't start by their bound; the first: {:?}", late.len(), late.first());
    for seed in 0..3 {
        assert_eq!(run(seed, false, 3 * 60 * MIN).started("big"), None, "seed {seed} without backfilling");
    }
}

/// W05-AC18, docs/needs.md E4.6 (G4): a gang of three 4-CPU members among competing 1- and 2-CPU
/// legs on three or four runners of 4 or 8 CPUs, a member refused by its runner in a third of the
/// seeds and a runner lost (and back two minutes later) in a third: in each of 1,000 seeds no grant
/// takes part of the gang; and room kept for it that stood until it started saw it start by its
/// bound (that room's time where nothing it waited for ran over). Room that went (a take-back, a
/// runner lost, room worked out again once a grant ran past its expected end) starts the bound
/// again from the next.
#[test]
fn w05_ac18_a_gang_of_three_never_starts_in_part() {
    use super::state::Gang;
    let (mut in_part, mut late, mut kept, mut started, mut refused, mut lost) = (Vec::new(), Vec::new(), 0, 0, 0, 0);
    for seed in 0..1000 {
        let mut d = Draw(seed);
        let mut sim = Sim::new(backfilling(true));
        let runners: Vec<RunnerIn> = (0..3 + d.below(2)).map(|i| sized(&format!("r{i}"), 4 + 4 * d.below(2))).collect();
        for r in &runners {
            sim.runner(0, r.clone());
        }
        stream(&mut sim, &mut d, 10_000, 30 * MIN, |d| 1 + d.below(2));
        for m in 0..3 {
            sim.leg(2 * MIN, LegIn { gang: Some(Gang { id: "sim".into(), size: 3 }), timeout: u64::try_from(60 * MIN).ok(), ..asking(&format!("m{m}"), 4, 5 * MIN) }, 5 * MIN);
        }
        if d.below(3) == 0 {
            refused += 1;
            sim.refuse(&format!("m{}", d.below(3)), 1);
        }
        if d.below(3) == 0
            && let Some(r) = runners.get(usize::try_from(d.below(runners.len() as u64)).unwrap_or(0))
        {
            lost += 1;
            let at = 2 * MIN + i64::try_from(d.below(10 * 60_000)).unwrap_or(0);
            sim.input(at, Input::RunnerLost { runner: r.id.clone() });
            sim.runner(at + 2 * MIN, r.clone());
        }
        sim.run(90 * MIN);
        in_part.extend(sim.in_part.iter().map(|x| (seed, x.clone())));
        for h in sim.held.iter().filter(|h| h.leg.starts_with('m')) {
            kept += 1;
            if let (Some(s), Some(b)) = (h.started, h.bound) {
                started += 1;
                if s > b {
                    late.push((seed, h.clone()));
                }
            }
        }
        assert!(["m0", "m1", "m2"].iter().all(|m| sim.started(m).is_some()), "seed {seed}: the gang never started");
    }
    assert!(in_part.is_empty(), "{} grants took part of the gang; the first: {:?}", in_part.len(), in_part.first());
    assert!(late.is_empty(), "{} members started past their bound; the first: {:?}", late.len(), late.first());
    eprintln!("w05_ac18: room kept for the gang's members {kept} times, {started} of them starting from it; {refused} seeds with a refusal, {lost} with a runner lost");
    assert!(kept > 0 && started > 0 && refused > 0 && lost > 0, "kept {kept}, started {started}, refused {refused}, lost {lost}");
}
