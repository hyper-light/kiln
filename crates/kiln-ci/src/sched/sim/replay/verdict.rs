//! Verdicts: when each commit knew how its run went (W25 step 1; W28-R53).
//!
//! Without coverage (P0 to P2) a commit's verdict is direct: its own run concluded. From P3 on,
//! [`crate::verdict`] decides, with each lane a [`Book`]: its commits are the lane's runs in the
//! order they arrived; each job is a check of its own, its unit the job; a commit affects the jobs
//! whose input keys it changed (the lane's first, all of its run's jobs); results are the jobs'
//! (ran, adopted or reused, or skipped when a need failed, with the failed step as a failure's
//! signature), and culprit findings the bisections'. A commit's verdict comes when it is answered,
//! direct, covered or culprit, and stays answered from then on: a covered verdict a later result
//! takes back (a nearer commit's new failure) counts from when it was answered again.

use std::collections::BTreeMap;

use super::trace::Run;
use super::{End, Outcome, RunEnd, Trace};
use crate::verdict::{Affects, Book, Check, Commit, Finding, Found, How, Outcome as Ended, Seen, Spec};

pub use crate::verdict::Kind;

/// When a commit's verdict came, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub at: i64,
    pub kind: Kind,
}

pub(super) fn lane(r: &Run) -> String {
    format!("{}|{}|{}", r.workflow, r.event, r.branch)
}

/// A job's check: the job, as a unit of its workflow.
pub(super) fn check(run: &Run, key: &str) -> Check {
    Check::unit(&run.workflow, key)
}

/// The checks of a run's jobs, each with the jobs it needs.
pub(super) fn specs(run: &Run) -> Vec<Spec> {
    run.jobs.iter().map(|j| Spec { check: check(run, &j.key), needs: j.needs.iter().map(|n| check(run, n)).collect() }).collect()
}

/// What run `r`'s commit could affect: the jobs whose input keys it changed since `prev`, the run
/// before it on its lane; the lane's first run, all of its jobs.
pub(super) fn affects(trace: &Trace, r: usize, prev: Option<usize>) -> Affects {
    let Some(run) = trace.runs.get(r) else { return Affects::All };
    let key = |run: &Run, class: &str| trace.commits.get(run.commit).and_then(|c| c.keys.get(class)).cloned();
    let before = prev.and_then(|p| trace.runs.get(p));
    Affects::Units(
        run.jobs
            .iter()
            .filter(|j| {
                let class = j.class.clone().unwrap_or_else(|| run.workflow.clone());
                let now = key(run, &class);
                now.is_none() || before.is_none_or(|b| key(b, &class) != now)
            })
            .map(|j| j.key.clone())
            .collect(),
    )
}

/// A job's end as a result's outcome: a failure's signature is its failed step.
pub(super) fn ended(end: End, failed_step: Option<&str>) -> Ended {
    match end {
        End::Succeeded => Ended::Passed,
        End::Failed => Ended::Failed { signature: failed_step.unwrap_or("failed").to_owned() },
        End::TimedOut => Ended::Failed { signature: failed_step.unwrap_or("timed out").to_owned() },
    }
}

pub(super) fn how(h: super::How) -> How {
    match h {
        super::How::Ran => How::Ran,
        super::How::Adopted => How::Adopted,
        super::How::Reused => How::Reused,
    }
}

/// Each lane's runs in the order they arrived: by when, then by place in the trace (the replay's
/// own order).
pub fn lanes(trace: &Trace) -> BTreeMap<String, Vec<usize>> {
    let mut by: Vec<usize> = (0..trace.runs.len()).collect();
    by.sort_by_key(|&r| (trace.runs.get(r).map_or(0, |x| x.at), r));
    let mut lanes: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for r in by {
        if let Some(run) = trace.runs.get(r) {
            lanes.entry(lane(run)).or_default().push(r);
        }
    }
    lanes
}

/// A lane as the core keeps it: its runs as commits, their jobs as checks.
pub(super) fn book(trace: &Trace, runs: &[usize]) -> Book {
    let mut book = Book::default();
    let mut prev = None;
    for &r in runs {
        let Some(run) = trace.runs.get(r) else { continue };
        for s in specs(run) {
            book.spec(s);
        }
        book.push(Commit { id: format!("run {r}"), at: run.at, affects: affects(trace, r, prev) });
        prev = Some(r);
    }
    book
}

/// One lane's commits' verdicts as they came: for each place, when its verdict was last answered,
/// if it is at the end, and its kind then.
pub fn lane_verdicts(trace: &Trace, out: &Outcome, runs: &[usize]) -> Vec<Option<Verdict>> {
    lane_detail(trace, out, runs, None).into_iter().map(|d| d.verdict).collect()
}

/// A commit's verdict as the replay saw it come.
#[derive(Debug, Clone)]
pub struct Detail {
    /// When it was last answered, if it is at the end, and its kind then.
    pub verdict: Option<Verdict>,
    /// Its verdict `hour` after its push, when it wasn't answered by then.
    pub late: Option<crate::verdict::Verdict>,
    /// Its verdict at the end of the window.
    pub last: Option<crate::verdict::Verdict>,
}

/// One lane's commits' verdicts as they came, with each one's verdict `hour` seconds after its
/// push when it wasn't answered by then.
pub fn lane_detail(trace: &Trace, out: &Outcome, runs: &[usize], hour: Option<i64>) -> Vec<Detail> {
    let mut book = book(trace, runs);
    let place: BTreeMap<usize, usize> = runs.iter().enumerate().map(|(p, &r)| (r, p)).collect();
    // What arrives, by when: results, then culprit findings.
    let mut events: Vec<(i64, u8, usize, Event)> = Vec::new();
    for (p, &r) in runs.iter().enumerate() {
        let Some(run) = trace.runs.get(r) else { continue };
        for (j, job) in run.jobs.iter().enumerate() {
            let Some(jo) = out.jobs.get(r).and_then(|v| v.get(j)) else { continue };
            let Some(at) = jo.at else { continue };
            let outcome = if jo.skipped {
                Ended::Skipped
            } else {
                match jo.end {
                    Some(end) => ended(end, jo.failed_step.as_deref()),
                    None => continue,
                }
            };
            let seen = Seen { commit: p, check: check(run, &job.key), outcome, how: jo.how.map_or(How::Ran, how), at };
            events.push((at, 0, events.len(), Event::Seen(seen)));
        }
    }
    for b in &out.bisections {
        let (Some(done), Some(&first)) = (b.done, b.suspects.first()) else { continue };
        let Some(run) = trace.runs.get(first) else { continue };
        if !place.contains_key(&first) {
            continue;
        }
        let suspects: Vec<usize> = b.suspects.iter().filter_map(|r| place.get(r).copied()).collect();
        let finding = match (b.culprit.and_then(|c| place.get(&c)), b.flake) {
            (Some(&commit), _) => Finding::Culprit { commit },
            (None, true) => Finding::NoCulprit,
            (None, false) => Finding::Inconclusive { why: "the bisection couldn't tell".into() },
        };
        events.push((done, 1, events.len(), Event::Found(Found { check: check(run, &b.key), suspects, finding, at: done })));
    }
    events.sort_by_key(|(at, order, seq, _)| (*at, *order, *seq));

    // Each commit: since when it has been answered, if it is.
    let mut since: Vec<Option<i64>> = book.verdicts().iter().map(|v| v.kind.answered().then_some(v.at.unwrap_or(0))).collect();
    // The hour after each push, soonest first: each commit's verdict then, when not answered.
    let mut due: Vec<(i64, usize)> = hour.map(|h| book.commits().iter().enumerate().map(|(c, x)| (x.at + h, c)).collect()).unwrap_or_default();
    due.sort_unstable();
    let mut due = due.into_iter().peekable();
    let mut late: Vec<Option<crate::verdict::Verdict>> = vec![None; book.len()];
    let mut i = 0;
    while i < events.len() {
        let at = events.get(i).map_or(0, |e| e.0);
        while let Some(&(t, c)) = due.peek()
            && t < at
        {
            if since.get(c).is_some_and(Option::is_none)
                && let Some(slot) = late.get_mut(c)
            {
                *slot = book.verdict(c);
            }
            due.next();
        }
        let (mut lo, mut hi) = (usize::MAX, 0);
        while let Some((t, _, _, ev)) = events.get(i)
            && *t == at
        {
            let range = match ev {
                Event::Seen(s) => book.see(s.clone()),
                Event::Found(f) => book.found(f.clone()),
            };
            if !range.is_empty() {
                (lo, hi) = (lo.min(range.start), hi.max(range.end));
            }
            i += 1;
        }
        for c in lo..hi {
            let answered = book.verdict(c).is_some_and(|v| v.kind.answered());
            if let Some(s) = since.get_mut(c) {
                match (answered, *s) {
                    (true, None) => *s = Some(at.max(book.commits().get(c).map_or(at, |x| x.at))),
                    (false, Some(_)) => *s = None,
                    _ => {}
                }
            }
        }
    }
    for (_, c) in due {
        if since.get(c).is_some_and(Option::is_none)
            && let Some(slot) = late.get_mut(c)
        {
            *slot = book.verdict(c);
        }
    }
    let end = book.verdicts();
    since.iter().zip(end).zip(late).map(|((s, v), late)| Detail { verdict: s.map(|at| Verdict { at, kind: v.kind }), late, last: Some(v) }).collect()
}

/// What a lane's book is given, in time order.
#[derive(Debug, Clone)]
enum Event {
    Seen(Seen),
    Found(Found),
}

/// Every run's verdict, if it had one inside the window.
pub fn verdicts(trace: &Trace, out: &Outcome) -> Vec<Option<Verdict>> {
    if !out.covers {
        return trace
            .runs
            .iter()
            .enumerate()
            .map(|(r, _)| {
                let o = out.runs.get(r)?;
                match o.end {
                    RunEnd::Passed | RunEnd::Failed => o.concluded.map(|at| Verdict { at, kind: Kind::Direct }),
                    _ => None,
                }
            })
            .collect();
    }
    let mut v: Vec<Option<Verdict>> = vec![None; trace.runs.len()];
    for runs in lanes(trace).values() {
        for (&r, x) in runs.iter().zip(lane_verdicts(trace, out, runs)) {
            if let Some(slot) = v.get_mut(r) {
                *slot = x;
            }
        }
    }
    v
}
