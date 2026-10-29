//! A finished job run again, here or elsewhere (docs/ci.md W05-R28): a leg of the CI engine (a
//! new attempt of its run: that leg, and the jobs that need it), a `.kiln/` pipeline's job (v1: in
//! its run again, with the jobs that need it), or another provider's job (its provider reruns it).
//! On any runner that can run it, on any but the one its last attempt ran on (`elsewhere`), or on
//! one named (`runner`).
//!
//! - `GET /api/ci/legs/:id/runners`: the runners that could run it again, each able to or why not.
//! - `POST /api/ci/legs/:id/retry` with `{ runner?, elsewhere? }`.

use std::collections::HashMap;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::sched::Placement;
use rusqlite::OptionalExtension as _;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::{JobRetry, Status};

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn conflict(m: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, m.into())
}

/// A job as a retry sees it.
struct Job {
    run: String,
    key: String,
    /// Its workflow job, for a leg of the engine's; `None` for a `.kiln/` pipeline's job.
    job: Option<String>,
    status: Status,
    /// The runner its last attempt ran on, if one did.
    agent: Option<String>,
    executor: Option<String>,
    /// A pipeline job's `runs-on`.
    labels: Vec<String>,
    /// A pipeline job's `requires`, which v1 places it by too (W05-R22).
    requires: Vec<String>,
    /// Its run has finished.
    run_done: bool,
    /// It's of its run's latest attempt (a pipeline's job always is).
    latest: bool,
    /// What ran it: `kiln`, or another CI provider (W23).
    provider: String,
}

impl Job {
    /// `ci` (the engine's leg), `pipeline` (v1), or the provider that ran it.
    fn kind(&self) -> &str {
        if self.provider != super::providers::KILN {
            &self.provider
        } else if self.job.is_some() {
            "ci"
        } else {
            "pipeline"
        }
    }

    /// Whether kiln picks its runner: a runner can be named for it, or avoided.
    fn placeable(&self) -> bool {
        matches!(self.kind(), "ci" | "pipeline")
    }

    /// It ran in a Kubernetes pod made for it, as each of its attempts does: avoiding that one
    /// says nothing.
    fn podded(&self) -> bool {
        self.executor.as_deref() == Some("k8s")
    }
}

async fn job_of(hub: &Hub, id: &str) -> Result<Job, Fail> {
    type Row = (String, String, Option<String>, String, Option<String>, Option<String>, String, i64, String, i64, String, Option<String>);
    let key = id.to_owned();
    let row: Option<Row> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT j.run_id, j.key, j.job_id, j.status, j.agent, j.executor, j.labels, j.run_attempt, r.status, r.attempt, r.provider, j.spec FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)),
            )
            .optional()?)
        })
        .await
        .map_err(internal)?;
    let Some((run, key, job, status, agent, executor, labels, leg_attempt, run_status, run_attempt, provider, spec)) = row else { return Err((StatusCode::NOT_FOUND, "no such job".into())) };
    let latest = job.is_none() || leg_attempt == run_attempt;
    let requires = spec.and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.get("requires").cloned()).and_then(|r| serde_json::from_value(r).ok()).unwrap_or_default();
    Ok(Job {
        run,
        key,
        job,
        status: Status::parse(&status),
        // A pipeline's job skipped by its `if:` was claimed by no runner (`-`).
        agent: agent.filter(|a| !a.is_empty() && a != "-"),
        executor,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        requires,
        run_done: Status::parse(&run_status).is_terminal(),
        latest,
        provider,
    })
}

/// Why it can't run again now, if it can't.
fn why_not(hub: &Hub, j: &Job) -> Option<String> {
    if !j.status.is_terminal() {
        return Some(format!("it hasn't finished: it's {}", j.status.as_str()));
    }
    match j.kind() {
        "pipeline" if j.status == Status::Skipped => Some("it was skipped: retry the job it needs, or the run".into()),
        "ci" if !hub.ci.enabled() => Some("the CI engine is off ([ci] enabled)".into()),
        "ci" if !j.latest => Some("it's of an earlier attempt of its run: retry it in the latest".into()),
        "ci" if !j.run_done => Some("its run is still in progress: cancel it or wait for it to finish".into()),
        _ => None,
    }
}

/// A runner that could run a job again, or why it can't.
struct Candidate {
    name: String,
    connected: bool,
    draining: bool,
    busy: u32,
    slots: u32,
    why: Vec<String>,
}

/// The runners of their own (no pod made for one job, no outpost, which takes none) a job could
/// run on again, each with why it can't when it can't: a pipeline's job as v1 places it (its
/// labels and `requires`), a leg as routing decides (its requirements, rules and capabilities,
/// W05-R20). Those able first.
async fn candidates(hub: &Hub, id: &str, j: &Job) -> Result<Vec<Candidate>, Fail> {
    let agents: Vec<crate::hub::AgentLive> = hub.agents.lock().values().filter(|a| !a.ephemeral && !a.outpost).cloned().collect();
    let candidate = |a: &crate::hub::AgentLive, why: Vec<String>| Candidate { name: a.name.clone(), connected: a.connected, draining: a.draining, busy: a.busy, slots: a.slots, why };
    let mut out: Vec<Candidate> = match j.kind() {
        "pipeline" => agents.iter().map(|a| candidate(a, crate::hub::cannot_take(&j.labels, &j.requires, a))).collect(),
        "ci" => {
            let route = super::dispatch::stored_route(hub, id).await.map_err(internal)?.ok_or_else(|| conflict("the leg's routing isn't kept"))?;
            let explained = super::routing::route_now(hub, route).await.ok_or_else(|| (StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned()))?;
            let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            let failing: HashMap<String, Vec<String>> = explained
                .get("runners")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| {
                    let why = r.get("checks").and_then(Value::as_array).into_iter().flatten().filter(|c| c.get("pass") == Some(&json!(false))).map(|c| format!("needs {}, has {}", text(c, "requirement"), text(c, "value"))).collect();
                    (text(r, "name"), why)
                })
                .collect();
            agents.iter().filter_map(|a| failing.get(&a.name).map(|why| candidate(a, why.clone()))).collect()
        }
        _ => Vec::new(),
    };
    out.sort_by(|a, b| b.why.is_empty().cmp(&a.why.is_empty()).then(b.connected.cmp(&a.connected)).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

/// Where the retry asks it to run, checked: a runner able to run it, or any but the one its
/// last attempt ran on. `None`: anywhere it can.
async fn placement_of(hub: &Hub, id: &str, j: &Job, body: &Value) -> Result<Option<Placement>, Fail> {
    let runner = body.get("runner").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned);
    let elsewhere = body.get("elsewhere").and_then(Value::as_bool).unwrap_or(false);
    match (runner, elsewhere) {
        (None, false) => Ok(None),
        (Some(_), true) => Err(bad("say `runner` or `elsewhere`, not both")),
        _ if !j.placeable() => Err(bad(format!("{} picks its own runners: retry it without `runner` or `elsewhere`", j.provider))),
        (None, true) if j.podded() => Err(bad("it ran in a Kubernetes pod made for it, as each of its attempts does: retry it without `elsewhere`")),
        (None, true) => {
            let ran = j.agent.clone().ok_or_else(|| conflict("it never ran on a runner: retry it without `elsewhere`"))?;
            Ok(Some(Placement { runner: None, avoid: Some(ran) }))
        }
        (Some(name), false) => match candidates(hub, id, j).await?.into_iter().find(|c| c.name == name) {
            Some(c) if c.why.is_empty() => Ok(Some(Placement { runner: Some(name), avoid: None })),
            Some(c) => Err(conflict(format!("{name} can't run it: {}", c.why.join("; ")))),
            None if hub.agents.lock().get(&name).is_some_and(|a| a.ephemeral || a.outpost) => Err(bad(format!("{name} takes no job asked of it: a pod made for another job, or an outpost"))),
            None => Err(bad(format!("no runner `{name}`"))),
        },
    }
}

/// What a placement says, for logs and the audit: ` on X`, ` anywhere but X`.
fn said(p: Option<&Placement>) -> String {
    match p.map(|p| (&p.runner, &p.avoid)) {
        Some((Some(r), _)) => format!(" on {r}"),
        Some((None, Some(a))) => format!(" anywhere but {a}"),
        _ => String::new(),
    }
}

/// A line in each of a job's step logs where its last attempt ends: a pipeline's job retried in
/// place keeps its logs, and the next attempt's follow.
async fn mark(hub: &Hub, job: &str, line: &str) {
    let steps = hub.store.job(job.to_owned()).await.ok().flatten().map(|j| j.steps).unwrap_or_default();
    for s in steps {
        hub.logs.mark(job, s.idx, line).await;
    }
}

/// `GET /api/ci/legs/:id/runners`: where a finished job could run again (W05-R28): the runners
/// of their own, each able to run it or why not; the one its last attempt ran on (`ran_on`), and
/// whether that one can be avoided (`elsewhere`); and whether it can be retried now.
pub async fn runners(hub: &Hub, id: &str) -> Result<Response, Fail> {
    let j = job_of(hub, id).await?;
    let why = why_not(hub, &j);
    // With the engine off, nothing can say where a leg would go.
    let list = if j.kind() == "ci" && !hub.ci.enabled() { Vec::new() } else { candidates(hub, id, &j).await? };
    let runners: Vec<Value> = list.iter().map(|c| json!({ "name": c.name, "connected": c.connected, "draining": c.draining, "busy": c.busy, "slots": c.slots, "able": c.why.is_empty(), "why": c.why })).collect();
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "job": id, "run": j.run, "key": j.key, "kind": j.kind(),
            "ran_on": j.agent, "elsewhere": j.agent.is_some() && j.placeable() && !j.podded(),
            "retry": { "can": why.is_none(), "why": why },
            "runners": runners,
        }),
    ))
}

/// `POST /api/ci/legs/:id/retry` with `{ runner?, elsewhere? }` (W05-R28): a finished job again,
/// on any runner that can run it, on any but the one its last attempt ran on, or on the one
/// named. A leg of the engine's is a new attempt of its run (the leg, and the jobs that need it,
/// run for real: none takes an earlier result over); a pipeline's job runs again in its run, with
/// the jobs that need it; another provider's is rerun by that provider.
pub async fn retry(hub: &Hub, id: &str, body: &Value, by: &str) -> Result<Response, Fail> {
    let j = job_of(hub, id).await?;
    if let Some(why) = why_not(hub, &j) {
        return Err(conflict(why));
    }
    let placement = placement_of(hub, id, &j, body).await?;
    let said = said(placement.as_ref());
    match j.kind() {
        "pipeline" => match hub.store.retry_job(id.to_owned(), placement.clone()).await.map_err(internal)? {
            JobRetry::NoSuchJob => Err((StatusCode::NOT_FOUND, "no such job".into())),
            JobRetry::Refused(why) => Err(conflict(why)),
            JobRetry::Again { run, attempt, waiting } => {
                mark(hub, id, &format!("retried by {by}{said}: its next attempt follows")).await;
                for w in &waiting {
                    mark(hub, w, &format!("run again with {}'s retry: its next attempt follows", j.key)).await;
                }
                for w in std::iter::once(id).chain(waiting.iter().map(String::as_str)) {
                    hub.emit_job(w).await;
                }
                hub.emit_run(&run).await;
                hub.wake.notify();
                hub.store.audit(by.to_owned(), "ci.legs.retry".into(), format!("{id}{said}")).await;
                Ok(srv::json(StatusCode::CREATED, &json!({ "kind": "pipeline", "run": run, "job": id, "attempt": attempt, "waiting": waiting, "placement": placement })))
            }
        },
        "ci" => {
            let mode = super::dispatch::RerunMode::Leg { job: j.job.clone().unwrap_or_default(), key: j.key.clone() };
            let placed: HashMap<String, Placement> = placement.clone().map(|p| (j.key.clone(), p)).into_iter().collect();
            let (tx, rx) = async_channel::bounded(1);
            hub.ci.send(super::dispatch::Msg::Rerun { run: j.run.clone(), mode, debug: false, keep_failed: None, actions: None, no_reuse: true, cold: false, placed, by: by.to_owned(), reply: tx });
            match crate::rt::timeout(Duration::from_secs(60), rx.recv()).await {
                Ok(Ok(Ok(attempt))) => {
                    hub.store.audit(by.to_owned(), "ci.legs.retry".into(), format!("{id}{said}")).await;
                    Ok(srv::json(StatusCode::CREATED, &json!({ "kind": "ci", "run": j.run, "attempt": attempt, "key": j.key, "placement": placement })))
                }
                Ok(Ok(Err(e))) => Err(conflict(e)),
                _ => Err((StatusCode::GATEWAY_TIMEOUT, "the dispatcher didn't answer".to_owned())),
            }
        }
        provider => {
            let Some((repo, provider, ext, _)) = super::providers::of_leg(hub, id).await.map_err(internal)? else { return Err(conflict(format!("{provider} doesn't know the job"))) };
            let v = super::providers::rerun(hub, &repo, &provider, &ext, "job", Some(id)).await?;
            Ok(srv::json(StatusCode::CREATED, &json!({ "kind": provider, "run": j.run, "rerun": v })))
        }
    }
}
