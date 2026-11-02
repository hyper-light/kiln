//! A job frozen where it runs, and thawed (docs/ci.md W26-R22): Pause in Ship stops its work in
//! place, the runner keeping it, and Resume lets it go on from where it was.
//!
//! - Linux: the job's cgroup (`crate::os::contain::Job`, `kiln-jobs-<pid>/<job>` under the
//!   runner's own), which each of its steps joins before it runs anything ([`enter`]), frozen whole
//!   (`cgroup.freeze`): whatever its steps started, however that's parented. A step that starts
//!   while the job is frozen is frozen as it joins.
//! - Elsewhere on Unix, and on Linux without a cgroup v2 this runner may write: its steps' process
//!   groups, stopped (SIGSTOP, then SIGCONT). A step that starts while the job is frozen is stopped
//!   as it starts.
//! - Windows: each of its steps is in a job object of its own (`crate::os::contain::Tree`), put in
//!   it before it ran anything, so whatever it starts is there however that's parented; a step on a
//!   pty (ConPTY starts it outside any job) by its process tree. Every one of their processes is
//!   suspended where it is (`NtSuspendProcess`), listed again until none is new, and exactly those
//!   are let go on (`NtResumeProcess`). A step that starts while the job is frozen is suspended as it
//!   starts.
//! - Its job container, either way: paused (`docker pause`, then `unpause`).
//!
//! Its time limit doesn't run while it is frozen: [`effective`] is a deadline set before, pushed
//! back by every while it has been. The server asks with `ServerMsg::Freeze`, only of runners
//! announcing `proto::CAP_FREEZE`: those on Unix and Windows. Nothing thaws a job by itself but its
//! cancelling ([`thaw`]): a stopped process can't hear it's to end, and a paused container takes no
//! `docker exec`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[cfg(target_os = "linux")]
use crate::os::contain::Job as Cgroup;

/// Elsewhere a job has no cgroup: its steps' process groups are stopped instead.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone)]
enum Cgroup {}

#[cfg(not(target_os = "linux"))]
impl Cgroup {
    fn of(_job: &str) -> Option<Cgroup> {
        None
    }
    fn enter(&self, _cmd: &mut std::process::Command) {
        match *self {}
    }
    fn adopt(&self, _pid: u32) -> bool {
        match *self {}
    }
    fn freeze(&self, _frozen: bool, _within: Duration) -> Result<(), String> {
        match *self {}
    }
    fn remove(&self) {
        match *self {}
    }
}

#[derive(Default)]
struct State {
    /// Its cgroup, which its steps join (Linux).
    cgroup: Option<Cgroup>,
    /// The process groups its steps started: what's stopped where it has no cgroup. On Windows, its
    /// pty steps' shells, whose process trees are suspended.
    groups: Vec<i32>,
    /// Its steps' jobs, whose processes are suspended (Windows).
    #[cfg(windows)]
    trees: Vec<crate::os::contain::Tree>,
    /// The processes it suspended, while it's frozen: exactly those are let go on (Windows).
    #[cfg(windows)]
    held: Vec<u32>,
    /// A step that starts now is suspended as it starts: set as freezing begins, cleared as
    /// letting go on does, with `held` taken in the same breath (Windows).
    #[cfg(windows)]
    hold_new: bool,
    /// Its job container, and the CLI that runs it.
    container: Option<(super::docker::Cli, String)>,
    /// Since when it has been frozen, while it is.
    since: Option<Instant>,
    /// How long it was frozen before, all told.
    total: Duration,
}

/// One running job's part in freezing: what of it can be stopped, and for how long it has been.
#[derive(Default)]
pub struct Job(Mutex<State>);

static JOBS: LazyLock<Mutex<HashMap<String, Arc<Job>>>> = LazyLock::new(Mutex::default);

/// How long `docker pause` and `unpause` are given: the runner's loop waits on them to answer.
const DOCKER_WITHIN: Duration = Duration::from_secs(10);
/// How long the kernel is given to say a job's cgroup is frozen, or going again.
#[cfg(unix)]
const CGROUP_WITHIN: Duration = Duration::from_secs(5);

/// The job `job` starts here: it can be frozen until [`Started`] is dropped. On Linux it gets a
/// cgroup of its own, which its steps join ([`enter`]).
pub fn begin(job: &str) -> Started {
    #[cfg(target_os = "linux")]
    crate::os::contain::prune_jobs(|name| JOBS.lock().contains_key(name));
    let j = Job::default();
    j.0.lock().cgroup = Cgroup::of(job);
    JOBS.lock().insert(job.to_owned(), Arc::new(j));
    Started(job.to_owned())
}

/// A running job's place in the registry, given up when it ends, and its cgroup with it.
pub struct Started(String);

impl Drop for Started {
    fn drop(&mut self) {
        let Some(j) = JOBS.lock().remove(&self.0) else { return };
        let st = j.0.lock();
        // Ended frozen (it never should: cancelling thaws it), what it left isn't left frozen.
        #[cfg(unix)]
        if st.since.is_some() {
            for g in &st.groups {
                let _ = signal_group(*g, false);
            }
        }
        // Whatever it still holds suspended goes on (Windows).
        #[cfg(windows)]
        for p in &st.held {
            crate::os::contain::hold(*p, false);
        }
        if let Some(cg) = &st.cgroup {
            if st.since.is_some() {
                let _ = cg.freeze(false, Duration::ZERO);
            }
            cg.remove();
        }
    }
}

/// The job `job`, while it runs here.
pub fn of(job: &str) -> Option<Arc<Job>> {
    JOBS.lock().get(job).cloned()
}

/// Makes step `cmd` of job `job` join the job's cgroup before it runs anything (Linux; elsewhere
/// nothing: its process group is stopped instead, [`Job::group`]).
pub fn enter(job: &str, cmd: &mut std::process::Command) {
    if let Some(cg) = of(job).and_then(|j| j.0.lock().cgroup.clone()) {
        cg.enter(cmd);
    }
}

/// A step of job `job` started some other way (on a pty), moved into the job's cgroup as it starts
/// (Linux).
pub fn adopt(job: &str, pid: u32) {
    if let Some(cg) = of(job).and_then(|j| j.0.lock().cgroup.clone())
        && !cg.adopt(pid)
    {
        tracing::info!(job, pid, "a step on a pty stays outside its job's cgroup: its process group is stopped to freeze it");
    }
}

impl Job {
    /// A step started through `crate::proc::spawn_group`: its process group (Unix), or its job
    /// (Windows), stopped at once if the job is frozen now.
    pub fn step(&self, running: &crate::proc::Running) {
        #[cfg(unix)]
        self.group(running.pgid);
        #[cfg(windows)]
        {
            let tree = running.tree();
            let held = {
                let mut st = self.0.lock();
                st.trees.push(tree.clone());
                st.hold_new.then(|| st.held.clone())
            };
            if let Some(held) = held {
                self.keep(suspend_more(&[tree], &[], &held));
            }
        }
    }

    /// Processes suspended as a step started: held while the job is frozen, let go on at once if it
    /// was let go on meanwhile (Windows).
    #[cfg(windows)]
    fn keep(&self, more: Vec<u32>) {
        let mut st = self.0.lock();
        if st.hold_new {
            st.held.extend(more);
        } else {
            drop(st);
            for p in more {
                crate::os::contain::hold(p, false);
            }
        }
    }

    /// A step's process group (on Windows, a pty step's shell: its process tree). Without a cgroup,
    /// it's stopped at once if the job is frozen now (in one, it joined frozen).
    pub fn group(&self, pgid: i32) {
        #[cfg(unix)]
        {
            let mut st = self.0.lock();
            st.groups.push(pgid);
            if st.since.is_some() && st.cgroup.is_none() {
                let _ = signal_group(pgid, true);
            }
        }
        #[cfg(windows)]
        {
            let held = {
                let mut st = self.0.lock();
                st.groups.push(pgid);
                st.hold_new.then(|| st.held.clone())
            };
            if let Some(held) = held {
                self.keep(suspend_more(&[], &[pgid], &held));
            }
        }
    }

    /// Its job container, once it's up.
    pub fn container(&self, cli: super::docker::Cli, id: String) {
        self.0.lock().container = Some((cli, id));
    }

    /// Whether it is frozen now.
    pub fn frozen(&self) -> bool {
        self.0.lock().since.is_some()
    }

    /// Freezes it (`true`) or lets it go on (`false`); asked again as it is, nothing changes. Why
    /// not, when nothing of it here could be stopped or let go. Freezing that fails half-way is
    /// undone; done again after, what was done already counts as done.
    pub async fn freeze(&self, frozen: bool) -> Result<(), String> {
        let (cgroup, groups, container, steps) = {
            let st = self.0.lock();
            if st.since.is_some() == frozen {
                return Ok(());
            }
            #[cfg(windows)]
            let steps = st.trees.len();
            #[cfg(not(windows))]
            let steps = 0;
            (st.cgroup.clone(), st.groups.clone(), st.container.clone(), steps)
        };
        if cgroup.is_none() && groups.is_empty() && container.is_none() && steps == 0 {
            return Err("nothing of it runs here yet that could be frozen".to_owned());
        }
        if frozen {
            self.processes(cgroup.clone(), &groups, true).await?;
            if let Some((cli, id)) = &container
                && let Err(e) = pause(cli, id, true).await
            {
                let _ = self.processes(cgroup, &groups, false).await;
                return Err(e);
            }
        } else {
            if let Some((cli, id)) = &container {
                pause(cli, id, false).await?;
            }
            self.processes(cgroup, &groups, false).await?;
        }
        let mut st = self.0.lock();
        match (frozen, st.since.take()) {
            (true, _) => st.since = Some(Instant::now()),
            (false, Some(since)) => st.total += since.elapsed(),
            (false, None) => {}
        }
        Ok(())
    }

    /// A deadline set before, pushed back by every while the job has been frozen. While it is
    /// frozen, a second from now: looked at again then.
    pub fn effective(&self, base: Instant) -> Instant {
        let st = self.0.lock();
        match st.since {
            Some(_) => Instant::now() + Duration::from_secs(1),
            None => base + st.total,
        }
    }

    /// How long it has been frozen, all told, the while it is now included.
    fn frozen_for(&self) -> Duration {
        let st = self.0.lock();
        st.total + st.since.map_or(Duration::ZERO, |s| s.elapsed())
    }

    /// Its processes stopped or let go on: its cgroup whole, or its steps' process groups.
    #[cfg(unix)]
    async fn processes(&self, cgroup: Option<Cgroup>, groups: &[i32], frozen: bool) -> Result<(), String> {
        if let Some(cg) = cgroup {
            return crate::rt::blocking(move || cg.freeze(frozen, CGROUP_WITHIN)).await.map_err(|e| format!("{e:#}"))?;
        }
        let failed: Vec<String> = groups.iter().filter_map(|g| signal_group(*g, frozen).err()).collect();
        if failed.is_empty() { Ok(()) } else { Err(failed.join("; ")) }
    }

    /// Its processes suspended where they are, or exactly those it suspended let go on: every one
    /// in its steps' jobs, and under its pty steps' shells. None running between its steps is no
    /// failure: the next step is suspended as it starts.
    #[cfg(windows)]
    async fn processes(&self, _cgroup: Option<Cgroup>, groups: &[i32], frozen: bool) -> Result<(), String> {
        if frozen {
            // From here a step that starts suspends itself; those already started are suspended here.
            let (trees, held) = {
                let mut st = self.0.lock();
                st.hold_new = true;
                (st.trees.clone(), st.held.clone())
            };
            let roots = groups.to_vec();
            let more = crate::rt::blocking(move || suspend_more(&trees, &roots, &held)).await.map_err(|e| format!("{e:#}"))?;
            self.0.lock().held.extend(more);
        } else {
            let held = {
                let mut st = self.0.lock();
                st.hold_new = false;
                std::mem::take(&mut st.held)
            };
            crate::rt::blocking(move || {
                for p in held {
                    crate::os::contain::hold(p, false);
                }
            })
            .await
            .map_err(|e| format!("{e:#}"))?;
        }
        Ok(())
    }
}

/// The processes in `trees` and under `roots` that aren't among `held` yet, suspended, listed again
/// until none is new (one could start another as the rest are being suspended): those it suspended.
#[cfg(windows)]
fn suspend_more(trees: &[crate::os::contain::Tree], roots: &[i32], held: &[u32]) -> Vec<u32> {
    let mut tried: Vec<u32> = held.to_vec();
    let mut more = Vec::new();
    // Each round reaches a process it hadn't: a fork bomb is let off after a while.
    for _ in 0..64 {
        let listed: Vec<u32> = trees.iter().flat_map(crate::os::contain::Tree::pids).chain(roots.iter().filter_map(|r| u32::try_from(*r).ok()).flat_map(crate::os::contain::tree_of)).collect();
        let new: Vec<u32> = listed.into_iter().filter(|p| !tried.contains(p)).collect();
        if new.is_empty() {
            break;
        }
        for p in new {
            tried.push(p);
            // One that ended meanwhile isn't reached, and has nothing left to stop.
            if crate::os::contain::hold(p, true) {
                more.push(p);
            }
        }
    }
    more
}

/// Its job container paused or let go on. One already as asked (a freeze undone half-way) is
/// left so.
async fn pause(cli: &super::docker::Cli, id: &str, frozen: bool) -> Result<(), String> {
    let verb = if frozen { "pause" } else { "unpause" };
    match cli.run(&[verb.to_owned(), id.to_owned()], &BTreeMap::new(), DOCKER_WITHIN).await {
        Ok(_) => Ok(()),
        Err(e) if format!("{e:#}").contains(if frozen { "already paused" } else { "not paused" }) => Ok(()),
        Err(e) => Err(format!("its container: docker {verb}: {e:#}")),
    }
}

/// A deadline set before for `job`, pushed back by its frozen time ([`Job::effective`]); itself
/// for a job that isn't running here.
pub fn effective(job: &str, base: Instant) -> Instant {
    of(job).map_or(base, |j| j.effective(base))
}

/// A deadline set now for `job` (`at`, as the clock reads), as [`effective`] reads it: only the
/// while the job is frozen from now on pushes it back, not the while it was before (a step's own
/// time limit, set as it starts).
pub fn base(job: &str, at: Instant) -> Instant {
    of(job).map_or(at, |j| at.checked_sub(j.frozen_for()).unwrap_or(at))
}

/// The job `job`, frozen, let go on as it's cancelled: a stopped process can't hear it's to end,
/// and a paused container takes no `docker exec`.
pub async fn thaw(job: &str) {
    if let Some(j) = of(job).filter(|j| j.frozen())
        && let Err(why) = j.freeze(false).await
    {
        tracing::warn!(%job, %why, "couldn't let a frozen job go on as it ends");
    }
}

/// A step's process group stopped or let go on. One whose processes have all ended is no
/// failure: nothing of it is left to stop.
#[cfg(unix)]
fn signal_group(pgid: i32, stop: bool) -> Result<(), String> {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    match killpg(Pid::from_raw(pgid), if stop { Signal::SIGSTOP } else { Signal::SIGCONT }) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(format!("process group {pgid}: {e}")),
    }
}


#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// CPU time a process has used, in clock ticks (Linux's `/proc/<pid>/stat`); 0 elsewhere.
    fn cpu(pid: u32) -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let fields: Vec<&str> = stat.rsplit_once(')').map(|(_, rest)| rest.split_whitespace().collect()).unwrap_or_default();
        fields.get(11).and_then(|u| u.parse::<u64>().ok()).unwrap_or(0) + fields.get(12).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
    }

    /// A step counting as fast as it can, and a process it leaves behind in a session of its own
    /// (a daemon), both counting; joined to job `job`'s cgroup where it has one.
    fn busy_step(job: &str) -> anyhow::Result<std::process::Child> {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "setsid sh -c 'i=0; while :; do i=$((i+1)); done' & i=0; while :; do i=$((i+1)); done"]).process_group(0);
        enter(job, &mut cmd);
        Ok(cmd.spawn()?)
    }

    /// `root` and every process under it: the step and the daemon it left (Linux; `root` alone
    /// elsewhere).
    fn tree(root: u32) -> Vec<u32> {
        let mut out = vec![root];
        let mut i = 0;
        while let Some(p) = out.get(i).copied() {
            let kids = std::fs::read_to_string(format!("/proc/{p}/task/{p}/children")).unwrap_or_default();
            out.extend(kids.split_whitespace().filter_map(|k| k.parse::<u32>().ok()));
            i += 1;
        }
        out
    }

    fn kill_all(pids: &[u32]) {
        for p in pids.iter().filter_map(|p| i32::try_from(*p).ok()) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), nix::sys::signal::Signal::SIGKILL);
        }
    }

    /// W26-R22: a job frozen where it runs uses no CPU while it is, the daemon it left included,
    /// and goes on after; on Linux its cgroup says frozen (`cgroup.freeze` 1). Its deadline is
    /// pushed back by the while it was frozen, a step's deadline set after by none of it; cancelling
    /// lets it go on.
    #[test]
    fn w26_r22_a_job_frozen_in_place_stops_and_goes_on() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let started = begin("job-w26-r22");
            let job = of("job-w26-r22").ok_or_else(|| anyhow::anyhow!("not begun"))?;
            #[cfg(target_os = "linux")]
            let freezer = job.0.lock().cgroup.as_ref().map(Cgroup::freezer);
            eprintln!("frozen by {}", if job.0.lock().cgroup.is_some() { "its cgroup" } else { "its process groups (no cgroup v2 to write here)" });
            let mut child = busy_step("job-w26-r22")?;
            job.group(i32::try_from(child.id())?);
            crate::rt::sleep(Duration::from_millis(300)).await;
            let pids = tree(child.id());
            let base = Instant::now() + Duration::from_secs(60);
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            assert!(job.frozen());
            #[cfg(target_os = "linux")]
            if let Some(f) = &freezer {
                assert_eq!(std::fs::read_to_string(f)?.trim(), "1", "its cgroup.freeze says 1");
            }
            crate::rt::sleep(Duration::from_millis(200)).await;
            if cfg!(target_os = "linux") {
                assert!(pids.len() >= 2, "the step and the daemon it left: {pids:?}");
                // Its cgroup frozen whole, the daemon included; without a cgroup v2 to write (as in
                // CI's pods) its steps' process groups are stopped, which the setsid daemon has left.
                let stopped: Vec<u32> = if job.0.lock().cgroup.is_some() { pids.clone() } else { vec![child.id()] };
                let a: Vec<u64> = stopped.iter().map(|p| cpu(*p)).collect();
                crate::rt::sleep(Duration::from_millis(500)).await;
                let b: Vec<u64> = stopped.iter().map(|p| cpu(*p)).collect();
                assert_eq!(a, b, "nothing of a frozen job uses CPU: {stopped:?} of {pids:?}");
            }
            // While frozen, a deadline is looked at again a second on, never reached.
            assert!(job.effective(base) < Instant::now() + Duration::from_secs(2));
            // Frozen again: nothing changes.
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            job.freeze(false).await.map_err(|e| anyhow::anyhow!(e))?;
            assert!(!job.frozen());
            #[cfg(target_os = "linux")]
            if let Some(f) = &freezer {
                assert_eq!(std::fs::read_to_string(f)?.trim(), "0");
            }
            // Thawed: the deadline falls later by the while it was frozen (at least 700 ms here).
            assert!(job.effective(base) >= base + Duration::from_millis(700));
            // A step's deadline set now isn't pushed back by the while the job was frozen before.
            let step = Instant::now() + Duration::from_secs(60);
            assert!(job.effective(super::base("job-w26-r22", step)) <= step + Duration::from_millis(50), "the while before doesn't count for a deadline set after");
            if cfg!(target_os = "linux") {
                let a: u64 = pids.iter().map(|p| cpu(*p)).sum();
                crate::rt::sleep(Duration::from_millis(500)).await;
                assert!(pids.iter().map(|p| cpu(*p)).sum::<u64>() > a, "thawed, it goes on");
            }
            // Cancelled while frozen: let go on first, so it hears it's to end.
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            thaw("job-w26-r22").await;
            assert!(!job.frozen(), "cancelling lets a frozen job go on");
            kill_all(&pids);
            let _ = child.wait();
            drop(started);
            assert!(of("job-w26-r22").is_none(), "an ended job is no longer here to freeze");
            Ok(())
        })
    }

    /// W26-R22, where a job has no cgroup (macOS, or a Linux runner with none it may write): its
    /// steps' process groups are stopped, a step that starts while it's frozen as it starts.
    #[test]
    fn w26_r22_without_a_cgroup_its_process_groups_are_stopped() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let job = Arc::new(Job::default());
            JOBS.lock().insert("job-w26-r22-groups".into(), job.clone());
            let started = Started("job-w26-r22-groups".into());
            let mut first = busy_step("job-w26-r22-groups")?;
            job.group(i32::try_from(first.id())?);
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            let mut second = busy_step("job-w26-r22-groups")?;
            job.group(i32::try_from(second.id())?);
            crate::rt::sleep(Duration::from_millis(200)).await;
            let state = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| s.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().next().map(str::to_owned)));
            if cfg!(target_os = "linux") {
                assert_eq!(state(first.id()).as_deref(), Some("T"), "the first step is stopped");
                assert_eq!(state(second.id()).as_deref(), Some("T"), "a step started while frozen is stopped as it starts");
            }
            job.freeze(false).await.map_err(|e| anyhow::anyhow!(e))?;
            crate::rt::sleep(Duration::from_millis(100)).await;
            if cfg!(target_os = "linux") {
                assert_ne!(state(first.id()).as_deref(), Some("T"), "thawed, it goes on");
            }
            for c in [&mut first, &mut second] {
                kill_all(&tree(c.id()));
                let _ = c.wait();
            }
            drop(started);
            Ok(())
        })
    }

    /// W26-R22: a job in a container is paused where it runs (`docker pause`), the `docker exec` of
    /// its step frozen with it: nothing in the container uses CPU while it is, and the step under
    /// way finishes once it's let go on.
    #[cfg(target_os = "linux")]
    #[test]
    fn w26_r22_a_container_job_is_paused_in_place_and_finishes_after() -> anyhow::Result<()> {
        use super::super::docker::{Cli, Containers, PathMap, Spec, find};
        let Some(bin) = find() else {
            eprintln!("no docker here: skipped");
            return Ok(());
        };
        let tmp = tempfile::tempdir()?;
        let leg = format!("test-w26-r22-{}", std::process::id());
        let cli = Cli::new(&bin, tmp.path().join("config")).owned_by(tmp.path());
        let spec = Spec { image: "alpine:latest".into(), ..Spec::default() };
        crate::rt::block_on(async move {
            let c = Containers::start(cli.clone(), &leg, PathMap::new(Vec::new()), "/", Some(spec), Vec::new(), None, &|_| {}).await?;
            let checked = async {
                let id = c.job.as_ref().map(|j| j.id.clone()).ok_or_else(|| anyhow::anyhow!("no job container"))?;
                let started = begin(&leg);
                let job = of(&leg).ok_or_else(|| anyhow::anyhow!("not begun"))?;
                job.container(cli.clone(), id.clone());
                // A step counting to a number, through `docker exec` as a job container's steps run.
                let mut cmd = std::process::Command::new(&bin);
                cmd.args(["exec", &id, "sh", "-c", "i=0; while [ $i -lt 5000000 ]; do i=$((i+1)); done; echo counted"]).stdout(std::process::Stdio::piped());
                enter(&leg, &mut cmd);
                let step = cmd.spawn()?;
                crate::rt::sleep(Duration::from_millis(500)).await;
                async fn docker(cli: &Cli, args: &[&str]) -> anyhow::Result<String> {
                    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
                    cli.run(&args, &BTreeMap::new(), Duration::from_secs(30)).await
                }
                // The container's processes, as the host sees them (`docker top`: UID PID ...).
                let top = docker(&cli, &["top", &id]).await?;
                let pids: Vec<u32> = top.lines().skip(1).filter_map(|l| l.split_whitespace().nth(1)?.parse().ok()).collect();
                anyhow::ensure!(!pids.is_empty(), "the container's processes: {top}");
                job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
                anyhow::ensure!(docker(&cli, &["inspect", "-f", "{{.State.Paused}}", &id]).await?.trim() == "true", "the container is paused");
                crate::rt::sleep(Duration::from_millis(200)).await;
                let a: u64 = pids.iter().map(|p| cpu(*p)).sum();
                crate::rt::sleep(Duration::from_millis(700)).await;
                let b: u64 = pids.iter().map(|p| cpu(*p)).sum();
                anyhow::ensure!(a == b, "nothing in a paused container uses CPU ({a} then {b} ticks)");
                job.freeze(false).await.map_err(|e| anyhow::anyhow!(e))?;
                anyhow::ensure!(docker(&cli, &["inspect", "-f", "{{.State.Paused}}", &id]).await?.trim() == "false", "the container goes on");
                let out = crate::rt::blocking(move || step.wait_with_output()).await??;
                anyhow::ensure!(String::from_utf8_lossy(&out.stdout).contains("counted"), "the step finishes after: {out:?}");
                drop(started);
                Ok::<(), anyhow::Error>(())
            }
            .await;
            c.stop().await;
            checked
        })
    }

    /// W26-R22: a job with nothing running here yet, and no cgroup, can't be frozen, and says so.
    #[test]
    fn w26_r22_nothing_to_freeze_says_so() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let job = Job::default();
            assert!(job.freeze(true).await.is_err_and(|e| e.contains("nothing of it runs here yet")));
            assert!(!job.frozen());
            Ok(())
        })
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A step counting in PowerShell to a number and then saying so, having started a second counter
    /// that never ends: two busy processes in its job.
    fn counting_step() -> anyhow::Result<crate::proc::Running> {
        let mut cmd = std::process::Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            "Start-Process -NoNewWindow powershell -ArgumentList '-NoProfile','-Command','$n=0; while($true){$n++}'; $i=0; while($i -lt 5000000){$i++}; 'counted'",
        ]);
        crate::proc::spawn_group(cmd)
    }

    /// Its exit code and everything it printed, once it has ended (within `within`).
    async fn finished(step: &mut crate::proc::Running, within: Duration) -> anyhow::Result<(Option<i32>, String)> {
        let code = crate::rt::timeout(within, step.wait()).await.map_err(|_| anyhow::anyhow!("not done within {within:?}"))?;
        let mut out = Vec::new();
        while let Ok(Ok(chunk)) = crate::rt::timeout(Duration::from_secs(2), step.output.recv()).await {
            out.extend(chunk);
        }
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// W26-R22 on Windows: a running job's step tree (the step, and the counter it left running)
    /// uses no CPU while the job is frozen, every process of it suspended; a step that starts
    /// meanwhile is frozen as it starts; let go on, the step finishes its count; cancelling lets a
    /// frozen job go on. Its waits are long: a runner's headroom may pace what it runs (a part of
    /// each period), which slows it without stopping it.
    #[test]
    fn w26_r22_on_windows_a_jobs_step_tree_is_suspended_in_place_and_finishes_after() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let started = begin("job-w26-r22-windows");
            let job = of("job-w26-r22-windows").ok_or_else(|| anyhow::anyhow!("not begun"))?;
            let mut step = counting_step()?;
            job.step(&step);
            let tree = step.tree();
            // Both counters up, and half a second of CPU used between them.
            let mut up = false;
            for _ in 0..1200 {
                if tree.pids().len() >= 2 && tree.cpu() > 5_000_000 {
                    up = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(100)).await;
            }
            anyhow::ensure!(up, "the step and the counter it left: {:?}, {} (100 ns)", tree.pids(), tree.cpu());
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            anyhow::ensure!(job.frozen());
            let held = job.0.lock().held.clone();
            anyhow::ensure!(held.len() >= 2 && tree.pids().iter().all(|p| held.contains(p)), "every process of its tree suspended: {held:?} of {:?}", tree.pids());
            crate::rt::sleep(Duration::from_millis(300)).await;
            let a = tree.cpu();
            crate::rt::sleep(Duration::from_millis(1000)).await;
            let b = tree.cpu();
            anyhow::ensure!(a == b, "nothing of a frozen job uses CPU: {a} then {b} (100 ns)");
            anyhow::ensure!(crate::rt::timeout(Duration::from_millis(100), step.wait()).await.is_err(), "frozen, it hasn't finished");
            // A step that starts while it's frozen is frozen as it starts.
            let mut late = counting_step()?;
            job.step(&late);
            let late_tree = late.tree();
            crate::rt::sleep(Duration::from_millis(300)).await;
            let c = late_tree.cpu();
            crate::rt::sleep(Duration::from_millis(1000)).await;
            let d = late_tree.cpu();
            anyhow::ensure!(c == d, "a step started while the job is frozen stays still: {c} then {d} (100 ns)");
            // Let go on: it counts again, and finishes its count.
            job.freeze(false).await.map_err(|e| anyhow::anyhow!(e))?;
            anyhow::ensure!(!job.frozen() && job.0.lock().held.is_empty(), "exactly those it suspended let go on");
            let mut going = false;
            for _ in 0..600 {
                if tree.cpu() > b {
                    going = true;
                    break;
                }
                crate::rt::sleep(Duration::from_millis(100)).await;
            }
            anyhow::ensure!(going, "thawed, it goes on");
            let (code, out) = finished(&mut step, Duration::from_secs(900)).await?;
            anyhow::ensure!(code == Some(0) && out.contains("counted"), "the step finishes after: {code:?} {out:?}");
            // Cancelled while frozen: let go on first, so it hears it's to end.
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            thaw("job-w26-r22-windows").await;
            anyhow::ensure!(!job.frozen(), "cancelling lets a frozen job go on");
            step.kill_group();
            late.kill_group();
            let _ = finished(&mut late, Duration::from_secs(30)).await;
            drop(started);
            anyhow::ensure!(of("job-w26-r22-windows").is_none(), "an ended job is no longer here to freeze");
            Ok(())
        })
    }

    /// W26-R22 on Windows: a step on a pty (ConPTY starts it outside any job) is followed by its
    /// process tree: it and the process it started are suspended, and exactly those let go on.
    #[test]
    fn w26_r22_on_windows_a_pty_steps_tree_is_suspended_by_its_parents() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let job = Arc::new(Job::default());
            JOBS.lock().insert("job-w26-r22-windows-pty".into(), job.clone());
            let started = Started("job-w26-r22-windows-pty".into());
            let mut shell = std::process::Command::new("powershell")
                .args(["-NoProfile", "-Command", "Start-Process -NoNewWindow powershell -ArgumentList '-NoProfile','-Command','Start-Sleep 60'; Start-Sleep 60"])
                .spawn()?;
            let root = shell.id();
            let mut kids = Vec::new();
            for _ in 0..1200 {
                kids = crate::os::contain::tree_of(root);
                if kids.len() >= 2 {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(100)).await;
            }
            anyhow::ensure!(kids.len() >= 2, "the shell and what it started: {kids:?}");
            job.group(i32::try_from(root)?);
            job.freeze(true).await.map_err(|e| anyhow::anyhow!(e))?;
            let held = job.0.lock().held.clone();
            anyhow::ensure!(kids.iter().all(|k| held.contains(k)), "its whole tree suspended: {held:?} of {kids:?}");
            job.freeze(false).await.map_err(|e| anyhow::anyhow!(e))?;
            anyhow::ensure!(job.0.lock().held.is_empty(), "exactly those let go on");
            crate::os::proc::taskkill_tree(root);
            let _ = shell.wait();
            drop(started);
            Ok(())
        })
    }
}
