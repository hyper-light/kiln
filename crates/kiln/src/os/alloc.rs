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

/// The settings as jemalloc's `malloc_conf` (tikv-jemalloc-sys's prefixed name for it), read at its first
/// allocation; jemalloc's own is a weak `NULL` this replaces. `Option<&u8>` is a nullable pointer, as C's
/// `const char *` is.
#[cfg(all(feature = "jemalloc", target_os = "linux"))]
#[allow(unsafe_code)]
// SAFETY: jemalloc reads this symbol as a `const char *` (an `Option<&u8>` has a pointer's layout, `None`
// as NULL) to a NUL-terminated string, once, as it starts; it points at `JEMALLOC_CONF`, which lives as
// long as the process and ends in a NUL (the unit tests hold it to that).
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MALLOC_CONF: Option<&'static u8> = JEMALLOC_CONF.as_bytes().first();

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
