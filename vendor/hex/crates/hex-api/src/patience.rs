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
//!   within it. A wait past it is a run-out, no sample (Karn's point), told once where a person sees it, the process's
//!   log ([`Patience::ran_out`]), and it refuses nothing by itself: the wait goes on following its party by its life,
//!   refused only at the party's end or a reset. No stall of the party's progress is judged: one judged over W*, or at
//!   the slowest gap seen, refuses healthy parties (`docs/research/runtime/learned-waits.md`, decision 9's bound). Its
//!   answer comes in uncensored, and W* rises to it. No factor: the following wait measures how long to wait.
//! - **After a run-out**: the tail it ran into is unknown, the cold start's case, so the next wait follows its party
//!   from its start ([`Patience::ran_out`]) until an answer comes.
//! - **Told**: a run-out is told on the process's stderr in every configuration, one line naming the wait, its W*, how
//!   long it waited and the party it follows from then on ([`Patience::ran_out`]): a node's journal, a service's log, a
//!   test's output. A wait that starts with no W* and whose party could hang alive tells, once as it starts, that it
//!   follows that party ([`Patience::following`]; the review's C-10), so even a cold wait is never silent. A wedged party that stays alive holds its wait, and that line says which, and how long the wait had
//!   gone. The party is named in pieces of a few kinds alone ([`Piece`]), so the line carries nothing a caller holds
//!   that could be large or secret, and the line is bounded ([`LINE_BOUND`]).
//! - **The census**: each wait is named, and every answer it records goes to the census sink as well, where a shell
//!   installs one ([`census`]): the waits census probe's record of each wait's distribution
//!   (`bench/competitive/probes/waits_census.md`), from the code's own timing points, the evidence decision 9's rule is
//!   held to. A wait that ran out tells the census too ([`Patience::ran_out`]). The census disarms no wait: one past W*
//!   follows its party to its answer, uncensored, so the distribution keeps its tail with every wait armed.

use std::io::Write;
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

/// The most bytes a told line takes, its newline among them: POSIX's least `PIPE_BUF`, `{_POSIX_PIPE_BUF}` (IEEE Std
/// 1003.1-2017, `<limits.h>`, "Minimum Values"). A write of at most `PIPE_BUF` bytes to a pipe is never interleaved with
/// another's (write(2), "Pipes and FIFOs"), so a told line, written whole in one write, stays one line on a pipe shared
/// with any other writer, on any POSIX host; Windows gets the same line.
pub const LINE_BOUND: usize = 512;

/// One piece of what a wait past its W* follows, as its told line names it ([`Patience::ran_out`]; ROOT's conditions of
/// 2026-11-14): the code's own words, and what identifies the party, by kind. Nothing else can enter the line, no spec,
/// command, error's words or credential a caller holds. Each piece is cut where it would pass what the line leaves its
/// party, and the cut is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// The code's own words: what the party is, and how the wait follows it.
    Kind(&'static str),
    /// What Hex names it by: a box's id, a container's, a run's.
    Name(&'a str),
    /// A process, by its pid.
    Pid(i64),
    /// Where it is: a cgroup's path, a directory's.
    Path(&'a std::path::Path),
    /// A number that names it: a seed's, a call's, a connection's, a port, an entry's place.
    Number(u64),
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

    /// The wait ran out, `waited` passing with no answer, at least `w_star`, the W* it was bounded by: no sample (Karn's
    /// point), so the slowest seen stays, but the tail is unknown, so the next wait follows its party until an answer
    /// comes. Told once, as W* passes, where a person sees it: a line on the process's stderr naming the wait, its W*, how
    /// long it waited and `party`, what it follows by its life from now on (a pid, a socket, a lock, a channel), in
    /// pieces ([`Piece`]); and to the census where one is installed, as `RanOut(w_star)`. A run-out refuses nothing by
    /// itself: the wait that ran out goes on, following its party past W* by its life, its answer the uncensored sample
    /// W* rises to.
    ///
    /// The line, at most [`LINE_BOUND`] bytes, its integers nanoseconds (ROOT and kiln's metrics owner, 2026-11-14;
    /// kiln counts it by its wait): `hex.wait event=ran_out wait=<name> w_star_ns=<W*> waited_ns=<how long>
    /// party=<its pieces, a JSON string>` ([`crate::event::json_string`]). A party cut at what the line leaves it ends
    /// `[cut N bytes]`, so the cut is told.
    pub fn ran_out(&self, w_star: Duration, waited: Duration, party: &[Piece<'_>]) {
        let line = told_line(self.name, w_star, waited, party);
        self.following.store(true, Ordering::Relaxed);
        // A process with no stderr to write has no log of its own to tell: the census, below, still has it.
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
        if let Some(sink) = CENSUS.get() {
            sink(self.name, Sample::RanOut(w_star));
        }
    }

    /// The wait starts with no W* to bound it (before its first answer, or from a run-out to the next), its party one
    /// that could hang alive (a VM's init on its way to READY): told once, as it starts, where a person sees it, so a party
    /// that never answers is never silent (the review's C-10). A wait site opts in where its party can hang with no W*;
    /// a wait with a W* tells nothing here, its run-out told past W* ([`Patience::ran_out`]). The line, at most
    /// [`LINE_BOUND`] bytes, as the run-out's is: `hex.wait event=following wait=<name> party=<its pieces, a JSON
    /// string>`. Says whether it told.
    pub fn following(&self, party: &[Piece<'_>]) -> bool {
        if self.deadline().is_some() {
            return false;
        }
        self.follows(party);
        true
    }

    /// Tells the line [`Patience::following`] tells, at a site whose W* is its own rather than this wait's learned one
    /// (a guest's boot, bounded by the slowest boot its node has seen; the review's C-9): its site knows it starts with
    /// none, whatever this wait has learned elsewhere.
    pub fn follows(&self, party: &[Piece<'_>]) {
        let line = following_line(self.name, party);
        // A process with no stderr to write has no log of its own to tell.
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
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

    /// How long the next wait lasts: the slowest answer seen; `None` where it follows its party: before any answer,
    /// and from a run-out until the next answer.
    pub fn deadline(&self) -> Option<Duration> {
        if self.following.load(Ordering::Relaxed) {
            return None;
        }
        self.slowest()
    }
}

/// The line a run-out is told by ([`Patience::ran_out`]): its party's pieces in order, each cut where it would pass
/// what the line leaves them (the bound less its other fields and the cut's own words, at a `usize`'s widest count),
/// and a cut told by those words, so no line passes [`LINE_BOUND`] and no cut is silent.
fn told_line(name: &str, w_star: Duration, waited: Duration, party: &[Piece<'_>]) -> String {
    with_party(
        format!(
            "hex.wait event=ran_out wait={name} w_star_ns={} waited_ns={} party=\"",
            w_star.as_nanos(),
            waited.as_nanos()
        ),
        party,
    )
}

/// The line a wait that starts with no W* is told by ([`Patience::following`]), bounded as [`told_line`]'s is.
fn following_line(name: &str, party: &[Piece<'_>]) -> String {
    with_party(
        format!("hex.wait event=following wait={name} party=\""),
        party,
    )
}

/// `line`, its fields to the party's opening quote, ended with the party's pieces and the line's end, cut as
/// [`told_line`] says, so no line passes [`LINE_BOUND`] and no cut is silent.
fn with_party(mut line: String, party: &[Piece<'_>]) -> String {
    let cut_words = |cut: usize| format!(" [cut {cut} bytes]");
    let room = LINE_BOUND
        .saturating_sub(cut_words(usize::MAX).len())
        .saturating_sub("\"\n".len());
    let mut cut = 0usize;
    for piece in party {
        let text: std::borrow::Cow<'_, str> = match piece {
            Piece::Kind(words) => (*words).into(),
            Piece::Name(name) => (*name).into(),
            Piece::Pid(pid) => pid.to_string().into(),
            Piece::Path(path) => path.to_string_lossy(),
            Piece::Number(n) => n.to_string().into(),
        };
        cut = match cut {
            0 => crate::event::push_json_contents(&mut line, &text, room),
            cut => cut.saturating_add(text.len()),
        };
    }
    if cut > 0 {
        line.push_str(&cut_words(cut));
    }
    line.push_str("\"\n");
    line
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
