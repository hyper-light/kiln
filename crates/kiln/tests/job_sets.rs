//! W26-R21 (docs/ci.md): any set of a run's jobs run again or stopped, over HTTP, on both engines,
//! on a server whose jobs no runner can take (they wait in the queue, and nothing else moves them):
//! a `.kiln/` pipeline's (v1) through `POST /api/runs/:id/cancel` and `/retry` with `jobs`, and a
//! GitHub workflow's legs through `POST /api/ci/runs/:id/cancel` with `jobs` and `/rerun` with
//! `{ mode: "set", jobs }`. Each job named answers what became of it; the jobs that need a
//! cancelled one are skipped as "dependency cancelled" and the run's others go on; and without
//! `jobs`, both endpoints do what they did before.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (_, body) = http(s.port, "GET", path, &s.admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

fn post(s: &Server, path: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "POST", path, &s.admin, Some(body))?;
    Ok((st, serde_json::from_str(&body).unwrap_or_default()))
}

/// Waits until `f` has what's waited for, at most `secs`.
fn until<T>(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Result<T, String>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            Ok(v) => return Ok(v),
            Err(why) => anyhow::ensure!(Instant::now() < deadline, "waited {secs}s for {what}: {why}\n--- the server:\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A run's jobs (v1's `jobs`, the engine's `legs`) by key: (id, status, error).
fn by_key(list: Option<&Vec<Value>>) -> std::collections::HashMap<String, (String, String, String)> {
    list.into_iter().flatten().map(|j| (text(j, "key").to_owned(), (text(j, "id").to_owned(), text(j, "status").to_owned(), text(j, "error").to_owned()))).collect()
}

/// The answer's word for job `id`, from `{ jobs: [{ job, <field>, why? }] }`.
fn said(v: &Value, id: &str, field: &str) -> (bool, String) {
    let one = v.get("jobs").and_then(Value::as_array).and_then(|a| a.iter().find(|j| text(j, "job") == id)).cloned().unwrap_or_default();
    (one.get(field).and_then(Value::as_bool).unwrap_or(false), text(&one, "why").to_owned())
}

#[test]
fn w26_r21_any_set_of_a_runs_jobs_run_again_or_stopped_on_both_engines() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path();
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    // Jobs no runner carries the label for: each waits in the queue until it's acted on.
    std::fs::write(
        repo.join(".kiln/pipeline.yaml"),
        "name: v1\non:\n  push:\n    branches: [main]\njobs:\n  a:\n    runs-on: [nobody]\n    steps: [{run: a}]\n  b:\n    needs: [a]\n    runs-on: [nobody]\n    steps: [{run: b}]\n  c:\n    runs-on: [nobody]\n    steps: [{run: c}]\n",
    )?;
    std::fs::write(
        repo.join(".github/workflows/v2.yml"),
        "name: v2\non: push\njobs:\n  a:\n    runs-on: [self-hosted, nobody]\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: [self-hosted, nobody]\n    steps: [{run: b}]\n  c:\n    runs-on: [self-hosted, nobody]\n    steps: [{run: c}]\n",
    )?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/sets\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir, &Options { agent: false, ci: "github_workflows = \"on\"", extra: &extra })?;
    until(&s, 120, "the repository's first poll", || {
        let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(if repos.contains("acme/sets") && !repos.contains("\"last_polled_at\":null") { Ok(()) } else { Err(repos) })
    })?;
    std::fs::write(repo.join("README.md"), "sets\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "go"])?;

    // v1: the pipeline's run, a and c queued, b waiting on a.
    let run = until(&s, 120, "the pipeline's run, its jobs queued", || {
        let runs = get(&s, "/api/runs?repo=acme/sets")?;
        let list = runs.as_array().cloned().or_else(|| runs.get("runs").and_then(Value::as_array).cloned()).unwrap_or_default();
        for r in list.iter().filter(|r| text(r, "pipeline") == "v1") {
            let id = text(r, "id").to_owned();
            let jobs = by_key(get(&s, &format!("/api/runs/{id}"))?.get("jobs").and_then(Value::as_array));
            if jobs.get("a").is_some_and(|j| j.1 == "queued") && jobs.get("c").is_some_and(|j| j.1 == "queued") {
                return Ok(Ok(id));
            }
        }
        Ok(Err(runs.to_string()))
    })?;
    let v1 = |s: &Server| -> anyhow::Result<std::collections::HashMap<String, (String, String, String)>> { Ok(by_key(get(s, &format!("/api/runs/{run}"))?.get("jobs").and_then(Value::as_array))) };
    let jobs = v1(&s)?;
    let id = |k: &str| jobs.get(k).map(|j| j.0.clone()).unwrap_or_default();
    let (a, c) = (id("a"), id("c"));

    // Cancel a, and one that isn't the run's: a stops, b is skipped with it, c goes on.
    let (st, v) = post(&s, &format!("/api/runs/{run}/cancel"), &json!({ "jobs": [a, "nope"] }))?;
    anyhow::ensure!(st == 200, "cancel with jobs: {st} {v}");
    anyhow::ensure!(said(&v, &a, "cancelled") == (true, String::new()), "{v}");
    anyhow::ensure!(said(&v, "nope", "cancelled") == (false, "not a job of this run".to_owned()), "{v}");
    let now = v1(&s)?;
    anyhow::ensure!(now.get("a").map(|j| j.1.as_str()) == Some("cancelled"), "{now:?}");
    anyhow::ensure!(now.get("b").map(|j| (j.1.as_str(), j.2.as_str())) == Some(("skipped", "dependency cancelled")), "{now:?}");
    anyhow::ensure!(now.get("c").map(|j| j.1.as_str()) == Some("queued"), "the run's other job goes on: {now:?}");
    // c is still to finish; a finished one is refused, saying how it ended.
    let (_, v) = post(&s, &format!("/api/runs/{run}/cancel"), &json!({ "jobs": [a] }))?;
    anyhow::ensure!(said(&v, &a, "cancelled").1.contains("already finished: it's cancelled"), "{v}");

    // Retry a: its next attempt waits in the queue again, and b waits on it.
    let (st, v) = post(&s, &format!("/api/runs/{run}/retry"), &json!({ "jobs": [a] }))?;
    anyhow::ensure!(st == 200, "retry with jobs: {st} {v}");
    anyhow::ensure!(said(&v, &a, "again").0, "{v}");
    let now = v1(&s)?;
    anyhow::ensure!(now.get("a").map(|j| j.1.as_str()) == Some("queued") && now.get("b").map(|j| j.1.as_str()) == Some("pending"), "{now:?}");
    // A job that hasn't finished can't run again yet: refused, saying so.
    let (_, v) = post(&s, &format!("/api/runs/{run}/retry"), &json!({ "jobs": [c] }))?;
    anyhow::ensure!(!said(&v, &c, "again").0 && said(&v, &c, "again").1.contains("hasn't finished"), "{v}");

    // Without `jobs`, as before: the whole run cancelled (204), then its failed and cancelled
    // jobs run again (`{ jobs: n }`).
    let (st, _) = http(s.port, "POST", &format!("/api/runs/{run}/cancel"), &s.admin, None)?;
    anyhow::ensure!(st == 204, "cancel without jobs: {st}");
    anyhow::ensure!(v1(&s)?.values().all(|j| j.1 == "cancelled" || j.1 == "skipped"), "{:?}", v1(&s)?);
    let (st, v) = post(&s, &format!("/api/runs/{run}/retry"), &json!({}))?;
    anyhow::ensure!(st == 200 && v.get("jobs").and_then(Value::as_u64) == Some(3), "retry without jobs: {st} {v}");

    // The engine's run of the workflow: a and c queued, b waiting on a.
    let ci = until(&s, 120, "the workflow's run, its legs queued", || {
        let runs = get(&s, "/api/runs?repo=acme/sets")?;
        let list = runs.as_array().cloned().or_else(|| runs.get("runs").and_then(Value::as_array).cloned()).unwrap_or_default();
        for r in list.iter().filter(|r| text(r, "pipeline") == "v2") {
            let id = text(r, "id").to_owned();
            let legs = by_key(get(&s, &format!("/api/ci/runs/{id}"))?.get("legs").and_then(Value::as_array));
            if legs.get("a").is_some_and(|j| j.1 == "queued") && legs.get("c").is_some_and(|j| j.1 == "queued") {
                return Ok(Ok(id));
            }
        }
        Ok(Err(runs.to_string()))
    })?;
    let v2 = |s: &Server| -> anyhow::Result<std::collections::HashMap<String, (String, String, String)>> { Ok(by_key(get(s, &format!("/api/ci/runs/{ci}"))?.get("legs").and_then(Value::as_array))) };
    let legs = v2(&s)?;
    let leg = |k: &str| legs.get(k).map(|j| j.0.clone()).unwrap_or_default();
    let (la, lc) = (leg("a"), leg("c"));

    // Cancel the leg a: it ends cancelled, b is skipped, c goes on.
    let (st, v) = post(&s, &format!("/api/ci/runs/{ci}/cancel"), &json!({ "jobs": [la, "nope"] }))?;
    anyhow::ensure!(st == 200, "engine cancel with jobs: {st} {v}");
    anyhow::ensure!(said(&v, &la, "cancelled").0 && !said(&v, "nope", "cancelled").0, "{v}");
    until(&s, 60, "the leg a cancelled and b skipped, c still queued", || {
        let now = v2(&s)?;
        let ok = now.get("a").is_some_and(|j| j.1 == "cancelled") && now.get("b").is_some_and(|j| j.1 == "skipped") && now.get("c").is_some_and(|j| j.1 == "queued");
        Ok(if ok { Ok(()) } else { Err(format!("{now:?}")) })
    })?;
    // A leg yet to finish can't be run again.
    let (_, v) = post(&s, &format!("/api/ci/runs/{ci}/rerun"), &json!({ "mode": "set", "jobs": [lc] }))?;
    anyhow::ensure!(said(&v, &lc, "again").1.contains("hasn't finished"), "{v}");
    // c cancelled too, the run ends; then the cancelled a runs again in a new attempt, with b.
    let (st, v) = post(&s, &format!("/api/ci/runs/{ci}/cancel"), &json!({ "jobs": [lc] }))?;
    anyhow::ensure!(st == 200 && said(&v, &lc, "cancelled").0, "{v}");
    until(&s, 60, "the run to end", || {
        let r = get(&s, &format!("/api/ci/runs/{ci}"))?;
        let status = r.get("run").map_or_else(|| text(&r, "status").to_owned(), |x| text(x, "status").to_owned());
        Ok(if status == "cancelled" { Ok(()) } else { Err(r.to_string()) })
    })?;
    let (st, v) = post(&s, &format!("/api/ci/runs/{ci}/rerun"), &json!({ "mode": "set", "jobs": [la] }))?;
    anyhow::ensure!(st == 201 && said(&v, &la, "again").0 && v.get("attempt").and_then(Value::as_u64).is_some(), "engine rerun of a set: {st} {v}");
    // And without `jobs`, the engine's cancel does what it did: the whole run.
    let (st, _) = http(s.port, "POST", &format!("/api/ci/runs/{ci}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 202 || st == 204, "engine cancel without jobs: {st}");
    Ok(())
}
