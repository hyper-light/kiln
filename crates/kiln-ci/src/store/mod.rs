//! The rules of the runtime store (docs/ci.md W08): how objects are cut into chunks, and GitHub's
//! cache semantics. The server's store and services in `crates/kiln/src/ci/services` carry them
//! out.
//!
//! - [`cdc`]: content-defined chunking (W08-R01).
//! - [`cache`]: cache modes, lookup, immutability and eviction (W08-R06, W08-R14).
//! - [`v1_artifacts_step`]: v1's `artifacts:` as an artifact (W08-R08).

pub mod cache;
pub mod cdc;

/// A v1 job's `artifacts: [globs]` (kiln dialect) as the step it runs after its own, as v1 kept
/// them: unless the job was cancelled, the files the globs match in the workspace, uploaded as the
/// run's artifact `artifacts` (`kiln/upload@v1`), quietly when nothing matches.
pub fn v1_artifacts_step(globs: &[String]) -> Option<crate::wf::ir::Step> {
    use crate::wf::ir::{Step, Tpl, YVal};
    let text = |s: &str| YVal::Str(Tpl::new(s, Default::default()));
    (!globs.is_empty()).then(|| Step {
        name: Some(Tpl::new("Artifacts", Default::default())),
        uses: Some("kiln/upload@v1".into()),
        cond: Some(Tpl::new("!cancelled()", Default::default())),
        with: vec![("name".to_owned(), text("artifacts")), ("path".to_owned(), text(&globs.join("\n"))), ("if-no-files-found".to_owned(), text("ignore"))],
        ..Step::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_r08_v1_artifacts_are_an_artifact() {
        let src = "on: push\njobs:\n  build:\n    steps:\n      - run: make\n    artifacts: [dist/*.tar.gz, report.txt]\n";
        let (w, d) = crate::wf::parse_workflow(".kiln/build.yaml", src, crate::expr::Dialect::Kiln);
        assert!(!d.iter().any(|x| x.is_error()), "{d:?}");
        let job = w.as_ref().and_then(|w| w.jobs.first()).cloned().unwrap_or_default();
        let step = v1_artifacts_step(&job.kiln.artifacts).unwrap_or_default();
        assert_eq!(step.uses.as_deref(), Some("kiln/upload@v1"));
        assert_eq!(step.cond.as_ref().map(|c| c.src.as_str()), Some("!cancelled()"));
        let with: Vec<(String, String)> = step.with.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect();
        assert_eq!(with, [("name".to_owned(), "artifacts".to_owned()), ("path".to_owned(), "dist/*.tar.gz\nreport.txt".to_owned()), ("if-no-files-found".to_owned(), "ignore".to_owned())]);
        assert!(v1_artifacts_step(&[]).is_none());
    }
}
