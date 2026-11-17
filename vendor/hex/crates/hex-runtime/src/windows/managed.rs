//! The original Shards row's native job receipt. The API drains output; the existing BoxProc alone
//! observes the controller. Helper completion waits observe the same nested job and never reap PIDs.
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsHandle;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use triomphe::Arc;

use hex_api::{Bounds, Class, Exit, Refusal, RuntimeCost, Stdio};
use hex_core::record::{NativeRecord, NativeStage};
use shards_api::{
    Identity, ManagedEnd, ManagedProcess, ManagedReceipt, ManagedStatus, ProcessFailure,
    ProcessOperation, TerminalScope,
};

use super::proc::{BoxProc, ObserveError, Operation};
use super::process::JobBox;
use super::sys::{self, Member};
use super::{Held, WinRuntime, copy_path, copy_text, pending, std_handles};

pub(super) struct Receipt {
    identity: Identity,
    process: Mutex<BoxProc>,
}

pub(super) fn host_bounds(record: NativeRecord<'_>) -> Result<Bounds, Refusal> {
    if record.class()? != Class::Shards {
        return Err(record
            .uncertain("its record isn't a Shards VM's, which Windows host bounds are made for"));
    }
    let allocation = record.allocation().ok_or_else(|| {
        record.uncertain("its record holds no allocation to bound its Windows host by")
    })?;
    allocation.guest()?;
    // No commit limit (joint plan step 14, ruling B; A-4), as a Linux VM has no memory.max: its VMM's own memory
    // beside its guest's RAM is never refused at a bound of its guest's size. Its budget's room, the last resort and
    // its fleet's budget hold it; a job a create before this rule bounded is lifted at its adoption
    // (JobBox::adopt_vm).
    let bounds = Bounds {
        cpu_max_milli: Some(u32::try_from(allocation.resources.cpu_milli).map_err(|_| {
            record.uncertain("its allocation's CPU is past a Windows host bound's")
        })?),
        ..Bounds::default()
    };
    bounds.check()?;
    Ok(bounds)
}

/// Called at initialized backend registration, before any candidate can be admitted. It checks the
/// exact CPU representation of every guest quantum plus host overhead; it invents no allowance.
pub(super) fn check_cost(runtime: &WinRuntime, cost: &RuntimeCost) -> Result<(), Refusal> {
    cost.check()?;
    let host = cost
        .allocate(hex_api::CapacityResources::default())?
        .resources;
    let denominator = u64::from(runtime.processors.get())
        .checked_mul(1000)
        .ok_or_else(|| pending("native CPU denominator overflows"))?;
    let exact = |cpu: u64| -> Result<bool, Refusal> {
        Ok(cpu
            .checked_mul(10_000)
            .ok_or_else(|| pending("native CPU cost conversion overflows"))?
            .checked_rem(denominator)
            .ok_or_else(|| pending("native CPU denominator is zero"))?
            == 0)
    };
    if !exact(host.cpu_milli)? || !exact(cost.quantum.cpu_milli)? {
        return Err(Refusal::Invalid {
            what: "initialized VM runtime cost",
            why: copy_text(
                "the measured CPU floor/overhead and quantum are not exactly representable by this host's job hard-cap units",
            )?,
        });
    }
    Ok(())
}

// CreateProcessW and Microsoft's CRT command-line grammar operate on UTF-16, including unpaired
// surrogates in an OS path. The core's String-only quoting cannot represent those paths losslessly.
fn command_line(
    plan: &shards_api::PreparedBoot,
    report: &[std::ffi::OsString],
) -> Result<Vec<u16>, Refusal> {
    let bound = hex_core::hcs::COMMAND_LINE_UNITS
        .checked_add(1)
        .ok_or_else(|| pending("native command-line bound overflows"))?;
    let mut line = Vec::new();
    line.try_reserve_exact(bound)
        .map_err(|_| super::allocation(bound))?;
    for (at, word) in std::iter::once(plan.binary().as_os_str())
        .chain(plan.arguments().iter().map(|arg| arg.as_os_str()))
        .chain(report.iter().map(|arg| arg.as_os_str()))
        .enumerate()
    {
        if at != 0 {
            put(&mut line, bound, u16::from(b' '))?;
        }
        quote(&mut line, bound, word)?;
    }
    put(&mut line, bound, 0)?;
    Ok(line)
}
fn put(line: &mut Vec<u16>, bound: usize, value: u16) -> Result<(), Refusal> {
    if line.len() >= bound {
        return Err(Refusal::Invalid {
            what: "managed VM command line",
            why: copy_text("the prepared command exceeds CreateProcessW's UTF-16 bound")?,
        });
    }
    line.push(value);
    Ok(())
}
fn slashes(line: &mut Vec<u16>, bound: usize, count: usize) -> Result<(), Refusal> {
    for _ in 0..count {
        put(line, bound, u16::from(b'\\'))?;
    }
    Ok(())
}
fn quote(line: &mut Vec<u16>, bound: usize, word: &OsStr) -> Result<(), Refusal> {
    let quoted = word.is_empty()
        || word.encode_wide().any(|unit| {
            [
                u16::from(b' '),
                u16::from(b'\t'),
                u16::from(b'\n'),
                u16::from(b'\x0b'),
                u16::from(b'"'),
            ]
            .contains(&unit)
        });
    if !quoted {
        for unit in word.encode_wide() {
            if unit == 0 {
                return Err(pending("a managed argument contains NUL"));
            }
            put(line, bound, unit)?;
        }
        return Ok(());
    }
    put(line, bound, u16::from(b'"'))?;
    let mut backslashes = 0usize;
    for unit in word.encode_wide() {
        if unit == 0 {
            return Err(pending("a managed argument contains NUL"));
        }
        if unit == u16::from(b'\\') {
            backslashes = backslashes
                .checked_add(1)
                .ok_or_else(|| super::allocation(bound))?;
            continue;
        }
        let count = if unit == u16::from(b'"') {
            backslashes
                .checked_mul(2)
                .and_then(|count| count.checked_add(1))
                .ok_or_else(|| super::allocation(bound))?
        } else {
            backslashes
        };
        slashes(line, bound, count)?;
        put(line, bound, unit)?;
        backslashes = 0;
    }
    slashes(
        line,
        bound,
        backslashes
            .checked_mul(2)
            .ok_or_else(|| super::allocation(bound))?,
    )?;
    put(line, bound, u16::from(b'"'))
}

fn job_name(runtime: &WinRuntime, record: NativeRecord<'_>) -> Result<Option<String>, Refusal> {
    let scope = record
        .scope()
        .ok_or_else(|| record.uncertain("its record names no native scope to name its job by"))?;
    let Some(node) = runtime.node.as_ref() else {
        return Ok(None);
    };
    let prefix = "Local\\hex-";
    let suffix = "-vm-";
    let hex_bytes = scope
        .bytes()
        .len()
        .checked_mul(2)
        .ok_or_else(|| super::allocation(usize::MAX))?;
    let size = prefix
        .len()
        .checked_add(node.len())
        .and_then(|size| size.checked_add(suffix.len()))
        .and_then(|size| size.checked_add(hex_bytes))
        .ok_or_else(|| super::allocation(usize::MAX))?;
    let mut name = String::new();
    name.try_reserve_exact(size)
        .map_err(|_| super::allocation(size))?;
    name.push_str(prefix);
    name.push_str(node);
    name.push_str(suffix);
    use std::fmt::Write;
    for byte in scope.bytes() {
        write!(name, "{byte:02x}").map_err(|_| super::allocation(size))?;
    }
    Ok(Some(name))
}

/// What a managed VM process is given of the node's own environment beside its configured pair: what Shards' own
/// spawner gives a child (its crates/ipc windows.rs `KEPT`, PM M208), so the VM process can give it the confined VM
/// process it starts: `LOCALAPPDATA`, without which Windows makes no child in an AppContainer (ERROR_ENVVAR_NOT_FOUND),
/// and `SystemRoot`, under which Winsock's providers are found as a socket is made.
fn kept(name: &'static str, missing: &'static str) -> Result<std::ffi::OsString, Refusal> {
    match std::env::var_os(name) {
        Some(value) => Ok(value),
        None => Err(Refusal::Invalid {
            what: "a managed VM's environment",
            why: copy_text(missing)?,
        }),
    }
}

fn runtime_environment(paths: &shards_api::RuntimePaths) -> Result<Vec<u16>, Refusal> {
    let local = kept(
        "LOCALAPPDATA",
        "the node's own environment has no LOCALAPPDATA, which Shards' VM process gives the confined VM process it starts (Shards PM M208)",
    )?;
    let root = kept(
        "SystemRoot",
        "the node's own environment has no SystemRoot, which Shards' VM process gives the confined VM process it starts (Shards PM M208)",
    )?;
    // The exact configured provider pair and the node's kept pair, in Windows' case-insensitive environment order.
    let values = [
        ("LOCALAPPDATA", local.as_os_str()),
        ("SHARDS_CACHE", paths.cache().as_os_str()),
        ("SHARDS_HOME", paths.home().as_os_str()),
        ("SystemRoot", root.as_os_str()),
    ];
    let mut size = 1usize;
    for (name, path) in values {
        let entry = name
            .len()
            .checked_add(path.encode_wide().count())
            .and_then(|count| count.checked_add(2))
            .ok_or_else(|| super::allocation(usize::MAX))?;
        if entry > hex_core::hcs::ENV_VAR_UNITS {
            return Err(Refusal::Invalid {
                what: "configured native runtime environment",
                why: copy_text(
                    "a configured runtime path or kept variable exceeds Windows' environment variable bound",
                )?,
            });
        }
        size = size
            .checked_add(entry)
            .ok_or_else(|| super::allocation(usize::MAX))?;
    }
    let mut environment = Vec::new();
    environment
        .try_reserve_exact(size)
        .map_err(|_| super::allocation(size))?;
    for (name, path) in values {
        environment.extend(name.encode_utf16());
        environment.push(u16::from(b'='));
        for unit in path.encode_wide() {
            if unit == 0 {
                return Err(pending(
                    "a configured runtime path or kept variable contains NUL",
                ));
            }
            environment.push(unit);
        }
        environment.push(0);
    }
    environment.push(0);
    Ok(environment)
}

pub(super) fn launch(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
    launch: &crate::devices::ManagedLaunch<'_>,
    io: shards_api::LaunchIo,
    patience: Option<Duration>,
    publish: &mut dyn FnMut(&Identity) -> Result<(), Refusal>,
) -> Result<ManagedReceipt, Refusal> {
    let crate::devices::ManagedLaunch::Shards(plan) = launch else {
        return Err(record.uncertain("its Windows launch isn't a Shards launch"));
    };
    let paths = plan
        .runtime_paths()
        .ok_or_else(|| record.uncertain("its Windows launch plan has no runtime paths"))?;
    // None: a launch that follows its guest, with no deadline of its caller's (the review's C-9).
    let deadline = patience
        .map(|patience| {
            Instant::now()
                .checked_add(patience)
                .filter(|_| patience != Duration::MAX && !patience.is_zero())
                .ok_or_else(|| {
                    record.uncertain("its Windows launch's patience isn't a finite time")
                })
        })
        .transpose()?;
    let deadline = crate::devices::check_managed_report(
        Some(&runtime.catalog),
        record,
        launch,
        io.gpu_report.is_some(),
        deadline,
    )?;
    let request = plan.gpu_report_request();
    record.scope().ok_or_else(|| {
        record.uncertain("its record names no native scope as its Windows launch begins")
    })?;
    let bounds = host_bounds(record)?;
    let shards_api::LaunchIo {
        stdout,
        stderr,
        gpu_report,
    } = io;
    // Duplicate only the owned optional writer. The API's original handle/flags remain untouched;
    // inherited handles retain THIS duplicate's value in CreateProcessW's exact child handle list.
    let report = gpu_report
        .as_ref()
        .map(|writer| sys::inheritable_copy(writer.as_handle()))
        .transpose()
        .map_err(|error| Refusal::os("preparing the private VM report handle", error))?;
    let report_words = match (request, report.as_ref()) {
        (Some(request), Some(writer)) => Some(
            shards_api::physical::report_arguments(request.scope(), writer).map_err(|_| {
                record.uncertain("its Windows native report's arguments couldn't be made")
            })?,
        ),
        (None, None) => None,
        _ => {
            return Err(
                record.uncertain("its Windows launch's report descriptor and request disagree")
            );
        }
    };
    // Prepare every argument, path and stream before a native controller can exist.
    let vsock = copy_path(plan.vsock())?;
    let mut spawn = sys::Spawn {
        command_line: command_line(
            plan,
            report_words.as_ref().map_or(&[], |words| words.as_slice()),
        )?,
        environment: runtime_environment(paths)?,
        cwd: None,
        stdio: std_handles(Stdio {
            stdin: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
            ask: hex_api::Ask::default(),
        })?,
    };
    let name = job_name(runtime, record)?;
    let boxed = JobBox::prepare_vm(
        record,
        bounds,
        runtime.processors,
        name.as_deref(),
        &runtime.holding,
    )?;
    // The canonical Store alone holds selected devices. This is its native process/boot receipt,
    // without another physical-device selection or holder list.
    runtime.holding.reserve(record.owner(), &[], None, None)?;
    let held = Held::Process(Arc::clone(&boxed));
    if runtime.holding.made(record.owner(), held.clone())? {
        boxed.not_submitted()?;
        return Err(record.uncertain(
            "its job's keeper was made before its Windows launch, so it wasn't submitted cleanly",
        ));
    }
    let result = (|| {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            boxed.not_submitted()?;
            return Err(
                record.uncertain("its Windows launch's deadline passed before it was submitted")
            );
        }
        boxed.bind_vm(name.as_deref())?;
        // The existing passive keeper holds this same job before any VM/helper exists.
        runtime.holding.keep(record.owner(), &held)?;
        let sys::Started {
            process,
            thread,
            pid,
        } = boxed.create_controller(&mut spawn, report.as_ref())?;
        // The suspended child now owns the only native-launch copies. Do not hold pipe EOF open
        // through publication/observation or send this writer into the passive keeper.
        drop(report);
        drop(gpu_report);
        let started = sys::created(&process)
            .map_err(|error| Refusal::os("reading the VM controller's birth", error))?;
        if pid == 0 || started == 0 {
            return Err(record.uncertain("its Windows VM controller's pid or start is zero"));
        }
        let identity = Identity {
            pid,
            started,
            vsock,
        };
        let receipt = Receipt {
            identity,
            process: Mutex::new(BoxProc::new(process, pid, boxed.shared())),
        };
        boxed
            .managed
            .set(receipt)
            .map_err(|_| record.uncertain("its Windows controller's receipt was set before"))?;
        let identity = &boxed
            .managed
            .get()
            .ok_or_else(|| {
                record.uncertain("its Windows controller's receipt vanished as it was published")
            })?
            .identity;
        publish(identity)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(record.uncertain(
                "its Windows launch's deadline passed as its controller was published",
            ));
        }
        sys::resume(&thread)
            .map_err(|error| Refusal::os("resuming the recorded VM controller", error))?;
        if !runtime.holding.ready(record.owner())? {
            return Err(
                record.uncertain("its Windows VM keeper wasn't ready after its controller resumed")
            );
        }
        Ok(shards_api::managed_receipt(Arc::clone(&boxed)))
    })();
    if let Err(error) = &result {
        // On every failure this creator's own slot/job/keeper keep the exact claims, marked for
        // cleanup; a refused mark returns beside the failure. No error is promoted to scope
        // absence; the caller's same Creating row remains durable.
        runtime.holding.uncertain_vm_after(&boxed, error)?;
    }
    result
}

pub(super) fn attach(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<ManagedReceipt, Refusal> {
    record.scope().ok_or_else(|| {
        record.uncertain("its record names no native scope as it is attached on Windows")
    })?;
    if identity.is_none()
        && !matches!(
            record.stage(),
            NativeStage::Creating | NativeStage::Deleting
        )
    {
        return Err(record.uncertain(
            "its Windows job box isn't a VM's, or isn't its record's scope, as it is attached",
        ));
    }
    let boxed = match existing(runtime, record)? {
        Some(boxed) => boxed,
        None => cold(runtime, record, identity)?,
    };
    if let Some(expected) = identity {
        match boxed.managed.get() {
            Some(receipt) if receipt.identity == *expected => {}
            None if boxed.recovered() && same_init(boxed.original_init(), expected) => {}
            _ => {
                return Err(record.uncertain(
                    "its Windows controller isn't the one it expected as it is attached",
                ));
            }
        }
    }
    // A recovered scope without its controller has no usable identity; its API view can only clean
    // up the proven same job. The canonical Store remains the sole device/resource holder.
    Ok(shards_api::managed_receipt(boxed))
}

fn same_init(init: Option<hex_core::record::ProcId>, identity: &Identity) -> bool {
    init.is_some_and(|init| init.pid == identity.pid && init.start_ticks == identity.started)
}

fn existing(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
) -> Result<Option<Arc<JobBox>>, Refusal> {
    let commitment = record.commitment()?;
    let boxes = runtime.boxes()?;
    let Ok(at) = super::owner_index(&boxes, record.owner()) else {
        return Ok(None);
    };
    let (_, slot) = boxes
        .get(at)
        .ok_or_else(|| record.uncertain("its Windows slot vanished as it was found"))?;
    match slot.physical() {
        Some(Held::Process(boxed)) if boxed.matches_vm(record, commitment) => {
            Ok(Some(Arc::clone(boxed)))
        }
        _ => Err(record.uncertain("its Windows slot isn't a VM's with its record's scope")),
    }
}

/// Joint plan step 14 (C4 on Windows): the managed VM's job is capped at `rate` milli-CPU, its water-filled share. Only a
/// VM this node holds is capped: one it hasn't taken back is refused, uncertain.
pub(super) fn set_rate(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
    rate: u64,
) -> Result<(), Refusal> {
    existing(runtime, record)?
        .ok_or_else(|| record.uncertain("its node holds no Windows VM slot to set its rate on"))?
        .set_cpu_cap(rate, runtime.processors)
}

/// Joint plan step 14 (C5 on Windows): the last resort ends the managed VM through its receipt, the job this node holds,
/// terminated as the receipt's own stop terminates it; its end is observed as any other. Only a VM this node holds is
/// ended: one it hasn't taken back is refused, uncertain.
pub(super) fn stop(runtime: &WinRuntime, record: NativeRecord<'_>) -> Result<(), Refusal> {
    let boxed = existing(runtime, record)?
        .ok_or_else(|| record.uncertain("its node holds no Windows VM slot to stop"))?;
    if !boxed.is_vm() {
        return Err(record.uncertain("its Windows slot isn't a VM's as it is stopped"));
    }
    boxed
        .stop_managed()
        .map_err(super::proc::ObserveError::refusal)
}

pub(super) fn usage(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
) -> Result<hex_api::Usage, Refusal> {
    let boxed = match existing(runtime, record)? {
        Some(boxed) => boxed,
        None => cold(runtime, record, None)?,
    };
    boxed.usage()
}

fn controller(
    boxed: &JobBox,
    expected: &Identity,
    vsock: std::path::PathBuf,
) -> Result<Receipt, ProcessFailure> {
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    let process = super::oci::sys::open_process(
        expected.pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
    )
    .map_err(|error| io_failure(ProcessOperation::OpenProcess, error))?;
    let started = sys::created(&process)
        .map_err(|error| io_failure(ProcessOperation::GetProcessTimes, error))?;
    if started != expected.started {
        return Err(ProcessFailure::DifferentStart { observed: started });
    }
    let job = boxed
        .job()
        .map_err(|_| ProcessFailure::InvalidStartRecord)?;
    if !super::oci::sys::in_job(&process, job)
        .map_err(|error| io_failure(ProcessOperation::QueryScope, error))?
    {
        return Err(ProcessFailure::DifferentScope);
    }
    Ok(Receipt {
        identity: Identity {
            pid: expected.pid,
            started,
            vsock,
        },
        process: Mutex::new(BoxProc::new(process, expected.pid, boxed.shared())),
    })
}

fn cold(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<Arc<JobBox>, Refusal> {
    if let Some(identity) = identity
        && !same_init(record.init(), identity)
    {
        return Err(record.uncertain("its cold Windows controller isn't its record's"));
    }
    let name = job_name(runtime, record)?.ok_or_else(|| {
        record.uncertain("its record names no native scope to name its cold job by")
    })?;
    let Some(keeping) = runtime.holding.keeper.as_ref() else {
        return Err(record.uncertain("its node runs no Windows VM keeper to take it back"));
    };
    let path = super::keeper::vm::path(&keeping.state, record.owner())?;
    let vsock = identity
        .map(|identity| copy_path(&identity.vsock))
        .transpose()?;
    // Both typed native owners are allocated before opening any old native scope. A retained
    // filename/PID/handle alone is never enough: all commitments and kernel object identity agree.
    let boxed = JobBox::prepare_vm(
        record,
        host_bounds(record)?,
        runtime.processors,
        Some(&name),
        &runtime.holding,
    )?;
    let held = super::keeper::vm::read(&path)?
        .ok_or_else(|| record.uncertain("its Windows VM keeper holds no record of it"))?;
    if !held.commits(boxed.vm_commitment()) {
        return Err(record.uncertain("its Windows VM keeper's record commits another scope"));
    }
    let _keeper = keeping
        .keeper
        .lock()
        .map_err(|_| pending("native keeper lock is poisoned"))?;
    let process = sys::open_process(held.pid)
        .map_err(|error| Refusal::os("opening the committed VM keeper", error))?
        .ok_or_else(|| record.uncertain("its committed Windows VM keeper is gone"))?;
    if sys::created(&process)
        .map_err(|error| Refusal::os("checking the committed VM keeper birth", error))?
        != held.started
        || sys::wait(&process, 0)
            .map_err(|error| Refusal::os("observing the committed VM keeper lifetime", error))?
    {
        return Err(record.uncertain("its committed Windows VM keeper isn't the process it named"));
    }
    let job = sys::Job::open(&name)
        .map_err(|error| Refusal::os("opening the committed native VM scope", error))?
        .ok_or_else(|| record.uncertain("its committed Windows job is gone"))?;
    if !sys::same_in(&process, held.value, job.borrowed())
        .map_err(|error| Refusal::os("checking the committed native VM kernel object", error))?
    {
        return Err(record.uncertain("its committed Windows job isn't the kernel object it named"));
    }
    boxed.adopt_vm(job)?;
    runtime.holding.reserve(record.owner(), &[], None, None)?;
    let retained = Held::Process(Arc::clone(&boxed));
    let cancelled = runtime.holding.made(record.owner(), retained)?;
    let result = (|| {
        runtime.holding.keeper_receipt(
            record.owner(),
            super::KeeperHold::Held(super::KeeperReceipt {
                pid: held.pid,
                created: held.started,
                value: held.value,
            }),
        )?;
        if let (Some(identity), Some(vsock)) = (identity, vsock) {
            match controller(&boxed, identity, vsock) {
                Ok(receipt) => boxed.managed.set(receipt).map_err(|_| {
                    record.uncertain("its cold Windows controller's receipt was set before")
                })?,
                Err(error) => boxed.observation_failure.set(error).map_err(|_| {
                    record.uncertain("its cold Windows controller's failure was set before")
                })?,
            }
        }
        if cancelled || !runtime.holding.ready(record.owner())? {
            return Err(record.uncertain(
                "its cold Windows take-back was cancelled, or its keeper wasn't ready",
            ));
        }
        Ok(())
    })();
    if let Err(error) = &result {
        // The adopted job is this slot's from `made` on: any later failure hands it to cleanup.
        runtime.holding.uncertain_vm_after(&boxed, error)?;
    }
    result.map(|()| boxed)
}

pub(super) fn finalize(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<(), Refusal> {
    record.scope().ok_or_else(|| {
        record.uncertain("its record names no native scope as it is finalized on Windows")
    })?;
    if !matches!(
        record.stage(),
        NativeStage::Deleting | NativeStage::Released
    ) {
        return Err(record.uncertain("it isn't deleting or released as it is finalized on Windows"));
    }
    let boxed = match existing(runtime, record)? {
        Some(boxed) => boxed,
        None if finalized_keeper_is_gone(runtime, record)? => return Ok(()),
        None => cold(runtime, record, identity)?,
    };
    match (identity, boxed.managed.get()) {
        (Some(expected), Some(receipt)) if receipt.identity == *expected => {}
        (Some(expected), None)
            if boxed.recovered() && same_init(boxed.original_init(), expected) => {}
        (None, None) => {}
        _ => {
            return Err(record
                .uncertain("its Windows controller isn't the one it expected as it is finalized"));
        }
    }
    // This query retains keeper/slot ownership. Only the durable Released caller may discharge it.
    let end = match boxed.terminal().map_err(|_| {
        record.uncertain("its Windows scope's end couldn't be read as it is finalized")
    })? {
        Some(
            end @ ManagedEnd {
                scope: TerminalScope::Released,
                ..
            },
        ) => end,
        _ => return Err(record.uncertain("its Windows scope isn't released as it is finalized")),
    };
    if let Some(receipt) = boxed.managed.get() {
        release_app_containers(record, receipt, end)?;
    }
    boxed.release_scope()
}

/// Deletes the AppContainer profiles, and their records, that the VM process of `receipt` made (Shards' D141), once its
/// scope has ended `end`, released: a VM ended by its job's end, as Hex ends every managed VM, leaves them, for Shards'
/// next VM start to sweep (shards_api::release_app_containers). Shards keeps their records per user, beside the
/// profiles, so a controller taken back after its node's end releases them as one that launched its VM does. Each
/// refusal is typed, the finalize refused with it, so its caller tries again: a scope short of released Uncertain; a
/// record a live process holds (a sweep under way, or a process given the VM's pid since) Busy; a deletion's failure an
/// OS refusal.
fn release_app_containers(
    record: NativeRecord<'_>,
    receipt: &Receipt,
    end: ManagedEnd,
) -> Result<usize, Refusal> {
    shards_api::release_app_containers(&receipt.identity, end).map_err(|why| match why {
        shards_api::Refusal::Release(shards_api::ReleaseRefusal::ScopeLive) => record
            .uncertain("its Windows scope wasn't released as its VM's AppContainer profiles were"),
        shards_api::Refusal::Release(shards_api::ReleaseRefusal::Held) => {
            record.refusal(hex_api::LifecycleCause::Busy)
        }
        shards_api::Refusal::ManagedIo { kind, code, .. } => Refusal::os(
            "releasing its VM's AppContainer profiles",
            code.map_or_else(
                || std::io::Error::from(kind),
                std::io::Error::from_raw_os_error,
            ),
        ),
        other => record.uncertain(&format!(
            "releasing its VM's AppContainer profiles: {other}"
        )),
    })
}

/// The caller already published the exact backend Released receipt. After a crash this path only
/// disposes this scope's private metadata; it never opens or terminates a possibly reused job name.
fn finalized_keeper_is_gone(
    runtime: &WinRuntime,
    record: NativeRecord<'_>,
) -> Result<bool, Refusal> {
    let Some(keeping) = runtime.holding.keeper.as_ref() else {
        return Err(record.uncertain("its node runs no Windows VM keeper to prove its end"));
    };
    let scope = record.scope().ok_or_else(|| {
        record.uncertain("its record names no native scope as its keeper's end is proven")
    })?;
    let commitment = record.commitment()?;
    let path = super::keeper::vm::path(&keeping.state, record.owner())?;
    let _keeper = keeping
        .keeper
        .lock()
        .map_err(|_| pending("native keeper lock is poisoned"))?;
    let Some(held) = super::keeper::vm::read(&path)? else {
        // Guarded finalize may have completed just before the canonical Store remove was interrupted.
        return Ok(true);
    };
    if !held.commits(Some((scope, commitment))) {
        return Err(record.uncertain(
            "its Windows VM keeper's record commits another scope as its end is proven",
        ));
    }
    if let Some(process) = sys::open_process(held.pid)
        .map_err(|error| Refusal::os("observing the finalized VM keeper", error))?
        && sys::created(&process)
            .map_err(|error| Refusal::os("checking the finalized VM keeper birth", error))?
            == held.started
        && !sys::wait(&process, 0)
            .map_err(|error| Refusal::os("observing the finalized VM keeper end", error))?
    {
        return Ok(false);
    }
    std::fs::remove_file(path)
        .map_err(|error| Refusal::os("removing the finalized VM keeper receipt", error))?;
    Ok(true)
}

fn cause(identity: Option<&Identity>, failure: ProcessFailure) -> shards_api::Refusal {
    match identity {
        Some(identity) => shards_api::Refusal::Process {
            pid: identity.pid,
            expected_started: identity.started,
            failure,
        },
        None => shards_api::Refusal::Scope(failure),
    }
}
fn io_cause(
    identity: Option<&Identity>,
    operation: ProcessOperation,
    error: std::io::Error,
) -> shards_api::Refusal {
    cause(identity, io_failure(operation, error))
}
fn io_failure(operation: ProcessOperation, error: std::io::Error) -> ProcessFailure {
    match error.raw_os_error() {
        Some(code) => ProcessFailure::Os { operation, code },
        None => ProcessFailure::UnrepresentedIo {
            operation,
            kind: error.kind(),
        },
    }
}
fn observed(identity: Option<&Identity>, error: ObserveError) -> shards_api::Refusal {
    match error {
        ObserveError::DeadlineOverflow => {
            match super::copy_text("the finite native wait deadline is not representable") {
                Ok(why) => shards_api::Refusal::Bounds {
                    what: "managed native wait",
                    why,
                },
                Err(_) => shards_api::Refusal::Allocation {
                    what: "managed wait refusal",
                },
            }
        }
        ObserveError::UnexpectedWait(returned) => {
            cause(identity, ProcessFailure::UnexpectedWait { returned })
        }
        ObserveError::Unpublished => shards_api::Refusal::Launch(shards_api::LaunchError::Observer),
        ObserveError::State(_) => cause(
            identity,
            ProcessFailure::Poisoned {
                what: "native job signal counts",
            },
        ),
        ObserveError::Os { operation, source } => io_cause(
            identity,
            match operation {
                Operation::Wait => ProcessOperation::WaitForExit,
                Operation::ExitCode => ProcessOperation::GetExitCode,
                Operation::StopScope => ProcessOperation::StopScope,
                Operation::QueryScope => ProcessOperation::QueryScope,
            },
            source,
        ),
    }
}
fn status(exit: Exit) -> ManagedStatus {
    match exit {
        Exit::Code(code) => ManagedStatus::Code(code),
        Exit::Signal(signal) => ManagedStatus::Signal(signal),
        Exit::Unknown => ManagedStatus::Unknown,
    }
}

fn scope_proof(boxed: &JobBox) -> Result<(), shards_api::Refusal> {
    if let Some(failure @ ProcessFailure::DifferentScope) = boxed.observation_failure.get() {
        // A positive contradictory membership fact cannot become release merely because the
        // retained job is empty: this observed controller may still own out-of-scope helpers.
        return Err(shards_api::Refusal::Scope(failure.clone()));
    }
    Ok(())
}

impl ManagedProcess for JobBox {
    fn identity(&self) -> Result<&Identity, shards_api::Refusal> {
        if let Some(failure) = self.observation_failure.get() {
            return Err(shards_api::Refusal::Scope(failure.clone()));
        }
        self.managed
            .get()
            .map(|receipt| &receipt.identity)
            .ok_or(shards_api::Refusal::Launch(
                shards_api::LaunchError::Observer,
            ))
    }
    fn stop(&self) -> Result<(), shards_api::Refusal> {
        if !self.is_vm() {
            return Err(shards_api::Refusal::Launch(
                shards_api::LaunchError::NativeRefused,
            ));
        }
        self.stop_managed()
            .map_err(|error| observed(self.managed.get().map(|receipt| &receipt.identity), error))
    }
    fn terminal(&self) -> Result<Option<ManagedEnd>, shards_api::Refusal> {
        scope_proof(self)?;
        if !self.is_vm() {
            return Err(shards_api::Refusal::Launch(
                shards_api::LaunchError::NativeRefused,
            ));
        }
        let receipt = self.managed.get();
        let identity = receipt.map(|receipt| &receipt.identity);
        let exit = match receipt {
            Some(receipt) => receipt
                .process
                .lock()
                .map_err(|_| {
                    cause(
                        identity,
                        ProcessFailure::Poisoned {
                            what: "native VM observer",
                        },
                    )
                })?
                .poll_raw(0)
                .map_err(|error| observed(identity, error))?,
            None => None,
        };
        let empty = self
            .scope_population()
            .map_err(|error| observed(identity, error))?
            == 0;
        if empty {
            return Ok(Some(ManagedEnd {
                status: exit.map_or(ManagedStatus::Unknown, status),
                scope: TerminalScope::Released,
            }));
        }
        Ok(exit.map(|exit| ManagedEnd {
            status: status(exit),
            scope: TerminalScope::Process,
        }))
    }
    fn wait_for(&self, patience: Duration) -> Result<Option<ManagedEnd>, shards_api::Refusal> {
        scope_proof(self)?;
        if !self.is_vm() {
            return Err(shards_api::Refusal::Launch(
                shards_api::LaunchError::NativeRefused,
            ));
        }
        let began = Instant::now();
        let deadline = began.checked_add(patience);
        if patience != Duration::MAX && deadline.is_none() {
            return Err(observed(
                self.managed.get().map(|receipt| &receipt.identity),
                ObserveError::DeadlineOverflow,
            ));
        }
        let receipt = self.managed.get();
        let identity = receipt.map(|receipt| &receipt.identity);
        let exit = match receipt {
            Some(receipt) => match receipt
                .process
                .lock()
                .map_err(|_| {
                    cause(
                        identity,
                        ProcessFailure::Poisoned {
                            what: "native VM observer",
                        },
                    )
                })?
                .wait_raw(patience)
                .map_err(|error| observed(identity, error))?
            {
                Some(exit) => Some(exit),
                None => return Ok(None),
            },
            None => None,
        };
        loop {
            let member = self.member().map_err(|error| observed(identity, error))?;
            let process = match member {
                Member::Empty => {
                    return Ok(Some(ManagedEnd {
                        status: exit.map_or(ManagedStatus::Unknown, status),
                        scope: TerminalScope::Released,
                    }));
                }
                Member::Process(process) => process,
            };
            let left = deadline.map_or(patience, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                return Ok(None);
            }
            let timeout = super::proc::millis(left);
            let ended = sys::wait(&process, timeout)
                .map_err(|error| io_cause(identity, ProcessOperation::WaitForExit, error))?;
            drop(process);
            if !ended {
                if patience == Duration::MAX {
                    return Err(cause(
                        identity,
                        ProcessFailure::UnexpectedWait {
                            returned: windows_sys::Win32::Foundation::WAIT_TIMEOUT,
                        },
                    ));
                }
                return Ok(None);
            }
            // A kernel exit event, not a timer probe, is followed by a fresh same-job membership
            // observation. Missing/recycled snapshot identities refuse above without another picker.
        }
    }
}

#[cfg(test)]
mod runtime_environment_tests {
    use super::*;

    /// AC-T7.4.1: ordinary CPU None keeps the existing CRT command line; the native report is an
    /// explicit four-word suffix, rendered from the final handle by the caller.
    #[test]
    fn cpu_command_line_golden_and_explicit_report_suffix() {
        let spec = shards_api::GuestSpec {
            kernel: "C:\\kernel".into(),
            init: shards_api::Init::Executable("C:\\init".into()),
            cmdline: "console=ttyS0 quiet".into(),
            vcpus: 1,
            memory_mib: 256,
            gpu: false,
            gpu_devices: Vec::new(),
            disks: Vec::new(),
            shares: Vec::new(),
            ports: vec![1024],
            binary: None,
        };
        let plan = shards_api::prepare_boot(
            &spec,
            std::path::Path::new("C:\\native space\\shards.exe"),
            std::path::Path::new("C:\\v"),
            0,
        )
        .unwrap();
        let cpu = "\"C:\\native space\\shards.exe\" run --kernel C:\\kernel --init C:\\init --cmdline \"console=ttyS0 quiet\" --cpus 1 --memory 256 --vsock C:\\v --no-console --console-said 0 --host-managed";
        let expected: Vec<u16> = cpu.encode_utf16().chain([0]).collect();
        assert_eq!(command_line(&plan, &[]).unwrap(), expected);
        let report = [
            "--gpu-report".into(),
            "1234".into(),
            "--gpu-report-scope".into(),
            "11".repeat(16).into(),
        ];
        let reported = format!(
            "{cpu} --gpu-report 1234 --gpu-report-scope {}\0",
            "11".repeat(16)
        );
        assert_eq!(
            command_line(&plan, &report).unwrap(),
            reported.encode_utf16().collect::<Vec<_>>()
        );
    }

    /// AC-T7.4.1 (goal 5's Windows driver, item4-whp's step B): do make a managed VM's environment; expect beside its
    /// configured pair this process's `LOCALAPPDATA` and `SystemRoot`, which Shards' VM process gives the confined VM
    /// process it starts (Shards' crates/ipc windows.rs `KEPT`, PM M208: without `LOCALAPPDATA` Windows makes no child
    /// in an AppContainer, ERROR_ENVVAR_NOT_FOUND), the four in Windows' case-insensitive order.
    #[test]
    fn a_managed_vms_environment_carries_what_shards_gives_its_confined_vm_process() {
        let paths = shards_api::RuntimePaths::new(
            std::path::PathBuf::from("C:\\cache"),
            std::path::PathBuf::from("C:\\home"),
        )
        .unwrap();
        let ours = |name: &str| std::env::var_os(name).unwrap().into_string().unwrap();
        let expected: Vec<u16> = format!(
            "LOCALAPPDATA={}\0SHARDS_CACHE=C:\\cache\0SHARDS_HOME=C:\\home\0SystemRoot={}\0\0",
            ours("LOCALAPPDATA"),
            ours("SystemRoot")
        )
        .encode_utf16()
        .collect();
        assert_eq!(runtime_environment(&paths).unwrap(), expected);
    }

    /// AC-T7.4.1: native OS paths remain lossless; no Unicode replacement or display conversion.
    #[test]
    fn runtime_environment_preserves_native_unpaired_utf16() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut wide: Vec<u16> = "C:\\cache\\雪".encode_utf16().collect();
        wide.push(0xd800);
        let cache = std::ffi::OsString::from_wide(&wide);
        let paths = shards_api::RuntimePaths::new(cache.into(), "C:\\home".into()).unwrap();
        let ours =
            |name: &str| -> Vec<u16> { std::env::var_os(name).unwrap().encode_wide().collect() };
        let mut expected: Vec<u16> = "LOCALAPPDATA=".encode_utf16().collect();
        expected.extend(ours("LOCALAPPDATA"));
        expected.extend("\0SHARDS_CACHE=".encode_utf16());
        expected.extend(wide);
        expected.extend("\0SHARDS_HOME=C:\\home\0SystemRoot=".encode_utf16());
        expected.extend(ours("SystemRoot"));
        expected.extend("\0\0".encode_utf16());
        assert_eq!(runtime_environment(&paths).unwrap(), expected);
    }
}
