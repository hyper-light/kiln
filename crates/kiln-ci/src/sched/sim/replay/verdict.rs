//! Verdicts: when each commit knew how its run went (W25 step 1).
//!
//! **Direct**: every job of its run got a result of its own (ran, adopted or reused), or was
//! skipped because a need failed. With P3 and later, a job without one is **covered** by the first
//! later result for that job on its lane: a pass, or a failure in the step that was already failing
//! at its base (the last result before it); a new failure instead makes it a suspect, answered when
//! the bisection that took it finishes (**bisected**). A job whose need failed, or was covered by a
//! failure or bisected, is settled with that need, as it would have been skipped. A run's verdict
//! comes when its last job's does.

use std::collections::BTreeMap;

use super::trace::Run;
use super::{End, Outcome, RunEnd, Trace};

/// How a commit got its verdict: the weakest of its jobs'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Direct,
    Covered,
    Bisected,
}

/// When a commit's run had its verdict, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub at: i64,
    pub kind: Kind,
}

pub(super) fn lane(r: &Run) -> String {
    format!("{}|{}|{}", r.workflow, r.event, r.branch)
}

/// A job's result on its lane: (run, when, how it ended, its failed step).
type Found<'a> = (usize, i64, Option<End>, Option<&'a str>);

/// Every run's verdict, if it had one inside the window.
pub fn verdicts(trace: &Trace, out: &Outcome) -> Vec<Option<Verdict>> {
    let mut lanes: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in trace.runs.iter().enumerate() {
        lanes.entry(lane(r)).or_default().push(i);
    }
    trace
        .runs
        .iter()
        .enumerate()
        .map(|(r, run)| {
            let o = out.runs.get(r)?;
            if !out.covers || run.jobs.is_empty() {
                return match o.end {
                    RunEnd::Passed | RunEnd::Failed => o.concluded.map(|at| Verdict { at, kind: Kind::Direct }),
                    _ => None,
                };
            }
            let on_lane = lanes.get(&lane(run)).map(Vec::as_slice).unwrap_or(&[]);
            // Each job's verdict and whether it settles its dependents (a failure, a skip, a
            // bisection), its needs first.
            let mut settled: Vec<Option<(Verdict, bool)>> = vec![None; run.jobs.len()];
            loop {
                let mut moved = false;
                for (j, job) in run.jobs.iter().enumerate() {
                    if settled.get(j).is_some_and(Option::is_some) {
                        continue;
                    }
                    let jo = out.jobs.get(r)?.get(j)?;
                    let needs: Vec<Option<(Verdict, bool)>> = job.needs.iter().filter_map(|n| run.jobs.iter().position(|o| &o.key == n)).map(|i| settled.get(i).copied().flatten()).collect();
                    let w = if let Some(at) = jo.at {
                        Some((Verdict { at, kind: Kind::Direct }, jo.skipped || matches!(jo.end, Some(End::Failed | End::TimedOut))))
                    } else if let Some(f) = needs.iter().flatten().filter(|(_, failed)| *failed).min_by_key(|(v, _)| v.at) {
                        Some(*f)
                    } else if needs.iter().all(Option::is_some) {
                        cover(trace, out, on_lane, r, &job.key)
                    } else {
                        None
                    };
                    if let (Some(w), Some(slot)) = (w, settled.get_mut(j)) {
                        *slot = Some(w);
                        moved = true;
                    }
                }
                if !moved {
                    break;
                }
            }
            let mut v = Verdict { at: run.at, kind: Kind::Direct };
            for w in settled {
                let (w, _) = w?;
                v = Verdict { at: v.at.max(w.at), kind: v.kind.max(w.kind) };
            }
            Some(v)
        })
        .collect()
}

/// The verdict a later result on the lane gives a job of run `r` that had none of its own, and
/// whether it settles the job's dependents (a failure, or a bisection).
fn cover(trace: &Trace, out: &Outcome, on_lane: &[usize], r: usize, key: &str) -> Option<(Verdict, bool)> {
    let results: Vec<Found<'_>> = on_lane
        .iter()
        .filter_map(|&o| {
            let j = trace.runs.get(o)?.jobs.iter().position(|x| x.key == key)?;
            let jo = out.jobs.get(o)?.get(j)?;
            if jo.skipped {
                return None;
            }
            jo.at.map(|t| (o, t, jo.end, jo.failed_step.as_deref()))
        })
        .collect();
    let base = results.iter().filter(|x| x.0 < r).max_by_key(|x| x.0);
    let later = results.iter().filter(|x| x.0 > r).min_by_key(|x| (x.1, x.0))?;
    if later.2 == Some(End::Succeeded) {
        return Some((Verdict { at: later.1, kind: Kind::Covered }, false));
    }
    if base.is_some_and(|b| b.2.is_some_and(|e| !e.passed()) && b.3 == later.3) {
        return Some((Verdict { at: later.1, kind: Kind::Covered }, true));
    }
    out.bisections.iter().find(|b| b.key == key && b.suspects.contains(&r)).and_then(|b| b.done).map(|at| (Verdict { at, kind: Kind::Bisected }, true))
}
