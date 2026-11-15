//! Opt-in measurement only: SAME Hex entry -> foreign CLI held -> owned abort/reap.
//! ROOT registers this process child/re-export and earliest CLI branch. No normal
//! runtime, helper protocol, worker or native-scope release calls this module.
#![cfg(feature = "native-trusted-entry-measurement")]

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hex_api::{Bounds, Class, Exit, Refusal};
use hex_core::record::{BoxRecord, NativeRecord, Phase};
use shards_api::{Identity, ImageIdentity, RuntimePaths};
use triomphe::Arc;

use super::{ProcessBox, Shared};
use crate::guest::{GuestConfig, native_fixture, shares::Share};
use crate::macos::managed::{Controller, Ownership, Scope};
use crate::macos::proc::{Prepared, Reaping};
use crate::macos::sys::{self, Clamp, Spawn, trusted_entry_native as native};

const SELECTOR: &[u8] = b"__hex-trusted-entry-measurement";
// argv0 + selector + the exact fixed fields below; every share adds dir/mode/at.
const FIXED_ARGUMENTS: usize = 22;
const SHARE_ARGUMENTS: usize = 3;

/// Every input is explicit. This does not create a Store/Process row or supply a
/// default profile. ROOT retains the original Creating row and its sole charge.
pub struct Input<'a> {
    pub record: &'a BoxRecord,
    pub shared: &'a Shared,
    pub config: &'a GuestConfig,
    pub cost_file: &'a Path,
    pub cost_sha256: [u8; 32],
    pub evidence: &'a Path,
    pub paths: &'a RuntimePaths,
    pub socket: &'a Path,
    pub console_kept: usize,
    pub bootstrap: &'a Path,
    pub bootstrap_image: ImageIdentity,
    pub deadline: Instant,
    pub journal: &'a mut File,
}

#[derive(Debug, thiserror::Error)]
enum Fault {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Native(#[from] native::Error),
    #[error(transparent)]
    Token(#[from] native::TokenFailure),
    #[error("canonical runtime refusal: {0}")]
    Runtime(Refusal),
    #[error("SDK refusal: {0}")]
    Sdk(shards_api::Refusal),
    #[error("native observer failure: {0:?}")]
    Process(shards_api::ProcessFailure),
    #[error("controller ended before the foreign held gate: {0:?}")]
    EarlyExit(Exit),
    #[error("SETEXEC unexpectedly returned PID {0}")]
    Returned(i32),
}
fn io(operation: &'static str, source: std::io::Error) -> Fault {
    Fault::Io { operation, source }
}
fn invalid(why: &'static str) -> Fault {
    Fault::Invalid(why)
}

/// A pin to the SAME ProcessBox, never another holder/ledger. Even a failed run
/// returns its receipt so incomplete native cleanup cannot be mistaken for absence.
pub struct Receipt {
    owner: Arc<ProcessBox>,
    pid: i32,
    birth: u64,
    reaped: bool,
}
impl Receipt {
    pub fn identity(&self) -> Option<&Identity> {
        self.owner.identity.get()
    }
    pub fn known_controller(&self) -> (i32, u64, bool) {
        (self.pid, self.birth, self.reaped)
    }
}
impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receipt")
            .field("pid", &self.pid)
            .field("birth", &self.birth)
            .field("reaped", &self.reaped)
            .finish_non_exhaustive()
    }
}
struct FailureDetails {
    primary: Fault,
    cleanup: Option<Fault>,
    diagnostic: [Option<Fault>; 3],
    receipt: Option<Receipt>,
    returned: Option<crate::macos::proc::BoxProc>,
}
/// One invocation's bounded error buffer, reserved fallibly BEFORE native effects.
/// This is neither a new native owner nor a holder ledger; it pins the SAME receipt.
pub struct Failure {
    details: Vec<FailureDetails>,
}
impl std::fmt::Debug for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.details.first() {
            Some(d) => write!(
                f,
                "{}; cleanup={:?}; diagnostic={:?}; receipt={:?}; returned_waiter={:?}",
                d.primary,
                d.cleanup,
                d.diagnostic,
                d.receipt,
                d.returned
                    .as_ref()
                    .map(crate::macos::proc::BoxProc::pid_i32)
            ),
            None => f.write_str("trusted-entry error storage unavailable before effects"),
        }
    }
}
impl std::error::Error for Failure {}
impl Failure {
    /// Observations from the SAME retained receipts, never cleanup authority
    /// reconstructed by parsing diagnostic text.
    pub fn known_controller(&self) -> Option<(i32, u64, bool)> {
        self.details
            .first()?
            .receipt
            .as_ref()
            .map(Receipt::known_controller)
            .filter(|(pid, _, _)| *pid > 0)
    }
    pub fn known_unexpected_child(&self) -> Option<i32> {
        self.details
            .first()?
            .returned
            .as_ref()
            .map(crate::macos::proc::BoxProc::pid_i32)
    }
}
impl From<Fault> for Failure {
    fn from(primary: Fault) -> Self {
        let mut details = Vec::new();
        if details.try_reserve_exact(1).is_ok() {
            details.push(FailureDetails {
                primary,
                cleanup: None,
                diagnostic: [None, None, None],
                receipt: None,
                returned: None,
            });
        }
        Self { details }
    }
}
impl From<native::Error> for Failure {
    fn from(e: native::Error) -> Self {
        Fault::Native(e).into()
    }
}
#[derive(Debug)]
pub struct Report {
    pub receipt: Receipt,
    pub foreign_held: bool,
    pub nproc_zero_reported: bool,
    pub exit: Exit,
    /// This is ONLY exact known-controller cleanup, never native Scope::Released.
    pub direct_controller_reaped: bool,
}

fn copy(value: &OsStr) -> Result<OsString, Fault> {
    let mut out = OsString::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| invalid("fixture byte storage unavailable"))?;
    out.push(value);
    Ok(out)
}
fn bytes(value: &[u8]) -> Result<CString, Fault> {
    let size = value
        .len()
        .checked_add(1)
        .ok_or(invalid("native string width"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| invalid("native string storage unavailable"))?;
    out.extend_from_slice(value);
    out.push(0);
    CString::from_vec_with_nul(out).map_err(|_| invalid("NUL in native fixture string"))
}
fn decimal(mut value: u64) -> Result<OsString, Fault> {
    // u64 has at most20 base10 digits, a representation bound, not a runtime knob.
    let mut buffer = [0u8; 20];
    let mut at = buffer.len();
    loop {
        at = at.checked_sub(1).ok_or(invalid("decimal width"))?;
        let digit = u8::try_from(value.checked_rem(10).ok_or(invalid("decimal divisor"))?)
            .map_err(|_| invalid("decimal digit"))?;
        *buffer.get_mut(at).ok_or(invalid("decimal position"))? =
            b'0'.checked_add(digit).ok_or(invalid("decimal encoding"))?;
        value = value.checked_div(10).ok_or(invalid("decimal divisor"))?;
        if value == 0 {
            break;
        }
    }
    let mut owned = Vec::new();
    let tail = buffer.get(at..).ok_or(invalid("decimal slice"))?;
    owned
        .try_reserve_exact(tail.len())
        .map_err(|_| invalid("decimal storage"))?;
    owned.extend_from_slice(tail);
    Ok(OsString::from_vec(owned))
}
fn hex(value: &[u8]) -> Result<OsString, Fault> {
    let mut out = Vec::new();
    out.try_reserve_exact(value.len().checked_mul(2).ok_or(invalid("digest width"))?)
        .map_err(|_| invalid("digest storage"))?;
    for value in value {
        for digit in [value >> 4, value & 15] {
            out.push(
                *b"0123456789abcdef"
                    .get(usize::from(digit))
                    .ok_or(invalid("digest digit"))?,
            );
        }
    }
    Ok(OsString::from_vec(out))
}
fn digest<const N: usize>(value: &OsStr) -> Result<[u8; N], Fault> {
    let value = value.as_bytes();
    if value.len() != N.checked_mul(2).ok_or(invalid("digest width"))? {
        return Err(invalid("digest length"));
    }
    let mut out = [0; N];
    for (byte, pair) in out.iter_mut().zip(value.as_chunks::<2>().0) {
        let text = std::str::from_utf8(pair).map_err(|_| invalid("digest ASCII"))?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("digest syntax"));
        }
        *byte = u8::from_str_radix(text, 16).map_err(|_| invalid("digest byte"))?;
    }
    Ok(out)
}
fn number(value: &OsStr) -> Result<u64, Fault> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes.len() > 1 && bytes.starts_with(b"0"))
    {
        return Err(invalid("noncanonical fixture number"));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| invalid("fixture number ASCII"))?
        .parse()
        .map_err(|_| invalid("fixture number width"))
}
fn text(value: OsString) -> Result<String, Fault> {
    value
        .into_string()
        .map_err(|_| invalid("fixture text is not UTF8"))
}
fn path(value: OsString) -> Result<PathBuf, Fault> {
    let p = PathBuf::from(value);
    if !p.is_absolute() || p.as_os_str().as_bytes().contains(&0) {
        return Err(invalid("fixture path is not explicit absolute/NUL-free"));
    }
    Ok(p)
}
fn check_exec_bytes(args: &[OsString], env: &[CString], maximum: usize) -> Result<(), Fault> {
    let pointers = args
        .len()
        .checked_add(env.len())
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_mul(size_of::<usize>()))
        .ok_or(invalid("exec pointer budget"))?;
    let mut used = pointers;
    for length in args
        .iter()
        .map(|a| a.len())
        .chain(env.iter().map(|s| s.as_bytes().len()))
    {
        used = used
            .checked_add(length)
            .and_then(|n| n.checked_add(1))
            .ok_or(invalid("exec byte budget"))?;
    }
    if used > maximum {
        return Err(invalid("fixed fixture exceeds native ARG_MAX"));
    }
    Ok(())
}
fn arguments(
    input: native_fixture::Input<'_>,
    bootstrap: &Path,
    scope: [u8; 16],
    commitment: [u8; 32],
    expiry: Duration,
    clamp: Clamp,
) -> Result<Vec<OsString>, Fault> {
    let count = FIXED_ARGUMENTS
        .checked_add(
            input
                .config
                .shares
                .len()
                .checked_mul(SHARE_ARGUMENTS)
                .ok_or(invalid("share argument width"))?,
        )
        .ok_or(invalid("fixture argc width"))?;
    let mut args = Vec::new();
    args.try_reserve_exact(count)
        .map_err(|_| invalid("fixture argv storage"))?;
    args.push(copy(bootstrap.as_os_str())?);
    args.push(copy(OsStr::from_bytes(SELECTOR))?);
    args.push(copy(input.cost_file.as_os_str())?);
    args.push(hex(&input.cost_sha256)?);
    args.push(copy(input.evidence.as_os_str())?);
    let (kernel, initrd) = input
        .config
        .direct()
        .map_err(|_| invalid("an image's boot has no kernel or initramfs"))?;
    for p in [input.config.binary.as_path(), kernel, initrd] {
        args.push(copy(p.as_os_str())?);
    }
    args.push(decimal(u64::from(input.config.vcpus))?);
    args.push(decimal(input.config.memory_mib)?);
    args.push(decimal(u64::from(input.config.gpu))?);
    args.push(copy(OsStr::new(&input.config.staged))?);
    args.push(copy(input.paths.cache().as_os_str())?);
    args.push(copy(input.paths.home().as_os_str())?);
    args.push(copy(input.socket.as_os_str())?);
    args.push(decimal(
        u64::try_from(input.console_kept).map_err(|_| invalid("console width"))?,
    )?);
    args.push(decimal(expiry.as_secs())?);
    args.push(decimal(u64::from(expiry.subsec_nanos()))?);
    args.push(hex(&scope)?);
    args.push(hex(&commitment)?);
    args.push(decimal(match clamp {
        Clamp::Unclamped => 0,
        Clamp::Background => 1,
    })?);
    args.push(decimal(
        u64::try_from(input.config.shares.len()).map_err(|_| invalid("share count"))?,
    )?);
    for share in &input.config.shares {
        args.push(copy(share.dir.as_os_str())?);
        args.push(decimal(u64::from(share.read_only))?);
        args.push(copy(OsStr::new(&share.at))?);
    }
    if args.len() != count {
        return Err(invalid("fixed fixture argument count disagrees"));
    }
    Ok(args)
}
struct EntryInput {
    config: GuestConfig,
    cost_file: PathBuf,
    cost_sha256: [u8; 32],
    evidence: PathBuf,
    paths: RuntimePaths,
    socket: PathBuf,
    console_kept: usize,
    expiry: Duration,
    scope: [u8; 16],
    commitment: [u8; 32],
    clamp: Clamp,
}
fn parse(args: Vec<OsString>) -> Result<EntryInput, Fault> {
    let count = args.len();
    let mut args = args.into_iter();
    let mut take = || {
        args.next()
            .ok_or(invalid("truncated fixed fixture arguments"))
    };
    let _binary = take()?;
    if take()?.as_bytes() != SELECTOR {
        return Err(invalid("foreign private selector"));
    }
    let cost_file = path(take()?)?;
    let cost_sha256 = digest(&take()?)?;
    let evidence = path(take()?)?;
    let binary = path(take()?)?;
    let kernel = path(take()?)?;
    let initrd = path(take()?)?;
    let vcpus = u32::try_from(number(&take()?)?).map_err(|_| invalid("CPU width"))?;
    let memory_mib = number(&take()?)?;
    let gpu = match number(&take()?)? {
        0 => false,
        1 => true,
        _ => return Err(invalid("fixture GPU flag")),
    };
    let staged = text(take()?)?;
    let cache = path(take()?)?;
    let home = path(take()?)?;
    let paths = RuntimePaths::new(cache, home).map_err(Fault::Sdk)?;
    let socket = path(take()?)?;
    let console_kept = usize::try_from(number(&take()?)?).map_err(|_| invalid("console width"))?;
    let seconds = number(&take()?)?;
    let nanos =
        u32::try_from(number(&take()?)?).map_err(|_| invalid("expiry nanoseconds width"))?;
    if nanos >= 1_000_000_000 {
        return Err(invalid("expiry nanoseconds range"));
    }
    let expiry = Duration::new(seconds, nanos);
    let scope = digest(&take()?)?;
    let commitment = digest(&take()?)?;
    let clamp = match number(&take()?)? {
        0 => Clamp::Unclamped,
        1 => Clamp::Background,
        _ => return Err(invalid("native clamp flag")),
    };
    let shares = usize::try_from(number(&take()?)?).map_err(|_| invalid("share count width"))?;
    if shares > crate::guest::shares::MAX_SHARES
        || FIXED_ARGUMENTS.checked_add(
            shares
                .checked_mul(SHARE_ARGUMENTS)
                .ok_or(invalid("share argc"))?,
        ) != Some(count)
    {
        return Err(invalid("fixed share argument count"));
    }
    let mut configured = Vec::new();
    configured
        .try_reserve_exact(shares)
        .map_err(|_| invalid("share storage"))?;
    for _ in 0..shares {
        let dir = path(take()?)?;
        let read_only = match number(&take()?)? {
            0 => false,
            1 => true,
            _ => return Err(invalid("share mode")),
        };
        configured.push(Share {
            dir,
            read_only,
            at: text(take()?)?,
        });
    }
    if args.next().is_some() {
        return Err(invalid("extra fixture argument"));
    }
    Ok(EntryInput {
        config: GuestConfig {
            binary,
            boot: crate::guest::Boot::Kernel { kernel, initrd },
            vcpus,
            memory_mib,
            gpu,
            shares: configured,
            staged,
        },
        cost_file,
        cost_sha256,
        evidence,
        paths,
        socket,
        console_kept,
        expiry,
        scope,
        commitment,
        clamp,
    })
}
fn environment(paths: &RuntimePaths) -> Result<Vec<CString>, Fault> {
    let mut env = Vec::new();
    env.try_reserve_exact(2)
        .map_err(|_| invalid("environment storage"))?;
    for (name, path) in [
        (b"SHARDS_CACHE=".as_slice(), paths.cache()),
        (b"SHARDS_HOME=".as_slice(), paths.home()),
    ] {
        let mut value = Vec::new();
        value
            .try_reserve_exact(
                name.len()
                    .checked_add(path.as_os_str().len())
                    .ok_or(invalid("environment width"))?,
            )
            .map_err(|_| invalid("environment bytes"))?;
        value.extend_from_slice(name);
        value.extend_from_slice(path.as_os_str().as_bytes());
        env.push(bytes(&value)?);
    }
    Ok(env)
}
fn spawn(
    binary: &Path,
    args: &[OsString],
    envp: Vec<CString>,
    stdio: [OwnedFd; 3],
    memory: u64,
) -> Result<Spawn, Fault> {
    let mut paths = Vec::new();
    paths
        .try_reserve_exact(1)
        .map_err(|_| invalid("image storage"))?;
    paths.push(bytes(binary.as_os_str().as_bytes())?);
    let mut argv = Vec::new();
    argv.try_reserve_exact(args.len())
        .map_err(|_| invalid("argv pointer storage"))?;
    for arg in args {
        argv.push(bytes(arg.as_bytes())?);
    }
    Ok(Spawn {
        paths,
        argv,
        envp,
        cwd: None,
        stdio,
        jetsam_mib: Some(
            sys::jetsam_cap_mib(memory)
                .ok_or(invalid("allocation has no representable native cap"))?,
        ),
    })
}
fn null() -> Result<File, Fault> {
    File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|e| io("opening exact native null stdio", e))
}

/// ROOT calls this exact function ahead of EVERY std argv collection. The final
/// foreign image is never continued here; normal nonmatching CLI keeps its path.
pub fn dispatch() -> Result<Option<u8>, Failure> {
    let Some(args) = native::arguments(SELECTOR).map_err(Fault::Native)? else {
        return Ok(None);
    };
    let mut details = Vec::new();
    details.try_reserve_exact(1).map_err(|_| Failure {
        details: Vec::new(),
    })?;
    let mut scratch = EntryScratch {
        cleanup: None,
        diagnostic: [None, None, None],
        returned: None,
    };
    let result = (|| {
        let input = parse(args)?;
        entry(input, &mut scratch)
    })();
    match result {
        Ok(()) => Ok(Some(0)),
        Err(primary) => {
            details.push(FailureDetails {
                primary,
                cleanup: scratch.cleanup,
                diagnostic: scratch.diagnostic,
                receipt: None,
                returned: scratch.returned,
            });
            Err(Failure { details })
        }
    }
}
struct EntryScratch {
    cleanup: Option<Fault>,
    diagnostic: [Option<Fault>; 3],
    returned: Option<crate::macos::proc::BoxProc>,
}
fn entry(input: EntryInput, scratch: &mut EntryScratch) -> Result<(), Fault> {
    native::remaining(input.expiry)?;
    let [stdin, report, stderr] = native::stdio()?;
    drop(stderr);
    let mut report = File::from(report);
    let pid = i32::try_from(std::process::id()).map_err(|_| invalid("entry PID width"))?;
    let birth = sys::rusage(pid)
        .map_err(|e| io("reading trusted entry birth", e))?
        .filter(|u| u.started != 0 && !u.ended)
        .ok_or(invalid("trusted entry birth absent"))?
        .started;
    // Immediate partial facts precede all input/file/native preparation failures.
    writeln!(report, "ENTER {pid} {birth}").map_err(|e| io("publishing entry identity", e))?;
    let scope = hex(&input.scope)?;
    let commitment = hex(&input.commitment)?;
    writeln!(
        report,
        "KEY {} {}",
        scope.to_str().ok_or(invalid("scope hex"))?,
        commitment.to_str().ok_or(invalid("commitment hex"))?
    )
    .map_err(|e| io("publishing entry commitment", e))?;
    let (plan, allocation) = native_fixture::prepare(native_fixture::Input {
        config: &input.config,
        cost_file: &input.cost_file,
        cost_sha256: input.cost_sha256,
        evidence: &input.evidence,
        paths: &input.paths,
        socket: &input.socket,
        console_kept: input.console_kept,
    })
    .map_err(Fault::Runtime)?;
    let view = plan.native_startup().map_err(Fault::Sdk)?;
    if view.binary() != input.config.binary || view.paths() != &input.paths {
        return Err(invalid("entry startup configuration drift"));
    }
    // The parent's SAME open image witnesses are retained in the parent. This
    // entry does not invent a later Instant or claim path execution is an exec seal.
    let mut args = Vec::new();
    args.try_reserve_exact(
        plan.arguments()
            .len()
            .checked_add(1)
            .ok_or(invalid("foreign argc"))?,
    )
    .map_err(|_| invalid("foreign argv storage"))?;
    args.push(copy(plan.binary().as_os_str())?);
    for arg in plan.arguments() {
        args.push(copy(arg)?);
    }
    let final_out = null()?;
    let final_err = null()?;
    let environment = environment(&input.paths)?;
    let maximum = sys::arg_max().map_err(|e| io("reading final exec budget", e))?;
    check_exec_bytes(&args, &environment, maximum)?;
    let foreign = spawn(
        plan.binary(),
        &args,
        environment,
        [stdin, final_out.into(), final_err.into()],
        allocation.resources.mem_bytes,
    )?;
    let prepared = native::Setexec::new(&foreign, pid, input.clamp)?;
    // Prepared BEFORE the only possible unexpected native child effect.
    sys::retained_children().map_err(|e| io("checking returned-child wait ownership", e))?;
    let observer = Prepared::new().map_err(Fault::Runtime)?;
    native::remaining(input.expiry)?;
    let returned = prepared.replace(input.expiry, &mut |limits| {
        writeln!(report, "NPROC {} {}", limits.soft, limits.hard).map_err(|e| {
            native::Error::Io {
                operation: "publishing NPROC",
                source: e,
            }
        })?;
        for (role, fd) in limits.fds.iter().enumerate() {
            // Roles0/1/2 are the exact prepared final stdin/stdout/stderr order.
            writeln!(report, "FD {role} {} {} {}", fd.fd, fd.before, fd.after).map_err(|e| {
                native::Error::Io {
                    operation: "publishing final source FD",
                    source: e,
                }
            })?;
        }
        Ok(())
    });
    match returned {
        Err(error) => {
            scratch.diagnostic[0] = writeln!(report, "EXECERR {error}")
                .err()
                .map(|e| io("publishing native exec refusal", e));
            Err(Fault::Native(error))
        }
        Ok(returned) => {
            if returned <= 0 || returned == pid {
                scratch.diagnostic[0] = writeln!(report, "RETURNED {returned}")
                    .err()
                    .map(|e| io("publishing invalid spawn return", e));
                return Err(Fault::Returned(returned));
            }
            // No fallible lookup/diagnostic intervenes between the actual return
            // and binding its already allocated sole direct-parent waiter.
            let (process, registered) = observer.bind(returned, None, Reaping::Retained);
            scratch.returned = Some(process);
            scratch.diagnostic[0] = writeln!(report, "RETURNED {returned}")
                .err()
                .map(|e| io("publishing spawn return", e));
            scratch.diagnostic[1] = registered
                .err()
                .map(|e| io("registering returned child", e));
            let cleanup = (|| {
                let child = sys::rusage(returned)
                    .map_err(|e| io("reading returned child birth", e))?
                    .filter(|r| r.started != 0)
                    .ok_or(invalid("returned child has no positive birth"))?;
                scratch.diagnostic[2] =
                    writeln!(report, "RETURNED_BIRTH {returned} {}", child.started)
                        .err()
                        .map(|e| io("publishing returned child birth", e));
                if !child.ended {
                    native::signal_member(
                        &native::capture_member(returned, child.started)?,
                        native::MemberSignal::Kill,
                    )?;
                }
                let process = scratch
                    .returned
                    .as_mut()
                    .ok_or(invalid("returned child waiter absent"))?;
                let exit = loop {
                    if let Some(exit) = process.native_poll().map_err(Fault::Process)? {
                        break exit;
                    }
                    process
                        .wait_scope(&[], native::remaining(input.expiry)?)
                        .map_err(Fault::Process)?;
                };
                process.finalize_retained().map_err(Fault::Process)?;
                if let Err(e) = writeln!(
                    report,
                    "RETURNED_REAPED {returned} {} {exit:?}",
                    child.started
                ) {
                    // Retain the write failure as well as the fact that native reap succeeded.
                    return Err(io("publishing positive returned-child reap", e));
                }
                Ok::<(), Fault>(())
            })();
            if let Err(error) = cleanup {
                scratch.cleanup = Some(error);
            } else {
                scratch.returned.take();
            }
            Err(Fault::Returned(returned))
        }
    }
}

fn with_process<T>(
    receipt: &mut Receipt,
    call: impl FnOnce(&mut crate::macos::proc::BoxProc) -> Result<T, Fault>,
) -> Result<T, Fault> {
    let mut held = receipt
        .owner
        .controller
        .lock()
        .map_err(|_| invalid("native controller lock poisoned"))?;
    match &mut *held {
        Controller::Held { process, .. } => call(process),
        _ => Err(invalid("canonical native controller missing")),
    }
}
fn cleanup(receipt: &mut Receipt, expiry: Duration) -> Result<Exit, Fault> {
    if receipt.pid <= 0 {
        return Err(invalid("submitted controller PID absent"));
    }
    let actual = sys::rusage(receipt.pid)
        .map_err(|e| io("reading cleanup identity", e))?
        .ok_or(invalid("cleanup birth absent"))?;
    if receipt.birth == 0 {
        if actual.started == 0 {
            return Err(invalid("cleanup birth is not positive"));
        }
        receipt.birth = actual.started;
        receipt
            .owner
            .introduce_group(receipt.pid, receipt.birth)
            .map_err(Fault::Runtime)?;
    }
    if actual.started != receipt.birth {
        return Err(invalid("cleanup controller birth changed"));
    }
    if !actual.ended {
        native::signal_member(
            &native::capture_member(receipt.pid, receipt.birth)?,
            native::MemberSignal::Kill,
        )?;
    }
    let exit = with_process(receipt, |p| {
        loop {
            if let Some(exit) = p.native_poll().map_err(Fault::Process)? {
                break Ok(exit);
            }
            p.wait_scope(&[], native::remaining(expiry)?)
                .map_err(Fault::Process)?;
        }
    })?;
    with_process(receipt, |p| p.finalize_retained().map_err(Fault::Process))?;
    receipt.reaped = true;
    Ok(exit)
}

fn drain(
    reader: &mut File,
    bytes: &mut Vec<u8>,
    maximum: usize,
    journal: &mut File,
    expiry: Duration,
) -> Result<(), Fault> {
    // POSIX PIPE_BUF's minimum512 bytes bounds one fixed numeric report chunk;
    // total retained report remains the existing native ARG_MAX byte budget.
    let mut chunk = [0u8; 512];
    loop {
        native::remaining(expiry)?;
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let got = chunk.get(..n).ok_or(invalid("native report read width"))?;
                if bytes.len().checked_add(n).is_none_or(|n| n > maximum) {
                    return Err(invalid("native report exceeded ARG_MAX"));
                }
                journal
                    .write_all(got)
                    .map_err(|e| io("retaining entry report bytes", e))?;
                bytes.extend_from_slice(got);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io("reading entry report", e)),
        }
    }
}
fn report_nproc(
    bytes: &[u8],
    pid: i32,
    birth: u64,
    scope: [u8; 16],
    commitment: [u8; 32],
) -> Result<(), Fault> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("entry report encoding"))?;
    let mut entered = false;
    let mut keyed = false;
    let mut sealed = false;
    let mut fds = [None; 3];
    let mut count = 0usize;
    for line in text.lines() {
        let mut words = line.split(' ');
        match words.next() {
            Some("ENTER") => {
                if entered
                    || words.next().and_then(|p| p.parse::<i32>().ok()) != Some(pid)
                    || words.next().and_then(|p| p.parse::<u64>().ok()) != Some(birth)
                    || words.next().is_some()
                {
                    return Err(invalid("entry identity report mismatch"));
                }
                entered = true;
            }
            Some("KEY") => {
                if !entered || keyed || sealed {
                    return Err(invalid("entry commitment row order"));
                }
                let got_scope = digest::<16>(OsStr::new(
                    words.next().ok_or(invalid("scope row missing"))?,
                ))?;
                let got_commit = digest::<32>(OsStr::new(
                    words.next().ok_or(invalid("commitment row missing"))?,
                ))?;
                if got_scope != scope || got_commit != commitment || words.next().is_some() {
                    return Err(invalid("entry scope/commitment mismatch"));
                }
                keyed = true;
            }
            Some("NPROC") => {
                if !keyed
                    || sealed
                    || words.next() != Some("0")
                    || words.next() != Some("0")
                    || words.next().is_some()
                {
                    return Err(invalid("entry NPROC report mismatch"));
                }
                sealed = true;
            }
            Some("FD") => {
                if !sealed {
                    return Err(invalid("source FD row before seal"));
                }
                if words.next().and_then(|p| p.parse::<usize>().ok()) != Some(count) {
                    return Err(invalid("source FD role/order mismatch"));
                }
                let fd = words
                    .next()
                    .and_then(|p| p.parse::<i32>().ok())
                    .filter(|fd| *fd >= 3)
                    .ok_or(invalid("source FD identity"))?;
                let before = words
                    .next()
                    .and_then(|p| p.parse::<i32>().ok())
                    .filter(|v| *v >= 0)
                    .ok_or(invalid("source FD before flags"))?;
                let after = words
                    .next()
                    .and_then(|p| p.parse::<i32>().ok())
                    .filter(|v| *v >= 0)
                    .ok_or(invalid("source FD after flags"))?;
                if after != (before & !libc::FD_CLOEXEC)
                    || words.next().is_some()
                    || fds.contains(&Some(fd))
                {
                    return Err(invalid("source FD flag/uniqueness mismatch"));
                }
                *fds.get_mut(count).ok_or(invalid("extra source FD row"))? = Some(fd);
                count = count.checked_add(1).ok_or(invalid("source FD count"))?;
            }
            _ => return Err(invalid("entry reported failure or foreign row")),
        }
    }
    if !entered || !keyed || !sealed || count != fds.len() {
        return Err(invalid("entry lacks complete positive preexec facts"));
    }
    Ok(())
}

/// Source-only driver. ROOT supplies the canonical row/actual cost/input images,
/// measured watcher seeds, original deadline and durable controller publisher.
pub fn run(
    input: Input<'_>,
    publish: &mut dyn FnMut(&Identity) -> Result<(), Refusal>,
) -> Result<Report, Failure> {
    let mut failure_storage = Vec::new();
    failure_storage.try_reserve_exact(1).map_err(|_| Failure {
        details: Vec::new(),
    })?;
    let expiry = native::expiry(input.deadline).map_err(Fault::Native)?;
    input.record.spec.check().map_err(Fault::Runtime)?;
    if input.record.phase != Phase::Creating
        || input.record.init.is_some()
        || input.record.run.is_some()
        || input.record.exit.is_some()
        || input.record.spec.class.class() != Class::Shards
        || !input.record.spec.devices.is_empty()
        || input.config.gpu
        || input.record.spec.route_execution() != Some(hex_api::Execution::ShardsDedicated)
        || input.record.spec.route.and_then(|r| r.backing).is_some()
    {
        return Err(invalid("probe requires canonical unsubmitted CPU Shards Creating row").into());
    }
    let fixture = native_fixture::Input {
        config: input.config,
        cost_file: input.cost_file,
        cost_sha256: input.cost_sha256,
        evidence: input.evidence,
        paths: input.paths,
        socket: input.socket,
        console_kept: input.console_kept,
    };
    let (plan, allocation) = native_fixture::prepare(fixture).map_err(Fault::Runtime)?;
    if input.record.spec.allocation != Some(allocation) {
        return Err(invalid("probe cost/allocation differs from canonical row").into());
    }
    let view = plan.native_startup().map_err(Fault::Sdk)?;
    let cli_file = view.verify_cli(input.deadline).map_err(Fault::Sdk)?;
    let kernel_file = view.verify_kernel(input.deadline).map_err(Fault::Sdk)?;
    let bootstrap_file =
        shards_api::verify_native_image(input.bootstrap, input.bootstrap_image, input.deadline)
            .map_err(Fault::Sdk)?;
    let expected_scope = *input
        .record
        .native_scope
        .ok_or(invalid("canonical scope absent"))?
        .bytes();
    let expected_commitment = input.record.native_commitment().map_err(Fault::Runtime)?;
    let bounds = Bounds {
        cpu_max_milli: Some(
            u32::try_from(allocation.resources.cpu_milli)
                .map_err(|_| invalid("native CPU width"))?,
        ),
        memory_max: Some(allocation.resources.mem_bytes),
        ..Bounds::default()
    };
    bounds.check().map_err(Fault::Runtime)?;
    let clamp = super::clamp(&bounds);
    let args = arguments(
        fixture,
        input.bootstrap,
        expected_scope,
        expected_commitment,
        expiry,
        clamp,
    )?;
    let maximum = sys::arg_max().map_err(|e| io("reading native fixture exec budget", e))?;
    check_exec_bytes(&args, &[], maximum)?;
    let mut report_bytes = Vec::new();
    report_bytes
        .try_reserve_exact(maximum)
        .map_err(|_| invalid("entry report storage unavailable"))?;
    let (reader, writer) = std::io::pipe().map_err(|e| io("creating private probe report", e))?;
    let flags = rustix::fs::fcntl_getfl(&reader)
        .map_err(|e| io("reading private report flags", e.into()))?;
    rustix::fs::fcntl_setfl(&reader, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| io("setting private report nonblocking", e.into()))?;
    let before = rustix::io::fcntl_getfd(&writer)
        .map_err(|e| io("reading original report inheritability", e.into()))?;
    let null = null()?;
    let null_before = rustix::io::fcntl_getfd(&null)
        .map_err(|e| io("reading original null inheritability", e.into()))?;
    writeln!(
        input.journal,
        "PARENT_SOURCE report {} {} null {} {}",
        writer.as_raw_fd(),
        before.bits(),
        null.as_raw_fd(),
        null_before.bits()
    )
    .map_err(|e| io("retaining original parent descriptor facts", e))?;
    let initial = spawn(
        input.bootstrap,
        &args,
        Vec::new(),
        [
            null.try_clone()
                .map_err(|e| io("copying probe stdin", e))?
                .into(),
            writer
                .try_clone()
                .map_err(|e| io("copying probe report writer", e))?
                .into(),
            writer
                .try_clone()
                .map_err(|e| io("copying private entry diagnostics", e))?
                .into(),
        ],
        allocation.resources.mem_bytes,
    )?;
    let mut events = native::ImageEvents::new()?;
    let mut identity_vsock = Some(PathBuf::from(copy(plan.vsock().as_os_str())?));
    sys::retained_children().map_err(|e| io("checking sole child wait ownership", e))?;
    let scope =
        Scope::new(NativeRecord::Box(input.record), Ownership::Child).map_err(Fault::Runtime)?;
    let observer = Prepared::new().map_err(Fault::Runtime)?;
    // Measurement only: SAME concrete owner, WITHOUT an ordinary watcher that
    // could issue CONT across the held final image. No ordinary cadence changes.
    let owner = Arc::try_new(
        ProcessBox::prepared(
            NativeRecord::Box(input.record)
                .owner()
                .try_owned()
                .map_err(Fault::Runtime)?,
            &bounds,
            input.shared,
            None,
            Some(scope),
        )
        .map_err(Fault::Runtime)?,
    )
    .map_err(|_| invalid("native receipt allocation unavailable"))?;
    *owner
        .controller
        .lock()
        .map_err(|_| invalid("native prepared controller lock poisoned"))? =
        Controller::Prepared(observer);
    let mut receipt = Receipt {
        owner,
        pid: 0,
        birth: 0,
        reaped: false,
    };
    let mut reader = File::from(OwnedFd::from(reader));
    let operation = (|| {
        native::remaining(expiry)?;
        let prepared = {
            let mut held = receipt
                .owner
                .controller
                .lock()
                .map_err(|_| invalid("native controller lock poisoned"))?;
            match std::mem::replace(&mut *held, Controller::Unsubmitted) {
                Controller::Prepared(p) => p,
                other => {
                    *held = other;
                    return Err(invalid("native observer was not prepared"));
                }
            }
        };
        let result = receipt.owner.held_exec(&initial);
        let pid = match result {
            Ok(pid) => pid,
            Err(error) => {
                let submitted = *receipt
                    .owner
                    .state
                    .pgid
                    .lock()
                    .map_err(|_| invalid("native submitted PID lock poisoned"))?;
                if let Some(pid) = submitted {
                    receipt.pid = pid;
                    let (process, registered) =
                        prepared.bind(pid, Some(receipt.owner.members()), Reaping::Retained);
                    *receipt
                        .owner
                        .controller
                        .lock()
                        .map_err(|_| invalid("native controller lock poisoned"))? =
                        Controller::Held {
                            process,
                            failure: registered.err().map(|e| {
                                crate::macos::proc::process_failure(
                                    shards_api::ProcessOperation::RegisterExit,
                                    e,
                                )
                            }),
                        };
                }
                return Err(Fault::Runtime(error));
            }
        };
        receipt.pid = pid;
        let (process, registered) =
            prepared.bind(pid, Some(receipt.owner.members()), Reaping::Retained);
        *receipt
            .owner
            .controller
            .lock()
            .map_err(|_| invalid("native controller lock poisoned"))? = Controller::Held {
            process,
            failure: None,
        };
        writeln!(input.journal, "SUBMITTED {pid}").map_err(|e| io("retaining submitted PID", e))?;
        registered.map_err(|e| io("registering original controller", e))?;
        let observed = sys::rusage(pid)
            .map_err(|e| io("reading held Hex birth", e))?
            .filter(|r| r.started != 0 && !r.ended)
            .ok_or(invalid("held Hex birth absent"))?;
        receipt.birth = observed.started;
        writeln!(input.journal, "TRUSTED_BIRTH {pid} {}", receipt.birth)
            .map_err(|e| io("retaining observed Hex birth", e))?;
        receipt
            .owner
            .introduce_group(pid, observed.started)
            .map_err(Fault::Runtime)?;
        if sys::run_state(pid).map_err(|e| io("checking held Hex state", e))?
            != Some(sys::Run::Stopped)
        {
            return Err(invalid("Hex was not natively held"));
        }
        writeln!(input.journal, "TRUSTED_HELD {pid} {}", receipt.birth)
            .map_err(|e| io("retaining held Hex fact", e))?;
        let token = native::capture_member(pid, receipt.birth)?;
        let identity = Identity {
            pid: u32::try_from(pid).map_err(|_| invalid("controller PID width"))?,
            started: receipt.birth,
            vsock: identity_vsock
                .take()
                .ok_or(invalid("prepared identity path absent"))?,
        };
        receipt
            .owner
            .identity
            .set(identity)
            .map_err(|_| invalid("controller identity changed"))?;
        events.bind(pid, Some(reader.as_raw_fd()))?;
        publish(
            receipt
                .owner
                .identity
                .get()
                .ok_or(invalid("controller identity absent"))?,
        )
        .map_err(Fault::Runtime)?;
        native::remaining(expiry)?;
        receipt
            .owner
            .scope()
            .ok_or(invalid("native scope absent"))?
            .running(receipt.owner.owner())
            .map_err(Fault::Runtime)?;
        native::signal_member(&token, native::MemberSignal::Continue)?;
        receipt.owner.state.wake().map_err(Fault::Runtime)?;
        loop {
            drain(
                &mut reader,
                &mut report_bytes,
                maximum,
                input.journal,
                expiry,
            )?;
            if let Some(exit) =
                with_process(&mut receipt, |p| p.native_poll().map_err(Fault::Process))?
            {
                return Err(Fault::EarlyExit(exit));
            }
            let event = events.next(expiry)?;
            if event.readable {
                drain(
                    &mut reader,
                    &mut report_bytes,
                    maximum,
                    input.journal,
                    expiry,
                )?;
            }
            if event.exec || event.exit {
                writeln!(
                    input.journal,
                    "IMAGE_EVENT {pid} {} {} {} {}",
                    receipt.birth, event.raw_flags, event.raw_fflags, event.raw_data
                )
                .map_err(|e| io("retaining native image event", e))?;
            }
            if event.waitable
                && let Some(exit) =
                    with_process(&mut receipt, |p| p.native_poll().map_err(Fault::Process))?
            {
                return Err(Fault::EarlyExit(exit));
            }
            if event.exec {
                writeln!(
                    input.journal,
                    "FOREIGN_EXEC {} {} {} {} {}",
                    pid, receipt.birth, event.raw_flags, event.raw_fflags, event.raw_data
                )
                .map_err(|e| io("retaining foreign exec event", e))?;
                if event.exit
                    || sys::run_state(pid).map_err(|e| io("checking foreign held state", e))?
                        != Some(sys::Run::Stopped)
                    || sys::rusage(pid)
                        .map_err(|e| io("checking postexec birth", e))?
                        .is_none_or(|r| r.started != receipt.birth || r.ended)
                {
                    return Err(invalid(
                        "foreign image did not retain exact held incarnation",
                    ));
                }
                // Genuine task token captured AFTER this exec. No foreign CONT exists.
                let fresh = native::capture_member(pid, receipt.birth)?;
                if fresh.pid() != pid || fresh.birth() != receipt.birth {
                    return Err(invalid("postexec token identity mismatch"));
                }
                drain(
                    &mut reader,
                    &mut report_bytes,
                    maximum,
                    input.journal,
                    expiry,
                )?;
                report_nproc(
                    &report_bytes,
                    pid,
                    receipt.birth,
                    expected_scope,
                    expected_commitment,
                )?;
                break;
            }
        }
        if rustix::io::fcntl_getfd(&writer)
            .map_err(|e| io("checking parent report flags", e.into()))?
            != before
            || rustix::io::fcntl_getfd(&null)
                .map_err(|e| io("checking parent null flags", e.into()))?
                != null_before
        {
            return Err(invalid("parent descriptor flags changed"));
        }
        writeln!(
            input.journal,
            "FOREIGN_HELD_NO_CONT {pid} {} PARENT_FLAGS {} {}",
            receipt.birth,
            before.bits(),
            null_before.bits()
        )
        .map_err(|e| io("retaining final held fact", e))?;
        Ok::<(), Fault>(())
    })();
    // Close inherited/report copies before the cleanup drain, independent of oracle outcome.
    drop(initial);
    drop(writer);
    let start_diagnostic = writeln!(
        input.journal,
        "CLEANUP_BEGIN {} {}",
        receipt.pid, receipt.birth
    )
    .err()
    .map(|e| io("retaining cleanup start", e));
    let cleaned = cleanup(&mut receipt, expiry);
    let end_diagnostic = match &cleaned {
        Ok(exit) => writeln!(
            input.journal,
            "CONTROLLER_REAPED {} {} {exit:?}",
            receipt.pid, receipt.birth
        )
        .err()
        .map(|e| io("retaining controller reap", e)),
        Err(error) => writeln!(input.journal, "CONTROLLER_CLEANUP_ERROR {error}")
            .err()
            .map(|e| io("retaining cleanup failure", e)),
    };
    let drained = drain(
        &mut reader,
        &mut report_bytes,
        maximum,
        input.journal,
        expiry,
    );
    // SAME content witnesses outlive held-image observation and cleanup. No exec seal claimed.
    drop((cli_file, kernel_file, bootstrap_file));
    match (
        operation,
        cleaned,
        drained,
        start_diagnostic,
        end_diagnostic,
    ) {
        (Ok(()), Ok(exit), Ok(()), None, None) => Ok(Report {
            receipt,
            foreign_held: true,
            nproc_zero_reported: true,
            exit,
            direct_controller_reaped: true,
        }),
        (primary, cleaned, drained, start, end) => {
            let cleanup = cleaned.err();
            let drain = drained.err();
            let primary = match primary {
                Err(error) => error,
                Ok(()) => invalid("cleanup or raw diagnostic failed after the held gate"),
            };
            // Exactly one push into the one-slot buffer reserved before any native effects.
            failure_storage.push(FailureDetails {
                primary,
                cleanup,
                diagnostic: [start, end, drain],
                receipt: Some(receipt),
                returned: None,
            });
            Err(Failure {
                details: failure_storage,
            })
        }
    }
}

#[cfg(test)]
mod tests;
