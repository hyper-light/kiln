//! docs/ci.md W02-AC04: every v1 file (F-02: every hyper-light repository's `.kiln/*.yaml`) read
//! by the new kiln-dialect front end plans exactly as v1 plans it: the same jobs, keys, needs,
//! labels, images, environment, conditions and steps, for each event and branch v1 runs it on.

use std::path::{Path, PathBuf};

use kiln::pipeline::{self, Context, PlannedJob};
use kiln_ci::expr::Dialect;
use kiln_ci::wf;

fn corpus() -> Result<Vec<PathBuf>, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kiln-ci/tests/fixtures/v1");
    let mut files = Vec::new();
    for repo in std::fs::read_dir(&root).map_err(|e| e.to_string())?.filter_map(Result::ok) {
        if repo.path().is_dir() {
            for f in std::fs::read_dir(repo.path()).map_err(|e| e.to_string())?.filter_map(Result::ok) {
                files.push(f.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn context(event: &str, branch: &str) -> Context {
    let mut c = Context::default();
    for (k, v) in [("git.sha", "0123456789abcdef"), ("git.short_sha", "01234567"), ("git.branch", branch), ("branch", branch), ("event", event), ("repo", "hyper-light/x"), ("sha", "0123456789abcdef")] {
        c.set(k, v);
    }
    c
}

fn show(jobs: &[PlannedJob]) -> String {
    serde_json::to_string_pretty(jobs).unwrap_or_default()
}

#[test]
fn w02_ac04_v1_files_plan_as_v1_plans_them() -> Result<(), String> {
    let files = corpus()?;
    let (mut compared, mut jobs, mut failures) = (0usize, 0usize, Vec::new());
    for file in &files {
        let src = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let name = file.display().to_string();
        // Only what v1 reads is v1's: a file v1 refuses has nothing to keep compatible.
        let Ok(v1) = pipeline::parse(&src) else { continue };
        let (ir, diags) = wf::parse_workflow(".kiln/x.yaml", &src, Dialect::Kiln);
        for d in diags.iter().filter(|d| d.is_error()) {
            failures.push(d.render(&name));
        }
        let Some(ir) = ir else { continue };
        let adapted = match pipeline::from_ir(&ir) {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        // The workflow's own settings.
        if adapted.name != v1.name || adapted.env != v1.env {
            failures.push(format!("{name}: name or env differ"));
        }
        let conc = |p: &pipeline::Pipeline| p.concurrency.as_ref().map(|c| (c.group.clone(), c.cancel_in_progress));
        if conc(&adapted) != conc(&v1) {
            failures.push(format!("{name}: concurrency differs: v1 {:?}, kiln {:?}", conc(&v1), conc(&adapted)));
        }
        let on = |p: &pipeline::Pipeline| {
            let f = |r: &Option<pipeline::RefFilter>| r.as_ref().map(|r| (r.branches.clone(), r.paths.clone()));
            (f(&p.on.push), f(&p.on.pull_request), f(&p.on.tag), p.on.schedule.clone(), p.on.manual)
        };
        if on(&adapted) != on(&v1) {
            failures.push(format!("{name}: triggers differ: v1 {:?}, kiln {:?}", on(&v1), on(&adapted)));
        }
        // The plan, for each event and branch.
        for event in ["push", "pull_request", "schedule", "manual"] {
            for branch in ["main", "feature/x"] {
                let cx = context(event, branch);
                let (old, new) = (pipeline::plan(&v1, &cx).map_err(|e| e.to_string()), pipeline::plan(&adapted, &cx).map_err(|e| e.to_string()));
                compared += 1;
                match (old, new) {
                    (Ok(a), Ok(b)) if a == b => jobs += a.len(),
                    (Ok(a), Ok(b)) => failures.push(format!("{name} on {event}/{branch}: plans differ\nv1:\n{}\nkiln:\n{}", show(&a), show(&b))),
                    (a, b) => failures.push(format!("{name} on {event}/{branch}: v1 {:?}, kiln {:?}", a.map(|j| j.len()), b.map(|j| j.len()))),
                }
            }
        }
    }
    eprintln!("v1 corpus: {} files, {compared} plans compared, {jobs} planned jobs", files.len());
    assert!(compared >= 200 && jobs > 1_000, "the corpus is there ({compared} plans, {jobs} jobs)");
    if failures.is_empty() { Ok(()) } else { Err(format!("{} differences:\n{}", failures.len(), failures.into_iter().take(20).collect::<Vec<_>>().join("\n"))) }
}

/// W02-S04: kiln's own e2e workflow plans the same 62 jobs either way.
#[test]
fn w02_s04_kilns_e2e_plans_62_jobs() -> Result<(), String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kiln-ci/tests/fixtures/v1/kiln/e2e.yaml");
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v1 = pipeline::parse(&src).map_err(|e| e.to_string())?;
    let (ir, _) = wf::parse_workflow(".kiln/e2e.yaml", &src, Dialect::Kiln);
    let adapted = pipeline::from_ir(&ir.ok_or("no model")?)?;
    let cx = context("push", "main");
    let (a, b) = (pipeline::plan(&v1, &cx).map_err(|e| e.to_string())?, pipeline::plan(&adapted, &cx).map_err(|e| e.to_string())?);
    assert_eq!(a.len(), 62);
    assert_eq!(a, b);
    Ok(())
}
