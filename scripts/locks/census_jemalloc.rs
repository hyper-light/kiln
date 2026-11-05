//! The allocator's locks in the lock census (goal 5 of the API runtime program, plan 01a1a59c2aae): never
//! part of kiln's own build. `scripts/locks.py census-tree` copies this file into the census tree as kiln's
//! `census_jemalloc` module, turns on jemalloc's statistics there (tikv-jemalloc-sys's `stats` feature, which
//! builds jemalloc with `--enable-stats`), and has `/metrics` render it beside the census of kiln's locks.
//!
//! Where that build has kiln's `jemalloc` feature (Linux), [`render`] says each of jemalloc's mutexes as
//! `kiln_lock_*` series of a kind of their own (`jemalloc`), from jemalloc's own counts (mallctl(3), "stats.mutexes"
//! and "stats.arenas.<i>.mutexes"): its global mutexes, each arena's, and each arena's bins' (the locks a thread's
//! cache takes as it fills or flushes, jemalloc's slow path) summed. A site is `jemalloc:global.<mutex>`,
//! `jemalloc:arena.<i>.<mutex>` or `jemalloc:arena.<i>.bins`:
//!
//! * `kiln_lock_taken_total` is the mutex's `num_ops`, its acquisitions;
//! * `kiln_lock_contended_total` is `num_wait` and `num_spin_acq` together: the acquisitions that found it held
//!   (jemalloc's first try failed), whether they then had it by spinning (up to `opt.mutex_max_spin`, 600 tries)
//!   or blocked for it;
//! * `kiln_lock_wait_seconds_total` is `total_wait_time`: how long those that blocked waited (jemalloc times
//!   no spin);
//! * `kiln_lock_wait_max_seconds` is `max_wait_time`, the longest any one waited.
//!
//! jemalloc keeps these per mutex as it's taken (`malloc_mutex_lock`), so reading them takes nothing on any
//! allocation's path: a scrape refreshes jemalloc's copy of them (writing `epoch`, which takes each arena's
//! mutexes once to read theirs) and reads the copy. A mutex never taken says nothing, as a site never taken
//! doesn't in kiln's census. Without the feature, or elsewhere, it renders nothing: musl's and glibc's mallocs
//! keep no count of their locks (docs/runtime-lab.md, goal 5).

/// jemalloc's mutexes' counts as `kiln_lock_*` series of kind `jemalloc`.
pub fn render(o: &mut String) {
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    imp::render(o);
    #[cfg(not(all(feature = "jemalloc", target_os = "linux")))]
    let _ = o;
}

#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
mod imp {
    use std::ffi::{CStr, CString};
    use std::fmt::Write as _;

    /// jemalloc 5.3's global mutexes (`stats.mutexes.<name>`): its `MUTEX_PROF_GLOBAL_MUTEXES`
    /// (include/jemalloc/internal/mutex_prof.h).
    const GLOBAL: [&str; 9] = ["background_thread", "max_per_bg_thd", "ctl", "prof", "prof_thds_data", "prof_dump", "prof_recent_alloc", "prof_recent_dump", "prof_stats"];

    /// Each arena's (`stats.arenas.<i>.mutexes.<name>`): its `MUTEX_PROF_ARENA_MUTEXES`.
    const ARENA: [&str; 12] = ["large", "extent_avail", "extents_dirty", "extents_muzzy", "extents_retained", "decay_dirty", "decay_muzzy", "base", "tcache_list", "hpa_shard", "hpa_shard_grow", "hpa_sec"];

    /// One of jemalloc's values (mallctl(3)) read into `value`, a value of the name's type in jemalloc's manual
    /// (`bool`, `unsigned`, `uint64_t`), as `os::alloc` reads its settings.
    fn read<T: Copy>(name: &CStr, mut value: T) -> Option<T> {
        let mut len = std::mem::size_of::<T>();
        // SAFETY: `name` ends in a NUL; jemalloc writes at most `len` bytes to `value`, both this frame's, and says
        // in `len` how many it wrote; it reads nothing of ours (no new value).
        let r = unsafe { tikv_jemalloc_sys::mallctl(name.as_ptr(), (&raw mut value).cast(), &raw mut len, std::ptr::null_mut(), 0) };
        (r == 0 && len == std::mem::size_of::<T>()).then_some(value)
    }

    fn read_named<T: Copy>(name: &str, value: T) -> Option<T> {
        read(CString::new(name).ok()?.as_c_str(), value)
    }

    /// Refreshes the copy of jemalloc's statistics that mallctl reads (`epoch`): whether jemalloc did.
    fn refresh() -> bool {
        let (mut epoch, mut now) = (1_u64, 0_u64);
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: "epoch" ends in a NUL; jemalloc reads `size_of::<u64>()` bytes from `epoch` (writing it refreshes
        // the statistics) and writes at most `len` bytes, the epoch it's at, to `now`: all three this frame's.
        let r = unsafe { tikv_jemalloc_sys::mallctl(c"epoch".as_ptr(), (&raw mut now).cast(), &raw mut len, (&raw mut epoch).cast(), std::mem::size_of::<u64>()) };
        r == 0
    }

    /// A mutex's counts (jemalloc's `mutex_prof_data_t`, as mallctl says them).
    #[derive(Default)]
    struct Counts {
        ops: u64,
        waits: u64,
        spins: u64,
        wait_ns: u64,
        max_wait_ns: u64,
    }

    impl Counts {
        /// The counts under `prefix` (`stats.mutexes.ctl`, `stats.arenas.0.mutexes.large`, ...).
        fn of(prefix: &str) -> Option<Self> {
            Some(Self {
                ops: read_named(&format!("{prefix}.num_ops"), 0_u64)?,
                waits: read_named(&format!("{prefix}.num_wait"), 0_u64)?,
                spins: read_named(&format!("{prefix}.num_spin_acq"), 0_u64)?,
                wait_ns: read_named(&format!("{prefix}.total_wait_time"), 0_u64)?,
                max_wait_ns: read_named(&format!("{prefix}.max_wait_time"), 0_u64)?,
            })
        }

        fn add(&mut self, other: &Self) {
            self.ops = self.ops.saturating_add(other.ops);
            self.waits = self.waits.saturating_add(other.waits);
            self.spins = self.spins.saturating_add(other.spins);
            self.wait_ns = self.wait_ns.saturating_add(other.wait_ns);
            self.max_wait_ns = self.max_wait_ns.max(other.max_wait_ns);
        }

        /// The site's series, as the census's own (`census::render`) says a site's.
        #[allow(clippy::cast_precision_loss)]
        fn say(&self, o: &mut String, site: &str) {
            if self.ops == 0 {
                return;
            }
            let labels = format!("site=\"jemalloc:{site}\",kind=\"jemalloc\"");
            let _ = writeln!(o, "kiln_lock_taken_total{{{labels}}} {}", self.ops);
            let _ = writeln!(o, "kiln_lock_contended_total{{{labels}}} {}", self.waits.saturating_add(self.spins));
            let _ = writeln!(o, "kiln_lock_wait_seconds_total{{{labels}}} {}", self.wait_ns as f64 / 1e9);
            let _ = writeln!(o, "kiln_lock_wait_max_seconds{{{labels}}} {}", self.max_wait_ns as f64 / 1e9);
        }
    }

    pub fn render(o: &mut String) {
        // Whether this jemalloc counts at all (`config.stats`: built with `--enable-stats`); one without answers
        // none of the names below, so this says why nothing follows.
        let stats = read(c"config.stats", false) == Some(true);
        let _ = writeln!(o, "kiln_lock_jemalloc_stats {}", u8::from(stats));
        if !stats || !refresh() {
            return;
        }
        for m in GLOBAL {
            if let Some(c) = Counts::of(&format!("stats.mutexes.{m}")) {
                c.say(o, &format!("global.{m}"));
            }
        }
        let arenas = read(c"arenas.narenas", 0_u32).unwrap_or(0);
        let bins = read(c"arenas.nbins", 0_u32).unwrap_or(0);
        for i in 0..arenas {
            if read_named(&format!("arena.{i}.initialized"), false) != Some(true) {
                continue;
            }
            for m in ARENA {
                if let Some(c) = Counts::of(&format!("stats.arenas.{i}.mutexes.{m}")) {
                    c.say(o, &format!("arena.{i}.{m}"));
                }
            }
            let mut all = Counts::default();
            for j in 0..bins {
                if let Some(c) = Counts::of(&format!("stats.arenas.{i}.bins.{j}.mutex")) {
                    all.add(&c);
                }
            }
            all.say(o, &format!("arena.{i}.bins"));
        }
    }
}
