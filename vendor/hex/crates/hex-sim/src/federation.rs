//! Cells and regions (`docs/design/hex-sim.md` H64-H66): a federation over independent cells, each
//! a hex-sim run of its own, with its own store and shards. A pure core: it routes workloads to
//! cells, reads a region's capacity from its cells' stores and re-places a lost region's work; each
//! cell runs as [`sim::run`] runs it alone.

use crate::error::SimError;
use crate::index::{FitIndex, Pick};
use crate::model::{Classes, Cluster, Demand, NodeId, NodeSpec, Time, WorkloadId, WorkloadSpec};
use crate::sim::{self, Config, Layout, Nemesis, Run};
use crate::store::Op;

/// The slowest round trip between two of the eight Azure regions in G3's WAN model: Southeast Asia
/// to Brazil South, 331 ms, Azure's published monthly P50 (`crates/hex-sim/src/wan.rs:45-54` on
/// cp/t7-sched-g3 at 5920b50; "Azure network round-trip latency statistics", 2026-07-30).
pub const SLOWEST_REGION_RTT_NS: Time = 331_000_000;

/// How long the federation waits, unheard, before it treats a region as lost: the bound by which
/// hyper-swim's own detector condemns a crashed member (G5's H40; hyper-raft
/// `crates/hyper-swim/src/detector.rs:1914-1957` at 6a236ae), with the regions as its members, so
/// a region is called lost by the same math as a node. From the last answer to a condemnation:
/// `2(2m - 1) + 1` periods for `m` other regions (SWIM's round-robin, §4.3), each a direct stage
/// and a relay stage at RFC 6298's retransmission timeout for the slowest inter-region round trip
/// `r` after a first sample (SRTT = r, RTTVAR = r/2, RTO = SRTT + 4 RTTVAR = 3r, §2.2-2.3,
/// rounded up to 1 s, §2.4), plus one round trip for another member's answer, the proof that the
/// condemner's own network works. Refused for one region: nothing else watches it.
pub fn region_grace_ns(regions: u32) -> Result<Time, SimError> {
    const SECOND: Time = 1_000_000_000;
    let m = u64::from(regions)
        .checked_sub(1)
        .filter(|&m| m > 0)
        .ok_or(SimError::Config(
            "a region's loss is seen by another region",
        ))?;
    let periods = m
        .checked_mul(2)
        .and_then(|x| x.checked_sub(1))
        .and_then(|x| x.checked_mul(2))
        .and_then(|x| x.checked_add(1))
        .ok_or(SimError::TimeOverflow)?;
    let stage = SLOWEST_REGION_RTT_NS
        .checked_mul(3)
        .ok_or(SimError::TimeOverflow)?
        .max(SECOND);
    periods
        .checked_mul(stage.checked_mul(2).ok_or(SimError::TimeOverflow)?)
        .and_then(|x| x.checked_add(SLOWEST_REGION_RTT_NS))
        .ok_or(SimError::TimeOverflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topology {
    pub regions: u32,
    /// Cells in each region.
    pub cells: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    None,
    /// Cell `cell` of region `region` loses its control plane for good at `at` (H65).
    Cell {
        region: u32,
        cell: u32,
        at: Time,
    },
    /// Region `region` is lost wholesale at `at`, its nodes with its cells' planes (H66).
    Region {
        region: u32,
        at: Time,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FedConfig {
    pub topology: Topology,
    /// Every cell's configuration; a lost cell's nemesis is the loss.
    pub cell: Config,
    pub loss: Loss,
    pub region_grace_ns: Time,
}

/// One cell's run: its own cluster, node and workload ids dense within it, and the cluster's id of
/// each of its workloads.
#[derive(Debug)]
pub struct CellRun {
    pub region: u32,
    pub cell: u32,
    /// The classes the cell declares: those any of its nodes runs.
    pub classes: Classes,
    pub cluster: Cluster,
    pub ids: Vec<WorkloadId>,
    pub run: Run,
}

/// Where a workload moved off a lost region went (H66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    /// To cell `cell` of region `region`, whose free nodes held it.
    To { region: u32, cell: u32 },
    /// No surviving region's free nodes held it: refused.
    NoCapacity,
}

#[derive(Debug)]
pub struct FedRun {
    /// Every cell, region by region, cell by cell.
    pub cells: Vec<CellRun>,
    /// Each workload moved off a lost region, by its id in the cluster, in id order.
    pub moved: Vec<(WorkloadId, Moved)>,
}

/// The regions over the cluster's nodes, and each region's cells over its nodes, as [`Layout`]
/// splits a cell's nodes into slices: contiguous ranges of ids.
#[derive(Debug)]
struct Topo {
    regions: Layout,
    cells: Vec<Layout>,
    topology: Topology,
}

impl Topo {
    fn try_clone(&self) -> Result<Self, SimError> {
        let regions = self.regions.try_clone()?;
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(self.cells.len())
            .map_err(|_| SimError::Full {
                what: "topology snapshot regions",
                bound: self.cells.len(),
            })?;
        for cell in &self.cells {
            cells.push(cell.try_clone()?);
        }
        Ok(Self {
            regions,
            cells,
            topology: self.topology,
        })
    }

    fn new(nodes: &[NodeSpec], topology: Topology) -> Result<Self, SimError> {
        if topology.cells == 0 {
            return Err(SimError::Config("a region has at least one cell"));
        }
        let regions = Layout::new(nodes, topology.regions)?;
        let mut cells = Vec::with_capacity(regions.len());
        for r in 0..regions.len() {
            let region = regions.nodes(r).ok_or(SimError::Config("region index"))?;
            cells.push(Layout::new(region, topology.cells)?);
        }
        Ok(Self {
            regions,
            cells,
            topology,
        })
    }

    /// The region a workload goes to, and the cell in it: its home region (`id mod regions`) or the
    /// first after whose empty nodes admit it, then that region's first such cell from `(id /
    /// regions) mod cells` (H64), so the cell doesn't repeat the region's choice. One that nothing
    /// admits goes to its home's home cell, which refuses it.
    fn route(&self, spec: &WorkloadSpec) -> Result<usize, SimError> {
        let r = match self.regions.route(spec) {
            Some(r) => r,
            None => home(spec.id, self.topology.regions)?,
        };
        let within = WorkloadSpec {
            id: within(spec.id, self.topology.regions)?,
            ..*spec
        };
        let cells = self.cells.get(r).ok_or(SimError::Config("region index"))?;
        let c = match cells.route(&within) {
            Some(c) => c,
            None => home(within.id, self.topology.cells)?,
        };
        self.index(r, c)
    }

    fn index(&self, r: usize, c: usize) -> Result<usize, SimError> {
        let cells = usize::try_from(self.topology.cells).map_err(|_| SimError::Config("cells"))?;
        r.checked_mul(cells)
            .and_then(|x| x.checked_add(c))
            .ok_or(SimError::Config("cells"))
    }

    fn len(&self) -> Result<usize, SimError> {
        self.index(self.regions.len(), 0)
    }

    fn nodes(&self, at: usize) -> Result<&[NodeSpec], SimError> {
        let (r, c) = self.place(at)?;
        self.cells
            .get(r)
            .and_then(|l| l.nodes(c))
            .ok_or(SimError::Config("cell index"))
    }

    /// The classes cell `at` declares.
    fn classes(&self, at: usize) -> Result<Classes, SimError> {
        let (r, c) = self.place(at)?;
        self.cells
            .get(r)
            .and_then(|l| l.classes(c))
            .ok_or(SimError::Config("cell index"))
    }

    fn place(&self, at: usize) -> Result<(usize, usize), SimError> {
        let cells = usize::try_from(self.topology.cells).map_err(|_| SimError::Config("cells"))?;
        Ok((
            at.checked_div(cells).ok_or(SimError::Config("cells"))?,
            at.checked_rem(cells).ok_or(SimError::Config("cells"))?,
        ))
    }
}

/// The id a workload's cell is chosen by within its region: its id over the regions, so a region's
/// cells see every residue of it whatever the two counts share.
fn within(id: WorkloadId, regions: u32) -> Result<WorkloadId, SimError> {
    id.0.checked_div(u64::from(regions))
        .map(WorkloadId)
        .ok_or(SimError::Config("a topology has at least one region"))
}

fn home(id: WorkloadId, of: u32) -> Result<usize, SimError> {
    id.0.checked_rem(u64::from(of))
        .and_then(|x| usize::try_from(x).ok())
        .ok_or(SimError::Config(
            "a topology has at least one region and cell",
        ))
}

pub fn run(cluster: &Cluster, config: &FedConfig) -> Result<FedRun, SimError> {
    let topo = Topo::new(&cluster.nodes, config.topology)?;
    match config.loss {
        Loss::None => {}
        Loss::Cell { region, cell, .. } => {
            if region >= config.topology.regions || cell >= config.topology.cells {
                return Err(SimError::Config("a lost cell is one of the topology's"));
            }
        }
        Loss::Region { region, .. } => {
            if config.topology.regions < 2 || region >= config.topology.regions {
                return Err(SimError::Config(
                    "a lost region is one of two or more, so the others can take its work",
                ));
            }
        }
    }
    let n = topo.len()?;
    let mut work: Vec<Vec<WorkloadSpec>> = vec![Vec::new(); n];
    // Work that reaches a region at or after its loss never enters it: it moves with the rest.
    let mut late = Vec::new();
    for w in &cluster.workloads {
        let i = topo.route(w)?;
        match config.loss {
            Loss::Region { region, at }
                if w.arrives >= at && u32::try_from(topo.place(i)?.0).ok() == Some(region) =>
            {
                late.push(*w);
            }
            Loss::None | Loss::Cell { .. } | Loss::Region { .. } => {
                work.get_mut(i)
                    .ok_or(SimError::Config("cell index"))?
                    .push(*w);
            }
        }
    }
    let mut cells = Vec::with_capacity(n);
    for (at, list) in work.iter().enumerate() {
        cells.push(cell_run(&topo, cluster, config, at, list.clone())?);
    }
    let Loss::Region { region, at } = config.loss else {
        return Ok(FedRun {
            cells,
            moved: Vec::new(),
        });
    };
    let lost = usize::try_from(region).map_err(|_| SimError::Config("region index"))?;
    let when = at
        .checked_add(config.region_grace_ns)
        .ok_or(SimError::TimeOverflow)?;
    let mut free = Vec::with_capacity(n);
    for (i, c) in cells.iter().enumerate() {
        free.push(if topo.place(i)?.0 == lost {
            None
        } else {
            Some(snapshot(c, when, config)?)
        });
    }
    let mut moving = late;
    for (i, c) in cells.iter().enumerate() {
        if topo.place(i)?.0 == lost {
            moving.extend(unfinished(c, at)?);
        }
    }
    moving.sort_by_key(|w| w.id);
    let mut capacity = Capacity {
        topo: topo.try_clone()?,
        free,
        pick: config.cell.pick,
    };
    let mut moved = Vec::with_capacity(moving.len());
    for w in &moving {
        if let Some(Admitted { region, cell, .. }) = capacity.admit(w)? {
            let i = topo.index(
                usize::try_from(region).map_err(|_| SimError::Config("region index"))?,
                usize::try_from(cell).map_err(|_| SimError::Config("cell index"))?,
            )?;
            let mut w = *w;
            w.arrives = w.arrives.max(when);
            work.get_mut(i)
                .ok_or(SimError::Config("cell index"))?
                .push(w);
            moved.push((w.id, Moved::To { region, cell }));
        } else {
            moved.push((w.id, Moved::NoCapacity));
        }
    }
    for (i, slot) in cells.iter_mut().enumerate() {
        if topo.place(i)?.0 == lost {
            continue;
        }
        let mut list = work.get(i).ok_or(SimError::Config("cell index"))?.clone();
        // Moved work arrives at or after `when`, so a stable sort keeps the cell's own order.
        list.sort_by_key(|w| w.arrives);
        *slot = cell_run(&topo, cluster, config, i, list)?;
    }
    Ok(FedRun { cells, moved })
}

/// Runs cell `at` over its workloads (cluster ids, in arrival order), numbered densely within it,
/// with the whole cluster's owners, groups and overheads.
fn cell_run(
    topo: &Topo,
    whole: &Cluster,
    config: &FedConfig,
    at: usize,
    list: Vec<WorkloadSpec>,
) -> Result<CellRun, SimError> {
    let (r, c) = topo.place(at)?;
    let region = u32::try_from(r).map_err(|_| SimError::Config("region index"))?;
    let cell = u32::try_from(c).map_err(|_| SimError::Config("cell index"))?;
    let ids: Vec<WorkloadId> = list.iter().map(|w| w.id).collect();
    let mut workloads = list;
    for (k, w) in workloads.iter_mut().enumerate() {
        w.id = WorkloadId(u64::try_from(k).map_err(|_| SimError::Config("too many workloads"))?);
    }
    let cluster = Cluster {
        nodes: topo.nodes(at)?.to_vec(),
        workloads,
        models: whole.models.clone(),
        owners: whole.owners.clone(),
        groups: whole.groups.clone(),
        overheads: whole.overheads.clone(),
    };
    let lost_at = match config.loss {
        Loss::Cell {
            region: lr,
            cell: lc,
            at,
        } if (lr, lc) == (region, cell) => Some(at),
        Loss::Region { region: lr, at } if lr == region => Some(at),
        Loss::None | Loss::Cell { .. } | Loss::Region { .. } => None,
    };
    let cell_config = match lost_at {
        Some(at) => Config {
            nemesis: Nemesis::LoseCell { at },
            ..config.cell
        },
        None => config.cell,
    };
    let run = sim::run(&cluster, &cell_config)?;
    Ok(CellRun {
        region,
        cell,
        classes: topo.classes(at)?,
        cluster,
        ids,
        run,
    })
}

/// A surviving cell's free nodes at `when`, read from its store's log (what it has placed and not
/// seen done by then), less what waits in its queue then, each on the best fit the cell itself would
/// give it, in queue order ("assume and deduct", H66).
fn snapshot(c: &CellRun, when: Time, config: &FedConfig) -> Result<FitIndex, SimError> {
    let mut index = FitIndex::new(&c.cluster.nodes)?;
    let mut queued = Vec::new();
    let mut placed = vec![false; c.cluster.workloads.len()];
    for e in c.run.store.log().iter().take_while(|e| e.at <= when) {
        match e.op {
            Op::Submit { workload, .. } => queued.push(workload),
            Op::Place { workload, node } => {
                index.take(node, workload, &demand(c, workload)?)?;
                if let Some(p) = local(workload).and_then(|k| placed.get_mut(k)) {
                    *p = true;
                }
            }
            Op::Complete { workload, node } => {
                index.give(node, workload, &demand(c, workload)?)?;
            }
            // Taken off its node and pending again, at its old place in the queue (F4).
            Op::Preempt { workload, node } => {
                index.give(node, workload, &demand(c, workload)?)?;
                if let Some(p) = local(workload).and_then(|k| placed.get_mut(k)) {
                    *p = false;
                }
            }
        }
    }
    for workload in queued {
        if local(workload).and_then(|k| placed.get(k).copied()) == Some(true) {
            continue;
        }
        let d = demand(c, workload)?;
        if let Some(k) = index.fit(&d, &config.cell.pick).best {
            index.take(k.node, workload, &d)?;
        }
    }
    Ok(index)
}

/// A lost cell's workloads its store hadn't recorded done by `at`, by their ids in the cluster: a
/// lost region is a total loss (H66).
fn unfinished(c: &CellRun, at: Time) -> Result<Vec<WorkloadSpec>, SimError> {
    let mut done = vec![false; c.cluster.workloads.len()];
    for e in c.run.store.log().iter().take_while(|e| e.at <= at) {
        if let Op::Complete { workload, .. } = e.op
            && let Some(d) = local(workload).and_then(|k| done.get_mut(k))
        {
            *d = true;
        }
    }
    let mut out = Vec::new();
    for (k, w) in c.cluster.workloads.iter().enumerate() {
        if done.get(k).copied() == Some(false) {
            let id = *c.ids.get(k).ok_or(SimError::UnknownWorkload(w.id))?;
            out.push(WorkloadSpec { id, ..*w });
        }
    }
    Ok(out)
}

/// Where [`Capacity::admit`] put a workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    pub region: u32,
    pub cell: u32,
    /// The node within the cell.
    pub node: NodeId,
}

/// Each cell's free nodes as the federation sees them, a cell it can't place on held as none, and
/// the admission over them: H66's re-placement of a lost region's work and H68's selection.
#[derive(Debug)]
pub struct Capacity {
    topo: Topo,
    free: Vec<Option<FitIndex>>,
    pick: Pick,
}

impl Capacity {
    /// Every cell of the topology over `nodes`, empty.
    pub fn empty(nodes: &[NodeSpec], topology: Topology, pick: Pick) -> Result<Self, SimError> {
        let topo = Topo::new(nodes, topology)?;
        let n = topo.len()?;
        let mut free = Vec::with_capacity(n);
        for i in 0..n {
            free.push(Some(FitIndex::new(topo.nodes(i)?)?));
        }
        Ok(Self { topo, free, pick })
    }

    /// The first region from the workload's home (`id mod regions`) on, and in it the first cell
    /// from `(id / regions) mod cells` on, whose free nodes hold it by the cells' own sampled best
    /// fit; that node's capacity is deducted ("assume and deduct", H66). `None`: no node of any cell
    /// holds it.
    pub fn admit(&mut self, w: &WorkloadSpec) -> Result<Option<Admitted>, SimError> {
        let Topology { regions, cells } = self.topo.topology;
        let (hr, hc) = (home(w.id, regions)?, home(within(w.id, regions)?, cells)?);
        let d = w.demand();
        for k in 0..regions {
            let r = turn(hr, k, regions)?;
            for j in 0..cells {
                let c = turn(hc, j, cells)?;
                let i = self.topo.index(r, c)?;
                if !self.topo.classes(i)?.has(w.class) {
                    continue;
                }
                let Some(Some(index)) = self.free.get_mut(i) else {
                    continue;
                };
                if let Some(key) = index.fit(&d, &self.pick).best {
                    index.take(key.node, w.id, &d)?;
                    return Ok(Some(Admitted {
                        region: u32::try_from(r).map_err(|_| SimError::Config("region index"))?,
                        cell: u32::try_from(c).map_err(|_| SimError::Config("cell index"))?,
                        node: key.node,
                    }));
                }
            }
        }
        Ok(None)
    }
}

fn turn(from: usize, k: u32, of: u32) -> Result<usize, SimError> {
    let k = usize::try_from(k).map_err(|_| SimError::Config("topology"))?;
    let of = usize::try_from(of).map_err(|_| SimError::Config("topology"))?;
    from.checked_add(k)
        .and_then(|x| x.checked_rem(of))
        .ok_or(SimError::Config("topology"))
}

fn local(workload: WorkloadId) -> Option<usize> {
    usize::try_from(workload.0).ok()
}

fn demand(c: &CellRun, workload: WorkloadId) -> Result<Demand, SimError> {
    local(workload)
        .and_then(|k| c.cluster.workloads.get(k))
        .map(WorkloadSpec::demand)
        .ok_or(SimError::UnknownWorkload(workload))
}
