//! Placement's decision under the trace every competitor's selection kernel runs (LB00, LB14;
//! `ci/bench/README.md`; `ci/bench/drivers/common.h` is the same trace in C for nginx, HAProxy and
//! Envoy). Not a test: `ci/bench/run.sh` runs it pinned, in release, beside the competitors.
//!
//! N runners of C CPUs each, one owner; K = round(rho * N * C) runs in flight. Each step a run
//! chosen uniformly at random ends (the same splitmix64 departures as the C drivers), then a new
//! run asking for one CPU (d = 1; d = 2 adds memory, d = 4 disk and a GPU: "no equivalent" in any
//! competitor) is placed: the owner probes two runners drawn at random (`place::owner::PROBES`),
//! each runner's admission answers, the answers reach the owner in an order drawn from a second
//! stream (the network's), the emptiest offer is taken, the other declined, and the run starts, as
//! kiln's shell carries `place` out. A run both runners hold full is queued there, and starts when
//! one of them has room (counted `refused`: not started within its decision). Messages are routed
//! in process: no network, no serialization (`ci/bench/e2e` measures those).
//!
//! `-a local` is the owner-local path: the asking node's own runner, the only one in its
//! directory, admits its own work (N = 1; what's timed is the owner's calls and its own runner's).
//!
//! What a decision costs, three ways, each the sum of the calls into `place` it took, the timer's
//! own cost taken off every interval:
//!
//! - `place.critical`: the owner's work and the chosen runner's: what lies on the decision's path
//!   when the other runners answer in parallel on their own machines (the like-for-like with a
//!   proxy's pick, which runs on one CPU);
//! - `place.owner`: the owner's work alone: what bounds one asking node's decisions a second;
//! - `place.total`: every runner's work as well: the CPU the whole fleet spends on one decision.
//!
//! Quality, as the C drivers count it: oversubscribed (a run placed on a runner already holding C
//! CPUs of runs), refused (a run not started by the end of its decision), cv and peak of the
//! runs in flight per runner. And the messages a decision sent, beside them.

use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::{ToOwner, ToRunner};
use kiln_ci::place::owner::{Act, Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Records, Said, Settings};
use kiln_ci::sched::needs::Needs;
use kiln_ci::sched::state::{Cpus, Resources};

const GIB: u64 = 1 << 30;
const OWNER: &str = "owner";

struct Args {
    algo: String,
    n: u32,
    c: u32,
    d: u32,
    rho: f64,
    steps: u64,
    seed: u64,
    out: Option<String>,
    /// Print each call's mean time by its kind (owner and runner), to stderr.
    profile: bool,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { algo: "place".into(), n: 0, c: 8, d: 1, rho: 0.0, steps: 200_000, seed: 1, out: None, profile: false };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.chunks(2);
    for pair in &mut it {
        let (Some(k), Some(v)) = (pair.first(), pair.get(1)) else { return Err(format!("{pair:?}: a flag without a value")) };
        let bad = |e: &dyn std::fmt::Display| format!("{k} {v}: {e}");
        match k.as_str() {
            "-a" => a.algo = v.clone(),
            "-n" => a.n = v.parse().map_err(|e| bad(&e))?,
            "-c" => a.c = v.parse().map_err(|e| bad(&e))?,
            "-d" => a.d = v.parse().map_err(|e| bad(&e))?,
            "-r" => a.rho = v.parse().map_err(|e| bad(&e))?,
            "-s" => a.steps = v.parse().map_err(|e| bad(&e))?,
            "-S" => a.seed = v.parse().map_err(|e| bad(&e))?,
            "-o" => a.out = Some(v.clone()),
            "-P" => a.profile = v == "1",
            // cargo bench passes --bench; nothing else is ours.
            "--bench" => {}
            _ => return Err(format!("unknown flag {k}")),
        }
    }
    if a.algo == "local" {
        a.n = 1;
    }
    if (a.n < 2 && a.algo != "local") || a.c < 1 || !(0.0..=1.0).contains(&a.rho) || ![1, 2, 4].contains(&a.d) || !["place", "local"].contains(&a.algo.as_str()) {
        return Err("usage: -a place|local -n N [-c C] [-d 1|2|4] [-r rho in [0,1]] [-s steps] [-S seed] [-o raw]".into());
    }
    Ok(a)
}

/// The C drivers' splitmix64, so the departures match theirs draw for draw.
fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The timer's own cost: the median of back-to-back reads, as `kb_overhead` measures it.
fn overhead() -> u64 {
    let mut d: Vec<u64> = (0..100_001)
        .map(|_| {
            let a = Instant::now();
            let b = Instant::now();
            u64::try_from(b.duration_since(a).as_nanos()).unwrap_or(u64::MAX)
        })
        .collect();
    d.sort_unstable();
    d.get(d.len() / 2).copied().unwrap_or(0)
}

/// `kb_quality`: runs in flight per runner, their sum of squares and the runners at each count.
struct Quality {
    n: u32,
    c: u32,
    max: u32,
    load: Vec<u32>,
    level: Vec<u32>,
    total: u64,
    sumsq: u64,
    cv: f64,
    peak: f64,
    samples: u64,
    over: u64,
    refused: u64,
}

impl Quality {
    fn new(n: u32, c: u32, k: u64) -> Quality {
        let mut level = vec![0u32; usize::try_from(k).unwrap_or(0) + 2];
        if let Some(l) = level.first_mut() {
            *l = n;
        }
        Quality { n, c, max: 0, load: vec![0; usize::try_from(n).unwrap_or(0)], level, total: 0, sumsq: 0, cv: 0.0, peak: 0.0, samples: 0, over: 0, refused: 0 }
    }

    fn bump(&mut self, at: u32, by: i64) {
        if let Some(v) = self.level.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
            *v = u32::try_from(i64::from(*v) + by).unwrap_or(0);
        } else if by > 0 {
            self.level.resize(usize::try_from(at).unwrap_or(0) + 1, 0);
            self.bump(at, by);
        }
    }

    fn add(&mut self, b: usize) {
        let Some(l) = self.load.get(b).copied() else { return };
        if l >= self.c {
            self.over += 1;
        }
        self.bump(l, -1);
        self.bump(l + 1, 1);
        if let Some(v) = self.load.get_mut(b) {
            *v = l + 1;
        }
        self.total += 1;
        self.sumsq += 2 * u64::from(l) + 1;
        self.max = self.max.max(l + 1);
    }

    fn drop(&mut self, b: usize) {
        let Some(l) = self.load.get(b).copied() else { return };
        if l == 0 {
            return;
        }
        self.bump(l, -1);
        self.bump(l - 1, 1);
        if let Some(v) = self.load.get_mut(b) {
            *v = l - 1;
        }
        self.total -= 1;
        self.sumsq -= 2 * u64::from(l) - 1;
        while self.max > 0 && self.level.get(usize::try_from(self.max).unwrap_or(usize::MAX)).copied().unwrap_or(0) == 0 {
            self.max -= 1;
        }
    }

    fn sample(&mut self) {
        if self.total == 0 {
            return;
        }
        let mean = self.total as f64 / f64::from(self.n);
        let var = self.sumsq as f64 / f64::from(self.n) - mean * mean;
        self.cv += var.max(0.0).sqrt() / mean;
        self.peak += f64::from(self.max) / mean;
        self.samples += 1;
    }
}

/// The fleet: one owner, N runners' admissions, and the clock each decision is timed by.
struct Fleet {
    owner: Owner,
    runners: Vec<Admission>,
    index: HashMap<String, usize>,
    names: Vec<String>,
    overhead: u64,
    /// The decision under way: the owner's time, each runner's time, the messages sent.
    owner_ns: u64,
    runner_ns: Vec<u64>,
    messages: u64,
    net: u64,
    /// Runs started since last looked at: their runner and run.
    starts: Vec<(usize, String)>,
    /// With `-P 1`: each kind of call's count and total time, and the kind of the next.
    prof: std::collections::BTreeMap<&'static str, (u64, u64)>,
    kind: &'static str,
}

fn to_owner_kind(m: &ToOwner) -> &'static str {
    match m {
        ToOwner::Offer { .. } => "owner.heard offer",
        ToOwner::Waiting { .. } => "owner.heard waiting",
        ToOwner::Never { .. } => "owner.heard never",
        ToOwner::Held { .. } => "owner.heard held",
        ToOwner::Lapsed { .. } => "owner.heard lapsed",
        ToOwner::Started { .. } => "owner.heard started",
        ToOwner::Ran { .. } => "owner.heard ran",
        ToOwner::Ended { .. } => "owner.heard ended",
    }
}

fn to_runner_kind(m: &ToRunner) -> &'static str {
    match m {
        ToRunner::Probe { .. } => "runner.probe",
        ToRunner::Withdraw { .. } => "runner.withdraw",
        ToRunner::Take { .. } => "runner.take",
        ToRunner::Decline { .. } => "runner.decline",
        ToRunner::Start { .. } => "runner.start (restart)",
        ToRunner::Ack { .. } => "runner.ack",
    }
}

impl Fleet {
    fn new(a: &Args) -> Fleet {
        let size = match a.d {
            1 => Resources { cpu: Cpus::whole(u64::from(a.c)), ..Resources::default() },
            2 => Resources { cpu: Cpus::whole(u64::from(a.c)), mem: u64::from(a.c) * 4 * GIB, ..Resources::default() },
            _ => Resources { cpu: Cpus::whole(u64::from(a.c)), mem: u64::from(a.c) * 4 * GIB, disk: u64::from(a.c) * 50 * GIB, gpu: a.c },
        };
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let names: Vec<String> = if a.algo == "local" { vec![OWNER.to_owned()] } else { (0..a.n).map(|j| format!("r{j}")).collect() };
        let runners = names
            .iter()
            .map(|name| {
                let init = Init {
                    me: name.clone(),
                    epoch: 1,
                    facts: facts.clone(),
                    labels: Vec::new(),
                    sample: Sample { size, ..Sample::default() },
                    records: Records::new(),
                    // Settings as the person's defaults are: no budget, no overcommit, backfill on.
                    settings: Settings { budget: None, overcommit_cpu: 1000, backfill: true, weights: Default::default(), ..Settings::default() },
                };
                Admission::new(init, 0)
            })
            .collect();
        let mut owner = Owner::seeded(OWNER, 1, a.seed ^ 0x6f77_6e65_7221);
        let machines = names.iter().map(|name| Machine { name: name.clone(), facts: facts.clone(), labels: Vec::new(), size: Size::known(&size), epoch: 1, connected: true, places: true, idle: None }).collect();
        let _ = owner.machines(machines);
        let index = names.iter().cloned().enumerate().map(|(i, n)| (n, i)).collect();
        Fleet { owner, runners, index, names, overhead: overhead(), owner_ns: 0, runner_ns: vec![0; usize::try_from(a.n).unwrap_or(0)], messages: 0, net: a.seed ^ 0x6e65_7477_6f72_6b21, starts: Vec::new(), prof: Default::default(), kind: "" }
    }

    fn less(&self, t: Instant) -> u64 {
        u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX).saturating_sub(self.overhead)
    }

    fn owner_does(&mut self, f: impl FnOnce(&mut Owner) -> Vec<Act>) -> Vec<Act> {
        let t = Instant::now();
        let acts = f(&mut self.owner);
        let dt = self.less(t);
        self.owner_ns += dt;
        self.count(dt);
        acts
    }

    fn count(&mut self, dt: u64) {
        let e = self.prof.entry(self.kind).or_default();
        e.0 += 1;
        e.1 += dt;
    }

    fn runner_does<R>(&mut self, i: usize, f: impl FnOnce(&mut Admission) -> R) -> Option<R> {
        let r = self.runners.get_mut(i)?;
        let t = Instant::now();
        let out = f(r);
        let dt = u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX).saturating_sub(self.overhead);
        if let Some(ns) = self.runner_ns.get_mut(i) {
            *ns += dt;
        }
        self.count(dt);
        Some(out)
    }

    /// Carry out what the owner said until nothing more is said, each run started kept in
    /// `starts`. What runners say back reaches the owner in the network's order: each batch of
    /// replies is shuffled before it is heard.
    fn carry(&mut self, acts: Vec<Act>, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        let mut acts = acts;
        loop {
            let mut replies: Vec<(usize, ToOwner)> = Vec::new();
            for act in acts.drain(..) {
                match act {
                    Act::Tell { runner, msg } => {
                        let Some(&i) = self.index.get(&runner) else { continue };
                        self.messages += 1;
                        let mut said: Vec<Said> = Vec::new();
                        self.kind = to_runner_kind(&msg);
                        self.runner_does(i, |r| match msg {
                            ToRunner::Probe { ask } => r.probe(peer, ask, None, now, &mut said),
                            ToRunner::Withdraw { run } => r.withdraw(peer, &run, now, &mut said),
                            ToRunner::Take { run, fence } => r.take(peer, &run, fence, now, &mut said),
                            ToRunner::Decline { run, fence } => r.decline(peer, &run, fence, now, &mut said),
                            ToRunner::Start { run, fence } => {
                                let _ = r.start(peer, &run, fence, now, &mut said);
                            }
                            ToRunner::Ack { run } => r.ack(peer, &run),
                        });
                        replies.extend(said.into_iter().map(|(_, m)| (i, m)));
                    }
                    Act::Go { run, runner, fence } => {
                        let Some(&i) = self.index.get(&runner) else { continue };
                        let t = Instant::now();
                        let go = self.owner.starting(&run, &runner, fence);
                        let dt = self.less(t);
                        self.owner_ns += dt;
                        self.kind = "owner.starting";
                        self.count(dt);
                        if go {
                            self.messages += 1;
                            let mut said: Vec<Said> = Vec::new();
                            self.kind = "runner.start";
                            let _ = self.runner_does(i, |r| r.start(peer, &run, fence, now, &mut said));
                            self.starts.push((i, run));
                            replies.extend(said.into_iter().map(|(_, m)| (i, m)));
                        }
                    }
                    Act::Answer { .. } | Act::Fallback { .. } | Act::Restart { .. } => {}
                }
            }
            if replies.is_empty() {
                return;
            }
            // Fisher-Yates over the network's stream.
            for j in (1..replies.len()).rev() {
                let k = usize::try_from(splitmix(&mut self.net) % (j as u64 + 1)).unwrap_or(0);
                replies.swap(j, k);
            }
            for (i, m) in replies {
                self.messages += 1;
                let name = self.names.get(i).cloned().unwrap_or_default();
                self.kind = to_owner_kind(&m);
                let more = self.owner_does(|o| o.heard(&name, m, now));
                acts.extend(more);
            }
        }
    }

    fn reset(&mut self) {
        self.owner_ns = 0;
        self.runner_ns.iter_mut().for_each(|v| *v = 0);
        self.messages = 0;
    }
}

fn ask(run: u64, d: u32) -> Ask {
    let mut request = Resources { cpu: Cpus::whole(1), ..Resources::default() };
    if d >= 2 {
        request.mem = 4 * GIB;
    }
    if d >= 4 {
        request.disk = 50 * GIB;
        request.gpu = 0;
    }
    Ask { run: format!("b{run}"), needs: Needs { request, ..Needs::default() }, repo: "repo".into(), actor: OWNER.into(), ..Ask::default() }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    sorted.get(sorted.len().saturating_sub(1) * p / 100).copied().unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn report(a: &Args, which: &str, k: u64, overhead: u64, ns: &[u64], q: &Quality, messages: f64, raw: Option<String>) {
    if let Some(path) = raw
        && let Ok(mut f) = std::fs::File::create(&path)
    {
        let bytes: Vec<u8> = ns.iter().flat_map(|v| u32::try_from(*v).unwrap_or(u32::MAX).to_le_bytes()).collect();
        let _ = f.write_all(&bytes);
    }
    let mut s = ns.to_vec();
    s.sort_unstable();
    let sum: u64 = s.iter().sum();
    let mean = if s.is_empty() { 0.0 } else { sum as f64 / s.len() as f64 };
    let per_s = if mean > 0.0 { 1e9 / mean } else { 0.0 };
    let (cv, peak) = if q.samples > 0 { (q.cv / q.samples as f64, q.peak / q.samples as f64) } else { (0.0, 0.0) };
    println!(
        "{{\"who\":\"kiln-{}\",\"algo\":\"{}.{which}\",\"n\":{},\"c\":{},\"rho\":{:.3},\"d\":{},\"k\":{k},\"steps\":{},\"seed\":{},\"overhead_ns\":{overhead},\"p50_ns\":{},\"p99_ns\":{},\"mean_ns\":{mean:.2},\"per_s\":{per_s:.0},\"over\":{},\"refused\":{},\"cv\":{cv:.5},\"peak\":{peak:.5},\"messages\":{messages:.1}}}",
        env!("CARGO_PKG_VERSION"),
        if a.algo == "local" { "local" } else { "place" },
        a.n,
        a.c,
        a.rho,
        a.d,
        s.len(),
        a.seed,
        percentile(&s, 50),
        percentile(&s, 99),
        q.over,
        q.refused,
    );
}

fn main() -> ExitCode {
    let a = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("lb_vs: {e}");
            return ExitCode::from(2);
        }
    };
    let k = (a.rho * f64::from(a.n) * f64::from(a.c)).round() as u64;
    let mut fleet = Fleet::new(&a);
    let mut q = Quality::new(a.n, a.c, k + 1);
    let peer = Peer { name: OWNER, epoch: 1 };
    // In flight: (runner, run).
    let mut live: Vec<(usize, String)> = Vec::with_capacity(usize::try_from(k).unwrap_or(0) + 1);
    let (mut seed, mut next, mut now) = (a.seed, 0u64, 0i64);
    // Each run started (the one asked now, or one queued before that a departure let start): in
    // flight, counted where it went; the runner the run asked now started on, if it did.
    let settle = |fleet: &mut Fleet, q: &mut Quality, live: &mut Vec<(usize, String)>, asked: Option<&str>| -> Option<usize> {
        let mut placed = None;
        for (i, run) in std::mem::take(&mut fleet.starts) {
            q.add(i);
            if asked == Some(run.as_str()) {
                placed = Some(i);
            }
            live.push((i, run));
        }
        placed
    };
    let place = |fleet: &mut Fleet, q: &mut Quality, live: &mut Vec<(usize, String)>, now: i64, next: &mut u64| -> Option<usize> {
        *next += 1;
        let x = ask(*next, a.d);
        let run = x.run.clone();
        fleet.kind = "owner.ask";
        let acts = fleet.owner_does(|o| o.ask(x, now));
        fleet.carry(acts, now);
        let placed = settle(fleet, q, live, Some(&run));
        if placed.is_none() {
            q.refused += 1;
        }
        placed
    };
    let end = |fleet: &mut Fleet, q: &mut Quality, live: &mut Vec<(usize, String)>, i: usize, run: &str, now: i64| {
        let mut said: Vec<Said> = Vec::new();
        fleet.kind = "runner.ended";
        let _ = fleet.runner_does(i, |r| r.ended(peer, run, true, now, &mut said));
        let replies = said.into_iter().map(|(_, m)| m).collect::<Vec<_>>();
        let name = fleet.names.get(i).cloned().unwrap_or_default();
        q.drop(i);
        for m in replies {
            fleet.kind = to_owner_kind(&m);
            let acts = fleet.owner_does(|o| o.heard(&name, m, now));
            fleet.carry(acts, now);
        }
        let _ = settle(fleet, q, live, None);
    };
    for _ in 0..k {
        now += 1;
        let _ = place(&mut fleet, &mut q, &mut live, now, &mut next);
    }
    q.over = 0;
    q.refused = 0;
    fleet.prof.clear();
    let steps = usize::try_from(a.steps).unwrap_or(0);
    let (mut critical, mut owner, mut total) = (Vec::with_capacity(steps), Vec::with_capacity(steps), Vec::with_capacity(steps));
    let mut messages = 0u64;
    for _ in 0..a.steps {
        now += 1;
        if !live.is_empty() && k > 0 {
            let j = usize::try_from(splitmix(&mut seed) % live.len() as u64).unwrap_or(0);
            let (i, run) = live.swap_remove(j);
            end(&mut fleet, &mut q, &mut live, i, &run, now);
        }
        fleet.reset();
        let placed = place(&mut fleet, &mut q, &mut live, now, &mut next);
        let all: u64 = fleet.runner_ns.iter().sum();
        let chosen = placed.and_then(|i| fleet.runner_ns.get(i).copied()).unwrap_or(0);
        critical.push(fleet.owner_ns + chosen);
        owner.push(fleet.owner_ns);
        total.push(fleet.owner_ns + all);
        messages += fleet.messages;
        let Some(i) = placed else { continue };
        q.sample();
        if k == 0
            && let Some(p) = live.iter().position(|(j, _)| *j == i)
        {
            let (i, run) = live.swap_remove(p);
            end(&mut fleet, &mut q, &mut live, i, &run, now);
        }
    }
    let per = if a.steps > 0 { messages as f64 / a.steps as f64 } else { 0.0 };
    let raw = |w: &str| a.out.as_ref().map(|o| format!("{o}.{w}"));
    report(&a, "critical", k, fleet.overhead, &critical, &q, per, raw("critical"));
    report(&a, "owner", k, fleet.overhead, &owner, &q, per, raw("owner"));
    report(&a, "total", k, fleet.overhead, &total, &q, per, raw("total"));
    if a.profile {
        for (kind, (n, ns)) in &fleet.prof {
            eprintln!("{kind:28} {n:>9} calls {:>9.0} ns each", *ns as f64 / (*n).max(1) as f64);
        }
    }
    ExitCode::SUCCESS
}
