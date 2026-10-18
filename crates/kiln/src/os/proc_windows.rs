//! Child processes on Windows. hyper-rt's readiness covers sockets only (IOCP/AFD), so each
//! anonymous pipe is drained by a small dedicated thread that forwards chunks over a channel; a
//! bounded pool would be wrong here, because a pipe read lasts as long as the child writes and a
//! queued reader could deadlock a child blocked on a full pipe. Exits are a 50 ms `try_wait`
//! poll (non-blocking). Every child runs in its own job object (`command-group`) created with
//! `KILL_ON_JOB_CLOSE`: a kill terminates the whole tree, and if the agent dies the OS closes the
//! job and takes its children with it.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use command_group::{CommandGroup as _, GroupChild};
use futures::channel::oneshot;
use parking_lot::Mutex;

/// Read size for pipes: one chunk per read, forwarded as is.
const CHUNK: usize = 32 * 1024;
/// How often a child's exit is polled.
const POLL: Duration = Duration::from_millis(50);
/// `CREATE_NEW_PROCESS_GROUP`: Ctrl-C at the agent's console does not reach job steps (the
/// counterpart of a Unix step's own process group).
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

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

async fn wait_child(child: &mut GroupChild) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        crate::rt::sleep(POLL).await;
    }
}

async fn wait_shared(child: &Mutex<GroupChild>) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.lock().try_wait()? {
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
pub fn stdout_stream(mut cmd: Command) -> anyhow::Result<async_channel::Receiver<Bytes>> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = spawn_job(&mut cmd, false).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let out = child.inner().stdout.take().context("no stdout pipe")?;
    let (tx, rx) = async_channel::bounded::<Bytes>(16);
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

/// A running child in its own job object, stdout+stderr merged into one ordered-per-pipe stream
/// of chunks, with its exit status delivered asynchronously.
pub struct Running {
    /// The child's pid (Windows has no process groups; the job is held here instead).
    pub pgid: i32,
    /// Output chunks; closed once both pipes reached EOF.
    pub output: async_channel::Receiver<Vec<u8>>,
    /// Resolves with the exit code once the child is reaped.
    pub exit: oneshot::Receiver<Option<i32>>,
    job: Arc<Mutex<GroupChild>>,
}

impl Running {
    /// Kill everything left in the child's job (best effort).
    pub fn kill_group(&self) {
        let _ = self.job.lock().kill();
    }

    /// The exit code (`None` when the wait failed).
    pub async fn wait(&mut self) -> Option<i32> {
        (&mut self.exit).await.ok().flatten()
    }
}

/// Spawn `cmd` in a new job object with piped output.
pub fn spawn_group(mut cmd: Command) -> anyhow::Result<Running> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_job(&mut cmd, true).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    let (tx, rx) = async_channel::bounded::<Vec<u8>>(64);
    let out = child.inner().stdout.take();
    let err = child.inner().stderr.take();
    if let Some(p) = out {
        let tx = tx.clone();
        pump("kiln-step-out", p, move |c| tx.send_blocking(c.to_vec()).is_ok())?;
    }
    if let Some(p) = err {
        let tx = tx.clone();
        pump("kiln-step-err", p, move |c| tx.send_blocking(c.to_vec()).is_ok())?;
    }
    drop(tx);
    let job = Arc::new(Mutex::new(child));
    let (exit_tx, exit_rx) = oneshot::channel();
    let waiter = Arc::clone(&job);
    crate::rt::spawn(async move {
        let code = wait_shared(&waiter).await.ok().and_then(|s| s.code());
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
    pub stdin: async_channel::Sender<Vec<u8>>,
    pub stdout: async_channel::Receiver<Vec<u8>>,
    pub stderr: async_channel::Receiver<Vec<u8>>,
    pub exit: oneshot::Receiver<Option<i32>>,
    job: Arc<Mutex<GroupChild>>,
}

/// A harness process handed from one image of this process to the next: never on Windows, which
/// has no in-place upgrade.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Detached {
    pub pid: u32,
    pub stdin: i32,
    pub stdout: i32,
    pub stderr: i32,
}

impl Duplex {
    /// Windows has no SIGTERM for a job: kill it at once.
    pub fn terminate(&self, _grace: Duration) {
        let _ = self.job.lock().kill();
    }

    /// Never handed over on Windows (no in-place upgrade).
    #[allow(clippy::unused_async)]
    pub async fn detach(&mut self) -> Option<Detached> {
        None
    }
}

/// Never on Windows (no in-place upgrade).
pub fn adopt_duplex(_d: Detached) -> anyhow::Result<Duplex> {
    anyhow::bail!("Windows has no in-place upgrade to hand a process across")
}

/// Spawn `cmd` with piped stdin/stdout/stderr in its own job.
pub fn spawn_duplex(mut cmd: Command) -> anyhow::Result<Duplex> {
    use std::io::Write as _;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn_job(&mut cmd, true).with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let pid = child.id();
    let (out_tx, stdout) = async_channel::bounded::<Vec<u8>>(256);
    let (err_tx, stderr) = async_channel::bounded::<Vec<u8>>(64);
    if let Some(p) = child.inner().stdout.take() {
        pump("kiln-harness-out", p, move |c| out_tx.send_blocking(c.to_vec()).is_ok())?;
    }
    if let Some(p) = child.inner().stderr.take() {
        pump("kiln-harness-err", p, move |c| err_tx.send_blocking(c.to_vec()).is_ok())?;
    }
    let (in_tx, in_rx) = async_channel::unbounded::<Vec<u8>>();
    if let Some(mut stdin) = child.inner().stdin.take() {
        std::thread::Builder::new().name("kiln-harness-in".to_owned()).spawn(move || {
            while let Ok(data) = in_rx.recv_blocking() {
                if stdin.write_all(&data).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        })?;
    }
    let job = Arc::new(Mutex::new(child));
    let (exit_tx, exit_rx) = oneshot::channel();
    let waiter = Arc::clone(&job);
    crate::rt::spawn(async move {
        let code = wait_shared(&waiter).await.ok().and_then(|s| s.code());
        let _ = exit_tx.send(code);
    });
    Ok(Duplex { pid, stdin: in_tx, stdout, stderr, exit: exit_rx, job })
}
