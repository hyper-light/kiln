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

/// How a [`Candidate`]'s value reads (its type in jemalloc's manual, mallctl(3)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `bool`.
    Bool,
    /// `unsigned`.
    Unsigned,
    /// `size_t`.
    Size,
}

/// One of jemalloc's settings past [`JEMALLOC_CONF`]'s that a lab's A/B gives a run through `_RJEM_MALLOC_CONF`
/// (goal 11, plan 01a1a87b2e56 step 5), each said in the allocator's log line where a run's differs from
/// jemalloc's default ([`said`]): its name in `malloc_conf`, its `opt.*` name, its type, and jemalloc 5.3.0's
/// default (tikv-jemalloc-sys 0.6.1's copy: src/tcache.c, src/extent.c, src/extent_mmap.c,
/// include/jemalloc/internal/arena_types.h). docs/runtime-lab.md ("The allocator's overhead, threads' stacks and
/// observe's index (step 5)") gives each one's expected saving, its latency risk and the A/B that tells.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub name: &'static str,
    pub ctl: &'static std::ffi::CStr,
    pub kind: Kind,
    pub default: &'static str,
}

/// The candidates, in the order the log says them:
/// - `tcache_max`: the largest size a thread's cache keeps (32 KiB). TUNING.md's memory-first pick is 4096.
/// - `tcache_nslots_small_max` and `tcache_nslots_large`: how many of a small size (200 at the most) and of a
///   large one (20) a thread's cache keeps.
/// - `lg_extent_max_active_fit`: how much larger (2^6) a dirty extent may be than the allocation it's cut for.
/// - `oversize_threshold`: the size (8 MiB) from which an allocation takes the arena that purges at once.
/// - `retain`: whether address space given back is kept for reuse (Linux: true); never resident either way.
pub const CANDIDATES: [Candidate; 6] = [
    Candidate { name: "tcache_max", ctl: c"opt.tcache_max", kind: Kind::Size, default: "32768" },
    Candidate { name: "tcache_nslots_small_max", ctl: c"opt.tcache_nslots_small_max", kind: Kind::Unsigned, default: "200" },
    Candidate { name: "tcache_nslots_large", ctl: c"opt.tcache_nslots_large", kind: Kind::Unsigned, default: "20" },
    Candidate { name: "lg_extent_max_active_fit", ctl: c"opt.lg_extent_max_active_fit", kind: Kind::Size, default: "6" },
    Candidate { name: "oversize_threshold", ctl: c"opt.oversize_threshold", kind: Kind::Size, default: "8388608" },
    Candidate { name: "retain", ctl: c"opt.retain", kind: Kind::Bool, default: "true" },
];

/// The environment variable a build with kiln's `jemalloc` feature reads jemalloc's settings from, over the ones
/// compiled in ([`JEMALLOC_CONF`]), a setting at a time, as jemalloc starts: tikv-jemalloc-sys builds jemalloc with
/// its names prefixed (`_rjem_`), its variable's too. glibc's malloc and a jemalloc preloaded read `MALLOC_CONF`,
/// which this build's jemalloc never reads ([`said`] says so where it's set).
pub const CONF_VAR: &str = "_RJEM_MALLOC_CONF";

/// How far apart the realistic profile's slowest sub-second client's calls come: an agent's MCP calls, 250 ms
/// (`kiln-lab`'s `Profile::realistic`, `mcp_think_ms`); its readers ask every 50 ms, its page loads every 100 ms and a
/// turn's events come 25 ms apart. What attaches and turns free recurs every 2.5 to 3 s: [`DIRTY_DECAY_MS`]'s basis.
pub const SUBSECOND_MS: u32 = 250;

/// The dirty decay that keeps what the sub-second work frees for its next call and lets go of what attaches and
/// turns free (goal 11, plan 01a1a87b2e56 step 5): twice [`SUBSECOND_MS`], as jemalloc keeps about what was freed in
/// the last half of its decay. Where pages are freed at an even rate, `dirty` holds about this over [`DIRTY_DECAY_MS`]
/// of what it holds now (a tenth); an attach's snapshot and a turn's packed chunks then fault their pages in anew.
/// jemalloc's background threads purge at most every 100 ms (`BACKGROUND_THREAD_MIN_INTERVAL_NS`), five times in it.
pub const SUBSECOND_DECAY_MS: u32 = 2 * SUBSECOND_MS;

/// The base-2 log of the pages jemalloc is built for on the lab's machine and production's (x86_64 Linux: 4 KiB);
/// an arm64 Linux build takes 64 KiB (`JEMALLOC_SYS_WITH_LG_PAGE=16`, docs/runtime-lab.md, step 4).
pub const LG_PAGE: u32 = 12;

/// jemalloc's largest size class cut from a slab with pages of `2^lg_page` bytes (`SC_SMALL_MAXCLASS`,
/// include/jemalloc/internal/sc.h: `2^(lg_page + SC_LG_NGROUP - 1) + (SC_NGROUP - 1) * 2^(lg_page - 1)`, its four
/// size classes a doubling): 14,336 bytes with 4 KiB pages. Past it an allocation is a run of whole pages of its own
/// (an extent), so a thread's cache of them keeps whole pages a slot: 20 a size (`tcache_nslots_large`), 2.4 MB of
/// the five sizes from 16 to 32 KiB for a thread that used them all. None past `usize`.
pub const fn small_maxclass(lg_page: u32) -> Option<usize> {
    let Some(base) = 1_usize.checked_shl(lg_page.saturating_add(1)) else {
        return None;
    };
    let Some(delta) = 1_usize.checked_shl(lg_page.saturating_sub(1)) else {
        return None;
    };
    let Some(deltas) = delta.checked_mul(3) else {
        return None;
    };
    base.checked_add(deltas)
}

/// One B side of the A/B of jemalloc's settings against [`JEMALLOC_CONF`], step 4's compiled in (goal 11, plan
/// 01a1a87b2e56 step 5; docs/runtime-lab.md, "The A/B of jemalloc's settings"): an environment variable the server
/// reads as it starts, so the lab runs it on the build it measures, with no rebuild
/// (`kiln-lab ab --a <kiln> --b <kiln> --env-b <var>=<value>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    /// What the lab's labels and the doc call it.
    pub name: &'static str,
    /// The variable: [`CONF_VAR`] (jemalloc's settings), or [`POOL_DECAY_VAR`] (the blocking pool's arena).
    pub var: &'static str,
    /// Its value.
    pub value: &'static str,
    /// The part of `kiln_memory_allocator_bytes{kind}` it moves (the lab's `server.allocator.<kind>_mb`), which caps
    /// what it can save: an arm whose part A's rounds held at less than the noise of the lab's anonymous memory is
    /// skipped, as no value of it could move that memory beyond noise.
    pub moves: &'static str,
    /// A candidate for the settings compiled in, once it holds every latency number calm and with hogs; else a
    /// reading of a floor and its cost, which says why kiln's setting stands.
    pub candidate: bool,
}

/// The muzzy decay TUNING.md pairs with the dirty decay kiln keeps (goal 11, plan 01a1a87b2e56 step 5): both of its
/// examples that set the two set them alike, its memory-first one ("High resource consumption application,
/// prioritizing memory usage") to 5 s each, its CPU-first one to 30 s each. Step 4 took the memory-first dirty decay
/// ([`DIRTY_DECAY_MS`]) and its muzzy decay at 0 ([`MUZZY_DECAY_MS`]): the `lazy-free` arm reads what that 0 keeps out
/// of resident memory, and what it costs in pages faulted in anew.
pub const PAIRED_MUZZY_DECAY_MS: u32 = DIRTY_DECAY_MS;

/// The arena count midway between jemalloc's floor (one, every thread in it) and [`NARENAS`] (goal 11, plan
/// 01a1a87b2e56 step 5): the `bisect-arenas` arm's, the next count where `one-arena` saves past the lab's noise but
/// a latency number is worse beyond it. jemalloc gives a thread the arena with the fewest threads as it first
/// allocates (`arena_choose_hard`), so an arena's threads are the process's in turn, the shards among them.
pub const BISECT_NARENAS: u32 = NARENAS.div_ceil(2);

/// The arms: the candidates by what they can save, then the readings; docs/runtime-lab.md's plan ("The A/B of
/// jemalloc's settings: the lab plan") runs them in its order, three of them only on an earlier arm's result. Each
/// value is derived (the tests hold each one to its derivation), and each changes one of step 4's settings
/// ([`JEMALLOC_CONF`]), or `tcache_max`, which step 4 left at jemalloc's default:
/// - `subsecond-decay`: [`SUBSECOND_DECAY_MS`].
/// - `pool-subsecond-decay`: the same decay for the blocking pool's arena alone ([`POOL_DECAY_VAR`]), in its place
///   where it's worse beyond noise: the shards' arenas keep [`DIRTY_DECAY_MS`].
/// - `slab-caches`: `tcache_max` at [`small_maxclass`] of [`LG_PAGE`]: threads' caches keep slab regions alone.
/// - `page-caches`: `tcache_max` at a page, TUNING.md's memory-first pick: after `slab-caches`, where `thread_caches`
///   still holds more than the lab's noise.
/// - `bisect-arenas`: `narenas` at [`BISECT_NARENAS`]: after `one-arena`, where it saved more than the lab's noise
///   and a latency number was worse beyond it.
/// - `one-arena` (a reading): `narenas` at 1, every thread in one arena: what [`NARENAS`]' arenas cost in room left
///   in slabs (`active` less `allocated`) and in dirty pages kept apart, against the shards' waits on one another.
/// - `no-decay` (a reading): every free purged at once, on the thread that frees: the floor of `dirty`, and its cost.
/// - `inline-purge` (a reading): no background threads: what they purge that the arenas' own ticks don't, and what
///   purging costs the threads that free ([`BACKGROUND_THREAD`]'s basis).
/// - `lazy-free` (a reading): `muzzy_decay_ms` at [`PAIRED_MUZZY_DECAY_MS`]: what the dirty decay purges goes with
///   `MADV_FREE`, and stays resident as `muzzy` until it decays or the kernel takes it ([`MUZZY_DECAY_MS`]'s basis).
///   Never a candidate in the lab, whose machine has memory to spare: resident memory can only rise.
pub const ARMS: [Arm; 9] = [
    Arm { name: "subsecond-decay", var: CONF_VAR, value: "dirty_decay_ms:500", moves: "dirty", candidate: true },
    Arm { name: "pool-subsecond-decay", var: POOL_DECAY_VAR, value: "500", moves: "dirty", candidate: true },
    Arm { name: "slab-caches", var: CONF_VAR, value: "tcache_max:14336", moves: "thread_caches", candidate: true },
    Arm { name: "page-caches", var: CONF_VAR, value: "tcache_max:4096", moves: "thread_caches", candidate: true },
    Arm { name: "bisect-arenas", var: CONF_VAR, value: "narenas:3", moves: "active - allocated", candidate: true },
    Arm { name: "one-arena", var: CONF_VAR, value: "narenas:1", moves: "active - allocated", candidate: false },
    Arm { name: "no-decay", var: CONF_VAR, value: "dirty_decay_ms:0", moves: "dirty", candidate: false },
    Arm { name: "inline-purge", var: CONF_VAR, value: "background_thread:false", moves: "dirty", candidate: false },
    Arm { name: "lazy-free", var: CONF_VAR, value: "muzzy_decay_ms:5000", moves: "muzzy", candidate: false },
];

/// jemalloc's background threads' least sleep (`BACKGROUND_THREAD_MIN_INTERVAL_NS`, src/background_thread.c): they
/// purge no oftener than this, so what's given back sooner waits for their next purge all the same.
pub const PURGE_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a pool's worker waits with nothing to do before it's cold (`went_cold`): jemalloc's dirty decay as it
/// took it, `decay_ms` (a lab's arm may change it, [`ARMS`]), at least [`PURGE_EVERY`]; [`COLD_AFTER`] where jemalloc
/// keeps dirty pages for good (a decay of -1) or doesn't say. The cold-after is the decay's (its basis), so an arm
/// that moves the decay moves it too.
pub fn cold_after(decay_ms: Option<isize>) -> std::time::Duration {
    match decay_ms.and_then(|ms| u64::try_from(ms).ok()) {
        Some(ms) => std::time::Duration::from_millis(ms).max(PURGE_EVERY),
        None => COLD_AFTER,
    }
}

/// jemalloc's dirty decay as it took it (`opt.dirty_decay_ms`): kiln's, or the environment's over it.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn decay_took() -> Option<isize> {
    jemalloc_opt(c"opt.dirty_decay_ms", 0_isize)
}

#[cfg(all(target_os = "linux", not(feature = "jemalloc")))]
fn decay_took() -> Option<isize> {
    None
}

/// The environment's word for the blocking pool's arena (goal 11, plan 01a1a87b2e56 step 5): its dirty decay, in
/// milliseconds. Set, the blocking pool's workers allocate in an arena of their own (jemalloc's `arenas.create`;
/// its TUNING.md, "Explicit arenas", "Explicit thread-to-arena binding"), whose dirty pages are kept that long,
/// while the shards' arenas keep theirs [`DIRTY_DECAY_MS`]. The shards' work recurs every few seconds and reuses
/// what it freed (the decay's basis); most of the pool's 54 workers wait, and a job's pages freed are idle until
/// the next job of its kind. Unset, they share the shards' arenas, as before: the lab's A/B on one build
/// (`--env-b KILN_ALLOC_POOL_DECAY_MS=1000`) says whether it's kept.
pub const POOL_DECAY_VAR: &str = "KILN_ALLOC_POOL_DECAY_MS";

/// The blocking pool's arena's dirty decay that `value` (the environment's [`POOL_DECAY_VAR`]) asks for, in
/// milliseconds: none where it's unset or not a whole number of them. jemalloc takes any of them (up to its
/// `NSTIME_SEC_MAX` seconds, past `u32`'s); 0 purges what's freed at once, on the thread that freed it.
pub fn pool_decay_ms(value: Option<&str>) -> Option<u32> {
    value?.trim().parse::<u32>().ok()
}

/// The blocking pool's arena, once made: jemalloc's index of it and its dirty decay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolArena {
    pub index: u32,
    pub dirty_decay_ms: u32,
}

/// What [`init`] made of the blocking pool's arena, where the environment asked for one ([`POOL_DECAY_VAR`]).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
static POOL_ARENA: OnceLock<Result<PoolArena, &'static str>> = OnceLock::new();

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
/// threads start (glibc makes an arena for a thread that finds the others busy, up to its cap). The blocking
/// pool's arena is made here where the environment asks for one ([`POOL_DECAY_VAR`]), and what a pool's worker
/// does as it starts and once it's cold is said, before the pools start (`hyper_rt::idle`).
pub fn init() {
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    let _ = std::hint::black_box(&MALLOC_CONF);
    #[cfg(all(feature = "jemalloc-prof", target_os = "linux"))]
    let _ = PROFILE.set(profile::start());
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    if let Some(ms) = pool_decay_ms(std::env::var(POOL_DECAY_VAR).ok().as_deref()) {
        let _ = POOL_ARENA.set(make_pool_arena(ms));
    }
    #[cfg(target_os = "linux")]
    {
        let _ = hyper_rt::idle::when_cold(cold_after(decay_took()), went_cold);
        let _ = hyper_rt::idle::when_started(worker_started);
    }
    // Before anything opens a database: SQLite takes its allocator only before it's first used, and while no other
    // thread uses it (sqlite3_config(3)), as at main's first line. Not in this crate's unit tests, which call this
    // beside other tests' databases: g11_s5_sqlite_allocates_through_jemalloc gives it in a process of its own.
    #[cfg(all(feature = "jemalloc", target_os = "linux", not(test)))]
    let _ = SQLITE.set(sqlite::through_jemalloc());
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
    #[cfg(target_os = "linux")]
    parts.push(format!("a runtime thread's stack takes no huge pages, and a pool's worker idle {} ms gives back its stack's pages below its frame", cold_after(decay_took()).as_millis()));
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
    let past: Vec<String> = past_kilns().iter().map(|(c, took)| format!("{}:{took} (jemalloc's {})", c.name, c.default)).collect();
    if !past.is_empty() {
        said.push_str(&format!("; past kiln's settings, {}", past.join(", ")));
    }
    said.push_str(&format!("; a pool's worker idle {} ms gives its thread's cache back", cold_after(decay_took()).as_millis()));
    if let Some(theirs) = std::env::var_os("MALLOC_CONF") {
        said.push_str(&format!("; MALLOC_CONF ({}) isn't read: this build's jemalloc reads {CONF_VAR}", theirs.to_string_lossy()));
    }
    said.push_str(&match POOL_ARENA.get() {
        Some(Ok(arena)) => format!("; the blocking pool's workers allocate in an arena of their own ({}), its dirty pages kept {} ms ({POOL_DECAY_VAR})", arena.index, arena.dirty_decay_ms),
        Some(Err(what)) => format!("; the blocking pool's workers share the shards' arenas: jemalloc refused {what} ({POOL_DECAY_VAR})"),
        None => String::new(),
    });
    said.push_str(match SQLITE.get() {
        Some(true) => "; SQLite allocates through it",
        Some(false) => "; SQLite refused it (it was in use already), so it allocates through the C library's malloc",
        None => "",
    });
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

/// The value jemalloc took for candidate `c` ([`CANDIDATES`]), written as `malloc_conf` writes it.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn candidate_took(c: &Candidate) -> Option<String> {
    Some(match c.kind {
        Kind::Bool => jemalloc_opt(c.ctl, false)?.to_string(),
        Kind::Unsigned => jemalloc_opt(c.ctl, 0_u32)?.to_string(),
        Kind::Size => jemalloc_opt(c.ctl, 0_usize)?.to_string(),
    })
}

/// The candidates jemalloc took at other than its default, with what it took: what a run's `_RJEM_MALLOC_CONF`
/// gave past kiln's settings (goal 11, plan 01a1a87b2e56 step 5).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
pub fn past_kilns() -> Vec<(&'static Candidate, String)> {
    CANDIDATES.iter().filter_map(|c| candidate_took(c).filter(|took| took != c.default).map(|took| (c, took))).collect()
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
/// as it was freed; given back then, it's purged within the next decay. Its stack's pages below its frame go then
/// too, by the same reckoning: idle that long, the job they were touched for is done (`crate::os::stack`). This is
/// the decay compiled in; a run whose environment changes jemalloc's has its pools' workers follow that one
/// ([`cold_after`]).
pub const COLD_AFTER: std::time::Duration = std::time::Duration::from_millis(DIRTY_DECAY_MS as u64);

/// Gives the calling thread's cache back to its arena (jemalloc's `thread.tcache.flush`): what a pool's worker does
/// once it's cold ([`COLD_AFTER`], [`went_cold`]).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
fn give_back_thread_cache() {
    // SAFETY: the name ends in a NUL; `thread.tcache.flush` reads and writes nothing of the caller's (no `oldp`, no
    // `newp`), and acts on the calling thread's cache alone.
    let _ = unsafe { tikv_jemalloc_sys::mallctl(c"thread.tcache.flush".as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0) };
}

/// What a pool's worker does once it's cold ([`COLD_AFTER`], `hyper_rt::idle`; goal 11, plan 01a1a87b2e56 step 5):
/// gives its thread's cache back to jemalloc's arenas, where jemalloc allocates, then its stack's pages below its
/// frame back to the kernel (`crate::os::stack`), from the shallowest frame it runs in.
#[cfg(target_os = "linux")]
fn went_cold() {
    #[cfg(feature = "jemalloc")]
    give_back_thread_cache();
    let _ = crate::os::stack::give_back_unused();
}

/// What a pool's worker does as it starts, on its own thread (`hyper_rt::idle`; goal 11, plan 01a1a87b2e56 step 5):
/// its stack takes no huge pages (`crate::os::stack`), and a blocking pool's worker takes the pools' arena where
/// the run made one ([`POOL_DECAY_VAR`]).
#[cfg(target_os = "linux")]
fn worker_started(pool: hyper_rt::idle::Pool) {
    let _ = crate::os::stack::no_huge_pages();
    #[cfg(feature = "jemalloc")]
    if pool == hyper_rt::idle::Pool::Blocking
        && let Some(Ok(arena)) = POOL_ARENA.get()
    {
        let _ = bind_to_arena(arena.index);
    }
    #[cfg(not(feature = "jemalloc"))]
    let _ = pool;
}

/// `arena.<index>.<what>`, one of an arena's own settings or states, as mallctl(3) names it.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn arena_ctl(index: u32, what: &str) -> Option<std::ffi::CString> {
    std::ffi::CString::new(format!("arena.{index}.{what}")).ok()
}

/// Makes an arena for the blocking pool's workers ([`POOL_DECAY_VAR`]) and has it keep its dirty pages
/// `dirty_decay_ms`; its muzzy pages, as every arena's, [`MUZZY_DECAY_MS`] or the environment's. Its index, or what
/// jemalloc refused.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn make_pool_arena(dirty_decay_ms: u32) -> Result<PoolArena, &'static str> {
    // Reading `arenas.create` makes the arena, and answers its index.
    let index = jemalloc_opt(c"arenas.create", 0_u32).ok_or("a new arena (arenas.create)")?;
    let decay = isize::try_from(dirty_decay_ms).map_err(|_| "a decay past ssize_t")?;
    let ctl = arena_ctl(index, "dirty_decay_ms").ok_or("its dirty decay's name")?;
    if !jemalloc_set(&ctl, decay) {
        return Err("its dirty decay (arena.<i>.dirty_decay_ms)");
    }
    Ok(PoolArena { index, dirty_decay_ms })
}

/// Has the calling thread allocate in arena `index` from now on (`thread.arena`): whether jemalloc took it. What the
/// thread freed and its cache keeps goes back to the arenas it came from, as ever.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
fn bind_to_arena(index: u32) -> bool {
    jemalloc_set(c"thread.arena", index)
}

/// Whether SQLite took jemalloc as its allocator ([`init`]).
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
static SQLITE: OnceLock<bool> = OnceLock::new();

/// SQLite's allocations through jemalloc, not the C library's malloc (goal 11, plan 01a1a87b2e56 step 5).
/// jemalloc is the binary's allocator under the prefix tikv-jemalloc-sys gives it (`_rjem_`), so the C libraries
/// linked in allocate through the C library's malloc still: on the static musl build, musl's, whose groups held
/// 18 to 24 MB of the server's anonymous memory in the lab (alloc2, the musl-jemalloc rounds: 167 to 262
/// mappings under 1 MiB, none of them jemalloc's, whose every mapping is 2 MiB or more), SQLite's page caches,
/// schemas, statements and lookaside nearly all of it (7.4 MB live in the train-4 heap profile, where a preloaded
/// jemalloc took the C libraries' allocations too). Through jemalloc they share its size classes, its purging and
/// its statistics. SQLite takes an allocator only before it's first used (`sqlite3_config`, `SQLITE_CONFIG_MALLOC`),
/// so [`init`], `main`'s first call, gives it; a process that used SQLite already is refused, and keeps the C
/// library's.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
mod sqlite {
    use std::ffi::{c_int, c_void};

    use rusqlite::ffi;

    /// `xMalloc`: `n` bytes, none for none.
    #[allow(unsafe_code)]
    unsafe extern "C" fn malloc(n: c_int) -> *mut c_void {
        match usize::try_from(n) {
            // SAFETY: jemalloc's malloc, for a size of at least one byte.
            Ok(n) if n > 0 => unsafe { tikv_jemalloc_sys::malloc(n) },
            _ => std::ptr::null_mut(),
        }
    }

    /// `xFree`: what `malloc` or `realloc` gave.
    #[allow(unsafe_code)]
    unsafe extern "C" fn free(p: *mut c_void) {
        // SAFETY: SQLite frees only what this allocator gave it, once (or a null, which jemalloc ignores).
        unsafe { tikv_jemalloc_sys::free(p) }
    }

    /// `xRealloc`: `p` grown or shrunk to `n` bytes (SQLite asks for at least one: a size `roundup` gave).
    #[allow(unsafe_code)]
    unsafe extern "C" fn realloc(p: *mut c_void, n: c_int) -> *mut c_void {
        match usize::try_from(n) {
            // SAFETY: `p` is this allocator's (or null), and the size at least one byte.
            Ok(n) if n > 0 => unsafe { tikv_jemalloc_sys::realloc(p, n) },
            _ => std::ptr::null_mut(),
        }
    }

    /// `xSize`: the bytes `p` holds.
    #[allow(unsafe_code)]
    unsafe extern "C" fn size(p: *mut c_void) -> c_int {
        if p.is_null() {
            return 0;
        }
        // SAFETY: `p` is a live allocation of this allocator's.
        let n = unsafe { tikv_jemalloc_sys::malloc_usable_size(p) };
        c_int::try_from(n).unwrap_or(c_int::MAX)
    }

    /// `xRoundup`: the size an allocation of `n` bytes takes (jemalloc's size class).
    #[allow(unsafe_code)]
    unsafe extern "C" fn roundup(n: c_int) -> c_int {
        let Ok(want) = usize::try_from(n) else {
            return n;
        };
        if want == 0 {
            return 0;
        }
        // SAFETY: nallocx only computes a size class; it allocates nothing.
        let got = unsafe { tikv_jemalloc_sys::nallocx(want, 0) };
        if got == 0 { n } else { c_int::try_from(got).unwrap_or(n) }
    }

    /// `xInit` and `xShutdown`: jemalloc needs neither.
    extern "C" fn init(_: *mut c_void) -> c_int {
        ffi::SQLITE_OK
    }

    extern "C" fn shutdown(_: *mut c_void) {}

    /// Gives SQLite jemalloc as its allocator: whether it took it (it doesn't once it's been used).
    #[allow(unsafe_code)]
    pub(super) fn through_jemalloc() -> bool {
        let methods = ffi::sqlite3_mem_methods {
            xMalloc: Some(malloc),
            xFree: Some(free),
            xRealloc: Some(realloc),
            xSize: Some(size),
            xRoundup: Some(roundup),
            xInit: Some(init),
            xShutdown: Some(shutdown),
            pAppData: std::ptr::null_mut(),
        };
        // SAFETY: SQLITE_CONFIG_MALLOC takes a pointer to a `sqlite3_mem_methods`, which SQLite copies before the call
        // returns (sqlite3_config(3)); its functions are this module's, thread-safe as jemalloc is. SQLite refuses
        // (SQLITE_MISUSE) once it's initialized, changing nothing.
        unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &raw const methods) == ffi::SQLITE_OK }
    }
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

    /// SQLite allocates through jemalloc once given it before its first use: its pages show in jemalloc's
    /// statistics, and go from them with the database (goal 11, plan 01a1a87b2e56 step 5). SQLite takes an
    /// allocator only before it's first used, and this test binary's other tests use it, so this runs again in a
    /// process of its own (this test's binary, this test alone).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_sqlite_allocates_through_jemalloc() -> anyhow::Result<()> {
        const ALONE: &str = "KILN_TEST_SQLITE_ALONE";
        if std::env::var_os(ALONE).is_none() {
            let me = format!("{}::g11_s5_sqlite_allocates_through_jemalloc", module_path!().split_once("::").map_or("", |(_, path)| path));
            let out = std::process::Command::new(std::env::current_exe()?).args(["--exact", me.as_str(), "--nocapture", "--test-threads=1"]).env(ALONE, "1").output()?;
            let said = String::from_utf8_lossy(&out.stdout);
            anyhow::ensure!(out.status.success(), "alone ({}): {said}{}", out.status, String::from_utf8_lossy(&out.stderr));
            anyhow::ensure!(said.contains("1 passed"), "{me} didn't run alone: {said}");
            return Ok(());
        }
        anyhow::ensure!(sqlite::through_jemalloc(), "SQLite took jemalloc before its first use");
        let before = stats().unwrap_or_default().allocated;
        let conn = rusqlite::Connection::open_in_memory()?;
        // 40,000 rows of 100 random bytes: 4 MB of pages, an in-memory database's all held by its page cache.
        conn.execute_batch("CREATE TABLE t(x); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40000) INSERT INTO t SELECT randomblob(100) FROM n;")?;
        let held = stats().unwrap_or_default().allocated;
        anyhow::ensure!(held >= before.saturating_add(2 << 20), "SQLite's pages through jemalloc: {before} then {held}");
        drop(conn);
        let after = stats().unwrap_or_default().allocated;
        anyhow::ensure!(after.saturating_add(2 << 20) <= held, "given back with the database: {held} then {after}");
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

    /// Runs this module's test `name` again in a process of its own (this test's binary, that test alone) with `env`
    /// set: for what jemalloc and [`init`] read once, as a process starts.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    fn alone(name: &str, env: &[(&str, &str)]) -> anyhow::Result<()> {
        let me = format!("{}::{name}", module_path!().split_once("::").map_or("", |(_, path)| path));
        let out = std::process::Command::new(std::env::current_exe()?).args(["--exact", me.as_str(), "--nocapture", "--test-threads=1"]).envs(env.iter().copied()).output()?;
        let said = String::from_utf8_lossy(&out.stdout);
        anyhow::ensure!(out.status.success(), "{me} with {env:?} ({}): {said}{}", out.status, String::from_utf8_lossy(&out.stderr));
        anyhow::ensure!(said.contains("1 passed"), "{me} with {env:?} didn't run alone: {said}");
        Ok(())
    }

    /// The candidates are jemalloc's settings with jemalloc 5.3.0's defaults, as jemalloc reads them back where a run
    /// gives none, and none is said past kiln's (goal 11, plan 01a1a87b2e56 step 5).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_the_candidates_are_at_jemallocs_defaults_unless_a_run_says() {
        if std::env::var_os("_RJEM_MALLOC_CONF").is_some() {
            return;
        }
        for c in &CANDIDATES {
            assert_eq!(candidate_took(c).as_deref(), Some(c.default), "{}", c.name);
        }
        assert!(past_kilns().is_empty());
        assert!(!said().contains("past kiln's settings"), "{}", said());
    }

    /// A run's settings past kiln's (a lab's variant, `_RJEM_MALLOC_CONF`) are taken over jemalloc's defaults, kiln's
    /// own stay as kiln sets them, and the log says each with jemalloc's default beside it (goal 11, plan 01a1a87b2e56
    /// step 5). jemalloc reads its settings once, so this runs again in a process of its own with the variant.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_a_runs_settings_past_kilns_are_taken_and_said() -> anyhow::Result<()> {
        const VARIANT: &str = "tcache_max:4096,lg_extent_max_active_fit:3,oversize_threshold:2097152,retain:false";
        if std::env::var_os("_RJEM_MALLOC_CONF").is_none() {
            return alone("g11_s5_a_runs_settings_past_kilns_are_taken_and_said", &[("_RJEM_MALLOC_CONF", VARIANT)]);
        }
        if std::env::var_os("_RJEM_MALLOC_CONF").is_some_and(|v| v != VARIANT) {
            return Ok(());
        }
        let past: Vec<(&str, String)> = past_kilns().into_iter().map(|(c, took)| (c.name, took)).collect();
        let want = [("tcache_max", "4096"), ("lg_extent_max_active_fit", "3"), ("oversize_threshold", "2097152"), ("retain", "false")];
        anyhow::ensure!(past.iter().map(|(n, v)| (*n, v.as_str())).eq(want), "{past:?}");
        let said = said();
        anyhow::ensure!(said.contains(&format!("({}, as kiln sets it)", JEMALLOC_CONF.trim_end_matches('\0'))), "{said}");
        anyhow::ensure!(
            said.contains("past kiln's settings, tcache_max:4096 (jemalloc's 32768), lg_extent_max_active_fit:3 (jemalloc's 6), oversize_threshold:2097152 (jemalloc's 8388608), retain:false (jemalloc's true)"),
            "{said}"
        );
        Ok(())
    }

    /// The pools' arena's decay is a whole number of milliseconds, or none asked (goal 11, plan 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_the_pools_arenas_decay_is_whole_milliseconds() {
        assert_eq!(pool_decay_ms(None), None);
        assert_eq!(pool_decay_ms(Some("1000")), Some(1000));
        assert_eq!(pool_decay_ms(Some(" 0 ")), Some(0));
        assert_eq!(pool_decay_ms(Some("")), None);
        assert_eq!(pool_decay_ms(Some("-1")), None, "never purging isn't a pick");
        assert_eq!(pool_decay_ms(Some("1.5")), None);
        assert_eq!(pool_decay_ms(Some("1s")), None);
    }

    /// The freed pages an arena keeps, in bytes (`stats.arenas.<i>.pdirty`, as of a new epoch).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    fn arena_dirty_bytes(index: u32) -> Option<u64> {
        if !jemalloc_set(c"epoch", 1_u64) {
            return None;
        }
        let page = u64::try_from(jemalloc_opt(c"arenas.page", 0_usize)?).ok()?;
        let name = std::ffi::CString::new(format!("stats.arenas.{index}.pdirty")).ok()?;
        u64::try_from(jemalloc_opt(&name, 0_usize)?).ok()?.checked_mul(page)
    }

    /// An arena made for the pools keeps what its threads free for its own decay, not the shards' arenas': at 0 it
    /// purges at once, at a minute it keeps it (goal 11, plan 01a1a87b2e56 step 5). On threads of their own, each
    /// bound to its arena, whose blocks are asked of jemalloc directly (the test binary's allocator is the system's).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    #[allow(unsafe_code)]
    fn g11_s5_an_arenas_own_decay_keeps_or_purges_what_its_threads_free() -> anyhow::Result<()> {
        use std::alloc::{GlobalAlloc as _, Layout};
        // Past a thread's cache (`tcache_max`, 32 KiB) and below the arena that purges at once (`oversize_threshold`,
        // 8 MiB): freed, it goes straight back to its arena.
        const BLOCK: usize = 4 << 20;
        for (decay, kept) in [(0_u32, false), (60_000, true)] {
            let arena = make_pool_arena(decay).map_err(anyhow::Error::msg)?;
            let ctl = arena_ctl(arena.index, "dirty_decay_ms").ok_or_else(|| anyhow::anyhow!("its name"))?;
            anyhow::ensure!(jemalloc_opt(&ctl, -2_isize) == isize::try_from(decay).ok(), "arena {}'s decay", arena.index);
            let dirty = std::thread::spawn(move || -> Option<u64> {
                if !bind_to_arena(arena.index) || jemalloc_opt(c"thread.arena", u32::MAX) != Some(arena.index) {
                    return None;
                }
                let layout = Layout::from_size_align(BLOCK, 4096).ok()?;
                // SAFETY: a layout of non-zero size.
                let block = unsafe { tikv_jemallocator::Jemalloc.alloc(layout) };
                if block.is_null() {
                    return None;
                }
                // SAFETY: `block` is BLOCK bytes, jemalloc's, written whole, then freed once with its layout.
                unsafe {
                    std::ptr::write_bytes(block, 1, BLOCK);
                    tikv_jemallocator::Jemalloc.dealloc(block, layout);
                }
                arena_dirty_bytes(arena.index)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("the test's thread panicked"))?
            .ok_or_else(|| anyhow::anyhow!("arena {} wasn't taken, or said nothing", arena.index))?;
            let block = u64::try_from(BLOCK)?;
            if kept {
                anyhow::ensure!(dirty >= block, "a minute's decay keeps the block: {dirty} bytes dirty");
            } else {
                anyhow::ensure!(dirty == 0, "no decay purges it at once: {dirty} bytes dirty");
            }
        }
        Ok(())
    }

    /// Where a run asks for the pools' arena, the blocking pool's workers take it as they start and the compute
    /// pool's keep the shards' arenas; the log says so (goal 11, plan 01a1a87b2e56 step 5). [`init`] reads the
    /// environment once, so this runs again in a process of its own with the variable set.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_the_blocking_pools_workers_take_the_pools_arena() -> anyhow::Result<()> {
        if std::env::var_os(POOL_DECAY_VAR).is_none() {
            return alone("g11_s5_the_blocking_pools_workers_take_the_pools_arena", &[(POOL_DECAY_VAR, "1000")]);
        }
        init();
        let arena = match POOL_ARENA.get() {
            Some(Ok(arena)) => *arena,
            other => anyhow::bail!("the pools' arena: {other:?}"),
        };
        anyhow::ensure!(arena.dirty_decay_ms == 1000, "{arena:?}");
        let on = |pool: hyper_rt::idle::Pool| {
            std::thread::spawn(move || {
                worker_started(pool);
                jemalloc_opt(c"thread.arena", u32::MAX)
            })
            .join()
            .ok()
            .flatten()
        };
        anyhow::ensure!(on(hyper_rt::idle::Pool::Blocking) == Some(arena.index), "a blocking pool's worker takes arena {}", arena.index);
        anyhow::ensure!(on(hyper_rt::idle::Pool::Compute).is_some_and(|i| i != arena.index), "a compute pool's worker keeps the shards'");
        let said = said();
        anyhow::ensure!(said.contains(&format!("the blocking pool's workers allocate in an arena of their own ({}), its dirty pages kept 1000 ms", arena.index)), "{said}");
        Ok(())
    }

    /// The arm called `name`.
    fn arm(name: &str) -> Option<Arm> {
        ARMS.iter().copied().find(|a| a.name == name)
    }

    /// Each arm of the A/B of jemalloc's settings is its derivation written out, and changes one of step 4's
    /// settings, or one past them, from what's compiled in (goal 11, plan 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_the_arms_are_their_derivations() {
        assert_eq!(SUBSECOND_DECAY_MS, 2 * SUBSECOND_MS);
        // Read through black_box: the checks are of the values as built, not constants folded away.
        let decay = std::hint::black_box(SUBSECOND_DECAY_MS);
        assert!(decay < DIRTY_DECAY_MS, "the arm keeps less than step 4's decay");
        assert!(u128::from(decay) >= 2 * PURGE_EVERY.as_millis(), "jemalloc's background threads purge within it");
        assert_eq!(small_maxclass(12), Some(14_336), "4 KiB pages");
        assert_eq!(small_maxclass(16), Some(229_376), "64 KiB pages: arm64 Linux's jemalloc");
        assert_eq!(small_maxclass(u32::MAX), None);
        // The bisection lies strictly between the floor and step 4's count; the paired muzzy decay is the dirty decay
        // kiln keeps, past step 4's 0.
        let (bisect, paired) = std::hint::black_box((BISECT_NARENAS, PAIRED_MUZZY_DECAY_MS));
        assert_eq!(bisect, NARENAS.div_ceil(2));
        assert!(1 < bisect && bisect < NARENAS, "{bisect} between 1 and {NARENAS}");
        assert_eq!(paired, DIRTY_DECAY_MS);
        assert!(paired > MUZZY_DECAY_MS, "the reading changes step 4's {MUZZY_DECAY_MS}");
        let want = [
            ("subsecond-decay", CONF_VAR, format!("dirty_decay_ms:{SUBSECOND_DECAY_MS}")),
            ("pool-subsecond-decay", POOL_DECAY_VAR, SUBSECOND_DECAY_MS.to_string()),
            ("slab-caches", CONF_VAR, format!("tcache_max:{}", small_maxclass(LG_PAGE).unwrap_or(0))),
            ("page-caches", CONF_VAR, format!("tcache_max:{}", 1_u64 << LG_PAGE)),
            ("bisect-arenas", CONF_VAR, format!("narenas:{BISECT_NARENAS}")),
            ("one-arena", CONF_VAR, "narenas:1".to_owned()),
            ("no-decay", CONF_VAR, "dirty_decay_ms:0".to_owned()),
            ("inline-purge", CONF_VAR, "background_thread:false".to_owned()),
            ("lazy-free", CONF_VAR, format!("muzzy_decay_ms:{PAIRED_MUZZY_DECAY_MS}")),
        ];
        assert_eq!(ARMS.len(), want.len());
        for (name, var, value) in &want {
            let a = arm(name);
            assert_eq!(a.map(|a| (a.var, a.value.to_owned())), Some((*var, value.clone())), "{name}");
        }
        assert_eq!(pool_decay_ms(arm("pool-subsecond-decay").map(|a| a.value)), Some(SUBSECOND_DECAY_MS));
        // Each changes what's compiled in: kiln's setting, or jemalloc 5.3's default past kiln's.
        let kilns = JEMALLOC_CONF.trim_end_matches('\0');
        for a in ARMS.iter().filter(|a| a.var == CONF_VAR) {
            let (setting, value) = a.value.split_once(':').unwrap_or_default();
            assert!(!setting.is_empty() && !value.is_empty(), "{}: a setting and its value", a.name);
            let compiled = kilns.split(',').find_map(|kv| kv.strip_prefix(setting).and_then(|v| v.strip_prefix(':')));
            let default = CANDIDATES.iter().find(|c| c.name == setting).map(|c| c.default);
            let was = compiled.or(default);
            assert!(was.is_some() && was != Some(value), "{}: {setting} is {was:?} already", a.name);
        }
        // Candidates first, then the readings; one name each.
        let first_reading = ARMS.iter().position(|a| !a.candidate).unwrap_or(ARMS.len());
        assert!(ARMS.iter().skip(first_reading).all(|a| !a.candidate));
        let names: std::collections::HashSet<&str> = ARMS.iter().map(|a| a.name).collect();
        assert_eq!(names.len(), ARMS.len());
    }

    /// A pool's worker goes cold after the decay jemalloc took, at least jemalloc's background threads' least
    /// sleep, and after the compiled-in decay where jemalloc keeps dirty pages for good or doesn't say (goal 11, plan
    /// 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_a_worker_goes_cold_after_the_decay_jemalloc_took() {
        assert_eq!(cold_after(isize::try_from(DIRTY_DECAY_MS).ok()), COLD_AFTER);
        assert_eq!(cold_after(Some(500)), std::time::Duration::from_millis(500));
        assert_eq!(cold_after(Some(0)), PURGE_EVERY);
        assert_eq!(cold_after(Some(-1)), COLD_AFTER, "jemalloc keeps dirty pages for good");
        assert_eq!(cold_after(None), COLD_AFTER);
    }

    /// The A/B the doc gives the lab is these arms, each by its name and its variable as kiln-lab sets it (goal 11, plan
    /// 01a1a87b2e56 step 5).
    #[test]
    fn g11_s5_the_docs_ab_runs_the_arms() -> anyhow::Result<()> {
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/runtime-lab.md"))?;
        for a in &ARMS {
            anyhow::ensure!(doc.contains(&format!("`{}`", a.name)), "docs/runtime-lab.md names {}", a.name);
            anyhow::ensure!(doc.contains(&format!("--env-b {}={}", a.var, a.value)), "docs/runtime-lab.md runs {} as --env-b {}={}", a.name, a.var, a.value);
        }
        Ok(())
    }

    /// The slab arm's `tcache_max` is the largest size class this jemalloc cuts from a slab, as it says itself (goal
    /// 11, plan 01a1a87b2e56 step 5).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_the_slab_arm_is_jemallocs_largest_slab_size() -> anyhow::Result<()> {
        let page = jemalloc_opt(c"arenas.page", 0_usize).ok_or_else(|| anyhow::anyhow!("arenas.page"))?;
        let nbins = jemalloc_opt(c"arenas.nbins", 0_u32).ok_or_else(|| anyhow::anyhow!("arenas.nbins"))?;
        let last = std::ffi::CString::new(format!("arenas.bin.{}.size", nbins.saturating_sub(1)))?;
        let largest = jemalloc_opt(&last, 0_usize).ok_or_else(|| anyhow::anyhow!("{last:?}"))?;
        anyhow::ensure!(Some(largest) == small_maxclass(page.trailing_zeros()), "jemalloc's largest slab size {largest} with {page}-byte pages");
        if page == 1 << LG_PAGE {
            anyhow::ensure!(arm("slab-caches").map(|a| a.value) == Some("tcache_max:14336"), "the arm's at this build's pages");
        }
        Ok(())
    }

    /// Each arm given as jemalloc's settings reaches jemalloc, which takes its value over kiln's or its own default,
    /// with kiln's other settings as they're compiled in; a dirty decay's arm moves the pools' cold-after with it, and
    /// the muzzy decay's leaves it (goal 11, plan 01a1a87b2e56 step 5). jemalloc reads its settings once, as it starts,
    /// so each runs in a process of its own.
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_each_arm_reaches_jemalloc() -> anyhow::Result<()> {
        let arms = ARMS.iter().filter(|a| a.var == CONF_VAR);
        let Some(env) = std::env::var_os(CONF_VAR) else {
            for a in arms {
                alone("g11_s5_each_arm_reaches_jemalloc", &[(CONF_VAR, a.value)])?;
            }
            return Ok(());
        };
        let Some(a) = arms.into_iter().find(|a| env == a.value) else {
            return Ok(());
        };
        let (setting, value) = a.value.split_once(':').ok_or_else(|| anyhow::anyhow!("{}", a.name))?;
        let took = match setting {
            "dirty_decay_ms" => jemalloc_opt(c"opt.dirty_decay_ms", 0_isize).map(|v| v.to_string()),
            "muzzy_decay_ms" => jemalloc_opt(c"opt.muzzy_decay_ms", 0_isize).map(|v| v.to_string()),
            "tcache_max" => jemalloc_opt(c"opt.tcache_max", 0_usize).map(|v| v.to_string()),
            "narenas" => jemalloc_opt(c"opt.narenas", 0_u32).map(|v| v.to_string()),
            "background_thread" => jemalloc_opt(c"opt.background_thread", true).map(|v| v.to_string()),
            other => anyhow::bail!("{}: no reading of {other}", a.name),
        };
        anyhow::ensure!(took.as_deref() == Some(value), "{}: jemalloc took {setting} {took:?}", a.name);
        let said = said();
        anyhow::ensure!(said.contains(&format!("then {CONF_VAR}'s {})", a.value)) || said.contains("past kiln's settings"), "{said}");
        let cold = cold_after(decay_took());
        if setting == "dirty_decay_ms" {
            let ms = value.parse::<u64>()?;
            anyhow::ensure!(cold == std::time::Duration::from_millis(ms).max(PURGE_EVERY), "{}", a.name);
        } else {
            anyhow::ensure!(cold == COLD_AFTER, "{}: the pools' workers go cold after the compiled decay, {cold:?}", a.name);
        }
        Ok(())
    }

    /// `MALLOC_CONF`, glibc's and a preloaded jemalloc's variable, changes nothing of this build's jemalloc, and the log
    /// says so: a lab's arm given by it would measure nothing (goal 11, plan 01a1a87b2e56 step 5).
    #[cfg(all(feature = "jemalloc", target_os = "linux"))]
    #[test]
    fn g11_s5_malloc_conf_is_said_unread() -> anyhow::Result<()> {
        const THEIRS: &str = "narenas:2";
        if std::env::var_os("MALLOC_CONF").is_none() {
            return alone("g11_s5_malloc_conf_is_said_unread", &[("MALLOC_CONF", THEIRS)]);
        }
        if std::env::var_os("MALLOC_CONF").is_some_and(|v| v != THEIRS) || std::env::var_os(CONF_VAR).is_some() {
            return Ok(());
        }
        anyhow::ensure!(jemalloc_opt(c"opt.narenas", 0_u32) == Some(NARENAS), "kiln's arenas");
        let said = said();
        anyhow::ensure!(said.contains(&format!("MALLOC_CONF ({THEIRS}) isn't read: this build's jemalloc reads {CONF_VAR}")), "{said}");
        Ok(())
    }
}
