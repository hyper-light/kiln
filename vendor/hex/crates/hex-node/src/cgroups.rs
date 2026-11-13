//! The `oci` boxes' cgroups that may be handed from box to box of one owner (goal 5 extended, step 6;
//! decision 5, "The cgroup lock"): the shell over [`hex_core::reuse`]'s pool. A create of an `oci` box
//! whose spec names an owner asks [`Choice`] for its cgroup once the runtime has read its spec: the
//! owner's idle cgroup under the box's device policy whose files read clean, with every bound written
//! again, else a fresh slot, else a cgroup of the box's own named by its id. A delete gives the box's
//! slot back once its record is gone, and what the pool's bounds or a failed check put out is removed by
//! a thread of its own, off create's path. How each box was ended is counted here too: through its
//! init's pidfd, that falling back to `cgroup.kill`, or `cgroup.kill` itself.

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use hex_api::{Bounds, BoxId, BoxOwner, Refusal, Usage};
use hex_core::cgroup::Baseline;
use hex_core::demand::Depth;
use hex_core::reuse::{self, Pool, Refused, Take, Whose};
use hex_runtime::linux::cgroup::{BoxCgroup, BoxRoot};
use hex_runtime::linux::oci::init::PreparedProc;
use hex_runtime::linux::oci::resources::Insn;
use hex_runtime::linux::oci::run::{Choose, Chosen, Parked, ParkedProc, ProcSink, Wants};
use hex_runtime::linux::oci::{OciError, procmount};

use crate::createpool::Parker;
use crate::devfs::ProcMaker;
use crate::sizes::Sizes;

/// Most memory an idle cgroup may still have charged to be handed on (research f27-f30), measured
/// (bench/competitive/probes/reuse_residual.md). A box whose processes have all ended leaves the charge
/// the kernel caches per CPU and some kernel memory, none of it the box's own (anon, shmem and dirty file
/// pages 0): the bench's lifecycle at most 1,748,992 bytes, 1.7 MiB (256 idle cgroups of fedora's OCI
/// pass 20261103T044133Z), and oci_reuse's test 2, a box that burned CPU, held 64 MiB and had a program
/// OOM-killed, 3,719,168 to 7,471,104 bytes (3.5 to 7.1 MiB) over six runs on adacachy and 5,054,464 in
/// fedora's root lab (unit hex-g5x-s6-fedora-20261103T011609Z). A box that left files in memory (its
/// tmpfs, `/dev/shm`, page cache of what it wrote) leaves tens of megabytes (64 MiB in test 3, 85 MB in
/// f27), which the next box would be charged for, so its cgroup goes. 16 MiB sits above the first two
/// and under the third (the lead, 2026-11-03T04:50Z).
pub const RESIDUAL: u64 = 16 << 20;

/// The device policy a cgroup's own program is under: `None` when it carries none.
type Policy = Option<Vec<Insn>>;

/// A live box's slot: what its usage is reported from and what its delete gives back.
#[derive(Debug)]
pub struct Held {
    pub slot: u64,
    pub baseline: Baseline,
    /// Its `memory.peak`, written when the box was given the slot ([`BoxCgroup::peak_from_now`]).
    peak: Option<OwnedFd>,
    owner: String,
    /// The device policy whose program its cgroup carries; `None` when it isn't known (a box taken back
    /// after a restart), so its cgroup is removed with it, never handed on.
    policy: Option<Policy>,
    /// Whether `cgroup.kill` was written to its cgroup in its box's life. Linux 6.19 compares a cgroup's
    /// count of kills with the one the forking task's own cgroup had when a task is cloned into it
    /// (`CLONE_INTO_CGROUP`) and kills the child if they differ (kernel/cgroup/cgroup.c:6702-6705 sample the
    /// forking task's own cgroup, `kargs->cgrp` set only at :6768; :6902 and :6941 compare it with the
    /// target's), so every init later cloned into such a cgroup would die: it is removed with its box.
    killed: bool,
    /// The namespaces the box's init was made with (`clone3`'s `CLONE_NEW*` bits), which an init made ahead
    /// for the owner's next box is made with (decision 8, "The p99.9 that stayed"); `None` until it is known.
    park_flags: Option<u64>,
    /// Its spec's prepared-proc key, which the proc made for that init is made for (parked procs); `None` when
    /// its spec has none or it isn't known.
    park_proc_key: Option<procmount::Key>,
}

impl Held {
    /// `cgroup.kill` was written to its cgroup.
    pub fn mark_killed(&mut self) {
        self.killed = true;
    }
}

/// How the node's deletes ended their boxes, counted: what tests and the bench read (research f45-f50).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kills {
    /// `oci` boxes ended through their init's pidfd, with no `cgroup.kill`.
    pub through_init: u64,
    /// `oci` boxes the pidfd didn't empty, something outside the init's pid namespace still in the
    /// cgroup, ended by `cgroup.kill` after it.
    pub fell_back: u64,
    /// Boxes ended by `cgroup.kill` alone: `process` boxes, and `oci` boxes whose init has no pid
    /// namespace of its own or isn't watched.
    pub cgroup_kill: u64,
}

/// What the pool and its remover have done since the node opened: [`reuse::Counts`] and the cgroups
/// removed off create's path, with the last removal that failed, which recovery finishes.
/// What the inits made ahead in idle cgroups did (decision 8, "The p99.9 that stayed"): made, claimed by the
/// owner's next box, ended unclaimed (their cgroup evicted, drained or refused, or the init refused by a create
/// that made other namespaces), and parks that failed (the slot then idle and empty, as before).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParkCounts {
    pub parked: u64,
    pub claimed: u64,
    pub ended_unclaimed: u64,
    pub failed: u64,
    /// The bytes charged to their cgroups for the inits made, summed: over `parked`, what one costs.
    pub charged_bytes: u64,
    /// Procs made for parked inits' own pid namespaces (parked procs: a root node's default, unless turned off).
    pub procs_made: u64,
    /// Of those, the ones made by the thread that parked their init: a delete's (reuse on), or the `hex-ahead`
    /// refill's (reuse off). Never a create's or a claim's.
    pub procs_made_at_delete: u64,
    pub procs_made_at_refill: u64,
    /// Creates whose init's claim took the proc made for it: the init attached it, mounting none of its own.
    pub proc_hits: u64,
    /// Procs made for a parked init that its create didn't take (another key, no room for it, the claim
    /// refused): each ended unused, its create's init mounting its own proc.
    pub proc_misses: u64,
    /// Creates that claimed a parked init with no proc made for it, which one could have served (its key unknown
    /// at its park, or its make failed): each init mounted its own.
    pub proc_dry: u64,
    /// Procs that couldn't be made for an init parked (the init then parked without one, as before).
    pub procs_failed: u64,
    /// Procs held now with the inits they were made for, at most one an init parked.
    pub procs_held: usize,
    /// Unattached procs (misses, and those of inits ended unclaimed) handed to the node's thread for them, those it
    /// has dropped, off every create's path, and those dropped where they were, its queue full.
    pub procs_queued: u64,
    pub procs_dropped: u64,
    pub procs_dropped_inline: u64,
    /// Procs alive now (held, or waiting for the dropper), at most the bound; and parks that made none, the bound
    /// reached.
    pub procs_alive: u64,
    pub procs_capped: u64,
}

/// What the cgroups made ahead for boxes of no owner did (decision 8, "The p99.9 that stayed"): made, each with an
/// init parked in it; taken by a box; creates that found none ready (each made its own cgroup and cloned its
/// init, as before); and makes that failed. `ready` is how many wait now, at most the node's bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AheadCounts {
    pub made: u64,
    pub taken: u64,
    pub dry: u64,
    pub failed: u64,
    pub ready: usize,
    /// How many the node keeps ready now: learned from its draws unless set (Ada's rule, 2026-11-06).
    pub size: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub pool: reuse::Counts,
    pub parked: ParkCounts,
    pub ahead: AheadCounts,
    /// Slots removed rather than kept idle because `cgroup.kill` was written to them in their box's life.
    pub discarded_killed: u64,
    pub removed: u64,
    pub removed_inline: u64,
    pub removals_failed: u64,
    pub last_failure: Option<String>,
}

#[derive(Debug, Default)]
struct Removal {
    removed: AtomicU64,
    inline: AtomicU64,
    failed: AtomicU64,
    last: Mutex<Option<String>>,
}

impl Removal {
    /// Ends what runs in `cg` and removes it, counting how it went; a removal that failed is kept as the
    /// last failure ([`Counts::last_failure`]) and returned.
    fn remove(&self, cg: &BoxCgroup) -> Result<(), Refusal> {
        match end_and_remove(cg) {
            Ok(()) => {
                self.removed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut last) = self.last.lock() {
                    *last = Some(format!("{}: {e}", cg.dir().display()));
                }
                Err(e)
            }
        }
    }
}

/// The node's slots, idle and held, and how its boxes were ended.
pub struct Cgroups {
    pool: Arc<Mutex<Pool<Policy>>>,
    /// The most idle cgroups the pool keeps ([`Sizes::idle_cgroups`]).
    most_idle: usize,
    held: Mutex<BTreeMap<BoxId, Held>>,
    remover: SyncSender<BoxCgroup>,
    removal: Arc<Removal>,
    through_init: AtomicU64,
    fell_back: AtomicU64,
    cgroup_kill: AtomicU64,
    discarded_killed: AtomicU64,
    /// `oci` boxes made with no owner, each given a cgroup of its own without asking the pool, so the
    /// path every box took before takes no lock of the pool's: counted here, read as the pool's
    /// [`Refused::NoOwner`].
    no_owner: AtomicU64,
    /// The inits made ahead in idle cgroups, by slot (decision 8, "The p99.9 that stayed").
    parking: Arc<Parking>,
    /// The cgroups made ahead for boxes of no owner, each with an init in it (decision 8).
    ahead: Arc<Ahead>,
    /// The create seeds' handle that parks inits, once they are up ([`Cgroups::start_parker`]).
    parker: OnceLock<Parker>,
}

/// Each slot's init made ahead, with the bytes charged to its cgroup for it.
type Slots = BTreeMap<u64, (Parked, u64)>;

/// The inits made ahead in idle cgroups, by slot, with the bytes charged to each cgroup for its init, and their
/// counts (decision 8, "The p99.9 that stayed"). Bounded by the idle slots the pool keeps, one init each.
#[derive(Default)]
struct Parking {
    slots: Mutex<Slots>,
    parked: AtomicU64,
    claimed: AtomicU64,
    ended: AtomicU64,
    failed: AtomicU64,
    /// The bytes charged to their cgroups for every init made ahead, summed.
    charged: AtomicU64,
    /// What makes each parked init's proc, where the node has one ([`Cgroups::set_parked_procs`]: a root node's).
    procs: OnceLock<ProcMaker>,
    procs_made: AtomicU64,
    procs_made_at_delete: AtomicU64,
    procs_made_at_refill: AtomicU64,
    proc_hits: AtomicU64,
    proc_misses: AtomicU64,
    proc_dry: AtomicU64,
    procs_failed: AtomicU64,
    /// Where unattached procs go to be dropped, and what its thread dropped, once the node's cgroups start.
    drops: OnceLock<ProcSink>,
    queued: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    dropped_inline: Arc<AtomicU64>,
    /// Procs alive and their bound ([`Sizes::procs_alive`] unless set): a make reserves one first, and a drop or a
    /// claim's hit gives it back.
    alive: Arc<AtomicU64>,
    bound: AtomicU64,
    /// The dropper's queue's capacity, the highest `bound` may be.
    capacity: AtomicU64,
    capped: AtomicU64,
}

impl Parking {
    /// Whether the node makes parked inits' procs now.
    fn makes_procs(&self) -> bool {
        self.procs.get().is_some()
    }

    fn slots(&self) -> Result<MutexGuard<'_, Slots>, Refusal> {
        self.slots
            .lock()
            .map_err(|_| Refusal::Broken("the node's parked inits' lock was poisoned".to_owned()))
    }

    /// Slot `n`'s init made ahead, taken from it, with the bytes charged for it.
    fn take(&self, n: u64) -> Result<Option<(Parked, u64)>, Refusal> {
        Ok(self.slots()?.remove(&n))
    }

    /// Slot `n`'s init made ahead, if any, ended (its cgroup is going).
    fn end(&self, n: u64) {
        if let Some(gone) = self.slots().ok().and_then(|mut slots| slots.remove(&n)) {
            self.ended.fetch_add(1, Ordering::Relaxed);
            drop(gone);
        }
    }

    fn counts(&self) -> ParkCounts {
        ParkCounts {
            parked: self.parked.load(Ordering::Relaxed),
            claimed: self.claimed.load(Ordering::Relaxed),
            ended_unclaimed: self.ended.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            charged_bytes: self.charged.load(Ordering::Relaxed),
            procs_made: self.procs_made.load(Ordering::Relaxed),
            procs_made_at_delete: self.procs_made_at_delete.load(Ordering::Relaxed),
            procs_made_at_refill: self.procs_made_at_refill.load(Ordering::Relaxed),
            proc_hits: self.proc_hits.load(Ordering::Relaxed),
            proc_misses: self.proc_misses.load(Ordering::Relaxed),
            proc_dry: self.proc_dry.load(Ordering::Relaxed),
            procs_failed: self.procs_failed.load(Ordering::Relaxed),
            procs_held: self.slots().map_or(0, |slots| {
                slots
                    .values()
                    .filter(|(p, _)| p.proc_key().is_some())
                    .count()
            }),
            procs_queued: self.queued.load(Ordering::Relaxed),
            procs_dropped: self.dropped.load(Ordering::Relaxed),
            procs_dropped_inline: self.dropped_inline.load(Ordering::Relaxed),
            procs_alive: self.alive.load(Ordering::Relaxed),
            procs_capped: self.capped.load(Ordering::Relaxed),
        }
    }

    /// The proc of `parked`'s own pid namespace made for `key` and held with it, when the node makes them and the
    /// key is known; one that can't be made leaves the init parked without one, counted.
    fn with_proc(&self, parked: &mut Parked, key: Option<&procmount::Key>, by: MadeBy) {
        if !self.makes_procs() {
            return;
        }
        let (Some(maker), Some(key), Some(sink)) = (self.procs.get(), key, self.drops.get()) else {
            return;
        };
        let bound = self.bound.load(Ordering::Relaxed);
        if self
            .alive
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < bound).then(|| n.saturating_add(1))
            })
            .is_err()
        {
            self.capped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match parked.pid_namespace().and_then(|ns| maker.make(ns, key)) {
            Ok(proc) => {
                parked.set_proc(key.clone(), proc, sink.clone());
                self.procs_made.fetch_add(1, Ordering::Relaxed);
                match by {
                    MadeBy::Delete => &self.procs_made_at_delete,
                    MadeBy::Refill => &self.procs_made_at_refill,
                }
                .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.procs_failed.fetch_add(1, Ordering::Relaxed);
                self.release_alive();
            }
        }
    }

    /// One alive proc gone: dropped, or handed to the init that claimed it.
    fn release_alive(&self) {
        let _ = self
            .alive
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }
}

/// Which thread parked an init and made its proc (decision 8, parked procs): the park's own, so the work scales with
/// the deletes and refills that cause it (195e86a's and 6dcd731's A/Bs: workers fell behind).
#[derive(Clone, Copy)]
enum MadeBy {
    Delete,
    Refill,
}

/// The cgroups made ahead for boxes of no owner (decision 8, "The p99.9 that stayed"): a queue of at most `bound`
/// slots, each with an init parked in it ([`Parking`]), refilled by a thread of its own off every create's path
/// once a box of no owner has said which namespaces its init was made with (`flags`, 0 until then). A box of no
/// owner takes the oldest; with none ready it makes its cgroup and clones its init as before, counted dry.
#[derive(Default)]
struct Ahead {
    /// How many the refill keeps ready, read without the lock: `depth`'s size, 0 once drained.
    bound: AtomicU64,
    /// The size learned from the takes' draws ([`Depth`]), or fixed where the node was given one.
    depth: Mutex<Depth>,
    flags: AtomicU64,
    /// The prepared-proc key of the last box of no owner to choose its cgroup, which each init parked ahead has its
    /// proc made for: set before that box takes one, so the refill its take wakes reads it.
    proc_key: Mutex<Option<procmount::Key>>,
    ready: Mutex<VecDeque<MadeAhead>>,
    /// Signalled when a slot is taken, the flags are first known, or the node is going.
    want: Condvar,
    shut: AtomicBool,
    /// Held across each make, so a drain waits for the one under way.
    busy: Mutex<()>,
    made: AtomicU64,
    taken: AtomicU64,
    dry: AtomicU64,
    failed: AtomicU64,
}

impl Ahead {
    /// The refill keeps `depth`'s size ready from here on.
    fn size(&self, depth: Depth) {
        if let Ok(mut d) = self.depth.lock() {
            *d = depth;
        }
        self.bound.store(
            u64::try_from(depth.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// A take, `dry` when it found none ready: the size learns from the draw.
    fn took(&self, dry: bool) {
        let Ok(mut d) = self.depth.lock() else { return };
        if d.took(dry)
            && !self.shut.load(Ordering::Relaxed)
            && self.bound.load(Ordering::Relaxed) > 0
        {
            crate::sizes::rose();
            self.bound.store(
                u64::try_from(d.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    /// `ready` cgroups are ready: the size's draw ends once they fill it.
    fn filled(&self, ready: usize) {
        if let Ok(mut d) = self.depth.lock()
            && ready >= d.size()
        {
            d.full();
        }
    }

    fn ready(&self) -> Result<MutexGuard<'_, VecDeque<MadeAhead>>, Refusal> {
        self.ready.lock().map_err(|_| {
            Refusal::Broken("the node's cgroups made ahead lock was poisoned".to_owned())
        })
    }

    fn counts(&self) -> AheadCounts {
        AheadCounts {
            made: self.made.load(Ordering::Relaxed),
            taken: self.taken.load(Ordering::Relaxed),
            dry: self.dry.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            ready: self.ready().map_or(0, |r| r.len()),
            size: usize::try_from(self.bound.load(Ordering::Relaxed)).unwrap_or(usize::MAX),
        }
    }
}

/// A cgroup made ahead, ready for a box of no owner: its slot, the cgroup with its init parked in it, and what the box's
/// usage is reported from, which the refill read once that init parked ([`refill_ahead`]): the counters' baseline and
/// the `memory.peak` descriptor written then ([`BoxCgroup::peak_from_now`]). A parked init does nothing until its box
/// takes it, so they are what the take would read, read off the create's path (goal 16: the breakdown's part 4, the
/// claim's 0.030 ms p50, create-breakdown-20261109T164604Z).
struct MadeAhead {
    n: u64,
    cg: BoxCgroup,
    baseline: Baseline,
    peak: Option<OwnedFd>,
}

/// The refill's loop: while fewer than the bound are ready and the flags are known, one more cgroup made ahead,
/// as a fresh slot with default bounds (a box's own are written when it takes it), an init parked in it, and its usage's
/// baseline and peak read once the init is parked ([`MadeAhead`]); a slot whose peak can't be reset is no make. A make
/// that fails waits for the next take before it tries again, so a lasting failure never spins. Ends when the node does.
fn refill_ahead(
    ahead: &Ahead,
    parking: &Parking,
    pool: &Mutex<Pool<Policy>>,
    root: &BoxRoot,
    parker: &Parker,
) {
    let mut after_failure = false;
    // Every file read or written in a cgroup made ahead, by this thread or by the create that takes it, opened with it
    // here, so that create looks none up (goal 16: its lookups queued behind this thread's next mkdir).
    let files = root.ahead_files();
    loop {
        let flags = {
            let Ok(mut ready) = ahead.ready() else { return };
            loop {
                if ahead.shut.load(Ordering::Relaxed) {
                    return;
                }
                let bound = usize::try_from(ahead.bound.load(Ordering::Relaxed)).unwrap_or(0);
                let flags = ahead.flags.load(Ordering::Relaxed);
                if ready.len() < bound && flags != 0 && !after_failure {
                    break flags;
                }
                after_failure = false;
                ready = match ahead.want.wait(ready) {
                    Ok(ready) => ready,
                    Err(_) => return,
                };
            }
        };
        let Ok(_busy) = ahead.busy.lock() else { return };
        let key = ahead.proc_key.lock().ok().and_then(|k| k.clone());
        let made = pool
            .lock()
            .ok()
            .and_then(|mut pool| pool.ahead())
            .ok_or_else(|| {
                OciError::Broken("no slot number left for a cgroup made ahead".to_owned())
            })
            .and_then(|n| {
                let cg = root
                    .create_slot(n, &Bounds::default())
                    .map_err(|e| OciError::Broken(e.to_string()))?
                    .open_ahead(&files);
                match park_counted(parking, parker, cg.dir(), flags) {
                    Ok(mut p) => {
                        parking.with_proc(&mut p.0, key.as_ref(), MadeBy::Refill);
                        Ok((n, cg, p))
                    }
                    Err(e) => {
                        let _ = end_and_remove(&cg);
                        Err(e)
                    }
                }
            });
        // What its box's usage is reported from, read now its init is parked and does nothing until taken.
        let made = made.and_then(|(n, cg, p)| match cg.peak_from_now() {
            Ok(peak) => Ok((n, cg.baseline(), cg, p, peak)),
            Err(e) => {
                drop(p);
                let _ = end_and_remove(&cg);
                Err(OciError::Broken(e.to_string()))
            }
        });
        match made {
            Ok((n, baseline, cg, p, peak)) => {
                parking.parked.fetch_add(1, Ordering::Relaxed);
                ahead.made.fetch_add(1, Ordering::Relaxed);
                let Ok(mut ready) = ahead.ready() else { return };
                // Drained while it was made (the bound set to 0): it goes, its init with it.
                if ahead.bound.load(Ordering::Relaxed) == 0 {
                    drop(ready);
                    parking.ended.fetch_add(1, Ordering::Relaxed);
                    drop(p);
                    let _ = end_and_remove(&cg);
                    continue;
                }
                if let Ok(mut slots) = parking.slots() {
                    slots.insert(n, p);
                }
                ready.push_back(MadeAhead {
                    n,
                    cg,
                    baseline,
                    peak,
                });
                ahead.filled(ready.len());
            }
            Err(_) => {
                ahead.failed.fetch_add(1, Ordering::Relaxed);
                after_failure = true;
            }
        }
    }
}

/// An init made in the cgroup at `dir` with the namespaces `flags`, and the bytes charged to the cgroup for it.
fn park_in(parker: &Parker, dir: &std::path::Path, flags: u64) -> Result<(Parked, u64), OciError> {
    let fd = rustix::fs::open(
        dir,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| OciError::os("opening an idle cgroup to park an init in", e))?;
    let before = memory_current(dir);
    let p = parker.park(fd.as_fd(), flags)?;
    Ok((p, memory_current(dir).saturating_sub(before)))
}

/// [`park_in`], its charge counted.
fn park_counted(
    parking: &Parking,
    parker: &Parker,
    dir: &std::path::Path,
    flags: u64,
) -> Result<(Parked, u64), OciError> {
    let made = park_in(parker, dir, flags)?;
    parking.charged.fetch_add(made.1, Ordering::Relaxed);
    Ok(made)
}

/// A cgroup's `memory.current`, 0 where it can't be read (no memory controller handed down: nothing is
/// charged to it).
fn memory_current(dir: &std::path::Path) -> u64 {
    std::fs::read_to_string(dir.join("memory.current"))
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

impl Cgroups {
    /// The node's cgroups, fresh slots numbered from `first` ([`Pool::new`]), bounded by `sizes`: at most its
    /// `idle_cgroups` idle, and each owner's bound learned from its misses from `start` up. The remover's thread
    /// starts with a queue of as many, every idle cgroup the pool can put out at one time, past which the caller
    /// removes the cgroup itself, as every delete did before; it ends when the node does.
    pub fn start(first: u64, sizes: Sizes) -> Result<Self, Refusal> {
        let (remover, queue) = mpsc::sync_channel(sizes.idle_cgroups);
        let removal = Arc::new(Removal::default());
        let theirs = Arc::clone(&removal);
        std::thread::Builder::new()
            .name("hex-cgroup-remover".to_owned())
            .spawn(move || remove_each(&queue, &theirs))
            .map_err(|e| Refusal::os("starting the cgroup remover", e))?;
        let parking = Parking::default();
        Ok(Self {
            pool: Arc::new(Mutex::new(Pool::new(
                sizes.starts.owner_cgroups,
                sizes.idle_cgroups,
                first,
            ))),
            most_idle: sizes.idle_cgroups,
            held: Mutex::new(BTreeMap::new()),
            remover,
            removal,
            through_init: AtomicU64::new(0),
            fell_back: AtomicU64::new(0),
            cgroup_kill: AtomicU64::new(0),
            discarded_killed: AtomicU64::new(0),
            no_owner: AtomicU64::new(0),
            parking: Arc::new(parking),
            ahead: Arc::new(Ahead::default()),
            parker: OnceLock::new(),
        })
    }

    /// Inits are made ahead in idle cgroups through `parker`, the node's create seeds (decision 8, "The p99.9
    /// that stayed"), from here on, and in cgroups made ahead under `root` for boxes of no owner when the node
    /// keeps any ([`Cgroups::set_ahead`]); until then none is.
    pub fn start_parker(&self, parker: Parker, root: &BoxRoot) -> Result<(), Refusal> {
        if self.parker.get().is_some() {
            return Ok(());
        }
        if self.ahead.bound.load(Ordering::Relaxed) > 0 {
            let (ahead, parking, pool, root, theirs) = (
                Arc::clone(&self.ahead),
                Arc::clone(&self.parking),
                Arc::clone(&self.pool),
                root.clone(),
                parker.clone(),
            );
            std::thread::Builder::new()
                .name("hex-ahead".to_owned())
                .spawn(move || refill_ahead(&ahead, &parking, &pool, &root, &theirs))
                .map_err(|e| Refusal::os("starting the cgroups-ahead refill", e))?;
        }
        let _ = self.parker.set(parker);
        Ok(())
    }

    /// `bound` cgroups are kept made ahead for boxes of no owner, each with an init in it (decision 8), fixed in place
    /// of the learned number ([`Cgroups::learn_ahead`], the node's): a test control; 0 makes none. Set before the
    /// node's first create.
    pub fn set_ahead(&self, bound: usize) {
        self.ahead.size(Depth::new(bound, bound));
    }

    /// The cgroups made ahead for boxes of no owner start at `start` and learn their number from the takes' draws,
    /// up to `cap` (Ada's rule, 2026-11-06): a take that finds none ready raises it to the draw's takes plus one
    /// for each found none ([`Depth`]), and it never falls. Set before the node's first create.
    pub fn learn_ahead(&self, start: usize, cap: usize) {
        self.ahead.size(Depth::new(start, cap));
    }

    /// Each init parked from here on has the proc of its own pid namespace made with it, through `maker`, for its
    /// owner's last spec (decision 8, parked procs), at most `bound` alive at once ([`Sizes::procs_alive`]), and the
    /// dropper started with a queue of as many. Set before the node's first create; unset, none is made. Set again once the
    /// dropper runs, it sets `bound`, refused past the dropper's queue (so a tree handed to it never finds it full).
    pub fn set_parked_procs(&self, maker: ProcMaker, bound: usize) -> Result<(), Refusal> {
        let parking = &self.parking;
        let bound = u64::try_from(bound).unwrap_or(u64::MAX);
        if parking.drops.get().is_some() {
            let capacity = parking.capacity.load(Ordering::Relaxed);
            if bound > capacity {
                return Err(Refusal::Invalid {
                    what: "parked procs' bound",
                    why: format!(
                        "{bound} alive is past the dropper's queue of {capacity}, started already"
                    ),
                });
            }
            parking.bound.store(bound, Ordering::Relaxed);
            return Ok(());
        }
        let (drops, unattached) = mpsc::sync_channel(usize::try_from(bound).unwrap_or(usize::MAX));
        let (dropped, alive) = (Arc::clone(&parking.dropped), Arc::clone(&parking.alive));
        std::thread::Builder::new()
            .name("hex-proc-drop".to_owned())
            .spawn(move || drop_each(&unattached, &dropped, &alive))
            .map_err(|e| Refusal::os("starting the unattached procs' dropper", e))?;
        let _ = parking.drops.set(ProcSink::new(
            drops,
            Arc::clone(&parking.queued),
            Arc::clone(&parking.dropped_inline),
            Arc::clone(&parking.alive),
        ));
        parking.bound.store(bound, Ordering::Relaxed);
        parking.capacity.store(bound, Ordering::Relaxed);
        let _ = parking.procs.set(maker);
        Ok(())
    }

    /// Whether parks make procs now.
    pub fn makes_parked_procs(&self) -> bool {
        self.parking.makes_procs()
    }

    /// Whether boxes of no owner take cgroups made ahead.
    pub fn takes_ahead(&self) -> bool {
        self.ahead.bound.load(Ordering::Relaxed) > 0
    }

    /// The oldest cgroup made ahead, with its init, taken for a box of no owner; `None` when none is ready
    /// (counted dry). The refill is told, to make another.
    fn take_ahead(&self) -> Option<(MadeAhead, Parked)> {
        let taken = loop {
            let next = self.ahead.ready().ok()?.pop_front();
            let Some(made) = next else {
                break None;
            };
            match self.parking.take(made.n) {
                Ok(Some((p, _))) => break Some((made, p)),
                // Its init is gone: the cgroup goes too, and the next is tried; the queue is bounded.
                _ => self.remove_later(made.cg),
            }
        };
        // A take wakes the refill once its create has done its cgroup's file work ([`Cgroups::wake_refill`]): the refill's
        // mkdir takes the hierarchy's lock to write, and a lookup the create made after it would queue behind it
        // (goal16_ab.md, "Whose work the no-owner tail waits on"). With none ready it is woken now.
        self.ahead.took(taken.is_none());
        match &taken {
            Some(_) => {
                self.ahead.taken.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.ahead.dry.fetch_add(1, Ordering::Relaxed);
                self.ahead.want.notify_all();
            }
        }
        taken
    }

    /// The refill is told a cgroup made ahead was taken, to make another: by the create that took it, once that
    /// create's own work on the cgroup's files is done.
    fn wake_refill(&self) {
        self.ahead.want.notify_all();
    }

    fn pool(&self) -> Result<MutexGuard<'_, Pool<Policy>>, Refusal> {
        self.pool
            .lock()
            .map_err(|_| Refusal::Broken("the node's cgroup pool's lock was poisoned".to_owned()))
    }

    fn held(&self) -> Result<MutexGuard<'_, BTreeMap<BoxId, Held>>, Refusal> {
        self.held
            .lock()
            .map_err(|_| Refusal::Broken("the node's held slots' lock was poisoned".to_owned()))
    }

    /// Box `id`'s slot, while it lives.
    pub fn slot_of(&self, id: &BoxId) -> Result<Option<u64>, Refusal> {
        Ok(self.held()?.get(id).map(|h| h.slot))
    }

    /// The slots live boxes hold, in order (joint plan step 14): a slot none holds is idle, and keeps the caps its
    /// last box left, as [`Cgroups::give_back`] writes no bound, so the charge learner books it at nothing.
    pub fn held_slots(&self) -> Result<Vec<u64>, Refusal> {
        let held = self.held()?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(held.len())
            .map_err(|_| Refusal::Exhausted {
                what: "the node's held slots",
                bound: held.len(),
            })?;
        slots.extend(held.values().map(|h| h.slot));
        slots.sort_unstable();
        Ok(slots)
    }

    /// Box `id`'s usage from its slot's cgroup `cg`, reported since it was given the slot; `None` for a
    /// box holding no slot.
    pub fn usage(&self, id: &BoxId, cg: &BoxCgroup) -> Result<Option<Usage>, Refusal> {
        let held = self.held()?;
        Ok(held
            .get(id)
            .map(|h| cg.usage_since(&h.baseline, h.peak.as_ref())))
    }

    /// Box `id`, made, holds the slot its create chose.
    pub fn hold(&self, id: &BoxId, held: Held) -> Result<(), Refusal> {
        self.held()?.insert(id.clone(), held);
        Ok(())
    }

    /// `cgroup.kill` is about to be written to box `id`'s cgroup (a signal KILL to it): the slot it holds,
    /// if any, is removed with the box rather than kept ([`Held::killed`]).
    pub fn killed_in(&self, id: &BoxId) -> Result<(), Refusal> {
        if let Some(held) = self.held()?.get_mut(id) {
            held.killed = true;
        }
        Ok(())
    }

    /// Box `id`, taken back after a restart, holds slot `slot` its record names, at `baseline`: its peak
    /// is read from its adoption on, and its cgroup is removed with it, since the device policy its
    /// cgroup carries isn't known.
    pub fn adopt(
        &self,
        id: &BoxId,
        slot: u64,
        baseline: Baseline,
        cg: &BoxCgroup,
    ) -> Result<(), Refusal> {
        let peak = cg.peak_from_now()?;
        self.hold(
            id,
            Held {
                slot,
                baseline,
                peak,
                owner: String::new(),
                policy: None,
                killed: false,
                park_flags: None,
                park_proc_key: None,
            },
        )
    }

    /// Box `id`'s slot, let go by its delete: what it holds, if anything.
    pub fn release(&self, id: &BoxId) -> Result<Option<Held>, Refusal> {
        Ok(self.held()?.remove(id))
    }

    /// Slot `cg`, held as `held`, whose box has ended and whose record is gone: idle for its owner when
    /// its device policy is known, else removed. What the pool's bounds put out is removed off create's
    /// path.
    pub fn give_back(&self, cg: BoxCgroup, held: Held, root: &BoxRoot) -> Result<(), Refusal> {
        if held.killed {
            self.discarded_killed.fetch_add(1, Ordering::Relaxed);
            return cg.remove();
        }
        let Some(policy) = held.policy else {
            return cg.remove();
        };
        // An init made ahead in the slot for the owner's next box, before the pool can hand the slot to one:
        // the clone into the cgroup, and its wait for the host-wide `cgroup_mutex`, are this delete's, never the
        // owner's next create's (decision 8, "The p99.9 that stayed"). One that can't be made leaves the slot
        // idle and empty, as before, counted.
        if let (Some(flags), Some(parker)) = (held.park_flags, self.parker.get()) {
            match park_counted(&self.parking, parker, cg.dir(), flags) {
                Ok(mut made) => {
                    self.parking.with_proc(
                        &mut made.0,
                        held.park_proc_key.as_ref(),
                        MadeBy::Delete,
                    );
                    self.parking.parked.fetch_add(1, Ordering::Relaxed);
                    self.parking.slots()?.insert(held.slot, made);
                }
                Err(_) => {
                    self.parking.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let out = self.pool()?.give(&held.owner, held.slot, policy);
        for &n in &out {
            self.parking.end(n);
            if let Some(old) = root.slot(n) {
                self.remove_later(old);
            }
        }
        Ok(())
    }

    /// Hands `cg` to the remover; with its queue full, removes it here.
    pub fn remove_later(&self, cg: BoxCgroup) {
        match self.remover.try_send(cg) {
            Ok(()) => {}
            Err(TrySendError::Full(cg) | TrySendError::Disconnected(cg)) => {
                self.removal.inline.fetch_add(1, Ordering::Relaxed);
                // Counted and kept as the last failure when it fails; recovery removes what's left.
                let _ = self.removal.remove(&cg);
            }
        }
    }

    /// Every idle cgroup, let go and removed before this returns: the node's caller's to call when it
    /// wants none left waiting (a test's end, a shutdown). Each is tried; the first that couldn't be
    /// removed is the refusal. Recovery removes any a crash leaves.
    pub fn drain(&self, root: &BoxRoot) -> Result<(), Refusal> {
        let out = self.pool()?.drain();
        let mut first = None;
        // None is made ahead from here on, and what is is removed.
        self.ahead.size(Depth::new(0, 0));
        let _made = self.ahead.busy.lock().map_err(|_| {
            Refusal::Broken("the cgroups-ahead refill's lock was poisoned".to_owned())
        })?;
        let ahead: Vec<MadeAhead> = self.ahead.ready()?.drain(..).collect();
        for made in ahead {
            self.parking.end(made.n);
            if let Err(e) = self.removal.remove(&made.cg) {
                first.get_or_insert(e);
            }
        }
        for n in out {
            self.parking.end(n);
            if let Some(cg) = root.slot(n)
                && let Err(e) = self.removal.remove(&cg)
            {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// An `oci` box with no owner was given a cgroup of its own.
    pub fn no_owner(&self) {
        self.no_owner.fetch_add(1, Ordering::Relaxed);
    }

    pub fn ended_through_init(&self) {
        self.through_init.fetch_add(1, Ordering::Relaxed);
    }

    pub fn fell_back(&self) {
        self.fell_back.fetch_add(1, Ordering::Relaxed);
    }

    pub fn killed(&self) {
        self.cgroup_kill.fetch_add(1, Ordering::Relaxed);
    }

    pub fn kills(&self) -> Kills {
        Kills {
            through_init: self.through_init.load(Ordering::Relaxed),
            fell_back: self.fell_back.load(Ordering::Relaxed),
            cgroup_kill: self.cgroup_kill.load(Ordering::Relaxed),
        }
    }

    /// What it has learned (Ada's rule, 2026-11-06): the most idle cgroups an owner keeps, and how many cgroups are
    /// kept made ahead for boxes of no owner.
    pub fn learned(&self) -> Result<(usize, usize), Refusal> {
        let per_owner = self.pool()?.per_owner();
        let ahead = usize::try_from(self.ahead.bound.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        Ok((per_owner, ahead))
    }

    pub fn counts(&self) -> Result<Counts, Refusal> {
        let mut pool = self.pool()?.counts();
        let no_owner = pool.refused.entry(Refused::NoOwner).or_default();
        *no_owner = no_owner.saturating_add(self.no_owner.load(Ordering::Relaxed));
        Ok(Counts {
            pool,
            parked: self.parking.counts(),
            ahead: self.ahead.counts(),
            discarded_killed: self.discarded_killed.load(Ordering::Relaxed),
            removed: self.removal.removed.load(Ordering::Relaxed),
            removed_inline: self.removal.inline.load(Ordering::Relaxed),
            removals_failed: self.removal.failed.load(Ordering::Relaxed),
            last_failure: self
                .removal
                .last
                .lock()
                .map_err(|_| Refusal::Broken("the cgroup remover's lock was poisoned".to_owned()))?
                .clone(),
        })
    }
}

/// The remover's thread: ends and removes each cgroup it's handed, until the node's end closes its
/// queue.
impl Drop for Cgroups {
    /// The refill ends with the node; what it made ahead is removed by the node's drain or its recovery.
    fn drop(&mut self) {
        self.ahead.shut.store(true, Ordering::Relaxed);
        self.ahead.want.notify_all();
    }
}

/// The dropper's thread: drops each unattached proc it's handed, so its unmount waits here, until the node's end
/// closes its queue (every sink with it).
fn drop_each(queue: &Receiver<PreparedProc>, dropped: &AtomicU64, alive: &AtomicU64) {
    while let Ok(proc) = queue.recv() {
        drop(proc);
        dropped.fetch_add(1, Ordering::Relaxed);
        let _ = alive.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

fn remove_each(queue: &Receiver<BoxCgroup>, removal: &Removal) {
    while let Ok(cg) = queue.recv() {
        // Counted and kept as the last failure when it fails; recovery removes what's left.
        let _ = removal.remove(&cg);
    }
}

/// The most sub-cgroups [`end_and_remove`] takes apart under a cgroup whose `cgroup.stat` reads `stat`: the kernel's
/// own count of its live descendants, `nr_descendants` (Linux Documentation/admin-guide/cgroup-v2.rst,
/// "cgroup.stat"; v6.19 kernel/cgroup/cgroup.c `cgroup_stat_show`), read before the walk, itself bounded by the
/// cgroup's `cgroup.max.descendants`. A box that may change its cgroup never holds a slot, so a slot's are what
/// something outside the node made in it while it was idle; one made during the walk takes it past the count, and
/// the removal is refused, by type, for recovery to finish (Ada's rule, 2026-11-06: no picked bound). `None` when
/// the text gives no count.
fn sub_cgroups(stat: &str) -> Option<usize> {
    hex_core::cgroup::flat(stat, "nr_descendants").and_then(|n| usize::try_from(n).ok())
}

/// Ends whatever runs in `cg` (a cgroup no box holds now: idle, refused, or a failed create's) and
/// removes it with any sub-cgroups it holds, deepest first: a flat walk, bounded (CLAUDE.md §2, §11).
fn end_and_remove(cg: &BoxCgroup) -> Result<(), Refusal> {
    if !cg.dir().exists() {
        return Ok(());
    }
    if cg.populated()? {
        let killed = std::time::Instant::now();
        cg.kill()?;
        // The same learned wait as a delete's for its box to empty, which this answer teaches too.
        let (emptied, patience) = crate::node::wait_empty(cg, killed)?;
        if !emptied {
            return Err(Refusal::Os {
                op: "removing an idle cgroup",
                why: format!(
                    "{} still holds processes {patience:?} after cgroup.kill",
                    cg.dir().display()
                ),
            });
        }
    }
    let stat = match std::fs::read_to_string(cg.dir().join("cgroup.stat")) {
        Ok(stat) => stat,
        // Removed meanwhile: nothing is left to take apart.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Refusal::os("reading an idle cgroup's cgroup.stat", e)),
    };
    let most = sub_cgroups(&stat).ok_or_else(|| Refusal::Os {
        op: "reading an idle cgroup's cgroup.stat",
        why: format!("{} counts no nr_descendants", cg.dir().display()),
    })?;
    let mut found = Vec::new();
    let mut queue = vec![cg.dir().to_path_buf()];
    while let Some(dir) = queue.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Refusal::os("listing an idle cgroup's sub-cgroups", e)),
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| Refusal::os("listing an idle cgroup's sub-cgroups", e))?;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if found.len() >= most {
                    return Err(Refusal::Full(most));
                }
                found.push(entry.path());
                queue.push(entry.path());
            }
        }
    }
    // Found parents first, so taken apart in reverse every child goes before its parent.
    for dir in found.iter().rev() {
        BoxCgroup::at(dir.clone()).remove()?;
    }
    cg.remove()
}

/// What a create's [`Choice`] gave its box.
#[derive(Debug)]
pub enum Made {
    /// A slot: its cgroup may be handed on after the box.
    Slot(Held),
    /// A cgroup of the box's own, named by its id, for this reason.
    Own(Refused),
}

/// An idle cgroup handed on: the cgroup, the slot as the box holds it, and the init made ahead in it, if any.
type HandedOn = (BoxCgroup, Held, Option<Parked>);

/// One create's choice of cgroup, made when the runtime has read the box's spec ([`Choose`]).
pub struct Choice<'a> {
    pub cgroups: &'a Cgroups,
    /// The oci boxes' root, which slots and the boxes' own cgroups are made under.
    pub root: &'a BoxRoot,
    pub id: &'a BoxId,
    /// The box's owner; `None` for a box of no owner, which takes a cgroup made ahead when one is ready.
    pub owner: Option<&'a BoxOwner>,
    pub bounds: &'a Bounds,
    made: Mutex<Option<Made>>,
    /// Whether it handed the create an init parked in the cgroup, so one the create doesn't claim (a flag mismatch,
    /// a refused claim) is counted ended unclaimed.
    gave_parked: AtomicBool,
    /// A refusal its choosing met (the id taken since, the node full, a bound's controller missing),
    /// kept as it was so the create is refused by its type, not as the runtime's error that carried it.
    refused: Mutex<Option<Refusal>>,
    /// A debug build's crash point: the node ends abruptly once a cgroup of the box's own is made, as it does
    /// when it makes one itself (`CrashPoint::CgroupMade`), so recovery is tested on this path too.
    #[cfg(debug_assertions)]
    crash_on_own: bool,
}

impl<'a> Choice<'a> {
    pub fn new(
        cgroups: &'a Cgroups,
        root: &'a BoxRoot,
        id: &'a BoxId,
        owner: Option<&'a BoxOwner>,
        bounds: &'a Bounds,
    ) -> Self {
        Self {
            cgroups,
            root,
            id,
            owner,
            bounds,
            made: Mutex::new(None),
            gave_parked: AtomicBool::new(false),
            refused: Mutex::new(None),
            #[cfg(debug_assertions)]
            crash_on_own: false,
        }
    }

    /// The choice, ending the node abruptly once it makes a cgroup of the box's own when `crash` (a debug build's
    /// crash point).
    #[cfg(debug_assertions)]
    pub fn crashing_on_own(mut self, crash: bool) -> Self {
        self.crash_on_own = crash;
        self
    }

    /// A release build has no crash points: the choice as it is.
    #[cfg(not(debug_assertions))]
    pub fn crashing_on_own(self, _crash: bool) -> Self {
        self
    }

    /// What it gave the box, once the runtime asked, and the refusal it met if one ended the create.
    pub fn made(self) -> (Option<Made>, Option<Refusal>) {
        (
            self.made.into_inner().ok().flatten(),
            self.refused.into_inner().ok().flatten(),
        )
    }

    /// `r`, kept as it is, carried through the runtime as its error.
    fn refuse_create(&self, r: Refusal) -> OciError {
        let carried = OciError::Broken(r.to_string());
        if let Ok(mut kept) = self.refused.lock() {
            *kept = Some(r);
        }
        carried
    }

    fn record(&self, made: Made) -> Result<(), OciError> {
        *self.made.lock().map_err(|_| {
            OciError::Broken("a create's cgroup choice's lock was poisoned".to_owned())
        })? = Some(made);
        Ok(())
    }

    /// Idle slot `n`'s cgroup, checked and given the box's bounds again, with the init made ahead in it if
    /// one is (decision 8, "The p99.9 that stayed"), or why it can't be handed on. A cgroup with an init in it is
    /// checked with that init the one process in it ([`reuse::check_parked`]).
    fn hand_on(&self, n: u64) -> Result<HandedOn, (Option<BoxCgroup>, Refused)> {
        let Some(cg) = self.root.slot(n) else {
            self.cgroups.parking.end(n);
            return Err((None, Refused::Unreadable));
        };
        let parked = match self.cgroups.parking.take(n) {
            Ok(parked) => parked,
            Err(_) => return Err((Some(cg), Refused::Unreadable)),
        };
        let files = match cg.idle_files() {
            Ok(files) => files,
            Err(_) => return Err((Some(cg), Refused::Unreadable)),
        };
        let checked = match &parked {
            None => reuse::check(&files, self.bounds.memory_max, RESIDUAL),
            Some((p, charge)) => {
                let alone = cg.procs().is_ok_and(|procs| {
                    p.pid()
                        .and_then(|pid| u32::try_from(pid).ok())
                        .is_some_and(|pid| procs == [pid])
                });
                if alone {
                    reuse::check_parked(&files, self.bounds.memory_max, RESIDUAL, *charge)
                } else {
                    Err(Refused::Populated)
                }
            }
        };
        if let Err(reason) = checked {
            if parked.is_some() {
                self.cgroups.parking.ended.fetch_add(1, Ordering::Relaxed);
            }
            return Err((Some(cg), reason));
        }
        let parked = parked.map(|(p, _)| p);
        // Every bound again, then the counters this box's usage is reported from, then its peak's
        // descriptor: a cgroup none of these can be done to goes, and the box gets a fresh one.
        if self.root.rebound(&cg, self.bounds).is_err() {
            return Err((Some(cg), Refused::Unreadable));
        }
        let baseline = cg.baseline();
        let peak = match cg.peak_from_now() {
            Ok(peak) => peak,
            Err(_) => return Err((Some(cg), Refused::Unreadable)),
        };
        let held = Held {
            slot: n,
            baseline,
            peak,
            owner: self.owner_name(),
            policy: None,
            killed: false,
            park_flags: None,
            park_proc_key: None,
        };
        Ok((cg, held, parked))
    }

    fn owner_name(&self) -> String {
        self.owner.map(|o| o.name.clone()).unwrap_or_default()
    }

    /// A cgroup made ahead with its init parked in it, given the box's bounds: the slot as the box holds it, its policy
    /// `None` so it is removed with the box and never handed on, or the cgroup back when it can't be bounded. The slot is
    /// fresh, made with no bound ([`refill_ahead`]), so the box's own bounds are all it is given, as a cgroup made on the
    /// create's path is ([`BoxRoot::bound`]); a cgroup an earlier box left is rebound instead. Its usage's baseline and
    /// peak are the refill's ([`MadeAhead`]): the take reads none of its counters.
    fn ready_ahead(&self, made: MadeAhead) -> Result<(BoxCgroup, Held), BoxCgroup> {
        let MadeAhead {
            n,
            cg,
            baseline,
            peak,
        } = made;
        if self.root.bound(&cg, self.bounds).is_err() {
            return Err(cg);
        }
        let held = Held {
            slot: n,
            baseline,
            peak,
            owner: String::new(),
            policy: None,
            killed: false,
            park_flags: None,
            park_proc_key: None,
        };
        Ok((cg, held))
    }
}

impl Choose for Choice<'_> {
    fn choose(&self, wants: &Wants<'_>) -> Result<Chosen, OciError> {
        let policy: Policy = wants.program.map(<[Insn]>::to_vec);
        if self.owner.is_none()
            && let Ok(mut key) = self.cgroups.ahead.proc_key.lock()
        {
            *key = wants.proc_key.cloned();
        }
        let whose = Whose {
            owner: self.owner.map(|o| o.name.as_str()),
            reuse: self.owner.is_some_and(|o| o.reuse),
            privileged: wants.privileged,
        };
        // Each refused idle cgroup leaves the pool, so the takes end within the node's bound on them.
        let most = self.cgroups.most_idle;
        for _ in 0..=most {
            let take = {
                let mut pool = self.cgroups.pool().map_err(|r| self.refuse_create(r))?;
                let before = pool.per_owner();
                let take = pool.take(whose, &policy);
                if pool.per_owner() > before {
                    crate::sizes::rose();
                }
                take
            };
            match take {
                Take::Own(reason) => {
                    // A box of no owner takes a cgroup made ahead, with its init in it, when one is ready.
                    if reason == Refused::NoOwner
                        && let Some((made, parked)) = self.cgroups.take_ahead()
                    {
                        let ready = self.ready_ahead(made);
                        self.cgroups.wake_refill();
                        match ready {
                            Ok((cgroup, held)) => {
                                self.record(Made::Slot(held))?;
                                self.gave_parked.store(true, Ordering::Relaxed);
                                return Ok(Chosen {
                                    cgroup,
                                    attached: false,
                                    parked: Some(parked),
                                });
                            }
                            Err(cg) => {
                                drop(parked);
                                self.cgroups.remove_later(cg);
                            }
                        }
                    }
                    let cgroup = self
                        .root
                        .create(self.id, self.bounds)
                        .map_err(|r| self.refuse_create(r))?;
                    #[cfg(debug_assertions)]
                    if self.crash_on_own {
                        let _ = rustix::process::kill_process(
                            rustix::process::getpid(),
                            rustix::process::Signal::KILL,
                        );
                    }
                    self.record(Made::Own(reason))?;
                    return Ok(Chosen {
                        cgroup,
                        attached: false,
                        parked: None,
                    });
                }
                Take::Fresh(n) => {
                    let cgroup = self
                        .root
                        .create_slot(n, self.bounds)
                        .map_err(|r| self.refuse_create(r))?;
                    self.record(Made::Slot(Held {
                        slot: n,
                        baseline: Baseline::default(),
                        peak: None,
                        owner: self.owner_name(),
                        policy: Some(policy),
                        killed: false,
                        park_flags: None,
                        park_proc_key: wants.proc_key.cloned(),
                    }))?;
                    return Ok(Chosen {
                        cgroup,
                        attached: false,
                        parked: None,
                    });
                }
                Take::Idle(n) => match self.hand_on(n) {
                    Ok((cgroup, mut held, parked)) => {
                        self.cgroups
                            .pool()
                            .map_err(|r| self.refuse_create(r))?
                            .hand_on();
                        held.policy = Some(policy);
                        held.park_proc_key = wants.proc_key.cloned();
                        self.record(Made::Slot(held))?;
                        self.gave_parked.store(parked.is_some(), Ordering::Relaxed);
                        return Ok(Chosen {
                            cgroup,
                            attached: true,
                            parked,
                        });
                    }
                    Err((cg, reason)) => {
                        self.cgroups
                            .pool()
                            .map_err(|r| self.refuse_create(r))?
                            .refuse(reason);
                        if let Some(cg) = cg {
                            self.cgroups.remove_later(cg);
                        }
                    }
                },
            }
        }
        Err(OciError::Broken(format!(
            "box {} found more than {most} of its owner's idle cgroups refused in one create",
            self.id
        )))
    }

    fn init_made(&self, flags: u64, claimed: bool) {
        if claimed {
            self.cgroups.parking.claimed.fetch_add(1, Ordering::Relaxed);
        } else if self.gave_parked.load(Ordering::Relaxed) {
            // Handed one, but made with other namespaces than this create's, or its claim refused: ended.
            self.cgroups.parking.ended.fetch_add(1, Ordering::Relaxed);
        }
        // A box of no owner's flags are those the cgroups made ahead park their inits with; the first known
        // starts the refill.
        if self.owner.is_none() && self.cgroups.ahead.flags.swap(flags, Ordering::Relaxed) != flags
        {
            self.cgroups.ahead.want.notify_all();
        }
        if let Ok(mut made) = self.made.lock()
            && let Some(Made::Slot(held)) = made.as_mut()
        {
            held.park_flags = Some(flags);
        }
    }

    fn parked_proc(&self, outcome: ParkedProc) {
        let parking = &self.cgroups.parking;
        let count = match outcome {
            ParkedProc::Hit => {
                // Handed to the init that claimed it: no longer the node's.
                parking.release_alive();
                &parking.proc_hits
            }
            ParkedProc::Miss => &parking.proc_misses,
            // Dry only means something where the node makes them.
            ParkedProc::Dry if parking.makes_procs() => &parking.proc_dry,
            ParkedProc::Dry => return,
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ada's rule, 2026-11-06 (T-T7.1.2): `REMOVE_DIRS`' 256 was picked; the walk's bound is the kernel's count of the
    /// cgroup's sub-cgroups. Do: read the counts as the kernel writes `cgroup.stat`, and hostile text. Expect: the
    /// live descendants' count, and none where the text gives none.
    #[test]
    fn the_walks_bound_is_the_kernels_count_of_sub_cgroups() {
        assert_eq!(
            sub_cgroups("nr_descendants 3\nnr_subsys_cpu 1\nnr_dying_descendants 7\n"),
            Some(3)
        );
        assert_eq!(sub_cgroups("nr_descendants 0\n"), Some(0));
        for bad in ["", "nr_descendants x\n", "nr_dying_descendants 2\n"] {
            assert_eq!(sub_cgroups(bad), None, "{bad:?}");
        }
    }
}
