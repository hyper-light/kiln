//! The CI engine's runs over the console's API (docs/ci.md §4.1, W21's data): runs, their graph,
//! annotations, summaries and artifacts, legs with their steps and routing, cancel and rerun,
//! and a repository's caches.
//!
//! - `GET /api/ci/runs?repo=&workflow=&branch=&event=&conclusion=&actor=&limit=&cursor=`
//! - `GET /api/ci/runs/:id`, `/graph` (with the critical path), `/annotations`, `/summary`,
//!   `/artifacts`
//! - `POST /api/ci/runs/:id/cancel`, `/rerun`
//! - `GET /api/ci/legs/:id`, `/logs?step=`, `/routing`
//! - `GET /api/ci/repos/:repo/caches`, `DELETE …/caches/:id`

use std::collections::{BTreeMap, HashMap};

use http::StatusCode;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn missing(m: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, m.into())
}

/// A run's row as the API shows it.
fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?, "number": r.get::<_, i64>(1)?, "repo": r.get::<_, String>(2)?, "workflow": r.get::<_, Option<String>>(3)?,
        "name": r.get::<_, String>(4)?, "run_name": r.get::<_, Option<String>>(5)?, "event": r.get::<_, String>(6)?, "branch": r.get::<_, String>(7)?,
        "sha": r.get::<_, String>(8)?, "actor": r.get::<_, String>(9)?, "status": r.get::<_, String>(10)?, "conclusion": r.get::<_, Option<String>>(11)?,
        "trust": r.get::<_, String>(12)?, "attempt": r.get::<_, i64>(13)?, "created_at": r.get::<_, i64>(14)?, "started_at": r.get::<_, Option<i64>>(15)?,
        "finished_at": r.get::<_, Option<i64>>(16)?, "error": r.get::<_, Option<String>>(17)?, "message": r.get::<_, String>(18)?, "github_run_id": r.get::<_, i64>(19)?,
    }))
}

const RUN_SELECT: &str = "SELECT r.id, r.number, p.name, r.workflow_id, r.pipeline, r.run_name, r.event, r.branch, r.sha, r.actor, r.status, r.conclusion, r.trust, r.attempt,
     r.created_at, r.started_at, r.finished_at, r.error, r.message, r.rowid FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.workflow_id IS NOT NULL";

/// `GET /api/ci/runs?…`: the engine's runs, newest first, cursor-paged.
pub async fn list(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(50).clamp(1, 200);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let f = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let (repo, workflow, branch, event, conclusion, actor) = (f("repo"), f("workflow"), f("branch"), f("event"), f("conclusion"), f("actor"));
    let rows = hub
        .store
        .call(move |c| {
            let sql = format!(
                "{RUN_SELECT} AND r.rowid < ?1 AND (?2 IS NULL OR p.name = ?2 OR p.id = ?2) AND (?3 IS NULL OR r.workflow_id = ?3 OR r.pipeline = ?3)
                 AND (?4 IS NULL OR r.branch = ?4) AND (?5 IS NULL OR r.event = ?5) AND (?6 IS NULL OR r.conclusion = ?6) AND (?7 IS NULL OR r.actor = ?7)
                 ORDER BY r.rowid DESC LIMIT ?8"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![cursor, repo, workflow, branch, event, conclusion, actor, limit + 1], run_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let page: Vec<Value> = rows.into_iter().take(usize::try_from(limit).unwrap_or(50)).collect();
    let next = if more { page.last().and_then(|r| r.get("github_run_id")).cloned() } else { None };
    Ok(srv::json(StatusCode::OK, &json!({ "runs": page, "next_cursor": next })))
}

async fn run_json(hub: &Hub, id: &str) -> Result<Value, Fail> {
    let id = id.to_owned();
    hub.store.call(move |c| Ok(c.query_row(&format!("{RUN_SELECT} AND r.id = ?1"), [&id], run_row).optional()?)).await.map_err(internal)?.ok_or_else(|| missing("no such run"))
}

/// The legs of a run, as the API shows them.
async fn legs(hub: &Hub, run: &str) -> Result<Vec<Value>, Fail> {
    let run = run.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, key, display, job_id, matrix, status, conclusion, agent, executor, attempt, wait_reason, queued_at, started_at, finished_at, error, needs, exit_code
                 FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY rowid",
            )?;
            let rows = st.query_map([&run], |r| {
                let json_col = |i: usize| -> rusqlite::Result<Value> { Ok(r.get::<_, Option<String>>(i)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)) };
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "key": r.get::<_, String>(1)?, "name": r.get::<_, Option<String>>(2)?, "job": r.get::<_, Option<String>>(3)?,
                    "matrix": json_col(4)?, "status": r.get::<_, String>(5)?, "conclusion": r.get::<_, Option<String>>(6)?, "runner": r.get::<_, Option<String>>(7)?,
                    "executor": r.get::<_, Option<String>>(8)?, "attempt": r.get::<_, i64>(9)?, "wait_reason": json_col(10)?, "queued_at": r.get::<_, Option<i64>>(11)?,
                    "started_at": r.get::<_, Option<i64>>(12)?, "finished_at": r.get::<_, Option<i64>>(13)?, "error": r.get::<_, Option<String>>(14)?, "needs": json_col(15)?,
                    "exit_code": r.get::<_, Option<i64>>(16)?,
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)
}

/// `GET /api/ci/runs/:id`: the run and its legs.
pub async fn get(hub: &Hub, id: &str) -> Result<Response, Fail> {
    let run = run_json(hub, id).await?;
    let legs = legs(hub, id).await?;
    Ok(srv::json(StatusCode::OK, &json!({ "run": run, "legs": legs })))
}

/// `GET /api/ci/runs/:id/graph`: the jobs as nodes (their legs inside), `needs` as edges, and the
/// critical path, the chain of jobs that took longest end to end.
pub async fn graph(hub: &Hub, id: &str) -> Result<Response, Fail> {
    run_json(hub, id).await?;
    let legs = legs(hub, id).await?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    // Jobs by id, in the order their first leg appeared.
    let mut order: Vec<String> = Vec::new();
    let mut by_job: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let mut job_of_key: HashMap<String, String> = HashMap::new();
    for l in &legs {
        let job = text(l, "job");
        if !by_job.contains_key(&job) {
            order.push(job.clone());
        }
        job_of_key.insert(text(l, "key"), job.clone());
        by_job.entry(job).or_default().push(l);
    }
    let mut edges: Vec<(String, String)> = Vec::new();
    for l in &legs {
        let to = text(l, "job");
        for k in l.get("needs").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(from) = k.as_str().and_then(|k| job_of_key.get(k))
                && !edges.contains(&(from.clone(), to.clone()))
            {
                edges.push((from.clone(), to.clone()));
            }
        }
    }
    // A job's span: its first leg's start to its last leg's end.
    let span = |job: &str| -> (i64, i64) {
        let ls = by_job.get(job).cloned().unwrap_or_default();
        let start = ls.iter().filter_map(|l| l.get("started_at").and_then(Value::as_i64)).min().unwrap_or(0);
        let end = ls.iter().filter_map(|l| l.get("finished_at").and_then(Value::as_i64)).max().unwrap_or(start);
        (start, end)
    };
    // The critical path: walk back from the job that finished last through the need that
    // finished last.
    let mut path: Vec<String> = Vec::new();
    let mut cur = order.iter().max_by_key(|j| span(j).1).cloned();
    while let Some(j) = cur {
        path.push(j.clone());
        cur = edges.iter().filter(|(_, to)| to == &j).map(|(from, _)| from.clone()).filter(|f| !path.contains(f)).max_by_key(|f| span(f).1);
    }
    path.reverse();
    let nodes: Vec<Value> = order
        .iter()
        .map(|j| {
            let ls = by_job.get(j).cloned().unwrap_or_default();
            let (start, end) = span(j);
            let leg_ids: Vec<Value> = ls.iter().map(|l| json!({ "id": l.get("id"), "key": l.get("key"), "name": l.get("name"), "status": l.get("status"), "conclusion": l.get("conclusion") })).collect();
            json!({ "job": j, "legs": leg_ids, "started_at": (start > 0).then_some(start), "finished_at": (end > 0).then_some(end) })
        })
        .collect();
    let edges: Vec<Value> = edges.iter().map(|(a, b)| json!({ "from": a, "to": b })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "nodes": nodes, "edges": edges, "critical_path": path })))
}

/// `GET /api/ci/runs/:id/annotations`, `/summary`, `/artifacts`.
pub async fn part(hub: &Hub, id: &str, which: &str) -> Result<Response, Fail> {
    run_json(hub, id).await?;
    let (run, which) = (id.to_owned(), which.to_owned());
    let v = hub
        .store
        .call(move |c| {
            Ok(match which.as_str() {
                "annotations" => {
                    let mut st = c.prepare(
                        "SELECT a.leg_id, j.key, a.step_idx, a.level, a.title, a.message, a.file, a.line, a.end_line, a.col, a.end_col FROM ci_annotations a
                         JOIN jobs j ON j.id = a.leg_id WHERE j.run_id = ?1 ORDER BY a.id",
                    )?;
                    let rows = st.query_map([&run], |r| {
                        Ok(json!({ "leg": r.get::<_, String>(0)?, "leg_key": r.get::<_, String>(1)?, "step": r.get::<_, Option<i64>>(2)?, "level": r.get::<_, String>(3)?,
                                   "title": r.get::<_, Option<String>>(4)?, "message": r.get::<_, String>(5)?, "file": r.get::<_, Option<String>>(6)?, "line": r.get::<_, Option<i64>>(7)?,
                                   "end_line": r.get::<_, Option<i64>>(8)?, "col": r.get::<_, Option<i64>>(9)?, "end_col": r.get::<_, Option<i64>>(10)? }))
                    })?;
                    json!({ "annotations": rows.collect::<Result<Vec<_>, _>>()? })
                }
                "summary" => {
                    let mut st = c.prepare("SELECT s.leg_id, j.key, s.step_idx, s.markdown FROM ci_summaries s JOIN jobs j ON j.id = s.leg_id WHERE j.run_id = ?1 ORDER BY j.rowid, s.step_idx")?;
                    let rows = st.query_map([&run], |r| Ok(json!({ "leg": r.get::<_, String>(0)?, "leg_key": r.get::<_, String>(1)?, "step": r.get::<_, i64>(2)?, "markdown": r.get::<_, String>(3)? })))?;
                    json!({ "summaries": rows.collect::<Result<Vec<_>, _>>()? })
                }
                _ => {
                    let mut st = c.prepare("SELECT id, name, size, digest, created_at, expires_at, leg_id, object IS NULL FROM ci_artifacts WHERE run_id = ?1 AND finalized = 1 ORDER BY id")?;
                    let now = crate::util::now_unix();
                    let rows = st.query_map([&run], |r| {
                        let expires: Option<i64> = r.get(5)?;
                        let gone: bool = r.get(7)?;
                        Ok(json!({ "id": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "size": r.get::<_, Option<i64>>(2)?, "digest": r.get::<_, Option<String>>(3)?,
                                   "created_at": r.get::<_, i64>(4)?, "expires_at": expires, "leg": r.get::<_, Option<String>>(6)?, "expired": gone || expires.is_some_and(|e| e <= now) }))
                    })?;
                    json!({ "artifacts": rows.collect::<Result<Vec<_>, _>>()? })
                }
            })
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &v))
}

/// `POST /api/ci/runs/:id/cancel`.
pub async fn cancel(hub: &Hub, id: &str) -> Result<Response, Fail> {
    run_json(hub, id).await?;
    hub.cancel_run(id, "cancelled from the API").await.map_err(internal)?;
    Ok(srv::status(StatusCode::ACCEPTED))
}

/// `POST /api/ci/runs/:id/rerun`: the run again, from its saved event and the workflow as it
/// was (a new run; `{ mode: "all" }`, the default).
pub async fn rerun(hub: &Hub, id: &str, body: &Value) -> Result<Response, Fail> {
    use super::dispatch::RerunMode;
    let mode = match body.get("mode").and_then(Value::as_str).unwrap_or("all") {
        "all" => RerunMode::All,
        "failed" => RerunMode::Failed,
        "job" => RerunMode::Job(body.get("job").and_then(Value::as_str).filter(|j| !j.is_empty()).ok_or_else(|| (StatusCode::BAD_REQUEST, "mode `job` names a `job`".to_owned()))?.to_owned()),
        other => return Err((StatusCode::BAD_REQUEST, format!("rerun mode `{other}`: `all`, `failed` or `job`"))),
    };
    let debug = body.get("debug").and_then(Value::as_bool).unwrap_or(false);
    // "Rerun with fresh actions" (W07-R02): pinned again now, instead of the run's pins.
    let actions = if body.get("fresh_actions").and_then(Value::as_bool).unwrap_or(false) {
        let run = id.to_owned();
        let saved: Option<(String, String, String)> = hub.store.call(move |c| Ok(c.query_row("SELECT r.repo_id, r.sha, p.workflow FROM runs r JOIN ci_plans p ON p.run_id = r.id WHERE r.id = ?1", [&run], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)).await.map_err(internal)?;
        let Some((repo_id, sha, model)) = saved else { return Err(missing("no such run of the CI engine")) };
        let repo = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == repo_id).ok_or_else(|| missing("the run's repository is gone"))?;
        let wf: kiln_ci::wf::Workflow = serde_json::from_str(&model).map_err(|e| (StatusCode::CONFLICT, format!("the run's workflow can't be read again: {e}")))?;
        Some(super::actions::resolve(hub, &repo, &sha, &wf).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?)
    } else {
        None
    };
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Rerun { run: id.to_owned(), mode, debug, actions, reply: tx });
    match crate::rt::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
        Ok(Ok(Ok(attempt))) => Ok(srv::json(StatusCode::CREATED, &json!({ "run": id, "attempt": attempt }))),
        Ok(Ok(Err(e))) => Err((StatusCode::CONFLICT, e)),
        _ => Err((StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned())),
    }
}

/// `GET /api/ci/legs/:id`: the leg, its steps and annotations.
pub async fn leg(hub: &Hub, id: &str) -> Result<Response, Fail> {
    let leg_id = id.to_owned();
    let found: Option<(String, Vec<Value>, Vec<Value>)> = hub
        .store
        .call(move |c| {
            let Some(run) = c.query_row("SELECT run_id FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [&leg_id], |r| r.get::<_, String>(0)).optional()? else { return Ok(None) };
            let mut st = c.prepare("SELECT idx, name, status, kind, step_id, action, outcome, conclusion, outputs, started_at, finished_at FROM steps WHERE job_id = ?1 ORDER BY idx")?;
            let steps = st
                .query_map([&leg_id], |r| {
                    Ok(json!({ "idx": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                               "id": r.get::<_, Option<String>>(4)?, "action": r.get::<_, Option<String>>(5)?, "outcome": r.get::<_, Option<String>>(6)?,
                               "conclusion": r.get::<_, Option<String>>(7)?, "outputs": r.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                               "started_at": r.get::<_, Option<i64>>(9)?, "finished_at": r.get::<_, Option<i64>>(10)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT step_idx, level, title, message, file, line FROM ci_annotations WHERE leg_id = ?1 ORDER BY id")?;
            let notes = st
                .query_map([&leg_id], |r| Ok(json!({ "step": r.get::<_, Option<i64>>(0)?, "level": r.get::<_, String>(1)?, "title": r.get::<_, Option<String>>(2)?, "message": r.get::<_, String>(3)?, "file": r.get::<_, Option<String>>(4)?, "line": r.get::<_, Option<i64>>(5)? })))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some((run, steps, notes)))
        })
        .await
        .map_err(internal)?;
    let Some((run, steps, annotations)) = found else { return Err(missing("no such leg of the CI engine")) };
    let all = legs(hub, &run).await?;
    let me = all.into_iter().find(|l| l.get("id").and_then(Value::as_str) == Some(id)).unwrap_or(Value::Null);
    Ok(srv::json(StatusCode::OK, &json!({ "run": run, "leg": me, "steps": steps, "annotations": annotations })))
}

/// `GET /api/ci/legs/:id/logs?step=`.
pub async fn logs(hub: &Hub, id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let step = q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut data = hub.logs.read(id, step).await;
    // A leg a rerun kept: the logs are those of the leg whose result it keeps (W05-R17).
    if data.is_empty() {
        let leg = id.to_owned();
        let from: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT reused_from FROM jobs WHERE id = ?1", [&leg], |r| r.get(0)).optional()?.flatten())).await.map_err(internal)?;
        if let Some(from) = from {
            data = hub.logs.read(&from, step).await;
        }
    }
    Ok(srv::response(StatusCode::OK, "text/plain; charset=utf-8", srv::full(data)))
}

/// `GET /api/ci/legs/:id/routing`: why it waits, and every runner's checks (W05-R11).
pub async fn routing(hub: &Hub, id: &str) -> Result<Response, Fail> {
    if !hub.ci.enabled() {
        return Err(missing("the CI engine is off"));
    }
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Explain { leg: id.to_owned(), reply: tx });
    let v = crate::rt::timeout(std::time::Duration::from_secs(5), rx.recv()).await.map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned()))?.map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned()))?;
    Ok(srv::json(StatusCode::OK, &v))
}

/// `GET /api/ci/repos/:repo/caches`, and `DELETE …/caches/:id`.
pub async fn caches(hub: &Hub, repo: &str, delete: Option<&str>) -> Result<Response, Fail> {
    let r = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == repo || r.name == repo).ok_or_else(|| missing(format!("no repository `{repo}`")))?;
    let repo_id = r.id;
    if let Some(id) = delete {
        let Ok(id) = id.parse::<i64>() else { return Err(missing("no such cache")) };
        let n = hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_caches WHERE id = ?1 AND repo_id = ?2", params![id, repo_id])?)).await.map_err(internal)?;
        return if n > 0 { Ok(srv::status(StatusCode::NO_CONTENT)) } else { Err(missing("no such cache")) };
    }
    let rows = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, scope, key, version, size, created_at, last_used_at, object IS NOT NULL FROM ci_caches WHERE repo_id = ?1 ORDER BY COALESCE(last_used_at, created_at) DESC")?;
            let rows = st.query_map([&repo_id], |r| {
                Ok(json!({ "id": r.get::<_, i64>(0)?, "ref": r.get::<_, String>(1)?, "key": r.get::<_, String>(2)?, "version": r.get::<_, String>(3)?, "size_in_bytes": r.get::<_, Option<i64>>(4)?,
                           "created_at": r.get::<_, i64>(5)?, "last_accessed_at": r.get::<_, Option<i64>>(6)?, "ready": r.get::<_, bool>(7)? }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let total: i64 = rows.iter().filter_map(|c| c.get("size_in_bytes").and_then(Value::as_i64)).sum();
    Ok(srv::json(StatusCode::OK, &json!({ "total_count": rows.len(), "total_size_in_bytes": total, "caches": rows })))
}
