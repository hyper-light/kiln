//! What of this binary stays in the server's resident memory once it's done with it (goal 11, plan
//! 01a1a87b2e56 step 5): all of it, unless `KILN_KEEP_PAGES=0` says to drop what startup alone needed (goal 11's
//! audit, row B14).
//!
//! A fault on the binary maps the 64 KiB of the file around it that the page cache holds, and the server reads
//! its whole binary into the page cache as it starts (its build hash). So every 64 KiB window of code and
//! read-only data that starting touched stayed resident for as long as the server ran: 127 MB of the binary's
//! pages at the window's end in the lab's realistic profile (102 MB of its 177 MiB of code, 18 MB of read-only
//! data), beside 401 MB of anonymous memory. Two drops take them out of resident memory (Linux,
//! `crate::os::pages`; elsewhere nothing):
//!
//! - The UI's embedded files ([`ui_files`]): read whole as the server starts, to keep on disk for pages loaded
//!   before a deploy (`crate::ui::keep_assets`: each file new to the data directory, every one in a new
//!   directory, 11 MB in this tree's build) and to compress with brotli (`crate::ui::warm`), then served as
//!   their brotli copies, which the heap holds. Once every copy is made, the files' pages go. A client that
//!   takes no brotli, or a file not worth compressing (a font, an image), reads its pages back from the page
//!   cache.
//! - Startup's pages ([`warmed`] and [`started`]): once the server has started everything it runs and the UI's
//!   files are packed, every page of the binary's code and read-only data goes, and what the server runs from
//!   then on comes back as it runs it. What only starting needed (the store's migrations, every subsystem's
//!   setting up, the UI's compression) stays out.
//!
//! Either drop takes a page only where it's still the file's own (`/proc/self/pagemap`): the page cache keeps
//! it, so its next touch is a minor fault of a few microseconds, which maps its 64 KiB window back. If the
//! page cache let it go meanwhile (it lets unmapped pages go sooner under memory pressure), that touch reads
//! it from the file: a major fault, on whichever thread runs that code next, a shard's among them.
//!
//! So neither drop is made unless `KILN_KEEP_PAGES=0` asks (goal 11's audit, row B14): what the drops save is
//! file pages the page cache holds anyway, and the price is every thread's code faulting back, minor faults at
//! best and a disk read on a shard where the host reclaimed them (production's host is short of memory, and
//! its file system stalls). `g11_b14_a_dropped_page_faults_on_the_shard_that_touches_it_and_a_kept_one_does_not`
//! measures it: a shard re-reading data whose pages were dropped is charged faults for them
//! (`getrusage(RUSAGE_THREAD)`), and one re-reading the same data kept is charged none. `KILN_KEEP_PAGES=0`
//! drops them, for the lab's memory A/B.

use std::sync::atomic::{AtomicU8, Ordering};

/// The UI's files are packed.
const WARMED: u8 = 1;
/// The server has started everything it runs.
const STARTED: u8 = 2;
/// What startup's drop waits for.
const BOTH: u8 = WARMED | STARTED;

/// The marks made so far, of [`WARMED`] and [`STARTED`].
static MARKS: AtomicU8 = AtomicU8::new(0);

/// Whether `KILN_KEEP_PAGES` (its value `keep`) keeps the binary's pages resident: unless it's `0`, which asks
/// for the drops (goal 11's audit, row B14).
fn keeps(keep: Option<&str>) -> bool {
    keep.is_none_or(|v| v.trim() != "0")
}

/// Whether this process keeps the binary's pages resident: unless `KILN_KEEP_PAGES=0`.
fn keep() -> bool {
    keeps(std::env::var("KILN_KEEP_PAGES").ok().as_deref())
}

/// What the process holds of files in resident memory, in MB, for the log.
fn file_mb() -> f64 {
    let file = crate::memory::resident().and_then(|kinds| kinds.iter().find(|(k, _)| *k == "file").map(|(_, b)| *b)).unwrap_or(0);
    mb(file)
}

fn mb(bytes: u64) -> f64 {
    // To the tenth of a MB the log says.
    (bytes as f64 / 1e5).round() / 10.0
}

/// The UI's embedded files, packed: their bytes in the binary (`spans`, each an address and a length) out of
/// resident memory. None for a build that reads its files from disk (a debug build).
pub async fn ui_files(spans: Vec<(usize, usize)>) {
    if spans.is_empty() || !crate::os::pages::CAN || keep() {
        return;
    }
    let files = spans.len();
    match crate::rt::blocking(move || crate::os::pages::drop_spans(&spans)).await {
        Ok(Ok(d)) => tracing::info!(
            files,
            dropped_mb = mb(d.bytes),
            file_mb = file_mb(),
            "the UI's embedded files left resident memory once their brotli copies were made: a copy is what's \
             served, and a file read again comes back from the page cache (goal 11, plan 01a1a87b2e56 step 5)"
        ),
        Ok(Err(e)) => tracing::warn!(error = %e, "the UI's embedded files stay resident: their pages couldn't be read or dropped"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "the UI's embedded files stay resident: the blocking pool didn't take the drop"),
    }
}

/// The UI's files are packed (`crate::ui::warm`): half of what startup's drop waits for.
pub fn warmed() {
    mark(WARMED);
}

/// The server has started everything it runs (`crate::server::run`): the other half.
pub fn started() {
    mark(STARTED);
}

fn mark(bit: u8) {
    if completes(MARKS.fetch_or(bit, Ordering::AcqRel), bit) {
        drop(crate::rt::spawn(drop_startup()));
    }
}

/// Whether marking `bit` over the marks `before` it is what makes both: true once, for whichever comes second.
fn completes(before: u8, bit: u8) -> bool {
    before & BOTH != BOTH && (before | bit) & BOTH == BOTH
}

/// Every page of the binary's code and read-only data out of resident memory, once startup is done.
async fn drop_startup() {
    if !crate::os::pages::CAN {
        return;
    }
    if keep() {
        tracing::info!(
            file_mb = file_mb(),
            "the binary's pages stay resident: a page dropped would fault back on whichever thread next ran it, a shard's \
             among them (KILN_KEEP_PAGES=0 drops them; goal 11's audit, row B14)"
        );
        return;
    }
    let before = file_mb();
    match crate::rt::blocking(crate::os::pages::drop_all_own).await {
        Ok(Ok(d)) => tracing::info!(
            dropped_mb = mb(d.bytes),
            runs = d.runs,
            kept_kb = d.kept / 1024,
            file_mb_before = before,
            file_mb = file_mb(),
            "startup is done: the binary's code and read-only data left resident memory, and what the server runs \
             comes back as it runs it, from the page cache (goal 11, plan 01a1a87b2e56 step 5)"
        ),
        Ok(Err(e)) => tracing::warn!(error = %e, "the binary's pages stay resident: they couldn't be read or dropped"),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "the binary's pages stay resident: the blocking pool didn't take the drop"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 5: startup's drop goes once, when the second of its two marks is made,
    /// in either order, and never again.
    #[test]
    fn g11_s5_startups_drop_goes_once_both_marks_are_made() {
        assert!(!completes(0, WARMED) && !completes(0, STARTED), "one mark alone isn't startup's end");
        assert!(completes(WARMED, STARTED) && completes(STARTED, WARMED), "the second completes it, whichever it is");
        assert!(!completes(BOTH, WARMED) && !completes(BOTH, STARTED), "marked again, nothing more");
        assert!(!completes(WARMED, WARMED), "the same mark twice isn't both");
        // In order, as `mark` folds them: only the step that makes both says so.
        let mut marks = 0;
        let went: Vec<bool> = [WARMED, WARMED, STARTED, STARTED, WARMED]
            .iter()
            .map(|bit| {
                let before = marks;
                marks |= bit;
                completes(before, *bit)
            })
            .collect();
        assert_eq!(went, [false, false, true, false, false]);
    }

    /// Goal 11's audit, row B14: the pages are kept unless `KILN_KEEP_PAGES=0` asks for the drops.
    #[test]
    fn g11_b14_the_pages_are_kept_unless_keep_pages_0() {
        assert!(keeps(None), "kept by default");
        assert!(keeps(Some("1")) && keeps(Some(" 1\n")) && keeps(Some("")) && keeps(Some("yes")));
        assert!(!keeps(Some("0")) && !keeps(Some(" 0\n")), "0 drops them");
    }

    /// Read-only data of this test binary's own, larger than four of the largest windows Linux maps around a fault
    /// (64 KiB), for a shard to read.
    #[cfg(target_os = "linux")]
    static SHARDS_OWN: [u8; 320 << 10] = [0xA5; 320 << 10];

    /// The faults the calling thread has been charged so far, minor and major (`getrusage(RUSAGE_THREAD)`).
    #[cfg(target_os = "linux")]
    fn faults_here() -> anyhow::Result<u64> {
        let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_THREAD)?;
        Ok(u64::try_from(usage.minor_page_faults())?.saturating_add(u64::try_from(usage.major_page_faults())?))
    }

    /// Goal 11's audit, row B14: a page of the binary that was dropped faults on the thread that next reads it, a
    /// shard of kiln's runtime here, and a page that was kept doesn't. On one shard, in one poll: the binary's own
    /// read-only data read whole (so resident), then read again after the drop as the default decides it (kept: no
    /// fault), then dropped as `KILN_KEEP_PAGES=0` drops it and read again (faults, a window at a time, and the
    /// same bytes). The faults counted are the shard's own (`getrusage(RUSAGE_THREAD)`), and nothing else runs on
    /// it in between.
    #[cfg(target_os = "linux")]
    #[test]
    fn g11_b14_a_dropped_page_faults_on_the_shard_that_touches_it_and_a_kept_one_does_not() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            anyhow::ensure!(crate::rt::on_shard(), "read on a shard");
            let data: &[u8] = std::hint::black_box(&SHARDS_OWN);
            let spans = [(data.as_ptr().addr(), data.len())];
            let sum = |d: &[u8]| d.iter().map(|b| u64::from(*b)).sum::<u64>();
            let want = sum(data);
            // As startup's drop decides by default.
            if !keeps(None) {
                crate::os::pages::drop_spans(&spans)?;
            }
            let before = faults_here()?;
            let kept = sum(std::hint::black_box(data));
            let kept_faults = faults_here()?.saturating_sub(before);
            // As `KILN_KEEP_PAGES=0` drops it.
            let dropped = crate::os::pages::drop_spans(&spans)?;
            let before = faults_here()?;
            let again = sum(std::hint::black_box(data));
            let dropped_faults = faults_here()?.saturating_sub(before);
            anyhow::ensure!(kept == want && again == want, "the same bytes either way");
            anyhow::ensure!(kept_faults == 0, "kept by default: {kept_faults} faults on the shard re-reading it");
            anyhow::ensure!(dropped.bytes > 0 && dropped_faults > 0, "dropped ({dropped:?}): {dropped_faults} faults on the shard re-reading it");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the log's MB are to the tenth.
    #[test]
    fn g11_s5_the_logs_megabytes_are_to_the_tenth() {
        assert_eq!(mb(127_000_000), 127.0);
        assert_eq!(mb(11_062_909), 11.1);
        assert_eq!(mb(0), 0.0);
    }
}
