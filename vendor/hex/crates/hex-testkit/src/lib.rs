//! What Hex's tests share. A test that can't run here (a host without cgroup v2 delegation, a pod's
//! root without CAP_SYS_ADMIN) says so with [`skip`] (CLAUDE.md §3): a line on stderr, which libtest
//! captures and shows only for a test that fails, and the same line appended to the skip record,
//! which libtest's capture can't reach. scripts/gates.sh points `HEX_SKIPS` at [`RECORD`] under the
//! target directory, empties it before `cargo test` and prints it after, pass or fail, so a CI log
//! names every skipped test, what it lacked and why (HEX-GATE-1, docs/design/README.md).
//!
//! A test's wait for a fact takes its own fact's record ([`waits`]): twice the slowest answer the waits census
//! recorded for it, or measuring where no row records it yet, its answers then recorded for the census.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod waits;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The skip record under the target directory: where scripts/gates.sh points `HEX_SKIPS`, and where a
/// skip goes without it.
pub const RECORD: &str = "tmp/hex-skips.tsv";

/// The variable naming the skip record, which scripts/gates.sh sets.
pub const VAR: &str = "HEX_SKIPS";

/// The longest line the record takes. A reason past it is cut at a character boundary, the cut marked
/// `…`, so a skip whose reason is a long error still lands as one line (CLAUDE.md §2). 4 KiB holds
/// every reason Hex's tests give, the longest an OS error and a path.
pub const MAX_LINE: usize = 4096;

/// Most directories looked through above the test binary for the target directory: cargo puts a test
/// binary in `<target>/<profile>/deps/`, or `<target>/<triple>/<profile>/deps/` when cross-building.
const MAX_UP: usize = 8;

/// Says that `test` skipped, lacking `missing`, and `why`: on stderr and in the skip record, one line
/// of four tab-separated fields (the test binary, `test`, `missing`, `why`). Refused when the record
/// can't be found or written, so a skip that can't be said fails its test instead of passing unseen.
pub fn skip(test: &str, missing: &str, why: &str) -> io::Result<()> {
    let line = line(&binary(), test, missing, why);
    io::stderr()
        .lock()
        .write_all(format!("SKIP {line}").as_bytes())?;
    append(&record()?, &line)
}

/// The skip record this test process writes: `HEX_SKIPS` when set, else [`RECORD`] under the target
/// directory the running test binary sits in (the nearest directory above it that holds cargo's
/// `CACHEDIR.TAG`).
pub fn record() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(VAR) {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe()?;
    exe.ancestors()
        .skip(1)
        .take(MAX_UP)
        .find(|dir| dir.join("CACHEDIR.TAG").is_file())
        .map(|target| target.join(RECORD))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no target directory (one holding CACHEDIR.TAG) within {MAX_UP} directories above {}",
                    exe.display()
                ),
            )
        })
}

/// Appends `line` to the record at `path`, making its directory first: one write of the whole line
/// to a file opened for appending, so lines from test processes running at once land whole.
fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// The record's line: four fields, each with its tabs and line breaks made spaces, the reason cut to
/// keep the line within [`MAX_LINE`].
fn line(binary: &str, test: &str, missing: &str, why: &str) -> String {
    let flat = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let head = format!("{}\t{}\t{}\t", flat(binary), flat(test), flat(missing));
    let why = flat(why);
    // The room left for the reason and the newline; none at all is what saturating means here.
    let room = MAX_LINE.saturating_sub(head.len()).saturating_sub(1);
    if why.len() <= room {
        return format!("{head}{why}\n");
    }
    let keep = room.saturating_sub('…'.len_utf8());
    let cut = why
        .char_indices()
        .map(|(at, c)| at.saturating_add(c.len_utf8()))
        .take_while(|end| *end <= keep)
        .last()
        .unwrap_or(0);
    format!("{head}{}…\n", why.get(..cut).unwrap_or_default())
}

/// The running test binary's name, without the hash cargo appends (`process_box-0123456789abcdef`).
fn binary() -> String {
    let exe = std::env::current_exe().ok();
    let stem = exe
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|s| s.to_str())
        .unwrap_or("an unnamed test binary");
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            name.to_owned()
        }
        _ => stem.to_owned(),
    }
}

#[cfg(test)]
mod tests;
