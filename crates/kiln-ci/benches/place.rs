//! Placement's decisions, timed (docs/needs.md §11, E5): what later work compares with Envoy's,
//! HAProxy's and nginx's picks (`~/.cache/kiln-lb/research-bench.md`). Not a test: run it with
//! `cargo bench -p kiln-ci --bench place`, on a quiet machine, in release.
//!
//! - **A runner's admission**: `N` runs waiting in its line, asking in `d` dimensions (1: CPUs;
//!   2: and memory; 4: and disk and GPUs), the runner full; each step a run ends, and the runner
//!   decides what its room goes to (the decision), the run it offers is taken and started, and a
//!   new run arrives to keep `N` waiting. Each end and each arrival is timed alone: the median and
//!   99th percentile in nanoseconds, nanoseconds per decision, and decisions a second on one core.
//! - **An owner's pick**: the first offer is taken and every other probe withdrawn: O(1) to
//!   decide, then a withdrawal to each of the other `N - 1` runners (the messages E-D9 counts).
//!
//! Beside the speed, its quality: oversubscription (claims past what the runner may place, in any
//! dimension: it must be 0) and refusals (runs answered never or lapsed: counted).

use std::collections::VecDeque;
use std::time::Instant;

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::ToOwner;
use kiln_ci::place::owner::{Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Records, Said, Settings, Start};
use kiln_ci::sched::needs::Needs;
use kiln_ci::sched::state::{Cpus, Fence, Resources};

const GIB: u64 = 1 << 30;
const OWNERS: [&str; 8] = ["o0", "o1", "o2", "o3", "o4", "o5", "o6", "o7"];

/// The bench's own inputs, drawn the same every run (Knuth's MMIX linear congruential generator).
struct Draw(u64);

impl Draw {
    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.draw() % n.max(1)
    }
}

fn peer(o: usize) -> Peer<'static> {
    Peer { name: OWNERS.get(o).copied().unwrap_or("o0"), epoch: 1 }
}

fn owner_index(name: &str) -> usize {
    OWNERS.iter().position(|o| *o == name).unwrap_or(0)
}

fn ask(draw: &mut Draw, run: u64, d: usize) -> (usize, Ask, Option<u64>) {
    let o = usize::try_from(draw.below(8)).unwrap_or(0);
    let mut request = Resources { cpu: Cpus::whole(1 + draw.below(8)), ..Resources::default() };
    if d >= 2 {
        request.mem = (1 + draw.below(32)) * GIB;
    }
    if d >= 4 {
        request.disk = (1 + draw.below(200)) * GIB;
        request.gpu = u32::from(draw.below(8) == 0);
    }
    let predicted = 60_000 + draw.below(600_000);
    let a = Ask { run: format!("r{run}"), needs: Needs { request, ..Needs::default() }, repo: format!("repo{}", o % 3), actor: OWNERS.get(o).copied().unwrap_or("o0").to_owned(), ..Ask::default() };
    (o, a, Some(predicted))
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    let i = sorted.len().saturating_sub(1).saturating_mul(p) / 100;
    sorted.get(i).copied().unwrap_or(0)
}

/// What the runner said, carried out as owners would: each offer taken and started.
fn carry_out(a: &mut Admission, said: &[Said], now: i64, running: &mut VecDeque<(usize, String)>, decisions: &mut u64, refused: &mut u64) {
    let offers: Vec<(usize, String, Fence)> = said
        .iter()
        .filter_map(|(o, m)| match m {
            ToOwner::Offer { run, fence, .. } => Some((owner_index(o), run.clone(), *fence)),
            _ => None,
        })
        .collect();
    *refused += u64::try_from(said.iter().filter(|(_, m)| matches!(m, ToOwner::Never { .. } | ToOwner::Lapsed { .. })).count()).unwrap_or(0);
    for (o, run, fence) in offers {
        *decisions += 1;
        let mut out = Vec::new();
        a.take(peer(o), &run, fence, now, &mut out);
        if a.start(peer(o), &run, fence, now, &mut out) == Some(Start::New) {
            running.push_back((o, run));
        }
    }
}

/// One configuration of the runner's admission: its line `n` long, asks in `d` dimensions.
fn admission(n: u32, d: usize, steps: u32) {
    let size = Resources { cpu: Cpus::whole(64), mem: 256 * GIB, disk: 2048 * GIB, gpu: 8 };
    let init = Init {
        since: None,
        me: "bench".into(),
        epoch: 1,
        facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() },
        labels: Vec::new(),
        sample: Sample { size, ..Sample::default() },
        records: Records::new(),
        settings: Settings { budget: None, overcommit_cpu: 1000, backfill: true, weights: Default::default(), ..Settings::default() },
    };
    let mut a = Admission::new(init, 0);
    let cap = a.cap();
    let mut draw = Draw(u64::from(n) * 31 + u64::try_from(d).unwrap_or(0));
    let (mut now, mut next_run, mut decisions, mut refused, mut oversubscribed) = (0i64, 0u64, 0u64, 0u64, 0u64);
    let mut running: VecDeque<(usize, String)> = VecDeque::new();
    let mut out: Vec<Said> = Vec::with_capacity(64);
    let (mut ends, mut arrivals, mut per): (Vec<u128>, Vec<u128>, Vec<u128>) = (Vec::new(), Vec::new(), Vec::new());
    let mut fill = |a: &mut Admission, now: i64, timed: Option<&mut Vec<u128>>, running: &mut VecDeque<(usize, String)>, decisions: &mut u64, refused: &mut u64, next_run: &mut u64| {
        let mut timed = timed;
        while a.queued() < n {
            *next_run += 1;
            let (o, ask, predicted) = ask(&mut draw, *next_run, d);
            let mut said = Vec::new();
            let t = Instant::now();
            a.probe(peer(o), ask, predicted, now, &mut said);
            let dt = t.elapsed().as_nanos();
            if let Some(v) = timed.as_deref_mut() {
                v.push(dt);
            }
            carry_out(a, &said, now, running, decisions, refused);
        }
    };
    fill(&mut a, now, None, &mut running, &mut decisions, &mut refused, &mut next_run);
    let warm = decisions;
    let mut decided_in = 0u128;
    for _ in 0..steps {
        now += 1;
        let Some((o, run)) = running.pop_front() else { break };
        out.clear();
        let t = Instant::now();
        a.ended(peer(o), &run, true, now, &mut out);
        let dt = t.elapsed().as_nanos();
        ends.push(dt);
        let before = decisions;
        carry_out(&mut a, &out, now, &mut running, &mut decisions, &mut refused);
        if decisions > before {
            decided_in += dt;
            per.push(dt / u128::from(decisions - before));
        }
        fill(&mut a, now, Some(&mut arrivals), &mut running, &mut decisions, &mut refused, &mut next_run);
        let (_, held) = a.room();
        oversubscribed += u64::from(held.iter().zip(&cap).any(|(h, c)| h > c));
    }
    let made = decisions.saturating_sub(warm);
    ends.sort_unstable();
    arrivals.sort_unstable();
    per.sort_unstable();
    let total: u128 = ends.iter().sum();
    let per_decision = if made > 0 { decided_in / u128::from(made) } else { 0 };
    let rate = if total > 0 { (made as f64) / (total as f64 / 1e9) } else { 0.0 };
    let checked = a.check().map_or_else(|e| format!(", check failed: {e}"), |()| String::new());
    println!(
        "admission N={n:>5} d={d}: {made} decisions over {} ends; per decision p50 {} ns p99 {} ns, mean {per_decision} ns; {rate:.0} decisions/s per core; per end p50 {} ns p99 {} ns; per arrival p50 {} ns p99 {} ns; oversubscribed steps {oversubscribed}; refused {refused}{checked}",
        ends.len(),
        percentile(&per, 50),
        percentile(&per, 99),
        percentile(&ends, 50),
        percentile(&ends, 99),
        percentile(&arrivals, 50),
        percentile(&arrivals, 99),
    );
}

/// The owner's pick among `n` runners: an ask probed at all of them, then the first offer taken
/// and the other probes withdrawn. Only the offer is timed.
fn pick(n: u32, rounds: u32) {
    let machines: Vec<Machine> = (0..n)
        .map(|j| Machine { name: format!("r{j}"), facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, labels: Vec::new(), size: Size::known(&Resources { cpu: Cpus::whole(64), ..Resources::default() }), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast: false })
        .collect();
    let mut o = Owner::new("bench", 1);
    let _ = o.machines(machines);
    let mut times = Vec::new();
    let mut withdrawals = 0usize;
    for round in 0..rounds {
        let run = format!("r{round}");
        let _ = o.ask(Ask { run: run.clone(), ..Ask::default() }, 0);
        let offer = ToOwner::Offer { run, fence: Fence { epoch: 1, n: u64::from(round) + 1 }, got: Resources::default(), free: Resources::default(), predicted: None, disk: Default::default(), pace: Default::default() };
        let t = Instant::now();
        let acts = o.heard("r0", offer, 1);
        times.push(t.elapsed().as_nanos());
        withdrawals = acts.len().saturating_sub(1);
    }
    times.sort_unstable();
    println!("owner's pick N={n:>5}: the first offer taken, then {withdrawals} withdrawals; p50 {} ns p99 {} ns", percentile(&times, 50), percentile(&times, 99));
}

fn main() {
    for n in [100u32, 500, 4095, 10_000] {
        for d in [1usize, 2, 4] {
            admission(n, d, 20_000);
        }
    }
    for n in [100u32, 500, 4095, 10_000] {
        pick(n, 200);
    }
}
