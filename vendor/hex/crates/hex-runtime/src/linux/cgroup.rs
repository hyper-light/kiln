//! Boxes as cgroup v2 directories under one root the node agent owns (docs/design/runtime.md, "The
//! process box"). The root's parent hands it the controllers boxes use, and the root hands them on
//! to every box; cgroup v2's no-internal-processes rule means neither may hold a process itself, so
//! the node agent runs beside the root, never in it.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hex_api::{Bounds, BoxId, Refusal, Signal, Usage};
use hex_core::cgroup::{self as core, Baseline, CONTROLLERS, Dev, UsageFiles};
use hex_core::reuse;
use hex_core::setup::{self, Undelegated};
use rustix::fs::{AtFlags, Mode, OFlags};

use super::os_err;

/// Where cgroup v2 is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub use crate::MAX_BOXES;
/// Most processes a signal is sent to one by one in a box (signals other than KILL, which the
/// kernel delivers to the whole box at once through `cgroup.kill`), and the kill sweep collects: no box
/// holds more than there are pids ([`PID_MAX_LIMIT`]).
pub const MAX_SIGNALLED: usize = PID_MAX_LIMIT;
/// The most cgroups a box's removal lists beneath it: Hex's own bound on what a container with a
/// writable cgroup2 mount may leave there (the kernel's `cgroup.max.descendants` is unlimited unless set),
/// as many as the boxes a node holds ([`MAX_BOXES`]); past it the removal is refused by type.
pub const MAX_DESCENDANTS: usize = MAX_BOXES;

/// `PID_MAX_LIMIT` on a 64-bit kernel, the most pids there are: the ceiling `pid_max` may be set to
/// (include/linux/threads.h; proc(5), /proc/sys/kernel/pid_max).
const PID_MAX_LIMIT: usize = 4 * 1024 * 1024;

/// The most bytes a box's `cgroup.procs` holds: a line for each of [`MAX_SIGNALLED`] processes, each pid at most
/// `PID_MAX_LIMIT - 1`, seven digits, and a newline.
const MAX_PROCS_FILE: usize = MAX_SIGNALLED * 8;

thread_local! {
    /// This thread's cgroupfs lookups so far ([`lookups_on_this_thread`]).
    static LOOKUPS: Cell<u64> = const { Cell::new(0) };
}

/// How many cgroupfs lookups this thread has made through this module: each open, stat, mkdir or rmdir of a cgroup's
/// file or directory by name. Each takes the hierarchy's kernfs root lock to read (`kernfs_iop_lookup`,
/// `kernfs_dop_revalidate`), so it queues behind any writer waiting on it, another box's `mkdir` among them
/// (bench/competitive/probes/goal16_ab.md, "Whose work the no-owner tail waits on"). Tests read it.
pub fn lookups_on_this_thread() -> u64 {
    LOOKUPS.with(Cell::get)
}

/// One more cgroupfs lookup on this thread.
fn looked_up() {
    LOOKUPS.with(|n| n.set(n.get().saturating_add(1)));
}

/// The cgroup this process is in, as a path under [`CGROUP_ROOT`].
pub fn own_cgroup() -> Result<PathBuf, Refusal> {
    let text = fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| Refusal::os("reading /proc/self/cgroup", e))?;
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| Refusal::Platform {
            what: "the process class",
            platform: "linux",
            why: "this process isn't in a cgroup v2 hierarchy (no 0:: line in /proc/self/cgroup)"
                .to_owned(),
        })?;
    Ok(Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// The directory that holds a node's boxes, each a cgroup below it, held open: a box's cgroup is looked
/// up by its name alone, relative to it, and each of the box's files by its name relative to the box's.
/// A path walk takes the read side of each kernfs directory's root lock it passes, the sysfs root's for
/// `/sys` among them (`kernfs_dop_revalidate`, Linux v6.19 fs/kernfs/dir.c:1145-1146 and :1185), against
/// every sysfs node made or removed on the host, a network device's among them (:796, :1554). At
/// concurrency 32 those waits were 2.65 ms of each lifecycle (bench/competitive/probes/create_serial.md,
/// run 12).
#[derive(Debug, Clone)]
pub struct BoxRoot {
    dir: PathBuf,
    fd: Arc<OwnedFd>,
    disks: Vec<Dev>,
    /// The cgroups under the root a box's id may name: those there when it was opened and those made through it since,
    /// slots apart, less those found gone or forgotten ([`BoxRoot::forget`]), so [`BoxRoot::get`] answers a name not
    /// among them without a lookup. At most [`MAX_BOXES`]; past it, `None`, and every name is looked up.
    named: Arc<Mutex<Option<BTreeSet<String>>>>,
    /// The most cgroups under the root, boxes and slots together, past which a create is refused as
    /// [`Refusal::Full`]: [`MAX_BOXES`], or fewer where a node fitted its bounds to its descriptor limit
    /// ([`BoxRoot::at_most`]).
    most: usize,
}

/// The cgroups a [`BoxRoot`] keeps names of ([`BoxRoot::names`]): shared with the root, so a name forgotten here is
/// forgotten there.
#[derive(Debug, Clone)]
pub struct Names(Arc<Mutex<Option<BTreeSet<String>>>>);

impl Names {
    /// The cgroup `name` under the root is gone ([`BoxRoot::forget`]).
    pub fn forget(&self, name: &str) {
        if let Ok(mut set) = self.0.lock()
            && let Some(set) = set.as_mut()
        {
            set.remove(name);
        }
    }
}

impl BoxRoot {
    /// Opens the box root at `dir`, making it when it's missing: its parent is asked to hand it every
    /// one of [`CONTROLLERS`], and it hands them on to boxes. `disks` are the devices `io.latency` and
    /// `io.max` apply to. Where the delegation Hex's installer sets up is missing (the parent lacks a
    /// controller, isn't this user's, or holds processes of its own, so the kernel can't hand
    /// controllers down), both Linux classes are refused by name, saying what's missing
    /// ([`setup::undelegated`]).
    pub fn open(dir: &Path, disks: Vec<Dev>) -> Result<Self, Refusal> {
        let parent = dir.parent().ok_or_else(|| Refusal::Invalid {
            what: "box root",
            why: format!("{} has no parent cgroup", dir.display()),
        })?;
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                return Err(setup::undelegated(
                    parent,
                    Undelegated::NotOurs(e.to_string()),
                ));
            }
            Err(e) => return Err(os_err("making the box root", dir, e)),
        }
        setup::check_controllers(parent, &read_set(&parent.join("cgroup.controllers"))?)?;
        let enable = CONTROLLERS
            .iter()
            .map(|c| format!("+{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        if let Err(e) = write_file(&parent.join("cgroup.subtree_control"), &enable) {
            let procs = fs::read_to_string(parent.join("cgroup.procs")).unwrap_or_default();
            let why = e.to_string();
            return Err(setup::undelegated(
                parent,
                if procs.trim().is_empty() {
                    Undelegated::NotOurs(why)
                } else {
                    Undelegated::Occupied(why)
                },
            ));
        }
        write_file(&dir.join("cgroup.subtree_control"), &enable)?;
        let fd = rustix::fs::open(
            dir,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| os_err("opening the box root", dir, e))?;
        let root = Self {
            dir: dir.to_owned(),
            fd: Arc::new(fd),
            disks,
            named: Arc::new(Mutex::new(None)),
            most: MAX_BOXES,
        };
        let named: BTreeSet<String> = root
            .boxes()?
            .into_iter()
            .filter(|n| reuse::slot_of(n).is_none())
            .collect();
        if let Ok(mut set) = root.named.lock() {
            *set = Some(named);
        }
        Ok(root)
    }

    /// The box root beside this process's own cgroup: `<parent of own cgroup>/hex`. The node agent
    /// runs in a leaf (`.../runner`) and its boxes in `.../hex/<id>`.
    pub fn beside_self(disks: Vec<Dev>) -> Result<Self, Refusal> {
        let own = own_cgroup()?;
        let parent = own.parent().ok_or_else(|| Refusal::Invalid {
            what: "box root",
            why: format!(
                "{} is the root cgroup; the node agent needs a delegated subtree",
                own.display()
            ),
        })?;
        Self::open(&parent.join("hex"), disks)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The disks `io.latency` and `io.max` apply to.
    pub fn disks(&self) -> &[Dev] {
        &self.disks
    }

    /// Makes box `id`'s cgroup with `bounds` in force: every bound's controller is there, since the root
    /// opened only with all of [`CONTROLLERS`] ([`BoxRoot::open`]).
    pub fn create(&self, id: &BoxId, bounds: &Bounds) -> Result<BoxCgroup, Refusal> {
        self.make(id.as_str(), bounds, || Refusal::Exists(id.clone()))
    }

    /// The managed VM path preallocates its SAME typed directory owner before mkdir. The initial
    /// private root observation FD is replaced by the new child FD while that owner is still unique.
    pub fn create_managed(&self, id: &BoxId, bounds: &Bounds) -> Result<BoxCgroup, Refusal> {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let parent = self.dir.as_os_str().as_bytes();
        let room = parent
            .len()
            .checked_add(1)
            .and_then(|size| size.checked_add(id.as_str().len()))
            .ok_or(Refusal::Exhausted {
                what: "managed cgroup path bytes",
                bound: usize::MAX,
            })?;
        let mut path = Vec::new();
        path.try_reserve_exact(room)
            .map_err(|_| Refusal::Exhausted {
                what: "managed cgroup path bytes",
                bound: room,
            })?;
        path.extend_from_slice(parent);
        if path.last() != Some(&b'/') {
            path.push(b'/');
        }
        path.extend_from_slice(id.as_str().as_bytes());
        let directory = PathBuf::from(std::ffi::OsString::from_vec(path));
        let placeholder = self
            .fd
            .as_ref()
            .try_clone()
            .map_err(|error| managed_read_error("preparing a managed directory pin", error))?;
        let pin = triomphe::Arc::try_new(placeholder).map_err(|_| Refusal::Exhausted {
            what: "managed cgroup directory owner",
            bound: size_of::<OwnedFd>(),
        })?;
        self.make_with_pin(
            id.as_str(),
            bounds,
            || match id.try_clone() {
                Ok(id) => Refusal::Exists(id),
                Err(error) => error,
            },
            Some((directory, pin)),
        )
    }

    /// Makes slot `n`'s cgroup ([`reuse::name`]) with `bounds` in force: a cgroup that may be handed
    /// from box to box of one owner (goal 5 extended, step 6). Slots are numbered once each, so one that
    /// is there already is a fault, refused as one.
    pub fn create_slot(&self, n: u64, bounds: &Bounds) -> Result<BoxCgroup, Refusal> {
        let name = reuse::name(n);
        self.make(&name, bounds, || {
            Refusal::Broken(format!(
                "slot {name} is under {} already, but slots are numbered once each",
                self.dir.display()
            ))
        })
    }

    /// Makes the cgroup `name` with `bounds` in force; `exists` is the refusal for one already there.
    fn make(
        &self,
        name: &str,
        bounds: &Bounds,
        exists: impl FnOnce() -> Refusal,
    ) -> Result<BoxCgroup, Refusal> {
        self.make_with_pin(name, bounds, exists, None)
    }

    fn make_with_pin(
        &self,
        name: &str,
        bounds: &Bounds,
        exists: impl FnOnce() -> Refusal,
        prepared: Option<(PathBuf, triomphe::Arc<OwnedFd>)>,
    ) -> Result<BoxCgroup, Refusal> {
        let writes = core::writes(bounds, &self.disks)?;
        if self.count()? >= self.most {
            return Err(Refusal::Full(self.most));
        }
        let (dir, pin) = match prepared {
            Some((directory, pin)) => {
                // BTreeSet cannot reserve this existing optimization's next insertion fallibly.
                // Invalidate it before effects; all later names use the actual directory.
                *self.named.lock().map_err(|_| {
                    managed_read_error(
                        "preparing managed cgroup names",
                        "the existing name cache lock was poisoned",
                    )
                })? = None;
                (directory, Some(pin))
            }
            None => (self.dir.join(name), None),
        };
        looked_up();
        match rustix::fs::mkdirat(self.fd.as_fd(), name, Mode::from_raw_mode(0o777)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => return Err(exists()),
            Err(e) => return Err(os_err("making a box's cgroup", &dir, e)),
        }
        let managed = pin.is_some();
        let cg = match pin {
            Some(mut pin) => {
                looked_up();
                let fd = rustix::fs::openat(
                    self.fd.as_fd(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| managed_read_error("opening the managed cgroup", error))?;
                let slot = triomphe::Arc::get_mut(&mut pin).ok_or_else(|| {
                    managed_read_error(
                        "installing the managed cgroup pin",
                        "the prepared owner is no longer unique",
                    )
                })?;
                *slot = fd;
                BoxCgroup {
                    dir,
                    fd: Some(DirectoryPin::Managed(pin)),
                    opened: None,
                }
            }
            None => {
                self.made(name);
                match self.child(name) {
                    Some(cg) => cg,
                    None => {
                        let _ = fs::remove_dir(&dir);
                        return Err(Refusal::Os {
                            op: "opening a box's cgroup",
                            why: format!("{} was made but couldn't be opened", dir.display()),
                        });
                    }
                }
            }
        };
        for w in writes {
            if let Err(e) = cg.write_bound(&w) {
                // The managed canonical Creating row retains cleanup authority on an incomplete
                // native setup. Do not remove a possibly replaced pathname after this failure.
                if !managed {
                    // A box is made whole or not at all; its empty cgroup goes with the refusal.
                    let _ = fs::remove_dir(&cg.dir);
                }
                return Err(e);
            }
        }
        Ok(cg)
    }

    /// Writes `bounds` into `cg`, a fresh cgroup no box has used, made with no bound (one made ahead): the box's own
    /// bounds alone, as [`BoxRoot::make`] gives a cgroup made on a create's path ([`core::writes`]), since a fresh
    /// cgroup holds the kernel's default for every other file already. Every default written again is a cgroupfs call
    /// on the create's path that changes nothing, `cpu.weight`'s walking every CPU's run queue and each disk's
    /// `io.latency` and `io.max` among them (goal 16). A cgroup an earlier box left takes [`BoxRoot::rebound`].
    pub fn bound(&self, cg: &BoxCgroup, bounds: &Bounds) -> Result<(), Refusal> {
        for w in core::writes(bounds, &self.disks)? {
            cg.write_bound(&w)?;
        }
        Ok(())
    }

    /// Writes every bound again into `cg`, a cgroup an earlier box left, so it holds `bounds` and
    /// nothing of the earlier box's ([`core::writes_all`]; goal 5 extended, step 6). None of these files'
    /// writes takes the kernel's `cgroup_mutex` (research f25). A default for a file the kernel doesn't
    /// have is skipped: no earlier box could have set it either. `cpu.max` alone is read first and
    /// written only where its values differ ([`cpu_max_holds`]): its write takes the host-wide
    /// `cfs_constraints_mutex` and walks every task group on the host (Linux v6.19 `tg_set_cfs_bandwidth`,
    /// kernel/sched/core.c:9461-9462, `__cfs_schedulable` :9624; research f26). On fedora's 279 cgroups it
    /// took 6.6 us p50 against 2.5 for `pids.max`, a cost that grows with the host's task groups, while its
    /// read takes no lock (`cpu_max_show`, :10104) and took 2.8 (bench/competitive/probes/cpu_max_write.md,
    /// run 1). So the bound is in force either way.
    pub fn rebound(&self, cg: &BoxCgroup, bounds: &Bounds) -> Result<(), Refusal> {
        for w in core::writes_all(bounds, &self.disks)? {
            if core::is_default(&w) && !cg.has(w.file) {
                continue;
            }
            if w.file == "cpu.max" && cpu_max_holds(cg, &w.value) {
                continue;
            }
            cg.write_bound(&w)?;
        }
        Ok(())
    }

    /// Slot `n`'s cgroup, when it exists.
    pub fn slot(&self, n: u64) -> Option<BoxCgroup> {
        self.child(&reuse::name(n))
    }

    /// Box `id`'s cgroup, when it exists.
    pub fn get(&self, id: &BoxId) -> Option<BoxCgroup> {
        let name = id.as_str();
        // A name the root never had answers without a lookup: a lookup queues behind every cgroup made or removed on
        // the host meanwhile (bench/competitive/probes/goal16_ab.md, "Whose work the no-owner tail waits on").
        if let Ok(set) = self.named.lock()
            && let Some(set) = set.as_ref()
            && !set.contains(name)
        {
            return None;
        }
        let found = self.child(name);
        if found.is_none() {
            self.forget(name);
        }
        found
    }

    /// The cgroup `name` under the root is gone, removed by its box's delete or found so.
    pub fn forget(&self, name: &str) {
        self.names().forget(name);
    }

    /// The names the root keeps, for one that removes a box's cgroup apart from the root (a run's, by its node's
    /// supervisor, decision 7) to forget it by.
    pub fn names(&self) -> Names {
        Names(Arc::clone(&self.named))
    }

    /// The cgroup `name` was made under the root: one a box's id may name, a slot's apart. Past [`MAX_BOXES`] names the
    /// root stops keeping them, and every name is looked up.
    fn made(&self, name: &str) {
        if reuse::slot_of(name).is_some() {
            return;
        }
        if let Ok(mut set) = self.named.lock() {
            let full = set.as_ref().is_some_and(|s| s.len() >= MAX_BOXES);
            if full {
                *set = None;
            } else if let Some(s) = set.as_mut() {
                s.insert(name.to_owned());
            }
        }
    }

    /// Every file read or written in a cgroup made ahead, by name: each bound's ([`core::writes_all`] over the root's
    /// disks), which the box's create writes, and its counters' ([`BoxCgroup::baseline`]) and `memory.peak`, which the
    /// refill reads and writes once the cgroup's init is parked. The refill opens them when it makes the cgroup
    /// ([`BoxCgroup::open_ahead`]), so neither looks one up.
    pub fn ahead_files(&self) -> Vec<&'static str> {
        ahead_files(&self.disks)
    }

    /// This root holding at most `most` cgroups, boxes and slots together, never past [`MAX_BOXES`]: a node whose start
    /// fitted its bounds to its descriptor limit holds no more boxes than it counted the descriptors of (hex-node's
    /// `netns::fit`), each one more refused as [`Refusal::Full`].
    pub fn at_most(mut self, most: usize) -> Self {
        self.most = most.min(MAX_BOXES);
        self
    }

    /// The cgroup `name` under the root, when it's there, opened relative to the root.
    fn child(&self, name: &str) -> Option<BoxCgroup> {
        looked_up();
        let fd = rustix::fs::openat(
            self.fd.as_fd(),
            name,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        Some(BoxCgroup {
            dir: self.dir.join(name),
            fd: Some(DirectoryPin::Shared(Arc::new(fd))),
            opened: None,
        })
    }

    /// How many cgroups are under the root, box or not: the count [`BoxRoot::boxes`] would list, read from
    /// the root's link count instead. A cgroup v2 directory's link count is its child directories plus two
    /// (kernfs sets it so, `set_nlink(inode, kn->dir.subdirs + 2)` in `kernfs_refresh_inode`, Linux v6.19
    /// fs/kernfs/inode.c; measured on fedora: 0, 1, 7 and 32 children, links 2, 3, 9 and 34), and a `stat`
    /// of it takes only kernfs's attribute lock, shared (`kernfs_iop_getattr`). A listing takes the root
    /// directory's own lock shared, and every box's `mkdir` and `rmdir` beside it take that lock
    /// exclusive, so at concurrency 32 a create waited behind them all: 5.7 ms of each lifecycle off the
    /// CPU, the largest wait a create had (goal 5 plan step 4, bench/competitive/probes/create_serial.md).
    ///
    /// A directory whose links don't count its children (fewer than two: btrfs, for one, gives every
    /// directory one) is listed instead, so the bound holds on any filesystem a root is opened on.
    pub fn count(&self) -> Result<usize, Refusal> {
        let links = rustix::fs::fstat(self.fd.as_fd())
            .map_err(|e| os_err("counting boxes", &self.dir, e))?
            .st_nlink;
        if links < 2 {
            return Ok(self.boxes()?.len());
        }
        Ok(usize::try_from(links.saturating_sub(2)).unwrap_or(usize::MAX))
    }

    /// The name of every cgroup under the root, box or not, bounded by [`MAX_BOXES`].
    pub fn boxes(&self) -> Result<Vec<String>, Refusal> {
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.dir).map_err(|e| os_err("listing boxes", &self.dir, e))?;
        for entry in entries.take(MAX_BOXES) {
            let entry = entry.map_err(|e| os_err("listing boxes", &self.dir, e))?;
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(out)
    }
}

/// One box's cgroup: its directory's path, and the directory held open where it was opened through its
/// root ([`BoxRoot`]), so its files are opened by their names alone.
#[derive(Debug, Clone)]
pub struct BoxCgroup {
    dir: PathBuf,
    fd: Option<DirectoryPin>,
    /// Its files opened ahead ([`BoxCgroup::open_ahead`]), so they're read and written without a lookup.
    opened: Option<Arc<Opened>>,
}

/// One directory owner in the existing slot. The legacy constructor keeps its original Arc and
/// Option semantics; managed cold observation allocates its same typed owner fallibly. Changing
/// the legacy child lookup's allocation failure into None would fabricate cgroup absence.
#[derive(Debug, Clone)]
enum DirectoryPin {
    Shared(Arc<OwnedFd>),
    Managed(triomphe::Arc<OwnedFd>),
}
impl AsFd for DirectoryPin {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Shared(fd) => fd.as_fd(),
            Self::Managed(fd) => fd.as_fd(),
        }
    }
}
impl DirectoryPin {
    fn try_clone(&self) -> std::io::Result<OwnedFd> {
        match self {
            Self::Shared(fd) => fd.as_ref().try_clone(),
            Self::Managed(fd) => fd.as_ref().try_clone(),
        }
    }
}

/// A cgroup's files opened ahead, by name: each the descriptor and whether it was opened for writing, or `None` where
/// the cgroup has no such file (its controller not handed down). A file not here is opened by name as before.
#[derive(Debug, Default)]
struct Opened(BTreeMap<&'static str, Option<(OwnedFd, bool)>>);

/// The counters a box's usage is reported from, which [`BoxCgroup::baseline`] reads.
const BASELINE_FILES: [&str; 4] = ["memory.events", "pids.events", "cpu.stat", "io.stat"];

/// The files a cgroup made ahead opens ahead under a root whose `io` bounds apply to `disks` ([`BoxRoot::ahead_files`]),
/// by name: what the refill opens, and what a node counts among its descriptors for each (hex-node's `netns`).
pub fn ahead_files(disks: &[Dev]) -> Vec<&'static str> {
    let mut files: Vec<&'static str> = core::writes_all(&Bounds::default(), disks)
        .map(|ws| ws.iter().map(|w| w.file).collect())
        .unwrap_or_default();
    files.extend(BASELINE_FILES);
    files.push("memory.peak");
    files.sort_unstable();
    files.dedup();
    files
}

impl BoxCgroup {
    /// The cgroup at `dir`, wherever it is: an `oci` container's cgroup goes where its spec puts it,
    /// not under a box root, and is killed, waited out and removed as a box's is. Its files are opened
    /// by their paths.
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            fd: None,
            opened: None,
        }
    }

    /// Read-only cold observation; never creates a root, changes controllers or adopts a process.
    /// NOENT alone reports a missing path. Neither that result nor the pin is scope-release proof.
    pub fn observe_managed(dir: PathBuf) -> Result<Option<Self>, Refusal> {
        looked_up();
        let fd = match rustix::fs::open(
            &dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(managed_read_error("observing a native cgroup", error)),
        };
        let pin = triomphe::Arc::try_new(fd).map_err(|_| Refusal::Exhausted {
            what: "native cgroup directory owner",
            bound: size_of::<OwnedFd>(),
        })?;
        Ok(Some(Self {
            dir,
            fd: Some(DirectoryPin::Managed(pin)),
            opened: None,
        }))
    }

    /// This cgroup with `files` opened now, each for reading and writing where the kernel lets it, else for reading:
    /// the refill's, as it makes a cgroup ahead, so the create that takes it looks none of them up (goal 16; the
    /// lookups queued behind the next cgroup's `mkdir`). A file the cgroup doesn't have is known absent; one that
    /// fails to open otherwise is left to be opened by name.
    pub fn open_ahead(mut self, files: &[&'static str]) -> Self {
        let mut opened = Opened::default();
        for &file in files {
            match self.open_file(file, OFlags::RDWR) {
                Ok(fd) => {
                    opened.0.insert(file, Some((fd, true)));
                }
                Err(rustix::io::Errno::NOENT) => {
                    opened.0.insert(file, None);
                }
                Err(_) => {
                    if let Ok(fd) = self.open_file(file, OFlags::RDONLY) {
                        opened.0.insert(file, Some((fd, false)));
                    }
                }
            }
        }
        self.opened = Some(Arc::new(opened));
        self
    }

    /// Its file `file` as opened ahead: `Some(Some(fd))` opened (a copy of the descriptor, its open file shared),
    /// `Some(None)` known absent, `None` not opened ahead, or not for writing where `write` asks it.
    fn opened(&self, file: &str, write: bool) -> Option<Option<OwnedFd>> {
        match self.opened.as_ref()?.0.get(file)? {
            None => Some(None),
            Some((fd, rw)) if *rw || !write => {
                rustix::io::fcntl_dupfd_cloexec(fd, 0).ok().map(Some)
            }
            Some(_) => None,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The cgroup's file `file`, opened with `flags` (close-on-exec): relative to its directory where it's
    /// held open, else by its path.
    fn open_file(&self, file: &str, flags: OFlags) -> rustix::io::Result<OwnedFd> {
        let write = flags.intersects(OFlags::WRONLY | OFlags::RDWR);
        if let Some(found) = self.opened(file, write) {
            return found.ok_or(rustix::io::Errno::NOENT);
        }
        looked_up();
        match &self.fd {
            Some(fd) => {
                rustix::fs::openat(fd.as_fd(), file, flags | OFlags::CLOEXEC, Mode::empty())
            }
            None => rustix::fs::open(self.dir.join(file), flags | OFlags::CLOEXEC, Mode::empty()),
        }
    }

    /// Whether the cgroup has the file `file`: a controller not handed down has none of its files.
    fn has(&self, file: &str) -> bool {
        if let Some(known) = self.opened.as_ref().and_then(|o| o.0.get(file)) {
            return known.is_some();
        }
        looked_up();
        match &self.fd {
            Some(fd) => rustix::fs::statat(fd.as_fd(), file, AtFlags::empty()).is_ok(),
            None => self.dir.join(file).exists(),
        }
    }

    /// The text of the cgroup's file `file` ([`read_fd`]), bounded by [`MAX_PROCS_FILE`] for its `cgroup.procs`,
    /// which grows with its processes, and by [`MAX_CGROUP_FILE`] for the rest.
    fn read(&self, file: &str) -> Result<String, Refusal> {
        let fd = self
            .open_file(file, OFlags::RDONLY)
            .map_err(|e| os_err("reading a cgroup file", &self.dir.join(file), e))?;
        let max = if file == "cgroup.procs" {
            MAX_PROCS_FILE
        } else {
            MAX_CGROUP_FILE
        };
        read_fd(fd.as_fd(), max, || self.dir.join(file))
    }

    /// Writes `value` to the cgroup's file `file` ([`write_fd`]).
    fn write(&self, file: &str, value: &str) -> Result<(), Refusal> {
        let fd = self
            .open_file(file, OFlags::WRONLY)
            .map_err(|e| os_err("opening a cgroup file", &self.dir.join(file), e))?;
        write_fd(fd.as_fd(), value, || self.dir.join(file))
    }

    /// Its `cgroup.procs`, open for writing: a process that writes "0" to it joins the box.
    pub fn procs_for_joining(&self) -> Result<OwnedFd, Refusal> {
        self.open_file("cgroup.procs", OFlags::WRONLY).map_err(|e| {
            os_err(
                "opening a box's cgroup.procs",
                &self.dir.join("cgroup.procs"),
                e,
            )
        })
    }

    /// The cgroup directory itself, open: what `clone3(CLONE_INTO_CGROUP)` takes. A copy of the
    /// descriptor it's held by, where it's held.
    pub fn open_dir(&self) -> Result<OwnedFd, Refusal> {
        match &self.fd {
            Some(fd) => fd
                .try_clone()
                .map_err(|e| os_err("opening a box's cgroup", &self.dir, e)),
            None => rustix::fs::open(
                &self.dir,
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| os_err("opening a box's cgroup", &self.dir, e)),
        }
    }

    /// The processes in the box now.
    pub fn procs(&self) -> Result<Vec<u32>, Refusal> {
        let text = self.read("cgroup.procs")?;
        Ok(text
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .take(MAX_SIGNALLED)
            .collect())
    }

    /// Strict population observation from the SAME native directory pin. A missing, unreadable or
    /// unparseable required counter refuses; false is a counter fact, never whole-scope release.
    pub fn managed_populated(&self) -> Result<bool, Refusal> {
        core::populated(&self.managed_read("cgroup.events", MAX_CGROUP_FILE)?).ok_or_else(|| {
            managed_read_error("cgroup.events", "missing or malformed populated counter")
        })
    }

    /// A complete, unambiguous membership observation from the SAME pinned native cgroup. Kernel
    /// cgroup.procs may repeat a PID during migration/reuse; that observation is refused, not
    /// silently deduplicated into ownership proof (cgroup-v2.rst, Organizing Processes).
    pub fn managed_procs(&self) -> Result<Vec<u32>, Refusal> {
        parse_managed_procs(
            &self.managed_read("cgroup.procs", MAX_PROCS_FILE)?,
            MAX_SIGNALLED,
        )
    }

    /// Read-only verification of native cgroup limit values against their canonical allocation.
    /// This observes limits, never rewrites them or treats a missing controller as an unlimited one.
    pub fn check_managed_bounds(&self, bounds: &Bounds) -> Result<(), Refusal> {
        bounds.check()?;
        check_managed_limits(
            bounds,
            &self.managed_read("cpu.max", MAX_CGROUP_FILE)?,
            &self.managed_read("memory.max", MAX_CGROUP_FILE)?,
            &self.managed_read("memory.swap.max", MAX_CGROUP_FILE)?,
        )?;
        if let Some(expected) = bounds.pids_max {
            let actual =
                managed_limit("pids.max", &self.managed_read("pids.max", MAX_CGROUP_FILE)?)?;
            if actual == 0 || actual > u64::from(expected) {
                return Err(managed_read_error(
                    "pids.max",
                    "the native limit exceeds its canonical bound",
                ));
            }
        }
        Ok(())
    }

    /// Whether any process runs in the box.
    pub fn populated(&self) -> Result<bool, Refusal> {
        self.parse_populated(&self.read("cgroup.events")?)
    }

    fn parse_populated(&self, text: &str) -> Result<bool, Refusal> {
        core::populated(text).ok_or_else(|| Refusal::Os {
            op: "reading cgroup.events",
            why: format!(
                "{}: no populated line",
                self.dir.join("cgroup.events").display()
            ),
        })
    }

    /// Whether any process runs, or its mandatory events file disappeared during removal (HEX-RT-15). Reads
    /// stay relative to this cgroup's pinned directory, so a new box at the same path cannot stand in for it.
    /// Other open/read failures, invalid text and oversized files remain refusals.
    pub fn populated_if_present(&self) -> Result<Option<bool>, Refusal> {
        self.populated_from(self.open_file("cgroup.events", OFlags::RDONLY))
    }

    /// [`BoxCgroup::populated_if_present`] from its events file's open, which a deterministic test stands
    /// in for one a removal raced.
    fn populated_from(&self, opened: rustix::io::Result<OwnedFd>) -> Result<Option<bool>, Refusal> {
        let fd = match opened {
            Ok(fd) => fd,
            // Kernfs refuses the open of a node the removal deactivated (`kernfs_fop_open`), and a
            // lookup in the removed directory finds nothing.
            Err(rustix::io::Errno::NODEV | rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => {
                return Err(os_err(
                    "reading a cgroup file",
                    &self.dir.join("cgroup.events"),
                    e,
                ));
            }
        };
        read_if_present(
            MAX_CGROUP_FILE,
            || self.dir.join("cgroup.events"),
            |buf, at| rustix::io::pread(fd.as_fd(), buf, at),
        )?
        .map(|text| self.parse_populated(&text))
        .transpose()
    }

    /// Sends `signal` to every process in the box; refused as not running when there is none.
    pub fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        if !self.populated()? {
            return Err(Refusal::NotRunning(id.clone(), "signal"));
        }
        if signal == Signal::KILL {
            return self.kill();
        }
        let sig =
            rustix::process::Signal::from_named_raw(signal.number()).ok_or(Refusal::Invalid {
                what: "signal",
                why: format!("{} isn't a signal this kernel names", signal.number()),
            })?;
        for pid in self.procs()? {
            // A process that ended since the list was read is no longer the box's to signal.
            if let Some(pid) = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, sig);
            }
        }
        Ok(())
    }

    /// The most cgroups a walk of this box's subtree meets ([`sweep_budget`]): what the kill's sweep is bounded
    /// by, and the node's other walks of a box's subtree with it.
    pub fn subtree_budget(&self) -> Result<usize, Refusal> {
        sweep_budget(
            &self.read("cgroup.stat")?,
            &self.read("cgroup.max.descendants")?,
        )
        .ok_or_else(|| Refusal::Os {
            op: "reading a box's cgroup count",
            why: format!(
                "{}: cgroup.stat or cgroup.max.descendants doesn't parse",
                self.dir.display()
            ),
        })
    }

    /// Kills every process in the box. `cgroup.kill` (Linux 5.14) kills the whole subtree in one write,
    /// but signals thread-group *leaders*, so a leaderless group — its leader thread exited while a
    /// non-leader thread lives — survives it and holds the box populated (proven on Linux 7.2.9 and
    /// 6.19, `bench/competitive/probes/delete_flake.md`). So if the box is still populated after it,
    /// sweep its `cgroup.procs` under a bound ([`sweep_targets`]) and send each process a
    /// *process-directed* SIGKILL through a process pidfd ([`sigkill_box_process`]): the kernel delivers
    /// it to the whole thread group (`PIDTYPE_TGID`), reaching a leaderless group's live worker the
    /// leader-directed `cgroup.kill` missed (probe `delete_flake_point4.c`). [`BoxCgroup::wait_empty`]
    /// then sees it empty.
    pub fn kill(&self) -> Result<(), Refusal> {
        self.write("cgroup.kill", "1")?;
        if !self.populated()? {
            return Ok(());
        }
        let budget = self.subtree_budget()?;
        for (dir, pid) in sweep_targets(&self.dir, &FsTree, budget, MAX_SIGNALLED)? {
            sigkill_box_process(&dir, pid)?;
        }
        Ok(())
    }

    /// Waits until no process is left in the box, at most `timeout`; whether none is.
    pub fn wait_empty(&self, timeout: Duration) -> Result<bool, Refusal> {
        let path = self.dir.join("cgroup.events");
        let fd = self
            .open_file("cgroup.events", OFlags::RDONLY)
            .map_err(|e| os_err("opening cgroup.events", &path, e))?;
        let deadline = Instant::now().checked_add(timeout);
        let mut buf = [0u8; 256];
        loop {
            let n = rustix::io::pread(&fd, &mut buf, 0)
                .map_err(|e| os_err("reading cgroup.events", &path, e))?;
            let text = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());
            if core::populated(&text) == Some(false) {
                return Ok(true);
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => timeout,
            };
            if left.is_zero() {
                return Ok(false);
            }
            // The kernel marks cgroup.events modified (POLLPRI) when `populated` changes, but on some
            // kernels that notification lags the actual depopulation by several ms (CachyOS 7.2;
            // probes/oci_breakdown.md run 3), which would leave `delete` waiting on a late wake. Cap the
            // poll so `populated` is re-read at least every POLL_CAP; 500 µs is well under crun's ~0.4 ms
            // delete there and far below the observed lag. Correctness is unchanged: it still returns only
            // when `populated` is false, or when the deadline passes.
            const POLL_CAP: Duration = Duration::from_micros(500);
            let poll_for = left.min(POLL_CAP);
            let ts = rustix::event::Timespec {
                tv_sec: i64::try_from(poll_for.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(poll_for.subsec_nanos()),
            };
            let borrowed = fd.as_fd();
            let mut fds = [rustix::event::PollFd::new(
                &borrowed,
                rustix::event::PollFlags::PRI,
            )];
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(os_err("waiting on cgroup.events", &path, e)),
            }
        }
    }

    /// Removes the box's cgroup, which must hold no process (`cgroup.kill` and `populated` cover its
    /// whole subtree), with every cgroup beneath it: a container with a cgroup namespace and a writable
    /// cgroup2 mount makes its own (runc's and crun's delete remove them the same way). Flat: the
    /// descendants are listed breadth first, at most [`MAX_DESCENDANTS`] of them, then removed deepest
    /// first (CLAUDE.md §11). Absent already is removed.
    pub fn remove(&self) -> Result<(), Refusal> {
        let mut found: Vec<PathBuf> = Vec::new();
        let mut next = 0;
        let mut dir = Some(self.dir.clone());
        while let Some(at) = dir {
            looked_up();
            let listing = match fs::read_dir(&at) {
                Ok(listing) => listing,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    dir = found.get(next).cloned();
                    next = next.saturating_add(1);
                    continue;
                }
                Err(e) => return Err(os_err("listing a box's cgroups", &at, e)),
            };
            for entry in listing {
                let entry = entry.map_err(|e| os_err("listing a box's cgroups", &at, e))?;
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    if found.len() >= MAX_DESCENDANTS {
                        return Err(Refusal::Exhausted {
                            what: "cgroups beneath a box that its removal lists",
                            bound: MAX_DESCENDANTS,
                        });
                    }
                    found.push(entry.path());
                }
            }
            dir = found.get(next).cloned();
            next = next.saturating_add(1);
        }
        for sub in found.iter().rev().chain(std::iter::once(&self.dir)) {
            looked_up();
            match fs::remove_dir(sub) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(os_err("removing a box's cgroup", sub, e)),
            }
        }
        Ok(())
    }

    /// Writes the one bound `w` and nothing else (joint plan step 14: the node's charge learner moves a running VM's
    /// `cpu.weight` and `memory.high`, and its fleet's parent's, each time the learned charges move them), as
    /// [`BoxRoot::rebound`] writes each.
    pub fn set(&self, w: &core::Write) -> Result<(), Refusal> {
        self.write_bound(w)
    }

    /// Writes one bound. A kernel without swap accounting has no memory.swap.max: with no swap on, the
    /// memory bound holds without it; with swap on it cannot hold, so the box is refused by name.
    fn write_bound(&self, w: &core::Write) -> Result<(), Refusal> {
        if w.file == "memory.swap.max" && !self.has(w.file) {
            return match swap_active()? {
                false => Ok(()),
                true => Err(Refusal::Platform {
                    what: "a memory bound",
                    platform: "linux",
                    why: "swap is on but this kernel has no swap accounting (no memory.swap.max), so a box could swap past its memory bound".to_owned(),
                }),
            };
        }
        self.write(w.file, &w.value)
    }

    /// What [`reuse::check`] reads of this cgroup before it is handed on: files whose reads take no
    /// lock (research f61). The memory files read as `None` where the memory controller isn't handed
    /// down, so the kernel has none.
    pub fn idle_files(&self) -> Result<reuse::Files, Refusal> {
        let read = |file: &str| self.read(file);
        let maybe = |file: &str| match self.read(file) {
            Ok(text) => Ok(Some(text)),
            Err(Refusal::Os { .. }) if !self.has(file) => Ok(None),
            Err(e) => Err(e),
        };
        Ok(reuse::Files {
            events: read("cgroup.events")?,
            stat: read("cgroup.stat")?,
            subtree_control: read("cgroup.subtree_control")?,
            kind: read("cgroup.type")?,
            freeze: read("cgroup.freeze")?,
            oom_group: maybe("memory.oom.group")?,
            memory_current: maybe("memory.current")?,
        })
    }

    /// Its cumulative counters now, which a box given this cgroup has its usage reported from
    /// ([`core::since`]); a file the kernel doesn't have reads as 0, as [`BoxCgroup::usage`] reads it.
    pub fn baseline(&self) -> Baseline {
        let read = |f: &str| self.read(f).unwrap_or_default();
        Baseline::of(&core::usage(&UsageFiles {
            memory_events: read("memory.events"),
            pids_events: read("pids.events"),
            cpu_stat: read("cpu.stat"),
            io_stat: read("io.stat"),
            ..UsageFiles::default()
        }))
    }

    /// Its `memory.peak`, opened and written, so reads through the descriptor say the peak from now on
    /// alone, never the cgroup's lifetime peak (Linux 6.12 on: "A write of any non-empty string to this
    /// file resets it to the current memory usage for subsequent reads through the same file
    /// descriptor", Documentation/admin-guide/cgroup-v2.rst; mm/memcontrol.c `peak_write`, v6.19 :4283;
    /// research f31). `None` where the memory controller isn't handed down; refused where the kernel
    /// can't reset it, so the cgroup isn't handed on with its predecessors' peak.
    pub fn peak_from_now(&self) -> Result<Option<std::os::fd::OwnedFd>, Refusal> {
        let path = self.dir.join("memory.peak");
        if !self.has("memory.peak") {
            return Ok(None);
        }
        let fd = self
            .open_file("memory.peak", OFlags::RDWR)
            .map_err(|e| os_err("opening a cgroup's memory.peak to reset it", &path, e))?;
        rustix::io::write(&fd, b"reset")
            .map_err(|e| os_err("resetting a cgroup's memory.peak", &path, e))?;
        Ok(Some(fd))
    }

    /// What usage the box given this cgroup at `base` has had: [`core::since`] over its files now, its
    /// peak read through `peak`, the descriptor [`BoxCgroup::peak_from_now`] wrote ([`BoxCgroup::usage`]'s
    /// lifetime peak where there is none).
    pub fn usage_since(&self, base: &Baseline, peak: Option<&std::os::fd::OwnedFd>) -> Usage {
        let now = self.usage();
        let peak = peak
            .and_then(|fd| {
                let mut buf = [0u8; PEAK_TEXT];
                let n = rustix::io::pread(fd, &mut buf, 0).ok()?;
                std::str::from_utf8(buf.get(..n)?).ok()?.trim().parse().ok()
            })
            .unwrap_or(now.memory_peak_bytes);
        core::since(&now, base, peak)
    }

    /// What the box has used, from its cgroup files; a file the kernel doesn't have reads as 0.
    pub fn usage(&self) -> Usage {
        let read = |f: &str| self.read(f).unwrap_or_default();
        core::usage(&UsageFiles {
            memory_current: read("memory.current"),
            memory_peak: read("memory.peak"),
            memory_events: read("memory.events"),
            pids_current: read("pids.current"),
            pids_events: read("pids.events"),
            cpu_stat: read("cpu.stat"),
            io_stat: read("io.stat"),
        })
    }

    /// Measured native backing usage, never a failed read represented as free host capacity. Only
    /// a directory already pinned by the native owner can supply it; a reused path is not authority.
    pub fn managed_usage(&self) -> Result<Usage, Refusal> {
        let read = |file: &'static str| self.managed_read(file, MAX_CGROUP_FILE);
        core::managed_usage(&UsageFiles {
            memory_current: read("memory.current")?,
            memory_peak: read("memory.peak")?,
            memory_events: read("memory.events")?,
            pids_current: read("pids.current")?,
            pids_events: read("pids.events")?,
            cpu_stat: read("cpu.stat")?,
            io_stat: read("io.stat")?,
        })
    }

    /// Shares the existing reader and its pre-reserved append bound with strict managed callers.
    fn managed_read(&self, file: &'static str, maximum: usize) -> Result<String, Refusal> {
        if self.fd.is_none() {
            return Err(managed_read_error(
                file,
                "the native owner has no pinned cgroup directory",
            ));
        }
        let mut storage = Vec::new();
        // The shared reader tests its bound after one chunk; reserve that last chunk too. This
        // keeps every append within the fallibly reserved storage without changing ordinary reads.
        let bound = maximum.checked_add(READ_CHUNK).ok_or(Refusal::Exhausted {
            what: "managed cgroup file bytes",
            bound: maximum,
        })?;
        storage
            .try_reserve_exact(bound)
            .map_err(|_| Refusal::Exhausted {
                what: "managed cgroup file bytes",
                bound,
            })?;
        let fd = self
            .open_file(file, OFlags::RDONLY)
            .map_err(|error| managed_read_error(file, error))?;
        read_text(
            maximum,
            |buf, at| rustix::io::pread(fd.as_fd(), buf, at),
            storage,
        )
        .map_err(|error| match error {
            ReadFailure::Io(error) => managed_read_error(file, error),
            ReadFailure::OverLimit(_) => {
                managed_read_error(file, "required native file exceeds its read bound")
            }
            ReadFailure::Encoding(error) => managed_read_error(file, error),
        })
    }
}

fn parse_managed_procs(text: &str, bound: usize) -> Result<Vec<u32>, Refusal> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(managed_read_error(
            "cgroup.procs",
            "an unterminated native PID row is not a complete observation",
        ));
    }
    let count = text.lines().count();
    if count > bound {
        return Err(Refusal::Exhausted {
            what: "native cgroup membership",
            bound,
        });
    }
    let mut pids = Vec::new();
    pids.try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "native cgroup membership",
            bound: count,
        })?;
    for row in text.lines() {
        if row.is_empty() || !row.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(managed_read_error(
                "cgroup.procs",
                "a native PID row is not positive decimal",
            ));
        }
        let pid = row
            .parse::<u32>()
            .map_err(|error| managed_read_error("cgroup.procs", error))?;
        let native =
            usize::try_from(pid).map_err(|error| managed_read_error("cgroup.procs", error))?;
        if native == 0 || native >= PID_MAX_LIMIT {
            return Err(managed_read_error(
                "cgroup.procs",
                "a native PID is outside the kernel representation",
            ));
        }
        pids.push(pid);
    }
    pids.sort_unstable();
    if pids.windows(2).any(|pair| matches!(pair, [a,b] if a==b)) {
        return Err(managed_read_error(
            "cgroup.procs",
            "a repeated native PID makes the observation ambiguous",
        ));
    }
    Ok(pids)
}

fn managed_limit(file: &'static str, text: &str) -> Result<u64, Refusal> {
    let word = text.trim();
    if word.is_empty() || !word.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(managed_read_error(
            file,
            "the native bound is not a finite decimal value",
        ));
    }
    word.parse()
        .map_err(|error| managed_read_error(file, error))
}

/// A VM scope's limits against its canonical bounds: its `cpu.max` at most its CPU bound, its swap off, and its
/// `memory.max` at most its memory bound where it has one. A VM has none (joint plan step 14, ROOT's ruling B under
/// decision 9: no per-VM hard memory cap, a chosen bound that would kill a VMM the first time it needs more than any
/// window showed): its memory is held by its budget's memory.high, the last resort and its fleet's budget, so any
/// `memory.max` it holds, "max" included, passes, and only its swap must be off, so all it holds is RAM.
fn check_managed_limits(
    bounds: &Bounds,
    cpu: &str,
    memory: &str,
    swap: &str,
) -> Result<(), Refusal> {
    let Some(cpu_milli) = bounds.cpu_max_milli else {
        return Err(managed_read_error(
            "native backing bounds",
            "a canonical CPU limit is required",
        ));
    };
    let cpu = core::CpuMax::parse(cpu)
        .ok_or_else(|| managed_read_error("cpu.max", "malformed native CPU bandwidth"))?;
    let quota = cpu.quota_us.filter(|quota| *quota != 0).ok_or_else(|| {
        managed_read_error("cpu.max", "a positive finite native CPU quota is required")
    })?;
    // cpu.max uses quota/period CPUs; the canonical allocation uses thousandths of a CPU.
    let actual = u128::from(quota)
        .checked_mul(1000)
        .ok_or_else(|| managed_read_error("cpu.max", "native CPU ratio overflows"))?;
    let admitted = u128::from(cpu_milli)
        .checked_mul(u128::from(cpu.period_us))
        .ok_or_else(|| managed_read_error("cpu.max", "canonical CPU ratio overflows"))?;
    if cpu.period_us == 0 || actual > admitted {
        return Err(managed_read_error(
            "cpu.max",
            "the native CPU limit exceeds its canonical bound",
        ));
    }
    if let Some(memory_max) = bounds.memory_max {
        let memory = managed_limit("memory.max", memory)?;
        if memory == 0 || memory > memory_max {
            return Err(managed_read_error(
                "memory.max",
                "the native memory limit exceeds its canonical bound",
            ));
        }
    } else if memory.trim() != "max" {
        managed_limit("memory.max", memory)?;
    }
    if managed_limit("memory.swap.max", swap)? != 0 {
        return Err(managed_read_error(
            "memory.swap.max",
            "the native VM scope permits memory outside its charged RAM bound",
        ));
    }
    Ok(())
}

fn managed_read_error(file: &'static str, error: impl std::fmt::Display) -> Refusal {
    struct Diagnostic {
        text: String,
        needed: usize,
    }
    impl std::fmt::Write for Diagnostic {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.needed = self
                .text
                .len()
                .checked_add(text.len())
                .ok_or(std::fmt::Error)?;
            self.text
                .try_reserve_exact(text.len())
                .map_err(|_| std::fmt::Error)?;
            self.text.push_str(text);
            Ok(())
        }
    }
    let mut diagnostic = Diagnostic {
        text: String::new(),
        needed: 0,
    };
    if std::fmt::write(&mut diagnostic, format_args!("{error}")).is_err() {
        return Refusal::Exhausted {
            what: "managed usage refusal bytes",
            bound: diagnostic.needed,
        };
    }
    Refusal::Os {
        op: file,
        why: diagnostic.text,
    }
}

/// Whether any swap is on: `/proc/swaps` lists one device per line past its header.
fn swap_active() -> Result<bool, Refusal> {
    let path = Path::new("/proc/swaps");
    let text =
        fs::read_to_string(path).map_err(|e| os_err("reading the active swap list", path, e))?;
    Ok(text.lines().skip(1).any(|l| !l.trim().is_empty()))
}

/// `memory.peak`'s most bytes: a `u64`'s 20 decimal digits and a newline (`peak_show`'s `"%llu\n"`, Linux v6.19
/// mm/memcontrol.c:4250).
const PEAK_TEXT: usize = 21;

/// The bytes [`read_fd`] asks of a cgroup file a read: a page, the least on x86_64 and aarch64, which is the
/// buffer the kernel first fills a seq_file read from (`seq_read_iter`, `seq_buf_alloc(m->size = PAGE_SIZE)`, Linux
/// v6.19 fs/seq_file.c:210), so a file that fits a page is read in one call.
const READ_CHUNK: usize = 4096;

/// Largest cgroup file Hex reads whole: `io.stat` and `cpu.stat` hold a few lines each; a file past
/// this is damage, refused by type rather than read on.
const MAX_CGROUP_FILE: usize = 64 << 10;

/// Whether `cg`'s `cpu.max` already holds `want`, its values compared ([`core::CpuMax`]), never its
/// text. A read that fails, or a value either side that doesn't parse, counts as not holding it, so the
/// bound is written: the write is skipped only on a read that shows the box's values in force.
fn cpu_max_holds(cg: &BoxCgroup, want: &str) -> bool {
    let now = cg.read("cpu.max").ok();
    match (
        now.as_deref().and_then(core::CpuMax::parse),
        core::CpuMax::parse(want),
    ) {
        (Some(now), Some(want)) => now == want,
        _ => false,
    }
}

/// A cgroup file's text from its open descriptor `fd`, in as few reads as it takes, no stat
/// (`fs::read_to_string` takes one per file to size its buffer): an idle cgroup's check and baseline read a
/// dozen on create's path. Refused past `max` bytes; `path` names the file in a refusal.
fn read_fd(fd: BorrowedFd<'_>, max: usize, path: impl Fn() -> PathBuf) -> Result<String, Refusal> {
    read_text(max, |buf, at| rustix::io::pread(fd, buf, at), Vec::new())
        .map_err(|e| e.refusal(&path()))
}

/// Preserve read, format and size failures until the caller renders its refusal. Only an explicit
/// removal-aware caller can interpret a removal errno as absence.
enum ReadFailure {
    Io(rustix::io::Errno),
    OverLimit(usize),
    Encoding(std::string::FromUtf8Error),
}

impl ReadFailure {
    fn refusal(self, path: &Path) -> Refusal {
        match self {
            Self::Io(e) => os_err("reading a cgroup file", path, e),
            Self::OverLimit(max) => Refusal::Os {
                op: "reading a cgroup file",
                why: format!("{} is over {max} bytes", path.display()),
            },
            Self::Encoding(e) => Refusal::Os {
                op: "reading a cgroup file",
                why: format!("{} isn't UTF-8: {e}", path.display()),
            },
        }
    }
}

/// A removed kernfs file can fail after it was opened. A partial read does not prove presence once the next
/// read reports removal; every other errno and every malformed or oversized text remains an error.
fn read_if_present(
    max: usize,
    path: impl Fn() -> PathBuf,
    read: impl FnMut(&mut [u8], u64) -> rustix::io::Result<usize>,
) -> Result<Option<String>, Refusal> {
    match read_text(max, read, Vec::new()) {
        Ok(text) => Ok(Some(text)),
        Err(ReadFailure::Io(rustix::io::Errno::NODEV | rustix::io::Errno::NOENT)) => Ok(None),
        Err(e) => Err(e.refusal(&path())),
    }
}

/// The same bounded descriptor read for strict and removal-aware callers; `read` is pread in production and
/// can stand in for a removed kernfs file in a deterministic unit test.
fn read_text(
    max: usize,
    mut read: impl FnMut(&mut [u8], u64) -> rustix::io::Result<usize>,
    mut out: Vec<u8>,
) -> Result<String, ReadFailure> {
    let mut buf = [0u8; READ_CHUNK];
    // From the file's start whatever its offset: a descriptor opened ahead is read more than once, through copies that
    // share it ([`BoxCgroup::open_ahead`]).
    loop {
        let at = u64::try_from(out.len()).unwrap_or(u64::MAX);
        let n = read(&mut buf, at).map_err(ReadFailure::Io)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(buf.get(..n).unwrap_or_default());
        if out.len() > max {
            return Err(ReadFailure::OverLimit(max));
        }
    }
    String::from_utf8(out).map_err(ReadFailure::Encoding)
}

/// Writes `value` to the cgroup file open at `fd`: one write(2), as the kernel takes a cgroup file's
/// value. `path` names the file in a refusal.
fn write_fd(fd: BorrowedFd<'_>, value: &str, path: impl Fn() -> PathBuf) -> Result<(), Refusal> {
    let n = rustix::io::write(fd, value.as_bytes()).map_err(|e| Refusal::Os {
        op: "writing a cgroup file",
        why: format!("{} = {value:?}: {e}", path().display()),
    })?;
    if n == value.len() {
        Ok(())
    } else {
        Err(Refusal::Os {
            op: "writing a cgroup file",
            why: format!("{}: wrote {n} of {} bytes", path().display(), value.len()),
        })
    }
}

/// Writes `value` to the cgroup file at `path` ([`write_fd`]).
pub fn write_file(path: &Path, value: &str) -> Result<(), Refusal> {
    let fd = rustix::fs::open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| os_err("opening a cgroup file", path, e))?;
    write_fd(fd.as_fd(), value, || path.to_owned())
}

/// The words of a cgroup file that lists controllers (`cgroup.controllers`, `cgroup.subtree_control`).
pub(crate) fn read_set(path: &Path) -> Result<BTreeSet<String>, Refusal> {
    let text =
        fs::read_to_string(path).map_err(|e| os_err("reading cgroup controllers", path, e))?;
    Ok(text.split_ascii_whitespace().map(str::to_owned).collect())
}

/// Sends process `raw`, found in the box's cgroup `dir`, a process-directed SIGKILL — but only through
/// a process pidfd that pins it, and only once `dir`'s `cgroup.procs` is re-read and still lists it, so
/// an id that named one of the box's processes when the sweep read it but has since exited (and may now
/// name an unrelated process) is not signalled. The pidfd pins the identity after acquisition; the
/// re-read is the membership-and-lifetime check that closes the exit/reuse race ([`BoxCgroup::kill`]).
/// A process gone since the sweep (`ESRCH`, or no longer listed) is already as dead as the signal would
/// make it. No freeze is taken, so a post-kill zombie (an exited init awaiting reap) is handled, not
/// waited on.
fn sigkill_box_process(dir: &Path, raw: u32) -> Result<(), Refusal> {
    let Some(pid) = i32::try_from(raw)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return Ok(());
    };
    // A process pidfd (no PIDFD_THREAD), so pidfd_send_signal is process-directed (PIDTYPE_TGID) and
    // reaches a leaderless group's live worker the leader-directed cgroup.kill missed (point4 probe).
    let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::SRCH) => return Ok(()),
        Err(e) => {
            return Err(Refusal::Os {
                op: "opening a box process's pidfd",
                why: format!("pid {raw}: {e}"),
            });
        }
    };
    if !cgroup_lists_pid(dir, raw)? {
        return Ok(());
    }
    match rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(e) => Err(Refusal::Os {
            op: "SIGKILL a box process",
            why: format!("pid {raw}: {e}"),
        }),
    }
}

/// Whether `dir`'s `cgroup.procs` still lists process `raw`; a cgroup gone reads as not listing it.
fn cgroup_lists_pid(dir: &Path, raw: u32) -> Result<bool, Refusal> {
    let path = dir.join("cgroup.procs");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .any(|p| p == raw)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(os_err("re-reading cgroup.procs", &path, e)),
    }
}

/// A box's cgroup subtree as [`sweep_targets`] reads it, behind a seam so the sweep's bounds and typed
/// refusals are provable without a cgroupfs (CLAUDE.md §9). A cgroup gone since the walk began is a
/// benign race — `Ok(None)`, not an error; a real read failure is returned.
trait CgroupTree {
    /// The child cgroup directories of `dir`; `None` if `dir` has vanished.
    fn children(&self, dir: &Path) -> Result<Option<Vec<PathBuf>>, Refusal>;
    /// The process ids in `dir`'s `cgroup.procs`; `None` if the cgroup has vanished.
    fn procs(&self, dir: &Path) -> Result<Option<Vec<u32>>, Refusal>;
}

/// Every process in the box's cgroup subtree rooted at `root`, each with the cgroup it sits in, so
/// [`BoxCgroup::kill`] can signal it. Breadth-first over `tree`, bounded: at most `max_cgroups` cgroups
/// visited — the pending queue never holds more than that either — and at most `max_procs` processes
/// collected; past either it refuses ([`Refusal::Full`]) rather than silently stop (CLAUDE.md §2, §3). A
/// cgroup gone mid-walk is skipped (a benign race); any other read failure is returned. Pure over
/// `tree`, so a mock proves its bounds and its typed refusals.
fn sweep_targets(
    root: &Path,
    tree: &impl CgroupTree,
    max_cgroups: usize,
    max_procs: usize,
) -> Result<Vec<(PathBuf, u32)>, Refusal> {
    let mut pending: VecDeque<PathBuf> = VecDeque::new();
    pending.push_back(root.to_path_buf());
    let mut visited = 0usize;
    let mut targets: Vec<(PathBuf, u32)> = Vec::new();
    while let Some(dir) = pending.pop_front() {
        visited = visited.checked_add(1).ok_or(Refusal::Full(max_cgroups))?;
        if visited > max_cgroups {
            return Err(Refusal::Full(max_cgroups));
        }
        if let Some(children) = tree.children(&dir)? {
            for child in children {
                if pending.len() >= max_cgroups {
                    return Err(Refusal::Full(max_cgroups));
                }
                pending.push_back(child);
            }
        }
        if let Some(procs) = tree.procs(&dir)? {
            for pid in procs {
                if targets.len() >= max_procs {
                    return Err(Refusal::Full(max_procs));
                }
                targets.push((dir.clone(), pid));
            }
        }
    }
    Ok(targets)
}

/// How many cgroups the kill sweep of a box may visit, the box's own count, learned from the kernel (CLAUDE.md §2):
/// the box and what its `cgroup.max.descendants` lets be under it where it sets a number, since the kernel refuses
/// a `mkdir` past it (`cgroup_check_hierarchy_limits`, Linux v6.19 kernel/cgroup/cgroup.c:5972), so the subtree
/// never outgrows it; else, `max` (`INT_MAX`, :2116, :3734-3735), the box and the descendants its `cgroup.stat`
/// counts as the sweep begins (`nr_descendants`, :3832-3833). A subtree grown past that count since is refused
/// ([`Refusal::Full`]) rather than walked on. The sweep holds a path for each cgroup the kernel holds, whose own
/// cost is charged to the box. `None` where either file doesn't parse.
fn sweep_budget(stat: &str, max_descendants: &str) -> Option<usize> {
    let under = match max_descendants.trim() {
        "max" => core::flat(stat, "nr_descendants")?,
        limit => limit.parse().ok()?,
    };
    usize::try_from(under).ok()?.checked_add(1)
}

/// The real cgroupfs for [`sweep_targets`].
struct FsTree;

impl CgroupTree for FsTree {
    fn children(&self, dir: &Path) -> Result<Option<Vec<PathBuf>>, Refusal> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(os_err("listing a box's cgroups", dir, e)),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(os_err("listing a box's cgroups", dir, e)),
            };
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(entry.path()),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(os_err("a box cgroup entry's type", &entry.path(), e)),
            }
        }
        Ok(Some(dirs))
    }

    fn procs(&self, dir: &Path) -> Result<Option<Vec<u32>>, Refusal> {
        let path = dir.join("cgroup.procs");
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(
                text.lines().filter_map(|l| l.trim().parse().ok()).collect(),
            )),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(os_err("reading cgroup.procs", &path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A cgroup tree from in-memory maps: a dir absent from both maps has vanished (its reads return
    /// `None`); a dir in `io_error_at` fails with a non-vanishing read error.
    struct Mock {
        children: BTreeMap<PathBuf, Vec<PathBuf>>,
        procs: BTreeMap<PathBuf, Vec<u32>>,
        io_error_at: Option<PathBuf>,
    }
    impl CgroupTree for Mock {
        fn children(&self, dir: &Path) -> Result<Option<Vec<PathBuf>>, Refusal> {
            if self.io_error_at.as_deref() == Some(dir) {
                return Err(Refusal::Os {
                    op: "listing a box's cgroups",
                    why: "a non-vanishing read failure".to_owned(),
                });
            }
            Ok(self.children.get(dir).cloned())
        }
        fn procs(&self, dir: &Path) -> Result<Option<Vec<u32>>, Refusal> {
            Ok(self.procs.get(dir).cloned())
        }
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn managed_counter_files(dir: &Path, memory: &str) {
        for (file, value) in [
            ("memory.current", memory),
            ("memory.peak", "4096\n"),
            ("memory.events", "oom_kill 0\n"),
            ("pids.current", "1\n"),
            ("pids.events", "max 0\n"),
            ("cpu.stat", "usage_usec 100\n"),
            ("io.stat", ""),
        ] {
            fs::write(dir.join(file), value).unwrap();
        }
    }

    fn pinned_managed_cgroup(dir: &Path) -> BoxCgroup {
        BoxCgroup::observe_managed(dir.to_path_buf())
            .unwrap()
            .unwrap()
    }

    /// AC-T7.1.2/R01: a managed create uses the original creation/bounds path and one fallibly
    /// prepared typed pin; cache insertion, collisions and a reused path cannot change its owner.
    #[test]
    fn managed_creation_uses_one_prepared_pin_and_refuses_colliding_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = BoxRoot {
            dir: tmp.path().to_path_buf(),
            fd: Arc::new(
                rustix::fs::open(
                    tmp.path(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .unwrap(),
            ),
            disks: Vec::new(),
            named: Arc::new(Mutex::new(Some(BTreeSet::new()))),
            most: MAX_BOXES,
        };
        let id = BoxId::new("owned-managed").unwrap();
        let cg = root.create_managed(&id, &Bounds::default()).unwrap();
        assert!(root.named.lock().unwrap().is_none());
        let copy = cg.clone();
        match (&cg.fd, &copy.fd) {
            (Some(DirectoryPin::Managed(a)), Some(DirectoryPin::Managed(b))) => {
                assert!(triomphe::Arc::ptr_eq(a, b))
            }
            _ => panic!("managed creation used an ordinary or absent directory owner"),
        }
        let path = tmp.path().join(id.as_str());
        fs::write(path.join("cgroup.events"), "populated 1\n").unwrap();
        let before = fs::read(path.join("cgroup.events")).unwrap();
        assert!(
            matches!(root.create_managed(&id, &Bounds::default()), Err(Refusal::Exists(actual)) if actual==id)
        );
        assert_eq!(fs::read(path.join("cgroup.events")).unwrap(), before);
        fs::rename(&path, tmp.path().join("retained")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("cgroup.events"), "populated 0\n").unwrap();
        assert!(cg.managed_populated().unwrap());
        assert!(
            !BoxCgroup::observe_managed(path)
                .unwrap()
                .unwrap()
                .managed_populated()
                .unwrap()
        );
        let bad = BoxId::new("invalid-bounds").unwrap();
        assert!(
            root.create_managed(
                &bad,
                &Bounds {
                    cpu_max_milli: Some(0),
                    ..Bounds::default()
                }
            )
            .is_err()
        );
        assert!(!tmp.path().join(bad.as_str()).exists());
        // A fresh colliding symlink is never followed or removed by the managed create path.
        let link = BoxId::new("collision-link").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("retained"), tmp.path().join(link.as_str()))
            .unwrap();
        assert!(root.create_managed(&link, &Bounds::default()).is_err());
        assert!(
            fs::symlink_metadata(tmp.path().join(link.as_str()))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// AC-T7.1.2: required population facts use the pinned directory; unreadable, absent,
    /// over-bound or malformed bytes cannot credit an empty native scope after path replacement.
    #[test]
    fn managed_population_refuses_required_failures_and_never_follows_path_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("scope");
        fs::create_dir(&dir).unwrap();
        let cg = pinned_managed_cgroup(&dir);
        assert!(cg.managed_populated().is_err());
        for invalid in [
            b"".as_slice(),
            b"frozen 0\n",
            b"populated unknown\n",
            b"populated \xff\n",
        ] {
            fs::write(dir.join("cgroup.events"), invalid).unwrap();
            assert!(cg.managed_populated().is_err());
            assert_eq!(fs::read(dir.join("cgroup.events")).unwrap(), invalid);
        }
        fs::remove_file(dir.join("cgroup.events")).unwrap();
        fs::create_dir(dir.join("cgroup.events")).unwrap();
        assert!(cg.managed_populated().is_err());
        fs::remove_dir(dir.join("cgroup.events")).unwrap();
        fs::write(dir.join("cgroup.events"), vec![b'0'; MAX_CGROUP_FILE + 1]).unwrap();
        assert!(cg.managed_populated().is_err());
        fs::write(dir.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!cg.managed_populated().unwrap());
        fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cg.managed_populated().unwrap());
        let retained = tmp.path().join("retained");
        fs::rename(&dir, &retained).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cgroup.events"), "populated 0\n").unwrap();
        assert!(cg.managed_populated().unwrap());
        assert!(BoxCgroup::at(dir.clone()).managed_populated().is_err());
        assert!(!pinned_managed_cgroup(&dir).managed_populated().unwrap());
    }

    /// AC-T7.1.2/R01: cold observation creates no directory, refuses a final symlink/non-directory,
    /// and a clone keeps the same fallibly allocated pin after its old path is reused.
    #[test]
    fn managed_observation_is_read_only_and_keeps_one_same_directory_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert!(
            BoxCgroup::observe_managed(missing.clone())
                .unwrap()
                .is_none()
        );
        assert!(!missing.exists());
        let file = tmp.path().join("not-a-directory");
        fs::write(&file, b"preserved").unwrap();
        assert!(BoxCgroup::observe_managed(file.clone()).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"preserved");
        let dir = tmp.path().join("owned");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cgroup.procs"), "11\n").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(BoxCgroup::observe_managed(link).is_err());
        let first = BoxCgroup::observe_managed(dir.clone()).unwrap().unwrap();
        let cloned = first.clone();
        match (first.fd.as_ref(), cloned.fd.as_ref()) {
            (Some(DirectoryPin::Managed(a)), Some(DirectoryPin::Managed(b))) => {
                assert!(triomphe::Arc::ptr_eq(a, b))
            }
            _ => panic!("managed cold observation must own one fallible native pin"),
        }
        fs::rename(&dir, tmp.path().join("original")).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cgroup.procs"), "99\n").unwrap();
        drop(first);
        assert_eq!(cloned.managed_procs().unwrap(), [11]);
        assert_eq!(
            BoxCgroup::observe_managed(dir)
                .unwrap()
                .unwrap()
                .managed_procs()
                .unwrap(),
            [99]
        );
    }

    /// AC-T7.1.2: every row participates in one complete membership observation. An ambiguous
    /// duplicate (which a changing kernel listing permits) supplies no lifetime/absence proof.
    #[test]
    fn managed_membership_refuses_ambiguous_incomplete_or_over_bound_rows() {
        assert_eq!(parse_managed_procs("", 2).unwrap(), Vec::<u32>::new());
        assert_eq!(parse_managed_procs("11\n5\n", 2).unwrap(), [5, 11]);
        for text in [
            "0\n",
            "-1\n",
            "+1\n",
            "0x1\n",
            "1 \n",
            "\n",
            "5\n5\n",
            "1\n01\n",
            "5",
            "4294967296\n",
            "4194304\n",
            "1\n2\n3\n",
        ] {
            assert!(parse_managed_procs(text, 2).is_err(), "accepted {text:?}");
        }
    }

    /// AC-T7.1.2: equivalent native CPU periods are compared as exact ratios; unlimited,
    /// malformed, missing or enlarged native limits cannot pass as the admitted hard envelope.
    #[test]
    fn managed_limits_compare_actual_ratios_and_retain_canonical_caps() {
        let bounds = Bounds {
            cpu_max_milli: Some(1000),
            memory_max: Some(8192),
            ..Bounds::default()
        };
        for cpu in [
            "100000 100000\n",
            "70000 70000\n",
            "40000 100000\n",
            "18446744073709551615 18446744073709551615\n",
        ] {
            check_managed_limits(&bounds, cpu, "4096\n", "0\n").unwrap();
        }
        for (cpu, memory, swap) in [
            ("100001 100000\n", "4096\n", "0\n"),
            ("max 100000\n", "4096\n", "0\n"),
            ("0 100000\n", "4096\n", "0\n"),
            ("1 0\n", "4096\n", "0\n"),
            ("100000 100000 extra\n", "4096\n", "0\n"),
            ("100000 100000\n", "max\n", "0\n"),
            ("100000 100000\n", "8193\n", "0\n"),
            ("100000 100000\n", "0\n", "0\n"),
            ("100000 100000\n", "4096\n", "1\n"),
            ("100000 100000\n", "4096\n", "max\n"),
        ] {
            assert!(check_managed_limits(&bounds, cpu, memory, swap).is_err());
        }
        assert!(
            check_managed_limits(&Bounds::default(), "100000 100000\n", "4096\n", "0\n").is_err()
        );
        assert_eq!(bounds.cpu_max_milli, Some(1000));
        assert_eq!(bounds.memory_max, Some(8192));
    }

    /// AC-T7.1.2/HEX-RT-15: strict metadata reads the actual pinned directory, never a replacement
    /// name, and required read/encoding/parse failures remain refusals with ordinary procs intact.
    #[test]
    fn managed_metadata_keeps_pinned_identity_and_required_read_failures() {
        fn files(dir: &Path) {
            let memory = format!("{}\n", hex_api::MIN_MEMORY);
            for (file, text) in [
                ("cgroup.procs", "11\n5\n"),
                ("cpu.max", "100000 100000\n"),
                ("memory.max", memory.as_str()),
                ("memory.swap.max", "0\n"),
                ("pids.max", "4\n"),
            ] {
                fs::write(dir.join(file), text).unwrap();
            }
        }
        let bounds = Bounds {
            cpu_max_milli: Some(1000),
            memory_max: Some(hex_api::MIN_MEMORY),
            pids_max: Some(4),
            ..Bounds::default()
        };
        bounds.check().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("managed");
        fs::create_dir(&dir).unwrap();
        files(&dir);
        let cg = pinned_managed_cgroup(&dir);
        assert_eq!(cg.managed_procs().unwrap(), [5, 11]);
        cg.check_managed_bounds(&bounds).unwrap();
        let old = tmp.path().join("original");
        fs::rename(&dir, &old).unwrap();
        fs::create_dir(&dir).unwrap();
        files(&dir);
        fs::write(dir.join("cgroup.procs"), "99\n").unwrap();
        assert_eq!(cg.managed_procs().unwrap(), [5, 11]);
        assert!(BoxCgroup::at(dir).managed_procs().is_err());
        for file in [
            "cgroup.procs",
            "cpu.max",
            "memory.max",
            "memory.swap.max",
            "pids.max",
        ] {
            for damage in 0..4 {
                files(&old);
                let path = old.join(file);
                fs::remove_file(&path).unwrap();
                match damage {
                    0 => {}
                    1 => fs::create_dir(&path).unwrap(),
                    2 => fs::write(&path, [0xff]).unwrap(),
                    _ => fs::write(&path, "malformed\n").unwrap(),
                }
                if file == "cgroup.procs" {
                    assert!(cg.managed_procs().is_err());
                } else {
                    assert!(cg.check_managed_bounds(&bounds).is_err());
                }
                if damage == 1 {
                    fs::remove_dir(&path).unwrap();
                }
            }
        }
        files(&old);
        fs::write(old.join("cgroup.procs"), "11\ninvalid\n5\n").unwrap();
        assert_eq!(
            cg.procs().unwrap(),
            [11, 5],
            "legacy filtering behavior is unchanged"
        );
        assert!(cg.managed_procs().is_err());
        files(&old);
        fs::write(old.join("pids.max"), "5\n").unwrap();
        assert!(cg.check_managed_bounds(&bounds).is_err());
    }

    /// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13, ruling B: no VM has a memory.max, cold or warm) with HEX-RT-15:
    /// a VM's scope holds no canonical memory bound of its own, its memory held by its budget's memory.high, the last
    /// resort and its fleet's budget, while its swap stays off, so all it holds is RAM. Do check, against a VM's
    /// bounds with no memory bound, its scope with memory.max "max" (as its first window leaves it) and with the value
    /// an earlier create wrote, each with swap off; then with swap on. Expect the first two passing and the last
    /// refused.
    #[test]
    fn w14_a_vm_scope_has_no_memory_bound_of_its_own_and_no_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("managed");
        fs::create_dir(&dir).unwrap();
        let bounds = Bounds {
            cpu_max_milli: Some(1000),
            ..Bounds::default()
        };
        let write = |memory: &str, swap: &str| {
            for (file, text) in [
                ("cgroup.procs", "5\n"),
                ("cpu.max", "100000 100000\n"),
                ("memory.max", memory),
                ("memory.swap.max", swap),
            ] {
                fs::write(dir.join(file), text).unwrap();
            }
        };
        let cg = pinned_managed_cgroup(&dir);
        write("max\n", "0\n");
        cg.check_managed_bounds(&bounds).unwrap();
        write(&format!("{}\n", hex_api::MIN_MEMORY), "0\n");
        cg.check_managed_bounds(&bounds).unwrap();
        write("max\n", "max\n");
        assert!(cg.check_managed_bounds(&bounds).is_err());
    }

    /// AC-T7.1.2/HEX-RT-15: native backing observation uses the pinned cgroup, never an unrelated
    /// replacement at its old name. An unpinned path is insufficient cleanup/accounting authority.
    #[test]
    fn managed_usage_reads_the_pinned_native_cgroup_after_path_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("managed");
        fs::create_dir(&dir).unwrap();
        managed_counter_files(&dir, "2048\n");
        let old = pinned_managed_cgroup(&dir);
        assert_eq!(old.managed_usage().unwrap().memory_bytes, 2048);
        fs::rename(&dir, tmp.path().join("original")).unwrap();
        fs::create_dir(&dir).unwrap();
        managed_counter_files(&dir, "0\n");
        assert_eq!(old.managed_usage().unwrap().memory_bytes, 2048);
        assert!(BoxCgroup::at(dir.clone()).managed_usage().is_err());
        assert_eq!(
            pinned_managed_cgroup(&dir)
                .managed_usage()
                .unwrap()
                .memory_bytes,
            0
        );
    }

    /// AC-T7.1.2: successful empty I/O is zero; absent, unreadable, non-UTF-8 and malformed required
    /// files refuse measured backing usage. Ordinary optional-counter behavior remains unchanged.
    #[test]
    fn managed_usage_refuses_required_file_failures_instead_of_crediting_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = pinned_managed_cgroup(tmp.path());
        for file in [
            "memory.current",
            "memory.peak",
            "memory.events",
            "pids.current",
            "pids.events",
            "cpu.stat",
            "io.stat",
        ] {
            managed_counter_files(tmp.path(), "2048\n");
            let path = tmp.path().join(file);
            fs::remove_file(&path).unwrap();
            assert!(cg.managed_usage().is_err(), "missing {file}");
            fs::create_dir(&path).unwrap();
            assert!(cg.managed_usage().is_err(), "unreadable {file}");
            fs::remove_dir(&path).unwrap();
            fs::write(&path, [0xff]).unwrap();
            assert!(cg.managed_usage().is_err(), "non-UTF-8 {file}");
            fs::write(&path, "malformed").unwrap();
            assert!(cg.managed_usage().is_err(), "malformed {file}");
        }
        managed_counter_files(tmp.path(), "2048\n");
        assert_eq!(cg.managed_usage().unwrap().io_read_bytes, 0);
        fs::remove_file(tmp.path().join("io.stat")).unwrap();
        assert!(cg.managed_usage().is_err());
        assert_eq!(cg.usage().io_read_bytes, 0, "legacy fallback is preserved");
    }

    /// AC-T7.1.2/CLAUDE §2: the shared reader accepts pre-reserved managed storage without growing
    /// it, and a later read error never publishes the earlier partial counter text.
    #[test]
    fn managed_counter_reader_keeps_its_storage_bound_and_partial_failures() {
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(MAX_CGROUP_FILE + READ_CHUNK)
            .unwrap();
        let pointer = storage.as_ptr();
        let text = b"usage_usec 100\n";
        let got = read_text(
            MAX_CGROUP_FILE,
            |buf, at| {
                if at == 0 {
                    buf[..text.len()].copy_from_slice(text);
                    Ok(text.len())
                } else {
                    Ok(0)
                }
            },
            storage,
        )
        .ok()
        .unwrap();
        assert_eq!(got.as_ptr(), pointer);
        assert_eq!(got.as_bytes(), text);
        let failed = read_text(
            MAX_CGROUP_FILE,
            |buf, at| {
                if at == 0 {
                    buf[..text.len()].copy_from_slice(text);
                    Ok(text.len())
                } else {
                    Err(rustix::io::Errno::IO)
                }
            },
            Vec::new(),
        );
        assert!(matches!(
            failed,
            Err(ReadFailure::Io(rustix::io::Errno::IO))
        ));
        let too_long = read_text(
            MAX_CGROUP_FILE,
            |buf, _| {
                buf.fill(b'0');
                Ok(buf.len())
            },
            Vec::new(),
        );
        assert!(matches!(
            too_long,
            Err(ReadFailure::OverLimit(MAX_CGROUP_FILE))
        ));
    }

    /// HEX-RT-15: once the run supervisor removes a cgroup, its pinned directory must report absence even if
    /// a new cgroup reuses its path. A present cgroup reports population; malformed text and real I/O failures
    /// remain refusals, not absence.
    #[test]
    fn population_distinguishes_a_removed_pinned_cgroup_from_a_reused_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("box");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cgroup.events"), "populated 0\n").unwrap();
        let fd = rustix::fs::open(
            &dir,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let old = BoxCgroup {
            dir: dir.clone(),
            fd: Some(DirectoryPin::Shared(Arc::new(fd))),
            opened: None,
        };
        assert_eq!(old.populated_if_present(), Ok(Some(false)));
        fs::write(dir.join("cgroup.events"), "populated 1\n").unwrap();
        assert_eq!(old.populated_if_present(), Ok(Some(true)));
        fs::remove_file(dir.join("cgroup.events")).unwrap();
        fs::remove_dir(&dir).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cgroup.events"), "populated 1\n").unwrap();
        assert_eq!(old.populated_if_present(), Ok(None));
        let new = BoxCgroup::at(dir.clone());
        assert_eq!(new.populated_if_present(), Ok(Some(true)));
        assert!(
            old.populated().is_err(),
            "the strict API still refuses a removed file"
        );
        fs::write(dir.join("cgroup.events"), "populated malformed\n").unwrap();
        assert!(new.populated_if_present().is_err());
        fs::write(dir.join("cgroup.events"), [0xff]).unwrap();
        assert!(new.populated_if_present().is_err());
        fs::remove_file(dir.join("cgroup.events")).unwrap();
        fs::create_dir(dir.join("cgroup.events")).unwrap();
        assert!(
            new.populated_if_present().is_err(),
            "EISDIR is a read failure, not disappearance"
        );
    }

    /// HEX-RT-15: kernel removal during the bounded read discards even an already-read population line.
    /// Permission, device I/O and directory read errors are refusals, never absent cgroups.
    #[test]
    fn population_reads_treat_only_removed_kernfs_errors_as_absence() {
        use rustix::io::Errno;
        for removed in [Errno::NODEV, Errno::NOENT] {
            let text = b"populated 1\n";
            let got = read_if_present(
                MAX_CGROUP_FILE,
                || p("events"),
                |buf, at| {
                    if at == 0 {
                        buf[..text.len()].copy_from_slice(text);
                        Ok(text.len())
                    } else {
                        Err(removed)
                    }
                },
            );
            assert_eq!(got, Ok(None));
        }
        for failed in [Errno::ACCESS, Errno::IO, Errno::ISDIR] {
            let got = read_if_present(MAX_CGROUP_FILE, || p("events"), |_, _| Err(failed));
            assert!(matches!(
                got,
                Err(Refusal::Os {
                    op: "reading a cgroup file",
                    ..
                })
            ));
        }
    }

    /// HEX-RT-15: a run's cgroup removed between its listing and its events file's open. Kernfs refuses
    /// the open of a node its removal deactivated with ENODEV (`kernfs_fop_open`), and a lookup in the
    /// removed directory finds nothing (ENOENT). Do open as each; expect both absence, never a refusal of
    /// the inspect, and permission, device I/O and descriptor exhaustion still refusals.
    #[test]
    fn population_opens_treat_only_removed_kernfs_errors_as_absence() {
        use rustix::io::Errno;
        let cg = BoxCgroup::at(p("box"));
        for removed in [Errno::NODEV, Errno::NOENT] {
            assert_eq!(cg.populated_from(Err(removed)), Ok(None), "{removed:?}");
        }
        for failed in [Errno::ACCESS, Errno::IO, Errno::MFILE] {
            assert!(
                matches!(
                    cg.populated_from(Err(failed)),
                    Err(Refusal::Os {
                        op: "reading a cgroup file",
                        ..
                    })
                ),
                "{failed:?}"
            );
        }
    }

    /// Goal 5 extended, step 6 (bench/competitive/probes/create_serial.md, run 12): a box's cgroup opened
    /// through its root is reached through the directory it holds, never by its path, so no file of it is a
    /// walk through `/sys` and every kernfs directory above it. Do: open a root and a slot under it, then move
    /// every directory above them away. Expect the slot's files read and written as before, its directory
    /// still there for `clone3`, and the root finding its next slot by name. Plain files stand in for the
    /// cgroup's.
    #[test]
    fn a_cgroup_opened_through_its_root_is_reached_by_its_directory_not_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(parent.join("root")).unwrap();
        std::fs::write(parent.join("cgroup.controllers"), "cpu io memory pids\n").unwrap();
        std::fs::write(parent.join("cgroup.subtree_control"), "").unwrap();
        std::fs::write(parent.join("root/cgroup.subtree_control"), "").unwrap();
        let root = BoxRoot::open(&parent.join("root"), Vec::new()).unwrap();
        let slot = parent.join("root").join(reuse::name(7));
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        std::fs::write(slot.join("cgroup.procs"), "").unwrap();
        std::fs::write(slot.join("cgroup.kill"), "").unwrap();
        let cg = root.slot(7).expect("slot 7");

        let moved = tmp.path().join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        assert_eq!(
            cg.populated(),
            Ok(false),
            "cgroup.events, read through the slot"
        );
        assert_eq!(cg.procs(), Ok(Vec::new()), "cgroup.procs");
        assert_eq!(cg.kill(), Ok(()), "cgroup.kill, written");
        let kill = moved.join("root").join(reuse::name(7)).join("cgroup.kill");
        assert_eq!(std::fs::read_to_string(kill).unwrap(), "1");
        assert!(cg.open_dir().is_ok(), "the slot's directory, for clone3");
        std::fs::create_dir(moved.join("root").join(reuse::name(8))).unwrap();
        assert!(
            root.slot(8).is_some(),
            "the next slot, found by its name under the root"
        );
        assert!(root.slot(9).is_none(), "and none that isn't there");
    }

    /// AC-T7.1.4 (delete ends every process), CLAUDE.md §2; Ada's 2026-11-06 rule (no picked constants): the kill
    /// sweep's bound on cgroups is the box's own, learned from the kernel, not a number picked for every box: its
    /// `cgroup.max.descendants` where it sets one, which the kernel never lets its subtree pass, else its
    /// `cgroup.stat`'s `nr_descendants` as the sweep begins. Do: kill a box still populated after `cgroup.kill`, two
    /// cgroups under it, its `cgroup.stat` counting one and its `cgroup.max.descendants` `max`; then again with
    /// `cgroup.max.descendants` 2. Expect: the first refused past the box and the one it counted (`Full(2)`), not
    /// walked on; the second swept. Plain files stand in for the cgroup's.
    #[test]
    fn the_kill_sweeps_bound_is_the_boxs_own() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("box");
        for sub in ["", "a", "b"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::write(dir.join(sub).join("cgroup.procs"), "").unwrap();
        }
        std::fs::write(dir.join("cgroup.kill"), "").unwrap();
        std::fs::write(dir.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        std::fs::write(
            dir.join("cgroup.stat"),
            "nr_descendants 1\nnr_dying_descendants 0\n",
        )
        .unwrap();
        std::fs::write(dir.join("cgroup.max.descendants"), "max\n").unwrap();
        let cg = BoxCgroup::at(dir.clone());
        assert_eq!(cg.kill(), Err(Refusal::Full(2)));
        std::fs::write(dir.join("cgroup.max.descendants"), "2\n").unwrap();
        assert_eq!(cg.kill(), Ok(()));
    }

    /// AC-T7.1.4 (delete ends every process), CLAUDE.md §2; Ada's 2026-11-06 rule (no picked constants): the most
    /// processes a box's signal or sweep is sent to is the kernel's ceiling on pids, so no box this host can make
    /// holds more. Do: read this host's `pid_max` (proc(5), at most `PID_MAX_LIMIT`). Expect: the bound no lower.
    #[test]
    fn the_signal_bound_is_no_lower_than_the_hosts_pid_max() {
        let pid_max: usize = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(MAX_SIGNALLED >= pid_max, "{MAX_SIGNALLED} < {pid_max}");
    }

    /// AC-T7.1.4, CLAUDE.md §2; Ada's 2026-11-06 rule (no picked constants): a box's `cgroup.procs` is bounded by the
    /// most processes a box can hold, not by the bound on the files of a few lines beside it. Do: list a box's
    /// processes when its `cgroup.procs` holds 10,000 of the widest pids there are, 80,000 bytes, past the 64 KiB once
    /// picked for every cgroup file. Expect: every one listed.
    #[test]
    fn a_box_with_many_processes_has_them_all_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let widest = PID_MAX_LIMIT - 1;
        let text = format!("{widest}\n").repeat(10_000);
        assert!(text.len() > 64 << 10);
        std::fs::write(tmp.path().join("cgroup.procs"), text).unwrap();
        let procs = BoxCgroup::at(tmp.path().to_owned()).procs().unwrap();
        assert_eq!(procs.len(), 10_000);
    }

    #[test]
    fn sweep_collects_every_process_in_the_subtree() {
        let tree = Mock {
            children: BTreeMap::from([(p("/b"), vec![p("/b/a"), p("/b/c")])]),
            procs: BTreeMap::from([(p("/b"), vec![10]), (p("/b/a"), vec![20, 21])]),
            io_error_at: None,
        };
        let got = sweep_targets(&p("/b"), &tree, 16, 16).unwrap();
        let pids: BTreeSet<u32> = got.iter().map(|(_, pid)| *pid).collect();
        assert_eq!(pids, BTreeSet::from([10, 20, 21]));
        assert!(
            got.contains(&(p("/b/a"), 20)),
            "a child's process carries its cgroup"
        );
    }

    #[test]
    fn sweep_refuses_past_the_cgroup_budget() {
        let tree = Mock {
            children: BTreeMap::from([(p("/b"), vec![p("/b/1"), p("/b/2"), p("/b/3"), p("/b/4")])]),
            procs: BTreeMap::new(),
            io_error_at: None,
        };
        assert_eq!(sweep_targets(&p("/b"), &tree, 3, 16), Err(Refusal::Full(3)));
    }

    #[test]
    fn sweep_refuses_past_the_process_budget() {
        let tree = Mock {
            children: BTreeMap::new(),
            procs: BTreeMap::from([(p("/b"), vec![1, 2, 3, 4])]),
            io_error_at: None,
        };
        assert_eq!(sweep_targets(&p("/b"), &tree, 16, 2), Err(Refusal::Full(2)));
    }

    #[test]
    fn sweep_returns_a_non_vanishing_io_failure() {
        let tree = Mock {
            children: BTreeMap::new(),
            procs: BTreeMap::new(),
            io_error_at: Some(p("/b")),
        };
        let err = sweep_targets(&p("/b"), &tree, 16, 16).unwrap_err();
        assert!(
            matches!(
                err,
                Refusal::Os {
                    op: "listing a box's cgroups",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn sweep_skips_a_vanished_cgroup() {
        // /b names a child /b/gone that is absent from the maps (vanished): it is skipped, not an error.
        let tree = Mock {
            children: BTreeMap::from([(p("/b"), vec![p("/b/gone")])]),
            procs: BTreeMap::from([(p("/b"), vec![7])]),
            io_error_at: None,
        };
        let got = sweep_targets(&p("/b"), &tree, 16, 16).unwrap();
        assert_eq!(got, vec![(p("/b"), 7)]);
    }

    /// The test's own cgroup, from `/proc/self/cgroup` (cgroup v2's `0::<path>`).
    fn own_cgroup() -> PathBuf {
        let text = fs::read_to_string("/proc/self/cgroup").unwrap();
        let path = text
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .unwrap()
            .trim_start_matches('/');
        Path::new(CGROUP_ROOT).join(path)
    }

    /// Found by the OCI validation lab (docs/design/shards-guest.md §6a, HEX-GUEST-20): a container with
    /// a cgroup namespace and a writable cgroup2 mount makes cgroups of its own under its box's. Make
    /// a box's cgroup with cgroups nested two deep beneath it, as the suite's inside.sh does, and remove
    /// the box: expect every one gone, deepest first, and the box's own.
    #[test]
    fn a_box_is_removed_with_the_cgroups_its_container_made_beneath_it() {
        let parent = own_cgroup().join(format!("hex-remove-test-{}", std::process::id()));
        if let Err(e) = fs::create_dir(&parent) {
            eprintln!(
                "SKIP: this test's cgroup {} takes no child ({e})",
                parent.display()
            );
            return;
        }
        let dir = parent.join("box");
        for sub in ["", "lab", "lab/t1", "lab/t2", "other"] {
            fs::create_dir(dir.join(sub)).unwrap();
        }
        let got = BoxCgroup::at(dir.clone()).remove();
        let left = dir.exists();
        let _ = fs::remove_dir(&parent);
        assert_eq!(got, Ok(()));
        assert!(!left, "the box's cgroup is left");
    }
}
