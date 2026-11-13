//! The node's receipt is only a proxy. The persistent helper owns the SAME native ProcessBox,
//! watcher, controller and WNOWAIT receipt; the canonical Store/physical claims remain in Node.
#[cfg(test)]
mod controls;
mod receipt;
mod server;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hex_api::{Exit, LifecycleCause, Refusal, Usage};
use hex_core::helper::{VERSION, managed as protocol};
use hex_core::record::{NativeRecord, ProcId};
use rustix::event::PollFlags;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use shards_api::{
    Identity, ManagedEnd, ManagedProcess, ManagedReceipt, ManagedStatus, ProcessFailure,
    TerminalScope,
};
use triomphe::Arc;

use super::{MacRuntime, ProcessBox, copy_raw, native_allocation, sys};
use crate::macos::helper_client::frame;

enum CallFailure {
    Native(protocol::Failure),
    Local(Refusal),
    Rejected(protocol::rejected::Rejected),
}
impl From<Refusal> for CallFailure {
    fn from(error: Refusal) -> Self {
        Self::Local(error)
    }
}
impl CallFailure {
    fn io(operation: protocol::Operation, error: impl Into<std::io::Error>) -> Self {
        let error = error.into();
        Self::Native(protocol::Failure::Io {
            operation,
            code: error.raw_os_error(),
            kind: error.kind(),
        })
    }
    fn framed(operation: protocol::Operation, error: frame::Failure) -> Self {
        Self::Native(error.managed(operation))
    }
    fn timeout(&self) -> bool {
        matches!(
            self,
            Self::Native(protocol::Failure::Io {
                kind: std::io::ErrorKind::TimedOut,
                ..
            })
        )
    }
    fn refusal(self) -> Refusal {
        match self {
            Self::Local(error) => error,
            Self::Rejected(rejected) => match String::from_utf8(rejected.detail) {
                Ok(why) => Refusal::Os {
                    op: "native helper refused",
                    why,
                },
                Err(_) => super::configuration("native helper refusal is not UTF-8"),
            },
            Self::Native(protocol::Failure::Allocation { bytes }) => {
                native_allocation(usize::try_from(bytes).unwrap_or(usize::MAX))
            }
            Self::Native(failure) => match diagnostic(format_args!("{failure:?}")) {
                Ok(why) => Refusal::Os {
                    op: "native helper control",
                    why,
                },
                Err(error) => error,
            },
        }
    }
    fn api(self) -> shards_api::Refusal {
        match self {
            Self::Rejected(rejected) => match rejected.failure {
                protocol::Failure::Io {
                    operation,
                    code,
                    kind,
                } => api_io(operation, code, kind),
                _ => match String::from_utf8(rejected.detail) {
                    Ok(why) => shards_api::Refusal::Boot(why),
                    Err(_) => shards_api::Refusal::Scope(ProcessFailure::InvalidStartRecord),
                },
            },
            Self::Native(protocol::Failure::Io {
                operation,
                code,
                kind,
            }) => api_io(operation, code, kind),
            Self::Native(protocol::Failure::Allocation { .. }) => shards_api::Refusal::Allocation {
                what: "native helper control",
            },
            Self::Native(
                protocol::Failure::Scope
                | protocol::Failure::Commitment
                | protocol::Failure::Controller,
            ) => shards_api::Refusal::Scope(ProcessFailure::DifferentScope),
            Self::Native(protocol::Failure::Poisoned) => {
                shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                    what: "native helper control",
                })
            }
            Self::Native(_) => shards_api::Refusal::Scope(ProcessFailure::InvalidStartRecord),
            Self::Local(error) => match diagnostic(error) {
                Ok(why) => shards_api::Refusal::Boot(why),
                Err(_) => shards_api::Refusal::Allocation {
                    what: "native helper refusal",
                },
            },
        }
    }
}

fn api_io(
    operation: protocol::Operation,
    code: Option<i32>,
    kind: std::io::ErrorKind,
) -> shards_api::Refusal {
    shards_api::Refusal::ManagedIo {
        operation: match operation {
            protocol::Operation::Prepare => "helper prepare",
            protocol::Operation::Resume => "helper resume",
            protocol::Operation::Observe => "helper observe",
            protocol::Operation::Stop => "helper stop",
            protocol::Operation::Finalize => "helper finalize",
            protocol::Operation::Notify => "helper notification",
            protocol::Operation::Connect => "helper connection",
        },
        code,
        kind,
    }
}

pub(super) fn diagnostic(error: impl std::fmt::Display) -> Result<String, Refusal> {
    struct Text {
        bytes: String,
        needed: usize,
    }
    impl std::fmt::Write for Text {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.needed = self
                .bytes
                .len()
                .checked_add(text.len())
                .filter(|size| *size <= hex_core::helper::ARG_MAX)
                .ok_or(std::fmt::Error)?;
            self.bytes
                .try_reserve_exact(text.len())
                .map_err(|_| std::fmt::Error)?;
            self.bytes.push_str(text);
            Ok(())
        }
    }
    let mut text = Text {
        bytes: String::new(),
        needed: 0,
    };
    std::fmt::write(&mut text, format_args!("{error}"))
        .map_err(|_| native_allocation(text.needed))?;
    Ok(text.bytes)
}

/// Explicit same-binary/private-state startup configuration. Construction performs no I/O.
/// rpc_budget is the node's configured control patience, never an inferred polling period.
#[derive(Debug)]
pub struct ManagedHelper {
    binary: PathBuf,
    endpoint: PathBuf,
    receipt: PathBuf,
    rpc_budget: Duration,
}
impl ManagedHelper {
    pub fn new(
        binary: PathBuf,
        endpoint: PathBuf,
        receipt: PathBuf,
        rpc_budget: Duration,
    ) -> Result<Self, Refusal> {
        if rpc_budget.is_zero()
            || rpc_budget == Duration::MAX
            || u64::try_from(rpc_budget.as_nanos()).is_err()
        {
            return Err(super::configuration(
                "helper control patience must be positive, finite and representable",
            ));
        }
        for path in [&binary, &endpoint, &receipt] {
            if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
                return Err(super::configuration(
                    "helper paths must be explicit absolute native paths",
                ));
            }
        }
        if binary == endpoint || binary == receipt || endpoint == receipt {
            return Err(super::configuration(
                "helper executable, endpoint and receipt must be distinct",
            ));
        }
        SocketAddrUnix::new(&endpoint)
            .map_err(|error| Refusal::os("checking the private helper endpoint", error))?;
        Ok(Self {
            binary,
            endpoint,
            receipt,
            rpc_budget,
        })
    }
    pub fn binary(&self) -> &Path {
        &self.binary
    }
    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
    pub fn receipt(&self) -> &Path {
        &self.receipt
    }
    pub fn rpc_budget(&self) -> Duration {
        self.rpc_budget
    }
}

enum HelperProcess {
    Idle,
    Starting,
    Submitted(crate::macos::proc::BoxProc),
    Live {
        process: ProcId,
        observer: Option<crate::macos::proc::BoxProc>,
    },
    Observing(ProcId),
    Completed(ProcId),
}
impl HelperProcess {
    fn occupied(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

pub(in crate::macos) struct Helper {
    config: ManagedHelper,
    cpu: protocol::CpuControl,
    rates: hex_core::watch::Rates,
    lateness: Duration,
    /// Same helper incarnation retained by every proxy. It cannot silently rotate under a scope.
    process: Mutex<HelperProcess>,
    books: OnceLock<std::sync::Weak<dyn crate::devices::Books>>,
    next: AtomicU64,
}

impl Helper {
    pub(in crate::macos) fn bind_books(
        &self,
        books: std::sync::Weak<dyn crate::devices::Books>,
    ) -> Result<(), Refusal> {
        if books.upgrade().is_none() {
            return Err(super::configuration(
                "native helper authority is already gone",
            ));
        }
        if let Some(known) = self.books.get() {
            return if std::sync::Weak::ptr_eq(known, &books) {
                Ok(())
            } else {
                Err(super::configuration(
                    "native helper authority cannot be rebound",
                ))
            };
        }
        self.books
            .set(books)
            .map_err(|_| super::configuration("native helper authority binding raced"))
    }
    fn restart_fence(&self, completed: &protocol::completed::Snapshot) -> Result<(), Refusal> {
        // No helper-global mutex is held while querying the SAME Store's short read-only seams.
        let books = self
            .books
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| super::configuration("native helper has no live canonical authority"))?;
        let records = books.records()?;
        let pools = books.pool_records()?;
        check_restart(completed, &records, &pools)
    }
    fn start(
        &self,
        until: Instant,
        host_memory: Option<u64>,
    ) -> Result<(ProcId, OwnedFd), Refusal> {
        if self
            .books
            .get()
            .and_then(std::sync::Weak::upgrade)
            .is_none()
        {
            return Err(super::configuration(
                "bind the canonical native owner before helper startup",
            ));
        }
        let prepared = crate::macos::proc::Prepared::new()?;
        let memory = host_memory.filter(|memory| *memory != 0).ok_or_else(|| {
            super::configuration("helper startup requires the same admitted host memory envelope")
        })?;
        let cap = sys::jetsam_cap_mib(memory).ok_or_else(|| {
            super::configuration("the admitted helper envelope is not natively representable")
        })?;
        let (parent, child) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::empty(),
            None,
        )
        .map_err(|error| Refusal::os("preparing the private helper startup channel", error))?;
        for end in [&parent, &child] {
            super::super::private_descriptor(end, true).map_err(|error| {
                Refusal::os("preparing the private helper startup channel", error)
            })?;
        }
        rustix::net::sockopt::set_socket_nosigpipe(&parent, true)
            .map_err(|error| Refusal::os("preparing helper startup disconnect", error))?;
        let mut argv = Vec::new();
        // argv0 and the branch, then three path options, the cost option and five numbers, each
        // with its value.
        const START_WORDS: usize = 2 + 3 * 2 + 2 + 5 * 2;
        argv.try_reserve_exact(START_WORDS)
            .map_err(|_| native_allocation(START_WORDS))?;
        let cstring = |raw: &[u8]| -> Result<std::ffi::CString, Refusal> {
            let count = raw
                .len()
                .checked_add(1)
                .ok_or_else(|| native_allocation(usize::MAX))?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(count)
                .map_err(|_| native_allocation(count))?;
            bytes.extend_from_slice(raw);
            bytes.push(0);
            std::ffi::CString::from_vec_with_nul(bytes)
                .map_err(|_| super::configuration("helper startup argument contains NUL"))
        };
        let mut push = |value: &[u8]| -> Result<(), Refusal> {
            if argv.len() >= START_WORDS {
                return Err(native_allocation(START_WORDS));
            }
            argv.push(cstring(value)?);
            Ok(())
        };
        push(self.config.binary.as_os_str().as_bytes())?;
        push(b"native-managed-helper")?;
        for (name, path) in [
            (b"--binary".as_slice(), &self.config.binary),
            (b"--endpoint".as_slice(), &self.config.endpoint),
            (b"--receipt".as_slice(), &self.config.receipt),
        ] {
            push(name)?;
            push(path.as_os_str().as_bytes())?;
        }
        let mut cost = String::new();
        cost.try_reserve_exact(64)
            .map_err(|_| native_allocation(64))?;
        use std::fmt::Write as _;
        for byte in self.cpu.cost_id {
            write!(&mut cost, "{byte:02x}").map_err(|_| native_allocation(64))?;
        }
        push(b"--cpu-cost")?;
        push(cost.as_bytes())?;
        for (name, value) in [
            (
                b"--rpc-nanos".as_slice(),
                u64::try_from(self.config.rpc_budget.as_nanos())
                    .map_err(|_| super::configuration("helper budget width"))?,
            ),
            (
                b"--cpu-response-nanos".as_slice(),
                u64::try_from(self.cpu.response.as_nanos())
                    .map_err(|_| super::configuration("helper CPU response width"))?,
            ),
            (b"--rate-bytes".as_slice(), self.rates.bytes.get()),
            (b"--rate-processes".as_slice(), self.rates.processes.get()),
            (
                b"--lateness-nanos".as_slice(),
                u64::try_from(self.lateness.as_nanos())
                    .map_err(|_| super::configuration("helper lateness width"))?,
            ),
        ] {
            let mut text = String::new();
            text.try_reserve_exact("18446744073709551615".len())
                .map_err(|_| native_allocation(20))?;
            write!(&mut text, "{value}").map_err(|_| native_allocation(20))?;
            push(name)?;
            push(text.as_bytes())?;
        }
        drop(push);
        let bytes = argv
            .iter()
            .try_fold(0usize, |sum, word| {
                sum.checked_add(word.as_bytes_with_nul().len())
            })
            .and_then(|sum| {
                argv.len()
                    .checked_add(2)?
                    .checked_mul(size_of::<*const std::ffi::c_char>())?
                    .checked_add(sum)
            })
            .ok_or_else(|| native_allocation(usize::MAX))?;
        if bytes > crate::macos::exec_limit()?.block {
            return Err(super::configuration(
                "helper startup arguments exceed the actual native exec bound",
            ));
        }
        let mut paths = Vec::new();
        paths
            .try_reserve_exact(1)
            .map_err(|_| native_allocation(1))?;
        paths.push(cstring(self.config.binary.as_os_str().as_bytes())?);
        let spawn = sys::Spawn {
            paths,
            argv,
            envp: Vec::new(),
            cwd: None,
            stdio: [
                crate::macos::null(rustix::fs::OFlags::RDONLY)?,
                child,
                crate::macos::null(rustix::fs::OFlags::WRONLY)?,
            ],
            report: None,
            lifeline: None,
            jetsam_mib: Some(cap),
        };
        if Instant::now() >= until {
            return Err(super::configuration(
                "helper startup expired before native submission",
            ));
        }
        let pid = sys::spawn(&spawn, 0, sys::Clamp::Unclamped)
            .map_err(|error| Refusal::os("starting the same native helper binary", error))?;
        let (observer, registered) =
            prepared.bind(pid, None, crate::macos::proc::Reaping::Retained);
        *self
            .process
            .lock()
            .map_err(|_| super::configuration("native helper receipt lock poisoned"))? =
            HelperProcess::Submitted(observer);
        registered
            .map_err(|error| Refusal::os("registering same native helper completion", error))?;
        drop(spawn);
        // Startup reply is an event on this exact inherited socket, not a socket-path/process probe.
        // No VMM has been requested yet. Failure retains the canonical Creating record and receipt.
        let (bytes, fds) = frame::receive(parent.as_fd(), protocol::MAX_MESSAGE, Some(until))
            .map_err(frame::Failure::refusal)?;
        if let Ok((version, id, rejected)) = protocol::rejected::read(&bytes, fds.len()) {
            if version == VERSION && id == 0 {
                return Err(CallFailure::Rejected(rejected).refusal());
            }
            return Err(super::configuration("foreign helper startup diagnostic"));
        }
        let (version, id, ready) =
            protocol::census::read_answer(&bytes, fds.len(), crate::MAX_BOXES)
                .map_err(|_| super::configuration("helper startup reply is invalid"))?;
        if version != VERSION
            || id != 0
            || !ready.owners.is_empty()
            || u32::try_from(pid).ok() != Some(ready.helper.pid)
        {
            return Err(super::configuration(
                "the helper startup reply names another incarnation",
            ));
        }
        peer(&parent, ready.helper)?;
        Ok((ready.helper, parent))
    }
    pub(in crate::macos) fn new(
        config: ManagedHelper,
        cpu: protocol::CpuControl,
        rates: hex_core::watch::Rates,
        lateness: Duration,
    ) -> Result<Arc<Self>, Refusal> {
        cpu.check()
            .map_err(|_| super::configuration("helper CPU profile must be frozen and finite"))?;
        Arc::try_new(Self {
            config,
            cpu,
            rates,
            lateness,
            process: Mutex::new(HelperProcess::Idle),
            books: OnceLock::new(),
            next: AtomicU64::new(1),
        })
        .map_err(|_| native_allocation(size_of::<Self>()))
    }
    fn until(&self, deadline: Option<Instant>) -> Result<Instant, Refusal> {
        let own = Instant::now()
            .checked_add(self.config.rpc_budget)
            .ok_or_else(|| super::configuration("helper deadline is not representable"))?;
        let until = deadline.map_or(own, |deadline| own.min(deadline));
        if until <= Instant::now() {
            return Err(super::configuration(
                "the native helper request budget has expired",
            ));
        }
        Ok(until)
    }
    fn next(&self) -> Result<u64, Refusal> {
        self.next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| super::configuration("helper request identity is exhausted"))
    }
    fn finish_helper(&self, process: ProcId, until: Instant) -> Result<(), Refusal> {
        let mut observer = {
            let mut state = self
                .process
                .lock()
                .map_err(|_| super::configuration("helper native receipt lock poisoned"))?;
            let previous = std::mem::replace(&mut *state, HelperProcess::Observing(process));
            match previous {
                HelperProcess::Idle => None,
                HelperProcess::Submitted(observer)
                    if u32::try_from(observer.pid_i32()).ok() == Some(process.pid) =>
                {
                    Some(observer)
                }
                HelperProcess::Live {
                    process: known,
                    observer,
                } if known == process => observer,
                HelperProcess::Completed(known) if known == process => None,
                other => {
                    *state = other;
                    return Err(super::configuration(
                        "helper completion is already observed or names another lifetime",
                    ));
                }
            }
        };
        // The local slot now explicitly says Observing; no global mutex is held across native I/O.
        let result = (|| {
            let pid = i32::try_from(process.pid)
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| super::configuration("helper terminal PID width"))?;
            loop {
                let actual = sys::rusage(pid).map_err(|error| {
                    Refusal::os("observing exact helper terminal lifetime", error)
                })?;
                if actual.is_none_or(|actual| {
                    actual.started != 0 && actual.started != process.start_ticks
                }) {
                    return Ok(());
                }
                if observer.is_none() {
                    let prepared = crate::macos::proc::Prepared::new()?;
                    let (borrowed, registered) = prepared.bind(
                        pid,
                        None,
                        crate::macos::proc::Reaping::Borrowed {
                            started: process.start_ticks,
                        },
                    );
                    observer = Some(borrowed);
                    registered.map_err(|error| {
                        Refusal::os("subscribing to exact helper termination", error)
                    })?;
                }
                let native = observer
                    .as_mut()
                    .ok_or_else(|| super::configuration("helper terminal receipt absent"))?;
                if native
                    .native_poll()
                    .map_err(|error| native_failure("observing exact helper", error))?
                    .is_some()
                {
                    // Retained direct children are reaped by their sole original receipt. A cold
                    // Borrowed observer never reaps; a zombie remains held for an external retry.
                    native
                        .finalize_retained()
                        .map_err(|error| native_failure("reaping exact helper", error))?;
                    match sys::rusage(pid)
                        .map_err(|error| Refusal::os("verifying reaped helper absence", error))?
                    {
                        None => return Ok(()),
                        Some(actual)
                            if actual.started != 0 && actual.started != process.start_ticks =>
                        {
                            return Ok(());
                        }
                        _ => {
                            return Err(super::configuration(
                                "helper exit observed but native absence/reap is not yet proven",
                            ));
                        }
                    }
                }
                let remaining = until
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or_else(|| {
                        super::configuration(
                            "helper termination exceeded original finalization patience",
                        )
                    })?;
                if !native
                    .wait_scope(&[], remaining)
                    .map_err(|error| native_failure("waiting for exact helper", error))?
                {
                    return Err(super::configuration(
                        "helper termination exceeded original finalization patience",
                    ));
                }
            }
        })();
        let mut state = self
            .process
            .lock()
            .map_err(|_| super::configuration("helper native completion lock poisoned"))?;
        if !matches!(&*state,HelperProcess::Observing(known) if *known==process) {
            return Err(super::configuration("helper completion owner changed"));
        }
        *state = if result.is_ok() {
            HelperProcess::Completed(process)
        } else {
            HelperProcess::Live { process, observer }
        };
        result
    }
    fn reset_completed(&self) -> Result<(), Refusal> {
        let mut state = self
            .process
            .lock()
            .map_err(|_| super::configuration("helper completion reset lock poisoned"))?;
        if !matches!(*state, HelperProcess::Completed(_)) {
            return Err(super::configuration(
                "helper reset has no positive terminal receipt",
            ));
        }
        *state = HelperProcess::Idle;
        Ok(())
    }
    fn remember(&self, process: ProcId) -> Result<(), Refusal> {
        check_process(process)?;
        let mut known = self
            .process
            .lock()
            .map_err(|_| super::configuration("native helper incarnation lock poisoned"))?;
        match &*known {
            HelperProcess::Idle => {}
            HelperProcess::Submitted(observer)
                if u32::try_from(observer.pid_i32()).ok() == Some(process.pid) => {}
            HelperProcess::Live {
                process: expected, ..
            } if *expected == process => return Ok(()),
            _ => {
                return Err(super::configuration(
                    "native helper incarnation changed under existing owner",
                ));
            }
        }
        let observer = match std::mem::replace(&mut *known, HelperProcess::Idle) {
            HelperProcess::Submitted(observer) => Some(observer),
            _ => None,
        };
        *known = HelperProcess::Live { process, observer };
        Ok(())
    }
    fn identity(
        &self,
        allow_start: bool,
        until: Instant,
        host_memory: Option<u64>,
    ) -> Result<(ProcId, Option<OwnedFd>), Refusal> {
        // Disk/Store observations precede any short helper-state turn. The native Starting state
        // refuses concurrent starts; no helper-global mutex spans I/O or another Books turn.
        if let Some(snapshot) = receipt::read(self)? {
            if let Some(completed) = &snapshot.completed {
                if !allow_start {
                    return Err(super::configuration("native helper is completed"));
                }
                self.restart_fence(completed)?;
                self.finish_helper(snapshot.process, until)?;
                snapshot.retire(self)?;
                self.reset_completed()?;
            } else {
                snapshot.check_live()?;
                self.remember(snapshot.process)?;
                return Ok((snapshot.process, None));
            }
        }
        if !allow_start {
            return Err(super::configuration("no live native helper receipt"));
        }
        {
            let mut state = self
                .process
                .lock()
                .map_err(|_| super::configuration("helper start ownership lock poisoned"))?;
            if state.occupied() {
                return Err(super::configuration(
                    "native helper startup is already owned",
                ));
            }
            *state = HelperProcess::Starting;
        }
        let result = (|| {
            match std::fs::symlink_metadata(&self.config.endpoint) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Refusal::os("checking helper endpoint collision", error)),
                Ok(_) => {
                    return Err(super::configuration(
                        "private helper endpoint already occupied",
                    ));
                }
            }
            let (process, connection) = self.start(until, host_memory)?;
            self.remember(process)?;
            if receipt::read(self)?
                .is_none_or(|snapshot| snapshot.process != process || snapshot.completed.is_some())
            {
                return Err(super::configuration(
                    "started helper failed to publish exact incarnation",
                ));
            }
            Ok((process, Some(connection)))
        })();
        if result.is_err() {
            let mut state = self
                .process
                .lock()
                .map_err(|_| super::configuration("helper startup recovery lock poisoned"))?;
            if matches!(*state, HelperProcess::Starting) {
                *state = HelperProcess::Idle;
            }
        }
        result
    }
    fn connect(&self, process: ProcId, until: Instant) -> Result<OwnedFd, CallFailure> {
        check_process(process)?;
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|error| CallFailure::io(protocol::Operation::Connect, error))?;
        super::super::private_descriptor(&fd, true)
            .map_err(|error| CallFailure::io(protocol::Operation::Connect, error))?;
        rustix::net::sockopt::set_socket_nosigpipe(&fd, true)
            .map_err(|error| CallFailure::io(protocol::Operation::Connect, error))?;
        let address = SocketAddrUnix::new(&self.config.endpoint)
            .map_err(|error| Refusal::os("naming the native helper", error))?;
        match rustix::net::connect(&fd, &address) {
            Ok(()) => {}
            Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::AGAIN) => {
                frame::wait(fd.as_fd(), PollFlags::OUT, Some(until))
                    .map_err(|error| CallFailure::framed(protocol::Operation::Connect, error))?;
                rustix::net::sockopt::socket_error(&fd)
                    .map_err(|error| CallFailure::io(protocol::Operation::Connect, error))?
                    .map_err(|error| CallFailure::io(protocol::Operation::Connect, error))?;
            }
            Err(error) => {
                return Err(CallFailure::io(protocol::Operation::Connect, error));
            }
        }
        peer(&fd, process)?;
        Ok(fd)
    }
    fn rpc(
        &self,
        request: &protocol::Request,
        descriptors: &[BorrowedFd<'_>],
        deadline: Option<Instant>,
    ) -> Result<(protocol::Answer, Vec<OwnedFd>), CallFailure> {
        self.rpc_on(request, descriptors, deadline, None)
    }
    fn rpc_on(
        &self,
        request: &protocol::Request,
        descriptors: &[BorrowedFd<'_>],
        deadline: Option<Instant>,
        connection: Option<BorrowedFd<'_>>,
    ) -> Result<(protocol::Answer, Vec<OwnedFd>), CallFailure> {
        let until = self.until(deadline)?;
        let id = self.next()?;
        let expires = expiry(until)?;
        let bytes = protocol::timed::encode(VERSION, id, expires, request).map_err(|_| {
            super::configuration("the native helper request is invalid or cannot be reserved")
        })?;
        let owned = match connection {
            Some(_) => None,
            None => Some(self.connect(request.key().helper, until)?),
        };
        let fd = connection
            .or_else(|| owned.as_ref().map(AsFd::as_fd))
            .ok_or_else(|| super::configuration("helper connection ownership absent"))?;
        peer(fd, request.key().helper)?;
        frame::send(
            fd.as_fd(),
            &bytes,
            descriptors,
            protocol::timed::MAX_MESSAGE,
            Some(until),
        )
        .map_err(|error| CallFailure::framed(protocol::Operation::Connect, error))?;
        let (bytes, descriptors) =
            frame::receive(fd.as_fd(), protocol::MAX_MESSAGE, Some(until))
                .map_err(|error| CallFailure::framed(protocol::Operation::Connect, error))?;
        peer_after_reply(fd, request.key().helper)?;
        if let Ok((version, answer_id, rejected)) =
            protocol::rejected::read(&bytes, descriptors.len())
        {
            if version != VERSION || (answer_id != id && answer_id != 0) {
                return Err(
                    super::configuration("unmatched native helper diagnostic reply").into(),
                );
            }
            return Err(CallFailure::Rejected(rejected));
        }
        let (version, answer_id, answer) = protocol::read_answer(&bytes, descriptors.len())
            .map_err(|_| super::configuration("the native helper reply is invalid"))?;
        if version != VERSION || id != answer_id || request.key() != answer.key() {
            return Err(super::configuration(
                "the native helper reply belongs to another request or incarnation",
            )
            .into());
        }
        if let protocol::Answer::Refused { failure, .. } = &answer {
            return Err(CallFailure::Native(*failure));
        }
        Ok((answer, descriptors))
    }
    /// Joint plan step 14 (C4 on macOS): sets `key`'s scope to `rate` milli-CPU, its water-filled share.
    fn set_rate(&self, key: protocol::Key, rate: u64) -> Result<(), CallFailure> {
        let until = self.until(None)?;
        let id = self.next()?;
        let bytes = protocol::rate::encode(VERSION, id, &key, rate)
            .map_err(|_| super::configuration("the native rate request is invalid"))?;
        let fd = self.connect(key.helper, until)?;
        peer(&fd, key.helper)?;
        frame::send(
            fd.as_fd(),
            &bytes,
            &[],
            protocol::rate::MAX_MESSAGE,
            Some(until),
        )
        .map_err(|error| CallFailure::framed(protocol::Operation::Connect, error))?;
        let (bytes, descriptors) =
            frame::receive(fd.as_fd(), protocol::MAX_MESSAGE, Some(until))
                .map_err(|error| CallFailure::framed(protocol::Operation::Connect, error))?;
        peer_after_reply(&fd, key.helper)?;
        if let Ok((version, answer_id, rejected)) =
            protocol::rejected::read(&bytes, descriptors.len())
        {
            if version != VERSION || (answer_id != id && answer_id != 0) {
                return Err(
                    super::configuration("unmatched native helper diagnostic reply").into(),
                );
            }
            return Err(CallFailure::Rejected(rejected));
        }
        let (version, answer_id, answer) = protocol::read_answer(&bytes, descriptors.len())
            .map_err(|_| super::configuration("the native helper reply is invalid"))?;
        if version != VERSION || id != answer_id || &key != answer.key() {
            return Err(super::configuration(
                "the native helper reply belongs to another request or incarnation",
            )
            .into());
        }
        match answer {
            protocol::Answer::Done { .. } => Ok(()),
            protocol::Answer::Refused { failure, .. } => Err(CallFailure::Native(failure)),
            _ => Err(
                super::configuration("the native helper answered a rate with another reply").into(),
            ),
        }
    }
    pub(in crate::macos) fn census(&self) -> Result<protocol::census::Census, Refusal> {
        let until = self.until(None)?;
        let (process, _) = self.identity(false, until, None)?;
        let id = self.next()?;
        let request = protocol::census::encode_request(VERSION, id, process)
            .map_err(|_| super::configuration("invalid native census request"))?;
        let fd = self.connect(process, until).map_err(CallFailure::refusal)?;
        frame::send(
            fd.as_fd(),
            &request,
            &[],
            protocol::MAX_MESSAGE,
            Some(until),
        )
        .map_err(frame::Failure::refusal)?;
        let bound = protocol::census::message_bound(crate::MAX_BOXES)
            .map_err(|_| native_allocation(crate::MAX_BOXES))?;
        let (bytes, descriptors) =
            frame::receive(fd.as_fd(), bound, Some(until)).map_err(frame::Failure::refusal)?;
        peer_after_reply(&fd, process)?;
        let (version, got, census) =
            protocol::census::read_answer(&bytes, descriptors.len(), crate::MAX_BOXES)
                .map_err(|_| super::configuration("invalid native census reply"))?;
        if version != VERSION || got != id || census.helper != process {
            return Err(super::configuration("foreign native census reply"));
        }
        Ok(census)
    }

    pub(in crate::macos) fn census_if_present(
        &self,
    ) -> Result<Option<protocol::census::Census>, Refusal> {
        match receipt::read(self)? {
            Some(snapshot) if snapshot.completed.is_some() => Err(super::configuration(
                "native helper has completed receipts pending canonical retirement",
            )),
            Some(_) => self.census().map(Some),
            None => {
                if self
                    .process
                    .lock()
                    .map_err(|_| super::configuration("helper incarnation lock poisoned"))?
                    .occupied()
                {
                    return Err(super::configuration(
                        "an initialized helper lost its startup receipt",
                    ));
                }
                match std::fs::symlink_metadata(&self.config.endpoint) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(Refusal::os("checking retained helper endpoint", error)),
                    Ok(_) => Err(super::configuration(
                        "helper endpoint has no matching startup receipt",
                    )),
                }
            }
        }
    }
}

pub(super) fn native_failure(operation: &'static str, error: ProcessFailure) -> Refusal {
    match diagnostic(error) {
        Ok(why) => Refusal::Os { op: operation, why },
        Err(error) => error,
    }
}

fn check_completed_metadata(
    completed: &protocol::completed::Snapshot,
    records: &crate::devices::Records,
    pools: &[hex_core::record::PoolRecord],
) -> Result<(), Refusal> {
    completed
        .check(crate::MAX_BOXES)
        .map_err(|_| super::configuration("invalid completed native helper cohort"))?;
    for entry in &completed.owners {
        let current = match entry.key.owner.as_ref() {
            hex_core::record::OwnerRef::Box(id) => match records.get(id) {
                None => continue,
                Some(Ok(record)) => NativeRecord::Box(record),
                Some(Err(_)) => return Err(super::uncertain(id)),
            },
            hex_core::record::OwnerRef::Pool(pool) => {
                match pools.iter().find(|record| record.key.pool == *pool) {
                    None => continue,
                    Some(record) => NativeRecord::Pool(record),
                }
            }
        };
        if current.scope() != Some(entry.key.scope) {
            if current.init().is_none()
                && matches!(
                    current.stage(),
                    hex_core::record::NativeStage::Prepared
                        | hex_core::record::NativeStage::Creating
                )
            {
                continue;
            }
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
        if !entry.key.matches(current, completed.helper)?
            || current
                .init()
                .is_some_and(|init| Some(init) != entry.controller)
        {
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
        let exit = match current {
            NativeRecord::Box(record) => record.exit,
            NativeRecord::Pool(record) => record.exit,
        };
        if exit.is_some_and(|exit| exit != entry.status) {
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
    }
    Ok(())
}

fn check_restart(
    completed: &protocol::completed::Snapshot,
    records: &crate::devices::Records,
    pools: &[hex_core::record::PoolRecord],
) -> Result<(), Refusal> {
    completed
        .check(crate::MAX_BOXES)
        .map_err(|_| super::configuration("invalid completed helper cohort"))?;
    for entry in &completed.owners {
        let current = match entry.key.owner.as_ref() {
            hex_core::record::OwnerRef::Box(id) => match records.get(id) {
                None => continue,
                Some(Ok(record)) => NativeRecord::Box(record),
                Some(Err(_)) => return Err(super::uncertain(id)),
            },
            hex_core::record::OwnerRef::Pool(pool) => {
                match pools.iter().find(|record| record.key.pool == *pool) {
                    None => continue,
                    Some(record) => NativeRecord::Pool(record),
                }
            }
        };
        match current {
            NativeRecord::Box(record) => record.spec.check()?,
            NativeRecord::Pool(record) => record.check()?,
        }
        if current.scope() != Some(entry.key.scope) {
            // Store preserves an introduced scope in place. A new unsubmitted lifetime can appear
            // at this OwnerKey only after the old canonical lifetime was removed/released.
            if current.init().is_none()
                && matches!(
                    current.stage(),
                    hex_core::record::NativeStage::Prepared
                        | hex_core::record::NativeStage::Creating
                )
            {
                if current.class()? == hex_api::Class::Shards && current.scope().is_none() {
                    return Err(current.refusal(LifecycleCause::Uncertain));
                }
                continue;
            }
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
        if !entry.key.matches(current, completed.helper)?
            || current
                .init()
                .is_some_and(|init| Some(init) != entry.controller)
        {
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
        if current.stage() != hex_core::record::NativeStage::Released {
            return Err(current.refusal(LifecycleCause::Busy));
        }
        if let NativeRecord::Pool(record) = current
            && record.exit != Some(entry.status)
        {
            return Err(current.refusal(LifecycleCause::Uncertain));
        }
    }
    Ok(())
}

fn check_process(expected: ProcId) -> Result<(), Refusal> {
    let pid = i32::try_from(expected.pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| super::configuration("native helper PID is unrepresentable"))?;
    let actual = sys::rusage(pid)
        .map_err(|error| Refusal::os("reading the retained helper birth", error))?
        .filter(|actual| {
            !actual.ended && actual.started == expected.start_ticks && expected.start_ticks != 0
        })
        .ok_or_else(|| {
            super::configuration("the retained helper process is absent, ended or reused")
        })?;
    if actual.started != expected.start_ticks {
        return Err(super::configuration("the helper birth changed"));
    }
    Ok(())
}
/// The same peer check once a whole reply is read. The helper closes a connection when it has
/// answered (each request's, and its startup socket after the first Prepare), at any point after
/// its answer, so the peer's PID is read once: a second read let a close landing between the two
/// refuse a whole answer. A peer already gone then is the one checked before the request, not
/// another: its UID (kept from the connection) and its process, still the same live incarnation,
/// are checked as before. Every other failure still refuses.
fn peer_after_reply(fd: impl AsFd, expected: ProcId) -> Result<(), Refusal> {
    checked_peer(fd.as_fd(), expected, Peer::Answered)
}

fn peer(fd: impl AsFd, expected: ProcId) -> Result<(), Refusal> {
    checked_peer(fd.as_fd(), expected, Peer::Connected)
}

/// What a helper connection's peer may be when it is checked: connected before a request, or gone
/// once its whole answer is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Peer {
    Connected,
    Answered,
}

fn checked_peer(fd: BorrowedFd<'_>, expected: ProcId, peer: Peer) -> Result<(), Refusal> {
    check_process(expected)?;
    if sys::peer_euid(fd).map_err(|error| Refusal::os("reading native helper peer UID", error))?
        != rustix::process::geteuid().as_raw()
    {
        return Err(super::configuration(
            "the native helper belongs to another user",
        ));
    }
    match peer_pid(fd) {
        Ok(pid) => {
            if u32::try_from(pid).ok() != Some(expected.pid) {
                return Err(super::configuration(
                    "the native helper socket peer changed",
                ));
            }
        }
        Err(sys::PeerPidFailure::Native(error))
            if peer == Peer::Answered && error.raw_os_error() == Some(libc::ENOTCONN) => {}
        Err(failure) => {
            return Err(Refusal::Os {
                op: "reading the native helper peer PID",
                why: diagnostic(failure)?,
            });
        }
    }
    check_process(expected)
}

/// The node's read of a helper connection's peer PID (LOCAL_PEERPID). The controls' nemesis runs
/// around each read on the reading thread, to land the helper's close before or after it.
fn peer_pid(fd: BorrowedFd<'_>) -> Result<libc::pid_t, sys::PeerPidFailure> {
    #[cfg(test)]
    controls::around_peer_read(fd, controls::Read::Before);
    let read = sys::peer_pid(fd);
    #[cfg(test)]
    controls::around_peer_read(fd, controls::Read::After);
    read
}

// Native raw uptime is the SAME clock used by pinned Rust1.99.1 Instant on Apple.
// Capture native time before sender Instant remaining; receiver captures Instant before native
// time. Both sampling orders only shorten the original expiry, without cross-clock tolerance.
fn uptime() -> Result<Duration, Refusal> {
    match sys::uptime_raw() {
        Ok(value) => Ok(value),
        Err(sys::UptimeFailure::Native(error)) => {
            Err(Refusal::os("reading native helper raw uptime", error))
        }
        Err(error) => Err(Refusal::Os {
            op: "invalid native helper raw uptime",
            why: diagnostic(error)?,
        }),
    }
}
fn expiry(until: Instant) -> Result<Duration, Refusal> {
    let native = uptime()?;
    let remaining = until
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| super::configuration("the native helper request budget has expired"))?;
    native
        .checked_add(remaining)
        .ok_or_else(|| super::configuration("native raw uptime expiry overflow"))
}
fn deadline(expires: Duration, budget: Duration) -> Result<Instant, Refusal> {
    let began = Instant::now();
    let remaining = expires
        .checked_sub(uptime()?)
        .filter(|left| !left.is_zero())
        .ok_or_else(|| super::configuration("the transferred native request has expired"))?;
    began
        .checked_add(remaining.min(budget))
        .ok_or_else(|| super::configuration("native helper deadline overflow"))
}

pub(super) struct Proxy {
    helper: Arc<Helper>,
    key: protocol::Key,
    identity: OnceLock<Identity>,
    notification: Mutex<Option<OwnedFd>>,
    completed: OnceLock<ManagedEnd>,
}
impl Proxy {
    fn prepared(helper: Arc<Helper>, key: protocol::Key) -> Result<Arc<Self>, Refusal> {
        Arc::try_new(Self {
            helper,
            key,
            identity: OnceLock::new(),
            notification: Mutex::new(None),
            completed: OnceLock::new(),
        })
        .map_err(|_| native_allocation(size_of::<Self>()))
    }
    fn controller(&self) -> Option<ProcId> {
        self.identity.get().map(|id| ProcId {
            pid: id.pid,
            start_ticks: id.started,
        })
    }
    fn control(
        &self,
        action: protocol::Action,
        deadline: Option<Instant>,
    ) -> Result<(protocol::Answer, Vec<OwnedFd>), CallFailure> {
        self.helper.rpc(
            &protocol::Request::Control {
                key: self.key.try_clone()?,
                action,
                controller: self.controller(),
            },
            &[],
            deadline,
        )
    }
    fn attached(
        &self,
        answer: protocol::Answer,
        mut descriptors: Vec<OwnedFd>,
    ) -> Result<(), Refusal> {
        let protocol::Answer::Attached {
            controller, vsock, ..
        } = answer
        else {
            return Err(super::configuration(
                "the helper did not attach its native receipt",
            ));
        };
        let channel = descriptors
            .pop()
            .ok_or_else(|| super::configuration("the helper omitted its scope notification"))?;
        if !descriptors.is_empty() {
            return Err(super::configuration(
                "the helper sent extra scope descriptors",
            ));
        }
        if let Some(controller) = controller {
            let identity = Identity {
                pid: controller.pid,
                started: controller.start_ticks,
                vsock: PathBuf::from(std::ffi::OsString::from_vec(vsock)),
            };
            if let Some(known) = self.identity.get() {
                if known != &identity {
                    return Err(super::configuration(
                        "the attached controller incarnation changed",
                    ));
                }
            } else {
                self.identity
                    .set(identity)
                    .map_err(|_| super::configuration("the attached identity raced publication"))?;
            }
        } else if self.identity.get().is_some() {
            return Err(super::configuration("the attached controller disappeared"));
        }
        *self
            .notification
            .lock()
            .map_err(|_| super::configuration("proxy notification lock poisoned"))? = Some(channel);
        Ok(())
    }
    fn terminal_until(&self, until: Option<Instant>) -> Result<Option<ManagedEnd>, CallFailure> {
        if let Some(end) = self.completed.get() {
            return Ok(Some(*end));
        }
        let (answer, _) = match self.control(protocol::Action::Terminal, until) {
            Ok(answer) => answer,
            Err(original) => {
                let closed = receipt::read(&self.helper)?;
                if let Some(closed) = closed
                    && closed.process == self.key.helper
                    && let Some(entry) = closed.completion(&self.key, self.controller())?
                {
                    let end = ManagedEnd {
                        status: super::status(entry.status),
                        scope: TerminalScope::Released,
                    };
                    let known = self.completed.get_or_init(|| end);
                    if *known != end {
                        return Err(super::configuration("completed proxy result changed").into());
                    }
                    return Ok(Some(end));
                }
                return Err(original);
            }
        };
        let protocol::Answer::Terminal { end, .. } = answer else {
            return Err(super::configuration(
                "native helper returned a nonterminal answer to observation",
            )
            .into());
        };
        Ok(end.map(|end| {
            let (exit, scope) = match end {
                protocol::End::Process(exit) => (exit, TerminalScope::Process),
                protocol::End::Released(exit) => (exit, TerminalScope::Released),
            };
            ManagedEnd {
                status: super::status(exit),
                scope,
            }
        }))
    }
}
impl ManagedProcess for Proxy {
    fn identity(&self) -> Result<&Identity, shards_api::Refusal> {
        self.identity.get().ok_or(shards_api::Refusal::Scope(
            ProcessFailure::InvalidStartRecord,
        ))
    }
    fn terminal(&self) -> Result<Option<ManagedEnd>, shards_api::Refusal> {
        self.terminal_until(None).map_err(CallFailure::api)
    }
    fn stop(&self) -> Result<(), shards_api::Refusal> {
        if self.completed.get().is_some() {
            return Ok(());
        }
        let (answer, _) = self
            .control(protocol::Action::Stop, None)
            .map_err(CallFailure::api)?;
        if !matches!(answer, protocol::Answer::Done { .. }) {
            return Err(shards_api::Refusal::Scope(
                ProcessFailure::InvalidStartRecord,
            ));
        }
        Ok(())
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
        loop {
            if until.is_some_and(|until| Instant::now() >= until) {
                return Ok(None);
            }
            match self.terminal_until(until) {
                Ok(Some(end)) if end.scope == TerminalScope::Released => return Ok(Some(end)),
                Ok(_) => {}
                Err(error)
                    if error.timeout() && until.is_some_and(|until| Instant::now() >= until) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error.api()),
            }
            let notification = self.notification.lock().map_err(|_| {
                shards_api::Refusal::Scope(ProcessFailure::Poisoned {
                    what: "proxy notification",
                })
            })?;
            let channel = notification.as_ref().ok_or(shards_api::Refusal::Scope(
                ProcessFailure::InvalidStartRecord,
            ))?;
            // No RPC/connection turn is held during this event-only wait. Stop and short Terminal
            // observations can be served independently; notifications grant no terminal authority.
            match frame::wait(channel.as_fd(), PollFlags::IN, until) {
                Err(frame::Failure::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(CallFailure::framed(protocol::Operation::Notify, error).api());
                }
                Ok(()) => {}
            }
            // The notification has no identity/release authority. A stale byte causes another
            // exact-key query; EOF is terminal only if the SAME completed receipt proves it.
            let mut byte = [0u8; 1];
            match rustix::io::read(channel.as_fd(), &mut byte) {
                Ok(1) => {}
                Err(rustix::io::Errno::AGAIN) => {}
                Err(error) => return Err(CallFailure::io(protocol::Operation::Notify, error).api()),
                Ok(_) => {
                    return match self.terminal_until(until) {
                        Ok(Some(end)) if end.scope == TerminalScope::Released => Ok(Some(end)),
                        Err(error) => Err(error.api()),
                        _ => Err(CallFailure::io(
                            protocol::Operation::Notify,
                            std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                        )
                        .api()),
                    };
                }
            }
        }
    }
}

pub(super) fn launch(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    launch: &crate::devices::ManagedLaunch<'_>,
    io: shards_api::LaunchIo,
    patience: Duration,
    publish: &mut dyn FnMut(&Identity) -> Result<(), Refusal>,
) -> Result<ManagedReceipt, Refusal> {
    let crate::devices::ManagedLaunch::Shards(plan) = launch else {
        return Err(record.refusal(LifecycleCause::Uncertain));
    };
    match record {
        NativeRecord::Box(record) => record.spec.check()?,
        NativeRecord::Pool(record) => record.check()?,
    }
    super::host_bounds(record)?;
    if record.scope().is_none()
        || record.stage() != hex_core::record::NativeStage::Creating
        || record.init().is_some()
    {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let until = Instant::now()
        .checked_add(patience)
        .filter(|_| patience != Duration::MAX && !patience.is_zero())
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let request = plan.gpu_report_request();
    if request.is_some() != io.gpu_report.is_some()
        || request.is_some() != !record.devices().is_empty()
        || request.is_some_and(|request| {
            record.scope().map(|scope| *scope.bytes()) != Some(request.scope())
        })
    {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    let until = request.map_or(until, |request| until.min(request.deadline()));
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let launch = make_launch(record, plan, helper.cpu)?;
    let (process, startup) = helper.identity(
        true,
        until,
        record
            .allocation()
            .map(|allocation| allocation.resources.mem_bytes),
    )?;
    let proxy = Proxy::prepared(
        Arc::clone(helper),
        protocol::Key::from_record(record, process)?,
    )?;
    let shards_api::LaunchIo {
        stdout,
        stderr,
        gpu_report,
    } = io;
    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(protocol::MAX_DESCRIPTORS)
        .map_err(|_| native_allocation(protocol::MAX_DESCRIPTORS))?;
    descriptors.push(stdout.as_fd());
    descriptors.push(stderr.as_fd());
    if let Some(writer) = &gpu_report {
        descriptors.push(writer.as_fd());
    }
    let (answer, notification) = helper
        .rpc_on(
            &protocol::Request::Prepare {
                key: proxy.key.try_clone()?,
                launch,
            },
            &descriptors,
            Some(until),
            startup.as_ref().map(AsFd::as_fd),
        )
        .map_err(CallFailure::refusal)?;
    drop(startup);
    proxy.attached(answer, notification)?;
    drop(descriptors);
    drop(gpu_report);
    drop(stdout);
    drop(stderr);
    let identity = proxy
        .identity
        .get()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    if identity.vsock != plan.vsock() || until <= Instant::now() {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    publish(identity)?;
    let (answer, _) = proxy
        .control(protocol::Action::Resume, Some(until))
        .map_err(CallFailure::refusal)?;
    if !matches!(answer, protocol::Answer::Done { .. }) {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    Ok(shards_api::managed_receipt(proxy))
}

pub(super) fn attach(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    expected: Option<&Identity>,
) -> Result<Arc<Proxy>, Refusal> {
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    if let Some(snapshot) = receipt::read(helper)?
        && snapshot.completed.is_some()
    {
        let key = protocol::Key::from_record(record, snapshot.process)?;
        let expected_controller = expected
            .map(|identity| ProcId {
                pid: identity.pid,
                start_ticks: identity.started,
            })
            .or(record.init());
        let entry = snapshot.completion(&key, expected_controller)?;
        let proxy = Proxy::prepared(Arc::clone(helper), key)?;
        let status = match entry {
            Some(entry) => {
                if let Some(controller) = entry.controller {
                    let identity = Identity {
                        pid: controller.pid,
                        started: controller.start_ticks,
                        vsock: PathBuf::from(std::ffi::OsString::from_vec(copy_raw(&entry.vsock)?)),
                    };
                    if expected.is_some_and(|expected| expected != &identity) {
                        return Err(record.refusal(LifecycleCause::Uncertain));
                    }
                    proxy
                        .identity
                        .set(identity)
                        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
                }
                entry.status
            }
            None if record.init().is_none()
                && matches!(
                    record.stage(),
                    hex_core::record::NativeStage::Creating
                        | hex_core::record::NativeStage::Deleting
                ) =>
            {
                Exit::Unknown
            }
            _ => return Err(record.refusal(LifecycleCause::Uncertain)),
        };
        proxy
            .completed
            .set(ManagedEnd {
                status: super::status(status),
                scope: TerminalScope::Released,
            })
            .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
        return Ok(proxy);
    }
    let (process, _) = helper.identity(false, helper.until(None)?, None)?;
    let proxy = Proxy::prepared(
        Arc::clone(helper),
        protocol::Key::from_record(record, process)?,
    )?;
    let (answer, notification) = proxy
        .control(protocol::Action::Attach, None)
        .map_err(CallFailure::refusal)?;
    proxy.attached(answer, notification)?;
    if expected.is_some_and(|expected| proxy.identity.get() != Some(expected)) {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    Ok(proxy)
}

/// Joint plan step 14 (C4 on macOS): sets the canonical scope's CPU rate in its native helper.
pub(super) fn set_rate(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    rate: u64,
) -> Result<(), Refusal> {
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let (process, _) = helper.identity(false, helper.until(None)?, None)?;
    let key = protocol::Key::from_record(record, process)?;
    helper.set_rate(key, rate).map_err(CallFailure::refusal)
}

pub(super) fn usage(runtime: &MacRuntime, record: NativeRecord<'_>) -> Result<Usage, Refusal> {
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let (process, _) = helper.identity(false, helper.until(None)?, None)?;
    let key = protocol::Key::from_record(record, process)?;
    let (answer, _) = helper
        .rpc(
            &protocol::Request::Control {
                key,
                action: protocol::Action::Usage,
                controller: record.init(),
            },
            &[],
            None,
        )
        .map_err(CallFailure::refusal)?;
    match answer {
        protocol::Answer::Usage { usage, .. } => Ok(usage),
        _ => Err(record.refusal(LifecycleCause::Uncertain)),
    }
}
pub(super) fn finalize(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
    identity: Option<&Identity>,
) -> Result<(), Refusal> {
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let until = helper.until(None)?;
    let controller = identity
        .map(|identity| ProcId {
            pid: identity.pid,
            start_ticks: identity.started,
        })
        .or(record.init());
    // A retry after lost native reply uses the same durable completed receipt, without redispatch.
    if let Some(snapshot) = receipt::read(helper)?
        && snapshot.completed.is_some()
    {
        let key = protocol::Key::from_record(record, snapshot.process)?;
        let entry = snapshot.completion(&key, controller)?;
        if entry.is_none()
            && (record.init().is_some()
                || !matches!(
                    record.stage(),
                    hex_core::record::NativeStage::Creating
                        | hex_core::record::NativeStage::Deleting
                ))
        {
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
        helper.finish_helper(snapshot.process, until)?;
        return Ok(()); // Receipt retained until canonical Released/remove; new launch fences it.
    }
    let (process, _) = helper.identity(false, until, None)?;
    let key = protocol::Key::from_record(record, process)?;
    let result = helper.rpc(
        &protocol::Request::Control {
            key: key.try_clone()?,
            action: protocol::Action::Finalize,
            controller,
        },
        &[],
        Some(until),
    );
    if let Some(snapshot) = receipt::read(helper)?
        && snapshot.completed.is_some()
        && snapshot.process == process
    {
        snapshot
            .completion(&key, controller)?
            .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
        helper.finish_helper(process, until)?;
        return Ok(());
    }
    match result.map_err(CallFailure::refusal)?.0 {
        protocol::Answer::Done { .. } => Ok(()),
        _ => Err(record.refusal(LifecycleCause::Uncertain)),
    }
}

/// Only one part of source-Live/Draining +canonical scope+guest no-Intent proof. In particular,
/// UnknownOwner here is not a native Released result and never constructs a ManagedReceipt.
pub(in crate::macos) fn preflight_unsubmitted(
    runtime: &MacRuntime,
    record: &hex_core::record::BoxRecord,
) -> Result<(), Refusal> {
    let native = NativeRecord::Box(record);
    let helper = runtime
        .managed_helper
        .as_ref()
        .ok_or_else(|| native.refusal(LifecycleCause::Uncertain))?;
    if let Some(snapshot) = receipt::read(helper)?
        && let Some(completed) = &snapshot.completed
    {
        if completed
            .owners
            .iter()
            .any(|entry| entry.key.owner.as_ref() == native.owner())
        {
            return Err(native.refusal(LifecycleCause::Uncertain));
        }
        return Ok(()); // Positive completed cohort is still not permission to release this Box.
    }
    let Some(census) = helper.census_if_present()? else {
        return Ok(());
    };
    let key = protocol::Key::from_record(native, census.helper)?;
    // Inspect distinguishes a genuine absent native slot from a poisoned/query/identity failure;
    // unlike census it also catches retained Finalized entries and mismatched old incarnations.
    match helper.rpc(
        &protocol::Request::Control {
            key,
            action: protocol::Action::Inspect,
            controller: None,
        },
        &[],
        None,
    ) {
        Err(CallFailure::Native(protocol::Failure::UnknownOwner)) => Ok(()),
        Err(error) => Err(error.refusal()),
        Ok(_) => Err(native.refusal(LifecycleCause::Uncertain)),
    }
}

pub(in crate::macos) fn preflight(
    runtime: &MacRuntime,
    record: NativeRecord<'_>,
) -> Result<(), Refusal> {
    let bad = || record.refusal(LifecycleCause::Uncertain);
    let cost = match record {
        NativeRecord::Box(record) => record
            .spec
            .allocation
            .map(|allocation| allocation.cost)
            .ok_or_else(bad)?,
        NativeRecord::Pool(record) => record.cost,
    };
    runtime.check_vm_cost(&cost)?;
    let helper = runtime.managed_helper.as_ref().ok_or_else(bad)?;
    if let Some(snapshot) = receipt::read(helper)?
        && snapshot.completed.is_some()
    {
        let key = protocol::Key::from_record(record, snapshot.process)?;
        if snapshot.completion(&key, record.init())?.is_some() {
            return Ok(());
        }
        return if record.init().is_none()
            && matches!(
                record.stage(),
                hex_core::record::NativeStage::Prepared
                    | hex_core::record::NativeStage::Creating
                    | hex_core::record::NativeStage::Deleting
            ) {
            Ok(())
        } else {
            Err(bad())
        };
    }
    let Some(census) = helper.census_if_present()? else {
        return if matches!(
            record.stage(),
            hex_core::record::NativeStage::Prepared | hex_core::record::NativeStage::Released
        ) {
            Ok(())
        } else {
            Err(bad())
        };
    };
    let key = protocol::Key::from_record(record, census.helper)?;
    let present = census.owners.iter().any(|actual| actual == &key);
    if !present {
        return if matches!(
            record.stage(),
            hex_core::record::NativeStage::Prepared | hex_core::record::NativeStage::Released
        ) {
            Ok(())
        } else {
            Err(bad())
        };
    }
    let (answer, _) = helper
        .rpc(
            &protocol::Request::Control {
                key,
                action: protocol::Action::Inspect,
                controller: record.init(),
            },
            &[],
            None,
        )
        .map_err(CallFailure::refusal)?;
    let protocol::Answer::Metadata { controller, .. } = answer else {
        return Err(bad());
    };
    if record.init().is_some() && record.init() != controller {
        return Err(bad());
    }
    Ok(())
}

pub(in crate::macos) fn preflight_retained(
    runtime: &MacRuntime,
    records: &crate::devices::Records,
    pools: &[hex_core::record::PoolRecord],
) -> Result<(), Refusal> {
    let Some(helper) = &runtime.managed_helper else {
        return Ok(());
    };
    if let Some(snapshot) = receipt::read(helper)?
        && let Some(completed) = &snapshot.completed
    {
        return check_completed_metadata(completed, records, pools);
    }
    let Some(census) = helper.census_if_present()? else {
        return Ok(());
    };
    for key in &census.owners {
        let record = match key.owner.as_ref() {
            hex_core::record::OwnerRef::Box(id) => NativeRecord::Box(
                records
                    .get(id)
                    .and_then(|row| row.as_ref().ok())
                    .ok_or_else(|| super::uncertain(id))?,
            ),
            hex_core::record::OwnerRef::Pool(pool) => {
                let at = pools
                    .binary_search_by_key(pool, |row| row.key.pool)
                    .map_err(|_| {
                        super::configuration("native helper retains an unknown backing")
                    })?;
                NativeRecord::Pool(
                    pools
                        .get(at)
                        .ok_or_else(|| native_allocation(pools.len()))?,
                )
            }
        };
        if !key.matches(record, census.helper)? {
            return Err(record.refusal(LifecycleCause::Uncertain));
        }
    }
    Ok(())
}

fn make_launch(
    record: NativeRecord<'_>,
    plan: &shards_api::PreparedBoot,
    cpu: protocol::CpuControl,
) -> Result<protocol::Launch, Refusal> {
    let paths = plan
        .runtime_paths()
        .ok_or_else(|| record.refusal(LifecycleCause::Uncertain))?;
    let record_bytes = encode_native(record)?;
    let mut arguments = Vec::new();
    arguments
        .try_reserve_exact(plan.arguments().len())
        .map_err(|_| native_allocation(plan.arguments().len()))?;
    for word in plan.arguments() {
        arguments.push(copy_raw(word.as_os_str().as_bytes())?);
    }
    let mut environment = Vec::new();
    environment
        .try_reserve_exact(2)
        .map_err(|_| native_allocation(2))?;
    for (name, path) in [
        (b"SHARDS_CACHE".as_slice(), paths.cache()),
        (b"SHARDS_HOME".as_slice(), paths.home()),
    ] {
        environment.push(super::environment(name, path, record)?.into_bytes());
    }
    let launch = protocol::Launch {
        record_kind: match record {
            NativeRecord::Box(_) => protocol::RecordKind::Box,
            NativeRecord::Pool(_) => protocol::RecordKind::Pool,
        },
        record: record_bytes,
        binary: copy_raw(plan.binary().as_os_str().as_bytes())?,
        arguments,
        environment,
        vsock: copy_raw(plan.vsock().as_os_str().as_bytes())?,
        cpu,
        report_scope: plan.gpu_report_request().map(|request| request.scope()),
    };
    launch
        .check()
        .map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    Ok(launch)
}

fn encode_native(record: NativeRecord<'_>) -> Result<Vec<u8>, Refusal> {
    struct Json {
        bytes: Vec<u8>,
        size: usize,
    }
    impl Write for Json {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let size = self
                .size
                .checked_add(bytes.len())
                .filter(|size| *size <= hex_core::record::MAX_RECORD_BYTES)
                .ok_or(std::io::ErrorKind::InvalidInput)?;
            if !self.bytes.is_empty() || self.bytes.capacity() != 0 {
                if size >= self.bytes.capacity() {
                    return Err(std::io::ErrorKind::OutOfMemory.into());
                }
                self.bytes.extend_from_slice(bytes);
            }
            self.size = size;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let write = |out: &mut Json| match record {
        NativeRecord::Box(record) => serde_json::to_writer(out, record),
        NativeRecord::Pool(record) => serde_json::to_writer(out, record),
    };
    let mut count = Json {
        bytes: Vec::new(),
        size: 0,
    };
    write(&mut count).map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    let size = count
        .size
        .checked_add(1)
        .filter(|size| *size <= hex_core::record::MAX_RECORD_BYTES)
        .ok_or_else(|| native_allocation(hex_core::record::MAX_RECORD_BYTES))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| native_allocation(size))?;
    let mut output = Json { bytes, size: 0 };
    write(&mut output).map_err(|_| record.refusal(LifecycleCause::Uncertain))?;
    if output.size != count.size {
        return Err(record.refusal(LifecycleCause::Uncertain));
    }
    output.bytes.push(b'\n');
    Ok(output.bytes)
}

impl Helper {
    fn fingerprint(&self) -> Result<[u8; 32], Refusal> {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        for path in [
            &self.config.binary,
            &self.config.endpoint,
            &self.config.receipt,
        ] {
            let bytes = path.as_os_str().as_bytes();
            let length = u64::try_from(bytes.len()).map_err(|_| native_allocation(bytes.len()))?;
            hash.update(length.to_le_bytes());
            hash.update(bytes);
        }
        for value in [
            self.rates.bytes.get(),
            self.rates.processes.get(),
            u64::try_from(self.lateness.as_nanos())
                .map_err(|_| super::configuration("helper lateness width"))?,
            u64::try_from(self.config.rpc_budget.as_nanos())
                .map_err(|_| super::configuration("helper budget width"))?,
            u64::try_from(self.cpu.response.as_nanos())
                .map_err(|_| super::configuration("helper CPU response width"))?,
        ] {
            hash.update(value.to_le_bytes());
        }
        hash.update(self.cpu.cost_id);
        Ok(hash.finalize().into())
    }
}

pub fn run_managed_helper(
    config: ManagedHelper,
    cpu: protocol::CpuControl,
    rates: hex_core::watch::Rates,
    lateness: Duration,
) -> Result<(), Refusal> {
    server::run(config, cpu, rates, lateness)
}
