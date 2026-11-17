//! Joint plan step 14 (AC-T7.4.1; `docs/design/learned-charge.md`): the node agent's charge learner, the one authority
//! on what its microVMs take, as the owner of their cgroups. Once a window (C1: `cpu.max`'s period) it reads each
//! running VM's cgroup and learns its charge and booking (`hex_sim::charge`). It then writes the placement that holds
//! the rule (C4, C5): each VM's `cpu.weight` by its booking and `memory.high` at its budget, its fleet's parent
//! (`<node root>/.vm/<node key>`) capped at the fleet's CPU budget and weighed by it on the machine's scale, and `.vm`
//! weighed by the budgets of every fleet under it. Every other child of the node's cgroup is on the same scale: each
//! process and OCI box weighs its booking and `.oci` its boxes' weights together, and the fleet's budget is what they
//! leave, so each child under contention gets at least min(its need, its booking or budget) (`Scale`). A capacity
//! answer for a VM class carries what it learned (C2, C8: `hex_api::Learned`), with each VM it couldn't read and why,
//! and, where a window failed, why (place 4, A-3). A Shards guest can't yet be held within its room: shards-api has no
//! call that sends init's `ROOM` (D188), so its class's answers name it awaiting `GuestRoom` (`Learned::awaits`) while
//! its VMM's `memory.high` holds it at its budget; its ring pins keep within any room it is told (Shards' ring-pins-v1,
//! D230) (C7). The last resort ends a VM where it would move it, its move refused `Awaits(LiveMove)` (C5).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};

use hex_api::{
    Allocation, Bounds, CapacityResources, Learned, LearnedEnd, LearnedHolder,
    MAX_LEARNED_HOLDINGS, Refusal, RuntimeCost, Upstream, Usage,
};
use hex_core::cgroup::{CPU_PERIOD_US, CpuMax, Write};
use hex_runtime::guest::{GuestRuntime, ManagedGuestTime};
use hex_runtime::linux::cgroup::BoxCgroup;
use hex_sim::charge::{self, ChargeError, ClassKey, Sample};
use hex_sim::model::Class;

use super::Node;
use crate::recorded::charges::{
    Carried, Decision, LastResort, Lease, Lifetime, Reach, Scope, Writes, between, bounds, mid,
    ran, record_last_resort, timed, write_ends,
};

/// The window the learner reads over (C1): `cpu.max`'s period, the one the kernel enforces a VM's CPU over.
pub(super) const WINDOW: Duration = Duration::from_micros(CPU_PERIOD_US);

/// One CPU in milli-CPU, the unit a vCPU and the machine's CPU are counted in.
const CPU_MILLI: u64 = 1_000;

/// What the learner reads and writes of cgroups: the kernel's in the node, a record's in its tests.
pub(super) trait Cgroups {
    fn usage(&self, cgroup: &BoxCgroup) -> Result<Usage, Refusal>;
    /// VM `id`'s vCPUs' guest time since its boot, as its VM process answers its use (D260), asked before its cgroup's
    /// use so its VMM's own (the cgroup's CPU less its vCPUs') is never less than none: none apart for a VM no managed
    /// guest of the node's holds (a Firecracker VM, a node VM), whose microVM hasn't started, or whose process answers
    /// no use, why told.
    fn guest_time(&self, id: &LearnedHolder) -> Result<ManagedGuestTime, Refusal>;
    fn write(&self, cgroup: &BoxCgroup, write: &Write) -> Result<(), Refusal>;
    /// The cgroups directly under `dir`.
    fn children(&self, dir: &Path) -> Result<Vec<PathBuf>, Refusal>;
    /// A cgroup's file as the kernel says it, `None` where the cgroup or the file is none.
    fn read(&self, dir: &Path, file: &str) -> Result<Option<String>, Refusal>;
    /// Kills every process in a VM's cgroup (C5's last resort), through the descriptor its native owner pinned.
    fn kill(&self, cgroup: &BoxCgroup) -> Result<(), Refusal>;
    /// The machine's memory and disk free now, as the node's facts read them (C7; the review's A-10), read after a
    /// window's VMs and boxes: a VM's or a box's growth between the reads shrinks the fleet's budget, never swells it.
    fn free(&self) -> Result<CapacityResources, Refusal>;
    /// Writes `end`, why the last resort is ending the VM of `scope`, into box `id`'s record where it is still that VM's
    /// lifetime, before its end (C5, intent first): whether it was written.
    fn intend(&self, id: &LearnedHolder, scope: Scope, end: &LearnedEnd) -> Result<bool, Refusal>;
}

/// The kernel's cgroups, the node's records, its Shards guest runtime, where it runs one, and the node, whose facts say
/// what the machine has free.
pub(super) struct Kernel<'a> {
    pub(super) store: &'a crate::store::Store,
    pub(super) guest: Option<&'a GuestRuntime>,
    pub(super) node: &'a Node,
}

impl Cgroups for Kernel<'_> {
    fn usage(&self, cgroup: &BoxCgroup) -> Result<Usage, Refusal> {
        cgroup.managed_usage()
    }

    fn guest_time(&self, id: &LearnedHolder) -> Result<ManagedGuestTime, Refusal> {
        let unheld = |why: &str| Ok(ManagedGuestTime::Unheld(why.to_owned()));
        let LearnedHolder::Box(id) = id else {
            return unheld("a node VM's use is its run's, which no managed guest answers");
        };
        let Some(guest) = self.guest else {
            return unheld("this node runs no Shards guest runtime");
        };
        let record = match self.store.get(id)? {
            Some(Ok(record)) => record,
            Some(Err(why)) => return Err(Refusal::Broken(format!("box {id}'s record: {why}"))),
            None => return Err(Refusal::NoSuchBox(id.try_clone()?)),
        };
        if !matches!(
            record.spec.class.class(),
            hex_api::Class::Shards | hex_api::Class::ShardsImage
        ) {
            return unheld("a Firecracker VM's process answers no use");
        }
        guest.managed_guest_time(&record)
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

    fn intend(&self, id: &LearnedHolder, scope: Scope, end: &LearnedEnd) -> Result<bool, Refusal> {
        record_last_resort(self.store, id, scope, Some(end))
    }

    fn free(&self) -> Result<CapacityResources, Refusal> {
        self.node.native_capacity().map(|capacity| capacity.free)
    }
}

/// The last resort's reach on Linux: a VM's record through the node's records, its end through its cgroup's kill: the
/// VM its decision ends and that VM's cgroup, where it ends one.
struct Kills<'a, C> {
    io: &'a C,
    target: Option<(Scope, &'a BoxCgroup)>,
}

impl<C: Cgroups> Reach for Kills<'_, C> {
    fn intend(&self, id: &LearnedHolder, scope: Scope, end: &LearnedEnd) -> Result<bool, Refusal> {
        self.io.intend(id, scope, end)
    }

    fn end(&self, scope: &Scope) -> Result<(), Refusal> {
        match self.target {
            Some((target, cgroup)) if target == *scope => self.io.kill(cgroup),
            _ => Err(Refusal::Broken(
                "the last resort chose a VM the learner doesn't hold".into(),
            )),
        }
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

/// What the learner reads of a VM's record when it first meets it (C2, C3, C5): its class, its receipt, its band and
/// its place in the node's start order, and why the last resort is ending it, where its record says so.
pub(super) struct Met {
    pub(super) class: Class,
    pub(super) allocation: Allocation,
    pub(super) band: u8,
    pub(super) started: Option<u64>,
    pub(super) intent: Option<LearnedEnd>,
}

/// One VM the learner reads.
struct Vm {
    id: LearnedHolder,
    /// How the core holds it: its place in the node's start order and its scope (C3).
    lifetime: Lifetime,
    class: Class,
    /// What its receipt booked: what the ledger holds of it.
    receipt: CapacityResources,
    cgroup: BoxCgroup,
    /// Its VMM's CPU so far, in microseconds, and the instant it holds at: its VM process's sample of its guest time, or
    /// its cgroup's read where none came (the review's A-11).
    last: (u64, Instant),
    /// Each vCPU's guest time at that reading, where its VM process answered it (D260).
    guest_ns: Option<Vec<u64>>,
    /// What its cgroup holds as the learner last wrote it, or as its create wrote it.
    written: Vec<Write>,
    /// The CPU cap its cgroup holds it at, in milli-CPU (`cpu.max`), as last written: a window that reaches it is no
    /// sample of its VMM's own CPU (C1).
    cap: Option<u64>,
    /// The memory its cgroup holds it at, as last written: its create's `memory.max`, then its placement's
    /// `memory.high` (C5).
    budget: Option<u64>,
    /// Why its last window couldn't write its cgroup (A-7; ROOT, 2026-11-14): while it couldn't, it is held at what was
    /// last written, counted at `budget`, and written again each window its placement moves.
    unwritten: Option<Refusal>,
}

/// The node's learner (module docs).
pub(super) struct Learner {
    node: charge::Node<Lifetime>,
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
    /// The last resort (C5), which ends a VM by killing its cgroup.
    last: LastResort,
    /// The VMs its last window couldn't read and why, each counted whole against the fleet's budget (ROOT, 2026-11-13).
    unread: Vec<(LearnedHolder, Option<Class>, Refusal)>,
    /// What it takes off its fleet's budget for the capacity view's holders, as its node last said (C7; the review's
    /// A-10): none before its first window.
    reservations: Option<hex_api::LearnedReservations>,
}

/// A VM's vCPUs: its admitted guest's CPU in whole CPUs, rounded up.
fn vcpus(allocation: Allocation) -> Result<u32, Refusal> {
    let guest = allocation.guest()?;
    u32::try_from(guest.cpu_milli.div_ceil(CPU_MILLI)).map_err(|_| Refusal::Invalid {
        what: "a microVM's learned charge",
        why: "its guest's vCPUs pass a u32".into(),
    })
}

/// What a cgroup's `bounds` ask of it past what it holds as last written (C4, C5): the writes to make, in the order the
/// kernel needs, and what it holds once they're made.
struct Rewrite {
    writes: Vec<Write>,
    written: Vec<Write>,
}

/// The writes `bounds` asks of a cgroup that `written` doesn't hold; a file written before that `bounds` sets no more
/// goes back to the kernel's default.
fn rewrite(bounds: &Bounds, written: &[Write]) -> Result<Rewrite, Refusal> {
    let writes = hex_core::cgroup::writes(bounds, &[])?;
    // A file written before that `bounds` sets no more goes back to the kernel's default first, before any bound is
    // raised past it: a VM's `memory.max` its create wrote is lifted, with the `memory.swap.max` it came with (C5).
    let defaults = hex_core::cgroup::writes_all(bounds, &[])?;
    let mut make = Vec::new();
    for before in written {
        if writes.iter().any(|write| write.file == before.file) {
            continue;
        }
        if let Some(default) = defaults.iter().find(|default| default.file == before.file) {
            make.push(default.clone());
        }
    }
    for write in &writes {
        if !written.contains(write) {
            make.push(write.clone());
        }
    }
    Ok(Rewrite {
        writes: make,
        written: writes,
    })
}

/// Makes a [`Rewrite`]'s writes to `cgroup`, in order, to the first that fails (outside the learner's lock; A-2).
fn make(io: &impl Cgroups, cgroup: &BoxCgroup, rewrite: &Rewrite) -> Result<(), Refusal> {
    rewrite
        .writes
        .iter()
        .try_for_each(|write| io.write(cgroup, write))
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
/// (D188), which it can't yet. Its ring pins (D230's long-term pinned slots, which its init makes on its own) come out of
/// whatever room init is told, or of the guest's available memory where it is told none: Shards' ring-pins-v1, which
/// Hex vendors at 66113379, so they await nothing more. A Firecracker guest has no room to be told and no ring, its
/// VMM's cgroup alone holding it.
fn room_awaits(class: Class) -> &'static [Upstream] {
    match class {
        Class::Shards | Class::ShardsImage => &[Upstream::GuestRoom],
        Class::Oci | Class::Firecracker => &[],
    }
}

/// One of the node's process or OCI boxes, as its cgroup books it (C4).
struct Native {
    dir: PathBuf,
    booked: CapacityResources,
    /// What it takes of the machine's free memory (C7; the review's A-10), its `memory.current` less its reclaimable file
    /// pages ([`taken`]): none where either can't be read.
    taken: Option<u64>,
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

/// A cgroup's reclaimable file pages, its `memory.stat`'s `active_file` and `inactive_file`, which the machine's
/// `MemAvailable` counts free already (`Documentation/filesystems/proc.rst`; cgroup-v2.rst, `memory.stat`): none where
/// `stat` names either not, or not as a count.
fn file_pages(stat: Option<&str>) -> Option<u64> {
    let field = |name: &str| {
        stat?.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then(|| value.trim().parse::<u64>().ok())?
        })
    };
    field("active_file")?.checked_add(field("inactive_file")?)
}

/// What a cgroup takes of the machine's free memory (C7; the review's A-10): its `memory.current` less its reclaimable
/// file pages ([`file_pages`]), counted free already; none where either can't be read, so a box whose use is unknown
/// keeps its whole booking off the fleet's budget.
fn taken(current: Option<&str>, stat: Option<&str>) -> Option<u64> {
    let current = current?.trim().parse::<u64>().ok()?;
    Some(current.saturating_sub(file_pages(stat)?))
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
        let bounds = bounds(machine)?;
        let root = parent.parent().ok_or_else(|| Refusal::Invalid {
            what: "a node's root cgroup",
            why: format!("{} has no parent", parent.display()),
        })?;
        Ok(Self {
            node: charge::Node::new(Some(WINDOW), machine, bounds).map_err(refused)?,
            vms: BTreeMap::new(),
            machine: machine.cpu_milli,
            fleet: (BoxCgroup::at(fleet.to_path_buf()), Vec::new()),
            parent: (BoxCgroup::at(parent.to_path_buf()), Vec::new()),
            root: root.to_path_buf(),
            oci: root.join(super::OCI_BOXES),
            rooms: BTreeMap::new(),
            fault: None,
            last: LastResort::new(bounds.vms.min(MAX_LEARNED_HOLDINGS)),
            unread: Vec::new(),
            reservations: None,
        })
    }

    /// What it takes off its fleet's budget for the capacity view's holders, as its node says it each window (C7; the
    /// review's A-10), told on its classes' capacity answers.
    pub(super) fn told_reservations(&mut self, reservations: hex_api::LearnedReservations) {
        self.reservations = Some(reservations);
    }

    /// The VMs its last window couldn't read, and why: each counted whole, at its receipt, against the fleet's budget
    /// until a read succeeds (joint plan step 14; ROOT, 2026-11-13).
    #[cfg(test)]
    pub(super) fn unread(&self) -> &[(LearnedHolder, Option<Class>, Refusal)] {
        &self.unread
    }

    /// Runs a window as the node does ([`window`]), the learner held alone, within the fleet's `budget` as given, less
    /// what the node's boxes book ([`given`]).
    #[cfg(test)]
    pub(super) fn window(
        &mut self,
        io: &impl Cgroups,
        running: Vec<(LearnedHolder, Scope, BoxCgroup)>,
        record: impl Fn(&LearnedHolder) -> Result<Option<Met>, Refusal>,
        held: &[u64],
        budget: CapacityResources,
        now: Instant,
    ) -> Result<(), Refusal> {
        window(
            self,
            io,
            running,
            record,
            held,
            |reads| given(budget, reads),
            || now,
        )
    }

    /// What a window asks of the node over the VMs `running` (A-2): each VM's guest time and its cgroup's use, and its
    /// record where it meets it; and where the node's boxes and the fleets beside its own are listed.
    fn asks(&self, running: Vec<(LearnedHolder, Scope, BoxCgroup)>) -> Result<Asks, Refusal> {
        let mut vms = Vec::new();
        vms.try_reserve_exact(running.len())
            .map_err(|_| Refusal::Exhausted {
                what: "a window's VMs",
                bound: running.len(),
            })?;
        for (id, scope, cgroup) in running {
            let known = self.vms.contains_key(&scope);
            vms.push(Asked {
                id,
                scope,
                cgroup,
                known,
            });
        }
        Ok(Asks {
            vms,
            root: self.root.clone(),
            oci: self.oci.clone(),
            vm: self.parent.0.dir().to_path_buf(),
            fleet: self.fleet.0.dir().to_path_buf(),
        })
    }

    /// Learns a window's `reads`, each at its own reads' instants (C1 to C5, C7; under its lock, A-2; the review's A-11),
    /// within the fleet's `budget`: the placement's writes and its last resort's decision, which it then acts on outside
    /// its lock ([`act`]).
    fn learn(&mut self, reads: Reads, budget: CapacityResources) -> Result<Acts, Refusal> {
        let present: BTreeSet<Scope> = reads.vms.iter().map(|answer| answer.scope).collect();
        let gone: Vec<Scope> = self
            .vms
            .keys()
            .filter(|scope| !present.contains(*scope))
            .copied()
            .collect();
        for scope in gone {
            if let Some(vm) = self.vms.get(&scope) {
                self.node.end(&vm.lifetime).map_err(refused)?;
            }
            self.vms.remove(&scope);
            self.rooms.remove(&scope);
            self.last.gone(&scope);
        }
        // What the VMs it couldn't read hold past their bookings, their receipts whole.
        let mut unread = CapacityResources::default();
        self.unread.clear();
        for Answer {
            id,
            scope,
            cgroup,
            read,
            file: _,
            met,
        } in reads.vms
        {
            // A VM whose cgroup can't be read counts whole, at its receipt, against the fleet's budget this window, its
            // learner untouched (ROOT, 2026-11-13: never starving its neighbours by holding nothing): no sample taken,
            // its booking as it was, and the failure told (no holding on the capacity answers, so the scheduler counts
            // it at its receipt). One that ended since its listing is ended by the next. D260: either of its guest
            // time's and its cgroup's reads failing counts the VM whole, as above.
            let (guest, usage, read_at) = match read {
                Ok(read) => read,
                Err(why) => {
                    // Its class, where its record names one, which class's answers tell it (place 4).
                    let met = met.transpose()?.flatten();
                    let class = met.as_ref().map(|met| met.class);
                    if let Some(met) = met {
                        let booked = self
                            .vms
                            .get(&scope)
                            .and_then(|vm| self.node.booking(&vm.lifetime))
                            .unwrap_or_default();
                        let past = CapacityResources {
                            cpu_milli: met
                                .allocation
                                .resources
                                .cpu_milli
                                .saturating_sub(booked.cpu_milli),
                            mem_bytes: met
                                .allocation
                                .resources
                                .mem_bytes
                                .saturating_sub(booked.mem_bytes),
                            disk_bytes: met
                                .allocation
                                .resources
                                .disk_bytes
                                .saturating_sub(booked.disk_bytes),
                        };
                        unread = unread.checked_add(past).ok_or_else(|| Refusal::Invalid {
                            what: "a microVM's learned charge",
                            why: "the unread VMs' receipts overflow".into(),
                        })?;
                    }
                    self.unread.try_reserve(1).map_err(|_| Refusal::Exhausted {
                        what: "the charge learner's unread VMs",
                        bound: MAX_LEARNED_HOLDINGS,
                    })?;
                    self.unread.push((id, class, why));
                    continue;
                }
            };
            let (guest_ns, charged) = match guest {
                ManagedGuestTime::PerVcpu { ns, charged, .. } => (Some(ns), charged),
                ManagedGuestTime::Unheld(_) => (None, hex_api::Charged::Unknown),
            };
            if let Some(vm) = self.vms.get_mut(&scope) {
                let (before, at) = vm.last;
                // Split where both readings hold its vCPUs' guest time (D260), its VMM's own the cgroup's less theirs, as
                // every OS's learner reads it; whole where either doesn't.
                let cpu = ran(
                    usage.cpu_usec.saturating_sub(before).saturating_mul(1_000),
                    guest_ns.as_deref(),
                    vm.guest_ns.as_deref(),
                    charged,
                )
                .map_err(refused)?;
                self.node
                    .observe(
                        vm.lifetime,
                        &Sample {
                            cpu,
                            // Over the span between its own reads (the review's A-11).
                            span: read_at.saturating_duration_since(at),
                            mem_bytes: usage.memory_bytes,
                            disk_bytes: None,
                            cap_milli: vm.cap,
                        },
                    )
                    .map_err(refused)?;
                vm.last = (usage.cpu_usec, read_at);
                vm.guest_ns = guest_ns;
                continue;
            }
            let Some(Met {
                class,
                allocation,
                band,
                started,
                intent,
            }) = met.transpose()?.flatten()
            else {
                continue;
            };
            let receipt = allocation.booking();
            let lifetime = Lifetime::of(scope, started);
            self.node
                .track(
                    lifetime,
                    ClassKey::of(class, allocation)?,
                    band,
                    vcpus(allocation)?,
                    receipt,
                )
                .map_err(refused)?;
            // What it holds as it is met is its baseline, never a window's growth (ROOT, 2026-11-13).
            self.node
                .baseline(&lifetime, usage.memory_bytes)
                .map_err(refused)?;
            // Its create wrote its ceiling's CPU as its `cpu.max` (`Node::managed_bounds`); a VM made before ruling B
            // also has a `memory.max`, taken as written so the first window lifts it. Its `memory.swap.max` stays off,
            // never the learner's to lift (HEX-RT-15).
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
                    lifetime,
                    class,
                    receipt,
                    cgroup,
                    last: (usage.cpu_usec, read_at),
                    guest_ns,
                    written: hex_core::cgroup::writes(&created, &[])?
                        .into_iter()
                        .filter(|write| write.file != "memory.swap.max")
                        .collect(),
                    cap: created.cpu_max_milli.map(u64::from),
                    budget: created.memory_max,
                    unwritten: None,
                },
            );
            self.last.met(class, scope, intent)?;
        }
        // Every child of the node's root on one scale (C4): its CPU is at most what the units the boxes leave carry, the
        // boxes' bookings off its `budget` already ([`measured`]).
        let natives = reads.natives?;
        let scale = Scale::of(&natives, self.machine)?;
        // The fleet's budget this window, less what its unread VMs hold past their bookings.
        self.node.set_budget(CapacityResources {
            cpu_milli: budget
                .cpu_milli
                .saturating_sub(unread.cpu_milli)
                .min(scale.carried),
            mem_bytes: budget.mem_bytes.saturating_sub(unread.mem_bytes),
            disk_bytes: budget.disk_bytes.saturating_sub(unread.disk_bytes),
        });
        // A VM whose cgroup its last window couldn't write is held at what was last written (A-7; ROOT, 2026-11-14,
        // ruling (ii)): counted at the memory its cgroup holds it at, the others' budgets shared from what it leaves,
        // as every OS's learner counts a VM held at its last cap. Its CPU is counted already: its `cpu.max` is its
        // ceiling, never a share, and the fleet's `cpu.max` holds every VM's CPU, its own too.
        let held = self
            .vms
            .values()
            .filter(|vm| vm.unwritten.is_some())
            .map(|vm| {
                let held = charge::Held {
                    cpu_milli: None,
                    mem_bytes: vm.budget,
                };
                (vm.lifetime, held)
            })
            .collect();
        self.node.set_held(held);
        self.node.settle().map_err(refused)?;
        self.placing(natives, &scale, reads.fleets)
    }

    /// The placement's writes (C4, C5): each VM's, the fleet's parent's, and `.vm`'s by the sum of its fleets' budgets
    /// within the units the boxes leave (`fleets`, the others' as their `cpu.max` say them); each process and OCI box
    /// and `.oci` weighed by the `scale`; and the last resort's decision, once the placement holds what it can. A cgroup
    /// that holds what its bounds ask already is kept as written, with nothing to write.
    fn placing(
        &mut self,
        natives: Vec<Native>,
        scale: &Scale,
        fleets: Result<Fleets, Refusal>,
    ) -> Result<Acts, Refusal> {
        let placed = self.node.placement(self.machine).map_err(refused)?;
        let mut vms = Vec::new();
        for (lifetime, bounds) in &placed.vms {
            let Some(vm) = self.vms.get_mut(&lifetime.scope) else {
                continue;
            };
            let rewrite = rewrite(bounds, &vm.written)?;
            let cap = bounds.cpu_max_milli.map(u64::from);
            if rewrite.writes.is_empty() {
                vm.written = rewrite.written;
                vm.cap = cap;
                vm.budget = bounds.memory_high;
                vm.unwritten = None;
                continue;
            }
            vms.try_reserve(1).map_err(|_| Refusal::Exhausted {
                what: "a window's cgroup writes",
                bound: placed.vms.len(),
            })?;
            vms.push(Placing {
                scope: lifetime.scope,
                cgroup: vm.cgroup.clone(),
                rewrite,
                cap,
                budget: bounds.memory_high,
            });
        }
        let fleet = rewrite(&placed.fleet, &self.fleet.1)?;
        let fleet = if fleet.writes.is_empty() {
            self.fleet.1 = fleet.written;
            None
        } else {
            Some((self.fleet.0.clone(), fleet))
        };
        let machine = self.machine;
        let parent = fleets.and_then(|fleets| {
            let own = if fleets.own {
                u64::from(placed.fleet.cpu_max_milli.unwrap_or(0))
            } else {
                0
            };
            let all = fleets.others.iter().fold(own, |sum, max| {
                sum.saturating_add(budget_of(max.as_deref(), machine))
            });
            let weight = charge::fleet_weight(all, machine)
                .map_err(refused)?
                .min(scale.rest);
            let bounds = Bounds {
                cpu_weight: Some(weight),
                ..Bounds::default()
            };
            rewrite(&bounds, &self.parent.1)
        });
        let parent = match parent {
            Ok(rewrite) if rewrite.writes.is_empty() => {
                self.parent.1 = rewrite.written;
                Ok(None)
            }
            Ok(rewrite) => Ok(Some((self.parent.0.clone(), rewrite))),
            Err(why) => Err(why),
        };
        let mut weights = Vec::new();
        weights
            .try_reserve_exact(natives.len())
            .map_err(|_| Refusal::Exhausted {
                what: "the node's boxes' weights",
                bound: natives.len(),
            })?;
        for (native, weight) in natives.into_iter().zip(&scale.weights) {
            weights.push((native.dir, *weight));
        }
        let held = &self.vms;
        let resort = self.last.decide(&self.node, |scope| {
            held.get(scope).map(|vm| (vm.class, &vm.id))
        })?;
        let target = match &resort {
            Decision::Due { scope, .. } | Decision::End { scope, .. } => {
                self.vms.get(scope).map(|vm| (*scope, vm.cgroup.clone()))
            }
            Decision::Wait | Decision::Set(_) => None,
        };
        Ok(Acts {
            vms,
            fleet,
            parent,
            weights,
            oci: (self.oci.clone(), scale.oci),
            resort,
            target,
        })
    }

    /// Keeps what a window's acts did (under its lock; A-2), or why its learning failed, and its outcome, which is kept
    /// until a window passes.
    fn acted(&mut self, acted: Result<Acted, Refusal>) -> Result<(), Refusal> {
        let outcome = acted.and_then(|acted| self.keep(acted));
        self.fault = outcome.as_ref().err().cloned();
        outcome
    }

    /// Keeps what each cgroup holds once a window's acts wrote it, each VM whose cgroup couldn't be written held at
    /// what was last written, and why (A-7), and what its last resort did, to the first act not one VM's that failed,
    /// whose refusal is the window's; then what each Shards guest's room awaits (C7).
    fn keep(&mut self, acted: Acted) -> Result<(), Refusal> {
        for written in acted.vms {
            if let Some(vm) = self.vms.get_mut(&written.scope) {
                vm.written = written.written;
                vm.cap = written.cap;
                vm.budget = written.budget;
                vm.unwritten = None;
            }
        }
        for (scope, why) in acted.unwritten {
            if let Some(vm) = self.vms.get_mut(&scope) {
                vm.unwritten = Some(why);
            }
        }
        if let Some(written) = acted.fleet {
            self.fleet.1 = written;
        }
        if let Some(written) = acted.parent {
            self.parent.1 = written;
        }
        if let Some(why) = acted.failed {
            return Err(why);
        }
        if let Some(carried) = acted.resort {
            self.last.settle(carried)?;
        }
        for (scope, vm) in &self.vms {
            let awaited = room_awaits(vm.class);
            if !awaited.is_empty() {
                self.rooms.insert(*scope, awaited);
            }
        }
        Ok(())
    }

    /// Takes the writes into VMs' records not yet made, oldest first (C5).
    pub(super) fn unrecorded(&mut self) -> Writes {
        self.last.unrecorded()
    }

    /// Keeps `left`, writes that failed, for the next window (C5).
    pub(super) fn keep_unrecorded(&mut self, left: Writes) {
        self.last.keep_unrecorded(left);
    }

    /// Carries the last-resort ends its node's start read back from node VM rows into its ends (joint plan step 39).
    pub(super) fn carry_ended(&mut self, ends: Vec<LearnedEnd>) -> Result<(), Refusal> {
        self.last.carry(ends)
    }

    /// The window's failure is `why`, which no read reached.
    pub(super) fn failed(&mut self, why: Refusal) {
        self.fault = Some(why);
    }

    /// Why its classes' capacity answers carry nothing learned (A-3): its last window's refusal, in its words.
    pub(super) fn unlearned(&self) -> Option<String> {
        self.fault.as_ref().map(ToString::to_string)
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
        for vm in self.vms.values() {
            // One its last window couldn't read carries no holding, so the scheduler counts it at its receipt (C8).
            if vm.class == class && !self.unread.iter().any(|(id, _, _)| *id == vm.id) {
                vms.push((vm.lifetime, &vm.id, vm.receipt));
            }
        }
        // Its window, `cpu.max`'s period, the one the kernel holds every VM's cap over (C1): no cap's cycle sets it.
        let window = hex_api::LearnedWindow {
            window_ns: u64::try_from(WINDOW.as_nanos()).unwrap_or(u64::MAX),
            slowed_by: None,
        };
        let mut learned = crate::recorded::charges::learned(
            &self.node,
            class,
            costs,
            vms,
            self.last.ended(class)?,
            Some(window),
        )?;
        // What it couldn't read and why, and what each guest's room of the class awaits, in its VMs' order (place 4).
        for (id, of, why) in &self.unread {
            if *of != Some(class) {
                continue;
            }
            learned
                .unread
                .try_reserve(1)
                .map_err(|_| Refusal::Exhausted {
                    what: "learned unread VMs",
                    bound: MAX_LEARNED_HOLDINGS,
                })?;
            learned.unread.push(hex_api::LearnedUnread {
                id: id.try_clone()?,
                why: why.to_string(),
            });
        }
        for (scope, needs) in &self.rooms {
            let Some(vm) = self.vms.get(scope).filter(|vm| vm.class == class) else {
                continue;
            };
            for needs in needs.iter() {
                learned
                    .awaits
                    .try_reserve(1)
                    .map_err(|_| Refusal::Exhausted {
                        what: "learned awaits",
                        bound: MAX_LEARNED_HOLDINGS,
                    })?;
                learned.awaits.push(hex_api::LearnedAwait {
                    id: vm.id.try_clone()?,
                    needs: *needs,
                });
            }
        }
        // Each VM of the class whose cgroup its window couldn't write, held at what was last written, and why (A-7).
        for vm in self.vms.values() {
            let Some(why) = vm.unwritten.as_ref().filter(|_| vm.class == class) else {
                continue;
            };
            learned
                .unacted
                .try_reserve(1)
                .map_err(|_| Refusal::Exhausted {
                    what: "learned unacted VMs",
                    bound: MAX_LEARNED_HOLDINGS,
                })?;
            learned.unacted.push(hex_api::LearnedUnacted {
                id: vm.id.try_clone()?,
                why: format!(
                    "its cgroup couldn't be written: {why}; held at what was last written"
                ),
            });
        }
        learned.reservations = self.reservations;
        Ok(Some(learned))
    }

    /// The last resort's state (C5).
    #[cfg(test)]
    fn resort(&self) -> &crate::recorded::charges::Resort {
        self.last.resort()
    }

    /// The capability each guest's room awaits, by its box (C7).
    #[cfg(test)]
    fn awaits(&self) -> Vec<(&LearnedHolder, Upstream)> {
        self.rooms
            .iter()
            .filter_map(|(scope, needs)| self.vms.get(scope).map(|vm| (vm, *needs)))
            .flat_map(|(vm, needs)| needs.iter().map(move |need| (&vm.id, *need)))
            .collect()
    }
}

/// A VM a window reads (A-2): its holder, its lifetime, its cgroup, and whether its learner holds it already.
struct Asked {
    id: LearnedHolder,
    scope: Scope,
    cgroup: BoxCgroup,
    known: bool,
}

/// What a window asks of the node (A-2), decided under its learner's lock: each VM it reads, and where the node's boxes
/// and the fleets beside its own are listed: its root, `.oci`, `.vm` and its fleet.
struct Asks {
    vms: Vec<Asked>,
    root: PathBuf,
    oci: PathBuf,
    vm: PathBuf,
    fleet: PathBuf,
}

/// What one VM answered a window (C1, D260): its guest time and its cgroup's use, or why not; and its record, where its
/// learner meets it or can't read it.
struct Answer {
    id: LearnedHolder,
    scope: Scope,
    cgroup: BoxCgroup,
    /// Its guest time and its cgroup's use, its CPU taken at its VM process's sample, and the instant that holds at (the
    /// review's A-11).
    read: Result<(ManagedGuestTime, Usage, Instant), Refusal>,
    /// Its reclaimable file pages, which the machine's free memory counts already ([`file_pages`]): none where its
    /// `memory.stat` can't be read (the review's A-10).
    file: Option<u64>,
    met: Option<Result<Option<Met>, Refusal>>,
}

/// The fleets under `.vm` beside a node's own (C4): whether its own is among them, and each other's `cpu.max` as the
/// kernel says it.
struct Fleets {
    own: bool,
    others: Vec<Option<String>>,
}

/// What a window read with its learner let go (A-2): each VM's answer, the node's boxes at their caps, the fleets under
/// `.vm`, and the machine's memory and disk free, read last (the review's A-10).
struct Reads {
    vms: Vec<Answer>,
    natives: Result<Vec<Native>, Refusal>,
    fleets: Result<Fleets, Refusal>,
    free: Result<CapacityResources, Refusal>,
}

/// A VM's cgroup the placement rewrites (C4, C5), and the CPU cap and memory it holds the VM at once its writes are
/// made.
struct Placing {
    scope: Scope,
    cgroup: BoxCgroup,
    rewrite: Rewrite,
    cap: Option<u64>,
    budget: Option<u64>,
}

/// A VM's cgroup a window wrote (C4, C5): what it holds now, and the CPU cap and memory it holds the VM at.
struct Written {
    scope: Scope,
    written: Vec<Write>,
    cap: Option<u64>,
    budget: Option<u64>,
}

/// What a window acts on once its learner has learned (C4, C5), outside its lock (A-2), in order: each VM's cgroup the
/// placement rewrites, the fleet's, `.vm`'s or why its fleets couldn't be read, each box's weight and `.oci`'s, and the
/// last resort's decision with the cgroup of the VM it ends, where it ends one.
struct Acts {
    vms: Vec<Placing>,
    fleet: Option<(BoxCgroup, Rewrite)>,
    parent: Result<Option<(BoxCgroup, Rewrite)>, Refusal>,
    weights: Vec<(PathBuf, u16)>,
    oci: (PathBuf, u16),
    resort: Decision,
    target: Option<(Scope, BoxCgroup)>,
}

/// What a window's acts did, in order, to the first act not one VM's that failed: what each cgroup holds once its
/// writes were made, with a VM's cap and memory, each VM whose cgroup couldn't be written and why (A-7), and its last
/// resort carried out, where the acts reached it.
#[derive(Default)]
struct Acted {
    vms: Vec<Written>,
    unwritten: Vec<(Scope, Refusal)>,
    fleet: Option<Vec<Write>>,
    parent: Option<Vec<Write>>,
    resort: Option<Carried>,
    failed: Option<Refusal>,
}

/// One window of the node's learner (C1 to C5, C7), read at `now`: the VMs `running`, each new one found by `record`
/// (its class, receipt, band and any intent; none for a box that isn't a VM with a receipt), the reuse slots live boxes
/// hold (`held`, in order), within the fleet's budget, which `budget` gives from what the window read ([`measured`]). Its
/// learner is taken for each of the window's pure steps and let go while the node's cgroups and records are read and
/// written and its VMs asked and ended (A-2; the review's, 2026-11-14), so a capacity answer, which takes it, never waits
/// on them. Its node runs one window at a time, on its learner's thread, so nothing but the window moves its learner
/// between its steps. A failure is kept until a window passes.
fn window(
    mut learner: impl Lease<Learner>,
    io: &impl Cgroups,
    running: Vec<(LearnedHolder, Scope, BoxCgroup)>,
    record: impl Fn(&LearnedHolder) -> Result<Option<Met>, Refusal>,
    held: &[u64],
    budget: impl FnOnce(&Reads) -> Result<CapacityResources, Refusal>,
    clock: impl Fn() -> Instant,
) -> Result<(), Refusal> {
    let asks = learner.take()?.asks(running);
    let read = asks
        .and_then(|asks| ask(io, asks, &record, held, &clock))
        .and_then(|reads| Ok((budget(&reads)?, reads)));
    let learned = match read {
        Ok((budget, reads)) => learner.take()?.learn(reads, budget),
        Err(why) => Err(why),
    };
    let acted = learned.and_then(|acts| act(io, acts));
    learner.take()?.acted(acted)
}

/// The fleet's budget a window measures (C4, C7; the review's A-10), from what it read (`reads`): every CPU of the
/// `machine` less what the node's boxes book; and the memory and disk the machine has free, read last, with what its VMs
/// take of it, less what each box may still grow into, its booking past what it takes. What a box takes is missing from
/// what is free already, so only that headroom comes off, never its booking beside its use (ROOT, 2026-11-14), and one
/// past its booking takes the excess from the fleet. A VM's and a box's reclaimable file pages, which `MemAvailable`
/// counts free already, are counted there alone: a VM gives back its memory less them, a box takes its use less them;
/// one whose file pages can't be read gives back nothing, or takes off its whole booking. A VM whose read failed gives
/// back nothing, its receipt counted whole instead ([`Learner::learn`]). What the capacity view's holders have reserved, as
/// its keeper last said (`reserved`, none where none is counted), comes off every dimension: kiln's runs' use is missing
/// from what is free too, so it is counted twice, an under-count and never a credit, until kiln's holder reports what its
/// runs use (C7). The budget is so never more than what is free and what its VMs take.
fn measured(
    machine: CapacityResources,
    reserved: CapacityResources,
    reads: &Reads,
) -> Result<CapacityResources, Refusal> {
    let natives = reads.natives.as_ref().map_err(Clone::clone)?;
    let free = reads.free.as_ref().map_err(Clone::clone)?;
    let overflow = || Refusal::Invalid {
        what: "a fleet's budget",
        why: "what is free, what its VMs take and what its boxes book overflow".into(),
    };
    let mut held = 0u64;
    for answer in &reads.vms {
        if let (Ok((_, usage, _)), Some(file)) = (&answer.read, answer.file) {
            held = held
                .checked_add(usage.memory_bytes.saturating_sub(file))
                .ok_or_else(overflow)?;
        }
    }
    let mut off = CapacityResources::default();
    for native in natives {
        let headroom = CapacityResources {
            cpu_milli: native.booked.cpu_milli,
            mem_bytes: native
                .booked
                .mem_bytes
                .saturating_sub(native.taken.unwrap_or(0)),
            disk_bytes: native.booked.disk_bytes,
        };
        off = off.checked_add(headroom).ok_or_else(overflow)?;
    }
    let memory = free.mem_bytes.checked_add(held).ok_or_else(overflow)?;
    Ok(CapacityResources {
        cpu_milli: machine
            .cpu_milli
            .saturating_sub(off.cpu_milli)
            .saturating_sub(reserved.cpu_milli),
        mem_bytes: memory
            .saturating_sub(off.mem_bytes)
            .saturating_sub(reserved.mem_bytes),
        disk_bytes: free
            .disk_bytes
            .saturating_sub(off.disk_bytes)
            .saturating_sub(reserved.disk_bytes),
    })
}

/// A window's fleet budget as a test gives it, `budget`, less what the node's boxes book, each at its caps, none below
/// nothing.
#[cfg(test)]
fn given(budget: CapacityResources, reads: &Reads) -> Result<CapacityResources, Refusal> {
    let natives = reads.natives.as_ref().map_err(Clone::clone)?;
    let booked = natives
        .iter()
        .try_fold(CapacityResources::default(), |sum, native| {
            sum.checked_add(native.booked)
        })
        .ok_or_else(|| Refusal::Invalid {
            what: "a node's boxes' bookings",
            why: "they overflow".into(),
        })?;
    Ok(CapacityResources {
        cpu_milli: budget.cpu_milli.saturating_sub(booked.cpu_milli),
        mem_bytes: budget.mem_bytes.saturating_sub(booked.mem_bytes),
        disk_bytes: budget.disk_bytes.saturating_sub(booked.disk_bytes),
    })
}

/// Asks each VM of `asks` its guest time and its cgroup its use, and its record from `record` where its learner meets
/// it or can't read it; lists the node's boxes at their caps (C4) and the fleets under `.vm`: all with its learner let
/// go (A-2).
fn ask(
    io: &impl Cgroups,
    asks: Asks,
    record: &impl Fn(&LearnedHolder) -> Result<Option<Met>, Refusal>,
    held: &[u64],
    clock: &impl Fn() -> Instant,
) -> Result<Reads, Refusal> {
    let mut vms = Vec::new();
    vms.try_reserve_exact(asks.vms.len())
        .map_err(|_| Refusal::Exhausted {
            what: "a window's VMs",
            bound: asks.vms.len(),
        })?;
    for Asked {
        id,
        scope,
        cgroup,
        known,
    } in asks.vms
    {
        // D260: a managed VM's vCPUs' guest time between two reads of its cgroup's use, its CPU taken at the instant its
        // VM process sampled them, so its VMM's own (the cgroup's less its vCPUs') counts nothing its vCPUs ran between
        // the reads (the review's A-11).
        let read = timed(clock, || io.usage(&cgroup)).and_then(|(before, from)| {
            let guest = io.guest_time(&id)?;
            let (usage, to) = timed(clock, || io.usage(&cgroup))?;
            let (cpu_usec, at) = match &guest {
                ManagedGuestTime::PerVcpu {
                    asked, answered, ..
                } => between(
                    (before.cpu_usec, from),
                    (usage.cpu_usec, to),
                    mid(*asked, *answered),
                ),
                ManagedGuestTime::Unheld(_) => (usage.cpu_usec, to),
            };
            Ok((guest, Usage { cpu_usec, ..usage }, at))
        });
        // Its reclaimable file pages, which the machine's free memory counts already (the review's A-10).
        let file = io
            .read(cgroup.dir(), "memory.stat")
            .ok()
            .and_then(|stat| file_pages(stat.as_deref()));
        // Its record where its learner meets it, or where its read failed, so its receipt is counted whole (C8).
        let met = if known && read.is_ok() {
            None
        } else {
            Some(record(&id))
        };
        vms.push(Answer {
            id,
            scope,
            cgroup,
            read,
            file,
            met,
        });
    }
    let natives = natives(io, &asks.root, &asks.oci, &asks.vm, held);
    let fleets = fleets(io, &asks.vm, &asks.fleet);
    Ok(Reads {
        vms,
        natives,
        fleets,
        // Last: a VM's or a box's growth since its read shrinks the fleet's budget, never swells it (A-10).
        free: io.free(),
    })
}

/// The fleets under `.vm` (`vm`) beside this node's own (`fleet`), each other's `cpu.max` as the kernel says it (C4).
fn fleets(io: &impl Cgroups, vm: &Path, fleet: &Path) -> Result<Fleets, Refusal> {
    let mut fleets = Fleets {
        own: false,
        others: Vec::new(),
    };
    for path in io.children(vm)? {
        if path == fleet {
            fleets.own = true;
        } else {
            fleets.others.push(io.read(&path, "cpu.max")?);
        }
    }
    Ok(fleets)
}

/// The node's process and OCI boxes (C4), each booked at its caps, its cgroup's `cpu.max` and `memory.max`: the native
/// classes offer no runtime cost (`facts::capacity`), so a native box's receipt books its caps, which its cgroup holds, a
/// reused slot's as its box's create wrote them, until it is learned. They are the children of the node's `root` and of
/// `.oci` (`oci`), but for `.vm` (`vm`) and `.oci` itself. A slot under `.oci` that no box holds (`held`, in order) is
/// idle: it keeps its last box's caps and books nothing.
fn natives(
    io: &impl Cgroups,
    root: &Path,
    oci: &Path,
    vm: &Path,
    held: &[u64],
) -> Result<Vec<Native>, Refusal> {
    let mut out = Vec::new();
    for (dir, under_oci) in [(root, false), (oci, true)] {
        for child in io.children(dir)? {
            if child == vm || child == oci {
                continue;
            }
            let idle = under_oci
                && child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(hex_core::reuse::slot_of)
                    .is_some_and(|slot| held.binary_search(&slot).is_err());
            let (booked, taken) = if idle {
                (CapacityResources::default(), None)
            } else {
                let booked = CapacityResources {
                    cpu_milli: booked_cpu(io.read(&child, "cpu.max")?.as_deref())?,
                    mem_bytes: booked_bytes(io.read(&child, "memory.max")?.as_deref())?,
                    disk_bytes: 0,
                };
                let current = io.read(&child, "memory.current")?;
                let stat = io.read(&child, "memory.stat")?;
                (booked, taken(current.as_deref(), stat.as_deref()))
            };
            out.push(Native {
                dir: child,
                booked,
                taken,
                oci: under_oci,
                idle,
            });
        }
    }
    Ok(out)
}

/// Acts on what a window's learner decided (C4, C5; outside its lock, A-2), in order: each VM's cgroup rewritten, the
/// fleet's, then `.vm`'s; each process and OCI box and `.oci` weighed; then the last resort's decision carried out.
/// Each VM's cgroup is its own (A-7; ROOT, 2026-11-14): one that can't be written is kept, and the acts go on; the
/// first act not one VM's that fails is kept as the window's.
fn act(io: &impl Cgroups, acts: Acts) -> Result<Acted, Refusal> {
    let mut acted = Acted::default();
    acted
        .vms
        .try_reserve_exact(acts.vms.len())
        .map_err(|_| Refusal::Exhausted {
            what: "a window's cgroup writes",
            bound: acts.vms.len(),
        })?;
    acted
        .unwritten
        .try_reserve_exact(acts.vms.len())
        .map_err(|_| Refusal::Exhausted {
            what: "a window's cgroup writes",
            bound: acts.vms.len(),
        })?;
    if let Err(why) = place(io, acts, &mut acted) {
        acted.failed = Some(why);
    }
    Ok(acted)
}

/// Makes the placement's writes and the last resort's end of `acts`, keeping in `acted` what each did.
fn place(io: &impl Cgroups, acts: Acts, acted: &mut Acted) -> Result<(), Refusal> {
    for placing in acts.vms {
        match make(io, &placing.cgroup, &placing.rewrite) {
            Ok(()) => acted.vms.push(Written {
                scope: placing.scope,
                written: placing.rewrite.written,
                cap: placing.cap,
                budget: placing.budget,
            }),
            Err(why) => acted.unwritten.push((placing.scope, why)),
        }
    }
    if let Some((cgroup, rewrite)) = acts.fleet {
        make(io, &cgroup, &rewrite)?;
        acted.fleet = Some(rewrite.written);
    }
    if let Some((cgroup, rewrite)) = acts.parent? {
        make(io, &cgroup, &rewrite)?;
        acted.parent = Some(rewrite.written);
    }
    for (dir, weight) in &acts.weights {
        weigh(io, dir, *weight)?;
    }
    weigh(io, &acts.oci.0, acts.oci.1)?;
    let target = acts.target.as_ref().map(|(scope, cgroup)| (*scope, cgroup));
    acted.resort = Some(acts.resort.carry(&Kills { io, target }));
    Ok(())
}

/// What a window of the node's learner reads first (C1, C4, C7): the VMs running, each by its holder, its lifetime and
/// its cgroup; the reuse slots live boxes hold, in order; and the machine's resources, which its fleet's budget is
/// measured within ([`measured`]).
pub(super) type Running = (
    Vec<(LearnedHolder, Scope, BoxCgroup)>,
    Vec<u64>,
    CapacityResources,
);

/// One window of `shared`, the node's charge learner (joint plan step 14): what it reads first (`read`), each VM it
/// meets found by `record`, its cgroups read and written through `io` at the instant `now` gives once those are read,
/// and each end its last resort made written into its box's record in `store` after the window. Its learner is taken for
/// each pure step and let go while anything is read or written (A-2), so a capacity answer never waits on them; its
/// fleet's budget less what the capacity view's holders have `reserved`, as its keeper last said (A-10). Its failure is
/// kept for the capacity answers.
pub(super) fn one_window(
    shared: &Shared,
    io: &impl Cgroups,
    store: &crate::store::Store,
    read: impl FnOnce() -> Result<Running, Refusal>,
    record: impl Fn(&LearnedHolder) -> Result<Option<Met>, Refusal>,
    reserved: CapacityResources,
    clock: impl Fn() -> Instant,
) {
    let (running, held, machine) = match read() {
        Ok(read) => read,
        Err(why) => {
            if let Ok(mut learner) = shared.lock() {
                learner.failed(why);
            }
            return;
        }
    };
    // Its outcome is kept as the learner's fault, which the capacity answers read.
    let _ = window(
        shared,
        io,
        running,
        record,
        &held,
        |reads| measured(machine, reserved, reads),
        clock,
    );
    // Each end the last resort made goes into its box's record (C5), off the learner's lock; a write that fails
    // is kept for the next window and fails this one.
    let Ok(ends) = shared.lock().map(|mut learner| learner.unrecorded()) else {
        return;
    };
    let (left, failed) = write_ends(store, ends);
    if let Some(why) = failed
        && let Ok(mut learner) = shared.lock()
    {
        learner.keep_unrecorded(left);
        learner.failed(why);
    }
}

impl Node {
    /// One window of the node's charge learner (joint plan step 14), its failure kept for the capacity answers.
    fn learn_window(&self) {
        let Some(shared) = self.backends.get().and_then(|b| b.learner.as_ref()) else {
            return;
        };
        // What the capacity view's holders have reserved, as the node's configuration and its keeper's word say (A-10):
        // until the keeper of a view that serves its machine has spoken since the node started, its VMs are booked whole,
        // none learned, told so.
        let reservations = match self.reservations() {
            Ok(reservations) => reservations,
            Err(why) => {
                if let Ok(mut learner) = shared.lock() {
                    learner.failed(why);
                }
                return;
            }
        };
        let reserved = {
            let Ok(mut learner) = shared.lock() else {
                return;
            };
            learner.told_reservations(reservations);
            match reservations {
                hex_api::LearnedReservations::Unheard => return,
                hex_api::LearnedReservations::Heard { reserved, .. } => reserved,
                hex_api::LearnedReservations::Unserved => CapacityResources::default(),
            }
        };
        let read = || -> Result<Running, Refusal> {
            let running = self.supervisor.running_vms()?;
            Ok((
                running
                    .into_iter()
                    .map(|(id, scope, cgroup)| {
                        (LearnedHolder::Box(id), Scope::Native(scope), cgroup)
                    })
                    .collect::<Vec<_>>(),
                self.cgroups.held_slots()?,
                self.native_capacity()?.total,
            ))
        };
        let record = |id: &LearnedHolder| -> Result<Option<Met>, Refusal> {
            // A node VM's row is the step-39 holder's: none of this learner's yet.
            let LearnedHolder::Box(id) = id else {
                return Ok(None);
            };
            let Some(record) = self.record(id)? else {
                return Ok(None);
            };
            let class = match record.spec.class.class() {
                hex_api::Class::Shards => Class::Shards,
                // Its own class (P2.2): its windows never raise a Hex guest's prior, nor theirs its.
                hex_api::Class::ShardsImage => Class::ShardsImage,
                hex_api::Class::Firecracker => Class::Firecracker,
                hex_api::Class::Process | hex_api::Class::Oci => return Ok(None),
            };
            let Some(allocation) = record.spec.allocation else {
                return Ok(None);
            };
            Ok(Some(Met {
                class,
                allocation,
                band: record.spec.band,
                started: record.started,
                intent: record.last_resort,
            }))
        };
        let io = Kernel {
            store: &self.store,
            guest: self.backends.get().and_then(|b| b.shards.as_deref()),
            node: self,
        };
        one_window(
            shared,
            &io,
            &self.store,
            read,
            record,
            reserved,
            Instant::now,
        );
    }

    /// The window the node's charge learner reads over, where one runs (C1): its VM backends' learner's.
    pub fn charge_window(&self) -> Option<Duration> {
        self.backends
            .get()
            .and_then(|b| b.learner.as_ref())
            .map(|_| WINDOW)
    }

    /// What `class`'s capacity answer carries of the learner (C8), fitted to one message.
    pub(super) fn attach_learned(
        &self,
        class: hex_api::Class,
        capacity: &mut hex_api::Capacity,
    ) -> Result<(), Refusal> {
        let class = match class {
            hex_api::Class::Shards => Class::Shards,
            hex_api::Class::ShardsImage => Class::ShardsImage,
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
        let learner = learner
            .lock()
            .map_err(|_| Refusal::Broken("the charge learner's lock was poisoned".into()))?;
        capacity.learned = learner.learned(class, &costs)?;
        // Where its last window failed, why (A-3; ROOT, 2026-11-13): never nothing learned with nothing said.
        capacity.unlearned = learner.unlearned();
        drop(learner);
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
