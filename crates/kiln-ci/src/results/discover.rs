//! Opt-in discovery of test reports (docs/ci.md W16-R01): a kiln job's `test-results: discover`
//! takes, after its steps, the reports its tools wrote where they usually write them, through
//! `kiln/test-results@v1` with `discover: true`. Only files the job wrote count: the runner skips
//! what git tracks and what's older than the job.

use crate::wf::ir::{Step, Tpl, YVal};

/// Where test tools write their reports by default, or as people usually name them: JUnit XML
/// from Maven's Surefire and Failsafe, Gradle and Ant (`TEST-*.xml`), nextest, jest-junit,
/// pytest and go-junit-report; .NET's TRX; CTRF. Dependencies and git's own files are left out.
pub const USUAL: &[&str] = &[
    "**/TEST-*.xml",
    "**/junit*.xml",
    "**/*-junit.xml",
    "**/*.junit.xml",
    "**/test-results/**/*.xml",
    "**/test-reports/**/*.xml",
    "**/TestResults/*.trx",
    "**/ctrf/*.json",
    "**/ctrf-report.json",
    "!**/node_modules/**",
    "!**/vendor/**",
    "!**/.git/**",
];

/// A job's `test-results:` (kiln dialect) as the step it runs after its own: `discover` (or
/// `true`, or `{ discover: true }`) is `kiln/test-results@v1` with `discover: true`, run unless
/// the job was cancelled (failed tests are when reports matter most).
pub fn job_step(v: &YVal) -> Option<Step> {
    let on = match v {
        YVal::Bool(b) => *b,
        YVal::Str(t) => t.src.trim() == "discover",
        YVal::Map(_) => matches!(v.get("discover"), Some(YVal::Bool(true))),
        _ => false,
    };
    on.then(|| Step {
        name: Some(Tpl::new("Test results", Default::default())),
        uses: Some("kiln/test-results@v1".into()),
        cond: Some(Tpl::new("!cancelled()", Default::default())),
        with: vec![("discover".to_owned(), YVal::Bool(true))],
        ..Step::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w16_r01_discovery_is_a_step() {
        let src = "on: push\njobs:\n  t:\n    runs-on: x\n    test-results: discover\n    steps:\n      - run: cargo nextest run\n";
        let (w, d) = crate::wf::parse_workflow(".kiln/t.yaml", src, crate::expr::Dialect::Kiln);
        assert!(!d.iter().any(|x| x.is_error()), "{d:?}");
        let job = w.as_ref().and_then(|w| w.jobs.first()).cloned().unwrap_or_default();
        let step = job.kiln.test_results.as_ref().and_then(job_step).unwrap_or_default();
        assert_eq!(step.uses.as_deref(), Some("kiln/test-results@v1"));
        assert_eq!(step.cond.as_ref().map(|c| c.src.as_str()), Some("!cancelled()"));
        assert_eq!(step.with, vec![("discover".to_owned(), YVal::Bool(true))]);
        assert!(job_step(&YVal::Bool(false)).is_none());
        assert!(job_step(&YVal::Str(Tpl::new("never", Default::default()))).is_none());
    }
}
