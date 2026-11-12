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

#[allow(clippy::disallowed_methods)] // (e) polls a child process's state; its exit (pidfd, waitpid on the reactor) would tell
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
/// The supervisor sets this when it can carry agent sessions' harness processes over a successor
/// upgrade, not terminals alone: a runner hands its sessions over only when it's set, so a runner
/// under an older supervisor (terminals alone) never sends it a mark it wouldn't know (the handover
/// channel is append-only: an older reader turns an unknown mark into nothing handed over).
const SUPERVISOR_SESSIONS: &str = "KILN_SUPERVISOR_SESSIONS";
/// The terminals a fresh runner takes over: their descriptor numbers, in the order the runner
/// before it listed them in its handover file.
const HANDED_FDS: &str = "KILN_HANDOVER_FDS";
/// The agent sessions' pipe descriptors a fresh runner takes over: three per session (stdin,
/// stdout, stderr), flattened, in the order the runner before it handed them (sorted by session
/// id), which the fresh runner re-maps the same way (`Host::restore`).
const HANDED_SESSION_FDS: &str = "KILN_HANDOVER_SESSION_FDS";

/// The supervisor's end of the channel a runner hands its terminals and agent sessions over on
/// before it exits to take up a new binary ([`crate::os::UPGRADE_BY_SUCCESSOR`]): a Unix socket pair
/// whose other end the child inherits. The child sends `T` with each PTY master attached, `S` with
/// each session's three pipes attached, then `E` ([`to_supervisor`]), and exits with
/// [`crate::supervise::HANDOVER_EXIT`]; a reader thread collects them as they come, so the
/// supervisor holds them once the child is gone.
pub struct Handover {
    theirs: Option<OwnedFd>,
    ours: Option<UnixStream>,
    collected: Option<futures::channel::oneshot::Receiver<Option<Handed>>>,
}

/// What a runner handed over before it exited: PTY masters (terminals), and agent sessions' pipes
/// (three each). What the next runner takes over.
#[derive(Default)]
struct Handed {
    masters: Vec<OwnedFd>,
    sessions: Vec<OwnedFd>,
}

/// What a runner handed over before it exited: what the next one takes over ([`Handed`]).
pub struct Terminals(Handed);

impl Handover {
    /// A channel for the child `cmd` is about to start (its end inherited, its number and this
    /// supervisor's pid in its environment, and that this supervisor carries sessions too). `None`
    /// if the socket pair cannot be made: the child then runs without one, as under an older
    /// supervisor.
    pub fn for_child(cmd: &mut Command) -> Option<Self> {
        let (ours, theirs) = UnixStream::pair().ok()?;
        let theirs = OwnedFd::from(theirs);
        crate::os::keep_across_exec(&theirs).ok()?;
        cmd.env(CHANNEL_FD, theirs.as_raw_fd().to_string()).env(SUPERVISOR_PID, std::process::id().to_string()).env(SUPERVISOR_SESSIONS, "1");
        Some(Self { theirs: Some(theirs), ours: Some(ours), collected: None })
    }

    /// The child is running: close its end here (so its exit ends the channel) and start
    /// collecting what it sends. (The child's pid is what the Windows supervisor duplicates handles
    /// out of; Unix takes them by `SCM_RIGHTS` and doesn't need it.)
    pub fn started(&mut self, _pid: u32) {
        drop(self.theirs.take());
        let Some(sock) = self.ours.take() else { return };
        let (tx, rx) = futures::channel::oneshot::channel();
        let reader = std::thread::Builder::new().name("kiln-handover".into()).spawn(move || {
            let _ = tx.send(read_handover(&sock));
        });
        if reader.is_ok() {
            self.collected = Some(rx);
        }
    }

    /// What the child handed over before it exited: `None` unless it finished doing so (an end
    /// mark after its masters and sessions), so a child that died midway leaves nothing half taken.
    pub async fn finish(mut self) -> Option<Terminals> {
        let rx = self.collected.take()?;
        match crate::rt::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(Some(handed))) => Some(Terminals(handed)),
            _ => None,
        }
    }
}

impl Terminals {
    /// How many terminals: what the supervisor's log counts (sessions go on through their records
    /// when none come across, so only the terminals' count stands for the shells kept).
    pub fn len(&self) -> usize {
        self.0.masters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.masters.is_empty() && self.0.sessions.is_empty()
    }

    /// Give these to the child `cmd` starts: inherited, their numbers in its environment (the
    /// masters, as before; the sessions' pipes under their own variable). Dropping `self` once it
    /// has started closes this process's copies.
    pub fn pass_to(&self, cmd: &mut Command) -> io::Result<()> {
        for fd in self.0.masters.iter().chain(&self.0.sessions) {
            crate::os::keep_across_exec(fd)?;
        }
        let masters: Vec<String> = self.0.masters.iter().map(|fd| fd.as_raw_fd().to_string()).collect();
        cmd.env(HANDED_FDS, masters.join(","));
        if !self.0.sessions.is_empty() {
            let sessions: Vec<String> = self.0.sessions.iter().map(|fd| fd.as_raw_fd().to_string()).collect();
            cmd.env(HANDED_SESSION_FDS, sessions.join(","));
        }
        Ok(())
    }
}

/// Read a child's handover until it ends: each `T` carries one PTY master, each `S` carries one
/// session's three pipes, `E` ends it.
fn read_handover(sock: &UnixStream) -> Option<Handed> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut handed = Handed::default();
    loop {
        let mut mark = [0u8; 1];
        let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
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
            (1, b'T') if attached.len() == 1 => handed.masters.append(&mut attached),
            (1, b'S') if attached.len() == 3 => handed.sessions.append(&mut attached),
            (1, b'E') if attached.is_empty() => return Some(handed),
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

/// Whether the supervisor that started this process carries agent sessions across a successor
/// upgrade too, not terminals alone: only then does the runner hand its sessions over (a runner
/// under an older supervisor keeps terminals and lets sessions resume from their records).
pub fn supervisor_takes_sessions() -> bool {
    from_our_supervisor(SUPERVISOR_SESSIONS).is_some_and(|v| v == "1")
}

/// The descriptor numbers of the terminals the supervisor handed this process, in the order of
/// the handover file the runner before it wrote; `None` when it handed none.
pub fn handed_terminals() -> Option<Vec<i32>> {
    let list = from_our_supervisor(HANDED_FDS)?;
    Some(list.split(',').filter_map(|n| n.trim().parse().ok()).collect())
}

/// The agent sessions' pipe descriptors the supervisor handed this process, three per session
/// (stdin, stdout, stderr), flattened and in the order the runner before it handed them; `None`
/// when it handed none. The fresh runner re-maps them onto its restored sessions, sorted by id, the
/// same order they were handed (`Host::restore`).
///
/// Taken once, at the fresh runner's first restore. A later restore (a handover this runner then
/// tries that doesn't go through, so it takes its sessions back) reads the record it just wrote,
/// whose descriptors are this process's own; the now-stale environment must not re-map them.
pub fn handed_sessions() -> Option<Vec<i64>> {
    static TAKEN: Mutex<Option<Option<Vec<i64>>>> = Mutex::new(None);
    let mut slot = TAKEN.lock().unwrap_or_else(PoisonError::into_inner);
    slot.get_or_insert_with(|| from_our_supervisor(HANDED_SESSION_FDS).map(|list| list.split(',').filter_map(|n| n.trim().parse::<i32>().ok().map(i64::from)).collect())).take()
}

/// Hand these PTY masters, then these agent sessions' pipes, to the supervisor, then the end mark.
/// The process exits with [`crate::supervise::HANDOVER_EXIT`] next; everything stays open in the
/// supervisor meanwhile.
#[allow(unsafe_code)]
pub fn to_supervisor(masters: &[i32], sessions: &[crate::proc::Detached]) -> io::Result<()> {
    let ch = channel().ok_or_else(|| io::Error::other("no handover channel to the supervisor"))?;
    let sock = ch.lock().unwrap_or_else(PoisonError::into_inner);
    for &raw in masters {
        // SAFETY: each number is a PTY master one of this process's terminal sessions holds open,
        // and they all stay open (the sessions are not dropped) until this call returns.
        let fd = unsafe { BorrowedFd::borrow_raw(raw) };
        send_mark(&sock, b'T', &[fd])?;
    }
    for d in sessions {
        // SAFETY: a handed session's three pipe descriptors, kept open (the detached process holds
        // them) until this call returns.
        let pipes = unsafe { [BorrowedFd::borrow_raw(d.stdin), BorrowedFd::borrow_raw(d.stdout), BorrowedFd::borrow_raw(d.stderr)] };
        send_mark(&sock, b'S', &pipes)?;
    }
    send_mark(&sock, b'E', &[])
}

fn send_mark(sock: &UnixStream, mark: u8, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
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
        let got = read_handover(&ours).ok_or_else(|| anyhow::anyhow!("nothing handed over"))?;
        sender.join().map_err(|_| anyhow::anyhow!("sender panicked"))??;
        assert_eq!(got.masters.len(), 2);
        assert!(got.sessions.is_empty());
        for ((fd, mut r), text) in got.masters.into_iter().zip([r1, r2]).zip(["one", "two"]) {
            let mut w = std::fs::File::from(fd);
            w.write_all(text.as_bytes())?;
            drop(w);
            let mut s = String::new();
            r.read_to_string(&mut s)?;
            assert_eq!(s, text, "each descriptor is the one sent in its place");
        }
        Ok(())
    }

    /// An agent session's three pipes go over together under their own mark, in the order they were
    /// sent, each still the one sent: the next runner re-maps them onto its restored sessions.
    #[test]
    fn handed_sessions_arrive_as_triples_each_still_open() -> anyhow::Result<()> {
        let (ours, theirs) = UnixStream::pair()?;
        // One master, then one session's three pipes (stdin, stdout, stderr).
        let (mut mr, mw) = std::io::pipe()?;
        let (r0, w0) = std::io::pipe()?;
        let (r1, w1) = std::io::pipe()?;
        let (r2, w2) = std::io::pipe()?;
        let (mw, w0, w1, w2) = (OwnedFd::from(mw), OwnedFd::from(w0), OwnedFd::from(w1), OwnedFd::from(w2));
        let sender = std::thread::spawn(move || -> io::Result<()> {
            send_mark(&theirs, b'T', &[mw.as_fd()])?;
            send_mark(&theirs, b'S', &[w0.as_fd(), w1.as_fd(), w2.as_fd()])?;
            send_mark(&theirs, b'E', &[])
        });
        let got = read_handover(&ours).ok_or_else(|| anyhow::anyhow!("nothing handed over"))?;
        sender.join().map_err(|_| anyhow::anyhow!("sender panicked"))??;
        assert_eq!(got.masters.len(), 1, "the one terminal");
        assert_eq!(got.sessions.len(), 3, "the session's three pipes");
        for ((fd, mut r), text) in got.sessions.into_iter().zip([r0, r1, r2]).zip(["in", "out", "err"]) {
            let mut w = std::fs::File::from(fd);
            w.write_all(text.as_bytes())?;
            drop(w);
            let mut s = String::new();
            r.read_to_string(&mut s)?;
            assert_eq!(s, text, "each session pipe is the one sent in its place");
        }
        // The master is the one sent too.
        let mut mw = std::fs::File::from(got.masters.into_iter().next().ok_or_else(|| anyhow::anyhow!("no master"))?);
        mw.write_all(b"m")?;
        drop(mw);
        let mut s = String::new();
        mr.read_to_string(&mut s)?;
        assert_eq!(s, "m");
        Ok(())
    }

    /// A1 (step 30's follow-up): a running harness carried under the session mark, as a successor
    /// upgrade carries it. Detached from the runner, its three pipes go through the channel to the
    /// supervisor's collector and arrive at new descriptor numbers (as the fresh runner inherits
    /// them); the runner's own copies close as it exits; and the fresh runner adopts the harness from
    /// the new numbers as foreign (it isn't the harness's parent). Its input and output go on, and
    /// its end is still seen.
    #[test]
    fn a1_a_harness_carried_under_the_session_mark_is_adopted_as_foreign() -> anyhow::Result<()> {
        use std::os::fd::IntoRawFd as _;
        crate::rt::block_on(async {
            let mut p = crate::proc::spawn_duplex(Command::new("cat"))?;
            p.stdin.send(b"one\n".to_vec()).await?;
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(10), p.stdout.recv()).await?? == b"one\n", "it runs before the handover");
            let d = p.detach().await.ok_or_else(|| anyhow::anyhow!("couldn't detach the harness"))?;
            drop(p);
            // The runner's side: the session's three pipes under the session mark, then the end.
            let take = |raw: i32| crate::os::reclaim_fd(raw).ok_or_else(|| anyhow::anyhow!("descriptor {raw} isn't open"));
            let (i, o, e) = (take(d.stdin)?, take(d.stdout)?, take(d.stderr)?);
            let (ours, theirs) = UnixStream::pair()?;
            send_mark(&theirs, b'S', &[i.as_fd(), o.as_fd(), e.as_fd()])?;
            send_mark(&theirs, b'E', &[])?;
            // The supervisor's collector: the same three pipes, at numbers of its own.
            let handed = read_handover(&ours).ok_or_else(|| anyhow::anyhow!("nothing handed over"))?;
            let [ci, co, ce]: [OwnedFd; 3] = handed.sessions.try_into().map_err(|_| anyhow::anyhow!("not one session's three pipes"))?;
            // The runner exits: its copies close. Only the carried ones hold the pipes now.
            drop((i, o, e));
            let carried = crate::proc::Detached { pid: d.pid, stdin: ci.into_raw_fd(), stdout: co.into_raw_fd(), stderr: ce.into_raw_fd(), foreign: true };
            let mut q = crate::proc::adopt_duplex(carried)?;
            anyhow::ensure!(q.pid == d.pid, "the same harness");
            q.stdin.send(b"two\n".to_vec()).await?;
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(10), q.stdout.recv()).await?? == b"two\n", "its output goes on after adoption");
            // It ends when its stdin closes, and the adopting process sees that though it isn't its parent.
            q.stdin.close();
            crate::rt::timeout(Duration::from_secs(10), &mut q.exit).await??;
            Ok(())
        })
    }

    #[test]
    fn a_handover_cut_short_gives_nothing() -> anyhow::Result<()> {
        // A master, then the runner died before the end mark.
        let (ours, theirs) = UnixStream::pair()?;
        let (_r, w) = std::io::pipe()?;
        send_mark(&theirs, b'T', &[OwnedFd::from(w).as_fd()])?;
        drop(theirs);
        assert!(read_handover(&ours).is_none());
        // A mark that should carry a master but doesn't.
        let (ours, theirs) = UnixStream::pair()?;
        send_mark(&theirs, b'T', &[])?;
        send_mark(&theirs, b'E', &[])?;
        assert!(read_handover(&ours).is_none());
        // A session mark with the wrong number of pipes.
        let (ours, theirs) = UnixStream::pair()?;
        let (_r, w) = std::io::pipe()?;
        send_mark(&theirs, b'S', &[OwnedFd::from(w).as_fd()])?;
        send_mark(&theirs, b'E', &[])?;
        assert!(read_handover(&ours).is_none());
        // Nothing to hand over is a handover too.
        let (ours, theirs) = UnixStream::pair()?;
        send_mark(&theirs, b'E', &[])?;
        assert_eq!(read_handover(&ours).map(|h| (h.masters.len(), h.sessions.len())), Some((0, 0)));
        Ok(())
    }

    #[test]
    fn handed_masters_and_sessions_are_inherited_by_the_next_child() -> anyhow::Result<()> {
        let (mut rm, wm) = std::io::pipe()?;
        let (mut rs, ws) = std::io::pipe()?;
        let handed = Terminals(Handed { masters: vec![OwnedFd::from(wm)], sessions: vec![OwnedFd::from(ws)] });
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            r#"for fd in $(echo "$KILN_HANDOVER_FDS" | tr , ' '); do echo master > "/dev/fd/$fd"; done; for fd in $(echo "$KILN_HANDOVER_SESSION_FDS" | tr , ' '); do echo session > "/dev/fd/$fd"; done"#,
        ]);
        handed.pass_to(&mut cmd)?;
        let status = cmd.status()?;
        // The supervisor's copies close once the child has them.
        drop(handed);
        let (mut m, mut s) = (String::new(), String::new());
        rm.read_to_string(&mut m)?;
        rs.read_to_string(&mut s)?;
        assert!(status.success());
        assert_eq!(m.trim(), "master");
        assert_eq!(s.trim(), "session");
        Ok(())
    }
}
