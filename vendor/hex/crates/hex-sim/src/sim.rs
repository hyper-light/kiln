//! The simulated cluster: one event loop over virtual time, every participant a state machine.
//!
//! - **Clients** submit each workload when it arrives and resubmit it, under the same id, until the
//!   control plane acknowledges it or refuses it; a resubmit of a stored workload is a duplicate.
//! - **The control plane** is `shards` shards (H60), each holding a slice of the nodes (contiguous
//!   ids) and the workloads routed to it ([`Layout`]); one shard holds the whole cluster. A slice's
//!   plane admits workloads (refusing one no empty node could hold), decides
//!   placements on `workers` parallel deciders against its current view, and commits them through one
//!   committer in queue order. A decision is late-bound (S2): at commit it is checked against every
//!   node change since it was made and, if one could change the answer, made again against the current
//!   view (a conflict, counted). So the committed log is one the serial specification could make
//!   ([`crate::spec::check_log`]), while deciding runs in parallel.
//! - **Node agents** start a workload when the store's feed shows it placed on them, and report it
//!   done when it ends, resending until the control plane records it.
//! - **The nemesis** crashes the control plane at store writes, before one is applied or after it
//!   is applied and before anyone hears of it (DS[1][5]): at one chosen write ([`Nemesis::At`]), or at
//!   writes its own seed picks ([`Nemesis::Random`], `AC-T7.3.3`'s crash seeds). Everything the
//!   control plane holds in memory is lost; it restarts after `restart_ns` and rebuilds its view from
//!   the store. Node agents and the work they run go on through it (`AC-T7.3.4`), as Borg's tasks do
//!   when the Borgmaster is down (Verma et al., EuroSys 2015, p. 6). A crash takes down the shard
//!   that made the write, with every slice it holds.
//! - **A lost shard** ([`Nemesis::Lose`], H61) never comes back. Its lease on each slice runs out
//!   `lease_ns` after its last renewal; then the next live shard takes the slice over and rebuilds its
//!   plane from the store, at `rebuild_ns_per_record` a record read (`T-T7.11.1`). The slice's
//!   running work goes on through it, as through a crash.
//! - **A lost cell** ([`Nemesis::LoseCell`], H65) loses every shard at once, and no shard is left to
//!   take a slice over; its running work goes on, its pending work waits.
//!
//! Every queue and map is bounded by the run's workloads or by a [`Config`] field, and the run by
//! `max_steps` (CLAUDE.md §2). Nothing in it calls itself (§11): each event is handled by a function
//! that schedules further events rather than calling their handlers.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::ops::Bound;

use crate::error::SimError;
use crate::fair::{self, Passed, Placed, Policy, Preemption, Queue, Shares};
use crate::index::{Fit, FitIndex, Key, Pick, Refusals};
use crate::model::{
    Class, Classes, Cluster, Demand, GangId, Group, GroupId, NodeId, NodeSpec, Owner, Time,
    WorkloadId, WorkloadSpec,
};
use crate::rng::Rng;
use crate::store::{GangOutcome, Outcome, PreemptOutcome, State, Store, Write};
use crate::trace::{Latencies, TraceHash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// The write is never applied.
    Before,
    /// The write is applied; its writer never learns it was.
    After,
}

/// Crash the control plane at its `write`th store write (counted from 0, over every write the
/// control plane attempts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crash {
    pub write: u64,
    pub when: When,
}

/// Where the nemesis crashes the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nemesis {
    None,
    At(Crash),
    /// A crash at write `first`, then one at each later write with probability `per_mille / 1000`,
    /// at most `max_crashes` in all, each before or after its write by a fair draw; every draw from
    /// `seed`'s own generator, so the faults are a function of the seed (DS[1][4]).
    Random {
        seed: u64,
        first: u64,
        per_mille: u64,
        max_crashes: u32,
    },
    /// Shard `shard` is lost for good at virtual time `at`.
    Lose {
        shard: u32,
        at: Time,
    },
    /// Every shard is lost for good at `at`: a cell's control plane, with no shard left to take its
    /// slices over (H65). Its running work keeps running; its takeovers never complete.
    LoseCell {
        at: Time,
    },
}

/// A takeover's rebuild, per record read. Measured with Hex's three classes (H69): unit
/// hex-sched-g6-classbench-20261106T141227Z, 2026-11-06, fedora 4-15 under the timed lock, best of 5:
/// 155 ns a record over rebuilds of 125,000 of 1,000,000 records, release, commit e311314, by
/// `tests/shards.rs`'s `rebuild_cost`; recorded in `bench/sched/takeover_rebuild.md`.
pub const REBUILD_NS_PER_RECORD: Time = 155;

/// The store's write round trip a shard's lease is seeded from: Amphora's durable write p99.9 under
/// kiln's K0 load (32 writers), 22.27 ms, recorded in Amphora's `bench/records/2026-10-23-adacachy-
/// matrix-v6.md:120` (main, 38feed2; one node, best of 3, adacachy at load 3.5-123.6). The only
/// recorded Amphora write latency: no quorum commit is recorded yet (AC-T1.8.2's 5 ms is a target),
/// so T-T7.11.1's takeover holds on this figure, and moves with it.
pub const STORE_WRITE_RTT_NS: Time = 22_270_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Parallel deciders.
    pub workers: u32,
    /// Virtual time one decision takes.
    pub decide_ns: Time,
    /// Virtual time from a commit being sent to the store applying it.
    pub commit_ns: Time,
    /// From the store applying a placement to its node agent seeing it.
    pub feed_ns: Time,
    pub restart_ns: Time,
    pub client_retry_ns: Time,
    pub client_attempts: u32,
    pub report_retry_ns: Time,
    pub report_attempts: u32,
    /// Node changes kept for late binding; a decision older than the window is made again.
    pub change_window: usize,
    pub max_events: usize,
    pub max_steps: u64,
    pub nemesis: Nemesis,
    /// How deciders sample nodes; the serial specification samples the same way.
    pub pick: Pick,
    /// How pending work is ordered; the serial specification orders the same way.
    pub policy: Policy,
    /// Workloads the control plane may preempt over the run (`docs/design/scheduler-fairness.md`
    /// F4): 0 turns preemption off. Past it, nothing more is preempted and the passes are counted;
    /// it bounds the store's log, which keeps a preemption and a placement again per victim.
    pub preemptions: u64,
    /// Shards of the control plane, each first holding one slice of the nodes (H60).
    pub shards: u32,
    /// How long a shard's lease on its slices outlives its last renewal, and how often it renews.
    pub lease_ns: Time,
    pub renew_ns: Time,
    /// Virtual time a takeover's rebuild takes for each record it reads.
    pub rebuild_ns_per_record: Time,
}

impl Default for Config {
    /// Costs in virtual nanoseconds, chosen for test runs, not measured: implementation goal 2 sets
    /// `decide_ns` and `commit_ns` from the measured fast path.
    fn default() -> Self {
        Self {
            workers: 4,
            decide_ns: 50_000,
            commit_ns: 20_000,
            feed_ns: 10_000,
            restart_ns: 5_000_000,
            client_retry_ns: 10_000_000,
            client_attempts: 16,
            report_retry_ns: 10_000_000,
            report_attempts: 16,
            change_window: 4_096,
            max_events: 1 << 22,
            max_steps: 1 << 32,
            nemesis: Nemesis::None,
            pick: Pick::default(),
            policy: Policy::default(),
            preemptions: 0,
            shards: 1,
            // The learned-wait rule, seeded from the recorded store write round trip (H61): renew
            // once a round trip, and let the lease run ten, etcd's rule for an election timeout
            // (tuning.md:20, docs/v3.5: "at least 10 times the round-trip time").
            lease_ns: STORE_WRITE_RTT_NS.saturating_mul(10),
            renew_ns: STORE_WRITE_RTT_NS,
            rebuild_ns_per_record: REBUILD_NS_PER_RECORD,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Store writes the control plane attempted, including one the nemesis cut off.
    pub writes: u64,
    pub crashes: u64,
    /// Of those, crashes before their write was applied, and after.
    pub crashes_before: u64,
    pub crashes_after: u64,
    /// Decisions made again at commit because a node change since could have changed them.
    pub conflicts: u64,
    /// Of those, decisions older than the change window.
    pub stale: u64,
    pub placements: u64,
    /// Gangs placed, each in one write.
    pub gangs: u64,
    /// Workloads preempted, and the writes that did it.
    pub preempted: u64,
    pub preemptions: u64,
    /// Candidates passed over because their group was at its disruption budget.
    pub budget_passes: u64,
    /// Preemptions not made because the run's budget of them was spent.
    pub preemptions_refused: u64,
    /// Completion reports dropped because the store had preempted the run they report.
    pub reports_superseded: u64,
    /// Fit decisions made, by deciders and again at commit.
    pub decisions: u64,
    /// Over those decisions' scans, nodes with room that a filter refused, by filter.
    pub refused: Refusals,
    /// Workloads found holding on no node, each marking its equivalence class so until capacity
    /// comes back (at most one per class per such stretch).
    pub class_checks: u64,
    /// Workloads passed over because their class was marked so, undecided.
    pub class_skips: u64,
    /// Summed over crashes, the workloads running on nodes when the control plane went down.
    pub running_through_crashes: u64,
    pub steps: u64,
    pub end: Time,
    /// Submit to placement, per placement.
    pub latencies: Latencies,
    pub trace_hash: u64,
    /// Each slice taken over from a lost shard (`T-T7.11.1`).
    pub takeovers: Vec<Takeover>,
}

/// A lost shard's slice, taken over by a live one (H61).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Takeover {
    pub slice: usize,
    /// The shard lost, and the shard that took the slice over.
    pub lost: u32,
    pub by: u32,
    pub lost_at: Time,
    /// From the loss to its lease running out: at most `lease_ns` after its last renewal.
    pub detection_ns: Time,
    /// Records the rebuild read (the slice's workloads not yet done), and the time it took.
    pub records: u64,
    pub rebuild_ns: Time,
    /// Workloads running on the slice's nodes when its shard was lost.
    pub running: u64,
    /// From the loss to the slice's first commit under its new shard; `None` if it never made one.
    pub takeover_ns: Option<Time>,
    /// Store writes the slice applied under its new shard.
    pub committed_after: u64,
}

/// A finished run: its report, the store as it ended, and what the clients and node agents saw.
#[derive(Debug)]
pub struct Run {
    pub report: Report,
    pub store: Store,
    /// Starts per workload, by id.
    pub starts: Vec<u32>,
    /// Whether each workload's client was told its submit was stored.
    pub acked: Vec<bool>,
    /// Whether each workload's client was told it could never be placed.
    pub refused: Vec<bool>,
    /// Where and when each workload's first start ran, by id.
    pub ran: Vec<Option<Ran>>,
    /// The sampling the run's deciders used, for its serial specification.
    pub pick: Pick,
    /// The order the run's committer took pending work in, and the owners it weighed.
    pub policy: Policy,
    pub owners: Vec<Owner>,
    /// Each gang's members, in id order.
    pub gangs: BTreeMap<GangId, Vec<WorkloadId>>,
    /// Whether the run preempted, and its groups' disruption budgets.
    pub preemption: bool,
    pub groups: Vec<Group>,
    /// What the control plane held in memory as the run ended, one per slice.
    pub planes: Vec<ControlPlane>,
}

/// A slice's control plane as the run left it: what it holds in memory, kept so its cost can be
/// measured (`AC-T7.11.4`).
#[derive(Debug)]
pub struct ControlPlane(Plane);

/// One run of a workload on a node, as its node agent saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ran {
    pub node: NodeId,
    pub started: Time,
    /// When the node agent saw it end; `None` while it runs.
    pub ended: Option<Time>,
}

/// A decision: the best-fit key, or none, against the view after change `snap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decision {
    fit: Fit,
    /// The class it was made for: a change to a node that runs it can change what a pick would
    /// choose now.
    class: Class,
    snap: u64,
}

impl Decision {
    fn node(&self) -> Option<Key> {
        self.fit.best
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Arrive {
        at: usize,
        attempt: u32,
    },
    ClientCheck {
        at: usize,
        attempt: u32,
    },
    Proposal {
        slice: usize,
        epoch: u64,
        seq: u64,
        decision: Decision,
    },
    CommitDue {
        slice: usize,
        epoch: u64,
        seq: u64,
        decision: Decision,
        resets: u64,
    },
    /// A gang's commit, decided whole when it comes due.
    GangDue {
        slice: usize,
        epoch: u64,
        seq: u64,
        resets: u64,
    },
    /// A preemption's commit, chosen again when it comes due.
    PreemptDue {
        slice: usize,
        epoch: u64,
        seq: u64,
        resets: u64,
    },
    NodeStart {
        at: usize,
        node: NodeId,
    },
    /// The node agent told to stop a preempted workload's run.
    NodeStop {
        at: usize,
        node: NodeId,
    },
    /// Run `run` (its start count when it started) ended, or its report is sent again.
    NodeDone {
        at: usize,
        node: NodeId,
        attempt: u32,
        run: u32,
    },
    Restart {
        shard: u32,
    },
    Lose {
        shard: u32,
    },
    Takeover {
        slice: usize,
    },
    Rebuilt {
        slice: usize,
        epoch: u64,
    },
}

impl Event {
    /// The words the trace hash takes for this event.
    fn words(&self) -> [u64; 3] {
        let at = |a: usize| u64::try_from(a).unwrap_or(u64::MAX);
        match *self {
            Event::Arrive { at: a, attempt } => [1, at(a), u64::from(attempt)],
            Event::ClientCheck { at: a, attempt } => [2, at(a), u64::from(attempt)],
            Event::Proposal { seq, decision, .. } => [
                3,
                seq,
                decision.node().map_or(u64::MAX, |k| u64::from(k.node.0)),
            ],
            Event::CommitDue { seq, decision, .. } => [
                4,
                seq,
                decision.node().map_or(u64::MAX, |k| u64::from(k.node.0)),
            ],
            Event::NodeStart { at: a, node } => [5, at(a), u64::from(node.0)],
            Event::NodeDone { at: a, node, .. } => [6, at(a), u64::from(node.0)],
            Event::Restart { shard } => [7, u64::from(shard), 0],
            Event::GangDue { seq, .. } => [8, seq, 0],
            Event::PreemptDue { seq, .. } => [9, seq, 0],
            Event::NodeStop { at: a, node } => [10, at(a), u64::from(node.0)],
            Event::Lose { shard } => [11, u64::from(shard), 0],
            Event::Takeover { slice } => [12, at(slice), 0],
            Event::Rebuilt { slice, epoch } => [13, at(slice), epoch],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduled {
    at: Time,
    order: u64,
    event: Event,
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.at, self.order).cmp(&(other.at, other.order))
    }
}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// How the cluster's nodes are split among the control plane's slices (H60): contiguous runs of node
/// ids, one a shard, and the rule that routes a workload to one. Nothing in it changes during a run,
/// so every shard, and the specification, computes the same slice for a workload.
#[derive(Debug)]
pub struct Layout {
    parts: Vec<Part>,
}

#[derive(Debug)]
struct Part {
    /// The slice's first node id in the cluster: a node's id in the slice plus `lo` is its id there.
    lo: u32,
    /// The slice's nodes, their ids dense from 0 within it.
    nodes: Vec<NodeSpec>,
    /// Its nodes empty, for admission.
    empty: FitIndex,
    /// The classes any of its nodes runs: what it declares it can place.
    classes: Classes,
}

impl Layout {
    /// Preserve initialized routes and retained index reservations in a fallible snapshot.
    pub fn try_clone(&self) -> Result<Self, SimError> {
        let mut parts = Vec::new();
        parts
            .try_reserve_exact(self.parts.len())
            .map_err(|_| SimError::Full {
                what: "layout snapshot slices",
                bound: self.parts.len(),
            })?;
        for part in &self.parts {
            let mut nodes = Vec::new();
            nodes
                .try_reserve_exact(part.nodes.len())
                .map_err(|_| SimError::Full {
                    what: "layout snapshot nodes",
                    bound: part.nodes.len(),
                })?;
            for node in &part.nodes {
                nodes.push(node.try_clone()?);
            }
            parts.push(Part {
                lo: part.lo,
                nodes,
                empty: part.empty.try_clone()?,
                classes: part.classes,
            });
        }
        Ok(Self { parts })
    }

    /// `shards` slices of `nodes`, slice `s` the ids from `s * n / shards` up to `(s + 1) * n / shards`.
    pub fn new(nodes: &[NodeSpec], shards: u32) -> Result<Self, SimError> {
        if shards == 0 {
            return Err(SimError::Config("the control plane has at least one shard"));
        }
        let n = u64::try_from(nodes.len()).map_err(|_| SimError::Config("too many nodes"))?;
        if shards > 1 && u64::from(shards) > n {
            return Err(SimError::Config(
                "every shard's slice has at least one node",
            ));
        }
        let bound = |s: u32| -> Result<usize, SimError> {
            n.checked_mul(u64::from(s))
                .and_then(|x| x.checked_div(u64::from(shards)))
                .and_then(|x| usize::try_from(x).ok())
                .ok_or(SimError::Config("too many nodes"))
        };
        let mut parts = Vec::with_capacity(usize::try_from(shards).unwrap_or(0));
        for s in 0..shards {
            let (from, to) = (
                bound(s)?,
                bound(s.checked_add(1).ok_or(SimError::Config("shards"))?)?,
            );
            let lo = u32::try_from(from).map_err(|_| SimError::Config("too many nodes"))?;
            let mut local = nodes
                .get(from..to)
                .ok_or(SimError::Config("node index"))?
                .to_vec();
            for (at, node) in local.iter_mut().enumerate() {
                node.id =
                    NodeId(u32::try_from(at).map_err(|_| SimError::Config("too many nodes"))?);
            }
            let classes = [Class::Oci, Class::Firecracker, Class::Shards]
                .into_iter()
                .filter(|&c| local.iter().any(|n| n.classes.has(c)))
                .fold(Classes::NONE, Classes::with);
            parts.push(Part {
                lo,
                empty: FitIndex::new(&local)?,
                nodes: local,
                classes,
            });
        }
        Ok(Self { parts })
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Slice `s`'s nodes, by their ids within it.
    pub fn nodes(&self, s: usize) -> Option<&[NodeSpec]> {
        self.parts.get(s).map(|p| p.nodes.as_slice())
    }

    /// The slice a workload goes to: the first, from its id modulo the slices on, that declares its
    /// class and whose empty nodes admit it; `None` when no slice's would, and it is refused.
    pub fn route(&self, spec: &WorkloadSpec) -> Option<usize> {
        let n = u64::try_from(self.parts.len()).ok()?;
        let first = spec.id.0.checked_rem(n)?;
        (0..n)
            .filter_map(|k| first.checked_add(k)?.checked_rem(n))
            .filter_map(|s| usize::try_from(s).ok())
            .find(|&s| {
                self.parts.get(s).is_some_and(|p| {
                    p.classes.has(spec.class) && p.empty.admitted_anywhere(&spec.demand())
                })
            })
    }

    /// The classes slice `s` declares: those any of its nodes runs.
    pub fn classes(&self, s: usize) -> Option<Classes> {
        self.parts.get(s).map(|p| p.classes)
    }

    /// The slice a workload goes to when it is refused: the one its client reaches first.
    fn first(&self, spec: &WorkloadSpec) -> Option<usize> {
        let n = u64::try_from(self.parts.len()).ok()?;
        usize::try_from(spec.id.0.checked_rem(n)?).ok()
    }

    /// The slice holding a cluster node.
    pub fn slice_of(&self, node: NodeId) -> Option<usize> {
        self.parts
            .partition_point(|p| p.lo <= node.0)
            .checked_sub(1)
            .filter(|&s| self.local(s, node).is_some())
    }

    /// A cluster node's id within slice `s`, if it is one of the slice's.
    pub fn local(&self, s: usize, node: NodeId) -> Option<NodeId> {
        let p = self.parts.get(s)?;
        let at = node.0.checked_sub(p.lo)?;
        if usize::try_from(at).ok()? < p.nodes.len() {
            Some(NodeId(at))
        } else {
            None
        }
    }

    /// Slice `s`'s node `local`, by its id in the cluster.
    fn global(&self, s: usize, local: NodeId) -> Result<NodeId, SimError> {
        self.parts
            .get(s)
            .and_then(|p| p.lo.checked_add(local.0))
            .map(NodeId)
            .ok_or(SimError::UnknownNode(local))
    }

    fn part(&self, s: usize) -> Result<&Part, SimError> {
        self.parts.get(s).ok_or(SimError::Config("slice index"))
    }
}

/// What a slice's control plane holds in memory, lost when its shard crashes or is lost. Its node
/// ids are the slice's own ([`Layout`]).
#[derive(Debug)]
struct Plane {
    up: bool,
    epoch: u64,
    view: FitIndex,
    policy: Policy,
    /// Pending workloads by queue, then by submit sequence; no queue is kept empty.
    pending: BTreeMap<Queue, BTreeMap<u64, WorkloadId>>,
    /// The queue each pending workload waits in, by its submit sequence.
    queue_of: BTreeMap<u64, Queue>,
    /// What each owner holds placed, for the order between queues.
    shares: Shares,
    /// The record version the plane last wrote or read, for each workload not yet done.
    versions: BTreeMap<WorkloadId, u64>,
    undecided: BTreeMap<Queue, BTreeSet<u64>>,
    deciding: BTreeSet<u64>,
    proposals: BTreeMap<u64, Decision>,
    idle_workers: u32,
    /// In each queue, every pending workload at or before this sequence holds on no node now.
    cursors: BTreeMap<Queue, u64>,
    /// In each queue, the first gang passed over since capacity last came back, by its first
    /// member's sequence: the cursor stays before it ([`Plane::pass`]). A gang's members are placed
    /// one by one on the pick's best, which isn't monotone in the free capacity, so a placement
    /// elsewhere can make a gang that held nowhere hold; a single workload can't.
    gang_floors: BTreeMap<Queue, u64>,
    /// Changes since which a commit already sent must be checked again: capacity given back (which
    /// voids `cursors`' claims), or a workload submitted ahead of the one in flight.
    resets: u64,
    /// The workload whose commit is in flight, and its queue.
    in_flight: Option<(Queue, u64)>,
    /// Node changes `(number, new key)`, the last `change_window` of them.
    changes: VecDeque<(u64, Key)>,
    change_no: u64,
    /// Changes up to this number have left the window.
    forgotten: u64,
    committing: bool,
    /// Equivalence classes (needs) no node holds, since capacity last came back.
    infeasible: BTreeSet<Demand>,
    /// What is placed, for preemption.
    placed: BTreeMap<WorkloadId, Placed>,
    /// Each group's members preempted and not placed again, and those workloads.
    disrupted: BTreeMap<GroupId, u32>,
    preempted: BTreeSet<WorkloadId>,
}

impl Plane {
    /// Slice `s`'s plane from the store, and the records it read: the slice's workloads not yet done
    /// (pending ones routed to it, placed ones on its nodes). Its owners' shares are of the slice's
    /// nodes, as its committer orders only the slice's queues (H60, F1).
    fn rebuild(
        store: &Store,
        cluster: &Cluster,
        layout: &Layout,
        s: usize,
        epoch: u64,
        config: &Config,
    ) -> Result<(Self, u64), SimError> {
        let nodes = &layout.part(s)?.nodes;
        let mut plane = Self {
            up: true,
            epoch,
            view: FitIndex::new(nodes)?,
            policy: config.policy,
            pending: BTreeMap::new(),
            queue_of: BTreeMap::new(),
            shares: Shares::new(nodes, cluster.owners.iter().map(|o| o.weight).collect())?,
            versions: BTreeMap::new(),
            undecided: BTreeMap::new(),
            deciding: BTreeSet::new(),
            proposals: BTreeMap::new(),
            idle_workers: config.workers,
            cursors: BTreeMap::new(),
            gang_floors: BTreeMap::new(),
            resets: 0,
            in_flight: None,
            changes: VecDeque::new(),
            change_no: 0,
            forgotten: 0,
            committing: false,
            infeasible: BTreeSet::new(),
            placed: BTreeMap::new(),
            disrupted: BTreeMap::new(),
            preempted: BTreeSet::new(),
        };
        let mut records = 0u64;
        for record in store.records() {
            match record.state {
                State::Pending => {
                    if layout.route(&record.spec) != Some(s) {
                        continue;
                    }
                    plane.enqueue(&record.spec, record.seq);
                    plane.versions.insert(record.spec.id, record.version);
                    if record.preempted {
                        plane.disrupt(&record.spec)?;
                    }
                }
                State::Placed(node) => {
                    let Some(local) = layout.local(s, node) else {
                        continue;
                    };
                    plane
                        .view
                        .take(local, record.spec.id, &record.spec.demand())?;
                    plane
                        .shares
                        .take(record.spec.tenancy.owner, &record.spec.need)?;
                    plane.versions.insert(record.spec.id, record.version);
                    plane.placed.insert(
                        record.spec.id,
                        placed(&record.spec, local, record.placed_write),
                    );
                }
                State::Complete(_) => continue,
            }
            records = records
                .checked_add(1)
                .ok_or(SimError::Config("too many records"))?;
        }
        Ok((plane, records))
    }

    fn enqueue(&mut self, spec: &WorkloadSpec, seq: u64) {
        let queue = self.policy.queue(spec);
        self.pending.entry(queue).or_default().insert(seq, spec.id);
        self.queue_of.insert(seq, queue);
        self.undecided.entry(queue).or_default().insert(seq);
    }

    /// Takes `seq` out of the pending queues.
    fn dequeue(&mut self, seq: u64) {
        let Some(queue) = self.queue_of.remove(&seq) else {
            return;
        };
        if let Some(q) = self.pending.get_mut(&queue) {
            q.remove(&seq);
            if q.is_empty() {
                self.pending.remove(&queue);
            }
        }
        if let Some(u) = self.undecided.get_mut(&queue) {
            u.remove(&seq);
            if u.is_empty() {
                self.undecided.remove(&queue);
            }
        }
    }

    fn pending_id(&self, seq: u64) -> Option<WorkloadId> {
        let queue = self.queue_of.get(&seq)?;
        self.pending.get(queue)?.get(&seq).copied()
    }

    /// Moves `queue`'s cursor to `seq`, or to just before the first gang passed over in it.
    fn pass(&mut self, queue: Queue, seq: u64) {
        match self.gang_floors.get(&queue) {
            Some(&floor) if floor <= seq => match floor.checked_sub(1) {
                Some(before) => {
                    self.cursors.insert(queue, before);
                }
                None => {
                    self.cursors.remove(&queue);
                }
            },
            _ => {
                self.cursors.insert(queue, seq);
            }
        }
    }

    /// The gang whose first member is `seq` held nowhere now: keep `queue`'s cursor before it.
    fn pass_gang(&mut self, queue: Queue, seq: u64) {
        let floor = self.gang_floors.entry(queue).or_insert(seq);
        *floor = (*floor).min(seq);
        self.pass(queue, seq);
    }

    fn mark_undecided(&mut self, seq: u64) {
        if let Some(&queue) = self.queue_of.get(&seq) {
            self.undecided.entry(queue).or_default().insert(seq);
        }
    }

    /// The queues with pending work, in policy order.
    fn order(&self) -> Vec<Queue> {
        self.shares.order(self.policy, self.pending.keys().copied())
    }

    /// `spec` preempted: back to pending, and one of its group's disruptions.
    fn disrupt(&mut self, spec: &WorkloadSpec) -> Result<(), SimError> {
        self.preempted.insert(spec.id);
        if let Some(g) = spec.tenancy.group {
            let n = self.disrupted.entry(g).or_default();
            *n = n.checked_add(1).ok_or(SimError::Config("disruptions"))?;
        }
        Ok(())
    }

    /// `spec` placed on `node` by write `write`: what its own placements change beyond the view
    /// and shares. AC-T7.5.3: with preemption on, every queue's cursor goes, including its own.
    /// A larger own share can reorder borrowing-first victims so an earlier workload can preempt
    /// within its disruption budgets now. A placement ending a disruption can reopen work too.
    /// Return whether the caller must leave its placing queue open instead of passing it again.
    fn note_placed(
        &mut self,
        spec: &WorkloadSpec,
        node: NodeId,
        write: u64,
        preemption: bool,
    ) -> bool {
        self.placed.insert(spec.id, placed(spec, node, write));
        // A disrupted member placed again ends its disruption (the store's rule).
        if self.preempted.remove(&spec.id)
            && let Some(g) = spec.tenancy.group
            && let Some(n) = self.disrupted.get_mut(&g)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.disrupted.remove(&g);
            }
        }
        if preemption {
            self.cursors.clear();
        }
        preemption
    }
}

/// A placed workload as preemption weighs it.
fn placed(spec: &WorkloadSpec, node: NodeId, write: u64) -> Placed {
    Placed {
        id: spec.id,
        node,
        demand: spec.demand(),
        owner: spec.tenancy.owner,
        band: spec.tenancy.band,
        group: spec.tenancy.group,
        in_gang: spec.tenancy.gang.is_some(),
        write,
    }
}

impl ControlPlane {
    /// Slice `s`'s plane rebuilt from `store`, as a takeover rebuilds it, and the records it read:
    /// what `Config::rebuild_ns_per_record` is measured on.
    pub fn rebuild(
        store: &Store,
        cluster: &Cluster,
        layout: &Layout,
        s: usize,
        config: &Config,
    ) -> Result<(Self, u64), SimError> {
        let (plane, records) = Plane::rebuild(store, cluster, layout, s, 0, config)?;
        Ok((Self(plane), records))
    }

    /// Workloads its queues hold, pending.
    pub fn pending(&self) -> usize {
        self.0.queue_of.len()
    }

    /// Workloads it holds a record version for: its slice's workloads not yet done.
    pub fn workloads(&self) -> usize {
        self.0.versions.len()
    }
}

/// A slice: its shard, its plane, and what runs on its nodes.
#[derive(Debug)]
struct Slice {
    owner: u32,
    plane: Plane,
    /// Workloads running on its nodes now.
    running: u64,
    /// Its entry in [`Report::takeovers`] once it has been taken over.
    takeover: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Shard {
    up: bool,
    /// Lost for good: it never restarts, and its slices go to another shard.
    lost: bool,
}

fn slot(slices: &mut [Slice], s: usize) -> Result<&mut Slice, SimError> {
    slices.get_mut(s).ok_or(SimError::Config("slice index"))
}

struct Sim<'a> {
    cluster: &'a Cluster,
    config: Config,
    now: Time,
    queue: BinaryHeap<Reverse<Scheduled>>,
    order: u64,
    store: Store,
    /// Each gang's members, by id; and whether the empty cluster holds the gang whole (admission).
    gangs: BTreeMap<GangId, (Vec<WorkloadId>, bool)>,
    layout: Layout,
    slices: Vec<Slice>,
    shards: Vec<Shard>,
    starts: Vec<u32>,
    acked: Vec<bool>,
    refused: Vec<bool>,
    ran: Vec<Option<Ran>>,
    /// Whether each workload's current run is going on its node.
    live: Vec<bool>,
    /// Each workload's runs up to this one were stopped by preemption: their reports are dropped.
    stopped: Vec<u32>,
    nemesis_rng: Rng,
    latencies: Vec<Time>,
    hash: TraceHash,
    report: Report,
}

pub fn run(cluster: &Cluster, config: &Config) -> Result<Run, SimError> {
    if config.workers == 0 {
        return Err(SimError::Config(
            "the control plane has at least one decider",
        ));
    }
    if config.change_window == 0 {
        return Err(SimError::Config(
            "the change window holds at least one change",
        ));
    }
    if config.renew_ns == 0 || config.renew_ns > config.lease_ns {
        return Err(SimError::Config(
            "a shard renews its lease, within the lease's length",
        ));
    }
    if let Nemesis::Lose { shard, .. } = config.nemesis
        && (config.shards < 2 || shard >= config.shards)
    {
        return Err(SimError::Config(
            "a shard lost is one of two or more, so another can take its slice over",
        ));
    }
    // Gangs and preemption are decided by one plane over the whole cluster (F3, F4): a gang's
    // members are routed by their own ids (H60), so a slice could hold only some of them, and a
    // disruption group's budget spans the cluster while a slice's plane sees its own slice.
    if config.shards > 1 && config.preemptions > 0 {
        return Err(SimError::Config(
            "preemption runs on one shard: a disruption budget spans every slice",
        ));
    }
    if config.shards > 1 && cluster.workloads.iter().any(|w| w.tenancy.gang.is_some()) {
        return Err(SimError::Config(
            "gangs run on one shard: a gang's members may be routed to different slices",
        ));
    }
    for (at, w) in cluster.workloads.iter().enumerate() {
        if usize::try_from(w.id.0).ok() != Some(at) {
            return Err(SimError::Config(
                "workload ids must be dense from 0, in order",
            ));
        }
    }
    if cluster
        .workloads
        .windows(2)
        .any(|p| p.first().map(|a| a.arrives) > p.get(1).map(|b| b.arrives))
    {
        return Err(SimError::Config("workloads must be in arrival order"));
    }
    let n = cluster.workloads.len();
    // The log holds a submit, a placement and a completion per workload, and a preemption and a
    // placement again per workload preempted.
    let max_log = u64::try_from(n)
        .ok()
        .and_then(|n| n.checked_mul(3))
        .and_then(|l| l.checked_add(config.preemptions.checked_mul(2)?))
        .and_then(|l| usize::try_from(l).ok())
        .ok_or(SimError::Config("too many workloads"))?;
    let store = Store::new(&cluster.nodes, n, max_log)?;
    for w in &cluster.workloads {
        if !usize::try_from(w.tenancy.owner.0).is_ok_and(|o| o < cluster.owners.len()) {
            return Err(SimError::Config(
                "a workload names an owner the cluster doesn't",
            ));
        }
        if let Some(g) = w.tenancy.group
            && !usize::try_from(g.0).is_ok_and(|g| g < cluster.groups.len())
        {
            return Err(SimError::Config(
                "a workload names a group the cluster doesn't",
            ));
        }
    }
    let gangs = gangs_of(cluster, &config.pick)?;
    let layout = Layout::new(&cluster.nodes, config.shards)?;
    let mut slices = Vec::with_capacity(layout.len());
    for s in 0..layout.len() {
        let (plane, _) = Plane::rebuild(&store, cluster, &layout, s, 0, config)?;
        slices.push(Slice {
            owner: u32::try_from(s).map_err(|_| SimError::Config("shards"))?,
            plane,
            running: 0,
            takeover: None,
        });
    }
    let mut sim = Sim {
        cluster,
        config: *config,
        now: 0,
        queue: BinaryHeap::new(),
        order: 0,
        store,
        gangs,
        shards: vec![
            Shard {
                up: true,
                lost: false
            };
            layout.len()
        ],
        layout,
        slices,
        starts: vec![0; n],
        acked: vec![false; n],
        refused: vec![false; n],
        ran: vec![None; n],
        live: vec![false; n],
        stopped: vec![0; n],
        nemesis_rng: Rng::new(match config.nemesis {
            Nemesis::Random { seed, .. } => seed,
            Nemesis::None | Nemesis::At(_) | Nemesis::Lose { .. } | Nemesis::LoseCell { .. } => 0,
        }),
        latencies: Vec::with_capacity(n),
        hash: TraceHash::default(),
        report: Report::default(),
    };
    if let Some(first) = cluster.workloads.first() {
        sim.schedule(first.arrives, Event::Arrive { at: 0, attempt: 0 })?;
    }
    if let Nemesis::Lose { shard, at } = config.nemesis {
        sim.schedule(at, Event::Lose { shard })?;
    }
    if let Nemesis::LoseCell { at } = config.nemesis {
        for shard in 0..config.shards {
            sim.schedule(at, Event::Lose { shard })?;
        }
    }
    while let Some(Reverse(next)) = sim.queue.pop() {
        sim.report.steps = sim
            .report
            .steps
            .checked_add(1)
            .ok_or(SimError::StepBudget(sim.config.max_steps))?;
        if sim.report.steps > sim.config.max_steps {
            return Err(SimError::StepBudget(sim.config.max_steps));
        }
        sim.now = next.at;
        sim.hash.word(next.at);
        for word in next.event.words() {
            sim.hash.word(word);
        }
        sim.handle(next.event)?;
    }
    for entry in sim.store.log() {
        sim.hash.word(entry.at);
    }
    sim.report.end = sim.now;
    sim.report.trace_hash = sim.hash.finish();
    sim.report.latencies = Latencies::new(sim.latencies);
    Ok(Run {
        report: sim.report,
        store: sim.store,
        starts: sim.starts,
        acked: sim.acked,
        refused: sim.refused,
        ran: sim.ran,
        pick: sim.config.pick,
        policy: sim.config.policy,
        owners: sim.cluster.owners.clone(),
        gangs: sim
            .gangs
            .into_iter()
            .map(|(g, (members, _))| (g, members))
            .collect(),
        preemption: sim.config.preemptions > 0,
        groups: sim.cluster.groups.clone(),
        planes: sim
            .slices
            .into_iter()
            .map(|s| ControlPlane(s.plane))
            .collect(),
    })
}

/// A count past `u64::MAX`, which a run's budget keeps far off.
fn tally(n: &mut u64, by: u64, max_steps: u64) -> Result<(), SimError> {
    *n = n.checked_add(by).ok_or(SimError::StepBudget(max_steps))?;
    Ok(())
}

/// Each gang's members in id order, and whether an empty cluster holds it whole. A gang's members
/// share one owner and band (one queue, so placed at one place in it) and one group.
fn gangs_of(
    cluster: &Cluster,
    pick: &Pick,
) -> Result<BTreeMap<GangId, (Vec<WorkloadId>, bool)>, SimError> {
    let mut members: BTreeMap<GangId, Vec<&WorkloadSpec>> = BTreeMap::new();
    for w in &cluster.workloads {
        if let Some(g) = w.tenancy.gang {
            members.entry(g).or_default().push(w);
        }
    }
    let mut out = BTreeMap::new();
    for (gang, ws) in members {
        let first = ws
            .first()
            .ok_or(SimError::Config("a gang with no members"))?;
        if ws.iter().any(|w| {
            (w.tenancy.owner, w.tenancy.band, w.tenancy.group)
                != (first.tenancy.owner, first.tenancy.band, first.tenancy.group)
        }) {
            return Err(SimError::Config(
                "a gang's members differ in owner, band or group",
            ));
        }
        let mut empty = FitIndex::new(&cluster.nodes)?;
        let demands: Vec<(WorkloadId, Demand)> = ws.iter().map(|w| (w.id, w.demand())).collect();
        let fits = fit_whole(&mut empty, &demands, pick)?.is_some();
        out.insert(gang, (ws.iter().map(|w| w.id).collect(), fits));
    }
    Ok(out)
}

/// Where each workload of `demands`, in order, goes on `view` (each taking its room and devices
/// before the next is sought), or `None` if one fits nowhere; `view` is left as it was.
pub fn fit_whole(
    view: &mut FitIndex,
    demands: &[(WorkloadId, Demand)],
    pick: &Pick,
) -> Result<Option<Vec<NodeId>>, SimError> {
    let mut taken: Vec<(NodeId, WorkloadId, Demand)> = Vec::with_capacity(demands.len());
    let mut whole = true;
    for (w, d) in demands {
        let Some(key) = view.fit(d, pick).best else {
            whole = false;
            break;
        };
        view.take(key.node, *w, d)?;
        taken.push((key.node, *w, *d));
    }
    for (node, w, d) in &taken {
        view.give(*node, *w, d)?;
    }
    Ok(whole.then(|| taken.into_iter().map(|(n, _, _)| n).collect()))
}

/// A gang member as its commit writes it: the workload, its node, the record version read and its
/// submit sequence.
type GangMember = (WorkloadId, NodeId, u64, u64);

/// Where a gang stands for the committer.
enum GangState {
    /// A member isn't submitted yet: what it waits for is its members, not capacity.
    Incomplete,
    /// Every member is pending; this is the first of them in its queue.
    First,
    /// Every member is pending; another comes first and is placed with this one.
    Later,
}

/// Slice `s`'s plane, to read.
fn plane_at(slices: &[Slice], s: usize) -> Result<&Plane, SimError> {
    slices
        .get(s)
        .map(|x| &x.plane)
        .ok_or(SimError::Config("slice index"))
}

impl Sim<'_> {
    fn schedule(&mut self, at: Time, event: Event) -> Result<(), SimError> {
        if self.queue.len() >= self.config.max_events {
            return Err(SimError::Full {
                what: "the event queue",
                bound: self.config.max_events,
            });
        }
        self.order = self
            .order
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        self.queue.push(Reverse(Scheduled {
            at,
            order: self.order,
            event,
        }));
        Ok(())
    }

    fn after(&self, delay: Time) -> Result<Time, SimError> {
        self.now.checked_add(delay).ok_or(SimError::TimeOverflow)
    }

    fn workload(&self, at: usize) -> Result<&WorkloadSpec, SimError> {
        self.cluster
            .workloads
            .get(at)
            .ok_or(SimError::Config("workload index"))
    }

    fn spec(&self, id: WorkloadId) -> Result<WorkloadSpec, SimError> {
        self.store
            .record(id)
            .map(|r| r.spec)
            .ok_or(SimError::UnknownWorkload(id))
    }

    /// A gang's members with their submit sequences, in id order.
    fn members(&self, gang: GangId) -> Result<Vec<(WorkloadId, Option<u64>)>, SimError> {
        let (ids, _) = self
            .gangs
            .get(&gang)
            .ok_or(SimError::Config("a gang the run doesn't know"))?;
        Ok(ids
            .iter()
            .map(|id| (*id, self.store.record(*id).map(|r| r.seq)))
            .collect())
    }

    fn gang_state(&self, s: usize, gang: GangId, seq: u64) -> Result<GangState, SimError> {
        let members = self.members(gang)?;
        let plane = plane_at(&self.slices, s)?;
        let mut first = u64::MAX;
        for (_, m) in members {
            match m {
                Some(m) if plane.queue_of.contains_key(&m) => first = first.min(m),
                _ => return Ok(GangState::Incomplete),
            }
        }
        Ok(if first == seq {
            GangState::First
        } else {
            GangState::Later
        })
    }

    /// The gang's members, their versions and where each goes now on slice `s`'s view (its own node
    /// ids), or `None` if the view doesn't hold it whole.
    fn decide_gang(&mut self, s: usize, gang: GangId) -> Result<Option<Vec<GangMember>>, SimError> {
        let members = self.members(gang)?;
        let mut demands = Vec::with_capacity(members.len());
        for (id, _) in &members {
            demands.push((*id, self.demand(*id)?));
        }
        self.report.decisions = self
            .report
            .decisions
            .checked_add(u64::try_from(members.len()).unwrap_or(u64::MAX))
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        let pick = self.config.pick;
        let plane = &mut slot(&mut self.slices, s)?.plane;
        let Some(nodes) = fit_whole(&mut plane.view, &demands, &pick)? else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(members.len());
        for ((id, seq), node) in members.into_iter().zip(nodes) {
            let version = *plane
                .versions
                .get(&id)
                .ok_or(SimError::UnknownWorkload(id))?;
            out.push((id, node, version, seq.ok_or(SimError::UnknownWorkload(id))?));
        }
        Ok(Some(out))
    }

    fn demand(&self, id: WorkloadId) -> Result<Demand, SimError> {
        self.store
            .record(id)
            .map(|r| r.spec.demand())
            .ok_or(SimError::UnknownWorkload(id))
    }

    fn shard(&mut self, shard: u32) -> Result<&mut Shard, SimError> {
        usize::try_from(shard)
            .ok()
            .and_then(|at| self.shards.get_mut(at))
            .ok_or(SimError::Config("shard index"))
    }

    /// The slices `shard` holds now, in order.
    fn owned(&self, shard: u32) -> Vec<usize> {
        (0..self.slices.len())
            .filter(|&s| self.slices.get(s).is_some_and(|x| x.owner == shard))
            .collect()
    }

    fn handle(&mut self, event: Event) -> Result<(), SimError> {
        match event {
            Event::Arrive { at, attempt } => self.arrive(at, attempt),
            Event::ClientCheck { at, attempt } => self.client_check(at, attempt),
            Event::Proposal {
                slice,
                epoch,
                seq,
                decision,
            } => self.proposal(slice, epoch, seq, decision),
            Event::CommitDue {
                slice,
                epoch,
                seq,
                decision,
                resets,
            } => self.commit_due(slice, epoch, seq, decision, resets),
            Event::GangDue {
                slice,
                epoch,
                seq,
                resets,
            } => self.gang_due(slice, epoch, seq, resets),
            Event::PreemptDue {
                slice,
                epoch,
                seq,
                resets,
            } => self.preempt_due(slice, epoch, seq, resets),
            Event::NodeStart { at, node } => self.node_start(at, node),
            Event::NodeStop { at, node } => self.node_stop(at, node),
            Event::NodeDone {
                at,
                node,
                attempt,
                run,
            } => self.node_done(at, node, attempt, run),
            Event::Restart { shard } => self.restart(shard),
            Event::Lose { shard } => self.lose(shard),
            Event::Takeover { slice } => self.take_over(slice),
            Event::Rebuilt { slice, epoch } => self.rebuilt(slice, epoch),
        }
    }

    fn restart(&mut self, shard: u32) -> Result<(), SimError> {
        let sh = self.shard(shard)?;
        if sh.lost {
            return Ok(());
        }
        sh.up = true;
        let owned = self.owned(shard);
        for &s in &owned {
            let epoch = slot(&mut self.slices, s)?.plane.epoch;
            let (plane, _) = Plane::rebuild(
                &self.store,
                self.cluster,
                &self.layout,
                s,
                epoch,
                &self.config,
            )?;
            slot(&mut self.slices, s)?.plane = plane;
        }
        for &s in &owned {
            self.assign(s)?;
            self.sequence(s)?;
        }
        Ok(())
    }

    /// The nemesis loses `shard` for good (H61). Its slices' leases run out `lease_ns` after the
    /// last renewal before the loss; then a live shard takes each over.
    fn lose(&mut self, shard: u32) -> Result<(), SimError> {
        let sh = self.shard(shard)?;
        if sh.lost {
            return Ok(());
        }
        sh.lost = true;
        sh.up = false;
        let renewed = self
            .now
            .checked_rem(self.config.renew_ns)
            .and_then(|r| self.now.checked_sub(r))
            .ok_or(SimError::TimeOverflow)?;
        let expires = renewed
            .checked_add(self.config.lease_ns)
            .ok_or(SimError::TimeOverflow)?;
        let detection_ns = expires
            .checked_sub(self.now)
            .ok_or(SimError::TimeOverflow)?;
        for s in self.owned(shard) {
            let entry = self.report.takeovers.len();
            let slice = slot(&mut self.slices, s)?;
            slice.plane.up = false;
            slice.plane.epoch = slice
                .plane
                .epoch
                .checked_add(1)
                .ok_or(SimError::StepBudget(self.config.max_steps))?;
            slice.takeover = Some(entry);
            let running = slice.running;
            self.report.takeovers.push(Takeover {
                slice: s,
                lost: shard,
                by: shard,
                lost_at: self.now,
                detection_ns,
                records: 0,
                rebuild_ns: 0,
                running,
                takeover_ns: None,
                committed_after: 0,
            });
            self.schedule(expires, Event::Takeover { slice: s })?;
        }
        Ok(())
    }

    /// A lost shard's lease on slice `s` has run out: the next live shard after it takes the slice
    /// and rebuilds its plane from the store, which takes `rebuild_ns_per_record` a record read. With
    /// every shard lost (a cell's plane, H65) none does, and its takeover stays without a time.
    fn take_over(&mut self, s: usize) -> Result<(), SimError> {
        let lost = slot(&mut self.slices, s)?.owner;
        let shards = u32::try_from(self.shards.len()).map_err(|_| SimError::Config("shards"))?;
        let Some(by) = (1..shards)
            .filter_map(|k| lost.checked_add(k)?.checked_rem(shards))
            .find(|&x| {
                usize::try_from(x)
                    .ok()
                    .and_then(|at| self.shards.get(at))
                    .is_some_and(|sh| !sh.lost)
            })
        else {
            return Ok(());
        };
        let epoch = slot(&mut self.slices, s)?.plane.epoch;
        let (mut plane, records) = Plane::rebuild(
            &self.store,
            self.cluster,
            &self.layout,
            s,
            epoch,
            &self.config,
        )?;
        // It serves once the rebuild has taken its time.
        plane.up = false;
        let rebuild_ns = records
            .checked_mul(self.config.rebuild_ns_per_record)
            .ok_or(SimError::TimeOverflow)?;
        let slice = slot(&mut self.slices, s)?;
        slice.owner = by;
        slice.plane = plane;
        if let Some(t) = slice
            .takeover
            .and_then(|entry| self.report.takeovers.get_mut(entry))
        {
            t.by = by;
            t.records = records;
            t.rebuild_ns = rebuild_ns;
        }
        self.schedule(self.after(rebuild_ns)?, Event::Rebuilt { slice: s, epoch })
    }

    fn rebuilt(&mut self, s: usize, epoch: u64) -> Result<(), SimError> {
        let owner = slot(&mut self.slices, s)?.owner;
        // A crash of the new shard since voids this rebuild; its restart rebuilds the slice.
        if !self.shard(owner)?.up {
            return Ok(());
        }
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if plane.epoch != epoch {
            return Ok(());
        }
        plane.up = true;
        self.assign(s)?;
        self.sequence(s)
    }

    /// Slice `s`'s store write, where the nemesis strikes its shard. `None`: the shard crashed there.
    fn write(&mut self, s: usize, write: Write) -> Result<Option<Outcome>, SimError> {
        let number = self.report.writes;
        self.report.writes = number
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        let owner = slot(&mut self.slices, s)?.owner;
        let crash = self.strike(number);
        if crash == Some(When::Before) {
            self.crash(owner, When::Before)?;
            return Ok(None);
        }
        let outcome = self.store.apply(self.now, write)?;
        if let (Outcome::Applied { .. }, Write::Place { workload, node, .. }) = (outcome, write) {
            // The store's feed tells the node, whatever becomes of the plane.
            let at =
                usize::try_from(workload.0).map_err(|_| SimError::UnknownWorkload(workload))?;
            self.schedule(
                self.after(self.config.feed_ns)?,
                Event::NodeStart { at, node },
            )?;
        }
        if crash.is_some() {
            self.crash(owner, When::After)?;
            return Ok(None);
        }
        if let Outcome::Applied { .. } = outcome {
            self.committed(s)?;
        }
        Ok(Some(outcome))
    }

    /// A write slice `s`'s shard applied: under a shard that took the slice over, counted, its first
    /// setting the takeover's time (`T-T7.11.1`).
    fn committed(&mut self, s: usize) -> Result<(), SimError> {
        if let Some(t) = slot(&mut self.slices, s)?
            .takeover
            .and_then(|entry| self.report.takeovers.get_mut(entry))
        {
            tally(&mut t.committed_after, 1, self.config.max_steps)?;
            if t.takeover_ns.is_none() {
                t.takeover_ns = Some(
                    self.now
                        .checked_sub(t.lost_at)
                        .ok_or(SimError::TimeOverflow)?,
                );
            }
        }
        Ok(())
    }

    /// Whether the nemesis crashes the plane at write `number`, and on which side of it.
    fn strike(&mut self, number: u64) -> Option<When> {
        match self.config.nemesis {
            Nemesis::None | Nemesis::Lose { .. } | Nemesis::LoseCell { .. } => None,
            Nemesis::At(crash) if crash.write == number => Some(crash.when),
            Nemesis::At(_) => None,
            Nemesis::Random {
                first,
                per_mille,
                max_crashes,
                ..
            } => {
                let room = self.report.crashes < u64::from(max_crashes);
                let hit = number == first || (number > first && self.nemesis_rng.chance(per_mille));
                if !(room && hit) {
                    return None;
                }
                Some(if self.nemesis_rng.chance(500) {
                    When::Before
                } else {
                    When::After
                })
            }
        }
    }

    /// `shard` crashes: every slice it holds loses its plane until it restarts.
    fn crash(&mut self, shard: u32, when: When) -> Result<(), SimError> {
        let max = self.config.max_steps;
        let side = match when {
            When::Before => &mut self.report.crashes_before,
            When::After => &mut self.report.crashes_after,
        };
        tally(side, 1, max)?;
        tally(&mut self.report.crashes, 1, max)?;
        self.shard(shard)?.up = false;
        for s in self.owned(shard) {
            let slice = slot(&mut self.slices, s)?;
            tally(&mut self.report.running_through_crashes, slice.running, max)?;
            slice.plane.up = false;
            slice.plane.epoch = slice
                .plane
                .epoch
                .checked_add(1)
                .ok_or(SimError::StepBudget(max))?;
        }
        self.schedule(
            self.after(self.config.restart_ns)?,
            Event::Restart { shard },
        )
    }

    fn arrive(&mut self, at: usize, attempt: u32) -> Result<(), SimError> {
        if attempt == 0 {
            let next = at
                .checked_add(1)
                .ok_or(SimError::Config("workload index"))?;
            if let Some(w) = self.cluster.workloads.get(next) {
                self.schedule(
                    w.arrives.max(self.now),
                    Event::Arrive {
                        at: next,
                        attempt: 0,
                    },
                )?;
            }
        }
        self.schedule(
            self.after(self.config.client_retry_ns)?,
            Event::ClientCheck { at, attempt },
        )?;
        let spec = *self.workload(at)?;
        let route = self.layout.route(&spec);
        let s = route
            .or_else(|| self.layout.first(&spec))
            .ok_or(SimError::Config("slice index"))?;
        if !slot(&mut self.slices, s)?.plane.up {
            return Ok(());
        }
        let gang_fits = spec
            .tenancy
            .gang
            .is_none_or(|g| self.gangs.get(&g).is_some_and(|(_, fits)| *fits));
        if route.is_none() || !gang_fits {
            if let Some(r) = self.refused.get_mut(at) {
                *r = true;
            }
            return Ok(());
        }
        let Some(outcome) = self.write(s, Write::Submit(spec))? else {
            return Ok(());
        };
        match outcome {
            Outcome::Applied {
                op: crate::store::Op::Submit { seq, .. },
                version,
            } => {
                let plane = &mut slot(&mut self.slices, s)?.plane;
                plane.enqueue(&spec, seq);
                plane.versions.insert(spec.id, version);
                if spec.tenancy.gang.is_some() {
                    // A gang this member completes may hold where its queue's cursor has passed.
                    let queue = plane.policy.queue(&spec);
                    plane.cursors.remove(&queue);
                }
                self.note_ahead(s, &spec)?;
            }
            // Stored before a crash; the rebuilt plane already queues it.
            Outcome::Duplicate { .. } => {}
            Outcome::Applied { .. } | Outcome::Conflict(_) => {
                return Err(SimError::UnexpectedConflict(spec.id));
            }
        }
        if let Some(a) = self.acked.get_mut(at) {
            *a = true;
        }
        self.assign(s)?;
        self.sequence(s)
    }

    fn client_check(&mut self, at: usize, attempt: u32) -> Result<(), SimError> {
        if self.acked.get(at).copied().unwrap_or(true)
            || self.refused.get(at).copied().unwrap_or(true)
        {
            return Ok(());
        }
        let next = attempt
            .checked_add(1)
            .ok_or(SimError::Config("client attempts"))?;
        if next >= self.config.client_attempts {
            return Err(SimError::ClientGaveUp(self.workload(at)?.id, next));
        }
        self.schedule(self.now, Event::Arrive { at, attempt: next })
    }

    /// A workload submitted to slice `s` ahead of the one whose commit is in flight there sends that
    /// commit back to the queue when it comes due, as capacity given back does: one in a queue the
    /// policy puts first, or a gang's last member, which makes the gang whole at its first member's
    /// place, so perhaps ahead of the one in flight in its own queue.
    fn note_ahead(&mut self, s: usize, spec: &WorkloadSpec) -> Result<(), SimError> {
        let max = self.config.max_steps;
        let plane = &mut slot(&mut self.slices, s)?.plane;
        let Some((queue, _)) = plane.in_flight else {
            return Ok(());
        };
        let policy = plane.policy;
        let new = policy.queue(spec);
        let ahead = if new == queue {
            spec.tenancy.gang.is_some()
        } else {
            plane.shares.rank(policy, new) < plane.shares.rank(policy, queue)
        };
        if ahead {
            plane.resets = plane
                .resets
                .checked_add(1)
                .ok_or(SimError::StepBudget(max))?;
        }
        Ok(())
    }

    /// Slice `s`'s next undecided workload, its queue first in policy order, lowest sequence first.
    fn next_undecided(&mut self, s: usize) -> Result<Option<u64>, SimError> {
        let plane = &mut slot(&mut self.slices, s)?.plane;
        let order = plane.order();
        for queue in order {
            let Some(u) = plane.undecided.get_mut(&queue) else {
                continue;
            };
            let seq = u.pop_first();
            if u.is_empty() {
                plane.undecided.remove(&queue);
            }
            if seq.is_some() {
                return Ok(seq);
            }
        }
        Ok(None)
    }

    /// Hands slice `s`'s undecided workloads, in policy order, to its idle deciders.
    fn assign(&mut self, s: usize) -> Result<(), SimError> {
        loop {
            let plane = &slot(&mut self.slices, s)?.plane;
            if !plane.up || plane.idle_workers == 0 {
                return Ok(());
            }
            let Some(seq) = self.next_undecided(s)? else {
                return Ok(());
            };
            let Some(id) = slot(&mut self.slices, s)?.plane.pending_id(seq) else {
                continue;
            };
            if self.spec(id)?.tenancy.gang.is_some() {
                // A gang is decided whole by the committer when it comes due.
                continue;
            }
            let need = self.demand(id)?;
            if slot(&mut self.slices, s)?.plane.infeasible.contains(&need) {
                // Its class holds nowhere; the committer passes over it until capacity comes back.
                continue;
            }
            let decision = self.decide(s, &need)?;
            let plane = &mut slot(&mut self.slices, s)?.plane;
            plane.deciding.insert(seq);
            plane.idle_workers = plane
                .idle_workers
                .checked_sub(1)
                .ok_or(SimError::Config("idle deciders"))?;
            let epoch = plane.epoch;
            self.schedule(
                self.after(self.config.decide_ns)?,
                Event::Proposal {
                    slice: s,
                    epoch,
                    seq,
                    decision,
                },
            )?;
        }
    }

    fn proposal(
        &mut self,
        s: usize,
        epoch: u64,
        seq: u64,
        decision: Decision,
    ) -> Result<(), SimError> {
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if epoch != plane.epoch || !plane.up {
            return Ok(());
        }
        plane.idle_workers = plane
            .idle_workers
            .checked_add(1)
            .ok_or(SimError::Config("idle deciders"))?;
        plane.deciding.remove(&seq);
        if plane.queue_of.contains_key(&seq) {
            plane.proposals.insert(seq, decision);
        }
        self.assign(s)?;
        self.sequence(s)
    }

    /// A decision against slice `s`'s view now: the kernel's pick (`FitIndex::fit`).
    fn decide(&mut self, s: usize, need: &Demand) -> Result<Decision, SimError> {
        let max = self.config.max_steps;
        tally(&mut self.report.decisions, 1, max)?;
        let plane = &slot(&mut self.slices, s)?.plane;
        let fit = plane.view.fit(need, &self.config.pick);
        let snap = plane.change_no;
        let r = &mut self.report.refused;
        for (total, now) in [
            (&mut r.gpu, fit.refused.gpu),
            (&mut r.shares, fit.refused.shares),
            (&mut r.hot, fit.refused.hot),
            (&mut r.class, fit.refused.class),
            (&mut r.platform, fit.refused.platform),
            (&mut r.device, fit.refused.device),
        ] {
            tally(total, now, max)?;
        }
        Ok(Decision {
            fit,
            class: need.class,
            snap,
        })
    }

    /// Whether `decision` is still what a pick now would choose: no node that runs its class changed
    /// since. A pick reads every node with the room for its class, so a change to a node that doesn't
    /// run the class leaves every node it read, and its choice, as they were.
    fn still_holds(&mut self, s: usize, decision: &Decision) -> Result<bool, SimError> {
        let plane = &slot(&mut self.slices, s)?.plane;
        if decision.snap < plane.forgotten {
            tally(&mut self.report.stale, 1, self.config.max_steps)?;
            return Ok(false);
        }
        let voided = plane
            .changes
            .iter()
            .rev()
            .take_while(|(no, _)| *no > decision.snap)
            .any(|(_, key)| plane.view.runs(key.node, decision.class));
        Ok(!voided)
    }

    /// `decision` if it still holds, else a fresh one (a conflict).
    fn late_bind(
        &mut self,
        s: usize,
        need: &Demand,
        decision: Decision,
    ) -> Result<Decision, SimError> {
        if self.still_holds(s, &decision)? {
            return Ok(decision);
        }
        tally(&mut self.report.conflicts, 1, self.config.max_steps)?;
        self.decide(s, need)
    }

    /// `need`'s class holds on no node of slice `s`: mark it so until capacity comes back.
    fn mark_infeasible(&mut self, s: usize, need: Demand) -> Result<(), SimError> {
        tally(&mut self.report.class_checks, 1, self.config.max_steps)?;
        slot(&mut self.slices, s)?.plane.infeasible.insert(need);
        Ok(())
    }

    /// Slice `s`'s committer: takes the first queue in policy order from past its cursor, passes
    /// over workloads no node holds, and sends the first placement it reaches; one commit in flight.
    /// When none of that queue's work holds on any node now, no queue after it is placed either:
    /// DRF's allocation stops at the lowest share whose next task doesn't fit (NSDI 2011,
    /// Algorithm 1), so smaller work of larger shares can't keep taking what frees up. Each pass
    /// visits each pending workload of the queue at most once.
    fn sequence(&mut self, s: usize) -> Result<(), SimError> {
        let plane = &slot(&mut self.slices, s)?.plane;
        let Some(q) = plane.order().first().copied() else {
            return Ok(());
        };
        let budget = plane.pending.get(&q).map_or(0, BTreeMap::len);
        // Where the pass is: past the cursor, and past gang members that don't move it.
        let mut from = plane
            .cursors
            .get(&q)
            .map_or(Bound::Unbounded, |&c| Bound::Excluded(c));
        for _ in 0..=budget {
            let plane = &slot(&mut self.slices, s)?.plane;
            if !plane.up || plane.committing {
                return Ok(());
            }
            let Some((&seq, &id)) = plane
                .pending
                .get(&q)
                .and_then(|p| p.range((from, Bound::Unbounded)).next())
            else {
                return Ok(());
            };
            from = Bound::Excluded(seq);
            if let Some(gang) = self.spec(id)?.tenancy.gang {
                match self.gang_state(s, gang, seq)? {
                    GangState::Incomplete | GangState::Later => continue,
                    GangState::First => {}
                }
                if self.decide_gang(s, gang)?.is_none() {
                    slot(&mut self.slices, s)?.plane.pass_gang(q, seq);
                    continue;
                }
                let plane = &mut slot(&mut self.slices, s)?.plane;
                plane.committing = true;
                plane.in_flight = Some((q, seq));
                let (epoch, resets) = (plane.epoch, plane.resets);
                return self.schedule(
                    self.after(self.config.commit_ns)?,
                    Event::GangDue {
                        slice: s,
                        epoch,
                        seq,
                        resets,
                    },
                );
            }
            let need = self.demand(id)?;
            if slot(&mut self.slices, s)?.plane.infeasible.contains(&need) {
                slot(&mut self.slices, s)?.plane.proposals.remove(&seq);
                if self.send_preemption(s, q, seq, id)? {
                    return Ok(());
                }
                slot(&mut self.slices, s)?.plane.pass(q, seq);
                tally(&mut self.report.class_skips, 1, self.config.max_steps)?;
                continue;
            }
            let plane = &mut slot(&mut self.slices, s)?.plane;
            let Some(decision) = plane.proposals.remove(&seq) else {
                if !plane.deciding.contains(&seq) {
                    plane.mark_undecided(seq);
                    self.assign(s)?;
                }
                return Ok(());
            };
            let decision = self.late_bind(s, &need, decision)?;
            if decision.node().is_none() {
                self.mark_infeasible(s, need)?;
                if self.send_preemption(s, q, seq, id)? {
                    return Ok(());
                }
                slot(&mut self.slices, s)?.plane.pass(q, seq);
                continue;
            }
            let plane = &mut slot(&mut self.slices, s)?.plane;
            plane.committing = true;
            plane.in_flight = Some((q, seq));
            let (epoch, resets) = (plane.epoch, plane.resets);
            return self.schedule(
                self.after(self.config.commit_ns)?,
                Event::CommitDue {
                    slice: s,
                    epoch,
                    seq,
                    decision,
                    resets,
                },
            );
        }
        Ok(())
    }

    fn commit_due(
        &mut self,
        s: usize,
        epoch: u64,
        seq: u64,
        decision: Decision,
        resets: u64,
    ) -> Result<(), SimError> {
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if epoch != plane.epoch || !plane.up {
            return Ok(());
        }
        plane.committing = false;
        plane.in_flight = None;
        let (Some(id), Some(&queue)) = (plane.pending_id(seq), plane.queue_of.get(&seq)) else {
            return self.sequence(s);
        };
        if resets != plane.resets {
            // Capacity came back, or work was submitted ahead, since it was sent: a workload ahead
            // may hold now. Take the queues again.
            plane.proposals.insert(seq, decision);
            return self.sequence(s);
        }
        let need = self.demand(id)?;
        let decision = self.late_bind(s, &need, decision)?;
        let Some(key) = decision.node() else {
            slot(&mut self.slices, s)?.plane.pass(queue, seq);
            self.mark_infeasible(s, need)?;
            return self.sequence(s);
        };
        let version = *slot(&mut self.slices, s)?
            .plane
            .versions
            .get(&id)
            .ok_or(SimError::UnknownWorkload(id))?;
        let node = self.layout.global(s, key.node)?;
        let Some(outcome) = self.write(
            s,
            Write::Place {
                workload: id,
                node,
                version,
            },
        )?
        else {
            return Ok(());
        };
        let Outcome::Applied { version, .. } = outcome else {
            return Err(SimError::UnexpectedConflict(id));
        };
        let changed = slot(&mut self.slices, s)?
            .plane
            .view
            .take(key.node, id, &need)?;
        self.note_change(s, changed)?;
        let (spec, write) = self
            .store
            .record(id)
            .map(|r| (r.spec, r.placed_write))
            .ok_or(SimError::UnknownWorkload(id))?;
        let preemption = self.config.preemptions > 0;
        let plane = &mut slot(&mut self.slices, s)?.plane;
        plane.shares.take(spec.tenancy.owner, &need.need)?;
        let reopened = plane.note_placed(&spec, key.node, write, preemption);
        plane.dequeue(seq);
        plane.versions.insert(id, version);
        if !reopened {
            plane.pass(queue, seq);
        }
        tally(&mut self.report.placements, 1, self.config.max_steps)?;
        let arrives = self
            .store
            .record(id)
            .map(|r| r.spec.arrives)
            .ok_or(SimError::UnknownWorkload(id))?;
        self.latencies.push(
            self.now
                .checked_sub(arrives)
                .ok_or(SimError::TimeOverflow)?,
        );
        self.sequence(s)
    }

    /// A gang's commit come due: decided whole again against slice `s`'s view now (late binding),
    /// and written in one store write.
    fn gang_due(&mut self, s: usize, epoch: u64, seq: u64, resets: u64) -> Result<(), SimError> {
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if epoch != plane.epoch || !plane.up {
            return Ok(());
        }
        plane.committing = false;
        plane.in_flight = None;
        let (Some(id), Some(&queue)) = (plane.pending_id(seq), plane.queue_of.get(&seq)) else {
            return self.sequence(s);
        };
        if resets != plane.resets {
            return self.sequence(s);
        }
        let gang = self
            .spec(id)?
            .tenancy
            .gang
            .ok_or(SimError::Config("a gang commit for a workload in no gang"))?;
        let Some(members) = self.decide_gang(s, gang)? else {
            slot(&mut self.slices, s)?.plane.pass_gang(queue, seq);
            return self.sequence(s);
        };
        let mut write = Vec::with_capacity(members.len());
        for &(w, n, v, _) in &members {
            write.push((w, self.layout.global(s, n)?, v));
        }
        let Some(versions) = self.write_gang(s, &write)? else {
            return Ok(());
        };
        let mut reopened = false;
        for (&(member, node, _, member_seq), version) in members.iter().zip(versions) {
            let spec = self.spec(member)?;
            let changed =
                slot(&mut self.slices, s)?
                    .plane
                    .view
                    .take(node, member, &spec.demand())?;
            self.note_change(s, changed)?;
            let write = self.store.record(member).map_or(0, |r| r.placed_write);
            let preemption = self.config.preemptions > 0;
            let plane = &mut slot(&mut self.slices, s)?.plane;
            plane.shares.take(spec.tenancy.owner, &spec.need)?;
            reopened |= plane.note_placed(&spec, node, write, preemption);
            plane.dequeue(member_seq);
            plane.proposals.remove(&member_seq);
            plane.versions.insert(member, version);
            tally(&mut self.report.placements, 1, self.config.max_steps)?;
            self.latencies.push(
                self.now
                    .checked_sub(spec.arrives)
                    .ok_or(SimError::TimeOverflow)?,
            );
        }
        tally(&mut self.report.gangs, 1, self.config.max_steps)?;
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if plane.gang_floors.get(&queue) == Some(&seq) {
            plane.gang_floors.remove(&queue);
        }
        if !reopened {
            plane.pass(queue, seq);
        }
        self.sequence(s)
    }

    /// Victims for `id` against slice `s`'s view now, within the run's budget of preemptions.
    fn choose(&mut self, s: usize, id: WorkloadId) -> Result<Option<Preemption>, SimError> {
        if self.config.preemptions == 0 {
            return Ok(None);
        }
        let spec = self.spec(id)?;
        let mut passed = Passed::default();
        let plane = &mut slot(&mut self.slices, s)?.plane;
        let placed: Vec<Placed> = plane.placed.values().copied().collect();
        let chosen = fair::choose_victims(
            &mut plane.view,
            &placed,
            &spec,
            &plane.shares,
            &self.cluster.groups,
            &plane.disrupted,
            &mut passed,
        )?;
        tally(
            &mut self.report.budget_passes,
            passed.for_budget,
            self.config.max_steps,
        )?;
        let Some(chosen) = chosen else {
            return Ok(None);
        };
        let after = u64::try_from(chosen.victims.len())
            .ok()
            .and_then(|n| self.report.preempted.checked_add(n));
        if after.is_none_or(|n| n > self.config.preemptions) {
            tally(
                &mut self.report.preemptions_refused,
                1,
                self.config.max_steps,
            )?;
            return Ok(None);
        }
        Ok(Some(chosen))
    }

    /// Sends a preemption for `seq` of slice `s`'s queue `q` if one can be made: `true` when one is
    /// in flight.
    fn send_preemption(
        &mut self,
        s: usize,
        q: Queue,
        seq: u64,
        id: WorkloadId,
    ) -> Result<bool, SimError> {
        if self.choose(s, id)?.is_none() {
            return Ok(false);
        }
        let plane = &mut slot(&mut self.slices, s)?.plane;
        plane.committing = true;
        plane.in_flight = Some((q, seq));
        let (epoch, resets) = (plane.epoch, plane.resets);
        self.schedule(
            self.after(self.config.commit_ns)?,
            Event::PreemptDue {
                slice: s,
                epoch,
                seq,
                resets,
            },
        )?;
        Ok(true)
    }

    /// A preemption's commit come due: its victims chosen again against slice `s`'s view now (late
    /// binding), and written with the preemptor's placement in one store write.
    fn preempt_due(&mut self, s: usize, epoch: u64, seq: u64, resets: u64) -> Result<(), SimError> {
        let plane = &mut slot(&mut self.slices, s)?.plane;
        if epoch != plane.epoch || !plane.up {
            return Ok(());
        }
        plane.committing = false;
        plane.in_flight = None;
        let Some(id) = plane.pending_id(seq) else {
            return self.sequence(s);
        };
        if resets != plane.resets {
            return self.sequence(s);
        }
        let Some(chosen) = self.choose(s, id)? else {
            return self.sequence(s);
        };
        let version = |plane: &Plane, w: WorkloadId| {
            plane
                .versions
                .get(&w)
                .copied()
                .ok_or(SimError::UnknownWorkload(w))
        };
        // The victims and the preemptor by the slice's node ids, for its plane, and as the store
        // names their nodes, for the write.
        let plane = plane_at(&self.slices, s)?;
        let mut victims = Vec::with_capacity(chosen.victims.len());
        let mut written = Vec::with_capacity(chosen.victims.len());
        for &v in &chosen.victims {
            let node = plane
                .placed
                .get(&v)
                .map(|p| p.node)
                .ok_or(SimError::UnknownWorkload(v))?;
            let at = version(plane, v)?;
            victims.push((v, node, at));
            written.push((v, self.layout.global(s, node)?, at));
        }
        let head = (id, self.layout.global(s, chosen.node)?, version(plane, id)?);
        let Some((versions, placed)) = self.write_preempt(s, &written, head)? else {
            return Ok(());
        };
        let max = self.config.max_steps;
        for (&(v, node, _), new) in victims.iter().zip(versions) {
            let spec = self.spec(v)?;
            let changed = slot(&mut self.slices, s)?
                .plane
                .view
                .give(node, v, &spec.demand())?;
            self.note_change(s, changed)?;
            let victim_seq = self
                .store
                .record(v)
                .map(|r| r.seq)
                .ok_or(SimError::UnknownWorkload(v))?;
            let plane = &mut slot(&mut self.slices, s)?.plane;
            plane.shares.give(spec.tenancy.owner, &spec.need)?;
            plane.versions.insert(v, new);
            plane.placed.remove(&v);
            plane.enqueue(&spec, victim_seq);
            plane.disrupt(&spec)?;
            tally(&mut self.report.preempted, 1, max)?;
        }
        let spec = self.spec(id)?;
        let changed =
            slot(&mut self.slices, s)?
                .plane
                .view
                .take(chosen.node, id, &spec.demand())?;
        self.note_change(s, changed)?;
        let write = self.store.record(id).map_or(0, |r| r.placed_write);
        let plane = &mut slot(&mut self.slices, s)?.plane;
        plane.shares.take(spec.tenancy.owner, &spec.need)?;
        // Every cursor goes below, whatever this placement ended.
        plane.note_placed(&spec, chosen.node, write, true);
        plane.dequeue(seq);
        plane.proposals.remove(&seq);
        plane.versions.insert(id, placed);
        tally(&mut self.report.placements, 1, max)?;
        tally(&mut self.report.preemptions, 1, max)?;
        self.latencies.push(
            self.now
                .checked_sub(spec.arrives)
                .ok_or(SimError::TimeOverflow)?,
        );
        // Victims are pending again and may hold elsewhere: every claim of holding nowhere goes.
        let plane = &mut slot(&mut self.slices, s)?.plane;
        plane.cursors.clear();
        plane.gang_floors.clear();
        plane.infeasible.clear();
        plane.resets = plane
            .resets
            .checked_add(1)
            .ok_or(SimError::StepBudget(max))?;
        self.assign(s)?;
        self.sequence(s)
    }

    /// A preemption's store write, where the nemesis strikes slice `s`'s shard as at any write: the
    /// victims' new versions and the preemptor's, or `None` if the shard crashed there.
    fn write_preempt(
        &mut self,
        s: usize,
        victims: &[(WorkloadId, NodeId, u64)],
        head: (WorkloadId, NodeId, u64),
    ) -> Result<Option<(Vec<u64>, u64)>, SimError> {
        let number = self.report.writes;
        self.report.writes = number
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        let owner = slot(&mut self.slices, s)?.owner;
        let crash = self.strike(number);
        if crash == Some(When::Before) {
            self.crash(owner, When::Before)?;
            return Ok(None);
        }
        let outcome = self
            .store
            .apply_preempt(self.now, victims, head, &self.cluster.groups)?;
        let PreemptOutcome::Applied {
            victims: new,
            placed,
        } = outcome
        else {
            return Err(SimError::UnexpectedConflict(head.0));
        };
        // The store's feed tells the nodes, whatever becomes of the plane: the victims' first, so
        // a node frees the room before its new run starts.
        for &(workload, node, _) in victims {
            let at =
                usize::try_from(workload.0).map_err(|_| SimError::UnknownWorkload(workload))?;
            self.schedule(
                self.after(self.config.feed_ns)?,
                Event::NodeStop { at, node },
            )?;
        }
        let at = usize::try_from(head.0.0).map_err(|_| SimError::UnknownWorkload(head.0))?;
        self.schedule(
            self.after(self.config.feed_ns)?,
            Event::NodeStart { at, node: head.1 },
        )?;
        if crash.is_some() {
            self.crash(owner, When::After)?;
            return Ok(None);
        }
        self.committed(s)?;
        Ok(Some((new, placed)))
    }

    /// A gang's store write, where the nemesis strikes slice `s`'s shard as at any write. `None`:
    /// the shard crashed there.
    fn write_gang(
        &mut self,
        s: usize,
        members: &[(WorkloadId, NodeId, u64)],
    ) -> Result<Option<Vec<u64>>, SimError> {
        let number = self.report.writes;
        self.report.writes = number
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        let owner = slot(&mut self.slices, s)?.owner;
        let crash = self.strike(number);
        if crash == Some(When::Before) {
            self.crash(owner, When::Before)?;
            return Ok(None);
        }
        let outcome = self.store.apply_gang(self.now, members)?;
        let GangOutcome::Applied { versions } = outcome else {
            let id = members.first().map_or(WorkloadId(u64::MAX), |m| m.0);
            return Err(SimError::UnexpectedConflict(id));
        };
        for &(workload, node, _) in members {
            let at =
                usize::try_from(workload.0).map_err(|_| SimError::UnknownWorkload(workload))?;
            self.schedule(
                self.after(self.config.feed_ns)?,
                Event::NodeStart { at, node },
            )?;
        }
        if crash.is_some() {
            self.crash(owner, When::After)?;
            return Ok(None);
        }
        self.committed(s)?;
        Ok(Some(versions))
    }

    fn note_change(&mut self, s: usize, key: Key) -> Result<(), SimError> {
        let window = self.config.change_window;
        let plane = &mut slot(&mut self.slices, s)?.plane;
        plane.change_no = plane
            .change_no
            .checked_add(1)
            .ok_or(SimError::StepBudget(self.config.max_steps))?;
        if plane.changes.len() >= window
            && let Some((no, _)) = plane.changes.pop_front()
        {
            plane.forgotten = no;
        }
        plane.changes.push_back((plane.change_no, key));
        Ok(())
    }

    /// The slice holding `node`, for a node agent's report.
    fn slice_of(&self, node: NodeId) -> Result<usize, SimError> {
        self.layout
            .slice_of(node)
            .ok_or(SimError::UnknownNode(node))
    }

    fn node_start(&mut self, at: usize, node: NodeId) -> Result<(), SimError> {
        let starts = self
            .starts
            .get_mut(at)
            .ok_or(SimError::Config("workload index"))?;
        *starts = starts.checked_add(1).ok_or(SimError::Config("starts"))?;
        let run = *starts;
        if let Some(live) = self.live.get_mut(at) {
            *live = true;
        }
        let ran = self
            .ran
            .get_mut(at)
            .ok_or(SimError::Config("workload index"))?;
        if ran.is_none() {
            *ran = Some(Ran {
                node,
                started: self.now,
                ended: None,
            });
        }
        let s = self.slice_of(node)?;
        let slice = slot(&mut self.slices, s)?;
        slice.running = slice
            .running
            .checked_add(1)
            .ok_or(SimError::Config("running"))?;
        if let Some(runs_for) = self.workload(at)?.runs_for {
            self.schedule(
                self.after(runs_for)?,
                Event::NodeDone {
                    at,
                    node,
                    attempt: 0,
                    run,
                },
            )?;
        }
        Ok(())
    }

    /// The node agent stops a preempted workload's run: it ends now, and its report, if one is
    /// still to come or being sent again, is dropped, as the run was stopped rather than done.
    fn node_stop(&mut self, at: usize, node: NodeId) -> Result<(), SimError> {
        let starts = self.starts.get(at).copied().unwrap_or(0);
        if let Some(stopped) = self.stopped.get_mut(at) {
            *stopped = starts;
        }
        if self.live.get(at).copied().unwrap_or(false) {
            if let Some(live) = self.live.get_mut(at) {
                *live = false;
            }
            let s = self.slice_of(node)?;
            let slice = slot(&mut self.slices, s)?;
            slice.running = slice
                .running
                .checked_sub(1)
                .ok_or(SimError::Config("running"))?;
            if let Some(Some(ran)) = self.ran.get_mut(at) {
                ran.ended.get_or_insert(self.now);
            }
        }
        Ok(())
    }

    fn node_done(
        &mut self,
        at: usize,
        node: NodeId,
        attempt: u32,
        run: u32,
    ) -> Result<(), SimError> {
        let id = self.workload(at)?.id;
        if self.stopped.get(at).is_some_and(|&s| run <= s) {
            return Ok(());
        }
        let s = self.slice_of(node)?;
        if attempt == 0 {
            if let Some(live) = self.live.get_mut(at) {
                *live = false;
            }
            // The run itself ends here; what follows is its report.
            let slice = slot(&mut self.slices, s)?;
            slice.running = slice
                .running
                .checked_sub(1)
                .ok_or(SimError::Config("running"))?;
            if let Some(Some(ran)) = self.ran.get_mut(at) {
                ran.ended.get_or_insert(self.now);
            }
        }
        let resend = |sim: &mut Self| -> Result<(), SimError> {
            let next = attempt
                .checked_add(1)
                .ok_or(SimError::Config("report attempts"))?;
            if next >= sim.config.report_attempts {
                return Err(SimError::ReportGaveUp(node, id, next));
            }
            sim.schedule(
                sim.after(sim.config.report_retry_ns)?,
                Event::NodeDone {
                    at,
                    node,
                    attempt: next,
                    run,
                },
            )
        };
        let plane = plane_at(&self.slices, s)?;
        if !plane.up {
            return resend(self);
        }
        // A workload the plane holds no version for was recorded done already (it rebuilt from a
        // store that has it complete); the store answers a repeat as a duplicate.
        let version = plane.versions.get(&id).copied().unwrap_or(0);
        let Some(outcome) = self.write(
            s,
            Write::Complete {
                workload: id,
                node,
                version,
            },
        )?
        else {
            return resend(self);
        };
        match outcome {
            Outcome::Applied { .. } => {
                let spec = self
                    .store
                    .record(id)
                    .map(|r| r.spec)
                    .ok_or(SimError::UnknownWorkload(id))?;
                let local = self
                    .layout
                    .local(s, node)
                    .ok_or(SimError::UnknownNode(node))?;
                let changed =
                    slot(&mut self.slices, s)?
                        .plane
                        .view
                        .give(local, spec.id, &spec.demand())?;
                self.note_change(s, changed)?;
                let max = self.config.max_steps;
                let plane = &mut slot(&mut self.slices, s)?.plane;
                plane.shares.give(spec.tenancy.owner, &spec.need)?;
                plane.versions.remove(&id);
                plane.placed.remove(&id);
                plane.cursors.clear();
                plane.gang_floors.clear();
                plane.infeasible.clear();
                plane.resets = plane
                    .resets
                    .checked_add(1)
                    .ok_or(SimError::StepBudget(max))?;
                self.assign(s)?;
                self.sequence(s)
            }
            Outcome::Duplicate { .. } => {
                slot(&mut self.slices, s)?.plane.versions.remove(&id);
                Ok(())
            }
            Outcome::Conflict(_) => {
                // The store preempted this run after it started (its record back in pending, or
                // placed again since) before the node saw the order to stop it: the run was
                // stopped, not done, and the workload runs again. Any other conflict is a bug.
                let superseded = self.store.record(id).is_some_and(|r| {
                    r.placements > run
                        || (r.placements == run && r.preempted && r.state == State::Pending)
                });
                if !superseded {
                    return Err(SimError::UnexpectedConflict(id));
                }
                if let Some(stopped) = self.stopped.get_mut(at) {
                    *stopped = (*stopped).max(run);
                }
                tally(
                    &mut self.report.reports_superseded,
                    1,
                    self.config.max_steps,
                )?;
                Ok(())
            }
        }
    }
}
