//! Proximity-scored placement against proximity-blind placement on a modelled WAN (scheduler design
//! S4 and A2; `hex-sim.md` H21): the goodput and completion-time tail each buys.
//!
//! **Workloads and nodes are recorded data** (`hex-sim.md` H10). [`Study::generate`] takes a recorded
//! workload, G2's of record (Huawei Cloud's Region 5, day 30, `bench/sched/huawei-r5-day30.trace`, and
//! its `.fleet` twin), and runs it as it is: its node rows, each with its capacity and the kinds it
//! runs (ten EC2 m5d.metal hosts running all three kinds in the main trace, Hex's own fleet's Linux,
//! Windows and macOS hosts in the `.fleet` one), and every launch with its owner, its kind (an OCI
//! container, a Firecracker microVM or a Shards microVM, given by G2's rule: a kind per owner, in
//! thirds of the day's launches) and its need, its kind's overhead included as the trace states it. A
//! job is placed only on a node that runs its kind.
//!
//! Each job talks to a home, the node holding its data or service, and makes its round trips to it,
//! their waits each the WAN's true RTT from the node it was placed on to its home, added to its
//! compute. Agent sessions are such workloads: TraceLab counts 101.4 tool calls a session on average,
//! p50 25 and p99 1,438 (`docs/research/scheduler/agentic-workloads.md` §1, its [5]), and tool calls
//! are 30-85% of production requests' latency (its [34]); round trips are drawn log-normal through
//! that p50 and p99. Homes sit in a region drawn uniformly over the regions some node is in.
//!
//! **Policies**, each over the fast path's own sample ([`FitIndex::fit`]'s window and its
//! admitting nodes, so they see the same candidates):
//! - [`Policy::Blind`]: the fast path's best fit, proximity ignored.
//! - [`Policy::Proximity`]: the least RTT to the workload's home that hyper-swim's coordinates
//!   predict; best fit breaks ties. A soft score: every admitting
//!   node stays a candidate.
//! - [`Policy::Oracle`]: as `Proximity` with the true RTT: the most a perfect coordinate could buy.
//!
//! **Load and compute are the day's own.** Every launch of the recorded day runs, in its order, at
//! its recorded arrival; its compute is its recorded lifetime, and its round trips' waits are added to
//! it, each the WAN's true RTT from its node to its home (Azure's table), so the RTT-to-compute ratio
//! is the data's. The study runs at the recorded day's size, on its own node model.
//!
//! **Measures.** The horizon is the day's last arrival. Goodput is the work finished by it:
//! workloads, and useful CPU time (CPU × compute, the round trips' waits excluded). Completion time is arrival to end, every
//! workload run to its end. The queue is first come, first served: the head waits until it fits.
//! Each measure is reported for all workloads and for each kind.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::time::Duration;

use crate::error::SimError;
use crate::index::{FitIndex, Key, Pick, Verdict};
use crate::model::{
    Class, Cluster, Demand, Kind, NodeId, NodeSpec, OwnerId, Resources, Time, WorkloadId,
};
use crate::rng::Rng;
use crate::trace::{Latencies, Quantiles, TraceHash};
use crate::wan::{Access, Coordinates, INPUT_MARGIN, REGIONS, Wan, WanConfig, node_id};

/// TraceLab's tool calls per session, p50 and p99 (agentic-workloads.md §1, [5]).
pub const ROUND_TRIPS_P50: f64 = 25.0;
pub const ROUND_TRIPS_P99: f64 = 1_438.0;
/// The standard normal's 99th percentile.
const Z99: f64 = 2.326_347_874;
/// The census's largest recorded day: `huawei-r5-day30.trace`'s 25,295 launches (its header).
pub const CENSUS_LAUNCHES: usize = 25_295;
/// The most workloads a study runs, by the input-bound rule; the queue and the event heap are
/// bounded by it.
pub const MAX_WORKLOADS: usize = CENSUS_LAUNCHES * INPUT_MARGIN;
/// Hex's three kinds, in the order reports give them.
pub const CLASSES: [Class; 3] = [Class::Oci, Class::Firecracker, Class::Shards];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Policy {
    Blind,
    Proximity,
    Oracle,
}

/// Where workloads' homes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Homes {
    /// A region drawn uniformly: the record's, which claims nothing about where homes sit.
    Uniform,
    /// Sensitivity only, never the record: half in the first region (a modelled share), the rest
    /// uniformly over all of them.
    Skewed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StudyConfig {
    pub access: Access,
    pub homes: Homes,
    pub pick: Pick,
    pub seed: u64,
}

/// One workload of a study.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    /// What its recorded launch asked, its kind's overhead included.
    pub need: Resources,
    pub owner: OwnerId,
    pub class: Class,
    /// Its recorded workload kind (what the fast path's filters keep apart).
    pub kind: Kind,
    pub home: NodeId,
    pub round_trips: u64,
    pub arrives: Time,
    /// Its recorded lifetime, taken as its compute.
    pub compute: Time,
}

/// A study's cluster: its WAN, the coordinates its nodes learned, and its workloads, shared by every
/// policy run over it.
pub struct Study {
    pub config: StudyConfig,
    pub wan: Wan,
    pub coordinates: Coordinates,
    pub nodes: Vec<NodeSpec>,
    pub jobs: Vec<Job>,
    pub horizon: Time,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyReport {
    pub policy: Policy,
    pub placements: u64,
    /// Decisions whose node differed from the one blind placement would have chosen then.
    pub moved: u64,
    /// Workloads, and useful CPU time (milli-CPU nanoseconds), finished by the horizon.
    pub done_by_horizon: u64,
    pub useful_by_horizon: u128,
    pub horizon: Time,
    /// Arrival to end, every workload.
    pub completion: Option<Quantiles>,
    /// The same two measures for each kind, in [`CLASSES`]' order.
    pub classes: [ClassReport; 3],
    /// The true RTT to home of each placement, summed.
    pub rtt_sum_ns: u128,
    /// The longest the queue grew.
    pub queue_peak: usize,
    pub hash: u64,
}

/// One kind's share of a [`PolicyReport`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClassReport {
    pub placements: u64,
    pub done_by_horizon: u64,
    pub completion: Option<Quantiles>,
}

impl PolicyReport {
    /// One kind's workloads finished by the horizon per second of it.
    pub fn class_goodput_per_s(&self, class: Class) -> f64 {
        self.classes.get(class_at(class)).map_or(0.0, |c| {
            c.done_by_horizon as f64 / (self.horizon as f64 / 1e9)
        })
    }

    /// Workloads finished by the horizon per second of it.
    pub fn goodput_per_s(&self) -> f64 {
        self.done_by_horizon as f64 / (self.horizon as f64 / 1e9)
    }

    /// Useful CPU cores kept busy by work finished by the horizon, averaged over it.
    pub fn useful_cores(&self) -> f64 {
        self.useful_by_horizon as f64 / 1e3 / self.horizon as f64
    }

    pub fn mean_rtt_ms(&self) -> f64 {
        if self.placements == 0 {
            return 0.0;
        }
        self.rtt_sum_ns as f64 / self.placements as f64 / 1e6
    }
}

impl Study {
    /// A study of `recorded` as it is: its node rows, and every launch at its arrival.
    pub fn generate(config: &StudyConfig, recorded: &Cluster) -> Result<Self, SimError> {
        if recorded.nodes.is_empty() || recorded.workloads.is_empty() {
            return Err(SimError::Config(
                "a recorded workload with nodes and launches",
            ));
        }
        if recorded.workloads.len() > MAX_WORKLOADS {
            return Err(SimError::Config(
                "a study runs at most MAX_WORKLOADS launches",
            ));
        }
        let mut rng = Rng::new(config.seed);
        let wan_config = WanConfig {
            nodes: recorded.nodes.len(),
            regions: REGIONS.len(),
            access: config.access,
            seed: config.seed,
        };
        let wan = Wan::generate(&wan_config, &mut rng)?;
        let coordinates = Coordinates::converge(&wan, config.seed, &mut rng)?;
        let mut nodes = Vec::with_capacity(wan.len());
        for (at, row) in recorded.nodes.iter().enumerate() {
            nodes.push(NodeSpec {
                runtime_pools: Vec::new(),
                id: node_id(at)?,
                ..row.clone()
            });
        }
        // Each region's nodes, to draw a home in a region.
        let regions = REGIONS.len();
        let mut by_region: Vec<Vec<NodeId>> = vec![Vec::new(); regions];
        for node in &nodes {
            let site = wan.site(node.id)?;
            by_region
                .get_mut(site.region)
                .ok_or(SimError::UnknownNode(node.id))?
                .push(node.id);
        }
        // Regions some node is in (a small node model leaves some empty).
        by_region.retain(|members| !members.is_empty());
        let last_region = u64::try_from(by_region.len())
            .ok()
            .and_then(|r| r.checked_sub(1))
            .ok_or(SimError::Config("regions"))?;
        let sigma = (ROUND_TRIPS_P99 / ROUND_TRIPS_P50).ln() / Z99;
        let mut jobs = Vec::with_capacity(recorded.workloads.len());
        for launch in &recorded.workloads {
            let compute = launch
                .runs_for
                .ok_or(SimError::Config("a launch with no recorded lifetime"))?;
            let region = match config.homes {
                Homes::Skewed if rng.chance(500) => 0,
                Homes::Uniform | Homes::Skewed => rng.range(0, last_region),
            };
            let members = by_region
                .get(usize::try_from(region).map_err(|_| SimError::Config("regions"))?)
                .ok_or(SimError::Config("regions"))?;
            let last = u64::try_from(members.len())
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or(SimError::Config("a region with no nodes"))?;
            let home = *members
                .get(usize::try_from(rng.range(0, last)).map_err(|_| SimError::Config("ids"))?)
                .ok_or(SimError::Config("regions"))?;
            // Bounded without a clamp: `normal`'s uniforms are at least 2^-53, so |z| is at most
            // √(106·ln 2) ≈ 8.57 and the draw at most 25·e^(σ·8.57) ≈ 7.7·10^7.
            let round_trips = whole(ROUND_TRIPS_P50 * (sigma * normal(&mut rng)).exp())
                .ok_or(SimError::Config("a round-trip draw past u64"))?
                .max(1);
            jobs.push(Job {
                need: launch.need,
                owner: launch.tenancy.owner,
                class: launch.class,
                kind: launch.kind,
                home,
                round_trips,
                arrives: launch.arrives,
                compute,
            });
        }
        jobs.sort_by_key(|j| j.arrives);
        let horizon = jobs
            .last()
            .map(|j| j.arrives)
            .filter(|&h| h > 0)
            .ok_or(SimError::Config("a recorded day of some length"))?;
        Ok(Self {
            config: *config,
            wan,
            coordinates,
            nodes,
            jobs,
            horizon,
        })
    }

    /// Runs every job under `policy` to its end.
    pub fn run(&self, policy: Policy) -> Result<PolicyReport, SimError> {
        let mut index = FitIndex::new(&self.nodes)?;
        let mut report = PolicyReport {
            policy,
            placements: 0,
            moved: 0,
            done_by_horizon: 0,
            useful_by_horizon: 0,
            horizon: self.horizon,
            completion: None,
            classes: [ClassReport::default(); 3],
            rtt_sum_ns: 0,
            queue_peak: 0,
            hash: 0,
        };
        let mut hash = TraceHash::default();
        let mut queue: VecDeque<usize> = VecDeque::with_capacity(self.jobs.len());
        // Ends: (time, job, node). Bounded by the jobs, each placed once.
        let mut ends: BinaryHeap<Reverse<(Time, usize, NodeId)>> =
            BinaryHeap::with_capacity(self.jobs.len());
        let mut completion = Vec::with_capacity(self.jobs.len());
        let mut by_class: [Vec<Time>; 3] = Default::default();
        let mut next = 0usize;
        // Each job arrives once and ends once: the loop's turns are bounded by twice the jobs.
        let budget = self.jobs.len().saturating_mul(2).saturating_add(1);
        for _ in 0..budget {
            let arrival = self.jobs.get(next).map(|j| j.arrives);
            let end = ends.peek().map(|Reverse((t, _, _))| *t);
            let now = match (arrival, end) {
                (None, None) => break,
                // An end at the same time as an arrival goes first: its room is free for it.
                (Some(a), Some(e)) if e <= a => self.end(&mut ends, &mut index)?,
                (Some(a), _) => {
                    queue.push_back(next);
                    next = next.saturating_add(1);
                    report.queue_peak = report.queue_peak.max(queue.len());
                    a
                }
                (None, Some(_)) => self.end(&mut ends, &mut index)?,
            };
            // Place from the head while it fits.
            while let Some(&job) = queue.front() {
                let spec = self.jobs.get(job).ok_or(SimError::Config("jobs"))?;
                let demand = demand(spec);
                let Some(node) = self.choose(policy, &index, &demand, spec.home, &mut report)?
                else {
                    break;
                };
                queue.pop_front();
                index.take(
                    node,
                    WorkloadId(u64::try_from(job).unwrap_or(u64::MAX)),
                    &demand,
                )?;
                let rtt = self.wan.rtt_ns(node, spec.home)?;
                let ends_at = now
                    .checked_add(run_ns(spec.round_trips, spec.compute, rtt)?)
                    .ok_or(SimError::TimeOverflow)?;
                ends.push(Reverse((ends_at, job, node)));
                let took = ends_at
                    .checked_sub(spec.arrives)
                    .ok_or(SimError::TimeOverflow)?;
                completion.push(took);
                let at = class_at(spec.class);
                by_class
                    .get_mut(at)
                    .ok_or(SimError::Config("classes"))?
                    .push(took);
                let class = report
                    .classes
                    .get_mut(at)
                    .ok_or(SimError::Config("classes"))?;
                class.placements = class.placements.saturating_add(1);
                if ends_at <= self.horizon {
                    class.done_by_horizon = class.done_by_horizon.saturating_add(1);
                }
                report.placements = report.placements.saturating_add(1);
                report.rtt_sum_ns = report.rtt_sum_ns.saturating_add(u128::from(rtt));
                if ends_at <= self.horizon {
                    report.done_by_horizon = report.done_by_horizon.saturating_add(1);
                    let compute =
                        u128::from(spec.compute).saturating_mul(u128::from(spec.need.cpu_milli));
                    report.useful_by_horizon = report.useful_by_horizon.saturating_add(compute);
                }
                for word in [
                    now,
                    u64::try_from(job).unwrap_or(u64::MAX),
                    u64::from(node.0),
                ] {
                    hash.word(word);
                }
            }
        }
        if !queue.is_empty() || next < self.jobs.len() || !ends.is_empty() {
            return Err(SimError::StepBudget(
                u64::try_from(budget).unwrap_or(u64::MAX),
            ));
        }
        report.completion = Latencies::new(completion).quantiles();
        for (class, times) in report.classes.iter_mut().zip(by_class) {
            class.completion = Latencies::new(times).quantiles();
        }
        report.hash = hash.finish();
        Ok(report)
    }

    fn end(
        &self,
        ends: &mut BinaryHeap<Reverse<(Time, usize, NodeId)>>,
        index: &mut FitIndex,
    ) -> Result<Time, SimError> {
        let Reverse((at, job, node)) = ends.pop().ok_or(SimError::Config("no end to take"))?;
        let spec = self.jobs.get(job).ok_or(SimError::Config("jobs"))?;
        index.give(
            node,
            WorkloadId(u64::try_from(job).unwrap_or(u64::MAX)),
            &demand(spec),
        )?;
        Ok(at)
    }

    /// The node `policy` places `demand` on now, or none while no node admits it.
    fn choose(
        &self,
        policy: Policy,
        index: &FitIndex,
        demand: &Demand,
        home: NodeId,
        report: &mut PolicyReport,
    ) -> Result<Option<NodeId>, SimError> {
        let fit = index.fit(demand, &self.config.pick);
        let Some(best) = fit.best else {
            return Ok(None);
        };
        if policy == Policy::Blind {
            return Ok(Some(best.node));
        }
        let n = index.len();
        // (score, key): the least wins.
        let mut chosen: Option<(f64, Key)> = None;
        for at in 0..n {
            let node = node_id(at)?;
            if index.verdict(node, demand)? != Verdict::Admits {
                continue;
            }
            let key = index.key(node)?;
            let score = match policy {
                Policy::Blind => 0.0,
                Policy::Proximity => self.predicted(node, home)?,
                Policy::Oracle => self.wan.rtt_ns(node, home)? as f64 / 1e9,
            };
            let wins = chosen.is_none_or(|(s, k)| score < s || (score == s && key < k));
            if wins {
                chosen = Some((score, key));
            }
        }
        let node = chosen.map_or(best.node, |(_, k)| k.node);
        if node != best.node {
            report.moved = report.moved.saturating_add(1);
        }
        Ok(Some(node))
    }

    /// hyper-swim's predicted RTT from `node` to `home`, seconds, and 0 from a node to itself: a job
    /// on its own home makes its round trips there, and Vivaldi's `2·height` for a node and itself
    /// is not a round trip. Unwidened: Hyperscale's `rtt_hat + K_SIGMA·σ` with its `K_SIGMA = 2`
    /// bought no least gain over the plain prediction on the proximity sweep (`hex-sim.md` H21), and a
    /// copied multiplier is not taken (Ada, 2026-11-06).
    fn predicted(&self, node: NodeId, home: NodeId) -> Result<f64, SimError> {
        if node == home {
            return Ok(0.0);
        }
        self.coordinates.predict(node, home)
    }
}

/// `compute + round_trips × rtt`.
fn run_ns(round_trips: u64, compute: Time, rtt_ns: Time) -> Result<Time, SimError> {
    rtt_ns
        .checked_mul(round_trips)
        .and_then(|waits| waits.checked_add(compute))
        .ok_or(SimError::TimeOverflow)
}

/// What a job asks of the index: its need, as one of its kind.
fn demand(job: &Job) -> Demand {
    Demand {
        need: job.need,
        kind: job.kind,
        class: job.class,
        ask: Default::default(),
    }
}

/// Where `class` sits in [`CLASSES`] and a report's `classes`.
fn class_at(class: Class) -> usize {
    match class {
        Class::Oci => 0,
        Class::Firecracker => 1,
        Class::Shards => 2,
    }
}

/// A standard normal draw (Box and Muller, 1958).
fn normal(rng: &mut Rng) -> f64 {
    let unit = |rng: &mut Rng| {
        // 53 random bits, in (0, 1].
        ((rng.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    };
    let (u, v) = (unit(rng), unit(rng));
    (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
}

/// `x` rounded down to a whole number, or none when it is negative, not a number or past u64. Clippy
/// denies `as` from a float to an integer; `Duration`'s checked conversion reads seconds the same way.
fn whole(x: f64) -> Option<u64> {
    Duration::try_from_secs_f64(x).ok().map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Classes;

    const POLICIES: [Policy; 3] = [Policy::Blind, Policy::Proximity, Policy::Oracle];

    /// H10's two node models, each G2's recorded day on its own hosts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fleet {
        /// `huawei-r5-day30.trace`: ten EC2 m5d.metal hosts, each running all three kinds.
        AllKinds,
        /// `huawei-r5-day30.fleet.trace`: Hex's own fleet's hosts, some running only some kinds.
        Mixed,
    }

    fn recorded(fleet: Fleet) -> &'static Cluster {
        static ALL: std::sync::OnceLock<Cluster> = std::sync::OnceLock::new();
        static MIXED: std::sync::OnceLock<Cluster> = std::sync::OnceLock::new();
        let (cell, text) = match fleet {
            Fleet::AllKinds => (
                &ALL,
                include_str!("../../../bench/sched/huawei-r5-day30.trace"),
            ),
            Fleet::Mixed => (
                &MIXED,
                include_str!("../../../bench/sched/huawei-r5-day30.fleet.trace"),
            ),
        };
        cell.get_or_init(|| crate::replay::parse(text, &crate::replay::Limits::default()).unwrap())
    }

    fn generate(config: &StudyConfig, fleet: Fleet) -> Study {
        Study::generate(config, recorded(fleet)).unwrap()
    }

    /// The record's study: Azure's zone bound, no heights or jitter, uniform homes.
    fn config(seed: u64) -> StudyConfig {
        StudyConfig {
            access: Access::Bound,
            homes: Homes::Uniform,
            pick: Pick::default(),
            seed,
        }
    }

    /// S4, A2 (G3 step 2): do run every policy over one study twice; expect the same report twice,
    /// every job placed once, the queue drained, and completion quantiles for all of them.
    #[test]
    fn every_policy_runs_every_job_deterministically() {
        let study = generate(&config(5), Fleet::AllKinds);
        let again = generate(&config(5), Fleet::AllKinds);
        let n = study.jobs.len();
        assert_eq!(study.jobs, again.jobs);
        for policy in POLICIES {
            let a = study.run(policy).unwrap();
            let b = again.run(policy).unwrap();
            assert_eq!(a, b, "{policy:?}");
            assert_eq!(a.placements, u64::try_from(n).unwrap());
            assert_eq!(a.completion.unwrap().count, n);
            assert!(a.done_by_horizon > 0 && a.done_by_horizon < a.placements);
            for (class, got) in CLASSES.iter().zip(&a.classes) {
                let jobs = study.jobs.iter().filter(|j| j.class == *class).count();
                assert!(jobs > 0, "{class:?}");
                assert_eq!(got.placements, u64::try_from(jobs).unwrap(), "{class:?}");
                assert_eq!(got.completion.unwrap().count, jobs, "{class:?}");
            }
            assert_eq!(
                a.classes.iter().map(|c| c.done_by_horizon).sum::<u64>(),
                a.done_by_horizon
            );
        }
    }

    /// AC-T7.4.3's kinds (H10), S4: do generate a study of each recorded fleet; expect its jobs to be
    /// the recorded day's launches, every one (owner, class, workload kind, need with overhead,
    /// arrival, and lifetime as compute), its nodes the recorded node rows in order, and on the fleet trace some nodes that
    /// run only some kinds, every kind on some node.
    #[test]
    fn jobs_and_nodes_are_the_recorded_ones() {
        for fleet in [Fleet::AllKinds, Fleet::Mixed] {
            let rec = recorded(fleet);
            let study = generate(&config(3), fleet);
            let mut want: Vec<_> = rec
                .workloads
                .iter()
                .map(|w| {
                    (
                        w.arrives,
                        w.tenancy.owner,
                        w.class,
                        w.kind,
                        w.need,
                        w.runs_for,
                    )
                })
                .collect();
            let mut got: Vec<_> = study
                .jobs
                .iter()
                .map(|j| (j.arrives, j.owner, j.class, j.kind, j.need, Some(j.compute)))
                .collect();
            want.sort();
            got.sort();
            assert_eq!(got, want, "{fleet:?}");
            assert_eq!(study.horizon, want.last().unwrap().0);
            assert_eq!(study.nodes.len(), rec.nodes.len());
            for (node, row) in study.nodes.iter().zip(&rec.nodes) {
                assert_eq!((node.capacity, node.classes), (row.capacity, row.classes));
            }
            for class in CLASSES {
                assert!(
                    study.nodes.iter().any(|n| n.classes.has(class)),
                    "{fleet:?} {class:?}"
                );
            }
        }
        assert!(
            recorded(Fleet::Mixed)
                .nodes
                .iter()
                .any(|n| n.classes != Classes::ALL)
        );
        assert!(
            recorded(Fleet::AllKinds)
                .nodes
                .iter()
                .all(|n| n.classes == Classes::ALL)
        );
    }

    /// The census constant is the trace's own (a doc-truth test).
    #[test]
    fn the_census_is_the_traces() {
        assert_eq!(
            CENSUS_LAUNCHES,
            recorded(Fleet::AllKinds)
                .workloads
                .len()
                .max(recorded(Fleet::Mixed).workloads.len())
        );
    }

    /// AC-T7.4.3's kinds (H10): do read the recorded day; expect G2's rule in it: each owner's
    /// launches one kind, the kinds in the thirds the trace's header records (oci 8432, firecracker
    /// 8432, shards 8431); and a study drawn from it keeps each owner's jobs one kind.
    #[test]
    fn kinds_are_g2s_by_owner_in_thirds() {
        let mut kind_of = std::collections::HashMap::new();
        let mut got = [0u64; 3];
        for w in &recorded(Fleet::AllKinds).workloads {
            assert_eq!(*kind_of.entry(w.tenancy.owner).or_insert(w.class), w.class);
            got[class_at(w.class)] += 1;
        }
        assert_eq!(got, [8432, 8432, 8431]);
        let study = generate(&config(7), Fleet::AllKinds);
        for job in &study.jobs {
            assert_eq!(kind_of[&job.owner], job.class);
        }
    }

    /// AC-T7.4.3's kinds (H10), S4: do run a study in which no node runs Firecracker microVMs;
    /// expect every policy to refuse it with a typed error, the Firecracker workloads left queued,
    /// never placed on a node that can't run them.
    #[test]
    fn a_kind_no_node_runs_is_refused_not_misplaced() {
        let mut study = generate(&config(4), Fleet::Mixed);
        for node in &mut study.nodes {
            node.classes = Classes::NONE.with(Class::Oci).with(Class::Shards);
        }
        for policy in POLICIES {
            assert!(
                matches!(study.run(policy), Err(SimError::StepBudget(_))),
                "{policy:?}"
            );
        }
    }

    /// S4, A2 (G3 step 2): do run blind placement; expect it never to count a decision as moved
    /// from itself. (That each other policy's effect can fail its test is shown by its mutants alone:
    /// `no-proximity-term` fails `proximity_beats_blind`.)
    #[test]
    fn blind_never_moves_from_itself() {
        let study = generate(&config(6), Fleet::AllKinds);
        assert_eq!(study.run(Policy::Blind).unwrap().moved, 0);
    }

    /// S4, A2 (G3 step 2): do run blind placement, proximity and the oracle over the recorded day on
    /// each fleet; expect proximity and the oracle each to place nearer home on average than blind.
    /// (How near proximity comes to the oracle is measured, not asserted: `bench/sched/proximity.md`.)
    #[test]
    fn proximity_and_the_oracle_place_nearer_than_blind() {
        for fleet in [Fleet::AllKinds, Fleet::Mixed] {
            let study = generate(&config(1), fleet);
            let [blind, near, oracle] = POLICIES.map(|p| study.run(p).unwrap());
            assert!(near.mean_rtt_ms() < blind.mean_rtt_ms(), "{fleet:?}");
            assert!(oracle.mean_rtt_ms() < blind.mean_rtt_ms(), "{fleet:?}");
        }
    }

    /// S4, A2 (G3 step 3): do run blind and proximity-scored placement over the recorded day on each
    /// fleet; expect what the record finds on every seed: the completion time's p99 and p50 lower over
    /// all workloads, and goodput, overall and for each kind, no lower (at the day's own load every
    /// launch finishes within it under either policy, so goodput does not move:
    /// `bench/sched/proximity.md`). Mutant `no-proximity-term` (proximity scored as 0) fails it.
    #[test]
    fn proximity_beats_blind() {
        for fleet in [Fleet::AllKinds, Fleet::Mixed] {
            let study = generate(&config(1), fleet);
            let blind = study.run(Policy::Blind).unwrap();
            let near = study.run(Policy::Proximity).unwrap();
            let (b, n) = (blind.completion.unwrap(), near.completion.unwrap());
            assert!(n.p99 < b.p99, "{fleet:?}");
            assert!(n.p50 < b.p50, "{fleet:?}");
            assert!(near.done_by_horizon >= blind.done_by_horizon, "{fleet:?}");
            for (class, (b, n)) in CLASSES.iter().zip(blind.classes.iter().zip(&near.classes)) {
                assert!(
                    n.done_by_horizon >= b.done_by_horizon,
                    "{fleet:?} {class:?}"
                );
            }
        }
    }

    /// S4, A2: do ask the study for a node's predicted RTT to itself on every seed the record's
    /// Shards miss was found on; expect 0, the true RTT of a job placed on its own home, never
    /// Vivaldi's `2·height` (which on seed 15 put node 6 at 246 ms from itself, above its 195 ms to
    /// node 4, so proximity never placed a job on its own home there).
    #[test]
    fn a_node_is_predicted_zero_from_itself() {
        for fleet in [Fleet::AllKinds, Fleet::Mixed] {
            let study = generate(&config(15), fleet);
            for node in &study.nodes {
                assert_eq!(study.predicted(node.id, node.id).unwrap(), 0.0, "{fleet:?}");
            }
        }
    }

    /// Hostile recorded workloads are refused, typed: no launches, no nodes, a launch with no
    /// recorded lifetime, and more launches than the input bound.
    #[test]
    fn hostile_recorded_workloads_are_refused() {
        let rec = recorded(Fleet::AllKinds);
        let mut no_lifetime = rec.clone();
        no_lifetime.workloads[0].runs_for = None;
        let mut too_many = rec.clone();
        while too_many.workloads.len() <= MAX_WORKLOADS {
            too_many.workloads.extend_from_slice(&rec.workloads);
        }
        for bad in [
            Cluster {
                workloads: Vec::new(),
                ..rec.clone()
            },
            Cluster {
                nodes: Vec::new(),
                ..rec.clone()
            },
            no_lifetime,
            too_many,
        ] {
            assert!(matches!(
                Study::generate(&config(1), &bad),
                Err(SimError::Config(_))
            ));
        }
    }
}
