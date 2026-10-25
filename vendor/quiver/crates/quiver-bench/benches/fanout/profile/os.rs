//! What the kernel counts for this process and its threads, unprivileged: getrusage(2)'s CPU, page
//! faults and context switches, for the process and for the profiler's own thread (whose share
//! comes off); each named thread's from `/proc/self/task/<tid>/` (proc(5): `schedstat`'s time on a
//! CPU and time waiting for one, `stat`'s page faults, `status`'s context switches); resident
//! memory now and at most (`/proc/self/status`'s `VmRSS` and `VmHWM`, the high-water mark reset at
//! a window's start by writing 5 to `/proc/self/clear_refs`); and the process's storage I/O
//! (`/proc/self/io`'s bytes fetched from and sent to storage, the profiler's own from
//! `/proc/thread-self/io`; its call counts leave out a socket's `recv` and `send`, so they're not
//! read). Linux; elsewhere getrusage's process figures where Unix keeps them, none on Windows.
//!
//! getrusage needs `unsafe` (CLAUDE.md §12).
#![allow(unsafe_code)]

/// getrusage's figures.
#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    pub cpu_ns: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub voluntary: u64,
    pub involuntary: u64,
}

impl Usage {
    /// What was charged between `earlier` and `self`.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_sub(earlier.cpu_ns),
            minor_faults: self.minor_faults.saturating_sub(earlier.minor_faults),
            major_faults: self.major_faults.saturating_sub(earlier.major_faults),
            voluntary: self.voluntary.saturating_sub(earlier.voluntary),
            involuntary: self.involuntary.saturating_sub(earlier.involuntary),
        }
    }
}

#[cfg(unix)]
fn rusage(who: libc::c_int) -> Option<Usage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a writable `rusage` that outlives the call.
    if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: getrusage filled it, and a zeroed `rusage` was already a valid one.
    let u = unsafe { usage.assume_init() };
    let ns = |t: libc::timeval| {
        u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000
            + u64::try_from(t.tv_usec).unwrap_or(0) * 1000
    };
    let count = |n: libc::c_long| u64::try_from(n).unwrap_or(0);
    Some(Usage {
        cpu_ns: ns(u.ru_utime) + ns(u.ru_stime),
        minor_faults: count(u.ru_minflt),
        major_faults: count(u.ru_majflt),
        voluntary: count(u.ru_nvcsw),
        involuntary: count(u.ru_nivcsw),
    })
}

/// The process's figures so far, every thread's (live and gone) together.
#[cfg(unix)]
pub fn process() -> Option<Usage> {
    rusage(libc::RUSAGE_SELF)
}

#[cfg(not(unix))]
pub fn process() -> Option<Usage> {
    None
}

/// The calling thread's figures so far (Linux's `RUSAGE_THREAD`).
#[cfg(target_os = "linux")]
pub fn this_thread() -> Option<Usage> {
    rusage(libc::RUSAGE_THREAD)
}

#[cfg(not(target_os = "linux"))]
pub fn this_thread() -> Option<Usage> {
    None
}

/// The calling thread's id, as `/proc/self/task/` names it.
#[cfg(target_os = "linux")]
pub fn tid() -> i32 {
    // SAFETY: gettid(2) takes nothing and always succeeds.
    unsafe { libc::gettid() }
}

#[cfg(not(target_os = "linux"))]
pub fn tid() -> i32 {
    0
}

/// One thread's figures so far, from `/proc/self/task/<tid>/`.
#[derive(Clone, Copy, Default, Debug)]
pub struct Thread {
    pub cpu_ns: u64,
    /// Time runnable but waiting for a CPU.
    pub wait_ns: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub voluntary: u64,
    pub involuntary: u64,
}

impl Thread {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_sub(earlier.cpu_ns),
            wait_ns: self.wait_ns.saturating_sub(earlier.wait_ns),
            minor_faults: self.minor_faults.saturating_sub(earlier.minor_faults),
            major_faults: self.major_faults.saturating_sub(earlier.major_faults),
            voluntary: self.voluntary.saturating_sub(earlier.voluntary),
            involuntary: self.involuntary.saturating_sub(earlier.involuntary),
        }
    }

    pub fn plus(&self, other: &Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns + other.cpu_ns,
            wait_ns: self.wait_ns + other.wait_ns,
            minor_faults: self.minor_faults + other.minor_faults,
            major_faults: self.major_faults + other.major_faults,
            voluntary: self.voluntary + other.voluntary,
            involuntary: self.involuntary + other.involuntary,
        }
    }
}

/// Thread `tid`'s figures, or `None` once it's gone (or off Linux).
pub fn thread(tid: i32) -> Option<Thread> {
    let dir = format!("/proc/self/task/{tid}");
    let sched = std::fs::read_to_string(format!("{dir}/schedstat")).ok()?;
    let mut times = sched.split_whitespace().map(str::parse::<u64>);
    let cpu_ns = times.next()?.ok()?;
    let wait_ns = times.next()?.ok()?;
    let stat = std::fs::read_to_string(format!("{dir}/stat")).ok()?;
    // Fields after the name, which is in parentheses and may hold spaces: the third onwards.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
    let status = std::fs::read_to_string(format!("{dir}/status")).ok()?;
    let line = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    Some(Thread {
        cpu_ns,
        wait_ns,
        minor_faults: field(10)?,
        major_faults: field(12)?,
        voluntary: line("voluntary_ctxt_switches:")?,
        involuntary: line("nonvoluntary_ctxt_switches:")?,
    })
}

/// The process's resident memory now and at most since the mark was last reset, bytes.
pub fn resident() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|k| k * 1024)
    };
    Some((kib("VmRSS:")?, kib("VmHWM:")?))
}

/// Resets the resident high-water mark to what is resident now; whether the kernel let it.
pub fn reset_peak() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

/// `/proc/<self or thread-self>/io`'s bytes fetched from and sent to storage.
#[derive(Clone, Copy, Default, Debug)]
pub struct Io {
    pub storage_read: u64,
    pub storage_written: u64,
}

impl Io {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            storage_read: self.storage_read.saturating_sub(earlier.storage_read),
            storage_written: self.storage_written.saturating_sub(earlier.storage_written),
        }
    }
}

/// The I/O account at `path` (`/proc/self/io` or `/proc/thread-self/io`).
pub fn io(path: &str) -> Option<Io> {
    let text = std::fs::read_to_string(path).ok()?;
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    Some(Io {
        storage_read: field("read_bytes:")?,
        storage_written: field("write_bytes:")?,
    })
}
