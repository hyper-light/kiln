//! A machine's disk, as its processes and a power loss see it: folders, files and links, what the
//! processes see apart from what survives the power going.
//!
//! - **Data** (a file's bytes, its exec bit) is durable once the file is fsynced. A power loss
//!   leaves each file written since its last fsync as it was written, as it was before, torn (the
//!   new bytes up to a page, the old after), or zeroed, by the seed's draws.
//! - **Names** (making, removing and renaming files, links and folders) go into a journal, in
//!   order, durable once their folder is fsynced (or, on a filesystem with one journal, as
//!   ext4's, once any file is: [`DiskFaults::fsync_commits_names`]). A power loss keeps a prefix
//!   of what wasn't durable yet: never a later change of names without the ones before it.
//! - **Faults**, while the nemesis lasts: fsyncs and writes that fail (EIO, nothing done),
//!   writes refused past the disk's room (ENOSPC), and a crash at a chosen write
//!   ([`Disk::freeze_after`]): that write and every one after fail until the model has crashed
//!   the machine and [`Disk::thaw`]s it.
//! - **Times**: a file's modification time is the simulated clock's, as fine as the seed's
//!   filesystem keeps it ([`DiskFaults::mtime_step`]): writes within one step leave it as it was.
//!
//! Paths are absolute and `/`-separated; the root always exists.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use kiln_seams::clock::{SIM_EPOCH_MS, SimClock};
use parking_lot::Mutex;

use crate::rng::{Rng, fnv};

/// A disk's page: the unit a torn write tears at.
pub const PAGE: usize = 4096;

/// How a disk fails, and how its filesystem keeps time and names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskFaults {
    /// Fsyncs that fail (EIO, nothing made durable), per thousand.
    pub fsync_fail: u64,
    /// Writes that fail (EIO, nothing written), per thousand.
    pub write_fail: u64,
    /// The most bytes the files may hold: a write past it is refused (ENOSPC). None: no limit.
    pub capacity: Option<u64>,
    /// How fine files' modification times are.
    pub mtime_step: Duration,
    /// Whether an fsync of a file makes every change of names before it durable too (one journal
    /// for the filesystem, as ext4 has); else only an fsync of their folder does, as POSIX says.
    pub fsync_commits_names: bool,
    /// When the faults stop (fsync, write and room faults; crashes and power losses are the
    /// model's to make).
    pub until: Duration,
}

impl DiskFaults {
    /// A disk that never fails, with nanosecond times and POSIX's names.
    pub fn none() -> Self {
        Self { fsync_fail: 0, write_fail: 0, capacity: None, mtime_step: Duration::from_nanos(1), fsync_commits_names: false, until: Duration::ZERO }
    }

    /// A seed's own disk: its filesystem's time step (a nanosecond, a millisecond, a 4 ms tick, a
    /// second, two), how it journals names, and its faults until `until`.
    pub fn draw(rng: &mut Rng, until: Duration) -> Self {
        let steps = [Duration::from_nanos(1), Duration::from_millis(1), Duration::from_millis(4), Duration::from_secs(1), Duration::from_secs(2)];
        let mtime_step = rng.pick(&steps).copied().unwrap_or(Duration::from_nanos(1));
        Self { fsync_fail: rng.range(0, 30), write_fail: rng.range(0, 10), capacity: None, mtime_step, fsync_commits_names: rng.chance(500), until }
    }
}

/// What a disk did, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCounts {
    /// Changes (writes, names, exec bits) and fsyncs asked for.
    pub writes: u64,
    pub fsyncs: u64,
    pub fsync_errors: u64,
    pub write_errors: u64,
    /// Writes refused for room.
    pub full: u64,
    /// Crashes at a chosen write.
    pub frozen: u64,
    pub power_losses: u64,
    /// At power losses: files left torn, zeroed, as before, and changes of names lost.
    pub torn: u64,
    pub zeroed: u64,
    pub lost: u64,
    pub names_lost: u64,
}

impl DiskCounts {
    pub fn add(&mut self, o: &Self) {
        let pairs = [
            (&mut self.writes, o.writes),
            (&mut self.fsyncs, o.fsyncs),
            (&mut self.fsync_errors, o.fsync_errors),
            (&mut self.write_errors, o.write_errors),
            (&mut self.full, o.full),
            (&mut self.frozen, o.frozen),
            (&mut self.power_losses, o.power_losses),
            (&mut self.torn, o.torn),
            (&mut self.zeroed, o.zeroed),
            (&mut self.lost, o.lost),
            (&mut self.names_lost, o.names_lost),
        ];
        for (mine, theirs) in pairs {
            *mine = mine.saturating_add(theirs);
        }
    }
}

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Link,
}

/// A path's metadata, as `symlink_metadata` gives it (a link is itself, not what it names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub len: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    pub exec: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Dir,
    Ino(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    File { data: Vec<u8>, exec: bool, mtime_ns: i128 },
    Link { target: String },
}

impl Body {
    fn len(&self) -> u64 {
        match self {
            Self::File { data, .. } => u64::try_from(data.len()).unwrap_or(u64::MAX),
            Self::Link { .. } => 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Inode {
    live: Body,
    durable: Body,
    /// Written since its last fsync.
    dirty: bool,
}

#[derive(Debug, Clone)]
enum NameOp {
    Put(String, Entry),
    Remove(String),
    Rename { from: String, to: String },
}

impl NameOp {
    /// Whether it changes a name in folder `dir`.
    fn touches(&self, dir: &str) -> bool {
        match self {
            Self::Put(p, _) | Self::Remove(p) => parent(p) == dir,
            Self::Rename { from, to } => parent(from) == dir || parent(to) == dir,
        }
    }

    fn apply(&self, names: &mut BTreeMap<String, Entry>) {
        match self {
            Self::Put(p, e) => {
                names.insert(p.clone(), *e);
            }
            Self::Remove(p) => {
                names.remove(p);
            }
            Self::Rename { from, to } => {
                if let Some(e) = names.remove(from) {
                    names.insert(to.clone(), e);
                }
            }
        }
    }
}

struct State {
    clock: SimClock,
    rng: Rng,
    faults: DiskFaults,
    /// What the processes see.
    live: BTreeMap<String, Entry>,
    /// What survives a power loss.
    durable: BTreeMap<String, Entry>,
    /// Changes of names not durable yet, oldest first.
    journal: Vec<NameOp>,
    inodes: BTreeMap<u64, Inode>,
    next_ino: u64,
    /// Bytes the live files hold.
    used: u64,
    /// Writes until the crash a model chose (`freeze_after`), and whether it has come.
    freeze: Option<u64>,
    frozen: bool,
    counts: DiskCounts,
}

/// A machine's disk. Clones share it.
#[derive(Clone)]
pub struct Disk {
    inner: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.inner.lock();
        f.debug_struct("Disk").field("paths", &st.live.len()).field("used", &st.used).field("counts", &st.counts).finish()
    }
}

/// `path` without a trailing `/` and with no empty parts: "/" for the root.
pub fn normal(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    format!("/{}", parts.join("/"))
}

/// The folder `path` is in ("/" for the root and what's in it).
pub fn parent(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((dir, _)) => dir,
    }
}

/// `dir` joined with `name`.
pub fn join(dir: &str, name: &str) -> String {
    normal(&format!("{dir}/{name}"))
}

fn err(kind: io::ErrorKind, what: &str, path: &str) -> io::Error {
    io::Error::new(kind, format!("{what}: {path} (kiln-sim)"))
}

fn crashed() -> io::Error {
    io::Error::other("the machine crashed at this write (kiln-sim)")
}

impl Disk {
    /// An empty disk (its root alone), its times by `clock`, its draws `rng`'s.
    pub fn new(clock: SimClock, rng: Rng, faults: DiskFaults) -> Self {
        let mut live = BTreeMap::new();
        live.insert("/".to_owned(), Entry::Dir);
        let durable = live.clone();
        Self {
            inner: Arc::new(Mutex::new(State {
                clock,
                rng,
                faults,
                live,
                durable,
                journal: Vec::new(),
                inodes: BTreeMap::new(),
                next_ino: 1,
                used: 0,
                freeze: None,
                frozen: false,
                counts: DiskCounts::default(),
            })),
        }
    }

    pub fn faults(&self) -> DiskFaults {
        self.inner.lock().faults
    }

    /// Room for at most `capacity` bytes from now (none: no limit).
    pub fn set_capacity(&self, capacity: Option<u64>) {
        self.inner.lock().faults.capacity = capacity;
    }

    /// The disk's clock now, in nanoseconds since the Unix epoch, as fine as the simulation's (what
    /// files' times are stamped from, before the filesystem's step).
    pub fn now_ns(&self) -> i128 {
        let st = self.inner.lock();
        i128::from(SIM_EPOCH_MS).saturating_mul(1_000_000).saturating_add(i128::try_from(st.now().as_nanos()).unwrap_or(i128::MAX))
    }

    /// Bytes the files hold.
    pub fn used(&self) -> u64 {
        self.inner.lock().used
    }

    pub fn counts(&self) -> DiskCounts {
        self.inner.lock().counts
    }

    // ---------------------------------------------------------------------------- reading

    /// A path's metadata, not following a link.
    pub fn meta(&self, path: &str) -> io::Result<Meta> {
        let st = self.inner.lock();
        let p = normal(path);
        match st.live.get(&p) {
            None => Err(err(io::ErrorKind::NotFound, "no such file or folder", &p)),
            Some(Entry::Dir) => Ok(Meta { kind: Kind::Dir, len: 0, mtime_ns: 0, exec: false }),
            Some(Entry::Ino(i)) => match st.inodes.get(i).map(|n| &n.live) {
                Some(Body::File { data, exec, mtime_ns }) => Ok(Meta { kind: Kind::File, len: u64::try_from(data.len()).unwrap_or(u64::MAX), mtime_ns: *mtime_ns, exec: *exec }),
                Some(Body::Link { .. }) => Ok(Meta { kind: Kind::Link, len: 0, mtime_ns: 0, exec: false }),
                None => Err(err(io::ErrorKind::NotFound, "a name without a file", &p)),
            },
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.inner.lock().live.contains_key(&normal(path))
    }

    pub fn is_file(&self, path: &str) -> bool {
        self.meta(path).is_ok_and(|m| m.kind == Kind::File)
    }

    pub fn is_dir(&self, path: &str) -> bool {
        self.meta(path).is_ok_and(|m| m.kind == Kind::Dir)
    }

    /// A file's bytes (a link isn't followed: reading one is refused).
    pub fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let st = self.inner.lock();
        let p = normal(path);
        match st.live.get(&p) {
            None => Err(err(io::ErrorKind::NotFound, "no such file", &p)),
            Some(Entry::Dir) => Err(err(io::ErrorKind::IsADirectory, "a folder", &p)),
            Some(Entry::Ino(i)) => match st.inodes.get(i).map(|n| &n.live) {
                Some(Body::File { data, .. }) => Ok(data.clone()),
                Some(Body::Link { .. }) => Err(err(io::ErrorKind::InvalidInput, "a link, not followed", &p)),
                None => Err(err(io::ErrorKind::NotFound, "a name without a file", &p)),
            },
        }
    }

    /// What a link names.
    pub fn read_link(&self, path: &str) -> io::Result<String> {
        let st = self.inner.lock();
        let p = normal(path);
        match st.live.get(&p).and_then(|e| if let Entry::Ino(i) = e { st.inodes.get(i) } else { None }).map(|n| &n.live) {
            Some(Body::Link { target }) => Ok(target.clone()),
            _ => Err(err(io::ErrorKind::InvalidInput, "not a link", &p)),
        }
    }

    /// What folder `dir` holds: each name, and what it is, by name.
    pub fn list(&self, dir: &str) -> io::Result<Vec<(String, Kind)>> {
        let st = self.inner.lock();
        let d = normal(dir);
        if st.live.get(&d) != Some(&Entry::Dir) {
            return Err(err(io::ErrorKind::NotFound, "no such folder", &d));
        }
        let prefix = if d == "/" { "/".to_owned() } else { format!("{d}/") };
        let mut out = Vec::new();
        for (p, e) in st.live.range(prefix.clone()..) {
            let Some(rest) = p.strip_prefix(&prefix) else { break };
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            let kind = match e {
                Entry::Dir => Kind::Dir,
                Entry::Ino(i) => match st.inodes.get(i).map(|n| &n.live) {
                    Some(Body::Link { .. }) => Kind::Link,
                    _ => Kind::File,
                },
            };
            out.push((rest.to_owned(), kind));
        }
        Ok(out)
    }

    /// Every file under `dir` (links as `->target`), by path relative to it: what a checker reads.
    pub fn files(&self, dir: &str) -> BTreeMap<String, (Vec<u8>, bool)> {
        let st = self.inner.lock();
        let d = normal(dir);
        let prefix = if d == "/" { "/".to_owned() } else { format!("{d}/") };
        let mut out = BTreeMap::new();
        for (p, e) in st.live.range(prefix.clone()..) {
            let Some(rest) = p.strip_prefix(&prefix) else { break };
            if let Entry::Ino(i) = e {
                match st.inodes.get(i).map(|n| &n.live) {
                    Some(Body::File { data, exec, .. }) => {
                        out.insert(rest.to_owned(), (data.clone(), *exec));
                    }
                    Some(Body::Link { target }) => {
                        out.insert(rest.to_owned(), (format!("->{target}").into_bytes(), false));
                    }
                    None => {}
                }
            }
        }
        out
    }

    /// The live tree folded into one number, for a trace.
    pub fn digest(&self) -> u64 {
        let st = self.inner.lock();
        let mut h = 0u64;
        for (p, e) in &st.live {
            h = crate::rng::mix(h ^ fnv(p.as_bytes()));
            if let Entry::Ino(i) = e
                && let Some(n) = st.inodes.get(i)
            {
                h = crate::rng::mix(
                    h ^ match &n.live {
                        Body::File { data, exec, .. } => fnv(data) ^ u64::from(*exec),
                        Body::Link { target } => fnv(target.as_bytes()),
                    },
                );
            }
        }
        h
    }

    // ---------------------------------------------------------------------------- changing

    /// Makes `path` hold `bytes` (made, or cut to nothing and written), its exec bit kept.
    pub fn write(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        st.write(&normal(path), bytes.to_vec(), None)
    }

    /// Makes `to` hold `from`'s bytes and exec bit, as `std::fs::copy` does: how many bytes.
    pub fn copy(&self, from: &str, to: &str) -> io::Result<u64> {
        let mut st = self.inner.lock();
        st.change()?;
        let f = normal(from);
        let (data, exec) = match st.live.get(&f).and_then(|e| if let Entry::Ino(i) = e { st.inodes.get(i) } else { None }).map(|n| &n.live) {
            Some(Body::File { data, exec, .. }) => (data.clone(), *exec),
            _ => return Err(err(io::ErrorKind::NotFound, "no such file to copy", &f)),
        };
        let n = u64::try_from(data.len()).unwrap_or(u64::MAX);
        st.write(&normal(to), data, Some(exec))?;
        Ok(n)
    }

    /// Sets a file's exec bit.
    pub fn set_exec(&self, path: &str, exec: bool) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        let p = normal(path);
        let ino = st.ino(&p)?;
        if let Some(n) = st.inodes.get_mut(&ino)
            && let Body::File { exec: x, .. } = &mut n.live
        {
            *x = exec;
            n.dirty = true;
        }
        Ok(())
    }

    /// Makes a link at `path` naming `target`.
    pub fn symlink(&self, target: &str, path: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        let p = normal(path);
        if st.live.contains_key(&p) {
            return Err(err(io::ErrorKind::AlreadyExists, "already there", &p));
        }
        st.parent_is_dir(&p)?;
        let ino = st.next_ino;
        st.next_ino = st.next_ino.saturating_add(1);
        let body = Body::Link { target: target.to_owned() };
        st.inodes.insert(ino, Inode { live: body.clone(), durable: body, dirty: false });
        st.live.insert(p.clone(), Entry::Ino(ino));
        st.journal.push(NameOp::Put(p, Entry::Ino(ino)));
        Ok(())
    }

    /// Makes folder `path` and every folder above it that's missing.
    pub fn create_dir_all(&self, path: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        let p = normal(path);
        let mut at = String::new();
        for part in p.split('/').filter(|s| !s.is_empty()) {
            at = format!("{at}/{part}");
            match st.live.get(&at) {
                Some(Entry::Dir) => {}
                Some(Entry::Ino(_)) => return Err(err(io::ErrorKind::NotADirectory, "a file on the way", &at)),
                None => {
                    st.change()?;
                    st.live.insert(at.clone(), Entry::Dir);
                    st.journal.push(NameOp::Put(at.clone(), Entry::Dir));
                }
            }
        }
        Ok(())
    }

    /// Removes a file or link.
    pub fn remove_file(&self, path: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        let p = normal(path);
        match st.live.get(&p).copied() {
            None => Err(err(io::ErrorKind::NotFound, "no such file", &p)),
            Some(Entry::Dir) => Err(err(io::ErrorKind::IsADirectory, "a folder", &p)),
            Some(Entry::Ino(i)) => {
                st.live.remove(&p);
                st.journal.push(NameOp::Remove(p));
                st.forget_if_unnamed(i);
                Ok(())
            }
        }
    }

    /// Removes an empty folder.
    pub fn remove_dir(&self, path: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        let p = normal(path);
        if p == "/" {
            return Err(err(io::ErrorKind::PermissionDenied, "the root", &p));
        }
        match st.live.get(&p) {
            None => return Err(err(io::ErrorKind::NotFound, "no such folder", &p)),
            Some(Entry::Ino(_)) => return Err(err(io::ErrorKind::NotADirectory, "not a folder", &p)),
            Some(Entry::Dir) => {}
        }
        let prefix = format!("{p}/");
        if st.live.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix)) {
            return Err(err(io::ErrorKind::DirectoryNotEmpty, "not empty", &p));
        }
        st.live.remove(&p);
        st.journal.push(NameOp::Remove(p));
        Ok(())
    }

    /// Renames a file or link over whatever file is at `to`, at once.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        let (f, t) = (normal(from), normal(to));
        let entry = match st.live.get(&f).copied() {
            None => return Err(err(io::ErrorKind::NotFound, "no such file to rename", &f)),
            Some(Entry::Dir) => return Err(err(io::ErrorKind::Unsupported, "renaming a folder", &f)),
            Some(e) => e,
        };
        if st.live.get(&t) == Some(&Entry::Dir) {
            return Err(err(io::ErrorKind::IsADirectory, "a folder in the way", &t));
        }
        st.parent_is_dir(&t)?;
        if f == t {
            return Ok(());
        }
        st.live.remove(&f);
        let replaced = st.live.insert(t.clone(), entry);
        st.journal.push(NameOp::Rename { from: f, to: t });
        if let Some(Entry::Ino(old)) = replaced {
            st.forget_if_unnamed(old);
        }
        Ok(())
    }

    /// Makes a file's data durable (and, on a filesystem with one journal, every change of names
    /// before it). A folder's fsync is [`Disk::fsync_dir`]'s.
    pub fn fsync(&self, path: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        st.change()?;
        st.counts.fsyncs = st.counts.fsyncs.saturating_add(1);
        let p = normal(path);
        if st.live.get(&p) == Some(&Entry::Dir) {
            drop(st);
            return self.fsync_dir_counted(&p);
        }
        let ino = st.ino(&p)?;
        if st.fault(|f| f.fsync_fail) {
            st.counts.fsync_errors = st.counts.fsync_errors.saturating_add(1);
            return Err(io::Error::other(format!("fsync failed (EIO): {p} (kiln-sim)")));
        }
        if let Some(n) = st.inodes.get_mut(&ino) {
            n.durable = n.live.clone();
            n.dirty = false;
        }
        if st.faults.fsync_commits_names {
            let all = st.journal.len();
            st.commit(all);
        }
        Ok(())
    }

    /// Makes the changes of names in folder `dir` durable (and every change before the last of
    /// them: the journal is kept in order).
    pub fn fsync_dir(&self, dir: &str) -> io::Result<()> {
        {
            let mut st = self.inner.lock();
            st.change()?;
            st.counts.fsyncs = st.counts.fsyncs.saturating_add(1);
        }
        self.fsync_dir_counted(&normal(dir))
    }

    fn fsync_dir_counted(&self, d: &str) -> io::Result<()> {
        let mut st = self.inner.lock();
        if st.live.get(d) != Some(&Entry::Dir) {
            return Err(err(io::ErrorKind::NotFound, "no such folder", d));
        }
        if st.fault(|f| f.fsync_fail) {
            st.counts.fsync_errors = st.counts.fsync_errors.saturating_add(1);
            return Err(io::Error::other(format!("fsync failed (EIO): {d} (kiln-sim)")));
        }
        if let Some(last) = st.journal.iter().rposition(|op| op.touches(d)) {
            st.commit(last.saturating_add(1));
        }
        Ok(())
    }

    /// Makes `path` (a file's bytes, or a folder) and every change of names so far durable, with
    /// no fault and no crash point: a model's own act, as a person's editor saving a file whole.
    /// What a power loss tears after that is then what the code under test left unsynced.
    pub fn make_durable(&self, path: &str) {
        let mut st = self.inner.lock();
        let p = normal(path);
        if let Some(Entry::Ino(i)) = st.live.get(&p).copied()
            && let Some(n) = st.inodes.get_mut(&i)
        {
            n.durable = n.live.clone();
            n.dirty = false;
        }
        let all = st.journal.len();
        st.commit(all);
    }

    // ---------------------------------------------------------------------------- the nemesis

    /// The power goes: names keep a prefix of what wasn't durable, each file written since its
    /// last fsync is left as written, as before, torn or zeroed, and that is what's seen after.
    pub fn power_loss(&self) {
        let mut st = self.inner.lock();
        st.power_loss();
    }

    /// A crash at the `n`th change from now (0: the next): it and every change after it fail,
    /// until [`Disk::thaw`].
    pub fn freeze_after(&self, n: u64) {
        let mut st = self.inner.lock();
        st.freeze = Some(n);
    }

    /// Whether the crash a model chose has come.
    pub fn frozen(&self) -> bool {
        self.inner.lock().frozen
    }

    /// Changes work again (the machine has crashed and come back).
    pub fn thaw(&self) {
        let mut st = self.inner.lock();
        st.freeze = None;
        st.frozen = false;
    }

    /// The changes made so far: where a model aims a crash.
    pub fn writes(&self) -> u64 {
        self.inner.lock().counts.writes
    }
}

impl State {
    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    fn mtime(&self) -> i128 {
        let ns = i128::from(SIM_EPOCH_MS).saturating_mul(1_000_000).saturating_add(i128::try_from(self.now().as_nanos()).unwrap_or(i128::MAX));
        let step = i128::try_from(self.faults.mtime_step.as_nanos()).unwrap_or(1).max(1);
        ns.saturating_sub(ns.checked_rem(step).unwrap_or(0))
    }

    /// Whether a fault `which` gives the chance of strikes now.
    fn fault(&mut self, which: impl Fn(&DiskFaults) -> u64) -> bool {
        let n = which(&self.faults);
        n > 0 && self.now() < self.faults.until && self.rng.chance(n)
    }

    /// A change is about to be made: counted, and refused once the chosen crash has come.
    fn change(&mut self) -> io::Result<()> {
        if self.frozen {
            return Err(crashed());
        }
        if let Some(n) = self.freeze {
            if n == 0 {
                self.frozen = true;
                self.counts.frozen = self.counts.frozen.saturating_add(1);
                return Err(crashed());
            }
            self.freeze = Some(n.saturating_sub(1));
        }
        self.counts.writes = self.counts.writes.saturating_add(1);
        Ok(())
    }

    fn ino(&self, p: &str) -> io::Result<u64> {
        match self.live.get(p) {
            Some(Entry::Ino(i)) => Ok(*i),
            Some(Entry::Dir) => Err(err(io::ErrorKind::IsADirectory, "a folder", p)),
            None => Err(err(io::ErrorKind::NotFound, "no such file", p)),
        }
    }

    fn parent_is_dir(&self, p: &str) -> io::Result<()> {
        match self.live.get(parent(p)) {
            Some(Entry::Dir) => Ok(()),
            Some(Entry::Ino(_)) => Err(err(io::ErrorKind::NotADirectory, "its folder is a file", p)),
            None => Err(err(io::ErrorKind::NotFound, "its folder doesn't exist", p)),
        }
    }

    fn write(&mut self, p: &str, data: Vec<u8>, exec: Option<bool>) -> io::Result<()> {
        if self.fault(|f| f.write_fail) {
            self.counts.write_errors = self.counts.write_errors.saturating_add(1);
            return Err(io::Error::other(format!("write failed (EIO): {p} (kiln-sim)")));
        }
        let mtime_ns = self.mtime();
        let new_len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        match self.live.get(p).copied() {
            Some(Entry::Dir) => Err(err(io::ErrorKind::IsADirectory, "a folder", p)),
            Some(Entry::Ino(i)) => {
                let old = self.inodes.get(&i).map_or(0, |n| n.live.len());
                self.room(old, new_len, p)?;
                if let Some(n) = self.inodes.get_mut(&i) {
                    let keep_exec = matches!(n.live, Body::File { exec: true, .. });
                    n.live = Body::File { data, exec: exec.unwrap_or(keep_exec), mtime_ns };
                    n.dirty = true;
                }
                self.used = self.used.saturating_sub(old).saturating_add(new_len);
                Ok(())
            }
            None => {
                self.parent_is_dir(p)?;
                self.room(0, new_len, p)?;
                let ino = self.next_ino;
                self.next_ino = self.next_ino.saturating_add(1);
                let exec = exec.unwrap_or(false);
                let empty = Body::File { data: Vec::new(), exec, mtime_ns };
                self.inodes.insert(ino, Inode { live: Body::File { data, exec, mtime_ns }, durable: empty, dirty: true });
                self.live.insert(p.to_owned(), Entry::Ino(ino));
                self.journal.push(NameOp::Put(p.to_owned(), Entry::Ino(ino)));
                self.used = self.used.saturating_add(new_len);
                Ok(())
            }
        }
    }

    /// Whether replacing `old` bytes with `new` fits the disk's room.
    fn room(&mut self, old: u64, new: u64, p: &str) -> io::Result<()> {
        if let Some(cap) = self.faults.capacity
            && new > old
            && self.used.saturating_sub(old).saturating_add(new) > cap
        {
            self.counts.full = self.counts.full.saturating_add(1);
            return Err(err(io::ErrorKind::StorageFull, "no room left on the disk (ENOSPC)", p));
        }
        Ok(())
    }

    /// Drops inode `i` when no live or durable name holds it.
    fn forget_if_unnamed(&mut self, i: u64) {
        let named = |m: &BTreeMap<String, Entry>| m.values().any(|e| *e == Entry::Ino(i));
        let journaled = self.journal.iter().any(|op| matches!(op, NameOp::Put(_, Entry::Ino(j)) if *j == i));
        if !named(&self.live) && !named(&self.durable) && !journaled {
            if let Some(n) = self.inodes.remove(&i) {
                self.used = self.used.saturating_sub(n.live.len());
            }
        } else if !named(&self.live)
            && let Some(n) = self.inodes.get(&i)
        {
            // Still on the platter by an older name: no longer what the processes' bytes hold.
            self.used = self.used.saturating_sub(n.live.len());
        }
    }

    /// The first `n` changes of names in the journal made durable.
    fn commit(&mut self, n: usize) {
        let n = n.min(self.journal.len());
        let done: Vec<NameOp> = self.journal.drain(..n).collect();
        for op in &done {
            op.apply(&mut self.durable);
        }
    }

    fn power_loss(&mut self) {
        self.counts.power_losses = self.counts.power_losses.saturating_add(1);
        let pending = self.journal.len();
        let kept = usize::try_from(self.rng.range(0, u64::try_from(pending).unwrap_or(0))).unwrap_or(0);
        self.commit(kept);
        let lost = self.journal.len();
        self.counts.names_lost = self.counts.names_lost.saturating_add(u64::try_from(lost).unwrap_or(0));
        self.journal.clear();
        let dirty: Vec<u64> = self.inodes.iter().filter(|(_, n)| n.dirty).map(|(i, _)| *i).collect();
        for i in dirty {
            let roll = self.rng.below(100);
            let page = self.rng.below(u64::MAX);
            let Some(n) = self.inodes.get_mut(&i) else { continue };
            let outcome = match (&n.live, &n.durable) {
                (Body::File { data: new, exec, mtime_ns }, Body::File { data: old, .. }) => match roll {
                    0..40 => Some(n.live.clone()),
                    40..70 => None,
                    70..90 => {
                        // The new bytes up to a page, the old after (or nothing past the old end).
                        let pages = new.len().div_ceil(PAGE);
                        let k = usize::try_from(page.checked_rem(u64::try_from(pages).unwrap_or(1).max(1)).unwrap_or(0)).unwrap_or(0);
                        let cut = k.saturating_mul(PAGE).min(new.len());
                        let mut torn = new.get(..cut).unwrap_or_default().to_vec();
                        torn.extend(old.get(cut..new.len().min(old.len().max(cut))).unwrap_or_default());
                        self.counts.torn = self.counts.torn.saturating_add(1);
                        Some(Body::File { data: torn, exec: *exec, mtime_ns: *mtime_ns })
                    }
                    _ => {
                        self.counts.zeroed = self.counts.zeroed.saturating_add(1);
                        Some(Body::File { data: vec![0; new.len()], exec: *exec, mtime_ns: *mtime_ns })
                    }
                },
                _ => (roll < 50).then(|| n.live.clone()),
            };
            match outcome {
                Some(body) => n.durable = body,
                None => self.counts.lost = self.counts.lost.saturating_add(1),
            }
            n.dirty = false;
        }
        self.live = self.durable.clone();
        let named: std::collections::BTreeSet<u64> = self.live.values().filter_map(|e| if let Entry::Ino(i) = e { Some(*i) } else { None }).collect();
        self.inodes.retain(|i, _| named.contains(i));
        for n in self.inodes.values_mut() {
            n.live = n.durable.clone();
        }
        self.used = self.inodes.values().map(|n| n.live.len()).fold(0u64, u64::saturating_add);
        self.freeze = None;
        self.frozen = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(seed: u64, faults: DiskFaults) -> (SimClock, Disk) {
        let clock = SimClock::new();
        (clock.clone(), Disk::new(clock, Rng::new(seed), faults))
    }

    /// CP-T6.1: what the processes see: folders, files, links, renames over a file, exec bits.
    #[test]
    fn a_disk_holds_folders_files_and_links_as_a_process_sees_them() -> io::Result<()> {
        let (_, d) = disk(1, DiskFaults::none());
        assert!(d.write("/a/x", b"no folder").is_err_and(|e| e.kind() == io::ErrorKind::NotFound));
        d.create_dir_all("/a/b")?;
        d.write("/a/b/f", b"one")?;
        d.symlink("f", "/a/b/l")?;
        d.write("/a/t", b"two")?;
        d.set_exec("/a/t", true)?;
        d.rename("/a/t", "/a/b/f")?;
        assert_eq!(d.read("/a/b/f")?, b"two");
        assert!(d.meta("/a/b/f")?.exec && !d.exists("/a/t"));
        assert_eq!(d.read_link("/a/b/l")?, "f");
        assert_eq!(d.list("/a/b")?, [("f".to_owned(), Kind::File), ("l".to_owned(), Kind::Link)]);
        assert!(d.remove_dir("/a/b").is_err_and(|e| e.kind() == io::ErrorKind::DirectoryNotEmpty));
        d.copy("/a/b/f", "/a/c")?;
        assert!(d.meta("/a/c")?.exec, "a copy takes the exec bit");
        d.remove_file("/a/b/f")?;
        d.remove_file("/a/b/l")?;
        d.remove_dir("/a/b")?;
        assert_eq!(d.files("/a").into_keys().collect::<Vec<_>>(), ["c"]);
        assert_eq!(d.used(), 3);
        Ok(())
    }

    /// CP-T6.1: a power loss keeps what was made durable: a file written, fsynced and its folder
    /// fsynced survives whatever the draws; a rename over a file is the old or the new, never
    /// neither, once the new file's bytes were fsynced.
    #[test]
    fn what_was_made_durable_survives_a_power_loss() -> io::Result<()> {
        for seed in 0..200 {
            let (_, d) = disk(seed, DiskFaults::none());
            d.create_dir_all("/s")?;
            d.fsync_dir("/")?;
            d.write("/s/state", b"old")?;
            d.fsync("/s/state")?;
            d.fsync_dir("/s")?;
            d.write("/s/state.tmp", b"new")?;
            d.fsync("/s/state.tmp")?;
            d.rename("/s/state.tmp", "/s/state")?;
            d.power_loss();
            let got = d.read("/s/state")?;
            assert!(got == b"old" || got == b"new", "seed {seed}: {got:?}");
        }
        Ok(())
    }

    /// CP-T6.1: what wasn't made durable is torn, zeroed, lost or kept, by the seed: a file
    /// renamed in without its bytes fsynced comes back empty, zeroed or torn (the bug every
    /// "write a temporary file and rename it" without an fsync has), and a seed does the same
    /// every time.
    #[test]
    fn what_wasnt_made_durable_is_torn_lost_or_kept() -> io::Result<()> {
        let run = |seed| -> io::Result<(Vec<u8>, DiskCounts)> {
            let (_, d) = disk(seed, DiskFaults { fsync_commits_names: true, ..DiskFaults::none() });
            d.create_dir_all("/s")?;
            d.write("/s/f", &vec![1u8; 3 * PAGE])?;
            d.fsync("/s/f")?;
            d.write("/s/f.tmp", &vec![2u8; 3 * PAGE])?;
            d.rename("/s/f.tmp", "/s/f")?;
            d.fsync_dir("/s")?;
            d.power_loss();
            Ok((d.read("/s/f")?, d.counts()))
        };
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..300 {
            let (got, counts) = run(seed)?;
            assert_eq!((got.clone(), counts), run(seed)?, "seed {seed} the same every time");
            seen.insert(match got.as_slice() {
                [] => "empty",
                g if g.len() == 3 * PAGE && g.iter().all(|b| *b == 2) => "new",
                g if g.iter().all(|b| *b == 0) => "zeroed",
                _ => "torn",
            });
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["empty", "new", "torn", "zeroed"]);
        Ok(())
    }

    /// CP-T6.1: changes of names survive in order: never a later one without those before it.
    #[test]
    fn names_survive_a_power_loss_as_a_prefix() -> io::Result<()> {
        for seed in 0..200 {
            let (_, d) = disk(seed, DiskFaults::none());
            for i in 0..10 {
                d.write(&format!("/f{i}"), b"x")?;
            }
            d.power_loss();
            let names: Vec<String> = d.list("/")?.into_iter().map(|(n, _)| n).collect();
            let want: Vec<String> = (0..names.len()).map(|i| format!("f{i}")).collect();
            assert_eq!(names, want, "seed {seed}");
        }
        Ok(())
    }

    /// CP-T6.1: a crash at a chosen write: it and every change after fail until thawed; faults
    /// strike only while they last; a full disk refuses typed.
    #[test]
    fn a_crash_comes_at_its_write_and_faults_fail_typed() -> io::Result<()> {
        let (clock, d) = disk(3, DiskFaults { fsync_fail: 1000, until: Duration::from_secs(10), ..DiskFaults::none() });
        d.write("/a", b"1")?;
        d.freeze_after(1);
        d.write("/b", b"2")?;
        assert!(d.write("/c", b"3").is_err() && d.frozen() && d.read("/a").is_ok());
        assert!(d.remove_file("/a").is_err(), "nothing changes after the crash");
        d.thaw();
        assert!(d.fsync("/a").is_err(), "fsync fails while its fault lasts");
        clock.advance(Duration::from_secs(10));
        d.fsync("/a")?;
        d.set_capacity(Some(4));
        assert!(d.write("/big", b"12345").is_err_and(|e| e.kind() == io::ErrorKind::StorageFull));
        assert_eq!(d.counts().full, 1);
        Ok(())
    }

    /// CP-T6.1: times as fine as the seed's filesystem keeps them: two writes within one step
    /// leave the time as it was.
    #[test]
    fn modification_times_are_as_coarse_as_the_filesystem() -> io::Result<()> {
        let (clock, d) = disk(4, DiskFaults { mtime_step: Duration::from_secs(1), ..DiskFaults::none() });
        clock.advance(Duration::from_millis(1200));
        d.write("/f", b"one")?;
        let t1 = d.meta("/f")?.mtime_ns;
        clock.advance(Duration::from_millis(500));
        d.write("/f", b"two")?;
        assert_eq!(d.meta("/f")?.mtime_ns, t1, "the same second");
        clock.advance(Duration::from_millis(500));
        d.write("/f", b"six")?;
        assert!(d.meta("/f")?.mtime_ns > t1);
        Ok(())
    }
}
