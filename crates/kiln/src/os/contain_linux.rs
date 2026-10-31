//! Each agent session's processes held together by the kernel: a cgroup (v2) of its own,
//! `kiln-agents/<session>` under the cgroup the runner runs in (`kiln-agents-<pid>/<session>` for a
//! runner that isn't the machine's installed one: [`prune_ended`]). Its harness joins it between fork
//! and exec, before it runs anything, so everything it starts is in it too, however that's
//! parented later (a command that daemonized, `setsid`, a double fork): no process leaves its
//! cgroup by forking. Listing an agent's processes is reading its `cgroup.procs`; whose a process
//! is, one read of `/proc/<pid>/cgroup`; ending them all, one write to `cgroup.kill`. Nothing is
//! scanned.
//!
//! Ordinary containment asks only membership (no controllers), so the runner's own cgroup needs no
//! delegation: any cgroup v2 it may write will do (a systemd user unit, a container with its own
//! cgroup namespace). Where there is none (cgroup v1, a read-only cgroup filesystem, a cgroup
//! another user owns), agents have no group and their processes are found by their mark
//! (`crate::harness::survivors`), as before.
//! Enforced executor limits require the managed systemd-control-v1 layout: the supervisor
//! stays in kiln-control, and only its declared empty service parent distributes controllers.

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
/// How deep cgroups an agent's processes made of their own are followed.
const DEPTH: usize = 8;
const LAYOUT_ENV: &str = "KILN_CGROUP_LAYOUT";
const LAYOUT: &str = "systemd-control-v1";
const CONTROL: &str = "kiln-control";

/// A folder of agents' groups: where it is, and its path as `/proc/<pid>/cgroup` names it.
#[derive(Debug)]
struct Base {
    dir: PathBuf,
    rel: String,
    /// None only after this exact managed layout has enabled/read back both controllers.
    limits_error: Option<String>,
}

impl Base {
    /// `name` under this process's own cgroup, made if it isn't there: `None` without a cgroup v2
    /// this process may move processes into.
    fn under_own(name: &str) -> Option<Base> {
        if !Path::new(ROOT).join("cgroup.controllers").is_file() {
            return None;
        }
        let own = unified_path(&fs::read_to_string("/proc/self/cgroup").ok()?).ok()?;
        Self::under(name, &own, &own)
    }

    fn under(name: &str, parent: &str, source: &str) -> Option<Base> {
        let rel = format!("{}/{name}", parent.trim_end_matches('/'));
        let dir = Path::new(ROOT).join(rel.trim_start_matches('/'));
        if let Err(e) = fs::create_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            tracing::info!(dir = %dir.display(), error = %e, "agents' processes have no cgroups of their own here: they're found by their mark");
            return None;
        }
        // Moving a process in needs write access to the folder's cgroup.procs and to the cgroup
        // it leaves (ours: its children start in it).
        let own_procs = Path::new(ROOT).join(source.trim_start_matches('/')).join("cgroup.procs");
        let writable = |p: &Path| nix::unistd::access(p, nix::unistd::AccessFlags::W_OK).is_ok();
        if !writable(&dir.join("cgroup.procs")) || !writable(&own_procs) {
            tracing::info!(dir = %dir.display(), "agents' processes have no cgroups of their own here (not this process's to write): they're found by their mark");
            return None;
        }
        Some(Base { dir, rel, limits_error: Some("enforced Firecracker startup requires the managed systemd-control-v1 service layout; update this service through kiln's managed installer".into()) })
    }

    fn managed(name: &str, layout: &str) -> anyhow::Result<Base> {
        let own = unified_path(&fs::read_to_string("/proc/self/cgroup")?)?;
        let parent = managed_parent(&own, layout, crate::os::supervise::started_by_supervisor())?;
        let supervisor = unified_path(&fs::read_to_string(format!("/proc/{}/cgroup", std::os::unix::process::parent_id()))?)?;
        anyhow::ensure!(supervisor == own, "managed cgroup layout requires this runner and its actual supervisor in the same kiln-control leaf");
        let service = Path::new(ROOT).join(parent.trim_start_matches('/'));
        let source = Path::new(ROOT).join(own.trim_start_matches('/'));
        anyhow::ensure!(fs::read_to_string(source.join("cgroup.type"))?.trim() == "domain", "managed control leaf must be a cgroup v2 domain");
        require_writable(&source.join("cgroup.procs"))?;
        enable_controllers(&service)?;
        let mut base = Self::under(name, parent, &own).ok_or_else(|| anyhow::anyhow!("managed service cannot create/write kiln's containment Base"))?;
        enable_controllers(&base.dir)?;
        base.limits_error = None;
        Ok(base)
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

fn unified_path(text: &str) -> anyhow::Result<String> {
    let mut paths = text.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = paths.next().ok_or_else(|| anyhow::anyhow!("cgroup v2 unified membership is unavailable"))?;
    anyhow::ensure!(paths.next().is_none() && path.starts_with('/')
        && !path.chars().any(char::is_control)
        && (path == "/" || path.split('/').skip(1).all(|part| !part.is_empty() && part != "." && part != "..")), "invalid cgroup v2 membership path");
    Ok(path.to_owned())
}

/// The schema identifies exactly one immediate parent, never an ancestor search or a path
/// supplied by configuration. A namespace-hidden parent cannot satisfy this contract.
fn managed_parent<'a>(own: &'a str, layout: &str, supervised: bool) -> anyhow::Result<&'a str> {
    anyhow::ensure!(layout == LAYOUT && supervised, "managed limits require systemd-control-v1 and the actual kiln supervisor");
    anyhow::ensure!(Path::new(own).file_name().and_then(|name| name.to_str()) == Some(CONTROL), "managed limits require the actual kiln-control leaf (DelegateSubgroup needs systemd >=254)");
    let parent = own.strip_suffix("/kiln-control").filter(|parent| !parent.is_empty()).ok_or_else(|| anyhow::anyhow!("managed service parent is hidden by the cgroup namespace"))?;
    Ok(parent)
}

fn require_writable(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(nix::unistd::access(path, nix::unistd::AccessFlags::W_OK).is_ok(), "managed cgroup file is not delegated for writing: {}", path.display());
    Ok(())
}

fn validate_empty_domain(dir: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(fs::read_to_string(dir.join("cgroup.type"))?.trim() == "domain", "managed cgroup parent must be a v2 domain");
    anyhow::ensure!(fs::read_to_string(dir.join("cgroup.procs"))?.trim().is_empty(), "managed cgroup parent must be empty; kiln never relocates shared processes");
    let controllers = fs::read_to_string(dir.join("cgroup.controllers"))?;
    anyhow::ensure!(["cpu", "memory"].iter().all(|wanted| controllers.split_whitespace().any(|name| name == *wanted)), "managed cgroup parent lacks delegated cpu/memory controllers");
    require_writable(&dir.join("cgroup.procs"))?;
    require_writable(&dir.join("cgroup.subtree_control"))
}

fn enable_controllers(dir: &Path) -> anyhow::Result<()> {
    validate_empty_domain(dir)?;
    fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory")?;
    let enabled = fs::read_to_string(dir.join("cgroup.subtree_control"))?;
    anyhow::ensure!(["cpu", "memory"].iter().all(|wanted| enabled.split_whitespace().any(|name| name == *wanted)), "managed cgroup controller distribution readback failed");
    Ok(())
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
    // main calls this before server/agent startup and headroom access. Resolve the same
    // immutable Base now; a reconnect never resets its layout or old physical identities.
    let _ = base();
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
        let ordinary = || {
            if cfg!(test) || crate::os::supervise::started_by_supervisor() { return Base::under_own(AGENTS); }
            let b = Base::under_own(&format!("{AGENTS}-{}", std::process::id()))?;
            if let Some(own) = b.dir.parent() { prune_ended(own); }
            Some(b)
        };
        let Some(layout) = std::env::var_os(LAYOUT_ENV) else { return ordinary() };
        match Base::managed(AGENTS, &layout.to_string_lossy()) {
            Ok(base) => Some(base),
            Err(error) => {
                tracing::warn!(error = %error, "managed cgroup delegation unavailable; ordinary membership remains available, enforced Firecracker startup is refused");
                let mut base = ordinary()?;
                base.limits_error = Some(format!("managed cgroup delegation unavailable: {error}; reinstall with systemd >=254, Delegate=cpu memory and DelegateSubgroup=kiln-control through kiln's managed installer"));
                Some(base)
            }
        }
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
    // Canonical CI domains retire only through their Group/Gate proof owner. A dead
    // runner PID and an empty subtree do not authorize deleting its evidence.
    if dir.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("ci-")) { return; }
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
    /// U01/C1a: an empty aggregate domain owned by one existing Execution. Only the
    /// already-owned Base is changed; a missing delegation is never repaired in ancestors.
    pub(crate) fn aggregate(session: &str) -> std::io::Result<Group> {
        let base = base().ok_or_else(|| std::io::Error::other("Firecracker CPU/memory limits require writable delegated cgroup v2"))?;
        if let Some(error) = &base.limits_error { return Err(std::io::Error::other(error.clone())); }
        let name = name_of(session).ok_or_else(|| std::io::Error::other("invalid aggregate execution identity"))?;
        let controllers = fs::read_to_string(base.dir.join("cgroup.controllers"))?;
        if !["cpu", "memory"].iter().all(|wanted| controllers.split_whitespace().any(|c| c == *wanted))
            || fs::read_to_string(base.dir.join("cgroup.type"))?.trim() != "domain"
            || !fs::read_to_string(base.dir.join("cgroup.procs"))?.trim().is_empty()
        {
            return Err(std::io::Error::other("Firecracker CPU/memory limits require cpu and memory controllers delegated to kiln's empty cgroup parent; configure the service through the managed installer"));
        }
        fs::write(base.dir.join("cgroup.subtree_control"), "+cpu +memory")?;
        let dir = base.dir.join(name);
        fs::create_dir(&dir)?; // A fresh owner never adopts an arbitrary existing domain.
        Ok(Self { dir })
    }

    pub(crate) fn set_limits(&self, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<()> {
        limits.validate()?;
        anyhow::ensure!(self.empty_proven_strict()?, "aggregate domain was populated before admission");
        fs::write(self.dir.join("cpu.max"), format!("{} {}", limits.cpu_quota_us, limits.cpu_period_us))?;
        fs::write(self.dir.join("memory.max"), limits.memory_bytes.to_string())?;
        fs::write(self.dir.join("cgroup.subtree_control"), "+cpu +memory")?;
        self.verify_limits(limits)
    }

    pub(crate) fn verify_limits(&self, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<()> {
        limits.validate()?;
        let cpu = fs::read_to_string(self.dir.join("cpu.max"))?;
        let mut values = cpu.split_whitespace();
        anyhow::ensure!(values.next().and_then(|v| v.parse::<u64>().ok()) == Some(limits.cpu_quota_us)
            && values.next().and_then(|v| v.parse::<u64>().ok()) == Some(limits.cpu_period_us) && values.next().is_none(), "aggregate CPU quota readback differs from the original grant");
        anyhow::ensure!(fs::read_to_string(self.dir.join("memory.max"))?.trim().parse::<u64>().ok() == Some(limits.memory_bytes), "aggregate memory limit readback differs from the original grant");
        Ok(())
    }

    pub(crate) fn child(&self, id: &str, limits: crate::os::cgroup::CpuMemoryLimit) -> anyhow::Result<Group> {
        self.verify_limits(limits)?;
        let name = name_of(id).ok_or_else(|| std::io::Error::other("invalid aggregate child identity"))?;
        let dir = self.dir.join(name);
        fs::create_dir(&dir)?;
        Ok(Self { dir })
    }

    /// Bound aggregate disappearance is unknown until its positive proof was persisted.
    pub(crate) fn empty_proven_strict(&self) -> std::io::Result<bool> {
        let events = fs::read_to_string(self.dir.join("cgroup.events"))?;
        match events.lines().find_map(|l| l.strip_prefix("populated ")) {
            Some("0") => Ok(true),
            Some("1") => Ok(false),
            _ => Err(std::io::Error::other("cgroup.events lacks a valid populated field")),
        }
    }

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

    /// U01: leased commands must enter containment before exec or fail to start.
    /// Unlike the harness compatibility path, this never falls back outside the cgroup.
    pub(crate) fn enter_leased(&self, cmd: &mut Command) -> std::io::Result<()> {
        let mut procs = fs::OpenOptions::new().write(true).open(self.dir.join("cgroup.procs"))?;
        #[allow(unsafe_code)]
        // SAFETY: only write(2) on an already open descriptor between fork and exec.
        unsafe {
            cmd.pre_exec(move || procs.write_all(b"0"));
        }
        Ok(())
    }

    /// U01: retain the exact containment identity across a runner moving cgroups on restart.
    pub(crate) fn lease_identity(&self) -> String { self.dir.to_string_lossy().into_owned() }

    pub(crate) fn recover_leased(identity: &str, id: &str) -> std::io::Result<Group> {
        let path = PathBuf::from(identity);
        let name = name_of(id).ok_or_else(|| std::io::Error::other("invalid lease group name"))?;
        if !path.starts_with(ROOT) || path.file_name().and_then(|n| n.to_str()) != Some(name)
            || path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
        {
            return Err(std::io::Error::other("invalid lease containment identity"));
        }
        Ok(Group { dir: path })
    }

    /// U01: kernel proof for all descendants, including child cgroups. Read failures are errors,
    /// never interpreted as an empty group.
    pub(crate) fn empty_proven(&self) -> std::io::Result<bool> {
        // A cgroup can only be removed once no processes inhabit it or its descendants.
        // Inspection failures remain errors; require the kernel mount before accepting absence.
        if !self.dir.try_exists()? && Path::new(ROOT).join("cgroup.controllers").try_exists()? {
            return Ok(true);
        }
        let events = fs::read_to_string(self.dir.join("cgroup.events"))?;
        match events.lines().find_map(|l| l.strip_prefix("populated ")) {
            Some("0") => Ok(true),
            Some("1") => Ok(false),
            _ => Err(std::io::Error::other("cgroup.events lacks a valid populated field")),
        }
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
    /// A runner that isn't the installed one never takes another's folder, and those of runners
    /// that ended go once empty (the 2026-10-29 outage: a lab runner paced production's agents).
    #[test]
    fn folders_of_ended_runners_go_once_empty_and_a_live_ones_stays() -> anyhow::Result<()> {
        use super::*;
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

    /// U01: only the kernel's explicit populated=0 is proof; unreadable or malformed
    /// membership information cannot accidentally turn a live reservation into free capacity.
    #[test]
    fn u01_containment_proof_refuses_missing_and_malformed_kernel_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let group = super::Group { dir: dir.path().to_path_buf() };
        let events = dir.path().join("cgroup.events");
        assert!(group.empty_proven().is_err());
        std::fs::write(&events, "populated 1\nfrozen 0\n")?;
        assert!(!group.empty_proven()?);
        std::fs::write(&events, "populated 0\nfrozen 0\n")?;
        assert!(group.empty_proven()?);
        std::fs::write(&events, "populated unknown\n")?;
        assert!(group.empty_proven().is_err());
        std::fs::remove_file(events)?;
        assert!(group.empty_proven().is_err());
        assert!(group.empty_proven_strict().is_err(), "aggregate absence is unknown, not physical proof");
        let limits = crate::os::cgroup::CpuMemoryLimit { cpu_quota_us: 150000, cpu_period_us: 100000, memory_bytes: 128 << 20 };
        std::fs::write(dir.path().join("cpu.max"), "150000 100000\n")?;
        std::fs::write(dir.path().join("memory.max"), "134217728\n")?;
        group.verify_limits(limits)?;
        std::fs::write(dir.path().join("cpu.max"), "max 100000\n")?;
        assert!(group.verify_limits(limits).is_err(), "unlimited CPU cannot attest the admitted envelope");
        std::fs::write(dir.path().join("cpu.max"), "150000 100000\n")?;
        std::fs::write(dir.path().join("memory.max"), "max\n")?;
        assert!(group.verify_limits(limits).is_err(), "unlimited memory cannot attest the admitted envelope");
        let old_base = dir.path().join("old-runner");
        std::fs::create_dir_all(old_base.join("ci-exact-owner").join("ci-child"))?;
        std::fs::create_dir_all(old_base.join("ordinary-session"))?;
        prune(&old_base, 8);
        assert!(old_base.join("ci-exact-owner").join("ci-child").is_dir(), "generic PID pruning cannot consume canonical Gate evidence");
        assert!(!old_base.join("ordinary-session").exists());
        let own = unified_path("0::/system.slice/kiln-agent.service/kiln-control\n")?;
        assert_eq!(managed_parent(&own, LAYOUT, true)?, "/system.slice/kiln-agent.service");
        let escaped = unified_path(r"0::/system.slice/kiln\x2dagent.service/kiln-control")?;
        assert_eq!(escaped, r"/system.slice/kiln\x2dagent.service/kiln-control");
        assert_eq!(managed_parent(&escaped, LAYOUT, true)?, r"/system.slice/kiln\x2dagent.service");
        assert!(managed_parent(&own, LAYOUT, false).is_err());
        assert!(managed_parent(&own, "unknown-v2", true).is_err());
        for path in ["/", "/kiln-control", "/unit/.control", "/unit/not-control"] {
            assert!(managed_parent(path, LAYOUT, true).is_err());
        }
        for raw in ["0::/unit/../kiln-control\n", "0::/unit//kiln-control\n", "0::/unit/./kiln-control\n", "0::relative\n", "0::/unit/kiln-control\n0::/other\n"] {
            assert!(unified_path(raw).is_err());
        }
        let declared = dir.path().join("declared-service");
        fs::create_dir(&declared)?;
        fs::write(declared.join("cgroup.type"), "domain\n")?;
        fs::write(declared.join("cgroup.procs"), "123\n")?;
        fs::write(declared.join("cgroup.controllers"), "cpu memory\n")?;
        fs::write(declared.join("cgroup.subtree_control"), "")?;
        assert!(validate_empty_domain(&declared).is_err(), "a populated service root is never repaired by moving processes");
        fs::write(declared.join("cgroup.procs"), "")?;
        fs::write(declared.join("cgroup.controllers"), "cpu\n")?;
        assert!(validate_empty_domain(&declared).is_err(), "requested delegation is insufficient without actual memory availability");
        fs::write(declared.join("cgroup.controllers"), "cpu memory\n")?;
        fs::write(declared.join("cgroup.type"), "domain threaded\n")?;
        assert!(validate_empty_domain(&declared).is_err());
        Ok(())
    }

    use super::*;

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
