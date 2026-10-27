//! The simulator's fleet tests (docs/ci.md W05): legs placed by their runners' live headroom.

use super::replay::rng::Rng;
use super::{Machine, Sim};
use crate::facts::RunnerFacts;
use crate::sched::routing::{LegRoute, RunnerView};
use crate::sched::state::{Config, LegIn, Resources, RunnerIn, TARGET_USE};

const GIB: u64 = 1 << 30;
const MIN: i64 = 60_000;
/// How often a runner samples itself (`kiln agent`'s default).
const SAMPLE_MS: i64 = 5_000;
/// The legs queued at once before the stream.
const BURST: usize = 100;
/// How long the stream of legs lasts.
const STREAM_MS: i64 = 120 * MIN;

/// A mixed fleet, CPUs 8 to 32 and memory 16 to 128 GiB: (name, CPUs, GiB, its own work's CPUs
/// and GiB, its I/O pressure). The last serves production too, which its samples count in use.
const FLEET: [(&str, u32, u64, f64, u64, f64); 6] = [
    ("small", 8, 16, 0.0, 0, 0.0),
    ("lean", 16, 24, 0.0, 0, 0.5),
    ("wide", 16, 64, 0.0, 0, 0.0),
    ("mid", 24, 48, 0.0, 0, 1.0),
    ("big", 32, 64, 0.0, 0, 0.0),
    ("prod", 32, 128, 4.0, 40, 2.0),
];

/// A Linux runner of `cpus` CPUs and `gib` GiB, with a slot a CPU: enough that its headroom, not
/// its slots, says what it takes.
fn runner(name: &str, cpus: u32, gib: u64) -> RunnerIn {
    let mut facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    facts.cpu.cores = cpus / 2;
    facts.cpu.threads = cpus;
    facts.mem.total = gib * GIB;
    RunnerIn { id: name.into(), view: RunnerView { name: name.into(), labels: vec!["fleet".into()], facts, caps: 1 << 10, pool: None }, slots: cpus, capacity: None, executor: None }
}

/// A seeded stream of legs, enough to keep work queued: a burst at `from`, then one every 2 to 14
/// s until `until`. Each asks 1 to 8 CPUs and half a GiB to a GiB and a half for each, or now and
/// then a link step's 12 GiB on one CPU, and takes 1 to 20 minutes: (queued at, leg, duration).
fn legs(seed: u64, from: i64, until: i64) -> Vec<(i64, LegIn, i64)> {
    let mut rng = Rng::stream(seed, "fleet");
    // What history expects each to take: within 30% of what it does.
    let mut guess = Rng::stream(seed, "estimates");
    let pick = |rng: &mut Rng, of: &[u64]| of.get(rng.below(of.len())).copied().unwrap_or(1);
    let mut out = Vec::new();
    let mut t = from;
    while t <= until {
        let n = out.len();
        let cpus = pick(&mut rng, &[1, 1, 1, 2, 2, 2, 4, 4, 8]);
        let mib = pick(&mut rng, &[512, 1024, 1024, 1536]);
        let (cpus, mem) = if rng.below(12) == 0 { (1, 12 * GIB) } else { (cpus, (cpus * mib) << 20) };
        let duration = pick(&mut rng, &[1, 2, 3, 5, 8, 13, 20]) as i64 * MIN + rng.below(60_000) as i64;
        let repo = ["acme/web", "acme/api", "acme/infra"].get(n % 3).copied().unwrap_or_default();
        let request = Resources { cpu: cpus as f64, mem, ..Resources::default() };
        let cost = duration as f64 / 1000.0 * (0.7 + guess.below(61) as f64 / 100.0);
        let leg = LegIn { id: format!("leg-{n}"), run: format!("run-{n}"), repo: repo.into(), route: LegRoute { labels: vec!["fleet".into()], ..LegRoute::default() }, priority: 50, request, cost: Some(cost), ..LegIn::default() };
        out.push((t, leg, duration));
        if out.len() >= BURST {
            t += 2_000 + rng.below(12_000) as i64;
        }
    }
    out
}

/// What a fleet's run measured, to where it is now.
#[derive(Debug, Default)]
struct Measured {
    /// How long work was queued.
    queued_ms: i64,
    /// The fleet's CPUs and memory in use while work was queued, time-weighted: its runners' own
    /// work's and what the legs there asked.
    cpu_use: f64,
    mem_use: f64,
    /// Of any runner at any moment: the most of its memory in use, and the most of its CPUs or
    /// its memory (the share), with the runner and when.
    worst_mem: f64,
    worst_use: (f64, String, i64),
}

/// Measures a run, to where it is now, from what it placed where ([`Sim::placed`]) and when each
/// leg was queued and started.
fn measure(sim: &Sim, legs: &[(i64, LegIn, i64)]) -> Measured {
    enum Change {
        Queued(i64),
        Placed(usize, Resources),
    }
    let mut changes: Vec<(i64, Change)> = Vec::new();
    for (at, leg, _) in legs {
        changes.push((*at, Change::Queued(1)));
        if let Some(started) = sim.started(&leg.id) {
            changes.push((started, Change::Queued(-1)));
        }
    }
    for (at, runner, on) in &sim.placed {
        if let Some(i) = FLEET.iter().position(|f| f.0 == runner) {
            changes.push((*at, Change::Placed(i, *on)));
        }
    }
    changes.sort_by_key(|(at, _)| *at);
    let (cpus, mem): (f64, f64) = FLEET.iter().fold((0.0, 0.0), |(c, m), f| (c + f64::from(f.1), m + (f.2 * GIB) as f64));
    let mut on = [Resources::default(); FLEET.len()];
    let mut queued = 0;
    let (mut cpu_time, mut mem_time) = (0.0, 0.0);
    let mut m = Measured::default();
    for (k, (at, change)) in changes.iter().enumerate() {
        match change {
            Change::Queued(d) => queued += d,
            Change::Placed(i, now) => {
                if let (Some(slot), Some(&(name, c, gib, own_cpu, own_gib, _))) = (on.get_mut(*i), FLEET.get(*i)) {
                    *slot = *now;
                    let mem_share = ((own_gib * GIB).saturating_add(now.mem)) as f64 / (gib * GIB) as f64;
                    let share = ((own_cpu + now.cpu) / f64::from(c)).max(mem_share);
                    m.worst_mem = m.worst_mem.max(mem_share);
                    if share > m.worst_use.0 {
                        m.worst_use = (share, name.to_owned(), *at);
                    }
                }
            }
        }
        let next = changes.get(k + 1).map_or(sim.now, |(t, _)| *t);
        if queued > 0 && next > *at {
            let dt = (next - at) as f64;
            let (c, g) = FLEET.iter().zip(&on).fold((0.0, 0.0), |(c, g), (f, o)| (c + f.3 + o.cpu, g + ((f.4 * GIB).saturating_add(o.mem)) as f64));
            cpu_time += c * dt;
            mem_time += g * dt;
            m.queued_ms += next - at;
        }
    }
    if m.queued_ms > 0 {
        m.cpu_use = cpu_time / (cpus * m.queued_ms as f64);
        m.mem_use = mem_time / (mem * m.queued_ms as f64);
    }
    m
}

/// W05 step 4: a mixed fleet, each runner sampling itself every 5 s, packs to the target use by
/// those samples without ever placing memory past a runner's capacity. On three seeds' streams of
/// legs, more than the fleet runs, the fleet's CPUs are at least 80% in use while the stream keeps
/// work queued (time-weighted; whole CPUs within 85% are 82.8% of this fleet's), and none starves:
/// legs that can't fit hold runners (W05's admission), so 8-CPU legs wait at most half as long
/// again as all legs do, on average. Once the stream ends the rest run, none left behind; and at
/// no moment of it all is a runner's CPUs or memory in use past the target.
#[test]
fn a_fleet_packs_to_the_target_by_headroom_without_overcommitting_memory() {
    for seed in [1, 2, 3] {
        let mut sim = Sim::new(Config::default());
        for (i, &(name, cpus, gib, own_cpu, own_gib, io_full)) in FLEET.iter().enumerate() {
            sim.runner(0, runner(name, cpus, gib));
            // It samples once it's up, the fleet's samples spread over the period.
            let first = i as i64 * SAMPLE_MS / FLEET.len() as i64;
            sim.sampled(first, name, Machine { cpus, mem: gib * GIB, own_cpu, own_mem: own_gib * GIB, io_full }, SAMPLE_MS);
        }
        // Legs come once every runner has sent its first sample, for two hours.
        let legs = legs(seed, SAMPLE_MS, STREAM_MS);
        for (at, leg, duration) in &legs {
            sim.leg(*at, leg.clone(), *duration);
        }
        sim.run(STREAM_MS);
        let stream = measure(&sim, &legs);
        // None starves: big legs, holding runners once they can't fit (W05's admission), wait not
        // much longer than the rest.
        let mean_wait = |cpus: Option<f64>| {
            let w: Vec<i64> = legs.iter().filter(|(_, l, _)| cpus.is_none_or(|c| l.request.cpu == c)).filter_map(|(q, l, _)| sim.started(&l.id).map(|s| s - q)).collect();
            w.iter().sum::<i64>() as f64 / w.len().max(1) as f64
        };
        let (all, big) = (mean_wait(None), mean_wait(Some(8.0)));
        assert!(big <= 1.5 * all, "seed {seed}: 8-CPU legs waited {:.0} s on average, all legs {:.0} s", big / 1000.0, all / 1000.0);
        assert!(stream.queued_ms >= STREAM_MS - SAMPLE_MS, "seed {seed}: work stayed queued while the stream lasted: {stream:?}");
        assert!(stream.cpu_use >= 0.80, "seed {seed}: the fleet's CPUs were {:.1}% in use while work was queued: {stream:?}", stream.cpu_use * 100.0);
        sim.run(STREAM_MS + 6 * 60 * MIN);
        let all = measure(&sim, &legs);
        assert_eq!(sim.settled.len(), legs.len(), "seed {seed}: every leg ran: {all:?}");
        assert!(all.worst_mem <= 1.0, "seed {seed}: memory was placed past a runner's capacity: {all:?}");
        assert!(all.worst_use.0 <= TARGET_USE + 1e-9, "seed {seed}: a runner was in use past the target: {all:?}");
    }
}
