//! How long a wait for an answer lasts (decision 9 of docs/design/runtime-architecture.md; Ada's 2026-11-06 rule, no
//! picked constants, and the lead's failure-bound rule of the same day). Pure: the shell times each answer on its
//! monotonic clock and reports it ([`Patience::answered`]), and asks how long the next wait lasts
//! ([`Patience::deadline`]). One type for every wait the node times, the seeds', the turns', an emptying's, a
//! listing's and the network check's; shared by threads as it is (`&self`, one atomic word), so a static may hold it.
//!
//! - **Seeded from a record**: before this node has an answer of its own, a wait's bound is [`TIMES_SLOWEST`] times
//!   the slowest answer in that wait's recorded distribution for the host class, a named record cited where the
//!   wait is made ([`Patience::seeded`]). No calibration read, no unbounded first wait.
//! - **Learned**: the bound is [`TIMES_SLOWEST`] times the larger of the record's slowest and this node's slowest
//!   answer, a running maximum, so it only rises. A wait runs out only for an answer more than twice slower than any
//!   recorded or seen, or than the least a run-out proved one.
//! - **Measuring**: a wait whose distribution has no record yet says so ([`Patience::measuring`], with the record to
//!   come named at the wait). It is never armed: its answers are only recorded, for the census that seeds it, so no
//!   value is picked for it and no first answer becomes its bound. A tree with a measuring wait doesn't land.
//! - **By build profile**: a wait's record is of the build it runs in ([`Patience::by_profile`]): a release build
//!   is seeded from the release census, a debug build (the suites') from the debug one, measuring until it has
//!   one, since a debug build's answers are slower and the release record isn't theirs (the lead, 2026-11-08). Being
//!   no faster, a debug build's slowest is at least the release record's, whose cohorts (a cold start the debug
//!   campaign never met) bound it from below: its seed is the larger of the two records.
//! - **Karn's point**: a wait that ran out is no sample, nor is an answer from a server taken for dead or hung; the
//!   shell reports only answers it waited for and got. A run-out for a slow answer still proves that answer slower
//!   than the bound it ran out at, so this node's slowest is at least that bound from then, and its next wait twice
//!   it: the timer backs off in process (RFC 6298 §5.5), rather than every slow call failing at one bound until a new
//!   record lands ([`Patience::ran_out`]).
//! - **The census**: each wait is named, and every answer it records goes to the census sink as well, where a shell
//!   installs one ([`census`]): the waits census probe's record of each wait's distribution
//!   (`bench/competitive/probes/waits_census.md`), from the code's own timing points. A wait that ran out for a slow
//!   answer tells the census too ([`Patience::ran_out`]): no sample, but its slowest is at least the bound it ran out
//!   at, which the census's seed rule takes, so a wait the census saw run out has its patience raised past that bound
//!   (Karn's point, made learned; the lead, 2026-11-09).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How many times the slowest answer a wait lasts: twice, so a wait runs out only for an answer more than twice
/// slower than any recorded or seen. The lead's failure-bound rule (2026-11-06), stated here.
pub const TIMES_SLOWEST: u32 = 2;

/// What a wait tells the census: an answer it got, the time after its wait began, or that it ran out, the bound that
/// passed with no answer, the least its slowest is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    Answered(Duration),
    RanOut(Duration),
    /// A completed typed refusal: learned exactly as an answer, separately marked for census analysis.
    Refused(Duration),
}

/// Where every named wait's answers and run-outs go besides its own maximum, once a shell installs it: none unless one
/// does.
static CENSUS: OnceLock<fn(&'static str, Sample)> = OnceLock::new();

/// Whether every wait in this process measures, unarmed: the waits census's, so it records the answers past a bound,
/// which a wait that runs out never gives (Karn's point).
static MEASURE_ALL: AtomicBool = AtomicBool::new(false);

/// Every wait in this process measures from now on, never armed, its answers only recorded: the waits census's
/// (`bench/competitive/probes/waits_census.md`), whose shell calls it as it installs its sink, so the distribution it
/// records has its tail.
pub fn measure_all() {
    MEASURE_ALL.store(true, Ordering::Relaxed);
}

/// Whether every wait in this process measures ([`measure_all`]): the waits census runs, so a wait whose measure has
/// no record yet may measure too, its first record taken there (the Shards image class's waits).
pub fn measures_all() -> bool {
    MEASURE_ALL.load(Ordering::Relaxed)
}

/// Installs `sink` to be told every answer every wait records, and every run-out, by the wait's name: the waits census
/// probe's. Only the first install takes; says whether this one did.
pub fn census(sink: fn(&'static str, Sample)) -> bool {
    CENSUS.set(sink).is_ok()
}

/// One sample as the census's file keeps it, an answer `name nanos`, refusal `name !nanos`, or run-out `name >nanos`, and a newline: the
/// line every census sink writes (the node's, `hex_node::census`, and the tests', `hex_testkit::waits`), so
/// `waits_census.py` reads one form.
pub fn census_line(name: &str, sample: Sample) -> String {
    match sample {
        Sample::Answered(took) => format!("{name} {}\n", took.as_nanos()),
        Sample::RanOut(after) => format!("{name} >{}\n", after.as_nanos()),
        Sample::Refused(took) => format!("{name} !{}\n", took.as_nanos()),
    }
}

/// The line a census sink writes once it has written its `max` lines, then nothing more: the record says it was
/// cut (CLAUDE.md §3), and `waits_census.py` says so for the arm.
pub fn census_full_line(max: u64) -> String {
    format!("census full at {max} lines\n")
}

/// A wait's bound, seeded from a record and learned from its answers ([module docs](self)).
#[derive(Debug)]
pub struct Patience {
    /// The wait's name, as the census records it: dotted, as the node's event targets are.
    name: &'static str,
    /// The record's slowest answer, in nanoseconds and one; 0 while measuring.
    recorded: u64,
    /// This node's slowest answer, or the least a run-out proved one, in nanoseconds and one; 0 before any.
    seen: AtomicU64,
}

impl Patience {
    /// The wait `name`, whose recorded distribution's slowest answer for this host class is `slowest`, cited at the
    /// caller.
    pub const fn seeded(name: &'static str, slowest: Duration) -> Self {
        Self {
            name,
            recorded: stored(slowest),
            seen: AtomicU64::new(0),
        }
    }

    /// The wait `name` seeded from its record for this build's profile: `release`'s slowest answer in a release build,
    /// `debug`'s in a debug one, never under `release`'s (`None`: measuring, until the census's debug arm records it),
    /// each cited at the caller. A debug build's answers are no faster than a release build's under the same
    /// conditions, so the release record's cohorts bound its slowest from below (AC-T7.4.1: macOS's guest hello, whose
    /// debug campaign met no cold first hello, ran out at a tenth of the release record). The choice is made as the
    /// wait is built, so a release build holds only its release seed.
    pub const fn by_profile(
        name: &'static str,
        release: Duration,
        debug: Option<Duration>,
    ) -> Self {
        if cfg!(debug_assertions) {
            match debug {
                Some(slowest) if stored(slowest) < stored(release) => Self::seeded(name, release),
                Some(slowest) => Self::seeded(name, slowest),
                None => Self::measuring(name),
            }
        } else {
            Self::seeded(name, release)
        }
    }

    /// The wait `name`, with no record yet (MEASURING, the record to come named at the caller): never armed, its
    /// answers only recorded.
    pub const fn measuring(name: &'static str) -> Self {
        Self {
            name,
            recorded: 0,
            seen: AtomicU64::new(0),
        }
    }

    /// An answer came `took` after its wait began: this node's slowest if it is slower than any before, and told
    /// to the census where one is installed.
    pub fn answered(&self, took: Duration) {
        self.seen.fetch_max(stored(took), Ordering::Relaxed);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::Answered(took));
        }
    }

    /// AC-T7.4.1: a completed typed refusal keeps exactly the same online maximum/deadline update
    /// as an answer. Only the optional census distinguishes it from successful completions.
    pub fn refused(&self, took: Duration) {
        self.seen.fetch_max(stored(took), Ordering::Relaxed);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::Refused(took));
        }
    }

    /// The wait ran out, `after` passing with no answer: no sample (Karn's point), but the answer is slower than
    /// `after`, so this node's slowest is at least that from now and its next wait twice it (RFC 6298 §5.5's back-off);
    /// told to the census too where one is installed, whose seed rule takes the same least. Said where the answer is
    /// slow, never where its server is taken for dead or hung (a seed's hang bound): that says nothing of how long an
    /// answer takes.
    pub fn ran_out(&self, after: Duration) {
        self.seen.fetch_max(stored(after), Ordering::Relaxed);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::RanOut(after));
        }
    }

    /// The wait's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The slowest answer, the record's or this node's (or the least a run-out proved), whichever is slower: `None`
    /// while measuring with none seen.
    pub fn slowest(&self) -> Option<Duration> {
        let slowest = self.recorded.max(self.seen.load(Ordering::Relaxed));
        slowest.checked_sub(1).map(Duration::from_nanos)
    }

    /// How long the next wait lasts: [`TIMES_SLOWEST`] times [`Patience::slowest`], `None` for a measuring wait.
    pub fn deadline(&self) -> Option<Duration> {
        if self.recorded == 0 || MEASURE_ALL.load(Ordering::Relaxed) {
            return None;
        }
        self.slowest()
            .map(|slowest| slowest.saturating_mul(TIMES_SLOWEST))
    }
}

/// `d` in nanoseconds and one, saturating: the form both words keep, so 0 is none. Const for the constructors.
const fn stored(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(d.subsec_nanos() as u64)
        .saturating_add(1)
}

#[cfg(test)]
mod tests;
