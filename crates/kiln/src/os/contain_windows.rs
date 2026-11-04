//! Each agent session's processes held together by Windows: a job object of its own,
//! `Local\kiln-agent-<session>`. Its harness is created suspended, put in the job, then let run
//! ([`crate::os::proc::spawn_duplex_in`]), so everything it starts is in the job too: a process
//! leaves a job only if the job lets it break away, and these don't. Listing an agent's processes
//! is asking its job; whose a process is, asking each job whether it holds it; ending them all,
//! terminating the job. Like every job kiln makes, it ends with the runner (`KILL_ON_JOB_CLOSE`).
//!
//! A CI step's processes are held the same way, in a job of the step's own ([`Tree`]), so a job
//! paused in Ship is suspended where it runs (docs/ci.md W26-R22): every process in it
//! (`NtSuspendProcess`), and let go on after (`NtResumeProcess`), [`hold`].
//!
//! Priority (docs/priority.md; goal 8, plan 01a1a59c2b29 step 4): an agent's job and each step's
//! run what's in them below normal: `BELOW_NORMAL_PRIORITY_CLASS` for every process in the job
//! (`JOB_OBJECT_LIMIT_PRIORITY_CLASS`), the least weight there is in weight-based CPU rate
//! control, and what background mode gives a process (very low I/O and memory priority) for the
//! process kiln puts in it. The server runs `ABOVE_NORMAL_PRIORITY_CLASS` ([`server_first`]).

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

type Handle = *mut c_void;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct Accounting {
    total_user_time: i64,
    total_kernel_time: i64,
    this_period_total_user_time: i64,
    this_period_total_kernel_time: i64,
    total_page_fault_count: u32,
    total_processes: u32,
    active_processes: u32,
    total_terminated_processes: u32,
}

/// `PROCESSENTRY32W`.
#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    pri_class_base: i32,
    flags: u32,
    exe_file: [u16; 260],
}

/// `THREADENTRY32`.
#[repr(C)]
#[derive(Default)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

/// `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`: its flags, and the rate, weight or bounds they name.
#[repr(C)]
#[derive(Default)]
struct CpuRate {
    control_flags: u32,
    value: u32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<CpuRate>() == 8);
    assert!(size_of::<BasicLimits>() == 64);
    assert!(offset_of!(BasicLimits, limit_flags) == 16);
    assert!(size_of::<ExtendedLimits>() == 144);
    assert!(size_of::<ThreadEntry>() == 28);
    assert!(size_of::<Accounting>() == 48);
    assert!(size_of::<ProcessEntry>() == 568);
    assert!(offset_of!(ProcessEntry, parent_process_id) == 32);
};

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: i32 = 1;
const JOB_OBJECT_BASIC_PROCESS_ID_LIST: i32 = 3;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const JOB_OBJECT_LIMIT_PRIORITY_CLASS: u32 = 0x0000_0020;
const JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION: i32 = 15;
const JOB_OBJECT_CPU_RATE_CONTROL_ENABLE: u32 = 0x1;
const JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED: u32 = 0x2;
/// Weight-based CPU rate control's least share: "a value from 1 through 9, where 1 is the smallest
/// share and 9 is the largest" (`JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`).
const LEAST_WEIGHT: u32 = 1;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
/// `PROCESS_INFORMATION_CLASS`'s `ProcessMemoryPriority`, and its least, `MEMORY_PRIORITY_VERY_LOW`.
const PROCESS_MEMORY_PRIORITY: i32 = 0;
const MEMORY_PRIORITY_VERY_LOW: u32 = 1;
/// ntdll's `ProcessIoPriority` class, and its least hint, `IoPriorityVeryLow`.
const PROCESS_IO_PRIORITY: i32 = 33;
const IO_PRIORITY_VERY_LOW: u32 = 0;
const PROCESS_SET_INFORMATION: u32 = 0x0200;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const PROCESS_SUSPEND_RESUME: u32 = 0x0800;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
/// The most of a job's processes listed at once.
const MOST_LISTED: usize = 4096;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn QueryInformationJobObject(job: Handle, class: i32, info: *mut c_void, len: u32, returned: *mut u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, code: u32) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> Handle;
    fn ResumeThread(thread: Handle) -> u32;
    fn SuspendThread(thread: Handle) -> u32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void;
    fn SetPriorityClass(process: Handle, class: u32) -> i32;
    fn GetPriorityClass(process: Handle) -> u32;
    fn GetCurrentProcess() -> Handle;
    fn SetProcessInformation(process: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn GetProcessInformation(process: Handle, class: i32, info: *mut c_void, len: u32) -> i32;
}

/// A handle kiln owns, closed when dropped.
struct Owned(Handle);

// SAFETY: a kernel handle is a process-wide value; the calls made with it are thread-safe.
unsafe impl Send for Owned {}
// SAFETY: as above.
unsafe impl Sync for Owned {}

impl Owned {
    fn new(h: Handle) -> Option<Owned> {
        // NULL (most failures) and INVALID_HANDLE_VALUE (a snapshot's) are no handle.
        (!h.is_null() && h as isize != -1).then_some(Owned(h))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a handle this owns, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The jobs of this runner's agents, by session: kept open while the runner runs (each ends with
/// it), and asked whose a process is.
fn jobs() -> &'static Mutex<HashMap<String, Arc<Owned>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<Owned>>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
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

/// A session id as part of a job's name: only what an id is made of (kiln's are hex).
fn name_of(session: &str) -> Option<Vec<u16>> {
    let ok = !session.is_empty() && session.len() <= 128 && session.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| format!("Local\\kiln-agent-{session}").encode_utf16().chain(std::iter::once(0)).collect())
}

/// An agent session's group of processes: a job object of its own.
#[derive(Clone)]
pub struct Group {
    session: String,
    job: Arc<Owned>,
}

impl std::fmt::Debug for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Group").field("session", &self.session).finish_non_exhaustive()
    }
}

impl Group {
    /// Agent session `session`'s job: the one it has (it outlives the harness), else a new one.
    pub fn of(session: &str) -> Option<Group> {
        if !holds() {
            return None;
        }
        if let Some(g) = Group::existing(session) {
            return Some(g);
        }
        Self::made(session, true)
    }

    /// A job `name` for work a person asked for (their own reach run): held together as an
    /// agent's is, at the priority of what the person runs rather than below normal.
    pub fn of_person(name: &str) -> Option<Group> {
        if !holds() {
            return None;
        }
        if let Some(g) = Group::existing(name) {
            return Some(g);
        }
        Self::made(name, false)
    }

    fn made(session: &str, background: bool) -> Option<Group> {
        let name = name_of(session)?;
        // SAFETY: a NUL-terminated name; no security attributes.
        let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) })?;
        if !made_for(&job, background) {
            tracing::warn!(session, "an agent's job couldn't be set to end with the runner: its processes have no group");
            return None;
        }
        let job = Arc::new(job);
        jobs().lock().insert(session.to_owned(), Arc::clone(&job));
        Some(Group { session: session.to_owned(), job })
    }

    /// The job `session` has already, without making one.
    pub fn existing(session: &str) -> Option<Group> {
        let job = jobs().lock().get(session).cloned()?;
        Some(Group { session: session.to_owned(), job })
    }

    /// Puts process `process` (created suspended, so it has run nothing yet) in the job, given what
    /// background mode gives a process where the job runs below normal.
    pub(crate) fn assign(&self, process: Handle) -> bool {
        // SAFETY: the job's handle and a process handle its creator holds.
        let assigned = unsafe { AssignProcessToJobObject(self.job.0, process) != 0 };
        if assigned && below_normal(&self.job) {
            background_process(process);
        }
        assigned
    }

    /// Every process in it now.
    pub fn pids(&self) -> Vec<u32> {
        pids_in(&self.job)
    }

    /// Whether `pid` is in it.
    fn holds(&self, process: &Owned) -> bool {
        let mut inside = 0i32;
        // SAFETY: a process handle opened for querying, and the job's handle.
        unsafe { IsProcessInJob(process.0, self.job.0, &raw mut inside) != 0 && inside != 0 }
    }

    /// Ends every process in it at once.
    pub fn kill(&self) {
        // SAFETY: the job's handle.
        unsafe { TerminateJobObject(self.job.0, 1) };
    }

    /// Lets go of it: once kiln's handle closes, nothing holds it and it goes (taking what's still
    /// in it, as every job kiln makes does).
    pub fn remove(&self) {
        jobs().lock().remove(&self.session);
    }
}

/// Sets job `job` to end what's in it once its last handle closes: with the runner, at the latest.
/// A `background` job (a leg's or an agent's) also runs every process in it below normal, at the
/// least weight weight-based CPU rate control gives (docs/priority.md): false only when it can't end
/// with the runner, the one thing the job is for.
fn made_for(job: &Owned, background: bool) -> bool {
    let mut limits = ExtendedLimits::default();
    limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if background {
        limits.basic.limit_flags |= JOB_OBJECT_LIMIT_PRIORITY_CLASS;
        limits.basic.priority_class = BELOW_NORMAL_PRIORITY_CLASS;
    }
    if !set_limits(job, &limits) {
        if !background {
            return false;
        }
        tracing::warn!("a leg's or an agent's job can't run its processes below normal here: they run at normal priority");
        limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        limits.basic.priority_class = 0;
        return set_limits(job, &limits);
    }
    if background {
        let rate = CpuRate { control_flags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED, value: LEAST_WEIGHT };
        let size = u32::try_from(std::mem::size_of::<CpuRate>()).unwrap_or(0);
        // SAFETY: the job's handle, and a whole JOBOBJECT_CPU_RATE_CONTROL_INFORMATION of that size.
        if unsafe { SetInformationJobObject(job.0, JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION, (&raw const rate).cast(), size) } == 0 {
            tracing::info!(error = %std::io::Error::last_os_error(), "a leg's or an agent's job has no CPU weight here: its priority alone ranks it");
        }
    }
    true
}

fn set_limits(job: &Owned, limits: &ExtendedLimits) -> bool {
    let size = u32::try_from(std::mem::size_of::<ExtendedLimits>()).unwrap_or(0);
    // SAFETY: the job's handle, and a whole JOBOBJECT_EXTENDED_LIMIT_INFORMATION of that size.
    unsafe { SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, std::ptr::from_ref(limits).cast(), size) != 0 }
}

/// Job `job`'s limits as Windows holds them.
fn limits_of(job: &Owned) -> Option<ExtendedLimits> {
    let mut limits = ExtendedLimits::default();
    let size = u32::try_from(std::mem::size_of::<ExtendedLimits>()).unwrap_or(0);
    // SAFETY: the job's handle, and room for a whole JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
    (unsafe { QueryInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&raw mut limits).cast(), size, std::ptr::null_mut()) } != 0).then_some(limits)
}

/// Job `job`'s CPU rate control as Windows holds it: its flags and its weight.
#[cfg(test)]
fn rate_of(job: &Owned) -> Option<(u32, u32)> {
    let mut rate = CpuRate::default();
    let size = u32::try_from(std::mem::size_of::<CpuRate>()).unwrap_or(0);
    // SAFETY: the job's handle, and room for a whole JOBOBJECT_CPU_RATE_CONTROL_INFORMATION.
    (unsafe { QueryInformationJobObject(job.0, JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION, (&raw mut rate).cast(), size, std::ptr::null_mut()) } != 0).then_some((rate.control_flags, rate.value))
}

/// Whether job `job` runs its processes below normal (a leg's or an agent's).
fn below_normal(job: &Owned) -> bool {
    limits_of(job).is_some_and(|l| l.basic.limit_flags & JOB_OBJECT_LIMIT_PRIORITY_CLASS != 0 && l.basic.priority_class == BELOW_NORMAL_PRIORITY_CLASS)
}

/// `NTSTATUS NtSetInformationProcess(HANDLE, PROCESSINFOCLASS, PVOID, ULONG)`, and its query.
type NtProcessInfoFn = unsafe extern "system" fn(Handle, i32, *mut c_void, u32) -> i32;

/// Process `process` (a handle that may set its information) given what background mode gives a
/// process (`PROCESS_MODE_BACKGROUND_BEGIN`, which a process can only enter by itself): very low
/// memory priority (`SetProcessInformation`'s `ProcessMemoryPriority`) and very low I/O priority
/// (ntdll's `ProcessIoPriority`); its CPU priority is its job's. Each is best-effort: a process
/// Windows won't lower runs as it would have.
fn background_process(process: Handle) {
    let memory = MEMORY_PRIORITY_VERY_LOW;
    let size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(0);
    // SAFETY: a process handle its creator holds, and a whole MEMORY_PRIORITY_INFORMATION.
    unsafe { SetProcessInformation(process, PROCESS_MEMORY_PRIORITY, (&raw const memory).cast(), size) };
    static SET: OnceLock<usize> = OnceLock::new();
    let at = nt_addr(c"NtSetInformationProcess", &SET);
    if at != 0 {
        let mut io = IO_PRIORITY_VERY_LOW;
        // SAFETY: ntdll's NtSetInformationProcess, whose signature the type spells out; a process
        // handle its creator holds and a whole IO_PRIORITY_HINT.
        unsafe { std::mem::transmute::<usize, NtProcessInfoFn>(at)(process, PROCESS_IO_PRIORITY, (&raw mut io).cast(), size) };
    }
}

/// What background mode's priorities process `process` has: its memory priority and its I/O
/// priority, as Windows says (tests check them).
#[cfg(test)]
fn background_of(process: Handle) -> (Option<u32>, Option<u32>) {
    let size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(0);
    let mut memory = u32::MAX;
    // SAFETY: a process handle opened for querying, and room for a MEMORY_PRIORITY_INFORMATION.
    let memory = (unsafe { GetProcessInformation(process, PROCESS_MEMORY_PRIORITY, (&raw mut memory).cast(), size) } != 0).then_some(memory);
    static QUERY: OnceLock<usize> = OnceLock::new();
    let at = nt_addr(c"NtQueryInformationProcess", &QUERY);
    let mut io = u32::MAX;
    // SAFETY: ntdll's NtQueryInformationProcess (its fifth argument, the length returned, may be
    // NULL), a process handle opened for querying and room for an IO_PRIORITY_HINT.
    let io = (at != 0 && unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(Handle, i32, *mut c_void, u32, *mut u32) -> i32>(at)(process, PROCESS_IO_PRIORITY, (&raw mut io).cast(), size, std::ptr::null_mut()) } >= 0).then_some(io);
    (memory, io)
}

/// Process `pid`, started outside any job (a CI step on a ConPTY, which starts it), below normal
/// and with what background mode gives a process (docs/priority.md): whether it was. What it starts
/// from then on inherits below normal (`CreateProcess`: a child of a below-normal process is).
pub fn background_pid(pid: u32) -> bool {
    // SAFETY: opening a process to set its priority and information.
    let Some(process) = Owned::new(unsafe { OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }) else { return false };
    // SAFETY: a process handle opened for that.
    let lowered = unsafe { SetPriorityClass(process.0, BELOW_NORMAL_PRIORITY_CLASS) } != 0;
    background_process(process.0);
    lowered
}

/// The server first (docs/priority.md): this process runs `ABOVE_NORMAL_PRIORITY_CLASS`, above
/// what the machine runs at normal priority and above kiln's own legs and agents (below normal).
/// Why not, when Windows refuses.
pub fn server_first() -> Result<(), String> {
    // SAFETY: the current process's pseudo-handle, which needs no closing.
    if unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

/// How legs and agents run here (docs/priority.md), for the runner's facts and its log.
pub fn priority() -> String {
    "below normal: each leg's and agent's job runs its processes at BELOW_NORMAL, at the least CPU weight among jobs, with very low I/O and memory priority".to_owned()
}

/// Every process in job `job` now.
fn pids_in(job: &Owned) -> Vec<u32> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST: two counts, then the ids (pointer-sized).
    let mut buf: Vec<usize> = vec![0; 2 + MOST_LISTED];
    let len = u32::try_from(buf.len() * std::mem::size_of::<usize>()).unwrap_or(0);
    let mut returned = 0u32;
    // SAFETY: the job's handle and a buffer of `len` bytes.
    if unsafe { QueryInformationJobObject(job.0, JOB_OBJECT_BASIC_PROCESS_ID_LIST, buf.as_mut_ptr().cast(), len, &raw mut returned) } == 0 {
        return Vec::new();
    }
    // The counts are two DWORDs packed into the first pointer-sized slot on 64-bit Windows.
    let header = buf.first().copied().unwrap_or(0) as u64;
    let listed = usize::try_from(if cfg!(target_pointer_width = "64") { header >> 32 } else { buf.get(1).copied().unwrap_or(0) as u64 }).unwrap_or(0);
    let start = if cfg!(target_pointer_width = "64") { 1 } else { 2 };
    buf.iter().skip(start).take(listed.min(MOST_LISTED)).filter_map(|p| u32::try_from(*p).ok()).collect()
}

/// A CI step's processes held together (docs/ci.md W26-R22): a job of the step's own, which it's put
/// in before it runs anything (created suspended), so whatever it starts is in it however that's
/// parented. Its processes are listed, ended at once, and told how much CPU they used; like every
/// job kiln makes, it ends what's left in it with its last handle, the runner's at the latest.
#[derive(Clone)]
pub struct Tree {
    job: Arc<Owned>,
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree").finish_non_exhaustive()
    }
}

impl Tree {
    /// A new job of a step's own.
    pub fn new() -> Option<Tree> {
        // SAFETY: no name, no security attributes.
        let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
        made_for(&job, true).then(|| Tree { job: Arc::new(job) })
    }

    /// Puts process `process` (created suspended, so it has run nothing yet) in the job, given what
    /// background mode gives a process where the job runs below normal.
    pub(crate) fn assign(&self, process: Handle) -> bool {
        // SAFETY: the job's handle and a process handle its creator holds.
        let assigned = unsafe { AssignProcessToJobObject(self.job.0, process) != 0 };
        if assigned && below_normal(&self.job) {
            background_process(process);
        }
        assigned
    }

    /// Every process in it now.
    pub fn pids(&self) -> Vec<u32> {
        pids_in(&self.job)
    }

    /// Ends every process in it at once.
    pub fn kill(&self) {
        // SAFETY: the job's handle.
        unsafe { TerminateJobObject(self.job.0, 1) };
    }

    /// The CPU time its processes have used, all told, those ended included, in 100 ns units.
    pub fn cpu(&self) -> u64 {
        let mut info = Accounting::default();
        let len = u32::try_from(std::mem::size_of::<Accounting>()).unwrap_or(0);
        // SAFETY: the job's handle and a whole JOBOBJECT_BASIC_ACCOUNTING_INFORMATION of that size.
        if unsafe { QueryInformationJobObject(self.job.0, JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION, (&raw mut info).cast(), len, std::ptr::null_mut()) } == 0 {
            return 0;
        }
        u64::try_from(info.total_user_time.saturating_add(info.total_kernel_time)).unwrap_or(0)
    }
}

/// `NTSTATUS NtSuspendProcess(HANDLE)` and `NtResumeProcess`.
type NtProcessFn = unsafe extern "system" fn(Handle) -> i32;

/// ntdll's `name`, looked up once into `slot`: `NtSuspendProcess` and `NtResumeProcess`, which
/// every Windows kiln runs on has (undocumented, unchanged since XP); every process has ntdll.
fn nt(name: &std::ffi::CStr, slot: &'static OnceLock<usize>) -> Option<NtProcessFn> {
    let p = nt_addr(name, slot);
    // SAFETY: ntdll's NtSuspendProcess or NtResumeProcess, whose signature the type spells out.
    (p != 0).then(|| unsafe { std::mem::transmute::<usize, NtProcessFn>(p) })
}

/// Where ntdll's `name` is, looked up once into `slot`; 0 when it isn't there.
fn nt_addr(name: &std::ffi::CStr, slot: &'static OnceLock<usize>) -> usize {
    *slot.get_or_init(|| {
        let dll: Vec<u16> = "ntdll.dll".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated name; only the system directory is searched.
        let module = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return 0;
        }
        // SAFETY: a loaded module (never freed) and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr()) as usize }
    })
}

/// Process `pid` suspended where it is (`suspend`: `NtSuspendProcess`, each of its threads, counted,
/// so one let-go undoes one suspension) or let go on once (`NtResumeProcess`); whether it was
/// reached: one that has ended isn't.
pub(crate) fn hold(pid: u32, suspend: bool) -> bool {
    static SUSPEND: OnceLock<usize> = OnceLock::new();
    static RESUME: OnceLock<usize> = OnceLock::new();
    let Some(f) = (if suspend { nt(c"NtSuspendProcess", &SUSPEND) } else { nt(c"NtResumeProcess", &RESUME) }) else { return false };
    // SAFETY: opening a process to suspend or resume it.
    let Some(process) = Owned::new(unsafe { OpenProcess(PROCESS_SUSPEND_RESUME, 0, pid) }) else { return false };
    // SAFETY: a process handle opened for that; an NTSTATUS of 0 or more is success.
    unsafe { f(process.0) >= 0 }
}

/// Process `root` and every process under it now, by each one's parent (one snapshot of the
/// system's processes): what a step on a pty started, which ConPTY starts outside any job. One whose
/// parent ended before it was looked at isn't reached.
pub(crate) fn tree_of(root: u32) -> Vec<u32> {
    let mut all: Vec<(u32, u32)> = Vec::new();
    // SAFETY: a snapshot of the system's processes.
    if let Some(snap) = Owned::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) {
        let size = u32::try_from(std::mem::size_of::<ProcessEntry>()).unwrap_or(0);
        let mut entry = ProcessEntry { size, usage: 0, process_id: 0, default_heap_id: 0, module_id: 0, threads: 0, parent_process_id: 0, pri_class_base: 0, flags: 0, exe_file: [0; 260] };
        // SAFETY: the snapshot's handle and an entry whose size is set.
        let mut more = unsafe { Process32FirstW(snap.0, &raw mut entry) } != 0;
        while more {
            all.push((entry.process_id, entry.parent_process_id));
            // SAFETY: as above.
            more = unsafe { Process32NextW(snap.0, &raw mut entry) } != 0;
        }
    }
    let mut out = vec![root];
    let mut i = 0;
    while let Some(p) = out.get(i).copied() {
        let kids: Vec<u32> = all.iter().filter(|(pid, parent)| *parent == p && *pid != p && !out.contains(pid)).map(|(pid, _)| *pid).collect();
        out.extend(kids);
        i += 1;
    }
    out
}

/// Calls `f` on every thread of the processes `pids` (one snapshot of the system's threads), and
/// says which processes had a thread it reached.
fn each_thread(pids: &[u32], f: unsafe extern "system" fn(Handle) -> u32) -> Vec<u32> {
    let mut reached = Vec::new();
    // SAFETY: a snapshot of the system's threads; no process id is needed for threads.
    let Some(snap) = Owned::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else { return reached };
    let mut entry = ThreadEntry { size: u32::try_from(std::mem::size_of::<ThreadEntry>()).unwrap_or(0), ..ThreadEntry::default() };
    // SAFETY: the snapshot's handle and an entry whose size is set.
    let mut more = unsafe { Thread32First(snap.0, &raw mut entry) } != 0;
    while more {
        if pids.contains(&entry.owner_process_id) {
            // SAFETY: a thread id the snapshot listed.
            if let Some(t) = Owned::new(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) }) {
                // SAFETY: a thread handle opened to suspend and resume it.
                if unsafe { f(t.0) } != u32::MAX && !reached.contains(&entry.owner_process_id) {
                    reached.push(entry.owner_process_id);
                }
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap.0, &raw mut entry) } != 0;
    }
    reached
}

/// Lets the threads of process `pid` (created suspended) run.
pub(crate) fn resume(pid: u32) {
    each_thread(&[pid], ResumeThread);
}

/// Stops every thread of the processes `pids` (each once: a thread's suspensions count, so each is
/// undone by one [`resume_all`]), and says which it stopped.
pub(crate) fn suspend_all(pids: &[u32]) -> Vec<u32> {
    each_thread(pids, SuspendThread)
}

/// Undoes one [`suspend_all`] of the processes `pids`.
pub(crate) fn resume_all(pids: &[u32]) {
    each_thread(pids, ResumeThread);
}

/// The sessions with a job now.
pub fn sessions() -> Vec<String> {
    jobs().lock().keys().cloned().collect()
}

/// The agent session process `pid` belongs to by its job, when it's in one of this runner's.
pub fn session_of(pid: u32) -> Option<String> {
    // SAFETY: opening a process for querying only.
    let process = Owned::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
    let all: Vec<(String, Arc<Owned>)> = jobs().lock().iter().map(|(s, j)| (s.clone(), Arc::clone(j))).collect();
    all.into_iter().find(|(s, job)| Group { session: s.clone(), job: Arc::clone(job) }.holds(&process)).map(|(s, _)| s)
}

/// How this machine holds an agent's processes together.
pub fn how() -> String {
    "a job object per agent".to_owned()
}

/// A job made, a suspended process put in it and let run: it and what it starts are listed, and
/// ending the job ends them all.
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::process::CommandExt as _;

    #[test]
    fn what_an_agent_starts_stays_in_its_job() -> anyhow::Result<()> {
        let session = format!("test{}", std::process::id());
        hold_agents();
        let group = Group::of(&session).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start /B ping -n 30 127.0.0.1 >NUL & ping -n 30 127.0.0.1 >NUL"]).creation_flags(0x0000_0004);
        let mut child = cmd.spawn()?;
        anyhow::ensure!(group.assign(child.as_raw_handle()), "a suspended process goes in its job");
        resume(child.id());
        let mut pids = Vec::new();
        for _ in 0..50 {
            pids = group.pids();
            if pids.len() >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mine = session_of(child.id());
        group.kill();
        let _ = child.wait();
        let after = group.pids();
        group.remove();
        anyhow::ensure!(pids.len() >= 3, "the shell and both pings: {pids:?}");
        anyhow::ensure!(mine.as_deref() == Some(session.as_str()), "whose it is, by its job: {mine:?}");
        anyhow::ensure!(after.is_empty(), "ending the job ends all of it: {after:?}");
        anyhow::ensure!(session_of(std::process::id()).is_none(), "the runner isn't an agent's");
        Ok(())
    }

    /// The priority class process `pid` runs at, as Windows says.
    fn class_of(pid: u32) -> Option<u32> {
        // SAFETY: opening a process for querying only.
        let process = Owned::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        // SAFETY: a process handle opened for querying.
        Some(unsafe { GetPriorityClass(process.0) })
    }

    /// Goal 8 (plan 01a1a59c2b29 step 4, docs/priority.md): an agent's job runs everything in it
    /// below normal, what the agent starts too, at the least weight of weight-based CPU rate
    /// control, and the process kiln put in it has background mode's very low memory and I/O
    /// priority; a person's job runs what's in it as the person would.
    #[test]
    fn g8_s4_an_agents_job_runs_below_normal_at_the_least_weight() -> anyhow::Result<()> {
        let session = format!("g8agent{}", std::process::id());
        hold_agents();
        let group = Group::of(&session).ok_or_else(|| anyhow::anyhow!("no job"))?;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "ping -n 30 127.0.0.1 >NUL"]).creation_flags(0x0000_0004);
        let mut child = cmd.spawn()?;
        anyhow::ensure!(group.assign(child.as_raw_handle()), "a suspended process goes in its job");
        resume(child.id());
        let mut pids = Vec::new();
        for _ in 0..50 {
            pids = group.pids();
            if pids.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let limits = limits_of(&group.job).map(|l| (l.basic.limit_flags & JOB_OBJECT_LIMIT_PRIORITY_CLASS != 0, l.basic.priority_class));
        let rate = rate_of(&group.job);
        let classes: Vec<Option<u32>> = pids.iter().map(|p| class_of(*p)).collect();
        let (memory, io) = background_of(child.as_raw_handle());
        group.kill();
        let _ = child.wait();
        group.remove();
        let person = Group::of_person(&format!("g8person{}", std::process::id())).ok_or_else(|| anyhow::anyhow!("no job for a person"))?;
        let persons = limits_of(&person.job).map(|l| l.basic.limit_flags & JOB_OBJECT_LIMIT_PRIORITY_CLASS != 0);
        person.remove();
        anyhow::ensure!(limits == Some((true, BELOW_NORMAL_PRIORITY_CLASS)), "the job's priority class: {limits:?}");
        anyhow::ensure!(rate == Some((JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED, LEAST_WEIGHT)), "the job's CPU rate control: {rate:?}");
        anyhow::ensure!(pids.len() >= 2 && classes.iter().all(|c| *c == Some(BELOW_NORMAL_PRIORITY_CLASS)), "cmd and its ping, each below normal: {pids:?} {classes:?}");
        anyhow::ensure!(memory == Some(MEMORY_PRIORITY_VERY_LOW) && io == Some(IO_PRIORITY_VERY_LOW), "background mode's memory and I/O priority: {memory:?} {io:?}");
        anyhow::ensure!(persons == Some(false), "a person's job sets no priority class: {persons:?}");
        Ok(())
    }

    /// Goal 8 (plan 01a1a59c2b29 step 4): the server runs above normal.
    #[test]
    fn g8_s4_the_server_runs_above_normal() -> anyhow::Result<()> {
        server_first().map_err(anyhow::Error::msg)?;
        let class = class_of(std::process::id());
        // The test process goes back to normal: its other tests run beside this one.
        // SAFETY: the current process's pseudo-handle.
        unsafe { SetPriorityClass(GetCurrentProcess(), 0x0000_0020) };
        anyhow::ensure!(class == Some(ABOVE_NORMAL_PRIORITY_CLASS), "the server's priority class: {class:?}");
        Ok(())
    }
}
