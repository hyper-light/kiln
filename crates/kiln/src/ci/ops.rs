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
    /// The route it is: method, path under `/api/ci`, query.
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
        Op { name: $name, summary: $summary, read_only: $ro, destructive: $d, input: $input, route: $route, run: |cx, v| Box::pin($run(cx, v)) }
    };
}

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
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::list(&cx.hub, &query(&v, &["repo", "workflow", "branch", "event", "conclusion", "actor", "limit", "cursor"])).await).await }),
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
                    super::events::workflow_dispatch(&cx.hub, &r, &git_ref, &text(&v, "workflow")?, &inputs, &by(&cx)).await.map_err(|e| (e.status, e.message))?;
                    Ok(json!({ "dispatched": true, "ref": git_ref }))
                }),
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
    let mut segs: Vec<&str> = vec!["api", "ci"];
    segs.extend(rest.iter().map(String::as_str));
    crate::authz::gate::check(&cx.hub, &cx.caller, &method, &segs, &q).await?;
    (op.run)(cx, input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operation is a route of the table, with a capability of its own (W20-R01).
    #[test]
    fn w20_r01_each_operation_is_a_route() {
        let probe = json!({"run": "r1", "leg": "l1", "repo": "acme/app", "path": ".github/workflows/ci.yml", "step": 0, "command": "true", "state": "approved", "minutes": 5, "content": "x", "workflow": "ci.yml"});
        for op in ops() {
            let route = (op.route)(&probe);
            assert!(route.is_ok(), "{}", op.name);
            let Ok((method, rest, _)) = route else { continue };
            let mut segs = vec!["api", "ci"];
            segs.extend(rest.iter().map(String::as_str));
            let rule = crate::authz::rule(&method, &segs);
            assert_ne!(rule.pattern, crate::authz::UNNAMED.pattern, "{} ({method} /{}) has a rule of its own", op.name, segs.join("/"));
            assert_eq!(op.read_only, method == Method::GET || rule.capability == "runs.read", "{}: read-only exactly when its route reads", op.name);
            assert!((op.input)().get("type").is_some(), "{}", op.name);
        }
    }
}
