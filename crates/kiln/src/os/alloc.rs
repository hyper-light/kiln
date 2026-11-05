//! The allocator and how it gives memory back (goal 11 of the API runtime program, plan 01a1a87b2e56 step 4;
//! docs/runtime-lab.md, "Goal 11 / The allocator (step 4)"). [`init`] is the first thing `main` does.
//!
//! * **Linux with kiln's `jemalloc` feature**: production's static musl build, as `ci/release/build.sh` builds
//!   it by default (`KILN_LINUX_ALLOCATOR` is `jemalloc` unless a build says `musl`), the lab's pick
//!   ([`JEMALLOC_CONF`]). jemalloc 5.3 is the binary's allocator (`main.rs`'s `#[global_allocator]`), with the
//!   settings below in its `malloc_conf`. jemalloc reads them at its first allocation, before `main`;
//!   `_RJEM_MALLOC_CONF` in the environment overrides them, a setting at a time (a lab's A/B of one build).
//! * **Linux on glibc** (a build from source for `*-linux-gnu`): glibc's malloc, its arenas capped at
//!   [`GLIBC_ARENAS`] unless the environment says how many. With the `jemalloc` feature too, that's the C
//!   libraries' malloc (SQLite's, aws-lc's).
//! * **Linux on musl without the feature** (a release build with `KILN_LINUX_ALLOCATOR=musl`), **macOS and
//!   Windows**: the platform's allocator, as it is.
//!
//! What jemalloc holds, live and kept, is on `/metrics` ([`stats`], step 5). A lab's build can carry jemalloc's
//! heap profile (the `jemalloc-prof` feature), off until a run's `_RJEM_MALLOC_CONF` says `prof:true`.

use std::sync::OnceLock;

/// The shards whose threads do most of the server's allocating (its requests, sessions' copies and feeds):
/// the lab's (`KILN_RT_SHARDS=4`), and the most [`crate::rt::Profile::server`] makes (a shard for every
/// 4 CPUs, 2 to 4). [`NARENAS`] and [`GLIBC_ARENAS`] are these and one more.
pub const SHARDS: u32 = 4;

/// jemalloc's background threads purge what decays on time (`background_thread`): without them jemalloc
/// purges an arena only as that arena allocates and frees (its decay is ticked by those), so what a server
/// holds after a burst stays while it's quiet; and a purge's `madvise` runs on jemalloc's own thread, not
/// inline on the thread that freed, a shard among them (jemalloc's TUNING.md: "unused memory purging is
/// shifted to the dedicated background threads"). At most 4 of them (`max_background_threads`' default).
pub const BACKGROUND_THREAD: bool = true;

/// jemalloc's arenas for threads to share (`narenas`): [`SHARDS`] and one more, the count the lab's A/B
/// held every latency number with on glibc's malloc (`MALLOC_ARENA_MAX=5`; 2 doubled the bus delivery's
/// p99). jemalloc's default, four for every CPU (32 on the lab's 8, 128 on a 32-CPU machine), keeps what
/// each arena's threads freed apart from every other's; and jemalloc takes an arena's locks less than
/// glibc does, only past a thread's own cache (up to 200 of a small size, against glibc's 7).
pub const NARENAS: u32 = SHARDS + 1;

/// How long dirty pages (freed, still resident) are kept before they're purged (`dirty_decay_ms`).
/// jemalloc keeps about as many as were freed in the last half of it (its decay curve is symmetric about
/// the middle), so 2.5 s of them: the lab's recurring work (an attach every 2.7 s across its 8 viewers, an
/// agent's turn about every 3 s) mostly finds what it freed last time, rather than faulting new pages in;
/// what's idle longer goes within the 5 s of the lab's warm-up after its fill. Half jemalloc's default
/// (10 s), as its TUNING.md suggests where memory comes first.
pub const DIRTY_DECAY_MS: u32 = 5_000;

/// How long purged pages the kernel may take lazily are kept (`muzzy_decay_ms`): none. A page given
/// back with `MADV_FREE` stays in the process's resident memory until the kernel wants memory
/// (madvise(2); smaps' `LazyFree`), so it isn't given back as resident memory counts (the lab's
/// `server.rss_mb`, the goal's bar, `kiln_memory_resident_bytes`); at 0, what decays is purged with
/// `MADV_DONTNEED` at once. jemalloc 5.3's code has 0 too (`MUZZY_DECAY_MS_DEFAULT`; its manual says
/// 10 s), set here so a jemalloc whose default moves doesn't move it.
pub const MUZZY_DECAY_MS: u32 = 0;

/// Transparent huge pages for jemalloc's memory (`thp`): never. The lab's machine has the kernel give
/// them only where asked (fedora: `madvise`), so the lab measures jemalloc without them; production's
/// host gives them to every anonymous mapping (adacachy: `always`), and its khugepaged collapses a
/// 2 MiB range back into one with 103 of its 512 pages in use (`max_ptes_none` 409), undoing a purge.
/// `never` has jemalloc ask for none (`MADV_NOHUGEPAGE`), so production's heap is the one the lab
/// measured; where they're given only when asked, it changes nothing.
pub const THP: &str = "never";

/// What jemalloc reads as its `malloc_conf`: the settings above, NUL-terminated, which the lab's A/B picked
/// (fedora, 2026-11-05 17:40 to 18:01 UTC, the realistic profile, calm, 8 rounds each; docs/runtime-lab.md,
/// "The allocator (step 4)"). On the static musl build, against musl's malloc: resident memory 523 to 333 MB
/// and anonymous 398 to 207 MB, both beyond noise, and none of the latency numbers worse beyond noise. The
/// lab gave them to a build that predates this string, by `_RJEM_MALLOC_CONF`. Unit tests hold the string to
/// what the lab ran and to the settings above. With the `jemalloc` feature, they also have jemalloc read
/// each back, and take a variant from the environment over them.
pub const JEMALLOC_CONF: &str = "background_thread:true,narenas:5,dirty_decay_ms:5000,muzzy_decay_ms:0,thp:never\0";

/// [`JEMALLOC_CONF`]'s settings with the heap profile's sampling held off (`prof_active:false`) until kiln's
/// own unwinder is in ([`init`] then turns it on): what a build with the `jemalloc-prof` feature has jemalloc
/// read (goal 11, plan 01a1a87b2e56 step 5). Otherwise a run with `prof:true` would sample from jemalloc's first
/// allocation, before `main`, through the backtrace its configure picked; where it found no libgcc to link,
/// that's gcc's frame-address walk, which follows frame pointers Rust's code doesn't keep. `prof_active` is
/// read only where profiling is built in. A unit test holds it to `JEMALLOC_CONF`.
pub const JEMALLOC_PROF_CONF: &str = "background_thread:true,narenas:5,dirty_decay_ms:5000,muzzy_decay_ms:0,thp:never,prof_active:false\0";

/// What jemalloc reads: [`JEMALLOC_PROF_CONF`] where its heap profile is built in, else [`JEMALLOC_CONF`].
pub const MALLOC_CONF_TEXT: &str = if cfg!(feature = "jemalloc-prof") { JEMALLOC_PROF_CONF } else { JEMALLOC_CONF };

/// The settings as jemalloc's `malloc_conf` (tikv-jemalloc-sys's prefixed name for it), read at its first
/// allocation; jemalloc's own is a weak `NULL` this replaces. `Option<&u8>` is a nullable pointer, as C's
/// `const char *` is.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
// SAFETY: jemalloc reads this symbol as a `const char *` (an `Option<&u8>` has a pointer's layout, `None`
// as NULL) to a NUL-terminated string, once, as it starts; it points at `MALLOC_CONF_TEXT`, which lives as
// long as the process and ends in a NUL (the unit tests hold it to that).
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MALLOC_CONF: Option<&'static u8> = MALLOC_CONF_TEXT.as_bytes().first();

/// glibc's malloc arenas at most (`M_ARENA_MAX`): [`SHARDS`] and glibc's main arena, the setting the lab's
/// A/B picked (fedora, 2026-11-05, the realistic profile, calm, train 4's tree, 7 and 8 rounds): resident
/// memory 523 to 459 MB (anonymous 396 to 333), and none of the other 201 numbers moved beyond noise. glibc's
/// default is eight for every core (88 in the lab, one for nearly each of the server's threads, 438 MB taken
/// and 297 MB of it free); at 2, resident memory fell further (545 to 420 MB) but the bus delivery's p99
/// went from 0.25 to 0.50 ms and runners' messages took 56% longer at p99, worse beyond noise.
pub const GLIBC_ARENAS: i32 = 5;

/// What [`init`] did with glibc's arenas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arenas {
    /// Capped at this many.
    Capped(i32),
    /// Left as the environment says: `MALLOC_ARENA_MAX`, or `glibc.malloc.arena_max` in `GLIBC_TUNABLES`.
    AsTheEnvironmentSays,
    /// glibc refused the cap (`mallopt` answered 0): its own count.
    Refused,
}

/// The cap to give glibc's arenas, if any: none where the environment already says how many (`MALLOC_ARENA_MAX`
/// set, or `GLIBC_TUNABLES` naming `glibc.malloc.arena_max`), which glibc then takes as it's told.
pub fn glibc_arenas(arena_max_set: bool, tunables: Option<&str>) -> Option<i32> {
    let tuned = tunables.is_some_and(|t| t.split(':').any(|item| item.trim_start().starts_with("glibc.malloc.arena_max=")));
    (!arena_max_set && !tuned).then_some(GLIBC_ARENAS)
}

/// What [`init`] did with glibc's arenas, on glibc.
static ARENAS: OnceLock<Arenas> = OnceLock::new();

/// The allocator's settings, as the process starts: `main`'s first call. jemalloc's are read by jemalloc
/// itself; this keeps them linked into the binary. glibc's arenas are capped here, before the process's
/// threads start (glibc makes an arena for a thread that finds the others busy, up to its cap).
pub fn init() {
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    let _ = std::hint::black_box(&MALLOC_CONF);
    #[cfg(all(feature = "jemalloc-prof", target_os = "linux"))]
    let _ = PROFILE.set(profile::start());
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    let _ = hyper_rt::idle::when_cold(COLD_AFTER, give_back_thread_cache);
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let tunables = std::env::var_os("GLIBC_TUNABLES").map(|t| t.to_string_lossy().into_owned());
        let arenas = match glibc_arenas(std::env::var_os("MALLOC_ARENA_MAX").is_some(), tunables.as_deref()) {
            Some(cap) if cap_glibc_arenas(cap) => Arenas::Capped(cap),
            Some(_) => Arenas::Refused,
            None => Arenas::AsTheEnvironmentSays,
        };
        let _ = ARENAS.set(arenas);
    }
}

/// Caps glibc's arenas at `cap` (mallopt(3), `M_ARENA_MAX`): whether glibc took it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
fn cap_glibc_arenas(cap: i32) -> bool {
    // SAFETY: mallopt(3) sets one of malloc's own parameters under its own lock; it reads and writes nothing
    // of the caller's.
    unsafe { nix::libc::mallopt(nix::libc::M_ARENA_MAX, cap) == 1 }
}

/// The allocator and its settings, for a server's and a runner's log: jemalloc's as jemalloc took them.
pub fn said() -> String {
    let mut parts: Vec<String> = jemalloc_said().into_iter().collect();
    if cfg!(all(target_os = "linux", target_env = "gnu")) {
        let whose = if cfg!(feature = "jemalloc") { "glibc's malloc allocates for the C libraries" } else { "glibc's malloc allocates" };
        parts.push(match ARENAS.get() {
            Some(Arenas::Capped(n)) => format!("{whose}, in at most {n} arenas (M_ARENA_MAX: the server's {SHARDS} shards and glibc's main arena)"),
            Some(Arenas::AsTheEnvironmentSays) => format!("{whose}, in as many arenas as MALLOC_ARENA_MAX or GLIBC_TUNABLES says"),
            Some(Arenas::Refused) => format!("{whose}, in its own count of arenas: it refused M_ARENA_MAX {GLIBC_ARENAS}"),
            None => format!("{whose}, in its own count of arenas"),
        });
    }
    if cfg!(all(target_os = "linux", target_env = "musl", not(feature = "jemalloc"))) {
        parts.push("musl's malloc allocates (kiln's jemalloc feature is off)".to_owned());
    }
    if parts.is_empty() {
        parts.push("the system's allocator allocates".to_owned());
    }
    parts.join("; ")
}

/// jemalloc and the settings it took: [`JEMALLOC_CONF`]'s, or what the environment changed of them.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn jemalloc_said() -> Option<String> {
    let asked = JEMALLOC_CONF.trim_end_matches('\0');
    let mut said = match jemalloc_took() {
        Some(took) if took == asked => format!("jemalloc 5.3 allocates ({took}, as kiln sets it)"),
        Some(took) => {
            let env = std::env::var_os("_RJEM_MALLOC_CONF").map(|v| format!(", then _RJEM_MALLOC_CONF's {}", v.to_string_lossy())).unwrap_or_default();
            format!("jemalloc 5.3 allocates ({took}: kiln's {asked}{env})")
        }
        None => format!("jemalloc 5.3 allocates (kiln's {asked}; jemalloc didn't say what it took)"),
    };
    if jemalloc_opt(c"opt.background_thread", false) == Some(true) && jemalloc_opt(c"background_thread", false) != Some(true) {
        said.push_str(", though its background threads didn't start: it purges only as its arenas are used");
    }
    said.push_str(&format!("; a pool's worker idle {} s gives its thread's cache back", COLD_AFTER.as_secs()));
    #[cfg(feature = "jemalloc-prof")]
    if let Some(profile) = PROFILE.get() {
        said.push_str(&profile.said());
    }
    Some(said)
}

#[cfg(not(all(feature = "jemalloc", target_os = "linux")))]
fn jemalloc_said() -> Option<String> {
    None
}

/// The settings jemalloc took, written as [`JEMALLOC_CONF`] writes them (without its NUL): kiln's, and the
/// environment's after them (`_RJEM_MALLOC_CONF`).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
pub fn jemalloc_took() -> Option<String> {
    Some(format!(
        "background_thread:{},narenas:{},dirty_decay_ms:{},muzzy_decay_ms:{},thp:{}",
        jemalloc_opt(c"opt.background_thread", false)?,
        jemalloc_opt(c"opt.narenas", 0_u32)?,
        jemalloc_opt(c"opt.dirty_decay_ms", 0_isize)?,
        jemalloc_opt(c"opt.muzzy_decay_ms", 0_isize)?,
        jemalloc_opt_text(c"opt.thp")?,
    ))
}

/// One of jemalloc's settings or states as it has it (mallctl(3)), read into `value`, a value of the name's
/// type in jemalloc's manual (`bool`, `unsigned`, `ssize_t`, `const char *`).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
fn jemalloc_opt<T: Copy>(name: &std::ffi::CStr, mut value: T) -> Option<T> {
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `name` ends in a NUL; jemalloc writes at most `len` bytes to `value`, both this frame's, and says
    // in `len` how many it wrote.
    let r = unsafe { tikv_jemalloc_sys::mallctl(name.as_ptr(), (&raw mut value).cast(), &raw mut len, std::ptr::null_mut(), 0) };
    (r == 0 && len == std::mem::size_of::<T>()).then_some(value)
}

/// A `const char *` setting of jemalloc's (`opt.thp`), as text.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
fn jemalloc_opt_text(name: &std::ffi::CStr) -> Option<String> {
    let text = jemalloc_opt(name, std::ptr::null::<std::ffi::c_char>())?;
    // SAFETY: jemalloc answered with a pointer to one of its own NUL-terminated strings, which live as long as
    // it does.
    (!text.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned())
}

/// Sets one of jemalloc's settings or states to `value` (mallctl(3)), a value of the name's type in its manual:
/// whether jemalloc took it.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
fn jemalloc_set<T: Copy>(name: &std::ffi::CStr, mut value: T) -> bool {
    // SAFETY: `name` ends in a NUL; jemalloc reads `size_of::<T>()` bytes from `value`, this frame's, and writes
    // nothing back (no `oldp`).
    let r = unsafe { tikv_jemalloc_sys::mallctl(name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), (&raw mut value).cast(), std::mem::size_of::<T>()) };
    r == 0
}

/// How long a pool's worker waits with nothing to do before it gives its thread's cache back to jemalloc's arenas
/// (`hyper_rt::idle`; goal 11, plan 01a1a87b2e56 step 5): [`DIRTY_DECAY_MS`]. jemalloc trims a thread's cache only
/// as that thread allocates (its garbage collection is an event of the thread's own allocations), so a worker that
/// ran a job and then waits keeps what the job freed for as long as it waits: in the lab, 54 of the blocking pool's
/// workers and the compute pool's, most of them waiting at any time (the pool hands a job to the worker idle
/// last). Past the decay, what the worker keeps is what jemalloc would have purged by now had it been given back
/// as it was freed; given back then, it's purged within the next decay.
pub const COLD_AFTER: std::time::Duration = std::time::Duration::from_millis(DIRTY_DECAY_MS as u64);

/// Gives the calling thread's cache back to its arena (jemalloc's `thread.tcache.flush`): what a pool's worker does
/// once it's cold ([`COLD_AFTER`]).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
fn give_back_thread_cache() {
    // SAFETY: the name ends in a NUL; `thread.tcache.flush` reads and writes nothing of the caller's (no `oldp`, no
    // `newp`), and acts on the calling thread's cache alone.
    let _ = unsafe { tikv_jemalloc_sys::mallctl(c"thread.tcache.flush".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0) };
}

/// What the allocator holds, in bytes, by what it holds it for: jemalloc's statistics (`stats.*` in its
/// manual), on `/metrics` as `kiln_memory_allocator_bytes{kind}` (goal 11, plan 01a1a87b2e56 step 5). They
/// split the anonymous memory no owner names into what the program holds live and what the allocator keeps:
/// live is `allocated` less `thread_caches`; the allocator's own is the rest of `resident`; what's resident
/// past `resident` (`kiln_memory_resident_bytes{kind="anon"}` less it) isn't jemalloc's (threads' stacks, the C
/// libraries' malloc, mappings made directly).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// The program's allocations (`stats.allocated`), what threads' caches keep of them included.
    pub allocated: u64,
    /// Of `allocated`, what threads' caches keep: freed and kept for the thread's next allocation of the size
    /// (`stats.arenas.<all>.tcache_bytes`). A thread's cache is trimmed as that thread allocates, so a thread
    /// that stopped keeps what it had.
    pub thread_caches: u64,
    /// The pages that hold allocations (`stats.active`): `allocated` and the room left in their pages and slabs.
    /// A large allocation's pages count whole, including those never written, which the OS hasn't made resident.
    pub active: u64,
    /// Freed pages not yet given back (`stats.arenas.<all>.pdirty`): resident until their decay purges them.
    pub dirty: u64,
    /// Pages given back lazily (`MADV_FREE`, `stats.arenas.<all>.pmuzzy`): resident until the kernel wants them.
    pub muzzy: u64,
    /// jemalloc's own metadata (`stats.metadata`).
    pub metadata: u64,
    /// The most of its pages that can be resident (`stats.resident`): active, dirty and metadata pages, written
    /// or not.
    pub resident: u64,
    /// Its active extents' pages (`stats.mapped`).
    pub mapped: u64,
    /// Address space it keeps for reuse, given back to the OS (`stats.retained`): never resident.
    pub retained: u64,
}

impl Stats {
    /// Each kind with its name on `/metrics`, in the exposition's order.
    pub fn kinds(&self) -> [(&'static str, u64); 9] {
        [
            ("allocated", self.allocated),
            ("thread_caches", self.thread_caches),
            ("active", self.active),
            ("dirty", self.dirty),
            ("muzzy", self.muzzy),
            ("metadata", self.metadata),
            ("resident", self.resident),
            ("mapped", self.mapped),
            ("retained", self.retained),
        ]
    }
}

/// `MALLCTL_ARENAS_ALL` (jemalloc_macros.h): the arena index that reads every arena's statistics merged.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
const ARENAS_ALL: u32 = 4096;

/// What jemalloc holds now ([`Stats`]): its statistics as of a new epoch (jemalloc keeps a copy it refreshes
/// only when asked to). None where jemalloc doesn't allocate, or keeps no statistics.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
pub fn stats() -> Option<Stats> {
    if !jemalloc_set(c"epoch", 1_u64) {
        return None;
    }
    let bytes = |name: &std::ffi::CStr| jemalloc_opt(name, 0_usize).and_then(|n| u64::try_from(n).ok());
    let page = bytes(c"arenas.page")?;
    let all = |what: &str| std::ffi::CString::new(format!("stats.arenas.{ARENAS_ALL}.{what}")).ok();
    let pages = |what: &str| bytes(&all(what)?)?.checked_mul(page);
    Some(Stats {
        allocated: bytes(c"stats.allocated")?,
        thread_caches: bytes(&all("tcache_bytes")?)?,
        active: bytes(c"stats.active")?,
        dirty: pages("pdirty")?,
        muzzy: pages("pmuzzy")?,
        metadata: bytes(c"stats.metadata")?,
        resident: bytes(c"stats.resident")?,
        mapped: bytes(c"stats.mapped")?,
        retained: bytes(c"stats.retained")?,
    })
}

#[cfg(not(all(feature = "jemalloc", target_os = "linux")))]
pub fn stats() -> Option<Stats> {
    None
}

/// What [`init`] did with jemalloc's heap profile, where it's built in (the `jemalloc-prof` feature).
#[cfg(all(feature = "jemalloc-prof", target_os = "linux"))]
static PROFILE: OnceLock<profile::Profile> = OnceLock::new();

/// jemalloc's heap profile in a lab's build (the `jemalloc-prof` feature; goal 11, plan 01a1a87b2e56 step 5):
/// samples taken through kiln's own unwinder, the one Rust's panics take (`_Unwind_Backtrace`, libgcc's on
/// glibc and LLVM's libunwind on musl), so a static musl build profiles as a glibc build does.
#[cfg(all(feature = "jemalloc-prof", target_os = "linux"))]
mod profile {
    use std::ffi::{c_int, c_uint, c_void};

    use super::{jemalloc_opt, jemalloc_opt_text, jemalloc_set};

    /// What [`start`] did.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum Profile {
        /// The run didn't ask for one (`prof:true` in `_RJEM_MALLOC_CONF`).
        Off,
        /// Sampling, one allocation every `2^lg_sample` bytes on average, dumps under `prefix`.
        On { lg_sample: usize, prefix: String },
        /// jemalloc refused this step, so nothing is sampled.
        Refused(&'static str),
    }

    impl Profile {
        /// What the allocator's log line says of it.
        pub(super) fn said(&self) -> String {
            match self {
                Profile::Off => String::new(),
                Profile::On { lg_sample, prefix } => {
                    format!("; its heap profile samples every 2^{lg_sample} bytes allocated through kiln's unwinder, dumping to {prefix}.*.heap")
                }
                Profile::Refused(what) => format!("; its heap profile was asked for, but jemalloc refused {what}, so nothing is sampled"),
            }
        }
    }

    /// The hook in, then sampling on, where the run asked for a profile; sampling stays off otherwise
    /// (`super::JEMALLOC_PROF_CONF`'s `prof_active:false`).
    #[allow(unsafe_code)]
    pub(super) fn start() -> Profile {
        if jemalloc_opt(c"opt.prof", false) != Some(true) {
            return Profile::Off;
        }
        // The unwinder's first walk finds the binary's unwind tables, which may allocate: done here, not in a
        // sample (jemalloc does the same for libgcc's, `prof_unwind_init`).
        let mut first = [std::ptr::null_mut::<c_void>(); 4];
        let mut len: c_uint = 0;
        // SAFETY: `first` has room for the 4 addresses the walk is told it has, and `len` counts them.
        unsafe { backtrace(first.as_mut_ptr(), &raw mut len, 4) };
        let hook: Hook = backtrace;
        if !jemalloc_set(c"experimental.hooks.prof_backtrace", hook) {
            return Profile::Refused("kiln's unwinder (experimental.hooks.prof_backtrace)");
        }
        if !jemalloc_set(c"prof.active", true) {
            return Profile::Refused("sampling (prof.active)");
        }
        Profile::On { lg_sample: jemalloc_opt(c"opt.lg_prof_sample", 0_usize).unwrap_or(0), prefix: jemalloc_opt_text(c"opt.prof_prefix").unwrap_or_else(|| "jeprof".to_owned()) }
    }

    /// jemalloc's `prof_backtrace_hook_t`: fills `frames` with up to `room` return addresses, counted in `len`.
    pub(super) type Hook = unsafe extern "C" fn(*mut *mut c_void, *mut c_uint, c_uint);

    /// libgcc's and LLVM libunwind's `struct _Unwind_Context`: only they read it.
    #[repr(C)]
    struct Context {
        _opaque: [u8; 0],
    }

    /// `_Unwind_Reason_Code` (the Itanium C++ ABI's level-1 unwinding, which both implement): go on; stop.
    const NO_REASON: c_int = 0;
    const END_OF_STACK: c_int = 5;

    type Trace = extern "C" fn(*mut Context, *mut c_void) -> c_int;

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn _Unwind_Backtrace(trace: Trace, arg: *mut c_void) -> c_int;
        fn _Unwind_GetIP(context: *mut Context) -> usize;
    }

    /// Where a walk writes: jemalloc's vector of return addresses, its count, and its room.
    struct Walk {
        frames: *mut *mut c_void,
        len: *mut c_uint,
        room: c_uint,
    }

    /// One frame of the walk: its address written down, the walk stopped at the vector's end or the stack's.
    #[allow(unsafe_code)]
    extern "C" fn step(context: *mut Context, walk: *mut c_void) -> c_int {
        // SAFETY: `walk` is the `Walk` that `backtrace` handed `_Unwind_Backtrace`, alive until it returns, and
        // nothing else reaches it meanwhile.
        let walk = unsafe { &mut *walk.cast::<Walk>() };
        // SAFETY: `context` is the unwinder's frame, valid for this call.
        let ip = unsafe { _Unwind_GetIP(context) };
        // SAFETY: `len` is the count jemalloc gave the hook, which it reads only after the hook returns.
        let n = unsafe { *walk.len };
        let Ok(at) = usize::try_from(n) else {
            return END_OF_STACK;
        };
        if ip == 0 || n >= walk.room {
            return END_OF_STACK;
        }
        // SAFETY: `at` is below `room`, the addresses `frames` has room for (jemalloc's PROF_BT_MAX); `n + 1` is at
        // most `room`, so it doesn't overflow.
        unsafe {
            *walk.frames.add(at) = std::ptr::without_provenance_mut(ip);
            *walk.len = n + 1;
        }
        NO_REASON
    }

    /// The hook: the calling thread's stack, from this function's own frame up, as return addresses.
    #[allow(unsafe_code)]
    pub(super) unsafe extern "C" fn backtrace(frames: *mut *mut c_void, len: *mut c_uint, room: c_uint) {
        let mut walk = Walk { frames, len, room };
        // SAFETY: `step` writes only within the `room` addresses at `frames` and the count at `len`, which the
        // caller gives (jemalloc's `prof_bt_t`); `walk` outlives the call.
        unsafe { _Unwind_Backtrace(step, (&raw mut walk).cast()) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The hook writes the calling thread's return addresses, never past the room it's given (goal 11, plan
        /// 01a1a87b2e56 step 5).
        #[test]
        #[allow(unsafe_code)]
        fn g11_s5_kilns_unwinder_writes_the_stack_within_its_room() {
            let mut frames = [std::ptr::null_mut::<c_void>(); 129];
            let mut len: c_uint = 0;
            // SAFETY: room for 128 addresses, told 128; the 129th stays as it was.
            unsafe { backtrace(frames.as_mut_ptr(), &raw mut len, 128) };
            let written = usize::try_from(len).unwrap_or(0);
            assert!(written >= 3, "this test's frame and the test harness's under it: {written}");
            assert!(frames.iter().take(written).all(|f| !f.is_null()));
            assert!(frames.iter().skip(written).all(|f| f.is_null()));
            let mut few = [std::ptr::null_mut::<c_void>(); 3];
            let mut n: c_uint = 0;
            // SAFETY: room for 2 of the 3, told 2.
            unsafe { backtrace(few.as_mut_ptr(), &raw mut n, 2) };
            assert_eq!(n, 2, "stopped at its room");
            assert!(few.get(2).is_some_and(|f| f.is_null()));
        }

        /// Without `prof:true` sampling stays off, and the log says nothing of it.
        #[test]
        fn g11_s5_no_profile_unless_the_run_asks() {
            if std::env::var_os("_RJEM_MALLOC_CONF").is_some() {
                return;
            }
            assert_eq!(start(), Profile::Off);
            assert_eq!(Profile::Off.said(), "");
            assert_eq!(jemalloc_opt(c"opt.prof_active", true), Some(false), "kiln's conf holds sampling off");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string jemalloc reads is the settings written out, one NUL at its end (goal 11, plan 01a1a87b2e56
    /// step 4).
    #[test]
    fn g11_s4_jemallocs_conf_is_its_settings_written_out() {
        let built = format!("background_thread:{BACKGROUND_THREAD},narenas:{NARENAS},dirty_decay_ms:{DIRTY_DECAY_MS},muzzy_decay_ms:{MUZZY_DECAY_MS},thp:{THP}\0");
        assert_eq!(JEMALLOC_CONF, built);
        assert!(std::ffi::CStr::from_bytes_with_nul(JEMALLOC_CONF.as_bytes()).is_ok(), "one NUL, at the end");
    }

    /// The settings are the ones the lab's A/B picked, as it ran them (goal 11, plan 01a1a87b2e56 step 4):
    /// fedora, 2026-11-05, g11alloc2's `_RJEM_MALLOC_CONF` on the static musl build and `MALLOC_CONF` on
    /// jemalloc preloaded (its alloc.env). Any other setting needs an A/B of its own (docs/runtime-lab.md,
    /// "The allocator (step 4)").
    #[test]
    fn g11_s4_the_settings_are_what_the_lab_picked() {
        const LAB_RAN: &str = "background_thread:true,narenas:5,dirty_decay_ms:5000,muzzy_decay_ms:0,thp:never";
        assert_eq!(JEMALLOC_CONF.strip_suffix('\0'), Some(LAB_RAN), "the lab's A/B measured {LAB_RAN}: another setting needs an A/B of its own");
    }

    /// The arenas follow the server's shards: if the server makes more, these say why they're no longer
    /// one for each and one more.
    #[test]
    fn g11_s4_the_arenas_are_the_servers_shards_and_one_more() {
        assert!(u32::from(crate::rt::Profile::server().shards) <= SHARDS);
        assert_eq!(NARENAS, SHARDS + 1);
        assert_eq!(i64::from(GLIBC_ARENAS), i64::from(SHARDS) + 1);
    }

    /// glibc's arenas are capped unless the environment says how many, which glibc then takes.
    #[test]
    fn g11_s4_glibcs_arenas_are_capped_unless_the_environment_says() {
        assert_eq!(glibc_arenas(false, None), Some(GLIBC_ARENAS));
        assert_eq!(glibc_arenas(true, None), None);
        assert_eq!(glibc_arenas(false, Some("glibc.malloc.tcache_count=0:glibc.malloc.arena_max=3")), None);
        assert_eq!(glibc_arenas(false, Some("glibc.malloc.arena_max=2")), None);
        assert_eq!(glibc_arenas(false, Some("glibc.malloc.tcache_count=0")), Some(GLIBC_ARENAS));
        assert_eq!(glibc_arenas(false, Some("glibc.malloc.arena_test=2")), Some(GLIBC_ARENAS));
        assert_eq!(glibc_arenas(false, Some("")), Some(GLIBC_ARENAS));
    }

    /// What's said names the allocator this build has, and its settings.
    #[test]
    fn g11_s4_the_allocator_is_said() {
        init();
        let said = said();
        if cfg!(all(feature = "jemalloc", target_os = "linux")) && std::env::var_os("_RJEM_MALLOC_CONF").is_none() {
            assert!(said.contains(&format!("({}, as kiln sets it)", JEMALLOC_CONF.trim_end_matches('\0'))), "{said}");
        }
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert!(said.contains("glibc's malloc"), "{said}");
            assert!(said.contains("arenas"), "{said}");
        }
        if cfg!(all(target_os = "linux", target_env = "musl", not(feature = "jemalloc"))) {
            assert!(said.contains("musl's malloc"), "{said}");
        }
        if cfg!(not(target_os = "linux")) {
            assert_eq!(said, "the system's allocator allocates");
        }
    }

    /// jemalloc reads `_rjem_malloc_conf` as it starts, and takes every setting as written (a pair it can't
    /// read it leaves at its default, and says so only on stderr).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s4_jemalloc_reads_its_settings_back() {
        if std::env::var_os("_RJEM_MALLOC_CONF").is_some() {
            return;
        }
        assert_eq!(jemalloc_opt(c"opt.background_thread", false), Some(BACKGROUND_THREAD));
        assert_eq!(jemalloc_opt(c"opt.narenas", 0_u32), Some(NARENAS));
        assert_eq!(jemalloc_opt(c"opt.dirty_decay_ms", 0_isize), isize::try_from(DIRTY_DECAY_MS).ok());
        assert_eq!(jemalloc_opt(c"opt.muzzy_decay_ms", -1_isize), isize::try_from(MUZZY_DECAY_MS).ok());
        assert_eq!(jemalloc_opt_text(c"opt.thp").as_deref(), Some(THP));
        assert_eq!(jemalloc_took().as_deref(), Some(JEMALLOC_CONF.trim_end_matches('\0')));
        // Its background threads run: purging is theirs.
        assert_eq!(jemalloc_opt(c"background_thread", false), Some(true));
    }

    /// A build with the heap profile reads kiln's settings with sampling held off until kiln's unwinder is in
    /// (goal 11, plan 01a1a87b2e56 step 5); every other build reads them as they are.
    #[test]
    fn g11_s5_the_profiles_conf_is_kilns_with_sampling_held_off() {
        assert_eq!(JEMALLOC_PROF_CONF, format!("{},prof_active:false\0", JEMALLOC_CONF.trim_end_matches('\0')));
        assert!(std::ffi::CStr::from_bytes_with_nul(JEMALLOC_PROF_CONF.as_bytes()).is_ok(), "one NUL, at the end");
        let read = if cfg!(feature = "jemalloc-prof") { JEMALLOC_PROF_CONF } else { JEMALLOC_CONF };
        assert_eq!(MALLOC_CONF_TEXT, read);
    }

    /// The allocator's kinds on /metrics, in their order, each its field (goal 11, plan 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_the_allocators_kinds_are_its_fields() {
        let stats = Stats { allocated: 1, thread_caches: 2, active: 3, dirty: 4, muzzy: 5, metadata: 6, resident: 7, mapped: 8, retained: 9 };
        let kinds = stats.kinds();
        assert_eq!(kinds.map(|(k, _)| k), ["allocated", "thread_caches", "active", "dirty", "muzzy", "metadata", "resident", "mapped", "retained"]);
        assert_eq!(kinds.map(|(_, v)| v), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    /// Where jemalloc doesn't allocate there are no allocator statistics to say.
    #[cfg(not(all(feature = "jemalloc", target_os = "linux")))]
    #[test]
    fn g11_s5_no_allocator_statistics_without_jemalloc() {
        assert_eq!(stats(), None);
    }

    /// jemalloc's statistics follow what it allocates: a block written whole counts in `allocated`, `active` and
    /// `resident` while it's held, and leaves `allocated` once freed (goal 11, plan 01a1a87b2e56 step 5). The test
    /// binary allocates through the system's allocator (`#[global_allocator]` is the server binary's), so the
    /// block is asked of jemalloc directly.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    #[allow(unsafe_code)]
    fn g11_s5_jemallocs_statistics_follow_what_it_allocates() {
        use std::alloc::{GlobalAlloc as _, Layout};
        const BLOCK: usize = 64 << 20;
        let Ok(layout) = Layout::from_size_align(BLOCK, 4096) else {
            return;
        };
        let before = stats();
        assert!(before.is_some(), "jemalloc keeps statistics (tikv-jemalloc-sys's `stats`)");
        let before = before.unwrap_or_default();
        // SAFETY: a layout of non-zero size.
        let block = unsafe { tikv_jemallocator::Jemalloc.alloc(layout) };
        assert!(!block.is_null());
        // SAFETY: `block` is BLOCK bytes, jemalloc's, written whole here.
        unsafe { std::ptr::write_bytes(block, 1, BLOCK) };
        let held = stats().unwrap_or_default();
        let block_bytes = u64::try_from(BLOCK).unwrap_or(u64::MAX);
        // Other tests allocate and free through jemalloc beside this one (a few hundred KiB at the most), so the
        // block is held to the whole of it, and its going to half of it.
        assert!(held.allocated >= block_bytes, "{before:?} then {held:?}");
        assert!(held.active >= held.allocated, "{held:?}");
        assert!(held.resident >= held.active, "{held:?}");
        assert!(held.mapped >= held.active, "{held:?}");
        assert!(held.allocated >= held.thread_caches, "{held:?}");
        // SAFETY: `block` came from this allocator with this layout, and is freed once.
        unsafe { tikv_jemallocator::Jemalloc.dealloc(block, layout) };
        let after = stats().unwrap_or_default();
        assert!(after.allocated.saturating_add(block_bytes / 2) <= held.allocated, "{held:?} then {after:?}");
    }

    /// A pool's worker goes cold after jemalloc's dirty decay (goal 11, plan 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_a_worker_goes_cold_after_the_dirty_decay() {
        assert_eq!(u128::from(DIRTY_DECAY_MS), COLD_AFTER.as_millis());
    }

    /// What a thread freed waits in its cache until the thread allocates again; given back, the cache holds none
    /// of it (goal 11, plan 01a1a87b2e56 step 5). On a thread of its own, whose cache holds this test's blocks alone.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    #[allow(unsafe_code)]
    fn g11_s5_a_cold_thread_gives_its_cache_back() -> anyhow::Result<()> {
        use std::alloc::{GlobalAlloc as _, Layout};
        let cached = std::thread::spawn(|| {
            let Ok(layout) = Layout::from_size_align(4096, 8) else {
                return None;
            };
            // SAFETY: a layout of non-zero size; each block is freed once, with it.
            let blocks: Vec<*mut u8> = (0..20).map(|_| unsafe { tikv_jemallocator::Jemalloc.alloc(layout) }).collect();
            for block in blocks.into_iter().filter(|b| !b.is_null()) {
                // SAFETY: as above.
                unsafe { tikv_jemallocator::Jemalloc.dealloc(block, layout) };
            }
            let freed = stats()?.thread_caches;
            give_back_thread_cache();
            let given = stats()?.thread_caches;
            Some((freed, given))
        })
        .join()
        .map_err(|_| anyhow::anyhow!("the test's thread panicked"))?;
        let (freed, given) = cached.ok_or_else(|| anyhow::anyhow!("no statistics on the test's thread"))?;
        // The 20 blocks freed wait in the thread's cache (a 4 KiB size keeps 20, tcache_nslots_small_min; a full one
        // keeps half as it takes more); given back, none of them.
        anyhow::ensure!(freed >= given.saturating_add(10 * 4096), "freed {freed}, given back {given}");
        Ok(())
    }

    /// The environment still overrides what kiln compiles in, a setting at a time: jemalloc reads
    /// `_RJEM_MALLOC_CONF` after `malloc_conf`, so a lab's variant needs no rebuild (goal 11, plan
    /// 01a1a87b2e56 step 4). jemalloc reads both once, as it starts. So this runs again in a process of its
    /// own (this test's binary, this test alone), started with a variant. There it reads back the variant's
    /// settings and kiln's for the rest, and checks what the log says of both.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s4_the_environment_overrides_kilns_settings() -> anyhow::Result<()> {
        // The variants docs/runtime-lab.md names for an A/B on one build.
        const VARIANT: &str = "narenas:8,dirty_decay_ms:1000";
        let Some(env) = std::env::var_os("_RJEM_MALLOC_CONF") else {
            let me = format!("{}::g11_s4_the_environment_overrides_kilns_settings", module_path!().split_once("::").map_or("", |(_, path)| path));
            let out = std::process::Command::new(std::env::current_exe()?).args(["--exact", me.as_str(), "--nocapture", "--test-threads=1"]).env("_RJEM_MALLOC_CONF", VARIANT).output()?;
            let said = String::from_utf8_lossy(&out.stdout);
            anyhow::ensure!(out.status.success(), "with _RJEM_MALLOC_CONF={VARIANT} ({}): {said}{}", out.status, String::from_utf8_lossy(&out.stderr));
            anyhow::ensure!(said.contains("1 passed"), "with _RJEM_MALLOC_CONF={VARIANT}, {me} didn't run: {said}");
            return Ok(());
        };
        // Run with settings of a person's own, jemalloc took theirs: nothing of kiln's to check.
        if env != VARIANT {
            return Ok(());
        }
        let variant = format!("background_thread:{BACKGROUND_THREAD},narenas:8,dirty_decay_ms:1000,muzzy_decay_ms:{MUZZY_DECAY_MS},thp:{THP}");
        assert_eq!(jemalloc_took(), Some(variant));
        let said = said();
        assert!(said.contains(&format!("kiln's {}, then _RJEM_MALLOC_CONF's {VARIANT})", JEMALLOC_CONF.trim_end_matches('\0'))), "{said}");
        Ok(())
    }
}
