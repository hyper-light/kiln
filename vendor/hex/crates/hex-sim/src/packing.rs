//! Stranded capacity (`AC-T7.4.3`): what a placement policy leaves free that no workload can use.
//!
//! [`pack`] places a workload's arrivals in order from a given one, with no completions or, by its
//! [`Clock`], with them, through the fast path's pick ([`FitIndex::fit`]), passing
//! over one no node admits, until the load reaches what is asked (90% for `AC-T7.4.3`). The load is the dominant share of what is
//! placed: the larger of placed CPU over all CPU and placed memory over all memory, as Dominant
//! Resource Fairness measures a share, so a workload that runs out of CPU first is loaded by its CPU.
//!
//! It then measures stranded capacity: on each node where no shape of the workload's requests fits
//! what is free, that node's free CPU and free memory, which nothing can use because another
//! resource there is spent. One shape set stands for all of them: a node's free capacity can host
//! some request exactly when it can host a Pareto-minimal one. This is Borg's stranding, resources
//! "that cannot be used because another resource on the machine is fully allocated" (EuroSys 2015
//! §3.2), asked of one cell at one load. A policy that leaves every node part-free strands nothing at
//! a load it never has to pack tightly at, so [`PackReport::first_full_per_mille`] records the load
//! at the first arrival that found no node with room, and every arrival passed over is counted by its
//! kind: spreading shows there as an earlier refusal.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use crate::error::SimError;
use crate::index::{FitIndex, Pick, Refusals};
use crate::model::{Class, Classes, Cluster, NodeId, Resources, Time, WorkloadId, WorkloadSpec};
use crate::rng::Rng;

/// The load `AC-T7.4.3` measures stranded capacity at, per mille: 90%.
pub const LOAD_PER_MILLE: u64 = 900;

/// How many starts a packing measure runs from: as many as the claim "at least 95% of starts meet
/// the bar, at 95% confidence" needs when every one does, the least `n` with `0.95^n < 0.05` (the
/// claim rule G3's seeds carry, hex-sim.md).
pub const SEEDS: u64 = 59;

/// Seed `seed`'s starting arrival among `arrivals`, drawn uniformly: the starts are a random sample
/// of the day, as [`SEEDS`]' claim takes them to be.
pub fn start(seed: u64, arrivals: usize) -> usize {
    let last = u64::try_from(arrivals.saturating_sub(1)).unwrap_or(u64::MAX);
    usize::try_from(Rng::new(seed).range(0, last)).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackReport {
    pub total_cpu_milli: u64,
    pub total_mem_bytes: u64,
    pub placed_cpu_milli: u64,
    pub placed_mem_bytes: u64,
    pub free_cpu_milli: u64,
    pub free_mem_bytes: u64,
    /// Free CPU and memory on nodes where no workload shape fits what is free.
    pub stranded_cpu_milli: u64,
    pub stranded_mem_bytes: u64,
    pub placements: u64,
    /// Workloads that ended and gave their room back before the pack stopped: none under
    /// [`Clock::Held`], and under [`Clock::Compressed`] whether lifetimes entered at all.
    pub completions: u64,
    /// Arrivals no node admitted when they came.
    pub passed_over: u64,
    /// Of them, by kind, in [`Class::index`]'s order: OCI containers, Firecracker microVMs, Shards
    /// microVMs.
    pub passed_over_by_class: [u64; 3],
    /// Whether the load reached what was asked before the arrivals ran out.
    pub reached: bool,
    /// The load when the first arrival was passed over, for room or by a filter.
    pub first_refusal_per_mille: Option<u64>,
    /// The load when the first arrival found no node with room (none refused it by a filter alone).
    pub first_full_per_mille: Option<u64>,
}

impl PackReport {
    /// The dominant share placed, in per-mille: the larger of placed CPU over all CPU and placed
    /// memory over all memory.
    pub fn load_per_mille(&self) -> u64 {
        per_mille(self.placed_cpu_milli, self.total_cpu_milli)
            .max(per_mille(self.placed_mem_bytes, self.total_mem_bytes))
    }

    /// Stranded capacity in per-mille: the stranded share of CPU and of memory, summed.
    pub fn stranded_per_mille(&self) -> u64 {
        per_mille(self.stranded_cpu_milli, self.total_cpu_milli)
            .saturating_add(per_mille(self.stranded_mem_bytes, self.total_mem_bytes))
    }
}

/// `part` as a share of `whole` in per-mille; 0 of a whole of 0, and past u64 saturates.
fn per_mille(part: u64, whole: u64) -> u64 {
    u128::from(part)
        .checked_mul(1000)
        .and_then(|x| x.checked_div(u128::from(whole)))
        .map_or(0, |x| u64::try_from(x).unwrap_or(u64::MAX))
}

/// Each class's Pareto-minimal needs: no other need of the class is at most it in every resource. A
/// capacity that holds any need of a class holds one of these. Bounded by the workloads' count;
/// quadratic in the distinct needs.
pub fn minimal_shapes(cluster: &Cluster) -> Vec<(Resources, Class)> {
    let distinct: BTreeSet<(Resources, Class)> = cluster
        .workloads
        .iter()
        .map(|w| (w.need, w.class))
        .collect();
    let all: Vec<(Resources, Class)> = distinct.into_iter().collect();
    all.iter()
        .filter(|&&(s, c)| !all.iter().any(|&(o, oc)| oc == c && o != s && s.holds(&o)))
        .copied()
        .collect()
}

/// The same arrivals, every one of `class`: each workload's need moved from its own class's overhead
/// to `class`'s for its own memory, so a class can be measured alone on a day recorded with every
/// class.
pub fn alone(cluster: &Cluster, class: Class) -> Result<Cluster, SimError> {
    let mut out = cluster.clone();
    let overheads = &cluster.overheads;
    for w in &mut out.workloads {
        w.need.mem_bytes = overheads
            .own(w.class, w.need.mem_bytes)
            .and_then(|own| own.checked_add(overheads.of(class, own)?))
            .ok_or(SimError::Need(w.id))?;
        w.class = class;
    }
    Ok(out)
}

/// How a pack runs through time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// No completions: what is placed stays, so lifetimes don't enter.
    Held,
    /// Completions: arrivals come this many times faster than recorded and each runs for its recorded
    /// lifetime, so a workload reaches a load its own traffic wouldn't, and what ends leaves holes.
    Compressed(Time),
}

/// Packs `cluster`'s arrivals from the `from`th on, round the day (none from past the last), to
/// `load_per_mille`, then measures what is stranded.
pub fn pack(
    cluster: &Cluster,
    pick: &Pick,
    load_per_mille: u64,
    from: usize,
    clock: Clock,
) -> Result<PackReport, SimError> {
    if clock == Clock::Compressed(0) {
        return Err(SimError::Config("a compressed clock runs at least 1x"));
    }
    let mut index = FitIndex::new(&cluster.nodes)?;
    let mut r = PackReport::default();
    // The load and what is stranded count only the nodes that can run some workload here.
    let asked = cluster
        .workloads
        .iter()
        .fold(Classes::NONE, |all, w| all.with(w.class));
    let eligible = || cluster.nodes.iter().filter(move |n| n.classes.meets(asked));
    for node in eligible() {
        r.total_cpu_milli = r
            .total_cpu_milli
            .checked_add(node.capacity.cpu_milli)
            .ok_or(SimError::Resources(node.id))?;
        r.total_mem_bytes = r
            .total_mem_bytes
            .checked_add(node.capacity.mem_bytes)
            .ok_or(SimError::Resources(node.id))?;
    }
    // From the `from`th arrival on, then round the day from its first: each wrapped arrival comes a
    // day's span later, so a start late in the day still has the whole day to draw on.
    let all = &cluster.workloads;
    let none: &[WorkloadSpec] = &[];
    let (later, earlier) = match (all.get(from..), all.get(..from)) {
        (Some(later), Some(earlier)) if !later.is_empty() => (later, earlier),
        _ => (none, none),
    };
    let span = match (all.first(), all.last()) {
        (Some(first), Some(last)) => last.arrives.saturating_sub(first.arrives).saturating_add(1),
        _ => 0,
    };
    let start = later.first().map_or(0, |w| w.arrives);
    let arrivals = later
        .iter()
        .map(|w| (w, w.arrives))
        .chain(earlier.iter().map(|w| (w, w.arrives.saturating_add(span))));
    // What runs, by when it ends. Bounded by the arrivals: each goes in once, when placed.
    let mut running: BinaryHeap<Reverse<(Time, WorkloadId, NodeId)>> = BinaryHeap::new();
    for (w, arrives) in arrivals {
        if r.load_per_mille() >= load_per_mille {
            r.reached = true;
            break;
        }
        let now = match clock {
            Clock::Held => 0,
            Clock::Compressed(speed) => arrives
                .saturating_sub(start)
                .checked_div(speed)
                .unwrap_or(0),
        };
        while let Some(&Reverse((end, ended, node))) = running.peek() {
            if end > now {
                break;
            }
            running.pop();
            let Some(done) = usize::try_from(ended.0).ok().and_then(|at| all.get(at)) else {
                continue;
            };
            index.give(node, done.id, &done.demand())?;
            r.placed_cpu_milli = r.placed_cpu_milli.saturating_sub(done.need.cpu_milli);
            r.placed_mem_bytes = r.placed_mem_bytes.saturating_sub(done.need.mem_bytes);
            r.completions = r.completions.saturating_add(1);
        }
        let demand = w.demand();
        let fit = index.fit(&demand, pick);
        let Some(key) = fit.best else {
            let load = r.load_per_mille();
            r.passed_over = r.passed_over.saturating_add(1);
            if let Some(n) = r.passed_over_by_class.get_mut(w.class.index()) {
                *n = n.saturating_add(1);
            }
            r.first_refusal_per_mille.get_or_insert(load);
            if r.first_full_per_mille.is_none() && fit.refused == Refusals::default() {
                r.first_full_per_mille = Some(load);
            }
            continue;
        };
        index.take(key.node, w.id, &demand)?;
        r.placed_cpu_milli = r.placed_cpu_milli.saturating_add(w.need.cpu_milli);
        r.placed_mem_bytes = r.placed_mem_bytes.saturating_add(w.need.mem_bytes);
        r.placements = r.placements.saturating_add(1);
        if let (Clock::Compressed(_), Some(life)) = (clock, w.runs_for) {
            running.push(Reverse((now.saturating_add(life), w.id, key.node)));
        }
    }
    r.reached = r.reached || r.load_per_mille() >= load_per_mille;
    let shapes = minimal_shapes(cluster);
    for node in eligible() {
        let free = index.key(node.id)?;
        let free = Resources {
            cpu_milli: free.cpu_milli,
            mem_bytes: free.mem_bytes,
            gpus: free.gpus,
            disk_bytes: 0,
        };
        r.free_cpu_milli = r.free_cpu_milli.saturating_add(free.cpu_milli);
        r.free_mem_bytes = r.free_mem_bytes.saturating_add(free.mem_bytes);
        if !shapes
            .iter()
            .any(|&(shape, class)| node.classes.has(class) && free.holds(&shape))
        {
            r.stranded_cpu_milli = r.stranded_cpu_milli.saturating_add(free.cpu_milli);
            r.stranded_mem_bytes = r.stranded_mem_bytes.saturating_add(free.mem_bytes);
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, NodeId, NodeSpec, Tenancy, WorkloadId, WorkloadSpec};

    fn res(cpu: u64, mem: u64) -> Resources {
        Resources {
            cpu_milli: cpu,
            mem_bytes: mem,
            gpus: 0,
            disk_bytes: 0,
        }
    }

    fn cluster(nodes: u32, node: Resources, needs: &[Resources]) -> Cluster {
        Cluster::single_owner(
            (0..nodes)
                .map(|i| NodeSpec {
                    routes: None,
                    costs: [None; crate::model::Class::COUNT],
                    priors: Default::default(),
                    runtime_pools: Vec::new(),
                    id: NodeId(i),
                    capacity: node,
                    background_milli: 0,
                    classes: Classes::ALL,
                    platform: Default::default(),
                    virt: Default::default(),
                    devices: Vec::new(),
                })
                .collect(),
            needs
                .iter()
                .zip(0u64..)
                .map(|(&need, id)| WorkloadSpec {
                    id: WorkloadId(id),
                    need,
                    kind: Kind::Plain,
                    arrives: id,
                    runs_for: None,
                    tenancy: Tenancy::default(),
                    class: Class::Oci,
                    ask: Default::default(),
                })
                .collect(),
        )
    }

    /// AC-T7.4.3's measure on a hand-made case: two nodes of (4 CPU, 4 mem); work of (3,1) and (1,3).
    /// Placing the two (3,1)s on separate nodes leaves (1,3) free on each, which a (1,3) uses; that is
    /// no stranding. The free capacity a shape can't use is counted.
    #[test]
    fn stranded_is_free_capacity_no_shape_fits() {
        let c = cluster(2, res(4000, 4), &[res(3000, 1), res(3000, 1), res(1000, 3)]);
        assert_eq!(minimal_shapes(&c).len(), 2);
        let r = pack(&c, &Pick::default(), 1000, 0, Clock::Held).unwrap();
        assert_eq!((r.placements, r.passed_over), (3, 0));
        // After all three: one node holds (3,1)+(1,3) = full; the other (3,1), free (1,3), which (1,3) fits.
        assert_eq!((r.stranded_cpu_milli, r.stranded_mem_bytes), (0, 0));
    }

    /// Do a memory-bound workload; expect its load to be its memory's share, the CPU left on a node
    /// whose memory is spent counted as stranded, and the first refusal's load recorded.
    #[test]
    fn a_memory_bound_load_strands_the_cpu_beside_spent_memory() {
        let c = cluster(1, res(8000, 4), &[res(1000, 3), res(1000, 2)]);
        let r = pack(&c, &Pick::default(), 1000, 0, Clock::Held).unwrap();
        assert_eq!((r.placements, r.passed_over), (1, 1));
        assert_eq!(r.load_per_mille(), 750);
        assert_eq!(r.first_full_per_mille, Some(750));
        // Free (7000, 1): the (1000, 2) shape doesn't fit, so all of it is stranded.
        assert_eq!((r.stranded_cpu_milli, r.stranded_mem_bytes), (7000, 1));
        assert_eq!(r.stranded_per_mille(), 875 + 250);
    }

    /// Do arrivals no node has room for, two OCI containers and a Shards microVM, while the cluster is
    /// mostly idle; expect each counted by its kind and the load at the first recorded, whatever that
    /// load: no count of them kept and no load chosen to call one early.
    #[test]
    fn every_refusal_is_counted_by_kind_with_the_load_at_the_first() {
        let mut needs = vec![res(1000, 1)];
        needs.extend([res(9000, 1); 3]);
        let mut c = cluster(1, res(8000, 8), &needs);
        c.workloads[3].class = Class::Shards;
        let r = pack(&c, &Pick::default(), 1000, 0, Clock::Held).unwrap();
        assert_eq!(r.passed_over, 3);
        assert_eq!(r.passed_over_by_class, [2, 0, 1]);
        assert_eq!(r.first_refusal_per_mille, Some(125));
        assert_eq!(r.first_full_per_mille, Some(125));
    }

    /// Do two arrivals that each fill the node, the first ending before the second comes; expect a held
    /// clock to refuse the second, a clock at the recorded speed to place it in the room the first
    /// gave back, a clock fast enough that the first hasn't ended to refuse it, the one completion
    /// counted only where it came, and a zero speed refused typed.
    #[test]
    fn a_compressed_clock_gives_back_what_ended() {
        let mut c = cluster(1, res(8000, 8), &[res(8000, 8), res(8000, 8)]);
        c.workloads[0].runs_for = Some(10);
        c.workloads[1].arrives = 100;
        let at = |clock| pack(&c, &Pick::default(), 2000, 0, clock).unwrap();
        assert_eq!(at(Clock::Held).passed_over, 1);
        assert_eq!(at(Clock::Compressed(1)).passed_over, 0);
        assert_eq!(at(Clock::Compressed(1)).placed_cpu_milli, 8000);
        assert_eq!(at(Clock::Compressed(100)).passed_over, 1);
        let completions =
            [Clock::Held, Clock::Compressed(1), Clock::Compressed(100)].map(|c| at(c).completions);
        assert_eq!(completions, [0, 1, 0]);
        assert_eq!(
            pack(&c, &Pick::default(), 2000, 0, Clock::Compressed(0)),
            Err(SimError::Config("a compressed clock runs at least 1x"))
        );
    }

    /// Do a day of an OCI container and a Shards microVM, overheads 1 and 5, run as Shards alone;
    /// expect each need moved to Shards' overhead and its class Shards, and a need smaller than its
    /// own class's overhead refused typed. Then with Shards' overhead by size (3 for a 2-byte guest, 5
    /// for a 4-byte one): expect each need moved to its own size's, and a size with no row refused.
    #[test]
    fn alone_moves_each_need_to_its_new_class() {
        let mut c = cluster(1, res(8000, 64), &[res(1000, 3), res(1000, 9)]);
        c.overheads.set(Class::Oci, 1);
        c.overheads.set(Class::Shards, 5);
        c.workloads[1].class = Class::Shards;
        let a = alone(&c, Class::Shards).unwrap();
        let got: Vec<_> = a
            .workloads
            .iter()
            .map(|w| (w.need.mem_bytes, w.class))
            .collect();
        assert_eq!(got, vec![(7, Class::Shards), (9, Class::Shards)]);
        c.workloads[0].need.mem_bytes = 0;
        assert_eq!(alone(&c, Class::Shards), Err(SimError::Need(WorkloadId(0))));

        let mut c = cluster(1, res(8000, 64), &[res(1000, 3), res(1000, 9)]);
        c.overheads.set(Class::Oci, 1);
        c.overheads.set_size(Class::Shards, 2, 3).unwrap();
        c.overheads.set_size(Class::Shards, 4, 5).unwrap();
        c.workloads[1].class = Class::Shards;
        let a = alone(&c, Class::Shards).unwrap();
        let got: Vec<_> = a.workloads.iter().map(|w| w.need.mem_bytes).collect();
        assert_eq!(got, vec![5, 9]);
        c.workloads[0].need.mem_bytes = 4;
        assert_eq!(alone(&c, Class::Shards), Err(SimError::Need(WorkloadId(0))));
    }

    /// Do OCI containers on a node that runs every class and on one that runs only Shards; expect
    /// the Shards-only node, which nothing here can use, left out of the load and of what is
    /// stranded.
    #[test]
    fn a_node_no_workload_here_can_run_isnt_counted() {
        let mut c = cluster(2, res(4000, 4), &[res(4000, 4)]);
        c.nodes[1].classes = Classes::NONE.with(Class::Shards);
        let r = pack(&c, &Pick::default(), 1000, 0, Clock::Held).unwrap();
        assert_eq!(
            (r.total_cpu_milli, r.placements, r.reached),
            (4000, 1, true)
        );
        assert_eq!(r.stranded_per_mille(), 0);
    }

    /// Do the claim's count: the least `n` whose `n` passing starts leave a start failing at least 5%
    /// of the time under 5% likely. Expect [`SEEDS`].
    #[test]
    fn the_seeds_are_what_the_claim_needs() {
        let (mut n, mut all_pass) = (0u64, 1.0f64);
        while all_pass >= 0.05 {
            all_pass *= 0.95;
            n += 1;
        }
        assert_eq!(n, SEEDS);
    }

    /// Do each seed's start over a day of 25,295 arrivals and of one; expect it inside the day, one
    /// seed one start, and the 59 not all alike.
    #[test]
    fn a_seed_names_one_start_inside_the_day() {
        let starts: Vec<usize> = (0..SEEDS).map(|s| start(s, 25_295)).collect();
        assert!(starts.iter().all(|&a| a < 25_295));
        assert_eq!(
            starts,
            (0..SEEDS).map(|s| start(s, 25_295)).collect::<Vec<_>>()
        );
        assert!(starts.windows(2).any(|p| p[0] != p[1]));
        assert_eq!(start(7, 1), 0);
        assert_eq!(start(7, 0), 0);
    }

    /// Do a pack from a later arrival; expect the arrivals before it left out.
    #[test]
    fn a_pack_starts_at_its_arrival() {
        let c = cluster(1, res(8000, 8), &[res(8000, 8), res(1000, 1)]);
        let r = pack(&c, &Pick::default(), 1000, 1, Clock::Held).unwrap();
        assert_eq!((r.placements, r.placed_cpu_milli), (1, 1000));
        let past = pack(&c, &Pick::default(), 1000, 9, Clock::Held).unwrap();
        assert_eq!(past.placements, 0);
    }
}
