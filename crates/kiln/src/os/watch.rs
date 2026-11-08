//! What changes in folders, as the OS tells it (goal 9: a loop wakes on these, never on a clock):
//! inotify on Linux, FSEvents on macOS, `FindFirstChangeNotificationW` on Windows; nothing
//! elsewhere, where the loops' other events stand alone.
//!
//! A [`Watched`] is one set of folders a loop waits on: a runner's config file's folder, the
//! folders its tools and harnesses are installed in, its warm workspaces' records. A change wakes
//! its waiter once however many came since it last looked, so a burst (an install writing a
//! hundred files) is one wake, and what the waiter does with it (read again, compare, say what
//! changed) is its own.
//!
//! - **Below a folder.** A watch reaches `depth` folders below each it's given (none: only the
//!   folder's own entries; unlimited: everything below), folders made later included, so a
//!   watch on `<work>/warm` two deep sees each lane's records without a watch on every folder of
//!   every checkout in it.
//! - **A folder not there yet** is watched from its nearest folder that is, for the one on the
//!   way to it: `~/.local/bin` made by an installer is a change, and watched from then on.
//! - **Names.** A watch given names keeps only changes to entries so named (`*.json`: any name
//!   ending `.json`) where the OS says names (Linux, macOS); on Windows, which doesn't, every
//!   change under the folders counts, and the waiter's reading again finds nothing new.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How far below each of its folders a watch reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// This many folders below (0: the folder's own entries alone).
    Below(u32),
    /// Every folder below, however deep.
    All,
}

impl Depth {
    /// Whether a folder `n` below a watched one is watched.
    pub fn reaches(self, n: u32) -> bool {
        match self {
            Depth::Below(d) => n <= d,
            Depth::All => true,
        }
    }
}

/// One set of folders watched, and the wake its changes leave.
pub struct Watched {
    changes: crate::rt::Notify,
    wakes: AtomicU64,
    dirs: Vec<PathBuf>,
    depth: Depth,
    names: Vec<String>,
    /// Its registration with the OS's watcher, gone with it.
    id: u64,
}

impl Watched {
    /// Watches each folder of `dirs` (and every folder below it when `recursive`), keeping only
    /// changes to entries named in `names` when it isn't empty (where the OS says names; elsewhere
    /// every change counts). A folder that isn't there yet is watched from its nearest one that is.
    pub fn new(dirs: &[PathBuf], recursive: bool, names: &[&str]) -> Arc<Watched> {
        Self::reaching(dirs, if recursive { Depth::All } else { Depth::Below(0) }, names)
    }

    /// [`Watched::new`], reaching `depth` folders below each of `dirs`.
    pub fn reaching(dirs: &[PathBuf], depth: Depth, names: &[&str]) -> Arc<Watched> {
        static IDS: AtomicU64 = AtomicU64::new(1);
        let mut kept: Vec<PathBuf> = Vec::new();
        for d in dirs.iter().filter(|d| d.is_absolute()).map(|d| normal(d.as_path())) {
            if !kept.contains(&d) {
                kept.push(d);
            }
        }
        let w = Arc::new(Watched { changes: crate::rt::Notify::new(), wakes: AtomicU64::new(0), dirs: kept, depth, names: names.iter().map(|n| (*n).to_owned()).collect(), id: IDS.fetch_add(1, Ordering::Relaxed) });
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
        self.depth != Depth::Below(0)
    }

    /// How far below its folders it watches.
    pub fn depth(&self) -> Depth {
        self.depth
    }

    /// Whether a change to the entry named `name` is one it keeps.
    pub fn keeps(&self, name: &Path) -> bool {
        self.names.is_empty() || name.file_name().is_some_and(|n| self.names.iter().any(|k| named(&n.to_string_lossy(), k)))
    }

    /// Whether a change at `path` is one it keeps: under one of its folders, as deep as it
    /// reaches, and named as it asks.
    pub fn covers(&self, path: &Path) -> bool {
        self.covers_from(&self.dirs, path)
    }

    /// [`Watched::covers`], its folders as `dirs` name them (the OS's own paths, links resolved).
    pub(crate) fn covers_from(&self, dirs: &[PathBuf], path: &Path) -> bool {
        dirs.iter().any(|d| match below(d, path) {
            // The folder itself (made, moved or gone): its waiter looks again.
            Some(0) => true,
            Some(n) => self.depth.reaches(n.saturating_sub(1)) && self.keeps(path),
            None => false,
        })
    }

    /// A change came (a backend's, or a test's).
    pub fn tell(&self) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
        self.changes.notify();
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        backend::forget(self.id);
    }
}

/// Whether file name `name` is what `pattern` asks: the same, or ending as `*<end>` says.
fn named(name: &str, pattern: &str) -> bool {
    match pattern.strip_prefix('*') {
        Some(end) => name.ends_with(end),
        None => name == pattern,
    }
}

/// How many components `path` is below `dir` (0: it is `dir`); none when it isn't under it.
pub(crate) fn below(dir: &Path, path: &Path) -> Option<u32> {
    let rest = path.strip_prefix(dir).ok()?;
    Some(u32::try_from(rest.components().count()).unwrap_or(u32::MAX))
}

/// `p` without `.` and `..` (as far as they go), so prefixes compare as the OS's paths will.
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// The nearest folder of `dir` that is there (`dir` itself when it is), and how many of its
/// components below that aren't yet.
pub(crate) fn nearest(dir: &Path) -> Option<(PathBuf, u32)> {
    let mut at = dir.to_path_buf();
    let mut missing = 0u32;
    loop {
        if at.is_dir() {
            return Some((at, missing));
        }
        if !at.pop() {
            return None;
        }
        missing = missing.saturating_add(1);
    }
}

/// What folder changes in a test look like, settled: `f` made a change; the watch's wakes are
/// counted until they've stayed put for `quiet` (bounded by `until`).
#[cfg(test)]
pub(crate) async fn settle(w: &Watched, quiet: std::time::Duration, until: std::time::Instant) -> u64 {
    let mut seen = w.wakes();
    loop {
        crate::rt::sleep(quiet).await;
        let now = w.wakes();
        if now == seen || std::time::Instant::now() >= until {
            return now;
        }
        seen = now;
    }
}

/// Until one of `pids` exits (goal 9: a process's end as the OS tells it, a pidfd on Linux,
/// kqueue's `EVFILT_PROC` on macOS, a wait on its handle on Windows); at once when one is gone
/// already; never with none to watch, or where the OS can't say.
pub async fn exited(pids: &[u32]) {
    backend::exited(pids).await;
}

#[cfg(target_os = "linux")]
#[path = "watch_linux.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "watch_macos.rs"]
mod backend;
#[cfg(windows)]
#[path = "watch_windows.rs"]
mod backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod backend {
    //! No folder events here: nothing is told but what [`super::Watched::tell`] is called for.
    pub(super) fn start(_w: &std::sync::Arc<super::Watched>) {}
    pub(super) fn forget(_id: u64) {}
    pub(super) async fn exited(_pids: &[u32]) {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Goal 9: what a watch keeps, by where a change is and what it's named, pure.
    #[test]
    fn g9_a_watch_covers_its_folders_as_deep_and_as_named_as_it_asks() {
        let w = Watched { changes: crate::rt::Notify::new(), wakes: AtomicU64::new(0), dirs: vec![PathBuf::from("/w")], depth: Depth::Below(1), names: vec!["*.json".into(), "lock".into()], id: 0 };
        assert!(w.covers(Path::new("/w")), "the folder itself");
        assert!(w.covers(Path::new("/w/a.json")) && w.covers(Path::new("/w/r/a.json")), "its entries and one below");
        assert!(!w.covers(Path::new("/w/r/l/a.json")), "two below: past its depth");
        assert!(!w.covers(Path::new("/w/a.json.tmp")) && w.covers(Path::new("/w/lock")) && !w.covers(Path::new("/v/a.json")), "named as it asks, under its folder");
        assert!(w.recursive() && w.depth() == Depth::Below(1));
        assert!(named("x.json", "*.json") && !named("x.jsonl", "*.json") && named("lock", "lock") && !named("locks", "lock"));
        assert_eq!(below(Path::new("/a"), Path::new("/a/b/c")), Some(2));
        assert_eq!(nearest(Path::new("/definitely/not/here/kiln")), Some((PathBuf::from("/"), 4)));
    }

    /// Goal 9: a change in a watched folder wakes its waiter, one later under it (in a folder made
    /// since) too, and a folder made where none was is watched from then on; a quiet span of
    /// seconds wakes nothing. Where the OS has no folder events this says nothing.
    #[test]
    fn g9_a_folder_change_wakes_its_watch_and_quiet_wakes_nothing() -> anyhow::Result<()> {
        if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            return Ok(());
        }
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let root = dir.path().canonicalize()?;
            let later = root.join("later").join("bin");
            let w = Watched::new(&[root.clone()], true, &[]);
            let l = Watched::new(&[later.clone()], false, &["tool"]);
            // A quiet span: nothing changes, nothing wakes it.
            crate::rt::sleep(Duration::from_secs(2)).await;
            anyhow::ensure!(w.wakes() == 0 && l.wakes() == 0, "an idle watch woke {} and {} times", w.wakes(), l.wakes());
            std::fs::write(root.join("a"), b"1")?;
            crate::rt::timeout(Duration::from_secs(30), w.changed()).await.map_err(|_| anyhow::anyhow!("a file made in the folder woke nothing"))?;
            std::fs::create_dir_all(root.join("sub"))?;
            crate::rt::sleep(Duration::from_millis(500)).await;
            let _ = settle(&w, Duration::from_millis(300), Instant::now() + Duration::from_secs(10)).await;
            let before = w.wakes();
            std::fs::write(root.join("sub").join("b"), b"2")?;
            let after = settle(&w, Duration::from_millis(300), Instant::now() + Duration::from_secs(30)).await;
            anyhow::ensure!(after > before, "a file in a folder made since woke nothing");
            // The folder not there before: made, then the tool put in it.
            std::fs::create_dir_all(&later)?;
            let _ = settle(&l, Duration::from_millis(300), Instant::now() + Duration::from_secs(10)).await;
            let before = l.wakes();
            std::fs::write(later.join("tool"), b"#!/bin/sh\n")?;
            let after = settle(&l, Duration::from_millis(300), Instant::now() + Duration::from_secs(30)).await;
            anyhow::ensure!(after > before, "a tool put in a folder made since woke nothing");
            // Quiet again: nothing more.
            let (w0, l0) = (w.wakes(), l.wakes());
            crate::rt::sleep(Duration::from_secs(2)).await;
            anyhow::ensure!(w.wakes() == w0 && l.wakes() == l0, "quiet, it woke {} and {} more times", w.wakes() - w0, l.wakes() - l0);
            Ok(())
        })
    }

    /// Goal 9: a change named otherwise is let go where the OS says names, and one past the
    /// depth asked is too.
    #[test]
    fn g9_a_watch_keeps_only_what_it_names_and_reaches() -> anyhow::Result<()> {
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return Ok(());
        }
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let root = dir.path().canonicalize()?;
            std::fs::create_dir_all(root.join("r").join("l").join("deep"))?;
            let w = Watched::reaching(&[root.clone()], Depth::Below(2), &["*.json"]);
            crate::rt::sleep(Duration::from_millis(500)).await;
            std::fs::write(root.join("r").join("l").join("x.tmp"), b"1")?;
            std::fs::write(root.join("r").join("l").join("deep").join("y.json"), b"1")?;
            crate::rt::sleep(Duration::from_secs(2)).await;
            anyhow::ensure!(w.wakes() == 0, "a change it doesn't name, or past its depth, woke it {} times", w.wakes());
            std::fs::write(root.join("r").join("l").join("1.json.tmp"), b"1")?;
            std::fs::rename(root.join("r").join("l").join("1.json.tmp"), root.join("r").join("l").join("1.json"))?;
            crate::rt::timeout(Duration::from_secs(30), w.changed()).await.map_err(|_| anyhow::anyhow!("a record renamed into place woke nothing"))?;
            Ok(())
        })
    }

    /// Goal 9: a process's end ends the wait on it, as the OS tells it (a pidfd, kqueue's
    /// `EVFILT_PROC`); one already gone ends it at once; with none to watch it never ends. The
    /// planted mistake it catches: `exited` pending for ever (no exit heard) fails at the kill.
    #[cfg(unix)]
    #[test]
    fn g9_a_process_end_ends_the_wait_on_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
            let pid = child.id();
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(1), exited(&[pid])).await.is_err(), "a running process's wait ended");
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(1), exited(&[])).await.is_err(), "a wait on nothing ended");
            let pids = [pid];
            let waiting = exited(&pids);
            child.kill()?;
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(20), waiting).await.is_ok(), "its end didn't end the wait");
            child.wait()?;
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(5), exited(&[pid])).await.is_ok(), "a wait on a process gone didn't end at once");
            Ok(())
        })
    }
}
