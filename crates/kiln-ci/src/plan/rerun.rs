//! Which jobs a rerun runs again (docs/ci.md W05-R17), GitHub-exact: every job; the jobs that
//! failed or were cancelled, with the jobs that need them; one job, with the jobs that need it; or
//! one leg (W05-R28), with the jobs that need its job, as GitHub reruns one combination of a
//! matrix (each is a job of its own there). The rest keep the last attempt's results. A called
//! workflow's jobs go with the job that calls them (`call/inner` is `call`'s).

use std::collections::BTreeSet;

use crate::wf::Workflow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    All,
    Failed,
    Job(String),
    /// One leg of `job` (its key): the job's other legs keep their results.
    Leg { job: String, key: String },
}

/// The workflow job a leg's job belongs to: `call` for `call/inner`.
pub fn top(job: &str) -> &str {
    job.split('/').next().unwrap_or(job)
}

/// What a rerun runs again: every leg of `jobs`, and the leg `leg` (a key) besides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Again {
    pub jobs: BTreeSet<String>,
    pub leg: Option<String>,
}

impl Again {
    /// Whether the leg `key` of job `job` (any form) runs again.
    pub fn runs(&self, job: &str, key: &str) -> bool {
        self.jobs.contains(top(job)) || self.leg.as_deref() == Some(key)
    }
}

/// What runs again. `failed` are the jobs (any form) with a leg that failed, timed out or was
/// cancelled in the last attempt.
pub fn jobs(wf: &Workflow, mode: &Mode, failed: &BTreeSet<String>) -> Result<Again, String> {
    let known = |j: &str| wf.jobs.iter().any(|x| x.id == j);
    let unknown = |j: &str| format!("the workflow has no job `{j}`");
    let (mut again, leg): (BTreeSet<String>, Option<(String, String)>) = match mode {
        Mode::All => (wf.jobs.iter().map(|j| j.id.clone()).collect(), None),
        Mode::Failed => (failed.iter().map(|j| top(j).to_owned()).filter(|j| known(j)).collect(), None),
        Mode::Job(j) => {
            if !known(j) {
                return Err(unknown(j));
            }
            (std::iter::once(j.clone()).collect(), None)
        }
        // A called workflow's leg goes with the job that calls it, whole, as `Failed` takes it.
        Mode::Leg { job, .. } if top(job) != job => {
            if !known(top(job)) {
                return Err(unknown(top(job)));
            }
            (std::iter::once(top(job).to_owned()).collect(), None)
        }
        Mode::Leg { job, key } => {
            if !known(job) {
                return Err(unknown(job));
            }
            (BTreeSet::new(), Some((job.clone(), key.clone())))
        }
    };
    if again.is_empty() && leg.is_none() {
        return Err("no job failed: there is nothing to run again".into());
    }
    // Then every job that needs one of them (or the leg's job), transitively.
    let mut needed: BTreeSet<String> = again.clone();
    needed.extend(leg.iter().map(|(job, _)| job.clone()));
    loop {
        let more: Vec<String> = wf.jobs.iter().filter(|j| !needed.contains(&j.id) && j.needs.iter().any(|n| needed.contains(n.job()))).map(|j| j.id.clone()).collect();
        if more.is_empty() {
            return Ok(Again { jobs: again, leg: leg.map(|(_, key)| key) });
        }
        needed.extend(more.iter().cloned());
        again.extend(more);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Dialect;

    fn wf() -> Workflow {
        let src = "on: push\njobs:\n  a: {runs-on: x, steps: [{run: a}]}\n  b: {needs: a, runs-on: x, steps: [{run: b}]}\n  c: {needs: b, runs-on: x, steps: [{run: c}]}\n  d: {runs-on: x, steps: [{run: d}]}\n  e: {needs: [a, d], runs-on: x, steps: [{run: e}]}\n  call: {uses: ./.github/workflows/lib.yml}\n";
        crate::wf::parse_workflow(".github/workflows/w.yml", src, Dialect::Gha).0.unwrap_or_default()
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn w05_r17_what_each_rerun_runs_again() {
        let w = wf();
        let jobs_of = |mode: Mode, failed: &[&str]| jobs(&w, &mode, &set(failed)).map(|a| a.jobs);
        assert_eq!(jobs_of(Mode::All, &[]), Ok(set(&["a", "b", "c", "d", "e", "call"])));
        // Failed: b failed, so b and c (which needs it); a, d and e keep their results.
        assert_eq!(jobs_of(Mode::Failed, &["b"]), Ok(set(&["b", "c"])));
        // One job: a, and b, c and e, which need it, directly or not.
        assert_eq!(jobs_of(Mode::Job("a".into()), &[]), Ok(set(&["a", "b", "c", "e"])));
        assert_eq!(jobs_of(Mode::Job("d".into()), &[]), Ok(set(&["d", "e"])));
        // A called workflow's failed job reruns the job that calls it.
        assert_eq!(jobs_of(Mode::Failed, &["call/inner"]), Ok(set(&["call"])));
        assert!(jobs_of(Mode::Failed, &[]).is_err_and(|e| e.contains("nothing to run again")));
        assert!(jobs_of(Mode::Job("nope".into()), &[]).is_err_and(|e| e.contains("no job `nope`")));
    }

    /// W05-R28: one leg runs again, with the jobs that need its job; its job's other legs, and the
    /// rest, keep their results.
    #[test]
    fn w05_r28_one_leg_runs_again() {
        let w = wf();
        let again = jobs(&w, &Mode::Leg { job: "b".into(), key: "b (1)".into() }, &set(&[]));
        assert_eq!(again, Ok(Again { jobs: set(&["c"]), leg: Some("b (1)".into()) }));
        let again = again.unwrap_or_default();
        assert!(again.runs("b", "b (1)") && again.runs("c", "c"));
        assert!(!again.runs("b", "b (2)") && !again.runs("a", "a") && !again.runs("e", "e"));
        // A leg of a job nothing needs runs alone.
        assert_eq!(jobs(&w, &Mode::Leg { job: "c".into(), key: "c".into() }, &set(&[])), Ok(Again { jobs: set(&[]), leg: Some("c".into()) }));
        // A called workflow's leg reruns the job that calls it, whole.
        assert_eq!(jobs(&w, &Mode::Leg { job: "call/inner".into(), key: "call / inner".into() }, &set(&[])), Ok(Again { jobs: set(&["call"]), leg: None }));
        assert!(jobs(&w, &Mode::Leg { job: "nope".into(), key: "nope".into() }, &set(&[])).is_err_and(|e| e.contains("no job `nope`")));
    }
}
