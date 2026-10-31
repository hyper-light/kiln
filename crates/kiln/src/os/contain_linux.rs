//! Each agent session's processes held together by the kernel: a cgroup (v2) of its own,
//! `kiln-agents/<session>` under the cgroup the runner runs in (`kiln-agents-<pid>/<session>` for a
//! runner that isn't the machine's installed one: [`prune_ended`]); and each CI job's steps on this
//! machine, `kiln-jobs/<job>` ([`job_group`]). Its harness joins it between fork
//! and exec, before it runs anything, so everything it starts is in it too, however that's
//! parented later (a command that daemonized, `setsid`, a double fork): no process leaves its
//! cgroup by forking. Listing an agent's processes is reading its `cgroup.procs`; whose a process
//! is, one read of `/proc/<pid>/cgroup`; ending them all, one write to `cgroup.kill`. Nothing is
//! scanned.
//!
//! Only membership is asked of the cgroups (no controllers), so the runner's own cgroup needs no
//! delegation: any cgroup v2 it may write will do (a systemd user unit, a container with its own
//! cgroup namespace). Where there is none (cgroup v1, a read-only cgroup filesystem, a cgroup
//! another user owns), agents have no group and their processes are found by their mark
//! (`crate::harness::survivors`), as before.

use std::fs;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

/// Where cgroup v2 is mounted.
const ROOT: &str = "/sys/fs/cgroup";
/// The folder under the runner's own cgroup that holds agents' groups (in tests, one of their own:
/// tests often run in a live runner's cgroup).
const AGENTS: &str = if cfg!(test) { "kiln-agents-test" } else { "kiln-agents" };
/// The folder under the runner's own cgroup that holds its CI jobs' groups (never paced).
const JOBS: &str = if cfg!(test) { "kiln-jobs-test" } else { "kiln-jobs" };
/// How deep cgroups an agent's processes made of their own are followed.
const DEPTH: usize = 8;

/// A folder of agents' groups: where it is, and its path as `/proc/<pid>/cgroup` names it.
#[derive(Debug)]
struct Base {
    dir: PathBuf,
    rel: String,
}

impl Base {
    /// `name` under this process's own cgroup, made if it isn't there: `None` without a cgroup v2
    /// this process may move processes into.
    fn under_own(name: &str) -> Option<Base> {
        if !Path::new(ROOT).join("cgroup.controllers").is_file() {
            return None;
        }
        let own = fs::read_to_string("/proc/self/cgroup").ok()?.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().trim_end_matches('/').to_owned())?;
        let rel = format!("{own}/{name}");
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
        Some(Base { dir, rel })
    }

    fn group(&self, session: &str, make: bool) -> Option<Group> {
        let dir = self.dir.join(name_of(session)?);
        if make {
            if let Err(e) = fs::create_dir(&dir)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                tracing::warn!(session, error = %e, "an agent's processes get no cgroup of their own: they're found by their mark");
                return None;
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

/// This runner's folder of agents' groups, worked out once (none in a process that doesn't start
/// agents for kiln).
fn base() -> Option<&'static Base> {
    if !holds() {
        return None;
    }
    static B: OnceLock<Option<Base>> = OnceLock::new();
    B.get_or_init(|| {
        if cfg!(test) || crate::os::supervise::started_by_supervisor() {
            return Base::under_own(AGENTS);
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
    let prefix = format!("{AGENTS}-");
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
        base()?.group(session, true)
    }

    /// The group `session` has already (after a restart), without making one.
    pub fn existing(session: &str) -> Option<Group> {
        base()?.group(session, false)
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

/// CI job `job`'s group (`kiln-jobs/<job>` under the runner's cgroup), for its steps' processes on
/// this machine. A kiln one of them starts (a test's or a lab's runner) takes the cgroup it runs in
/// as its own, so it holds and paces its agents in this job's group, never in this runner's
/// `kiln-agents`, whatever build it is (one from a branch older than `kiln-agents-<pid>` too).
/// Headroom never paces it. `None` where agents get no groups (see [`Base::under_own`]).
pub fn job_group(job: &str) -> Option<Group> {
    if !holds() {
        return None;
    }
    static B: OnceLock<Option<Base>> = OnceLock::new();
    B.get_or_init(|| {
        let b = Base::under_own(JOBS)?;
        // Those of jobs that ended while the runner before this one ran.
        for e in fs::read_dir(&b.dir).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                prune(&e.path(), DEPTH);
            }
        }
        Some(b)
    })
    .as_ref()?
    .group(job, true)
}

/// The folder that holds this runner's agents' groups (headroom paces them), when it has one.
pub fn agents_dir() -> Option<PathBuf> {
    base().map(|b| b.dir.clone())
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
        let Some(base) = Base::under_own(&format!("{JOBS}-{}", std::process::id())) else {
            eprintln!("no cgroup v2 this test may write: skipped");
            return Ok(());
        };
        let job = base.group("01a1job", true).ok_or_else(|| anyhow::anyhow!("no group under {}", base.dir.display()))?;
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
        let group = base.group("01a1test", true).ok_or_else(|| anyhow::anyhow!("no group under {}", base.dir.display()))?;
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
