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
//! What an agent starts through launchd (`launchctl bootstrap`, `load`, `submit`, `start`,
//! `kickstart`) leaves the agent's tree for launchd's, with nothing of the agent's environment, so
//! neither forks nor marks find it: the `launchctl` an agent's process runs is read as it runs (its
//! arguments, and the label of each property list it names), and the job's process, whenever
//! launchd has one, is followed as the agent's from then on, paced and ended with it (ending the
//! agent boots its jobs out). On 2026-11-05 an agent's Windows VM (24 GB) and its measurements ran
//! as launchd jobs, outside everything kiln knew of the agent, when the Mac ran out of memory.
//!
//! Priority (docs/priority.md): an agent's harness, and each CI step, starts below normal
//! ([`background`]: nice [`NICE`]), and so does all they start. Not under Darwin background policy:
//! Apple silicon runs that on its efficiency cores alone and throttles its disk and network I/O, so
//! agents had a fraction of the machine and moved their work where kiln couldn't see it (launchd
//! jobs, VMs). Below normal, their work has every core the machine isn't using for something
//! higher, and headroom slows it when the server's delay, a message's delivery or memory says so.

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
/// The nice value agents' and legs' processes start at: below a process started plainly (0), so the
/// server and what a person runs come first, on every core alike.
pub const NICE: i32 = 10;
/// How often launchd is asked for the processes of agents' jobs, at most.
const JOBS_EVERY: Duration = Duration::from_secs(1);

/// The processes followed, by pid: whose each is.
struct Tracker {
    kq: i32,
    members: Mutex<HashMap<i32, String>>,
    /// The sessions with a group: those whose marks the sweep looks for.
    sessions: Mutex<HashSet<String>>,
    /// The launchd jobs agents started, by label: whose each is.
    jobs: Mutex<HashMap<String, String>>,
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
        Some(Tracker { kq, members: Mutex::new(HashMap::new()), sessions: Mutex::new(HashSet::new()), jobs: Mutex::new(HashMap::new()) })
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
        ev.fflags = libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_EXIT;
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
        let mut jobs_asked = Instant::now();
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
                if ev.fflags & libc::NOTE_EXEC != 0
                    && let Some(session) = self.members.lock().get(&pid).cloned()
                {
                    for label in launchd_labels(pid) {
                        tracing::info!(session = %session, label = %label, "an agent started a launchd job: it's followed as the agent's");
                        self.jobs.lock().insert(label, session.clone());
                    }
                }
                if ev.fflags & libc::NOTE_EXIT != 0 {
                    self.members.lock().remove(&pid);
                }
            }
            if jobs_asked.elapsed() >= JOBS_EVERY {
                jobs_asked = Instant::now();
                self.adopt_jobs();
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

    /// The process of each agent's launchd job that has one and isn't followed yet: followed as its
    /// agent's from now on, below normal as the agent's own are.
    fn adopt_jobs(&self) {
        let jobs: Vec<(String, String)> = self.jobs.lock().iter().map(|(l, s)| (l.clone(), s.clone())).collect();
        for (label, session) in jobs {
            let Some(pid) = job_pid(&label) else { continue };
            if self.members.lock().contains_key(&pid) {
                continue;
            }
            self.watch(pid, &session, DEPTH);
            let mut tree = vec![pid];
            let mut i = 0;
            while let Some(p) = tree.get(i).copied() {
                tree.extend(children(p));
                i += 1;
                if tree.len() > MOST_CHILDREN {
                    break;
                }
            }
            for p in tree {
                // SAFETY: one system call on a process id.
                unsafe { libc::setpriority(libc::PRIO_PROCESS, u32::try_from(p).unwrap_or(0), NICE) };
            }
            tracing::info!(session = %session, label = %label, pid, "an agent's launchd job runs: its processes are the agent's");
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

/// Process `pid`'s arguments, its program first (`KERN_PROCARGS2`), while it runs.
fn args(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: asks the size only.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &raw mut size, std::ptr::null_mut(), 0) } != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: a buffer of `size` bytes.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &raw mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(size);
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    // The executable's path, then NULs up to the arguments.
    let rest = buf.get(4..)?;
    let after_path = rest.iter().position(|b| *b == 0)?;
    let start = after_path + rest.get(after_path..)?.iter().position(|b| *b != 0)?;
    let out: Vec<String> = rest.get(start..)?.split(|b| *b == 0).take(argc).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    (out.len() == argc).then_some(out)
}

/// Process `pid`'s working directory.
fn cwd(pid: i32) -> Option<std::path::PathBuf> {
    // `struct proc_vnodepathinfo`: the working directory's `vnode_info_path` first, its path after
    // a `vnode_info` (152 bytes), MAXPATHLEN long; then the root directory's.
    const VNODE_INFO: usize = 152;
    const PATH_MAX: usize = 1024;
    let mut buf = vec![0u8; 2 * (VNODE_INFO + PATH_MAX)];
    let size = i32::try_from(buf.len()).ok()?;
    // SAFETY: a buffer of `size` bytes for PROC_PIDVNODEPATHINFO.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, buf.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    let path = buf.get(VNODE_INFO..VNODE_INFO + PATH_MAX)?;
    let end = path.iter().position(|b| *b == 0)?;
    let p = std::path::PathBuf::from(String::from_utf8_lossy(path.get(..end)?).into_owned());
    p.is_absolute().then_some(p)
}

/// The labels of the launchd jobs process `pid` starts, when it's a `launchctl` that starts any.
fn launchd_labels(pid: i32) -> Vec<String> {
    let Some(argv) = args(pid) else { return Vec::new() };
    let is_launchctl = argv.first().is_some_and(|a| std::path::Path::new(a).file_name().is_some_and(|n| n == "launchctl"));
    if !is_launchctl {
        return Vec::new();
    }
    let rest: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    let (labels, plists) = launchctl_starts(&rest);
    let here = cwd(pid);
    labels
        .into_iter()
        .chain(plists.into_iter().filter_map(|p| {
            let p = std::path::Path::new(p);
            let p = if p.is_absolute() { p.to_path_buf() } else { here.as_ref()?.join(p) };
            plist_label(&p)
        }))
        .collect()
}

/// What `launchctl ARGS` starts: labels it names, and property lists whose labels name jobs.
fn launchctl_starts<'a>(args: &[&'a str]) -> (Vec<String>, Vec<&'a str>) {
    let Some((sub, rest)) = args.split_first() else { return (Vec::new(), Vec::new()) };
    let target_label = |t: &str| t.rsplit('/').next().filter(|l| !l.is_empty()).map(str::to_owned);
    match *sub {
        // bootstrap <domain> <paths…>
        "bootstrap" => (Vec::new(), rest.iter().skip(1).copied().filter(|a| !a.starts_with('-')).collect()),
        // load [-wF] [-S session] [-D domain] <paths…>
        "load" => {
            let mut paths = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match *a {
                    "-S" | "-D" => {
                        it.next();
                    }
                    a if a.starts_with('-') => {}
                    a => paths.push(a),
                }
            }
            (Vec::new(), paths)
        }
        // submit -l <label> [-p program] … -- <command…>
        "submit" => (rest.iter().position(|a| *a == "-l").and_then(|i| rest.get(i + 1)).map(|l| (*l).to_owned()).into_iter().collect(), Vec::new()),
        // start <label>
        "start" => (rest.first().map(|l| (*l).to_owned()).into_iter().collect(), Vec::new()),
        // kickstart [-kps] <service-target>
        "kickstart" => (rest.iter().rfind(|a| !a.starts_with('-')).and_then(|t| target_label(t)).into_iter().collect(), Vec::new()),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The `Label` of the property list at `path`.
fn plist_label(path: &std::path::Path) -> Option<String> {
    let out = Command::new("/usr/bin/plutil").args(["-extract", "Label", "raw", "-o", "-"]).arg(path).output().ok()?;
    let label = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !label.is_empty()).then_some(label)
}

/// The process launchd runs for job `label` of this user's domain now, if any.
fn job_pid(label: &str) -> Option<i32> {
    let out = Command::new("/bin/launchctl").args(["list", label]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.trim().strip_prefix("\"PID\" = ")?.trim_end_matches(';').trim().parse().ok()).filter(|p| *p > 1)
}

/// Ends launchd job `label` of this user's GUI domain (so it isn't started again).
fn boot_out(label: &str) {
    // SAFETY: getuid takes nothing.
    let uid = unsafe { libc::getuid() };
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/{label}")]).output();
}

/// An agent session's group of processes: those followed as its, and whether they start under
/// background policy.
#[derive(Debug, Clone)]
pub struct Group {
    session: String,
    idle: bool,
}

impl Group {
    /// What its processes used: nothing this OS accounts per group (a run's own `wait4` counts
    /// instead, docs/needs.md).
    pub fn usage(&self) -> crate::sense::run::Usage {
        crate::sense::run::Usage::default()
    }

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

    /// Ends every process of it (SIGKILL to each, again while any forked meanwhile is left), its
    /// launchd jobs booted out first so launchd doesn't start them again.
    pub fn kill(&self) {
        let Some(t) = tracker() else { return };
        let jobs: Vec<String> = t.jobs.lock().iter().filter(|(_, s)| **s == self.session).map(|(l, _)| l.clone()).collect();
        for label in &jobs {
            boot_out(label);
        }
        t.jobs.lock().retain(|_, s| *s != self.session);
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
            t.jobs.lock().retain(|_, s| *s != self.session);
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

/// Makes `cmd`'s process start below normal (docs/priority.md): nice [`NICE`], set between fork and
/// exec, which everything it starts inherits. Every core, and its I/O unthrottled; only ranked
/// below what runs at normal.
pub fn background(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: the closure runs in the child between fork and exec, and makes one system call.
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, NICE);
            Ok(())
        });
    }
}

/// Process `pid`, started some other way (a CI step on a pty), below normal from now on, as
/// `renice` puts one: whether it was.
pub fn background_pid(pid: u32) -> bool {
    // SAFETY: one system call on a process id.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, NICE) == 0 }
}

/// How legs and agents run here (docs/priority.md), for the runner's facts and its log.
pub fn priority() -> String {
    format!("below normal: each leg's and agent's processes run at nice {NICE}, on every core")
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

    /// What `launchctl` starts is read from its arguments, for every way it starts a job.
    #[test]
    fn what_launchctl_starts_is_read_from_its_arguments() {
        let read = |a: &[&str]| {
            let (l, p) = launchctl_starts(a);
            (l, p.into_iter().map(str::to_owned).collect::<Vec<_>>())
        };
        let none: Vec<String> = Vec::new();
        assert_eq!(read(&["bootstrap", "gui/501", "/a/x.plist", "b.plist"]), (none.clone(), vec!["/a/x.plist".to_owned(), "b.plist".to_owned()]));
        assert_eq!(read(&["load", "-w", "-S", "Aqua", "/a/x.plist"]), (none.clone(), vec!["/a/x.plist".to_owned()]));
        assert_eq!(read(&["submit", "-l", "com.x.vm", "--", "/bin/sleep", "9"]), (vec!["com.x.vm".to_owned()], Vec::new()));
        assert_eq!(read(&["start", "com.x.vm"]), (vec!["com.x.vm".to_owned()], Vec::new()));
        assert_eq!(read(&["kickstart", "-k", "gui/501/com.x.vm"]), (vec!["com.x.vm".to_owned()], Vec::new()));
        assert_eq!(read(&["list"]), (none.clone(), Vec::new()));
        assert_eq!(read(&["bootout", "gui/501/com.x.vm"]), (none, Vec::new()));
    }

    #[test]
    fn a_processs_arguments_and_folder_are_read() -> anyhow::Result<()> {
        let mut child = Command::new("sleep").arg("33").current_dir("/").stdin(std::process::Stdio::null()).spawn()?;
        let pid = i32::try_from(child.id())?;
        let a = args(pid);
        let here = cwd(pid);
        let _ = child.kill();
        let _ = child.wait();
        anyhow::ensure!(a == Some(vec!["sleep".to_owned(), "33".to_owned()]), "{a:?}");
        anyhow::ensure!(here.as_deref() == Some(std::path::Path::new("/")), "{here:?}");
        Ok(())
    }

    /// A launchd job an agent starts (here as `launchd-run.sh` started one on 2026-11-05) is followed
    /// as the agent's, below normal, and ending the agent boots it out.
    #[test]
    fn what_an_agent_starts_through_launchd_is_followed_as_its() -> anyhow::Result<()> {
        let session = format!("ld{}", std::process::id());
        let label = format!("io.github.hyper-light.kiln.test.{session}");
        let dir = tempfile::tempdir()?;
        let plist = dir.path().join("job.plist");
        std::fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>/bin/sleep</string><string>120</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
            ),
        )?;
        hold_agents();
        let group = Group::of(&session).ok_or_else(|| anyhow::anyhow!("no kqueue"))?;
        let mut cmd = Command::new("sh");
        // Relative, as an agent's script names it from its folder.
        cmd.args(["-c", "sleep 1; launchctl bootstrap gui/$(id -u) job.plist; sleep 20"]).current_dir(dir.path()).stdin(std::process::Stdio::null());
        group.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        group.started(child.id());
        let mut job = None;
        let mut followed = false;
        for _ in 0..100 {
            job = job_pid(&label);
            followed = job.is_some_and(|p| group.pids().contains(&u32::try_from(p).unwrap_or(0)));
            if followed {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let lowered = job.and_then(|p| nice(u32::try_from(p).ok()?));
        group.kill();
        let _ = child.wait();
        let after = job_pid(&label);
        let listed = Command::new("/bin/launchctl").args(["list", &label]).output()?.status.success();
        group.remove();
        boot_out(&label);
        anyhow::ensure!(job.is_some(), "launchd never ran the job");
        anyhow::ensure!(followed, "the job's process {job:?} isn't the agent's");
        anyhow::ensure!(lowered == Some(NICE), "the job's process runs at nice {lowered:?}, not below normal");
        anyhow::ensure!(after.is_none() && !listed, "ending the agent left its job: {after:?}");
        Ok(())
    }

    /// A process's nice value as `ps` says it (`ni`). (Its scheduling priority, `pri`, doesn't show
    /// a nice value on macOS.)
    fn nice(pid: u32) -> Option<i32> {
        let out = Command::new("ps").args(["-o", "ni=", "-p", &pid.to_string()]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    /// Goal 8 (plan 01a1a59c2b29 step 3, docs/priority.md): an agent's harness, and what it starts,
    /// run below normal (nice), below a process started plainly; a person's run doesn't; a
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
        let normal = nice(plain.id());
        let agents: Vec<Option<i32>> = pids.iter().map(|p| nice(*p)).collect();
        let person = Group::of_person(&format!("run-g8{}", std::process::id())).ok_or_else(|| anyhow::anyhow!("no kqueue"))?;
        let mut cmd = Command::new("sleep");
        cmd.arg("32").stdin(std::process::Stdio::null());
        person.enter(&mut cmd);
        let mut persons = cmd.spawn()?;
        let persons_before = nice(persons.id());
        let put = background_pid(persons.id());
        let persons_after = nice(persons.id());
        group.kill();
        let _ = child.wait();
        group.remove();
        for c in [&mut persons, &mut plain] {
            let _ = c.kill();
            let _ = c.wait();
        }
        person.remove();
        let normal = normal.ok_or_else(|| anyhow::anyhow!("ps said nothing of a plain process"))?;
        anyhow::ensure!(pids.len() >= 2 && agents.iter().all(|p| *p == Some(NICE) && NICE > normal), "the harness and its sleep, each at nice {NICE}, a plain process at {normal}: {pids:?} {agents:?}");
        anyhow::ensure!(persons_before == Some(normal), "a person's run lowered: {persons_before:?}, a plain process {normal}");
        anyhow::ensure!(put && persons_after == Some(NICE), "a running process not put below normal: {persons_after:?}");
        Ok(())
    }
}
