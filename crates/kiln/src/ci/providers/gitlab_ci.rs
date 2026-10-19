//! GitLab CI/CD as a CI provider (docs/ci.md W23-R04), through the GitLab host's API
//! (`crate::githost::gitlab`): pipelines, their jobs and traces kept in kiln's tables as they change
//! (pipeline, job and deployment webhooks, and a look every 20 seconds); retries, cancels, manual
//! jobs played, new pipelines with variables; job artifacts; protected environments' deployment
//! approvals; `.gitlab-ci.yml` checked with GitLab's CI lint.
//!
//! GitLab's jobs have no steps: a job is a leg, its trace the leg's log. A retried job is a new job
//! in the same pipeline, so a pipeline keeps attempt 1 and its legs are the jobs GitLab lists now.

use std::sync::Arc;

use anyhow::{anyhow, bail};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::{ExtJob, ExtRun, GITLAB_CI, kiln_status, unix};
use crate::githost::gitlab::{self, Gitlab, id, said};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

async fn host_for(hub: &Hub, repo: &Repo) -> anyhow::Result<(Arc<Gitlab>, String)> {
    gitlab::for_repo(hub, repo).await.ok_or_else(|| anyhow!("{} isn't on a GitLab kiln reaches", repo.name))
}

/// GitLab's status in GitHub's words (status, conclusion), which kiln's are made from.
fn words(s: &str) -> (&'static str, Option<&'static str>) {
    match s {
        "running" | "canceling" => ("in_progress", None),
        "success" => ("completed", Some("success")),
        "failed" => ("completed", Some("failure")),
        "canceled" => ("completed", Some("cancelled")),
        "skipped" => ("completed", Some("skipped")),
        "manual" => ("waiting", None),
        // created, waiting_for_resource, preparing, pending, scheduled
        _ => ("queued", None),
    }
}

/// What started a pipeline, in kiln's (GitHub's) words.
fn event_of(source: &str) -> String {
    match source {
        "merge_request_event" | "external_pull_request_event" => "pull_request",
        "web" | "api" | "chat" => "workflow_dispatch",
        "trigger" => "repository_dispatch",
        other => other,
    }
    .to_owned()
}

/// A pipeline of GitLab's API (`GET /projects/:id/pipelines/:id`), in kiln's words; GitLab's own
/// status kept as the conclusion once it's done.
pub fn ext_run(p: &Value) -> ExtRun {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let raw = s("status");
    let (status, conclusion) = words(&raw);
    ExtRun {
        external_id: p.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default(),
        number: p.get("iid").and_then(Value::as_i64).unwrap_or(0),
        name: p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("pipeline").to_owned(),
        path: ".gitlab-ci.yml".into(),
        title: p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_owned),
        event: event_of(&s("source")),
        branch: s("ref"),
        sha: s("sha"),
        actor: p.pointer("/user/username").and_then(Value::as_str).unwrap_or_default().to_owned(),
        status: kiln_status(status, conclusion).to_owned(),
        conclusion: conclusion.map(|_| raw.clone()),
        attempt: 1,
        url: s("web_url"),
        created_at: unix(p.get("created_at")).unwrap_or(0),
        started_at: unix(p.get("started_at")),
        finished_at: unix(p.get("finished_at")),
    }
}

/// A pipeline's jobs (`GET /projects/:id/pipelines/:id/jobs`), in kiln's words.
pub fn ext_jobs(v: &Value) -> Vec<ExtJob> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let raw = j.get("status").and_then(Value::as_str).unwrap_or_default();
            let (status, conclusion) = words(raw);
            ExtJob {
                external_id: j.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default(),
                name: j.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                status: kiln_status(status, conclusion).to_owned(),
                conclusion: conclusion.map(|_| raw.to_owned()),
                runner: j.pointer("/runner/description").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_owned),
                labels: j.get("tag_list").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
                url: j.get("web_url").and_then(Value::as_str).unwrap_or_default().to_owned(),
                started_at: unix(j.get("started_at")),
                finished_at: unix(j.get("finished_at")),
                steps: Vec::new(),
            }
        })
        .collect()
}

/// Every page of a list (GitLab gives 100 at most at a time), up to 2,000 items.
async fn all(host: &Gitlab, path: &str) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::new();
    let join = if path.contains('?') { '&' } else { '?' };
    for page in 1..=20 {
        let (st, v) = host.call(Method::Get, &format!("{path}{join}per_page=100&page={page}"), None).await?;
        if st != 200 {
            bail!("GitLab ({st}): {}", said(&v));
        }
        let items = v.as_array().cloned().unwrap_or_default();
        let n = items.len();
        out.extend(items);
        if n < 100 {
            break;
        }
    }
    Ok(out)
}

/// One pipeline and its jobs, kept: its kiln id.
pub async fn sync_run(hub: &Hub, repo: &Repo, pipeline: &str) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, p) = host.call(Method::Get, &format!("/projects/{}/pipelines/{pipeline}", id(&name)), None).await?;
    if st != 200 {
        bail!("GitLab's pipeline {pipeline} ({st}): {}", said(&p));
    }
    let jobs = all(&host, &format!("/projects/{}/pipelines/{pipeline}/jobs", id(&name))).await?;
    let jobs = ext_jobs(&Value::Array(jobs));
    super::record(hub, repo, GITLAB_CI, &ext_run(&p), Some(&jobs)).await
}

/// The project's recent pipelines, kept: those kiln hasn't seen finish.
pub async fn sync(hub: &Hub, repo: &Repo) -> anyhow::Result<()> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, v) = host.call(Method::Get, &format!("/projects/{}/pipelines?per_page=30&order_by=id&sort=desc", id(&name)), None).await?;
    if st != 200 {
        bail!("GitLab's pipelines ({st}): {}", said(&v));
    }
    for p in v.as_array().into_iter().flatten() {
        let r = ext_run(p);
        let (repo_id, ext) = (repo.id.clone(), r.external_id.clone());
        let kept: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT status FROM runs WHERE repo_id = ?1 AND provider = ?2 AND external_id = ?3", params![repo_id, GITLAB_CI, ext], |x| x.get(0)).optional()?)).await?;
        let settled = kept.is_some_and(|s| s == r.status && matches!(s.as_str(), "succeeded" | "failed" | "cancelled" | "skipped" | "timed_out"));
        if !settled {
            sync_run(hub, repo, &r.external_id).await?;
        }
    }
    Ok(())
}

/// A webhook delivery GitLab signed (its `X-Gitlab-Event`): the pipeline it's about, kept now.
pub async fn on_event(hub: &Hub, repo: &Repo, event: &str, v: &Value) -> anyhow::Result<()> {
    if !super::chosen(hub, repo).await.iter().any(|p| p == GITLAB_CI) {
        return Ok(());
    }
    let pipeline = match event {
        "Pipeline Hook" => v.pointer("/object_attributes/id").and_then(Value::as_u64),
        "Job Hook" => v.get("pipeline_id").and_then(Value::as_u64),
        "Deployment Hook" => {
            // The deployment's job, and the job's pipeline.
            let Some(job) = v.get("deployable_id").and_then(Value::as_u64) else { return Ok(()) };
            let (host, name) = host_for(hub, repo).await?;
            let (st, j) = host.call(Method::Get, &format!("/projects/{}/jobs/{job}", id(&name)), None).await?;
            (st == 200).then(|| j.pointer("/pipeline/id").and_then(Value::as_u64)).flatten()
        }
        _ => None,
    };
    if let Some(p) = pipeline {
        let run = sync_run(hub, repo, &p.to_string()).await?;
        hub.emit_run(&run).await;
    }
    Ok(())
}

/// The leg's job id on GitLab.
async fn job_of(hub: &Hub, leg: &str) -> anyhow::Result<String> {
    let key = leg.to_owned();
    let ext: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT external_id FROM jobs WHERE id = ?1", [&key], |r| r.get(0)).optional()?.flatten())).await?;
    ext.ok_or_else(|| anyhow!("no such job on GitLab"))
}

/// Reruns: `failed` (GitLab's retry of a pipeline's failed and canceled jobs), one `job` (retried,
/// or played when it's a manual job waiting), `play` (a manual job), or `all`: a new pipeline on the
/// same branch or tag, or merge request, when it is still at the pipeline's commit (GitLab starts
/// pipelines only on a head, and retrying every job at once would run them out of their stages'
/// order).
pub async fn rerun(hub: &Hub, repo: &Repo, run: &str, mode: &str, job: Option<&str>) -> anyhow::Result<Value> {
    let (host, name) = host_for(hub, repo).await?;
    let pid = id(&name);
    match (mode, job) {
        ("job" | "play", Some(leg)) => {
            let j = job_of(hub, leg).await?;
            let (st, now) = host.call(Method::Get, &format!("/projects/{pid}/jobs/{j}"), None).await?;
            anyhow::ensure!(st == 200, "GitLab's job {j} ({st}): {}", said(&now));
            let manual = now.get("status").and_then(Value::as_str) == Some("manual");
            let verb = if mode == "play" || manual { "play" } else { "retry" };
            let (st, v) = host.call(Method::Post, &format!("/projects/{pid}/jobs/{j}/{verb}"), None).await?;
            anyhow::ensure!(st == 200 || st == 201, "GitLab didn't {verb} the job ({st}): {}", said(&v));
            sync_run(hub, repo, run).await?;
            Ok(json!({ "provider": GITLAB_CI, "rerun": mode, "job": verb }))
        }
        ("failed", _) => {
            let (st, v) = host.call(Method::Post, &format!("/projects/{pid}/pipelines/{run}/retry"), None).await?;
            anyhow::ensure!(st == 201 || st == 200, "GitLab didn't retry it ({st}): {}", said(&v));
            sync_run(hub, repo, run).await?;
            Ok(json!({ "provider": GITLAB_CI, "rerun": "failed" }))
        }
        ("all", _) => {
            let (st, p) = host.call(Method::Get, &format!("/projects/{pid}/pipelines/{run}"), None).await?;
            anyhow::ensure!(st == 200, "GitLab's pipeline {run} ({st}): {}", said(&p));
            let (git_ref, sha) = (p.get("ref").and_then(Value::as_str).unwrap_or_default().to_owned(), p.get("sha").and_then(Value::as_str).unwrap_or_default().to_owned());
            let made = if let Some(mr) = git_ref.strip_prefix("refs/merge-requests/").and_then(|r| r.split('/').next()) {
                host.call(Method::Post, &format!("/projects/{pid}/merge_requests/{mr}/pipelines"), None).await?
            } else {
                let head = head_of(&host, &pid, &git_ref, p.get("tag").and_then(Value::as_bool).unwrap_or(false)).await?;
                if head != sha {
                    bail!("{git_ref} has moved on from {} since; GitLab runs a new pipeline only on a head (rerun failed jobs instead, or start one on {git_ref})", sha.get(..12).unwrap_or(&sha));
                }
                host.call(Method::Post, &format!("/projects/{pid}/pipeline"), Some(&json!({ "ref": git_ref }))).await?
            };
            let (st, v) = made;
            anyhow::ensure!(st == 201, "GitLab started no pipeline ({st}): {}", said(&v));
            let new = v.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default();
            let kiln = sync_run(hub, repo, &new).await?;
            Ok(json!({ "provider": GITLAB_CI, "rerun": "all", "run": kiln, "pipeline": new }))
        }
        (other, _) => bail!("rerun mode `{other}`: `all`, `failed`, `job` or `play` (with a `job`)"),
    }
}

/// Where a branch or tag is.
async fn head_of(host: &Gitlab, pid: &str, git_ref: &str, tag: bool) -> anyhow::Result<String> {
    let kind = if tag { "tags" } else { "branches" };
    let (st, v) = host.call(Method::Get, &format!("/projects/{pid}/repository/{kind}/{}", crate::ci::cli::enc(git_ref)), None).await?;
    anyhow::ensure!(st == 200, "{git_ref} on GitLab ({st}): {}", said(&v));
    Ok(v.pointer("/commit/id").and_then(Value::as_str).unwrap_or_default().to_owned())
}

pub async fn cancel(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Value> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, v) = host.call(Method::Post, &format!("/projects/{}/pipelines/{run}/cancel", id(&name)), None).await?;
    anyhow::ensure!(st == 200 || st == 201, "GitLab didn't cancel it ({st}): {}", said(&v));
    sync_run(hub, repo, run).await?;
    Ok(json!({ "provider": GITLAB_CI, "cancelled": true }))
}

/// A new pipeline on `git_ref` with `inputs` as its variables: its kiln id.
pub async fn dispatch(hub: &Hub, repo: &Repo, git_ref: &str, inputs: &Value) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let variables: Vec<Value> = inputs.as_object().into_iter().flatten().map(|(k, v)| json!({ "key": k, "value": v.as_str().map_or_else(|| v.to_string(), str::to_owned), "variable_type": "env_var" })).collect();
    let (st, v) = host.call(Method::Post, &format!("/projects/{}/pipeline", id(&name)), Some(&json!({ "ref": git_ref, "variables": variables }))).await?;
    anyhow::ensure!(st == 201, "GitLab started no pipeline ({st}): {}", said(&v));
    let new = v.get("id").and_then(Value::as_u64).map(|n| n.to_string()).unwrap_or_default();
    sync_run(hub, repo, &new).await
}

/// A job's trace.
pub async fn logs(hub: &Hub, repo: &Repo, job: &str) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, b) = host.bytes(&format!("/projects/{}/jobs/{job}/trace", id(&name)), 64 << 20).await?;
    anyhow::ensure!(st == 200, "GitLab has no trace for it ({st})");
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The pipeline's job artifacts: one archive per job that kept any.
pub async fn artifacts(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Vec<Value>> {
    let (host, name) = host_for(hub, repo).await?;
    let jobs = all(&host, &format!("/projects/{}/pipelines/{run}/jobs", id(&name))).await?;
    let now = crate::util::now_unix();
    Ok(jobs
        .iter()
        .filter_map(|j| {
            let file = j.get("artifacts_file").filter(|f| !f.is_null())?;
            let expires = unix(j.get("artifacts_expire_at"));
            Some(json!({
                "id": j.get("id").and_then(Value::as_u64).map(|n| n.to_string()),
                "name": j.get("name").and_then(Value::as_str).unwrap_or_default(),
                "file": file.get("filename").and_then(Value::as_str).unwrap_or("artifacts.zip"),
                "size": file.get("size").and_then(Value::as_i64),
                "created_at": unix(j.get("finished_at")),
                "expires_at": expires,
                "expired": expires.is_some_and(|e| e <= now),
            }))
        })
        .collect())
}

/// One job's artifacts archive.
pub async fn artifact(hub: &Hub, repo: &Repo, job: &str) -> anyhow::Result<bytes::Bytes> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, b) = host.bytes(&format!("/projects/{}/jobs/{job}/artifacts", id(&name)), 2 << 30).await?;
    anyhow::ensure!(st == 200, "GitLab has no artifacts for job {job} ({st})");
    Ok(b)
}

/// The pipeline's deployments waiting for approval (protected environments).
async fn blocked(host: &Gitlab, name: &str, run: &str) -> anyhow::Result<Vec<Value>> {
    let list = all(host, &format!("/projects/{}/deployments?status=blocked&order_by=id&sort=desc", id(name))).await?;
    Ok(list.into_iter().filter(|d| d.pointer("/deployable/pipeline/id").and_then(Value::as_u64).map(|n| n.to_string()).as_deref() == Some(run)).collect())
}

/// What the pipeline waits for: `[{ environment, deployment, job, approvals_left }]`.
pub async fn pending(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Vec<Value>> {
    let (host, name) = host_for(hub, repo).await?;
    Ok(blocked(&host, &name, run)
        .await?
        .iter()
        .map(|d| {
            json!({
                "environment": d.pointer("/environment/name").and_then(Value::as_str).unwrap_or_default(),
                "deployment": d.get("id").and_then(Value::as_u64),
                "job": d.pointer("/deployable/name").and_then(Value::as_str),
                "approvals_left": d.get("pending_approval_count").and_then(Value::as_i64),
            })
        })
        .collect())
}

/// Approves or rejects the pipeline's blocked deployments to `environments` (every one when none
/// is named), as the person when they signed in with this GitLab (`token`), else as kiln's token.
pub async fn approve(hub: &Hub, repo: &Repo, run: &str, environments: &[String], approved: bool, comment: &str, token: Option<&str>) -> anyhow::Result<Value> {
    let (host, name) = host_for(hub, repo).await?;
    let waiting = blocked(&host, &name, run).await?;
    let chosen: Vec<&Value> = waiting.iter().filter(|d| environments.is_empty() || d.pointer("/environment/name").and_then(Value::as_str).is_some_and(|e| environments.iter().any(|n| n == e))).collect();
    if chosen.is_empty() {
        bail!("the pipeline waits for no approval{}", if environments.is_empty() { String::new() } else { format!(" to {}", environments.join(", ")) });
    }
    let mut done = Vec::new();
    for d in chosen {
        let did = d.get("id").and_then(Value::as_u64).unwrap_or(0);
        let body = json!({ "status": if approved { "approved" } else { "rejected" }, "comment": comment });
        let path = format!("/projects/{}/deployments/{did}/approval", id(&name));
        let (st, v) = match token {
            Some(t) => host.call_as(t, Method::Post, &path, Some(&body)).await?,
            None => host.call(Method::Post, &path, Some(&body)).await?,
        };
        anyhow::ensure!(st == 201 || st == 200, "GitLab took no approval ({st}): {}", said(&v));
        done.push(d.pointer("/environment/name").and_then(Value::as_str).unwrap_or_default().to_owned());
    }
    sync_run(hub, repo, run).await?;
    Ok(json!({ "provider": GITLAB_CI, "environments": done, "state": if approved { "approved" } else { "rejected" } }))
}

/// The project's environments, each with its latest deployment.
pub async fn environments(hub: &Hub, repo: &Repo) -> anyhow::Result<Vec<Value>> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, envs) = host.call(Method::Get, &format!("/projects/{}/environments?states=available&per_page=100", id(&name)), None).await?;
    anyhow::ensure!(st == 200, "GitLab's environments ({st}): {}", said(&envs));
    let (st, deps) = host.call(Method::Get, &format!("/projects/{}/deployments?order_by=id&sort=desc&per_page=100", id(&name)), None).await?;
    let deps = if st == 200 { deps.as_array().cloned().unwrap_or_default() } else { Vec::new() };
    Ok(envs
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let env = e.get("name").and_then(Value::as_str).unwrap_or_default();
            let last = deps.iter().find(|d| d.pointer("/environment/name").and_then(Value::as_str) == Some(env));
            json!({
                "provider": GITLAB_CI,
                "name": env,
                "kind": e.get("tier").and_then(Value::as_str),
                "url": e.get("external_url").and_then(Value::as_str),
                "deployment": last.map(|d| json!({
                    "state": d.get("status").and_then(Value::as_str),
                    "commit": d.get("sha").and_then(Value::as_str),
                    "ref": d.get("ref").and_then(Value::as_str),
                    "pipeline": d.pointer("/deployable/pipeline/id").and_then(Value::as_u64).map(|n| n.to_string()),
                    "created_at": unix(d.get("created_at")),
                })),
            })
        })
        .collect())
}

/// `.gitlab-ci.yml` at `git_ref` (the default branch: none), checked by GitLab's CI lint.
pub async fn lint(hub: &Hub, repo: &Repo, git_ref: Option<&str>) -> anyhow::Result<Value> {
    let (host, name) = host_for(hub, repo).await?;
    let at = git_ref.map(|r| format!("?content_ref={}", crate::ci::cli::enc(r))).unwrap_or_default();
    let (st, v) = host.call(Method::Get, &format!("/projects/{}/ci/lint{at}", id(&name)), None).await?;
    anyhow::ensure!(st == 200, "GitLab's CI lint ({st}): {}", said(&v));
    Ok(json!({
        "provider": GITLAB_CI,
        "path": ".gitlab-ci.yml",
        "valid": v.get("valid").and_then(Value::as_bool).unwrap_or(false),
        "errors": v.get("errors").cloned().unwrap_or_else(|| json!([])),
        "warnings": v.get("warnings").cloned().unwrap_or_else(|| json!([])),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r04_shapes() {
        let p = json!({ "id": 61, "iid": 7, "name": "nightly", "status": "failed", "source": "push", "ref": "main", "sha": "abc", "web_url": "https://gitlab.com/acme/app/-/pipelines/61",
                        "user": { "username": "ada" }, "created_at": "2026-10-18T12:00:00.000Z", "started_at": "2026-10-18T12:00:05.000Z", "finished_at": "2026-10-18T12:03:00.000Z" });
        let r = ext_run(&p);
        assert_eq!((r.external_id.as_str(), r.number, r.status.as_str(), r.conclusion.as_deref(), r.actor.as_str()), ("61", 7, "failed", Some("failed"), "ada"));
        assert_eq!(r.finished_at.zip(r.started_at).map(|(f, s)| f - s), Some(175));
        assert_eq!(ext_run(&json!({ "status": "manual" })).status, "queued");
        assert_eq!((ext_run(&json!({ "status": "running" })).status, ext_run(&json!({ "status": "running" })).conclusion), ("running".to_owned(), None));
        assert_eq!(event_of("merge_request_event"), "pull_request");
        let jobs = json!([{ "id": 8, "name": "test", "stage": "test", "status": "success", "tag_list": ["docker"], "runner": { "description": "r1" }, "web_url": "u",
                            "started_at": "2026-10-18T12:00:06Z", "finished_at": "2026-10-18T12:01:00Z" }, { "id": 9, "name": "deploy", "status": "manual" }]);
        let j = ext_jobs(&jobs);
        assert_eq!(j.iter().map(|j| (j.name.as_str(), j.status.as_str())).collect::<Vec<_>>(), [("test", "succeeded"), ("deploy", "queued")]);
        assert_eq!(j.first().map(|j| (j.runner.clone(), j.labels.clone())), Some((Some("r1".into()), vec!["docker".into()])));
    }
}
