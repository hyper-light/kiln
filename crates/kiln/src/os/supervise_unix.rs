//! `kiln supervise` on Unix: the child is signalled by pid (SIGHUP reload, SIGTERM stop,
//! SIGKILL), awaited through its pidfd (Linux) or a poll, and as PID 1 every orphan is reaped.
//! On macOS a runner that takes up a new binary hands its terminals to the supervisor first
//! ([`Handover`]), and the supervisor gives them to the fresh process it starts next.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

/// How the supervisor talks to its running child.
pub struct ChildControl {
    pid: Option<Pid>,
}

impl ChildControl {
    /// Spawn the supervised child (stdio inherited).
    pub fn spawn(cmd: &mut Command) -> std::io::Result<(Child, Self)> {
        let child = cmd.spawn()?;
        let pid = i32::try_from(child.id()).ok().map(Pid::from_raw);
        Ok((child, Self { pid }))
    }

    /// Take control of a child a previous image of this supervisor started (it re-executed itself
    /// in place; the child is still ours).
    pub fn adopt(pid: u32) -> Option<Self> {
        i32::try_from(pid).ok().map(|p| Self { pid: Some(Pid::from_raw(p)) })
    }

    pub fn pid(&self) -> Option<i32> {
        self.pid.map(Pid::as_raw)
    }

    /// Ask the child to reload its configuration (SIGHUP).
    pub fn reload(&self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGHUP);
        }
    }

    /// Ask the child to shut down gracefully (SIGTERM).
    pub fn stop(&mut self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGTERM);
        }
    }

    /// Kill the child outright (SIGKILL).
    pub async fn kill(&mut self) {
        if let Some(p) = self.pid {
            let _ = kill(p, Signal::SIGKILL);
        }
    }
}

/// Wait for the child without a thread and report how it ended: its exit code, if it exited,
/// and a description. Normally that is its pidfd. As PID 1 (containers) every orphan re-parented
/// to us must be reaped too, so a `waitpid(-1, WNOHANG)` sweep runs whenever the child's pidfd
/// fires and at least once a second; only our own child's status is reported. One reaper for
/// everything means no two waits race for a pid.
pub async fn wait_child(child: Child) -> (Option<i32>, String) {
    if std::process::id() == 1 { wait_as_pid1(child).await } else { wait_plain(child).await }
}

async fn wait_plain(mut child: Child) -> (Option<i32>, String) {
    let pid = child.id();
    match crate::os::proc::wait_pid_exit(pid, || child.try_wait()).await {
        Ok(status) => (status.code(), status.to_string()),
        Err(e) => (None, format!("wait error: {e}")),
    }
}

async fn wait_as_pid1(child: Child) -> (Option<i32>, String) {
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(0));
    let pidfd = crate::os::proc::pidfd(child.id());
    loop {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(st) if st.pid() == Some(pid) => return describe(st),
                Ok(_) | Err(Errno::EINTR) => {}
                Err(Errno::ECHILD) => return (None, "child vanished".to_owned()),
                Err(e) => return (None, format!("wait error: {e}")),
            }
        }
        match &pidfd {
            Some(fd) => {
                let _ = crate::rt::timeout(Duration::from_secs(1), crate::os::proc::wait_readable(fd)).await;
            }
            None => crate::rt::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn describe(st: WaitStatus) -> (Option<i32>, String) {
    match st {
        WaitStatus::Exited(_, code) => (Some(code), format!("exit status: {code}")),
        WaitStatus::Signaled(_, sig, _) => (None, format!("signal: {sig}")),
        other => (None, format!("{other:?}")),
    }
}

/// Spawn `cmd` detached from the terminal: its own process group.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt as _;
    cmd.process_group(0).spawn()
}

/// A supervised child on Unix is stopped by signals, which `main` already handles.
pub fn watch_supervisor(_shutdown: crate::rt::Shutdown) {}

/// `kiln uninstall` on Unix leaves running supervisors to the boot manager that started them
/// (`systemctl disable --now`, `launchctl bootout`); nothing to find here.
pub fn stop_supervisors(_subcommand: &str) -> usize {
    0
}

/// Stop these supervisors: SIGTERM, which each passes on to its child with the child's grace
/// period; SIGKILL to whatever is left after that.
pub fn stop(pids: &[u32]) {
    crate::os::stop_processes(pids, None, std::time::Duration::from_secs(30));
}

// ------------------------------------------------------------------ handing terminals over

/// The handover channel's environment: the descriptor number of the child's end and the
/// supervisor's pid. A kiln that a job or a terminal starts inherits the variables but not the
/// descriptor, so they count only while the parent is that supervisor.
const CHANNEL_FD: &str = "KILN_SUPERVISOR_FD";
const SUPERVISOR_PID: &str = "KILN_SUPERVISOR_PID";
/// The terminals a fresh runner takes over: their descriptor numbers, in the order the runner
/// before it listed them in its handover file.
const HANDED_FDS: &str = "KILN_HANDOVER_FDS";

/// The supervisor's end of the channel a runner hands its terminals over on before it exits to
/// take up a new binary ([`crate::os::UPGRADE_BY_SUCCESSOR`]): a Unix socket pair whose other end
/// the child inherits. The child sends `T` with each PTY master attached, then `E`
/// ([`to_supervisor`]), and exits with [`crate::supervise::HANDOVER_EXIT`]; a reader thread
/// collects the masters as they come, so the supervisor holds them once the child is gone.
pub struct Handover {
    theirs: Option<OwnedFd>,
    ours: Option<UnixStream>,
    collected: Option<futures::channel::oneshot::Receiver<Option<Vec<OwnedFd>>>>,
}

/// PTY masters a runner handed over before it exited: what the next one takes over.
pub struct Terminals(Vec<OwnedFd>);

impl Handover {
    /// A channel for the child `cmd` is about to start (its end inherited, its number and this
    /// supervisor's pid in its environment). `None` if the socket pair cannot be made: the child
    /// then runs without one, as under an older supervisor.
    pub fn for_child(cmd: &mut Command) -> Option<Self> {
        let (ours, theirs) = UnixStream::pair().ok()?;
        let theirs = OwnedFd::from(theirs);
        crate::os::keep_across_exec(&theirs).ok()?;
        cmd.env(CHANNEL_FD, theirs.as_raw_fd().to_string()).env(SUPERVISOR_PID, std::process::id().to_string());
        Some(Self { theirs: Some(theirs), ours: Some(ours), collected: None })
    }

    /// The child is running: close its end here (so its exit ends the channel) and start
    /// collecting what it sends.
    pub fn started(&mut self) {
        drop(self.theirs.take());
        let Some(sock) = self.ours.take() else { return };
        let (tx, rx) = futures::channel::oneshot::channel();
        let reader = std::thread::Builder::new().name("kiln-handover".into()).spawn(move || {
            let _ = tx.send(read_terminals(&sock));
        });
        if reader.is_ok() {
            self.collected = Some(rx);
        }
    }

    /// What the child handed over before it exited: `None` unless it finished doing so (an end
    /// mark after its masters), so a child that died midway leaves nothing half taken.
    pub async fn finish(mut self) -> Option<Terminals> {
        let rx = self.collected.take()?;
        match crate::rt::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(Some(fds))) => Some(Terminals(fds)),
            _ => None,
        }
    }
}

impl Terminals {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Give these masters to the child `cmd` starts: inherited, their numbers in its environment.
    /// Dropping `self` once it has started closes this process's copies.
    pub fn pass_to(&self, cmd: &mut Command) -> io::Result<()> {
        for fd in &self.0 {
            crate::os::keep_across_exec(fd)?;
        }
        let numbers: Vec<String> = self.0.iter().map(|fd| fd.as_raw_fd().to_string()).collect();
        cmd.env(HANDED_FDS, numbers.join(","));
        Ok(())
    }
}

/// Read a child's handover until it ends: each `T` must carry one descriptor, `E` ends it.
fn read_terminals(sock: &UnixStream) -> Option<Vec<OwnedFd>> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut fds = Vec::new();
    loop {
        let mut mark = [0u8; 1];
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let got = match rustix::net::recvmsg(sock, &mut [io::IoSliceMut::new(&mut mark)], &mut control, RecvFlags::empty()) {
            Ok(m) => m.bytes,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return None,
        };
        let mut attached = Vec::new();
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(received) = m {
                attached.extend(received);
            }
        }
        for fd in &attached {
            let _ = crate::os::close_on_exec(fd);
        }
        let [mark] = mark;
        match (got, mark) {
            (1, b'T') if attached.len() == 1 => fds.append(&mut attached),
            (1, b'E') if attached.is_empty() => return Some(fds),
            // The end of the channel before the end mark, or anything else: nothing to trust.
            _ => return None,
        }
    }
}

fn from_our_supervisor(var: &str) -> Option<String> {
    let pid: u32 = std::env::var(SUPERVISOR_PID).ok()?.parse().ok()?;
    if pid != std::os::unix::process::parent_id() {
        return None;
    }
    std::env::var(var).ok()
}

/// Whether this process is the one its supervisor started (the machine's installed runner): its
/// parent is the supervisor it was told of. What it starts inherits what it was told, not its
/// parent, so a test's runner started from an agent's shell isn't it.
pub fn started_by_supervisor() -> bool {
    static SUPERVISOR: OnceLock<Option<u32>> = OnceLock::new();
    SUPERVISOR.get_or_init(supervisor_pid).is_some_and(|pid| pid == std::os::unix::process::parent_id())
}

/// The supervisor this process was told of: as it names itself, or (a supervisor from before it
/// did) the process serving the control socket it was given.
fn supervisor_pid() -> Option<u32> {
    if let Some(pid) = std::env::var(SUPERVISOR_PID).ok().and_then(|p| p.parse().ok()) {
        return Some(pid);
    }
    #[cfg(target_os = "linux")]
    {
        let s = UnixStream::connect(std::env::var_os(crate::bluegreen::SUPERVISOR_ENV)?).ok()?;
        let cred = nix::sys::socket::getsockopt(&s, nix::sys::socket::sockopt::PeerCredentials).ok()?;
        u32::try_from(cred.pid()).ok()
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// This process's end of the handover channel, when the supervisor that started it offers one.
/// Taken once (the first call, early at start-up) and closed at exec from then on, so nothing
/// this process starts inherits it.
fn channel() -> Option<&'static Mutex<UnixStream>> {
    static CHANNEL: OnceLock<Option<Mutex<UnixStream>>> = OnceLock::new();
    CHANNEL
        .get_or_init(|| {
            let fd = crate::os::reclaim_fd(from_our_supervisor(CHANNEL_FD)?.parse().ok()?)?;
            crate::os::close_on_exec(&fd).ok()?;
            let sock = UnixStream::from(fd);
            let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
            Some(Mutex::new(sock))
        })
        .as_ref()
}

/// Take what the supervisor handed this process at start (call it first thing in a runner).
pub fn claim_inherited() {
    let _ = channel();
}

/// Whether this process can hand its terminals to its supervisor ([`to_supervisor`]).
pub fn has_supervisor_channel() -> bool {
    channel().is_some()
}

/// The descriptor numbers of the terminals the supervisor handed this process, in the order of
/// the handover file the runner before it wrote; `None` when it handed none.
pub fn handed_terminals() -> Option<Vec<i32>> {
    let list = from_our_supervisor(HANDED_FDS)?;
    Some(list.split(',').filter_map(|n| n.trim().parse().ok()).collect())
}

/// Hand these PTY masters to the supervisor, in order, then the end mark. The process exits with
/// [`crate::supervise::HANDOVER_EXIT`] next; the masters stay open in the supervisor meanwhile.
#[allow(unsafe_code)]
pub fn to_supervisor(masters: &[i32]) -> io::Result<()> {
    let ch = channel().ok_or_else(|| io::Error::other("no handover channel to the supervisor"))?;
    let sock = ch.lock().unwrap_or_else(PoisonError::into_inner);
    for &raw in masters {
        // SAFETY: each number is a PTY master one of this process's terminal sessions holds open,
        // and they all stay open (the sessions are not dropped) until this call returns.
        let fd = unsafe { BorrowedFd::borrow_raw(raw) };
        send_mark(&sock, b'T', &[fd])?;
    }
    send_mark(&sock, b'E', &[])
}

fn send_mark(sock: &UnixStream, mark: u8, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::other("descriptors do not fit one handover message"));
    }
    loop {
        match rustix::net::sendmsg(sock, &[io::IoSlice::new(&[mark])], &mut control, SendFlags::empty()) {
            Ok(1) => return Ok(()),
            Ok(_) => return Err(io::Error::other("the handover message was not sent")),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(io::Error::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsFd as _;

    use super::*;

    #[test]
    fn handed_masters_arrive_in_order_each_still_open() -> anyhow::Result<()> {
        let (ours, theirs) = UnixStream::pair()?;
        let (r1, w1) = std::io::pipe()?;
        let (r2, w2) = std::io::pipe()?;
        let (w1, w2) = (OwnedFd::from(w1), OwnedFd::from(w2));
        let sender = std::thread::spawn(move || -> io::Result<()> {
            send_mark(&theirs, b'T', &[w1.as_fd()])?;
            send_mark(&theirs, b'T', &[w2.as_fd()])?;
            send_mark(&theirs, b'E', &[])
            // The sender's own copies close here, as the runner's do when it exits.
        });
        let got = read_terminals(&ours).ok_or_else(|| anyhow::anyhow!("nothing handed over"))?;
        sender.join().map_err(|_| anyhow::anyhow!("sender panicked"))??;
        assert_eq!(got.len(), 2);
        for ((fd, mut r), text) in got.into_iter().zip([r1, r2]).zip(["one", "two"]) {
            let mut w = std::fs::File::from(fd);
            w.write_all(text.as_bytes())?;
            drop(w);
            let mut s = String::new();
            r.read_to_string(&mut s)?;
            assert_eq!(s, text, "each descriptor is the one sent in its place");
        }
        Ok(())
    }

    #[test]
    fn a_handover_cut_short_gives_nothing() -> anyhow::Result<()> {
        // A master, then the runner died before the end mark.
        let (ours, theirs) = UnixStream::pair()?;
        let (_r, w) = std::io::pipe()?;
        send_mark(&theirs, b'T', &[OwnedFd::from(w).as_fd()])?;
        drop(theirs);
        assert!(read_terminals(&ours).is_none());
        // A mark that should carry a master but doesn't.
        let (ours, theirs) = UnixStream::pair()?;
        send_mark(&theirs, b'T', &[])?;
        send_mark(&theirs, b'E', &[])?;
        assert!(read_terminals(&ours).is_none());
        // Nothing to hand over is a handover too.
        let (ours, theirs) = UnixStream::pair()?;
        send_mark(&theirs, b'E', &[])?;
        assert_eq!(read_terminals(&ours).map(|v| v.len()), Some(0));
        Ok(())
    }

    #[test]
    fn handed_masters_are_inherited_by_the_next_child() -> anyhow::Result<()> {
        let (mut r, w) = std::io::pipe()?;
        let handed = Terminals(vec![OwnedFd::from(w)]);
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", r#"for fd in $(echo "$KILN_HANDOVER_FDS" | tr , ' '); do echo inherited > "/dev/fd/$fd"; done"#]);
        handed.pass_to(&mut cmd)?;
        let status = cmd.status()?;
        // The supervisor's copies close once the child has them.
        drop(handed);
        let mut s = String::new();
        r.read_to_string(&mut s)?;
        assert!(status.success());
        assert_eq!(s.trim(), "inherited");
        Ok(())
    }
}
