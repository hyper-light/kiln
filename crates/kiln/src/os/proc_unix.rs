//! Child processes on Unix, without threads: `std::process` spawns, the pipes are made
//! non-blocking and read as hyper-rt readiness fires (epoll on Linux, kqueue on macOS), and exits
//! are awaited through a pidfd on Linux (readable once the child has exited; Linux ≥ 5.3) or a
//! short `try_wait` poll elsewhere. Kills go by pid / process group, so a cancelled or timed-out
//! child (and everything it started) is stopped and then reaped.

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use futures::channel::oneshot;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// Read size for pipes: one chunk per read, forwarded as is.
const CHUNK: usize = 32 * 1024;

/// Put an fd in non-blocking mode (pipes, the PTY master).
pub fn nonblocking(fd: &OwnedFd) -> io::Result<()> {
    rustix::io::ioctl_fionbio(fd, true).map_err(io::Error::from)
}

/// Wait until `fd` is readable (or writable), under the task's own waker however it is polled (through a
/// combinator's own waker: `crate::rt::native`). A refused wait degrades to a short sleep, so
/// callers keep making progress (just more slowly) rather than fail.
#[allow(clippy::disallowed_methods)] // (e) polls a child process's state; its exit (pidfd, waitpid on the reactor) would tell
async fn ready(fd: &OwnedFd, write: bool) {
    let raw = fd.as_raw_fd();
    let r = if write { crate::rt::native(hyper_rt::readiness::writable(raw)).await } else { crate::rt::native(hyper_rt::readiness::readable(raw)).await };
    if !matches!(r, Ok(Ok(()))) {
        crate::rt::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until `fd` is readable (a pidfd: until its process has exited).
pub async fn wait_readable(fd: &OwnedFd) {
    ready(fd, false).await;
}

/// One read from a non-blocking fd: `Ok(None)` at end of stream (EIO counts: a PTY master whose
/// other side has closed).
pub async fn read_chunk(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<Option<usize>> {
    loop {
        match rustix::io::read(fd, &mut *buf) {
            Ok(0) => return Ok(None),
            Ok(n) => return Ok(Some(n)),
            Err(rustix::io::Errno::AGAIN) => ready(fd, false).await,
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::IO) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Write all of `data` to a non-blocking fd.
pub async fn write_all(fd: &OwnedFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match rustix::io::write(fd, data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = data.get(n..).unwrap_or_default(),
            Err(rustix::io::Errno::AGAIN) => ready(fd, true).await,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn read_to_end(fd: Option<OwnedFd>) -> io::Result<Vec<u8>> {
    let Some(fd) = fd else { return Ok(Vec::new()) };
    nonblocking(&fd)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    while let Some(n) = read_chunk(&fd, &mut buf).await? {
        out.extend_from_slice(buf.get(..n).unwrap_or_default());
    }
    Ok(out)
}

/// A pidfd for `pid` (readable once it has exited): Linux ≥ 5.3 only.
#[cfg(target_os = "linux")]
pub fn pidfd(pid: u32) -> Option<OwnedFd> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .and_then(|p| rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()).ok())
}

/// No pidfds outside Linux: exits are polled.
#[cfg(not(target_os = "linux"))]
pub fn pidfd(_pid: u32) -> Option<OwnedFd> {
    None
}

/// Wait for a pid to exit without a thread: its pidfd turns readable at exit. Without a pidfd
/// (macOS, very old kernels) the status is polled every 50 ms.
#[allow(clippy::disallowed_methods)] // (e) polls a child process's state; its exit (pidfd, waitpid on the reactor) would tell
pub async fn wait_pid_exit<S>(pid: u32, mut try_wait: impl FnMut() -> io::Result<Option<S>>) -> io::Result<S> {
    let pidfd = pidfd(pid);
    loop {
        if let Some(status) = try_wait()? {
            return Ok(status);
        }
        match &pidfd {
            // At least once a second whatever the readiness says: a wake-up missed must cost a
            // second, not leave the exited child unreaped (a zombie) for good.
            Some(fd) => {
                let _ = crate::rt::timeout(Duration::from_secs(1), ready(fd, false)).await;
            }
            None => crate::rt::sleep(Duration::from_millis(50)).await,
        }
    }
}

async fn wait_child(child: &mut Child) -> io::Result<ExitStatus> {
    let pid = child.id();
    wait_pid_exit(pid, || child.try_wait()).await
}

/// Kill and reap a child that is no longer wanted (bounded, so a stuck kernel never pins us).
async fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = crate::rt::timeout(Duration::from_secs(10), wait_child(child)).await;
}

/// Run to completion and capture stdout/stderr, killing the child if it outlives `timeout`.
pub async fn output(mut cmd: Command, timeout: Duration) -> anyhow::Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    // Started from a shard, it would run on the shard's core (goal 9, plan 01a1a87b2cec step 4).
    crate::os::keep_child_off_shards(child.id());
    let out = child.stdout.take().map(OwnedFd::from);
    let err = child.stderr.take().map(OwnedFd::from);
    let collected = crate::rt::timeout(timeout, async {
        let (stdout, stderr, status) = futures::join!(read_to_end(out), read_to_end(err), wait_child(&mut child));
        Ok::<_, io::Error>(Output { status: status?, stdout: stdout?, stderr: stderr? })
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
/// first, the child is killed. The child is reaped either way.
pub fn stdout_stream(mut cmd: Command) -> anyhow::Result<crate::memory::queue::Receiver<Bytes>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    // Started from a shard, it would run on the shard's core (goal 9, plan 01a1a87b2cec step 4).
    crate::os::keep_child_off_shards(child.id());
    let out = OwnedFd::from(child.stdout.take().context("no stdout pipe")?);
    nonblocking(&out)?;
    // 16 chunks read ahead of their reader, as the channel it was held, with no lock (goal 5, plan
    // 01a1a59c2aae).
    let (tx, rx) = crate::memory::queue::places::<Bytes>(&crate::memory::owners::CHANNELS, 16);
    crate::rt::spawn(async move {
        let mut buf = vec![0u8; CHUNK * 2];
        let mut consumer_gone = false;
        while let Ok(Some(n)) = read_chunk(&out, &mut buf).await {
            if tx.send(Bytes::copy_from_slice(buf.get(..n).unwrap_or_default())).await.is_err() {
                consumer_gone = true;
                break;
            }
        }
        drop(out);
        if consumer_gone || crate::rt::timeout(Duration::from_secs(60), wait_child(&mut child)).await.is_err() {
            kill_and_reap(&mut child).await;
        }
    });
    Ok(rx)
}

/// A running child in its own process group, stdout+stderr merged into one ordered-per-pipe
/// stream of chunks, with its exit status delivered asynchronously.
pub struct Running {
    /// The child's pid, which is also its process-group id.
    pub pgid: i32,
    /// Output chunks; closed once both pipes reached EOF.
    pub output: crate::memory::queue::Receiver<Vec<u8>>,
    /// Resolves with the exit code (`None`: killed by a signal) once the child is reaped.
    pub exit: oneshot::Receiver<Option<i32>>,
}

impl Running {
    /// Kill whatever is left in the child's process group (best effort).
    pub fn kill_group(&self) {
        kill_group(self.pgid);
    }

    /// The exit code (`None` when killed by a signal or the wait failed).
    pub async fn wait(&mut self) -> Option<i32> {
        (&mut self.exit).await.ok().flatten()
    }
}

/// Spawn `cmd` in a new process group with piped output.
pub fn spawn_group(mut cmd: Command) -> anyhow::Result<Running> {
    use std::os::unix::process::CommandExt as _;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    // Started from a shard, it would run on the shard's core (goal 9, plan 01a1a87b2cec step 4).
    crate::os::keep_child_off_shards(child.id());
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    let pipes = [child.stdout.take().map(OwnedFd::from), child.stderr.take().map(OwnedFd::from)];
    // 64 chunks of both pipes, as the channel it was held, with no lock (goal 5, plan 01a1a59c2aae).
    let (tx, rx) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, 64);
    for fd in pipes.into_iter().flatten() {
        if let Err(e) = nonblocking(&fd) {
            tracing::warn!(error = %e, "cannot make a child pipe non-blocking; output dropped");
            continue;
        }
        let tx = tx.clone();
        crate::rt::spawn(async move {
            let mut buf = vec![0u8; CHUNK];
            while let Ok(Some(n)) = read_chunk(&fd, &mut buf).await {
                if tx.send(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).await.is_err() {
                    return;
                }
            }
        });
    }
    drop(tx);
    let (exit_tx, exit_rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let code = wait_child(&mut child).await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(Running { pgid, output: rx, exit: exit_rx })
}

/// SIGTERM the group, give it `grace`, then SIGKILL whatever is left.
pub async fn terminate_group(running: &mut Running, grace: Duration) {
    if running.pgid <= 0 {
        return;
    }
    let group = Pid::from_raw(running.pgid);
    let _ = killpg(group, Signal::SIGTERM);
    if crate::rt::timeout(grace, running.wait()).await.is_err() {
        let _ = killpg(group, Signal::SIGKILL);
    }
}

/// GitHub's cancel (docs/ci.md W05-R18): SIGINT to the group, SIGTERM after `int_grace`, SIGKILL
/// after `term_grace` more.
pub async fn interrupt_group(running: &mut Running, int_grace: Duration, term_grace: Duration) {
    if running.pgid <= 0 {
        return;
    }
    let group = Pid::from_raw(running.pgid);
    let _ = killpg(group, Signal::SIGINT);
    if crate::rt::timeout(int_grace, running.wait()).await.is_ok() {
        return;
    }
    let _ = killpg(group, Signal::SIGTERM);
    if crate::rt::timeout(term_grace, running.wait()).await.is_err() {
        let _ = killpg(group, Signal::SIGKILL);
    }
}

/// Kill a whole process group immediately (best effort).
pub fn kill_group(pgid: i32) {
    if pgid > 0 {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// SIGTERM the process group led by `pgid`, wait `grace`, then SIGKILL it (a PTY step's shell is
/// its session and group leader).
#[allow(clippy::disallowed_methods)] // (e) SIGTERM's grace before SIGKILL: an operation's deadline
pub async fn terminate_tree(pgid: i32, grace: Duration) {
    if pgid <= 0 {
        return;
    }
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
    crate::rt::sleep(grace).await;
    let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
}

/// Kill the process group `pgid` if its leader still carries `marker` (`KEY=value`) in its
/// environment, so a recycled pid is never touched. Linux reads `/proc/<pid>/environ`; macOS asks
/// the kernel through sysinfo.
pub fn kill_orphan_group(pgid: i32, marker: &str) -> bool {
    if pgid <= 0 {
        return false;
    }
    leader_has_env(pgid, marker) && killpg(Pid::from_raw(pgid), Signal::SIGKILL).is_ok()
}

#[cfg(target_os = "linux")]
fn leader_has_env(pid: i32, marker: &str) -> bool {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    environ.split(|b| *b == 0).any(|kv| kv == marker.as_bytes())
}

#[cfg(not(target_os = "linux"))]
fn leader_has_env(pid: i32, marker: &str) -> bool {
    u32::try_from(pid).is_ok_and(|pid| crate::os::metrics::process_has_env(pid, marker))
}

/// A child kiln talks to over all three pipes (a harness in its structured mode), in its own
/// process group so stopping it stops everything it started.
pub struct Duplex {
    /// The child's pid, which is also its process-group id.
    pub pid: u32,
    /// Bytes for its stdin; dropping every sender closes stdin.
    pub stdin: crate::memory::queue::Sender<Vec<u8>>,
    /// Its stdout, chunk by chunk; closed at EOF.
    pub stdout: crate::memory::queue::Receiver<Vec<u8>>,
    /// Its stderr, chunk by chunk; closed at EOF.
    pub stderr: crate::memory::queue::Receiver<Vec<u8>>,
    /// The exit code once reaped (`None`: killed by a signal).
    pub exit: oneshot::Receiver<Option<i32>>,
    /// What it and the descendants it waited for used, as `wait4` returned it when it was reaped
    /// (docs/needs.md); none for a process this one didn't start.
    pub usage: Option<oneshot::Receiver<crate::sense::run::Usage>>,
    /// The pipes' descriptors (stdin, stdout, stderr), for [`Duplex::detach`].
    fds: [i32; 3],
    /// The process came from another runner: its exit is watched, not reaped.
    foreign: bool,
    /// Taken by [`Duplex::detach`], `stopped` set first, to stop the readers: they read nothing
    /// more. Dropped otherwise (the `Duplex` let go of with its stdout taken elsewhere), the
    /// readers read on to the end, as they always did.
    stop: Option<crate::memory::queue::Sender<()>>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The writer has written everything sent to it, once stdin's sender closes.
    written: Option<oneshot::Receiver<()>>,
    /// Closed once both readers have ended: everything they read is in `stdout` and `stderr`.
    read: Option<crate::memory::queue::Receiver<()>>,
    /// The shard its stdout's reader read its first chunk on (`usize::MAX` before it read one, or off every shard).
    #[cfg(test)]
    read_on: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Held by a boundary, no process of this machine's (a microVM's member, kiln plan step 17): its
    /// pipes are the boundary's run's, and so are its end and its stop ([`Duplex::held`]).
    boundary: Option<std::sync::Arc<crate::sandbox::held::Held>>,
}

/// A harness process let go of by one image of this process for the next (an in-place upgrade):
/// its pid and its pipes' descriptors, kept open across the exec.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Detached {
    pub pid: u32,
    pub stdin: i32,
    pub stdout: i32,
    pub stderr: i32,
    /// The process is another process's child (handed over by another runner of this machine,
    /// blue/green): its exit is watched, not reaped, and how it ended isn't known.
    #[serde(default)]
    pub foreign: bool,
}

impl Detached {
    /// Keep its pipes' descriptors open across the exec that comes next (clear close-on-exec).
    /// Only just before it: a process started while they're kept would hold them for good (a
    /// harness whose stdin never closes).
    pub fn keep_across_exec(&self) -> io::Result<()> {
        use std::os::fd::BorrowedFd;
        for raw in [self.stdin, self.stdout, self.stderr] {
            #[allow(unsafe_code)]
            // SAFETY: the copies `Duplex::detach` made, which nothing in this image closes; the
            // borrow only lives for the call.
            let fd = unsafe { BorrowedFd::borrow_raw(raw) };
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
        }
        Ok(())
    }
}

impl Duplex {
    /// Let go of the process without stopping it, for this process's next image to take over
    /// ([`adopt_duplex`]): nothing more is read from its pipes (what was read is still in `stdout`
    /// and `stderr`, to drain), everything sent to its stdin is written, and its pipes' descriptors
    /// are copied (kept open across the exec just before it: [`Detached::keep_across_exec`]).
    /// `None` when they can't be (the process is then left as it is, still running).
    pub async fn detach(&mut self) -> Option<Detached> {
        // A boundary's run has no descriptors of this process's to carry across an exec: it stops,
        // and starts again in the next image, its conversation going on.
        if self.boundary.is_some() {
            return None;
        }
        let copy = |raw: i32| -> Option<OwnedFd> {
            use std::os::fd::BorrowedFd;
            #[allow(unsafe_code)]
            // SAFETY: `raw` is one of this child's pipes, still owned (open) by its reader or
            // writer task: those only end after the copies are made.
            let fd = unsafe { BorrowedFd::borrow_raw(raw) };
            rustix::io::fcntl_dupfd_cloexec(fd, 3).ok()
        };
        let input = copy(self.fds[0])?;
        let out = copy(self.fds[1])?;
        let err = copy(self.fds[2])?;
        self.stopped.store(true, std::sync::atomic::Ordering::Release);
        self.stop.take();
        self.stdin.close();
        if let Some(w) = self.written.take() {
            let _ = crate::rt::timeout(Duration::from_secs(5), w).await;
        }
        // A reader part-way through a read puts what it read into its channel before it ends:
        // waited for, so nothing read after this returns is lost with the `Duplex`.
        if let Some(r) = self.read.take() {
            let _ = crate::rt::timeout(Duration::from_secs(5), r.recv()).await;
        }
        use std::os::fd::IntoRawFd as _;
        Some(Detached { pid: self.pid, stdin: input.into_raw_fd(), stdout: out.into_raw_fd(), stderr: err.into_raw_fd(), foreign: self.foreign })
    }
    /// SIGTERM the group, then SIGKILL whatever is left after `grace` (without waiting here). Held by
    /// a boundary, it's ended as its boundary ends a run ([`crate::sandbox::held::Held::terminate`]).
    #[allow(clippy::disallowed_methods)] // (e) SIGTERM's grace before SIGKILL: an operation's deadline
    pub fn terminate(&self, grace: Duration) {
        if let Some(held) = &self.boundary {
            held.terminate(grace);
            return;
        }
        let Ok(pgid) = i32::try_from(self.pid) else { return };
        if pgid <= 0 {
            return;
        }
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGTERM);
        crate::rt::spawn(async move {
            crate::rt::sleep(grace).await;
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
        });
    }
}

impl Duplex {
    /// A harness a boundary holds, from its run's pipes and end ([`crate::sandbox::held::pump`]):
    /// no process of this machine's, so no pid ([`Duplex::host_pid`]), nothing to hand over, and its
    /// stop its boundary's.
    pub fn held(parts: crate::sandbox::held::Parts) -> Duplex {
        Duplex {
            pid: 0,
            stdin: parts.stdin,
            stdout: parts.stdout,
            stderr: parts.stderr,
            exit: parts.exit,
            usage: None,
            fds: [-1; 3],
            foreign: false,
            stop: None,
            stopped: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            written: None,
            read: None,
            #[cfg(test)]
            read_on: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            boundary: Some(parts.held),
        }
    }

    /// Its process on this machine, where it is one; none for a harness a boundary holds.
    pub fn host_pid(&self) -> Option<u32> {
        self.boundary.is_none().then_some(self.pid)
    }

    /// The boundary's run that holds it, where one does (a microVM's member): its name there, its
    /// image and the link its stdio rides.
    pub fn boundary(&self) -> Option<std::sync::Arc<crate::sandbox::held::Held>> {
        self.boundary.clone()
    }
}

/// Spawn `cmd` with piped stdin/stdout/stderr in a new process group.
pub fn spawn_duplex(cmd: Command) -> anyhow::Result<Duplex> {
    spawn_duplex_at(cmd, None)
}

/// `fut` as a task of its own on shard `home` of the runtime's (`crate::rt::spawn_owner`) where one's named, else
/// where `crate::rt::spawn` puts it: on the caller's shard, or, off every shard, the next round-robin.
#[track_caller]
fn spawn_at<F: Future<Output = ()> + Send + 'static>(home: Option<usize>, fut: F) {
    match home {
        Some(shard) => crate::rt::spawn_owner(shard, fut),
        None => drop(crate::rt::spawn(fut)),
    }
}

/// [`spawn_duplex`], its exit awaited and its pipes served on shard `home` where one's named (the shard of the task
/// that takes what they read: goal 10, plan 01a1a87b2dae step 5), else where `crate::rt::spawn` puts them.
fn spawn_duplex_at(mut cmd: Command, home: Option<usize>) -> anyhow::Result<Duplex> {
    use std::os::unix::process::CommandExt as _;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    // Started from a shard, it would run on the shard's core (goal 9, plan 01a1a87b2cec step 4).
    crate::os::keep_child_off_shards(child.id());
    let pid = child.id();
    let input = OwnedFd::from(child.stdin.take().context("no stdin pipe")?);
    let out = OwnedFd::from(child.stdout.take().context("no stdout pipe")?);
    let err = OwnedFd::from(child.stderr.take().context("no stderr pipe")?);
    let (exit_tx, exit_rx) = oneshot::channel();
    let (usage_tx, usage_rx) = oneshot::channel();
    spawn_at(home, async move {
        // Reaped by `wait4`, for what it used; `child` is only kept until then.
        let reaped = match i32::try_from(pid) {
            Ok(p) => wait_pid_exit(pid, || reap(p)).await.ok(),
            Err(_) => wait_child(&mut child).await.ok().map(|s| (s.code(), crate::sense::run::Usage::default())),
        };
        drop(child);
        let code = reaped.and_then(|(code, usage)| {
            let _ = usage_tx.send(usage);
            code
        });
        let _ = exit_tx.send(code);
    });
    let mut d = pump(pid, input, out, err, exit_rx, false, home)?;
    d.usage = Some(usage_rx);
    Ok(d)
}

/// Reaps child `pid` if it has exited: its exit code (none if a signal ended it) and what it and
/// the descendants it waited for used (`wait4`'s rusage). Linux counts blocks in 512-byte units of
/// storage I/O (`task_io_get_inblock`); macOS counts block operations.
#[allow(unsafe_code)]
fn reap(pid: i32) -> io::Result<Option<(Option<i32>, crate::sense::run::Usage)>> {
    use nix::libc;
    let mut status: libc::c_int = 0;
    // SAFETY: rusage is plain integers, for which zero is a value.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a child's pid, places for its status and its rusage.
    let r = unsafe { libc::wait4(pid, &raw mut status, libc::WNOHANG, &raw mut ru) };
    if r == 0 {
        return Ok(None);
    }
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(e) };
    }
    let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
    let us = |t: libc::timeval| u64::try_from(t.tv_sec).unwrap_or(0).saturating_mul(1_000_000).saturating_add(u64::try_from(t.tv_usec).unwrap_or(0));
    let (inb, outb) = (u64::try_from(ru.ru_inblock).unwrap_or(0), u64::try_from(ru.ru_oublock).unwrap_or(0));
    let linux = cfg!(any(target_os = "linux", target_os = "android"));
    let usage = crate::sense::run::Usage {
        cpu_us: Some(us(ru.ru_utime).saturating_add(us(ru.ru_stime))),
        read_bytes: linux.then(|| inb.saturating_mul(512)),
        write_bytes: linux.then(|| outb.saturating_mul(512)),
        ops: (!linux).then(|| inb.saturating_add(outb)),
        // Linux counts it in KiB, macOS in bytes.
        peak_rss: u64::try_from(ru.ru_maxrss).ok().map(|m| if linux { m.saturating_mul(1024) } else { m }),
        ..crate::sense::run::Usage::default()
    };
    Ok(Some((code, usage)))
}

/// [`spawn_duplex`] in an agent's group of processes ([`crate::os::contain`]): the process joins
/// it before it runs anything, so everything it starts is in it too. Its pipes are served on shard
/// `home` where one's named ([`spawn_duplex_at`]).
pub fn spawn_duplex_in(mut cmd: Command, group: Option<&crate::os::contain::Group>, home: Option<usize>) -> anyhow::Result<Duplex> {
    if let Some(g) = group {
        g.enter(&mut cmd);
    }
    let d = spawn_duplex_at(cmd, home)?;
    if let Some(g) = group {
        g.started(d.pid);
    }
    Ok(d)
}

/// Take over a harness process the previous image of this process let go of ([`Duplex::detach`]):
/// its pipes by their descriptors, and its exit (it is still this process's child).
pub fn adopt_duplex(d: Detached) -> anyhow::Result<Duplex> {
    let take = |raw: i32| crate::os::reclaim_fd(raw).with_context(|| format!("descriptor {raw} didn't survive the upgrade"));
    let (input, out, err) = (take(d.stdin)?, take(d.stdout)?, take(d.stderr)?);
    // Not handed to anything this image starts.
    for fd in [&input, &out, &err] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    let pid = d.pid;
    let (exit_tx, exit_rx) = oneshot::channel();
    if d.foreign {
        crate::rt::spawn(async move {
            wait_foreign_exit(pid).await;
            let _ = exit_tx.send(None);
        });
        return pump(pid, input, out, err, exit_rx, d.foreign, None);
    }
    crate::rt::spawn(async move {
        let p = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw);
        let status = wait_pid_exit(pid, || match p {
            Some(p) => match rustix::process::waitpid(Some(p), rustix::process::WaitOptions::NOHANG) {
                Ok(s) => Ok(s.map(|(_, s)| s.exit_status())),
                // Reaped already (it ended while it was handed over): how isn't known.
                Err(rustix::io::Errno::CHILD) => Ok(Some(None)),
                Err(e) => Err(io::Error::from(e)),
            },
            None => Ok(Some(None)),
        })
        .await;
        let _ = exit_tx.send(status.ok().flatten());
    });
    pump(pid, input, out, err, exit_rx, d.foreign, None)
}

/// Wait for `pid` to end when it isn't this process's child (another runner of this machine
/// started it and handed it over): its pidfd says so on Linux; elsewhere it is polled, with its
/// start time checked so a recycled pid isn't taken for it.
pub async fn wait_foreign_exit(pid: u32) {
    wait_gone(pid, None).await;
}

/// [`wait_foreign_exit`] for the process `pid` that started at `born` ([`started_at`]), as a record
/// kept it (a run whose work outlived the runner process that started it): one with its pid that
/// started at another time is another process, and `pid`'s ended.
pub async fn wait_exit_of(pid: u32, born: u64) {
    wait_gone(pid, Some(born)).await;
}

#[allow(clippy::disallowed_methods)] // (e) polls a process's state where no pidfd says its exit
async fn wait_gone(pid: u32, born: Option<u64>) {
    if let Some(fd) = pidfd(pid) {
        // Its pid given out again before the pidfd was opened: the process asked for is gone.
        if born.is_some_and(|b| started_at(pid) != Some(b)) {
            return;
        }
        // Readable once it has ended (as a zombie, if its parent hasn't reaped it yet).
        loop {
            if crate::rt::timeout(Duration::from_secs(5), ready(&fd, false)).await.is_ok() {
                return;
            }
            if !crate::os::pid_alive(pid) {
                return;
            }
        }
    }
    let born = born.or_else(|| started_at(pid));
    loop {
        if !crate::os::pid_alive(pid) || started_at(pid) != born {
            return;
        }
        crate::rt::sleep(Duration::from_millis(500)).await;
    }
}

/// When `pid` started (seconds since the epoch), to tell it from a later process with its pid.
pub fn started_at(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[p]), false, ProcessRefreshKind::nothing());
    sys.process(p).map(sysinfo::Process::start_time)
}

/// Serve a harness process's pipes: a reader for stdout and one for stderr, a writer for stdin. Each on a
/// lock-free queue with the bound its channel had (goal 5, plan 01a1a59c2aae, its top goal): an
/// async-channel's wait took event-listener's `Mutex`, a reader's shard sending each chunk and the
/// session's taking it on its own. Each on shard `home` where one's named ([`spawn_at`]).
fn pump(pid: u32, input: OwnedFd, out: OwnedFd, err: OwnedFd, exit: oneshot::Receiver<Option<i32>>, foreign: bool, home: Option<usize>) -> anyhow::Result<Duplex> {
    for fd in [&input, &out, &err] {
        nonblocking(fd)?;
    }
    let fds = [input.as_raw_fd(), out.as_raw_fd(), err.as_raw_fd()];
    let (stop_tx, stop_rx) = crate::memory::queue::places::<()>(&crate::memory::owners::CHANNELS, 1);
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Each reader holds a sender while it runs: the channel closes once both have ended.
    let (reading_tx, read_rx) = crate::memory::queue::places::<()>(&crate::memory::owners::CHANNELS, 1);
    #[cfg(test)]
    let read_on = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let reader = |fd: OwnedFd, cap: usize| {
        let (tx, rx) = crate::memory::queue::places::<Vec<u8>>(&crate::memory::owners::CHANNELS, cap);
        let (stop, stopped, reading) = (stop_rx.clone(), std::sync::Arc::clone(&stopped), reading_tx.clone());
        #[cfg(test)]
        let read_on = std::sync::Arc::clone(&read_on);
        spawn_at(home, async move {
            let _reading = reading;
            use futures::FutureExt as _;
            let mut buf = vec![0u8; CHUNK * 2];
            // Whether the `Duplex` is still there to stop the readers.
            let mut watching = true;
            loop {
                let read = if watching {
                    futures::select_biased! {
                        _ = stop.recv().fuse() => {
                            // Stopped (handed over): nothing more is read, so nothing read is
                            // lost. Only let go of: read on to the end.
                            if stopped.load(std::sync::atomic::Ordering::Acquire) {
                                return;
                            }
                            watching = false;
                            continue;
                        },
                        r = read_chunk(&fd, &mut buf).fuse() => r,
                    }
                } else {
                    read_chunk(&fd, &mut buf).await
                };
                let Ok(Some(n)) = read else { return };
                #[cfg(test)]
                let _ = read_on.compare_exchange(usize::MAX, crate::rt::shard_index().unwrap_or(usize::MAX), std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire);
                if tx.send(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).await.is_err() {
                    return;
                }
            }
        });
        rx
    };
    let stdout = reader(out, crate::proc::STDOUT_CHUNKS);
    let stderr = reader(err, crate::proc::STDERR_CHUNKS);
    drop(reading_tx);
    let (in_tx, in_rx) = crate::memory::queue::unbounded::<Vec<u8>>(&crate::memory::owners::CHANNELS);
    let (written_tx, written_rx) = oneshot::channel();
    spawn_at(home, async move {
        while let Ok(data) = in_rx.recv().await {
            if write_all(&input, &data).await.is_err() {
                break;
            }
        }
        let _ = written_tx.send(());
        // Dropping the fd here closes the child's stdin (unless it was handed over: a copy holds it).
    });
    Ok(Duplex {
        pid,
        stdin: in_tx,
        stdout,
        stderr,
        exit,
        usage: None,
        fds,
        foreign,
        stop: Some(stop_tx),
        stopped,
        written: Some(written_rx),
        read: Some(read_rx),
        #[cfg(test)]
        read_on,
        boundary: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10, plan 01a1a87b2dae step 5: a harness process asked for from a task on a shard (an agent session's) has its
    /// pipes read on that shard, every shard in turn, where the blocking pool's spawn dealt them round-robin; its output
    /// comes whole all the same.
    #[test]
    fn g10_s5_a_harness_pipes_are_read_on_the_shard_that_asked_for_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for shard in 0..crate::rt::shard_count() {
                let (tx, rx) = oneshot::channel();
                let asked = crate::rt::spawn_on(shard, async move {
                    let asked = async {
                        let mut cmd = Command::new("sh");
                        cmd.args(["-c", "echo read"]);
                        let p = crate::proc::spawn_duplex_in_pool(cmd, None).await?;
                        let first = p.stdout.recv().await?;
                        anyhow::Ok((first, p.read_on.load(std::sync::atomic::Ordering::Acquire), crate::rt::shard_index()))
                    };
                    let _ = tx.send(asked.await);
                });
                anyhow::ensure!(asked, "shard {shard} took no task");
                let (first, read_on, asked_on) = crate::rt::timeout(Duration::from_secs(30), rx).await.map_err(|_| anyhow::anyhow!("shard {shard}: no answer within 30 s"))???;
                anyhow::ensure!(first == b"read\n", "{:?}", String::from_utf8_lossy(&first));
                anyhow::ensure!(asked_on == Some(shard) && read_on == shard, "asked on {asked_on:?}, read on {read_on}, where shard {shard} asked");
            }
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side: a harness's pipes are lock-free queues
    /// with the bounds their channels had. Its stdout, not taken while it writes far more than 256 chunks,
    /// holds 256 and no more (its reader waits for room, and the process for its pipe); then taken, every
    /// byte comes once, in order; its stderr after it; and each ends once the process has.
    #[test]
    fn g5_a_harness_pipes_hold_their_bound_keep_their_order_and_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            // Past 256 of the reader's largest chunks (`CHUNK` * 2) and the pipe's own buffer.
            let lines = 3_000_000u32;
            let mut cmd = Command::new("sh");
            // Integers on every seq: macOS's (BSD's) prints with %g by default, 1000000 as `1e+06`.
            cmd.args(["-c", &format!("seq -f %.0f 1 {lines}; echo done >&2")]);
            let p = spawn_duplex(cmd)?;
            let full = crate::rt::timeout(Duration::from_secs(30), async {
                while p.stdout.len() < 256 {
                    crate::rt::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            anyhow::ensure!(full.is_ok(), "its stdout never filled: {} chunks", p.stdout.len());
            crate::rt::sleep(Duration::from_millis(200)).await;
            anyhow::ensure!(p.stdout.len() == 256, "{} chunks held, past the 256 it holds", p.stdout.len());
            let mut out = Vec::new();
            while let Ok(c) = crate::rt::timeout(Duration::from_secs(30), p.stdout.recv()).await.map_err(|_| anyhow::anyhow!("its stdout never ended"))? {
                out.extend(c);
            }
            let want: String = (1..=lines).map(|n| format!("{n}\n")).collect();
            anyhow::ensure!(out == want.as_bytes(), "{} bytes of {}, in order: {}", out.len(), want.len(), out.starts_with(want.as_bytes()));
            let mut err = Vec::new();
            while let Ok(c) = crate::rt::timeout(Duration::from_secs(10), p.stderr.recv()).await.map_err(|_| anyhow::anyhow!("its stderr never ended"))? {
                err.extend(c);
            }
            anyhow::ensure!(err == b"done\n", "{:?}", String::from_utf8_lossy(&err));
            Ok(())
        })
    }

    #[test]
    fn let_go_of_with_its_stdout_taken_a_process_is_read_to_the_end() -> anyhow::Result<()> {
        // As the CI git endpoint does: stdout taken, the rest of the `Duplex` dropped at once.
        crate::rt::block_on(async {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "for i in 1 2 3; do echo $i; sleep 0.2; done"]);
            let p = spawn_duplex(cmd)?;
            let out = p.stdout.clone();
            drop(p);
            let mut got = Vec::new();
            while let Ok(Ok(c)) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await {
                got.extend(c);
            }
            assert_eq!(got, b"1\n2\n3\n");
            Ok(())
        })
    }

    #[test]
    fn a_harness_process_goes_on_after_it_is_handed_over() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut p = spawn_duplex(Command::new("cat"))?;
            p.stdin.send(b"one\n".to_vec()).await?;
            assert_eq!(p.stdout.recv().await?, b"one\n");
            // Sent just before the hand-over: written, not lost.
            p.stdin.send(b"two\n".to_vec()).await?;
            let d = p.detach().await.context("couldn't detach")?;
            // Whatever was read before the readers stopped is still here; the rest waits in the pipe.
            let mut got = Vec::new();
            while let Ok(Ok(c)) = crate::rt::timeout(Duration::from_millis(500), p.stdout.recv()).await {
                got.extend(c);
            }
            drop(p);
            let mut q = adopt_duplex(d)?;
            assert_eq!(q.pid, d.pid);
            q.stdin.send(b"three\n".to_vec()).await?;
            while got.len() < b"two\nthree\n".len() {
                got.extend(crate::rt::timeout(Duration::from_secs(5), q.stdout.recv()).await??);
            }
            assert_eq!(got, b"two\nthree\n");
            // It ends when its stdin closes, as before.
            q.stdin.close();
            crate::rt::timeout(Duration::from_secs(5), &mut q.exit).await??;
            Ok(())
        })
    }

    #[test]
    fn a_foreign_harness_process_stays_foreign_when_handed_over_again() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "exec 3<&0; cat <&3 & child=$!; printf '%s\\n' \"$child\" >&2; wait \"$child\""]);
            let mut parent = spawn_duplex(cmd)?;
            struct Stop(i32);
            impl Drop for Stop {
                fn drop(&mut self) {
                    let _ = killpg(Pid::from_raw(self.0), Signal::SIGKILL);
                }
            }
            let _stop = Stop(i32::try_from(parent.pid)?);
            let said = crate::rt::timeout(Duration::from_secs(5), parent.stderr.recv()).await??;
            let pid: u32 = std::str::from_utf8(&said)?.trim().parse()?;
            let mut d = parent.detach().await.context("couldn't detach the parent's pipes")?;
            d.pid = pid;
            d.foreign = true;
            let mut p = adopt_duplex(d)?;
            p.stdin.send(b"one\n".to_vec()).await?;
            assert_eq!(crate::rt::timeout(Duration::from_secs(5), p.stdout.recv()).await??, b"one\n");
            let d = p.detach().await.context("couldn't detach the foreign process")?;
            assert!(d.foreign, "a process received from another runner is still not our child");
            drop(p);
            let mut q = adopt_duplex(d)?;
            q.stdin.send(b"two\n".to_vec()).await?;
            assert_eq!(crate::rt::timeout(Duration::from_secs(5), q.stdout.recv()).await??, b"two\n");
            assert!(matches!(q.exit.try_recv(), Ok(None)), "the foreign process is still running");
            q.stdin.close();
            crate::rt::timeout(Duration::from_secs(5), &mut q.exit).await??;
            crate::rt::timeout(Duration::from_secs(5), &mut parent.exit).await??;
            Ok(())
        })
    }
}
