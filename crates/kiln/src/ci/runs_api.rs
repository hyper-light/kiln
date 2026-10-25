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
        // W18-R06: `required` while an outside contributor's run waits for a maintainer.
        "approval": r.get::<_, Option<String>>(20)?, "approved_by": r.get::<_, Option<String>>(21)?,
        // W23: where it ran (`kiln`, `github-actions`, …), and its page and number there.
        "provider": r.get::<_, String>(22)?, "external_url": r.get::<_, Option<String>>(23)?, "external_number": r.get::<_, Option<i64>>(24)?,
    }))
}

const RUN_SELECT: &str = "SELECT r.id, r.number, p.name, r.workflow_id, r.pipeline, r.run_name, r.event, r.branch, r.sha, r.actor, r.status, r.conclusion, r.trust, r.attempt,
     r.created_at, r.started_at, r.finished_at, r.error, r.message, r.rowid, r.approval, r.approved_by, r.provider, r.external_url, r.external_number FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.workflow_id IS NOT NULL";

/// `GET /api/ci/runs?…`: the engine's runs, newest first, cursor-paged; of the repositories the
/// caller may read (W10).
pub async fn list(hub: &Hub, q: &HashMap<String, String>, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    let only: Option<String> = match caller {
        Some(c) => crate::authz::gate::readable(hub, c, "runs.read").await.map(|names| serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())),
        None => None,
    };
    Ok(srv::json(StatusCode::OK, &list_in(&hub.store, q, only).await?))
}

/// The engine's runs, newest first, a page at a time: of the repositories named in `only` (JSON,
/// those the caller may read), or all. The server's `GET /api/ci/runs`, and a runner's from its
/// replica (`crate::reads`).
pub async fn list_in(store: &crate::store::Store, q: &HashMap<String, String>, only: Option<String>) -> Result<Value, Fail> {
    let limit = q.get("limit").and_then(|l| l.parse::<i64>().ok()).unwrap_or(50).clamp(1, 200);
    let cursor = q.get("cursor").and_then(|c| c.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let f = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
    let (repo, workflow, branch, event, conclusion, actor) = (f("repo"), f("workflow"), f("branch"), f("event"), f("conclusion"), f("actor"));
    let rows = store
        .call(move |c| {
            let sql = format!(
                "{RUN_SELECT} AND r.rowid < ?1 AND (?2 IS NULL OR p.name = ?2 OR p.id = ?2) AND (?3 IS NULL OR r.workflow_id = ?3 OR r.pipeline = ?3)
                 AND (?4 IS NULL OR r.branch = ?4) AND (?5 IS NULL OR r.event = ?5) AND (?6 IS NULL OR r.conclusion = ?6) AND (?7 IS NULL OR r.actor = ?7)
                 AND (?9 IS NULL OR p.name IN (SELECT value FROM json_each(?9)))
                 ORDER BY r.rowid DESC LIMIT ?8"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![cursor, repo, workflow, branch, event, conclusion, actor, limit + 1, only], run_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let page: Vec<Value> = rows.into_iter().take(usize::try_from(limit).unwrap_or(50)).collect();
    let next = if more { page.last().and_then(|r| r.get("github_run_id")).cloned() } else { None };
    Ok(json!({ "runs": page, "next_cursor": next }))
}

async fn run_json(store: &crate::store::Store, id: &str) -> Result<Value, Fail> {
    let id = id.to_owned();
    store.call(move |c| Ok(c.query_row(&format!("{RUN_SELECT} AND r.id = ?1"), [&id], run_row).optional()?)).await.map_err(internal)?.ok_or_else(|| missing("no such run"))
}

/// The legs of a run, as the API shows them.
/// An attempt the query names (`?attempt=`), else the current one.
fn attempt_of(q: &HashMap<String, String>) -> Option<i64> {
    q.get("attempt").and_then(|a| a.parse().ok())
}

async fn legs(store: &crate::store::Store, run: &str, attempt: Option<i64>) -> Result<Vec<Value>, Fail> {
    let run = run.to_owned();
    store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, key, display, job_id, matrix, status, conclusion, agent, executor, attempt, wait_reason, queued_at, started_at, finished_at, error, needs, exit_code, reused_from,
                        CASE WHEN kept_until > strftime('%s', 'now') THEN kept_until END, waits,
                        (SELECT s.run_id FROM jobs s WHERE s.id = jobs.reused_from), (SELECT r.number FROM jobs s JOIN runs r ON r.id = s.run_id WHERE s.id = jobs.reused_from)
                 FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = COALESCE(?2, (SELECT attempt FROM runs WHERE id = ?1)) ORDER BY rowid",
            )?;
            let rows = st.query_map(params![run, attempt], |r| {
                let json_col = |i: usize| -> rusqlite::Result<Value> { Ok(r.get::<_, Option<String>>(i)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)) };
                let work = reused_report(r.get(17)?, r.get(20)?, r.get(21)?, r.get(7)?, r.get(12)?, r.get(13)?);
                let mut leg = json!({
                    "id": r.get::<_, String>(0)?, "key": r.get::<_, String>(1)?, "name": r.get::<_, Option<String>>(2)?, "job": r.get::<_, Option<String>>(3)?,
                    "matrix": json_col(4)?, "status": r.get::<_, String>(5)?, "conclusion": r.get::<_, Option<String>>(6)?, "runner": r.get::<_, Option<String>>(7)?,
                    "executor": r.get::<_, Option<String>>(8)?, "attempt": r.get::<_, i64>(9)?, "wait_reason": json_col(10)?, "queued_at": r.get::<_, Option<i64>>(11)?,
                    "started_at": r.get::<_, Option<i64>>(12)?, "finished_at": r.get::<_, Option<i64>>(13)?, "error": r.get::<_, Option<String>>(14)?, "needs": json_col(15)?,
                    "exit_code": r.get::<_, Option<i64>>(16)?, "reused_from": r.get::<_, Option<String>>(17)?,
                    // A failed leg kept to look into, until when (W17-R02).
                    "kept_until": r.get::<_, Option<i64>>(18)?,
                    // Seconds queued, by wait reason (W19-R01).
                    "waits": json_col(19)?,
                    // Whose result it took over: an earlier attempt's of this run, or another
                    // run's with the same inputs (W08-R10).
                    "reused_run": r.get::<_, Option<String>>(20)?, "reused_number": r.get::<_, Option<i64>>(21)?,
                });
                // What it did, as W24-R28 reports it: a reused leg says whose result, from where.
                if let (Some(work), Some(o)) = (work, leg.as_object_mut()) {
                    o.insert("work".into(), work);
                }
                Ok(leg)
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)
}

/// A reused leg's report (W24-R28), from its row: the leg and run it took its result from, the
/// runner that ran that (a copy keeps its source's runner and times), and the time it saved, what
/// its source ran; with the report in a sentence. `None` for a leg that wasn't reused.
fn reused_report(from: Option<String>, run: Option<String>, number: Option<i64>, runner: Option<String>, started: Option<i64>, finished: Option<i64>) -> Option<Value> {
    use kiln_ci::work::why::{How, Report};
    let leg = from?;
    let saved_s = started.zip(finished).map(|(s, f)| f.saturating_sub(s)).filter(|s| *s > 0).map(|s| s as f64);
    let report = Report { how: How::Reused { run: run.unwrap_or_default(), number: number.and_then(|n| u64::try_from(n).ok()), leg: Some(leg), runner }, hits: None, saved_s, why: Vec::new() };
    let mut v = serde_json::to_value(&report).ok()?;
    if let Some(o) = v.as_object_mut() {
        o.insert("summary".into(), Value::String(report.summary()));
    }
    Some(v)
}

/// `GET /api/ci/runs/:id`: the run and its legs.
pub async fn get(hub: &Hub, id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    Ok(srv::json(StatusCode::OK, &get_in(&hub.store, id, q).await?))
}

/// A run and its legs, from a store: the server's `GET /api/ci/runs/:id`, and a runner's from its
/// replica (`crate::reads`).
pub async fn get_in(store: &crate::store::Store, id: &str, q: &HashMap<String, String>) -> Result<Value, Fail> {
    let run = run_json(store, id).await?;
    let legs = legs(store, id, attempt_of(q)).await?;
    // The actions the run pinned when it started (W07-R02).
    let rid = id.to_owned();
    let saved: Option<(String, Option<String>)> = store.call(move |c| Ok(c.query_row("SELECT input, removed FROM ci_plans WHERE run_id = ?1", [&rid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.map_err(internal)?;
    let (input, removed) = saved.map_or((None, None), |(i, r)| (Some(i), r));
    // The combinations `exclude` and `where` removed, by job, with why (W04-R29).
    let removed: Value = removed.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_else(|| json!({}));
    let actions: Vec<Value> = input
        .and_then(|i| serde_json::from_str::<Value>(&i).ok())
        .and_then(|v| v.get("actions").and_then(Value::as_object).cloned())
        .map(|m| m.into_iter().map(|(uses, a)| json!({ "uses": uses, "repo": a.get("repo"), "sha": a.get("sha") })).collect())
        .unwrap_or_default();
    Ok(json!({ "run": run, "legs": legs, "actions": actions, "removed": removed }))
}

/// `GET /api/ci/runs/:id/graph`: the jobs as nodes (their legs inside), `needs` as edges, and the
/// critical path, the chain of jobs that took longest end to end; and the workflow's jobs as the
/// run's plan has them (`jobs`, `plan_jobs`).
pub async fn graph(hub: &Hub, id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    Ok(srv::json(StatusCode::OK, &graph_in(&hub.store, id, q).await?))
}

/// A run's graph, from a store: the server's `GET /api/ci/runs/:id/graph`, and a runner's from its
/// replica (`crate::reads`).
pub async fn graph_in(store: &crate::store::Store, id: &str, q: &HashMap<String, String>) -> Result<Value, Fail> {
    run_json(store, id).await?;
    let mut g = graph_json(store, id, attempt_of(q)).await?;
    let jobs = plan_jobs(store, id).await?;
    if let Some(o) = g.as_object_mut() {
        o.insert("jobs".into(), Value::Array(jobs));
    }
    Ok(g)
}

/// `GET /api/ci/runs/:id/flow` (W04-R23), in the attempt asked for: each job output, from the
/// step whose output it reads to the jobs that read it (where in each, and their legs), with the
/// value each leg gave it (a secret's never, a long one cut); and each job's exported environment
/// (the names its legs set) and the jobs that take it (`env-from`). Nothing for a run without a
/// plan.
pub async fn flow(hub: &Hub, id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    run_json(&hub.store, id).await?;
    let run = id.to_owned();
    let model: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT workflow FROM ci_plans WHERE run_id = ?1", [&run], |r| r.get(0)).optional()?)).await.map_err(internal)?;
    let Some(wf) = model.and_then(|m| serde_json::from_str::<kiln_ci::wf::Workflow>(&m).ok()) else {
        return Ok(srv::json(StatusCode::OK, &json!({ "outputs": [], "exports": [] })));
    };
    let f = kiln_ci::wf::flow::flow(&wf);
    let attempt = attempt_of(q);
    let legs = legs(&hub.store, id, attempt).await?;
    // What each leg reported, by its key.
    let run = id.to_owned();
    let reports: HashMap<String, Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT leg, report FROM ci_plan_log WHERE run_id = ?1 AND kind = 'leg' AND attempt = COALESCE(?2, (SELECT attempt FROM runs WHERE id = ?1)) ORDER BY seq")?;
            let rows = st.query_map(params![run, attempt], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default())))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?.into_iter().map(|(k, r)| (k, serde_json::from_str(&r).unwrap_or(Value::Null))).collect())
        })
        .await
        .map_err(internal)?;
    let legs_of = |job: &str| legs.iter().filter(|l| l.get("job").and_then(Value::as_str).is_some_and(|j| j.eq_ignore_ascii_case(job))).collect::<Vec<_>>();
    let brief = |l: &&Value| json!({ "leg": l.get("id"), "key": l.get("key"), "status": l.get("status") });
    // A report's `[name, value]` pair of `name`, in its list `k`.
    let pair = |r: Option<&Value>, k: &str, name: &str| -> Option<Value> {
        r?.get(k)?.as_array()?.iter().find(|p| p.get(0).and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)))?.get(1).cloned()
    };
    let clip = |v: String| if v.chars().count() > 200 { format!("{}…", v.chars().take(200).collect::<String>()) } else { v };
    let outputs: Vec<Value> = f
        .outputs
        .iter()
        .map(|o| {
            let values: Vec<Value> = legs_of(&o.job)
                .into_iter()
                .map(|l| {
                    let key = l.get("key").and_then(Value::as_str).unwrap_or_default();
                    let r = reports.get(key);
                    let secret = o.secret || r.and_then(|r| r.get("secret_outputs")).and_then(Value::as_array).is_some_and(|a| a.iter().any(|n| n.as_str().is_some_and(|n| n.eq_ignore_ascii_case(&o.name))));
                    let given = pair(r, "outputs", &o.name).and_then(|v| v.as_str().map(str::to_owned)).or_else(|| pair(r, "json_outputs", &o.name).map(|v| v.to_string()));
                    json!({ "leg": l.get("id"), "key": key, "secret": secret, "value": if secret { None } else { given.map(clip) } })
                })
                .collect();
            let readers: Vec<Value> = o.readers.iter().map(|r| json!({ "job": r.job, "at": r.at, "legs": legs_of(&r.job).iter().map(brief).collect::<Vec<_>>() })).collect();
            json!({ "job": o.job, "name": o.name, "step": o.step.as_ref().map(|(i, id)| json!({ "index": i, "id": id })), "secret": o.secret, "values": values, "readers": readers })
        })
        .collect();
    let exports: Vec<Value> = f
        .exports
        .iter()
        .map(|e| {
            let set: Vec<Value> = legs_of(&e.job)
                .into_iter()
                .map(|l| {
                    let key = l.get("key").and_then(Value::as_str).unwrap_or_default();
                    let names: Vec<String> = reports
                        .get(key)
                        .and_then(|r| r.get("env"))
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(|p| p.get(0).and_then(Value::as_str).map(str::to_owned)).collect())
                        .unwrap_or_default();
                    json!({ "leg": l.get("id"), "key": key, "names": names })
                })
                .collect();
            let readers: Vec<Value> = e.readers.iter().map(|r| json!({ "job": r, "legs": legs_of(r).iter().map(brief).collect::<Vec<_>>() })).collect();
            json!({ "job": e.job, "names": e.names, "legs": set, "readers": readers })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "outputs": outputs, "exports": exports })))
}

/// The workflow's jobs as the run's plan has them (W21-R04), each whether its legs are made yet
/// or not (a job's are made once what it needs is done): its name, what it needs, whether its
/// matrix comes from what it needs (`matrix_waits`: then how many legs it has is known only once
/// they're done), how many steps it has, whether it calls a workflow (its legs are then the
/// called workflow's jobs, `<job>/<theirs>`), and how long it typically takes (`typical`,
/// seconds; see `typical_spans`). None for a run without a plan (a v1 pipeline's, another
/// provider's).
async fn plan_jobs(store: &crate::store::Store, id: &str) -> Result<Vec<Value>, Fail> {
    use kiln_ci::wf::ir::{Axis, Matrix};
    let run = id.to_owned();
    let saved: Option<(String, Option<String>, String, i64)> = store
        .call(move |c| {
            Ok(c.query_row("SELECT p.workflow, r.workflow_id, r.repo_id, r.number FROM ci_plans p JOIN runs r ON r.id = p.run_id WHERE p.run_id = ?1", [&run], |x| {
                Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?))
            })
            .optional()?)
        })
        .await
        .map_err(internal)?;
    let Some((model, workflow, repo, number)) = saved else { return Ok(Vec::new()) };
    let Ok(wf) = serde_json::from_str::<kiln_ci::wf::Workflow>(&model) else { return Ok(Vec::new()) };
    let typical = match workflow {
        Some(w) => typical_spans(store, &repo, &w, number).await?,
        None => HashMap::new(),
    };
    // An expression that reads what the job needs (`fromJSON(needs.plan.outputs.matrix)`).
    let reads_needs = |s: &str| s.contains("${{") && s.contains("needs.");
    Ok(wf
        .jobs
        .iter()
        .map(|j| {
            let waits = match j.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
                None => false,
                Some(Matrix::Expr(t)) => reads_needs(&t.src),
                Some(Matrix::Spec(m)) => {
                    m.axes.iter().any(|(_, a)| matches!(a, Axis::Expr(t) if reads_needs(&t.src)))
                        || [&m.include, &m.exclude].into_iter().flatten().any(|v| serde_json::to_string(v).is_ok_and(|s| reads_needs(&s)))
                }
            };
            json!({
                "job": j.id,
                // Its name as written, unless an expression makes it (its legs then say it).
                "name": j.name.as_ref().filter(|t| !t.has_expr()).map(|t| t.src.clone()),
                "needs": j.needs.iter().map(|n| n.job()).collect::<Vec<_>>(),
                "matrix_waits": waits,
                "steps": j.steps.len(),
                "calls": j.uses.is_some(),
                "typical": typical.get(&j.id),
            })
        })
        .collect())
}

/// How long each of a workflow's jobs typically takes, in seconds: the median of its spans (its
/// first leg's start to its last leg's end) over the workflow's last 20 runs before run `number`
/// that succeeded.
async fn typical_spans(store: &crate::store::Store, repo: &str, workflow: &str, number: i64) -> Result<HashMap<String, f64>, Fail> {
    let (repo, workflow) = (repo.to_owned(), workflow.to_owned());
    let spans: Vec<(String, i64)> = store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT j.job_id, MAX(j.finished_at) - MIN(j.started_at)
                 FROM (SELECT id, attempt FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND number < ?3 AND conclusion = 'success' ORDER BY number DESC LIMIT 20) r
                 JOIN jobs j ON j.run_id = r.id AND j.run_attempt = r.attempt
                 WHERE j.job_id IS NOT NULL AND j.started_at IS NOT NULL AND j.finished_at IS NOT NULL
                 GROUP BY r.id, j.job_id",
            )?;
            let rows = st.query_map(params![repo, workflow, number], |x| Ok((x.get(0)?, x.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    let mut by_job: HashMap<String, Vec<i64>> = HashMap::new();
    for (job, span) in spans {
        by_job.entry(job).or_default().push(span.max(0));
    }
    Ok(by_job
        .into_iter()
        .map(|(job, mut s)| {
            s.sort_unstable();
            // The middle one, or the mean of the middle two.
            let at = |i: usize| s.get(i).copied().unwrap_or(0) as f64;
            let n = s.len();
            (job, (at(n.saturating_sub(1) / 2) + at(n / 2)) / 2.0)
        })
        .collect())
}

/// A run's graph: its jobs with their legs and spans, the edges between them, and the critical
/// path.
pub async fn graph_json(store: &crate::store::Store, id: &str, attempt: Option<i64>) -> Result<Value, Fail> {
    let legs = legs(store, id, attempt).await?;
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
    // The critical path (W19-R01): the longest path through `needs` by the jobs' spans.
    let jobs: Vec<kiln_ci::insights::Node> = order
        .iter()
        .map(|j| {
            let (start, end) = span(j);
            let needs = edges.iter().filter(|(_, to)| to == j).map(|(from, _)| from.clone()).collect();
            kiln_ci::insights::Node { id: j.clone(), needs, duration: if start > 0 { (end - start).max(0) as f64 } else { 0.0 } }
        })
        .collect();
    let path = kiln_ci::insights::critical_path(&jobs);
    let nodes: Vec<Value> = order
        .iter()
        .map(|j| {
            let ls = by_job.get(j).cloned().unwrap_or_default();
            let (start, end) = span(j);
            let leg_ids: Vec<Value> = ls.iter().map(|l| json!({ "id": l.get("id"), "key": l.get("key"), "name": l.get("name"), "matrix": l.get("matrix"), "status": l.get("status"), "conclusion": l.get("conclusion"), "wait_reason": l.get("wait_reason"), "reused": l.get("reused_from").is_some_and(|r| !r.is_null()) })).collect();
            json!({ "job": j, "legs": leg_ids, "started_at": (start > 0).then_some(start), "finished_at": (end > 0).then_some(end) })
        })
        .collect();
    let edges: Vec<Value> = edges.iter().map(|(a, b)| json!({ "from": a, "to": b })).collect();
    Ok(json!({ "nodes": nodes, "edges": edges, "critical_path": path }))
}

/// `GET /api/ci/runs/:id/annotations`, `/summary`, `/artifacts`.
pub async fn part(hub: &Hub, id: &str, which: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    run_json(&hub.store, id).await?;
    // Another provider's run (W23-R01): its artifacts, from the provider.
    if which == "artifacts"
        && let Some((repo, provider, ext)) = super::providers::of_run(hub, id).await.map_err(internal)?
    {
        let list = super::providers::artifacts(hub, &repo, &provider, &ext).await?;
        return Ok(srv::json(StatusCode::OK, &json!({ "provider": provider, "artifacts": list })));
    }
    let (run, which, attempt) = (id.to_owned(), which.to_owned(), attempt_of(q));
    let v = hub
        .store
        .call(move |c| {
            Ok(match which.as_str() {
                "annotations" => {
                    // The attempt's legs; a leg a rerun kept shows its original's.
                    let mut st = c.prepare(
                        "SELECT j.id, j.key, a.step_idx, a.level, a.title, a.message, a.file, a.line, a.end_line, a.col, a.end_col FROM jobs j
                         JOIN ci_annotations a ON a.leg_id = COALESCE(j.reused_from, j.id)
                         WHERE j.run_id = ?1 AND j.run_attempt = COALESCE(?2, (SELECT attempt FROM runs WHERE id = ?1)) ORDER BY a.id",
                    )?;
                    let rows = st.query_map(params![run, attempt], |r| {
                        Ok(json!({ "leg": r.get::<_, String>(0)?, "leg_key": r.get::<_, String>(1)?, "step": r.get::<_, Option<i64>>(2)?, "level": r.get::<_, String>(3)?,
                                   "title": r.get::<_, Option<String>>(4)?, "message": r.get::<_, String>(5)?, "file": r.get::<_, Option<String>>(6)?, "line": r.get::<_, Option<i64>>(7)?,
                                   "end_line": r.get::<_, Option<i64>>(8)?, "col": r.get::<_, Option<i64>>(9)?, "end_col": r.get::<_, Option<i64>>(10)? }))
                    })?;
                    json!({ "annotations": rows.collect::<Result<Vec<_>, _>>()? })
                }
                "summary" => {
                    let mut st = c.prepare(
                        "SELECT j.id, j.key, s.step_idx, s.markdown FROM jobs j JOIN ci_summaries s ON s.leg_id = COALESCE(j.reused_from, j.id)
                         WHERE j.run_id = ?1 AND j.run_attempt = COALESCE(?2, (SELECT attempt FROM runs WHERE id = ?1)) ORDER BY j.rowid, s.step_idx",
                    )?;
                    let rows = st.query_map(params![run, attempt], |r| Ok(json!({ "leg": r.get::<_, String>(0)?, "leg_key": r.get::<_, String>(1)?, "step": r.get::<_, i64>(2)?, "markdown": r.get::<_, String>(3)? })))?;
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

/// `GET /api/ci/runs/:id/pending`: the deployments a run waits for a review of, whichever provider.
pub async fn pending(hub: &Hub, id: &str) -> Result<Response, Fail> {
    run_json(&hub.store, id).await?;
    if let Some((repo, provider, ext)) = super::providers::of_run(hub, id).await.map_err(internal)? {
        let list = super::providers::pending(hub, &repo, &provider, &ext).await?;
        return Ok(srv::json(StatusCode::OK, &json!({ "provider": provider, "pending": list })));
    }
    let list: Vec<Value> = super::environments::waiting(hub, id).await.map_err(internal)?.into_iter().map(|e| json!({ "environment": e })).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "provider": super::providers::KILN, "pending": list })))
}

/// `GET /api/ci/runs/:id/artifacts/:artifact/download`: an artifact's archive, whichever provider
/// (KilnCI's: a short-lived link to its blob).
pub async fn download(hub: &Hub, id: &str, artifact: &str) -> Result<Response, Fail> {
    run_json(&hub.store, id).await?;
    if let Some((repo, provider, ext)) = super::providers::of_run(hub, id).await.map_err(internal)? {
        let bytes = super::providers::artifact(hub, &repo, &provider, &ext, artifact).await?;
        return Ok(srv::response(StatusCode::OK, "application/zip", srv::full(bytes)));
    }
    let (run, aid) = (id.to_owned(), artifact.parse::<i64>().map_err(|_| (StatusCode::NOT_FOUND, "no such artifact".to_owned()))?);
    let found: Option<(bool, Option<i64>)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT object IS NULL, expires_at FROM ci_artifacts WHERE id = ?1 AND run_id = ?2 AND finalized = 1", params![aid, run], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(internal)?;
    match found {
        Some((gone, expires)) if gone || expires.is_some_and(|e| e <= crate::util::now_unix()) => Err((StatusCode::GONE, "the artifact has expired".into())),
        Some(_) => {
            let origin = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
            let url = super::services::blob::url(hub, &origin, super::services::blob::Target::Artifact(aid), "r", 60);
            Ok(srv::with_header(srv::status(StatusCode::FOUND), http::header::LOCATION, &url))
        }
        None => Err((StatusCode::NOT_FOUND, "no such artifact".into())),
    }
}

/// `POST /api/ci/runs/:id/cancel`.
pub async fn cancel(hub: &Hub, id: &str) -> Result<Response, Fail> {
    run_json(&hub.store, id).await?;
    // Another provider's run (W23): its provider cancels it.
    if let Some((repo, provider, ext)) = super::providers::of_run(hub, id).await.map_err(internal)? {
        let v = super::providers::cancel(hub, &repo, &provider, &ext).await?;
        return Ok(srv::json(StatusCode::ACCEPTED, &v));
    }
    hub.cancel_run(id, "cancelled from the API").await.map_err(internal)?;
    Ok(srv::status(StatusCode::ACCEPTED))
}

/// `POST /api/ci/runs/:id/rerun`: the run again, from its saved event and the workflow as it
/// was (a new run; `{ mode: "all" }`, the default).
pub async fn rerun(hub: &Hub, id: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    use super::dispatch::RerunMode;
    // Another provider's run (W23): its provider reruns it.
    if let Some((repo, provider, ext)) = super::providers::of_run(hub, id).await.map_err(internal)? {
        let mode = body.get("mode").and_then(Value::as_str).unwrap_or("all");
        let v = super::providers::rerun(hub, &repo, &provider, &ext, mode, body.get("job").and_then(Value::as_str)).await?;
        return Ok(srv::json(StatusCode::CREATED, &v));
    }
    let mode = match body.get("mode").and_then(Value::as_str).unwrap_or("all") {
        "all" => RerunMode::All,
        "failed" => RerunMode::Failed,
        "job" => RerunMode::Job(body.get("job").and_then(Value::as_str).filter(|j| !j.is_empty()).ok_or_else(|| (StatusCode::BAD_REQUEST, "mode `job` names a `job`".to_owned()))?.to_owned()),
        other => return Err((StatusCode::BAD_REQUEST, format!("rerun mode `{other}`: `all`, `failed` or `job`"))),
    };
    let debug = body.get("debug").and_then(Value::as_bool).unwrap_or(false);
    // "Re-run and keep if it fails" (W17-R02): `keep_failed: true` (30 minutes) or minutes.
    let keep_failed = match body.get("keep_failed") {
        Some(Value::Bool(true)) => Some(30),
        Some(Value::Number(n)) => n.as_u64().filter(|m| *m > 0).map(|m| u32::try_from(m.min(u64::from(crate::runner::kept::MAX_KEEP_MINUTES))).unwrap_or(30)),
        _ => None,
    };
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
    // "Run anyway" (W08-R10): no leg takes an earlier result over. Run cold (W24-R23): nor does
    // any start from kept folders or restore a cache.
    let cold = body.get("cold").and_then(Value::as_bool).unwrap_or(false);
    let no_reuse = cold || body.get("no_reuse").and_then(Value::as_bool).unwrap_or(false);
    let (tx, rx) = async_channel::bounded(1);
    hub.ci.send(super::dispatch::Msg::Rerun { run: id.to_owned(), mode, debug, keep_failed, actions, no_reuse, cold, by: by.to_owned(), reply: tx });
    match crate::rt::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
        Ok(Ok(Ok(attempt))) => Ok(srv::json(StatusCode::CREATED, &json!({ "run": id, "attempt": attempt }))),
        Ok(Ok(Err(e))) => Err((StatusCode::CONFLICT, e)),
        _ => Err((StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned())),
    }
}

/// `GET /api/ci/legs/:id`: the leg, its steps and annotations, its attempts (each with its
/// conclusion, failure class and runner) and its outputs (W21-R05).
pub async fn leg(hub: &Hub, id: &str) -> Result<Response, Fail> {
    let leg_id = id.to_owned();
    let found: Option<(String, Vec<Value>, Vec<Value>, Vec<Value>, Value)> = hub
        .store
        .call(move |c| {
            let Some(run) = c.query_row("SELECT run_id FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [&leg_id], |r| r.get::<_, String>(0)).optional()? else { return Ok(None) };
            let mut st = c.prepare("SELECT idx, name, status, kind, step_id, action, outcome, conclusion, outputs, started_at, finished_at, started_seq FROM steps WHERE job_id = ?1 ORDER BY idx")?;
            let steps = st
                .query_map([&leg_id], |r| {
                    Ok(json!({ "idx": r.get::<_, i64>(0)?, "name": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                               "id": r.get::<_, Option<String>>(4)?, "action": r.get::<_, Option<String>>(5)?, "outcome": r.get::<_, Option<String>>(6)?,
                               "conclusion": r.get::<_, Option<String>>(7)?, "outputs": r.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                               "started_at": r.get::<_, Option<i64>>(9)?, "finished_at": r.get::<_, Option<i64>>(10)?, "seq": r.get::<_, Option<i64>>(11)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT step_idx, level, title, message, file, line FROM ci_annotations WHERE leg_id = ?1 ORDER BY id")?;
            let notes = st
                .query_map([&leg_id], |r| Ok(json!({ "step": r.get::<_, Option<i64>>(0)?, "level": r.get::<_, String>(1)?, "title": r.get::<_, Option<String>>(2)?, "message": r.get::<_, String>(3)?, "file": r.get::<_, Option<String>>(4)?, "line": r.get::<_, Option<i64>>(5)? })))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT number, status, conclusion, failure_class, runner, executor, queued_at, started_at, finished_at, exit_code, error FROM ci_attempts WHERE leg_id = ?1 ORDER BY number")?;
            let attempts = st
                .query_map([&leg_id], |r| {
                    Ok(json!({ "number": r.get::<_, i64>(0)?, "status": r.get::<_, String>(1)?, "conclusion": r.get::<_, Option<String>>(2)?, "class": r.get::<_, Option<String>>(3)?,
                               "runner": r.get::<_, Option<String>>(4)?, "executor": r.get::<_, Option<String>>(5)?, "queued_at": r.get::<_, Option<i64>>(6)?, "started_at": r.get::<_, Option<i64>>(7)?,
                               "finished_at": r.get::<_, Option<i64>>(8)?, "exit_code": r.get::<_, Option<i64>>(9)?, "error": r.get::<_, Option<String>>(10)? }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            // Its outputs as its job's `needs` read them (a secret one is kept sealed elsewhere).
            let outputs: Option<String> = c.query_row("SELECT outputs FROM jobs WHERE id = ?1", [&leg_id], |r| r.get(0)).optional()?.flatten();
            let outputs = outputs.and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or_else(|| json!({}));
            Ok(Some((run, steps, notes, attempts, outputs)))
        })
        .await
        .map_err(internal)?;
    let Some((run, steps, annotations, attempts, outputs)) = found else { return Err(missing("no such leg of the CI engine")) };
    let lid = id.to_owned();
    let attempt: Option<i64> = hub.store.call(move |c| Ok(c.query_row("SELECT run_attempt FROM jobs WHERE id = ?1", [&lid], |r| r.get(0)).optional()?)).await.map_err(internal)?;
    let all = legs(&hub.store, &run, attempt).await?;
    let me = all.into_iter().find(|l| l.get("id").and_then(Value::as_str) == Some(id)).unwrap_or(Value::Null);
    Ok(srv::json(StatusCode::OK, &json!({ "run": run, "leg": me, "steps": steps, "annotations": annotations, "attempts": attempts, "outputs": outputs })))
}

/// A leg's runner, if it is kept there now.
async fn kept_on(hub: &Hub, id: &str) -> Result<String, Fail> {
    let leg = id.to_owned();
    let row: Option<(Option<String>, Option<i64>)> = hub.store.call(move |c| Ok(c.query_row("SELECT agent, kept_until FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [&leg], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await.map_err(internal)?;
    match row {
        None => Err(missing("no such leg of the CI engine")),
        Some((Some(agent), Some(until))) if until > crate::util::now_unix() => Ok(agent),
        Some(_) => Err((StatusCode::CONFLICT, "the leg isn't kept (only a failed leg with keep-on-failure is)".into())),
    }
}

/// `POST /api/ci/legs/:id/release`: a kept leg let go now (W17-R02).
pub async fn release(hub: &Hub, id: &str, by: &str) -> Result<Response, Fail> {
    let agent = kept_on(hub, id).await?;
    // Parked: a runner that's away lets it go once it's back. Refused, it says why.
    let answer = hub.command(&agent, &crate::proto::ServerMsg::KeepRelease { job: id.to_owned() }, crate::hub::COMMAND_WITHIN).await;
    if let Some(f) = answer.failure(&agent) {
        return Err(f);
    }
    super::runs::kept(hub, id, None).await.map_err(internal)?;
    hub.ci.keep(id, None);
    hub.store.audit(by.to_owned(), "ci.legs.release".into(), id.to_owned()).await;
    hub.emit_job(id).await;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// `POST /api/ci/legs/:id/keep` with `{ minutes }` (default 30): a kept leg kept that much longer
/// from now ("Extend", W17-R02).
pub async fn extend(hub: &Hub, id: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    let agent = kept_on(hub, id).await?;
    let minutes = body.get("minutes").and_then(Value::as_u64).unwrap_or(30).clamp(1, u64::from(crate::runner::kept::MAX_KEEP_MINUTES));
    let minutes = u32::try_from(minutes).unwrap_or(30);
    let answer = hub.command(&agent, &crate::proto::ServerMsg::KeepExtend { job: id.to_owned(), minutes }, crate::hub::COMMAND_WITHIN).await;
    if let Some(f) = answer.failure(&agent) {
        return Err(f);
    }
    let until = crate::util::now_unix() + i64::from(minutes) * 60;
    super::runs::kept(hub, id, Some(until)).await.map_err(internal)?;
    hub.ci.keep(id, Some((agent.clone(), until)));
    hub.store.audit(by.to_owned(), "ci.legs.keep".into(), format!("{id} {minutes}m")).await;
    hub.emit_job(id).await;
    Ok(srv::json(StatusCode::OK, &json!({ "leg": id, "kept_until": until })))
}

/// `POST /api/ci/legs/:id/rerun-step` with `{ step }` (from 1, or `idx` from 0): a kept leg runs
/// that step again in place, as a new section of its log (W17-R04). Answers the section's index.
pub async fn rerun_step(hub: &Hub, id: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    let agent = kept_on(hub, id).await?;
    let step = match (body.get("idx").and_then(Value::as_u64), body.get("step").and_then(Value::as_u64)) {
        (Some(i), _) => i,
        (None, Some(n)) if n > 0 => n - 1,
        _ => return Err((StatusCode::BAD_REQUEST, "say which step: `step` (from 1) or `idx` (from 0)".into())),
    };
    let step = u32::try_from(step).map_err(|_| (StatusCode::BAD_REQUEST, "no such step".to_owned()))?;
    let leg = id.to_owned();
    let (known, busy, next): (bool, bool, u32) = hub
        .store
        .call(move |c| {
            let known: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM steps WHERE job_id = ?1 AND idx = ?2)", params![leg, step], |r| r.get(0))?;
            let busy: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM steps WHERE job_id = ?1 AND status = 'running')", [&leg], |r| r.get(0))?;
            let next: i64 = c.query_row("SELECT COALESCE(MAX(idx), -1) + 1 FROM steps WHERE job_id = ?1", [&leg], |r| r.get(0))?;
            Ok((known, busy, u32::try_from(next).unwrap_or(u32::MAX)))
        })
        .await
        .map_err(internal)?;
    if !known {
        return Err(missing("the leg has no such step"));
    }
    if busy {
        return Err((StatusCode::CONFLICT, "a step of this leg is running again already; rerun once it ends".into()));
    }
    // Live: refused at once while its runner is away, rather than run later.
    if let Some(f) = hub.command(&agent, &crate::proto::ServerMsg::JobRerunStep { job: id.to_owned(), step, idx: next }, crate::hub::COMMAND_WITHIN).await.failure(&agent) {
        return Err(f);
    }
    hub.store.audit(by.to_owned(), "ci.legs.rerun_step".into(), format!("{id} step {}", step + 1)).await;
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "leg": id, "step": step, "idx": next })))
}

/// The most of a command's output `exec` answers with.
const MAX_EXEC_OUTPUT: usize = 1 << 20;

/// `POST /api/ci/legs/:id/exec` with `{ command, timeout }`: runs the command in a running or kept
/// leg's shell, as its terminal would (W17-R05, for agents). Answers `{ output, exit_code,
/// timed_out, truncated }`, the output masked as the leg's is. `timeout` is in seconds: 60 unless
/// said, at most 600; past it the command is stopped.
pub async fn exec(hub: &std::sync::Arc<Hub>, id: &str, body: &Value, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    use crate::termsess::Frame;
    let command = body.get("command").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty()).ok_or_else(|| (StatusCode::BAD_REQUEST, "`command` is required".to_owned()))?.to_owned();
    let seconds = body.get("timeout").and_then(Value::as_u64).unwrap_or(60).clamp(1, 600);
    let leg = id.to_owned();
    let row: Option<(Option<String>, Option<String>, String, Option<i64>)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT agent, executor, status, kept_until FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [&leg], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?))
        .await
        .map_err(internal)?;
    let Some((agent, executor, status, kept_until)) = row else { return Err(missing("no such leg of the CI engine")) };
    let kept = kept_until.is_some_and(|t| t > crate::util::now_unix());
    let Some(agent) = agent.filter(|_| status == "running" || kept) else {
        return Err((StatusCode::CONFLICT, "the leg isn't running or kept: a command runs in a running leg, or a failed one kept with keep-on-failure".into()));
    };
    let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
    // On the host executor the leg's shell is a shell on the runner itself.
    if executor.as_deref() == Some("host") && caller.is_some_and(|c| !c.can("runners.terminal", &crate::authz::Scope::Runner { runner: agent.clone() })) {
        return Err((StatusCode::FORBIDDEN, format!("{by} may not run commands on runner {agent}: a host leg's shell needs runners.terminal there")));
    }
    hub.store.audit(by, "ci.legs.exec".into(), format!("{id}: {command}")).await;
    let (session, mut seen) = crate::termsess::exec_leg(hub, agent, id, &command);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let (mut out, mut code, mut error, mut timed_out, mut truncated) = (Vec::new(), None, None, false, false);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match crate::rt::timeout(left, seen.live.recv()).await {
            Ok(Ok(Frame::Data(d))) => {
                let room = MAX_EXEC_OUTPUT.saturating_sub(out.len());
                truncated |= d.len() > room;
                out.extend_from_slice(d.get(..d.len().min(room)).unwrap_or_default());
            }
            Ok(Ok(Frame::Exit { code: c, error: e })) => {
                (code, error) = (c, e);
                break;
            }
            Ok(Ok(Frame::Size { .. })) => {}
            Ok(Err(async_broadcast::RecvError::Overflowed(_))) => truncated = true,
            Ok(Err(_)) => break,
            Err(_) => {
                timed_out = true;
                session.close();
                break;
            }
        }
    }
    session.detach();
    if let (None, Some(e)) = (code, &error)
        && out.is_empty()
        && !timed_out
    {
        return Err((StatusCode::CONFLICT, e.clone()));
    }
    let output = String::from_utf8_lossy(&out).replace("\r\n", "\n");
    Ok(srv::json(StatusCode::OK, &json!({ "output": output, "exit_code": code, "timed_out": timed_out, "truncated": truncated })))
}

/// `GET /api/ci/legs/:id/logs?step=&from=`: from byte `from` on, for a follower.
pub async fn logs(hub: &Hub, id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let step = q.get("step").and_then(|v| v.parse().ok()).unwrap_or(0);
    let from: usize = q.get("from").and_then(|v| v.parse().ok()).unwrap_or(0);
    // Another provider's leg (W23): its log, read from the provider.
    if let Some((repo, provider, run, job)) = super::providers::of_leg(hub, id).await.map_err(internal)? {
        let text = super::providers::logs(hub, &repo, &provider, &run, &job).await?;
        let data = text.into_bytes();
        let data = if from == 0 { data } else { data.get(from..).map(<[u8]>::to_vec).unwrap_or_default() };
        return Ok(srv::response(StatusCode::OK, "text/plain; charset=utf-8", srv::full(data)));
    }
    let mut data = hub.logs.read(id, step).await;
    // A leg a rerun kept: the logs are those of the leg whose result it keeps (W05-R17).
    if data.is_empty() {
        let leg = id.to_owned();
        let from_leg: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT reused_from FROM jobs WHERE id = ?1", [&leg], |r| r.get(0)).optional()?.flatten())).await.map_err(internal)?;
        if let Some(reused) = from_leg {
            data = hub.logs.read(&reused, step).await;
        }
    }
    let data = if from == 0 { data } else { data.get(from..).map(<[u8]>::to_vec).unwrap_or_default() };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// W24-R28 on the leg API: a reused leg's report names its source, its runner and the time it
    /// saved; a leg that ran has none (yet).
    #[test]
    fn w24_r28_a_reused_legs_report() {
        let v = reused_report(Some("l1".into()), Some("r1".into()), Some(12), Some("fedora".into()), Some(100), Some(943));
        assert_eq!(
            v,
            Some(json!({ "how": "reused", "run": "r1", "number": 12, "leg": "l1", "runner": "fedora", "saved_s": 843.0, "summary": "reused from run #12 on fedora, 14 m 3 s saved" }))
        );
        let unknown = reused_report(Some("l1".into()), Some("r1".into()), None, None, None, None);
        assert_eq!(unknown, Some(json!({ "how": "reused", "run": "r1", "leg": "l1", "summary": "reused from run r1" })));
        assert_eq!(reused_report(None, None, None, Some("fedora".into()), Some(1), Some(2)), None);
    }

    /// The same through the legs `GET /api/ci/runs/:id` and `/api/ci/legs/:id` show: the copy
    /// that took a leg's result carries the report, naming the run and leg it came from; that leg,
    /// which ran, carries none.
    #[test]
    fn w24_r28_the_leg_api_reports_a_reused_leg() -> anyhow::Result<()> {
        let store = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            let repo = crate::store::Repo {
                id: "r1".into(),
                name: "acme/app".into(),
                clone_url: "https://example.com/acme/app.git".into(),
                branches: Vec::new(),
                poll_seconds: 60,
                enabled: true,
                created_at: 0,
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            };
            let repo_id = store.upsert_repo(repo).await?.id;
            store
                .call(move |c| {
                    c.execute("INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('run1', ?1, 11, 'ci', 'push', 'main', 'abc', 'completed', 0), ('run2', ?1, 12, 'ci', 'push', 'main', 'def', 'completed', 0)", params![repo_id])?;
                    for (id, run, from) in [("src", "run1", None), ("copy", "run2", Some("src"))] {
                        c.execute(
                            "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, job_id, agent, started_at, finished_at, reused_from) VALUES (?1, ?2, 'build', 'build', 'succeeded', '[]', '[]', '{}', 'build', 'fedora', 100, 943, ?3)",
                            params![id, run, from],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            let first = |ls: Vec<Value>| ls.into_iter().next().unwrap_or(Value::Null);
            let copy = first(legs(&store, "run2", None).await.map_err(|e| anyhow::anyhow!("{e:?}"))?);
            anyhow::ensure!(
                copy.get("work") == Some(&json!({ "how": "reused", "run": "run1", "number": 11, "leg": "src", "runner": "fedora", "saved_s": 843.0, "summary": "reused from run #11 on fedora, 14 m 3 s saved" })),
                "{copy}"
            );
            let source = first(legs(&store, "run1", None).await.map_err(|e| anyhow::anyhow!("{e:?}"))?);
            anyhow::ensure!(source.get("id") == Some(&json!("src")) && source.get("work").is_none(), "{source}");
            Ok(())
        })
    }
}
