//! What changes in folders, as the OS tells it (goal 9: a loop wakes on these, never on a clock):
//! inotify on Linux, FSEvents on macOS, `FindFirstChangeNotificationW` on Windows; nothing
//! elsewhere, where the loops' other events stand alone.
//!
//! A [`Watched`] is one set of folders a loop waits on: a runner's config file's folder, the
//! folders its tools and harnesses are installed in, its warm workspaces' records. A change wakes
//! its waiter once however many came since it last looked, so a burst (an install writing a
//! hundred files) is one wake, and what the waiter does with it (read again, compare, say what
//! changed) is its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// One set of folders watched, and the wake its changes leave.
pub struct Watched {
    changes: crate::rt::Notify,
    wakes: AtomicU64,
    dirs: Vec<PathBuf>,
    recursive: bool,
    names: Vec<String>,
}

impl Watched {
    /// Watches each folder of `dirs` (and every folder below it when `recursive`), keeping only
    /// changes to entries named in `names` when it isn't empty (where the OS says names; elsewhere
    /// every change counts). A folder that can't be watched (it doesn't exist) is left out, logged.
    pub fn new(dirs: &[PathBuf], recursive: bool, names: &[&str]) -> Arc<Watched> {
        let w = Arc::new(Watched { changes: crate::rt::Notify::new(), wakes: AtomicU64::new(0), dirs: dirs.to_vec(), recursive, names: names.iter().map(|n| (*n).to_owned()).collect() });
        backend::start(&w);
        w
    }

    /// A change since the last one taken, or the next: a burst counts once.
    pub async fn changed(&self) {
        self.changes.notified().await;
    }

    /// The changes it was told so far (tests count an idle loop's wakes by it).
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::Relaxed)
    }

    /// The folders it watches.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Whether it watches below its folders.
    pub fn recursive(&self) -> bool {
        self.recursive
    }

    /// Whether a change to the entry named `name` is one it keeps.
    pub fn keeps(&self, name: &Path) -> bool {
        self.names.is_empty() || name.file_name().is_some_and(|n| self.names.iter().any(|k| n == k.as_str()))
    }

    /// A change came (a backend's, or a test's).
    pub fn tell(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.changes.notify();
    }
}

mod backend {
    //! No backend yet: nothing is told but what [`super::Watched::tell`] is called for.
    pub(super) fn start(_w: &std::sync::Arc<super::Watched>) {}
}
