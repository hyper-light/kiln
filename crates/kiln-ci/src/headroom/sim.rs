//! A deterministic simulation of one machine (docs/headroom.md, "Simulation"): headroom v2's
//! engine against AIMD with first-come-first-served admission, and against no control, over the
//! same traces. Virtual time only, a seeded generator only.
//!
//! The machine is a fluid model of the 2026-10-26 mechanism: work writes into the page cache, the
//! disk drains it at its capacity, and writers are throttled only once the dirty backlog reaches
//! its limit (as Linux's dirty-page throttling does). A sync waits behind the whole backlog, so
//! its latency is the backlog over the disk's capacity: what hung Docker and the node that day.
//! The numbers below describe the simulated machine and its work (adacachy-like: 32 cores,
//! 128 GB, a 1 GB/s disk); none of them is given to the engine.

use super::*;

const TOLERANCE_US: f64 = 1e6;
/// Each step of virtual time.
const DT: f64 = 0.25;
/// The machine's calm history before the trace begins (what a runner learned before that day).
const HISTORY_S: f64 = 3600.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Kind {
    key: &'static str,
    write_bps: f64,
    cpu: f64,
    mem: f64,
    /// Its run at full speed.
    secs: f64,
}

/// A workspace gate: links writing 1.3-2 GB each, about 60 GB in all.
const GATE: Kind = Kind { key: "gate", write_bps: 400e6, cpu: 4.0, mem: 8e9, secs: 150.0 };
/// An agent's turn: a little CPU, a few writes.
const TURN: Kind = Kind { key: "turn", write_bps: 5e6, cpu: 1.0, mem: 1e9, secs: 60.0 };

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        (0..12).map(|_| self.next()).sum::<f64>() - 6.0
    }

    fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next()).ln()
    }
}

#[derive(Clone, Debug)]
struct Trace {
    name: &'static str,
    arrivals: Vec<(f64, Kind)>,
    /// (when, the disk's capacity from then).
    disk: Vec<(f64, f64)>,
}

fn crash() -> Trace {
    // Five gates at once; then the runner, restarted, resumes 47 sessions and their gates at once.
    let mut arrivals = vec![(0.0, GATE); 5];
    arrivals.extend(vec![(600.0, GATE); 47]);
    Trace { name: "10-26 crash", arrivals, disk: vec![(0.0, 1e9)] }
}

fn steady(seed: u64, name: &'static str, disk: Vec<(f64, f64)>) -> Trace {
    let mut rng = Rng(seed);
    let mut arrivals = Vec::new();
    // Gates at 80% of the disk: 60 GB each at 0.8 GB/s is one every 75 s; a turn every 30 s.
    for (kind, mean) in [(GATE, 75.0), (TURN, 30.0)] {
        let mut t = rng.exp(mean);
        while t < 7200.0 {
            arrivals.push((t, kind));
            t += rng.exp(mean);
        }
    }
    arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
    Trace { name, arrivals, disk }
}

fn bursts() -> Trace {
    let mut arrivals = Vec::new();
    for b in 0..4 {
        let t = f64::from(b) * 1200.0;
        arrivals.extend(vec![(t, GATE); 8]);
        arrivals.extend(vec![(t, TURN); 10]);
    }
    Trace { name: "bursts", arrivals, disk: vec![(0.0, 1e9)] }
}

#[derive(Clone, Debug)]
struct Job {
    id: String,
    kind: Kind,
    arrive: f64,
    start: Option<f64>,
    done: Option<f64>,
    /// Seconds of full-speed work left.
    left: f64,
    share: f64,
    /// As last measured.
    write_bps: f64,
    cpu: f64,
}

struct Machine {
    disk_bps: f64,
    dirty_limit: f64,
    cores: f64,
    mem: f64,
    backlog: f64,
    noise: f64,
    rng: Rng,
}

impl Machine {
    /// How long a sync started now takes: its own few milliseconds, then the backlog ahead of it.
    fn sync_s(&mut self) -> f64 {
        // Calm noise: autocorrelated (AR(1)) around 2 ms.
        self.noise = 0.9 * self.noise + 0.3 * (1.0f64 - 0.81).sqrt() * self.rng.normal();
        0.002 * self.noise.exp() + self.backlog / self.disk_bps
    }

    fn sync_now_s(&self) -> f64 {
        0.002 + self.backlog / self.disk_bps
    }

    /// One step of the running jobs at their shares; returns the disk's delivered rate.
    fn step(&mut self, jobs: &mut [Job]) -> f64 {
        let running: Vec<usize> = jobs.iter().enumerate().filter(|(_, j)| j.start.is_some() && j.done.is_none()).map(|(i, _)| i).collect();
        let want_cpu: f64 = running.iter().filter_map(|i| jobs.get(*i)).map(|j| j.kind.cpu * j.share).sum();
        let cpu_scale = if want_cpu > self.cores { self.cores / want_cpu } else { 1.0 };
        let incoming: f64 = running.iter().filter_map(|i| jobs.get(*i)).map(|j| j.kind.write_bps * j.share * cpu_scale).sum::<f64>() * DT;
        // Writers are throttled once the backlog reaches its limit: what fits is what drains.
        let room = (self.dirty_limit - self.backlog).max(0.0) + self.disk_bps * DT;
        let disk_scale = if incoming > room { room / incoming } else { 1.0 };
        for i in &running {
            if let Some(j) = jobs.get_mut(*i) {
                let scale = j.share * cpu_scale * disk_scale;
                j.left -= DT * scale;
                j.write_bps = j.kind.write_bps * scale;
                j.cpu = j.kind.cpu * j.share * cpu_scale;
            }
        }
        let drained = (self.backlog + incoming * disk_scale).min(self.disk_bps * DT);
        self.backlog = (self.backlog + incoming * disk_scale - drained).max(0.0);
        drained / DT
    }
}

enum Control {
    None,
    /// A window of concurrent jobs: +1 for each calm probe, halved when one passes the tolerance.
    Aimd { window: f64 },
    V2(Box<Engine>),
}

impl Control {
    fn name(&self) -> &'static str {
        match self {
            Control::None => "none",
            Control::Aimd { .. } => "AIMD+FCFS",
            Control::V2(_) => "v2",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Report {
    trace: &'static str,
    control: &'static str,
    jobs: usize,
    done: usize,
    mean_wait_s: f64,
    p99_wait_s: f64,
    mean_flow_s: f64,
    makespan_s: f64,
    peak_sync_s: f64,
    /// Time the machine's sync latency was over the tolerance.
    over_s: f64,
    /// Of it, the time the controller had already detected trouble (v2: an alarm up since an
    /// earlier step; the others detect nothing).
    over_detected_s: f64,
    /// Full-speed seconds of work done by each 10 s of the trace.
    work: Vec<f64>,
    /// While work remained: the disk's mean utilization, and the mean number of jobs running.
    busy: f64,
    running: f64,
}

impl Report {
    fn work_by(&self, t: f64) -> f64 {
        let i = (t / 10.0) as usize;
        self.work.get(i).or(self.work.last()).copied().unwrap_or(0.0)
    }
}

fn run(trace: &Trace, mut control: Control, seed: u64) -> Report {
    let mut m = Machine { disk_bps: trace.disk.first().map_or(1e9, |d| d.1), dirty_limit: 10e9, cores: 32.0, mem: 128e9, backlog: 0.0, noise: 0.0, rng: Rng(seed) };
    let mut jobs: Vec<Job> =
        trace.arrivals.iter().enumerate().map(|(i, (t, k))| Job { id: format!("j{i}"), kind: *k, arrive: *t, start: None, done: None, left: k.secs, share: 1.0, write_bps: 0.0, cpu: 0.0 }).collect();
    let total_work: f64 = jobs.iter().map(|j| j.kind.secs).sum();
    let us = |t: f64| ((t + HISTORY_S) * 1e6) as u64;

    let mut probe_every = 1.0;
    let mut probe_out: Option<(f64, f64)> = None; // (started, takes)
    let mut next_probe = -HISTORY_S;
    let mut answers: Vec<(String, Answer)> = Vec::new();
    let mut delivered = 0.0;
    let (mut peak, mut over) = (0.0f64, 0.0);
    let (mut busy, mut ran, mut steps) = (0.0, 0.0, 0.0);
    let (mut over_detected, mut detected) = (0.0, false);
    let mut work = Vec::new();
    let mut t = -HISTORY_S;
    let cap = 6.0 * 3600.0;
    while t < cap && jobs.iter().any(|j| j.done.is_none()) {
        for (at, bps) in &trace.disk {
            if (*at - t).abs() < DT / 2.0 {
                m.disk_bps = *bps;
            }
        }
        // The probe: a sync of a small file, out until the backlog ahead of it is written.
        let mut answered = None;
        if let Some((started, takes)) = probe_out
            && t >= started + takes
        {
            answers.push(("disk".into(), Answer::Took(takes * 1e6)));
            answered = Some(takes);
            probe_out = None;
            next_probe = t + probe_every;
        }
        if probe_out.is_none() && t >= next_probe {
            probe_out = Some((t, m.sync_s()));
        }

        // Control.
        let queued: Vec<usize> = jobs.iter().enumerate().filter(|(_, j)| j.arrive <= t && j.start.is_none()).map(|(i, _)| i).collect();
        let running = jobs.iter().filter(|j| j.start.is_some() && j.done.is_none()).count();
        match &mut control {
            Control::None => {
                for i in queued {
                    if let Some(j) = jobs.get_mut(i) {
                        j.start = Some(t);
                    }
                }
                answers.clear();
            }
            Control::Aimd { window } => {
                // Grown only while it limits (as TCP's window is: RFC 7661), else a calm hour opens it
                // wide before any work comes.
                if let Some(took) = answered {
                    if took * 1e6 > TOLERANCE_US {
                        *window = (*window / 2.0).max(1.0);
                    } else if running as f64 >= window.floor() {
                        *window += 1.0;
                    }
                }
                let room = (window.floor() as usize).saturating_sub(running);
                for i in queued.into_iter().take(room) {
                    if let Some(j) = jobs.get_mut(i) {
                        j.start = Some(t);
                    }
                }
                answers.clear();
            }
            Control::V2(e) => {
                let obs = Observation {
                    now_us: us(t),
                    answers: std::mem::take(&mut answers),
                    inflight: probe_out.map(|(s, _)| vec![("disk".to_owned(), (t - s) * 1e6)]).unwrap_or_default(),
                    delivered_bps: Some(delivered),
                    mem_free: m.mem - jobs.iter().filter(|j| j.start.is_some() && j.done.is_none()).map(|j| j.kind.mem).sum::<f64>(),
                    running: jobs
                        .iter()
                        .filter(|j| j.start.is_some() && j.done.is_none())
                        .map(|j| Group { id: j.id.clone(), key: j.kind.key.into(), class: Class::Agent, protected: false, write_bps: j.write_bps, cpu_cores: j.cpu, mem_bytes: j.kind.mem, share: j.share })
                        .collect(),
                    waiting: queued.iter().filter_map(|i| jobs.get(*i)).map(|j| Waiting { id: j.id.clone(), key: j.kind.key.into(), class: Class::Agent, since_us: us(j.arrive) }).collect(),
                    ended: Vec::new(),
                };
                let plan = e.step(&obs);
                // Trouble known from this step on (counted from the next).
                let alarmed = plan.status.targets.iter().any(|t| t.alarm.is_some());
                if std::env::var_os("KILN_SIM_TRACE").is_some() && (t >= 0.0 && (t % 20.0 < DT || !plan.events.is_empty())) {
                    let run: Vec<String> = jobs.iter().filter(|j| j.start.is_some() && j.done.is_none()).map(|j| format!("{:.2}", j.share)).collect();
                    eprintln!(
                        "t={t:.1} backlog={:.1}GB budget={:?} safe={:?} unsafe={:?} cpu={:?} lag={:?} ours={:.0}MB/s q={} run={:?} ev={:?}",
                        m.backlog / 1e9,
                        plan.status.budget_bps.map(|b| (b / 1e6).round()),
                        plan.status.safe_bps.map(|b| (b / 1e6).round()),
                        plan.status.unsafe_bps.map(|b| (b / 1e6).round()),
                        plan.status.cpu_budget,
                        plan.status.lag_us.map(|l| (l / 1e3).round()),
                        obs.running.iter().map(|g| g.write_bps).sum::<f64>() / 1e6,
                        obs.waiting.len(),
                        run,
                        plan.events
                    );
                }
                if let Some(every) = plan.probe_every_us.get("disk") {
                    probe_every = every / 1e6;
                }
                for d in plan.decisions.iter().filter(|d| d.start) {
                    if let Some(j) = jobs.iter_mut().find(|j| j.id == d.id) {
                        j.start = Some(t);
                    }
                }
                for j in &mut jobs {
                    j.share = plan.shares.iter().find(|p| p.id == j.id).map_or(1.0, |p| p.share);
                }
                let was = detected;
                detected = alarmed;
                if was && m.sync_now_s() * 1e6 > TOLERANCE_US && t >= 0.0 {
                    over_detected += DT;
                }
            }
        }

        delivered = m.step(&mut jobs);
        for j in &mut jobs {
            if j.start.is_some() && j.done.is_none() && j.left <= 0.0 {
                j.done = Some(t + DT);
            }
        }
        if t >= 0.0 {
            steps += 1.0;
            busy += delivered / m.disk_bps;
            ran += jobs.iter().filter(|j| j.start.is_some() && j.done.is_none()).count() as f64;
            let s = m.sync_now_s();
            peak = peak.max(s);
            if s * 1e6 > TOLERANCE_US {
                over += DT;
            }
            let i = (t / 10.0) as usize;
            if work.len() <= i {
                work.push(jobs.iter().map(|j| j.kind.secs - j.left.max(0.0)).sum::<f64>());
            }
        }
        t += DT;
    }
    work.push(total_work - jobs.iter().map(|j| j.left.max(0.0)).sum::<f64>());

    let mut waits: Vec<f64> = jobs.iter().filter_map(|j| j.start.map(|s| s - j.arrive)).collect();
    waits.sort_by(f64::total_cmp);
    let flows: Vec<f64> = jobs.iter().filter_map(|j| j.done.map(|d| d - j.arrive)).collect();
    let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    Report {
        trace: trace.name,
        control: control.name(),
        jobs: jobs.len(),
        done: jobs.iter().filter(|j| j.done.is_some()).count(),
        mean_wait_s: mean(&waits),
        p99_wait_s: waits.get(((waits.len() as f64 * 0.99).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0.0),
        mean_flow_s: mean(&flows),
        makespan_s: jobs.iter().filter_map(|j| j.done).fold(0.0, f64::max),
        peak_sync_s: peak,
        over_s: over,
        over_detected_s: over_detected,
        work,
        busy: busy / f64::max(steps, 1.0),
        running: ran / f64::max(steps, 1.0),
    }
}

fn v2() -> Control {
    Control::V2(Box::new(Engine::new(
        Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 },
        vec![Target { name: "disk".into(), tolerance_us: TOLERANCE_US, served: false }],
        Learned::default(),
        0,
    )))
}

fn traces(seed: u64) -> Vec<Trace> {
    vec![crash(), steady(seed, "steady 80%", vec![(0.0, 1e9)]), bursts(), steady(seed, "disk halves at 30 min", vec![(0.0, 1e9), (1800.0, 0.5e9)])]
}

fn row(r: &Report) -> String {
    format!(
        "| {} | {} | {}/{} | {:.0} | {:.0} | {:.0} | {:.0} | {:.2} | {:.0} |",
        r.trace, r.control, r.done, r.jobs, r.mean_wait_s, r.p99_wait_s, r.mean_flow_s, r.makespan_s, r.peak_sync_s, r.over_s
    )
}

#[test]
fn the_same_seed_runs_the_same_and_no_control_hangs_on_the_crash() {
    let a = run(&crash(), Control::None, 1);
    let b = run(&crash(), Control::None, 1);
    assert_eq!(a, b);
    assert!(a.peak_sync_s * 1e6 > TOLERANCE_US, "{}", row(&a));
}

#[test]
fn every_control_finishes_the_steady_trace() {
    for c in [Control::None, Control::Aimd { window: 1.0 }, v2()] {
        let r = run(&steady(7, "steady 80%", vec![(0.0, 1e9)]), c, 7);
        assert_eq!(r.done, r.jobs, "{}", row(&r));
    }
}

/// The comparison docs/headroom.md records ("Simulation"): its table is this test's output
/// (`cargo test -p kiln-ci headroom::sim::compare -- --nocapture`). On every trace and seed, v2
/// does as much work as AIMD+FCFS by the time the first of them finishes, with a lower mean wait,
/// less time over the tolerance and a lower peak, and none at all once it has detected trouble.
#[test]
fn compare() {
    let mut table = vec![
        "| trace | control | done | mean wait s | p99 wait s | mean flow s | makespan s | peak sync s | s over 1 s |".to_owned(),
        "|---|---|---|---|---|---|---|---|---|".to_owned(),
    ];
    let mut failures = Vec::new();
    for seed in [1u64, 2, 3] {
        for trace in traces(seed) {
            let none = run(&trace, Control::None, seed);
            let aimd = run(&trace, Control::Aimd { window: 1.0 }, seed);
            let new = run(&trace, v2(), seed);
            if seed == 1 {
                table.extend([row(&none), row(&aimd), row(&new)]);
            }
            let h = new.makespan_s.min(aimd.makespan_s);
            println!(
                "seed {seed} {}: over after detection v2 {:.2} | util v2 {:.3} AIMD {:.3} | running v2 {:.1} AIMD {:.1} | work by {h:.0} s v2/AIMD {:.4} | mean wait v2 {:.0} AIMD {:.0} | mean flow v2 {:.0} AIMD {:.0} | makespan v2 {:.0} AIMD {:.0} | s over v2 {:.0} AIMD {:.0}",
                trace.name, new.over_detected_s, new.busy, aimd.busy, new.running, aimd.running, new.work_by(h) / aimd.work_by(h), new.mean_wait_s, aimd.mean_wait_s, new.mean_flow_s, aimd.mean_flow_s, new.makespan_s, aimd.makespan_s, new.over_s, aimd.over_s
            );
            for r in [&none, &aimd, &new] {
                if r.done != r.jobs {
                    failures.push(format!("{} seed {seed}: {} finished {}/{}", trace.name, r.control, r.done, r.jobs));
                }
            }
            if new.over_s >= aimd.over_s || new.peak_sync_s >= aimd.peak_sync_s {
                failures.push(format!("{} seed {seed}: v2 {:.0} s over the tolerance (peak {:.2} s), AIMD {:.0} s (peak {:.2} s)", trace.name, new.over_s, new.peak_sync_s, aimd.over_s, aimd.peak_sync_s));
            }
            if new.over_detected_s > 0.0 {
                failures.push(format!("{} seed {seed}: v2 over the tolerance {:.2} s after it detected trouble", trace.name, new.over_detected_s));
            }
            if new.work_by(h) < aimd.work_by(h) {
                failures.push(format!("{} seed {seed}: v2 did {:.0} s of work by {h:.0} s, AIMD {:.0}", trace.name, new.work_by(h), aimd.work_by(h)));
            }
            if new.mean_wait_s >= aimd.mean_wait_s {
                failures.push(format!("{} seed {seed}: v2 mean wait {:.0} s, AIMD {:.0}", trace.name, new.mean_wait_s, aimd.mean_wait_s));
            }
        }
    }
    println!("{}", table.join("\n"));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "a trace to read: KILN_SIM_TRACE=1 cargo test -p kiln-ci headroom::sim::trace -- --ignored --nocapture"]
fn trace() {
    let name = std::env::var("KILN_SIM_TRACE_NAME").unwrap_or_default();
    for t in traces(1).into_iter().filter(|t| name.is_empty() || t.name == name) {
        eprintln!("{}", row(&run(&t, v2(), 1)));
    }
}
