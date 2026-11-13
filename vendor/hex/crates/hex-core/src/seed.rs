//! The node agent's seeds as bytes (decisions 3 and 5 of docs/design/runtime-architecture.md, goal 3
//! plan step 2 and goal 5 plan step 4): what the agent asks a seed (`hex __oci-seed`) to run in a
//! container or to make one, and what the seed answers, as messages of the `SOCK_SEQPACKET` socketpair
//! between them. Pure: the agent's seed pools (`hex-node`) and the seed (`hex-runtime`) move the
//! messages and the descriptors that ride with them; this decides what each says, and refuses by type
//! every one that says nothing valid.
//!
//! A message is one `SOCK_SEQPACKET` record: an 8-byte header, then the body its kind defines,
//! encoded as the socket's messages are ([`crate::wire`]: little-endian integers, a string as a `u32`
//! length and its bytes, an option as a byte before the value, a list as a `u32` count before its
//! items).
//!
//! | bytes | field |
//! |---|---|
//! | 0..4 | `HEXS` |
//! | 4..6 | version |
//! | 6..8 | kind: a request below `0x8000`, an answer from it |
//!
//! An exec [`Request`] carries [`REQUEST_FDS`] descriptors: the program's stdin, stdout and stderr, then
//! the container init's pidfd, by which the program joins the init's namespaces (`setns(2)` with a pidfd
//! joins several at once, and names that process, never a later one given its pid). The container's
//! cgroup isn't a descriptor: the request names its path, which the seed opens. A [`Create`] carries
//! first the write end of the pipe on which the container's init tells the agent it is ready and, in
//! the older kinds, the read end of its map pipe ([`Fixed`]); then, when it says so, the network
//! namespace made ahead that the init joins. [`Answer::Started`] carries the pidfd of the
//! process the seed started, the program or the container's init, which the agent's supervisor
//! watches; [`Answer::Refused`] carries none. A node spawns its seeds from the `hex` binary it is
//! given (the bench names one), so the two sides can be of different builds: a reader refuses another
//! version's message, or a kind it doesn't know, by type, and the format changes append-only
//! (CLAUDE.md), as the socket's does: the create is a kind added to version 1, whose exec bytes are as
//! they were.

use crate::wire::{Fault, Reader, Writer};

/// Every message's first bytes.
pub const MAGIC: [u8; 4] = *b"HEXS";
/// The version this side writes and reads. Version 2 adds a create's pruned-root descriptors (goal 5
/// plan step 2): a create's body gained a `pruned` flag and a `binds` count, so its bytes differ from
/// version 1's, and a reader of either refuses the other's by type ([`read_header`]). A node spawns
/// its seeds from the one `hex` binary it is given, so the two sides are always of this build.
pub const VERSION: u16 = 2;
/// The header's length: magic, version and kind.
pub const HEADER_BYTES: usize = 8;
/// Largest request. It sits under the kernel's bound on one `SOCK_SEQPACKET` record (212,960 bytes
/// under the default send buffer; [`crate::wire::MAX_MESSAGE`] says where that was measured) and above
/// the largest request a socket client's exec makes, its program and environment under
/// [`crate::wire::MAX_MESSAGE`]. A larger one is refused by type before it is sent.
pub const MAX_REQUEST: usize = 160 << 10;
/// Largest answer. A refusal's reason past [`MAX_WHY`] bytes is cut, so every answer fits.
pub const MAX_ANSWER: usize = 4 << 10;
/// Longest refusal reason an answer carries; the rest is cut, at a character's boundary.
pub const MAX_WHY: usize = 2 << 10;
/// Descriptors an exec request carries: stdin, stdout, stderr, the container init's pidfd.
pub const REQUEST_FDS: usize = 4;
/// Descriptors a create of the older kinds carries first ([`Fixed::All`]): the write end of the init's sync pipe and
/// the read end of its map pipe. A lean create carries [`LEAN_FDS`].
pub const CREATE_FDS: usize = 2;
/// Descriptors a lean create carries first ([`Fixed::Lean`]): the write end of the init's sync pipe alone.
pub const LEAN_FDS: usize = 1;
/// Descriptors a pruned create of the older kinds carries after the network namespace, before the bind-source trees
/// (goal 5 plan step 2): the bundle directory (to read `config.json`), the container's cgroup
/// directory, the container's state directory (the exec FIFO's), and the rootfs as a detached tree.
/// A pruned seed reaches no host path, so each comes as a descriptor. A lean create carries
/// [`LEAN_PRUNED_FIXED_FDS`] there.
pub const PRUNED_FIXED_FDS: usize = 4;
/// Descriptors a lean create carries there ([`Fixed::Lean`]): the container's state directory and the rootfs, after the
/// cgroup directory where it rides.
pub const LEAN_PRUNED_FIXED_FDS: usize = 2;
/// Most namespaces a pruned create joins by path, each a descriptor the agent opened (its host path
/// out of the pruned seed's reach): the network, ipc, uts and cgroup kinds — a pid or user namespace by
/// path is made through the shim instead, a mount namespace by path too (goal 5 plan step 2).
pub const MAX_PATH_JOINS: usize = 4;
/// Most bind-source trees a pruned create carries, so its descriptors (the two pipes, a network
/// namespace, the four fixed, the bind trees and the path-joined namespaces) stay under `SCM_MAX_FD`,
/// the kernel's bound on one `SCM_RIGHTS` message (253, `include/net/scm.h`). A spec with more bind
/// mounts is refused by type and made through the shim (CLAUDE.md §2).
pub const MAX_BINDS: usize = 253 - CREATE_FDS - 1 - PRUNED_FIXED_FDS - MAX_PATH_JOINS;
/// Most descriptors any request carries: what a seed's read makes room for, so one carrying more is
/// cut and refused. A pruned create is the largest (the two pipes, a network namespace, the four fixed,
/// up to [`MAX_BINDS`] bind trees and up to [`MAX_PATH_JOINS`] path-joined namespaces), which this holds
/// under `SCM_MAX_FD` (253).
pub const MAX_REQUEST_FDS: usize = CREATE_FDS + 1 + PRUNED_FIXED_FDS + MAX_BINDS + MAX_PATH_JOINS;
/// Descriptors a started answer carries: the pidfd of the process the seed started.
pub const STARTED_FDS: usize = 1;
/// Descriptors a create carries last when it carries the spec its runtime checked ([`Create::config`]): a sealed memfd
/// of the exact `config.json` bytes the runtime parsed and checked, which the seed makes the container from in place
/// of reading the bundle again.
pub const CONFIG_FDS: usize = 1;
/// Descriptors of an ipc and a uts namespace made ahead, which ride last in a create that says so
/// ([`Create::ipc_uts`]).
pub const IPC_UTS_FDS: usize = 2;

/// A park's descriptors: the idle cgroup the init is cloned into, then the init's end of the socket its
/// claim comes on.
pub const PARK_FDS: usize = 2;

/// The kinds of version 1.
mod kind {
    pub const EXEC: u16 = 0x0001;
    pub const CREATE: u16 = 0x0002;
    pub const CREATE_PREPARED: u16 = 0x0003;
    pub const CREATE_AHEAD: u16 = 0x0004;
    pub const CREATE_CONFIG: u16 = 0x0005;
    pub const EXEC_CHECKED: u16 = 0x0006;
    /// A create that names how its pruned mount namespace is made or carries a prepared proc or the masked
    /// directories' source: CREATE_CONFIG's body, then its proc's index as an option, then the way, then the
    /// masked directories' flag (decision 6's update). It carries the checked spec, as CREATE_CONFIG does. A
    /// create with none of these keeps CREATE_CONFIG's kind and bytes.
    pub const CREATE_MOUNT_NAMESPACE: u16 = 0x0007;
    /// A request that parks an init ahead in a slot's cgroup, its flags and its two descriptors (decision 8). Appended
    /// after goal 6's CREATE_MOUNT_NAMESPACE, which took 0x0007 first on this tree.
    pub const PARK: u16 = 0x0008;
    /// A pruned create carrying only the descriptors its init reads ([`super::Fixed::Lean`]; the lead's (B) of
    /// 2026-11-09): CREATE_MOUNT_NAMESPACE's body whatever the way, then whether the cgroup directory rides. A create of
    /// the older layout keeps its kind and its bytes.
    pub const CREATE_LEAN: u16 = 0x0009;
    pub const STARTED: u16 = 0x8001;
    pub const REFUSED: u16 = 0x8002;
    /// A started answer, then how the init's mount namespace was made.
    pub const STARTED_IN: u16 = 0x8003;
}

/// What to run in a container and where: the init whose namespaces the program joins, the cgroup it is
/// born in, and the program with the exec's overrides of the container's process (its arguments, and
/// optionally its working directory, environment entries and user). The intermediate reads the rest
/// (the capabilities, rlimits, seccomp filter and the rest of the environment) from the bundle's
/// `config.json`, as `hex __oci-exec` does, so the wire carries only what the exec names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// The container init's pid, for what the seed reports; its namespaces are joined by its pidfd.
    pub init_pid: i32,
    /// The container's bundle, whose `config.json` gives the process the program becomes.
    pub bundle: String,
    /// The container's cgroup directory, which the intermediate opens; the program is born in it.
    /// `None` for a container with no cgroup of its own.
    pub cgroup: Option<String>,
    /// The program and its arguments; the first is the program.
    pub args: Vec<String>,
    /// Overrides the process's working directory.
    pub cwd: Option<String>,
    /// `K=V` entries, each overriding the process's entry for `K` or adding one.
    pub env: Vec<String>,
    /// Overrides the process's user: a uid, and a gid (the process's own when absent).
    pub user: Option<(u32, Option<u32>)>,
    /// New EXEC_CHECKED kind (AC-T7.1.1, found 2026-11-04): the container's state directory, where its create kept
    /// the spec it checked (`checked.json`), which the program's process settings are taken from, never the
    /// bundle's `config.json` as it is now. Absent in the older EXEC kind, whose bytes are unchanged; the seed then
    /// checks the bundle's spec again, with the same checks.
    pub dir: Option<String>,
}

/// A container for the seed to make (decision 5): its init cloned into its cgroup and the namespaces its
/// spec asks for, set up to the exec FIFO. On the classic path the seed opens the cgroup the create names, as
/// `hex __oci-exec` does, and takes the spec from the create's last descriptor ([`Create::config`]); the descriptors
/// that ride with it ([`CREATE_FDS`]) are the init's sync pipe (the write end) and the map pipe (the read
/// end), and, when `network` says so, the network namespace made ahead the init joins.
///
/// On the pruned path (goal 5 plan step 2, `pruned`), the seed's mount namespace holds nothing of the
/// host, so what it read by path comes as descriptors instead: after the network namespace ride
/// [`PRUNED_FIXED_FDS`] (the bundle directory, the cgroup directory, the state directory and the rootfs
/// tree), then `binds` bind-source trees in the order the spec lists its bind mounts. The agent cloned
/// each with `open_tree` in its own mount namespace and sends them here. Last ride an ipc and a uts namespace made
/// ahead, when `ipc_uts` says so (goal 5 extended, step 4). A lean create ([`Fixed::Lean`]) carries only what its
/// init reads: no map pipe and no bundle directory, and the cgroup directory only for a seed's clone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Create {
    /// The container's bundle, whose `config.json` gives its namespaces and the init's set-up.
    pub bundle: String,
    /// The container's state directory, which holds the exec FIFO its init waits on.
    pub dir: String,
    /// The container's cgroup directory, which the seed opens and the init is born in; `None` for a
    /// container with no cgroup of its own (the seam always gives one). On the pruned path the cgroup
    /// rides as a descriptor instead, and this names it only for the seed's messages.
    pub cgroup: Option<String>,
    /// Whether a network namespace made ahead rides with it, after the fixed pair, for the init to join
    /// in place of making its own (decision 4).
    pub network: bool,
    /// Whether this create takes the pruned-seed root path (goal 5 plan step 2): its sources ride as
    /// descriptors after the network namespace, since a pruned seed reaches no host path. A create to
    /// a seed whose node isn't a root node, or whose spec needs the classic root, is made through the
    /// shim and never reaches a seed as pruned.
    pub pruned: bool,
    /// How many bind-source trees ride, when `pruned`: one per bind mount the spec lists, in that
    /// order, after the fixed descriptors. Zero when not pruned. At most [`MAX_BINDS`].
    pub binds: u32,
    /// How many namespaces the container joins by path ride last, when `pruned`: one descriptor per
    /// network, ipc, uts or cgroup namespace the spec joins by path, which the agent opened (the pruned
    /// seed can't reach the host path). Zero when not pruned. At most [`MAX_PATH_JOINS`].
    pub path_joins: u32,
    /// New CREATE_PREPARED kind: mount-plan index of a prepared `/dev`; its fd rides last. Old
    /// CREATE bytes stay unchanged and readers without this kind refuse it explicitly.
    pub dev_mount: Option<u32>,
    /// New CREATE_AHEAD kind (goal 5 extended, step 4): an ipc and a uts namespace made ahead ride last, which the
    /// init joins in place of its clone making them. Pruned creates only: a seed that isn't pruned belongs to a
    /// rootless node, whose containers' namespaces belong to user namespaces of their own. Older kinds' bytes
    /// stay unchanged, and readers without this kind refuse it explicitly.
    pub ipc_uts: bool,
    /// New CREATE_CONFIG kind (AC-T7.1.1, found 2026-11-04): the spec the runtime parsed and checked rides last, a
    /// sealed memfd of its `config.json` bytes, and the seed makes the container from it, never from a later read
    /// of the bundle, which could have been swapped for a spec nobody checked. Its body is every field above, the
    /// prepared `/dev`'s index an option and the pair a flag. Older kinds' bytes stay unchanged, readers without
    /// this kind refuse it explicitly, and a seed of this build refuses a create that doesn't carry it.
    pub config: bool,
    /// How a pruned create's mount namespace is made (decision 6's update): the seed's namespace copied,
    /// empty (`CLONE_EMPTY_MNTNS`), or the agent's namespace made from the rootfs's tree
    /// (`OPEN_TREE_NAMESPACE`), which then rides in the rootfs tree's place. [`MountNamespace::Copy`]
    /// for every create that isn't pruned.
    pub mount_namespace: MountNamespace,
    /// The mount-plan index of a proc made ahead (decision 6's update, part 2): its pid namespace and its
    /// tree ride after the prepared `/dev`'s, and the init is cloned into that pid namespace.
    pub proc_mount: Option<u32>,
    /// Whether the node's one empty read-only tmpfs rides after the proc, which the init clones for each masked
    /// directory in place of a tmpfs of its own (decision 6's update): no superblock per mask.
    pub masked_directory: bool,
    /// Which of the fixed descriptors ride ([`Fixed`]): all of them in every older kind; in the new CREATE_LEAN kind, a
    /// pruned create's alone, only those its init reads.
    pub fixed: Fixed,
}

/// Which of a create's fixed descriptors ride (the lead's (B) of 2026-11-09, decision 8's claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fixed {
    /// The older kinds' (1 to 7): the map pipe's read end after the sync pipe's write end, then on the pruned path the
    /// bundle and cgroup directories before the state directory and the rootfs ([`CREATE_FDS`], [`PRUNED_FIXED_FDS`]).
    #[default]
    All,
    /// A pruned create's in kind 9: only what its init reads. No map pipe, which carries a user namespace's maps and the
    /// create-time hooks' state, neither of which a pruned create has; no bundle directory, since the spec rides
    /// checked ([`Create::config`]); and the cgroup directory only where `cgroup`, for a seed's clone, which is born in
    /// it, never for a parked init's claim, whose init is in its cgroup already.
    Lean { cgroup: bool },
}

/// Descriptors a prepared proc adds to a create: its pid namespace and its tree.
pub const PROC_FDS: usize = 2;

/// How a pruned create's mount namespace is made (decision 6's update, `goal6-mntns.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MountNamespace {
    /// The seed's pruned namespace copied by the init's clone (`CLONE_NEWNS`): goal 5's way, on every
    /// kernel.
    #[default]
    Copy,
    /// A namespace holding only its root, made by the init's clone (`CLONE_EMPTY_MNTNS`, Linux 7.1).
    Empty,
    /// A namespace the agent made from the rootfs's tree (`OPEN_TREE_NAMESPACE`, Linux 7.0; a layered
    /// root's by `FSMOUNT_NAMESPACE`, 7.1), which the init joins.
    FromTree,
}

impl MountNamespace {
    /// This way's byte on the wire.
    pub fn code(self) -> u8 {
        match self {
            Self::Copy => 0,
            Self::Empty => 1,
            Self::FromTree => 2,
        }
    }

    /// The way a byte on the wire names, or `None` for one no way has.
    pub fn of(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Copy),
            1 => Some(Self::Empty),
            2 => Some(Self::FromTree),
            _ => None,
        }
    }
}

impl Create {
    /// How many descriptors this create carries: the pipes ([`Fixed`]: both in the older kinds, the sync pipe's
    /// alone in a lean create), then a network namespace made ahead when there is one, then on the pruned path the
    /// fixed descriptors (the four of the older kinds, or a lean create's two and the cgroup directory where it
    /// rides), one tree per bind mount, one descriptor per namespace joined by path, a prepared `/dev` and an ipc and
    /// uts pair.
    pub fn fds(&self) -> usize {
        let (first, fixed) = match self.fixed {
            Fixed::All => (CREATE_FDS, PRUNED_FIXED_FDS),
            Fixed::Lean { cgroup } => (
                LEAN_FDS,
                LEAN_PRUNED_FIXED_FDS.saturating_add(usize::from(cgroup)),
            ),
        };
        let mut n = first.saturating_add(usize::from(self.network));
        if self.pruned {
            n = n
                .saturating_add(fixed)
                .saturating_add(self.binds as usize)
                .saturating_add(self.path_joins as usize)
                .saturating_add(usize::from(self.dev_mount.is_some()))
                .saturating_add(if self.ipc_uts { IPC_UTS_FDS } else { 0 })
                .saturating_add(if self.proc_mount.is_some() {
                    PROC_FDS
                } else {
                    0
                })
                .saturating_add(usize::from(self.masked_directory));
        }
        if self.config {
            n = n.saturating_add(CONFIG_FDS);
        }
        n
    }
}

/// An init to make ahead into an idle cgroup (decision 8, "The p99.9 that stayed"): the seed clones it into the
/// cgroup with `flags`, the namespaces the owner's last box made fresh, as the agent's own child, and the init
/// waits on its socket for the [`Create`] of the box that claims it, which it then makes as a freshly cloned
/// init does, or refuses by type when the create asks for other namespaces. The kind is added (append-only):
/// readers without it refuse it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Park {
    /// The `clone3` flags of the namespaces the init is made in: `CLONE_NEW*` bits only.
    pub flags: u64,
}

/// A request a seed reads: an exec into a container, a container to make, or an init to make ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Exec(Request),
    Create(Create),
    Park(Park),
}

/// A park as a message.
pub fn park(park: &Park) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_REQUEST);
    header(&mut w, kind::PARK);
    w.u64(park.flags);
    w.done()
}

/// What failed when a seed refuses, so the agent reports a typed refusal and knows whether to replace
/// the seed. The detail is in the answer's reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failed {
    /// The request wasn't one the seed could read: its bytes or its descriptors.
    Request,
    /// Joining the container (its namespaces or its cgroup), or cloning the program into it.
    Start,
    /// The seed's own pipes, sockets or fork: the seed is broken and is replaced.
    Seed,
    /// The seed can't make this container in one clone here: this kernel or sandbox has no `clone3`
    /// into a cgroup. Nothing was made, and the seed is whole; the agent makes it as the command line
    /// does.
    Unable,
    /// The program is past the exec bound the seed's own stack limit gives it (HEX-RT-14): nothing was
    /// started, the seed is whole, and the agent refuses the program by name. Appended to version 1.
    Program,
}

impl Failed {
    fn code(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Start => 2,
            Self::Seed => 3,
            Self::Unable => 4,
            Self::Program => 5,
        }
    }

    fn of(code: u16) -> Result<Self, Fault> {
        Ok(match code {
            1 => Self::Request,
            2 => Self::Start,
            3 => Self::Seed,
            4 => Self::Unable,
            5 => Self::Program,
            n => {
                return Err(Fault::value(
                    "refusal",
                    format!("code {n} isn't one version 1 defines"),
                ));
            }
        })
    }
}

/// A seed's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The program runs: its pid (the descriptor is its pidfd).
    Started { pid: u32 },
    /// Nothing was started.
    Refused { failed: Failed, why: String },
    /// A pruned create's init runs: its pid (the descriptor is its pidfd), and how its mount namespace
    /// was made, which can differ from the way asked where the kernel hasn't it (decision 6's update).
    StartedIn {
        pid: u32,
        mount_namespace: MountNamespace,
    },
}

impl Answer {
    /// How many descriptors this answer carries.
    pub fn fds(&self) -> usize {
        match self {
            Self::Started { .. } | Self::StartedIn { .. } => STARTED_FDS,
            Self::Refused { .. } => 0,
        }
    }
}

/// An exec request as a message.
pub fn request(req: &Request) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_REQUEST);
    header(
        &mut w,
        if req.dir.is_some() {
            kind::EXEC_CHECKED
        } else {
            kind::EXEC
        },
    );
    w.i32(req.init_pid);
    w.string(&req.bundle)?;
    w.option_with(req.cgroup.as_deref(), |w, c| w.string(c))?;
    w.list(&req.args, |w, a| w.string(a))?;
    w.option_with(req.cwd.as_deref(), |w, c| w.string(c))?;
    w.list(&req.env, |w, e| w.string(e))?;
    w.option(req.user, |w, (uid, gid)| {
        w.u32(uid);
        w.option(gid, Writer::u32);
    });
    if let Some(dir) = &req.dir {
        w.string(dir)?;
    }
    w.done()
}

/// A message's exec request. `size` is its whole size as the socket gave it, larger than `bytes`
/// when the message was cut to the reader's buffer.
pub fn read_request(bytes: &[u8], size: usize) -> Result<Request, Fault> {
    if size > MAX_REQUEST {
        return Err(Fault::Oversized {
            bytes: size,
            bound: MAX_REQUEST,
        });
    }
    let mut r = Reader::new(bytes);
    let (version, kind) = read_header(&mut r)?;
    if kind != kind::EXEC && kind != kind::EXEC_CHECKED {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "an exec request",
        });
    }
    let req = Request {
        init_pid: r.i32()?,
        bundle: r.string("bundle")?,
        cgroup: r.option(|r| r.string("cgroup"))?,
        args: r.list(|r| r.string("args"))?,
        cwd: r.option(|r| r.string("cwd"))?,
        env: r.list(|r| r.string("env"))?,
        user: r.option(|r| Ok((r.u32()?, r.option(Reader::u32)?)))?,
        dir: if kind == kind::EXEC_CHECKED {
            Some(r.string("dir")?)
        } else {
            None
        },
    };
    r.end(kind, version)?;
    if req.args.is_empty() {
        return Err(Fault::value("args", "an exec needs a program"));
    }
    Ok(req)
}

/// A create as a message.
pub fn create(create: &Create) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_REQUEST);
    let named = create.mount_namespace != MountNamespace::Copy
        || create.proc_mount.is_some()
        || create.masked_directory;
    let lean = match create.fixed {
        Fixed::All => None,
        Fixed::Lean { cgroup } => Some(cgroup),
    };
    // Those kinds are CREATE_CONFIG's with more, so they carry the checked spec, as every create of this build does.
    if (named || lean.is_some()) && !create.config {
        return Err(Fault::value(
            "config",
            "a create that names its mount namespace, or carries only what its init reads, \
             carries the spec its runtime checked",
        ));
    }
    header(
        &mut w,
        match (lean, named, create.config, create.ipc_uts, create.dev_mount) {
            (Some(_), _, _, _, _) => kind::CREATE_LEAN,
            (None, true, _, _, _) => kind::CREATE_MOUNT_NAMESPACE,
            (None, false, true, _, _) => kind::CREATE_CONFIG,
            (None, false, false, true, _) => kind::CREATE_AHEAD,
            (None, false, false, false, Some(_)) => kind::CREATE_PREPARED,
            (None, false, false, false, None) => kind::CREATE,
        },
    );
    w.string(&create.bundle)?;
    w.string(&create.dir)?;
    w.option_with(create.cgroup.as_deref(), |w, c| w.string(c))?;
    w.bool(create.network);
    w.bool(create.pruned);
    w.u32(create.binds);
    w.u32(create.path_joins);
    // CREATE_PREPARED's index is bare, as that kind's bytes have it; CREATE_AHEAD's is an option, since a pair rides
    // with or without a prepared `/dev`; CREATE_CONFIG's is an option, then the pair's flag; CREATE_MOUNT_NAMESPACE's
    // is CREATE_CONFIG's, then the proc's index, the way and the masked directories' flag; CREATE_LEAN's is
    // CREATE_MOUNT_NAMESPACE's whatever the way, then the cgroup directory's flag.
    if create.config {
        w.option_with(create.dev_mount, |w, mount| {
            w.u32(mount);
            Ok(())
        })?;
        w.bool(create.ipc_uts);
        if named || lean.is_some() {
            w.option(create.proc_mount, Writer::u32);
            w.u8(create.mount_namespace.code());
            w.bool(create.masked_directory);
        }
        if let Some(cgroup) = lean {
            w.bool(cgroup);
        }
    } else if create.ipc_uts {
        w.option_with(create.dev_mount, |w, mount| {
            w.u32(mount);
            Ok(())
        })?;
    } else if let Some(mount) = create.dev_mount {
        w.u32(mount);
    }
    w.done()
}

/// A message's create. `size` is its whole size as the socket gave it, as [`read_request`] takes it.
pub fn read_create(bytes: &[u8], size: usize) -> Result<Create, Fault> {
    if size > MAX_REQUEST {
        return Err(Fault::Oversized {
            bytes: size,
            bound: MAX_REQUEST,
        });
    }
    let mut r = Reader::new(bytes);
    let (version, kind) = read_header(&mut r)?;
    if !matches!(
        kind,
        kind::CREATE
            | kind::CREATE_PREPARED
            | kind::CREATE_AHEAD
            | kind::CREATE_CONFIG
            | kind::CREATE_MOUNT_NAMESPACE
            | kind::CREATE_LEAN
    ) {
        return Err(Fault::Kind {
            kind,
            version,
            expected: "a create",
        });
    }
    let mut create = Create {
        bundle: r.string("bundle")?,
        dir: r.string("dir")?,
        cgroup: r.option(|r| r.string("cgroup"))?,
        network: r.bool("network")?,
        pruned: r.bool("pruned")?,
        binds: r.u32()?,
        path_joins: r.u32()?,
        dev_mount: match kind {
            kind::CREATE_PREPARED => Some(r.u32()?),
            kind::CREATE_AHEAD
            | kind::CREATE_CONFIG
            | kind::CREATE_MOUNT_NAMESPACE
            | kind::CREATE_LEAN => r.option(|r| r.u32())?,
            _ => None,
        },
        ipc_uts: match kind {
            kind::CREATE_AHEAD => true,
            kind::CREATE_CONFIG | kind::CREATE_MOUNT_NAMESPACE | kind::CREATE_LEAN => {
                r.bool("ipc_uts")?
            }
            _ => false,
        },
        config: matches!(
            kind,
            kind::CREATE_CONFIG | kind::CREATE_MOUNT_NAMESPACE | kind::CREATE_LEAN
        ),
        mount_namespace: MountNamespace::Copy,
        proc_mount: None,
        masked_directory: false,
        fixed: Fixed::All,
    };
    if matches!(kind, kind::CREATE_MOUNT_NAMESPACE | kind::CREATE_LEAN) {
        create.proc_mount = r.option(Reader::u32)?;
        create.mount_namespace = MountNamespace::of(r.u8()?).ok_or_else(|| {
            Fault::value("mount_namespace", "no mount namespace is made that way")
        })?;
        create.masked_directory = r.bool("masked_directory")?;
    }
    // A create with none of these has its old kinds: one encoding a value, so it is refused here.
    if kind == kind::CREATE_MOUNT_NAMESPACE
        && create.mount_namespace == MountNamespace::Copy
        && create.proc_mount.is_none()
        && !create.masked_directory
    {
        return Err(Fault::value(
            "mount_namespace",
            "a create in this kind is made empty or from its tree, or carries a proc or a \
             masked directory's source",
        ));
    }
    if kind == kind::CREATE_LEAN {
        create.fixed = Fixed::Lean {
            cgroup: r.bool("cgroup_dir")?,
        };
    }
    r.end(kind, version)?;
    if create.bundle.is_empty() || create.dir.is_empty() {
        return Err(Fault::value(
            "create",
            "a create names its bundle and its state directory",
        ));
    }
    // A create's trees and path-joined namespaces are bounded, so its descriptors stay under
    // `SCM_MAX_FD`; a count past its bound, or set when the create isn't pruned, is refused (§2, §3).
    if create.binds as usize > MAX_BINDS {
        return Err(Fault::value(
            "binds",
            format!("a create carries at most {MAX_BINDS} bind-source trees"),
        ));
    }
    if create.path_joins as usize > MAX_PATH_JOINS {
        return Err(Fault::value(
            "path_joins",
            format!("a create joins at most {MAX_PATH_JOINS} namespaces by path"),
        ));
    }
    if !create.pruned && (create.binds != 0 || create.path_joins != 0 || create.dev_mount.is_some())
    {
        return Err(Fault::value(
            "binds",
            "only a pruned create carries bind-source trees or path-joined namespaces",
        ));
    }
    if !create.pruned && create.ipc_uts {
        return Err(Fault::value(
            "ipc_uts",
            "only a pruned create carries ipc and uts namespaces made ahead",
        ));
    }
    if !create.pruned
        && (create.mount_namespace != MountNamespace::Copy
            || create.proc_mount.is_some()
            || create.masked_directory)
    {
        return Err(Fault::value(
            "mount_namespace",
            "only a pruned create's mount namespace is made empty or from its tree, or carries a proc",
        ));
    }
    if !create.pruned && create.fixed != Fixed::All {
        return Err(Fault::value(
            "fixed",
            "only a pruned create carries only what its init reads: a classic create's init \
             may ask for its maps, or stop for its hooks, on the map pipe",
        ));
    }
    if create.fds() > MAX_REQUEST_FDS {
        return Err(Fault::value(
            "dev_mount",
            "a prepared create exceeds SCM_MAX_FD",
        ));
    }
    Ok(create)
}

/// A request a seed reads, of either kind, which came with `fds` descriptors: as many as its kind and
/// body say it carries, or it's refused.
pub fn read_message(bytes: &[u8], size: usize, fds: usize) -> Result<Message, Fault> {
    let mut r = Reader::new(bytes);
    let (version, kind) = read_header(&mut r)?;
    let (message, want) = match kind {
        kind::EXEC | kind::EXEC_CHECKED => (Message::Exec(read_request(bytes, size)?), REQUEST_FDS),
        kind::CREATE
        | kind::CREATE_PREPARED
        | kind::CREATE_AHEAD
        | kind::CREATE_CONFIG
        | kind::CREATE_MOUNT_NAMESPACE
        | kind::CREATE_LEAN => {
            let create = read_create(bytes, size)?;
            let want = create.fds();
            (Message::Create(create), want)
        }
        kind::PARK => {
            if size > MAX_REQUEST {
                return Err(Fault::Oversized {
                    bytes: size,
                    bound: MAX_REQUEST,
                });
            }
            let flags = r.u64()?;
            r.end(kind, version)?;
            (Message::Park(Park { flags }), PARK_FDS)
        }
        _ => {
            return Err(Fault::Kind {
                kind,
                version,
                expected: "an exec, a create or a park",
            });
        }
    };
    if fds != want {
        return Err(Fault::Descriptors {
            what: "this request",
            want,
            got: fds,
        });
    }
    Ok(message)
}

/// An answer as a message. A refusal's reason past [`MAX_WHY`] bytes is cut, so it always fits.
pub fn answer(answer: &Answer) -> Result<Vec<u8>, Fault> {
    let mut w = Writer::new(MAX_ANSWER);
    match answer {
        Answer::Started { pid } => {
            header(&mut w, kind::STARTED);
            w.u32(*pid);
        }
        Answer::StartedIn {
            pid,
            mount_namespace,
        } => {
            header(&mut w, kind::STARTED_IN);
            w.u32(*pid);
            w.u8(mount_namespace.code());
        }
        Answer::Refused { failed, why } => {
            header(&mut w, kind::REFUSED);
            w.u16(failed.code());
            w.string(cut(why, MAX_WHY))?;
        }
    }
    w.done()
}

/// A message's answer, which came with `fds` descriptors: as many as it says it carries, or it's
/// refused.
pub fn read_answer(bytes: &[u8], size: usize, fds: usize) -> Result<Answer, Fault> {
    if size > MAX_ANSWER {
        return Err(Fault::Oversized {
            bytes: size,
            bound: MAX_ANSWER,
        });
    }
    let mut r = Reader::new(bytes);
    let (version, kind) = read_header(&mut r)?;
    let answer = match kind {
        kind::STARTED => Answer::Started { pid: r.u32()? },
        kind::STARTED_IN => Answer::StartedIn {
            pid: r.u32()?,
            mount_namespace: MountNamespace::of(r.u8()?).ok_or_else(|| {
                Fault::value("mount_namespace", "no mount namespace is made that way")
            })?,
        },
        kind::REFUSED => Answer::Refused {
            failed: Failed::of(r.u16()?)?,
            why: r.string("why")?,
        },
        _ => {
            return Err(Fault::Kind {
                kind,
                version,
                expected: "an answer",
            });
        }
    };
    r.end(kind, version)?;
    if answer.fds() != fds {
        return Err(Fault::Descriptors {
            what: "this answer",
            want: answer.fds(),
            got: fds,
        });
    }
    Ok(answer)
}

fn header(w: &mut Writer, kind: u16) {
    w.raw(&MAGIC);
    w.u16(VERSION);
    w.u16(kind);
}

/// A message's version and kind: magic first, so a message of something else is refused as that.
fn read_header(r: &mut Reader<'_>) -> Result<(u16, u16), Fault> {
    if r.array::<4>()? != MAGIC {
        return Err(Fault::value("message", "it doesn't start with HEXS"));
    }
    let version = r.u16()?;
    if version != VERSION {
        return Err(Fault::Version {
            got: version,
            speaks: VERSION,
        });
    }
    Ok((version, r.u16()?))
}

/// `s` cut to at most `max` bytes, at a character's boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests;
