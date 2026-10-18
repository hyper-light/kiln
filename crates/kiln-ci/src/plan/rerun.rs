//! Which jobs a rerun runs again (docs/ci.md W05-R17), GitHub-exact: every job; the jobs that
//! failed or were cancelled, with the jobs that need them; or one job, with the jobs that need it.
//! The rest keep the last attempt's results. A called workflow's jobs go with the job that calls
//! them (`call/inner` is `call`'s).

use std::collections::BTreeSet;

use crate::wf::Workflow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    All,
    Failed,
    Job(String),
}

/// The workflow job a leg's job belongs to: `call` for `call/inner`.
pub fn top(job: &str) -> &str {
    job.split('/').next().unwrap_or(job)
}

/// The jobs to run again. `failed` are the jobs (any form) with a leg that failed, timed out or
/// was cancelled in the last attempt.
pub fn jobs(wf: &Workflow, mode: &Mode, failed: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let mut again: BTreeSet<String> = match mode {
        Mode::All => wf.jobs.iter().map(|j| j.id.clone()).collect(),
        Mode::Failed => failed.iter().map(|j| top(j).to_owned()).filter(|j| wf.jobs.iter().any(|x| &x.id == j)).collect(),
        Mode::Job(j) => {
            if !wf.jobs.iter().any(|x| &x.id == j) {
                return Err(format!("the workflow has no job `{j}`"));
            }
            std::iter::once(j.clone()).collect()
        }
    };
    if again.is_empty() {
        return Err("no job failed: there is nothing to run again".into());
    }
    // Then every job that needs one of them, transitively.
    loop {
        let more: Vec<String> = wf.jobs.iter().filter(|j| !again.contains(&j.id) && j.needs.iter().any(|n| again.contains(n.job()))).map(|j| j.id.clone()).collect();
        if more.is_empty() {
            return Ok(again);
        }
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
        assert_eq!(jobs(&w, &Mode::All, &set(&[])), Ok(set(&["a", "b", "c", "d", "e", "call"])));
        // Failed: b failed, so b and c (which needs it); a, d and e keep their results.
        assert_eq!(jobs(&w, &Mode::Failed, &set(&["b"])), Ok(set(&["b", "c"])));
        // One job: a, and b, c and e, which need it, directly or not.
        assert_eq!(jobs(&w, &Mode::Job("a".into()), &set(&[])), Ok(set(&["a", "b", "c", "e"])));
        assert_eq!(jobs(&w, &Mode::Job("d".into()), &set(&[])), Ok(set(&["d", "e"])));
        // A called workflow's failed job reruns the job that calls it.
        assert_eq!(jobs(&w, &Mode::Failed, &set(&["call/inner"])), Ok(set(&["call"])));
        assert!(jobs(&w, &Mode::Failed, &set(&[])).is_err_and(|e| e.contains("nothing to run again")));
        assert!(jobs(&w, &Mode::Job("nope".into()), &set(&[])).is_err_and(|e| e.contains("no job `nope`")));
    }
}
