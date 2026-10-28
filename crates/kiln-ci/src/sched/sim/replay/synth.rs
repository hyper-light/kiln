//! Synthetic traces for the replay (W25 step 1): a lane of pushes at a steady rate, each a run of
//! one job of a fixed length whose inputs every push changes.

use std::collections::BTreeMap;

use super::trace::{Commit, Job, Pool, Run, Trace};

/// A lane: a push every `every` seconds until `until`, each a run of `run` seconds on one of
/// `runners` runners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lane {
    pub every: i64,
    pub run: i64,
    pub until: i64,
    pub runners: u32,
}

/// The lane as a trace: its runs in one concurrency group that cancels the run in progress (as
/// kiln's own workflows do today), each push a new input key.
pub fn lane(l: &Lane) -> Trace {
    let n = if l.every > 0 { (l.until + l.every - 1) / l.every } else { 0 };
    let n = usize::try_from(n).unwrap_or(0);
    let mut commits = Vec::with_capacity(n);
    let mut runs = Vec::with_capacity(n);
    for i in 0..n {
        let at = i64::try_from(i).unwrap_or(0) * l.every;
        commits.push(Commit { keys: BTreeMap::from([("lane".to_owned(), format!("{i:04}"))]) });
        let job = Job { key: "test".into(), labels: vec!["runner".into()], class: Some("lane".into()), queued: Some(at), started: Some(at), finished: Some(at + l.run), status: "succeeded".into(), ..Job::default() };
        runs.push(Run { workflow: "lane".into(), event: "push".into(), branch: "main".into(), group: Some("lane".into()), cancel_in_progress: true, at, commit: i, status: "succeeded".into(), finished: Some(at + l.run), jobs: vec![job] });
    }
    Trace { end: l.until + 20 * l.run, measured: None, pools: vec![Pool { name: "runners".into(), labels: vec!["runner".into()], slots: Some(l.runners), ..Pool::default() }], commits, runs }
}
