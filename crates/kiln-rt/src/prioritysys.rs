//! A thread's scheduling priority, lowered and read back: the OS-interface file of the compute pool's
//! threads ([`crate::compute`], kiln's departure, ORIGIN.md). Each `unsafe` block states its invariant.
//!
//! - **Linux:** the idle policy, `SCHED_IDLE`, by `sched_setattr` on the calling thread [B: sched(7);
//!   sched_setattr(2)]: "Idle tasks are by definition preempted by non-idle tasks" (the kernel's fair class),
//!   so a shard that wakes on a CPU a pool thread holds runs at once, and a CPU running only idle-policy
//!   threads counts as idle where the scheduler places a waking thread. Measured on Linux 7.2 (EEVDF), a shard
//!   sharing one core with four spinning threads woke with a p99 lateness of 1.64 ms beside threads at its
//!   own priority, 1.44 ms at nice +10, 1.66 ms under `SCHED_BATCH`, 1.45 ms under both (the policy this pool
//!   was planned with), and 0.20 ms under `SCHED_IDLE`, its calm rounds 0.21 to 0.51 ms
//!   (`benches/compute_priority.rs`, `ONE_CORE=4`, 2026-11-04): a waking task preempts a lower-weight one
//!   only where its slice has run out, so nice and batch leave a shard waiting up to a slice. An idle-policy
//!   thread still runs whenever its CPU has nothing else, and within the process's cgroup it gets what the
//!   shards and the blocking pool leave. Its nice value goes [`INCREMENT`] above the thread's own too, for
//!   the two fallbacks: where the call is refused (a kernel before 3.14 has none; a seccomp policy may refuse
//!   it) the batch policy at that nice value, and failing that the nice value alone, by `setpriority` on the
//!   thread's id. A thread may lower its own priority unprivileged, and its policy and nice value are its own
//!   on Linux (sched(7), "The nice value").
//! - **macOS:** the background QoS class, `pthread_set_qos_class_self_np` [B: Apple, Energy Efficiency Guide
//!   for Mac Apps, "Prioritize Work at the Task Level"]: the system ranks the thread's CPU, I/O and timers
//!   below every higher class's, and runs background work on the efficiency cores. Measured on a 14-core Mac
//!   Studio, a shard at the default class beside 13 spinning threads woke with a p99 lateness of 3.2 ms
//!   beside threads at its own class, 2.6 ms beside threads at utility (the class this pool was planned
//!   with) and 0.37 ms beside threads at background, its calm rounds 0.20 to 0.50 ms; a shard at utility
//!   (as a runner starts its commands) 6.8 ms beside utility and 0.50 ms beside background
//!   (`benches/compute_priority.rs`, `SHARD_QOS=default`, 2026-11-04). The pool runs on the efficiency
//!   cores where the machine is busy: its jobs take longer there, and the shards don't wait.
//! - **Windows:** `THREAD_PRIORITY_BELOW_NORMAL` by `SetThreadPriority` [B: Microsoft Learn, "Scheduling
//!   Priorities": "Background threads, particularly those that are processor intensive, can be set to
//!   THREAD_PRIORITY_BELOW_NORMAL or THREAD_PRIORITY_LOWEST, to ensure that they can be preempted when
//!   necessary"].
//!
//! Each only ever lowers: a thread already below the target (a process started under `nice`, at background
//! QoS, at a lower level) keeps what it has, as `nice` adds its increment to what a process runs at.
#![allow(unsafe_code)]

use crate::compute::{Priority, Refusal};

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

/// Lowers the calling thread's priority as its OS allows (the module's notes): what it runs at now, or
/// the call the OS refused.
pub(crate) fn lower_current_thread() -> Result<Priority, Refusal> {
    platform::lower()
}

/// The calling thread's priority as its OS reports it.
pub(crate) fn of_current_thread() -> Priority {
    platform::of_current()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{INCREMENT, MAX_NICE};
    use crate::compute::{Priority, Refusal};

    /// The idle policy by `sched_setattr`; the batch policy at a nice value [`INCREMENT`] above the thread's
    /// own (at most [`MAX_NICE`]) where the idle policy is refused, and that nice value alone where both are.
    /// What the thread runs at then, as the OS reports it (the kernel keeps no nice value an idle-policy
    /// thread is given).
    pub(super) fn lower() -> Result<Priority, Refusal> {
        let thread = rustix::thread::gettid();
        let now = rustix::process::getpriority_process(Some(thread)).unwrap_or(0);
        let nice = now.saturating_add(INCREMENT).min(MAX_NICE);
        if set(libc::SCHED_IDLE, nice) || set(libc::SCHED_BATCH, nice) {
            return Ok(of_current());
        }
        nice_alone(nice)
    }

    /// The calling thread's policy and nice value, set together by `sched_setattr`: whether the kernel took
    /// them.
    fn set(policy: libc::c_int, nice: i32) -> bool {
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
            sched_runtime: 0,
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

    /// The calling thread's policy (`sched_getscheduler`) and nice value (`getpriority` on its id).
    pub(super) fn of_current() -> Priority {
        let pid: libc::pid_t = 0;
        // SAFETY: no pointer passes; pid 0 names the calling thread, and the call only reads its policy
        // (sched_getscheduler(2)).
        let policy = unsafe { libc::sched_getscheduler(pid) };
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
    use crate::compute::{Priority, QOS_CLASS_BACKGROUND, Refusal};

    /// The background QoS class for the calling thread, unless it runs at that class or below already.
    pub(super) fn lower() -> Result<Priority, Refusal> {
        if let Priority::Qos { class } = of_current()
            && class != 0
            && class <= QOS_CLASS_BACKGROUND
        {
            return Ok(Priority::Qos { class });
        }
        let (target, class) = (libc::qos_class_t::QOS_CLASS_BACKGROUND, QOS_CLASS_BACKGROUND);
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
    use crate::compute::{Priority, Refusal};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };

    /// Format: what `GetThreadPriority` returns when it fails, `THREAD_PRIORITY_ERROR_RETURN` (MAXLONG,
    /// winbase.h).
    const ERROR_RETURN: i32 = i32::MAX;

    /// `THREAD_PRIORITY_BELOW_NORMAL` for the calling thread, unless it runs at that level or below already.
    pub(super) fn lower() -> Result<Priority, Refusal> {
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
    use crate::compute::{Priority, Refusal};

    pub(super) fn lower() -> Result<Priority, Refusal> {
        Err(Refusal {
            call: "no priority call on this OS",
            code: None,
        })
    }

    pub(super) fn of_current() -> Priority {
        Priority::Unknown
    }
}
