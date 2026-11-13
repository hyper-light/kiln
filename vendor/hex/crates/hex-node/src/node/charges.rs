//! Joint plan step 14 (AC-T7.4.1; `docs/design/learned-charge.md`): the node agent's charge learner, the one authority
//! on what its microVMs take, as the owner of their cgroups. Once a window (C1: `cpu.max`'s period) it reads each
//! running VM's cgroup and learns its charge and booking (`hex_sim::charge`). It then writes the placement that holds
//! the rule (C4, C5): each VM's `cpu.weight` by its booking and `memory.high` at its budget, its fleet's parent
//! (`<node root>/.vm/<node key>`) capped at the fleet's CPU budget and weighed by it on the machine's scale, and `.vm`
//! weighed by the budgets of every fleet under it. Every other child of the node's cgroup is on the same scale: each
//! process and OCI box weighs its booking and `.oci` its boxes' weights together, and the fleet's budget is what they
//! leave, so each child under contention gets at least min(its need, its booking or budget) (`Scale`). A capacity
//! answer for a VM class carries what it learned (C2, C8: `hex_api::Learned`). A Shards guest can't yet be held within
//! its room: shards-api has no call that sends init's `ROOM` (D188), and no Shards revision keeps its ring pins within
//! it, so that is refused `Awaits(GuestRoom)` and `Awaits(RingPinsWithinRoom)` while its VMM's `memory.high` holds it
//! at its budget (C7). The last resort ends a VM where it would move it, its move refused `Awaits(LiveMove)` (C5).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    Allocation, Bounds, BoxId, BoxInfo, CapacityResources, Learned, LearnedEnd,
    MAX_LEARNED_HOLDINGS, Refusal, RuntimeCost, Upstream, Usage,
};
use hex_core::cgroup::{CPU_PERIOD_US, CpuMax, Write};
use hex_core::record::BoxRecord;
use hex_runtime::linux::cgroup::BoxCgroup;
use hex_sim::charge::{self, ChargeError, ClassKey, Cpu, Excess, Sample};
use hex_sim::model::Class;

use super::Node;

/// The window the learner reads over (C1): `cpu.max`'s period, the one the kernel enforces a VM's CPU over.
pub(super) const WINDOW: Duration = Duration::from_micros(CPU_PERIOD_US);

/// One CPU in milli-CPU, the unit a vCPU and the machine's CPU are counted in.
const CPU_MILLI: u64 = 1_000;

/// A VM by its native scope: one VM's lifetime.
type Scope = [u8; 16];

/// What the learner reads and writes of cgroups: the kernel's in the node, a record's in its tests.
pub(super) trait Cgroups {
    fn usage(&self, cgroup: &BoxCgroup) -> Result<Usage, Refusal>;
    fn write(&self, cgroup: &BoxCgroup, write: &Write) -> Result<(), Refusal>;
    /// The cgroups directly under `dir`.
    fn children(&self, dir: &Path) -> Result<Vec<PathBuf>, Refusal>;
    /// A cgroup's file as the kernel says it, `None` where the cgroup or the file is none.
    fn read(&self, dir: &Path, file: &str) -> Result<Option<String>, Refusal>;
    /// Kills every process in a VM's cgroup (C5's last resort), through the descriptor its native owner pinned.
    fn kill(&self, cgroup: &BoxCgroup) -> Result<(), Refusal>;
}

/// The kernel's cgroups.
pub(super) struct Kernel;

impl Cgroups for Kernel {
    fn usage(&self, cgroup: &BoxCgroup) -> Result<Usage, Refusal> {
        cgroup.managed_usage()
    }

    fn write(&self, cgroup: &BoxCgroup, write: &Write) -> Result<(), Refusal> {
        cgroup.set(write)
    }

    fn children(&self, dir: &Path) -> Result<Vec<PathBuf>, Refusal> {
        let listing = |e| Refusal::os("listing a cgroup's children", e);
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(listing)? {
            let entry = entry.map_err(listing)?;
            if entry.file_type().map_err(listing)?.is_dir() {
                out.push(entry.path());
            }
        }
        Ok(out)
    }

    fn read(&self, dir: &Path, file: &str) -> Result<Option<String>, Refusal> {
        match std::fs::read_to_string(dir.join(file)) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Refusal::os("reading a cgroup's file", e)),
        }
    }

    fn kill(&self, cgroup: &BoxCgroup) -> Result<(), Refusal> {
        cgroup.kill()
    }
}

fn refused(why: ChargeError) -> Refusal {
    match why {
        ChargeError::Full { what, bound } => Refusal::Exhausted { what, bound },
        ChargeError::Awaits(needs) => Refusal::Awaits { needs },
        other => Refusal::Invalid {
            what: "a microVM's learned charge",
            why: other.to_string(),
        },
    }
}

/// One VM the learner reads.
struct Vm {
    id: BoxId,
    class: Class,
    /// What its receipt booked: what the ledger holds of it.
    receipt: CapacityResources,
    cgroup: BoxCgroup,
    /// Its VMM's CPU so far, in microseconds, and when it was read.
    last: (u64, Instant),
    /// What its cgroup holds as the learner last wrote it, or as its create wrote it.
    written: Vec<Write>,
}

/// The node's learner (module docs).
pub(super) struct Learner {
    node: charge::Node<Scope>,
    vms: BTreeMap<Scope, Vm>,
    /// The machine's CPU in milli-CPU: the scale every weight is on (C4).
    machine: u64,
    /// This node's fleet's parent and what the learner last wrote there.
    fleet: (BoxCgroup, Vec<Write>),
    /// `.vm`, the parent of every fleet under the node's root, and what the learner last wrote there.
    parent: (BoxCgroup, Vec<Write>),
    /// The node's root (`<node root>`), whose children are its process boxes, `.oci` and `.vm`.
    root: PathBuf,
    /// `.oci`, the parent of the node's OCI boxes.
    oci: PathBuf,
    /// The capabilities each Shards guest's room awaits, by its VM (C7).
    rooms: BTreeMap<Scope, &'static [Upstream]>,
    /// Why the last window failed: until one passes, the capacity answers carry nothing learned.
    fault: Option<Refusal>,
    /// The last resort's state (C5).
    resort: Resort,
    /// The VMs the last resort ended and why, by class, oldest first: at most `ends` of them, the oldest forgotten
    /// first, a machine never holding more VMs than its population at once.
    ended: VecDeque<(Class, LearnedEnd)>,
    ends: usize,
    /// The same ends not yet in their boxes' records, oldest first, by box and VM lifetime: the node writes each into
    /// its record after the window ([`Learner::unrecorded`]) and keeps one whose write failed for the next, at most
    /// `ends` of them, the oldest forgotten first.
    unrecorded: VecDeque<(BoxId, Scope, LearnedEnd)>,
}

/// The last resort's state from window to window (C5), explicit (R04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resort {
    /// What the fleet's VMs hold, with the fastest growth on record in one window, is within its budget.
    Held,
    /// It wasn't: the VM `id` held most past its own budget, lowest band first, and its cgroup was killed for `why`.
    /// None other is ended until its VMM is gone, so one end is never two.
    Ending {
        scope: Scope,
        id: BoxId,
        why: Excess,
    },
    /// It isn't, and the node can't act: every VM holds within its own budget, growth being no fact yet, or ending
    /// the VM the rule chose was refused. Kept until a window moves it.
    Refused(Refusal),
}

/// The most VMs a machine of `machine` could hold, each at least the least reservation placement takes
/// (`MIN_CPU_MILLI` of its CPU, `MIN_MEMORY` of its memory).
fn population(machine: CapacityResources) -> Result<usize, Refusal> {
    let cpus = machine
        .cpu_milli
        .checked_div(u64::from(hex_api::MIN_CPU_MILLI));
    let memory = machine.mem_bytes.checked_div(hex_api::MIN_MEMORY);
    cpus.zip(memory)
        .and_then(|(cpus, memory)| usize::try_from(cpus.min(memory)).ok())
        .ok_or_else(|| Refusal::Invalid {
            what: "a microVM's learned charge",
            why: "the machine's population passes a usize".into(),
        })
}

/// A VM's vCPUs: its admitted guest's CPU in whole CPUs, rounded up.
fn vcpus(allocation: Allocation) -> Result<u32, Refusal> {
    let guest = allocation.guest()?;
    u32::try_from(guest.cpu_milli.div_ceil(CPU_MILLI)).map_err(|_| Refusal::Invalid {
        what: "a microVM's learned charge",
        why: "its guest's vCPUs pass a u32".into(),
    })
}

/// Writes what `bounds` holds that `written` doesn't, in the order the kernel needs, and keeps it as written.
fn rewrite(
    io: &impl Cgroups,
    cgroup: &BoxCgroup,
    bounds: &Bounds,
    written: &mut Vec<Write>,
) -> Result<(), Refusal> {
    let writes = hex_core::cgroup::writes(bounds, &[])?;
    for write in &writes {
        if !written.contains(write) {
            io.write(cgroup, write)?;
        }
    }
    *written = writes;
    Ok(())
}

/// A fleet's CPU budget as its `cpu.max` says it, in milli-CPU: `max`, or none read, is the whole machine.
fn budget_of(max: Option<&str>, machine: u64) -> u64 {
    match max.and_then(CpuMax::parse) {
        Some(CpuMax {
            quota_us: Some(quota),
            period_us,
        }) => u128::from(quota)
            .checked_mul(u128::from(CPU_MILLI))
            .and_then(|n| n.checked_div(u128::from(period_us)))
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(machine),
        _ => machine,
    }
}

/// What holding a VM's guest within its room awaits (C7): a Shards guest needs shards-api to send init's `ROOM`
/// (D188), which it can't yet, and its ring pins (D230's long-term pinned slots, which its init makes on its own) kept
/// within that room, which p/ring-pins-v1's room-bounded pins don't do yet; a Firecracker guest has no room to be told
/// and no ring, its VMM's cgroup alone holding it.
fn room_awaits(class: Class) -> &'static [Upstream] {
    match class {
        Class::Shards => &[Upstream::GuestRoom, Upstream::RingPinsWithinRoom],
        _ => &[],
    }
}

/// What moving a VM of `class` to another machine instead of ending it awaits, where that move is another product's
/// (C5): a Shards VM saved whole and resumed elsewhere is Shards' D176, in progress. A Firecracker VM's move would be
/// Hex's own, through Firecracker's snapshot, which Hex doesn't drive yet: none to await.
fn move_awaits(class: Class) -> Option<Upstream> {
    match class {
        Class::Shards => Some(Upstream::LiveMove),
        _ => None,
    }
}

/// A VM box's inspect from its backend, with why the last resort ended it where its record says so (C5), across the
/// node's restarts.
pub(super) fn with_last_resort(info: &mut BoxInfo, record: &BoxRecord) -> Result<(), Refusal> {
    info.last_resort = record
        .last_resort
        .as_ref()
        .map(LearnedEnd::try_clone)
        .transpose()?;
    Ok(())
}

/// Writes why the last resort ended VM `id` into its record in `store`, where the record is still that VM's lifetime
/// (`scope`): false where none is, the box deleted since or made again.
pub(super) fn record_last_resort(
    store: &crate::store::Store,
    id: &BoxId,
    scope: Scope,
    end: &LearnedEnd,
) -> Result<bool, Refusal> {
    let end = end.try_clone()?;
    store.update(id, move |record| {
        if record.native_scope.as_ref().map(|s| *s.bytes()) != Some(scope) {
            return false;
        }
        record.last_resort = Some(end);
        true
    })
}

/// One of the node's process or OCI boxes, as its cgroup books it (C4).
struct Native {
    dir: PathBuf,
    booked: CapacityResources,
    /// Whether it is under `.oci`.
    oci: bool,
    /// A reuse slot no box holds: it runs nothing, so it books nothing and takes none of the scale.
    idle: bool,
}

/// Every child of the node's cgroup on the machine's scale (C4): each box weighs its booking rounded up
/// (`charge::fleet_weight`), an idle slot the least, `.oci` its boxes' weights together, and the fleets under `.vm`
/// the units those leave, whose CPU, rounded down, bounds the fleet's budget. The weights of every child that may run
/// so sum to at most the scale's top, and each child's weight carries its booking.
struct Scale {
    /// Each native's weight, in the order of the natives it was made from.
    weights: Vec<u16>,
    oci: u16,
    /// The units the boxes leave `.vm`, and the CPU they carry.
    rest: u16,
    carried: u64,
}

impl Scale {
    fn of(natives: &[Native], machine: u64) -> Result<Self, Refusal> {
        let mut weights = Vec::new();
        weights
            .try_reserve_exact(natives.len())
            .map_err(|_| Refusal::Exhausted {
                what: "the node's boxes' weights",
                bound: natives.len(),
            })?;
        let (mut root, mut oci) = (0u64, 0u64);
        for native in natives {
            let weight = if native.idle {
                charge::WEIGHT_MIN
            } else {
                charge::fleet_weight(native.booked.cpu_milli, machine).map_err(refused)?
            };
            if !native.idle {
                let sum = if native.oci { &mut oci } else { &mut root };
                *sum = sum.saturating_add(u64::from(weight));
            }
            weights.push(weight);
        }
        let weight = u16::try_from(oci.min(u64::from(charge::WEIGHT_MAX)))
            .unwrap_or(charge::WEIGHT_MAX)
            .max(charge::WEIGHT_MIN);
        // `.oci` with no box in it runs nothing, so takes none of the scale.
        let taken = if oci == 0 {
            root
        } else {
            root.saturating_add(u64::from(weight))
        };
        let (rest, carried) = charge::fleet_room(taken, machine).map_err(refused)?;
        Ok(Self {
            weights,
            oci: weight,
            rest,
            carried,
        })
    }
}

fn unreadable(file: &'static str, text: &str) -> Refusal {
    Refusal::Invalid {
        what: file,
        why: format!("{text:?} is no value the kernel writes"),
    }
}

/// A box's CPU booking from its `cpu.max`, in milli-CPU: `max`, or none, books nothing.
fn booked_cpu(max: Option<&str>) -> Result<u64, Refusal> {
    let Some(text) = max else {
        return Ok(0);
    };
    let parsed = CpuMax::parse(text).ok_or_else(|| unreadable("cpu.max", text))?;
    let Some(quota) = parsed.quota_us else {
        return Ok(0);
    };
    u128::from(quota)
        .checked_mul(u128::from(CPU_MILLI))
        .and_then(|n| n.checked_div(u128::from(parsed.period_us)))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| unreadable("cpu.max", text))
}

/// A box's memory booking from its `memory.max`, in bytes: `max`, or none, books nothing.
fn booked_bytes(max: Option<&str>) -> Result<u64, Refusal> {
    match max.map(str::trim) {
        None | Some("max") => Ok(0),
        Some(text) => text.parse().map_err(|_| unreadable("memory.max", text)),
    }
}

/// Gives the cgroup at `dir` `weight` as its `cpu.weight` where it holds another: read first, as a box's own create or
/// reuse writes its owner's weight. A cgroup gone before the write is passed over.
fn weigh(io: &impl Cgroups, dir: &Path, weight: u16) -> Result<(), Refusal> {
    let value = weight.to_string();
    match io.read(dir, "cpu.weight")? {
        None => return Ok(()),
        Some(text) if text.trim() == value => return Ok(()),
        Some(_) => {}
    }
    let write = Write {
        file: "cpu.weight",
        value,
    };
    match io.write(&BoxCgroup::at(dir.to_path_buf()), &write) {
        Ok(()) => Ok(()),
        Err(_) if io.read(dir, "cpu.weight")?.is_none() => Ok(()),
        Err(why) => Err(why),
    }
}

impl Learner {
    /// A learner for the fleet under `fleet` (`<node root>/.vm/<node key>`) on a machine of `machine`.
    pub(super) fn new(fleet: &Path, machine: CapacityResources) -> Result<Self, Refusal> {
        let parent = fleet.parent().ok_or_else(|| Refusal::Invalid {
            what: "a microVM fleet's parent cgroup",
            why: format!("{} has no parent", fleet.display()),
        })?;
        let bound = population(machine)?;
        let root = parent.parent().ok_or_else(|| Refusal::Invalid {
            what: "a node's root cgroup",
            why: format!("{} has no parent", parent.display()),
        })?;
        Ok(Self {
            node: charge::Node::new(
                WINDOW,
                machine,
                charge::Bounds {
                    vms: bound,
                    classes: bound,
                },
            )
            .map_err(refused)?,
            vms: BTreeMap::new(),
            machine: machine.cpu_milli,
            fleet: (BoxCgroup::at(fleet.to_path_buf()), Vec::new()),
            parent: (BoxCgroup::at(parent.to_path_buf()), Vec::new()),
            root: root.to_path_buf(),
            oci: root.join(super::OCI_BOXES),
            rooms: BTreeMap::new(),
            fault: None,
            resort: Resort::Held,
            ended: VecDeque::new(),
            ends: bound.min(MAX_LEARNED_HOLDINGS),
            unrecorded: VecDeque::new(),
        })
    }

    /// One window (C1 to C5, C7), read at `now`: the VMs `running`, each new one found by `record` (its class and
    /// receipt; none for a box that isn't a VM with a receipt), the reuse slots live boxes hold (`held`, in order),
    /// within the fleet's `budget`. A failure is kept until a window passes.
    pub(super) fn window(
        &mut self,
        io: &impl Cgroups,
        running: Vec<(BoxId, Scope, BoxCgroup)>,
        record: impl Fn(&BoxId) -> Result<Option<(Class, Allocation)>, Refusal>,
        held: &[u64],
        budget: CapacityResources,
        now: Instant,
    ) -> Result<(), Refusal> {
        let outcome = self.learn(io, running, record, held, budget, now);
        self.fault = outcome.as_ref().err().cloned();
        outcome
    }

    fn learn(
        &mut self,
        io: &impl Cgroups,
        running: Vec<(BoxId, Scope, BoxCgroup)>,
        record: impl Fn(&BoxId) -> Result<Option<(Class, Allocation)>, Refusal>,
        held: &[u64],
        budget: CapacityResources,
        now: Instant,
    ) -> Result<(), Refusal> {
        let present: BTreeSet<Scope> = running.iter().map(|(_, scope, _)| *scope).collect();
        let gone: Vec<Scope> = self
            .vms
            .keys()
            .filter(|scope| !present.contains(*scope))
            .copied()
            .collect();
        for scope in gone {
            self.node.end(&scope).map_err(refused)?;
            self.vms.remove(&scope);
            self.rooms.remove(&scope);
        }
        for (id, scope, cgroup) in running {
            // A VM ending between the listing and its read is read no more; the next listing ends it.
            let Ok(usage) = io.usage(&cgroup) else {
                continue;
            };
            if let Some(vm) = self.vms.get_mut(&scope) {
                let (before, at) = vm.last;
                self.node
                    .observe(
                        scope,
                        &Sample {
                            cpu: Cpu::Whole(Duration::from_micros(
                                usage.cpu_usec.saturating_sub(before),
                            )),
                            span: now.saturating_duration_since(at),
                            mem_bytes: usage.memory_bytes,
                            disk_bytes: None,
                        },
                    )
                    .map_err(refused)?;
                vm.last = (usage.cpu_usec, now);
                continue;
            }
            let Some((class, allocation)) = record(&id)? else {
                continue;
            };
            let receipt = allocation.booking();
            self.node
                .track(
                    scope,
                    ClassKey::new(class, allocation.cost.id, allocation.resources),
                    0,
                    vcpus(allocation)?,
                    receipt,
                )
                .map_err(refused)?;
            // Its create wrote its ceiling as its `cpu.max` and `memory.max` (`Node::managed_bounds`).
            let created = Bounds {
                cpu_max_milli: Some(u32::try_from(allocation.resources.cpu_milli).map_err(
                    |_| Refusal::Invalid {
                        what: "a microVM's learned charge",
                        why: "its ceiling's CPU passes cpu.max".into(),
                    },
                )?),
                memory_max: Some(allocation.resources.mem_bytes),
                ..Bounds::default()
            };
            self.vms.insert(
                scope,
                Vm {
                    id,
                    class,
                    receipt,
                    cgroup,
                    last: (usage.cpu_usec, now),
                    written: hex_core::cgroup::writes(&created, &[])?,
                },
            );
        }
        // Every child of the node's root on one scale (C4): the boxes' bookings come off the fleet's budget, and its CPU
        // is at most what the units the boxes leave carry.
        let natives = self.natives(io, held)?;
        let scale = Scale::of(&natives, self.machine)?;
        let booked = natives
            .iter()
            .try_fold(CapacityResources::default(), |sum, native| {
                sum.checked_add(native.booked)
            })
            .ok_or_else(|| Refusal::Invalid {
                what: "a node's boxes' bookings",
                why: "they overflow".into(),
            })?;
        self.node.set_budget(CapacityResources {
            cpu_milli: budget
                .cpu_milli
                .saturating_sub(booked.cpu_milli)
                .min(scale.carried),
            mem_bytes: budget.mem_bytes.saturating_sub(booked.mem_bytes),
            disk_bytes: budget.disk_bytes.saturating_sub(booked.disk_bytes),
        });
        self.node.settle().map_err(refused)?;
        self.place(io, &natives, &scale)?;
        self.last_resort(io)?;
        for (scope, vm) in &self.vms {
            let awaited = room_awaits(vm.class);
            if !awaited.is_empty() {
                self.rooms.insert(*scope, awaited);
            }
        }
        Ok(())
    }

    /// The last resort (C5), once the placement holds what it can: while a VM it ended still runs, nothing more, so one
    /// end is never two; else the core's verdict, the VM it chooses ended by killing its cgroup and why recorded, or
    /// refused, typed, where the node can't act.
    fn last_resort(&mut self, io: &impl Cgroups) -> Result<(), Refusal> {
        if let Resort::Ending { scope, .. } = &self.resort
            && self.vms.contains_key(scope)
        {
            return Ok(());
        }
        self.resort = match self.node.last_resort().map_err(refused)? {
            charge::Resort::Held => Resort::Held,
            charge::Resort::Within(fleet) => Resort::Refused(Refusal::Exhausted {
                what: "a microVM fleet's memory budget, every VM within its own",
                bound: usize::try_from(fleet.budget).unwrap_or(usize::MAX),
            }),
            charge::Resort::End { vm: scope, why } => {
                let vm = self.vms.get(&scope).ok_or_else(|| {
                    Refusal::Broken("the last resort chose a VM the learner doesn't hold".into())
                })?;
                let (class, id, cgroup) = (vm.class, vm.id.try_clone()?, vm.cgroup.clone());
                match io.kill(&cgroup) {
                    Ok(()) => {
                        self.record_end(class, &id, scope, why)?;
                        Resort::Ending { scope, id, why }
                    }
                    Err(refusal) => Resort::Refused(refusal),
                }
            }
        };
        Ok(())
    }

    /// Keeps why the last resort ended the VM `id` of lifetime `scope`, for its class's capacity answers and for its
    /// box's record, forgetting the oldest end past the bound.
    fn record_end(
        &mut self,
        class: Class,
        id: &BoxId,
        scope: Scope,
        why: Excess,
    ) -> Result<(), Refusal> {
        let end = LearnedEnd {
            id: id.try_clone()?,
            held: why.held,
            budget: why.budget,
            band: why.band,
            fleet_held: why.fleet.held,
            fleet_growth: why.fleet.growth,
            fleet_budget: why.fleet.budget,
            unmoved: move_awaits(class),
        };
        let bound = self.ends;
        let full = |what| Refusal::Exhausted { what, bound };
        if self.ended.len() >= bound {
            self.ended.pop_front();
        }
        if self.unrecorded.len() >= bound {
            self.unrecorded.pop_front();
        }
        self.ended
            .try_reserve(1)
            .map_err(|_| full("the last resort's ends"))?;
        self.unrecorded
            .try_reserve(1)
            .map_err(|_| full("the last resort's unrecorded ends"))?;
        self.unrecorded
            .push_back((id.try_clone()?, scope, end.try_clone()?));
        self.ended.push_back((class, end));
        Ok(())
    }

    /// Takes the ends not yet in their boxes' records, oldest first (C5).
    pub(super) fn unrecorded(&mut self) -> VecDeque<(BoxId, Scope, LearnedEnd)> {
        std::mem::take(&mut self.unrecorded)
    }

    /// Keeps `left`, ends whose record writes failed, for the next window, before any made since and within the bound.
    pub(super) fn keep_unrecorded(&mut self, mut left: VecDeque<(BoxId, Scope, LearnedEnd)>) {
        left.append(&mut self.unrecorded);
        while left.len() > self.ends {
            left.pop_front();
        }
        self.unrecorded = left;
    }

    /// Writes the placement (C4, C5): each VM, the fleet's parent, `.vm` by the sum of its fleets' budgets within the
    /// units the boxes leave, and each process and OCI box and `.oci` by the `scale`.
    fn place(
        &mut self,
        io: &impl Cgroups,
        natives: &[Native],
        scale: &Scale,
    ) -> Result<(), Refusal> {
        let placed = self.node.placement(self.machine).map_err(refused)?;
        for (scope, bounds) in &placed.vms {
            if let Some(vm) = self.vms.get_mut(scope) {
                rewrite(io, &vm.cgroup, bounds, &mut vm.written)?;
            }
        }
        rewrite(io, &self.fleet.0, &placed.fleet, &mut self.fleet.1)?;
        let mut fleets = 0u64;
        for path in io.children(self.parent.0.dir())? {
            let budget = if path == self.fleet.0.dir() {
                u64::from(placed.fleet.cpu_max_milli.unwrap_or(0))
            } else {
                budget_of(io.read(&path, "cpu.max")?.as_deref(), self.machine)
            };
            fleets = fleets.saturating_add(budget);
        }
        let weight = charge::fleet_weight(fleets, self.machine)
            .map_err(refused)?
            .min(scale.rest);
        let parent = Bounds {
            cpu_weight: Some(weight),
            ..Bounds::default()
        };
        rewrite(io, &self.parent.0, &parent, &mut self.parent.1)?;
        for (native, weight) in natives.iter().zip(&scale.weights) {
            weigh(io, &native.dir, *weight)?;
        }
        weigh(io, &self.oci, scale.oci)
    }

    /// The node's process and OCI boxes (C4), each booked at its caps, its cgroup's `cpu.max` and `memory.max`: the
    /// native classes offer no runtime cost (`facts::capacity`), so a native box's receipt books its caps, which its
    /// cgroup holds, a reused slot's as its box's create wrote them, until it is learned. A slot under `.oci` that no
    /// box holds (`held`, in order) is idle: it keeps its last box's caps and books nothing.
    fn natives(&self, io: &impl Cgroups, held: &[u64]) -> Result<Vec<Native>, Refusal> {
        let vm = self.parent.0.dir();
        let mut out = Vec::new();
        for (dir, oci) in [(self.root.as_path(), false), (self.oci.as_path(), true)] {
            for child in io.children(dir)? {
                if child == vm || child == self.oci {
                    continue;
                }
                let idle = oci
                    && child
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(hex_core::reuse::slot_of)
                        .is_some_and(|slot| held.binary_search(&slot).is_err());
                let booked = if idle {
                    CapacityResources::default()
                } else {
                    CapacityResources {
                        cpu_milli: booked_cpu(io.read(&child, "cpu.max")?.as_deref())?,
                        mem_bytes: booked_bytes(io.read(&child, "memory.max")?.as_deref())?,
                        disk_bytes: 0,
                    }
                };
                out.push(Native {
                    dir: child,
                    booked,
                    oci,
                    idle,
                });
            }
        }
        Ok(out)
    }

    /// The window's failure is `why`, which no read reached.
    pub(super) fn failed(&mut self, why: Refusal) {
        self.fault = Some(why);
    }

    /// What `class`'s capacity answer carries (C2, C5, C8): the priors of `costs` (its own and its routes'), the
    /// class's learned use, each of its running VMs' holdings, those most past their receipts first (memory, then CPU,
    /// then disk), and the VMs of the class the last resort ended and why, latest first. None after a failed window,
    /// so its VMs are booked whole.
    pub(super) fn learned(
        &self,
        class: Class,
        costs: &[RuntimeCost],
    ) -> Result<Option<Learned>, Refusal> {
        if self.fault.is_some() {
            return Ok(None);
        }
        let mut vms = Vec::new();
        vms.try_reserve_exact(self.vms.len())
            .map_err(|_| Refusal::Exhausted {
                what: "a class's learned holdings",
                bound: self.vms.len(),
            })?;
        for (scope, vm) in &self.vms {
            if vm.class == class {
                vms.push((*scope, &vm.id, vm.receipt));
            }
        }
        let mut ended = Vec::new();
        for (of, end) in self.ended.iter().rev() {
            if *of == class {
                ended.try_reserve(1).map_err(|_| Refusal::Exhausted {
                    what: "a class's learned ends",
                    bound: self.ends,
                })?;
                ended.push(end.try_clone()?);
            }
        }
        crate::recorded::charges::learned(&self.node, class, costs, vms, ended).map(Some)
    }

    /// The last resort's state (C5).
    #[cfg(test)]
    fn resort(&self) -> &Resort {
        &self.resort
    }

    /// The capability each guest's room awaits, by its box (C7).
    #[cfg(test)]
    fn awaits(&self) -> Vec<(&BoxId, Upstream)> {
        self.rooms
            .iter()
            .filter_map(|(scope, needs)| self.vms.get(scope).map(|vm| (vm, *needs)))
            .flat_map(|(vm, needs)| needs.iter().map(move |need| (&vm.id, *need)))
            .collect()
    }
}

impl Node {
    /// One window of the node's charge learner (joint plan step 14), its failure kept for the capacity answers.
    fn learn_window(&self) {
        let Some(shared) = self.backends.get().and_then(|b| b.learner.as_ref()) else {
            return;
        };
        let Ok(mut learner) = shared.lock() else {
            return;
        };
        let read = self.supervisor.running_vms().and_then(|running| {
            Ok((
                running,
                self.cgroups.held_slots()?,
                self.native_capacity()?.total,
            ))
        });
        let (running, held, budget) = match read {
            Ok(read) => read,
            Err(why) => return learner.failed(why),
        };
        let record = |id: &BoxId| -> Result<Option<(Class, Allocation)>, Refusal> {
            let Some(record) = self.record(id)? else {
                return Ok(None);
            };
            let class = match record.spec.class.class() {
                hex_api::Class::Shards => Class::Shards,
                hex_api::Class::Firecracker => Class::Firecracker,
                hex_api::Class::Process | hex_api::Class::Oci => return Ok(None),
            };
            Ok(record.spec.allocation.map(|allocation| (class, allocation)))
        };
        // Its outcome is kept as the learner's fault, which the capacity answers read.
        let _ = learner.window(&Kernel, running, record, &held, budget, Instant::now());
        // Each end the last resort made goes into its box's record (C5), off the learner's lock; a write that fails
        // is kept for the next window and fails this one.
        let mut ends = learner.unrecorded();
        drop(learner);
        let mut failed = None;
        ends.retain(
            |(id, scope, end)| match record_last_resort(&self.store, id, *scope, end) {
                Ok(_) => false,
                Err(why) => {
                    failed.get_or_insert(why);
                    true
                }
            },
        );
        if let Some(why) = failed
            && let Ok(mut learner) = shared.lock()
        {
            learner.keep_unrecorded(ends);
            learner.failed(why);
        }
    }

    /// What `class`'s capacity answer carries of the learner (C8), fitted to one message.
    pub(super) fn attach_learned(
        &self,
        class: hex_api::Class,
        capacity: &mut hex_api::Capacity,
    ) -> Result<(), Refusal> {
        let class = match class {
            hex_api::Class::Shards => Class::Shards,
            hex_api::Class::Firecracker => Class::Firecracker,
            hex_api::Class::Process | hex_api::Class::Oci => return Ok(()),
        };
        let Some(learner) = self.backends.get().and_then(|b| b.learner.as_ref()) else {
            return Ok(());
        };
        let mut costs = Vec::new();
        costs
            .try_reserve_exact(capacity.routes.len().saturating_add(1))
            .map_err(|_| Refusal::Exhausted {
                what: "a capacity answer's cost profiles",
                bound: capacity.routes.len(),
            })?;
        costs.extend(capacity.cost);
        costs.extend(capacity.routes.iter().filter_map(|route| route.cost));
        capacity.learned = learner
            .lock()
            .map_err(|_| Refusal::Broken("the charge learner's lock was poisoned".into()))?
            .learned(class, &costs)?;
        hex_core::wire::fit_learned(capacity).map_err(|e| Refusal::Invalid {
            what: "a capacity answer",
            why: e.to_string(),
        })
    }
}

/// Starts the node's charge learner (joint plan step 14): a thread woken each window by a monotonic timer, which ends
/// once the node is gone.
pub(super) fn spawn(node: Weak<Node>) -> Result<(), Refusal> {
    use rustix::time::{
        Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
        timerfd_settime,
    };
    let timer = timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::CLOEXEC)
        .map_err(|e| Refusal::os("making the charge learner's timer", e))?;
    let period = Timespec {
        tv_sec: i64::try_from(WINDOW.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(WINDOW.subsec_nanos()),
    };
    timerfd_settime(
        &timer,
        TimerfdTimerFlags::empty(),
        &Itimerspec {
            it_interval: period,
            it_value: period,
        },
    )
    .map_err(|e| Refusal::os("setting the charge learner's timer", e))?;
    std::thread::Builder::new()
        .name("hex-charges".to_owned())
        .spawn(move || {
            let mut expirations = [0u8; 8];
            loop {
                match rustix::io::read(&timer, &mut expirations) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return,
                }
                let Some(node) = node.upgrade() else {
                    return;
                };
                node.learn_window();
            }
        })
        .map_err(|e| Refusal::os("starting the charge learner", e))?;
    Ok(())
}

/// The learner's lock, as `Backends` holds it.
pub(super) type Shared = Mutex<Learner>;

#[cfg(test)]
mod tests;
