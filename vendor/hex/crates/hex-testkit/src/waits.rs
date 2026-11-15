//! What the tests' waits take (decision 9 of docs/design/runtime-architecture.md; the person's ruling of 2026-11-13, R03
//! for tests: a wait for a fact waits for the answer). A test's wait for a fact is never refused at its patience: past
//! it, its record or the slowest answer seen for that fact, the run-out is told, once, and the wait goes on to its fact,
//! whose answer is the sample its W* rises to, or to its party's end, which fails the test ([`follow`]). What each
//! patience is:
//! - a fact a census row records takes that row's record, here; a program's end through a `Proc` is followed to it,
//!   as the suites follow one (`hex_api::conformance::end_of`);
//! - a fact one node operation makes within itself takes that operation's record, the relation stated where it is
//!   waited for and tested there (a hook holding a create is bounded by the create's answer);
//! - a fact no recorded operation makes is waited for measuring, named `test.<crate>.<fact>`: for the fact alone until
//!   its first answer, then at the slowest seen ([`hex_core::patience::Patience::measuring`]).
//!
//! The tests run debug builds, so each record here is a debug build's (`bench/competitive/probes/waits_census.md`,
//! run 5, waits-census-20261109T184054Z on adacachy2, at ef259dbe): the largest answer of the debug-build arms, the
//! debug arm and the suites', for the row and every test wait seeded from it, never under a bound one ran out at, named
//! by its row, in nanoseconds as the census keeps them. They seed tests' waits alone ([`Patience::seeded`], a test-only
//! constructor): no production wait is seeded from a record (decision 9). Every wait names its fact's party and fails at
//! the party's end ([`follow`]; the review's C-5): no wait here takes none. Where the census probe runs the suites
//! (`HEX_WAITS_CENSUS`), every answer goes to its file, so the tests' own waits have rows of their own.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use hex_core::patience::Patience;

/// `lifecycle.exec`: a program's run to its end (18.262 ms: `oci_box`'s program run in the suites, of 272, above the
/// debug arm's 18.003 ms of 10,000), here for a program a test runs as a child of its own (`std::process::Child`)
/// rather than through a `Proc`: its end, and what it does on its way there. (A `Proc`'s end is followed to it,
/// `hex_api::conformance::end_of`.)
pub const PROGRAM: Duration = Duration::from_nanos(18_262_060);
/// `node.emptying`: a box's kill to its cgroup's `populated 0` (904.705 ms: the suites', of 14,977, where run 3's
/// record ran out at 255.505 ms), what a test waits on for a killed box's processes to end, its emptying or the end of
/// a process killed with it.
pub const EMPTYING: Duration = Duration::from_nanos(904_705_253);
/// `netns.make`: the pool's refill thread's make of a network namespace (50.825 ms: the debug arm's, of 53,982).
pub const NETNS_MAKE: Duration = Duration::from_nanos(50_825_287);
/// `store.turns`: a record change's wait for the same box's, a held turn's write and its fsyncs (367.733 ms: the
/// suites', of 4,109,178).
pub const STORE_TURN: Duration = Duration::from_nanos(367_732_601);
/// `node.turns`: a create's or delete's wait for the same box's, the one under way's hold of its turn (1,719.743 ms:
/// the suites', of 398,973).
pub const NODE_TURN: Duration = Duration::from_nanos(1_719_743_234);
/// `client.answer.create`: a create's answer over the node's socket (49.798 ms: the debug arm's, of 46,021), which
/// bounds an in-process create's, and every fact a create makes within itself.
pub const CREATE: Duration = Duration::from_nanos(49_798_121);
/// `client.answer.start`: a start's answer (16.121 ms: the debug arm's, of 20,021, above the 15.220 ms the suites ran
/// out at), and every fact a start makes within itself.
pub const START: Duration = Duration::from_nanos(16_120_605);
/// `client.answer.delete`: a delete's answer (706.091 ms: the suites', of 4,209), and every fact a delete makes within
/// itself.
pub const DELETE: Duration = Duration::from_nanos(706_090_511);
/// `client.answer.exec`: an exec's answer (100.781 ms: the suites', of 1,890), once its program is started.
pub const EXEC: Duration = Duration::from_nanos(100_780_659);
/// `node.listing`: a recovery's listing, an entry (137.124 us: the bound run 3's record ran out at in the suites, the
/// least its slowest is), what a restart's recovery takes for each entry of the directories it lists.
pub const LISTING_ENTRY: Duration = Duration::from_nanos(137_124);
/// `seeds.hang`: a seed's answer to a create (652.938 ms: the suites', of 493,529), what a test's wait for a seed, in
/// line or for its answer, takes.
pub const SEEDS: Duration = Duration::from_nanos(652_937_547);

/// An answer a test timed itself, a fact it waited for its own way, told to `wait` and to the census where the probe
/// runs the suites, as [`until`] tells it.
pub fn answered(wait: &Patience, took: Duration) {
    census();
    wait.answered(took);
}

/// The variable naming the census's file, and the one bounding its lines: the node's (`hex_node::census`).
pub const CENSUS_FILE: &str = "HEX_WAITS_CENSUS";
/// The most lines this process writes to the census.
pub const CENSUS_MAX: &str = "HEX_WAITS_CENSUS_MAX";

static OUT: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static MAX: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);

/// The census of the tests' own waits, installed at the first wait where the probe runs the suites: [`CENSUS_FILE`]
/// names its file, [`CENSUS_MAX`] its most lines, as for the node. Unlike the node's, it disarms no wait: a test's
/// patiences keep their bounds, and its answers are only recorded. Where either is unset, or the file won't open, it
/// installs nothing and the waits run as they do without it.
fn census() {
    let out = OUT.get_or_init(|| {
        let file = std::env::var_os(CENSUS_FILE)?;
        let max: u64 = std::env::var(CENSUS_MAX).ok()?.parse().ok()?;
        let out = File::options().create(true).append(true).open(file).ok()?;
        MAX.store(max, Ordering::Relaxed);
        Some(Mutex::new(out))
    });
    if out.is_some() {
        hex_core::patience::census(record);
    }
}

/// One answer or run-out, appended as the census keeps it; past the bound, one line saying the census is full, then
/// nothing.
fn record(name: &'static str, sample: hex_core::patience::Sample) {
    let Some(Some(out)) = OUT.get() else {
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

pub mod follow;

#[cfg(test)]
mod tests;
