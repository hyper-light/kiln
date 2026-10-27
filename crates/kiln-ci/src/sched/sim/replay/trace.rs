//! The recorded week the replay reads: `ci/sim/trace.json`, as `ci/sim/export_trace.py` writes it
//! (its README describes every field). Times are whole seconds from the window's start.

use std::collections::BTreeMap;

use serde::Deserialize;

/// A recorded window of v1 runs, with the runners' capacity by label.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Trace {
    /// When the runs were fetched: what was still running then never finished in the trace.
    pub end: i64,
    /// The figures measured on the window (`runs_analyze.py`'s), when the trace is a recorded one.
    #[serde(default)]
    pub measured: Option<Measured>,
    pub pools: Vec<Pool>,
    pub commits: Vec<Commit>,
    pub runs: Vec<Run>,
}

/// `runs_analyze.py`'s figures for the window.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Measured {
    pub superseded_share: f64,
    pub ci_concluded: u32,
    pub ci_pushes: u32,
}

/// Runner capacity: a machine with slots, or a quota of pods (`k8s`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Pool {
    pub name: String,
    pub labels: Vec<String>,
    #[serde(default)]
    pub slots: Option<u32>,
    #[serde(default)]
    pub cpu_milli: Option<u64>,
    #[serde(default)]
    pub mem_mib: Option<u64>,
    #[serde(default)]
    pub pods: Option<u32>,
}

/// A commit the runs built: for each input class, a key; equal keys, equal inputs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Commit {
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
}

/// One recorded run.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Run {
    pub workflow: String,
    pub event: String,
    pub branch: String,
    #[serde(default)]
    pub group: Option<String>,
    /// As the workflow file at the run's commit had it; the newer run's setting decides (v1).
    #[serde(default)]
    pub cancel_in_progress: bool,
    pub at: i64,
    pub commit: usize,
    pub status: String,
    #[serde(default)]
    pub finished: Option<i64>,
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// One recorded job of a run.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Job {
    pub key: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    /// The input class whose key is the job's (`rust`, `ui`, ...); the workflow's name when unset.
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub exclusive: bool,
    /// A k8s job's requests: millicores, MiB.
    #[serde(default)]
    pub req: Option<(u64, u64)>,
    #[serde(default)]
    pub queued: Option<i64>,
    #[serde(default)]
    pub started: Option<i64>,
    #[serde(default)]
    pub finished: Option<i64>,
    pub status: String,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub failed_step: Option<String>,
    /// Durations of the steps another pipeline's work could replace: `ui`, `glibc`.
    #[serde(default)]
    pub shared: BTreeMap<String, i64>,
}

/// How a job that ran to its end ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum End {
    Succeeded,
    Failed,
    TimedOut,
}

impl End {
    pub fn from_status(s: &str) -> Option<End> {
        match s {
            "succeeded" => Some(End::Succeeded),
            "failed" => Some(End::Failed),
            "timed_out" => Some(End::TimedOut),
            _ => None,
        }
    }

    pub fn passed(self) -> bool {
        self == End::Succeeded
    }
}

/// Why a trace was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TraceError {
    #[error("the trace isn't valid JSON for its format: {0}")]
    Json(String),
    #[error("run {run} names commit {commit}, but the trace has {commits} commits")]
    Commit { run: usize, commit: usize, commits: usize },
    #[error("the trace has no pools")]
    NoPools,
}

impl Trace {
    /// Reads `ci/sim/trace.json`'s text.
    pub fn parse(text: &str) -> Result<Trace, TraceError> {
        let t: Trace = serde_json::from_str(text).map_err(|e| TraceError::Json(e.to_string()))?;
        if t.pools.is_empty() {
            return Err(TraceError::NoPools);
        }
        for (i, r) in t.runs.iter().enumerate() {
            if r.commit >= t.commits.len() {
                return Err(TraceError::Commit { run: i, commit: r.commit, commits: t.commits.len() });
            }
        }
        Ok(t)
    }
}

impl Run {
    /// A push to a branch: a commit that wants a verdict (what P2 and later call a demand).
    pub fn is_push(&self) -> bool {
        self.event == "push"
    }
}

impl Job {
    /// It ran to its end in the trace: its duration and outcome are known.
    pub fn completed(&self) -> Option<(i64, End)> {
        let end = End::from_status(&self.status)?;
        match (self.started, self.finished) {
            (Some(s), Some(f)) if f >= s => Some((f - s, end)),
            _ => None,
        }
    }

    pub fn is_k8s(&self) -> bool {
        self.labels.iter().any(|l| l == "k8s")
    }
}
