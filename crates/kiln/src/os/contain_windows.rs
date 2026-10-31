//! Each agent session's processes held together by Windows: a job object of its own,
//! `Local\kiln-agent-<session>`. Its harness is created suspended, put in the job, then let run
//! ([`crate::os::proc::spawn_duplex_in`]), so everything it starts is in the job too: a process
//! leaves a job only if the job lets it break away, and these don't. Listing an agent's processes
//! is asking its job; whose a process is, asking each job whether it holds it; ending them all,
//! terminating the job. Like every job kiln makes, it ends with the runner (`KILL_ON_JOB_CLOSE`).

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
};

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_BASIC_PROCESS_ID_LIST: i32 = 3;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
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
    fn CloseHandle(handle: Handle) -> i32;
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
        let name = name_of(session)?;
        // SAFETY: a NUL-terminated name; no security attributes.
        let job = Owned::new(unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) })?;
        let mut limits = ExtendedLimits::default();
        limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(std::mem::size_of::<ExtendedLimits>()).unwrap_or(0);
        // SAFETY: the job's handle, and a whole JOBOBJECT_EXTENDED_LIMIT_INFORMATION of that size.
        if unsafe { SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&raw const limits).cast(), size) } == 0 {
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
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: two counts, then the ids (pointer-sized).
        let mut buf: Vec<usize> = vec![0; 2 + MOST_LISTED];
        let len = u32::try_from(buf.len() * std::mem::size_of::<usize>()).unwrap_or(0);
        let mut returned = 0u32;
        // SAFETY: the job's handle and a buffer of `len` bytes.
        if unsafe { QueryInformationJobObject(self.job.0, JOB_OBJECT_BASIC_PROCESS_ID_LIST, buf.as_mut_ptr().cast(), len, &raw mut returned) } == 0 {
            return Vec::new();
        }
        // The counts are two DWORDs packed into the first pointer-sized slot on 64-bit Windows.
        let header = buf.first().copied().unwrap_or(0) as u64;
        let listed = usize::try_from(if cfg!(target_pointer_width = "64") { header >> 32 } else { buf.get(1).copied().unwrap_or(0) as u64 }).unwrap_or(0);
        let start = if cfg!(target_pointer_width = "64") { 1 } else { 2 };
        buf.iter().skip(start).take(listed.min(MOST_LISTED)).filter_map(|p| u32::try_from(*p).ok()).collect()
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

    /// CI jobs get no group of their own here ([`job_group`]).
    pub fn release(&self) {}
}

/// A CI job's group of processes: Linux's alone (`crate::os::contain` there), where a kiln a step
/// starts would otherwise take the runner's own cgroup as its.
pub fn job_group(_job: &str) -> Option<Group> {
    None
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
