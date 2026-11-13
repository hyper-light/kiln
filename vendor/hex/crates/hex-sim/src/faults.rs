//! The fault nemesis (`docs/design/failure-isolation.md`, `hex-sim.md` H42, scheduler
//! implementation goal 5): each fault class injected into a simulated cell from a seed, the cell
//! driving `hex-health` as the scheduler does, and each fault's detection and isolation time
//! measured against the bound the design states. One seed, virtual time, one thread (hex-sim H1): a
//! run is a function of its seed.
//!
//! The crash class is [`crate::membership`]'s. Here:
//! - [`gray`]: one node's work runs slower than its peers' ([`hex_health::grade`]).
//! - [`disk_stall`]: one node's disk probe stops completing ([`hex_health::probe`]).
//! - [`antagonist`]: a co-tenant raises its neighbours' CPI ([`hex_health::antagonist`]).
//! - [`stuck_loop`], [`quota`], [`provider_outage`]: the agent faults ([`hex_health::agent`]).
//! - [`dangerous`]: a security signal, per kind Hex places ([`crate::danger`]).
//! - [`correlated`]: many nodes of a loaded cell slow at once, each ejected only where the rest of
//!   the cell re-places its work, and a mass loss against the eviction rate ([`hex_health::penalty`]).

use hex_health::agent::{Cause, LoopDetector, ProviderError, ProviderHealth, Stuck};
use hex_health::antagonist::{Antagonists, Sample};
use hex_health::danger::Signal;
use hex_health::grade::{self, PeerGrade};
use hex_health::penalty::{Ejector, EvictionLimiter};
use hex_health::probe::DiskProbe;
use hex_health::{HealthError, Level};

use crate::danger::{self, Isolation, Refusal, STRONGER, Tripped};
use crate::index::{FitIndex, Pick as Placement, Verdict};
use crate::model::{
    Class, Classes, Demand, Kind, NodeId, NodeSpec, Overheads, Resources, WorkloadId,
};
use crate::rng::Rng;

pub const MS: u64 = 1_000_000;
pub const S: u64 = 1_000_000_000;

/// Every way a fault run refuses to go on, typed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FaultError {
    #[error("the health core refused: {0}")]
    Health(#[from] HealthError),
    #[error("the run took more than its budget of {0} steps")]
    StepBudget(u64),
    #[error("virtual time overflowed")]
    TimeOverflow,
    #[error("the simulated cell refused: {0}")]
    Sim(crate::SimError),
}

/// One injected fault's measured times, from its onset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Measured {
    pub detected_after_ns: Option<u64>,
    pub isolated_after_ns: Option<u64>,
    /// Verdicts against what wasn't at fault: a healthy node suspected, a bystander throttled, a node
    /// charged with its provider's or account's fault.
    pub wrong_verdicts: u64,
}

/// A switch for mutation tests: each turns one safeguard off, and the test that guards it must fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    None,
    /// The gray detector off: the grade's verdicts are never acted on.
    NoDetector,
    /// The re-placement check ignored: every drain ejects, its work placed where it fits.
    NoReplacementCheck,
    /// The eviction rate ignored: every dead node's work moves at once.
    NoEvictionRate,
    /// Provider errors fed to the node grade as slowness.
    ProviderErrorsAsNodeSlowness,
}

fn after(onset: u64, at: u64) -> Option<u64> {
    at.checked_sub(onset)
}

fn add(a: u64, b: u64) -> Result<u64, FaultError> {
    a.checked_add(b).ok_or(FaultError::TimeOverflow)
}

/// `mem` with `class`'s overhead for it added; refused where the class's overhead goes by guest size
/// and names no row for `mem`.
fn with_overhead(overheads: &Overheads, class: Class, mem: u64) -> Result<u64, FaultError> {
    let bytes = overheads
        .of(class, mem)
        .ok_or(FaultError::Sim(crate::SimError::Config(
            "the class's overhead names no row for the workload's memory",
        )))?;
    add(mem, bytes)
}

/// A slowdown around 1x with the spread of ordinary work: 0.7x to 1.5x, in thousandths.
fn noise(rng: &mut Rng) -> u64 {
    rng.range(700, 1500)
}

/// Gray / fail-slow (F2): a cell of `nodes`, each reporting about ten pieces of work a second; from a
/// seeded onset one node's work runs `factor_milli` / 1000 times slower. Detected at its `Suspect`,
/// isolated at its `Drain` taken by the ejector.
pub fn gray(seed: u64) -> Result<(Measured, u64), FaultError> {
    gray_with(seed, Mutant::None)
}

/// [`gray`] with a mutant switched on.
pub fn gray_with(seed: u64, mutant: Mutant) -> Result<(Measured, u64), FaultError> {
    let mut rng = Rng::new(seed);
    let nodes = rng.range(4, 200);
    let bad = rng.range(0, nodes.saturating_sub(1));
    let factor = rng.range(2_500, 8_000);
    let onset = rng.range(5 * S, 20 * S);
    let end = add(onset, 10 * S)?;
    let mut g = PeerGrade::new(usize::try_from(nodes).unwrap_or(usize::MAX), 0);
    let mut ejector = Ejector::new(usize::try_from(nodes).unwrap_or(usize::MAX));
    let mut m = Measured::default();
    let mut changes = Vec::new();
    // Every node reports at 100 ms steps, with a seeded phase each, so windows close in time order.
    let mut t: u64 = 0;
    let mut steps: u64 = 0;
    while t < end {
        for n in 0..nodes {
            let at = add(t, n.saturating_mul(97) % (100 * MS))?;
            let mut slowdown = noise(&mut rng);
            if n == bad && at >= onset {
                slowdown = slowdown.saturating_mul(factor) / 1000;
            }
            changes.clear();
            g.observe(n, at, slowdown, &mut changes)?;
            if mutant == Mutant::NoDetector {
                changes.clear();
            }
            for c in &changes {
                if c.node != bad || at < onset {
                    m.wrong_verdicts = m.wrong_verdicts.saturating_add(1);
                    continue;
                }
                if c.level >= Level::Suspect && m.detected_after_ns.is_none() {
                    m.detected_after_ns = after(onset, at);
                }
                if c.level == Level::Drain
                    && m.isolated_after_ns.is_none()
                    && ejector.eject(c.node, at, true)?.is_ok()
                {
                    m.isolated_after_ns = after(onset, at);
                }
            }
        }
        t = add(t, 100 * MS)?;
        steps = steps.saturating_add(1);
        if steps > 1_000_000 {
            return Err(FaultError::StepBudget(steps));
        }
    }
    Ok((m, factor))
}

/// The re-placement times the probe's cost rule was replayed at
/// (`bench/health/records/2026-11-06-probe-floor-apfs.md`): an OCI start, and Shards' measured boot p99.
pub const REPLACE_NS: [u64; 2] = [2 * MS, 194 * MS];

/// What a disk-stall run judged by: the slowest healthy answer and the drain age its probe held at the
/// onset, its period and its price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallJudge {
    pub slowest_ns: u64,
    pub drain_age_ns: u64,
    pub period_ns: u64,
    /// The re-placement time its false drains were priced at.
    pub replace_ns: u64,
}

/// Disk stall (F3, `AC-T7.10.3`): a node probes its disk every `period` with 1-5 ms flushes, its probe
/// seeded with a record of 1,000 answers of the same disk and one of [`REPLACE_NS`]; from a seeded
/// onset the outstanding write never completes.
pub fn disk_stall(seed: u64) -> Result<(Measured, StallJudge), FaultError> {
    let mut rng = Rng::new(seed);
    let period = rng.range(100 * MS, 500 * MS);
    let onset_probe = rng.range(20, 200);
    let replace = if rng.chance(500) {
        REPLACE_NS[0]
    } else {
        REPLACE_NS[1]
    };
    let record: Vec<(u64, u32)> = (0..1_000).map(|_| (rng.range(MS, 5 * MS), 1)).collect();
    let mut p = DiskProbe::new(&record, replace)?;
    let mut t: u64 = 0;
    for _ in 0..onset_probe {
        p.start(t)?;
        let lat = rng.range(MS, 5 * MS);
        p.finish(add(t, lat)?)?;
        t = add(t, period)?;
    }
    let judge = StallJudge {
        slowest_ns: p.slowest_ns(),
        drain_age_ns: p.drain_age_ns()?,
        period_ns: period,
        replace_ns: replace,
    };
    // The stall: the write the node starts at its next probe after `onset` never finishes.
    let onset = add(t, rng.range(0, period.saturating_sub(1)))?;
    let start = add(t, period)?;
    p.start(start)?;
    let mut m = Measured::default();
    let mut now = start;
    let mut steps: u64 = 0;
    // The driver looks again each millisecond, as a node agent's timer would at its own granularity.
    while m.isolated_after_ns.is_none() {
        now = add(now, MS)?;
        match p.level(now)? {
            Level::Healthy => {}
            Level::Suspect => {
                m.detected_after_ns.get_or_insert(now.saturating_sub(onset));
            }
            Level::Drain => {
                m.detected_after_ns.get_or_insert(now.saturating_sub(onset));
                m.isolated_after_ns = Some(now.saturating_sub(onset));
            }
        }
        steps = steps.saturating_add(1);
        if steps > 10_000_000 {
            return Err(FaultError::StepBudget(steps));
        }
    }
    Ok((m, judge))
}

/// What a fail-slow disk run saw: whether the node's own probe drained it, and the peer grade's
/// verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailSlow {
    pub probe_drained: bool,
    pub measured: Measured,
    /// The node's disk slowdown when the grade drained it, in thousandths.
    pub slowdown_at_drain_milli: Option<u64>,
}

/// A disk that slows gradually (F2, F3): one node of 5-50 among healthy peers, its disk 0.5-5% slower
/// each window, from 1x to 4x. Its work, slowed as its disk is, runs slow against its peers', which
/// the peer grade judges; its own probe (1-5 ms flushes every 100 ms, seeded with a record of 1,000 of
/// them) is watched beside it.
pub fn fail_slow_disk(seed: u64) -> Result<FailSlow, FaultError> {
    let mut rng = Rng::new(seed);
    let nodes = rng.range(5, 50);
    let step = rng.range(5, 50);
    let replace = if rng.chance(500) {
        REPLACE_NS[0]
    } else {
        REPLACE_NS[1]
    };
    let record: Vec<(u64, u32)> = (0..1_000).map(|_| (rng.range(MS, 5 * MS), 1)).collect();
    let mut probe = DiskProbe::new(&record, replace)?;
    let mut g = PeerGrade::new(usize::try_from(nodes).unwrap_or(usize::MAX), 0);
    let mut out = FailSlow {
        probe_drained: false,
        measured: Measured::default(),
        slowdown_at_drain_milli: None,
    };
    let mut changes = Vec::new();
    let (mut t, mut slowdown) = (0u64, 1_000u64);
    while slowdown < 4_000 && out.measured.isolated_after_ns.is_none() {
        // One window: ten probes of the node's disk, ten pieces of work on each node.
        for k in 0..10u64 {
            let at = add(t, k.saturating_mul(100 * MS))?;
            let latency = rng.range(MS, 5 * MS).saturating_mul(slowdown) / 1000;
            probe.start(at)?;
            out.probe_drained |= probe.level(add(at, latency)?)? == Level::Drain;
            probe.finish(add(at, latency)?)?;
            for n in 0..nodes {
                let mut s = noise(&mut rng);
                if n == 0 {
                    s = s.saturating_mul(slowdown) / 1000;
                }
                changes.clear();
                g.observe(n, add(at, n)?, s, &mut changes)?;
                for c in &changes {
                    if c.node != 0 {
                        out.measured.wrong_verdicts = out.measured.wrong_verdicts.saturating_add(1);
                    } else if c.level == Level::Drain {
                        out.measured.isolated_after_ns = Some(at);
                        out.slowdown_at_drain_milli = Some(slowdown);
                    }
                }
            }
        }
        t = add(t, S)?;
        slowdown = slowdown.saturating_add(slowdown.saturating_mul(step) / 1000);
    }
    Ok(out)
}

/// Antagonist (F4): a node of 5-10 latency-sensitive tasks of one job and one batch co-tenant,
/// sampled once a minute (CPI²'s cadence); from a seeded onset after the job has its spec, the
/// co-tenant's CPU use rises tenfold and its neighbours' CPI by `factor_milli` / 1000 until it is
/// capped. Detected and isolated at the cap.
pub fn antagonist(seed: u64) -> Result<(Measured, u64), FaultError> {
    const MIN: u64 = 60 * S;
    let mut rng = Rng::new(seed);
    let tasks = rng.range(5, 10);
    // The class the design states: the neighbours' CPI at least doubled.
    let factor = rng.range(2_000, 3_000);
    let onset_min = rng.range(110, 140);
    let best_effort = rng.chance(500);
    let mut a = Antagonists::new(64);
    let mut m = Measured::default();
    let onset = onset_min.saturating_mul(MIN);
    for minute in 0..onset_min.saturating_add(30) {
        let t = minute.saturating_mul(MIN);
        let hot = t >= onset && !a.throttled(99, t);
        // The node's samples of one minute go in together, as CPI²'s per-machine agent takes them.
        let mut batch = Vec::with_capacity(usize::try_from(tasks).unwrap_or(0).saturating_add(1));
        for task in 0..tasks {
            let base = rng.range(1000, 1060);
            let cpi = if hot {
                base.saturating_mul(factor) / 1000
            } else {
                base
            };
            batch.push(Sample {
                task,
                job: 1,
                node: 0,
                at_ns: t,
                cpi_milli: cpi,
                cpu_milli: rng.range(600, 900),
                best_effort: false,
            });
        }
        batch.push(Sample {
            task: 99,
            job: 2,
            node: 0,
            at_ns: t,
            cpi_milli: 1500,
            cpu_milli: if hot {
                rng.range(2500, 4000)
            } else {
                rng.range(250, 400)
            },
            best_effort,
        });
        if let Some(th) = a.sample(&batch)? {
            if th.antagonist != 99 || t < onset {
                m.wrong_verdicts = m.wrong_verdicts.saturating_add(1);
            } else if m.isolated_after_ns.is_none() {
                m.detected_after_ns = after(onset, t);
                m.isolated_after_ns = after(onset, t);
            }
        }
    }
    Ok((m, factor))
}

/// Stuck tool loop (F5): `sessions` agents make calls a few seconds apart, with progress every few
/// calls; from a seeded onset one of them repeats its last call without progress. Detected and
/// isolated (stopped) at the stuck verdict. Returns, beside the times, how many calls the loop made
/// before it was stopped.
pub fn stuck_loop(seed: u64) -> Result<(Measured, u32), FaultError> {
    let mut rng = Rng::new(seed);
    let sessions = rng.range(1, 50);
    let looping = rng.range(0, sessions.saturating_sub(1));
    let onset = rng.range(30 * S, 120 * S);
    let mut d = LoopDetector::new(64);
    let mut m = Measured::default();
    let mut last_call = vec![0u64; usize::try_from(sessions).unwrap_or(0)];
    let mut t: u64 = 0;
    let mut loop_calls: u32 = 0;
    while t < add(onset, 300 * S)? && m.isolated_after_ns.is_none() {
        for s in 0..sessions {
            let idx = usize::try_from(s).unwrap_or(usize::MAX);
            let in_loop = s == looping && t >= onset;
            let call = if in_loop {
                loop_calls = loop_calls.saturating_add(1);
                last_call.get(idx).copied().unwrap_or(0)
            } else {
                rng.next_u64()
            };
            if let Some(slot) = last_call.get_mut(idx) {
                *slot = call;
            }
            if let Some(stuck) = d.call(s, call)? {
                if !in_loop {
                    m.wrong_verdicts = m.wrong_verdicts.saturating_add(1);
                } else if let Stuck::Repeated { .. } = stuck {
                    m.detected_after_ns = after(onset, t);
                    m.isolated_after_ns = after(onset, t);
                    d.end(s);
                }
            }
            if !in_loop && rng.chance(300) {
                d.progress(s);
            }
        }
        t = add(t, rng.range(S, 10 * S))?;
    }
    Ok((m, loop_calls))
}

/// The account and provider half of a run: calls from `nodes` nodes to `providers` providers, each
/// call's error fed to [`ProviderHealth`] and, under the mutant, to the node grade.
struct Calls {
    nodes: u64,
    health: ProviderHealth,
    grade: PeerGrade,
    causes: Vec<Cause>,
    changes: Vec<grade::Change>,
    mutant: Mutant,
}

impl Calls {
    fn new(nodes: u64, mutant: Mutant) -> Self {
        let n = usize::try_from(nodes).unwrap_or(usize::MAX);
        Self {
            nodes,
            health: ProviderHealth::new(8, n, 0),
            grade: PeerGrade::new(n, 0),
            causes: Vec::new(),
            changes: Vec::new(),
            mutant,
        }
    }

    /// One call; local work beside it is reported to the node grade as ordinary.
    fn call(
        &mut self,
        rng: &mut Rng,
        provider: u64,
        node: u64,
        at: u64,
        outcome: Result<(), ProviderError>,
    ) -> Result<(), FaultError> {
        self.causes.clear();
        self.changes.clear();
        self.health
            .call(provider, node, node, at, outcome, &mut self.causes)?;
        let slowdown = match (outcome, self.mutant) {
            (Err(_), Mutant::ProviderErrorsAsNodeSlowness) => 10_000,
            _ => noise(rng),
        };
        self.grade.observe(node, at, slowdown, &mut self.changes)?;
        Ok(())
    }

    fn node_verdicts(&self) -> u64 {
        let charged = self
            .causes
            .iter()
            .filter(|c| matches!(c, Cause::NodePath { .. }))
            .count();
        let suspected = self
            .changes
            .iter()
            .filter(|c| c.level != Level::Healthy)
            .count();
        u64::try_from(charged.saturating_add(suspected)).unwrap_or(u64::MAX)
    }
}

/// Quota shown as slowness (F5): nodes call a provider on many accounts; from a seeded onset one
/// account's calls are refused as rate-limited or out of quota. Detected and isolated when the
/// account is held; any node verdict is wrong.
pub fn quota(seed: u64, mutant: Mutant) -> Result<Measured, FaultError> {
    let mut rng = Rng::new(seed);
    let nodes = rng.range(4, 64);
    let onset = rng.range(5 * S, 20 * S);
    let account_node = rng.range(0, nodes.saturating_sub(1));
    let mut c = Calls::new(nodes, mutant);
    let mut m = Measured::default();
    let mut t: u64 = 0;
    while t < add(onset, 10 * S)? {
        for n in 0..c.nodes {
            let at = add(t, n % (100 * MS))?;
            let out = if n == account_node && at >= onset {
                if rng.chance(500) {
                    Err(ProviderError::QuotaExhausted)
                } else {
                    Err(ProviderError::RateLimited {
                        retry_after_ns: Some(rng.range(S, 60 * S)),
                    })
                }
            } else {
                Ok(())
            };
            c.call(&mut rng, 1, n, at, out)?;
            if c.causes
                .iter()
                .any(|x| matches!(x, Cause::Account { account, .. } if *account == account_node))
                && m.detected_after_ns.is_none()
            {
                m.detected_after_ns = after(onset, at);
                m.isolated_after_ns = after(onset, at);
            }
            m.wrong_verdicts = m.wrong_verdicts.saturating_add(c.node_verdicts());
        }
        t = add(t, 100 * MS)?;
    }
    Ok(m)
}

/// Partial provider outage (F5): from a seeded onset a provider overloads or times out for a
/// majority of its callers (60-100%). Detected and isolated when it is marked degraded; any node
/// verdict is wrong.
pub fn provider_outage(seed: u64, mutant: Mutant) -> Result<Measured, FaultError> {
    let mut rng = Rng::new(seed);
    let nodes = rng.range(4, 64);
    let onset = rng.range(5 * S, 20 * S);
    let share = rng.range(600, 1000);
    let mut c = Calls::new(nodes, mutant);
    let mut m = Measured::default();
    let mut t: u64 = 0;
    while t < add(onset, 10 * S)? {
        for n in 0..c.nodes {
            let at = add(t, n % (100 * MS))?;
            let failing = n.saturating_mul(1000) < nodes.saturating_mul(share);
            let out = if failing && at >= onset {
                if rng.chance(500) {
                    Err(ProviderError::Overloaded)
                } else {
                    Err(ProviderError::Timeout)
                }
            } else {
                Ok(())
            };
            c.call(&mut rng, 1, n, at, out)?;
            if c.causes
                .iter()
                .any(|x| matches!(x, Cause::Provider { provider: 1 }))
                && m.detected_after_ns.is_none()
            {
                m.detected_after_ns = after(onset, at);
                m.isolated_after_ns = after(onset, at);
            }
            m.wrong_verdicts = m.wrong_verdicts.saturating_add(c.node_verdicts());
        }
        t = add(t, 100 * MS)?;
    }
    Ok(m)
}

/// The kinds' overheads as G2's traces stated them when G5 was measured (`docs/design/hex-sim.md` H10,
/// the recorded workload): OCI 0.42 MiB, measured (`bench/competitive/RESULT.md:95`); Firecracker 5 MiB,
/// its specification (Agache et al., NSDI '20); Shards 5 MiB, the stand-in G2's traces have since
/// replaced with Shards' own, measured by guest size (`bench/sched/shards_overhead.md`).
pub fn kind_overheads() -> Overheads {
    let mut o = Overheads::default();
    o.set(Class::Oci, 440_402);
    o.set(Class::Firecracker, 5_242_880);
    o.set(Class::Shards, 5_242_880);
    o
}

/// One of G2's node models (`hex-sim.md` H10), seeded: an m5d.metal host running all three kinds;
/// Hex's fleet's Linux hosts, all three; its Windows hosts, OCI and Shards; its macOS hosts, Shards
/// alone.
fn g2_node(rng: &mut Rng) -> (Classes, Resources) {
    const GIB: u64 = 1 << 30;
    let model = rng.range(0, 3);
    let classes = match model {
        0 | 1 => Classes::ALL,
        2 => Classes::NONE.with(Class::Oci).with(Class::Shards),
        _ => Classes::NONE.with(Class::Shards),
    };
    let capacity = if model == 0 {
        Resources {
            cpu_milli: 48_000,
            mem_bytes: 384 * GIB,
            gpus: 0,
            disk_bytes: 0,
        }
    } else {
        Resources {
            cpu_milli: rng.range(4, 48).saturating_mul(1000),
            mem_bytes: rng.range(8, 384).saturating_mul(GIB),
            gpus: u32::try_from(rng.range(0, 2)).unwrap_or(0),
            disk_bytes: 0,
        }
    };
    (classes, capacity)
}

/// A seeded workload of one of the three kinds, its kind's overhead charged.
fn g2_workload(rng: &mut Rng, overheads: &Overheads) -> Result<Demand, FaultError> {
    const GIB: u64 = 1 << 30;
    let class = match rng.range(0, 2) {
        0 => Class::Oci,
        1 => Class::Firecracker,
        _ => Class::Shards,
    };
    Ok(Demand {
        need: Resources {
            cpu_milli: rng.range(1_000, 24_000),
            mem_bytes: with_overhead(overheads, class, rng.range(1, 192).saturating_mul(GIB))?,
            gpus: 0,
            disk_bytes: 0,
        },
        kind: Kind::Plain,
        class,
        ask: Default::default(),
    })
}

fn node_id(id: u64) -> Result<NodeId, FaultError> {
    u32::try_from(id)
        .map(NodeId)
        .map_err(|_| FaultError::Sim(crate::SimError::Config("a node id past u32")))
}

/// What a dangerous run did: its isolation (the stop) measured, the verdict, and where the scheduler
/// re-placed it, measured apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dangerous {
    pub measured: Measured,
    pub isolation: Isolation,
    /// Re-placed on another node than the one it tripped on.
    pub elsewhere: bool,
    /// Re-placed, where the on-node rule (the first microVM its own node admits) would have refused it.
    pub rescued: bool,
}

/// Dangerous (F7): a cell of 4-32 nodes, each one of G2's node models (an m5d.metal host running all
/// three kinds; Hex's fleet's Linux hosts, all three; its Windows hosts, OCI and Shards; its macOS
/// hosts, Shards alone: `hex-sim.md` H10), with seeded capacity and a seeded load placed through G2's
/// fast path. A workload of kind `kind` on node 0 raises a seeded trip signal. Isolated at the signal
/// (stopped); then re-placed through the fast path as the first stronger kind some node admits, or
/// refused for the reason that holds, against an oracle on the cell as it stood once stopped.
pub fn dangerous(seed: u64, kind: Class) -> Result<Dangerous, FaultError> {
    const MIB: u64 = 1 << 20;
    let mut rng = Rng::new(seed);
    let n = rng.range(4, 32);
    let overheads = kind_overheads();
    let need = Resources {
        cpu_milli: rng.range(250, 8_000),
        mem_bytes: rng.range(128, 16_384).saturating_mul(MIB),
        gpus: u32::from(kind == Class::Oci && rng.chance(150)),
        disk_bytes: 0,
    };
    let mut specs = Vec::new();
    for id in 0..n {
        let (mut classes, mut capacity) = g2_node(&mut rng);
        if id == 0 {
            // The tripped workload's node runs its kind and holds it.
            classes = classes.with(kind);
            capacity.cpu_milli = capacity.cpu_milli.max(need.cpu_milli);
            capacity.mem_bytes =
                capacity
                    .mem_bytes
                    .max(with_overhead(&overheads, kind, need.mem_bytes)?);
            capacity.gpus = capacity.gpus.max(need.gpus);
        }
        specs.push(NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: node_id(id)?,
            capacity,
            background_milli: 0,
            classes,
            platform: Default::default(),
            virt: Default::default(),
            devices: crate::model::unnamed_gpus(capacity.gpus),
        });
    }
    let mut index = FitIndex::new(&specs).map_err(FaultError::Sim)?;
    let tripped = Tripped {
        id: WorkloadId(0),
        node: NodeId(0),
        need,
        kind: Kind::Plain,
        class: kind,
    };
    let running = Demand {
        need: Resources {
            mem_bytes: with_overhead(&overheads, kind, need.mem_bytes)?,
            ..need
        },
        kind: Kind::Plain,
        class: kind,
        ask: Default::default(),
    };
    index
        .take(NodeId(0), tripped.id, &running)
        .map_err(FaultError::Sim)?;
    // A seeded load: up to 4 workloads a node, each placed through the fast path where it fits.
    let pick = Placement::default();
    for k in 0..n.saturating_mul(4) {
        let filler = g2_workload(&mut rng, &overheads)?;
        if let Some(key) = index.fit(&filler, &pick).best {
            index
                .take(key.node, WorkloadId(k.saturating_add(1)), &filler)
                .map_err(FaultError::Sim)?;
        }
    }
    let signal = match rng.range(0, 2) {
        0 => Signal::SeccompKill,
        1 => Signal::EgressRefused,
        _ => Signal::PrivilegeEscalation,
    };
    let onset = rng.range(S, 100 * S);
    // The oracle's cell: as it stands once the workload is stopped.
    let mut stopped = index.try_clone().map_err(FaultError::Sim)?;
    stopped
        .give(NodeId(0), tripped.id, &running)
        .map_err(FaultError::Sim)?;
    let isolation = danger::isolate(&tripped, signal, &mut index, &overheads, &pick)
        .map_err(FaultError::Sim)?;
    let first = if kind == Class::Oci {
        let mut first = None;
        for to in STRONGER {
            if danger::admitted_as(&stopped, &tripped, to, &overheads).map_err(FaultError::Sim)? {
                first = Some(to);
                break;
            }
        }
        first
    } else {
        None
    };
    let mut any_microvm = false;
    for to in STRONGER {
        any_microvm |= danger::runs(&stopped, to).map_err(FaultError::Sim)?;
    }
    let right = match isolation {
        Isolation::Move { from, to, node, .. } => {
            let at = Demand {
                need: Resources {
                    mem_bytes: with_overhead(&overheads, to, need.mem_bytes)?,
                    ..need
                },
                kind: Kind::Plain,
                class: to,
                ask: Default::default(),
            };
            from == kind
                && first == Some(to)
                && stopped.verdict(node, &at).map_err(FaultError::Sim)? == Verdict::Admits
        }
        Isolation::Refuse { from, why } => {
            from == kind
                && first.is_none()
                && match why {
                    Refusal::InMicroVm => kind != Class::Oci,
                    Refusal::NoMicroVmInCell => kind == Class::Oci && !any_microvm,
                    Refusal::NoneHolds => kind == Class::Oci && any_microvm,
                }
        }
        Isolation::None => false,
    };
    let mut measured = Measured::default();
    if right {
        measured.detected_after_ns = after(onset, onset);
        measured.isolated_after_ns = after(onset, onset);
    } else {
        measured.wrong_verdicts = 1;
    }
    let elsewhere = matches!(isolation, Isolation::Move { node, .. } if node != NodeId(0));
    let mut on_node = false;
    for to in STRONGER {
        let at = Demand {
            need: Resources {
                mem_bytes: with_overhead(&overheads, to, need.mem_bytes)?,
                ..need
            },
            kind: Kind::Plain,
            class: to,
            ask: Default::default(),
        };
        on_node |= stopped.verdict(NodeId(0), &at).map_err(FaultError::Sim)? == Verdict::Admits;
    }
    let rescued = matches!(isolation, Isolation::Move { .. }) && !on_node;
    Ok(Dangerous {
        measured,
        isolation,
        elsewhere,
        rescued,
    })
}

/// What a correlated fault did to a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Correlated {
    pub nodes: u64,
    /// Workloads placed in the cell before the fault.
    pub workloads: u64,
    /// Nodes ejected, and the most out at once.
    pub ejected: u64,
    pub max_ejected: u64,
    /// Drains refused because the rest of the cell couldn't re-place the node's work (non-vacuity:
    /// the cell filled).
    pub refused: u64,
    /// Workloads moved off ejected nodes, and those left with no node (the criterion: none).
    pub re_placed: u64,
    pub stranded: u64,
    /// Nodes whose work was moved after a mass loss, and the most moved in any `window` stretch.
    pub moved: u64,
    pub max_moved_in_window: u64,
    pub window_ns: u64,
    pub allowed_in_window: u64,
}

/// The cell as it would stand with `node` taken out of placement: its workloads given back and its
/// whole capacity fenced, then each re-placed through the fast path in turn. Returns the cell, where
/// each went, and how many no node admitted at their turn (`admitted_anywhere`).
fn without(
    index: &FitIndex,
    node: NodeId,
    work: &[(WorkloadId, Demand)],
    pick: &Placement,
) -> Result<(FitIndex, Vec<Option<NodeId>>, u64), FaultError> {
    let mut rest = index.try_clone().map_err(FaultError::Sim)?;
    for (id, w) in work {
        rest.give(node, *id, w).map_err(FaultError::Sim)?;
    }
    fence(&mut rest, node)?;
    let mut to = Vec::with_capacity(work.len());
    let mut stranded: u64 = 0;
    for (id, w) in work {
        if !rest.admitted_anywhere(w) {
            stranded = stranded.saturating_add(1);
            to.push(None);
            continue;
        }
        let at = rest.fit(w, pick).best.map(|k| k.node);
        match at {
            Some(at) => {
                rest.take(at, *id, w).map_err(FaultError::Sim)?;
            }
            None => stranded = stranded.saturating_add(1),
        }
        to.push(at);
    }
    Ok((rest, to, stranded))
}

/// Takes `node`'s whole free capacity, so nothing more is placed there.
fn fence(index: &mut FitIndex, node: NodeId) -> Result<(), FaultError> {
    let k = index.key(node).map_err(FaultError::Sim)?;
    let all = Demand {
        need: Resources {
            cpu_milli: k.cpu_milli,
            mem_bytes: k.mem_bytes,
            gpus: k.gpus,
            disk_bytes: 0,
        },
        kind: Kind::Plain,
        class: Class::Oci,
        ask: Default::default(),
    };
    index
        .take(node, WorkloadId(u64::MAX), &all)
        .map_err(FaultError::Sim)?;
    Ok(())
}

/// Correlated faults (F6): a cell of 20-200 of G2's node models carries a seeded load (1-6 workloads
/// a node asked, of the three kinds, placed through the fast path where they fit); a third to two
/// fifths of it turns slow at once (a bad rollout, a noisy shared dependency). Each slow node's drain
/// ejects it only where the rest of the cell re-places its every workload at that moment, measured on
/// a copy of the cell with the node fenced; otherwise the ejector refuses it ([`Refusal::NoRoom`]) and
/// it stays in placement, graded. Then most of the cell is lost at once (a partition), its work moved
/// through the eviction limiter.
///
/// [`Refusal::NoRoom`]: hex_health::penalty::Refusal::NoRoom
pub fn correlated(seed: u64, mutant: Mutant) -> Result<Correlated, FaultError> {
    let mut rng = Rng::new(seed);
    let nodes = rng.range(20, 200);
    let slow_share = rng.range(300, 400);
    let overheads = kind_overheads();
    let mut specs = Vec::new();
    for id in 0..nodes {
        let (classes, capacity) = g2_node(&mut rng);
        specs.push(NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: node_id(id)?,
            capacity,
            background_milli: 0,
            classes,
            platform: Default::default(),
            virt: Default::default(),
            devices: crate::model::unnamed_gpus(capacity.gpus),
        });
    }
    let mut index = FitIndex::new(&specs).map_err(FaultError::Sim)?;
    let mut work: Vec<Vec<(WorkloadId, Demand)>> = vec![Vec::new(); specs.len()];
    let pick = Placement::default();
    let asked = nodes.saturating_mul(rng.range(1, 6));
    let mut out = Correlated {
        nodes,
        ..Correlated::default()
    };
    for id in (0..asked).map(WorkloadId) {
        let w = g2_workload(&mut rng, &overheads)?;
        if let Some(k) = index.fit(&w, &pick).best {
            index.take(k.node, id, &w).map_err(FaultError::Sim)?;
            if let Some(on) = usize::try_from(k.node.0).ok().and_then(|i| work.get_mut(i)) {
                on.push((id, w));
                out.workloads = out.workloads.saturating_add(1);
            }
        }
    }
    let mut g = PeerGrade::new(specs.len(), 0);
    let mut ejector = Ejector::new(specs.len());
    let mut changes = Vec::new();
    let mut t: u64 = 0;
    while t < 10 * S {
        for n in 0..nodes {
            let at = add(t, n % (100 * MS))?;
            let slow = n.saturating_mul(1000) < nodes.saturating_mul(slow_share) && at >= 2 * S;
            let s = if slow {
                noise(&mut rng).saturating_mul(4)
            } else {
                noise(&mut rng)
            };
            changes.clear();
            g.observe(n, at, s, &mut changes)?;
            for c in changes.iter().filter(|c| c.level == Level::Drain) {
                if ejector.is_ejected(c.node, at) {
                    continue;
                }
                let node = node_id(c.node)?;
                let i = usize::try_from(c.node).map_err(|_| {
                    FaultError::Sim(crate::SimError::Config("a node id past usize"))
                })?;
                let on = work.get(i).cloned().unwrap_or_default();
                let (rest, to, stranded) = without(&index, node, &on, &pick)?;
                let re_placeable = stranded == 0 || mutant == Mutant::NoReplacementCheck;
                if ejector.eject(c.node, at, re_placeable)?.is_err() {
                    out.refused = out.refused.saturating_add(1);
                    continue;
                }
                out.ejected = out.ejected.saturating_add(1);
                out.stranded = out.stranded.saturating_add(stranded);
                index = rest;
                if let Some(left) = work.get_mut(i) {
                    left.clear();
                }
                for (w, to) in on.into_iter().zip(to) {
                    let Some(to) = to else { continue };
                    if let Some(there) = usize::try_from(to.0).ok().and_then(|j| work.get_mut(j)) {
                        there.push(w);
                        out.re_placed = out.re_placed.saturating_add(1);
                    }
                }
            }
            out.max_ejected = out.max_ejected.max(ejector.ejected_at(at));
        }
        t = add(t, 100 * MS)?;
    }
    // The partition: 55-90% of the cell declared dead at one instant; their work moves through the
    // limiter, which is asked again each 100 ms for 10 minutes.
    let lost = nodes.saturating_mul(rng.range(550, 900)) / 1000;
    let mut limiter = EvictionLimiter::new(t);
    let mut pending = lost;
    let mut moves: Vec<u64> = Vec::new();
    let stop = add(t, 600 * S)?;
    while t < stop && pending > 0 {
        let granted = mutant == Mutant::NoEvictionRate || limiter.take(t, nodes, lost)?.is_ok();
        if granted {
            pending = pending.saturating_sub(1);
            moves.push(t);
            continue;
        }
        t = add(t, 100 * MS)?;
    }
    out.moved = u64::try_from(moves.len()).unwrap_or(u64::MAX);
    out.window_ns = 100 * S;
    let rate = EvictionLimiter::rate(nodes, lost);
    // One token to start with, then the rate's worth in a window.
    out.allowed_in_window = 1u64.saturating_add(rate.saturating_mul(100) / 1000);
    let mut lo = 0usize;
    for (hi, &at) in moves.iter().enumerate() {
        while moves
            .get(lo)
            .is_some_and(|&first| first.saturating_add(out.window_ns) <= at)
        {
            lo = lo.saturating_add(1);
        }
        let in_window = u64::try_from(hi.saturating_sub(lo).saturating_add(1)).unwrap_or(u64::MAX);
        out.max_moved_in_window = out.max_moved_in_window.max(in_window);
    }
    Ok(out)
}

/// The disk stall's bounds (F3): marked within the earlier of its probe's two ages and one period of
/// the onset, drained within its drain age and one period (and the driver's millisecond).
pub fn disk_stall_bounds(j: &StallJudge) -> (u64, u64) {
    let first = j.slowest_ns.min(j.drain_age_ns);
    let suspect = first.saturating_add(j.period_ns).saturating_add(MS);
    let drain = j
        .drain_age_ns
        .saturating_add(j.period_ns)
        .saturating_add(MS);
    (suspect, drain)
}
