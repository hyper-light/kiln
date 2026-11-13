//! The macOS calls the `process` class makes that Rust can't make safely (CLAUDE.md §12 names this
//! file): libc binds them only as `unsafe` foreign functions and rustix wraps none of them. Each `unsafe`
//! block states the invariant that makes it sound. What rustix does wrap safely (`kqueue`, `kill`,
//! `waitid`, `waitpid`, `fcntl`) is called through it, outside this file.

// The macOS class's one place `unsafe` is allowed (CLAUDE.md §12); every block below carries its SAFETY
// comment.
#![allow(unsafe_code)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::Duration;

/// Mach absolute time's units (mach_timebase_info(3)): a tick is `numer / denom` nanoseconds, 125/3 on
/// Apple silicon and 1/1 on Intel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    // libc marks mach_timebase_info and its struct fields deprecated in favour of the mach2 crate, which
    // binds them identically; Hex takes them from libc, which it already links.
    #[allow(deprecated)]
    pub fn get() -> io::Result<Self> {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: mach_timebase_info writes the two fields of the struct it is given, which outlives the call.
        let kr = unsafe { libc::mach_timebase_info(&raw mut info) };
        if kr != 0 || info.numer == 0 || info.denom == 0 {
            return Err(io::Error::other(format!(
                "mach_timebase_info: {kr} ({}/{})",
                info.numer, info.denom
            )));
        }
        Ok(Self {
            numer: info.numer,
            denom: info.denom,
        })
    }

    /// `ticks` of Mach absolute time in nanoseconds, saturating past 2^64 (584 years).
    pub fn nanos(&self, ticks: u64) -> u64 {
        u128::from(ticks)
            .checked_mul(u128::from(self.numer))
            .and_then(|n| n.checked_div(u128::from(self.denom)))
            .map_or(u64::MAX, |n| u64::try_from(n).unwrap_or(u64::MAX))
    }
}

/// A process's use as `proc_pid_rusage(RUSAGE_INFO_V4)` reads it (`libproc.h`). Times are Mach absolute
/// time ([`Timebase`]): XNU fills them from the task's `rm_time_mach` (`osfmk/kern/task.c:6811`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rusage {
    /// When it started: with its pid, which process it is.
    pub started: u64,
    /// It has ended and waits to be reaped. XNU keeps an ended process's use, its footprint zeroed
    /// (`bsd/kern/kern_exit.c:2075-2080`), and reads a live one's exit time as 0
    /// (`bsd/kern/kern_resource.c:3455-3462`).
    pub ended: bool,
    pub footprint: u64,
    /// The highest footprint it reached.
    pub peak: u64,
    /// CPU it used itself, user and system.
    pub cpu: u64,
    /// CPU it ran its own code with (user time): one whose user time rose over a run had a core in it.
    /// Stopped, its time is whole: XNU totals a thread's time as it leaves its core.
    pub user: u64,
    /// CPU its reaped children used.
    pub child_cpu: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
}

/// Process `pid`'s use, or `None` when no process has that pid (`ESRCH`).
pub fn rusage(pid: i32) -> io::Result<Option<Rusage>> {
    let mut ri = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: for RUSAGE_INFO_V4 proc_pid_rusage writes at most one rusage_info_v4 into the buffer it is
    // given, which is one, alive for the call. The C prototype takes `rusage_info_t *`, a pointer to the
    // buffer, as libc declares it.
    let r = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            ri.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(e),
        };
    }
    // SAFETY: zeroed above and written by the kernel; every field is an integer, so any bytes are valid.
    let ri = unsafe { ri.assume_init() };
    Ok(Some(Rusage {
        started: ri.ri_proc_start_abstime,
        ended: ri.ri_proc_exit_abstime != 0,
        footprint: ri.ri_phys_footprint,
        peak: ri.ri_lifetime_max_phys_footprint,
        cpu: ri.ri_user_time.saturating_add(ri.ri_system_time),
        user: ri.ri_user_time,
        child_cpu: ri
            .ri_child_user_time
            .saturating_add(ri.ri_child_system_time),
        read_bytes: ri.ri_diskio_bytesread,
        written_bytes: ri.ri_diskio_byteswritten,
    }))
}

/// A positive group identity for a still existing process. Absence grants no cleanup authority.
pub(crate) fn group_of(pid: i32) -> io::Result<Option<i32>> {
    if pid <= 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // SAFETY: getpgid reads this positive PID's group and writes no caller memory.
    let group = unsafe { libc::getpgid(pid) };
    if group > 0 {
        return Ok(Some(group));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(None),
        _ => Err(error),
    }
}

/// The same native waiter must retain the direct child's zombie until durable Released. Querying
/// the disposition changes no handler, mask or process state; a foreign handler may reap children.
pub(crate) fn retained_children() -> io::Result<()> {
    let mut action = MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: null act requests only the current SIGCHLD action, written into one live sigaction.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sigaction succeeded and initialized the whole output value.
    let action = unsafe { action.assume_init() };
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other(
            "SIGCHLD does not retain children for their sole native waiter",
        ));
    }
    Ok(())
}

/// Where a process is in its life, as `proc_pidinfo(PROC_PIDTBSDINFO)` reads it: XNU copies the process's
/// `p_stat` into `pbi_status` (`bsd/kern/proc_info.c`), whose values are `sys/proc.h`'s (`SSTOP` 4, `SZOMB`
/// 5), and a stop signal sets `SSTOP` (`bsd/kern/kern_sig.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    /// Stopped by a signal: it runs again only once continued.
    Stopped,
    /// Ended, waiting to be reaped.
    Ended,
    /// Being made, runnable or asleep: each can run.
    Live,
}

/// Process `pid`'s run state, or `None` when no process has that pid (`ESRCH`).
pub fn run_state(pid: i32) -> io::Result<Option<Run>> {
    Ok(bsdinfo(pid)?.map(|info| match info.pbi_status {
        libc::SSTOP => Run::Stopped,
        libc::SZOMB => Run::Ended,
        _ => Run::Live,
    }))
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)` of `pid`, or `None` when no process has that pid (`ESRCH`).
fn bsdinfo(pid: i32) -> io::Result<Option<libc::proc_bsdinfo>> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .map_err(|_| io::Error::other("a proc_bsdinfo past an int's bytes"))?;
    // SAFETY: for PROC_PIDTBSDINFO proc_pidinfo writes at most `size` bytes, one proc_bsdinfo, into the
    // buffer it is given, which is one proc_bsdinfo, alive for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if n <= 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(e),
        };
    }
    if n != size {
        return Err(io::Error::other(format!(
            "proc_pidinfo(PROC_PIDTBSDINFO) of {pid} wrote {n} of {size} bytes"
        )));
    }
    // SAFETY: zeroed above and written whole by the kernel; every field is an integer or an array of them,
    // so any bytes are valid.
    Ok(Some(unsafe { info.assume_init() }))
}

/// The descriptors process `pid` holds, as `proc_pidinfo(PROC_PIDLISTFDS)` lists them: the read opens
/// none itself. XNU sizes the list from a call with no buffer (its open slots and 20 more) and then
/// writes at most the slots the buffer holds (`bsd/kern/proc_info.c` `proc_pidfdlist`).
#[cfg(test)]
pub(crate) fn descriptors(pid: i32) -> io::Result<Vec<i32>> {
    // SAFETY: with a null buffer and a size of 0, proc_pidinfo writes nothing and returns the bytes it needs.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(io::Error::last_os_error());
    }
    let each = size_of::<libc::proc_fdinfo>();
    let slots = usize::try_from(needed).map_err(io::Error::other)? / each;
    let mut list = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        slots
    ];
    let size = c_int::try_from(slots * each).map_err(io::Error::other)?;
    // SAFETY: proc_pidinfo writes at most `size` bytes, whole proc_fdinfo entries, into `list`, which
    // holds `slots` of them and outlives the call.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            list.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if wrote <= 0 {
        return Err(io::Error::last_os_error());
    }
    list.truncate(usize::try_from(wrote).map_err(io::Error::other)? / each);
    Ok(list.iter().map(|info| info.proc_fd).collect())
}

/// `PROC_FP_CLEXEC` (`bsd/sys/proc_info.h`): a descriptor's `proc_fileinfo.fi_status` bit for close-on-exec.
#[cfg(test)]
const PROC_FP_CLEXEC: u32 = 2;

/// The `proc_pidfdinfo` flavor that reads a descriptor of `kind` (`proc_fdinfo.proc_fdtype`), each answer
/// beginning with the descriptor's `proc_fileinfo` (`bsd/sys/proc_info.h`: `PROC_PIDFDVNODEINFO` 1,
/// `PROC_PIDFDSOCKETINFO` 3, `PROC_PIDFDPSEMINFO` 4, `PROC_PIDFDPSHMINFO` 5, `PROC_PIDFDPIPEINFO` 6,
/// `PROC_PIDFDKQUEUEINFO` 7, `PROC_PIDFDCHANNELINFO` 10), or none for a kind with no such flavor.
#[cfg(test)]
fn fd_flavor(kind: u32) -> Option<c_int> {
    let kind = c_int::try_from(kind).ok()?;
    Some(match kind {
        libc::PROX_FDTYPE_VNODE => 1,
        libc::PROX_FDTYPE_SOCKET => 3,
        libc::PROX_FDTYPE_PSEM => 4,
        libc::PROX_FDTYPE_PSHM => 5,
        libc::PROX_FDTYPE_PIPE => 6,
        libc::PROX_FDTYPE_KQUEUE => 7,
        libc::PROX_FDTYPE_CHANNEL => 10,
        _ => return None,
    })
}

/// The descriptors past stdio that process `pid` holds without close-on-exec, each with its kind
/// (`proc_fdinfo.proc_fdtype`): what a spawn that inherits would hand on. Each is read by its kind's
/// `proc_pidfdinfo` flavor, whose answer begins with its `proc_fileinfo` (`fi_openflags`, then `fi_status`,
/// whose `PROC_FP_CLEXEC` bit says close-on-exec). One of a kind with no flavor is listed: it can't be read
/// close-on-exec. One closed between the list and its read is passed over.
#[cfg(test)]
pub(crate) fn inheritable_descriptors(pid: i32) -> io::Result<Vec<(i32, u32)>> {
    // SAFETY: with a null buffer and a size of 0, proc_pidinfo writes nothing and returns the bytes it needs.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(io::Error::last_os_error());
    }
    let each = size_of::<libc::proc_fdinfo>();
    let slots = usize::try_from(needed).map_err(io::Error::other)? / each;
    let mut list = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        slots
    ];
    let size = c_int::try_from(slots * each).map_err(io::Error::other)?;
    // SAFETY: proc_pidinfo writes at most `size` bytes, whole proc_fdinfo entries, into `list`, which
    // holds `slots` of them and outlives the call.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            list.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if wrote <= 0 {
        return Err(io::Error::last_os_error());
    }
    list.truncate(usize::try_from(wrote).map_err(io::Error::other)? / each);
    let mut open = Vec::new();
    for entry in list.iter().filter(|entry| entry.proc_fd > 2) {
        let Some(flavor) = fd_flavor(entry.proc_fdtype) else {
            open.push((entry.proc_fd, entry.proc_fdtype));
            continue;
        };
        // Room for every flavor's answer (a socket's is the largest, under a page), aligned as each is.
        let mut answer = [0u64; 512];
        let room = c_int::try_from(size_of_val(&answer)).map_err(io::Error::other)?;
        // SAFETY: proc_pidfdinfo writes at most `room` bytes, one answer of `flavor`, into `answer`, which is
        // that many bytes, aligned for any of them, and alive for the call.
        let got = unsafe {
            libc::proc_pidfdinfo(
                pid,
                entry.proc_fd,
                flavor,
                answer.as_mut_ptr().cast::<c_void>(),
                room,
            )
        };
        if got <= 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(e);
        }
        // fi_openflags, then fi_status: the first word's two halves, in this host's byte order.
        let first = answer[0].to_ne_bytes();
        let status = u32::from_ne_bytes([first[4], first[5], first[6], first[7]]);
        if status & PROC_FP_CLEXEC == 0 {
            open.push((entry.proc_fd, entry.proc_fdtype));
        }
    }
    Ok(open)
}

/// What a process's threads are doing, as `proc_pidinfo(PROC_PIDTASKINFO)` reads them: XNU counts each
/// thread whose state has `TH_RUN`, on a core or waiting for one (`fill_taskprocinfo`,
/// `osfmk/kern/bsd_kern.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Task {
    /// A thread of it runs or waits for a core; with none, each is asleep or stopped.
    pub runnable: bool,
}

/// Process `pid`'s threads, or `None` when no process has that pid (`ESRCH`).
pub fn task(pid: i32) -> io::Result<Option<Task>> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>())
        .map_err(|_| io::Error::other("a proc_taskinfo past an int's bytes"))?;
    // SAFETY: for PROC_PIDTASKINFO proc_pidinfo writes at most `size` bytes, one proc_taskinfo, into the
    // buffer it is given, which is one proc_taskinfo, alive for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if n <= 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(e),
        };
    }
    if n != size {
        return Err(io::Error::other(format!(
            "proc_pidinfo(PROC_PIDTASKINFO) of {pid} wrote {n} of {size} bytes"
        )));
    }
    // SAFETY: zeroed above and written whole by the kernel; every field is an integer, so any bytes are valid.
    let info = unsafe { info.assume_init() };
    Ok(Some(Task {
        runnable: info.pti_numrunning > 0,
    }))
}

/// `PROC_PIDLISTTHREADS` (`sys/proc_info.h:732`): a process's thread handles, 8 bytes each, each one
/// `PROC_PIDTHREADINFO` reads.
const PROC_PIDLISTTHREADS: c_int = 6;
/// `PROC_PIDREGIONINFO` (`sys/proc_info.h:735`): the entry of a process's map at or after an address.
const PROC_PIDREGIONINFO: c_int = 7;
/// `PROC_PIDREGIONPATHINFO` (`sys/proc_info.h:738`): the same entry and the file it maps, if any.
const PROC_PIDREGIONPATHINFO: c_int = 8;

/// One thread of a process: its name (`pthread_setname_np`'s, empty for none) and the CPU it has used,
/// user and system, as `proc_pidinfo(PROC_PIDTHREADINFO)` reads them (`struct proc_threadinfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTime {
    pub name: Vec<u8>,
    pub cpu: Duration,
}

/// The threads of process `pid`, each by name and CPU, or `None` when no process has that pid (`ESRCH`).
/// `handles` is the room for the list (8 bytes a thread), sized by the caller to the most threads a task
/// may have (`kern.num_taskthreads`): one it fills is refused, as the list may have been cut short. A
/// thread that ends between the list and its read is passed over.
pub fn threads(pid: i32, handles: &mut [u64]) -> io::Result<Option<Vec<ThreadTime>>> {
    let bytes = c_int::try_from(std::mem::size_of_val(handles))
        .map_err(|_| io::Error::other("a thread list past an int's bytes"))?;
    // SAFETY: for PROC_PIDLISTTHREADS proc_pidinfo writes at most `bytes` bytes, whole 8-byte thread handles,
    // into the buffer it is given, which is `handles`, that many bytes long and alive for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDLISTTHREADS,
            0,
            handles.as_mut_ptr().cast::<c_void>(),
            bytes,
        )
    };
    if n <= 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(None),
            _ => Err(e),
        };
    }
    if n >= bytes {
        return Err(io::Error::other(format!(
            "{pid}'s threads fill the {} handles given",
            handles.len()
        )));
    }
    let count = usize::try_from(n)
        .map_err(|_| io::Error::other("a negative thread list"))?
        .checked_div(size_of::<u64>())
        .ok_or_else(|| io::Error::other("a thread handle of no bytes"))?;
    let listed = handles
        .get(..count)
        .ok_or_else(|| io::Error::other("a thread list past its room"))?;
    let size = c_int::try_from(size_of::<libc::proc_threadinfo>())
        .map_err(|_| io::Error::other("a proc_threadinfo past an int's bytes"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    for &handle in listed {
        let mut info = MaybeUninit::<libc::proc_threadinfo>::zeroed();
        // SAFETY: for PROC_PIDTHREADINFO proc_pidinfo writes at most `size` bytes, one proc_threadinfo, into
        // the buffer it is given, which is one proc_threadinfo, alive for the call; `handle` is a value.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTHREADINFO,
                handle,
                info.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if got <= 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ESRCH) => continue,
                _ => return Err(e),
            }
        }
        if got != size {
            return Err(io::Error::other(format!(
                "proc_pidinfo(PROC_PIDTHREADINFO) of {pid} wrote {got} of {size} bytes"
            )));
        }
        // SAFETY: zeroed above and written whole by the kernel; every field is an integer or an array of
        // them, so any bytes are valid.
        let info = unsafe { info.assume_init() };
        let name: Vec<u8> = info
            .pth_name
            .iter()
            .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
            .take_while(|b| *b != 0)
            .collect();
        out.push(ThreadTime {
            name,
            cpu: Duration::from_nanos(info.pth_user_time.saturating_add(info.pth_system_time)),
        });
    }
    Ok(Some(out))
}

/// `struct proc_regioninfo` (`sys/proc_info.h:166`), which libc doesn't bind.
#[repr(C)]
struct RegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    behavior: u32,
    user_wired_count: u32,
    user_tag: u32,
    pages_resident: u32,
    pages_shared_now_private: u32,
    pages_swapped_out: u32,
    pages_dirtied: u32,
    ref_count: u32,
    shadow_depth: u32,
    share_mode: u32,
    private_pages_resident: u32,
    shared_pages_resident: u32,
    obj_id: u32,
    depth: u32,
    address: u64,
    size: u64,
}

/// `struct proc_regionwithpathinfo` (`sys/proc_info.h:331`).
#[repr(C)]
struct RegionWithPath {
    info: RegionInfo,
    file: libc::vnode_info_path,
}

/// `PROC_REGION_SUBMAP` (`sys/proc_info.h:190`): an entry that is a submap (the shared region's), no
/// memory of the process's own.
const PROC_REGION_SUBMAP: u32 = 1;

/// One entry of a process's memory map: where it is, how long, its pages resident and its pages compressed
/// or swapped out (in the map's pages), its VM tag (0 for memory no allocator tagged), the most access it
/// may ever be given (`VM_PROT_*`: none for a reservation of address space alone), whether it is a submap,
/// and whether it maps a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub address: u64,
    pub size: u64,
    pub resident: u64,
    pub swapped_out: u64,
    pub tag: u32,
    pub max_protection: u32,
    pub submap: bool,
    /// It maps a file (`PROC_PIDREGIONPATHINFO` names a vnode): `None` where read without its path.
    pub file: Option<bool>,
}

/// The entry of process `pid`'s map at or after `address`, as `proc_pidinfo(PROC_PIDREGIONINFO)` reads it,
/// or `None` past the map's last entry (`EINVAL`). A map is read of a live process: one gone is an error.
pub fn region(pid: i32, address: u64) -> io::Result<Option<Region>> {
    let mut info = MaybeUninit::<RegionInfo>::zeroed();
    let size = c_int::try_from(size_of::<RegionInfo>())
        .map_err(|_| io::Error::other("a proc_regioninfo past an int's bytes"))?;
    // SAFETY: for PROC_PIDREGIONINFO proc_pidinfo writes at most `size` bytes, one proc_regioninfo, into the
    // buffer it is given, which is one RegionInfo (that struct, field for field), alive for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDREGIONINFO,
            address,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if !read_whole(n, size, "PROC_PIDREGIONINFO", pid)? {
        return Ok(None);
    }
    // SAFETY: zeroed above and written whole by the kernel; every field is an integer, so any bytes are valid.
    let info = unsafe { info.assume_init() };
    Ok(Some(Region {
        address: info.address,
        size: info.size,
        resident: u64::from(info.pages_resident),
        swapped_out: u64::from(info.pages_swapped_out),
        tag: info.user_tag,
        max_protection: info.max_protection,
        submap: info.flags & PROC_REGION_SUBMAP != 0,
        file: None,
    }))
}

/// [`region`], with whether the entry maps a file (`proc_pidinfo(PROC_PIDREGIONPATHINFO)`: XNU zeroes the
/// path of an entry with no vnode).
pub fn region_with_path(pid: i32, address: u64) -> io::Result<Option<Region>> {
    let mut info = MaybeUninit::<RegionWithPath>::zeroed();
    let size = c_int::try_from(size_of::<RegionWithPath>())
        .map_err(|_| io::Error::other("a proc_regionwithpathinfo past an int's bytes"))?;
    // SAFETY: for PROC_PIDREGIONPATHINFO proc_pidinfo writes at most `size` bytes, one
    // proc_regionwithpathinfo, into the buffer it is given, which is one RegionWithPath (that struct, field
    // for field), alive for the call.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDREGIONPATHINFO,
            address,
            info.as_mut_ptr().cast::<c_void>(),
            size,
        )
    };
    if !read_whole(n, size, "PROC_PIDREGIONPATHINFO", pid)? {
        return Ok(None);
    }
    // SAFETY: zeroed above and written whole by the kernel; every field is an integer or an array of them,
    // so any bytes are valid.
    let info = unsafe { info.assume_init() };
    let path = info.file.vip_path.iter().flatten().next().copied();
    Ok(Some(Region {
        address: info.info.address,
        size: info.info.size,
        resident: u64::from(info.info.pages_resident),
        swapped_out: u64::from(info.info.pages_swapped_out),
        tag: info.info.user_tag,
        max_protection: info.info.max_protection,
        submap: info.info.flags & PROC_REGION_SUBMAP != 0,
        file: Some(path.is_some_and(|first| first != 0)),
    }))
}

/// Whether a map read wrote its whole struct: false past the map's last entry (`EINVAL`), an error for any
/// other failure or a short write.
fn read_whole(n: c_int, size: c_int, flavor: &str, pid: i32) -> io::Result<bool> {
    if n <= 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EINVAL) => Ok(false),
            _ => Err(e),
        };
    }
    if n != size {
        return Err(io::Error::other(format!(
            "proc_pidinfo({flavor}) of {pid} wrote {n} of {size} bytes"
        )));
    }
    Ok(true)
}

/// `PROC_PGRP_ONLY` (`sys/proc_info.h:52`): `proc_listpids` lists one process group.
const PROC_PGRP_ONLY: u32 = 2;
/// `PROC_UID_ONLY` (`sys/proc_info.h:54`): `proc_listpids` lists one user's processes. A box's user,
/// the only marker a member inherits and can't drop (native-platforms.md §3), so this finds every member
/// — including one that daemonized (`setsid`) out of its process group (the box helper's listing, step 2).
const PROC_UID_ONLY: u32 = 4;

/// The members of process group `pgid`, written over `pids` up to its capacity, its zombies included
/// (XNU walks `zombproc` after `allproc`, `bsd/kern/proc_info.c:470-473`). Returns whether the listing may
/// have been cut short: it filled `pids`.
pub fn group(pgid: i32, pids: &mut Vec<i32>) -> io::Result<bool> {
    let group = u32::try_from(pgid)
        .map_err(|_| io::Error::other(format!("{pgid} isn't a process group")))?;
    list_pids(PROC_PGRP_ONLY, group, pids)
}

/// The processes of user `uid`, written over `pids` up to its capacity (the box helper's membership
/// listing, step 2); returns whether the listing may have been cut short.
pub fn user_pids(uid: u32, pids: &mut Vec<i32>) -> io::Result<bool> {
    list_pids(PROC_UID_ONLY, uid, pids)
}

/// `proc_listpids` for one `flavor` and its `arg`, written over `pids` up to its capacity.
fn list_pids(flavor: u32, arg: u32, pids: &mut Vec<i32>) -> io::Result<bool> {
    pids.clear();
    let room = pids.capacity();
    let bytes = room
        .checked_mul(size_of::<i32>())
        .and_then(|b| c_int::try_from(b).ok())
        .ok_or_else(|| {
            io::Error::other("a pid listing's buffer is past what proc_listpids takes")
        })?;
    // SAFETY: proc_listpids writes at most `bytes` bytes of pids into the buffer it is given, which has
    // room for `room` i32s: exactly `bytes`.
    let n = unsafe { libc::proc_listpids(flavor, arg, pids.as_mut_ptr().cast::<c_void>(), bytes) };
    let written = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    let count = written.checked_div(size_of::<i32>()).unwrap_or(0).min(room);
    // SAFETY: the kernel wrote the first `count` pids, and `count` is at most the capacity.
    unsafe { pids.set_len(count) };
    Ok(count == room)
}

/// This process's CPU so far, user and system over all its threads (`getrusage(RUSAGE_SELF)`): the macOS
/// node's own cost, for the class bench to charge a box's watcher (bench/native/macos) and for
/// observability (CLAUDE.md §10).
pub fn process_cpu() -> io::Result<Duration> {
    let ru = own_rusage()?;
    let secs = |tv: libc::timeval| {
        let s = u64::try_from(tv.tv_sec).unwrap_or(0);
        let us = u64::try_from(tv.tv_usec).unwrap_or(0);
        Duration::from_secs(s).saturating_add(Duration::from_micros(us))
    };
    Ok(secs(ru.ru_utime).saturating_add(secs(ru.ru_stime)))
}

fn own_rusage() -> io::Result<libc::rusage> {
    let mut ru = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes one `rusage` into the buffer given, which is zeroed and outlives the call.
    let r = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zeroed and written by the kernel on success; every field is an integer, so any bytes are valid.
    Ok(unsafe { ru.assume_init() })
}

/// Managed CPU credit cannot turn a malformed/overflowing required counter into free CPU room.
pub(super) fn managed_process_cpu() -> io::Result<Duration> {
    let ru = own_rusage()?;
    checked_process_cpu(ru.ru_utime, ru.ru_stime)
}

fn checked_process_cpu(user: libc::timeval, system: libc::timeval) -> io::Result<Duration> {
    let part = |value: libc::timeval| -> io::Result<Duration> {
        let seconds = u64::try_from(value.tv_sec).map_err(|_| io::ErrorKind::InvalidData)?;
        let micros = u64::try_from(value.tv_usec).map_err(|_| io::ErrorKind::InvalidData)?;
        // timeval's fractional field is microseconds in one second, not an unbounded counter.
        if micros >= 1_000_000 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Duration::from_secs(seconds)
            .checked_add(Duration::from_micros(micros))
            .ok_or_else(|| io::ErrorKind::InvalidData.into())
    };
    part(user)?
        .checked_add(part(system)?)
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

#[cfg(test)]
mod managed_cpu_counter_tests {
    use super::*;

    /// AC-T7.4.1: required user/system CPU values cannot manufacture free credit on malformed input.
    #[test]
    fn required_cpu_counters_refuse_bad_fields_and_sum_both_thread_totals() {
        let valid = libc::timeval {
            tv_sec: 1,
            tv_usec: 750_000,
        };
        let other = libc::timeval {
            tv_sec: 2,
            tv_usec: 250_000,
        };
        assert_eq!(
            checked_process_cpu(valid, other).unwrap(),
            Duration::from_secs(4)
        );
        for malformed in [
            libc::timeval {
                tv_sec: -1,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: 0,
                tv_usec: -1,
            },
            libc::timeval {
                tv_sec: 0,
                tv_usec: 1_000_000,
            },
        ] {
            for (user, system) in [(valid, malformed), (malformed, other)] {
                assert_eq!(
                    checked_process_cpu(user, system).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
            }
        }
    }
}

/// The system's 1-minute load average (`getloadavg(3)`), for a bench to record the host's load with its
/// figures (CLAUDE.md §7) and for observability (§10); `None` if the kernel won't say.
pub fn loadavg() -> Option<f64> {
    let mut avg = [0f64; 3];
    // SAFETY: getloadavg writes at most `avg.len()` doubles into the buffer given, which has 3.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    if n >= 1 { avg.first().copied() } else { None }
}

/// The most processes this kernel holds at once (`kern.maxproc`): no box can list more members.
pub fn max_processes() -> io::Result<usize> {
    sysctl_count(c"kern.maxproc")
}

/// An empty process listing with room for every process the kernel holds (`kern.maxproc`), so one
/// listing call fills it; refused naming `kern.maxproc` when it can't be read, never sized by a guess.
pub fn listing() -> io::Result<Vec<i32>> {
    listing_from(max_processes())
}

/// A listing sized from a read of `kern.maxproc` ([`listing`] with the read given, so its refusal is
/// testable).
pub fn listing_from(max: io::Result<usize>) -> io::Result<Vec<i32>> {
    let max = max.map_err(|e| io::Error::new(e.kind(), format!("reading kern.maxproc: {e}")))?;
    let mut pids = Vec::new();
    pids.try_reserve_exact(max).map_err(io::Error::from)?;
    Ok(pids)
}

/// The most processes one uid may hold (`kern.maxprocperuid`, `bsd/kern/kern_resource.c:169-170`): a
/// box-user box's process cap when its spec names none, each box having a uid of its own.
pub fn max_processes_per_uid() -> io::Result<usize> {
    sysctl_count(c"kern.maxprocperuid")
}

/// The most descriptors one process may open (`kern.maxfilesperproc`): what caps its `RLIMIT_NOFILE`.
pub fn max_files_per_process() -> io::Result<usize> {
    sysctl_count(c"kern.maxfilesperproc")
}

/// The most threads one process may run (`kern.num_taskthreads`).
pub fn max_threads_per_task() -> io::Result<usize> {
    sysctl_count(c"kern.num_taskthreads")
}

/// A kernel count read by name (an `int` sysctl), as read now: an administrator may change it.
fn sysctl_count(name: &std::ffi::CStr) -> io::Result<usize> {
    let mut v: c_int = 0;
    let mut len = size_of::<c_int>();
    // SAFETY: sysctlbyname reads the NUL-terminated name and writes at most `len` bytes, one c_int, into `v`,
    // and the new length into `len`; all three outlive the call, and nothing is set (newp null).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut v).cast::<c_void>(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(v)
        .map_err(|_| io::Error::other(format!("{} reads {v}", name.to_string_lossy())))
}

/// A string sysctl (`hw.model`, `kern.bootsessionuuid`), read from the kernel itself, so no program runs
/// to print it: sized by a call with no buffer, then read whole, its terminating NUL dropped.
pub fn sysctl_string(name: &std::ffi::CStr) -> io::Result<String> {
    let mut len = 0usize;
    // SAFETY: with a null buffer sysctlbyname writes only the bytes it needs into `len`; it reads the
    // NUL-terminated name; both outlive the call, and nothing is set (newp null).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(io::Error::other)?;
    bytes.resize(len, 0u8);
    let mut wrote = len;
    // SAFETY: sysctlbyname writes at most `wrote` bytes, the buffer's length, into `bytes`, and the length
    // it wrote into `wrote`; all three outlive the call, and nothing is set (newp null).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast::<c_void>(),
            &raw mut wrote,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    bytes.truncate(wrote);
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

/// The longest login name the system takes, `sysconf(_SC_LOGIN_NAME_MAX)` (XNU's `MAXLOGNAME`): the most
/// a directory record's name holds.
pub fn login_name_max() -> io::Result<usize> {
    // SAFETY: sysconf reads its integer argument alone and writes no caller memory.
    let longest = unsafe { libc::sysconf(libc::_SC_LOGIN_NAME_MAX) };
    if longest <= 0 {
        return Err(io::Error::other(
            "sysconf(_SC_LOGIN_NAME_MAX) names no longest login name",
        ));
    }
    usize::try_from(longest).map_err(io::Error::other)
}

/// A numeric sysctl (`hw.memsize`, `hw.physicalcpu`, `kern.hv_support`, `vm.page_free_count`), whether
/// the kernel keeps it in 4 bytes or 8: the node's facts (`hex-node`'s `facts`).
pub fn sysctl_number(name: &std::ffi::CStr) -> io::Result<u64> {
    let mut v = [0u8; 8];
    let mut len = v.len();
    // SAFETY: sysctlbyname reads the NUL-terminated name and writes at most `len` bytes (8) into `v`,
    // and the length it wrote into `len`; all three outlive the call, and nothing is set (newp null).
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            v.as_mut_ptr().cast::<c_void>(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    match (len, v) {
        (4, [a, b, c, d, ..]) => Ok(u64::from(u32::from_ne_bytes([a, b, c, d]))),
        (8, v) => Ok(u64::from_ne_bytes(v)),
        _ => Err(io::Error::other(format!(
            "{} is {len} bytes, not a number",
            name.to_string_lossy()
        ))),
    }
}

/// The most bytes execve takes of a program's arguments and environment, their pointers included
/// (`kern.argmax`, XNU's `NCARGS`: bsd/kern/kern_exec.c:578, :6546-6549; docs/research/runtime/exec-size-bounds.md).
pub fn arg_max() -> io::Result<usize> {
    sysctl_count(c"kern.argmax")
}

/// The QoS a box's processes run at from their first instruction, as its declared CPU weight asks (a box's
/// CPU share follows its `cpu.weight` on Linux): passed to every process they start (XNU's task policy
/// copies the clamp to a forked child, `osfmk/kern/task_policy.c:746`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// No clamp: the default weight, the QoS the node's own programs run at.
    Unclamped,
    /// `QOS_CLASS_BACKGROUND` for a weight under the default, which also puts the box in darwin
    /// background (lowest CPU priority, I/O throttled; `task_policy.c:946-950`).
    Background,
}

/// `QOS_CLASS_BACKGROUND` (`sys/qos.h`, macOS 26 SDK).
const QOS_CLASS_BACKGROUND: u32 = 0x09;

// SAFETY: declared as `pthread/spawn.h` (macOS 26 SDK, line 64) declares it: it sets the spawn attribute's
// QoS clamp and returns 0 or an errno.
unsafe extern "C" {
    fn posix_spawnattr_set_qos_class_np(attr: *mut libc::posix_spawnattr_t, qos: u32) -> c_int;
}

// SAFETY: pinned libsyscall's spawn implementation declares this initialized file-actions pointer
// and scalar descriptor. PSFA_INHERIT clears the child's CLOEXEC and marks that exact fd inherited.
unsafe extern "C" {
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: c_int,
    ) -> c_int;
}

// SAFETY: declared as `spawn_private.h` declares it ("All memory limits are in MiB."): it writes a
// per-process jetsam memory limit into the spawn attribute and returns 0 or an errno. The symbol is
// exported from libsystem_kernel.dylib (`_posix_spawnattr_setjetsam_ext`); the macOS 26 CommandLineTools
// SDK ships neither its prototype nor the flags, so Hex declares them (the `_np` form is absent). Rust
// can express neither the foreign call nor the raw attribute pointer safely.
unsafe extern "C" {
    fn posix_spawnattr_setjetsam_ext(
        attr: *mut libc::posix_spawnattr_t,
        flags: i16,
        priority: c_int,
        memlimit_active: c_int,
        memlimit_inactive: c_int,
    ) -> c_int;
}

/// Jetsam spawn-attribute flags (xnu `bsd/sys/spawn_internal.h`, `psa_jetsam_flags`): SET marks the
/// attribute as carrying jetsam settings; ACTIVE_FATAL and INACTIVE_FATAL make the per-process footprint
/// limit fatal whether the process is active or inactive, so the kernel SIGKILLs it the instant it
/// crosses the limit — 0 overshoot, rootless (bench/native/macos/jetsam_probe.c, record
/// jetsam-probe-20261101T2052Z.txt).
const POSIX_SPAWN_JETSAM_SET: u16 = 0x8000;
const POSIX_SPAWN_JETSAM_MEMLIMIT_ACTIVE_FATAL: u16 = 0x04;
const POSIX_SPAWN_JETSAM_MEMLIMIT_INACTIVE_FATAL: u16 = 0x08;

/// A middling jetsam band for a box member. A *fatal* per-process limit kills the process when it crosses
/// the limit regardless of band; the band only orders kills under genuine system memory pressure. 40 is
/// the band the fatal kill was confirmed with (bench/native/macos/jetsam_probe.c; step 2's
/// boxuser_probe.c record boxuser-rootless-e532df3).
const JETSAM_PRIORITY: c_int = 40;

/// Arms `attr` with a fatal per-process memory cap of `mib` MiB, active and inactive, so the kernel
/// SIGKILLs the spawned program if its physical footprint crosses the cap (native plan step 1's `_ext`
/// backstop). `attr` is an initialized `posix_spawnattr_t`; `mib` is the box's whole bound in MiB.
fn set_jetsam_cap(attr: *mut libc::posix_spawnattr_t, mib: c_int) -> io::Result<()> {
    let flags = POSIX_SPAWN_JETSAM_SET
        | POSIX_SPAWN_JETSAM_MEMLIMIT_ACTIVE_FATAL
        | POSIX_SPAWN_JETSAM_MEMLIMIT_INACTIVE_FATAL;
    // 0x800c has the sign bit set, past a positive i16, so it is built from its bytes, not a narrowing cast.
    let flags = i16::from_ne_bytes(flags.to_ne_bytes());
    // SAFETY: posix_spawnattr_setjetsam_ext reads the flags, priority and two memory limits given and
    // writes them into the initialized attribute object `attr` points at, which outlives the call. Both
    // limits are the same `mib`, so the cap is fatal whether the process is active or inactive.
    errno(unsafe { posix_spawnattr_setjetsam_ext(attr, flags, JETSAM_PRIORITY, mib, mib) })
}

/// A mebibyte, the jetsam cap's unit.
const MIB: u64 = 1 << 20;

/// The per-process jetsam cap for a box bounded to `memory_max` bytes, in MiB: rounded DOWN, so the cap
/// in bytes never exceeds the bound, and floored to 1 MiB — the SPI's unit is MiB, `Bounds::check`
/// already refuses a bound under 1 MiB, and a 0 MiB cap would mean no limit. `None` when the MiB value is
/// past what the SPI's `int` takes (~2 PiB), where the member is left uncapped and the watcher's own rule
/// bounds the box (never a silent miscap, CLAUDE.md §3).
pub fn jetsam_cap_mib(memory_max: u64) -> Option<c_int> {
    let mib = memory_max.checked_div(MIB).unwrap_or(0).max(1);
    c_int::try_from(mib).ok()
}

/// The cap in bytes the kernel enforces for a box bounded to `memory_max`: [`jetsam_cap_mib`] times a
/// mebibyte. The watcher reads a dead member's peak footprint against this to tell a fatal-cap kill from
/// an ordinary exit (native plan step 1's `_ext` backstop).
pub fn jetsam_cap_bytes(memory_max: u64) -> Option<u64> {
    let mib = jetsam_cap_mib(memory_max)?;
    u64::try_from(mib).ok()?.checked_mul(MIB)
}

/// A program to spawn into a box: every string and descriptor its spawn uses.
pub struct Spawn {
    /// The paths to execute, in order: the program as named when it holds a `/`, else its name in each
    /// `PATH` folder, as execvp(3) tries them.
    pub paths: Vec<CString>,
    pub argv: Vec<CString>,
    pub envp: Vec<CString>,
    pub cwd: Option<CString>,
    /// stdin, stdout and stderr: each is dup2'd onto 0, 1 and 2, and nothing else is inherited.
    pub stdio: [OwnedFd; 3],
    /// Only the actual final launcher-owned descriptors are carried beyond stdio. Their numeric
    /// argv suffixes are rendered after transfer/duplication; neither is a parent-side raw value.
    pub report: Option<OwnedFd>,
    pub lifeline: Option<OwnedFd>,
    /// The box's whole memory bound in MiB, when it has one: each spawned member is armed with a fatal
    /// per-process cap at it (native plan step 1's `_ext` backstop), so the kernel bounds a single-member
    /// box with no overshoot. `None` leaves the member uncapped (the watcher's rule bounds the box).
    pub jetsam_mib: Option<c_int>,
}

/// An errno as posix_spawn returns it.
fn errno(r: c_int) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(r))
    }
}

/// A `posix_spawnattr_t`, destroyed when dropped.
struct Attr(libc::posix_spawnattr_t);

impl Drop for Attr {
    fn drop(&mut self) {
        // SAFETY: initialized by posix_spawnattr_init and destroyed once, here.
        unsafe {
            libc::posix_spawnattr_destroy(&raw mut self.0);
        }
    }
}

/// A `posix_spawn_file_actions_t`, destroyed when dropped.
struct Actions(libc::posix_spawn_file_actions_t);

impl Drop for Actions {
    fn drop(&mut self) {
        // SAFETY: initialized by posix_spawn_file_actions_init and destroyed once, here.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.0);
        }
    }
}

/// Spawns `s` into process group `pgroup` (0: a group of its own, led by the program) under `clamp`, its
/// signal mask emptied, every signal's disposition at its default, and no descriptor inherited but its
/// stdio (`POSIX_SPAWN_CLOEXEC_DEFAULT`; XNU marks a dup2's target inherited, `bsd/kern/kern_exec.c:3193-3199`).
/// posix_spawn makes the child whole in the kernel, so it is safe from a multi-threaded process. Returns its
/// pid; a path that isn't there, or isn't executable, is skipped for the next, as execvp(3) does.
pub fn spawn(s: &Spawn, pgroup: i32, clamp: Clamp) -> io::Result<i32> {
    spawn_using(s, pgroup, clamp, Start::Running)
}

/// The same native group/jetsam launch, stopped by the kernel before its first instruction.
/// Its native owner publishes pid/birth durably before SIGCONT; no post-spawn placement occurs.
pub(crate) fn spawn_suspended(s: &Spawn, pgroup: i32, clamp: Clamp) -> io::Result<i32> {
    spawn_using(s, pgroup, clamp, Start::Suspended)
}

#[derive(Clone, Copy)]
enum Start {
    Running,
    Suspended,
}

fn spawn_using(s: &Spawn, pgroup: i32, clamp: Clamp, start: Start) -> io::Result<i32> {
    let prepared = PreparedSpawn::new(s, pgroup, clamp, start)?;
    let mut refused = None;
    for path in &s.paths {
        let mut pid: libc::pid_t = 0;
        // SAFETY: path, argv and envp are NUL-terminated strings, the lists NULL-terminated, all alive for the
        // call; the attribute and action objects were initialized above. posix_spawn writes the pid.
        let r = unsafe {
            libc::posix_spawn(
                &raw mut pid,
                path.as_ptr(),
                &raw const prepared.actions.0,
                &raw const prepared.attr.0,
                prepared.argv.as_ptr(),
                prepared.envp.as_ptr(),
            )
        };
        match r {
            0 => return Ok(pid),
            libc::ENOENT | libc::ENOTDIR => {}
            libc::EACCES => refused = Some(r),
            e => return Err(io::Error::from_raw_os_error(e)),
        }
    }
    Err(io::Error::from_raw_os_error(
        refused.unwrap_or(libc::ENOENT),
    ))
}

/// One checked attribute/action preparation serves ordinary spawn and the opt-in measurement.
struct PreparedSpawn {
    // Preserve the old local teardown order: pointer vectors, file actions, attributes.
    envp: Vec<*mut c_char>,
    argv: Vec<*mut c_char>,
    actions: Actions,
    attr: Attr,
}

impl PreparedSpawn {
    fn new(s: &Spawn, pgroup: i32, clamp: Clamp, start: Start) -> io::Result<Self> {
        let mut attr = Attr(std::ptr::null_mut());
        // SAFETY: posix_spawnattr_init initializes the attribute object it is given; `attr` destroys it once.
        errno(unsafe { libc::posix_spawnattr_init(&raw mut attr.0) })?;
        let mut flags = libc::POSIX_SPAWN_SETPGROUP
            | libc::POSIX_SPAWN_SETSIGMASK
            | libc::POSIX_SPAWN_SETSIGDEF
            | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
        if matches!(start, Start::Suspended) {
            // XNU kern_exec.c applies START_SUSPENDED before returning the new thread to user mode.
            // This is Apple's public spawn.h flag, not a signal sent after the child starts running.
            flags |= libc::POSIX_SPAWN_START_SUSPENDED;
        }
        let flags =
            i16::try_from(flags).map_err(|_| io::Error::other("posix_spawn flags past a short"))?;
        let mut none: libc::sigset_t = 0;
        let mut all: libc::sigset_t = 0;
        // SAFETY: each call takes the attribute object initialized above and reads or writes only the signal
        // sets given, which outlive the calls.
        unsafe {
            errno(libc::posix_spawnattr_setflags(&raw mut attr.0, flags))?;
            errno(libc::posix_spawnattr_setpgroup(&raw mut attr.0, pgroup))?;
            errno(libc::sigemptyset(&raw mut none))?;
            errno(libc::sigfillset(&raw mut all))?;
            errno(libc::posix_spawnattr_setsigmask(
                &raw mut attr.0,
                &raw const none,
            ))?;
            errno(libc::posix_spawnattr_setsigdefault(
                &raw mut attr.0,
                &raw const all,
            ))?;
            if clamp == Clamp::Background {
                errno(posix_spawnattr_set_qos_class_np(
                    &raw mut attr.0,
                    QOS_CLASS_BACKGROUND,
                ))?;
            }
        }
        // Arm the box's whole memory bound as a fatal per-process cap, so the kernel bounds this member with
        // no overshoot (native plan step 1's `_ext` backstop). The watcher tracks it as armed.
        if let Some(mib) = s.jetsam_mib {
            set_jetsam_cap(&raw mut attr.0, mib)?;
        }
        let mut actions = Actions(std::ptr::null_mut());
        // SAFETY: initializes the file-actions object it is given; `actions` destroys it once.
        errno(unsafe { libc::posix_spawn_file_actions_init(&raw mut actions.0) })?;
        for (fd, target) in s.stdio.iter().zip(0..) {
            // SAFETY: records a dup2 of a descriptor open for the whole spawn (`s` owns it) onto `target`.
            errno(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &raw mut actions.0,
                    fd.as_fd().as_raw_fd(),
                    target,
                )
            })?;
        }
        for fd in [&s.report, &s.lifeline].into_iter().flatten() {
            let raw = fd.as_fd().as_raw_fd();
            if raw < 3 {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            // SAFETY: fd is owned by Spawn throughout launch. This explicit action clears CLOEXEC only
            // in the child and marks the same numeric target inherited (pinned exec_handle_file_actions
            // PSFA_INHERIT); the parent's CLOEXEC stays set and no unused pipe end is carried.
            errno(unsafe { posix_spawn_file_actions_addinherit_np(&raw mut actions.0, raw) })?;
        }
        if let Some(cwd) = &s.cwd {
            // SAFETY: records a chdir to a NUL-terminated path the action list copies. The `_np` form is the one
            // every macOS from 10.15 has; macOS 26 adds the unsuffixed name.
            errno(unsafe {
                libc::posix_spawn_file_actions_addchdir_np(&raw mut actions.0, cwd.as_ptr())
            })?;
        }
        let argv = nul_terminated(&s.argv)?;
        let envp = nul_terminated(&s.envp)?;
        Ok(Self {
            attr,
            actions,
            argv,
            envp,
        })
    }
}

/// `strings` as the NULL-terminated list of pointers exec takes; it borrows them.
fn nul_terminated(strings: &[CString]) -> io::Result<Vec<*mut c_char>> {
    let count = strings
        .len()
        .checked_add(1)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let mut pointers = Vec::new();
    pointers.try_reserve_exact(count).map_err(io::Error::from)?;
    pointers.extend(strings.iter().map(|value| value.as_ptr().cast_mut()));
    pointers.push(std::ptr::null_mut());
    Ok(pointers)
}

/// What woke a [`Kqueue`] wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A subscribed signal was delivered; the native waiter still checks only its owned child.
    Signal { number: i32 },
    /// Its timer fired.
    Timer,
    /// Another thread triggered it.
    User,
    /// Process `pid`, watched with tag `tag`, forked; `exited` when it has also ended since (the two
    /// coalesce into one event until it is taken).
    Fork { pid: i32, tag: u64, exited: bool },
    /// Process `pid`, watched with tag `tag`, ended with wait `status` (`NOTE_EXITSTATUS`). The node is
    /// the member's parent, so the kernel delivers the status in the event: the watcher reads a member's
    /// death — a jetsam fatal-cap `SIGKILL` among them — without racing the member's own reaper.
    Exit { pid: i32, tag: u64, status: i32 },
    /// A descriptor watched with tag `tag` ([`Kqueue::fd`]) has bytes to read, or its peer has gone
    /// (`ended`, `EV_EOF`): the node socket's connections and listener (`hex_node::serve::macos`).
    Readable { tag: u64, ended: bool },
    /// A descriptor watched with tag `tag` has room to write, or its peer has gone (`ended`).
    Writable { tag: u64, ended: bool },
}

/// `NOTE_EXITSTATUS` (`sys/event.h`, macOS 26 SDK): a watched child's exit event carries its wait status
/// in the event's `data`. Valid for a child process (the node spawned the box's members).
const NOTE_EXITSTATUS: u32 = 0x0400_0000;

/// The [`Kqueue`] timer's and user event's idents.
const TIMER: usize = 1;
const USER: usize = 2;

/// Most events one [`Kqueue::wait`] takes: the most one wait met in the bench, 28 (one a member of its
/// largest box, all killed at once; bench/native/macos/records/watcher-readcost-events-a7d13c6-20261106T165609Z.txt),
/// and the [`TIMER`] and [`USER`] events, which can be pending beside them, so one call takes such a box's
/// events (the lead's call 2, 2026-11-06). A watched process's forks and end coalesce into one event until
/// it is taken (XNU sets `EV_CLEAR` on them, `bsd/kern/kern_event.c:1139`), so a larger box's further events
/// wait for the next call, none lost.
pub const EVENTS: usize = 28 + 2;

/// A kqueue (kqueue(2)): timers, a user event other threads trigger, and processes' forks and ends.
#[derive(Debug)]
pub struct Kqueue(OwnedFd);

impl Kqueue {
    /// A kqueue with its user event added (`EVFILT_USER`, cleared as it is read).
    pub fn new() -> io::Result<Self> {
        let kq = Self(rustix::event::kqueue::kqueue().map_err(io::Error::from)?);
        kq.change(&[kevent(
            USER,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
            0,
        )])?;
        Ok(kq)
    }

    /// Wakes a thread waiting on it.
    pub fn trigger(&self) -> io::Result<()> {
        self.change(&[kevent(USER, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0, 0)])
    }

    /// Observe child completion without changing a signal handler or mask. XNU posts SIGCHLD
    /// after SZOMB, whereas NOTE_EXIT can precede waitable zombie publication.
    pub fn child_completions(&self) -> io::Result<()> {
        let signal = usize::try_from(libc::SIGCHLD).map_err(|_| io::ErrorKind::InvalidData)?;
        self.change(&[kevent(
            signal,
            libc::EVFILT_SIGNAL,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
            0,
            0,
        )])
    }

    /// Watches each of `pids` for its forks and its end with the end's wait status (`EVFILT_PROC` with
    /// `NOTE_FORK | NOTE_EXIT | NOTE_EXITSTATUS`, `bsd/kern/kern_event.c:1099-1155`), tagged `tag`. XNU
    /// attaches `NOTE_EXITSTATUS` only for the process's parent (`EACCES` otherwise, :1116-1133), so each
    /// of `pids` must be this process's child (the box's members); [`Kqueue::watch_end`] watches another's.
    /// Returns those that have ended already (`ESRCH`), which can't be watched.
    pub fn watch(&self, pids: &[i32], tag: u64) -> io::Result<Vec<i32>> {
        self.watch_with(
            pids,
            tag,
            libc::NOTE_FORK | libc::NOTE_EXIT | NOTE_EXITSTATUS,
        )
    }

    /// One native controller registration uses fixed stack storage after its prepared launch.
    /// True means ESRCH; it supplies no process or whole-scope terminal fact by itself.
    pub fn watch_one(&self, pid: i32, tag: u64, child: bool) -> io::Result<bool> {
        let ident = usize::try_from(pid)
            .ok()
            .filter(|pid| *pid != 0)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let flags = libc::NOTE_EXIT | libc::NOTE_FORK | if child { NOTE_EXITSTATUS } else { 0 };
        let changes = [kevent(
            ident,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_RECEIPT,
            flags,
            0,
            tag,
        )];
        let mut receipts = [kevent(0, 0, 0, 0, 0, 0)];
        if self.kevent(&changes, &mut receipts, Some(Duration::ZERO))? != receipts.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let receipt = receipts.first().ok_or(io::ErrorKind::UnexpectedEof)?;
        let code = c_int::try_from(receipt.data).map_err(|_| io::ErrorKind::InvalidData)?;
        match code {
            0 => Ok(false),
            libc::ESRCH => Ok(true),
            code => Err(io::Error::from_raw_os_error(code)),
        }
    }

    /// Watches process `pid`, which needn't be this process's child (a box-user program is the helper's),
    /// for its end alone (`NOTE_EXIT`, which XNU attaches to any process), tagged 0. True when it has ended
    /// already (`ESRCH`), so no event will come.
    pub fn watch_end(&self, pid: i32) -> io::Result<bool> {
        Ok(self.watch_with(&[pid], 0, libc::NOTE_EXIT)?.contains(&pid))
    }

    fn watch_with(&self, pids: &[i32], tag: u64, fflags: u32) -> io::Result<Vec<i32>> {
        let mut gone = Vec::new();
        // One registration call per EVENTS processes: the same batch the waits take.
        for chunk in pids.chunks(EVENTS) {
            let changes: Vec<libc::kevent> = chunk
                .iter()
                .filter_map(|pid| usize::try_from(*pid).ok())
                .map(|pid| {
                    kevent(
                        pid,
                        libc::EVFILT_PROC,
                        libc::EV_ADD | libc::EV_RECEIPT,
                        fflags,
                        0,
                        tag,
                    )
                })
                .collect();
            let mut receipts = vec![kevent(0, 0, 0, 0, 0, 0); changes.len()];
            let n = self.kevent(&changes, &mut receipts, Some(Duration::ZERO))?;
            for r in receipts.iter().take(n) {
                match c_int::try_from(r.data).unwrap_or(c_int::MAX) {
                    0 => {}
                    libc::ESRCH => {
                        if let Ok(pid) = i32::try_from(r.ident) {
                            gone.push(pid);
                        }
                    }
                    e => return Err(io::Error::from_raw_os_error(e)),
                }
            }
        }
        Ok(gone)
    }

    /// Watches descriptor `fd` under `tag` for reads (`read`) and room to write (`write`), level-triggered
    /// (`EVFILT_READ` and `EVFILT_WRITE` without `EV_CLEAR`: reported while true, kevent(2)), each filter
    /// added and then enabled or disabled, so changing what `fd` is watched for is this call again. The
    /// caller keeps `fd` open while it is watched; closing it takes it out (kqueue(2)).
    pub fn fd(&self, fd: impl AsFd, tag: u64, read: bool, write: bool) -> io::Result<()> {
        let ident = usize::try_from(fd.as_fd().as_raw_fd())
            .map_err(|_| io::Error::other("a descriptor isn't one"))?;
        let on = |on: bool| {
            libc::EV_ADD
                | if on {
                    libc::EV_ENABLE
                } else {
                    libc::EV_DISABLE
                }
        };
        self.change(&[
            kevent(ident, libc::EVFILT_READ, on(read), 0, 0, tag),
            kevent(ident, libc::EVFILT_WRITE, on(write), 0, 0, tag),
        ])
    }

    /// Stops watching descriptor `fd`, if it was watched.
    pub fn forget_fd(&self, fd: impl AsFd) -> io::Result<()> {
        let Ok(ident) = usize::try_from(fd.as_fd().as_raw_fd()) else {
            return Ok(());
        };
        for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
            match self.change(&[kevent(ident, filter, libc::EV_DELETE, 0, 0, 0)]) {
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                other => other?,
            }
        }
        Ok(())
    }

    /// Stops watching process `pid`, if it was watched.
    pub fn unwatch(&self, pid: i32) -> io::Result<()> {
        let Ok(ident) = usize::try_from(pid) else {
            return Ok(());
        };
        match self.change(&[kevent(ident, libc::EVFILT_PROC, libc::EV_DELETE, 0, 0, 0)]) {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            other => other,
        }
    }

    /// Waits for events, after arming a one-shot timer for `timer` marked `NOTE_CRITICAL` ("system does
    /// minimal timer coalescing", `sys/event.h:308`; a plain sleep ran about six times long,
    /// bench/native/macos/RESULT.md) when one is given, and for at most `timeout` when one is given. Takes
    /// at most [`EVENTS`] events into `out`, which the caller keeps so a wait allocates nothing; more wait
    /// for the next call. A process watched with [`Kqueue::watch`] that forked is reported as a fork even
    /// if it has ended since, so its new member isn't missed.
    pub fn wait(
        &self,
        timer: Option<Duration>,
        timeout: Option<Duration>,
        out: &mut Vec<Event>,
    ) -> io::Result<()> {
        out.try_reserve(EVENTS.saturating_sub(out.len()))
            .map_err(io::Error::from)?;
        out.clear();
        let armed;
        let changes: &[libc::kevent] = match timer {
            Some(t) => {
                // At least a microsecond, so a timer is never armed for 0, which fires at once.
                let us = t.as_micros().clamp(1, u128::from(u32::MAX));
                armed = [kevent(
                    TIMER,
                    libc::EVFILT_TIMER,
                    libc::EV_ADD | libc::EV_ONESHOT,
                    libc::NOTE_USECONDS | libc::NOTE_CRITICAL,
                    isize::try_from(us).unwrap_or(isize::MAX),
                    0,
                )];
                &armed
            }
            None => &[],
        };
        let mut events = [kevent(0, 0, 0, 0, 0, 0); EVENTS];
        let n = match self.kevent(changes, &mut events, timeout) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
            other => other?,
        };
        for e in events.iter().take(n) {
            if e.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(
                    c_int::try_from(e.data).unwrap_or(libc::EINVAL),
                ));
            }
            let tag = u64::try_from(e.udata.addr()).unwrap_or(u64::MAX);
            let pid = i32::try_from(e.ident).unwrap_or(0);
            let event = match e.filter {
                libc::EVFILT_SIGNAL => Event::Signal { number: pid },
                libc::EVFILT_TIMER => Event::Timer,
                libc::EVFILT_USER => Event::User,
                libc::EVFILT_PROC if e.fflags & libc::NOTE_FORK != 0 => Event::Fork {
                    pid,
                    tag,
                    exited: e.fflags & libc::NOTE_EXIT != 0,
                },
                // The wait status rides in `data` (NOTE_EXITSTATUS); the terminating signal is its low 7
                // bits, so a jetsam fatal-cap SIGKILL is told apart here without reading the zombie.
                libc::EVFILT_PROC => Event::Exit {
                    pid,
                    tag,
                    status: i32::try_from(e.data).unwrap_or(0),
                },
                libc::EVFILT_READ => Event::Readable {
                    tag,
                    ended: e.flags & libc::EV_EOF != 0,
                },
                libc::EVFILT_WRITE => Event::Writable {
                    tag,
                    ended: e.flags & libc::EV_EOF != 0,
                },
                _ => continue,
            };
            out.push(event);
        }
        Ok(())
    }

    fn change(&self, changes: &[libc::kevent]) -> io::Result<()> {
        self.kevent(changes, &mut [], Some(Duration::ZERO))
            .map(|_| ())
    }

    fn kevent(
        &self,
        changes: &[libc::kevent],
        events: &mut [libc::kevent],
        timeout: Option<Duration>,
    ) -> io::Result<usize> {
        // A timeout no deadline can hold (the clock can't add it to now: a run's wait for its program's end)
        // bounds nothing, so it waits for the event alone, rather than hand the kernel a time it refuses
        // (EINVAL for a timespec past its range).
        let ts = timeout
            .filter(|t| std::time::Instant::now().checked_add(*t).is_some())
            .map(|t| libc::timespec {
                tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
                tv_nsec: libc::c_long::from(t.subsec_nanos()),
            });
        let nchanges = c_int::try_from(changes.len())
            .map_err(|_| io::Error::other("too many kevent changes"))?;
        let nevents = c_int::try_from(events.len())
            .map_err(|_| io::Error::other("too many kevent events"))?;
        // SAFETY: kevent reads `nchanges` events from `changes` and writes at most `nevents` into `events`,
        // both slices alive for the call, and reads the timeout if one is given. No event carries a pointer:
        // a tag is a number in `udata`, never dereferenced.
        let n = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                changes.as_ptr(),
                nchanges,
                events.as_mut_ptr(),
                nevents,
                ts.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
            )
        };
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }
}

/// The effective user id of the process at the other end of local socket `fd` (getpeereid(3)): who
/// connected to the node's socket, which admits this user and root (`hex_node::serve::macos`).
pub fn peer_euid(fd: impl AsFd) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: getpeereid reads the descriptor and writes one uid_t and one gid_t through the two pointers,
    // each to a local alive for the call.
    let r = unsafe { libc::getpeereid(fd.as_fd().as_raw_fd(), &raw mut uid, &raw mut gid) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// A failed/invalid clock observation cannot grant a new native deadline.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UptimeFailure {
    #[error("CLOCK_UPTIME_RAW failed: {0}")]
    Native(#[source] io::Error),
    #[error("CLOCK_UPTIME_RAW returned negative/unrepresentable seconds {0}")]
    Seconds(libc::time_t),
    #[error("CLOCK_UPTIME_RAW returned invalid nanoseconds {0}")]
    Nanoseconds(libc::c_long),
    #[error("CLOCK_UPTIME_RAW duration overflow")]
    Overflow,
}

/// Rust 1.99.1 Instant uses CLOCK_UPTIME_RAW on Apple. Use the SAME native clock to
/// transfer an absolute expiry between node and helper; no cross-clock rebase or tolerance.
pub(crate) fn uptime_raw() -> Result<Duration, UptimeFailure> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the fixed native clock is read-only. The fully initialized timespec is live and
    // writable for exactly its ABI extent; errno is read only after the call reports failure.
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &raw mut value) } != 0 {
        return Err(UptimeFailure::Native(io::Error::last_os_error()));
    }
    checked_uptime(value)
}
fn checked_uptime(value: libc::timespec) -> Result<Duration, UptimeFailure> {
    let seconds = u64::try_from(value.tv_sec).map_err(|_| UptimeFailure::Seconds(value.tv_sec))?;
    if !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(UptimeFailure::Nanoseconds(value.tv_nsec));
    }
    let nanos =
        u64::try_from(value.tv_nsec).map_err(|_| UptimeFailure::Nanoseconds(value.tv_nsec))?;
    Duration::from_secs(seconds)
        .checked_add(Duration::from_nanos(nanos))
        .ok_or(UptimeFailure::Overflow)
}

#[cfg(test)]
mod uptime_controls {
    use super::*;
    /// AC-T7.4.1/R01: invalid native counters cannot normalize into a later transferred deadline.
    #[test]
    fn invalid_native_uptime_never_extends_an_expiry() {
        assert!(matches!(
            checked_uptime(libc::timespec {
                tv_sec: -1,
                tv_nsec: 0
            }),
            Err(UptimeFailure::Seconds(-1))
        ));
        for nanos in [-1, 1_000_000_000] {
            assert!(matches!(
                checked_uptime(libc::timespec {
                    tv_sec: 1,
                    tv_nsec: nanos
                }),
                Err(UptimeFailure::Nanoseconds(_))
            ));
        }
        assert_eq!(
            checked_uptime(libc::timespec {
                tv_sec: 2,
                tv_nsec: 999_999_999
            })
            .unwrap(),
            Duration::new(2, 999_999_999)
        );
        assert_eq!(
            checked_uptime(libc::timespec {
                tv_sec: 0,
                tv_nsec: 0
            })
            .unwrap(),
            Duration::ZERO
        );
    }
}

/// A peer PID query failure is not an absent or released native scope.
#[derive(Debug, thiserror::Error)]
pub enum PeerPidFailure {
    #[error("LOCAL_PEERPID failed: {0}")]
    Native(#[source] io::Error),
    #[error("LOCAL_PEERPID length cannot represent pid_t")]
    LengthUnrepresentable,
    #[error("LOCAL_PEERPID returned {got} bytes, expected {expected}")]
    Length {
        got: libc::socklen_t,
        expected: libc::socklen_t,
    },
    #[error("LOCAL_PEERPID returned nonpositive PID {0}")]
    Nonpositive(libc::pid_t),
}

/// The connected peer socket's last PID, not a birth or ownership proof (XNU f6217f891ac0,
/// bsd/kern/uipc_usrreq.c uipc_ctloutput). Join with same UID, recorded birth, live helper and the
/// exact scope/commitment before any control; a disconnected peer is refused by the native query.
pub fn peer_pid(fd: impl AsFd) -> Result<libc::pid_t, PeerPidFailure> {
    let expected = libc::socklen_t::try_from(size_of::<libc::pid_t>())
        .map_err(|_| PeerPidFailure::LengthUnrepresentable)?;
    let mut length = expected;
    let mut pid: libc::pid_t = 0;
    // SAFETY: fd is borrowed for the call; getsockopt reads no option input and writes at most
    // length (the initialized size of pid) bytes to pid, and the actual length to its live scalar.
    let result = unsafe {
        libc::getsockopt(
            fd.as_fd().as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast::<c_void>(),
            &raw mut length,
        )
    };
    if result != 0 {
        return Err(PeerPidFailure::Native(io::Error::last_os_error()));
    }
    if length != expected {
        return Err(PeerPidFailure::Length {
            got: length,
            expected,
        });
    }
    if pid <= 0 {
        return Err(PeerPidFailure::Nonpositive(pid));
    }
    Ok(pid)
}

// XNU f6217f891ac0 mach/message.h and mach/task_info.h: audit_token_t is eight
// unsigned words; TASK_AUDIT_TOKEN is flavor 15 and returns that many natural_t words.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct AuditToken {
    words: [u32; 8],
}
const TASK_AUDIT_TOKEN: libc::task_flavor_t = 15;

// SAFETY: declarations match pinned mach/task.h, mach/mach_port.h and libproc.h.
// All pointers name live caller-owned scalars; proc_signal_with_audittoken returns errno directly.
unsafe extern "C" {
    fn mach_task_self() -> libc::mach_port_t;
    fn task_name_for_pid(
        task: libc::mach_port_t,
        pid: libc::pid_t,
        name: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

// SAFETY: pinned libproc.h declares one audit_token_t input and signal; the direct return is errno.
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_signal_with_audittoken(token: *mut AuditToken, signal: c_int) -> c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum TokenFailure {
    #[error("native member identity is not positive")]
    InvalidIdentity,
    #[error("{operation} failed with errno {code:?}, kind {kind:?}")]
    Io {
        operation: &'static str,
        code: Option<i32>,
        kind: io::ErrorKind,
    },
    #[error("native member birth changed or is unavailable (observed {observed:?})")]
    Changed { observed: Option<u64> },
    #[error("{operation} failed with Mach status {code}")]
    Mach {
        operation: &'static str,
        code: libc::kern_return_t,
    },
    #[error("task audit query failed {query}, and owned right release failed {release}")]
    QueryAndRelease {
        query: libc::kern_return_t,
        release: libc::kern_return_t,
    },
    #[error("native task audit token returned {got} words, expected {expected}")]
    Length {
        got: libc::mach_msg_type_number_t,
        expected: libc::mach_msg_type_number_t,
    },
    #[error("task-name query returned no owned right")]
    NoRight,
}

fn token_io(operation: &'static str, error: io::Error) -> TokenFailure {
    TokenFailure::Io {
        operation,
        code: error.raw_os_error(),
        kind: error.kind(),
    }
}

impl TokenFailure {
    /// The kernel resolved the token to no process: `proc_signal_with_audittoken`'s ESRCH, for a pid with
    /// no process or one whose pid version, which exec changes, isn't the token's (XNU `proc_info.c`).
    pub(super) fn unresolved(&self) -> bool {
        matches!(
            self,
            Self::Io {
                operation: "proc_signal_with_audittoken",
                code: Some(libc::ESRCH),
                ..
            }
        )
    }
}

/// A genuine native incarnation retained in the SAME Hold entry; no public token constructor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct MemberToken {
    pid: i32,
    birth: u64,
    token: AuditToken,
}
impl std::fmt::Debug for MemberToken {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output
            .debug_struct("MemberToken")
            .field("pid", &self.pid)
            .field("birth", &self.birth)
            .finish_non_exhaustive()
    }
}
impl MemberToken {
    pub(super) fn pid(&self) -> i32 {
        self.pid
    }
    pub(super) fn birth(&self) -> u64 {
        self.birth
    }
}

fn member_birth(pid: i32, expected: u64) -> Result<(), TokenFailure> {
    let actual = rusage(pid).map_err(|error| token_io("reading native member birth", error))?;
    if actual.is_none_or(|usage| usage.started != expected || usage.ended) {
        return Err(TokenFailure::Changed {
            observed: actual.map(|usage| usage.started),
        });
    }
    Ok(())
}

/// Capture only a genuine task-name-derived token, joining the expected PID/birth on both sides.
/// A task-name port has no task-control rights; the acquired send right is released on every path.
pub(super) fn capture_member(pid: i32, birth: u64) -> Result<MemberToken, TokenFailure> {
    join_member_token(pid, birth, || member_birth(pid, birth), || task_audit(pid))
}

fn join_member_token(
    pid: i32,
    birth: u64,
    mut observe: impl FnMut() -> Result<(), TokenFailure>,
    capture: impl FnOnce() -> Result<AuditToken, TokenFailure>,
) -> Result<MemberToken, TokenFailure> {
    if pid <= 0 || birth == 0 {
        return Err(TokenFailure::InvalidIdentity);
    }
    observe()?;
    let token = capture()?;
    observe()?;
    Ok(MemberToken { pid, birth, token })
}

fn task_audit(pid: i32) -> Result<AuditToken, TokenFailure> {
    let mut port: libc::mach_port_t = 0;
    // SAFETY: mach_task_self names this process's task; task_name_for_pid writes only its live
    // out-parameter. Success transfers one task-name send right, owned until deallocated below.
    let status = unsafe { task_name_for_pid(mach_task_self(), pid, &raw mut port) };
    if status != 0 {
        return Err(TokenFailure::Mach {
            operation: "task_name_for_pid",
            code: status,
        });
    }
    if port == 0 {
        return Err(TokenFailure::NoRight);
    }
    let mut token = AuditToken { words: [0; 8] };
    let expected: libc::mach_msg_type_number_t = 8;
    let mut count = expected;
    // SAFETY: the owned name port authorizes TASK_AUDIT_TOKEN. The initialized output is exactly
    // eight natural_t words, and count describes its whole writable extent for the MIG call.
    let query = unsafe {
        libc::task_info(
            port,
            TASK_AUDIT_TOKEN,
            (&raw mut token).cast::<i32>(),
            &raw mut count,
        )
    };
    // SAFETY: this is the one task-name send right acquired above; no early return or other
    // operation transfers/releases it. Its name is not reused by this owner after this call.
    let release = unsafe { mach_port_deallocate(mach_task_self(), port) };
    if release != 0 {
        return Err(TokenFailure::QueryAndRelease { query, release });
    }
    if query != 0 {
        return Err(TokenFailure::Mach {
            operation: "TASK_AUDIT_TOKEN",
            code: query,
        });
    }
    if count != expected {
        return Err(TokenFailure::Length {
            got: count,
            expected,
        });
    }
    Ok(token)
}

#[cfg(test)]
pub(super) fn simulated_member(pid: i32, birth: u64, version: u32) -> MemberToken {
    // A pure fake-kernel receipt for adversarial state tests; never sent to a native syscall.
    join_member_token(
        pid,
        birth,
        || Ok(()),
        || {
            Ok(AuditToken {
                words: [version; 8],
            })
        },
    )
    .unwrap()
}

#[cfg(test)]
mod token_controls {
    use super::*;
    use std::cell::Cell;

    /// AC-T7.4.1: a reused/unavailable birth at either observation never produces a native-control
    /// receipt. Count capture calls to show an invalid initial fact cannot even acquire a task right.
    #[test]
    fn member_capture_rejects_reused_birth_before_and_after_genuine_capture() {
        for failed_at in [0, 1] {
            let observations = Cell::new(0);
            let captures = Cell::new(0);
            let receipt = join_member_token(
                71,
                73,
                || {
                    let at = observations.get();
                    observations.set(at + 1);
                    if at == failed_at {
                        Err(TokenFailure::Changed { observed: Some(79) })
                    } else {
                        Ok(())
                    }
                },
                || {
                    captures.set(captures.get() + 1);
                    Ok(AuditToken { words: [83; 8] })
                },
            );
            assert!(matches!(
                receipt,
                Err(TokenFailure::Changed { observed: Some(79) })
            ));
            assert_eq!(captures.get(), failed_at);
        }
        let observations = Cell::new(0);
        assert!(matches!(
            join_member_token(
                0,
                73,
                || {
                    observations.set(1);
                    Ok(())
                },
                || Ok(AuditToken { words: [0; 8] })
            ),
            Err(TokenFailure::InvalidIdentity)
        ));
        assert_eq!(observations.get(), 0);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum MemberSignal {
    Stop,
    Continue,
    Kill,
}

/// The kernel resolves the token's incarnation and checks signal authority while retaining the
/// exact proc reference. ESRCH/permission/changed identities remain typed failures, never PID fallback.
pub(super) fn signal_member(
    member: &MemberToken,
    signal: MemberSignal,
) -> Result<(), TokenFailure> {
    let mut token = member.token;
    let signal = match signal {
        MemberSignal::Stop => libc::SIGSTOP,
        MemberSignal::Continue => libc::SIGCONT,
        MemberSignal::Kill => libc::SIGKILL,
    };
    // SAFETY: token is a copy of the genuine kernel-produced token in the immutable native Hold
    // receipt. libproc reads its full fixed ABI extent and forwards only the allowed signal above.
    let error = unsafe { proc_signal_with_audittoken(&raw mut token, signal) };
    if error != 0 {
        return Err(token_io(
            "proc_signal_with_audittoken",
            io::Error::from_raw_os_error(error),
        ));
    }
    Ok(())
}

/// An `EV_SET` (`sys/event.h`), its tag carried in `udata` as a number.
fn kevent(
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
    tag: u64,
) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata: std::ptr::without_provenance_mut(usize::try_from(tag).unwrap_or(usize::MAX)),
    }
}

/// Opt-in trusted-entry measurement primitives. No ordinary runtime path calls these.
#[cfg(feature = "native-trusted-entry-measurement")]
pub(crate) mod trusted_entry_native {
    use std::ffi::{OsString, c_char, c_int};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt as _;
    use std::time::{Duration, Instant};

    use super::{
        Clamp, Kqueue, PreparedSpawn, Spawn, Start, UptimeFailure, kevent, rusage, uptime_raw,
    };

    #[derive(Debug, thiserror::Error)]
    pub(crate) enum Error {
        #[error("{0}")]
        Invalid(&'static str),
        #[error("{operation}: {source}")]
        Io {
            operation: &'static str,
            #[source]
            source: io::Error,
        },
        #[error(transparent)]
        Clock(#[from] UptimeFailure),
        #[error("the original native expiry was reached")]
        Deadline,
    }
    fn io(operation: &'static str, source: io::Error) -> Error {
        Error::Io { operation, source }
    }
    fn count(argc: c_int, maximum: usize) -> Result<usize, Error> {
        let count = usize::try_from(argc).map_err(|_| Error::Invalid("negative argc"))?;
        let pointers = count
            .checked_add(1)
            .and_then(|n| n.checked_mul(size_of::<*const c_char>()))
            .ok_or(Error::Invalid("argv pointer width overflow"))?;
        if count == 0 || pointers > maximum || maximum > isize::MAX.unsigned_abs() {
            return Err(Error::Invalid("argc exceeds the native exec budget"));
        }
        Ok(count)
    }
    // The process loader supplies a NUL-terminated argument. Stop at its first NUL;
    // never scan beyond the remaining native exec budget or after that terminator.
    unsafe fn argument<'a>(pointer: *const c_char, maximum: usize) -> Result<&'a [u8], Error> {
        if pointer.is_null() {
            return Err(Error::Invalid("NULL argument before argc"));
        }
        for at in 0..maximum {
            // SAFETY: called only on the loader's live initial argv, before any application
            // argument mutation. Its argument is readable up to/including its first NUL;
            // the loop stops there and the caller bounds every offset by isize::MAX.
            if unsafe { pointer.add(at).read() } == 0 {
                // SAFETY: the preceding bounded reads establish these exact initialized bytes.
                return Ok(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), at) });
            }
        }
        Err(Error::Invalid(
            "argument has no NUL within the native exec budget",
        ))
    }
    /// Called by the FIRST main branch, before std::env::args_os copies every argument.
    /// A nonmatching selector performs no argument allocation. These are the same
    /// initialized dyld accessors pinned Rust uses; no borrowed pointer leaves this call.
    pub(crate) fn arguments(selector: &[u8]) -> Result<Option<Vec<OsString>>, Error> {
        let maximum = super::arg_max().map_err(|e| io("reading native ARG_MAX", e))?;
        // SAFETY: libdyld initializes both globals before main. This first dispatch does
        // not mutate them and precedes every other application/library argument reader.
        let (argc_ptr, argv_ptr) = unsafe { (libc::_NSGetArgc(), libc::_NSGetArgv()) };
        if argc_ptr.is_null()
            || !argc_ptr.is_aligned()
            || argv_ptr.is_null()
            || !argv_ptr.is_aligned()
        {
            return Err(Error::Invalid("invalid dyld argc/argv globals"));
        }
        // SAFETY: the checked globals have their declared initialized scalar/pointer extent.
        let (argc, argv) = unsafe { (argc_ptr.read(), argv_ptr.read()) };
        let argc = count(argc, maximum)?;
        if argc < 2 {
            return Ok(None);
        }
        if argv.is_null() || !argv.is_aligned() {
            return Err(Error::Invalid("invalid initial argv"));
        }
        // SAFETY: loader argc was checked above; argv has argc initialized pointers plus NUL.
        let first = unsafe { argument(argv.add(1).read(), maximum)? };
        if first != selector {
            return Ok(None);
        }
        let pointers = argc
            .checked_add(1)
            .and_then(|n| n.checked_mul(size_of::<*const c_char>()))
            .ok_or(Error::Invalid("argv pointer width overflow"))?;
        let mut left = maximum
            .checked_sub(pointers)
            .ok_or(Error::Invalid("argv budget"))?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(argc)
            .map_err(|_| Error::Invalid("argv storage unavailable"))?;
        for at in 0..argc {
            // SAFETY: same immutable loader argv and checked count; argument scans stop at NUL.
            let bytes = unsafe { argument(argv.add(at).read(), left)? };
            let consumed = bytes
                .len()
                .checked_add(1)
                .ok_or(Error::Invalid("argument width"))?;
            left = left
                .checked_sub(consumed)
                .ok_or(Error::Invalid("argv byte budget"))?;
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(bytes.len())
                .map_err(|_| Error::Invalid("argument storage unavailable"))?;
            owned.extend_from_slice(bytes);
            result.push(OsString::from_vec(owned));
        }
        // SAFETY: loader argv has one further pointer; no pointed-to bytes are read here.
        if !unsafe { argv.add(argc).read() }.is_null() {
            return Err(Error::Invalid("argv lacks final NULL"));
        }
        Ok(Some(result))
    }
    pub(crate) fn expiry(deadline: Instant) -> Result<Duration, Error> {
        let raw = uptime_raw()?; // MUST precede the Instant remaining sample.
        conservative_expiry(raw, deadline.checked_duration_since(Instant::now()))
    }
    fn conservative_expiry(raw: Duration, left: Option<Duration>) -> Result<Duration, Error> {
        let left = left
            .filter(|left| !left.is_zero() && *left != Duration::MAX)
            .ok_or(Error::Deadline)?;
        raw.checked_add(left)
            .ok_or(Error::Invalid("native expiry overflow"))
    }
    pub(crate) fn remaining(expiry: Duration) -> Result<Duration, Error> {
        expiry
            .checked_sub(uptime_raw()?)
            .filter(|left| !left.is_zero())
            .ok_or(Error::Deadline)
    }
    /// Duplicate only existing stdio, with separate descriptor flags. No parent/global
    /// inheritability is changed; partial array construction closes earlier owned copies.
    pub(crate) fn stdio() -> Result<[OwnedFd; 3], Error> {
        fn copy(raw: c_int) -> Result<OwnedFd, Error> {
            // SAFETY: these are the three initial stdio descriptors supplied by the parent.
            // F_DUPFD_CLOEXEC returns a fresh descriptor without mutating the source flags.
            let fd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                return Err(io("duplicating probe stdio", io::Error::last_os_error()));
            }
            // SAFETY: this successful fcntl transferred exactly one new, exclusively owned fd.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
        Ok([copy(0)?, copy(1)?, copy(2)?])
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct FdObservation {
        pub fd: c_int,
        pub before: c_int,
        pub after: c_int,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Limits {
        pub soft: u64,
        pub hard: u64,
        pub fds: [FdObservation; 3],
    }
    /// Prepared solely inside an ordinarily execed trusted entry. Raw pointers borrow
    /// this entry's Spawn; neither pointers nor opaque spawn objects cross process exec.
    pub(crate) struct Setexec<'a> {
        spawn: &'a Spawn,
        prepared: PreparedSpawn,
    }
    impl<'a> Setexec<'a> {
        pub(crate) fn new(spawn: &'a Spawn, group: i32, clamp: Clamp) -> Result<Self, Error> {
            if spawn.paths.len() != 1
                || spawn
                    .paths
                    .first()
                    .is_none_or(|p| !p.as_bytes().starts_with(b"/"))
            {
                return Err(Error::Invalid(
                    "SETEXEC requires one explicit absolute image",
                ));
            }
            let mut prepared = PreparedSpawn::new(spawn, group, clamp, Start::Suspended)
                .map_err(|e| io("preparing native held SETEXEC", e))?;
            let mut flags: i16 = 0;
            // SAFETY: both native objects were initialized by the shared checked preparation;
            // the getter writes one short, and the setter copies that short into this owned attr.
            super::errno(unsafe {
                libc::posix_spawnattr_getflags(&raw const prepared.attr.0, &raw mut flags)
            })
            .map_err(|e| io("reading native SETEXEC flags", e))?;
            flags |= i16::try_from(libc::POSIX_SPAWN_SETEXEC)
                .map_err(|_| Error::Invalid("SETEXEC flag width"))?;
            // SAFETY: same initialized private attribute object; no foreign code has run.
            super::errno(unsafe {
                libc::posix_spawnattr_setflags(&raw mut prepared.attr.0, flags)
            })
            .map_err(|e| io("setting native SETEXEC flag", e))?;
            Ok(Self { spawn, prepared })
        }
        /// Success normally replaces THIS PID. If a wrapper unexpectedly returns0,
        /// always return its actual PID to the preallocated direct-child owner.
        pub(crate) fn replace(
            self,
            expiry: Duration,
            publish: &mut dyn FnMut(Limits) -> Result<(), Error>,
        ) -> Result<i32, Error> {
            remaining(expiry)?;
            if rustix::process::getuid().as_raw() == 0
                || rustix::process::geteuid() != rustix::process::getuid()
            {
                return Err(Error::Invalid(
                    "the trusted-entry probe must be rootless without credentials change",
                ));
            }
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: this changes ONLY this already-execed process's NPROC soft/hard
            // values. The fully initialized constant has the native rlimit ABI extent.
            if unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &raw const zero) } != 0 {
                return Err(io("sealing native NPROC", io::Error::last_os_error()));
            }
            let mut limits = zero;
            // SAFETY: reads this process's same limit into a fully initialized live object.
            if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &raw mut limits) } != 0 {
                return Err(io(
                    "reading sealed native NPROC",
                    io::Error::last_os_error(),
                ));
            }
            if limits.rlim_cur != 0 || limits.rlim_max != 0 {
                return Err(Error::Invalid("NPROC seal did not hold"));
            }
            let mut observed = [FdObservation {
                fd: -1,
                before: 0,
                after: 0,
            }; 3];
            for (slot, fd) in observed.iter_mut().zip(&self.spawn.stdio) {
                let raw = fd.as_raw_fd();
                // SAFETY: Spawn owns each source through this complete native call. F_GETFD
                // reads its local descriptor flags; only these entry-owned copies may change.
                let before = unsafe { libc::fcntl(raw, libc::F_GETFD) };
                if before < 0 {
                    return Err(io(
                        "reading SETEXEC source flags",
                        io::Error::last_os_error(),
                    ));
                }
                if before & libc::FD_CLOEXEC != 0 {
                    // SAFETY: modifies only this entry's owned source, not the original parent.
                    // Pinned exec-copy prunes CLOEXEC sources BEFORE applying file actions.
                    if unsafe { libc::fcntl(raw, libc::F_SETFD, before & !libc::FD_CLOEXEC) } != 0 {
                        return Err(io(
                            "preparing SETEXEC source flags",
                            io::Error::last_os_error(),
                        ));
                    }
                }
                // SAFETY: same still-owned descriptor, read-only observation after the change.
                let after = unsafe { libc::fcntl(raw, libc::F_GETFD) };
                if after < 0 {
                    return Err(io(
                        "checking SETEXEC source flags",
                        io::Error::last_os_error(),
                    ));
                }
                *slot = FdObservation {
                    fd: raw,
                    before,
                    after,
                };
            }
            publish(Limits {
                soft: limits.rlim_cur,
                hard: limits.rlim_max,
                fds: observed,
            })?;
            remaining(expiry)?;
            let image = self
                .spawn
                .paths
                .first()
                .ok_or(Error::Invalid("missing SETEXEC image"))?;
            let mut returned = 0;
            // SAFETY: all strings and pointer arrays are NUL-terminated, attributes/actions
            // prepared in THIS ordinarily execed process and live through the call. PID is
            // always captured, even for SETEXEC, so an unexpected child cannot be discarded.
            let result = unsafe {
                libc::posix_spawn(
                    &raw mut returned,
                    image.as_ptr(),
                    &raw const self.prepared.actions.0,
                    &raw const self.prepared.attr.0,
                    self.prepared.argv.as_ptr(),
                    self.prepared.envp.as_ptr(),
                )
            };
            super::errno(result).map_err(|e| io("native held SETEXEC", e))?;
            Ok(returned)
        }
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct ImageEvent {
        pub exec: bool,
        pub exit: bool,
        pub readable: bool,
        pub waitable: bool,
        pub raw_flags: u16,
        pub raw_fflags: u32,
        pub raw_data: isize,
    }
    /// Notifications only. The SAME BoxProc remains the sole native wait/reap owner.
    pub(crate) struct ImageEvents {
        queue: Kqueue,
        pid: Option<i32>,
        report: Option<c_int>,
    }
    impl ImageEvents {
        pub(crate) fn new() -> Result<Self, Error> {
            let queue = Kqueue::new().map_err(|e| io("preparing native image events", e))?;
            queue
                .child_completions()
                .map_err(|e| io("preparing waitable child notification", e))?;
            Ok(Self {
                queue,
                pid: None,
                report: None,
            })
        }
        pub(crate) fn bind(&mut self, pid: i32, report: Option<c_int>) -> Result<(), Error> {
            let ident = usize::try_from(pid)
                .ok()
                .filter(|p| *p != 0)
                .ok_or(Error::Invalid("nonpositive image PID"))?;
            if self.pid.is_some() {
                return Err(Error::Invalid("image observer already bound"));
            }
            self.pid = Some(pid);
            self.report = report;
            let changes = [kevent(
                ident,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_CLEAR | libc::EV_RECEIPT,
                libc::NOTE_EXEC | libc::NOTE_EXIT | super::NOTE_EXITSTATUS,
                0,
                0,
            )];
            let mut reply = [kevent(0, 0, 0, 0, 0, 0)];
            let n = self
                .queue
                .kevent(&changes, &mut reply, Some(Duration::ZERO))
                .map_err(|e| io("registering held image", e))?;
            let reply = reply
                .first()
                .ok_or(Error::Invalid("missing image registration receipt"))?;
            if n != 1
                || reply.ident != ident
                || reply.filter != libc::EVFILT_PROC
                || reply.flags & libc::EV_ERROR == 0
            {
                return Err(Error::Invalid("invalid image registration receipt"));
            }
            if reply.data != 0 {
                return Err(io(
                    "registering held image",
                    io::Error::from_raw_os_error(
                        c_int::try_from(reply.data)
                            .map_err(|_| Error::Invalid("image errno width"))?,
                    ),
                ));
            }
            if let Some(fd) = report {
                if fd < 0 {
                    return Err(Error::Invalid("invalid report fd"));
                }
                self.queue
                    .change(&[kevent(
                        usize::try_from(fd).map_err(|_| Error::Invalid("report fd width"))?,
                        libc::EVFILT_READ,
                        libc::EV_ADD | libc::EV_CLEAR,
                        0,
                        0,
                        0,
                    )])
                    .map_err(|e| io("watching probe report", e))?;
            }
            Ok(())
        }
        pub(crate) fn next(&self, expiry: Duration) -> Result<ImageEvent, Error> {
            let pid = self.pid.ok_or(Error::Invalid("unbound image observer"))?;
            loop {
                let left = remaining(expiry)?;
                let timeout = libc::timespec {
                    tv_sec: libc::time_t::try_from(left.as_secs())
                        .map_err(|_| Error::Invalid("image wait seconds"))?,
                    tv_nsec: libc::c_long::from(left.subsec_nanos()),
                };
                let mut events = [kevent(0, 0, 0, 0, 0, 0); 3]; // One PID, its report, and SIGCHLD.
                // SAFETY: exact fixed output array and initialized finite timeout; this does
                // not rebase the raw expiry or use the legacy overflow-to-infinite fallback.
                let got = unsafe {
                    libc::kevent(
                        self.queue.0.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        c_int::try_from(events.len()).map_err(|_| Error::Invalid("event width"))?,
                        &raw const timeout,
                    )
                };
                if got < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(io("waiting for held image", error));
                }
                remaining(expiry)?;
                let got = usize::try_from(got)
                    .map_err(|_| Error::Invalid("negative native event count"))?;
                if got == 0 {
                    return Err(Error::Deadline);
                }
                let mut result = ImageEvent {
                    exec: false,
                    exit: false,
                    readable: false,
                    waitable: false,
                    raw_flags: 0,
                    raw_fflags: 0,
                    raw_data: 0,
                };
                for event in events
                    .get(..got)
                    .ok_or(Error::Invalid("oversized native event count"))?
                {
                    if event.flags & libc::EV_ERROR != 0 {
                        return Err(io(
                            "native image event",
                            io::Error::from_raw_os_error(
                                c_int::try_from(event.data)
                                    .map_err(|_| Error::Invalid("event errno width"))?,
                            ),
                        ));
                    }
                    match event.filter {
                        libc::EVFILT_PROC
                            if event.ident
                                == usize::try_from(pid)
                                    .map_err(|_| Error::Invalid("image PID width"))? =>
                        {
                            result.exec |= event.fflags & libc::NOTE_EXEC != 0;
                            result.exit |= event.fflags & libc::NOTE_EXIT != 0;
                            result.raw_flags = event.flags;
                            result.raw_fflags = event.fflags;
                            result.raw_data = event.data;
                        }
                        libc::EVFILT_READ
                            if self.report.and_then(|fd| usize::try_from(fd).ok())
                                == Some(event.ident) =>
                        {
                            result.readable = true
                        }
                        libc::EVFILT_SIGNAL
                            if event.ident
                                == usize::try_from(libc::SIGCHLD)
                                    .map_err(|_| Error::Invalid("signal width"))? =>
                        {
                            result.waitable = true
                        }
                        _ => return Err(Error::Invalid("foreign native image event")),
                    }
                }
                return Ok(result);
            }
        }
    }

    // XNU f6217f891ac0 mach/message.h and mach/task_info.h: audit_token_t is eight
    // unsigned words; TASK_AUDIT_TOKEN is flavor 15 and returns that many natural_t words.
    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct AuditToken {
        words: [u32; 8],
    }
    const TASK_AUDIT_TOKEN: libc::task_flavor_t = 15;

    // SAFETY: declarations match pinned mach/task.h, mach/mach_port.h and libproc.h.
    // All pointers name live caller-owned scalars; proc_signal_with_audittoken returns errno directly.
    unsafe extern "C" {
        fn mach_task_self() -> libc::mach_port_t;
        fn task_name_for_pid(
            task: libc::mach_port_t,
            pid: libc::pid_t,
            name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    // SAFETY: pinned libproc.h declares one audit_token_t input and signal; the direct return is errno.
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_signal_with_audittoken(token: *mut AuditToken, signal: c_int) -> c_int;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    pub(crate) enum TokenFailure {
        #[error("native member identity is not positive")]
        InvalidIdentity,
        #[error("{operation} failed with errno {code:?}, kind {kind:?}")]
        Io {
            operation: &'static str,
            code: Option<i32>,
            kind: io::ErrorKind,
        },
        #[error("native member birth changed or is unavailable (observed {observed:?})")]
        Changed { observed: Option<u64> },
        #[error("{operation} failed with Mach status {code}")]
        Mach {
            operation: &'static str,
            code: libc::kern_return_t,
        },
        #[error("task audit query failed {query}, and owned right release failed {release}")]
        QueryAndRelease {
            query: libc::kern_return_t,
            release: libc::kern_return_t,
        },
        #[error("native task audit token returned {got} words, expected {expected}")]
        Length {
            got: libc::mach_msg_type_number_t,
            expected: libc::mach_msg_type_number_t,
        },
        #[error("task-name query returned no owned right")]
        NoRight,
    }

    fn token_io(operation: &'static str, error: io::Error) -> TokenFailure {
        TokenFailure::Io {
            operation,
            code: error.raw_os_error(),
            kind: error.kind(),
        }
    }

    /// A genuine native incarnation retained in the SAME Hold entry; no public token constructor.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) struct MemberToken {
        pid: i32,
        birth: u64,
        token: AuditToken,
    }
    impl std::fmt::Debug for MemberToken {
        fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            output
                .debug_struct("MemberToken")
                .field("pid", &self.pid)
                .field("birth", &self.birth)
                .finish_non_exhaustive()
        }
    }
    impl MemberToken {
        pub(crate) fn pid(&self) -> i32 {
            self.pid
        }
        pub(crate) fn birth(&self) -> u64 {
            self.birth
        }
    }

    fn member_birth(pid: i32, expected: u64) -> Result<(), TokenFailure> {
        let actual = rusage(pid).map_err(|error| token_io("reading native member birth", error))?;
        if actual.is_none_or(|usage| usage.started != expected || usage.ended) {
            return Err(TokenFailure::Changed {
                observed: actual.map(|usage| usage.started),
            });
        }
        Ok(())
    }

    /// Capture only a genuine task-name-derived token, joining the expected PID/birth on both sides.
    /// A task-name port has no task-control rights; the acquired send right is released on every path.
    pub(crate) fn capture_member(pid: i32, birth: u64) -> Result<MemberToken, TokenFailure> {
        join_member_token(pid, birth, || member_birth(pid, birth), || task_audit(pid))
    }

    fn join_member_token(
        pid: i32,
        birth: u64,
        mut observe: impl FnMut() -> Result<(), TokenFailure>,
        capture: impl FnOnce() -> Result<AuditToken, TokenFailure>,
    ) -> Result<MemberToken, TokenFailure> {
        if pid <= 0 || birth == 0 {
            return Err(TokenFailure::InvalidIdentity);
        }
        observe()?;
        let token = capture()?;
        observe()?;
        Ok(MemberToken { pid, birth, token })
    }

    fn task_audit(pid: i32) -> Result<AuditToken, TokenFailure> {
        let mut port: libc::mach_port_t = 0;
        // SAFETY: mach_task_self names this process's task; task_name_for_pid writes only its live
        // out-parameter. Success transfers one task-name send right, owned until deallocated below.
        let status = unsafe { task_name_for_pid(mach_task_self(), pid, &raw mut port) };
        if status != 0 {
            return Err(TokenFailure::Mach {
                operation: "task_name_for_pid",
                code: status,
            });
        }
        if port == 0 {
            return Err(TokenFailure::NoRight);
        }
        let mut token = AuditToken { words: [0; 8] };
        let expected: libc::mach_msg_type_number_t = 8;
        let mut count = expected;
        // SAFETY: the owned name port authorizes TASK_AUDIT_TOKEN. The initialized output is exactly
        // eight natural_t words, and count describes its whole writable extent for the MIG call.
        let query = unsafe {
            libc::task_info(
                port,
                TASK_AUDIT_TOKEN,
                (&raw mut token).cast::<i32>(),
                &raw mut count,
            )
        };
        // SAFETY: this is the one task-name send right acquired above; no early return or other
        // operation transfers/releases it. Its name is not reused by this owner after this call.
        let release = unsafe { mach_port_deallocate(mach_task_self(), port) };
        if release != 0 {
            return Err(TokenFailure::QueryAndRelease { query, release });
        }
        if query != 0 {
            return Err(TokenFailure::Mach {
                operation: "TASK_AUDIT_TOKEN",
                code: query,
            });
        }
        if count != expected {
            return Err(TokenFailure::Length {
                got: count,
                expected,
            });
        }
        Ok(token)
    }

    #[derive(Debug, Clone, Copy)]
    pub(crate) enum MemberSignal {
        Continue,
        Kill,
    }

    /// The kernel resolves the token's incarnation and checks signal authority while retaining the
    /// exact proc reference. ESRCH/permission/changed identities remain typed failures, never PID fallback.
    pub(crate) fn signal_member(
        member: &MemberToken,
        signal: MemberSignal,
    ) -> Result<(), TokenFailure> {
        let mut token = member.token;
        let signal = match signal {
            MemberSignal::Continue => libc::SIGCONT,
            MemberSignal::Kill => libc::SIGKILL,
        };
        // SAFETY: token is a copy of the genuine kernel-produced token in the immutable native Hold
        // receipt. libproc reads its full fixed ABI extent and forwards only the allowed signal above.
        let error = unsafe { proc_signal_with_audittoken(&raw mut token, signal) };
        if error != 0 {
            return Err(token_io(
                "proc_signal_with_audittoken",
                io::Error::from_raw_os_error(error),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod controls {
        use super::*;
        use std::cell::Cell;

        /// Hostile argc cannot make the earliest selector walk an unbounded pointer table.
        #[test]
        fn initial_argument_pointer_budget_is_checked_before_access() {
            let two = size_of::<*const c_char>().checked_mul(3).unwrap();
            assert_eq!(count(2, two).unwrap(), 2);
            assert!(count(-1, two).is_err());
            assert!(count(2, two - 1).is_err());
            assert!(count(c_int::MAX, two).is_err());
        }

        /// A delayed receiver cannot obtain a fresh interval or an infinite expiry.
        #[test]
        fn transported_expiry_retains_original_authority_without_rebase() {
            let expiry =
                conservative_expiry(Duration::from_secs(10), Some(Duration::from_secs(3))).unwrap();
            assert_eq!(expiry, Duration::from_secs(13));
            assert_eq!(expiry.checked_sub(Duration::from_secs(14)), None);
            assert!(conservative_expiry(Duration::MAX, Some(Duration::from_nanos(1))).is_err());
            for left in [None, Some(Duration::ZERO), Some(Duration::MAX)] {
                assert!(matches!(
                    conservative_expiry(Duration::from_secs(10), left),
                    Err(Error::Deadline)
                ));
            }
        }

        /// Capture does not grant a signal identity after a birth change between observations.
        #[test]
        fn changed_member_during_token_capture_never_yields_a_receipt() {
            let reads = Cell::new(0);
            let result = join_member_token(
                7,
                9,
                || {
                    reads.set(reads.get() + 1);
                    if reads.get() == 1 {
                        Ok(())
                    } else {
                        Err(TokenFailure::Changed { observed: Some(10) })
                    }
                },
                || Ok(AuditToken { words: [0; 8] }),
            );
            assert!(matches!(
                result,
                Err(TokenFailure::Changed { observed: Some(10) })
            ));
            assert_eq!(reads.get(), 2);
            let touched = Cell::new(false);
            assert!(
                join_member_token(
                    -1,
                    9,
                    || {
                        touched.set(true);
                        Ok(())
                    },
                    || Ok(AuditToken { words: [0; 8] })
                )
                .is_err()
            );
            assert!(!touched.get());
        }
    }
}

#[cfg(test)]
mod map_controls {
    use super::*;

    /// The most threads a task may have, the room a whole thread list needs.
    fn room() -> Vec<u64> {
        let most = sysctl_number(c"kern.num_taskthreads").unwrap();
        vec![0; usize::try_from(most).unwrap()]
    }

    fn me() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    /// AC-T7.4.1 (guest_cost's macOS CPU split): a thread named as a VMM names its vCPU threads spins; read
    /// this process's threads: expect it listed by its name with CPU it used, and a room too small for the
    /// list refused rather than read short.
    #[test]
    fn a_process_threads_are_read_by_name_and_cpu() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spinning = std::sync::Arc::clone(&stop);
        let spinner = std::thread::Builder::new()
            .name("vcpu7".to_owned())
            .spawn(move || {
                while !spinning.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
            .unwrap();
        let mut handles = room();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let named = loop {
            let listed = threads(me(), &mut handles).unwrap().unwrap();
            if let Some(thread) = listed.iter().find(|t| t.name == b"vcpu7")
                && !thread.cpu.is_zero()
            {
                break thread.clone();
            }
            assert!(std::time::Instant::now() < deadline, "{listed:?}");
            std::thread::yield_now();
        };
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spinner.join().unwrap();
        assert!(named.cpu > Duration::ZERO);
        assert!(threads(me(), &mut [0u64; 1]).is_err());
    }

    /// AC-T7.4.1 (guest_cost's macOS guest-RAM accounting): touch some pages of a fresh anonymous region and
    /// read this process's map: expect the entry holding it found with at least those pages resident, no
    /// file, and its process free to read and write it; the entry of this program's own code a file's; and a
    /// walk of the map from its start to its end.
    #[test]
    fn a_process_map_is_read_entry_by_entry() {
        let page = rustix::param::page_size();
        let mut memory = vec![0u8; 64 * page];
        for at in (0..16 * page).step_by(page) {
            memory[at] = 1;
        }
        let address = u64::try_from(memory.as_ptr() as usize).unwrap();
        let entry = region_with_path(me(), address).unwrap().unwrap();
        assert!(
            entry.address <= address && address < entry.address + entry.size,
            "{entry:?}"
        );
        assert!(entry.resident >= 16, "{entry:?}");
        assert_eq!(entry.file, Some(false));
        assert!(entry.max_protection & 3 == 3 && !entry.submap, "{entry:?}");
        // The same entry read without its path (its pages may change between the reads: tests share it).
        let plain = region(me(), address).unwrap().unwrap();
        assert_eq!(
            (plain.address, plain.size, plain.file),
            (entry.address, entry.size, None)
        );
        assert!(plain.resident >= 16, "{plain:?}");
        let code =
            u64::try_from(region as fn(i32, u64) -> io::Result<Option<Region>> as usize).unwrap();
        assert_eq!(
            region_with_path(me(), code).unwrap().unwrap().file,
            Some(true)
        );
        let (mut at, mut entries) = (0u64, 0usize);
        while let Some(entry) = region(me(), at).unwrap() {
            assert!(entry.address + entry.size > at);
            at = entry.address + entry.size;
            entries += 1;
        }
        assert!(entries > 2, "{entries}");
        std::hint::black_box(&memory);
    }
}
