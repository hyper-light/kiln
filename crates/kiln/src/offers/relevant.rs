//! What counts as a change worth telling (docs/reach.md §2.2, AUDIT O2): a runner's hint changes
//! the offers view only when it changes something a person or an agent would decide by. Every
//! sample moves the measured room a little; that alone changes nothing. A hint is a change when,
//! against what was last shown:
//!
//! - the runner's state (taking, draining, strained and why), its budget, or its line's counts by
//!   class differ;
//! - its answer for some ask size known (one it quotes, one it quoted, one the fleet's waiting
//!   work asks) flips: now, waits (and its reason), never;
//! - an expected start moves enough to change what's shown of it, at the granularity it's shown
//!   at ([`shown`]).
//!
//! No threshold here is picked: the answers are the runner's own admission's, or follow from its
//! room and budget; a start's granularity is the clock unit its distance is told in.

use serde::Serialize;

use super::hint::{Amounts, Hint, Quoted, Taking};

/// Milliseconds in each clock unit a start is shown in.
const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// The clock unit a duration is told in: the largest of seconds, minutes, hours and days that it
/// holds once whole (so "40 s", "3 min", "2 h", "5 d").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "OffersUnit"))]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    S,
    Min,
    H,
    D,
}

impl Unit {
    /// The unit `ms` is told in.
    pub fn of(ms: u64) -> Unit {
        if ms < MINUTE {
            Unit::S
        } else if ms < HOUR {
            Unit::Min
        } else if ms < DAY {
            Unit::H
        } else {
            Unit::D
        }
    }

    /// Its length in milliseconds.
    pub fn ms(self) -> u64 {
        match self {
            Unit::S => SECOND,
            Unit::Min => MINUTE,
            Unit::H => HOUR,
            Unit::D => DAY,
        }
    }
}

/// A start as it's shown: so many whole units after the moment it was said (`from`, the server's
/// clock), which the view states with it, so it holds without being said again as time passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "OffersShown"))]
pub struct Shown {
    pub unit: Unit,
    /// How many whole units after `from`.
    pub count: u64,
    /// When it was said, in the server's clock (unix ms).
    pub from: i64,
}

/// A start `start_in` ms after `from`, as it's shown.
pub fn shown(from: i64, start_in: u64) -> Shown {
    let unit = Unit::of(start_in);
    Shown { unit, count: start_in / unit.ms(), from }
}

/// A runner's hint as the server heard it: what it said, and when the server heard it (its own
/// clock, unix ms): the hint's durations count from there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Said {
    pub hint: Hint,
    pub heard: i64,
}

/// An answer for one ask size, as the view shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub kind: Kind,
    /// When it would start (soonest, latest), as shown, for one that waits and has an estimate.
    pub soonest: Option<Shown>,
    pub latest: Option<Shown>,
    /// The runner didn't quote this size: the answer follows from its room and budget alone.
    pub derived: bool,
}

/// What an answer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Now,
    Waits { reason: String },
    Never { why: String },
}

/// Whether `ask` is within `of` in every dimension.
pub fn within(ask: &Amounts, of: &Amounts) -> bool {
    ask.iter().zip(of).all(|(a, o)| a <= o)
}

/// The dimensions' names, for a never's words.
const NAMES: [&str; 4] = ["CPUs", "memory", "disk", "GPUs"];

impl Said {
    /// Its answer for `ask`, its starts shown counting from `from` (what's shown now, or what
    /// would be, from the same moment, so two hints' starts compare as their display does).
    pub fn answer(&self, ask: &Amounts, from: i64) -> Answer {
        let h = &self.hint;
        let start = |d: Option<u64>| d.map(|d| shown(from, u64::try_from(self.heard.saturating_sub(from)).unwrap_or(0).saturating_add(d)));
        match h.quotes.iter().find(|q| q.ask == *ask).map(|q| &q.answer) {
            Some(Quoted::Now) => Answer { kind: Kind::Now, soonest: None, latest: None, derived: false },
            Some(Quoted::Waits { reason, soonest_in, latest_in }) => Answer { kind: Kind::Waits { reason: reason.clone() }, soonest: start(*soonest_in), latest: start(*latest_in), derived: false },
            Some(Quoted::Never { why }) => Answer { kind: Kind::Never { why: why.clone() }, soonest: None, latest: None, derived: false },
            None => {
                // Not quoted: never past its budget; now within its room while it takes work; else
                // it waits for room, when not known.
                let kind = match ask.iter().zip(&h.budget).position(|(a, b)| a > b) {
                    Some(d) => Kind::Never { why: format!("past its budget of {}", NAMES.get(d).copied().unwrap_or("room")) },
                    None if h.state == Taking::Taking && within(ask, &h.room) => Kind::Now,
                    None => Kind::Waits { reason: match &h.state {
                        Taking::Taking => "room".into(),
                        Taking::Draining => "draining".into(),
                        Taking::Strained { .. } => "strained".into(),
                    } },
                };
                Answer { kind, soonest: None, latest: None, derived: true }
            }
        }
    }
}

/// Whether `new` changes what's shown of the runner against `shown` (what was last told), for the
/// ask sizes `sizes` the fleet's waiting work has besides the two hints' own quotes. O(quotes +
/// sizes); nothing in it reads the room but through an answer.
pub fn relevant<'a>(shown: &'a Said, new: &'a Said, sizes: impl IntoIterator<Item = &'a Amounts>) -> bool {
    let (a, b) = (&shown.hint, &new.hint);
    if a.state != b.state || a.budget != b.budget || a.queued != b.queued {
        return true;
    }
    let from = shown.heard;
    let flips = |ask: &Amounts| {
        let (x, y) = (shown.answer(ask, from), new.answer(ask, from));
        x.kind != y.kind || x.soonest != y.soonest || x.latest != y.latest
    };
    a.quotes.iter().chain(&b.quotes).map(|q| &q.ask).chain(sizes).any(flips)
}
