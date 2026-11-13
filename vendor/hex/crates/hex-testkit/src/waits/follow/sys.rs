//! A thread as a party, as each OS shows another thread of the process: Linux in procfs, macOS through
//! `proc_pidinfo`, which needs its one FFI call here.

#![cfg_attr(target_os = "macos", allow(unsafe_code))]

use super::Party;

/// A thread of this process, as its OS names it: Linux's tid, macOS's 64-bit thread id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Id(u64);

/// The calling thread's id: `/proc/thread-self` names `<pid>/task/<tid>`.
#[cfg(target_os = "linux")]
pub fn current() -> std::io::Result<Id> {
    let link = std::fs::read_link("/proc/thread-self")?;
    link.file_name()
        .and_then(|tid| tid.to_str())
        .and_then(|tid| tid.parse().ok())
        .map(Id)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "/proc/thread-self names no thread: {}",
                link.display()
            ))
        })
}

/// Thread `id` as a party, from `/proc/self/task/<tid>/stat`: gone or a zombie, ended; otherwise live.
#[cfg(target_os = "linux")]
pub fn party(id: Id) -> Party {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/self/task/{}/stat", id.0)) else {
        return Party::Ended;
    };
    // The command name is in parentheses and may hold anything: the state, after it, is read past its last ')'.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().next());
    match state {
        Some("Z" | "X" | "x") => Party::Ended,
        _ => Party::Live,
    }
}

/// The calling thread's id, as `pthread_threadid_np` gives it.
#[cfg(target_os = "macos")]
pub fn current() -> std::io::Result<Id> {
    let mut id: u64 = 0;
    // SAFETY: with no thread named (0), pthread_threadid_np writes the calling thread's id to `id`, a valid u64.
    let failed = unsafe { libc::pthread_threadid_np(0, &mut id) };
    if failed != 0 {
        return Err(std::io::Error::from_raw_os_error(failed));
    }
    Ok(Id(id))
}

/// XNU's flavor reading a thread by its 64-bit id (`sys/proc_info.h`), which libc doesn't name.
#[cfg(target_os = "macos")]
const PROC_PIDTHREADID64INFO: libc::c_int = 15;

/// Thread `id` as a party, from `proc_pidinfo(PROC_PIDTHREADID64INFO)`: no such thread, ended; otherwise live.
#[cfg(target_os = "macos")]
pub fn party(id: Id) -> Party {
    let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::zeroed();
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<libc::proc_threadinfo>()) else {
        return Party::Live;
    };
    // SAFETY: proc_pidinfo writes at most `size` bytes, one proc_threadinfo, into `info`, which holds that many; the
    // flavor reads this process's thread whose 64-bit id is `id.0`.
    let wrote = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            PROC_PIDTHREADID64INFO,
            id.0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote <= 0 {
        Party::Ended
    } else {
        Party::Live
    }
}
