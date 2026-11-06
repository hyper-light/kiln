//! Transparent huge pages for the server's process: off, where nothing the server starts would inherit the switch
//! (goal 11, plan 01a1a87b2e56 step 5; docs/runtime-lab.md, "Transparent huge pages off for the server").
//!
//! A transparent huge page makes a whole 2 MiB of anonymous memory resident at the first touch of any byte of it,
//! and khugepaged makes a 2 MiB range one huge page again once enough of its pages are in use. kiln keeps its own
//! memory off them already: jemalloc's mappings (`thp:never`, [`crate::os::alloc::THP`]) and the runtime threads'
//! stacks (`crate::os::stack::no_huge_pages`). What's left asks for them, or takes what the machine gives:
//!
//! - **turso's buffer pools** (vendor/turso `core/storage/buffer_pool.rs`, `arena::alloc`): each database it opens
//!   (the conversation store, records.db) maps two arenas of 3 MiB, its pages' and its WAL frames', and advises them
//!   `MADV_HUGEPAGE`. In the lab's realistic profile (alloc2, musl-jemalloc, smaps at the window's end) the two
//!   databases' mappings (6,164 kB each, `VmFlags` `hg`) held 2,064 and 5,760 kB resident, 2,048 kB of each in a
//!   huge page: 4 MiB of `AnonHugePages` where the pages in use are what turso's page cache and writes touch. The lab's
//!   machine gives huge pages only where they're asked for (fedora: `madvise`), so this is all of them there.
//! - **Everything else, on production's host**, which gives them to every anonymous mapping (adacachy: `always`,
//!   khugepaged's `max_ptes_none` 409, so a range 103 of whose 512 pages are in use becomes one huge page): the C
//!   libraries' malloc (musl's, aws-lc's and the like), the stacks of threads the runtime didn't start (jemalloc's
//!   background threads, the vendored crates' own), and any other anonymous mapping. The lab can't show those.
//!
//! `PR_SET_THP_DISABLE` (prctl(2), Linux 3.15) turns them off for the whole process, whatever a mapping asks, so
//! each of these holds the pages it touches. It's what focal does for a bus node (vendor/focal-bus,
//! `focal_platform::memory::disable_transparent_huge_pages`: a fault in an advised range entered direct compaction
//! for 70 to 685 ms on a busy host; with huge pages off there were none, and the bus's throughput and p99 were both
//! better) and Redis for itself (`disable-thp`). Quiver's memory is jemalloc's, kept off them already, so nothing of
//! the bus's changes.
//!
//! The switch is kept across `fork` and `execve`: every process the server starts inherits it. So the server turns
//! them off only where it starts no runner of its own (`[local_agent] enabled = false`, as the lab's server and a
//! server whose runners are machines of their own): the server's own runner starts legs and agents (compilers,
//! language servers, browsers) in this process, and those keep what the machine gives. The server's own helpers
//! (git, a render) inherit it. `KILN_KEEP_THP=1` keeps them as the machine gives them, for the lab's A/B on one
//! build. macOS and Windows give no transparent huge pages to turn off.

use std::io;

/// The environment variable that keeps transparent huge pages as the machine gives them (`1`): the A side of the
/// lab's A/B on one build (`kiln-lab ab --env-a KILN_KEEP_THP=1`).
pub const KEEP_VAR: &str = "KILN_KEEP_THP";

/// What the server does with transparent huge pages ([`choose`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Off for the process (`PR_SET_THP_DISABLE`).
    Off,
    /// As the machine gives them: [`KEEP_VAR`] said so.
    KeptAsAsked,
    /// As the machine gives them: the server's own runner starts legs and agents in this process, which would
    /// inherit the switch.
    KeptForItsRunner,
}

/// Whether `KILN_KEEP_THP` (its value `keep`) asks to keep transparent huge pages as the machine gives them.
fn keeps(keep: Option<&str>) -> bool {
    keep.is_some_and(|v| v.trim() == "1")
}

/// What a server does with transparent huge pages: off, unless `keep` (the environment's [`KEEP_VAR`]) asks to keep
/// them or the server has a runner of its own (`own_runner`, `[local_agent] enabled`), whose legs and agents would
/// inherit the switch.
pub fn choose(keep: Option<&str>, own_runner: bool) -> Choice {
    if keeps(keep) {
        Choice::KeptAsAsked
    } else if own_runner {
        Choice::KeptForItsRunner
    } else {
        Choice::Off
    }
}

/// What [`for_server`] did, for the server's log.
#[derive(Debug)]
pub struct Done {
    pub choice: Choice,
    /// Whether the process's transparent huge pages are off now, as its kernel says: none where the platform has
    /// none, or the kernel didn't say.
    pub off: Option<bool>,
    /// What the kernel refused, where [`Choice::Off`] wasn't taken.
    pub refused: Option<io::Error>,
}

impl Done {
    /// The server's log line.
    pub fn said(&self) -> String {
        const WHY: &str = "goal 11, plan 01a1a87b2e56 step 5";
        match (&self.refused, self.off, self.choice) {
            (Some(e), _, _) => format!("transparent huge pages stay as the machine gives them: the kernel refused PR_SET_THP_DISABLE ({e}) ({WHY})"),
            (None, None, _) => format!("transparent huge pages: this system gives a process none to turn off ({WHY})"),
            (None, Some(true), Choice::Off) => format!(
                "transparent huge pages are off for the server's process (PR_SET_THP_DISABLE): turso's arenas and the memory the machine would give them to hold the pages they touch, not 2 MiB around each ({WHY})"
            ),
            (None, Some(_), Choice::KeptAsAsked) => format!("transparent huge pages stay as the machine gives them ({KEEP_VAR}=1) ({WHY})"),
            (None, Some(_), Choice::KeptForItsRunner) => {
                format!("transparent huge pages stay as the machine gives them: the server's own runner starts legs and agents here, which would inherit the switch ({WHY})")
            }
            (None, Some(false), Choice::Off) => format!("transparent huge pages stay as the machine gives them: the kernel didn't keep PR_SET_THP_DISABLE ({WHY})"),
        }
    }
}

/// Transparent huge pages for the server's process, before its runtime starts its threads: [`choose`]'s choice, made
/// with the environment's [`KEEP_VAR`] and whether the server has a runner of its own (`own_runner`).
pub fn for_server(own_runner: bool) -> Done {
    let choice = choose(std::env::var(KEEP_VAR).ok().as_deref(), own_runner);
    let refused = match choice {
        Choice::Off => turn_off().err(),
        Choice::KeptAsAsked | Choice::KeptForItsRunner => None,
    };
    Done { choice, off: are_off(), refused }
}

/// Turns transparent huge pages off for this process and every process it starts (`PR_SET_THP_DISABLE`).
#[cfg(target_os = "linux")]
fn turn_off() -> io::Result<()> {
    rustix::thread::disable_transparent_huge_pages(true).map_err(io::Error::from)
}

/// Nothing to turn off: this system gives a process no transparent huge pages.
#[cfg(not(target_os = "linux"))]
fn turn_off() -> io::Result<()> {
    Ok(())
}

/// Whether transparent huge pages are off for this process, as its kernel says (`PR_GET_THP_DISABLE`).
#[cfg(target_os = "linux")]
pub fn are_off() -> Option<bool> {
    rustix::thread::transparent_huge_pages_are_disabled().ok()
}

/// None: this system gives a process no transparent huge pages.
#[cfg(not(target_os = "linux"))]
pub fn are_off() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 5: off, unless `KILN_KEEP_THP=1` keeps them or the server's own runner would
    /// pass the switch to its legs and agents.
    #[test]
    fn g11_s5_thp_is_off_unless_kept_or_the_server_has_its_own_runner() {
        assert_eq!(choose(None, false), Choice::Off);
        assert_eq!(choose(Some("0"), false), Choice::Off);
        assert_eq!(choose(Some(""), false), Choice::Off);
        assert_eq!(choose(Some("yes"), false), Choice::Off, "only 1 keeps them, as KILN_KEEP_PAGES");
        assert_eq!(choose(Some("1"), false), Choice::KeptAsAsked);
        assert_eq!(choose(Some(" 1\n"), true), Choice::KeptAsAsked, "the environment's word first");
        assert_eq!(choose(None, true), Choice::KeptForItsRunner);
        assert_eq!(choose(Some("0"), true), Choice::KeptForItsRunner);
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the log says what was done and why, each case in its own words.
    #[test]
    fn g11_s5_the_log_says_what_was_done_with_thp() {
        let off = Done { choice: Choice::Off, off: Some(true), refused: None }.said();
        assert!(off.contains("are off for the server's process (PR_SET_THP_DISABLE)"), "{off}");
        let asked = Done { choice: Choice::KeptAsAsked, off: Some(false), refused: None }.said();
        assert!(asked.contains("KILN_KEEP_THP=1"), "{asked}");
        let runner = Done { choice: Choice::KeptForItsRunner, off: Some(false), refused: None }.said();
        assert!(runner.contains("the server's own runner"), "{runner}");
        let refused = Done { choice: Choice::Off, off: Some(false), refused: Some(io::Error::other("EINVAL")) }.said();
        assert!(refused.contains("refused PR_SET_THP_DISABLE (EINVAL)"), "{refused}");
        let none = Done { choice: Choice::Off, off: None, refused: None }.said();
        assert!(none.contains("gives a process none"), "{none}");
        let lost = Done { choice: Choice::Off, off: Some(false), refused: None }.said();
        assert!(lost.contains("didn't keep"), "{lost}");
    }

    /// The pages of `range` that are resident, as `/proc/self/pagemap` says (its present bit, bit 63:
    /// Documentation/admin-guide/mm/pagemap.rst).
    #[cfg(target_os = "linux")]
    fn resident(range: std::ops::Range<usize>, page: usize) -> io::Result<usize> {
        use std::os::unix::fs::FileExt as _;
        const ENTRY: usize = std::mem::size_of::<u64>();
        const PRESENT: u64 = 1 << 63;
        let pages = range.end.saturating_sub(range.start) / page;
        let mut buf = vec![0_u8; pages * ENTRY];
        let at = u64::try_from(range.start / page * ENTRY).map_err(io::Error::other)?;
        std::fs::File::open("/proc/self/pagemap")?.read_exact_at(&mut buf, at)?;
        Ok(buf.as_chunks::<ENTRY>().0.iter().filter(|c| u64::from_ne_bytes(**c) & PRESENT != 0).count())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: with the server's switch, a range that asks for huge pages
    /// (`MADV_HUGEPAGE`, as turso's arenas do) holds only the page a touch reached, not the 2 MiB around it; and the
    /// kernel says the switch is on. The switch is the whole process's, kept across `execve`, so this runs again in a
    /// process of its own (this test's binary, this test alone).
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn g11_s5_with_thp_off_an_advised_range_holds_only_what_it_touched() -> anyhow::Result<()> {
        const ALONE: &str = "KILN_TEST_THP_ALONE";
        if std::env::var_os(ALONE).is_none() {
            let me = format!("{}::g11_s5_with_thp_off_an_advised_range_holds_only_what_it_touched", module_path!().split_once("::").map_or("", |(_, path)| path));
            let out = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", me.as_str(), "--nocapture", "--test-threads=1"])
                .env(ALONE, "1")
                .env_remove(KEEP_VAR)
                .output()?;
            let said = String::from_utf8_lossy(&out.stdout);
            anyhow::ensure!(out.status.success(), "alone ({}): {said}{}", out.status, String::from_utf8_lossy(&out.stderr));
            anyhow::ensure!(said.contains("1 passed"), "{me} didn't run alone: {said}");
            return Ok(());
        }
        let done = for_server(false);
        anyhow::ensure!(done.choice == Choice::Off && done.refused.is_none(), "{}", done.said());
        anyhow::ensure!(are_off() == Some(true), "the kernel says the switch is on: {:?}", are_off());
        // A huge page's size on x86-64 and arm64 with 4 KiB pages (a PMD's reach); a whole one of it lies in a mapping
        // of twice that, which the touch below lands in.
        const HUGE: usize = 2 << 20;
        let page = rustix::param::page_size().max(1);
        // SAFETY: a new private anonymous mapping of 4 MiB, made here and given to nothing else; rustix answers its
        // address or an error.
        let base = unsafe { rustix::mm::mmap_anonymous(std::ptr::null_mut(), 2 * HUGE, rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE, rustix::mm::MapFlags::PRIVATE) }?;
        let start = base.addr().next_multiple_of(HUGE);
        let range = start..start + HUGE;
        let first = base.cast::<u8>().wrapping_add(start - base.addr());
        // A kernel without transparent huge pages refuses the advice (EINVAL), and holds a page all the same.
        // SAFETY: `range` lies wholly in the mapping above (its start rounded up to 2 MiB, at most 2 MiB past the
        // base, of 4 MiB). MADV_HUGEPAGE sets a flag on the mapping and changes no byte of it.
        let _ = unsafe { rustix::mm::madvise(first.cast(), HUGE, rustix::mm::Advice::LinuxHugepage) };
        // SAFETY: `first` is `range.start`, derived from the mapping's own pointer and within it, readable and
        // writable, and nothing else refers to it.
        unsafe { first.write_volatile(1) };
        let held = resident(range, page);
        // SAFETY: the mapping made above, unmapped once; nothing refers to it after (`first` isn't used again).
        let unmapped = unsafe { rustix::mm::munmap(base, 2 * HUGE) };
        unmapped?;
        let held = held?;
        anyhow::ensure!(held == 1, "one byte touched in a range advised MADV_HUGEPAGE: {held} pages resident (a huge page is {})", HUGE / page);
        Ok(())
    }
}
