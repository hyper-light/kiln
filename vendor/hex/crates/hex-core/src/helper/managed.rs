//! Rootless managed native-owner messages appended to the existing HEXM framing/version.
//! The helper keeps native receipts only; canonical Store admission/physical claims remain in Node.
use std::time::Duration;

pub mod census;
pub mod completed;
pub mod rate;
pub mod rejected;
pub mod response;
pub mod timed;

use crate::record::{NativeRecord, NativeScope, OwnerKey, OwnerRef, ProcId};
use crate::wire::{Fault, Reader};
use hex_api::{Exit, IoKind, Refusal, Usage};

use super::{ARG_MAX, BoxId, FIRST, HEADER_BYTES, MAGIC, PATH_MAX, VERSION};

/// Frozen measured configuration, never a separately calculated CPU charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuControl {
    pub cost_id: [u8; 32],
    /// Kept for helpers older than goal 14's learning helper (main a8dc671c and before), which take it as their
    /// control's response; a learning helper reads its own, the slowest stop it has seen ([`response`]). The node sends
    /// [`COLD_RESPONSE`] (joint plan step 14, 2c; ROOT, 2026-11-13): wire compatibility for a persistent helper holding
    /// live VMs across a node upgrade, never a census number.
    pub response: Duration,
}

/// The cold response a node sends in [`CpuControl::response`]: the least the field holds that a helper's check takes,
/// a nanosecond (its unit), so an older helper reading it holds a scope at its credit's end with no burst to speak of.
pub const COLD_RESPONSE: Duration = Duration::from_nanos(1);
impl CpuControl {
    /// The control a node sends for `cost_id` (joint plan step 14, 2c): the cold response, which a learning helper
    /// ignores ([`COLD_RESPONSE`]).
    pub fn cold(cost_id: [u8; 32]) -> Self {
        Self {
            cost_id,
            response: COLD_RESPONSE,
        }
    }

    pub fn check(self) -> Result<(), Fault> {
        if self.cost_id == [0; 32]
            || self.response.is_zero()
            || u64::try_from(self.response.as_nanos()).is_err()
        {
            return Err(invalid("finite measured CPU control"));
        }
        Ok(())
    }
}

/// Same canonical owner and native lifetime, with the actual persistent helper's PID/birth.
#[derive(Debug, PartialEq, Eq)]
pub struct Key {
    pub owner: OwnerKey,
    pub scope: NativeScope,
    pub commitment: [u8; 32],
    pub helper: ProcId,
}
impl Key {
    pub fn from_record(record: NativeRecord<'_>, helper: ProcId) -> Result<Self, Refusal> {
        if helper.pid == 0 || helper.start_ticks == 0 {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        Ok(Self {
            owner: record.owner().try_owned()?,
            scope: record
                .scope()
                .ok_or_else(|| record.refusal(hex_api::LifecycleCause::Uncertain))?,
            commitment: record.commitment()?,
            helper,
        })
    }
    pub fn matches(&self, record: NativeRecord<'_>, helper: ProcId) -> Result<bool, Refusal> {
        Ok(self.owner.as_ref() == record.owner()
            && self.helper == helper
            && Some(self.scope) == record.scope()
            && self.commitment == record.commitment()?)
    }
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        Ok(Self {
            owner: self.owner.try_clone()?,
            scope: self.scope,
            commitment: self.commitment,
            helper: self.helper,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Box,
    Pool,
}

/// Native byte strings preserve non-Unicode Unix paths/argv. Only the actual prepared launch
/// enters this local protocol; no environment, binary or selector is picked by this codec.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub record_kind: RecordKind,
    pub record: Vec<u8>,
    pub binary: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub environment: Vec<Vec<u8>>,
    pub vsock: Vec<u8>,
    pub cpu: CpuControl,
    /// Pure scope request only. The helper renders the numeric argument from the writer it
    /// actually received, after SCM_RIGHTS transfer and before its held native launch.
    pub report_scope: Option<[u8; 16]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Resume,
    Attach,
    Stop,
    Terminal,
    Usage,
    Finalize,
    /// Read only the same receipt's immutable metadata; no notification/attach/native query.
    Inspect,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Prepare {
        key: Key,
        launch: Launch,
    },
    Control {
        key: Key,
        action: Action,
        controller: Option<ProcId>,
    },
}
impl Request {
    pub fn key(&self) -> &Key {
        match self {
            Self::Prepare { key, .. } | Self::Control { key, .. } => key,
        }
    }
    pub fn descriptors(&self) -> usize {
        match self {
            // Native stdin is NUL. The caller supplies stdout/stderr and the optional report writer.
            Self::Prepare { launch, .. } => {
                if launch.report_scope.is_some() {
                    3
                } else {
                    2
                }
            }
            Self::Control { .. } => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Process(Exit),
    Released(Exit),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Prepare,
    Resume,
    Observe,
    Stop,
    Finalize,
    Notify,
    Connect,
}

/// Native errors remain typed. No missing helper/socket/process becomes positive release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    UnknownOwner,
    Scope,
    Commitment,
    Controller,
    Phase,
    Control,
    Allocation {
        bytes: u64,
    },
    Io {
        operation: Operation,
        code: Option<i32>,
        kind: std::io::ErrorKind,
    },
    Poisoned,
    Uncertain,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Attached {
        key: Key,
        controller: Option<ProcId>,
        vsock: Vec<u8>,
    },
    Terminal {
        key: Key,
        end: Option<End>,
    },
    Usage {
        key: Key,
        usage: Usage,
    },
    Done {
        key: Key,
    },
    Refused {
        key: Key,
        failure: Failure,
    },
    Metadata {
        key: Key,
        controller: Option<ProcId>,
        vsock: Vec<u8>,
    },
}
impl Answer {
    pub fn key(&self) -> &Key {
        match self {
            Self::Attached { key, .. }
            | Self::Terminal { key, .. }
            | Self::Usage { key, .. }
            | Self::Done { key }
            | Self::Refused { key, .. } => key,
            Self::Metadata { key, .. } => key,
        }
    }
    pub fn descriptors(&self) -> usize {
        usize::from(matches!(self, Self::Attached { .. }))
    }
}

// All numbers derive from fixed field widths and existing record/exec/path protocol bounds.
const OWNER_BYTES: usize = 1 + if BoxId::MAX_LEN + 4 > 32 {
    BoxId::MAX_LEN + 4
} else {
    32
};
const PROC_BYTES: usize = 4 + 8;
const KEY_BYTES: usize = OWNER_BYTES + 16 + 32 + PROC_BYTES;
const CONTROL_BYTES: usize = 32 + 8 + 4;
const ENV_BYTES: usize = 4 + 2 * (4 + b"SHARDS_CACHE=".len() + PATH_MAX);
/// One native record and one prepared exec, without a second framing envelope.
pub const MAX_MESSAGE: usize = HEADER_BYTES
    + KEY_BYTES
    + 1
    + 4
    + crate::record::MAX_RECORD_BYTES
    + 4
    + PATH_MAX
    + 4
    + ARG_MAX
    + ENV_BYTES
    + 4
    + PATH_MAX
    + CONTROL_BYTES
    + 16
    + 1;
pub const MAX_DESCRIPTORS: usize = 3;

const PREPARE: u16 = 0x000a;
const RESUME: u16 = 0x000b;
const ATTACH: u16 = 0x000c;
const STOP: u16 = 0x000d;
const TERMINAL: u16 = 0x000e;
const USAGE: u16 = 0x000f;
const FINALIZE: u16 = 0x0010;
const A_ATTACHED: u16 = 0x800a;
const A_TERMINAL: u16 = 0x800b;
const A_USAGE: u16 = 0x800c;
const A_DONE: u16 = 0x800d;
const A_REFUSED: u16 = 0x800e;
const INSPECT: u16 = 0x0013;
const A_METADATA: u16 = 0x8010;

fn invalid(what: &'static str) -> Fault {
    Fault::Value {
        what,
        why: String::new(),
    }
}
fn bounded(bytes: usize) -> Fault {
    Fault::Oversized {
        bytes,
        bound: MAX_MESSAGE,
    }
}
fn copy(bytes: &[u8]) -> Result<Vec<u8>, Fault> {
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| Fault::Storage {
            what: "managed helper bytes",
            bound: bytes.len(),
        })?;
    out.extend_from_slice(bytes);
    Ok(out)
}
fn key_check(key: &Key) -> Result<(), Fault> {
    if key.helper.pid == 0 || key.helper.start_ticks == 0 {
        return Err(invalid("native helper identity"));
    }
    Ok(())
}
fn proc_check(process: Option<ProcId>) -> Result<(), Fault> {
    if process.is_some_and(|p| p.pid == 0 || p.start_ticks == 0) {
        return Err(invalid("native controller identity"));
    }
    Ok(())
}
impl Launch {
    pub fn check(&self) -> Result<(), Fault> {
        self.cpu.check()?;
        if self.record.len() > crate::record::MAX_RECORD_BYTES || !self.record.ends_with(b"\n") {
            return Err(invalid("canonical native record bytes"));
        }
        if self.binary.is_empty()
            || self.binary.first() != Some(&b'/')
            || self.binary.len() >= PATH_MAX
            || self.binary.contains(&0)
            || self.vsock.is_empty()
            || self.vsock.len() >= PATH_MAX
            || self.vsock.contains(&0)
        {
            return Err(invalid("prepared native path"));
        }
        let mut exec = self
            .binary
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_add(size_of::<usize>()))
            .ok_or_else(|| bounded(usize::MAX))?;
        for word in &self.arguments {
            if word.contains(&0) {
                return Err(invalid("prepared native argument"));
            }
            exec = exec
                .checked_add(word.len())
                .and_then(|n| n.checked_add(1))
                .and_then(|n| n.checked_add(size_of::<usize>()))
                .ok_or_else(|| bounded(usize::MAX))?;
        }
        check_prepared_arguments(self.arguments.iter().map(Vec::as_slice))?;
        let [cache, home] = self.environment.as_slice() else {
            return Err(invalid("configured native environment"));
        };
        if !cache.starts_with(b"SHARDS_CACHE=/") || !home.starts_with(b"SHARDS_HOME=/") {
            return Err(invalid("configured native runtime paths"));
        }
        let path_bound = b"SHARDS_CACHE="
            .len()
            .checked_add(PATH_MAX)
            .ok_or_else(|| bounded(usize::MAX))?;
        for value in &self.environment {
            if value.contains(&0) || value.len() > path_bound {
                return Err(invalid("configured native runtime path"));
            }
            exec = exec
                .checked_add(value.len())
                .and_then(|n| n.checked_add(1))
                .and_then(|n| n.checked_add(size_of::<usize>()))
                .ok_or_else(|| bounded(usize::MAX))?;
        }
        if exec > ARG_MAX {
            return Err(Fault::Oversized {
                bytes: exec,
                bound: ARG_MAX,
            });
        }
        Ok(())
    }
}

/// Only the option/value roles emitted by SDK PreparedBoot::arguments/prepare_boot. This does not
/// interpret the VM's values, paths, decimal selectors or guest command line. A literal flag or
/// `--` consumed as a value cannot hide a later option; native report/lifeline words are appended
/// only by the final launcher. The shell uses this same check, never a second raw-word scan.
pub fn check_prepared_arguments<'a>(
    words: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), Fault> {
    let mut words = words.into_iter();
    let mut exec = 0usize;
    // Public callers may supply a lazy iterator. Charge each consumed word (NUL plus exec pointer)
    // before scanning its bytes, so even an endless option stream stops at the existing ARG_MAX.
    let mut next = || -> Result<Option<&'a [u8]>, Fault> {
        let Some(word) = words.next() else {
            return Ok(None);
        };
        exec = exec
            .checked_add(word.len())
            .and_then(|size| size.checked_add(1))
            .and_then(|size| size.checked_add(size_of::<usize>()))
            .filter(|size| *size <= ARG_MAX)
            .ok_or(Fault::Oversized {
                bytes: usize::MAX,
                bound: ARG_MAX,
            })?;
        Ok(Some(word))
    };
    if next()? != Some(b"run".as_slice()) {
        return Err(invalid("prepared native action"));
    }
    let mut managed = false;
    while let Some(option) = next()? {
        match option {
            b"--kernel" | b"--init" | b"--initrd" | b"--cmdline" | b"--cpus" | b"--memory"
            | b"--disk" | b"--share" | b"--vsock" | b"--console-said" | b"--gpu-device" => {
                let value = next()?.ok_or_else(|| invalid("prepared native option value"))?;
                if value.contains(&0) {
                    return Err(invalid("prepared native option value"));
                }
            }
            b"--gpu" | b"--no-console" => {}
            b"--host-managed" if !managed => managed = true,
            // No positional/guest separator is emitted by the canonical direct boot producer.
            // A new SDK option needs an explicit role before this helper may execute it.
            _ => return Err(invalid("unprepared native option")),
        }
    }
    if !managed {
        return Err(invalid("prepared native management marker"));
    }
    Ok(())
}

trait Output {
    fn raw(&mut self, bytes: &[u8]) -> Result<(), Fault>;
}
struct Count {
    bytes: usize,
    maximum: usize,
}
impl Output for Count {
    fn raw(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.maximum)
            .ok_or(Fault::Oversized {
                bytes: usize::MAX,
                bound: self.maximum,
            })?;
        Ok(())
    }
}
struct Encoded {
    bytes: Vec<u8>,
    size: usize,
}
impl Output for Encoded {
    fn raw(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        let size = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= self.size)
            .ok_or_else(|| bounded(usize::MAX))?;
        if size > self.bytes.capacity() {
            return Err(invalid("reserved managed message"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}
fn encode(write: impl Fn(&mut dyn Output) -> Result<(), Fault>) -> Result<Vec<u8>, Fault> {
    encode_bounded(MAX_MESSAGE, write)
}
fn encode_bounded(
    maximum: usize,
    write: impl Fn(&mut dyn Output) -> Result<(), Fault>,
) -> Result<Vec<u8>, Fault> {
    let mut count = Count { bytes: 0, maximum };
    write(&mut count)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count.bytes)
        .map_err(|_| Fault::Storage {
            what: "managed helper message",
            bound: count.bytes,
        })?;
    let mut encoded = Encoded {
        bytes,
        size: count.bytes,
    };
    write(&mut encoded)?;
    if encoded.bytes.len() != count.bytes {
        return Err(invalid("managed message encoding size"));
    }
    Ok(encoded.bytes)
}
fn u8(out: &mut dyn Output, value: u8) -> Result<(), Fault> {
    out.raw(&[value])
}
fn u32(out: &mut dyn Output, value: u32) -> Result<(), Fault> {
    out.raw(&value.to_le_bytes())
}
fn u64(out: &mut dyn Output, value: u64) -> Result<(), Fault> {
    out.raw(&value.to_le_bytes())
}
fn blob(out: &mut dyn Output, value: &[u8]) -> Result<(), Fault> {
    u32(
        out,
        u32::try_from(value.len()).map_err(|_| bounded(value.len()))?,
    )?;
    out.raw(value)
}
fn header(out: &mut dyn Output, version: u16, kind: u16, id: u64) -> Result<(), Fault> {
    if version < FIRST || version > VERSION {
        return Err(Fault::Version {
            got: version,
            speaks: VERSION,
        });
    }
    out.raw(&MAGIC)?;
    out.raw(&version.to_le_bytes())?;
    out.raw(&kind.to_le_bytes())?;
    u64(out, id)
}
fn put_proc(out: &mut dyn Output, process: ProcId) -> Result<(), Fault> {
    proc_check(Some(process))?;
    u32(out, process.pid)?;
    u64(out, process.start_ticks)
}
fn put_optional_proc(out: &mut dyn Output, process: Option<ProcId>) -> Result<(), Fault> {
    match process {
        None => u8(out, 0),
        Some(process) => {
            u8(out, 1)?;
            put_proc(out, process)
        }
    }
}
fn put_key(out: &mut dyn Output, key: &Key) -> Result<(), Fault> {
    key_check(key)?;
    match key.owner.as_ref() {
        OwnerRef::Box(id) => {
            u8(out, 0)?;
            blob(out, id.as_str().as_bytes())?;
        }
        OwnerRef::Pool(pool) => {
            u8(out, 1)?;
            out.raw(pool)?;
        }
    }
    out.raw(key.scope.bytes())?;
    out.raw(&key.commitment)?;
    put_proc(out, key.helper)
}
fn put_words(out: &mut dyn Output, words: &[Vec<u8>]) -> Result<(), Fault> {
    u32(
        out,
        u32::try_from(words.len()).map_err(|_| bounded(usize::MAX))?,
    )?;
    for word in words {
        blob(out, word)?;
    }
    Ok(())
}
fn action_kind(action: Action) -> u16 {
    match action {
        Action::Resume => RESUME,
        Action::Attach => ATTACH,
        Action::Stop => STOP,
        Action::Terminal => TERMINAL,
        Action::Usage => USAGE,
        Action::Finalize => FINALIZE,
        Action::Inspect => INSPECT,
    }
}
/// Same HEXM header and framing. Count and fallibly reserve the complete new message before copy.
pub fn encode_request(version: u16, id: u64, request: &Request) -> Result<Vec<u8>, Fault> {
    check_request(request)?;
    encode(|out| put_request(out, version, id, request))
}
fn check_request(request: &Request) -> Result<(), Fault> {
    match request {
        Request::Prepare { key, launch } => {
            launch.check()?;
            if launch
                .report_scope
                .is_some_and(|scope| scope != *key.scope.bytes())
            {
                return Err(invalid("native report scope commitment"));
            }
        }
        Request::Control {
            action, controller, ..
        } => {
            proc_check(*controller)?;
            if *action == Action::Resume && controller.is_none() {
                return Err(invalid("resume controller"));
            }
        }
    }
    Ok(())
}
fn put_request(
    out: &mut dyn Output,
    version: u16,
    id: u64,
    request: &Request,
) -> Result<(), Fault> {
    header(
        out,
        version,
        match request {
            Request::Prepare { .. } => PREPARE,
            Request::Control { action, .. } => action_kind(*action),
        },
        id,
    )?;
    put_key(out, request.key())?;
    match request {
        Request::Prepare { launch, .. } => {
            u8(
                out,
                match launch.record_kind {
                    RecordKind::Box => 0,
                    RecordKind::Pool => 1,
                },
            )?;
            blob(out, &launch.record)?;
            blob(out, &launch.binary)?;
            put_words(out, &launch.arguments)?;
            put_words(out, &launch.environment)?;
            blob(out, &launch.vsock)?;
            out.raw(&launch.cpu.cost_id)?;
            u64(out, launch.cpu.response.as_secs())?;
            u32(out, launch.cpu.response.subsec_nanos())?;
            out.raw(&launch.report_scope.unwrap_or([0; 16]))?;
            u8(out, u8::from(launch.report_scope.is_some()))
        }
        Request::Control { controller, .. } => put_optional_proc(out, *controller),
    }
}
fn put_exit(out: &mut dyn Output, exit: Exit) -> Result<(), Fault> {
    match exit {
        Exit::Unknown => u8(out, 0),
        Exit::Code(code) => {
            u8(out, 1)?;
            out.raw(&code.to_le_bytes())
        }
        Exit::Signal(signal) => {
            u8(out, 2)?;
            out.raw(&signal.to_le_bytes())
        }
    }
}

// A kind's tag is its place in hex-api's `IoKind::NAMED`, the node wire's one append-only list,
// never Rust's enum discriminants. A kind it doesn't name refuses encoding rather than silently
// becoming Other; a native errno stays exact beside it.
fn put_io_kind(out: &mut dyn Output, kind: std::io::ErrorKind) -> Result<(), Fault> {
    let tag = IoKind::NAMED
        .iter()
        .position(|value| *value == kind)
        .and_then(|at| u8::try_from(at).ok())
        .ok_or_else(|| invalid("unrepresented helper I/O failure"))?;
    u8(out, tag)
}
fn read_io_kind(input: &mut Reader<'_>) -> Result<std::io::ErrorKind, Fault> {
    IoKind::NAMED
        .get(usize::from(input.u8()?))
        .copied()
        .ok_or_else(|| invalid("helper I/O failure kind"))
}
fn put_failure(out: &mut dyn Output, failure: Failure) -> Result<(), Fault> {
    u8(
        out,
        match failure {
            Failure::UnknownOwner => 0,
            Failure::Scope => 1,
            Failure::Commitment => 2,
            Failure::Controller => 3,
            Failure::Phase => 4,
            Failure::Control => 5,
            Failure::Allocation { .. } => 6,
            Failure::Io { .. } => 7,
            Failure::Poisoned => 8,
            Failure::Uncertain => 9,
        },
    )?;
    match failure {
        Failure::Allocation { bytes } => u64(out, bytes),
        Failure::Io {
            operation,
            code,
            kind,
        } => {
            u8(
                out,
                match operation {
                    Operation::Prepare => 0,
                    Operation::Resume => 1,
                    Operation::Observe => 2,
                    Operation::Stop => 3,
                    Operation::Finalize => 4,
                    Operation::Notify => 5,
                    Operation::Connect => 6,
                },
            )?;
            match code {
                None => {
                    u8(out, 0)?;
                    put_io_kind(out, kind)
                }
                Some(code) => {
                    if std::io::Error::from_raw_os_error(code).kind() != kind {
                        return Err(invalid("native helper errno kind"));
                    }
                    u8(out, 1)?;
                    out.raw(&code.to_le_bytes())
                }
            }
        }
        _ => Ok(()),
    }
}
/// A native scope's usage: its counters, read from its native owner. Guest time per vCPU is no native
/// counter of a scope, so a usage holding one is refused here rather than dropped (CLAUDE.md §3).
fn put_usage(out: &mut dyn Output, value: &Usage) -> Result<(), Fault> {
    if value.guest.is_some() {
        return Err(invalid("native scope usage with guest time"));
    }
    for field in [
        value.memory_bytes,
        value.memory_peak_bytes,
        value.oom_kills,
        value.pids,
        value.pids_refused,
        value.cpu_usec,
        value.io_read_bytes,
        value.io_write_bytes,
    ] {
        u64(out, field)?;
    }
    Ok(())
}
pub fn encode_answer(version: u16, id: u64, answer: &Answer) -> Result<Vec<u8>, Fault> {
    encode(|out| {
        header(
            out,
            version,
            match answer {
                Answer::Attached { .. } => A_ATTACHED,
                Answer::Terminal { .. } => A_TERMINAL,
                Answer::Usage { .. } => A_USAGE,
                Answer::Done { .. } => A_DONE,
                Answer::Refused { .. } => A_REFUSED,
                Answer::Metadata { .. } => A_METADATA,
            },
            id,
        )?;
        put_key(out, answer.key())?;
        match answer {
            Answer::Attached {
                controller, vsock, ..
            }
            | Answer::Metadata {
                controller, vsock, ..
            } => {
                if vsock.is_empty() || vsock.len() >= PATH_MAX || vsock.contains(&0) {
                    return Err(invalid("native vsock path"));
                }
                put_optional_proc(out, *controller)?;
                blob(out, vsock)
            }
            Answer::Terminal { end, .. } => match end {
                None => u8(out, 0),
                Some(End::Process(exit)) => {
                    u8(out, 1)?;
                    put_exit(out, *exit)
                }
                Some(End::Released(exit)) => {
                    u8(out, 2)?;
                    put_exit(out, *exit)
                }
            },
            Answer::Usage { usage, .. } => put_usage(out, usage),
            Answer::Done { .. } => Ok(()),
            Answer::Refused { failure, .. } => put_failure(out, *failure),
        }
    })
}
fn owned_blob(input: &mut Reader<'_>, bound: usize) -> Result<Vec<u8>, Fault> {
    let bytes = input.bytes()?;
    if bytes.len() > bound {
        return Err(Fault::Oversized {
            bytes: bytes.len(),
            bound,
        });
    }
    copy(bytes)
}
fn read_proc(input: &mut Reader<'_>) -> Result<ProcId, Fault> {
    let process = ProcId {
        pid: input.u32()?,
        start_ticks: input.u64()?,
    };
    proc_check(Some(process))?;
    Ok(process)
}
fn read_key(input: &mut Reader<'_>) -> Result<Key, Fault> {
    let owner = match input.u8()? {
        0 => {
            let bytes = input.bytes()?;
            if bytes.len() > BoxId::MAX_LEN {
                return Err(invalid("managed BoxId length"));
            }
            let text = std::str::from_utf8(bytes).map_err(|_| invalid("managed BoxId encoding"))?;
            OwnerKey::Box(BoxId::try_new(text).map_err(|error| match error {
                Refusal::Exhausted { what, bound } => Fault::Storage { what, bound },
                _ => invalid("managed BoxId"),
            })?)
        }
        1 => OwnerKey::Pool(input.array::<32>()?),
        _ => return Err(invalid("native owner kind")),
    };
    let scope = NativeScope::from_bytes(input.array::<16>()?);
    let commitment = input.array::<32>()?;
    let helper = read_proc(input)?;
    Ok(Key {
        owner,
        scope,
        commitment,
        helper,
    })
}
fn words(input: &mut Reader<'_>, frame_bytes: usize) -> Result<Vec<Vec<u8>>, Fault> {
    let count = usize::try_from(input.u32()?).map_err(|_| invalid("native word count"))?;
    // Even an empty argument consumes its length prefix in this frame and a pointer+NUL at exec.
    let remaining = frame_bytes
        .checked_sub(input.at())
        .ok_or_else(|| invalid("native word frame position"))?;
    if count
        > remaining
            .checked_div(size_of::<u32>())
            .ok_or_else(|| bounded(frame_bytes))?
        || count
            > ARG_MAX
                .checked_div(
                    size_of::<usize>()
                        .checked_add(1)
                        .ok_or_else(|| bounded(ARG_MAX))?,
                )
                .ok_or_else(|| bounded(ARG_MAX))?
    {
        return Err(invalid("native word count"));
    }
    let mut words = Vec::new();
    words.try_reserve_exact(count).map_err(|_| Fault::Storage {
        what: "managed native words",
        bound: count,
    })?;
    for _ in 0..count {
        words.push(owned_blob(input, ARG_MAX)?);
    }
    Ok(words)
}
fn descriptors(got: usize, want: usize) -> Result<(), Fault> {
    if got != want {
        return Err(Fault::Descriptors {
            what: "managed native helper message",
            want,
            got,
        });
    }
    Ok(())
}
pub fn read_request(bytes: &[u8], fds: usize) -> Result<(u16, u64, Request), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(bounded(bytes.len()));
    }
    let (mut input, version, kind, id) = super::header(bytes)?;
    // The descriptor selector is the final fixed byte; reject a hostile FD count before allocating.
    if kind == PREPARE {
        let report = match bytes.last() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(invalid("native report descriptor flag")),
        };
        descriptors(fds, if report { 3 } else { 2 })?;
    } else {
        descriptors(fds, 0)?;
    }
    let key = read_key(&mut input)?;
    let request = if kind == PREPARE {
        let record_kind = match input.u8()? {
            0 => RecordKind::Box,
            1 => RecordKind::Pool,
            _ => return Err(invalid("native record kind")),
        };
        let record = owned_blob(&mut input, crate::record::MAX_RECORD_BYTES)?;
        let binary = owned_blob(&mut input, PATH_MAX)?;
        let arguments = words(&mut input, bytes.len())?;
        let environment = words(&mut input, bytes.len())?;
        let vsock = owned_blob(&mut input, PATH_MAX)?;
        let cost_id = input.array::<32>()?;
        let seconds = input.u64()?;
        let nanos = input.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(invalid("native CPU response nanoseconds"));
        }
        let cpu = CpuControl {
            cost_id,
            response: Duration::new(seconds, nanos),
        };
        let scope = input.array::<16>()?;
        let report = input.bool("native report descriptor flag")?;
        let report_scope = if report {
            if scope != *key.scope.bytes() {
                return Err(invalid("native report scope commitment"));
            }
            Some(scope)
        } else {
            if scope != [0; 16] {
                return Err(invalid("unused native report scope"));
            }
            None
        };
        let launch = Launch {
            record_kind,
            record,
            binary,
            arguments,
            environment,
            vsock,
            cpu,
            report_scope,
        };
        launch.check()?;
        Request::Prepare { key, launch }
    } else {
        let action = match kind {
            RESUME => Action::Resume,
            ATTACH => Action::Attach,
            STOP => Action::Stop,
            TERMINAL => Action::Terminal,
            USAGE => Action::Usage,
            FINALIZE => Action::Finalize,
            INSPECT => Action::Inspect,
            _ => {
                return Err(Fault::Kind {
                    kind,
                    version,
                    expected: "a managed helper request",
                });
            }
        };
        let controller = input.option(read_proc)?;
        if action == Action::Resume && controller.is_none() {
            return Err(invalid("resume controller"));
        }
        Request::Control {
            key,
            action,
            controller,
        }
    };
    input.end(kind, version)?;
    descriptors(fds, request.descriptors())?;
    Ok((version, id, request))
}
fn read_exit(input: &mut Reader<'_>) -> Result<Exit, Fault> {
    match input.u8()? {
        0 => Ok(Exit::Unknown),
        1 => Ok(Exit::Code(input.i32()?)),
        2 => Ok(Exit::Signal(input.i32()?)),
        _ => Err(invalid("native terminal status")),
    }
}
fn read_failure(input: &mut Reader<'_>) -> Result<Failure, Fault> {
    Ok(match input.u8()? {
        0 => Failure::UnknownOwner,
        1 => Failure::Scope,
        2 => Failure::Commitment,
        3 => Failure::Controller,
        4 => Failure::Phase,
        5 => Failure::Control,
        6 => Failure::Allocation {
            bytes: input.u64()?,
        },
        7 => {
            let operation = match input.u8()? {
                0 => Operation::Prepare,
                1 => Operation::Resume,
                2 => Operation::Observe,
                3 => Operation::Stop,
                4 => Operation::Finalize,
                5 => Operation::Notify,
                6 => Operation::Connect,
                _ => return Err(invalid("native helper operation")),
            };
            let code = input.option(|input| input.i32())?;
            let kind = match code {
                Some(code) => std::io::Error::from_raw_os_error(code).kind(),
                None => read_io_kind(input)?,
            };
            Failure::Io {
                operation,
                kind,
                code,
            }
        }
        8 => Failure::Poisoned,
        9 => Failure::Uncertain,
        _ => return Err(invalid("native helper refusal")),
    })
}
fn read_usage(input: &mut Reader<'_>) -> Result<Usage, Fault> {
    Ok(Usage {
        memory_bytes: input.u64()?,
        memory_peak_bytes: input.u64()?,
        oom_kills: input.u64()?,
        pids: input.u64()?,
        pids_refused: input.u64()?,
        cpu_usec: input.u64()?,
        io_read_bytes: input.u64()?,
        io_write_bytes: input.u64()?,
        guest: None,
        started_from: None,
    })
}
pub fn read_answer(bytes: &[u8], fds: usize) -> Result<(u16, u64, Answer), Fault> {
    if bytes.len() > MAX_MESSAGE {
        return Err(bounded(bytes.len()));
    }
    let (mut input, version, kind, id) = super::header(bytes)?;
    descriptors(fds, usize::from(kind == A_ATTACHED))?;
    let key = read_key(&mut input)?;
    let answer = match kind {
        A_ATTACHED => {
            let controller = input.option(read_proc)?;
            let vsock = owned_blob(&mut input, PATH_MAX)?;
            if vsock.is_empty() || vsock.contains(&0) {
                return Err(invalid("native vsock path"));
            }
            Answer::Attached {
                key,
                controller,
                vsock,
            }
        }
        A_METADATA => {
            let controller = input.option(read_proc)?;
            let vsock = owned_blob(&mut input, PATH_MAX)?;
            if vsock.is_empty() || vsock.contains(&0) {
                return Err(invalid("native vsock path"));
            }
            Answer::Metadata {
                key,
                controller,
                vsock,
            }
        }
        A_TERMINAL => {
            let end = match input.u8()? {
                0 => None,
                1 => Some(End::Process(read_exit(&mut input)?)),
                2 => Some(End::Released(read_exit(&mut input)?)),
                _ => return Err(invalid("native terminal scope")),
            };
            Answer::Terminal { key, end }
        }
        A_USAGE => Answer::Usage {
            key,
            usage: read_usage(&mut input)?,
        },
        A_DONE => Answer::Done { key },
        A_REFUSED => Answer::Refused {
            key,
            failure: read_failure(&mut input)?,
        },
        _ => {
            return Err(Fault::Kind {
                kind,
                version,
                expected: "a managed helper answer",
            });
        }
    };
    input.end(kind, version)?;
    Ok((version, id, answer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        Key {
            owner: OwnerKey::Pool([3; 32]),
            scope: NativeScope::from_bytes([5; 16]),
            commitment: [7; 32],
            helper: ProcId {
                pid: 11,
                start_ticks: 13,
            },
        }
    }
    fn launch(report: bool) -> Launch {
        Launch {
            record_kind: RecordKind::Pool,
            record: b"{}\n".to_vec(), // Codec framing only; native dispatch must decode/check canonical record.
            binary: b"/private/shards".to_vec(),
            arguments: vec![b"run".to_vec(), b"--host-managed".to_vec()],
            environment: vec![
                b"SHARDS_CACHE=/private/cache".to_vec(),
                b"SHARDS_HOME=/private/home".to_vec(),
            ],
            vsock: b"/private/vsock".to_vec(),
            cpu: CpuControl {
                cost_id: [17; 32],
                response: Duration::from_nanos(19),
            },
            report_scope: if report { Some([5; 16]) } else { None },
        }
    }

    /// Joint plan step 14 (2c; ROOT, 2026-11-13): the node sends a launch's CPU control with the cold value, the least
    /// response the field holds that a helper's check takes (a nanosecond, the wire's unit). An older helper, which reads
    /// it as its response, then holds a scope at its credit's end with no burst to speak of, never at a census number;
    /// a learning helper reads its own. Do encode a launch with the cold control and read it back as every helper reads
    /// it. Expect its response a nanosecond and its check passed.
    #[test]
    fn w14_an_older_helper_reads_a_new_launchs_cpu_control_as_the_cold_value() {
        let cold = Launch {
            cpu: CpuControl::cold([17; 32]),
            ..launch(false)
        };
        let request = Request::Prepare {
            key: key(),
            launch: cold,
        };
        let bytes = encode_request(VERSION, 5, &request).unwrap();
        let (_, _, Request::Prepare { launch, .. }) = read_request(&bytes, 2).unwrap() else {
            panic!("not a prepare");
        };
        assert_eq!(launch.cpu.response, Duration::from_nanos(1));
        assert!(launch.cpu.check().is_ok());
    }

    /// AC-T7.4.1: option-shaped values remain opaque data; only actual prepared option positions
    /// can request the management marker or attempt a pre-rendered native handle.
    #[test]
    fn prepared_argument_roles_keep_values_opaque_and_refuse_unprepared_effects() {
        for option in [
            "--kernel",
            "--init",
            "--initrd",
            "--cmdline",
            "--cpus",
            "--memory",
            "--disk",
            "--share",
            "--vsock",
            "--console-said",
            "--gpu-device",
        ] {
            for value in [
                "--",
                "--gpu-report",
                "--gpu-report-scope",
                "--host-lifeline",
                "--host-managed",
                "",
            ] {
                let words = ["run", option, value, "--host-managed"];
                check_prepared_arguments(words.map(str::as_bytes)).unwrap();
            }
        }
        for words in [
            vec!["run", "--cmdline", "--host-managed"],
            vec!["run", "--host-managed", "--cmdline"],
            vec!["run", "--host-managed", "--host-managed"],
            vec!["run", "--host-managed", "--gpu-report", "41"],
            vec!["run", "--host-managed", "--gpu-report-scope", "1234"],
            vec!["run", "--host-managed", "--host-lifeline", "41"],
            vec!["run", "--host-managed", "--gpu-probe"],
            vec!["run", "--host-managed", "--", "guest"],
            vec!["restore", "--host-managed"],
        ] {
            assert!(check_prepared_arguments(words.into_iter().map(str::as_bytes)).is_err());
        }
    }

    /// AC-T7.4.1/R01: the public iterator API refuses within the existing native exec-byte bound,
    /// even without a finite slice. Option values cannot bypass that same counted budget.
    #[test]
    fn lazy_prepared_argument_scan_has_the_native_exec_bound() {
        use std::cell::Cell;
        let calls = Cell::new(0usize);
        let endless = std::iter::once(b"run".as_slice()).chain(std::iter::from_fn(|| {
            calls.set(calls.get() + 1);
            Some(b"--gpu".as_slice())
        }));
        assert!(matches!(
            check_prepared_arguments(endless),
            Err(Fault::Oversized { bound: ARG_MAX, .. })
        ));
        assert!(calls.get() > 0 && calls.get() <= ARG_MAX / (size_of::<usize>() + 1) + 1);
        let huge = vec![b'x'; ARG_MAX];
        assert!(matches!(
            check_prepared_arguments([b"run".as_slice(), b"--cmdline", &huge, b"--host-managed"]),
            Err(Fault::Oversized { bound: ARG_MAX, .. })
        ));
        // A finite canonical stream still reaches the required management marker after opaque values.
        check_prepared_arguments([b"run".as_slice(), b"--cmdline", b"--", b"--host-managed"])
            .unwrap();
    }

    /// AC-T7.4.1: transfer carries a pure report scope, never a parent's raw writer argument; exact
    /// descriptor count and scope commitment are required before any launch can be dispatched.
    #[test]
    fn preparation_fences_descriptors_and_final_launcher_report_rendering() {
        for report in [false, true] {
            let request = Request::Prepare {
                key: key(),
                launch: launch(report),
            };
            let encoded = encode_request(VERSION, 23, &request).unwrap();
            assert_eq!(
                read_request(&encoded, request.descriptors()).unwrap(),
                (VERSION, 23, request)
            );
            for wrong in [0, 1, 4] {
                assert!(matches!(
                    read_request(&encoded, wrong),
                    Err(Fault::Descriptors { .. })
                ));
            }
        }
        let mut prepared = launch(true);
        prepared.report_scope = Some([29; 16]);
        assert!(
            encode_request(
                VERSION,
                23,
                &Request::Prepare {
                    key: key(),
                    launch: prepared
                }
            )
            .is_err()
        );
        let mut prepared = launch(false);
        prepared
            .arguments
            .extend([b"--gpu-report".to_vec(), b"41".to_vec()]);
        assert!(prepared.check().is_err());
        let mut encoded = encode_request(
            VERSION,
            23,
            &Request::Prepare {
                key: key(),
                launch: launch(true),
            },
        )
        .unwrap();
        let scope = encoded.len() - 17;
        encoded[scope] ^= 1;
        assert!(read_request(&encoded, 3).is_err());
    }

    /// AC-T7.4.1: read-only preflight observes immutable native metadata without obtaining a
    /// notification pipe, resuming a controller or interpreting metadata as a release witness.
    #[test]
    fn metadata_only_frames_keep_legacy_attach_and_descriptor_contract() {
        let request = Request::Control {
            key: key(),
            action: Action::Inspect,
            controller: None,
        };
        let bytes = encode_request(VERSION, 79, &request).unwrap();
        assert_eq!(&bytes[..8], b"HEXM\x01\x00\x13\x00");
        assert_eq!(read_request(&bytes, 0).unwrap(), (VERSION, 79, request));
        assert!(read_request(&bytes, 1).is_err());
        for controller in [
            None,
            Some(ProcId {
                pid: 83,
                start_ticks: 89,
            }),
        ] {
            let metadata = Answer::Metadata {
                key: key(),
                controller,
                vsock: b"/private/reusable-path".to_vec(),
            };
            let bytes = encode_answer(VERSION, 97, &metadata).unwrap();
            assert_eq!(&bytes[..8], b"HEXM\x01\x00\x10\x80");
            assert_eq!(metadata.descriptors(), 0);
            assert_eq!(read_answer(&bytes, 0).unwrap(), (VERSION, 97, metadata));
            assert!(read_answer(&bytes, 1).is_err());
            let mut forged = bytes;
            forged[6..8].copy_from_slice(&A_ATTACHED.to_le_bytes());
            assert!(read_answer(&forged, 0).is_err());
            assert!(matches!(
                read_answer(&forged, 1).unwrap().2,
                Answer::Attached { .. }
            ));
        }
        // A hostile tiny frame cannot ask for pointer storage larger than its length prefixes.
        let mut tiny = Reader::new(&[0xff, 0xff, 0xff, 0x00]);
        assert!(matches!(words(&mut tiny, 4), Err(Fault::Value { .. })));
    }

    /// AC-T7.4.1: control requests retain helper/owner/scope/commitment/controller identity, and
    /// a process exit remains distinct from the positive Released scope observation.
    #[test]
    fn control_and_terminal_frames_keep_native_lifetime_and_release_distinct() {
        for action in [
            Action::Resume,
            Action::Attach,
            Action::Stop,
            Action::Terminal,
            Action::Usage,
            Action::Finalize,
        ] {
            let request = Request::Control {
                key: key(),
                action,
                controller: Some(ProcId {
                    pid: 31,
                    start_ticks: 37,
                }),
            };
            let encoded = encode_request(VERSION, 43, &request).unwrap();
            assert_eq!(read_request(&encoded, 0).unwrap(), (VERSION, 43, request));
            assert!(read_request(&encoded, 1).is_err());
        }
        let process = Answer::Terminal {
            key: key(),
            end: Some(End::Process(Exit::Code(47))),
        };
        let released = Answer::Terminal {
            key: key(),
            end: Some(End::Released(Exit::Code(47))),
        };
        let process_bytes = encode_answer(VERSION, 53, &process).unwrap();
        let released_bytes = encode_answer(VERSION, 53, &released).unwrap();
        assert_ne!(process_bytes, released_bytes);
        assert_eq!(read_answer(&process_bytes, 0).unwrap().2, process);
        assert_eq!(read_answer(&released_bytes, 0).unwrap().2, released);
        assert!(read_answer(&released_bytes, 1).is_err());
        let no_controller = Request::Control {
            key: key(),
            action: Action::Resume,
            controller: None,
        };
        assert!(encode_request(VERSION, 59, &no_controller).is_err());
    }

    /// AC-T7.4.1: new messages share the unchanged legacy framing/version. Hostile length, truncated
    /// bodies and ambient environment refuse; native errno and unrepresented I/O kinds survive.
    #[test]
    fn managed_extension_preserves_legacy_frames_and_hostile_failure_controls() {
        let old = super::super::encode_request(
            VERSION,
            61,
            &super::super::Request::Hello { version: VERSION },
        )
        .unwrap();
        let mut expected = b"HEXM\x01\x00\x01\x00".to_vec();
        expected.extend(61u64.to_le_bytes());
        expected.extend(VERSION.to_le_bytes());
        assert_eq!(old, expected);
        assert_eq!(
            super::super::read_request(&old, 0).unwrap().2,
            super::super::Request::Hello { version: VERSION }
        );
        assert!(read_request(&old, 0).is_err());
        let request = Request::Prepare {
            key: key(),
            launch: launch(false),
        };
        let bytes = encode_request(VERSION, 67, &request).unwrap();
        for end in 0..bytes.len() {
            assert!(read_request(&bytes[..end], 2).is_err());
        }
        let mut oversized = bytes;
        oversized.resize(MAX_MESSAGE + 1, 0);
        assert!(matches!(
            read_request(&oversized, 2),
            Err(Fault::Oversized { .. })
        ));
        let mut ambient = launch(false);
        ambient.environment.push(b"PATH=/foreign".to_vec());
        assert!(ambient.check().is_err());
        for kind in IoKind::NAMED {
            let answer = Answer::Refused {
                key: key(),
                failure: Failure::Io {
                    operation: Operation::Connect,
                    code: None,
                    kind,
                },
            };
            let bytes = encode_answer(VERSION, 71, &answer).unwrap();
            assert_eq!(read_answer(&bytes, 0).unwrap().2, answer);
        }
        let error = std::io::Error::from_raw_os_error(13);
        let answer = Answer::Refused {
            key: key(),
            failure: Failure::Io {
                operation: Operation::Prepare,
                code: error.raw_os_error(),
                kind: error.kind(),
            },
        };
        assert_eq!(
            read_answer(&encode_answer(VERSION, 73, &answer).unwrap(), 0)
                .unwrap()
                .2,
            answer
        );
    }
}
