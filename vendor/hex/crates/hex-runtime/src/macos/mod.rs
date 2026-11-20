//! The macOS runtime: [`hex_api::Runtime`] over the `process` class (docs/design/runtime.md). A box is a
//! process group at the QoS its declared weight asks, its bounds held by a watcher thread reading each
//! member's footprint (`process`, `sys`, `watch`); the `oci` class is a Linux kernel's, refused by name
//! here: a node runs it in its Shards guest by wrapping this in [`crate::guest::WithGuest`] (the native
//! plan's step 5). This is the node's runtime seam on macOS; the records,
//! recovery and socket that wrap it on Linux (`hex-node`) are not yet cross-OS, so this holds its boxes
//! in memory and is driven in process.

pub mod command;
mod cpu;
pub mod directory;
pub mod helper_client;
pub mod helper_daemon;
pub mod helper_sys;
mod managed;
pub mod proc;
pub mod process;
pub mod sys;

#[cfg(feature = "native-trusted-entry-measurement")]
pub use process::trusted_entry;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::num::NonZeroU64;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use triomphe::Arc as NativeArc;

use hex_api::{
    Bounds, BoxId, BoxInfo, BoxSpec, BoxState, Class, ClassSpec, ExecLimit, Membership, Proc,
    Program, Refusal, Runtime, Signal, Stdio, Usage,
};
use rustix::fs::{Mode, OFlags};

use hex_core::record::{NativeRecord, OwnerKey, OwnerRef};
use process::{ProcessBox, Shared};

pub use hex_core::helper::managed::CpuControl;
pub use managed::remote::{ManagedHelper, run_managed_helper};

/// The bound macOS's execve puts on a program's arguments and environment, read from the kernel now
/// (`kern.argmax`, kind (a); docs/research/runtime/exec-size-bounds.md). XNU has no bound on one string apart.
pub fn exec_limit() -> Result<ExecLimit, Refusal> {
    Ok(ExecLimit {
        block: sys::arg_max().map_err(|e| Refusal::os("reading kern.argmax", e))?,
        counts: hex_api::BlockCount::Execve,
        string: None,
        unit: hex_api::StringUnit::Bytes,
        source: "macOS's execve (kern.argmax)",
    })
}

/// The headroom held back from a box's bound for the fork arming window, in bytes ([`hex_core::boxuser`]'s
/// reserve): a just-forked member is unarmed until the helper arms it, and this is what it may grow into
/// first. A constant until measured on the running machine (CLAUDE.md §5, `bench/native/macos/boxlab.sh`);
/// capped at a quarter of a box's bound so a small box still has most of its bound to apportion.
const RESERVE_BYTES: u64 = 16 << 20;

/// A box-user box as the node tracks it (the helper holds the live state): its bounds, to report on
/// inspect, and whether its leader has started — the first `exec` opens the box user's session.
struct UserBox {
    bounds: Bounds,
    leader_started: bool,
    /// A run's box's claim on its removal, carried from its making ([`crate::run::Claim`]); none for any other box.
    run: Option<Arc<crate::run::Claim>>,
}

/// The node's box-user backend (native plan step 2): a connection to the privileged helper and the boxes
/// it manages.
struct BoxUser {
    /// Shared with each program it starts, which asks the helper for its end ([`proc::UserProc`]).
    helper: Arc<helper_client::Helper>,
    /// Shared with a run's thread, which removes its box through [`remove_user_run`] as a delete of it does.
    boxes: Arc<Mutex<BTreeMap<BoxId, UserBox>>>,
}

/// The macOS node's runtime over the `process` class. Rootless (`helper` `None`), a box is a process group
/// under a watcher (step 1); with the privileged helper installed (`helper` `Some`), a box is a user of
/// its own the helper starts and bounds in the kernel (step 2). `new` is rootless; `detect` picks box-user
/// when the helper's socket answers. Holds its boxes in memory, keyed by id.
/// This machine's model, `hw.model` (`Mac16,9`): its class, whose recorded growth rates seed its boxes'
/// pacing (`hex_core::watch::seed`). Read once at node start by sysctlbyname: no program runs, so none
/// can inherit a descriptor another thread holds.
pub fn machine_model() -> Result<String, Refusal> {
    sys::sysctl_string(c"hw.model")
        .map(|model| model.trim().to_owned())
        .map_err(|e| Refusal::os("reading the machine's model", e))
}

/// The managed CPU control's maximum, in milli-CPU: every logical processor of the hardware. XNU exports
/// the maximum logical processors in the hardware; active/available thread counts could understate the
/// CPU that a whole native scope can consume during a pulse.
pub fn cpu_ceiling() -> Result<u64, Refusal> {
    sys::sysctl_number(c"hw.logicalcpu_max")
        .map_err(|error| Refusal::os("reading the native hardware CPU ceiling", error))?
        .checked_mul(1000)
        .filter(|ceiling| *ceiling != 0)
        .ok_or_else(|| managed::configuration("hardware CPU ceiling is not representable"))
}

/// AC-T7.4.1: the managed CPU control's cycle for a scope at `rate` milli-CPU of this host's
/// [`cpu_ceiling`] with the measured `response`: how long it runs from full credit, then how long it is
/// held (the control's own budget, `cpu::cycle`). The window that control enforces a scope's rate over.
pub fn cpu_control_cycle(
    rate: u64,
    response: std::time::Duration,
) -> Result<(std::time::Duration, std::time::Duration), Refusal> {
    cpu::cycle(rate, cpu_ceiling()?, response).map_err(|_| {
        managed::configuration("the CPU control has no cycle for this rate and response")
    })
}

/// AC-T7.4.1 (the review's C-9 (c)): the longest the managed CPU control arms its watcher's timer for with the measured
/// stop `response`: its hold at the least rate it holds a scope at ([`cpu::LEAST_RATE`]; [`cpu_control_cycle`]), twice
/// its burst over that rate, longer than any run, whose longest, near the ceiling, is its burst over one milli-CPU. The
/// CPU-response producer measures the timer's waits up to it.
pub fn cpu_control_longest_hold(
    response: std::time::Duration,
) -> Result<std::time::Duration, Refusal> {
    cpu_control_cycle(cpu::LEAST_RATE, response).map(|(_, hold)| hold)
}

pub struct MacRuntime {
    shared: NativeArc<Shared>,
    /// Shared with a run's thread, which removes its box through [`remove_process_run`] as a delete of it does.
    boxes: NativeArc<NativeBoxes>,
    helper: Option<BoxUser>,
    /// The kernel's bound on a program's arguments and environment, read at start (`kern.argmax`).
    exec_limit: ExecLimit,
    /// Frozen measured response and actual hardware ceiling. No absent profile has a default.
    cpu_control: Option<(CpuControl, u64)>,
    managed_helper: Option<NativeArc<managed::remote::Helper>>,
}

type NativeBoxEntries = Vec<(OwnerKey, NativeArc<ProcessBox>)>;
type NativeBoxes = Mutex<NativeBoxEntries>;

fn native_owner_index(
    boxes: &[(OwnerKey, NativeArc<ProcessBox>)],
    id: OwnerRef<'_>,
) -> Result<usize, usize> {
    boxes.binary_search_by(|(key, _)| key.as_ref().cmp(&id))
}
fn native_index(boxes: &[(OwnerKey, NativeArc<ProcessBox>)], id: &BoxId) -> Result<usize, usize> {
    native_owner_index(boxes, OwnerRef::Box(id))
}

fn native_allocation(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "native box receipt storage",
        bound,
    }
}

/// macOS's socket, socketpair, accept and pipe take no CLOEXEC or NONBLOCK flag, so each new
/// private descriptor gets them here before it is shared. No program Hex starts on macOS inherits
/// one in the gap: `sys::spawn` passes POSIX_SPAWN_CLOEXEC_DEFAULT (and [`command`] starts std's
/// `Command` calls through it, as std's spawn passes none), and `helper_sys`'s fork closes every
/// descriptor past stdio before its exec.
pub(crate) fn private_descriptor(fd: impl AsFd, nonblocking: bool) -> rustix::io::Result<()> {
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
    if nonblocking {
        let flags = rustix::fs::fcntl_getfl(&fd)?;
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    }
    Ok(())
}

impl MacRuntime {
    /// Checks the same immutable cost before route registration/admission. A missing measured
    /// control profile is not a guessed VM allowance or an initialized native route.
    pub fn check_vm_cost(&self, cost: &hex_api::RuntimeCost) -> Result<(), Refusal> {
        cost.check()?;
        let (control, hardware) = self
            .cpu_control
            .ok_or_else(|| managed::configuration("managed VM CPU control is not initialized"))?;
        if cost.id != control.cost_id {
            return Err(managed::configuration(
                "managed VM cost differs from the measured CPU response profile",
            ));
        }
        let resources = cost
            .allocate(hex_api::CapacityResources::default())?
            .resources;
        if resources.cpu_milli > hardware || u32::try_from(resources.cpu_milli).is_err() {
            return Err(managed::configuration(
                "managed VM CPU floor exceeds the actual native hardware envelope",
            ));
        }
        if self.managed_helper.is_none() {
            return Err(managed::configuration(
                "managed VM persistent native owner is not configured",
            ));
        }
        Ok(())
    }
    /// Same post-Arc authority, only for read-only completed-helper restart fencing.
    pub fn bind_native_books(
        &self,
        books: std::sync::Weak<dyn crate::devices::Books>,
    ) -> Result<(), Refusal> {
        match &self.managed_helper {
            Some(helper) => helper.bind_books(books),
            None => Ok(()),
        }
    }

    /// Install the same rootless persistent helper factory before native effects. Its process is
    /// started only by an admitted managed launch; construction/registration never probes or boots.
    pub fn with_managed_helper(mut self, config: ManagedHelper) -> Result<Self, Refusal> {
        if self.managed_helper.is_some() || !self.boxes()?.is_empty() {
            return Err(managed::configuration(
                "managed helper is immutable after registration",
            ));
        }
        if let Some(helper) = &self.helper
            && !helper.boxes()?.is_empty()
        {
            return Err(managed::configuration(
                "managed helper is immutable after native registration",
            ));
        }
        let (cpu, _) = self.cpu_control.ok_or_else(|| {
            managed::configuration("configure the measured CPU profile before its native helper")
        })?;
        self.managed_helper = Some(managed::remote::Helper::new(
            config,
            cpu,
            self.shared.rates()?,
            self.shared.lateness()?,
        )?);
        Ok(self)
    }
    /// Configure managed scope CPU control before this runtime owns any native effects. The exact
    /// cost identity is also checked against each canonical allocation before native preparation.
    pub fn with_cpu_control(mut self, control: CpuControl) -> Result<Self, Refusal> {
        control.check().map_err(|_| {
            managed::configuration("CPU control must name a positive finite measured response")
        })?;
        if self.cpu_control.is_some() || !self.boxes()?.is_empty() {
            return Err(managed::configuration(
                "CPU control is immutable after native registration",
            ));
        }
        if let Some(helper) = &self.helper
            && !helper.boxes()?.is_empty()
        {
            return Err(managed::configuration(
                "CPU control is immutable after native registration",
            ));
        }
        self.cpu_control = Some((control, cpu_ceiling()?));
        Ok(self)
    }

    /// The original canonical VM row's native owner. It neither chooses devices nor creates a
    /// synthetic Process workload; its receipt is retained in the same native box table.
    pub fn launch_managed(
        &self,
        record: NativeRecord<'_>,
        plan: &crate::devices::ManagedLaunch<'_>,
        io: shards_api::LaunchIo,
        patience: Option<std::time::Duration>,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        managed::launch(self, record, plan, io, patience, publish)
    }
    pub fn attach_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        managed::attach(self, record, identity)
    }
    /// The internal Books transaction calls this only after the exact durable Released boot receipt.
    pub fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        managed::finalize(self, record, identity)
    }
    pub fn managed_usage(&self, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
        managed::usage(self, record)
    }
    /// Joint plan step 14 (C1 on macOS; ROOT, 2026-11-13; 2c-4b): the cycle this control holds a scope at `rate`
    /// milli-CPU over, its run and then its hold ([`cpu::cycle`]) at the stop response its managed helper estimates from
    /// its own holds, asked of it each time ([`cycle_of`]), so a VM it holds there keeps its rate on average over one:
    /// the charge learner's window takes it for each VM the control holds. Never at the control's ceiling or past it;
    /// cold, told, until its helper has measured a hold, or where none runs; used and told cold while its estimate is
    /// (the review's A-14). Refused where no control is configured.
    pub fn cap_cycle(&self, rate: u64) -> Result<crate::guest::CapCycle, Refusal> {
        let (_, ceiling) = self
            .cpu_control
            .ok_or_else(|| managed::configuration("managed VM CPU control is not initialized"))?;
        if rate >= ceiling {
            return Ok(crate::guest::CapCycle::Never);
        }
        cycle_of(rate, ceiling, managed::response(self)?)
    }

    /// Joint plan step 14 (C4 on macOS; ROOT, 2026-11-13): the least share a managed VM's CPU control holds it at, its
    /// rate's unit.
    pub fn managed_least(&self) -> u64 {
        cpu::LEAST_RATE
    }

    /// Joint plan step 14 (D260): how long the process that started at `started` has run, a managed VM's VM process by
    /// its record's `init` ([`process_life`]).
    pub fn managed_life(&self, started: u64) -> Result<std::time::Duration, Refusal> {
        process_life(started)
    }

    /// Joint plan step 14 (C4 on macOS): the managed scope's CPU rate becomes `rate` milli-CPU, its share as the node's
    /// charge learner water-fills it, through its persistent native owner.
    pub fn set_managed_rate(&self, record: NativeRecord<'_>, rate: u64) -> Result<(), Refusal> {
        managed::set_rate(self, record, rate)
    }

    /// Joint plan step 14 (C5 on macOS): the node's last resort ends the managed VM through its persistent native owner,
    /// as its receipt's stop ends it; the VM's end is observed and finalized as any other.
    pub fn stop_managed(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        managed::stop(self, record)
    }

    /// The growth rates the node's boxes are paced at, as its watchers have learned them (observability:
    /// what the watchers pace by).
    pub fn pacing(&self) -> Result<hex_core::watch::Rates, Refusal> {
        self.shared.rates()
    }

    /// The stop response the managed CPU control estimates from its own holds (joint plan step 14, 2c; the review's
    /// A-14): none before its first.
    pub fn cpu_response(
        &self,
    ) -> Result<Option<hex_core::helper::managed::response::Estimate>, Refusal> {
        self.shared.cpu_response()
    }

    /// How late the node's watchers have woken past their waits, at the latest (its class's record to
    /// start; observability: what the boxes' holds reckon with).
    pub fn lateness(&self) -> Result<std::time::Duration, Refusal> {
        self.shared.lateness()
    }

    /// A rootless runtime (no helper): boxes are process groups whose bounds a watcher holds, reckoned to
    /// grow at `rate_bytes` bytes and `rate_processes` processes a second, and its watchers to wake up to
    /// `lateness` past their waits (the bound's pacing and its hold, CLAUDE.md §5: measured at node start or
    /// cited, the caller says which).
    pub fn new(
        rate_bytes: NonZeroU64,
        rate_processes: NonZeroU64,
        lateness: std::time::Duration,
    ) -> Result<Self, Refusal> {
        let shared = Shared::new(rate_bytes, rate_processes, lateness)
            .map_err(|e| Refusal::os("reading the machine's timebase", e))?;
        Ok(Self {
            shared: NativeArc::try_new(shared)
                .map_err(|_| native_allocation(std::mem::size_of::<Shared>()))?,
            boxes: NativeArc::try_new(Mutex::new(Vec::new()))
                .map_err(|_| native_allocation(std::mem::size_of::<NativeBoxes>()))?,
            helper: None,
            exec_limit: exec_limit()?,
            cpu_control: None,
            managed_helper: None,
        })
    }

    /// Picks box-user mode if the helper at `helper_socket` answers, else the rootless process-group mode
    /// of [`MacRuntime::new`]. The node's startup calls this; the conformance suite uses `new`.
    pub fn detect(
        rate_bytes: NonZeroU64,
        rate_processes: NonZeroU64,
        lateness: std::time::Duration,
        helper_socket: &std::path::Path,
    ) -> Result<Self, Refusal> {
        match helper_client::Helper::connect(helper_socket)? {
            Some(helper) => {
                let shared = Shared::new(rate_bytes, rate_processes, lateness)
                    .map_err(|e| Refusal::os("reading the machine's timebase", e))?;
                Ok(Self {
                    shared: NativeArc::try_new(shared)
                        .map_err(|_| native_allocation(std::mem::size_of::<Shared>()))?,
                    boxes: NativeArc::try_new(Mutex::new(Vec::new()))
                        .map_err(|_| native_allocation(std::mem::size_of::<NativeBoxes>()))?,
                    helper: Some(BoxUser {
                        helper: Arc::new(helper),
                        boxes: Arc::new(Mutex::new(BTreeMap::new())),
                    }),
                    exec_limit: exec_limit()?,
                    cpu_control: None,
                    managed_helper: None,
                })
            }
            None => Self::new(rate_bytes, rate_processes, lateness),
        }
    }

    /// Box `id`'s process group and its leader's start time, once its first member has started: what names
    /// its members after the node is gone, for the node's record (native plan step 8, T-T7.1.2).
    pub fn group(&self, id: &BoxId) -> Result<Option<(i32, u64)>, Refusal> {
        self.get(id)?.group()
    }

    /// Takes back box `spec` after the node was gone, on `group` where it is still the box's
    /// ([`ProcessBox::adopt`]).
    pub fn adopt(&self, spec: &BoxSpec, group: Option<(i32, u64)>) -> Result<(), Refusal> {
        let mut boxes = self.boxes()?;
        let at = native_index(&boxes, &spec.id)
            .map_or_else(Ok, |_| Err(Refusal::Exists(spec.id.clone())))?;
        if boxes.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        boxes
            .try_reserve(1)
            .map_err(|_| native_allocation(crate::MAX_BOXES))?;
        let b = ProcessBox::adopt(spec.id.clone(), &spec.bounds, self.shared.as_ref(), group)?;
        let b = NativeArc::try_new(b)
            .map_err(|_| native_allocation(std::mem::size_of::<ProcessBox>()))?;
        boxes.insert(at, (OwnerKey::Box(spec.id.try_clone()?), b));
        Ok(())
    }

    /// Ends what still runs of a box the node's recovery removes, named by `group`
    /// ([`process::end_group`]).
    pub fn end(group: Option<(i32, u64)>) -> Result<(), Refusal> {
        match group {
            Some((pgid, started)) => process::end_group(pgid, started),
            None => Ok(()),
        }
    }

    /// Hands run box `id`'s program to its own thread ([`crate::run::Run`]), with the box's KILL and its removal,
    /// each the mode's own, the removal touching the box that carries `claim` alone.
    fn watch_run(
        &self,
        proc: Box<dyn Proc>,
        id: &BoxId,
        claim: &Arc<crate::run::Claim>,
    ) -> Result<crate::run::Run, Refusal> {
        if let Some(bu) = &self.helper {
            let (killer, kill_id) = (Arc::clone(&bu.helper), id.clone());
            let (helper, boxes, gone) = (Arc::clone(&bu.helper), Arc::clone(&bu.boxes), id.clone());
            let carried = Arc::clone(claim);
            return crate::run::Run::start(
                proc,
                id,
                claim,
                Box::new(move || killer.signal(&kill_id, Signal::KILL.number())),
                Box::new(move || remove_user_run(&helper, &boxes, &gone, &carried)),
            );
        }
        let b = self.get(id)?;
        let (boxes, gone) = (NativeArc::clone(&self.boxes), id.clone());
        let carried = Arc::clone(claim);
        crate::run::Run::start(
            proc,
            id,
            claim,
            Box::new(move || b.signal(Signal::KILL)),
            Box::new(move || remove_process_run(&boxes, &gone, &carried)),
        )
    }

    /// Removes run box `id`, which carries `claim`, as a delete of it does ([`crate::run::delete`]), touching no other
    /// box: a run that couldn't start removes what it made so.
    fn delete_run(&self, id: &BoxId, claim: &Arc<crate::run::Claim>) -> Result<(), Refusal> {
        match &self.helper {
            Some(bu) => crate::run::delete(claim, id, || {
                remove_user_run(&bu.helper, &bu.boxes, id, claim)
            }),
            None => crate::run::delete(claim, id, || remove_process_run(&self.boxes, id, claim)),
        }
    }

    /// Makes box `spec`, a run's carrying `run`, its removal's claim, from its making ([`crate::run::Claim`]).
    fn create_with(
        &self,
        spec: BoxSpec,
        run: Option<&Arc<crate::run::Claim>>,
    ) -> Result<BoxInfo, Refusal> {
        spec.check_no_devices("macos")?;
        if spec.allocation.is_some() {
            let text = "this ordinary native route advertises identity cost";
            let mut why = String::new();
            why.try_reserve_exact(text.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "native allocation refusal bytes",
                    bound: text.len(),
                })?;
            why.push_str(text);
            return Err(Refusal::Invalid {
                what: "native runtime allocation",
                why,
            });
        }
        let ClassSpec::Process = spec.class else {
            return Err(Refusal::Platform {
                what: "the oci class",
                platform: "macos",
                why: "a Linux image needs a Linux kernel: the node's Shards guest carries it \
                      (hex_runtime::guest::WithGuest routes the class there)"
                    .to_owned(),
            });
        };
        // A bound macOS can't hold of a process box is refused, typed, never dropped, in either mode (CLAUDE.md §3).
        hex_core::watch::check(&spec.bounds)?;
        if let Some(bu) = &self.helper {
            return bu.create(&spec, run);
        }
        let mut boxes = self.boxes()?;
        let at = native_index(&boxes, &spec.id)
            .map_or_else(Ok, |_| Err(Refusal::Exists(spec.id.clone())))?;
        if boxes.len() >= crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        boxes
            .try_reserve(1)
            .map_err(|_| native_allocation(crate::MAX_BOXES))?;
        let b = NativeArc::try_new(ProcessBox::create(
            spec.id.clone(),
            &spec.bounds,
            self.shared.as_ref(),
        )?)
        .map_err(|_| native_allocation(std::mem::size_of::<ProcessBox>()))?;
        if let Some(claim) = run {
            b.carry(claim)?;
        }
        let info = self.info(&b)?;
        boxes.insert(at, (OwnerKey::Box(spec.id), b));
        Ok(info)
    }

    fn boxes(&self) -> Result<std::sync::MutexGuard<'_, NativeBoxEntries>, Refusal> {
        self.boxes
            .lock()
            .map_err(|_| Refusal::Broken("the node's box map lock is poisoned".to_owned()))
    }

    /// Box `id` as its watcher last recorded it, why it ended included (observability: the footprint the
    /// watcher read when it ended a box, beside the peak).
    pub fn status(&self, id: &BoxId) -> Result<process::Status, Refusal> {
        self.get(id)?.status()
    }

    fn get(&self, id: &BoxId) -> Result<NativeArc<ProcessBox>, Refusal> {
        let boxes = self.boxes()?;
        let at = native_index(&boxes, id).map_err(|_| Refusal::NoSuchBox(id.clone()))?;
        let (_, boxed) = boxes.get(at).ok_or_else(|| {
            managed::uncertain(id, "its native slot is gone from where its index found it")
        })?;
        if boxed.is_managed() {
            return Err(Refusal::NoSuchBox(id.try_clone()?));
        }
        Ok(NativeArc::clone(boxed))
    }

    fn info(&self, b: &ProcessBox) -> Result<BoxInfo, Refusal> {
        Ok(BoxInfo {
            published: Vec::new(),
            grants: Vec::new(),
            route: None,
            id: b.id()?.try_clone()?,
            class: Class::Process,
            state: if b.running_now()? {
                BoxState::Running
            } else {
                BoxState::Empty
            },
            bounds: b.bounds(),
            adopted: false,
            // No helper installed: this is the rootless process-group class (step 1), and inspect says so.
            membership: Some(Membership::ProcessGroup),
            // A process box has no init, so no container exit to report (HEX-RT-11).
            exit: None,
            abandoned: 0,
            allocation: None,
            last_resort: None,
            ended: None,
            control: None,
            control_wake: None,
            credentials: Vec::new(),
        })
    }
}

impl BoxUser {
    fn boxes(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<BoxId, UserBox>>, Refusal> {
        self.boxes
            .lock()
            .map_err(|_| Refusal::Broken("the node's box-user map lock is poisoned".to_owned()))
    }

    /// Acquires a pool user for the box and remembers its bounds, and the claim it carries where it is a run's box.
    fn create(
        &self,
        spec: &BoxSpec,
        run: Option<&Arc<crate::run::Claim>>,
    ) -> Result<BoxInfo, Refusal> {
        let mut boxes = self.boxes()?;
        if boxes.contains_key(&spec.id) {
            return Err(Refusal::Exists(spec.id.clone()));
        }
        let bound = spec.bounds.memory_max.unwrap_or(u64::MAX);
        let reserve = RESERVE_BYTES.min(bound / 4);
        self.helper
            .acquire(&spec.id, bound, reserve, spec.bounds.pids_max)?;
        boxes.insert(
            spec.id.clone(),
            UserBox {
                bounds: spec.bounds.clone(),
                leader_started: false,
                run: run.map(Arc::clone),
            },
        );
        Ok(info(spec.id.clone(), spec.bounds.clone(), BoxState::Empty))
    }

    /// Starts `program` as the box's pool user through the helper (the first one opens the session).
    fn exec(&self, id: &BoxId, program: &Program, fds: [OwnedFd; 3]) -> Result<i32, Refusal> {
        let leader = {
            let mut boxes = self.boxes()?;
            let b = boxes
                .get_mut(id)
                .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
            let leader = !b.leader_started;
            b.leader_started = true;
            leader
        };
        self.helper.start(
            id,
            program,
            [fds[0].as_fd(), fds[1].as_fd(), fds[2].as_fd()],
            leader,
        )
    }

    fn info(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        let bounds = self
            .boxes()?
            .get(id)
            .map(|b| b.bounds.clone())
            .ok_or_else(|| Refusal::NoSuchBox(id.clone()))?;
        let state = self.helper.inspect(id)?;
        let box_state = if state.members > 0 {
            BoxState::Running
        } else {
            BoxState::Empty
        };
        Ok(info(id.clone(), bounds, box_state))
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        let s = self.helper.inspect(id)?;
        Ok(Usage {
            memory_bytes: s.footprint,
            memory_peak_bytes: s.peak,
            oom_kills: s.crossings,
            pids: u64::from(s.processes),
            pids_refused: 0,
            cpu_usec: 0,
            io_read_bytes: 0,
            io_write_bytes: 0,
            guest: None,
            started_from: None,
            paused_ns: None,
        })
    }

    /// A run's box is its removal's claimer's ([`crate::run::delete`]); any other is ended and forgotten here.
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        let claim = self.boxes()?.get(id).and_then(|b| b.run.clone());
        match claim {
            Some(claim) => crate::run::delete(&claim, id, || {
                remove_user_run(&self.helper, &self.boxes, id, &claim)
            }),
            None => remove_user_box(&self.helper, &self.boxes, id),
        }
    }
}

fn user_boxes(
    boxes: &Mutex<BTreeMap<BoxId, UserBox>>,
) -> Result<std::sync::MutexGuard<'_, BTreeMap<BoxId, UserBox>>, Refusal> {
    boxes
        .lock()
        .map_err(|_| Refusal::Broken("the node's box-user map lock is poisoned".to_owned()))
}

/// Ends box-user box `id` through the helper and forgets it.
fn remove_user_box(
    helper: &helper_client::Helper,
    boxes: &Mutex<BTreeMap<BoxId, UserBox>>,
    id: &BoxId,
) -> Result<(), Refusal> {
    helper.end(id)?;
    user_boxes(boxes)?.remove(id);
    Ok(())
}

/// Ends box-user run box `id` where it still carries `claim`, its removal's claimer's (the run's end or a delete):
/// ended through the helper while it stays in view, so a delete beside it waits for the claimer's word and no box
/// takes its id meanwhile, then forgotten. A box under the id that carries no such claim is another, which this
/// touches not: the claimer's box is gone.
fn remove_user_run(
    helper: &helper_client::Helper,
    boxes: &Mutex<BTreeMap<BoxId, UserBox>>,
    id: &BoxId,
    claim: &Arc<crate::run::Claim>,
) -> Result<(), Refusal> {
    let carries = |b: &UserBox| b.run.as_ref().is_some_and(|run| Arc::ptr_eq(run, claim));
    if !user_boxes(boxes)?.get(id).is_some_and(carries) {
        return Ok(());
    }
    helper.end(id)?;
    let mut held = user_boxes(boxes)?;
    if held.get(id).is_some_and(carries) {
        held.remove(id);
    }
    Ok(())
}

fn native_boxes<'a>(
    boxes: &'a NativeBoxes,
    id: &BoxId,
) -> Result<std::sync::MutexGuard<'a, NativeBoxEntries>, Refusal> {
    boxes
        .lock()
        .map_err(|_| managed::uncertain(id, "its node's native boxes' lock is poisoned"))
}

/// The runtime's delete of rootless box `id`: a run's box is its removal's claimer's ([`crate::run::delete`]); any
/// other is removed and ended here (kill, drain, stop its watcher). Dropping the last Arc does the same, so removing a
/// box a caller still holds through `list` is safe.
fn remove_process_box(boxes: &NativeBoxes, id: &BoxId) -> Result<(), Refusal> {
    let b = {
        let mut held = native_boxes(boxes, id)?;
        let at = native_index(&held, id).map_err(|_| Refusal::NoSuchBox(id.clone()))?;
        let boxed = held.get(at).map(|(_, boxed)| boxed).ok_or_else(|| {
            managed::uncertain(id, "its native slot is gone from where its index found it")
        })?;
        if boxed.is_managed() {
            return Err(managed::uncertain(
                id,
                "its box is a managed VM's, which only its own scope's stop removes",
            ));
        }
        if let Some(claim) = boxed.claim().cloned() {
            drop(held);
            return crate::run::delete(&claim, id, || remove_process_run(boxes, id, &claim));
        }
        held.remove(at).1
    };
    b.delete()
}

/// Removes rootless run box `id` where it still carries `claim`, its removal's claimer's (the run's end or a delete):
/// ended (kill, drain, stop its watcher) while it stays in view, so a delete beside it waits for the claimer's word and
/// no box takes its id meanwhile, then taken out. A box under the id that carries no such claim is another, which this
/// touches not: the claimer's box is gone.
fn remove_process_run(
    boxes: &NativeBoxes,
    id: &BoxId,
    claim: &Arc<crate::run::Claim>,
) -> Result<(), Refusal> {
    let carries = |b: &NativeArc<ProcessBox>| b.claim().is_some_and(|run| Arc::ptr_eq(run, claim));
    let b = {
        let held = native_boxes(boxes, id)?;
        let Some(b) = native_index(&held, id)
            .ok()
            .and_then(|at| held.get(at))
            .map(|(_, boxed)| boxed)
            .filter(|boxed| carries(boxed))
        else {
            return Ok(());
        };
        NativeArc::clone(b)
    };
    let ended = b.delete();
    let mut held = native_boxes(boxes, id)?;
    if let Ok(at) = native_index(&held, id)
        && held.get(at).is_some_and(|(_, boxed)| carries(boxed))
    {
        held.remove(at);
    }
    ended
}

/// A box-user box's [`BoxInfo`]: a `process` box, with box-user membership (the helper holds it), no init
/// exit, and no abandoned programs (the node's disconnect rule is the helper's here).
fn info(id: BoxId, bounds: Bounds, state: BoxState) -> BoxInfo {
    BoxInfo {
        published: Vec::new(),
        grants: Vec::new(),
        route: None,
        id,
        class: Class::Process,
        state,
        bounds,
        adopted: false,
        membership: Some(Membership::BoxUser),
        exit: None,
        abandoned: 0,
        allocation: None,
        last_resort: None,
        ended: None,
        control: None,
        control_wake: None,
        credentials: Vec::new(),
    }
}

impl Runtime for MacRuntime {
    fn classes(&self) -> Vec<Class> {
        vec![Class::Process]
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.create_with(spec, None)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        // A process box has no init to start: exec runs its programs. Refused as the simulator and the
        // Linux node refuse it (HEX-RT-10). An absent box is NoSuchBox.
        if let Some(bu) = &self.helper {
            if !bu.boxes()?.contains_key(id) {
                return Err(Refusal::NoSuchBox(id.clone()));
            }
        } else {
            let _ = self.get(id)?;
        }
        Err(Refusal::Invalid {
            what: "start",
            why: format!("box {id} is a process box, which has no init to start"),
        })
    }

    /// Streams asked for are made as pipes, the program's ends its stdio, the caller's held by its `Proc`.
    fn exec(
        &self,
        id: &BoxId,
        program: Program,
        mut stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        program.fits(&self.exec_limit)?;
        if let Some(bu) = &self.helper {
            program.check()?;
            if program.user.is_some() {
                return Err(Refusal::Invalid {
                    what: "program",
                    why: "a box-user box runs its programs as its pool user; a user is the oci class's"
                        .to_owned(),
                });
            }
            let given = stdio.make_asked()?;
            let pid = bu.exec(id, &program, stdio_fds(stdio)?)?;
            let proc = proc::UserProc::watch(pid, id.clone(), Arc::clone(&bu.helper))?;
            return Ok(Box::new(hex_api::Asked::new(proc, given)));
        }
        let given = stdio.make_asked()?;
        let b = self.get(id)?;
        // The box's whole memory bound arms each member with a fatal per-process cap (native plan step 1's
        // `_ext` backstop); the box tracks which members carry it.
        let spawn = build_spawn(&program, stdio, b.bounds().memory_max)?;
        let pid = b.exec(spawn)?;
        let proc = proc::BoxProc::watch(pid, Some(b.members()))?;
        Ok(Box::new(hex_api::Asked::new(proc, given)))
    }

    /// A run's box goes once its program ends (decision 7): a thread of the run's own waits for the end and
    /// removes the box through the delete's own path ([`crate::run::Run`]), whether or not its caller still holds
    /// the run, and only then gives the caller the end. Its kill is the box's KILL, the box holding that program
    /// alone. In both modes: a process group's and a box user's. The box carries its removal's claim from its making,
    /// held here until it is whole and its program watched, so no delete moves it meanwhile, and it is removed once,
    /// by the run's end or a delete, never after another box has taken its id (runtime goal 7).
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        spec.check_no_devices("macos")?;
        spec.check_run()?;
        let id = spec.id.clone();
        let (claim, making) = crate::run::Claim::making();
        self.create_with(spec, Some(&claim))?;
        let begun = self
            .exec(&id, program, stdio)
            .and_then(|proc| self.watch_run(proc, &id, &claim));
        making.made();
        match begun {
            Ok(run) => Ok(Box::new(run)),
            Err(why) => Err(crate::run::undone(why, self.delete_run(&id, &claim), &id)),
        }
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        if let Some(bu) = &self.helper {
            return bu.helper.signal(id, signal.number());
        }
        self.get(id)?.signal(signal)
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        if let Some(bu) = &self.helper {
            return bu.info(id);
        }
        let b = self.get(id)?;
        self.info(&b)
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        if let Some(bu) = &self.helper {
            return bu.usage(id);
        }
        self.get(id)?.usage()
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        if let Some(bu) = &self.helper {
            let ids: Vec<BoxId> = bu.boxes()?.keys().cloned().collect();
            return ids.iter().map(|id| bu.info(id)).collect();
        }
        let boxes = {
            let held = self.boxes()?;
            let mut boxes = Vec::new();
            boxes
                .try_reserve_exact(held.len())
                .map_err(|_| native_allocation(held.len()))?;
            for (_, boxed) in held.iter().filter(|(_, boxed)| !boxed.is_managed()) {
                boxes.push(NativeArc::clone(boxed));
            }
            boxes
        };
        boxes.iter().map(|b| self.info(b)).collect()
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        if let Some(bu) = &self.helper {
            return bu.delete(id);
        }
        remove_process_box(&self.boxes, id)
    }
}

/// A program's three stdio descriptors for a box-user `exec`, each the null device where the caller gave
/// none (as the process class's spawn does), to ride to the helper by `SCM_RIGHTS`.
fn stdio_fds(stdio: Stdio) -> Result<[OwnedFd; 3], Refusal> {
    Ok([
        stdio
            .stdin
            .map_or_else(|| null(OFlags::RDONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stdout
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stderr
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
    ])
}

/// Builds the [`sys::Spawn`] for `program` with `stdio`: its exec path search (as the Linux process class
/// does it), its argv and environment, its working folder, its three streams (each the null device where
/// the caller gave none), and, when the box has a memory bound (`memory_max`), the fatal per-process
/// jetsam cap in MiB that arms each member at the box's whole bound (native plan step 1's `_ext`
/// backstop). The cap rounds down to MiB, so it never exceeds the bound ([`sys::jetsam_cap_mib`]).
fn build_spawn(
    program: &Program,
    stdio: Stdio,
    memory_max: Option<u64>,
) -> Result<sys::Spawn, Refusal> {
    if program.user.is_some() {
        return Err(Refusal::Invalid {
            what: "program",
            why: "a process box runs its programs as the node agent's user; a user is the oci class's"
                .to_owned(),
        });
    }
    let stdio = [
        stdio
            .stdin
            .map_or_else(|| null(OFlags::RDONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stdout
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
        stdio
            .stderr
            .map_or_else(|| null(OFlags::WRONLY), |p| Ok(OwnedFd::from(p)))?,
    ];
    let parts = program_parts(program)?;
    Ok(sys::Spawn {
        paths: parts.paths,
        argv: parts.argv,
        envp: parts.envp,
        cwd: parts.cwd,
        stdio,
        report: None,
        lifeline: None,
        jetsam_mib: memory_max.and_then(sys::jetsam_cap_mib),
    })
}

/// A program's exec parts, apart from its stdio: the paths to try, its argv and environment, and its
/// working folder, as C strings. Shared by the `process` class's [`build_spawn`] and the box helper
/// daemon's privileged spawn ([`helper_sys`]), which supplies its own stdio and user.
pub(crate) struct Parts {
    pub paths: Vec<CString>,
    pub argv: Vec<CString>,
    pub envp: Vec<CString>,
    pub cwd: Option<CString>,
}

/// Builds a program's [`Parts`]: its path search, argv, environment and working folder.
pub(crate) fn program_parts(program: &Program) -> Result<Parts, Refusal> {
    program.check()?;
    Ok(Parts {
        paths: paths(program)?,
        argv: program
            .args
            .iter()
            .map(|a| c_string(a))
            .collect::<Result<_, _>>()?,
        envp: program
            .env
            .iter()
            .map(|(k, v)| c_string(&format!("{k}={v}")))
            .collect::<Result<_, _>>()?,
        cwd: program.cwd.as_deref().map(c_string).transpose()?,
    })
}

/// The paths a program is executed from, in order, as execvp(3) tries them: its name when it holds a `/`,
/// else that name in each folder of its own `PATH`, or of the default `/usr/bin:/bin` when it sets none.
fn paths(program: &Program) -> Result<Vec<CString>, Refusal> {
    let first = program.args.first().ok_or(Refusal::Invalid {
        what: "program",
        why: "no arguments".to_owned(),
    })?;
    if first.contains('/') {
        return Ok(vec![c_string(first)?]);
    }
    let path = program
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("/usr/bin:/bin");
    path.split(':')
        .map(|dir| {
            c_string(&format!(
                "{}/{first}",
                if dir.is_empty() { "." } else { dir }
            ))
        })
        .collect()
}

fn c_string(s: &str) -> Result<CString, Refusal> {
    CString::new(s).map_err(|_| Refusal::Invalid {
        what: "program",
        why: format!("{s:?} holds a NUL byte"),
    })
}

/// The null device, for a stream the caller gave none.
fn null(flags: OFlags) -> Result<OwnedFd, Refusal> {
    rustix::fs::open("/dev/null", flags | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| Refusal::os("opening /dev/null", e))
}

/// Joint plan step 14 (C1 on macOS; 2c-4b): the managed control's cycle at `rate` milli-CPU below its `ceiling`, its run
/// and its hold at the stop `response` its helper estimates ([`cpu::cycle`]); cold, told, where it has learned none, as
/// no response is guessed (decision 9's cold start); and while its estimate is cold, the slowest of too few holds or an
/// older helper's slowest seen, that cycle used and told cold (the review's A-14).
pub(crate) fn cycle_of(
    rate: u64,
    ceiling: u64,
    response: Option<hex_core::helper::managed::response::Estimate>,
) -> Result<crate::guest::CapCycle, Refusal> {
    let Some(response) = response else {
        return Ok(crate::guest::CapCycle::Cold(
            "its control's cycle is unknown until its managed helper measures a stop".to_owned(),
        ));
    };
    let (run, hold) = cpu::cycle(rate, ceiling, response.response).map_err(|_| {
        managed::configuration("the CPU control has no cycle at the rate it is asked")
    })?;
    let cycle = run
        .checked_add(hold)
        .ok_or_else(|| managed::configuration("the CPU control's cycle overflows"))?;
    Ok(if response.cold {
        crate::guest::CapCycle::ColdOf {
            cycle,
            why: "its control's cycle is at the slowest stop its managed helper has measured, not their p99.9"
                .to_owned(),
        }
    } else {
        crate::guest::CapCycle::Of(cycle)
    })
}

/// Joint plan step 14 (D260): how long the process that started at `started` has run: from its start, as `proc_pid_rusage`
/// reads it (`ri_proc_start_abstime`, Mach absolute time: a managed VM's `init`, Shards' `Identity::started`), to now on
/// the same clock (`CLOCK_UPTIME_RAW`, Mach absolute time in nanoseconds). Refused where a clock can't be read, or where
/// the start is past now, which no process's is.
pub fn process_life(started: u64) -> Result<std::time::Duration, Refusal> {
    let timebase = sys::Timebase::get()
        .map_err(|why| Refusal::os("reading Mach absolute time's units", why))?;
    let now = sys::uptime_raw().map_err(|why| Refusal::Os {
        op: "reading the clock a process's life is reckoned on",
        why: why.to_string(),
    })?;
    now.checked_sub(std::time::Duration::from_nanos(timebase.nanos(started)))
        .ok_or_else(|| Refusal::Invalid {
            what: "a process's start",
            why: format!("{started} Mach ticks is past now on its own clock"),
        })
}
