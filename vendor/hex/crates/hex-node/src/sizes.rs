//! The bounds of the node's pools, derived from the host it starts on, and where their learned sizes start (Ada's
//! rule, 2026-11-06: "We do not support arbitrary constants. We support learned and data derived values"; the
//! lead's calls the same day; docs/design/runtime-architecture.md, "Pool sizes learned from demand"). Each pool
//! learns its size from the demand it sees ([`hex_core::demand`]). It starts at the peak the node's last start
//! learned, kept in its state ([`Keeper`]), or on a node's first start at the host's parallelism; it stops at its
//! bound, the count of its items that fits in its share of the memory the node may hold, at the item's measured
//! cost, each pool's share in proportion to its takes times that cost (CLAUDE.md §2).
//!
//! [`Sizes::of`] is pure; [`Host::now`] reads the host's facts, and [`read_peaks`] the node's last peaks.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use hex_api::Refusal;
use hex_core::demand::{MEMORY_POOLS, Peaks, cgroup_headroom, mem_available, weighted};

/// An idle cgroup kept for its owner's next box: what the bench's lifecycle leaves charged to it, at most
/// 1,748,992 bytes over 256 idle cgroups of fedora's OCI pass 20261103T044133Z
/// (bench/competitive/probes/reuse_residual.md), and the init parked in it for that box, about 24.7 KB an init with
/// reuse on (bench/competitive/probes/create_tail.md, "What a park charged its cgroup"): 1,773,692 bytes. The check
/// admits up to [`crate::cgroups::RESIDUAL`] a cgroup; a node whose idle cgroups hold more than measured here is what
/// would falsify this bound.
pub const IDLE_CGROUP_BYTES: u64 = 1_773_692;
/// A cgroup made ahead for a box of no owner with its init parked in it: about 262 KB an init in a fresh cgroup
/// (bench/competitive/probes/create_tail.md, "What a park charged its cgroup", reuse off).
pub const AHEAD_BYTES: u64 = 262_000;
/// A network namespace held, the most the host's Slab and Percpu rose for one: 762.0 KiB on the Mac Studio, the
/// heavier of the two machines measured (bench/competitive/probes/netns_pool.md).
pub const NETNS_BYTES: u64 = 780_288;
/// A create seed, its largest `VmRSS` measured: 2,488 KiB on fedora (bench/competitive/probes/seed_path.md).
pub const SEED_BYTES: u64 = 2_547_712;
/// An ipc and uts namespace pair made ahead, the most the host's Slab rose for one: 14.1 KiB on fedora
/// (bench/competitive/probes/netns_pool.md, goal 5 extended step 4).
pub const IPC_UTS_BYTES: u64 = 14_438;
/// A device program a node holds loaded, what a loaded program charged its loader's cgroup: one vmalloc'd page and
/// 1.20 KiB of per-CPU `stats` and `active`, 5.20 KiB, the same in every round of bench/competitive/RESULT.md's
/// per-container attribution (adacachy, `20261027T033009Z`, `kernel/bpf/core.c:102-118,149-159`), and gone once one
/// program was shared (`20261027T034522Z`).
pub const PROGRAM_BYTES: u64 = 5_325;

/// The pools that hold the node's memory, by their items' cost, in the order [`Peaks::takes`] keeps their takes:
/// idle cgroups, cgroups made ahead, idle network namespaces, network namespaces made ahead (decision 4), seeds,
/// ipc and uts pairs, and device programs held (their takes the programs loaded, one per new policy).
const COSTS: [u64; MEMORY_POOLS] = [
    IDLE_CGROUP_BYTES,
    AHEAD_BYTES,
    NETNS_BYTES,
    NETNS_BYTES,
    SEED_BYTES,
    IPC_UTS_BYTES,
    PROGRAM_BYTES,
];

/// What the node reads of its host as it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// The memory the node may hold: the host's `MemAvailable`, or less where a cgroup the node is in, or one
    /// above it, leaves less under its `memory.max`.
    pub memory: u64,
    /// The hardware threads the node may run on (`std::thread::available_parallelism`, which reads the process's
    /// affinity and its cgroup's `cpu.max`).
    pub parallelism: NonZeroUsize,
    /// The most threads the node may have: the least of its soft `RLIMIT_NPROC`, which counts its user's threads
    /// (getrlimit(2)), and `/proc/sys/kernel/threads-max`, the host's (proc(5)); `u64::MAX` where neither bounds it.
    pub threads: u64,
    /// The most descriptors the node may hold: its hard `RLIMIT_NOFILE`, which the node's start raises its soft limit
    /// to (getrlimit(2), `netns::hold_descriptors`), or where that is unlimited `/proc/sys/fs/nr_open`, the most the
    /// kernel lets any process open (proc(5)); `u64::MAX` where neither can be read.
    pub descriptors: u64,
}

impl Host {
    /// The host's facts now. Refused when `/proc/meminfo` gives no `MemAvailable` or the parallelism can't be read:
    /// a node that can't say what it may hold sizes nothing.
    pub fn now() -> Result<Self, Refusal> {
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| Refusal::os("reading /proc/meminfo", e))?;
        let available = mem_available(&meminfo).ok_or_else(|| {
            Refusal::Broken("/proc/meminfo gives no MemAvailable in kB".to_owned())
        })?;
        let memory = cgroup_headrooms().into_iter().fold(available, u64::min);
        let parallelism = std::thread::available_parallelism()
            .map_err(|e| Refusal::os("reading the host's parallelism", e))?;
        let number = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
        };
        let nproc = rustix::process::getrlimit(rustix::process::Resource::Nproc).current;
        let threads = [nproc, number("/proc/sys/kernel/threads-max")]
            .into_iter()
            .flatten()
            .fold(u64::MAX, u64::min);
        let descriptors = rustix::process::getrlimit(rustix::process::Resource::Nofile)
            .maximum
            .or_else(|| number("/proc/sys/fs/nr_open"))
            .unwrap_or(u64::MAX);
        Ok(Self {
            memory,
            parallelism,
            threads,
            descriptors,
        })
    }
}

/// What each cgroup from the node's own up to the root leaves under its `memory.max`, where it sets one: a walk up
/// the path `/proc/self/cgroup` names, one step a component, so it ends.
fn cgroup_headrooms() -> Vec<u64> {
    let Ok(own) = std::fs::read_to_string("/proc/self/cgroup") else {
        return Vec::new();
    };
    let Some(path) = own.lines().find_map(|l| l.strip_prefix("0::")) else {
        return Vec::new();
    };
    let mut dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let mut out = Vec::new();
    loop {
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
        if let (Some(max), Some(current)) = (read("memory.max"), read("memory.current"))
            && let Some(room) = cgroup_headroom(&max, &current)
        {
            out.push(room);
        }
        if dir.as_os_str() == "/sys/fs/cgroup" || !dir.pop() {
            return out;
        }
    }
}

/// Where each learned size starts: the peak the node's last start learned, never under the host's parallelism (or,
/// for the spare, one: the fewest by which a create finds a seed idle rather than waits on a seed's exec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starts {
    pub owner_cgroups: usize,
    pub owner_netns: usize,
    pub netns: usize,
    pub ipc_uts: usize,
    pub ahead: usize,
    pub devfs_keys: usize,
    pub devfs_per_key: usize,
    pub seeds: usize,
    pub spare: usize,
}

/// The node's pools' bounds, and where their learned sizes start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    /// The host's parallelism (32 on fedora, the bench host, so no pool starts smaller there than the constant it
    /// replaces).
    pub start: usize,
    /// Where each learned size starts.
    pub starts: Starts,
    /// The most idle cgroups the node keeps, all owners' together, and so the most any owner keeps.
    pub idle_cgroups: usize,
    /// The most idle network namespaces the node keeps, all owners' together.
    pub idle_netns: usize,
    /// The most create seeds.
    pub seeds: NonZeroUsize,
    /// The most cgroups made ahead for boxes of no owner. No more creates take one at once than hold a seed.
    pub ahead: usize,
    /// The most ipc and uts pairs made ahead. No more creates take one at once than hold a seed.
    pub ipc_uts: usize,
    /// The most network namespaces made ahead (decision 4). No more creates take one at once than hold a seed.
    pub netns: usize,
    /// The most workers the node's socket runs calls on, each started as a call finds none idle and kept: the
    /// threads the kernel lets the node have, one at least, and so the most calls the socket admits at once
    /// ([`crate::serve::limits`]), each with a worker that can exist. The bound the seeds' line of waiting creates
    /// is set by.
    pub workers: usize,
    /// The most connections the node's socket admits, a descriptor each ([`crate::serve::limits`]): the most
    /// descriptors the node may hold ([`Host::descriptors`]), which a started node lowers to what its programs' and
    /// calls' budgets admit, counting them among its own as it fits its bounds to its hard limit
    /// (`netns::hold_descriptors`).
    pub descriptors: usize,
    /// The most `oci` boxes the node holds at once, each keeping its network namespace from its create to its delete
    /// (`crate::netns`): the boxes a node holds at most ([`hex_runtime::MAX_BOXES`]), or fewer where its start fits its
    /// bounds to its hard descriptor limit (`netns::hold_descriptors`), each one more refused as `Refusal::Full`
    /// (`hex_runtime::linux::cgroup::BoxRoot::at_most`).
    pub boxes: usize,
    /// The most inits the node watches at once, a pidfd each (the supervisor's budget, and the programs the socket
    /// watches): one per live `oci` box, so at most the node's boxes ([`Sizes::boxes`]), and within the descriptors it
    /// may hold, its hard `RLIMIT_NOFILE` read at run time (getrlimit(2)), to which its start fits it with every other
    /// bound that holds descriptors (`netns::hold_descriptors`). It was `INIT_BUDGET`, 65,536, systemd's figure copied
    /// (Ada's 2026-11-06 constants rule).
    pub inits: usize,
    /// The most device programs the node holds loaded, one per device policy a create used, past which the one used
    /// longest ago is closed (`devshare::Programs`; it lives on in the cgroups it's attached to). A memory pool as the
    /// others are: what fits of [`PROGRAM_BYTES`] each in its demand-weighted share ([`weighted`], its takes the
    /// programs loaded), never under the host's parallelism, a descriptor each, which its start fits to its hard
    /// descriptor limit with every other bound that holds them (`netns::hold_descriptors`).
    /// Grown on a miss, a program loaded per new policy. At most one per box the node holds
    /// ([`hex_runtime::MAX_BOXES`]): no more policies are in use at once, and each held is a descriptor of the
    /// node's, counted among its own (`netns::FilesBound`). No peak is kept for a next start: nothing is made ahead, a
    /// policy being known only at its first create, so a start has nothing to begin from but its first create's
    /// load. It was a fixed 64 (`devshare::MAX_PROGRAMS`, Ada's 2026-11-06 constants rule).
    pub programs: usize,
}

impl Sizes {
    /// The bounds on `host`, and the starts from `peaks`, a node's last start's (none on its first). Each memory
    /// pool's share follows its takes times its item's cost there, evenly where none was taken ([`weighted`]); its
    /// bound is what fits in that share, never under its peak nor the host's parallelism (latency first: a pool is
    /// never bounded below what it was seen to need, or below where a first start starts it).
    pub fn of(host: Host, peaks: &Peaks) -> Self {
        let demand: Vec<(u64, u64)> = peaks.takes.iter().copied().zip(COSTS).collect();
        let shares = weighted(host.memory, &demand);
        let start = host.parallelism.get();
        let fit = |pool: usize, peak: usize| {
            let share = shares.get(pool).copied().unwrap_or(0);
            let cost = COSTS.get(pool).copied().unwrap_or(u64::MAX);
            usize::try_from(share.checked_div(cost).unwrap_or(0))
                .unwrap_or(usize::MAX)
                .max(peak)
                .max(start)
        };
        let seeds = NonZeroUsize::new(fit(4, peaks.seeds)).unwrap_or(NonZeroUsize::MIN);
        let at = |peak: usize| start.max(peak);
        let to_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        Self {
            start,
            starts: Starts {
                owner_cgroups: at(peaks.owner_cgroups),
                owner_netns: at(peaks.owner_netns),
                netns: at(peaks.netns),
                ipc_uts: at(peaks.ipc_uts),
                ahead: at(peaks.ahead),
                devfs_keys: at(peaks.devfs_keys),
                devfs_per_key: at(peaks.devfs_per_key),
                seeds: at(peaks.seeds),
                spare: peaks.spare.max(1),
            },
            idle_cgroups: fit(0, peaks.owner_cgroups),
            ahead: fit(1, peaks.ahead).min(seeds.get().max(peaks.ahead)),
            idle_netns: fit(2, peaks.owner_netns),
            netns: fit(3, peaks.netns).min(seeds.get().max(peaks.netns)),
            seeds,
            ipc_uts: fit(5, peaks.ipc_uts).min(seeds.get().max(peaks.ipc_uts)),
            workers: to_usize(host.threads).max(1),
            descriptors: to_usize(host.descriptors),
            boxes: hex_runtime::MAX_BOXES,
            inits: hex_runtime::MAX_BOXES,
            programs: fit(6, 0).min(hex_runtime::MAX_BOXES),
        }
    }

    /// The bounds on this host as a node's first start has them, from its facts as this process first read them:
    /// what a caller reckons without a node's state.
    pub fn now() -> Result<Self, Refusal> {
        host().map(|h| Self::of(h, &Peaks::default()))
    }

    /// The bounds on this host for a node whose last peaks are `peaks`.
    pub fn learned(peaks: &Peaks) -> Result<Self, Refusal> {
        host().map(|h| Self::of(h, peaks))
    }

    /// What the node's socket is bounded by ([`crate::serve::limits`]): the descriptors it may hold, its workers'
    /// threads, and its inits' pidfd budget as the programs it watches.
    pub fn platform(&self) -> crate::serve::Platform {
        crate::serve::Platform {
            descriptors: self.descriptors,
            threads: self.workers,
            programs: self.inits,
        }
    }

    /// The most procs alive at once with parked inits (decision 8, parked procs): one an init parked, in an idle
    /// cgroup or one made ahead.
    pub fn procs_alive(&self) -> usize {
        self.idle_cgroups.saturating_add(self.ahead)
    }

    /// The memory the pools at these bounds hold at most.
    pub fn bytes(&self) -> u64 {
        let of = |n: usize, each: u64| u64::try_from(n).unwrap_or(u64::MAX).saturating_mul(each);
        [
            of(self.idle_cgroups, IDLE_CGROUP_BYTES),
            of(self.ahead, AHEAD_BYTES),
            of(self.idle_netns, NETNS_BYTES),
            of(self.netns, NETNS_BYTES),
            of(self.seeds.get(), SEED_BYTES),
            of(self.ipc_uts, IPC_UTS_BYTES),
            of(self.programs, PROGRAM_BYTES),
        ]
        .into_iter()
        .fold(0, u64::saturating_add)
    }
}

/// The host's facts as this process first read them: every node a process opens is bounded alike, and what a caller
/// reckons from them (the descriptors at the bounds, [`crate::netns::FilesBound`]) is what its node holds.
fn host() -> Result<Host, Refusal> {
    static READ: OnceLock<Host> = OnceLock::new();
    if let Some(host) = READ.get() {
        return Ok(*host);
    }
    let host = Host::now()?;
    Ok(*READ.get_or_init(|| host))
}

/// Learned sizes that rose in this process: each pool's shell adds one when its size does, which tells the node a
/// later peak is there to keep. A count, never a wait.
static RISES: AtomicU64 = AtomicU64::new(0);

/// A pool's learned size rose.
pub fn rose() {
    RISES.fetch_add(1, Ordering::Relaxed);
}

/// The rises so far.
pub fn rises() -> u64 {
    RISES.load(Ordering::Relaxed)
}

/// The name of the node's record of its peaks in its state directory.
const PEAKS: &str = "peaks";

/// What reading a node's last peaks found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Last {
    /// Its last start's peaks.
    Peaks(Peaks),
    /// None: the node's first start.
    First,
    /// A record that couldn't be read, and why: the start is a first start, counted ([`Keeper::unread`]).
    Unread(String),
}

/// The peaks the node in `state_dir` last kept, read whole: at most twice what this node writes
/// ([`Peaks::most_bytes`]), room for a later node's as many keys again; past it the record is refused unread.
pub fn read_peaks(state_dir: &Path) -> Last {
    use std::io::Read;
    let path = state_dir.join(PEAKS);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Last::First,
        Err(e) => return Last::Unread(format!("{}: {e}", path.display())),
    };
    let most = Peaks::most_bytes().saturating_mul(2);
    let mut text = String::new();
    let limit = u64::try_from(most).unwrap_or(u64::MAX).saturating_add(1);
    if let Err(e) = file.take(limit).read_to_string(&mut text) {
        return Last::Unread(format!("{}: {e}", path.display()));
    }
    if text.len() > most {
        return Last::Unread(format!("{}: longer than {most} bytes", path.display()));
    }
    match Peaks::decode(&text) {
        Ok(peaks) => Last::Peaks(peaks),
        Err(e) => Last::Unread(format!("{}: {e}", path.display())),
    }
}

/// Writes `peaks` as the node in `state_dir`'s record: whole to a temporary file, then renamed into place, so a
/// crash leaves the last record or this one, never part of one (rename(2)); not synced, as a box's record of the
/// runtime directory isn't (decision 1): a host crash at worst loses the last peaks, and the next start is a first
/// start or an earlier one's.
pub fn write_peaks(state_dir: &Path, peaks: &Peaks) -> Result<(), Refusal> {
    let tmp = state_dir.join(format!("{PEAKS}.tmp"));
    std::fs::write(&tmp, peaks.encode()).map_err(|e| Refusal::os("writing the node's peaks", e))?;
    std::fs::rename(&tmp, state_dir.join(PEAKS))
        .map_err(|e| Refusal::os("renaming the node's peaks into place", e))
}

/// Keeps the node's peaks in its state as they rise, off every create's path: a thread of its own writes each one
/// it is handed, at most one waiting for it (CLAUDE.md §2). An offer that finds one waiting is let go, and the
/// node offers again at its next create, a later snapshot holding every earlier peak, since none falls.
pub struct Keeper {
    dir: PathBuf,
    /// The peaks the node started from: what it keeps is never under them, and its takes add to theirs.
    base: Peaks,
    /// Why the last record couldn't be read, where it couldn't.
    unread: Option<String>,
    /// The rises the last peaks handed on held.
    offered: AtomicU64,
    writes: Option<SyncSender<Peaks>>,
    thread: Option<JoinHandle<()>>,
    last_failure: Arc<Mutex<Option<String>>>,
    written: Arc<AtomicU64>,
}

impl Keeper {
    /// The keeper of the node in `state_dir`, its last peaks read: those, or none on a first start, which an
    /// unreadable record makes it too, counted.
    pub fn open(state_dir: &Path) -> Result<Self, Refusal> {
        let (base, unread) = match read_peaks(state_dir) {
            Last::Peaks(p) => (p, None),
            Last::First => (Peaks::default(), None),
            Last::Unread(why) => (Peaks::default(), Some(why)),
        };
        let (writes, queue) = std::sync::mpsc::sync_channel::<Peaks>(1);
        let dir = state_dir.to_path_buf();
        let (theirs, last_failure) = (dir.clone(), Arc::new(Mutex::new(None)));
        let written = Arc::new(AtomicU64::new(0));
        let (failure, count) = (Arc::clone(&last_failure), Arc::clone(&written));
        let thread = std::thread::Builder::new()
            .name("hex-peaks".to_owned())
            .spawn(move || keep_each(&queue, &theirs, &failure, &count))
            .map_err(|e| Refusal::os("starting the node's peaks keeper", e))?;
        Ok(Self {
            dir,
            base,
            unread,
            offered: AtomicU64::new(0),
            writes: Some(writes),
            thread: Some(thread),
            last_failure,
            written,
        })
    }

    /// The peaks the node started from.
    pub fn base(&self) -> Peaks {
        self.base
    }

    /// Why the last record couldn't be read, where it couldn't: the start was a first start.
    pub fn unread(&self) -> Option<&str> {
        self.unread.as_deref()
    }

    /// Whether a size rose since the last peaks handed on: the node's word to take a snapshot.
    pub fn behind(&self) -> bool {
        rises() != self.offered.load(Ordering::Relaxed)
    }

    /// `now`, this start's peaks and takes as of `rises`, kept: never under the start's peaks, its takes added to
    /// the start's. Handed to the keeper's thread, never waited for; with one waiting, let go, so the node is still
    /// behind and offers again.
    pub fn offer(&self, now: &Peaks, rises: u64) {
        let Some(writes) = &self.writes else { return };
        if !matches!(
            writes.try_send(self.merged(now)),
            Err(TrySendError::Full(_))
        ) {
            self.offered.store(rises, Ordering::Relaxed);
        }
    }

    /// `now` kept against the start's peaks.
    fn merged(&self, now: &Peaks) -> Peaks {
        let b = &self.base;
        let mut takes = b.takes;
        for (t, n) in takes.iter_mut().zip(now.takes) {
            *t = t.saturating_add(n);
        }
        Peaks {
            owner_cgroups: b.owner_cgroups.max(now.owner_cgroups),
            owner_netns: b.owner_netns.max(now.owner_netns),
            netns: b.netns.max(now.netns),
            ipc_uts: b.ipc_uts.max(now.ipc_uts),
            ahead: b.ahead.max(now.ahead),
            devfs_keys: b.devfs_keys.max(now.devfs_keys),
            devfs_per_key: b.devfs_per_key.max(now.devfs_per_key),
            seeds: b.seeds.max(now.seeds),
            spare: b.spare.max(now.spare),
            takes,
        }
    }

    /// `now` written here and now, as the node ends: what a create's offer left waiting is superseded by it.
    pub fn finish(&mut self, now: &Peaks) -> Result<(), Refusal> {
        drop(self.writes.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        write_peaks(&self.dir, &self.merged(now))
    }

    /// Peaks written by the keeper's thread, and the last write's failure, if one failed.
    pub fn counts(&self) -> (u64, Option<String>) {
        (
            self.written.load(Ordering::Relaxed),
            self.last_failure.lock().ok().and_then(|l| l.clone()),
        )
    }
}

/// The keeper's thread: writes each peaks it is handed until the node's end closes its queue.
fn keep_each(
    queue: &Receiver<Peaks>,
    dir: &Path,
    failure: &Mutex<Option<String>>,
    written: &AtomicU64,
) {
    while let Ok(peaks) = queue.recv() {
        match write_peaks(dir, &peaks) {
            Ok(()) => {
                written.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                if let Ok(mut last) = failure.lock() {
                    *last = Some(e.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
