//! Folder changes on Linux (goal 9): one inotify instance for the process, read by one thread
//! (`kiln-watch`) blocked in read(2) until the kernel has events, never woken otherwise. Each
//! folder a [`Watched`] reaches has a watch; one made later under it is watched as its creation
//! is read; one not there yet is watched from its nearest folder that is, for the name on the way
//! to it. A watch the kernel let go of (its folder deleted) is watched for again the same way. An
//! overflowed queue tells every watch: what was lost, each waiter reads again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use parking_lot::Mutex;

use super::{Depth, Watched};

/// What a folder's watch is for, for one of a [`Watched`]'s folders (`root`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    /// It is `n` folders below the root (0: the root), at `dir` as the root's path names it.
    Under { n: u32, dir: PathBuf },
    /// It is the root's nearest folder that's there, at `dir`, while the root isn't.
    Toward { dir: PathBuf },
}

/// One folder of one [`Watched`].
struct Root {
    watched: Weak<Watched>,
    id: u64,
    root: PathBuf,
    depth: Depth,
}

#[derive(Default)]
struct State {
    roots: HashMap<u64, Root>,
    next: u64,
    /// Each watch, and whose roots' folders it watches.
    wds: HashMap<WatchDescriptor, Vec<(u64, Role)>>,
}

struct Shared {
    fd: Inotify,
    state: Mutex<State>,
}

/// What every change is watched for: entries made, gone, moved, written or changed, and the
/// folder itself gone or moved.
fn mask() -> AddWatchFlags {
    AddWatchFlags::IN_CREATE
        | AddWatchFlags::IN_DELETE
        | AddWatchFlags::IN_MOVED_FROM
        | AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_CLOSE_WRITE
        | AddWatchFlags::IN_MODIFY
        | AddWatchFlags::IN_ATTRIB
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF
        | AddWatchFlags::IN_ONLYDIR
}

fn shared() -> Option<&'static Shared> {
    static S: OnceLock<Option<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        let fd = match Inotify::init(InitFlags::IN_CLOEXEC) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!(error = %e, "no inotify: folders' changes aren't heard, and what waits on them waits on its other events");
                return None;
            }
        };
        Some(Shared { fd, state: Mutex::new(State::default()) })
    })
    .as_ref()
    .inspect(|s| s.run_once())
}

pub(super) fn start(w: &Arc<Watched>) {
    let Some(s) = shared() else { return };
    let mut st = s.state.lock();
    for root in w.dirs() {
        let key = st.next;
        st.next = st.next.saturating_add(1);
        st.roots.insert(key, Root { watched: Arc::downgrade(w), id: w.id, root: root.clone(), depth: w.depth() });
        s.arm(&mut st, key);
    }
}

pub(super) fn forget(id: u64) {
    let Some(s) = shared() else { return };
    let mut st = s.state.lock();
    let keys: Vec<u64> = st.roots.iter().filter(|(_, r)| r.id == id).map(|(k, _)| *k).collect();
    for k in &keys {
        st.roots.remove(k);
    }
    s.drop_roles(&mut st, |key, _| keys.contains(&key));
}

impl Shared {
    fn run_once(&'static self) {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            if let Err(e) = std::thread::Builder::new().name("kiln-watch".into()).spawn(move || self.run()) {
                tracing::warn!(error = %e, "folders' changes aren't read: what waits on them waits on its other events");
            }
        });
    }

    fn run(&self) {
        loop {
            match self.fd.read_events() {
                Ok(events) => self.handle(events),
                Err(nix::errno::Errno::EINTR) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "folders' changes stopped being read: what waits on them waits on its other events");
                    return;
                }
            }
        }
    }

    /// Watches root `key` again: its folder, and as far below as it reaches, when it's there;
    /// else its nearest folder that is.
    fn arm(&self, st: &mut State, key: u64) {
        let Some(r) = st.roots.get(&key) else { return };
        let (root, depth) = (r.root.clone(), r.depth);
        self.drop_roles(st, |k, _| k == key);
        match super::nearest(&root) {
            Some((at, 0)) => self.tree(st, key, &at, 0, depth),
            Some((at, _)) => {
                self.add(st, key, &at, Role::Toward { dir: at.clone() });
            }
            None => {}
        }
    }

    /// Watches `dir`, `n` below root `key`, and the folders below it as far as `depth` reaches.
    fn tree(&self, st: &mut State, key: u64, dir: &Path, n: u32, depth: Depth) {
        if !self.add(st, key, dir, Role::Under { n, dir: dir.to_path_buf() }) || !depth.reaches(n.saturating_add(1)) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                self.tree(st, key, &e.path(), n.saturating_add(1), depth);
            }
        }
    }

    /// A watch on `dir` for `role` of root `key`; whether it took.
    fn add(&self, st: &mut State, key: u64, dir: &Path, role: Role) -> bool {
        match self.fd.add_watch(dir, mask()) {
            Ok(wd) => {
                let roles = st.wds.entry(wd).or_default();
                if !roles.iter().any(|(k, r)| *k == key && *r == role) {
                    roles.push((key, role));
                }
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, dir = %dir.display(), "a folder couldn't be watched");
                false
            }
        }
    }

    /// Takes the roles `gone` names off every watch, and lets go of the watches left with none.
    fn drop_roles(&self, st: &mut State, gone: impl Fn(u64, &Role) -> bool) {
        let mut empty = Vec::new();
        for (wd, roles) in &mut st.wds {
            roles.retain(|(k, r)| !gone(*k, r));
            if roles.is_empty() {
                empty.push(*wd);
            }
        }
        for wd in empty {
            st.wds.remove(&wd);
            let _ = self.fd.rm_watch(wd);
        }
    }

    fn handle(&self, events: Vec<nix::sys::inotify::InotifyEvent>) {
        // Those to tell, and every other watch upgraded meanwhile: none is let go while the lock is
        // held (a watch's drop takes it, in `forget`).
        let (mut tell, mut alive): (Vec<Arc<Watched>>, Vec<Arc<Watched>>) = (Vec::new(), Vec::new());
        let mut st = self.state.lock();
        for ev in events {
            if ev.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                tell.extend(st.roots.values().filter_map(|r| r.watched.upgrade()));
                continue;
            }
            let Some(roles) = st.wds.get(&ev.wd).cloned() else { continue };
            let gone = ev.mask.intersects(AddWatchFlags::IN_IGNORED | AddWatchFlags::IN_DELETE_SELF | AddWatchFlags::IN_MOVE_SELF);
            let made_dir = ev.mask.contains(AddWatchFlags::IN_ISDIR) && ev.mask.intersects(AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO);
            let mut rearm: Vec<u64> = Vec::new();
            for (key, role) in roles {
                let Some(r) = st.roots.get(&key) else { continue };
                let Some(w) = r.watched.upgrade() else { continue };
                let (root, depth) = (r.root.clone(), r.depth);
                let tells = match role {
                    Role::Under { n, dir } => match &ev.name {
                        Some(name) => {
                            let path = dir.join(name);
                            if made_dir && depth.reaches(n.saturating_add(1)) {
                                self.tree(&mut st, key, &path, n.saturating_add(1), depth);
                            }
                            w.covers(&path)
                        }
                        // The folder itself: the root changed (or went: watched for again).
                        None => {
                            if gone && n == 0 {
                                rearm.push(key);
                            }
                            n == 0
                        }
                    },
                    // The root's way made (or its nearest folder gone): watched again from there.
                    Role::Toward { dir } => {
                        let next = ev.name.as_ref().map(|name| dir.join(name));
                        let moved = gone || next.as_ref().is_some_and(|p| root.starts_with(p));
                        if moved {
                            rearm.push(key);
                        }
                        moved
                    }
                };
                if tells { tell.push(w) } else { alive.push(w) }
            }
            if ev.mask.contains(AddWatchFlags::IN_IGNORED) {
                st.wds.remove(&ev.wd);
            }
            for key in rearm {
                self.arm(&mut st, key);
            }
        }
        drop(st);
        let mut told: Vec<Arc<Watched>> = Vec::new();
        for w in tell {
            if !told.iter().any(|t| Arc::ptr_eq(t, &w)) {
                w.tell();
                told.push(w);
            }
        }
        drop(alive);
    }
}

/// Until one of `pids` exits (goal 9): each one's pidfd readable, as hyper-rt's readiness says; one
/// already gone at once; one that can't be watched (a kernel before pidfds) is left out, and with
/// none to watch, never.
pub(super) async fn exited(pids: &[u32]) {
    use nix::poll::{PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd as _;
    let mut fds = Vec::new();
    for pid in pids {
        let Some(p) = i32::try_from(*pid).ok().and_then(rustix::process::Pid::from_raw) else { continue };
        match rustix::process::pidfd_open(p, rustix::process::PidfdFlags::empty()) {
            Ok(fd) => fds.push(fd),
            Err(rustix::io::Errno::SRCH) => return,
            Err(_) => {}
        }
    }
    if fds.is_empty() {
        return std::future::pending().await;
    }
    loop {
        let waits: Vec<_> = fds.iter().map(|fd| Box::pin(crate::os::proc::wait_readable(fd))).collect();
        futures::future::select_all(waits).await;
        // Readiness said so; the pidfd says whether it's so (a refused wait returns early).
        let mut polled: Vec<PollFd<'_>> = fds.iter().map(|fd| PollFd::new(fd.as_fd(), PollFlags::POLLIN)).collect();
        if nix::poll::poll(&mut polled, PollTimeout::ZERO).is_ok_and(|n| n > 0) {
            return;
        }
    }
}
