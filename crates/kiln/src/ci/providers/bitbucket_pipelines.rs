//! Bitbucket Pipelines as a CI provider (docs/ci.md W23-R05), through the Bitbucket host's API
//! (`crate::githost::bitbucket`): pipelines and their steps kept in kiln's tables as they change
//! (commit status webhooks, and a look every 20 seconds); a step's log read when asked for; a
//! pipeline run again on its commit, stopped, or started as a custom pipeline with variables; the
//! repository's environments and their deployments.
//!
//! A Bitbucket step runs in its own container, so it is a leg; its log is the leg's log. Bitbucket's
//! API runs whole pipelines: it has no call that reruns only the failed steps, which its web page
//! does.

use std::sync::Arc;

use anyhow::{anyhow, bail};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::{BITBUCKET_PIPELINES, ExtJob, ExtRun, kiln_status, unix};
use crate::githost::bitbucket::{self, Bitbucket, said, seg};
use crate::http::client::Method;
use crate::hub::Hub;
use crate::store::Repo;

async fn host_for(hub: &Hub, repo: &Repo) -> anyhow::Result<(Arc<Bitbucket>, String)> {
    bitbucket::for_repo(hub, repo).await.ok_or_else(|| anyhow!("{} isn't on the Bitbucket kiln reaches", repo.name))
}

/// A pipeline's or step's state in GitHub's words (status, conclusion), and Bitbucket's own word for
/// how it ended.
fn words(state: &Value) -> (&'static str, Option<&'static str>, Option<String>) {
    let s = |p: &str| state.pointer(p).and_then(Value::as_str).unwrap_or_default();
    match s("/name") {
        "PENDING" => ("queued", None, None),
        // Paused at a manual step, or halted for one.
        "PAUSED" | "HALTED" => ("waiting", None, None),
        "IN_PROGRESS" if matches!(s("/stage/name"), "PAUSED" | "HALTED") => ("waiting", None, None),
        "IN_PROGRESS" => ("in_progress", None, None),
        "COMPLETED" => {
            let result = s("/result/name");
            let c = match result {
                "SUCCESSFUL" => "success",
                "STOPPED" | "EXPIRED" => "cancelled",
                "NOT_RUN" | "SKIPPED" => "skipped",
                _ => "failure",
            };
            ("completed", Some(c), Some(result.to_owned()))
        }
        _ => ("queued", None, None),
    }
}

/// A pipeline of Bitbucket's API, in kiln's words.
pub fn ext_run(p: &Value, web: &str, name: &str) -> ExtRun {
    let t = |q: &str| p.pointer(q).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (status, conclusion, own) = words(p.get("state").unwrap_or(&Value::Null));
    let pr = t("/target/type") == "pipeline_pullrequest_target";
    let number = p.get("build_number").and_then(Value::as_i64).unwrap_or(0);
    let selector = p.pointer("/target/selector/pattern").or_else(|| p.pointer("/target/selector/type")).and_then(Value::as_str).unwrap_or("default").to_owned();
    let event = if pr {
        "pull_request".to_owned()
    } else {
        match t("/trigger/name").as_str() {
            "PUSH" => "push".to_owned(),
            "MANUAL" => "workflow_dispatch".to_owned(),
            "SCHEDULE" => "schedule".to_owned(),
            other => other.to_ascii_lowercase(),
        }
    };
    let created = unix(p.get("created_on")).unwrap_or(0);
    ExtRun {
        external_id: t("/uuid"),
        number,
        name: selector.clone(),
        path: "bitbucket-pipelines.yml".into(),
        title: None,
        event,
        branch: if pr { t("/target/source") } else { t("/target/ref_name") },
        sha: t("/target/commit/hash"),
        actor: p.pointer("/creator/nickname").or_else(|| p.pointer("/creator/display_name")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        status: kiln_status(status, conclusion).to_owned(),
        conclusion: own,
        attempt: p.get("run_number").and_then(Value::as_i64).unwrap_or(1).max(1),
        url: format!("{web}/{name}/pipelines/results/{number}"),
        created_at: created,
        started_at: (status != "queued").then_some(created),
        finished_at: unix(p.get("completed_on")),
    }
}

/// A pipeline's steps, as legs.
pub fn ext_jobs(v: &Value, run_url: &str) -> Vec<ExtJob> {
    v.get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, st)| {
            let (status, conclusion, own) = words(st.get("state").unwrap_or(&Value::Null));
            let uuid = st.get("uuid").and_then(Value::as_str).unwrap_or_default().to_owned();
            ExtJob {
                name: st.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map_or_else(|| format!("Step {}", i + 1), str::to_owned),
                status: kiln_status(status, conclusion).to_owned(),
                conclusion: own,
                runner: None,
                labels: st.pointer("/image/name").and_then(Value::as_str).map(|i| vec![i.to_owned()]).unwrap_or_default(),
                url: format!("{run_url}/steps/{uuid}"),
                started_at: unix(st.get("started_on")),
                finished_at: unix(st.get("completed_on")),
                steps: Vec::new(),
                external_id: uuid,
            }
        })
        .collect()
}

/// One pipeline (its `{uuid}`) and its steps, kept: its kiln id.
pub async fn sync_run(hub: &Hub, repo: &Repo, pipeline: &str) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, p) = host.call(Method::Get, &format!("/repositories/{name}/pipelines/{}", seg(pipeline)), None).await?;
    if st != 200 {
        bail!("Bitbucket's pipeline {pipeline} ({st}): {}", said(&p));
    }
    let run = ext_run(&p, &host.web, &name);
    let (st, steps) = host.call(Method::Get, &format!("/repositories/{name}/pipelines/{}/steps?pagelen=100", seg(pipeline)), None).await?;
    let jobs = (st == 200).then(|| ext_jobs(&steps, &run.url));
    super::record(hub, repo, BITBUCKET_PIPELINES, &run, jobs.as_deref()).await
}

/// The repository's recent pipelines, kept: those kiln hasn't seen finish.
pub async fn sync(hub: &Hub, repo: &Repo) -> anyhow::Result<()> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, v) = host.call(Method::Get, &format!("/repositories/{name}/pipelines?sort=-created_on&pagelen=30"), None).await?;
    if st != 200 {
        bail!("Bitbucket's pipelines ({st}): {}", said(&v));
    }
    for p in v.get("values").and_then(Value::as_array).into_iter().flatten() {
        let r = ext_run(p, &host.web, &name);
        let (repo_id, ext, attempt) = (repo.id.clone(), r.external_id.clone(), r.attempt);
        let kept: Option<(String, i64)> = hub
            .store
            .call(move |c| Ok(c.query_row("SELECT status, attempt FROM runs WHERE repo_id = ?1 AND provider = ?2 AND external_id = ?3", params![repo_id, BITBUCKET_PIPELINES, ext], |x| Ok((x.get(0)?, x.get(1)?))).optional()?))
            .await?;
        let settled = kept.is_some_and(|(s, a)| a == attempt && s == r.status && matches!(s.as_str(), "succeeded" | "failed" | "cancelled" | "skipped" | "timed_out"));
        if !settled {
            sync_run(hub, repo, &r.external_id).await?;
        }
    }
    Ok(())
}

/// A webhook delivery Bitbucket signed: a build status changed (Pipelines reports through them), so
/// the repository's pipelines are looked at now.
pub async fn on_event(hub: &Hub, repo: &Repo, event: &str) -> anyhow::Result<()> {
    if !matches!(event, "repo:commit_status_created" | "repo:commit_status_updated") || !super::chosen(hub, repo).await.iter().any(|p| p == BITBUCKET_PIPELINES) {
        return Ok(());
    }
    sync(hub, repo).await?;
    let (repo_id, now) = (repo.id.clone(), crate::util::now_unix());
    let touched: Vec<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id FROM runs WHERE repo_id = ?1 AND provider = ?2 AND (finished_at IS NULL OR finished_at > ?3)")?;
            let rows = q.query_map(params![repo_id, BITBUCKET_PIPELINES, now - 120], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<String>, _>>()?)
        })
        .await?;
    for run in touched {
        hub.emit_run(&run).await;
    }
    Ok(())
}

/// The pipeline again: `all` runs a new one on the same commit and selector. Bitbucket's API has
/// no rerun of failed steps or of one step.
pub async fn rerun(hub: &Hub, repo: &Repo, run: &str, mode: &str) -> anyhow::Result<Value> {
    if mode != "all" {
        bail!("Bitbucket's API reruns whole pipelines only (rerun `all`); its web page reruns failed steps");
    }
    let (host, name) = host_for(hub, repo).await?;
    let (st, p) = host.call(Method::Get, &format!("/repositories/{name}/pipelines/{}", seg(run)), None).await?;
    anyhow::ensure!(st == 200, "Bitbucket's pipeline {run} ({st}): {}", said(&p));
    let target = p.get("target").cloned().unwrap_or(Value::Null);
    let mut again = json!({ "type": target.get("type").cloned().unwrap_or_else(|| json!("pipeline_commit_target")) });
    if let Some(o) = again.as_object_mut() {
        for k in ["ref_type", "ref_name", "source", "destination", "destination_commit"] {
            if let Some(v) = target.get(k).filter(|v| !v.is_null()) {
                o.insert(k.into(), v.clone());
            }
        }
        if let Some(h) = target.pointer("/commit/hash") {
            o.insert("commit".into(), json!({ "type": "commit", "hash": h }));
        }
        if let Some(s) = target.get("selector").filter(|s| !s.is_null()) {
            o.insert("selector".into(), json!({ "type": s.get("type"), "pattern": s.get("pattern") }));
        }
        if let Some(pr) = target.pointer("/pull_request/id") {
            o.insert("pull_request".into(), json!({ "id": pr }));
        }
    }
    let (st, v) = host.call(Method::Post, &format!("/repositories/{name}/pipelines"), Some(&json!({ "target": again }))).await?;
    anyhow::ensure!(st == 201 || st == 200, "Bitbucket started no pipeline ({st}): {}", said(&v));
    let new = v.get("uuid").and_then(Value::as_str).unwrap_or_default().to_owned();
    let kiln = sync_run(hub, repo, &new).await?;
    Ok(json!({ "provider": BITBUCKET_PIPELINES, "rerun": "all", "run": kiln, "pipeline": new }))
}

pub async fn cancel(hub: &Hub, repo: &Repo, run: &str) -> anyhow::Result<Value> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, v) = host.call(Method::Post, &format!("/repositories/{name}/pipelines/{}/stopPipeline", seg(run)), None).await?;
    anyhow::ensure!(st == 204 || st == 200, "Bitbucket didn't stop it ({st}): {}", said(&v));
    sync_run(hub, repo, run).await?;
    Ok(json!({ "provider": BITBUCKET_PIPELINES, "cancelled": true }))
}

/// A pipeline on `git_ref`: the custom pipeline `pipeline` (none, or `default`: the branch's own)
/// with `inputs` as its variables. Its kiln id.
pub async fn dispatch(hub: &Hub, repo: &Repo, pipeline: &str, git_ref: &str, inputs: &Value) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let (ref_type, ref_name) = match git_ref.strip_prefix("refs/tags/") {
        Some(t) => ("tag", t),
        None => ("branch", git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref)),
    };
    let mut target = json!({ "type": "pipeline_ref_target", "ref_type": ref_type, "ref_name": ref_name });
    let custom = pipeline.rsplit('/').next().unwrap_or(pipeline).trim_end_matches(".yml");
    if !custom.is_empty()
        && custom != "default"
        && custom != "bitbucket-pipelines"
        && let Some(o) = target.as_object_mut()
    {
        o.insert("selector".into(), json!({ "type": "custom", "pattern": custom }));
    }
    let variables: Vec<Value> = inputs.as_object().into_iter().flatten().map(|(k, v)| json!({ "key": k, "value": v.as_str().map_or_else(|| v.to_string(), str::to_owned), "secured": false })).collect();
    let (st, v) = host.call(Method::Post, &format!("/repositories/{name}/pipelines"), Some(&json!({ "target": target, "variables": variables }))).await?;
    anyhow::ensure!(st == 201 || st == 200, "Bitbucket started no pipeline ({st}): {}", said(&v));
    let new = v.get("uuid").and_then(Value::as_str).unwrap_or_default().to_owned();
    sync_run(hub, repo, &new).await
}

/// A step's log.
pub async fn logs(hub: &Hub, repo: &Repo, run: &str, step: &str) -> anyhow::Result<String> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, b) = host.bytes(&format!("/repositories/{name}/pipelines/{}/steps/{}/log", seg(run), seg(step)), 64 << 20).await?;
    anyhow::ensure!(st == 200 || st == 206, "Bitbucket has no log for it ({st})");
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The repository's environments, each with its latest deployment.
pub async fn environments(hub: &Hub, repo: &Repo) -> anyhow::Result<Vec<Value>> {
    let (host, name) = host_for(hub, repo).await?;
    let (st, envs) = host.call(Method::Get, &format!("/repositories/{name}/environments?pagelen=100"), None).await?;
    anyhow::ensure!(st == 200, "Bitbucket's environments ({st}): {}", said(&envs));
    let (st, deps) = host.call(Method::Get, &format!("/repositories/{name}/deployments?pagelen=100"), None).await?;
    let deps = if st == 200 { deps.get("values").and_then(Value::as_array).cloned().unwrap_or_default() } else { Vec::new() };
    Ok(envs
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            let uuid = e.get("uuid").and_then(Value::as_str).unwrap_or_default();
            let last = deps.iter().filter(|d| d.pointer("/environment/uuid").and_then(Value::as_str) == Some(uuid)).max_by_key(|d| unix(d.pointer("/state/started_on").or_else(|| d.get("last_update_time"))).unwrap_or(0));
            json!({
                "provider": BITBUCKET_PIPELINES,
                "name": e.get("name").and_then(Value::as_str).unwrap_or_default(),
                "kind": e.pointer("/environment_type/name").and_then(Value::as_str),
                "deployment": last.map(|d| json!({
                    "state": d.pointer("/state/name").and_then(Value::as_str),
                    "result": d.pointer("/state/status/name").and_then(Value::as_str),
                    "commit": d.pointer("/release/commit/hash").or_else(|| d.pointer("/deployable/commit/hash")).and_then(Value::as_str),
                    "pipeline": d.pointer("/release/pipeline/uuid").or_else(|| d.pointer("/deployable/pipeline/uuid")).and_then(Value::as_str),
                    "url": d.pointer("/release/url").or_else(|| d.pointer("/deployable/url")).and_then(Value::as_str),
                })),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w23_r05_shapes() {
        let p = json!({ "uuid": "{9f}", "build_number": 12, "run_number": 2, "created_on": "2026-10-18T12:00:00Z", "completed_on": "2026-10-18T12:03:00Z",
                        "state": { "name": "COMPLETED", "result": { "name": "FAILED" } }, "trigger": { "name": "PUSH" }, "creator": { "nickname": "ada" },
                        "target": { "type": "pipeline_ref_target", "ref_type": "branch", "ref_name": "main", "commit": { "hash": "abc" }, "selector": { "type": "branches", "pattern": "main" } } });
        let r = ext_run(&p, "https://bitbucket.org", "acme/app");
        assert_eq!((r.external_id.as_str(), r.number, r.attempt, r.status.as_str(), r.conclusion.as_deref()), ("{9f}", 12, 2, "failed", Some("FAILED")));
        assert_eq!((r.event.as_str(), r.branch.as_str(), r.name.as_str(), r.url.as_str()), ("push", "main", "main", "https://bitbucket.org/acme/app/pipelines/results/12"));
        let paused = json!({ "state": { "name": "IN_PROGRESS", "stage": { "name": "PAUSED" } } });
        assert_eq!(ext_run(&paused, "w", "n").status, "queued");
        assert_eq!(ext_run(&json!({ "state": { "name": "IN_PROGRESS", "stage": { "name": "RUNNING" } } }), "w", "n").status, "running");
        assert_eq!(ext_run(&json!({ "state": { "name": "COMPLETED", "result": { "name": "STOPPED" } } }), "w", "n").status, "cancelled");
        let steps = json!({ "values": [{ "uuid": "{s1}", "name": "Build", "state": { "name": "COMPLETED", "result": { "name": "SUCCESSFUL" } }, "image": { "name": "node:20" } }, { "uuid": "{s2}", "state": { "name": "PENDING" } }] });
        let j = ext_jobs(&steps, "u");
        assert_eq!(j.iter().map(|j| (j.name.as_str(), j.status.as_str(), j.external_id.as_str())).collect::<Vec<_>>(), [("Build", "succeeded", "{s1}"), ("Step 2", "queued", "{s2}")]);
    }
}
