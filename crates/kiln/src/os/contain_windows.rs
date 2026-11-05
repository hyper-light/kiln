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

/// `JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION`: the basic accounting, then `IO_COUNTERS`
/// (read, write and other operations, then their bytes).
#[repr(C)]
#[derive(Default)]
struct AccountingIo {
    basic: Accounting,
    read_ops: u64,
    write_ops: u64,
    other_ops: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

const _: () = assert!(std::mem::size_of::<AccountingIo>() == 96);

/// `JobObjectBasicAndIoAccountingInformation`.
const JOB_OBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION: i32 = 8;

/// What the processes of job `job` used, all told, those ended included (docs/needs.md): CPU
/// time, and the I/O they asked of the OS (files, pipes and devices alike: Windows counts no
/// storage I/O apart).
fn usage_of(job: &Owned) -> crate::sense::run::Usage {
    let mut info = AccountingIo::default();
    let len = u32::try_from(std::mem::size_of::<AccountingIo>()).unwrap_or(0);
    // SAFETY: the job's handle and a whole JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION of that
    // size.
    if unsafe { QueryInformationJobObject(job.0, JOB_OBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, (&raw mut info).cast(), len, std::ptr::null_mut()) } == 0 {
        return crate::sense::run::Usage::default();
    }
    let cpu_100ns = u64::try_from(info.basic.total_user_time.saturating_add(info.basic.total_kernel_time)).unwrap_or(0);
    crate::sense::run::Usage { cpu_us: Some(cpu_100ns / 10), read_bytes: Some(info.read_bytes), write_bytes: Some(info.write_bytes), ops: Some(info.read_ops.saturating_add(info.write_ops)), ..crate::sense::run::Usage::default() }
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

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
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
    /// What its processes used, all told (docs/needs.md).
    pub fn usage(&self) -> crate::sense::run::Usage {
        usage_of(&self.job)
    }

    /// Agent session `session`'s job: the one it has (it outlives the harness), else a new one.
    pub fn of(session: &str) -> Option<Group> {
        if !holds() {
            return None;
        }
        if let Some(g) = Group::existing(session) {
            return Some(g);
        }
        let name = name_of(session)?;
        // SAFETY: a NUL-terminated name; no security attributes.
        let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) })?;
        if !ends_with_runner(&job) {
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

    /// Puts process `process` (created suspended, so it has run nothing yet) in the job.
    pub(crate) fn assign(&self, process: Handle) -> bool {
        // SAFETY: the job's handle and a process handle its creator holds.
        unsafe { AssignProcessToJobObject(self.job.0, process) != 0 }
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
fn ends_with_runner(job: &Owned) -> bool {
    let mut limits = ExtendedLimits::default();
    limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = u32::try_from(std::mem::size_of::<ExtendedLimits>()).unwrap_or(0);
    // SAFETY: the job's handle, and a whole JOBOBJECT_EXTENDED_LIMIT_INFORMATION of that size.
    unsafe { SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&raw const limits).cast(), size) != 0 }
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
        ends_with_runner(&job).then(|| Tree { job: Arc::new(job) })
    }

    /// Puts process `process` (created suspended, so it has run nothing yet) in the job.
    pub(crate) fn assign(&self, process: Handle) -> bool {
        // SAFETY: the job's handle and a process handle its creator holds.
        unsafe { AssignProcessToJobObject(self.job.0, process) != 0 }
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

    /// What its processes used, all told (docs/needs.md).
    pub fn usage(&self) -> crate::sense::run::Usage {
        usage_of(&self.job)
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
    let p = *slot.get_or_init(|| {
        let dll: Vec<u16> = "ntdll.dll".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated name; only the system directory is searched.
        let module = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return 0;
        }
        // SAFETY: a loaded module (never freed) and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr()) as usize }
    });
    // SAFETY: ntdll's NtSuspendProcess or NtResumeProcess, whose signature the type spells out.
    (p != 0).then(|| unsafe { std::mem::transmute::<usize, NtProcessFn>(p) })
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
}
