//! A thread's scheduling priority, lowered and read back: the OS-interface file of the compute pool's
//! threads ([`crate::compute`], kiln's departure, ORIGIN.md). Each `unsafe` block states its invariant.
//!
//! Each OS has two ranks (`crate::compute::Rank`). The one below the shards keeps a share of the CPU against any
//! other thread at the process's priority: requests wait on the pool's jobs, and the idle rank leaves them waiting
//! on whatever else busies the process's cgroup (beside one thread spinning on its core, a 5 ms job took 6.1 s at
//! the median at the idle rank and 48 ms at the rank below, which the weights' 51.5 ms derives;
//! `benches/compute_wait.rs`, 2026-11-04). kiln takes the rank below.
//!
//! - **Linux:** the rank below is the batch policy at a nice value [`INCREMENT`] above the thread's own, both set by
//!   one `sched_setattr` on the calling thread [B: sched(7); sched_setattr(2)]: a weight of 110 against 1024
//!   (sched(7), "The nice value": each step a factor of about 1.25), the thread taken as CPU-bound and disfavoured
//!   at wakeup. Alone that leaves a shard waking on a CPU the pool holds waiting up to a slice: EEVDF preempts a
//!   running task for a waking one by their deadlines, not their weights. A shard that asks for a slice of its
//!   step ([`prefer_current_thread`]) preempts it at once: on one core shared with four such threads, a shard's p99
//!   wake lateness was 1.1 ms without its slice and 0.21 ms with a 250 µs one, its calm rounds 0.20 to 0.28 ms
//!   (`benches/compute_priority.rs`, `SHARD_SLICE_NS`, `POOL_RANK=below`, 2026-11-04). The idle rank is the idle
//!   policy, `SCHED_IDLE`: "Idle tasks are by definition preempted by non-idle tasks" (the kernel's fair class), a
//!   shard's p99 0.20 ms beside it with no slice of its own; but a weight of 3, and a thread can't leave it without
//!   `RLIMIT_NICE`'s room. Where a call is refused (a kernel before 3.14 has no `sched_setattr`; a seccomp policy
//!   may refuse it) the next one down: the batch policy, then the nice value alone by `setpriority` on the
//!   thread's id. A thread may lower its own priority unprivileged, and its policy and nice value are its own on
//!   Linux (sched(7)).
//! - **macOS:** the rank below is the utility QoS class and the idle rank the background class,
//!   `pthread_set_qos_class_self_np` [B: Apple, Energy Efficiency Guide for Mac Apps, "Prioritize Work at the Task
//!   Level"]: the system ranks a thread's CPU, I/O and timers by its class, and runs background work on the
//!   efficiency cores. Measured on a 14-core Mac Studio, a shard at the default class beside 13 spinning threads
//!   woke with a p99 lateness of 9.6 ms beside threads at its own class, 8.9 ms beside utility and 0.54 ms beside
//!   background, its calm rounds 0.39 to 1.1 ms, and no better at the user-interactive class (12.1 ms beside
//!   utility) (`benches/compute_priority.rs`, `SHARD_QOS`, `POOL_RANK=below`, 2026-11-04); but tests that waited
//!   on background threads took 30 to 160 s on that machine where they took 5 s. macOS keeps no slice a shard can
//!   ask for.
//! - **Windows:** `THREAD_PRIORITY_BELOW_NORMAL` by `SetThreadPriority` [B: Microsoft Learn, "Scheduling
//!   Priorities": "Background threads, particularly those that are processor intensive, can be set to
//!   THREAD_PRIORITY_BELOW_NORMAL or THREAD_PRIORITY_LOWEST, to ensure that they can be preempted when
//!   necessary"].
//!
//! Each only ever lowers: a thread already below the target (a process started under `nice`, at background
//! QoS, at a lower level) keeps what it has, as `nice` adds its increment to what a process runs at.
//!
//! One call raises, for a shard alone, where the system grants it ([`realtime_current_thread`]; kiln's goal 8,
//! plan 01a1a59c2b29 step 7, and goal 9, plan 01a1a87b2cec step 4): Linux's round-robin real-time class at
//! the lowest real-time priority, with the reset-on-fork flag, so nothing the shard starts inherits it. A
//! real-time thread waking where a normal one runs preempts it at once under every preemption mode, the lazy
//! one too, where a normal thread waking beside one busy in user space waits for the next tick (research
//! 01a1a87b2cee f17; kiln's docs/priority.md, "The kernel's preemption"). macOS and Windows refuse it: what
//! each has instead is in their `realtime`.
#![allow(unsafe_code)]

use crate::compute::{Priority, Rank, Refusal};

/// How far the pool's threads' nice value goes above the thread's own on Linux (what ranks them where the idle
/// policy is refused): POSIX's default for `nice`,
/// "If -n increment is not specified, an increment of 10 shall be used" [B: IEEE Std 1003.1-2017, the `nice`
/// utility], the adjustment kiln makes for its own heavy work elsewhere (its AGENTS.md: `nice -n 10`).
#[cfg(target_os = "linux")]
pub(crate) const INCREMENT: i32 = 10;

/// Format: the least favourable nice value, "+19 (low priority)" [B: setpriority(2), sched(7)]; Linux's
/// `MAX_NICE`, past which `sched_setattr` refuses a value.
#[cfg(target_os = "linux")]
const MAX_NICE: i32 = 19;

/// Lowers the calling thread's priority to `rank` as its OS allows (the module's notes): what it runs at now,
/// or the call the OS refused.
pub(crate) fn lower_current_thread(rank: Rank) -> Result<Priority, Refusal> {
    platform::lower(rank)
}

/// The calling thread's priority as its OS reports it.
pub(crate) fn of_current_thread() -> Priority {
    platform::of_current()
}

/// Asks for a slice of `slice_ns` for the calling thread (a shard), keeping its policy and nice value: Linux only.
pub(crate) fn prefer_current_thread(slice_ns: u64) -> Result<(), Refusal> {
    platform::prefer(slice_ns)
}

/// Puts the calling thread (a shard) in Linux's round-robin real-time class at `priority`, what it starts
/// not (reset on fork): Linux only ([`crate::compute::realtime_current_thread`]).
pub(crate) fn realtime_current_thread(priority: i32) -> Result<(), Refusal> {
    platform::realtime(priority)
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{INCREMENT, MAX_NICE};
    use crate::compute::{Priority, Rank, Refusal};

    /// [`Rank::Idle`]: the idle policy by `sched_setattr`. [`Rank::Below`], or where the idle policy is
    /// refused: the batch policy at a nice value [`INCREMENT`] above the thread's own (at most [`MAX_NICE`]),
    /// and that nice value alone where the batch policy is refused too. What the thread runs at then, as the
    /// OS reports it (the kernel keeps no nice value an idle-policy thread is given).
    pub(super) fn lower(rank: Rank) -> Result<Priority, Refusal> {
        let thread = rustix::thread::gettid();
        let now = rustix::process::getpriority_process(Some(thread)).unwrap_or(0);
        let nice = now.saturating_add(INCREMENT).min(MAX_NICE);
        if (rank == Rank::Idle && set(libc::SCHED_IDLE, nice)) || set(libc::SCHED_BATCH, nice) {
            return Ok(of_current());
        }
        nice_alone(nice)
    }

    /// The calling thread's policy and nice value, set together by `sched_setattr`, its slice the kernel's
    /// default: whether the kernel took them.
    fn set(policy: libc::c_int, nice: i32) -> bool {
        set_with(policy, nice, 0)
    }

    /// The calling thread's policy, nice value and EEVDF slice (`sched_runtime`; 0 is the kernel's default),
    /// set together by `sched_setattr`: whether the kernel took them.
    fn set_with(policy: libc::c_int, nice: i32, slice_ns: u64) -> bool {
        let (Ok(size), Ok(policy)) = (
            u32::try_from(std::mem::size_of::<libc::sched_attr>()),
            u32::try_from(policy),
        ) else {
            return false;
        };
        let attr = libc::sched_attr {
            size,
            sched_policy: policy,
            sched_flags: 0,
            sched_nice: nice,
            sched_priority: 0,
            sched_runtime: slice_ns,
            sched_deadline: 0,
            sched_period: 0,
        };
        let pid: libc::pid_t = 0;
        let flags: libc::c_uint = 0;
        // SAFETY: `attr` is a live, initialised `sched_attr` whose `size` states its own size, which the kernel
        // reads for the call's duration and does not keep; pid 0 names the calling thread and flags 0 asks
        // nothing more (sched_setattr(2)). No memory of this process is written.
        let done = unsafe { libc::syscall(libc::SYS_sched_setattr, pid, &raw const attr, flags) };
        done == 0
    }

    /// The round-robin real-time policy at `priority`, with the reset-on-fork flag, so a thread or process the
    /// calling thread starts runs at the normal policy (sched(7), "The reset-on-fork flag"): one `sched_setattr`.
    /// The kernel refuses it (EPERM) to a thread with neither `CAP_SYS_NICE` nor `RLIMIT_RTPRIO` room for
    /// `priority`, and to one in a cgroup given no real-time runtime where the kernel keeps real-time group
    /// scheduling (sched(7); `CONFIG_RT_GROUP_SCHED`).
    pub(super) fn realtime(priority: i32) -> Result<(), Refusal> {
        let refused = || Refusal {
            call: "sched_setattr",
            code: std::io::Error::last_os_error().raw_os_error(),
        };
        let (Ok(size), Ok(policy), Ok(priority), Ok(flags)) = (
            u32::try_from(std::mem::size_of::<libc::sched_attr>()),
            u32::try_from(libc::SCHED_RR),
            u32::try_from(priority),
            u64::try_from(libc::SCHED_FLAG_RESET_ON_FORK),
        ) else {
            return Err(Refusal {
                call: "sched_setattr",
                code: Some(libc::EINVAL),
            });
        };
        let attr = libc::sched_attr {
            size,
            sched_policy: policy,
            sched_flags: flags,
            sched_nice: 0,
            sched_priority: priority,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
        };
        let pid: libc::pid_t = 0;
        let call_flags: libc::c_uint = 0;
        // SAFETY: `attr` is a live, initialised `sched_attr` whose `size` states its own size, which the kernel
        // reads for the call's duration and does not keep; pid 0 names the calling thread and flags 0 asks
        // nothing more (sched_setattr(2)). No memory of this process is written.
        let done = unsafe { libc::syscall(libc::SYS_sched_setattr, pid, &raw const attr, call_flags) };
        if done == 0 { Ok(()) } else { Err(refused()) }
    }

    /// The calling thread's real-time priority, by `sched_getattr`: `None` where the kernel doesn't say.
    fn realtime_priority() -> Option<i32> {
        let size = u32::try_from(std::mem::size_of::<libc::sched_attr>()).ok()?;
        let mut attr = libc::sched_attr {
            size,
            sched_policy: 0,
            sched_flags: 0,
            sched_nice: 0,
            sched_priority: 0,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
        };
        let pid: libc::pid_t = 0;
        let flags: libc::c_uint = 0;
        // SAFETY: `attr` is live, writable and `size` bytes long, the most the kernel writes, for the call's
        // duration only; pid 0 names the calling thread and flags 0 asks nothing more (sched_getattr(2)).
        let done = unsafe { libc::syscall(libc::SYS_sched_getattr, pid, &raw mut attr, size, flags) };
        if done == 0 {
            i32::try_from(attr.sched_priority).ok()
        } else {
            None
        }
    }

    /// The normal policy at the thread's own nice value with an EEVDF slice of `slice_ns` (`sched_runtime`).
    pub(super) fn prefer(slice_ns: u64) -> Result<(), Refusal> {
        let thread = rustix::thread::gettid();
        let nice = rustix::process::getpriority_process(Some(thread)).unwrap_or(0);
        if set_with(libc::SCHED_OTHER, nice, slice_ns) {
            Ok(())
        } else {
            Err(Refusal {
                call: "sched_setattr",
                code: std::io::Error::last_os_error().raw_os_error(),
            })
        }
    }

    /// The nice value alone, by `setpriority` on the calling thread's id (a thread's own on Linux,
    /// sched(7)).
    fn nice_alone(nice: i32) -> Result<Priority, Refusal> {
        let thread = rustix::thread::gettid();
        match rustix::process::setpriority_process(Some(thread), nice) {
            Ok(()) => Ok(of_current()),
            Err(error) => Err(Refusal {
                call: "setpriority",
                code: Some(error.raw_os_error()),
            }),
        }
    }

    /// The calling thread's policy (`sched_getscheduler`, its reset-on-fork flag set aside) and nice value
    /// (`getpriority` on its id), or its real-time priority in a real-time class.
    pub(super) fn of_current() -> Priority {
        let pid: libc::pid_t = 0;
        // SAFETY: no pointer passes; pid 0 names the calling thread, and the call only reads its policy
        // (sched_getscheduler(2)).
        let policy = unsafe { libc::sched_getscheduler(pid) } & !libc::SCHED_RESET_ON_FORK;
        if policy == libc::SCHED_RR || policy == libc::SCHED_FIFO {
            return realtime_priority().map_or(Priority::Unknown, |priority| Priority::Realtime { priority });
        }
        let Ok(nice) = rustix::process::getpriority_process(Some(rustix::thread::gettid())) else {
            return Priority::Unknown;
        };
        match policy {
            libc::SCHED_IDLE => Priority::Idle { nice },
            libc::SCHED_BATCH => Priority::Batch { nice },
            _ => Priority::Other { nice },
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use crate::compute::{Priority, QOS_CLASS_BACKGROUND, QOS_CLASS_UTILITY, Rank, Refusal};

    /// [`Rank::Idle`]: the background QoS class; [`Rank::Below`]: the utility class, or background for a thread
    /// at utility already, so the pool's threads are below the thread that starts them. A thread at background
    /// or below keeps its own.
    pub(super) fn lower(rank: Rank) -> Result<Priority, Refusal> {
        let now = match of_current() {
            Priority::Qos { class } => class,
            _ => 0,
        };
        if now != 0 && now <= QOS_CLASS_BACKGROUND {
            return Ok(Priority::Qos { class: now });
        }
        let (target, class) = match rank {
            Rank::Below if now == 0 || now > QOS_CLASS_UTILITY => {
                (libc::qos_class_t::QOS_CLASS_UTILITY, QOS_CLASS_UTILITY)
            }
            Rank::Below | Rank::Idle => {
                (libc::qos_class_t::QOS_CLASS_BACKGROUND, QOS_CLASS_BACKGROUND)
            }
        };
        // SAFETY: no pointer passes; the call changes only the calling thread's QoS class, to a class
        // `sys/qos.h` defines, at no relative priority (pthread/qos.h).
        let code = unsafe { libc::pthread_set_qos_class_self_np(target, 0) };
        if code == 0 {
            Ok(Priority::Qos { class })
        } else {
            Err(Refusal {
                call: "pthread_set_qos_class_self_np",
                code: Some(code),
            })
        }
    }

    /// macOS keeps no slice a thread can ask for.
    pub(super) fn prefer(_slice_ns: u64) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no slice to ask for on macOS",
            code: None,
        })
    }

    /// No real-time class is asked for on macOS: its one for user threads, the time-constraint policy
    /// (`THREAD_TIME_CONSTRAINT_POLICY`), is for periodic work of a known computation each period, as audio's
    /// is, and demotes a thread that overruns what it declared; a shard's work has no such period. The server's
    /// launchd job runs `Interactive` and legs and agents under background policy, which the scheduler keeps off
    /// the performance cores (docs/priority.md).
    pub(super) fn realtime(_priority: i32) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no real-time class to ask for on macOS",
            code: None,
        })
    }

    /// The calling thread's QoS class, as `pthread_get_qos_class_np` reports it.
    pub(super) fn of_current() -> Priority {
        let mut class: u32 = 0;
        let mut relative: libc::c_int = 0;
        // SAFETY: the calling thread's own handle and two live, writable outputs of the sizes the call writes:
        // a `qos_class_t` is an unsigned 32-bit value (`sys/qos.h`), read here as that integer so a class
        // the binding's enum doesn't name is a value, never an invalid enum.
        let code = unsafe {
            libc::pthread_get_qos_class_np(
                libc::pthread_self(),
                (&raw mut class).cast::<libc::qos_class_t>(),
                &raw mut relative,
            )
        };
        if code == 0 {
            Priority::Qos { class }
        } else {
            Priority::Unknown
        }
    }
}

#[cfg(windows)]
mod platform {
    use crate::compute::{Priority, Rank, Refusal};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };

    /// Format: what `GetThreadPriority` returns when it fails, `THREAD_PRIORITY_ERROR_RETURN` (MAXLONG,
    /// winbase.h).
    const ERROR_RETURN: i32 = i32::MAX;

    /// `THREAD_PRIORITY_BELOW_NORMAL` for the calling thread at either rank, unless it runs at that level or
    /// below already.
    pub(super) fn lower(_rank: Rank) -> Result<Priority, Refusal> {
        if let Priority::Level { level } = of_current()
            && level <= THREAD_PRIORITY_BELOW_NORMAL
        {
            return Ok(Priority::Level { level });
        }
        // SAFETY: the calling thread's pseudo-handle, valid for the call; no pointer passes.
        let done = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
        if done != 0 {
            Ok(Priority::Level {
                level: THREAD_PRIORITY_BELOW_NORMAL,
            })
        } else {
            Err(Refusal {
                call: "SetThreadPriority",
                code: std::io::Error::last_os_error().raw_os_error(),
            })
        }
    }

    /// Windows' scheduler preempts a lower priority level at once; there is no slice to ask for.
    pub(super) fn prefer(_slice_ns: u64) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no slice to ask for on Windows",
            code: None,
        })
    }

    /// No real-time class is asked for on Windows: its scheduler runs a waking thread of a higher priority
    /// level at once, "without allowing it to finish using its time slice" [B: Microsoft Learn, "Scheduling
    /// Priorities"], and the server's process runs above normal while legs and agents run below it
    /// (docs/priority.md), so its shards already preempt them as they wake. `REALTIME_PRIORITY_CLASS` would put
    /// it above the system's own work: "a real-time process that executes for more than a very brief interval
    /// can cause disk caches not to flush or cause the mouse to be unresponsive" [B: the same].
    pub(super) fn realtime(_priority: i32) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no real-time class to ask for on Windows",
            code: None,
        })
    }

    /// The calling thread's priority level, as `GetThreadPriority` reports it.
    pub(super) fn of_current() -> Priority {
        // SAFETY: the calling thread's pseudo-handle, valid for the call; no pointer passes.
        let level = unsafe { GetThreadPriority(GetCurrentThread()) };
        if level == ERROR_RETURN {
            Priority::Unknown
        } else {
            Priority::Level { level }
        }
    }
}

/// An OS this file has no call for: the thread keeps the process's priority.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use crate::compute::{Priority, Rank, Refusal};

    pub(super) fn lower(_rank: Rank) -> Result<Priority, Refusal> {
        Err(Refusal {
            call: "no priority call on this OS",
            code: None,
        })
    }

    pub(super) fn of_current() -> Priority {
        Priority::Unknown
    }

    pub(super) fn prefer(_slice_ns: u64) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no slice to ask for on this OS",
            code: None,
        })
    }

    pub(super) fn realtime(_priority: i32) -> Result<(), Refusal> {
        Err(Refusal {
            call: "no real-time class to ask for on this OS",
            code: None,
        })
    }
}
