//! What each node can take, and the fast path's pick over it: the kernel's walk, ported from kiln.
//!
//! A node admits a workload when its hard filters pass (`scheduler-architecture.md` S3) and it has
//! the room: the image's platform the node's, GPUs and devices only where the node has them free and
//! the workload's class can be given them on the node's OS ([`Class::exposes`]), a benchmark never
//! beside a build nor a build beside a benchmark, and an agent session never on a node above 90% CPU
//! (`AC-T7.6.3`). Among the nodes that admit it, best fit is the node with the least free CPU, then
//! least free memory, then fewest free GPUs, then lowest id: one total order ([`Key`]), so the
//! scheduler and its serial specification can't disagree on a tie. Best fit is the baseline the
//! design scores against (Borg's, S1, CM[1]). GPU work goes first to the node whose GPUs are least
//! loaded, as kiln's pick goes to the runner with the most CPU free.

use crate::census;
use crate::error::SimError;
use crate::lb::{self, BUCKETS, Index};
use crate::model::{
    Class, Classes, Demand, Device, DeviceKind, GpuLoad, Held, Kind, NodeId, NodeSpec, Platform,
    Resources, RuntimePool, WorkloadId, bind_observed, fits_observed,
};
use crate::start_cost::{self, Costs, Prepared, Rates, StartObservation, Terms};

mod routes;
pub(crate) use routes::PhysicalSlot;
use routes::{Choice, Holder, Routes};

/// A session is refused a node whose CPU in use, background and placed, is above this share of
/// its CPU, per mille: `AC-T7.6.3`'s 90%.
pub const HOT_PER_MILLE: u64 = 900;

#[cfg(test)]
thread_local! {
    /// Mutation proving the log/filter oracle detects pool oversubscription independently.
    static IGNORE_RUNTIME_POOLS:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};
}

/// A node's place in the best-fit order: its free capacity, then its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
    pub gpus: u32,
    pub node: NodeId,
}

impl Key {
    fn of(node: NodeId, free: &Resources) -> Self {
        Self {
            cpu_milli: free.cpu_milli,
            mem_bytes: free.mem_bytes,
            gpus: free.gpus,
            node,
        }
    }
}

/// How a pick ranks the nodes that admit a demand: its scorer, and the workload shapes
/// [`Scorer::Fragment`] keeps usable. A pick reads every node with the room, from the least room that
/// could hold the need up (the kernel's walk, [`crate::lb::Index`]): no sample, so no sample size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pick {
    /// How the nodes that admit the demand are ranked.
    pub scorer: Scorer,
    /// The workload shapes [`Scorer::Fragment`] keeps usable.
    pub shapes: Shapes,
}

/// At most [`Shapes::MAX`] workload shapes, each with the class that asks it, kept in a fixed array
/// so a [`Pick`] stays a plain value. More is refused, typed: the shapes a pick keeps are
/// Pareto-minimal, none fitting wherever another does, so dropping one could leave a leftover only it
/// fits read as stranded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shapes {
    len: usize,
    items: [(Resources, Class); Shapes::MAX],
}

impl Shapes {
    /// The input-bound rule ([`crate::census`]): twice the most minimal shapes a committed trace's
    /// workload has.
    pub const MAX: usize = census::SHAPES * census::INPUT_MARGIN;

    /// `shapes`, each once, in order (by CPU, then memory, then GPUs, then class); more than
    /// [`Shapes::MAX`] distinct ones refused.
    pub fn of(mut shapes: Vec<(Resources, Class)>) -> Result<Self, SimError> {
        shapes.sort_unstable();
        shapes.dedup();
        if shapes.len() > Self::MAX {
            return Err(SimError::Full {
                what: "a sample's workload shapes",
                bound: Self::MAX,
            });
        }
        let mut out = Self::default();
        for (slot, shape) in out.items.iter_mut().zip(shapes) {
            *slot = shape;
            out.len = out.len.saturating_add(1);
        }
        Ok(out)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Resources, Class)> {
        self.items.iter().take(self.len)
    }

    /// Whether some shape of a class `classes` holds fits `free`.
    pub fn usable(&self, free: &Resources, classes: Classes) -> bool {
        self.iter()
            .any(|(shape, class)| classes.has(*class) && free.holds(shape))
    }
}

/// How a pick ranks the nodes that admit a demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scorer {
    /// The least free capacity first ([`Key`]'s order): best fit, the baseline (Borg's, S1).
    #[default]
    BestFit,
    /// Best fit across resources: the node most allocated once the need is placed, CPU and memory
    /// each as a share of the node's and weighted alike, as Kubernetes' `MostAllocated` bin-packing
    /// score is; then [`Key`]'s order. [`Scorer::BestFit`] ranks by free CPU first, which on a
    /// memory-bound workload stops packing the resource that binds.
    MostAllocated,
    /// Tetris's alignment (Grandl et al., SIGCOMM 2014): the largest dot product of the need and the
    /// node's free capacity, each resource taken as a share of the node's capacity, ties by
    /// [`Key`]'s order.
    Align,
    /// Least stranded: the node whose leftover after placing the need is either nothing or still
    /// fits one of the pick's [`Shapes`], so no CPU or memory is left that no workload can use;
    /// then the [`Tie`]; then [`Key`]'s order. [`Scorer::HEX`] is Hex's packing score.
    Fragment(Tie),
}

impl Scorer {
    /// Hex's packing score: least stranded, then the node that runs the fewest classes, then the most
    /// allocated, so a node only some classes can use is spent before one that every class can, and
    /// what isn't stranded packs. On the recorded workload of OCI containers, Firecracker microVMs and
    /// Shards microVMs it strands at least 30% less than either best fit at 90% load on every seed,
    /// for the mix and for each class alone, and passes over fewer of them (hex-sim.md H11).
    pub const HEX: Self = Self::Fragment(Tie::Narrowest);
}

/// What ranks the nodes [`Scorer::Fragment`] finds equally stranded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tie {
    /// Tetris's alignment ([`Scorer::Align`]): the plan's alignment score with a fragmentation term.
    Align,
    /// The node least allocated once the need is placed (Kubernetes' `LeastAllocated`).
    LeastAllocated,
    /// The node most allocated once the need is placed (Kubernetes' `MostAllocated`).
    MostAllocated,
    /// The node that runs the fewest classes, then the most allocated: a node every class can use is
    /// kept for the classes only it can take.
    Narrowest,
}

/// `part` as a share of `whole` in millionths; 0 of a whole of 0, and past u64 saturates.
fn millionths(part: u64, whole: u64) -> u64 {
    u128::from(part)
        .checked_mul(1_000_000)
        .and_then(|x| x.checked_div(u128::from(whole)))
        .map_or(0, |x| u64::try_from(x).unwrap_or(u64::MAX))
}

/// Nodes a scan passed over because a filter refused them though they had the room, by filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Refusals {
    /// GPU work, a node without the GPUs it asks, or of a class that can't be given them there.
    pub gpu: u64,
    /// An image built for another OS or architecture.
    pub platform: u64,
    /// Devices or KVM the node hasn't, or that the workload's class can't be given there.
    pub device: u64,
    /// A benchmark and a build, one already running there.
    pub shares: u64,
    /// A session, a node above 90% CPU.
    pub hot: u64,
    /// A workload of a class the node can't run.
    pub class: u64,
}

/// A pick: the best node that admits the demand, the nodes its walk read, and what the filters
/// refused among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub best: Option<Key>,
    /// Nodes the walk read: every one with the CPU for the need.
    pub walked: u64,
    pub refused: Refusals,
}

/// Why a node doesn't admit a demand, or that it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Admits,
    NoClass,
    /// The image is built for another OS or architecture.
    NoPlatform,
    /// The need asks GPUs the node hasn't, of the model and with the memory free asked, or that its
    /// class can't be given on the node's OS ([`Class::exposes`]).
    NoGpu,
    /// The ask names devices or KVM the node hasn't, or that its class can't be given there.
    NoDevice,
    NoRoom,
    Shares,
    Hot,
}

#[derive(Debug)]
struct Slot {
    capacity: Resources,
    free: Resources,
    background_milli: u64,
    classes: Classes,
    platform: Platform,
    kvm: bool,
    builds: u32,
    benchmarks: u32,
    used_by_class: [Resources; Class::COUNT],
    runtime_pools: [Option<RuntimePool>; Class::COUNT],
    costs: [Option<hex_api::RuntimeCost>; Class::COUNT],
    /// What a new VM of each class is booked at here (joint plan step 14, C2).
    priors: [Option<crate::model::Prior>; Class::COUNT],
    routes: Option<Routes>,
}

fn pending_gpus(
    slot: &Slot,
    physical: &[PhysicalSlot],
    holders: &[Option<Holder>],
    node: NodeId,
) -> Result<u32, SimError> {
    let bound = physical
        .iter()
        .zip(holders)
        .filter(|(physical, holder)| {
            holder.is_some()
                && physical
                    .observed
                    .as_ref()
                    .map(|device| device.kind == DeviceKind::Gpu)
                    .unwrap_or_else(|| {
                        physical
                            .id
                            .is_some_and(|id| id.kind == hex_api::DeviceKind::Gpu)
                    })
        })
        .count();
    slot.capacity
        .gpus
        .checked_sub(slot.free.gpus)
        .and_then(|reserved| reserved.checked_sub(u32::try_from(bound).ok()?))
        .ok_or(SimError::Resources(node))
}

/// A GPU's load as one share, in millionths: the larger of its utilisation and its memory in use, as
/// DRF takes a dominant share (Ghodsi et al., NSDI 2011).
fn gpu_busy(d: &Device) -> Option<u64> {
    Some(
        millionths(d.load.memory_used?, d.size)
            .max(u64::from(d.load.busy_per_mille?).saturating_mul(1000)),
    )
}

/// How idle the GPUs `held` names are: the sum of each one's share free of its load, in millionths.
fn gpu_idle(devices: &[PhysicalSlot], held: &[u16]) -> u64 {
    held.iter()
        .filter_map(|&i| {
            devices
                .get(usize::from(i))
                .and_then(|slot| slot.observed.as_ref())
        })
        .filter(|d| d.kind == DeviceKind::Gpu)
        // An unread GPU earns no idle credit. Fresh matching already requires both readings;
        // surviving bindings still retain their exact identity during telemetry loss.
        .filter_map(gpu_busy)
        .map(|busy| 1_000_000u64.saturating_sub(busy))
        .fold(0, u64::saturating_add)
}

impl Slot {
    fn choice(&self, demand: &Demand) -> Result<Option<Choice>, Verdict> {
        self.routes
            .as_ref()
            .map(|routes| routes.chosen(demand))
            .transpose()
    }

    /// The receipt a VM placed here would take, booked at this node's prior for its class (C2).
    fn allocation(&self, demand: &Demand) -> Result<Option<hex_api::Allocation>, SimError> {
        let prior = self.priors.get(demand.class.index()).copied().flatten();
        if let Some(routes) = &self.routes {
            let choice = routes
                .chosen(demand)
                .map_err(|_| SimError::Config("no unambiguous initialized route"))?;
            return routes
                .allocation(choice, demand)
                .map(|allocation| crate::model::booked_at(allocation, prior))
                .map_err(|_| SimError::Config("invalid initialized route charge"));
        }
        self.costs
            .get(demand.class.index())
            .copied()
            .flatten()
            .map(|cost| cost.allocate(crate::model::capacity_resources(demand.need)))
            .transpose()
            .map(|allocation| crate::model::booked_at(allocation, prior))
            .map_err(|_| SimError::Config("invalid runtime charge"))
    }

    fn charged(&self, demand: &Demand) -> Option<Resources> {
        let allocation = self.allocation(demand).ok()?;
        if let Some(routes) = &self.routes {
            return routes
                .charged(routes.chosen(demand).ok()?, demand, allocation)
                .ok();
        }
        crate::model::allocated_resources(demand.need, allocation).ok()
    }

    fn pool_used(&self, classes: Classes) -> Option<Resources> {
        let mut used = Resources::default();
        for class in Class::ALL {
            if classes.has(class) {
                used = used.checked_add(self.used_by_class.get(class.index())?)?;
            }
        }
        Some(used)
    }

    fn runtime_holds(&self, demand: &Demand, need: &Resources) -> bool {
        self.runtime_holds_routed(demand, need, None)
    }

    fn runtime_holds_routed(
        &self,
        demand: &Demand,
        need: &Resources,
        receipt: Option<hex_api::RouteReceipt>,
    ) -> bool {
        let class = demand.class;
        if let Some(routes) = &self.routes {
            if let Some(receipt) = receipt {
                return routes
                    .committed(receipt)
                    .is_ok_and(|choice| routes.holds_inner(choice, demand));
            }
            return routes
                .chosen(demand)
                .is_ok_and(|choice| routes.holds_inner(choice, demand));
        }
        #[cfg(test)]
        if IGNORE_RUNTIME_POOLS.get() {
            return true;
        }
        self.runtime_pools
            .iter()
            .flatten()
            .find(|pool| pool.classes.has(class))
            .is_none_or(|pool| {
                self.pool_used(pool.classes)
                    .and_then(|used| pool.capacity.checked_sub(&used))
                    .is_some_and(|free| free.holds(need))
            })
    }

    /// A durable local or remote owner restores its original tuple, not a fresh candidate.
    fn runtime_holds_committed(
        &self,
        demand: &Demand,
        need: &Resources,
        allocation: Option<hex_api::Allocation>,
        receipt: Option<hex_api::RouteReceipt>,
    ) -> bool {
        if let (Some(routes), Some(receipt)) = (&self.routes, receipt) {
            return routes
                .committed(receipt)
                .is_ok_and(|choice| routes.holds_recovered_inner(choice, demand, allocation));
        }
        self.runtime_holds_routed(demand, need, receipt)
    }
    /// The verdict on `demand` here, `devices` the node's and `free(i)` whether device `i` is free.
    fn verdict(
        &self,
        demand: &Demand,
        devices: &[PhysicalSlot],
        pending_gpus: u32,
        free: impl Fn(usize) -> bool,
    ) -> Verdict {
        let (ask, class, os) = (&demand.ask, demand.class, self.platform.os);
        if !self.classes.has(class) {
            return Verdict::NoClass;
        }
        let choice = match self.choice(demand) {
            Ok(choice) => choice,
            Err(why) => return why,
        };
        let Some(need) = self.charged(demand) else {
            return Verdict::NoRoom;
        };
        // Process asks retain host meaning; an actual routed OCI/VM offer names its image.
        let image_os = match class {
            Class::Oci => os,
            Class::Shards | Class::Firecracker => crate::model::Os::Linux,
        };
        if choice.is_none()
            && (ask.os.is_some_and(|o| o != image_os)
                || ask.arch.is_some_and(|a| a != self.platform.arch))
        {
            return Verdict::NoPlatform;
        }
        let exposed_class = match choice.map(|choice| choice.offer.key.backend_class()) {
            Some(hex_api::Class::Shards) => Class::Shards,
            Some(hex_api::Class::Firecracker) => Class::Firecracker,
            _ => class,
        };
        let gpus_only = crate::model::Ask {
            vfio: 0,
            block: 0,
            net: 0,
            kvm: false,
            ..*ask
        };
        let exposed_device = |index: usize| {
            devices.get(index).is_some_and(|slot| {
                slot.observed
                    .as_ref()
                    .is_some_and(|device| device.classes.has(class))
                    && choice.is_none_or(|choice| {
                        u32::try_from(choice.at)
                            .ok()
                            .and_then(|at| 1u64.checked_shl(at))
                            .is_some_and(|bit| slot.routes & bit != 0)
                    })
                    && choice
                        .and_then(|choice| choice.offer.backing)
                        .is_none_or(|key| {
                            self.routes
                                .as_ref()
                                .and_then(|routes| routes.backing(key))
                                .is_some_and(|(backing, _)| {
                                    slot.id.is_some_and(|id| {
                                        backing.devices.binary_search(&id).is_ok()
                                    })
                                })
                        })
            })
        };
        let observed = |index: usize| devices.get(index).and_then(|slot| slot.observed.as_ref());
        if let Some(key) = choice.and_then(|choice| choice.offer.backing)
            && choice.is_some_and(|choice| {
                choice.offer.key.execution != hex_api::Execution::ShardsShared
            })
        {
            let selected = usize::try_from(demand.need.gpus)
                .ok()
                .and_then(|n| n.checked_add(usize::from(ask.vfio)))
                .and_then(|n| n.checked_add(usize::from(ask.block)))
                .and_then(|n| n.checked_add(usize::from(ask.net)));
            if self
                .routes
                .as_ref()
                .and_then(|routes| routes.backing(key))
                .is_none_or(|(backing, _)| {
                    selected.is_none_or(|count| count == 0 || count > backing.devices.len())
                })
            {
                return Verdict::NoGpu;
            }
        }
        if demand.need.gpus > 0
            && (!exposed_class.exposes(os, DeviceKind::Gpu)
                || !fits_observed(
                    devices.len(),
                    observed,
                    exposed_device,
                    demand.need.gpus,
                    &gpus_only,
                ))
        {
            return Verdict::NoGpu;
        }
        if ask.devices() {
            let exposed = [
                (ask.vfio, DeviceKind::Vfio),
                (ask.block, DeviceKind::Block),
                (ask.net, DeviceKind::Net),
            ]
            .iter()
            .all(|&(n, kind)| n == 0 || exposed_class.exposes(os, kind));
            let kvm = !ask.kvm
                || (self.kvm
                    && choice.is_none_or(|choice| {
                        choice.offer.key.execution == hex_api::Execution::Native
                    })
                    && class.gives_kvm(os));
            if !exposed || !kvm || !fits_observed(devices.len(), observed, exposed_device, 0, ask) {
                return Verdict::NoDevice;
            }
        }
        if !self.free.holds(&need) || !self.runtime_holds(demand, &need) {
            return Verdict::NoRoom;
        }
        if demand.need.gpus > 0 && pending_gpus > 0 {
            let Some(required) = demand.need.gpus.checked_add(pending_gpus) else {
                return Verdict::NoRoom;
            };
            // Pending owner rows have no proven receiver binding yet. They still consume eligible
            // room; foreign/unread cards cannot sponsor an additional fresh GPU reservation.
            if !fits_observed(
                devices.len(),
                observed,
                |i| exposed_device(i) && free(i),
                required,
                &gpus_only,
            ) {
                return Verdict::NoRoom;
            }
        }
        if (demand.need.gpus > 0 || ask.devices())
            && !fits_observed(
                devices.len(),
                observed,
                |i| exposed_device(i) && free(i),
                demand.need.gpus,
                ask,
            )
        {
            return Verdict::NoRoom;
        }
        match demand.kind {
            Kind::Build if self.benchmarks > 0 => Verdict::Shares,
            Kind::Benchmark if self.builds > 0 => Verdict::Shares,
            Kind::Session if self.hot() => Verdict::Hot,
            Kind::Plain | Kind::Build | Kind::Benchmark | Kind::Session => Verdict::Admits,
        }
    }

    /// Tetris's alignment of `need` with this node's free capacity: the sum over CPU, memory and
    /// GPUs of (need / capacity) * (free / capacity), each share in millionths, so each term is at
    /// most 10^12 and the sum fits u64. A resource the node has none of adds nothing.
    fn alignment(&self, need: &Resources) -> u64 {
        let term =
            |n: u64, f: u64, c: u64| millionths(n, c).checked_mul(millionths(f, c)).unwrap_or(0);
        let (c, f) = (&self.capacity, &self.free);
        [
            term(need.cpu_milli, f.cpu_milli, c.cpu_milli),
            term(need.mem_bytes, f.mem_bytes, c.mem_bytes),
            term(u64::from(need.gpus), u64::from(f.gpus), u64::from(c.gpus)),
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }

    /// What placing `need` here would leave that no shape can use: when the leftover fits no shape,
    /// its CPU and its memory, each as a share of the node's in millionths, summed; else nothing.
    fn stranded_after(&self, need: &Resources, shapes: &Shapes) -> u64 {
        let Some(left) = self.free.checked_sub(need) else {
            return u64::MAX;
        };
        if shapes.usable(&left, self.classes) {
            0
        } else {
            let c = &self.capacity;
            millionths(left.cpu_milli, c.cpu_milli)
                .saturating_add(millionths(left.mem_bytes, c.mem_bytes))
        }
    }

    /// The node's CPU and memory allocated once `need` is placed, each as a share of the node's in
    /// millionths, summed: Kubernetes' `MostAllocated` score with equal weights, and the inverse of
    /// its `LeastAllocated`.
    fn allocated_after(&self, need: &Resources) -> u64 {
        let (c, f) = (&self.capacity, &self.free);
        let used = |cap: u64, free: u64, more: u64| cap.saturating_sub(free).saturating_add(more);
        millionths(used(c.cpu_milli, f.cpu_milli, need.cpu_milli), c.cpu_milli).saturating_add(
            millionths(used(c.mem_bytes, f.mem_bytes, need.mem_bytes), c.mem_bytes),
        )
    }

    /// What `pick`'s scorer gives placing `need` here: higher is better.
    fn score(&self, need: &Resources, pick: &Pick) -> u128 {
        match pick.scorer {
            Scorer::BestFit => 0,
            Scorer::MostAllocated => u128::from(self.allocated_after(need)),
            Scorer::Align => u128::from(self.alignment(need)),
            Scorer::Fragment(tie) => {
                // Lexicographic in one number: least stranded capacity in the high word, then the tie
                // in the low word.
                let stranded = self.stranded_after(need, &pick.shapes);
                let then = match tie {
                    Tie::Align => self.alignment(need),
                    Tie::LeastAllocated => u64::MAX.saturating_sub(self.allocated_after(need)),
                    Tie::MostAllocated => self.allocated_after(need),
                    // Fewer classes in the high bits; the allocation (at most 2,000,000) below.
                    Tie::Narrowest => {
                        u64::from(Classes::ALL.count().saturating_sub(self.classes.count()))
                            .checked_shl(32)
                            .unwrap_or(0)
                            | self.allocated_after(need)
                    }
                };
                u128::from(u64::MAX.saturating_sub(stranded))
                    .checked_mul(1 << 64)
                    .unwrap_or(0)
                    | u128::from(then)
            }
        }
    }

    /// CPU in use, background and placed, above 90% of the node's CPU. Compared as
    /// busy * 1000 > capacity * 900 in u128, where neither side can overflow; a busy count past u64
    /// is hot.
    fn hot(&self) -> bool {
        let busy = self
            .capacity
            .cpu_milli
            .checked_sub(self.free.cpu_milli)
            .and_then(|placed| placed.checked_add(self.background_milli));
        let Some(busy) = busy else {
            return true;
        };
        let lhs = u128::from(busy).checked_mul(1000);
        let rhs = u128::from(self.capacity.cpu_milli).checked_mul(u128::from(HOT_PER_MILLE));
        matches!((lhs, rhs), (Some(l), Some(r)) if l > r)
    }
}

/// The nodes' room and filter state, and the kernel's index of them by free whole CPUs
/// ([`crate::lb::Index`], kiln's `lb/index.rs`): a pick walks it from the need's bucket up, so it
/// reads only nodes with the CPU for the need, every one of them.
#[derive(Debug)]
pub struct FitIndex {
    costs: Costs,
    /// By node id; ids are dense from 0.
    slots: Vec<Slot>,
    /// Each node's devices, by node id.
    devices: Vec<Vec<PhysicalSlot>>,
    /// The workload holding each of a node's devices, until it is deleted.
    held: Vec<Vec<Option<Holder>>>,
    ix: Index,
    /// The highest bucket: the most whole CPUs any node has.
    last: u32,
}

/// A node's bucket in the kernel's index: its free whole CPUs, at most `last`.
fn bucket(free: &Resources, last: u32) -> u32 {
    let whole = free.cpu_milli.checked_div(lb::CPU).unwrap_or(0);
    u32::try_from(whole).unwrap_or(u32::MAX).min(last)
}

fn bucket_slot(slot: &Slot, last: u32) -> u32 {
    let mut upper = slot.free;
    upper.cpu_milli = upper
        .cpu_milli
        .max(slot.routes.as_ref().map_or(0, Routes::rank_cpu));
    bucket(&upper, last)
}

pub(crate) struct PreparedRefresh {
    node: NodeId,
    slot: Slot,
    devices: Vec<PhysicalSlot>,
    holders: Vec<Option<Holder>>,
}

/// A node's capacity as the kernel's amounts: CPU, memory, disk, GPUs.
fn amounts(r: &Resources) -> lb::Amounts {
    [r.cpu_milli, r.mem_bytes, r.disk_bytes, u64::from(r.gpus)]
}

fn runtime_pools(node: &NodeSpec) -> Result<[Option<RuntimePool>; Class::COUNT], SimError> {
    node.check_runtime_pools()?;
    let mut pools = [None; Class::COUNT];
    for (slot, pool) in pools.iter_mut().zip(&node.runtime_pools) {
        *slot = Some(*pool);
    }
    Ok(pools)
}

fn check_device_keys(node: &NodeSpec) -> Result<(), SimError> {
    let mut keys = Vec::new();
    keys.try_reserve_exact(node.devices.len())
        .map_err(|_| SimError::Resources(node.id))?;
    keys.extend(
        node.devices
            .iter()
            .filter_map(|device| device.key.map(|key| (device.kind, key))),
    );
    keys.sort_unstable();
    if keys.windows(2).any(|pair| matches!(pair, [a, b] if a == b)) {
        return Err(SimError::Config("duplicate physical device identity"));
    }
    Ok(())
}

impl FitIndex {
    pub fn try_clone(&self) -> Result<Self, SimError> {
        let mut slots = Vec::new();
        let mut devices = Vec::new();
        let mut held = Vec::new();
        slots
            .try_reserve_exact(self.len())
            .map_err(|_| SimError::Full {
                what: "index snapshot nodes",
                bound: self.len(),
            })?;
        devices
            .try_reserve_exact(self.len())
            .map_err(|_| SimError::Full {
                what: "index snapshot devices",
                bound: self.len(),
            })?;
        held.try_reserve_exact(self.len())
            .map_err(|_| SimError::Full {
                what: "index snapshot holders",
                bound: self.len(),
            })?;
        for (at, slot) in self.slots.iter().enumerate() {
            let node =
                NodeId(u32::try_from(at).map_err(|_| SimError::Config("node index overflows"))?);
            slots.push(Slot {
                capacity: slot.capacity,
                free: slot.free,
                background_milli: slot.background_milli,
                classes: slot.classes,
                platform: slot.platform,
                kvm: slot.kvm,
                builds: slot.builds,
                benchmarks: slot.benchmarks,
                used_by_class: slot.used_by_class,
                runtime_pools: slot.runtime_pools,
                costs: slot.costs,
                priors: slot.priors,
                routes: slot
                    .routes
                    .as_ref()
                    .map(|routes| routes.try_clone(node))
                    .transpose()?,
            });
            devices.push(routes::copy(
                self.devices.get(at).ok_or(SimError::UnknownNode(node))?,
                node,
            )?);
            held.push(routes::copy(
                self.held.get(at).ok_or(SimError::UnknownNode(node))?,
                node,
            )?);
        }
        Ok(Self {
            costs: self.costs.try_clone()?,
            slots,
            devices,
            held,
            ix: self.ix.try_clone().ok_or(SimError::Full {
                what: "bucket index snapshot",
                bound: self.len(),
            })?,
            last: self.last,
        })
    }

    /// Nodes' ids must be dense from 0, in order.
    pub fn new(nodes: &[NodeSpec]) -> Result<Self, SimError> {
        let mut slots = Vec::new();
        let (mut devices, mut held) = (Vec::new(), Vec::new());
        devices
            .try_reserve_exact(nodes.len())
            .map_err(|_| SimError::Full {
                what: "physical index nodes",
                bound: nodes.len(),
            })?;
        held.try_reserve_exact(nodes.len())
            .map_err(|_| SimError::Full {
                what: "physical holder nodes",
                bound: nodes.len(),
            })?;
        slots
            .try_reserve_exact(nodes.len())
            .map_err(|_| SimError::Full {
                what: "placement index nodes",
                bound: nodes.len(),
            })?;
        for (at, node) in nodes.iter().enumerate() {
            if usize::try_from(node.id.0).ok() != Some(at) {
                return Err(SimError::Config("node ids must be dense from 0, in order"));
            }
            if !node.devices_agree() {
                return Err(SimError::Config(
                    "a node's GPUs listed aren't its capacity's, or it lists more devices than a PCI segment has",
                ));
            }
            check_device_keys(node)?;
            let pools = runtime_pools(node)?;
            let routes = node
                .routes
                .as_ref()
                .map(|catalog| Routes::new(catalog, node.id))
                .transpose()?;
            let physical = routes::physical(node)?;
            let mut holders = Vec::new();
            holders
                .try_reserve_exact(physical.len())
                .map_err(|_| SimError::Resources(node.id))?;
            holders.resize(physical.len(), None);
            let mut free = node.capacity;
            if let Some(routes) = &routes {
                routes::restore_backings(node.id, routes, &physical, &mut holders, &mut free)?;
            }
            free.gpus = u32::try_from(
                physical
                    .iter()
                    .zip(&holders)
                    .filter(|(slot, holder)| {
                        holder.is_none()
                            && slot
                                .observed
                                .as_ref()
                                .map(|device| device.kind == DeviceKind::Gpu)
                                .unwrap_or_else(|| {
                                    slot.id
                                        .is_some_and(|id| id.kind == hex_api::DeviceKind::Gpu)
                                })
                    })
                    .count(),
            )
            .map_err(|_| SimError::Resources(node.id))?;
            slots.push(Slot {
                capacity: node.capacity,
                free,
                background_milli: node.background_milli,
                classes: node.classes,
                platform: node.platform,
                kvm: node.virt.kvm,
                builds: 0,
                benchmarks: 0,
                used_by_class: [Resources::default(); Class::COUNT],
                runtime_pools: pools,
                costs: node.costs,
                priors: node.priors,
                routes,
            });
            devices.push(physical);
            held.push(holders);
        }
        let whole = nodes
            .iter()
            .map(|n| n.capacity.cpu_milli.checked_div(lb::CPU).unwrap_or(0))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let buckets = u32::try_from(whole).unwrap_or(BUCKETS).min(BUCKETS);
        let last = buckets.saturating_sub(1);
        let count = u32::try_from(nodes.len()).map_err(|_| SimError::Full {
            what: "the index's nodes",
            bound: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
        })?;
        let mut ix = Index::new(0);
        if !ix.grow(buckets, count) {
            return Err(SimError::Config("the index can't grow to the cluster"));
        }
        for (slot, id) in slots.iter().zip(0u32..) {
            if !ix.join(id, &amounts(&slot.capacity)) {
                return Err(SimError::Config("the index can't hold the cluster"));
            }
            ix.push(id, bucket_slot(slot, last));
        }
        Ok(Self {
            costs: Costs::new(nodes.len()),
            slots,
            devices,
            held,
            ix,
            last,
        })
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn slot(&self, node: NodeId) -> Result<&Slot, SimError> {
        usize::try_from(node.0)
            .ok()
            .and_then(|at| self.slots.get(at))
            .ok_or(SimError::UnknownNode(node))
    }

    /// What `node`'s admissions hold of its capacity, workloads and backings alike: its capacity less
    /// its free room (joint plan step 13: the host's own held in the capacity view).
    pub fn held(&self, node: NodeId) -> Result<Resources, SimError> {
        let slot = self.slot(node)?;
        slot.capacity
            .checked_sub(&slot.free)
            .ok_or(SimError::Resources(node))
    }

    pub fn key(&self, node: NodeId) -> Result<Key, SimError> {
        Ok(Key::of(node, &self.slot(node)?.free))
    }

    pub fn verdict(&self, node: NodeId, demand: &Demand) -> Result<Verdict, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        Ok(self.judge(at, self.slot(node)?, demand))
    }

    /// The candidate's initialized immutable cost receipt, before owner commit or receiver effects.
    pub fn allocation(
        &self,
        node: NodeId,
        demand: &Demand,
    ) -> Result<Option<hex_api::Allocation>, SimError> {
        self.slot(node)?
            .allocation(demand)
            .map_err(|_| SimError::Resources(node))
    }

    pub fn route(
        &self,
        node: NodeId,
        demand: &Demand,
    ) -> Result<Option<hex_api::RouteReceipt>, SimError> {
        self.slot(node)?
            .choice(demand)
            .map(|choice| choice.map(Choice::receipt))
            .map_err(|_| SimError::Config("no unambiguous initialized route"))
    }

    fn physical_free(holder: Option<Holder>, backing: Option<hex_api::BackingKey>) -> bool {
        match holder {
            None => true,
            Some(Holder::Backing(key)) => backing == Some(key),
            Some(Holder::Box(_) | Holder::Handoff { .. }) => false,
        }
    }

    /// The verdict on `demand` at node `at`, its slot `slot`.
    fn judge(&self, at: usize, slot: &Slot, demand: &Demand) -> Verdict {
        let (devices, held) = (
            self.devices.get(at).map_or(&[][..], Vec::as_slice),
            self.held.get(at).map_or(&[][..], Vec::as_slice),
        );
        let backing = slot
            .choice(demand)
            .ok()
            .flatten()
            .and_then(|choice| choice.offer.backing);
        let pending = if backing.is_some() {
            0
        } else {
            let Ok(node) = u32::try_from(at) else {
                return Verdict::NoRoom;
            };
            let Ok(pending) = pending_gpus(slot, devices, held, NodeId(node)) else {
                return Verdict::NoRoom;
            };
            pending
        };
        slot.verdict(demand, devices, pending, |i| {
            held.get(i)
                .is_some_and(|holder| Self::physical_free(*holder, backing))
                && devices
                    .get(i)
                    .and_then(|slot| slot.observed.as_ref())
                    .is_some_and(|d| d.available)
        })
    }

    /// The devices `demand` would hold on node `at` now, through the sole nullable-observation picker.
    fn bound(&self, at: usize, demand: &Demand) -> Option<Held> {
        let devices = self.devices.get(at)?;
        let held = self.held.get(at)?;
        let slot = self.slots.get(at)?;
        let choice = slot.choice(demand).ok()?;
        let backing = choice.and_then(|choice| choice.offer.backing);
        let bit = choice.and_then(|choice| {
            u32::try_from(choice.at)
                .ok()
                .and_then(|at| 1u64.checked_shl(at))
        });
        bind_observed(
            devices.len(),
            |i| devices.get(i).and_then(|slot| slot.observed.as_ref()),
            |i| {
                held.get(i)
                    .is_some_and(|holder| Self::physical_free(*holder, backing))
                    && devices.get(i).is_some_and(|physical| {
                        physical.observed.as_ref().is_some_and(|device| {
                            device.available && device.classes.has(demand.class)
                        }) && bit.is_none_or(|bit| physical.routes & bit != 0)
                            && backing.is_none_or(|key| {
                                slot.routes
                                    .as_ref()
                                    .and_then(|routes| routes.backing(key))
                                    .is_some_and(|(backing, _)| {
                                        physical.id.is_some_and(|id| {
                                            backing.devices.binary_search(&id).is_ok()
                                        })
                                    })
                            })
                    })
            },
            demand.need.gpus,
            &demand.ask,
        )
    }

    /// The devices `workload` holds on `node`, including its one in-progress backing handoff.
    pub fn holds(&self, node: NodeId, workload: WorkloadId) -> Held {
        let at = usize::try_from(node.0).unwrap_or(usize::MAX);
        self.held
            .get(at)
            .into_iter()
            .flatten()
            .zip(0u16..=u16::MAX)
            .filter(|(holder, _)| holder.and_then(Holder::workload) == Some(workload))
            .map(|(_, i)| i)
            .collect()
    }

    pub(crate) fn devices(&self, node: NodeId) -> Result<&[PhysicalSlot], SimError> {
        usize::try_from(node.0)
            .ok()
            .and_then(|at| self.devices.get(at))
            .map(Vec::as_slice)
            .ok_or(SimError::UnknownNode(node))
    }

    pub(crate) fn held_devices(
        &self,
        node: NodeId,
        workload: WorkloadId,
    ) -> Result<Vec<Device>, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let holders = self.held.get(at).ok_or(SimError::UnknownNode(node))?;
        let devices = self.devices(node)?;
        let count = holders
            .iter()
            .filter(|holder| holder.and_then(Holder::workload) == Some(workload))
            .count();
        let mut selected = Vec::new();
        selected
            .try_reserve_exact(count)
            .map_err(|_| SimError::Resources(node))?;
        for (holder, physical) in holders.iter().zip(devices) {
            if holder.and_then(Holder::workload) == Some(workload) {
                selected.push(physical.observed.ok_or(SimError::Config(
                    "held workload has no retained physical observation",
                ))?);
            }
        }
        Ok(selected)
    }

    /// Prepare both peer views before committing either, so allocation/refusal cannot publish a
    /// partial refresh. Stable physical IDs remap slots; retained holds are never repicked.
    pub(crate) fn prepare_refresh_node(
        &self,
        node: &NodeSpec,
    ) -> Result<PreparedRefresh, SimError> {
        let at = usize::try_from(node.id.0).map_err(|_| SimError::UnknownNode(node.id))?;
        let old = self.slot(node.id)?;
        let pools = runtime_pools(node)?;
        if !node.devices_agree() {
            return Err(SimError::Config("refreshed devices disagree with capacity"));
        }
        check_device_keys(node)?;
        let routes = match (&old.routes, &node.routes) {
            (Some(old), Some(new)) => Some(old.refreshed(new, node.id)?),
            (None, Some(new))
                if old
                    .used_by_class
                    .iter()
                    .all(|used| *used == Resources::default()) =>
            {
                Some(Routes::new(new, node.id)?)
            }
            (Some(old), None)
                if old.used.iter().all(|used| *used == Resources::default())
                    && old.phases.iter().all(|phase| {
                        matches!(
                            phase,
                            hex_api::BackingPhase::Prepared | hex_api::BackingPhase::Released
                        )
                    }) =>
            {
                None
            }
            (None, None) => None,
            _ => {
                return Err(SimError::Config(
                    "refreshed facts remove or reinterpret retained route grants",
                ));
            }
        };
        for class in Class::ALL {
            let used = old
                .used_by_class
                .get(class.index())
                .ok_or(SimError::Resources(node.id))?;
            if *used != Resources::default()
                && old
                    .runtime_pools
                    .iter()
                    .flatten()
                    .any(|pool| pool.classes.has(class))
                && !pools.iter().flatten().any(|pool| pool.classes.has(class))
                && routes.is_none()
            {
                return Err(SimError::Config(
                    "refreshed facts remove a held runtime pool",
                ));
            }
        }
        for pool in pools.iter().flatten().filter(|_| routes.is_none()) {
            if !pool.capacity.holds(
                &old.pool_used(pool.classes)
                    .ok_or(SimError::Resources(node.id))?,
            ) {
                return Err(SimError::Resources(node.id));
            }
        }
        let old_physical = self.devices.get(at).ok_or(SimError::UnknownNode(node.id))?;
        let old_holders = self.held.get(at).ok_or(SimError::UnknownNode(node.id))?;
        let pending_gpus = pending_gpus(old, old_physical, old_holders, node.id)?;
        let mut devices = routes::physical(node)?;
        let mut holders = Vec::new();
        holders
            .try_reserve_exact(devices.len())
            .map_err(|_| SimError::Resources(node.id))?;
        holders.resize(devices.len(), None);
        let mut backing_free = node.capacity;
        if let Some(routes) = &routes {
            routes::restore_backings(node.id, routes, &devices, &mut holders, &mut backing_free)?;
        }
        let mut workload_used = old
            .capacity
            .checked_sub(&old.free)
            .ok_or(SimError::Resources(node.id))?;
        workload_used.gpus = 0;
        if let Some(routes) = &old.routes {
            for (backing, phase) in routes.catalog.backings.iter().zip(&routes.phases) {
                if !matches!(
                    phase,
                    hex_api::BackingPhase::Prepared | hex_api::BackingPhase::Released
                ) {
                    workload_used = workload_used
                        .checked_sub(&routes::backing_charge(backing, node.id)?)
                        .ok_or(SimError::Resources(node.id))?;
                }
            }
        }
        let mut free = backing_free
            .checked_sub(&workload_used)
            .ok_or(SimError::Resources(node.id))?;
        let old_devices = self.devices.get(at).ok_or(SimError::UnknownNode(node.id))?;
        let old_holders = self.held.get(at).ok_or(SimError::UnknownNode(node.id))?;
        let mut by_id = Vec::new();
        by_id
            .try_reserve_exact(devices.len())
            .map_err(|_| SimError::Resources(node.id))?;
        by_id.extend(
            devices
                .iter()
                .enumerate()
                .filter_map(|(at, physical)| physical.id.map(|id| (id, at))),
        );
        by_id.sort_unstable_by_key(|(id, _)| *id);
        for (old_at, (physical, owner)) in old_devices.iter().zip(old_holders).enumerate() {
            let Some(workload) = owner.and_then(Holder::workload) else {
                continue;
            };
            let next_at = if routes.is_some() {
                let id = physical
                    .id
                    .ok_or(SimError::Config("routed retained identity is synthetic"))?;
                by_id
                    .binary_search_by_key(&id, |(id, _)| *id)
                    .ok()
                    .and_then(|at| by_id.get(at))
                    .map(|(_, at)| *at)
                    .ok_or(SimError::Config(
                        "refreshed facts remove a retained physical claim",
                    ))?
            } else {
                old_at
            };
            let next = devices
                .get_mut(next_at)
                .ok_or(SimError::Resources(node.id))?;
            if next.id != physical.id
                || next
                    .observed
                    .zip(physical.observed)
                    .is_some_and(|(new, old)| {
                        new.kind != old.kind
                            || new.key != old.key
                            || new.model != old.model
                            || new.size != old.size
                    })
            {
                return Err(SimError::Config(
                    "refreshed facts replace a retained physical identity",
                ));
            }
            if next.observed.is_none() {
                next.observed = physical.observed.map(|mut device| {
                    device.available = false;
                    device.load = GpuLoad::default();
                    device
                });
            }
            let holder = holders
                .get_mut(next_at)
                .ok_or(SimError::Resources(node.id))?;
            *holder = Some(match (*holder, owner.and_then(Holder::backing)) {
                (Some(Holder::Backing(key)), Some(old)) if key == old => Holder::Handoff {
                    backing: key,
                    workload,
                },
                (None, _) => Holder::Box(workload),
                _ => {
                    return Err(SimError::Config(
                        "refreshed backing overlaps another retained physical owner",
                    ));
                }
            });
        }
        free.gpus = u32::try_from(
            devices
                .iter()
                .zip(&holders)
                .filter(|(physical, holder)| {
                    holder.is_none()
                        && physical
                            .observed
                            .as_ref()
                            .map(|device| device.kind == DeviceKind::Gpu)
                            .unwrap_or_else(|| {
                                physical
                                    .id
                                    .is_some_and(|id| id.kind == hex_api::DeviceKind::Gpu)
                            })
                })
                .count(),
        )
        .map_err(|_| SimError::Resources(node.id))?
        .checked_sub(pending_gpus)
        .ok_or(SimError::Resources(node.id))?;
        Ok(PreparedRefresh {
            node: node.id,
            slot: Slot {
                capacity: node.capacity,
                free,
                background_milli: node.background_milli,
                classes: node.classes,
                platform: node.platform,
                kvm: node.virt.kvm,
                builds: old.builds,
                benchmarks: old.benchmarks,
                used_by_class: old.used_by_class,
                runtime_pools: pools,
                costs: node.costs,
                priors: node.priors,
                routes,
            },
            devices,
            holders,
        })
    }

    pub(crate) fn commit_refresh_node(
        &mut self,
        prepared: PreparedRefresh,
    ) -> Result<(), SimError> {
        let at =
            usize::try_from(prepared.node.0).map_err(|_| SimError::UnknownNode(prepared.node))?;
        let old = self
            .slots
            .get(at)
            .ok_or(SimError::UnknownNode(prepared.node))?;
        if self.devices.get(at).is_none() || self.held.get(at).is_none() {
            return Err(SimError::UnknownNode(prepared.node));
        }
        let old_capacity = old.capacity;
        let from = bucket_slot(old, self.last);
        let to = bucket_slot(&prepared.slot, self.last);
        self.ix
            .reshape(&amounts(&old_capacity), &amounts(&prepared.slot.capacity));
        self.ix.relink(prepared.node.0, from, to);
        *self
            .slots
            .get_mut(at)
            .ok_or(SimError::UnknownNode(prepared.node))? = prepared.slot;
        *self
            .devices
            .get_mut(at)
            .ok_or(SimError::UnknownNode(prepared.node))? = prepared.devices;
        *self
            .held
            .get_mut(at)
            .ok_or(SimError::UnknownNode(prepared.node))? = prepared.holders;
        Ok(())
    }

    pub fn refresh_node(&mut self, node: &NodeSpec) -> Result<(), SimError> {
        let prepared = self.prepare_refresh_node(node)?;
        self.commit_refresh_node(prepared)
    }

    /// A GPU's load from what isn't Hex's, as its node's agent reads it (hex-gpu's `sample()`).
    pub fn set_gpu_load(
        &mut self,
        node: NodeId,
        device: usize,
        load: GpuLoad,
    ) -> Result<(), SimError> {
        let d = usize::try_from(node.0)
            .ok()
            .and_then(|at| self.devices.get_mut(at))
            .and_then(|ds| ds.get_mut(device))
            .and_then(|slot| slot.observed.as_mut())
            .filter(|d| d.kind == DeviceKind::Gpu)
            .ok_or(SimError::UnknownNode(node))?;
        d.load = load;
        Ok(())
    }

    /// Whether `node`'s CPU in use is above 90% of its CPU.
    pub fn hot(&self, node: NodeId) -> Result<bool, SimError> {
        Ok(self.slot(node)?.hot())
    }

    /// Whether `node` runs `class`: a change to it can change a pick for that class.
    pub fn runs(&self, node: NodeId, class: Class) -> bool {
        self.slot(node).is_ok_and(|s| s.classes.has(class))
    }

    /// Whether any node admits `demand` now.
    pub fn admitted_anywhere(&self, demand: &Demand) -> bool {
        self.slots
            .iter()
            .enumerate()
            .any(|(at, slot)| self.judge(at, slot, demand) == Verdict::Admits)
    }

    /// The best node for `demand` by `pick`'s scorer, ties broken by [`Key`]'s order: the kernel's
    /// walk ([`crate::lb::Index`], kiln's `owner.rs` walk) from the bucket of the need's whole CPUs up,
    /// every node in it read and checked exactly. Kiln's walk stops at the first runner that fits,
    /// the least loaded; Hex's reads them all, since the packing score (H11) ranks by what each node
    /// would have left. Each node is read once, so the node count bounds it. GPU work ranks first by
    /// how idle the GPUs it would hold are ([`gpu_busy`]), as kiln ranks by CPU free: the less-loaded
    /// GPU wins.
    pub fn fit(&self, demand: &Demand, pick: &Pick) -> Fit {
        self.fit_excluding(demand, pick, &std::collections::BTreeSet::new())
    }

    /// The same kernel walk and scoring, excluding peers that explicitly refused this attempt.
    pub fn fit_excluding(
        &self,
        demand: &Demand,
        pick: &Pick,
        excluded: &std::collections::BTreeSet<NodeId>,
    ) -> Fit {
        self.fit_scored(demand, pick, None, excluded)
    }

    /// CP-T7.4/B4: exact kiln per-ask terms, sharing the same admission and packing core. No
    /// context uses the original path. Image identity stays in the caller's borrowed warm facts.
    pub fn fit_with_terms(
        &self,
        demand: &Demand,
        pick: &Pick,
        terms: &Terms<'_>,
    ) -> Result<Fit, SimError> {
        self.fit_with_terms_excluding(demand, pick, terms, &std::collections::BTreeSet::new())
    }

    pub fn fit_with_terms_excluding(
        &self,
        demand: &Demand,
        pick: &Pick,
        terms: &Terms<'_>,
        excluded: &std::collections::BTreeSet<NodeId>,
    ) -> Result<Fit, SimError> {
        let prepared = self.costs.prepare(terms)?;
        // No measured class service and no modeled component means no time signal. Keep exactly
        // the original single walk; a measured zero still counts as a real observation.
        if !terms.costs() && !self.costs.observed_class(demand.class) {
            return Ok(self.fit_excluding(demand, pick, excluded));
        }
        Ok(self.fit_scored(demand, pick, Some(&prepared), excluded))
    }

    /// Measured rates, converted off the pick path just as kiln converts a node's new word/link.
    pub fn set_rates(&mut self, node: NodeId, rates: Rates) -> Result<(), SimError> {
        self.costs.set_rates(node, rates)
    }

    /// Observe actual complete create/start service, including opaque image staging, for this
    /// node/class. Explicit modeled Terms omit this combined elapsed time, preventing double charge.
    pub fn observe_start(
        &mut self,
        node: NodeId,
        class: Class,
        elapsed_ns: u64,
    ) -> Result<(), SimError> {
        self.costs.observe(node, class, elapsed_ns)
    }

    pub fn start_observations(&self) -> Vec<StartObservation> {
        self.costs.snapshot()
    }

    /// Atomically restore at most one checked aggregate per node/class from the shell's durable record.
    pub fn restore_start_observations(
        &mut self,
        rows: &[StartObservation],
    ) -> Result<(), SimError> {
        self.costs.restore(rows)
    }

    fn fit_scored(
        &self,
        demand: &Demand,
        pick: &Pick,
        terms: Option<&Prepared<'_>>,
        excluded: &std::collections::BTreeSet<NodeId>,
    ) -> Fit {
        let floor = u32::try_from(demand.need.cpu_milli.checked_div(lb::CPU).unwrap_or(0))
            .unwrap_or(u32::MAX);
        let (mut best, mut best_score) = (None::<Key>, (0u64, 0u128));
        let mut soonest = None::<u128>;
        let mut gpu_rank = 0;
        let mut refused = Refusals::default();
        let mut walked = 0u64;
        let mut b = self.ix.at_or_above(floor.min(self.last));
        while let Some(bb) = b {
            for r in self.ix.bucket(bb) {
                if excluded.contains(&NodeId(r)) {
                    continue;
                }
                walked = walked.saturating_add(1);
                let Some(slot) = self.slots.get(lb::ix(r)) else {
                    continue;
                };
                match self.judge(lb::ix(r), slot, demand) {
                    Verdict::Admits => {}
                    Verdict::NoRoom => continue,
                    Verdict::NoClass => {
                        refused.class = refused.class.saturating_add(1);
                        continue;
                    }
                    Verdict::NoPlatform => {
                        refused.platform = refused.platform.saturating_add(1);
                        continue;
                    }
                    Verdict::NoGpu => {
                        refused.gpu = refused.gpu.saturating_add(1);
                        continue;
                    }
                    Verdict::NoDevice => {
                        refused.device = refused.device.saturating_add(1);
                        continue;
                    }
                    Verdict::Shares => {
                        refused.shares = refused.shares.saturating_add(1);
                        continue;
                    }
                    Verdict::Hot => {
                        refused.hot = refused.hot.saturating_add(1);
                        continue;
                    }
                }
                let key = Key::of(NodeId(r), &slot.free);
                let idle = if demand.need.gpus > 0 {
                    let devices = self.devices.get(lb::ix(r)).map_or(&[][..], Vec::as_slice);
                    self.bound(lb::ix(r), demand)
                        .map_or(0, |held| gpu_idle(devices, &held))
                } else {
                    0
                };
                let Some(charged) = slot.charged(demand) else {
                    continue;
                };
                let score = (idle, slot.score(&charged, pick));
                if let Some(terms) = terms {
                    let finish = self.costs.finish(
                        NodeId(r),
                        demand.class,
                        charged.cpu_milli,
                        slot.free.cpu_milli,
                        terms,
                    );
                    if soonest.is_none() || idle > gpu_rank {
                        gpu_rank = idle;
                        soonest = Some(finish);
                    } else if idle == gpu_rank {
                        soonest = soonest.map(|old| old.min(finish));
                    }
                }
                // A higher score wins; between equal scores, the least key.
                let wins =
                    best.is_none_or(|k| score > best_score || (score == best_score && key < k));
                if wins {
                    best = Some(key);
                    best_score = score;
                }
            }
            b = self.ix.above(bb);
        }
        if let (Some(terms), Some(soonest)) = (terms, soonest) {
            // Kiln's within_bound: packing decides inside the exact 1/64 finish band. Hex's
            // established GPU-load preference remains first; no scalar weights join these units.
            let bound = start_cost::within_bound(soonest);
            best = None;
            let mut packing = 0;
            let mut b = self.ix.at_or_above(floor.min(self.last));
            while let Some(bb) = b {
                for r in self.ix.bucket(bb) {
                    if excluded.contains(&NodeId(r)) {
                        continue;
                    }
                    walked = walked.saturating_add(1);
                    let Some(slot) = self.slots.get(lb::ix(r)) else {
                        continue;
                    };
                    if self.judge(lb::ix(r), slot, demand) != Verdict::Admits {
                        continue;
                    }
                    let Some(charged) = slot.charged(demand) else {
                        continue;
                    };
                    let idle = if demand.need.gpus > 0 {
                        let devices = self.devices.get(lb::ix(r)).map_or(&[][..], Vec::as_slice);
                        self.bound(lb::ix(r), demand)
                            .map_or(0, |held| gpu_idle(devices, &held))
                    } else {
                        0
                    };
                    if idle != gpu_rank
                        || self.costs.finish(
                            NodeId(r),
                            demand.class,
                            charged.cpu_milli,
                            slot.free.cpu_milli,
                            terms,
                        ) > bound
                    {
                        continue;
                    }
                    let key = Key::of(NodeId(r), &slot.free);
                    let score = slot.score(&charged, pick);
                    if best.is_none_or(|k| score > packing || (score == packing && key < k)) {
                        best = Some(key);
                        packing = score;
                    }
                }
                b = self.ix.above(bb);
            }
        }
        Fit {
            best,
            walked,
            refused,
        }
    }

    /// Places `workload`, of `demand`, on `node`: its need taken from the free capacity, its devices
    /// held by it ([`bind`]), its kind counted; the node's new key. Refused, changing nothing, where
    /// the room or the devices aren't free.
    pub fn take(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
    ) -> Result<Key, SimError> {
        self.take_allocated(node, workload, demand, None)
    }

    /// Charge the receipt already chosen and persisted by the owner. A receiver never quotes a
    /// different profile silently; a changed initialized cost refuses before native creation.
    pub fn take_allocated(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        allocation: Option<hex_api::Allocation>,
    ) -> Result<Key, SimError> {
        let route = self.route(node, demand)?;
        self.take_routed(node, workload, demand, allocation, route)
    }

    pub fn take_routed(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        allocation: Option<hex_api::Allocation>,
        route: Option<hex_api::RouteReceipt>,
    ) -> Result<Key, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let slot = self.slot(node)?;
        if self.route(node, demand)? != route
            || !crate::model::same_charge(self.allocation(node, demand)?, allocation)
        {
            return Err(SimError::Config(
                "committed route/charge differs from initialized admission",
            ));
        }
        if slot.routes.is_some() && self.judge(at, slot, demand) != Verdict::Admits {
            return Err(SimError::Resources(node));
        }
        let charged = crate::model::reserved_resources(demand.need, allocation, route)
            .map_err(|_| SimError::Resources(node))?;
        if !slot.free.holds(&charged) || !slot.runtime_holds(demand, &charged) {
            return Err(SimError::Resources(node));
        }
        let held = self.bound(at, demand).ok_or(SimError::Resources(node))?;
        let pending = self.pending_gpus(node)?;
        self.change(node, demand, route, true, &charged)?;
        self.claim_workload(
            node,
            workload,
            &held,
            route.and_then(|route| route.backing),
            pending,
        )?;
        self.key(node)
    }

    fn claim_workload(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        held: &[u16],
        backing: Option<hex_api::BackingKey>,
        pending: u32,
    ) -> Result<(), SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let holders = self.held.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        for &index in held {
            let holder = holders
                .get_mut(usize::from(index))
                .ok_or(SimError::Resources(node))?;
            *holder = Some(match (*holder, backing) {
                (Some(Holder::Backing(owned)), Some(key)) if owned == key => Holder::Handoff {
                    backing: key,
                    workload,
                },
                (None, _) => Holder::Box(workload),
                _ => {
                    return Err(SimError::Config(
                        "physical handoff conflicts with another owner",
                    ));
                }
            });
        }
        self.recount_gpus(node, pending)
    }

    fn pending_gpus(&self, node: NodeId) -> Result<u32, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        pending_gpus(
            self.slot(node)?,
            self.devices.get(at).ok_or(SimError::UnknownNode(node))?,
            self.held.get(at).ok_or(SimError::UnknownNode(node))?,
            node,
        )
    }

    fn recount_gpus(&mut self, node: NodeId, pending_gpus: u32) -> Result<(), SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let physical = self.devices.get(at).ok_or(SimError::UnknownNode(node))?;
        let holders = self.held.get(at).ok_or(SimError::UnknownNode(node))?;
        let count = physical
            .iter()
            .zip(holders)
            .filter(|(slot, holder)| {
                holder.is_none()
                    && slot
                        .observed
                        .as_ref()
                        .map(|d| d.kind == DeviceKind::Gpu)
                        .unwrap_or_else(|| {
                            slot.id
                                .is_some_and(|id| id.kind == hex_api::DeviceKind::Gpu)
                        })
            })
            .count();
        let count = u32::try_from(count)
            .ok()
            .and_then(|count| count.checked_sub(pending_gpus))
            .ok_or(SimError::Resources(node))?;
        self.slots
            .get_mut(at)
            .ok_or(SimError::UnknownNode(node))?
            .free
            .gpus = count;
        Ok(())
    }

    /// Restore a durable remote-owned row while asking the selected peer Holds. This reserves
    /// the original charge/tuple without a fresh candidate or replacement physical binding.
    pub(crate) fn take_pending_routed(
        &mut self,
        node: NodeId,
        demand: &Demand,
        allocation: Option<hex_api::Allocation>,
        route: Option<hex_api::RouteReceipt>,
    ) -> Result<Key, SimError> {
        let charged = crate::model::reserved_resources(demand.need, allocation, route)
            .map_err(|_| SimError::Resources(node))?;
        let slot = self.slot(node)?;
        if !slot.free.holds(&charged)
            || !slot.runtime_holds_committed(demand, &charged, allocation, route)
        {
            return Err(SimError::Resources(node));
        }
        self.change(node, demand, route, true, &charged)
    }

    /// Validate a durable binding against stable authority, not transient GPU utilisation.
    pub(crate) fn check_recovered_routed_devices(
        &self,
        node: NodeId,
        demand: &Demand,
        held: &[u16],
        receipt: Option<hex_api::RouteReceipt>,
    ) -> Result<(), SimError> {
        let slot = self.slot(node)?;
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let devices = self.devices.get(at).ok_or(SimError::UnknownNode(node))?;
        let class = demand.class;
        let os = slot.platform.os;
        let image_os = receipt
            .map(|receipt| match receipt.key.image.os {
                hex_api::ImageOs::Linux => crate::model::Os::Linux,
                hex_api::ImageOs::Windows => crate::model::Os::Windows,
                hex_api::ImageOs::Macos => crate::model::Os::Macos,
            })
            .unwrap_or(match class {
                Class::Oci => os,
                Class::Shards | Class::Firecracker => crate::model::Os::Linux,
            });
        let image_arch = receipt
            .map(|receipt| match receipt.key.image.arch {
                hex_api::ImageArch::Amd64 => crate::model::Arch::X86_64,
                hex_api::ImageArch::Arm64 => crate::model::Arch::Aarch64,
            })
            .unwrap_or(slot.platform.arch);
        if !slot.classes.has(class)
            || demand.ask.os.is_some_and(|asked| asked != image_os)
            || demand.ask.arch.is_some_and(|asked| asked != image_arch)
            || (demand.ask.kvm
                && (!slot.kvm
                    || !class.gives_kvm(os)
                    || receipt.is_some_and(|receipt| {
                        receipt.key.execution != hex_api::Execution::Native
                    })))
        {
            return Err(SimError::Config(
                "recovered binding class or platform differs",
            ));
        }
        let expected = [
            usize::try_from(demand.need.gpus).map_err(|_| SimError::Resources(node))?,
            usize::from(demand.ask.vfio),
            usize::from(demand.ask.block),
            usize::from(demand.ask.net),
        ];
        let count = expected
            .iter()
            .try_fold(0usize, |n, more| n.checked_add(*more))
            .ok_or(SimError::Resources(node))?;
        if held.len() != count || held.len() > devices.len() {
            return Err(SimError::Config(
                "recovered binding count differs from demand",
            ));
        }
        if held.is_empty() {
            return Ok(());
        }
        let mut seen = Vec::new();
        seen.try_reserve_exact(devices.len())
            .map_err(|_| SimError::Resources(node))?;
        seen.resize(devices.len(), false);
        let mut counts = [0usize; 4];
        for &index in held {
            let index = usize::from(index);
            let device = devices
                .get(index)
                .and_then(|slot| slot.observed.as_ref())
                .ok_or(SimError::Config(
                    "recovered device has no retained observation",
                ))?;
            let used = seen
                .get_mut(index)
                .ok_or(SimError::Config("recovered device index out of range"))?;
            if *used {
                return Err(SimError::Config("duplicate recovered device index"));
            }
            *used = true;
            if !class.exposes(os, device.kind) || !device.classes.has(class) {
                return Err(SimError::Config(
                    "recovered device is not exposed by its class",
                ));
            }
            let kind = match device.kind {
                DeviceKind::Gpu => {
                    if demand
                        .ask
                        .gpu_model
                        .is_some_and(|model| model != device.model)
                        || device.size < demand.ask.gpu_memory
                    {
                        return Err(SimError::Config(
                            "recovered GPU model or total size differs",
                        ));
                    }
                    0
                }
                DeviceKind::Vfio => 1,
                DeviceKind::Block => {
                    if device.size < demand.ask.block_size {
                        return Err(SimError::Config("recovered block device is too small"));
                    }
                    2
                }
                DeviceKind::Net => 3,
            };
            let count = counts.get_mut(kind).ok_or(SimError::Resources(node))?;
            *count = count.checked_add(1).ok_or(SimError::Resources(node))?;
        }
        if counts != expected {
            return Err(SimError::Config(
                "recovered device kinds differ from demand",
            ));
        }
        Ok(())
    }

    /// Restore the old immutable charge, independently of the profile offered for new boxes.
    #[cfg(test)]
    pub(crate) fn take_recovered_allocated(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        held: &[u16],
        allocation: Option<hex_api::Allocation>,
    ) -> Result<Key, SimError> {
        let route = self.route(node, demand)?;
        self.take_recovered_routed(node, workload, demand, held, allocation, route)
    }

    pub(crate) fn take_recovered_routed(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        held: &[u16],
        allocation: Option<hex_api::Allocation>,
        route: Option<hex_api::RouteReceipt>,
    ) -> Result<Key, SimError> {
        self.check_recovered_routed_devices(node, demand, held, route)?;
        let charged = crate::model::reserved_resources(demand.need, allocation, route)
            .map_err(|_| SimError::Resources(node))?;
        let slot = self.slot(node)?;
        if !slot.free.holds(&charged)
            || !slot.runtime_holds_committed(demand, &charged, allocation, route)
            || matches!(demand.kind, Kind::Build) && slot.benchmarks > 0
            || matches!(demand.kind, Kind::Benchmark) && slot.builds > 0
        {
            return Err(SimError::Resources(node));
        }
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let holders = self.held.get(at).ok_or(SimError::UnknownNode(node))?;
        let backing = route.and_then(|route| route.backing);
        if holders
            .iter()
            .any(|holder| holder.and_then(Holder::workload) == Some(workload))
            || held.iter().any(|index| {
                holders
                    .get(usize::from(*index))
                    .is_none_or(|holder| !Self::physical_free(*holder, backing))
            })
        {
            return Err(SimError::Config(
                "recovered device overlaps an existing reservation",
            ));
        }
        let pending = self.pending_gpus(node)?;
        self.change(node, demand, route, true, &charged)?;
        self.claim_workload(node, workload, held, backing, pending)?;
        self.key(node)
    }

    pub fn give(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
    ) -> Result<Key, SimError> {
        if self
            .slot(node)?
            .costs
            .get(demand.class.index())
            .is_some_and(Option::is_some)
        {
            return Err(SimError::Config(
                "costed release requires its immutable allocation receipt",
            ));
        }
        self.give_allocated(node, workload, demand, None)
    }

    pub fn give_allocated(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        allocation: Option<hex_api::Allocation>,
    ) -> Result<Key, SimError> {
        let route = self.route(node, demand)?;
        self.give_routed(node, workload, demand, allocation, route)
    }

    pub fn give_routed(
        &mut self,
        node: NodeId,
        workload: WorkloadId,
        demand: &Demand,
        allocation: Option<hex_api::Allocation>,
        route: Option<hex_api::RouteReceipt>,
    ) -> Result<Key, SimError> {
        let charged = crate::model::reserved_resources(demand.need, allocation, route)
            .map_err(|_| SimError::Resources(node))?;
        let mut pending = self.pending_gpus(node)?;
        let physical_owner = self
            .held
            .get(usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?)
            .ok_or(SimError::UnknownNode(node))?
            .iter()
            .any(|holder| holder.and_then(Holder::workload) == Some(workload));
        if !physical_owner {
            pending = pending
                .checked_sub(charged.gpus)
                .ok_or(SimError::Resources(node))?;
        }
        self.change(node, demand, route, false, &charged)?;
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        for holder in self.held.get_mut(at).into_iter().flatten() {
            if holder.and_then(Holder::workload) == Some(workload) {
                *holder = match *holder {
                    Some(Holder::Handoff { backing, .. }) => Some(Holder::Backing(backing)),
                    _ => None,
                };
            }
        }
        self.recount_gpus(node, pending)?;
        self.key(node)
    }

    fn change(
        &mut self,
        node: NodeId,
        demand: &Demand,
        receipt: Option<hex_api::RouteReceipt>,
        adding: bool,
        charged: &Resources,
    ) -> Result<Key, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let slot = self.slots.get(at).ok_or(SimError::UnknownNode(node))?;
        let next_free = if adding {
            slot.free.checked_sub(charged)
        } else {
            slot.free.checked_add(charged)
        }
        .filter(|free| slot.capacity.holds(free))
        .ok_or(SimError::Resources(node))?;
        let next_used = slot
            .used_by_class
            .get(demand.class.index())
            .and_then(|used| {
                if adding {
                    used.checked_add(charged)
                } else {
                    used.checked_sub(charged)
                }
            })
            .ok_or(SimError::Resources(node))?;
        let count = |value: u32| {
            if adding {
                value.checked_add(1)
            } else {
                value.checked_sub(1)
            }
        };
        let (builds, benchmarks) = match demand.kind {
            Kind::Build => (
                count(slot.builds).ok_or(SimError::Resources(node))?,
                slot.benchmarks,
            ),
            Kind::Benchmark => (
                slot.builds,
                count(slot.benchmarks).ok_or(SimError::Resources(node))?,
            ),
            Kind::Plain | Kind::Session => (slot.builds, slot.benchmarks),
        };
        let route_used = match (&slot.routes, receipt) {
            (Some(routes), Some(receipt)) => Some(routes.used_next(receipt, demand, adding)?),
            (None, None) | (Some(_), None) => None,
            _ => {
                return Err(SimError::Config(
                    "routed admission/release lacks its original receipt",
                ));
            }
        };
        let from = bucket_slot(slot, self.last);
        let slot = self.slots.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        if let Some((at, next)) = route_used {
            *slot
                .routes
                .as_mut()
                .and_then(|routes| routes.used.get_mut(at))
                .ok_or(SimError::Resources(node))? = next;
        }
        *slot
            .used_by_class
            .get_mut(demand.class.index())
            .ok_or(SimError::Resources(node))? = next_used;
        slot.free = next_free;
        slot.builds = builds;
        slot.benchmarks = benchmarks;
        let to = bucket_slot(slot, self.last);
        self.ix.relink(node.0, from, to);
        Ok(Key::of(node, &next_free))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RuntimePool, bind, fits_devices};

    fn explicit_fixture_cost(cpu: u64) -> hex_api::RuntimeCost {
        let guest = hex_api::CapacityResources {
            cpu_milli: cpu,
            mem_bytes: 500,
            disk_bytes: 0,
        };
        hex_api::RuntimeCost {
            id: [7; 32], // Synthetic fixture identity, never an initialized/measured host profile.
            floor: guest,
            ceiling: guest,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1_000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources {
                cpu_milli: 100,
                mem_bytes: 50,
                disk_bytes: 0,
            },
        }
    }

    /// AC-T7.4.1: a CPU-only dedicated VM pays its complete initialized envelope before selection.
    #[test]
    fn costed_vm_candidates_and_admission_charge_the_same_original_demand() {
        let need = Resources {
            cpu_milli: 100,
            mem_bytes: 10,
            ..Resources::default()
        };
        let demand = Demand {
            need,
            class: Class::Shards,
            kind: Kind::Plain,
            ask: crate::model::Ask::default(),
        };
        let mut small = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 1_000,
                mem_bytes: 1_000,
                ..Resources::default()
            },
        );
        small.costs[Class::Shards.index()] = Some(explicit_fixture_cost(1_000));
        let index = FitIndex::new(&[small.clone()]).unwrap();
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoRoom);
        assert!(index.fit(&demand, &Pick::default()).best.is_none());
        small.capacity.cpu_milli = 2_100;
        let mut index = FitIndex::new(&[small]).unwrap();
        let allocation = index.allocation(NodeId(0), &demand).unwrap().unwrap();
        assert_eq!(allocation.resources.cpu_milli, 1_100);
        assert_eq!(allocation.resources.mem_bytes, 550);
        assert!(index.take(NodeId(0), WorkloadId(1), &demand).is_err());
        let key = index
            .take_allocated(NodeId(0), WorkloadId(1), &demand, Some(allocation))
            .unwrap();
        assert_eq!((key.cpu_milli, key.mem_bytes), (1_000, 450));
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoRoom);
        let native = Demand {
            class: Class::Oci,
            ..demand
        };
        assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::Admits);
        assert_eq!(demand.need, need, "original workload remains unchanged");
    }

    /// AC-T7.4.1: profile refresh affects new work; recovery and release use the old exact receipt.
    #[test]
    fn persisted_allocation_survives_cost_refresh_without_recalculation() {
        let mut node = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 5_000,
                mem_bytes: 2_000,
                ..Resources::default()
            },
        );
        node.costs[Class::Shards.index()] = Some(explicit_fixture_cost(1_000));
        let demand = Demand {
            need: Resources {
                cpu_milli: 100,
                mem_bytes: 10,
                ..Resources::default()
            },
            class: Class::Shards,
            kind: Kind::Plain,
            ask: crate::model::Ask::default(),
        };
        let mut old = FitIndex::new(&[node.clone()]).unwrap();
        let receipt = old.allocation(NodeId(0), &demand).unwrap();
        old.take_allocated(NodeId(0), WorkloadId(1), &demand, receipt)
            .unwrap();
        node.costs[Class::Shards.index()] = Some(explicit_fixture_cost(2_000));
        old.refresh_node(&node).unwrap();
        assert!(
            old.take_allocated(NodeId(0), WorkloadId(2), &demand, receipt)
                .is_err()
        );
        let mut recovered = FitIndex::new(&[node.clone()]).unwrap();
        recovered
            .take_recovered_allocated(NodeId(0), WorkloadId(1), &demand, &[], receipt)
            .unwrap();
        assert_eq!(
            recovered.key(NodeId(0)).unwrap(),
            old.key(NodeId(0)).unwrap()
        );
        let mut forged = receipt.unwrap();
        forged.resources.cpu_milli -= 1;
        assert!(
            recovered
                .give_allocated(NodeId(0), WorkloadId(1), &demand, Some(forged))
                .is_err()
        );
        assert_eq!(
            recovered.key(NodeId(0)).unwrap(),
            old.key(NodeId(0)).unwrap()
        );
        let restored = recovered
            .give_allocated(NodeId(0), WorkloadId(1), &demand, receipt)
            .unwrap();
        assert_eq!(
            (restored.cpu_milli, restored.mem_bytes),
            (node.capacity.cpu_milli, node.capacity.mem_bytes)
        );
    }

    fn res(cpu: u64, mem: u64, gpus: u32) -> Resources {
        Resources {
            cpu_milli: cpu,
            mem_bytes: mem,
            gpus,
            disk_bytes: 0,
        }
    }

    /// A workload's id where only one is placed at a time.
    const W: WorkloadId = WorkloadId(0);

    fn plain(need: Resources) -> Demand {
        Demand {
            need,
            kind: Kind::Plain,
            class: Class::Oci,
            ask: Default::default(),
        }
    }

    fn nodes(caps: &[Resources]) -> Vec<NodeSpec> {
        caps.iter()
            .enumerate()
            .map(|(i, c)| NodeSpec::new(NodeId(u32::try_from(i).unwrap()), *c))
            .collect()
    }

    /// Joint plan step 14 (AC-T7.4.1), C2 in the placement index: do place one VM's whole charge (1,100 milli-CPU of
    /// 2,100) on a node with no prior; expect no room for a second. Do give the node a learned prior of 300 milli-CPU
    /// and 100 bytes for the profile; expect a new VM's receipt booked at it, its charge and configuration unchanged,
    /// and the node's room less only what was booked. Do admit a receipt a stale view booked whole; expect it admitted
    /// as booked. Release both; expect the node's room whole again.
    #[test]
    fn w14_a_new_vm_is_booked_at_the_nodes_learned_prior() {
        let mut node = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 2_100,
                mem_bytes: 1_000,
                ..Resources::default()
            },
        );
        node.costs[Class::Shards.index()] = Some(explicit_fixture_cost(1_000));
        let demand = Demand {
            need: Resources {
                cpu_milli: 100,
                mem_bytes: 10,
                ..Resources::default()
            },
            class: Class::Shards,
            kind: Kind::Plain,
            ask: crate::model::Ask::default(),
        };
        let mut whole_only = FitIndex::new(&[node.clone()]).unwrap();
        let whole = whole_only.allocation(NodeId(0), &demand).unwrap();
        assert_eq!(whole.unwrap().booked, None);
        whole_only
            .take_allocated(NodeId(0), WorkloadId(1), &demand, whole)
            .unwrap();
        assert_eq!(
            whole_only.verdict(NodeId(0), &demand).unwrap(),
            Verdict::NoRoom
        );
        let prior = hex_api::CapacityResources {
            cpu_milli: 300,
            mem_bytes: 100,
            disk_bytes: 0,
        };
        node.priors[Class::Shards.index()] = Some(crate::model::Prior {
            profile: [7; 32],
            booking: prior,
            ceiling: whole.unwrap().resources,
        });
        let mut learned = FitIndex::new(&[node.clone()]).unwrap();
        let receipt = learned.allocation(NodeId(0), &demand).unwrap().unwrap();
        assert_eq!(receipt.booking(), prior);
        assert_eq!(receipt.resources, whole.unwrap().resources);
        assert_eq!(receipt.guest().unwrap(), whole.unwrap().guest().unwrap());
        let key = learned
            .take_allocated(NodeId(0), WorkloadId(1), &demand, Some(receipt))
            .unwrap();
        assert_eq!((key.cpu_milli, key.mem_bytes), (1_800, 900));
        let key = learned
            .take_allocated(NodeId(0), WorkloadId(2), &demand, whole)
            .unwrap();
        assert_eq!((key.cpu_milli, key.mem_bytes), (700, 350));
        learned
            .give_allocated(NodeId(0), WorkloadId(2), &demand, whole)
            .unwrap();
        learned
            .give_allocated(NodeId(0), WorkloadId(1), &demand, Some(receipt))
            .unwrap();
        assert_eq!(
            learned.key(NodeId(0)).unwrap(),
            FitIndex::new(&[node]).unwrap().key(NodeId(0)).unwrap()
        );
    }

    fn pool_node(classes: Classes, mem: u64) -> NodeSpec {
        let mut node = NodeSpec::new(NodeId(0), res(4_000, 1_024 << 20, 0));
        node.runtime_pools = vec![RuntimePool {
            classes,
            capacity: res(4_000, mem, 0),
        }];
        node
    }

    fn pool_demand(class: Class, mem: u64) -> Demand {
        Demand {
            class,
            ..plain(res(100, mem, 0))
        }
    }

    /// AC-T7.4.1: the real 633MiB-in-632MiB counterexample must fail before a guest create.
    #[test]
    fn runtime_pool_r329_guest_bound_refuses_633_mib_but_admits_64() {
        let node = pool_node(Classes::NONE.with(Class::Shards), 632 << 20);
        let mut index = FitIndex::new(&[node]).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let oversized = pool_demand(Class::Shards, 633 << 20);
        assert_eq!(
            index.verdict(NodeId(0), &oversized).unwrap(),
            Verdict::NoRoom
        );
        assert!(index.fit(&oversized, &Pick::default()).best.is_none());
        assert!(index.take(NodeId(0), W, &oversized).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
        let valid = pool_demand(Class::Shards, 64 << 20);
        index.take(NodeId(0), W, &valid).unwrap();
        index.give(NodeId(0), W, &valid).unwrap();
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
    }

    /// AC-T7.4.1: a guest cap does not strand unrelated native host capacity; both budgets apply.
    #[test]
    fn runtime_pool_keeps_unpooled_native_room_and_global_host_limit() {
        let node = pool_node(Classes::NONE.with(Class::Shards), 632 << 20);
        let mut index = FitIndex::new(&[node]).unwrap();
        let native = pool_demand(Class::Oci, 900 << 20);
        assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::Admits);
        index.take(NodeId(0), WorkloadId(1), &native).unwrap();
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 128 << 20))
                .unwrap(),
            Verdict::NoRoom
        );
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 64 << 20))
                .unwrap(),
            Verdict::Admits
        );
    }

    /// AC-T7.4.1: two class names backed by one runtime share one total, and a refused take is atomic.
    #[test]
    fn runtime_pool_shared_classes_aggregate_and_delete_frees_exactly() {
        let classes = Classes::NONE.with(Class::Shards).with(Class::Firecracker);
        let mut index = FitIndex::new(&[pool_node(classes, 100 << 20)]).unwrap();
        let original = index.key(NodeId(0)).unwrap();
        let shards = pool_demand(Class::Shards, 60 << 20);
        index.take(NodeId(0), WorkloadId(1), &shards).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let mut fail = pool_demand(Class::Firecracker, 41 << 20);
        fail.kind = Kind::Build;
        assert!(index.take(NodeId(0), WorkloadId(2), &fail).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
        let benchmark = Demand {
            kind: Kind::Benchmark,
            ..pool_demand(Class::Oci, 1)
        };
        assert_eq!(
            index.verdict(NodeId(0), &benchmark).unwrap(),
            Verdict::Admits
        );
        let firecracker = pool_demand(Class::Firecracker, 40 << 20);
        index.take(NodeId(0), WorkloadId(3), &firecracker).unwrap();
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 1))
                .unwrap(),
            Verdict::NoRoom
        );
        index.give(NodeId(0), WorkloadId(1), &shards).unwrap();
        assert_eq!(index.verdict(NodeId(0), &shards).unwrap(), Verdict::Admits);
        index.give(NodeId(0), WorkloadId(3), &firecracker).unwrap();
        assert_eq!(index.key(NodeId(0)).unwrap(), original);
        assert!(index.give(NodeId(0), WorkloadId(3), &firecracker).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), original);
    }

    /// AC-T7.4.1: refresh may split a shared pool, but never remove/shrink a held runtime budget.
    #[test]
    fn runtime_pool_refresh_preserves_class_charges_and_refuses_invalid_shrink_atomically() {
        let classes = Classes::NONE.with(Class::Shards).with(Class::Firecracker);
        let mut node = pool_node(classes, 100 << 20);
        let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        let shards = pool_demand(Class::Shards, 60 << 20);
        let firecracker = pool_demand(Class::Firecracker, 30 << 20);
        index.take(NodeId(0), WorkloadId(1), &shards).unwrap();
        index.take(NodeId(0), WorkloadId(2), &firecracker).unwrap();
        node.runtime_pools = vec![
            RuntimePool {
                classes: Classes::NONE.with(Class::Shards),
                capacity: res(4_000, 70 << 20, 0),
            },
            RuntimePool {
                classes: Classes::NONE.with(Class::Firecracker),
                capacity: res(4_000, 30 << 20, 0),
            },
        ];
        index.refresh_node(&node).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let mut shrink = node.clone();
        shrink.capacity.cpu_milli = 10_000;
        shrink.runtime_pools[0].capacity.mem_bytes = 59 << 20;
        assert!(index.refresh_node(&shrink).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 10 << 20))
                .unwrap(),
            Verdict::Admits
        );
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Firecracker, 1))
                .unwrap(),
            Verdict::NoRoom
        );
        let mut merged = node.clone();
        merged.runtime_pools = vec![RuntimePool {
            classes,
            capacity: res(4_000, 89 << 20, 0),
        }];
        assert!(index.refresh_node(&merged).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
        merged.runtime_pools[0].capacity.mem_bytes = 90 << 20;
        index.refresh_node(&merged).unwrap();
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 1))
                .unwrap(),
            Verdict::NoRoom
        );
        let mut removed = node.clone();
        removed.runtime_pools.clear();
        assert!(index.refresh_node(&removed).is_err());
        index.give(NodeId(0), WorkloadId(1), &shards).unwrap();
        index.give(NodeId(0), WorkloadId(2), &firecracker).unwrap();
        index.refresh_node(&removed).unwrap();
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Shards, 900 << 20))
                .unwrap(),
            Verdict::Admits
        );
    }

    /// AC-T7.4.1: give a held box back under the wrong kind, expect a typed refusal that publishes
    /// neither its free room nor its class use (the kind's count fails after both are computed);
    /// refresh its held pool away, expect a refusal until the box is given back (from d03fb24a).
    #[test]
    fn runtime_pool_ledger_stays_atomic_when_kind_charge_fails() {
        let node = pool_node(Classes::NONE.with(Class::Shards), 64 << 20);
        let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        let demand = pool_demand(Class::Shards, 64 << 20);
        let held = index.take(NodeId(0), W, &demand).unwrap();
        let wrong_kind = Demand {
            kind: Kind::Build,
            ..demand
        };
        assert_eq!(
            index.give(NodeId(0), W, &wrong_kind),
            Err(SimError::Resources(NodeId(0)))
        );
        assert_eq!(index.key(NodeId(0)).unwrap(), held);
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoRoom);
        let mut missing_pool = node.clone();
        missing_pool.runtime_pools.clear();
        assert!(index.refresh_node(&missing_pool).is_err());
        index.give(NodeId(0), W, &demand).unwrap();
        index.refresh_node(&missing_pool).unwrap();
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::Admits);
    }

    /// AC-T7.4.1: a newly declared pool must cover already held amounts of every included class.
    #[test]
    fn runtime_pool_new_pool_checks_preexisting_host_backed_class_holds() {
        let mut node = NodeSpec::new(NodeId(0), res(4_000, 1_024 << 20, 0));
        let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        let demand = pool_demand(Class::Oci, 64 << 20);
        index.take(NodeId(0), W, &demand).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        node.runtime_pools = vec![RuntimePool {
            classes: Classes::NONE.with(Class::Oci),
            capacity: res(4_000, 63 << 20, 0),
        }];
        assert!(index.refresh_node(&node).is_err());
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
        node.runtime_pools[0].capacity.mem_bytes = 64 << 20;
        index.refresh_node(&node).unwrap();
        assert_eq!(
            index
                .verdict(NodeId(0), &pool_demand(Class::Oci, 1))
                .unwrap(),
            Verdict::NoRoom
        );
    }

    /// AC-T7.4.1: pool masks are nonempty, disjoint and within the node's declared class set.
    #[test]
    fn runtime_pool_invalid_masks_and_excess_count_are_typed_refusals() {
        let one = RuntimePool {
            classes: Classes::NONE.with(Class::Shards),
            capacity: res(4_000, 100 << 20, 0),
        };
        let mut node = pool_node(Classes::NONE, 100 << 20);
        assert!(FitIndex::new(std::slice::from_ref(&node)).is_err());
        node.runtime_pools = vec![one, one];
        assert!(FitIndex::new(std::slice::from_ref(&node)).is_err());
        node.runtime_pools = vec![one];
        node.classes = Classes::NONE.with(Class::Oci);
        assert!(FitIndex::new(std::slice::from_ref(&node)).is_err());
        node.classes = Classes::ALL;
        node.runtime_pools = vec![one; Class::COUNT + 1];
        assert!(matches!(FitIndex::new(&[node]), Err(SimError::Full { .. })));
    }

    /// AC-T7.4.1: absent pools retain existing host-backed admission, including after reconstruction.
    #[test]
    fn runtime_pool_empty_defaults_keep_host_model_and_replay_charges_on_restart() {
        let node = NodeSpec::new(NodeId(0), res(4_000, 1_024 << 20, 0));
        assert!(node.runtime_pools.is_empty());
        let demand = pool_demand(Class::Shards, 633 << 20);
        let mut first = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        first.take(NodeId(0), W, &demand).unwrap();
        let mut rebooted = FitIndex::new(&[node]).unwrap();
        rebooted.take(NodeId(0), W, &demand).unwrap();
        assert_eq!(
            first.key(NodeId(0)).unwrap(),
            rebooted.key(NodeId(0)).unwrap()
        );
    }

    /// AC-T7.4.1: disable the real guard, place across a shared runtime beyond capacity, then
    /// require both independent oracles to reject that actual simulator history while still disabled.
    #[test]
    fn runtime_pool_guard_removal_is_caught_by_independent_log_and_filter_oracles() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                IGNORE_RUNTIME_POOLS.set(self.0);
            }
        }
        let _restore = Restore(IGNORE_RUNTIME_POOLS.replace(true));
        for sizes in [
            vec![(Class::Shards, 633u64)],
            vec![(Class::Shards, 400), (Class::Firecracker, 300)],
        ] {
            let node = pool_node(
                Classes::NONE.with(Class::Shards).with(Class::Firecracker),
                632 << 20,
            );
            let workloads = sizes
                .iter()
                .enumerate()
                .map(|(id, (class, mem))| crate::model::WorkloadSpec {
                    id: WorkloadId(u64::try_from(id).unwrap()),
                    need: res(100, *mem << 20, 0),
                    kind: Kind::Plain,
                    arrives: 0,
                    runs_for: None,
                    tenancy: Default::default(),
                    class: *class,
                    ask: Default::default(),
                })
                .collect();
            let cluster = crate::model::Cluster::single_owner(vec![node], workloads);
            let run = crate::sim::run(&cluster, &crate::sim::Config::default()).unwrap();
            assert_eq!(run.report.placements, u64::try_from(sizes.len()).unwrap());
            assert!(matches!(
                crate::spec::check_log(&cluster.nodes, &run),
                Err(crate::spec::Violation::RuntimePoolExceeded { .. })
            ));
            let report = crate::spec::check_filters(&cluster, &run).unwrap();
            assert_eq!(report.runtime_pool_off, 1);
            assert!(report.violations() > 0);
        }
    }

    /// AC-T7.4.1: allocation-free verdict matching and binding use identical device predicates.
    #[test]
    fn runtime_pool_device_predicate_keeps_binding_semantics() {
        let devices = vec![
            Device::gpu(crate::model::ModelId(0), 100),
            Device::gpu(crate::model::ModelId(1), 200),
        ];
        for count in 0..=3 {
            for model in [None, Some(crate::model::ModelId(1))] {
                let ask = crate::model::Ask {
                    gpu_model: model,
                    gpu_memory: 100,
                    ..Default::default()
                };
                assert_eq!(
                    fits_devices(&devices, |i| i != 0, count, &ask),
                    bind(&devices, |i| i != 0, count, &ask).is_some()
                );
            }
        }
    }

    /// AC-T7.4.1: runtime pools constrain each resource without replacing the host budget.
    #[test]
    fn runtime_pool_checks_cpu_disk_and_gpu_dimensions() {
        let mut node = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 4_000,
                mem_bytes: 1_024 << 20,
                gpus: 1,
                disk_bytes: 1_024,
            },
        );
        node.runtime_pools = vec![RuntimePool {
            classes: Classes::NONE.with(Class::Oci),
            capacity: Resources {
                cpu_milli: 200,
                mem_bytes: 64 << 20,
                gpus: 0,
                disk_bytes: 10,
            },
        }];
        let mut index = FitIndex::new(&[node]).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        for need in [
            res(201, 1, 0),
            Resources {
                disk_bytes: 11,
                ..res(100, 1, 0)
            },
            res(100, 1, 1),
        ] {
            let demand = plain(need);
            assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoRoom);
            assert!(index.take(NodeId(0), W, &demand).is_err());
            assert_eq!(index.key(NodeId(0)).unwrap(), before);
        }
    }

    /// Brute force: the least key among the nodes that admit the demand.
    fn oracle(index: &FitIndex, demand: &Demand) -> Option<Key> {
        (0..index.len())
            .map(|i| NodeId(u32::try_from(i).unwrap()))
            .filter(|&n| index.verdict(n, demand).unwrap() == Verdict::Admits)
            .map(|n| index.key(n).unwrap())
            .min()
    }

    /// T-T7.4.1: a whole-cluster sample is the least-free node that admits the need; GPU work goes
    /// only on a GPU node.
    #[test]
    fn whole_best_fit_is_the_least_free_node_that_admits() {
        let index = FitIndex::new(&nodes(&[
            res(8000, 16, 0),
            res(2000, 16, 0),
            res(4000, 4, 1),
        ]))
        .unwrap();
        let fit = |need| {
            index
                .fit(&plain(need), &Pick::default())
                .best
                .map(|k| k.node)
        };
        assert_eq!(fit(res(1000, 8, 0)), Some(NodeId(1)));
        assert_eq!(fit(res(3000, 8, 0)), Some(NodeId(0)));
        assert_eq!(fit(res(1000, 1, 1)), Some(NodeId(2)));
        assert_eq!(fit(res(1000, 8, 1)), None);
    }

    /// CP-T7.4/B4: measured startup can move a pick, packing still wins inside kiln's 1/64
    /// finish band, and the original no-context core is unaffected by learned timing state.
    #[test]
    fn start_cost_changes_the_actual_pick_but_retains_packing_ties() {
        for (times, expected) in [([1000, 990], NodeId(0)), ([1000, 500], NodeId(1))] {
            let mut index = FitIndex::new(&nodes(&[res(2000, 8, 0), res(4000, 8, 0)])).unwrap();
            let demand = plain(res(1000, 1, 0));
            for (node, ns) in [NodeId(0), NodeId(1)].into_iter().zip(times) {
                index.observe_start(node, Class::Oci, ns).unwrap();
            }
            assert_eq!(
                index
                    .fit_with_terms(&demand, &Pick::default(), &Terms::default())
                    .unwrap()
                    .best
                    .map(|k| k.node),
                Some(expected)
            );
            assert_eq!(
                index.fit(&demand, &Pick::default()).best.map(|k| k.node),
                Some(NodeId(0))
            );
            let shards = Demand {
                class: Class::Shards,
                ..demand
            };
            assert_eq!(
                index
                    .fit_with_terms(&shards, &Pick::default(), &Terms::default())
                    .unwrap()
                    .best
                    .map(|k| k.node),
                Some(NodeId(0))
            );
        }
    }

    /// CP-T7.4: calling the production terms path before learning anything costs the same single
    /// candidate walk; another class's observations do not activate a model for this request.
    #[test]
    fn start_cost_empty_context_keeps_the_original_single_walk() {
        let mut index =
            FitIndex::new(&nodes(&[res(2000, 8, 0), res(4000, 8, 0), res(8000, 8, 0)])).unwrap();
        let demand = plain(res(1000, 1, 0));
        let pick = Pick::default();
        let original = index.fit(&demand, &pick);
        let cold = index
            .fit_with_terms(&demand, &pick, &Terms::default())
            .unwrap();
        assert_eq!(
            (cold.best, cold.walked, cold.refused),
            (original.best, original.walked, original.refused)
        );
        index.observe_start(NodeId(0), Class::Shards, 100).unwrap();
        assert_eq!(
            index
                .fit_with_terms(&demand, &pick, &Terms::default())
                .unwrap()
                .walked,
            original.walked
        );
        index.observe_start(NodeId(0), Class::Oci, 100).unwrap();
        let learned = index
            .fit_with_terms(&demand, &pick, &Terms::default())
            .unwrap();
        assert_eq!(learned.walked, original.walked * 2);
    }

    /// CP-T7.4: requested image warmth is borrowed per ask, never retained under a hash or
    /// leaked to the next image; refused nodes remain excluded even when their start is free.
    #[test]
    fn start_cost_image_terms_and_exclusions_share_the_admission_core() {
        use crate::start_cost::{Link, Warm};
        let mut index = FitIndex::new(&nodes(&[res(2000, 8, 0), res(4000, 8, 0)])).unwrap();
        for node in [NodeId(0), NodeId(1)] {
            index
                .set_rates(
                    node,
                    Rates {
                        link: Link {
                            rate: 1_000_000_000,
                            ..Link::default()
                        },
                        ..Rates::default()
                    },
                )
                .unwrap();
        }
        let demand = plain(res(1000, 1, 0));
        let warm = [Warm {
            node: NodeId(1),
            ..Warm::default()
        }];
        let image = Terms {
            bytes: 1000,
            warm: &warm,
            ..Terms::default()
        };
        assert_eq!(
            index
                .fit_with_terms(&demand, &Pick::default(), &image)
                .unwrap()
                .best
                .map(|k| k.node),
            Some(NodeId(1))
        );
        let excluded = [NodeId(1)].into_iter().collect();
        assert_eq!(
            index
                .fit_with_terms_excluding(&demand, &Pick::default(), &image, &excluded)
                .unwrap()
                .best
                .map(|k| k.node),
            Some(NodeId(0))
        );
        let another_image = Terms { warm: &[], ..image };
        assert_eq!(
            index
                .fit_with_terms(&demand, &Pick::default(), &another_image)
                .unwrap()
                .best
                .map(|k| k.node),
            Some(NodeId(0))
        );
    }

    /// CP-T7.4: learned time never overrides Hex's existing least-loaded GPU preference.
    #[test]
    fn start_cost_preserves_the_existing_gpu_load_preference() {
        use crate::model::{Ask, Os};
        let mut index = FitIndex::new(&[
            gpu_node(0, 4_000, Os::Linux, &[(1, 8 << 30, 700)]),
            gpu_node(1, 8_000, Os::Linux, &[(1, 8 << 30, 100)]),
        ])
        .unwrap();
        index.observe_start(NodeId(0), Class::Oci, 1).unwrap();
        index.observe_start(NodeId(1), Class::Oci, 1000).unwrap();
        let demand = asking(1, Class::Oci, Ask::default());
        assert_eq!(
            index
                .fit_with_terms(&demand, &Pick::default(), &Terms::default())
                .unwrap()
                .best
                .map(|k| k.node),
            Some(NodeId(1))
        );
    }

    /// S3's filters in Hex's kinds (AC-T7.4.3's workload): do a Firecracker microVM where one node
    /// runs only OCI containers and the other every class; expect the first to refuse it as NoClass
    /// and the scan to count that refusal, and an OCI container still to fit both.
    #[test]
    fn a_class_the_node_cant_run_is_refused() {
        let mut nodes = nodes(&[res(8_000, 8, 0), res(8_000, 8, 0)]);
        nodes[0].classes = Classes::NONE.with(Class::Oci);
        let index = FitIndex::new(&nodes).unwrap();
        let microvm = Demand {
            need: res(1_000, 1, 0),
            kind: Kind::Plain,
            class: Class::Firecracker,
            ask: Default::default(),
        };
        assert_eq!(
            index.verdict(NodeId(0), &microvm).unwrap(),
            Verdict::NoClass
        );
        assert_eq!(index.verdict(NodeId(1), &microvm).unwrap(), Verdict::Admits);
        let s = index.fit(&microvm, &Pick::default());
        assert_eq!(
            (s.best.map(|k| k.node), s.refused.class),
            (Some(NodeId(1)), 1)
        );
        let container = plain(res(1_000, 1, 0));
        assert_eq!(
            index.verdict(NodeId(0), &container).unwrap(),
            Verdict::Admits
        );
    }

    /// S3's GPU filter in Hex's kinds: do an OCI container, a Firecracker microVM and a Shards
    /// microVM, each asking a GPU, on a GPU node that runs every class; expect the container admitted
    /// and each microVM refused as NoGpu (neither can be given a GPU), so no node admits it and
    /// admission turns it away.
    #[test]
    fn only_an_oci_container_is_given_a_gpu() {
        let index = FitIndex::new(&nodes(&[res(8_000, 8, 2)])).unwrap();
        let asking = |class| Demand {
            need: res(1_000, 1, 1),
            kind: Kind::Plain,
            class,
            ask: Default::default(),
        };
        assert_eq!(
            index.verdict(NodeId(0), &asking(Class::Oci)).unwrap(),
            Verdict::Admits
        );
        for class in [Class::Firecracker, Class::Shards] {
            assert_eq!(
                index.verdict(NodeId(0), &asking(class)).unwrap(),
                Verdict::NoGpu
            );
            assert!(!index.admitted_anywhere(&asking(class)));
        }
    }

    /// AC-T7.6.3 and S3's filters, each alone: a benchmark refused beside a build and a build beside a
    /// benchmark; a session refused a node above 90% CPU, counting its background; GPU work refused a
    /// node without GPUs.
    #[test]
    fn each_filter_refuses_what_it_names() {
        let mut ns = nodes(&[res(10_000, 100, 0), res(10_000, 100, 2)]);
        ns[0].background_milli = 9_100;
        let mut index = FitIndex::new(&ns).unwrap();
        let session = Demand {
            need: res(1_000, 1, 0),
            kind: Kind::Session,
            class: Class::Oci,
            ask: Default::default(),
        };
        assert_eq!(index.verdict(NodeId(0), &session).unwrap(), Verdict::Hot);
        assert_eq!(index.verdict(NodeId(1), &session).unwrap(), Verdict::Admits);
        let gpu = plain(res(1, 1, 1));
        assert_eq!(index.verdict(NodeId(0), &gpu).unwrap(), Verdict::NoGpu);
        let build = Demand {
            need: res(1, 1, 0),
            kind: Kind::Build,
            class: Class::Oci,
            ask: Default::default(),
        };
        let bench = Demand {
            need: res(1, 1, 0),
            kind: Kind::Benchmark,
            class: Class::Oci,
            ask: Default::default(),
        };
        index.take(NodeId(1), W, &build).unwrap();
        assert_eq!(index.verdict(NodeId(1), &bench).unwrap(), Verdict::Shares);
        index.give(NodeId(1), W, &build).unwrap();
        index.take(NodeId(1), W, &bench).unwrap();
        assert_eq!(index.verdict(NodeId(1), &build).unwrap(), Verdict::Shares);
        let s = index.fit(&build, &Pick::default());
        assert_eq!(
            (s.best.map(|k| k.node), s.refused.shares),
            (Some(NodeId(0)), 1)
        );
    }

    fn gpu_node(id: u32, cpu: u64, os: crate::model::Os, gpus: &[(u32, u64, u16)]) -> NodeSpec {
        use crate::model::{Device, GpuLoad, ModelId, Platform};
        let mut n = NodeSpec::new(NodeId(id), res(cpu, 64, 0));
        n.platform = Platform {
            os,
            arch: crate::model::Arch::X86_64,
        };
        n.classes = os.classes(crate::model::Virt {
            kvm: true,
            nested: false,
            hvf: true,
        });
        n.devices = gpus
            .iter()
            .map(|&(model, memory, busy)| Device {
                load: GpuLoad {
                    busy_per_mille: Some(busy),
                    memory_used: Some(0),
                },
                ..Device::gpu(ModelId(model), memory)
            })
            .collect();
        n.capacity.gpus = u32::try_from(gpus.len()).unwrap();
        n
    }

    fn asking(gpus: u32, class: Class, ask: crate::model::Ask) -> Demand {
        Demand {
            need: res(1_000, 1, gpus),
            kind: Kind::Plain,
            class,
            ask,
        }
    }

    /// AC-T7.4.1: missing telemetry prevents a new GPU grant, preserves the old exact holder and
    /// leaves CPU-only work admissible. Release still follows the original held assignment.
    #[test]
    fn unread_gpu_load_preserves_held_identity_and_independent_cpu_progress() {
        use crate::model::{Ask, GpuLoad, Os};
        let spec = gpu_node(0, 4_000, Os::Linux, &[(1, 4096, 0), (1, 4096, 500)]);
        let mut index = FitIndex::new(&[spec]).unwrap();
        let owner = WorkloadId(9);
        let gpu = asking(1, Class::Oci, Ask::default());
        index.take(NodeId(0), owner, &gpu).unwrap();
        let held = index.holds(NodeId(0), owner);
        assert_eq!(held, vec![0]);
        for device in 0..2 {
            index
                .set_gpu_load(NodeId(0), device, GpuLoad::default())
                .unwrap();
        }
        assert_eq!(index.holds(NodeId(0), owner), held);
        assert!(index.fit(&gpu, &Pick::default()).best.is_none());
        let cpu = asking(0, Class::Oci, Ask::default());
        assert_eq!(
            index.fit(&cpu, &Pick::default()).best.map(|key| key.node),
            Some(NodeId(0))
        );
        index.give(NodeId(0), owner, &gpu).unwrap();
        assert!(index.holds(NodeId(0), owner).is_empty());
        assert!(index.fit(&gpu, &Pick::default()).best.is_none());
        index
            .set_gpu_load(
                NodeId(0),
                1,
                GpuLoad {
                    busy_per_mille: Some(0),
                    memory_used: Some(0),
                },
            )
            .unwrap();
        assert_eq!(
            index.fit(&gpu, &Pick::default()).best.map(|key| key.node),
            Some(NodeId(0))
        );
    }

    /// AC-T7.4.1: image OS follows the initialized class; host capability classes stay unchanged.
    #[test]
    fn image_os_matches_vm_guest_and_oci_host() {
        use crate::model::{Arch, Ask, Os, Virt};
        let mut mac = NodeSpec::new(NodeId(0), res(4_000, 16, 0));
        mac.platform = Platform {
            os: Os::Macos,
            arch: Arch::Aarch64,
        };
        mac.virt = Virt {
            hvf: true,
            ..Virt::default()
        };
        mac.classes = Os::Macos.classes(mac.virt);
        let mut windows = NodeSpec::new(NodeId(1), res(4_000, 16, 0));
        windows.platform.os = Os::Windows;
        windows.classes = Os::Windows.classes(windows.virt);
        let linux = NodeSpec::new(NodeId(2), res(4_000, 16, 0));
        let ix = FitIndex::new(&[mac, windows, linux]).unwrap();
        let guest = |os, arch| {
            asking(
                0,
                Class::Shards,
                Ask {
                    os: Some(os),
                    arch: Some(arch),
                    ..Ask::default()
                },
            )
        };
        assert_eq!(
            ix.verdict(NodeId(0), &guest(Os::Linux, Arch::Aarch64))
                .unwrap(),
            Verdict::Admits
        );
        assert_eq!(
            ix.verdict(NodeId(0), &guest(Os::Macos, Arch::Aarch64))
                .unwrap(),
            Verdict::NoPlatform
        );
        assert_eq!(
            ix.verdict(NodeId(0), &guest(Os::Linux, Arch::X86_64))
                .unwrap(),
            Verdict::NoPlatform
        );
        let win = asking(
            0,
            Class::Oci,
            Ask {
                os: Some(Os::Windows),
                ..Ask::default()
            },
        );
        assert_eq!(ix.verdict(NodeId(1), &win).unwrap(), Verdict::Admits);
        assert_eq!(ix.verdict(NodeId(2), &win).unwrap(), Verdict::NoPlatform);
        let mut firecracker = guest(Os::Linux, Arch::X86_64);
        firecracker.class = Class::Firecracker;
        assert_eq!(
            ix.verdict(NodeId(2), &firecracker).unwrap(),
            Verdict::Admits
        );
        firecracker.ask.os = Some(Os::Macos);
        assert_eq!(
            ix.verdict(NodeId(2), &firecracker).unwrap(),
            Verdict::NoPlatform
        );
    }

    /// CP-T7.4, the needs model's filters: do ask an image's platform, a GPU by model and by memory,
    /// and devices of each kind, of each class, on nodes of each OS; expect each node that can't give
    /// it refused for the reason it can't (another platform; no GPU of the model or with the memory;
    /// a class that can't be given the device there: Firecracker and Shards none, OCI on Windows only
    /// a GPU), each refusal counted by the walk (non-vacuity), and the node that can admitting it.
    #[test]
    fn platform_gpus_and_devices_are_matched_against_node_and_class() {
        use crate::model::{Arch, Ask, Device, DeviceKind, GpuLoad, ModelId, Os};
        const GIB: u64 = 1 << 30;
        let mut linux = gpu_node(0, 8_000, Os::Linux, &[(1, 8 * GIB, 0)]);
        linux.devices.push(Device {
            kind: DeviceKind::Vfio,
            key: None,
            available: true,
            classes: crate::model::Classes::NONE.with(crate::model::Class::Oci),
            model: ModelId::UNNAMED,
            size: 0,
            load: GpuLoad::default(),
        });
        linux.devices.push(Device {
            kind: DeviceKind::Block,
            key: None,
            available: true,
            classes: crate::model::Classes::NONE.with(crate::model::Class::Oci),
            model: ModelId::UNNAMED,
            size: 100 * GIB,
            load: GpuLoad::default(),
        });
        let windows = gpu_node(1, 8_000, Os::Windows, &[(2, 16 * GIB, 0)]);
        let mut index = FitIndex::new(&[linux, windows]).unwrap();
        let verdicts = |index: &FitIndex, d: &Demand| {
            [NodeId(0), NodeId(1)].map(|n| index.verdict(n, d).unwrap())
        };
        // The image's platform.
        let arm = asking(
            0,
            Class::Oci,
            Ask {
                arch: Some(Arch::Aarch64),
                ..Ask::default()
            },
        );
        assert_eq!(
            verdicts(&index, &arm),
            [Verdict::NoPlatform, Verdict::NoPlatform]
        );
        let win = asking(
            0,
            Class::Oci,
            Ask {
                os: Some(Os::Windows),
                ..Ask::default()
            },
        );
        assert_eq!(
            verdicts(&index, &win),
            [Verdict::NoPlatform, Verdict::Admits]
        );
        // A GPU by model, by memory, and only to OCI.
        let model1 = asking(
            1,
            Class::Oci,
            Ask {
                gpu_model: Some(ModelId(1)),
                ..Ask::default()
            },
        );
        assert_eq!(verdicts(&index, &model1), [Verdict::Admits, Verdict::NoGpu]);
        let big = asking(
            1,
            Class::Oci,
            Ask {
                gpu_memory: 12 * GIB,
                ..Ask::default()
            },
        );
        assert_eq!(verdicts(&index, &big), [Verdict::NoGpu, Verdict::Admits]);
        let shards_gpu = asking(1, Class::Shards, Ask::default());
        assert_eq!(
            verdicts(&index, &shards_gpu),
            [Verdict::NoGpu, Verdict::NoGpu]
        );
        // Devices whole: OCI on Linux alone; Windows' OCI is given only a GPU.
        let vfio = asking(
            0,
            Class::Oci,
            Ask {
                vfio: 1,
                ..Ask::default()
            },
        );
        assert_eq!(
            verdicts(&index, &vfio),
            [Verdict::Admits, Verdict::NoDevice]
        );
        for class in [Class::Firecracker, Class::Shards] {
            assert_eq!(
                index
                    .verdict(
                        NodeId(0),
                        &asking(
                            0,
                            class,
                            Ask {
                                vfio: 1,
                                ..Ask::default()
                            }
                        )
                    )
                    .unwrap(),
                Verdict::NoDevice
            );
        }
        let disk = asking(
            0,
            Class::Oci,
            Ask {
                block: 1,
                block_size: 200 * GIB,
                ..Ask::default()
            },
        );
        assert_eq!(index.verdict(NodeId(0), &disk).unwrap(), Verdict::NoDevice);
        let kvm = asking(
            0,
            Class::Oci,
            Ask {
                kvm: true,
                ..Ask::default()
            },
        );
        assert_eq!(verdicts(&index, &kvm), [Verdict::Admits, Verdict::NoDevice]);
        // Held until deleted: the one VFIO function taken, the next ask finds no room; given back, it
        // does.
        index.take(NodeId(0), WorkloadId(7), &vfio).unwrap();
        assert_eq!(index.holds(NodeId(0), WorkloadId(7)), vec![1]);
        assert_eq!(index.verdict(NodeId(0), &vfio).unwrap(), Verdict::NoRoom);
        assert_eq!(index.fit(&vfio, &Pick::default()).best, None);
        index.give(NodeId(0), WorkloadId(7), &vfio).unwrap();
        assert_eq!(index.holds(NodeId(0), WorkloadId(7)), Vec::<u16>::new());
        assert_eq!(index.verdict(NodeId(0), &vfio).unwrap(), Verdict::Admits);
        // The walk counts each refusal.
        let f = index.fit(&vfio, &Pick::default());
        assert_eq!(
            (f.best.map(|k| k.node), f.refused.device),
            (Some(NodeId(0)), 1)
        );
        let f = index.fit(&win, &Pick::default());
        assert_eq!(
            (f.best.map(|k| k.node), f.refused.platform),
            (Some(NodeId(1)), 1)
        );
        let f = index.fit(&model1, &Pick::default());
        assert_eq!(
            (f.best.map(|k| k.node), f.refused.gpu),
            (Some(NodeId(0)), 1)
        );
    }

    /// CP-T7.4, GPU load: do place a GPU box where two nodes match it, the one packing prefers (less
    /// CPU free) with its GPU 70% busy and the other 10%; expect the less-loaded GPU to win, then,
    /// the loads swapped as hex-gpu's samples would say, the other; and within a node, the idlest of
    /// its GPUs held, memory in use counting as load.
    #[test]
    fn the_less_loaded_gpu_wins() {
        use crate::model::{Ask, GpuLoad, Os};
        const GIB: u64 = 1 << 30;
        let mut index = FitIndex::new(&[
            gpu_node(0, 4_000, Os::Linux, &[(1, 8 * GIB, 700)]),
            gpu_node(1, 8_000, Os::Linux, &[(1, 8 * GIB, 100)]),
        ])
        .unwrap();
        let gpu = asking(1, Class::Oci, Ask::default());
        let cpu_only = asking(0, Class::Oci, Ask::default());
        let pick = Pick {
            scorer: Scorer::HEX,
            ..Pick::default()
        };
        // Packing alone sends work to node 0.
        assert_eq!(
            index.fit(&cpu_only, &pick).best.map(|k| k.node),
            Some(NodeId(0))
        );
        assert_eq!(index.fit(&gpu, &pick).best.map(|k| k.node), Some(NodeId(1)));
        index
            .set_gpu_load(
                NodeId(0),
                0,
                GpuLoad {
                    busy_per_mille: Some(50),
                    memory_used: Some(0),
                },
            )
            .unwrap();
        index
            .set_gpu_load(
                NodeId(1),
                0,
                GpuLoad {
                    busy_per_mille: Some(900),
                    memory_used: Some(0),
                },
            )
            .unwrap();
        assert_eq!(index.fit(&gpu, &pick).best.map(|k| k.node), Some(NodeId(0)));
        // Memory in use is load too: node 0's GPU idle but 95% of its memory in use.
        index
            .set_gpu_load(
                NodeId(0),
                0,
                GpuLoad {
                    busy_per_mille: Some(0),
                    memory_used: Some(8 * GIB / 20 * 19),
                },
            )
            .unwrap();
        assert_eq!(index.fit(&gpu, &pick).best.map(|k| k.node), Some(NodeId(1)));
        // Within a node: the idlest GPU is held.
        let mut two = FitIndex::new(&[gpu_node(
            0,
            8_000,
            Os::Linux,
            &[(1, 8 * GIB, 600), (1, 8 * GIB, 200)],
        )])
        .unwrap();
        two.take(NodeId(0), WorkloadId(1), &gpu).unwrap();
        assert_eq!(two.holds(NodeId(0), WorkloadId(1)), vec![1]);
        two.take(NodeId(0), WorkloadId(2), &gpu).unwrap();
        assert_eq!(two.holds(NodeId(0), WorkloadId(2)), vec![0]);
        two.give(NodeId(0), WorkloadId(1), &gpu).unwrap();
        assert_eq!(two.holds(NodeId(0), WorkloadId(2)), vec![0]);
        assert_eq!(two.verdict(NodeId(0), &gpu).unwrap(), Verdict::Admits);
    }

    #[test]
    fn ids_out_of_order_are_refused() {
        let mut ns = nodes(&[res(1, 1, 0), res(1, 1, 0)]);
        ns[1].id = NodeId(5);
        assert!(matches!(FitIndex::new(&ns), Err(SimError::Config(_))));
    }

    #[test]
    fn taking_more_than_is_free_is_refused_and_changes_nothing() {
        let mut index = FitIndex::new(&nodes(&[res(1000, 10, 0)])).unwrap();
        assert_eq!(
            index.take(NodeId(0), W, &plain(res(2000, 1, 0))),
            Err(SimError::Resources(NodeId(0)))
        );
        assert_eq!(index.key(NodeId(0)).unwrap().cpu_milli, 1000);
        assert_eq!(
            index.take(NodeId(3), W, &plain(res(1, 1, 0))),
            Err(SimError::UnknownNode(NodeId(3)))
        );
    }

    /// CLAUDE.md §3: do give `Shapes::of` as many incomparable shapes as its bound, then one more;
    /// expect the first taken whole and the second refused, typed, never cut to its smallest: none of
    /// them fits wherever another does, so a dropped one could leave a usable leftover read as
    /// stranded.
    #[test]
    fn more_shapes_than_the_bound_are_refused_never_cut() {
        let incomparable = |n: u64| -> Vec<(Resources, Class)> {
            (0..n)
                .map(|i| (res(1 + i, 100 - i, 0), Class::Oci))
                .collect()
        };
        let bound = u64::try_from(Shapes::MAX).unwrap();
        assert_eq!(
            Shapes::of(incomparable(bound)).unwrap().iter().count(),
            Shapes::MAX
        );
        assert_eq!(
            Shapes::of(incomparable(bound + 1)),
            Err(SimError::Full {
                what: "a sample's workload shapes",
                bound: Shapes::MAX
            })
        );
    }

    proptest::proptest! {
        /// T-T7.4.1 (oracle test): after any sequence of takes and gives of any kinds, the whole-cluster
        /// sampled fit equals the brute-force least admitting key.
        #[test]
        fn whole_fit_matches_brute_force(
            caps in proptest::collection::vec((0u64..8, 0u64..8, 0u32..3, 0u64..10), 1..12),
            ops in proptest::collection::vec((0u32..12, 0u64..4, 0u64..4, 0u32..2, 0u8..4), 0..40),
            needs in proptest::collection::vec((0u64..8, 0u64..8, 0u32..3, 0u8..4), 1..12),
        ) {
            let kind = |k: u8| [Kind::Plain, Kind::Build, Kind::Benchmark, Kind::Session][usize::from(k)];
            let mut ns = nodes(&caps.iter().map(|&(c, m, g, _)| res(c, m, g)).collect::<Vec<_>>());
            for (n, &(_, _, _, b)) in ns.iter_mut().zip(&caps) {
                n.background_milli = b;
            }
            let mut index = FitIndex::new(&ns).unwrap();
            for (node, c, m, g, k) in ops {
                let node = NodeId(node % u32::try_from(caps.len()).unwrap());
                let _ = index.take(node, W, &Demand { need: res(c, m, g), kind: kind(k), class: Class::Oci, ask: Default::default(), });
            }
            for (c, m, g, k) in needs {
                let demand = Demand { need: res(c, m, g), kind: kind(k), class: Class::Oci, ask: Default::default(), };
                proptest::prop_assert_eq!(index.fit(&demand, &Pick::default()).best, oracle(&index, &demand));
            }
        }
    }
}

#[cfg(test)]
#[path = "index/route_offer_tests.rs"]
mod route_offer_tests;

#[cfg(test)]
#[path = "index/pending_gpu_cleanup_tests.rs"]
mod pending_gpu_cleanup_tests;
