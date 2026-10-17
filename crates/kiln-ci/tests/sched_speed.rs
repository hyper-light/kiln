//! W05-AC08, asserted (release builds only): 1,000 runners, 10,000 queued legs over 50
//! signatures and a stream of completions: at least 2,000 assignments per second, and every
//! input handled within 50 ms at p99.

#[cfg(not(debug_assertions))]
#[test]
fn w05_ac08_scheduler_speed() {
    use std::collections::VecDeque;
    use std::time::Instant;

    use kiln_ci::facts::RunnerFacts;
    use kiln_ci::sched::routing::{LegRoute, RunnerView};
    use kiln_ci::sched::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler};

    let mut s = Scheduler::new(Config::default());
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    for i in 0..1_000 {
        let r = RunnerIn { id: format!("r{i}"), view: RunnerView { name: format!("r{i}"), labels: vec![format!("pool-{}", i % 50)], facts: facts.clone(), caps: 0, pool: None }, slots: 4, capacity: None };
        s.handle(Input::RunnerUp(Box::new(r)), 0);
    }
    let mut lat: Vec<f64> = Vec::new();
    let mut running: VecDeque<String> = VecDeque::new();
    let mut assigned = 0usize;
    let handle = |s: &mut Scheduler, i: Input, now: i64, running: &mut VecDeque<String>, assigned: &mut usize, lat: &mut Vec<f64>| {
        let t = Instant::now();
        let out = s.handle(i, now);
        lat.push(t.elapsed().as_secs_f64());
        for c in out {
            if let Command::Assign { leg, .. } = c {
                *assigned += 1;
                running.push_back(leg);
            }
        }
    };
    let start = Instant::now();
    for j in 0..10_000 {
        let l = LegIn { id: format!("l{j}"), run: format!("run{}", j / 10), repo: format!("acme/r{}", j % 7), route: LegRoute { labels: vec![format!("pool-{}", j % 50)], ..LegRoute::default() }, priority: 60, ..LegIn::default() };
        handle(&mut s, Input::LegQueued(Box::new(l)), 1, &mut running, &mut assigned, &mut lat);
    }
    // Completions free the slots; each lets the next queued leg in.
    let mut now = 2;
    while let Some(leg) = running.pop_front() {
        now += 1;
        handle(&mut s, Input::AttemptFinished { leg, attempt: 1, outcome: Outcome::Success }, now, &mut running, &mut assigned, &mut lat);
    }
    let secs = start.elapsed().as_secs_f64();
    assert_eq!(assigned, 10_000);
    let rate = assigned as f64 / secs;
    lat.sort_by(f64::total_cmp);
    let p99 = lat.get(lat.len() * 99 / 100).copied().unwrap_or(0.0);
    eprintln!("scheduler: {assigned} assignments in {secs:.3} s = {rate:.0}/s; p99 input {:.3} ms", p99 * 1000.0);
    assert!(rate >= 2_000.0, "{rate:.0} assignments per second");
    assert!(p99 < 0.050, "p99 {:.1} ms", p99 * 1000.0);
}
