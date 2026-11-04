//! Each agent session's processes held together by the kernel: a cgroup (v2) of its own,
//! `kiln-agents/<session>` under the cgroup the runner runs in (`kiln-agents-<pid>/<session>` for a
//! runner that isn't the machine's installed one: [`prune_ended`]); and each CI job's steps on this
//! machine, `kiln-jobs-<pid>/<job>` ([`Job`]). Its harness joins it between fork
//! and exec, before it runs anything, so everything it starts is in it too, however that's
//! parented later (a command that daemonized, `setsid`, a double fork): no process leaves its
//! cgroup by forking. Listing an agent's processes is reading its `cgroup.procs`; whose a process
//! is, one read of `/proc/<pid>/cgroup`; ending them all, one write to `cgroup.kill`. Nothing is
//! scanned.
//!
//! Membership alone needs no delegation: any cgroup v2 the runner may write will do (a systemd
//! user unit, a container with its own cgroup namespace). Where there is none (cgroup v1, a
//! read-only cgroup filesystem, a cgroup another user owns), agents have no group and their
//! processes are found by their mark (`crate::harness::survivors`), as before.
//!
//! Priority (docs/priority.md; goal 8, plan 01a1a59c2b29 step 2): where the machine's installed
//! runner runs in a unit its service manager delegated to it (systemd's `Delegate=yes`, which
//! `kiln install` sets), the CPU is shared among what it starts, and each agent's and each CI
//! job's group runs at idle priority (`cpu.idle` 1, the cgroup's SCHED_IDLE): it gets the CPU only
//! when nothing else on the machine wants it. cgroup v2 gives a cgroup's children a controller only
//! while it holds no process itself, so kiln's own processes run in the unit's leaf [`LEAF`]
//! (systemd's `DelegateSubgroup=`, or moved there by the runner) and the folders of groups sit
//! beside it, under the unit.

use std::fs;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Where cgroup v2 is mounted.
const ROOT: &str = "/sys/fs/cgroup";
/// The leaf of a delegated unit's cgroup that kiln's own processes run in ([`super::UNIT_LEAF`]).
const LEAF: &str = super::UNIT_LEAF;
/// The folder under the runner's own cgroup that holds agents' groups (in tests, one of their own:
/// tests often run in a live runner's cgroup).
const AGENTS: &str = if cfg!(test) { "kiln-agents-test" } else { "kiln-agents" };
/// The folder under the runner's own cgroup that holds its CI jobs' groups, one a runner process
/// (`kiln-jobs-<pid>`): a job ends with the process that runs it. Headroom paces each as it does an
/// agent's (docs/priority.md).
const JOBS: &str = if cfg!(test) { "kiln-jobs-test" } else { "kiln-jobs" };
/// How deep cgroups an agent's processes made of their own are followed.
const DEPTH: usize = 8;

/// A folder of agents' groups: where it is, its path as `/proc/<pid>/cgroup` names it, and whether
/// the CPU is shared among the groups in it (`cpu` in its `cgroup.subtree_control`), so each can
/// run at idle priority.
#[derive(Debug)]
struct Base {
    dir: PathBuf,
    rel: String,
    cpu: bool,
}

/// This process's own cgroup, as `/proc/self/cgroup` names it (cgroup v2's `0::` line).
fn own_rel() -> Option<String> {
    fs::read_to_string("/proc/self/cgroup").ok()?.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().trim_end_matches('/').to_owned())
}

impl Base {
    /// `name` under this process's own cgroup, made if it isn't there: `None` without a cgroup v2
    /// this process may move processes into.
    fn under_own(name: &str) -> Option<Base> {
        Self::under(&own_rel()?, name, false)
    }

    /// `name` under the cgroup `home` (this process's own, or the unit whose [`LEAF`] it runs in),
    /// made if it isn't there, with the CPU shared among its groups when `cpu` (the home shares it
    /// with its children): `None` without a cgroup v2 this process may move processes into.
    fn under(home: &str, name: &str, cpu: bool) -> Option<Base> {
        if !Path::new(ROOT).join("cgroup.controllers").is_file() {
            return None;
        }
        let own = own_rel()?;
        let rel = format!("{home}/{name}");
        let dir = Path::new(ROOT).join(rel.trim_start_matches('/'));
        if let Err(e) = fs::create_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            tracing::info!(dir = %dir.display(), error = %e, "agents' processes have no cgroups of their own here: they're found by their mark");
            return None;
        }
        // Moving a process in needs write access to the folder's cgroup.procs and to the cgroup
        // it leaves (ours: its children start in it).
        let own_procs = Path::new(ROOT).join(own.trim_start_matches('/')).join("cgroup.procs");
        let writable = |p: &Path| nix::unistd::access(p, nix::unistd::AccessFlags::W_OK).is_ok();
        if !writable(&dir.join("cgroup.procs")) || !writable(&own_procs) {
            tracing::info!(dir = %dir.display(), "agents' processes have no cgroups of their own here (not this process's to write): they're found by their mark");
            return None;
        }
        let cpu = cpu
            && share_cpu(&dir)
                .inspect_err(|why| tracing::warn!(dir = %dir.display(), why = %why, "the CPU isn't shared among kiln's groups here: they run at normal priority"))
                .is_ok();
        Some(Base { dir, rel, cpu })
    }

    /// Group `session` (made when `make`): at idle priority when `idle` and the CPU is shared here
    /// (docs/priority.md).
    fn group(&self, session: &str, make: bool, idle: bool) -> Option<Group> {
        let dir = self.dir.join(name_of(session)?);
        if make {
            if let Err(e) = fs::create_dir(&dir)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                tracing::warn!(session, error = %e, "an agent's processes get no cgroup of their own: they're found by their mark");
                return None;
            }
            if idle && self.cpu {
                let _ = mark_idle(&dir).inspect_err(|why| tracing::warn!(session, why = %why, "an agent's group runs at normal priority"));
            }
        } else if !dir.is_dir() {
            return None;
        }
        Some(Group { dir })
    }

    /// The agent session `pid` belongs to by its cgroup.
    fn session_of(&self, pid: u32) -> Option<String> {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        let name = path.strip_prefix(self.rel.as_str())?.strip_prefix('/')?.split('/').next()?;
        name_of(name).map(str::to_owned)
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

/// Where this process keeps its folders of groups, and whether the CPU is shared among them there
/// (docs/priority.md), worked out once: the machine's installed runner arranges its unit for that
/// where the unit is delegated to it; any other process keeps them under its own cgroup, as ever.
struct Arranged {
    /// The cgroup the folders go under, as `/proc/<pid>/cgroup` names it: the unit's, where kiln's
    /// own processes run in its [`LEAF`]; else this process's own.
    home: String,
    /// The home shares the CPU among its children, or why not.
    cpu: Result<(), String>,
}

fn arranged() -> &'static Arranged {
    static A: OnceLock<Arranged> = OnceLock::new();
    A.get_or_init(arrange_installed)
}

/// Passes over a unit's processes, each moving every one there into its leaf, before kiln gives up
/// sharing the CPU among the unit's children: only something forking all the while outlasts them.
const PASSES: usize = 10;

fn arrange_installed() -> Arranged {
    let own = own_rel().unwrap_or_default();
    if cfg!(test) || !holds() || !crate::os::supervise::started_by_supervisor() {
        return Arranged { home: own, cpu: Err("only the machine's installed runner arranges the cgroups it runs in".to_owned()) };
    }
    let (home, delegated) = match unit_of_leaf(&own) {
        // In its unit's leaf: put there by systemd (`DelegateSubgroup=`) or by a runner before this
        // one, and only a delegated unit has one.
        Some(unit) => (unit.to_owned(), Ok(())),
        None => (own.clone(), delegated(&own)),
    };
    let cpu = delegated.and_then(|()| arrange(&Path::new(ROOT).join(home.trim_start_matches('/'))));
    match &cpu {
        Ok(()) => tracing::info!(unit = %home, "legs and agents run at idle priority here: each in a cgroup of its own with cpu.idle"),
        Err(why) => tracing::info!(why = %why, "legs and agents run at normal priority here"),
    }
    Arranged { home, cpu }
}

/// The unit whose [`LEAF`] the cgroup `own` is, when it's one.
fn unit_of_leaf(own: &str) -> Option<&str> {
    own.rsplit_once('/').filter(|(unit, leaf)| *leaf == LEAF && is_unit(unit)).map(|(unit, _)| unit)
}

/// Whether `rel` names a systemd unit's cgroup (a service or a scope).
fn is_unit(rel: &str) -> bool {
    rel.rsplit('/').next().is_some_and(|u| u.ends_with(".service") || u.ends_with(".scope"))
}

/// Whether the service manager delegated the unit this process runs in (the last part of `rel`) to
/// it (systemd's `Delegate=yes`), asked of the user's manager for a unit under one and of the
/// system's otherwise: why not, when it didn't.
fn delegated(rel: &str) -> Result<(), String> {
    let unit = rel.rsplit('/').next().filter(|_| is_unit(rel)).ok_or_else(|| "kiln's runner doesn't run as a systemd unit of its own here".to_owned())?;
    let mut c = Command::new("systemctl");
    if rel.contains("/user@") {
        c.arg("--user");
    }
    c.args(["show", "--property=Delegate", "--value", unit]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // systemctl gives up on its service manager after its own timeout.
    let said = c.output().map_err(|e| format!("systemctl couldn't say whether its unit {unit} is delegated to it: {e}"))?;
    if String::from_utf8_lossy(&said.stdout).trim() == "yes" {
        Ok(())
    } else {
        Err(format!("its unit {unit} isn't delegated to it (systemd's Delegate=yes, which `kiln install` sets)"))
    }
}

/// Moves every process in the unit's cgroup `unit` (kiln's supervisor and runners, and what they
/// started outside any group) into its [`LEAF`], then shares the CPU among the unit's children:
/// cgroup v2 gives a cgroup's children a controller only while it holds no process itself. One that
/// forked as they moved is moved on the next pass.
fn arrange(unit: &Path) -> Result<(), String> {
    let controllers = fs::read_to_string(unit.join("cgroup.controllers")).map_err(|e| format!("its unit's cgroup can't be read: {e}"))?;
    if !controllers.split_whitespace().any(|c| c == "cpu") {
        return Err("its service manager gives its unit no cpu controller to share".to_owned());
    }
    let leaf = unit.join(LEAF);
    if let Err(e) = fs::create_dir(&leaf)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(format!("its unit's leaf for kiln's own processes couldn't be made: {e}"));
    }
    let mut last = String::new();
    for _ in 0..PASSES {
        for pid in fs::read_to_string(unit.join("cgroup.procs")).unwrap_or_default().split_whitespace() {
            // One that ended meanwhile is no matter.
            let _ = write_file(&leaf.join("cgroup.procs"), pid);
        }
        match share_cpu(unit) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("its unit's cgroup kept gaining processes, so the CPU couldn't be shared among its children ({last})"))
}

/// Writes `text` to the cgroup file `path`, which must be there (cgroupfs makes no files).
fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.write_all(text.as_bytes())
}

/// Shares the CPU among the children of the cgroup at `dir` (`+cpu` in its `cgroup.subtree_control`).
fn share_cpu(dir: &Path) -> Result<(), String> {
    write_file(&dir.join("cgroup.subtree_control"), "+cpu").map_err(|e| format!("its cgroup.subtree_control: {e}"))
}

/// The group at `dir` at idle priority (docs/priority.md): its `cpu.idle` (the cgroup's SCHED_IDLE,
/// Linux 5.15 and later), else the least weight a cgroup can have (`cpu.weight` 1).
fn mark_idle(dir: &Path) -> Result<(), String> {
    match write_file(&dir.join("cpu.idle"), "1") {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_file(&dir.join("cpu.weight"), "1").map_err(|e| format!("its cpu.weight: {e}")),
        r => r.map_err(|e| format!("its cpu.idle: {e}")),
    }
}

/// How legs and agents run here (docs/priority.md), for the runner's facts and its log.
pub fn priority() -> String {
    let shared = base().or_else(jobs_base).filter(|b| b.cpu);
    match (&arranged().cpu, shared) {
        (Ok(()), Some(b)) if b.dir.join("cpu.idle").is_file() => "idle: each leg's and agent's cgroup has cpu.idle 1".to_owned(),
        (Ok(()), Some(_)) => "the least CPU weight: each leg's and agent's cgroup has cpu.weight 1 (this kernel has no cpu.idle)".to_owned(),
        (Ok(()), None) => "normal: the CPU isn't shared among kiln's groups here".to_owned(),
        (Err(why), _) => format!("normal: {why}"),
    }
}

/// This runner's folder of agents' groups, worked out once (none in a process that doesn't start
/// agents for kiln).
fn base() -> Option<&'static Base> {
    if !holds() {
        return None;
    }
    static B: OnceLock<Option<Base>> = OnceLock::new();
    B.get_or_init(|| {
        if cfg!(test) {
            return Base::under_own(AGENTS);
        }
        if crate::os::supervise::started_by_supervisor() {
            let a = arranged();
            let b = Base::under(&a.home, AGENTS, a.cpu.is_ok())?;
            if b.cpu {
                // Agents' groups a runner made before the CPU was shared here, at idle priority now
                // (a run's may be a person's: it ends soon).
                for e in fs::read_dir(&b.dir).into_iter().flatten().flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with(super::RUN_GROUP) {
                        let _ = mark_idle(&e.path());
                    }
                }
            }
            return Some(b);
        }
        let b = Base::under_own(&format!("{AGENTS}-{}", std::process::id()))?;
        if let Some(own) = b.dir.parent() {
            prune_ended(own);
        }
        Some(b)
    })
    .as_ref()
}

/// The shared folder (`kiln-agents`) is the machine's installed runner's: it outlives that runner's
/// process for its next image or its blue/green successor to take up. Any other runner (a test's or
/// a lab's, one started by hand) may run in a live runner's cgroup (a CI job's commands run in the
/// runner's own), where that folder holds the live runner's agents, so it makes `kiln-agents-<pid>`
/// instead and never holds, paces or ends another runner's agents. Those of runners that have ended
/// go once nothing runs in them.
fn prune_ended(own: &Path) {
    prune_ended_of(own, AGENTS);
}

/// The `<folder>-<pid>` folders under `own` of processes that have ended, gone once nothing runs
/// in them.
fn prune_ended_of(own: &Path, folder: &str) {
    let prefix = format!("{folder}-");
    for e in fs::read_dir(own).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(&prefix).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if !Path::new(&format!("/proc/{pid}")).exists() && e.file_type().is_ok_and(|t| t.is_dir()) {
            prune(&e.path(), DEPTH);
        }
    }
}

/// Removes `dir` and the groups in it that nothing runs in, at once (a busy one stays).
fn prune(dir: &Path, depth: usize) {
    if depth > 0 {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                prune(&e.path(), depth - 1);
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

/// A session id as a folder's name: only what an id is made of (kiln's are hex), so it can't name
/// another folder.
fn name_of(session: &str) -> Option<&str> {
    (!session.is_empty() && session.len() <= 128 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(session)
}

/// An agent session's group of processes: a cgroup of its own.
#[derive(Debug, Clone)]
pub struct Group {
    dir: PathBuf,
}

impl Group {
    /// Agent session `session`'s group: the one it has (it outlives the harness, and the runner
    /// process too), else a new one. `None` where this machine can't hold processes together.
    pub fn of(session: &str) -> Option<Group> {
        base()?.group(session, true, true)
    }

    /// A group `name` for work a person asked for (their own reach run): held together as an
    /// agent's is, at the priority of what the person runs rather than an agent's idle one
    /// (docs/priority.md).
    pub fn of_person(name: &str) -> Option<Group> {
        base()?.group(name, true, false)
    }

    /// The group `session` has already (after a restart), without making one.
    pub fn existing(session: &str) -> Option<Group> {
        base()?.group(session, false, false)
    }

    /// Makes `cmd`'s process join the group before it runs anything: it writes itself into the
    /// group's `cgroup.procs` between fork and exec. If that write fails (the group went), the
    /// process still starts, outside it, and is found by its mark.
    pub fn enter(&self, cmd: &mut Command) {
        let Ok(mut procs) = fs::OpenOptions::new().write(true).open(self.dir.join("cgroup.procs")) else { return };
        #[allow(unsafe_code)]
        // SAFETY: the closure runs in the child between fork and exec, and only writes to a
        // descriptor opened beforehand (one write(2) call; nothing allocated, nothing locked).
        unsafe {
            cmd.pre_exec(move || {
                let _ = procs.write_all(b"0");
                Ok(())
            });
        }
    }

    /// `pid` was started with [`Group::enter`]: whether it's in the group (said when not).
    pub fn started(&self, pid: u32) -> bool {
        let inside = self.pids().contains(&pid);
        if !inside {
            tracing::warn!(pid, group = %self.dir.display(), "an agent's harness started outside its cgroup: what it starts is found by its mark");
        }
        inside
    }

    /// Every process in it now, also in cgroups its processes made inside it.
    pub fn pids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        collect(&self.dir, DEPTH, &mut out);
        out
    }

    /// Ends every process in it at once (`cgroup.kill`; before Linux 5.14, SIGKILL to each until
    /// none is left, so one forking meanwhile is caught on the next pass).
    pub fn kill(&self) {
        if fs::write(self.dir.join("cgroup.kill"), "1").is_ok() {
            return;
        }
        for _ in 0..20 {
            let pids = self.pids();
            if pids.is_empty() {
                return;
            }
            for p in pids.into_iter().filter_map(|p| i32::try_from(p).ok()).filter(|p| *p > 1) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Removes it once nothing runs in it (nothing happens while something does).
    pub fn remove(&self) {
        remove_tree(&self.dir, DEPTH);
    }

    /// Removes it, and the groups its processes made in it, once nothing runs in them, on a thread
    /// of its own (its last processes count for a moment after they're reaped): never waited for
    /// here. What still runs keeps its group, which goes with the next runner's start.
    pub fn release(&self) {
        let dir = self.dir.clone();
        if std::thread::Builder::new().name("kiln-group-release".into()).spawn(move || remove_tree(&dir, DEPTH)).is_err() {
            prune(&self.dir, DEPTH);
        }
    }
}

/// The folder that holds this runner's agents' groups (headroom paces them), when it has one.
pub fn agents_dir() -> Option<PathBuf> {
    base().map(|b| b.dir.clone())
}

/// A CI job's processes held together: a cgroup of its own, `kiln-jobs-<pid>/<job>` under the
/// runner's own, which each of its steps joins before it runs anything (`crate::runner::freeze`).
/// - The whole job can be frozen where it runs (`cgroup.freeze`) and let go on, whatever its steps
///   started, however that's parented (docs/ci.md W26-R22).
/// - A kiln one of its steps starts (a test's or a lab's runner) takes the cgroup it runs in as its
///   own, so it holds and paces its agents in this job's group, never in the runner's
///   `kiln-agents`, whatever build it is (the 2026-10-29 outage).
/// - Headroom paces it as it paces an agent's group, unless it's paused (docs/priority.md).
///
/// Where the CPU is shared among the jobs' groups, it runs at idle priority (`cpu.idle`); nothing
/// else it holds is counted or limited by it.
#[derive(Debug, Clone)]
pub struct Job {
    dir: PathBuf,
}

impl Job {
    /// Job `job`'s group, made now. `None` where this process may move no process into a cgroup
    /// v2: its steps' process groups are stopped instead (`crate::runner::freeze`).
    pub fn of(job: &str) -> Option<Job> {
        let base = jobs_base()?;
        let dir = base.dir.join(name_of(job)?);
        if let Err(e) = fs::create_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            tracing::info!(job, error = %e, "a job's processes get no cgroup of their own: its steps' process groups are stopped to freeze it");
            return None;
        }
        // A leg runs at idle priority where the CPU is shared among the jobs' groups (docs/priority.md).
        if base.cpu {
            let _ = mark_idle(&dir).inspect_err(|why| tracing::warn!(job, why = %why, "a job's group runs at normal priority"));
        }
        Some(Job { dir })
    }

    /// Makes `cmd`'s process join the group before it runs anything, as [`Group::enter`] does.
    pub fn enter(&self, cmd: &mut Command) {
        Group { dir: self.dir.clone() }.enter(cmd);
    }

    /// Its folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Moves process `pid` into it, started some other way (a step on a pty): what it started
    /// before it moved stays where it was.
    pub fn adopt(&self, pid: u32) -> bool {
        fs::write(self.dir.join("cgroup.procs"), pid.to_string()).is_ok()
    }

    /// Every process in it now.
    pub fn pids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        collect(&self.dir, DEPTH, &mut out);
        out
    }

    /// Freezes everything in it (`cgroup.freeze`), or lets it go on, and waits up to `within` for
    /// the kernel to say it has (`cgroup.events`). A process that joins it while it's frozen is
    /// frozen as it joins. Blocks: call it off the shards.
    pub fn freeze(&self, frozen: bool, within: Duration) -> Result<(), String> {
        fs::write(self.dir.join("cgroup.freeze"), if frozen { "1" } else { "0" }).map_err(|e| format!("its cgroup's freezer: {e}"))?;
        let deadline = std::time::Instant::now() + within;
        loop {
            if self.frozen() == frozen {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("its processes weren't all {} within {} s", if frozen { "stopped" } else { "going again" }, within.as_secs()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether the kernel says everything in it is frozen.
    pub fn frozen(&self) -> bool {
        fs::read_to_string(self.dir.join("cgroup.events")).is_ok_and(|t| t.lines().any(|l| l.trim() == "frozen 1"))
    }

    /// Gone once nothing runs in it: one try, a busy one goes as a later job starts.
    pub fn remove(&self) {
        let _ = fs::remove_dir(&self.dir);
    }

    /// Its `cgroup.freeze`, for tests to read.
    #[cfg(test)]
    pub fn freezer(&self) -> PathBuf {
        self.dir.join("cgroup.freeze")
    }
}

/// This runner process's folder of jobs' groups, worked out once; the folders of runner processes
/// that have ended go once nothing runs in them, what they hold let go on first: a job its runner
/// left frozen, gone, would be frozen for good (W26-R22).
fn jobs_base() -> Option<&'static Base> {
    static B: OnceLock<Option<Base>> = OnceLock::new();
    B.get_or_init(|| {
        let a = arranged();
        let b = Base::under(&a.home, &format!("{JOBS}-{}", std::process::id()), a.cpu.is_ok())?;
        if let Some(own) = b.dir.parent() {
            thaw_ended(own, JOBS);
            prune_ended_of(own, JOBS);
            // The one folder builds before kept every runner process's jobs in, once nothing runs there.
            prune(&own.join(JOBS), DEPTH);
        }
        Some(b)
    })
    .as_ref()
}

/// The folders of CI jobs' groups beside this runner's (`kiln-jobs-<pid>`, each of its unit's runner
/// processes'), for headroom to pace each job in them as it paces agents (docs/priority.md).
pub fn jobs_dirs() -> Vec<PathBuf> {
    let Some(home) = jobs_base().and_then(|b| b.dir.parent()) else { return Vec::new() };
    let prefix = format!("{JOBS}-");
    fs::read_dir(home).into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix) && e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect()
}

/// At a runner process's start: the jobs runner processes before it left frozen as they ended go
/// on, as a dead runner's jobs always did (docs/ci.md W26-R22), and their folders go once empty.
pub fn release_ended_jobs() {
    let _ = jobs_base();
}

/// Everything in the `<folder>-<pid>` folders under `own` of processes that have ended let go on
/// (`cgroup.freeze` 0, every group in them).
fn thaw_ended(own: &Path, folder: &str) {
    let prefix = format!("{folder}-");
    for e in fs::read_dir(own).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(&prefix).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if !Path::new(&format!("/proc/{pid}")).exists() && e.file_type().is_ok_and(|t| t.is_dir()) {
            thaw_tree(&e.path(), DEPTH);
        }
    }
}

fn thaw_tree(dir: &Path, depth: usize) {
    if fs::read_to_string(dir.join("cgroup.freeze")).is_ok_and(|v| v.trim() == "1") {
        let _ = fs::write(dir.join("cgroup.freeze"), "0");
    }
    if depth > 0 {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                thaw_tree(&e.path(), depth - 1);
            }
        }
    }
}

/// The groups of jobs that ended with something still in them ([`Job::remove`] found them busy),
/// gone once nothing runs in them: those `running` doesn't name.
pub fn prune_jobs(running: impl Fn(&str) -> bool) {
    let Some(b) = jobs_base() else { return };
    for e in fs::read_dir(&b.dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) && !running(&e.file_name().to_string_lossy()) {
            prune(&e.path(), DEPTH);
        }
    }
}

/// The agent session process `pid` belongs to by its group, when it's in one.
pub fn session_of(pid: u32) -> Option<String> {
    base()?.session_of(pid)
}

/// How this machine holds an agent's processes together.
pub fn how() -> String {
    match base() {
        Some(b) => format!("a cgroup per agent under {}", b.rel),
        None => "none (processes are found by their mark)".to_owned(),
    }
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<u32>) {
    if let Ok(text) = fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    if depth == 0 {
        return;
    }
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            collect(&e.path(), depth - 1, out);
        }
    }
}

fn remove_tree(dir: &Path, depth: usize) {
    if depth > 0 {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_tree(&e.path(), depth - 1);
            }
        }
    }
    // A group whose last processes were just killed counts them until they're reaped: busy for
    // a moment, then removable.
    for _ in 0..40 {
        match fs::remove_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::ResourceBusy => std::thread::sleep(Duration::from_millis(25)),
            _ => return,
        }
    }
}

/// A group in a folder of its own (`kiln-agents-test-<pid>` beside the runner's), so tests never
/// touch the runner's: the processes it holds, whose they are, and ending them.
#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 8 (plan 01a1a59c2b29 step 2): kiln's own processes in a unit's leaf make the unit the
    /// home of its folders; anywhere else, they stay where the process runs.
    #[test]
    fn g8_s2_the_unit_whose_leaf_kiln_runs_in_is_its_home() {
        let unit = "/user.slice/user-1000.slice/user@1000.service/app.slice/kiln-agent.service";
        assert_eq!(unit_of_leaf(&format!("{unit}/kiln")), Some(unit));
        assert_eq!(unit_of_leaf(unit), None);
        assert_eq!(unit_of_leaf("/user.slice/kiln"), None, "a leaf of something that isn't a unit");
        assert_eq!(unit_of_leaf(&format!("{unit}/kiln-agents/kiln")), None);
        assert!(is_unit(unit) && is_unit("/system.slice/run-1.scope") && !is_unit("/kiln.slice"));
    }

    /// Goal 8 (plan 01a1a59c2b29 step 2, docs/priority.md): in a unit delegated to kiln (a scope of
    /// the user's own here, its process standing in for kiln's supervisor), kiln's own processes
    /// move into the unit's leaf, the CPU is shared among the unit's children, and an agent's group
    /// in a folder there runs at idle priority with what it starts in it (`cpu.idle` 1), a person's
    /// run's at normal priority.
    #[test]
    fn g8_s2_a_delegated_units_groups_run_at_idle_priority() -> anyhow::Result<()> {
        use std::process::Stdio;
        let unit = format!("kiln-g8-test-{}.scope", std::process::id());
        let spawned = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes", &format!("--unit={unit}"), "--", "sleep", "300"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut held) = spawned else {
            eprintln!("no systemd-run here: skipped");
            return Ok(());
        };
        // Its cgroup, once systemd has put it there.
        let mut rel = None;
        for _ in 0..100 {
            rel = fs::read_to_string(format!("/proc/{}/cgroup", held.id())).ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().to_owned())).filter(|r| r.ends_with(&unit));
            if rel.is_some() || held.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let Some(rel) = rel else {
            let _ = held.kill();
            let _ = held.wait();
            eprintln!("no delegated scope of the user's own here: skipped");
            return Ok(());
        };
        let dir = Path::new(ROOT).join(rel.trim_start_matches('/'));
        let checked = (|| {
            arrange(&dir).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(fs::read_to_string(dir.join("cgroup.procs"))?.trim().is_empty(), "the unit's cgroup still holds a process");
            anyhow::ensure!(fs::read_to_string(dir.join(LEAF).join("cgroup.procs"))?.lines().any(|p| p.trim() == held.id().to_string()), "its process isn't in its leaf");
            anyhow::ensure!(fs::read_to_string(dir.join("cgroup.subtree_control"))?.split_whitespace().any(|c| c == "cpu"), "the CPU isn't shared among its children");
            let folder = Base::under(&rel, AGENTS, true).ok_or_else(|| anyhow::anyhow!("no folder under {}", dir.display()))?;
            anyhow::ensure!(folder.cpu, "the folder doesn't share the CPU among its groups");
            let agent = folder.group("01a1g8", true, true).ok_or_else(|| anyhow::anyhow!("no agent's group"))?;
            let person = folder.group("run-01a1g8", true, false).ok_or_else(|| anyhow::anyhow!("no person's group"))?;
            let mut cmd = Command::new("sleep");
            cmd.arg("30").stdin(Stdio::null());
            agent.enter(&mut cmd);
            let mut child = cmd.spawn()?;
            let inside = agent.started(child.id());
            let idle = fs::read_to_string(agent.dir.join("cpu.idle")).or_else(|_| fs::read_to_string(agent.dir.join("cpu.weight")))?;
            let persons = fs::read_to_string(person.dir.join("cpu.idle")).unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            agent.remove();
            person.remove();
            let _ = fs::remove_dir(&folder.dir);
            anyhow::ensure!(inside, "what an agent starts doesn't run in its group");
            anyhow::ensure!(idle.trim() == "1", "an agent's group isn't at idle priority: {idle}");
            anyhow::ensure!(persons.trim() != "1", "a person's run's group is at idle priority: {persons}");
            Ok(())
        })();
        let _ = held.kill();
        let _ = held.wait();
        let _ = fs::remove_dir(dir.join(LEAF));
        checked
    }

    /// W26-R22: a job its runner process left frozen as it ended goes on once another starts: its
    /// group is let go of, and the folder goes once nothing runs in it.
    #[test]
    fn w26_r22_a_job_left_frozen_by_an_ended_runner_goes_on() -> anyhow::Result<()> {
        // This process's own start first: it lets go of ended runners' jobs once, and a test beside
        // this one starting it meanwhile would let this one's go before it's looked at.
        let _ = jobs_base();
        // A runner process that has ended: no such process.
        let gone = (4_000_000..4_100_000).find(|p| !Path::new(&format!("/proc/{p}")).exists()).ok_or_else(|| anyhow::anyhow!("no free pid"))?;
        let Some(base) = Base::under_own(&format!("{JOBS}-{gone}")) else {
            eprintln!("no cgroup v2 this test may write: skipped");
            return Ok(());
        };
        let job = Job { dir: base.dir.join("01a1job") };
        fs::create_dir_all(&job.dir)?;
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while :; do :; done"]);
        job.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        let checked = (|| {
            job.freeze(true, Duration::from_secs(5)).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(job.frozen(), "frozen");
            let own = base.dir.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
            thaw_ended(own, JOBS);
            for _ in 0..100 {
                if !job.frozen() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::ensure!(!job.frozen(), "the ended runner's job goes on");
            Ok(())
        })();
        let _ = child.kill();
        let _ = child.wait();
        remove_tree(&base.dir, 2);
        checked?;
        anyhow::ensure!(!base.dir.exists(), "its folder goes once empty");
        Ok(())
    }

    /// A runner that isn't the installed one never takes another's folder, and those of runners
    /// that ended go once empty (the 2026-10-29 outage: a lab runner paced production's agents).
    #[test]
    fn folders_of_ended_runners_go_once_empty_and_a_live_ones_stays() -> anyhow::Result<()> {
        let dead = {
            let mut c = Command::new("true").spawn()?;
            c.wait()?;
            c.id()
        };
        let own = tempfile::tempdir()?;
        let ended_empty = own.path().join(format!("{AGENTS}-{dead}"));
        let ended_busy = own.path().join(format!("{AGENTS}-{}", u32::MAX));
        let live = own.path().join(format!("{AGENTS}-{}", std::process::id()));
        let shared = own.path().join(AGENTS);
        fs::create_dir_all(ended_empty.join("s1"))?;
        fs::create_dir_all(ended_busy.join("s2"))?;
        fs::write(ended_busy.join("s2").join("cgroup.procs"), "123\n")?;
        fs::create_dir_all(live.join("s3"))?;
        fs::create_dir_all(shared.join("s4"))?;
        prune_ended(own.path());
        anyhow::ensure!(!ended_empty.exists(), "an ended runner's empty folder stays");
        anyhow::ensure!(ended_busy.join("s2").exists(), "one with something in it went");
        anyhow::ensure!(live.join("s3").exists() && shared.join("s4").exists(), "a live runner's folder or the shared one went");
        Ok(())
    }

    /// A CI step's kiln (a lab's runner) takes the cgroup it runs in as its own: in its job's
    /// group, the agents' folder it makes is inside that group, never the runner's `kiln-agents`
    /// (the 2026-10-29 outage). The job's group goes once its step has ended, and not before.
    #[test]
    fn a_step_s_kiln_makes_its_agents_folder_inside_its_job_s_group() -> anyhow::Result<()> {
        // A folder of its own: the jobs' one (`kiln-jobs-test-<pid>`) holds other tests' jobs, which
        // its pruning at the end would race.
        let Some(base) = Base::under_own(&format!("{JOBS}-steps-{}", std::process::id())) else {
            eprintln!("no cgroup v2 this test may write: skipped");
            return Ok(());
        };
        let job = base.group("01a1job", true, true).ok_or_else(|| anyhow::anyhow!("no group under {}", base.dir.display()))?;
        // The step: a shell that makes `kiln-agents` under its own cgroup, as any kiln does.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "own=$(sed -n 's/^0:://p' /proc/self/cgroup); mkdir /sys/fs/cgroup$own/kiln-agents && echo $own; sleep 30"]).stdout(std::process::Stdio::piped()).stdin(std::process::Stdio::null());
        job.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        let mut line = String::new();
        if let Some(out) = child.stdout.take() {
            use std::io::BufRead as _;
            std::io::BufReader::new(out).read_line(&mut line)?;
        }
        let made = Path::new(ROOT).join(line.trim().trim_start_matches('/')).join("kiln-agents");
        let inside = made.starts_with(&job.dir) && made.is_dir();
        job.release();
        let kept_while_running = job.dir.is_dir();
        let _ = child.kill();
        let _ = child.wait();
        job.release();
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(30));
            !job.dir.exists()
        });
        prune(&base.dir, 1);
        anyhow::ensure!(inside, "the step's agents folder {} isn't inside its job's group {}", made.display(), job.dir.display());
        anyhow::ensure!(kept_while_running, "the job's group went while its step ran");
        anyhow::ensure!(gone, "the job's group stayed after its step ended");
        Ok(())
    }

    #[test]
    fn what_an_agent_starts_stays_in_its_group_however_it_s_parented() -> anyhow::Result<()> {
        assert_eq!(name_of("../etc"), None);
        assert_eq!(name_of("01a1-b_c"), Some("01a1-b_c"));
        let Some(base) = Base::under_own(&format!("kiln-agents-test-{}", std::process::id())) else {
            eprintln!("no cgroup v2 this test may write: skipped");
            return Ok(());
        };
        let group = base.group("01a1test", true, true).ok_or_else(|| anyhow::anyhow!("no group under {}", base.dir.display()))?;
        // A harness that starts a command, and one that leaves its session and its parent (a
        // daemon), then waits.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 61 & setsid sh -c 'sleep 62 & exit 0' & sleep 63"]).stdin(std::process::Stdio::null());
        group.enter(&mut cmd);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        // The daemon's sleep left the harness's tree (its parent is init, or the nearest
        // subreaper, now), not its group.
        let parent = |p: &u32| fs::read_to_string(format!("/proc/{p}/stat")).ok().and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().nth(1).map(str::to_owned)));
        let orphaned = |pids: &[u32]| pids.iter().filter(|p| **p != pid).any(|p| parent(p).is_some_and(|pp| pids.iter().all(|q| q.to_string() != pp)));
        let mut pids = Vec::new();
        for _ in 0..100 {
            pids = group.pids();
            if pids.len() >= 3 && orphaned(&pids) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let checked = (|| {
            anyhow::ensure!(group.started(pid), "the harness is in its group");
            // The harness (or the command it became), its command and the daemon's sleep.
            anyhow::ensure!(pids.len() >= 3, "{pids:?}");
            anyhow::ensure!(pids.iter().all(|p| base.session_of(*p).as_deref() == Some("01a1test")), "each is the agent's by its cgroup");
            anyhow::ensure!(base.session_of(std::process::id()).is_none(), "the runner isn't an agent's");
            anyhow::ensure!(orphaned(&pids), "a process of the agent's whose parent isn't the agent's: {pids:?}");
            Ok(())
        })();
        group.kill();
        let _ = child.wait();
        for _ in 0..50 {
            if group.pids().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let ended = group.pids().is_empty();
        group.remove();
        let removed = !group.dir.exists();
        remove_tree(&base.dir, 1);
        checked?;
        anyhow::ensure!(ended, "ending the group ends all of it, the daemon too");
        anyhow::ensure!(removed, "an empty group goes");
        Ok(())
    }
}
