//! docs/ci.md W22-AC01, the planning half: every workflow of the GHA corpus (F-03) plans on the
//! engine for each event it runs on, as its own repository (so `if: github.repository == …`
//! guards see their repository), with the called workflows of that repository read from the
//! corpus. A plan that can't be made says why; only a workflow called from outside the corpus is
//! an expected reason.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kiln_ci::expr::Dialect;
use kiln_ci::plan::{PlanSource, RunInput, RunPlan, Settings};
use kiln_ci::wf::{Workflow, parse_workflow};
use serde_json::{Value, json};

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

/// Starter workflows are templates: GitHub fills these in when one is added to a repository.
fn source(path: &Path, rel: &str) -> Result<String, String> {
    let mut src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if rel.starts_with("starter-workflows") {
        for (placeholder, value) in [("$cron-daily", "0 0 * * *"), ("$cron-weekly", "0 0 * * 0"), ("$codeql-languages-matrix", "language: [ 'actions' ]"), ("$default-branch", "main"), ("$protected-branches", "[ main ]")] {
            src = src.replace(placeholder, value);
        }
    }
    Ok(src)
}

/// One repository's workflows in the corpus, for the workflows they call.
struct Repo {
    dir: PathBuf,
    rel: String,
    /// `owner/name`.
    name: String,
}

impl PlanSource for Repo {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        let (path, _) = uses.split_once('@').unwrap_or((uses, ""));
        // `$/` is the repository's root at the run's commit, as `./` is.
        let path = path.strip_prefix("$/").map_or_else(|| path.to_owned(), |p| format!("./{p}"));
        let file = match path.strip_prefix("./.github/workflows/") {
            Some(f) => f.to_owned(),
            None => match path.strip_prefix(&format!("{}/.github/workflows/", self.name)) {
                Some(f) => f.to_owned(),
                None => return Err(format!("outside the corpus: `{uses}`")),
            },
        };
        let p = self.dir.join(&file);
        if !p.is_file() {
            return Err(format!("outside the corpus: `{uses}` (not vendored)"));
        }
        let rel = format!("{}/{file}", self.rel);
        let (wf, diags) = parse_workflow(&format!(".github/workflows/{file}"), &source(&p, &rel)?, Dialect::Gha);
        if let Some(d) = diags.iter().find(|d| d.is_error()) {
            return Err(d.render(&rel));
        }
        wf.map(Arc::new).ok_or_else(|| format!("{rel}: no model"))
    }
}

/// The `github` context of `event` in `repo`, as GitHub fills it in (the parts workflows read).
fn github(event: &str, repo: &str, workflow: &str) -> Value {
    let owner = repo.split('/').next().unwrap_or_default();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let repository = json!({ "full_name": repo, "name": repo.split('/').nth(1).unwrap_or_default(), "owner": { "login": owner }, "fork": false, "default_branch": "main", "private": false, "html_url": format!("https://github.com/{repo}") });
    let pr = json!({
        "number": 7, "title": "A change", "body": "", "draft": false, "merged": false, "labels": [], "user": { "login": "octocat" }, "author_association": "MEMBER",
        "head": { "ref": "feature", "sha": sha, "repo": repository.clone(), "label": format!("{owner}:feature") },
        "base": { "ref": "main", "sha": sha, "repo": repository.clone() },
    });
    let (git_ref, payload) = match event {
        "pull_request" | "pull_request_target" => ("refs/pull/7/merge", json!({ "action": "opened", "number": 7, "pull_request": pr, "repository": repository })),
        "push" => ("refs/heads/main", json!({ "ref": "refs/heads/main", "before": sha, "after": sha, "created": false, "deleted": false, "forced": false, "commits": [], "head_commit": { "id": sha, "message": "A commit" }, "repository": repository })),
        "release" => ("refs/tags/v1.0.0", json!({ "action": "published", "release": { "tag_name": "v1.0.0", "name": "v1.0.0", "prerelease": false, "draft": false }, "repository": repository })),
        "issue_comment" => ("refs/heads/main", json!({ "action": "created", "issue": { "number": 7, "pull_request": { "url": "" }, "labels": [], "user": { "login": "octocat" } }, "comment": { "body": "/test", "user": { "login": "octocat" }, "author_association": "MEMBER" }, "repository": repository })),
        "issues" => ("refs/heads/main", json!({ "action": "opened", "issue": { "number": 7, "title": "A bug", "labels": [], "user": { "login": "octocat" } }, "repository": repository })),
        "workflow_run" => ("refs/heads/main", json!({ "action": "completed", "workflow_run": { "id": 1, "conclusion": "success", "event": "push", "head_branch": "main", "head_sha": sha, "pull_requests": [] }, "repository": repository })),
        "merge_group" => ("refs/heads/gh-readonly-queue/main/pr-7", json!({ "action": "checks_requested", "merge_group": { "head_sha": sha, "head_ref": "refs/heads/gh-readonly-queue/main/pr-7", "base_ref": "refs/heads/main" }, "repository": repository })),
        "schedule" => ("refs/heads/main", json!({ "schedule": "0 0 * * *", "repository": repository })),
        _ => ("refs/heads/main", json!({ "repository": repository })),
    };
    let pr_like = matches!(event, "pull_request" | "pull_request_target");
    json!({
        "event_name": event, "event": payload, "ref": git_ref, "ref_name": git_ref.rsplit('/').next().unwrap_or_default(),
        "ref_type": if git_ref.starts_with("refs/tags/") { "tag" } else { "branch" }, "ref_protected": false,
        "sha": sha, "repository": repo, "repository_owner": owner, "repository_id": "1", "repository_owner_id": "1",
        "actor": "octocat", "triggering_actor": "octocat", "workflow": workflow, "job": "", "run_id": "100", "run_number": "1", "run_attempt": "1",
        "head_ref": if pr_like { "feature" } else { "" }, "base_ref": if pr_like { "main" } else { "" },
        "server_url": "https://github.com", "api_url": "https://api.github.com", "graphql_url": "https://api.github.com/graphql",
        "workspace": "/home/runner/work/x/x", "action": "", "token": "***", "retention_days": "90", "repositoryUrl": format!("git://github.com/{repo}.git"),
    })
}

/// A `workflow_dispatch`'s inputs: defaults, and a value for each required input without one.
fn dispatch_inputs(wf: &Workflow) -> Value {
    let Some(t) = wf.on.iter().find(|t| t.event == "workflow_dispatch") else { return json!({}) };
    let given: serde_json::Map<String, Value> = t
        .inputs
        .iter()
        .filter(|i| i.required && i.default.is_none())
        .map(|i| {
            let v = match i.ty.as_deref() {
                Some("boolean") => json!(true),
                Some("number") => json!(1),
                Some("choice") => json!(i.options.first().cloned().unwrap_or_default()),
                _ => json!("x"),
            };
            (i.name.clone(), v)
        })
        .collect();
    kiln_ci::trigger::dispatch::inputs(&t.inputs, &given).map(|(typed, _)| typed).unwrap_or_else(|_| json!({}))
}

#[test]
fn w22_ac01_the_corpus_plans_for_every_event_it_runs_on() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gha");
    let mut files = Vec::new();
    yaml_files(&root, &mut files);
    files.sort();
    let (mut plans, mut legs, mut later, mut outside) = (0usize, 0usize, 0usize, Vec::new());
    let mut failures = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        let dir = f.parent().unwrap_or(&root).to_path_buf();
        let dir_rel = dir.strip_prefix(&root).unwrap_or(&dir).display().to_string();
        // `owner_name` → `owner/name`; starter workflows are planned as a repository of their own.
        let name = if dir_rel == "starter-workflows" { "acme/app".to_owned() } else { dir_rel.replacen('_', "/", 1) };
        let (wf, _) = parse_workflow(&format!(".github/workflows/{}", f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()), &source(f, &rel)?, Dialect::Gha);
        let Some(wf) = wf else {
            failures.push(format!("{rel}: no model"));
            continue;
        };
        let src = Repo { dir: dir.clone(), rel: dir_rel.clone(), name: name.clone() };
        let mut events: Vec<String> = wf.on.iter().map(|t| t.event.clone()).filter(|e| e != "workflow_call").collect();
        events.dedup();
        let wf = Arc::new(wf);
        for event in events.iter().map(String::as_str) {
            let input = RunInput {
                github: github(event, &name, wf.name.as_deref().unwrap_or(&rel)),
                inputs: if event == "workflow_dispatch" { dispatch_inputs(&wf) } else { json!({}) },
                vars: json!({}),
                changed: None,
                trusted: event != "pull_request_target",
            };
            match RunPlan::preview(wf.clone(), input, Settings::default(), &src) {
                Ok(p) => {
                    plans += 1;
                    legs += p.legs.len();
                    for d in p.decided.iter().filter(|d| d.conclusion == kiln_ci::plan::Conclusion::Failure) {
                        if d.reason.contains("outside the corpus") {
                            outside.push(format!("{rel} ({event}) {}: {}", d.job, d.reason));
                        } else {
                            failures.push(format!("{rel} ({event}) {}: {}", d.job, d.reason));
                        }
                    }
                    later += p.later.len();
                    for l in &p.legs {
                        if l.runs_on.labels.is_empty() && l.runs_on.group.is_none() {
                            failures.push(format!("{rel} ({event}): leg {} runs on nothing", l.key));
                        }
                    }
                }
                Err(e) if e.contains("outside the corpus") => outside.push(format!("{rel} ({event}): {e}")),
                Err(e) => failures.push(format!("{rel} ({event}): {e}")),
            }
        }
    }
    eprintln!("GHA corpus planned: {} files, {plans} plans, {legs} legs, {later} jobs decided by their needs' outputs; {} call workflows outside the corpus; {} failed", files.len(), outside.len(), failures.len());
    for o in &outside {
        eprintln!("  outside: {o}");
    }
    assert!(plans >= 400, "only {plans} plans");
    if failures.is_empty() { Ok(()) } else { Err(failures.join("\n")) }
}
