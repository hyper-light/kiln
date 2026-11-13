//! Managed Shards controllers reuse the original native ProcessBox, its watcher and its one waiter.
//! The controller remains an unreaped child until the durable selected-boot Released publication.

mod preflight;
pub(super) mod remote;

use std::ffi::CString;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hex_api::{Bounds, BoxId, Class, Exit, LifecycleCause, Refusal, Usage};
use hex_core::record::{NativeRecord, NativeScope, OwnerRef};
use shards_api::{
    Identity, ManagedEnd, ManagedProcess, ManagedReceipt, ManagedStatus, ProcessFailure,
    ProcessOperation, TerminalScope,
};
use triomphe::Arc;

use super::proc::{BoxProc, Prepared, Reaping, process_failure};
use super::process::ProcessBox;
use super::{MacRuntime, native_allocation, native_owner_index, sys};

pub(super) fn uncertain(id: &BoxId) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: LifecycleCause::Uncertain,
        },
        Err(error) => error,
    }
}

pub(super) fn configuration(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return native_allocation(message.len());
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "managed native configuration",
        why,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Prepared,
    Held,
    Running,
    NotSubmitted,
    Uncertain(NativeFailure),
    Finalizing(ManagedEnd),
    Finalized(ManagedEnd),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NativeFailure {
    Process(ProcessFailure),
    Control(sys::TokenFailure),
}
impl NativeFailure {
    fn process(&self) -> ProcessFailure {
        match self {
            Self::Process(failure) => failure.clone(),
            Self::Control(sys::TokenFailure::Io {
                code: Some(code), ..
            }) => ProcessFailure::Os {
                operation: ProcessOperation::QueryScope,
                code: *code,
            },
            Self::Control(sys::TokenFailure::Io {
                code: None, kind, ..
            }) => ProcessFailure::UnrepresentedIo {
                operation: ProcessOperation::QueryScope,
                kind: *kind,
            },
            // Keep the exact typed Mach/birth failure in this native owner. The SDK has no Mach
            // status variant; it receives uncertainty, never a fabricated errno or terminal fact.
            Self::Control(_) => ProcessFailure::InvalidStartRecord,
        }
    }
}

#[derive(Debug)]
pub(super) struct Scope {
    scope: NativeScope,
    commitment: [u8; 32],
    phase: Mutex<Phase>,
    /// The original preparation failure remains in this SAME receipt across failed publication.
    preparation: Mutex<Option<Refusal>>,
    cpu: Option<Mutex<super::cpu::Control>>,
    /// The same helper keeps this writer for the native controller's complete lifetime. Helper
    /// death closes it; the VMM's existing managed lifeline is a fail-closed fence, not release proof.
    lifeline: Mutex<Option<OwnedFd>>,
    /// One coalesced notification writer in the SAME receipt; reconnect replaces only this channel.
    notification: Mutex<Option<OwnedFd>>,
    /// Failed coalesced notification never replaces native terminal authority; retain its first
    /// exact cause for later Inspect even if the watcher has no caller to return it to.
    notification_error: Mutex<Option<ProcessFailure>>,
    vsock: std::sync::OnceLock<std::path::PathBuf>,
}

impl Scope {
    pub(super) fn new(
        record: NativeRecord<'_>,
        control: Option<(super::CpuControl, u64)>,
        baseline: Duration,
    ) -> Result<Self, Refusal> {
        let allocation = record
            .allocation()
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
        allocation.guest()?;
        if record.class()? != Class::Shards {
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
        let (control, maximum) =
            control.ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
        if control.cost_id != allocation.cost.id {
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
        control
            .check()
            .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
        let cpu = Some(Mutex::new(
            super::cpu::Control::new(
                allocation.resources.cpu_milli,
                maximum,
                control.response,
                baseline,
            )
            .map_err(|_| record.refusal(LifecycleCause::Uncertain))?,
        ));
        Ok(Self {
            scope: record
                .scope()
                .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?,
            commitment: record.commitment()?,
            phase: Mutex::new(Phase::Prepared),
            preparation: Mutex::new(None),
            cpu,
            lifeline: Mutex::new(None),
            notification: Mutex::new(None),
            notification_error: Mutex::new(None),
            vsock: std::sync::OnceLock::new(),
        })
    }
    pub(super) fn refusal(&self, owner: OwnerRef<'_>) -> Refusal {
        match owner {
            OwnerRef::Box(id) => uncertain(id),
            OwnerRef::Pool(pool) => Refusal::Backing {
                key: hex_api::BackingKey {
                    pool: *pool,
                    generation: *self.scope.bytes(),
                },
                cause: LifecycleCause::Uncertain,
            },
        }
    }

    fn preparation_failed(&self, error: Refusal) -> Result<(), Refusal> {
        let mut held = self
            .preparation
            .lock()
            .map_err(|_| configuration("native preparation error lock poisoned"))?;
        if held.is_none() {
            *held = Some(error);
        }
        self.notify()
            .map_err(|error| remote::native_failure("publishing native preparation failure", error))
    }
    fn preparation_error(&self) -> Result<Option<Refusal>, Refusal> {
        self.preparation
            .lock()
            .map_err(|_| configuration("native preparation error lock poisoned"))?
            .as_ref()
            .map(|error| {
                remote::diagnostic(error).map(|why| Refusal::Os {
                    op: "original native preparation",
                    why,
                })
            })
            .transpose()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Phase>, ProcessFailure> {
        self.phase.lock().map_err(|_| ProcessFailure::Poisoned {
            what: "native scope phase",
        })
    }
    fn matches(&self, record: NativeRecord<'_>, commitment: [u8; 32]) -> bool {
        record.scope() == Some(self.scope) && commitment == self.commitment
    }
    pub(super) fn held(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        let mut phase = self.lock().map_err(|_| self.refusal(id))?;
        if *phase != Phase::Prepared {
            return Err(self.refusal(id));
        }
        *phase = Phase::Held;
        Ok(())
    }
    pub(super) fn running(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        let mut phase = self.lock().map_err(|_| self.refusal(id))?;
        if *phase != Phase::Held {
            return Err(self.refusal(id));
        }
        *phase = Phase::Running;
        Ok(())
    }
    pub(super) fn not_submitted(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        let mut phase = self.lock().map_err(|_| self.refusal(id))?;
        if !matches!(*phase, Phase::Prepared | Phase::Held) {
            return Err(self.refusal(id));
        }
        *phase = Phase::NotSubmitted;
        Ok(())
    }
    pub(super) fn watchable(&self, id: OwnerRef<'_>) -> Result<bool, Refusal> {
        self.lock()
            .map(|phase| *phase == Phase::Running)
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn no_submission(&self, id: OwnerRef<'_>) -> Result<bool, Refusal> {
        self.lock()
            .map(|phase| matches!(*phase, Phase::Prepared | Phase::NotSubmitted))
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn notify(&self) -> Result<(), ProcessFailure> {
        let mut channel = self
            .notification
            .lock()
            .map_err(|_| ProcessFailure::Poisoned {
                what: "native scope notification",
            })?;
        let Some(writer) = channel.as_ref() else {
            return Ok(());
        };
        let result = match rustix::io::write(writer.as_fd(), &[1]) {
            Ok(1) | Err(rustix::io::Errno::AGAIN) => Ok(()),
            // Its one reader, the attached node's proxy, has ended (that node died or let go): no one is
            // left to wake. The channel goes; the next attach makes a new one and notifies it at once
            // (remote's `attached`), so nothing it would have said is lost.
            Err(rustix::io::Errno::PIPE) => {
                *channel = None;
                return Ok(());
            }
            Ok(_) => Err(ProcessFailure::InvalidStartRecord),
            Err(error) => Err(process_failure(ProcessOperation::QueryScope, error.into())),
        };
        if let Err(error) = &result {
            let mut retained =
                self.notification_error
                    .lock()
                    .map_err(|_| ProcessFailure::Poisoned {
                        what: "native notification failure",
                    })?;
            if retained.is_none() {
                *retained = Some(error.clone());
            }
        }
        result
    }
    fn observation_error(&self) -> Result<Option<Refusal>, Refusal> {
        if let Some(error) = self
            .notification_error
            .lock()
            .map_err(|_| configuration("native notification failure lock poisoned"))?
            .as_ref()
        {
            return Ok(Some(remote::native_failure(
                "original native notification failure",
                error.clone(),
            )));
        }
        let phase = self
            .lock()
            .map_err(|error| remote::native_failure("native scope phase lock", error))?;
        match &*phase {
            Phase::Uncertain(failure) => Ok(Some(Refusal::Os {
                op: "original native scope failure",
                why: remote::diagnostic(format_args!("{failure:?}"))?,
            })),
            _ => Ok(None),
        }
    }
    pub(super) fn start_cpu(&self, id: OwnerRef<'_>) -> Result<super::cpu::Decision, Refusal> {
        let cpu = self.cpu.as_ref().ok_or_else(|| self.refusal(id))?;
        let observed =
            sys::managed_process_cpu().map_err(|error| Refusal::os("reading helper CPU", error))?;
        let now = Instant::now();
        let mut cpu = cpu.lock().map_err(|_| self.refusal(id))?;
        cpu.start(now, observed).map_err(|_| self.refusal(id))?;
        cpu.observe(now, observed).map_err(|_| self.refusal(id))
    }
    pub(super) fn cpu_decision(
        &self,
        id: OwnerRef<'_>,
    ) -> Result<Option<super::cpu::Decision>, Refusal> {
        let Some(cpu) = &self.cpu else {
            return Ok(None);
        };
        let observed =
            sys::managed_process_cpu().map_err(|error| Refusal::os("reading helper CPU", error))?;
        cpu.lock()
            .map_err(|_| self.refusal(id))?
            .observe(Instant::now(), observed)
            .map(Some)
            .map_err(|_| self.refusal(id))
    }
    /// Joint plan step 14 (C4 on macOS): the scope's CPU rate becomes `rate`, its share as the node's learner water-fills
    /// it; what the helper used up to now is charged at the rate it had (`cpu::Control::set_rate`).
    pub(super) fn set_cpu_rate(&self, id: OwnerRef<'_>, rate: u64) -> Result<(), Refusal> {
        let cpu = self.cpu.as_ref().ok_or_else(|| self.refusal(id))?;
        let observed =
            sys::managed_process_cpu().map_err(|error| Refusal::os("reading helper CPU", error))?;
        cpu.lock()
            .map_err(|_| self.refusal(id))?
            .set_rate(Instant::now(), observed, rate)
            .map(drop)
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn cpu_permit(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        self.cpu
            .as_ref()
            .ok_or_else(|| self.refusal(id))?
            .lock()
            .map_err(|_| self.refusal(id))?
            .permit()
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn cpu_held(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        self.cpu_decision(id)?;
        self.cpu
            .as_ref()
            .ok_or_else(|| self.refusal(id))?
            .lock()
            .map_err(|_| self.refusal(id))?
            .held()
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn finalizing(&self, end: ManagedEnd, id: OwnerRef<'_>) -> Result<(), Refusal> {
        if end.scope != TerminalScope::Released {
            return Err(self.refusal(id));
        }
        let mut phase = self.lock().map_err(|_| self.refusal(id))?;
        match &*phase {
            Phase::Finalizing(known) | Phase::Finalized(known) if *known != end => {
                return Err(self.refusal(id));
            }
            Phase::Finalized(_) => return Ok(()),
            _ => {}
        }
        *phase = Phase::Finalizing(end);
        Ok(())
    }
    fn finalized(&self, id: OwnerRef<'_>) -> Result<(), Refusal> {
        let mut phase = self.lock().map_err(|_| self.refusal(id))?;
        match *phase {
            Phase::Finalizing(end) => {
                *phase = Phase::Finalized(end);
                Ok(())
            }
            Phase::Finalized(_) => Ok(()),
            _ => Err(self.refusal(id)),
        }
    }
    pub(super) fn is_finalized(&self) -> bool {
        self.lock()
            .is_ok_and(|phase| matches!(*phase, Phase::Finalizing(_) | Phase::Finalized(_)))
    }
    pub(super) fn failed(&self, failure: ProcessFailure) {
        if let Ok(mut phase) = self.lock()
            && !matches!(
                *phase,
                Phase::Finalizing(_)
                    | Phase::Finalized(_)
                    | Phase::Uncertain(NativeFailure::Control(_))
            )
        {
            *phase = Phase::Uncertain(NativeFailure::Process(failure));
        }
        let _ = self.notify();
    }
    pub(super) fn control_failed(
        &self,
        failure: sys::TokenFailure,
        owner: OwnerRef<'_>,
    ) -> Refusal {
        if let Ok(mut phase) = self.lock()
            && !matches!(*phase, Phase::Finalizing(_) | Phase::Finalized(_))
        {
            *phase = Phase::Uncertain(NativeFailure::Control(failure));
        }
        self.refusal(owner)
    }
    pub(super) fn validate(
        &self,
        pgid: Option<i32>,
        started: Option<u64>,
        id: OwnerRef<'_>,
    ) -> Result<(), Refusal> {
        self.validate_native(pgid, started)
            .map_err(|_| self.refusal(id))
    }
    pub(super) fn validate_native(
        &self,
        pgid: Option<i32>,
        started: Option<u64>,
    ) -> Result<(), ProcessFailure> {
        let Some(pid) = pgid else {
            return Ok(());
        };
        if pid <= 0 {
            return Err(ProcessFailure::PidOutOfRange {
                operation: ProcessOperation::QueryScope,
            });
        }
        // This persistent owner only creates direct children. The proxy never constructs a
        // new ProcessBox from a cold PID/PGID, and this child is pinned until durable finalize.
        let actual = sys::rusage(pid)
            .map_err(|error| process_failure(ProcessOperation::ProcPidRusage, error))?
            .ok_or(ProcessFailure::InvalidStartRecord)?;
        if started.is_some_and(|expected| expected != actual.started) {
            return Err(ProcessFailure::DifferentStart {
                observed: actual.started,
            });
        }
        // getpgid finds no process once the controller is exiting, before its rusage says it ended: that
        // has left no group. Only a live controller in another group has left its scope.
        if !actual.ended
            && sys::group_of(pid)
                .map_err(|error| process_failure(ProcessOperation::QueryScope, error))?
                .is_some_and(|group| group != pid)
        {
            return Err(ProcessFailure::DifferentScope);
        }
        Ok(())
    }
}

pub(super) enum Controller {
    Unsubmitted,
    Prepared(Prepared),
    Held {
        process: BoxProc,
        failure: Option<ProcessFailure>,
    },
}

fn host_bounds(record: NativeRecord<'_>) -> Result<Bounds, Refusal> {
    if record.class()? != Class::Shards {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let allocation = record
        .allocation()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    allocation.guest()?;
    let bounds = Bounds {
        cpu_max_milli: Some(
            u32::try_from(allocation.resources.cpu_milli)
                .map_err(|_| record.refusal(LifecycleCause::Uncertain))?,
        ),
        memory_max: Some(allocation.resources.mem_bytes),
        ..Bounds::default()
    };
    bounds.check()?;
    Ok(bounds)
}

fn bytes(value: &[u8], record: NativeRecord<'_>) -> Result<CString, Refusal> {
    let count = value
        .len()
        .checked_add(1)
        .ok_or_else(|| native_allocation(usize::MAX))?;
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(count)
        .map_err(|_| native_allocation(count))?;
    copied.extend_from_slice(value);
    copied.push(0);
    CString::from_vec_with_nul(copied).map_err(|_| record.refusal(LifecycleCause::Uncertain))
}
fn environment(
    name: &[u8],
    path: &std::path::Path,
    record: NativeRecord<'_>,
) -> Result<CString, Refusal> {
    let path = path.as_os_str().as_bytes();
    let count = name
        .len()
        .checked_add(path.len())
        .and_then(|count| count.checked_add(2))
        .ok_or_else(|| native_allocation(usize::MAX))?;
    let mut value = Vec::new();
    value
        .try_reserve_exact(count)
        .map_err(|_| native_allocation(count))?;
    value.extend_from_slice(name);
    value.push(b'=');
    value.extend_from_slice(path);
    value.push(0);
    CString::from_vec_with_nul(value).map_err(|_| record.refusal(LifecycleCause::Uncertain))
}
fn spawn(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    launch: &hex_core::helper::managed::Launch,
    descriptors: Vec<OwnedFd>,
    lifeline: OwnedFd,
    bounds: &Bounds,
) -> Result<sys::Spawn, Refusal> {
    launch
        .check()
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    if launch
        .report_scope
        .is_some_and(|scope| record.scope().map(|actual| *actual.bytes()) != Some(scope))
    {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let mut descriptors = descriptors.into_iter();
    let stdout = descriptors
        .next()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let stderr = descriptors
        .next()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let report = descriptors.next();
    if descriptors.next().is_some() || report.is_some() != launch.report_scope.is_some() {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    // SCM_RIGHTS can change the numeric descriptor. Duplicate only the receiver-owned end, with
    // CLOEXEC, then render from that FINAL copy. Never alter sender flags or rewrite base argv.
    let report = report
        .map(|writer| rustix::io::fcntl_dupfd_cloexec(writer, 3))
        .transpose()
        .map_err(|error| Refusal::os("preparing the final native report writer", error))?;
    let lifeline = rustix::io::fcntl_dupfd_cloexec(lifeline, 3)
        .map_err(|error| Refusal::os("preparing the final native lifeline", error))?;
    let report_words = match (launch.report_scope, report.as_ref()) {
        (Some(scope), Some(writer)) => Some(
            shards_api::physical::report_arguments(scope, writer)
                .map_err(|_| record.refusal(LifecycleCause::Uncertain))?,
        ),
        (None, None) => None,
        _ => return Err(record.refusal(LifecycleCause::Uncertain)),
    };
    let lifeline_words = shards_api::physical::lifeline_arguments(&lifeline)
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    let count = launch
        .arguments
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_add(report_words.as_ref().map_or(0, |words| words.len())))
        .and_then(|count| count.checked_add(lifeline_words.len()))
        .ok_or_else(|| native_allocation(usize::MAX))?;
    let mut argv = Vec::new();
    argv.try_reserve_exact(count)
        .map_err(|_| native_allocation(count))?;
    argv.push(bytes(&launch.binary, record)?);
    for word in &launch.arguments {
        argv.push(bytes(word, record)?);
    }
    for word in report_words.iter().flatten().chain(&lifeline_words) {
        argv.push(bytes(word.as_os_str().as_bytes(), record)?);
    }
    let mut envp = Vec::new();
    envp.try_reserve_exact(launch.environment.len())
        .map_err(|_| native_allocation(launch.environment.len()))?;
    for value in &launch.environment {
        envp.push(bytes(value, record)?);
    }
    let size = argv
        .iter()
        .chain(&envp)
        .try_fold(0usize, |sum, word| {
            sum.checked_add(word.as_bytes_with_nul().len())
        })
        .ok_or_else(|| native_allocation(usize::MAX))?;
    let pointers = argv
        .len()
        .checked_add(envp.len())
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_mul(size_of::<*const std::ffi::c_char>()))
        .ok_or_else(|| native_allocation(usize::MAX))?;
    if size
        .checked_add(pointers)
        .is_none_or(|size| size > runtime.exec_limit.block)
    {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let mut binaries = Vec::new();
    binaries
        .try_reserve_exact(1)
        .map_err(|_| native_allocation(1))?;
    binaries.push(bytes(&launch.binary, record)?);
    Ok(sys::Spawn {
        paths: binaries,
        argv,
        envp,
        cwd: None,
        stdio: [super::null(rustix::fs::OFlags::RDONLY)?, stdout, stderr],
        report,
        lifeline: Some(lifeline),
        jetsam_mib: bounds.memory_max.and_then(sys::jetsam_cap_mib),
    })
}

fn retain(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    boxed: &Arc<ProcessBox>,
) -> Result<(), Refusal> {
    let id = record.owner().try_owned()?;
    let mut boxes = runtime.boxes()?;
    let at = match native_owner_index(&boxes, id.as_ref()) {
        Ok(at) => {
            let old = boxes
                .get(at)
                .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
            let scope = old
                .1
                .scope()
                .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
            if !matches!(
                *scope
                    .lock()
                    .map_err(|_| record.refusal(LifecycleCause::Uncertain))?,
                Phase::Finalized(_)
            ) {
                return Err(record.refusal(LifecycleCause::Uncertain));
            }
            let old = boxes
                .get_mut(at)
                .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
            let retired = std::mem::replace(old, (id, Arc::clone(boxed)));
            drop(boxes);
            drop(retired);
            return Ok(());
        }
        Err(at) => at,
    };
    if boxes.len() >= crate::MAX_BOXES {
        return Err(Refusal::Full(crate::MAX_BOXES));
    }
    boxes
        .try_reserve(1)
        .map_err(|_| native_allocation(crate::MAX_BOXES))?;
    boxes.insert(at, (id, Arc::clone(boxed)));
    Ok(())
}

pub(super) fn launch(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    plan: &crate::devices::ManagedLaunch<'_>,
    io: shards_api::LaunchIo,
    patience: Duration,
    publish: &mut dyn FnMut(&Identity) -> Result<(), Refusal>,
) -> Result<ManagedReceipt, Refusal> {
    remote::launch(runtime, record, plan, io, patience, publish)
}

fn prepare_native(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    launch: &hex_core::helper::managed::Launch,
    descriptors: Vec<OwnedFd>,
    deadline: Instant,
) -> Result<Arc<ProcessBox>, Refusal> {
    let bounds = host_bounds(record)?;
    let (lifeline_reader, lifeline_writer) = std::io::pipe()
        .map(|(reader, writer)| (OwnedFd::from(reader), OwnedFd::from(writer)))
        .map_err(|error| Refusal::os("preparing the native managed lifeline", error))?;
    for end in [&lifeline_reader, &lifeline_writer] {
        super::private_descriptor(end, false)
            .map_err(|error| Refusal::os("preparing the native managed lifeline", error))?;
    }
    let spawn = spawn(
        runtime,
        record,
        launch,
        descriptors,
        lifeline_reader,
        &bounds,
    )?;
    let vsock = std::path::PathBuf::from(std::ffi::OsString::from_vec(copy_raw(&launch.vsock)?));
    sys::retained_children()
        .map_err(|error| Refusal::os("checking retained-child ownership", error))?;
    let first = {
        let boxes = runtime.boxes()?;
        boxes.is_empty()
    };
    let baseline = if first {
        Duration::ZERO
    } else {
        sys::managed_process_cpu()
            .map_err(|error| Refusal::os("reading pre-prepare helper CPU", error))?
    };
    let boxed = ProcessBox::prepare_managed(
        record,
        &bounds,
        &runtime.shared,
        runtime.cpu_control,
        baseline,
    )?;
    boxed
        .scope()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?
        .vsock
        .set(copy_path(&vsock)?)
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    *boxed
        .scope()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?
        .lifeline
        .lock()
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))? = Some(lifeline_writer);
    retain(runtime, record, &boxed)?;
    if Instant::now() >= deadline {
        *boxed
            .scope()
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?
            .lock()
            .map_err(|_| record.refusal(LifecycleCause::Uncertain))? = Phase::NotSubmitted;
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let mut controller = boxed
        .controller
        .lock()
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    let prepared = match std::mem::replace(&mut *controller, Controller::Unsubmitted) {
        Controller::Prepared(prepared) => prepared,
        other => {
            *controller = other;
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
    };
    let pid = match boxed.held_exec(&spawn) {
        Ok(pid) => pid,
        Err(error) => {
            boxed.not_submitted()?;
            return Err(error);
        }
    };
    let (process, registered) = prepared.bind(pid, Some(boxed.members()), Reaping::Retained);
    *controller = Controller::Held {
        process,
        failure: registered
            .err()
            .map(|error| process_failure(ProcessOperation::RegisterExit, error)),
    };
    let observed = sys::rusage(pid)
        .map_err(|error| Refusal::os("reading the held VM controller birth", error))?
        .filter(|usage| usage.started != 0)
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    boxed.introduce_group(pid, observed.started)?;
    let identity = Identity {
        pid: u32::try_from(pid).map_err(|_| record.refusal(LifecycleCause::Uncertain))?,
        started: observed.started,
        vsock,
    };
    boxed
        .identity
        .set(identity)
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    if let Controller::Held {
        failure: Some(_), ..
    } = &*controller
    {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    drop(controller);
    // Identity is returned while the exact controller is still held. The node alone performs its
    // canonical Store publication; Resume is a separate scope/commitment/controller-fenced action.
    Ok(boxed)
}

fn copy_raw(raw: &[u8]) -> Result<Vec<u8>, Refusal> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(raw.len())
        .map_err(|_| native_allocation(raw.len()))?;
    copied.extend_from_slice(raw);
    Ok(copied)
}

fn copy_path(path: &std::path::Path) -> Result<std::path::PathBuf, Refusal> {
    let mut value = std::ffi::OsString::new();
    value
        .try_reserve_exact(path.as_os_str().len())
        .map_err(|_| native_allocation(path.as_os_str().len()))?;
    value.push(path);
    Ok(value.into())
}

pub(super) fn attach(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<ManagedReceipt, Refusal> {
    Ok(shards_api::managed_receipt(remote::attach(
        runtime, record, identity,
    )?))
}
pub(super) fn usage(runtime: &MacRuntime, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
    remote::usage(runtime, record)
}
pub(super) fn set_rate(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    rate: u64,
) -> Result<(), Refusal> {
    remote::set_rate(runtime, record, rate)
}
pub(super) fn finalize(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<(), Refusal> {
    remote::finalize(runtime, record, identity)
}

fn finalize_native(boxed: &Arc<ProcessBox>) -> Result<(), Refusal> {
    let scope = boxed
        .scope()
        .ok_or_else(|| configuration("native finalization has no owned scope"))?;
    let end = boxed
        .terminal()
        .map_err(|_| scope.refusal(boxed.owner()))?
        .filter(|end| end.scope == TerminalScope::Released)
        .ok_or_else(|| scope.refusal(boxed.owner()))?;
    boxed.finalize_watcher(end)?;
    if let Controller::Held { process, .. } = &mut *boxed
        .controller
        .lock()
        .map_err(|_| scope.refusal(boxed.owner()))?
    {
        process
            .finalize_retained()
            .map_err(|_| scope.refusal(boxed.owner()))?;
    }
    scope.finalized(boxed.owner())
}

fn status(exit: Exit) -> ManagedStatus {
    match exit {
        Exit::Code(code) => ManagedStatus::Code(code),
        Exit::Signal(signal) => ManagedStatus::Signal(signal),
        Exit::Unknown => ManagedStatus::Unknown,
    }
}

impl ProcessBox {
    fn snapshot(
        &self,
        controller: &mut Controller,
        pids: &mut Vec<i32>,
    ) -> Result<Option<ManagedEnd>, ProcessFailure> {
        let scope = self.scope().ok_or(ProcessFailure::InvalidStartRecord)?;
        let phase = scope.lock()?;
        let observation_failure = match &*phase {
            Phase::Uncertain(failure) => Some(failure.process()),
            _ => None,
        };
        if let Phase::Finalizing(end) | Phase::Finalized(end) = *phase {
            return Ok(Some(end));
        }
        if *phase == Phase::NotSubmitted {
            return Ok(Some(ManagedEnd {
                status: ManagedStatus::Unknown,
                scope: TerminalScope::Released,
            }));
        }
        drop(phase);
        let Controller::Held { process, failure } = controller else {
            return Ok(None);
        };
        let observation_failure = failure.clone().or(observation_failure);
        let exit = process.native_poll()?;
        let Some(exit) = exit else {
            return match observation_failure {
                Some(failure) => Err(failure),
                None => Ok(None),
            };
        };
        let pid = process.pid_i32();
        let actual = sys::rusage(pid)
            .map_err(|error| process_failure(ProcessOperation::ProcPidRusage, error))?
            .ok_or(ProcessFailure::InvalidStartRecord)?;
        if self
            .identity
            .get()
            .is_some_and(|identity| actual.started != identity.started)
        {
            return Err(ProcessFailure::DifferentStart {
                observed: actual.started,
            });
        }
        // WNOWAIT still owns this exact direct child. Its zombie pins the launch PGID even
        // where getpgid no longer exposes an exited process through proc_find.
        if !actual.ended {
            return Err(ProcessFailure::InvalidStartRecord);
        }
        if self
            .status()
            .map_err(|_| ProcessFailure::Poisoned {
                what: "native scope accounting",
            })?
            .escaped
            != 0
        {
            return Err(ProcessFailure::DifferentScope);
        }
        if sys::group(pid, pids)
            .map_err(|error| process_failure(ProcessOperation::QueryScope, error))?
        {
            return Err(ProcessFailure::BufferBounds);
        }
        let mut observation_error = None;
        // A member the listing held and rusage no longer has was reaped since: it is gone, as the
        // ordinary watcher's read_members leaves it out. A reused pid reads as live, never less.
        pids.retain(|member| match sys::rusage(*member) {
            Ok(Some(usage)) => !usage.ended,
            Ok(None) => false,
            Err(error) => {
                observation_error
                    .get_or_insert_with(|| process_failure(ProcessOperation::ProcPidRusage, error));
                false
            }
        });
        if let Some(error) = observation_error {
            return Err(error);
        }
        let live = !pids.is_empty();
        Ok(Some(ManagedEnd {
            status: status(exit),
            scope: if live {
                TerminalScope::Process
            } else {
                TerminalScope::Released
            },
        }))
    }
}

impl ManagedProcess for ProcessBox {
    fn identity(&self) -> Result<&Identity, shards_api::Refusal> {
        self.identity.get().ok_or(shards_api::Refusal::Scope(
            ProcessFailure::InvalidStartRecord,
        ))
    }
    fn terminal(&self) -> Result<Option<ManagedEnd>, shards_api::Refusal> {
        let mut controller = self.controller.lock().map_err(|_| {
            shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                what: "native controller",
            })
        })?;
        let mut pids = self.scope_pids.lock().map_err(|_| {
            shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                what: "native scope members",
            })
        })?;
        self.snapshot(&mut controller, &mut pids)
            .map_err(shards_api::Refusal::Scope)
    }
    fn wait_for(&self, patience: Duration) -> Result<Option<ManagedEnd>, shards_api::Refusal> {
        let until = if patience == Duration::MAX {
            None
        } else {
            Some(
                Instant::now()
                    .checked_add(patience)
                    .ok_or(shards_api::Refusal::Scope(
                        ProcessFailure::InvalidStartRecord,
                    ))?,
            )
        };
        let mut controller = self.controller.lock().map_err(|_| {
            shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                what: "native controller",
            })
        })?;
        let mut pids = self.scope_pids.lock().map_err(|_| {
            shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                what: "native scope members",
            })
        })?;
        loop {
            if let Some(end) = self
                .snapshot(&mut controller, &mut pids)
                .map_err(shards_api::Refusal::Scope)?
                && end.scope == TerminalScope::Released
            {
                return Ok(Some(end));
            }
            let remaining = until.map_or(Duration::MAX, |until| {
                until.saturating_duration_since(Instant::now())
            });
            if remaining.is_zero() {
                return Ok(None);
            }
            let Controller::Held { process, .. } = &mut *controller else {
                return Err(shards_api::Refusal::Scope(
                    ProcessFailure::InvalidStartRecord,
                ));
            };
            if !process
                .wait_scope(&pids, remaining)
                .map_err(shards_api::Refusal::Scope)?
            {
                return Ok(None);
            }
        }
    }
    fn stop(&self) -> Result<(), shards_api::Refusal> {
        self.scope().ok_or(shards_api::Refusal::Scope(
            ProcessFailure::InvalidStartRecord,
        ))?;
        self.stop_managed_raw().map_err(shards_api::Refusal::Scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            scope: NativeScope::from_bytes([1; 16]),
            commitment: [2; 32],
            phase: Mutex::new(Phase::Prepared),
            preparation: Mutex::new(None),
            cpu: None,
            lifeline: Mutex::new(None),
            notification: Mutex::new(None),
            notification_error: Mutex::new(None),
            vsock: std::sync::OnceLock::new(),
        }
    }

    /// AC-T7.4.1: typed uncertainty retains the exact backing generation without a Box alias.
    #[test]
    fn pool_scope_errors_keep_the_same_generation() {
        let scope = scope();
        let pool = [9; 32];
        assert!(
            matches!(scope.refusal(OwnerRef::Pool(&pool)), Refusal::Backing { key, .. }
            if key.pool == pool && key.generation == [1; 16])
        );
        assert!(scope.running(OwnerRef::Pool(&pool)).is_err());
        scope.held(OwnerRef::Pool(&pool)).unwrap();
        scope.running(OwnerRef::Pool(&pool)).unwrap();
    }

    /// AC-T7.4.1 (restart): a scope whose attached node died (its notification reader closed) is
    /// notified: expect no failure, none retained, and the channel let go for the next attach's.
    #[test]
    fn a_notification_whose_reader_ended_lets_the_channel_go() {
        let scope = scope();
        let (reader, writer) = std::io::pipe().unwrap();
        *scope.notification.lock().unwrap() = Some(OwnedFd::from(writer));
        drop(reader);
        scope.notify().unwrap();
        assert!(scope.notification.lock().unwrap().is_none());
        assert!(scope.notification_error.lock().unwrap().is_none());
        assert!(scope.observation_error().unwrap().is_none());
    }

    /// AC-T7.4.1: a controller cannot become watchable before its held publication stage.
    #[test]
    fn preparation_cannot_skip_held_publication() {
        let id = BoxId::new("scope-order").unwrap();
        let scope = scope();
        assert!(!scope.watchable(OwnerRef::Box(&id)).unwrap());
        assert!(scope.running(OwnerRef::Box(&id)).is_err());
        scope.held(OwnerRef::Box(&id)).unwrap();
        assert!(!scope.watchable(OwnerRef::Box(&id)).unwrap());
        scope.running(OwnerRef::Box(&id)).unwrap();
        assert!(scope.watchable(OwnerRef::Box(&id)).unwrap());
        assert!(scope.not_submitted(OwnerRef::Box(&id)).is_err());
    }

    /// AC-T7.4.1: cleanup retries keep the exact already observed terminal result after durable release.
    #[test]
    fn finalize_keeps_the_exact_released_result() {
        let id = BoxId::new("scope-result").unwrap();
        let scope = scope();
        let end = ManagedEnd {
            status: ManagedStatus::Code(7),
            scope: TerminalScope::Released,
        };
        assert!(
            scope
                .finalizing(
                    ManagedEnd {
                        status: end.status,
                        scope: TerminalScope::Process
                    },
                    OwnerRef::Box(&id)
                )
                .is_err()
        );
        scope.finalizing(end, OwnerRef::Box(&id)).unwrap();
        scope.failed(ProcessFailure::InvalidStartRecord);
        assert!(scope.is_finalized());
        assert!(
            scope
                .finalizing(
                    ManagedEnd {
                        status: ManagedStatus::Code(8),
                        ..end
                    },
                    OwnerRef::Box(&id)
                )
                .is_err()
        );
        scope.finalized(OwnerRef::Box(&id)).unwrap();
        assert_eq!(*scope.lock().unwrap(), Phase::Finalized(end));
    }
}
