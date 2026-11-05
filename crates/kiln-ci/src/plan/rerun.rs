//! Which jobs a rerun runs again (docs/ci.md W05-R17), GitHub-exact: every job; the jobs that
//! failed or were cancelled, with the jobs that need them; one job, with the jobs that need it; or
//! one leg (W05-R28), with the jobs that need its job, as GitHub reruns one combination of a
//! matrix (each is a job of its own there). The rest keep the last attempt's results. A called
//! workflow's jobs go with the job that calls them (`call/inner` is `call`'s), and a gang's leg
//! with its job's others (W05-R40).

use std::collections::BTreeSet;

use crate::wf::Workflow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    All,
    Failed,
    Job(String),
    /// One leg of `job` (its key): the job's other legs keep their results.
    Leg { job: String, key: String },
    /// Several at once (docs/ci.md W26-R21): what each would run again, together.
    Set(Vec<Mode>),
}

/// The workflow job a leg's job belongs to: `call` for `call/inner`.
pub fn top(job: &str) -> &str {
    job.split('/').next().unwrap_or(job)
}

/// What a rerun runs again: every leg of `jobs`, and the legs `legs` (their keys) besides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Again {
    pub jobs: BTreeSet<String>,
    pub legs: BTreeSet<String>,
}

impl Again {
    /// Whether the leg `key` of job `job` (any form) runs again.
    pub fn runs(&self, job: &str, key: &str) -> bool {
        self.jobs.contains(top(job)) || self.legs.contains(key)
    }
}

/// What runs again. `failed` are the jobs (any form) with a leg that failed, timed out or was
/// cancelled in the last attempt.
pub fn jobs(wf: &Workflow, mode: &Mode, failed: &BTreeSet<String>) -> Result<Again, String> {
    if let Mode::Set(modes) = mode {
        let mut all = Again::default();
        for m in modes {
            let one = jobs(wf, m, failed)?;
            all.jobs.extend(one.jobs);
            all.legs.extend(one.legs);
        }
        if all.jobs.is_empty() && all.legs.is_empty() {
            return Err("nothing was named to run again".into());
        }
        return Ok(all);
    }
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
        // A gang's leg goes with its job's others (W05-R40): they start together or not at all, and
        // one alone would wait for them forever.
        Mode::Leg { job, .. } if wf.job(job).is_some_and(|j| j.kiln.together) => (std::iter::once(job.clone()).collect(), None),
        Mode::Leg { job, key } => {
            if !known(job) {
                return Err(unknown(job));
            }
            (BTreeSet::new(), Some((job.clone(), key.clone())))
        }
        // Taken above.
        Mode::Set(_) => (BTreeSet::new(), None),
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
            return Ok(Again { jobs: again, legs: leg.map(|(_, key)| key).into_iter().collect() });
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
    fn w26_r21_a_set_runs_again_each_and_what_needs_it() {
        let w = wf();
        // Two jobs at once: each, and every job that needs either, transitively.
        assert_eq!(jobs(&w, &Mode::Set(vec![Mode::Job("b".into()), Mode::Job("d".into())]), &set(&[])), Ok(Again { jobs: set(&["b", "c", "d", "e"]), legs: set(&[]) }));
        // A job and a leg of another: the leg runs again, its job's other legs keep their results,
        // and the jobs that need its job run again too.
        assert_eq!(jobs(&w, &Mode::Set(vec![Mode::Job("d".into()), Mode::Leg { job: "a".into(), key: "a (2)".into() }]), &set(&[])), Ok(Again { jobs: set(&["b", "c", "d", "e"]), legs: set(&["a (2)"]) }));
        // A passed job is run again like any other: what ran doesn't matter, only what was named.
        assert_eq!(jobs(&w, &Mode::Set(vec![Mode::Job("c".into())]), &set(&["a"])), Ok(Again { jobs: set(&["c"]), legs: set(&[]) }));
        // One unknown name refuses the set, saying which; nothing named, nothing to do.
        assert!(jobs(&w, &Mode::Set(vec![Mode::Job("c".into()), Mode::Job("nope".into())]), &set(&[])).is_err_and(|e| e.contains("no job `nope`")));
        assert!(jobs(&w, &Mode::Set(Vec::new()), &set(&[])).is_err_and(|e| e.contains("nothing was named")));
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
        assert_eq!(again, Ok(Again { jobs: set(&["c"]), legs: set(&["b (1)"]) }));
        let again = again.unwrap_or_default();
        assert!(again.runs("b", "b (1)") && again.runs("c", "c"));
        assert!(!again.runs("b", "b (2)") && !again.runs("a", "a") && !again.runs("e", "e"));
        // A leg of a job nothing needs runs alone.
        assert_eq!(jobs(&w, &Mode::Leg { job: "c".into(), key: "c".into() }, &set(&[])), Ok(Again { jobs: set(&[]), legs: set(&["c"]) }));
        // A called workflow's leg reruns the job that calls it, whole.
        assert_eq!(jobs(&w, &Mode::Leg { job: "call/inner".into(), key: "call / inner".into() }, &set(&[])), Ok(Again { jobs: set(&["call"]), legs: set(&[]) }));
        assert!(jobs(&w, &Mode::Leg { job: "nope".into(), key: "nope".into() }, &set(&[])).is_err_and(|e| e.contains("no job `nope`")));
    }

    /// W05-R40, docs/needs.md E4.4: a gang's leg reruns its whole job, with the jobs that need it:
    /// its legs start together or not at all.
    #[test]
    fn w05_r40_a_gang_s_leg_reruns_its_whole_job() {
        let src = "name: ci\non: push\njobs:\n  sim:\n    strategy: { matrix: { node: [a, b, c] } }\n    together: true\n    steps:\n      - run: ./node\n  report:\n    needs: sim\n    steps:\n      - run: ./report\n";
        let (w, d) = crate::wf::parse_workflow(".kiln/sim.yaml", src, Dialect::Kiln);
        assert!(d.iter().all(|x| x.severity != crate::wf::Severity::Error), "{d:?}");
        let w = w.unwrap_or_default();
        assert!(w.job("sim").is_some_and(|j| j.kiln.together));
        assert_eq!(jobs(&w, &Mode::Leg { job: "sim".into(), key: "sim (b)".into() }, &set(&[])), Ok(Again { jobs: set(&["sim", "report"]), legs: set(&[]) }));
    }
}
