//! Kueue's cohort sharing as a competitor to measure G4 against (`T-T7.5.1`,
//! `docs/design/scheduler-fairness.md` F5): a serial model, one admission cycle at every arrival
//! and completion, with no decision or commit time, so it is compared with itself (with and
//! without a burst), never with the control plane's latencies.
//!
//! Each owner is a ClusterQueue in one cohort, its nominal quota the cluster's capacity times its
//! weight over the sum of the weights, with borrowing from the cohort unlimited ([KueuePre] in the
//! design record). Within a ClusterQueue work waits in BestEffortFIFO order (band, then submit
//! order; work that can't go is passed over). Each cycle takes every ClusterQueue's first workload
//! that can go, admits the one that needs no borrowing first, then the higher band, then the earlier
//! submit, and goes again. A workload goes if a node holds it, or if its ClusterQueue stays within
//! its nominal quota with it and preempting workloads of borrowing ClusterQueues frees a node for it
//! (`reclaimWithinCohort: Any`), newest first, on the node needing fewest victims. Admission places
//! on the best fit, as the cluster's scheduler would.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use crate::error::SimError;
use crate::index::{FitIndex, Pick, Verdict};
use crate::model::{Cluster, Demand, NodeId, OwnerId, Resources, Time};

/// What a run of the model did: each workload's first start, and its preemptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// By workload index: when it first started, if it did.
    pub started: Vec<Option<Time>>,
    pub preempted: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    /// Workload `at`'s run number `run` ends.
    End {
        at: usize,
        run: u32,
    },
    Arrive {
        at: usize,
    },
}

/// Runs the model over `cluster`'s workloads. `max_cycles` bounds the admissions and preemptions it
/// makes; past it the run is refused, typed.
pub fn kueue_cohort(cluster: &Cluster, max_cycles: u64) -> Result<Outcome, SimError> {
    let n = cluster.workloads.len();
    let mut index = FitIndex::new(&cluster.nodes)?;
    let whole = Pick::default();
    let mut total = Resources::default();
    for node in &cluster.nodes {
        total = total
            .checked_add(&node.capacity)
            .ok_or(SimError::Resources(node.id))?;
    }
    let weights = cluster
        .owners
        .iter()
        .try_fold(0u64, |a, o| a.checked_add(u64::from(o.weight)))
        .ok_or(SimError::Config("owner weights"))?;
    let nominal: Vec<Resources> = cluster
        .owners
        .iter()
        .map(|o| scale(&total, u64::from(o.weight), weights))
        .collect::<Result<_, _>>()?;
    let mut usage = vec![Resources::default(); cluster.owners.len()];
    // Pending by owner: (band first, highest first; then submit order) -> workload index.
    let mut pending: BTreeMap<u32, BTreeSet<(Reverse<u8>, usize)>> = BTreeMap::new();
    // Running: workload index -> (node, run number, start).
    let mut running: BTreeMap<usize, (NodeId, u32, Time)> = BTreeMap::new();
    let mut runs = vec![0u32; n];
    let mut out = Outcome {
        started: vec![None; n],
        preempted: 0,
    };
    let mut events: BinaryHeap<Reverse<(Time, Event)>> = BinaryHeap::new();
    for (at, w) in cluster.workloads.iter().enumerate() {
        events.push(Reverse((w.arrives, Event::Arrive { at })));
    }
    let mut cycles = 0u64;
    while let Some(Reverse((now, event))) = events.pop() {
        match event {
            Event::Arrive { at } => {
                let w = cluster
                    .workloads
                    .get(at)
                    .ok_or(SimError::Config("workload index"))?;
                pending
                    .entry(w.tenancy.owner.0)
                    .or_default()
                    .insert((Reverse(w.tenancy.band), at));
            }
            Event::End { at, run } => {
                if runs.get(at).copied() != Some(run) {
                    continue;
                }
                let Some((node, _, _)) = running.remove(&at) else {
                    continue;
                };
                let w = cluster
                    .workloads
                    .get(at)
                    .ok_or(SimError::Config("workload index"))?;
                index.give(node, w.id, &w.demand())?;
                give(&mut usage, w.tenancy.owner, &w.need)?;
            }
        }
        // One admission cycle, until nothing more can go.
        loop {
            cycles = cycles
                .checked_add(1)
                .ok_or(SimError::StepBudget(max_cycles))?;
            if cycles > max_cycles {
                return Err(SimError::StepBudget(max_cycles));
            }
            let mut best: Option<(Rank, u32, Go)> = None;
            // Classes found unable to go this cycle, by owner: the same need of the same owner
            // goes no better further down its queue.
            let mut stuck: BTreeSet<(u32, Demand)> = BTreeSet::new();
            for (&owner, queue) in &pending {
                for &(band, at) in queue {
                    let w = cluster
                        .workloads
                        .get(at)
                        .ok_or(SimError::Config("workload index"))?;
                    if stuck.contains(&(owner, w.demand())) {
                        continue;
                    }
                    let o = usize::try_from(owner).map_err(|_| SimError::Config("owner"))?;
                    let with = usage
                        .get(o)
                        .ok_or(SimError::Config("owner"))?
                        .checked_add(&w.need)
                        .ok_or(SimError::Config("usage"))?;
                    let within = nominal.get(o).is_some_and(|q| q.holds(&with));
                    let go = if let Some(key) = index.fit(&w.demand(), &whole).best {
                        Go::Fits(key.node)
                    } else if within {
                        match reclaim(&mut index, cluster, &running, &usage, &nominal, at)? {
                            Some((node, victims)) => Go::Preempts(node, victims),
                            None => {
                                stuck.insert((owner, w.demand()));
                                continue;
                            }
                        }
                    } else {
                        stuck.insert((owner, w.demand()));
                        continue;
                    };
                    let key = (!within, band, at);
                    if best.as_ref().is_none_or(|(k, _, _)| key < *k) {
                        best = Some((key, owner, go));
                    }
                    break;
                }
            }
            let Some(((_, band, at), owner, go)) = best else {
                break;
            };
            let w = cluster
                .workloads
                .get(at)
                .ok_or(SimError::Config("workload index"))?;
            let node = match go {
                Go::Fits(node) => node,
                Go::Preempts(node, victims) => {
                    for v in victims {
                        let Some((on, _, _)) = running.remove(&v) else {
                            return Err(SimError::Config("a victim not running"));
                        };
                        let victim = cluster
                            .workloads
                            .get(v)
                            .ok_or(SimError::Config("workload index"))?;
                        index.give(on, victim.id, &victim.demand())?;
                        give(&mut usage, victim.tenancy.owner, &victim.need)?;
                        pending
                            .entry(victim.tenancy.owner.0)
                            .or_default()
                            .insert((Reverse(victim.tenancy.band), v));
                        out.preempted = out
                            .preempted
                            .checked_add(1)
                            .ok_or(SimError::StepBudget(max_cycles))?;
                    }
                    node
                }
            };
            if let Some(q) = pending.get_mut(&owner) {
                q.remove(&(band, at));
                if q.is_empty() {
                    pending.remove(&owner);
                }
            }
            index.take(node, w.id, &w.demand())?;
            let o = usize::try_from(w.tenancy.owner.0).map_err(|_| SimError::Config("owner"))?;
            let u = usage.get_mut(o).ok_or(SimError::Config("owner"))?;
            *u = u.checked_add(&w.need).ok_or(SimError::Config("usage"))?;
            let run = runs.get_mut(at).ok_or(SimError::Config("workload index"))?;
            *run = run.checked_add(1).ok_or(SimError::Config("runs"))?;
            running.insert(at, (node, *run, now));
            if let Some(first) = out.started.get_mut(at) {
                first.get_or_insert(now);
            }
            if let Some(d) = w.runs_for {
                let end = now.checked_add(d).ok_or(SimError::TimeOverflow)?;
                events.push(Reverse((end, Event::End { at, run: *run })));
            }
        }
    }
    Ok(out)
}

/// Where a ClusterQueue's head stands in a cycle: needing to borrow last, then by band, highest
/// first, then by submit order.
type Rank = (bool, Reverse<u8>, usize);

/// How a workload goes: on a node with room, or on one freed by preempting the workloads named.
enum Go {
    Fits(NodeId),
    Preempts(NodeId, Vec<usize>),
}

/// Victims of borrowing ClusterQueues (usage past nominal in any resource) that free a node for
/// workload `at`, newest first on each node, on the node needing fewest; `index` is left as it was.
fn reclaim(
    index: &mut FitIndex,
    cluster: &Cluster,
    running: &BTreeMap<usize, (NodeId, u32, Time)>,
    usage: &[Resources],
    nominal: &[Resources],
    at: usize,
) -> Result<Option<(NodeId, Vec<usize>)>, SimError> {
    let head = cluster
        .workloads
        .get(at)
        .ok_or(SimError::Config("workload index"))?;
    let demand = head.demand();
    let borrowing = |owner: OwnerId| {
        let o = usize::try_from(owner.0).ok();
        match (o.and_then(|o| usage.get(o)), o.and_then(|o| nominal.get(o))) {
            (Some(u), Some(q)) => !q.holds(u),
            _ => false,
        }
    };
    let mut by_node: BTreeMap<NodeId, Vec<(Reverse<Time>, usize)>> = BTreeMap::new();
    for (&v, &(node, _, start)) in running {
        let w = cluster
            .workloads
            .get(v)
            .ok_or(SimError::Config("workload index"))?;
        if w.tenancy.owner != head.tenancy.owner && borrowing(w.tenancy.owner) {
            by_node.entry(node).or_default().push((Reverse(start), v));
        }
    }
    let mut best: Option<(NodeId, Vec<usize>)> = None;
    for (node, mut candidates) in by_node {
        candidates.sort_unstable();
        let mut taken = Vec::new();
        let mut fits = false;
        for (_, v) in candidates {
            if best.as_ref().is_some_and(|(_, b)| taken.len() >= b.len()) {
                break;
            }
            let w = cluster
                .workloads
                .get(v)
                .ok_or(SimError::Config("workload index"))?;
            index.give(node, w.id, &w.demand())?;
            taken.push(v);
            if index.verdict(node, &demand)? == Verdict::Admits {
                fits = true;
                break;
            }
        }
        for &v in taken.iter().rev() {
            let w = cluster
                .workloads
                .get(v)
                .ok_or(SimError::Config("workload index"))?;
            index.take(node, w.id, &w.demand())?;
        }
        if fits && best.as_ref().is_none_or(|(_, b)| taken.len() < b.len()) {
            best = Some((node, taken));
        }
    }
    Ok(best)
}

fn give(usage: &mut [Resources], owner: OwnerId, need: &Resources) -> Result<(), SimError> {
    let o = usize::try_from(owner.0).map_err(|_| SimError::Config("owner"))?;
    let u = usage.get_mut(o).ok_or(SimError::Config("owner"))?;
    *u = u.checked_sub(need).ok_or(SimError::Config("usage"))?;
    Ok(())
}

/// `total * part / whole`, per resource, rounded down.
fn scale(total: &Resources, part: u64, whole: u64) -> Result<Resources, SimError> {
    let f = |x: u64| {
        u128::from(x)
            .checked_mul(u128::from(part))
            .and_then(|y| y.checked_div(u128::from(whole)))
            .and_then(|y| u64::try_from(y).ok())
            .ok_or(SimError::Config("a nominal quota"))
    };
    Ok(Resources {
        cpu_milli: f(total.cpu_milli)?,
        mem_bytes: f(total.mem_bytes)?,
        gpus: u32::try_from(f(u64::from(total.gpus))?)
            .map_err(|_| SimError::Config("a nominal quota"))?,
        disk_bytes: 0,
    })
}
