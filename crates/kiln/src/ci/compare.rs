//! Comparing runs and legs (docs/ci.md W19-R03).
//!
//! - `POST /_kiln/facts`: a leg reports what it runs on (machine, runner version, executor,
//!   images, its environment with secrets masked), kept on its attempt.
//! - `GET /api/ci/runs/:id/compare/:other` and `GET /api/ci/legs/:id/compare/:other`: what
//!   differs from `:other` (the base) to `:id` (the head), each leg's steps side by side, and the
//!   files changed between their commits. `:other` may be `last-green`: the last run of the same
//!   workflow and branch that succeeded before this one.

use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use kiln_ci::compare::{LegSide, RunSide};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// `POST /_kiln/facts` from a leg: kept on its current attempt.
pub async fn facts_from_leg(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let body: Value = srv::body_json(req.into_body(), 256 << 10).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let (leg, facts) = (claims.sub.clone(), body.to_string());
    hub.store
        .call(move |c| Ok(c.execute("UPDATE ci_attempts SET facts = ?2 WHERE leg_id = ?1 AND number = (SELECT attempt FROM jobs WHERE id = ?1)", params![leg, facts]).map(|_| ())?))
        .await
        .map_err(internal)?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

/// A run's row, as the comparison heads it.
#[derive(Debug, Clone)]
struct RunRow {
    id: String,
    number: i64,
    repo: String,
    workflow: String,
    branch: String,
    sha: String,
    conclusion: Option<String>,
    created: i64,
    duration: Option<i64>,
}

impl RunRow {
    fn json(&self) -> Value {
        json!({ "id": self.id, "number": self.number, "workflow": self.workflow, "branch": self.branch, "sha": self.sha, "conclusion": self.conclusion, "created_at": self.created, "duration": self.duration })
    }
}

async fn run_row(hub: &Hub, id: &str) -> anyhow::Result<Option<RunRow>> {
    let id = id.to_owned();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT id, number, repo_id, COALESCE(workflow_id, ''), branch, sha, conclusion, created_at, finished_at - COALESCE(started_at, created_at) FROM runs WHERE id = ?1",
                [id],
                |x| Ok(RunRow { id: x.get(0)?, number: x.get(1)?, repo: x.get(2)?, workflow: x.get(3)?, branch: x.get(4)?, sha: x.get(5)?, conclusion: x.get(6)?, created: x.get(7)?, duration: x.get(8)? }),
            )
            .optional()?)
        })
        .await
}

/// The last run of `run`'s workflow and branch that succeeded before it.
async fn last_green(hub: &Hub, run: &RunRow) -> anyhow::Result<Option<String>> {
    let r = run.clone();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT id FROM runs WHERE repo_id = ?1 AND workflow_id = ?2 AND branch = ?3 AND conclusion = 'success' AND created_at <= ?4 AND id != ?5 ORDER BY created_at DESC, number DESC LIMIT 1",
                params![r.repo, r.workflow, r.branch, r.created, r.id],
                |x| x.get(0),
            )
            .optional()?)
        })
        .await
}

/// A leg's row: its id, key, and side.
type LegRow = (String, String, LegSide);

/// What a run used and saw, its legs (of its latest attempt) by key, with their ids.
async fn side(hub: &Hub, run: &str) -> anyhow::Result<(RunSide, BTreeMap<String, String>)> {
    let run = run.to_owned();
    let (input, legs): (Option<String>, Vec<LegRow>) = hub
        .store
        .call(move |c| {
            let input: Option<String> = c.query_row("SELECT input FROM ci_plans WHERE run_id = ?1", [&run], |x| x.get(0)).optional()?;
            let mut st = c.prepare(
                "SELECT j.id, j.key, COALESCE(j.conclusion, j.status), COALESCE(j.agent, ''), COALESCE(j.executor, ''), j.finished_at - j.started_at,
                        (SELECT facts FROM ci_attempts a WHERE a.leg_id = j.id ORDER BY a.number DESC LIMIT 1)
                 FROM jobs j WHERE j.run_id = ?1 AND j.job_id IS NOT NULL AND j.run_attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY j.rowid",
            )?;
            let rows = st.query_map([&run], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?, x.get::<_, String>(2)?, x.get::<_, String>(3)?, x.get::<_, String>(4)?, x.get::<_, Option<i64>>(5)?, x.get::<_, Option<String>>(6)?)))?;
            let mut legs = Vec::new();
            for r in rows {
                let (id, key, status, runner, executor, duration, facts) = r?;
                let mut leg = LegSide { conclusion: conclusion(&status), duration: duration.map(|d| d.max(0) as f64), runner, executor, ..LegSide::default() };
                if let Some(f) = facts.and_then(|f| serde_json::from_str::<Value>(&f).ok()) {
                    let text = |k: &str| f.get(k).and_then(Value::as_str).map(str::to_owned);
                    leg.machine = text("machine");
                    leg.version = text("version");
                    leg.images = f
                        .get("images")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|i| Some((i.get("role")?.as_str()?.to_owned(), (i.get("image")?.as_str()?.to_owned(), i.get("id")?.as_str()?.to_owned()))))
                        .collect();
                    leg.env = f.get("env").and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default();
                }
                let mut st = c.prepare("SELECT name, finished_at - started_at, COALESCE(conclusion, status) FROM steps WHERE job_id = ?1 AND started_at IS NOT NULL ORDER BY COALESCE(started_seq, 1000000000 + idx)")?;
                leg.steps = st.query_map([&id], |x| Ok((x.get::<_, String>(0)?, x.get::<_, Option<i64>>(1)?.map(|d| d.max(0) as f64), conclusion(&x.get::<_, String>(2)?))))?.collect::<Result<Vec<_>, _>>()?;
                let mut st = c.prepare("SELECT key, kind FROM ci_cache_events WHERE leg_id = ?1 AND kind IN ('hit', 'miss') ORDER BY id")?;
                for r in st.query_map([&id], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))? {
                    let (key, kind) = r?;
                    leg.cache.insert(super::insights::key_prefix(&key), kind);
                }
                legs.push((id, key, leg));
            }
            Ok((input, legs))
        })
        .await?;
    let input: Value = input.and_then(|i| serde_json::from_str(&i).ok()).unwrap_or_default();
    let mut s = RunSide::default();
    if let Some(a) = input.get("actions").and_then(Value::as_object) {
        s.actions = a.iter().filter_map(|(uses, x)| Some((uses.clone(), x.get("sha")?.as_str()?.to_owned()))).collect();
    }
    if let Some(v) = input.get("vars").and_then(Value::as_object) {
        s.vars = v.iter().map(|(k, x)| (k.clone(), x.as_str().map_or_else(|| x.to_string(), str::to_owned))).collect();
    }
    let mut ids = BTreeMap::new();
    for (id, key, leg) in legs {
        ids.insert(key.clone(), id);
        s.legs.insert(key, leg);
    }
    Ok((s, ids))
}

fn conclusion(s: &str) -> String {
    match s {
        "succeeded" => "success",
        "failed" => "failure",
        other => other,
    }
    .to_owned()
}

fn leg_json(id: Option<&String>, l: &LegSide) -> Value {
    json!({ "id": id, "conclusion": l.conclusion, "duration": l.duration, "runner": l.runner, "executor": l.executor, "machine": l.machine })
}

/// The files changed from `base` to `head` (at most 500 listed).
async fn files(hub: &Hub, repo: &str, base: &str, head: &str) -> Value {
    if base == head {
        return json!({ "count": 0, "files": [] });
    }
    let found = match hub.store.repos().await {
        Ok(rs) => rs.into_iter().find(|r| r.id == repo),
        Err(_) => None,
    };
    match found {
        Some(r) => match crate::gitops::changed_files(hub, &r, base, head).await {
            Some(f) => json!({ "count": f.len(), "files": f.iter().take(500).collect::<Vec<_>>() }),
            None => json!({ "count": null, "files": [], "error": "the commits aren't both in the server's mirror" }),
        },
        None => json!({ "count": null, "files": [] }),
    }
}

/// The other run: an id, or `last-green`. In the same repository as `head`.
async fn other_run(hub: &Hub, head: &RunRow, other: &str) -> Result<RunRow, Fail> {
    let id = if other == "last-green" {
        last_green(hub, head).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "no run of this workflow and branch succeeded before this one".to_owned()))?
    } else {
        other.to_owned()
    };
    let base = run_row(hub, &id).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no run `{id}`")))?;
    if base.repo != head.repo {
        return Err((StatusCode::BAD_REQUEST, "runs compare within one repository".into()));
    }
    Ok(base)
}

/// `GET /api/ci/runs/:id/compare/:other`.
pub async fn runs(hub: &Hub, id: &str, other: &str) -> Result<Response, Fail> {
    let head = run_row(hub, id).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no run `{id}`")))?;
    let base = other_run(hub, &head, other).await?;
    let ((a, a_ids), (b, b_ids)) = (side(hub, &base.id).await.map_err(internal)?, side(hub, &head.id).await.map_err(internal)?);
    let differences = kiln_ci::compare::differences(&a, &b);
    let keys: std::collections::BTreeSet<&String> = a.legs.keys().chain(b.legs.keys()).collect();
    let legs: Vec<Value> = keys
        .into_iter()
        .map(|k| {
            let (x, y) = (a.legs.get(k), b.legs.get(k));
            let steps = kiln_ci::compare::steps(x.unwrap_or(&LegSide::default()), y.unwrap_or(&LegSide::default()));
            json!({ "key": k, "base": x.map(|l| leg_json(a_ids.get(k), l)), "head": y.map(|l| leg_json(b_ids.get(k), l)), "steps": steps })
        })
        .collect();
    let files = files(hub, &head.repo, &base.sha, &head.sha).await;
    Ok(srv::json(StatusCode::OK, &json!({ "base": base.json(), "head": head.json(), "differences": differences, "files": files, "legs": legs })))
}

/// `GET /api/ci/legs/:id/compare/:other`: two legs (`:other` a leg's id, or `last-green`: the
/// leg of the same key in the run's last green one).
pub async fn legs(hub: &Hub, id: &str, other: &str) -> Result<Response, Fail> {
    let of = |leg: String| async move {
        hub.store
            .call(move |c| Ok(c.query_row("SELECT run_id, key FROM jobs WHERE id = ?1 AND job_id IS NOT NULL", [leg], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?))).optional()?))
            .await
    };
    let (head_run, head_key) = of(id.to_owned()).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no leg `{id}`")))?;
    let head = run_row(hub, &head_run).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, "the run is gone".to_owned()))?;
    let (base, base_key) = if other == "last-green" {
        (other_run(hub, &head, "last-green").await?, head_key.clone())
    } else {
        let (run, key) = of(other.to_owned()).await.map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("no leg `{other}`")))?;
        (other_run(hub, &head, &run).await?, key)
    };
    let ((a, a_ids), (b, b_ids)) = (side(hub, &base.id).await.map_err(internal)?, side(hub, &head.id).await.map_err(internal)?);
    let (Some(x), Some(y)) = (a.legs.get(&base_key), b.legs.get(&head_key)) else { return Err((StatusCode::NOT_FOUND, format!("the other run has no leg `{base_key}`"))) };
    let mut differences = kiln_ci::compare::differences(&RunSide { legs: BTreeMap::new(), ..a.clone() }, &RunSide { legs: BTreeMap::new(), ..b.clone() });
    differences.extend(kiln_ci::compare::leg_differences(&head_key, x, y));
    let files = files(hub, &head.repo, &base.sha, &head.sha).await;
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "base": base.json(), "head": head.json(), "differences": differences, "files": files,
            "legs": [{ "key": head_key, "base": leg_json(a_ids.get(&base_key), x), "head": leg_json(b_ids.get(&head_key), y), "steps": kiln_ci::compare::steps(x, y) }],
        }),
    ))
}
