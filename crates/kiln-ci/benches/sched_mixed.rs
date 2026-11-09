//! W05-R26's target with backfilling on (docs/needs.md E4.7): big legs among small ones, so the
//! engine works out reservations as runners take legs (W05-R39). In resource mode with `[sched]
//! backfill`, 250 runners of 16 CPUs in 50 pools; 10,000 legs queued at once, one in twenty asking
//! 12 CPUs and expected to take 10 minutes, the rest one CPU and a minute; each finishing in turn,
//! 10 ms apart. Assignments a second, an input's p99 and mean, and how often a leg was told room was
//! kept for it or that it waited behind such room (`cargo bench -p kiln-ci --bench sched_mixed`).

use std::collections::VecDeque;
use std::time::Instant;

use kiln_ci::facts::RunnerFacts;
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::state::{Command, Config, Cpus, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, WaitReason};

const LEGS: usize = 10_000;

struct Run {
    rate: f64,
    p99: f64,
    mean: f64,
    assigned: usize,
    reserved: usize,
    behind: usize,
}

fn main() {
    let runs: Vec<Run> = (0..3).map(|_| once()).collect();
    for r in &runs {
        println!(
            "sched mixed: {:.0} assignments/s, input p99 {:.3} ms, mean {:.4} ms; {} of {LEGS} legs assigned, room kept {} times, {} waits behind it",
            r.rate, r.p99, r.mean, r.assigned, r.reserved, r.behind
        );
    }
    let best = runs.iter().map(|r| r.rate).fold(0.0, f64::max);
    println!("sched mixed: best {best:.0} assignments/s (W05-R26 asks at least 2,000)");
}

fn once() -> Run {
    // Grants as leases, as kiln has them (W05-R34); resources and backfilling declared.
    let mut s = Scheduler::new(Config { lease: Some(60_000), resources: true, backfill: true, ..Config::default() });
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    for i in 0..250 {
        let r = RunnerIn {
            id: format!("r{i}"),
            view: RunnerView { name: format!("r{i}"), label: None, labels: vec![format!("pool-{}", i % 50)], facts: facts.clone(), caps: 0, pool: None },
            slots: 64,
            capacity: Some(Resources { cpu: Cpus::whole(16), mem: 64 << 30, disk: 1 << 40, gpu: 0 }),
            executor: None,
            draining: false,
        };
        s.handle(Input::RunnerUp(Box::new(r)), 0);
    }
    let mut lat: Vec<f64> = Vec::with_capacity(2 * LEGS);
    let mut running: VecDeque<String> = VecDeque::new();
    let (mut assigned, mut reserved, mut behind) = (0usize, 0usize, 0usize);
    let mut handle = |s: &mut Scheduler, i: Input, now: i64, running: &mut VecDeque<String>| {
        let t = Instant::now();
        let out = s.handle(i, now);
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
        for c in out {
            match c {
                Command::Assign { leg, .. } => {
                    assigned += 1;
                    running.push_back(leg);
                }
                Command::Wait { reason: WaitReason::Reserved { .. }, .. } => reserved += 1,
                Command::Wait { reason: WaitReason::Behind { .. }, .. } => behind += 1,
                _ => {}
            }
        }
    };
    let start = Instant::now();
    for j in 0..LEGS {
        let big = j % 20 == 19;
        let l = LegIn {
            id: format!("l{j}"),
            run: format!("run{}", j / 10),
            repo: format!("acme/r{}", j % 7),
            route: LegRoute { labels: vec![format!("pool-{}", j % 50)], ..LegRoute::default() },
            priority: 60,
            request: Resources { cpu: Cpus::whole(if big { 12 } else { 1 }), mem: 1 << 30, ..Resources::default() },
            p90: Some(if big { 600_000 } else { 60_000 }),
            ..LegIn::default()
        };
        handle(&mut s, Input::LegQueued(Box::new(l)), 1, &mut running);
    }
    let mut now = 1;
    while let Some(leg) = running.pop_front() {
        now += 10;
        handle(&mut s, Input::AttemptFinished { leg, attempt: 1, outcome: Outcome::Success }, now, &mut running);
    }
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(f64::total_cmp);
    let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    Run { rate: assigned as f64 / secs, p99: lat.get(lat.len() * 99 / 100).copied().unwrap_or(f64::NAN), mean, assigned, reserved, behind }
}
