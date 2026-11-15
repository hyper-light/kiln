//! The waits census (`bench/competitive/probes/waits_census.md`; decision 9): with [`ENV_FILE`] naming a file, every
//! answer every named wait in this process records ([`hex_core::patience`]) is appended as `name nanos` (completed
//! wire refusals: `name !nanos`, run-outs `name >nanos`), from the waits' own timing points, so the probe reads each
//! wait's distribution without re-implementing it. It disarms no wait: one past its W* tells its run-out, as anywhere,
//! and follows its party to its answer, which comes in uncensored, so the record keeps each wait's tail. With it unset
//! nothing is installed and a wait's answer costs one atomic load more. A bench's switch: the probe sets it.
//!
//! Bounded (CLAUDE.md §2): at most [`ENV_MAX`] lines, which the probe sets from its N; past it one last line says
//! the census is full, and no more is written, so the record says it was cut rather than end silently (§3).

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use hex_api::Refusal;

/// The file the census appends to.
pub const ENV_FILE: &str = "HEX_WAITS_CENSUS";
/// The most lines it writes.
pub const ENV_MAX: &str = "HEX_WAITS_CENSUS_MAX";

static OUT: OnceLock<Mutex<File>> = OnceLock::new();
static MAX: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);

/// Installs the census where [`ENV_FILE`] and [`ENV_MAX`] are set: says whether it did. One set without the other,
/// or a bound that isn't a number, is refused by type.
pub fn install_from_env() -> Result<bool, Refusal> {
    let (file, max) = match (std::env::var_os(ENV_FILE), std::env::var_os(ENV_MAX)) {
        (None, None) => return Ok(false),
        (Some(file), Some(max)) => (file, max),
        _ => {
            return Err(Refusal::Invalid {
                what: "waits census",
                why: format!("{ENV_FILE} and {ENV_MAX} are set together or not at all"),
            });
        }
    };
    let max: u64 = max
        .to_str()
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| Refusal::Invalid {
            what: "waits census",
            why: format!("{ENV_MAX} isn't a count of lines"),
        })?;
    let out = File::options()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| Refusal::os("opening the waits census", e))?;
    MAX.store(max, Ordering::Relaxed);
    let _ = OUT.set(Mutex::new(out));
    Ok(hex_core::patience::census(record))
}

/// An answer `name nanos`, typed refusal `name !nanos`, or run-out `name >nanos`; past the bound, one line saying the census is
/// full, then nothing.
fn record(name: &'static str, sample: hex_core::patience::Sample) {
    let Some(out) = OUT.get() else {
        return;
    };
    let n = LINES.fetch_add(1, Ordering::Relaxed);
    let max = MAX.load(Ordering::Relaxed);
    let line = if n < max {
        hex_core::patience::census_line(name, sample)
    } else if n == max {
        hex_core::patience::census_full_line(max)
    } else {
        return;
    };
    if let Ok(mut out) = out.lock() {
        let _ = out.write_all(line.as_bytes());
    }
}
