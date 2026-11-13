//! `hex __oci-seed`: a prepared, single-threaded process the node agent keeps and reuses, so that an
//! exec into a container spawns no `hex` process of its own and re-execs no `hex` binary per call
//! (decision 3 of docs/design/runtime-architecture.md, goal 3 plan step 2). The agent hands each exec
//! to a seed over a `SOCK_SEQPACKET` socket: the request's bytes ([`hex_core::seed`]) with the
//! program's stdin, stdout and stderr as descriptors ([`send_request`]). The seed answers with the
//! program's pidfd, and the program is the agent's own child, which the agent's supervisor watches.
//!
//! **One clone, where the seed may choose its children's pid namespace.** A privileged seed sets its
//! children's pid namespace to the container's (`setns(CLONE_NEWPID)`, which governs only the caller's
//! future children) and clones the program with `clone3(CLONE_PARENT | CLONE_INTO_CGROUP |
//! CLONE_PIDFD)`: born in the container's cgroup and pid namespace, and the agent's child from its
//! first instruction, so the seed answers at once. `exit_signal` is 0, the one value `clone3` takes
//! with `CLONE_PARENT` ([`sys::clone_sibling`]). The program then joins the container's other
//! namespaces itself (user first, mount next to last, cgroup last, as `hex oci exec`'s child does),
//! becomes the container's process and execs.
//!
//! **An intermediate, where it may not.** Choosing a pid namespace needs `CAP_SYS_ADMIN` in the
//! caller's own user namespace and in the one that owns the pid namespace (`pidns_install` in
//! kernel/pid_namespace.c), so a rootless seed is refused `EPERM` for a container with a pid namespace
//! of its own. Then the seed forks a short-lived intermediate that joins the container's user
//! namespace first (which grants it those capabilities), then the rest, and clones the program; the
//! intermediate answers the seed over an internal socketpair and exits, and the seed reaps it (so the
//! program, orphaned to the agent, a child subreaper, is the agent's own child) before it forwards the
//! answer, so the agent's `waitid(P_PIDFD)` never meets a program not yet its child. The intermediate is
//! a fork of the seed, no exec, gone before the program runs. So both paths keep "no process but the
//! program" and "no `hex` exec per call".
//!
//! The namespace joins, the clone and the `become_process` are the same ones `hex __oci-exec` makes
//! ([`super::exec`], [`super::init`]); only the plumbing — a socket in place of an inherited request
//! and a waited status — differs.

use std::collections::VecDeque;
use std::ffi::{CString, c_char};
use std::io::{IoSlice, IoSliceMut, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use hex_core::seed::{self, Answer, Failed, MountNamespace, Request};
use rustix::fs::Mode;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};
use rustix::pipe::PipeFlags;
use rustix::thread::ThreadNameSpaceType as Ns;

use crate::linux::oci::ipcuts::Pair;
use crate::linux::oci::spec::{Namespace, Process, Spec};
use crate::linux::oci::{Failure, OciError, exec, init, sys};

/// The environment entry by which the node agent tells a create seed to join the node's one pruned mount
/// namespace at start (goal 5 plan step 2): the agent sends that namespace as the first record on the
/// seed's socket ([`send_namespace`]) and the seed joins it by `setns`, so its namespace holds only the
/// pruned read-only tmpfs, holding nothing. The node prunes once for all its seeds ([`prune_namespace`]),
/// since each `pivot_root` walks every thread on the host under the mount lock (`goal6-mntns.md` §11,
/// §12). Set only for a root node's create seeds; absent for an exec seed and a rootless node, which keep
/// the agent's mount namespace and take the classic root path.
pub const ENV_JOIN: &str = "HEX_OCI_SEED_JOIN";

/// A debug-only crash point the node agent arms on a create seed (its `with_inject`, never a container's
/// config, which reaches the init only at `execve`, after the root is built): its value names a point in
/// the pruned init's root build at which the init ends abruptly, as a kill would, so a test proves the
/// agent cleans up a container whose init dies mid-build (CLAUDE.md §6, crash injection at every write).
/// The seed's cloned init inherits this entry and reads it ([`init`]). Absent from release builds, where
/// the init never looks for it and the agent never sets it.
#[cfg(debug_assertions)]
pub const ENV_INJECT: &str = "HEX_OCI_SEED_INJECT";

/// The one point [`ENV_INJECT`] names: the pruned init ends between attaching its mounts and the
/// `chroot` that makes them the container's root, so the mounts exist, in the init's own namespace,
/// when it dies.
#[cfg(debug_assertions)]
pub const INJECT_DIE_BEFORE_SWITCH: &str = "die-before-switch";

/// A program that fails before it can start exits as a shell does for a program it can't run (126),
/// and for one that doesn't exist (127), as [`super::exec`] reports the same.
const CANNOT_RUN: u8 = 126;
const NOT_FOUND: u8 = 127;

/// The seed's entry (`hex __oci-seed`): serve execs until the agent closes the socket, then exit 0; a
/// broken socket or a poisoned environment exits non-zero with why on stderr.
pub fn run() -> u8 {
    match serve() {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "hex __oci-seed: {e}");
            1
        }
    }
}

fn serve() -> Result<(), OciError> {
    // The agent hands its socket in as the seed's stdin, duplicated into this process alone by the
    // spawn, so the agent never clears close-on-exec on it. Moved off fd 0 close-on-exec, so it doesn't
    // survive into a program an intermediate's clone execs (the intermediate is a fork, which keeps it;
    // only the workload's execve, which must not hold the agent's socket, closes it), and /dev/null put
    // on fd 0 in its place. It goes to the lowest free descriptor from 3, the first past standard
    // input, output and error's 0-2 (stdin(3)), so it never takes one of theirs.
    let mut stdin = sys::own_fd(0)?;
    let mut socket = rustix::io::fcntl_dupfd_cloexec(&stdin, 3)
        .map_err(|e| OciError::os("moving the seed's socket off its stdin", e))?;
    let null = rustix::fs::open(
        "/dev/null",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| OciError::os("opening /dev/null", e))?;
    crate::linux::sys::mark_cloexec_from_3()
        .map_err(|e| OciError::os("marking descriptors close-on-exec", e))?;
    rustix::io::dup2(&null, &mut stdin)
        .map_err(|e| OciError::os("putting /dev/null on fd 0", e))?;
    // The seed's own pid namespace is opened before any prune: once pruned there is no `/proc` to open
    // it through. A create seed never uses it (it never execs), but the serve loop is shared.
    let mut pid_for = PidFor::open()?;
    // A create seed on a root node joins the node's pruned mount namespace once, here, off every create's
    // path, so every init it clones copies a namespace holding nothing of the host (goal 5 plan step 2).
    if std::env::var_os(ENV_JOIN).is_some() {
        // The namespace's mount cap, read while there is a `/proc` to read it from, for every init this seed
        // clones to bound its walks of the namespace's mounts by (init::MOUNT_MAX).
        init::mount_max()?;
        join_pruned(socket.as_fd())?;
    }
    let mut specs = Specs::default();
    let mut buf = vec![0u8; seed::MAX_REQUEST];
    loop {
        match recv_request(socket.as_fd(), &mut buf)? {
            // The agent closed the socket: nothing more will come, so the seed is done.
            Received::Closed => return Ok(()),
            Received::Message { size, stdio } => {
                let (answer, pidfd) =
                    serve_one(&buf, size, stdio, &mut pid_for, &mut specs, &mut socket);
                send_answer(socket.as_fd(), &answer, pidfd.as_ref().map(AsFd::as_fd))?;
            }
        }
    }
}

/// The data of the pruned root's tmpfs: one block, a page on any page size (`nr_blocks`,
/// Documentation/filesystems/tmpfs.rst:82), the least bound tmpfs takes (0 is none, :94). It holds nothing and is
/// remounted read-only; a container's root is attached on it, which takes no block.
pub(crate) const ROOT_TMPFS: &std::ffi::CStr = c"nr_blocks=1,mode=755";

/// Prunes a mount namespace for the node's create seeds, once per node (goal 5 plan step 2,
/// `goal6-mntns.md` §12): unshare a mount namespace, make `/` private, mount a small tmpfs over
/// `mountpoint`, pivot into it, detach the old root and make the tmpfs read-only. The namespace then
/// holds exactly two mounts — its root and that tmpfs on it, holding nothing — and nothing of the host:
/// no proc, no sysfs (f78). The init attaches the container's root on the tmpfs's own root (plan step 3),
/// so the tmpfs needs no place in it. Every seed joins it ([`ENV_JOIN`]) and every init a seed clones
/// (`clone3` with `CLONE_NEWNS`) copies it, so the init builds the container's root from the descriptors
/// the agent hands it, with nothing of the host beneath it. `mountpoint` is a directory the agent ensured
/// exists; after the pivot and detach the host path it was is gone from this namespace.
///
/// The caller is a thread of the agent's of its own, which this leaves in the pruned namespace: `unshare`
/// gives it a mount namespace and root of its own (`CLONE_NEWNS` implies `CLONE_FS`), so no other thread's
/// changes. The namespace is opened before the pivot, while `/proc` is there to open it by. This is the one
/// `pivot_root` on the pruned path: each walks every thread on the host under the mount lock
/// (`fs/fs_struct.c:54-80`, `goal6-mntns.md` §11), so one per node, not one per seed.
pub fn prune_namespace(mountpoint: &Path) -> Result<OwnedFd, OciError> {
    sys::unshare(rustix::thread::UnshareFlags::NEWNS)?;
    let namespace = rustix::fs::open(
        "/proc/thread-self/ns/mnt",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the pruned mount namespace", e))?;
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| OciError::os("making the pruned namespace's mounts private", e))?;
    // A fresh small tmpfs over the mountpoint (made first if the agent didn't), left empty.
    let _ = rustix::fs::mkdir(mountpoint, Mode::from_bits_truncate(0o755));
    rustix::mount::mount(
        "tmpfs",
        mountpoint,
        "tmpfs",
        MountFlags::NOSUID,
        Some(ROOT_TMPFS),
    )
    .map_err(|e| OciError::os("the pruned namespace's root tmpfs", e))?;
    rustix::process::chdir(mountpoint)
        .map_err(|e| OciError::os("chdir the pruned namespace's root", e))?;
    rustix::process::pivot_root(c".", c".")
        .map_err(|e| OciError::os("the pruned namespace's pivot_root", e))?;
    rustix::mount::unmount(".", UnmountFlags::DETACH)
        .map_err(|e| OciError::os("detaching the pruned namespace's old root", e))?;
    rustix::process::chdir("/").map_err(|e| OciError::os("chdir /", e))?;
    rustix::mount::mount_remount("/", MountFlags::RDONLY | MountFlags::BIND, "")
        .map_err(|e| OciError::os("remounting the pruned namespace's root read-only", e))?;
    Ok(namespace)
}

/// Joins the node's pruned mount namespace, the first record on the seed's socket ([`send_namespace`]):
/// `setns(CLONE_NEWNS)` sets this process's root and working directory to the namespace's root and takes
/// no mount lock (`mntns_install`, v6.19 `fs/namespace.c:6218-6258`). The seed is single-threaded here, as
/// it must be (`fs->users == 1`). Anything but one byte and one descriptor is refused by type.
fn join_pruned(socket: BorrowedFd<'_>) -> Result<(), OciError> {
    let mut buf = [0u8; 1];
    match recv_request(socket, &mut buf)? {
        Received::Message { size: 1, stdio } => match <[OwnedFd; 1]>::try_from(stdio) {
            Ok([namespace]) => {
                sys::setns(namespace.as_fd(), libc::CLONE_NEWNS).map_err(OciError::from)
            }
            Err(fds) => Err(OciError::Broken(format!(
                "the pruned namespace came with {} descriptors, not 1",
                fds.len()
            ))),
        },
        Received::Message { size, .. } => Err(OciError::Broken(format!(
            "the pruned namespace's record is {size} bytes, not 1"
        ))),
        Received::Closed => Err(OciError::Broken(
            "the agent closed the seed's socket before sending the pruned namespace".to_owned(),
        )),
    }
}

/// The pid namespace the seed's children are born in: its own, or the container's the last single
/// clone set. Set explicitly before every clone, so no clone lands in a namespace a past request left.
struct PidFor {
    /// The seed's own pid namespace, opened at its start.
    own: OwnedFd,
    is_own: bool,
}

impl PidFor {
    fn open() -> Result<Self, OciError> {
        let own = rustix::fs::open(
            "/proc/self/ns/pid",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| OciError::os("opening the seed's own pid namespace", e))?;
        Ok(Self { own, is_own: true })
    }

    /// The pid namespace of the container init `init` (its pidfd) for the seed's next children.
    /// `EPERM` (an unprivileged seed) is returned for the caller to take the intermediate path;
    /// nothing changed then.
    fn set(&mut self, init: BorrowedFd<'_>) -> rustix::io::Result<()> {
        rustix::thread::move_into_thread_name_spaces(init, Ns::PROCESS_ID)?;
        self.is_own = false;
        Ok(())
    }

    /// The pid namespace `ns` (a namespace file) for the seed's next children: a prepared proc's, made ahead
    /// (decision 6's update, part 2).
    fn join(&mut self, ns: BorrowedFd<'_>) -> Result<(), OciError> {
        sys::setns(ns, libc::CLONE_NEWPID)?;
        self.is_own = false;
        Ok(())
    }

    /// The seed's own pid namespace again for its next children, if a single clone changed it.
    fn reset(&mut self) -> Result<(), OciError> {
        if !self.is_own {
            sys::setns(self.own.as_fd(), libc::CLONE_NEWPID)?;
            self.is_own = true;
        }
        Ok(())
    }
}

/// Most parsed `config.json`s a seed keeps ([`Specs`]). A node's seeds meet few bundles at once, and a
/// parsed config is a few KiB, so 64 bound a seed's memory to well under a MiB; past it the oldest is
/// dropped and read again at its next exec. A stated bound (CLAUDE.md §2), not a tuning.
const SPECS: usize = 64;

/// The bundles' parsed `config.json`s, each under the file's identity when it was read (device, inode,
/// modification time and size): an exec into a container whose config the seed has parsed costs one
/// `stat`, not a read and a parse, and a config replaced or rewritten since is read again. At most
/// [`SPECS`], the oldest dropped first.
#[derive(Default)]
pub struct Specs {
    entries: VecDeque<(Identity, Spec)>,
}

/// A file's identity, as `stat` gives it: a rewrite in place changes the time or the size, and a
/// replacement the inode.
#[derive(PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    dev: u64,
    ino: u64,
    mtime: (i128, i128),
    size: i128,
}

impl Specs {
    /// The spec an exec into container `dir` (its state directory) takes its process from: the one its create
    /// checked and kept there, never its bundle as it is now; with none kept (an older container) or no `dir`
    /// named (an older request), the bundle's `config.json`, checked again with the same checks (AC-T7.1.1,
    /// [`super::checked::spec_of`]). Parsed once per file, by its identity.
    fn get(&mut self, dir: Option<&str>, bundle: &str) -> Result<&Spec, OciError> {
        let kept = dir.map(|d| Path::new(d).join(super::checked::KEPT));
        let found = match kept {
            Some(kept) => match rustix::fs::stat(&kept) {
                Ok(st) => Some((kept, st)),
                Err(rustix::io::Errno::NOENT) => None,
                Err(e) => return Err(OciError::os("reading the checked spec", e)),
            },
            None => None,
        };
        let (path, st) = match found {
            Some(found) => found,
            None => {
                let path = Path::new(bundle).join("config.json");
                let st =
                    rustix::fs::stat(&path).map_err(|e| OciError::os("reading config.json", e))?;
                (path, st)
            }
        };
        let id = Identity {
            path,
            dev: st.st_dev,
            ino: st.st_ino,
            mtime: (i128::from(st.st_mtime), i128::from(st.st_mtime_nsec)),
            size: i128::from(st.st_size),
        };
        let at = match self.entries.iter().position(|(seen, _)| *seen == id) {
            Some(at) => at,
            None => {
                let spec = Spec::parse(
                    &std::fs::read(&id.path).map_err(|e| OciError::os("reading config.json", e))?,
                )?;
                if self.entries.len() >= SPECS {
                    self.entries.pop_front();
                }
                self.entries.push_back((id, spec));
                self.entries.len().saturating_sub(1)
            }
        };
        self.entries
            .get(at)
            .map(|(_, spec)| spec)
            .ok_or_else(|| OciError::Broken("the seed's parsed configs lost one".to_owned()))
    }
}

/// What an exec needs, read and opened by the seed before it clones anything: the container's process
/// with the exec's overrides, its seccomp profile, its cgroup, the init's pidfd and the namespaces of
/// the init's to join by it.
struct Prepared {
    /// The container's process, ready to become.
    becoming: init::Becoming,
    /// The program's execve, made here.
    launch: Launch,
    /// The cgroup's directory, where the program is born (or, where a birth there can't be, whose
    /// `cgroup.procs` it joins by).
    cgroup: Option<OwnedFd>,
    /// The container init's pidfd, from the agent.
    init: OwnedFd,
    /// The kinds of the init's namespaces that aren't the seed's own.
    joins: Ns,
}

/// Applies the exec's overrides to the container's process with `hex __oci-exec`'s own logic (one code
/// path, CLAUDE.md §8). The cgroup is the caller's to give: a seed opens it ([`open_cgroup`]), and the
/// node agent holds it open for an exec straight from its threads.
fn prepare(req: &Request, init: OwnedFd, specs: &mut Specs) -> Result<Prepared, OciError> {
    let spec = specs.get(req.dir.as_deref(), &req.bundle)?;
    let request = exec::Request {
        pid: req.init_pid,
        bundle: req.bundle.clone(),
        cgroup: req.cgroup.clone(),
        args: req.args.clone(),
        cwd: req.cwd.clone(),
        env: req.env.clone(),
        user: req.user,
        seam: true,
        dir: req.dir.clone(),
    };
    let process = exec::process_for(spec.process.clone().unwrap_or_default(), &request);
    // The program as it will run, the container's environment with the request's over it, held to the
    // bound of the box's own stack before anything is cloned (HEX-RT-14).
    crate::linux::process::fits_process(&process.args, &process.env, process.stack())
        .map_err(|e| OciError::Program(e.to_string()))?;
    Ok(Prepared {
        becoming: init::Becoming::of(&process, spec.linux.seccomp.as_ref())?,
        launch: Launch::of(&process)?,
        joins: joins(&spec.linux.namespaces)?,
        cgroup: None,
        init,
    })
}

/// The container's cgroup directory, `dir`, opened (while the opener is still in its own mount
/// namespace) for a clone into it, or for its `cgroup.procs`.
pub fn open_cgroup(dir: Option<&str>) -> Result<Option<OwnedFd>, OciError> {
    dir.map(|dir| {
        rustix::fs::open(
            dir,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| OciError::os("opening the container's cgroup", e))
    })
    .transpose()
}

/// The program's `execve`, made before the clone: its arguments and environment as the null-terminated
/// arrays execve(2) takes, the paths to try as execvp(3) tries them, and its working directory. So the
/// clone's child allocates nothing on its way to the program: in a fork every heap write copies a page,
/// which std's `Command` makes many of (the process class's `Spawn` holds the same discipline).
struct Launch {
    /// Owns the strings `argv` points at; moving it moves no string's bytes.
    _args: Vec<CString>,
    /// Owns the strings `envp` points at.
    _env: Vec<CString>,
    argv: Vec<*const c_char>,
    envp: Vec<*const c_char>,
    paths: Vec<CString>,
    cwd: CString,
}

impl Launch {
    /// `process`'s program: its first argument where it names a path, else that name in each folder of
    /// the process's `PATH` (the usual folders when it has none), as [`init::resolve_program`] finds it.
    fn of(process: &Process) -> Result<Self, OciError> {
        let c = |s: &str| {
            CString::new(s).map_err(|_| OciError::Config(format!("{s:?} holds a NUL byte")))
        };
        let program = process
            .args
            .first()
            .ok_or_else(|| OciError::Config("exec needs a program".to_owned()))?;
        let paths = if program.contains('/') {
            vec![c(program)?]
        } else {
            process
                .env
                .iter()
                .find_map(|e| e.strip_prefix("PATH="))
                .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                .split(':')
                .map(|dir| c(&Path::new(dir).join(program).to_string_lossy()))
                .collect::<Result<_, _>>()?
        };
        let args: Vec<CString> = process
            .args
            .iter()
            .map(|a| c(a))
            .collect::<Result<_, _>>()?;
        // As std's Command takes them: an entry without `=` names no variable.
        let env: Vec<CString> = process
            .env
            .iter()
            .filter(|e| e.contains('='))
            .map(|e| c(e))
            .collect::<Result<_, _>>()?;
        let pointers = |strs: &[CString]| -> Vec<*const c_char> {
            strs.iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect()
        };
        Ok(Self {
            argv: pointers(&args),
            envp: pointers(&env),
            _args: args,
            _env: env,
            paths,
            cwd: c(&process.cwd)?,
        })
    }

    /// In the clone's child: the program's working directory and signal state, then its execve. Returns
    /// only when it couldn't run: the status a shell gives (127 for a program not there, 126 else) and
    /// what failed.
    fn exec(&self) -> (u8, Failure) {
        if let Err(errno) = rustix::process::chdir(self.cwd.as_c_str()) {
            return (
                CANNOT_RUN,
                Failure::of("the program's working directory")(errno),
            );
        }
        if let Err(errno) = sys::default_signals() {
            return (CANNOT_RUN, Failure::of("the program's signal state")(errno));
        }
        match sys::execve_each(&self.paths, &self.argv, &self.envp) {
            e @ (libc::ENOENT | libc::ENOTDIR) => (
                NOT_FOUND,
                Failure::of("the program isn't in the container")(
                    rustix::io::Errno::from_raw_os_error(e),
                ),
            ),
            e => (
                CANNOT_RUN,
                Failure::of("execve")(rustix::io::Errno::from_raw_os_error(e)),
            ),
        }
    }
}

/// The kinds of namespace an exec joins: those the container's spec gives it, as crun's exec joins
/// them. One it made is its own; one it joined by path may be the seed's own, which is left out, since
/// joining one's own user namespace is refused (`userns_install`) and an unprivileged intermediate may
/// not join the host's others. A kind Hex doesn't make (`time`) isn't joined, as `hex oci exec` doesn't.
fn joins(namespaces: &[Namespace]) -> Result<Ns, OciError> {
    let mut joins = Ns::empty();
    for ns in namespaces {
        let (kind, proc_name) = match ns.kind.as_str() {
            "user" => (Ns::USER, "user"),
            "ipc" => (Ns::INTER_PROCESS_COMMUNICATION, "ipc"),
            "uts" => (Ns::HOST_NAME_AND_NIS_DOMAIN_NAME, "uts"),
            "network" => (Ns::NETWORK, "net"),
            "pid" => (Ns::PROCESS_ID, "pid"),
            "cgroup" => (Ns::CONTROL_GROUP, "cgroup"),
            "mount" => (Ns::MOUNT, "mnt"),
            _ => continue,
        };
        if let Some(path) = &ns.path {
            let theirs = rustix::fs::stat(path.as_str())
                .map_err(|e| OciError::os("reading a namespace the container joined", e))?;
            let ours = rustix::fs::stat(format!("/proc/self/ns/{proc_name}"))
                .map_err(|e| OciError::os("reading the seed's own namespace", e))?;
            if (theirs.st_dev, theirs.st_ino) == (ours.st_dev, ours.st_ino) {
                continue;
            }
        }
        joins |= kind;
    }
    Ok(joins)
}

/// What [`exec_direct`] did.
pub enum Direct {
    /// The program runs, the caller's own child: its pid and pidfd.
    Started(u32, OwnedFd),
    /// Nothing was started: this thread may not choose the container's pid namespace (a rootless node),
    /// or there is no vfork-style clone here (an architecture without the trampoline, a kernel or sandbox
    /// without clone3 into a cgroup), or the container has no cgroup to be born in. The init's pidfd
    /// comes back, for the caller to hand the exec to a seed instead.
    Unable(OwnedFd),
}

/// The pid namespace the calling thread's children are born in is the container's for one clone, then the
/// process's own again: `setns(2)` changes it for the calling thread alone.
fn own_pid_namespace() -> Result<&'static OwnedFd, OciError> {
    static OWN: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();
    if let Some(own) = OWN.get() {
        return Ok(own);
    }
    let own = rustix::fs::open(
        "/proc/self/ns/pid",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the node's own pid namespace", e))?;
    let _ = OWN.set(own);
    OWN.get()
        .ok_or_else(|| OciError::Broken("the node's pid namespace wasn't kept".to_owned()))
}

std::thread_local! {
    /// The stack a vfork-style clone's child runs on, per thread that clones ([`sys::ChildStack`]); and
    /// whether this thread's children's pid namespace couldn't be set back after a clone, which makes it
    /// hand every later exec to a seed rather than clone into a namespace it can't be sure of.
    static STACK: std::cell::RefCell<Option<sys::ChildStack>> = const { std::cell::RefCell::new(None) };
    static UNSURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The calling thread pinned to the CPU it runs on, with the CPUs it may run on kept to give back.
struct Pin {
    mask: rustix::thread::CpuSet,
}

impl Pin {
    /// Pins the calling thread to its CPU; `None` (and nothing changed) where its mask can't be read or
    /// set, as on a CPU a cpuset no longer allows.
    fn here() -> Option<Self> {
        let mask = rustix::thread::sched_getaffinity(None).ok()?;
        let mut here = rustix::thread::CpuSet::new();
        here.set(rustix::thread::sched_getcpu());
        rustix::thread::sched_setaffinity(None, &here).ok()?;
        Some(Self { mask })
    }

    /// Gives the calling thread back the CPUs it had; where the kernel refuses them (a cpuset changed
    /// since), every CPU, which its cpuset then narrows, so no thread or program stays held to one CPU.
    /// A system call alone on memory made before, so a vfork-style clone's child may make it.
    fn release(&self) {
        if rustix::thread::sched_setaffinity(None, &self.mask).is_err() {
            let mut all = rustix::thread::CpuSet::new();
            for cpu in 0..rustix::thread::CpuSet::MAX_CPU {
                all.set(cpu);
            }
            let _ = rustix::thread::sched_setaffinity(None, &all);
        }
    }
}

/// Exec in a container straight from the calling thread (a thread of the node agent), as posix_spawn
/// starts a program: the thread's children's pid namespace set to the container init `init`'s, then one
/// `clone3(CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP | CLONE_PIDFD)` ([`sys::clone_vfork`]). The program
/// is born in the container's cgroup and pid namespace as the caller's own child, shares the caller's
/// memory until its execve (so nothing of the caller is copied, and nothing torn down), joins the
/// container's other namespaces, becomes its process and execs, making system calls alone on what
/// [`prepare`] made. `stdio` become its standard streams and `cgroup` (the container's cgroup directory,
/// which the caller holds open) is where it is born. No process but the program, no seed, no `hex`
/// exec. Where it can't, it says so ([`Direct::Unable`]) and starts nothing.
pub fn exec_direct(
    req: &Request,
    stdio: [BorrowedFd<'_>; 3],
    init: OwnedFd,
    cgroup: Option<BorrowedFd<'_>>,
    specs: &mut Specs,
) -> Result<Direct, OciError> {
    // Prepared first, whichever way the program then starts: a program past its box's exec bound is refused
    // here, by name, before a seed is asked (HEX-RT-14).
    let p = prepare(req, init, specs)?;
    if UNSURE.with(std::cell::Cell::get) {
        return Ok(Direct::Unable(p.init));
    }
    let Some(cgroup) = cgroup else {
        return Ok(Direct::Unable(p.init));
    };
    let blocked = sys::block_signals()?;
    let set = p.joins.contains(Ns::PROCESS_ID);
    if set {
        match rustix::thread::move_into_thread_name_spaces(p.init.as_fd(), Ns::PROCESS_ID) {
            Ok(()) => {}
            Err(rustix::io::Errno::PERM) => {
                drop(blocked);
                return Ok(Direct::Unable(p.init));
            }
            Err(e) => {
                return Err(OciError::os("choosing the container's pid namespace", e));
            }
        }
    }
    let rest = p.joins.difference(Ns::PROCESS_ID);
    // The child starts on this thread's CPU: the thread is pinned to it for the clone, so the child
    // inherits the pin and runs as soon as the thread blocks in the vfork, rather than wait for an idle
    // CPU the kernel's fork balancing picks to wake. Measured on fedora (bench/competitive/probes/
    // exec_pin.md): the child first runs 18 µs after its fork pinned and 86 µs unpinned (p50s), and
    // exec's p50 falls from 1.186–1.227 ms to 1.123–1.141. The child gives the pin back before anything
    // else, so the program never runs pinned, and the thread does after the clone.
    let pin = Pin::here();
    // What the child failed at, if it did: written by the child into this thread's memory, which it
    // shares, before it exits, and read once the vfork has let this thread go, so the kernel's E2BIG
    // reaches the caller as a refusal, not as a program that ends 126 (HEX-RT-14).
    let failed = std::cell::Cell::new(None);
    let child = || {
        if let Some(pin) = &pin {
            pin.release();
        }
        let (code, why) = program(
            &p.becoming,
            &p.launch,
            stdio,
            None,
            None,
            (p.init.as_fd(), rest),
        );
        failed.set(Some(why));
        sys::report(why);
        code
    };
    let cloned = STACK.with(|stack| -> Result<_, OciError> {
        let mut stack = stack
            .try_borrow_mut()
            .map_err(|_| OciError::Broken("a thread's child stack is in use".to_owned()))?;
        if stack.is_none() {
            *stack = Some(sys::ChildStack::map()?);
        }
        match stack.as_ref() {
            Some(stack) => sys::clone_vfork(cgroup, stack, &child),
            None => Ok(None),
        }
    });
    if let Some(pin) = &pin {
        pin.release();
    }
    // Whatever the clone did, this thread's children are born in the node's own pid namespace again.
    let reset = if set {
        own_pid_namespace()
            .and_then(|own| sys::setns(own.as_fd(), libc::CLONE_NEWPID).map_err(OciError::from))
    } else {
        Ok(())
    };
    drop(blocked);
    if let Err(e) = reset {
        UNSURE.with(|unsure| unsure.set(true));
        // A program started into a namespace this thread can't leave must not run unwatched: end it.
        if let Ok(Some((_, pidfd))) = &cloned {
            let _ = rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL);
        }
        return Err(e);
    }
    match (cloned?, failed.get()) {
        // The kernel refused the program's arguments and environment: this thread's stack limit is below
        // what the node last read. The child has exited (the vfork waited for it); it is reaped here,
        // and the limit read again, so the next exec is held to it before any clone.
        (
            Some((_, pidfd)),
            Some(Failure {
                errno: rustix::io::Errno::TOOBIG,
                ..
            }),
        ) => {
            let _reaped = rustix::process::waitid(
                rustix::process::WaitId::PidFd(pidfd.as_fd()),
                rustix::process::WaitIdOptions::EXITED,
            )
            .map_err(|e| OciError::os("reaping a program the kernel refused", e))?;
            crate::linux::process::relearn_stack();
            Err(OciError::Program(format!(
                "execve refused its arguments and environment (E2BIG): this node's stack limit, read \
                 again, allows {} bytes ({})",
                crate::linux::process::exec_limit().block,
                crate::linux::process::exec_limit().source
            )))
        }
        (Some((pid, pidfd)), _) => {
            let pid = u32::try_from(pid)
                .map_err(|_| OciError::os("clone3 the program", "a pid out of range"))?;
            Ok(Direct::Started(pid, pidfd))
        }
        (None, _) => Ok(Direct::Unable(p.init)),
    }
}

/// Serves one request: an exec into a container ([`serve_exec`]) or a container to make
/// ([`serve_create`]), read by its kind and the descriptors it carried. A refusal the seed makes is
/// returned in its place, and the seed stays serving.
fn serve_one(
    buf: &[u8],
    size: usize,
    fds: Vec<OwnedFd>,
    pid_for: &mut PidFor,
    specs: &mut Specs,
    socket: &mut OwnedFd,
) -> (Answer, Option<OwnedFd>) {
    let bytes = buf.get(..size.min(buf.len())).unwrap_or_default();
    match seed::read_message(bytes, size, fds.len()) {
        Ok(seed::Message::Exec(req)) => serve_exec(req, fds, pid_for, specs),
        Ok(seed::Message::Create(create)) => serve_create(
            &create,
            fds,
            Begin {
                how: How::Clone,
                pid_for,
            },
        ),
        Ok(seed::Message::Park(park)) => serve_park(park, fds, socket, pid_for),
        Err(fault) => refused(Failed::Request, fault.to_string()),
    }
}

/// How a create's init comes to be: cloned by the seed now, or the init parked ahead that this process is
/// (decision 8, "The p99.9 that stayed").
#[derive(Clone, Copy)]
enum How {
    /// The seed clones the init into its cgroup and namespaces ([`sys::clone_sibling`]).
    Clone,
    /// This process is an init parked in its cgroup with the namespaces `flags` made, leading a session of its own
    /// since it parked, and claimed on its claim socket, which it answers only to refuse ([`parked`]).
    Parked { flags: u64 },
}

impl How {
    /// Makes the init a session's leader ([`init::lead_session`]) as a fresh clone's first act. A parked init led its
    /// own at park, so its create's path takes no `setsid` (goal 17, lever L1).
    fn lead_session(self) -> Result<(), OciError> {
        match self {
            How::Clone => init::lead_session(),
            How::Parked { .. } => Ok(()),
        }
    }
}

/// How a create's init comes to be ([`How`]), and the pid namespace the seed's next children are born in, which a
/// clone carrying a proc made ahead changes and sets back ([`PidFor`]).
struct Begin<'p> {
    how: How,
    pid_for: &'p mut PidFor,
}

/// Where a create's init is cloned, or, for a parked init, the claim taken: the clone's outcome as
/// [`sys::clone_sibling`] gives it. A parked init made with other namespaces than the create's `flags` is
/// refused by type, never made into a container its spec didn't ask for (CLAUDE.md §3). One that takes the claim
/// says nothing on its claim socket (the lead's (A) of 2026-11-09: no early answer) and goes on as the child: its
/// first word is the one on its sync pipe, which the agent's claim waits for, and the agent holds its pid and pidfd
/// from its park, and knows its mount namespace is made the create's way, the one it was parked for.
fn begin(
    how: How,
    cgroup: Option<BorrowedFd<'_>>,
    flags: u64,
) -> Result<Option<sys::Sibling>, OciError> {
    match how {
        How::Clone => sys::clone_sibling(cgroup, flags),
        How::Parked { flags: made } => {
            if made != flags {
                return Err(OciError::Broken(format!(
                    "an init parked with namespaces {made:#x} was claimed by a create that makes {flags:#x}"
                )));
            }
            Ok(Some(sys::Sibling::Child))
        }
    }
}

/// Makes an init ahead into an idle cgroup (decision 8, "The p99.9 that stayed"): `fds` are the cgroup and the
/// init's end of its claim socket. The seed clones it into the cgroup with the park's namespaces, the agent's
/// own child, and answers with its pid and pidfd. The init holds nothing of any box until a create claims it
/// ([`parked`]); its copy of the seed's socket is replaced by an end of a pipe of its own, so the seed's
/// requests reach the seed alone.
fn serve_park(
    park: seed::Park,
    fds: Vec<OwnedFd>,
    socket: &mut OwnedFd,
    pid_for: &mut PidFor,
) -> (Answer, Option<OwnedFd>) {
    let mut fds = fds.into_iter();
    let (Some(cgroup), Some(claim)) = (fds.next(), fds.next()) else {
        return refused(
            Failed::Request,
            format!("a park carries {} descriptors", seed::PARK_FDS),
        );
    };
    if park.flags & !parkable() != 0 {
        return refused(
            Failed::Request,
            format!("a park's flags {:#x} name more than namespaces", park.flags),
        );
    }
    match sys::clone_sibling(Some(cgroup.as_fd()), park.flags) {
        Ok(None) => refused(
            Failed::Unable,
            "this kernel or sandbox has no clone3 into a cgroup".to_owned(),
        ),
        Ok(Some(sys::Sibling::Child)) => {
            drop(cgroup);
            let replaced = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
                .and_then(|(r, _w)| rustix::io::dup2(&r, socket));
            if replaced.is_err() {
                sys::exit_child(127);
            }
            parked(claim, park.flags, pid_for)
        }
        Ok(Some(sys::Sibling::Parent { pid, pidfd })) => {
            drop((cgroup, claim));
            match u32::try_from(pid) {
                Ok(pid) => (Answer::Started { pid }, Some(pidfd)),
                Err(_) => {
                    let _ =
                        rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
                    refused(
                        Failed::Start,
                        "clone3 returned a pid out of range".to_owned(),
                    )
                }
            }
        }
        Err(e) => refused(Failed::Start, e.to_string()),
    }
}

/// The namespaces a park may make: `CLONE_NEW*` bits and `CLONE_EMPTY_MNTNS` (an init parked for the empty way,
/// [`super::mntns::clone_flags`]), nothing that shares or changes the clone otherwise. A user
/// namespace's maps are written once the init is claimed, by the create's own handshake, as for a fresh clone.
fn parkable() -> u64 {
    [
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWNS,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWNET,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUTS,
    ]
    .into_iter()
    .filter_map(|f| u64::try_from(f).ok())
    .fold(super::mntns::CLONE_EMPTY_MNTNS, |all, f| all | f)
}

/// A parked init's life (decision 8): it empties its effective capabilities, says on `claim` that it is parked,
/// leads a session of its own, and waits there for one create, then makes that container as a
/// freshly cloned init does, through the create's own path with [`How::Parked`]. A refusal goes back on the
/// claim socket and the init ends; so does the agent closing the socket (an idle cgroup evicted, the node
/// shutting down). It never returns.
fn parked(claim: OwnedFd, flags: u64, pid_for: &mut PidFor) -> ! {
    // While parked it exercises no capability: its effective set is emptied, and raised again from its
    // permitted set only once a create claims it (capabilities(7): "Effective: the set used by the kernel to
    // perform permission checks"; a capability dropped from the permitted set could never be regained, and the
    // init must still build the container as root). One that can't empty it ends, never parked holding it.
    let Ok(held) = rustix::thread::capabilities(None) else {
        sys::exit_child(127);
    };
    let idle = rustix::thread::CapabilitySets {
        effective: rustix::thread::CapabilitySet::empty(),
        ..held
    };
    if rustix::thread::set_capabilities(None, idle).is_err() {
        sys::exit_child(127);
    }
    // And non-dumpable while it waits, so no other process can ptrace it, read its memory or take its descriptors
    // (its claim socket) without CAP_SYS_PTRACE (ptrace(2), "Ptrace access mode checking": a target that isn't
    // dumpable refuses a caller without it; pidfd_getfd(2) checks PTRACE_MODE_ATTACH_REALCREDS). Restored once
    // claimed: a rootless node writes the claimed init's user namespace maps through its /proc files.
    let Ok(dumpable) = rustix::process::dumpable_behavior() else {
        sys::exit_child(127);
    };
    if rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .is_err()
    {
        sys::exit_child(127);
    }
    // Parked, said on its claim socket as a seed says a start, so the agent holds it only once it is so.
    let said = rustix::process::pidfd_open(
        rustix::process::getpid(),
        rustix::process::PidfdFlags::empty(),
    )
    .map_err(|e| OciError::os("opening a parked init's own pidfd", e))
    .and_then(|me| send_answer(claim.as_fd(), &Answer::Started { pid: 0 }, Some(me.as_fd())));
    if said.is_err() {
        sys::exit_child(127);
    }
    // A session of its own, led now that it has said it is parked ([`init::lead_session`]): the reschedule its move to
    // a new autogroup forces is neither the delete's that parked it, which waits for that word
    // (`run::Parked::wait_parked`), nor the claiming create's (goal 17, lever L1). One that couldn't lead one refuses
    // its claim, never making a container in the seed's session.
    let led = init::lead_session();
    let mut buf = vec![0u8; seed::MAX_REQUEST];
    let (size, fds) = match recv_request(claim.as_fd(), &mut buf) {
        Ok(Received::Message { size, stdio }) => (size, stdio),
        Ok(Received::Closed) | Err(_) => sys::exit_child(0),
    };
    let bytes = buf.get(..size.min(buf.len())).unwrap_or_default();
    let answer = match seed::read_message(bytes, size, fds.len()) {
        Ok(seed::Message::Create(_)) if led.is_err() => {
            refused(
                Failed::Start,
                "a parked init couldn't lead a session of its own".to_owned(),
            )
            .0
        }
        Ok(seed::Message::Create(_))
            if rustix::thread::set_capabilities(None, held).is_err()
                || rustix::process::set_dumpable_behavior(dumpable).is_err() =>
        {
            refused(
                Failed::Start,
                "a parked init couldn't raise its capabilities again".to_owned(),
            )
            .0
        }
        // The seed's PidFor, inherited through the fork: a root node's seed opened it before it pruned its mount
        // namespace, which leaves no /proc to open another through. A create it can take never uses it: the only
        // proc made ahead it takes shows its own pid namespace, which it is in already.
        Ok(seed::Message::Create(create)) => {
            serve_create(
                &create,
                fds,
                Begin {
                    how: How::Parked { flags },
                    pid_for,
                },
            )
            .0
        }
        Ok(_) => {
            refused(
                Failed::Request,
                "a parked init takes a create and nothing else".to_owned(),
            )
            .0
        }
        Err(fault) => refused(Failed::Request, fault.to_string()).0,
    };
    let _ = send_answer(claim.as_fd(), &answer, None);
    sys::exit_child(127)
}

/// Makes a container the agent asked for (decision 5, goal 5 plan step 4): the seed opens its cgroup and,
/// with the fresh namespaces its spec asks for, clones its init as a sibling — the agent's own child,
/// born in the cgroup, PID 1 of its new pid namespace — and hands the agent its pidfd. The child does
/// what the shim's child does ([`init::clone_child`]), up to the exec FIFO, then execs the container's
/// program, so nothing of the seed's address space is carried past the exec (f17) and any state unique to
/// the container is made anew in the child (f15). No process but the init, no `hex` exec per create.
///
/// `fds` are the create's: the init's sync pipe (write end), the map pipe (read end) in the older kinds, then a
/// network namespace made ahead when the create says one rides with it. The init joins the namespaces its spec
/// names by path itself; one that joins a pid or user namespace by path can't be made by one clone, so the
/// runtime makes it through the shim and never hands it here (`init::needs_shim`).
fn serve_create(
    create: &seed::Create,
    fds: Vec<OwnedFd>,
    begun: Begin<'_>,
) -> (Answer, Option<OwnedFd>) {
    // The spec the runtime checked rides last (`read_message` held the count to the create's), and the
    // container is made from its bytes alone, never from a read of the bundle, which may since hold a spec
    // nobody checked (AC-T7.1.1). A create without it is refused.
    let mut fds = fds;
    let checked = if create.config { fds.pop() } else { None };
    let Some(checked) = checked else {
        return refused(
            Failed::Request,
            "a create carries the spec its runtime checked (a create kind of this build)"
                .to_owned(),
        );
    };
    let spec = match super::checked::read(checked.as_fd()).and_then(|bytes| Spec::parse(&bytes)) {
        Ok(spec) => spec,
        Err(e) => return refused(Failed::Request, e.to_string()),
    };
    drop(checked);
    let mut fds = fds.into_iter();
    // `read_message` held the descriptor count to the create's, so each rides exactly when the create says: the sync
    // pipe, the map pipe in the older kinds (never in a lean create, which is pruned), the network namespace, then
    // (on the pruned path) the root descriptors.
    let sync_w = fds.next();
    let map_r = match create.fixed {
        seed::Fixed::All => fds.next(),
        seed::Fixed::Lean { .. } => None,
    };
    let Some(sync_w) = sync_w else {
        return refused(
            Failed::Request,
            "a create carries its init's sync pipe".to_owned(),
        );
    };
    let network = if create.network { fds.next() } else { None };
    if create.pruned {
        // The map pipe carries only a user namespace's maps and the hooks' state, which the pruned path never asks
        // for: an older kind's copy is dropped before the clone, so the init doesn't inherit it.
        drop(map_r);
        return serve_create_pruned(create, &spec, fds, sync_w, network, begun);
    }
    // The runtime hands a seed no container that joins a pid or user namespace by path (it makes those
    // through the shim, `init::needs_shim`); one that came anyway is refused by type, never made in the
    // wrong namespace (CLAUDE.md §3). Every other namespace by path the init joins itself.
    if init::needs_shim(&spec) {
        return refused(
            Failed::Start,
            "a container that joins a pid or user namespace by path is made through the shim, not a seed"
                .to_owned(),
        );
    }
    let cgroup = match open_cgroup(create.cgroup.as_deref()) {
        Ok(cgroup) => cgroup,
        Err(e) => return refused(Failed::Start, e.to_string()),
    };
    let made_ahead = network.is_some();
    let flags = init::fresh_clone_flags(&spec, made_ahead, false);
    match begin(begun.how, cgroup.as_ref().map(AsFd::as_fd), flags) {
        Ok(None) => refused(
            Failed::Unable,
            "this kernel or sandbox has no clone3 into a cgroup".to_owned(),
        ),
        Ok(Some(sys::Sibling::Child)) => {
            // The init. It returns here only if a step before the exec FIFO failed; its error goes to
            // the agent on the sync pipe, as the shim's does, then it exits (it must not run the seed's
            // serve loop). Born in the cgroup and the fresh namespaces, the agent's own child.
            let r = begun.how.lead_session().and_then(|()| {
                init::clone_child(
                    &spec,
                    &create.bundle,
                    Path::new(&create.dir),
                    sync_w.as_fd(),
                    map_r,
                    network,
                )
            });
            if let Err(e) = r {
                init::msg(sync_w.as_fd(), &format!("E{e}"));
                sys::exit_child(127);
            }
            // The no-process case: set up, nothing to run. Its readiness went on the sync pipe already.
            sys::exit_child(0);
        }
        Ok(Some(sys::Sibling::Parent { pid, pidfd })) => {
            drop((sync_w, map_r, network, cgroup));
            match u32::try_from(pid) {
                Ok(pid) => (Answer::Started { pid }, Some(pidfd)),
                Err(_) => {
                    let _ =
                        rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
                    refused(
                        Failed::Start,
                        "clone3 returned a pid out of range".to_owned(),
                    )
                }
            }
        }
        Err(e) => refused(Failed::Start, e.to_string()),
    }
}

/// Makes a container on the pruned-seed root path (goal 5 plan step 2). The seed's own mount namespace
/// is pruned to two mounts, so it reaches no host path; the create carried, after the sync pipe and a
/// network namespace, its fixed descriptors (a lean create's: the cgroup directory for a seed's clone, the state
/// directory and the rootfs tree; the older kinds' the bundle directory, read by nothing, and the cgroup directory
/// before those two) and one tree per bind mount, and last the spec the runtime checked. The seed makes the
/// container from that spec, clones the init into the cgroup with the fresh namespaces the spec asks for (`CLONE_NEWNS` copies the
/// pruned namespace), and the init builds the root from the trees ([`init::clone_child_pruned`]), with no
/// `pivot_root`. The runtime routes a user-namespaced, hook-bearing or path-joined container to the shim,
/// never here; one that reaches here anyway is refused by type (CLAUDE.md §3).
fn serve_create_pruned(
    create: &seed::Create,
    spec: &Spec,
    mut fds: impl Iterator<Item = OwnedFd>,
    sync_w: OwnedFd,
    network: Option<OwnedFd>,
    begun: Begin<'_>,
) -> (Answer, Option<OwnedFd>) {
    let Begin { how, pid_for } = begun;
    // The older kinds carry the bundle directory, which nothing reads (the spec rides checked), and the cgroup
    // directory always; a lean create carries the cgroup directory only for a seed's clone, which is born in it.
    let (bundle_dir, cgroup_dir) = match create.fixed {
        seed::Fixed::All => (fds.next(), fds.next()),
        seed::Fixed::Lean { cgroup: true } => (None, fds.next()),
        seed::Fixed::Lean { cgroup: false } => (None, None),
    };
    drop(bundle_dir);
    let (Some(container_dir), Some(rootfs)) = (fds.next(), fds.next()) else {
        return refused(
            Failed::Request,
            "a pruned create carries its state directory and its rootfs".to_owned(),
        );
    };
    let binds: Vec<OwnedFd> = fds.by_ref().take(create.binds as usize).collect();
    if binds.len() != create.binds as usize {
        return refused(
            Failed::Request,
            format!(
                "a pruned create said {} bind trees but carried {}",
                create.binds,
                binds.len()
            ),
        );
    }
    let path_joins: Vec<OwnedFd> = fds.by_ref().take(create.path_joins as usize).collect();
    if path_joins.len() != create.path_joins as usize {
        return refused(
            Failed::Request,
            format!(
                "a pruned create said {} path-joined namespaces but carried {}",
                create.path_joins,
                path_joins.len()
            ),
        );
    }
    let dev_mount = match create.dev_mount {
        Some(mount) => match fds.next() {
            Some(tree) => Some(super::run::PreparedDev { mount, tree }),
            None => {
                return refused(
                    Failed::Request,
                    "a prepared create carried no /dev tree".to_owned(),
                );
            }
        },
        None => None,
    };
    // The pair rides last, when the create says so: `read_message` held the descriptor count to the create's.
    let ipc_uts = if create.ipc_uts {
        match (fds.next(), fds.next()) {
            (Some(ipc), Some(uts)) => Some(Pair { ipc, uts }),
            _ => {
                return refused(
                    Failed::Request,
                    "a create that says it carries an ipc and a uts namespace carried none"
                        .to_owned(),
                );
            }
        }
    } else {
        None
    };
    let proc_mount = match create.proc_mount {
        Some(mount) => match (fds.next(), fds.next()) {
            (Some(pid_namespace), Some(tree)) => Some((
                mount,
                init::PreparedProc {
                    pid_namespace,
                    tree,
                },
            )),
            _ => {
                return refused(
                    Failed::Request,
                    "a create with a prepared proc carried no pid namespace and tree".to_owned(),
                );
            }
        },
        None => None,
    };
    let masked_directory = match (create.masked_directory, fds.next()) {
        (false, _) => None,
        (true, Some(empty)) => Some(empty),
        (true, None) => {
            return refused(
                Failed::Request,
                "a create with the masked directories' source carried none".to_owned(),
            );
        }
    };
    // The runtime routes a container with a user namespace of its own, or one that joins a pid, user or
    // mount namespace by path, through the shim (the classic root path), never to a pruned seed; one
    // that came anyway is refused by type, never built in the wrong namespace (CLAUDE.md §3). A network,
    // ipc, uts or cgroup namespace by path is joined by the `path_joins` descriptors, not here.
    let joins_mount_by_path = spec
        .linux
        .namespaces
        .iter()
        .any(|n| n.kind == "mount" && n.path.is_some());
    if spec.makes_namespace("user") || init::needs_shim(spec) || joins_mount_by_path {
        return refused(
            Failed::Start,
            "a pruned create makes no user namespace and joins no pid, user or mount namespace by path; \
             such a container is made through the shim, not a pruned seed"
                .to_owned(),
        );
    }
    // The agent gives a pair only to a container that makes both namespaces, so one that doesn't is refused by
    // type, never given namespaces its spec didn't ask for (CLAUDE.md §3).
    if ipc_uts.is_some() && !(spec.makes_namespace("ipc") && spec.makes_namespace("uts")) {
        return refused(
            Failed::Start,
            "a create carried an ipc and a uts namespace for a container that doesn't make both"
                .to_owned(),
        );
    }
    let made_ahead = network.is_some();
    let mut flags = init::fresh_clone_flags(spec, made_ahead, ipc_uts.is_some());
    let (made, cloned) = match how {
        How::Clone => {
            // Born in the box's cgroup, so a seed's clone needs its directory, which the agent opens for it.
            let Some(cgroup_dir) = &cgroup_dir else {
                return refused(
                    Failed::Request,
                    "a seed's clone carries the cgroup directory the init is born in".to_owned(),
                );
            };
            // A proc made ahead shows a pid namespace made ahead (decision 6's update, part 2): the init is cloned
            // into it, its pid 1, in place of a fresh one, and the seed's next children are its own again after.
            if let Some((_, proc)) = &proc_mount {
                if let Err(e) = pid_for.join(proc.pid_namespace.as_fd()) {
                    return refused(Failed::Start, e.to_string());
                }
                flags &= !u64::try_from(libc::CLONE_NEWPID).unwrap_or(0);
            }
            let (made, cloned) = clone_pruned(cgroup_dir.as_fd(), flags, create.mount_namespace);
            // The seed's next children are born in its own pid namespace again, whatever the clone did; a seed that
            // can't set that back would bear them in another container's, so it is broken and the agent replaces
            // it (with the init it just started ended). The clone's child doesn't: it is the init.
            let reset = match &cloned {
                Ok(Some(sys::Sibling::Child)) => Ok(()),
                _ => pid_for.reset(),
            };
            if let Err(e) = reset {
                if let Ok(Some(sys::Sibling::Parent { pidfd, .. })) = &cloned {
                    let _ =
                        rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::KILL);
                }
                return refused(Failed::Seed, e.to_string());
            }
            (
                made,
                cloned.map_err(|e| OciError::os("clone3 the program into the container", e)),
            )
        }
        // An init parked ahead has its pid namespace already (decision 8), and its mount namespace as the create's
        // way makes it (decision 6's update; [`super::mntns::clone_flags`]): copied or empty, made at its clone; from
        // the tree, none of its own until it joins the agent's in the init's build, as a fresh one does. A create
        // whose way's flags aren't the ones it was parked with is refused ([`begin`]), and so is one carrying a proc
        // of another pid namespace than its own. The proc the node made for it (parked procs) shows this init as its pid 1, its
        // `self` reading "1" (proc(5), /proc/self; the pid as the mount's pid namespace numbers it), and only that
        // proc is taken. The runtime sends it no other (run.rs, start_via_seed); one that came anyway is refused by
        // type.
        How::Parked { .. } => {
            if let Some((_, proc)) = &proc_mount
                && !shows_self_as_one(proc.tree.as_fd())
            {
                return refused(
                    Failed::Request,
                    "a parked init was claimed with a proc of another pid namespace than its own"
                        .to_owned(),
                );
            }
            (
                create.mount_namespace,
                // In its cgroup already: a claim carries no cgroup directory, and an older kind's goes unused.
                begin(
                    how,
                    None,
                    super::mntns::clone_flags(flags, create.mount_namespace),
                ),
            )
        }
    };
    match cloned {
        Ok(None) => refused(
            Failed::Unable,
            "this kernel or sandbox has no clone3 into a cgroup".to_owned(),
        ),
        Ok(Some(sys::Sibling::Child)) => {
            // The init, born in the cgroup and the fresh namespaces, the agent's own child. It builds
            // the root from the trees the agent cloned and returns here only on a failure before the
            // exec FIFO, whose error goes to the agent on the sync pipe.
            let sources = init::PrunedSources {
                rootfs,
                from_tree: made == MountNamespace::FromTree,
                dev_mount,
                proc_mount,
                masked_directory,
                binds,
                path_joins,
            };
            let r = how.lead_session().and_then(|()| {
                init::clone_child_pruned(
                    spec,
                    &create.bundle,
                    container_dir,
                    sources,
                    sync_w.as_fd(),
                    network,
                    ipc_uts,
                )
            });
            if let Err(e) = r {
                init::msg(sync_w.as_fd(), &format!("E{e}"));
                sys::exit_child(127);
            }
            sys::exit_child(0);
        }
        Ok(Some(sys::Sibling::Parent { pid, pidfd })) => {
            drop((sync_w, cgroup_dir, container_dir, rootfs, binds, network));
            drop((path_joins, dev_mount, ipc_uts, proc_mount, masked_directory));
            match u32::try_from(pid) {
                Ok(pid) => (
                    Answer::StartedIn {
                        pid,
                        mount_namespace: made,
                    },
                    Some(pidfd),
                ),
                Err(_) => {
                    let _ =
                        rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
                    refused(
                        Failed::Start,
                        "clone3 returned a pid out of range".to_owned(),
                    )
                }
            }
        }
        Err(e) => refused(Failed::Start, e.to_string()),
    }
}

/// Whether the proc `tree` names the calling process pid 1: its `self` link, which proc reads as the caller's pid
/// in the mount's pid namespace (v6.19 `fs/proc/self.c:11-28`), reads "1". A process not in that namespace has no
/// pid there, and the link fails with `ENOENT` (`:19-20`).
fn shows_self_as_one(tree: BorrowedFd<'_>) -> bool {
    rustix::fs::readlinkat(tree, "self", Vec::new()).is_ok_and(|link| link.as_bytes() == b"1")
}

/// Whether this seed's kernel refused `CLONE_EMPTY_MNTNS` once (Linux 7.1): learned from its first such
/// clone and kept for the seed's life, so the refusal costs one `clone3` a seed (`goal6-mntns.md` §6).
static NO_EMPTY_MNTNS: AtomicBool = AtomicBool::new(false);

/// Clones a pruned create's init (decision 6's update) with its namespace made `way`, and says how it was
/// made. The spec's `flags` hold `CLONE_NEWNS`, the seed's namespace copied. From its tree, the init is
/// cloned without it and joins the namespace the agent made. Empty, `CLONE_EMPTY_MNTNS` takes its place;
/// a kernel that refuses that flag with `EINVAL` (it refuses unknown flags before it makes anything,
/// `kernel/fork.c:3017-3022`) gets the copy, now and for the seed's life, and the answer says so. The
/// spec's other flags are the ones the copy's clone takes, so an `EINVAL` the copy would get too comes
/// back from the copy's clone, not hidden.
fn clone_pruned(
    cgroup: BorrowedFd<'_>,
    flags: u64,
    way: MountNamespace,
) -> (
    MountNamespace,
    Result<Option<sys::Sibling>, rustix::io::Errno>,
) {
    match way {
        MountNamespace::FromTree => (
            way,
            sys::clone_sibling_errno(Some(cgroup), super::mntns::clone_flags(flags, way)),
        ),
        MountNamespace::Empty if !NO_EMPTY_MNTNS.load(Ordering::Relaxed) => {
            let empty = super::mntns::clone_flags(flags, way);
            match sys::clone_sibling_errno(Some(cgroup), empty) {
                Err(rustix::io::Errno::INVAL) => {
                    NO_EMPTY_MNTNS.store(true, Ordering::Relaxed);
                    (
                        MountNamespace::Copy,
                        sys::clone_sibling_errno(Some(cgroup), flags),
                    )
                }
                cloned => (way, cloned),
            }
        }
        MountNamespace::Empty | MountNamespace::Copy => (
            MountNamespace::Copy,
            sys::clone_sibling_errno(Some(cgroup), flags),
        ),
    }
}

/// Serves an exec into a container (decision 3): the program started by a single clone where the seed
/// may, or through an intermediate where it may not, with its pidfd. A refusal is returned in its place.
fn serve_exec(
    req: Request,
    fds: Vec<OwnedFd>,
    pid_for: &mut PidFor,
    specs: &mut Specs,
) -> (Answer, Option<OwnedFd>) {
    let [stdin, stdout, stderr, init]: [OwnedFd; seed::REQUEST_FDS] = match fds.try_into() {
        Ok(four) => four,
        Err(got) => {
            return refused(
                Failed::Request,
                format!(
                    "an exec carries {} descriptors, not {} (its stdio and the init's pidfd)",
                    got.len(),
                    seed::REQUEST_FDS
                ),
            );
        }
    };
    let stdio = [stdin, stdout, stderr];
    let prepared = match prepare(&req, init, specs).and_then(|mut p| {
        p.cgroup = open_cgroup(req.cgroup.as_deref())?;
        Ok(p)
    }) {
        Ok(prepared) => prepared,
        // Past the exec bound this seed's own stack limit gives (HEX-RT-14): said as that, so the agent
        // refuses the program by name and reads its own limit again.
        Err(OciError::Program(why)) => return refused(Failed::Program, why),
        Err(e) => return refused(Failed::Start, e.to_string()),
    };
    match single_clone(prepared, stdio, pid_for) {
        Ok(Single::Started(pid, pidfd)) => (Answer::Started { pid }, Some(pidfd)),
        Ok(Single::Unable(prepared, stdio)) => through_intermediate(*prepared, stdio, pid_for),
        Err((failed, e)) => refused(failed, e.to_string()),
    }
}

/// What [`single_clone`] did.
enum Single {
    /// The program runs, the agent's child: its pid and pidfd.
    Started(u32, OwnedFd),
    /// Nothing was started: the seed may not choose the container's pid namespace (a rootless seed),
    /// or this kernel has no `clone3` into a cgroup. What it was given comes back for the intermediate.
    Unable(Box<Prepared>, [OwnedFd; 3]),
}

/// Starts the program by one clone of the seed's: its children's pid namespace set to the container's,
/// then `clone3(CLONE_PARENT | CLONE_INTO_CGROUP | CLONE_PIDFD)`. The program joins the container's
/// other namespaces itself, all at once by the init's pidfd.
fn single_clone(
    p: Prepared,
    stdio: [OwnedFd; 3],
    pid_for: &mut PidFor,
) -> Result<Single, (Failed, OciError)> {
    if p.joins.contains(Ns::PROCESS_ID) {
        match pid_for.set(p.init.as_fd()) {
            Ok(()) => {}
            // `pidns_install` wants CAP_SYS_ADMIN in the seed's own user namespace: a rootless seed
            // hasn't it. The intermediate gains it by joining the container's user namespace first.
            Err(rustix::io::Errno::PERM) => return Ok(Single::Unable(Box::new(p), stdio)),
            Err(e) => {
                return Err((
                    Failed::Start,
                    OciError::os("choosing the container's pid namespace", e),
                ));
            }
        }
    } else {
        pid_for.reset().map_err(|e| (Failed::Seed, e))?;
    }
    let cloned = sys::clone_sibling(p.cgroup.as_ref().map(AsFd::as_fd), 0)
        .map_err(|e| (Failed::Start, e))?;
    match cloned {
        None => Ok(Single::Unable(Box::new(p), stdio)),
        Some(sys::Sibling::Child) => {
            // The program. It returns here only if it couldn't join, become the container's process
            // or exec; its status then leaves through its exit, which the agent's pidfd reads. Born in
            // the container's cgroup and pid namespace, it joins the rest, the cgroup namespace with
            // them.
            let Prepared {
                becoming,
                launch,
                cgroup,
                init,
                joins,
            } = p;
            drop(cgroup);
            let rest = joins.difference(Ns::PROCESS_ID);
            let [i, o, e] = &stdio;
            let (code, failed) = program(
                &becoming,
                &launch,
                [i.as_fd(), o.as_fd(), e.as_fd()],
                None,
                None,
                (init.as_fd(), rest),
            );
            let _ = writeln!(
                std::io::stderr().lock(),
                "hex oci exec: {}",
                OciError::from(failed)
            );
            sys::exit_child(code);
        }
        Some(sys::Sibling::Parent { pid, pidfd }) => match u32::try_from(pid) {
            Ok(pid) => Ok(Single::Started(pid, pidfd)),
            Err(_) => {
                // A clone never returns a negative pid; were it to, the program must not run
                // unwatched: end it (the agent's sweep reaps it, a zombie in a box cgroup it doesn't own).
                let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
                Err((
                    Failed::Start,
                    OciError::os("clone3 the program", "a pid out of range"),
                ))
            }
        },
    }
}

/// Starts the program through an intermediate (see the module's docs): forks it, lets it answer over
/// an internal socket, reaps it (so the program has reparented to the agent), and returns its answer.
fn through_intermediate(
    p: Prepared,
    stdio: [OwnedFd; 3],
    pid_for: &mut PidFor,
) -> (Answer, Option<OwnedFd>) {
    // The intermediate is born in the seed's own pid namespace; it chooses the container's itself.
    if let Err(e) = pid_for.reset() {
        return refused(Failed::Seed, e.to_string());
    }
    // The internal socket on which the intermediate answers the seed. The seed is not a child
    // subreaper (it never set the attribute), so the program reparents past it to the agent.
    let (seed_end, intermediate_end) = match socketpair() {
        Ok(pair) => pair,
        Err(e) => return refused(Failed::Seed, e.to_string()),
    };
    match sys::fork() {
        Err(e) => refused(Failed::Seed, e.to_string()),
        Ok(sys::Fork::Child) => {
            // The intermediate. It answers the seed (not the agent) and exits; the seed's `waitpid`
            // below turns that exit into the program's adoption by the agent before the agent is told.
            drop(seed_end);
            let (answer, pidfd) = match start_program(p, stdio) {
                Ok((pid, pidfd)) => (Answer::Started { pid }, Some(pidfd)),
                Err((failed, e)) => (
                    Answer::Refused {
                        failed,
                        why: e.to_string(),
                    },
                    None,
                ),
            };
            if let Err(e) = send_answer(
                intermediate_end.as_fd(),
                &answer,
                pidfd.as_ref().map(AsFd::as_fd),
            ) {
                let _ = writeln!(std::io::stderr().lock(), "hex __oci-seed: {e}");
            }
            let code = u8::from(!matches!(answer, Answer::Started { .. }));
            // `_exit`, not `std::process::exit`: a forked child must not run the seed's `atexit`
            // handlers or flush its buffers a second time ([`sys::exit_child`]).
            sys::exit_child(code);
        }
        Ok(sys::Fork::Parent(pid)) => {
            drop((intermediate_end, stdio, p));
            let answered = recv_answer(seed_end.as_fd());
            // Reap the intermediate, so the program has reparented to the agent before it is forwarded.
            let _ = sys::waitpid(pid);
            match answered {
                Ok((answer, pidfd)) => (answer, pidfd),
                Err(e) => refused(
                    Failed::Seed,
                    format!("the exec intermediate ended without answering: {e}"),
                ),
            }
        }
    }
}

/// A refusal the seed makes itself, with no descriptor.
fn refused(failed: Failed, why: String) -> (Answer, Option<OwnedFd>) {
    (Answer::Refused { failed, why }, None)
}

/// The intermediate: joins the container's namespaces by the init's pidfd (the user namespace first,
/// which grants it the rest; the cgroup namespace is left for the program, once it is in the cgroup) and
/// clones the program into them (the same work as [`super::exec`]'s helper), returning the program's
/// pid and the pidfd its clone made. Returns in the intermediate only; the program (the clone's child)
/// execs and never returns here.
fn start_program(p: Prepared, stdio: [OwnedFd; 3]) -> Result<(u32, OwnedFd), (Failed, OciError)> {
    let start = |e| (Failed::Start, e);
    let Prepared {
        becoming,
        launch,
        cgroup,
        init,
        joins,
    } = p;
    rustix::thread::move_into_thread_name_spaces(init.as_fd(), joins.difference(Ns::CONTROL_GROUP))
        .map_err(|e| start(OciError::os("joining the container's namespaces", e)))?;
    // A birth in the cgroup is checked against the credentials the clone runs with, which a joined
    // user namespace has changed; there the program joins the cgroup by migration instead (as the
    // shim does).
    let in_userns = joins.contains(Ns::USER);
    // Born in the cgroup with a pidfd where it can be; otherwise forked, to migrate in and have its
    // pidfd opened from its pid.
    let born = match &cgroup {
        Some(dir) if !in_userns => sys::fork_into_cgroup_pidfd(dir.as_fd()).map_err(start)?,
        _ => None,
    };
    // On the migrate path (rootless, or a kernel without clone-into-cgroup) the program joins the box
    // cgroup itself after the fork, so the intermediate must report it started only once it has — else a
    // delete that races the exec would `cgroup.kill` the box before the program is a member, miss it,
    // and the box would never empty (HEX-RT-7). A program born in the cgroup is a member already, so it
    // needs no such wait. The program signals by one byte on this pipe once it has joined.
    let migrating = born.is_none() && cgroup.is_some();
    let joined_pipe = if migrating {
        Some(
            rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
                .map_err(|e| start(OciError::os("the program's cgroup-join pipe", e)))?,
        )
    } else {
        None
    };
    let (fork, pidfd, procs) = match (born, cgroup) {
        (Some((fork, pidfd)), _) => (fork, pidfd, None),
        (None, cgroup) => {
            // The cgroup.procs the program joins by, opened by its directory before the fork.
            let procs = cgroup
                .map(|dir| {
                    rustix::fs::openat(
                        &dir,
                        "cgroup.procs",
                        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(|e| start(OciError::os("opening the container's cgroup", e)))
                })
                .transpose()?;
            (sys::fork().map_err(start)?, None, procs)
        }
    };
    match fork {
        sys::Fork::Child => {
            // The program. It returns here only if it couldn't become the container's process or
            // exec; its status then leaves through the process's exit, which the agent's pidfd reads.
            let joined = joined_pipe.map(|(_read, write)| write);
            let cgroup_ns = joins.intersection(Ns::CONTROL_GROUP);
            let [i, o, e] = &stdio;
            let (code, failed) = program(
                &becoming,
                &launch,
                [i.as_fd(), o.as_fd(), e.as_fd()],
                procs.as_ref().map(AsFd::as_fd),
                joined.as_ref().map(AsFd::as_fd),
                (init.as_fd(), cgroup_ns),
            );
            let _ = writeln!(
                std::io::stderr().lock(),
                "hex oci exec: {}",
                OciError::from(failed)
            );
            sys::exit_child(code);
        }
        sys::Fork::Parent(pid) => {
            drop((procs, stdio, init));
            // On the migrate path, wait for the program's one byte, meaning it has joined the box
            // cgroup; if the pipe closes with no byte, the program ended first, so reap it and refuse.
            if let Some((read, _write)) = joined_pipe {
                let mut byte = [0u8; 1];
                let joined = loop {
                    match rustix::io::read(&read, &mut byte) {
                        Ok(1) => break true,
                        Ok(_) => break false,
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(_) => break false,
                    }
                };
                if !joined {
                    let _ = sys::waitpid(pid);
                    return Err(start(OciError::os(
                        "starting the program",
                        "it ended before it joined the container's cgroup",
                    )));
                }
            }
            let pidfd = match pidfd {
                Some(pidfd) => pidfd,
                None => {
                    let raw = rustix::process::Pid::from_raw(pid).ok_or_else(|| {
                        start(OciError::os("clone", "the child's pid isn't a pid"))
                    })?;
                    rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty())
                        .map_err(|e| start(OciError::os("opening the program's pidfd", e)))?
                }
            };
            let pid = u32::try_from(pid)
                .map_err(|_| start(OciError::os("clone", "a pid out of range")))?;
            Ok((pid, pidfd))
        }
    }
}

/// The program (the clone's child): its stdio set to the ones the caller handed in, migrated into the
/// container's cgroup when it wasn't born there, then into the namespaces `joins` names of the init's
/// (the single clone's all but pid; the intermediate's program's the cgroup namespace alone, once it is
/// in the cgroup), then the container's process become and the program exec'd. System calls alone on
/// what was made before the clone, so it allocates nothing. Returns the status to exit with and what
/// failed only when a step did.
fn program(
    becoming: &init::Becoming,
    launch: &Launch,
    stdio: [BorrowedFd<'_>; 3],
    procs: Option<BorrowedFd<'_>>,
    joined: Option<BorrowedFd<'_>>,
    (init, joins): (BorrowedFd<'_>, Ns),
) -> (u8, Failure) {
    let setup = || -> Result<(), Failure> {
        let [stdin, stdout, stderr] = stdio;
        rustix::stdio::dup2_stdin(stdin).map_err(Failure::of("the program's stdin"))?;
        rustix::stdio::dup2_stdout(stdout).map_err(Failure::of("the program's stdout"))?;
        rustix::stdio::dup2_stderr(stderr).map_err(Failure::of("the program's stderr"))?;
        if let Some(procs) = procs {
            let n = rustix::io::write(procs, b"0")
                .map_err(Failure::of("joining the container's cgroup"))?;
            if n != 1 {
                return Err(Failure::of("joining the container's cgroup")(
                    rustix::io::Errno::IO,
                ));
            }
        }
        // Now a member of the box cgroup (born there, or just migrated in): tell the intermediate, so it
        // reports the program started only once the box's `cgroup.kill` would reach it. Before the
        // program's own setup, so the report can't beat the membership it stands for.
        if let Some(joined) = joined {
            let n = rustix::io::write(joined, b"1")
                .map_err(Failure::of("signalling the container's cgroup was joined"))?;
            if n != 1 {
                return Err(Failure::of("signalling the container's cgroup was joined")(
                    rustix::io::Errno::IO,
                ));
            }
        }
        if !joins.is_empty() {
            rustix::thread::move_into_thread_name_spaces(init, joins)
                .map_err(Failure::of("joining the container's namespaces"))?;
        }
        becoming.apply()
    };
    if let Err(failed) = setup() {
        return (CANNOT_RUN, failed);
    }
    launch.exec()
}

/// What [`recv_request`] read.
pub enum Received {
    /// The peer closed the socket.
    Closed,
    /// A request: its whole size (larger than the buffer when the message was cut), and the
    /// descriptors it carried.
    Message { size: usize, stdio: Vec<OwnedFd> },
}

/// Sends one exec request on `socket`, its descriptors riding with it (`SCM_RIGHTS`): the program's
/// stdin, stdout and stderr, then the container init's pidfd. The agent's end of the agent↔seed socket
/// (`hex-node`'s pool) sends this.
pub fn send_request(
    socket: BorrowedFd<'_>,
    req: &Request,
    fds: [BorrowedFd<'_>; seed::REQUEST_FDS],
) -> Result<(), OciError> {
    // A request past the wire's one message is a program no seed can be handed: refused as that.
    let bytes = seed::request(req).map_err(|f| match f {
        hex_core::wire::Fault::Oversized { .. } => OciError::Program(format!(
            "the exec request past the seed wire's one message: {f}"
        )),
        f => OciError::Broken(format!("encoding an exec request: {f}")),
    })?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::REQUEST_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(OciError::os(
            "sending an exec request's descriptors",
            "no room for the descriptors",
        ));
    }
    send(socket, &bytes, &mut control)
}

/// Sends one create on `socket`, the descriptors it carries riding with it (`SCM_RIGHTS`): the init's
/// sync pipe (write end) and map pipe (read end), then a network namespace made ahead when one is given.
/// The agent's end of the agent↔seed socket (`hex-node`'s create pool) sends this.
pub fn send_create(
    socket: BorrowedFd<'_>,
    create: &seed::Create,
    fds: &[BorrowedFd<'_>],
) -> Result<(), OciError> {
    if fds.len() != create.fds() {
        return Err(OciError::Broken(format!(
            "a create carries {} descriptors, not {}",
            create.fds(),
            fds.len()
        )));
    }
    let bytes =
        seed::create(create).map_err(|f| OciError::Broken(format!("encoding a create: {f}")))?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::MAX_REQUEST_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(OciError::os(
            "sending a create's descriptors",
            "no room for the descriptors",
        ));
    }
    send(socket, &bytes, &mut control)
}

/// Sends one park on `socket` (decision 8): the idle cgroup the init is cloned into, then the init's end of
/// its claim socket, riding with it (`SCM_RIGHTS`).
pub fn send_park(
    socket: BorrowedFd<'_>,
    park: &seed::Park,
    fds: [BorrowedFd<'_>; seed::PARK_FDS],
) -> Result<(), OciError> {
    let bytes = seed::park(park).map_err(|f| OciError::Broken(format!("encoding a park: {f}")))?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::PARK_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(OciError::os(
            "sending a park's descriptors",
            "no room for the descriptors",
        ));
    }
    send(socket, &bytes, &mut control)
}

/// Reads one exec request into `buf` with the descriptors that rode with it, or `Closed` at EOF.
pub fn recv_request(socket: BorrowedFd<'_>, buf: &mut [u8]) -> Result<Received, OciError> {
    loop {
        // Room for the most descriptors any request carries, a pruned create's (goal 5 plan step 2):
        // the two pipes, a network namespace, the four fixed and up to `MAX_BINDS` bind trees. One
        // carrying more sets `CTRUNC` and is refused by type below.
        let mut space =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::MAX_REQUEST_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let msg = match rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(buf)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC | RecvFlags::TRUNC,
        ) {
            Ok(msg) => msg,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(OciError::os("reading the seed's socket", e)),
        };
        let stdio = descriptors(&mut control);
        if msg.bytes == 0 && stdio.is_empty() {
            return Ok(Received::Closed);
        }
        // More descriptors than a request carries, or a message larger than the buffer: refused as a
        // bad request, not read as a good one (CLAUDE.md §3). The size is kept so the parser refuses
        // it by type.
        if msg.flags.contains(ReturnFlags::CTRUNC) {
            return Err(OciError::Broken(
                "an exec request carried more descriptors than it may".to_owned(),
            ));
        }
        return Ok(Received::Message {
            size: msg.bytes,
            stdio,
        });
    }
}

/// Sends one answer on `socket`, the program's pidfd riding with it when it started (`SCM_RIGHTS`).
pub fn send_answer(
    socket: BorrowedFd<'_>,
    answer: &Answer,
    pidfd: Option<BorrowedFd<'_>>,
) -> Result<(), OciError> {
    let bytes =
        seed::answer(answer).map_err(|f| OciError::Broken(format!("encoding an answer: {f}")))?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::STARTED_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds: Vec<BorrowedFd<'_>> = pidfd.into_iter().collect();
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(OciError::os(
            "sending the program's pidfd",
            "no room for the descriptor",
        ));
    }
    send(socket, &bytes, &mut control)
}

/// Reads one answer from `socket` and the pidfd that may ride with it (`Started` carries one). The
/// agent's end reading a seed, and the seed reading its intermediate.
pub fn recv_answer(socket: BorrowedFd<'_>) -> Result<(Answer, Option<OwnedFd>), OciError> {
    let mut buf = [0u8; seed::MAX_ANSWER];
    loop {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(seed::STARTED_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let msg = match rustix::net::recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC | RecvFlags::TRUNC,
        ) {
            Ok(msg) => msg,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(OciError::os("reading an answer", e)),
        };
        let mut fds = descriptors(&mut control);
        if msg.bytes == 0 && fds.is_empty() {
            return Err(OciError::Broken(
                "the peer closed before answering".to_owned(),
            ));
        }
        if msg.flags.contains(ReturnFlags::CTRUNC) {
            return Err(OciError::Broken(
                "an answer carried more descriptors than it may".to_owned(),
            ));
        }
        let bytes = buf.get(..msg.bytes.min(buf.len())).unwrap_or_default();
        let answer = seed::read_answer(bytes, msg.bytes, fds.len())
            .map_err(|f| OciError::Broken(format!("decoding an answer: {f}")))?;
        return Ok((answer, fds.drain(..).next()));
    }
}

/// The descriptors an ancillary buffer carried.
fn descriptors(control: &mut RecvAncillaryBuffer<'_>) -> Vec<OwnedFd> {
    control
        .drain()
        .filter_map(|m| match m {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Sends the node's pruned mount namespace on a new create seed's socket as its first record: one byte,
/// the namespace's descriptor riding with it (`SCM_RIGHTS`), which the seed joins ([`ENV_JOIN`]).
pub fn send_namespace(socket: BorrowedFd<'_>, namespace: BorrowedFd<'_>) -> Result<(), OciError> {
    let fds = [namespace];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(OciError::os(
            "sending the pruned namespace",
            "no room for its descriptor",
        ));
    }
    send(socket, &[1], &mut control)
}

/// Sends `bytes` as one record with `control`'s descriptors, retrying an interrupted call.
fn send(
    socket: BorrowedFd<'_>,
    bytes: &[u8],
    control: &mut SendAncillaryBuffer<'_, '_, '_>,
) -> Result<(), OciError> {
    loop {
        match rustix::net::sendmsg(socket, &[IoSlice::new(bytes)], control, SendFlags::NOSIGNAL) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(OciError::os("sending on the seed's socket", e)),
        }
    }
}

/// A socketpair of `SOCK_SEQPACKET`, as the agent makes to reach a seed: one end stays with the agent,
/// the other is given to the seed. Close-on-exec on both; the seed's is cleared before it is passed.
pub fn socketpair() -> Result<(OwnedFd, OwnedFd), OciError> {
    rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| OciError::os("making the seed's socket", e))
}
