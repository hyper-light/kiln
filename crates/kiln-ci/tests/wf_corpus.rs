//! docs/ci.md W02-AC01: every real workflow of F-03 (GitHub's starter workflows and popular
//! repositories' workflows) reads in the GHA dialect with no errors, and its model round-trips
//! through JSON.

use std::path::{Path, PathBuf};

use kiln_ci::expr::Dialect;
use kiln_ci::wf::{Workflow, parse_workflow};

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            yaml_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "yml" || x == "yaml") {
            out.push(p);
        }
    }
}

#[test]
fn w02_ac01_real_github_workflows_read_without_errors() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gha");
    let mut files = Vec::new();
    yaml_files(&root, &mut files);
    files.sort();
    let mut failures = Vec::new();
    let mut warnings = 0usize;
    for f in &files {
        let mut src = std::fs::read_to_string(f).map_err(|e| e.to_string())?;
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        if rel.starts_with("starter-workflows") {
            // Starter workflows are templates: GitHub fills these in when one is added to a repository.
            for (placeholder, value) in [("$cron-daily", "0 0 * * *"), ("$cron-weekly", "0 0 * * 0"), ("$codeql-languages-matrix", "language: [ 'actions' ]")] {
                src = src.replace(placeholder, value);
            }
        }
        let (wf, diags) = parse_workflow(&format!(".github/workflows/{rel}"), &src, Dialect::Gha);
        for d in diags.iter().filter(|d| d.is_error()) {
            failures.push(d.render(&rel));
        }
        warnings += diags.iter().filter(|d| !d.is_error()).count();
        let Some(wf) = wf else {
            failures.push(format!("{rel}: no model"));
            continue;
        };
        let json = serde_json::to_string(&wf).map_err(|e| e.to_string())?;
        let back: Workflow = serde_json::from_str(&json).map_err(|e| format!("{rel}: {e}"))?;
        if serde_json::to_string(&back).map_err(|e| e.to_string())? != json {
            failures.push(format!("{rel}: the model doesn't round-trip through JSON"));
        }
    }
    eprintln!("GHA corpus: {} files, {} errors, {warnings} warnings and infos", files.len(), failures.len());
    assert!(files.len() >= 300, "the corpus has {} files", files.len());
    if failures.is_empty() { Ok(()) } else { Err(failures.into_iter().take(1000).collect::<Vec<_>>().join("\n")) }
}
