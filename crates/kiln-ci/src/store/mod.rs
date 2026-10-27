//! The rules of the runtime store (docs/ci.md W08): how objects are cut into chunks, and GitHub's
//! cache semantics. The server's store and services in `crates/kiln/src/ci/services` carry them
//! out.
//!
//! - [`cdc`]: content-defined chunking (W08-R01).
//! - [`cache`]: cache modes, lookup, immutability and eviction (W08-R06, W08-R14).
//! - [`v1_artifacts_step`]: v1's `artifacts:` as an artifact of each leg (W08-R08).

pub mod cache;
pub mod cdc;

/// The artifact leg `key`'s `artifacts:` are uploaded as: `artifacts-<key>`, so each leg of a run
/// keeps its own (an artifact's name is unique within its run, W08-R08), with what an artifact's
/// name can't hold (`" : < > | * ? \r \n \\ /`) as `_`.
pub fn v1_artifacts_name(key: &str) -> String {
    let key: String = key.chars().map(|c| if matches!(c, '"' | ':' | '<' | '>' | '|' | '*' | '?' | '\r' | '\n' | '\\' | '/') { '_' } else { c }).collect();
    format!("artifacts-{key}")
}

/// A v1 job's `artifacts: [globs]` (kiln dialect) as the step leg `key` runs after its own, as v1
/// kept them: unless the job was cancelled, the files the globs match in the workspace, uploaded
/// as the run's artifact [`v1_artifacts_name`] (`kiln/upload@v1`), quietly when nothing matches.
pub fn v1_artifacts_step(globs: &[String], key: &str) -> Option<crate::wf::ir::Step> {
    use crate::wf::ir::{Step, Tpl, YVal};
    let text = |s: &str| YVal::Str(Tpl::new(s, Default::default()));
    (!globs.is_empty()).then(|| Step {
        name: Some(Tpl::new("Artifacts", Default::default())),
        uses: Some("kiln/upload@v1".into()),
        cond: Some(Tpl::new("!cancelled()", Default::default())),
        with: vec![("name".to_owned(), text(&v1_artifacts_name(key))), ("path".to_owned(), text(&globs.join("\n"))), ("if-no-files-found".to_owned(), text("ignore"))],
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
        let step = v1_artifacts_step(&job.kiln.artifacts, "build").unwrap_or_default();
        assert_eq!(step.uses.as_deref(), Some("kiln/upload@v1"));
        assert_eq!(step.cond.as_ref().map(|c| c.src.as_str()), Some("!cancelled()"));
        let with: Vec<(String, String)> = step.with.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect();
        assert_eq!(with, [("name".to_owned(), "artifacts-build".to_owned()), ("path".to_owned(), "dist/*.tar.gz\nreport.txt".to_owned()), ("if-no-files-found".to_owned(), "ignore".to_owned())]);
        assert!(v1_artifacts_step(&[], "build").is_none());
        // Each leg its own: a matrix's, a called workflow's.
        assert_eq!(v1_artifacts_name("build (linux, 14)"), "artifacts-build (linux, 14)");
        assert_eq!(v1_artifacts_name("call (a) / test"), "artifacts-call (a) _ test");
        assert_eq!(v1_artifacts_name("t (a:b, c\\d)"), "artifacts-t (a_b, c_d)");
    }
}
