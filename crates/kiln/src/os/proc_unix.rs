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

/// Wait until `fd` is readable (or writable). A refused wait degrades to a short sleep, so callers
/// keep making progress (just more slowly) rather than fail.
async fn ready(fd: &OwnedFd, write: bool) {
    let raw = fd.as_raw_fd();
    let r = if write { hyper_rt::readiness::writable(raw).await } else { hyper_rt::readiness::readable(raw).await };
    if r.is_err() {
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
pub fn stdout_stream(mut cmd: Command) -> anyhow::Result<async_channel::Receiver<Bytes>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let out = OwnedFd::from(child.stdout.take().context("no stdout pipe")?);
    nonblocking(&out)?;
    let (tx, rx) = async_channel::bounded::<Bytes>(16);
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
    pub output: async_channel::Receiver<Vec<u8>>,
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
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    let pipes = [child.stdout.take().map(OwnedFd::from), child.stderr.take().map(OwnedFd::from)];
    let (tx, rx) = async_channel::bounded::<Vec<u8>>(64);
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

/// Kill a whole process group immediately (best effort).
pub fn kill_group(pgid: i32) {
    if pgid > 0 {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// SIGTERM the process group led by `pgid`, wait `grace`, then SIGKILL it (a PTY step's shell is
/// its session and group leader).
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
    pub stdin: async_channel::Sender<Vec<u8>>,
    /// Its stdout, chunk by chunk; closed at EOF.
    pub stdout: async_channel::Receiver<Vec<u8>>,
    /// Its stderr, chunk by chunk; closed at EOF.
    pub stderr: async_channel::Receiver<Vec<u8>>,
    /// The exit code once reaped (`None`: killed by a signal).
    pub exit: oneshot::Receiver<Option<i32>>,
}

impl Duplex {
    /// SIGTERM the group, then SIGKILL whatever is left after `grace` (without waiting here).
    pub fn terminate(&self, grace: Duration) {
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

/// Spawn `cmd` with piped stdin/stdout/stderr in a new process group.
pub fn spawn_duplex(mut cmd: Command) -> anyhow::Result<Duplex> {
    use std::os::unix::process::CommandExt as _;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let pid = child.id();
    let input = OwnedFd::from(child.stdin.take().context("no stdin pipe")?);
    let out = OwnedFd::from(child.stdout.take().context("no stdout pipe")?);
    let err = OwnedFd::from(child.stderr.take().context("no stderr pipe")?);
    for fd in [&input, &out, &err] {
        nonblocking(fd)?;
    }
    let reader = |fd: OwnedFd, cap: usize| {
        let (tx, rx) = async_channel::bounded::<Vec<u8>>(cap);
        crate::rt::spawn(async move {
            let mut buf = vec![0u8; CHUNK * 2];
            while let Ok(Some(n)) = read_chunk(&fd, &mut buf).await {
                if tx.send(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default()).await.is_err() {
                    return;
                }
            }
        });
        rx
    };
    let stdout = reader(out, 256);
    let stderr = reader(err, 64);
    let (in_tx, in_rx) = async_channel::unbounded::<Vec<u8>>();
    crate::rt::spawn(async move {
        while let Ok(data) = in_rx.recv().await {
            if write_all(&input, &data).await.is_err() {
                break;
            }
        }
        // Dropping the fd here closes the child's stdin.
    });
    let (exit_tx, exit_rx) = oneshot::channel();
    crate::rt::spawn(async move {
        let code = wait_child(&mut child).await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(Duplex { pid, stdin: in_tx, stdout, stderr, exit: exit_rx })
}
