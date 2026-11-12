//! `kiln supervise` on Windows. There is no SIGTERM or SIGHUP, and console control events
//! reach a whole console rather than one process, so the supervisor hands its child a stdin pipe
//! and asks it to stop by closing that pipe: the child (with `KILN_SUPERVISED=1`) watches its
//! stdin and shuts down gracefully at EOF. That also stops the child cleanly if the supervisor
//! itself dies. A child that ignores the request is killed with its tree after the grace period.
//!
//! A runner that takes up a new binary hands its agent sessions' harness processes over to the
//! supervisor ([`Handover`]), which starts the new binary as a fresh process that goes on with
//! them (`os::UPGRADE_BY_SUCCESSOR`; terminals, ConPTY, are not handed over yet). Windows has no
//! `SCM_RIGHTS`: the runner writes its harness handle values over a pipe the child inherits, the
//! supervisor duplicates each into itself (`DuplicateHandle`, inheritable) and only then answers
//! `K`, so the runner keeps them open until the supervisor holds its own copies; the runner exits,
//! and the supervisor starts the fresh runner with those handles inherited, their values in its
//! environment. Keeping the job handle (kill-on-close) across the gap is what keeps the harness
//! alive while no runner holds it.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// `CREATE_NO_WINDOW`: a hidden console that the supervisor's children inherit (no window pops
/// up at logon). `CREATE_NEW_PROCESS_GROUP`: Ctrl-C elsewhere does not reach it.
/// `CREATE_BREAKAWAY_FROM_JOB`: outlive the Scheduled Task (or shell) that launched it.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// How the supervisor talks to its running child.
pub struct ChildControl {
    pid: u32,
    stdin: Option<ChildStdin>,
}

impl ChildControl {
    /// No in-place re-execution on Windows, so there is never a child to adopt.
    pub fn adopt(_pid: u32) -> Option<Self> {
        None
    }

    /// Spawn the supervised child with a stdin pipe the supervisor keeps (its stop request).
    pub fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Self)> {
        let mut child = cmd.stdin(Stdio::piped()).spawn()?;
        let stdin = child.stdin.take();
        let pid = child.id();
        Ok((child, Self { pid, stdin }))
    }

    pub fn pid(&self) -> Option<i32> {
        i32::try_from(self.pid).ok()
    }

    /// Windows has no reload signal; restart the service to apply new configuration.
    pub fn reload(&self) {}

    /// Ask the child to shut down gracefully: close its stdin.
    pub fn stop(&mut self) {
        drop(self.stdin.take());
    }

    /// Kill the child and everything it started.
    pub async fn kill(&mut self) {
        let pid = self.pid;
        let _ = crate::rt::blocking(move || crate::os::proc::taskkill_tree(pid)).await;
    }
}

/// Wait for the child (a 100 ms `try_wait` poll) and report how it ended: its exit code and a
/// description.
#[allow(clippy::disallowed_methods)] // (e) polls a child on Windows; its handle's wait would tell
pub async fn wait_child(mut child: Child) -> (Option<i32>, String) {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code(), status.to_string()),
            Ok(None) => crate::rt::sleep(Duration::from_millis(100)).await,
            Err(e) => return (None, format!("wait error: {e}")),
        }
    }
}

/// Spawn `cmd` detached: no console window, its own process group, and out of any job object
/// that allows breakaway (refused breakaway is retried without it).
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    match cmd.spawn() {
        Ok(child) => Ok(child),
        Err(_) => {
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        }
    }
}

/// Kill every running `kiln supervise … -- <subcommand> …` of this user (with its child and the
/// child's jobs): a detached supervisor outlives the Scheduled Task that launched it, so
/// `kiln uninstall` stops it here. Returns how many were stopped.
pub fn stop_supervisors(subcommand: &str) -> usize {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let me = std::process::id();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    let mut stopped = 0;
    for (pid, p) in sys.processes() {
        let args: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let is_kiln = args.first().is_some_and(|a| crate::os::program_stem(a) == "kiln");
        let ours = is_kiln
            && args.iter().any(|a| a == "supervise")
            && args.iter().any(|a| a == subcommand)
            && (subcommand == "agent" || !args.iter().any(|a| a == "agent"));
        if ours && pid.as_u32() != me && crate::os::proc::taskkill_tree(pid.as_u32()) {
            stopped += 1;
        }
    }
    stopped
}

/// Stop these supervisors, with their children and the children's jobs.
pub fn stop(pids: &[u32]) {
    for pid in pids {
        crate::os::proc::taskkill_tree(*pid);
    }
}

/// In a supervised child, treat EOF on stdin (the supervisor closed it, or died) as a shutdown
/// request.
pub fn watch_supervisor(shutdown: crate::rt::Shutdown) {
    if !std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1") {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-supervisor-pipe".into()).spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        tracing::info!("the supervisor asked for a shutdown");
        shutdown.cancel();
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot watch the supervisor pipe");
    }
}

// ------------------------------------------------------------------ handing sessions over

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(read: *mut *mut c_void, write: *mut *mut c_void, attrs: *const c_void, size: u32) -> i32;
    fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
    fn DuplicateHandle(src_proc: *mut c_void, src: *mut c_void, dst_proc: *mut c_void, dst: *mut *mut c_void, access: u32, inherit: i32, options: u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
}

const HANDLE_FLAG_INHERIT: u32 = 0x1;
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
const PROCESS_DUP_HANDLE: u32 = 0x0040;

/// The handover channel's environment: this supervisor's pid, that it carries sessions, and the two
/// pipe ends the child inherits (its write end of the runner → supervisor pipe, and its read end of
/// the supervisor → runner acknowledgement pipe).
const SUPERVISOR_PID: &str = "KILN_SUPERVISOR_PID";
const SUPERVISOR_SESSIONS: &str = "KILN_SUPERVISOR_SESSIONS";
const PIPE_OUT: &str = "KILN_SUPERVISOR_PIPE_OUT";
const PIPE_IN: &str = "KILN_SUPERVISOR_PIPE_IN";
/// The agent sessions' handles a fresh runner takes over: five per session (stdin, stdout, stderr,
/// process, job), flattened, in the order the runner before it handed them (sorted by id), which the
/// fresh runner re-maps the same way (`Host::restore`). The supervisor inherited these handles to it.
const HANDED_SESSION_HANDLES: &str = "KILN_HANDOVER_SESSION_HANDLES";

/// How many handles travel per session (stdin, stdout, stderr, process, job).
const PER_SESSION: usize = 5;

fn handle_to_i64(h: RawHandle) -> i64 {
    h as isize as i64
}

fn i64_to_handle(v: i64) -> RawHandle {
    v as isize as *mut c_void
}

/// A pipe's two ends (read, write), neither inheritable (the child's end is made inheritable by the
/// caller). `None` when Windows refuses.
fn pipe() -> Option<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write): (*mut c_void, *mut c_void) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: places for the two handles on this stack; default (non-inheritable) security.
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } != 0;
    if !ok || read.is_null() || write.is_null() {
        return None;
    }
    // SAFETY: two handles CreatePipe just made in this process; nobody else owns them.
    Some(unsafe { (OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write)) })
}

/// Make handle `h` inheritable (so the child takes it), or not (so nothing this process starts does).
fn make_inheritable(h: RawHandle, on: bool) -> bool {
    // SAFETY: a handle this process holds.
    unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, if on { HANDLE_FLAG_INHERIT } else { 0 }) != 0 }
}

/// A runner's handover channel: PTY masters aside (ConPTY is not handed over on Windows), it hands
/// its agent sessions' harness handles over the pipe the child inherits, and the supervisor
/// duplicates them into itself before answering, so the runner can exit knowing they are held.
pub struct Handover {
    /// The supervisor's read end of the runner → supervisor pipe.
    from_runner: Option<File>,
    /// The supervisor's write end of the supervisor → runner acknowledgement pipe.
    ack: Option<File>,
    /// The child's ends, closed here once the child has them (it inherited them at spawn).
    child_ends: Option<(OwnedHandle, OwnedHandle)>,
    collected: Option<futures::channel::oneshot::Receiver<Vec<OwnedHandle>>>,
}

/// The sessions' handles a runner handed over, duplicated into the supervisor (inheritable): what
/// the next runner takes over. Five handles per session.
pub struct Terminals {
    sessions: Vec<OwnedHandle>,
}

impl Handover {
    /// A channel for the child `cmd` is about to start: two pipes whose child ends it inherits,
    /// their values and this supervisor's pid in its environment, and that this supervisor carries
    /// sessions. `None` if the pipes can't be made: the child then runs without one.
    pub fn for_child(cmd: &mut Command) -> Option<Self> {
        // A: runner → supervisor (runner writes its handle values). B: supervisor → runner (the ack).
        let (a_read, a_write) = pipe()?;
        let (b_read, b_write) = pipe()?;
        // The child inherits its write end of A and its read end of B.
        if !make_inheritable(a_write.as_raw_handle(), true) || !make_inheritable(b_read.as_raw_handle(), true) {
            return None;
        }
        cmd.env(SUPERVISOR_PID, std::process::id().to_string())
            .env(SUPERVISOR_SESSIONS, "1")
            .env(PIPE_OUT, handle_to_i64(a_write.as_raw_handle()).to_string())
            .env(PIPE_IN, handle_to_i64(b_read.as_raw_handle()).to_string());
        Some(Self { from_runner: Some(File::from(a_read)), ack: Some(File::from(b_write)), child_ends: Some((a_write, b_read)), collected: None })
    }

    /// The child (pid `pid`) is running: close the supervisor's copies of its ends, and start
    /// collecting (and duplicating) what it hands over.
    pub fn started(&mut self, pid: u32) {
        drop(self.child_ends.take());
        let (Some(from_runner), Some(ack)) = (self.from_runner.take(), self.ack.take()) else { return };
        let (tx, rx) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new().name("kiln-handover".into()).spawn(move || {
            let _ = tx.send(collect(from_runner, ack, pid).unwrap_or_default());
        });
        if spawned.is_ok() {
            self.collected = Some(rx);
        }
    }

    /// What the runner handed over, duplicated into this supervisor: `None` when it handed nothing
    /// or a handle couldn't be duplicated (then its sessions resume from their records).
    pub async fn finish(mut self) -> Option<Terminals> {
        let rx = self.collected.take()?;
        match crate::rt::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(sessions)) if !sessions.is_empty() => Some(Terminals { sessions }),
            _ => None,
        }
    }
}

impl Terminals {
    /// How many sessions came across: what the supervisor's log counts (no terminals are handed on
    /// Windows).
    pub fn len(&self) -> usize {
        self.sessions.len() / PER_SESSION
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Give these handles to the child `cmd` starts: inherited (they are already inheritable), their
    /// values in its environment. Dropping `self` once it has started closes this process's copies.
    pub fn pass_to(&self, cmd: &mut Command) -> std::io::Result<()> {
        if !self.sessions.is_empty() {
            let values: Vec<String> = self.sessions.iter().map(|h| handle_to_i64(h.as_raw_handle()).to_string()).collect();
            cmd.env(HANDED_SESSION_HANDLES, values.join(","));
        }
        Ok(())
    }
}

/// The supervisor's side of the handover: read the runner's handle values, duplicate each into this
/// process (inheritable) from the runner process, and only then answer `K` so the runner may exit.
/// The duplicates (five per session, in the order sent) are what the next runner inherits; empty if
/// the runner handed nothing or any handle couldn't be duplicated.
fn collect(mut from_runner: File, mut ack: File, pid: u32) -> Option<Vec<OwnedHandle>> {
    let mut count_bytes = [0u8; 4];
    if from_runner.read_exact(&mut count_bytes).is_err() {
        return None;
    }
    let count = u32::from_le_bytes(count_bytes) as usize;
    // A sane number of sessions; anything else is a runner that isn't ours.
    if count > 4096 {
        return None;
    }
    let bytes = count.checked_mul(PER_SESSION * 8)?;
    let mut values = vec![0u8; bytes];
    if from_runner.read_exact(&mut values).is_err() {
        return None;
    }
    // The runner now waits for the ack; from here, it is always sent before returning.
    let dups = (|| {
        let runner = open_for_dup(pid)?;
        let mut dups = Vec::with_capacity(count * PER_SESSION);
        for value in values.as_chunks::<8>().0 {
            dups.push(duplicate_from(runner.as_raw_handle(), i64::from_le_bytes(*value))?);
        }
        Some(dups)
    })();
    let _ = ack.write_all(b"K").and_then(|()| ack.flush());
    dups
}

/// Open process `pid` to duplicate handles out of it.
fn open_for_dup(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: a pid; the handle it gives is owned here.
    let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
    (!h.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Duplicate handle value `value` out of process `source` into this one, inheritable (so the fresh
/// runner takes it). `None` when Windows refuses.
fn duplicate_from(source: RawHandle, value: i64) -> Option<OwnedHandle> {
    let src = i64_to_handle(value);
    if src.is_null() {
        return None;
    }
    let mut out: *mut c_void = std::ptr::null_mut();
    // SAFETY: a handle open in `source`, duplicated into this process; the out pointer lives here.
    let ok = unsafe { DuplicateHandle(source, src, GetCurrentProcess(), &mut out, 0, 1, DUPLICATE_SAME_ACCESS) } != 0;
    if !ok || out.is_null() {
        return None;
    }
    // SAFETY: a handle DuplicateHandle just made in this process; nobody else owns it.
    Some(unsafe { OwnedHandle::from_raw_handle(out) })
}

/// This process's handover channel (write to the supervisor, read its ack), when the supervisor that
/// started it set one up. Claimed once, early at start-up: its ends' inherit flag is cleared so
/// nothing this runner starts inherits them, and they are validated as open.
fn channel() -> Option<&'static (File, File)> {
    static CHANNEL: OnceLock<Option<(File, File)>> = OnceLock::new();
    CHANNEL
        .get_or_init(|| {
            let write = claim(from_our_supervisor(PIPE_OUT)?)?;
            let read = claim(from_our_supervisor(PIPE_IN)?)?;
            Some((File::from(write), File::from(read)))
        })
        .as_ref()
}

/// Take over an inherited channel handle named by value `value`: validate it is open in this
/// process, clear its inherit flag (so this runner's harnesses never take it), and own it.
fn claim(value: String) -> Option<OwnedHandle> {
    let raw = i64_to_handle(value.parse::<i64>().ok()?);
    if raw.is_null() {
        return None;
    }
    let mut flags = 0u32;
    // SAFETY: a handle value; GetHandleInformation only reads it, and fails for one not open here.
    if unsafe { GetHandleInformation(raw, &mut flags) } == 0 {
        return None;
    }
    make_inheritable(raw, false);
    // SAFETY: a handle open in this process, inherited from the supervisor; owned by nobody else.
    Some(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The handover variables the supervisor set for its direct child; a harness inherits the strings
/// but not the channel handles (they are made non-inheritable before any harness starts), so it
/// never takes the channel. `None` unless this process was started under a supervisor.
fn from_our_supervisor(var: &str) -> Option<String> {
    std::env::var(SUPERVISOR_PID).ok()?;
    std::env::var(var).ok()
}

/// Take what the supervisor handed this process at start (call it first thing in a runner).
pub fn claim_inherited() {
    let _ = channel();
}

/// Whether this process can hand its sessions to its supervisor ([`to_supervisor`]).
pub fn has_supervisor_channel() -> bool {
    channel().is_some()
}

/// Whether the supervisor that started this process carries agent sessions across a successor
/// upgrade (only a supervisor that sets up the channel does).
pub fn supervisor_takes_sessions() -> bool {
    from_our_supervisor(SUPERVISOR_SESSIONS).is_some_and(|v| v == "1")
}

/// Terminals are not handed over on Windows (ConPTY): none ever come across.
pub fn handed_terminals() -> Option<Vec<i32>> {
    None
}

/// The agent sessions' handles the supervisor handed this process, five per session (stdin, stdout,
/// stderr, process, job), in the order the runner before it handed them. Taken once, at the fresh
/// runner's first restore, for the same reason as the Unix layer.
pub fn handed_sessions() -> Option<Vec<i64>> {
    static TAKEN: Mutex<Option<Option<Vec<i64>>>> = Mutex::new(None);
    let mut slot = TAKEN.lock().unwrap_or_else(PoisonError::into_inner);
    slot.get_or_insert_with(|| from_our_supervisor(HANDED_SESSION_HANDLES).map(|list| list.split(',').filter_map(|n| n.trim().parse::<i64>().ok()).collect())).take()
}

/// Hand these agent sessions' handles to the supervisor and wait for it to take them. PTY masters
/// (`_masters`) are not handed over on Windows. The process exits with
/// [`crate::supervise::HANDOVER_EXIT`] next; its handles stay open until the ack, so the supervisor
/// can duplicate them out of it.
pub fn to_supervisor(_masters: &[i32], sessions: &[crate::proc::Detached]) -> std::io::Result<()> {
    let (write, read) = channel().ok_or_else(|| std::io::Error::other("no handover channel to the supervisor"))?;
    let mut buf = Vec::with_capacity(4 + sessions.len() * PER_SESSION * 8);
    buf.extend_from_slice(&u32::try_from(sessions.len()).unwrap_or(u32::MAX).to_le_bytes());
    for d in sessions {
        for value in [d.stdin, d.stdout, d.stderr, d.process, d.job] {
            buf.extend_from_slice(&value.to_le_bytes());
        }
    }
    let mut w: &File = write;
    w.write_all(&buf)?;
    w.flush()?;
    let mut ack = [0u8; 1];
    let mut r: &File = read;
    r.read_exact(&mut ack)?;
    if ack != *b"K" {
        return Err(std::io::Error::other("the supervisor did not take the sessions over"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A1 (step 30's follow-up): the supervisor's custody round trip. A handle (here a pipe's write
    /// end) is duplicated into this process inheritable, as the supervisor duplicates a runner's
    /// harness handle into itself; a fresh child inherits it at the same value and writes to it by
    /// that value, as a fresh runner adopts a handed handle; and what it wrote arrives. This is the
    /// core of carrying a harness's pipes, process and job across a successor upgrade on Windows.
    #[test]
    fn a1_a_duplicated_handle_is_inherited_and_adopted_by_a_fresh_child() -> anyhow::Result<()> {
        let (read, write) = pipe().ok_or_else(|| anyhow::anyhow!("no pipe"))?;
        // SAFETY: this process's pseudo-handle; duplicate_from only reads it.
        let me = unsafe { GetCurrentProcess() };
        let dup = duplicate_from(me, handle_to_i64(write.as_raw_handle())).ok_or_else(|| anyhow::anyhow!("couldn't duplicate the handle inheritable"))?;
        let value = handle_to_i64(dup.as_raw_handle());
        let script = format!(
            "$h=[System.IntPtr]::new([long]{value}); $s=New-Object Microsoft.Win32.SafeHandles.SafeFileHandle($h,$false); $f=New-Object System.IO.FileStream($s,[System.IO.FileAccess]::Write); $f.Write([Text.Encoding]::ASCII.GetBytes('inherited'),0,9); $f.Flush()"
        );
        let mut child = Command::new("powershell");
        // std spawns with bInheritHandles = TRUE when stdio is piped, so the inheritable duplicate
        // is inherited at its value.
        child.args(["-NoProfile", "-Command", &script]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut c = child.spawn()?;
        // The fresh child holds its own inherited copy now; close this process's write handles so the
        // only write end left is the child's, and the read end sees its end once the child exits.
        drop(write);
        drop(dup);
        let ok = c.wait()?.success();
        let mut s = String::new();
        std::fs::File::from(read).read_to_string(&mut s)?;
        anyhow::ensure!(ok, "the fresh child wrote to the inherited handle");
        assert_eq!(s, "inherited", "what the fresh child wrote to the handle it adopted by value");
        Ok(())
    }
}
