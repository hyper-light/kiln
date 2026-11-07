//! Memory pressure as the kernel says it, and what the server does then (join-burst, item 3): jemalloc's dirty pages
//! given back at once ([`crate::os::alloc::purge`]), where jemalloc keeps them through its decay (5 s,
//! `crate::os::alloc::DIRTY_DECAY_MS`), which stays as the lab's A/B left it: decay off bought no latency and held 200 MB
//! more. Runners joining filled a constrained instance's heap faster than the decay gave it back (startup-footprint's
//! join: dirty pages up to 31 MB at the anon peak, and the OOM killer).
//!
//! - **What says it** (Linux): the `memory.events` of the cgroup that counts the process's memory, as its `high` or `max`
//!   count rises (a charge past `memory.high`, throttled and reclaimed; one past `memory.max`, reclaimed before the OOM
//!   killer is asked): its every change "generates a file modified event" (the kernel's cgroup-v2.rst, "memory.events"),
//!   which inotify hands over. And PSI's memory pressure: that cgroup's `memory.pressure`, else the machine's
//!   `/proc/pressure/memory`, through a trigger on any stall, 1 µs of `some` (the least the kernel takes), in the shortest
//!   window it lets the process ask: 500 ms with `CAP_SYS_RESOURCE`, else 2 s (psi.rst, "Userspace monitors").
//! - **A thread of its own, blocked in poll(2)** on both: nothing runs, nothing is read and no timer fires while memory
//!   isn't short. A change of another of the file's counts is let be.
//! - Elsewhere (macOS, Windows: kiln allocates through the platform's allocator there, nothing of jemalloc's to give
//!   back), and where neither file is there: no thread.

use std::sync::atomic::{AtomicU64, Ordering};

/// What said memory is short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The cgroup's `memory.events`: its `high` or `max` count rose.
    Events,
    /// PSI's trigger: a task stalled on memory within its window.
    Stall,
}

impl Source {
    /// Its name on `/metrics` (`kiln_memory_pressure_total{source}`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Events => "memory_events",
            Self::Stall => "psi",
        }
    }
}

/// Each source's word that memory was short, so far: `/metrics` says them ([`heard`]).
static HEARD: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// How many times each source has said memory was short since the server started.
pub fn heard() -> [(Source, u64); 2] {
    [Source::Events, Source::Stall].map(|s| (s, HEARD.get(s as usize).map_or(0, |n| n.load(Ordering::Relaxed))))
}

/// What the server does as memory is short: counted, and jemalloc's dirty pages given back.
pub fn relieve(source: Source) {
    if let Some(n) = HEARD.get(source as usize) {
        n.fetch_add(1, Ordering::Relaxed);
    }
    crate::os::alloc::purge();
}

/// The server's watch on memory pressure ([`relieve`] at each word of it), on a thread of its own, where the kernel says
/// it (Linux, a cgroup's `memory.events` or PSI): one a process, however many servers it runs. False where nothing can be
/// watched.
pub fn start() -> bool {
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STARTED.get_or_init(|| imp::start(relieve))
}

/// The `high` and `max` counts of a cgroup's `memory.events`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub high: u64,
    pub max: u64,
}

impl Counts {
    /// What `memory.events`' text says (a count it doesn't say is none).
    pub fn of(text: &str) -> Self {
        let n = |key| crate::os::cgroup::keyed(text, key).unwrap_or(0);
        Self { high: n("high"), max: n("max") }
    }

    /// Whether `now` says memory was short since these: its `high` or its `max` count rose.
    pub fn rose(self, now: Self) -> bool {
        now.high > self.high || now.max > self.max
    }
}

/// PSI's trigger for any stall on memory (`some`, its least threshold, 1 µs) within the shortest window the kernel lets a
/// process of these capabilities ask: 500 ms with `CAP_SYS_RESOURCE`, else a multiple of 2 s (psi.rst; Linux 6.5's
/// "sched/psi: Allow unprivileged polling of N*2s period"). `status` is `/proc/self/status`'s text, whose `CapEff` says.
pub fn stall_trigger(status: &str) -> &'static [u8] {
    /// `CAP_SYS_RESOURCE`'s bit (linux/capability.h).
    const CAP_SYS_RESOURCE: u32 = 24;
    let effective = status.lines().find_map(|l| l.strip_prefix("CapEff:")).and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok()).unwrap_or(0);
    if (effective >> CAP_SYS_RESOURCE) & 1 == 1 { b"some 1 500000\0".as_slice() } else { b"some 1 2000000\0".as_slice() }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};

    use nix::poll::{PollFd, PollFlags, PollTimeout};
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

    use super::{Counts, Source};

    /// The files that say the process's memory is short: its memory-counting cgroup's `memory.events` (none outside a
    /// cgroup v2 that counts memory), and its `memory.pressure`, else the machine's PSI file.
    pub(super) fn sources() -> (Option<PathBuf>, Option<PathBuf>) {
        let dir = crate::os::cgroup::memory_dir();
        let events = dir.as_ref().map(|d| d.join("memory.events")).filter(|p| p.exists());
        let stall = dir.map(|d| d.join("memory.pressure")).filter(|p| p.exists()).or_else(|| Some(PathBuf::from("/proc/pressure/memory")).filter(|p| p.exists()));
        (events, stall)
    }

    pub(super) fn start(act: fn(Source)) -> bool {
        let (events, stall) = sources();
        let watch = match Watch::open(events.as_deref(), stall.as_deref()) {
            Ok(w) => w,
            Err(e) => {
                tracing::info!(error = %e, "no memory pressure to watch: jemalloc's dirty pages go by its decay alone");
                return false;
            }
        };
        tracing::info!(events = ?events, psi = ?stall, "memory pressure watched: jemalloc's dirty pages given back as it rises");
        std::thread::Builder::new().name("kiln-pressure".into()).spawn(move || watch.run(act)).is_ok()
    }

    /// A cgroup's `memory.events`, heard through inotify, and the counts it said last.
    struct Events {
        path: PathBuf,
        inotify: Inotify,
        last: Counts,
        /// Its text as last read, the room kept for the next.
        text: String,
    }

    impl Events {
        fn open(path: &Path) -> std::io::Result<Self> {
            let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
            inotify.add_watch(path, AddWatchFlags::IN_MODIFY)?;
            let mut me = Self { path: path.to_path_buf(), inotify, last: Counts::default(), text: String::new() };
            me.last = me.read()?;
            Ok(me)
        }

        fn read(&mut self) -> std::io::Result<Counts> {
            self.text.clear();
            std::fs::File::open(&self.path)?.read_to_string(&mut self.text)?;
            Ok(Counts::of(&self.text))
        }

        /// What inotify said, taken, then the file read again: whether its `high` or `max` count rose.
        fn heard(&mut self) -> std::io::Result<bool> {
            let mut drain = [0u8; 1024];
            while nix::unistd::read(self.inotify.as_fd(), &mut drain).is_ok_and(|n| n > 0) {}
            let now = self.read()?;
            let rose = self.last.rose(now);
            self.last = now;
            Ok(rose)
        }
    }

    /// The pressure files a watch is on: either may be missing, not both.
    pub(super) struct Watch {
        events: Option<Events>,
        stall: Option<std::fs::File>,
    }

    impl Watch {
        pub(super) fn open(events: Option<&Path>, stall: Option<&Path>) -> std::io::Result<Self> {
            let events = events.map(Events::open).transpose().map_err(|e| std::io::Error::new(e.kind(), format!("memory.events: {e}")));
            let stall = stall.map(|p| -> std::io::Result<std::fs::File> {
                let mut f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(nix::libc::O_NONBLOCK).open(p)?;
                let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
                f.write_all(super::stall_trigger(&status))?;
                Ok(f)
            });
            // Each source on its own: a refused one (no PSI in the kernel, a trigger it won't take) leaves the other.
            let (events, stall) = match (events, stall.transpose()) {
                (Ok(e), Ok(s)) => (e, s),
                (Ok(e), Err(err)) if e.is_some() => {
                    tracing::info!(error = %err, "PSI's memory pressure isn't watched: memory.events alone");
                    (e, None)
                }
                (Err(err), Ok(s)) if s.is_some() => {
                    tracing::info!(error = %err, "memory.events isn't watched: PSI's memory pressure alone");
                    (None, s)
                }
                (Err(err), _) | (_, Err(err)) => return Err(err),
            };
            if events.is_none() && stall.is_none() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no cgroup memory.events and no PSI file"));
            }
            Ok(Self { events, stall })
        }

        /// Waits for either file's word, and says each to `act`, until both are gone (a cgroup removed: POLLERR).
        pub(super) fn run(mut self, act: fn(Source)) {
            while self.wait(act) {}
        }

        /// One wait for the files, and what it heard said to `act`: false once nothing's left to wait on.
        pub(super) fn wait(&mut self, act: fn(Source)) -> bool {
            let (events_fired, stall_fired) = match self.poll() {
                Ok(fired) => fired,
                Err(nix::errno::Errno::EINTR) => return true,
                Err(e) => {
                    tracing::warn!(error = %e, "memory pressure's watch ended");
                    return false;
                }
            };
            let gone = PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL;
            if let Some(f) = events_fired {
                if f.intersects(gone) {
                    self.events = None;
                } else if f.contains(PollFlags::POLLIN) {
                    match self.events.as_mut().map(Events::heard) {
                        Some(Ok(true)) => act(Source::Events),
                        Some(Ok(false)) | None => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "memory.events can't be read: no longer watched");
                            self.events = None;
                        }
                    }
                }
            }
            if let Some(f) = stall_fired {
                if f.intersects(gone) {
                    self.stall = None;
                } else if f.contains(PollFlags::POLLPRI) {
                    act(Source::Stall);
                }
            }
            self.events.is_some() || self.stall.is_some()
        }

        /// poll(2) on the files watched, until one says something: what each said (none for one not watched).
        fn poll(&self) -> nix::Result<(Option<PollFlags>, Option<PollFlags>)> {
            let events = self.events.as_ref().map(|e| PollFd::new(e.inotify.as_fd(), PollFlags::POLLIN));
            let stall = self.stall.as_ref().map(|f| PollFd::new(f.as_fd(), PollFlags::POLLPRI));
            match (events, stall) {
                (Some(e), Some(s)) => {
                    let mut both = [e, s];
                    nix::poll::poll(&mut both, PollTimeout::NONE)?;
                    let [e, s] = both;
                    Ok((e.revents(), s.revents()))
                }
                (Some(e), None) => {
                    let mut one = [e];
                    nix::poll::poll(&mut one, PollTimeout::NONE)?;
                    let [e] = one;
                    Ok((e.revents(), None))
                }
                (None, Some(s)) => {
                    let mut one = [s];
                    nix::poll::poll(&mut one, PollTimeout::NONE)?;
                    let [s] = one;
                    Ok((None, s.revents()))
                }
                (None, None) => Ok((None, None)),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Source;

    pub(super) fn start(_act: fn(Source)) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// join-burst, item 3: a rise of `memory.events`' `high` or `max` count is pressure; another count moving, or none, isn't.
    #[test]
    fn join_burst_pressure_is_a_rise_of_the_high_or_max_count() {
        let was = Counts::of("low 0\nhigh 3\nmax 7\noom 0\noom_kill 0\noom_group_kill 0\n");
        assert_eq!(was, Counts { high: 3, max: 7 });
        assert!(was.rose(Counts::of("low 0\nhigh 4\nmax 7\noom 0\noom_kill 0\n")), "high rose");
        assert!(was.rose(Counts::of("low 0\nhigh 3\nmax 8\noom 0\noom_kill 0\n")), "max rose");
        assert!(!was.rose(Counts::of("low 9\nhigh 3\nmax 7\noom 1\noom_kill 1\n")), "neither rose");
        assert!(!was.rose(was), "nothing moved");
        assert_eq!(Counts::of(""), Counts::default());
    }

    /// join-burst, item 3: PSI's trigger is the least stall in the shortest window the kernel lets the process ask, as its
    /// capabilities say: 500 ms with `CAP_SYS_RESOURCE` (bit 24 of `CapEff`), 2 s without.
    #[test]
    fn join_burst_the_stall_trigger_is_the_kernels_least() {
        assert_eq!(stall_trigger("Name:\tkiln\nCapEff:\t0000000000000000\n"), b"some 1 2000000\0");
        assert_eq!(stall_trigger("CapEff:\t000001ffffffffff\n"), b"some 1 500000\0");
        assert_eq!(stall_trigger("CapEff:\t0000000001000000\n"), b"some 1 500000\0");
        assert_eq!(stall_trigger("CapEff:\t0000000000ffffff\n"), b"some 1 2000000\0");
        assert_eq!(stall_trigger(""), b"some 1 2000000\0");
    }

    /// join-burst, item 3: the watch hears `memory.events` change through inotify and says a rise of its `high` or `max`
    /// count, once a change, and nothing for a change of another count; while nothing changes it waits in poll(2),
    /// waking for nothing. A file in a folder of the test's own stands in for the cgroup's (inotify takes both alike),
    /// each change written in place in one call, as the kernel changes the cgroup's (a reader never finds it half made).
    #[cfg(target_os = "linux")]
    #[test]
    fn join_burst_the_watch_hears_memory_events_rise_and_nothing_else() -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt as _;
        use std::sync::atomic::AtomicU64;
        static SAID: AtomicU64 = AtomicU64::new(0);
        fn act(source: Source) {
            assert_eq!(source, Source::Events);
            SAID.fetch_add(1, Ordering::SeqCst);
        }
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("memory.events");
        std::fs::write(&path, "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n")?;
        let file = std::fs::OpenOptions::new().write(true).open(&path)?;
        let say = |text: &str| file.write_all_at(text.as_bytes(), 0);
        let mut watch = imp::Watch::open(Some(&path), None)?;
        say("low 0\nhigh 1\nmax 0\noom 0\noom_kill 0\n")?;
        anyhow::ensure!(watch.wait(act), "still watching");
        assert_eq!(SAID.load(Ordering::SeqCst), 1, "high rose: said once");
        say("low 5\nhigh 1\nmax 0\noom 0\noom_kill 0\n")?;
        anyhow::ensure!(watch.wait(act), "still watching");
        assert_eq!(SAID.load(Ordering::SeqCst), 1, "low moved: nothing said");
        say("low 5\nhigh 1\nmax 2\noom 0\noom_kill 0\n")?;
        anyhow::ensure!(watch.wait(act), "still watching");
        assert_eq!(SAID.load(Ordering::SeqCst), 2, "max rose: said");
        // Nothing changes: a wait on a thread of its own, as the watch's is, says nothing for a while, then hears the next
        // rise at once.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let more = watch.wait(act);
            let _ = tx.send(more);
        });
        anyhow::ensure!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "it woke with nothing changed");
        assert_eq!(SAID.load(Ordering::SeqCst), 2);
        say("low 5\nhigh 2\nmax 2\noom 0\noom_kill 0\n")?;
        anyhow::ensure!(rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or(false), "the rise woke it");
        assert_eq!(SAID.load(Ordering::SeqCst), 3);
        waiting.join().map_err(|_| anyhow::anyhow!("the waiting thread unwound"))?;
        Ok(())
    }

    /// join-burst, item 3: a purge gives jemalloc's dirty pages back at once, where its decay (5 s) would have kept them: a
    /// 64 MiB block written whole and freed leaves its pages dirty (resident) until then; after the purge, under half of
    /// it is dirty (other tests allocate beside this one), and the block's pages are faulted in again only if asked again.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    #[allow(unsafe_code)]
    fn join_burst_a_purge_gives_jemallocs_dirty_pages_back_at_once() {
        use std::alloc::{GlobalAlloc as _, Layout};
        const BLOCK: usize = 64 << 20;
        let Ok(layout) = Layout::from_size_align(BLOCK, 4096) else {
            return;
        };
        // SAFETY: a layout of non-zero size.
        let block = unsafe { tikv_jemallocator::Jemalloc.alloc(layout) };
        assert!(!block.is_null());
        // SAFETY: `block` is BLOCK bytes, jemalloc's, written whole here.
        unsafe { std::ptr::write_bytes(block, 1, BLOCK) };
        // SAFETY: `block` came from jemalloc with `layout`.
        unsafe { tikv_jemallocator::Jemalloc.dealloc(block, layout) };
        let freed = crate::os::alloc::stats().unwrap_or_default();
        let block_bytes = u64::try_from(BLOCK).unwrap_or(u64::MAX);
        assert!(freed.dirty >= block_bytes / 2, "the freed block's pages are dirty until its decay: {freed:?}");
        let faults = crate::os::page_faults().unwrap_or_default();
        relieve(Source::Events);
        let purged = crate::os::alloc::stats().unwrap_or_default();
        assert!(purged.dirty < block_bytes / 2, "purged at once: {freed:?} then {purged:?}");
        assert!(heard().iter().any(|(s, n)| *s == Source::Events && *n >= 1));
        // A purge faults nothing in: it gives pages back.
        let after = crate::os::page_faults().unwrap_or_default();
        assert!(after.major == faults.major, "{faults:?} then {after:?}");
    }
}
