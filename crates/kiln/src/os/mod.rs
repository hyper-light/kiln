//! The platform layer. Everything kiln does differently on Linux, macOS and Windows lives behind
//! this module, so the rest of the code has no `cfg`s.
//!
//! * **Unix** ([`unix`], Linux and macOS): child pipes and PTY masters are non-blocking and driven
//!   by hyper-rt readiness (epoll on Linux, kqueue on macOS); process trees are process groups.
//!   Linux additionally awaits exits through a pidfd and reaps orphans as PID 1; macOS has no
//!   pidfd, so exits are a short `try_wait` poll.
//! * **Windows** ([`windows`]): hyper-rt readiness covers sockets only (AFD), so child pipes and
//!   the ConPTY are read and written on dedicated threads, exits are a short `try_wait` poll, and
//!   process trees are job objects (killed as one, and with the agent if it dies).
//!
//! Both implementations export the same items; the portable helpers below sit on top of them.

use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
/// The Win32 calls installing needs (elevation, the user's `Path`, UAC) and a volume's room,
/// declared by hand.
#[cfg(windows)]
mod win32;

/// The allocator and how it gives memory back: jemalloc's settings, glibc's arenas (goal 11, plan
/// 01a1a87b2e56 step 4).
pub mod alloc;
pub mod battery;
/// The unit tests' allocator, counting what each thread allocates (goal 9, plan 01a1a87b2cec step 5).
#[cfg(test)]
pub mod counted;
pub mod cgroup;
/// kiln's own code signature on a Mac, so macOS knows every upgrade as one program.
pub mod macsign;

/// Child processes: [`crate::proc`]'s implementation.
#[cfg(unix)]
#[path = "proc_unix.rs"]
pub mod proc;
#[cfg(windows)]
#[path = "proc_windows.rs"]
pub mod proc;

/// `kiln supervise`'s child control.
#[cfg(unix)]
#[path = "supervise_unix.rs"]
pub mod supervise;
#[cfg(windows)]
#[path = "supervise_windows.rs"]
pub mod supervise;

/// Each agent session's processes held together by the system, so everything an agent started is
/// known (however it was parented since), whose a process is takes one look, and ending them all
/// is one call: a cgroup of its own on Linux, a job object on Windows, followed fork by fork on
/// macOS. Where there's none, `Group::of` is `None` and an agent's processes are found by their
/// mark (`crate::harness::survivors`).
#[cfg(target_os = "linux")]
#[path = "contain_linux.rs"]
pub mod contain;
#[cfg(windows)]
#[path = "contain_windows.rs"]
pub mod contain;
#[cfg(target_os = "macos")]
#[path = "contain_macos.rs"]
pub mod contain;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[path = "contain_none.rs"]
pub mod contain;

/// PTY session I/O: [`crate::terminal`]'s plumbing.
#[cfg(unix)]
#[path = "pty_unix.rs"]
pub mod pty;
#[cfg(windows)]
#[path = "pty_windows.rs"]
pub mod pty;

/// Host and process metrics: [`crate::host`]'s implementation (procfs on Linux, sysinfo elsewhere).
#[cfg(target_os = "linux")]
#[path = "metrics_procfs.rs"]
pub mod metrics;
#[cfg(not(target_os = "linux"))]
#[path = "metrics_sysinfo.rs"]
pub mod metrics;

/// This binary's own pages in resident memory, taken out once they're done with: [`crate::resident`]'s
/// implementation (Linux; elsewhere left as they are). Goal 11, plan 01a1a87b2e56 step 5.
#[cfg(target_os = "linux")]
#[path = "pages_linux.rs"]
pub mod pages;
#[cfg(not(target_os = "linux"))]
#[path = "pages_none.rs"]
pub mod pages;

/// The calling thread's own stack in resident memory: a runtime thread's kept off huge pages, a cold pool worker's
/// unused pages given back ([`alloc`]'s hooks; Linux, elsewhere left as they are). Goal 11, plan 01a1a87b2e56 step 5.
#[cfg(target_os = "linux")]
#[path = "stack_linux.rs"]
pub mod stack;
#[cfg(not(target_os = "linux"))]
#[path = "stack_none.rs"]
pub mod stack;

/// Transparent huge pages for the server's process: off where nothing it starts would inherit the switch (Linux;
/// elsewhere there are none to turn off). Goal 11, plan 01a1a87b2e56 step 5.
pub mod thp;

/// What a reach run's group of processes is named with (`run-<id>`, `crate::reach::serve`): a
/// person's or an agent's, never an agent's session.
pub const RUN_GROUP: &str = "run-";

/// The leaf of a delegated systemd unit's cgroup that kiln's own processes (its supervisor and
/// runners) run in, so the unit's cgroup holds none and can share the CPU among the legs and agents
/// beside it (docs/priority.md): what `kiln install` names systemd's `DelegateSubgroup=`, and where
/// a runner moves them where systemd has none.
pub const UNIT_LEAF: &str = "kiln";

/// A CPU list as Linux writes one (`0-3,8,10-11`: cpuset(7)'s list format, systemd's
/// `AllowedCPUs=`): the CPUs it names, in order, each once; why not, for one that isn't.
pub fn cpu_list(text: &str) -> Result<Vec<u32>, String> {
    /// No Linux numbers more CPUs than this: `NR_CPUS` at its largest (x86's `MAXSMP`).
    const MOST: u32 = 8192;
    let mut out = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let cpu = |s: &str| s.trim().parse::<u32>().ok().filter(|c| *c < MOST).ok_or_else(|| format!("{part:?} isn't a CPU (0 to {}) or a range of them (`8-15`)", MOST - 1));
        let (a, b) = (cpu(a)?, cpu(b)?);
        if b < a {
            return Err(format!("{part:?} runs backwards"));
        }
        out.extend(a..=b);
    }
    out.sort_unstable();
    out.dedup();
    if out.is_empty() { Err(format!("{text:?} names no CPU")) } else { Ok(out) }
}

/// Cores of the server's own (`[server] cores`, docs/priority.md "Cores of its own"; goal 8, plan
/// 01a1a59c2b29 step 6): those of the CPU list `list` this process may run on, in order; why none,
/// when none.
pub fn own_cores(list: &str) -> Result<Vec<u32>, String> {
    let cores = cpu_list(list)?;
    #[cfg(target_os = "linux")]
    {
        let allowed = rustix::thread::sched_getaffinity(None).map_err(|e| format!("this process's CPUs can't be read: {e}"))?;
        // A CPU past what a CPU set holds is none this process may run on (and reading it would index past the set).
        let mine: Vec<u32> = cores.into_iter().filter(|c| usize::try_from(*c).is_ok_and(|c| c < rustix::thread::CpuSet::MAX_CPU && allowed.is_set(c))).collect();
        if mine.is_empty() {
            return Err(format!("this process may run on none of {list}: its kiln.slice doesn't give them to it (`kiln install` writes it)"));
        }
        Ok(mine)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cores;
        Err("cores of the server's own are Linux's alone (a slice's cpuset)".to_owned())
    }
}

/// CPUs as a list, as Linux writes one (`0-3,8,10-11`): what [`cpu_list`] reads.
pub fn cpu_list_text(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(u32, u32)> = None;
    for c in sorted {
        run = match run {
            Some((a, b)) if b.checked_add(1) == Some(c) => Some((a, c)),
            Some((a, b)) => {
                out.push(if a == b { a.to_string() } else { format!("{a}-{b}") });
                Some((c, c))
            }
            None => Some((c, c)),
        };
    }
    if let Some((a, b)) = run {
        out.push(if a == b { a.to_string() } else { format!("{a}-{b}") });
    }
    out.join(",")
}

/// Where kiln's own legs and agents run beside a server with cores of its own (`[server] cores`;
/// docs/priority.md, "Legs and agents keep off the server's cores"; goal 8, plan 01a1a59c2b29 step
/// 7): the work CPUs. Each leg's and agent's processes take them from their first instruction, so a
/// shard waking on its core finds no hog there, whatever the kernel's preemption (under lazy
/// preemption a shard waking beside a process busy in user space waits up to a tick, 1 ms, for it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkCpus {
    /// On these: the CPUs this process may run on, less the server's.
    Apart(Vec<u32>),
    /// Where they'd run anyway: this process may run on none of the server's cores (a system install's
    /// partition keeps everything outside its slice off them, or a runner is held elsewhere).
    Clear,
    /// On the server's cores: they're every CPU this process may run on (its slice holds it there,
    /// as it holds the server's own runner).
    Shared,
    /// On the server's cores but off its shards': they're every CPU this process may run on (its slice
    /// holds it there, as a system install's partition holds the server's own runner), and these are
    /// those of them its shards aren't fixed to (goal 9, plan 01a1a87b2cec step 4).
    Beside(Vec<u32>),
}

/// [`WorkCpus`] for a process that may run on `allowed`, beside a server whose cores are `server` and
/// whose shards are fixed to `shards` of them (none known: a runner of its own beside the server).
pub fn work_cpus_for(allowed: &[u32], server: &[u32], shards: &[u32]) -> WorkCpus {
    if !allowed.iter().any(|c| server.contains(c)) {
        return WorkCpus::Clear;
    }
    let rest: Vec<u32> = allowed.iter().copied().filter(|c| !server.contains(c)).collect();
    if !rest.is_empty() {
        return WorkCpus::Apart(rest);
    }
    let beside: Vec<u32> = allowed.iter().copied().filter(|c| !shards.contains(c)).collect();
    if beside.is_empty() || beside.len() == allowed.len() { WorkCpus::Shared } else { WorkCpus::Beside(beside) }
}

impl WorkCpus {
    /// What it means, beside the server's cores `server` (as declared): for the log, a runner's facts
    /// and `/healthz/server`.
    pub fn said(&self, server: &str) -> String {
        match self {
            WorkCpus::Apart(cpus) => format!("kiln's own legs and agents keep off the server's cores {server}: they run on {}", cpu_list_text(cpus)),
            WorkCpus::Clear => format!("kiln's own legs and agents run where they did: this process may run on none of the server's cores {server}"),
            WorkCpus::Shared => format!(
                "kiln's own legs and agents share the server's cores {server}: this process may run on them alone, as its slice holds everything the server starts; a runner of its own beside the server keeps them off ([local_agent] enabled = false, and `kiln install --agent`)"
            ),
            WorkCpus::Beside(cpus) => format!(
                "kiln's own legs and agents run on the server's cores {server}, off its shards': on {}, as this process may run on the server's cores alone (its slice holds everything the server starts); a runner of its own beside the server keeps them off all of them ([local_agent] enabled = false, and `kiln install --agent`)",
                cpu_list_text(cpus)
            ),
        }
    }
}

/// Keeps kiln's own legs and agents off the server's cores, `server` (a CPU list, `[server] cores`),
/// as this process starts them from now on (goal 8, plan 01a1a59c2b29 step 7), or, where its slice
/// holds this process on them, off the cores of them its shards are fixed to, `shards` (goal 9, plan
/// 01a1a87b2cec step 4: a system install's partition holds the server's own runner there), and says
/// what it did and why; `whose` starts them (for the log). Worked out once, from the CPUs the calling
/// thread may run on: call it on the thread that starts the runtime, before it starts and before a
/// server fixes its shards to their cores (`crate::rt::pin`). Linux only: elsewhere a server has no
/// cores of its own (`own_cores`), so nothing is kept off and it says so.
pub fn keep_work_off(server: &str, shards: &[u32], whose: &str) {
    let cores = match cpu_list(server) {
        Ok(cores) => cores,
        Err(why) => {
            tracing::warn!(whose, why = %why, "kiln's own legs and agents run where they did: [server] cores isn't a CPU list");
            return;
        }
    };
    #[cfg(target_os = "linux")]
    match contain::keep_work_off(&cores, shards, server) {
        Ok(work) => tracing::info!(whose, "{}", work.said(server)),
        Err(why) => tracing::warn!(whose, why = %why, "kiln's own legs and agents run where they did"),
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cores, shards);
        tracing::info!(whose, "kiln's own legs and agents run where they did: cores of the server's own are Linux's alone");
    }
}

/// Gives up the room for Linux's real-time class (`RLIMIT_RTPRIO`, soft and hard, set to 0) this process
/// has, or process `pid`'s: a server does once its shards have asked for the class (`crate::rt::realtime`;
/// goal 9, plan 01a1a87b2cec step 4), so the room its unit gave them (`LimitRTPRIO=`) is theirs alone,
/// and nothing it starts from then on, its own runner's legs and agents among them, inherits it and takes
/// the class beside them. A thread in the class already keeps it: the limit bounds asking, not having.
/// Why not, where it can't. Linux only: elsewhere there's no such room to give up.
pub fn drop_realtime_room(pid: Option<u32>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let pid = pid.map(|p| i32::try_from(p).ok().and_then(rustix::process::Pid::from_raw).ok_or_else(|| format!("{p} isn't a process's id"))).transpose()?;
        let none = rustix::process::Rlimit { current: Some(0), maximum: Some(0) };
        rustix::process::prlimit(pid, rustix::process::Resource::Rtprio, none).map(|_| ()).map_err(|e| format!("RLIMIT_RTPRIO can't be set: {e}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Ok(())
    }
}

/// `RLIMIT_RTPRIO` as `/proc/<pid>/limits` gives it (`Max realtime priority  0  0`): its soft and hard
/// limits, `u64::MAX` for unlimited.
pub fn rtprio_limits(limits: &str) -> Option<(u64, u64)> {
    let rest = limits.lines().find_map(|l| l.strip_prefix("Max realtime priority"))?;
    let limit = |s: &str| if s == "unlimited" { Some(u64::MAX) } else { s.parse().ok() };
    let mut words = rest.split_whitespace();
    Some((limit(words.next()?)?, limit(words.next()?)?))
}

/// One CPU this process may run on, placed in its machine as Linux's topology files place it (goal 9, plan
/// 01a1a87b2cec step 4): what `crate::rt` fixes the server's shards to where they take the real-time class and
/// no cores of the server's own are declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// The kernel's id for it, what affinity calls take.
    pub id: u32,
    /// The physical core it's a hardware thread of, as the lowest id among its SMT siblings
    /// (`topology/thread_siblings_list`, itself among them); its own where none is said.
    pub core: u32,
    /// The last-level cache it shares, as the lowest id among the CPUs sharing its highest-level cache
    /// (`cache/index<n>/shared_cpu_list` of the largest `level`); its core's where none is said.
    pub llc: u32,
    /// How fast its class is, higher faster: on a hybrid machine whose kernel lists its core types
    /// (`/sys/devices/cpu_core/cpus` and `cpu_atom/cpus`), 1 for a performance core and 0 for an efficiency one, whatever
    /// the cores' capacities (which may differ by a favoured core's turbo within a type); else its `cpu_capacity` (the
    /// largest core's is 1024, a big.LITTLE machine's clusters apart); else 1. Only ever compared with the other CPUs of
    /// its machine, read the same way.
    pub rank: u32,
}

impl Cpu {
    /// CPU `id` from what Linux's files say of it: its SMT siblings (`thread_siblings_list`), each of its caches'
    /// level and the CPUs sharing it (`cache/index<n>/level` and `shared_cpu_list`), its capacity
    /// (`cpu_capacity`), and its core type where a hybrid machine's kernel lists it (`Some(true)` for an efficiency
    /// core).
    pub fn of(id: u32, siblings: Option<&str>, caches: &[(u32, &str)], capacity: Option<&str>, efficiency: Option<bool>) -> Cpu {
        let lowest = |list: &str| cpu_list(list).ok().and_then(|cpus| cpus.first().copied());
        let core = siblings.and_then(lowest).unwrap_or(id);
        let llc = caches.iter().max_by_key(|(level, _)| *level).and_then(|(_, shared)| lowest(shared)).unwrap_or(core);
        let rank = match efficiency {
            Some(efficiency) => u32::from(!efficiency),
            None => capacity.and_then(|c| c.trim().parse().ok()).unwrap_or(1),
        };
        Cpu { id, core, llc, rank }
    }
}

/// The CPUs the calling thread may run on, in order, each placed in its machine ([`Cpu`]) from Linux's sysfs.
/// None off Linux, nor where the thread's CPUs can't be read.
pub fn cpu_topology() -> Vec<Cpu> {
    #[cfg(target_os = "linux")]
    {
        let Ok(allowed) = rustix::thread::sched_getaffinity(None) else { return Vec::new() };
        let read = |path: &str| std::fs::read_to_string(path).ok();
        // A hybrid machine's core types, where its kernel lists them: none on a machine of one type.
        let listed = |path: &str| read(path).and_then(|list| cpu_list(&list).ok()).unwrap_or_default();
        let (performance, efficiency) = (listed("/sys/devices/cpu_core/cpus"), listed("/sys/devices/cpu_atom/cpus"));
        let core_type = |id: u32| match (performance.contains(&id), efficiency.contains(&id)) {
            (false, false) => None,
            (_, efficient) => Some(efficient),
        };
        (0..rustix::thread::CpuSet::MAX_CPU)
            .filter(|c| allowed.is_set(*c))
            .filter_map(|c| u32::try_from(c).ok())
            .map(|id| {
                let dir = format!("/sys/devices/system/cpu/cpu{id}");
                // Each cache's level and who shares it; the directory's other entries (`uevent`) have neither.
                let caches: Vec<(u32, String)> = std::fs::read_dir(format!("{dir}/cache"))
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let path = entry.ok()?.path();
                        let level = std::fs::read_to_string(path.join("level")).ok()?.trim().parse().ok()?;
                        Some((level, std::fs::read_to_string(path.join("shared_cpu_list")).ok()?))
                    })
                    .collect();
                let caches: Vec<(u32, &str)> = caches.iter().map(|(level, shared)| (*level, shared.as_str())).collect();
                Cpu::of(id, read(&format!("{dir}/topology/thread_siblings_list")).as_deref(), &caches, read(&format!("{dir}/cpu_capacity")).as_deref(), core_type(id))
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    Vec::new()
}

/// The round-robin real-time class's quantum, nanoseconds: how long a `SCHED_RR` thread keeps its CPU before
/// another of its priority waiting there gets a turn (Linux's `/proc/sys/kernel/sched_rr_timeslice_ms`; 100 ms
/// unless root set another). None off Linux, and where it can't be read.
pub fn rr_quantum_ns() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        rr_quantum_of(&std::fs::read_to_string("/proc/sys/kernel/sched_rr_timeslice_ms").ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// [`rr_quantum_ns`] from the file's milliseconds: none for what isn't a positive count (the kernel takes 0 or
/// less as its default, and writes that back).
#[cfg(any(target_os = "linux", test))]
fn rr_quantum_of(ms: &str) -> Option<u64> {
    ms.trim().parse::<u64>().ok().filter(|ms| *ms > 0)?.checked_mul(1_000_000)
}

/// Makes `cmd`'s process, a leg's or an agent's, start on the work CPUs ([`keep_work_off`]) from its
/// first instruction, and so everything it starts: those of the CPUs it would start on that are work
/// CPUs (a leg's share stays its share), else the work CPUs (started from a thread fixed to a shard's
/// core). Nothing where nothing is kept off. Linux only: macOS has no CPU affinity for a process (its
/// legs and agents run below normal, on every core), and on
/// Windows a server has no cores of its own to keep anything off.
pub fn onto_work_cpus(cmd: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    contain::onto_work_cpus(cmd);
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

/// Moves process `pid`, a leg's step started some other way (on a pty), onto the work CPUs as
/// [`onto_work_cpus`] starts one there: what it started before stays where it was. Whether it moved
/// any of its threads. Linux only, as [`onto_work_cpus`].
pub fn onto_work_cpus_pid(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        contain::onto_work_cpus_pid(pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// The work CPUs ([`keep_work_off`]) where kiln's own legs and agents keep off the server's cores:
/// what a runner lends its legs from (`crate::runner::cpus`) and gives their containers. `None`
/// where nothing is kept off, and off Linux.
pub fn work_cpus() -> Option<Vec<usize>> {
    #[cfg(target_os = "linux")]
    {
        contain::work_cpus()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// What [`keep_work_off`] decided, as [`WorkCpus::said`] says it: `None` where the server's cores
/// weren't declared to this process.
pub fn work_said() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        contain::work_said()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Keeps the calling thread, and every thread it starts from here on, off `cores`: its affinity less
/// them (Linux's `sched_setaffinity`). What a server with cores of its own does before its runtime
/// starts, so its shards alone run on theirs (goal 9, plan 01a1a87b2cec step 4): its pools' and
/// other threads, started after, inherit the rest. Why not, where it can't: another OS, or `cores`
/// all the thread may run on.
pub fn keep_threads_off(cores: &[u32]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let mut set = rustix::thread::sched_getaffinity(None).map_err(|e| format!("this thread's CPUs can't be read: {e}"))?;
        for c in cores.iter().filter_map(|c| usize::try_from(*c).ok()) {
            set.unset(c);
        }
        if set.count() == 0 {
            return Err("those are all the CPUs it may run on".to_owned());
        }
        rustix::thread::sched_setaffinity(None, &set).map_err(|e| format!("this thread's CPUs can't be set: {e}"))?;
        let _ = APART.set((cores.to_vec(), set));
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cores;
        Err("cores of the server's own are Linux's alone (a slice's cpuset)".to_owned())
    }
}

/// The shards' cores [`keep_threads_off`] kept the process's other threads off, and the CPUs it left
/// them: what [`keep_off_shards`] moves a thread to.
#[cfg(target_os = "linux")]
static APART: std::sync::OnceLock<(Vec<u32>, rustix::thread::CpuSet)> = std::sync::OnceLock::new();

/// The calling thread's id in the kernel (Linux's `gettid`), for [`keep_off_shards`] to tell a
/// shard's own thread from the rest; none on other platforms.
pub fn thread_id() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        u32::try_from(rustix::thread::gettid().as_raw_nonzero().get()).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Moves to the CPUs [`keep_threads_off`] left the process's other threads each of its threads not
/// in `seen` that may run on its shards' cores, but for the shards' own (`shards`, their kernel ids,
/// [`thread_id`]), and notes each it looked at in `seen`, which keeps only threads still running:
/// how many it moved (goal 9, plan 01a1a87b2cec step 4). A thread starts with the CPUs of the thread
/// that started it, so one that a task on a shard starts (the server starts most of its threads from
/// tasks) would run on that shard's core beside it. A thread is looked at once, as it is first seen:
/// one that later fixes itself to a CPU of its own choosing keeps it. Nothing where nothing was kept
/// apart, and on other platforms.
pub fn keep_off_shards(shards: &[u32], seen: &mut std::collections::HashSet<u32>) -> usize {
    #[cfg(target_os = "linux")]
    {
        let Some((cores, rest)) = APART.get() else { return 0 };
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else { return 0 };
        let now: std::collections::HashSet<u32> = entries.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok()).collect();
        let moved = now.iter().filter(|tid| !seen.contains(*tid) && !shards.contains(*tid)).filter(|tid| move_off(**tid, cores, rest)).count();
        *seen = now;
        moved
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (shards, seen);
        0
    }
}

/// Moves the child `pid` the calling thread just started off the shards' cores, as
/// [`keep_off_shards`] moves the process's threads: a child started from a shard's thread runs on
/// that shard's core (goal 9, plan 01a1a87b2cec step 4). Whether it moved it: not where nothing was
/// kept apart, nor a child that started off them.
pub fn keep_child_off_shards(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some((cores, rest)) = APART.get() else { return false };
        // Its threads: just started, it has one, unless it started more already.
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return false };
        entries.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok()).filter(|tid| move_off(*tid, cores, rest)).count() > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Moves the thread `tid` to `rest` if it may run on any of `cores`: whether it did. A thread that
/// ended, or one confined where none of `rest` is, stays as it was.
#[cfg(target_os = "linux")]
fn move_off(tid: u32, cores: &[u32], rest: &rustix::thread::CpuSet) -> bool {
    let Some(pid) = i32::try_from(tid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
    let on_shards = |set: &rustix::thread::CpuSet| cores.iter().filter_map(|c| usize::try_from(*c).ok()).any(|c| set.is_set(c));
    rustix::thread::sched_getaffinity(Some(pid)).is_ok_and(|set| on_shards(&set)) && rustix::thread::sched_setaffinity(Some(pid), rest).is_ok()
}

/// The server first (docs/priority.md; goal 8, plan 01a1a59c2b29 steps 2 to 4): on Windows its
/// process runs above normal priority; on Linux and macOS its service says how it runs (the
/// weight of its `kiln.slice`, launchd's `ProcessType`), as `kiln install` writes it.
pub fn server_first() {
    #[cfg(windows)]
    match contain::server_first() {
        Ok(()) => tracing::info!("the server runs above normal priority"),
        Err(e) => tracing::warn!(error = %e, "the server runs at normal priority: Windows refused above normal"),
    }
}

/// Lowers the calling thread's scheduling priority, ten nice levels below its own (19 at the
/// most): work that may wait (a query's helper threads) yields to what may not (the writes the
/// server takes). Linux's threads each have their own nice value (`setpriority(PRIO_PROCESS, tid)`);
/// elsewhere the thread runs as it was.
pub fn lower_thread_priority() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let me = rustix::thread::gettid();
        if let Ok(now) = rustix::process::getpriority_process(Some(me)) {
            let _ = rustix::process::setpriority_process(Some(me), (now + 10).min(19));
        }
    }
}

/// The first executable called `name` on `PATH`. On Windows a name without a program's extension
/// is looked for as cmd.exe looks for it, as `name` + each `PATHEXT` extension a process starts
/// from, never bare: npm puts a shell script `codex`, which Windows can't start, beside `codex.cmd`.
pub fn which(name: &str) -> Option<PathBuf> {
    which_in(name, &std::env::var_os("PATH")?)
}

/// [`which`] over `paths`, a `PATH` value.
pub fn which_in(name: &str, paths: &std::ffi::OsStr) -> Option<PathBuf> {
    let pathext = cfg!(windows).then(|| std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned()));
    find_in(name, paths, pathext.as_deref())
}

/// [`which_in`] with Windows' `PATHEXT` (None: Unix, where a program's name is its file's).
fn find_in(name: &str, paths: &std::ffi::OsStr, pathext: Option<&str>) -> Option<PathBuf> {
    let candidates = exe_names(name, pathext);
    std::env::split_paths(paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates.iter().map(move |n| dir.join(n)).collect::<Vec<_>>())
        .find(|p| p.is_file())
}

/// Whether `name` is an executable on `PATH`.
pub fn have(name: &str) -> bool {
    which(name).is_some()
}

/// This kiln's binary, as a path another process can start. On Linux, once an upgrade has put a new
/// binary in its place (and until this process takes it up, once its jobs are done), the path
/// reads `<path> (deleted)`: the kiln at `<path>` is the one to start.
pub fn this_kiln() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(replaced(&exe).unwrap_or(exe))
}

/// The binary now at a running binary's path that was replaced (`<path> (deleted)`).
fn replaced(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?.strip_suffix(" (deleted)").map(PathBuf::from).filter(|p| p.is_file())
}

/// Where [`add_to_user_path`] put a folder on the user's PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAdded {
    /// The shell startup file that puts it there (Unix); none where the system keeps the user's
    /// PATH itself (Windows).
    pub file: Option<PathBuf>,
    /// What puts it on this terminal's PATH too: only new terminals read that file.
    pub now: Option<String>,
    /// Whether that file was made for it.
    pub created: bool,
}

/// A Windows `Path` value (`;`-separated, as the registry keeps it) with `dir` added at its end,
/// or `None` when one of its entries is `dir` already. Entries are compared as Windows compares
/// folders: without case, quotes or a trailing `\`, and with `%VARIABLES%` expanded by `var`.
pub fn path_list_with(list: &str, dir: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let norm = |entry: &str| {
        let expanded = expand_percent_vars(entry.trim().trim_matches('"'), &var);
        expanded.trim_end_matches(['\\', '/']).to_lowercase()
    };
    let want = norm(dir);
    if list.split(';').any(|e| norm(e) == want) {
        return None;
    }
    let kept = list.trim_end_matches(';');
    Some(if kept.is_empty() { dir.to_owned() } else { format!("{kept};{dir}") })
}

/// A Windows `Path` value without its entries that are `dir` (compared as [`path_list_with`]
/// does), or `None` when none is.
pub fn path_list_without(list: &str, dir: &str, var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let norm = |entry: &str| expand_percent_vars(entry.trim().trim_matches('"'), &var).trim_end_matches(['\\', '/']).to_lowercase();
    let want = norm(dir);
    let kept: Vec<&str> = list.split(';').filter(|e| e.is_empty() || norm(e) != want).collect();
    (kept.len() != list.split(';').count()).then(|| kept.join(";"))
}

/// `%NAME%` replaced by `var(NAME)`; a name it doesn't know, and a lone `%`, stay as they are.
fn expand_percent_vars(s: &str, var: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let after = from.get(1..).unwrap_or_default();
        match after.find('%') {
            Some(end) => {
                let name = after.get(..end).unwrap_or_default();
                match var(name).filter(|_| !name.is_empty()) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(from.get(..end + 2).unwrap_or_default()),
                }
                rest = after.get(end + 1..).unwrap_or_default();
            }
            None => {
                out.push_str(from);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The size of the volume holding `path` and the bytes this user may still write on it (None:
/// unknown).
// The statvfs fields' types differ by platform: a conversion that is one elsewhere is none here.
#[allow(clippy::useless_conversion)]
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let s = nix::sys::statvfs::statvfs(path).ok()?;
        let frag = u64::from(s.fragment_size());
        Some((u64::from(s.blocks()).saturating_mul(frag), u64::from(s.blocks_available()).saturating_mul(frag)))
    }
    // Asked of the volume itself: `sysinfo`'s mount points (`C:\`) don't prefix the `\\?\` path
    // `canonicalize` gives.
    #[cfg(windows)]
    {
        win32::disk_space(path).ok()
    }
}

/// Bytes this user may still write on the volume holding `path` (None: unknown).
pub fn disk_available(path: &Path) -> Option<u64> {
    disk_space(path).map(|(_, available)| available)
}

/// Keep git from ever prompting (terminal, askpass helper, or Git Credential Manager's window):
/// a missing credential fails fast instead of hanging a job. On Windows there is no
/// `/bin/true` to answer askpass, and git without one simply fails the credential.
pub fn git_no_prompts(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never");
    if cfg!(unix) {
        cmd.env("GIT_ASKPASS", "/bin/true").env("SSH_ASKPASS", "/bin/true");
    }
    cmd
}

/// How a job step runs `script` by default: `bash -eo pipefail` (else `sh -e`) on Unix;
/// PowerShell 7 (else Windows PowerShell) on Windows, as GitHub Actions does there.
pub fn default_step_shell(script: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        let ps = if have("pwsh") { "pwsh" } else { "powershell" };
        return (ps.to_owned(), script_args(ps, script));
    }
    if have("bash") {
        let args = ["--noprofile", "--norc", "-eo", "pipefail", "-c", script];
        return ("bash".to_owned(), args.map(str::to_owned).to_vec());
    }
    ("sh".to_owned(), vec!["-ec".to_owned(), script.to_owned()])
}

/// The arguments that make shell `program` run `script`: `-c` for Unix shells, `/D /S /C` for
/// cmd, and for PowerShell `-Command` with errors made fatal and the last native exit code
/// passed on (GitHub Actions' wrapper).
pub fn script_args(program: &str, script: &str) -> Vec<String> {
    match program_stem(program).as_str() {
        "cmd" => ["/D", "/S", "/C", script].map(str::to_owned).to_vec(),
        "pwsh" | "powershell" => {
            let wrapped = format!(
                "$ErrorActionPreference = 'Stop'\n{script}\nif ((Test-Path -LiteralPath variable:\\LASTEXITCODE)) {{ exit $LASTEXITCODE }}"
            );
            vec!["-NoLogo".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), wrapped]
        }
        _ => vec!["-c".into(), script.to_owned()],
    }
}

/// Open `url` in the desktop's browser (best effort, never waits).
pub fn open_url(url: &str) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    let _ = cmd.arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// hyper-rt's signal kinds that stand for kiln's `SIGINT` / `SIGTERM` / `SIGHUP` here. Unix has
/// them as such; Windows delivers console control events, of which Ctrl-Break, closing the
/// console and system shutdown mean "terminate", and nothing means "reload".
pub fn signal_kinds(sig: i32) -> &'static [hyper_rt::signal::Signal] {
    use hyper_rt::signal::Signal;
    match sig {
        crate::rt::SIGINT => &[Signal::Interrupt],
        crate::rt::SIGTERM if cfg!(windows) => &[Signal::Break, Signal::Close, Signal::Shutdown],
        crate::rt::SIGTERM => &[Signal::Terminate],
        crate::rt::SIGHUP if cfg!(windows) => &[],
        crate::rt::SIGHUP => &[Signal::Hangup],
        _ => &[],
    }
}

/// kiln's signal number for a delivered hyper-rt signal kind.
pub fn signal_number(kind: hyper_rt::signal::Signal) -> i32 {
    use hyper_rt::signal::Signal;
    match kind {
        Signal::Interrupt => crate::rt::SIGINT,
        Signal::Hangup => crate::rt::SIGHUP,
        Signal::Terminate | Signal::Break | Signal::Close | Signal::Logoff | Signal::Shutdown => crate::rt::SIGTERM,
    }
}

/// The extensions of the files Windows starts as programs: binaries, and batch files (which the
/// standard library runs through cmd.exe). `PATHEXT`'s others (`.js`, `.vbs`) open in a program.
const PROGRAM_EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];

/// The names `name` may have on disk: itself on Unix; on Windows (`pathext` given) itself when
/// it has a program's extension, else it with each of `pathext`'s that is one, in that order.
fn exe_names(name: &str, pathext: Option<&str>) -> Vec<String> {
    let is_program = |ext: &str| PROGRAM_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str());
    let Some(pathext) = pathext else { return vec![name.to_owned()] };
    if Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(is_program) {
        return vec![name.to_owned()];
    }
    pathext.split(';').map(|e| e.trim().trim_start_matches('.')).filter(|e| is_program(e)).map(|e| format!("{name}.{}", e.to_ascii_lowercase())).collect()
}

/// The file name of a program, without directory or extension, lowercased: what to compare when
/// choosing how to talk to a shell (`C:\Program Files\PowerShell\7\pwsh.exe` → `pwsh`).
pub fn program_stem(program: &str) -> String {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && PROGRAM_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => stem,
        _ => name,
    };
    stem.to_ascii_lowercase()
}

/// This machine, as kiln tells machines apart: a hash of the id its OS keeps for it (macOS's
/// hardware UUID, Linux's `/etc/machine-id`, Windows' `MachineGuid`), so two machines with one host
/// name (two Macs both named "Ada's MacBook Pro") are still two. A container has no machine of its
/// own: its id is the one kept with its data ([`kept_machine_id`]). `None` where there is none to
/// read. Read once.
pub fn machine_id() -> Option<String> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        // Said outright (`KILN_MACHINE_ID`): a machine of its own whatever the OS says, as each runner
        // a test starts on one machine is, or a second runner someone runs on one machine on purpose.
        if let Some(said) = std::env::var("KILN_MACHINE_ID").ok().filter(|v| !v.trim().is_empty()) {
            return machine_id_of(&said);
        }
        if in_container() {
            let data = std::env::var_os("KILN_DATA_DIR").map(PathBuf::from).unwrap_or_else(crate::config::default_data_dir);
            kept_machine_id(&data, std::env::var("KILN_POD_UID").ok().as_deref())
        } else {
            raw_machine_id().as_deref().and_then(machine_id_of)
        }
    })
    .clone()
}

/// Whether kiln runs in a container (Docker's, Podman's, a Kubernetes pod's), whose
/// `/etc/machine-id` is missing or its image's, alike in every container of it.
fn in_container() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists() || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
}

/// A container's machine id: the one kept in its data (`data`, its volume where it has one), made
/// the first time from its pod's UID (`KILN_POD_UID`, from Kubernetes' Downward API) or at random.
/// Started again with the same volume, it's the same machine; made again with a new one (or none),
/// a new machine, as it keeps nothing.
fn kept_machine_id(data: &Path, pod_uid: Option<&str>) -> Option<String> {
    let file = data.join("machine-id");
    let kept = std::fs::read_to_string(&file).ok().map(|s| s.trim().to_ascii_lowercase());
    if let Some(id) = kept.filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Some(id);
    }
    let made = pod_uid.and_then(machine_id_of).or_else(|| machine_id_of(&crate::util::new_id()))?;
    if let Err(e) = std::fs::create_dir_all(data).and_then(|()| std::fs::write(&file, format!("{made}\n"))) {
        tracing::debug!(error = %e, file = %file.display(), "couldn't keep this container's machine id");
    }
    Some(made)
}

/// The id kiln gives a machine whose OS's own is `raw`: a hash of it, so the OS's id itself never
/// leaves the machine. `None` for an empty one.
fn machine_id_of(raw: &str) -> Option<String> {
    let raw = raw.trim().to_ascii_lowercase();
    (!raw.is_empty()).then(|| crate::util::sha256_hex(format!("kiln machine id\n{raw}").as_bytes()).chars().take(32).collect())
}

/// The id the OS keeps for this machine.
fn raw_machine_id() -> Option<String> {
    let output = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if cfg!(windows) {
        // The registry's 64-bit view, which a 32-bit `reg` wouldn't read otherwise.
        reg_query_value(&output("reg", &["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid", "/reg:64"])?, "MachineGuid")
    } else if cfg!(target_os = "macos") {
        ioreg_platform_uuid(&output("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])?)
    } else {
        // systemd's, else D-Bus's (the same id, where systemd isn't).
        ["/etc/machine-id", "/var/lib/dbus/machine-id"].into_iter().find_map(|p| std::fs::read_to_string(p).ok().and_then(|s| etc_machine_id(&s)))
    }
}

/// The id in an `/etc/machine-id`: none when it's empty, or `uninitialized` (an image made to get
/// its id at first boot, which every container of that image says alike).
fn etc_machine_id(file: &str) -> Option<String> {
    let id = file.trim();
    (!id.is_empty() && id != "uninitialized").then(|| id.to_owned())
}

/// macOS's hardware UUID in `ioreg -rd1 -c IOPlatformExpertDevice`'s output
/// (`"IOPlatformUUID" = "0A1B…"`).
fn ioreg_platform_uuid(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("\"IOPlatformUUID\"")?.trim_start().strip_prefix('=').map(|v| v.trim().trim_matches('"').to_owned()))
        .filter(|v| !v.is_empty())
}

/// Value `name` in `reg query`'s output (`    MachineGuid    REG_SZ    1b2c…`).
fn reg_query_value(out: &str, name: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        if words.next()? != name {
            return None;
        }
        words.nth(1).map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 8 (plan 01a1a59c2b29 step 6): `[server] cores` reads as Linux writes a CPU list, and
    /// says what's wrong with one that isn't.
    #[test]
    fn g8_s6_a_cpu_list_reads_as_linux_writes_one() {
        assert_eq!(cpu_list("8-11"), Ok(vec![8, 9, 10, 11]));
        assert_eq!(cpu_list(" 4-5, 2 ,4"), Ok(vec![2, 4, 5]));
        for bad in ["", "x", "3-1", "8192", "0-99999"] {
            assert!(cpu_list(bad).is_err(), "{bad:?} read");
        }
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7): a CPU list written as Linux writes one reads back as itself.
    #[test]
    fn g8_s7_cpus_are_written_as_linux_writes_a_list() {
        assert_eq!(cpu_list_text(&[0, 1, 2, 3, 8, 10, 11]), "0-3,8,10-11");
        assert_eq!(cpu_list_text(&[24, 25, 0, 26, 1]), "0-1,24-26");
        assert_eq!(cpu_list_text(&[5]), "5");
        assert_eq!(cpu_list_text(&[]), "");
        assert_eq!(cpu_list(&cpu_list_text(&[3, 4, 9])), Ok(vec![3, 4, 9]));
    }

    /// Goal 8 (plan 01a1a59c2b29 step 7): kiln's own legs and agents run on this process's CPUs less the
    /// server's; where they'd run anyway when this process has none of the server's (a partition keeps it
    /// off them); and share them, said so, when the server's are all this process has (its slice holds it).
    #[test]
    fn g8_s7_legs_and_agents_keep_off_the_servers_cores() {
        let all: Vec<u32> = (0..32).collect();
        let server: Vec<u32> = (16..24).collect();
        let shards: Vec<u32> = (17..21).collect();
        let rest: Vec<u32> = (0..16).chain(24..32).collect();
        assert_eq!(work_cpus_for(&all, &server, &shards), WorkCpus::Apart(rest));
        let lab: Vec<u32> = (16..32).collect();
        assert_eq!(work_cpus_for(&lab, &server, &shards), WorkCpus::Apart((24..32).collect()), "a runner held to 16-31 puts them on 24-31");
        assert_eq!(work_cpus_for(&(0..16).collect::<Vec<u32>>(), &server, &shards), WorkCpus::Clear);
        assert_eq!(work_cpus_for(&server, &server, &[]), WorkCpus::Shared, "held to the server's cores, its shards unknown");
        assert_eq!(WorkCpus::Apart((0..16).collect()).said("16-23"), "kiln's own legs and agents keep off the server's cores 16-23: they run on 0-15");
        assert!(WorkCpus::Shared.said("16-23").contains("share the server's cores 16-23"));
        assert!(WorkCpus::Clear.said("16-23").contains("none of the server's cores 16-23"));
    }

    /// Goal 9, plan 01a1a87b2cec step 4: where the server's slice holds its own runner on its cores (a
    /// system install's partition), kiln's own legs and agents run on those cores off its shards': the
    /// first, which its other threads keep, and those past the shards'; with every core a shard's, they
    /// share them, said so.
    #[test]
    fn g9_s4_legs_and_agents_in_a_partition_keep_off_the_shards_cores() {
        let server: Vec<u32> = (16..24).collect();
        let shards: Vec<u32> = (17..21).collect();
        let beside = work_cpus_for(&server, &server, &shards);
        assert_eq!(beside, WorkCpus::Beside(vec![16, 21, 22, 23]));
        assert_eq!(
            beside.said("16-23"),
            "kiln's own legs and agents run on the server's cores 16-23, off its shards': on 16,21-23, as this process may run on the server's cores alone (its slice holds everything the server starts); a runner of its own beside the server keeps them off all of them ([local_agent] enabled = false, and `kiln install --agent`)"
        );
        assert_eq!(work_cpus_for(&[8, 9], &[8, 9], &[9]), WorkCpus::Beside(vec![8]), "two cores: the shards on the second, legs and agents on the first");
        assert_eq!(work_cpus_for(&[8], &[8], &[8]), WorkCpus::Shared, "one core: the shards' and everything's");
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a server gives up the real-time room its unit gave its shards once
    /// they've asked, so what it starts inherits none: a process's `RLIMIT_RTPRIO` set to 0, soft and hard,
    /// read back from its `/proc/<pid>/limits` as Linux writes them.
    #[cfg(target_os = "linux")]
    #[test]
    fn g9_s4_the_real_time_room_is_given_up_whole() -> anyhow::Result<()> {
        assert_eq!(rtprio_limits("Max realtime priority     99                   99                   \n"), Some((99, 99)));
        assert_eq!(rtprio_limits("Max realtime priority     0                    unlimited            \n"), Some((0, u64::MAX)));
        assert_eq!(rtprio_limits("Max realtime timeout      unlimited            unlimited            us\n"), None);
        // A process of its own stands in for the server: whatever room it started with (this machine's), none after.
        let mut child = std::process::Command::new("sleep").arg("30").stdin(std::process::Stdio::null()).spawn()?;
        let pid = child.id();
        let before = rtprio_limits(&std::fs::read_to_string(format!("/proc/{pid}/limits"))?);
        let dropped = drop_realtime_room(Some(pid));
        let after = rtprio_limits(&std::fs::read_to_string(format!("/proc/{pid}/limits"))?);
        let _ = child.kill();
        let _ = child.wait();
        dropped.map_err(|e| anyhow::anyhow!(e))?;
        anyhow::ensure!(before.is_some() && after == Some((0, 0)), "before {before:?}, after {after:?}");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a server with cores of its own keeps its other threads off its
    /// shards' cores: the thread that starts the runtime, and every thread it starts after, run on the
    /// rest; asked to keep off every CPU it has, it says why not and changes nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn g9_s4_other_threads_keep_off_the_shards_cores() -> anyhow::Result<()> {
        std::thread::spawn(|| -> anyhow::Result<()> {
            let before = rustix::thread::sched_getaffinity(None)?;
            let mine: Vec<u32> = (0..rustix::thread::CpuSet::MAX_CPU).filter(|c| before.is_set(*c)).filter_map(|c| u32::try_from(c).ok()).collect();
            anyhow::ensure!(keep_threads_off(&mine).is_err(), "kept off every CPU it has");
            anyhow::ensure!(rustix::thread::sched_getaffinity(None)? == before, "a refusal changed its CPUs");
            let Some(off) = mine.first().copied().filter(|_| mine.len() > 1) else { return Ok(()) };
            keep_threads_off(&[off]).map_err(|e| anyhow::anyhow!(e))?;
            let child = std::thread::spawn(move || rustix::thread::sched_getaffinity(None).map(|s| s.is_set(off as usize))).join().map_err(|_| anyhow::anyhow!("the child panicked"))??;
            anyhow::ensure!(!rustix::thread::sched_getaffinity(None)?.is_set(off as usize) && !child, "CPU {off} still taken");
            Ok(())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("the test's thread panicked"))?
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a CPU's place in its machine reads as Linux writes it. Production's
    /// machine (AMD, SMT siblings n and n + 16, an L3 for each eight cores): CPU 24 is a thread of core 8 and
    /// shares the L3 of 8-15 and 24-31; its L1 and L2, shared with its sibling alone, aren't the last level.
    /// A hybrid machine's listed core types rank its cores whatever their capacities say (a favoured core's turbo
    /// within a type); a big.LITTLE machine's capacities rank its clusters; without siblings or caches said, a CPU is
    /// its own core and its core's cache.
    #[test]
    fn g9_s4_a_cpus_place_reads_as_linux_writes_it() {
        let caches = [(1, "8,24\n"), (1, "8,24\n"), (2, "8,24\n"), (3, "8-15,24-31\n")];
        assert_eq!(Cpu::of(24, Some("8,24\n"), &caches, Some("1024\n"), None), Cpu { id: 24, core: 8, llc: 8, rank: 1024 });
        assert_eq!(Cpu::of(3, Some("2-3\n"), &[(3, "0-31\n")], Some("987\n"), Some(false)), Cpu { id: 3, core: 2, llc: 0, rank: 1 }, "a performance core, its capacity below a favoured one's");
        assert_eq!(Cpu::of(20, Some("20\n"), &[(2, "20-23\n"), (3, "0-31\n")], Some("650\n"), Some(true)), Cpu { id: 20, core: 20, llc: 0, rank: 0 }, "an efficiency core");
        assert_eq!(Cpu::of(6, Some("6\n"), &[(2, "4-7\n")], Some("446\n"), None).rank, 446, "a little cluster's core");
        assert_eq!(Cpu::of(5, None, &[], None, None), Cpu { id: 5, core: 5, llc: 5, rank: 1 });
        assert_eq!(Cpu::of(5, Some("x"), &[(3, "?")], Some("big"), None), Cpu { id: 5, core: 5, llc: 5, rank: 1 }, "what isn't a list or a count is unsaid");
    }

    /// Goal 9, plan 01a1a87b2cec step 4: the round-robin real-time class's quantum reads from its file's
    /// milliseconds; what isn't a positive count is none (the kernel's default stands).
    #[test]
    fn g9_s4_the_round_robin_quantum_reads_as_linux_writes_it() {
        assert_eq!(rr_quantum_of("100\n"), Some(100_000_000));
        assert_eq!(rr_quantum_of("1"), Some(1_000_000));
        for none in ["0", "-1", "", "ms"] {
            assert_eq!(rr_quantum_of(none), None, "{none:?}");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 4: this machine's CPUs, as this thread may run on them, each placed in
    /// it: its core and its last-level cache named by CPUs that share them, at or below its own id, and its core
    /// among those its cache serves; the class's quantum is read where Linux keeps one.
    #[cfg(target_os = "linux")]
    #[test]
    fn g9_s4_this_machines_cpus_are_placed() -> anyhow::Result<()> {
        let cpus = cpu_topology();
        anyhow::ensure!(!cpus.is_empty(), "no CPU read");
        anyhow::ensure!(cpus.windows(2).all(|w| w.first().map(|c| c.id) < w.get(1).map(|c| c.id)), "in order, each once: {cpus:?}");
        for cpu in &cpus {
            anyhow::ensure!(cpu.core <= cpu.id && cpu.llc <= cpu.core, "{cpu:?}");
        }
        anyhow::ensure!(rr_quantum_ns().is_some_and(|ns| ns > 0), "the round-robin quantum unread");
        Ok(())
    }

    #[test]
    fn machines_are_told_apart_by_the_id_their_os_keeps() {
        let mac = "+-o Mac15,9  <class IOPlatformExpertDevice, id 0x100000253, registered, matched, active, busy 0 (0 ms), retain 39>\n  {\n    \"IOPlatformSerialNumber\" = \"XYZ123\"\n    \"IOPlatformUUID\" = \"0A1B2C3D-4E5F-6A7B-8C9D-0E1F2A3B4C5D\"\n  }\n";
        assert_eq!(ioreg_platform_uuid(mac).as_deref(), Some("0A1B2C3D-4E5F-6A7B-8C9D-0E1F2A3B4C5D"));
        assert_eq!(ioreg_platform_uuid("{\n  \"IOPlatformSerialNumber\" = \"XYZ123\"\n}\n"), None);
        let win = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\r\n    MachineGuid    REG_SZ    1b2c3d4e-0000-4000-8000-123456789abc\r\n\r\n";
        assert_eq!(reg_query_value(win, "MachineGuid").as_deref(), Some("1b2c3d4e-0000-4000-8000-123456789abc"));
        assert_eq!(reg_query_value(win, "ProductId"), None);
        assert_eq!(etc_machine_id("4b1c0e2a9f3d4c5b8a7e6f5d4c3b2a19\n").as_deref(), Some("4b1c0e2a9f3d4c5b8a7e6f5d4c3b2a19"));
        assert_eq!(etc_machine_id("uninitialized\n"), None, "an image's, alike in every container of it");
        assert_eq!(etc_machine_id("\n"), None);
        // What leaves the machine is a hash: the same for the same id in any case, never the id.
        let id = machine_id_of("0A1B2C3D-4E5F-6A7B-8C9D-0E1F2A3B4C5D");
        assert_eq!(id, machine_id_of(" 0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d\n"));
        assert_ne!(id, machine_id_of("1b2c3d4e-0000-4000-8000-123456789abc"));
        assert!(id.as_deref().is_some_and(|h| h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()) && !h.contains("0a1b2c3d")), "{id:?}");
        assert_eq!(machine_id_of(" \n"), None);
        // Read once: it holds still for the process's life.
        assert_eq!(machine_id(), machine_id());
    }

    #[test]
    fn a_containers_machine_is_the_one_kept_with_its_data() -> anyhow::Result<()> {
        // A pod's: made from its UID, and kept.
        let volume = tempfile::tempdir()?;
        let pod = kept_machine_id(volume.path(), Some("8f2b4c1e-1d2a-4f3b-9c8d-7e6f5a4b3c2d"));
        assert_eq!(pod, machine_id_of("8f2b4c1e-1d2a-4f3b-9c8d-7e6f5a4b3c2d"));
        // Its pod made again with the same volume (a StatefulSet's): the same machine.
        assert_eq!(kept_machine_id(volume.path(), Some("a1b2c3d4-0000-4000-8000-000000000001")), pod);
        // A container that isn't a pod: made once, at random, and kept.
        let other = tempfile::tempdir()?;
        let made = kept_machine_id(other.path(), None);
        assert!(made.is_some() && made != pod);
        assert_eq!(kept_machine_id(other.path(), None), made);
        // What isn't an id kept there is made again.
        std::fs::write(other.path().join("machine-id"), "uninitialized\n")?;
        let again = kept_machine_id(other.path(), None);
        assert!(again.is_some() && again != made, "{again:?}");
        Ok(())
    }

    #[test]
    fn finds_programs_on_path() {
        let sh = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(have(sh), "{sh} not on PATH");
        assert!(!have("kiln-surely-not-a-real-program"));
        assert_eq!(program_stem("/usr/bin/bash"), "bash");
        assert_eq!(program_stem(r"C:\Program Files\PowerShell\7\pwsh.exe"), "pwsh");
        assert_eq!(program_stem("CMD.EXE"), "cmd");
    }

    #[test]
    fn a_replaced_binary_s_path_names_the_new_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let kiln = dir.path().join("kiln");
        std::fs::write(&kiln, b"new")?;
        let gone = |p: &Path| PathBuf::from(format!("{} (deleted)", p.display()));
        assert_eq!(replaced(&gone(&kiln)), Some(kiln.clone()));
        assert_eq!(replaced(&kiln), None, "not replaced");
        assert_eq!(replaced(&gone(&dir.path().join("uninstalled"))), None, "nothing in its place");
        assert!(this_kiln()?.is_file());
        Ok(())
    }

    /// As an upgrade does it: a running binary's path given a new file, the process names the new
    /// one (its own `/proc/self/exe` reads `<path> (deleted)`). The running binary is this test's,
    /// linked into a folder of its own (a copy would be gigabytes).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_binary_replaced_on_disk_names_the_one_in_its_place() -> anyhow::Result<()> {
        const GO: &str = "KILN_TEST_REPLACED_GO";
        // The run in the replaced binary: once told, what it names.
        if let Some(go) = std::env::var_os(GO) {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !Path::new(&go).exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("exe={}", std::env::current_exe()?.display());
            println!("this_kiln={}", this_kiln()?.display());
            return Ok(());
        }
        let me = std::env::current_exe()?;
        let dir = tempfile::tempdir_in(me.parent().unwrap_or(Path::new(".")))?;
        let bin = dir.path().join("kiln");
        std::fs::hard_link(&me, &bin)?;
        let go = dir.path().join("go");
        let child = std::process::Command::new(&bin)
            .args(["--exact", "os::tests::a_running_binary_replaced_on_disk_names_the_one_in_its_place", "--nocapture", "--test-threads=1"])
            .env(GO, &go)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let new = dir.path().join("kiln.new");
        std::fs::write(&new, b"the next kiln")?;
        std::fs::rename(&new, &bin)?;
        std::fs::write(&go, b"")?;
        let said = String::from_utf8_lossy(&child.wait_with_output()?.stdout).into_owned();
        println!("{said}");
        assert!(said.contains(&format!("exe={} (deleted)\n", bin.display())), "the replaced binary's own path: {said}");
        assert!(said.contains(&format!("this_kiln={}\n", bin.display())), "{said}");
        Ok(())
    }

    #[test]
    fn a_windows_program_is_one_cmd_would_start() -> anyhow::Result<()> {
        let pathext = Some(".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC");
        assert_eq!(exe_names("codex", pathext), ["codex.com", "codex.exe", "codex.bat", "codex.cmd"]);
        assert_eq!(exe_names("codex.CMD", pathext), ["codex.CMD"]);
        assert_eq!(exe_names("codex", None), ["codex"]);
        // What `npm install -g @openai/codex` leaves in Node's folder: a shell script for Git Bash,
        // a batch file for cmd, a script for PowerShell.
        let npm = tempfile::tempdir()?;
        for f in ["codex", "codex.cmd", "codex.ps1"] {
            std::fs::write(npm.path().join(f), "")?;
        }
        let path = std::env::join_paths([npm.path()])?;
        assert_eq!(find_in("codex", &path, pathext), Some(npm.path().join("codex.cmd")));
        assert_eq!(find_in("codex.cmd", &path, pathext), Some(npm.path().join("codex.cmd")));
        assert_eq!(find_in("codex", &path, None), Some(npm.path().join("codex")), "Unix starts the script");
        // A file Windows opens rather than starts is no program, even first on PATH.
        let (scripts, node) = (tempfile::tempdir()?, tempfile::tempdir()?);
        std::fs::write(scripts.path().join("node.js"), "")?;
        std::fs::write(scripts.path().join("gemini"), "")?;
        std::fs::write(node.path().join("node.exe"), "")?;
        let path = std::env::join_paths([scripts.path(), node.path()])?;
        assert_eq!(find_in("node", &path, pathext), Some(node.path().join("node.exe")));
        assert_eq!(find_in("gemini", &path, pathext), None, "a shell script alone is not installed");
        Ok(())
    }

    #[test]
    fn a_folder_goes_on_a_windows_path_once() {
        let var = |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\me\AppData\Local".to_owned());
        let kiln = r"C:\Users\me\AppData\Local\Programs\kiln";
        assert_eq!(path_list_with("", kiln, var).as_deref(), Some(kiln));
        assert_eq!(path_list_with(r"C:\a;C:\b", kiln, var), Some(format!(r"C:\a;C:\b;{kiln}")));
        assert_eq!(path_list_with(r"C:\a;;C:\b;", kiln, var), Some(format!(r"C:\a;;C:\b;{kiln}")), "the user's list kept as it is");
        // Already there: another case, quoted, a trailing backslash, or through a variable.
        assert_eq!(path_list_with(r"C:\a;c:\users\ME\appdata\local\programs\KILN", kiln, var), None);
        assert_eq!(path_list_with(r#"C:\a;"C:\Users\me\AppData\Local\Programs\kiln\""#, kiln, var), None);
        assert_eq!(path_list_with(r"%LOCALAPPDATA%\Programs\kiln;C:\a", kiln, var), None);
        // A variable it can't expand stays as written, and is no match.
        assert!(path_list_with(r"%NOPE%\Programs\kiln", kiln, var).is_some());
        assert_eq!(expand_percent_vars(r"%NOPE%\x;100%;%%", &var), r"%NOPE%\x;100%;%%");
    }

    #[test]
    fn a_folder_comes_off_a_windows_path_as_it_went_on() {
        let var = |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\me\AppData\Local".to_owned());
        let kiln = r"C:\Users\me\AppData\Local\Programs\kiln";
        let list = r"C:\a;C:\b";
        let added = path_list_with(list, kiln, var).unwrap_or_default();
        assert_eq!(path_list_without(&added, kiln, var).as_deref(), Some(list));
        assert_eq!(path_list_without(r"C:\a;%LOCALAPPDATA%\Programs\kiln\;;C:\b", kiln, var).as_deref(), Some(r"C:\a;;C:\b"));
        assert_eq!(path_list_without(list, kiln, var), None, "not there: nothing to change");
    }

    #[test]
    fn shells_get_the_right_flags() {
        assert_eq!(script_args("/bin/bash", "echo hi"), ["-c", "echo hi"]);
        assert_eq!(script_args(r"C:\Windows\System32\cmd.exe", "dir"), ["/D", "/S", "/C", "dir"]);
        let ps = script_args("pwsh", "Write-Output hi");
        assert_eq!(ps.get(..4), Some(&["-NoLogo".to_owned(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into()][..]));
        assert!(ps.last().is_some_and(|s| s.contains("Write-Output hi") && s.contains("exit $LASTEXITCODE")));
        let (prog, args) = default_step_shell("true");
        if cfg!(windows) {
            assert!(prog == "pwsh" || prog == "powershell");
        } else {
            assert!(prog == "bash" || prog == "sh");
            assert_eq!(args.last().map(String::as_str), Some("true"));
        }
    }

    #[test]
    fn positional_io_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f");
        let f = std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path)?;
        write_all_at(&f, b"world", 6)?;
        write_all_at(&f, b"hello ", 0)?;
        let mut buf = [0u8; 11];
        let n = read_at(&f, &mut buf, 0)?;
        assert_eq!(buf.get(..n), Some(&b"hello world"[..]));
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod priority_tests {
    /// A thread lowered runs ten nice levels below the process; the rest keep theirs.
    #[test]
    fn a_lowered_thread_runs_ten_nice_levels_below_the_rest() {
        let me = rustix::thread::gettid();
        let before = rustix::process::getpriority_process(Some(me)).unwrap_or(0);
        let lowered = std::thread::spawn(|| {
            super::lower_thread_priority();
            rustix::process::getpriority_process(Some(rustix::thread::gettid())).unwrap_or(-100)
        })
        .join()
        .unwrap_or(-100);
        assert_eq!(lowered, (before + 10).min(19));
        assert_eq!(rustix::process::getpriority_process(Some(me)).unwrap_or(-100), before);
    }
}
