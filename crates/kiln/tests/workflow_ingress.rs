//! U01/U02, W02-R06: old kiln syntax enters the workflow engine, including manual replies;
//! invalid workflows fail visibly instead of falling back to another planner.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http};
use serde_json::{Value, json};

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false"]).args(args).output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(())
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (status, body) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(status == 200, "{path}: {status} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn trigger(s: &Server, repo: &str) -> anyhow::Result<String> {
    let (status, body) = http(s.port, "POST", &format!("/api/repos/{repo}/trigger"), &s.admin, Some(&json!({ "branch": "main", "pipeline": "ci" })))?;
    anyhow::ensure!(status == 200, "manual trigger: {status} {body}\n{}", s.log());
    let runs: Vec<Value> = serde_json::from_str(&body)?;
    anyhow::ensure!(runs.len() == 1, "manual dispatch returns its one recorded run: {body}");
    runs.first().and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("manual dispatch returned no run id: {body}"))
}

#[test]
fn u01_old_workflow_switch_cannot_select_a_second_engine() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    let yaml = "name: ci\non: { manual: true }\njobs:\n  test:\n    runs-on: local\n    steps:\n      - run: test \"${{ 'ABC' == 'abc' }}\" = true\n";
    std::fs::write(repo.join(".kiln/ci.yaml"), yaml)?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "workflow"])?;
    let extra = format!("[[repos]]\nname = \"acme/unify\"\nclone_url = \"file://{}\"\npoll_seconds = 1\n", repo.display());
    let s = common::start(dir.path(), &Options { agent: true, ci: "kiln_workflows = false", extra: &extra })?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let repo_id = loop {
        let repos = get(&s, "/api/repos")?;
        if let Some(id) = repos.as_array().and_then(|rs| rs.iter().find(|r| r.get("name").and_then(Value::as_str) == Some("acme/unify"))).and_then(|r| r.get("id")).and_then(Value::as_str) {
            break id.to_owned();
        }
        anyhow::ensure!(Instant::now() < deadline, "repository was not discovered: {repos}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    };
    let id = trigger(&s, &repo_id)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let run = get(&s, &format!("/api/ci/runs/{id}"))?;
        assert_eq!(run.pointer("/run/workflow").and_then(Value::as_str), Some(".kiln/ci.yaml"), "{run}");
        if run.pointer("/run/finished_at").is_some_and(|x| !x.is_null()) {
            assert_eq!(run.pointer("/run/conclusion").and_then(Value::as_str), Some("success"), "the shared expression evaluator and runtime execute the workflow: {run}\n{}", s.log());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "workflow never finished: {run}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    let all = get(&s, "/api/ci/runs?repo=acme/unify&limit=10")?;
    assert_eq!(all.get("runs").and_then(Value::as_array).map(Vec::len), Some(1), "one manual request created one run: {all}");

    std::fs::write(repo.join(".kiln/ci.yaml"), "name: ci\non: { manual: true }\njobs: invalid\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "invalid workflow"])?;
    let failed = trigger(&s, &repo_id)?;
    let run = get(&s, &format!("/api/ci/runs/{failed}"))?;
    assert_eq!(run.pointer("/run/conclusion").and_then(Value::as_str), Some("startup_failure"), "{run}");
    assert!(run.pointer("/run/error").and_then(Value::as_str).is_some_and(|e| e.contains("jobs")), "the diagnostic is visible: {run}");
    Ok(())
}
