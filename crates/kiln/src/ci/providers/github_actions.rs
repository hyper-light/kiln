//! GitHub Actions as a CI provider (docs/ci.md W23-R03), through the GitHub App: its workflow runs,
//! their jobs and steps kept in kiln's tables as they change (the App's `workflow_run` and
//! `workflow_job` events, and a look every 20 seconds); a job's log read from GitHub when asked for;
//! reruns (all, failed jobs, one job), cancels and `workflow_dispatch` sent to GitHub.

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
        "workflow_run" => v.get("workflow_run").cloned(),
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
    let client = crate::http::client::github()?;
    let resp = client.request(Method::Get, &format!("{}/repos/{name}/actions/jobs/{job}/logs", app.api)).bearer(&t).header("Accept", "application/vnd.github+json").header("User-Agent", "kiln").send().await?;
    let status = resp.status();
    if (300..400).contains(&status) {
        let to = resp.header("location").ok_or_else(|| anyhow!("GitHub's redirect has no location"))?.to_owned();
        let r = client.request(Method::Get, &to).header("User-Agent", "kiln").send().await?;
        anyhow::ensure!(r.is_success(), "the log ({})", r.status());
        return Ok(String::from_utf8_lossy(&r.bytes(64 << 20).await?).into_owned());
    }
    anyhow::ensure!(status == 200, "GitHub has no log for it ({status})");
    Ok(String::from_utf8_lossy(&resp.bytes(64 << 20).await?).into_owned())
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
