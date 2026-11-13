//! The node agent as its containers' parent (decision 2 of docs/design/runtime-architecture.md):
//! the agent is a child subreaper, so an `oci` box's init, orphaned when the shim exits after
//! forking it, becomes the agent's child (PR_SET_CHILD_SUBREAPER(2const)). The agent holds each
//! init's pidfd, which turns readable when the init ends (pidfd_open(2)); a thread waits on them all
//! through one epoll set, reaps each ended init with `waitid(P_PIDFD)` and records how it ended in
//! the box's record. A box's other processes that are orphaned (a daemon a `process` box's program
//! forks, a container's process without a pid namespace) become the agent's children too; a sweep
//! each [`SWEEP_PERIOD`] reaps those that have ended, and only those: a zombie in one of the node's
//! box cgroups that no [`Proc`] handle and no watched init owns. A box a run made for one program's life
//! (decision 7) is watched through its program's pidfd in the same set, and removed once the program ends
//! ([`run`]).

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, Weak};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Proc, Refusal};
use hex_core::record::{NativeRecord, NativeScope, OwnerKey, OwnerRef, ProcId};
use rustix::buffer::spare_capacity;
use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions, WaitIdStatus};

use crate::store::Store;

mod managed_preflight;
mod run;
#[cfg(all(debug_assertions, feature = "test-controls"))]
pub use run::RemovalHold;
pub use run::{Claimed, Run, RunProc, Said, Taken};

/// What [`Refusal::Exhausted`] names the supervisor's budget: its pidfds, one for each live `oci` box's init and
/// one for each run's program (decision 7), each box holding one at most.
pub const PIDFDS: &str = "pidfds of containers' inits and runs' programs";

/// Most exec'd programs the agent watches for one box at once (to reap on its delete), past which exec
/// is refused ([`Refusal::Exhausted`]), so the per-box tracking is bounded (CLAUDE.md §2). A program
/// the caller reaps is forgotten at once, and one let go once it has ended and been reaped (by the sweep, or
/// by the box's next exec, which looks), so this bounds concurrent unreaped execs, not a box's total;
/// it sits well above a box's steps, which run one at a time.
pub const PROGRAM_BUDGET: usize = 1_024;

/// How often the supervisor sweeps for orphaned zombies, and so the longest one waits to be reaped.
/// A stated bound on a zombie's life, not a tuning: a box would have to orphan and end
/// `pid_max / SWEEP_PERIOD` processes a second to fill the pid space with them (pid_max is 4,194,304
/// on a 64-bit kernel, proc_sys_kernel(5)).
pub const SWEEP_PERIOD: Duration = Duration::from_secs(1);

/// Most children one sweep looks at; more wait for the next sweep. Bounds a sweep's time (§2).
const SWEEP_CHILDREN: usize = 4_096;

/// Most threads of the agent whose children a sweep reads (`/proc/self/task/*/children`).
const SWEEP_TASKS: usize = 4_096;

/// Most epoll events one wake takes; more come on the next wait.
const EVENTS: usize = 256;

/// Held to read while a process is spawned (or a shim reaped) until its pid is in [`OWNED`]; held to
/// write by a sweep, which so never takes a child before it is owned. Process-wide, as the subreaper
/// attribute is: every node in the process spawns under it and every node's sweep honours it.
static SPAWNING: RwLock<()> = RwLock::new(());

/// The pids some node in this process still reaps itself: programs whose [`Proc`] may wait on them,
/// and watched inits. A sweep leaves them alone. Bounded by the live programs and inits, each held
/// to its own bound (the supervisor's init budget, a box's process bound).
static OWNED: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn owned() -> Result<MutexGuard<'static, BTreeSet<u32>>, Refusal> {
    OWNED.lock().map_err(poisoned)
}

/// One `oci` box's init, watched through its pidfd.
struct ContainerInit {
    pid: i32,
    /// Which process it is, as its box's record names it: its exit goes into that record alone.
    who: ProcId,
    /// `None` once reaped.
    pidfd: Option<OwnedFd>,
    /// How it ended, kept until the box's record holds it.
    exit: Option<Exit>,
    /// What an exec into the box needs of its container.
    container: Arc<Container>,
}

/// All native controllers occupy the existing init table, token namespace and epoll set.
enum Init {
    Container(ContainerInit),
    Managed(triomphe::Arc<ManagedVm>),
}

enum ManagedPhase {
    Reserved,
    Absent,
    Held { pid: i64, pidfd: OwnedFd },
    Observed { pid: i64, pidfd: OwnedFd },
    Running { pid: i64, pidfd: OwnedFd },
    Exited(Exit),
    Finalized(Exit),
}

/// The SAME fallibly allocated receipt is retained in Inits.rows and returned through the SDK.
/// The controller pidfd is reaped by this Supervisor, while cgroup.events proves whole-scope absence.
pub(crate) struct ManagedVm {
    owner: OwnerKey,
    scope: NativeScope,
    commitment: [u8; 32],
    cgroup: hex_runtime::linux::cgroup::BoxCgroup,
    events: Option<OwnedFd>,
    identity: OnceLock<shards_api::Identity>,
    pending_path: Mutex<Option<PathBuf>>,
    phase: Mutex<ManagedPhase>,
    #[cfg(test)]
    isolation_ack: OnceLock<Mutex<Option<std::io::PipeWriter>>>,
    supervisor: Weak<Supervisor>,
    token: u64,
}

/// What an exec into an `oci` box needs of its container that holds while its init lives: the bundle
/// and cgroup its state names. Kept with the watched init from the box's create, so an exec reads
/// nothing of the box's from disk (goal 3 plan step 6), and dropped with the init. No descriptor is
/// held for it: every descriptor the agent holds is copied into an exec's child and closed again at
/// its execve (`dup_fd` and `do_close_on_exec`), so one per box would grow every exec's cost with the
/// node's boxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub bundle: String,
    pub cgroup: Option<String>,
    /// Whether the init is the first process of a pid namespace its create made, so ending it ends the
    /// whole namespace ([`Forgotten::kill`]).
    pub pid_namespace: bool,
}

/// A box's running init, for an exec into it: its pid, a duplicate of its pidfd (by which the program
/// joins its namespaces, so they are that process's and never a later one's given its pid) and its
/// [`Container`].
pub struct Live {
    pub pid: i32,
    pub pidfd: OwnedFd,
    pub container: Arc<Container>,
}

#[derive(Default)]
struct Inits {
    /// One bounded table for containers and managed Box/Pool controllers, sorted by native owner.
    rows: Vec<(OwnerKey, Init)>,
    /// The existing epoll index, sorted by token. Neither index allocates during lookup.
    tokens: Vec<(u64, OwnerKey)>,
    /// The next epoll token, an init's or a run's.
    next_token: u64,
    /// Creates and runs under way that hold a place in the budget.
    reserved: usize,
    /// The most pidfds held at once, the inits' and the runs' together.
    budget: usize,
    /// The boxes runs made, each watched through its program's pidfd ([`run`]).
    runs: run::Runs,
}

impl Inits {
    fn position(&self, owner: OwnerRef<'_>) -> Result<usize, usize> {
        self.rows
            .binary_search_by(|(key, _)| key.as_ref().cmp(&owner))
    }

    fn get(&self, owner: OwnerRef<'_>) -> Option<&Init> {
        self.position(owner)
            .ok()
            .and_then(|at| self.rows.get(at).map(|(_, init)| init))
    }

    fn get_mut(&mut self, owner: OwnerRef<'_>) -> Option<&mut Init> {
        self.position(owner)
            .ok()
            .and_then(|at| self.rows.get_mut(at).map(|(_, init)| init))
    }

    fn token_owner(&self, token: u64) -> Option<&OwnerKey> {
        self.tokens
            .binary_search_by_key(&token, |(token, _)| *token)
            .ok()
            .and_then(|at| self.tokens.get(at).map(|(_, owner)| owner))
    }

    fn reserve_place(&mut self) -> Result<(), Refusal> {
        self.rows
            .len()
            .checked_add(self.reserved)
            .and_then(|count| count.checked_add(self.runs.len()))
            .and_then(|count| count.checked_add(1))
            .filter(|count| *count <= self.budget)
            .ok_or(Refusal::Exhausted {
                what: PIDFDS,
                bound: self.budget,
            })?;
        let additional = self.reserved.checked_add(1).ok_or_else(managed_storage)?;
        // Every outstanding Slot can publish one row in either order. Reserve both existing
        // indexes before any caller can spawn; publication cannot invoke the allocator.
        self.rows
            .try_reserve_exact(additional)
            .map_err(|_| managed_storage())?;
        self.tokens
            .try_reserve_exact(additional)
            .map_err(|_| managed_storage())?;
        self.reserved = additional;
        Ok(())
    }

    fn insert(
        &mut self,
        owner: OwnerKey,
        token_owner: OwnerKey,
        token: u64,
        init: Init,
    ) -> Result<(), Refusal> {
        let at = self
            .position(owner.as_ref())
            .err()
            .ok_or_else(managed_table_invalid)?;
        let token_at = self
            .tokens
            .binary_search_by_key(&token, |(token, _)| *token)
            .err()
            .ok_or_else(managed_table_invalid)?;
        if self.rows.len() == self.rows.capacity()
            || self.tokens.len() == self.tokens.capacity()
            || owner.as_ref() != token_owner.as_ref()
            || (matches!(&owner, OwnerKey::Pool(_)) && matches!(&init, Init::Container(_)))
        {
            return Err(managed_table_invalid());
        }
        // The search positions are in 0..=len and both vectors have a reserved slot. Mutation is
        // O(n) shifts under this short lock, with no OS/Store call and no allocator invocation.
        self.rows.insert(at, (owner, init));
        self.tokens.insert(token_at, (token, token_owner));
        Ok(())
    }

    fn remove(&mut self, owner: OwnerRef<'_>) -> Option<Init> {
        let at = self.position(owner).ok()?;
        self.tokens.retain(|(_, key)| key.as_ref() != owner);
        // A successful binary search identifies an existing row; neither operation changes it.
        Some(self.rows.remove(at).1)
    }

    /// The next epoll token, refused once they have run out.
    fn token(&mut self) -> Result<u64, Refusal> {
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .ok_or_else(|| Refusal::Broken("the supervisor's tokens ran out".to_owned()))?;
        Ok(token)
    }
}

struct ManagedRoot {
    prefix: String,
    // Keep sole native authority through the last supervising pass, including Node drop.
    _lease: std::fs::File,
}

/// The node's supervision of its boxes' processes.
pub struct Supervisor {
    epoll: Arc<OwnedFd>,
    inits: Mutex<Inits>,
    /// The node's box root as `/proc/<pid>/cgroup` names it: a zombie under it is the node's.
    box_root: String,
    /// Only this initialized Node's managed subtree belongs to its orphan reaper.
    managed_root: OnceLock<ManagedRoot>,
    store: Store,
    /// Why the last pass of the supervising thread failed, if it did; cleared by a clean pass.
    fault: Mutex<Option<String>>,
    /// Exec'd programs by box, each with a pidfd the agent reaps on the box's delete. An `oci` box's
    /// init blocks in the kernel's pid-namespace teardown until the programs in its pid namespace are
    /// reaped (decision 3); the caller's [`Proc`] usually reaps its program, but a delete beats it, so
    /// the agent reaps what is left and records each exit for the caller to read.
    programs: Mutex<BTreeMap<BoxId, Vec<Program>>>,
    /// A debug build's hold on the runs' removals, armed by the node's caller alone ([`run::Removals`]).
    #[cfg(all(debug_assertions, feature = "test-controls"))]
    removals: run::Removals,
}

/// An exec'd program the agent may reap on its box's delete: the agent's own duplicate of its pidfd,
/// and the slot its exit is recorded in for the caller's [`Proc`] once the agent has reaped it.
struct Program {
    pid: u32,
    pidfd: OwnedFd,
    exit: Arc<Mutex<Option<Exit>>>,
    /// Let go by its caller: no one waits for its exit, but the box's delete still reaps it.
    released: bool,
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a supervisor lock was poisoned".to_owned())
}

impl Supervisor {
    pub(crate) fn bind_managed_root(
        &self,
        path: &Path,
        lease: std::fs::File,
    ) -> Result<(), Refusal> {
        let relative = path
            .strip_prefix(hex_runtime::linux::cgroup::CGROUP_ROOT)
            .ok()
            .and_then(|path| path.to_str())
            .ok_or_else(managed_storage)?;
        let room = relative.len().checked_add(1).ok_or_else(managed_storage)?;
        let mut root = String::new();
        root.try_reserve_exact(room)
            .map_err(|_| managed_storage())?;
        root.push('/');
        root.push_str(relative);
        if self
            .managed_root
            .get()
            .is_some_and(|existing| existing.prefix != root)
        {
            return Err(managed_storage());
        }
        self.managed_root
            .set(ManagedRoot {
                prefix: root,
                _lease: lease,
            })
            .map_err(|_| managed_storage())
    }

    /// Reconstruct only this canonical scope. A missing/reused PID is not signal authority;
    /// helpers remain owned through the exact persisted scope and cgroup.events observation.
    pub(crate) fn recover_managed(
        self: &Arc<Self>,
        record: NativeRecord<'_>,
        cgroup: hex_runtime::linux::cgroup::BoxCgroup,
        identity: Option<&shards_api::Identity>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        if record.scope().is_none() {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        let path = identity.map_or(Path::new(""), |identity| identity.vsock.as_path());
        let native = self.reserve_managed(record, cgroup, path)?;
        let phase = match identity {
            None => ManagedPhase::Exited(Exit::Unknown),
            Some(identity) => {
                if record.init()
                    != Some(ProcId {
                        pid: identity.pid,
                        start_ticks: identity.started,
                    })
                    || identity.started == 0
                {
                    return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
                }
                let copied = copy_managed_identity(identity)?;
                native
                    .identity
                    .set(copied)
                    .map_err(|_| record.refusal(hex_api::LifecycleCause::Uncertain))?;
                let pid = i32::try_from(identity.pid)
                    .map_err(|_| record.refusal(hex_api::LifecycleCause::Uncertain))?;
                let raw = Pid::from_raw(pid)
                    .ok_or_else(|| record.refusal(hex_api::LifecycleCause::Uncertain))?;
                match rustix::process::pidfd_open(raw, PidfdFlags::empty()) {
                    Err(Errno::SRCH) => ManagedPhase::Exited(Exit::Unknown),
                    Err(error) => {
                        return Err(Refusal::os("observing canonical managed controller", error));
                    }
                    Ok(pidfd) => {
                        let stat = Stat::of(pid);
                        if stat.is_some_and(|stat| stat.start_ticks == identity.started)
                            && !ended(pidfd.as_fd())?
                        {
                            if !native.cgroup.procs()?.contains(&identity.pid) {
                                return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
                            }
                            ManagedPhase::Observed {
                                pid: i64::from(pid),
                                pidfd,
                            }
                        } else {
                            ManagedPhase::Exited(Exit::Unknown)
                        }
                    }
                }
            }
        };
        *native.phase.lock().map_err(poisoned)? = phase;
        {
            let mut phase = native.phase.lock().map_err(poisoned)?;
            if let ManagedPhase::Observed { pidfd, .. } = &*phase {
                epoll::add(
                    self.epoll.as_fd(),
                    pidfd.as_fd(),
                    EventData::new_u64(native.token),
                    EventFlags::IN,
                )
                .map_err(|e| Refusal::os("watching recovered managed controller", e))?;
                let observed = std::mem::replace(&mut *phase, ManagedPhase::Reserved);
                if let ManagedPhase::Observed { pid, pidfd } = observed {
                    *phase = ManagedPhase::Running { pid, pidfd };
                }
            }
        }
        Ok(shards_api::managed_receipt(native))
    }

    /// Allocate the native owner and publish its place in the EXISTING table before child creation.
    /// No global table lock spans spawn, identity I/O, record publication or gate release.
    pub(crate) fn reserve_managed(
        self: &Arc<Self>,
        record: NativeRecord<'_>,
        cgroup: hex_runtime::linux::cgroup::BoxCgroup,
        vsock: &Path,
    ) -> Result<triomphe::Arc<ManagedVm>, Refusal> {
        let owner = record.owner();
        let scope = record
            .scope()
            .ok_or_else(|| record.refusal(hex_api::LifecycleCause::Uncertain))?;
        let commitment = record.commitment()?;
        let mut slot = self.reserve()?;
        let token = self.inits()?.token()?;
        let mut path = PathBuf::new();
        path.try_reserve_exact(vsock.as_os_str().len())
            .map_err(|_| managed_storage())?;
        path.push(vsock);
        let events = match cgroup.populated_if_present()? {
            None => None,
            Some(_) => {
                let dir = cgroup.open_dir()?;
                Some(
                    rustix::fs::openat(
                        &dir,
                        "cgroup.events",
                        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(|error| Refusal::os("opening the managed scope's events", error))?,
                )
            }
        };
        let phase = if events.is_some() {
            ManagedPhase::Reserved
        } else {
            ManagedPhase::Absent
        };
        let native = triomphe::Arc::try_new(ManagedVm {
            owner: owner.try_owned()?,
            scope,
            commitment,
            cgroup,
            events,
            identity: OnceLock::new(),
            pending_path: Mutex::new(Some(path)),
            phase: Mutex::new(phase),
            #[cfg(test)]
            isolation_ack: OnceLock::new(),
            supervisor: Arc::downgrade(self),
            token,
        })
        .map_err(|_| managed_storage())?;
        let row_owner = owner.try_owned()?;
        let token_owner = owner.try_owned()?;
        let mut inits = self.inits()?;
        if inits.get(owner).is_some() {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        inits.insert(
            row_owner,
            token_owner,
            token,
            Init::Managed(triomphe::Arc::clone(&native)),
        )?;
        inits.reserved = inits.reserved.saturating_sub(1);
        slot.held = false;
        Ok(native)
    }

    pub(crate) fn bind_managed(
        &self,
        native: &triomphe::Arc<ManagedVm>,
        program: &hex_api::Program,
        stdio: hex_api::Stdio,
        launch: hex_runtime::linux::process::HeldLaunch,
        gate_budget: Duration,
        publish: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
    ) -> Result<shards_api::ManagedReceipt, Refusal> {
        self.same_managed(native)?;
        #[cfg(test)]
        let isolation_ack = native.take_isolation_ack()?;
        let _spawning = self.spawning()?;
        let gate = {
            let mut phase = native.phase.lock().map_err(poisoned)?;
            if !matches!(*phase, ManagedPhase::Reserved) {
                return Err(native.uncertain());
            }
            // The per-VM lock was acquired before creation. Ownership publication below cannot
            // fail through a poisoned lock, conversion, allocation or a second native owner.
            let child = hex_runtime::linux::process::exec_held(
                &native.cgroup,
                program,
                stdio,
                launch,
                gate_budget,
            )?;
            let (pid, pidfd, gate) = child.into_parts();
            *phase = ManagedPhase::Held { pid, pidfd };
            gate
        };
        {
            let phase = native.phase.lock().map_err(poisoned)?;
            if let ManagedPhase::Held { pidfd, .. } = &*phase {
                epoll::add(
                    self.epoll.as_fd(),
                    pidfd.as_fd(),
                    EventData::new_u64(native.token),
                    EventFlags::IN,
                )
                .map_err(|error| Refusal::os("watching the managed controller", error))?;
            } else {
                return Err(native.uncertain());
            }
        }
        #[cfg(test)]
        if let Some(writer) = isolation_ack {
            // Held is registered in the SAME owner/token table and watcher above. The orphan
            // sweep recognizes that exact PID; no owner/phase/table/reaper lock spans the probe.
            // Without a per-launch probe the production spawning/gate behavior is unchanged.
            drop(_spawning);
            notify_isolation_ack(&gate, writer)?;
        }
        let identity = native.observe_identity()?;
        publish(identity)?;
        gate.release()
            .map_err(|error| Refusal::os("releasing the recorded managed controller", error))?;
        let mut phase = native.phase.lock().map_err(poisoned)?;
        if matches!(*phase, ManagedPhase::Held { .. }) {
            let held = std::mem::replace(&mut *phase, ManagedPhase::Reserved);
            if let ManagedPhase::Held { pid, pidfd } = held {
                *phase = ManagedPhase::Running { pid, pidfd };
            }
        }
        Ok(shards_api::managed_receipt(triomphe::Arc::clone(native)))
    }

    fn same_managed(&self, native: &triomphe::Arc<ManagedVm>) -> Result<(), Refusal> {
        match self.inits()?.get(native.owner.as_ref()) {
            Some(Init::Managed(current)) if triomphe::Arc::ptr_eq(current, native) => Ok(()),
            _ => Err(native.uncertain()),
        }
    }

    pub(crate) fn observe_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<Option<shards_api::ManagedReceipt>, Refusal> {
        let owner = record.owner();
        let native = match self.inits()?.get(owner) {
            Some(Init::Managed(native)) => triomphe::Arc::clone(native),
            Some(Init::Container(_)) => {
                return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
            }
            None => return Ok(None),
        };
        native.matches_record(record)?;
        native.matches(identity)?;
        if identity.is_some()
            && matches!(
                *native.phase.lock().map_err(poisoned)?,
                ManagedPhase::Reserved
            )
        {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        Ok(Some(shards_api::managed_receipt(native)))
    }

    /// Joint plan step 14 (AC-T7.4.1): each managed VM box running now, by its box id, its native scope and its
    /// cgroup, for the node's charge learner to read and bound once a window. A pool's controller is no box's.
    pub(crate) fn running_vms(
        &self,
    ) -> Result<Vec<(BoxId, [u8; 16], hex_runtime::linux::cgroup::BoxCgroup)>, Refusal> {
        let managed: Vec<(BoxId, triomphe::Arc<ManagedVm>)> = {
            let inits = self.inits()?;
            let mut managed = Vec::new();
            managed
                .try_reserve_exact(inits.rows.len())
                .map_err(|_| Refusal::Exhausted {
                    what: "running microVMs",
                    bound: inits.rows.len(),
                })?;
            for (owner, init) in &inits.rows {
                if let (OwnerKey::Box(id), Init::Managed(native)) = (owner, init) {
                    managed.push((id.try_clone()?, triomphe::Arc::clone(native)));
                }
            }
            managed
        };
        let mut out = Vec::new();
        out.try_reserve_exact(managed.len())
            .map_err(|_| Refusal::Exhausted {
                what: "running microVMs",
                bound: managed.len(),
            })?;
        for (id, native) in managed {
            let running = matches!(
                *native.phase.lock().map_err(poisoned)?,
                ManagedPhase::Running { .. } | ManagedPhase::Observed { .. }
            );
            if running {
                out.push((id, *native.scope.bytes(), native.cgroup.clone()));
            }
        }
        Ok(out)
    }

    pub(crate) fn managed_usage(
        &self,
        record: NativeRecord<'_>,
    ) -> Result<hex_api::Usage, Refusal> {
        let native = match self.inits()?.get(record.owner()) {
            Some(Init::Managed(native)) => triomphe::Arc::clone(native),
            _ => return Err(record.refusal(hex_api::LifecycleCause::Uncertain)),
        };
        native.matches_record(record)?;
        if native.cgroup.populated_if_present()?.is_none() {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        native
            .cgroup
            .managed_usage()
            .map_err(|_| record.refusal(hex_api::LifecycleCause::Uncertain))
    }

    pub(crate) fn finalize_managed(
        &self,
        record: NativeRecord<'_>,
        identity: Option<&shards_api::Identity>,
    ) -> Result<(), Refusal> {
        let owner = record.owner();
        use shards_api::ManagedProcess;
        let native = match self.inits()?.get(owner) {
            Some(Init::Managed(native)) => triomphe::Arc::clone(native),
            Some(Init::Container(_)) => {
                return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
            }
            None => return Err(record.refusal(hex_api::LifecycleCause::Uncertain)),
        };
        native.matches_record(record)?;
        native.matches(identity)?;
        let end = native
            .terminal()
            .map_err(|_| record.refusal(hex_api::LifecycleCause::Uncertain))?
            .filter(|end| end.scope == shards_api::TerminalScope::Released)
            .ok_or_else(|| record.refusal(hex_api::LifecycleCause::Uncertain))?;
        // The backend has durably recorded Released before calling this function. Its canonical
        // method/device turns exclude reuse until this same Arc and cgroup have been finalized.
        if native.events.is_some() {
            native.cgroup.remove()?;
        } else if native.cgroup.populated_if_present()?.is_some() {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        self.same_managed(&native)?;
        *native.phase.lock().map_err(poisoned)? = ManagedPhase::Finalized(match end.status {
            shards_api::ManagedStatus::Code(code) => Exit::Code(code),
            shards_api::ManagedStatus::Signal(signal) => Exit::Signal(signal),
            shards_api::ManagedStatus::Unknown => Exit::Unknown,
        });
        let mut inits = self.inits()?;
        inits.remove(owner);
        Ok(())
    }

    fn managed_pid(&self, pid: i32) -> Result<bool, Refusal> {
        let inits = self.inits()?;
        for (_, init) in &inits.rows {
            if let Init::Managed(native) = init {
                let phase = native.phase.lock().map_err(poisoned)?;
                if matches!(&*phase, ManagedPhase::Held { pid: owned, .. } | ManagedPhase::Observed { pid: owned, .. } | ManagedPhase::Running { pid: owned, .. } if *owned == i64::from(pid))
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Makes this process a child subreaper and starts the supervising thread. `box_root` is the
    /// node's box root under the cgroup root; records of reaped inits go to `store`. `budget` is the most
    /// pidfds it holds, one per live `oci` box's init or run's program, past which `create` and `run` refuse with
    /// [`Refusal::Exhausted`] before making anything ([`crate::sizes::Sizes::inits`]: at most the node's boxes, each
    /// holding one at most, within its hard `RLIMIT_NOFILE` read at run time, getrlimit(2)). It was 65,536, systemd's
    /// figure copied (Ada's 2026-11-06 constants rule).
    pub fn start(box_root: &Path, store: Store, budget: usize) -> Result<Arc<Self>, Refusal> {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
            .map_err(|e| Refusal::os("making the node agent a child subreaper", e))?;
        let rel = box_root
            .strip_prefix(hex_runtime::linux::cgroup::CGROUP_ROOT)
            .map_err(|_| Refusal::Invalid {
                what: "box root",
                why: format!("{} isn't under the cgroup root", box_root.display()),
            })?;
        let epoll = Arc::new(
            epoll::create(CreateFlags::CLOEXEC)
                .map_err(|e| Refusal::os("making the supervisor's epoll set", e))?,
        );
        let supervisor = Arc::new(Self {
            epoll: Arc::clone(&epoll),
            inits: Mutex::new(Inits {
                budget,
                ..Inits::default()
            }),
            box_root: Path::new("/").join(rel).to_string_lossy().into_owned(),
            managed_root: OnceLock::new(),
            store,
            fault: Mutex::new(None),
            programs: Mutex::new(BTreeMap::new()),
            #[cfg(all(debug_assertions, feature = "test-controls"))]
            removals: run::Removals::default(),
        });
        let weak = Arc::downgrade(&supervisor);
        std::thread::Builder::new()
            .name("hex-supervisor".to_owned())
            .spawn(move || supervise(&epoll, &weak))
            .map_err(|e| Refusal::os("starting the supervisor thread", e))?;
        Ok(supervisor)
    }

    fn inits(&self) -> Result<MutexGuard<'_, Inits>, Refusal> {
        self.inits.lock().map_err(poisoned)
    }

    /// Sets the budget, the inits' and the runs' pidfds together.
    pub fn set_budget(&self, budget: usize) -> Result<(), Refusal> {
        self.inits()?.budget = budget;
        Ok(())
    }

    /// Takes a place in the budget for a create or a run, or refuses with [`Refusal::Exhausted`] naming
    /// the budget when none is left. The place is given back when the [`Slot`] is dropped, or turned
    /// into the init's by [`Supervisor::watch`], or the run's by [`Supervisor::watch_run`].
    pub fn reserve(self: &Arc<Self>) -> Result<Slot, Refusal> {
        let mut inits = self.inits()?;
        if let Some(fault) = self.fault.lock().map_err(poisoned)?.as_ref() {
            return Err(Refusal::Broken(format!("the node's supervisor: {fault}")));
        }
        inits.reserve_place()?;
        Ok(Slot {
            supervisor: Arc::clone(self),
            held: true,
        })
    }

    /// Held while a process is spawned and until it is owned, so a sweep never reaps it first.
    pub fn spawning(&self) -> Result<RwLockReadGuard<'static, ()>, Refusal> {
        SPAWNING.read().map_err(poisoned)
    }

    /// Watches box `id`'s init, `pid`, which create recorded with `start_ticks`: opens its pidfd,
    /// then checks it is the agent's child with that start time, so a later process given its pid
    /// is never taken for it (proc_pid_stat(5) fields 4 and 22). Uses `slot`'s place in the budget.
    /// `container` is kept with it for the box's execs.
    pub fn watch(
        &self,
        mut slot: Slot,
        id: &BoxId,
        pid: i32,
        start_ticks: u64,
        container: Container,
    ) -> Result<(), Refusal> {
        let not_a_pid = || Refusal::Invalid {
            what: "pid",
            why: format!("{pid} isn't a process id"),
        };
        let raw = Pid::from_raw(pid).ok_or_else(not_a_pid)?;
        let who = ProcId {
            pid: u32::try_from(pid).map_err(|_| not_a_pid())?,
            start_ticks,
        };
        let pidfd = rustix::process::pidfd_open(raw, PidfdFlags::empty())
            .map_err(|e| Refusal::os("pidfd_open of a container's init", e))?;
        let stat = Stat::of(pid)
            .ok_or_else(|| Refusal::Broken(format!("box {id}'s init {pid} has no /proc stat")))?;
        let me = rustix::process::getpid().as_raw_nonzero().get();
        if stat.ppid != me || stat.start_ticks != start_ticks {
            return Err(Refusal::Broken(format!(
                "box {id}'s init {pid} isn't the node agent's child started at {start_ticks} \
                 (parent {}, started at {})",
                stat.ppid, stat.start_ticks
            )));
        }
        let owner = OwnerKey::Box(id.try_clone()?);
        let token_owner = owner.try_clone()?;
        let mut inits = self.inits()?;
        let token = inits.token()?;
        epoll::add(
            self.epoll.as_fd(),
            pidfd.as_fd(),
            EventData::new_u64(token),
            EventFlags::IN,
        )
        .map_err(|e| Refusal::os("watching a container's init", e))?;
        owned()?.insert(who.pid);
        inits.insert(
            owner,
            token_owner,
            token,
            Init::Container(ContainerInit {
                pid,
                who,
                pidfd: Some(pidfd),
                exit: None,
                container: Arc::new(container),
            }),
        )?;
        inits.reserved = inits.reserved.saturating_sub(1);
        slot.held = false;
        Ok(())
    }

    /// Box `id`'s init, if it is watched and runs: not yet reaped, and its pidfd not readable, as it is
    /// once the init has ended (pidfd_open(2)). `None` for a box with no such init: a `process` box, an
    /// `oci` box mid-create or stopped, or no box.
    pub fn live(&self, id: &BoxId) -> Result<Option<Live>, Refusal> {
        let inits = self.inits()?;
        let Some((init, pidfd)) = inits.get(OwnerRef::Box(id)).and_then(|init| match init {
            Init::Container(init) => init.pidfd.as_ref().map(|fd| (init, fd)),
            Init::Managed(_) => None,
        }) else {
            return Ok(None);
        };
        if ended(pidfd.as_fd())? {
            return Ok(None);
        }
        let dup = rustix::io::fcntl_dupfd_cloexec(pidfd, 0)
            .map_err(|e| Refusal::os("duplicating a container init's pidfd", e))?;
        Ok(Some(Live {
            pid: init.pid,
            pidfd: dup,
            container: Arc::clone(&init.container),
        }))
    }

    /// Whether box `id` is an `oci` box whose init the supervisor watches, running or ended: read from its table alone,
    /// so a call that only asks what kind of box it is reads nothing from disk ([`crate::Node`]'s exec forwarding).
    pub fn watches_container(&self, id: &BoxId) -> Result<bool, Refusal> {
        Ok(matches!(
            self.inits()?.get(OwnerRef::Box(id)),
            Some(Init::Container(_))
        ))
    }

    /// How box `id`'s init ended, if it is watched and has: reaped now if it has ended and the
    /// supervising thread hasn't come to it yet, so inspect never reads a stopped box without its
    /// exit. `None` when it runs or isn't watched.
    pub fn exit(&self, id: &BoxId) -> Result<Option<Exit>, Refusal> {
        self.collect(id)
    }

    /// Stops watching box `id`'s init, which the box's delete is about to end; returns it, with its pidfd
    /// if it hasn't been reaped, for the delete to end and reap once the box is empty. An exit being
    /// recorded as it's forgotten goes into this box's record alone, which the delete removes. `None` for
    /// a box whose init isn't watched: a `process` box, or one the node took back after a restart.
    pub fn forget(&self, id: &BoxId) -> Result<Option<Forgotten>, Refusal> {
        let mut inits = self.inits()?;
        if matches!(inits.get(OwnerRef::Box(id)), Some(Init::Managed(_))) {
            return Err(managed_uncertain(id));
        }
        let Some(Init::Container(init)) = inits.remove(OwnerRef::Box(id)) else {
            return Ok(None);
        };
        let pidfd = init.pidfd;
        if pidfd.is_none() {
            disown(init.pid)?;
        }
        if let Some(fd) = &pidfd {
            // Out of the epoll set, so its end doesn't wake the thread until the delete reaps it.
            epoll::delete(self.epoll.as_fd(), fd.as_fd())
                .map_err(|e| Refusal::os("unwatching a deleted box's init", e))?;
        }
        // Still owned while its pidfd is held: the delete reaps it ([`reap`]), which disowns it.
        Ok(Some(Forgotten {
            pid: init.pid,
            pidfd,
            pid_namespace: init.container.pid_namespace,
        }))
    }

    /// A [`Proc`] whose pid the sweep leaves to it until it has reaped it or is dropped.
    pub fn own(&self, proc: Box<dyn Proc>) -> Result<Box<dyn Proc>, Refusal> {
        let pid = proc.pid();
        owned()?.insert(pid);
        Ok(Box::new(Owned {
            proc,
            pid,
            released: false,
        }))
    }

    /// Owns an exec'd program of box `id` that the agent may have to reap on the box's delete: `proc`
    /// reaps it for the caller, and `pidfd` (the agent's own duplicate) lets the agent reap it instead
    /// if a delete beats the caller. The container's init blocks in the kernel until the programs in its
    /// pid namespace are reaped, so a delete that didn't reap them would never empty the box (decision
    /// 3). Refused past [`PROGRAM_BUDGET`] watched for the box at once, and past the node's budget kept for every box
    /// together, the inits', which is the programs its socket watches and which its start fits to its descriptor limit
    /// with every pidfd counted (the lead, 2026-11-11; `netns::hold_descriptors`).
    pub fn own_program(
        self: &Arc<Self>,
        id: &BoxId,
        proc: Box<dyn Proc>,
        pidfd: OwnedFd,
    ) -> Result<Box<dyn Proc>, Refusal> {
        let pid = proc.pid();
        let exit = Arc::new(Mutex::new(None));
        let budget = self.inits()?.budget;
        {
            let mut programs = self.programs.lock().map_err(poisoned)?;
            let watched = programs.entry(id.clone()).or_default();
            // A program let go that has ended is reaped here, or was by the sweep, and no longer counts.
            watched.retain(|p| !p.released || !ended_and_reaped(p.pidfd.as_fd()));
            if watched.len() >= PROGRAM_BUDGET {
                return Err(Refusal::Exhausted {
                    what: "exec'd programs watched for one box",
                    bound: PROGRAM_BUDGET,
                });
            }
            let kept = |programs: &BTreeMap<BoxId, Vec<Program>>| {
                programs
                    .values()
                    .map(Vec::len)
                    .fold(0, usize::saturating_add)
            };
            if kept(&programs) >= budget {
                // Every box's programs let go that have ended no longer count, as each box's next exec finds.
                for watched in programs.values_mut() {
                    watched.retain(|p| !p.released || !ended_and_reaped(p.pidfd.as_fd()));
                }
                if kept(&programs) >= budget {
                    return Err(Refusal::Exhausted {
                        what: "exec'd programs the node keeps to reap",
                        bound: budget,
                    });
                }
            }
            programs.entry(id.clone()).or_default().push(Program {
                pid,
                pidfd,
                exit: Arc::clone(&exit),
                released: false,
            });
        }
        owned()?.insert(pid);
        Ok(Box::new(ProgramProc {
            supervisor: Arc::clone(self),
            id: id.clone(),
            proc,
            pid,
            exit,
            done: false,
        }))
    }

    /// Reaps every exec'd program of box `id` the agent still holds, those let go among them, recording each exit
    /// for its caller's [`Proc`] to read. The box's delete calls this after its kill, so the programs — killed, in
    /// the container's pid namespace — are reaped, letting the init finish exiting and the box empty: an init that is
    /// its namespace's first process doesn't finish until every process in the namespace is reaped
    /// (kernel/pid_namespace.c, `zap_pid_ns_processes`), so a program left to the sweep held the box for as long as
    /// [`SWEEP_PERIOD`] (the waits census's run 5: `oci_box`'s delete "326.645366ms after SIGKILL" in every loop).
    /// Each program's end is waited for by `ended`, its caller's wait on a killed box's processes (the node's, decision
    /// 9: it follows the box past its learned wait, and gives up only on a wedge), then the program reaped; one a
    /// wedge holds is left, and the box's emptying reports it, as it does for any wedge.
    pub fn reap_programs(
        &self,
        id: &BoxId,
        ended: &mut dyn FnMut(BorrowedFd<'_>) -> Result<bool, Refusal>,
    ) -> Result<(), Refusal> {
        let watched = self
            .programs
            .lock()
            .map_err(poisoned)?
            .remove(id)
            .unwrap_or_default();
        let mut owned = owned()?;
        for program in watched {
            ended(program.pidfd.as_fd())?;
            reap_into(program.pidfd.as_fd(), &program.exit)?;
            owned.remove(&program.pid);
        }
        Ok(())
    }

    /// Keeps a program its caller let go unreaped for the box's delete to reap, its exit read by no one; the sweep
    /// reaps it instead if it ends first.
    fn let_go(&self, id: &BoxId, pid: u32) {
        if let Ok(mut programs) = self.programs.lock()
            && let Some(program) = programs
                .get_mut(id)
                .and_then(|watched| watched.iter_mut().find(|p| p.pid == pid))
        {
            program.released = true;
        }
    }

    /// Stops watching a program the caller has reaped, freeing the agent's pidfd for it, so the box's delete no
    /// longer looks for it.
    fn forget_program(&self, id: &BoxId, pid: u32) {
        if let Ok(mut programs) = self.programs.lock()
            && let Some(watched) = programs.get_mut(id)
        {
            watched.retain(|p| p.pid != pid);
            if watched.is_empty() {
                programs.remove(id);
            }
        }
    }

    /// Reaps box `id`'s init if it has ended, and records how in its record. The record is written
    /// with no lock of the supervisor's held, so one box's durable record write never holds up
    /// another box's create, exec or delete, which take `inits` (goal 5 plan step 2); and only into
    /// the record naming this init, so an exit recorded as its box is deleted, and another made under
    /// its id, goes into no other box's record.
    fn collect(&self, id: &BoxId) -> Result<Option<Exit>, Refusal> {
        self.collect_owner(OwnerRef::Box(id))
    }

    fn collect_owner(&self, owner: OwnerRef<'_>) -> Result<Option<Exit>, Refusal> {
        let managed = {
            let inits = self.inits()?;
            match inits.get(owner) {
                Some(Init::Managed(managed)) => Some(triomphe::Arc::clone(managed)),
                _ => None,
            }
        };
        if let Some(managed) = managed {
            return managed.collect().map_err(|_| managed.uncertain());
        }
        let OwnerRef::Box(id) = owner else {
            return Ok(None);
        };
        let (who, exit) = {
            let mut inits = self.inits()?;
            let Some(Init::Container(init)) = inits.get_mut(OwnerRef::Box(id)) else {
                return Ok(None);
            };
            if let Some(pidfd) = &init.pidfd {
                match rustix::process::waitid(
                    WaitId::PidFd(pidfd.as_fd()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
                ) {
                    Ok(None) => return Ok(None),
                    Ok(Some(status)) => init.exit = Some(exit_of(&status)),
                    // Not the agent's child any more: something else in this process reaped it.
                    Err(Errno::CHILD) => init.exit = Some(Exit::Unknown),
                    Err(e) => return Err(Refusal::os("reaping a container's init", e)),
                }
                // Closing the pidfd takes it out of the epoll set.
                init.pidfd = None;
                owned()?.remove(&init.who.pid);
            }
            (init.who, init.exit)
        };
        // Kept in memory until the record holds it; a failed write is tried again on the next
        // inspect or sweep, never dropped (§3). A box with no readable record has none to hold it:
        // inspect refuses such a box as broken, and recovery removes it. One change of the record, so
        // a count the socket writes at the same moment isn't lost.
        self.store.update(id, |record| {
            if record.init != Some(who) {
                return false;
            }
            record.exit = exit;
            true
        })?;
        let mut inits = self.inits()?;
        if inits
            .get(OwnerRef::Box(id))
            .is_some_and(|init| matches!(init, Init::Container(init) if init.who == who))
        {
            inits.remove(OwnerRef::Box(id));
        }
        Ok(exit)
    }

    /// The supervising thread's pass over the inits and runs whose pidfds turned readable.
    fn ready(&self, tokens: &[u64]) -> Result<(), Refusal> {
        let (owners, runs) = {
            let inits = self.inits()?;
            let mut owners = Vec::new();
            let mut runs = Vec::new();
            owners
                .try_reserve_exact(tokens.len())
                .map_err(|_| managed_storage())?;
            runs.try_reserve_exact(tokens.len())
                .map_err(|_| managed_storage())?;
            for token in tokens {
                if let Some(owner) = inits.token_owner(*token) {
                    owners.push(owner.try_clone()?);
                }
                if let Some(run) = inits.runs.by_token(*token) {
                    runs.push(run);
                }
            }
            (owners, runs)
        };
        let mut first_error = self.runs_ready(&runs).err();
        for owner in &owners {
            if let Err(error) = self.collect_owner(owner.as_ref()) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Records exits a failed write left in memory, then reaps the box processes orphaned to the agent
    /// that have ended. The exits are recorded first, with no lock held across their writes; the
    /// reaping is under the spawning lock, so no child is taken before it is registered.
    fn sweep(&self) -> Result<usize, Refusal> {
        let unrecorded = {
            let inits = self.inits()?;
            let mut ids = Vec::new();
            ids.try_reserve_exact(inits.rows.len())
                .map_err(|_| managed_storage())?;
            for (owner, init) in &inits.rows {
                let pending = match init {
                    Init::Container(init) => init.exit.is_some(),
                    // If epoll registration failed, the held child is still owned. The existing
                    // orphan sweep must reap that cancelled controller; no second reaper/timer.
                    Init::Managed(native) => matches!(
                        *native.phase.lock().map_err(poisoned)?,
                        ManagedPhase::Held { .. } | ManagedPhase::Observed { .. }
                    ),
                };
                if pending {
                    ids.push(owner.try_clone()?);
                }
            }
            ids
        };
        for owner in &unrecorded {
            self.collect_owner(owner.as_ref())?;
        }
        let _no_spawns = SPAWNING.write().map_err(poisoned)?;
        let owned = owned()?;
        let mut reaped = 0usize;
        for pid in children()? {
            if u32::try_from(pid).is_ok_and(|p| owned.contains(&p)) {
                continue;
            }
            if self.managed_pid(pid)? {
                continue;
            }
            if self.reap_orphan(pid)? {
                reaped = reaped.saturating_add(1);
            }
        }
        Ok(reaped)
    }

    /// Reaps `pid` if it is an ended process in one of the node's boxes. Its pidfd is opened first,
    /// so the checks and the reap are of the same process.
    fn reap_orphan(&self, pid: i32) -> Result<bool, Refusal> {
        let Some(raw) = Pid::from_raw(pid) else {
            return Ok(false);
        };
        let Ok(pidfd) = rustix::process::pidfd_open(raw, PidfdFlags::empty()) else {
            // Gone already (reaped by its owner since the children were read).
            return Ok(false);
        };
        let ended = Stat::of(pid).is_some_and(|s| s.zombie);
        if !ended || !self.in_boxes(pid) {
            return Ok(false);
        }
        match rustix::process::waitid(
            WaitId::PidFd(pidfd.as_fd()),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        ) {
            Ok(status) => Ok(status.is_some()),
            Err(Errno::CHILD) => Ok(false),
            Err(e) => Err(Refusal::os("reaping an orphaned box process", e)),
        }
    }

    /// Whether process `pid` is (or, ended, was) in a cgroup under the node's box root. A zombie's
    /// `/proc/<pid>/cgroup` still names the cgroup it ended in.
    fn in_boxes(&self, pid: i32) -> bool {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            return false;
        };
        text.lines()
            .filter_map(|l| l.strip_prefix("0::"))
            .any(|path| {
                let Some(rest) = path.strip_prefix(self.box_root.as_str()) else {
                    return false;
                };
                if rest.starts_with("/.vm/") {
                    return self.managed_root.get().is_some_and(|root| {
                        path.strip_prefix(root.prefix.as_str())
                            .is_some_and(|child| child.starts_with('/'))
                    });
                }
                rest.starts_with('/')
            })
    }

    fn note(&self, outcome: Result<(), Refusal>) {
        if let Ok(mut fault) = self.fault.lock() {
            *fault = outcome.err().map(|e| e.to_string());
        }
    }
}

fn copy_managed_identity(identity: &shards_api::Identity) -> Result<shards_api::Identity, Refusal> {
    let mut vsock = PathBuf::new();
    vsock
        .try_reserve_exact(identity.vsock.as_os_str().len())
        .map_err(|_| managed_storage())?;
    vsock.push(&identity.vsock);
    Ok(shards_api::Identity {
        pid: identity.pid,
        started: identity.started,
        vsock,
    })
}

fn managed_storage() -> Refusal {
    Refusal::Exhausted {
        what: "managed native receipt",
        bound: 1,
    }
}

#[cfg(test)]
fn notify_isolation_ack(
    gate: &hex_runtime::linux::sys::HeldGate,
    writer: std::io::PipeWriter,
) -> Result<(), Refusal> {
    gate.wait_isolated()
        .map_err(|error| Refusal::os("observing this child's isolation acknowledgment", error))?;
    write_isolation_ack(writer)
}

#[cfg(test)]
fn write_isolation_ack(writer: std::io::PipeWriter) -> Result<(), Refusal> {
    // Fresh, private NONBLOCK pipe; exactly one byte, one attempt, then owned writer closure.
    // The notification carries observation only. It is never the held gate's authorization byte.
    match rustix::io::write(&writer, &[2]) {
        Ok(1) => Ok(()),
        Ok(_) => Err(Refusal::os(
            "notifying per-launch descriptor isolation",
            Errno::IO,
        )),
        Err(error) => Err(Refusal::os(
            "notifying per-launch descriptor isolation",
            error,
        )),
    }
}

fn managed_table_invalid() -> Refusal {
    let message = "native supervisor publication has no unique reserved owner slot";
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return managed_storage();
    }
    why.push_str(message);
    Refusal::Broken(why)
}

fn managed_uncertain(id: &BoxId) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: hex_api::LifecycleCause::Uncertain,
        },
        Err(error) => error,
    }
}

fn managed_poisoned<T>(_: std::sync::PoisonError<T>) -> shards_api::Refusal {
    shards_api::Refusal::Scope(shards_api::ProcessFailure::Poisoned {
        what: "Linux native scope",
    })
}

fn managed_os(operation: shards_api::ProcessOperation, error: Errno) -> shards_api::Refusal {
    shards_api::Refusal::Scope(shards_api::ProcessFailure::Os {
        operation,
        code: error.raw_os_error(),
    })
}

impl ManagedVm {
    /// Installs one private notification before spawn on this SAME preallocated receipt. A
    /// duplicate (including after consumption), non-Reserved phase or native setup error refuses.
    #[cfg(test)]
    pub(crate) fn prepare_isolation_ack(&self) -> Result<std::io::PipeReader, Refusal> {
        let phase = self.phase.lock().map_err(poisoned)?;
        if !matches!(*phase, ManagedPhase::Reserved) || self.isolation_ack.get().is_some() {
            return Err(self.uncertain());
        }
        let (reader, writer) = std::io::pipe()
            .map_err(|error| Refusal::os("creating per-launch isolation acknowledgment", error))?;
        let flags = rustix::fs::fcntl_getfl(&writer)
            .map_err(|error| Refusal::os("observing isolation acknowledgment flags", error))?;
        rustix::fs::fcntl_setfl(&writer, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|error| Refusal::os("bounding isolation acknowledgment write", error))?;
        self.isolation_ack
            .set(Mutex::new(Some(writer)))
            .map_err(|_| self.uncertain())?;
        Ok(reader)
    }

    #[cfg(test)]
    fn take_isolation_ack(&self) -> Result<Option<std::io::PipeWriter>, Refusal> {
        self.isolation_ack
            .get()
            .map(|ack| ack.lock().map_err(poisoned).map(|mut ack| ack.take()))
            .transpose()
            .map(Option::flatten)
    }

    fn uncertain(&self) -> Refusal {
        match self.owner.as_ref() {
            OwnerRef::Box(id) => managed_uncertain(id),
            OwnerRef::Pool(pool) => Refusal::Backing {
                key: hex_api::BackingKey {
                    pool: *pool,
                    generation: *self.scope.bytes(),
                },
                cause: hex_api::LifecycleCause::Uncertain,
            },
        }
    }

    fn matches_record(&self, record: NativeRecord<'_>) -> Result<(), Refusal> {
        if self.owner.as_ref() != record.owner()
            || Some(self.scope) != record.scope()
            || self.commitment != record.commitment()?
        {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        Ok(())
    }

    fn observe_identity(&self) -> Result<&shards_api::Identity, Refusal> {
        if let Some(identity) = self.identity.get() {
            return Ok(identity);
        }
        let pid = match &*self.phase.lock().map_err(poisoned)? {
            ManagedPhase::Held { pid, .. } | ManagedPhase::Running { pid, .. } => *pid,
            _ => return Err(self.uncertain()),
        };
        let raw = i32::try_from(pid).map_err(|_| self.uncertain())?;
        let stat = Stat::of(raw).ok_or_else(|| self.uncertain())?;
        if stat.ppid != rustix::process::getpid().as_raw_nonzero().get() || stat.start_ticks == 0 {
            return Err(self.uncertain());
        }
        let vsock = self
            .pending_path
            .lock()
            .map_err(poisoned)?
            .take()
            .ok_or_else(|| self.uncertain())?;
        self.identity
            .set(shards_api::Identity {
                pid: u32::try_from(pid).map_err(|_| self.uncertain())?,
                started: stat.start_ticks,
                vsock,
            })
            .map_err(|_| self.uncertain())?;
        self.identity.get().ok_or_else(|| self.uncertain())
    }

    fn matches(&self, identity: Option<&shards_api::Identity>) -> Result<(), Refusal> {
        if let Some(expected) = identity
            && self.identity.get() != Some(expected)
        {
            return Err(self.uncertain());
        }
        Ok(())
    }

    fn collect(&self) -> Result<Option<Exit>, shards_api::Refusal> {
        let mut phase = self.phase.lock().map_err(managed_poisoned)?;
        let exit = match &*phase {
            ManagedPhase::Reserved => return Ok(None),
            ManagedPhase::Absent => return Ok(Some(Exit::Unknown)),
            ManagedPhase::Exited(exit) | ManagedPhase::Finalized(exit) => return Ok(Some(*exit)),
            ManagedPhase::Held { pidfd, .. }
            | ManagedPhase::Observed { pidfd, .. }
            | ManagedPhase::Running { pidfd, .. } => {
                match rustix::process::waitid(
                    WaitId::PidFd(pidfd.as_fd()),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
                ) {
                    Ok(Some(status)) => Some(exit_of(&status)),
                    Ok(None) => None,
                    Err(Errno::CHILD) => {
                        // Recovery may hold a process reparented elsewhere. ECHILD alone is not an
                        // exit; a readable exact pidfd must independently establish its termination.
                        let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
                        rustix::event::poll(
                            &mut fds,
                            Some(&Timespec {
                                tv_sec: 0,
                                tv_nsec: 0,
                            }),
                        )
                        .map_err(|error| {
                            managed_os(shards_api::ProcessOperation::QueryScope, error)
                        })?;
                        fds.first()
                            .filter(|fd| fd.revents().contains(PollFlags::IN))
                            .map(|_| Exit::Unknown)
                    }
                    Err(error) => {
                        return Err(managed_os(shards_api::ProcessOperation::WaitForExit, error));
                    }
                }
            }
        };
        if let Some(exit) = exit {
            *phase = ManagedPhase::Exited(exit);
        }
        Ok(exit)
    }

    fn populated(&self) -> Result<bool, shards_api::Refusal> {
        let Some(events) = &self.events else {
            return Ok(false);
        };
        // Same bounded kernel-file read as BoxCgroup::wait_empty, without adding its periodic poll
        // cap. The native wait below subscribes to cgroup.events and the controller pidfd directly.
        let mut bytes = [0; 256];
        let count = rustix::io::pread(events, &mut bytes, 0)
            .map_err(|error| managed_os(shards_api::ProcessOperation::QueryScope, error))?;
        if count == bytes.len() {
            return Err(shards_api::Refusal::Scope(
                shards_api::ProcessFailure::BufferBounds,
            ));
        }
        let text = bytes
            .get(..count)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or(shards_api::Refusal::Scope(
                shards_api::ProcessFailure::BufferBounds,
            ))?;
        hex_core::cgroup::populated(text).ok_or(shards_api::Refusal::Scope(
            shards_api::ProcessFailure::InvalidStartRecord,
        ))
    }

    fn poll_pidfd(&self) -> Result<Option<OwnedFd>, shards_api::Refusal> {
        let phase = self.phase.lock().map_err(managed_poisoned)?;
        match &*phase {
            ManagedPhase::Held { pidfd, .. }
            | ManagedPhase::Observed { pidfd, .. }
            | ManagedPhase::Running { pidfd, .. } => rustix::io::fcntl_dupfd_cloexec(pidfd, 3)
                .map(Some)
                .map_err(|error| managed_os(shards_api::ProcessOperation::QueryScope, error)),
            _ => Ok(None),
        }
    }
}

impl shards_api::ManagedProcess for ManagedVm {
    fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
        self.identity.get().ok_or(shards_api::Refusal::Launch(
            shards_api::LaunchError::Observer,
        ))
    }

    fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
        let exit = match self.collect()? {
            Some(exit) => exit,
            None if matches!(
                *self.phase.lock().map_err(managed_poisoned)?,
                ManagedPhase::Reserved
            ) && !self.populated()? =>
            {
                Exit::Unknown
            }
            None => return Ok(None),
        };
        let finalized = matches!(
            *self.phase.lock().map_err(managed_poisoned)?,
            ManagedPhase::Finalized(_)
        );
        let scope = if finalized || !self.populated()? {
            shards_api::TerminalScope::Released
        } else {
            shards_api::TerminalScope::Process
        };
        Ok(Some(shards_api::ManagedEnd {
            status: match exit {
                Exit::Code(code) => shards_api::ManagedStatus::Code(code),
                Exit::Signal(signal) => shards_api::ManagedStatus::Signal(signal),
                Exit::Unknown => shards_api::ManagedStatus::Unknown,
            },
            scope,
        }))
    }

    fn stop(&self) -> Result<(), shards_api::Refusal> {
        // No observation lock is held. Helpers inherit the same cgroup from their first native
        // instruction; stopping the controller alone cannot grant terminal release.
        if matches!(
            *self.phase.lock().map_err(managed_poisoned)?,
            ManagedPhase::Absent | ManagedPhase::Finalized(_)
        ) {
            return Ok(());
        }
        self.cgroup
            .kill()
            .map_err(|_| shards_api::Refusal::Launch(shards_api::LaunchError::NativeRefused))
    }

    fn wait_for(
        &self,
        patience: Duration,
    ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
        let deadline = if patience == Duration::MAX {
            None
        } else {
            Some(
                Instant::now()
                    .checked_add(patience)
                    .ok_or(shards_api::Refusal::Scope(
                        shards_api::ProcessFailure::IdentifierOutOfRange,
                    ))?,
            )
        };
        loop {
            if let Some(end) = self.terminal()?
                && end.scope == shards_api::TerminalScope::Released
            {
                return Ok(Some(end));
            }
            let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if left.is_some_and(|left| left.is_zero()) {
                return Ok(None);
            }
            let timeout = left
                .map(|left| -> Result<Timespec, shards_api::Refusal> {
                    Ok(Timespec {
                        tv_sec: i64::try_from(left.as_secs()).map_err(|_| {
                            shards_api::Refusal::Scope(
                                shards_api::ProcessFailure::IdentifierOutOfRange,
                            )
                        })?,
                        tv_nsec: i64::from(left.subsec_nanos()),
                    })
                })
                .transpose()?;
            let events = self.events.as_ref().ok_or(shards_api::Refusal::Launch(
                shards_api::LaunchError::Observer,
            ))?;
            let pidfd = self.poll_pidfd()?;
            let waited = match pidfd.as_ref() {
                Some(pidfd) => rustix::event::poll(
                    &mut [
                        PollFd::new(events, PollFlags::PRI),
                        PollFd::new(pidfd, PollFlags::IN),
                    ],
                    timeout.as_ref(),
                ),
                None => rustix::event::poll(
                    &mut [PollFd::new(events, PollFlags::PRI)],
                    timeout.as_ref(),
                ),
            };
            match waited {
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => {
                    return Err(managed_os(shards_api::ProcessOperation::WaitForExit, error));
                }
            }
            if self.supervisor.upgrade().is_none() {
                return Err(shards_api::Refusal::Launch(
                    shards_api::LaunchError::Observer,
                ));
            }
        }
    }
}

/// Whether the process a pidfd names has ended: its pidfd is readable then (pidfd_open(2)). A poll
/// that doesn't wait.
fn ended(pidfd: std::os::fd::BorrowedFd<'_>) -> Result<bool, Refusal> {
    let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now))
        .map_err(|e| Refusal::os("looking at a container init's pidfd", e))?;
    Ok(fds.iter().any(|f| f.revents().contains(PollFlags::IN)))
}

/// The supervising thread: waits on the inits' pidfds, collecting each that ends, and sweeps each
/// [`SWEEP_PERIOD`]; ends once the node is dropped.
fn supervise(epoll: &OwnedFd, supervisor: &Weak<Supervisor>) {
    let period = Timespec {
        tv_sec: i64::try_from(SWEEP_PERIOD.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(SWEEP_PERIOD.subsec_nanos()),
    };
    let mut events = Vec::with_capacity(EVENTS);
    let mut swept = Instant::now();
    loop {
        events.clear();
        let waited = match epoll::wait(epoll.as_fd(), spare_capacity(&mut events), Some(&period)) {
            Ok(_) | Err(Errno::INTR) => Ok(()),
            Err(e) => Err(Refusal::os("waiting on the inits' pidfds", e)),
        };
        let Some(supervisor) = supervisor.upgrade() else {
            return;
        };
        if let Err(e) = waited {
            // epoll_wait fails only on a bad set or buffer (epoll_wait(2)): the supervisor can't go
            // on. The fault stays, so every later create is refused with it (§3).
            supervisor.note(Err(e));
            return;
        }
        let tokens: Vec<u64> = events.iter().map(|e| e.data.u64()).collect();
        let mut outcome = supervisor.ready(&tokens);
        if swept.elapsed() >= SWEEP_PERIOD {
            swept = Instant::now();
            outcome = outcome.and_then(|()| supervisor.sweep().map(|_| ()));
        }
        // A fault is kept for create to refuse on until a pass is clean (§3).
        supervisor.note(outcome);
    }
}

/// A place in the budget, given back on drop unless [`Supervisor::watch`] or [`Supervisor::watch_run`] took it.
pub struct Slot {
    supervisor: Arc<Supervisor>,
    held: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.held
            && let Ok(mut inits) = self.supervisor.inits.lock()
        {
            inits.reserved = inits.reserved.saturating_sub(1);
        }
    }
}

/// A program the node started, its pid kept from the sweep until it is reaped or let go.
struct Owned {
    proc: Box<dyn Proc>,
    pid: u32,
    released: bool,
}

impl Owned {
    fn release(&mut self) {
        if !self.released
            && let Ok(mut owned) = OWNED.lock()
        {
            owned.remove(&self.pid);
            self.released = true;
        }
    }

    fn seen(&mut self, exit: Option<Exit>) -> Option<Exit> {
        if exit.is_some() {
            self.release();
        }
        exit
    }
}

impl Proc for Owned {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        let exit = self.proc.try_wait()?;
        Ok(self.seen(exit))
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let exit = self.proc.wait_for(timeout)?;
        Ok(self.seen(exit))
    }

    fn kill(&mut self) -> Result<(), Refusal> {
        self.proc.kill()
    }

    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        self.proc.streams()
    }
}

impl Drop for Owned {
    // Let go unreaped, an ended program is the sweep's to reap.
    fn drop(&mut self) {
        self.release();
    }
}

/// A [`Proc`] for an exec'd program the agent watches for its box's delete. It reaps the program itself
/// when the caller waits, unless a delete reaped it first — then it reads the exit the agent recorded.
/// Dropped unreaped, it lets the program go: the sweep reaps it if it ends first, else the box's delete does.
struct ProgramProc {
    supervisor: Arc<Supervisor>,
    id: BoxId,
    proc: Box<dyn Proc>,
    pid: u32,
    exit: Arc<Mutex<Option<Exit>>>,
    done: bool,
}

impl ProgramProc {
    /// The exit a delete recorded, if the agent reaped the program.
    fn recorded(&self) -> Option<Exit> {
        self.exit.lock().ok().and_then(|slot| *slot)
    }

    /// Stops watching and leaves [`OWNED`], once the program is reaped or let go.
    fn done(&mut self, exit: Exit) -> Exit {
        if !self.done {
            self.done = true;
            self.supervisor.forget_program(&self.id, self.pid);
            if let Ok(mut owned) = OWNED.lock() {
                owned.remove(&self.pid);
            }
        }
        exit
    }
}

impl Proc for ProgramProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        if let Some(exit) = self.recorded() {
            return Ok(Some(self.done(exit)));
        }
        match self.proc.try_wait() {
            Ok(Some(exit)) => Ok(Some(self.done(exit))),
            Ok(None) => Ok(None),
            // A delete may have reaped it, so the caller's own wait can't; read what the agent recorded.
            Err(e) => match self.recorded() {
                Some(exit) => Ok(Some(self.done(exit))),
                None => Err(e),
            },
        }
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        if let Some(exit) = self.recorded() {
            return Ok(Some(self.done(exit)));
        }
        match self.proc.wait_for(timeout) {
            Ok(Some(exit)) => Ok(Some(self.done(exit))),
            Ok(None) => match self.recorded() {
                Some(exit) => Ok(Some(self.done(exit))),
                None => Ok(None),
            },
            Err(e) => match self.recorded() {
                Some(exit) => Ok(Some(self.done(exit))),
                None => Err(e),
            },
        }
    }

    /// Ended already when a delete reaped it; else killed through its own `Proc`.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.done || self.recorded().is_some() {
            return Ok(());
        }
        self.proc.kill()
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        self.proc.streams()
    }
}

impl Drop for ProgramProc {
    fn drop(&mut self) {
        if !self.done {
            // Let go unreaped: the box's delete reaps it, its init's end waiting for that; the sweep may first, the
            // program out of the owned set.
            self.supervisor.let_go(&self.id, self.pid);
            if let Ok(mut owned) = OWNED.lock() {
                owned.remove(&self.pid);
            }
        }
    }
}

/// Whether a program let go has ended and been reaped, by this look or by the sweep before it.
fn ended_and_reaped(pidfd: BorrowedFd<'_>) -> bool {
    matches!(
        rustix::process::waitid(
            WaitId::PidFd(pidfd),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
        ),
        Ok(Some(_)) | Err(_)
    )
}

/// Reaps `pidfd`'s process if it has ended and records how in `slot`, for the caller's [`Proc`]. The
/// slot is held across both, so a caller whose own wait meets the process reaped, and then reads the
/// slot, finds its exit: never the moment between the reap and its record.
fn reap_into(pidfd: BorrowedFd<'_>, slot: &Mutex<Option<Exit>>) -> Result<(), Refusal> {
    let mut slot = slot.lock().map_err(poisoned)?;
    match rustix::process::waitid(
        WaitId::PidFd(pidfd),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
    ) {
        Ok(Some(status)) => *slot = Some(exit_of(&status)),
        Ok(None) | Err(Errno::CHILD) => {}
        Err(e) => return Err(Refusal::os("reaping a deleted box's program", e)),
    }
    Ok(())
}

/// How a reaped process ended.
fn exit_of(status: &WaitIdStatus) -> Exit {
    match (status.exit_status(), status.terminating_signal()) {
        (_, Some(sig)) => Exit::Signal(sig),
        (Some(code), None) => Exit::Code(code),
        (None, None) => Exit::Unknown,
    }
}

/// A deleted box's init, unwatched but still owned until reaped, for the delete to end and reap.
pub struct Forgotten {
    pid: i32,
    /// `None` once the supervisor reaped it.
    pidfd: Option<OwnedFd>,
    /// Whether ending it ends a pid namespace of its own ([`Container::pid_namespace`]).
    pub pid_namespace: bool,
}

impl Forgotten {
    /// Its pid, in the node agent's pid namespace.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Sends the init SIGKILL through its pidfd, which ends its pid namespace with it: the kernel kills
    /// every process in the namespace when its init ends (pid_namespaces(7); Linux v6.19
    /// `zap_pid_ns_processes`, kernel/pid_namespace.c:231), and a SIGKILL from an ancestor namespace
    /// reaches an init without a handler (kernel/signal.c:1190-1191). The signal takes no `cgroup_mutex`,
    /// where `cgroup.kill`'s write does (research f45, f46, f50). An init that has ended already, or been
    /// reaped, has nothing left to end.
    pub fn kill(&self) -> Result<(), Refusal> {
        let Some(pidfd) = &self.pidfd else {
            return Ok(());
        };
        match rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(e) => Err(Refusal::os(
                "killing a deleted box's init through its pidfd",
                e,
            )),
        }
    }

    /// Waits until the init has ended, at most `patience`: its pidfd turns readable then
    /// (pidfd_open(2)). Its pid namespace's processes are all gone by then, since its end waits for them
    /// (kernel/pid_namespace.c:263-265). One reaped already has ended.
    pub fn ended_within(&self, patience: Duration) -> Result<bool, Refusal> {
        let Some(pidfd) = &self.pidfd else {
            return Ok(true);
        };
        let ts = Timespec {
            tv_sec: i64::try_from(patience.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(patience.subsec_nanos()),
        };
        let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
        loop {
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(n) => return Ok(n > 0),
                Err(Errno::INTR) => continue,
                Err(e) => return Err(Refusal::os("waiting on a deleted box's init", e)),
            }
        }
    }
}

/// Whether the process `pidfd` names has ended within `timeout` (`Duration::MAX`: until it does).
pub fn pidfd_ends_within(pidfd: BorrowedFd<'_>, timeout: Duration) -> Result<bool, Refusal> {
    let ts = Timespec {
        tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
    loop {
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(n) => return Ok(n > 0),
            Err(Errno::INTR) => {}
            Err(e) => return Err(Refusal::os("waiting on a deleted box's program", e)),
        }
    }
}

/// Reaps a deleted box's init, which has ended since its box is empty, and disowns it. One the
/// supervisor reaped already was disowned then.
pub fn reap(init: &Forgotten) -> Result<(), Refusal> {
    let Some(pidfd) = &init.pidfd else {
        return Ok(());
    };
    let reaped = match rustix::process::waitid(
        WaitId::PidFd(pidfd.as_fd()),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
    ) {
        Ok(_) | Err(Errno::CHILD) => Ok(()),
        Err(e) => Err(Refusal::os("reaping a deleted box's init", e)),
    };
    disown(init.pid)?;
    reaped
}

/// Leaves `pid` to the sweep.
fn disown(pid: i32) -> Result<(), Refusal> {
    if let Ok(pid) = u32::try_from(pid) {
        owned()?.remove(&pid);
    }
    Ok(())
}

/// The agent's children, from each of its threads' `children` file (proc_pid_task(5)), at most
/// [`SWEEP_CHILDREN`] of them.
fn children() -> Result<Vec<i32>, Refusal> {
    let tasks = std::fs::read_dir("/proc/self/task")
        .map_err(|e| Refusal::os("listing the node agent's threads", e))?;
    let mut out = Vec::new();
    for task in tasks.take(SWEEP_TASKS) {
        let task = task.map_err(|e| Refusal::os("listing the node agent's threads", e))?;
        let path: PathBuf = task.path().join("children");
        // A thread that has ended since the listing has no file; it has no children either.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for pid in text.split_whitespace().filter_map(|p| p.parse().ok()) {
            if out.len() >= SWEEP_CHILDREN {
                return Ok(out);
            }
            out.push(pid);
        }
    }
    Ok(out)
}

/// The fields of `/proc/<pid>/stat` the supervisor reads (proc_pid_stat(5)).
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    /// State `Z`: ended, waiting to be reaped.
    zombie: bool,
    /// Field 4.
    ppid: i32,
    /// Field 22, clock ticks after boot.
    start_ticks: u64,
}

impl Stat {
    fn of(pid: i32) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// The fields after the command name, which is parenthesized and may itself hold spaces or
    /// parentheses: the state (field 3), the parent (4), then eighteen on, the start time (22).
    fn parse(stat: &str) -> Option<Self> {
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        let ppid = fields.next()?.parse().ok()?;
        let start_ticks = fields.nth(17)?.parse().ok()?;
        Some(Self {
            zombie: state == "Z",
            ppid,
            start_ticks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> hex_core::record::PoolRecord {
        let fixed = hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: hex_api::MIN_MEMORY,
            disk_bytes: 0,
        };
        let cost = hex_api::RuntimeCost {
            id: [3; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources::default(),
        };
        hex_core::record::PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: hex_api::BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            execution: hex_api::Execution::ShardsShared,
            cost,
            allocation: Some(
                cost.allocate(hex_api::CapacityResources::default())
                    .unwrap(),
            ),
            devices: Vec::new(),
            phase: hex_api::BackingPhase::Granted,
            init: None,
            exit: None,
        }
    }

    fn pool_receipt(record: &hex_core::record::PoolRecord) -> triomphe::Arc<ManagedVm> {
        triomphe::Arc::try_new(ManagedVm {
            owner: OwnerKey::Pool(record.key.pool),
            scope: record.scope(),
            commitment: record.native_commitment().unwrap(),
            cgroup: hex_runtime::linux::cgroup::BoxCgroup::at(PathBuf::new()),
            events: None,
            identity: OnceLock::new(),
            pending_path: Mutex::new(None),
            phase: Mutex::new(ManagedPhase::Absent),
            isolation_ack: OnceLock::new(),
            supervisor: Weak::new(),
            token: 3,
        })
        .ok()
        .unwrap()
    }

    /// AC-T7.4.1: one owned notification on the same receipt, armed before spawn. No global FD,
    /// late/duplicate preparation or second writer survives consumption; errors remain typed.
    #[test]
    fn per_launch_isolation_notification_is_single_owned_bounded_and_phase_checked() {
        use std::io::Read as _;
        let record = pool();
        let native = pool_receipt(&record);
        assert!(native.prepare_isolation_ack().is_err());
        assert!(native.isolation_ack.get().is_none());
        *native.phase.lock().unwrap() = ManagedPhase::Reserved;
        let mut reader = native.prepare_isolation_ack().unwrap();
        let same = triomphe::Arc::clone(&native);
        assert!(same.prepare_isolation_ack().is_err());
        let writer = native.take_isolation_ack().unwrap().unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&writer)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        );
        assert!(same.take_isolation_ack().unwrap().is_none());
        assert!(same.prepare_isolation_ack().is_err());
        write_isolation_ack(writer).unwrap();
        let mut byte = [0u8];
        assert_eq!(reader.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [2]);
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
        // A failed notification cannot become success or leak a fresh writer.
        let (read_only, actual_writer) = std::io::pipe().unwrap();
        drop(actual_writer);
        let wrong = std::io::PipeWriter::from(OwnedFd::from(read_only));
        assert!(matches!(
            write_isolation_ack(wrong),
            Err(Refusal::Os {
                op: "notifying per-launch descriptor isolation",
                ..
            })
        ));
        let late = pool_receipt(&record);
        *late.phase.lock().unwrap() = ManagedPhase::Exited(Exit::Code(0));
        assert!(late.prepare_isolation_ack().is_err());
        assert!(late.isolation_ack.get().is_none());
    }

    /// AC-T7.4.1: a real Pool owner consumes the SAME init budget and token namespace as a Box.
    /// This is a pure receipt-table control: no Supervisor/thread/process/cgroup is opened.
    #[test]
    fn managed_pool_and_box_share_one_bounded_preallocated_init_table() {
        let mut inits = Inits {
            budget: 2,
            ..Inits::default()
        };
        inits.reserve_place().unwrap();
        inits.reserve_place().unwrap();
        assert!(matches!(
            inits.reserve_place(),
            Err(Refusal::Exhausted {
                what: PIDFDS,
                bound: 2
            })
        ));
        let capacities = (inits.rows.capacity(), inits.tokens.capacity());
        let id = BoxId::new("pool-shaped-box-id").unwrap();
        inits
            .insert(
                OwnerKey::Box(id.try_clone().unwrap()),
                OwnerKey::Box(id.try_clone().unwrap()),
                9,
                Init::Container(ContainerInit {
                    pid: 7,
                    who: ProcId {
                        pid: 7,
                        start_ticks: 11,
                    },
                    pidfd: None,
                    exit: None,
                    container: Arc::new(Container {
                        bundle: String::new(),
                        cgroup: None,
                        pid_namespace: false,
                    }),
                }),
            )
            .unwrap();
        inits.reserved -= 1;
        let record = pool();
        let native = pool_receipt(&record);
        inits
            .insert(
                OwnerKey::Pool(record.key.pool),
                OwnerKey::Pool(record.key.pool),
                3,
                Init::Managed(triomphe::Arc::clone(&native)),
            )
            .unwrap();
        inits.reserved -= 1;
        assert_eq!((inits.rows.capacity(), inits.tokens.capacity()), capacities);
        assert_eq!(inits.token_owner(9).unwrap().as_ref(), OwnerRef::Box(&id));
        assert_eq!(
            inits.token_owner(3).unwrap().as_ref(),
            OwnerRef::Pool(&record.key.pool)
        );
        assert!(matches!(
            inits.get(OwnerRef::Box(&id)),
            Some(Init::Container(_))
        ));
        assert!(matches!(
            inits.get(OwnerRef::Pool(&record.key.pool)),
            Some(Init::Managed(known)) if triomphe::Arc::ptr_eq(known, &native)
        ));
        assert!(inits.reserve_place().is_err());
        assert!(inits.remove(OwnerRef::Pool(&record.key.pool)).is_some());
        assert!(inits.token_owner(3).is_none());
        assert!(inits.get(OwnerRef::Box(&id)).is_some());
        inits.reserve_place().unwrap();
    }

    /// AC-T7.4.1: a same-pool retry may not replace the retained scope or admitted charge.
    /// Record/commitment checks precede all native observation and effects.
    #[test]
    fn managed_pool_receipt_rejects_another_generation_or_admitted_charge() {
        let mut record = pool();
        let native = pool_receipt(&record);
        native.matches_record(NativeRecord::Pool(&record)).unwrap();
        record.key.generation = [4; 16];
        assert!(matches!(
            native.matches_record(NativeRecord::Pool(&record)),
            Err(Refusal::Backing { key, cause: hex_api::LifecycleCause::Uncertain })
                if key == record.key
        ));
        record.key.generation = [2; 16];
        record.cost.id = [5; 32];
        record.allocation = Some(
            record
                .cost
                .allocate(hex_api::CapacityResources::default())
                .unwrap(),
        );
        assert!(native.matches_record(NativeRecord::Pool(&record)).is_err());
        record.cost.id = [3; 32];
        record.allocation = Some(
            record
                .cost
                .allocate(hex_api::CapacityResources::default())
                .unwrap(),
        );
        native.matches_record(NativeRecord::Pool(&record)).unwrap();
        assert!(matches!(native.uncertain(), Refusal::Backing { key, .. } if key == record.key));
    }

    /// Decision 3 (HEX-RT-7): a deleted box's programs are reaped by its delete, each exit recorded for
    /// its caller's wait, and a caller whose own wait meets the program already reaped reads the record.
    /// So the record is there whenever its slot is free and the program is reaped: a caller that reads
    /// it in the moment between the reap and the record finds nothing, and over the socket the program
    /// was reported to have ended `Exit::Unknown` (fedora's gates under load, 2026-10-29). Each round a
    /// child ends and is reaped into its slot on another thread, while this one holds the slot and looks
    /// whether the child is still there, until it has been reaped.
    #[test]
    fn a_deleted_boxs_program_is_never_reaped_with_its_exit_unrecorded() {
        let mut caught = 0usize;
        for _ in 0..200 {
            #[expect(
                clippy::zombie_processes,
                reason = "reaped through its pidfd by reap_into, the reap this test watches"
            )]
            let child = std::process::Command::new("true").spawn().unwrap();
            let pidfd =
                rustix::process::pidfd_open(Pid::from_child(&child), PidfdFlags::empty()).unwrap();
            let mut fds = [PollFd::new(&pidfd, PollFlags::IN)];
            rustix::event::poll(&mut fds, None).unwrap();
            let look = pidfd.try_clone().unwrap();
            let slot = Arc::new(Mutex::new(None));
            let reaper = {
                let slot = Arc::clone(&slot);
                std::thread::spawn(move || reap_into(pidfd.as_fd(), &slot))
            };
            loop {
                let recorded = slot.lock().unwrap();
                let gone = matches!(
                    rustix::process::waitid(
                        WaitId::PidFd(look.as_fd()),
                        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                    ),
                    Err(Errno::CHILD)
                );
                if gone {
                    if recorded.is_none() {
                        caught += 1;
                    }
                    break;
                }
                drop(recorded);
                std::thread::yield_now();
            }
            reaper.join().unwrap().unwrap();
            assert_eq!(
                *slot.lock().unwrap(),
                Some(Exit::Code(0)),
                "the child's exit is recorded"
            );
        }
        assert_eq!(
            caught, 0,
            "{caught} of 200 children were reaped with their exit unrecorded"
        );
    }

    /// The waits census's suites, every loop of run 5 (waits-census-20261109T184054Z): `oci_box`'s delete of a box whose
    /// exec'd program its caller had let go ran out of the emptying wait, the box's init "still runs 326.645366ms after
    /// SIGKILL". The init is its pid namespace's first process, which doesn't finish exiting until every process in the
    /// namespace is reaped (kernel/pid_namespace.c, `zap_pid_ns_processes`); the program it killed is the agent's child,
    /// and a program let go was forgotten by the box, left to the sweep, which reaps once in [`SWEEP_PERIOD`]. Do: own a
    /// child as box `b`'s exec'd program, let its `Proc` go, kill the child, and reap box `b`'s programs as its delete
    /// does. Expect: the child reaped by that, none of it left for the sweep.
    #[test]
    fn a_program_let_go_is_reaped_by_its_boxs_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        // A box root no process of this test is in, so the sweep reaps none of them.
        let supervisor = Supervisor::start(
            Path::new(hex_runtime::linux::cgroup::CGROUP_ROOT)
                .join("hex-supervise-test-none")
                .as_path(),
            store,
            1,
        )
        .unwrap();
        #[expect(
            clippy::zombie_processes,
            reason = "reaped by the box's delete, through the agent's pidfd: the reap this test looks for"
        )]
        let child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();
        let pid = Pid::from_child(&child);
        let id = BoxId::new("b").unwrap();
        let proc = hex_runtime::linux::proc::PidfdProc::of_child(child.id()).unwrap();
        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty()).unwrap();
        let program = supervisor.own_program(&id, Box::new(proc), pidfd).unwrap();
        drop(program);
        let look = rustix::process::pidfd_open(pid, PidfdFlags::empty()).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
        supervisor
            .reap_programs(&id, &mut |pidfd| {
                pidfd_ends_within(pidfd, hex_testkit::waits::EMPTYING)
            })
            .unwrap();
        assert_eq!(
            rustix::process::waitid(
                WaitId::PidFd(look.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map(|status| status.is_some()),
            Err(Errno::CHILD),
            "the program let go is still the agent's child after its box's delete reaped the box's programs"
        );
    }

    /// The lead's ruling of 2026-11-11 (AC-T7.1.1; original): the exec'd programs' pidfds the supervisor keeps to reap
    /// are bounded for the node, at its budget, not only for each box, so the node's own count of them holds whoever
    /// execs: its socket's clients or a process it is embedded in. Do: start a supervisor whose budget is two; keep an
    /// exec'd program of each of three boxes; reap the first's; keep the third's again. Expect: the first two kept; the
    /// third refused by type, naming the budget; kept once the first's is reaped.
    #[test]
    fn the_exec_d_programs_kept_are_bounded_for_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        // A box root no process of this test is in, so the sweep reaps none of them.
        let supervisor = Supervisor::start(
            Path::new(hex_runtime::linux::cgroup::CGROUP_ROOT)
                .join("hex-supervise-test-none")
                .as_path(),
            store,
            2,
        )
        .unwrap();
        let keep = |id: &str| {
            #[expect(
                clippy::zombie_processes,
                reason = "reaped through its Proc or killed and reaped by this test's end"
            )]
            let child = std::process::Command::new("sleep")
                .arg("600")
                .spawn()
                .unwrap();
            let pid = Pid::from_child(&child);
            let proc = hex_runtime::linux::proc::PidfdProc::of_child(child.id()).unwrap();
            let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty()).unwrap();
            (
                pid,
                supervisor.own_program(&BoxId::new(id).unwrap(), Box::new(proc), pidfd),
            )
        };
        let (first, kept) = keep("a");
        let mut first_proc = kept.unwrap();
        let (second, kept) = keep("b");
        let second_proc = kept.unwrap();
        let (third, refused) = keep("c");
        assert!(
            matches!(&refused, Err(Refusal::Exhausted { bound: 2, .. })),
            "the third box's program past the node's budget of two: {:?}",
            refused.as_ref().err()
        );
        rustix::process::kill_process(third, rustix::process::Signal::KILL).unwrap();
        rustix::process::waitpid(Some(third), rustix::process::WaitOptions::empty()).unwrap();
        rustix::process::kill_process(first, rustix::process::Signal::KILL).unwrap();
        assert_eq!(
            first_proc.wait_for(hex_testkit::waits::EMPTYING).unwrap(),
            Some(Exit::Signal(9))
        );
        let (again, kept) = keep("c");
        let again_proc = kept.expect("kept once the first's is reaped");
        for (pid, mut proc) in [(second, second_proc), (again, again_proc)] {
            rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
            assert_eq!(
                proc.wait_for(hex_testkit::waits::EMPTYING).unwrap(),
                Some(Exit::Signal(9))
            );
        }
    }

    /// Decision 2: the supervisor reads a process's state, parent and start time from its stat line,
    /// a command name with spaces and parentheses included; a truncated or malformed line is no stat.
    #[test]
    fn a_stat_line_gives_state_parent_and_start_time() {
        let line = "4242 (a (b) c) Z 17 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(
            Stat::parse(line),
            Some(Stat {
                zombie: true,
                ppid: 17,
                start_ticks: 987_654
            })
        );
        let running = line.replace(") Z ", ") S ");
        assert!(!Stat::parse(&running).unwrap().zombie);
        for bad in [
            "",
            "4242 (x",
            "4242 (x) S",
            "4242 (x) S notapid 1 1",
            "4242 (x) S 1 2 3",
        ] {
            assert_eq!(Stat::parse(bad), None, "{bad:?}");
        }
    }

    /// This process's own stat line parses, its parent is its parent, and its start time is the one
    /// the oci state reads (the same field 22 create records).
    #[test]
    fn this_processs_stat_agrees_with_the_oci_start_time() {
        let me = i32::try_from(std::process::id()).unwrap();
        let stat = Stat::of(me).unwrap();
        assert!(!stat.zombie);
        assert_eq!(
            Some(stat.start_ticks),
            hex_runtime::linux::oci::state::start_time(me)
        );
        assert_eq!(
            stat.ppid,
            rustix::process::getppid().unwrap().as_raw_nonzero().get()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/completion_native_gate_tests.rs"]
mod completion_native_gate_tests;
