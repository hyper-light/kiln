//! The `process` class on Windows (docs/design/runtime.md, decision "each OS runs its own boxes
//! natively"): a box is a job object. Its limits are the kernel's and hard — a commit cap, an
//! active-process cap, a CPU weight or hard cap, kill-on-close, and no breakaway — so the box holds its
//! bounds without a watcher enforcing them (`sys`, [`hex_core::job`]). A member is created suspended
//! inside the job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`) and resumed, so nothing runs outside it.
//!
//! Reaching the memory bound is read from the job's own accounting: its peak commit at or past its
//! commit limit ([`hex_core::job::reached_bound`]). The job's memory notification, the class Windows
//! calls guaranteed, is set late under load and never once the member's commit has fallen back, so it
//! can't tell a box that ended at its bound (bench/install/records/windows-jupiter-20261105.txt entry
//! 21). A fork refused at the active-process limit is a
//! hard, kernel-enforced refusal delivered synchronously to the forking process. Windows has no §3-clean
//! count of such refusals for the job's owner: its accounting doesn't move on a refusal, and the only
//! job-side signal is the completion-port active-process-limit message, which Microsoft documents as not
//! guaranteed to be delivered — a lossy message the step forbids as a source (CLAUDE.md §3,
//! `docs/research/runtime/windows-pids-refused.md`, which surveys how others handle this). So Hex counts
//! only the refusals it observes synchronously and reliably: the node's own `exec` into a full box,
//! whose `CreateProcess` returns `ERROR_NOT_ENOUGH_QUOTA` ([`JobBox::exec`]). A member's own internal
//! fork, refused by the kernel in that member, is enforced but not counted by the box (no §3-clean
//! source exists). HEX-RT-6 on Windows therefore checks the bound's deterministic effect — the member
//! that can't start a child it needs exits non-zero — rather than the exact count, through
//! `hex_api::conformance`'s `refused_forks_counted` capability (Ada's decision, 2026-11-03); the
//! exact-count assertion stays where a kernel keeps it (Linux's `pids.events`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use triomphe::Arc;

use hex_api::{Bounds, BoxId, Class, Refusal, Signal, Usage};
use hex_core::job::{self, Sent};
use hex_core::patience::Piece;
use hex_core::record::{NativeRecord, OwnerKey, OwnerRef};

use super::sys::{self, Job, Spawn, Started};

/// What the box counts beyond what the job's accounting gives: how many forks it saw refused at its
/// process limit, and which signals Hex has ended the job for.
#[derive(Debug, Default)]
struct Counts {
    /// Forks the box saw refused at its active-process limit — the node's own `exec` into a full box,
    /// observed synchronously ([`JobBox::exec`]). A member's internal fork, refused in that member, has
    /// no §3-clean job-side signal and is not counted here (`Usage::pids_refused`).
    pids_refused: u64,
    /// Signals Hex ended the box's job for, so [`super::proc`] reads a terminated member's exit code as
    /// the signal.
    sent: Sent,
    deletion: Deletion,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Deletion {
    #[default]
    Live,
    Requested,
    Released,
}

/// One box's shared state: its job and its counts. Held behind an `Arc` by the runtime and by each
/// member's [`super::proc::BoxProc`] (which reads `sent` to map an exit).
pub struct State {
    id: OwnerKey,
    bounds: Bounds,
    /// Introduced exactly once, after both fallible native-owner allocations succeed.
    job: OnceLock<NativeScope>,
    /// The job's limits, against which its accounting says whether the box reached its memory bound.
    limits: job::Limits,
    counts: Mutex<Counts>,
    /// Whether the box is being deleted; a member started after this is refused (the job is ending).
    stopping: AtomicBool,
    /// Whether the box outlives its node: a named job whose other handle a keeper holds
    /// (`super::keeper`), so dropping the box leaves it running for the next node to take back
    /// ([`JobBox::take_back`]).
    outlives: bool,
}

enum NativeScope {
    /// No controller was submitted and every empty job opened by this attempt has been dropped.
    NotSubmitted,
    Bound(Job),
    Adopted(Job),
}

impl State {
    fn counts(&self) -> Result<std::sync::MutexGuard<'_, Counts>, Refusal> {
        self.counts
            .lock()
            .map_err(|_| super::pending("a box's counts lock is poisoned"))
    }

    /// The signals Hex has ended the box's job for.
    pub fn sent(&self) -> Result<Sent, Refusal> {
        Ok(self.counts()?.sent)
    }
}

/// A Windows `process` box. Dropping one made in process ([`JobBox::create`]) closes the job's last
/// handle, which (kill-on-close) ends every process still in it; one a node made by name
/// ([`JobBox::create_named`]) outlives the node, and dropping it leaves it running.
pub struct JobBox {
    state: Arc<State>,
    purpose: Purpose,
    /// The original box's optional VM controller receipt, introduced once before it resumes.
    /// A managed API Arc is a view of this existing JobBox allocation, never another process row.
    pub(super) managed: OnceLock<super::managed::Receipt>,
    pub(super) observation_failure: OnceLock<shards_api::ProcessFailure>,
}

enum Purpose {
    Process,
    Vm {
        scope: hex_core::record::NativeScope,
        commitment: [u8; 32],
        original_init: Option<hex_core::record::ProcId>,
        owner: std::sync::Weak<super::Holding>,
        released: OnceLock<()>,
    },
}

impl JobBox {
    /// A box with `bounds` on a machine of `processors` logical processors (the CPU hard cap's
    /// denominator). The job is made and limited while empty; nothing runs in it until
    /// [`JobBox::exec`].
    pub fn create(
        id: BoxId,
        bounds: &Bounds,
        processors: std::num::NonZeroU32,
    ) -> Result<Self, Refusal> {
        let job = Job::create().map_err(|e| Refusal::os("creating a box's job", e))?;
        Self::held(id, bounds, processors, job, false)
    }

    /// A box that outlives its node: its job named `name` (a node's own name for the box), held by the node
    /// and by its keeper, so a node killed with it running leaves it running for the next node to take
    /// back; with both gone, kill-on-close ends it.
    /// A job of that name held already is refused: that box is a node's before, to be taken back.
    pub fn create_named(
        id: BoxId,
        bounds: &Bounds,
        processors: std::num::NonZeroU32,
        name: &str,
    ) -> Result<Self, Refusal> {
        let job = Job::create_named(name).map_err(|e| Refusal::os("creating a box's job", e))?;
        Self::held(id, bounds, processors, job, true)
    }

    /// Takes back the box a node before this one made as `name`, with whatever runs in it, its limits set
    /// again from `bounds`; where no job holds the name (its members all ended, or the machine restarted),
    /// the box is made again, empty.
    pub fn take_back(
        id: BoxId,
        bounds: &Bounds,
        processors: std::num::NonZeroU32,
        name: &str,
    ) -> Result<Self, Refusal> {
        match Job::open(name).map_err(|e| Refusal::os("opening a box's job", e))? {
            Some(job) => Self::held(id, bounds, processors, job, true),
            None => Self::create_named(id, bounds, processors, name),
        }
    }

    /// Ends what runs in the box a node before this one made as `name`, for a box recovery removes, and waits for its
    /// job to empty as decision 9 waits ([`follow_emptying`]): TerminateJobObject only begins its members' ends. One no
    /// job holds has nothing left to end.
    pub fn end_named(name: &str) -> Result<(), Refusal> {
        if let Some(job) = Job::open(name).map_err(|e| Refusal::os("opening a box's job", e))? {
            let killed = Instant::now();
            job.terminate(job::terminated_code(Signal::KILL))
                .map_err(|e| Refusal::os("ending a box's job", e))?;
            follow_emptying(
                &job,
                killed,
                &[
                    Piece::Kind("the killed job "),
                    Piece::Name(name),
                    Piece::Kind(" of a box recovery removes, by its members' process handles"),
                ],
                name,
            )?;
        }
        Ok(())
    }

    /// A box over `job`, its limits set from `bounds` while it holds nothing new; `outlives` says dropping
    /// it leaves it to the keeper's handle.
    fn held(
        id: BoxId,
        bounds: &Bounds,
        processors: std::num::NonZeroU32,
        job: Job,
        outlives: bool,
    ) -> Result<Self, Refusal> {
        let limits = job::limits(bounds, processors)?;
        job.configure(&sys_limits(&limits))
            .map_err(|e| Refusal::os("setting a box's job limits", e))?;
        Ok(Self {
            purpose: Purpose::Process,
            managed: OnceLock::new(),
            observation_failure: OnceLock::new(),
            state: Arc::try_new(State {
                id: OwnerKey::Box(id),
                bounds: bounds.clone(),
                job: OnceLock::from(NativeScope::Bound(job)),
                limits,
                counts: Mutex::new(Counts::default()),
                stopping: AtomicBool::new(false),
                outlives,
            })
            .map_err(|_| super::allocation(std::mem::size_of::<State>()))?,
        })
    }

    pub fn id(&self) -> Result<&BoxId, Refusal> {
        match &self.state.id {
            OwnerKey::Box(id) => Ok(id),
            OwnerKey::Pool(_) => Err(self.uncertain("a pool's job box has no box id")),
        }
    }

    pub(super) fn owner(&self) -> OwnerRef<'_> {
        self.state.id.as_ref()
    }

    pub fn bounds(&self) -> Bounds {
        self.state.bounds.clone()
    }

    /// The shared state, for a member's waiter to read which signals the box was ended for.
    pub fn shared(&self) -> Arc<State> {
        Arc::clone(&self.state)
    }

    /// Allocate the same native slot/API owner and its existing shared counts before creating a
    /// kernel job. OnceLock introduces the native handle without unsafe uninitialized storage.
    pub(super) fn prepare_vm(
        record: NativeRecord<'_>,
        bounds: Bounds,
        processors: std::num::NonZeroU32,
        name: Option<&str>,
        owner: &std::sync::Arc<super::Holding>,
    ) -> Result<Arc<Self>, Refusal> {
        let scope = record
            .scope()
            .ok_or_else(|| super::pending("native scope is absent"))?;
        let commitment = record.commitment()?;
        let id = record.owner().try_owned()?;
        let allocation = record
            .allocation()
            .ok_or_else(|| super::pending("VM allocation is absent"))?;
        allocation.guest()?;
        // Its bounds are exactly what its allocation gives a VM's host (`managed::host_bounds`): its CPU, and no commit
        // limit since ruling B (A-4), which left this check asking for its guest's RAM and refusing every VM.
        if record.class()? != Class::Shards || bounds != super::managed::host_bounds(record)? {
            return Err(super::pending(
                "VM native bounds differ from its allocation",
            ));
        }
        let limits = job::limits(&bounds, processors)?;
        let state = Arc::try_new(State {
            id,
            bounds,
            job: OnceLock::new(),
            limits,
            counts: Mutex::new(Counts::default()),
            stopping: AtomicBool::new(false),
            outlives: name.is_some(),
        })
        .map_err(|_| super::allocation(std::mem::size_of::<State>()))?;
        let boxed = Arc::try_new(Self {
            state,
            purpose: Purpose::Vm {
                scope,
                commitment,
                original_init: record.init(),
                owner: std::sync::Arc::downgrade(owner),
                released: OnceLock::new(),
            },
            managed: OnceLock::new(),
            observation_failure: OnceLock::new(),
        })
        .map_err(|_| super::allocation(std::mem::size_of::<Self>()))?;
        Ok(boxed)
    }

    /// The same slot already owns this receipt. A failed empty-job preparation is explicit positive
    /// non-submission; it is never inferred from an absent process or textual job name.
    pub(super) fn bind_vm(&self, name: Option<&str>) -> Result<(), Refusal> {
        if !self.is_vm() || self.state.job.get().is_some() {
            return Err(self.uncertain("it isn't a VM's job box, or its job was bound before"));
        }
        let result = (|| {
            let job = match name {
                Some(name) => Job::create_named(name),
                None => Job::create(),
            }
            .map_err(|error| Refusal::os("creating the VM's native job", error))?;
            job.configure(&sys_limits(&self.state.limits))
                .map_err(|error| Refusal::os("setting the VM's native job limits", error))?;
            Ok(job)
        })();
        match result {
            Ok(job) => self
                .state
                .job
                .set(NativeScope::Bound(job))
                .map_err(|_| self.uncertain("its job was bound before")),
            Err(error) => {
                self.state
                    .job
                    .set(NativeScope::NotSubmitted)
                    .map_err(|_| self.uncertain("its job was marked unsubmitted before"))?;
                Err(error)
            }
        }
    }

    pub(super) fn not_submitted(&self) -> Result<(), Refusal> {
        if !self.is_vm() {
            return Err(self.uncertain("it isn't a VM's job box as its submission is refused"));
        }
        self.state
            .job
            .set(NativeScope::NotSubmitted)
            .map_err(|_| self.uncertain("its job was bound before its submission was refused"))
    }

    pub(super) fn is_vm(&self) -> bool {
        matches!(&self.purpose, Purpose::Vm { .. })
    }

    pub(super) fn adopt_vm(&self, job: Job) -> Result<(), Refusal> {
        if !self.is_vm() || self.state.job.get().is_some() {
            return Err(self
                .uncertain("it isn't a VM's job box, or its job was bound before it was adopted"));
        }
        // Its bounds as its record gives them now, its CPU rate left as the learner holds it: a commit limit a create
        // before ruling B wrote at its guest's RAM is lifted (joint plan step 14, A-4), as Linux lifts a VM's
        // memory.max at its first window.
        job.set_bounds(&sys_limits(&self.state.limits))
            .map_err(|error| Refusal::os("setting the adopted VM's native job bounds", error))?;
        self.state
            .job
            .set(NativeScope::Adopted(job))
            .map_err(|_| self.uncertain("its job was bound before it was adopted"))
    }

    pub(super) fn recovered(&self) -> bool {
        matches!(self.state.job.get(), Some(NativeScope::Adopted(_)))
    }

    pub(super) fn original_init(&self) -> Option<hex_core::record::ProcId> {
        match self.purpose {
            Purpose::Vm { original_init, .. } => original_init,
            Purpose::Process => None,
        }
    }

    pub(super) fn matches_vm(&self, record: NativeRecord<'_>, expected: [u8; 32]) -> bool {
        // The commitment binds the full original spec, including class and allocation profile.
        // Keep that proof rather than duplicating the complete admission receipt in each owner.
        matches!(&self.purpose, Purpose::Vm { scope, commitment, .. }
            if Some(*scope) == record.scope() && *commitment == expected)
    }

    pub(super) fn vm_commitment(&self) -> Option<(hex_core::record::NativeScope, [u8; 32])> {
        match self.purpose {
            Purpose::Vm {
                scope, commitment, ..
            } => Some((scope, commitment)),
            Purpose::Process => None,
        }
    }

    pub(super) fn released(&self) -> bool {
        matches!(&self.purpose, Purpose::Vm { released, .. } if released.get().is_some())
    }

    pub(super) fn release_scope(&self) -> Result<(), Refusal> {
        let Purpose::Vm { owner, .. } = &self.purpose else {
            return Err(self.uncertain("it isn't a VM's job box as its scope is released"));
        };
        if self.released() {
            return Ok(());
        }
        owner
            .upgrade()
            .ok_or_else(|| self.uncertain("its VM's owner is gone as its scope is released"))?
            .release_vm(self)
    }

    pub(super) fn mark_released(&self) -> Result<(), Refusal> {
        let Purpose::Vm { released, .. } = &self.purpose else {
            return Err(self.uncertain("it isn't a VM's job box as it is marked released"));
        };
        let _ = released.set(());
        Ok(())
    }

    /// Its owner refused Uncertain, and `why` in the refusing site's own words (joint plan steps 14 and 12): a pool's
    /// backing refusal and a box's Lifecycle refusal carry them alike.
    fn uncertain(&self, why: &str) -> Refusal {
        match self.owner() {
            OwnerRef::Box(id) => match id.try_clone() {
                Ok(id) => Refusal::Lifecycle {
                    id,
                    cause: hex_api::LifecycleCause::Uncertain,
                    why: Some(why.to_owned()),
                },
                Err(error) => error,
            },
            OwnerRef::Pool(pool) => match self.vm_commitment() {
                Some((scope, _)) => Refusal::Backing {
                    key: hex_api::BackingKey {
                        pool: *pool,
                        generation: *scope.bytes(),
                    },
                    cause: hex_api::LifecycleCause::Uncertain,
                    why: Some(why.to_owned()),
                },
                None => super::inconsistent_slots(),
            },
        }
    }

    fn native_job(&self) -> Result<&Job, Refusal> {
        match self.state.job.get() {
            Some(NativeScope::Bound(job) | NativeScope::Adopted(job)) => Ok(job),
            _ => Err(self.uncertain("its job isn't bound or adopted")),
        }
    }

    pub(super) fn member(&self) -> Result<sys::Member, super::proc::ObserveError> {
        match self.state.job.get() {
            Some(NativeScope::NotSubmitted) => Ok(sys::Member::Empty),
            Some(NativeScope::Bound(job) | NativeScope::Adopted(job)) => {
                job.member()
                    .map_err(|source| super::proc::ObserveError::Os {
                        operation: super::proc::Operation::QueryScope,
                        source,
                    })
            }
            None => Err(super::proc::ObserveError::Unpublished),
        }
    }

    pub(super) fn create_controller(
        &self,
        spawn: &mut Spawn,
        report: Option<&sys::InheritedHandle>,
    ) -> Result<Started, Refusal> {
        if self.state.stopping.load(Ordering::SeqCst) || !self.is_vm() {
            return Err(self.uncertain(
                "it is stopping, or isn't a VM's job box, as its controller is created",
            ));
        }
        sys::create_with_report(spawn, &[self.native_job()?.raw()], report)
            .map_err(|error| Refusal::os("creating a suspended bound VM controller", error))
    }

    /// Joint plan step 14 (C4 on Windows): the VM job's CPU hard cap becomes `milli` thousandths of a CPU, its
    /// water-filled share, which its learner holds at least at the job's least ([`job::least_milli`]); a rate below
    /// it is refused, typed ([`job::cap`]), never raised quietly.
    pub(super) fn set_cpu_cap(
        &self,
        milli: u64,
        processors: std::num::NonZeroU32,
    ) -> Result<(), Refusal> {
        if !self.is_vm() {
            return Err(self.uncertain("it isn't a VM's job box as its CPU cap is set"));
        }
        let milli = u32::try_from(milli).map_err(|_| Refusal::Invalid {
            what: "a VM's CPU rate",
            why: "it passes a u32 of thousandths of a CPU".to_owned(),
        })?;
        let cap = job::cap(milli, processors)?;
        self.native_job()?
            .set_cpu(job::Cpu::Cap(cap))
            .map_err(|error| Refusal::os("setting a VM job's CPU rate", error))
    }

    /// Stop can run while the sole process observer waits; it never takes that observer's lock.
    pub(super) fn stop_managed(&self) -> Result<(), super::proc::ObserveError> {
        self.state.stopping.store(true, Ordering::SeqCst);
        self.state
            .counts()
            .map_err(super::proc::ObserveError::State)?
            .sent
            .add(Signal::KILL);
        match self.state.job.get() {
            Some(NativeScope::NotSubmitted) => Ok(()),
            Some(NativeScope::Bound(job) | NativeScope::Adopted(job)) => job
                .terminate(job::terminated_code(Signal::KILL))
                .map_err(|source| super::proc::ObserveError::Os {
                    operation: super::proc::Operation::StopScope,
                    source,
                }),
            None => Err(super::proc::ObserveError::Unpublished),
        }
    }

    /// Starts `spawn` suspended inside the box's job and resumes it, returning its process handle and
    /// pid. Nothing runs outside the job: the member is a job member from its creation.
    pub fn exec(&self, mut spawn: Spawn) -> Result<Started, Refusal> {
        if self.state.stopping.load(Ordering::SeqCst) {
            return Err(Refusal::NoSuchBox(self.id()?.try_clone()?));
        }
        let started = match sys::create(&mut spawn, &[self.native_job()?.raw()]) {
            Ok(started) => started,
            // The box is at its active-process limit: the node's own start is refused synchronously
            // (`ERROR_NOT_ENOUGH_QUOTA`). This refusal Hex sees directly, so it is counted exactly
            // (unlike a member's own fork, whose refusal only reaches the box as an async notification);
            // refused by name (docs/research/runtime/windows-pids-refused.md).
            Err(e) if sys::is_process_limit(&e) => return Err(self.refused()?),
            Err(e) => return Err(Refusal::os("starting a program in a box", e)),
        };
        sys::resume(&started.thread).map_err(|e| Refusal::os("resuming a program in a box", e))?;
        Ok(started)
    }

    /// Counts a start refused at the box's process bound, seen synchronously (the node's own `exec`, or
    /// an `oci` box's launcher replying that the job is full), and returns the refusal to give for it.
    pub fn refused(&self) -> Result<Refusal, Refusal> {
        let mut c = self.state.counts()?;
        c.pids_refused = c.pids_refused.saturating_add(1);
        let bound = self
            .state
            .bounds
            .pids_max
            .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
        Ok(Refusal::Exhausted {
            what: "processes",
            bound,
        })
    }

    /// The box's job, for an `oci` box's launcher to start a member in (`super::oci::launch`).
    pub fn job(&self) -> Result<std::os::windows::io::BorrowedHandle<'_>, Refusal> {
        Ok(self.native_job()?.borrowed())
    }

    /// The box's job handle value, for the keeper to duplicate (`super::keeper`); the box keeps the handle.
    pub fn job_token(&self) -> Result<usize, Refusal> {
        Ok(self.native_job()?.raw() as usize)
    }

    /// Whether the box is being deleted, so no member is started in it.
    pub fn stopping(&self) -> bool {
        self.state.stopping.load(Ordering::SeqCst)
    }

    /// Whether a member runs in the box now, from the job's live process-id list (not its accounting,
    /// which lingers while a terminated member's handle is held).
    pub fn running(&self) -> Result<bool, Refusal> {
        Ok(self.live_processes()? > 0)
    }

    fn accounting(&self) -> Result<sys::Accounting, Refusal> {
        self.native_job()?
            .accounting()
            .map_err(|e| Refusal::os("reading a box's job accounting", e))
    }

    fn live_processes(&self) -> Result<u64, Refusal> {
        self.scope_population()
            .map_err(super::proc::ObserveError::refusal)
    }

    pub(super) fn scope_population(&self) -> Result<u64, super::proc::ObserveError> {
        match self.state.job.get() {
            Some(NativeScope::NotSubmitted) => Ok(0),
            Some(NativeScope::Bound(job) | NativeScope::Adopted(job)) => job
                .live_processes()
                .map_err(|source| super::proc::ObserveError::Os {
                    operation: super::proc::Operation::QueryScope,
                    source,
                }),
            None => Err(super::proc::ObserveError::Unpublished),
        }
    }

    /// What the box has used and holds now: the job's accounting and peak, whether it reached its memory
    /// bound (from the peak), and the refused-fork count the box saw.
    pub fn usage(&self) -> Result<Usage, Refusal> {
        let a = self.accounting()?;
        let live = self.live_processes()?;
        // On Windows a commit past the bound is refused, not killed: oom_kills says the box reached its
        // bound, once, from the job's peak (HEX-RT-5). The peak counts a refused commit's ask, so the
        // memory the box reports is the peak at most its bound ([`job::committed_peak`]).
        let oom_kills = u64::from(job::reached_bound(a.peak_memory, &self.state.limits));
        let peak = job::committed_peak(a.peak_memory, &self.state.limits);
        // pids_refused is the count the box saw synchronously (node-mediated exec refusals, [`exec`]).
        let pids_refused = self.state.counts()?.pids_refused;
        Ok(Usage {
            memory_bytes: peak,
            memory_peak_bytes: peak,
            oom_kills,
            pids: live,
            pids_refused,
            // The job keeps CPU in 100 ns ticks; report microseconds.
            cpu_usec: a.cpu_100ns.saturating_div(10),
            io_read_bytes: a.read_bytes,
            io_write_bytes: a.write_bytes,
            guest: None,
            started_from: None,
            paused_ns: None,
        })
    }

    /// Ends every process in the box with `signal` (KILL or TERM, which both end the job). A member's
    /// exit code then reads as `signal` ([`super::proc`]). Refused as not running when no member runs,
    /// as every class refuses it (HEX-DIFF-2).
    pub fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        job::ends_job(signal)?;
        if !self.running()? {
            return Err(Refusal::NotRunning(self.id()?.try_clone()?, "signal"));
        }
        {
            let mut c = self.state.counts()?;
            c.sent.add(signal);
        }
        self.native_job()?
            .terminate(job::terminated_code(signal))
            .map_err(|e| Refusal::os("ending a box's job", e))
    }

    /// Ends the box: marks it stopping, records that it ended the job for KILL (so a member's exit reads
    /// as Signal(9)), requests termination, and waits for the job to empty as decision 9 waits
    /// ([`follow_emptying`]). Acceptance is not scope completion: TerminateJobObject only begins its members'
    /// ends, and the same native job must positively report no members before deletion succeeds. An OS error
    /// retains retry authority; a previously accepted request is observed rather than dispatched again.
    pub fn delete(&self) -> Result<(), Refusal> {
        self.state.stopping.store(true, Ordering::SeqCst);
        let request = {
            let mut c = self.state.counts()?;
            match c.deletion {
                Deletion::Released => return Ok(()),
                Deletion::Requested => false,
                Deletion::Live => {
                    c.sent.add(Signal::KILL);
                    true
                }
            }
        };
        let killed = Instant::now();
        if request {
            self.native_job()?
                .terminate(job::terminated_code(Signal::KILL))
                .map_err(|e| Refusal::os("ending a box's job", e))?;
            let mut c = self.state.counts()?;
            if c.deletion == Deletion::Live {
                c.deletion = Deletion::Requested;
            }
        }
        match self.state.job.get() {
            Some(NativeScope::Bound(job) | NativeScope::Adopted(job)) => {
                let id = match self.owner() {
                    OwnerRef::Box(id) => id.as_str(),
                    OwnerRef::Pool(_) => "",
                };
                follow_emptying(
                    job,
                    killed,
                    &[
                        Piece::Kind("the killed job of box "),
                        Piece::Name(id),
                        Piece::Kind(", by its members' process handles"),
                    ],
                    id,
                )?;
            }
            Some(NativeScope::NotSubmitted) => {}
            None => return Err(super::proc::ObserveError::Unpublished.refusal()),
        }
        self.state.counts()?.deletion = Deletion::Released;
        Ok(())
    }
}

/// Waits for killed job `job` to empty, as decision 9 waits on [`crate::emptying::EMPTYING`] (the review's E-12): within
/// W*, the slowest emptying seen, its emptying; past it, its run-out told once, naming `party`, and the job followed by
/// its members' own ends ([`empties_within`]) however late, never refused at a time, nor said done before it is: it
/// refused at once while TerminateJobObject's members were still ending, and a recovery's end didn't wait at all.
/// `held` names the box to the test hold ([`crate::emptying::hold`]).
fn follow_emptying(
    job: &Job,
    killed: Instant,
    party: &[Piece<'_>],
    held: &str,
) -> Result<(), Refusal> {
    crate::emptying::followed(&crate::emptying::EMPTYING, killed, party, |slice| {
        empties_within(job, slice, held)
    })
}

/// Whether killed job `job` empties within `slice` (`Duration::MAX`: until it does): the first member it lists waited
/// for by its process handle, which the kernel signals at the member's end (WaitForSingleObject, no poll), and the job
/// looked at again, until it lists none (`JobObjectBasicProcessIdList`'s count, which an ended member leaves though its
/// handle is held). The job is terminated again before each member's wait, so a process a member started as the job was
/// terminated, which may have joined it after the termination reached the rest, is ended too (terminating members already
/// ending changes nothing). A member listed that ended before it could be opened (its process gone, or its id another's)
/// is looked past. The job's completion port isn't the event:
/// Microsoft documents its messages as not guaranteed to arrive (`JOBOBJECT_ASSOCIATE_COMPLETION_PORT`), and its
/// active-process count lingers while a terminated member's handle is held ([`Job::live_processes`]).
fn empties_within(job: &Job, slice: Duration, held: &str) -> Result<bool, Refusal> {
    #[cfg(test)]
    if !crate::emptying::hold::lets_empty(held, slice) {
        return Ok(false);
    }
    #[cfg(not(test))]
    let _ = held;
    let by = Instant::now().checked_add(slice);
    loop {
        let process = match job.member() {
            Ok(sys::Member::Empty) => return Ok(true),
            Ok(sys::Member::Process(process)) => process,
            Err(e) if gone_before_its_open(&e) => continue,
            Err(e) => return Err(Refusal::os("reading a killed box's job", e)),
        };
        job.terminate(job::terminated_code(Signal::KILL))
            .map_err(|e| Refusal::os("ending a box's job", e))?;
        let timeout = match by {
            None => u32::MAX,
            Some(by) => match by.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => ceil_millis(left),
                _ => return Ok(false),
            },
        };
        if !sys::wait(&process, timeout)
            .map_err(|e| Refusal::os("waiting on a killed box's member", e))?
        {
            return Ok(false);
        }
    }
}

/// Whether a member the job listed ended before it could be opened: its process gone (OpenProcess's
/// `ERROR_INVALID_PARAMETER`, no handle keeping it), or its id another process's, outside the job ([`Job::member`]'s
/// `WouldBlock`). Either way the job lists it no more.
fn gone_before_its_open(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
        || e.raw_os_error()
            == i32::try_from(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER).ok()
}

/// `left` in WaitForSingleObject's whole milliseconds, rounded up so a slice's wait never ends before its time, at most
/// the longest finite wait (`INFINITE`, `u32::MAX`, less one).
fn ceil_millis(left: Duration) -> u32 {
    u32::try_from(left.as_nanos().div_ceil(1_000_000))
        .unwrap_or(0xFFFF_FFFE)
        .min(0xFFFF_FFFE)
}

impl Drop for JobBox {
    fn drop(&mut self) {
        // One that outlives its node is left running for the next node: only a delete ends it.
        if !self.state.outlives {
            let _ = self.delete();
        }
    }
}

/// The job limits for `limits` ([`hex_core::job`] already sets the commit limit a margin under the
/// bound and the notification under that).
fn sys_limits(limits: &job::Limits) -> sys::Limits {
    sys::Limits {
        memory: limits.memory,
        processes: limits.processes,
        cpu: limits.cpu,
    }
}
