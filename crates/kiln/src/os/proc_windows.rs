//! Child processes on Windows. hyper-rt's readiness covers sockets only (IOCP/AFD), so each
//! anonymous pipe is drained by a small dedicated thread that forwards chunks over a channel; a
//! bounded pool would be wrong here, because a pipe read lasts as long as the child writes and a
//! queued reader could deadlock a child blocked on a full pipe. Exits are a 50 ms `try_wait`
//! poll (non-blocking). Every child runs in its own job object (`command-group`) created with
//! `KILL_ON_JOB_CLOSE`: a kill terminates the whole tree, and if the agent dies the OS closes the
//! job and takes its children with it. A step's job is kiln's own (`crate::os::contain::Tree`), so
//! a paused job's steps can be suspended where they run (docs/ci.md W26-R22).
//!
//! A harness process ([`Duplex`]) is handed across a successor upgrade (`os::UPGRADE_BY_SUCCESSOR`,
//! a new binary taken up as a fresh process): [`Duplex::detach`] stops its pump and stdin threads
//! without closing the pipes and without losing a byte (`CancelSynchronousIo` unblocks a pump
//! thread's `ReadFile`; the aborted read consumes nothing, so what it hadn't read stays in the pipe
//! for the next process), and keeps its pipe, process and job handles. [`adopt_duplex`] serves them
//! again in the fresh process: a process it didn't start, whose exit it still sees through the
//! process handle (`WaitForSingleObject`, `GetExitCodeProcess`), and whose job (kill-on-close) it
//! keeps open so nothing it holds is taken. The handles travel between the two processes through the
//! supervisor (`os::supervise::Handover`), which duplicates them into itself and lets the fresh
//! process inherit them.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, IntoRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::ExitStatusExt as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use command_group::{CommandGroup as _, GroupChild};
use futures::channel::oneshot;
use parking_lot::Mutex;

// The handful of Win32 calls the harness handover needs, declared by hand (std only), as
// `os::win32` declares its own.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn DuplicateHandle(src_proc: *mut c_void, src: *mut c_void, dst_proc: *mut c_void, dst: *mut *mut c_void, access: u32, inherit: i32, options: u32) -> i32;
    fn CancelSynchronousIo(thread: *mut c_void) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
    fn TerminateJobObject(job: *mut c_void, code: u32) -> i32;
}

/// `DUPLICATE_SAME_ACCESS`.
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
const WAIT_OBJECT_0: u32 = 0;

/// A handle value as it travels in a [`Detached`] (serialized, and carried in the supervisor's
/// environment list): Windows handle values are 32 significant bits even on 64-bit Windows, and a
/// null handle is `0`.
fn handle_to_i64(h: RawHandle) -> i64 {
    h as isize as i64
}

fn i64_to_handle(v: i64) -> RawHandle {
    v as isize as *mut c_void
}

/// Duplicate a handle this process holds into this process, so it outlives the owner it was read
/// from (a pump thread's pipe, the job in a [`GroupChild`]). Not inheritable: the supervisor makes
/// the inheritable copies the fresh process takes. `None` when Windows refuses.
fn duplicate(raw: RawHandle) -> Option<OwnedHandle> {
    if raw.is_null() {
        return None;
    }
    let mut out: *mut c_void = std::ptr::null_mut();
    // SAFETY: `raw` is a handle open in this process; the result goes into this process. The out
    // pointer lives on this stack.
    let ok = unsafe { DuplicateHandle(GetCurrentProcess(), raw, GetCurrentProcess(), &mut out, 0, 0, DUPLICATE_SAME_ACCESS) } != 0;
    if !ok || out.is_null() {
        return None;
    }
    // SAFETY: a handle DuplicateHandle just created in this process; nobody else owns it.
    Some(unsafe { OwnedHandle::from_raw_handle(out.cast()) })
}

/// Whether process handle `h` has exited, and with what code: `None` still running, `Some(code)`
/// exited (`code` is the process's exit code; `None` inside means it couldn't be read).
fn process_exit(h: RawHandle) -> Option<Option<i32>> {
    // SAFETY: a process handle this process holds; a zero timeout only polls.
    if unsafe { WaitForSingleObject(h, 0) } != WAIT_OBJECT_0 {
        return None;
    }
    let mut code = 0u32;
    // SAFETY: the same handle, and a place for the code on this stack.
    let got = unsafe { GetExitCodeProcess(h, &mut code) } != 0;
    Some(got.then_some(code as i32))
}

/// Read size for pipes: one chunk per read, forwarded as is.
const CHUNK: usize = 32 * 1024;
/// How often a child's exit is polled.
const POLL: Duration = Duration::from_millis(50);
/// `CREATE_NEW_PROCESS_GROUP`: Ctrl-C at the agent's console does not reach job steps (the
/// counterpart of a Unix step's own process group).
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// `CREATE_SUSPENDED`: an agent's harness runs nothing until it's in the agent's job.
const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Spawn `cmd` in a new job object that is killed when its last handle closes.
fn spawn_job(cmd: &mut Command, new_group: bool) -> io::Result<GroupChild> {
    let mut b = cmd.group();
    b.kill_on_drop(true);
    if new_group {
        b.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    b.spawn()
}

/// Drain `pipe` on a thread of its own, handing each chunk to `sink` until it returns false
/// (the consumer is gone); then keep draining so the child never blocks on a full pipe.
fn pump(name: &str, mut pipe: impl Read + Send + 'static, mut sink: impl FnMut(&[u8]) -> bool + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        let mut wanted = true;
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if wanted {
                        wanted = sink(buf.get(..n).unwrap_or_default());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    })?;
    Ok(())
}

/// Read a whole pipe on a thread; the bytes arrive through the returned receiver.
fn collect(name: &str, pipe: Option<impl Read + Send + 'static>) -> io::Result<oneshot::Receiver<Vec<u8>>> {
    let (tx, rx) = oneshot::channel();
    match pipe {
        None => {
            let _ = tx.send(Vec::new());
        }
        Some(mut pipe) => {
            std::thread::Builder::new().name(name.to_owned()).spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send(buf);
            })?;
        }
    }
    Ok(rx)
}

#[allow(clippy::disallowed_methods)] // (e) polls a process on Windows; its handle's wait would tell
async fn wait_child(child: &mut GroupChild) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        crate::rt::sleep(POLL).await;
    }
}

#[allow(clippy::disallowed_methods)] // (e) polls a process on Windows; its handle's wait would tell
async fn wait_step(step: &Mutex<Step>) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = step.lock().child.try_wait()? {
            return Ok(status);
        }
        crate::rt::sleep(POLL).await;
    }
}

#[allow(clippy::disallowed_methods)] // (e) polls a process on Windows; its handle's wait would tell
async fn wait_held(held: &Mutex<Held>) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = held.lock().try_wait()? {
            return Ok(status);
        }
        crate::rt::sleep(POLL).await;
    }
}

/// Kill the job and reap the child (bounded, so a stuck kernel never pins us).
async fn kill_and_reap(child: &mut GroupChild) {
    let _ = child.kill();
    let _ = crate::rt::timeout(Duration::from_secs(10), wait_child(child)).await;
}

/// Wait for a pid to exit by polling `try_wait` (no pidfds on Windows).
#[allow(clippy::disallowed_methods)] // (e) polls a process on Windows; its handle's wait would tell
pub async fn wait_pid_exit<S>(_pid: u32, mut try_wait: impl FnMut() -> io::Result<Option<S>>) -> io::Result<S> {
    loop {
        if let Some(status) = try_wait()? {
            return Ok(status);
        }
        crate::rt::sleep(POLL).await;
    }
}

/// Run to completion and capture stdout/stderr, killing the job if it outlives `timeout`.
pub async fn output(mut cmd: Command, timeout: Duration) -> anyhow::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_job(&mut cmd, false).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let out = collect("kiln-pipe", child.inner().stdout.take())?;
    let err = collect("kiln-pipe", child.inner().stderr.take())?;
    let collected = crate::rt::timeout(timeout, async {
        let status = wait_child(&mut child).await?;
        let stdout = out.await.unwrap_or_default();
        let stderr = err.await.unwrap_or_default();
        Ok::<_, io::Error>(Output { status, stdout, stderr })
    })
    .await;
    match collected {
        Ok(r) => Ok(r?),
        Err(_) => {
            kill_and_reap(&mut child).await;
            anyhow::bail!("timed out after {timeout:?}")
        }
    }
}

/// Stream a child's stdout (stderr discarded). The stream ends at EOF; if the consumer goes away
/// first, the child's job is killed. The child is reaped either way.
#[allow(clippy::disallowed_methods)] // (e) polls a pipe on Windows; overlapped I/O would tell
pub fn stdout_stream(mut cmd: Command) -> anyhow::Result<crate::memory::queue::Receiver<Bytes>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = spawn_job(&mut cmd, false).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let out = child.inner().stdout.take().context("no stdout pipe")?;
    // 16 chunks read ahead of their reader, as the channel it was held, with no lock (goal 5, plan
    // 01a1a59c2aae): its thread waits for room.
    let (tx, rx) = crate::memory::queue::places::<Bytes>(&crate::memory::owners::CHANNELS, 16);
    let watch = tx.clone();
    pump("kiln-pipe", out, move |chunk| tx.send_blocking(Bytes::copy_from_slice(chunk)).is_ok())?;
    crate::rt::spawn(async move {
        loop {
            if watch.is_closed() {
                kill_and_reap(&mut child).await;
                return;
            }
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => crate::rt::sleep(POLL).await,
                Err(_) => {
                    kill_and_reap(&mut child).await;
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// A step's process, and the job of its own it was put in before it ran anything: everything it
/// starts is in the job too.
struct Step {
    child: std::process::Child,
    tree: crate::os::contain::Tree,
}

/// A running child in its own job object, stdout+stderr merged into one ordered-per-pipe stream
/// of chunks, with its exit status delivered asynchronously.
pub struct Running {
    /// The child's pid (Windows has no process groups; the job is held here instead).
    pub pgid: i32,
    /// Output chunks; closed once both pipes reached EOF.
    pub output: crate::memory::queue::Receiver<Vec<u8>>,
    /// Resolves with the exit code once the child is reaped.
    pub exit: oneshot::Receiver<Option<i32>>,
    job: Arc<Mutex<Step>>,
}

impl Running {
    /// Kill everything left in the child's job (best effort).
    pub fn kill_group(&self) {
        self.job.lock().tree.kill();
    }

    /// Its job: every process the step started, however that's parented (docs/ci.md W26-R22).
    pub fn tree(&self) -> crate::os::contain::Tree {
        self.job.lock().tree.clone()
    }

    /// The exit code (`None` when the wait failed).
    pub async fn wait(&mut self) -> Option<i32> {
        (&mut self.exit).await.ok().flatten()
    }
}

/// Spawn `cmd` in a new job object with piped output: created suspended, put in a job of its own,
/// then let run, so nothing it starts is outside the job.
pub fn spawn_group(mut cmd: Command) -> anyhow::Result<Running> {
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::process::CommandExt as _;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    let tree = crate::os::contain::Tree::new().ok_or_else(|| anyhow::anyhow!("spawn {:?}: no job object to hold it", cmd.get_program()))?;
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    if !tree.assign(child.as_raw_handle()) {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("spawn {:?}: it couldn't be put in its job", cmd.get_program());
    }
    crate::os::contain::resume(child.id());
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    // 64 chunks of both pipes, as the channel it was held, with no lock (goal 5, plan 01a1a59c2aae).
    let (tx, rx) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, 64);
    let out = child.stdout.take();
    let err = child.stderr.take();
    if let Some(p) = out {
        let tx = tx.clone();
        pump("kiln-step-out", p, move |c| tx.send_blocking(c.to_vec()).is_ok())?;
    }
    if let Some(p) = err {
        let tx = tx.clone();
        pump("kiln-step-err", p, move |c| tx.send_blocking(c.to_vec()).is_ok())?;
    }
    drop(tx);
    let job = Arc::new(Mutex::new(Step { child, tree }));
    let (exit_tx, exit_rx) = oneshot::channel();
    let waiter = Arc::clone(&job);
    crate::rt::spawn(async move {
        let code = wait_step(&waiter).await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(Running { pgid, output: rx, exit: exit_rx, job })
}

/// Windows has no SIGTERM for a job: kill it at once, then wait up to `grace` for the reap.
pub async fn terminate_group(running: &mut Running, grace: Duration) {
    running.kill_group();
    let _ = crate::rt::timeout(grace, running.wait()).await;
}

/// GitHub's cancel sequence has no Windows equivalent for a job object: kill it, then wait.
pub async fn interrupt_group(running: &mut Running, int_grace: Duration, term_grace: Duration) {
    terminate_group(running, int_grace + term_grace).await;
}

/// Kill the process tree rooted at `pid` (`taskkill /T /F`; a PTY step's shell is not in a job,
/// since ConPTY spawns it). There is no graceful stop on Windows, so `grace` is unused.
pub async fn terminate_tree(pid: i32, _grace: Duration) {
    if pid <= 0 {
        return;
    }
    let _ = crate::rt::blocking(move || taskkill_tree(u32::try_from(pid).unwrap_or(0))).await;
}

/// `taskkill /PID <pid> /T /F`, quietly. Blocks briefly: call it off the shards.
pub fn taskkill_tree(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Kill the tree rooted at `pid` if that process still carries `marker` (`KEY=value`) in its
/// environment, so a recycled pid is never touched. (Job steps die with the agent anyway; this
/// catches PTY steps, whose shells ConPTY spawned outside a job.)
pub fn kill_orphan_group(pid: i32, marker: &str) -> bool {
    let Ok(pid) = u32::try_from(pid) else { return false };
    pid > 0 && crate::os::metrics::process_has_env(pid, marker) && taskkill_tree(pid)
}

/// A child kiln talks to over all three pipes (a harness in its structured mode), in a job object
/// so stopping it stops everything it started. The pipes are served by threads.
pub struct Duplex {
    pub pid: u32,
    /// Bytes for its stdin; dropping every sender closes stdin.
    pub stdin: crate::memory::queue::Sender<Vec<u8>>,
    pub stdout: crate::memory::queue::Receiver<Vec<u8>>,
    pub stderr: crate::memory::queue::Receiver<Vec<u8>>,
    pub exit: oneshot::Receiver<Option<i32>>,
    /// What it used, from its process alone; none here: a run's job object counts its whole tree
    /// (`crate::os::contain::Group::usage`, docs/needs.md).
    pub usage: Option<oneshot::Receiver<crate::sense::run::Usage>>,
    job: Arc<Mutex<Held>>,
    /// Its pipes' handle values (stdin write, stdout read, stderr read), for [`Duplex::detach`]:
    /// the threads own the pipes, so these are kept to duplicate at handover without reaching into
    /// a thread.
    pipes: [i64; 3],
    /// The process came from another runner image (adopted): its exit is watched through its
    /// handle, and a re-handover keeps it foreign.
    foreign: bool,
    /// Set by [`Duplex::detach`] before it cancels the pump threads' reads, so they stop rather than
    /// loop back into another `ReadFile`.
    stopped: Arc<AtomicBool>,
    /// The stdout and stderr pump threads, to cancel and join at handover.
    readers: Vec<JoinHandle<()>>,
    /// The stdin thread, to join at handover once it has written everything queued.
    writer: Option<JoinHandle<()>>,
}

/// What holds a harness process and what it starts: a job of its own, or the job of its agent
/// session (`crate::os::contain`), which its next harness goes in too; or, once adopted across a
/// successor upgrade, the process and job handles the runner before this one handed over.
enum Held {
    Own(GroupChild),
    Agent(std::process::Child, crate::os::contain::Group),
    /// A harness this process took over from the runner before it ([`adopt_duplex`]): not this
    /// process's child, so its exit is seen through its process handle, and its job (kill-on-close)
    /// is held so nothing in it is taken while it runs.
    Adopted { process: OwnedHandle, job: OwnedHandle },
}

impl Held {
    fn kill(&mut self) {
        match self {
            Held::Own(c) => {
                let _ = c.kill();
            }
            Held::Agent(c, g) => {
                g.kill();
                let _ = c.kill();
            }
            Held::Adopted { job, .. } => {
                // SAFETY: a job handle this owns; ending it ends the harness and all it started.
                unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            }
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Held::Own(c) => c.try_wait(),
            Held::Agent(c, _) => c.try_wait(),
            // A process this one didn't start: its exit comes through its handle, not `wait`.
            Held::Adopted { process, .. } => Ok(process_exit(process.as_raw_handle()).map(|code| ExitStatus::from_raw(code.unwrap_or(0) as u32))),
        }
    }
}

/// A harness process handed from one runner image to the next across a successor upgrade (Windows
/// has no in-place exec): its process, job and three pipes carried as handle values. The runner
/// handing over duplicates them into the supervisor, which the fresh runner inherits, so the values
/// here are rewritten before [`adopt_duplex`] from what that process holds (`Host::restore`).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Detached {
    pub pid: u32,
    pub stdin: i64,
    pub stdout: i64,
    pub stderr: i64,
    /// The harness process handle (wait on it for the exit).
    #[serde(default)]
    pub process: i64,
    /// The harness job handle (kill-on-close): whoever holds it keeps the harness alive.
    #[serde(default)]
    pub job: i64,
    #[serde(default)]
    pub foreign: bool,
}

impl Detached {
    /// Nothing to clear across an exec: Windows carries a harness across a successor by inheriting
    /// duplicated handles, not by keeping descriptors across a re-exec.
    pub fn keep_across_exec(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An inherited or supervisor-duplicated handle value as a Windows `OwnedHandle`; `None` for a null
/// value (one that didn't come across).
fn owned(value: i64) -> Option<OwnedHandle> {
    let raw = i64_to_handle(value);
    // SAFETY: a handle the supervisor duplicated into this process and this process inherited at the
    // same value, owned by nobody else here.
    (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// A child's pipe (`ChildStdin`/`Stdout`/`Stderr`) as a `File`, so one spawned child and one adopted
/// across an upgrade are served the same way.
fn child_pipe<T: IntoRawHandle>(pipe: T) -> File {
    // SAFETY: a pipe handle this process owns, taken over whole from the child's stdio.
    unsafe { File::from_raw_handle(pipe.into_raw_handle()) }
}

impl Duplex {
    /// Windows has no SIGTERM for a job: kill it at once.
    pub fn terminate(&self, _grace: Duration) {
        self.job.lock().kill();
    }

    /// Let go of the harness without stopping it, for the runner after this one to take over
    /// ([`adopt_duplex`]) across a successor upgrade: its pump and stdin threads are stopped without
    /// closing the pipes (an aborted blocking `ReadFile` consumes nothing, so what it hadn't read
    /// waits in the pipe), and its pipe, process and job handles are duplicated to carry over.
    /// `None` when it can't be (a plain job of its own, or a handle Windows wouldn't duplicate): the
    /// harness is then left running and stopped by the caller, to resume from its record.
    pub async fn detach(&mut self) -> Option<Detached> {
        let (process_raw, job_raw) = {
            let held = self.job.lock();
            match &*held {
                Held::Agent(child, group) => (child.as_raw_handle(), group.job_handle()),
                Held::Adopted { process, job } => (process.as_raw_handle(), job.as_raw_handle()),
                // A plain job of its own isn't carried: its harness stops and resumes from its record.
                Held::Own(_) => return None,
            }
        };
        // Duplicate everything that must outlive the threads and the `Held`: the three pipes, the
        // process (to see its exit), and the job (kill-on-close, so holding it keeps the harness).
        let stdin = duplicate(i64_to_handle(self.pipes[0]))?;
        let stdout = duplicate(i64_to_handle(self.pipes[1]))?;
        let stderr = duplicate(i64_to_handle(self.pipes[2]))?;
        let process = duplicate(process_raw)?;
        let job = duplicate(job_raw)?;
        // Stop the readers, then cancel the `ReadFile` each is blocked in: the aborted read loses no
        // byte. The writer stops once stdin's channel closes, after it has written everything queued.
        self.stopped.store(true, Ordering::Release);
        for jh in &self.readers {
            // SAFETY: a pump thread's handle, valid while its JoinHandle lives here.
            unsafe { CancelSynchronousIo(jh.as_raw_handle()) };
        }
        self.stdin.close();
        let readers = std::mem::take(&mut self.readers);
        let writer = self.writer.take();
        // Joining threads can wait on a blocked read's cancellation; off the shards.
        if crate::rt::blocking(move || {
            for jh in readers {
                let _ = jh.join();
            }
            if let Some(w) = writer {
                let _ = w.join();
            }
        })
        .await
        .is_err()
        {
            tracing::warn!(pid = self.pid, "couldn't join a harness thread before handover");
        }
        Some(Detached {
            pid: self.pid,
            stdin: handle_to_i64(stdin.into_raw_handle()),
            stdout: handle_to_i64(stdout.into_raw_handle()),
            stderr: handle_to_i64(stderr.into_raw_handle()),
            process: handle_to_i64(process.into_raw_handle()),
            job: handle_to_i64(job.into_raw_handle()),
            foreign: self.foreign,
        })
    }
}

/// Take over a harness process the runner before this one handed over across a successor upgrade
/// ([`Duplex::detach`]): its pipes, its process (its exit is watched through this handle, as it is
/// not this process's child) and its job (held so nothing in it is taken). The handle values are
/// this process's: it inherited them from the supervisor, which duplicated them from the old runner.
pub fn adopt_duplex(d: Detached) -> anyhow::Result<Duplex> {
    let pipe = |value: i64, which: &str| -> anyhow::Result<File> {
        let raw = i64_to_handle(value);
        anyhow::ensure!(!raw.is_null(), "the harness {which} pipe didn't come across the upgrade");
        // SAFETY: a pipe handle the supervisor duplicated into this process and this process
        // inherited; owned by nobody else here.
        Ok(unsafe { File::from_raw_handle(raw) })
    };
    let stdin = pipe(d.stdin, "stdin")?;
    let stdout = pipe(d.stdout, "stdout")?;
    let stderr = pipe(d.stderr, "stderr")?;
    let process = owned(d.process).context("the harness process handle didn't come across the upgrade")?;
    let job = owned(d.job).context("the harness job handle didn't come across the upgrade")?;
    serve(d.pid, Some(stdin), Some(stdout), Some(stderr), true, Held::Adopted { process, job })
}

/// [`spawn_duplex`] in an agent's group of processes ([`crate::os::contain`]): created suspended,
/// put in the agent's job, then let run, so nothing it starts is outside the job. Its pipes are
/// drained by threads of their own here, on no shard, so `_home` (the shard Unix serves them on, the
/// one that takes what they read) places nothing.
pub fn spawn_duplex_in(mut cmd: Command, group: Option<&crate::os::contain::Group>, _home: Option<usize>) -> anyhow::Result<Duplex> {
    use std::os::windows::process::CommandExt as _;
    let Some(group) = group else { return spawn_duplex(cmd) };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let pid = child.id();
    if !group.assign(child.as_raw_handle()) {
        tracing::warn!(pid, "an agent's harness couldn't go in its job: what it starts is found by its mark");
    }
    crate::os::contain::resume(pid);
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    serve(pid, stdin.map(child_pipe), stdout.map(child_pipe), stderr.map(child_pipe), false, Held::Agent(child, group.clone()))
}

/// Spawn `cmd` with piped stdin/stdout/stderr in its own job.
pub fn spawn_duplex(mut cmd: Command) -> anyhow::Result<Duplex> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_job(&mut cmd, true).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let pid = child.id();
    let (stdin, stdout, stderr) = (child.inner().stdin.take(), child.inner().stdout.take(), child.inner().stderr.take());
    serve(pid, stdin.map(child_pipe), stdout.map(child_pipe), stderr.map(child_pipe), false, Held::Own(child))
}

/// A harness process's pipes served by threads, and its exit awaited. Each pipe's on a lock-free queue
/// with the bound its channel had (goal 5, plan 01a1a59c2aae, its top goal), its thread waiting for room
/// or for what to write as it did. A pump thread stops at EOF, at a cancelled read ([`Duplex::detach`]),
/// or when told to; its `File` closes this process's copy of the pipe as it ends.
fn serve(pid: u32, stdin: Option<File>, stdout: Option<File>, stderr: Option<File>, foreign: bool, held: Held) -> anyhow::Result<Duplex> {
    use std::io::Write as _;
    let pipes = [
        stdin.as_ref().map_or(0, |f| handle_to_i64(f.as_raw_handle())),
        stdout.as_ref().map_or(0, |f| handle_to_i64(f.as_raw_handle())),
        stderr.as_ref().map_or(0, |f| handle_to_i64(f.as_raw_handle())),
    ];
    let stopped = Arc::new(AtomicBool::new(false));
    let (out_tx, stdout_rx) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, 256);
    let (err_tx, stderr_rx) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, 64);
    let mut readers = Vec::new();
    if let Some(p) = stdout {
        readers.push(pump_handed("kiln-harness-out", p, Arc::clone(&stopped), move |c| out_tx.send_blocking(c.to_vec()).is_ok())?);
    }
    if let Some(p) = stderr {
        readers.push(pump_handed("kiln-harness-err", p, Arc::clone(&stopped), move |c| err_tx.send_blocking(c.to_vec()).is_ok())?);
    }
    let (in_tx, in_rx) = crate::memory::queue::unbounded::<Vec<u8>>(&crate::memory::owners::CHANNELS);
    let mut writer = None;
    if let Some(mut stdin) = stdin {
        writer = Some(
            std::thread::Builder::new().name("kiln-harness-in".to_owned()).spawn(move || {
                while let Ok(data) = in_rx.recv_blocking() {
                    if stdin.write_all(&data).and_then(|()| stdin.flush()).is_err() {
                        break;
                    }
                }
                // `stdin` (the write end) closes here; at a handover a duplicate holds it, so the
                // harness never sees stdin's end.
            })?,
        );
    }
    let job = Arc::new(Mutex::new(held));
    let (exit_tx, exit_rx) = oneshot::channel();
    let waiter = Arc::clone(&job);
    crate::rt::spawn(async move {
        let code = wait_held(&waiter).await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(Duplex { pid, stdin: in_tx, stdout: stdout_rx, stderr: stderr_rx, exit: exit_rx, usage: None, job, pipes, foreign, stopped, readers, writer })
}

/// Drain `pipe` on a thread of its own ([`pump`]), but as a harness pipe that may be handed over:
/// it stops at a cancelled read or once `stopped` is set, so [`Duplex::detach`] can take the pipe
/// back without the thread reading another byte of it. Returns its handle, to cancel and join.
fn pump_handed(name: &str, mut pipe: File, stopped: Arc<AtomicBool>, mut sink: impl FnMut(&[u8]) -> bool + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        let mut wanted = true;
        loop {
            if stopped.load(Ordering::Acquire) {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if wanted {
                        wanted = sink(buf.get(..n).unwrap_or_default());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // EOF, or `CancelSynchronousIo`'s aborted read at a handover.
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A harness process handed across a successor upgrade goes on: its output pipe is taken over
    /// mid-stream with no byte lost or doubled, and its exit code is seen though the process that
    /// adopts it never started it. The child flushes two lines, pauses, then writes a third and
    /// exits 7, so the detach falls between — the pump thread's `ReadFile` is cancelled while it
    /// waits for the third line, which must not lose or double the first two.
    #[test]
    fn a_harness_process_goes_on_after_it_is_handed_over() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            crate::os::contain::hold_agents();
            let session = format!("detach{}", std::process::id());
            let group = crate::os::contain::Group::of(&session).context("an agent needs a job to hand its harness over")?;
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-Command",
                "[Console]::Out.WriteLine('one'); [Console]::Out.WriteLine('two'); [Console]::Out.Flush(); Start-Sleep -Milliseconds 700; [Console]::Out.WriteLine('three'); exit 7",
            ]);
            let mut p = spawn_duplex_in(cmd, Some(&group), None)?;
            // The first two lines, flushed before the pause.
            let mut got = crate::rt::timeout(Duration::from_secs(20), p.stdout.recv()).await??;
            // Hand it over while the third line is still to come.
            let d = p.detach().await.context("couldn't detach the harness")?;
            assert_eq!(d.pid, p.pid);
            // Whatever the readers had already put in the channel before they stopped.
            while let Ok(Ok(c)) = crate::rt::timeout(Duration::from_millis(300), p.stdout.recv()).await {
                got.extend(c);
            }
            drop(p);
            let mut q = adopt_duplex(d)?;
            assert_eq!(q.pid, d.pid);
            while !got.windows(b"three".len()).any(|w| w == b"three") {
                got.extend(crate::rt::timeout(Duration::from_secs(20), q.stdout.recv()).await??);
            }
            let code = crate::rt::timeout(Duration::from_secs(20), &mut q.exit).await?.ok().flatten();
            group.kill();
            group.remove();
            assert_eq!(got, b"one\r\ntwo\r\nthree\r\n", "every line once, in order: {:?}", String::from_utf8_lossy(&got));
            assert_eq!(code, Some(7), "the adopted harness's exit code is seen");
            Ok(())
        })
    }
}
