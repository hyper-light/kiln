//! Fair share between owners (`docs/design/scheduler-fairness.md` F1): which queue goes first, and
//! how a run's shares are measured (`AC-T7.5.2`).
//!
//! The control plane and its serial specification both take queues in [`Policy`] order: by band,
//! highest first, then (under [`Policy::Drf`]) by owner, lowest weighted dominant share first
//! (Ghodsi et al., NSDI 2011, §4 and §4.3), then by submit order within a queue.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use crate::error::SimError;
use crate::index::{FitIndex, Verdict};
use crate::model::{
    Cluster, Demand, Group, GroupId, NodeId, NodeSpec, OwnerId, Resources, Time, WorkloadId,
    WorkloadSpec,
};
use crate::sim::Ran;

/// How pending work is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// One queue in submit order: the baseline G0-G2 measured against.
    Fifo,
    /// Bands, then weighted DRF between owners, then submit order.
    #[default]
    Drf,
}

/// A queue of pending work: one band of one owner. Ordered band first, highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Queue {
    pub band: Reverse<u8>,
    pub owner: OwnerId,
}

impl Policy {
    /// The queue `spec` waits in.
    pub fn queue(&self, spec: &WorkloadSpec) -> Queue {
        match self {
            Policy::Fifo => Queue {
                band: Reverse(0),
                owner: OwnerId(0),
            },
            Policy::Drf => Queue {
                band: Reverse(spec.tenancy.band),
                owner: spec.tenancy.owner,
            },
        }
    }
}

/// Fixed-point scale of a dominant share: 2^40 resolves a share to about 1e-12, far below one
/// millicore of a 10,000-node cluster.
const SCALE: u128 = 1 << 40;

/// Every owner's placed resources and weight, and the cluster's capacity they are shares of.
#[derive(Debug, Clone)]
pub struct Shares {
    total: Resources,
    weights: Vec<u32>,
    used: BTreeMap<OwnerId, Resources>,
}

impl Shares {
    pub fn new(cluster_nodes: &[NodeSpec], weights: Vec<u32>) -> Result<Self, SimError> {
        let mut total = Resources::default();
        for node in cluster_nodes {
            total = total
                .checked_add(&node.capacity)
                .ok_or(SimError::Resources(node.id))?;
        }
        if weights.is_empty() || weights.contains(&0) {
            return Err(SimError::Config("every owner has a weight of at least 1"));
        }
        Ok(Self {
            total,
            weights,
            used: BTreeMap::new(),
        })
    }

    pub fn of(cluster: &Cluster) -> Result<Self, SimError> {
        Self::new(
            &cluster.nodes,
            cluster.owners.iter().map(|o| o.weight).collect(),
        )
    }

    pub fn owners(&self) -> usize {
        self.weights.len()
    }

    pub fn take(&mut self, owner: OwnerId, need: &Resources) -> Result<(), SimError> {
        let used = self.used.entry(owner).or_default();
        *used = used
            .checked_add(need)
            .ok_or(SimError::Config("an owner's usage overflowed"))?;
        Ok(())
    }

    pub fn give(&mut self, owner: OwnerId, need: &Resources) -> Result<(), SimError> {
        let used = self.used.entry(owner).or_default();
        *used = used
            .checked_sub(need)
            .ok_or(SimError::Config("an owner gave back more than it held"))?;
        Ok(())
    }

    /// `owner`'s dominant share of the cluster over its weight, in units of `1 / SCALE`.
    pub fn key(&self, owner: OwnerId) -> u128 {
        self.key_of(owner, &self.held(owner))
    }

    /// What `owner` holds placed.
    pub fn held(&self, owner: OwnerId) -> Resources {
        self.used.get(&owner).copied().unwrap_or_default()
    }

    /// `owner`'s key were it to hold `used`.
    pub fn key_of(&self, owner: OwnerId, used: &Resources) -> u128 {
        let weight = usize::try_from(owner.0)
            .ok()
            .and_then(|o| self.weights.get(o))
            .copied()
            .map_or(1, u128::from);
        dominant(used, &self.total).checked_div(weight).unwrap_or(0)
    }

    /// Where `queue` stands: band first, then (under DRF) lowest weighted dominant share, then
    /// owner id. A lower rank goes first.
    pub fn rank(&self, policy: Policy, queue: Queue) -> (Reverse<u8>, u128, OwnerId) {
        let key = match policy {
            Policy::Drf => self.key(queue.owner),
            Policy::Fifo => 0,
        };
        (queue.band, key, queue.owner)
    }

    /// `queues` in policy order.
    pub fn order(&self, policy: Policy, queues: impl Iterator<Item = Queue>) -> Vec<Queue> {
        let mut out: Vec<Queue> = queues.collect();
        out.sort_by_key(|q| self.rank(policy, *q));
        out
    }
}

/// A placed workload, as preemption weighs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub id: WorkloadId,
    pub node: NodeId,
    pub demand: Demand,
    pub owner: OwnerId,
    pub band: u8,
    pub group: Option<GroupId>,
    pub in_gang: bool,
    /// The write that placed it: the larger, the newer.
    pub write: u64,
}

/// What a preemption takes: the node the preemptor goes to and the workloads taken off it, in the
/// order they were chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preemption {
    pub node: NodeId,
    pub victims: Vec<WorkloadId>,
}

/// What [`choose_victims`] passed over, for the run's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Passed {
    /// Candidates passed over because their group was at its disruption budget.
    pub for_budget: u64,
}

/// Which placed workloads `head` may preempt, and from which node (`docs/design/scheduler-fairness.md`
/// F4), or `None`: when a node already admits it, when it is in a gang, or when no node can be
/// freed for it within the rules. A candidate is in a strictly lower band, or in its band under
/// another owner whose share, less what it would lose, stays above the head's owner's share with
/// the head placed (Kueue's fair-sharing preemption rule, so no two owners preempt each other back
/// and forth); never a gang's member (a gang is taken whole or not at all); and never one whose
/// group would pass its budget of members disrupted at once, counting `disrupted` and those taken
/// here (a group `groups` doesn't name has a budget of 0). Candidates are taken borrowing first (an
/// owner above the head's owner's share), then lowest band, then newest, on each node until the
/// node admits the head; the node needing fewest victims wins, then the lowest id. `view` is left
/// as it was. Bounded by the placed workloads given: each is weighed at most once.
pub fn choose_victims(
    view: &mut FitIndex,
    placed: &[Placed],
    head: &WorkloadSpec,
    shares: &Shares,
    groups: &[Group],
    disrupted: &BTreeMap<GroupId, u32>,
    passed: &mut Passed,
) -> Result<Option<Preemption>, SimError> {
    let demand = head.demand();
    if head.tenancy.gang.is_some() || view.admitted_anywhere(&demand) {
        return Ok(None);
    }
    let owner = head.tenancy.owner;
    let with_head = shares
        .held(owner)
        .checked_add(&head.need)
        .ok_or(SimError::Config("an owner's usage overflowed"))?;
    let head_key = shares.key_of(owner, &with_head);
    let borrowing = |p: &Placed| p.owner != owner && shares.key(p.owner) > head_key;
    let mut by_node: BTreeMap<NodeId, Vec<&Placed>> = BTreeMap::new();
    for p in placed {
        let lower = p.band < head.tenancy.band;
        if !p.in_gang && (lower || (p.band == head.tenancy.band && borrowing(p))) {
            by_node.entry(p.node).or_default().push(p);
        }
    }
    let mut best: Option<Preemption> = None;
    for (node, mut candidates) in by_node {
        candidates.sort_by_key(|p| (!borrowing(p), p.band, Reverse(p.write), p.id));
        let mut taken: Vec<&Placed> = Vec::new();
        let mut lost: BTreeMap<OwnerId, Resources> = BTreeMap::new();
        let mut more: BTreeMap<GroupId, u32> = BTreeMap::new();
        let mut fits = false;
        for p in candidates {
            if best
                .as_ref()
                .is_some_and(|b| taken.len() >= b.victims.len())
            {
                break;
            }
            let lost_now = lost
                .get(&p.owner)
                .copied()
                .unwrap_or_default()
                .checked_add(&p.demand.need)
                .ok_or(SimError::Config("an owner's usage overflowed"))?;
            if p.band == head.tenancy.band {
                let left = shares
                    .held(p.owner)
                    .checked_sub(&lost_now)
                    .ok_or(SimError::Config("an owner gave back more than it held"))?;
                if shares.key_of(p.owner, &left) <= head_key {
                    continue;
                }
            }
            if let Some(g) = p.group {
                let budget = usize::try_from(g.0)
                    .ok()
                    .and_then(|at| groups.get(at))
                    .map_or(0, |g| g.budget);
                let now = disrupted.get(&g).copied().unwrap_or(0);
                let here = more.get(&g).copied().unwrap_or(0);
                if now
                    .checked_add(here)
                    .and_then(|n| n.checked_add(1))
                    .is_none_or(|n| n > budget)
                {
                    passed.for_budget = passed.for_budget.saturating_add(1);
                    continue;
                }
                more.insert(
                    g,
                    here.checked_add(1).ok_or(SimError::Config("disruptions"))?,
                );
            }
            lost.insert(p.owner, lost_now);
            view.give(node, p.id, &p.demand)?;
            taken.push(p);
            if view.verdict(node, &demand)? == Verdict::Admits {
                fits = true;
                break;
            }
        }
        for p in taken.iter().rev() {
            view.take(node, p.id, &p.demand)?;
        }
        if fits && best.as_ref().is_none_or(|b| taken.len() < b.victims.len()) {
            best = Some(Preemption {
                node,
                victims: taken.iter().map(|p| p.id).collect(),
            });
        }
    }
    Ok(best)
}

/// The largest of `used`'s fractions of `total`, over the resources the cluster has, scaled.
fn dominant(used: &Resources, total: &Resources) -> u128 {
    let share = |u: u128, t: u128| {
        u.checked_mul(SCALE)
            .and_then(|x| x.checked_div(t))
            .unwrap_or(0)
    };
    share(u128::from(used.cpu_milli), u128::from(total.cpu_milli))
        .max(share(
            u128::from(used.mem_bytes),
            u128::from(total.mem_bytes),
        ))
        .max(share(u128::from(used.gpus), u128::from(total.gpus)))
}

/// Parts per million: shares are reported in whole numbers so no float rounding enters a bar.
const PPM: u128 = 1_000_000;

/// What `AC-T7.5.2` reads off a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareReport {
    /// Virtual time during which every owner had work waiting.
    pub contended_ns: Time,
    /// Each owner's part of the dominant shares held while contended, in parts per million, so
    /// the parts sum to (about, by rounding down) a million.
    pub shares_ppm: Vec<u64>,
    /// Each owner's mean dominant share of the cluster while contended, in parts per million.
    pub of_cluster_ppm: Vec<u64>,
    /// The length of each stretch of contention, in order: at most one per event of the sweep.
    pub intervals: Vec<Time>,
}

impl ShareReport {
    /// The largest distance of any owner's part from an equal part, in parts per million.
    pub fn worst_from_equal_ppm(&self) -> u64 {
        let n = u64::try_from(self.shares_ppm.len())
            .unwrap_or(u64::MAX)
            .max(1);
        let equal = 1_000_000u64.checked_div(n).unwrap_or(0);
        self.shares_ppm
            .iter()
            .map(|s| s.abs_diff(equal))
            .max()
            .unwrap_or(0)
    }
}

/// Each owner's share while every owner had work waiting: a sweep over the run's starts, ends and
/// arrivals, so bounded by four events per workload. A workload waits from its arrival until it
/// first starts (or the run ends), unless it was refused at admission (`refused`, by index), when it
/// never waits; it holds its need from its start until its end (or the run ends).
pub fn measure(
    cluster: &Cluster,
    ran: &[Option<Ran>],
    refused: &[bool],
    end: Time,
) -> Result<ShareReport, SimError> {
    let owners = cluster.owners.len();
    let total = Shares::of(cluster)?.total;
    // (time, what, workload index): 0 an end, 1 a start, 2 a wait begun, 3 a wait over; at one
    // time, capacity is given back before it is taken.
    let mut events: Vec<(Time, u8, usize)> = Vec::new();
    for (at, w) in cluster.workloads.iter().enumerate() {
        let r = ran.get(at).copied().flatten();
        if r.is_none() && refused.get(at).copied().unwrap_or(false) {
            // Refused at admission: it never waits for capacity.
            continue;
        }
        // A run started at or past `end` holds nothing in the window: it waits until `end`.
        let r = r.filter(|r| r.started < end);
        let started = r.map_or(end, |r| r.started);
        events.push((w.arrives.min(started), 2, at));
        events.push((started, 3, at));
        if let Some(r) = r {
            events.push((r.ended.unwrap_or(end).min(end), 0, at));
            events.push((r.started, 1, at));
        }
    }
    events.sort_unstable();
    let overflow = || SimError::Config("a share sum overflowed");
    let mut waiting = vec![0u64; owners];
    let mut used = vec![Resources::default(); owners];
    let mut held = vec![0u128; owners];
    let mut contended_ns: Time = 0;
    let mut last: Time = 0;
    let mut intervals: Vec<Time> = Vec::new();
    let mut open: Option<Time> = None;
    for (t, what, at) in events {
        let t = t.min(end);
        if !waiting.iter().all(|&w| w > 0)
            && let Some(o) = open.take()
            && o > 0
        {
            intervals.push(o);
        }
        if waiting.iter().all(|&w| w > 0) {
            let dt = t.checked_sub(last).ok_or(SimError::TimeOverflow)?;
            contended_ns = contended_ns.checked_add(dt).ok_or(SimError::TimeOverflow)?;
            if let Some(o) = open.as_mut() {
                *o = o.checked_add(dt).ok_or(SimError::TimeOverflow)?;
            } else {
                open = Some(dt);
            }
            for (h, u) in held.iter_mut().zip(&used) {
                let add = dominant(u, &total)
                    .checked_mul(u128::from(dt))
                    .ok_or_else(overflow)?;
                *h = h.checked_add(add).ok_or_else(overflow)?;
            }
        }
        last = t;
        let w = cluster
            .workloads
            .get(at)
            .ok_or(SimError::Config("workload index"))?;
        let o = usize::try_from(w.tenancy.owner.0).map_err(|_| SimError::Config("owner"))?;
        let unknown = SimError::Config("a workload names an owner the cluster doesn't");
        match what {
            0 => {
                let u = used.get_mut(o).ok_or(unknown)?;
                *u = u.checked_sub(&w.need).ok_or(SimError::Config("usage"))?;
            }
            1 => {
                let u = used.get_mut(o).ok_or(unknown)?;
                *u = u.checked_add(&w.need).ok_or(SimError::Config("usage"))?;
            }
            2 => {
                let n = waiting.get_mut(o).ok_or(unknown)?;
                *n = n.checked_add(1).ok_or(SimError::Config("waiting"))?;
            }
            _ => {
                let n = waiting.get_mut(o).ok_or(unknown)?;
                *n = n.checked_sub(1).ok_or(SimError::Config("waiting"))?;
            }
        }
    }
    if let Some(o) = open.filter(|&o| o > 0) {
        intervals.push(o);
    }
    let sum = held
        .iter()
        .try_fold(0u128, |a, h| a.checked_add(*h))
        .ok_or_else(overflow)?;
    let ppm = |num: u128, den: u128| -> u64 {
        num.checked_mul(PPM)
            .and_then(|x| x.checked_div(den))
            .and_then(|x| u64::try_from(x).ok())
            .unwrap_or(0)
    };
    let held_time = u128::from(contended_ns).checked_mul(SCALE);
    Ok(ShareReport {
        contended_ns,
        shares_ppm: held.iter().map(|&h| ppm(h, sum)).collect(),
        of_cluster_ppm: held
            .iter()
            .map(|&h| held_time.map_or(0, |d| ppm(h, d)))
            .collect(),
        intervals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeId;

    fn node(cpu: u64, mem: u64) -> NodeSpec {
        NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(0),
            capacity: Resources {
                cpu_milli: cpu,
                mem_bytes: mem,
                gpus: 0,
                disk_bytes: 0,
            },
            background_milli: 0,
            classes: crate::model::Classes::ALL,
            platform: Default::default(),
            virt: Default::default(),
            devices: Vec::new(),
        }
    }

    /// AC-T7.5.2's measure: a workload refused at admission never runs and is not waiting for
    /// capacity. Do two owners on one node, owner 0's workload waiting 10 ns before it runs and owner
    /// 1's refused; expect no contention, as owner 1 never had work waiting.
    #[test]
    fn a_refused_workload_is_not_waiting() {
        let work = |id, owner| WorkloadSpec {
            id: WorkloadId(id),
            need: Resources {
                cpu_milli: 1,
                mem_bytes: 1,
                gpus: 0,
                disk_bytes: 0,
            },
            kind: crate::model::Kind::Plain,
            class: crate::model::Class::Oci,
            arrives: 0,
            runs_for: Some(10),
            tenancy: crate::model::Tenancy {
                owner: OwnerId(owner),
                ..crate::model::Tenancy::default()
            },
            ask: Default::default(),
        };
        let cluster = Cluster {
            nodes: vec![node(10, 10)],
            workloads: vec![work(0, 0), work(1, 1)],
            owners: vec![crate::model::Owner::default(); 2],
            groups: Vec::new(),
            overheads: crate::model::Overheads::default(),
            models: Vec::new(),
        };
        let ran = [
            Some(Ran {
                node: NodeId(0),
                started: 10,
                ended: Some(20),
            }),
            None,
        ];
        let shares = measure(&cluster, &ran, &[false, true], 30).unwrap();
        assert_eq!(shares.contended_ns, 0);
        // Not refused, owner 1's workload waits the whole run: the two contend for 10 ns.
        let shares = measure(&cluster, &ran, &[false, false], 30).unwrap();
        assert_eq!(shares.contended_ns, 10);
        // Measured up to 5 (a recorded day's end before the run's): owner 0's workload, started at
        // 10, holds nothing in the window and waits all of it, as owner 1's does.
        let shares = measure(&cluster, &ran, &[false, false], 5).unwrap();
        assert_eq!(shares.contended_ns, 5);
        assert_eq!(shares.of_cluster_ppm, vec![0, 0]);
    }

    /// AC-T7.5.2 (DRF's order, NSDI 2011 §4): the owner with the lower dominant share over its
    /// weight goes first; a band above goes before any share.
    #[test]
    fn the_lowest_weighted_dominant_share_goes_first() {
        let mut s = Shares::new(&[node(10_000, 100)], vec![1, 1, 2]).unwrap();
        // Owner 0 holds 30% of CPU; owner 1 40% of memory; owner 2 60% of CPU at weight 2 (30%).
        s.take(
            OwnerId(0),
            &Resources {
                cpu_milli: 3_000,
                mem_bytes: 1,
                gpus: 0,
                disk_bytes: 0,
            },
        )
        .unwrap();
        s.take(
            OwnerId(1),
            &Resources {
                cpu_milli: 1_000,
                mem_bytes: 40,
                gpus: 0,
                disk_bytes: 0,
            },
        )
        .unwrap();
        s.take(
            OwnerId(2),
            &Resources {
                cpu_milli: 6_000,
                mem_bytes: 1,
                gpus: 0,
                disk_bytes: 0,
            },
        )
        .unwrap();
        let q = |band, owner| Queue {
            band: Reverse(band),
            owner: OwnerId(owner),
        };
        let order = s.order(
            Policy::Drf,
            [q(0, 1), q(0, 2), q(0, 0), q(1, 1)].into_iter(),
        );
        assert_eq!(order, vec![q(1, 1), q(0, 0), q(0, 2), q(0, 1)]);
        s.give(
            OwnerId(1),
            &Resources {
                cpu_milli: 1_000,
                mem_bytes: 40,
                gpus: 0,
                disk_bytes: 0,
            },
        )
        .unwrap();
        assert_eq!(s.key(OwnerId(1)), 0);
        assert!(
            s.give(
                OwnerId(1),
                &Resources {
                    cpu_milli: 1,
                    mem_bytes: 0,
                    gpus: 0,
                    disk_bytes: 0,
                }
            )
            .is_err()
        );
    }
}
