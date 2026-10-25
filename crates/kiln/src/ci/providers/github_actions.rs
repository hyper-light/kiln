//! GitHub Actions as a CI provider (docs/ci.md W23-R03), through the GitHub App: its workflow runs,
//! their jobs and steps kept in kiln's tables as they change (the App's `workflow_run`,
//! `workflow_job` and `deployment_review` events, and a look every 20 seconds); a job's log and a
//! run's artifacts read from GitHub when asked for; reruns (all, failed jobs, one job), cancels,
//! `workflow_dispatch` and deployment reviews sent to GitHub; environments and their deployments.

use anyhow::{anyhow, bail};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::{ExtJob, ExtRun, ExtStep, GITHUB_ACTIONS, kiln_status, unix};
use crate::githost::github::{self, App, Auth};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

async fn app_for(hub: &Hub, repo: &Repo) -> anyhow::Result<(std::sync::Arc<App>, String)> {
    let app = github::app(hub).await.ok_or_else(|| anyhow!("no GitHub App here"))?;
    let name = app.name_of(repo).ok_or_else(|| anyhow!("{} isn't on GitHub", repo.name))?;
    Ok((app, name))
}

async fn token(app: &App, name: &str, write: bool) -> anyhow::Result<String> {
    let p = [("actions".to_owned(), if write { "write" } else { "read" }.to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
    app.token(name, &p).await
}

/// A workflow run of GitHub's API, in kiln's words.
pub fn ext_run(v: &Value) -> ExtRun {
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (status, conclusion) = (s("/status"), v.get("conclusion").and_then(Value::as_str).map(str::to_owned));
    let k = kiln_status(&status, conclusion.as_deref());
    let finished = (status == "completed").then(|| unix(v.get("updated_at"))).flatten();
    ExtRun {
        external_id: v.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default(),
        number: v.get("run_number").and_then(Value::as_i64).unwrap_or(0),
        name: s("/name"),
        path: s("/path"),
        title: v.get("display_title").and_then(Value::as_str).map(str::to_owned),
        event: s("/event"),
        branch: s("/head_branch"),
        sha: s("/head_sha"),
        actor: v.pointer("/triggering_actor/login").or_else(|| v.pointer("/actor/login")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        status: k.to_owned(),
        conclusion,
        attempt: v.get("run_attempt").and_then(Value::as_i64).unwrap_or(1),
        url: s("/html_url"),
        created_at: unix(v.get("created_at")).unwrap_or(0),
        started_at: unix(v.get("run_started_at")),
        finished_at: finished,
    }
}

/// A run's jobs, in kiln's words.
pub fn ext_jobs(v: &Value) -> Vec<ExtJob> {
    v.get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|j| {
            let s = |p: &str| j.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
            let conclusion = j.get("conclusion").and_then(Value::as_str).map(str::to_owned);
            let steps = j
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|st| {
                    let c = st.get("conclusion").and_then(Value::as_str).map(str::to_owned);
                    ExtStep {
                        number: st.get("number").and_then(Value::as_i64).unwrap_or(0),
                        name: st.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                        status: kiln_status(st.get("status").and_then(Value::as_str).unwrap_or_default(), c.as_deref()).to_owned(),
                        conclusion: c,
                        started_at: unix(st.get("started_at")),
                        finished_at: unix(st.get("completed_at")),
                    }
                })
                .collect();
            ExtJob {
                external_id: j.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default(),
                name: s("/name"),
                status: kiln_status(&s("/status"), conclusion.as_deref()).to_owned(),
                conclusion,
                runner: j.get("runner_name").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned),
                labels: j.get("labels").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
                url: s("/html_url"),
                started_at: unix(j.get("started_at")),
                finished_at: unix(j.get("completed_at")),
                steps,
            }
        })
        .collect()
}

/// One workflow run and its jobs, kept: its kiln id.
pub async fn sync_run(hub: &Hub, repo: &Repo, run: &Value) -> anyhow::Result<String> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    let r = ext_run(run);
    let (st, jobs) = app.call(Method::Get, &format!("/repos/{name}/actions/runs/{}/jobs?per_page=100", r.external_id), Auth::Token(&t), None).await?;
    let jobs = (st == 200).then(|| ext_jobs(&jobs));
    super::record(hub, repo, GITHUB_ACTIONS, &r, jobs.as_deref()).await
}

/// The repository's recent workflow runs, kept: those kiln hasn't seen finish.
pub async fn sync(hub: &Hub, repo: &Repo) -> anyhow::Result<()> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    let (st, v) = app.call(Method::Get, &format!("/repos/{name}/actions/runs?per_page=30"), Auth::Token(&t), None).await?;
    if st != 200 {
        bail!("GitHub's runs ({st}): {}", github::said(&v));
    }
    for run in v.get("workflow_runs").and_then(Value::as_array).into_iter().flatten() {
        let r = ext_run(run);
        let (repo_id, ext, attempt) = (repo.id.clone(), r.external_id.clone(), r.attempt);
        let done: Option<(String, i64)> = hub
            .store
            .call(move |c| Ok(c.query_row("SELECT status, attempt FROM runs WHERE repo_id = ?1 AND provider = ?2 AND external_id = ?3", params![repo_id, GITHUB_ACTIONS, ext], |x| Ok((x.get(0)?, x.get(1)?))).optional()?))
            .await?;
        let settled = done.is_some_and(|(s, a)| a == attempt && matches!(s.as_str(), "succeeded" | "failed" | "cancelled" | "skipped" | "timed_out") && r.status == s);
        if !settled {
            sync_run(hub, repo, run).await?;
        }
    }
    Ok(())
}

/// The App's `workflow_run` and `workflow_job` events: the run they're about, kept now.
pub async fn on_event(hub: &Hub, event: &str, v: &Value) -> anyhow::Result<()> {
    let full = v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default();
    let Some(app) = github::app(hub).await else { return Ok(()) };
    let repos = hub.store.repos().await?;
    let Some(repo) = repos.into_iter().find(|r| app.name_of(r).is_some_and(|n| n.eq_ignore_ascii_case(full))) else { return Ok(()) };
    if !super::chosen(hub, &repo).await.iter().any(|p| p == GITHUB_ACTIONS) {
        return Ok(());
    }
    let run = match event {
        "workflow_run" | "deployment_review" => v.get("workflow_run").cloned(),
        _ => {
            let id = v.pointer("/workflow_job/run_id").and_then(Value::as_u64).ok_or_else(|| anyhow!("a job event without its run"))?;
            let name = app.name_of(&repo).unwrap_or_default();
            let t = token(&app, &name, false).await?;
            let (st, r) = app.call(Method::Get, &format!("/repos/{name}/actions/runs/{id}"), Auth::Token(&t), None).await?;
            (st == 200).then_some(r)
        }
    };
    if let Some(run) = run {
        let id = sync_run(hub, &repo, &run).await?;
        hub.emit_run(&id).await;
    }
    Ok(())
}

/// Reruns: `all`, `failed` (jobs), or one `job` (its kiln leg id).
pub async fn rerun(hub: &Hub, repo: &Repo, run: &str, mode: &str, job: Option<&str>) -> anyhow::Result<Value> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, true).await?;
    let path = match (mode, job) {
        ("job", Some(leg)) => {
            let key = leg.to_owned();
            let ext: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT external_id FROM jobs WHERE id = ?1", [&key], |r| r.get(0)).optional()?.flatten())).await?;
            format!("/repos/{name}/actions/jobs/{}/rerun", ext.ok_or_else(|| anyhow!("no such job on GitHub"))?)
        }
        ("failed", _) => format!("/repos/{name}/actions/runs/{run}/rerun-failed-jobs"),
        _ => format!("/repos/{name}/actions/runs/{run}/rerun"),
    };
    let (st, v) = app.call(Method::Post, &path, Auth::Token(&t), Some(&json!({}))).await?;
    if st != 201 {
        bail!("GitHub didn't rerun it ({st}): {}", github::said(&v));
    }
    Ok(json!({ "provider": GITHUB_ACTIONS, "rerun": mode }))
}

pub async fn cancel(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Value> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, true).await?;
    let (st, v) = app.call(Method::Post, &format!("/repos/{name}/actions/runs/{run}/cancel"), Auth::Token(&t), Some(&json!({}))).await?;
    if st != 202 {
        bail!("GitHub didn't cancel it ({st}): {}", github::said(&v));
    }
    Ok(json!({ "provider": GITHUB_ACTIONS, "cancelled": true }))
}

/// `workflow_dispatch` of `workflow` (its file name or id) at `git_ref` with `inputs`.
pub async fn dispatch(hub: &Hub, repo: &Repo, workflow: &str, git_ref: &str, inputs: &Value) -> anyhow::Result<()> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, true).await?;
    let file = workflow.rsplit('/').next().unwrap_or(workflow);
    let (st, v) = app.call(Method::Post, &format!("/repos/{name}/actions/workflows/{file}/dispatches"), Auth::Token(&t), Some(&json!({ "ref": git_ref, "inputs": inputs }))).await?;
    if st != 204 {
        bail!("GitHub didn't start it ({st}): {}", github::said(&v));
    }
    Ok(())
}

/// A job's log: GitHub answers with a redirect to it, which is read without the token.
pub async fn logs(hub: &Hub, repo: &Repo, job: &str) -> anyhow::Result<String> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    let b = redirected(&app, &t, &format!("/repos/{name}/actions/jobs/{job}/logs"), 64 << 20).await?;
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The run's artifacts: `[{ id, name, size, created_at, expires_at, expired }]`.
pub async fn artifacts(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Vec<Value>> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    let (st, v) = app.call(Method::Get, &format!("/repos/{name}/actions/runs/{run}/artifacts?per_page=100"), Auth::Token(&t), None).await?;
    if st != 200 {
        bail!("GitHub's artifacts ({st}): {}", github::said(&v));
    }
    Ok(v.get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| {
            json!({
                "id": a.get("id").and_then(Value::as_u64).map(|n| n.to_string()),
                "name": a.get("name").and_then(Value::as_str).unwrap_or_default(),
                "size": a.get("size_in_bytes").and_then(Value::as_i64),
                "created_at": unix(a.get("created_at")),
                "expires_at": unix(a.get("expires_at")),
                "expired": a.get("expired").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect())
}

/// One artifact's zip: GitHub answers with a redirect to it, which is read without the token.
pub async fn artifact(hub: &Hub, repo: &Repo, id: &str) -> anyhow::Result<bytes::Bytes> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    redirected(&app, &t, &format!("/repos/{name}/actions/artifacts/{id}/zip"), 2 << 30).await
}

/// A download GitHub redirects to storage for (a log, an artifact).
async fn redirected(app: &App, token: &str, path: &str, limit: usize) -> anyhow::Result<bytes::Bytes> {
    let client = crate::http::client::github()?;
    let resp = client.request(Method::Get, &format!("{}{path}", app.api)).bearer(token).header("Accept", "application/vnd.github+json").header("User-Agent", "kiln").send().await?;
    let status = resp.status();
    if (300..400).contains(&status) {
        let to = resp.header("location").ok_or_else(|| anyhow!("GitHub's redirect has no location"))?.to_owned();
        let r = client.request(Method::Get, &to).header("User-Agent", "kiln").timeout(std::time::Duration::from_secs(120)).send().await?;
        anyhow::ensure!(r.is_success(), "the download ({})", r.status());
        return r.bytes(limit).await;
    }
    anyhow::ensure!(status == 200, "GitHub has nothing there ({status})");
    resp.bytes(limit).await
}

/// The run's deployments waiting for review: `[{ environment, environment_id, wait_timer, can_approve }]`.
pub async fn pending(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Vec<Value>> {
    let (app, name) = app_for(hub, repo).await?;
    let t = token(&app, &name, false).await?;
    let (st, v) = app.call(Method::Get, &format!("/repos/{name}/actions/runs/{run}/pending_deployments"), Auth::Token(&t), None).await?;
    if st != 200 {
        bail!("GitHub's pending deployments ({st}): {}", github::said(&v));
    }
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            json!({
                "environment": p.pointer("/environment/name").and_then(Value::as_str).unwrap_or_default(),
                "environment_id": p.pointer("/environment/id").and_then(Value::as_u64),
                "wait_timer": p.get("wait_timer").and_then(Value::as_i64),
                "can_approve": p.get("current_user_can_approve").and_then(Value::as_bool),
            })
        })
        .collect())
}

/// Approves or rejects the run's deployments waiting for `environments` (every one when none is
/// named). GitHub takes reviews from its required reviewers, people: the person's own token when
/// they signed in with GitHub (`as_person`), else the App's, which GitHub accepts only where the App
/// may review.
pub async fn approve(hub: &Hub, repo: &Repo, run: &str, environments: &[String], approved: bool, comment: &str, as_person: Option<&str>) -> anyhow::Result<Value> {
    let (app, name) = app_for(hub, repo).await?;
    let waiting = pending(hub, repo, run).await?;
    let ids: Vec<u64> = waiting
        .iter()
        .filter(|p| environments.is_empty() || p.get("environment").and_then(Value::as_str).is_some_and(|e| environments.iter().any(|n| n.eq_ignore_ascii_case(e))))
        .filter_map(|p| p.get("environment_id").and_then(Value::as_u64))
        .collect();
    if ids.is_empty() {
        bail!("the run waits for no review{}", if environments.is_empty() { String::new() } else { format!(" of {}", environments.join(", ")) });
    }
    let t = match as_person {
        Some(t) => t.to_owned(),
        None => {
            let p = [("actions".to_owned(), "write".to_owned()), ("deployments".to_owned(), "write".to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
            app.token(&name, &p).await?
        }
    };
    let body = json!({ "environment_ids": ids, "state": if approved { "approved" } else { "rejected" }, "comment": comment });
    let (st, v) = app.call(Method::Post, &format!("/repos/{name}/actions/runs/{run}/pending_deployments"), Auth::Token(&t), Some(&body)).await?;
    if st != 200 {
        bail!("GitHub took no review ({st}): {}", github::said(&v));
    }
    Ok(json!({ "provider": GITHUB_ACTIONS, "environments": ids, "state": if approved { "approved" } else { "rejected" }, "as": if as_person.is_some() { "person" } else { "app" } }))
}

/// The repository's environments on GitHub, each with its latest deployment.
pub async fn environments(hub: &Hub, repo: &Repo) -> anyhow::Result<Vec<Value>> {
    let (app, name) = app_for(hub, repo).await?;
    let p = [("deployments".to_owned(), "read".to_owned()), ("actions".to_owned(), "read".to_owned()), ("metadata".to_owned(), "read".to_owned())].into();
    let t = app.token(&name, &p).await?;
    let (st, v) = app.call(Method::Get, &format!("/repos/{name}/environments?per_page=30"), Auth::Token(&t), None).await?;
    if st != 200 {
        bail!("GitHub's environments ({st}): {}", github::said(&v));
    }
    let mut out = Vec::new();
    for e in v.get("environments").and_then(Value::as_array).into_iter().flatten().take(20) {
        let env = e.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (st, deps) = app.call(Method::Get, &format!("/repos/{name}/deployments?environment={}&per_page=1", crate::ci::cli::enc(&env)), Auth::Token(&t), None).await?;
        let last = (st == 200).then(|| deps.as_array().and_then(|l| l.first()).cloned()).flatten();
        let state = match last.as_ref().and_then(|d| d.get("id")).and_then(Value::as_u64) {
            Some(id) => {
                let (st, s) = app.call(Method::Get, &format!("/repos/{name}/deployments/{id}/statuses?per_page=1"), Auth::Token(&t), None).await?;
                (st == 200).then(|| s.pointer("/0/state").and_then(Value::as_str).map(str::to_owned)).flatten()
            }
            None => None,
        };
        out.push(json!({
            "provider": GITHUB_ACTIONS,
            "name": env,
            "deployment": last.map(|d| json!({ "state": state, "commit": d.get("sha"), "ref": d.get("ref"), "created_at": unix(d.get("created_at")) })),
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r03_shapes() {
        let run = json!({ "id": 30433642, "name": "ci", "path": ".github/workflows/ci.yml", "display_title": "fix: x", "event": "push", "head_branch": "main", "head_sha": "abc",
                          "status": "completed", "conclusion": "failure", "run_number": 7, "run_attempt": 2, "html_url": "https://github.com/acme/app/actions/runs/30433642",
                          "created_at": "2026-10-18T12:00:00Z", "run_started_at": "2026-10-18T12:00:05Z", "updated_at": "2026-10-18T12:03:00Z", "actor": { "login": "ada" } });
        let r = ext_run(&run);
        assert_eq!((r.external_id.as_str(), r.status.as_str(), r.attempt, r.number, r.actor.as_str()), ("30433642", "failed", 2, 7, "ada"));
        assert_eq!(r.finished_at.zip(r.started_at).map(|(f, s)| f - s), Some(175));
        let jobs = json!({ "jobs": [{ "id": 399444496, "name": "build", "status": "completed", "conclusion": "success", "runner_name": "r1", "labels": ["ubuntu-latest"],
                                      "started_at": "2026-10-18T12:00:06Z", "completed_at": "2026-10-18T12:01:00Z", "html_url": "u",
                                      "steps": [{ "name": "Set up job", "status": "completed", "conclusion": "success", "number": 1 }, { "name": "test", "status": "in_progress", "conclusion": null, "number": 2 }] }] });
        let j = ext_jobs(&jobs);
        assert_eq!(j.len(), 1);
        let first = j.first().cloned().unwrap_or_default();
        assert_eq!((first.status.as_str(), first.runner.as_deref(), first.steps.len()), ("succeeded", Some("r1"), 2));
        assert_eq!(first.steps.get(1).map(|s| s.status.as_str()), Some("running"));
    }
}
