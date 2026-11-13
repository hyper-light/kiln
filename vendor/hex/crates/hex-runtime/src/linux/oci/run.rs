//! The OCI lifecycle the runtime-tools suite drives: `create`, `start`, `state`, `kill`, `delete`.
//! `create` makes the container's cgroup with its resources in force, starts the shim
//! ([`super::init`]), runs the sync protocol (user-namespace maps, the container pid, the ready
//! signal), and writes the state; the container then blocks until `start` opens the exec FIFO. The
//! shim exits once it has forked the container, so nothing of Hex's stays beside it, and `stopped` is
//! never written: it is a recorded init that no longer runs ([`State::init_alive`]; decision 2 of
//! docs/design/runtime-architecture.md). `delete` kills what is left in the cgroup, waits for it to
//! empty and removes it.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::mount::OpenTreeFlags;
use rustix::pipe::PipeFlags;
use rustix::process::PidfdFlags;

use crate::linux::cgroup::{BoxCgroup, CGROUP_ROOT, write_file};
use crate::linux::oci::checked;
use crate::linux::oci::programs::{self, Programs};
use crate::linux::oci::resources::{self, Insn, Plan};
use crate::linux::oci::spec::{IdMapping, Resources, Spec};
use crate::linux::oci::state::{self, Dir, State, Status};
use crate::linux::oci::{OciError, devshare, sys};
use crate::linux::oci::{hooks, init};

/// The controllers a container's cgroup gets, beyond those its resources need, where its parent has
/// them: what Hex's boxes get ([`hex_core::cgroup::CONTROLLERS`]), so a container's memory, CPU, IO
/// and pids are accounted whether or not it is limited (the spec lets a runtime attach more
/// controllers than its resources need, config-linux.md "Cgroups Path").
const DEFAULT_CONTROLLERS: [&str; 4] = hex_core::cgroup::CONTROLLERS;

/// The longest container id: a file name's limit (`NAME_MAX`, 255 bytes), since the id names the
/// container's state directory.
pub(crate) const MAX_ID_BYTES: usize = 255;

/// Refuses an id that isn't a plain name, before anything is touched. The id names the container's
/// state directory and its default cgroup, so a `/` or `..` would reach outside the state root, and
/// `delete` would remove whatever directory it reached. Ids are `[A-Za-z0-9_+.-]`, as runc takes
/// them, and neither `.` nor `..`.
fn check_id(id: &str) -> Result<(), OciError> {
    let plain = !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-'));
    if plain {
        Ok(())
    } else {
        Err(OciError::Config(format!(
            "container id {id:?} isn't a plain name of [A-Za-z0-9_+.-]"
        )))
    }
}

fn from_refusal(r: Refusal) -> OciError {
    match r {
        Refusal::Os { op, why } => OciError::Os { op, why },
        other => OciError::Os {
            op: "cgroup",
            why: other.to_string(),
        },
    }
}

/// The OCI runtime: containers under one state root.
pub struct Runtime {
    root: PathBuf,
    /// The binary re-exec'd as `hex __oci-init` (the shim) and `hex __oci-exec` (the exec helper).
    /// `None` is this process's own binary, as the `hex oci` command line uses: it handles those
    /// subcommands. A library caller whose binary doesn't — the node agent linked into another
    /// program, the bench — names the `hex` binary explicitly ([`Runtime::with_helper`]).
    helper: Option<PathBuf>,
    /// What a create does with a label whose module the host's kernel doesn't run ([`Runtime::with_lsm`]).
    lsm: super::lsm::Policy,
}

/// One container's create as its shim is started: its id, bundle, state directory and the pid file
/// asked for.
#[derive(Clone, Copy)]
struct Making<'a> {
    id: &'a str,
    bundle: &'a Path,
    dir: &'a Dir,
    pid_file: Option<&'a Path>,
    /// The `config.json` bytes the runtime parsed and checked, sealed ([`checked::seal`]): what the shim or a
    /// seed makes the container from, never a later read of the bundle (AC-T7.1.1).
    config: BorrowedFd<'a>,
}

/// The network namespaces a node agent makes ahead (decision 4's pool), which a create asks once it
/// has read the spec, for a container that makes a network namespace of its own: [`take`](Self::take)
/// for one in this process's user namespace, [`on_path`](Self::on_path) for one in a user namespace
/// of its own. Each answer is the pool's to count, and a refusal refuses the create.
pub trait Networks {
    /// A namespace made ahead, its loopback up, now the container's: the caller keeps no copy. `None`
    /// when there is none to give, and the shim makes the container's on the create's path.
    fn take(&self) -> Result<Option<OwnedFd>, OciError>;

    /// A container whose network namespace a namespace made ahead can't be, since its user namespace
    /// isn't this process's and a namespace belongs to its maker's (user_namespaces(7)): its shim makes
    /// it on the create's path.
    fn on_path(&self) -> Result<(), OciError>;

    /// A namespace for a container whose spec says whether it may share its owner's
    /// ([`Spec::may_share_its_network`]): a node that keeps its owners' namespaces answers with one of
    /// them, or one it keeps for the owner (goal 5 extended, step 6). By default, as [`take`](Self::take).
    fn take_for(
        &self,
        may_share: Result<(), hex_core::netns_reuse::Refused>,
    ) -> Result<Option<OwnedFd>, OciError> {
        let _ = may_share;
        self.take()
    }
}

/// A pool of fresh tmpfs `/dev` mounts, with defaults made before a create takes one. A dry pool
/// returns None; the init then uses ordinary setup. A taken mount is never returned or shared.
pub trait DevMounts {
    fn take(&self, key: super::devmount::Key) -> Result<Option<OwnedFd>, OciError>;
    fn unpooled(&self, reason: hex_core::devfs::Unpooled) -> Result<(), OciError>;
    /// A proc made ahead for `key`, with its pid namespace (decision 6's update, part 2), or `None` when
    /// none is ready: the create makes its own, counted.
    fn take_proc(&self, key: super::procmount::Key)
    -> Result<Option<init::PreparedProc>, OciError>;
    /// A create no prepared proc can serve, counted by `reason`.
    fn unpooled_proc(&self, reason: hex_core::devfs::Unpooled) -> Result<(), OciError>;
    /// The node's one empty read-only tmpfs, which the init clones for each masked directory in place of a tmpfs
    /// of its own (decision 6's update): `None` where the node keeps none.
    fn masked_directory(&self) -> Option<BorrowedFd<'_>>;
}

/// The ipc and uts namespaces a node agent makes ahead (goal 5 extended, step 4), which a create routed to a
/// pruned seed asks for.
pub trait IpcUtsAhead {
    /// A pair made ahead, now the container's: the caller keeps no copy. `None` when there is none to give, and
    /// the init's clone makes them.
    fn take(&self) -> Result<Option<super::ipcuts::Pair>, OciError>;

    /// A container that makes either namespace but can't take a pair: its user namespace isn't this process's
    /// and a namespace belongs to its maker's (user_namespaces(7)), or it makes one of the two alone, or its
    /// create has no room for the descriptors. Its clone, or its shim, makes them on the path.
    fn on_path(&self) -> Result<(), OciError>;
}

/// What a node agent holds ready for its creates: the network namespaces it makes ahead (decision 4's
/// pool), asked once the spec is read, and the device programs it keeps loaded, one per policy
/// ([`Programs::attach`]; goal 5 extended, step 1). A create given neither makes its container's
/// network namespace on its path, and finds or loads its device program as the command line does.
#[derive(Clone, Copy, Default)]
pub struct Held<'a> {
    pub networks: Option<&'a dyn Networks>,
    pub devices: Option<&'a Programs>,
    pub dev_mounts: Option<&'a dyn DevMounts>,
    pub ipc_uts: Option<&'a dyn IpcUtsAhead>,
}

/// What a seed's create may take from the node once it knows its route.
struct SeedPools<'a> {
    dev_mounts: Option<&'a dyn DevMounts>,
    ipc_uts: Option<&'a dyn IpcUtsAhead>,
    /// The init parked in a handed-on cgroup, and the caller that chose it, told what the init was made with
    /// (decision 8, "The p99.9 that stayed").
    parked: Option<Parked>,
    chooser: Option<&'a dyn Choose>,
}

/// How a create makes its container: where its cgroup comes from, the network namespaces made ahead it
/// may join one of, the device programs a node holds, and how its init is started.
struct Route<'a> {
    cgroup: Cgroup<'a>,
    networks: Option<&'a dyn Networks>,
    devices: Option<&'a Programs>,
    dev_mounts: Option<&'a dyn DevMounts>,
    ipc_uts: Option<&'a dyn IpcUtsAhead>,
    start: Start<'a>,
}

/// How a create starts the container's init: the command line spawns `hex __oci-init` (the shim,
/// [`Runtime::create`]); the node agent's seam hands a seed the container to clone
/// ([`Runtime::create_in_cgroup_seeded`], decision 5), and the shim only for a spec a seed can't serve.
enum Start<'a> {
    /// Spawn `hex __oci-init`, which unshares the namespaces and forks the container.
    Shim,
    /// Hand a seed the container to clone, in place of that re-exec.
    Seed(&'a dyn CloneInit),
}

/// Where a shim's standard streams come from, and through its fork its container's: the init and the
/// program it execs keep them.
#[derive(Clone, Copy)]
enum ShimStdio {
    /// The caller's, as the command line's `create` passes its own on (runc's pass-through).
    Inherited,
    /// The null device: what a seed-made container's streams are (the seed's own), so a seam container's
    /// streams don't depend on whether a seed or the shim made it, nor carry the node agent's.
    Null,
}

/// What a create made ahead and hands the shim, each by the path of this process's descriptor for it: a network
/// namespace from the node's pool, and a `/dev` tree from its `/dev` pool.
struct Ahead {
    network: Option<OwnedFd>,
    dev: Option<PreparedDev>,
    /// The node's empty read-only tmpfs, which the init clones for each masked directory (decision 6's update).
    masked_directory: Option<OwnedFd>,
}

/// A seed that clones a container's init for the `Runtime` seam (decision 5, goal 5 plan step 4). The
/// node agent implements it over its pool of `hex __oci-seed` processes: in place of the shim's re-exec
/// of the hex binary, a seed clones the init with `clone3` into its cgroup and the fresh namespaces its
/// spec asks for, as the agent's own child, and hands back its pid and a pidfd. No `hex` binary is
/// exec'd per create.
pub trait CloneInit {
    /// Have a seed clone container `dir`'s init ([`CloneReq`]): born in `cgroup`, in the fresh namespaces
    /// its spec asks for and the network namespace made ahead if one is given, writing the sync pipe as
    /// the shim's child does (`R`, or `E` with a reason; `U` for a user namespace, whose maps the runtime
    /// writes). Returns the init's pid in this process's pid namespace and a pidfd for it, the caller's to
    /// own.
    fn clone_init(&self, req: CloneReq<'_>) -> Result<(i32, OwnedFd), OciError>;

    /// Hands the create `req` to the init `parked` in its cgroup (decision 8, "The p99.9 that stayed"), in place
    /// of a clone: the same message a seed takes, on the init's own socket. A claimed init says nothing on that socket
    /// once it takes the claim (the lead's (A) of 2026-11-09: no early answer), so the claim is taken at its first word
    /// on `sync`, the read end of its sync pipe, which stays there for the handshake to read, and fails on its refusal or
    /// its end on the socket, or on its silence past the claim's patience. Returns the parked init's pid and pidfd, the
    /// park's, as [`CloneInit::clone_init`] returns a clone's, or why not, after which the parked init is gone and
    /// `req` unused.
    fn claim(
        &self,
        parked: Parked,
        req: &CloneReq<'_>,
        sync: BorrowedFd<'_>,
    ) -> Result<(i32, OwnedFd), OciError>;

    /// Why this node's create seeds keep the agent's mount namespace, or `None` where they prune it (goal 5 plan
    /// step 2). A root node's prune, so a pruned-eligible container takes the pruned root path (no `pivot_root`)
    /// and the rest go through the shim, counted. A rootless node's keep it ([`ClassicReason::Rootless`]), as do a
    /// root node's built for the classic root ([`ClassicReason::Baseline`], goal 17's baseline arm): the init a seed
    /// clones builds the root the classic way, counted under that reason.
    fn unpruned(&self) -> Option<ClassicReason>;

    /// How this node makes a pruned create's mount namespace (decision 6's update): the seed's copy,
    /// empty, or from the rootfs's tree, chosen once ([`super::mntns::choose`]).
    fn mount_namespace(&self) -> super::mntns::MountNamespace;

    /// A container made on the classic root path rather than a pruned seed's, by its reason (a fixed set,
    /// so no high-cardinality label; CLAUDE.md §10): counted, so the node can tell how often a create
    /// still pivots, and why. For every reason but [`ClassicReason::Rootless`] and [`ClassicReason::Baseline`],
    /// whose creates a seed makes, the create went through the shim, a re-exec of the hex binary.
    fn classic(&self, reason: ClassicReason);
}

/// Why a create takes the classic root path (the init's `pivot_root`) rather than a pruned seed's (goal 5
/// plan step 2): through the shim, or by a seed with the classic root where the node's seeds keep the agent's mount
/// namespace (a rootless node's, [`ClassicReason::Rootless`]; a root node's built for the classic root,
/// [`ClassicReason::Baseline`]). A fixed set: each is a label on a counter, none derived from the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassicReason {
    /// A pid or user namespace joined by path: a seed's one clone can't make it ([`init::needs_shim`]).
    NamespaceByPath,
    /// A user namespace of the container's own: a tree the agent clones from its own user namespace
    /// isn't locked in the container's, so the pruned path isn't taken for it (a follow-up).
    UserNamespace,
    /// Create-time hooks: they may expect the container's root reachable by host path, as crun gates its
    /// own fd-based path on hooks.
    Hooks,
    /// A mount namespace joined by path, which the pruned init would join rather than build; or a network,
    /// ipc, uts or cgroup namespace by path the agent couldn't open for the init (its namespace holds
    /// nothing of the host to open it by), or more of them than one create carries
    /// ([`hex_core::seed::MAX_PATH_JOINS`]). A pid or user namespace by path is
    /// [`ClassicReason::NamespaceByPath`] instead, refused earlier by the clone.
    JoinsByPath,
    /// The spec makes no mount namespace of its own: there is no root to build on the pruned path.
    NoMountNamespace,
    /// A bind source the agent couldn't clone with `open_tree`, or more bind mounts than fit one
    /// `SCM_RIGHTS` message ([`hex_core::seed::MAX_BINDS`]).
    Source,
    /// A rootless node: its seeds can't own a mount namespace without a user namespace of their own, so
    /// they don't prune, and the init a seed clones builds the root the classic way. Its creates don't go
    /// through the shim.
    Rootless,
    /// A root node built for the classic root by a test control (hex-node's `Node::with_classic_roots`, goal 17's
    /// baseline arm, which the goal sets its create p50 and p99 against): its seeds keep the agent's mount
    /// namespace, as a rootless node's do, and the init a seed clones builds the root the classic way. Its creates
    /// don't go through the shim, and no production node's are counted here.
    Baseline,
}

impl ClassicReason {
    /// The fixed count of reasons, for a per-reason counter.
    pub const COUNT: usize = 8;

    /// This reason's index, for a per-reason counter.
    pub fn index(self) -> usize {
        match self {
            Self::NamespaceByPath => 0,
            Self::UserNamespace => 1,
            Self::Hooks => 2,
            Self::JoinsByPath => 3,
            Self::NoMountNamespace => 4,
            Self::Source => 5,
            Self::Rootless => 6,
            Self::Baseline => 7,
        }
    }
}

/// What [`CloneInit::clone_init`] is given: the container to make, its cgroup and a network namespace
/// made ahead (if any), the pipe ends the init talks to the runtime over (the sync pipe it writes and, on
/// the classic path, the map pipe it reads the runtime's answer on), and, on the pruned path, the root
/// descriptors.
pub struct CloneReq<'a> {
    pub bundle: &'a Path,
    pub dir: &'a Path,
    pub cgroup: &'a BoxCgroup,
    pub network: Option<OwnedFd>,
    pub sync_w: OwnedFd,
    /// The map pipe's read end, on the classic path alone: a pruned create's init asks for no maps and stops for no
    /// hooks (`Runtime::pruned_create` routes both to the classic path), so it carries none
    /// ([`hex_core::seed::Fixed::Lean`]; the lead's (B) of 2026-11-09).
    pub map_r: Option<OwnedFd>,
    /// The spec the runtime checked, sealed ([`checked::seal`]), which the seed makes the container from in place
    /// of reading the bundle (AC-T7.1.1).
    pub config: BorrowedFd<'a>,
    /// On the pruned root path (goal 5 plan step 2): the state directory (and, for a seed's clone, the cgroup
    /// directory) as descriptors and the rootfs and bind-source trees the runtime cloned in its own mount namespace,
    /// which the seed hands the init to build the root from. `None` on the classic seed path (a rootless
    /// node), where the seed reads them by path.
    pub pruned: Option<PrunedCreate>,
}

/// The root descriptors the runtime cloned for a pruned create (goal 5 plan step 2), handed to the seed
/// over `SCM_RIGHTS` with the create. The runtime opens them in its own (host) mount namespace, where
/// the host filesystem is reachable, and closes them once the seed has answered; a tree never attached
/// dissolves with its last descriptor. No bundle directory: the init makes the container from the spec the runtime
/// checked ([`CloneReq::config`]), so nothing would read one.
pub struct PrunedCreate {
    /// The box's cgroup directory, opened only for a seed's clone, which is born in it (the create's first try with no
    /// init parked, or a failed claim's fallback): a parked init is in its cgroup already, so its claim carries none
    /// ([`hex_core::seed::Fixed::Lean`]; the lead's (B) of 2026-11-09).
    pub cgroup_dir: Option<OwnedFd>,
    pub container_dir: OwnedFd,
    /// The rootfs's detached tree; where `mount_namespace` is from its tree, the mount namespace made from
    /// it (decision 6's update), which rides in its place.
    pub rootfs: OwnedFd,
    /// How the init's mount namespace is made: asked of the seed, which answers how it was.
    pub mount_namespace: super::mntns::MountNamespace,
    pub dev_mount: Option<PreparedDev>,
    /// An ipc and uts pair made ahead, which the init joins in place of its clone making them (goal 5 extended,
    /// step 4).
    pub ipc_uts: Option<super::ipcuts::Pair>,
    /// A proc made ahead, its index in the mount plan (decision 6's update, part 2).
    pub proc_mount: Option<(u32, init::PreparedProc)>,
    /// Where `proc_mount` goes unattached when it is the one made for the init parked in the cgroup (decision 8,
    /// parked procs): set only then, so only a claim of that init takes it.
    pub proc_sink: Option<ProcSink>,
    /// The node's empty read-only tmpfs, which the init clones for each masked directory.
    pub masked_directory: Option<OwnedFd>,
    pub binds: Vec<OwnedFd>,
    /// One descriptor per network, ipc, uts or cgroup namespace the spec joins by path, in the spec's
    /// namespace order, which the init joins by `setns` (the pruned seed can't open the host path). A
    /// pid or user namespace by path takes the shim, a mount namespace by path the classic path.
    pub path_joins: Vec<OwnedFd>,
}

/// A fresh `/dev` mount made off the create path, for exactly this index in the mount plan.
pub struct PreparedDev {
    pub mount: u32,
    pub tree: OwnedFd,
}

/// A `/dev` made ahead for a create a node's seed sends through the shim (goal 5 extended, step 3), taken as
/// the pruned path takes one: by the spec's eligibility ([`super::devmount::eligible`]), each miss counted by
/// its reason. A container in a user namespace of its own binds the host's device nodes in, which the node's
/// can't stand for, so it makes its own, counted.
fn shim_dev(
    spec: &Spec,
    dev_mounts: Option<&dyn DevMounts>,
) -> Result<Option<PreparedDev>, OciError> {
    let Some(pool) = dev_mounts else {
        return Ok(None);
    };
    if spec.in_user_namespace() {
        pool.unpooled(hex_core::devfs::Unpooled::UserNamespace)?;
        return Ok(None);
    }
    // A plan the spec can't make takes nothing: its init refuses it, as it would with no pool.
    let Ok(plan) = super::mount::plan(&spec.mounts) else {
        return Ok(None);
    };
    match super::devmount::eligible(spec, &plan) {
        Ok((mount, key)) => Ok(pool.take(key)?.map(|tree| PreparedDev { mount, tree })),
        Err(reason) => {
            pool.unpooled(reason)?;
            Ok(None)
        }
    }
}

/// A duplicate of the node's empty read-only tmpfs for a create made through the shim, whose init clones it for
/// each masked directory (decision 6's update). A container with a user namespace of its own makes its own
/// tmpfs per mask in that namespace, as before: a mount made in the agent's user namespace isn't one its init
/// may attach there.
fn shim_masked_directory(
    spec: &Spec,
    dev_mounts: Option<&dyn DevMounts>,
) -> Result<Option<OwnedFd>, OciError> {
    match dev_mounts.and_then(DevMounts::masked_directory) {
        Some(empty) if !spec.in_user_namespace() => duplicate(empty).map(Some),
        _ => Ok(None),
    }
}

/// A close-on-exec duplicate of `fd`.
fn duplicate(fd: BorrowedFd<'_>) -> Result<OwnedFd, OciError> {
    rustix::io::fcntl_dupfd_cloexec(fd, 0).map_err(|e| OciError::os("duplicating a descriptor", e))
}

/// An init parked in the create's cgroup, if any, and the caller that chose the cgroup (decision 8, "The p99.9
/// that stayed"). Dropped unused, the parked init is ended ([`Parked`]).
struct Parking<'a> {
    parked: Option<Parked>,
    chooser: Option<&'a dyn Choose>,
}

/// Who starts a seed create's init: the node's create seeds, or the init parked in its cgroup ([`Parking`]).
struct Via<'a> {
    seeds: &'a dyn CloneInit,
    parking: Parking<'a>,
}

/// Where a container's cgroup comes from.
enum Cgroup<'a> {
    /// Made from the spec's `cgroupsPath` under the cgroup root (the command line).
    FromSpec,
    /// Made and bounded already by the caller (the `Runtime` seam, the node agent's box root).
    Provided(BoxCgroup),
    /// Chosen by the caller once the spec is read ([`Choose`]): the node agent's for a box whose owner
    /// may hand it a cgroup its earlier box left (goal 5 extended, step 6).
    Chosen(&'a dyn Choose),
}

/// What a create says of its container once its spec is read, for its caller to choose a cgroup by
/// ([`Choose`]).
#[derive(Debug, Clone, Copy)]
pub struct Wants<'a> {
    /// The device program the container's own cgroup must carry ([`Programs::wants`]); `None` when it
    /// carries none.
    pub program: Option<&'a [Insn]>,
    /// Whether its process could change its own cgroup or what is attached to it
    /// ([`Spec::could_change_its_cgroup`]), so the cgroup it leaves can't be handed on.
    pub privileged: bool,
    /// The spec's prepared-proc key ([`super::procmount::eligible`]), `None` where no proc made ahead can serve it:
    /// what the init parked for the owner's next box has its proc made for (decision 8, parked procs).
    pub proc_key: Option<&'a super::procmount::Key>,
}

/// What came of the proc made for the init parked in a create's cgroup (decision 8, parked procs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkedProc {
    /// The claimed init attached it, mounting no proc of its own.
    Hit,
    /// It ended unused (another key, no room for it, the claim refused): the init mounted its own.
    Miss,
    /// The claimed init had none, for a create one could have served: it mounted its own.
    Dry,
}

/// A cgroup a caller chose for a container: made or handed on, its bounds in force either way, and
/// whether the device program the container wants is attached to it already (a cgroup handed on from a
/// box under the same device policy), so the create attaches nothing.
#[derive(Debug)]
pub struct Chosen {
    pub cgroup: BoxCgroup,
    pub attached: bool,
    /// An init made ahead in the cgroup, waiting to be claimed (decision 8, "The p99.9 that stayed").
    pub parked: Option<Parked>,
}

/// A caller that chooses a container's cgroup once its spec is read: the node agent, over the idle
/// cgroups its owners' boxes left (goal 5 extended, step 6). The cgroup is the caller's to remove, as a
/// provided one is.
pub trait Choose {
    fn choose(&self, wants: &Wants<'_>) -> Result<Chosen, OciError>;

    /// The container's init was made with the namespaces `flags` (`clone3`'s `CLONE_NEW*` bits), by a claim of
    /// the init parked in its cgroup when `claimed`, else by a clone: what the caller parks for the owner's next
    /// box, and counts.
    fn init_made(&self, flags: u64, claimed: bool);

    /// What came of the proc made for the init parked in the cgroup (decision 8, parked procs): counted.
    fn parked_proc(&self, outcome: ParkedProc);
}

/// An init made ahead in an idle cgroup (decision 8, "The p99.9 that stayed"): its pid in this process's pid
/// namespace, its pidfd, this end of the socket its claim goes on, and the namespaces it was made with. One
/// that isn't claimed is ended and reaped when dropped, so none is left running or a zombie.
pub struct Parked {
    held: Option<(i32, OwnedFd, OwnedFd)>,
    pub flags: u64,
    /// The proc made for this init's own pid namespace and its key (decision 8, parked procs), held by the node alone
    /// until a claim with the key takes it; it ends with the init otherwise.
    proc: Option<(super::procmount::Key, init::PreparedProc, ProcSink)>,
}

/// Where a parked init's proc goes when no init attaches it (decision 8, parked procs): a thread of the node's, by a
/// queue of a bound the node sets. A detached tree's last close unmounts it and its masks, which takes the host's
/// `namespace_sem` for write and then waits for an expedited RCU grace period (`namespace_unlock`, v6.19
/// `fs/namespace.c:1680-1721`; the wait at `:1715`), so that is never a create's to pay. Past the bound the tree is
/// dropped where it is, counted.
#[derive(Clone, Debug)]
pub struct ProcSink {
    queue: std::sync::mpsc::SyncSender<init::PreparedProc>,
    queued: std::sync::Arc<AtomicU64>,
    inline: std::sync::Arc<AtomicU64>,
    alive: std::sync::Arc<AtomicU64>,
}

impl ProcSink {
    /// A sink onto `queue`, counting in `queued` each tree handed to it and in `inline` each dropped where it was,
    /// its queue full or closed; `alive` counts the node's procs not yet dropped, which one dropped here leaves.
    pub fn new(
        queue: std::sync::mpsc::SyncSender<init::PreparedProc>,
        queued: std::sync::Arc<AtomicU64>,
        inline: std::sync::Arc<AtomicU64>,
        alive: std::sync::Arc<AtomicU64>,
    ) -> Self {
        Self {
            queue,
            queued,
            inline,
            alive,
        }
    }

    /// `proc`, unattached, handed to the node's thread to drop.
    pub fn discard(&self, proc: init::PreparedProc) {
        match self.queue.try_send(proc) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(
                std::sync::mpsc::TrySendError::Full(proc)
                | std::sync::mpsc::TrySendError::Disconnected(proc),
            ) => {
                self.inline.fetch_add(1, Ordering::Relaxed);
                drop(proc);
                let _ = self
                    .alive
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_sub(1))
                    });
            }
        }
    }
}

/// A create's proc taken from the init parked in its cgroup goes back to the node's dropper if the create ends before
/// its claim hands it over: never dropped, and unmounted, on the create's thread (decision 8, parked procs).
impl Drop for PrunedCreate {
    fn drop(&mut self) {
        if let (Some(sink), Some((_, proc))) = (self.proc_sink.take(), self.proc_mount.take()) {
            sink.discard(proc);
        }
    }
}

impl std::fmt::Debug for Parked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parked")
            .field("pid", &self.pid())
            .field("flags", &self.flags)
            .field("proc", &self.proc.as_ref().map(|(k, ..)| k))
            .finish()
    }
}

impl Parked {
    pub fn new(pid: i32, pidfd: OwnedFd, socket: OwnedFd, flags: u64) -> Self {
        Self {
            held: Some((pid, pidfd, socket)),
            flags,
            proc: None,
        }
    }

    /// The init's own pid namespace, opened by its pid in this process's `/proc` and then held to the init by its
    /// pidfd (decision 8, parked procs: the lead's 14:37Z, no new `unsafe`). Opened first, polled second: a pidfd
    /// reads ready once its process has exited or been reaped (v6.19 `fs/pidfs.c:261-265`; pidfd_open(2)), and a
    /// process doesn't come back, so one that reads not ready after the open was running at the open, and its pid
    /// (a running process's, never reused, the init being this process's child) named it. One that reads ready is
    /// refused, whoever reaped it.
    pub fn pid_namespace(&self) -> Result<OwnedFd, OciError> {
        let Some((pid, pidfd, _)) = &self.held else {
            return Err(OciError::Broken("a parked init no longer held".to_owned()));
        };
        let ns = rustix::fs::open(
            format!("/proc/{pid}/ns/pid").as_str(),
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| OciError::os("opening a parked init's pid namespace", e))?;
        let mut fds = [rustix::event::PollFd::new(
            pidfd,
            rustix::event::PollFlags::IN,
        )];
        let now = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ended = rustix::event::poll(&mut fds, Some(&now))
            .map_err(|e| OciError::os("asking whether a parked init runs", e))?;
        if ended != 0 {
            return Err(OciError::Broken(
                "a parked init ended before its pid namespace was opened".to_owned(),
            ));
        }
        Ok(ns)
    }

    /// The proc made for this init's pid namespace, for `key`, held with it until a claim takes it; `sink` drops it
    /// if none does.
    pub fn set_proc(
        &mut self,
        key: super::procmount::Key,
        proc: init::PreparedProc,
        sink: ProcSink,
    ) {
        self.proc = Some((key, proc, sink));
    }

    /// The key this init's proc was made for, if one was.
    pub fn proc_key(&self) -> Option<&super::procmount::Key> {
        self.proc.as_ref().map(|(k, ..)| k)
    }

    /// This init's proc, the caller's from here: taken once, so no second init is ever handed it.
    pub fn take_proc(&mut self) -> Option<(super::procmount::Key, init::PreparedProc, ProcSink)> {
        self.proc.take()
    }

    /// The init's pid, if it is still held.
    pub fn pid(&self) -> Option<i32> {
        self.held.as_ref().map(|h| h.0)
    }

    /// Waits up to `patience` for the init to say on its socket that it is parked: its effective capabilities
    /// emptied, waiting for its claim (`seed::parked`).
    pub fn wait_parked(&self, patience: std::time::Duration) -> Result<(), OciError> {
        let Some((_, _, socket)) = &self.held else {
            return Err(OciError::Broken("a parked init no longer held".to_owned()));
        };
        let mut fds = [rustix::event::PollFd::new(
            socket,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: i64::try_from(patience.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: patience.subsec_nanos().into(),
        };
        let ready = rustix::event::poll(&mut fds, Some(&timeout))
            .map_err(|e| OciError::os("waiting for an init to park", e))?;
        if ready == 0 {
            return Err(OciError::Broken(format!(
                "an init didn't say it was parked within {patience:?}"
            )));
        }
        match super::seed::recv_answer(socket.as_fd())? {
            (hex_core::seed::Answer::Started { .. }, Some(_)) => Ok(()),
            _ => Err(OciError::Broken(
                "an init said something other than that it was parked".to_owned(),
            )),
        }
    }

    /// The init's pid, pidfd and claim socket, the caller's from here.
    pub fn take(mut self) -> Option<(i32, OwnedFd, OwnedFd)> {
        self.held.take()
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some((_, pidfd, socket)) = self.held.take() {
            drop(socket);
            let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
            let _ = rustix::process::waitid(
                rustix::process::WaitId::PidFd(pidfd.as_fd()),
                rustix::process::WaitIdOptions::EXITED,
            );
        }
        // Its proc, if no create took it, goes to the node's thread to drop ([`ProcSink`]).
        if let Some((_, proc, sink)) = self.proc.take() {
            sink.discard(proc);
        }
    }
}

impl Runtime {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            helper: None,
            lsm: super::lsm::Policy::default(),
        }
    }

    /// The runtime with `policy` for a label whose module the host's kernel doesn't run: the command line's
    /// and a node's owner choose it by [`super::lsm::ENV_POLICY`].
    pub fn with_lsm(mut self, policy: super::lsm::Policy) -> Self {
        self.lsm = policy;
        self
    }

    /// The runtime with its container init and exec helper re-exec'd from `helper` (the `hex`
    /// binary), for a caller whose own binary isn't `hex`: the node agent behind the `Runtime` seam,
    /// and the bench that drives it.
    pub fn with_helper(root: PathBuf, helper: PathBuf) -> Self {
        Self {
            root,
            helper: Some(helper),
            lsm: super::lsm::Policy::default(),
        }
    }

    /// The binary to re-exec for the shim and the exec helper.
    fn helper_exe(&self) -> Result<PathBuf, OciError> {
        match &self.helper {
            Some(exe) => Ok(exe.clone()),
            None => std::env::current_exe().map_err(|e| OciError::os("finding the hex binary", e)),
        }
    }

    /// The default state root: `$HEX_OCI_ROOT`, else `$XDG_RUNTIME_DIR/hex-oci`, else
    /// `/run/hex-oci`.
    pub fn default_root() -> PathBuf {
        if let Some(root) = std::env::var_os("HEX_OCI_ROOT") {
            return PathBuf::from(root);
        }
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(xdg).join("hex-oci");
        }
        PathBuf::from("/run/hex-oci")
    }

    /// Container `id`'s directory: in its shard of the root (goal 5 extended, step 6). A runtime from before the
    /// shards kept it in the root itself; none such was ever deployed, so nothing looks there (the lead's ruling of
    /// 2026-11-06 under Ada's rule).
    fn dir(&self, id: &str) -> Result<Dir, OciError> {
        check_id(id)?;
        Ok(Dir::new(&self.root, id))
    }

    /// Makes the container (OCI `create`): sets up its directory and the init blocked at the exec
    /// FIFO, in the `created` state, and returns that state as it wrote it. `bundle` holds
    /// `config.json`; `pid_file`, if given, receives the container init's pid. The container's cgroup
    /// is made from the spec's `cgroupsPath` under the cgroup root, as the runtime-spec's command line
    /// does.
    pub fn create(
        &self,
        id: &str,
        bundle: &Path,
        pid_file: Option<&Path>,
    ) -> Result<State, OciError> {
        self.create_with(
            id,
            bundle,
            pid_file,
            Route {
                cgroup: Cgroup::FromSpec,
                networks: None,
                devices: None,
                dev_mounts: None,
                ipc_uts: None,
                start: Start::Shim,
            },
        )
    }

    /// Makes the container in a cgroup the caller already made and bounded, rather than from the spec, and
    /// starts its init by handing it to a seed to clone (decision 5, goal 5 plan step 4): the `Runtime` seam's
    /// create, where the node agent puts each container under its own delegated box root with the box's bounds
    /// in force (as the `process` class does) and `seeds` is its pool of `hex __oci-seed` processes. The spec's
    /// device list still governs the container's device access; its `cgroupsPath` and `linux.resources` are
    /// ignored, the box's bounds standing in their place. The caller owns the cgroup and removes it; a failed
    /// create leaves it be. `held` is what the caller holds ready for it ([`Held`]): a container that makes a
    /// network namespace of its own in this process's user namespace joins one made ahead in place of making its
    /// own, and its device program is one the caller holds. The container's namespaces, cgroup, device policy,
    /// state and create-time hooks are the shim path's; only the init's start differs. A spec that joins a pid or user namespace by path can't
    /// be made by a seed's one clone ([`init::needs_shim`]), so it is made through the shim, as the command
    /// line makes every container, and the seeds count it ([`CloneInit::through_shim`]): the one create
    /// that still re-execs the hex binary. A create no seed came free for is refused by type, naming the
    /// seeds' bound ([`OciError::SeedsExhausted`]), never sent through the shim. Returns the state it wrote, so
    /// the caller needn't read it back (goal 5 extended, step 5).
    pub fn create_in_cgroup_seeded(
        &self,
        id: &str,
        bundle: &Path,
        pid_file: Option<&Path>,
        cgroup: BoxCgroup,
        held: Held<'_>,
        seeds: &dyn CloneInit,
    ) -> Result<State, OciError> {
        self.create_with(
            id,
            bundle,
            pid_file,
            Route {
                cgroup: Cgroup::Provided(cgroup),
                networks: held.networks,
                devices: held.devices,
                dev_mounts: held.dev_mounts,
                ipc_uts: held.ipc_uts,
                start: Start::Seed(seeds),
            },
        )
    }

    /// Makes the container as [`Runtime::create_in_cgroup_seeded`] does, in a cgroup `chooser` chooses
    /// once the spec is read, which it may hand on from an earlier box (goal 5 extended, step 6): the
    /// container's device program is attached only when the chosen cgroup doesn't carry it already.
    /// The chosen cgroup is the caller's to remove, as a provided one is.
    pub fn create_chosen_seeded(
        &self,
        id: &str,
        bundle: &Path,
        chooser: &dyn Choose,
        held: Held<'_>,
        seeds: &dyn CloneInit,
    ) -> Result<State, OciError> {
        self.create_with(
            id,
            bundle,
            None,
            Route {
                cgroup: Cgroup::Chosen(chooser),
                networks: held.networks,
                devices: held.devices,
                dev_mounts: held.dev_mounts,
                ipc_uts: held.ipc_uts,
                start: Start::Seed(seeds),
            },
        )
    }

    fn create_with(
        &self,
        id: &str,
        bundle: &Path,
        pid_file: Option<&Path>,
        route: Route<'_>,
    ) -> Result<State, OciError> {
        let dir = self.dir(id)?;
        dir.create(id)?;
        let result = self.create_inner(id, bundle, &dir, pid_file, route);
        if result.is_err() {
            let _ = dir.remove();
        }
        result
    }

    fn create_inner(
        &self,
        id: &str,
        bundle: &Path,
        dir: &Dir,
        pid_file: Option<&Path>,
        route: Route<'_>,
    ) -> Result<State, OciError> {
        let Route {
            cgroup,
            networks,
            devices,
            dev_mounts,
            ipc_uts,
            start,
        } = route;
        let config = std::fs::read(bundle.join("config.json"))
            .map_err(|e| OciError::os("reading config.json", e))?;
        let spec = Spec::parse(&config)?;
        // A label the host's kernel runs the module for and can't apply is refused here, before anything is
        // made; one whose module it doesn't run is skipped with a warning, or refused under strict (lsm::admit).
        let mut err = std::io::stderr().lock();
        for warning in super::lsm::admit(&spec, self.lsm)? {
            let _ = writeln!(err, "hex: warning: {warning}");
        }
        drop(err);
        // A labelled container takes nothing the node made ahead, which it made unlabelled: its init makes
        // its own /dev, proc masks and masked-directory tmpfs, each with the label.
        let dev_mounts = match dev_mounts {
            Some(pool) if spec.mount_label().is_some() => {
                pool.unpooled(hex_core::devfs::Unpooled::MountLabel)?;
                pool.unpooled_proc(hex_core::devfs::Unpooled::MountLabel)?;
                None
            }
            pools => pools,
        };
        // Kept with the container's state, so its execs and its start's and delete's hooks take this spec too,
        // never the bundle as it is by then (checked::spec_of).
        checked::keep(dir.path(), &config)?;
        // The bytes just parsed and checked, sealed, so whatever starts the init makes the container from this
        // spec and never reads the bundle again: a config.json swapped after this read is never the container's
        // (AC-T7.1.1).
        let config = checked::seal(&config)?;
        // A spec without resources still gets the default device policy, as runc's does. crun's attaches
        // no device program to a spec without devices (cgroup-resources.c, update_cgroup_v2_resources);
        // Hex keeps the guarantee and shares one program per policy (devshare): a node's oci boxes' root
        // carries the default's, so such a container's create attaches nothing there.
        let plan = resources::plan(
            spec.linux
                .resources
                .as_ref()
                .unwrap_or(&Resources::default()),
        )?;
        make_fifo(&dir.fifo())?;
        // A caller-provided cgroup is already made and bounded, so only the spec's device program is
        // attached to it; one made from the spec takes the whole plan (its resource writes and device
        // program). The provided cgroup is the caller's to remove, so a failure here leaves it.
        let mut parked = None;
        let mut chosen_by: Option<&dyn Choose> = None;
        let (cgroup, caller_owns) = match cgroup {
            Cgroup::FromSpec => (make_cgroup(&spec, id, &plan, &self.root)?, false),
            Cgroup::Provided(cg) => {
                match devices {
                    Some(held) => held.attach(&cg, &plan, spec.in_user_namespace())?,
                    None => {
                        attach_device_program(&cg, &plan, spec.in_user_namespace(), &self.root)?
                    }
                }
                (cg, true)
            }
            Cgroup::Chosen(chooser) => {
                let held = devices.ok_or_else(|| {
                    OciError::Broken(
                        "a create whose caller chooses its cgroup was given no device programs"
                            .to_owned(),
                    )
                })?;
                let program = held.wants(&plan, spec.in_user_namespace())?;
                let proc_key = super::mount::plan(&spec.mounts)
                    .ok()
                    .and_then(|mounts| super::procmount::eligible(&spec, &mounts).ok())
                    .map(|(_, key)| key);
                let chosen = chooser.choose(&Wants {
                    program: program.as_deref(),
                    privileged: spec.could_change_its_cgroup(),
                    proc_key: proc_key.as_ref(),
                })?;
                if let Some(insns) = program
                    && !chosen.attached
                {
                    held.attach_program(&chosen.cgroup, insns)?;
                }
                parked = chosen.parked;
                chosen_by = Some(chooser);
                (chosen.cgroup, true)
            }
        };
        let making = Making {
            id,
            bundle,
            dir,
            pid_file,
            config: config.as_fd(),
        };
        // Asked last, just before the init that joins it, so a create refused before it takes none.
        let result = network_for(&spec, networks).and_then(|network| match start {
            // The command line's create: nothing made ahead, the caller's streams passed on.
            Start::Shim => self.start_shim(
                &making,
                &spec,
                &cgroup,
                Ahead {
                    network,
                    dev: None,
                    masked_directory: None,
                },
                ShimStdio::Inherited,
                &Command::spawn,
            ),
            // A create no seed came free for is refused by type (OciError::SeedsExhausted), never sent through the
            // shim: one implementation per job (Ada's rule; the lead's ruling of 2026-11-06).
            Start::Seed(seeds) => self.start_seeded(
                &making,
                &spec,
                &cgroup,
                network,
                seeds,
                SeedPools {
                    dev_mounts,
                    ipc_uts,
                    parked,
                    chooser: chosen_by,
                },
            ),
        });
        if result.is_err() && !caller_owns {
            // A create that fails leaves no cgroup behind either: whatever ran in it is killed first.
            let _ = end_cgroup(&cgroup, id);
        }
        result
    }

    fn start_shim(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        cgroup: &BoxCgroup,
        ahead: Ahead,
        stdio: ShimStdio,
        spawn: &dyn Fn(&mut Command) -> std::io::Result<std::process::Child>,
    ) -> Result<State, OciError> {
        // `pid_file` and the state write are the shared tail's ([`record_created`]).
        let Making {
            id,
            bundle,
            dir,
            pid_file: _,
            config,
        } = *making;
        let Ahead {
            network,
            dev,
            masked_directory,
        } = ahead;
        // Two pipes: the init writes the sync pipe, the runtime writes the map pipe. The shim opens its
        // ends, and a network namespace made ahead, by the path of this process's descriptors for them,
        // which stay close-on-exec: a flag cleared for the spawn would leave a window in which a process
        // another thread of this multi-threaded agent starts (another create's shim, an exec'd program, a
        // hook) inherits them (CVE-2024-21626's class; runc v1.5.2 `libcontainer/container_linux.go:341-349`).
        let pipe =
            || rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| OciError::os("pipe", e));
        let (sync_r, sync_w) = pipe()?;
        let (map_r, map_w) = pipe()?;
        let by_path = |fd: &OwnedFd| format!("/proc/{}/fd/{}", std::process::id(), fd.as_raw_fd());

        let exe = self.helper_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("__oci-init")
            .env(init::ENV_SYNC_W, by_path(&sync_w))
            .env(init::ENV_MAP_R, by_path(&map_r))
            .env(init::ENV_DIR, dir.path())
            .env(init::ENV_BUNDLE, bundle)
            .env(init::ENV_CGROUP, cgroup.dir())
            .env(
                init::ENV_CONFIG,
                format!("/proc/{}/fd/{}", std::process::id(), config.as_raw_fd()),
            );
        if let Some(network) = &network {
            cmd.env(init::ENV_NETWORK, by_path(network));
        }
        // A `/dev` made ahead goes the same way, by the path of this process's descriptor for the
        // detached tree, with its mount's index in the spec's mounts.
        if let Some(dev) = &dev {
            cmd.env(
                init::ENV_DEV,
                format!("/proc/{}/fd/{}", std::process::id(), dev.tree.as_raw_fd()),
            )
            .env(init::ENV_DEV_MOUNT, dev.mount.to_string());
        }
        if let Some(empty) = &masked_directory {
            cmd.env(init::ENV_MASKED_DIRECTORY, by_path(empty));
        }
        if let ShimStdio::Null = stdio {
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let mut shim =
            spawn(&mut cmd).map_err(|e| OciError::os("starting the container init", e))?;
        // The shim opens both ends before its first word on the sync pipe, so this process keeps them,
        // and the paths name them, until the shim has spoken or ended. Until then this end of the sync
        // pipe keeps it from reading end-of-file, so the shim's pidfd says it ended.
        let heard = first_word(&sync_r, &shim);
        drop(sync_w);
        drop(map_r);
        if let Err(e) = heard {
            let _ = shim.kill();
            let _ = shim.wait();
            return Err(e);
        }

        let mut container_pid = None;
        let outcome = self
            .handshake(making, spec, sync_r, map_w, &mut container_pid)
            .and_then(|pid| {
                // The container is blocked at the exec FIFO, so its start time is its own.
                let start_time = state::start_time(pid).ok_or_else(|| {
                    OciError::Broken(format!("container {id}'s init ended during create"))
                })?;
                end_shim(&mut shim)?;
                Ok((pid, start_time))
            });
        // The shim has opened the namespace made ahead by its pid line, or never will: it is the
        // container's alone now. So with the `/dev` made ahead, which the container attached before its
        // ready line, or which goes with this descriptor, unattached.
        drop(network);
        drop(dev);
        if outcome.is_err() {
            // A create that fails leaves nothing running: the container the shim forked, if it said
            // which, and the shim itself, killed and reaped here (on success it exits by itself and
            // end_shim reaps it).
            if let Some(pid) = container_pid {
                let _ = sys::kill(pid, libc::SIGKILL, false);
            }
            let _ = shim.kill();
            let _ = shim.wait();
        }
        match outcome {
            Ok((container_pid, start_time)) => {
                self.record_created(making, spec, cgroup, container_pid, start_time)
            }
            Err(e) => Err(e),
        }
    }

    /// Routes a seam create to a seed: through the shim (counted) for a container a seed can't make in one clone or a
    /// pruned seed can't serve, else to the seed, on the pruned root path on a root node (goal 5 plan step 2), and the
    /// classic path where the node's seeds keep the agent's mount namespace, counted under the reason they keep it
    /// ([`CloneInit::unpruned`]). The pruned path clones the rootfs and each bind source here, in the agent's own mount
    /// namespace, and hands them to the seed; a container not eligible for it (a user namespace of its own, create-time
    /// hooks, a mount namespace joined by path, none of its own, or a source that can't be cloned) is made through the
    /// shim, by reason.
    fn start_seeded(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        cgroup: &BoxCgroup,
        network: Option<OwnedFd>,
        seeds: &dyn CloneInit,
        pools: SeedPools<'_>,
    ) -> Result<State, OciError> {
        let SeedPools {
            dev_mounts,
            ipc_uts,
            parked,
            chooser,
        } = pools;
        let mut parking = Parking { parked, chooser };
        if init::needs_shim(spec) {
            // A parked init is ended before the shim's is cloned beside it in the cgroup.
            drop(parking);
            seeds.classic(ClassicReason::NamespaceByPath);
            ipc_uts_on_path(spec, ipc_uts)?;
            let dev = shim_dev(spec, dev_mounts)?;
            return self.start_shim(
                making,
                spec,
                cgroup,
                Ahead {
                    network,
                    dev,
                    masked_directory: shim_masked_directory(spec, dev_mounts)?,
                },
                ShimStdio::Null,
                &Command::spawn,
            );
        }
        if let Some(kept) = seeds.unpruned() {
            seeds.classic(kept);
            ipc_uts_on_path(spec, ipc_uts)?;
            self.start_via_seed(making, spec, cgroup, network, None, Via { seeds, parking })
        } else {
            match self.pruned_create(making, spec, seeds.mount_namespace()) {
                Ok(mut pruned) => {
                    if let Some(pool) = dev_mounts {
                        let plan = super::mount::plan(&spec.mounts)?;
                        // Keep the optional descriptor within SCM_MAX_FD without changing old limits.
                        let spare = pruned
                            .binds
                            .len()
                            .checked_add(pruned.path_joins.len())
                            .and_then(|n| {
                                n.checked_add(
                                    hex_core::seed::CREATE_FDS
                                        + 1
                                        + hex_core::seed::PRUNED_FIXED_FDS
                                        + hex_core::seed::CONFIG_FDS
                                        + 1,
                                )
                            })
                            .is_some_and(|n| n <= hex_core::seed::MAX_REQUEST_FDS);
                        if !spare {
                            pool.unpooled(hex_core::devfs::Unpooled::Capacity)?;
                        } else {
                            match super::devmount::eligible(spec, &plan) {
                                Ok((mount, key)) => {
                                    if let Some(tree) = pool.take(key)? {
                                        pruned.dev_mount = Some(PreparedDev { mount, tree });
                                    }
                                }
                                Err(reason) => pool.unpooled(reason)?,
                            }
                        }
                        // A proc made ahead adds its pid namespace and its tree, within the same bound. None is
                        // taken from the pool where an init is parked in the cgroup, whatever the way: the parked
                        // init, which has its pid namespace already, takes the create (decision 8), and a pool's
                        // proc would keep it from doing so: a create takes one or the other, never both. The proc made for that init's own pid
                        // namespace, if the node made one (parked procs), is the create's when its key is the
                        // spec's and it fits; otherwise it ends unused, a miss, and the init mounts its own.
                        let parked_takes = parking.parked.is_some();
                        let parked_proc = if parked_takes {
                            parking.parked.as_mut().and_then(Parked::take_proc)
                        } else {
                            None
                        };
                        let room = pruned
                            .binds
                            .len()
                            .checked_add(pruned.path_joins.len())
                            .and_then(|n| {
                                n.checked_add(
                                    hex_core::seed::CREATE_FDS
                                        + 1
                                        + hex_core::seed::PRUNED_FIXED_FDS
                                        + hex_core::seed::CONFIG_FDS
                                        + hex_core::seed::PROC_FDS,
                                )
                            })
                            .and_then(|n| n.checked_add(usize::from(pruned.dev_mount.is_some())))
                            .is_some_and(|n| n <= hex_core::seed::MAX_REQUEST_FDS);
                        if parked_takes {
                            let eligible = super::procmount::eligible(spec, &plan).ok();
                            let outcome = match (parked_proc, eligible) {
                                (Some((made, proc, sink)), Some((mount, key)))
                                    if room && made == key =>
                                {
                                    pruned.proc_mount = Some((mount, proc));
                                    pruned.proc_sink = Some(sink);
                                    // Counted once the claim is known (start_via_seed).
                                    None
                                }
                                (None, Some(_)) if room => Some(ParkedProc::Dry),
                                // None made, for a create none could serve: neither.
                                (None, _) => None,
                                (Some((_, proc, sink)), _) => {
                                    sink.discard(proc);
                                    Some(ParkedProc::Miss)
                                }
                            };
                            if let (Some(outcome), Some(chooser)) = (outcome, parking.chooser) {
                                chooser.parked_proc(outcome);
                            }
                        } else if !room {
                            pool.unpooled_proc(hex_core::devfs::Unpooled::Capacity)?;
                        } else {
                            match super::procmount::eligible(spec, &plan) {
                                Ok((mount, key)) => {
                                    if let Some(proc) = pool.take_proc(key)? {
                                        pruned.proc_mount = Some((mount, proc));
                                    }
                                }
                                Err(reason) => pool.unpooled_proc(reason)?,
                            }
                        }
                        // The node's empty tmpfs for the masked directories adds one, within the same bound;
                        // past it the init makes a tmpfs per mask, as before.
                        let room = pruned
                            .binds
                            .len()
                            .checked_add(pruned.path_joins.len())
                            .and_then(|n| {
                                n.checked_add(
                                    hex_core::seed::CREATE_FDS
                                        + 1
                                        + hex_core::seed::PRUNED_FIXED_FDS
                                        + hex_core::seed::CONFIG_FDS
                                        + 1,
                                )
                            })
                            .and_then(|n| n.checked_add(usize::from(pruned.dev_mount.is_some())))
                            .and_then(|n| {
                                n.checked_add(if pruned.proc_mount.is_some() {
                                    hex_core::seed::PROC_FDS
                                } else {
                                    0
                                })
                            })
                            .is_some_and(|n| n <= hex_core::seed::MAX_REQUEST_FDS);
                        if room && let Some(empty) = pool.masked_directory() {
                            pruned.masked_directory = Some(duplicate(empty)?);
                        }
                    }
                    // Asked last, as a network namespace is, so a create routed elsewhere takes none.
                    // A count past usize reads as no room.
                    let ahead = usize::from(pruned.dev_mount.is_some())
                        .checked_add(if pruned.proc_mount.is_some() {
                            hex_core::seed::PROC_FDS
                        } else {
                            0
                        })
                        .and_then(|n| n.checked_add(usize::from(pruned.masked_directory.is_some())))
                        .unwrap_or(usize::MAX);
                    let fits = pair_fits(pruned.binds.len(), pruned.path_joins.len(), ahead);
                    pruned.ipc_uts = ipc_uts_for(spec, ipc_uts, fits)?;
                    self.start_via_seed(
                        making,
                        spec,
                        cgroup,
                        network,
                        Some(pruned),
                        Via { seeds, parking },
                    )
                }
                Err(reason) => {
                    drop(parking);
                    seeds.classic(reason);
                    ipc_uts_on_path(spec, ipc_uts)?;
                    let dev = shim_dev(spec, dev_mounts)?;
                    self.start_shim(
                        making,
                        spec,
                        cgroup,
                        Ahead {
                            network,
                            dev,
                            masked_directory: shim_masked_directory(spec, dev_mounts)?,
                        },
                        ShimStdio::Null,
                        &Command::spawn,
                    )
                }
            }
        }
    }

    /// The root descriptors for a pruned create (goal 5 plan step 2), or the reason it can't take the
    /// pruned path. The rootfs and each bind source are cloned as detached trees (`open_tree` with
    /// `OPEN_TREE_CLONE | AT_RECURSIVE`) in the agent's own mount namespace, where the host filesystem is
    /// reachable, as crun opens every mount ahead for its `OPEN_TREE_NAMESPACE` path (f17, f73); the
    /// binds are cloned in [`crate::linux::oci::mount::plan`]'s order, which the seed's init follows when
    /// it attaches them. The state directory is opened as a descriptor too, so a pruned seed, which reaches no
    /// host path, builds the whole root from descriptors; the cgroup directory only where a seed clones the init
    /// ([`Runtime::start_via_seed`]), never for a parked init's claim.
    ///
    /// Where `way` is from its tree (decision 6's update), the agent makes the container's mount namespace
    /// itself from the rootfs, here where the rootfs's host path is reachable, in one hold of the mount
    /// lock (`goal6-mntns.md` §4): a directory root by `open_tree(OPEN_TREE_NAMESPACE)` on its path, a
    /// layered root's overlay as a new namespace's filesystem ([`init::make_fs_namespace`]). The init joins
    /// it and changes its propagation itself, so the agent clones nothing for the root and sets nothing.
    fn pruned_create(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        way: super::mntns::MountNamespace,
    ) -> Result<PrunedCreate, ClassicReason> {
        let from_tree = way == super::mntns::MountNamespace::FromTree;
        // A mount namespace joined by path: the pruned init would `setns` into an existing namespace,
        // not build the pruned one. The container names a mount namespace, by path, so this is
        // `JoinsByPath`, not `NoMountNamespace`; it is checked first because `makes_namespace` counts
        // only a namespace made (no path), so a mount namespace by path would otherwise fall to the
        // `NoMountNamespace` check below. (A pid or user namespace by path went to the shim already,
        // `needs_shim`; a network, ipc, uts or cgroup namespace by path the agent opens below.)
        if spec
            .linux
            .namespaces
            .iter()
            .any(|n| n.kind == "mount" && n.path.is_some())
        {
            return Err(ClassicReason::JoinsByPath);
        }
        if !spec.makes_namespace("mount") {
            return Err(ClassicReason::NoMountNamespace);
        }
        if spec.makes_namespace("user") {
            return Err(ClassicReason::UserNamespace);
        }
        // Create-time hooks stop the init before its root switch, and the init runs createContainer and
        // startContainer itself, over the map pipe the pruned init doesn't hold.
        if spec.hooks.sync_init() {
            return Err(ClassicReason::Hooks);
        }
        let plan =
            crate::linux::oci::mount::plan(&spec.mounts).map_err(|_| ClassicReason::Source)?;
        // The rootfs: the mount at `/` built as a detached mount where there is one (a layered root's
        // overlay, whose layers are host paths reachable here but not in the pruned seed), else the
        // root directory cloned (a directory root). The init attaches it and skips the `/` mount.
        let tree = |path: &Path| {
            if from_tree {
                init::namespace_of(rustix::fs::CWD, path)
            } else {
                clone_tree(path)
            }
        };
        let rootfs = match plan.iter().find(|op| op.destination == "/") {
            Some(op) if !op.is_bind => if from_tree {
                init::make_fs_namespace(
                    &op.kind_source,
                    &op.fstype,
                    &op.data,
                    op.flags,
                    spec.mount_label(),
                )
            } else {
                init::make_fs(
                    &op.kind_source,
                    &op.fstype,
                    &op.data,
                    op.flags,
                    spec.mount_label(),
                )
            }
            .map_err(|_| ClassicReason::Source)?,
            Some(op) => tree(Path::new(&op.kind_source)).map_err(|_| ClassicReason::Source)?,
            None => {
                let p = Path::new(&spec.root.path);
                let rootfs_path = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    making.bundle.join(p)
                };
                tree(&rootfs_path).map_err(|_| ClassicReason::Source)?
            }
        };
        // open_tree clones retain a source's shared peer group. Isolate the detached root before
        // the init attaches it or mounts beneath it (goal5-root.md §9). A slave keeps incoming host
        // events; a final shared root gets a new peer group instead of sharing the host's. A namespace
        // made from the tree isn't this process's to change: its init does the same once it has joined.
        if !from_tree {
            sys::mount_setattr(
                rootfs.as_fd(),
                true,
                &sys::MountAttrs {
                    set: 0,
                    clr: 0,
                    propagation: init::isolating_propagation(spec),
                },
            )
            .map_err(|_| ClassicReason::Source)?;
        }
        let mut binds = Vec::new();
        for op in &plan {
            if op.destination == "/" {
                continue;
            }
            // A non-root overlay's layers are host paths the pruned init can't reach; the shim makes it.
            if op.fstype == "overlay" {
                return Err(ClassicReason::Source);
            }
            if op.is_bind {
                if binds.len() >= hex_core::seed::MAX_BINDS {
                    return Err(ClassicReason::Source);
                }
                binds.push(
                    clone_tree(Path::new(&op.kind_source)).map_err(|_| ClassicReason::Source)?,
                );
            }
        }
        // Each network, ipc, uts or cgroup namespace the spec joins by path, opened here (the pruned
        // seed can't reach the host path), in the spec's namespace order, which the init joins by
        // `setns`. The init derives each one's kind from the spec in the same order.
        let mut path_joins = Vec::new();
        for ns in &spec.linux.namespaces {
            if ns.path.is_none()
                || !matches!(ns.kind.as_str(), "network" | "ipc" | "uts" | "cgroup")
            {
                continue;
            }
            if path_joins.len() >= hex_core::seed::MAX_PATH_JOINS {
                return Err(ClassicReason::JoinsByPath);
            }
            let Some(path) = &ns.path else { continue };
            let fd = rustix::fs::open(
                path.as_str(),
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| ClassicReason::JoinsByPath)?;
            path_joins.push(fd);
        }
        let container_dir = open_dir(making.dir.path()).map_err(|_| ClassicReason::Source)?;
        Ok(PrunedCreate {
            proc_sink: None,
            // Opened only where a seed clones the init ([`Runtime::start_via_seed`]).
            cgroup_dir: None,
            container_dir,
            rootfs,
            mount_namespace: way,
            dev_mount: None,
            ipc_uts: None,
            proc_mount: None,
            masked_directory: None,
            binds,
            path_joins,
        })
    }

    /// Starts the container's init by handing it to a seed to clone (decision 5, goal 5 plan step 4), in
    /// place of the shim's re-exec. The seed clones the init into the cgroup and the fresh namespaces its
    /// spec asks for, as the agent's own child, and hands back its pid and a pidfd. The runtime then runs
    /// the same sync handshake as the shim path ([`handshake_seed`]: a user namespace's maps, the ready
    /// signal), reads the init's start time and writes the `created` state. The pidfd is held only to end
    /// the init cleanly if the handshake fails; the node agent opens its own for the init it watches, as
    /// it does for a shim-forked init. `pruned`, when given, carries the root descriptors for the pruned
    /// path (goal 5 plan step 2); the runtime holds them until the seed answers, then drops them.
    fn start_via_seed(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        cgroup: &BoxCgroup,
        network: Option<OwnedFd>,
        pruned: Option<PrunedCreate>,
        via: Via<'_>,
    ) -> Result<State, OciError> {
        let Via { seeds, parking } = via;
        // Pipes, close-on-exec: the init writes the sync pipe, the runtime writes the map pipe. The
        // seed hands the init's ends over `SCM_RIGHTS`, so they never need their close-on-exec cleared;
        // they close in the init only when it execs the container's program. The map pipe carries a user
        // namespace's maps and the create-time hooks' state, which a pruned create has neither of
        // (`pruned_create` routes both to the classic path), so only a classic create makes one.
        let pipe =
            || rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| OciError::os("pipe", e));
        let (sync_r, sync_w) = pipe()?;
        let (map_r, map_w) = if pruned.is_none() {
            let (r, w) = pipe()?;
            (Some(r), Some(w))
        } else {
            (None, None)
        };
        // The namespaces the init is made in, as the seed works them out (`seed::serve_create`): a parked init
        // made with these takes the create, in place of a clone into the cgroup and its wait for the host-wide
        // `cgroup_mutex` (decision 8, "The p99.9 that stayed"); one made with others is ended, never used.
        let flags = init::fresh_clone_flags(
            spec,
            network.is_some(),
            pruned.as_ref().is_some_and(|p| p.ipc_uts.is_some()),
        );
        let mut req = CloneReq {
            bundle: making.bundle,
            dir: making.dir.path(),
            cgroup,
            network,
            sync_w,
            map_r,
            config: making.config,
            pruned,
        };
        let Parking { parked, chooser } = parking;
        // A parked init was cloned with its own pid namespace: it can't take a create that carries a pool's proc
        // made ahead (whose pid namespace the init would be cloned into), so such a create is cloned fresh and the
        // parked init ended. The proc made for the parked init's own pid namespace is the one it takes.
        let takes_parked = req
            .pruned
            .as_ref()
            .is_none_or(|p| p.proc_mount.is_none() || p.proc_sink.is_some());
        // Its mount namespace's way changes the flags it is cloned with, so a parked init is made, and held, to the
        // way's (decision 6's update, decision 8).
        let way = req
            .pruned
            .as_ref()
            .map_or(super::mntns::MountNamespace::Copy, |p| p.mount_namespace);
        let made_with = super::mntns::clone_flags(flags, way);
        let claimed = match parked {
            // A failed claim falls back to a seed's clone of the same request, which the claim only borrowed;
            // the seeds count it by why, hung or refused, where they know it ([`CloneInit::claim`]).
            Some(p) if p.flags == made_with && takes_parked => {
                seeds.claim(p, &req, sync_r.as_fd()).ok()
            }
            _ => None,
        };
        // A parked init's proc shows that init's pid namespace, so a create its init didn't take mounts its own:
        // the proc goes unused, to the node's thread to drop, as the init it was made for has ended.
        if let Some(p) = req.pruned.as_mut()
            && let Some(sink) = &p.proc_sink
        {
            if claimed.is_none()
                && let Some((_, proc)) = p.proc_mount.take()
            {
                sink.discard(proc);
            }
            // Claimed, the init holds its own copies of the tree: these close as any descriptor, unmounting nothing.
            if claimed.is_some() {
                p.proc_sink = None;
            }
            if let Some(chooser) = chooser {
                chooser.parked_proc(if claimed.is_some() {
                    ParkedProc::Hit
                } else {
                    ParkedProc::Miss
                });
            }
        }
        if let Some(chooser) = chooser {
            chooser.init_made(made_with, claimed.is_some());
        }
        let (pid, pidfd) = match claimed {
            Some(started) => {
                // The init holds its ends of the pipes now; the runtime's copies go, as a seed's do once it has
                // cloned, so the sync pipe reads its end when the init does.
                drop(req);
                started
            }
            None => {
                // A seed's clone is born in the box's cgroup, so it is handed the cgroup's directory, opened only
                // now: a parked init's claim, whose init is in its cgroup already, carried none.
                if let Some(p) = req.pruned.as_mut() {
                    p.cgroup_dir = Some(open_cgroup_dir(cgroup.dir())?);
                }
                seeds.clone_init(req)?
            }
        };
        // The runtime holds the read end of sync and the write end of map; the seed closed its copies of
        // the init's ends once it had cloned, so the init alone holds them now.
        let outcome = self
            .handshake_seed(making, &sync_r, map_w.as_ref(), pid, spec)
            .and_then(|()| {
                // The init is blocked at the exec FIFO, so its start time is its own.
                state::start_time(pid).ok_or_else(|| {
                    OciError::Broken(format!(
                        "container {}'s init ended during create",
                        making.id
                    ))
                })
            });
        match outcome {
            Ok(start_time) => self.record_created(making, spec, cgroup, pid, start_time),
            Err(e) => {
                // A create that fails leaves nothing running: the init, if it still runs, ended through
                // the pidfd the seed handed back (no pid-reuse race, unlike a kill by pid).
                let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
                Err(e)
            }
        }
    }

    /// Writes the `created` state for a container whose init is at the exec FIFO, and its pid file (if
    /// asked): the tail both the shim and the seed paths share. Returns the state as written.
    fn record_created(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        cgroup: &BoxCgroup,
        pid: i32,
        start_time: u64,
    ) -> Result<State, OciError> {
        let mut state = State::new(making.id, &making.bundle.to_string_lossy());
        state.status = Status::Created;
        state.pid = pid;
        state.start_time = Some(start_time);
        state.annotations = spec.annotations.clone();
        state.cgroup = Some(cgroup.dir().to_string_lossy().into_owned());
        state.poststart = Some(!spec.hooks.poststart.is_empty());
        state.pid_namespace = Some(spec.makes_namespace("pid"));
        if !spec.hooks.start_container.is_empty() {
            // A hook naming no timeout is waited for as long as it runs: recorded as the most seconds there are.
            state.start_container = Some(
                hooks::budget("startContainer", &spec.hooks.start_container)?.unwrap_or(u64::MAX),
            );
        }
        making.dir.put(&state)?;
        if let Some(path) = making.pid_file {
            std::fs::write(path, format!("{pid}\n"))
                .map_err(|e| OciError::os("writing the pid file", e))?;
        }
        // The create-time hooks ran while the init waited before its root switch ([`answer_hooks`]).
        Ok(state)
    }

    /// The sync handshake for a create through a seed: the init writes the sync pipe, as the shim's child
    /// does, but its pid came back from the seed, so the protocol carries no pid line. A `U` asks for the
    /// user namespace's maps, which the runtime writes against `pid` (the init's) and acks on the map
    /// pipe; `H` stops the init before its root switch for the create-time hooks ([`answer_hooks`]); `R`
    /// is the ready signal; `E` a setup failure. Returns once the init is ready, or the error. A pruned create's init
    /// has no map pipe (`map_w` is `None`): it asks for no maps and stops for no hooks, so one that did is refused.
    fn handshake_seed(
        &self,
        making: &Making<'_>,
        sync_r: &OwnedFd,
        map_w: Option<&OwnedFd>,
        pid: i32,
        spec: &Spec,
    ) -> Result<(), OciError> {
        let mut reader = BufReader::new(std::fs::File::from(
            sync_r
                .try_clone()
                .map_err(|e| OciError::os("duplicating the sync pipe", e))?,
        ));
        let mut map_w = map_w
            .map(|w| {
                w.try_clone()
                    .map(std::fs::File::from)
                    .map_err(|e| OciError::os("duplicating the map pipe", e))
            })
            .transpose()?;
        let unanswerable = || {
            OciError::Broken(
                "a pruned create's init asked for maps or hooks, and has no map pipe for them"
                    .to_owned(),
            )
        };
        let mut hooked = false;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| OciError::os("reading the sync pipe", e))?;
            if n == 0 {
                return Err(OciError::Broken(
                    "the container init closed before its ready signal".to_owned(),
                ));
            }
            let msg = line.trim_end();
            match msg.chars().next() {
                Some('U') => {
                    let map_w = map_w.as_mut().ok_or_else(unanswerable)?;
                    write_id_maps(pid, &spec.linux.uid_mappings, &spec.linux.gid_mappings)?;
                    map_w
                        .write_all(&[1])
                        .map_err(|e| OciError::os("acking the maps", e))?;
                }
                Some('H') => {
                    answer_hooks(making, spec, pid, map_w.as_mut().ok_or_else(unanswerable)?)?;
                    hooked = true;
                }
                Some('R') if spec.hooks.sync_init() && !hooked => return Err(unhooked(making.id)),
                Some('R') => return Ok(()),
                Some('E') => {
                    return Err(OciError::Os {
                        op: "container init",
                        why: msg.get(1..).unwrap_or_default().to_owned(),
                    });
                }
                None => {}
                _ => return Err(OciError::Broken(format!("unknown sync message {msg:?}"))),
            }
        }
    }

    /// Reads the sync pipe: writes the user-namespace maps when asked, runs the create-time hooks when
    /// the init stops for them ([`answer_hooks`]), then returns the container pid once the init has sent
    /// both it and its ready signal. `container_pid` holds the pid as
    /// soon as it is known, so a create that fails can still end the container.
    fn handshake(
        &self,
        making: &Making<'_>,
        spec: &Spec,
        sync_r: OwnedFd,
        map_w: OwnedFd,
        container_pid: &mut Option<i32>,
    ) -> Result<i32, OciError> {
        let mut reader = BufReader::new(std::fs::File::from(sync_r));
        let mut map_w = std::fs::File::from(map_w);
        read_sync(
            &mut reader,
            making.id,
            spec.hooks.sync_init(),
            |ask| match ask {
                Ask::Maps(pid) => {
                    write_id_maps(pid, &spec.linux.uid_mappings, &spec.linux.gid_mappings)?;
                    map_w
                        .write_all(&[1])
                        .map_err(|e| OciError::os("acking the maps", e))
                }
                Ask::Hooks(pid) => answer_hooks(making, spec, pid, &mut map_w),
            },
            container_pid,
        )
    }

    /// Starts the container (OCI `start`) in one operation on the exec FIFO, as crun does: the state is read once,
    /// the FIFO the init is blocked on opened, its byte taken, and the FIFO removed. Nothing is written: `running`
    /// is derived from the FIFO's absence while the init lives ([`state::derive`]), so start costs what libcrun's
    /// does (decision 3, goal 3 plan step 3). A FIFO already gone is a start that happened: refused as running.
    pub fn start(&self, id: &str) -> Result<(), OciError> {
        self.start_with(id, None)
    }

    /// [`Runtime::start`] with the init's pidfd: the node agent hands the one it holds for the init;
    /// with none it is opened here from the state's pid and checked against the start time create
    /// recorded, so a later process given the pid is never taken for the init.
    pub fn start_with(&self, id: &str, init: Option<OwnedFd>) -> Result<(), OciError> {
        let dir = self.dir(id)?;
        let state = dir.get(id)?;
        // A container with startContainer hooks is marked as starting while the start waits for them, so a
        // node agent that stops meanwhile leaves its restart a fact to remove the box by
        // ([`state::start_interrupted`]); the mark goes when the start returns, however it ends.
        if state.start_container.is_none() {
            return self.take_start(id, &dir, state, init);
        }
        let begun = dir.path().join(hooks::START_BEGUN);
        std::fs::File::create(&begun).map_err(|e| OciError::os("marking the start begun", e))?;
        let started = self.take_start(id, &dir, state, init);
        let unmarked =
            std::fs::remove_file(&begun).map_err(|e| OciError::os("unmarking the start", e));
        started.and(unmarked)
    }

    /// The start itself: [`Runtime::start_with`]'s, the FIFO opened, its byte waited for and the FIFO removed.
    fn take_start(
        &self,
        id: &str,
        dir: &Dir,
        state: State,
        init: Option<OwnedFd>,
    ) -> Result<(), OciError> {
        if !matches!(state.status, Status::Created | Status::Running) {
            return Err(OciError::WrongState(
                id.to_owned(),
                state.status.name(),
                "created",
            ));
        }
        let stopped = || OciError::WrongState(id.to_owned(), "stopped", "created");
        let Some(init) = (match init {
            Some(init) => Some(init),
            None => init_pidfd(&state)?,
        }) else {
            return Err(stopped());
        };
        let fifo = match open_start_end(&dir.fifo()) {
            Ok(fifo) => fifo,
            Err(rustix::io::Errno::NOENT) => {
                return Err(OciError::WrongState(id.to_owned(), "running", "created"));
            }
            Err(e) => return Err(OciError::os("opening the exec fifo", e)),
        };
        // A FIFO's pipe lives only while something holds it open, and an open with none made a new one
        // (v6.19 `fs/pipe.c:709-721`, `:1129-1147`). So an open whose lookup came before another start's
        // removal, and which landed after that start's close, holds a new pipe with no byte ever to come.
        // That start removed the FIFO before it closed it, so a FIFO with no name says it has the byte.
        if !still_named(&fifo)? {
            return Err(OciError::WrongState(id.to_owned(), "running", "created"));
        }
        // The init writes one byte as it takes the start. An init that ended without it (killed, or
        // OOM'd, before or after it opened its end) is stopped, not running: refused, never reported
        // running (CLAUDE.md §3); the FIFO stays, and the init's end says stopped.
        // The init runs its startContainer hooks before it writes, so the start waits for them too, as
        // long as they may take, saturating where that is past what a Duration holds.
        let patience = state.start_container.map_or(START_PATIENCE, |secs| {
            START_PATIENCE.saturating_add(Duration::from_secs(secs))
        });
        match wait_for_byte(&fifo, &init, patience)? {
            Took::Byte => {}
            Took::End => return Err(stopped()),
            Took::Nothing => {
                return Err(OciError::Broken(format!(
                    "container {id} did not take the start within {patience:?}"
                )));
            }
        }
        // A startContainer hook that failed wrote why in place of the byte, and its init ends: the start
        // fails, naming it (runtime.md:69-70). Read without taking it, as another start may be waiting.
        if state.start_container.is_some()
            && let Some(why) = hooks::peek_start(fifo.as_fd())?
        {
            return Err(OciError::Os { op: "start", why });
        }
        // From here the FIFO's absence is what says the container runs. Of two starts at once, both
        // see the byte, and the one whose removal finds the FIFO gone finds the container running.
        match std::fs::remove_file(dir.fifo()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(OciError::WrongState(id.to_owned(), "running", "created"));
            }
            Err(e) => return Err(OciError::os("removing the exec fifo", e)),
        }
        // poststart hooks run after the workload has started (OCI §Lifecycle), with the state they see as
        // running; the config is read only when create found hooks there (or state from before it looked).
        if state.poststart != Some(false)
            && let Some(hooks) = load_hooks(dir.path(), &state)?
        {
            let mut running = state;
            running.status = Status::Running;
            hooks::run("poststart hook", &hooks.poststart, &running)?;
        }
        Ok(())
    }

    /// The command that runs `args` in the container (`hex oci exec`): the helper `hex __oci-exec`,
    /// for the caller to exec into, so a signal its own caller sends reaches the helper that passes it
    /// on (exec.rs). The container must be created or running, its init alive. `seam` makes the helper
    /// end the way its program did (for the node agent's pidfd watch) rather than as a shell reports.
    pub fn exec_command(
        &self,
        id: &str,
        args: Vec<String>,
        cwd: Option<String>,
        env: Vec<String>,
        user: Option<(u32, Option<u32>)>,
        seam: bool,
    ) -> Result<Command, OciError> {
        let dir = self.dir(id)?;
        let state = dir.get(id)?;
        if !matches!(state.status, Status::Created | Status::Running) {
            return Err(OciError::WrongState(
                id.to_owned(),
                state.status.name(),
                "created or running",
            ));
        }
        if !state.init_alive() {
            return Err(OciError::WrongState(
                id.to_owned(),
                "stopped",
                "created or running",
            ));
        }
        if args.is_empty() {
            return Err(OciError::Config("exec needs a program".to_owned()));
        }
        let request = crate::linux::oci::exec::Request {
            pid: state.pid,
            bundle: state.bundle,
            cgroup: state.cgroup,
            args,
            cwd,
            env,
            user,
            seam,
            dir: Some(dir.path().to_string_lossy().into_owned()),
        };
        let json = serde_json::to_string(&request)
            .map_err(|e| OciError::Broken(format!("the exec request: {e}")))?;
        let exe = self.helper_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("__oci-exec")
            .env(crate::linux::oci::exec::ENV_REQUEST, json);
        Ok(cmd)
    }

    /// The container's state (OCI `state`), its status derived as crun derives it ([`state::derive`]): `created`
    /// until start takes the exec FIFO, `running` after while the init lives, `stopped` once it doesn't.
    pub fn state(&self, id: &str) -> Result<State, OciError> {
        let dir = self.dir(id)?;
        let mut state = dir.get(id)?;
        state.status =
            crate::linux::oci::state::derive(state.status, dir.fifo_present(), state.init_alive());
        Ok(state)
    }

    /// Sends signal `sig` to the container (OCI `kill`): with `all`, to every process in its cgroup
    /// (its process group, for a container made before it had one); otherwise to the init alone.
    pub fn kill(&self, id: &str, sig: i32, all: bool) -> Result<(), OciError> {
        let state = self.dir(id)?.get(id)?;
        if !matches!(state.status, Status::Created | Status::Running) {
            return Err(OciError::WrongState(
                id.to_owned(),
                state.status.name(),
                "created or running",
            ));
        }
        if !state.init_alive() {
            return Err(OciError::WrongState(
                id.to_owned(),
                "stopped",
                "created or running",
            ));
        }
        match (&state.cgroup, all) {
            // `--all` is every process in the container's cgroup, which a process group isn't (a
            // process can leave its group with setsid); KILL goes to all of them at once.
            (Some(cg), true) => signal_cgroup(&BoxCgroup::at(PathBuf::from(cg)), sig),
            _ => sys::kill(state.pid, sig, all),
        }
    }

    /// Removes the container (OCI `delete`). A running container is refused unless `force`, which
    /// kills it first. The container's cgroup goes with it: whatever still runs there (a stopped
    /// container's init may leave processes behind without a pid namespace) is killed, the cgroup
    /// waited out and removed.
    pub fn delete(&self, id: &str, force: bool) -> Result<(), OciError> {
        let dir = self.dir(id)?;
        let state = dir.get(id)?;
        let running =
            matches!(state.status, Status::Created | Status::Running) && state.init_alive();
        if running && !force {
            return Err(OciError::WrongState(
                id.to_owned(),
                state.status.name(),
                "stopped",
            ));
        }
        if let Some(cg) = &state.cgroup {
            // Its state is kept when the cgroup won't empty, so a later delete can finish this one.
            end_cgroup(&BoxCgroup::at(PathBuf::from(cg)), id)?;
        } else if running {
            // The pidfd is opened before the kill, so the wait below is for this container and
            // not a later process given its pid; a container already gone has none to wait on.
            let pidfd = rustix::process::Pid::from_raw(state.pid)
                .and_then(|p| rustix::process::pidfd_open(p, PidfdFlags::empty()).ok());
            sys::kill(state.pid, 9, true)?;
            if let Some(pidfd) = pidfd
                && !wait_exited(&pidfd, DELETE_PATIENCE)?
            {
                // Its state is kept, so a later delete can finish what this one could not.
                return Err(OciError::Broken(format!(
                    "container {id} did not exit within {DELETE_PATIENCE:?} of SIGKILL"
                )));
            }
        }
        // poststop hooks run after the container is stopped, before its directory is removed.
        if let Some(hooks) = load_hooks(dir.path(), &state)? {
            let mut stopped = state.clone();
            stopped.status = Status::Stopped;
            hooks::run("poststop hook", &hooks.poststop, &stopped)?;
        }
        dir.remove()
    }
}

/// How long `delete --force` waits for the killed container to exit: `hex box run`'s cleanup patience
/// (`crates/hex-cli/src/box_run.rs`, `CLEANUP_PATIENCE`). A SIGKILLed process exits once it is
/// scheduled unless it is blocked in uninterruptible I/O; past this the delete is refused, typed.
const DELETE_PATIENCE: Duration = Duration::from_secs(10);

/// How long `start` waits for the init's byte on the exec FIFO, and `create` for the shim to exit once
/// it has forked the container: delete's patience. Both are already blocked or done when the wait
/// begins, so missing it is a fault to report, typed.
const START_PATIENCE: Duration = DELETE_PATIENCE;

/// Ends the shim once the container is forked and ready: it exits by itself (init.rs), and is reaped
/// here; one that outlives [`START_PATIENCE`] is killed first.
fn end_shim(shim: &mut std::process::Child) -> Result<(), OciError> {
    let pid = i32::try_from(shim.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| OciError::Broken("the shim's pid".to_owned()))?;
    let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
        .map_err(|e| OciError::os("pidfd_open of the shim", e))?;
    if !wait_exited(&pidfd, START_PATIENCE)? {
        let _ = shim.kill();
    }
    shim.wait()
        .map_err(|e| OciError::os("reaping the shim", e))?;
    Ok(())
}

/// What a start's wait on the exec FIFO came to.
#[derive(Debug)]
enum Took {
    /// The init's byte is in the FIFO: it has taken the start.
    Byte,
    /// The init ended without its byte, before or after it opened its end.
    End,
    /// Neither, within the start's patience.
    Nothing,
}

/// The start's end of the exec FIFO, opened to read and to write. Linux never blocks such an open and
/// counts it a reader and a writer (v6.19 `fs/pipe.c:1202-1216`), so the init's blocking open to
/// write returns at once, and for as long as the start holds it the FIFO has a writer and a reader.
/// Close-on-exec, so the poststart hooks don't inherit it.
fn open_start_end(path: &Path) -> rustix::io::Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Whether the FIFO `fifo` is an end of still has a name.
fn still_named(fifo: &OwnedFd) -> Result<bool, OciError> {
    rustix::fs::fstat(fifo)
        .map(|stat| stat.st_nlink > 0)
        .map_err(|e| OciError::os("reading the exec fifo's links", e))
}

/// The init's pidfd, from the state's pid, while that pid is still the init's: opened, then the start
/// time checked (proc_pid_stat(5) field 22), so the pidfd names the init or there is none.
fn init_pidfd(state: &State) -> Result<Option<OwnedFd>, OciError> {
    let Some(pid) = rustix::process::Pid::from_raw(state.pid) else {
        return Ok(None);
    };
    match rustix::process::pidfd_open(pid, PidfdFlags::empty()) {
        Ok(pidfd) if state.init_alive() => Ok(Some(pidfd)),
        Ok(_) | Err(rustix::io::Errno::SRCH) => Ok(None),
        Err(e) => Err(OciError::os("pidfd_open of the container's init", e)),
    }
}

/// Waits up to `patience` in all for the init's byte on the exec FIFO, or the init's end, and says
/// which came. The start holds its end of the FIFO to read and to write ([`open_start_end`]), so the
/// FIFO always has a writer and a reader, and Linux's `pipe_poll` reports neither a hang-up nor an
/// error on it (v6.19 `fs/pipe.c:691-692`, `:702-703`), only input: no poll can report the hang-up an
/// open for writing races with (`probes/fifo_hup.md`). The byte is counted, under the pipe's lock
/// (`FIONREAD`, `:625-637`), and never read: it stays for every start that holds the FIFO, so two
/// starts at once both see it, and the FIFO's removal says which of them reports the start. The init's
/// pidfd turns readable once it has ended (pidfd_open(2)); the init writes before it ends, so a FIFO
/// counted empty after that is an init that ended without taking the start.
fn wait_for_byte(fifo: &OwnedFd, init: &OwnedFd, patience: Duration) -> Result<Took, OciError> {
    let deadline = Instant::now().checked_add(patience);
    loop {
        let left = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => patience,
        };
        let ts = Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut fds = [
            PollFd::new(fifo, PollFlags::IN),
            PollFd::new(init, PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(OciError::os("poll on the exec fifo and the init", e)),
        }
        let [on_fifo, ended] = fds.map(|fd| fd.revents());
        // None of these can come with the start's two ends open; one that did would return every poll.
        if on_fifo.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
            return Err(OciError::Broken(format!(
                "the exec fifo polled {on_fifo:?} with the start's own end open"
            )));
        }
        let ended = ended.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
        let bytes = rustix::io::ioctl_fionread(fifo)
            .map_err(|e| OciError::os("counting the exec fifo's bytes", e))?;
        if bytes > 0 {
            return Ok(Took::Byte);
        }
        if ended {
            return Ok(Took::End);
        }
        if left.is_zero() {
            return Ok(Took::Nothing);
        }
    }
}

/// Waits up to `patience` for the process behind `pidfd` to exit; a pidfd turns readable then, whether
/// or not its parent has reaped it yet. Returns whether it exited.
fn wait_exited(pidfd: &OwnedFd, patience: Duration) -> Result<bool, OciError> {
    wait_for(pidfd, patience, "poll on the container's pidfd")
}

/// Polls `fd` for input until it has some or `patience` passes; whether it had.
fn wait_for(fd: &OwnedFd, patience: Duration, what: &'static str) -> Result<bool, OciError> {
    wait_events(fd, patience, what).map(|events| events.is_some())
}

/// Polls `fd` for input until it has some or `patience` passes: the events the poll returned, `None` past
/// `patience`.
fn wait_events(
    fd: &OwnedFd,
    patience: Duration,
    what: &'static str,
) -> Result<Option<PollFlags>, OciError> {
    let deadline = Instant::now().checked_add(patience);
    loop {
        let left = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => patience,
        };
        let ts = Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(n) if n > 0 => {
                return Ok(Some(
                    fds.first().map_or(PollFlags::empty(), PollFd::revents),
                ));
            }
            Ok(_) if left.is_zero() => return Ok(None),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(OciError::os(what, e)),
        }
    }
}

/// Makes the container's cgroup with `plan` in force.
///
/// Each ancestor below the mount point is made when missing and asked to hand down the controllers
/// the container uses. Those its resources need must be there: "The OCI runtime MUST ensure that the
/// needed cgroup controllers are enabled for the cgroup" and "MUST generate an error when the
/// configuration refers to a cgroup controller that is not present or that cannot be enabled"
/// (config-linux.md, "Unified"); [`DEFAULT_CONTROLLERS`] are added where the parent has them. The
/// cgroup itself is made, or reused when it exists empty; an occupied one is refused, as the spec
/// lets a runtime refuse a cgroup unfit for create, naming "a non-empty cgroup" (config-linux.md,
/// "Control groups"). Then the writes, in order, and the device program. A cgroup this made is removed
/// again when a later step fails.
fn make_cgroup(
    spec: &Spec,
    id: &str,
    plan: &Plan,
    state_root: &Path,
) -> Result<BoxCgroup, OciError> {
    let root = Path::new(CGROUP_ROOT);
    if !root.join("cgroup.controllers").exists() {
        return Err(OciError::os(
            "making the container's cgroup",
            format!("no cgroup v2 hierarchy at {CGROUP_ROOT}; Hex's oci class runs on cgroup v2"),
        ));
    }
    // A rootless runtime may not write the hierarchy's root: a container of a spec naming no cgroupsPath goes beside the
    // runtime's own cgroup, as crun's does ([`resources::cgroup_path_for`]). As root, where it went before.
    let rootless_own = if rustix::process::geteuid().is_root() {
        None
    } else {
        let own = crate::linux::cgroup::own_cgroup().map_err(from_refusal)?;
        Some(own.strip_prefix(root).map(Path::to_path_buf).map_err(|_| {
            OciError::os(
                "finding the runtime's own cgroup",
                format!("{} isn't under {CGROUP_ROOT}", own.display()),
            )
        })?)
    };
    let rel = resources::cgroup_path_for(
        spec.linux.cgroups_path.as_deref(),
        id,
        rootless_own.as_deref(),
    )?;
    let name = rel
        .file_name()
        .ok_or_else(|| OciError::Config(format!("linux.cgroupsPath: {rel:?} names no cgroup")))?;
    let mut wanted: BTreeSet<String> = DEFAULT_CONTROLLERS
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    wanted.extend(plan.controllers.iter().cloned());
    let mut dir = root.to_path_buf();
    for part in rel.parent().map(Path::components).into_iter().flatten() {
        enable_controllers(&dir, &wanted, &plan.controllers)?;
        dir.push(part);
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(OciError::os(
                    "making a cgroup above the container's",
                    format!("{}: {e}", dir.display()),
                ));
            }
        }
    }
    enable_controllers(&dir, &wanted, &plan.controllers)?;
    let path = dir.join(name);
    let made = match std::fs::create_dir(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if BoxCgroup::at(path.clone())
                .populated()
                .map_err(from_refusal)?
            {
                return Err(OciError::Config(format!(
                    "linux.cgroupsPath: {} already holds processes, and a new container's cgroup must be empty",
                    path.display()
                )));
            }
            false
        }
        Err(e) => {
            return Err(OciError::os(
                "making the container's cgroup",
                format!("{}: {e}", path.display()),
            ));
        }
    };
    let cg = BoxCgroup::at(path);
    if let Err(e) = apply(&cg, plan, spec.in_user_namespace(), state_root) {
        if made {
            let _ = cg.remove();
        }
        return Err(e);
    }
    Ok(cg)
}

/// The plan's writes into the cgroup, in order, then its device program, attached to it.
fn apply(cg: &BoxCgroup, plan: &Plan, rootless: bool, state_root: &Path) -> Result<(), OciError> {
    for w in &plan.writes {
        write_file(&cg.dir().join(&w.file), &w.value).map_err(from_refusal)?;
    }
    attach_device_program(cg, plan, rootless, state_root)
}

/// Attaches the plan's device program to the cgroup, if it has one: the command line's path, and a
/// provided cgroup's whose caller holds no programs. The device access a container gets is the spec's,
/// whether its cgroup was made from the spec or by the node under its box root. A container in a user
/// namespace of its own gets no program ([`Plan::check_in_userns`] says why, and what it refuses).
fn attach_device_program(
    cg: &BoxCgroup,
    plan: &Plan,
    rootless: bool,
    state_root: &Path,
) -> Result<(), OciError> {
    let Some(insns) = &plan.devices else {
        return Ok(());
    };
    if rootless {
        return plan.check_in_userns();
    }
    let prog = device_program(insns, state_root)?;
    let dir = cg.open_dir().map_err(from_refusal)?;
    sys::attach_device_program(dir.as_fd(), prog.as_fd(), sys::Attach::Multi)?;
    Ok(())
}

/// The device program to attach for `insns` (decision 2; [`devshare`]): the one the hints name for the
/// policy's digest, when the kernel shows its translation is `insns` byte for byte
/// ([`programs::program_for`]), with nothing loaded; else a copy loaded now and remembered under that
/// digest for the next create (goal 5 extended, step 1). A stale or planted hint costs that load, never
/// the policy.
fn device_program(insns: &[resources::Insn], state_root: &Path) -> Result<OwnedFd, OciError> {
    let digest = devshare::digest(insns);
    let path = state_root.join(devshare::HINTS_FILE);
    let mut hints = read_hints(&path);
    if let Some(shared) = hints
        .candidate(digest)
        .and_then(|id| programs::program_for(id, insns))
    {
        return Ok(shared);
    }
    let ours = sys::load_device_program(insns)?;
    // Without the copy's id there's nothing to remember it by; it enforces the policy all the same, so
    // it's attached, only not shared.
    if let Ok(identity) = sys::program_identity(ours.as_fd()) {
        hints.remember(digest, identity.id);
        write_hints(&path, &hints)?;
    }
    Ok(ours)
}

/// The hints in the state root. None, or a file that isn't one, is no hints: a hint is advisory, and
/// the next program remembered rewrites the file whole. A file past the bound isn't read past it.
fn read_hints(path: &Path) -> devshare::Hints {
    let mut bytes = Vec::new();
    // One byte past the bound tells a file longer than it.
    let read = std::fs::File::open(path).and_then(|f| {
        f.take(devshare::MAX_HINT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    match read {
        Ok(n) if u64::try_from(n).is_ok_and(|n| n <= devshare::MAX_HINT_BYTES) => {
            devshare::Hints::parse(&bytes)
        }
        _ => devshare::Hints::default(),
    }
}

/// Writes the hints whole: a temp file of this writer's own, renamed into place, never fsynced, like
/// the state beside it (decision 1). Concurrent writers each rename a whole file in; the last stays.
fn write_hints(path: &Path, hints: &devshare::Hints) -> Result<(), OciError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(
        "{}.{}.{n}.tmp",
        devshare::HINTS_FILE,
        std::process::id()
    ));
    std::fs::write(&tmp, hints.to_bytes()?)
        .map_err(|e| OciError::os("writing the device program hints", e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        OciError::os("renaming the device program hints", e)
    })
}

/// Asks cgroup `dir` to hand controllers down to its children: each of `required` must be available
/// there and enabled, or the container is refused by name; the rest of `wanted` are added where they
/// are available. An optional controller the parent can't hand down (it holds processes of its own,
/// which cgroup v2's no-internal-processes rule forbids beside enabled controllers) leaves the
/// container accounted in its parent for that controller, as before it had a cgroup, and bounded by
/// nothing it asked for.
fn enable_controllers(
    dir: &Path,
    wanted: &BTreeSet<String>,
    required: &BTreeSet<String>,
) -> Result<(), OciError> {
    let available =
        crate::linux::cgroup::read_set(&dir.join("cgroup.controllers")).map_err(from_refusal)?;
    if let Some(missing) = required.iter().find(|c| !available.contains(*c)) {
        return Err(OciError::Config(format!(
            "linux.resources: the {missing} controller isn't available at {} (it has {available:?})",
            dir.display()
        )));
    }
    let enabled = crate::linux::cgroup::read_set(&dir.join("cgroup.subtree_control"))
        .map_err(from_refusal)?;
    let control = dir.join("cgroup.subtree_control");
    let missing = |only_required: bool| -> Vec<String> {
        wanted
            .iter()
            .filter(|c| available.contains(*c) && !enabled.contains(*c))
            .filter(|c| required.contains(*c) == only_required)
            .map(|c| format!("+{c}"))
            .collect()
    };
    let needed = missing(true);
    if !needed.is_empty() {
        write_file(&control, &needed.join(" ")).map_err(|e| {
            OciError::Config(format!(
                "linux.resources: {} can't hand down {}: {} (a cgroup holding processes of its own can't; run under a leaf)",
                dir.display(),
                needed.join(" "),
                from_refusal(e)
            ))
        })?;
    }
    let optional = missing(false);
    if !optional.is_empty() {
        let _ = write_file(&control, &optional.join(" "));
    }
    Ok(())
}

/// Ends what runs in the container's cgroup and removes it: `cgroup.kill`, a wait bounded by
/// [`DELETE_PATIENCE`] for it to empty, then `rmdir`. A cgroup already gone is done.
fn end_cgroup(cg: &BoxCgroup, id: &str) -> Result<(), OciError> {
    if !cg.dir().is_dir() {
        return Ok(());
    }
    cg.kill().map_err(from_refusal)?;
    if !cg.wait_empty(DELETE_PATIENCE).map_err(from_refusal)? {
        return Err(OciError::Broken(format!(
            "container {id}'s cgroup still held processes {DELETE_PATIENCE:?} after cgroup.kill"
        )));
    }
    cg.remove().map_err(from_refusal)
}

/// Sends `sig` to every process in the container's cgroup: KILL to all at once through
/// `cgroup.kill`, any other to each process listed (at most [`crate::linux::cgroup::MAX_SIGNALLED`]).
fn signal_cgroup(cg: &BoxCgroup, sig: i32) -> Result<(), OciError> {
    if sig == libc::SIGKILL {
        return cg.kill().map_err(from_refusal);
    }
    for pid in cg.procs().map_err(from_refusal)? {
        if let Ok(pid) = i32::try_from(pid) {
            // A process that ended since the list was read is no longer the container's to signal.
            let _ = sys::kill(pid, sig, false);
        }
    }
    Ok(())
}

/// The sync protocol's reading half, apart from any pipe: `U<pid>` asks for the user-namespace maps
/// (`on_map`), `P<pid>` is the container's pid, `R` its ready signal, `E<why>` a setup failure.
/// Returns the pid once both `P` and `R` have come, in either order: the shim writes `P` after its
/// fork and the container writes `R` after its setup, two processes on one pipe, so under load the
/// container can finish first. `container_pid` holds the pid as soon as it arrives.
fn read_sync(
    reader: &mut impl BufRead,
    id: &str,
    hooks: bool,
    mut on: impl FnMut(Ask) -> Result<(), OciError>,
    container_pid: &mut Option<i32>,
) -> Result<i32, OciError> {
    let mut ready = false;
    // The shim's `P` and its child's `H` race on the pipe, so the hooks wait for the pid either way.
    let (mut asked, mut answered) = (false, false);
    let mut line = String::new();
    loop {
        if asked
            && !answered
            && let Some(pid) = *container_pid
        {
            on(Ask::Hooks(pid))?;
            answered = true;
        }
        if ready && let Some(pid) = *container_pid {
            if hooks && !answered {
                return Err(unhooked(id));
            }
            return Ok(pid);
        }
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| OciError::os("reading the sync pipe", e))?;
        if n == 0 {
            let what = if ready {
                "a ready signal but no container pid"
            } else {
                "no ready signal"
            };
            return Err(OciError::Broken(format!(
                "the container init for {id} closed after {what}"
            )));
        }
        let msg = line.trim_end();
        let mut chars = msg.chars();
        let tag = chars.next();
        let rest = chars.as_str();
        match tag {
            Some('U') => {
                let pid: i32 = rest
                    .parse()
                    .map_err(|_| OciError::Broken(format!("bad map request {msg:?}")))?;
                on(Ask::Maps(pid))?;
            }
            Some('H') => asked = true,
            Some('P') => {
                *container_pid = Some(
                    rest.parse()
                        .map_err(|_| OciError::Broken(format!("bad pid {msg:?}")))?,
                );
            }
            Some('R') => ready = true,
            Some('E') => {
                return Err(OciError::Os {
                    op: "container init",
                    why: rest.to_owned(),
                });
            }
            None => {}
            _ => return Err(OciError::Broken(format!("unknown sync message {msg:?}"))),
        }
    }
}

/// What the init asks of the runtime on the sync pipe, with the pid the runtime knows it by.
enum Ask {
    /// A user namespace's id maps (`U`).
    Maps(i32),
    /// The create-time hooks (`H`), the init stopped before its root switch.
    Hooks(i32),
}

/// The init's `H`: it stops before its root switch, its mounts made, for the runtime to run the prestart
/// then the createRuntime hooks in the runtime's namespace, given `created` (lifecycle steps 3 and 4 come
/// after step 2, runtime.md:20, :62-65; crun's and runtime-tools' reading) and the init's pid as the
/// runtime sees it (config.md:598, :607-610; runtime.md:26). Then the init is handed that state, for the
/// createContainer hooks it runs before the switch and the startContainer hooks it runs at `start`, each
/// given the pid the container sees (runtime.md:27; docs/research/runtime/oci-hooks.md). A hook that fails
/// fails the create, which ends the init.
fn answer_hooks(
    making: &Making<'_>,
    spec: &Spec,
    pid: i32,
    map_w: &mut impl Write,
) -> Result<(), OciError> {
    let mut state = State::new(making.id, &making.bundle.to_string_lossy());
    state.status = Status::Created;
    state.pid = pid;
    state.annotations = spec.annotations.clone();
    hooks::run("prestart hook", &spec.hooks.prestart, &state)?;
    hooks::run("createRuntime hook", &spec.hooks.create_runtime, &state)?;
    hooks::send_state(map_w, &state)
}

/// An init that said it was ready without stopping for the create-time hooks its spec names: refused,
/// never a create whose hooks didn't run (CLAUDE.md §3).
fn unhooked(id: &str) -> OciError {
    OciError::Broken(format!(
        "the container init for {id} was ready without stopping for its create-time hooks"
    ))
}

/// The hooks of container `dir` (its state directory): from the spec its create checked and kept, never its
/// bundle as it is now, since the agent runs them on the host (AC-T7.1.1). A container made before its spec was
/// kept has its bundle's spec checked again ([`checked::spec_of`]); one the checks refuse refuses the start or the
/// delete by type, and `None` only where there is no spec at all (its bundle gone), so `delete`/`start` proceed.
fn load_hooks(
    dir: &Path,
    state: &State,
) -> Result<Option<crate::linux::oci::spec::Hooks>, OciError> {
    Ok(checked::spec_of(Some(dir), Path::new(&state.bundle))?.map(|s| s.hooks))
}

/// Makes the exec FIFO, world-writable so a container that has dropped to a non-root user (or a
/// user namespace's mapped id) can still open it to wait for `start`; `mknod`'s mode is masked by
/// the umask, so the mode is set explicitly after.
/// A detached clone of the mount tree at `path` (`open_tree` with `OPEN_TREE_CLONE | AT_RECURSIVE`),
/// made in the agent's own mount namespace for a pruned create (goal 5 plan step 2): the rootfs or a
/// bind source, handed to the seed over `SCM_RIGHTS`. Needs `CAP_SYS_ADMIN` in the agent's user
/// namespace (a root node has it); where it is refused, the create takes the classic path.
fn clone_tree(path: &Path) -> Result<OwnedFd, OciError> {
    rustix::mount::open_tree(
        rustix::fs::CWD,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_RECURSIVE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    )
    .map_err(|e| OciError::os("cloning a mount source", e))
}

/// A directory opened `O_PATH` for a pruned create's seed to resolve a file in by descriptor (the
/// bundle's `config.json`, the state directory's exec FIFO).
fn open_dir(path: &Path) -> Result<OwnedFd, OciError> {
    rustix::fs::open(
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening a directory for a pruned create", e))
}

/// The container's cgroup directory opened for a pruned create, as [`crate::linux::oci::seed::open_cgroup`]
/// opens it: the seed's `clone3` is born into it (`CLONE_INTO_CGROUP`).
fn open_cgroup_dir(path: &Path) -> Result<OwnedFd, OciError> {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| OciError::os("opening the cgroup for a pruned create", e))
}

fn make_fifo(path: &Path) -> Result<(), OciError> {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        Mode::from_bits_truncate(0o622),
        rustix::fs::makedev(0, 0),
    )
    .map_err(|e| OciError::os("making the exec fifo", e))?;
    rustix::fs::chmodat(
        rustix::fs::CWD,
        path,
        Mode::from_bits_truncate(0o622),
        rustix::fs::AtFlags::empty(),
    )
    .map_err(|e| OciError::os("setting the exec fifo mode", e))
}

/// The network namespace made ahead that a container joins, asked of `networks` (decision 4): taken
/// for a container that makes a network namespace of its own in this process's user namespace; for one
/// in a user namespace of its own, made by the spec or joined by path, none, its namespace counted as
/// made on the path; and none asked for a container whose network namespace is given by path or isn't
/// its own.
fn network_for(spec: &Spec, networks: Option<&dyn Networks>) -> Result<Option<OwnedFd>, OciError> {
    let Some(networks) = networks else {
        return Ok(None);
    };
    if !spec.makes_namespace("network") {
        return Ok(None);
    }
    if spec.linux.namespaces.iter().any(|n| n.kind == "user") {
        networks.on_path()?;
        return Ok(None);
    }
    networks.take_for(spec.may_share_its_network())
}

/// Waits until the shim has written on the sync pipe or has ended, whichever comes first.
fn first_word(sync_r: &OwnedFd, shim: &std::process::Child) -> Result<(), OciError> {
    let pid = i32::try_from(shim.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| OciError::Broken("the shim's pid isn't a pid".to_owned()))?;
    // The shim is this process's child, not yet reaped, so its pid names it.
    let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
        .map_err(|e| OciError::os("pidfd_open of the shim", e))?;
    loop {
        let mut fds = [
            PollFd::new(sync_r, PollFlags::IN),
            PollFd::new(&pidfd, PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(OciError::os("poll on the sync pipe and the shim", e)),
        }
    }
}

/// Whether a pruned create with `binds` trees, `path_joins` namespaces joined by path and `ahead` descriptors made
/// ahead (a prepared `/dev`, a prepared proc's two, the masked directories' source) has room for a pair under
/// `SCM_MAX_FD`, as those are held to it, leaving the older limits as they were: a create with no room makes its
/// namespaces in its clone, counted.
fn pair_fits(binds: usize, path_joins: usize, ahead: usize) -> bool {
    const FIXED: usize = hex_core::seed::CREATE_FDS
        + 1
        + hex_core::seed::PRUNED_FIXED_FDS
        + hex_core::seed::CONFIG_FDS
        + hex_core::seed::IPC_UTS_FDS;
    binds
        .checked_add(path_joins)
        .and_then(|n| n.checked_add(FIXED))
        .and_then(|n| n.checked_add(ahead))
        .is_some_and(|n| n <= hex_core::seed::MAX_REQUEST_FDS)
}

/// The ipc and uts pair a pruned create takes (goal 5 extended, step 4): taken for a container that makes both
/// namespaces, when its create has room (`fits`); counted as made on the path for one that makes one of the two or
/// has no room, whose clone makes them; and nothing asked for one that makes neither.
fn ipc_uts_for(
    spec: &Spec,
    pool: Option<&dyn IpcUtsAhead>,
    fits: bool,
) -> Result<Option<super::ipcuts::Pair>, OciError> {
    let Some(pool) = pool else {
        return Ok(None);
    };
    match (spec.makes_namespace("ipc"), spec.makes_namespace("uts")) {
        (false, false) => Ok(None),
        (true, true) if fits => pool.take(),
        _ => pool.on_path().map(|()| None),
    }
}

/// Counts a container routed where no pair goes, through the shim or a rootless node's seed, that makes either
/// namespace: as a container in a user namespace of its own must be, since a namespace belongs to its maker's user
/// namespace, and `setns` into a pair the node made would be refused it (`ipc/namespace.c:233-244`).
fn ipc_uts_on_path(spec: &Spec, pool: Option<&dyn IpcUtsAhead>) -> Result<(), OciError> {
    match pool {
        Some(pool) if spec.makes_namespace("ipc") || spec.makes_namespace("uts") => pool.on_path(),
        _ => Ok(()),
    }
}

/// Writes a user namespace's uid and gid maps for process `pid` (OCI `uidMappings`/`gidMappings`).
/// `setgroups` is denied before the gid map, as the kernel requires when mapping without privilege.
fn write_id_maps(pid: i32, uids: &[IdMapping], gids: &[IdMapping]) -> Result<(), OciError> {
    let render = |ms: &[IdMapping]| {
        ms.iter()
            .map(|m| format!("{} {} {}", m.container_id, m.host_id, m.size))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let _ = std::fs::write(format!("/proc/{pid}/setgroups"), "deny");
    std::fs::write(format!("/proc/{pid}/uid_map"), render(uids))
        .map_err(|e| OciError::os("writing uid_map", e))?;
    std::fs::write(format!("/proc/{pid}/gid_map"), render(gids))
        .map_err(|e| OciError::os("writing gid_map", e))?;
    Ok(())
}

/// Reads a pid file's pid (helper for callers that passed `--pid-file`).
pub fn read_pid_file(path: &Path) -> Result<i32, OciError> {
    let mut s = String::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| OciError::os("reading the pid file", e))?;
    s.trim()
        .parse()
        .map_err(|_| OciError::Broken(format!("pid file {path:?} isn't a pid")))
}

#[cfg(test)]
mod tests {
    //! The sync protocol's reading half (AC-T7.1.4: a conforming `create`). The competitive bench
    //! found `create` failing under load with "ready before the container pid": the shim's `P` and
    //! the container's `R` come from two processes, in either order.

    use std::io::Cursor;

    use super::*;

    // Hostile input (CLAUDE.md §6): a container id is a directory name under the state root, so one
    // that could name another place is refused before anything is touched; `delete ../../x` would
    // otherwise remove a directory outside the root. runc takes ids of [\w+.-] alone.
    #[test]
    fn an_id_that_isnt_a_plain_name_is_refused_before_anything_is_touched() {
        let root = tempfile::tempdir().unwrap();
        let rt = Runtime::new(root.path().join("state"));
        let victim = root.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("state.json"), b"{}").unwrap();
        for id in [
            "",
            ".",
            "..",
            "../victim",
            "a/b",
            "a\0b",
            "a b",
            &"x".repeat(256),
        ] {
            for result in [
                rt.create(id, root.path(), None).map(|_| ()),
                rt.start(id),
                rt.kill(id, 9, true),
                rt.delete(id, true),
                rt.state(id).map(|_| ()),
            ] {
                match result {
                    Err(OciError::Config(why)) => assert!(why.contains("id"), "{id:?}: {why}"),
                    other => panic!("{id:?}: {other:?}"),
                }
            }
        }
        assert!(victim.join("state.json").exists(), "the victim survives");
        assert!(!root.path().join("state").exists(), "nothing was made");
        for id in ["c1", "hex-l000001", "a.b+c_d"] {
            assert!(check_id(id).is_ok(), "{id}");
        }
    }

    fn read(wire: &str) -> (Result<i32, OciError>, Option<i32>, Vec<i32>) {
        let (got, pid, maps, _) = read_hooked(wire, false);
        (got, pid, maps)
    }

    /// `read`, for a spec with create-time hooks when `hooks` is set: also the pids the hooks were asked
    /// for, in order.
    fn read_hooked(
        wire: &str,
        hooks: bool,
    ) -> (Result<i32, OciError>, Option<i32>, Vec<i32>, Vec<i32>) {
        let (mut maps, mut hooked) = (Vec::new(), Vec::new());
        let mut pid = None;
        let got = read_sync(
            &mut Cursor::new(wire.as_bytes()),
            "c1",
            hooks,
            |ask| {
                match ask {
                    Ask::Maps(p) => maps.push(p),
                    Ask::Hooks(p) => hooked.push(p),
                }
                Ok(())
            },
            &mut pid,
        );
        (got, pid, maps, hooked)
    }

    /// The create-time hooks (AC-T7.1.4; config.md:607, :619): the shim's child stops for them before
    /// its root switch, and its `H` and the shim's `P` race on the pipe. Do: read each order. Expect: the
    /// hooks run once, with the pid, whichever came first, before the ready signal is taken.
    #[test]
    fn the_hooks_run_once_with_the_pid_whichever_line_comes_first() {
        for wire in ["H\nP42\nR\n", "P42\nH\nR\n"] {
            let (got, _, _, hooked) = read_hooked(wire, true);
            assert_eq!((got.unwrap(), hooked), (42, vec![42]), "{wire:?}");
        }
    }

    /// CLAUDE.md §3: a create whose spec names create-time hooks is never reported made without them. Do:
    /// an init that says it's ready without stopping for them. Expect: the create refused, by type.
    #[test]
    fn an_init_ready_without_stopping_for_its_hooks_is_refused() {
        let (got, _, _, hooked) = read_hooked("P42\nR\n", true);
        assert!(
            matches!(&got, Err(OciError::Broken(why)) if why.contains("create-time hooks")),
            "{got:?}"
        );
        assert!(hooked.is_empty());
    }

    #[test]
    fn ready_before_the_pid_still_gives_the_pid() {
        let (got, pid, _) = read("R\nP4242\n");
        assert_eq!(got.unwrap(), 4242);
        assert_eq!(pid, Some(4242));
    }

    #[test]
    fn the_pid_then_ready_gives_the_pid() {
        assert_eq!(read("P17\nR\n").0.unwrap(), 17);
    }

    #[test]
    fn a_map_request_is_answered_and_the_pid_still_follows() {
        let (got, _, maps) = read("U900\nP901\nR\n");
        assert_eq!(got.unwrap(), 901);
        assert_eq!(maps, vec![900]);
    }

    #[test]
    fn a_setup_failure_is_the_error_and_keeps_the_pid_for_cleanup() {
        let (got, pid, _) = read("P55\nEpivot_root: EINVAL\n");
        assert!(
            matches!(got, Err(OciError::Os { op: "container init", ref why }) if why == "pivot_root: EINVAL")
        );
        assert_eq!(
            pid,
            Some(55),
            "a failed create must still know the container to end"
        );
    }

    #[test]
    fn closing_without_ready_is_refused_with_the_pid_kept() {
        let (got, pid, _) = read("P8\n");
        assert!(matches!(got, Err(OciError::Broken(ref why)) if why.contains("no ready signal")));
        assert_eq!(pid, Some(8));
    }

    #[test]
    fn closing_after_ready_without_a_pid_is_refused() {
        let (got, pid, _) = read("R\n");
        assert!(matches!(got, Err(OciError::Broken(ref why)) if why.contains("no container pid")));
        assert_eq!(pid, None);
    }

    /// `delete --force` waits for the killed container to exit (AC-T7.1.4); the competitive bench
    /// found it returning at once, which made its delete look 16x faster than crun's.
    fn sleeper() -> (std::process::Child, OwnedFd) {
        // Until it is killed: each test that starts one ends it.
        let child = std::process::Command::new("sleep")
            .arg("infinity")
            .spawn()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty()).unwrap();
        (child, pidfd)
    }

    #[test]
    fn a_killed_process_is_seen_to_exit() {
        let (mut child, pidfd) = sleeper();
        child.kill().unwrap();
        assert!(wait_exited(&pidfd, DELETE_PATIENCE).unwrap());
        child.wait().unwrap();
    }

    /// A live process isn't seen to exit when the patience is spent, however short: none at all is the shortest.
    #[test]
    fn a_live_process_outlasts_a_short_patience() {
        let (mut child, pidfd) = sleeper();
        assert!(!wait_exited(&pidfd, Duration::ZERO).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// An exec FIFO and the start's end of it, opened as `start_with` opens it, with no writer yet.
    fn exec_fifo() -> (tempfile::TempDir, PathBuf, OwnedFd) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exec.fifo");
        make_fifo(&path).unwrap();
        let start = open_start_end(&path).unwrap();
        (dir, path, start)
    }

    fn open_to_write(path: &Path) -> OwnedFd {
        rustix::fs::open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty()).unwrap()
    }

    /// A start sees the init's byte whichever of their ends opened first (AC-T7.1.4: a conforming
    /// `start`). Do: once with the init's end opened after the start's, once with the init blocked
    /// opening its end before the start opens, as init.rs opens it. Expect the byte both times.
    #[test]
    fn a_start_sees_the_inits_byte_whichever_end_opens_first() {
        let (mut init, pidfd) = sleeper();
        let (_dir, path, start) = exec_fifo();
        let end = open_to_write(&path);
        assert_eq!(rustix::io::write(&end, b"0").unwrap(), 1);
        let took = wait_for_byte(&start, &pidfd, START_PATIENCE);
        assert!(matches!(took, Ok(Took::Byte)), "{took:?}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exec.fifo");
        make_fifo(&path).unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let end = open_to_write(&path);
                assert_eq!(rustix::io::write(&end, b"0").unwrap(), 1);
            });
            let start = open_start_end(&path).unwrap();
            let took = wait_for_byte(&start, &pidfd, START_PATIENCE);
            assert!(matches!(took, Ok(Took::Byte)), "{took:?}");
            writer.join().unwrap();
        });
        init.kill().unwrap();
        init.wait().unwrap();
    }

    /// The race the competitive bench met, "reading the exec fifo failed: Resource temporarily
    /// unavailable" (`probes/fifo_hup.md`), needs a poll's hang-up with the init there: a FIFO whose
    /// writers' counter moved while it had none. Do: open the start's end, then open and close a writer
    /// with no byte, with the init alive; then the init writes its byte. Expect: no hang-up in a poll of
    /// the FIFO, and the start waiting out a short patience (where a reader alone, the start's end before
    /// this, reads end-of-file), then finding the byte.
    #[test]
    fn a_writer_that_came_and_went_is_no_end_while_the_init_lives() {
        let (mut init, pidfd) = sleeper();
        let (_dir, path, start) = exec_fifo();
        drop(open_to_write(&path));
        let mut fds = [PollFd::new(&start, PollFlags::IN)];
        let none = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(rustix::event::poll(&mut fds, Some(&none)).unwrap(), 0);
        let took = wait_for_byte(&start, &pidfd, Duration::ZERO);
        assert!(matches!(took, Ok(Took::Nothing)), "{took:?}");
        let end = open_to_write(&path);
        assert_eq!(rustix::io::write(&end, b"0").unwrap(), 1);
        let took = wait_for_byte(&start, &pidfd, START_PATIENCE);
        assert!(matches!(took, Ok(Took::Byte)), "{took:?}");
        init.kill().unwrap();
        init.wait().unwrap();
    }

    /// An init that ended without its byte ends the start's wait at its end, by its pidfd, not at the
    /// patience: the start is refused as stopped (AC-T7.1.4). Do: an init that never opened its end,
    /// and one that opened it and ended. Expect End both times.
    #[test]
    fn an_init_that_ended_without_its_byte_is_stopped_by_its_pidfd() {
        for opened in [false, true] {
            let (mut init, pidfd) = sleeper();
            let (_dir, path, start) = exec_fifo();
            let end = opened.then(|| open_to_write(&path));
            init.kill().unwrap();
            init.wait().unwrap();
            drop(end);
            let took = wait_for_byte(&start, &pidfd, START_PATIENCE);
            assert!(matches!(took, Ok(Took::End)), "opened {opened}: {took:?}");
        }
    }

    /// An init that wrote its byte and then ended (its program done at once) took the start: started,
    /// the byte counted past the pidfd's end (AC-T7.1.4).
    #[test]
    fn an_init_that_wrote_its_byte_and_ended_is_started() {
        let (mut init, pidfd) = sleeper();
        let (_dir, path, start) = exec_fifo();
        let end = open_to_write(&path);
        assert_eq!(rustix::io::write(&end, b"0").unwrap(), 1);
        drop(end);
        init.kill().unwrap();
        init.wait().unwrap();
        let took = wait_for_byte(&start, &pidfd, START_PATIENCE);
        assert!(matches!(took, Ok(Took::Byte)), "{took:?}");
    }

    /// A created container's state directory as create leaves it: its exec FIFO, and its state naming
    /// init `pid`, started at `start_time`, with no poststart hooks.
    fn created(rt: &Runtime, id: &str, pid: i32, start_time: Option<u64>) -> Dir {
        let dir = rt.dir(id).unwrap();
        dir.create(id).unwrap();
        make_fifo(&dir.fifo()).unwrap();
        let mut state = State::new(id, "/bundle");
        state.status = Status::Created;
        state.pid = pid;
        state.start_time = start_time;
        state.poststart = Some(false);
        dir.put(&state).unwrap();
        dir
    }

    /// Two starts of one container at once (AC-T7.1.4: a conforming `start`): exactly one reports it
    /// started, and the other finds it running, a typed refusal, at once rather than at the start's
    /// patience; neither reads a byte the init didn't write. Do: a created container whose init (a
    /// sleeper, its end of the exec FIFO opened here as init.rs opens it) writes its byte while two
    /// starts race for it, 64 times. Expect one start and one `running` refusal each time, and the FIFO
    /// gone.
    #[test]
    fn of_two_starts_at_once_one_starts_and_the_other_finds_the_container_running() {
        let (mut init, _pidfd) = sleeper();
        let pid = i32::try_from(init.id()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let rt = Runtime::new(root.path().join("state"));
        let running = |r: &Result<(), OciError>| {
            matches!(r, Err(OciError::WrongState(_, "running", "created")))
        };
        for round in 0..64 {
            let id = format!("c{round}");
            let dir = created(&rt, &id, pid, state::start_time(pid));
            let fifo = dir.fifo();
            let (a, b) = std::thread::scope(|scope| {
                let end = scope.spawn(|| {
                    let end = open_to_write(&fifo);
                    rustix::io::write(&end, b"0").unwrap()
                });
                let a = scope.spawn(|| rt.start(&id));
                let b = scope.spawn(|| rt.start(&id));
                let (a, b) = (a.join().unwrap(), b.join().unwrap());
                // Had neither start opened its end, the init's open would wait for ever: a reader opened
                // here lets it return, so the test fails rather than hangs.
                let _reader = rustix::fs::open(
                    &fifo,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                );
                assert_eq!(end.join().unwrap(), 1);
                (a, b)
            });
            assert!(
                (a.is_ok() && running(&b)) || (running(&a) && b.is_ok()),
                "round {round}: {a:?}, {b:?}"
            );
            assert!(!dir.fifo_present(), "round {round}");
        }
        init.kill().unwrap();
        init.wait().unwrap();
    }

    /// An open whose lookup came before another start's removal and which landed after that start's close
    /// holds a new pipe on a FIFO with no name, the byte gone with the old pipe (v6.19 `fs/pipe.c:709-721`,
    /// `:1129-1147`), so it reads as no longer named, not as a byte to wait for (AC-T7.1.4). Do: hold the FIFO
    /// as a lookup holds it (`O_PATH`), remove it and close a first start's end, then open what's held. Expect:
    /// named before the removal, no name after. The byte's going isn't asserted: a process another test forks
    /// holds the first end, and so the old pipe, until its exec.
    #[test]
    fn an_open_crossing_anothers_removal_and_close_finds_no_name() {
        let (_dir, path, first) = exec_fifo();
        assert!(still_named(&first).unwrap());
        let held = rustix::fs::open(&path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap();
        std::fs::remove_file(&path).unwrap();
        drop(first);
        let second = rustix::fs::open(
            format!("/proc/self/fd/{}", held.as_raw_fd()),
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        assert!(!still_named(&second).unwrap());
    }

    /// A start handed no pidfd opens the init's by the state's pid and checks the start time create
    /// recorded before it trusts it, so a later process given the pid is never taken for the init
    /// (AC-T7.1.4). Do: a created container whose recorded init started a tick before the live process
    /// at its pid, a byte in its FIFO; then one whose recorded start time is the process's own. Expect the
    /// first refused as stopped with its FIFO kept, and the second started.
    #[test]
    fn a_pid_given_to_a_later_process_is_never_taken_for_the_init() {
        let (mut init, _pidfd) = sleeper();
        let pid = i32::try_from(init.id()).unwrap();
        let began = state::start_time(pid).unwrap();
        let root = tempfile::tempdir().unwrap();
        let rt = Runtime::new(root.path().join("state"));
        for (id, recorded, starts) in [
            ("c1", began.checked_sub(1), false),
            ("c2", Some(began), true),
        ] {
            let dir = created(&rt, id, pid, recorded);
            // Opened to read and to write, so it never blocks: the byte waits as an init leaves it.
            let end = rustix::fs::open(
                dir.fifo(),
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            assert_eq!(rustix::io::write(&end, b"0").unwrap(), 1);
            let got = rt.start(id);
            if starts {
                assert!(got.is_ok(), "{id}: {got:?}");
            } else {
                assert!(
                    matches!(got, Err(OciError::WrongState(_, "stopped", "created"))),
                    "{id}: {got:?}"
                );
            }
            assert_eq!(dir.fifo_present(), !starts, "{id}");
        }
        init.kill().unwrap();
        init.wait().unwrap();
    }

    #[test]
    fn an_unknown_message_is_refused() {
        assert!(
            matches!(read("Xhello\n").0, Err(OciError::Broken(ref why)) if why.contains("unknown sync message"))
        );
    }

    /// A stand-in for the shim: writes which of its descriptors are network namespaces, what the path
    /// it was handed for the namespace made ahead names, and the flags the agent's descriptor has
    /// meanwhile, then ends without a word on the sync pipe.
    const STAND_IN: &str = r#"#!/bin/sh
for fd in /proc/$$/fd/*; do
  case "$(readlink "$fd")" in net:*) echo "inherited $(readlink "$fd")" ;; esac
done > "$HEX_OCI_DIR/report"
echo "named $(readlink "$HEX_OCI_NETWORK")" >> "$HEX_OCI_DIR/report"
sed -n 's/^flags:[[:space:]]*/flags /p' "$(echo "$HEX_OCI_NETWORK" | sed 's#/fd/#/fdinfo/#')" >> "$HEX_OCI_DIR/report"
"#;

    /// A stand-in shim at `path`, written by a child process. Written from here, a descriptor open to write
    /// it could reach a process another test forks in that moment, and until that process's exec closed it
    /// Linux would refuse to run the file (ETXTBSY, execve(2)), as unit hex-g5x-s6-fdh9-20261103T215230Z saw.
    fn stand_in_at(path: &Path, text: &str) {
        let mut writer = Command::new("sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
    }

    /// HEX-NETNS-6 (decision 4, goal 4 plan step 4): a create hands its shim a network namespace made
    /// ahead by the path of the agent's descriptor for it, which stays close-on-exec, so neither the
    /// shim nor any process the multi-threaded agent starts meanwhile (another create's shim, an exec'd
    /// program) inherits it. Do: start a stand-in shim for a container with a namespace made ahead
    /// (this process's own stands in for it). Expect: none of the stand-in's descriptors is a network
    /// namespace, the path it was handed names the namespace, and the agent's descriptor is
    /// close-on-exec while the stand-in runs.
    #[test]
    fn a_namespace_made_ahead_reaches_the_shim_by_path_and_no_process_inherits_it() {
        let lab = tempfile::tempdir().unwrap();
        let stand_in = lab.path().join("shim");
        stand_in_at(&stand_in, STAND_IN);
        let rt = Runtime::with_helper(lab.path().join("state"), stand_in);
        let dir = rt.dir("c1").unwrap();
        dir.create("c1").unwrap();
        let spec = Spec::parse(
            br#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"linux":{"namespaces":[{"type":"network"}]}}"#,
        )
        .unwrap();
        let network = rustix::fs::open(
            "/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let named = std::fs::read_link("/proc/self/ns/net").unwrap();
        let checked = checked::seal(b"{}").unwrap();
        let making = Making {
            id: "c1",
            bundle: lab.path(),
            dir: &dir,
            pid_file: None,
            config: checked.as_fd(),
        };
        let cgroup = BoxCgroup::at(lab.path().to_owned());
        assert!(
            rt.start_shim(
                &making,
                &spec,
                &cgroup,
                Ahead {
                    network: Some(network),
                    dev: None,
                    masked_directory: None,
                },
                ShimStdio::Inherited,
                &Command::spawn
            )
            .is_err(),
            "the stand-in ends without a word"
        );

        let report = std::fs::read_to_string(dir.path().join("report")).unwrap();
        assert!(!report.contains("inherited"), "{report}");
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[0], format!("named {}", named.display()), "{report}");
        let flags = u32::from_str_radix(lines[1].strip_prefix("flags ").unwrap(), 8).unwrap();
        assert_ne!(flags & 0o2_000_000, 0, "O_CLOEXEC in {report}");
    }

    /// The kernel rule [`stand_in_at`] works around (execve(2), ETXTBSY; AC-T7.1.4, CLAUDE.md §4). Do: write a
    /// stand-in, have another process open it to write and keep it open, run the stand-in; then end that process and
    /// run it again. Expect: refused with "Text file busy" while the other process holds it open to write, and run
    /// once none does. A process another test forks while this one's descriptor is open holds it the same way until
    /// its exec, which is how the namespace test failed when it wrote its stand-in itself (unit
    /// hex-native-s7-gates-20261105T110236Z).
    #[test]
    fn a_file_another_process_holds_open_to_write_is_refused_to_run() {
        use std::io::BufRead as _;

        let lab = tempfile::tempdir().unwrap();
        let path = lab.path().join("stand-in");
        stand_in_at(&path, "#!/bin/sh\nexit 0\n");
        let mut holder = Command::new("sh")
            .args(["-c", "exec 3>>\"$0\" && echo ready && exec sleep infinity"])
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(
            ready.trim(),
            "ready",
            "the holder opened the stand-in to write"
        );
        let busy = Command::new(&path).status();
        let _ = holder.kill();
        let _ = holder.wait();
        assert_eq!(
            busy.as_ref().err().and_then(std::io::Error::raw_os_error),
            Some(libc::ETXTBSY),
            "refused while another process holds it open to write: {busy:?}"
        );
        assert!(Command::new(&path).status().unwrap().success());
    }

    /// [`stand_in_at`] under the overlap that flaked (AC-T7.1.4, CLAUDE.md §4). Do: while another thread starts
    /// processes without pause, as the other tests do, write a stand-in through [`stand_in_at`] and run it, 200
    /// times. Expect: every one runs, none refused "Text file busy", and the other thread started processes
    /// meanwhile (so the overlap was there to meet).
    #[test]
    fn a_stand_in_written_by_a_child_runs_while_other_threads_start_processes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU32};

        let lab = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicU32::new(0));
        let spawner = {
            let (stop, started) = (Arc::clone(&stop), Arc::clone(&started));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                    started.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let mut refused = Vec::new();
        for i in 0..200 {
            let path = lab.path().join(format!("stand-in-{i}"));
            stand_in_at(&path, "#!/bin/sh\nexit 0\n");
            match Command::new(&path).status() {
                Ok(status) => assert!(status.success(), "stand-in {i}: {status}"),
                Err(e) => refused.push((i, e)),
            }
        }
        stop.store(true, Ordering::Relaxed);
        spawner.join().unwrap();
        assert!(refused.is_empty(), "stand-ins refused: {refused:?}");
        assert!(
            started.load(Ordering::Relaxed) > 0,
            "the other thread started processes meanwhile"
        );
    }

    /// A process another of the agent's threads starts while a create starts its shim (another create's
    /// shim, an exec'd program, a hook) inherits none of the create's descriptors: the shim's pipes stay
    /// close-on-exec in the agent (CVE-2024-21626's class: runc v1.5.2 `libcontainer/container_linux.go:341-349`).
    /// Do: start a stand-in shim with a bystander started inside the spawn, as another thread's would be.
    /// Expect: the bystander holds no pipe past its stdio.
    #[test]
    fn a_process_started_while_a_shim_starts_inherits_none_of_its_descriptors() {
        let lab = tempfile::tempdir().unwrap();
        let stand_in = lab.path().join("shim");
        stand_in_at(&stand_in, "#!/bin/sh\nexit 0\n");
        let rt = Runtime::with_helper(lab.path().join("state"), stand_in);
        let dir = rt.dir("c1").unwrap();
        dir.create("c1").unwrap();
        let spec = Spec::parse(br#"{"ociVersion":"1.2.0","root":{"path":"rootfs"}}"#).unwrap();
        let checked = checked::seal(b"{}").unwrap();
        let making = Making {
            id: "c1",
            bundle: lab.path(),
            dir: &dir,
            pid_file: None,
            config: checked.as_fd(),
        };
        let cgroup = BoxCgroup::at(lab.path().to_owned());
        // What the bystander holds is read, and it is ended, inside the spawn: a pipe it inherited would
        // otherwise keep the create's handshake from its end-of-file for as long as it lives.
        let held = std::cell::RefCell::new(Vec::new());
        let spawn = |cmd: &mut Command| {
            let mut bystander = Command::new("sleep").arg("infinity").spawn()?;
            // Linux lets a vfork parent go before the exec closes what is close-on-exec (v6.19
            // `fs/exec.c:1156`, `:1194`, `kernel/fork.c:1488`), and sets the new program's arguments after
            // that (`fs/binfmt_elf.c:1296`): what it holds is read once it shows them.
            let cmdline = format!("/proc/{}/cmdline", bystander.id());
            let deadline = Instant::now() + Duration::from_secs(10);
            while !std::fs::read(&cmdline)?.starts_with(b"sleep\0") {
                assert!(
                    Instant::now() < deadline,
                    "the bystander never became sleep"
                );
                std::thread::yield_now();
            }
            for fd in std::fs::read_dir(format!("/proc/{}/fd", bystander.id()))? {
                let fd = fd?;
                let stdio = ["0", "1", "2"].contains(&fd.file_name().to_string_lossy().as_ref());
                let target = std::fs::read_link(fd.path())?;
                if !stdio && target.to_string_lossy().starts_with("pipe:") {
                    held.borrow_mut().push(target);
                }
            }
            bystander.kill()?;
            bystander.wait()?;
            cmd.spawn()
        };
        assert!(
            rt.start_shim(
                &making,
                &spec,
                &cgroup,
                Ahead {
                    network: None,
                    dev: None,
                    masked_directory: None,
                },
                ShimStdio::Inherited,
                &spawn,
            )
            .is_err(),
            "the stand-in ends without a word"
        );
        let held = held.into_inner();
        assert!(held.is_empty(), "the bystander inherited {held:?}");
    }

    /// A hook holds nothing the agent above it left open (CVE-2024-21626's class; decision 3, "Descriptors reach a
    /// program only on purpose"; AC-T7.1.4). Do: leave a file open in this process without close-on-exec, as a
    /// window above a program would, and run a hook that writes what each of its descriptors names. Expect: its
    /// stdio there, and the file not.
    #[test]
    fn a_hook_holds_nothing_the_agent_left_open() {
        let lab = tempfile::tempdir().unwrap();
        let stray_at = lab.path().join("stray");
        std::fs::write(&stray_at, b"").unwrap();
        let _stray = rustix::fs::open(&stray_at, OFlags::RDONLY, Mode::empty()).unwrap();
        let report = lab.path().join("report");
        let hook_at = lab.path().join("hook");
        stand_in_at(
            &hook_at,
            &format!(
                "#!/bin/sh\nfor fd in /proc/$$/fd/*; do readlink \"$fd\"; done > {}\nexit 0\n",
                report.display()
            ),
        );
        let hook = crate::linux::oci::spec::Hook {
            path: hook_at.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: Vec::new(),
            timeout: None,
        };
        hooks::run("a hook", &[hook], &State::new("c1", "/bundle")).unwrap();
        let held = std::fs::read_to_string(&report).unwrap();
        assert!(held.lines().count() >= 3, "its stdio: {held}");
        assert!(!held.contains("stray"), "the hook inherited it: {held}");
    }

    /// A pool that records what it is asked, its take answered by `answer`.
    struct Recorded {
        takes: std::cell::Cell<u32>,
        on_paths: std::cell::Cell<u32>,
        answer: fn() -> Result<Option<OwnedFd>, OciError>,
        refuse_on_path: bool,
    }

    impl Recorded {
        fn answering(answer: fn() -> Result<Option<OwnedFd>, OciError>) -> Self {
            Self {
                takes: std::cell::Cell::new(0),
                on_paths: std::cell::Cell::new(0),
                answer,
                refuse_on_path: false,
            }
        }

        fn asked(&self) -> (u32, u32) {
            (self.takes.get(), self.on_paths.get())
        }
    }

    impl Networks for Recorded {
        fn take(&self) -> Result<Option<OwnedFd>, OciError> {
            self.takes.set(self.takes.get() + 1);
            (self.answer)()
        }

        fn on_path(&self) -> Result<(), OciError> {
            self.on_paths.set(self.on_paths.get() + 1);
            if self.refuse_on_path {
                return Err(OciError::Broken("shut down".to_owned()));
            }
            Ok(())
        }
    }

    fn pooled() -> Result<Option<OwnedFd>, OciError> {
        Ok(Some(
            rustix::fs::open(
                "/proc/self/ns/net",
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap(),
        ))
    }

    fn dry() -> Result<Option<OwnedFd>, OciError> {
        Ok(None)
    }

    fn refused() -> Result<Option<OwnedFd>, OciError> {
        Err(OciError::Broken(
            "the network-namespace pool is shut down".to_owned(),
        ))
    }

    /// A pool of pairs as a create meets it, recording what it's asked.
    struct Pairs {
        takes: std::cell::Cell<u32>,
        on_paths: std::cell::Cell<u32>,
        dry: bool,
    }

    impl Pairs {
        fn new(dry: bool) -> Self {
            Self {
                takes: std::cell::Cell::new(0),
                on_paths: std::cell::Cell::new(0),
                dry,
            }
        }

        fn asked(&self) -> (u32, u32) {
            (self.takes.get(), self.on_paths.get())
        }
    }

    impl IpcUtsAhead for Pairs {
        fn take(&self) -> Result<Option<crate::linux::oci::ipcuts::Pair>, OciError> {
            self.takes.set(self.takes.get() + 1);
            if self.dry {
                return Ok(None);
            }
            crate::linux::oci::ipcuts::Pair::of_this_thread().map(Some)
        }

        fn on_path(&self) -> Result<(), OciError> {
            self.on_paths.set(self.on_paths.get() + 1);
            Ok(())
        }
    }

    /// HEX-IPCUTS-8 (goal 5 extended, step 4). Do: ask for each kind of container, on the pruned route and off it.
    /// Expect: one that makes both takes a pair, or none from a dry pool; one that makes one of the two, or has no
    /// room, is counted on the path; one that makes neither asks nothing; off the route, one in a user namespace of
    /// its own, or any that makes either, is counted on the path and takes none.
    #[test]
    fn a_pair_goes_only_to_a_pruned_create_that_makes_both_and_has_room() {
        let both = spec_with(r#"{"type":"pid"},{"type":"ipc"},{"type":"uts"}"#);
        let pool = Pairs::new(false);
        assert!(ipc_uts_for(&both, Some(&pool), true).unwrap().is_some());
        assert_eq!(pool.asked(), (1, 0));
        assert!(ipc_uts_for(&both, Some(&pool), false).unwrap().is_none());
        assert_eq!(pool.asked(), (1, 1));
        let dry = Pairs::new(true);
        assert!(ipc_uts_for(&both, Some(&dry), true).unwrap().is_none());
        assert_eq!(dry.asked(), (1, 0));
        assert!(ipc_uts_for(&both, None, true).unwrap().is_none());
        for one in [
            r#"{"type":"ipc"}"#,
            r#"{"type":"uts"}"#,
            r#"{"type":"ipc"},{"type":"uts","path":"/proc/1/ns/uts"}"#,
        ] {
            let pool = Pairs::new(false);
            assert!(
                ipc_uts_for(&spec_with(one), Some(&pool), true)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(pool.asked(), (0, 1), "{one}");
        }
        let neither = spec_with(r#"{"type":"pid"},{"type":"mount"}"#);
        let pool = Pairs::new(false);
        assert!(ipc_uts_for(&neither, Some(&pool), true).unwrap().is_none());
        ipc_uts_on_path(&neither, Some(&pool)).unwrap();
        assert_eq!(pool.asked(), (0, 0));
        let user = spec_with(r#"{"type":"user"},{"type":"ipc"},{"type":"uts"}"#);
        ipc_uts_on_path(&user, Some(&pool)).unwrap();
        assert_eq!(pool.asked(), (0, 1));
    }

    /// HEX-IPCUTS-8's room: do ask at `SCM_MAX_FD`'s edge; expect the pair to fit up to it and not past it, a prepared
    /// `/dev` counted.
    #[test]
    fn a_pair_fits_up_to_the_most_descriptors_a_create_carries() {
        use hex_core::seed::{
            CONFIG_FDS, CREATE_FDS, IPC_UTS_FDS, MAX_REQUEST_FDS, PRUNED_FIXED_FDS,
        };
        let most = MAX_REQUEST_FDS - CREATE_FDS - 1 - PRUNED_FIXED_FDS - CONFIG_FDS - IPC_UTS_FDS;
        assert!(pair_fits(most, 0, 0));
        assert!(!pair_fits(most + 1, 0, 0));
        assert!(!pair_fits(most, 0, 1));
        assert!(pair_fits(most - 4, 3, 1));
        // A prepared `/dev`, a prepared proc's two and the masked directories' source count alike.
        assert!(pair_fits(most - 4, 0, 4));
        assert!(!pair_fits(most - 4, 0, 5));
        assert!(!pair_fits(usize::MAX, 1, 0));
    }

    fn spec_with(namespaces: &str) -> Spec {
        Spec::parse(
            format!(
                r#"{{"ociVersion":"1.2.0","root":{{"path":"rootfs"}},"linux":{{"namespaces":[{namespaces}]}}}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    /// HEX-NETNS-7 (decision 4, goal 4 plan step 4): a create asks the pool once it has read the spec.
    /// Do: choose for each kind of container. Expect: one that makes its own network namespace in this
    /// user namespace takes one, and joins it when the take gives one or makes its own on the path when
    /// the pool is dry; one with a user namespace of its own, made or joined, is counted on the path and
    /// takes none; one whose network namespace is given by path, or isn't its own, asks nothing; a
    /// refused take or count refuses the create; and a create with no pool asks nothing.
    #[test]
    fn a_create_asks_the_pool_for_what_its_container_can_join() {
        let own = spec_with(r#"{"type":"pid"},{"type":"network"}"#);
        let pool = Recorded::answering(pooled);
        assert!(network_for(&own, Some(&pool)).unwrap().is_some());
        assert_eq!(pool.asked(), (1, 0), "one take");

        let pool = Recorded::answering(dry);
        assert!(network_for(&own, Some(&pool)).unwrap().is_none());
        assert_eq!(pool.asked(), (1, 0), "a dry take, made on the path");

        for user in [
            r#"{"type":"user"}"#,
            r#"{"type":"user","path":"/proc/1/ns/user"}"#,
        ] {
            let pool = Recorded::answering(pooled);
            let spec = spec_with(&format!(r#"{user},{{"type":"network"}}"#));
            assert!(network_for(&spec, Some(&pool)).unwrap().is_none(), "{user}");
            assert_eq!(pool.asked(), (0, 1), "counted on the path: {user}");
        }

        for namespaces in [
            r#"{"type":"pid"},{"type":"network","path":"/proc/1/ns/net"}"#,
            r#"{"type":"pid"}"#,
            r#"{"type":"user"},{"type":"network","path":"/proc/1/ns/net"}"#,
        ] {
            let pool = Recorded::answering(pooled);
            assert!(
                network_for(&spec_with(namespaces), Some(&pool))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(pool.asked(), (0, 0), "nothing asked: {namespaces}");
        }

        let pool = Recorded::answering(refused);
        assert!(network_for(&own, Some(&pool)).is_err(), "a refused take");
        let pool = Recorded {
            refuse_on_path: true,
            ..Recorded::answering(pooled)
        };
        let spec = spec_with(r#"{"type":"user"},{"type":"network"}"#);
        assert!(network_for(&spec, Some(&pool)).is_err(), "a refused count");

        assert!(network_for(&own, None).unwrap().is_none(), "no pool");
    }

    /// A stand-in for a node's create seeds whose line is full: every clone is refused at the seeds' bound, and each
    /// classic route is noted. They keep the agent's mount namespace for `kept`.
    struct FullLine {
        classic: std::cell::RefCell<Vec<ClassicReason>>,
        kept: ClassicReason,
    }

    impl CloneInit for FullLine {
        fn clone_init(&self, _req: CloneReq<'_>) -> Result<(i32, OwnedFd), OciError> {
            Err(OciError::SeedsExhausted {
                bound: 2,
                why: "no seed is free and 2 creates already wait for one".to_owned(),
            })
        }
        fn claim(
            &self,
            _parked: Parked,
            _req: &CloneReq<'_>,
            _sync: BorrowedFd<'_>,
        ) -> Result<(i32, OwnedFd), OciError> {
            Err(OciError::Broken("no init is parked here".to_owned()))
        }
        fn unpruned(&self) -> Option<ClassicReason> {
            Some(self.kept)
        }
        fn mount_namespace(&self) -> super::super::mntns::MountNamespace {
            super::super::mntns::MountNamespace::Copy
        }
        fn classic(&self, reason: ClassicReason) {
            self.classic.borrow_mut().push(reason);
        }
    }

    /// A chooser that gives every create the one cgroup it holds, carrying its program already.
    struct OneCgroup(PathBuf);

    impl Choose for OneCgroup {
        fn choose(&self, _wants: &Wants<'_>) -> Result<Chosen, OciError> {
            Ok(Chosen {
                cgroup: BoxCgroup::at(self.0.clone()),
                attached: true,
                parked: None,
            })
        }
        fn init_made(&self, _flags: u64, _claimed: bool) {}
        fn parked_proc(&self, _outcome: ParkedProc) {}
    }

    /// One implementation per job, no fallback path (Ada's rule; the lead's NoSeed ruling of 2026-11-06; AC-T7.1.4,
    /// CLAUDE.md §3). Do: a seam create whose seeds' line is full (every clone refused at the seeds' bound), with a
    /// stand-in shim that leaves a mark if it runs. Expect: the create refused by type, naming the seeds' bound, as
    /// the caller's `Refusal::Exhausted`; the shim never run; no create counted through it (the one route noted is
    /// the rootless seed's, which isn't the shim); and nothing of the container left.
    #[test]
    fn a_create_past_the_seeds_bound_is_refused_and_never_takes_the_shim() {
        let (made, routes, shim_ran, left) = on_a_full_line(ClassicReason::Rootless);
        match made {
            Err(e @ OciError::SeedsExhausted { bound: 2, .. }) => assert_eq!(
                Refusal::from(e),
                Refusal::Exhausted {
                    what: "create seeds",
                    bound: 2
                }
            ),
            other => panic!("refused by the seeds' bound, not {other:?}"),
        }
        assert!(!shim_ran, "the shim ran");
        assert_eq!(
            routes,
            vec![ClassicReason::Rootless],
            "no create went through the shim"
        );
        assert!(!left, "the container's directory is left");
    }

    /// Goal 17's baseline arm (decision 6; the lead's ruling of 2026-11-09): a root node built for the classic root
    /// keeps its seeds' mount namespace, and each create takes the classic root in a seed, counted under
    /// [`ClassicReason::Baseline`], never under a rootless node's reason nor through the shim. Do: a seam create on
    /// seeds that keep the agent's namespace for the baseline, their line full so nothing is made. Expect: the one
    /// route noted is the baseline's, and the shim never ran.
    #[test]
    fn a_root_node_built_for_the_classic_root_counts_each_create_under_the_baseline() {
        let (_, routes, shim_ran, _) = on_a_full_line(ClassicReason::Baseline);
        assert_eq!(routes, vec![ClassicReason::Baseline]);
        assert!(!shim_ran, "the shim ran");
    }

    /// A seam create of a `true` container on seeds whose line is full and which keep the agent's mount namespace for
    /// `kept`, beside a stand-in shim that leaves a mark if it runs: what the create made, the classic routes the
    /// seeds noted, whether the shim ran, and whether the container's directory is left.
    fn on_a_full_line(
        kept: ClassicReason,
    ) -> (Result<State, OciError>, Vec<ClassicReason>, bool, bool) {
        let lab = tempfile::tempdir().unwrap();
        let ran = lab.path().join("shim-ran");
        let stand_in = lab.path().join("shim");
        stand_in_at(
            &stand_in,
            &format!("#!/bin/sh\ntouch '{}'\n", ran.display()),
        );
        let rt = Runtime::with_helper(lab.path().join("state"), stand_in);
        let bundle = lab.path().join("bundle");
        std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
        std::fs::write(
            bundle.join("config.json"),
            br#"{"ociVersion":"1.2.0","root":{"path":"rootfs"},"process":{"args":["/bin/true"],"cwd":"/"},"linux":{"namespaces":[{"type":"pid"},{"type":"mount"}]}}"#,
        )
        .unwrap();
        let cgroup = lab.path().join("cgroup");
        std::fs::create_dir(&cgroup).unwrap();
        let seeds = FullLine {
            classic: std::cell::RefCell::new(Vec::new()),
            kept,
        };
        let devices = Programs::rootless(1);
        let made = rt.create_chosen_seeded(
            "c1",
            &bundle,
            &OneCgroup(cgroup),
            Held {
                devices: Some(&devices),
                ..Held::default()
            },
            &seeds,
        );
        let left = rt.dir("c1").unwrap().path().exists();
        (made, seeds.classic.into_inner(), ran.exists(), left)
    }

    /// What a claim or a clone was handed: by which, whether the map pipe rode, and where the cgroup directory that rode
    /// is, if one did.
    type Handed = (&'static str, bool, Option<PathBuf>);

    /// A node's create seeds that note what each claim and each clone of a create was handed, and refuse both.
    #[derive(Default)]
    struct Noting(std::cell::RefCell<Vec<Handed>>);

    impl Noting {
        fn note(&self, by: &'static str, req: &CloneReq<'_>) {
            let cgroup = req
                .pruned
                .as_ref()
                .and_then(|p| p.cgroup_dir.as_ref())
                .map(|fd| std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap());
            self.0.borrow_mut().push((by, req.map_r.is_some(), cgroup));
        }
    }

    impl CloneInit for Noting {
        fn clone_init(&self, req: CloneReq<'_>) -> Result<(i32, OwnedFd), OciError> {
            self.note("clone", &req);
            Err(OciError::Broken("refused for the test".to_owned()))
        }
        fn claim(
            &self,
            _parked: Parked,
            req: &CloneReq<'_>,
            _sync: BorrowedFd<'_>,
        ) -> Result<(i32, OwnedFd), OciError> {
            self.note("claim", req);
            Err(OciError::Broken("refused for the test".to_owned()))
        }
        fn unpruned(&self) -> Option<ClassicReason> {
            None
        }
        fn mount_namespace(&self) -> super::super::mntns::MountNamespace {
            super::super::mntns::MountNamespace::Copy
        }
        fn classic(&self, _reason: ClassicReason) {}
    }

    /// The lead's (B) of 2026-11-09 (decision 8's claim; goal 16): a pruned create carries only what its init reads. Do:
    /// a pruned create handed to seeds that note what each claim and clone was handed and refuse both, with an init
    /// parked for it in its cgroup, then with none. Expect: no map pipe in any, since a pruned create's init asks for no
    /// maps and stops for no hooks (`pruned_create` routes both to the classic path); no cgroup directory in the claim,
    /// whose init is in its cgroup already; and the box's in each clone, the failed claim's fallback and the first try of
    /// a create with no init parked.
    #[test]
    fn a_pruned_create_carries_no_map_pipe_and_its_cgroup_to_a_seeds_clone_alone() {
        let lab = tempfile::tempdir().unwrap();
        let rt = Runtime::new(lab.path().join("state"));
        let dir = rt.dir("c1").unwrap();
        dir.create("c1").unwrap();
        let spec = spec_with(r#"{"type":"pid"},{"type":"mount"}"#);
        let checked = checked::seal(b"{}").unwrap();
        let making = Making {
            id: "c1",
            bundle: lab.path(),
            dir: &dir,
            pid_file: None,
            config: checked.as_fd(),
        };
        let box_dir = lab.path().join("cgroup");
        std::fs::create_dir(&box_dir).unwrap();
        let cgroup = BoxCgroup::at(box_dir.clone());
        let open = || open_dir(lab.path()).unwrap();
        let pruned = || PrunedCreate {
            cgroup_dir: None,
            container_dir: open(),
            rootfs: open(),
            mount_namespace: super::super::mntns::MountNamespace::Copy,
            dev_mount: None,
            ipc_uts: None,
            proc_mount: None,
            proc_sink: None,
            masked_directory: None,
            binds: Vec::new(),
            path_joins: Vec::new(),
        };
        // An init parked for this create, with the namespaces it makes: a process of the test's, which the claim's
        // refusal ends and reaps.
        let parked = || {
            #[expect(
                clippy::zombie_processes,
                reason = "reaped by its pidfd through the Parked made of it, which the claim drops"
            )]
            let sleeper = Command::new("sleep").arg("infinity").spawn().unwrap();
            let pid = i32::try_from(sleeper.id()).unwrap();
            let pidfd = rustix::process::pidfd_open(
                rustix::process::Pid::from_child(&sleeper),
                PidfdFlags::empty(),
            )
            .unwrap();
            let (ours, _theirs) = super::super::seed::socketpair().unwrap();
            let flags = super::super::mntns::clone_flags(
                init::fresh_clone_flags(&spec, false, false),
                super::super::mntns::MountNamespace::Copy,
            );
            Parked::new(pid, pidfd, ours, flags)
        };
        let the_box = std::fs::canonicalize(&box_dir).unwrap();
        for (parked, want) in [
            (
                Some(parked()),
                vec![
                    ("claim", false, None),
                    ("clone", false, Some(the_box.clone())),
                ],
            ),
            (None, vec![("clone", false, Some(the_box.clone()))]),
        ] {
            let seeds = Noting::default();
            let made = rt.start_via_seed(
                &making,
                &spec,
                &cgroup,
                None,
                Some(pruned()),
                Via {
                    seeds: &seeds,
                    parking: Parking {
                        parked,
                        chooser: None,
                    },
                },
            );
            assert!(made.is_err(), "the seeds refused it");
            assert_eq!(seeds.0.into_inner(), want);
        }
    }
}
