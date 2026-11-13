//! The node agent's pool of create seeds (decision 5 of docs/design/runtime-architecture.md, goal 5
//! plan step 4): the shell around [`hex_core::seeds::Seeds`], the pure policy step 3 built. A seed is a
//! prepared `hex __oci-seed` process the agent keeps and reuses; a create hands an idle one the
//! container's spec over its socket and the seed clones the container's init with `clone3` into its
//! cgroup and the fresh namespaces its spec asks for, as the agent's own child (`hex-runtime`'s
//! `seed::serve_create`), in place of the shim's re-exec of the hex binary. So a seam create spawns no
//! process of the agent's and re-execs no `hex` binary per call.
//!
//! The policy — how many seeds a node keeps ([`hex_core::seeds::Seeds::growing`], up to the node's bound,
//! [`crate::sizes::Sizes::seeds`]), which one each create is
//! handed, how long a create waits, when a seed is taken for dead or hung and restarted, and what it
//! counts — is the pure core's; this shell holds it behind one lock (no core call waits), and does each
//! create's socket round-trip with the lock let go, so creates to different seeds run at once. The core's
//! clock is the shell's monotonic time since the pool opened.
//!
//! **Seeds start ahead of the creates, never on one's path.** A thread of the pool's own starts each seed
//! the core asks for, with the lock let go: it keeps a spare up and idle beyond the creates waiting, as many
//! as the most creates it has seen waiting at once ([`Inner::spare`]), so a create finds one idle and no
//! create's path holds a seed's exec. The pool starts its
//! first spare as it opens. A create that finds none idle (a burst) waits in line for a seed to free or
//! to start, while the thread adds more, up to the bound.
//!
//! Bounds (CLAUDE.md §2), each a typed refusal past it: at most [`hex_core::seeds::Limits::seeds`] seeds
//! up or starting, [`Limits::waiting`] creates in line, a wait learned from the line's waits, and a seed
//! killed past the hang bound learned from its answers: the shell's wait for a seed's answer ends where the
//! core takes the seed for hung. A seed that dies, or a round trip that breaks, refuses its create by type
//! and leaves its slot, which the thread starts again, counted.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hex_api::Refusal;
use hex_core::demand::Learned;
use hex_core::seed::{Answer, Create, Failed, Fixed, MountNamespace, Park};
use hex_core::seeds::{Admit, Claim, Counts, Limits, Refused, SeedId, Seeds, Stale};
use hex_runtime::linux::oci::run::{ClassicReason, CloneInit, CloneReq, Parked};
use hex_runtime::linux::oci::seed as rt_seed;
use hex_runtime::linux::oci::{OciError, mntns};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};

/// Where a node's create seeds work (goal 5 plan step 2): in the one mount namespace the node prunes at `At`'s
/// mountpoint, once, as the pool opens (a root node's); or in the agent's own, kept for the reason `Keep` names, under
/// which their creates' classic roots are counted ([`CloneInit::unpruned`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prune {
    At(PathBuf),
    Keep(ClassicReason),
}

/// Where a pool's create seeds work, as it opened ([`Prune`]).
enum SeedRoot {
    /// The one mount namespace the node pruned, which each seed joins.
    Pruned(OwnedFd),
    /// The agent's own, kept for this reason.
    Kept(ClassicReason),
}

impl SeedRoot {
    /// The pruned namespace a new seed joins, if the seeds prune.
    fn pruned(&self) -> Option<BorrowedFd<'_>> {
        match self {
            Self::Pruned(namespace) => Some(namespace.as_fd()),
            Self::Kept(_) => None,
        }
    }

    /// Why the seeds keep the agent's mount namespace, if they do.
    fn kept(&self) -> Option<ClassicReason> {
        match self {
            Self::Pruned(_) => None,
            Self::Kept(reason) => Some(*reason),
        }
    }
}

/// The node agent's create seeds. Dropped, it stops its starting thread and ends and reaps every seed it
/// started.
pub struct CreatePool {
    shared: Arc<Shared>,
    starter: Option<JoinHandle<()>>,
}

struct Shared {
    /// The most seeds a create is tried on before it is refused: the pool's bound, each try past the first
    /// having retired a seed taken for hung.
    retries: usize,
    /// The `hex` binary re-exec'd as `hex __oci-seed`.
    exe: PathBuf,
    /// Where the create seeds work (goal 5 plan step 2): the node's one pruned mount namespace, made as the pool
    /// opens and joined by each seed ([`rt_seed::ENV_JOIN`]), so the node pivots once, not once per seed
    /// (`goal6-mntns.md` §12); or the agent's own, kept, and why, whose creates take the classic root path in the
    /// init.
    root: SeedRoot,
    /// A debug-only crash point the node arms on each seed it spawns (its [`rt_seed::ENV_INJECT`]), for
    /// the crash-safety test of the pruned init's root build; `None` unless a test arms it, and absent
    /// from release builds (CLAUDE.md §6).
    #[cfg(debug_assertions)]
    inject: Option<String>,
    /// The monotonic point the core's durations are measured from.
    base: Instant,
    inner: Mutex<Inner>,
    /// A create in line waits on it; a seed handed to one, a refusal of one, and shutdown wake it.
    free: Condvar,
    /// The starting thread waits on it; a create that leaves it a seed to start, a seed's death or hang,
    /// and shutdown wake it.
    wake: Condvar,
    /// Creates on the classic root path, counted by reason ([`ClassicReason`], a fixed set, so no
    /// high-cardinality label): through the shim when a pruned seed couldn't serve them or they join a
    /// pid or user namespace by path, and by a seed with the classic root on a rootless node. Each stops
    /// at `u64::MAX`, as the core's counts do.
    classic: [AtomicU64; ClassicReason::COUNT],
    /// How this node makes a pruned create's mount namespace (decision 6's update), chosen as the pool
    /// opens: the copy where the seeds keep the agent's mount namespace.
    mount_namespace: MountNamespace,
    /// Creates a pruned seed made on the pruned root path (goal 5 plan step 2), with no `pivot_root`, by
    /// the way their mount namespace was made as the seed answered ([`MountNamespace::code`]'s index).
    /// Zero on a rootless node, whose seeds don't prune. Each stops at `u64::MAX`.
    made: [AtomicU64; 3],
    /// Claims of a parked init that failed, by why: it didn't answer within the hang bound, or it refused or broke.
    /// Each falls back to a seed's clone (the runtime's request, which the claim only borrowed, goes to
    /// [`CloneInit::clone_init`]), so these say how often a parked init cost its create a clone. Each stops at
    /// `u64::MAX`.
    claims_failed: [AtomicU64; 2],
    /// The wakes sent the starting thread and the line, for the tests.
    #[cfg(test)]
    woke: [AtomicU64; 2],
}

struct Inner {
    seeds: Seeds,
    /// Each slot's live seed process, `Some` while the seed is up and not checked out for a create.
    procs: Vec<Option<Proc>>,
    /// Why the last seed start failed, while none has started since: a create the core refuses names it,
    /// so a node whose seeds can't start says why (CLAUDE.md §3), not only that none came free.
    start_failure: Option<String>,
    /// Set as the pool is dropped: the starting thread ends.
    shut: bool,
    /// The seeds kept up and idle beyond the creates waiting (Ada's rule, 2026-11-06): one at first, the fewest
    /// that lets a create find a seed idle rather than wait on a seed's exec, raised to the most creates seen
    /// waiting at once, so a burst that waited once finds as many idle the next time; never lowered, at most the
    /// seeds' bound. Each spare holds a seed's memory, 2,488 KiB at most (`seed_path.md`), within the node's
    /// share for seeds ([`crate::sizes`]).
    spare: Learned,
}

/// One seed process: the agent's end of its socket and the pidfd by which the agent ends and reaps it.
struct Proc {
    socket: OwnedFd,
    pidfd: OwnedFd,
}

impl CreatePool {
    /// A pool that re-execs `exe` as `hex __oci-seed`, with the node's bounds on its host (`sizes`: at most its
    /// `seeds`, slots growing from its `start`, [`Seeds::growing`], and a line of at most its `workers`, the most
    /// calls its socket runs at once, so no create over the socket is refused for want of room in line), its
    /// starting thread up and starting the first spare. `prune` is where its seeds work ([`Prune`]): the mountpoint
    /// the node prunes their one mount namespace at, once, as the pool opens (a root node, goal 5 plan step 2,
    /// `goal6-mntns.md` §12), or the agent's own namespace, kept, and why. `asked`, when given, is the
    /// way a pruned create's mount namespace is made (`copy`, `empty` or `tree`)
    /// (decision 6's update); a way this node can't take is refused by type.
    pub fn new(
        exe: PathBuf,
        sizes: crate::sizes::Sizes,
        prune: Prune,
        asked: Option<&str>,
        #[cfg(debug_assertions)] inject: Option<String>,
    ) -> Result<Self, Refusal> {
        let limits = Limits {
            seeds: sizes.seeds,
            waiting: sizes.workers,
        };
        Self::open(
            exe,
            Seeds::growing(limits, sizes.starts.seeds),
            limits,
            sizes.starts.spare,
            prune,
            asked,
            #[cfg(debug_assertions)]
            inject,
        )
    }

    /// A pool with `limits`, every slot from the start, its waits seeded from the seeds' own record for this build, as
    /// a node's are (decision 9, by build profile): a test's wait for a seed takes the record of the seeds' answers,
    /// never another fact's.
    #[cfg(test)]
    pub fn with_limits(
        exe: PathBuf,
        limits: Limits,
        prune: Prune,
        asked: Option<&str>,
        #[cfg(debug_assertions)] inject: Option<String>,
    ) -> Result<Self, Refusal> {
        Self::open(
            exe,
            Seeds::new(limits),
            limits,
            1,
            prune,
            asked,
            #[cfg(debug_assertions)]
            inject,
        )
    }

    fn open(
        exe: PathBuf,
        seeds: Seeds,
        limits: Limits,
        spare: usize,
        prune: Prune,
        asked: Option<&str>,
        #[cfg(debug_assertions)] inject: Option<String>,
    ) -> Result<Self, Refusal> {
        // Only a pruning node's creates have a mount namespace to make another way; the probe makes
        // nothing and takes no lock (`goal6-mntns.md` §6).
        let mount_namespace = match &prune {
            Prune::At(_) => mntns::choose(mntns::probe()?, asked)?,
            Prune::Keep(_) => MountNamespace::Copy,
        };
        let root = match prune {
            Prune::At(mountpoint) => SeedRoot::Pruned(prune_once(mountpoint)?),
            Prune::Keep(reason) => SeedRoot::Kept(reason),
        };
        let slots = limits.seeds.get();
        let shared = Arc::new(Shared {
            retries: slots,
            exe,
            root,
            #[cfg(debug_assertions)]
            inject,
            base: Instant::now(),
            inner: Mutex::new(Inner {
                seeds,
                procs: (0..slots).map(|_| None).collect(),
                start_failure: None,
                shut: false,
                spare: Learned::new(spare, slots),
            }),
            free: Condvar::new(),
            wake: Condvar::new(),
            classic: std::array::from_fn(|_| AtomicU64::new(0)),
            mount_namespace,
            made: std::array::from_fn(|_| AtomicU64::new(0)),
            claims_failed: std::array::from_fn(|_| AtomicU64::new(0)),
            #[cfg(test)]
            woke: [AtomicU64::new(0), AtomicU64::new(0)],
        });
        let starting = Arc::clone(&shared);
        let starter = std::thread::Builder::new()
            .name("hex-seeds".to_owned())
            .spawn(move || start_seeds(&starting))
            .map_err(|e| Refusal::os("starting the create seeds' thread", e))?;
        Ok(Self {
            shared,
            starter: Some(starter),
        })
    }

    /// How many creates were made through the shim since the pool opened: those a pruned seed couldn't
    /// serve, and those that join a pid or user namespace by path. Every reason but [`ClassicReason::Rootless`] and
    /// [`ClassicReason::Baseline`], whose creates a seed makes with the classic root.
    pub fn creates_through_shim(&self) -> u64 {
        self.shared
            .classic
            .iter()
            .enumerate()
            .filter(|&(i, _)| {
                i != ClassicReason::Rootless.index() && i != ClassicReason::Baseline.index()
            })
            .map(|(_, c)| c.load(Ordering::Relaxed))
            .fold(0u64, u64::saturating_add)
    }

    /// How many creates took the classic root path for `reason` since the pool opened.
    pub fn classic_creates_for(&self, reason: ClassicReason) -> u64 {
        self.shared
            .classic
            .get(reason.index())
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// How many creates a pruned seed made on the pruned root path since the pool opened (goal 5 plan
    /// step 2): zero on a rootless node, positive on a root node once it has made a pruned-eligible box.
    pub fn pruned_creates(&self) -> u64 {
        self.shared
            .made
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .fold(0u64, u64::saturating_add)
    }

    /// How many pruned creates had their mount namespace made `way` since the pool opened, as their
    /// seeds answered (decision 6's update).
    pub fn mount_namespaces_made(&self, way: MountNamespace) -> u64 {
        self.shared
            .made
            .get(usize::from(way.code()))
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// What the pool has done, as the bench and the node's metrics read it.
    /// What it has learned (Ada's rule, 2026-11-06): its slots, the most creates it has seen at once, and its spare.
    pub fn learned(&self) -> Result<(usize, usize), Refusal> {
        let inner = self.shared.lock().map_err(Refusal::from)?;
        Ok((inner.seeds.slots(), inner.spare.size()))
    }

    /// Claims of a parked init that failed and fell back to a seed's clone, by why: (hung, refused or broken).
    pub fn claims_failed(&self) -> (u64, u64) {
        let at = |i: usize| {
            self.shared
                .claims_failed
                .get(i)
                .map_or(0, |c| c.load(Ordering::Relaxed))
        };
        (at(0), at(1))
    }

    pub fn counts(&self) -> Result<Counts, Refusal> {
        Ok(self.shared.lock().map_err(Refusal::from)?.seeds.counts())
    }
}

impl CloneInit for CreatePool {
    fn unpruned(&self) -> Option<ClassicReason> {
        self.shared.root.kept()
    }

    fn mount_namespace(&self) -> MountNamespace {
        self.shared.mount_namespace
    }

    fn classic(&self, reason: ClassicReason) {
        if let Some(count) = self.shared.classic.get(reason.index()) {
            let _ = count.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        }
    }

    fn clone_init(&self, req: CloneReq<'_>) -> Result<(i32, OwnedFd), OciError> {
        let shared = &self.shared;
        let pruned = req.pruned.is_some();
        let (create, fds) = message(&req);
        // A seed past its hang bound is retired, counted, and the create goes to another seed (the lead's rule of
        // 2026-11-06: no caller failure where an alternative exists), at most once a slot.
        let mut tries = 0usize;
        loop {
            tries = tries.saturating_add(1);
            match self.try_seed(&create, &fds, pruned) {
                Err(Trip::Hung(e)) => {
                    // The hung seed is killed and reaped, so its clone either made the init in the box's cgroup or
                    // never will: with the cgroup empty nothing of this create was made, and another seed may make
                    // it; otherwise it is refused, by type.
                    if tries >= shared.retries || req.cgroup.populated().unwrap_or(true) {
                        return Err(e);
                    }
                }
                Err(Trip::Refused(e) | Trip::Broken(e)) => return Err(e),
                Ok(started) => return Ok(started),
            }
        }
    }

    fn claim(
        &self,
        parked: Parked,
        req: &CloneReq<'_>,
        sync: BorrowedFd<'_>,
    ) -> Result<(i32, OwnedFd), OciError> {
        self.claim_parked(parked, req, sync)
    }
}

impl CreatePool {
    /// One try of a create on a seed the pool hands it: the init's pid and pidfd, or why not, the seed given
    /// back, ended or retired as its answer says.
    fn try_seed(
        &self,
        create: &Create,
        fds: &[BorrowedFd<'_>],
        pruned: bool,
    ) -> Result<(i32, OwnedFd), Trip> {
        let shared = &self.shared;
        let (id, proc, patience) = shared.acquire().map_err(Trip::Refused)?;
        match round_trip(proc.socket.as_fd(), create, fds, patience) {
            Ok((pid, pidfd, made)) => {
                if pruned && let Some(count) = shared.made.get(usize::from(made.code())) {
                    let _ = count.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_add(1))
                    });
                }
                let started = (pid, pidfd);
                shared.done(id, proc);
                Ok(started)
            }
            // The seed refused by type (a bad request, or a container it can't make) and is whole: back
            // to the pool, the refusal to the caller.
            Err(Trip::Refused(e)) => {
                shared.done(id, proc);
                Err(Trip::Refused(e))
            }
            // The seed is unusable (its socket broke, or it says its own machinery failed): end it, free
            // its slot for a restart, refuse this create.
            Err(Trip::Broken(e)) => {
                shared.died(id, proc);
                Err(Trip::Broken(e))
            }
            // It didn't answer within the hang bound: retired, taken for hung and counted (no sample).
            Err(Trip::Hung(e)) => {
                shared.hung(id, proc);
                Err(Trip::Hung(e))
            }
        }
    }

    fn claim_parked(
        &self,
        parked: Parked,
        req: &CloneReq<'_>,
        sync: BorrowedFd<'_>,
    ) -> Result<(i32, OwnedFd), OciError> {
        let pruned = req.pruned.is_some();
        let Some((pid, pidfd, socket)) = parked.take() else {
            return Err(OciError::Broken(
                "a parked init was claimed twice".to_owned(),
            ));
        };
        let (create, fds) = message(req);
        // The claim is taken at the init's first word on its sync pipe, with no answer before it (the lead's (A) of
        // 2026-11-09), within the claim's own patience: its pid and pidfd are the park's.
        let began = Instant::now();
        match claim_round(socket.as_fd(), sync, &create, &fds, CLAIMS.deadline()) {
            Ok(made) => {
                CLAIMS.answered(began.elapsed());
                // Counted by how its mount namespace was made, as a seed's clone is (clone_init).
                if pruned && let Some(count) = self.shared.made.get(usize::from(made.code())) {
                    let _ = count.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_add(1))
                    });
                }
                Ok((pid, pidfd))
            }
            Err(trip) => {
                self.shared.claim_failed(&trip);
                let (Trip::Refused(e) | Trip::Broken(e) | Trip::Hung(e)) = trip;
                // Refused, the init ends itself; ended either way, and reaped, so none is left.
                drop(socket);
                let _ = rustix::process::pidfd_send_signal(&pidfd, Signal::KILL);
                let _ =
                    rustix::process::waitid(WaitId::PidFd(pidfd.as_fd()), WaitIdOptions::EXITED);
                Err(e)
            }
        }
    }
}

impl CreatePool {
    /// A handle on the pool's seeds that parks inits ahead from another thread (decision 8, "The p99.9 that
    /// stayed").
    pub fn parker(&self) -> Parker {
        Parker(Arc::clone(&self.shared))
    }
}

/// A handle on a create pool's seeds that makes inits ahead ([`Parker::park`]).
#[derive(Clone)]
pub struct Parker(Arc<Shared>);

impl Parker {
    /// Has a seed clone an init into `cgroup` with the namespaces `flags`, where it waits to be claimed
    /// (decision 8, "The p99.9 that stayed"). The clone, and its wait for the host-wide `cgroup_mutex`, are
    /// here, off every create's path.
    pub fn park(&self, cgroup: BorrowedFd<'_>, flags: u64) -> Result<Parked, OciError> {
        let shared = &self.0;
        let (id, proc, patience) = shared.acquire()?;
        let (ours, theirs) = rt_seed::socketpair()?;
        let sent = rt_seed::send_park(
            proc.socket.as_fd(),
            &Park { flags },
            [cgroup, theirs.as_fd()],
        );
        drop(theirs);
        let answered = match sent.and_then(|()| readable(proc.socket.as_fd(), patience)) {
            Ok(true) => rt_seed::recv_answer(proc.socket.as_fd()),
            // No answer within the hang bound: taken for hung, which backs the bound off.
            Ok(false) => {
                shared.hung(id, proc);
                return Err(OciError::Broken(format!(
                    "a create seed did not answer a park within {:?}",
                    patience.unwrap_or_default()
                )));
            }
            Err(e) => Err(e),
        };
        match answered {
            Ok((Answer::Started { pid }, Some(pidfd))) => {
                shared.done(id, proc);
                let pid = i32::try_from(pid)
                    .map_err(|_| OciError::Broken("a parked init's pid out of range".to_owned()))?;
                // Held from here, so ended and reaped if it never says it is parked, which it does within
                // the hang bound the seeds' answers taught.
                let parked = Parked::new(pid, pidfd, ours, flags);
                // The hang bound, seeded from the record, is armed; were it measuring with nothing seen, the
                // wait would be unbounded.
                let patience = shared.lock()?.seeds.deadlines().1;
                parked.wait_parked(patience.unwrap_or(Duration::MAX))?;
                Ok(parked)
            }
            Ok((Answer::Refused { failed, why }, _)) => {
                if failed == Failed::Seed {
                    shared.died(id, proc);
                } else {
                    shared.done(id, proc);
                }
                Err(OciError::Os {
                    op: "parking an init",
                    why,
                })
            }
            Ok((Answer::Started { .. }, None)) => {
                shared.died(id, proc);
                Err(OciError::Broken(
                    "a create seed said an init was parked but sent no pidfd".to_owned(),
                ))
            }
            // A create's answer (decision 6's update), which no park is given.
            Ok((Answer::StartedIn { .. }, _)) => {
                shared.died(id, proc);
                Err(OciError::Broken(
                    "a create seed answered a park as a create".to_owned(),
                ))
            }
            Err(e) => {
                shared.died(id, proc);
                Err(e)
            }
        }
    }
}

/// A create as the seed's message and its descriptors, in the order the seed reads them (`serve_create`):
/// the sync pipe, the map pipe on the classic path, a network namespace, then on the pruned path the fixed
/// descriptors of a lean create (the cgroup directory where it rides, for a seed's clone; the state directory and the
/// rootfs) and the bind trees, in `mount::plan`'s order (the order the init attaches them), and last the spec the
/// runtime checked. A pruned create carries only what its init reads ([`Fixed::Lean`]; the lead's (B) of 2026-11-09).
fn message<'r>(req: &'r CloneReq<'_>) -> (Create, Vec<BorrowedFd<'r>>) {
    let pruned = req.pruned.is_some();
    let create = Create {
        bundle: req.bundle.to_string_lossy().into_owned(),
        dir: req.dir.to_string_lossy().into_owned(),
        cgroup: Some(req.cgroup.dir().to_string_lossy().into_owned()),
        network: req.network.is_some(),
        pruned,
        binds: req
            .pruned
            .as_ref()
            .map_or(0, |p| u32::try_from(p.binds.len()).unwrap_or(u32::MAX)),
        dev_mount: req
            .pruned
            .as_ref()
            .and_then(|p| p.dev_mount.as_ref().map(|d| d.mount)),
        path_joins: req
            .pruned
            .as_ref()
            .map_or(0, |p| u32::try_from(p.path_joins.len()).unwrap_or(u32::MAX)),
        ipc_uts: req.pruned.as_ref().is_some_and(|p| p.ipc_uts.is_some()),
        config: true,
        mount_namespace: req
            .pruned
            .as_ref()
            .map_or(MountNamespace::Copy, |p| p.mount_namespace),
        proc_mount: req
            .pruned
            .as_ref()
            .and_then(|p| p.proc_mount.as_ref().map(|(mount, _)| *mount)),
        masked_directory: req
            .pruned
            .as_ref()
            .is_some_and(|p| p.masked_directory.is_some()),
        fixed: req.pruned.as_ref().map_or(Fixed::All, |p| Fixed::Lean {
            cgroup: p.cgroup_dir.is_some(),
        }),
    };
    let mut fds: Vec<BorrowedFd<'_>> = vec![req.sync_w.as_fd()];
    fds.extend(req.map_r.as_ref().map(AsFd::as_fd));
    if let Some(network) = &req.network {
        fds.push(network.as_fd());
    }
    if let Some(pruned) = &req.pruned {
        fds.extend(pruned.cgroup_dir.as_ref().map(AsFd::as_fd));
        fds.push(pruned.container_dir.as_fd());
        fds.push(pruned.rootfs.as_fd());
        fds.extend(pruned.binds.iter().map(AsFd::as_fd));
        fds.extend(pruned.path_joins.iter().map(AsFd::as_fd));
        if let Some(dev) = &pruned.dev_mount {
            fds.push(dev.tree.as_fd());
        }
        if let Some(pair) = &pruned.ipc_uts {
            fds.push(pair.ipc.as_fd());
            fds.push(pair.uts.as_fd());
        }
        if let Some((_, proc)) = &pruned.proc_mount {
            fds.push(proc.pid_namespace.as_fd());
            fds.push(proc.tree.as_fd());
        }
        if let Some(empty) = &pruned.masked_directory {
            fds.push(empty.as_fd());
        }
    }
    // Last, the spec the runtime checked, which the seed makes the container from (AC-T7.1.1).
    fds.push(req.config);
    (create, fds)
}

impl Drop for CreatePool {
    fn drop(&mut self) {
        // A poisoned lock still holds the seeds' processes: they are ended either way, none left running
        // or unreaped.
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.shut = true;
        for proc in inner.procs.iter_mut().filter_map(Option::take) {
            end(&proc);
        }
        drop(inner);
        self.shared.wake.notify_all();
        self.shared.free.notify_all();
        if let Some(starter) = self.starter.take() {
            let _ = starter.join();
        }
    }
}

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, Inner>, OciError> {
        self.inner
            .lock()
            .map_err(|_| OciError::Broken("the create seed pool's lock was poisoned".to_owned()))
    }

    fn now(&self) -> Duration {
        self.base.elapsed()
    }

    /// A parked init's claim failed with `trip`: counted by its kind.
    fn claim_failed(&self, trip: &Trip) {
        let i = match trip {
            Trip::Hung(_) => 0,
            Trip::Refused(_) | Trip::Broken(_) => 1,
        };
        if let Some(count) = self.claims_failed.get(i) {
            let _ = count.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
        }
    }

    /// Wakes the starting thread, for a seed to start.
    fn wake_starter(&self) {
        #[cfg(test)]
        self.woke[0].fetch_add(1, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// Wakes the creates in line, each to claim by its ticket: a seed was handed to one or one refused.
    fn wake_line(&self) {
        #[cfg(test)]
        self.woke[1].fetch_add(1, Ordering::Relaxed);
        self.free.notify_all();
    }

    /// An idle seed for a create: handed one at once, or, with none idle, a wait in line for one, within
    /// the core's bounds. The starting thread is woken when the create leaves it a seed to start, to keep
    /// its spare, and not otherwise: a wake is a system call, and the woken thread takes the lock this
    /// create holds. Returns the seed and its checked-out process (its slot's `procs` entry taken until
    /// [`done`](Self::done) or [`died`](Self::died)), and how long the shell waits for its answer: until the
    /// core takes it for hung, or `None` while the hang bound measures with nothing seen.
    fn acquire(&self) -> Result<(SeedId, Proc, Option<Duration>), OciError> {
        let mut inner = self.lock()?;
        // A seed that has served a create for the hang bound is taken for hung here, killed and its slot
        // freed for a restart, so a wedged seed never holds a slot for ever.
        self.reap_hung(&mut inner);
        let now = self.now();
        let slots_before = inner.seeds.slots();
        let admit = inner
            .seeds
            .create(now)
            .map_err(|r| refused(r, inner.start_failure.as_deref(), inner.procs.len()))?;
        if matches!(admit, Admit::Wait { .. }) {
            let waiting = inner.seeds.waiting();
            // A slot added, or the spare raised, is a peak the node keeps for its next start.
            if inner.spare.observe(waiting) || inner.seeds.slots() > slots_before {
                crate::sizes::rose();
            }
        }
        if wants_start(&inner) {
            self.wake_starter();
        }
        let ticket = match admit {
            Admit::Seed(id) => return self.checked_out(&mut inner, id),
            Admit::Wait { ticket, .. } => ticket,
        };
        loop {
            let now = self.now();
            let until = match inner.seeds.claim(ticket, now).map_err(stale)? {
                Claim::Seed(id) => return self.checked_out(&mut inner, id),
                Claim::Refused(r) => {
                    return Err(refused(
                        r,
                        inner.start_failure.as_deref(),
                        inner.procs.len(),
                    ));
                }
                Claim::Wait { until } => until,
            };
            fn poisoned<T>(_: T) -> OciError {
                OciError::Broken("the create seed pool's lock was poisoned".to_owned())
            }
            // A measuring line's wait, with nothing seen, lasts until a seed is handed to it.
            inner = match until {
                Some(until) => {
                    let left = until.saturating_sub(self.now());
                    self.free.wait_timeout(inner, left).map_err(poisoned)?.0
                }
                None => self.free.wait(inner).map_err(poisoned)?,
            };
        }
    }

    /// Seed `id`'s process checked out for a create, with what is left of its hang bound.
    fn checked_out(
        &self,
        inner: &mut Inner,
        id: SeedId,
    ) -> Result<(SeedId, Proc, Option<Duration>), OciError> {
        let until = inner.seeds.until(id).map_err(stale)?;
        let now = self.now();
        Ok((id, take(inner, id)?, until.map(|u| u.saturating_sub(now))))
    }

    /// Takes each seed past the hang bound over its create, killing and reaping it and freeing its slot;
    /// the core refuses that create and asks for a restart. A waiting create handed the seed meets the
    /// refusal at its claim, so the line and the starting thread are woken.
    fn reap_hung(&self, inner: &mut Inner) {
        let now = self.now();
        let mut woke = false;
        while let Some((id, _refused)) = inner.seeds.hung(now) {
            if let Some(proc) = inner.procs.get_mut(id.slot()).and_then(Option::take) {
                end(&proc);
            }
            woke = true;
        }
        if woke {
            self.wake_line();
            self.wake_starter();
        }
    }

    /// A healthy seed back to the pool after its create: its process returned to its slot, the core told
    /// it is done, and the create that has waited longest, if any, woken.
    fn done(&self, id: SeedId, proc: Proc) {
        // A pool whose lock is poisoned takes nothing back: the seed is ended and reaped, not dropped.
        let Ok(mut inner) = self.lock() else {
            end(&proc);
            return;
        };
        match inner.procs.get_mut(id.slot()) {
            Some(slot) => *slot = Some(proc),
            None => end(&proc),
        }
        let now = self.now();
        // The line is woken for the create the seed went to, and not when none waits. Stale only if the
        // seed left meanwhile (it can't: this create held it); the line is woken then too.
        if !matches!(inner.seeds.done(id, now), Ok(None)) {
            self.wake_line();
        }
    }

    /// A seed that didn't answer within its hang bound: killed and reaped, and taken for hung by the core,
    /// which refuses its create, backs the bound off and asks for a restart. Were the core's clock short of
    /// the bound (the shell's wait began after the core's), it is reported dead instead.
    fn hung(&self, id: SeedId, proc: Proc) {
        end(&proc);
        if let Ok(mut inner) = self.lock() {
            self.reap_hung(&mut inner);
            if inner.seeds.died(id).is_ok() {
                self.wake_line();
                self.wake_starter();
            }
        }
    }

    /// An unusable seed ended and its slot freed: killed and reaped, the core told it died (which counts
    /// it and asks for a restart), the waiters woken, and the starting thread woken to replace it.
    fn died(&self, id: SeedId, proc: Proc) {
        end(&proc);
        if let Ok(mut inner) = self.lock() {
            let _ = inner.seeds.died(id);
            self.wake_line();
            self.wake_starter();
        }
    }
}

/// Whether the starting thread should start a seed: the core asks for one (a vacant slot, its asking not
/// stopped by a failed start), and fewer than the spare ([`Inner::spare`]) beyond the creates waiting are up idle or
/// starting.
fn wants_start(inner: &Inner) -> bool {
    inner.seeds.wanted() > 0
        && inner.seeds.idle().saturating_add(inner.seeds.starting())
            < inner.spare.size().saturating_add(inner.seeds.waiting())
}

/// The starting thread: starts each seed [`wants_start`] asks for, one at a time and with the lock let go,
/// so no create's path holds a seed's exec and no create waits on the lock meanwhile. A started seed goes
/// to the create that has waited longest, which is woken. A failed start stops the core's asking until the
/// next create, so a node whose seeds can't start tries each slot once a create, not in a loop. It ends at
/// shutdown, ending a seed it started meanwhile, or on a poisoned lock, whereupon every create refuses on
/// the same lock.
fn start_seeds(shared: &Shared) {
    loop {
        let Ok(mut inner) = shared.inner.lock() else {
            return;
        };
        while !inner.shut && !wants_start(&inner) {
            inner = match shared.wake.wait(inner) {
                Ok(inner) => inner,
                Err(_) => return,
            };
        }
        if inner.shut {
            return;
        }
        let Some(id) = inner.seeds.start() else {
            // `wants_start` saw room; were the core to give none, wait for the next change rather than
            // ask again at once.
            drop(match shared.wake.wait(inner) {
                Ok(inner) => inner,
                Err(_) => return,
            });
            continue;
        };
        drop(inner);
        let spawned = spawn(
            &shared.exe,
            shared.root.pruned(),
            #[cfg(debug_assertions)]
            shared.inject.as_deref(),
        );
        let Ok(mut inner) = shared.inner.lock() else {
            if let Ok(proc) = &spawned {
                end(proc);
            }
            return;
        };
        match spawned {
            Ok(proc) if inner.shut => {
                end(&proc);
                return;
            }
            Ok(proc) => match inner.seeds.started(id, shared.now()) {
                Ok(handed) => {
                    inner.start_failure = None;
                    match inner.procs.get_mut(id.slot()) {
                        Some(slot) => *slot = Some(proc),
                        None => end(&proc),
                    }
                    // The create it went to, if any, is woken; one it didn't go to waits on.
                    if handed.is_some() {
                        shared.wake_line();
                    }
                }
                // The core isn't starting it (it was reported dead meanwhile): end it.
                Err(_stale) => end(&proc),
            },
            // Counted by the core, which stops asking until the next create; the cause is kept for the
            // refusals of the creates that find no seed meanwhile, each at its wait's end.
            Err(e) => {
                inner.start_failure = Some(e.to_string());
                // With no seed up or starting, the core refuses the creates in line: woken to say why.
                if matches!(inner.seeds.start_failed(id, shared.now()), Ok(true)) {
                    shared.wake_line();
                }
            }
        }
    }
}

/// Spawns a seed: a `hex __oci-seed` child of the agent, holding one end of a fresh socket; the agent
/// keeps the other and the child's pidfd, by which it ends and reaps it. On a root node `pruned` is the
/// node's pruned mount namespace, sent as the seed's first record for it to join (goal 5 plan step 2).
fn spawn(
    exe: &std::path::Path,
    pruned: Option<BorrowedFd<'_>>,
    #[cfg(debug_assertions)] inject: Option<&str>,
) -> Result<Proc, OciError> {
    let (ours, theirs) = rt_seed::socketpair()?;
    // The seed's end rides in as its stdin, which the spawn duplicates into the child alone: it stays
    // close-on-exec here, so no process another thread of the agent starts meanwhile inherits it, as one
    // would in the window a cleared flag leaves (CVE-2024-21626's class). The seed keeps only its socket;
    // every other inherited descriptor closes on its re-exec.
    let mut cmd = Command::new(exe);
    cmd.arg("__oci-seed")
        .stdin(std::process::Stdio::from(theirs))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if pruned.is_some() {
        cmd.env(rt_seed::ENV_JOIN, "1");
    }
    // The seed's init looks for a crash point only when a test arms one on the seed; otherwise it is
    // cleared from the inherited environment, so nothing but this node arms it (CLAUDE.md §6). Debug only.
    #[cfg(debug_assertions)]
    match inject {
        Some(point) => {
            cmd.env(rt_seed::ENV_INJECT, point);
        }
        None => {
            cmd.env_remove(rt_seed::ENV_INJECT);
        }
    }
    let child = cmd
        .spawn()
        .map_err(|e| OciError::os("starting a create seed", e))?;
    let pid = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| OciError::os("starting a create seed", "the seed's pid isn't a pid"))?;
    let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
        .map_err(|e| OciError::os("opening a create seed's pidfd", e))?;
    let proc = Proc {
        socket: ours,
        pidfd,
    };
    if let Some(namespace) = pruned
        && let Err(e) = rt_seed::send_namespace(proc.socket.as_fd(), namespace)
    {
        end(&proc);
        return Err(e);
    }
    Ok(proc)
}

/// The node's one pruned mount namespace (`goal6-mntns.md` §12), made at `mountpoint` by a thread of its
/// own, which the prune leaves in that namespace and which then ends, so no other thread of the agent's
/// changes.
fn prune_once(mountpoint: PathBuf) -> Result<OwnedFd, Refusal> {
    let pruning = std::thread::Builder::new()
        .name("hex-prune".to_owned())
        .spawn(move || rt_seed::prune_namespace(&mountpoint))
        .map_err(|e| Refusal::os("starting the pruning thread", e))?;
    match pruning.join() {
        Ok(made) => Ok(made?),
        Err(_) => Err(Refusal::os(
            "pruning the create seeds' mount namespace",
            "the pruning thread ended without an answer",
        )),
    }
}

/// The seed's process checked out of its slot for a create (the core has marked it busy, so no other
/// create takes it).
fn take(inner: &mut Inner, id: SeedId) -> Result<Proc, OciError> {
    inner
        .procs
        .get_mut(id.slot())
        .and_then(Option::take)
        .ok_or_else(|| OciError::Broken(format!("create seed {id} has no process")))
}

/// Ends a seed (KILL by its pidfd) and reaps it, so none the agent started is left running or a zombie.
/// A seed that already exited takes a harmless failed signal.
fn end(proc: &Proc) {
    let _ = rustix::process::pidfd_send_signal(proc.pidfd.as_fd(), Signal::KILL);
    let _ = rustix::process::waitid(WaitId::PidFd(proc.pidfd.as_fd()), WaitIdOptions::EXITED);
}

/// Why a round trip with a seed didn't make the container.
enum Trip {
    /// The seed refused by type and is still usable.
    Refused(OciError),
    /// The seed is unusable (its socket broke, or it says its own machinery failed): replace it.
    Broken(OciError),
    /// The seed didn't answer within its hang bound: take it for hung and replace it.
    Hung(OciError),
}

/// Sends the create and its descriptors to the seed and reads the answer: the init's pid and pidfd, or a
/// refusal. The wait for the answer is bounded by `patience`, what is left of the hang bound the core
/// learned, so a wedged seed is taken for hung rather than waited on for ever (CLAUDE.md §2, §4); `None`
/// only while the hang bound measures with nothing seen.
fn round_trip(
    socket: BorrowedFd<'_>,
    create: &Create,
    fds: &[BorrowedFd<'_>],
    patience: Option<Duration>,
) -> Result<(i32, OwnedFd, MountNamespace), Trip> {
    rt_seed::send_create(socket, create, fds).map_err(Trip::Broken)?;
    if !readable(socket, patience).map_err(Trip::Broken)? {
        return Err(Trip::Hung(OciError::Broken(format!(
            "a create seed did not answer within {:?}",
            patience.unwrap_or_default()
        ))));
    }
    let (answer, pidfd) = rt_seed::recv_answer(socket).map_err(Trip::Broken)?;
    let (pid, made) = match answer {
        // A classic create's answer names no way: its namespace, if any, is the seed's copied.
        Answer::Started { pid } => (pid, MountNamespace::Copy),
        Answer::StartedIn {
            pid,
            mount_namespace,
        } => (pid, mount_namespace),
        Answer::Refused { failed, why } => return Err(refusal(failed, why)),
    };
    let pid = i32::try_from(pid).map_err(|_| {
        Trip::Broken(OciError::Broken(
            "a create seed sent a pid out of range".to_owned(),
        ))
    })?;
    match pidfd {
        Some(pidfd) => Ok((pid, pidfd, made)),
        None => Err(Trip::Broken(OciError::Broken(
            "a create seed said an init started but sent no pidfd".to_owned(),
        ))),
    }
}

/// A claim's wait for its parked init's first word on its sync pipe, from the claim's send (the lead's (A) of
/// 2026-11-09: no early answer): the init's ready on a root node's pruned path, its ask for its user namespace's maps
/// on a rootless node's classic one. Seeded by build profile from the `parked.ready` rows of
/// `bench/competitive/probes/waits_census.md` run 5 (decision 9): release, the largest max of the release arms,
/// 12.896 ms (adacachy2, its tmpfs arm, of 29,007); debug, the largest answer of the debug-build arms, the debug arm and
/// the suites', 652.431 ms (the suites', of 190,580). An init that ends or refuses is caught at once, its claim socket
/// closed or carrying the refusal; one that says nothing within the claim's patience falls back to a seed's clone.
static CLAIMS: hex_core::patience::Patience = hex_core::patience::Patience::by_profile(
    "parked.ready",
    std::time::Duration::from_nanos(12_896_493),
    Some(std::time::Duration::from_nanos(652_430_750)),
);

/// Sends a parked init its claim and waits, within `patience` (`None` while the claim's wait measures: unbounded), for
/// the claim to be taken or not: how the init makes its mount namespace, or why not. A claimed init says nothing on its
/// claim socket once it takes the claim (the lead's (A) of 2026-11-09), so the claim is taken at the init's first word
/// on `sync`, the read end of its sync pipe, which stays there for the handshake to read, and the init makes its mount
/// namespace the create's way, the one it was parked for (the seed's `begin` refuses another). A word there is taken
/// before anything on the socket, since an init that reports a setup failure ends after it. Not taken: its refusal on
/// the socket, its end (the socket closed with no word on the pipe), or its silence past `patience`.
fn claim_round(
    socket: BorrowedFd<'_>,
    sync: BorrowedFd<'_>,
    create: &Create,
    fds: &[BorrowedFd<'_>],
    patience: Option<Duration>,
) -> Result<MountNamespace, Trip> {
    rt_seed::send_create(socket, create, fds).map_err(Trip::Broken)?;
    match first_readable([sync, socket], patience).map_err(Trip::Broken)? {
        Some(0) => Ok(create.mount_namespace),
        Some(_) => match rt_seed::recv_answer(socket) {
            Ok((Answer::Refused { failed, why }, _)) => Err(refusal(failed, why)),
            Ok(_) => Err(Trip::Broken(OciError::Broken(
                "a claimed init answered other than to refuse its claim".to_owned(),
            ))),
            Err(e) => Err(Trip::Broken(e)),
        },
        None => Err(Trip::Hung(OciError::Broken(format!(
            "a claimed init said nothing within {:?}",
            patience.unwrap_or_default()
        )))),
    }
}

/// Which of `fds` turns readable first within `patience`, the earlier in the list where both have: `None` past it, and
/// with none (a wait measuring) it waits until one does. A peer's end (`POLLHUP`) counts, for the read that says so.
fn first_readable(
    fds: [BorrowedFd<'_>; 2],
    patience: Option<Duration>,
) -> Result<Option<usize>, OciError> {
    let [first, second] = fds;
    let deadline = patience.and_then(|p| Instant::now().checked_add(p));
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let ts = left.map(|left| Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        });
        let mut polled = [
            PollFd::new(&first, PollFlags::IN),
            PollFd::new(&second, PollFlags::IN),
        ];
        match rustix::event::poll(&mut polled, ts.as_ref()) {
            Ok(n) if n > 0 => return Ok(polled.iter().position(|p| !p.revents().is_empty())),
            Ok(_) if left.is_some_and(|l| l.is_zero()) => return Ok(None),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(OciError::os("poll on a claimed init's pipe and socket", e)),
        }
    }
}

/// A refusal as a round trip's outcome: a seed's own machinery broken (replace it), or a refusal by type (the seed is
/// whole, the create refused).
fn refusal(failed: Failed, why: String) -> Trip {
    let os = |why| OciError::Os {
        op: "create in a container",
        why,
    };
    match failed {
        // An init past the exec bound the seed's own stack limit gives it (HEX-RT-14): the seed is whole, and the
        // create is refused naming the program.
        Failed::Program => Trip::Refused(OciError::Program(why)),
        // The seed's own machinery broke: replace it.
        Failed::Seed => Trip::Broken(os(why)),
        // A bad request, a container the seed couldn't make, or one no clone3-into-cgroup kernel
        // can make here: the seed is whole, the create refused by type.
        Failed::Request | Failed::Start | Failed::Unable => Trip::Refused(os(why)),
    }
}

/// Whether `fd` turns readable within `patience`; with none (a wait measuring) it waits until it does.
fn readable(fd: BorrowedFd<'_>, patience: Option<Duration>) -> Result<bool, OciError> {
    let deadline = patience.and_then(|p| Instant::now().checked_add(p));
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let ts = left.map(|left| Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        });
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, ts.as_ref()) {
            Ok(n) if n > 0 => return Ok(true),
            Ok(_) if left.is_some_and(|l| l.is_zero()) => return Ok(false),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(OciError::os("poll on a create seed's socket", e)),
        }
    }
}

/// A core refusal as a typed node error naming why. A line at the seeds' `bound` (full, or its wait ended) is
/// [`OciError::SeedsExhausted`], which reaches the caller as [`Refusal::Exhausted`] naming the bound: refused, never
/// sent through the shim (one implementation per job, the lead's ruling of 2026-11-06), and counted by the core
/// (`refused_busy`, `refused_waited`). Where the last seed start failed and none has started since, no seed could
/// come at any bound, so the refusal names that cause instead, as a [`Refusal::Os`]; as does a seed that died or
/// was taken for hung over the create.
fn refused(r: Refused, start_failure: Option<&str>, bound: usize) -> OciError {
    match (r, start_failure) {
        (Refused::Busy { .. } | Refused::Waited { .. }, None) => OciError::SeedsExhausted {
            bound,
            why: r.to_string(),
        },
        (_, Some(cause)) => OciError::Os {
            op: "create via a seed",
            why: format!("{r}; the last create seed start failed: {cause}"),
        },
        (Refused::Died { .. } | Refused::Hung { .. }, None) => OciError::Os {
            op: "create via a seed",
            why: r.to_string(),
        },
    }
}

/// A core stale report as a typed node error (the shell reports only what it was handed, so this is a
/// bug if it happens).
fn stale(s: Stale) -> OciError {
    OciError::Broken(format!("the create seed pool: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_core::patience::Patience;
    use hex_testkit::waits::until;

    /// The calls this host's node runs at once, the line's room ([`crate::sizes::Sizes::workers`]).
    fn workers() -> usize {
        crate::sizes::Sizes::now().unwrap().workers
    }

    /// A create's wait in line for a seed, on the line's own record for this build: the seeds' answers'
    /// (`hex_core::seeds`, decision 9), as the pool's line wait starts from.
    fn in_line() -> Patience {
        Patience::by_profile(
            "test.createpool.in_line",
            hex_core::seeds::CLONE_SLOWEST,
            Some(hex_core::seeds::CLONE_SLOWEST_DEBUG),
        )
    }

    /// Goal 5 plan step 2 (AC-T7.1.4; CLAUDE.md §10): classic-path creates are counted by a fixed set
    /// of reasons. Do: tell a pool a create went through the shim for each of two reasons, one twice,
    /// and that a rootless node's seed made one with the classic root. Expect: each reason's count is its
    /// own, the shim's total is the two shim reasons' sum without the rootless create, and a reason never
    /// told stays 0 (non-vacuity, so a miscounted reason shows).
    #[test]
    fn classic_creates_are_counted_by_reason() {
        let pool = CreatePool::new(
            PathBuf::from("/nonexistent/hex-for-the-test"),
            crate::sizes::Sizes::now().unwrap(),
            Prune::Keep(ClassicReason::Rootless),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        pool.classic(ClassicReason::UserNamespace);
        pool.classic(ClassicReason::UserNamespace);
        pool.classic(ClassicReason::Hooks);
        pool.classic(ClassicReason::Rootless);
        assert_eq!(pool.classic_creates_for(ClassicReason::UserNamespace), 2);
        assert_eq!(pool.classic_creates_for(ClassicReason::Hooks), 1);
        assert_eq!(pool.classic_creates_for(ClassicReason::Rootless), 1);
        assert_eq!(pool.classic_creates_for(ClassicReason::Source), 0);
        assert_eq!(pool.creates_through_shim(), 3);
    }

    /// Goal 17's baseline arm (the lead's ruling of 2026-11-09): a pool whose seeds keep the agent's mount namespace
    /// says why, and counts none of those classic creates through the shim. Do: open a pool kept for the baseline, and
    /// tell it a seed made one create with the classic root. Expect: it says its seeds keep the namespace for the
    /// baseline, counts that create under the baseline, and none through the shim.
    #[test]
    fn a_pool_kept_for_the_baseline_says_so_and_counts_none_of_its_creates_through_the_shim() {
        let pool = CreatePool::new(
            PathBuf::from("/nonexistent/hex-for-the-test"),
            crate::sizes::Sizes::now().unwrap(),
            Prune::Keep(ClassicReason::Baseline),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        assert_eq!(pool.unpruned(), Some(ClassicReason::Baseline));
        pool.classic(ClassicReason::Baseline);
        assert_eq!(pool.classic_creates_for(ClassicReason::Baseline), 1);
        assert_eq!(pool.creates_through_shim(), 0);
    }

    /// Goal 5 plan step 4 (T-T7.1.4; CLAUDE.md §3): a node whose seeds can't start says why. Do: open a
    /// pool on a `hex` binary that isn't there, wait until it has counted a failed start, then ask for a
    /// seed under the node's own bounds, its waits seeded from the record. Expect: the create is refused by
    /// type once the start it asked for fails too (no seed is up or starting, so none can come), naming the
    /// failed start's cause, not the seeds' bound (no bound was reached); never left waiting, never sent
    /// another way.
    #[test]
    fn a_pool_whose_seeds_cannot_start_names_why_it_refuses() {
        let pool = CreatePool::new(
            PathBuf::from("/nonexistent/hex-for-the-test"),
            crate::sizes::Sizes::now().unwrap(),
            Prune::Keep(ClassicReason::Rootless),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        // The pool's starting thread tries its first start once the pool opens, no recorded operation making it:
        // measuring, for the fact alone (the census's row is to come, from the suites run under it).
        let tried = Patience::measuring("test.createpool.start_tried");
        until(&tried, "the pool's first start tried", || {
            pool.counts().unwrap().start_failed > 0
        })
        .unwrap();
        let Err(OciError::Os { why, .. }) = pool.shared.acquire() else {
            panic!("a pool with no seed refuses its create by type, naming why");
        };
        assert!(
            why.contains("the last create seed start failed: starting a create seed"),
            "the refusal names the failed start's cause: {why}"
        );
        let counts = pool.counts().unwrap();
        assert_eq!(
            (counts.refused_waited, counts.started),
            (1, 0),
            "{counts:?}"
        );
    }

    /// Goal 5 extended step 6 (criterion 2, the rate at concurrency 32): a create wakes the starting thread
    /// only when it leaves it a seed to start, and a seed given back wakes the line only when a create waits
    /// in it. At concurrency 32 the creates waited 0.190 ms a lifecycle on the pool's lock (create_serial.md
    /// run 21): each woke the thread, which took the lock the create held to find nothing to start, and
    /// each seed given back made a system call to wake a line no create was in. Do: open a pool of two slots
    /// on a stand-in seed program; take a seed and give it back until both slots' seeds are up and idle;
    /// then take and give back a seed 1,000 times. Expect: the takes before woke the thread, which started
    /// the second seed (non-vacuity); the 1,000 woke neither the thread nor the line.
    #[test]
    fn a_create_wakes_the_starting_thread_only_for_a_seed_to_start() {
        let limits = Limits {
            seeds: std::num::NonZeroUsize::new(2).unwrap(),
            waiting: workers(),
        };
        let pool = CreatePool::with_limits(
            PathBuf::from("/bin/sh"),
            limits,
            Prune::Keep(ClassicReason::Rootless),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        let shared = &pool.shared;
        // The starting thread brings the seeds up as the takes ask for them, no recorded operation making both: measuring,
        // for the fact alone (the census's row is to come, from the suites run under it).
        let up = Patience::measuring("test.createpool.seeds_up");
        until(&up, "both of the pool's seeds up", || {
            let (id, proc, _) = shared.acquire().unwrap();
            shared.done(id, proc);
            shared.lock().unwrap().seeds.idle() == 2
        })
        .unwrap();
        let (starter, line) = (
            shared.woke[0].load(Ordering::Relaxed),
            shared.woke[1].load(Ordering::Relaxed),
        );
        assert!(
            starter >= 1,
            "the take of the only spare woke the starting thread"
        );
        assert_eq!(pool.counts().unwrap().started, 2);
        for _ in 0..1_000 {
            let (id, proc, _) = shared.acquire().unwrap();
            shared.done(id, proc);
        }
        assert_eq!(
            (
                shared.woke[0].load(Ordering::Relaxed) - starter,
                shared.woke[1].load(Ordering::Relaxed) - line
            ),
            (0, 0),
            "the wakes sent the starting thread and the line over 1,000 creates that left nothing to start"
        );
    }

    /// The lead's rule of 2026-11-06 (AC-T7.1.3): a parked init's claim that fails falls back to a seed's clone, and
    /// is counted by why, so a pass shows how often a parked init cost its create a clone. Do: tell a pool a claim
    /// hung once and was refused twice. Expect: (1, 2), and none before.
    #[test]
    fn a_failed_parked_claim_is_counted_by_why() {
        let pool = CreatePool::new(
            PathBuf::from("/nonexistent/hex-for-the-test"),
            crate::sizes::Sizes::now().unwrap(),
            Prune::Keep(ClassicReason::Rootless),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        assert_eq!(pool.claims_failed(), (0, 0));
        let e = || OciError::Broken("for the test".to_owned());
        pool.shared.claim_failed(&Trip::Hung(e()));
        pool.shared.claim_failed(&Trip::Refused(e()));
        pool.shared.claim_failed(&Trip::Broken(e()));
        assert_eq!(pool.claims_failed(), (1, 2));
    }

    /// The lead's (B) of 2026-11-09 (decision 8's claim), HEX-SEED-3: a request's descriptors are those its create
    /// counts, in the order the seed reads them. Do: the message of a pruned request without its cgroup directory (a
    /// parked init's claim), of one with it (a seed's clone), and of a classic request. Expect: the pruned ones lean,
    /// the sync pipe, the cgroup directory where it rides, the state directory, the rootfs and the checked spec, with no
    /// map pipe and no bundle directory; the classic one in the older layout, the sync and map pipes and the checked
    /// spec; and each as many as its create counts.
    #[test]
    fn a_requests_descriptors_are_those_its_create_counts_in_the_seeds_order() {
        use hex_runtime::linux::cgroup::BoxCgroup;
        use hex_runtime::linux::oci::run::PrunedCreate;
        use rustix::fs::{Mode, OFlags};
        use std::os::fd::AsRawFd;
        let lab = tempfile::tempdir().unwrap();
        let open = || {
            rustix::fs::open(
                lab.path(),
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap()
        };
        let cgroup = BoxCgroup::at(lab.path().to_owned());
        let config = open();
        let request = |map_r: Option<OwnedFd>, pruned: Option<PrunedCreate>| CloneReq {
            bundle: lab.path(),
            dir: lab.path(),
            cgroup: &cgroup,
            network: None,
            sync_w: open(),
            map_r,
            config: config.as_fd(),
            pruned,
        };
        let pruned = |cgroup_dir: Option<OwnedFd>| PrunedCreate {
            cgroup_dir,
            container_dir: open(),
            rootfs: open(),
            mount_namespace: MountNamespace::Copy,
            dev_mount: None,
            ipc_uts: None,
            proc_mount: None,
            proc_sink: None,
            masked_directory: None,
            binds: Vec::new(),
            path_joins: Vec::new(),
        };
        let raw = |fds: &[BorrowedFd<'_>]| fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
        for cgroup_dir in [None, Some(open())] {
            let rides = cgroup_dir.as_ref().map(AsRawFd::as_raw_fd);
            let req = request(None, Some(pruned(cgroup_dir)));
            let (create, fds) = message(&req);
            assert_eq!(
                create.fixed,
                Fixed::Lean {
                    cgroup: rides.is_some(),
                }
            );
            let p = req.pruned.as_ref().unwrap();
            let want: Vec<_> = [Some(req.sync_w.as_raw_fd()), rides]
                .into_iter()
                .flatten()
                .chain([
                    p.container_dir.as_raw_fd(),
                    p.rootfs.as_raw_fd(),
                    config.as_raw_fd(),
                ])
                .collect();
            assert_eq!(raw(&fds), want);
            assert_eq!(fds.len(), create.fds());
        }
        let req = request(Some(open()), None);
        let (create, fds) = message(&req);
        assert_eq!(create.fixed, Fixed::All);
        assert_eq!(
            raw(&fds),
            vec![
                req.sync_w.as_raw_fd(),
                req.map_r.as_ref().unwrap().as_raw_fd(),
                config.as_raw_fd()
            ]
        );
        assert_eq!(fds.len(), create.fds());
    }

    /// The lead's (A) of 2026-11-09 (decision 8's claim): a claimed init says nothing on its claim socket once it takes
    /// the claim, so the claim is taken at the init's first word on its sync pipe, which stays there for the handshake
    /// to read, and fails on the init's refusal, its end, or its silence past the claim's patience. Do: claim a
    /// stand-in init that, given its claim, writes its ready; writes a setup failure and ends; refuses; ends without a
    /// word; or holds its claim and says nothing. Expect: taken at the ready, still on the pipe; taken at the failure,
    /// still on the pipe for the handshake to report; refused; broken; and hung, given no time at all to wait. The
    /// others wait at most the claim's bound before (A), the seeds' answers' record, so a claim that waits for an
    /// answer fails here rather than waiting for ever.
    #[test]
    fn a_claim_is_taken_at_its_inits_first_word_and_otherwise_falls_back() {
        use hex_runtime::linux::cgroup::BoxCgroup;
        use hex_runtime::linux::oci::run::PrunedCreate;
        use rustix::fs::{Mode, OFlags};
        use rustix::pipe::{PipeFlags, pipe_with};
        let lab = tempfile::tempdir().unwrap();
        let open = || {
            rustix::fs::open(
                lab.path(),
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap()
        };
        let cgroup = BoxCgroup::at(lab.path().to_owned());
        let config = open();
        let before = Patience::by_profile(
            "test.createpool.claim_before",
            hex_core::seeds::CLONE_SLOWEST,
            Some(hex_core::seeds::CLONE_SLOWEST_DEBUG),
        )
        .deadline();
        // A claim of a stand-in that does `then` with its claim's sync pipe end and its end of the claim socket, or,
        // with none, holds its claim unread: what came of it, and what is left on the sync pipe.
        let claim = |then: Option<fn(OwnedFd, OwnedFd)>, patience: Option<Duration>| {
            let (sync_r, sync_w) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
            let (ours, theirs) = rt_seed::socketpair().unwrap();
            let req = CloneReq {
                bundle: lab.path(),
                dir: lab.path(),
                cgroup: &cgroup,
                network: None,
                sync_w,
                map_r: None,
                config: config.as_fd(),
                pruned: Some(PrunedCreate {
                    cgroup_dir: None,
                    container_dir: open(),
                    rootfs: open(),
                    mount_namespace: MountNamespace::Copy,
                    dev_mount: None,
                    ipc_uts: None,
                    proc_mount: None,
                    proc_sink: None,
                    masked_directory: None,
                    binds: Vec::new(),
                    path_joins: Vec::new(),
                }),
            };
            let (create, fds) = message(&req);
            let (stand_in, _held) = match then {
                Some(then) => (
                    Some(std::thread::spawn(move || {
                        let mut buf = vec![0u8; hex_core::seed::MAX_REQUEST];
                        let Ok(rt_seed::Received::Message { stdio, .. }) =
                            rt_seed::recv_request(theirs.as_fd(), &mut buf)
                        else {
                            panic!("the stand-in was sent no claim");
                        };
                        then(stdio.into_iter().next().unwrap(), theirs);
                    })),
                    None,
                ),
                None => (None, Some(theirs)),
            };
            let came = match claim_round(ours.as_fd(), sync_r.as_fd(), &create, &fds, patience) {
                Ok(MountNamespace::Copy) => "taken",
                Ok(_) => "taken another way",
                Err(Trip::Refused(_)) => "refused",
                Err(Trip::Broken(_)) => "broken",
                Err(Trip::Hung(_)) => "hung",
            };
            if let Some(stand_in) = stand_in {
                stand_in.join().unwrap();
            }
            let mut left = [0u8; 64];
            let n = rustix::io::read(&sync_r, &mut left).unwrap_or(0);
            (came, String::from_utf8_lossy(&left[..n]).into_owned())
        };
        let ready: fn(OwnedFd, OwnedFd) = |sync, _socket| {
            rustix::io::write(&sync, b"R\n").unwrap();
        };
        let failed: fn(OwnedFd, OwnedFd) = |sync, _socket| {
            rustix::io::write(&sync, b"Ea step failed\n").unwrap();
        };
        let refuses: fn(OwnedFd, OwnedFd) = |_sync, socket| {
            let why = "refused for the test".to_owned();
            let refusal = Answer::Refused {
                failed: Failed::Start,
                why,
            };
            rt_seed::send_answer(socket.as_fd(), &refusal, None).unwrap();
        };
        let ends: fn(OwnedFd, OwnedFd) = |_sync, _socket| {};
        assert_eq!(claim(Some(ready), before), ("taken", "R\n".to_owned()));
        assert_eq!(
            claim(Some(failed), before),
            ("taken", "Ea step failed\n".to_owned())
        );
        assert_eq!(claim(Some(refuses), before), ("refused", String::new()));
        assert_eq!(claim(Some(ends), before), ("broken", String::new()));
        assert_eq!(claim(None, Some(Duration::ZERO)), ("hung", String::new()));
    }

    /// Goal 5 extended step 6: a seed given back goes at once to the create waiting in line, woken by the
    /// hand-off and not by its wait's end. Do: open a pool of one slot, whose waits are seeded from the test's
    /// patience, so a create in it is refused only past twice that; take its seed; ask for another from a thread of the test's, and
    /// once that create waits in line, give the first back. Expect: the waiting create is handed the seed
    /// given back within the test's patience (a hand-off that woke no one fails).
    #[test]
    fn a_seed_given_back_goes_at_once_to_the_create_in_line() {
        let limits = Limits {
            seeds: std::num::NonZeroUsize::new(1).unwrap(),
            waiting: workers(),
        };
        let pool = CreatePool::with_limits(
            PathBuf::from("/bin/sh"),
            limits,
            Prune::Keep(ClassicReason::Rootless),
            None,
            #[cfg(debug_assertions)]
            None,
        )
        .unwrap();
        let (id, proc, _) = pool.shared.acquire().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Arc::clone(&pool.shared);
        std::thread::spawn(move || {
            let handed = shared.acquire().map(|(id, proc, _)| {
                shared.done(id, proc);
                id
            });
            tx.send(handed).unwrap();
        });
        // The second create joins the line within its own call, and is handed the seed given back as the line's wait
        // answers: both within the line's record.
        let line = in_line();
        until(&line, "the second create waiting in line", || {
            pool.shared.lock().unwrap().seeds.waiting() > 0
        })
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "a create in line is handed no seed before one is given back"
        );
        pool.shared.done(id, proc);
        let handed = rx
            .recv_timeout(line.deadline().unwrap_or(Duration::MAX))
            .expect("the create in line was handed the seed given back");
        assert_eq!(handed.unwrap(), id);
    }

    /// Ada's rule, 2026-11-06 (AC-T7.1.3): `SPARE`'s one seed beyond the creates waiting was picked; the spare is now
    /// the most creates seen waiting at once. Do: a pool's state with four slots and none up, its spare at one, then
    /// told three creates waited at once. Expect: one seed is wanted at first, none once one is starting, and with
    /// three seen waiting, starts are wanted until three are up idle or starting; the spare never passes the slots.
    #[test]
    fn the_spare_is_the_most_creates_seen_waiting_at_once() {
        let limits = Limits {
            seeds: std::num::NonZeroUsize::new(4).unwrap(),
            waiting: workers(),
        };
        let mut inner = Inner {
            seeds: Seeds::growing(limits, 4),
            procs: (0..4).map(|_| None).collect(),
            start_failure: None,
            shut: false,
            spare: Learned::new(1, 4),
        };
        assert!(wants_start(&inner));
        let _one = inner.seeds.start().unwrap();
        assert!(!wants_start(&inner), "one spare starting");
        inner.spare.observe(3);
        let mut started = 1;
        while wants_start(&inner) {
            let _ = inner.seeds.start().unwrap();
            started += 1;
        }
        assert_eq!(started, 3);
        inner.spare.observe(9);
        assert_eq!(inner.spare.size(), 4, "never past the slots");
    }
}
