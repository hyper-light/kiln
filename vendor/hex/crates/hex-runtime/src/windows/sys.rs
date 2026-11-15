//! The Windows calls the `process` class makes that Rust can't make safely (CLAUDE.md §12 names this
//! file): `windows-sys` binds them only as `unsafe` foreign functions, and std's own job-object and
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`/`_HANDLE_LIST` process-creation support is unstable. Each `unsafe`
//! block states the invariant that makes it sound. A box is a job object: this file creates one, sets
//! its limits, starts a member suspended inside it (`PROC_THREAD_ATTRIBUTE_JOB_LIST`, so nothing runs
//! outside it) and resumes it, reads the job's accounting, and ends the job. It also counts the handles
//! the process holds (`GetProcessHandleCount`): what a node holds at its start beside its bounds as it
//! fits them (`hex_node::serve::fit`). Names, structures and flags are Windows' own (`windows-sys`;
//! docs/research/runtime/windows-job-objects.md).

// The Windows class's one place `unsafe` is allowed (CLAUDE.md §12); every block below carries its
// SAFETY comment.
#![allow(unsafe_code)]

use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{
    AsHandle, AsRawHandle, AsRawSocket, BorrowedHandle, BorrowedSocket, OwnedHandle,
};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS,
    DuplicateHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA,
    ERROR_NOT_ENOUGH_QUOTA, FILETIME, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    INVALID_SOCKET, POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, SOCKET, SOCKET_ERROR, WSAEINTR,
    WSAGetLastError, WSAPOLLFD, WSAPoll,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAndIoAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW,
    CreateProcessW, CreateWaitableTimerExW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetActiveProcessorCount, GetCurrentProcess, GetCurrentThread,
    GetExitCodeProcess, GetProcessHandleCount, GetProcessTimes, GetSystemTimes,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_DUP_HANDLE,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResetEvent,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, SetEvent, SetPriorityClass,
    SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_TIME_CRITICAL, TIMER_ALL_ACCESS,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
};

use hex_core::job::Cpu;

/// Windows' active processor count across every processor group (not one group's 64-at-most), the
/// denominator the CPU hard cap maps against ([`super::job`]).
pub fn active_processors() -> u32 {
    // SAFETY: GetActiveProcessorCount reads nothing and returns a count; ALL_PROCESSOR_GROUPS is the
    // documented argument for every group. A 0 return (no information) is treated as one processor.
    let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    n.max(1)
}

/// This process's memory budget and the system's physical-memory facts.
fn memory_status() -> io::Result<windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = zeroed_struct();
    m.dwLength = u32::try_from(size_of::<MEMORYSTATUSEX>())
        .map_err(|_| io::Error::other("MEMORYSTATUSEX's size past u32"))?;
    // SAFETY: m names its actual size and outlives the call, which writes only that structure.
    if unsafe { GlobalMemoryStatusEx(&raw mut m) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(m)
}

/// The machine's physical memory and how much is available: the node's facts.
pub fn memory() -> io::Result<(u64, u64)> {
    let m = memory_status()?;
    Ok((m.ullTotalPhys, m.ullAvailPhys))
}

/// Bytes this process can still commit (MEMORYSTATUSEX::ullAvailPageFile), read at node startup.
pub fn commit_available() -> io::Result<u64> {
    memory_status().map(|m| m.ullAvailPageFile)
}

/// The calling thread's stack extent, as Windows reports it; the node measures a default worker.
pub fn thread_stack_bytes() -> io::Result<usize> {
    use windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits;
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: both outputs name writable usize values that outlive the call.
    unsafe { GetCurrentThreadStackLimits(&raw mut low, &raw mut high) };
    high.checked_sub(low)
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| io::Error::other("Windows reported no thread stack extent"))
}

/// How many handles this process holds now: `GetProcessHandleCount` of its own pseudo-handle
/// (learn.microsoft.com, "GetProcessHandleCount function"), what a node held at its start beside its
/// bounds as it fits them (`hex_node::serve::fit`), as Linux's node reads `/proc/self/fd`.
pub fn open_handles() -> io::Result<u64> {
    let mut count: u32 = 0;
    // SAFETY: GetCurrentProcess returns this process's pseudo-handle, which needs no closing;
    // GetProcessHandleCount writes one u32 to `count`, a local that outlives the call.
    let held = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    if held == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(count))
}

/// The enclosing job's declared process ceiling, if it sets one. Kernel allocation failures remain
/// authoritative when the current job has none, or another enclosing job is tighter.
pub fn enclosing_process_limit() -> io::Result<Option<usize>> {
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOBOBJECT_BASIC_LIMIT_INFORMATION, JobObjectBasicLimitInformation,
    };
    let mut inside = 0;
    // SAFETY: GetCurrentProcess is a pseudo-handle, NULL asks about any job, and inside outlives the call.
    if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &raw mut inside) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if inside == 0 {
        return Ok(None);
    }
    let mut info: JOBOBJECT_BASIC_LIMIT_INFORMATION = zeroed_struct();
    let bytes = u32::try_from(size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>())
        .map_err(|_| io::Error::other("job limit information's size past u32"))?;
    // SAFETY: NULL selects this process's immediate job. info is correctly typed, sized and alive.
    if unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectBasicLimitInformation,
            (&raw mut info).cast(),
            bytes,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.LimitFlags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS == 0 {
        Ok(None)
    } else {
        usize::try_from(info.ActiveProcessLimit)
            .map(Some)
            .map_err(|_| io::Error::other("job's active-process ceiling past usize"))
    }
}

/// The size of the volume `dir` is on and the bytes this user may still write there
/// (`GetDiskFreeSpaceExW`): the node's facts.
pub fn disk_space(dir: &std::path::Path) -> io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut caller, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; the three outputs are u64s
    // that outlive it, each written once.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut caller,
            &raw mut total,
            &raw mut free,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((total, caller))
}

/// The users folder (`FOLDERID_UserProfiles`, `%SystemDrive%\\Users` unless moved), read from its known
/// folder as Windows resolves it, never a literal: the node guest's default share
/// (docs/design/shards-guest.md §4a).
pub fn user_profiles() -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_UserProfiles, SHGetKnownFolderPath};
    let mut path: windows_sys::core::PWSTR = null_mut();
    // SAFETY: FOLDERID_UserProfiles is a static GUID read for the call's length; flags 0 and a null
    // token ask for the folder as the calling user sees it; `path` is a valid place for the callee to
    // store a CoTaskMemAlloc'd, NUL-terminated wide string, which it does on success and may on
    // failure, so it is freed below either way.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_UserProfiles, 0, null_mut(), &mut path) };
    let read = if hr >= 0 && !path.is_null() {
        let mut len = 0usize;
        // SAFETY: on success `path` is NUL-terminated, so each read up to and at the NUL is within
        // the string the callee allocated; the walk stops at the NUL.
        while unsafe { *path.add(len) } != 0 {
            len = len.saturating_add(1);
        }
        // SAFETY: `path` holds `len` initialised u16s before its NUL (counted above), alive until
        // the CoTaskMemFree below.
        let wide = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
            wide,
        )))
    } else {
        Err(io::Error::from_raw_os_error(hr))
    };
    // SAFETY: `path` is null or the callee's CoTaskMemAlloc'd string, freed once here and not used
    // after; CoTaskMemFree takes null.
    unsafe { CoTaskMemFree(path.cast_const().cast()) };
    read
}

/// The temp folder, as Windows resolves it for this process (`GetTempPath2W`: `TMP`, `TEMP`, the
/// user's profile, then the Windows folder), the node guest's other default share.
pub fn temp_folder() -> io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::GetTempPath2W;
    // MAX_PATH + 1, the most GetTempPath2W writes (its documentation: "The maximum possible return
    // value is MAX_PATH+1 (261)").
    let mut buf = [0u16; 261];
    let size = u32::try_from(buf.len()).unwrap_or(0);
    // SAFETY: `buf` is a writable array of `size` u16s, alive for the call; GetTempPath2W writes at
    // most that many, NUL included, and returns how many it wrote before the NUL, 0 on failure.
    let n = unsafe { GetTempPath2W(size, buf.as_mut_ptr()) };
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    match buf.get(..n) {
        Some(wide) if n > 0 => Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
            wide,
        ))),
        _ => Err(last_error()),
    }
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(
        // SAFETY: GetLastError reads the calling thread's own last-error value and returns it.
        i32::try_from(unsafe { GetLastError() }).unwrap_or(-1),
    )
}

/// Whether `err` is the refusal Windows returns when a `CreateProcess` would exceed a job's
/// active-process limit (`ERROR_NOT_ENOUGH_QUOTA`): the box is full.
pub fn is_process_limit(err: &io::Error) -> bool {
    err.raw_os_error() == i32::try_from(ERROR_NOT_ENOUGH_QUOTA).ok()
}

/// An owned Windows handle, closed when dropped. `HANDLE` is a raw pointer, so this is the one owner;
/// it is only made from a handle this process owns (a job, a completion port, a process or thread this
/// process created). Sendable: a handle is a process-wide value, not tied to a thread.
#[derive(Debug)]
pub struct Handle(HANDLE);

// SAFETY: a Windows HANDLE is valid in any thread of the process; nothing thread-local is held.
unsafe impl Send for Handle {}
// SAFETY: the kernel serializes handle operations; sharing `&Handle` races only in the kernel, which is
// sound. Hex shares one only behind its own lock.
unsafe impl Sync for Handle {}

impl Handle {
    /// The raw handle, for a call that borrows it (it stays owned by `self`).
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// The handle borrowed, for a call that takes a `BorrowedHandle`.
    pub fn borrowed(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        // SAFETY: self owns a valid handle for at least the borrow's life (Handle's invariant).
        unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(self.0) }
    }

    /// Takes ownership of a raw handle this process owns.
    ///
    /// # Safety
    /// `h` must be a valid handle this process owns and no other `Handle` owns.
    pub(crate) unsafe fn own(h: HANDLE) -> Handle {
        Handle(h)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: self owns a valid handle (Handle's invariant); closing it once is sound, and Drop
            // runs once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Only this private duplicate is inheritable. The caller's original writer flags never change.
pub(crate) struct InheritedHandle(Handle);

impl AsHandle for InheritedHandle {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.borrowed()
    }
}

pub(crate) fn inheritable_copy(source: BorrowedHandle<'_>) -> io::Result<InheritedHandle> {
    let mut copied: HANDLE = null_mut();
    // SAFETY: source is borrowed alive for this call; the current-process pseudo handles are
    // documented DuplicateHandle arguments. Only the new same-access copy is made inheritable.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source.as_raw_handle(),
            GetCurrentProcess(),
            &raw mut copied,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last_error());
    }
    if copied.is_null() || copied == INVALID_HANDLE_VALUE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: successful DuplicateHandle returned the unique copy owned by this process.
    Ok(InheritedHandle(unsafe { Handle::own(copied) }))
}

fn pair_timeout(left: std::time::Duration) -> io::Result<i32> {
    // WSAPoll accepts signed milliseconds: round a positive fractional millisecond up, then cap
    // at the native finite width. The caller rechecks its SAME absolute deadline after each wait.
    let fractional = !left.subsec_nanos().is_multiple_of(1_000_000);
    let millis = left
        .as_millis()
        .checked_add(u128::from(fractional))
        .ok_or(io::ErrorKind::InvalidInput)?;
    let maximum = u128::try_from(i32::MAX).map_err(|_| io::ErrorKind::InvalidInput)?;
    i32::try_from(millis.min(maximum)).map_err(|_| io::ErrorKind::InvalidInput.into())
}

/// Wait for the caller's active endpoints in its exact two-socket observer. None omits a completed
/// stream entirely from the native wait. No data is consumed, physical socket
/// mode/timeout changed, handle duplicated or asynchronous I/O left pending. HUP/ERR is read-ready
/// so the same caller observes its actual EOF/error; neither is a whole-scope termination fact.
/// `patience` None waits with no deadline, as its caller's budget follows its guest (the review's C-9, (c2)): until an
/// endpoint is ready, the native wait's finite width at a time; one past what an instant holds is refused.
pub fn readable_pair(
    sockets: [Option<BorrowedSocket<'_>>; 2],
    patience: Option<std::time::Duration>,
) -> io::Result<[bool; 2]> {
    let deadline = match patience {
        None => None,
        Some(patience) => Some(
            std::time::Instant::now()
                .checked_add(patience)
                .ok_or(io::ErrorKind::InvalidInput)?,
        ),
    };
    let mut descriptors = [WSAPOLLFD {
        fd: INVALID_SOCKET,
        events: POLLRDNORM,
        revents: 0,
    }; 2];
    let mut positions = [0usize; 2];
    let mut count = 0usize;
    for (position, socket) in sockets.into_iter().enumerate() {
        let Some(socket) = socket else {
            continue;
        };
        let raw =
            SOCKET::try_from(socket.as_raw_socket()).map_err(|_| io::ErrorKind::InvalidInput)?;
        if raw == INVALID_SOCKET || descriptors.iter().any(|descriptor| descriptor.fd == raw) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        descriptors
            .get_mut(count)
            .ok_or(io::ErrorKind::InvalidInput)?
            .fd = raw;
        *positions
            .get_mut(count)
            .ok_or(io::ErrorKind::InvalidInput)? = position;
        count = count.checked_add(1).ok_or(io::ErrorKind::InvalidInput)?;
    }
    if count == 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let native_count = u32::try_from(count).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut initial = true;
    loop {
        let left = deadline.map_or(std::time::Duration::MAX, |deadline| {
            deadline.saturating_duration_since(std::time::Instant::now())
        });
        if !initial && left.is_zero() {
            return Ok([false; 2]);
        }
        initial = false;
        let timeout = pair_timeout(left)?;
        for descriptor in &mut descriptors {
            descriptor.revents = 0;
        }
        // SAFETY: exactly native_count initialized WSAPOLLFD records are writable on this stack; active
        // borrowed live sockets outlast the call. timeout is nonnegative/finite, never INFINITE.
        let returned = unsafe { WSAPoll(descriptors.as_mut_ptr(), native_count, timeout) };
        if returned == SOCKET_ERROR {
            // SAFETY: capture this thread's Winsock error immediately after the failed WSAPoll.
            let code = unsafe { WSAGetLastError() };
            if code == WSAEINTR {
                continue;
            }
            return Err(io::Error::from_raw_os_error(code));
        }
        if returned == 0 {
            continue;
        }
        if returned < 0
            || u32::try_from(returned).map_err(|_| io::ErrorKind::InvalidData)? > native_count
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mask = POLLRDNORM | POLLHUP | POLLERR;
        let mut ready = [false; 2];
        for (descriptor, position) in descriptors.iter().zip(positions).take(count) {
            if descriptor.revents & POLLNVAL != 0 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            *ready.get_mut(position).ok_or(io::ErrorKind::InvalidData)? =
                descriptor.revents & mask != 0;
        }
        if ready == [false; 2] {
            return Err(io::ErrorKind::InvalidData.into());
        }
        return Ok(ready);
    }
}

/// A member started suspended in its box's job.
pub struct Started {
    pub process: Handle,
    pub thread: Handle,
    pub pid: u32,
}

/// What a box's job holds for its members: the fully-built command line, environment block and working
/// directory as wide, NUL-handled strings, and the three standard handles the member inherits (each the
/// NUL device where the caller gave none). Built by the shell ([`super::mod`]); consumed by [`create`].
pub struct Spawn {
    /// The command line, UTF-16, NUL-terminated.
    pub command_line: Vec<u16>,
    /// The environment block: NUL-separated `name=value`, double-NUL-terminated, sorted (Windows reads a
    /// sorted block); empty but for one trailing NUL when the member gets an empty environment.
    pub environment: Vec<u16>,
    /// The working directory, UTF-16 NUL-terminated, or `None` for the node's.
    pub cwd: Option<Vec<u16>>,
    /// stdin, stdout, stderr: the handles the member inherits as its standard streams.
    pub stdio: [OwnedHandle; 3],
}

/// What a box's job enforces, as [`create`] sets it on the job.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory: Option<u64>,
    pub processes: Option<u32>,
    pub cpu: Option<Cpu>,
}

/// The rights a node takes on a box's job it opens by name (winnt.h's `JOB_OBJECT_ASSIGN_PROCESS`,
/// `_SET_ATTRIBUTES`, `_QUERY`, `_TERMINATE`; windows-sys has them only under
/// `Win32_System_SystemServices`): start members in it, set its limits, read it, end it.
const JOB_RIGHTS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0008;

/// A box's job: created, limited, its member started inside it, its accounting read, and ended. The
/// handle is closed on drop; with kill-on-close set, that ends every process still in the job.
pub struct Job(Handle);

fn job_name_wide(name: &str) -> io::Result<Vec<u16>> {
    if name.is_empty() || name.contains('\0') {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let units = name
        .encode_utf16()
        .count()
        .checked_add(1)
        .ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut wide = Vec::new();
    wide.try_reserve_exact(units)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    wide.extend(name.encode_utf16());
    wide.push(0);
    Ok(wide)
}

impl Job {
    /// A new, empty job object.
    pub fn create() -> io::Result<Job> {
        // SAFETY: CreateJobObjectW with null attributes and null name makes an unnamed job and returns
        // its handle, or null on failure.
        let h = unsafe { CreateJobObjectW(null(), null()) };
        if h.is_null() {
            return Err(last_error());
        }
        // SAFETY: h is a job handle this process just created and nothing else owns.
        Ok(Job(unsafe { Handle::own(h) }))
    }

    /// A new, empty job object named `name` (in the session's `Local\` namespace), its security the
    /// creator's default: this user and SYSTEM. One of that name held already is refused
    /// (`ERROR_ALREADY_EXISTS`): it is a box a node made before, which [`Job::open`] takes back.
    pub fn create_named(name: &str) -> io::Result<Job> {
        let wide = job_name_wide(name)?;
        // SAFETY: null attributes; `wide` is a NUL-terminated name alive for the call. The handle returned
        // (to a new job, or to one of that name already, which GetLastError then says) is owned below.
        let h = unsafe { CreateJobObjectW(null(), wide.as_ptr()) };
        if h.is_null() {
            return Err(last_error());
        }
        let existed = last_error().raw_os_error() == i32::try_from(ERROR_ALREADY_EXISTS).ok();
        // SAFETY: h is a job handle this call returned, owned from here, closed as the value drops.
        let job = Job(unsafe { Handle::own(h) });
        if existed {
            let code = i32::try_from(ERROR_ALREADY_EXISTS)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(job)
    }

    /// The job named `name`, if one is: a box a node before this one made, which outlived it. `None` where
    /// no job holds the name (the box's members all ended and its last handle closed, or the machine
    /// restarted).
    pub fn open(name: &str) -> io::Result<Option<Job>> {
        let wide = job_name_wide(name)?;
        // SAFETY: `wide` is a NUL-terminated name alive for the call; not inherited. The handle returned
        // is owned below.
        let h = unsafe { OpenJobObjectW(JOB_RIGHTS, 0, wide.as_ptr()) };
        if h.is_null() {
            let e = last_error();
            if e.raw_os_error() == i32::try_from(ERROR_FILE_NOT_FOUND).ok() {
                return Ok(None);
            }
            return Err(e);
        }
        // SAFETY: h is a job handle this call returned, owned from here.
        Ok(Some(Job(unsafe { Handle::own(h) })))
    }

    /// The job's handle, borrowed.
    pub fn borrowed(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.0.borrowed()
    }

    pub fn raw(&self) -> HANDLE {
        self.0.raw()
    }

    /// Sets the job's limits: its bounds ([`Job::set_bounds`]) and the CPU weight or hard cap.
    pub fn configure(&self, limits: &Limits) -> io::Result<()> {
        self.set_bounds(limits)?;
        if let Some(cpu) = limits.cpu {
            self.set_cpu(cpu)?;
        }
        Ok(())
    }

    /// Sets the job's bounds alone, its CPU control left as it is: a hard commit cap, an active-process cap and
    /// kill-on-close (so closing the last handle ends the tree). Its limit flags replace the job's, so a bound
    /// `limits` doesn't name is lifted. Breakaway is never allowed (neither `BREAKAWAY_OK` nor
    /// `SILENT_BREAKAWAY_OK` is set), so no member can leave the job. A node adopting a VM's job writes them
    /// (joint plan step 14, ruling B), where the learner holds its CPU rate.
    pub fn set_bounds(&self, limits: &Limits) -> io::Result<()> {
        let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed_struct();
        let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = limits.memory {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            ext.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
        }
        if let Some(n) = limits.processes {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            ext.BasicLimitInformation.ActiveProcessLimit = n;
        }
        ext.BasicLimitInformation.LimitFlags = flags;
        self.set(JobObjectExtendedLimitInformation, &ext)
    }

    /// Sets the job's CPU weight or hard cap, as [`Job::configure`] does; also the charge learner's, which caps a
    /// managed VM's job at its water-filled share each window (joint plan step 14, C4 on Windows).
    pub fn set_cpu(&self, cpu: Cpu) -> io::Result<()> {
        let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = zeroed_struct();
        match cpu {
            Cpu::Weight(w) => {
                rate.ControlFlags =
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED;
                rate.Anonymous.Weight = w;
            }
            Cpu::Cap(r) => {
                rate.ControlFlags =
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                rate.Anonymous.CpuRate = r;
            }
        }
        self.set(JobObjectCpuRateControlInformation, &rate)
    }

    /// The job's commit limit, where it holds one (`JOB_OBJECT_LIMIT_JOB_MEMORY` set): its tests read back what
    /// a configure or an adoption wrote.
    #[cfg(test)]
    pub fn memory_limit(&self) -> io::Result<Option<u64>> {
        let ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            self.query(JobObjectExtendedLimitInformation)?;
        let held = (ext.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY) != 0;
        Ok(held.then(|| u64::try_from(ext.JobMemoryLimit).unwrap_or(u64::MAX)))
    }

    fn set<T>(&self, class: i32, value: &T) -> io::Result<()> {
        let len = u32::try_from(size_of::<T>()).unwrap_or(u32::MAX);
        // SAFETY: SetInformationJobObject reads `len` bytes from `value` (one T, alive for the call) and
        // sets them on the job self owns. `class` names the structure T is.
        if unsafe { SetInformationJobObject(self.0.raw(), class, (value as *const T).cast(), len) }
            == 0
        {
            return Err(last_error());
        }
        Ok(())
    }

    fn query<T>(&self, class: i32) -> io::Result<T> {
        let mut value: T = zeroed_struct();
        let len = u32::try_from(size_of::<T>()).unwrap_or(u32::MAX);
        // SAFETY: QueryInformationJobObject writes at most `len` bytes (one T) into `value`, which is
        // zeroed and alive for the call; `class` names the structure T is.
        if unsafe {
            QueryInformationJobObject(
                self.0.raw(),
                class,
                (&mut value as *mut T).cast(),
                len,
                null_mut(),
            )
        } == 0
        {
            return Err(last_error());
        }
        Ok(value)
    }

    /// The job's peak committed memory, CPU time (user + kernel, in 100 ns ticks) and I/O bytes. All
    /// authoritative reads, never a lossy message (CLAUDE.md §3). The live process count is
    /// [`Job::live_processes`]; the job's accounting `ActiveProcesses` is not used, as it lingers while
    /// a terminated member's handle is held.
    pub fn accounting(&self) -> io::Result<Accounting> {
        let a: JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION =
            self.query(JobObjectBasicAndIoAccountingInformation)?;
        let ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            self.query(JobObjectExtendedLimitInformation)?;
        let ticks = |t: i64| u64::try_from(t).unwrap_or(0);
        Ok(Accounting {
            peak_memory: u64::try_from(ext.PeakJobMemoryUsed).unwrap_or(u64::MAX),
            cpu_100ns: ticks(a.BasicInfo.TotalUserTime)
                .saturating_add(ticks(a.BasicInfo.TotalKernelTime)),
            read_bytes: a.IoInfo.ReadTransferCount,
            write_bytes: a.IoInfo.WriteTransferCount,
        })
    }

    /// How many processes are live in the job right now: `JobObjectBasicProcessIdList`'s
    /// `NumberOfAssignedProcesses`, which Windows fills in even when the buffer holds fewer ids
    /// (`ERROR_MORE_DATA`; jobobject_basic_process_id_list.md: "The number of process identifiers to be
    /// stored in ProcessIdList"), so one read with room for one id learns the job's whole count at any
    /// size (CLAUDE.md §2: bounded by the job's own count, not a picked one). An ended member leaves the
    /// list though the node still holds its process handle, before it leaves the count (HEX-WIN-1's lag), so a
    /// read listing none counts none ([`live`]) and a box whose members have exited reads as empty. The job's
    /// accounting `ActiveProcesses` is not used.
    pub fn live_processes(&self) -> io::Result<u64> {
        self.process_list().map(|list| {
            live(
                list.NumberOfAssignedProcesses,
                list.NumberOfProcessIdsInList,
            )
        })
    }

    /// An event to wait on for this same nested scope, or positive emptiness. A recycled snapshot
    /// PID is checked against the held job before its observation handle can be used.
    pub(crate) fn member(&self) -> io::Result<Member> {
        let list = self.process_list()?;
        if empty(
            list.NumberOfAssignedProcesses,
            list.NumberOfProcessIdsInList,
        ) {
            return Ok(Member::Empty);
        }
        let pid = list
            .ProcessIdList
            .first()
            .copied()
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid != 0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        let process =
            process_with_rights(pid, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)?;
        if !super::oci::sys::in_job(&process, self.borrowed())? {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(Member::Process(process))
    }

    fn process_list(&self) -> io::Result<JOBOBJECT_BASIC_PROCESS_ID_LIST> {
        #[cfg(test)]
        tests::ID_LIST_READS.with(|n| n.set(n.get() + 1));
        let mut list: JOBOBJECT_BASIC_PROCESS_ID_LIST = zeroed_struct();
        let bytes = u32::try_from(size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>())
            .map_err(|_| io::Error::other("a job's process list header exceeds a DWORD"))?;
        // SAFETY: the SDK structure has room for one ULONG_PTR and is alive for this bounded
        // query. NumberOfAssignedProcesses includes descendants in nested jobs even when the
        // returned PID buffer is too small (Microsoft JOBOBJECT_BASIC_PROCESS_ID_LIST contract).
        let ok = unsafe {
            QueryInformationJobObject(
                self.0.raw(),
                JobObjectBasicProcessIdList,
                (&raw mut list).cast(),
                bytes,
                null_mut(),
            )
        };
        if ok == 0 {
            let error = last_error();
            if error.raw_os_error() != i32::try_from(ERROR_MORE_DATA).ok() {
                return Err(error);
            }
        }
        Ok(list)
    }

    /// Ends every process in the job with `code`, at once. Idempotent: an already-empty job is fine.
    pub fn terminate(&self, code: u32) -> io::Result<()> {
        // SAFETY: self owns a valid job handle; TerminateJobObject ends its processes and returns.
        if unsafe { TerminateJobObject(self.0.raw(), code) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

/// This is a short observation of an existing job, not a process receipt/reaper or holder table.
pub(crate) enum Member {
    Empty,
    Process(Handle),
}

/// A job's live members as one id-list read gives them, `assigned` its `NumberOfAssignedProcesses` and `listed` its
/// `NumberOfProcessIdsInList` ([`Job::live_processes`]): none where it lists none, though its assigned count may still
/// hold members that have ended (Windows drops an ended member from the list before its count: HEX-WIN-1's lag, 2
/// assigned and 1 listed in one read); else its assigned count, every listed member live and the rest unlisted only
/// past the read's room of one id.
fn live(assigned: u32, listed: u32) -> u64 {
    if listed == 0 { 0 } else { u64::from(assigned) }
}

/// Whether one id-list read says the job holds no live member ([`Job::member`]): none assigned, or none listed.
fn empty(assigned: u32, listed: u32) -> bool {
    assigned == 0 || listed == 0
}

/// A job's resource accounting, read whole.
#[derive(Debug, Clone, Copy, Default)]
pub struct Accounting {
    pub peak_memory: u64,
    pub cpu_100ns: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
}

#[cfg(test)]
thread_local! {
    /// HEX-INST-9: count this thread's process starts to detect a feature read spawning PowerShell.
    static STARTED_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn started_here() -> u64 {
    STARTED_HERE.with(std::cell::Cell::get)
}

/// Starts `spawn` suspended in `jobs` (its box's job, and the node's where there is one), so it is a
/// member of them from its creation and nothing runs outside them (`PROC_THREAD_ATTRIBUTE_JOB_LIST`).
/// Only its three standard handles are inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`). The caller
/// resumes it with [`resume`].
pub fn create(spawn: &mut Spawn, jobs: &[HANDLE]) -> io::Result<Started> {
    create_with_report(spawn, jobs, None)
}

/// The SAME atomic job-list launcher, optionally carrying one already-owned report writer.
/// Inherited handles retain their value in this child (CreateProcessW/Inheritance contract).
pub(crate) fn create_with_report(
    spawn: &mut Spawn,
    jobs: &[HANDLE],
    report: Option<&InheritedHandle>,
) -> io::Result<Started> {
    // The three standard handles, made inheritable, as a contiguous array for the handle-list attribute.
    let handles = spawn
        .stdio
        .each_ref()
        .map(|handle| handle.as_raw_handle() as HANDLE);
    for h in handles {
        // SAFETY: each handle is owned by spawn.stdio for the call; marking it inheritable is sound.
        if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(last_error());
        }
    }

    // Two attributes: the inherited-handle list and the job list.
    let mut size = 0usize;
    // SAFETY: the first call, with a null list, only writes the required byte size into `size`; it is
    // expected to fail with ERROR_INSUFFICIENT_BUFFER, which we ignore in favour of `size`.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size) };
    if size == 0 {
        return Err(io::Error::other("attribute list size is zero"));
    }
    // Pointer-aligned storage for the native attribute list, reserved before any process exists.
    let words = size.div_ceil(size_of::<usize>());
    let mut buf = Vec::<usize>::new();
    buf.try_reserve_exact(words)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    buf.resize(words, 0);
    let list = buf.as_mut_ptr().cast::<core::ffi::c_void>();
    // SAFETY: `list` points at `size` writable bytes (buf), the size the call above asked for, for 2
    // attributes.
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
        return Err(last_error());
    }
    let [input, output, error] = handles;
    let inherited = [
        input,
        output,
        error,
        report.map_or(null_mut(), |report| report.0.raw()),
    ];
    let inherited = if report.is_some() {
        inherited.as_slice()
    } else {
        handles.as_slice()
    };
    // Attribute values, including the optional fourth handle, remain alive until the list is
    // destroyed below (UpdateProcThreadAttribute's lpValue lifetime contract).
    let started = create_in(spawn, jobs, &handles, inherited, list);
    // SAFETY: `list` was initialized above; deleting it frees its internal allocations once, and buf
    // (its backing bytes) is still alive and dropped after.
    unsafe { DeleteProcThreadAttributeList(list) };
    started
}

/// The body of [`create`] once the attribute list is allocated, so the list is always deleted.
fn create_in(
    spawn: &mut Spawn,
    jobs: &[HANDLE],
    stdio: &[HANDLE; 3],
    handles: &[HANDLE],
    list: LPPROC_THREAD_ATTRIBUTE_LIST,
) -> io::Result<Started> {
    let handles_bytes = size_of::<HANDLE>()
        .checked_mul(handles.len())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: the handle-list points at exactly handles.len() live inheritable handles. The three
    // stdio handles and optional private report copy outlive this call; the list has two slots.
    if unsafe {
        windows_sys::Win32::System::Threading::UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            handles_bytes,
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(last_error());
    }
    if !jobs.is_empty() {
        let jobs_bytes = size_of::<HANDLE>()
            .checked_mul(jobs.len())
            .ok_or_else(|| io::Error::other("job-list attribute size overflows"))?;
        // SAFETY: the job-list attribute points at `jobs` (jobs.len() HANDLEs), alive for this call; the
        // size matches.
        if unsafe {
            windows_sys::Win32::System::Threading::UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                jobs_bytes,
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(last_error());
        }
    }

    let mut si: STARTUPINFOEXW = zeroed_struct();
    si.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
        .map_err(|_| io::Error::other("STARTUPINFOEXW exceeds a DWORD"))?;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let [input, output, error] = *stdio;
    si.StartupInfo.hStdInput = input;
    si.StartupInfo.hStdOutput = output;
    si.StartupInfo.hStdError = error;
    si.lpAttributeList = list;

    let cwd = spawn.cwd.as_ref().map_or(null(), |c| c.as_ptr());
    let environment = spawn.environment.as_ptr().cast();
    let mut pi: PROCESS_INFORMATION = zeroed_struct();
    // SAFETY: command_line is a NUL-terminated writable UTF-16 buffer (CreateProcessW may modify it);
    // environment is a supplied double-NUL-terminated UTF-16 block; cwd is NUL-terminated or null; si and pi are
    // valid and alive for the call. bInheritHandles is TRUE because only the listed handles are
    // inherited (the handle-list attribute). The process is created suspended and in its jobs.
    let ok = unsafe {
        CreateProcessW(
            null(),
            spawn.command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT,
            environment,
            cwd,
            (&si as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    #[cfg(test)]
    STARTED_HERE.with(|n| n.set(n.get().saturating_add(1)));
    // SAFETY: CreateProcessW succeeded, so pi holds a process handle this process now owns.
    let process = unsafe { Handle::own(pi.hProcess) };
    // SAFETY: and a thread handle this process now owns.
    let thread = unsafe { Handle::own(pi.hThread) };
    Ok(Started {
        process,
        thread,
        pid: pi.dwProcessId,
    })
}

/// Lets a suspended member run (its one initial thread).
pub fn resume(thread: &Handle) -> io::Result<()> {
    // SAFETY: thread is a valid, owned thread handle. ResumeThread returns the previous suspend count,
    // or u32::MAX on failure.
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        return Err(last_error());
    }
    Ok(())
}

/// A copy of `source` (a handle of this process's) made in `into`'s process, with the same access: its
/// value there, which only `into` uses (a node's keeper, `super::keeper`). Not inheritable.
pub fn duplicate_into(source: usize, into: &Handle) -> io::Result<usize> {
    let mut theirs: HANDLE = null_mut();
    // SAFETY: `source` is a handle value of this process, `into` a process handle opened with
    // PROCESS_DUP_HANDLE (owned by the caller for the call); the value written is `into`'s, a number here.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source as HANDLE,
            into.raw(),
            &raw mut theirs,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(theirs as usize)
}

/// Closes the handle `value` in `into`'s process (`DUPLICATE_CLOSE_SOURCE`, no copy made).
pub fn close_in(into: &Handle, value: usize) -> io::Result<()> {
    // SAFETY: `into` is a process handle opened with PROCESS_DUP_HANDLE; with DUPLICATE_CLOSE_SOURCE and no
    // target the call closes `value` there and makes nothing here.
    let ok = unsafe {
        DuplicateHandle(
            into.raw(),
            value as HANDLE,
            null_mut(),
            null_mut(),
            0,
            0,
            DUPLICATE_CLOSE_SOURCE,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// The caller holds its existing keeper lock from comparison through close. Duplicate only an
/// observation handle here; a stale/reused remote value never authorizes closing another object.
pub(crate) fn close_in_if_same(
    into: &Handle,
    value: usize,
    expected: BorrowedHandle<'_>,
) -> io::Result<bool> {
    if !same_in(into, value, expected)? {
        return Ok(false);
    }
    close_in(into, value)?;
    Ok(true)
}

pub(crate) fn same_in(
    into: &Handle,
    value: usize,
    expected: BorrowedHandle<'_>,
) -> io::Result<bool> {
    let mut copied: HANDLE = null_mut();
    // SAFETY: into is a verified keeper process handle with PROCESS_DUP_HANDLE. Its value is
    // borrowed, not consumed; the duplicate is non-inheritable and owned below on success.
    if unsafe {
        DuplicateHandle(
            into.raw(),
            value as HANDLE,
            GetCurrentProcess(),
            &raw mut copied,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: DuplicateHandle succeeded and transferred ownership of this new local handle.
    let observed = unsafe { Handle::own(copied) };
    // SAFETY: both handles remain alive for the comparison; this API compares kernel object
    // identity without modifying either object or requiring object-specific access rights.
    Ok(unsafe { CompareObjectHandles(observed.raw(), expected.as_raw_handle() as HANDLE) } != 0)
}

/// Process `pid` opened for waiting on, handing handles to and from, and reading its times; `None` where
/// no process has the pid.
pub fn open_process(pid: u32) -> io::Result<Option<Handle>> {
    match process_with_rights(
        pid,
        PROCESS_SYNCHRONIZE | PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
    ) {
        Ok(process) => Ok(Some(process)),
        Err(error) if error.raw_os_error() == Some(87) => Ok(None),
        Err(error) => Err(error),
    }
}

fn process_with_rights(pid: u32, rights: u32) -> io::Result<Handle> {
    // SAFETY: opens `pid` with the rights named; the handle returned is owned below.
    let h = unsafe { OpenProcess(rights, 0, pid) };
    if h.is_null() {
        let e = last_error();
        return Err(e);
    }
    // SAFETY: a handle just opened, owned from here.
    Ok(unsafe { Handle::own(h) })
}

/// When `process` was created (100 ns ticks since 1601), which with its pid names it: a process given
/// the pid since has another.
pub fn created(process: &Handle) -> io::Result<u64> {
    let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (
        zeroed_struct(),
        zeroed_struct(),
        zeroed_struct(),
        zeroed_struct(),
    );
    // SAFETY: a process handle with PROCESS_QUERY_LIMITED_INFORMATION; four FILETIMEs written.
    let ok = unsafe {
        GetProcessTimes(
            process.raw(),
            &raw mut c,
            &raw mut e,
            &raw mut k,
            &raw mut u,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// A manual-reset event named `name` (NUL-terminated wide, in the session's `Local\\` namespace),
/// unsignalled: made if new, else the one of that name opened. The node and its keeper share it by name so
/// either side makes it.
pub fn named_event(name: &[u16]) -> io::Result<Handle> {
    // SAFETY: no attributes; manual-reset, unsignalled; `name` is NUL-terminated and alive for the call.
    // CreateEventW returns a handle to a new event or to one of that name already, owned below.
    let h = unsafe { CreateEventW(null(), 1, 0, name.as_ptr()) };
    if h.is_null() {
        return Err(last_error());
    }
    // SAFETY: a handle just made or opened, owned from here.
    Ok(unsafe { Handle::own(h) })
}

/// Signals `event`.
pub fn set(event: &Handle) -> io::Result<()> {
    // SAFETY: an event handle this process owns.
    if unsafe { SetEvent(event.raw()) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

pub(crate) fn reset(event: &Handle) -> io::Result<()> {
    // SAFETY: event is an owned kernel event, alive for the call.
    if unsafe { ResetEvent(event.raw()) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Event-driven keeper wake/stop. Exactly these two owned handles remain alive across the wait.
pub(crate) fn wait_either(first: &Handle, second: &Handle) -> io::Result<bool> {
    let handles = [first.raw(), second.raw()];
    // SAFETY: this fixed two-element array outlives the call; FALSE waits for any event, without polling.
    match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) } {
        WAIT_OBJECT_0 => Ok(true),
        value
            if value
                == WAIT_OBJECT_0
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("wait index overflows"))? =>
        {
            Ok(false)
        }
        _ => Err(last_error()),
    }
}

/// Joint plan step 14 (AC-T7.4.1): runs the calling thread at `THREAD_PRIORITY_TIME_CRITICAL`, so a sampler keeps its own
/// deadlines beside busy threads of normal priority (the Windows charge-window probe's,
/// crates/hex-runtime/tests/windows_charge_window.rs).
pub fn time_critical() -> io::Result<()> {
    // SAFETY: GetCurrentThread returns the calling thread's pseudo-handle, valid for this call and closing nothing;
    // SetThreadPriority takes it and a documented priority level.
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Joint plan step 14 (AC-T7.4.1): runs the calling process at `BELOW_NORMAL_PRIORITY_CLASS`, so its busy threads give
/// way to a sampler of normal priority or above (the charge-window probe's spinners).
pub fn below_normal() -> io::Result<()> {
    // SAFETY: GetCurrentProcess returns the calling process's pseudo-handle, valid for this call and closing nothing;
    // SetPriorityClass takes it and a documented priority class.
    if unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// Joint plan step 14 (AC-T7.4.1): the machine's idle, kernel and user time so far, in 100 ns, summed over its
/// processors (`GetSystemTimes`; kernel time holds idle time), so the charge-window probe can say what else ran.
pub fn system_times() -> io::Result<[u64; 3]> {
    let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) =
        (zeroed_struct(), zeroed_struct(), zeroed_struct());
    // SAFETY: three FILETIMEs this frame owns, each written by the call.
    if unsafe { GetSystemTimes(&raw mut idle, &raw mut kernel, &raw mut user) } == 0 {
        return Err(last_error());
    }
    let time = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Ok([time(idle), time(kernel), time(user)])
}

/// A high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`): a wait on it ends near its due time, not
/// at the system timer's tick (15.6 ms unless raised), so a sampler can keep a 1 ms cadence without spinning (joint
/// plan step 14's charge-window probe).
pub struct HighResTimer(Handle);

impl HighResTimer {
    pub fn new() -> io::Result<Self> {
        // SAFETY: no security attributes and no name are allowed; the flag and the access are documented values. The
        // handle returned, where not null, is this process's own and owned below.
        let h = unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if h.is_null() {
            return Err(last_error());
        }
        // SAFETY: a timer handle just made, owned from here.
        Ok(Self(unsafe { Handle::own(h) }))
    }

    /// Waits `after` from now: the timer is set that far ahead, relative (a negative due time, in 100 ns), and waited
    /// on.
    pub fn wait(&self, after: std::time::Duration) -> io::Result<()> {
        let due = i64::try_from(after.as_nanos() / 100)
            .ok()
            .and_then(i64::checked_neg)
            .ok_or_else(|| io::Error::other("a timer's due time passes an i64 of 100 ns"))?;
        // SAFETY: the timer is a valid handle this value owns; `due` outlives the call; no period, no completion
        // routine and no argument for one, and no resume from suspend.
        if unsafe { SetWaitableTimer(self.0.raw(), &due, 0, None, null(), 0) } == 0 {
            return Err(last_error());
        }
        // SAFETY: the timer is a valid handle this value owns; the wait ends when it is signalled.
        match unsafe { WaitForSingleObject(self.0.raw(), u32::MAX) } {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(last_error()),
        }
    }
}

/// Whether `process` ended within `timeout_ms` (`u32::MAX` is forever).
pub fn wait(process: &Handle, timeout_ms: u32) -> io::Result<bool> {
    // SAFETY: process is a valid, owned process handle; WaitForSingleObject waits on it.
    match unsafe { WaitForSingleObject(process.raw(), timeout_ms) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(last_error()),
    }
}

/// The host's boot sequence: `KUSER_SHARED_DATA`'s `BootId`, "Boot sequence, incremented for each boot attempt by the
/// OS loader" (ntddk.h; learn.microsoft.com/windows-hardware/drivers/ddi/ntddk/ns-ntddk-kuser_shared_data), so no two
/// boots of a host share one. Read where the kernel maps that structure read-only into every process, ntddk.h's
/// `MM_SHARED_USER_DATA_VA` (0x7FFE0000): `BootId` is at 0x2C4 in the header's layout, after `ProcessorFeatures`' 64
/// bytes from 0x274 and the four 32-bit members `Reserved1`, `Reserved3`, `TimeSlip` and `AlternativeArchitecture`.
/// No call is made and no handle opened, so no program another thread starts can inherit one.
pub fn boot_id() -> u32 {
    /// `MM_SHARED_USER_DATA_VA` and `BootId`'s offset in `KUSER_SHARED_DATA`, 0x7FFE0000 + 0x2C4.
    const BOOT_ID: usize = 0x7FFE_02C4;
    // SAFETY: the kernel maps KUSER_SHARED_DATA read-only at MM_SHARED_USER_DATA_VA into every user-mode process, on
    // every architecture Windows runs, for the process's whole life and before any of its code runs. BootId is a
    // ULONG at a 4-aligned offset inside that page, initialized at boot; this process never writes it.
    unsafe { std::ptr::with_exposed_provenance::<u32>(BOOT_ID).read_volatile() }
}

/// Flushes directory `dir` (FlushFileBuffers on its handle), so a rename or removal in it outlives a host crash.
/// CreateFileW opens a directory only with `FILE_FLAG_BACKUP_SEMANTICS`, and FlushFileBuffers needs a handle with
/// `GENERIC_WRITE` (both in their Win32 reference pages), so it is opened for writing: std's `File::open` opens it
/// for reading alone, refused "Access is denied" (os error 5). No `unsafe`: std's open takes the flag.
pub fn flush_directory(dir: &std::path::Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

/// Publish a flushed temp file beside its final name. MoveFileExW documents write-through move
/// completion; this is not by itself a hardware power-loss proof (managed-native.md).
pub fn replace_durable(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    fn path(path: &std::path::Path) -> io::Result<Vec<u16>> {
        use std::os::windows::ffi::OsStrExt;
        let count = path
            .as_os_str()
            .encode_wide()
            .count()
            .checked_add(1)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let mut wide = Vec::new();
        wide.try_reserve_exact(count)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        for unit in path.as_os_str().encode_wide() {
            if unit == 0 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            wide.push(unit);
        }
        wide.push(0);
        Ok(wide)
    }
    let source = path(from)?;
    let destination = path(to)?;
    // SAFETY: both buffers are owned, NUL-terminated native paths alive through the call. The caller
    // owns both names; flags request immediate same-volume replacement, never copy or reboot work.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(())
}

/// Ends `process` alone with exit code `code` (`Proc::kill`: its job and the job's other members run on).
/// A process that has ended already is refused `ERROR_ACCESS_DENIED`; the caller reads its exit to tell.
pub fn terminate_process(process: &Handle, code: u32) -> io::Result<()> {
    // SAFETY: process is a valid, owned process handle; TerminateProcess ends that process and returns.
    if unsafe { TerminateProcess(process.raw(), code) } == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// `process`'s exit code, once it has ended (`STILL_ACTIVE`, 259, while it runs).
pub fn exit_code(process: &Handle) -> io::Result<u32> {
    let mut code = 0u32;
    // SAFETY: GetExitCodeProcess writes one u32 into `code`, alive for the call, for the owned handle.
    if unsafe { GetExitCodeProcess(process.raw(), &mut code) } == 0 {
        return Err(last_error());
    }
    Ok(code)
}

// The guest-cost producer's reads (crates/hex-cli/examples/guest_cost, AC-T7.4.1), beside what a node reads of a
// managed VM (its job's accounting, `JobBox::usage`, and its vCPUs' guest time from Shards' answer): where a VM
// process's guest RAM lies and what it has committed, so its VMM's own memory is told from its guest's; the memory and
// disk quanta; and what its folders hold.

/// `ERROR_INVALID_PARAMETER`: what Windows answers for a process id no process has now, or an address past a process's
/// highest.
const INVALID_PARAMETER: i32 = 87;

/// Process `pid` opened for the guest-cost producer's reads of its address space's regions
/// (`PROCESS_QUERY_INFORMATION`, what `VirtualQueryEx` asks); `None` where no process has the pid.
pub fn open_for_reading(pid: u32) -> io::Result<Option<Handle>> {
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;
    match process_with_rights(pid, PROCESS_QUERY_INFORMATION) {
        Ok(process) => Ok(Some(process)),
        Err(error) if error.raw_os_error() == Some(INVALID_PARAMETER) => Ok(None),
        Err(error) => Err(error),
    }
}

/// One region of a process's address space, as `VirtualQueryEx` reads it: where it starts, how long it is, and whether
/// it is committed private read/write memory (`MEM_COMMIT`, `MEM_PRIVATE`, `PAGE_READWRITE`), as Shards' WHP guest RAM
/// is (its `reserve_ram`: a placeholder replaced at once by committed read/write memory, its whole commit charged from
/// the start, which its job's accounting counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub base: u64,
    pub size: u64,
    pub private_rw: bool,
}

/// The region of `process`'s address space holding address `at`; `None` past its last.
pub fn region(process: &Handle, at: u64) -> io::Result<Option<Region>> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQueryEx,
    };
    let address = usize::try_from(at).map_err(|_| io::Error::other("an address past usize"))?;
    let mut info: MEMORY_BASIC_INFORMATION = zeroed_struct();
    // SAFETY: a process handle with query rights; `address` names a place in its space, an integer the call reads and
    // never dereferences here; `info` of the size given, written by the call.
    let wrote = unsafe {
        VirtualQueryEx(
            process.raw(),
            std::ptr::without_provenance(address),
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if wrote == 0 {
        let error = last_error();
        // An address past the process's highest.
        return if error.raw_os_error() == Some(INVALID_PARAMETER) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    Ok(Some(Region {
        base: u64::try_from(info.BaseAddress.addr())
            .map_err(|_| io::Error::other("an address past u64"))?,
        size: u64::try_from(info.RegionSize).map_err(|_| io::Error::other("a region past u64"))?,
        private_rw: info.State == MEM_COMMIT
            && info.Type == MEM_PRIVATE
            && info.Protect == PAGE_READWRITE,
    }))
}

/// This machine's page size (`GetSystemInfo`'s `dwPageSize`).
pub fn page_size() -> u32 {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    let mut info: SYSTEM_INFO = zeroed_struct();
    // SAFETY: GetSystemInfo writes one SYSTEM_INFO this frame owns, and can't fail.
    unsafe { GetSystemInfo(&raw mut info) };
    info.dwPageSize
}

/// The allocation unit of the volume that holds `path`: its mount point (`GetVolumePathNameW`, never longer than the
/// path it holds and its trailing separator), then that volume's sectors per cluster times bytes per sector
/// (`GetDiskFreeSpaceW`): the disk's quantum, as statvfs's `f_frsize` is elsewhere.
pub fn cluster_bytes(path: &std::path::Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let room = wide
        .len()
        .checked_add(1)
        .ok_or_else(|| io::Error::other("a path past usize"))?;
    let mut volume = vec![0u16; room];
    let units = u32::try_from(volume.len()).map_err(|_| io::Error::other("a path past u32"))?;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; `volume` holds `units` u16s, which the
    // call writes a NUL-terminated mount point into.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), units) } == 0 {
        return Err(last_error());
    }
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `volume` is the NUL-terminated mount point the call above wrote; four u32s this frame owns, each written.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            volume.as_ptr(),
            &raw mut sectors,
            &raw mut bytes,
            &raw mut free,
            &raw mut total,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    u64::from(sectors)
        .checked_mul(u64::from(bytes))
        .filter(|unit| *unit > 0)
        .ok_or_else(|| io::Error::other("a volume that names no allocation unit"))
}

/// The file at `path` as the guest-cost producer counts what its folders hold: the file it names (its volume's serial
/// number and its file index, which a rename keeps: `GetFileInformationByHandle`) and its allocated bytes
/// (`FileStandardInfo`'s `AllocationSize`), opened for its attributes alone (`FILE_READ_ATTRIBUTES`, which no sharing
/// mode refuses), a link read as itself. `None` where nothing is at `path` now.
pub fn file_held(path: &std::path::Path) -> io::Result<Option<((u64, u64), u64)>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; no security attributes or template; the
    // handle returned is owned below.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        let error = last_error();
        let gone = [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
            .iter()
            .any(|code| error.raw_os_error() == i32::try_from(*code).ok());
        return if gone { Ok(None) } else { Err(error) };
    }
    // SAFETY: a handle just opened, owned from here and closed as `file` drops.
    let file = unsafe { Handle::own(raw) };
    let mut info: BY_HANDLE_FILE_INFORMATION = zeroed_struct();
    // SAFETY: the file's handle; one BY_HANDLE_FILE_INFORMATION this frame owns, written by the call.
    if unsafe { GetFileInformationByHandle(file.raw(), &raw mut info) } == 0 {
        return Err(last_error());
    }
    let mut standard: FILE_STANDARD_INFO = zeroed_struct();
    let size = u32::try_from(size_of::<FILE_STANDARD_INFO>())
        .map_err(|_| io::Error::other("FILE_STANDARD_INFO's size past u32"))?;
    // SAFETY: the file's handle; one FILE_STANDARD_INFO of the size given, which this frame owns, written by the call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.raw(),
            FileStandardInfo,
            (&raw mut standard).cast(),
            size,
        )
    } == 0
    {
        return Err(last_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let allocated = u64::try_from(standard.AllocationSize)
        .map_err(|_| io::Error::other("a file's allocation below none"))?;
    Ok(Some((
        (u64::from(info.dwVolumeSerialNumber), index),
        allocated,
    )))
}

/// A zeroed `T` of a plain Windows structure (every field an integer, pointer or union of them, for
/// which all-zero is a valid, inert value). Used for the job-info and startup structures this file
/// fills before a call.
fn zeroed_struct<T>() -> T {
    // SAFETY: T is a `#[repr(C)]` Windows structure of integers, pointers and unions of them; all-zero
    // is a valid bit pattern (null pointers, zero counts, empty flags). Only such types are passed here.
    unsafe { zeroed() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_NOTIFICATION_LIMIT_INFORMATION, JobObjectNotificationLimitInformation,
    };

    thread_local! {
        /// The job id-list reads [`Job::live_processes`] made on this thread.
        pub(super) static ID_LIST_READS: Cell<u32> = const { Cell::new(0) };
    }

    fn nul() -> OwnedHandle {
        OwnedHandle::from(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("NUL")
                .unwrap(),
        )
    }

    /// The job's id list read whole, with room for `room` ids: its `NumberOfAssignedProcesses`,
    /// `NumberOfProcessIdsInList` and the ids listed.
    fn id_list(job: &Job, room: usize) -> (u32, u32, Vec<usize>) {
        let mut buf = vec![0usize; room + 1];
        let bytes = u32::try_from(buf.len() * size_of::<usize>()).unwrap();
        // SAFETY: QueryInformationJobObject writes at most `bytes` bytes into `buf`, alive and that long
        // for the call; the header is two u32s in the first usize (x86_64), then the ids.
        let ok = unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr().cast(),
                bytes,
                null_mut(),
            )
        };
        assert_ne!(ok, 0, "{}", last_error());
        let head = buf[0].to_ne_bytes();
        let assigned = u32::from_ne_bytes(head[..4].try_into().unwrap());
        let listed = u32::from_ne_bytes(head[4..8].try_into().unwrap());
        (assigned, listed, buf[1..=listed as usize].to_vec())
    }

    /// HEX-WIN-1's lag (ROOT's native run of 2026-11-14 at 211292c80: 2 assigned, 1 listed in one read): Windows drops
    /// an ended member from a job's id list before its assigned count, so the list is the live truth. Do read the
    /// lagging counts as one read gives them. Expect: a read listing no member is empty and counts none live, whatever
    /// its assigned count still holds; one listing any counts its assigned (each listed member live, the rest unlisted
    /// only past the read's room); none of each, empty.
    #[test]
    fn a_job_listing_no_member_is_empty_though_its_assigned_count_lags() {
        assert_eq!(
            (live(2, 0), empty(2, 0)),
            (0, true),
            "lagging: ended members alone"
        );
        assert_eq!((live(1, 0), empty(1, 0)), (0, true));
        assert_eq!(
            (live(2, 1), empty(2, 1)),
            (2, false),
            "a live member listed"
        );
        assert_eq!((live(0, 0), empty(0, 0)), (0, true));
    }

    /// HEX-WIN-1 (native plan step 3): a member that has ended leaves the job's id list, which
    /// `live_processes` counts, though the node still holds its process handle. Do: run `cmd /c exit 0`
    /// in a job, wait for its end, keep its handle, read the list with room for every id. Expect: the
    /// member's id isn't listed, and the read's assigned count is at least what it lists: Windows drops an ended
    /// member from the list before its count (HEX-WIN-1's lag, seen natively), so `live_processes` counts from the
    /// list ([`live`]).
    /// Another id may be listed: the console host Windows starts for a console program with no console,
    /// which ends after its client.
    #[test]
    fn an_ended_member_whose_handle_is_held_leaves_the_id_list() {
        let job = Job::create().unwrap();
        let system = std::env::var("SystemRoot").unwrap();
        let mut spawn = Spawn {
            command_line: format!("{system}\\System32\\cmd.exe /c exit 0")
                .encode_utf16()
                .chain([0])
                .collect(),
            environment: hex_core::job::environment(&[("SystemRoot".to_owned(), system)]),
            cwd: None,
            stdio: [nul(), nul(), nul()],
        };
        let started = create(&mut spawn, &[job.raw()]).unwrap();
        resume(&started.thread).unwrap();
        assert!(wait(&started.process, u32::MAX).unwrap());
        let (assigned, listed, ids) = id_list(&job, 64);
        assert!(
            !ids.contains(&(started.pid as usize)),
            "ended member {} still listed: {ids:?}",
            started.pid
        );
        // Its count may still hold it a moment (HEX-WIN-1's lag): the list is the live truth.
        assert!(assigned >= listed, "{assigned} assigned, {listed} listed");
        drop(started);
    }

    /// T-T7.1.2 (native plan step 8), the evidence for how a Windows box outlives its node: do make a named
    /// job without kill-on-close, start a member in it, open the job by name while a handle is held, then
    /// close every handle to the job. Expect: the open finds it while held; the member runs on after the
    /// last handle closes; and the name is gone then (Windows' object manager drops a temporary object's
    /// name with its last handle, though its processes still hold the job), so a node that held the only
    /// handle can't be followed by one that opens its box by name.
    #[test]
    fn a_named_jobs_name_lapses_with_its_last_handle_though_its_member_runs() {
        let name = format!("Local\\hex-test-{}-lapse", std::process::id());
        let job = Job::create_named(&name).unwrap();
        // Kill-on-close off for this check alone, so the member's running past the last handle shows the
        // name lapsing and not the job ending it.
        let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed_struct();
        ext.BasicLimitInformation.LimitFlags = 0;
        job.set(JobObjectExtendedLimitInformation, &ext).unwrap();
        let system = std::env::var("SystemRoot").unwrap();
        let mut spawn = Spawn {
            command_line: format!("{system}\\System32\\PING.EXE -n 600 127.0.0.1")
                .encode_utf16()
                .chain([0])
                .collect(),
            environment: hex_core::job::environment(&[("SystemRoot".to_owned(), system)]),
            cwd: None,
            stdio: [nul(), nul(), nul()],
        };
        let started = create(&mut spawn, &[job.raw()]).unwrap();
        resume(&started.thread).unwrap();
        let held = Job::open(&name).unwrap();
        assert!(
            held.is_some(),
            "a named job wasn't found by its name while held"
        );
        drop(held);
        drop(job);
        let running = !wait(&started.process, 0).unwrap();
        let after = Job::open(&name).unwrap();
        // No job is left to end it by: it is ended as any process is.
        std::process::Command::new("taskkill")
            .args(["/PID", &started.pid.to_string(), "/F"])
            .output()
            .unwrap();
        assert!(running, "the member ended with the job's last handle");
        assert!(
            after.is_none(),
            "the job was found by its name after its last handle closed"
        );
    }

    /// HEX-WIN-1 (native plan step 3): a box's live count is one read of its job, however many members
    /// it holds. Do: start two members that wait on stdin in a job, count the job's id-list reads one
    /// `live_processes` makes. Expect: one read, and a count of at least the two members (each console
    /// member may bring its console host).
    #[test]
    fn a_live_count_of_many_members_is_one_read() {
        let job = Job::create().unwrap();
        let system = std::env::var("SystemRoot").unwrap();
        let mut writers = Vec::new();
        let mut members = Vec::new();
        for _ in 0..2 {
            let (reader, writer) = std::io::pipe().unwrap();
            writers.push(writer);
            let mut spawn = Spawn {
                command_line: format!("{system}\\System32\\cmd.exe /c set /p x=")
                    .encode_utf16()
                    .chain([0])
                    .collect(),
                environment: hex_core::job::environment(&[(
                    "SystemRoot".to_owned(),
                    system.clone(),
                )]),
                cwd: None,
                stdio: [OwnedHandle::from(reader), nul(), nul()],
            };
            let started = create(&mut spawn, &[job.raw()]).unwrap();
            resume(&started.thread).unwrap();
            members.push(started);
        }
        ID_LIST_READS.with(|n| n.set(0));
        let live = job.live_processes().unwrap();
        let reads = ID_LIST_READS.with(Cell::get);
        drop(writers);
        for m in &members {
            assert!(wait(&m.process, u32::MAX).unwrap());
        }
        assert!(live >= 2, "two waiting members read as {live}");
        assert_eq!(reads, 1, "{reads} id-list reads for one live count");
    }

    /// Not a check on its own: run inside a box by
    /// `a_member_reaching_its_bound_is_counted_though_no_notification_comes`, it commits 1 MiB at a
    /// time until the box refuses (`HEX_TEST_COMMIT_TO_REFUSAL` set); otherwise it does nothing.
    #[test]
    fn commits_until_refused_when_asked() {
        if std::env::var_os("HEX_TEST_COMMIT_TO_REFUSAL").is_none() {
            return;
        }
        let mut held: Vec<Vec<u8>> = Vec::with_capacity(4096);
        while held.len() < 4096 {
            let mut chunk = Vec::new();
            if chunk.try_reserve_exact(1 << 20).is_err() {
                break;
            }
            chunk.resize(1 << 20, 1u8);
            held.push(chunk);
        }
    }

    /// HEX-RT-5 (native plan step 3): a member that reaches its box's memory bound and ends at once is
    /// counted, whether or not the job's memory notification is ever delivered. Under load Windows sets
    /// that notification seconds late, or never once the member's commit has fallen back (bench record
    /// windows-jupiter-20261105.txt entry 21, experiment e18). Do: make a 256 MiB box, clear its job's
    /// notification limit (52b298c's box set one and counted only its violation, so this is the
    /// notification that doesn't come), run a member that commits until the box refuses, wait for its
    /// end, read the box's usage. Expect: oom_kills at least 1, and the peak it reports within the bound.
    #[test]
    fn a_member_reaching_its_bound_is_counted_though_no_notification_comes() {
        let bound = 256u64 << 20;
        let bx = super::super::process::JobBox::create(
            hex_api::BoxId::new("sys-bound").unwrap(),
            &hex_api::Bounds {
                memory_max: Some(bound),
                ..hex_api::Bounds::default()
            },
            std::num::NonZeroU32::MIN,
        )
        .unwrap();
        let off: JOBOBJECT_NOTIFICATION_LIMIT_INFORMATION = zeroed_struct();
        // SAFETY: SetInformationJobObject reads one JOBOBJECT_NOTIFICATION_LIMIT_INFORMATION, alive for
        // the call, and sets it on the box's job, which `bx` owns for the test.
        let ok = unsafe {
            SetInformationJobObject(
                bx.job().unwrap().as_raw_handle() as HANDLE,
                JobObjectNotificationLimitInformation,
                (&off as *const JOBOBJECT_NOTIFICATION_LIMIT_INFORMATION).cast(),
                u32::try_from(size_of::<JOBOBJECT_NOTIFICATION_LIMIT_INFORMATION>()).unwrap(),
            )
        };
        assert_ne!(ok, 0, "switching the notification off: {}", last_error());
        let exe = std::env::current_exe().unwrap();
        let system = std::env::var("SystemRoot").unwrap();
        let spawn = Spawn {
            command_line: format!(
                "\"{}\" windows::sys::tests::commits_until_refused_when_asked --exact --test-threads=1",
                exe.display()
            )
            .encode_utf16()
            .chain([0])
            .collect(),
            environment: hex_core::job::environment(&[
                ("SystemRoot".to_owned(), system),
                ("HEX_TEST_COMMIT_TO_REFUSAL".to_owned(), "1".to_owned()),
            ]),
            cwd: None,
            stdio: [nul(), nul(), nul()],
        };
        let started = bx.exec(spawn).unwrap();
        assert!(wait(&started.process, u32::MAX).unwrap());
        let usage = bx.usage().unwrap();
        assert!(
            usage.memory_peak_bytes <= bound,
            "the box reports more committed than its bound: {usage:?}"
        );
        assert!(
            usage.oom_kills >= 1,
            "the bound reached isn't counted: {usage:?}"
        );
    }

    /// HEX-WIN-2 (native plan step 3): a box commits up to its memory bound itself, and a commit that
    /// would cross it is refused whole. Windows' job refuses such a commit whole and counts its ask in the
    /// job's peak (`PeakJobMemoryUsed`), so the peak passes the commit limit by the refused ask (bench
    /// record windows-jupiter-20261105.txt entry 22, experiment e20). Do: make a 256 MiB box, run a member
    /// that commits 1 MiB at a time until the box refuses, read the job's raw peak. Expect: past the bound
    /// (the member was refused at the bound, not short of it) by at most the refused 1 MiB ask and the
    /// 131,072 bytes e20 measured beside one.
    #[test]
    fn a_box_commits_up_to_its_bound_and_is_refused_past_it() {
        let bound = 256u64 << 20;
        let bx = super::super::process::JobBox::create(
            hex_api::BoxId::new("sys-up-to-bound").unwrap(),
            &hex_api::Bounds {
                memory_max: Some(bound),
                ..hex_api::Bounds::default()
            },
            std::num::NonZeroU32::MIN,
        )
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        let system = std::env::var("SystemRoot").unwrap();
        let spawn = Spawn {
            command_line: format!(
                "\"{}\" windows::sys::tests::commits_until_refused_when_asked --exact --test-threads=1",
                exe.display()
            )
            .encode_utf16()
            .chain([0])
            .collect(),
            environment: hex_core::job::environment(&[
                ("SystemRoot".to_owned(), system),
                ("HEX_TEST_COMMIT_TO_REFUSAL".to_owned(), "1".to_owned()),
            ]),
            cwd: None,
            stdio: [nul(), nul(), nul()],
        };
        let started = bx.exec(spawn).unwrap();
        assert!(wait(&started.process, u32::MAX).unwrap());
        let mut x: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed_struct();
        // SAFETY: QueryInformationJobObject writes at most one JOBOBJECT_EXTENDED_LIMIT_INFORMATION into
        // `x`, alive for the call, for the box's job, which `bx` owns for the test.
        let ok = unsafe {
            QueryInformationJobObject(
                bx.job().unwrap().as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                (&mut x as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap(),
                null_mut(),
            )
        };
        assert_ne!(ok, 0, "reading the job's peak: {}", last_error());
        let peak = u64::try_from(x.PeakJobMemoryUsed).unwrap();
        assert!(
            peak > bound,
            "refused short of its bound: peak {peak}, bound {bound}"
        );
        assert!(
            peak <= bound + (1 << 20) + 131_072,
            "past its bound by more than one refused ask: peak {peak}, bound {bound}"
        );
    }
}

#[cfg(test)]
mod report_handle_tests {
    use super::*;
    use std::io::{Read, Write};

    fn flags(handle: BorrowedHandle<'_>) -> u32 {
        let mut flags = 0;
        // SAFETY: a read-only handle query; the borrowed live handle and initialized output remain
        // valid throughout this synchronous call. No original inheritability is changed.
        let result = unsafe {
            windows_sys::Win32::Foundation::GetHandleInformation(handle.as_raw_handle(), &mut flags)
        };
        assert_ne!(
            result,
            0,
            "reading private report handle flags: {}",
            last_error()
        );
        flags
    }

    /// AC-T7.4.1: the report names the final private duplicate, leaves the source untouched, and
    /// closes the same pipe object after its last owned writer. No VM/job/controller is launched.
    #[test]
    fn report_duplicate_uses_its_own_value_and_preserves_original_flags() {
        let (mut reader, mut original) = std::io::pipe().unwrap();
        let original_flags = flags(original.as_handle());
        let inherited = inheritable_copy(original.as_handle()).unwrap();
        assert_eq!(flags(original.as_handle()), original_flags);
        assert_ne!(
            inherited.as_handle().as_raw_handle(),
            original.as_raw_handle()
        );
        assert_ne!(flags(inherited.as_handle()) & HANDLE_FLAG_INHERIT, 0);
        let words = shards_api::physical::report_arguments([0x11; 16], &inherited).unwrap();
        assert_eq!(words[0], "--gpu-report");
        assert_eq!(
            words[1],
            std::ffi::OsString::from(inherited.as_handle().as_raw_handle().addr().to_string())
        );
        assert_ne!(
            words[1],
            std::ffi::OsString::from(original.as_raw_handle().addr().to_string())
        );
        assert_eq!(words[2], "--gpu-report-scope");
        assert_eq!(words[3], std::ffi::OsString::from("11".repeat(16)));
        original.write_all(b"same private pipe").unwrap();
        drop(original);
        drop(inherited);
        let mut observed = String::new();
        reader.read_to_string(&mut observed).unwrap();
        assert_eq!(observed, "same private pipe");
    }
}

#[cfg(test)]
mod observer_socket_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::io::AsSocket;
    use std::time::Duration;

    #[test]
    fn observer_pair_preserves_finite_native_timeout_units() {
        assert_eq!(pair_timeout(Duration::ZERO).unwrap(), 0);
        assert_eq!(pair_timeout(Duration::from_nanos(1)).unwrap(), 1);
        assert_eq!(pair_timeout(Duration::from_millis(1)).unwrap(), 1);
        assert_eq!(
            pair_timeout(Duration::from_millis(2) + Duration::from_nanos(1)).unwrap(),
            3
        );
        assert_eq!(
            pair_timeout(Duration::from_secs(u64::MAX)).unwrap(),
            i32::MAX
        );
    }

    /// AC-T7.4.1: the same caller waits for the actual stdout/stderr fact without consuming it.
    /// This is a private loopback socket control, not AF_UNIX/GPU/VM or scope-release evidence.
    #[test]
    fn observer_pair_selects_data_or_eof_without_consuming_or_duplicating_sockets() {
        fn pair() -> (TcpStream, TcpStream) {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let peer = TcpStream::connect_timeout(
                &listener.local_addr().unwrap(),
                Duration::from_secs(10),
            )
            .unwrap();
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            (socket, peer)
        }
        let (mut stdout, stdout_peer) = pair();
        let (mut stderr, mut stderr_peer) = pair();
        assert_eq!(
            readable_pair(
                [Some(stdout.as_socket()), Some(stderr.as_socket())],
                Some(Duration::ZERO)
            )
            .unwrap(),
            [false, false]
        );
        assert!(
            readable_pair(
                [Some(stdout.as_socket()), Some(stdout.as_socket())],
                Some(Duration::ZERO)
            )
            .is_err()
        );
        assert!(
            readable_pair(
                [Some(stdout.as_socket()), Some(stderr.as_socket())],
                Some(Duration::MAX)
            )
            .is_err()
        );
        stderr_peer.write_all(b"R").unwrap();
        // A wait with no deadline (its caller's budget follows its guest: the review's C-9, (c2)) returns as an endpoint
        // is ready.
        assert_eq!(
            readable_pair([Some(stdout.as_socket()), Some(stderr.as_socket())], None).unwrap(),
            [false, true]
        );
        let mut byte = [0u8; 1];
        stderr.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"R");
        assert_eq!(
            readable_pair(
                [Some(stdout.as_socket()), Some(stderr.as_socket())],
                Some(Duration::ZERO)
            )
            .unwrap(),
            [false, false]
        );
        stdout_peer.shutdown(std::net::Shutdown::Write).unwrap();
        let ready = readable_pair(
            [Some(stdout.as_socket()), Some(stderr.as_socket())],
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        assert_eq!(ready, [true, false]);
        assert_eq!(stdout.read(&mut byte).unwrap(), 0);
        assert_eq!(
            readable_pair([None, Some(stderr.as_socket())], Some(Duration::ZERO)).unwrap(),
            [false, false],
            "completed stdout HUP must be unwatched"
        );
        stderr_peer.write_all(b"S").unwrap();
        assert_eq!(
            readable_pair(
                [None, Some(stderr.as_socket())],
                Some(Duration::from_secs(10))
            )
            .unwrap(),
            [false, true]
        );
        stderr.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"S");
        assert!(readable_pair([None, None], Some(Duration::ZERO)).is_err());
    }

    /// AC-T7.4.1 (the guest-cost producer's readers): do: commit 64 pages of read/write memory as Shards' WHP guest RAM
    /// is committed (`VirtualAlloc`, `MEM_RESERVE | MEM_COMMIT`), reserve 64 more uncommitted, and read this process from
    /// outside as the producer reads a VM process (`open_for_reading` by its own pid). Expect: the committed run one
    /// region, exactly the allocation, committed private read/write memory; the reserved run one region of its own,
    /// not committed read/write memory; and no process at a pid none has.
    #[test]
    fn a_committed_run_is_read_as_one_private_read_write_region() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let page = u64::from(page_size());
        let len = usize::try_from(64 * page).unwrap();
        // SAFETY: two fresh allocations of `len` bytes this test owns, each released below.
        let (committed, reserved) = unsafe {
            (
                VirtualAlloc(null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE),
                VirtualAlloc(null(), len, MEM_RESERVE, PAGE_READWRITE),
            )
        };
        assert!(
            !committed.is_null() && !reserved.is_null(),
            "{}",
            last_error()
        );
        let me = open_for_reading(std::process::id()).unwrap().unwrap();
        let at = |base: *mut std::ffi::c_void| u64::try_from(base.addr()).unwrap();
        let read = (region(&me, at(committed)), region(&me, at(reserved)));
        // SAFETY: the allocations above, each released once and not used after.
        unsafe {
            VirtualFree(committed, 0, MEM_RELEASE);
            VirtualFree(reserved, 0, MEM_RELEASE);
        }
        let size = 64 * page;
        assert_eq!(
            read.0.unwrap().unwrap(),
            Region {
                base: at(committed),
                size,
                private_rw: true
            }
        );
        assert_eq!(
            read.1.unwrap().unwrap(),
            Region {
                base: at(reserved),
                size,
                private_rw: false
            }
        );
        // Process ids are multiples of four (Windows Internals, 7th ed., ch. 3): none has an odd one.
        assert!(open_for_reading(1).unwrap().is_none());
    }

    /// AC-T7.4.1 (the guest-cost producer's readers): do: write a file one byte past its volume's allocation unit
    /// (`cluster_bytes` of its folder) and read it as the producer reads what a folder holds (`file_held`), then rename
    /// it, link it and read it again. Expect: two units allocated to it; the same file (its volume and index) by its
    /// new name and through its link; and nothing at its old name.
    #[test]
    fn a_file_is_held_in_its_volumes_units_and_named_by_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let unit = cluster_bytes(dir.path()).unwrap();
        let path = dir.path().join("held");
        std::fs::write(&path, vec![7u8; usize::try_from(unit + 1).unwrap()]).unwrap();
        let (file, bytes) = file_held(&path).unwrap().unwrap();
        assert_eq!(bytes, 2 * unit, "{unit}-byte units");
        let moved = dir.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        assert_eq!(file_held(&moved).unwrap().unwrap().0, file);
        let linked = dir.path().join("linked");
        std::fs::hard_link(&moved, &linked).unwrap();
        assert_eq!(file_held(&linked).unwrap().unwrap().0, file);
        assert_eq!(file_held(&path).unwrap(), None);
    }
}
