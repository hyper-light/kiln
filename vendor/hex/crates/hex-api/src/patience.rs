//! How long a wait for an answer lasts (decision 9 of docs/design/runtime-architecture.md; Ada's rules of 2026-11-06
//! and 2026-11-13: no picked constant, and no value measured on our machines and baked in, per profile, OS or build:
//! "citing a number doesn't count as derived; an estimator of what's seen does"). Pure: the shell times each answer on
//! its monotonic clock and reports it ([`Patience::answered`]), and asks how long the next wait lasts
//! ([`Patience::deadline`]). One type for every wait the node times; shared by threads as it is (`&self`, atomic
//! words), so a static may hold it.
//!
//! - **Learned**: once this process has an answer, a wait's bound is the slowest answer it has seen, a running maximum
//!   that only rises: the least bound of least expected cost per call where no answer fails to come, as the waits
//!   census measures them (decision 9). No factor, no margin: past the slowest of `n` answers no bound holds whatever
//!   their distribution (Wilks, 1941), and the next answer outruns them with chance at most `1 / (n + 1)`.
//! - **Following** (the cold start): before its first answer a wait has no bound ([`Patience::deadline`] is `None`). It
//!   follows its party by its life: it lasts while the party lives, and is refused, typed, at the party's death or a
//!   reset. No record is read: a record is another machine's answers, which the waits census keeps as evidence of the
//!   method, never as a seed.
//! - **Past W*** (the person's ruling of 2026-11-13): W*, the slowest answer seen, is a bound only while answers come
//!   within it. A wait past it is a run-out, no sample (Karn's point), told to the census ([`Patience::ran_out`]), and
//!   it refuses nothing by itself: the wait goes on following its party by its life, refused only at the party's end
//!   or a reset. No stall of the party's progress is judged: one judged over W*, or at the slowest gap seen, refuses
//!   healthy parties (`docs/research/runtime/learned-waits.md`, decision 9's bound). Its answer comes in uncensored,
//!   and W* rises to it. No factor: the following wait measures how long to wait.
//! - **After a run-out**: the tail it ran into is unknown, the cold start's case, so the next wait follows its party
//!   from its start ([`Patience::ran_out`]) until an answer comes.
//! - **The census**: each wait is named, and every answer it records goes to the census sink as well, where a shell
//!   installs one ([`census`]): the waits census probe's record of each wait's distribution
//!   (`bench/competitive/probes/waits_census.md`), from the code's own timing points, the evidence decision 9's rule is
//!   held to. A wait that ran out for a slow answer tells the census too ([`Patience::ran_out`]). Where the census
//!   runs, every wait follows its party, unarmed ([`measure_all`]), so the distribution keeps its tail.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

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

/// What a following wait's party is doing as its shell looks at it (decision 9): alive, so its answer may yet come; or
/// ended (it died or finished, or its connection was reset or closed), so nothing can change its answer now. A wait
/// past W* follows its party by this alone ([module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Party {
    /// Alive: its answer may yet come.
    Live,
    /// Ended: nothing can change its answer now.
    Ended,
}

/// A wait's bound, learned from its answers, following its party where it has none ([module docs](self)).
#[derive(Debug)]
pub struct Patience {
    /// The wait's name, as the census records it: dotted, as the node's event targets are.
    name: &'static str,
    /// The slowest answer this process has seen, in nanoseconds and one; 0 before any.
    seen: AtomicU64,
    /// Whether the next wait follows its party though an answer was seen: from a run-out until the next answer.
    following: AtomicBool,
}

impl Patience {
    /// The wait `name`, with no answer yet: it follows its party until its first answer.
    pub const fn measuring(name: &'static str) -> Self {
        Self {
            name,
            seen: AtomicU64::new(0),
            following: AtomicBool::new(false),
        }
    }

    /// For tests alone (cfg(test), or the `test-seeds` feature, which only dev-dependencies turn on: hex-core's own
    /// tests', and hex-testkit's through hex-core's, hex-testkit a dev-dependency everywhere): the wait `name` as it
    /// would be once it had seen an answer of `slowest`. No production wait is seeded (decision 9).
    #[cfg(any(test, feature = "test-seeds"))]
    pub const fn seeded(name: &'static str, slowest: Duration) -> Self {
        Self {
            name,
            seen: AtomicU64::new(stored(slowest)),
            following: AtomicBool::new(false),
        }
    }

    /// An answer came `took` after its wait began: this process's slowest if it is slower than any before, the wait
    /// bounded again by the slowest, and told to the census where one is installed.
    pub fn answered(&self, took: Duration) {
        self.learn(took);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::Answered(took));
        }
    }

    /// AC-T7.4.1: a completed typed refusal keeps exactly the same online maximum/deadline update
    /// as an answer. Only the optional census distinguishes it from successful completions.
    pub fn refused(&self, took: Duration) {
        self.learn(took);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::Refused(took));
        }
    }

    fn learn(&self, took: Duration) {
        self.seen.fetch_max(stored(took), Ordering::Relaxed);
        self.following.store(false, Ordering::Relaxed);
    }

    /// The wait ran out, `after` passing with no answer: no sample (Karn's point), so the slowest seen stays, but the
    /// tail is unknown, so the next wait follows its party until an answer comes; told to the census where one is
    /// installed. A run-out refuses nothing by itself: the wait that ran out goes on, following its party past W* by its
    /// life, its answer the uncensored sample W* rises to.
    pub fn ran_out(&self, after: Duration) {
        self.following.store(true, Ordering::Relaxed);
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::RanOut(after));
        }
    }

    /// The wait's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The slowest answer this process has seen: `None` before any.
    pub fn slowest(&self) -> Option<Duration> {
        self.seen
            .load(Ordering::Relaxed)
            .checked_sub(1)
            .map(Duration::from_nanos)
    }

    /// How long the next wait lasts: the slowest answer seen; `None` where it follows its party: before
    /// any answer, from a run-out until the next answer, and where the census measures every wait ([`measure_all`]).
    pub fn deadline(&self) -> Option<Duration> {
        if self.following.load(Ordering::Relaxed) || MEASURE_ALL.load(Ordering::Relaxed) {
            return None;
        }
        self.slowest()
    }
}

/// `d` in nanoseconds and one, saturating: the form the word keeps, so 0 is none. Const for the constructors.
const fn stored(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(d.subsec_nanos() as u64)
        .saturating_add(1)
}

#[cfg(test)]
mod tests;
