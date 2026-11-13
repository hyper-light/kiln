//! The node agent's network namespaces made ahead (decision 4 of docs/design/runtime-architecture.md,
//! goal 4 plan step 4): the shell around [`hex_core::netns::Pool`]. One thread of the agent makes each
//! namespace off the create path whenever the pool asks for more ([`net::make_ahead`]: unshared on that
//! thread, its loopback brought up inside it, the thread back home after). A create asks the pool once
//! the runtime has read its spec ([`Networks`]): a container that makes a network namespace of its own
//! in the agent's user namespace takes one, which its shim joins, and the pool counts every container's
//! namespace, taken or made on the path. A namespace taken is the container's alone, so it ends with the
//! container, and the kernel's deferred cleanup tears it down after delete returns.
//!
//! But for a box that may share its owner's (goal 5 extended, step 6; Ada's call of 2026-11-03): its
//! namespace, taken from the pool or made by the node on the path, is kept by the node while the box lives
//! ([`Netns::hold`]), and when the box and its record are gone it goes to the node's checker threads
//! ([`netcheck::check`]), one of which keeps it idle for the owner's next box ([`hex_core::netns_reuse::Idle`]) if
//! nothing of the box is left in it, and closes it, counted by why, if something is. Nothing runs in an idle
//! namespace, so what the check saw holds until it's handed on. That takes the making, the loopback's bringing
//! up and the teardown, each under the host's one RTNL and one `sysctl_lock`, off the owner's next box's
//! lifecycle (docs/research/runtime/goal5-netns-reuse.md).

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use hex_api::{BoxId, BoxOwner, Refusal};
use hex_core::netns::{Counts, Missing, Pool, Reason, Step, Take};
use hex_core::netns_reuse::{self, Idle, Refused as Why, Take as Kept, Whose};
use hex_runtime::linux::oci::OciError;
use hex_runtime::linux::oci::net;
use hex_runtime::linux::oci::netcheck::{self, Fresh, ProcNet, Scratch};
use hex_runtime::linux::oci::run::Networks;

use crate::sizes::Sizes;

/// The pool and the thread that fills it, and the namespaces kept for their owners' next boxes (goal 5
/// extended, step 6) with the thread that checks each as its box goes. Dropped, it shuts the pool down,
/// which closes what it held, closes every kept namespace, and waits for its threads.
pub struct Netns {
    shared: Arc<Shared>,
    refill: Option<JoinHandle<()>>,
    kept: Option<Keeping>,
}

/// How long the refill thread's make of a network namespace takes: no wait in the node is bounded by it, but the
/// waits census records it here (`waits_census.md`'s `netns.make` row), which seeds the netns tests' patience.
static MAKES: hex_core::patience::Patience = hex_core::patience::Patience::measuring("netns.make");

/// How many threads check the namespaces given back. With one, [`netcheck::check`] took 127 to 131 us a
/// namespace at concurrency 32 on fedora, so it checked at most about 7,700 a second while that concurrency gave
/// back 8,364, and 12% of its boxes found none of their owner's idle and were given a fresh one
/// (bench/competitive/probes/reuse_pass.md, run 7). Four check about 30,000 a second, 3.6 times that, so a burst
/// of 32 given back at once clears in about a millisecond; a namespace given back with the line full is closed
/// unchecked, counted.
const CHECKERS: usize = 4;

/// The namespaces kept for owners: none on a node without a pool (a rootless node's containers make
/// theirs in user namespaces of their own), so its creates take as before.
struct Keeping {
    state: Arc<KeptState>,
    returns: Option<SyncSender<(String, KeptNet)>>,
    checkers: Vec<JoinHandle<()>>,
}

/// A namespace kept for an owner, the netlink socket the checks keep in it from its first check on, so they
/// never close one of their own there, and its `/proc/net` files they hold from its first clean check on
/// ([`netcheck::check`]), so they needn't enter it, with the place under the node's [`FilesBound`] they take:
/// closed together.
pub struct KeptNet {
    ns: OwnedFd,
    socket: Option<OwnedFd>,
    files: Option<netcheck::Files>,
    slot: Option<FilesSlot>,
}

/// A kept namespace's own descriptors: the namespace and the check's socket.
const FDS_A_KEPT: u64 = 2;
/// The `/proc/net` files a kept namespace may hold besides ([`netcheck::Files`]): `sockstat`, `dev` and the two
/// iptables files.
const FILES_A_KEPT: u64 = 4;
/// A live box's besides its kept namespace's: the supervisor's pidfd of its init and its cgroup's directory.
const FDS_A_BOX: u64 = 2;
/// A seed's: its socket and its pidfd.
const FDS_A_SEED: u64 = 2;
/// A root node's create seeds' one pruned mount namespace, held for the node's life (`goal6-mntns.md` §12).
const FDS_PRUNED: u64 = 1;
/// A pool's descriptors past what it holds: its refill thread's own namespace, which it goes back to, and the one
/// it is making.
const FDS_A_REFILL: u64 = 2;
/// An init parked in an idle cgroup or one made ahead (decision 8): its pidfd and this end of the socket its claim
/// goes on (`hex_runtime::linux::oci::run::Parked`).
const FDS_A_PARKED: u64 = 2;
/// A parked init's proc, a root node's (decision 8, parked procs): its pid namespace and its tree
/// (`hex_runtime::linux::oci::init::PreparedProc`).
const FDS_A_PARKED_PROC: u64 = 2;
/// A cgroup made ahead's besides its parked init and the files it opens ahead: its directory and its `memory.peak`
/// (`crate::cgroups`'s `MadeAhead`).
const FDS_A_MADE_AHEAD: u64 = 2;
/// The node's socket's own (`crate::serve::linux::Linux`): its listener, its epoll set and the eventfd that wakes it.
const FDS_THE_SOCKET: u64 = 3;
/// The supervisor's epoll set ([`crate::supervise::Supervisor::start`]), made once the node's bounds are fitted, as its
/// budget is the inits'.
const FDS_THE_SUPERVISOR: u64 = 1;
/// A program place's streams at their most ([`crate::serve`]): for each of the [`hex_core::wire::MAX_FDS`] an exec
/// carries, the one passed, or the pipe of two the node makes for one asked for (`hex_api::Stdio::make_asked`),
/// until its program starts; then each asked for's given end, kept until its client takes it, or, for a program in a
/// guest, each stream's connection to the guest and the passed pipe copied through it.
const FDS_A_PLACE_STREAM: u64 = 2;
/// A VM box's besides a box's (a Firecracker VM or a Shards guest, a row of the supervisor's inits): its cgroup's
/// directory and `cgroup.events` (`crate::supervise`'s `ManagedVm`), its guest's control connections (a call's and the
/// evented one, `hex_runtime::guest`'s `Booted`), and while it is made its readiness listener, the VMM's output pipe
/// and its staging connection (`hex_runtime::firecracker`).
const FDS_A_VM: u64 = 7;
/// The VM boxes' root's own, once the node runs them (`crate::node`'s `initialize_vm_root`): its directory and the
/// native VM owner's lease.
const FDS_THE_VM_ROOT: u64 = 2;

/// The mounts made ahead's (crate::devfs): the prepared `/dev` pool's at its bound, one descriptor each, and the
/// proc pool's, two each (its pid namespace and its tree), each pool holding at most the seeds' bound
/// (`hex_core::devfs::Shape::cap`).
fn devfs_held(sizes: Sizes) -> u64 {
    let each_pool = sizes.seeds.get();
    of(each_pool, 1).saturating_add(of(each_pool, 2))
}
/// The one maker of both pools at its peak, a proc's (init::prepare_proc): the worker's own pid namespace, the one
/// made, proc's context and its tree, and a mask's target and its clone.
const FDS_A_DEVFS_MAKE: u64 = 6;
/// The mask sources a root node keeps (init::ProcSources): the host's `/dev/null` and the empty read-only tmpfs.
const FDS_MASK_SOURCES: u64 = 2;
/// A call's takes of the mounts made ahead until its box's init has them: a `/dev` and a proc's two.
const FDS_DEVFS_TAKES: u64 = 3;

/// The bounds of the node's pools made ahead: decision 4's network namespaces, one descriptor each, and step 4's
/// ipc and uts pairs, two each; and whether it keeps the mounts made ahead ([`crate::devfs::Devfs`]), a root
/// node's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pools {
    /// The most network namespaces the pool holds.
    pub netns: usize,
    /// The most ipc and uts pairs the pool holds.
    pub ipc_uts: usize,
    /// Whether the node keeps step 3's prepared `/dev` pool, goal 6's prepared proc pool and their mask sources.
    pub devfs: bool,
    /// The node's pools' bounds on its host, which the kept namespaces and the seeds are counted at.
    pub sizes: Sizes,
    /// The files each cgroup made ahead opens ahead (`hex_runtime::linux::cgroup::ahead_files` over the disks its
    /// boxes' `io` bounds apply to), each a descriptor of the node's.
    pub files_ahead: usize,
    /// Whether the node runs VM boxes (a Firecracker VM's or a Shards guest's), each holding descriptors of the node's
    /// past a box's ([`Pools::running_vms`]).
    pub vms: bool,
}

impl Pools {
    /// A root node's on a host whose pools' bounds are `sizes`, each pool at its bound unless set: the network
    /// namespaces and ipc and uts pairs at the most their learned sizes may reach; its cgroups made ahead each open
    /// `files_ahead` files.
    pub fn root(sizes: Sizes, files_ahead: usize) -> Self {
        Self {
            netns: sizes.netns,
            ipc_uts: sizes.ipc_uts,
            devfs: true,
            sizes,
            files_ahead,
            vms: false,
        }
    }

    /// A rootless node's, which makes no namespaces or mounts ahead; its cgroups made ahead each open `files_ahead`
    /// files.
    pub fn rootless(sizes: Sizes, files_ahead: usize) -> Self {
        Self {
            netns: 0,
            ipc_uts: 0,
            devfs: false,
            sizes,
            files_ahead,
            vms: false,
        }
    }

    /// These pools for a node that runs VM boxes, or doesn't, as `vms` says: its start counts each VM box's
    /// descriptors among its own (`crate::node::Runs`).
    pub fn running_vms(self, vms: bool) -> Self {
        Self { vms, ..self }
    }

    /// The descriptors the pools hold at their bounds, each pool's refill thread's beside, and the takes of every
    /// create a seed serves at once ([`Sizes::seeds`]), one of each pool's until its box's init joins them through
    /// the seed's clone. A pool of none has no thread. The mounts made ahead the same, where the node keeps them,
    /// their mask sources beside.
    fn descriptors(self) -> u64 {
        let pool = |bound: usize, each: u64| {
            if bound == 0 {
                0
            } else {
                of(bound, each).saturating_add(FDS_A_REFILL.saturating_mul(each))
            }
        };
        let takes =
            u64::from(self.netns > 0).saturating_add(u64::from(self.ipc_uts > 0).saturating_mul(2));
        let devfs = if self.devfs {
            devfs_held(self.sizes)
                .saturating_add(FDS_A_DEVFS_MAKE)
                .saturating_add(FDS_MASK_SOURCES)
                .saturating_add(of(self.sizes.seeds.get(), FDS_DEVFS_TAKES))
        } else {
            0
        };
        pool(self.netns, 1)
            .saturating_add(pool(self.ipc_uts, 2))
            .saturating_add(of(self.sizes.seeds.get(), takes))
            .saturating_add(devfs)
    }
}

fn of(count: usize, each: u64) -> u64 {
    u64::try_from(count)
        .unwrap_or(u64::MAX)
        .saturating_mul(each)
}

/// The kept namespaces there may be at once: idle, on the checkers' line, and one for each `oci` box the node holds
/// ([`Sizes::boxes`]).
fn kept_at_most(sizes: Sizes) -> u64 {
    of(sizes.idle_netns, 2).saturating_add(of(sizes.boxes, 1))
}

/// The descriptors this node may hold at once without the namespaces' `/proc/net` files, each by its bound and named: the
/// kept namespaces; each live box's own; the programs its socket watches, a pidfd each, at the node's budget for
/// them ([`crate::serve::limits`]); the seeds of both pools (the exec seeds at their most, [`crate::seed::most`]);
/// the namespaces and the mounts made ahead under `pools`; the inits parked in idle cgroups and the cgroups made
/// ahead, each with its parked init and the files it opens ahead; the create seeds' one pruned mount namespace, which a
/// root node holds for its life (`goal6-mntns.md` §12), counted for every node, as a rootless node holds none and a
/// bound may be one over, never under; its socket's connections at their most ([`connections`]), the one it accepts
/// past them to refuse and the socket's own, counted as the pruned namespace is, as a node embedded in a process
/// serves no socket; each program place's streams, and the message read past the places, refused with its streams
/// closed ([`crate::serve`] takes a program's place as it reads its call); the exec'd programs' pidfds the supervisor
/// keeps to reap, at the same budget, which it holds for every caller; where the node runs VM boxes, each one's past a
/// box's and their root's; the supervisor's epoll set; and `beside`, what the process held open as the bound was taken
/// (its standard streams, the node's store, keeper and roots, and the GPU handles of a prober made before it). Each
/// bound among them is fitted to the hard limit as the node starts ([`hold_descriptors`]), so they hold no more than
/// it (the lead, 2026-11-11, over 2026-11-06's connections taking what these left).
fn descriptor_terms(pools: Pools, beside: u64) -> [(&'static str, u64); 17] {
    let parked = if pools.devfs {
        FDS_A_PARKED.saturating_add(FDS_A_PARKED_PROC)
    } else {
        FDS_A_PARKED
    };
    let made_ahead = FDS_A_MADE_AHEAD
        .saturating_add(u64::try_from(pools.files_ahead).unwrap_or(u64::MAX))
        .saturating_add(parked);
    let places = crate::serve::limits(&pools.sizes.platform()).programs;
    let streams = u64::try_from(hex_core::wire::MAX_FDS).unwrap_or(u64::MAX);
    let (vm_boxes, vm_root) = if pools.vms {
        (of(pools.sizes.inits, FDS_A_VM), FDS_THE_VM_ROOT)
    } else {
        (0, 0)
    };
    [
        ("the pools made ahead", pools.descriptors()),
        (
            "kept namespaces",
            kept_at_most(pools.sizes).saturating_mul(FDS_A_KEPT),
        ),
        ("live boxes", of(pools.sizes.boxes, FDS_A_BOX)),
        (
            "programs watched",
            of(crate::serve::limits(&pools.sizes.platform()).programs, 1),
        ),
        // The device programs held, a descriptor each at their bound. A rootless node loads none, so its count is an
        // over-bound there, and harmless: it keeps no namespaces whose files this bounds.
        ("device programs", of(pools.sizes.programs, 1)),
        ("create seeds", of(pools.sizes.seeds.get(), FDS_A_SEED)),
        (
            "exec seeds",
            of(crate::seed::most(&pools.sizes), FDS_A_SEED),
        ),
        (
            "idle cgroups' parked inits",
            of(pools.sizes.idle_cgroups, parked),
        ),
        ("cgroups made ahead", of(pools.sizes.ahead, made_ahead)),
        ("the create seeds' pruned mount namespace", FDS_PRUNED),
        (
            "the socket's connections",
            of(connections(&pools.sizes), 1)
                .saturating_add(1)
                .saturating_add(FDS_THE_SOCKET),
        ),
        (
            "the program places' streams",
            of(places, FDS_A_PLACE_STREAM.saturating_mul(streams)).saturating_add(streams),
        ),
        ("the exec'd programs the supervisor keeps", of(places, 1)),
        ("VM boxes", vm_boxes),
        ("the VM boxes' root", vm_root),
        ("the supervisor's epoll set", FDS_THE_SUPERVISOR),
        ("beside those", beside),
    ]
}

/// The connections the node's socket holds at most, a descriptor each: its bound on them, and no more than its
/// programs' and calls' budgets admit, as it keeps [`crate::serve::KEPT`] of each for every connection it admits
/// ([`crate::serve::limits`]).
fn connections(sizes: &Sizes) -> usize {
    let limits = crate::serve::limits(&sizes.platform());
    let admitted = |budget: usize| budget.checked_div(crate::serve::KEPT).unwrap_or(budget);
    limits
        .connections
        .min(admitted(limits.programs))
        .min(admitted(limits.in_flight))
}

/// All the descriptors this node may hold at once without the namespaces' files ([`descriptor_terms`]).
fn descriptors_of_the_node(pools: Pools, beside: u64) -> u64 {
    descriptor_terms(pools, beside)
        .into_iter()
        .map(|(_, n)| n)
        .fold(0, u64::saturating_add)
}

/// How many kept namespaces may hold their `/proc/net` files at once: what the soft descriptor limit, which the node's
/// start raised to its hard one ([`hold_descriptors`]), leaves past its own descriptors at its bounds, four a
/// namespace, none where it leaves none. A namespace past the bound is checked by entering it, counted
/// (`files_bounded`), as one whose files couldn't be read is, and as a rootless node's are. The node's own are fitted
/// to the limit as it starts, so the files and its own together never pass it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesBound {
    /// The soft limit, `None` where it is unlimited.
    pub soft: Option<u64>,
    /// The node's own descriptors at its bounds, without the files ([`descriptors_of_the_node`]), those it held
    /// as the bound was taken among them.
    pub own: u64,
    /// The namespaces that may hold their files at once.
    pub namespaces: u64,
}

impl FilesBound {
    /// The bound under `limit` for a node whose pools are `pools` and that holds `beside` descriptors past its
    /// bounds, an unlimited one holding them for every namespace there may be.
    pub fn of(limit: &rustix::process::Rlimit, pools: Pools, beside: u64) -> Self {
        let own = descriptors_of_the_node(pools, beside);
        let namespaces = match limit.current {
            None => kept_at_most(pools.sizes),
            Some(soft) => (soft.saturating_sub(own) / FILES_A_KEPT).min(kept_at_most(pools.sizes)),
        };
        Self {
            soft: limit.current,
            own,
            namespaces,
        }
    }

    /// The bound under this process's descriptor limit now, for a node whose pools are `pools`, with the
    /// descriptors it holds now counted ([`open_descriptors`]): learned, not stated (Ada's 2026-11-06 rule).
    pub fn now(pools: Pools) -> Result<Self, Refusal> {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        Ok(Self::of(&limit, pools, open_descriptors(&limit)?))
    }

    /// What a node whose pools are `pools`, holding `beside` past its bounds, needs of its soft descriptor limit to
    /// hold every namespace it may keep with its files: its own descriptors at its bounds, as [`FilesBound::of`]
    /// counts them, and four for each namespace a root node may keep; a rootless node keeps none, its own alone. The
    /// node says it (`hex node --descriptors`), so an installer gives its service that and the install lab checks it,
    /// never a figure copied from one host's (goal 6 step 6's HEX-INST-5; Ada's 2026-11-06 constants rule).
    pub fn needed(pools: Pools, beside: u64) -> Needed {
        // The namespaces kept for owners: none on a node without a pool (a rootless node's containers make theirs in
        // user namespaces of their own), so none of their files either.
        let kept = if pools.netns > 0 {
            kept_at_most(pools.sizes)
        } else {
            0
        };
        Needed {
            own: descriptors_of_the_node(pools, beside),
            files: kept.saturating_mul(FILES_A_KEPT),
        }
    }
}

/// The descriptors a node needs, by what holds them ([`FilesBound::needed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Needed {
    /// Its own at its bounds, those it held beside them among them.
    pub own: u64,
    /// Every namespace it may keep, holding its `/proc/net` files.
    pub files: u64,
}

impl Needed {
    /// The soft limit that holds both.
    pub fn total(self) -> u64 {
        self.own.saturating_add(self.files)
    }
}

/// What a node of this process's owner, its state under `state`, running what `runs` says, needs of its descriptor
/// limit on this host at a first start's bounds ([`crate::sizes::Sizes::now`]): its own and every kept namespace's
/// files ([`FilesBound::needed`]), its cgroups made ahead opening their files over the disks under `state`, which its
/// boxes' `io` bounds apply to. What this process holds now, with the GPU handles a node's prober holds opened
/// ([`crate::facts::shared_prober`]), stands for what the node holds beside its bounds. The one formula for it: what
/// `hex node --descriptors` says and what a guest's init gives its node (the lead, 2026-11-11), so an installer gives
/// its service that and the install lab checks it, never a figure copied from one host's (goal 6 step 6's HEX-INST-5).
pub fn needed_on_this_host(state: &std::path::Path, runs: crate::Runs) -> Result<Needed, Refusal> {
    let sizes = Sizes::now()?;
    let disks = hex_runtime::linux::disks::under(state)?;
    let files_ahead = hex_runtime::linux::cgroup::ahead_files(&disks).len();
    let pools = if rustix::process::geteuid().is_root() {
        Pools::root(sizes, files_ahead)
    } else {
        Pools::rootless(sizes, files_ahead)
    }
    .running_vms(runs.vms);
    let _prober = crate::facts::shared_prober(state, crate::facts::Declared::default())?;
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    Ok(FilesBound::needed(pools, open_descriptors(&limit)?))
}

/// A node's start within its descriptor limit, whatever process it runs in, `hex node` or an embedding: the soft limit
/// raised to the hard one (setrlimit(2), as any process may), and every bound that holds descriptors fitted to what the
/// hard limit leaves past those held ([`fit`]; bounds come from the OS limit at run time, less what is held, as the
/// namespaces' files' do, [`FilesBound::of`]: the lead's ruling, 2026-11-11). The node then admits what fits, refusing
/// past it by its budgets' own refusals: its create turns' (`Refusal::Full`), its supervisor's inits' and its socket's
/// connections' and programs' (`Refusal::Exhausted`). Its start is refused by type only where the hard limit can't hold
/// the least node, one box and one connection, naming both. The fitted pools come with the bound on the namespaces'
/// files under the same limit, beside the same descriptors held, so the node's own and the files together fit it.
pub fn hold_descriptors(pools: Pools) -> Result<(Pools, FilesBound), Refusal> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let given = getrlimit(Resource::Nofile);
    if let (Some(soft), Some(hard)) = (given.current, given.maximum)
        && soft < hard
    {
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(hard),
                maximum: Some(hard),
            },
        )
        .map_err(|e| Refusal::os("raising the descriptor limit to its hard one", e))?;
    }
    let limit = getrlimit(Resource::Nofile);
    let beside = open_descriptors(&limit)?;
    let shown = |n: Option<u64>| n.map_or_else(|| "unlimited".to_owned(), |n| n.to_string());
    // An unlimited limit holds the node at the bounds its host gave.
    let hard = limit.current.unwrap_or(u64::MAX);
    let fitted = fit(pools, hard, beside).map_err(|least| Refusal::Platform {
        what: "a node within its descriptor limit",
        platform: "linux",
        why: format!(
            "the least node, one box and one connection, holds {least} descriptors, past RLIMIT_NOFILE: the hard limit \
             of {}, its soft one raised to {hard} from {} as given",
            shown(limit.maximum),
            shown(given.current)
        ),
    })?;
    Ok((fitted, FilesBound::of(&limit, fitted, beside)))
}

/// `pools` with every bound that holds descriptors fitted to `hard`, the node holding `beside` past its bounds: where
/// its own at the bounds its host gave fit, they stay; where they don't, each such bound (boxes, inits, device
/// programs, seeds, idle and made-ahead cgroups, idle and made-ahead namespaces, ipc and uts pairs, and with the inits
/// the socket's connections) keeps one common share of what it would hold past its least, the largest whose
/// descriptors fit, found by halving the shares (at most 64 halvings, a `u64`'s width). The socket's bound on its
/// connections is then what it admits ([`connections`]), and the namespaces' files take what the node's own leave
/// ([`FilesBound::of`]). `Err` carries the least node's descriptors, one box, init, device program, seed and
/// connection and no pool made ahead, where even those don't fit.
pub fn fit(pools: Pools, hard: u64, beside: u64) -> Result<Pools, u64> {
    let own = |at: Pools| descriptors_of_the_node(at, beside);
    let least = own(shrunk(pools, 0, 1));
    if least > hard {
        return Err(least);
    }
    let whole = own(pools);
    let mut fitted = if whole <= hard {
        pools
    } else {
        shrunk(pools, share(pools, hard, beside), whole)
    };
    fitted.sizes.descriptors = connections(&fitted.sizes);
    Ok(fitted)
}

/// The largest share of what `pools`' bounds hold past their least, in parts of the descriptors they hold whole, whose
/// descriptors fit `hard` beside `beside` ([`fit`]). Each bound holds at most the whole, so each part moves each bound
/// by one at most. Share 0 is the least node's, which the caller has found to fit; and the node's own grows with the
/// share, so halving finds it.
fn share(pools: Pools, hard: u64, beside: u64) -> u64 {
    let whole = descriptors_of_the_node(pools, beside);
    let (mut low, mut high) = (0u64, whole);
    while low < high {
        let mid = low.saturating_add(high.saturating_sub(low).div_ceil(2));
        if descriptors_of_the_node(shrunk(pools, mid, whole), beside) <= hard {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    low
}

/// `pools` with each bound that holds descriptors at its least plus the share `num`/`den` of the rest, its learned
/// starts within them.
fn shrunk(pools: Pools, num: u64, den: u64) -> Pools {
    let mut sizes = pools.sizes;
    sizes.boxes = scaled(sizes.boxes, 1, num, den);
    sizes.inits = scaled(sizes.inits, 1, num, den).min(sizes.boxes);
    sizes.programs = scaled(sizes.programs, 1, num, den).min(sizes.boxes);
    sizes.seeds = std::num::NonZeroUsize::new(scaled(sizes.seeds.get(), 1, num, den))
        .unwrap_or(std::num::NonZeroUsize::MIN);
    sizes.idle_cgroups = scaled(sizes.idle_cgroups, 0, num, den);
    sizes.ahead = scaled(sizes.ahead, 0, num, den);
    sizes.idle_netns = scaled(sizes.idle_netns, 0, num, den);
    sizes.netns = scaled(sizes.netns, 0, num, den);
    sizes.ipc_uts = scaled(sizes.ipc_uts, 0, num, den);
    let starts = &mut sizes.starts;
    starts.owner_cgroups = starts.owner_cgroups.min(sizes.idle_cgroups);
    starts.ahead = starts.ahead.min(sizes.ahead);
    starts.owner_netns = starts.owner_netns.min(sizes.idle_netns);
    starts.netns = starts.netns.min(sizes.netns);
    starts.ipc_uts = starts.ipc_uts.min(sizes.ipc_uts);
    starts.seeds = starts.seeds.min(sizes.seeds.get());
    starts.devfs_per_key = starts.devfs_per_key.min(sizes.seeds.get());
    Pools {
        netns: scaled(pools.netns, 0, num, den),
        ipc_uts: scaled(pools.ipc_uts, 0, num, den),
        devfs: pools.devfs,
        sizes,
        files_ahead: pools.files_ahead,
        vms: pools.vms,
    }
}

/// `least` plus the share `num`/`den` of what `bound` holds past it, never past `bound`; a bound under `least` as it
/// is.
fn scaled(bound: usize, least: usize, num: u64, den: u64) -> usize {
    let Some(span) = bound.checked_sub(least) else {
        return bound;
    };
    let part = u128::from(u64::try_from(span).unwrap_or(u64::MAX))
        .saturating_mul(u128::from(num))
        .checked_div(u128::from(den))
        .unwrap_or(0);
    least
        .saturating_add(usize::try_from(part).unwrap_or(usize::MAX))
        .min(bound)
}

/// How many descriptors this process holds now: the entries of `/proc/self/fd` (proc(5)), less the one the
/// listing itself opens. The listing ends within the soft limit's entries, since no process holds more
/// (getrlimit(2), `RLIMIT_NOFILE`), and refuses by type past it. Linux's alone, as this module is: macOS
/// and Windows nodes keep no namespaces to bound files for.
pub fn open_descriptors(limit: &rustix::process::Rlimit) -> Result<u64, Refusal> {
    let listing = |e| Refusal::os("counting the agent's open descriptors in /proc/self/fd", e);
    let most = limit.current.unwrap_or(u64::MAX);
    let mut count = 0u64;
    for entry in std::fs::read_dir("/proc/self/fd").map_err(listing)? {
        entry.map_err(listing)?;
        count = count.saturating_add(1);
        if count > most.saturating_add(1) {
            return Err(Refusal::Os {
                op: "counting the agent's open descriptors in /proc/self/fd",
                why: format!("more than the soft limit's {most} listed"),
            });
        }
    }
    Ok(count.saturating_sub(1))
}

/// A place under the [`FilesBound`], taken before a check opens a namespace's files and given back as it drops
/// with them, so at most the bound's namespaces hold files at once.
struct FilesSlot(Arc<AtomicU64>);

impl FilesSlot {
    /// One of `held`'s places, `None` with all `bound` of them taken.
    fn take(held: &Arc<AtomicU64>, bound: u64) -> Option<Self> {
        held.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < bound).then(|| n.checked_add(1)).flatten()
        })
        .ok()
        .map(|_| Self(Arc::clone(held)))
    }
}

impl Drop for FilesSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct KeptState {
    idle: Mutex<Idle<KeptNet>>,
    /// Each live box's namespace, kept from its create to its delete, with its owner: at most one a box.
    held: Mutex<BTreeMap<BoxId, (String, KeptNet)>>,
    /// Namespaces given back as their boxes went, each then checked or closed unchecked.
    given_back: AtomicU64,
    /// Namespaces the checker ruled on, kept idle or closed.
    checked: AtomicU64,
    /// Namespaces given back with the checkers' line full (the node's `idle_netns`), or after every checker
    /// ended,
    /// closed unchecked.
    unchecked: AtomicU64,
    /// Checks after which the checker couldn't go back to its own namespace.
    lost_home: AtomicU64,
    /// Namespaces the node made on a create's path for a box it keeps one for, the pool dry, which the box's
    /// init joins as it joins one taken from the pool. The pool counts each among its takes made on the path
    /// too, with the takes whose container makes its own, so this tells the two apart.
    made_on_path: AtomicU64,
    /// Checks whose sockets' count wasn't the check's own socket alone, which read the protocols.
    protocols: AtomicU64,
    /// Checks that came to their verdict through the namespace's held files, with no entry.
    through_files: AtomicU64,
    /// Checks in which a read or an open of those files failed, which looked by entering instead.
    files_failed: AtomicU64,
    /// Checks that entered because the namespace held no files with the [`FilesBound`] all taken.
    files_bounded: AtomicU64,
    /// The namespaces that may hold their files at once.
    files_bound: FilesBound,
    /// The namespaces holding them now ([`FilesSlot`]).
    files_held: Arc<AtomicU64>,
    /// The time the checks took, all together, in nanoseconds: each check's wall time on the checker.
    check_ns: AtomicU64,
}

/// What the kept namespaces have done, as the bench and the tests read it. Every namespace given back is
/// checked or closed unchecked, so `checked + unchecked` reaches `given_back` once the checker has ruled on
/// each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReuseCounts {
    pub pool: netns_reuse::Counts,
    pub given_back: u64,
    pub checked: u64,
    pub unchecked: u64,
    pub lost_home: u64,
    pub made_on_path: u64,
    /// Checks that read the protocols, `sockstat`'s count past the check's own socket.
    pub protocols: u64,
    /// Checks that came to their verdict through the namespace's held files, with no entry.
    pub through_files: u64,
    /// Checks in which a read or an open of those files failed, which looked by entering instead.
    pub files_failed: u64,
    /// Checks that entered because the namespace held no files with the [`FilesBound`] all taken.
    pub files_bounded: u64,
    /// The bound the node runs with.
    pub files_bound: Option<FilesBound>,
    /// The namespaces holding their files now.
    pub files_held: u64,
    /// The checks' time, all together, in nanoseconds.
    pub check_ns: u64,
}

struct Shared {
    pool: Mutex<Pool<OwnedFd>>,
    /// The refill thread waits on it while the pool wants nothing. A take, which may leave the pool
    /// wanting one, and shutdown wake it; nothing else changes what it waits for.
    wake: Condvar,
}

impl Shared {
    fn pool(&self) -> Result<MutexGuard<'_, Pool<OwnedFd>>, OciError> {
        self.pool.lock().map_err(|_| {
            OciError::Broken("the network-namespace pool's lock was poisoned".to_owned())
        })
    }
}

impl Netns {
    /// A pool that holds at most `bound` namespaces, and the thread that starts filling it at once, its kept
    /// namespaces holding their `/proc/net` files under `files`. With a bound of 0 there is no thread, and every
    /// container that makes a network namespace makes it on its create's path, counted, as a rootless node's
    /// containers do (decision 4). The namespaces kept idle for their owners are bounded by `sizes`: the node's
    /// `idle_netns`, and each owner's learned from its fresh takes, starting at the host's parallelism.
    pub fn start(bound: usize, files: FilesBound, sizes: Sizes) -> Result<Self, Refusal> {
        Self::open(Pool::new(bound), bound, files, sizes)
    }

    /// As [`Netns::start`], the pool starting where the node's last start peaked (the host's parallelism on its first)
    /// and learning its size from its draws, up to
    /// `cap` ([`Pool::learning`]; Ada's rule, 2026-11-06): a root node's default, `cap` its [`Sizes::netns`].
    pub fn learning(cap: usize, files: FilesBound, sizes: Sizes) -> Result<Self, Refusal> {
        Self::open(Pool::learning(sizes.starts.netns, cap), cap, files, sizes)
    }

    fn open(
        pool: Pool<OwnedFd>,
        cap: usize,
        files: FilesBound,
        sizes: Sizes,
    ) -> Result<Self, Refusal> {
        let shared = Arc::new(Shared {
            pool: Mutex::new(pool),
            wake: Condvar::new(),
        });
        if cap == 0 {
            return Ok(Self {
                shared,
                refill: None,
                kept: None,
            });
        }
        let filling = Arc::clone(&shared);
        let refill = std::thread::Builder::new()
            .name("hex-netns".to_owned())
            .spawn(move || refill(&filling))
            .map_err(|e| Refusal::os("starting the network-namespace thread", e))?;
        let state = Arc::new(KeptState {
            idle: Mutex::new(Idle::new(sizes.starts.owner_netns, sizes.idle_netns)),
            held: Mutex::new(BTreeMap::new()),
            given_back: AtomicU64::new(0),
            checked: AtomicU64::new(0),
            unchecked: AtomicU64::new(0),
            lost_home: AtomicU64::new(0),
            made_on_path: AtomicU64::new(0),
            protocols: AtomicU64::new(0),
            through_files: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            files_bounded: AtomicU64::new(0),
            files_bound: files,
            files_held: Arc::new(AtomicU64::new(0)),
            check_ns: AtomicU64::new(0),
        });
        let (returns, line) = std::sync::mpsc::sync_channel(sizes.idle_netns);
        let line = Arc::new(Mutex::new(line));
        let mut checkers = Vec::with_capacity(CHECKERS);
        for n in 0..CHECKERS {
            let (checking, taking) = (Arc::clone(&state), Arc::clone(&line));
            let checker = std::thread::Builder::new()
                .name(format!("hex-netns-check-{n}"))
                .spawn(move || check_returns(&checking, &taking))
                .map_err(|e| Refusal::os("starting a network-namespace checker", e))?;
            checkers.push(checker);
        }
        Ok(Self {
            shared,
            refill: Some(refill),
            kept: Some(Keeping {
                state,
                returns: Some(returns),
                checkers,
            }),
        })
    }

    /// The chooser for one create of a box of `owner` (none when `None`): it answers the runtime's take
    /// from the owner's kept namespaces, or keeps the one it gives, and says after which it kept.
    pub fn choice<'a>(&'a self, owner: Option<&'a BoxOwner>) -> NetChoice<'a> {
        NetChoice {
            netns: self,
            owner,
            kept: Mutex::new(None),
        }
    }

    /// Keeps box `id`'s namespace, `ns`, for its owner, from its create on.
    pub fn hold(&self, id: &BoxId, owner: &str, ns: KeptNet) -> Result<(), Refusal> {
        let Some(kept) = &self.kept else {
            return Ok(());
        };
        kept.state
            .held
            .lock()
            .map_err(|_| Refusal::Broken("the kept namespaces' lock was poisoned".to_owned()))?
            .insert(id.clone(), (owner.to_owned(), ns));
        Ok(())
    }

    /// Box `id` is gone, its record too: its namespace, if kept, goes to the checker, which keeps it idle
    /// for the owner's next box if it passes ([`netcheck::check`]) and closes it if not. With the checker's
    /// line full it's closed unchecked, counted.
    pub fn give_back(&self, id: &BoxId) -> Result<(), Refusal> {
        let Some(kept) = &self.kept else {
            return Ok(());
        };
        let held = kept
            .state
            .held
            .lock()
            .map_err(|_| Refusal::Broken("the kept namespaces' lock was poisoned".to_owned()))?
            .remove(id);
        let (Some((owner, ns)), Some(returns)) = (held, &kept.returns) else {
            return Ok(());
        };
        // Counted before it's sent, so a reader never sees it ruled on and not given back.
        kept.state.given_back.fetch_add(1, Ordering::AcqRel);
        match returns.try_send((owner, ns)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(gone) | TrySendError::Disconnected(gone)) => {
                kept.state.unchecked.fetch_add(1, Ordering::AcqRel);
                drop(gone);
                Ok(())
            }
        }
    }

    /// What the kept namespaces have done; `None` on a node that keeps none.
    pub fn reuse_counts(&self) -> Result<Option<ReuseCounts>, Refusal> {
        let Some(kept) = &self.kept else {
            return Ok(None);
        };
        // The rulings first: each is counted after its verdict is in the pool, so the pool read after shows
        // every verdict counted here.
        let checked = kept.state.checked.load(Ordering::Acquire);
        let unchecked = kept.state.unchecked.load(Ordering::Acquire);
        let pool = kept
            .state
            .idle
            .lock()
            .map_err(|_| Refusal::Broken("the kept namespaces' lock was poisoned".to_owned()))?
            .counts();
        Ok(Some(ReuseCounts {
            pool,
            given_back: kept.state.given_back.load(Ordering::Acquire),
            checked,
            unchecked,
            lost_home: kept.state.lost_home.load(Ordering::Acquire),
            made_on_path: kept.state.made_on_path.load(Ordering::Acquire),
            protocols: kept.state.protocols.load(Ordering::Acquire),
            through_files: kept.state.through_files.load(Ordering::Acquire),
            files_failed: kept.state.files_failed.load(Ordering::Acquire),
            files_bounded: kept.state.files_bounded.load(Ordering::Acquire),
            files_bound: Some(kept.state.files_bound),
            files_held: kept.state.files_held.load(Ordering::Acquire),
            check_ns: kept.state.check_ns.load(Ordering::Acquire),
        }))
    }

    /// Closes every idle kept namespace, counted as drained: the node lets them go.
    pub fn release_idle(&self) -> Result<(), Refusal> {
        let Some(kept) = &self.kept else {
            return Ok(());
        };
        let gone = kept
            .state
            .idle
            .lock()
            .map_err(|_| Refusal::Broken("the kept namespaces' lock was poisoned".to_owned()))?
            .drain();
        drop(gone);
        Ok(())
    }

    /// A namespace made on the calling thread, when the pool is dry and the node keeps the box's: made
    /// as the pool makes them ([`net::make_ahead`]), so the node holds it, and counted
    /// ([`ReuseCounts::made_on_path`]).
    fn make_on_path(&self) -> Result<OwnedFd, OciError> {
        let home = net::thread_network()?;
        let ns = net::make_ahead(home.as_fd()).map_err(|f| f.error)?;
        if let Some(kept) = &self.kept {
            kept.state.made_on_path.fetch_add(1, Ordering::AcqRel);
        }
        Ok(ns)
    }

    /// What it has learned (Ada's rule, 2026-11-06): the pool's size, and the most idle namespaces an owner keeps
    /// (0 on a node that keeps none).
    pub fn learned(&self) -> Result<(usize, usize), Refusal> {
        let bound = self.shared.pool().map_err(Refusal::from)?.bound();
        let per_owner = match &self.kept {
            None => 0,
            Some(kept) => kept
                .state
                .idle
                .lock()
                .map_err(|_| Refusal::Broken("the kept namespaces' lock was poisoned".to_owned()))?
                .per_owner(),
        };
        Ok((bound, per_owner))
    }

    /// A pool refused because the node lacks what a make takes (`missing`): no thread, and every take is
    /// made on the path, counted under that reason ([`Pool::refused`]).
    pub fn refused(missing: Missing) -> Self {
        Self {
            shared: Arc::new(Shared {
                pool: Mutex::new(Pool::refused(missing)),
                wake: Condvar::new(),
            }),
            refill: None,
            kept: None,
        }
    }

    /// What the pool has done. Counted at the take, so a create that fails after it took a namespace,
    /// or found the pool dry, is counted all the same.
    pub fn counts(&self) -> Result<Counts, Refusal> {
        Ok(self.shared.pool().map_err(Refusal::from)?.counts())
    }
}

impl Networks for Netns {
    fn take(&self) -> Result<Option<OwnedFd>, OciError> {
        let taken = {
            let mut pool = self.shared.pool()?;
            let before = pool.bound();
            let taken = pool.take().map_err(|e| OciError::Broken(e.to_string()))?;
            if pool.bound() > before {
                crate::sizes::rose();
            }
            taken
        };
        self.shared.wake.notify_one();
        Ok(match taken {
            Take::Pooled(ns) => Some(ns),
            Take::MakeOnPath => None,
        })
    }

    fn on_path(&self) -> Result<(), OciError> {
        self.shared
            .pool()?
            .on_path()
            .map_err(|e| OciError::Broken(e.to_string()))
    }
}

impl Drop for Netns {
    fn drop(&mut self) {
        if let Ok(mut pool) = self.shared.pool.lock() {
            // Closed as they go: namespaces never used.
            drop(pool.shut_down());
        }
        self.shared.wake.notify_all();
        if let Some(refill) = self.refill.take() {
            let _ = refill.join();
        }
        if let Some(mut kept) = self.kept.take() {
            // The checker ends once its line is closed and drained.
            drop(kept.returns.take());
            for checker in kept.checkers.drain(..) {
                let _ = checker.join();
            }
            if let Ok(mut idle) = kept.state.idle.lock() {
                drop(idle.drain());
            }
        }
    }
}

/// One create's chooser ([`Netns::choice`]): the runtime's take answered from the box's owner's kept
/// namespaces when its spec lets it share, and the namespace it was given kept for the owner.
pub struct NetChoice<'a> {
    netns: &'a Netns,
    owner: Option<&'a BoxOwner>,
    kept: Mutex<Option<KeptNet>>,
}

impl NetChoice<'_> {
    /// The namespace this create was given and the node keeps for its owner, if it was.
    pub fn kept(self) -> Option<KeptNet> {
        self.kept.into_inner().ok().flatten()
    }

    fn whose(&self, eligible: Result<(), Why>) -> Whose<'_> {
        Whose {
            owner: self.owner.map(|o| o.name.as_str()),
            reuse: self.owner.is_some_and(|o| o.reuse),
            eligible,
        }
    }

    fn idle(&self) -> Result<Option<MutexGuard<'_, Idle<KeptNet>>>, OciError> {
        match &self.netns.kept {
            None => Ok(None),
            Some(kept) => {
                kept.state.idle.lock().map(Some).map_err(|_| {
                    OciError::Broken("the kept namespaces' lock was poisoned".to_owned())
                })
            }
        }
    }
}

impl Networks for NetChoice<'_> {
    fn take(&self) -> Result<Option<OwnedFd>, OciError> {
        self.netns.take()
    }

    fn on_path(&self) -> Result<(), OciError> {
        if let Some(mut idle) = self.idle()? {
            // Counted, never kept: a namespace in a user namespace of the box's own.
            let _ = idle.take(self.whose(Err(Why::OwnUserNamespace)));
        }
        self.netns.on_path()
    }

    fn take_for(&self, may_share: Result<(), Why>) -> Result<Option<OwnedFd>, OciError> {
        let taken = match self.idle()? {
            None => return self.netns.take(),
            Some(mut idle) => {
                let before = idle.per_owner();
                let taken = idle.take(self.whose(may_share));
                if idle.per_owner() > before {
                    crate::sizes::rose();
                }
                if matches!(taken, Kept::Idle(_)) {
                    // Checked as its box went (`check_returns`), and nothing ran in it since.
                    idle.hand_on();
                }
                taken
            }
        };
        let (ns, socket, files, slot) = match taken {
            Kept::Own(_) => return self.netns.take(),
            Kept::Idle(kept) => (kept.ns, kept.socket, kept.files, kept.slot),
            Kept::Fresh => match self.netns.take()? {
                Some(ns) => (ns, None, None, None),
                None => (self.netns.make_on_path()?, None, None, None),
            },
        };
        let keep = KeptNet {
            ns: ns
                .try_clone()
                .map_err(|e| OciError::os("keeping a box's network namespace", e))?,
            socket,
            files,
            slot,
        };
        *self.kept.lock().map_err(|_| {
            OciError::Broken("a create's kept namespace's lock was poisoned".to_owned())
        })? = Some(keep);
        Ok(Some(ns))
    }
}

/// A checker, one of [`CHECKERS`]: checks each namespace it takes off the line against a fresh namespace's
/// dumps ([`Fresh::read`]), keeping it idle for its owner if it passes and closing it, counted by why, if not.
/// It ends once the line is closed. Until it has read a fresh namespace's dumps, which it tries again at each
/// namespace it takes, it refuses each as unreadable, so a node whose kernel it can't read keeps none and says
/// so in its counts. A check after which it couldn't go back to its own namespace refuses the namespace and
/// ends that checker, counted, so no namespace is checked from another's; once every checker has ended, those
/// given back are closed unchecked, counted ([`Netns::give_back`]).
fn check_returns(state: &KeptState, line: &Mutex<Receiver<(String, KeptNet)>>) {
    let home = net::thread_network().ok();
    let proc_net = ProcNet::of_this_thread().ok();
    let mut fresh = None;
    let mut scratch = Scratch::default();
    loop {
        // One checker waits on the line at a time, the others on its lock; each checks what it took apart.
        let next = match line.lock() {
            Ok(line) => line.recv(),
            Err(_) => return,
        };
        let Ok((owner, mut kept)) = next else {
            return;
        };
        if fresh.is_none()
            && let (Some(home), Some(proc_net)) = (&home, &proc_net)
        {
            fresh = Fresh::read(home.as_fd(), proc_net).ok();
        }
        let (verdict, lost) = match (&home, &proc_net, &fresh) {
            (Some(home), Some(proc_net), Some(fresh)) => {
                let started = std::time::Instant::now();
                // A namespace holding no files takes a place under the bound before its check may open them,
                // and gives it back below unless it holds them after.
                if kept.files.is_none() && kept.slot.is_none() {
                    kept.slot = FilesSlot::take(&state.files_held, state.files_bound.namespaces);
                    if kept.slot.is_none() {
                        state.files_bounded.fetch_add(1, Ordering::AcqRel);
                    }
                }
                let checked = netcheck::check(
                    kept.ns.as_fd(),
                    home.as_fd(),
                    proc_net,
                    fresh,
                    &mut scratch,
                    &mut kept.socket,
                    if kept.slot.is_some() {
                        Some(&mut kept.files)
                    } else {
                        None
                    },
                );
                if kept.files.is_none() {
                    kept.slot = None;
                }
                let took = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                state.check_ns.fetch_add(took, Ordering::AcqRel);
                if checked.protocols {
                    state.protocols.fetch_add(1, Ordering::AcqRel);
                }
                if checked.through_files {
                    state.through_files.fetch_add(1, Ordering::AcqRel);
                }
                if checked.files_failed {
                    state.files_failed.fetch_add(1, Ordering::AcqRel);
                }
                (checked.verdict, !checked.home)
            }
            _ => (Err(Why::Unreadable), false),
        };
        let verdict = if lost {
            state.lost_home.fetch_add(1, Ordering::Relaxed);
            Err(Why::Unreadable)
        } else {
            verdict
        };
        let Ok(mut idle) = state.idle.lock() else {
            state.unchecked.fetch_add(1, Ordering::AcqRel);
            return;
        };
        let closed = match verdict {
            Ok(()) => idle.give(&owner, kept),
            Err(why) => {
                idle.refuse(why);
                vec![kept]
            }
        };
        // Closed with the lock let go.
        drop(idle);
        drop(closed);
        state.checked.fetch_add(1, Ordering::AcqRel);
        if lost {
            return;
        }
    }
}

/// The refill thread: makes a namespace whenever the pool asks for one and hands it in, one at a time,
/// as the pool's count of what it asks for needs, and ends once the pool is shut down. It opens its
/// home namespace first; a thread that can't makes none, each make it is asked for failing, counted
/// under [`Step::Home`]. Each failed make is counted by the step that failed, its reason (the errno
/// and the error's text) kept as that step's latest, and said on the agent's stderr ([`failed_line`]):
/// at most one a take, since a failed make stops the asking until the next.
/// A namespace the pool refuses (past its bound, which one maker never reaches, or after shutdown) is
/// closed here. A poisoned lock ends it, and every create then refuses on the same lock.
fn refill(shared: &Shared) {
    let home = net::home();
    loop {
        let Ok(mut pool) = shared.pool.lock() else {
            return;
        };
        while !pool.is_shut_down() && pool.wanted() == 0 {
            pool = match shared.wake.wait(pool) {
                Ok(pool) => pool,
                Err(_) => return,
            };
        }
        if pool.is_shut_down() {
            return;
        }
        // Made with the lock let go, so a take never waits on a make.
        drop(pool);
        let began = std::time::Instant::now();
        let made = match &home {
            Ok(home) => net::make_ahead(home.as_fd()),
            Err(e) => Err(e.clone()),
        };
        if made.is_ok() {
            MAKES.answered(began.elapsed());
        }
        let Ok(mut pool) = shared.pool.lock() else {
            return;
        };
        match made {
            Ok(ns) => {
                let _ = pool.made(ns);
            }
            Err(failed) => {
                let reason = failed.reason();
                // Said where the agent's operator reads it; the reason is kept in the counts all the
                // same, so a stderr that can't be written loses nothing inspect can't show.
                let _ = std::io::stderr()
                    .lock()
                    .write_all(failed_line(failed.step, &reason).as_bytes());
                pool.failed(failed.step, reason);
            }
        }
    }
}

/// What the agent says on stderr when a make ahead fails: the step and why.
fn failed_line(step: Step, reason: &Reason) -> String {
    format!("hex node: a network namespace made ahead failed at {step}: {reason}\n")
}

#[cfg(test)]
mod tests;
