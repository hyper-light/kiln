//! The system calls the OCI runtime makes that Rust can't make safely (CLAUDE.md §12 names this
//! file). The runtime re-execs itself for the container init (`hex __oci-init`), so the process that
//! calls [`fork`] is single-threaded — the fork-in-a-threaded-process hazard does not apply here,
//! and the child runs ordinary Rust, not only async-signal-safe calls.

#![allow(unsafe_code)]

use crate::linux::oci::{Failure, OciError};
use crate::linux::sys::{CLONE_INTO_CGROUP, CloneArgs};

/// What [`fork`] returns.
pub enum Fork {
    /// The parent; holds the child's pid.
    Parent(i32),
    /// The child.
    Child,
}

/// Forks. The caller must be single-threaded (the OCI init is, having just re-exec'd).
pub fn fork() -> Result<Fork, OciError> {
    // SAFETY: fork(2) with no arguments. The one real constraint — that a forked child of a
    // multi-threaded process may touch only async-signal-safe state before exec — does not apply:
    // every caller (`hex __oci-init`) has just re-exec'd and is single-threaded, so the child owns a
    // faithful copy of the address space and runs ordinary code. The return value is checked.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(OciError::os("fork", std::io::Error::last_os_error())),
        0 => Ok(Fork::Child),
        child => Ok(Fork::Parent(child)),
    }
}

/// Forks into a cgroup: `clone3` with `CLONE_INTO_CGROUP`, so the child is born in the cgroup that
/// the open directory `cgroup` is, and nothing migrates it there. A migration takes the cgroup
/// threadgroup lock, whose writer can wait out a whole RCU grace period (docs/design/runtime.md); a
/// birth in the cgroup takes no such lock. `None` when this kernel or sandbox has no such clone:
/// `ENOSYS` (before Linux 5.3, or a seccomp profile like Docker's default, which answers `clone3`
/// so) or `E2BIG` (before 5.7, which doesn't know the `cgroup` field). The caller must be
/// single-threaded, as for [`fork`].
pub fn fork_into_cgroup(cgroup: std::os::fd::BorrowedFd<'_>) -> Result<Option<Fork>, OciError> {
    use std::os::fd::AsRawFd;
    let fd = u64::try_from(cgroup.as_raw_fd())
        .map_err(|_| OciError::os("clone3 into a cgroup", "a negative file descriptor"))?;
    let exit_signal = u64::try_from(libc::SIGCHLD)
        .map_err(|_| OciError::os("clone3 into a cgroup", "SIGCHLD is negative"))?;
    let args = CloneArgs {
        flags: CLONE_INTO_CGROUP,
        pidfd: 0,
        child_tid: 0,
        parent_tid: 0,
        exit_signal,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: fd,
    };
    // SAFETY: clone3(2) with a valid pointer to a fully initialized `CloneArgs` and its exact size,
    // which the kernel only reads. With no stack given, the child goes on with a copy of the caller's
    // stack, as after fork(2), and no flag shares anything with the parent (no CLONE_VM, CLONE_FILES
    // or CLONE_THREAD), so the child owns a faithful copy of the address space. fork's one
    // constraint, that the child of a multi-threaded process may touch only async-signal-safe state,
    // doesn't apply: every caller has re-exec'd and is single-threaded, so no other thread held a lock
    // across the clone, and Hex registers no atfork handler for libc's fork to have run. The return
    // value is checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    match r {
        -1 => {
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOSYS) | Some(libc::E2BIG) => Ok(None),
                _ => Err(OciError::os("clone3 into the container's cgroup", e)),
            }
        }
        0 => Ok(Some(Fork::Child)),
        child => Ok(Some(Fork::Parent(i32::try_from(child).map_err(|_| {
            OciError::os("clone3 into a cgroup", "a pid out of range")
        })?))),
    }
}

/// `CLONE_PIDFD` (Linux 5.2, `include/uapi/linux/sched.h`): the kernel writes a pidfd for the child
/// where `clone_args.pidfd` points. libc's constant is the same value, written here as
/// [`crate::linux::sys`] writes `CLONE_INTO_CGROUP`.
const CLONE_PIDFD: u64 = 0x1000;

/// Forks into a cgroup with a pidfd for the child: `clone3(CLONE_INTO_CGROUP | CLONE_PIDFD)`, so the
/// child is born in the cgroup that the open directory `cgroup` is, and the parent is handed a pidfd
/// that it (or a process it sends it to) watches for the child's end. Like [`fork_into_cgroup`] but for
/// the exec seed (`seed.rs`), whose intermediate hands the pidfd to the node agent rather than waiting
/// on the child itself. `None` when this kernel or sandbox has no clone into a cgroup (`ENOSYS` or
/// `E2BIG`, as [`fork_into_cgroup`]); the caller falls back to [`fork`] and a migration. The caller
/// must be single-threaded, as for [`fork`].
pub fn fork_into_cgroup_pidfd(
    cgroup: std::os::fd::BorrowedFd<'_>,
) -> Result<Option<(Fork, Option<std::os::fd::OwnedFd>)>, OciError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = u64::try_from(cgroup.as_raw_fd())
        .map_err(|_| OciError::os("clone3 into a cgroup", "a negative file descriptor"))?;
    let exit_signal = u64::try_from(libc::SIGCHLD)
        .map_err(|_| OciError::os("clone3 into a cgroup", "SIGCHLD is negative"))?;
    let mut pidfd: libc::c_int = -1;
    let pidfd_ptr = u64::try_from((&raw mut pidfd).addr())
        .map_err(|_| OciError::os("clone3 into a cgroup", "a pointer wider than 64 bits"))?;
    let args = CloneArgs {
        flags: CLONE_INTO_CGROUP | CLONE_PIDFD,
        pidfd: pidfd_ptr,
        child_tid: 0,
        parent_tid: 0,
        exit_signal,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: fd,
    };
    // SAFETY: clone3(2) with a valid pointer to a fully initialized `CloneArgs` and its exact size,
    // which the kernel only reads, and in `pidfd` the address of a live `c_int` it writes the child's
    // pidfd to. With no stack given, the child goes on with a copy of the caller's stack, as after
    // fork(2), and no flag shares memory, files or signal handlers with the parent. fork's one
    // constraint, that the child of a multi-threaded process may touch only async-signal-safe state,
    // doesn't apply: every caller has re-exec'd (the seed) and is single-threaded. The return value is
    // checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    match r {
        -1 => {
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ENOSYS) | Some(libc::E2BIG) => Ok(None),
                _ => Err(OciError::os("clone3 into the container's cgroup", e)),
            }
        }
        0 => Ok(Some((Fork::Child, None))),
        child => {
            // SAFETY: the clone succeeded, so the kernel made `pidfd` for the child and nothing else
            // in this process owns it.
            let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) };
            let pid = i32::try_from(child)
                .map_err(|_| OciError::os("clone3 into a cgroup", "a pid out of range"))?;
            Ok(Some((Fork::Parent(pid), Some(owned))))
        }
    }
}

/// `CLONE_PARENT` (`include/uapi/linux/sched.h`): the child's parent is the caller's parent.
const CLONE_PARENT: u64 = 0x8000;

/// Clones a sibling: `clone3(CLONE_PARENT | CLONE_PIDFD | ns)`, with `CLONE_INTO_CGROUP` when `cgroup`
/// is given, so the child's parent is the caller's parent (the seed's: the node agent), it is born in
/// the cgroup the open directory `cgroup` is and in the fresh namespaces `ns` names (`CLONE_NEW*`;
/// `0` for an exec, which joins the container's by `setns`; the spec's for a create, which makes them),
/// and the caller is handed its pidfd. `exit_signal` is 0 because `clone3` refuses `CLONE_PARENT` with
/// any nonzero `exit_signal` (`clone3_args_valid`, v6.19 `kernel/fork.c:2911-2913`): the child takes the
/// caller's own exit signal instead (`copy_process`), which for a seed the agent spawned is `SIGCHLD`. A
/// create's child is PID 1 of its new pid namespace and the agent's own child at once: `clone(2)`'s
/// ERRORS (man-pages 6.19) once barred `CLONE_PARENT` with `CLONE_NEWPID`, but the kernel dropped that
/// bar (Biederman, "fork: Allow CLONE_PARENT after setns(CLONE_NEWPID)", in the tree since before
/// v3.12), and that ERRORS entry is out of date: v6.19's `copy_process` refuses `CLONE_PARENT` only from
/// an init (`SIGNAL_UNKILLABLE`, `kernel/fork.c:2010-2012`) and `CLONE_NEWUSER` or `CLONE_NEWPID` only
/// together with `CLONE_THREAD` (`2018-2022`) (raw.githubusercontent.com/torvalds/linux/v6.19/
/// kernel/fork.c; the lab's `oci_box` tests, rootless and as root). `None` when this kernel or
/// sandbox has no `clone3` or no clone into a cgroup (`ENOSYS` or `E2BIG`, as [`fork_into_cgroup`]).
/// The caller must be single-threaded, as for [`fork`].
pub fn clone_sibling(
    cgroup: Option<std::os::fd::BorrowedFd<'_>>,
    ns: u64,
) -> Result<Option<Sibling>, OciError> {
    clone_sibling_errno(cgroup, ns)
        .map_err(|e| OciError::os("clone3 the program into the container", e))
}

/// [`clone_sibling`], its refusal the kernel's errno, so a caller can tell one refusal from another: a
/// create seed reads `EINVAL` for `CLONE_EMPTY_MNTNS` as a kernel without it (Linux 7.1,
/// `docs/research/runtime/goal6-mntns.md` §6).
pub fn clone_sibling_errno(
    cgroup: Option<std::os::fd::BorrowedFd<'_>>,
    ns: u64,
) -> Result<Option<Sibling>, rustix::io::Errno> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let (flags, cgroup) = match cgroup {
        Some(dir) => (
            CLONE_PARENT | CLONE_PIDFD | CLONE_INTO_CGROUP | ns,
            u64::try_from(dir.as_raw_fd()).map_err(|_| rustix::io::Errno::BADF)?,
        ),
        None => (CLONE_PARENT | CLONE_PIDFD | ns, 0),
    };
    let mut pidfd: libc::c_int = -1;
    let pidfd_ptr =
        u64::try_from((&raw mut pidfd).addr()).map_err(|_| rustix::io::Errno::OVERFLOW)?;
    let args = CloneArgs {
        flags,
        pidfd: pidfd_ptr,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: 0,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup,
    };
    // SAFETY: clone3(2) with a valid pointer to a fully initialized `CloneArgs` and its exact size,
    // which the kernel only reads, and in `pidfd` the address of a live `c_int` it writes the child's
    // pidfd to. With no stack given, the child goes on with a copy of the caller's stack, as after
    // fork(2), and no flag shares memory, files or signal handlers with the caller. fork's one
    // constraint, that the child of a multi-threaded process may touch only async-signal-safe state,
    // doesn't apply: the caller, the seed, re-exec'd and is single-threaded. The return value is
    // checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    match r {
        -1 => match rustix::io::Errno::from_io_error(&std::io::Error::last_os_error()) {
            Some(rustix::io::Errno::NOSYS | rustix::io::Errno::TOOBIG) => Ok(None),
            Some(e) => Err(e),
            None => Err(rustix::io::Errno::INVAL),
        },
        0 => Ok(Some(Sibling::Child)),
        child => {
            // SAFETY: the clone succeeded, so the kernel made `pidfd` for the child and nothing else
            // in this process owns it.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) };
            let pid = i32::try_from(child).map_err(|_| rustix::io::Errno::RANGE)?;
            Ok(Some(Sibling::Parent { pid, pidfd }))
        }
    }
}

/// What [`clone_sibling`] returns in each process.
pub enum Sibling {
    /// The caller: the sibling's pid and its pidfd.
    Parent {
        pid: i32,
        pidfd: std::os::fd::OwnedFd,
    },
    /// The sibling.
    Child,
}

/// The signal state a program starts with, as std's `Command` leaves a child: an empty signal mask and
/// SIGPIPE at its default (Rust's runtime ignores it, and an ignored signal stays ignored across execve).
/// Async-signal-safe calls only, for the seed's clone's child, which allocates nothing.
pub fn default_signals() -> Result<(), rustix::io::Errno> {
    // SAFETY: `sigset_t` is plain data, initialized by sigemptyset before anything reads it; then
    // pthread_sigmask and signal(2), both async-signal-safe, with valid arguments.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        let r = libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        if r != 0 {
            return Err(rustix::io::Errno::from_raw_os_error(r));
        }
        if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
            return Err(last_errno());
        }
    }
    Ok(())
}

/// execve(2) of each of `paths` in turn, as execvp(3) tries them, with `argv` and `envp` the
/// null-terminated arrays execve takes. Returns only when none ran: the errno that ended it, `EACCES`
/// if a path it may not run came before paths that aren't there, else the last.
pub fn execve_each(
    paths: &[std::ffi::CString],
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> i32 {
    // SAFETY: __errno_location returns this thread's errno.
    let errno = || unsafe { *libc::__errno_location() };
    let mut last = libc::ENOENT;
    let mut denied = false;
    for path in paths {
        // SAFETY: execve(2) with a valid C string and null-terminated arrays of valid C strings (the
        // caller's `argv`/`envp`, built over strings that outlive the call); it returns only on failure.
        unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        match errno() {
            e @ (libc::ENOENT | libc::ENOTDIR) => last = e,
            libc::EACCES => denied = true,
            e => return e,
        }
    }
    if denied { libc::EACCES } else { last }
}

/// `CLONE_VM` and `CLONE_VFORK` (`include/uapi/linux/sched.h`): the child shares the caller's memory,
/// and the caller's thread waits until the child has exec'd or exited.
const CLONE_VM: u64 = 0x100;
const CLONE_VFORK: u64 = 0x4000;

/// The stack a vfork-style clone's child runs on ([`clone_vfork`]): 256 KiB, mapped once per thread that
/// clones and reused, since the child is done with it by the time the clone returns. Its lowest page is a
/// guard, inaccessible, so an overflow faults rather than writing into the parent's memory. The child's
/// frames are shallow (the joins, the credential and capability calls, execve); 256 KiB is a stated
/// bound well above them, not a tuning.
pub struct ChildStack {
    base: *mut libc::c_void,
    len: usize,
}

const CHILD_STACK_BYTES: usize = 256 << 10;

impl ChildStack {
    pub fn map() -> Result<Self, OciError> {
        let page = page_size()?;
        // SAFETY: an anonymous private mapping at an address the kernel picks: it aliases nothing. The
        // result is checked; the guard page is made inaccessible before anything uses the mapping.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                CHILD_STACK_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(OciError::os(
                "mapping a child's stack",
                std::io::Error::last_os_error(),
            ));
        }
        let stack = Self {
            base,
            len: CHILD_STACK_BYTES,
        };
        // SAFETY: the first page of the mapping just made, which nothing references.
        if unsafe { libc::mprotect(base, page, libc::PROT_NONE) } != 0 {
            return Err(OciError::os(
                "guarding a child's stack",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(stack)
    }
}

impl Drop for ChildStack {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, which no clone's child uses any more (each clone returned).
        unsafe {
            libc::munmap(self.base, self.len);
        }
    }
}

/// The size of a page.
pub(crate) fn page_size() -> Result<usize, OciError> {
    // SAFETY: sysconf(3) takes a constant and reads nothing of ours.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(n).map_err(|_| OciError::os("the page size", "sysconf gave none"))
}

/// Every signal blocked on the calling thread until dropped, which restores the mask it had. A vfork-style
/// clone's child inherits the mask, so it runs none of the process's signal handlers before its execve
/// (which resets them), as posix_spawn blocks them.
pub struct Blocked {
    old: libc::sigset_t,
}

pub fn block_signals() -> Result<Blocked, OciError> {
    // SAFETY: `sigset_t` is plain data, filled by sigfillset before pthread_sigmask reads it, and `old`
    // is written by pthread_sigmask; both calls take valid pointers. Checked.
    unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        let r = libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);
        if r != 0 {
            return Err(OciError::os(
                "blocking signals for a clone",
                std::io::Error::from_raw_os_error(r),
            ));
        }
        Ok(Blocked { old })
    }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        // SAFETY: the mask `block_signals` saved, a valid sigset_t.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut());
        }
    }
}

/// Clones a child that shares this process's memory and runs `child` on `stack`, born in the cgroup the
/// open directory `cgroup` is, with a pidfd for it: `clone3(CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP |
/// CLONE_PIDFD)`, as posix_spawn clones. No page of this process is copied for the child, and its execve
/// tears none down; the calling thread waits until the child has exec'd or exited. The caller blocks
/// signals first ([`block_signals`]). `child` makes system calls alone on memory made before the clone,
/// and returns only when one failed, the status to exit with, having reported why as its caller wants
/// (the seed's exec writes it to the program's stderr, [`report`]; a process box's spawn leaves its errno
/// in the memory it shares): it shares this process's memory with the process's other threads, so it may
/// take no lock and allocate nothing. `None` on an architecture without the trampoline below, or a kernel
/// or sandbox without clone3 into a cgroup (`ENOSYS`, `E2BIG`).
pub fn clone_vfork<F: Fn() -> u8>(
    cgroup: std::os::fd::BorrowedFd<'_>,
    stack: &ChildStack,
    child: &F,
) -> Result<Option<(i32, std::os::fd::OwnedFd)>, OciError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    /// The child's start, on its own stack: `child`, then its exit.
    extern "C" fn entry<F: Fn() -> u8>(arg: *mut libc::c_void) -> ! {
        // SAFETY: `arg` is `child`, borrowed by the clone below, alive until this child has exec'd or
        // exited, which the vfork makes its parent thread wait for.
        let child = unsafe { &*arg.cast::<F>().cast_const() };
        exit_child(child())
    }
    let page = page_size()?;
    let fd = u64::try_from(cgroup.as_raw_fd())
        .map_err(|_| OciError::os("clone3 into a cgroup", "a negative file descriptor"))?;
    let mut pidfd: libc::c_int = -1;
    let as_u64 = |v: usize| {
        u64::try_from(v)
            .map_err(|_| OciError::os("clone3 into a cgroup", "a value wider than 64 bits"))
    };
    let args = CloneArgs {
        flags: CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP | CLONE_PIDFD,
        pidfd: as_u64((&raw mut pidfd).addr())?,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: u64::try_from(libc::SIGCHLD)
            .map_err(|_| OciError::os("clone3 into a cgroup", "SIGCHLD is negative"))?,
        // The usable stack: above the guard page; the kernel starts the child at its top.
        stack: as_u64(stack.base.addr().checked_add(page).ok_or_else(|| {
            OciError::os("clone3 into a cgroup", "a stack past the address space")
        })?)?,
        stack_size: as_u64(stack.len.checked_sub(page).ok_or_else(|| {
            OciError::os("clone3 into a cgroup", "a stack smaller than its guard")
        })?)?,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: fd,
    };
    let entry_fn: extern "C" fn(*mut libc::c_void) -> ! = entry::<F>;
    let arg = std::ptr::from_ref(child).cast_mut().cast::<libc::c_void>();
    let Some(r) = clone3_on_stack(&args, entry_fn, arg) else {
        return Ok(None);
    };
    match r {
        n if n < 0 => match i32::try_from(n.unsigned_abs()).unwrap_or(libc::EIO) {
            libc::ENOSYS | libc::E2BIG => Ok(None),
            e => Err(OciError::os(
                "clone3 the program into the container",
                std::io::Error::from_raw_os_error(e),
            )),
        },
        child_pid => {
            // SAFETY: the clone succeeded, so the kernel made `pidfd` for the child and nothing else in
            // this process owns it.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(pidfd) };
            let pid = i32::try_from(child_pid)
                .map_err(|_| OciError::os("clone3 into a cgroup", "a pid out of range"))?;
            Ok(Some((pid, pidfd)))
        }
    }
}

/// The raw clone3 with the child started on `args.stack` at `entry(arg)`: the kernel's return in the
/// parent (the child's pid, or a negated errno), or `None` on an architecture without this trampoline.
/// The child never returns here: it calls `entry`, which never returns, on the stack the kernel gave it.
#[cfg(target_arch = "x86_64")]
fn clone3_on_stack(
    args: &CloneArgs,
    entry: extern "C" fn(*mut libc::c_void) -> !,
    arg: *mut libc::c_void,
) -> Option<i64> {
    let ret: i64;
    // SAFETY: clone3(2) reads `args` (a fully initialized CloneArgs of the size given) and writes the
    // child's pidfd where `args.pidfd` points. In the parent the syscall returns in rax, with rcx and r11
    // clobbered as declared, and touches nothing else of ours. In the child (rax 0) the kernel has set
    // rsp to the top of the stack `args` names, 16-byte aligned, and kept r12/r13: it calls entry(arg)
    // there, which never returns, so this function's own frame on the parent's stack is never touched
    // by the child. CLONE_VFORK keeps the parent's thread stopped until the child has exec'd or exited.
    unsafe {
        std::arch::asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            "mov rdi, r12",
            "call r13",
            "ud2",
            "2:",
            inlateout("rax") libc::SYS_clone3 => ret,
            in("rdi") std::ptr::from_ref(args),
            in("rsi") std::mem::size_of::<CloneArgs>(),
            in("r12") arg,
            in("r13") entry,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    Some(ret)
}

/// The same trampoline on aarch64: the syscall in x8, its arguments in x0 and x1; in the child (x0 0) the
/// kernel has set sp to the top of the stack `args` names and kept x9/x10, and it branches to entry(arg).
#[cfg(target_arch = "aarch64")]
fn clone3_on_stack(
    args: &CloneArgs,
    entry: extern "C" fn(*mut libc::c_void) -> !,
    arg: *mut libc::c_void,
) -> Option<i64> {
    let ret: i64;
    // SAFETY: as the x86_64 trampoline's: clone3(2) reads `args` and writes the child's pidfd where it
    // points; the parent gets the result in x0 with every other register kept; the child calls
    // entry(arg) on its own stack and never returns, so this frame is never touched by it.
    unsafe {
        std::arch::asm!(
            "svc 0",
            "cbnz x0, 2f",
            "mov x0, x10",
            "blr x9",
            "brk #0x1",
            "2:",
            inlateout("x0") std::ptr::from_ref(args) => ret,
            in("x1") std::mem::size_of::<CloneArgs>(),
            in("x8") libc::SYS_clone3,
            in("x9") entry,
            in("x10") arg,
            options(nostack),
        );
    }
    Some(ret)
}

/// No trampoline here: the seed's own clone path serves.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn clone3_on_stack(
    _args: &CloneArgs,
    _entry: extern "C" fn(*mut libc::c_void) -> !,
    _arg: *mut libc::c_void,
) -> Option<i64> {
    None
}

/// Writes `hex oci exec: <what>: errno <n>` to stderr with write(2) alone, the number made on the stack:
/// a vfork-style clone's child reports why it couldn't start the program without allocating.
pub fn report(failed: Failure) {
    let mut digits = [0u8; 12];
    let mut at = digits.len();
    let mut n = failed.errno.raw_os_error().unsigned_abs();
    loop {
        at = at.saturating_sub(1);
        if let Some(d) = digits.get_mut(at) {
            *d = b'0'.saturating_add(u8::try_from(n % 10).unwrap_or(0));
        }
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
    }
    let number = digits.get(at..).unwrap_or_default();
    for part in [
        b"hex oci exec: ".as_slice(),
        failed.what.as_bytes(),
        b": errno ",
        number,
        b"\n",
    ] {
        // SAFETY: write(2) of a live buffer to descriptor 2; async-signal-safe. A short or failed write
        // loses the report, which the program's exit status still carries.
        unsafe {
            libc::write(2, part.as_ptr().cast(), part.len());
        }
    }
}

/// Ends a forked child at once with status `code`, running no `atexit` handler and flushing no
/// buffer: the seed's intermediate and the program it forks must not return up into the seed's serve
/// loop, nor flush the seed's buffers a second time (`_exit(2)`, async-signal-safe). The CLI helpers
/// that re-exec'd first (`hex __oci-exec`, `hex __oci-init`) return their status up instead; this is
/// for a fork that did not re-exec.
pub fn exit_child(code: u8) -> ! {
    // SAFETY: _exit(2) takes an int and never returns; it makes no allocation and touches no shared
    // state, so it is sound in a forked child that may run only async-signal-safe calls.
    unsafe { libc::_exit(i32::from(code)) }
}

/// Signals blocked for this process and read from a `signalfd` instead, so a parent can pass them on to
/// a child it waits on, with no handler: `hex oci exec` forwards the signals a caller ends a step with.
pub struct ForwardedSignals {
    old: libc::sigset_t,
    fd: std::os::fd::OwnedFd,
}

/// Blocks `signals` and opens a `signalfd` that reads them. The caller is single-threaded (it
/// re-exec'd), so the process's mask is this thread's.
pub fn forward_signals(signals: &[i32]) -> Result<ForwardedSignals, OciError> {
    // SAFETY: `sigset_t` is plain data, and sigemptyset initializes it before anything reads it.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: sigemptyset(3) on a valid pointer to a local `sigset_t`; the return value is checked.
    if unsafe { libc::sigemptyset(&mut set) } == -1 {
        return Err(OciError::os("sigemptyset", std::io::Error::last_os_error()));
    }
    for &sig in signals {
        // SAFETY: sigaddset(3) on a valid pointer to the initialized set; the return value is checked.
        if unsafe { libc::sigaddset(&mut set, sig) } == -1 {
            return Err(OciError::os("sigaddset", std::io::Error::last_os_error()));
        }
    }
    // SAFETY: as above; sigprocmask fills it with the mask it replaces before anything reads it.
    let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: sigprocmask(2) with valid pointers to two local sets; it writes only `old`. The return
    // value is checked.
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old) } == -1 {
        return Err(OciError::os("sigprocmask", std::io::Error::last_os_error()));
    }
    // SAFETY: signalfd(2) with -1 makes a new descriptor reading the signals in `set`, a valid
    // pointer; the return value is checked.
    let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_CLOEXEC) };
    if fd == -1 {
        let e = std::io::Error::last_os_error();
        // SAFETY: sigprocmask(2) restoring the mask just saved; nothing else to undo.
        unsafe { libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
        return Err(OciError::os("signalfd", e));
    }
    // SAFETY: `fd` is the new descriptor signalfd just returned, owned by nothing else.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    Ok(ForwardedSignals { old, fd })
}

impl ForwardedSignals {
    /// The `signalfd`, for polling beside the child's pidfd.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.fd)
    }

    /// Unblocks the signals again, as they were before: what a forked child does before it execs,
    /// which would otherwise inherit them blocked.
    pub fn restore(&self) -> Result<(), OciError> {
        // SAFETY: sigprocmask(2) setting the mask saved when the signals were blocked, a valid
        // pointer; it writes nothing. The return value is checked.
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut()) } == -1 {
            return Err(OciError::os("sigprocmask", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// The next pending signal's number, read from the `signalfd` (call it once poll says it's
    /// readable).
    pub fn next(&self) -> Result<i32, OciError> {
        let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        let n = rustix::io::read(&self.fd, &mut info)
            .map_err(|e| OciError::os("reading the signalfd", e))?;
        if n != info.len() {
            return Err(OciError::os(
                "reading the signalfd",
                "a short read of signalfd_siginfo",
            ));
        }
        // `ssi_signo`, the struct's first field, a `u32` (signalfd(2)).
        let signo = info
            .first_chunk::<4>()
            .map(|b| u32::from_ne_bytes(*b))
            .ok_or_else(|| OciError::os("reading the signalfd", "no ssi_signo"))?;
        i32::try_from(signo)
            .map_err(|_| OciError::os("reading the signalfd", "a signal out of range"))
    }
}

/// Reaps child `pid`, waiting for it to end; its exit as a `waitpid` status, or the error.
pub fn waitpid(pid: i32) -> Result<WaitStatus, OciError> {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid(2) writing through a valid pointer to a local `c_int`. The return value is
    // checked; the status is read only on success.
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r == -1 {
        return Err(OciError::os("waitpid", std::io::Error::last_os_error()));
    }
    Ok(WaitStatus { raw: status })
}

/// A `waitpid` status.
pub struct WaitStatus {
    raw: libc::c_int,
}

impl WaitStatus {
    /// The exit code, when the child exited normally.
    pub fn exit_code(&self) -> Option<i32> {
        if libc::WIFEXITED(self.raw) {
            Some(libc::WEXITSTATUS(self.raw))
        } else {
            None
        }
    }

    /// The signal that ended the child, when one did.
    pub fn term_signal(&self) -> Option<i32> {
        if libc::WIFSIGNALED(self.raw) {
            Some(libc::WTERMSIG(self.raw))
        } else {
            None
        }
    }
}

/// Sets signal `sig` to its default disposition, unblocks `sig` alone, and raises it on this process,
/// so the process ends the way the signal says: `hex __oci-exec` in seam mode ends by the signal that
/// ended its program, so the node agent's pidfd watch reads that outcome. Only `sig` is unblocked, so
/// another forwarded signal left pending can't be delivered first and misreport. SIGKILL is raised as
/// it is: its disposition can't be changed nor the signal blocked, and signal(2) refuses to try
/// (EINVAL; signal(7)), which would leave the program's SIGKILL reported as an exit of 137. Returns
/// only if the raise didn't end it.
pub fn raise_default(sig: i32) -> Result<(), OciError> {
    if sig != libc::SIGKILL {
        default_and_unblock(sig)?;
    }
    // SAFETY: raise(3) with a valid signal number; no pointers, writes nothing. Checked.
    if unsafe { libc::raise(sig) } != 0 {
        return Err(OciError::os("raise", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Sets `sig` to its default disposition and unblocks it alone.
fn default_and_unblock(sig: i32) -> Result<(), OciError> {
    // SAFETY: signal(2) with SIG_DFL and a valid signal number installs the default disposition and
    // writes nothing of ours; the return value is checked against SIG_ERR.
    if unsafe { libc::signal(sig, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(OciError::os(
            "signal SIG_DFL",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: `sigset_t` is plain data; sigemptyset then sigaddset initialize it before it is read.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: sigemptyset(3) on a valid pointer to the local set; return checked.
    if unsafe { libc::sigemptyset(&mut set) } == -1 {
        return Err(OciError::os("sigemptyset", std::io::Error::last_os_error()));
    }
    // SAFETY: sigaddset(3) adding `sig` to the initialized local set; return checked.
    if unsafe { libc::sigaddset(&mut set, sig) } == -1 {
        return Err(OciError::os("sigaddset", std::io::Error::last_os_error()));
    }
    // SAFETY: sigprocmask(2) unblocking just `sig` from a valid local set; the old-mask pointer is
    // null, so it writes nothing of ours. Return checked.
    if unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) } == -1 {
        return Err(OciError::os(
            "sigprocmask SIG_UNBLOCK",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Sends signal `sig` to the container: to the process group when `all`, else to the init `pid`.
pub fn kill(pid: i32, sig: i32, all: bool) -> Result<(), OciError> {
    let target = if all {
        pid.checked_neg()
            .ok_or_else(|| OciError::os("kill", "pid has no negation"))?
    } else {
        pid
    };
    // SAFETY: kill(2) with a pid (or negated pid for the group) and a signal number; no pointers,
    // writes nothing. The return value is checked.
    let r = unsafe { libc::kill(target, sig) };
    if r == -1 {
        Err(OciError::os("kill", std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

/// Sets whether the process keeps its permitted capabilities across a uid change
/// (`PR_SET_KEEPCAPS`), so a container can drop to a non-root user and still hold the capabilities
/// its spec grants.
pub fn set_keepcaps(keep: bool) -> Result<(), rustix::io::Errno> {
    prctl2(libc::PR_SET_KEEPCAPS, libc::c_ulong::from(u8::from(keep)))
}

/// Joins the namespace the open `fd` refers to, checking it is of type `nstype` (a `CLONE_NEW*`
/// flag); `setns` fails if the fd's namespace is of another type. Refused with its errno.
pub fn setns(fd: std::os::fd::BorrowedFd<'_>, nstype: i32) -> Result<(), Failure> {
    use std::os::fd::AsRawFd;
    // SAFETY: setns(2) with a file descriptor and a type flag; it takes no pointers and writes
    // nothing. The return value is checked.
    let r = unsafe { libc::setns(fd.as_raw_fd(), nstype) };
    if r == -1 {
        Err(Failure {
            what: "setns",
            errno: last_errno(),
        })
    } else {
        Ok(())
    }
}

/// `unshare` of namespace flags. Hex never passes `CLONE_FILES`, the one flag that makes
/// `unshare` unsound (it can detach file descriptors another thread is using); its callers, the init
/// (single-threaded after its re-exec) and the node agent's thread that makes network namespaces ahead
/// ([`super::net::make_ahead`]), use only the `NEW*` namespace flags.
pub fn unshare(flags: rustix::thread::UnshareFlags) -> Result<(), Failure> {
    // CLONE_FILES is the one flag that makes `unshare` unsound; clear it defensively (callers only
    // ever pass namespace flags) so the invariant below holds whatever the caller passed.
    let flags = flags.difference(rustix::thread::UnshareFlags::FILES);
    if flags.is_empty() {
        return Ok(());
    }
    // SAFETY: the flags are namespace flags only (CLONE_FILES cleared above), so no file-descriptor
    // table is detached; the invariant `unshare_unsafe` documents holds.
    unsafe { rustix::thread::unshare_unsafe(flags) }.map_err(Failure::of("unshare"))
}

/// Drops capability number `cap` from the process's bounding set (`PR_CAPBSET_DROP`).
pub fn capbset_drop(cap: u32) -> Result<(), rustix::io::Errno> {
    prctl2(libc::PR_CAPBSET_DROP, libc::c_ulong::from(cap))
}

/// Clears the process's ambient capability set (`PR_CAP_AMBIENT_CLEAR_ALL`).
pub fn ambient_clear() -> Result<(), rustix::io::Errno> {
    // SAFETY: prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) takes no pointers; checked.
    let r = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

/// Raises capability number `cap` in the process's ambient set (`PR_CAP_AMBIENT_RAISE`).
pub fn ambient_raise(cap: u32) -> Result<(), rustix::io::Errno> {
    // SAFETY: prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap, 0, 0) takes no pointers; checked.
    let r = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_RAISE,
            libc::c_ulong::from(cap),
            0,
            0,
        )
    };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

fn prctl2(option: libc::c_int, arg2: libc::c_ulong) -> Result<(), rustix::io::Errno> {
    // SAFETY: prctl with a scalar option and one scalar argument (here a capability number); it
    // takes no pointers and writes nothing. The return value is checked.
    let r = unsafe { libc::prctl(option, arg2, 0, 0, 0) };
    if r == -1 { Err(last_errno()) } else { Ok(()) }
}

/// This thread's errno, as plain data: no allocation, so the child of a vfork-style clone may read it.
fn last_errno() -> rustix::io::Errno {
    rustix::io::Errno::from_raw_os_error(
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

// `bpf(2)`'s commands and the device program's type and attach point (include/uapi/linux/bpf.h).
const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_PROG_ATTACH: libc::c_long = 8;
const BPF_PROG_GET_FD_BY_ID: libc::c_long = 13;
const BPF_OBJ_GET_INFO_BY_FD: libc::c_long = 15;
const BPF_PROG_QUERY: libc::c_long = 16;
pub const BPF_PROG_TYPE_CGROUP_DEVICE: u32 = 15;
const BPF_CGROUP_DEVICE: u32 = 6;
/// `BPF_F_ALLOW_OVERRIDE` (1 << 0): "the program in this cgroup yields to sub-cgroup program".
pub const BPF_F_ALLOW_OVERRIDE: u32 = 1;
/// `BPF_F_ALLOW_MULTI` (1 << 1): the program joins whatever the cgroup's ancestors attached, all of
/// which must allow an access, as runc and crun attach theirs.
const BPF_F_ALLOW_MULTI: u32 = 2;
/// Most programs the kernel attaches to one cgroup for one attach type, `BPF_CGROUP_MAX_PROGS` (64,
/// kernel/bpf/cgroup.c:794), past which it refuses an attach (`E2BIG`, :1009): every id a query reads.
const BPF_CGROUP_MAX_PROGS: u32 = 64;
/// Most bytes of the verifier's log kept when it refuses a program: enough for the refusing
/// instruction and its context; the kernel truncates the log at the buffer's end.
const VERIFIER_LOG_BYTES: usize = 65_536;

/// The `union bpf_attr` member `BPF_PROG_LOAD` reads, as the kernel lays it out; fields past
/// `expected_attach_type` are absent, which the kernel takes as zero.
#[repr(C)]
struct ProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
}

/// The `union bpf_attr` member `BPF_PROG_ATTACH` reads.
#[repr(C)]
struct ProgAttachAttr {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
}

/// An address the kernel reads through, as `bpf_attr`'s `__aligned_u64`.
fn user_ptr<T: ?Sized>(p: *const T) -> Result<u64, OciError> {
    u64::try_from(p.cast::<u8>().expose_provenance())
        .map_err(|_| OciError::os("bpf", "an address wider than 64 bits"))
}

/// Loads a `BPF_PROG_TYPE_CGROUP_DEVICE` program ([`super::resources::compile`]'s output). When the
/// verifier refuses it, the error carries the tail of the verifier's log.
pub fn load_device_program(
    insns: &[super::resources::Insn],
) -> Result<std::os::fd::OwnedFd, OciError> {
    let insn_cnt = u32::try_from(insns.len())
        .map_err(|_| OciError::os("loading the device program", "too many instructions"))?;
    let license = c"MIT";
    // The verifier's log, only asked for when a first load is refused; it lives to the function's end,
    // past every call that may write it.
    let mut log: Vec<u8> = Vec::new();
    let mut attr = ProgLoadAttr {
        prog_type: BPF_PROG_TYPE_CGROUP_DEVICE,
        insn_cnt,
        insns: user_ptr(insns.as_ptr())?,
        license: user_ptr(license.as_ptr())?,
        log_level: 0,
        log_size: 0,
        log_buf: 0,
        kern_version: 0,
        prog_flags: 0,
        prog_name: *b"hex_devices\0\0\0\0\0",
        prog_ifindex: 0,
        expected_attach_type: 0,
    };
    for verbose in [false, true] {
        if verbose {
            log = vec![0u8; VERIFIER_LOG_BYTES];
            attr.log_level = 1;
            attr.log_size = u32::try_from(log.len()).unwrap_or(u32::MAX);
            attr.log_buf = user_ptr(log.as_mut_ptr())?;
        }
        // SAFETY: bpf(BPF_PROG_LOAD) reads `attr` (a live, initialised repr(C) struct of the size
        // passed) and, through it, `insns` (insn_cnt live instructions laid out as struct bpf_insn), the
        // NUL-terminated license, and writes at most log_size bytes into `log`, which outlives the call.
        // The return value is checked before it is owned.
        let r = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                BPF_PROG_LOAD,
                &raw const attr,
                std::mem::size_of::<ProgLoadAttr>(),
            )
        };
        if r >= 0 {
            let fd =
                i32::try_from(r).map_err(|_| OciError::os("bpf", "a descriptor out of range"))?;
            use std::os::fd::FromRawFd;
            // SAFETY: a successful BPF_PROG_LOAD returns a new descriptor no one else holds.
            return Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        }
        let err = std::io::Error::last_os_error();
        if verbose || err.raw_os_error() != Some(libc::EINVAL) {
            let text = String::from_utf8_lossy(&log);
            let tail: String = text
                .trim_end_matches('\0')
                .chars()
                .rev()
                .take(500)
                .collect();
            let tail: String = tail.chars().rev().collect();
            return Err(OciError::os(
                "loading the device program",
                if tail.is_empty() {
                    err.to_string()
                } else {
                    format!("{err}: verifier: {tail}")
                },
            ));
        }
    }
    Err(OciError::os(
        "loading the device program",
        "the verifier refused it",
    ))
}

/// How a device program is attached to a cgroup, which decides how it composes with the programs above
/// and below it (include/uapi/linux/bpf.h, "BPF_F_ALLOW_OVERRIDE" and "BPF_F_ALLOW_MULTI"; Linux v6.19
/// kernel/bpf/cgroup.c, `compute_effective_progs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// With `BPF_F_ALLOW_MULTI`, as a container's own program is: it runs with every MULTI program above.
    Multi,
    /// With `BPF_F_ALLOW_OVERRIDE`, as the oci boxes' root carries the default policy's: it decides for
    /// each cgroup below that has no program of its own, and a cgroup that has one is decided by its own.
    Override,
}

/// Attaches device program `prog` to the cgroup whose directory is open as `cgroup`, as `how` says. On
/// a cgroup that already holds an OVERRIDE program, an OVERRIDE attach replaces it in one call
/// (include/uapi/linux/bpf.h: "Attaching another program on top of NONE or BPF_F_ALLOW_OVERRIDE will
/// release old program and attach the new one").
pub fn attach_device_program(
    cgroup: std::os::fd::BorrowedFd<'_>,
    prog: std::os::fd::BorrowedFd<'_>,
    how: Attach,
) -> Result<(), OciError> {
    use std::os::fd::AsRawFd;
    let fd = |f: std::os::fd::BorrowedFd<'_>| {
        u32::try_from(f.as_raw_fd()).map_err(|_| OciError::os("bpf", "a negative descriptor"))
    };
    let attr = ProgAttachAttr {
        target_fd: fd(cgroup)?,
        attach_bpf_fd: fd(prog)?,
        attach_type: BPF_CGROUP_DEVICE,
        attach_flags: match how {
            Attach::Multi => BPF_F_ALLOW_MULTI,
            Attach::Override => BPF_F_ALLOW_OVERRIDE,
        },
        replace_bpf_fd: 0,
    };
    // SAFETY: bpf(BPF_PROG_ATTACH) reads `attr`, a live, initialised repr(C) struct of the size
    // passed, holding two descriptors this process has open; it writes nothing of ours.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_ATTACH,
            &raw const attr,
            std::mem::size_of::<ProgAttachAttr>(),
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(OciError::os(
            "attaching the device program",
            std::io::Error::last_os_error(),
        ))
    }
}

/// The `union bpf_attr` member `BPF_PROG_QUERY` reads and writes, whole to `revision`: Linux v6.19
/// writes `revision` back whatever the size passed (kernel/bpf/cgroup.c:1262), so a shorter struct would
/// have it write past this one.
#[repr(C)]
#[derive(Default)]
struct ProgQueryAttr {
    target_fd: u32,
    attach_type: u32,
    query_flags: u32,
    attach_flags: u32,
    prog_ids: u64,
    prog_cnt: u32,
    _reserved: u32,
    prog_attach_flags: u64,
    link_ids: u64,
    link_attach_flags: u64,
    revision: u64,
}

/// The device programs attached to one cgroup itself, its ancestors' left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attached {
    /// The flags they were attached with (0 when none is).
    pub flags: u32,
    /// How many there are.
    pub count: u32,
    /// Their ids, every one of them up to the kernel's cap, [`BPF_CGROUP_MAX_PROGS`].
    pub ids: Vec<u32>,
}

/// How many ids a query of a cgroup holding `count` device programs reads: all of them, at most the kernel's cap.
fn ids_to_read(count: u32) -> usize {
    usize::try_from(count.min(BPF_CGROUP_MAX_PROGS)).unwrap_or(0)
}

/// The device programs attached to the cgroup whose directory is open as `cgroup` (`BPF_PROG_QUERY`,
/// which needs CAP_NET_ADMIN or CAP_SYS_ADMIN and takes the kernel's `cgroup_mutex`: a node asks it at
/// its start, never per create). Asked twice: for the count first, then for that many ids, so every one is
/// read; one attached between the two is counted and its id left out (`ENOSPC`).
pub fn attached_device_programs(cgroup: std::os::fd::BorrowedFd<'_>) -> Result<Attached, OciError> {
    let (_, count) = query_device_programs(cgroup, &mut [])?;
    let mut ids = vec![0u32; ids_to_read(count)];
    let (flags, count) = query_device_programs(cgroup, &mut ids)?;
    ids.truncate(ids_to_read(count));
    Ok(Attached { flags, count, ids })
}

/// One `BPF_PROG_QUERY` of the device programs attached to `cgroup`: their flags and count, and the first ids,
/// as many as `ids` holds, written into it. With `ids` empty the kernel writes the count alone
/// (kernel/bpf/cgroup.c:1494-1496).
fn query_device_programs(
    cgroup: std::os::fd::BorrowedFd<'_>,
    ids: &mut [u32],
) -> Result<(u32, u32), OciError> {
    use std::os::fd::AsRawFd;
    let mut attr = ProgQueryAttr {
        target_fd: u32::try_from(cgroup.as_raw_fd())
            .map_err(|_| OciError::os("bpf", "a negative descriptor"))?,
        attach_type: BPF_CGROUP_DEVICE,
        prog_ids: if ids.is_empty() {
            0
        } else {
            user_ptr(ids.as_mut_ptr().cast_const())?
        },
        prog_cnt: u32::try_from(ids.len())
            .map_err(|_| OciError::os("bpf", "more ids asked than a u32 counts"))?,
        ..ProgQueryAttr::default()
    };
    // SAFETY: bpf(BPF_PROG_QUERY) reads `attr`, a live, initialised repr(C) struct of the size passed,
    // and writes into it only its own fields (attach_flags, prog_cnt, revision), all within that size.
    // Through `prog_ids` it writes at most `prog_cnt` u32s into `ids`, a live slice of exactly that many
    // that outlives the call; with `ids` empty, `prog_ids` is null and `prog_cnt` 0, and it writes nothing
    // through it (kernel/bpf/cgroup.c:1494-1496). `prog_attach_flags` and the link fields are zero, so
    // nothing is written through them. Checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_QUERY,
            &raw mut attr,
            std::mem::size_of::<ProgQueryAttr>(),
        )
    };
    let err = std::io::Error::last_os_error();
    // ENOSPC: more are attached than `ids` holds; the count and the first ids are written all the same.
    if r != 0 && err.raw_os_error() != Some(libc::ENOSPC) {
        return Err(OciError::os("querying a cgroup's device programs", err));
    }
    Ok((attr.attach_flags, attr.prog_cnt))
}

/// Loads seccomp filter `program` on the calling thread and leaves no_new_privs as it is: the kernel
/// takes a filter from a thread without no_new_privs when it has CAP_SYS_ADMIN in its user namespace
/// (seccomp(2), `SECCOMP_SET_MODE_FILTER`). [`seccompiler::apply_filter`] sets no_new_privs first,
/// which a spec without `noNewPrivileges` doesn't ask for.
pub fn load_seccomp_filter(filter: &[libc::sock_filter]) -> Result<(), rustix::io::Errno> {
    let len = u16::try_from(filter.len()).map_err(|_| rustix::io::Errno::INVAL)?;
    let prog = libc::sock_fprog {
        len,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: seccomp(SECCOMP_SET_MODE_FILTER) reads `prog`, a live sock_fprog whose `filter` points at
    // `len` live, initialised sock_filter instructions that outlive the call; the kernel copies them
    // and keeps no pointer into them, and it writes nothing of ours. Checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &raw const prog,
        )
    };
    if r == 0 { Ok(()) } else { Err(last_errno()) }
}

/// The `union bpf_attr` member `BPF_PROG_GET_FD_BY_ID` reads.
#[repr(C)]
struct GetFdByIdAttr {
    prog_id: u32,
    next_id: u32,
    open_flags: u32,
}

/// The `union bpf_attr` member `BPF_OBJ_GET_INFO_BY_FD` reads.
#[repr(C)]
struct InfoByFdAttr {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

/// What the kernel says a loaded program is: the head of `struct bpf_prog_info`
/// (include/uapi/linux/bpf.h) as far as `xlated_prog_len`. The tag is the first 8 bytes of the digest
/// the kernel takes of the instructions it was loaded from (SHA-256 in Linux 7.2: kernel/bpf/core.c,
/// `bpf_prog_calc_tag`): a quick check, not a proof, that two programs are one;
/// [`program_instructions`] is the proof. The two lengths are zero on the way in, which asks the
/// kernel for no instruction dump.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProgramIdentity {
    pub prog_type: u32,
    pub id: u32,
    pub tag: [u8; 8],
    pub jited_len: u32,
    pub xlated_len: u32,
}

/// The head of `struct bpf_prog_info` through `xlated_prog_insns`: with `xlated_prog_len` and
/// `xlated_prog_insns` set, the kernel dumps the program's translated instructions there.
#[repr(C)]
#[derive(Default)]
struct InfoWithDump {
    head: ProgramIdentity,
    jited_prog_insns: u64,
    xlated_prog_insns: u64,
}

/// Most bytes of translated instructions [`program_instructions`] reads: `BPF_MAXINSNS` (4,096,
/// include/uapi/linux/bpf_common.h) instructions of 8 bytes, which Hex's device programs stay under
/// (`resources::MAX_DEVICE_EXCEPTIONS`).
const MAX_DUMP_BYTES: u32 = 4096 * 8;

/// The identity of the loaded program open as `prog`.
pub fn program_identity(prog: std::os::fd::BorrowedFd<'_>) -> Result<ProgramIdentity, OciError> {
    use std::os::fd::AsRawFd;
    let mut info = ProgramIdentity::default();
    let attr = InfoByFdAttr {
        bpf_fd: u32::try_from(prog.as_raw_fd())
            .map_err(|_| OciError::os("bpf", "a negative descriptor"))?,
        info_len: u32::try_from(std::mem::size_of::<ProgramIdentity>())
            .map_err(|_| OciError::os("bpf", "an info size out of range"))?,
        info: user_ptr((&raw mut info).cast_const())?,
    };
    // SAFETY: bpf(BPF_OBJ_GET_INFO_BY_FD) reads `attr`, a live, initialised repr(C) struct of the size
    // passed, and writes at most `info_len` bytes into `info`, a live repr(C) struct of exactly that
    // size that outlives the call; its zero lengths ask for no dump through any pointer. Checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_OBJ_GET_INFO_BY_FD,
            &raw const attr,
            std::mem::size_of::<InfoByFdAttr>(),
        )
    };
    if r == 0 {
        Ok(info)
    } else {
        Err(OciError::os(
            "reading a device program's identity",
            std::io::Error::last_os_error(),
        ))
    }
}

/// The translated instructions of the loaded program open as `prog`, as the kernel dumps them
/// (kernel/bpf/syscall.c, `bpf_prog_get_info_by_fd`), when there are exactly `len` bytes of them.
/// `None` when there aren't, when `len` passes [`MAX_DUMP_BYTES`], or when the kernel dumps none: it
/// withholds them from a caller without CAP_BPF, and from one without raw-dump rights when it blinded
/// the program's constants (clearing the pointer to say so).
pub fn program_instructions(
    prog: std::os::fd::BorrowedFd<'_>,
    len: u32,
) -> Result<Option<Vec<u8>>, OciError> {
    use std::os::fd::AsRawFd;
    if len == 0 || len > MAX_DUMP_BYTES {
        return Ok(None);
    }
    let mut buf = vec![
        0u8;
        usize::try_from(len)
            .map_err(|_| OciError::os("bpf", "a dump length out of range"))?
    ];
    let mut info = InfoWithDump::default();
    info.head.xlated_len = len;
    info.xlated_prog_insns = user_ptr(buf.as_mut_ptr())?;
    let attr = InfoByFdAttr {
        bpf_fd: u32::try_from(prog.as_raw_fd())
            .map_err(|_| OciError::os("bpf", "a negative descriptor"))?,
        info_len: u32::try_from(std::mem::size_of::<InfoWithDump>())
            .map_err(|_| OciError::os("bpf", "an info size out of range"))?,
        info: user_ptr((&raw mut info).cast_const())?,
    };
    // SAFETY: bpf(BPF_OBJ_GET_INFO_BY_FD) reads `attr`, a live, initialised repr(C) struct of the size
    // passed. It writes at most `info_len` bytes into `info`, a live repr(C) struct of exactly that
    // size, and at most `xlated_len` bytes through `xlated_prog_insns` into `buf`, a live buffer of
    // that length; both outlive the call. `jited_len` is zero, so nothing is written through
    // `jited_prog_insns`. Checked.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_OBJ_GET_INFO_BY_FD,
            &raw const attr,
            std::mem::size_of::<InfoByFdAttr>(),
        )
    };
    if r != 0 {
        return Err(OciError::os(
            "reading a device program's instructions",
            std::io::Error::last_os_error(),
        ));
    }
    // The kernel reports the whole translation's length, and a cleared pointer when it withheld it.
    if info.head.xlated_len != len || info.xlated_prog_insns == 0 {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// A descriptor for loaded program `id`, or `None` when no program has that id now. Opening a
/// program by id needs CAP_SYS_ADMIN, which the runtime that loads device programs has.
pub fn program_by_id(id: u32) -> Result<Option<std::os::fd::OwnedFd>, OciError> {
    let attr = GetFdByIdAttr {
        prog_id: id,
        next_id: 0,
        open_flags: 0,
    };
    // SAFETY: bpf(BPF_PROG_GET_FD_BY_ID) reads `attr`, a live, initialised repr(C) struct of the size
    // passed; it writes nothing of ours. The return value is checked before it is owned.
    let r = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_GET_FD_BY_ID,
            &raw const attr,
            std::mem::size_of::<GetFdByIdAttr>(),
        )
    };
    if r >= 0 {
        let fd = i32::try_from(r).map_err(|_| OciError::os("bpf", "a descriptor out of range"))?;
        use std::os::fd::FromRawFd;
        // SAFETY: a successful BPF_PROG_GET_FD_BY_ID returns a new descriptor no one else holds.
        return Ok(Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }));
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(OciError::os("opening a device program by id", err))
    }
}

/// Owns a descriptor the spawn of this process put at `fd` for it: a seed's stdin, the agent's socket.
pub fn own_fd(fd: std::os::fd::RawFd) -> Result<std::os::fd::OwnedFd, OciError> {
    use std::os::fd::FromRawFd;
    if fd < 0 {
        return Err(OciError::os("fd", "a negative file descriptor number"));
    }
    // SAFETY: the agent's spawn duplicated the seed's socket onto this number in this process alone, and
    // nothing else in the seed uses it, so this process is its sole owner.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// A mount attribute to set on a mount named by a descriptor (`include/uapi/linux/mount.h`), for
/// [`mount_setattr`]. `RDONLY` makes the mount read-only; the rest match a spec mount's `nosuid`,
/// `nodev` and `noexec` options. These are the values `mount_attr`'s `attr_set` and `attr_clr` take.
pub const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
pub const MOUNT_ATTR_NOSUID: u64 = 0x0000_0002;
pub const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;
pub const MOUNT_ATTR_NOEXEC: u64 = 0x0000_0008;

/// A mount's propagation, the one value `mount_attr`'s `propagation` field takes
/// (`include/uapi/linux/fs.h`): a mount is unbindable, private, a slave (downstream) or shared, and
/// exactly one at a time. `0` leaves it unchanged.
pub const MS_UNBINDABLE: u64 = 1 << 17;
pub const MS_PRIVATE: u64 = 1 << 18;
pub const MS_SLAVE: u64 = 1 << 19;
pub const MS_SHARED: u64 = 1 << 20;

/// What [`mount_setattr`] sets on a mount: the attributes to set and to clear, and the propagation to
/// apply (`0` for none). The init on the pruned-seed path has no `/proc/self/fd` to name a mount by
/// path (its namespace holds no proc), so it names each mount by a descriptor and changes it here,
/// where the classic path remounts by path.
pub struct MountAttrs {
    pub set: u64,
    pub clr: u64,
    pub propagation: u64,
}

/// `mount_setattr(target, "", AT_EMPTY_PATH[|AT_RECURSIVE], &attr, sizeof attr)`: changes the mount
/// the descriptor `target` names (a mount fd from `fsmount`/`open_tree`, or an `O_PATH` descriptor of
/// a mount point), recursively over its submounts when `recursive`. rustix doesn't wrap this call
/// (`not_implemented!(mount_setattr)`), and the pruned-seed init needs it to make a mount read-only or
/// set its propagation without a path (CLAUDE.md §12 names this file and this call). A read-only bind
/// made recursive covers the submounts a recursive bind carried, so a recursive read-only bind of a
/// host tree is read-only to the leaves (unlike a `MS_REMOUNT` of the top alone).
pub fn mount_setattr(
    target: std::os::fd::BorrowedFd<'_>,
    recursive: bool,
    attrs: &MountAttrs,
) -> Result<(), OciError> {
    use std::os::fd::AsRawFd;
    /// `struct mount_attr` (`include/uapi/linux/mount.h`, `MOUNT_ATTR_SIZE_VER0`, 32 bytes): four
    /// `__u64`s, the attributes to set and clear, the propagation, and a user-namespace descriptor
    /// (unused here).
    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }
    let attr = MountAttr {
        attr_set: attrs.set,
        attr_clr: attrs.clr,
        propagation: attrs.propagation,
        userns_fd: 0,
    };
    // AT_EMPTY_PATH: operate on the descriptor itself, so the empty path names the mount `target` is.
    let mut flags: libc::c_int = libc::AT_EMPTY_PATH;
    if recursive {
        flags |= libc::AT_RECURSIVE;
    }
    // SAFETY: mount_setattr(2) with a descriptor the caller owns, an empty path (AT_EMPTY_PATH, so no
    // path is read), the exact size of the fully initialized `MountAttr` the kernel only reads, and
    // flags the kernel validates. The return value is checked; nothing of ours is written.
    let r = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            target.as_raw_fd(),
            c"".as_ptr(),
            flags,
            &raw const attr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(OciError::os(
            "mount_setattr",
            std::io::Error::last_os_error(),
        ))
    }
}

// statmount(2) and listmount(2), Linux 6.8. On x86_64 and aarch64 they are the common table's 457 and 458 (v6.19
// arch/x86/entry/syscalls/syscall_64.tbl:384-385; scripts/syscall.tbl:400-401, which arm64's table is made from);
// the libc crate at 0.2.191 numbers them for m68k alone.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_STATMOUNT: libc::c_long = 457;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const SYS_LISTMOUNT: libc::c_long = 458;

/// `struct mnt_id_req` as Linux 6.8 published it (`MNT_ID_REQ_SIZE_VER0`, 24 bytes, v6.19
/// include/uapi/linux/mount.h:198-207), which every later kernel still takes: a mount by its unique id
/// (`statx`'s `STATX_MNT_ID_UNIQUE`), and for `statmount` the fields asked, for `listmount` the last id
/// listed. No namespace descriptor: the caller's own namespace.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[repr(C)]
struct MntIdReq {
    size: u32,
    mnt_ns_fd: u32,
    mnt_id: u64,
    param: u64,
}

/// The size `MntIdReq` is, for its `size` field.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const MNT_ID_REQ_SIZE: u32 = 24;

/// `listmount`'s and `statmount`'s `flags`: none (`LISTMOUNT_REVERSE` lists the other way).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const NO_FLAGS: libc::c_uint = 0;

/// The unique ids of the mounts beneath mount `parent` in this process's mount namespace, its own
/// excluded, after the id `last` (0 for the first), written into `ids` (listmount(2): every mount
/// reachable from `parent`, not only its children, in id order). How many it wrote: fewer than
/// `ids.len()` when none is left. `ENOSYS` from a kernel before 6.8.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn listmount(parent: u64, last: u64, ids: &mut [u64]) -> Result<usize, rustix::io::Errno> {
    let req = MntIdReq {
        size: MNT_ID_REQ_SIZE,
        mnt_ns_fd: 0,
        mnt_id: parent,
        param: last,
    };
    // SAFETY: listmount(2) reads `req`, a live, fully initialized repr(C) struct of the size its `size`
    // field gives, and writes at most `ids.len()` u64s into `ids`, a live buffer of exactly that many
    // that outlives the call; no flags. The return value is checked.
    let r = unsafe {
        libc::syscall(
            SYS_LISTMOUNT,
            &raw const req,
            ids.as_mut_ptr(),
            ids.len(),
            NO_FLAGS,
        )
    };
    if r < 0 {
        Err(last_errno())
    } else {
        usize::try_from(r).map_err(|_| rustix::io::Errno::RANGE)
    }
}

/// Mount `id`'s fields that `mask` asks for (statmount(2)), written into `buf`: the kernel's fixed
/// `struct statmount`, then the strings asked, each at the offset its field gives past the fixed part.
/// `ENOSYS` from a kernel before 6.8; `EOVERFLOW` when the strings don't fit.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn statmount(id: u64, mask: u64, buf: &mut [u8]) -> Result<(), rustix::io::Errno> {
    let req = MntIdReq {
        size: MNT_ID_REQ_SIZE,
        mnt_ns_fd: 0,
        mnt_id: id,
        param: mask,
    };
    // SAFETY: statmount(2) reads `req`, a live, fully initialized repr(C) struct of the size its `size`
    // field gives, and writes at most `buf.len()` bytes into `buf`, a live buffer of exactly that many
    // that outlives the call (the kernel copies bytes, so it needs no alignment); no flags. The return
    // value is checked.
    let r = unsafe {
        libc::syscall(
            SYS_STATMOUNT,
            &raw const req,
            buf.as_mut_ptr(),
            buf.len(),
            NO_FLAGS,
        )
    };
    if r < 0 { Err(last_errno()) } else { Ok(()) }
}

/// Where the kernel numbers no `listmount`, it is missing, as on a kernel before 6.8.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn listmount(_parent: u64, _last: u64, _ids: &mut [u64]) -> Result<usize, rustix::io::Errno> {
    Err(rustix::io::Errno::NOSYS)
}

/// Where the kernel numbers no `statmount`, it is missing, as on a kernel before 6.8.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn statmount(_id: u64, _mask: u64, _buf: &mut [u8]) -> Result<(), rustix::io::Errno> {
    Err(rustix::io::Errno::NOSYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ada's 2026-11-06 rule (AC-T7.1.4, decision 2's device programs): do size a query for cgroups the kernel says
    /// hold 1, 9 (past the 8 once picked) and 65 programs; expect room for every one of them, up to the kernel's own
    /// cap on one cgroup's programs, 64, and no more.
    #[test]
    fn a_query_reads_every_program_the_kernel_can_attach() {
        assert_eq!(ids_to_read(0), 0);
        assert_eq!(ids_to_read(1), 1);
        assert_eq!(ids_to_read(9), 9);
        assert_eq!(ids_to_read(64), 64);
        assert_eq!(ids_to_read(65), 64);
    }
}
