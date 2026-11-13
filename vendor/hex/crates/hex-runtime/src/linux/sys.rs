//! The system calls Hex makes that Rust can't make safely (CLAUDE.md §12 names this file). Each
//! `unsafe` block states the invariant that makes it sound.

// The one place `unsafe` is allowed (CLAUDE.md §12); every block below carries its SAFETY comment.
#![allow(unsafe_code)]

use std::ffi::{CString, c_char, c_int};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use rustix::process::{Resource, Rlimit, WaitId, WaitIdOptions};

use super::oci::OciError;
use super::oci::sys::{ChildStack, block_signals, clone_vfork, default_signals, execve_each};

/// Ignores SIGINT and SIGTERM in this process, so a wrapper (`hex box run`) outlives the signals
/// its caller sends the whole process group and stays alive to clean the box up; the boxed child,
/// in the same group, still receives them. Best-effort: a failure leaves the default disposition.
pub fn ignore_int_term() {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: signal(2) with SIG_IGN and a valid signal number; it installs a disposition and
        // writes nothing of ours. The return value isn't needed (best-effort).
        unsafe {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
}

/// Marks every descriptor from 3 up close-on-exec, so the program this process execs next holds its
/// stdio alone: whatever it inherited by accident, from a window anywhere above it, never reaches foreign
/// code (CVE-2024-21626's class; runc v1.5.2 `libcontainer/init_linux.go:300-306`, crun 1.30.1
/// `src/libcrun/container.c:1096`). `close_range(2)`; where the kernel refuses it (before Linux 5.11, or under
/// a filter) each descriptor `/proc/self/fd` lists, as crun falls back (`src/libcrun/utils.c:1959-1975`); with
/// neither the error, so the caller refuses to run the program. System calls alone on the stack, allocating
/// nothing, so the child of a fork or a vfork-style clone in a multi-threaded process may make them.
pub fn mark_cloexec_from_3() -> rustix::io::Result<()> {
    // SAFETY: close_range(2) with CLOSE_RANGE_CLOEXEC closes nothing: it sets a flag on the descriptors
    // this process has from 3 up, so no descriptor anything in it owns is invalidated.
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if r == 0 {
        return Ok(());
    }
    // SAFETY: __errno_location returns this thread's errno.
    let errno = rustix::io::Errno::from_raw_os_error(unsafe { *libc::__errno_location() });
    match errno {
        rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::PERM => {
            mark_cloexec_by_proc()
        }
        e => Err(e),
    }
}

/// [`mark_cloexec_from_3`]'s way where the kernel refuses `close_range`: each descriptor `/proc/self/fd`
/// lists, read onto the stack.
fn mark_cloexec_by_proc() -> rustix::io::Result<()> {
    let dir = rustix::fs::open(
        c"/proc/self/fd",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    let mut entries = rustix::fs::RawDir::new(&dir, &mut buf);
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .ok()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        if fd < 3 || fd == dir.as_raw_fd() {
            continue;
        }
        // SAFETY: `fd` was just listed as open here, and is borrowed only for this call: a descriptor
        // closed since is EBADF, and setting the flag on one reused since is what this is for.
        let listed = unsafe { BorrowedFd::borrow_raw(fd) };
        match rustix::io::fcntl_setfd(listed, rustix::io::FdFlags::CLOEXEC) {
            Ok(()) | Err(rustix::io::Errno::BADF) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Makes `cmd`'s child mark every descriptor from 3 up close-on-exec before it executes
/// ([`mark_cloexec_from_3`]): a foreign program the agent starts itself, a hook.
pub fn sweep_on_exec(cmd: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls are
    // allowed. It makes the system calls of mark_cloexec_from_3, allocates nothing, and turns a failure
    // into the errno std reports.
    unsafe {
        cmd.pre_exec(|| mark_cloexec_from_3().map_err(std::io::Error::from));
    }
}

/// Makes `cmd`'s child join the cgroup whose `cgroup.procs` is open as `procs` before it executes, and
/// take the descriptor limit `nofile` where one is given (else it keeps the one it inherits), so the
/// program and everything it starts are in the box, under its limit, from their first instruction; and
/// marks every descriptor from 3 up close-on-exec ([`mark_cloexec_from_3`]).
pub fn join_cgroup_on_exec(cmd: &mut Command, procs: Arc<OwnedFd>, nofile: Option<Rlimit>) {
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls
    // are allowed. It makes one write(2) on a descriptor opened before the fork (kept open by the
    // `Arc` the closure owns), at most one setrlimit(2) of a value made before it and the system calls of
    // mark_cloexec_from_3, allocates nothing, and turns a failure into the errno std reports.
    unsafe {
        cmd.pre_exec(move || {
            match rustix::io::write(&*procs, b"0") {
                Ok(1) => {}
                Ok(_) => return Err(std::io::Error::from(rustix::io::Errno::IO)),
                Err(e) => return Err(std::io::Error::from(e)),
            }
            if let Some(limit) = nofile {
                rustix::process::setrlimit(Resource::Nofile, limit)
                    .map_err(std::io::Error::from)?;
            }
            mark_cloexec_from_3().map_err(std::io::Error::from)
        });
    }
}

/// `struct clone_args` through its `cgroup` field (`CLONE_ARGS_SIZE_VER2`, 88 bytes), as Linux's
/// `include/uapi/linux/sched.h` lays it out: every field an `__aligned_u64`.
#[repr(C, align(8))]
pub(crate) struct CloneArgs {
    pub(crate) flags: u64,
    pub(crate) pidfd: u64,
    pub(crate) child_tid: u64,
    pub(crate) parent_tid: u64,
    pub(crate) exit_signal: u64,
    pub(crate) stack: u64,
    pub(crate) stack_size: u64,
    pub(crate) tls: u64,
    pub(crate) set_tid: u64,
    pub(crate) set_tid_size: u64,
    pub(crate) cgroup: u64,
}

/// `CLONE_INTO_CGROUP` (Linux 5.7, `include/uapi/linux/sched.h`): the child starts in the cgroup
/// `clone_args.cgroup` names. libc's constant is an `i32` that overflows (libc issue 3584), so it is
/// written here.
pub(crate) const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// A box's program, ready to clone: every string and descriptor the child uses, made before the
/// clone. The child of a multi-threaded process may make only async-signal-safe calls until it
/// executes (signal-safety(7); fork(2), NOTES), so it allocates nothing.
pub struct Spawn {
    /// The paths to execute, in order: the program as named when it holds a `/`, else its name in each
    /// `PATH` folder, as execvp(3) tries them.
    paths: Vec<CString>,
    argv: Vec<CString>,
    envp: Vec<CString>,
    cwd: Option<CString>,
    /// stdin, stdout and stderr, each at fd 3 or above, so no `dup2` onto 0, 1 or 2 overwrites another.
    stdio: [OwnedFd; 3],
    /// The descriptor limit the program starts under.
    nofile: Rlimit,
    /// The locked-memory limit the program starts under where it isn't the one it inherits ([`Spawn::with_memlock`]).
    memlock: Option<Rlimit>,
}

impl Spawn {
    /// `stdio` is moved to descriptors at 3 or above where it isn't there already.
    pub fn new(
        paths: Vec<CString>,
        argv: Vec<CString>,
        envp: Vec<CString>,
        cwd: Option<CString>,
        stdio: [OwnedFd; 3],
        nofile: Rlimit,
    ) -> std::io::Result<Self> {
        let [a, b, c] = stdio;
        // Each stdio fd ends up at 3 or above and close-on-exec, so the child's `dup2` onto 0, 1 and
        // 2 makes the only copies the workload keeps (dup2 clears close-on-exec on its target) and
        // the original closes on exec rather than leaking in beside it. One already above 2 keeps its
        // number but is marked close-on-exec; one at 0..=2 is moved above 2 (which sets it too).
        let above = |fd: OwnedFd| -> std::io::Result<OwnedFd> {
            if fd.as_raw_fd() > 2 {
                rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
                Ok(fd)
            } else {
                Ok(rustix::io::fcntl_dupfd_cloexec(&fd, 3)?)
            }
        };
        Ok(Self {
            paths,
            argv,
            envp,
            cwd,
            stdio: [above(a)?, above(b)?, above(c)?],
            nofile,
            memlock: None,
        })
    }

    /// The program started under `memlock` as its RLIMIT_MEMLOCK, where one is given, else the one it inherits: a VMM
    /// that pins its guest's RAM for DMA, its soft limit raised to its hard one (`firecracker::vfio`).
    pub fn with_memlock(self, memlock: Option<Rlimit>) -> Self {
        Self { memlock, ..self }
    }

    /// Its stdio back, for a caller that starts the program another way.
    pub fn into_stdio(self) -> [OwnedFd; 3] {
        self.stdio
    }
}

/// The single parent-side execution gate. Its byte is sent only after the existing native owner
/// holds the child and the canonical identity is durable. Closing it cancels before execve.
pub struct HeldGate {
    socket: OwnedFd,
    deadline: libc::timespec,
}

impl HeldGate {
    /// Development-only observation of THIS child's cgroup/descriptor-isolation phase. Peek
    /// preserves byte 2 for release's original handshake; it never sends authorization byte 1.
    #[cfg(any(test, feature = "native-gate-test"))]
    pub fn wait_isolated(&self) -> std::io::Result<()> {
        held_receive(self.socket.as_raw_fd(), self.deadline, 2, true)?;
        Ok(())
    }

    pub fn release(self) -> std::io::Result<()> {
        // Acknowledges the child-side cgroup join and inherited-writer closure, including on the
        // legacy-clone fallback. The caller already owns the pidfd before any wait can fail.
        held_wait(self.socket.as_raw_fd(), self.deadline, 2)?;
        match rustix::net::send(
            &self.socket,
            &[1],
            rustix::net::SendFlags::NOSIGNAL | rustix::net::SendFlags::DONTWAIT,
        )? {
            1 => Ok(()),
            _ => Err(std::io::Error::from(rustix::io::Errno::IO)),
        }
    }
}

/// A child plus its atomically created pidfd. This is transferred, without allocation or narrowing
/// the native PID, into the SAME preallocated Supervisor receipt before fallible observation.
pub struct HeldChild {
    pid: libc::c_long,
    pidfd: OwnedFd,
    gate: HeldGate,
}

struct HeldStart<'a> {
    spawn: &'a Spawn,
    argv: &'a [*const c_char],
    envp: &'a [*const c_char],
    reader: RawFd,
    report: Option<RawFd>,
    deadline: libc::timespec,
}

impl HeldChild {
    pub fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }

    pub fn into_parts(self) -> (libc::c_long, OwnedFd, HeldGate) {
        (self.pid, self.pidfd, self.gate)
    }
}

/// Unlike the ordinary vfork exec path, a held child has COW memory and returns immediately with a
/// pidfd while foreign code is still gated. Clone3 places it in the cgroup atomically. A kernel or
/// sandbox without that operation uses clone(CLONE_PIDFD), then joins in the child before the gate;
/// it never launches a foreign instruction outside the admitted native envelope.
pub fn spawn_held(
    cgroup: BorrowedFd<'_>,
    procs: BorrowedFd<'_>,
    spawn: Spawn,
    report: Option<OwnedFd>,
    gate_budget: Duration,
) -> std::io::Result<HeldChild> {
    if report.as_ref().is_some_and(|fd| fd.as_raw_fd() < 3) {
        return Err(rustix::io::Errno::BADF.into());
    }
    let deadline = held_deadline(gate_budget)?;
    let argv = held_pointers(&spawn.argv)?;
    let envp = held_pointers(&spawn.envp)?;
    let (reader, writer) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )?;
    // stdio sources and both gate ends must stay above the dup2 targets even if this agent started
    // with a standard descriptor closed. There are no descriptor allocations after the clone.
    let above = |fd: OwnedFd| -> rustix::io::Result<OwnedFd> {
        if fd.as_raw_fd() > 2 {
            Ok(fd)
        } else {
            rustix::io::fcntl_dupfd_cloexec(&fd, 3)
        }
    };
    let reader = above(reader)?;
    let writer = above(writer)?;
    let blocked = block_signals().map_err(|error| std::io::Error::other(error.to_string()))?;
    // SAFETY: spawn and both pointer arrays were prepared before cloning and live until this call
    // returns. The COW child touches only that prepared memory and async-signal-safe system calls,
    // then execve or _exit; no child returns to Rust ownership/destructor paths.
    let child = unsafe {
        held_clone(
            cgroup,
            procs,
            &HeldStart {
                spawn: &spawn,
                argv: &argv,
                envp: &envp,
                reader: reader.as_raw_fd(),
                report: report.as_ref().map(AsRawFd::as_raw_fd),
                deadline,
            },
        )
    };
    drop(blocked);
    let (pid, pidfd) = child?;
    Ok(HeldChild {
        pid,
        pidfd,
        gate: HeldGate {
            socket: writer,
            deadline,
        },
    })
}

fn held_pointers(strings: &[CString]) -> std::io::Result<Vec<*const c_char>> {
    let room = strings
        .len()
        .checked_add(1)
        .ok_or(rustix::io::Errno::OVERFLOW)?;
    let mut pointers = Vec::new();
    pointers
        .try_reserve_exact(room)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
    for string in strings {
        pointers.push(string.as_ptr());
    }
    pointers.push(std::ptr::null());
    Ok(pointers)
}

/// A protocol deadline, derived solely from the caller's explicit remaining create budget. The
/// nanoseconds/second conversion is the POSIX timespec unit, not a guessed timeout constant.
fn held_deadline(budget: Duration) -> std::io::Result<libc::timespec> {
    held_deadline_at(
        rustix::time::clock_gettime(rustix::time::ClockId::Monotonic),
        budget,
    )
}

fn held_deadline_at(
    now: rustix::time::Timespec,
    budget: Duration,
) -> std::io::Result<libc::timespec> {
    if budget.is_zero() {
        return Err(std::io::Error::from(rustix::io::Errno::TIMEDOUT));
    }
    let nanos = now
        .tv_nsec
        .checked_add(i64::from(budget.subsec_nanos()))
        .ok_or(rustix::io::Errno::OVERFLOW)?;
    let seconds = now
        .tv_sec
        .checked_add(i64::try_from(budget.as_secs()).map_err(|_| rustix::io::Errno::OVERFLOW)?)
        .and_then(|seconds| seconds.checked_add(nanos.checked_div(1_000_000_000)?))
        .ok_or(rustix::io::Errno::OVERFLOW)?;
    Ok(libc::timespec {
        tv_sec: seconds,
        tv_nsec: nanos
            .checked_rem(1_000_000_000)
            .ok_or(rustix::io::Errno::OVERFLOW)?,
    })
}

/// # Safety
/// Only from spawn_held with fully prepared, immutable C strings and pointers. The child must never
/// return, allocate, access a lock or run a Rust destructor after either clone returns zero.
unsafe fn held_clone(
    cgroup: BorrowedFd<'_>,
    procs: BorrowedFd<'_>,
    start: &HeldStart<'_>,
) -> std::io::Result<(libc::c_long, OwnedFd)> {
    let mut pidfd: c_int = -1;
    let args = CloneArgs {
        flags: u64::from(libc::CLONE_PIDFD as u32) | CLONE_INTO_CGROUP,
        pidfd: std::ptr::from_mut(&mut pidfd).addr() as u64,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: u64::from(libc::SIGCHLD as u32),
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: u64::try_from(cgroup.as_raw_fd()).map_err(|_| rustix::io::Errno::BADF)?,
    };
    // SAFETY: clone_args has Linux's size/alignment and only a live output pointer and cgroup fd.
    // No VM/FILES/VFORK sharing: the child keeps this thread's COW stack and runs held_child only.
    let pid = unsafe { libc::syscall(libc::SYS_clone3, &args, std::mem::size_of::<CloneArgs>()) };
    if pid == 0 {
        // SAFETY: this is the newly created COW child and every referenced object predates it.
        unsafe { held_child(start, None) }
    }
    let pid = if pid == -1 {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::E2BIG | libc::EINVAL | libc::EPERM) => {}
            _ => return Err(error),
        }
        // x86_64 and arm64 place parent_tid (the CLONE_PIDFD output) in argument three. stack=0
        // preserves the copied stack; the unused child_tid and tls arguments are both zero.
        // SAFETY: CLONE_PIDFD is atomic with child creation, unlike fork + pidfd_open. All pointers
        // are live on this thread's stack. The COW child immediately takes the no-return branch.
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                libc::CLONE_PIDFD | libc::SIGCHLD,
                0usize,
                std::ptr::from_mut(&mut pidfd),
                0usize,
                0usize,
            )
        };
        if pid == 0 {
            // SAFETY: same prepared COW child; it must join procs before any foreign execution.
            unsafe { held_child(start, Some(procs)) }
        }
        if pid == -1 {
            return Err(std::io::Error::last_os_error());
        }
        pid
    } else {
        pid
    };
    // SAFETY: either successful CLONE_PIDFD call installed exactly one owned descriptor in pidfd.
    // No conversion or fallible operation intervenes between native creation and this ownership.
    Ok((pid, unsafe { OwnedFd::from_raw_fd(pidfd) }))
}

/// # Safety
/// Only in the held COW child, with buffers and descriptors prepared before creation. Never returns.
unsafe fn held_child(start: &HeldStart<'_>, procs: Option<BorrowedFd<'_>>) -> ! {
    let joined = procs.is_none_or(|procs| matches!(rustix::io::write(procs, b"0"), Ok(1)));
    let [stdin, stdout, stderr] = &start.spawn.stdio;
    let mut keep = [
        stdin.as_raw_fd(),
        stdout.as_raw_fd(),
        stderr.as_raw_fd(),
        start.reader,
        // None admits no extra descriptor; repeating the existing gate keeps the set fixed-size.
        start.report.unwrap_or(start.reader),
    ];
    keep.sort_unstable();
    // Closing, not merely CLOEXEC, is necessary here: a concurrently cloned sibling must not keep
    // this gate's writer alive while it too waits. EOF after parent death then cancels every child.
    if joined
        && held_close_unexpected(&keep).is_ok()
        && held_ready(start.reader).is_ok()
        && held_wait(start.reader, start.deadline, 1).is_ok()
    {
        // SAFETY: same prepared exec arguments as ordinary child; the gate and native envelope
        // have now been established. child does only signal/descriptor/limit/cwd/execve syscalls.
        let _ = unsafe { child_with_fd(start.spawn, start.argv, start.envp, start.report) };
    }
    // SAFETY: _exit ends only this child without inherited Rust destructors or atexit handlers.
    unsafe { libc::_exit(127) }
}

fn held_ready(reader: RawFd) -> rustix::io::Result<()> {
    let byte = 2u8;
    // SAFETY: the one prepared gate endpoint and one readable stack byte; this cannot block or
    // raise SIGPIPE. Byte 2 acknowledges joining/descriptor isolation, byte 1 authorizes exec.
    let sent = unsafe {
        libc::send(
            reader,
            std::ptr::from_ref(&byte).cast(),
            1,
            libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
        )
    };
    if sent == 1 {
        Ok(())
    } else {
        Err(if sent < 0 {
            held_errno()
        } else {
            rustix::io::Errno::IO
        })
    }
}

fn held_wait(reader: RawFd, deadline: libc::timespec, expected: u8) -> rustix::io::Result<()> {
    held_receive(reader, deadline, expected, false)
}

fn held_receive(
    reader: RawFd,
    deadline: libc::timespec,
    expected: u8,
    peek: bool,
) -> rustix::io::Result<()> {
    let remaining = held_remaining(deadline)?;
    let mut fd = libc::pollfd {
        fd: reader,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd and a valid finite relative timespec; no signal-mask change. Every
    // signal was blocked before clone, so an interrupted wait is cancellation rather than a retry.
    let ready = unsafe { libc::ppoll(&mut fd, 1, &remaining, std::ptr::null()) };
    if ready != 1 {
        return Err(if ready == 0 {
            rustix::io::Errno::TIMEDOUT
        } else {
            held_errno()
        });
    }
    held_remaining(deadline)?;
    let mut byte = 0u8;
    // SAFETY: a single stack byte is writable; this is the retained gate reader. DONTWAIT bounds
    // the read even on an unexpected readiness transition; EOF or any other byte cancels.
    // The private test observation may only PEEK; consuming production callers pass false.
    let read = unsafe {
        libc::recv(
            reader,
            std::ptr::from_mut(&mut byte).cast(),
            1,
            libc::MSG_DONTWAIT | if peek { libc::MSG_PEEK } else { 0 },
        )
    };
    if read == 1 && byte == expected {
        Ok(())
    } else {
        Err(if read < 0 {
            held_errno()
        } else {
            rustix::io::Errno::CANCELED
        })
    }
}

fn held_remaining(deadline: libc::timespec) -> rustix::io::Result<libc::timespec> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC writes exactly this live stack timespec and takes no lock/allocator.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Err(held_errno());
    }
    held_remaining_at(deadline, now)
}

fn held_remaining_at(
    deadline: libc::timespec,
    now: libc::timespec,
) -> rustix::io::Result<libc::timespec> {
    let seconds = deadline
        .tv_sec
        .checked_sub(now.tv_sec)
        .ok_or(rustix::io::Errno::OVERFLOW)?;
    let nanos = deadline
        .tv_nsec
        .checked_sub(now.tv_nsec)
        .ok_or(rustix::io::Errno::OVERFLOW)?;
    let remaining = if nanos < 0 {
        libc::timespec {
            tv_sec: seconds.checked_sub(1).ok_or(rustix::io::Errno::OVERFLOW)?,
            tv_nsec: nanos
                .checked_add(1_000_000_000)
                .ok_or(rustix::io::Errno::OVERFLOW)?,
        }
    } else {
        libc::timespec {
            tv_sec: seconds,
            tv_nsec: nanos,
        }
    };
    if remaining.tv_sec < 0 || remaining.tv_sec == 0 && remaining.tv_nsec == 0 {
        return Err(rustix::io::Errno::TIMEDOUT);
    }
    Ok(remaining)
}

fn held_errno() -> rustix::io::Errno {
    // SAFETY: errno is this thread's (in the COW child, no other thread can mutate it).
    rustix::io::Errno::from_raw_os_error(unsafe { *libc::__errno_location() })
}

fn held_close_unexpected(keep: &[RawFd; 5]) -> rustix::io::Result<()> {
    let mut first = 3u32;
    for fd in keep {
        let fd = u32::try_from(*fd).map_err(|_| rustix::io::Errno::BADF)?;
        if fd > first
            && !held_close_range(first, fd.checked_sub(1).ok_or(rustix::io::Errno::OVERFLOW)?)?
        {
            return held_close_by_proc(keep);
        }
        first = fd.checked_add(1).ok_or(rustix::io::Errno::OVERFLOW)?;
    }
    if held_close_range(first, u32::MAX)? {
        Ok(())
    } else {
        held_close_by_proc(keep)
    }
}

fn held_close_range(first: u32, last: u32) -> rustix::io::Result<bool> {
    // SAFETY: no shared fd table (CLONE_FILES absent). These ranges exclude the fixed retained
    // descriptors and 0..=2. Only this no-return child loses the other inherited descriptors.
    if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } == 0 {
        return Ok(true);
    }
    match held_errno() {
        rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::PERM => Ok(false),
        error => Err(error),
    }
}

fn held_close_by_proc(keep: &[RawFd; 5]) -> rustix::io::Result<()> {
    let dir = rustix::fs::open(
        c"/proc/self/fd",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    // The existing descriptor sweep's getdents buffer. No child heap allocation; Linux's signed
    // descriptor namespace bounds the finite listing, and the child opens no other descriptors.
    let mut buffer = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    let mut entries = rustix::fs::RawDir::new(&dir, &mut buffer);
    for _ in 0..=c_int::MAX {
        let Some(entry) = entries.next() else {
            return Ok(());
        };
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .ok()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        if fd < 3 || fd == dir.as_raw_fd() || keep.contains(&fd) {
            continue;
        }
        // SAFETY: the fd is open in this child's private copied table and is excluded from the
        // retained set. No Rust owner is subsequently dropped in this no-return child. Linux closes
        // even on EINTR; an error cancels, never retries a potentially reused descriptor.
        if unsafe { libc::close(fd) } != 0 {
            return Err(held_errno());
        }
    }
    Err(rustix::io::Errno::OVERFLOW)
}

/// What [`clone_into_cgroup`] did.
pub enum Cloned {
    /// The program runs: its pid, and the pidfd the clone made for it.
    Started { pid: u32, pidfd: OwnedFd },
    /// This kernel or sandbox has no clone into a cgroup: `ENOSYS` (before Linux 5.3, or a seccomp
    /// profile like Docker's default, which answers `clone3` so) or `E2BIG` (before 5.7, which doesn't
    /// know the `cgroup` field). The spawn comes back for the caller's fork path.
    Unsupported(Spawn),
}

std::thread_local! {
    /// The stack a process box's vfork-style clone runs its child on, mapped once per agent thread that
    /// clones and reused ([`ChildStack`]).
    static STACK: std::cell::RefCell<Option<ChildStack>> = const { std::cell::RefCell::new(None) };
}

/// Clones `spawn`'s program straight into the cgroup open as `cgroup`, as posix_spawn starts a program:
/// `clone3(CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP | CLONE_PIDFD)` from the calling thread
/// ([`clone_vfork`], goal 5's trampoline). The program is in the box from its first instruction, with no
/// `cgroup.procs` write and the kernel's migration locks it takes, and nothing of the node agent is
/// copied for it: the child runs on a stack of its own in the agent's memory until its execve, and this
/// thread waits until then (decision 7 of docs/design/runtime-architecture.md). Returns once the child's
/// execve has replaced its memory, past which the exec can't come back to the caller (the vfork's release,
/// Linux v6.19 fs/exec.c:854, kernel/fork.c:1488); the kernel names the program a moment later
/// (fs/exec.c:1242-1246). One that can't execute comes back as its errno, its child reaped.
pub fn clone_into_cgroup(cgroup: BorrowedFd<'_>, spawn: Spawn) -> std::io::Result<Cloned> {
    let argv = pointers(&spawn.argv);
    let envp = pointers(&spawn.envp);
    // Where the child leaves its errno when no path executes. It shares this memory until it executes
    // or exits, and this thread runs again only then (CLONE_VFORK), so the value is read once the clone
    // has returned. No pipe: a vfork's child and its parent need none to tell an exec from a failure.
    let failed = AtomicI32::new(0);
    let run = || {
        // SAFETY: this closure runs only as the vfork-style clone's child, with `argv` and `envp` built
        // by `pointers` over `spawn`'s strings, which outlive the clone.
        let e = unsafe { child(&spawn, &argv, &envp) };
        failed.store(e, Ordering::Relaxed);
        127
    };
    let other = |e: OciError| std::io::Error::other(e.to_string());
    let blocked = block_signals().map_err(other)?;
    let cloned = STACK.with(|stack| -> Result<_, OciError> {
        let mut stack = stack
            .try_borrow_mut()
            .map_err(|_| OciError::Broken("a thread's child stack is in use".to_owned()))?;
        if stack.is_none() {
            *stack = Some(ChildStack::map()?);
        }
        match stack.as_ref() {
            Some(stack) => clone_vfork(cgroup, stack, &run),
            None => Ok(None),
        }
    });
    drop(blocked);
    let Some((pid, pidfd)) = cloned.map_err(other)? else {
        return Ok(Cloned::Unsupported(spawn));
    };
    match failed.load(Ordering::Relaxed) {
        0 => {
            let pid = u32::try_from(pid)
                .map_err(|_| std::io::Error::other("clone3 returned a pid out of range"))?;
            Ok(Cloned::Started { pid, pidfd })
        }
        e => {
            // The child has exited; reap it, so its pid isn't left a zombie.
            let _ = rustix::process::waitid(WaitId::PidFd(pidfd.as_fd()), WaitIdOptions::EXITED);
            Err(std::io::Error::from_raw_os_error(e))
        }
    }
}

/// `strs` as the null-terminated array of pointers execve(2) takes; `strs` must outlive it.
fn pointers(strs: &[CString]) -> Vec<*const c_char> {
    strs.iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// The clone's child: stdio onto 0, 1 and 2, every descriptor from 3 up close-on-exec, the signal
/// state std's `Command` leaves a child ([`default_signals`]: an empty mask, SIGPIPE back to its
/// default, which Rust ignores), its descriptor limit, the working folder, then each path through
/// execve(2), as execvp(3) tries them. System calls alone, on memory made before the clone, which it
/// shares with the node agent's other threads: it takes no lock and allocates nothing. Returns only when
/// the program couldn't start: the errno why.
///
/// # Safety
/// Only in the child of [`clone_into_cgroup`]'s clone, with `argv` and `envp` built by [`pointers`].
unsafe fn child(spawn: &Spawn, argv: &[*const c_char], envp: &[*const c_char]) -> c_int {
    // SAFETY: the ordinary caller has the original prepared C strings/stdio contract. None
    // preserves the complete descriptor sweep; only the held managed path supplies a report FD.
    unsafe { child_with_fd(spawn, argv, envp, None) }
}

/// The SAME exec body; its sole optional descriptor is the admitted managed GPU report writer.
/// # Safety
/// Same prepared child contract as `child`. Some is only the held child's final owned report FD,
/// retained by its exact allowlist in its private copied descriptor table, never a caller raw FD.
unsafe fn child_with_fd(
    spawn: &Spawn,
    argv: &[*const c_char],
    envp: &[*const c_char],
    report: Option<RawFd>,
) -> c_int {
    // SAFETY: __errno_location returns this thread's errno: the calling thread's, whose memory the child
    // shares while that thread waits in the vfork, so nothing else reads it meanwhile.
    let errno = || unsafe { *libc::__errno_location() };
    for (target, fd) in (0..).zip(&spawn.stdio) {
        // SAFETY: dup2(2) of an open descriptor onto 0, 1 or 2, which clears close-on-exec there.
        if unsafe { libc::dup2(fd.as_raw_fd(), target) } == -1 {
            return errno();
        }
    }
    if let Err(e) = mark_cloexec_from_3() {
        return e.raw_os_error();
    }
    if let Some(report) = report {
        // SAFETY: spawn_held checked >=3, owns this exact duplicate before clone, and the held
        // child's private allowlist kept it open. Borrowing changes no ownership or parent flags.
        let report = unsafe { BorrowedFd::borrow_raw(report) };
        let flags = match rustix::io::fcntl_getfd(report) {
            Ok(flags) => flags,
            Err(error) => return error.raw_os_error(),
        };
        if let Err(error) = rustix::io::fcntl_setfd(report, flags & !rustix::io::FdFlags::CLOEXEC) {
            return error.raw_os_error();
        }
    }
    if let Err(e) = default_signals() {
        return e.raw_os_error();
    }
    // The child is a process of its own (no CLONE_THREAD), so the limit is its own, never the agent's.
    if let Err(e) = rustix::process::setrlimit(Resource::Nofile, spawn.nofile) {
        return e.raw_os_error();
    }
    if let Some(limit) = spawn.memlock
        && let Err(e) = rustix::process::setrlimit(Resource::Memlock, limit)
    {
        return e.raw_os_error();
    }
    if let Some(cwd) = &spawn.cwd {
        // SAFETY: chdir(2) with a valid C string.
        if unsafe { libc::chdir(cwd.as_ptr()) } == -1 {
            return errno();
        }
    }
    execve_each(&spawn.paths, argv, envp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: observing byte2 does not consume it, authorize execution or create another
    /// deadline. Release retains its original consume2/send1 sequence after repeated peeks.
    #[test]
    fn observing_isolation_ack_peeks_without_consuming_or_authorizing_exec() {
        let (parent, child) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let gate = HeldGate {
            socket: parent,
            deadline: held_deadline(Duration::from_secs(1)).unwrap(),
        };
        assert_eq!(
            rustix::net::send(&child, &[2], rustix::net::SendFlags::DONTWAIT).unwrap(),
            1
        );
        gate.wait_isolated().unwrap();
        gate.wait_isolated().unwrap();
        let mut byte = [0u8];
        assert_eq!(
            rustix::net::recv(&child, &mut byte, rustix::net::RecvFlags::DONTWAIT),
            Err(rustix::io::Errno::AGAIN)
        );
        gate.release().unwrap();
        assert_eq!(
            rustix::net::recv(&child, &mut byte, rustix::net::RecvFlags::DONTWAIT).unwrap(),
            (1, 1)
        );
        assert_eq!(byte, [1]);
    }

    /// AC-T7.4.1: wrong phase, EOF or the original expired deadline are observation failure;
    /// none may be accepted as isolation or send the foreign execution authorization byte.
    #[test]
    fn isolation_ack_wrong_byte_eof_and_expired_original_deadline_refuse() {
        for mode in [0, 1, 2] {
            let (parent, child) = rustix::net::socketpair(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::STREAM,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            let gate = HeldGate {
                socket: parent,
                deadline: if mode == 2 {
                    libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    }
                } else {
                    held_deadline(Duration::from_secs(1)).unwrap()
                },
            };
            if mode == 0 {
                rustix::net::send(&child, &[3], rustix::net::SendFlags::DONTWAIT).unwrap();
                assert!(gate.wait_isolated().is_err());
                let mut byte = [0u8];
                assert_eq!(
                    rustix::net::recv(&gate.socket, &mut byte, rustix::net::RecvFlags::DONTWAIT)
                        .unwrap(),
                    (1, 1)
                );
                assert_eq!(byte, [3], "even a rejected observation is nonconsuming");
                assert_eq!(
                    rustix::net::recv(&child, &mut byte, rustix::net::RecvFlags::DONTWAIT),
                    Err(rustix::io::Errno::AGAIN)
                );
            } else if mode == 1 {
                drop(child);
                assert!(gate.wait_isolated().is_err());
            } else {
                rustix::net::send(&child, &[2], rustix::net::SendFlags::DONTWAIT).unwrap();
                let error = gate.wait_isolated().unwrap_err();
                assert_eq!(
                    error.raw_os_error(),
                    Some(rustix::io::Errno::TIMEDOUT.raw_os_error())
                );
            }
        }
    }

    /// AC-T7.4.1: the launch gate is finite, uses the caller's budget and never wraps a timespec.
    #[test]
    fn a_held_launch_carries_one_finite_deadline_across_the_handshake() {
        let now = rustix::time::Timespec {
            tv_sec: 17,
            tv_nsec: 999_999_999,
        };
        assert!(held_deadline_at(now, Duration::ZERO).is_err());
        assert!(held_deadline_at(now, Duration::MAX).is_err());
        let deadline = held_deadline_at(now, Duration::from_nanos(2)).unwrap();
        assert_eq!((deadline.tv_sec, deadline.tv_nsec), (18, 1));
        let remaining = held_remaining_at(
            deadline,
            libc::timespec {
                tv_sec: 17,
                tv_nsec: 999_999_999,
            },
        )
        .unwrap();
        assert_eq!((remaining.tv_sec, remaining.tv_nsec), (0, 2));
        for now in [
            libc::timespec {
                tv_sec: 18,
                tv_nsec: 1,
            },
            libc::timespec {
                tv_sec: 19,
                tv_nsec: 0,
            },
        ] {
            assert_eq!(
                held_remaining_at(deadline, now).unwrap_err(),
                rustix::io::Errno::TIMEDOUT
            );
        }
    }

    /// Before a foreign execve every descriptor from 3 up is close-on-exec, one opened without the flag
    /// included (CVE-2024-21626's class), by `close_range` and by the walk of `/proc/self/fd` a kernel
    /// that refuses it falls back to. Do: open a descriptor without the flag, mark. Expect the flag on it,
    /// each way.
    #[test]
    fn a_stray_descriptor_is_marked_close_on_exec_either_way() {
        for mark in [mark_cloexec_from_3, mark_cloexec_by_proc] {
            let stray = rustix::fs::open(
                c"/dev/null",
                rustix::fs::OFlags::RDONLY,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            let cloexec = |fd: &OwnedFd| {
                rustix::io::fcntl_getfd(fd)
                    .unwrap()
                    .contains(rustix::io::FdFlags::CLOEXEC)
            };
            assert!(!cloexec(&stray));
            mark().unwrap();
            assert!(cloexec(&stray));
        }
    }
}
