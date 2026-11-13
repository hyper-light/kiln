//! The scheduler's serial specification (`scheduler-architecture.md` S6): one scheduler, one cycle.
//! Workloads queue in the order the store accepted them, one queue per band and owner; whenever state
//! changes, the scheduler takes the first queue in policy order ([`crate::fair`]: band, then weighted
//! dominant share, then owner) in order, and puts each workload on the best fit its class's pick finds
//! ([`FitIndex::fit`], the fast path's own choice), passing over one no node holds now and placing nothing past that queue when none of its work holds. A placement is correct when this specification could have made it.
//!
//! [`serial_place`] runs the specification directly on a cluster whose capacity only shrinks.
//! [`check_log`] replays a run's store log and refuses the first write the specification couldn't
//! have made: the model-based oracle of `T-T7.4.1`, in the shape of QuickCheck's state-machine model
//! checked at every step (DS[10], DS[11] in `docs/research/scheduler/deterministic-simulation.md`).
//! [`check_slices`] does the same for each slice of a sharded control plane (H60), on the slice's own
//! nodes and the workloads routed to it.
//! [`check_end`] counts what `AC-T7.3.3` holds to zero: workloads lost or started twice.
//!
//! Counters here saturate: each counts a run's workloads or writes, bounded by the store far below
//! `u64::MAX`, so saturation is the unreachable case's stand-in, never a value a report shows.

use std::collections::BTreeMap;

use crate::error::SimError;
use crate::fair::{self, Passed, Placed, Preemption, Queue, Shares};
use crate::index::{FitIndex, Pick};
use crate::model::{
    Class, Cluster, Demand, DeviceKind, GroupId, Kind, NodeId, NodeSpec, Resources, WorkloadId,
    WorkloadSpec, bind,
};
use crate::sim::{Layout, Run};
use crate::store::{Entry, Op, State};

/// A write the serial specification couldn't have made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    #[error("{0:?} was submitted twice")]
    SubmittedTwice(WorkloadId),
    #[error("{0:?} was placed, completed or checked without having been submitted")]
    NotSubmitted(WorkloadId),
    #[error("{0:?} was placed while not pending")]
    NotPending(WorkloadId),
    #[error(
        "{placed:?} was placed while {ahead:?}, ahead of it in policy order, waited for a node"
    )]
    JumpedQueue {
        placed: WorkloadId,
        ahead: WorkloadId,
    },
    #[error("{workload:?} went to {chose:?}; the specification puts it on {spec:?}")]
    NotSerialChoice {
        workload: WorkloadId,
        chose: NodeId,
        spec: Option<NodeId>,
    },
    #[error("{0:?} completed on a node it wasn't placed on")]
    CompletedElsewhere(WorkloadId),
    #[error("{0:?} was left pending while a node held it")]
    LeftPending(WorkloadId),
    #[error("{0:?} was placed apart from the rest of its gang")]
    PartialGang(WorkloadId),
    #[error("a preemption put {0:?} past its disruption budget")]
    BudgetBroken(GroupId),
    #[error("{0:?}'s preemption is not the one the specification makes")]
    NotSerialPreemption(WorkloadId),
    #[error("{0:?} was left pending while it could preempt within the rules")]
    Inversion(WorkloadId),
    #[error("{workload:?} was routed to slice {slice} and written on {node:?}, outside it")]
    OutsideSlice {
        workload: WorkloadId,
        node: NodeId,
        slice: usize,
    },
    #[error("{workload:?} exceeded a runtime pool on {node:?}")]
    RuntimePoolExceeded { workload: WorkloadId, node: NodeId },
    #[error("the replay itself failed: {0}")]
    Replay(SimError),
}

impl From<SimError> for Violation {
    fn from(e: SimError) -> Self {
        Violation::Replay(e)
    }
}

/// The specification run directly: each workload, in order, on its pick's best or none.
pub fn serial_place(
    nodes: &[NodeSpec],
    workloads: &[WorkloadSpec],
    pick: &Pick,
) -> Result<Vec<Option<NodeId>>, SimError> {
    let mut index = FitIndex::new(nodes)?;
    let mut out = Vec::with_capacity(workloads.len());
    for w in workloads {
        let choice = index.fit(&w.demand(), pick).best.map(|k| k.node);
        if let Some(node) = choice {
            index.take(node, w.id, &w.demand())?;
        }
        out.push(choice);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogReport {
    pub submits: u64,
    pub placements: u64,
    pub completions: u64,
    /// Workloads preempted, each within its group's budget.
    pub preempted: u64,
    /// Workloads still pending at the end that no node holds: waiting, correctly.
    pub waiting: u64,
}

/// Replays `run`'s store log against the specification. Each placement must be the best fit at that
/// point, nothing may wait in a queue ahead of it in policy order, no earlier workload of its own
/// queue may have had a node, and nothing may be left pending at the end that a node holds unless a
/// queue ahead of it waits. A gang is placed in one write, whole, at its first member's place in its
/// queue, each member on the best fit left by those before it. Its cost is each placement times the
/// pending workloads ahead of it times the node count: an oracle for test-sized runs.
pub fn check_log(nodes: &[NodeSpec], run: &Run) -> Result<LogReport, Violation> {
    check_entries(nodes, run, run.store.log())
}

/// [`check_log`] for each of the run's slices: the log's writes of the workloads routed to it, its
/// nodes by their ids within it. A write on a node outside its workload's slice is refused.
pub fn check_slices(cluster: &Cluster, run: &Run) -> Result<Vec<LogReport>, Violation> {
    let shards = u32::try_from(run.planes.len()).map_err(|_| SimError::Config("shards"))?;
    check_layout(&Layout::new(&cluster.nodes, shards)?, run)
}

/// [`check_slices`] over a given layout, for a run that kept no planes (a bench's own store).
pub fn check_layout(layout: &Layout, run: &Run) -> Result<Vec<LogReport>, Violation> {
    let mut reports = Vec::with_capacity(layout.len());
    for s in 0..layout.len() {
        let nodes = layout.nodes(s).ok_or(SimError::Config("slice index"))?;
        let mut entries = Vec::new();
        for e in run.store.log() {
            if let Some(op) = in_slice(layout, run, s, e.op)? {
                entries.push(Entry { op, ..*e });
            }
        }
        reports.push(check_entries(nodes, run, &entries)?);
    }
    Ok(reports)
}

/// `op` in slice `s`'s node ids, if its workload was routed there.
fn in_slice(layout: &Layout, run: &Run, s: usize, op: Op) -> Result<Option<Op>, Violation> {
    let workload = match op {
        Op::Submit { workload, .. }
        | Op::Place { workload, .. }
        | Op::Complete { workload, .. }
        | Op::Preempt { workload, .. } => workload,
    };
    let spec = run
        .store
        .record(workload)
        .ok_or(Violation::NotSubmitted(workload))?
        .spec;
    if layout.route(&spec) != Some(s) {
        return Ok(None);
    }
    let local = |node: NodeId| {
        layout.local(s, node).ok_or(Violation::OutsideSlice {
            workload,
            node,
            slice: s,
        })
    };
    Ok(Some(match op {
        Op::Submit { .. } => op,
        Op::Place { workload, node } => Op::Place {
            workload,
            node: local(node)?,
        },
        Op::Complete { workload, node } => Op::Complete {
            workload,
            node: local(node)?,
        },
        Op::Preempt { workload, node } => Op::Preempt {
            workload,
            node: local(node)?,
        },
    }))
}

/// [`check_log`] over `log`, a run's whole log or one slice's part of it, its nodes `nodes`.
fn check_entries(nodes: &[NodeSpec], run: &Run, log: &[Entry]) -> Result<LogReport, Violation> {
    let mut index = FitIndex::new(nodes)?;
    let mut pools = PoolLedger::new(nodes);
    let mut shares = Shares::new(nodes, run.owners.iter().map(|o| o.weight).collect())?;
    let mut pending: Pending = BTreeMap::new();
    let mut seq_of: BTreeMap<WorkloadId, u64> = BTreeMap::new();
    // Each placed workload's node and the write that placed it.
    let mut placed: BTreeMap<WorkloadId, (NodeId, u64)> = BTreeMap::new();
    let mut disrupted: BTreeMap<GroupId, u32> = BTreeMap::new();
    let mut preempted: BTreeMap<WorkloadId, Option<GroupId>> = BTreeMap::new();
    let mut report = LogReport::default();
    let mut at = 0usize;
    while let Some(entry) = log.get(at) {
        at = at.saturating_add(1);
        match entry.op {
            Op::Submit { workload, seq } => {
                let spec = spec_of(run, workload)?;
                if seq_of.insert(workload, seq).is_some() {
                    return Err(Violation::SubmittedTwice(workload));
                }
                pending.insert(seq, (workload, spec.demand(), run.policy.queue(&spec)));
                report.submits = report.submits.saturating_add(1);
            }
            Op::Place { workload, node } => {
                // The write's placements: one, or a gang's.
                let mut write = vec![(workload, node)];
                while let Some(next) = log.get(at).filter(|e| e.write == entry.write) {
                    let Op::Place { workload, node } = next.op else {
                        break;
                    };
                    write.push((workload, node));
                    at = at.saturating_add(1);
                }
                let gang = spec_of(run, workload)?.tenancy.gang;
                let members: Vec<WorkloadId> = match gang {
                    Some(g) => run
                        .gangs
                        .get(&g)
                        .cloned()
                        .ok_or(Violation::PartialGang(workload))?,
                    None => vec![workload],
                };
                if write.iter().map(|(w, _)| *w).collect::<Vec<_>>() != members {
                    return Err(Violation::PartialGang(workload));
                }
                let mut seqs = Vec::with_capacity(write.len());
                for (w, _) in &write {
                    let seq = *seq_of.get(w).ok_or(Violation::NotSubmitted(*w))?;
                    if !pending.contains_key(&seq) {
                        return Err(Violation::NotPending(*w));
                    }
                    seqs.push(seq);
                }
                let first = seqs.iter().copied().min().unwrap_or(u64::MAX);
                let (_, _, queue) = *pending.get(&first).ok_or(Violation::NotPending(workload))?;
                // Ahead: anything in a queue the policy puts first (whether or not it holds, as
                // allocation stops at that queue), or earlier in its own queue and holding.
                let rank = shares.rank(run.policy, queue);
                let earlier: Vec<(u64, WorkloadId, Demand, Queue)> = pending
                    .iter()
                    .filter(|(s, (_, _, q))| {
                        shares.rank(run.policy, *q) < rank || (*q == queue && **s < first)
                    })
                    .map(|(s, (w, d, q))| (*s, *w, *d, *q))
                    .collect();
                let state = Now {
                    run,
                    pending: &pending,
                    placed: &placed,
                    shares: &shares,
                    disrupted: &disrupted,
                };
                for (_, ahead, d, q) in earlier {
                    if q != queue || state.goes(&mut index, ahead, &d)? {
                        return Err(Violation::JumpedQueue {
                            placed: workload,
                            ahead,
                        });
                    }
                }
                let mut demands = Vec::with_capacity(write.len());
                for (w, _) in &write {
                    demands.push((*w, spec_of(run, *w)?.demand()));
                }
                for ((w, node), (_, demand)) in write.iter().zip(&demands) {
                    if !pools.change(nodes, *node, demand, true)? {
                        return Err(Violation::RuntimePoolExceeded {
                            workload: *w,
                            node: *node,
                        });
                    }
                }
                let chosen = crate::sim::fit_whole(&mut index, &demands, &run.pick)?;
                for (i, ((w, node), (_, need))) in write.iter().zip(&demands).enumerate() {
                    let spec = chosen.as_ref().and_then(|c| c.get(i)).copied();
                    if spec != Some(*node) {
                        return Err(Violation::NotSerialChoice {
                            workload: *w,
                            chose: *node,
                            spec,
                        });
                    }
                    index.take(*node, *w, need)?;
                    shares.take(owner_of(run, *w)?, &need.need)?;
                    placed.insert(*w, (*node, entry.write));
                    placed_again(&mut preempted, &mut disrupted, *w);
                    report.placements = report.placements.saturating_add(1);
                }
                for seq in seqs {
                    pending.remove(&seq);
                }
            }
            Op::Preempt { workload, node } => {
                // The write: its victims, then the preemptor's placement.
                let mut victims = vec![(workload, node)];
                let mut head = None;
                while let Some(next) = log.get(at).filter(|e| e.write == entry.write) {
                    at = at.saturating_add(1);
                    match next.op {
                        Op::Preempt { workload, node } if head.is_none() => {
                            victims.push((workload, node));
                        }
                        Op::Place { workload, node } if head.is_none() => {
                            head = Some((workload, node));
                        }
                        _ => return Err(Violation::NotSerialPreemption(workload)),
                    }
                }
                let (id, node) = head.ok_or(Violation::NotSerialPreemption(workload))?;
                let spec = spec_of(run, id)?;
                let seq = *seq_of.get(&id).ok_or(Violation::NotSubmitted(id))?;
                let (_, need, queue) = *pending.get(&seq).ok_or(Violation::NotPending(id))?;
                let rank = shares.rank(run.policy, queue);
                let earlier: Vec<(WorkloadId, Demand, Queue)> = pending
                    .iter()
                    .filter(|(s, (_, _, q))| {
                        shares.rank(run.policy, *q) < rank || (*q == queue && **s < seq)
                    })
                    .map(|(_, (w, d, q))| (*w, *d, *q))
                    .collect();
                let state = Now {
                    run,
                    pending: &pending,
                    placed: &placed,
                    shares: &shares,
                    disrupted: &disrupted,
                };
                for (ahead, d, q) in earlier {
                    if q != queue || state.goes(&mut index, ahead, &d)? {
                        return Err(Violation::JumpedQueue { placed: id, ahead });
                    }
                }
                let want = Preemption {
                    node,
                    victims: victims.iter().map(|(w, _)| *w).collect(),
                };
                let serial = state.preemption(&mut index, &spec)?;
                // Budgets first: a preemption past one is that, whatever else is wrong with it.
                for (w, n) in victims {
                    if placed.remove(&w).map(|(at, _)| at) != Some(n) {
                        return Err(Violation::NotSerialPreemption(id));
                    }
                    let victim = spec_of(run, w)?;
                    pools.change(nodes, n, &victim.demand(), false)?;
                    index.give(n, victim.id, &victim.demand())?;
                    shares.give(victim.tenancy.owner, &victim.need)?;
                    let s = *seq_of.get(&w).ok_or(Violation::NotSubmitted(w))?;
                    pending.insert(s, (w, victim.demand(), run.policy.queue(&victim)));
                    preempted.insert(w, victim.tenancy.group);
                    if let Some(g) = victim.tenancy.group {
                        let d = disrupted.entry(g).or_default();
                        *d = d.saturating_add(1);
                        let budget = usize::try_from(g.0)
                            .ok()
                            .and_then(|at| run.groups.get(at))
                            .map_or(0, |g| g.budget);
                        if *d > budget {
                            return Err(Violation::BudgetBroken(g));
                        }
                    }
                    report.preempted = report.preempted.saturating_add(1);
                }
                if !run.preemption || serial != Some(want) {
                    return Err(Violation::NotSerialPreemption(id));
                }
                if !pools.change(nodes, node, &need, true)? {
                    return Err(Violation::RuntimePoolExceeded { workload: id, node });
                }
                let chosen = crate::sim::fit_whole(&mut index, &[(id, need)], &run.pick)?;
                let spec_node = chosen.as_ref().and_then(|c| c.first()).copied();
                if spec_node != Some(node) {
                    return Err(Violation::NotSerialChoice {
                        workload: id,
                        chose: node,
                        spec: spec_node,
                    });
                }
                index.take(node, id, &need)?;
                shares.take(spec.tenancy.owner, &spec.need)?;
                placed.insert(id, (node, entry.write));
                placed_again(&mut preempted, &mut disrupted, id);
                pending.remove(&seq);
                report.placements = report.placements.saturating_add(1);
            }
            Op::Complete { workload, node } => {
                if placed.remove(&workload).map(|(at, _)| at) != Some(node) {
                    return Err(Violation::CompletedElsewhere(workload));
                }
                let need = spec_of(run, workload)?.demand();
                pools.change(nodes, node, &need, false)?;
                index.give(node, workload, &need)?;
                shares.give(owner_of(run, workload)?, &need.need)?;
                report.completions = report.completions.saturating_add(1);
            }
        }
    }
    let left: Vec<(WorkloadId, Demand, Queue)> = pending.values().copied().collect();
    let state = Now {
        run,
        pending: &pending,
        placed: &placed,
        shares: &shares,
        disrupted: &disrupted,
    };
    for (workload, need, queue) in left {
        let rank = shares.rank(run.policy, queue);
        let blocked = pending
            .values()
            .any(|(_, _, q)| shares.rank(run.policy, *q) < rank);
        if !blocked && holds(&mut index, run, &pending, workload, &need)? {
            return Err(Violation::LeftPending(workload));
        }
        if !blocked
            && run.preemption
            && state
                .preemption(&mut index, &spec_of(run, workload)?)?
                .is_some()
        {
            return Err(Violation::Inversion(workload));
        }
        report.waiting = report.waiting.saturating_add(1);
    }
    Ok(report)
}

/// The specification's state at one point of the log, for what may go there.
struct Now<'a> {
    run: &'a Run,
    pending: &'a Pending,
    placed: &'a BTreeMap<WorkloadId, (NodeId, u64)>,
    shares: &'a Shares,
    disrupted: &'a BTreeMap<GroupId, u32>,
}

impl Now<'_> {
    /// Whether `workload` would go now: a node holds it, or it can preempt.
    fn goes(
        &self,
        index: &mut FitIndex,
        workload: WorkloadId,
        need: &Demand,
    ) -> Result<bool, Violation> {
        if holds(index, self.run, self.pending, workload, need)? {
            return Ok(true);
        }
        Ok(self.run.preemption
            && self
                .preemption(index, &spec_of(self.run, workload)?)?
                .is_some())
    }

    /// The preemption the specification makes for `spec` now, if any ([`fair::choose_victims`]).
    fn preemption(
        &self,
        index: &mut FitIndex,
        spec: &WorkloadSpec,
    ) -> Result<Option<Preemption>, Violation> {
        let mut list = Vec::with_capacity(self.placed.len());
        for (&id, &(node, write)) in self.placed {
            let p = spec_of(self.run, id)?;
            list.push(Placed {
                id,
                node,
                demand: p.demand(),
                owner: p.tenancy.owner,
                band: p.tenancy.band,
                group: p.tenancy.group,
                in_gang: p.tenancy.gang.is_some(),
                write,
            });
        }
        Ok(fair::choose_victims(
            index,
            &list,
            spec,
            self.shares,
            &self.run.groups,
            self.disrupted,
            &mut Passed::default(),
        )?)
    }
}

/// A placement of `workload` ends its disruption, if it was preempted.
fn placed_again(
    preempted: &mut BTreeMap<WorkloadId, Option<GroupId>>,
    disrupted: &mut BTreeMap<GroupId, u32>,
    workload: WorkloadId,
) {
    if let Some(Some(g)) = preempted.remove(&workload)
        && let Some(d) = disrupted.get_mut(&g)
    {
        *d = d.saturating_sub(1);
        if *d == 0 {
            disrupted.remove(&g);
        }
    }
}

/// Pending workloads by submit sequence: each one's id, class and queue.
type Pending = BTreeMap<u64, (WorkloadId, Demand, Queue)>;

/// Whether a node holds `workload` now: alone, or, in a gang, with every member pending and the
/// whole gang placed member by member.
fn holds(
    index: &mut FitIndex,
    run: &Run,
    pending: &Pending,
    workload: WorkloadId,
    need: &Demand,
) -> Result<bool, Violation> {
    let Some(gang) = spec_of(run, workload)?.tenancy.gang else {
        return Ok(index.admitted_anywhere(need));
    };
    let members = run
        .gangs
        .get(&gang)
        .ok_or(Violation::PartialGang(workload))?;
    let mut demands = Vec::with_capacity(members.len());
    for m in members {
        let Some(record) = run.store.record(*m) else {
            return Ok(false);
        };
        if !pending.contains_key(&record.seq) {
            return Ok(false);
        }
        demands.push((*m, record.spec.demand()));
    }
    Ok(crate::sim::fit_whole(index, &demands, &run.pick)?.is_some())
}

fn spec_of(run: &Run, workload: WorkloadId) -> Result<crate::model::WorkloadSpec, Violation> {
    run.store
        .record(workload)
        .map(|r| r.spec)
        .ok_or(Violation::NotSubmitted(workload))
}

/// What `AC-T7.5.1`'s gangs come to over a run's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GangReport {
    /// Gangs placed.
    pub placed: u64,
    /// Writes after which some gang had members placed and members not: zero when gangs are whole.
    pub partial: u64,
}

/// Counts gangs placed, and every write that left a gang part placed.
pub fn check_gangs(run: &Run) -> GangReport {
    let mut placed_members: BTreeMap<crate::model::GangId, u64> = BTreeMap::new();
    let mut report = GangReport::default();
    let log = run.store.log();
    let mut at = 0usize;
    while let Some(entry) = log.get(at) {
        let mut touched = Vec::new();
        while let Some(e) = log.get(at).filter(|e| e.write == entry.write) {
            at = at.saturating_add(1);
            if let Op::Place { workload, .. } = e.op
                && let Some(g) = run.store.record(workload).and_then(|r| r.spec.tenancy.gang)
            {
                let n = placed_members.entry(g).or_default();
                *n = n.saturating_add(1);
                touched.push(g);
            }
        }
        touched.dedup();
        for g in touched {
            let size = run
                .gangs
                .get(&g)
                .map_or(0, |m| u64::try_from(m.len()).unwrap_or(u64::MAX));
            match placed_members.get(&g).copied().unwrap_or(0) {
                n if n == size => report.placed = report.placed.saturating_add(1),
                _ => report.partial = report.partial.saturating_add(1),
            }
        }
    }
    report
}

fn owner_of(run: &Run, workload: WorkloadId) -> Result<crate::model::OwnerId, Violation> {
    run.store
        .record(workload)
        .map(|r| r.spec.tenancy.owner)
        .ok_or(Violation::NotSubmitted(workload))
}

/// What `AC-T7.3.3` counts, over every workload of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EndReport {
    /// Acknowledged to its client, then never placed nor waiting, or placed and its run never
    /// recorded as done.
    pub lost: u64,
    /// Started on a node more than once.
    pub started_twice: u64,
    /// Refused at admission because no node could ever hold it.
    pub refused: u64,
    pub complete: u64,
    pub running: u64,
    pub waiting: u64,
}

impl EndReport {
    pub fn lost_or_double(&self) -> u64 {
        self.lost.saturating_add(self.started_twice)
    }
}

pub fn check_end(cluster: &Cluster, run: &Run) -> EndReport {
    let mut r = EndReport::default();
    for (at, w) in cluster.workloads.iter().enumerate() {
        let starts = run.starts.get(at).copied().unwrap_or(0);
        let acked = run.acked.get(at).copied().unwrap_or(false);
        let refused = run.refused.get(at).copied().unwrap_or(false);
        let record = run.store.record(w.id);
        // Each placement starts it once: a preempted workload placed again starts again.
        let placements = record.map_or(0, |rec| rec.placements);
        if starts > placements.max(1) {
            r.started_twice = r.started_twice.saturating_add(1);
        }
        let ok = match record.map(|rec| rec.state) {
            None => {
                r.refused = r.refused.saturating_add(u64::from(refused));
                refused && starts == 0
            }
            Some(State::Pending) => {
                r.waiting = r.waiting.saturating_add(1);
                acked && starts == placements
            }
            Some(State::Placed(_)) => {
                r.running = r.running.saturating_add(1);
                acked && starts == placements && w.runs_for.is_none()
            }
            Some(State::Complete(_)) => {
                r.complete = r.complete.saturating_add(1);
                acked && starts == placements
            }
        };
        if !ok && starts <= placements.max(1) {
            r.lost = r.lost.saturating_add(1);
        }
    }
    r
}

/// What `AC-T7.3.4` holds: control-plane restarts leave running work running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeptReport {
    /// Workloads that ran, each started once on the node it was placed on, ran its whole run time
    /// there, and is recorded done there (or still running there, for one that never ends).
    pub kept: u64,
    /// Workloads that ran and broke any of that.
    pub disturbed: u64,
}

pub fn check_kept_running(cluster: &Cluster, run: &Run) -> KeptReport {
    let mut r = KeptReport::default();
    for (at, w) in cluster.workloads.iter().enumerate() {
        let Some(Some(ran)) = run.ran.get(at).copied() else {
            continue;
        };
        let once = run.starts.get(at).copied() == Some(1);
        let state = run.store.record(w.id).map(|rec| rec.state);
        let whole = match w.runs_for {
            Some(d) => {
                ran.started
                    .checked_add(d)
                    .is_some_and(|end| ran.ended == Some(end))
                    && state == Some(State::Complete(ran.node))
            }
            None => ran.ended.is_none() && state == Some(State::Placed(ran.node)),
        };
        if once && whole {
            r.kept = r.kept.saturating_add(1);
        } else {
            r.disturbed = r.disturbed.saturating_add(1);
        }
    }
    r
}

/// What step 2's filters hold to zero over a run's placements (`AC-T7.6.3`, S3), counted by a replay
/// of the log against the cluster's facts alone, apart from [`FitIndex`]'s own verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterReport {
    pub placements: u64,
    /// GPU work placed where it gets none: on a node without GPUs, or of a class a GPU can't be given
    /// to ([`crate::model::Class::takes_gpus`]).
    pub gpu_off_gpu: u64,
    /// A benchmark placed beside a running build, or a build beside a running benchmark.
    pub benchmark_with_build: u64,
    /// A session placed on a node above 90% CPU while another node had the room and wasn't.
    pub hot_sessions: u64,
    /// A workload placed on a node that can't run its class.
    pub class_off_node: u64,
    /// A workload placed where its class cannot run the image OS or native architecture.
    pub platform_off: u64,
    /// A workload placed where the devices it asks weren't free and matching (one held twice, one of
    /// another model or with too little memory), or of a class that can't be given them there.
    pub device_off: u64,
    /// Placements that held a device or were given KVM: the device filter's non-vacuity.
    pub with_devices: u64,
    /// A placement past its matching runtime pool, replayed independently of FitIndex.
    pub runtime_pool_off: u64,
}

impl FilterReport {
    pub fn violations(&self) -> u64 {
        self.gpu_off_gpu
            .saturating_add(self.benchmark_with_build)
            .saturating_add(self.hot_sessions)
            .saturating_add(self.class_off_node)
            .saturating_add(self.platform_off)
            .saturating_add(self.device_off)
            .saturating_add(self.runtime_pool_off)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    used: Resources,
    builds: u64,
    benchmarks: u64,
}

/// Independent invariant ledger: replay class amounts from writes, never query Slot/verdict.
/// Older host-only clusters allocate no extra ledger, retaining their original replay path.
struct PoolLedger {
    by_node: Vec<[Resources; Class::COUNT]>,
}

impl PoolLedger {
    fn new(nodes: &[NodeSpec]) -> Self {
        Self {
            by_node: if nodes.iter().any(|node| !node.runtime_pools.is_empty()) {
                vec![[Resources::default(); Class::COUNT]; nodes.len()]
            } else {
                Vec::new()
            },
        }
    }

    fn change(
        &mut self,
        nodes: &[NodeSpec],
        node: NodeId,
        demand: &Demand,
        placing: bool,
    ) -> Result<bool, Violation> {
        if self.by_node.is_empty() {
            return Ok(true);
        }
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let facts = nodes.get(at).ok_or(SimError::UnknownNode(node))?;
        let row = self
            .by_node
            .get_mut(at)
            .ok_or(SimError::UnknownNode(node))?;
        let amount = row
            .get_mut(demand.class.index())
            .ok_or(SimError::Resources(node))?;
        *amount = if placing {
            amount.checked_add(&demand.need)
        } else {
            amount.checked_sub(&demand.need)
        }
        .ok_or(SimError::Resources(node))?;
        for pool in &facts.runtime_pools {
            let mut total = Resources::default();
            for class in Class::ALL {
                if pool.classes.has(class) {
                    total = total
                        .checked_add(row.get(class.index()).ok_or(SimError::Resources(node))?)
                        .ok_or(SimError::Resources(node))?;
                }
            }
            if !pool.capacity.holds(&total) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Above 90%: busy * 10 > capacity * 9, in u128 so neither side can overflow.
fn over_ninety(capacity: &NodeSpec, used: &Resources) -> bool {
    let busy = u128::from(used.cpu_milli).saturating_add(u128::from(capacity.background_milli));
    busy.saturating_mul(10) > u128::from(capacity.capacity.cpu_milli).saturating_mul(9)
}

pub fn check_filters(cluster: &Cluster, run: &Run) -> Result<FilterReport, Violation> {
    for node in &cluster.nodes {
        node.check_runtime_pools()?;
    }
    let mut pools = PoolLedger::new(&cluster.nodes);
    let mut seen = vec![Seen::default(); cluster.nodes.len()];
    // Each node's devices' holders, replayed from the log: a device is held from its workload's
    // placement until its completion or preemption.
    let mut held: Vec<Vec<Option<WorkloadId>>> = cluster
        .nodes
        .iter()
        .map(|n| vec![None; n.devices.len()])
        .collect();
    let mut r = FilterReport::default();
    let at = |node: NodeId| {
        usize::try_from(node.0).map_err(|_| Violation::Replay(SimError::UnknownNode(node)))
    };
    for entry in run.store.log() {
        let (workload, node, placing) = match entry.op {
            Op::Submit { .. } => continue,
            Op::Place { workload, node } => (workload, node, true),
            Op::Complete { workload, node } | Op::Preempt { workload, node } => {
                (workload, node, false)
            }
        };
        let spec = run
            .store
            .record(workload)
            .ok_or(Violation::NotSubmitted(workload))?
            .spec;
        let i = at(node)?;
        let facts = cluster
            .nodes
            .get(i)
            .ok_or(Violation::Replay(SimError::UnknownNode(node)))?;
        if !pools.change(&cluster.nodes, node, &spec.demand(), placing)? && placing {
            r.runtime_pool_off = r.runtime_pool_off.saturating_add(1);
        }
        if placing {
            let now = seen.get(i).copied().unwrap_or_default();
            r.placements = r.placements.saturating_add(1);
            if spec.need.gpus > 0
                && (facts.capacity.gpus == 0
                    || !spec.class.exposes(facts.platform.os, DeviceKind::Gpu))
            {
                r.gpu_off_gpu = r.gpu_off_gpu.saturating_add(1);
            }
            if !facts.classes.has(spec.class) {
                r.class_off_node = r.class_off_node.saturating_add(1);
            }
            let (ask, os) = (&spec.ask, facts.platform.os);
            // Independent hard-filter oracle: containers share their host OS; VM images run Linux.
            let image_os = if spec.class == crate::model::Class::Oci {
                os
            } else {
                crate::model::Os::Linux
            };
            if ask.os.is_some_and(|o| o != image_os)
                || ask.arch.is_some_and(|a| a != facts.platform.arch)
            {
                r.platform_off = r.platform_off.saturating_add(1);
            }
            if spec.need.gpus > 0 || ask.devices() {
                let exposed = [
                    (u64::from(spec.need.gpus), DeviceKind::Gpu),
                    (u64::from(ask.vfio), DeviceKind::Vfio),
                    (u64::from(ask.block), DeviceKind::Block),
                    (u64::from(ask.net), DeviceKind::Net),
                ]
                .iter()
                .all(|&(n, kind)| n == 0 || spec.class.exposes(os, kind))
                    && (!ask.kvm || (facts.virt.kvm && spec.class.gives_kvm(os)));
                let holders = held
                    .get_mut(i)
                    .ok_or(Violation::Replay(SimError::UnknownNode(node)))?;
                let bound = bind(
                    &facts.devices,
                    |d| {
                        holders.get(d).is_some_and(Option::is_none)
                            && facts
                                .devices
                                .get(d)
                                .is_some_and(|device| device.classes.has(spec.class))
                    },
                    spec.need.gpus,
                    ask,
                );
                match bound {
                    Some(bound) if exposed => {
                        for d in bound {
                            if let Some(h) = holders.get_mut(usize::from(d)) {
                                *h = Some(workload);
                            }
                        }
                        r.with_devices = r.with_devices.saturating_add(1);
                    }
                    _ => r.device_off = r.device_off.saturating_add(1),
                }
            }
            let shares = match spec.kind {
                Kind::Benchmark => now.builds > 0,
                Kind::Build => now.benchmarks > 0,
                Kind::Plain | Kind::Session => false,
            };
            if shares {
                r.benchmark_with_build = r.benchmark_with_build.saturating_add(1);
            }
            if spec.kind == Kind::Session && over_ninety(facts, &now.used) {
                let elsewhere = cluster.nodes.iter().zip(&seen).any(|(other, o)| {
                    other.id != node
                        && other
                            .capacity
                            .checked_sub(&o.used)
                            .is_some_and(|free| free.holds(&spec.need))
                        && !over_ninety(other, &o.used)
                });
                if elsewhere {
                    r.hot_sessions = r.hot_sessions.saturating_add(1);
                }
            }
        }
        if !placing {
            for h in held.get_mut(i).into_iter().flatten() {
                if *h == Some(workload) {
                    *h = None;
                }
            }
        }
        let slot = seen
            .get_mut(i)
            .ok_or(Violation::Replay(SimError::UnknownNode(node)))?;
        let step = |n: u64| {
            if placing {
                n.saturating_add(1)
            } else {
                n.saturating_sub(1)
            }
        };
        slot.used = if placing {
            slot.used.checked_add(&spec.need)
        } else {
            slot.used.checked_sub(&spec.need)
        }
        .ok_or(Violation::Replay(SimError::Resources(node)))?;
        match spec.kind {
            Kind::Build => slot.builds = step(slot.builds),
            Kind::Benchmark => slot.benchmarks = step(slot.benchmarks),
            Kind::Plain | Kind::Session => {}
        }
    }
    Ok(r)
}
