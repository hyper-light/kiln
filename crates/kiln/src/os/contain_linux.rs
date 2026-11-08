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
//! job's group has the least CPU weight (`cpu.weight` 1), below kiln's own processes and a person's
//! runs; the server comes first by its slice's weight. Not `cpu.idle`, which made the server slower
//! in the lab ([`lower`]). cgroup v2 gives a cgroup's children a controller only
//! while it holds no process itself, so kiln's own processes run in the unit's leaf [`LEAF`]
//! (systemd's `DelegateSubgroup=`, or moved there by the runner) and the folders of groups sit
//! beside it, under the unit.
//!
//! Memory (docs/priority.md, "Memory"): a delegated unit that declares a floor for kiln's own
//! processes (systemd's `MemoryMin=`, `MemoryLow=`: production's server's) has it kept for them
//! alone, in its leaf, and the agents' folder gets what the unit may have less that floor
//! ([`budget`]), so what agents run (builds of 8-10 GB each) is slowed, reclaimed and killed inside
//! their folder and never takes the floor, slows the unit or fills it. Before this, they ran in the
//! server's own cgroup: its `memory.high` was hit 15.9 million times.

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
/// have a weight of its own.
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

    /// Group `session` (made when `make`): at the least weight when `low` and the CPU is shared here
    /// (docs/priority.md).
    fn group(&self, session: &str, make: bool, low: bool) -> Option<Group> {
        let dir = self.dir.join(name_of(session)?);
        if make {
            if let Err(e) = fs::create_dir(&dir)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                tracing::warn!(session, error = %e, "an agent's processes get no cgroup of their own: they're found by their mark");
                return None;
            }
            if low && self.cpu {
                let _ = lower(&dir).inspect_err(|why| tracing::warn!(session, why = %why, "an agent's group runs at the default weight"));
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
    /// The agents' share of the home's memory, kiln's own floor kept in its leaf; or why they share
    /// all of it with kiln's own processes.
    memory: Result<Budget, String>,
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
        let why = "only the machine's installed runner arranges the cgroups it runs in".to_owned();
        return Arranged { home: own, cpu: Err(why.clone()), memory: Err(why) };
    }
    let (home, delegated) = match unit_of_leaf(&own) {
        // In its unit's leaf: put there by systemd (`DelegateSubgroup=`) or by a runner before this
        // one, and only a delegated unit has one.
        Some(unit) => (unit.to_owned(), Ok(())),
        None => (own.clone(), delegated(&own)),
    };
    let unit = Path::new(ROOT).join(home.trim_start_matches('/'));
    let cpu = delegated.and_then(|()| arrange(&unit));
    match &cpu {
        Ok(()) => tracing::info!(unit = %home, "legs and agents run at the least CPU weight here, each in a cgroup of its own"),
        Err(why) => tracing::info!(why = %why, "legs and agents run at normal priority here"),
    }
    // Only once kiln's own processes are all in the unit's leaf ([`arrange`]): a cgroup shares a
    // controller among its children only while it holds no process itself.
    let memory = match &cpu {
        Ok(()) => keep_floor(&unit, &unit.join(AGENTS)),
        Err(why) => Err(format!("kiln's own processes aren't in a leaf of their own here ({why})")),
    };
    match &memory {
        Ok(budget) => tracing::info!(unit = %home, agents = %budget.said(), "kiln's own processes keep their unit's memory floor in its leaf; the agents beside them share the rest"),
        Err(why) => tracing::info!(why = %why, "agents share their unit's memory with kiln's own processes here"),
    }
    Arranged { home, cpu, memory }
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

/// Puts the group at `dir` below what runs beside it (docs/priority.md): the least weight a cgroup
/// can have, `cpu.weight` 1. Not `cpu.idle`: an idle group gives way only to its siblings, while the
/// scheduler takes a CPU running nothing but idle work as free for any thread that wakes, the
/// server's too, which then waited there behind the leg (the lab, docs/priority.md).
fn lower(dir: &Path) -> Result<(), String> {
    write_file(&dir.join("cpu.weight"), "1").map_err(|e| format!("its cpu.weight: {e}"))
}

/// What a cgroup declares of its memory, as systemd writes a unit's `MemoryMin=`, `MemoryLow=`,
/// `MemoryHigh=` and `MemoryMax=`: its protection (`memory.min`, `memory.low`; 0 where none) and its
/// limits (`memory.high`, past which it's slowed; `memory.max`, the most it holds; none where `max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Declared {
    min: u64,
    low: u64,
    high: Option<u64>,
    max: Option<u64>,
}

impl Declared {
    /// What the cgroup at `dir` declares, as far as it can be read (a file it lacks declares nothing).
    fn of(dir: &Path) -> Declared {
        let read = |f: &str| fs::read_to_string(dir.join(f)).ok();
        let protected = |f: &str| read(f).and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        let limit = |f: &str| read(f).and_then(|t| crate::os::cgroup::limit(&t));
        Declared { min: protected("memory.min"), low: protected("memory.low"), high: limit("memory.high"), max: limit("memory.max") }
    }

    /// The floor it keeps for what runs in it, and the setting that says so: the most it protects,
    /// best effort (`MemoryLow=`) or come what may (`MemoryMin=`).
    fn floor(&self) -> (u64, &'static str) {
        if self.low >= self.min { (self.low, "MemoryLow=") } else { (self.min, "MemoryMin=") }
    }
}

/// The agents' share of their unit's memory (docs/priority.md, "Memory"): their folder's
/// `memory.max` and `memory.high`, and what they're worked out from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Budget {
    /// The most the agents' processes hold together: past it the kernel takes their memory back,
    /// and kills the largest of them where it can't.
    max: u64,
    /// Past this they're slowed and their memory taken back first, where their unit (or a slice
    /// above it) has a point it's slowed at itself.
    high: Option<u64>,
    /// What it's worked out from, for a person.
    basis: String,
}

impl Budget {
    /// It, for a person.
    fn said(&self) -> String {
        let max = crate::doctor::said_bytes(self.max);
        match self.high {
            Some(high) => format!("at most {max}, slowed past {}: {}", crate::doctor::said_bytes(high), self.basis),
            None => format!("at most {max}: {}", self.basis),
        }
    }
}

/// The agents' share of the memory of a unit that declares `unit`, in slices whose least
/// `memory.high` and `memory.max` are `above`, on a machine with `machine` bytes of memory. The most
/// they hold is what the unit may have (the least of its own `MemoryMax=`, its slices' and the
/// machine's memory) less the floor it keeps for kiln's own processes (`MemoryLow=`, or `MemoryMin=`
/// where that's more), so together they never take that floor or fill the unit (an OOM kill there
/// could take the server); and they're slowed that much short of where the unit is slowed
/// (`MemoryHigh=`, or a slice's), so they never slow the unit (the server's threads with it) while
/// kiln's own processes hold no more than their floor. Every number is the unit's, a slice's or the
/// machine's. Why not, where the unit leaves them nothing past the floor.
fn budget(unit: Declared, above: (Option<u64>, Option<u64>), machine: u64) -> Result<Budget, String> {
    let said = crate::doctor::said_bytes;
    let (floor, kept_by) = unit.floor();
    let floor_is = format!("the floor kept for kiln's own processes ({kept_by} {})", said(floor));
    // The least of them, and what said it (the first of those equal).
    let least = |of: &[(Option<u64>, &'static str)]| of.iter().filter_map(|(v, by)| v.map(|v| (v, *by))).min_by_key(|(v, _)| *v);
    let (grant, granted_by) = least(&[(unit.max, "its unit's MemoryMax="), (above.1, "a slice's MemoryMax="), (Some(machine), "the machine's memory,")]).unwrap_or((machine, "the machine's memory,"));
    let grant_is = format!("{granted_by} {}", said(grant));
    let max = grant.checked_sub(floor).filter(|m| *m > 0).ok_or_else(|| format!("{grant_is} leaves its agents nothing past {floor_is}"))?;
    let high = match least(&[(unit.high, "its unit's MemoryHigh="), (above.0, "a slice's MemoryHigh=")]) {
        Some((at, by)) => {
            let at_is = format!("{by} {}", said(at));
            let high = at.checked_sub(floor).filter(|h| *h > 0).ok_or_else(|| format!("{at_is} slows its agents from their first byte: it's no more than {floor_is}"))?;
            // A point past the most they hold slows nothing.
            (high < max).then_some((high, at_is))
        }
        None => None,
    };
    let basis = match &high {
        Some((_, at_is)) => format!("{grant_is} and {at_is}, less {floor_is}"),
        None => format!("{grant_is}, less {floor_is}"),
    };
    Ok(Budget { max, high: high.map(|(h, _)| h), basis })
}

/// The least `memory.high` and `memory.max` of the slices above the cgroup at `unit`, up to the
/// hierarchy's root (which has neither).
fn above(unit: &Path) -> (Option<u64>, Option<u64>) {
    let root = Path::new(ROOT);
    let least = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    unit.ancestors().skip(1).take_while(|d| d.starts_with(root) && *d != root).map(Declared::of).fold((None, None), |(high, max), d| (least(high, d.high), least(max, d.max)))
}

/// The machine's memory, in bytes (`MemTotal`).
fn machine_memory() -> Result<u64, String> {
    let text = fs::read_to_string("/proc/meminfo").map_err(|e| format!("the machine's memory can't be read: {e}"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| "/proc/meminfo doesn't say the machine's memory".to_owned())
}

/// Keeps the floor the delegated unit at `unit` declares for kiln's own processes (`MemoryMin=`,
/// `MemoryLow=`) for them alone, in its leaf, and gives the agents' folder at `agents` the rest of
/// what the unit may have ([`budget`]). The unit must hold no process itself ([`arrange`]): only
/// then can it share its memory among its children. The agents' share, or why they have none of
/// their own: a unit that declares no floor has nothing to keep and is left as it was. Worked out as
/// kiln starts: a unit's memory changed while it runs (`systemctl set-property`) is taken up at its
/// next start.
fn keep_floor(unit: &Path, agents: &Path) -> Result<Budget, String> {
    let declared = Declared::of(unit);
    if declared.floor().0 == 0 {
        return Err("its unit declares no floor for kiln's own processes (systemd's MemoryMin=, MemoryLow=)".to_owned());
    }
    let controllers = fs::read_to_string(unit.join("cgroup.controllers")).map_err(|e| format!("its unit's cgroup can't be read: {e}"))?;
    if !controllers.split_whitespace().any(|c| c == "memory") {
        return Err("its service manager gives its unit no memory controller to share".to_owned());
    }
    write_file(&unit.join("cgroup.subtree_control"), "+memory").map_err(|e| format!("its unit's memory can't be shared among its children (its cgroup.subtree_control: {e})"))?;
    // The leaf takes the unit's protection whole, so the agents beside it share none of it but what
    // kiln's own processes leave unused.
    let leaf = unit.join(LEAF);
    for (file, bytes) in [("memory.min", declared.min), ("memory.low", declared.low)] {
        write_file(&leaf.join(file), &bytes.to_string()).map_err(|e| format!("its leaf's {file}: {e}"))?;
    }
    let budget = budget(declared, above(unit), machine_memory()?).map_err(|why| format!("{why} (kiln's own floor is kept in its leaf all the same)"))?;
    if let Err(e) = fs::create_dir(agents)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(format!("the agents' folder couldn't be made: {e}"));
    }
    let high = budget.high.map_or_else(|| "max".to_owned(), |h| h.to_string());
    for (file, value) in [("memory.max", budget.max.to_string()), ("memory.high", high)] {
        write_file(&agents.join(file), &value).map_err(|e| format!("the agents' folder's {file}: {e}"))?;
    }
    Ok(budget)
}

/// How legs and agents run here (docs/priority.md), for the runner's facts and its log: their
/// weight, and where they run beside a server with cores of its own ([`keep_work_off`]).
pub fn priority() -> String {
    let shared = base().or_else(jobs_base).filter(|b| b.cpu);
    let weight = match (&arranged().cpu, shared) {
        (Ok(()), Some(_)) => "the least CPU weight: each leg's and agent's cgroup has cpu.weight 1".to_owned(),
        (Ok(()), None) => "normal: the CPU isn't shared among kiln's groups here".to_owned(),
        (Err(why), _) => format!("normal: {why}"),
    };
    match work_said() {
        Some(work) => format!("{weight}; {work}"),
        None => weight,
    }
}

/// Where this process starts kiln's own legs and agents beside a server with cores of its own, as
/// [`keep_work_off`] worked it out (docs/priority.md, "Legs and agents keep off the server's cores";
/// goal 8, plan 01a1a59c2b29 step 7): the server's cores as declared, what that means, and the work
/// CPUs as a set, ready for a child to take between fork and exec.
struct Work {
    server: String,
    cpus: super::WorkCpus,
    set: Option<rustix::thread::CpuSet>,
}

static WORK: OnceLock<Work> = OnceLock::new();

/// Works out where this process starts kiln's own legs and agents beside a server whose cores are
/// `server` (`text` as declared) and whose shards are fixed to `shards` of them: the CPUs the calling
/// thread may run on less the server's, or, where those are all it may run on, less the shards'
/// (`super::work_cpus_for`). Affinity rather than a cpuset on kiln's folders of groups: a user's
/// service manager doesn't delegate the cpuset controller (systemd's `user@.service` delegates
/// `pids memory cpu`), and where a system install could, its partition already keeps everything
/// outside the server's slice off its cores (docs/priority.md). Only the first call counts.
pub fn keep_work_off(server: &[u32], shards: &[u32], text: &str) -> Result<super::WorkCpus, String> {
    let allowed = rustix::thread::sched_getaffinity(None).map_err(|e| format!("this process's CPUs can't be read: {e}"))?;
    let mine: Vec<u32> = (0..rustix::thread::CpuSet::MAX_CPU).filter(|c| allowed.is_set(*c)).filter_map(|c| u32::try_from(c).ok()).collect();
    let cpus = super::work_cpus_for(&mine, server, shards);
    let set = match &cpus {
        super::WorkCpus::Apart(onto) | super::WorkCpus::Beside(onto) => Some(cpu_set(onto)),
        super::WorkCpus::Clear | super::WorkCpus::Shared => None,
    };
    let _ = WORK.set(Work { server: text.to_owned(), cpus: cpus.clone(), set });
    Ok(cpus)
}

/// `cpus` as a CPU set, those past what a set holds left out.
fn cpu_set(cpus: &[u32]) -> rustix::thread::CpuSet {
    let mut set = rustix::thread::CpuSet::new();
    for c in cpus.iter().filter_map(|c| usize::try_from(*c).ok()).filter(|c| *c < rustix::thread::CpuSet::MAX_CPU) {
        set.set(c);
    }
    set
}

/// What of the CPUs `now` the work CPUs `work` hold, or `work` itself where they hold none of them (a
/// thread fixed to a shard's core starts the process): where a leg's or an agent's process goes, so
/// a leg's share of the runner's CPUs (`crate::runner::cpus`, all of them work CPUs) stays its own.
fn narrowed(now: &rustix::thread::CpuSet, work: &rustix::thread::CpuSet) -> rustix::thread::CpuSet {
    let mut out = *now;
    for c in 0..rustix::thread::CpuSet::MAX_CPU {
        if out.is_set(c) && !work.is_set(c) {
            out.unset(c);
        }
    }
    if out.count() == 0 { *work } else { out }
}

/// Makes `cmd`'s process start on the work CPUs ([`narrowed`] from those it would start on) before it
/// runs anything, between fork and exec, so everything it starts inherits them. Nothing where nothing
/// is kept off. A process that fails to move starts where it would have.
pub fn onto_work_cpus(cmd: &mut Command) {
    if let Some(work) = WORK.get().and_then(|w| w.set) {
        start_on(cmd, work);
    }
}

/// Makes `cmd`'s process start on what [`narrowed`] gives of `work`, between fork and exec.
fn start_on(cmd: &mut Command, work: rustix::thread::CpuSet) {
    #[allow(unsafe_code)]
    // SAFETY: the closure runs in the child between fork and exec, and makes two system calls on the
    // child's one thread (sched_getaffinity, sched_setaffinity) with CPU sets on its stack: nothing
    // allocated, nothing locked.
    unsafe {
        cmd.pre_exec(move || {
            let now = rustix::thread::sched_getaffinity(None).unwrap_or(work);
            let _ = rustix::thread::sched_setaffinity(None, &narrowed(&now, &work));
            Ok(())
        });
    }
}

/// Moves every thread of process `pid` onto the work CPUs, as [`onto_work_cpus`] starts one there:
/// whether it moved any. Those gone meanwhile are passed over.
pub fn onto_work_cpus_pid(pid: u32) -> bool {
    let Some(work) = WORK.get().and_then(|w| w.set) else { return false };
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else { return false };
    let mut moved = false;
    for tid in tasks.flatten().filter_map(|t| t.file_name().to_str()?.parse::<i32>().ok()).filter_map(rustix::process::Pid::from_raw) {
        if let Ok(now) = rustix::thread::sched_getaffinity(Some(tid)) {
            let onto = narrowed(&now, &work);
            moved |= onto != now && rustix::thread::sched_setaffinity(Some(tid), &onto).is_ok();
        }
    }
    moved
}

/// The work CPUs, where kiln's own legs and agents keep off the server's cores (or its shards').
pub fn work_cpus() -> Option<Vec<usize>> {
    match &WORK.get()?.cpus {
        super::WorkCpus::Apart(cpus) | super::WorkCpus::Beside(cpus) => Some(cpus.iter().filter_map(|c| usize::try_from(*c).ok()).collect()),
        super::WorkCpus::Clear | super::WorkCpus::Shared => None,
    }
}

/// What [`keep_work_off`] decided, for a person.
pub fn work_said() -> Option<String> {
    WORK.get().map(|w| w.cpus.said(&w.server))
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
                // Agents' groups a runner made before the CPU was shared here, at the least weight
                // now (a run's may be a person's: it ends soon).
                for e in fs::read_dir(&b.dir).into_iter().flatten().flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with(super::RUN_GROUP) {
                        let _ = lower(&e.path());
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
    /// agent's is, at the weight of what the person runs rather than an agent's least one
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
/// Where the CPU is shared among the jobs' groups, it has the least CPU weight (`cpu.weight` 1);
/// nothing else it holds is counted or limited by it.
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
        // A leg has the least weight where the CPU is shared among the jobs' groups (docs/priority.md).
        if base.cpu {
            let _ = lower(&dir).inspect_err(|why| tracing::warn!(job, why = %why, "a job's group runs at the default weight"));
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

/// How this machine holds an agent's processes together, and the memory they share.
pub fn how() -> String {
    match base() {
        Some(b) => match &arranged().memory {
            Ok(budget) => format!("a cgroup per agent under {}, their memory together {}", b.rel, budget.said()),
            Err(why) => format!("a cgroup per agent under {}, their memory their unit's: {why}", b.rel),
        },
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
    /// in a folder there has the least weight (`cpu.weight` 1) with what it starts in it, never
    /// `cpu.idle`, and a person's run's the default.
    #[test]
    fn g8_s2_a_delegated_units_groups_run_at_the_least_weight() -> anyhow::Result<()> {
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
            let weight = fs::read_to_string(agent.dir.join("cpu.weight"))?;
            let idle = fs::read_to_string(agent.dir.join("cpu.idle")).unwrap_or_default();
            let persons = fs::read_to_string(person.dir.join("cpu.weight"))?;
            let _ = child.kill();
            let _ = child.wait();
            agent.remove();
            person.remove();
            let _ = fs::remove_dir(&folder.dir);
            anyhow::ensure!(inside, "what an agent starts doesn't run in its group");
            anyhow::ensure!(weight.trim() == "1", "an agent's group isn't at the least weight: {weight}");
            anyhow::ensure!(idle.trim() != "1", "an agent's group is idle");
            anyhow::ensure!(persons.trim() != "1", "a person's run's group is at the least weight: {persons}");
            Ok(())
        })();
        let _ = held.kill();
        let _ = held.wait();
        let _ = fs::remove_dir(dir.join(LEAF));
        checked
    }

    /// The agents' share of production's server's unit as it was written (2026-11-07: MemoryMin=8G,
    /// MemoryLow=12G, MemoryHigh=14G, MemoryMax=16G, on a machine of 123 GiB): what the unit may have
    /// less the floor kept for kiln's own processes, slowed as far short of where the unit is slowed;
    /// without the unit's limits, the machine's memory less the floor. A slice's tighter limits hold;
    /// a unit that leaves nothing past the floor gives the agents nothing, and says why.
    #[test]
    fn agents_share_what_their_unit_may_have_less_kilns_own_floor() {
        const GIB: u64 = 1 << 30;
        let machine = 128_893_460 * 1024;
        let production = Declared { min: 8 * GIB, low: 12 * GIB, high: Some(14 * GIB), max: Some(16 * GIB) };
        let got = budget(production, (None, None), machine);
        assert_eq!(got.as_ref().map(|b| (b.max, b.high)), Ok((4 * GIB, Some(2 * GIB))));
        let said = got.map(|b| b.said()).unwrap_or_default();
        assert_eq!(said, "at most 4.0 GiB, slowed past 2.0 GiB: its unit's MemoryMax= 16.0 GiB and its unit's MemoryHigh= 14.0 GiB, less the floor kept for kiln's own processes (MemoryLow= 12.0 GiB)");
        // Its floor alone: the machine's memory less it, slowed nowhere short of the most.
        let floor_alone = Declared { high: None, max: None, ..production };
        let got = budget(floor_alone, (None, None), machine);
        assert_eq!(got.as_ref().map(|b| (b.max, b.high)), Ok((machine - 12 * GIB, None)));
        assert_eq!(got.map(|b| b.said()).unwrap_or_default(), "at most 111 GiB: the machine's memory, 123 GiB, less the floor kept for kiln's own processes (MemoryLow= 12.0 GiB)");
        // A slice above with less than the unit or the machine.
        assert_eq!(budget(production, (Some(13 * GIB), Some(15 * GIB)), machine).map(|b| (b.max, b.high)), Ok((3 * GIB, Some(GIB))));
        assert_eq!(budget(floor_alone, (Some(40 * GIB), Some(48 * GIB)), machine).map(|b| (b.max, b.high)), Ok((36 * GIB, Some(28 * GIB))));
        // MemoryMin= alone is the floor; where it's more than MemoryLow=, it is.
        assert_eq!(budget(Declared { low: 0, ..production }, (None, None), machine).map(|b| (b.max, b.high)), Ok((8 * GIB, Some(6 * GIB))));
        assert_eq!(budget(Declared { min: 13 * GIB, ..production }, (None, None), machine).map(|b| (b.max, b.high)), Ok((3 * GIB, Some(GIB))));
        // A point it's slowed at past the most the agents hold slows nothing.
        assert_eq!(budget(Declared { high: Some(20 * GIB), ..production }, (None, None), machine).map(|b| (b.max, b.high)), Ok((4 * GIB, None)));
        // Nothing past the floor, or slowed from the first byte: none, and why.
        let full = budget(Declared { low: 16 * GIB, ..production }, (None, None), machine);
        assert!(full.as_ref().is_err_and(|why| why == "its unit's MemoryMax= 16.0 GiB leaves its agents nothing past the floor kept for kiln's own processes (MemoryLow= 16.0 GiB)"), "{full:?}");
        let slowed = budget(Declared { high: Some(12 * GIB), ..production }, (None, None), machine);
        assert!(slowed.as_ref().is_err_and(|why| why.contains("its unit's MemoryHigh= 12.0 GiB slows its agents from their first byte")), "{slowed:?}");
    }

    /// A scope of the user's own delegated to this process (`systemd-run --user --scope -p
    /// Delegate=yes`, and `props`), its process (a `sleep`) standing in for kiln's, and its cgroup as
    /// `/proc/<pid>/cgroup` names it; none where there's none to be had (no systemd, no user manager).
    fn delegated_scope(name: &str, props: &[&str]) -> anyhow::Result<Option<(std::process::Child, String)>> {
        use std::process::Stdio;
        let unit = format!("{name}-{}.scope", std::process::id());
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "--collect", "-p", "Delegate=yes"]);
        for p in props {
            cmd.args(["-p", p]);
        }
        cmd.arg(format!("--unit={unit}")).args(["--", "sleep", "300"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let Ok(mut held) = cmd.spawn() else { return Ok(None) };
        // Its cgroup, once systemd has put it there.
        for _ in 0..100 {
            let rel = fs::read_to_string(format!("/proc/{}/cgroup", held.id())).ok().and_then(|t| t.lines().find_map(|l| l.strip_prefix("0::")).map(|p| p.trim().to_owned())).filter(|r| r.ends_with(&unit));
            if let Some(rel) = rel {
                return Ok(Some((held, rel)));
            }
            if held.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = held.kill();
        let _ = held.wait();
        Ok(None)
    }

    /// The memory of a delegated unit that declares a floor for kiln's own processes (a scope of the
    /// user's own, its process standing in for kiln's): the floor kept in its leaf alone, the agents'
    /// folder given the rest, and an agent that runs past that (a build's worth, in small) killed
    /// inside its folder, the unit itself never slowed, filled or made to kill, kiln's own process
    /// untouched. Before this, production's agents ran in the server's own cgroup, which hit its
    /// `memory.high` 15.9 million times.
    #[test]
    fn agents_memory_is_held_in_their_folder_and_never_takes_kilns_floor() -> anyhow::Result<()> {
        const MIB: u64 = 1 << 20;
        // kiln's floor 64 MiB (32 come what may), the unit slowed at 255 and full at 256: the agents
        // get 192, slowed past 191. Nothing written out to swap, so what's past their most is killed.
        let props = ["MemoryMin=32M", "MemoryLow=64M", "MemoryHigh=255M", "MemoryMax=256M", "MemorySwapMax=0"];
        let Some((mut held, rel)) = delegated_scope("kiln-memory-test", &props)? else {
            eprintln!("no delegated scope of the user's own here: skipped");
            return Ok(());
        };
        let dir = Path::new(ROOT).join(rel.trim_start_matches('/'));
        let agents = dir.join(AGENTS);
        let read = |p: &Path| fs::read_to_string(p).map(|t| t.trim().to_owned()).unwrap_or_default();
        let event = |p: &Path, key: &str| read(&p.join("memory.events.local")).lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.parse::<u64>().ok()).unwrap_or(0);
        let mut group = None;
        let checked = (|| {
            arrange(&dir).map_err(anyhow::Error::msg)?;
            let budget = keep_floor(&dir, &agents).map_err(anyhow::Error::msg)?;
            // A process this test starts goes into the scope only from a cgroup the user may write
            // (not from a system service's, as in an agent's session on production's server).
            let Some(folder) = Base::under(&rel, AGENTS, true) else {
                eprintln!("this test's own cgroup isn't the user's to move processes out of: skipped");
                return Ok(());
            };
            let agent = folder.group("01a1mem", true, true).ok_or_else(|| anyhow::anyhow!("no agent's group"))?;
            group = Some(agent.clone());
            // Something harmless first: where it can't be moved there, the agent's work would run
            // where this test runs.
            let mut probe = Command::new("sleep");
            probe.arg("30").stdin(std::process::Stdio::null());
            agent.enter(&mut probe);
            let mut probe = probe.spawn()?;
            let inside = agent.started(probe.id());
            let _ = probe.kill();
            let _ = probe.wait();
            if !inside {
                eprintln!("this test's process can't move processes into its scope: skipped");
                return Ok(());
            }
            // The agent's work: 384 MiB held at once, twice what the agents may hold together.
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "head -c 402653184 /dev/zero | tail -c 402653184 >/dev/null"]).stdin(std::process::Stdio::null());
            agent.enter(&mut cmd);
            let mut work = cmd.spawn()?;
            let mut ended = None;
            for _ in 0..1200 {
                ended = work.try_wait()?;
                if ended.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if ended.is_none() {
                let _ = work.kill();
                let _ = work.wait();
            }
            // It was killed in the agents' folder; the unit itself was never slowed, filled or made
            // to kill, and kiln's own process runs on.
            anyhow::ensure!(ended.is_some_and(|s| !s.success()), "the agent's work ran past the agents' most: {ended:?}");
            for key in ["high", "max", "oom", "oom_kill"] {
                anyhow::ensure!(event(&dir, key) == 0, "the unit itself took the agents' work ({key}): {}", read(&dir.join("memory.events.local")).replace('\n', ", "));
            }
            anyhow::ensure!(event(&agents, "oom_kill") >= 1, "nothing was killed in the agents' folder: {}", read(&agents.join("memory.events.local")).replace('\n', ", "));
            anyhow::ensure!(held.try_wait()?.is_none(), "kiln's own process didn't run on");
            // The floor in kiln's own leaf alone, and the agents' share of the rest.
            anyhow::ensure!(read(&dir.join(LEAF).join("memory.min")) == (32 * MIB).to_string() && read(&dir.join(LEAF).join("memory.low")) == (64 * MIB).to_string(), "the leaf's floor isn't the unit's");
            anyhow::ensure!(read(&agents.join("memory.max")) == (192 * MIB).to_string() && read(&agents.join("memory.high")) == (191 * MIB).to_string(), "the agents' share: {budget:?}");
            anyhow::ensure!(budget.max == 192 * MIB && budget.high == Some(191 * MIB), "{budget:?}");
            Ok(())
        })();
        if let Some(g) = group {
            g.kill();
            g.remove();
        }
        let _ = fs::remove_dir(&agents);
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

    /// Goal 8 (plan 01a1a59c2b29 step 7): a leg's or an agent's process takes what of the CPUs it would
    /// start on are work CPUs (a leg's share stays its own), or the work CPUs whole where none of them
    /// is (started from a thread fixed to a shard's core).
    #[test]
    fn g8_s7_a_process_keeps_what_of_its_cpus_are_work_cpus() {
        let set = |cpus: &[u32]| cpu_set(cpus);
        let work = set(&[0, 1, 2, 3, 8, 9, 10, 11]);
        assert!(narrowed(&set(&[2, 3, 4, 5]), &work) == set(&[2, 3]), "a leg's share keeps what of it is work CPUs");
        assert!(narrowed(&set(&[5]), &work) == work, "from a shard's core alone: the work CPUs whole");
        assert!(narrowed(&work, &work) == work);
        let everything: Vec<u32> = (0..16).collect();
        assert!(narrowed(&set(&everything), &work) == work, "from every CPU: the work CPUs");
        assert_eq!(cpu_set(&[1, 5000]).count(), 1, "a CPU past what a set holds is left out");
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7): a process started on the work CPUs runs on them from its
    /// first instruction, from a thread that may run anywhere and from one fixed to a CPU the work
    /// CPUs leave out (a shard's), and so does what it starts.
    #[test]
    fn g8_s7_a_leg_or_agent_starts_on_the_work_cpus() -> anyhow::Result<()> {
        use rustix::thread::{CpuSet, sched_getaffinity, sched_setaffinity};
        let mine: Vec<u32> = {
            let s = sched_getaffinity(None)?;
            (0..CpuSet::MAX_CPU).filter(|c| s.is_set(*c)).filter_map(|c| u32::try_from(c).ok()).collect()
        };
        // The first of this test's CPUs stands in for the server's: two at least.
        let (Some(server), Some(rest)) = (mine.first().copied(), mine.get(1..).filter(|r| !r.is_empty())) else { return Ok(()) };
        let work = cpu_set(rest);
        let started_on = move |fixed: Option<u32>| -> anyhow::Result<(CpuSet, CpuSet)> {
            std::thread::spawn(move || -> anyhow::Result<(CpuSet, CpuSet)> {
                if let Some(cpu) = fixed {
                    sched_setaffinity(None, &cpu_set(&[cpu]))?;
                }
                // It starts a process of its own and says its id.
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "sleep 5 & echo $!; wait"]).stdout(std::process::Stdio::piped());
                start_on(&mut cmd, work);
                let mut child = cmd.spawn()?;
                let pid = rustix::process::Pid::from_raw(i32::try_from(child.id())?).ok_or_else(|| anyhow::anyhow!("no pid"))?;
                let mut line = String::new();
                if let Some(out) = child.stdout.take() {
                    std::io::BufRead::read_line(&mut std::io::BufReader::new(out), &mut line)?;
                }
                let grandchild = line.trim().parse::<i32>().ok();
                let theirs = sched_getaffinity(Some(pid));
                let its = grandchild.and_then(rustix::process::Pid::from_raw).map(|g| sched_getaffinity(Some(g)));
                if let Some(g) = grandchild {
                    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(g), nix::sys::signal::Signal::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let its = its.ok_or_else(|| anyhow::anyhow!("what it started never said its id: {line:?}"))??;
                Ok((theirs?, its))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("the starting thread panicked"))?
        };
        for fixed in [None, Some(server)] {
            let (theirs, its) = started_on(fixed)?;
            anyhow::ensure!(theirs == work && its == work, "started from {fixed:?}: on {theirs:?}, what it started on {its:?}, not {work:?}");
        }
        Ok(())
    }
}
