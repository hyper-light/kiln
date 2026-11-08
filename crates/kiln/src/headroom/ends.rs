//! Ends, as the kernel tells them (LB05, G11): a process's end, and a folder's entries changing, waited
//! on instead of looked for. A pacer's guard waits on its pacer's end this way (it looked every 2 s), and
//! a pacer on the ends of the pacers and guards its folder's records name (it looked once a period).
//!
//! Linux: a pidfd turns readable once its process has ended (Linux 5.3, `pidfd_open(2)`), and inotify
//! says when a folder's entries change. macOS: kqueue's `EVFILT_PROC` with `NOTE_EXIT`, and
//! `EVFILT_VNODE` with `NOTE_WRITE` on the folder (an entry added or removed). Windows: no wait on a
//! process that isn't a child, without `unsafe`, so none: a guard's input is the pacer's alone there
//! (the standard library makes a parent's end of a child's pipe uninheritable), and it closes with the
//! pacer; a record left behind is released when the next pacer starts.

use std::path::Path;

/// Waits until process `pid` ends: true once it has (or is gone already); false where this
/// platform or kernel can't say, and the caller has only its other word. `alive` says whether `pid`
/// is still the process meant (an id may be another's by now): asked once the wait is armed, so
/// one that ended before it was armed isn't waited for.
pub fn await_end(pid: u32, alive: impl Fn() -> bool) -> bool {
    imp::await_end(pid, alive)
}

/// What a [`Watch`] watches, so it watches each once: processes by id.
pub struct Watch(imp::Watch);

impl Watch {
    /// Watches `dir`'s entries, until `stop`'s other end closes.
    pub fn new(dir: &Path, stop: std::io::PipeReader) -> std::io::Result<Self> {
        imp::Watch::new(dir, stop).map(Self)
    }

    /// Arms process `pid`'s end, if it isn't armed already.
    pub fn process(&mut self, pid: u32) {
        self.0.process(pid);
    }

    /// Lets go of every armed process not in `pids`.
    pub fn keep(&mut self, pids: &std::collections::HashSet<u32>) {
        self.0.keep(pids);
    }

    /// Waits until `dir`'s entries change or an armed process ends (true), or `stop`'s other end
    /// closes or the wait fails (false).
    pub fn wait(&mut self) -> bool {
        self.0.wait()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::io::Read as _;
    use std::os::fd::OwnedFd;
    use std::path::Path;

    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::fs::inotify;

    /// Blocks until `fd` is readable (a pidfd: until its process has ended).
    fn readable(fd: &OwnedFd) -> bool {
        loop {
            let mut fds = [PollFd::new(fd, PollFlags::IN)];
            match poll(&mut fds, None) {
                Ok(_) => return true,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return false,
            }
        }
    }

    pub fn await_end(pid: u32, alive: impl Fn() -> bool) -> bool {
        match crate::os::proc::pidfd(pid) {
            // Armed: one that isn't the process meant any more ended already.
            Some(fd) => !alive() || readable(&fd),
            // Gone already, or a kernel without pidfds (it can't say).
            None => !alive(),
        }
    }

    pub struct Watch {
        notes: std::fs::File,
        stop: std::io::PipeReader,
        pids: HashMap<u32, OwnedFd>,
    }

    impl Watch {
        pub fn new(dir: &Path, stop: std::io::PipeReader) -> std::io::Result<Self> {
            let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
            // A record is written whole and renamed into place (`util::write_secret_unsynced`); one
            // written in place says so as it's closed.
            inotify::add_watch(&fd, dir, inotify::WatchFlags::CREATE | inotify::WatchFlags::MOVED_TO | inotify::WatchFlags::CLOSE_WRITE)?;
            Ok(Self { notes: std::fs::File::from(fd), stop, pids: HashMap::new() })
        }

        pub fn process(&mut self, pid: u32) {
            if !self.pids.contains_key(&pid)
                && let Some(fd) = crate::os::proc::pidfd(pid)
            {
                self.pids.insert(pid, fd);
            }
        }

        pub fn keep(&mut self, pids: &HashSet<u32>) {
            self.pids.retain(|p, _| pids.contains(p));
        }

        pub fn wait(&mut self) -> bool {
            let (stopped, noted, ended) = loop {
                let mut fds: Vec<PollFd<'_>> = Vec::with_capacity(self.pids.len() + 2);
                fds.push(PollFd::new(&self.stop, PollFlags::IN));
                fds.push(PollFd::new(&self.notes, PollFlags::IN));
                let order: Vec<u32> = self.pids.keys().copied().collect();
                fds.extend(order.iter().filter_map(|p| self.pids.get(p)).map(|fd| PollFd::new(fd, PollFlags::IN)));
                match poll(&mut fds, None) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => return false,
                }
                let mut it = fds.iter().map(|f| !f.revents().is_empty());
                let stopped = it.next().unwrap_or(true);
                let noted = it.next().unwrap_or(false);
                let ended: Vec<u32> = order.iter().zip(it).filter(|(_, r)| *r).map(|(p, _)| *p).collect();
                break (stopped, noted, ended);
            };
            if stopped {
                return false;
            }
            if noted {
                // What it said is read and let go: the folder is read again whatever it said.
                let mut buf = [0u8; 4096];
                while matches!(self.notes.read(&mut buf), Ok(n) if n > 0) {}
            }
            for p in ended {
                self.pids.remove(&p);
            }
            true
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashSet;
    use std::os::fd::AsRawFd as _;
    use std::path::Path;

    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    fn exit_of(pid: u32) -> KEvent {
        KEvent::new(pid as usize, EventFilter::EVFILT_PROC, EvFlags::EV_ADD | EvFlags::EV_ONESHOT, FilterFlag::NOTE_EXIT, 0, 0)
    }

    pub fn await_end(pid: u32, alive: impl Fn() -> bool) -> bool {
        let Ok(kq) = Kqueue::new() else { return !alive() };
        match kq.kevent(&[exit_of(pid)], &mut [], None) {
            Ok(_) => {}
            // Gone already (ESRCH), or it can't say.
            Err(_) => return !alive(),
        }
        if !alive() {
            return true;
        }
        let mut got = [KEvent::new(0, EventFilter::EVFILT_PROC, EvFlags::empty(), FilterFlag::empty(), 0, 0)];
        loop {
            match kq.kevent(&[], &mut got, None) {
                Ok(n) if n > 0 => return true,
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(_) => return false,
            }
        }
    }

    pub struct Watch {
        kq: Kqueue,
        /// Kept open while watched: the descriptors the kernel's filters name.
        _dir: std::fs::File,
        stop: std::io::PipeReader,
        pids: HashSet<u32>,
    }

    impl Watch {
        pub fn new(dir: &Path, stop: std::io::PipeReader) -> std::io::Result<Self> {
            let kq = Kqueue::new()?;
            let d = std::fs::File::open(dir)?;
            // An entry added or removed is a write to the folder; a record is written whole and
            // renamed into place (`util::write_secret_unsynced`).
            let folder = KEvent::new(d.as_raw_fd() as usize, EventFilter::EVFILT_VNODE, EvFlags::EV_ADD | EvFlags::EV_CLEAR, FilterFlag::NOTE_WRITE, 0, 0);
            let closed = KEvent::new(stop.as_raw_fd() as usize, EventFilter::EVFILT_READ, EvFlags::EV_ADD, FilterFlag::empty(), 0, 0);
            kq.kevent(&[folder, closed], &mut [], None)?;
            Ok(Self { kq, _dir: d, stop, pids: HashSet::new() })
        }

        pub fn process(&mut self, pid: u32) {
            if !self.pids.contains(&pid) && self.kq.kevent(&[exit_of(pid)], &mut [], None).is_ok() {
                self.pids.insert(pid);
            }
        }

        pub fn keep(&mut self, pids: &HashSet<u32>) {
            // A filter let go of ends with its process, or with this watch: only the set is kept.
            self.pids.retain(|p| pids.contains(p));
        }

        pub fn wait(&mut self) -> bool {
            let none = KEvent::new(0, EventFilter::EVFILT_READ, EvFlags::empty(), FilterFlag::empty(), 0, 0);
            let mut got = [none; 16];
            let n = loop {
                match self.kq.kevent(&[], &mut got, None) {
                    Ok(n) => break n,
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => return false,
                }
            };
            let stop = self.stop.as_raw_fd() as usize;
            for ev in got.iter().take(n) {
                match ev.filter() {
                    Ok(EventFilter::EVFILT_READ) if ev.ident() == stop => return false,
                    Ok(EventFilter::EVFILT_PROC) => {
                        self.pids.remove(&(ev.ident() as u32));
                    }
                    _ => {}
                }
            }
            true
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::collections::HashSet;
    use std::path::Path;

    pub fn await_end(_pid: u32, alive: impl Fn() -> bool) -> bool {
        !alive()
    }

    pub struct Watch;

    impl Watch {
        pub fn new(_dir: &Path, _stop: std::io::PipeReader) -> std::io::Result<Self> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }

        pub fn process(&mut self, _pid: u32) {}

        pub fn keep(&mut self, _pids: &HashSet<u32>) {}

        pub fn wait(&mut self) -> bool {
            false
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn lb05_g11_a_process_end_is_told_at_once() -> anyhow::Result<()> {
        let mut c = Command::new("sleep").arg("60").spawn()?;
        let pid = c.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(super::await_end(pid, || true));
        });
        // Still running: nothing's told.
        anyhow::ensure!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "told of an end before it came");
        let at = Instant::now();
        c.kill()?;
        c.wait()?;
        let told = rx.recv_timeout(Duration::from_secs(10))?;
        anyhow::ensure!(told, "its end wasn't told");
        // Told as it happened, not at a look: well within the 2 s the guard once looked every.
        anyhow::ensure!(at.elapsed() < Duration::from_secs(1), "told {:?} after it ended", at.elapsed());
        // One gone already is told at once.
        anyhow::ensure!(super::await_end(pid, || false), "a process gone already wasn't told ended");
        Ok(())
    }

    #[test]
    fn lb05_g11_a_watch_wakes_for_an_entry_a_process_end_and_its_stop() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (stop_rx, stop) = std::io::pipe()?;
        let mut w = super::Watch::new(dir.path(), stop_rx)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut c = Command::new("sleep").arg("60").spawn()?;
        w.process(c.id());
        std::thread::spawn(move || {
            while w.wait() {
                if tx.send(true).is_err() {
                    return;
                }
            }
            let _ = tx.send(false);
        });
        // What one change wakes it for, and whatever else that change made (a record's temporary
        // file and its rename are two of the folder's entries changing).
        let woke = |what: &str| -> anyhow::Result<()> {
            anyhow::ensure!(rx.recv_timeout(Duration::from_secs(10))?, "{what} ended it");
            while let Ok(more) = rx.recv_timeout(Duration::from_millis(200)) {
                anyhow::ensure!(more, "{what} ended it");
            }
            Ok(())
        };
        anyhow::ensure!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "woke with nothing to tell");
        crate::util::write_secret_unsynced(&dir.path().join("a.json"), b"{}", 0o644)?;
        woke("a new entry")?;
        c.kill()?;
        c.wait()?;
        woke("a process's end")?;
        drop(stop);
        anyhow::ensure!(!rx.recv_timeout(Duration::from_secs(10))?, "its stop didn't end it");
        Ok(())
    }
}
