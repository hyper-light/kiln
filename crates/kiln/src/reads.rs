//! The API's reads of kiln's records, answered from a store: the server's kiln.db, or a runner's
//! replica of it, where the runner's front door answers the owner's (docs/p2p.md, P2). The server's
//! routes call these too, so an answer is the same wherever it's read.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::store::{Repo, Run, Store};

/// Whether a replica answers `GET` of this route (its path's segments).
pub fn answers(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["api", "runs"] | ["api", "runs", _] | ["api", "jobs", _] | ["api", "repos"] | ["api", "ci", "runs"] | ["api", "ci", "runs", _] | ["api", "ci", "runs", _, "graph"]
    )
}

/// Runs, newest first or by `sort` (oldest, longest, shortest), of the repositories `among` (all:
/// `None`), each with the runners and executors its legs ran on.
pub async fn runs(store: &Store, q: &HashMap<String, String>, among: Option<Vec<String>>) -> anyhow::Result<Vec<Value>> {
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
    let before = q.get("before").and_then(|v| v.parse().ok());
    let list = match q.get("sort").filter(|s| matches!(s.as_str(), "oldest" | "longest" | "shortest")) {
        // Another order than newest first pages by `offset` (W21-R03's sort).
        Some(sort) => store.runs_sorted(among, q.get("repo").cloned(), sort, limit, q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0)).await?,
        None => match among {
            None => store.runs(q.get("repo").cloned(), limit, before).await?,
            Some(names) => store.runs_among(names, q.get("repo").cloned(), limit, before).await?,
        },
    };
    with_hosts(store, &list).await
}

/// Runs, each with the runners and executors its legs ran on (W21-R03's facets).
pub async fn with_hosts(store: &Store, runs: &[Run]) -> anyhow::Result<Vec<Value>> {
    let hosts = store.run_hosts(runs.iter().map(|r| r.id.clone()).collect()).await?;
    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        let mut v = serde_json::to_value(r)?;
        if let (Some(o), Some((runners, executors))) = (v.as_object_mut(), hosts.get(&r.id)) {
            o.insert("runners".into(), json!(runners));
            o.insert("executors".into(), json!(executors));
        }
        out.push(v);
    }
    Ok(out)
}

/// A run and its jobs: a CI run's `attempt`, the current one by default (W05-R17). `None`: no such
/// run.
pub async fn run(store: &Store, id: &str, q: &HashMap<String, String>) -> anyhow::Result<Option<Value>> {
    let Some(run) = store.run(id.to_owned()).await? else { return Ok(None) };
    let attempt = q.get("attempt").and_then(|a| a.parse().ok());
    let jobs = store.jobs_at(id.to_owned(), false, attempt).await?;
    Ok(Some(json!({ "run": run, "jobs": jobs })))
}

/// A job. `None`: no such job.
pub async fn job(store: &Store, id: &str) -> anyhow::Result<Option<Value>> {
    Ok(store.job(id.to_owned()).await?.map(serde_json::to_value).transpose()?)
}

/// The repositories `keep` lets through, each saying whether it's public.
pub async fn repos(store: &Store, keep: impl Fn(&Repo) -> bool) -> anyhow::Result<Vec<Value>> {
    let mut repos = store.repos().await?;
    repos.retain(|r| keep(r));
    let public = store.public_repos().await.unwrap_or_default();
    Ok(repos
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("public".into(), json!(public.contains(&r.id)));
            }
            v
        })
        .collect())
}

/// A replica's answer to the owner's `GET` of `segments` with query `q`, or `None` for anything it
/// doesn't answer as the server would (another route, nothing there, a read that failed): the
/// server answers that.
pub async fn answer(store: &Store, segments: &[&str], q: &HashMap<String, String>) -> Option<Value> {
    let read = match segments {
        ["api", "runs"] => runs(store, q, None).await.map(|v| Some(Value::Array(v))),
        ["api", "runs", id] => run(store, id, q).await,
        ["api", "jobs", id] => job(store, id).await,
        ["api", "repos"] => repos(store, |_| true).await.map(|v| Some(Value::Array(v))),
        // The CI pages' own (the engine's runs, a run with its legs, its graph).
        ["api", "ci", "runs"] => return crate::ci::runs_api::list_in(store, q, None).await.ok(),
        ["api", "ci", "runs", id] => return crate::ci::runs_api::get_in(store, id, q).await.ok(),
        ["api", "ci", "runs", id, "graph"] => return crate::ci::runs_api::graph_in(store, id, q).await.ok(),
        _ => return None,
    };
    match read {
        Ok(v) => v,
        Err(e) => {
            // A replica whose schema is another build's, say: the server answers.
            tracing::debug!(error = %format!("{e:#}"), "a read the replica couldn't answer");
            None
        }
    }
}
