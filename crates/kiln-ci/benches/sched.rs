//! W05-AC08 on the bare engine: 1,000 runners of 4 slots in 50 pools, 10,000 legs queued at once,
//! each finishing in turn; assignments a second, and an input's p99 and mean
//! (`cargo bench -p kiln-ci --bench sched`). docs/needs.md's E1 measures with it before and after a
//! change to how the engine keeps its state.

use std::collections::VecDeque;
use std::time::Instant;

use kiln_ci::facts::RunnerFacts;
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::state::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler};

fn main() {
    let runs: Vec<(f64, f64, f64)> = (0..3).map(|_| once()).collect();
    for (rate, p99, mean) in &runs {
        println!("sched: {rate:.0} assignments/s, input p99 {p99:.3} ms, mean {mean:.4} ms");
    }
    let best = runs.iter().map(|r| r.0).fold(0.0, f64::max);
    println!("sched: best {best:.0} assignments/s (W05-AC08 asks at least 2,000)");
}

fn once() -> (f64, f64, f64) {
    // Grants as leases, as kiln has them (W05-R34).
    let mut s = Scheduler::new(Config { lease: Some(60_000), ..Config::default() });
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    for i in 0..1_000 {
        let r = RunnerIn { id: format!("r{i}"), view: RunnerView { name: format!("r{i}"), labels: vec![format!("pool-{}", i % 50)], facts: facts.clone(), caps: 0, pool: None }, slots: 4, capacity: None, executor: None, draining: false };
        s.handle(Input::RunnerUp(Box::new(r)), 0);
    }
    let mut lat: Vec<f64> = Vec::with_capacity(20_000);
    let mut running: VecDeque<String> = VecDeque::new();
    let mut assigned = 0usize;
    let mut handle = |s: &mut Scheduler, i: Input, now: i64, running: &mut VecDeque<String>| {
        let t = Instant::now();
        let out = s.handle(i, now);
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
        for c in out {
            if let Command::Assign { leg, .. } = c {
                assigned += 1;
                running.push_back(leg);
            }
        }
    };
    let start = Instant::now();
    for j in 0..10_000 {
        let l = LegIn { id: format!("l{j}"), run: format!("run{}", j / 10), repo: format!("acme/r{}", j % 7), route: LegRoute { labels: vec![format!("pool-{}", j % 50)], ..LegRoute::default() }, priority: 60, ..LegIn::default() };
        handle(&mut s, Input::LegQueued(Box::new(l)), 1, &mut running);
    }
    let mut now = 2;
    while let Some(leg) = running.pop_front() {
        now += 1;
        handle(&mut s, Input::AttemptFinished { leg, attempt: 1, outcome: Outcome::Success }, now, &mut running);
    }
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(f64::total_cmp);
    let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    (assigned as f64 / secs, lat.get(lat.len() * 99 / 100).copied().unwrap_or(f64::NAN), mean)
}
