//! E1.4 (docs/needs.md): the engine gives the same commands on every machine. Seeded scenarios
//! (SplitMix64, as kiln-sim draws) of runners of mixed sizes and platforms, legs of mixed needs,
//! classes, priorities, repositories and p90s, attempts finishing, failing and retried, legs cancelled
//! and runners lost and back drive the engine through its simulator; in half the seeds grants are
//! leases, renewed by runners' heartbeats, some runners falling silent past them (W05-R34), and in
//! half it backfills, keeping room for each class's first item that fits no runner (W05-R39).
//! Every command it gives is written
//! in a stable text form and hashed with SHA-256, and the digest must be the one committed beside
//! this file (`replay.digest`). kiln's CI runs it on x86-64 and on arm64 (`.kiln/ci.yaml`).
//!
//! A change that moves a decision moves the digest: `KILN_SCHED_REPLAY_PRINT=1 cargo test -p
//! kiln-ci --lib sched::replay -- --nocapture` prints the commands and the digest, and the change
//! that moves it updates `replay.digest` on purpose.

use sha2::{Digest as _, Sha256};

use super::retry::{FailureClass, RetryOn, RetryPolicy, RunnerChoice};
use super::routing::{LegRoute, RunnerView};
use super::sim::Sim;
use super::state::{Class, ClassConfig, Command, Config, Cpus, FairBy, Input, LeaseLoss, LegIn, Loss, Outcome, Resources, RunnerIn};
use super::tests::Draw;
use crate::facts::RunnerFacts;

const SEEDS: u64 = 32;
const SECOND: i64 = 1_000;
const DAY: i64 = 24 * 60 * 60 * SECOND;
const GIB: u64 = 1 << 30;
const PLATFORMS: [(&str, &str); 3] = [("linux", "x86_64"), ("linux", "aarch64"), ("macos", "aarch64")];
const PRIORITIES: [u8; 6] = [0, 20, 40, 60, 80, 100];
const CHOICES: [RunnerChoice; 5] = [RunnerChoice::Default, RunnerChoice::Any, RunnerChoice::Same, RunnerChoice::Different, RunnerChoice::Another];

/// A string as JSON writes it: quoted and escaped alike everywhere.
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn opt(s: &Option<String>) -> String {
    s.as_deref().map_or_else(|| "-".to_owned(), q)
}

fn outcome(o: &Outcome) -> String {
    match o {
        Outcome::Success => "success".into(),
        Outcome::Cancelled => "cancelled".into(),
        Outcome::Failed(f) => format!("failed {} exit={} hits={}", f.class.as_str(), f.exit_code.map_or_else(|| "-".to_owned(), |c| c.to_string()), serde_json::to_string(&f.log_hits).unwrap_or_default()),
    }
}

/// A command in a stable text form: each field written out (a wait's reason as its JSON), never
/// as `Debug` writes it, whose form Rust doesn't promise to keep.
fn line(c: &Command) -> String {
    match c {
        Command::StartRun { run } => format!("start-run {}", q(run)),
        Command::CancelRun { run, reason } => format!("cancel-run {} {}", q(run), q(reason)),
        Command::Assign { leg, runner, attempt, executor, image, docker, slot, fence, expires, lease_loss } => {
            let expires = expires.map_or_else(|| "-".to_owned(), |at| at.to_string());
            let loss = match lease_loss {
                LeaseLoss::Finish => "finish",
                LeaseLoss::Stop => "stop",
            };
            format!("assign {} {} attempt={attempt} executor={} image={} docker={} slot={} fence={}.{} expires={expires} on-lease-loss={loss}", q(leg), q(runner), opt(executor), opt(image), opt(docker), opt(slot), fence.epoch, fence.n)
        }
        Command::StartExecutor { pool, executor, runner, kind, image, leg } => format!("start-executor {} {} {} {} image={} leg={}", q(pool), q(executor), q(runner), q(kind), opt(image), opt(leg)),
        Command::StopExecutor { executor, runner } => format!("stop-executor {} {}", q(executor), q(runner)),
        Command::Cancel { leg, attempt, reason } => format!("cancel {} attempt={attempt} {}", q(leg), q(reason)),
        Command::Retry { leg, attempt, at } => format!("retry {} attempt={attempt} at={at}", q(leg)),
        Command::Wait { leg, reason } => format!("wait {} {}", q(leg), serde_json::to_string(reason).unwrap_or_default()),
        Command::Settled { leg, outcome: o, reason } => format!("settled {} {} reason={}", q(leg), outcome(o), opt(reason)),
        Command::Timer { at } => format!("timer at={at}"),
        Command::Demand { leg, decision } => format!("demand {} {}", q(leg), serde_json::to_string(decision).unwrap_or_default()),
        Command::Adopted { leg, from, outcome: o } => format!("adopted {} from {} {}", q(leg), q(from), outcome(o)),
        Command::Lost { leg, attempt, runner, fence, why } => {
            let why = match why {
                Loss::Expired => "expired",
                Loss::RunnerLost => "runner-lost",
                Loss::Gang => "gang",
            };
            format!("lost {} attempt={attempt} {} fence={}.{} {why}", q(leg), q(runner), fence.epoch, fence.n)
        }
        Command::Overflowed { leg, why } => format!("overflowed {} {}", q(leg), q(why)),
    }
}

fn ms(d: &mut Draw, below: i64) -> i64 {
    i64::try_from(d.below(u64::try_from(below).unwrap_or(1))).unwrap_or(0)
}

/// A runner: on one of three platforms, with one to four slots, and half of them sized (resource
/// mode, W05-R03), a quarter of those with GPUs (fair share's dimensions, docs/needs.md E2.1).
fn runner(d: &mut Draw, id: &str) -> RunnerIn {
    let (os, arch) = PLATFORMS.get(usize::try_from(d.below(3)).unwrap_or(0)).copied().unwrap_or(("linux", "x86_64"));
    let slots = 1 + u32::try_from(d.below(4)).unwrap_or(0);
    let capacity = (d.below(2) == 0).then(|| Resources { cpu: Cpus::whole(2 + d.below(15)), mem: (4 + d.below(61)) * GIB, gpu: if d.below(4) == 0 { 1 + u32::try_from(d.below(4)).unwrap_or(0) } else { 0 }, ..Resources::default() });
    let facts = RunnerFacts { os: os.into(), arch: arch.into(), ..RunnerFacts::default() };
    RunnerIn { id: id.into(), view: RunnerView { name: id.into(), label: None, labels: vec!["kiln".into()], facts, caps: 0, pool: None }, slots, capacity, executor: None, draining: false }
}

/// A leg, how long it runs, and how its first attempts fail.
fn leg(d: &mut Draw, n: u64) -> (LegIn, i64, Vec<FailureClass>) {
    let labels = |l: &str| vec![l.to_owned()];
    let route = match d.below(10) {
        0..=5 => LegRoute { labels: labels("self-hosted"), ..LegRoute::default() },
        6 => LegRoute { labels: labels("Linux"), ..LegRoute::default() },
        7 => LegRoute { labels: labels("ARM64"), ..LegRoute::default() },
        8 => LegRoute { labels: labels("self-hosted"), requires: vec!["arch == aarch64".into()], ..LegRoute::default() },
        _ => LegRoute { labels: labels("self-hosted"), requires: vec!["os == linux".into()], ..LegRoute::default() },
    };
    let mut request = match d.below(3) {
        0 => Resources::default(),
        1 => Resources { cpu: Cpus::milli(250 * (1 + d.below(16))), mem: (1 + d.below(8)) * GIB / 2, ..Resources::default() },
        _ => Resources { cpu: Cpus::whole(1 + d.below(8)), mem: (1 + d.below(32)) * GIB, ..Resources::default() },
    };
    if !request.cpu.is_zero() && d.below(6) == 0 {
        request.gpu = 1;
    }
    let retry = (d.below(5) == 0).then(|| RetryPolicy {
        max: 2,
        on: vec![RetryOn::Class(FailureClass::Infra), RetryOn::Class(FailureClass::Oom)],
        initial_ms: 5_000,
        factor: 2_000,
        max_ms: 60_000,
        runner: CHOICES.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default(),
    });
    let duration = 5 * SECOND + ms(d, 120 * SECOND);
    let cost = (d.below(2) == 0).then(|| u64::try_from(duration / 2 + ms(d, duration)).unwrap_or(0));
    // Its p90 from history (W05-R38) half the time, about how long it runs, now and then less, so
    // grants run past it.
    let p90 = (d.below(2) == 0).then(|| u64::try_from(duration / 2 + ms(d, duration)).unwrap_or(0));
    let l = LegIn {
        id: format!("l{n}"),
        run: format!("run{}", n / 4),
        repo: format!("acme/r{}", d.below(3)),
        route,
        priority: PRIORITIES.get(usize::try_from(d.below(6)).unwrap_or(0)).copied().unwrap_or(60),
        exclusive: d.below(8) == 0,
        request,
        retry,
        cost,
        p90,
        actor: format!("a{}", d.below(3)),
        lane: format!("lane{}", d.below(2)),
        class: Class::ALL.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default(),
        ..LegIn::default()
    };
    let fails = match d.below(6) {
        0 => vec![FailureClass::Infra],
        1 => vec![FailureClass::Oom, FailureClass::Infra],
        2 => vec![FailureClass::Failure],
        _ => Vec::new(),
    };
    (l, duration, fails)
}

/// One seed's commands, each with its time, in the order the engine gave them. With leases
/// (W05-R34), runners renew every 10 s, some fall silent for half a minute to three (their grants
/// lost past a minute), and some legs stop on losing theirs.
fn scenario(seed: u64, cfg: &Config) -> Vec<(i64, String)> {
    let mut d = Draw(seed);
    let mut sim = Sim::new(cfg.clone());
    sim.trace = Some(Vec::new());
    if cfg.lease.is_some() {
        sim.renew_every(10 * SECOND);
    }
    let runners: Vec<RunnerIn> = (0..3 + d.below(5)).map(|i| runner(&mut d, &format!("r{i}"))).collect();
    for r in &runners {
        sim.runner(0, r.clone());
    }
    let (mut t, mut ids) = (0, Vec::new());
    for n in 0..15 + d.below(30) {
        t += ms(&mut d, 20 * SECOND);
        let (mut l, duration, fails) = leg(&mut d, n);
        if cfg.lease.is_some() && d.below(4) == 0 {
            l.on_lease_loss = LeaseLoss::Stop;
        }
        sim.fail(&l.id, &fails);
        ids.push(l.id.clone());
        sim.leg(t, l, duration);
    }
    if cfg.lease.is_some() {
        for _ in 0..1 + d.below(3) {
            let from = ms(&mut d, t + 60 * SECOND);
            if let Some(r) = runners.get(usize::try_from(d.below(runners.len() as u64)).unwrap_or(0)) {
                sim.silence(&r.id, from, from + 30 * SECOND + ms(&mut d, 150 * SECOND));
            }
        }
    }
    // Runners lost, and back a while later.
    for _ in 0..d.below(4) {
        let at = ms(&mut d, t + 60 * SECOND);
        if let Some(r) = runners.get(usize::try_from(d.below(runners.len() as u64)).unwrap_or(0)) {
            sim.input(at, Input::RunnerLost { runner: r.id.clone() });
            sim.runner(at + 1 + ms(&mut d, 120 * SECOND), r.clone());
        }
    }
    // Legs cancelled, queued or running.
    for _ in 0..d.below(3) {
        let at = ms(&mut d, t + 60 * SECOND);
        if let Some(id) = d.pick(&ids) {
            sim.input(at, Input::LegCancelled { leg: id.clone(), reason: "its run was cancelled".into() });
        }
    }
    sim.run(DAY);
    sim.trace.take().unwrap_or_default().into_iter().map(|(at, c)| (at, line(&c))).collect()
}

/// Every seed's commands, as lines, and their SHA-256.
fn replay(cfg: &Config) -> (Vec<String>, String) {
    let mut lines = Vec::new();
    let mut h = Sha256::new();
    for seed in 0..SEEDS {
        // Odd seeds in resource mode, where fair share counts CPUs, memory and GPUs beside slots;
        // fair share by repository, by actor within it, or by lane within that (docs/needs.md E2.2);
        // the classes' floors and bounds as they come, or every other six seeds higher floors and
        // bounds of minutes, which a scenario's legs pass (W05-R31).
        let fair_by = match (seed / 2) % 3 {
            0 => vec![FairBy::Repo],
            1 => vec![FairBy::Repo, FairBy::Actor],
            _ => vec![FairBy::Repo, FairBy::Actor, FairBy::Lane],
        };
        // Grants are leases in seeds 4 to 7 of every 8 (W05-R34); it backfills in seeds 8 to 15 of
        // every 16 (W05-R39).
        let lease = ((seed / 4) % 2 == 1).then_some(60 * 1000);
        let mut cfg = Config { resources: seed % 2 == 1, fair_by, lease, backfill: (seed / 8) % 2 == 1, ..cfg.clone() };
        if (seed / 6) % 2 == 1 {
            let minutes = |m: u64| Some(m * 60 * 1000);
            cfg.classes = [(Class::Interactive, ClassConfig { floor: 50, bound: None }), (Class::Batch, ClassConfig { floor: 250, bound: minutes(2) }), (Class::Background, ClassConfig { floor: 100, bound: minutes(3) })].into_iter().collect();
        }
        for (at, l) in scenario(seed, &cfg) {
            let l = format!("{seed} {at} {l}");
            h.update(l.as_bytes());
            h.update(b"\n");
            lines.push(l);
        }
    }
    (lines, h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// E1.4: the replay gives the digest committed beside it, on whatever machine it runs.
#[test]
fn e1_4_the_replay_gives_the_committed_digest() {
    let (lines, digest) = replay(&Config::default());
    if std::env::var_os("KILN_SCHED_REPLAY_PRINT").is_some() {
        for l in &lines {
            println!("{l}");
        }
        println!("digest {digest}");
    }
    // What the scenarios cover: legs waiting, finishing, failing, cancelled queued and running,
    // retried after a backoff and assigned again, and runners lost under legs (a leg retried at
    // once, or failed for it).
    let has = |what: &[&str]| lines.iter().any(|l| what.iter().all(|w| l.contains(w)));
    // And grants lost past their leases, a stop leg's among them (W05-R34); and room kept for a
    // class's first item, legs waiting behind it (W05-R39).
    for what in [&[" wait "][..], &[" success "], &[" failed "], &[" settled ", " cancelled "], &[" cancel ", " attempt="], &[" timer "], &[" assign ", " attempt=2 "], &[" lost ", " expired"], &[" assign ", " on-lease-loss=stop"], &["\"reason\":\"reserved\""], &["\"reason\":\"behind\""]] {
        assert!(has(what), "no command has {what:?} in it");
    }
    let lost = lines.iter().any(|l| {
        let at = l.split(' ').nth(1).unwrap_or_default();
        l.contains(" failed runner-lost ") || (l.contains(" retry ") && l.ends_with(&format!(" at={at}")))
    });
    assert!(lost, "no runner was lost under a leg");
    assert_eq!(
        digest,
        include_str!("replay.digest").trim(),
        "the replay's commands changed ({} of them): where the change meant to move a decision, see them with KILN_SCHED_REPLAY_PRINT=1 cargo test -p kiln-ci --lib sched::replay -- --nocapture, and put the digest it prints in crates/kiln-ci/src/sched/replay.digest",
        lines.len()
    );
}

/// E1.4: the digest moves with any decision: the same scenarios with aging off and one
/// repository's weight raised give another.
#[test]
fn e1_4_another_decision_gives_another_digest() {
    let mut cfg = Config { aging_per_minute: 0, ..Config::default() };
    cfg.weights.insert("acme/r0".into(), 4_000);
    assert_ne!(replay(&cfg).1, include_str!("replay.digest").trim());
}
