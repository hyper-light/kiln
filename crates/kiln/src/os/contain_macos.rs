//! Each agent session's processes followed on macOS, which has no way for an unprivileged process
//! to hold others together: its harness is watched with kqueue (`EVFILT_PROC`: fork, exit), and
//! whenever one of the agent's processes forks, its children (`proc_listchildpids`) are the
//! agent's too, and watched in turn, so the agent's processes are known as they come and go
//! without looking at the machine's. A process that forked and ended before its children were
//! listed (a quick double fork) leaves those children unseen: once a minute, the user's processes
//! are looked at for the agent's mark (`crate::harness::survivors::MARK`), and what carries it is
//! followed from then on. After a runner restart nothing is followed until the agent's harness
//! starts again; until then its processes are found by their mark, as before.
//!
//! Priority (docs/priority.md; goal 8, plan 01a1a59c2b29 step 3): an agent's harness, and each CI
//! step, starts under background policy ([`background`]), and so does all they start.

#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nix::libc;
use parking_lot::Mutex;

/// How often the user's processes are looked at for agents' marks.
const SWEEP: Duration = Duration::from_secs(60);
/// The most children of one process listed at once.
const MOST_CHILDREN: usize = 1024;
/// How deep a fork's descendants are listed when it's seen.
const DEPTH: usize = 8;

/// The processes followed, by pid: whose each is.
struct Tracker {
    kq: i32,
    members: Mutex<HashMap<i32, String>>,
    /// The sessions with a group: those whose marks the sweep looks for.
    sessions: Mutex<HashSet<String>>,
}

fn tracker() -> Option<&'static Tracker> {
    static T: OnceLock<Option<Tracker>> = OnceLock::new();
    T.get_or_init(|| {
        // SAFETY: kqueue() takes nothing and returns a descriptor or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            tracing::info!("no kqueue: agents' processes are found by their mark");
            return None;
        }
        Some(Tracker { kq, members: Mutex::new(HashMap::new()), sessions: Mutex::new(HashSet::new()) })
    })
    .as_ref()
    .inspect(|t| t.run_once())
}

impl Tracker {
    /// The thread that follows the agents' processes, started the first time one is.
    fn run_once(&'static self) {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            let spawned = std::thread::Builder::new().name("kiln-agents-follow".to_owned()).spawn(move || self.run());
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "agents' processes can't be followed: they're found by their mark");
            }
        });
    }

    /// Follows `pid` as `session`'s, and what it started already.
    fn watch(&self, pid: i32, session: &str, depth: usize) {
        if pid <= 1 || self.members.lock().insert(pid, session.to_owned()).is_some() {
            return;
        }
        // SAFETY: an all-zero kevent is a valid value of the C struct.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = usize::try_from(pid).unwrap_or(0);
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        ev.fflags = libc::NOTE_FORK | libc::NOTE_EXIT;
        // SAFETY: one change, no events asked back, no timeout.
        let r = unsafe { libc::kevent(self.kq, &raw const ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 {
            // It ended already.
            self.members.lock().remove(&pid);
            return;
        }
        if depth > 0 {
            for c in children(pid) {
                self.watch(c, session, depth - 1);
            }
        }
    }

    fn run(&self) {
        let mut swept = Instant::now();
        // SAFETY: an all-zero kevent is a valid value of the C struct.
        let mut events: Vec<libc::kevent> = (0..64).map(|_| unsafe { std::mem::zeroed() }).collect();
        let wait = libc::timespec { tv_sec: 5, tv_nsec: 0 };
        loop {
            let cap = i32::try_from(events.len()).unwrap_or(0);
            // SAFETY: no changes; room for `cap` events; a timeout so the sweep runs.
            let n = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, events.as_mut_ptr(), cap, &raw const wait) };
            for ev in events.iter().take(usize::try_from(n).unwrap_or(0)) {
                let Ok(pid) = i32::try_from(ev.ident) else { continue };
                // Whose it is, the lock let go before its children are followed (`watch` takes it).
                let session = self.members.lock().get(&pid).cloned();
                if ev.fflags & libc::NOTE_FORK != 0
                    && let Some(session) = session
                {
                    for c in children(pid) {
                        self.watch(c, &session, DEPTH);
                    }
                }
                if ev.fflags & libc::NOTE_EXIT != 0 {
                    self.members.lock().remove(&pid);
                }
            }
            if swept.elapsed() >= SWEEP {
                swept = Instant::now();
                self.sweep();
            }
        }
    }

    /// What carries a followed session's mark and isn't followed yet (it left between a fork and
    /// its listing): followed from now on.
    fn sweep(&self) {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let sessions = self.sessions.lock().clone();
        if sessions.is_empty() {
            return;
        }
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_environ(UpdateKind::Always).with_user(UpdateKind::Always).without_tasks());
        let me = sys.process(sysinfo::Pid::from_u32(std::process::id())).and_then(|p| p.user_id().cloned());
        let prefix = format!("{}=", crate::harness::survivors::MARK);
        for (pid, p) in sys.processes() {
            if me.is_some() && p.user_id() != me.as_ref() {
                continue;
            }
            let Some(session) = p.environ().iter().find_map(|kv| kv.to_str().and_then(|s| s.strip_prefix(prefix.as_str()))) else { continue };
            let Ok(pid) = i32::try_from(pid.as_u32()) else { continue };
            // Looked up and let go of before it's followed (`watch` takes the lock).
            let followed = self.members.lock().contains_key(&pid);
            if sessions.contains(session) && !followed {
                self.watch(pid, session, DEPTH);
            }
        }
    }

    fn of(&self, session: &str) -> Vec<i32> {
        self.members.lock().iter().filter(|(_, s)| s.as_str() == session).map(|(p, _)| *p).collect()
    }
}

/// Whether this process holds agents' processes together: only the processes that start agents for
/// kiln (the runner, the server) do, from [`hold_agents`] on. Anything else linking kiln (an
/// integration test, a tool) that starts a harness leaves its processes where they are, so it never
/// makes groups in a live runner's cgroup it happens to run in (a test in a kiln terminal).
static HOLDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This process starts agents for kiln (the runner, the server): their processes are held together
/// from now on.
pub fn hold_agents() {
    HOLDS.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn holds() -> bool {
    HOLDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The children of `pid` now.
fn children(pid: i32) -> Vec<i32> {
    let mut buf = vec![0i32; MOST_CHILDREN];
    let size = i32::try_from(buf.len() * std::mem::size_of::<i32>()).unwrap_or(0);
    // SAFETY: a buffer of `size` bytes for pids.
    let n = unsafe { libc::proc_listchildpids(pid, buf.as_mut_ptr().cast(), size) };
    let n = usize::try_from(n).unwrap_or(0).min(buf.len());
    buf.into_iter().take(n).filter(|p| *p > 1).collect()
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// An agent session's group of processes: those followed as its, and whether they start under
/// background policy.
#[derive(Debug, Clone)]
pub struct Group {
    session: String,
    idle: bool,
}

impl Group {
    /// Agent session `session`'s group: its processes are followed once its harness has started,
    /// which starts under background policy.
    pub fn of(session: &str) -> Option<Group> {
        Self::made(session, true)
    }

    /// A group `name` for work a person asked for (their own reach run): followed as an agent's
    /// is, at the priority of what the person runs rather than under background policy.
    pub fn of_person(name: &str) -> Option<Group> {
        Self::made(name, false)
    }

    fn made(session: &str, idle: bool) -> Option<Group> {
        if !holds() {
            return None;
        }
        let t = tracker()?;
        t.sessions.lock().insert(session.to_owned());
        Some(Group { session: session.to_owned(), idle })
    }

    /// The group `session` has, while any of its processes is followed.
    pub fn existing(session: &str) -> Option<Group> {
        if !holds() {
            return None;
        }
        let t = tracker()?;
        let followed = t.members.lock().values().any(|s| s == session);
        followed.then(|| Group { session: session.to_owned(), idle: false })
    }

    /// Before it starts: macOS can't hold it anywhere, but an agent's starts under background
    /// policy ([`background`]).
    pub fn enter(&self, cmd: &mut Command) {
        if self.idle {
            background(cmd);
        }
    }

    /// Its harness started: followed from now on, and what it started already.
    pub fn started(&self, pid: u32) -> bool {
        let (Some(t), Ok(pid)) = (tracker(), i32::try_from(pid)) else { return false };
        t.watch(pid, &self.session, DEPTH);
        t.members.lock().contains_key(&pid)
    }

    /// Every process of it followed now.
    pub fn pids(&self) -> Vec<u32> {
        let Some(t) = tracker() else { return Vec::new() };
        t.of(&self.session).into_iter().filter(|p| alive(*p)).filter_map(|p| u32::try_from(p).ok()).collect()
    }

    /// Ends every process of it (SIGKILL to each, again while any forked meanwhile is left).
    pub fn kill(&self) {
        let Some(t) = tracker() else { return };
        for _ in 0..10 {
            let pids = t.of(&self.session);
            if pids.is_empty() {
                return;
            }
            for p in pids {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(50));
            t.members.lock().retain(|p, _| alive(*p));
        }
    }

    /// Stops following it.
    pub fn remove(&self) {
        if let Some(t) = tracker() {
            t.members.lock().retain(|_, s| *s != self.session);
            t.sessions.lock().remove(&self.session);
        }
    }
}

/// The sessions with processes followed now.
pub fn sessions() -> Vec<String> {
    let Some(t) = tracker() else { return Vec::new() };
    let mut s: Vec<String> = t.members.lock().values().cloned().collect();
    s.sort();
    s.dedup();
    s
}

/// The agent session process `pid` belongs to, when it's one followed.
pub fn session_of(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok()?;
    tracker()?.members.lock().get(&pid).cloned()
}

/// Makes `cmd`'s process run under background policy (docs/priority.md), as `taskpolicy -b` runs a
/// program: Darwin's background priority (`setpriority(PRIO_DARWIN_PROCESS, 0, PRIO_DARWIN_BG)`,
/// set between fork and exec), which lowers its CPU priority and throttles its disk and network
/// I/O; everything it starts inherits it (taskpolicy(8)).
pub fn background(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the child between fork and exec, and makes one system call.
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG);
            Ok(())
        });
    }
}

/// Process `pid`, started some other way (a CI step on a pty), under background policy from now on,
/// as `taskpolicy -b -p` puts one: whether it was.
pub fn background_pid(pid: u32) -> bool {
    // SAFETY: one system call on a process id.
    unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid, libc::PRIO_DARWIN_BG) == 0 }
}

/// How legs and agents run here (docs/priority.md), for the runner's facts and its log.
pub fn priority() -> String {
    "background: each leg's and agent's processes run under Darwin background policy (taskpolicy -b)".to_owned()
}

/// How this machine holds an agent's processes together.
pub fn how() -> String {
    match tracker() {
        Some(_) => "followed fork by fork (kqueue), with a sweep for the agent's mark each minute".to_owned(),
        None => "none (processes are found by their mark)".to_owned(),
    }
}

/// A harness followed from its start: what it starts, and what that starts, are its; ending the
/// group ends them all.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_an_agent_starts_is_followed_fork_by_fork() -> anyhow::Result<()> {
        let session = format!("test{}", std::process::id());
        hold_agents();
        let group = Group::of(&session).ok_or_else(|| anyhow::anyhow!("no kqueue"))?;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 61 & sh -c 'sleep 62 & wait' & sleep 63"]).stdin(std::process::Stdio::null());
        group.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        anyhow::ensure!(group.started(child.id()), "its harness is followed");
        let mut pids = Vec::new();
        for _ in 0..50 {
            pids = group.pids();
            if pids.len() >= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mine = session_of(child.id());
        group.kill();
        let _ = child.wait();
        let after = group.pids();
        group.remove();
        anyhow::ensure!(pids.len() >= 4, "the harness (or the command it became), its sleep, the inner shell and its sleep: {pids:?}");
        anyhow::ensure!(mine.as_deref() == Some(session.as_str()), "whose it is: {mine:?}");
        anyhow::ensure!(after.is_empty(), "ending the group ends all of it: {after:?}");
        anyhow::ensure!(session_of(std::process::id()).is_none(), "the runner isn't an agent's");
        Ok(())
    }

    /// A process's scheduling priority as `ps` says it (`pri`), which background policy lowers
    /// (`getpriority(PRIO_DARWIN_PROCESS)` asked of another process says only what was set from
    /// outside it, so it misses a process that put itself there).
    fn pri(pid: u32) -> Option<i32> {
        let out = Command::new("ps").args(["-o", "pri=", "-p", &pid.to_string()]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    /// Goal 8 (plan 01a1a59c2b29 step 3, docs/priority.md): an agent's harness, and what it starts,
    /// run under background policy, below a process started plainly; a person's run doesn't; a
    /// process started some other way (a step on a pty) is put there as it runs.
    #[test]
    fn g8_s3_an_agents_processes_run_under_background_policy() -> anyhow::Result<()> {
        let session = format!("g8agent{}", std::process::id());
        hold_agents();
        let mut plain = Command::new("sleep").arg("30").stdin(std::process::Stdio::null()).spawn()?;
        let group = Group::of(&session).ok_or_else(|| anyhow::anyhow!("no kqueue"))?;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 31 & wait"]).stdin(std::process::Stdio::null());
        group.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        group.started(child.id());
        let mut pids = Vec::new();
        for _ in 0..50 {
            pids = group.pids();
            if pids.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let normal = pri(plain.id());
        let agents: Vec<Option<i32>> = pids.iter().map(|p| pri(*p)).collect();
        let person = Group::of_person(&format!("run-g8{}", std::process::id())).ok_or_else(|| anyhow::anyhow!("no kqueue"))?;
        let mut cmd = Command::new("sleep");
        cmd.arg("32").stdin(std::process::Stdio::null());
        person.enter(&mut cmd);
        let mut persons = cmd.spawn()?;
        let persons_before = pri(persons.id());
        let put = background_pid(persons.id());
        let persons_after = pri(persons.id());
        group.kill();
        let _ = child.wait();
        group.remove();
        for c in [&mut persons, &mut plain] {
            let _ = c.kill();
            let _ = c.wait();
        }
        person.remove();
        let normal = normal.ok_or_else(|| anyhow::anyhow!("ps said nothing of a plain process"))?;
        anyhow::ensure!(pids.len() >= 2 && agents.iter().all(|p| p.is_some_and(|p| p < normal)), "the harness and its sleep, each below a plain process's {normal}: {pids:?} {agents:?}");
        anyhow::ensure!(persons_before == Some(normal), "a person's run lowered: {persons_before:?}, a plain process {normal}");
        anyhow::ensure!(put && persons_after.is_some_and(|p| p < normal), "a running process not put under background policy: {persons_after:?}");
        Ok(())
    }
}
