//! What of this binary stays in the server's resident memory once it's done with it (goal 11, plan
//! 01a1a87b2e56 step 5).
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
//! it from the file. `KILN_KEEP_PAGES=1` keeps every page as before, for the lab's A/B.

use std::sync::atomic::{AtomicU8, Ordering};

/// The UI's files are packed.
const WARMED: u8 = 1;
/// The server has started everything it runs.
const STARTED: u8 = 2;
/// What startup's drop waits for.
const BOTH: u8 = WARMED | STARTED;

/// The marks made so far, of [`WARMED`] and [`STARTED`].
static MARKS: AtomicU8 = AtomicU8::new(0);

/// Whether `KILN_KEEP_PAGES` (its value `keep`) asks to keep the binary's pages resident as before.
fn keeps(keep: Option<&str>) -> bool {
    keep.is_some_and(|v| v.trim() == "1")
}

/// Whether this process keeps the binary's pages resident as before (`KILN_KEEP_PAGES=1`).
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
        tracing::info!(file_mb = file_mb(), "the binary's pages stay resident (KILN_KEEP_PAGES=1)");
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

    /// Goal 11, plan 01a1a87b2e56 step 5: only `KILN_KEEP_PAGES=1` keeps the pages.
    #[test]
    fn g11_s5_only_keep_pages_1_keeps_them() {
        assert!(keeps(Some("1")) && keeps(Some(" 1\n")));
        assert!(!keeps(None) && !keeps(Some("0")) && !keeps(Some("")) && !keeps(Some("yes")));
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the log's MB are to the tenth.
    #[test]
    fn g11_s5_the_logs_megabytes_are_to_the_tenth() {
        assert_eq!(mb(127_000_000), 127.0);
        assert_eq!(mb(11_062_909), 11.1);
        assert_eq!(mb(0), 0.0);
    }
}
