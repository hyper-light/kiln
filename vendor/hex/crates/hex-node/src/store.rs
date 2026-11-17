//! The node agent's store of box records, one file each. A record is kept in one of two places, by
//! what it names, and how it is written follows. One naming a directory on a filesystem a host crash
//! leaves behind must outlive the host, so it is kept in the node's state directory, fsynced file and
//! directory, and written whole to a temp file and renamed into place: a crash of the node agent
//! leaves either the old record or the new one, never a torn one (T-T7.1.2), because the page cache
//! outlives the process and rename(2) replaces atomically. Every other record names only directories
//! on tmpfs (an `oci` box's container state, under the node's runtime directory) or none (a `process`
//! box's), so a host crash leaves nothing it describes; it is kept beside that state, in a directory of
//! the node's own under the runtime directory, and never synced, where a record-sized write measured
//! about a seventh of one on the overlay or btrfs (bench/competitive/probes/levers.md; goal 5 extended
//! step 5, research f92 and f93).
//!
//! A box's record is written once over its create (goal 5 extended step 5), so that one write to a
//! non-durable record goes straight to its file with `O_EXCL` — no temp, no rename (criterion 3) — and
//! only a later write of the same record, an update, takes the temp-and-rename path every durable
//! record takes. Writing in place opens a window a durable record never has: a node crash between the
//! file's open and the write leaves a torn record, short of the trailing newline its encoding ends on,
//! that never decodes. Recovery covers it. A torn record names only tmpfs directories, so a host crash
//! takes it with everything it described; a node-only crash leaves it on tmpfs, where recovery reads
//! it, fails to decode it, and removes its box as unreadable (recovery's `Why::Unreadable`). So
//! recovery scrubs each record it finds, torn or whole, and only a record naming a directory on disk —
//! always written whole — points at anything a host crash leaves behind to remove
//! (docs/design/runtime-architecture.md, decision 1).
//!
//! On macOS and Windows the node has no tmpfs runtime directory: those records go to `@records` under its
//! state directory, unsynced, every write of them to a temp file renamed into place. A crash of the node
//! leaves the old record or the new one there too; a crash of the host can leave one torn, its rename on
//! disk before its bytes. Whose box it named, and whether that box ended, recovery proves where it can and
//! holds the record where it can't (`crate::recorded`). Only a record in the state directory that can't be
//! read is never a crash's (every write there is whole and synced): damage the node won't start over.
//!
//! Changes of one box's record are made one at a time; changes of different boxes never wait on each
//! other, so one box's fsync never holds up another box's create (goal 5 plan step 2). Nor do they wait on
//! one directory's lock: the runtime directory's records are spread over [`SHARDS`] subdirectories by
//! their box's id (goal 5 extended, step 6).
//!
//! A store is its state directory's one writer: it holds the directory from its open until its last clone drops, and a
//! second store over it, in this process or another, is refused at its open, naming it ([`lock`]). So the store keeps
//! every box record and node VM row in memory, its index ([`index`]): read once by its open's scan, and changed by each
//! write in the write's own turn, once the file holds it, so every read of a record is the index's and none reads a
//! file (the review's per-window reads, step 2). The index is no cache of the directory: nothing else writes a record,
//! so it is the directory's state. A file changed behind the store's back is outside that contract and is seen by the
//! next store's open, as recovery reads it now. Its memory is the records', within the store's bounds: at most
//! [`TURNS`] box records and as many rows, each within the record bound, held decoded, about their size on disk.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(all(debug_assertions, target_os = "linux"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use hex_api::{BoxId, Refusal};
use hex_core::patience::Patience;
use hex_core::record::{BootSession, BoxRecord, Phase};

use crate::turns::{Turn, Turns};

mod index;
mod lock;
mod node_vm;
mod pool;
pub use index::{Entry, Snapshot};
pub use node_vm::{NodeVmEnds, NodeVmSettled, NodeVmsSettled};
use pool::scoped_paths;

/// How many subdirectories the runtime directory's records are spread over, by their box's id: 64, the shard's
/// name two hex digits of [`SHARD_MASK`]. Each create makes its box's record file and each delete removes it, and
/// a directory makes a name or removes one only under its own inode's lock, held exclusively (Linux v6.19
/// fs/namei.c:4545-4546 for an open that may create, :2873 for an unlink, from :5429), so with every record in
/// one directory the boxes made and deleted at once queued on it: 0.548 ms a lifecycle waiting to write it and
/// 0.189 to read it at concurrency 32 on fedora's E-cores (bench/competitive/probes/create_serial.md, run 19).
/// Derived from a record, not learned (Ada's rule, 2026-11-06): over 64 shards those waits were 0.000 ms at the same
/// concurrency (run 20), a shard's lock no longer measurable among the lifecycle's waits. It can't be learned as
/// the node starts: where a record lives is its id's hash over this count, so a node restarted on a host of another
/// parallelism, or recovery after a crash, must find each record where the last node put it, and a count that
/// changed would need every record moved first (the store's layout, like a wire format, changes append-only).
/// What would falsify it: a shard's directory measurable in `create_serial.md`'s locks table at the most writers a
/// node runs at once; the remedy then is a layout change with its migration, not a learned count.
pub const SHARDS: usize = 64;
/// The bits of a box id's hash that name its shard, [`SHARDS`] of them.
const SHARD_MASK: u64 = 0x3f;

/// Most boxes whose records are being changed at once: one for each box the node can hold, its box
/// budget ([`hex_runtime::MAX_BOXES`]), so the changes of the boxes a node holds never reach it. Past it
/// a change is refused by type ([`Refusal::Exhausted`]), never kept waiting (CLAUDE.md §2).
pub const TURNS: usize = hex_runtime::MAX_BOXES;

/// The records torn in the runtime place, each with why it couldn't be read ([`Store::take_torn`]).
pub(crate) type Torn = Vec<(BoxId, String)>;

/// The boot session marker's name in the state directory, and its write's temp ([`Store::take_boot_marker`]):
/// `@` is in no box id, so neither is read as a box's record.
const BOOT_MARKER: &str = "@boot";
const BOOT_MARKER_TEMP: &str = ".@boot.tmp";

/// The host's boot session as its kernel names it, read once a process, as no process outlives its boot:
/// Linux's `/proc/sys/kernel/random/boot_id`, macOS's `kern.bootsessionuuid`, Windows' boot sequence
/// (`KUSER_SHARED_DATA`'s `BootId`). `Err` says why it couldn't be read; `None` where the OS names none, so its
/// records keep recovery's other rules.
pub(crate) fn host_boot() -> &'static Result<Option<BootSession>, String> {
    static HOST_BOOT: OnceLock<Result<Option<BootSession>, String>> = OnceLock::new();
    HOST_BOOT.get_or_init(read_host_boot)
}

#[cfg(target_os = "linux")]
fn read_host_boot() -> Result<Option<BootSession>, String> {
    let path = "/proc/sys/kernel/random/boot_id";
    let text = fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    BootSession::new(&text)
        .map(Some)
        .map_err(|e| format!("{path}: {e}"))
}

/// Read by sysctlbyname: no program runs, so none can inherit a descriptor another thread holds.
#[cfg(target_os = "macos")]
fn read_host_boot() -> Result<Option<BootSession>, String> {
    let text = hex_runtime::macos::sys::sysctl_string(c"kern.bootsessionuuid")
        .map_err(|e| format!("reading kern.bootsessionuuid: {e}"))?;
    BootSession::new(&text)
        .map(Some)
        .map_err(|e| format!("kern.bootsessionuuid: {e}"))
}

/// Windows' boot sequence, in decimal ([`hex_runtime::windows::sys::boot_id`]), read from the page the kernel maps into
/// every process: no program runs and no handle opens. A 0 is taken for none read, the reading that removes nothing:
/// records then carry no session and keep recovery's other rules.
#[cfg(windows)]
fn read_host_boot() -> Result<Option<BootSession>, String> {
    match hex_runtime::windows::sys::boot_id() {
        0 => Err("KUSER_SHARED_DATA's BootId reads 0, no boot sequence".to_owned()),
        id => BootSession::new(&id.to_string())
            .map(Some)
            .map_err(|e| format!("KUSER_SHARED_DATA's BootId: {e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_host_boot() -> Result<Option<BootSession>, String> {
    Ok(None)
}

/// `record` as this store writes it in this boot of the host: carrying its session ([`host_boot`]).
#[cfg(test)]
pub(crate) fn in_this_boot(record: BoxRecord) -> BoxRecord {
    BoxRecord {
        boot: host_boot().clone().ok().flatten(),
        ..record
    }
}

/// Flushes the state directory `dir` after the boot session marker's rename or removal, so it outlives a
/// host crash; apart from the records' own counted flushes ([`Store::syncs`]). On Windows the directory is
/// opened as FlushFileBuffers asks ([`hex_runtime::windows::sys::flush_directory`]).
fn sync_marker_dir(dir: &Path) -> Result<(), Refusal> {
    #[cfg(windows)]
    let flushed = hex_runtime::windows::sys::flush_directory(dir);
    #[cfg(not(windows))]
    let flushed = File::open(dir).and_then(|d| d.sync_all());
    flushed.map_err(|e| Refusal::os("flushing the boot session marker's directory", e))
}

/// Box records, one file each, named `<id>.json`, in the place each belongs ([`Store`]).
#[derive(Debug, Clone)]
pub struct Store {
    /// Where the records that must outlive the host are kept, each fsynced: the node's state
    /// directory.
    disk: PathBuf,
    /// Where every other record is kept, none synced: a directory of the node's own under its runtime
    /// directory.
    run: PathBuf,
    /// Two descriptors per store, shared by clones. Relative names keep the runtime directory's
    /// prefix out of each record's open and rename (decision 1, goal 5 extended step 5).
    #[cfg(target_os = "linux")]
    disk_fd: Arc<File>,
    #[cfg(target_os = "linux")]
    run_fd: Arc<File>,
    /// The boxes whose records are being changed now, one change each: [`Store::put`],
    /// [`Store::update`] and [`Store::remove`] of a box wait for the change of it under way, so an
    /// update's read and write of a record are one change and two users changing one record at once
    /// (the supervisor recording an init's exit, the socket counting an abandoned program) never lose
    /// either's change. A change of another box never waits. At most [`TURNS`] of them.
    turns: Arc<Turns>,
    /// The fsyncs the store has made since it opened.
    synced: Arc<Synced>,
    /// A debug build's one crash point in the store: once armed, the node ends its own process after
    /// it opens a non-durable record's file and before it writes, so T-T7.1.2's crash test leaves a
    /// torn record ([`crate::Node::with_crash_point`]). Shared by clones, as every field is.
    #[cfg(all(debug_assertions, target_os = "linux"))]
    crash_before_write: Arc<AtomicBool>,
    /// The host's boot session ([`host_boot`]), which every record it writes carries; `None` where it
    /// can't be read, with why in [`Store::boot_unread`].
    boot: Option<BootSession>,
    /// The next place in the node's start order (joint plan step 14, C3) the store gives a VM lifetime it begins
    /// ([`BoxRecord::started`]): one past the most any of its records held when it opened, so the order runs on
    /// across the node's restarts. Shared by clones.
    begun: Arc<AtomicU64>,
    /// The node VMs this process runs (joint plan step 39), each by its run with the one handle of its lock file, whose
    /// owner-only lock it holds from its row's Booting write until its Ended one ([`node_vm`]). Shared by clones.
    node_vm_owners: Arc<Mutex<BTreeMap<hex_api::NodeRun, File>>>,
    /// Every record and row the directory holds ([`index`]), changed by each write in its turn. Shared by clones.
    index: Arc<index::Index>,
    /// The store's hold of its directory, its one writer's ([`lock`]): let go as its last clone drops.
    _hold: Arc<lock::Hold>,
}

#[derive(Debug, Default)]
struct Synced {
    // The existing preparation owner fences strict metadata snapshots through publication.
    // Ordinary writes share it; preparation takes it exclusively, without waiting. It holds no
    // inventory/native callback and creates no second reference or resource authority.
    pool_preparation: RwLock<()>,
    files: AtomicU64,
    dirs: AtomicU64,
    /// Record files read: box records, node VM rows and pool rows.
    reads: AtomicU64,
}

/// How far a write of a box's record got ([`Store::write_encoded`]).
#[derive(Debug, Clone, Copy)]
enum Wrote {
    /// Its file is as it was.
    Nothing,
    /// Its file may hold part of the bytes: a first write in its place, cut short (Linux's, where a record that
    /// needn't outlive the host is written in place once).
    #[cfg(target_os = "linux")]
    Part,
    /// Its file holds the bytes written.
    Whole,
}

/// The fsyncs a store has made since it opened: of record files and of its directory, one of each per
/// write of a record that must outlive the host. Non-vacuity counters (CLAUDE.md §6) for the tests of
/// which records are durable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Syncs {
    pub files: u64,
    pub dirs: u64,
}

impl Store {
    /// The existing owner's persistent state directory, also its native capacity volume.
    pub(crate) fn directory(&self) -> &Path {
        &self.disk
    }

    /// Opens the store over the node's state directory `disk`, for the records that must outlive the
    /// host, and `run`, a directory of the node's own under its runtime directory for the rest, making
    /// each when it's missing. `run` is readable by the node alone: a record holds its box's spec, an
    /// init's environment among it.
    pub fn open(disk: &Path, run: &Path) -> Result<Self, Refusal> {
        fs::create_dir_all(disk).map_err(|e| Refusal::os("making the record directory", e))?;
        fs::create_dir_all(run)
            .and_then(|()| owner_only(run))
            .map_err(|e| Refusal::os("making the runtime record directory", e))?;
        for n in 0..=SHARD_MASK {
            let shard = run.join(format!("{n:02x}"));
            fs::create_dir_all(&shard)
                .and_then(|()| owner_only(&shard))
                .map_err(|e| Refusal::os("making a shard of the runtime record directory", e))?;
        }
        // Before any record is read: the records this store holds in memory are its directory's only while it is the
        // directory's one writer.
        let hold = lock::Hold::take(disk)?;
        let store = Self {
            disk: disk.to_owned(),
            run: run.to_owned(),
            #[cfg(target_os = "linux")]
            disk_fd: Arc::new(open_directory(disk)?),
            #[cfg(target_os = "linux")]
            run_fd: Arc::new(open_directory(run)?),
            // A change waits for the change of the same box before it: that one's write and its fsyncs, made by
            // this process's own thread. The slowest hold is learned from the changes made (decision 9); a hold
            // past it is a run-out, told to the census, and the wait follows the holder until it lets go, as before
            // the first change is let go: a live holder is waited for, so changes of one box made at once all land
            // (HEX-SOCK-6). A disk that has stopped answering holds its writer in the kernel; refusing that wait,
            // typed, takes the hold's progress, owed (`docs/research/runtime/learned-waits.md`'s sites). No record
            // is read (the `store.turns` rows of `bench/competitive/probes/waits_census.md` are evidence, never a
            // seed).
            turns: Arc::new(Turns::new(
                "changing a box's record",
                "record changes under way",
                TURNS,
                Patience::measuring("store.turns"),
            )),
            synced: Arc::default(),
            #[cfg(all(debug_assertions, target_os = "linux"))]
            crash_before_write: Arc::default(),
            boot: host_boot().as_ref().ok().cloned().flatten(),
            begun: Arc::new(AtomicU64::new(1)),
            node_vm_owners: Arc::default(),
            index: Arc::default(),
            _hold: Arc::new(hold),
        };
        store.load()?;
        store.count_starts()?;
        Ok(store)
    }

    /// Reads every box record and node VM row into the store's index as it opens: the one scan of its records, after
    /// which every read is the index's (the review's per-window reads, step 2). Refused past the store's box bound, as
    /// its records past it could never be recovered ([`Store::retained_records`]).
    fn load(&self) -> Result<(), Refusal> {
        let boxes = self.scan_records(false)?;
        if boxes.len() > TURNS {
            return Err(Refusal::Full(TURNS));
        }
        let rows = self.scan_node_vms()?;
        self.index.load(boxes, rows)
    }

    /// Arms this store's one crash point ([`Store::crash_before_write`]): a debug build's node ends its
    /// own process the next time it opens a non-durable record's file, before it writes, so T-T7.1.2's
    /// crash test leaves a torn record for recovery. Debug builds only.
    #[cfg(all(debug_assertions, target_os = "linux"))]
    pub fn crash_before_first_write(&self) {
        self.crash_before_write.store(true, Ordering::Relaxed);
    }

    /// Box `id`'s turn to change its record: waits for a change of the same box under way, never for
    /// another box's, bounded by the slowest hold the store's changes taught it and following the holder
    /// past it ([`Turns::take`]); refused past [`TURNS`] boxes changed at once.
    fn claim(&self, id: &BoxId) -> Result<Turn<'_>, Refusal> {
        self.turns.take(id)
    }

    /// All reference publication paths share the same short metadata owner as Pool removal.
    /// No reference can appear after its removal scan and before unlink. Source precedes Box.
    fn claim_backing(&self, record: &BoxRecord) -> Result<Option<Turn<'_>>, Refusal> {
        match record.spec.route.and_then(|route| route.backing) {
            Some(key) => self.turns.take_pool(&key).map(Some),
            None => Ok(None),
        }
    }

    fn changing_metadata(&self) -> Result<RwLockReadGuard<'_, ()>, Refusal> {
        self.still_held()?;
        self.synced.pool_preparation.try_read().map_err(|error| {
            metadata_refusal(match error {
                TryLockError::WouldBlock => {
                    "a backing preparation owns the Store metadata snapshot"
                }
                TryLockError::Poisoned(_) => "the Store metadata preparation fence is poisoned",
            })
        })
    }

    fn preparing_metadata(&self) -> Result<RwLockWriteGuard<'_, ()>, Refusal> {
        self.still_held()?;
        self.synced.pool_preparation.try_write().map_err(|error| {
            metadata_refusal(match error {
                TryLockError::WouldBlock => {
                    "a Store metadata mutation prevents backing preparation"
                }
                TryLockError::Poisoned(_) => "the Store metadata preparation fence is poisoned",
            })
        })
    }

    /// Refused once a test's crash let the store's hold go ([`Store::end_as_a_crash`]): a store whose process ended writes
    /// nothing, so the store opened after it is its directory's one writer. Every change of the store takes it first,
    /// through [`Store::changing_metadata`] or [`Store::preparing_metadata`].
    fn still_held(&self) -> Result<(), Refusal> {
        #[cfg(all(debug_assertions, feature = "test-controls"))]
        if self._hold.ended() {
            return Err(Refusal::Broken(
                "the record store's process ended (a test's crash): it writes nothing more"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Lets the store's hold of its directory go as its process's end would (T-T7.1.2: a test's crash in this process,
    /// [`crate::Node::crash`]): a store opened after over the directory takes it, and this store, every clone of it,
    /// writes nothing more ([`Store::still_held`]). A test control of debug builds alone, on Linux, where `Node` is.
    #[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
    pub(crate) fn end_as_a_crash(&self) {
        self._hold.end();
    }

    /// How many changes of a box's record have waited for a change of the same box to finish.
    pub fn waited(&self) -> u64 {
        self.turns.waited()
    }

    /// Where `record` is kept, and whether it's fsynced there: the state directory for a record that
    /// must outlive the host ([`must_outlive_host`]), else the runtime directory's.
    fn place(&self, record: &BoxRecord) -> (&Path, bool) {
        if must_outlive_host(record) {
            (&self.disk, true)
        } else {
            (&self.run, false)
        }
    }

    /// Box `id`'s record files in the two places it may be kept: the runtime directory's, in its shard, then the
    /// state directory's.
    fn files(&self, id: &BoxId) -> [PathBuf; 2] {
        [
            record_file(&self.run.join(shard(id)), id),
            record_file(&self.disk, id),
        ]
    }

    /// The directories records are found in, each listed whole by recovery: the state directory, then each of the
    /// runtime directory's shards.
    fn listed(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.disk.clone()];
        dirs.extend((0..=SHARD_MASK).map(|n| self.run.join(format!("{n:02x}"))));
        dirs
    }

    /// The fsyncs the store has made since it opened.
    pub fn syncs(&self) -> Syncs {
        Syncs {
            files: self.synced.files.load(Ordering::Relaxed),
            dirs: self.synced.dirs.load(Ordering::Relaxed),
        }
    }

    /// The record files the store has read since it opened, box records, node VM rows and pool rows alike: a
    /// non-vacuity counter (CLAUDE.md §6) for the tests of what a read costs (the review's per-window reads).
    pub fn reads(&self) -> u64 {
        self.synced.reads.load(Ordering::Relaxed)
    }

    /// `file`, a record file, read by the one bounded reader, and counted ([`Store::reads`]).
    fn read_bytes(&self, file: File) -> Result<Vec<u8>, Refusal> {
        self.synced.reads.fetch_add(1, Ordering::Relaxed);
        read_record_bytes(file)
    }

    /// Writes `record` whole, in its place: either the old file or the new one survives a crash of
    /// the node agent, and a record that must outlive the host ([`must_outlive_host`]) is fsynced too.
    pub fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _source = self.claim_backing(record)?;
        let _turn = self.claim(&record.spec.id)?;
        if let Some(current) = self.read_lifecycle(&record.spec.id)?
            && current.spec.route.and_then(|route| route.backing)
                != record.spec.route.and_then(|route| route.backing)
        {
            return Err(uncertain(
                &record.spec.id,
                "its record names another backing than the one it is put with",
            ));
        }
        let _backing_phase = self.check_box_backing(record)?;
        self.write(record)
    }

    /// Claims an unused identity before a runtime effect. An unfinished or unreadable record owns
    /// its name too: retrying create cannot replace the only durable cleanup/binding identity.
    pub(crate) fn begin(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _source = self.claim_backing(record)?;
        let _turn = self.claim(&record.spec.id)?;
        if self.get(&record.spec.id)?.is_some() {
            return Err(Refusal::Exists(record.spec.id.try_clone()?));
        }
        let _backing_phase = self.check_box_backing(record)?;
        self.write_fresh(record)
    }

    /// Publishes only the create still described by this immutable record. A concurrent delete's
    /// durable intent wins; an old completion must never turn Deleting back into Ready.
    pub(crate) fn made(&self, expected: &BoxRecord) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _source = self.claim_backing(expected)?;
        let _turn = self.claim(&expected.spec.id)?;
        let Some(Ok(mut record)) = self.get(&expected.spec.id)? else {
            return Err(Refusal::Broken(
                "a create lost its durable record".to_owned(),
            ));
        };
        if record.spec != expected.spec || record.phase != Phase::Creating {
            return Err(Refusal::Invalid {
                what: "box lifecycle",
                why: "create completion disagrees with the retained identity or deletion intent"
                    .to_owned(),
            });
        }
        record.phase = Phase::Ready;
        let _backing_phase = self.check_box_backing(&record)?;
        self.write(&record)
    }

    /// The initialized adapters share this store under the node's existing method/device turns.
    /// Record turns stay short and separate; none is reacquired through a public store mutation.
    pub(crate) fn read_lifecycle(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        match self.get(id)? {
            Some(Ok(record)) if &record.spec.id == id => Ok(Some(record)),
            None => Ok(None),
            Some(_) => Err(uncertain(
                id,
                "its record can't be read as its own: unreadable, or another box's",
            )),
        }
    }

    /// Preserves the immutable cleanup authority and concurrent accounting when a backend advances
    /// a record. A fresh native effect must first publish Creating; deletion never rolls back.
    pub(crate) fn put_lifecycle(&self, record: &BoxRecord) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        // Only scoped reference writes take this short existing Store configuration turn, before
        // the Box record turn. This short record mutation holds no native callback.
        let _source = self.claim_backing(record)?;
        let _turn = self.claim(&record.spec.id)?;
        let _backing_phase = self.check_box_backing(record)?;
        if record.version != hex_core::record::RECORD_VERSION {
            return Err(uncertain(
                &record.spec.id,
                "its record is of another version than this node writes",
            ));
        }
        record.spec.check()?;
        let Some(mut current) = self.read_lifecycle(&record.spec.id)? else {
            return if record.phase == Phase::Creating {
                self.write_fresh(record)
            } else {
                Err(uncertain(
                    &record.spec.id,
                    "it has no record for a Ready or Deleting one to be put over",
                ))
            };
        };
        if current.version != record.version
            || current.spec != record.spec
            || current.dirs != record.dirs
            || !matches!(
                (current.phase, record.phase),
                (Phase::Creating, _)
                    | (Phase::Ready, Phase::Ready | Phase::Deleting)
                    | (Phase::Deleting, Phase::Deleting)
            )
        {
            return Err(uncertain(
                &record.spec.id,
                "its record's directories or phase can't become the ones put",
            ));
        }
        if current.native_scope.is_none()
            && managed_scope_required(&current)
            && (current.init != record.init
                || (current.phase == Phase::Creating && record.phase == Phase::Ready))
        {
            return Err(uncertain(
                &record.spec.id,
                "its record names no native scope for its managed init to be put under",
            ));
        }
        // A VMM's pid/start is introduced while Creating, before its guest is Ready (the
        // Firecracker native create caller). Slot/run are likewise lifetime identities; their
        // later removal or substitution cannot authorize cleanup of a different native object.
        // Native process first-exec tracing uses Store::update and does not enter this VM seam.
        let introducing = current.phase == Phase::Creating && record.phase != Phase::Deleting;
        let init = lifecycle_fact(
            "init",
            current.init,
            record.init,
            introducing,
            &record.spec.id,
        )?;
        let slot = lifecycle_fact(
            "slot",
            current.slot,
            record.slot,
            introducing,
            &record.spec.id,
        )?;
        let run = lifecycle_fact("run", current.run, record.run, introducing, &record.spec.id)?;
        let exit = lifecycle_fact("exit", current.exit, record.exit, true, &record.spec.id)?;
        // A run's words where it failed (D249) and how its microVM started (D253) are facts as its exit is, a Shards
        // image VM's written through here (joint plan step 12).
        let ended = lifecycle_fact(
            "ended",
            current.ended.take(),
            record.ended.clone(),
            true,
            &record.spec.id,
        )?;
        let started_from = lifecycle_fact(
            "started_from",
            current.started_from,
            record.started_from,
            true,
            &record.spec.id,
        )?;
        // What its VM used in all where its run ended on its own (D260's final use), written with its end: a fact as its
        // exit is, kept for its delete with its host use.
        let final_use = lifecycle_fact(
            "final_use",
            current.final_use.take(),
            record.final_use.clone(),
            true,
            &record.spec.id,
        )?;
        // The file its run host's diagnostics are kept in (D258), named with its start's intent alone.
        let diagnostics = lifecycle_fact(
            "diagnostics",
            current.diagnostics.take(),
            record.diagnostics.clone(),
            introducing,
            &record.spec.id,
        )?;
        // Scope is introduced only by write_fresh before the first canonical publication. An old
        // legacy row can never gain authority over an already-alive native scope by an update.
        let native_scope = lifecycle_fact(
            "native_scope",
            current.native_scope,
            record.native_scope,
            false,
            &record.spec.id,
        )?;
        current.phase = record.phase;
        current.init = init;
        current.slot = slot;
        current.run = run;
        current.exit = exit;
        current.ended = ended;
        current.started_from = started_from;
        current.final_use = final_use;
        current.diagnostics = diagnostics;
        current.native_scope = native_scope;
        // Both writers count monotonically: publishing a backend completion cannot discard a
        // disconnect counted by the existing socket owner while native I/O was in flight.
        current.abandoned = current.abandoned.max(record.abandoned);
        self.write(&current)
    }

    /// The caller owns the shared native method/device receipts and has proved physical cleanup.
    /// A retained Ready or unreadable record cannot be dropped by a delayed remove.
    pub(crate) fn remove_lifecycle(&self, id: &BoxId) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.claim(id)?;
        if let Some(record) = self.read_lifecycle(id)?
            && record.phase != Phase::Deleting
        {
            return Err(uncertain(
                id,
                "its record isn't Deleting, so no delete has proved its cleanup",
            ));
        }
        self.unlink(id)
    }

    /// Changes `id`'s record by `change`, which says whether it changed anything, and writes it if it
    /// did, as one change among the box's; `false` when there's no readable record or nothing changed.
    pub fn update(
        &self,
        id: &BoxId,
        change: impl FnOnce(&mut BoxRecord) -> bool,
    ) -> Result<bool, Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.claim(id)?;
        let Some(Ok(mut record)) = self.get(id)? else {
            return Ok(false);
        };
        let source = record.spec.route.and_then(|route| route.backing);
        if !change(&mut record) {
            return Ok(false);
        }
        // Existing update callers change init, exit or abandoned accounting. They cannot introduce
        // or erase a backing reference while owning only a Box turn (source-before-Box ordering).
        if record.spec.route.and_then(|route| route.backing) != source {
            return Err(uncertain(
                id,
                "a change of its record would introduce or erase its backing under its turn alone",
            ));
        }
        self.write(&record)?;
        Ok(true)
    }

    fn write_fresh(&self, record: &BoxRecord) -> Result<(), Refusal> {
        if record.native_scope.is_some() {
            return Err(uncertain(
                &record.spec.id,
                "a fresh record names a native scope already",
            ));
        }
        if !managed_scope_required(record) {
            return self.write(record);
        }
        if record.phase != Phase::Creating {
            return Err(uncertain(
                &record.spec.id,
                "a fresh record of a box its managed scope holds isn't Creating",
            ));
        }
        let scope = fresh_scope()?;
        let started = self.next_start()?;
        self.write_encoded(
            record,
            record.encode_fresh_scope(scope, started, self.boot.as_ref())?,
            true,
        )
    }

    /// The node's start order as the store opens (joint plan step 14, C3): the next place is one past the most any
    /// record holds, a box's or a node VM's (step 39); then each VM record of this boot that holds none, begun by an
    /// earlier node agent, is given the next, in box-id order, so every VM that may run holds one before the node takes
    /// a create. A record of an earlier boot names nothing that runs and is recovery's to remove, so it is left as that
    /// boot wrote it, as is a torn one.
    fn count_starts(&self) -> Result<(), Refusal> {
        let records = self.snapshot()?;
        let most = records
            .values()
            .filter_map(|record| record.as_ref().as_ref().ok()?.started)
            .chain(self.all_node_vms()?.iter().filter_map(|row| row.started))
            .max()
            .unwrap_or(0);
        let next = most.checked_add(1).ok_or(Refusal::Exhausted {
            what: "a node's start order",
            bound: usize::MAX,
        })?;
        self.begun.store(next, Ordering::Relaxed);
        for (id, record) in records.iter() {
            let Ok(record) = record.as_ref() else {
                continue;
            };
            let unplaced = record.native_scope.is_some()
                && record.started.is_none()
                && record.boot == self.boot;
            if !unplaced {
                continue;
            }
            let place = self.next_start()?;
            let scope = record.native_scope;
            self.update(id, |current| {
                if current.native_scope != scope || current.started.is_some() {
                    return false;
                }
                current.started = Some(place);
                true
            })?;
        }
        Ok(())
    }

    /// The next place in the node's start order (C3), refused once the count would pass a `u64`.
    fn next_start(&self) -> Result<u64, Refusal> {
        self.begun
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Refusal::Exhausted {
                what: "a node's start order",
                bound: usize::MAX,
            })
    }

    /// Writes `record` in its place, carrying the host's boot session (T-T7.1.2).
    fn write(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.write_encoded(
            record,
            record.encode_in(self.boot.as_ref())?,
            record.native_scope.is_some(),
        )
    }

    /// Writes `bytes`, `record` encoded, as its box's record, and holds what its file then holds in the index, in the
    /// same turn: the bytes written once they are its file's, what a write cut short left in its place read back. A new
    /// box's first write takes a place under the box bound first, refused past it.
    fn write_encoded(
        &self,
        record: &BoxRecord,
        bytes: Vec<u8>,
        scoped: bool,
    ) -> Result<(), Refusal> {
        let id = &record.spec.id;
        let mut place = self.index.admit(id)?;
        let mut wrote = Wrote::Nothing;
        let written = self.write_file(record, &bytes, scoped, &mut wrote);
        match wrote {
            Wrote::Nothing => {}
            Wrote::Whole => self
                .index
                .publish(id, BoxRecord::decode(&bytes), &mut place)?,
            // A record's first write cut short in its place: what it left there is the record, as a restart reads it.
            #[cfg(target_os = "linux")]
            Wrote::Part => {
                if let Ok(left) = self.read_files(id) {
                    self.index.reset(id, left)?;
                }
            }
        }
        written
    }

    /// Writes `bytes` as `record`'s file, saying in `wrote` how far it got.
    fn write_file(
        &self,
        record: &BoxRecord,
        bytes: &[u8],
        scoped: bool,
        wrote: &mut Wrote,
    ) -> Result<(), Refusal> {
        let (dir, durable) = if scoped {
            (self.disk.as_path(), true)
        } else {
            self.place(record)
        };
        if scoped {
            let (temporary, final_path) = scoped_paths(dir, record.spec.id.as_str())?;
            return self.publish_durable(dir, &temporary, &final_path, bytes, || {
                *wrote = Wrote::Whole;
            });
        }
        #[cfg(target_os = "linux")]
        let dir_fd = if durable { &self.disk_fd } else { &self.run_fd };
        // A non-durable record goes to its shard of the runtime directory: a name under it on linux, where
        // the runtime directory's descriptor is held.
        let under = if durable {
            String::new()
        } else {
            format!("{}/", shard(&record.spec.id))
        };
        #[cfg(target_os = "linux")]
        let (tmp, final_path) = (
            format!("{under}.{}.tmp", record.spec.id),
            format!("{under}{}.json", record.spec.id),
        );
        #[cfg(not(target_os = "linux"))]
        let final_path = record_file(&dir.join(&under), &record.spec.id);
        #[cfg(not(target_os = "linux"))]
        let tmp = temp_file(&dir.join(&under), &record.spec.id);
        // A record that need not outlive the host (its box's directories all on tmpfs) is written once
        // per create (goal 5 extended step 5), so its first write goes straight to its file with O_EXCL:
        // no temp, no rename (criterion 3). A crash mid-write leaves a torn file that never decodes, which
        // recovery discards (T-T7.1.2) — the record is whole only once encode's trailing newline lands. A
        // later write of the same record (an update) finds it present (EEXIST) and falls through to the
        // atomic temp-and-rename path, as every durable record takes.
        #[cfg(target_os = "linux")]
        if !durable {
            match rustix::fs::openat(
                dir_fd.as_ref(),
                final_path.as_str(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::from_raw_mode(0o666),
            ) {
                Ok(fd) => {
                    let mut file = File::from(fd);
                    *wrote = Wrote::Part;
                    // T-T7.1.2's crash test kills the node here, after the record's file is open and
                    // before it is written, to leave a torn (empty) record recovery must discard (goal
                    // 5 extended step 5, criterion 3). Debug builds only, and only once the node's
                    // caller armed it ([`crate::Node::with_crash_point`]); release builds never test it.
                    #[cfg(debug_assertions)]
                    if self.crash_before_write.load(Ordering::Relaxed) {
                        let _ = rustix::process::kill_process(
                            rustix::process::getpid(),
                            rustix::process::Signal::KILL,
                        );
                    }
                    file.write_all(bytes)
                        .map_err(|e| Refusal::os("writing a record", e))?;
                    *wrote = Wrote::Whole;
                    return Ok(());
                }
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(Refusal::os("making a record file", e)),
            }
        }
        {
            #[cfg(target_os = "linux")]
            let mut f = File::from(
                rustix::fs::openat(
                    dir_fd.as_ref(),
                    tmp.as_str(),
                    rustix::fs::OFlags::WRONLY
                        | rustix::fs::OFlags::CREATE
                        | rustix::fs::OFlags::TRUNC
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::from_raw_mode(0o666),
                )
                .map_err(|e| Refusal::os("making a record temp file", e))?,
            );
            #[cfg(not(target_os = "linux"))]
            let mut f =
                File::create(&tmp).map_err(|e| Refusal::os("making a record temp file", e))?;
            f.write_all(bytes)
                .map_err(|e| Refusal::os("writing a record", e))?;
            if durable {
                f.sync_all()
                    .map_err(|e| Refusal::os("flushing a record", e))?;
                self.synced.files.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[cfg(target_os = "linux")]
        rustix::fs::renameat(
            dir_fd.as_ref(),
            tmp.as_str(),
            dir_fd.as_ref(),
            final_path.as_str(),
        )
        .map_err(|e| Refusal::os("renaming a record into place", e))?;
        #[cfg(not(target_os = "linux"))]
        fs::rename(&tmp, &final_path)
            .map_err(|e| Refusal::os("renaming a record into place", e))?;
        *wrote = Wrote::Whole;
        if durable {
            self.sync_dir(dir)?;
        }
        Ok(())
    }

    /// The record for `id`, if the store holds one, from its index: an unreadable file is returned as its reason, so
    /// recovery can clean the box up rather than crash (T-T7.1.2).
    pub fn get(&self, id: &BoxId) -> Result<Option<Result<BoxRecord, String>>, Refusal> {
        Ok(self.index.entry(id)?.map(|entry| Entry::clone(&entry)))
    }

    /// What box `id`'s files hold now: the runtime directory's, else the state directory's. Read back after a change
    /// of them cut short, as the next open reads them.
    fn read_files(&self, id: &BoxId) -> Result<Option<Entry>, Refusal> {
        for file in self.files(id) {
            match File::open(file) {
                Ok(file) => return Ok(Some(BoxRecord::decode(&self.read_bytes(file)?))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("opening a record", e)),
            }
        }
        Ok(None)
    }

    /// Removes `id`'s record from both places; absent already is removed. Not fsynced: the box's
    /// directories are gone before its record is, so a removal a host crash undoes leaves a record
    /// recovery scrubs again.
    pub fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.claim(id)?;
        self.unlink(id)
    }

    /// Removes box `id`'s files and lets its record go from the index; where a removal is cut short, the index holds
    /// what is left.
    fn unlink(&self, id: &BoxId) -> Result<(), Refusal> {
        let removed = self.remove_files(id);
        match &removed {
            Ok(()) => self.index.forget(id)?,
            Err(_) => {
                if let Ok(left) = self.read_files(id) {
                    self.index.reset(id, left)?;
                }
            }
        }
        removed
    }

    fn remove_files(&self, id: &BoxId) -> Result<(), Refusal> {
        for file in self.files(id) {
            match fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("removing a record", e)),
            }
        }
        Ok(())
    }

    /// Every record the store holds, by box id, a copy of its index's; an id whose file can't be read is included with
    /// its reason. A reader of every record that needs no copy of its own reads [`Store::snapshot`].
    pub fn all(&self) -> Result<BTreeMap<BoxId, Result<BoxRecord, String>>, Refusal> {
        let snapshot = self.snapshot()?;
        let mut all = BTreeMap::new();
        for (id, entry) in snapshot.iter() {
            all.insert(id.try_clone()?, Entry::clone(entry));
        }
        Ok(all)
    }

    /// Every record the store holds, by box id, shared: pointers into its index, never a copy (the review's per-window
    /// reads). A write made while it is held leaves it as it was.
    pub fn snapshot(&self) -> Result<Snapshot, Refusal> {
        self.index.snapshot()
    }

    /// Recovery must see every retained canonical name, including a conflicting duplicate in
    /// another Store directory. No overwritten/unknown `.json` row may disappear before cleanup.
    pub(crate) fn retained_records(
        &self,
    ) -> Result<BTreeMap<BoxId, Result<BoxRecord, String>>, Refusal> {
        self.scan_records(true)
    }

    /// Takes the torn records out of `records`, a [`Store::retained_records`]: each unreadable one the
    /// runtime place holds, a crash's (above): on Linux a first write cut short, elsewhere an unsynced write a
    /// host crash cut. Only an unscoped record is kept there; whose box that makes it, the caller says. Each
    /// comes with why it couldn't be read.
    pub(crate) fn take_torn(
        &self,
        records: &mut BTreeMap<BoxId, Result<BoxRecord, String>>,
    ) -> Result<Torn, Refusal> {
        let mut torn = Vec::new();
        torn.try_reserve_exact(records.len())
            .map_err(|_| Refusal::Exhausted {
                what: "torn record identities",
                bound: records.len(),
            })?;
        for (id, record) in records.iter() {
            let Err(why) = record else {
                continue;
            };
            match fs::symlink_metadata(runtime_record(&self.run, id)) {
                Ok(_) => torn.push((id.clone(), why.clone())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("finding a torn record's place", e)),
            }
        }
        for (id, _) in &torn {
            records.remove(id);
        }
        Ok(torn)
    }

    /// Where box `id`'s record is kept in the runtime place, where [`Store::take_torn`] finds a torn one.
    pub(crate) fn runtime_place(&self, id: &BoxId) -> PathBuf {
        runtime_record(&self.run, id)
    }

    /// Refuses the node's start over a record in the state directory that can't be read, naming its file and
    /// what to do (T-T7.1.2). Every write there is whole and synced, so no crash tore it: it is damage, and what
    /// its box holds can't be known from it. `records` is past [`Store::take_torn`], so every unreadable one
    /// left is the state directory's.
    pub(crate) fn refuse_unreadable(
        &self,
        records: &BTreeMap<BoxId, Result<BoxRecord, String>>,
    ) -> Result<(), Refusal> {
        let mut unreadable = records
            .iter()
            .filter_map(|(id, record)| record.as_ref().err().map(|why| (id, why)));
        let Some((id, why)) = unreadable.next() else {
            return Ok(());
        };
        let mut said = format!(
            "box {id}'s record {} can't be read ({why}). The state directory's records are written whole \
             and synced, so this is damage, not a crash, and what the box holds can't be known from it: \
             restore the file, or end what remains of box {id} and move the file aside; the node starts \
             once every record reads",
            record_file(&self.disk, id).display()
        );
        let more = unreadable.count();
        if more > 0 {
            said.push_str(&format!(" ({more} more can't be read)"));
        }
        Err(Refusal::Broken(said))
    }

    /// Whether `record` was written in an earlier boot of the host than this one, whose end ended every
    /// process of it (T-T7.1.2). Not where either session is unknown: an older node's record, or a host
    /// whose session can't be read.
    pub(crate) fn earlier_boot(&self, record: &BoxRecord) -> bool {
        self.before_this_boot(record.boot.as_ref())
    }

    /// Whether boot session `then` is an earlier one than this boot's: not where either is unknown.
    pub(crate) fn before_this_boot(&self, then: Option<&BootSession>) -> bool {
        matches!((&self.boot, then), (Some(now), Some(then)) if now != then)
    }

    /// Why the host's boot session couldn't be read, where it couldn't: records then carry none, and keep
    /// recovery's other rules.
    pub(crate) fn boot_unread(&self) -> Option<&str> {
        host_boot().as_ref().err().map(String::as_str)
    }

    /// The host's boot session this store stamps every record it writes with ([`host_boot`]); `None` where it can't
    /// be read. Every node's VM backends judge a record's boot by it: the Linux node's (`Node::boot_session`), and the
    /// guest's image class on every OS (`Recorded::bind_guest_books`).
    pub(crate) fn boot(&self) -> Option<&BootSession> {
        self.boot.as_ref()
    }

    /// The boot session the runtime place's records were written in before this node, as the marker says
    /// that its node keeps beside them: then this boot's, written durably before this node writes any record
    /// (T-T7.1.2). A torn record's own session can't be read, so a node over a place whose writes a host
    /// crash can tear (`crate::recorded`) takes this once, as it opens, to know one from an earlier boot.
    /// `None` with no marker, or an unreadable one, as an older node leaves. Where this boot's session is
    /// unknown the marker goes, so no later node takes a record of this boot for an earlier one's.
    pub(crate) fn take_boot_marker(&self) -> Result<Option<BootSession>, Refusal> {
        let path = self.disk.join(BOOT_MARKER);
        // Its session and a newline at most: read no further than one byte past it, whatever its file holds.
        let bound = BootSession::MAX_BYTES
            .checked_add(1)
            .ok_or(Refusal::Exhausted {
                what: "boot session marker bytes",
                bound: BootSession::MAX_BYTES,
            })?;
        let was = match File::open(&path) {
            Ok(file) => {
                let bytes = read_bounded(file, bound)?;
                if bytes.len() <= bound {
                    std::str::from_utf8(&bytes)
                        .ok()
                        .and_then(|text| BootSession::new(text).ok())
                } else {
                    None
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(Refusal::os("reading the boot session marker", e)),
        };
        match &self.boot {
            Some(now) if was.as_ref() == Some(now) => {}
            Some(now) => {
                let temporary = self.disk.join(BOOT_MARKER_TEMP);
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(now.as_str().len().saturating_add(1))
                    .map_err(|_| Refusal::Exhausted {
                        what: "boot session marker bytes",
                        bound: BootSession::MAX_BYTES,
                    })?;
                bytes.extend_from_slice(now.as_str().as_bytes());
                bytes.push(b'\n');
                {
                    let mut file = File::create(&temporary)
                        .map_err(|e| Refusal::os("making the boot session marker", e))?;
                    file.write_all(&bytes)
                        .and_then(|()| file.sync_all())
                        .map_err(|e| Refusal::os("writing the boot session marker", e))?;
                }
                fs::rename(&temporary, &path)
                    .map_err(|e| Refusal::os("publishing the boot session marker", e))?;
                sync_marker_dir(&self.disk)?;
            }
            None => match fs::remove_file(&path) {
                Ok(()) => sync_marker_dir(&self.disk)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Refusal::os("removing the boot session marker", e)),
            },
        }
        Ok(was)
    }

    fn scan_records(
        &self,
        retained: bool,
    ) -> Result<BTreeMap<BoxId, Result<BoxRecord, String>>, Refusal> {
        let mut out = BTreeMap::new();
        for dir in self.listed() {
            for (id, path) in records_in(&dir, retained)? {
                if retained && (out.len() >= TURNS || out.contains_key(&id)) {
                    return Err(uncertain(
                        &id,
                        "the node's retained records name it twice, or more boxes than the store's turns",
                    ));
                }
                let bytes = if retained {
                    let file = File::open(&path)
                        .map_err(|error| Refusal::os("opening a retained record", error))?;
                    Ok(self.read_bytes(file)?)
                } else {
                    File::open(&path)
                        .map_err(|error| Refusal::os("opening a record", error))
                        .and_then(|file| self.read_bytes(file))
                };
                let record = match bytes {
                    Ok(bytes) => BoxRecord::decode(&bytes),
                    Err(e) => Err(format!("reading the record: {e}")),
                };
                out.insert(id, record);
            }
        }
        Ok(out)
    }

    /// Removes the temp files a crash mid-write left in either place, each never renamed into place,
    /// under its box's turn, so a write under way keeps its own; how many it removed. Recovery's, so a
    /// crash at a record write leaves nothing behind (T-T7.1.2).
    pub fn remove_strays(&self) -> Result<usize, Refusal> {
        let _metadata = self.changing_metadata()?;
        let mut removed = 0usize;
        for dir in self.listed() {
            let entries = fs::read_dir(&dir).map_err(|e| Refusal::os("listing records", e))?;
            for entry in entries {
                let entry = entry.map_err(|e| Refusal::os("listing records", e))?;
                let name = entry.file_name();
                let Some(id) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix('.'))
                    .and_then(|n| n.strip_suffix(".tmp"))
                    .and_then(|n| BoxId::new(n).ok())
                else {
                    continue;
                };
                let _turn = self.claim(&id)?;
                match fs::remove_file(temp_file(&dir, &id)) {
                    Ok(()) => removed = removed.saturating_add(1),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Refusal::os("removing a record's stray temp file", e)),
                }
            }
        }
        Ok(removed)
    }

    /// fsync directory `dir`, so a rename in it is durable.
    fn sync_dir(&self, dir: &Path) -> Result<(), Refusal> {
        #[cfg(target_os = "linux")]
        {
            // Only durable records reach this call, and their directory is always disk.
            let _ = dir;
            self.disk_fd
                .sync_all()
                .map_err(|e| Refusal::os("flushing the record directory", e))?;
        }
        // Windows flushes a directory only through a handle opened as FlushFileBuffers asks
        // (`hex_runtime::windows::sys::flush_directory`); macOS's open for reading flushes it.
        #[cfg(windows)]
        hex_runtime::windows::sys::flush_directory(dir)
            .map_err(|e| Refusal::os("flushing the record directory", e))?;
        #[cfg(all(not(target_os = "linux"), not(windows)))]
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| Refusal::os("flushing the record directory", e))?;
        self.synced.dirs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Every record kind's one reader, for box records, node VM rows and pool records alike: a buffer the file's own length,
/// read through `take(bound + 1)`, so a read costs the record's bytes and an oversized file is read only as far as its
/// decoder needs to refuse it, past [`hex_core::record::MAX_RECORD_BYTES`] (the review's per-window reads). A torn or
/// oversized row remains a decoder refusal, never an absent owner.
fn read_record_bytes(file: File) -> Result<Vec<u8>, Refusal> {
    read_bounded(file, hex_core::record::MAX_RECORD_BYTES)
}

/// `file` read by the one bounded reader ([`read_record_bytes`]) to `bound` and one byte past it: a buffer its own length,
/// so a read past the bound says the file is over it, having read no more.
fn read_bounded(file: File, bound: usize) -> Result<Vec<u8>, Refusal> {
    use std::io::Read;
    let past = bound.checked_add(1).ok_or(Refusal::Exhausted {
        what: "record bytes",
        bound,
    })?;
    let length = file
        .metadata()
        .map_err(|error| Refusal::os("reading a record's length", error))?
        .len();
    // A length past what this host's `usize` holds is past the bound too.
    let room = usize::try_from(length).map_or(past, |length| length.min(past));
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| Refusal::Exhausted {
            what: "record bytes",
            bound: room,
        })?;
    let limit = u64::try_from(past).map_err(|_| Refusal::Exhausted {
        what: "record bytes",
        bound,
    })?;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| Refusal::os("reading a record", error))?;
    Ok(bytes)
}

fn retained_identity_error(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "retained identity refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "retained record identity",
        why,
    }
}

fn metadata_refusal(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return Refusal::Exhausted {
            what: "Store metadata refusal bytes",
            bound: message.len(),
        };
    }
    why.push_str(message);
    Refusal::Os {
        op: "entering the retained owner metadata fence",
        why,
    }
}

fn fresh_scope() -> Result<hex_core::record::NativeScope, Refusal> {
    let mut bytes = [0u8; 16];
    // One checked OS fill for BOTH canonical owner kinds; no PID/time/path fallback or retry loop.
    // Early-boot entropy may block. The node's external startup lifetime guard remains required.
    getrandom::fill(&mut bytes)
        .map_err(|error| Refusal::os("creating a fresh native scope identity", error))?;
    Ok(hex_core::record::NativeScope::from_bytes(bytes))
}

/// A stale absence preserves an observed fact; two different positive facts are uncertainty.
fn lifecycle_fact<T: PartialEq>(
    fact: &str,
    current: Option<T>,
    proposed: Option<T>,
    introducing: bool,
    id: &BoxId,
) -> Result<Option<T>, Refusal> {
    match (current, proposed) {
        (Some(known), Some(other)) if known != other => Err(uncertain(
            id,
            &format!("its record's {fact} differs from the one put, a fact once written"),
        )),
        (None, Some(_)) if !introducing => Err(uncertain(
            id,
            &format!(
                "a write would introduce its {fact}, which its record lacks, outside the write that introduces it"
            ),
        )),
        (Some(known), _) => Ok(Some(known)),
        (None, next) => Ok(next),
    }
}

/// Box `id`'s lifecycle uncertain, in the refusing site's own words (joint plan step 12).
fn uncertain(id: &BoxId, why: &str) -> Refusal {
    match id.try_clone() {
        Ok(id) => Refusal::Lifecycle {
            id,
            cause: hex_api::LifecycleCause::Uncertain,
            why: Some(why.to_owned()),
        },
        Err(error) => error,
    }
}

/// Retain a close-on-exec directory descriptor for relative record writes and durable renames.
#[cfg(target_os = "linux")]
fn open_directory(dir: &Path) -> Result<File, Refusal> {
    rustix::fs::open(
        dir,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|e| Refusal::os("opening a record directory", e))
}

/// Makes `dir` readable by its owner alone, by its mode where the platform has one (unix); elsewhere
/// it keeps the access its parent's gives it.
#[cfg(unix)]
fn owner_only(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn owner_only(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Box `id`'s shard of the runtime directory: two hex digits of FNV-1a over its id, the same in every run of
/// every build.
fn shard(id: &BoxId) -> String {
    format!("{:02x}", fnv1a(id.as_str().as_bytes()) & SHARD_MASK)
}

/// FNV-1a, 64-bit: its arithmetic is modulo 2^64 by definition, so the multiply wraps. A record's shard's hash,
/// and the node's key's on linux. FNV-1a because the hash must come out the same in every run of every build,
/// which std's hashers don't promise; the offset basis and prime are FNV-1a's own (IETF
/// draft-eastlake-fnv, "FNV Basics" and "FNV Constants").
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash: u64, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Where box `id`'s record is kept in the runtime record directory `run` ([`crate::records_dir`]): in its
/// shard. For tests and tools that look for a box's record from outside the store.
pub fn runtime_record(run: &Path, id: &BoxId) -> PathBuf {
    record_file(&run.join(shard(id)), id)
}

/// The temp file a write of box `id`'s record goes through in the runtime record directory `run`.
pub fn runtime_temp(run: &Path, id: &BoxId) -> PathBuf {
    temp_file(&run.join(shard(id)), id)
}

/// Box `id`'s record file in `dir`.
fn record_file(dir: &Path, id: &BoxId) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// The temp file a write of box `id`'s record in `dir` goes through.
fn temp_file(dir: &Path, id: &BoxId) -> PathBuf {
    dir.join(format!(".{id}.tmp"))
}

/// Canonical writers use `.json`. Other ASCII casing must refuse at startup even on Windows,
/// where a case-insensitive filesystem can resolve it to the same durable object.
fn retained_json_name(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.as_encoded_bytes().eq_ignore_ascii_case(b"json"))
}

/// The records in `dir`, each box's id with its file: every `<id>.json`, nothing else.
fn records_in(dir: &Path, retained: bool) -> Result<Vec<(BoxId, PathBuf)>, Refusal> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| Refusal::os("listing records", e))?;
    // Same bounded canonical/temporary/owner-metadata scan budget as the Pool directory. This
    // limits startup work; it grants no extra boxes or native ownership.
    let scan_bound = TURNS.checked_mul(3).ok_or(Refusal::Full(TURNS))?;
    let mut scanned = 0usize;
    for entry in entries {
        if retained {
            if scanned >= scan_bound {
                return Err(Refusal::Full(TURNS));
            }
            scanned = scanned.checked_add(1).ok_or(Refusal::Full(TURNS))?;
        }
        let entry = entry.map_err(|e| Refusal::os("listing records", e))?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| BoxId::new(n).ok())
        else {
            if retained && retained_json_name(&name) {
                return Err(retained_identity_error(
                    "a retained Box record has no valid canonical BoxId name",
                ));
            }
            continue;
        };
        if retained {
            if out.len() >= TURNS {
                return Err(Refusal::Full(TURNS));
            }
            out.try_reserve(1).map_err(|_| Refusal::Full(TURNS))?;
        }
        out.push((id, entry.path()));
    }
    Ok(out)
}

/// Whether a record must outlive the host, and so is fsynced: one naming a directory a host crash
/// leaves behind. A directory on tmpfs goes with the host, as the box's cgroup does, so a record whose
/// directories all lie on tmpfs (an `oci` box's container state, under the node's runtime directory)
/// describes nothing left to remove after a host crash; nor does one naming none (a `process` box's).
/// A rendered bundle's writable layer on disk makes its record durable.
fn must_outlive_host(record: &BoxRecord) -> bool {
    record.native_scope.is_some() || record.dirs.iter().any(|dir| !on_tmpfs(dir))
}

fn managed_scope_required(record: &BoxRecord) -> bool {
    matches!(
        record.spec.class.class(),
        hex_api::Class::Shards | hex_api::Class::ShardsImage | hex_api::Class::Firecracker
    ) || (record.spec.class.class() == hex_api::Class::Oci && record.spec.allocation.is_some())
}

/// tmpfs's magic number (include/uapi/linux/magic.h, `TMPFS_MAGIC`).
#[cfg(target_os = "linux")]
const TMPFS_MAGIC: i128 = 0x0102_1994;

/// Whether `dir` lies on tmpfs, judged by its filesystem ([`filesystem`]).
#[cfg(target_os = "linux")]
fn on_tmpfs(dir: &Path) -> bool {
    gone_with_the_host(filesystem(dir))
}

/// Whether a filesystem, by its magic number, goes with the host: tmpfs, which lives in memory, alone.
/// Every other (overlayfs, btrfs, ext4, xfs) outlives it, and so does one whose type can't be read, so
/// a record that has to be durable is never written without its fsync.
#[cfg(target_os = "linux")]
fn gone_with_the_host(filesystem: Option<i128>) -> bool {
    filesystem == Some(TMPFS_MAGIC)
}

/// The magic number of the filesystem `dir` lies on (statfs(2)'s `f_type`), or, when it isn't made yet,
/// of the nearest directory above it that is, where it will be made; `None` when that can't be read.
#[cfg(target_os = "linux")]
fn filesystem(dir: &Path) -> Option<i128> {
    // As many steps as the path has components.
    for at in dir.ancestors() {
        match rustix::fs::statfs(at) {
            Ok(fs) => return Some(i128::from(fs.f_type)),
            Err(rustix::io::Errno::NOENT) => {}
            Err(_) => return None,
        }
    }
    None
}

/// tmpfs is Linux's: elsewhere every directory is taken to outlive the host.
#[cfg(not(target_os = "linux"))]
fn on_tmpfs(_dir: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use hex_api::{Bounds, BoxSpec, ClassSpec};
    use hex_core::record::Phase;
    use hex_testkit::waits::follow::{self, Party};

    fn record(id: &str) -> BoxRecord {
        let spec = BoxSpec {
            route: None,
            devices: Vec::new(),
            id: BoxId::new(id).unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Process,
            owner: None,
            allocation: None,
            band: 0,
        };
        BoxRecord::new(spec, Phase::Ready, vec![])
    }

    /// A store over `<dir>/state` and `<dir>/run`, its two places.
    fn store_in(dir: &Path) -> Store {
        Store::open(&dir.join("state"), &dir.join("run")).unwrap()
    }

    /// AC-T7.4.1/T-T7.1.2: a failed create retains its physical identity; neither an equal retry
    /// nor a different payload at the same name may overwrite it before cleanup.
    #[test]
    fn lifecycle_begin_preserves_unfinished_identity_and_refuses_recycled_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut first = record("retained");
        first.phase = Phase::Creating;
        store.begin(&first).unwrap();
        let mut replacement = first.clone();
        replacement.spec.bounds.memory_max = Some(4096);
        for attempt in [&first, &replacement] {
            assert!(matches!(store.begin(attempt), Err(Refusal::Exists(_))));
            assert_eq!(
                store.get(&first.spec.id).unwrap(),
                Some(Ok(in_this_boot(first.clone())))
            );
        }
        store.remove(&first.spec.id).unwrap();
        store.begin(&replacement).unwrap();
        assert_eq!(
            store.get(&first.spec.id).unwrap(),
            Some(Ok(in_this_boot(replacement)))
        );
    }

    /// AC-T7.4.1/T-T7.1.2: a delayed native create cannot erase cancellation, and cannot publish a
    /// changed physical payload. Independent fields recorded while it ran survive publication.
    #[test]
    fn lifecycle_completion_keeps_deletion_intent_and_immutable_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut first = record("completing");
        first.phase = Phase::Creating;
        store.begin(&first).unwrap();
        let mut wrong = first.clone();
        wrong.spec.bounds.memory_max = Some(4096);
        assert!(store.made(&wrong).is_err());
        store
            .update(&first.spec.id, |r| {
                r.abandoned = 7;
                true
            })
            .unwrap();
        store.made(&first).unwrap();
        let ready = store.get(&first.spec.id).unwrap().unwrap().unwrap();
        assert_eq!(ready.phase, Phase::Ready);
        assert_eq!(ready.abandoned, 7);
        store
            .update(&first.spec.id, |r| {
                r.phase = Phase::Deleting;
                true
            })
            .unwrap();
        let deleting = store.get(&first.spec.id).unwrap();
        assert!(store.made(&first).is_err());
        assert_eq!(store.get(&first.spec.id).unwrap(), deleting);
    }

    /// The review's B-12 (AC-T7.4.1; T-T7.1.2): a lifecycle write refused over a fact its record holds already says
    /// which fact. Do record a box Ready with its exit, then put it with another exit. Expect it refused, naming its
    /// exit.
    #[test]
    fn b12_a_lifecycle_write_refused_over_a_fact_names_the_fact() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut kept = record("fact-box");
        kept.phase = Phase::Creating;
        store.put_lifecycle(&kept).unwrap();
        kept.phase = Phase::Ready;
        kept.exit = Some(hex_api::Exit::Code(0));
        store.put_lifecycle(&kept).unwrap();
        kept.exit = Some(hex_api::Exit::Code(1));
        match store.put_lifecycle(&kept) {
            Err(Refusal::Lifecycle { why: Some(why), .. }) => {
                assert!(why.contains("exit"), "{why}");
            }
            other => panic!("another exit put: {other:?}"),
        }
    }

    /// AC-T7.4.1/T-T7.1.2: both VM adapters update the sole Store with unchanged payload/cleanup
    /// authority. Metadata written by the socket while native I/O runs survives publication.
    #[test]
    fn lifecycle_transactions_preserve_authority_accounting_and_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut original = record("shared-backend");
        original.phase = Phase::Creating;
        store.put_lifecycle(&original).unwrap();
        store
            .update(&original.spec.id, |r| {
                r.abandoned = 11;
                true
            })
            .unwrap();
        let mut changed = original.clone();
        changed.spec.bounds.memory_max = Some(8192);
        assert!(store.put_lifecycle(&changed).is_err());
        changed = original.clone();
        changed.dirs.push(PathBuf::from("foreign-cleanup"));
        assert!(store.put_lifecycle(&changed).is_err());
        original.phase = Phase::Ready;
        original.init = Some(hex_core::record::ProcId {
            pid: 7,
            start_ticks: 9,
        });
        store.put_lifecycle(&original).unwrap();
        let ready = store.read_lifecycle(&original.spec.id).unwrap().unwrap();
        assert_eq!(ready.spec, original.spec);
        assert_eq!(ready.abandoned, 11);
        assert_eq!(ready.init, original.init);
        assert!(store.remove_lifecycle(&original.spec.id).is_err());
        original.phase = Phase::Deleting;
        store.put_lifecycle(&original).unwrap();
        let deleting = store.get(&original.spec.id).unwrap();
        original.phase = Phase::Ready;
        assert!(store.put_lifecycle(&original).is_err());
        assert_eq!(store.get(&original.spec.id).unwrap(), deleting);
        store.remove_lifecycle(&original.spec.id).unwrap();
        assert!(store.read_lifecycle(&original.spec.id).unwrap().is_none());
    }

    /// Joint plan step 12 (AC-T7.4.1): the VM adapters' lifecycle writes keep how a box's run ended, in its runtime's
    /// words (D249, `BoxRecord::ended`), and how its microVM started (D253, `BoxRecord::started_from`), as they keep its
    /// exit: a Shards image VM writes both through them. Do write a Ready record, then it with how its microVM started,
    /// then with its exit and its run's words, then as a writer that read it before either; then other words, and
    /// another start. Expect each fact kept as written, a stale writer's absence keeping it, and another value refused
    /// as uncertain, the record as it was.
    #[test]
    fn w12_lifecycle_writes_keep_a_runs_words_and_how_its_vm_started() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut stale = record("image-words");
        stale.phase = Phase::Creating;
        store.put_lifecycle(&stale).unwrap();
        stale.phase = Phase::Ready;
        store.put_lifecycle(&stale).unwrap();
        let started = BoxRecord {
            started_from: Some(hex_api::StartedFrom::Restored),
            ..stale.clone()
        };
        store.put_lifecycle(&started).unwrap();
        let words = "its VM failed: the guest's kernel panicked";
        let ended = BoxRecord {
            exit: Some(hex_api::Exit::Unknown),
            ended: Some(words.to_owned()),
            ..started.clone()
        };
        store.put_lifecycle(&ended).unwrap();
        let read = || store.read_lifecycle(&stale.spec.id).unwrap().unwrap();
        let kept = read();
        assert_eq!(
            (kept.started_from, kept.exit, kept.ended.as_deref()),
            (
                Some(hex_api::StartedFrom::Restored),
                Some(hex_api::Exit::Unknown),
                Some(words)
            )
        );
        store.put_lifecycle(&stale).unwrap();
        assert_eq!(read(), kept, "a stale writer's absence keeps both");
        let other_words = BoxRecord {
            ended: Some("other words".to_owned()),
            ..ended.clone()
        };
        assert!(store.put_lifecycle(&other_words).is_err());
        let other_start = BoxRecord {
            started_from: Some(hex_api::StartedFrom::Booted),
            ..ended.clone()
        };
        assert!(store.put_lifecycle(&other_start).is_err());
        assert_eq!(read(), kept);
    }

    /// Kiln's P7.1b (Shards' D260; AC-T7.4.1): a Shards image VM whose run ended on its own has what its VM used in all
    /// written with its end through the lifecycle writes, kept for its delete with its host use across the node's
    /// restarts, a fact as its exit is. Do write a Ready record, then it with its exit and its final use, then as a
    /// writer that read it before; then another use; then reopen the store's directory and read it. Expect the use kept
    /// as written, the stale writer's absence keeping it, another refused Uncertain naming it, and the reopened store
    /// reading it alike.
    #[test]
    fn p7_lifecycle_writes_keep_a_runs_final_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut stale = record("image-final-use");
        stale.phase = Phase::Creating;
        store.put_lifecycle(&stale).unwrap();
        stale.phase = Phase::Ready;
        store.put_lifecycle(&stale).unwrap();
        let used = hex_api::HostUse {
            cpu_ns: 9_000_000,
            footprint_bytes: 1 << 20,
            resident_bytes: 1 << 20,
            guest_ns: vec![1_000, 2_000],
            window: std::time::Duration::from_millis(250),
            charged: hex_api::Charged::ToThread,
        };
        let ended = BoxRecord {
            exit: Some(hex_api::Exit::Code(0)),
            final_use: Some(used.clone()),
            ..stale.clone()
        };
        store.put_lifecycle(&ended).unwrap();
        let read = || store.read_lifecycle(&stale.spec.id).unwrap().unwrap();
        assert_eq!(read().final_use.as_ref(), Some(&used));
        store.put_lifecycle(&stale).unwrap();
        assert_eq!(
            read().final_use.as_ref(),
            Some(&used),
            "a stale writer's absence keeps it"
        );
        let other = BoxRecord {
            final_use: Some(hex_api::HostUse {
                cpu_ns: 1,
                ..used.clone()
            }),
            ..ended.clone()
        };
        let refused = store.put_lifecycle(&other);
        assert!(
            matches!(&refused, Err(Refusal::Lifecycle { why: Some(why), .. }) if why.contains("final_use")),
            "{refused:?}"
        );
        assert_eq!(read().final_use.as_ref(), Some(&used));
        drop(store);
        let reopened = store_in(dir.path());
        assert_eq!(
            reopened
                .read_lifecycle(&stale.spec.id)
                .unwrap()
                .unwrap()
                .final_use,
            Some(used)
        );
    }

    /// Joint plan step 12 (ROOT, 2026-11-13: every Lifecycle Uncertain gets its reason; a bare one cost two diagnoses):
    /// a store write it can't take is refused Uncertain in the refusing site's own words. Do write a record, then another
    /// value of a fact it holds, then one of another record version. Expect each refused Uncertain, its words saying
    /// which: the fact once written, the version this node writes; its message ending in them.
    #[test]
    fn w12_a_store_write_it_cant_take_says_why_in_its_own_words() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut written = record("image-why");
        written.phase = Phase::Creating;
        store.put_lifecycle(&written).unwrap();
        written.phase = Phase::Ready;
        written.started_from = Some(hex_api::StartedFrom::Restored);
        store.put_lifecycle(&written).unwrap();
        let said = |refused: Result<(), Refusal>| match refused {
            Err(
                why @ Refusal::Lifecycle {
                    cause: hex_api::LifecycleCause::Uncertain,
                    why: Some(_),
                    ..
                },
            ) => why,
            other => panic!("not refused Uncertain in words: {other:?}"),
        };
        let another = said(store.put_lifecycle(&BoxRecord {
            started_from: Some(hex_api::StartedFrom::Booted),
            ..written.clone()
        }));
        assert!(
            another.to_string().ends_with(
                ": its record's started_from differs from the one put, a fact once written"
            ),
            "{another}"
        );
        let versioned = said(store.put_lifecycle(&BoxRecord {
            version: hex_core::record::RECORD_VERSION.wrapping_add(1),
            ..written.clone()
        }));
        assert!(
            versioned
                .to_string()
                .ends_with(": its record is of another version than this node writes"),
            "{versioned}"
        );
    }

    /// Joint plan step 12 (D258; AC-T7.4.1): a Shards image VM's record names the file its run host's diagnostics are
    /// kept in as its start's intent is written (its record's step from Creating to Ready), and the VM adapters' later
    /// writes keep it. Do write a Creating record, then it Ready naming its file; then as a writer that read it before,
    /// then naming another file; then another box's Ready record naming one after its start. Expect the file kept as
    /// named, a stale writer's absence keeping it, and another file refused as uncertain, as is one named past a start.
    #[test]
    fn w12_lifecycle_writes_keep_the_file_a_vms_start_names_for_its_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut created = record("image-diagnostics");
        created.phase = Phase::Creating;
        store.put_lifecycle(&created).unwrap();
        let file = dir
            .path()
            .join("image-diagnostics")
            .join("image-diagnostics.log");
        let started = BoxRecord {
            phase: Phase::Ready,
            diagnostics: Some(file.clone()),
            ..created.clone()
        };
        store.put_lifecycle(&started).unwrap();
        let named = |id: &str| {
            store
                .read_lifecycle(&BoxId::new(id).unwrap())
                .unwrap()
                .unwrap()
                .diagnostics
        };
        assert_eq!(named("image-diagnostics"), Some(file.clone()));
        let stale = BoxRecord {
            phase: Phase::Ready,
            ..created.clone()
        };
        store.put_lifecycle(&stale).unwrap();
        assert_eq!(
            named("image-diagnostics"),
            Some(file.clone()),
            "a stale writer's absence keeps it"
        );
        let other = BoxRecord {
            diagnostics: Some(file.with_file_name("other.log")),
            ..started.clone()
        };
        assert!(store.put_lifecycle(&other).is_err());
        let mut late = record("image-late");
        late.phase = Phase::Creating;
        store.put_lifecycle(&late).unwrap();
        late.phase = Phase::Ready;
        store.put_lifecycle(&late).unwrap();
        let named_late = BoxRecord {
            diagnostics: Some(file.with_file_name("image-late.log")),
            ..late.clone()
        };
        assert!(store.put_lifecycle(&named_late).is_err());
        assert_eq!(named("image-late"), None);
        assert_eq!(named("image-diagnostics"), Some(file));
    }

    /// The names of the records in the runtime record directory `run`'s shards, sorted.
    fn sharded(run: &Path) -> Vec<String> {
        let mut out: Vec<String> = (0..=SHARD_MASK)
            .flat_map(|n| names(&run.join(format!("{n:02x}"))))
            .collect();
        out.sort();
        out
    }

    /// The names of the files in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_record_is_put_got_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        assert_eq!(store.get(&BoxId::new("a").unwrap()).unwrap(), None);
        let r = record("a");
        store.put(&r).unwrap();
        assert_eq!(
            store.get(&r.spec.id).unwrap(),
            Some(Ok(in_this_boot(r.clone())))
        );
        store.remove(&r.spec.id).unwrap();
        assert_eq!(store.get(&r.spec.id).unwrap(), None);
    }

    /// A directory on tmpfs and one on disk this test can write in, judged as the store judges them,
    /// or why the host has no such pair.
    #[cfg(target_os = "linux")]
    fn tmpfs_and_disk() -> Result<(tempfile::TempDir, tempfile::TempDir), &'static str> {
        let tmpfs = ["/dev/shm", "/tmp"]
            .into_iter()
            .map(PathBuf::from)
            .chain(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
            .filter(|d| on_tmpfs(d))
            .find_map(|d| tempfile::tempdir_in(d).ok())
            .ok_or("no tmpfs this test can write in")?;
        // Beside this test's binary, else its crate's sources.
        let disk = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned))
            .into_iter()
            .chain([PathBuf::from(env!("CARGO_MANIFEST_DIR"))])
            .filter(|d| !on_tmpfs(d))
            .find_map(|d| tempfile::tempdir_in(d).ok())
            .ok_or("no disk this test can write in")?;
        Ok((tmpfs, disk))
    }

    /// T-T7.1.2 and decision 1 (goal 5 plan step 2; goal 5 extended step 5): a record whose
    /// directories all lie on tmpfs (an `oci` box's container state under the node's runtime
    /// directory), or that names none, is written without a sync, in the runtime directory's place;
    /// one with a directory on disk (a rendered bundle's writable layer) is written in the state
    /// directory, with a sync of the file and of that directory, so recovery after a host crash still
    /// finds what to remove. Directories not made yet are judged where they will be made.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_record_with_a_directory_on_disk_is_synced_and_kept_on_disk() {
        let (tmpfs, disk) = match tmpfs_and_disk() {
            Ok(pair) => pair,
            Err(why) => {
                eprintln!("SKIP: {why}");
                return;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let none = Syncs::default();

        store.put(&record("p")).unwrap();
        assert_eq!(store.syncs(), none, "a record naming no directory");

        let mut on_tmpfs_alone = record("t");
        on_tmpfs_alone.dirs = vec![tmpfs.path().join("t"), tmpfs.path().to_owned()];
        assert!(!must_outlive_host(&on_tmpfs_alone));
        store.put(&on_tmpfs_alone).unwrap();
        assert_eq!(store.syncs(), none, "a record on tmpfs alone is not synced");
        assert_eq!(
            store.get(&on_tmpfs_alone.spec.id).unwrap(),
            Some(Ok(in_this_boot(on_tmpfs_alone.clone()))),
            "it's written all the same"
        );

        let mut with_disk = record("d");
        with_disk.dirs = vec![tmpfs.path().join("d"), disk.path().join("d")];
        assert!(must_outlive_host(&with_disk));
        store.put(&with_disk).unwrap();
        assert_eq!(
            store.syncs(),
            Syncs { files: 1, dirs: 1 },
            "a record with a directory on disk is synced, file and directory"
        );
        let change = |r: &mut BoxRecord| {
            r.abandoned = r.abandoned.saturating_add(1);
            true
        };
        assert_eq!(store.update(&with_disk.spec.id, change), Ok(true));
        assert_eq!(
            store.syncs(),
            Syncs { files: 2, dirs: 2 },
            "and so is each change of it"
        );
        assert_eq!(store.update(&on_tmpfs_alone.spec.id, change), Ok(true));
        assert_eq!(store.syncs(), Syncs { files: 2, dirs: 2 });
        assert_eq!(
            (
                names(&dir.path().join("state")),
                sharded(&dir.path().join("run"))
            ),
            (
                vec!["@lock".to_owned(), "d.json".to_owned()],
                vec!["p.json".to_owned(), "t.json".to_owned()]
            ),
            "each record in its place: the durable one in the state directory, the rest beside the runtime state"
        );
    }

    /// T-T7.1.2 on every OS: do write, then update, a record naming a directory on disk (no native scope), which must
    /// outlive the host. Expect: each write flushed, its file and its directory, and the record kept in the state
    /// directory. Windows refused the directory's flush of a handle opened for reading alone: "Access is denied" (os
    /// error 5).
    #[test]
    fn a_record_with_a_directory_on_disk_is_written_durably_on_every_os() {
        let dir = tempfile::tempdir().unwrap();
        // A directory on disk, beside this test's binary, else its crate's sources: a host's `/tmp` may be tmpfs.
        let disk = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned))
            .into_iter()
            .chain([PathBuf::from(env!("CARGO_MANIFEST_DIR"))])
            .filter(|d| !on_tmpfs(d))
            .find_map(|d| tempfile::tempdir_in(d).ok())
            .unwrap();
        let store = store_in(dir.path());
        let mut kept = record("d");
        kept.dirs = vec![disk.path().join("d")];
        assert!(must_outlive_host(&kept));
        assert_eq!(store.put(&kept), Ok(()));
        assert_eq!(store.syncs(), Syncs { files: 1, dirs: 1 });
        let bump = |r: &mut BoxRecord| {
            r.abandoned = r.abandoned.saturating_add(1);
            true
        };
        assert_eq!(store.update(&kept.spec.id, bump), Ok(true));
        assert_eq!(store.syncs(), Syncs { files: 2, dirs: 2 });
        assert_eq!(
            names(&dir.path().join("state")),
            vec!["@lock".to_owned(), "d.json".to_owned()]
        );
    }

    /// Goal 5 plan step 2: writes of two boxes never wait on each other; two of one box do. While one
    /// box's turn is held, as its write holds it across the fsyncs, 31 other boxes' records are written
    /// from 31 threads at once, none of them waiting, as the store counts its waits; a second write of the held box
    /// waits, its record not written, until the turn is let go, and then is. Each wait follows the threads that write
    /// (the review's C-5), never a time: the store's own count says whether a write waited.
    #[test]
    fn writes_of_two_boxes_never_wait_on_each_other_and_of_one_box_do() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let held = record("held");
        let turn = store.claim(&held.spec.id).unwrap();
        std::thread::scope(|s| {
            // Hex's channel: std's receive comes late on macOS by the thread's QoS leeway (docs/design/deadline-waits.md).
            let (done, written) = hex_wait::mpsc::sync_channel(31).unwrap();
            let writers: Vec<_> = (0..31)
                .map(|n| {
                    let (store, done) = (&store, done.clone());
                    s.spawn(move || {
                        done.try_send(store.put(&record(&format!("b{n}")))).unwrap();
                    })
                })
                .collect();
            // Each writer holds a sender: once all have ended, the channel ends too.
            drop(done);
            let writes = Patience::measuring("test.store.other_boxes_writes");
            for _ in 0..31 {
                let put = follow::within(
                    &writes,
                    "another box's write answered",
                    || {
                        if writers.iter().all(|writer| writer.is_finished()) {
                            Party::Ended
                        } else {
                            Party::Live
                        }
                    },
                    |by| match by {
                        Some(at) => written
                            .recv_timeout(at.saturating_duration_since(std::time::Instant::now()))
                            .ok(),
                        None => written.recv().ok(),
                    },
                )
                .unwrap();
                put.unwrap();
            }
        });
        assert_eq!(store.all().unwrap().len(), 31);
        assert_eq!(store.waited(), 0, "no write of another box waited");
        drop(turn);

        let turn = store.claim(&held.spec.id).unwrap();
        std::thread::scope(|s| {
            let second = s.spawn(|| store.put(&held));
            // A look: the store tells no event as a write starts waiting, its count read.
            let waits = Patience::measuring("test.store.second_write_waits");
            follow::until(
                &waits,
                "the second write of the held box waiting",
                || {
                    if second.is_finished() {
                        Party::Ended
                    } else {
                        Party::Live
                    }
                },
                || store.waited() == 1,
            )
            .unwrap();
            assert!(
                !second.is_finished(),
                "a write waiting on its box's turn ends only once the turn is let go"
            );
            assert_eq!(
                store.get(&held.spec.id).unwrap(),
                None,
                "and writes nothing while the turn is held"
            );
            drop(turn);
            second.join().unwrap().unwrap();
        });
        assert_eq!(
            store.get(&held.spec.id).unwrap(),
            Some(Ok(in_this_boot(held)))
        );
    }

    /// CLAUDE.md §2 and §3: with [`TURNS`] boxes' records being changed at once, a change of one more
    /// is refused by type, writing nothing, and goes ahead once they're done.
    #[test]
    fn a_change_past_the_turns_bound_is_refused_by_type() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let ids: Vec<BoxId> = (0..TURNS)
            .map(|n| BoxId::new(&format!("b{n}")).unwrap())
            .collect();
        let turns: Vec<Turn<'_>> = ids.iter().map(|id| store.claim(id).unwrap()).collect();
        let past = record("past");
        assert_eq!(
            store.put(&past),
            Err(Refusal::Exhausted {
                what: "record changes under way",
                bound: TURNS
            })
        );
        assert_eq!(store.get(&past.spec.id).unwrap(), None, "nothing written");
        drop(turns);
        store.put(&past).unwrap();
    }

    /// Doc-truth (CLAUDE.md §6): the bound on record changes under way is the node's box budget, the one
    /// the cgroup root refuses past.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_turns_bound_is_the_box_budget() {
        assert_eq!(TURNS, hex_runtime::linux::cgroup::MAX_BOXES);
    }

    /// Decision 1's rule by filesystem (goal 5 plan step 2), on the magic numbers statfs(2) gives
    /// (include/uapi/linux/magic.h): tmpfs goes with the host; overlayfs (a container's root) and btrfs
    /// (fedora's /home), as every filesystem but tmpfs, outlive it, and so does one that can't be read.
    #[cfg(target_os = "linux")]
    #[test]
    fn tmpfs_alone_goes_with_the_host() {
        const OVERLAYFS_SUPER_MAGIC: i128 = 0x794c_7630;
        const BTRFS_SUPER_MAGIC: i128 = 0x9123_683e;
        assert!(gone_with_the_host(Some(TMPFS_MAGIC)));
        for outlives in [Some(OVERLAYFS_SUPER_MAGIC), Some(BTRFS_SUPER_MAGIC), None] {
            assert!(!gone_with_the_host(outlives), "{outlives:x?}");
        }
    }

    /// Goal 5 plan step 2: a directory not made yet is judged by the nearest one above it that is,
    /// where it will be made: three levels below a directory on tmpfs it lies on tmpfs, and below one on
    /// disk on that disk's filesystem, which the store judges to outlive the host.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_directory_is_judged_by_its_nearest_ancestor() {
        let (tmpfs, disk) = match tmpfs_and_disk() {
            Ok(pair) => pair,
            Err(why) => {
                eprintln!("SKIP: {why}");
                return;
            }
        };
        let deep = tmpfs.path().join("a/b/c");
        assert!(!deep.exists());
        assert_eq!(filesystem(&deep), Some(TMPFS_MAGIC));
        assert!(on_tmpfs(&deep));
        let on_disk = disk.path().join("x/y");
        assert!(!on_disk.exists());
        let disk_fs = filesystem(disk.path()).unwrap();
        assert_eq!(filesystem(&on_disk), Some(disk_fs));
        assert!(!on_tmpfs(&on_disk));
        // `/` is overlayfs in a container (the root lab's, seam_root.sh), btrfs or another disk's on a
        // host: a path under it that isn't made is judged by it.
        let root_fs = filesystem(Path::new("/")).unwrap();
        let under_root = Path::new("/hex-no-such-directory/x");
        assert!(!under_root.exists());
        assert_eq!(filesystem(under_root), Some(root_fs));
        assert_eq!(on_tmpfs(under_root), root_fs == TMPFS_MAGIC);
        eprintln!("the disk here is filesystem {disk_fs:#x}; the root, {root_fs:#x}");
    }

    /// HEX-SOCK-6: an update reads and writes a record as one change, so changes made from many
    /// threads at once all land; a record that isn't there, or a change that changes nothing, is no
    /// update.
    #[test]
    fn updates_from_many_threads_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let r = record("a");
        store.put(&r).unwrap();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..25 {
                        let counted = store.update(&r.spec.id, |r| {
                            r.abandoned += 1;
                            true
                        });
                        assert!(counted.unwrap());
                    }
                });
            }
        });
        let back = store.get(&r.spec.id).unwrap().unwrap().unwrap();
        assert_eq!(back.abandoned, 200);
        assert!(
            !store
                .update(&BoxId::new("none").unwrap(), |_| true)
                .unwrap()
        );
        assert!(!store.update(&r.spec.id, |_| false).unwrap());
    }

    /// T-T7.1.2: the temp files a crash mid-write leaves in either place are removed, and only they;
    /// a box's record and anything else in each directory stay.
    #[test]
    fn strays_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.put(&record("a")).unwrap();
        let (state, run) = (dir.path().join("state"), dir.path().join("run"));
        // One in a shard, one in the state directory.
        fs::write(runtime_temp(&run, &BoxId::new("a").unwrap()), b"{ half").unwrap();
        fs::write(state.join(".b.tmp"), b"").unwrap();
        fs::write(run.join(".notes"), b"hi").unwrap();
        fs::write(state.join(".notes"), b"hi").unwrap();
        assert_eq!(store.remove_strays().unwrap(), 2);
        assert_eq!(sharded(&run), ["a.json"]);
        assert!(
            names(&run).iter().all(|n| n == ".notes" || n.len() == 2),
            "the runtime directory holds its shards and the notes: {:?}",
            names(&run)
        );
        assert_eq!(names(&state), [".notes", "@lock"]);
        assert_eq!(store.remove_strays().unwrap(), 0);
    }

    /// A store's open reads every record in both places, a damaged one with why, and nothing else: do write two records,
    /// leave a stray temp file, an unrelated file and a damaged record beside them, then open the store again. Expect
    /// the three records listed, the damaged one with why.
    #[test]
    fn all_reads_every_record_in_both_places_and_ignores_strays() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store.put(&record("a")).unwrap();
        store.put(&record("b")).unwrap();
        drop(store);
        let state = dir.path().join("state");
        // A stray temp file (a crash mid-write) and an unrelated file are not records.
        fs::write(state.join(".c.tmp"), b"{}").unwrap();
        fs::write(state.join("notes.txt"), b"hi").unwrap();
        // A damaged record is reported, not skipped.
        fs::write(state.join("d.json"), b"{ not json").unwrap();
        let all = store_in(dir.path()).all().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[&BoxId::new("a").unwrap()].is_ok());
        assert!(all[&BoxId::new("d").unwrap()].is_err());
    }

    /// The review's per-window reads (AC-T7.4.1): do read a box record's file, then a file twice the record bound, through
    /// the store's record reader, which every node VM row, pool record and retained box record is read by. Expect: each
    /// read's one allocation, its buffer, the file's own length, no more: never the bound's MiB for a record of a few
    /// hundred bytes; the oversized file read to the bound and one byte, all its decoder needs to refuse it.
    #[test]
    fn a_records_read_allocates_its_files_length_never_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let bound = hex_core::record::MAX_RECORD_BYTES;
        let small = dir.path().join("small.json");
        let encoded = record("small").encode().unwrap();
        fs::write(&small, &encoded).unwrap();
        let read = read_record_bytes(File::open(&small).unwrap()).unwrap();
        assert_eq!(read, encoded);
        assert!(
            read.capacity() <= encoded.len(),
            "a {}-byte record's read allocated {} bytes",
            encoded.len(),
            read.capacity()
        );
        let large = dir.path().join("large.json");
        fs::write(&large, vec![b' '; bound * 2]).unwrap();
        let read = read_record_bytes(File::open(&large).unwrap()).unwrap();
        assert_eq!(read.len(), bound + 1);
        assert!(
            read.capacity() <= bound + 1,
            "a {}-byte file's read allocated {} bytes",
            bound * 2,
            read.capacity()
        );
    }

    /// The review's per-window reads (AC-T7.4.1): do write a file twice the record bound where a box's record goes, then
    /// open a store, get it, list every record, and list them as recovery does. Expect: each refuses it as damaged having
    /// read the bound and one byte, never the rest of the file.
    #[test]
    fn an_oversized_box_record_is_read_no_further_than_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let bound = hex_core::record::MAX_RECORD_BYTES;
        let id = BoxId::new("oversized").unwrap();
        fs::create_dir_all(dir.path().join("state")).unwrap();
        fs::write(
            record_file(&dir.path().join("state"), &id),
            vec![b' '; bound * 2],
        )
        .unwrap();
        let store = store_in(dir.path());
        let refused = format!("{} bytes, over {bound}", bound + 1);
        assert_eq!(store.get(&id).unwrap(), Some(Err(refused.clone())));
        assert_eq!(store.all().unwrap().get(&id), Some(&Err(refused.clone())));
        assert_eq!(
            store.retained_records().unwrap().get(&id),
            Some(&Err(refused))
        );
    }

    /// Goal 5 extended step 6: the runtime directory's records are spread over its shards by their box's id, each
    /// where [`runtime_record`] says, none in the directory itself, so the boxes made and deleted at once rarely
    /// take one directory's lock; got, listed and removed from there. Non-vacuity: 256 ids reach most shards,
    /// at least [`shards_256_ids_reach`] of them.
    #[test]
    fn records_are_spread_over_the_shards_by_their_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let run = dir.path().join("run");
        assert_eq!(
            names(&run).len(),
            SHARDS,
            "the shards are made as the store opens"
        );
        let ids: Vec<BoxId> = (0..256)
            .map(|n| BoxId::new(&format!("s{n:06}")).unwrap())
            .collect();
        for id in &ids {
            store.put(&record(id.as_str())).unwrap();
            assert!(runtime_record(&run, id).exists(), "{id} in its shard");
        }
        assert_eq!(names(&run).len(), SHARDS, "nothing in the directory itself");
        let used: BTreeSet<PathBuf> = ids
            .iter()
            .filter_map(|id| runtime_record(&run, id).parent().map(Path::to_owned))
            .collect();
        let least = shards_256_ids_reach();
        assert!(
            used.len() >= least,
            "256 ids took {} of the shards, fewer than {least}",
            used.len()
        );
        assert_eq!(store.all().unwrap().len(), 256);
        for id in &ids {
            assert!(store.get(id).unwrap().is_some_and(|r| r.is_ok()));
            store.remove(id).unwrap();
            assert!(!runtime_record(&run, id).exists());
        }
        assert!(sharded(&run).is_empty());
    }

    /// The fewest of the [`SHARDS`] shards 256 ids reach but once in 2^32 sets of ids, were the id hash
    /// uniform. Each shard is missed by all 256 with chance (63/64)^256, so the missed shards number μ =
    /// 64·(63/64)^256 ≈ 1.14 on average and the ids reach 64·(1 − (63/64)^256) ≈ 62.9. The misses are
    /// negatively associated (balls in bins: Dubhashi and Ranjan, "Balls and Bins: A Study in Negative
    /// Dependence", Random Structures & Algorithms 13(2), 1998, §2), so Chernoff's bound holds for their count:
    /// P(misses ≥ t) ≤ (e^δ / (1+δ)^(1+δ))^μ with t = (1+δ)μ. This is the least t that bound puts under 2^-32,
    /// taken from [`SHARDS`]. Derived, not picked: a hash that spreads ids worse than chance fails it.
    fn shards_256_ids_reach() -> usize {
        let shards = SHARDS as f64;
        let mu = shards * ((shards - 1.0) / shards).powi(256);
        let tail = |t: f64| {
            let delta = t / mu - 1.0;
            mu * (delta - (1.0 + delta) * (1.0 + delta).ln())
        };
        let misses = (1..SHARDS)
            .find(|&t| t as f64 > mu && tail(t as f64) <= -32.0 * std::f64::consts::LN_2)
            .unwrap();
        SHARDS - misses
    }

    /// Doc-truth for the bound above (CLAUDE.md §6): do derive it; expect the ids' average reach, about 62.9 of
    /// 64, and a least reach of 50: 14 shards missed is the first count Chernoff's bound puts under 2^-32.
    #[test]
    fn the_shards_256_ids_reach_follow_from_the_hash_being_uniform() {
        let shards = SHARDS as f64;
        let reach = shards * (1.0 - ((shards - 1.0) / shards).powi(256));
        assert!((reach - 62.86).abs() < 0.01, "{reach}");
        assert_eq!(shards_256_ids_reach(), 50);
    }

    /// T-T7.1.2/AC-T7.4.1: do keep a record torn at its first write in the runtime directory, one
    /// unreadable in the state directory, which no write tears, and a whole one; expect only the torn one
    /// taken, with why it couldn't be read, and the other two left for the whole-owner barrier, which refuses
    /// the unreadable one naming its file.
    #[test]
    fn only_a_record_torn_in_the_runtime_directory_is_taken_as_torn() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let whole = record("whole");
        store.put(&whole).unwrap();
        let torn = BoxId::new("torn").unwrap();
        let path = runtime_record(&dir.path().join("run"), &torn);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"version\":").unwrap();
        let unread = BoxId::new("unread").unwrap();
        fs::write(
            record_file(&dir.path().join("state"), &unread),
            b"{\"version\":",
        )
        .unwrap();
        let mut records = store.retained_records().unwrap();
        assert_eq!(records.len(), 3);
        let taken = store.take_torn(&mut records).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, torn);
        assert!(!taken[0].1.is_empty());
        assert!(!records.contains_key(&torn));
        assert!(records.get(&unread).is_some_and(Result::is_err));
        assert!(records.get(&whole.spec.id).is_some_and(Result::is_ok));
        let Err(Refusal::Broken(said)) = store.refuse_unreadable(&records) else {
            panic!("the unreadable state record was let through");
        };
        let file = record_file(&dir.path().join("state"), &unread);
        assert!(said.contains(&file.display().to_string()), "{said}");
    }

    /// T-T7.1.2: do read the host's boot session, write a record through the store, update it, and update
    /// one an earlier boot wrote. Expect: the session read on Linux, macOS and Windows, and each record carrying it.
    #[test]
    fn t_t7_1_2_every_record_the_store_writes_carries_this_boot() {
        let boot = host_boot().clone().unwrap();
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(boot.is_some(), "the host's boot session wasn't read");
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let carried = record("carried");
        store.put(&carried).unwrap();
        let read = |id: &BoxId| store.get(id).unwrap().unwrap().unwrap();
        assert_eq!(read(&carried.spec.id).boot, boot);
        let bump = |r: &mut BoxRecord| {
            r.abandoned += 1;
            true
        };
        assert!(store.update(&carried.spec.id, bump).unwrap());
        assert_eq!(read(&carried.spec.id).boot, boot);
        // One an earlier boot wrote, read by the store's next open, as a node's start reads it.
        drop(store);
        let mut earlier = record("earlier");
        earlier.boot = Some(BootSession::new("00000000-0000-0000-0000-000000000000").unwrap());
        let path = runtime_record(&dir.path().join("run"), &earlier.spec.id);
        fs::write(&path, earlier.encode().unwrap()).unwrap();
        let store = store_in(dir.path());
        let read = |id: &BoxId| store.get(id).unwrap().unwrap().unwrap();
        assert_eq!(read(&earlier.spec.id).boot, earlier.boot);
        assert_eq!(store.earlier_boot(&read(&earlier.spec.id)), boot.is_some());
        assert!(store.update(&earlier.spec.id, bump).unwrap());
        assert_eq!(read(&earlier.spec.id).boot, boot);
        assert!(!store.earlier_boot(&read(&earlier.spec.id)));
    }

    /// T-T7.1.2: do take the boot session marker with none there, again, and after a node of an earlier boot
    /// left one. Expect: none, then this boot's, then that earlier boot's, and the marker naming this boot after
    /// each.
    #[test]
    fn t_t7_1_2_the_boot_marker_names_the_last_boot_and_takes_this_one() {
        let Ok(Some(now)) = host_boot().clone() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let marker = dir.path().join("state").join(BOOT_MARKER);
        let this_boot = format!("{}\n", now.as_str());
        assert_eq!(store.take_boot_marker().unwrap(), None);
        assert_eq!(fs::read_to_string(&marker).unwrap(), this_boot);
        assert_eq!(store.take_boot_marker().unwrap(), Some(now.clone()));
        fs::write(&marker, "00000000-0000-0000-0000-000000000000\n").unwrap();
        let earlier = store.take_boot_marker().unwrap().unwrap();
        assert!(store.before_this_boot(Some(&earlier)));
        assert!(!store.before_this_boot(Some(&now)));
        assert!(!store.before_this_boot(None));
        assert_eq!(fs::read_to_string(&marker).unwrap(), this_boot);
        assert!(!dir.path().join("state").join(BOOT_MARKER_TEMP).exists());
    }

    /// T-T7.1.2 on every OS: a node of an earlier boot left the boot session marker. Do: take it. Expect: that
    /// boot's session, and the marker's change made with its directory flushed: naming this boot where the host's
    /// session is read, gone where it isn't (Windows'), so no later node takes a record of this boot for an earlier
    /// one's. Windows refused the flush of the directory opened for reading alone: "Access is denied" (os error 5).
    #[test]
    fn t_t7_1_2_an_earlier_boots_marker_is_taken_and_its_change_flushed_on_every_os() {
        let earlier = "00000000-0000-0000-0000-000000000000";
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let marker = dir.path().join("state").join(BOOT_MARKER);
        fs::write(&marker, format!("{earlier}\n")).unwrap();
        assert_eq!(
            store.take_boot_marker(),
            Ok(Some(BootSession::new(earlier).unwrap()))
        );
        match store.boot() {
            Some(now) => assert_eq!(
                fs::read_to_string(&marker).unwrap(),
                format!("{}\n", now.as_str())
            ),
            None => assert!(!marker.exists(), "the marker outlived its taking"),
        }
        assert!(!dir.path().join("state").join(BOOT_MARKER_TEMP).exists());
    }

    /// A Shards VM's record as its create begins it: the store introduces its lifetime's scope.
    fn vm_record(id: &str) -> BoxRecord {
        let mut record = record(id);
        record.spec.class = ClassSpec::Shards(Box::new(hex_api::OciSpec {
            rootfs: hex_api::Rootfs::Dir(PathBuf::from("/images/agent")),
            init: hex_api::Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        }));
        record.phase = Phase::Creating;
        record
    }

    /// Joint plan step 14 (AC-T7.4.1), C3's order (ROOT, 2026-11-12): the store gives each VM lifetime it begins its
    /// place in the node's start order, kept in its record across the node's restarts and never its scope's; a VM of this
    /// boot begun by an earlier node agent, its record holding no place, is given one as the store opens, in box-id order;
    /// a process box's record holds none. Do begin vm-b then vm-a, reopen the store as a restarted node does, and begin
    /// vm-c and a process box; then take the three VMs' places out of their records, as an earlier node agent wrote them,
    /// reopen, and begin vm-d. Expect vm-b 1, vm-a 2 and vm-c 3, past the most on disk after the restart, and the process
    /// box none; after the second reopening vm-a 1, vm-b 2 and vm-c 3, by box id, and vm-d 4.
    #[test]
    fn w14_the_store_keeps_each_vms_place_in_its_start_order() {
        let dir = tempfile::tempdir().unwrap();
        let started = |store: &Store, id: &str| {
            store
                .get(&BoxId::new(id).unwrap())
                .unwrap()
                .unwrap()
                .unwrap()
                .started
        };
        let places = |store: &Store| ["vm-a", "vm-b", "vm-c"].map(|id| started(store, id));
        let store = store_in(dir.path());
        for id in ["vm-b", "vm-a"] {
            store.begin(&vm_record(id)).unwrap();
        }
        drop(store);
        let store = store_in(dir.path());
        store.begin(&vm_record("vm-c")).unwrap();
        let mut process = record("process");
        process.phase = Phase::Creating;
        store.begin(&process).unwrap();
        assert_eq!(places(&store), [Some(2), Some(1), Some(3)]);
        assert_eq!(started(&store, "process"), None);
        for id in ["vm-a", "vm-b", "vm-c"] {
            store
                .update(&BoxId::new(id).unwrap(), |r| {
                    r.started = None;
                    true
                })
                .unwrap();
        }
        drop(store);
        let store = store_in(dir.path());
        assert_eq!(places(&store), [Some(1), Some(2), Some(3)]);
        store.begin(&vm_record("vm-d")).unwrap();
        assert_eq!(started(&store, "vm-d"), Some(4));
    }

    /// One line of [`store_open_role`]'s: what a store's open over its directory said, in another process.
    const ROLE_SAID: &str = "HEX_STORE_ROLE ";

    /// Opens a store over `dir` in another process, this test binary run as [`store_open_role`], and says what its open
    /// answered: `opened`, or `refused: ` and why.
    fn store_opened_elsewhere(dir: &Path) -> String {
        let full = concat!(module_path!(), "::", "store_open_role");
        let test = full.split_once("::").map_or(full, |(_, test)| test);
        let ran = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env("HEX_STORE_ROLE_DIR", dir)
            .output()
            .unwrap();
        assert!(ran.status.success(), "{ran:?}");
        String::from_utf8(ran.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.split_once(ROLE_SAID).map(|(_, said)| said.to_owned()))
            .unwrap_or_default()
    }

    /// [`store_opened_elsewhere`]'s other process, run alone where `HEX_STORE_ROLE_DIR` names a directory (an ordinary
    /// run passes at once): it opens a store over that directory, says what the open answered, and ends.
    #[test]
    fn store_open_role() {
        let Some(dir) = std::env::var_os("HEX_STORE_ROLE_DIR") else {
            return;
        };
        let said = match Store::open(&Path::new(&dir).join("state"), &Path::new(&dir).join("run")) {
            Ok(_) => "opened".to_owned(),
            Err(why) => format!("refused: {why}"),
        };
        println!("{ROLE_SAID}{said}");
    }

    /// The review's per-window reads, step 2 (batch 1114d's reads2; AC-T7.4.1): a store is its directory's one writer,
    /// so the records it holds in memory are its directory's. Do open a store over a directory; open another over the
    /// same one in this process, and in another process; then let the first go and open one in each again. Expect each
    /// open refused while the first is open, naming the directory, and each to open once the first is let go.
    #[test]
    fn reads2_a_second_store_over_one_directory_is_refused_until_the_first_goes() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let first = store_in(dir.path());
        let again = Store::open(&state, &dir.path().join("run"));
        assert!(
            matches!(&again, Err(Refusal::Broken(why)) if why.contains(&state.display().to_string())),
            "a second store in this process: {again:?}"
        );
        let elsewhere = store_opened_elsewhere(dir.path());
        assert!(
            elsewhere.starts_with("refused: ") && elsewhere.contains(&state.display().to_string()),
            "a second store in another process: {elsewhere}"
        );
        drop(again);
        drop(first);
        assert_eq!(store_opened_elsewhere(dir.path()), "opened");
        drop(store_in(dir.path()));
    }

    /// The review's side finding on the boot session marker (batch 1114d's reads2; AC-T7.4.1): the marker is read no
    /// further than its bound, whatever its file holds. Do make the marker a pipe whose writer offers 1 MiB, then take it
    /// as a node's start does. Expect it taken as no marker, and its writer cut off before the reader took all it offered.
    #[cfg(unix)]
    #[test]
    fn reads2_the_boot_marker_is_read_no_further_than_its_bound() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let marker = dir.path().join("state").join(BOOT_MARKER);
        let made = std::process::Command::new("mkfifo")
            .arg(&marker)
            .status()
            .unwrap();
        assert!(made.success(), "{made}");
        let offered = 1 << 20;
        let writer = {
            let marker = marker.clone();
            std::thread::spawn(move || {
                let mut pipe = fs::OpenOptions::new().write(true).open(&marker).unwrap();
                let chunk = [b'0'; 4096];
                let mut written = 0usize;
                while written < offered {
                    match pipe.write(&chunk) {
                        Ok(n) => written += n,
                        Err(_) => break,
                    }
                }
                written
            })
        };
        assert_eq!(store.take_boot_marker().unwrap(), None);
        let written = writer.join().unwrap();
        assert!(
            written < offered,
            "the marker's read took all {written} bytes its writer offered"
        );
    }

    /// The review's per-window reads, step 2 (batch 1114d's reads2; ROOT, 2026-11-14: a snapshot copies pointers only;
    /// AC-T7.4.1): a write made while a reader holds every record leaves the reader's as it was and copies no record. Do
    /// write 100 records; take every record, as a window does; write one more and change another; take every record
    /// again. Expect the first taking as it was; the second holding the new record and the change; every other record the
    /// same one in memory in both, neither copied nor decoded again; and no record file read.
    #[test]
    fn reads2_a_write_while_a_snapshot_is_held_copies_pointers_never_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        for n in 0..100 {
            store.put(&record(&format!("b{n}"))).unwrap();
        }
        let before = store.snapshot().unwrap();
        let read = store.reads();
        store.put(&record("new")).unwrap();
        let changed = BoxId::new("b7").unwrap();
        assert!(
            store
                .update(&changed, |record| {
                    record.abandoned = 1;
                    true
                })
                .unwrap()
        );
        let after = store.snapshot().unwrap();
        assert_eq!((before.len(), after.len()), (100, 101));
        assert!(!before.contains_key(&BoxId::new("new").unwrap()));
        for (id, held) in before.iter() {
            let now = &after[&**id];
            if **id == changed {
                assert_eq!(held.as_ref().as_ref().unwrap().abandoned, 0);
                assert_eq!(now.as_ref().as_ref().unwrap().abandoned, 1);
            } else {
                assert!(Arc::ptr_eq(held, now), "box {id}'s record copied");
            }
        }
        assert_eq!(store.reads(), read, "a record file read");
    }
}
