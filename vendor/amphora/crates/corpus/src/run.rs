//! A corpus file run on one engine, record by record, as SQLite's runner runs it (bench/corpus/slt.py,
//! `run_file`): the records SQLite runs (its `skipif`/`onlyif`), each statement to its end, each
//! query's answer rendered, sorted and compared with SQLite's. Before the first query after any
//! statement the engine settles into its steady state (its log copied into the file and emptied),
//! so what a query reads, and what it costs, is the database's, not a leftover log's (G2 step 4).

use std::time::{Duration, Instant};

use crate::answer::{Render, Value, answer, compare, expected_values};
use crate::complete::split;
use crate::dialect::Translated;
use crate::slt::{Item, Kind, Record, parse};

/// The engine whose conditions the corpus's records follow: every answer in it is SQLite's.
pub const AS_ENGINE: &str = "sqlite";

/// Why a statement didn't run to its end.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// The engine's error.
    Refused(String),
    /// Its bound came first: the engine stopped it (its interrupt, or its process ended).
    PastBudget,
}

/// One engine, a fresh database per file (the seam every adapter fills: crate::engines).
pub trait Engine {
    /// Runs one statement to its end; its rows, at most `rows` of them kept (past that, the rest
    /// are counted and dropped, and the answer already disagrees), or why it stopped short. When
    /// `until` comes first the engine stops the statement itself (G2 step 7: one record can't hold
    /// a run past its budget) and answers [`Stop::PastBudget`].
    fn run(
        &mut self,
        sql: &str,
        rows: usize,
        until: Option<Instant>,
    ) -> Result<Vec<Vec<Value>>, Stop>;
    /// Copies the log into the database and empties it, stopped at `until` as a statement is
    /// ([`Stop::PastBudget`]); a refusal says the steady state wasn't reached.
    fn settle(&mut self, until: Option<Instant>) -> Result<(), Stop>;
    /// For an engine whose answers can't carry every text whole (a shell writing values between
    /// separators with no escapes: Turso's list mode), how many rows `sql` answers, by the engine's
    /// own count; None for an engine whose answers read whole. A disagreeing answer whose rows read
    /// back as more or fewer than that, or as rows of the wrong width, held a separator or a line
    /// end in its text: it is unreadable, not a disagreement (G2 step 4).
    fn recount(&mut self, _sql: &str, _until: Option<Instant>) -> Option<Result<u64, Stop>> {
        None
    }
    /// Whether stopping a statement ended the engine's database (a shell's process stopped): the
    /// file's later records aren't run (Unreached).
    fn ended(&self) -> bool {
        false
    }
}

/// What became of one record.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Agree,
    /// A query's answer differs: the file's and the engine's.
    Answer {
        want: String,
        got: String,
    },
    /// The engine refused what SQLite runs.
    Refused(String),
    /// The engine ran what SQLite refuses.
    Accepted,
    /// The run's deadline came first: the record wasn't run.
    Unreached,
    /// The record ran past its bound and the engine stopped it.
    PastBudget,
    /// The engine's answer can't be read back as it wrote it (text holding its separators, which
    /// its output doesn't escape): neither an agreement nor a disagreement.
    Unreadable(String),
    /// The record has no translation into the engine's dialect, or it disagrees after a statement
    /// of the file (or its fixture) that has none or that the engine refused in its dialect, which
    /// it may rest on: not a disagreement (G2 step 4).
    Untranslatable(String),
}

/// The time a file's records may take, both from the run's own budget (never a picked number).
#[derive(Clone, Copy, Debug, Default)]
pub struct Limits {
    /// Records not yet begun at it aren't run (Unreached): the fill's end.
    pub deadline: Option<Instant>,
    /// A begun record is stopped this long after it began, or at the deadline if that's sooner: a
    /// record past the whole budget can't be in it.
    pub record: Option<Duration>,
}

impl Limits {
    fn until(&self, began: Instant) -> Option<Instant> {
        let own = self.record.and_then(|r| began.checked_add(r));
        match (own, self.deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

pub struct Ran<'a> {
    pub record: &'a Record,
    pub outcome: Outcome,
    /// Time spent running and comparing it, in nanoseconds.
    pub nanos: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub agree: u64,
    pub disagree: u64,
    pub skipped: u64,
    pub unreached: u64,
    /// Records the engine stopped at their bound.
    pub past_budget: u64,
    /// Settles that failed (the steady state not reached before a query).
    pub unsettled: u64,
    /// Records with no translation into the engine's dialect, or resting on one that has none.
    pub untranslatable: u64,
    /// Answers the engine's output couldn't carry whole (Outcome::Unreadable).
    pub unreadable: u64,
}

/// Runs `text` (after `fixture`'s statements, a file of tables a source's files share) on `engine`,
/// handing each record's outcome to `each`. Records after the deadline aren't run (Unreached), and
/// a record past its bound is stopped by the engine (PastBudget). With `translated`, each record
/// runs as its translation into the engine's dialect says (crate::dialect), and one with none is
/// Untranslatable; so is a disagreement after a statement with none (a table it would have made).
pub fn run_file(
    engine: &mut dyn Engine,
    render: &mut dyn Render,
    text: &str,
    fixture: Option<&str>,
    translated: Option<Translated<'_>>,
    limits: Limits,
    each: &mut dyn FnMut(Ran<'_>),
) -> Result<Counts, String> {
    let mut counts = Counts::default();
    // The first statement the file's records may rest on that didn't run in the engine's dialect.
    let mut untranslated: Option<String> = None;
    if let Some(f) = fixture {
        for item in parse(f)? {
            if let Item::Record(r) = item
                && matches!(r.kind, Kind::Statement { .. })
            {
                let sql = match translated.map(|t| {
                    t.fixture
                        .map_or(Err("the fixture has no translation"), |f| f.get(r.line))
                }) {
                    None => r.sql.as_str(),
                    Some(Ok(sql)) => sql,
                    Some(Err(why)) => {
                        untranslated
                            .get_or_insert_with(|| format!("the fixture's line {}: {why}", r.line));
                        continue;
                    }
                };
                for part in split(sql) {
                    match (engine.run(&part, 0, None), translated) {
                        (Ok(_), _) => {}
                        // Refused in the dialect: the translation's gap or the dialect's, the
                        // file's records' to say (classify.rs), not the run's failure.
                        (Err(e), Some(_)) => {
                            untranslated.get_or_insert_with(|| {
                                format!(
                                    "the fixture's line {}, refused in the dialect: {e:?}",
                                    r.line
                                )
                            });
                        }
                        (Err(e), None) => {
                            return Err(format!("the fixture's line {}: {e:?}", r.line));
                        }
                    }
                }
            }
        }
    }
    let mut threshold = 0usize;
    let mut dirty = true;
    for item in parse(text)? {
        let record = match item {
            Item::HashThreshold(n) => {
                threshold = n;
                continue;
            }
            Item::Halt(conditions) => {
                if conditions.iter().all(|(only, e)| {
                    if *only {
                        e == AS_ENGINE
                    } else {
                        e != AS_ENGINE
                    }
                }) {
                    break;
                }
                continue;
            }
            Item::Record(r) => r,
        };
        if !record.runs_on(AS_ENGINE) {
            counts.skipped = counts.skipped.saturating_add(1);
            continue;
        }
        if engine.ended() || limits.deadline.is_some_and(|d| Instant::now() >= d) {
            counts.unreached = counts.unreached.saturating_add(1);
            each(Ran {
                record: &record,
                outcome: Outcome::Unreached,
                nanos: 0,
            });
            continue;
        }
        let sql = match translated.map(|t| t.file.get(record.line)) {
            None => record.sql.as_str(),
            Some(Ok(sql)) => sql,
            Some(Err(why)) => {
                if matches!(record.kind, Kind::Statement { ok: true }) {
                    untranslated.get_or_insert_with(|| format!("line {}: {why}", record.line));
                }
                counts.untranslatable = counts.untranslatable.saturating_add(1);
                each(Ran {
                    record: &record,
                    outcome: Outcome::Untranslatable(why.to_owned()),
                    nanos: 0,
                });
                continue;
            }
        };
        let began = Instant::now();
        let until = limits.until(began);
        let outcome = match &record.kind {
            Kind::Statement { ok } => {
                dirty = true;
                let ran = split(sql)
                    .iter()
                    .try_for_each(|part| engine.run(part, 0, until).map(drop));
                match (ran, ok) {
                    (Err(Stop::PastBudget), _) => Outcome::PastBudget,
                    (Ok(()), true) | (Err(Stop::Refused(_)), false) => Outcome::Agree,
                    (Ok(()), false) => Outcome::Accepted,
                    (Err(Stop::Refused(e)), true) => Outcome::Refused(e),
                }
            }
            Kind::Query {
                types,
                sort,
                expected,
            } => {
                // The settle is bounded as the record is: a settle past it stops the record.
                let mut settled = Ok(());
                if dirty {
                    settled = match engine.settle(until) {
                        Err(Stop::PastBudget) => Err(Stop::PastBudget),
                        Err(Stop::Refused(_)) => {
                            counts.unsettled = counts.unsettled.saturating_add(1);
                            Ok(())
                        }
                        Ok(()) => Ok(()),
                    };
                    dirty = !record.reads_only();
                }
                // One row past the right answer's values already disagrees.
                let width = types.len().max(1);
                let keep = expected.as_ref().map_or(usize::MAX, |e| {
                    expected_values(e).div_ceil(width).saturating_add(1)
                });
                match settled.and_then(|()| engine.run(sql, keep, until)) {
                    Err(Stop::PastBudget) => Outcome::PastBudget,
                    Err(Stop::Refused(e)) => Outcome::Refused(e),
                    Ok(rows) => {
                        let got = answer(&rows, types, *sort, render)?;
                        match expected {
                            None => Outcome::Agree,
                            Some(want) => match compare(want, &got, threshold) {
                                Ok(()) => Outcome::Agree,
                                Err((want, got)) => {
                                    match misread(engine, sql, &rows, width, keep, until) {
                                        Some(Ok(why)) => Outcome::Unreadable(why),
                                        Some(Err(Stop::PastBudget)) => Outcome::PastBudget,
                                        Some(Err(Stop::Refused(_))) | None => {
                                            Outcome::Answer { want, got }
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
        };
        let outcome = match (outcome, &untranslated) {
            (
                o @ (Outcome::Answer { .. } | Outcome::Refused(_) | Outcome::Accepted),
                Some(after),
            ) => Outcome::Untranslatable(format!(
                "after a statement with no translation ({after}): {o:?}"
            )),
            (o, _) => o,
        };
        // A statement the file rests on, refused in the engine's dialect: it is this record's
        // disagreement (classify.rs says whose), and the later records' disagreements rest on it.
        if translated.is_some()
            && let (Kind::Statement { ok: true }, Outcome::Refused(e)) = (&record.kind, &outcome)
        {
            untranslated.get_or_insert_with(|| {
                format!("line {}, refused in the dialect: {e}", record.line)
            });
        }
        let nanos = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if outcome == Outcome::Agree {
            counts.agree = counts.agree.saturating_add(1);
        } else if outcome == Outcome::PastBudget {
            counts.past_budget = counts.past_budget.saturating_add(1);
        } else if matches!(outcome, Outcome::Untranslatable(_)) {
            counts.untranslatable = counts.untranslatable.saturating_add(1);
        } else if matches!(outcome, Outcome::Unreadable(_)) {
            counts.unreadable = counts.unreadable.saturating_add(1);
        } else {
            counts.disagree = counts.disagree.saturating_add(1);
        }
        each(Ran {
            record: &record,
            outcome,
            nanos,
        });
    }
    Ok(counts)
}

/// Whether a disagreeing answer was misread (Engine::recount): its rows aren't all `width` wide
/// (a separator in a text), or there are more or fewer of them than the engine counts (a line end
/// in a text). `kept` is how many rows were kept: as many read means more may have been dropped.
/// None when the engine's answers read whole, or when the count itself wasn't answered.
fn misread(
    engine: &mut dyn Engine,
    sql: &str,
    rows: &[Vec<Value>],
    width: usize,
    kept: usize,
    until: Option<Instant>,
) -> Option<Result<String, Stop>> {
    let counted = match engine.recount(sql, until)? {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    if let Some(r) = rows.iter().find(|r| r.len() != width) {
        return Some(Ok(format!(
            "a row read as {} values, not {width}: a text holding the output's separator",
            r.len()
        )));
    }
    let read = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let all_read = rows.len() < kept;
    if (all_read && read != counted) || (!all_read && counted < read) {
        return Some(Ok(format!(
            "{read} rows read, {counted} counted: a text holding a line end"
        )));
    }
    None
}
