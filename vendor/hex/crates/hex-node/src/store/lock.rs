//! A store's hold of its directory (the review's per-window reads, step 2; ROOT, 2026-11-14): a store is the one writer
//! of its state directory, so the records it holds in memory ([`super::Index`]) are the directory's. It takes the hold
//! as it opens, before it reads a record, and keeps it while any clone of it lives; a second store over the directory,
//! in this process or another, is refused at its open, naming the directory.
//!
//! The hold is a lock on one file in the directory, `@lock`, taken without waiting, on the one handle of it this process
//! opens:
//! - on Unix a POSIX record lock, fcntl(2)'s `F_SETLK` (rustix's `fcntl_lock`), as the node VM rows' locks are: "Record
//!   locks are not inherited by a child created via fork(2)" (fcntl(2)), so no child of the node keeps it, and the
//!   kernel lets it go as its process ends. It goes too as its process closes any handle of its file, and a second
//!   handle's lock in the same process is no conflict, so the process opens the file once: the directories its stores
//!   hold, by device and inode, are kept here, and a second store over one is refused before the file is opened again;
//! - on Windows LockFileEx (std's `File::try_lock`), on a handle no child inherits (std opens none inheritable): a lock
//!   on any other handle, in this process or another, conflicts, and the lock goes as its handle closes or its process
//!   ends.

use std::fs::{File, OpenOptions};
use std::path::Path;

use hex_api::Refusal;

/// The lock file's name in the state directory: `@` is in no box id, so it is never read as a box's record.
const LOCK: &str = "@lock";

/// A store's hold of its directory, let go as the last clone of its store drops.
#[derive(Debug)]
pub(super) struct Hold {
    /// The one handle of the lock file this process holds; `None` once let go.
    file: std::sync::Mutex<Option<File>>,
    /// The directory's device and inode, in this process's table of the directories its stores hold.
    #[cfg(unix)]
    key: (u64, u64),
    /// Let go as its process's end lets it go though its store's clones live on ([`Hold::end`], a test's crash in this
    /// process): its store writes nothing after.
    #[cfg(all(debug_assertions, feature = "test-controls"))]
    ended: std::sync::atomic::AtomicBool,
}

/// The directories this process's stores hold, by device and inode: a POSIX record lock is the process's, so a second
/// store's lock over one would be granted, and its handle's close would let the first's go.
#[cfg(unix)]
static HELD: std::sync::Mutex<std::collections::BTreeSet<(u64, u64)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A second store over `disk`, `holder` naming where the first is, refused: one store writes a directory.
fn held_by(disk: &Path, holder: &str) -> Refusal {
    Refusal::Broken(format!(
        "the record store of {} is held by another store {holder}: one store writes a state directory, its records \
         in memory being the directory's",
        disk.display()
    ))
}

impl Hold {
    /// Takes the hold of state directory `disk`, refused where another store holds it.
    pub(super) fn take(disk: &Path) -> Result<Self, Refusal> {
        #[cfg(unix)]
        let key = {
            use std::os::unix::fs::MetadataExt;
            let directory = std::fs::metadata(disk)
                .map_err(|e| Refusal::os("reading the record directory's identity", e))?;
            (directory.dev(), directory.ino())
        };
        // Held across the open and the lock, so two stores opened at once in this process are one after the other.
        #[cfg(unix)]
        let mut held = HELD.lock().map_err(|_| {
            Refusal::Broken("the held record directories' lock is poisoned".to_owned())
        })?;
        #[cfg(unix)]
        if held.contains(&key) {
            return Err(held_by(disk, "in this process"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(disk.join(LOCK))
            .map_err(|e| Refusal::os("opening the record directory's lock file", e))?;
        if !try_lock(&file)? {
            return Err(held_by(disk, "in another process"));
        }
        #[cfg(unix)]
        held.insert(key);
        Ok(Self {
            file: std::sync::Mutex::new(Some(file)),
            #[cfg(unix)]
            key,
            #[cfg(all(debug_assertions, feature = "test-controls"))]
            ended: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Lets the hold go as its process's end would, though its store's clones live on (T-T7.1.2: a test's crash in this
    /// process, `Node::crash`): a store opened after over the directory takes it, and no write of this store's is taken
    /// after ([`Hold::ended`]), as a process that ended writes nothing. A test control of debug builds alone, on Linux,
    /// where `Node` is.
    #[cfg(all(debug_assertions, feature = "test-controls", target_os = "linux"))]
    pub(super) fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        self.let_go();
    }

    /// Whether [`Hold::end`] let it go.
    #[cfg(all(debug_assertions, feature = "test-controls"))]
    pub(super) fn ended(&self) -> bool {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The handle first: the directory leaves this process's table only once no handle of its lock file is open here,
    /// so a store opened after never shares one about to close.
    fn let_go(&self) {
        drop(
            self.file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        #[cfg(unix)]
        HELD.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        // A hold a test's crash let go ([`Hold::end`]) may be another store's now, its table entry with it.
        #[cfg(all(debug_assertions, feature = "test-controls"))]
        if self.ended() {
            return;
        }
        self.let_go();
    }
}

/// Takes `file`'s lock where no other holds it: whether it did. POSIX lets a lock another process holds answer
/// `F_SETLK` with either EACCES or EAGAIN (fcntl(2)).
#[cfg(unix)]
fn try_lock(file: &File) -> Result<bool, Refusal> {
    match rustix::fs::fcntl_lock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::ACCESS | rustix::io::Errno::AGAIN) => Ok(false),
        Err(e) => Err(Refusal::os("locking the record directory", e)),
    }
}

#[cfg(windows)]
fn try_lock(file: &File) -> Result<bool, Refusal> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(Refusal::os("locking the record directory", e)),
    }
}
