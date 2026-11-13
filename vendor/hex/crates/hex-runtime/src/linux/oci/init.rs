//! The container init, reached by re-exec (`hex __oci-init`): the shim unshares the namespaces,
//! forks the container and exits, leaving the container to the caller's subreaper as crun's create
//! does (decision 2 of docs/design/runtime-architecture.md). The container sets up its root
//! filesystem, devices, paths, limits, capabilities and user, then blocks on the exec FIFO until
//! `start` and finally `execve`s the workload. Because the process re-exec'd, it is single-threaded
//! when it forks (see [`super::sys`]).
//!
//! It talks to the `create` runtime over two inherited pipes (fd numbers in the environment): a sync
//! pipe the init writes (`U`ser-ns map request, container `P`id, `H`ooks, `R`eady, or `E`rror), and a
//! map pipe the runtime writes its acknowledgement on, and, for the hooks, the container's state.

use std::fs;
use std::os::fd::{AsFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use hex_core::limits;
use rustix::fs::{Mode, OFlags};
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MountPropagationFlags, MoveMountFlags,
    OpenTreeFlags, UnmountFlags,
};
use rustix::thread::UnshareFlags;

use crate::linux::oci::inroot::{self, Make};
use crate::linux::oci::ipcuts::Pair;
use crate::linux::oci::spec::{Device, Process, Seccomp, Spec};
use crate::linux::oci::state::State;
use crate::linux::oci::{Failure, OciError, caps, hooks, net, seccomp, sys};

/// Environment the `create` runtime sets for `hex __oci-init`.
pub const ENV_SYNC_W: &str = "HEX_OCI_SYNC_W";
pub const ENV_MAP_R: &str = "HEX_OCI_MAP_R";
/// The container's own state directory (holds `state.json` and `exec.fifo`).
pub const ENV_DIR: &str = "HEX_OCI_DIR";
pub const ENV_BUNDLE: &str = "HEX_OCI_BUNDLE";
/// The container's cgroup directory, which `create` has made with its resources in force.
pub const ENV_CGROUP: &str = "HEX_OCI_CGROUP";
/// A network namespace made ahead, its loopback up (decision 4's pool), by the path of the runtime's
/// descriptor for it (`/proc/<pid>/fd/<n>`), which stays close-on-exec: the shim opens it, and joins
/// it in place of making the container's.
pub const ENV_NETWORK: &str = "HEX_OCI_NETWORK";
/// A `/dev` made ahead (the node's pool, goal 5 extended step 3), by the path of the runtime's descriptor for
/// the detached tmpfs (`/proc/<pid>/fd/<n>`), which the shim opens as it opens the network namespace, and its
/// mount's index in the spec's mounts: the container attaches it in place of making its own.
pub const ENV_DEV: &str = "HEX_OCI_DEV";
pub const ENV_DEV_MOUNT: &str = "HEX_OCI_DEV_MOUNT";
/// The spec the runtime parsed and checked, a sealed memfd (`checked::seal`), by the path of the runtime's
/// descriptor for it (`/proc/<pid>/fd/<n>`), which stays close-on-exec: the shim makes the container from it, never
/// from the bundle's `config.json`, which may since hold a spec nobody checked (AC-T7.1.1).
pub const ENV_CONFIG: &str = "HEX_OCI_CONFIG";
/// The node's one empty read-only tmpfs (decision 6's update), by the path of the runtime's descriptor for it,
/// which the shim opens as it opens the `/dev` made ahead: the container clones it for each masked directory in
/// place of a tmpfs of its own. Plumbing, as the entries above are, not a switch: the runtime sets it on the shim it
/// spawns to hand that descriptor down (`run.rs`, `start_shim`), and no caller sets it to choose a behaviour; unset,
/// the shim has no empty tmpfs handed to it and makes one per masked directory, as a rootless container must.
pub const ENV_MASKED_DIRECTORY: &str = "HEX_OCI_MASKED_DIRECTORY";

/// The default device nodes a runtime makes in `/dev` (OCI spec §Default Devices), as
/// (name in `/dev`, major, minor, mode); all character devices.
const DEFAULT_DEVICES: [(&str, u32, u32, u32); 6] = [
    ("null", 1, 3, 0o666),
    ("zero", 1, 5, 0o666),
    ("full", 1, 7, 0o666),
    ("random", 1, 8, 0o666),
    ("urandom", 1, 9, 0o666),
    ("tty", 5, 0, 0o666),
];

/// The links a runtime makes in `/dev` (OCI spec §Dev symbolic links), as (target, name in `/dev`):
/// the descriptors and the three streams through the container's own `/proc`, and, for a devpts
/// mounted at `/dev/pts`, its multiplexer.
const DEFAULT_LINKS: [(&str, &str); 5] = [
    ("/proc/self/fd", "fd"),
    ("/proc/self/fd/0", "stdin"),
    ("/proc/self/fd/1", "stdout"),
    ("/proc/self/fd/2", "stderr"),
    ("pts/ptmx", "ptmx"),
];

/// The `hex __oci-init` entry. Returns the process exit code; a setup failure is reported to the
/// runtime over the sync pipe before returning non-zero.
pub fn run() -> u8 {
    let sync = match handed_fd(ENV_SYNC_W, OFlags::WRONLY) {
        Ok(fd) => fd,
        Err(_) => return 125,
    };
    match shim(sync.as_fd()) {
        Ok(code) => code,
        Err(e) => {
            msg(sync.as_fd(), &format!("E{e}"));
            127
        }
    }
}

/// The shim: unshares namespaces, forks the container, reports its pid and exits. Nothing waits on the
/// container: `state` finds it stopped once its recorded init no longer runs. The shim stays outside
/// the container's cgroup, so the container's limits bound the container alone.
fn shim(sync: BorrowedFd<'_>) -> Result<u8, OciError> {
    let container_dir = PathBuf::from(env_var(ENV_DIR)?);
    let bundle = env_var(ENV_BUNDLE)?;
    let checked = handed_fd(ENV_CONFIG, OFlags::RDONLY)?;
    let spec = Spec::parse(&super::checked::read(checked.as_fd())?)?;
    drop(checked);
    // The cgroup, opened before any namespace changes: its directory, for the container to be born in
    // (`clone3` with `CLONE_INTO_CGROUP`), and its `cgroup.procs`, for it to join by migration where it
    // can't be. The kernel checks a migration against the credentials and cgroup namespace the file was
    // opened with (Linux 5.16, the fix for CVE-2021-4197), so the join works from inside a user
    // namespace too.
    let cgroup = std::env::var_os(ENV_CGROUP)
        .map(|dir| {
            let dir = Path::new(&dir);
            let open = |path: &Path, flags: OFlags| {
                rustix::fs::open(path, flags | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| OciError::os("opening the container's cgroup", e))
            };
            Ok::<_, OciError>((
                open(dir, OFlags::RDONLY | OFlags::DIRECTORY)?,
                open(&dir.join("cgroup.procs"), OFlags::WRONLY)?,
            ))
        })
        .transpose()?;
    // A birth in the cgroup is checked against the credentials the clone runs with, and in a user
    // namespace the shim's are no longer the caller's, so there the container joins by migration.
    let born_with_callers_credentials = !spec.linux.namespaces.iter().any(|n| n.kind == "user");
    // A network namespace made ahead (decision 4), opened by its path before any namespace changes,
    // while this process's /proc is the runtime's. It belongs to the runtime's user namespace, so it
    // serves only a container that makes its own network namespace there.
    let network = match std::env::var_os(ENV_NETWORK) {
        Some(path) => {
            if !spec.makes_namespace("network") || !born_with_callers_credentials {
                return Err(OciError::Config(
                    "a network namespace made ahead is for a container that makes its own network \
                     namespace and has no user namespace"
                        .to_owned(),
                ));
            }
            Some(
                rustix::fs::open(
                    Path::new(&path),
                    OFlags::RDONLY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| OciError::os("opening the network namespace made ahead", e))?,
            )
        }
        None => None,
    };
    // A `/dev` made ahead, opened by its path while this process's /proc is the runtime's, as the network
    // namespace is.
    let dev = prepared_dev()?;
    let masked_directory = match std::env::var_os(ENV_MASKED_DIRECTORY) {
        Some(path) => Some(
            rustix::fs::open(
                Path::new(&path),
                OFlags::PATH | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| OciError::os("opening the masked directories' empty tmpfs", e))?,
        ),
        None => None,
    };

    // The map pipe's end is read for a user namespace's maps, here, and for the hooks' state, by the
    // container; the shim closes it before the fork where the container has no hooks to stop for.
    let map = handed_fd(ENV_MAP_R, OFlags::RDONLY)?;
    if spec.makes_namespace("user") {
        sys::unshare(UnshareFlags::NEWUSER)?;
        msg(sync, &format!("U{}", std::process::id()));
        let mut ack = [0u8; 1];
        rustix::io::read(&map, &mut ack).map_err(|e| OciError::os("reading the map ack", e))?;
        become_userns_root()?;
    }
    let map = if spec.hooks.sync_init() {
        Some(map)
    } else {
        None
    };

    join_namespace_paths(&spec)?;
    // The network namespace made ahead is joined in place of a fresh one, and the shim's copy closed
    // before the fork, so the container holds it by being in it and by nothing else.
    let mut flags = namespace_flags(&spec);
    let made_ahead = match network {
        Some(network) => {
            sys::setns(network.as_fd(), libc::CLONE_NEWNET)?;
            flags.remove(UnshareFlags::NEWNET);
            true
        }
        None => false,
    };
    sys::unshare(flags)?;

    // Born in its cgroup where it can be: a migration takes the cgroup threadgroup lock, whose writer can
    // wait out an RCU grace period (docs/design/runtime.md). Otherwise forked, to join by migration.
    let born = match &cgroup {
        Some((dir, _)) if born_with_callers_credentials => sys::fork_into_cgroup(dir.as_fd())?,
        _ => None,
    };
    let (forked, procs) = match (born, cgroup) {
        (Some(forked), _) => (forked, None),
        (None, cgroup) => (sys::fork()?, cgroup.map(|(_, procs)| procs)),
    };
    match forked {
        sys::Fork::Child => Ok(
            match lead_session().and_then(|()| {
                container(
                    &spec,
                    &bundle,
                    FifoDir::Path(&container_dir),
                    sync,
                    procs,
                    made_ahead,
                    Root::Classic {
                        dev: dev.as_ref(),
                        masked_directory: masked_directory.as_ref(),
                        map,
                    },
                )
            }) {
                Ok(()) => 0,
                Err(e) => {
                    msg(sync, &format!("E{e}"));
                    127
                }
            },
        ),
        sys::Fork::Parent(container_pid) => {
            drop((procs, map));
            msg(sync, &format!("P{container_pid}"));
            Ok(0)
        }
    }
}

/// A create's clone child, born by [`super::sys::clone_sibling`] with the fresh namespaces its spec asks
/// for, in its cgroup, the agent's own child (decision 5, goal 5 plan step 4): what the shim's child does,
/// after the seed's one clone makes the namespaces the shim made by `unshare`. For a user namespace the
/// child is born unmapped, so it asks the runtime for its maps (`U`; the runtime knows this init's pid
/// from the seed's answer, so the message carries none), waits the ack on `map`, and becomes root in the
/// namespace, as the shim does before it forks. Then it joins the namespaces its spec names by `path`, in
/// the shim's order ([`join_namespace_paths`]): the network, ipc, uts, mount and cgroup kinds, which a
/// `setns` moves the caller itself into. A pid or user namespace by path can't be joined here — a pid
/// namespace's `setns` moves only the caller's later children, and the clone has already made the fresh
/// namespaces, owned by the seed's user namespace — so the runtime makes such a container through the shim
/// instead ([`needs_shim`]). A network namespace made ahead (`network`) is joined in place of the fresh
/// one the clone left out. Then the shared [`container`] setup, born in the cgroup (`procs` is `None`).
/// It leads a session of its own already ([`lead_session`]: a fresh clone's first act, a parked init's at park).
/// Returns only on the no-process case or an error (the caller reports `E`).
pub(crate) fn clone_child(
    spec: &Spec,
    bundle: &str,
    container_dir: &Path,
    sync: BorrowedFd<'_>,
    map: Option<OwnedFd>,
    network: Option<OwnedFd>,
) -> Result<(), OciError> {
    if spec.makes_namespace("user") {
        let map = map
            .as_ref()
            .ok_or_else(|| OciError::Broken("a user namespace needs the map pipe".to_owned()))?;
        msg(sync, "U");
        let mut ack = [0u8; 1];
        rustix::io::read(map, &mut ack).map_err(|e| OciError::os("reading the map ack", e))?;
        become_userns_root()?;
    }
    join_namespace_paths(spec)?;
    let made_ahead = match network {
        Some(network) => {
            sys::setns(network.as_fd(), libc::CLONE_NEWNET)?;
            true
        }
        None => false,
    };
    container(
        spec,
        bundle,
        FifoDir::Path(container_dir),
        sync,
        None,
        made_ahead,
        Root::Classic {
            dev: None,
            masked_directory: None,
            map: map.filter(|_| spec.hooks.sync_init()),
        },
    )
}

/// A create's clone child on the pruned-seed root path (goal 5 plan step 2): what [`clone_child`] does,
/// but its namespace is a copy of the seed's two-mount pruned namespace, so it builds the root from the
/// `sources` the agent cloned (the rootfs and one tree per bind mount) rather than from host paths, and
/// opens the exec FIFO through the state-directory descriptor `fifo_dir` the agent handed it. The pruned
/// path is only taken on a root node for a container that makes its own mount namespace and joins no user
/// or mount namespace by path (the runtime's routing), so no user-namespace map handshake runs here, and
/// a network, ipc, uts or cgroup namespace joined by path is joined by the descriptor the agent opened.
/// An ipc and uts pair made ahead (`ipc_uts`, goal 5 extended, step 4) is joined before all that lives in it: the
/// mqueue mount, the hostname and the sysctls [`container`] makes.
pub(crate) fn clone_child_pruned(
    spec: &Spec,
    bundle: &str,
    fifo_dir: OwnedFd,
    mut sources: PrunedSources,
    sync: BorrowedFd<'_>,
    network: Option<OwnedFd>,
    ipc_uts: Option<Pair>,
) -> Result<(), OciError> {
    // A namespace made from the rootfs's tree is joined first, before anything else the init does, so
    // nothing happens in the seed's namespace, which the init shares until then (cloned without
    // `CLONE_NEWNS`). `setns` takes no mount lock and sets the init's root and working directory to the
    // namespace's topmost mount, the rootfs (`goal6-mntns.md` §1), whose root it then holds.
    if sources.from_tree {
        sys::setns(sources.rootfs.as_fd(), libc::CLONE_NEWNS)?;
        sources.rootfs = rustix::fs::open(
            "/",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| OciError::os("opening the joined namespace's root", e))?;
    }
    // Join each network, ipc, uts or cgroup namespace the spec names by path, by the descriptor the
    // agent opened, in the spec's namespace order (the order the agent opened them). The pruned init
    // can't open the host path itself; a pid, user or mount namespace by path never reaches here.
    let mut joined = sources.path_joins.iter();
    for ns in &spec.linux.namespaces {
        let flag = match (ns.path.as_ref(), ns.kind.as_str()) {
            (Some(_), "network") => libc::CLONE_NEWNET,
            (Some(_), "ipc") => libc::CLONE_NEWIPC,
            (Some(_), "uts") => libc::CLONE_NEWUTS,
            (Some(_), "cgroup") => libc::CLONE_NEWCGROUP,
            _ => continue,
        };
        let fd = joined.next().ok_or_else(|| {
            OciError::Broken("a path-joined namespace has no descriptor from the agent".to_owned())
        })?;
        sys::setns(fd.as_fd(), flag)?;
    }
    if let Some(pair) = ipc_uts {
        pair.join()?;
        // Left for the exec to close, as the pipes are (close-on-exec): closed here, the init's copies are the
        // last, and the last close frees each namespace's file, dentry and inode on create's path.
        let _ = (pair.ipc.into_raw_fd(), pair.uts.into_raw_fd());
    }
    let made_ahead = match network {
        Some(network) => {
            sys::setns(network.as_fd(), libc::CLONE_NEWNET)?;
            true
        }
        None => false,
    };
    container(
        spec,
        bundle,
        FifoDir::Fd(fifo_dir),
        sync,
        None,
        made_ahead,
        Root::Pruned(&sources),
    )
}

/// Where the container's exec FIFO directory (its state directory) comes from. The shim and the
/// classic clone child open it by path, as they can; the pruned clone child can't (its namespace holds
/// no host path), so the agent hands it as a descriptor with the create (goal 5 plan step 2).
pub(crate) enum FifoDir<'a> {
    Path(&'a Path),
    Fd(OwnedFd),
}

impl FifoDir<'_> {
    fn open(self) -> Result<OwnedFd, OciError> {
        match self {
            FifoDir::Path(p) => rustix::fs::open(
                p,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| OciError::os("opening the container directory", e)),
            FifoDir::Fd(fd) => Ok(fd),
        }
    }
}

/// How the container's root is built. The classic path binds the rootfs in a copy of the agent's full
/// mount namespace and `pivot_root`s into it, with the `/dev` the node made ahead if the shim was handed
/// one, holding the map pipe's read end where the spec names create- or start-time hooks, for the
/// state they are given ([`create_hooks`]); the pruned path (goal 5 plan step 2) attaches the rootfs and bind sources the agent cloned (here)
/// in a copy of the seed's two-mount namespace and ends with `chroot`, with no `pivot_root` walk of the
/// host's threads.
pub(crate) enum Root<'a> {
    Classic {
        dev: Option<&'a super::run::PreparedDev>,
        /// The node's empty read-only tmpfs, cloned for each masked directory where the shim was handed it.
        masked_directory: Option<&'a OwnedFd>,
        /// The map pipe's read end, held where the spec names create- or start-time hooks.
        map: Option<OwnedFd>,
    },
    Pruned(&'a PrunedSources),
}

/// The `/dev` made ahead the runtime named to the shim ([`ENV_DEV`], [`ENV_DEV_MOUNT`]), opened by its
/// descriptor's path, or `None` where it named none. A name without its index, or one the shim can't open,
/// is refused: the create would otherwise make its own `/dev` and leave the runtime's unused.
fn prepared_dev() -> Result<Option<super::run::PreparedDev>, OciError> {
    let (path, index) = match (std::env::var_os(ENV_DEV), std::env::var_os(ENV_DEV_MOUNT)) {
        (None, None) => return Ok(None),
        (Some(path), Some(index)) => (path, index),
        _ => {
            return Err(OciError::Broken(
                "a /dev made ahead needs both its descriptor's path and its mount's index"
                    .to_owned(),
            ));
        }
    };
    let mount = index.to_str().and_then(|i| i.parse().ok()).ok_or_else(|| {
        OciError::Broken(format!(
            "the /dev made ahead's mount index {index:?} isn't one"
        ))
    })?;
    let tree = rustix::fs::open(
        Path::new(&path),
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the /dev made ahead", e))?;
    Ok(Some(super::run::PreparedDev { mount, tree }))
}

/// Makes the init the leader of a session of its own (and of its process group), as runc's and crun's inits do, so
/// nothing it or its program does reaches the agent's session or group. It is a fresh init's first act (the shim's
/// child's, and a seed's clone's, `seed::How::lead_session`): at Linux v6.19 every `setsid` moves the caller to a new
/// autogroup through `sched_move_task`, which reschedules a running task whether its group changed or not
/// (kernel/sched/autogroup.c:177-188, kernel/sched/core.c:9123-9128). Taken first, that pass through the scheduler
/// meets a task freshly placed by its clone; taken mid-build, it met one that had run its share, and under another
/// tenant's load it waited about a millisecond for the CPU back (docs/research/runtime/goal5-mounts.md §8). An init
/// parked ahead takes it at park, once it has said it is parked (`seed::parked`), so neither the create that claims
/// it nor the delete that parked it waits on it (goal 17, lever L1): taken first on a claim, the record's traced
/// rounds at 12 busy loops put it at a mean of 128.8 us of every create, p99 1,618 us, mostly preempted
/// (mntns-host-20261108T220817Z). Its allocations aren't charged to a memory cgroup (`GFP_KERNEL` without
/// `__GFP_ACCOUNT`, autogroup.c:89, fair.c `alloc_fair_sched_group`), so it may come before the shim's child joins its
/// cgroup.
pub(crate) fn lead_session() -> Result<(), OciError> {
    rustix::process::setsid()
        .map(|_| ())
        .map_err(|e| OciError::os("setsid", e))
}

/// The container (PID 1 of its namespaces): join its cgroup through `procs` (when it wasn't born there),
/// set up the world, signal ready, wait for `start`, exec. `network_made_ahead`: its network namespace
/// came from the node's pool, its loopback already up. `root` chooses how the root filesystem is built
/// (classic or the pruned seed's), `fifo` where the exec FIFO's directory comes from.
pub(crate) fn container(
    spec: &Spec,
    bundle: &str,
    fifo: FifoDir<'_>,
    sync: BorrowedFd<'_>,
    procs: Option<OwnedFd>,
    network_made_ahead: bool,
    root: Root<'_>,
) -> Result<(), OciError> {
    // First of all, before it allocates or starts anything: into its cgroup, so its limits and
    // accounting cover all it does. Then a fresh cgroup namespace, so the namespace's root is the
    // container's own cgroup (the shim no longer unshares it: it isn't in that cgroup).
    if let Some(procs) = procs {
        let n = rustix::io::write(&procs, b"0")
            .map_err(|e| OciError::os("joining the container's cgroup", e))?;
        if n != 1 {
            return Err(OciError::os(
                "joining the container's cgroup",
                "a short write to cgroup.procs",
            ));
        }
    }
    if spec.makes_namespace("cgroup") {
        sys::unshare(UnshareFlags::NEWCGROUP)?;
    }

    // The exec FIFO lives in the runtime's state dir, outside the rootfs; its directory is opened now,
    // before the root switch removes that path from view, and the FIFO opened through this fd later.
    let fifo_dir = fifo.open()?;

    // A network namespace of the container's own holds loopback, down until it's brought up, as
    // runc and crun bring it up; one made ahead had it brought up when it was made.
    if spec.makes_namespace("network") && !network_made_ahead {
        net::loopback_up()?;
    }
    // The create-time hooks run with the mounts made, before the root switch (config.md:607, :618); a
    // container with no mount namespace of its own has no switch, and runs them once its world is made.
    let mut hooked = None;
    match root {
        Root::Classic {
            dev,
            masked_directory,
            map,
        } => {
            let mut before_switch = || {
                hooked = create_hooks(spec, sync, map.as_ref())?;
                Ok(())
            };
            if spec.makes_namespace("mount") {
                setup_rootfs(spec, bundle, dev, masked_directory, &mut before_switch)?;
            } else {
                before_switch()?;
            }
        }
        // The runtime routes a spec with hooks classic; one that came here anyway is refused, as the
        // pruned init holds no map pipe.
        Root::Pruned(sources) => {
            hooked = create_hooks(spec, sync, None)?;
            if spec.makes_namespace("mount") {
                setup_rootfs_pruned(spec, sources)?;
            }
        }
    }
    if spec.makes_namespace("uts")
        && let Some(hostname) = &spec.hostname
    {
        rustix::system::sethostname(hostname.as_bytes())
            .map_err(|e| OciError::os("sethostname", e))?;
    }
    apply_sysctls(spec)?;
    // A container may have no process (OCI `process` is optional): it is set up and, once started,
    // simply has nothing to run, so it stops at once.
    let Some(process) = spec.process.clone() else {
        msg(sync, "R");
        let fifo = rustix::fs::openat(&fifo_dir, "exec.fifo", OFlags::WRONLY, Mode::empty())
            .map_err(|e| OciError::os("opening the exec fifo", e))?;
        start_hooks(spec, hooked.as_ref(), &fifo)?;
        let _ = rustix::io::write(&fifo, &[hooks::START_TAKEN]);
        return Ok(());
    };
    become_process(&process, spec.linux.seccomp.as_ref())?;

    // Ready: the runtime may now report `created`. Then block until `start` opens the FIFO.
    msg(sync, "R");
    let fifo = rustix::fs::openat(&fifo_dir, "exec.fifo", OFlags::WRONLY, Mode::empty())
        .map_err(|e| OciError::os("opening the exec fifo", e))?;
    start_hooks(spec, hooked.as_ref(), &fifo)?;
    if let Taken::Abandoned = take_start(&fifo)? {
        sys::exit_child(hooks::START_ABANDONED);
    }
    drop(fifo);

    exec(&process)
}

/// What the init's write of the start's byte on the exec FIFO found.
#[derive(Debug, PartialEq, Eq)]
enum Taken {
    /// The start has the byte.
    Taken,
    /// The start that opened the FIFO had ended: the write found its reader gone (EPIPE). The init, a
    /// `hex` process, has SIGPIPE ignored (Rust's runtime ignores it in every program it starts), so the
    /// write returns the error rather than the signal ending the init; it then ends with
    /// [`hooks::START_ABANDONED`].
    Abandoned,
}

/// Writes the start's byte on the exec FIFO `fifo`, or finds the start gone ([`Taken`]).
fn take_start(fifo: &OwnedFd) -> Result<Taken, OciError> {
    match rustix::io::write(fifo, &[hooks::START_TAKEN]) {
        Ok(_) => Ok(Taken::Taken),
        Err(rustix::io::Errno::PIPE) => Ok(Taken::Abandoned),
        Err(e) => Err(OciError::os("writing the exec fifo", e)),
    }
}

/// Stops for the runtime before the root switch, where the spec names create- or start-time hooks
/// (`H`): it runs prestart and createRuntime and hands back the container's state
/// ([`super::run`]'s `answer_hooks`), `created`. Then runs the createContainer hooks here, in the
/// container's namespaces, given that state with the pid the container sees (config.md:618-622;
/// runtime.md:27). Returns the state the startContainer hooks are given at `start`, the same; `None`
/// where the spec names no such hooks.
fn create_hooks(
    spec: &Spec,
    sync: BorrowedFd<'_>,
    map: Option<&OwnedFd>,
) -> Result<Option<State>, OciError> {
    if !spec.hooks.sync_init() {
        return Ok(None);
    }
    let map =
        map.ok_or_else(|| OciError::Broken("the create-time hooks need the map pipe".to_owned()))?;
    msg(sync, "H");
    let mut state = hooks::read_state(map.as_fd())?;
    state.pid = rustix::process::getpid().as_raw_pid();
    hooks::run("createContainer hook", &spec.hooks.create_container, &state)?;
    Ok(Some(state))
}

/// Runs the startContainer hooks once `start` has opened the exec FIFO (`fifo`), in the container
/// before its process (config.md:630-634), given `created` and the pid the container sees. They come
/// before the FIFO's byte, so `start` returns, and runs poststart, only after them (runtime.md:69-73);
/// one that fails writes why in its place ([`hooks::start_failure`]), for `start` to report, and the
/// init ends.
fn start_hooks(spec: &Spec, hooked: Option<&State>, fifo: &OwnedFd) -> Result<(), OciError> {
    if spec.hooks.start_container.is_empty() {
        return Ok(());
    }
    let state = hooked.ok_or_else(|| {
        OciError::Broken("the startContainer hooks were handed no state".to_owned())
    })?;
    hooks::run("startContainer hook", &spec.hooks.start_container, state).inspect_err(|e| {
        let _ = rustix::io::write(fifo, &hooks::start_failure(e));
    })
}

/// Becomes the container's process as its spec gives it: its OOM score, limits, AppArmor profile, seccomp
/// filter, capabilities, user, no_new_privs and umask. The container's first process does this before
/// `start`, and each one `hex oci exec` runs does it before it execs ([`Becoming`]).
pub(crate) fn become_process(process: &Process, seccomp: Option<&Seccomp>) -> Result<(), OciError> {
    Ok(Becoming::of(process, seccomp)?.apply()?)
}

/// The container's process, made ready to become before a clone (goal 3 plan step 2): its seccomp filter
/// compiled to the kernel's instructions, its limits, groups and capability sets resolved, its OOM score's
/// text made. [`Becoming::apply`] then makes system calls alone on what was made here, so the child of a
/// vfork-style clone, which shares its parent's memory and may not allocate, can become the process.
///
/// no_new_privs is exactly the spec's `noNewPrivileges`, so the seccomp filter goes on where that
/// allows, as runc puts it on (libcontainer/standard_init_linux.go). Without no_new_privs, loading a
/// filter needs CAP_SYS_ADMIN (seccomp(2)), so it goes on before the user and capabilities change,
/// and the profile also binds Hex's own calls from there to `execve`. With no_new_privs, it goes on
/// last, so as few of Hex's own calls as possible run under it.
pub(crate) struct Becoming {
    oom: Option<Vec<u8>>,
    rlimits: Vec<(rustix::process::Resource, rustix::process::Rlimit)>,
    filter: Option<Vec<libc::sock_filter>>,
    no_new_privileges: bool,
    caps: Option<caps::Ready>,
    uid: rustix::process::Uid,
    gid: rustix::process::Gid,
    groups: Vec<rustix::process::Gid>,
    umask: Option<Mode>,
    profile: Option<super::lsm::ProfileOnExec>,
}

impl Becoming {
    /// `process` made ready. A profile Hex can't honour is refused by name here, before anything about
    /// the process changes (seccomp::compile), never run unfiltered (CLAUDE.md §3).
    pub(crate) fn of(process: &Process, seccomp: Option<&Seccomp>) -> Result<Self, OciError> {
        let filter = seccomp.map(seccomp::compile).transpose()?.map(|program| {
            program
                .iter()
                .map(|f| libc::sock_filter {
                    code: f.code,
                    jt: f.jt,
                    jf: f.jf,
                    k: f.k,
                })
                .collect::<Vec<_>>()
        });
        if filter
            .as_ref()
            .is_some_and(|f| u16::try_from(f.len()).is_err())
        {
            return Err(OciError::Config(
                "the seccomp filter has more instructions than a filter holds".to_owned(),
            ));
        }
        let mut rlimits = process
            .rlimits
            .iter()
            .map(|limit| {
                Ok((
                    caps::rlimit_resource(&limit.kind)?,
                    rustix::process::Rlimit {
                        current: Some(limit.soft),
                        maximum: Some(limit.hard),
                    },
                ))
            })
            .collect::<Result<Vec<_>, OciError>>()?;
        // A spec that names no RLIMIT_NOFILE gets Docker 29's, not the node's own (HEX-INST-6).
        if !limits::names_nofile(process.rlimits.iter().map(|l| l.kind.as_str())) {
            rlimits.push((
                rustix::process::Resource::Nofile,
                crate::linux::default_nofile(),
            ));
        }
        Ok(Self {
            oom: process
                .oom_score_adj
                .map(|adj| adj.to_string().into_bytes()),
            rlimits,
            filter,
            no_new_privileges: process.no_new_privileges,
            caps: process.capabilities.as_ref().map(caps::ready),
            uid: rustix::process::Uid::from_raw(process.user.uid),
            gid: rustix::process::Gid::from_raw(process.user.gid),
            groups: process
                .user
                .additional_gids
                .iter()
                .map(|g| rustix::process::Gid::from_raw(*g))
                .collect(),
            umask: process.user.umask.map(Mode::from_bits_truncate),
            profile: process
                .apparmor_profile()
                .map(|p| super::lsm::ProfileOnExec::of(p, process.no_new_privileges))
                .transpose()?
                .flatten(),
        })
    }

    /// Becomes the process: system calls alone, in the order the type's docs give.
    pub(crate) fn apply(&self) -> Result<(), Failure> {
        // Before anything else, so before the seccomp filter a strict profile would refuse it under, as
        // crun marks them before its own (src/libcrun/container.c:1096).
        crate::linux::sys::mark_cloexec_from_3()
            .map_err(Failure::of("marking descriptors close-on-exec"))?;
        // oom_score_adj is set while still privileged (lowering it needs CAP_SYS_RESOURCE).
        if let Some(text) = &self.oom {
            let fd = rustix::fs::open(
                c"/proc/self/oom_score_adj",
                OFlags::WRONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(Failure::of("opening oom_score_adj"))?;
            rustix::io::write(&fd, text).map_err(Failure::of("writing oom_score_adj"))?;
        }
        for (resource, limit) in &self.rlimits {
            rustix::process::setrlimit(*resource, *limit).map_err(Failure::of("setrlimit"))?;
        }
        // The AppArmor profile, for the execve: before the uid changes, while the task still owns its /proc
        // files, and before a filter goes on, as crun orders it (src/libcrun/container.c:1455-1468).
        if let Some(profile) = &self.profile {
            profile.apply()?;
        }
        if !self.no_new_privileges
            && let Some(filter) = &self.filter
        {
            sys::load_seccomp_filter(filter).map_err(Failure::of("loading the seccomp filter"))?;
        }
        // Capabilities around the uid change (runc's order): drop the bounding set while still root
        // (needs effective CAP_SETPCAP), keep the permitted set across setuid, then set the sets exactly
        // as the dropped user, so a non-root container still holds the capabilities its spec grants.
        match &self.caps {
            Some(c) => {
                c.drop_bounding()?;
                sys::set_keepcaps(true).map_err(Failure::of("PR_SET_KEEPCAPS"))?;
                self.set_user()?;
                c.set_process()?;
                sys::set_keepcaps(false).map_err(Failure::of("PR_SET_KEEPCAPS"))?;
            }
            None => self.set_user()?,
        }
        if self.no_new_privileges {
            rustix::thread::set_no_new_privs(true).map_err(Failure::of("set_no_new_privs"))?;
        }
        if let Some(mask) = self.umask {
            rustix::process::umask(mask);
        }
        // With no_new_privs now set, the filter needs no privilege.
        if self.no_new_privileges
            && let Some(filter) = &self.filter
        {
            sys::load_seccomp_filter(filter).map_err(Failure::of("loading the seccomp filter"))?;
        }
        Ok(())
    }

    fn set_user(&self) -> Result<(), Failure> {
        // A user namespace with setgroups denied (the rootless one) refuses this, and the process keeps
        // the groups it has: none beyond its gid there.
        let _ = rustix::thread::set_thread_groups(&self.groups);
        rustix::thread::set_thread_res_gid(self.gid, self.gid, self.gid)
            .map_err(Failure::of("setgid"))?;
        rustix::thread::set_thread_res_uid(self.uid, self.uid, self.uid)
            .map_err(Failure::of("setuid"))?;
        Ok(())
    }
}

/// `execve`s the workload, resolving `args[0]` in `PATH` when it has no slash. Only returns on error.
pub(crate) fn exec(process: &Process) -> Result<(), OciError> {
    let prog = process
        .args
        .first()
        .ok_or_else(|| OciError::Config("process.args is empty".to_owned()))?;
    let path = resolve_program(prog, &process.env)?;
    let mut cmd = Command::new(&path);
    cmd.arg0(prog);
    cmd.args(process.args.get(1..).unwrap_or_default());
    cmd.current_dir(&process.cwd);
    cmd.env_clear();
    for entry in &process.env {
        if let Some((k, v)) = entry.split_once('=') {
            cmd.env(k, v);
        }
    }
    // exec() returns only if it failed.
    Err(OciError::os("execve", cmd.exec()))
}

/// Resolves a program name to a path: used as-is when it has a slash, else searched in `PATH`.
pub(crate) fn resolve_program(prog: &str, env: &[String]) -> Result<PathBuf, OciError> {
    if prog.contains('/') {
        return Ok(PathBuf::from(prog));
    }
    let path = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(prog);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(OciError::Config(format!(
        "program {prog:?} not found in PATH"
    )))
}

/// Becomes uid/gid 0 within a fresh user namespace (after the runtime wrote the maps).
fn become_userns_root() -> Result<(), OciError> {
    let zero_g = rustix::process::Gid::from_raw(0);
    let zero_u = rustix::process::Uid::from_raw(0);
    let _ = rustix::thread::set_thread_groups(&[]);
    rustix::thread::set_thread_res_gid(zero_g, zero_g, zero_g)
        .map_err(|e| OciError::os("setresgid in userns", e))?;
    rustix::thread::set_thread_res_uid(zero_u, zero_u, zero_u)
        .map_err(|e| OciError::os("setresuid in userns", e))?;
    Ok(())
}

/// The `CLONE_NEW*` flag for a namespace kind.
fn ns_clone_flag(kind: &str) -> Option<i32> {
    let f = match kind {
        "mount" => libc::CLONE_NEWNS,
        "uts" => libc::CLONE_NEWUTS,
        "ipc" => libc::CLONE_NEWIPC,
        "network" => libc::CLONE_NEWNET,
        "cgroup" => libc::CLONE_NEWCGROUP,
        "pid" => libc::CLONE_NEWPID,
        "user" => libc::CLONE_NEWUSER,
        _ => return None,
    };
    Some(f)
}

/// Joins each namespace the spec gives by `path` (as opposed to making a fresh one), checking each
/// is of the named type. User and pid are joined with an eye to order: user first (its privileges
/// gate the rest), pid last (it governs the fork that follows).
fn join_namespace_paths(spec: &Spec) -> Result<(), OciError> {
    let order = |kind: &str| match kind {
        "user" => 0,
        "pid" => 2,
        _ => 1,
    };
    let mut with_path: Vec<&crate::linux::oci::spec::Namespace> = spec
        .linux
        .namespaces
        .iter()
        .filter(|n| n.path.is_some())
        .collect();
    with_path.sort_by_key(|n| order(&n.kind));
    for ns in with_path {
        let Some(path) = &ns.path else { continue };
        let flag = ns_clone_flag(&ns.kind)
            .ok_or_else(|| OciError::Config(format!("unknown namespace type {:?}", ns.kind)))?;
        let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| OciError::os("opening a namespace path", e))?;
        sys::setns(fd.as_fd(), flag)?;
    }
    Ok(())
}

/// Whether a container can't be made by a seed's one clone and is made through the shim instead: one
/// that joins a pid or a user namespace by `path` ([`clone_child`] says why). The node counts each.
pub fn needs_shim(spec: &Spec) -> bool {
    spec.linux
        .namespaces
        .iter()
        .any(|n| n.path.is_some() && matches!(n.kind.as_str(), "pid" | "user"))
}

/// The `CLONE_NEW*` flags a create's one clone makes fresh for the container (decision 5): each
/// namespace the spec makes (not one joined by `path`, which [`clone_child`] joins), as `clone3` flags,
/// but the cgroup namespace, which [`container`] unshares once the child is in its cgroup, and the
/// network namespace when one is made ahead (`made_ahead`), which [`clone_child`] joins by `setns` in
/// its place, and the ipc and uts namespaces when a pair is made ahead (`ipc_uts_ahead`), which
/// [`clone_child_pruned`] joins in theirs.
pub(crate) fn fresh_clone_flags(spec: &Spec, made_ahead: bool, ipc_uts_ahead: bool) -> u64 {
    let mut flags: u64 = 0;
    for (kind, flag, ahead) in [
        ("mount", libc::CLONE_NEWNS, false),
        ("uts", libc::CLONE_NEWUTS, ipc_uts_ahead),
        ("ipc", libc::CLONE_NEWIPC, ipc_uts_ahead),
        ("pid", libc::CLONE_NEWPID, false),
        ("user", libc::CLONE_NEWUSER, false),
    ] {
        if spec.makes_namespace(kind) && !ahead {
            flags |= u64::try_from(flag).unwrap_or(0);
        }
    }
    if spec.makes_namespace("network") && !made_ahead {
        flags |= u64::try_from(libc::CLONE_NEWNET).unwrap_or(0);
    }
    flags
}

/// The namespace flags the shim makes fresh for the container (those without a `path`). A fresh
/// cgroup namespace isn't among them: the container makes it once it has joined its own cgroup.
fn namespace_flags(spec: &Spec) -> UnshareFlags {
    let mut flags = UnshareFlags::empty();
    for (kind, flag) in [
        ("mount", UnshareFlags::NEWNS),
        ("uts", UnshareFlags::NEWUTS),
        ("ipc", UnshareFlags::NEWIPC),
        ("network", UnshareFlags::NEWNET),
        ("pid", UnshareFlags::NEWPID),
    ] {
        if spec.makes_namespace(kind) {
            flags |= flag;
        }
    }
    flags
}

/// Prepares the container's root: private propagation, the spec mounts, default devices and
/// symlinks, masked and readonly paths, then `pivot_root` into it. Every path in the root is resolved
/// inside it ([`inroot`]), from one descriptor of the bound rootfs: until the pivot `/` is the host's,
/// and an image's links may point there. A `/dev` the node made ahead (`dev`, goal 5 extended step 3) is
/// attached in place of the spec's, under the pruned path's guards ([`setup_rootfs_pruned`]): only where
/// `/dev` is reached without a link, its defaults taken out again before a later mount that could reach
/// them, so every mount and refusal is what making `/dev` here gives.
fn setup_rootfs(
    spec: &Spec,
    bundle: &str,
    dev: Option<&super::run::PreparedDev>,
    masked_directory: Option<&OwnedFd>,
    before_switch: &mut dyn FnMut() -> Result<(), OciError>,
) -> Result<(), OciError> {
    let rootfs = {
        let p = Path::new(&spec.root.path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(bundle).join(p)
        }
    };
    // In a user namespace mknod is refused (CAP_MKNOD has no effect across the userns boundary), so
    // devices are bind-mounted from the host instead of created, and a `/dev` made in the node's own
    // namespace can't stand in: the runtime never hands one there ([`super::run`], `shim_dev`).
    let in_userns = spec.in_user_namespace();
    let plan = crate::linux::oci::mount::plan(&spec.mounts)?;
    if let Some(dev) = dev
        && (in_userns || !super::devmount::eligible(spec, &plan).is_ok_and(|(i, _)| i == dev.mount))
    {
        return Err(OciError::Broken(
            "the prepared /dev does not match the mount plan".to_owned(),
        ));
    }
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| OciError::os("making mounts private", e))?;
    rustix::mount::mount_bind(&rootfs, &rootfs)
        .map_err(|e| OciError::os("binding the rootfs", e))?;
    // Opened after the bind, so it is the bind's root, which the pivot makes `/`. A spec mount at `/`
    // itself (a layered root's overlay) goes over the bind, and what follows resolves in it, so the
    // root is opened again after one, at its top then, as a lookup of the rootfs's path would land.
    let mut root_fd = inroot::open_root(&rootfs)?;
    let mut ready = false;
    for (index, op) in plan.iter().enumerate() {
        let root = inroot::Root::new(root_fd.as_fd());
        if ready
            && (!super::devmount::target_unaffected(&op.destination)
                || !root.without_symlinks(&op.destination))
        {
            if let Some(dev) = dev {
                clear_prepared_dev(&dev.tree)?;
            }
            ready = false;
        }
        let prepared = dev
            .filter(|d| usize::try_from(d.mount).ok() == Some(index))
            .filter(|_| root.without_symlinks(&op.destination));
        match prepared {
            Some(d) => attach_prepared_dev(&root, op, &d.tree)?,
            None => do_mount(&root, op, spec.mount_label())?,
        }
        ready |= prepared.is_some();
        if inroot::is_root(&op.destination) {
            root_fd = inroot::open_root(&rootfs)?;
        }
    }
    let root = inroot::Root::new(root_fd.as_fd());
    if !ready {
        make_dev(&root, spec, in_userns)?;
    }
    // The mounts and /dev made, the masks and read-only remounts not yet, as runc runs them
    // (rootfs_linux.go:205-232; docs/research/runtime/oci-hooks.md §2): a createContainer hook can still
    // write the root and reach the host's paths.
    before_switch()?;
    for path in &spec.linux.masked_paths {
        mask_path(&root, path, masked_directory, spec.mount_label())?;
    }
    for path in &spec.linux.readonly_paths {
        readonly_path(&root, path)?;
    }
    if spec.root.readonly {
        rustix::mount::mount_remount(&rootfs, MountFlags::RDONLY | MountFlags::BIND, "")
            .map_err(|e| OciError::os("remounting rootfs ro", e))?;
    }

    rustix::process::fchdir(&root_fd).map_err(|e| OciError::os("chdir rootfs", e))?;
    rustix::process::pivot_root(c".", c".").map_err(|e| OciError::os("pivot_root", e))?;
    rustix::mount::unmount(".", UnmountFlags::DETACH)
        .map_err(|e| OciError::os("detaching the old root", e))?;
    rustix::process::chdir("/").map_err(|e| OciError::os("chdir /", e))?;
    // The rootfs propagation the spec asks for (default: left private, as set above).
    if let Some(prop) = spec
        .linux
        .rootfs_propagation
        .as_deref()
        .and_then(propagation)
    {
        rustix::mount::mount_change("/", prop)
            .map_err(|e| OciError::os("setting rootfs propagation", e))?;
    }
    Ok(())
}

/// The sources the agent cloned in its own mount namespace for a pruned create's root (goal 5 plan
/// step 2): the rootfs and one detached tree per bind mount, in the order the spec lists its bind
/// mounts. The clone child attaches them with `move_mount`, so nothing of the host is read by path.
pub(crate) struct PrunedSources {
    /// The rootfs's detached tree; or, where `from_tree`, the mount namespace the agent made from it,
    /// which the init joins and then holds the root of here.
    pub rootfs: OwnedFd,
    /// Whether the agent made the container's mount namespace from the rootfs's tree (decision 6's
    /// update, `OPEN_TREE_NAMESPACE`): the init joins it by `setns`, its root already the container's.
    pub from_tree: bool,
    pub dev_mount: Option<super::run::PreparedDev>,
    pub binds: Vec<OwnedFd>,
    /// One descriptor per network, ipc, uts or cgroup namespace the spec joins by path, in the spec's
    /// namespace order: the agent opened them (the pruned init can't reach the host path), and the init
    /// joins each by `setns`, deriving its kind from the spec in the same order.
    pub path_joins: Vec<OwnedFd>,
    /// A proc made ahead and its index in the mount plan (decision 6's update, part 2): the init was
    /// cloned into its pid namespace, attaches it whole, and makes none of the masks and read-only paths
    /// beneath `/proc`, which it carries.
    pub proc_mount: Option<(u32, PreparedProc)>,
    /// The node's empty read-only tmpfs, cloned for each masked directory; `None` where none rode.
    pub masked_directory: Option<OwnedFd>,
}

/// Masks the directory `target` with a clone of the node's one empty read-only tmpfs `empty`, as crun binds one
/// empty directory for every masked directory (1.30.1 `src/libcrun/linux.c:1181-1210`, `:1266`): a clone makes
/// no superblock, so none takes `list_lrus_mutex` or `shrinker_mutex` (step 6, `create_serial.md` runs 23-24).
/// A fresh tmpfs's mount has no peer group, nor has its clone.
fn clone_masked_directory(empty: BorrowedFd<'_>, target: BorrowedFd<'_>) -> rustix::io::Result<()> {
    let clone = rustix::mount::open_tree(
        empty,
        "",
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_EMPTY_PATH
            | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )?;
    rustix::mount::move_mount(clone.as_fd(), "", target, "", FD_TO_FD)
}

/// The propagation a pruned create's rootfs takes before anything is mounted beneath it, so it leaves its
/// source's peer group (`goal5-root.md` §9): slave where the spec asks for a slave or shared root (a slave
/// keeps incoming host events; a final shared root gets a new peer group afterward), private otherwise.
pub(crate) fn isolating_propagation(spec: &Spec) -> u64 {
    match spec.linux.rootfs_propagation.as_deref() {
        Some("slave" | "shared") => sys::MS_SLAVE,
        _ => sys::MS_PRIVATE,
    }
}

/// `move_mount` from one descriptor to another, each named by the descriptor itself (empty paths).
const FD_TO_FD: MoveMountFlags =
    MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH.union(MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH);

/// Builds the container's root on the pruned-seed path (goal 5 plan step 2), fd-based. The init's
/// namespace is a copy of the seed's two mounts — its root and a read-only tmpfs on it — so it attaches
/// the rootfs the agent cloned on `/`, over that tmpfs, makes the spec's mounts in it (each bind from
/// the tree the agent cloned, by `move_mount`; each filesystem by `fsopen`/`fsmount`), its devices,
/// symlinks, masks and read-only paths, and ends with `chroot` into it, not `pivot_root`: no walk of the
/// host's threads (f6, f75, f80) and a copied table of two mounts (f9, f76). Every target is resolved
/// inside the rootfs with `openat2(RESOLVE_IN_ROOT)` from the rootfs tree's own descriptor ([`inroot`]),
/// as the classic path resolves inside the bound rootfs, and the mount is attached to that descriptor,
/// there being no `/proc/self/fd` to name it by path in this namespace. Attached on `/` it lies where
/// step 2's `MS_MOVE` from `/r` put it, the same tree, with one attach and one hold of the kernel's
/// `namespace_sem` fewer (goal 5 plan step 3, `goal5-mounts.md`); until the `chroot` the init's own root
/// is still the tmpfs's, and it walks no path by name. This is safe here, where it is not in a copy of
/// the host's namespace, because nothing of the host lies beneath the root (`goal5-root.md` §3): the
/// escapes it opens there (E2, E3, a user-namespaced root) are closed.
fn setup_rootfs_pruned(spec: &Spec, sources: &PrunedSources) -> Result<(), OciError> {
    let plan = crate::linux::oci::mount::plan(&spec.mounts)?;
    if sources.from_tree {
        // The namespace's tree is a copy of the rootfs's source, which keeps a shared source's peer
        // group (`goal6-mntns.md` §1): it leaves the host's before anything is mounted beneath, as the
        // agent sets the pruned path's clone (`run.rs`, `pruned_create`). A layered root's overlay is a
        // fresh filesystem with no peer group, so it needs none.
        if !plan.iter().any(|op| op.destination == "/" && !op.is_bind) {
            sys::mount_setattr(
                sources.rootfs.as_fd(),
                true,
                &sys::MountAttrs {
                    set: 0,
                    clr: 0,
                    propagation: isolating_propagation(spec),
                },
            )
            .map_err(|e| OciError::os("isolating the rootfs's propagation", e))?;
        }
    } else {
        rustix::mount::move_mount(
            sources.rootfs.as_fd(),
            "",
            rustix::fs::CWD,
            "/",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .map_err(|e| OciError::os("attaching the rootfs", e))?;
    }
    let root = inroot::Root::new(sources.rootfs.as_fd());

    if let Some(dev) = &sources.dev_mount
        && !super::devmount::eligible(spec, &plan).is_ok_and(|(i, _)| i == dev.mount)
    {
        return Err(OciError::Broken(
            "the prepared /dev does not match the mount plan".to_owned(),
        ));
    }
    let mut binds = sources.binds.iter();
    let mut ready = false;
    for (index, op) in plan.iter().enumerate() {
        // The mount at `/` is the rootfs itself (a layered root's overlay, with host-path layers the
        // agent built as the rootfs tree; a directory root has none). It is already attached above, so
        // it is skipped here, as the agent skipped it when it cloned the bind sources.
        if op.destination == "/" {
            continue;
        }
        if ready
            && (!super::devmount::target_unaffected(&op.destination)
                || !root.without_symlinks(&op.destination))
        {
            if let Some(dev) = &sources.dev_mount {
                clear_prepared_dev(&dev.tree)?;
            }
            ready = false;
        }
        let prepared = sources
            .dev_mount
            .as_ref()
            .filter(|d| usize::try_from(d.mount).ok() == Some(index))
            .filter(|_| root.without_symlinks(&op.destination))
            .map(|d| &d.tree);
        ready |= prepared.is_some();
        let proc = sources
            .proc_mount
            .as_ref()
            .filter(|(mount, _)| usize::try_from(*mount).ok() == Some(index))
            .map(|(_, p)| &p.tree);
        do_mount_pruned(&root, op, &mut binds, prepared.or(proc), spec.mount_label())?;
    }
    if binds.next().is_some() {
        return Err(OciError::Broken(
            "the create carried more bind-source trees than the spec has bind mounts".to_owned(),
        ));
    }
    // A pruned create is a root node's, never a user namespace's (the runtime routes those classic), so
    // devices are made with `mknod`, as the classic path makes them outside a user namespace.
    if !ready {
        make_dev(&root, spec, false)?;
    }
    // A prepared proc carries the masks and read-only paths beneath `/proc` already.
    let carried =
        |path: &str| sources.proc_mount.is_some() && super::procmount::beneath_proc(path).is_some();
    let mut null = None;
    for path in spec.linux.masked_paths.iter().filter(|p| !carried(p)) {
        mask_path_pruned(
            &root,
            path,
            &mut null,
            sources.masked_directory.as_ref(),
            spec.mount_label(),
        )?;
    }
    for path in spec.linux.readonly_paths.iter().filter(|p| !carried(p)) {
        readonly_path_pruned(&root, path)?;
    }
    if spec.root.readonly {
        // The rootfs mount alone read-only (not its submounts): not recursive, as the classic path's
        // remount changes only the bind's top.
        sys::mount_setattr(
            sources.rootfs.as_fd(),
            false,
            &sys::MountAttrs {
                set: sys::MOUNT_ATTR_RDONLY,
                clr: 0,
                propagation: 0,
            },
        )
        .map_err(|e| OciError::os("remounting the rootfs read-only", e))?;
    }

    // A debug-only test crash here, between the mounts' attach and the switch (`seed::ENV_INJECT`): the
    // init ends as a kill would, its mounts still in its own namespace, so a test proves the agent leaves
    // no container, clone or mount of it in its own (CLAUDE.md §6, crash injection at every write). Absent
    // from release builds, where the init never looks for it.
    #[cfg(debug_assertions)]
    if std::env::var(crate::linux::oci::seed::ENV_INJECT).as_deref()
        == Ok(crate::linux::oci::seed::INJECT_DIE_BEFORE_SWITCH)
    {
        sys::exit_child(70);
    }

    // The root switch, with no pivot_root: into the rootfs, then chroot there. A joined namespace's
    // `setns` made the rootfs the init's root already.
    if !sources.from_tree {
        rustix::process::fchdir(&sources.rootfs).map_err(|e| OciError::os("chdir rootfs", e))?;
        rustix::process::chroot(".").map_err(|e| OciError::os("chroot into the root", e))?;
    }
    rustix::process::chdir("/").map_err(|e| OciError::os("chdir /", e))?;
    if let Some(prop) = spec
        .linux
        .rootfs_propagation
        .as_deref()
        .and_then(propagation)
    {
        rustix::mount::mount_change("/", prop)
            .map_err(|e| OciError::os("setting rootfs propagation", e))?;
    }
    Ok(())
}

/// One of the spec's mounts on the pruned path: a bind from the next tree the agent cloned (its flags
/// set on the tree, then attached by `move_mount`), or a filesystem made with `fsopen`/`fsmount` and
/// attached the same way. The destination is resolved inside the rootfs ([`target`]), as the classic
/// [`do_mount`] resolves it, and the mount attached to that descriptor (no `/proc/self/fd` here). A
/// propagation the spec asks for is set on the attached mount through its own descriptor, which names
/// the mount now on top of the destination.
fn do_mount_pruned<'a>(
    root: &inroot::Root<'_>,
    op: &crate::linux::oci::mount::Op,
    binds: &mut impl Iterator<Item = &'a OwnedFd>,
    prepared: Option<&OwnedFd>,
    label: Option<&str>,
) -> Result<(), OciError> {
    let at = || format!("{} at {}", op.kind_source, op.destination);
    let fs;
    let attached = if op.is_bind {
        let tree = binds.next().ok_or_else(|| {
            OciError::Broken(format!("the bind at {} has no source tree", op.destination))
        })?;
        // A missing destination is made as what the source is: a file for a file's bind.
        let target_fd = match root.open(&op.destination, Make::Nothing)? {
            Some(fd) => fd,
            None if is_regular_file(tree.as_fd())? => target(root, &op.destination, Make::File)?,
            None => target(root, &op.destination, Make::Dir)?,
        };
        // A cloned bind must leave every host peer group before anything can be mounted beneath
        // it. Slave first preserves incoming propagation for explicit shared/slave; shared is made
        // a new peer group below. Other binds are recursively private, as the classic root's are.
        let propagation = if op.propagation.is_some_and(|p| {
            p.intersects(MountPropagationFlags::SHARED | MountPropagationFlags::DOWNSTREAM)
        }) {
            sys::MS_SLAVE
        } else {
            sys::MS_PRIVATE
        };
        let set = attr_set(op.flags);
        sys::mount_setattr(
            tree.as_fd(),
            true,
            &sys::MountAttrs {
                set: if op.recursive_bind { set } else { 0 },
                clr: 0,
                propagation,
            },
        )
        .map_err(|e| OciError::os("setting a bind's attributes", format!("{}: {e}", at())))?;
        // Privacy is always recursive; a nonrecursive bind's attribute request is not. Keep that
        // scope even when the cloned source carries submounts, rather than widening its flags.
        if !op.recursive_bind && set != 0 {
            sys::mount_setattr(
                tree.as_fd(),
                false,
                &sys::MountAttrs {
                    set,
                    clr: 0,
                    propagation: 0,
                },
            )
            .map_err(|e| OciError::os("setting a bind's attributes", format!("{}: {e}", at())))?;
        }
        rustix::mount::move_mount(tree.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
            .map_err(|e| OciError::os("attaching a bind", format!("{}: {e}", at())))?;
        tree.as_fd()
    } else {
        let target_fd = target(root, &op.destination, Make::Dir)?;
        let filesystem = match prepared {
            Some(tree) => tree,
            None => {
                fs = make_fs(&op.kind_source, &op.fstype, &op.data, op.flags, label).map_err(
                    |e| {
                        OciError::os(
                            "making a filesystem",
                            format!("{} at {}: {e}", op.fstype, op.destination),
                        )
                    },
                )?;
                &fs
            }
        };
        rustix::mount::move_mount(filesystem.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
            .map_err(|e| {
                OciError::os(
                    "attaching a filesystem",
                    format!("{} at {}: {e}", op.fstype, op.destination),
                )
            })?;
        filesystem.as_fd()
    };
    if let Some(prop) = op.propagation {
        let (ms, rec) = propagation_ms(prop);
        // Bind privacy/slavery was applied recursively before attachment, with its attributes.
        // A requested shared group or unbindable state still needs the final transition.
        if op.is_bind && matches!(ms, sys::MS_PRIVATE | sys::MS_SLAVE) {
            return Ok(());
        }
        sys::mount_setattr(
            attached,
            rec,
            &sys::MountAttrs {
                set: 0,
                clr: 0,
                propagation: ms,
            },
        )
        .map_err(|e| OciError::os("setting a mount's propagation", format!("{}: {e}", at())))?;
    }
    Ok(())
}

/// The data of the empty read-only tmpfs a masked directory gets: one block, a page on any page size
/// (`nr_blocks`, Documentation/filesystems/tmpfs.rst:82), the least bound tmpfs takes, for 0 is none at all
/// (:94). Read-only and empty, it never holds a block; a bound costs the create nothing, the superblock's
/// block counter being made whatever its bound (mm/shmem.c, `shmem_fill_super`).
pub(crate) const EMPTY_TMPFS: &str = "nr_blocks=1";

/// A filesystem made with the new mount API (`fsopen`/`fsconfig`/`fsmount`), its source, data options and
/// flags the spec's: a detached mount descriptor the caller attaches with `move_mount`. The pruned path makes proc, sysfs, tmpfs, mqueue,
/// devpts and cgroup2 this way, since it has no path to mount onto through `/proc/self/fd`; the agent
/// ([`super::run`]) makes a layered root's overlay this way too, where it can reach the layers' host
/// paths that the pruned seed can't, and hands the init the result as the rootfs. The source is set as
/// `mount(2)` sets it, the context's `source` parameter (v6.19 `fs/namespace.c:3706`), which every
/// filesystem takes (`fs/fs_context.c:151-156`): else the mount's source reads `none` where the spec
/// named one. Its flags are taken as `mount(2)` takes them: the per-mount ones at `fsmount`
/// ([`attr_flags`]), `sync` and `dirsync` by the context, where once only read-only, `nosuid`, `nodev` and
/// `noexec` were, an access time the spec set lost.
pub(crate) fn make_fs(
    source: &str,
    fstype: &str,
    data: &str,
    flags: MountFlags,
    label: Option<&str>,
) -> Result<OwnedFd, OciError> {
    let fs = fs_context(source, fstype, data, flags, label)?;
    let mount =
        rustix::mount::fsmount(fs.as_fd(), FsMountFlags::FSMOUNT_CLOEXEC, attr_flags(flags))
            .map_err(|e| OciError::os("fsmount", e))?;
    if let Some(label) = label
        && super::lsm::labelling(fstype) == super::lsm::Labelling::Xattr
    {
        // As for a context: skipped where SELinux doesn't run, which the create warned of.
        if let Err(e) = super::lsm::label_root(mount.as_fd(), label)
            && super::lsm::selinux_module()? == super::lsm::Module::Active
        {
            return Err(e);
        }
    }
    Ok(mount)
}

/// A filesystem made as [`make_fs`] makes it, as the tree of a new mount namespace (decision 6's update):
/// by `fsmount(FSMOUNT_NAMESPACE)` where the kernel has it (Linux 7.1), one hold of the mount lock and no
/// detached mount left to dissolve (`goal6-mntns.md` §3); else the detached mount made a namespace by
/// `open_tree(OPEN_TREE_NAMESPACE)` (7.0), whose mount then dissolves with its descriptor. A fresh
/// filesystem's mount has no peer group, so the namespace's tree needs no propagation changed.
pub(crate) fn make_fs_namespace(
    source: &str,
    fstype: &str,
    data: &str,
    flags: MountFlags,
    label: Option<&str>,
) -> Result<OwnedFd, OciError> {
    // A namespace's root is labelled by its context alone: one labelled by an xattr isn't a root.
    if label.is_some() && super::lsm::labelling(fstype) == super::lsm::Labelling::Xattr {
        return Err(OciError::Config(format!(
            "linux.mountLabel is set and the root is a {fstype}, which takes the label by an xattr: Hex \
             makes no namespace of one"
        )));
    }
    let fs = fs_context(source, fstype, data, flags, label)?;
    let namespace = FsMountFlags::from_bits_retain(
        FsMountFlags::FSMOUNT_CLOEXEC.bits() | super::mntns::FSMOUNT_NAMESPACE,
    );
    match rustix::mount::fsmount(fs.as_fd(), namespace, attr_flags(flags)) {
        Ok(ns) => Ok(ns),
        // Below 7.1 fsmount refuses the flag before it makes anything (v7.0 `fs/namespace.c`, its flag
        // check), so the context still waits for its mount.
        Err(rustix::io::Errno::INVAL) => {
            let mount = rustix::mount::fsmount(
                fs.as_fd(),
                FsMountFlags::FSMOUNT_CLOEXEC,
                attr_flags(flags),
            )
            .map_err(|e| OciError::os("fsmount", e))?;
            namespace_of(mount.as_fd(), Path::new(""))
        }
        Err(e) => Err(OciError::os("fsmount a namespace", e)),
    }
}

/// A new mount namespace whose tree is a recursive copy of the tree at `path` beneath `dir` (empty: `dir`
/// itself), by `open_tree(OPEN_TREE_NAMESPACE | AT_RECURSIVE)` (Linux 7.0, `goal6-mntns.md` §1): one hold
/// of the mount lock, owned by this process's user namespace. The copy keeps a shared source's peer
/// group; the init changes its propagation before it mounts anything beneath.
pub(crate) fn namespace_of(dir: BorrowedFd<'_>, path: &Path) -> Result<OwnedFd, OciError> {
    let mut flags = OpenTreeFlags::from_bits_retain(super::mntns::OPEN_TREE_NAMESPACE)
        | OpenTreeFlags::AT_RECURSIVE
        | OpenTreeFlags::OPEN_TREE_CLOEXEC;
    if path.as_os_str().is_empty() {
        flags |= OpenTreeFlags::AT_EMPTY_PATH;
    }
    rustix::mount::open_tree(dir, path, flags)
        .map_err(|e| OciError::os("open_tree(OPEN_TREE_NAMESPACE)", e))
}

/// The filesystem context [`make_fs`] mounts: opened, its source, flags and options set, and created.
fn fs_context(
    source: &str,
    fstype: &str,
    data: &str,
    flags: MountFlags,
    label: Option<&str>,
) -> Result<OwnedFd, OciError> {
    // The refill worker runs concurrently with helper execs: the context must not leak through
    // a seed/shim exec while this function is between fsopen and fsmount.
    let fs = rustix::mount::fsopen(fstype, FsOpenFlags::FSOPEN_CLOEXEC)
        .map_err(|e| OciError::os("fsopen", e))?;
    rustix::mount::fsconfig_set_string(fs.as_fd(), "source", source)
        .map_err(|e| OciError::os("a filesystem's source", format!("{source}: {e}")))?;
    // The superblock's own flags, which a mount can't carry: as flags of the context
    // (`fs/fs_context.c:41-46`), as `mount(2)` takes them from its flags.
    for (flag, name) in [
        (MountFlags::SYNCHRONOUS, "sync"),
        (MountFlags::DIRSYNC, "dirsync"),
    ] {
        if flags.contains(flag) {
            rustix::mount::fsconfig_set_flag(fs.as_fd(), name)
                .map_err(|e| OciError::os("a filesystem flag", format!("{name}: {e}")))?;
        }
    }
    for opt in data.split(',').filter(|s| !s.is_empty()) {
        match opt.split_once('=') {
            Some((key, value)) => rustix::mount::fsconfig_set_string(fs.as_fd(), key, value)
                .map_err(|e| OciError::os("a filesystem option", format!("{opt}: {e}")))?,
            None => rustix::mount::fsconfig_set_flag(fs.as_fd(), opt)
                .map_err(|e| OciError::os("a filesystem flag", format!("{opt}: {e}")))?,
        }
    }
    // The mount label as a parameter of its own, which an MLS label's commas can't break (research §1).
    if let Some(label) = label
        && super::lsm::labelling(fstype) == super::lsm::Labelling::Context
        && let Err(e) = rustix::mount::fsconfig_set_string(fs.as_fd(), "context", label)
        // Where SELinux doesn't run, the label is skipped: the create found that first, warned of it, and
        // under HEX_OCI_LSM=strict refused (lsm::admit). Where it runs, the refusal is the label's.
        && super::lsm::selinux_module()? == super::lsm::Module::Active
    {
        return Err(super::lsm::refused_label(label, fstype, e));
    }
    rustix::mount::fsconfig_create(fs.as_fd()).map_err(|e| match label {
        Some(label) => OciError::os(
            "fsconfig create",
            format!("with linux.mountLabel {label:?}: {e}"),
        ),
        None => OciError::os("fsconfig create", e),
    })?;
    Ok(fs)
}

/// Masks a path on the pruned path (goal 5 plan step 2): a directory with a read-only empty tmpfs, a
/// file by binding the container's own `/dev/null` over it. The classic [`mask_path`] binds the host's
/// `/dev/null`; this namespace holds none, so the container's own is used, as the research prescribes.
/// `null` keeps it once the first file's mask has opened it, for the masks after.
fn mask_path_pruned(
    root: &inroot::Root<'_>,
    path: &str,
    null: &mut Option<OwnedFd>,
    masked_directory: Option<&OwnedFd>,
    label: Option<&str>,
) -> Result<(), OciError> {
    let Some(target_fd) = root.open(path, Make::Nothing)? else {
        return Ok(());
    };
    let is_dir = rustix::fs::fstat(&target_fd)
        .map(|st| rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir())
        .map_err(|e| OciError::os("masking a path", format!("{path}: {e}")))?;
    if is_dir {
        match masked_directory {
            Some(empty) => clone_masked_directory(empty.as_fd(), target_fd.as_fd())
                .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?,
            None => {
                let mount = make_fs("tmpfs", "tmpfs", EMPTY_TMPFS, MountFlags::RDONLY, label)
                    .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?;
                rustix::mount::move_mount(mount.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
                    .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?;
            }
        }
    } else {
        let null = match null {
            Some(fd) => fd,
            None => null.insert(root.open("/dev/null", Make::Nothing)?.ok_or_else(|| {
                OciError::Config(format!(
                    "masking {path} needs the container's /dev/null, which isn't there"
                ))
            })?),
        };
        let tree = rustix::mount::open_tree(
            null.as_fd(),
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_EMPTY_PATH
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )
        .map_err(|e| OciError::os("masking a file", format!("{path}: {e}")))?;
        rustix::mount::move_mount(tree.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
            .map_err(|e| OciError::os("masking a file", format!("{path}: {e}")))?;
    }
    Ok(())
}

/// A read-only path on the pruned path: the target cloned as a detached tree, set read-only on the
/// tree (recursively), and moved back over itself. The classic [`readonly_path`] binds and remounts
/// through `/proc/self/fd`, which this namespace has not.
fn readonly_path_pruned(root: &inroot::Root<'_>, path: &str) -> Result<(), OciError> {
    let Some(target_fd) = root.open(path, Make::Nothing)? else {
        return Ok(());
    };
    let tree = rustix::mount::open_tree(
        target_fd.as_fd(),
        "",
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_EMPTY_PATH
            | OpenTreeFlags::AT_RECURSIVE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )
    .map_err(|e| OciError::os("a read-only path", format!("{path}: {e}")))?;
    sys::mount_setattr(
        tree.as_fd(),
        true,
        &sys::MountAttrs {
            set: sys::MOUNT_ATTR_RDONLY,
            clr: 0,
            propagation: 0,
        },
    )
    .map_err(|e| OciError::os("a read-only path", format!("{path}: {e}")))?;
    rustix::mount::move_mount(tree.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
        .map_err(|e| OciError::os("a read-only path", format!("{path}: {e}")))?;
    Ok(())
}

/// Whether the mount a detached tree's descriptor roots is a regular file (a file-source bind), so its
/// target is made as a file, not a directory.
fn is_regular_file(fd: BorrowedFd<'_>) -> Result<bool, OciError> {
    let st = rustix::fs::fstat(fd).map_err(|e| OciError::os("stat a bind source", e))?;
    Ok(rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile)
}

/// A spec mount's read-only, `nosuid`, `nodev` and `noexec` as `mount_attr`'s `attr_set` bits, for a
/// bind tree set with [`sys::mount_setattr`].
fn attr_set(flags: MountFlags) -> u64 {
    let mut set = 0u64;
    if flags.contains(MountFlags::RDONLY) {
        set |= sys::MOUNT_ATTR_RDONLY;
    }
    if flags.contains(MountFlags::NOSUID) {
        set |= sys::MOUNT_ATTR_NOSUID;
    }
    if flags.contains(MountFlags::NODEV) {
        set |= sys::MOUNT_ATTR_NODEV;
    }
    if flags.contains(MountFlags::NOEXEC) {
        set |= sys::MOUNT_ATTR_NOEXEC;
    }
    set
}

/// A spec mount's per-mount flags as [`MountAttrFlags`], for a filesystem made at `fsmount`, each as
/// `mount(2)` takes it (v6.19 `fs/namespace.c:3972-3992`): read-only, `nosuid`, `nodev`, `noexec`,
/// `nodiratime` and `nosymfollow`, and the access-time mode, `strictatime` over `noatime` over the
/// relatime the kernel defaults to.
fn attr_flags(flags: MountFlags) -> MountAttrFlags {
    let mut a = if flags.contains(MountFlags::STRICTATIME) {
        MountAttrFlags::MOUNT_ATTR_STRICTATIME
    } else if flags.contains(MountFlags::NOATIME) {
        MountAttrFlags::MOUNT_ATTR_NOATIME
    } else {
        MountAttrFlags::MOUNT_ATTR_RELATIME
    };
    if flags.contains(MountFlags::NODIRATIME) {
        a |= MountAttrFlags::MOUNT_ATTR_NODIRATIME;
    }
    if flags.contains(MountFlags::NOSYMFOLLOW) {
        a |= MountAttrFlags::MOUNT_ATTR_NOSYMFOLLOW;
    }
    if flags.contains(MountFlags::RDONLY) {
        a |= MountAttrFlags::MOUNT_ATTR_RDONLY;
    }
    if flags.contains(MountFlags::NOSUID) {
        a |= MountAttrFlags::MOUNT_ATTR_NOSUID;
    }
    if flags.contains(MountFlags::NODEV) {
        a |= MountAttrFlags::MOUNT_ATTR_NODEV;
    }
    if flags.contains(MountFlags::NOEXEC) {
        a |= MountAttrFlags::MOUNT_ATTR_NOEXEC;
    }
    a
}

/// A mount's propagation as `mount_attr`'s `propagation` value and whether it applies recursively,
/// for [`sys::mount_setattr`]. Exactly one of unbindable, private, slave (downstream) or shared.
fn propagation_ms(flags: MountPropagationFlags) -> (u64, bool) {
    let recursive = flags.contains(MountPropagationFlags::REC);
    let ms = if flags.contains(MountPropagationFlags::SHARED) {
        sys::MS_SHARED
    } else if flags.contains(MountPropagationFlags::DOWNSTREAM) {
        sys::MS_SLAVE
    } else if flags.contains(MountPropagationFlags::UNBINDABLE) {
        sys::MS_UNBINDABLE
    } else {
        sys::MS_PRIVATE
    };
    (ms, recursive)
}

/// A rootfs-propagation name (OCI `rootfsPropagation`) as a propagation flag.
fn propagation(name: &str) -> Option<MountPropagationFlags> {
    let f = match name {
        "shared" => MountPropagationFlags::SHARED,
        "slave" => MountPropagationFlags::DOWNSTREAM,
        "private" => MountPropagationFlags::PRIVATE,
        "unbindable" => MountPropagationFlags::UNBINDABLE,
        _ => return None,
    };
    Some(f)
}

fn do_mount(
    root: &inroot::Root<'_>,
    op: &crate::linux::oci::mount::Op,
    label: Option<&str>,
) -> Result<(), OciError> {
    // Which mount failed, and of what, with why.
    let failed = |e: rustix::io::Errno| {
        format!(
            "{} at {}: {e}",
            if op.is_bind {
                op.kind_source.as_str()
            } else {
                op.fstype.as_str()
            },
            op.destination
        )
    };
    // A missing destination is made as what the mount needs: a file for a file's bind.
    let target_fd = match root.open(&op.destination, Make::Nothing)? {
        Some(fd) => fd,
        None if op.is_bind && Path::new(&op.kind_source).is_file() => {
            target(root, &op.destination, Make::File)?
        }
        None => target(root, &op.destination, Make::Dir)?,
    };
    let at = inroot::fd_path(&target_fd);
    if op.is_bind {
        if op.recursive_bind {
            rustix::mount::mount_bind_recursive(&op.kind_source, &at)
                .map_err(|e| OciError::os("bind mount", failed(e)))?;
        } else {
            rustix::mount::mount_bind(&op.kind_source, &at)
                .map_err(|e| OciError::os("bind mount", failed(e)))?;
        }
        if op.flags.intersects(MountFlags::RDONLY) {
            // Opened again, the destination is the bind just made on top of the target.
            let top_fd = target(root, &op.destination, Make::Nothing)?;
            let top = inroot::fd_path(&top_fd);
            rustix::mount::mount_remount(&top, op.flags | MountFlags::BIND, op.data.as_str())
                .map_err(|e| OciError::os("remounting bind", failed(e)))?;
            // A recursive bind carries the source's submounts with the source's own flags, so the
            // remount above, which changes only the bind's top, leaves them writable. For a rootless
            // box on the host's network that means the host's cgroup filesystem stays writable
            // beneath the box's own cgroup mounts over it (crates/hex-core/src/bundle.rs,
            // `default_mounts`), where a box holding CAP_SYS_ADMIN in its user namespace could reach
            // every box's cgroup files by unmounting its own cgroup mounts off (T-T7.1.4). So each
            // cgroup filesystem the bind carries is remounted read-only too.
            if op.recursive_bind {
                remount_cgroupfs_readonly(root, &op.destination, &top_fd, op.flags)?;
            }
        }
    } else {
        // A filesystem is made as the pruned path makes it ([`make_fs`]: its source, flags and data as
        // `mount(2)` takes them) and attached to the target's descriptor, the one hold of the mount lock
        // `mount(2)` takes too, without `mount(2)`'s walk of `/proc/self/fd` to name the target (goal 5
        // extended step 3, `goal5-mounts.md` §12). Its propagation is set on it through its own
        // descriptor, as on the pruned path.
        let fs = make_fs(&op.kind_source, &op.fstype, &op.data, op.flags, label).map_err(|e| {
            OciError::os("mount", format!("{} at {}: {e}", op.fstype, op.destination))
        })?;
        rustix::mount::move_mount(fs.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
            .map_err(|e| OciError::os("mount", failed(e)))?;
        if let Some(prop) = op.propagation {
            let (ms, rec) = propagation_ms(prop);
            sys::mount_setattr(
                fs.as_fd(),
                rec,
                &sys::MountAttrs {
                    set: 0,
                    clr: 0,
                    propagation: ms,
                },
            )
            .map_err(|e| {
                OciError::os(
                    "mount propagation",
                    format!("{} at {}: {e}", op.fstype, op.destination),
                )
            })?;
        }
        return Ok(());
    }
    if let Some(prop) = op.propagation {
        let top_fd = target(root, &op.destination, Make::Nothing)?;
        rustix::mount::mount_change(inroot::fd_path(&top_fd), prop)
            .map_err(|e| OciError::os("mount propagation", failed(e)))?;
    }
    Ok(())
}

/// The `/dev` the node made ahead, attached where the spec's goes (goal 5 extended, step 3): on its target
/// resolved in the root and made when missing, as [`do_mount`] makes it, by `move_mount` from the detached
/// tree, with the propagation the spec asks for set on the mount through its own descriptor. Its making
/// (the tmpfs's superblock, its nodes and links) was the pool's, off the create's path.
fn attach_prepared_dev(
    root: &inroot::Root<'_>,
    op: &crate::linux::oci::mount::Op,
    tree: &OwnedFd,
) -> Result<(), OciError> {
    let at = || format!("{} at {}", op.fstype, op.destination);
    let target_fd = target(root, &op.destination, Make::Dir)?;
    rustix::mount::move_mount(tree.as_fd(), "", target_fd.as_fd(), "", FD_TO_FD)
        .map_err(|e| OciError::os("attaching the prepared /dev", format!("{}: {e}", at())))?;
    if let Some(prop) = op.propagation {
        let (ms, rec) = propagation_ms(prop);
        sys::mount_setattr(
            tree.as_fd(),
            rec,
            &sys::MountAttrs {
                set: 0,
                clr: 0,
                propagation: ms,
            },
        )
        .map_err(|e| OciError::os("setting a mount's propagation", format!("{}: {e}", at())))?;
    }
    Ok(())
}

/// `dest` opened in the container's root, made as `make` says when it's missing.
fn target(root: &inroot::Root<'_>, dest: &str, make: Make) -> Result<OwnedFd, OciError> {
    root.open(dest, make)?
        .ok_or_else(|| OciError::Config(format!("{dest} isn't in the container's root")))
}

/// The most mounts one mount namespace holds, `fs.mount-max` (Documentation/admin-guide/sysctl/fs.rst:126-130), to
/// which the kernel holds every namespace (fs/namespace.c, `count_mounts`). It bounds every walk here of a
/// namespace's mounts, the kernel's listing of a bind's submounts and the mount table's lines alike, and so the
/// cgroup filesystems found among them (CLAUDE.md §2): past it a recursive read-only bind is refused, by name,
/// rather than its cgroup filesystems left unchecked. Read once a process, when a walk first goes past what one
/// listing or one read holds (a usual bind's never does), and kept; a create seed reads it before its prune leaves
/// it no `/proc` ([`super::seed`]), so every init it clones has it.
static MOUNT_MAX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// [`MOUNT_MAX`], read from `/proc/sys/fs/mount-max` where this process hasn't yet.
pub(crate) fn mount_max() -> Result<u64, OciError> {
    use std::io::Read;
    const WHAT: &str = "reading fs.mount-max";
    if let Some(&n) = MOUNT_MAX.get() {
        return Ok(n);
    }
    // An `unsigned int` (fs/namespace.c:42), written in at most 10 digits and a newline.
    let mut text = String::new();
    fs::File::open("/proc/sys/fs/mount-max")
        .and_then(|f| f.take(11).read_to_string(&mut text))
        .map_err(|e| OciError::os(WHAT, e))?;
    let n = text
        .trim()
        .parse::<u64>()
        .map_err(|e| OciError::os(WHAT, format!("{text:?}: {e}")))?;
    Ok(*MOUNT_MAX.get_or_init(|| n))
}

/// The refusal of the recursive bind at `destination`, whose walk of the namespace's mounts went past the
/// namespace's own cap, `max` ([`MOUNT_MAX`]): the mounts changed under the walk faster than it read them.
fn past_mount_max(destination: &str, max: u64) -> OciError {
    OciError::Config(format!(
        "the recursive bind at {destination} carries over fs.mount-max ({max}) mounts to look at for \
         cgroup filesystems"
    ))
}

/// Each cgroup filesystem a recursive read-only bind carries, remounted read-only. A `mount(2)`
/// remount changes one mount, not the submounts a recursive bind brought with it, so remounting the
/// bind's top leaves them with the source's flags. For a rootless box's `/sys` that is the host's
/// cgroup filesystem, which stays writable beneath the box's own cgroup mounts over it; this makes it
/// read-only, so a box holding `CAP_SYS_ADMIN` that unmounts its own cgroup mounts off reaches a
/// read-only host tree, not a writable one. They are asked of the kernel ([`cgroupfs_listed`]), or
/// found in the mount table where the kernel can't answer ([`cgroupfs_in_table`]); at most as many as the
/// namespace holds mounts ([`MOUNT_MAX`], §2).
fn remount_cgroupfs_readonly(
    root: &inroot::Root<'_>,
    destination: &str,
    top_fd: &OwnedFd,
    flags: MountFlags,
) -> Result<(), OciError> {
    let beneath = match cgroupfs_listed(top_fd, destination)? {
        Some(beneath) => beneath,
        None => cgroupfs_in_table(top_fd, destination)?,
    };
    let base = destination.trim_end_matches('/');
    for under in beneath {
        let dest = format!("{base}/{under}");
        let sub = target(root, &dest, Make::Nothing)?;
        rustix::mount::mount_remount(inroot::fd_path(&sub), flags | MountFlags::BIND, "").map_err(
            |e| {
                OciError::os(
                    "remounting a cgroup filesystem read-only",
                    format!("{dest}: {e}"),
                )
            },
        )?;
    }
    Ok(())
}

/// Mount ids `listmount` writes per call. Not learned or derived: an init lives for one create, so it has no
/// listing before its own to learn from, and the namespace's cap ([`MOUNT_MAX`], 100,000 by default) would make a
/// buffer of 800 KB on every create's path, whose pages the init touches (goal5-mounts.md §5 counts each page a
/// fault in the init's set-up). A usual bind's submounts fit one call (`/sys` carries about ten); a larger one
/// takes one call more per this many. Unmeasured: a record of its calls against its pages would set it.
const LISTMOUNT_BATCH: usize = 64;

// `statmount(2)`'s fields this reads and the bits that ask for its strings (v6.19
// include/uapi/linux/mount.h:155-189, :217-218): the kernel writes its fixed 512 bytes, then each string
// asked at the offset its field gives past them (fs/namespace.c:5472).
const STATMOUNT_MNT_POINT: u64 = 0x10;
const STATMOUNT_FS_TYPE: u64 = 0x20;
const STATMOUNT_SIZE_AT: usize = 0;
const STATMOUNT_MASK_AT: usize = 8;
const STATMOUNT_FS_TYPE_AT: usize = 36;
const STATMOUNT_MNT_POINT_AT: usize = 108;
const STATMOUNT_FIXED: usize = 512;
/// `statmount`'s buffer to begin with: the fixed part and 1.5 KiB of strings, a filesystem type and a
/// mount point of any usual length. Each page of a larger one the init zeroes is a page fault in its
/// set-up: this 2 KiB start (5cccb60) took 2 of the init's faults per create against the 8.5 KiB of
/// [`STATMOUNT_BUF`], 27.1 against 29.1, and 4.3 us against 9.3 before its `statmount` (fedora, runs
/// 20261102T052422Z against 052432Z, traced; goal5-mounts.md §5), so it grows only when the kernel says the
/// strings don't fit (`EOVERFLOW`). The 1.5 KiB within it is unmeasured: the record compares 2 KiB with 8.5.
const STATMOUNT_FIRST: usize = STATMOUNT_FIXED + 1536;
/// The most `statmount`'s buffer grows to: the fixed part, then a filesystem type and a mount point,
/// each at most [`PATH_MAX`] with its NUL. A longer one is refused (`EOVERFLOW`), not cut.
const STATMOUNT_BUF: usize = STATMOUNT_FIXED + 2 * PATH_MAX;

/// The cgroup filesystems beneath the bind open as `top_fd`, each as its path under the bind's top,
/// asked of the kernel (Linux 6.8, `docs/research/runtime/goal5-mounts.md` §2): the bind's unique mount
/// id (`statx`'s `STATX_MNT_ID_UNIQUE`) and its mount point (`statmount`), the ids of every mount
/// beneath it (`listmount`), and each one's type and point. Flat: listed in batches, at most as many looked
/// at as the namespace holds mounts ([`MOUNT_MAX`], read only for a second batch; §2, §11). `None` where the kernel has no answer (before 6.8: no
/// unique id, `ENOSYS`), for the table to give instead.
fn cgroupfs_listed(top_fd: &OwnedFd, destination: &str) -> Result<Option<Vec<String>>, OciError> {
    let unique = rustix::fs::StatxFlags::from_bits_retain(libc::STATX_MNT_ID_UNIQUE);
    let stx = rustix::fs::statx(top_fd, "", rustix::fs::AtFlags::EMPTY_PATH, unique)
        .map_err(|e| OciError::os("reading the bind's mount id", e))?;
    if !rustix::fs::StatxFlags::from_bits_retain(stx.stx_mask).contains(unique) {
        return Ok(None);
    }
    let mut buf = vec![0u8; STATMOUNT_FIRST];
    let Some((_, top)) = mount_info(stx.stx_mnt_id, STATMOUNT_MNT_POINT, &mut buf)? else {
        return Ok(None);
    };
    let top = Path::new(&top);
    let mut found = Vec::new();
    let mut ids = [0u64; LISTMOUNT_BATCH];
    let (mut last, mut looked) = (0u64, 0u64);
    loop {
        let n = match sys::listmount(stx.stx_mnt_id, last, &mut ids) {
            Ok(n) => n,
            Err(rustix::io::Errno::NOSYS) => return Ok(None),
            Err(e) => return Err(OciError::os("listing the bind's submounts", e)),
        };
        let listed = ids.get(..n).ok_or_else(|| {
            OciError::os(
                "listing the bind's submounts",
                format!("{n} ids, past the {LISTMOUNT_BATCH} asked"),
            )
        })?;
        for &id in listed {
            looked = looked
                .checked_add(1)
                .ok_or_else(|| past_mount_max(destination, u64::MAX))?;
            let Some((fstype, point)) =
                mount_info(id, STATMOUNT_FS_TYPE | STATMOUNT_MNT_POINT, &mut buf)?
            else {
                return Ok(None);
            };
            if !matches!(fstype.as_str(), "cgroup2" | "cgroup") {
                continue;
            }
            if let Some(under) = beneath_top(&point, top) {
                found.push(under);
            }
        }
        match listed.last() {
            Some(&id) if n == ids.len() => {
                let max = mount_max()?;
                if looked > max {
                    return Err(past_mount_max(destination, max));
                }
                last = id;
            }
            _ => break,
        }
    }
    Ok(Some(found))
}

/// Mount `id`'s filesystem type and mount point, as many of them as `mask` asks (the other is empty),
/// read through `statmount` into `buf`, which grows once to [`STATMOUNT_BUF`] when they don't fit.
/// `None` from a kernel without it.
fn mount_info(id: u64, mask: u64, buf: &mut Vec<u8>) -> Result<Option<(String, String)>, OciError> {
    const WHAT: &str = "reading a mount's type and point";
    let mut asked = sys::statmount(id, mask, buf);
    if asked == Err(rustix::io::Errno::OVERFLOW) && buf.len() < STATMOUNT_BUF {
        buf.resize(STATMOUNT_BUF, 0);
        asked = sys::statmount(id, mask, buf);
    }
    match asked {
        Ok(()) => {}
        Err(rustix::io::Errno::NOSYS) => return Ok(None),
        Err(e) => return Err(OciError::os(WHAT, e)),
    }
    let buf = &**buf;
    let short = || OciError::os(WHAT, "a short answer");
    let used = field_u32(buf, STATMOUNT_SIZE_AT).ok_or_else(short)?;
    if field_u64(buf, STATMOUNT_MASK_AT).ok_or_else(short)? & mask != mask {
        return Err(OciError::os(
            WHAT,
            "the kernel gave not all the fields asked",
        ));
    }
    let end = usize::try_from(used).map_or(buf.len(), |used| used.min(buf.len()));
    let text = |field: usize, bit: u64| -> Result<String, OciError> {
        if mask & bit == 0 {
            return Ok(String::new());
        }
        let off = field_u32(buf, field).ok_or_else(short)?;
        let rest = usize::try_from(off)
            .ok()
            .and_then(|off| STATMOUNT_FIXED.checked_add(off))
            .and_then(|start| buf.get(start..end))
            .ok_or_else(|| OciError::os(WHAT, "a string outside the answer"))?;
        let s = rest.split(|&b| b == 0).next().unwrap_or_default();
        Ok(String::from_utf8_lossy(s).into_owned())
    };
    Ok(Some((
        text(STATMOUNT_FS_TYPE_AT, STATMOUNT_FS_TYPE)?,
        text(STATMOUNT_MNT_POINT_AT, STATMOUNT_MNT_POINT)?,
    )))
}

/// The `u32` the kernel wrote at byte `at` of `buf`, in its own byte order.
fn field_u32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        buf.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// The `u64` the kernel wrote at byte `at` of `buf`, in its own byte order.
fn field_u64(buf: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(
        buf.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// `point`'s path under the bind's top `top`, or `None` when it isn't beneath it or is the top itself
/// (the bind's own top, which the caller remounted).
fn beneath_top(point: &str, top: &Path) -> Option<String> {
    let under = Path::new(point).strip_prefix(top).ok()?.to_string_lossy();
    (!under.is_empty()).then(|| under.into_owned())
}

/// The cgroup filesystems beneath the bind open as `top_fd`, found in this process's mount table, where
/// the kernel has no `listmount` (before 6.8). Flat: one bounded pass over the table (§11), read a page at a time.
fn cgroupfs_in_table(top_fd: &OwnedFd, destination: &str) -> Result<Vec<String>, OciError> {
    let top_real = std::fs::read_link(inroot::fd_path(top_fd))
        .map_err(|e| OciError::os("reading the bind's path", e))?;
    let file = std::fs::File::open("/proc/self/mountinfo")
        .map_err(|e| OciError::os("reading the mount table", e))?;
    cgroupfs_in_mountinfo(file, &top_real, destination)
}

/// `PATH_MAX`, the longest path the kernel takes, its NUL included (include/uapi/linux/limits.h:13).
const PATH_MAX: usize = 4096;

/// The longest mount point the table's walk keeps, as the table writes it: a point a remount can reach is a path
/// `openat2` takes, under [`PATH_MAX`] with its NUL, and the table writes each of its bytes as at most four (a
/// space, tab, newline or backslash as `\ooo`, fs/proc_namespace.c:151).
const MAX_POINT_WRITTEN: usize = 4 * PATH_MAX;

/// The cgroup filesystems beneath `top` in the mount table `table` (proc_pid_mountinfo(5)), for the recursive bind
/// at `destination`. One pass, a page at a time, keeping of each line only its mount point and filesystem type:
/// the kernel hands a reader at most its seq_file's buffer a call, a page unless one line needed more
/// (fs/seq_file.c:211, :246), so a larger buffer saves no call. Bounded by the kernel's own limits alone: lines by
/// the namespace's mount cap ([`MOUNT_MAX`], read only when the table runs past its first page), a line by the
/// most one seq_file record holds, `MAX_RW_COUNT` (`INT_MAX` rounded down to a page; fs/seq_file.c:36,
/// include/linux/fs.h:2453), and what's kept of a line by [`MAX_POINT_WRITTEN`]. A cgroup filesystem whose point is
/// past `PATH_MAX` is refused by name: no remount could reach it.
fn cgroupfs_in_mountinfo(
    table: impl std::io::Read,
    top: &Path,
    destination: &str,
) -> Result<Vec<String>, OciError> {
    use std::io::BufRead;
    const WHAT: &str = "reading the mount table";
    let page = sys::page_size()?;
    let max_record = u64::try_from(i32::MAX)
        .ok()
        .zip(u64::try_from(page).ok())
        .map(|(max, page)| max & !page.saturating_sub(1))
        .ok_or_else(|| OciError::os(WHAT, "a page size past 64 bits"))?;
    let mut table = std::io::BufReader::with_capacity(page, table);
    let mut found = Vec::new();
    let mut line = MountLine::default();
    let (mut lines, mut reads, mut max) = (0u64, 0u64, None);
    loop {
        let chunk = table.fill_buf().map_err(|e| OciError::os(WHAT, e))?;
        if chunk.is_empty() {
            break;
        }
        reads = reads
            .checked_add(1)
            .ok_or_else(|| OciError::os(WHAT, "reads past u64"))?;
        if reads > 1 && max.is_none() {
            max = Some(mount_max()?);
        }
        for &b in chunk {
            line.bytes = line
                .bytes
                .checked_add(1)
                .filter(|&n| n <= max_record)
                .ok_or_else(|| OciError::os(WHAT, format!("a line past {max_record} bytes")))?;
            if b != b'\n' {
                line.take(b);
                continue;
            }
            lines = lines
                .checked_add(1)
                .ok_or_else(|| OciError::os(WHAT, "lines past u64"))?;
            if let Some(max) = max.filter(|&max| lines > max) {
                return Err(past_mount_max(destination, max));
            }
            line.finish(top, destination, &mut found)?;
            line = MountLine::default();
        }
        let n = chunk.len();
        table.consume(n);
    }
    if line.bytes > 0 {
        line.finish(top, destination, &mut found)?;
    }
    Ok(found)
}

/// What [`cgroupfs_in_mountinfo`] keeps of one mount table line as it reads it: which field it is in, the mount
/// point (field 5) and the filesystem type (the field after the lone `-` that ends the optional fields).
#[derive(Default)]
struct MountLine {
    bytes: u64,
    field: usize,
    /// The field so far is `-` alone.
    dash: bool,
    /// The field so far holds a byte.
    begun: bool,
    /// The type's field, once the `-` before it is read.
    type_field: Option<usize>,
    point: Vec<u8>,
    point_long: bool,
    fstype: Vec<u8>,
}

impl MountLine {
    /// One more byte of the line, its newline excepted.
    fn take(&mut self, b: u8) {
        if b == b' ' {
            if self.type_field.is_none() && self.field > 5 && self.dash {
                self.type_field = self.field.checked_add(1);
            }
            // A line's fields are fewer than its bytes, which `max_record` bounds far below `usize::MAX`.
            self.field = self.field.saturating_add(1);
            (self.dash, self.begun) = (false, false);
            return;
        }
        self.dash = !self.begun && b == b'-';
        self.begun = true;
        if self.field == 4 {
            if self.point.len() < MAX_POINT_WRITTEN {
                self.point.push(b);
            } else {
                self.point_long = true;
            }
        } else if Some(self.field) == self.type_field && self.fstype.len() <= "cgroup2".len() {
            // One byte past the longest name sought is enough to say it is neither.
            self.fstype.push(b);
        }
    }

    /// The line read whole: its point under `top` added to `found` where it is a cgroup filesystem's, and refused,
    /// naming the bind at `destination`, where that point is past what a remount can reach. A point cut at
    /// [`MAX_POINT_WRITTEN`] is judged beneath `top` or not by what was kept of it, which starts as it does.
    fn finish(
        &self,
        top: &Path,
        destination: &str,
        found: &mut Vec<String>,
    ) -> Result<(), OciError> {
        if !matches!(self.fstype.as_slice(), b"cgroup2" | b"cgroup") {
            return Ok(());
        }
        let point = unescape_mountpoint(&String::from_utf8_lossy(&self.point));
        let Some(under) = beneath_top(&point, top) else {
            return Ok(());
        };
        if self.point_long || point.len() >= PATH_MAX {
            return Err(OciError::Config(format!(
                "the recursive bind at {destination} carries a cgroup filesystem whose mount point is \
                 past PATH_MAX, which no remount can reach"
            )));
        }
        if std::str::from_utf8(&self.point).is_err() {
            return Err(OciError::os(
                "reading the mount table",
                "a cgroup filesystem's mount point not UTF-8",
            ));
        }
        found.push(under);
        Ok(())
    }
}

/// A `/proc/self/mountinfo` mount point with its escapes (space `\040`, tab `\011`, newline `\012`,
/// backslash `\134`; proc_pid_mountinfo(5)) turned back into their characters, the backslash last so a
/// decoded one isn't read as the start of another escape.
fn unescape_mountpoint(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// [`prepare_proc`]'s failure to open the pid namespace it made, which a kernel below Linux 7.1 refuses while
/// the namespace has no process (`pidns_for_children_get`, v6.19 `kernel/pid_namespace.c:372-379`; v7.1
/// `:361-373` dropped the check): the node's worker reads it as a kernel that can make no proc ahead.
pub const PID_NAMESPACE_AHEAD: &str = "opening a pid namespace made ahead";

/// What every proc made ahead binds its masks from, opened once for the node's life: the host's `/dev/null`
/// for a masked file, and one empty read-only tmpfs, detached, cloned for each masked directory. crun binds one
/// empty directory for every masked directory alike (1.30.1 `src/libcrun/linux.c:1181-1210`, `:1266`): a
/// clone makes no superblock, so none takes `list_lrus_mutex` or `shrinker_mutex` (step 6,
/// `create_serial.md` runs 23-24).
///
/// The empty tmpfs is kept only where the kernel clones a detached mount. Before Linux 6.15 `open_tree` refuses a
/// mount outside the caller's namespace with `EINVAL` (v6.8 `fs/namespace.c:2588`; v6.19's `may_copy_tree`,
/// `:2928-2948`, admits an anonymous namespace's), so there every masked directory gets a tmpfs of its own.
pub struct ProcSources {
    null: OwnedFd,
    empty: Option<OwnedFd>,
}

impl ProcSources {
    /// The empty read-only tmpfs, which an init clones for each masked directory, where the kernel clones it.
    pub fn empty(&self) -> Option<BorrowedFd<'_>> {
        self.empty.as_ref().map(AsFd::as_fd)
    }

    pub fn open() -> Result<Self, OciError> {
        let null = rustix::fs::open("/dev/null", OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| OciError::os("opening /dev/null for the prepared procs", e))?;
        let empty = make_fs("tmpfs", "tmpfs", EMPTY_TMPFS, MountFlags::RDONLY, None)?;
        // One clone, dropped at once, asks the kernel whether it clones a detached mount.
        let empty = match rustix::mount::open_tree(
            &empty,
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_EMPTY_PATH
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        ) {
            Ok(_) => Some(empty),
            Err(rustix::io::Errno::INVAL) => None,
            Err(e) => return Err(OciError::os("cloning the empty tmpfs", e)),
        };
        Ok(Self { null, empty })
    }
}

/// A container's proc made ahead (decision 6's update, part 2): the pid namespace it shows and the detached
/// proc tree, its masks and read-only paths on it.
pub struct PreparedProc {
    pub pid_namespace: OwnedFd,
    pub tree: OwnedFd,
}

/// Makes a pid namespace and its proc ahead of a create, on the node's refill worker, with the masks and
/// read-only paths `key` puts beneath `/proc` already on it, so the init attaches the whole with one
/// `move_mount` and makes no superblock (`goal6-mntns.md` §7).
/// - The pid namespace: `unshare(CLONE_NEWPID)` changes only this thread's children's namespace
///   (`copy_pid_ns`, v7.1 `kernel/pid_namespace.c:175-183`), which is opened and then set back to the
///   thread's own, so no other thread's and no later child's changes. It is empty until the init is cloned
///   into it, its pid 1.
/// - Proc for it by `pidns=` (`fs/proc/root.c:48`, `:115-160`), with the spec's options and flags.
/// - Each mask and read-only path as the pruned init makes it ([`mask_path_pruned`],
///   [`readonly_path_pruned`]), on the detached proc: a missing one skipped as there, a directory masked by a
///   clone of the one empty read-only tmpfs the node keeps ([`ProcSources`]), so no mask makes a superblock, a
///   file by the host's `/dev/null` (made private here, as runc and crun bind it), a read-only path by its own
///   clone made read-only. A target reached through a symbolic link is
///   refused, so the create keeps ordinary construction, which resolves it in the container's root.
pub fn prepare_proc(
    key: &super::procmount::Key,
    sources: &ProcSources,
) -> Result<PreparedProc, OciError> {
    let own = rustix::fs::open(
        "/proc/thread-self/ns/pid",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the worker's pid namespace", e))?;
    sys::unshare(UnshareFlags::NEWPID)?;
    let made = rustix::fs::open(
        "/proc/thread-self/ns/pid_for_children",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os(PID_NAMESPACE_AHEAD, e));
    sys::setns(own.as_fd(), libc::CLONE_NEWPID)?;
    prepare_proc_for(made?, key, sources)
}

/// The proc for `pid_namespace`, with the masks and read-only paths `key` puts beneath `/proc` on it, as
/// [`prepare_proc`] makes it for a pid namespace made ahead. Also for the live pid namespace of an init parked ahead
/// (decision 8, "Proposal: a parked init with its proc made ahead"): `pidns=` takes any pid namespace the mounter
/// may administer and descends from, whether or not a process is in it (v6.19 `fs/proc/root.c:115-160`).
pub fn prepare_proc_for(
    pid_namespace: OwnedFd,
    key: &super::procmount::Key,
    sources: &ProcSources,
) -> Result<PreparedProc, OciError> {
    let null = sources.null.as_fd();
    let fs = rustix::mount::fsopen("proc", FsOpenFlags::FSOPEN_CLOEXEC)
        .map_err(|e| OciError::os("fsopen", e))?;
    rustix::mount::fsconfig_set_string(fs.as_fd(), "source", "proc")
        .map_err(|e| OciError::os("a filesystem's source", e))?;
    rustix::mount::fsconfig_set_fd(fs.as_fd(), "pidns", pid_namespace.as_fd())
        .map_err(|e| OciError::os("proc's pid namespace", e))?;
    for opt in key.data.split(',').filter(|s| !s.is_empty()) {
        match opt.split_once('=') {
            Some((k, v)) => rustix::mount::fsconfig_set_string(fs.as_fd(), k, v)
                .map_err(|e| OciError::os("a filesystem option", format!("{opt}: {e}")))?,
            None => rustix::mount::fsconfig_set_flag(fs.as_fd(), opt)
                .map_err(|e| OciError::os("a filesystem flag", format!("{opt}: {e}")))?,
        }
    }
    rustix::mount::fsconfig_create(fs.as_fd()).map_err(|e| OciError::os("fsconfig create", e))?;
    let tree = rustix::mount::fsmount(
        fs.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        attr_flags(key.flags),
    )
    .map_err(|e| OciError::os("fsmount", e))?;

    let within = |rel: &str| -> Result<Option<OwnedFd>, OciError> {
        match rustix::fs::openat2(
            &tree,
            rel,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            rustix::fs::ResolveFlags::BENEATH
                | rustix::fs::ResolveFlags::NO_SYMLINKS
                | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => Ok(Some(fd)),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(e) => Err(OciError::os(
                "a prepared proc's target",
                format!("{rel}: {e}"),
            )),
        }
    };
    for rel in &key.masked {
        let Some(target) = within(rel)? else {
            continue;
        };
        let is_dir = rustix::fs::fstat(&target)
            .map(|st| rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir())
            .map_err(|e| OciError::os("masking a path", format!("{rel}: {e}")))?;
        let mount = if is_dir {
            match &sources.empty {
                Some(empty) => rustix::mount::open_tree(
                    empty,
                    "",
                    OpenTreeFlags::OPEN_TREE_CLONE
                        | OpenTreeFlags::AT_EMPTY_PATH
                        | OpenTreeFlags::OPEN_TREE_CLOEXEC,
                )
                .map_err(|e| OciError::os("masking a directory", format!("{rel}: {e}")))?,
                None => make_fs("tmpfs", "tmpfs", EMPTY_TMPFS, MountFlags::RDONLY, None)
                    .map_err(|e| OciError::os("masking a directory", format!("{rel}: {e}")))?,
            }
        } else {
            let clone = rustix::mount::open_tree(
                null,
                "",
                OpenTreeFlags::OPEN_TREE_CLONE
                    | OpenTreeFlags::AT_EMPTY_PATH
                    | OpenTreeFlags::OPEN_TREE_CLOEXEC,
            )
            .map_err(|e| OciError::os("masking a file", format!("{rel}: {e}")))?;
            sys::mount_setattr(
                clone.as_fd(),
                false,
                &sys::MountAttrs {
                    set: 0,
                    clr: 0,
                    propagation: sys::MS_PRIVATE,
                },
            )
            .map_err(|e| OciError::os("masking a file", format!("{rel}: {e}")))?;
            clone
        };
        rustix::mount::move_mount(mount.as_fd(), "", target.as_fd(), "", FD_TO_FD)
            .map_err(|e| OciError::os("masking a path", format!("{rel}: {e}")))?;
    }
    for rel in &key.readonly {
        let Some(target) = within(rel)? else {
            continue;
        };
        let clone = rustix::mount::open_tree(
            target.as_fd(),
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_EMPTY_PATH
                | OpenTreeFlags::AT_RECURSIVE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )
        .map_err(|e| OciError::os("a read-only path", format!("{rel}: {e}")))?;
        sys::mount_setattr(
            clone.as_fd(),
            true,
            &sys::MountAttrs {
                set: sys::MOUNT_ATTR_RDONLY,
                clr: 0,
                propagation: 0,
            },
        )
        .map_err(|e| OciError::os("a read-only path", format!("{rel}: {e}")))?;
        rustix::mount::move_mount(clone.as_fd(), "", target.as_fd(), "", FD_TO_FD)
            .map_err(|e| OciError::os("a read-only path", format!("{rel}: {e}")))?;
    }
    Ok(PreparedProc {
        pid_namespace,
        tree,
    })
}

/// Makes a fresh detached tmpfs `/dev` with defaults, before any create can see it. Each node gets
/// its exact mode through chmod: the worker must never change the multi-threaded agent's umask.
pub fn prepare_dev(source: &str, data: &str, flags: MountFlags) -> Result<OwnedFd, OciError> {
    let tree = make_fs(source, "tmpfs", data, flags, None)?;
    for &(name, major, minor, mode) in &DEFAULT_DEVICES {
        make_node(
            &tree,
            name,
            rustix::fs::FileType::CharacterDevice,
            mode,
            major,
            minor,
        )?;
    }
    for &(source, name) in &DEFAULT_LINKS {
        rustix::fs::symlinkat(source, &tree, name)
            .map_err(|e| OciError::os("preparing a /dev link", e))?;
    }
    Ok(tree)
}

pub(crate) fn default_dev_name(name: &str) -> bool {
    DEFAULT_DEVICES.iter().any(|&(n, ..)| n == name)
        || DEFAULT_LINKS.iter().any(|&(_, n)| n == name)
}

/// A later mount might depend on defaults which ordinary setup has not made yet. Earlier guarded
/// mounts have touched none of these names, so remove the pristine defaults, then run ordinary setup.
fn clear_prepared_dev(tree: &OwnedFd) -> Result<(), OciError> {
    for name in DEFAULT_DEVICES
        .iter()
        .map(|&(n, ..)| n)
        .chain(DEFAULT_LINKS.iter().map(|&(_, n)| n))
    {
        rustix::fs::unlinkat(tree, name, rustix::fs::AtFlags::empty())
            .map_err(|e| OciError::os("restoring /dev mount ordering", e))?;
    }
    Ok(())
}

/// The container's `/dev`: the default devices (OCI spec §Default Devices), the spec's own, then the
/// default links (§Dev symbolic links). `/dev` is opened once, made when missing, for the defaults,
/// which go directly in it. Each default is made first and looked for only when the kernel refuses it
/// as there (`EEXIST`), not looked for before each: those looks and their walks were most of the
/// init's calls in goal 5 plan step 3's trace (`docs/research/runtime/goal5-mounts.md`).
fn make_dev(root: &inroot::Root<'_>, spec: &Spec, in_userns: bool) -> Result<(), OciError> {
    if in_userns {
        // mknod is refused in a user namespace, so the host's nodes are bound in, each where none is.
        for (name, ..) in DEFAULT_DEVICES {
            let path = format!("/dev/{name}");
            if root.open(&path, Make::Nothing)?.is_none() {
                bind_host_device(root, &path, &path)?;
            }
        }
    }
    let dev = target(root, "/dev", Make::Dir)?;
    if !in_userns {
        make_default_devices(root, &dev)?;
    }
    for device in &spec.linux.devices {
        make_device(root, device, in_userns)?;
    }
    make_default_links(root, &dev)
}

/// The default devices made in `/dev` (`dev`) with `mknodat`, each with its mode exactly: the umask is
/// cleared for their making and put back after, in place of a `chmod` each.
fn make_default_devices(root: &inroot::Root<'_>, dev: &OwnedFd) -> Result<(), OciError> {
    let umask = rustix::process::umask(Mode::empty());
    let made = DEFAULT_DEVICES
        .iter()
        .try_for_each(|&(name, major, minor, mode)| {
            default_device(root, dev, name, major, minor, mode)
        });
    rustix::process::umask(umask);
    made
}

/// One default device, `name` in `/dev` (`dev`). One the kernel refuses to make is then looked for, as
/// each was looked for before it was made, so the outcome is the look's: a name that leads to something
/// in the root is left as it is, whatever it is; one whose look is refused (a magic link, a file taken
/// for a directory) is refused as the look refuses it; and one that leads to nothing (a link to nothing
/// inside the root), or a node the kernel won't make, is refused, naming it.
fn default_device(
    root: &inroot::Root<'_>,
    dev: &OwnedFd,
    name: &str,
    major: u32,
    minor: u32,
    mode: u32,
) -> Result<(), OciError> {
    let made = rustix::fs::mknodat(
        dev,
        name,
        rustix::fs::FileType::CharacterDevice,
        Mode::from_bits_truncate(mode),
        rustix::fs::makedev(major, minor),
    );
    match made {
        Ok(()) => Ok(()),
        Err(e) => match root.open(&format!("/dev/{name}"), Make::Nothing)? {
            Some(_) => Ok(()),
            None => Err(OciError::os("making a device", format!("/dev/{name}: {e}"))),
        },
    }
}

fn make_device(root: &inroot::Root<'_>, dev: &Device, in_userns: bool) -> Result<(), OciError> {
    if root.open(&dev.path, Make::Nothing)?.is_some() {
        return Ok(());
    }
    if in_userns {
        return bind_host_device(root, &dev.path, &dev.path);
    }
    let kind = match dev.kind.as_str() {
        "c" | "u" => rustix::fs::FileType::CharacterDevice,
        "b" => rustix::fs::FileType::BlockDevice,
        "p" => rustix::fs::FileType::Fifo,
        other => {
            return Err(OciError::Config(format!(
                "device type {other:?} isn't c, b, u or p"
            )));
        }
    };
    let major = u32::try_from(dev.major.unwrap_or(0))
        .map_err(|_| OciError::Config("device major out of range".to_owned()))?;
    let minor = u32::try_from(dev.minor.unwrap_or(0))
        .map_err(|_| OciError::Config("device minor out of range".to_owned()))?;
    let (dir, name) = root.parent(&dev.path)?;
    make_node(
        &dir,
        &name,
        kind,
        dev.file_mode.unwrap_or(0o666),
        major,
        minor,
    )?;
    if let (Some(uid), Some(gid)) = (dev.uid, dev.gid) {
        rustix::fs::chownat(
            &dir,
            name.as_str(),
            Some(rustix::fs::Uid::from_raw(uid)),
            Some(rustix::fs::Gid::from_raw(gid)),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| OciError::os("chowning a device", e))?;
    }
    Ok(())
}

/// Makes a device node `name` in the directory `dir`, then sets its mode exactly (mknod's mode is
/// masked by the umask).
fn make_node(
    dir: &OwnedFd,
    name: &str,
    kind: rustix::fs::FileType,
    mode: u32,
    major: u32,
    minor: u32,
) -> Result<(), OciError> {
    rustix::fs::mknodat(
        dir,
        name,
        kind,
        Mode::from_bits_truncate(mode),
        rustix::fs::makedev(major, minor),
    )
    .map_err(|e| OciError::os("making a device", e))?;
    rustix::fs::chmodat(
        dir,
        name,
        Mode::from_bits_truncate(mode),
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|e| OciError::os("setting a device's mode", e))?;
    Ok(())
}

/// Binds the host's device file `host_path` at `dest` in the container (the user-namespace path,
/// where mknod is refused).
fn bind_host_device(root: &inroot::Root<'_>, host_path: &str, dest: &str) -> Result<(), OciError> {
    if !Path::new(host_path).exists() {
        return Ok(());
    }
    let target_fd = target(root, dest, Make::File)?;
    rustix::mount::mount_bind(host_path, inroot::fd_path(&target_fd))
        .map_err(|e| OciError::os("binding a host device", e))
}

/// The default links made in `/dev` (`dev`). One the kernel refuses to make is then looked for, as each
/// was looked for before it was made, so the outcome is the look's: a name there is left as it is,
/// whatever it leads to or a link to what isn't mounted yet; one whose look is refused (a magic link, a
/// file taken for a directory) is refused as the look refuses it; and a link the kernel won't make where
/// nothing is, is refused, naming it.
fn make_default_links(root: &inroot::Root<'_>, dev: &OwnedFd) -> Result<(), OciError> {
    for (target, name) in DEFAULT_LINKS {
        if let Err(e) = rustix::fs::symlinkat(target, dev, name) {
            let there = root.open(&format!("/dev/{name}"), Make::Nothing)?.is_some();
            if !there && e != rustix::io::Errno::EXIST {
                return Err(OciError::os(
                    "making a /dev link",
                    format!("/dev/{name}: {e}"),
                ));
            }
        }
    }
    Ok(())
}

/// A masked path is hidden: a directory gets a read-only empty tmpfs, a file a bind of /dev/null.
fn mask_path(
    root: &inroot::Root<'_>,
    path: &str,
    masked_directory: Option<&OwnedFd>,
    label: Option<&str>,
) -> Result<(), OciError> {
    let Some(target) = root.open(path, Make::Nothing)? else {
        return Ok(());
    };
    let dir = rustix::fs::fstat(&target)
        .map(|st| rustix::fs::FileType::from_raw_mode(st.st_mode).is_dir())
        .map_err(|e| OciError::os("masking a path", format!("{path}: {e}")))?;
    let at = inroot::fd_path(&target);
    if dir {
        match masked_directory {
            Some(empty) => clone_masked_directory(empty.as_fd(), target.as_fd())
                .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?,
            None => {
                let mount = make_fs("tmpfs", "tmpfs", EMPTY_TMPFS, MountFlags::RDONLY, label)
                    .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?;
                rustix::mount::move_mount(mount.as_fd(), "", target.as_fd(), "", FD_TO_FD)
                    .map_err(|e| OciError::os("masking a directory", format!("{path}: {e}")))?;
            }
        }
    } else {
        rustix::mount::mount_bind("/dev/null", &at)
            .map_err(|e| OciError::os("masking a file", format!("{path}: {e}")))?;
    }
    Ok(())
}

/// A readonly path is bound onto itself and remounted read-only.
fn readonly_path(root: &inroot::Root<'_>, path: &str) -> Result<(), OciError> {
    let Some(target) = root.open(path, Make::Nothing)? else {
        return Ok(());
    };
    let at = inroot::fd_path(&target);
    rustix::mount::mount_bind(&at, &at)
        .map_err(|e| OciError::os("binding a readonly path", format!("{path}: {e}")))?;
    // Opened again, the path is the bind just made on top of it.
    let top_fd = self::target(root, path, Make::Nothing)?;
    rustix::mount::mount_remount(
        inroot::fd_path(&top_fd),
        MountFlags::RDONLY | MountFlags::BIND,
        "",
    )
    .map_err(|e| OciError::os("remounting a readonly path", format!("{path}: {e}")))?;
    Ok(())
}

fn apply_sysctls(spec: &Spec) -> Result<(), OciError> {
    for (key, value) in &spec.linux.sysctl {
        let path = Path::new("/proc/sys").join(key.replace('.', "/"));
        fs::write(&path, value).map_err(|e| OciError::os("writing a sysctl", e))?;
    }
    Ok(())
}

fn env_var(name: &str) -> Result<String, OciError> {
    std::env::var(name)
        .map_err(|_| OciError::Broken(format!("{name} not set for the container init")))
}

/// A pipe end the runtime holds, named by `name` as the path of the runtime's descriptor for it
/// (`/proc/<pid>/fd/<n>`), which stays close-on-exec there: the runtime never clears the flag for the
/// spawn, so no process another of its threads starts meanwhile inherits it. Opened close-on-exec here, so
/// the program the container execs never holds it (crun's holds only its stdio, and so does Hex's).
fn handed_fd(name: &str, flags: OFlags) -> Result<OwnedFd, OciError> {
    let path = env_var(name)?;
    rustix::fs::open(path.as_str(), flags | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| OciError::os("opening a descriptor the runtime holds", e))
}

/// Writes one newline-terminated sync message; a broken pipe is ignored (the runtime has gone).
pub(crate) fn msg(sync: BorrowedFd<'_>, text: &str) {
    let line = format!("{text}\n");
    let _ = rustix::io::write(sync, line.as_bytes());
}

#[cfg(test)]
mod tests {
    //! Hostile images' `/dev` against the init's making of it (goal 5 plan step 3, T-T7.1.4; CLAUDE.md
    //! §6): each default node and link is made first and looked for only when the kernel refuses its
    //! name as there, and every outcome is the one the look before each gave: what's there is left as
    //! it is, a name that leads out of the root or whose look is refused is refused, naming it. Run
    //! unprivileged: with every name there, `mknodat` refuses on `EEXIST` before it asks for
    //! `CAP_MKNOD`, so no node is ever made here.
    use super::*;
    use std::os::unix::fs::symlink;

    /// HEX-IPCUTS-9 (goal 5 extended, step 4): do ask for a container's clone flags with each kind made ahead and
    /// without; expect each made ahead left out, and only those.
    #[test]
    fn the_clone_leaves_out_what_is_made_ahead() {
        let spec = Spec::parse(
            br#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"linux":{"namespaces":[{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"network"}]}}"#,
        )
        .unwrap();
        let flag = |f: i32| u64::try_from(f).unwrap();
        let rest = flag(libc::CLONE_NEWPID) | flag(libc::CLONE_NEWNS);
        let pair = flag(libc::CLONE_NEWIPC) | flag(libc::CLONE_NEWUTS);
        let net = flag(libc::CLONE_NEWNET);
        assert_eq!(fresh_clone_flags(&spec, false, false), rest | pair | net);
        assert_eq!(fresh_clone_flags(&spec, true, false), rest | pair);
        assert_eq!(fresh_clone_flags(&spec, false, true), rest | net);
        assert_eq!(fresh_clone_flags(&spec, true, true), rest);
    }

    /// Ada's 2026-11-06 rule (T-T7.1.4, CLAUDE.md §2): do mount a tmpfs with the data of each empty tmpfs Hex makes (a
    /// masked directory's and the pruned seed's root), read-only, in a user and mount namespace of
    /// its own (unprivileged); expect its size bounded at one block, a page (`statfs`'s `f_blocks` 1), never 0, which
    /// is a tmpfs with no bound at all (Documentation/filesystems/tmpfs.rst:94).
    #[test]
    fn each_empty_tmpfs_is_bounded_at_one_page() {
        let point = tempfile::tempdir().unwrap();
        for data in [
            EMPTY_TMPFS,
            super::super::seed::ROOT_TMPFS.to_str().unwrap(),
        ] {
            let out = Command::new("unshare")
                .args(["-Urm", "sh", "-c"])
                .arg(r#"mount -t tmpfs -o "ro,$1" none "$0" && stat -f -c %b "$0""#)
                .arg(point.path())
                .arg(data)
                .output()
                .unwrap();
            assert!(out.status.success(), "{data}: {out:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1", "{data}");
        }
    }

    /// A root holding `/dev`, and the default nodes' names as plain files, so `mknodat` meets each.
    fn dev_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dev")).unwrap();
        for (name, ..) in DEFAULT_DEVICES {
            std::fs::write(root.path().join("dev").join(name), name).unwrap();
        }
        root
    }

    fn with_dev<T>(root: &Path, f: impl FnOnce(&inroot::Root<'_>, &OwnedFd) -> T) -> T {
        let fd = inroot::open_root(root).unwrap();
        let in_root = inroot::Root::new(fd.as_fd());
        let dev = target(&in_root, "/dev", Make::Dir).unwrap();
        f(&in_root, &dev)
    }

    /// Do: an image's `/dev/null` is a link to a host directory, which inside the root leads nowhere.
    /// Expect: refused naming `/dev/null`, and nothing made on the host.
    #[test]
    fn a_device_name_leading_out_of_the_root_is_refused_by_name() {
        let host = tempfile::tempdir().unwrap();
        let root = dev_root();
        std::fs::remove_file(root.path().join("dev/null")).unwrap();
        symlink(host.path().join("null"), root.path().join("dev/null")).unwrap();
        let got = with_dev(root.path(), make_default_devices);
        assert!(
            matches!(&got, Err(OciError::Os { op, why }) if *op == "making a device" && why.starts_with("/dev/null: ")),
            "{got:?}"
        );
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
    }

    /// Do: an image's `/dev` already holds each default node's name, as a file, a directory, a link to a
    /// file inside the root. Expect: each is left as it is, whatever it is, as the look before each left it.
    #[test]
    fn a_device_name_already_there_is_left_as_it_is() {
        let root = dev_root();
        std::fs::remove_file(root.path().join("dev/zero")).unwrap();
        std::fs::create_dir(root.path().join("dev/zero")).unwrap();
        std::fs::write(root.path().join("real-full"), "inside").unwrap();
        std::fs::remove_file(root.path().join("dev/full")).unwrap();
        symlink("/real-full", root.path().join("dev/full")).unwrap();
        with_dev(root.path(), make_default_devices).unwrap();
        let dev = root.path().join("dev");
        assert_eq!(std::fs::read_to_string(dev.join("null")).unwrap(), "null");
        assert!(dev.join("zero").is_dir());
        assert_eq!(
            std::fs::read_link(dev.join("full")).unwrap(),
            Path::new("/real-full")
        );
        assert_eq!(std::fs::read_to_string(dev.join("tty")).unwrap(), "tty");
    }

    /// Do: an image's `/dev/null` is a link through a file, as though it were a directory. Expect: its
    /// look is refused, naming `/dev/null` and the component the walk was refused at.
    #[test]
    fn a_device_name_whose_look_is_refused_is_refused_by_name() {
        let root = dev_root();
        std::fs::remove_file(root.path().join("dev/null")).unwrap();
        symlink("zero/x", root.path().join("dev/null")).unwrap();
        let got = with_dev(root.path(), make_default_devices);
        assert!(
            matches!(&got, Err(OciError::Os { why, .. }) if why.starts_with("/dev/null (dev/null): ")),
            "{got:?}"
        );
    }

    /// Do: an image's `/dev` is a file. Expect: the first default node is refused as the look before it
    /// refused it, naming `/dev/null` and the component the walk was refused at.
    #[test]
    fn a_dev_that_is_a_file_is_refused_as_the_look_refused_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("dev"), "a file").unwrap();
        let got = with_dev(root.path(), make_default_devices);
        assert!(
            matches!(&got, Err(OciError::Os { op, why }) if *op == "resolving a path in the container's root" && why.starts_with("/dev/null (dev/null): ")),
            "{got:?}"
        );
        let links = with_dev(root.path(), make_default_links);
        assert!(
            matches!(&links, Err(OciError::Os { why, .. }) if why.starts_with("/dev/fd (dev/fd): ")),
            "{links:?}"
        );
    }

    /// Do: make the default links in an empty `/dev`. Expect: each points where the OCI spec says.
    #[test]
    fn the_default_links_are_made_as_the_spec_names_them() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dev")).unwrap();
        with_dev(root.path(), make_default_links).unwrap();
        for (target, name) in DEFAULT_LINKS {
            assert_eq!(
                std::fs::read_link(root.path().join("dev").join(name)).unwrap(),
                Path::new(target),
                "/dev/{name}"
            );
        }
    }

    /// Do: an image's `/dev` already holds each link's name: a directory, a file, a link to nothing yet,
    /// a link to a file inside the root. Expect: each is left as it is.
    #[test]
    fn a_link_name_already_there_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        std::fs::create_dir(&dev).unwrap();
        std::fs::create_dir(dev.join("fd")).unwrap();
        std::fs::write(dev.join("stdin"), "a file").unwrap();
        symlink("/proc/self/fd/1", dev.join("stdout")).unwrap();
        std::fs::write(root.path().join("real-err"), "").unwrap();
        symlink("/real-err", dev.join("stderr")).unwrap();
        with_dev(root.path(), make_default_links).unwrap();
        assert!(dev.join("fd").is_dir());
        assert_eq!(
            std::fs::read_to_string(dev.join("stdin")).unwrap(),
            "a file"
        );
        assert_eq!(
            std::fs::read_link(dev.join("stdout")).unwrap(),
            Path::new("/proc/self/fd/1")
        );
        assert_eq!(
            std::fs::read_link(dev.join("stderr")).unwrap(),
            Path::new("/real-err")
        );
        assert_eq!(
            std::fs::read_link(dev.join("ptmx")).unwrap(),
            Path::new("pts/ptmx")
        );
    }

    /// Do: an image's `/dev/stdin` is a link through a file. Expect: its look is refused, naming it.
    #[test]
    fn a_link_name_whose_look_is_refused_is_refused_by_name() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        std::fs::create_dir(&dev).unwrap();
        std::fs::write(dev.join("file"), "").unwrap();
        symlink("file/x", dev.join("stdin")).unwrap();
        let got = with_dev(root.path(), make_default_links);
        assert!(
            matches!(&got, Err(OciError::Os { why, .. }) if why.starts_with("/dev/stdin (dev/stdin): ")),
            "{got:?}"
        );
    }

    /// Do: list the cgroup filesystems beneath the host's `/sys` both ways, from the kernel (`listmount`
    /// and `statmount`, Linux 6.8) and from the mount table, as a recursive read-only bind of it has them
    /// remounted (goal 5 plan step 3, T-T7.1.4). Expect: the same paths under `/sys`, `fs/cgroup` among
    /// them where `/sys/fs/cgroup` is a cgroup filesystem, and the kernel's listing made (this host's
    /// kernel has the calls), so the two can't agree by both finding nothing.
    #[test]
    fn the_kernel_lists_the_cgroup_filesystems_beneath_a_bind_as_the_table_does() {
        let sys = rustix::fs::open(
            "/sys",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let mut listed = cgroupfs_listed(&sys, "/sys")
            .unwrap()
            .expect("this host's kernel has listmount and statmount (Linux 6.8)");
        let mut table = cgroupfs_in_table(&sys, "/sys").unwrap();
        listed.sort();
        table.sort();
        assert_eq!(listed, table);
        // CGROUP2_SUPER_MAGIC (include/uapi/linux/magic.h).
        let cgroup2 = rustix::fs::statfs("/sys/fs/cgroup")
            .is_ok_and(|st| u64::try_from(st.f_type).ok() == Some(0x6367_7270));
        if cgroup2 {
            assert!(listed.iter().any(|p| p == "fs/cgroup"), "{listed:?}");
        }
    }

    /// Do: read the host's `/sys` mount point through `statmount` into a buffer that holds the fixed part
    /// alone, which the kernel answers `EOVERFLOW` (fs/namespace.c:5682-5683) (goal 5 plan step 3,
    /// T-T7.1.4). Expect: the buffer grown to its bound and the point read whole, `/sys`, not refused.
    #[test]
    fn a_mount_point_past_the_first_buffer_is_read_whole() {
        let sys = rustix::fs::open(
            "/sys",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let unique = rustix::fs::StatxFlags::from_bits_retain(libc::STATX_MNT_ID_UNIQUE);
        let stx = rustix::fs::statx(&sys, "", rustix::fs::AtFlags::EMPTY_PATH, unique).unwrap();
        let mut buf = vec![0u8; STATMOUNT_FIXED];
        let got = mount_info(stx.stx_mnt_id, STATMOUNT_MNT_POINT, &mut buf)
            .unwrap()
            .expect("this host's kernel has statmount (Linux 6.8)");
        assert_eq!(got, (String::new(), "/sys".to_owned()));
        assert_eq!(buf.len(), STATMOUNT_BUF);
    }

    /// A mount table's line for a cgroup2 filesystem at `point`, as the kernel writes one (proc_pid_mountinfo(5)),
    /// its filesystem's own options `options`.
    fn cgroup_line(id: usize, point: &str, options: &str) -> String {
        format!("{id} 25 0:31 / {point} rw,nosuid,nodev shared:9 - cgroup2 cgroup2 rw,{options}\n")
    }

    /// Ada's 2026-11-06 rule (goal 5 plan step 3, T-T7.1.4; CLAUDE.md §2): do read a mount table holding 300 cgroup
    /// filesystems beneath a bind's top (past the 256 once picked as their bound), and one whose lines carry 90 KiB
    /// of a filesystem's own options each, 17 MiB in all (past the 16 MiB once picked as the table's), as an
    /// overlay's long `lowerdir` makes. Expect every cgroup filesystem found in both, the namespace's own mount
    /// cap the one bound on lines, and a line's length bounded by the kernel's alone.
    #[test]
    fn a_mount_table_is_bounded_by_the_namespaces_mount_cap_alone() {
        let top = Path::new("/sys");
        let table: String = (0..300)
            .map(|i| cgroup_line(i, &format!("/sys/fs/cgroup/c{i}"), "nsdelegate"))
            .collect();
        let found = cgroupfs_in_mountinfo(table.as_bytes(), top, "/sys").unwrap();
        assert_eq!(found.len(), 300);
        assert_eq!(found[299], "fs/cgroup/c299");
        let long = format!("lowerdir={}", "/l".repeat(45 * 1024));
        let mut table: String = (0..200)
            .map(|i| format!("{i} 25 0:40 / /o{i} rw - overlay overlay rw,{long}\n"))
            .collect();
        table.push_str(&cgroup_line(201, "/sys/fs/cgroup", "nsdelegate"));
        assert!(table.len() > 17 << 20);
        let found = cgroupfs_in_mountinfo(table.as_bytes(), top, "/sys").unwrap();
        assert_eq!(found, ["fs/cgroup"]);
    }

    /// Ada's 2026-11-06 rule (T-T7.1.4): do read the namespace's mount cap twice; expect this host's
    /// `fs.mount-max` both times, the bound learned from the kernel, not picked.
    #[test]
    fn the_mount_cap_is_the_hosts_own() {
        let host: u64 = std::fs::read_to_string("/proc/sys/fs/mount-max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(mount_max().unwrap(), host);
        assert_eq!(mount_max().unwrap(), host);
    }

    /// CLAUDE.md §2, §3: do read a mount table whose cgroup filesystem beneath the bind has a mount point past what
    /// `openat2` takes (`PATH_MAX`), so no remount could reach it, and one whose point holds a space (`\040`).
    /// Expect the first refused, naming the bind, and the second found, its point decoded.
    #[test]
    fn a_cgroup_filesystem_past_path_max_is_refused_by_name() {
        let top = Path::new("/sys");
        let deep = format!("/sys/fs/cgroup/{}", "d/".repeat(4096));
        let refused = cgroupfs_in_mountinfo(cgroup_line(1, &deep, "").as_bytes(), top, "/sys");
        assert!(
            matches!(&refused, Err(OciError::Config(why)) if why.contains("/sys")),
            "{refused:?}"
        );
        let spaced = cgroup_line(1, "/sys/fs/cgroup/a\\040b", "");
        let found = cgroupfs_in_mountinfo(spaced.as_bytes(), top, "/sys").unwrap();
        assert_eq!(found, ["fs/cgroup/a b"]);
    }

    /// Do: a mount's target is a link to a host directory, or a path through a file. Expect: a target
    /// the mount needs made is refused naming the link that leads nowhere inside the root, and nothing is
    /// made on the host; a target through a file is refused naming the component it was refused at, as
    /// the walk named it before one call resolved a target that's there.
    #[test]
    fn a_mount_target_out_of_the_root_or_through_a_file_is_refused_by_name() {
        let host = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        symlink(host.path(), root.path().join("mnt")).unwrap();
        std::fs::write(root.path().join("file"), "").unwrap();
        let fd = inroot::open_root(root.path()).unwrap();
        let in_root = inroot::Root::new(fd.as_fd());
        let out = target(&in_root, "/mnt/data", Make::Dir);
        assert!(
            matches!(&out, Err(OciError::Config(why)) if why.starts_with("/mnt/data: mnt ")),
            "{out:?}"
        );
        assert_eq!(std::fs::read_dir(host.path()).unwrap().count(), 0);
        let through = target(&in_root, "/file/data", Make::Dir);
        assert!(
            matches!(&through, Err(OciError::Os { why, .. }) if why.starts_with("/file/data (file/data): ")),
            "{through:?}"
        );
    }

    /// T-T7.1.2 (CLAUDE.md §3): the init's end when its start is gone. Do: write the start's byte on a
    /// pipe whose reader has closed, in a process with SIGPIPE ignored as every `hex` process has it. Expect:
    /// `Abandoned`, the EPIPE taken, never the signal; and on a pipe with its reader, `Taken`.
    #[test]
    fn a_start_whose_reader_is_gone_is_abandoned_not_a_sigpipe() {
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        assert_eq!(take_start(&w).unwrap(), Taken::Taken);
        drop(r);
        assert_eq!(take_start(&w).unwrap(), Taken::Abandoned);
    }
}
