//! CI's operations (docs/ci.md W20-R01, R02, R04; W17-R05): what agents reach through MCP, as
//! tools one by one (`ci_runs_list`) and as code mode's `kiln.ci.*`. Each is one of the HTTP
//! routes of §4.1: it runs that route's handler, and the route table's rule for it decides who
//! may, so an agent can do through an operation exactly what its caller can over HTTP.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use http::{Method, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};

use crate::http::server::Response;
use crate::ops::{Cx, Fail, Out};

type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

/// One operation.
pub struct Op {
    /// Its name, dotted (`ci.runs.list`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: fn() -> Value,
    /// The routes' prefix under `/api`: `ci` for CI's own, empty for the rest (`/api/diffs/…`).
    base: &'static str,
    /// The route it is: method, path under `/api/<base>`, query.
    route: fn(&Value) -> Result<(Method, Vec<String>, HashMap<String, String>), Fail>,
    run: fn(Cx, Value) -> Fut,
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn text(v: &Value, k: &str) -> Result<String, Fail> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| bad(format!("`{k}` is required")))
}

/// The query of a list: these keys of the input, as strings.
fn query(v: &Value, keys: &[&str]) -> HashMap<String, String> {
    keys.iter()
        .filter_map(|k| {
            let x = v.get(*k)?;
            let s = match x {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some(((*k).to_owned(), s))
        })
        .collect()
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const RUN: &str = "The run's id.";
const LEG: &str = "The leg's id (a job of a run, one matrix combination of it).";
const REPO: &str = "The repository, `owner/name`.";

/// A handler's response as an operation's answer.
async fn answer(r: Result<Response, Fail>) -> Out {
    let resp = r?;
    let status = resp.status();
    let json = resp.headers().get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("json"));
    let bytes = resp.into_body().collect().await.map(http_body_util::Collected::to_bytes).unwrap_or_default();
    let v = if bytes.is_empty() {
        Value::Null
    } else if json {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    } else {
        json!({ "text": String::from_utf8_lossy(&bytes) })
    };
    if status.is_success() { Ok(v) } else { Err((status, v.get("error").and_then(Value::as_str).map_or_else(|| status.to_string(), str::to_owned))) }
}

fn by(cx: &Cx) -> String {
    cx.caller.name.clone()
}

macro_rules! op {
    ($name:literal, $summary:literal, ro: $ro:expr, destructive: $d:expr, input: $input:expr, route: $route:expr, run: $run:expr) => {
        Op { name: $name, summary: $summary, read_only: $ro, destructive: $d, input: $input, base: "ci", route: $route, run: |cx, v| Box::pin($run(cx, v)) }
    };
    // One whose route is outside `/api/ci`.
    ($name:literal, $summary:literal, base: $base:literal, ro: $ro:expr, destructive: $d:expr, input: $input:expr, route: $route:expr, run: $run:expr) => {
        Op { name: $name, summary: $summary, read_only: $ro, destructive: $d, input: $input, base: $base, route: $route, run: |cx, v| Box::pin($run(cx, v)) }
    };
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A diff route (W14), as its HTTP route answers.
async fn diffs(cx: &Cx, method: Method, segs: &[&str], q: &HashMap<String, String>, body: &Value) -> Out {
    answer(super::diffs::route(&cx.hub, &method, segs, q, &cx.caller, body).await).await
}

/// A secret or variable scope (`repo:acme/app`, `env:acme/app:production`, …).
async fn scope_of(cx: &Cx, v: &Value) -> Result<super::secrets::Scope, Fail> {
    super::secrets::parse_scope(&cx.hub, &text(v, "scope")?).await.map_err(bad)
}

const DIFF: &str = "The diff: `D12`.";
const SCOPE: &str = "Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or `env:<repo>:<environment>`.";

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// The operations.
pub fn ops() -> &'static [Op] {
    static OPS: std::sync::OnceLock<Vec<Op>> = std::sync::OnceLock::new();
    OPS.get_or_init(|| {
        vec![
            op!("ci.runs.list", "Runs, newest first, filtered: by repository, workflow, branch, event, conclusion or actor; a page at a time (cursor).", ro: true, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "workflow": {"type": "string", "description": "Its path, e.g. `.github/workflows/ci.yml`."},
                    "branch": {"type": "string"}, "event": {"type": "string"}, "conclusion": {"type": "string", "description": "success, failure, cancelled, skipped, timed_out, startup_failure."},
                    "actor": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 200}, "cursor": {"type": "string", "description": "The `next` of the previous page."}
                }), &[]),
                route: |v| Ok((Method::GET, path(&["runs"]), query(v, &["repo", "workflow", "branch", "event", "conclusion", "actor", "limit", "cursor"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::list(&cx.hub, &query(&v, &["repo", "workflow", "branch", "event", "conclusion", "actor", "limit", "cursor"]), Some(&cx.caller)).await).await }),
            op!("ci.runs.get", "A run and its legs: status, conclusion, wait reasons, matrix values, runners.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}}), &["run"]),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::get(&cx.hub, &text(&v, "run")?, &HashMap::new()).await).await }),
            op!("ci.runs.trigger", "Start a workflow by hand (`workflow_dispatch`) at a branch or tag, with its inputs.", ro: false, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "workflow": {"type": "string", "description": "Its path or file name."},
                    "ref": {"type": "string", "description": "A branch or tag; the default branch when left out."}, "inputs": {"type": "object"}
                }), &["repo", "workflow"]),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "dispatch"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let repo = text(&v, "repo")?;
                    let repos = cx.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                    let r = repos.into_iter().find(|r| r.name == repo || r.id == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
                    let git_ref = match v.get("ref").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        Some(x) => x.to_owned(),
                        None => super::events::facts(&cx.hub, &r).await.default_branch,
                    };
                    let inputs = v.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default();
                    super::events::workflow_dispatch(&cx.hub, &r, &git_ref, &text(&v, "workflow")?, &inputs, &by(&cx), cx.caller.started_by()).await.map_err(|e| (e.status, e.message))?;
                    Ok(json!({ "dispatched": true, "ref": git_ref }))
                }),
            op!("ci.runs.ask", "As an agent, ask for a run of a workflow that takes kiln's `agent` event (`on: { agent: { agents, inputs } }`, its `agents` naming you or `*`), at a branch or tag, with its inputs. You are the run's actor.", ro: false, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "workflow": {"type": "string", "description": "Its path or file name."},
                    "ref": {"type": "string", "description": "A branch or tag; the default branch when left out."}, "inputs": {"type": "object"}
                }), &["repo", "workflow"]),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "agent"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let repo = text(&v, "repo")?;
                    let repos = cx.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
                    let r = repos.into_iter().find(|r| r.name == repo || r.id == repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
                    let git_ref = match v.get("ref").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        Some(x) => x.to_owned(),
                        None => super::events::facts(&cx.hub, &r).await.default_branch,
                    };
                    let inputs = v.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default();
                    super::events::agent(&cx.hub, &r, &git_ref, &text(&v, "workflow")?, &inputs, &by(&cx), cx.caller.started_by()).await.map_err(|e| (e.status, e.message))?;
                    Ok(json!({ "asked": true, "ref": git_ref }))
                }),
            op!("ci.runs.compare", "What changed between two runs of a workflow, or since its last green run (`other`: a run's id or `last-green`, the default): the actions, variables, environment, images, machines and cache that differ, the files changed between the commits, and each leg's steps side by side.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}, "other": {"type": "string", "description": "The run to compare with, or `last-green`."}}), &["run"]),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "compare", v.get("other").and_then(Value::as_str).unwrap_or("last-green")]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::compare::runs(&cx.hub, &text(&v, "run")?, v.get("other").and_then(Value::as_str).unwrap_or("last-green")).await).await }),
            op!("ci.runs.cancel", "Cancel a run: its queued legs settle, its running ones are stopped.", ro: false, destructive: true,
                input: || schema(json!({"run": {"type": "string", "description": RUN}}), &["run"]),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "cancel"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::cancel(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.runs.rerun", "A new attempt of a finished run: every job, the failed ones and what needs them, or one job; with debug logging, keeping failed legs to look into.", ro: false, destructive: false,
                input: || schema(json!({
                    "run": {"type": "string", "description": RUN}, "mode": {"type": "string", "enum": ["all", "failed", "job"]},
                    "job": {"type": "string", "description": "With mode `job`."}, "debug": {"type": "boolean"},
                    "keep_failed": {"type": "integer", "description": "Minutes a failed leg is kept to look into (W17-R02)."}
                }), &["run"]),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &v).await).await }),
            op!("ci.runs.approve", "Approve or reject a run's deployments waiting on environments' reviewers.", ro: false, destructive: false,
                input: || schema(json!({
                    "run": {"type": "string", "description": RUN}, "state": {"type": "string", "enum": ["approved", "rejected"]},
                    "environments": {"type": "array", "items": {"type": "string"}, "description": "Which; all the run waits on when left out."}, "comment": {"type": "string"}
                }), &["run", "state"]),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "approve"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let run = text(&v, "run")?; answer(super::environments::approve(&cx.hub, &run, v, Some(&cx.caller)).await).await }),
            op!("ci.legs.get", "A leg: its steps (status, conclusion, outputs), annotations, runner and attempt.", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}}), &["leg"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::leg(&cx.hub, &text(&v, "leg")?).await).await }),
            op!("ci.legs.logs", "A step's log, from byte `from` on: `{ text }`.", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "step": {"type": "integer", "description": "The step's index (`idx` in ci.legs.get)."}, "from": {"type": "integer"}}), &["leg", "step"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs"]), query(v, &["step", "from"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::logs(&cx.hub, &text(&v, "leg")?, &query(&v, &["step", "from"])).await).await }),
            op!("ci.legs.search", "Search one leg's logs (a running one's too) for a string: `{ hits: [{ step, step_name, line, text, at }] }`.", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "q": {"type": "string", "description": "What to find, case-insensitive."}}), &["leg", "q"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs", "search"]), query(v, &["q"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::leg(&cx.hub, &text(&v, "leg")?, "search", &query(&v, &["q"])).await).await }),
            op!("ci.legs.failure_excerpt", "What to read first when a leg failed: its failed step's log from just before the first line that reads like an error (else the step's last lines).", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "lines": {"type": "integer", "description": "How many lines (40 by default)."}}), &["leg"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs", "failure"]), query(v, &["lines"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::leg(&cx.hub, &text(&v, "leg")?, "failure", &query(&v, &["lines"])).await).await }),
            op!("ci.logs.search", "Search step logs across runs for a string of three characters or more, newest first: when an error started, how often it comes. Filter by repository, workflow, branch, conclusion and days back.", ro: true, destructive: false,
                input: || schema(json!({"q": {"type": "string", "description": "What to find, case-insensitive."}, "repo": {"type": "string", "description": REPO}, "workflow": {"type": "string", "description": "The workflow's path."}, "branch": {"type": "string"}, "conclusion": {"type": "string", "description": "The run's: success, failure, cancelled…"}, "days": {"type": "integer"}, "limit": {"type": "integer"}}), &["q"]),
                route: |v| Ok((Method::GET, path(&["logs", "search"]), query(v, &["q", "repo", "workflow", "branch", "conclusion", "days", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::route(&cx.hub, &query(&v, &["q", "repo", "workflow", "branch", "conclusion", "days", "limit"])).await).await }),
            op!("ci.legs.routing", "Why a leg waits, and how each runner measures up against what it needs.", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}}), &["leg"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "routing"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::routing(&cx.hub, &text(&v, "leg")?).await).await }),
            op!("ci.legs.exec", "Run a command in a running leg, or a failed one kept to look into, as its terminal would (its workspace and environment, or its container): `{ output, exit_code, timed_out, truncated }`, masked.", ro: false, destructive: true,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "command": {"type": "string", "description": "Shell text, run by bash."}, "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Seconds; 60 when left out."}}), &["leg", "command"]),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "exec"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::exec(&cx.hub, &text(&v, "leg")?, &v, Some(&cx.caller)).await).await }),
            op!("ci.legs.rerun_step", "Run a kept leg's step again in place, with the environment the leg ended with, as a new section of its log (after fixing what failed with ci.legs.exec).", ro: false, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "step": {"type": "integer", "description": "From 1, as the run page numbers them."}, "idx": {"type": "integer", "description": "Or its index, from 0."}}), &["leg"]),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "rerun-step"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::rerun_step(&cx.hub, &text(&v, "leg")?, &v, &by(&cx)).await).await }),
            op!("ci.legs.keep", "Keep a kept leg longer: `minutes` from now (at most a day).", ro: false, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}, "minutes": {"type": "integer", "minimum": 1, "maximum": 1440}}), &["leg", "minutes"]),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "keep"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::extend(&cx.hub, &text(&v, "leg")?, &v, &by(&cx)).await).await }),
            op!("ci.legs.release", "Let a kept leg go now: its containers stop, its workspace goes.", ro: false, destructive: true,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}}), &["leg"]),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "release"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::release(&cx.hub, &text(&v, "leg")?, &by(&cx)).await).await }),
            op!("ci.workflows.list", "A repository's workflows at a ref, each with whether it runs and its diagnostics.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "ref": {"type": "string"}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "workflows"]), query(v, &["ref"]))),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::list(&cx.hub, &text(&v, "repo")?, &query(&v, &["ref"])).await).await }),
            op!("ci.workflows.get", "A workflow's file at a ref, its model and its diagnostics.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "path": {"type": "string"}, "ref": {"type": "string"}}), &["repo", "path"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "workflows", &text(v, "path")?]), query(v, &["ref"]))),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::get(&cx.hub, &text(&v, "repo")?, &text(&v, "path")?, &query(&v, &["ref"])).await).await }),
            op!("ci.workflows.validate", "Check a workflow's text: diagnostics with codes and positions, before proposing it.", ro: true, destructive: false,
                input: || schema(json!({"path": {"type": "string", "description": "Where it would live, e.g. `.github/workflows/ci.yml`."}, "content": {"type": "string"}, "dialect": {"type": "string", "enum": ["gha", "kiln"]}}), &["content"]),
                route: |_| Ok((Method::POST, path(&["validate"]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(super::workflows::validate_value(v).await).await }),
            op!("ci.workflows.plan_preview", "The plan a workflow would make for an event, without a run: legs, matrix values, routing, what waits on outputs.", ro: true, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "path": {"type": "string"}, "content": {"type": "string", "description": "Text to plan instead of the file at the ref."},
                    "event": {"type": "string"}, "ref": {"type": "string"}, "sha": {"type": "string"}, "payload": {"type": "object"}, "inputs": {"type": "object"}
                }), &["repo", "path"]),
                route: |_| Ok((Method::POST, path(&["plan-preview"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::preview::of(&cx.hub, Some(&cx.caller), v).await).await }),
            op!("ci.runs.rerun_failed", "A new attempt of a finished run with only its failed and cancelled jobs, and the jobs that need them; the rest keep their results.", ro: false, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}, "debug": {"type": "boolean"}, "keep_failed": {"type": "integer", "description": "Minutes a failed leg is kept to look into (W17-R02)."}}), &["run"]),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let mut body = v.clone();
                    if let Some(o) = body.as_object_mut() {
                        o.insert("mode".into(), json!("failed"));
                    }
                    answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &body).await).await
                }),
            op!("ci.runs.rerun_job", "A new attempt of a finished run with one job and the jobs that need it.", ro: false, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}, "job": {"type": "string", "description": "The job's id."}, "debug": {"type": "boolean"}, "keep_failed": {"type": "integer"}}), &["run", "job"]),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let mut body = v.clone();
                    if let Some(o) = body.as_object_mut() {
                        o.insert("mode".into(), json!("job"));
                    }
                    answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &body).await).await
                }),
            op!("ci.artifacts.list", "A run's artifacts: names, sizes, digests, expiry.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}}), &["run"]),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "artifacts"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::part(&cx.hub, &text(&v, "run")?, "artifacts", &HashMap::new()).await).await }),
            op!("ci.tests.run", "A run's test results, failures first: each with its message, place, trace, and flaky and quarantined marks.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}, "attempt": {"type": "string", "description": "An attempt's number, or `all`; the latest when left out."}}), &["run"]),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "tests"]), query(v, &["attempt"]))),
                run: |cx: Cx, v: Value| async move { let run = text(&v, "run")?; answer(super::results::of(&cx.hub, Some(run.as_str()), None, &query(&v, &["attempt"])).await).await }),
            op!("ci.tests.leg", "One leg's test results, failures first.", ro: true, destructive: false,
                input: || schema(json!({"leg": {"type": "string", "description": LEG}}), &["leg"]),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "tests"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let leg = text(&v, "leg")?; answer(super::results::of(&cx.hub, None, Some(leg.as_str()), &HashMap::new()).await).await }),
            op!("ci.tests.repo", "A repository's tests over its recent runs: the most failing, the slowest or the flakiest, with history.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "sort": {"type": "string", "enum": ["failing", "slowest", "flaky"]}, "branch": {"type": "string"}, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "tests"]), query(v, &["sort", "branch", "days"]))),
                run: |cx: Cx, v: Value| async move { answer(super::results::repo(&cx.hub, &text(&v, "repo")?, &query(&v, &["sort", "branch", "days"])).await).await }),
            op!("ci.metrics.run", "A run's benchmark metrics, each against its baseline: the change and its 95% interval, or no clear difference.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}}), &["run"]),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "metrics"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::results::run_metrics(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.metrics.repo", "A metric's values over time in a repository, by runner fingerprint.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "name": {"type": "string", "description": "The metric's name."}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "metrics"]), query(v, &["name"]))),
                run: |cx: Cx, v: Value| async move { answer(super::results::repo_metrics(&cx.hub, &text(&v, "repo")?, &query(&v, &["name"])).await).await }),
            op!("ci.runners.list", "The runners: connected or not, labels, facts, what each is running, slots.", base: "", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["agents"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { Ok(serde_json::to_value(cx.hub.agent_list().await).unwrap_or(Value::Null)) }),
            op!("ci.pools.list", "The executor pools: kind, image, how many executors are starting, idle and busy.", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["pools"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::pools::list(&cx.hub, None).await).await }),
            op!("ci.pools.set", "Make or change a stored pool (its executor, image, labels, how many idle at least and at most), with what the change does now: warnings (no runner can start its executor; a runner carries its labels) and the size it heads for.", ro: false, destructive: false,
                input: || schema(json!({"name": {"type": "string"}, "executor": {"type": "string", "enum": ["docker", "firecracker", "shards", "k8s"]}, "image": {"type": "string"}, "labels": {"type": "array", "items": {"type": "string"}}, "min_idle": {"type": "integer"}, "max": {"type": "integer"}}), &["name", "executor", "labels"]),
                route: |v| Ok((Method::PUT, path(&["pools", &text(v, "name")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::pools::put(&cx.hub, &name, &v).await).await }),
            op!("ci.images.list", "The runner images a repository may use: names, digests, platforms, where built.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["images"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::images::list(&cx.hub, &query(&v, &["repo"])).await).await }),
            op!("ci.images.validate", "Check a runner image's definition as the editor does: a Dockerfile by kiln's parser and lints (hadolint's codes), with each finding's line.", ro: true, destructive: false,
                input: || schema(json!({"content": {"type": "string"}, "path": {"type": "string", "description": "e.g. `.kiln/images/web/Dockerfile`."}, "kind": {"type": "string", "enum": ["dockerfile", "agentfile"]}}), &["content"]),
                route: |_| Ok((Method::POST, path(&["images", "validate"]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(super::images::validate_value(&v).await).await }),
            op!("ci.images.propose", "Propose a runner image's definition for a repository as a diff (`.kiln/images/<name>/Dockerfile` or `Agentfile`), never a direct write: a Dockerfile with errors isn't proposed.", ro: false, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "name": {"type": "string"}, "kind": {"type": "string", "enum": ["dockerfile", "agentfile"]}, "content": {"type": "string"}, "draft": {"type": "boolean"}, "reviewers": {"type": "array", "items": {"type": "string"}}}), &["repo", "name", "content"]),
                route: |v| Ok((Method::POST, path(&["images"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::images::propose(&cx.hub, &query(&v, &["repo"]), &cx.caller, &v).await).await }),
            op!("ci.routing.rules", "The routing rules that send legs to runners (W05-R21), in the order they're tried.", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["routing", "rules"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::routing::get(&cx.hub).await).await }),
            op!("ci.routing.explain", "Where a leg with these needs would go: each runner against its constraints, and the rules that apply.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "runs_on": {"description": "A job's `runs-on`."}, "requires": {"type": "array", "items": {"type": "string"}}}), &[]),
                route: |_| Ok((Method::POST, path(&["routing", "explain"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::routing::explain(&cx.hub, v).await).await }),
            op!("ci.routing.set", "Replace the server's or an organization's routing rules (in the order they're tried), with what the change does now: a warning for each rule no runner connected now meets, and where the last week's legs would go.", ro: false, destructive: false,
                input: || schema(json!({"scope": {"type": "string", "description": "`server` (the default) or `org:<owner>`."}, "rules": {"type": "array", "items": {"type": "object"}, "description": "Each `{ match: { labels, repos, job }, require, executor, image, prefer }`."}}), &["rules"]),
                route: |v| Ok((Method::PUT, path(&["routing", "rules"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let by = by(&cx); answer(super::routing::put(&cx.hub, &query(&v, &["scope"]), v.clone(), &by).await).await }),
            op!("ci.secrets.list", "A scope's secret names, with when each was set and last used (never their values).", ro: true, destructive: false,
                input: || schema(json!({"scope": {"type": "string", "description": SCOPE}}), &["scope"]),
                route: |v| Ok((Method::GET, path(&["secrets"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let scope = scope_of(&cx, &v).await?; Ok(json!({ "scope": text(&v, "scope")?, "secrets": super::secrets::list(&cx.hub, &scope).await.map_err(internal)? })) }),
            op!("ci.secrets.set", "Set a secret in a scope (its value is stored encrypted and never shown again).", ro: false, destructive: true,
                input: || schema(json!({"scope": {"type": "string", "description": SCOPE}, "name": {"type": "string"}, "value": {"type": "string"}}), &["scope", "name", "value"]),
                route: |v| Ok((Method::PUT, path(&["secrets", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    super::secrets::set(&cx.hub, &scope, &text(&v, "name")?, &text(&v, "value")?, &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(json!({ "set": true }))
                }),
            op!("ci.variables.list", "A scope's variables, with their values.", ro: true, destructive: false,
                input: || schema(json!({"scope": {"type": "string", "description": SCOPE}}), &["scope"]),
                route: |v| Ok((Method::GET, path(&["variables"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let scope = scope_of(&cx, &v).await?; Ok(json!({ "scope": text(&v, "scope")?, "variables": super::secrets::list_vars(&cx.hub, &scope).await.map_err(internal)? })) }),
            op!("ci.variables.set", "Set a variable in a scope.", ro: false, destructive: false,
                input: || schema(json!({"scope": {"type": "string", "description": SCOPE}, "name": {"type": "string"}, "value": {"type": "string"}}), &["scope", "name", "value"]),
                route: |v| Ok((Method::PUT, path(&["variables", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    super::secrets::set_var(&cx.hub, &scope, &text(&v, "name")?, &text(&v, "value")?, &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(json!({ "set": true }))
                }),
            op!("ci.integrations.list", "The integrations jobs and deployments use: kind, name, scope, what they're for (never their secrets).", ro: true, destructive: false,
                input: || schema(json!({}), &[]),
                route: |_| Ok((Method::GET, path(&["integrations"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { Ok(json!({ "integrations": super::integrations::list(&cx.hub).await.map_err(internal)? })) }),
            op!("ci.integrations.verify", "Check an integration works: its credentials, or a federated exchange as a repository's job (and environment) would make it.", ro: false, destructive: false,
                input: || schema(json!({"integration": {"type": "string", "description": "Its name."}, "repository": {"type": "string", "description": REPO}, "environment": {"type": "string"}}), &["integration"]),
                route: |v| Ok((Method::POST, path(&["integrations", &text(v, "integration")?, "verify"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let as_: super::integrations::VerifyAs = serde_json::from_value(json!({ "repository": v.get("repository"), "environment": v.get("environment") })).unwrap_or_default();
                    let r = Box::pin(super::integrations::verify(&cx.hub, &text(&v, "integration")?, &as_)).await.map_err(|e| (StatusCode::NOT_FOUND, format!("{e:#}")))?;
                    Ok(match r {
                        Ok(message) => json!({ "ok": true, "message": message }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    })
                }),
            op!("ci.environments.list", "A repository's environments: protection (reviewers, wait timer, branches, windows), concurrency limit, URL, latest deployment.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["environments"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::environments::route(&cx.hub, &Method::GET, &[], &query(&v, &["repo"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.save", "Make an environment in kiln, or change one made here: its protection, URL and concurrency limit (`.kiln/environments.yaml`'s are changed by a diff).", ro: false, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "name": {"type": "string"},
                    "protection": {"type": "object", "description": "`reviewers`, `prevent-self-review`, `wait-timer`, `branches`, `tags`, `windows`."},
                    "url": {"type": "string"}, "concurrency": {"type": "integer", "minimum": 1}
                }), &["repo", "name"]),
                route: |v| Ok((Method::PUT, path(&["environments", &text(v, "name")?]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::PUT, &[name.as_str()], &query(&v, &["repo"]), v.clone(), Some(&cx.caller)).await).await }),
            op!("ci.insights.repos", "Insights (W19-R01): every repository's runs, failures, durations and costs; or one repository's, by workflow.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &[]),
                route: |v| Ok((Method::GET, match v.get("repo").and_then(Value::as_str) { Some(r) => path(&["insights", "repos", r]), None => path(&["insights", "repos"]) }, query(v, &["days"]))),
                run: |cx: Cx, v: Value| async move {
                    let repo = v.get("repo").and_then(Value::as_str).map(str::to_owned);
                    let rest: Vec<&str> = match &repo { Some(r) => vec!["repos", r.as_str()], None => vec!["repos"] };
                    answer(super::insights::route(&cx.hub, &rest, &query(&v, &["days"])).await).await
                }),
            op!("ci.insights.workflows", "A workflow's insights (`workflow`, its path; `ci.insights.repos` with `repo` lists them): durations and success over time, the critical path, its jobs, slowest steps, executors and pools, cache hit rates, queue time, flakiness.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "workflow": {"type": "string", "description": "Its path, e.g. `.github/workflows/ci.yml`."}, "days": {"type": "integer", "minimum": 1, "maximum": 365}}), &["repo", "workflow"]),
                route: |v| Ok((Method::GET, path(&["insights", "workflows", &text(v, "repo")?]), query(v, &["workflow", "days"]))),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; answer(super::insights::route(&cx.hub, &["workflows", repo.as_str()], &query(&v, &["workflow", "days"])).await).await }),
            op!("ci.insights.runners", "Runners' insights: utilization, queue time and failures by runner.", ro: true, destructive: false,
                input: || schema(json!({"days": {"type": "integer", "minimum": 1, "maximum": 365}}), &[]),
                route: |v| Ok((Method::GET, path(&["insights", "runners"]), query(v, &["days"]))),
                run: |cx: Cx, v: Value| async move { answer(super::insights::route(&cx.hub, &["runners"], &query(&v, &["days"])).await).await }),
            op!("ci.diffs.list", "A repository's diffs (W14): `open` (the default), a state, or `all`; by author.", base: "", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "state": {"type": "string"}, "author": {"type": "string"}, "limit": {"type": "integer"}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "diffs"]), query(v, &["state", "author", "limit"]))),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; diffs(&cx, Method::GET, &["repos", repo.as_str(), "diffs"], &query(&v, &["state", "author", "limit"]), &Value::Null).await }),
            op!("ci.diffs.get", "A diff: its versions, state, reviewers, stack and runs.", base: "", ro: true, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}}), &["diff"]),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str()], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.threads", "A diff's comment threads: where each is (version, file, side, lines), its comments, and whether it's resolved.", base: "", ro: true, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}}), &["diff"]),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "comments"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.readiness", "What keeps a diff from landing (reviews, checks, policy), or that it's ready.", base: "", ro: true, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}}), &["diff"]),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "land"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "land"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.comment", "Comment on a diff: on the whole, or on a file's lines (`file`, `line` or `start_line`/`end_line`, `side`), with an optional suggestion; a draft until published.", base: "", ro: false, destructive: false,
                input: || schema(json!({
                    "diff": {"type": "string", "description": DIFF}, "body": {"type": "string"}, "file": {"type": "string"}, "line": {"type": "integer"},
                    "start_line": {"type": "integer"}, "end_line": {"type": "integer"}, "side": {"type": "string", "enum": ["new", "old"]},
                    "suggestion": {"type": "string", "description": "Replacement text for the lines."}, "version": {"type": "integer"}, "publish": {"type": "boolean"}
                }), &["diff", "body"]),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "comments"], &HashMap::new(), &v).await }),
            op!("ci.diffs.reply", "Reply in a diff's thread.", base: "", ro: false, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}, "thread": {"type": "string"}, "body": {"type": "string"}, "publish": {"type": "boolean"}}), &["diff", "thread", "body"]),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "comments"], &HashMap::new(), &v).await }),
            op!("ci.diffs.resolve", "Resolve a comment's thread, or open it again.", base: "", ro: false, destructive: false,
                input: || schema(json!({"comment": {"type": "string", "description": "The thread's first comment's id."}, "state": {"type": "string", "enum": ["resolved", "open"]}}), &["comment"]),
                route: |v| Ok((Method::PATCH, path(&["comments", &text(v, "comment")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let c = text(&v, "comment")?;
                    let state = v.get("state").and_then(Value::as_str).unwrap_or("resolved");
                    diffs(&cx, Method::PATCH, &["comments", c.as_str()], &HashMap::new(), &json!({ "state": state })).await
                }),
            op!("ci.diffs.review", "Review a diff: accept, request changes, comment, or resign; publishes your draft comments.", base: "", ro: false, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}, "action": {"type": "string", "enum": ["accept", "request_changes", "comment", "resign"]}, "body": {"type": "string"}}), &["diff", "action"]),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "review"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "review"], &HashMap::new(), &v).await }),
            op!("ci.diffs.submit_patch", "Submit commits as diffs without git: patches (`git format-patch`'s mbox, or a list) onto the trunk, each a diff or a new version of its diff (by Change-Id).", base: "", ro: false, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}, "mbox": {"type": "string"}, "patches": {"type": "array"}, "trunk": {"type": "string"}}), &["repo"]),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "submit"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; diffs(&cx, Method::POST, &["repos", repo.as_str(), "submit"], &HashMap::new(), &v).await }),
            op!("ci.diffs.land", "Land a diff (with the diffs below it) through the land queue, by a strategy: `stack`, `stack-squash` or `ff-only` (the trunk's default when left out).", base: "", ro: false, destructive: false,
                input: || schema(json!({"diff": {"type": "string", "description": DIFF}, "strategy": {"type": "string", "enum": ["stack", "stack-squash", "ff-only"]}}), &["diff"]),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "land"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "land"], &HashMap::new(), &v).await }),
            op!("ci.workflows.edit", "Propose a workflow's new text as a diff, never a direct write: checked first (refused with its errors), then a diff on the trunk that lands through review. Validate and preview it before, and watch its run after.", base: "", ro: false, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "path": {"type": "string", "description": "The workflow's path, e.g. `.github/workflows/ci.yml`."},
                    "content": {"type": "string", "description": "Its whole new text."}, "title": {"type": "string", "description": "The diff's title (`Edit <path>` when left out)."},
                    "draft": {"type": "boolean"}, "reviewers": {"type": "array", "items": {"type": "string"}}, "topic": {"type": "string"}
                }), &["repo", "path", "content"]),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "submit"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let (repo, file, content) = (text(&v, "repo")?, text(&v, "path")?, text(&v, "content")?);
                    let (f, c) = (file.clone(), content.clone());
                    let checked = crate::rt::blocking(move || kiln_ci::wf::check(&f, &c, None)).await.map_err(internal)?;
                    let errors: Vec<String> = checked.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(&file)).collect();
                    if !errors.is_empty() {
                        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{file} has errors, so it isn't proposed:\n{}", errors.join("\n"))));
                    }
                    let title = v.get("title").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map_or_else(|| format!("Edit {file}"), str::to_owned);
                    let shown = content.clone();
                    let mut out = super::diffs::propose_file(&cx.hub, &repo, &cx.caller, &file, &title, &v, move |_| Ok(content)).await?;
                    // W20-R03: its effect too, as a push to the trunk plans it once it lands.
                    let preview = answer(super::preview::of(&cx.hub, Some(&cx.caller), json!({ "repo": repo, "path": file, "content": shown, "event": "push" })).await).await.unwrap_or(Value::Null);
                    if let Some(o) = out.as_object_mut() {
                        o.insert("diagnostics".into(), serde_json::to_value(&checked.diagnostics).unwrap_or(Value::Null));
                        o.insert("preview".into(), preview);
                    }
                    Ok(out)
                }),
            op!("ci.oidc.token", "Your own workload token, as the agent kiln started (W10-R11): `sub` kiln:agent:<session>, acting for whoever started you, for `audience` (a model provider, a secret store, a cloud's STS). Federate with it instead of a stored key.", ro: false, destructive: false,
                input: || schema(json!({"audience": {"type": "string", "description": "Who it's for, as their trust policy names it."}, "ttl": {"type": "integer", "minimum": 60, "maximum": 3600, "description": "Seconds it lasts (600 by default)."}}), &["audience"]),
                route: |_| Ok((Method::POST, path(&["oidc", "token"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::oidc::agent_token(&cx.hub, &cx.caller, &v).await }),
            op!("ci.notifications.deliveries", "What notification rules sent (W19-R06), newest first: each delivery's rule, event, channel, whether it went and why not, and its attempts.", ro: true, destructive: false,
                input: || schema(json!({"run": {"type": "string", "description": RUN}, "limit": {"type": "integer", "minimum": 1, "maximum": 1000}}), &[]),
                route: |v| Ok((Method::GET, path(&["notify", "deliveries"]), query(v, &["run", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::notify::deliveries(&cx.hub, &query(&v, &["run", "limit"])).await).await }),
            op!("ci.tests.propose_quarantine", "Propose quarantining a test (W16-R05) as a diff adding it to the trunk's `.kiln/quarantine.yaml`: under `quarantine: enforce`, its failures alone then let a step pass, with a warning. It lands through review.", base: "", ro: false, destructive: false,
                input: || schema(json!({
                    "repo": {"type": "string", "description": REPO}, "test": {"type": "string", "description": "suite.name, suite::name, the name, or the id; `*` matches any run of characters."},
                    "reason": {"type": "string"}, "issue": {"type": "string", "description": "Where it's tracked."}, "until": {"type": "string", "description": "The last day it applies, YYYY-MM-DD."},
                    "draft": {"type": "boolean"}, "reviewers": {"type": "array", "items": {"type": "string"}}
                }), &["repo", "test", "reason"]),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "submit"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let (repo, test, reason) = (text(&v, "repo")?, text(&v, "test")?, text(&v, "reason")?);
                    let entry = kiln_ci::results::quarantine::Entry { test: test.clone(), reason, issue: v.get("issue").and_then(Value::as_str).map(str::to_owned), until: v.get("until").and_then(Value::as_str).map(str::to_owned) };
                    let title = format!("Quarantine {test}");
                    super::diffs::propose_file(&cx.hub, &repo, &cx.caller, ".kiln/quarantine.yaml", &title, &v, move |old| {
                        let mut text = old.unwrap_or_default();
                        let had = kiln_ci::results::quarantine::parse(&text)?;
                        if had.iter().any(|e| e.test == entry.test) {
                            return Err(format!("`{}` is in .kiln/quarantine.yaml already", entry.test));
                        }
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        // As people write it: a scalar quoted only when YAML needs it, and no
                        // empty keys.
                        let scalar = |x: &str| serde_yaml::to_string(x).map(|y| y.trim_end().to_owned()).map_err(|e| e.to_string());
                        text.push_str(&format!("- test: {}\n  reason: {}\n", scalar(&entry.test)?, scalar(&entry.reason)?));
                        for (k, x) in [("issue", &entry.issue), ("until", &entry.until)] {
                            if let Some(x) = x {
                                text.push_str(&format!("  {k}: {}\n", scalar(x)?));
                            }
                        }
                        kiln_ci::results::quarantine::parse(&text)?;
                        Ok(text)
                    })
                    .await
                }),
            op!("ci.caches.list", "A repository's caches: keys, sizes, scopes, last use.", ro: true, destructive: false,
                input: || schema(json!({"repo": {"type": "string", "description": REPO}}), &["repo"]),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "caches"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::caches(&cx.hub, &text(&v, "repo")?, None).await).await }),
        ]
    })
}

/// Runs `name` for `cx`'s caller, when the rule of the route it is lets the caller.
pub async fn call(cx: Cx, name: &str, input: Value) -> Out {
    let op = ops().iter().find(|o| o.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no operation {name}")))?;
    let (method, rest, q) = (op.route)(&input)?;
    // The route's segments as the HTTP layer has them once decoded: a repository's name
    // (`acme/app`) is one segment, as `acme%2Fapp` is in the path.
    let mut segs: Vec<&str> = vec!["api"];
    if !op.base.is_empty() {
        segs.push(op.base);
    }
    segs.extend(rest.iter().map(String::as_str));
    let path = format!("/{}", segs.iter().map(|s| s.replace('/', "%2F")).collect::<Vec<_>>().join("/"));
    let run = async {
        crate::authz::gate::check(&cx.hub, &cx.caller, &method, &segs, &q).await?;
        (op.run)(cx.clone(), input).await
    };
    if op.read_only {
        return run.await;
    }
    // A change is audited as its route's would be (W19-R05): one row.
    crate::audit::request(&cx.hub, crate::audit::Req { caller: &cx.caller, method: &method, path: &path, q: &q, ip: None }, run, |r: &Out| match r {
        Ok(_) => 200,
        Err((s, _)) => s.as_u16(),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operation is a route of the table, with a capability of its own (W20-R01).
    #[test]
    fn w20_r01_each_operation_is_a_route() {
        let probe = json!({"run": "r1", "leg": "l1", "repo": "acme/app", "path": ".github/workflows/ci.yml", "step": 0, "command": "true", "state": "approved", "minutes": 5, "content": "x", "workflow": "ci.yml", "q": "error", "job": "build", "scope": "repo:acme/app", "name": "N", "value": "v", "integration": "hook", "diff": "D1", "thread": "t1", "body": "b", "comment": "c1", "action": "accept"});
        for op in ops() {
            let route = (op.route)(&probe);
            assert!(route.is_ok(), "{}", op.name);
            let Ok((method, rest, _)) = route else { continue };
            let mut segs = vec!["api"];
            if !op.base.is_empty() {
                segs.push(op.base);
            }
            segs.extend(rest.iter().map(String::as_str));
            let rule = crate::authz::rule(&method, &segs);
            assert_ne!(rule.pattern, crate::authz::UNNAMED.pattern, "{} ({method} /{}) has a rule of its own", op.name, segs.join("/"));
            assert_eq!(op.read_only, method == Method::GET || rule.capability == "runs.read", "{}: read-only exactly when its route reads", op.name);
            assert!((op.input)().get("type").is_some(), "{}", op.name);
        }
    }
}
