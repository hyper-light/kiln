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
use crate::ops::{Cx, Fail, Input, Out};

type Fut = Pin<Box<dyn Future<Output = Out> + Send>>;

/// One operation.
pub struct Op {
    /// Its name, dotted (`ci.runs.list`).
    pub name: &'static str,
    pub summary: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub input: Input,
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

/// A handler's response as an operation's answer.
pub(crate) async fn answer(r: Result<Response, Fail>) -> Out {
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

/// A route of the host's pull requests (W23-R06), as its HTTP route answers.
async fn pulls(cx: &Cx, method: Method, repo: &str, tail: &[&str], body: Value) -> Out {
    answer(crate::githost::pulls::route(&cx.hub, &method, repo, tail, body, Some(&cx.caller)).await).await
}

/// A pull request's number, as its route's segment.
fn number(v: &Value) -> Result<String, Fail> {
    v.get("number").and_then(Value::as_u64).map(|n| n.to_string()).ok_or_else(|| bad("`number` is required"))
}

/// A diff's version, as its route's segment.
fn version(v: &Value) -> Result<String, Fail> {
    v.get("version").and_then(Value::as_u64).map(|n| n.to_string()).ok_or_else(|| bad("`version` is required"))
}

/// Where to download one of a run's artifacts: the signed URL kiln's own download redirects to,
/// or, for another provider's run, kiln's download route itself.
async fn artifact_link(cx: &Cx, run: &str, artifact: &str) -> Out {
    let r = super::runs_api::download(&cx.hub, run, artifact).await?;
    if let Some(url) = r.headers().get(http::header::LOCATION).and_then(|l| l.to_str().ok()) {
        return Ok(json!({ "url": url, "expires_in": 60 }));
    }
    let base = cx.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    Ok(json!({ "url": format!("{base}/api/ci/runs/{run}/artifacts/{artifact}/download"), "with_token": true }))
}

/// A secret or variable scope (`repo:acme/app`, `env:acme/app:production`, …).
async fn scope_of(cx: &Cx, v: &Value) -> Result<super::secrets::Scope, Fail> {
    super::secrets::parse_scope(&cx.hub, &text(v, "scope")?).await.map_err(bad)
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
                input: Input::of::<input::CiRunsList>(),
                route: |v| Ok((Method::GET, path(&["runs"]), query(v, &["repo", "workflow", "branch", "event", "conclusion", "actor", "limit", "cursor"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::list(&cx.hub, &query(&v, &["repo", "workflow", "branch", "event", "conclusion", "actor", "limit", "cursor"]), Some(&cx.caller)).await).await }),
            op!("ci.runs.get", "A run and its legs: status, conclusion, wait reasons, matrix values, runners.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsGet>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::get(&cx.hub, &text(&v, "run")?, &HashMap::new()).await).await }),
            op!("ci.runs.trigger", "Start a workflow by hand (`workflow_dispatch`) at a branch or tag, with its inputs.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsTrigger>(),
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
                input: Input::of::<input::CiRunsAsk>(),
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
            op!("ci.runs.flow", "A run's data flow (W04-R23): each job output, from the step that made it to the jobs that read it (where in each, and their legs), with each leg's value (never a secret's); and each job's exported environment and the jobs that take it.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsFlow>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "flow"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::flow(&cx.hub, &text(&v, "run")?, &HashMap::new()).await).await }),
            op!("ci.runs.compare", "What changed between two runs of a workflow, or since its last green run (`other`: a run's id or `last-green`, the default): the actions, variables, environment, images, machines and cache that differ, the files changed between the commits, and each leg's steps side by side.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsCompare>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "compare", v.get("other").and_then(Value::as_str).unwrap_or("last-green")]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::compare::runs(&cx.hub, &text(&v, "run")?, v.get("other").and_then(Value::as_str).unwrap_or("last-green")).await).await }),
            op!("ci.runs.cancel", "Cancel a run: its queued legs settle, its running ones are stopped.", ro: false, destructive: true,
                input: Input::of::<input::CiRunsCancel>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "cancel"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::cancel(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.runs.rerun", "A new attempt of a finished run: every job, the failed ones and what needs them, or one job; with debug logging, keeping failed legs to look into.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsRerun>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &v, &cx.caller.name).await).await }),
            op!("ci.runs.approve", "Approve or reject a run's deployments waiting on environments' reviewers.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsApprove>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "approve"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let run = text(&v, "run")?; answer(super::environments::approve(&cx.hub, &run, v, Some(&cx.caller)).await).await }),
            op!("ci.legs.get", "A leg: its steps (status, conclusion, outputs), annotations, runner and attempt.", ro: true, destructive: false,
                input: Input::of::<input::CiLegsGet>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::leg(&cx.hub, &text(&v, "leg")?).await).await }),
            op!("ci.legs.logs", "A step's log, from byte `from` on: `{ text }`.", ro: true, destructive: false,
                input: Input::of::<input::CiLegsLogs>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs"]), query(v, &["step", "from"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::logs(&cx.hub, &text(&v, "leg")?, &query(&v, &["step", "from"])).await).await }),
            op!("ci.legs.search", "Search one leg's logs (a running one's too) for a string: `{ hits: [{ step, step_name, line, text, at }] }`.", ro: true, destructive: false,
                input: Input::of::<input::CiLegsSearch>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs", "search"]), query(v, &["q"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::leg(&cx.hub, &text(&v, "leg")?, "search", &query(&v, &["q"])).await).await }),
            op!("ci.legs.failure_excerpt", "What to read first when a leg failed: its failed step's log from just before the first line that reads like an error (else the step's last lines).", ro: true, destructive: false,
                input: Input::of::<input::CiLegsFailureExcerpt>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "logs", "failure"]), query(v, &["lines"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::leg(&cx.hub, &text(&v, "leg")?, "failure", &query(&v, &["lines"])).await).await }),
            op!("ci.logs.search", "Search step logs across runs for a string of three characters or more, newest first: when an error started, how often it comes. Filter by repository, workflow, branch, conclusion and days back.", ro: true, destructive: false,
                input: Input::of::<input::CiLogsSearch>(),
                route: |v| Ok((Method::GET, path(&["logs", "search"]), query(v, &["q", "repo", "workflow", "branch", "conclusion", "days", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::logsearch::route(&cx.hub, &query(&v, &["q", "repo", "workflow", "branch", "conclusion", "days", "limit"])).await).await }),
            op!("ci.legs.routing", "Why a leg waits, and how each runner measures up against what it needs.", ro: true, destructive: false,
                input: Input::of::<input::CiLegsRouting>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "routing"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::routing(&cx.hub, &text(&v, "leg")?).await).await }),
            op!("ci.legs.exec", "Run a command in a running leg, or a failed one kept to look into, as its terminal would (its workspace and environment, or its container): `{ output, exit_code, timed_out, truncated }`, masked.", ro: false, destructive: true,
                input: Input::of::<input::CiLegsExec>(),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "exec"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::exec(&cx.hub, &text(&v, "leg")?, &v, Some(&cx.caller)).await).await }),
            op!("ci.legs.rerun_step", "Run a kept leg's step again in place, with the environment the leg ended with, as a new section of its log (after fixing what failed with ci.legs.exec).", ro: false, destructive: false,
                input: Input::of::<input::CiLegsRerunStep>(),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "rerun-step"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::rerun_step(&cx.hub, &text(&v, "leg")?, &v, &by(&cx)).await).await }),
            op!("ci.legs.keep", "Keep a kept leg longer: `minutes` from now (at most a day).", ro: false, destructive: false,
                input: Input::of::<input::CiLegsKeep>(),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "keep"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::extend(&cx.hub, &text(&v, "leg")?, &v, &by(&cx)).await).await }),
            op!("ci.legs.release", "Let a kept leg go now: its containers stop, its workspace goes.", ro: false, destructive: true,
                input: Input::of::<input::CiLegsRelease>(),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "release"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::release(&cx.hub, &text(&v, "leg")?, &by(&cx)).await).await }),
            op!("ci.legs.runners", "Where a finished leg, or a `.kiln/` pipeline's job, could run again (W05-R28): every runner of its own, able to run it or why not; the one its last attempt ran on (`ran_on`) and whether that one can be avoided (`elsewhere`); and whether it can be retried now (`retry`).", ro: true, destructive: false,
                input: Input::of::<input::CiLegsRunners>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "runners"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::retry::runners(&cx.hub, &text(&v, "leg")?).await).await }),
            op!("ci.legs.retry", "Run a finished leg, or a `.kiln/` pipeline's job, again (W05-R28): on any runner that can run it, on any but the one its last attempt ran on (`elsewhere`), or on one you name that can (`runner`, from ci.legs.runners). A leg of the CI engine is a new attempt of its run (the leg, and the jobs that need it); a pipeline's job runs again in its run, with the jobs that need it; another provider's job is rerun by that provider.", ro: false, destructive: false,
                input: Input::of::<input::CiLegsRetry>(),
                route: |v| Ok((Method::POST, path(&["legs", &text(v, "leg")?, "retry"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::retry::retry(&cx.hub, &text(&v, "leg")?, &v, &by(&cx)).await).await }),
            op!("ci.workflows.list", "A repository's workflows at a ref, each with whether it runs and its diagnostics.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "workflows"]), query(v, &["ref"]))),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::list(&cx.hub, &text(&v, "repo")?, &query(&v, &["ref"])).await).await }),
            op!("ci.workflows.get", "A workflow's file at a ref, its model and its diagnostics.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsGet>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "workflows", &text(v, "path")?]), query(v, &["ref"]))),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::get(&cx.hub, &text(&v, "repo")?, &text(&v, "path")?, &query(&v, &["ref"])).await).await }),
            op!("ci.images.build", "Build a runner image's definition as you have it, saved or not (W21-R08): `kiln/build-image@v1` on what `runs_on` names, from `.kiln/images/<name>/`. With `push`, pushed to kiln's registry and registered: the server owner's only. Watch the run and read its log after.", ro: false, destructive: false,
                input: Input::of::<input::CiImagesBuild>(),
                route: |v| Ok((Method::POST, path(&["images", "build"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let q = query(&v, &["repo"]); super::uploads::build(&cx.hub, Some(&cx.caller), &q, v).await }),
            op!("ci.actions.try", "Try an action as you have it, saved or not (W21-R07): it runs once with `inputs` on what `runs_on` names (a label or a runner), its outputs the job's. Watch the run and read its log after.", ro: false, destructive: false,
                input: Input::of::<input::CiActionsTry>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "actions", "try"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; super::uploads::try_action(&cx.hub, Some(&cx.caller), &repo, v).await }),
            op!("ci.workflows.labels", "What a workflow's `runs-on` may name in a repository: its runners' labels (and GitHub's for self-hosted runners), its pools' and the routing rules', each with where it comes from. A label none has waits for one.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsLabels>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "labels"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::labels(&cx.hub, &text(&v, "repo")?).await).await }),
            op!("ci.workflows.validate", "Check a workflow's text: diagnostics with codes and positions, before proposing it. With `repo`, against what its run would meet too: labels no runner, rule or pool has (KCI6007), actions its policies refuse.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsValidate>(),
                route: |_| Ok((Method::POST, path(&["validate"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::workflows::validate_value(&cx.hub, Some(&cx.caller), v).await).await }),
            op!("ci.workflows.plan_preview", "The plan a workflow would make for an event, without a run: legs, matrix values, routing, what waits on outputs.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsPlanPreview>(),
                route: |_| Ok((Method::POST, path(&["plan-preview"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::preview::of(&cx.hub, Some(&cx.caller), v).await).await }),
            op!("ci.runs.rerun_failed", "A new attempt of a finished run with only its failed and cancelled jobs, and the jobs that need them; the rest keep their results.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsRerunFailed>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let mut body = v.clone();
                    if let Some(o) = body.as_object_mut() {
                        o.insert("mode".into(), json!("failed"));
                    }
                    answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &body, &cx.caller.name).await).await
                }),
            op!("ci.runs.rerun_job", "A new attempt of a finished run with one job and the jobs that need it.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsRerunJob>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "rerun"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let mut body = v.clone();
                    if let Some(o) = body.as_object_mut() {
                        o.insert("mode".into(), json!("job"));
                    }
                    answer(super::runs_api::rerun(&cx.hub, &text(&v, "run")?, &body, &cx.caller.name).await).await
                }),
            op!("ci.artifacts.list", "A run's artifacts: names, sizes, digests, expiry.", ro: true, destructive: false,
                input: Input::of::<input::CiArtifactsList>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "artifacts"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::part(&cx.hub, &text(&v, "run")?, "artifacts", &HashMap::new()).await).await }),
            op!("ci.tests.run", "A run's test results, failures first: each with its message, place, trace, and flaky and quarantined marks.", ro: true, destructive: false,
                input: Input::of::<input::CiTestsRun>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "tests"]), query(v, &["attempt"]))),
                run: |cx: Cx, v: Value| async move { let run = text(&v, "run")?; answer(super::results::of(&cx.hub, Some(run.as_str()), None, &query(&v, &["attempt"])).await).await }),
            op!("ci.tests.leg", "One leg's test results, failures first.", ro: true, destructive: false,
                input: Input::of::<input::CiTestsLeg>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "tests"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let leg = text(&v, "leg")?; answer(super::results::of(&cx.hub, None, Some(leg.as_str()), &HashMap::new()).await).await }),
            op!("ci.tests.repo", "A repository's tests over its recent runs: the most failing, the slowest or the flakiest, with history.", ro: true, destructive: false,
                input: Input::of::<input::CiTestsRepo>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "tests"]), query(v, &["sort", "branch", "days"]))),
                run: |cx: Cx, v: Value| async move { answer(super::results::repo(&cx.hub, &text(&v, "repo")?, &query(&v, &["sort", "branch", "days"])).await).await }),
            op!("ci.metrics.run", "A run's benchmark metrics, each against its baseline: the change and its 95% interval, or no clear difference.", ro: true, destructive: false,
                input: Input::of::<input::CiMetricsRun>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "metrics"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::results::run_metrics(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.metrics.repo", "A metric's values over time in a repository, by runner fingerprint.", ro: true, destructive: false,
                input: Input::of::<input::CiMetricsRepo>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "metrics"]), query(v, &["name"]))),
                run: |cx: Cx, v: Value| async move { answer(super::results::repo_metrics(&cx.hub, &text(&v, "repo")?, &query(&v, &["name"])).await).await }),
            op!("ci.runners.list", "The runners: connected or not, labels, facts, what each is running, slots.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiRunnersList>(),
                route: |_| Ok((Method::GET, path(&["agents"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { Ok(serde_json::to_value(cx.hub.agent_list().await).unwrap_or(Value::Null)) }),
            op!("ci.pools.list", "The executor pools: kind, image, how many executors are starting, idle and busy.", ro: true, destructive: false,
                input: Input::of::<input::CiPoolsList>(),
                route: |_| Ok((Method::GET, path(&["pools"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::pools::list(&cx.hub, None).await).await }),
            op!("ci.pools.history", "A pool's scaling history (W21-R08): its executors starting, idle and busy, and the legs waiting for it, a minute apart, over the last `hours` (24 by default, a week at most).", ro: true, destructive: false,
                input: Input::of::<input::CiPoolsHistory>(),
                route: |v| Ok((Method::GET, path(&["pools", &text(v, "name")?, "history"]), query(v, &["hours"]))),
                run: |cx: Cx, v: Value| async move { answer(super::pools::history(&cx.hub, &text(&v, "name")?, &query(&v, &["hours"])).await).await }),
            op!("ci.pools.set", "Make or change a stored pool (its executor, image, labels, how many idle at least and at most), with what the change does now: warnings (no runner can start its executor; a runner carries its labels) and the size it heads for.", ro: false, destructive: false,
                input: Input::of::<input::CiPoolsSet>(),
                route: |v| Ok((Method::PUT, path(&["pools", &text(v, "name")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::pools::put(&cx.hub, &name, &v).await).await }),
            op!("ci.images.list", "The runner images a repository may use: names, digests, platforms, where built.", ro: true, destructive: false,
                input: Input::of::<input::CiImagesList>(),
                route: |v| Ok((Method::GET, path(&["images"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::images::list(&cx.hub, &query(&v, &["repo"])).await).await }),
            op!("ci.registry.mirrors", "The registries kiln's registry pulls through, and what a machine is given to pull through it: Docker's `registry-mirrors`, and containerd's `hosts.toml` for each (a cluster's nodes).", ro: true, destructive: false,
                input: Input::of::<input::CiRegistryMirrors>(),
                route: |_| Ok((Method::GET, path(&["registry", "mirrors"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::registry::mirror::mirrors(&cx.hub).await).await }),
            op!("ci.images.validate", "Check a runner image's definition as the editor does: a Dockerfile by kiln's parser and lints (hadolint's codes), with each finding's line.", ro: true, destructive: false,
                input: Input::of::<input::CiImagesValidate>(),
                route: |_| Ok((Method::POST, path(&["images", "validate"]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(super::images::validate_value(&v).await).await }),
            op!("ci.images.propose", "Propose a runner image's definition for a repository as a diff (`.kiln/images/<name>/Dockerfile` or `Agentfile`), never a direct write: a Dockerfile with errors isn't proposed.", ro: false, destructive: false,
                input: Input::of::<input::CiImagesPropose>(),
                route: |v| Ok((Method::POST, path(&["images"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::images::propose(&cx.hub, &query(&v, &["repo"]), &cx.caller, &v).await).await }),
            op!("ci.routing.rules", "The routing rules that send legs to runners (W05-R21), in the order they're tried.", ro: true, destructive: false,
                input: Input::of::<input::CiRoutingRules>(),
                route: |_| Ok((Method::GET, path(&["routing", "rules"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::routing::get(&cx.hub).await).await }),
            op!("ci.routing.explain", "Where a leg with these needs would go: each runner against its constraints, and the rules that apply.", ro: true, destructive: false,
                input: Input::of::<input::CiRoutingExplain>(),
                route: |_| Ok((Method::POST, path(&["routing", "explain"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::routing::explain(&cx.hub, v).await).await }),
            op!("ci.routing.set", "Replace the server's or an organization's routing rules (in the order they're tried), with what the change does now: a warning for each rule no runner connected now meets, and where the last week's legs would go.", ro: false, destructive: false,
                input: Input::of::<input::CiRoutingSet>(),
                route: |v| Ok((Method::PUT, path(&["routing", "rules"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let by = by(&cx); answer(super::routing::put(&cx.hub, &query(&v, &["scope"]), v.clone(), &by).await).await }),
            op!("ci.work.reap.policies", "The reaper's policies (W24-R31) set for the system and, when named, a runner, a repository and a session, and the one in force for them together, each value with the scope it came from.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkReapPolicies>(),
                route: |v| Ok((Method::GET, path(&["work", "reap", "policies"]), query(v, &["runner", "repo", "session", "volume"]))),
                run: |cx: Cx, v: Value| async move { answer(super::reap::get(&cx.hub, &query(&v, &["runner", "repo", "session", "volume"])).await).await }),
            op!("ci.work.reap.set", "Set the reaper's policy (W24-R31) for the system, a runner, a repository or a session: floor, ceiling and target (the system's and a runner's only), keep_last, keep_within, max_age and schedule; `{}` removes it. Answers the policy in force.", ro: false, destructive: false,
                input: Input::of::<input::CiWorkReapSet>(),
                route: |v| Ok((Method::PUT, path(&["work", "reap", "policies"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { answer(super::reap::put(&cx.hub, &query(&v, &["scope"]), v.get("policy").cloned().unwrap_or(Value::Null), Some(&cx.caller)).await).await }),
            op!("ci.secrets.list", "A scope's secret names, with when each was set and last used (never their values).", ro: true, destructive: false,
                input: Input::of::<input::CiSecretsList>(),
                route: |v| Ok((Method::GET, path(&["secrets"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let scope = scope_of(&cx, &v).await?; Ok(json!({ "scope": text(&v, "scope")?, "secrets": super::secrets::list(&cx.hub, &scope).await.map_err(internal)? })) }),
            op!("ci.secrets.set", "Set a secret in a scope (its value is stored encrypted and never shown again).", ro: false, destructive: true,
                input: Input::of::<input::CiSecretsSet>(),
                route: |v| Ok((Method::PUT, path(&["secrets", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    super::secrets::set(&cx.hub, &scope, &text(&v, "name")?, &text(&v, "value")?, &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(json!({ "set": true }))
                }),
            op!("ci.variables.list", "A scope's variables, with their values.", ro: true, destructive: false,
                input: Input::of::<input::CiVariablesList>(),
                route: |v| Ok((Method::GET, path(&["variables"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { let scope = scope_of(&cx, &v).await?; Ok(json!({ "scope": text(&v, "scope")?, "variables": super::secrets::list_vars(&cx.hub, &scope).await.map_err(internal)? })) }),
            op!("ci.variables.set", "Set a variable in a scope.", ro: false, destructive: false,
                input: Input::of::<input::CiVariablesSet>(),
                route: |v| Ok((Method::PUT, path(&["variables", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    super::secrets::set_var(&cx.hub, &scope, &text(&v, "name")?, &text(&v, "value")?, &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(json!({ "set": true }))
                }),
            op!("ci.integrations.list", "The integrations jobs and deployments use: kind, name, scope, what they're for (never their secrets).", ro: true, destructive: false,
                input: Input::of::<input::CiIntegrationsList>(),
                route: |_| Ok((Method::GET, path(&["integrations"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { Ok(json!({ "integrations": super::integrations::list(&cx.hub).await.map_err(internal)? })) }),
            op!("ci.integrations.verify", "Check an integration works: its credentials, or a federated exchange as a repository's job (and environment) would make it.", ro: false, destructive: false,
                input: Input::of::<input::CiIntegrationsVerify>(),
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
                input: Input::of::<input::CiEnvironmentsList>(),
                route: |v| Ok((Method::GET, path(&["environments"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::environments::route(&cx.hub, &Method::GET, &[], &query(&v, &["repo"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.board", "Every environment of every repository you may read: its kind and targets, protection, latest deployment, and the legs waiting to deploy there (for a review, a wait timer, a window, a promotion, a gate or their turn).", ro: true, destructive: false,
                input: Input::of::<input::CiEnvironmentsBoard>(),
                route: |_| Ok((Method::GET, path(&["environments", "board"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::environments::route(&cx.hub, &Method::GET, &["board"], &HashMap::new(), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.starter", "Make a repository's starter environments in one step: dev, staging (the trunk only) and production (the trunk only, once you approve), each unless it has one of that name.", ro: false, destructive: false,
                input: Input::of::<input::CiEnvironmentsStarter>(),
                route: |v| Ok((Method::POST, path(&["environments", "starter"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::environments::route(&cx.hub, &Method::POST, &["starter"], &query(&v, &["repo"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.save", "Make an environment in kiln, or change one made here: its kind and where it deploys, protection, URL and concurrency limit (`.kiln/environments.yaml`'s are changed by a diff).", ro: false, destructive: false,
                input: Input::of::<input::CiEnvironmentsSave>(),
                route: |v| Ok((Method::PUT, path(&["environments", &text(v, "name")?]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::PUT, &[name.as_str()], &query(&v, &["repo"]), v.clone(), Some(&cx.caller)).await).await }),
            op!("ci.insights.repos", "Insights (W19-R01): every repository's runs, failures, durations and costs; or one repository's, by workflow.", ro: true, destructive: false,
                input: Input::of::<input::CiInsightsRepos>(),
                route: |v| Ok((Method::GET, match v.get("repo").and_then(Value::as_str) { Some(r) => path(&["insights", "repos", r]), None => path(&["insights", "repos"]) }, query(v, &["days"]))),
                run: |cx: Cx, v: Value| async move {
                    let repo = v.get("repo").and_then(Value::as_str).map(str::to_owned);
                    let rest: Vec<&str> = match &repo { Some(r) => vec!["repos", r.as_str()], None => vec!["repos"] };
                    answer(super::insights::route(&cx.hub, &rest, &query(&v, &["days"])).await).await
                }),
            op!("ci.insights.workflows", "A workflow's insights (`workflow`, its path; `ci.insights.repos` with `repo` lists them): durations and success over time, the critical path, its jobs, slowest steps, executors and pools, cache hit rates, queue time, flakiness.", ro: true, destructive: false,
                input: Input::of::<input::CiInsightsWorkflows>(),
                route: |v| Ok((Method::GET, path(&["insights", "workflows", &text(v, "repo")?]), query(v, &["workflow", "days"]))),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; answer(super::insights::route(&cx.hub, &["workflows", repo.as_str()], &query(&v, &["workflow", "days"])).await).await }),
            op!("ci.insights.runners", "Runners' insights: utilization, queue time and failures by runner.", ro: true, destructive: false,
                input: Input::of::<input::CiInsightsRunners>(),
                route: |v| Ok((Method::GET, path(&["insights", "runners"]), query(v, &["days"]))),
                run: |cx: Cx, v: Value| async move { answer(super::insights::route(&cx.hub, &["runners"], &query(&v, &["days"])).await).await }),
            op!("ci.diffs.list", "A repository's diffs (W14): `open` (the default), a state, or `all`; by author.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "diffs"]), query(v, &["state", "author", "limit"]))),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; diffs(&cx, Method::GET, &["repos", repo.as_str(), "diffs"], &query(&v, &["state", "author", "limit"]), &Value::Null).await }),
            op!("ci.diffs.get", "A diff: its versions, state, reviewers, stack and runs.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsGet>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str()], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.threads", "A diff's comment threads: where each is (version, file, side, lines), its comments, and whether it's resolved.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsThreads>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "comments"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.readiness", "What keeps a diff from landing (reviews, checks, policy), or that it's ready.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsReadiness>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "land"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "land"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.comment", "Comment on a diff: on the whole, or on a file's lines (`file`, `line` or `start_line`/`end_line`, `side`), with an optional suggestion; a draft until published.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsComment>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "comments"], &HashMap::new(), &v).await }),
            op!("ci.diffs.reply", "Reply in a diff's thread.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsReply>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "comments"], &HashMap::new(), &v).await }),
            op!("ci.diffs.resolve", "Resolve a comment's thread, or open it again.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsResolve>(),
                route: |v| Ok((Method::PATCH, path(&["comments", &text(v, "comment")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let c = text(&v, "comment")?;
                    let state = v.get("state").and_then(Value::as_str).unwrap_or("resolved");
                    diffs(&cx, Method::PATCH, &["comments", c.as_str()], &HashMap::new(), &json!({ "state": state })).await
                }),
            op!("ci.diffs.review", "Review a diff: accept, request changes, comment, or resign; publishes your draft comments.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsReview>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "review"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "review"], &HashMap::new(), &v).await }),
            op!("ci.diffs.submit_patch", "Submit commits as diffs without git: patches (`git format-patch`'s mbox, or a list) onto the trunk, each a diff or a new version of its diff (by its change: its Kiln-Change, a Change-Id, or the diff it rewrites).", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsSubmitPatch>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "submit"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let repo = text(&v, "repo")?; diffs(&cx, Method::POST, &["repos", repo.as_str(), "submit"], &HashMap::new(), &v).await }),
            op!("ci.diffs.land", "Land a diff (with the diffs below it) through the land queue, by a strategy: `stack`, `stack-squash` or `ff-only` (the trunk's default when left out).", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsLand>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "land"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "land"], &HashMap::new(), &v).await }),
            op!("ci.workflows.edit", "Save a workflow's new text, never straight onto a trunk: checked first (refused with its errors), then as `to` says: `diff`, a diff on the trunk of a repository kiln hosts that lands through review; `change`, a new branch and its pull or merge request on GitHub (the App), GitLab or Bitbucket; `branch`, a commit to `branch`, refused as a push of it would be; `patch`, only the patch. By default the first the repository takes. Validate and preview it before, and watch its run after.", ro: false, destructive: false,
                input: Input::of::<input::CiWorkflowsEdit>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "workflows", &text(v, "path")?, "edit"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let (repo, file) = (text(&v, "repo")?, text(&v, "path")?);
                    super::workflows::edit(&cx.hub, &cx.caller, &repo, &file, &v).await
                }),
            op!("ci.oidc.token", "Your own workload token, as the agent kiln started (W10-R11): `sub` kiln:agent:<session>, acting for whoever started you, for `audience` (a model provider, a secret store, a cloud's STS). Federate with it instead of a stored key.", ro: false, destructive: false,
                input: Input::of::<input::CiOidcToken>(),
                route: |_| Ok((Method::POST, path(&["oidc", "token"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::oidc::agent_token(&cx.hub, &cx.caller, &v).await }),
            op!("ci.notifications.deliveries", "What notification rules sent (W19-R06), newest first: each delivery's rule, event, channel, whether it went and why not, and its attempts.", ro: true, destructive: false,
                input: Input::of::<input::CiNotificationsDeliveries>(),
                route: |v| Ok((Method::GET, path(&["notify", "deliveries"]), query(v, &["run", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::notify::deliveries(&cx.hub, &query(&v, &["run", "limit"])).await).await }),
            op!("ci.tests.propose_quarantine", "Propose quarantining a test (W16-R05) as a diff adding it to the trunk's `.kiln/quarantine.yaml`: under `quarantine: enforce`, its failures alone then let a step pass, with a warning. It lands through review.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiTestsProposeQuarantine>(),
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
                input: Input::of::<input::CiCachesList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "caches"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::caches(&cx.hub, &text(&v, "repo")?, None).await).await }),
            // §4.1's other routes (W20-AC01): every one an operation.
            op!("ci.caches.delete", "Delete one of a repository's caches (its id, from ci.caches.list).", ro: false, destructive: true,
                input: Input::of::<input::CiCachesDelete>(),
                route: |v| Ok((Method::DELETE, path(&["repos", &text(v, "repo")?, "caches", &text(v, "cache")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let id = text(&v, "cache")?; answer(super::runs_api::caches(&cx.hub, &text(&v, "repo")?, Some(id.as_str())).await).await }),
            op!("ci.tests.history", "A test's last results on a branch (the default branch unless said), newest first: each one's run, leg, attempt, commit, outcome and duration (W16-R03).", ro: true, destructive: false,
                input: Input::of::<input::CiTestsHistory>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "tests", &text(v, "test")?, "history"]), query(v, &["branch", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::results::history(&cx.hub, &text(&v, "repo")?, &text(&v, "test")?, &query(&v, &["branch", "limit"])).await).await }),
            op!("ci.runs.bisect", "Bisect a failure: the run's job (the one that failed, by default), run again on the commits between the last it passed on (`good`: the last green run of its workflow on its branch before it, by default) and the run's (`bad`), the range halved each time, until the first commit it fails on is found. Each try is a run of that job at that commit, trusted as the run was. Follow it with ci.runs.bisects.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsBisect>(),
                route: |v| Ok((Method::POST, path(&["runs", &text(v, "run")?, "bisect"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::bisect::start(&cx.hub, &text(&v, "run")?, &v, &by(&cx)).await).await }),
            op!("ci.runs.bisects", "A run's bisections, newest first, or one (`bisect`): the commits between, each tried with its run and conclusion, and the first commit the job fails on once found (`culprit`), or why it couldn't be.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsBisects>(),
                route: |v| Ok((Method::GET, match v.get("bisect").and_then(Value::as_str) { Some(b) => path(&["runs", &text(v, "run")?, "bisects", b]), None => path(&["runs", &text(v, "run")?, "bisects"]) }, HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::bisect::list(&cx.hub, &text(&v, "run")?, v.get("bisect").and_then(Value::as_str)).await).await }),
            op!("experiments.start", "Start an experiment (W20-R11): options measured against each other and together on a base commit (the default branch's head by default). Each option is a `commit`, a `branch` or a `diff`; `strategy` makes the variants: each, all (every combination), pairs, greedy, or custom (`variants`); the baseline is one unless `baseline: false`. `criteria`: each `{name, kind: pipeline (workflow) | command (command, runs-on) | metric (metric, better: lower | higher), required, threshold: {at_most | at_least | better}}`, a metric read from the pipelines' and commands' runs. Each variant is measured `repetitions` times (3), interleaved, `max_parallel` at once (4), within `budget_minutes`. `decide`: ask (bring the results back) or auto (take the clear winner); the agent that asks is told.", base: "", ro: false, destructive: false,
                input: Input::of::<input::ExperimentsStart>(),
                route: |_| Ok((Method::POST, path(&["experiments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::experiments::start(&cx.hub, &cx.caller, v).await).await }),
            op!("experiments.list", "Experiments you may read, newest first (one repository's with `repo`): each one's state, how many of its runs are measured, and its recommendation and decision.", base: "", ro: true, destructive: false,
                input: Input::of::<input::ExperimentsList>(),
                route: |v| Ok((Method::GET, path(&["experiments"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { answer(super::experiments::list(&cx.hub, &cx.caller, &query(&v, &["repo"])).await).await }),
            op!("experiments.get", "An experiment: its options and variants (each one's commit, or why it couldn't be built), its runs, and its results: each variant's required checks, its metrics against the baseline with 95% intervals, each option's main effect, each pair's interaction, and the recommendation.", base: "", ro: true, destructive: false,
                input: Input::of::<input::ExperimentsGet>(),
                route: |v| Ok((Method::GET, path(&["experiments", &text(v, "experiment")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::experiments::show(&cx.hub, &cx.caller, &text(&v, "experiment")?).await).await }),
            op!("experiments.decide", "Continue with one of an experiment's variants: the agent that asked is told its commit (`refs/kiln/experiments/<id>/<variant>`).", base: "", ro: false, destructive: false,
                input: Input::of::<input::ExperimentsDecide>(),
                route: |v| Ok((Method::POST, path(&["experiments", &text(v, "experiment")?, "decide"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::experiments::decide(&cx.hub, &cx.caller, &text(&v, "experiment")?, &v).await).await }),
            op!("experiments.cancel", "Stop an experiment: no more runs, and those running are cancelled.", base: "", ro: false, destructive: false,
                input: Input::of::<input::ExperimentsCancel>(),
                route: |v| Ok((Method::POST, path(&["experiments", &text(v, "experiment")?, "cancel"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::experiments::cancel(&cx.hub, &cx.caller, &text(&v, "experiment")?).await).await }),
            op!("ci.store.verify", "Re-hash every chunk of the store on the server's disk (W08-R01): how many and how much, and any whose contents aren't their digest (each moved aside, so it's stored again on its next write).", ro: false, destructive: false,
                input: Input::of::<input::CiStoreVerify>(),
                route: |_| Ok((Method::POST, path(&["store", "verify"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { Ok(serde_json::to_value(super::services::cas::verify(&cx.hub).await.map_err(internal)?).unwrap_or_default()) }),
            op!("ci.store.gc", "Collect the store now: objects nothing refers to (no cache entry, artifact, release asset, test report, registry blob), then chunks no object uses; anything younger than an hour stays.", ro: false, destructive: true,
                input: Input::of::<input::CiStoreGc>(),
                route: |_| Ok((Method::POST, path(&["store", "gc"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move {
                    let c = super::services::cas::collect(&cx.hub).await.map_err(internal)?;
                    Ok(json!({ "objects": c.objects, "bytes": c.bytes }))
                }),
            op!("ci.workflows.schema", "The JSON Schema of a workflow file (`gha` or `kiln`), an overlay (`overlay`) or an action (`action`): what editors complete and validate against.", ro: true, destructive: false,
                input: Input::of::<input::CiWorkflowsSchema>(),
                route: |v| Ok((Method::GET, path(&["schema", &text(v, "kind")?]), HashMap::new())),
                run: |_cx: Cx, v: Value| async move { answer(super::workflows::schema(&text(&v, "kind")?)).await }),
            op!("ci.runs.graph", "A run's graph: its jobs (each with its legs, their states and wait reasons, and its span), the edges its needs make, and the critical path.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsGraph>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "graph"]), query(v, &["attempt"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::graph(&cx.hub, &text(&v, "run")?, &query(&v, &["attempt"])).await).await }),
            op!("ci.runs.summary", "A run's job summaries (`$GITHUB_STEP_SUMMARY`), Markdown, by leg and step.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsSummary>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "summary"]), query(v, &["attempt"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::part(&cx.hub, &text(&v, "run")?, "summary", &query(&v, &["attempt"])).await).await }),
            op!("ci.runs.annotations", "A run's annotations (errors, warnings and notices its steps and tests made), with where in the code each is.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsAnnotations>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "annotations"]), query(v, &["attempt"]))),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::part(&cx.hub, &text(&v, "run")?, "annotations", &query(&v, &["attempt"])).await).await }),
            op!("ci.runs.attestations", "The provenance kiln signed for what a run built (W18): each subject's digest, its predicate and signature.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsAttestations>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "attestations"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::attest::for_run(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.runs.pending", "What a run waits for, whichever provider runs it: approvals, environments' reviews, runners.", ro: true, destructive: false,
                input: Input::of::<input::CiRunsPending>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "pending"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::runs_api::pending(&cx.hub, &text(&v, "run")?).await).await }),
            op!("ci.runs.artifact", "Where to download one of a run's artifacts (its id, from ci.artifacts.list), whichever provider ran it: a URL that works for a minute without a token, or kiln's for another provider's (with your token).", ro: true, destructive: false,
                input: Input::of::<input::CiRunsArtifact>(),
                route: |v| Ok((Method::GET, path(&["runs", &text(v, "run")?, "artifacts", &text(v, "artifact")?, "download"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { artifact_link(&cx, &text(&v, "run")?, &text(&v, "artifact")?).await }),
            op!("ci.runs.repository_dispatch", "GitHub's `repository_dispatch`: the default branch's workflows that listen for `event_type` run, with `client_payload` in their event.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsRepositoryDispatch>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "repository-dispatch"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let ask = json!({ "event_type": text(&v, "event_type")?, "client_payload": v.get("client_payload").cloned().unwrap_or(Value::Null) });
                    answer(super::workflows::repository_dispatch_value(&cx.hub, &text(&v, "repo")?, ask, cx.caller.started_by()).await).await
                }),
            op!("ci.uploads.heads", "The commits of a repository the server has (its branches' and tags' heads): what a working tree's pack (ci.uploads.pack) can leave out.", ro: true, destructive: false,
                input: Input::of::<input::CiUploadsHeads>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "heads"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::uploads::heads(&cx.hub, &text(&v, "repo")?).await).await }),
            op!("ci.uploads.pack", "Send a commit the server doesn't have as a git pack (base64; thin over ci.uploads.heads' commits), for ci.runs.local to run.", ro: false, destructive: false,
                input: Input::of::<input::CiUploadsPack>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "packs"]), query(v, &["sha"]))),
                run: |cx: Cx, v: Value| async move {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(text(&v, "pack")?.trim()).map_err(|e| bad(format!("`pack` isn't base64: {e}")))?;
                    answer(super::uploads::pack_from(&cx.hub, &text(&v, "repo")?, &query(&v, &["sha"]), super::uploads::PackFrom::Bytes(bytes), &by(&cx)).await).await
                }),
            op!("ci.runs.local", "Run a commit's workflows as an event of your choosing, before you push (`kiln workflow run --server`): the commit (sent with ci.uploads.pack, or one the server has), the event and its payload, which workflows, one job, matrix combinations, inputs, variables and secrets for these runs only. Untrusted unless you're the owner.", ro: false, destructive: false,
                input: Input::of::<input::CiRunsLocal>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "local-runs"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let mut ask = v.clone();
                    if let Some(o) = ask.as_object_mut() {
                        o.remove("repo");
                    }
                    answer(super::uploads::local_runs_value(&cx.hub, &text(&v, "repo")?, Some(&cx.caller), ask).await).await
                }),
            op!("ci.legs.compare", "What changed between two legs (`other`: a leg's id, or `last-green`): their steps side by side, timings, environment, the runner and image, the commits between them.", ro: true, destructive: false,
                input: Input::of::<input::CiLegsCompare>(),
                route: |v| Ok((Method::GET, path(&["legs", &text(v, "leg")?, "compare", &text(v, "other")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::compare::legs(&cx.hub, &text(&v, "leg")?, &text(&v, "other")?).await).await }),
            op!("ci.oidc.subject", "The subject claim of a scope's ID tokens (W10-R09): GitHub's default or the claims it's made of (`include_claim_keys`).", ro: true, destructive: false,
                input: Input::of::<input::CiOidcSubject>(),
                route: |v| Ok((Method::GET, path(&["oidc", "subject"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { answer(super::oidc::subject(&cx.hub, &Method::GET, &query(&v, &["scope"]), Value::Null, &by(&cx)).await).await }),
            op!("ci.oidc.set_subject", "Set the subject claim of a scope's ID tokens: GitHub's default (`use_default`), or the claims it's made of, in order (`include_claim_keys`).", ro: false, destructive: false,
                input: Input::of::<input::CiOidcSetSubject>(),
                route: |v| Ok((Method::PUT, path(&["oidc", "subject"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let body = json!({ "use_default": v.get("use_default"), "include_claim_keys": v.get("include_claim_keys") });
                    answer(super::oidc::subject(&cx.hub, &Method::PUT, &query(&v, &["scope"]), body, &by(&cx)).await).await
                }),
            op!("ci.oidc.reset_subject", "A scope's ID tokens' subject back to what it inherits.", ro: false, destructive: false,
                input: Input::of::<input::CiOidcResetSubject>(),
                route: |v| Ok((Method::DELETE, path(&["oidc", "subject"]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move { answer(super::oidc::subject(&cx.hub, &Method::DELETE, &query(&v, &["scope"]), Value::Null, &by(&cx)).await).await }),
            op!("ci.oidc.status", "kiln's OIDC issuer: its address, its keys and their ages, whether its discovery document and keys are published where providers read them, and each provider's readiness.", ro: true, destructive: false,
                input: Input::of::<input::CiOidcStatus>(),
                route: |_| Ok((Method::GET, path(&["oidc"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::oidc::admin(&cx.hub, &Method::GET, &[], Value::Null).await).await }),
            op!("ci.oidc.jwks", "The issuer's discovery document and keys, to publish by hand where providers read them.", ro: true, destructive: false,
                input: Input::of::<input::CiOidcJwks>(),
                route: |_| Ok((Method::GET, path(&["oidc", "jwks-export"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::oidc::admin(&cx.hub, &Method::GET, &["jwks-export"], Value::Null).await).await }),
            op!("ci.oidc.rotate", "Rotate the issuer's signing key: a new key signs from now on; the old one is still published until tokens it signed have expired.", ro: false, destructive: false,
                input: Input::of::<input::CiOidcRotate>(),
                route: |_| Ok((Method::POST, path(&["oidc", "rotate"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::oidc::admin(&cx.hub, &Method::POST, &["rotate"], json!({})).await).await }),
            op!("ci.oidc.publish", "Publish the issuer's discovery document and keys where it's configured to (an object store, a static site).", ro: false, destructive: false,
                input: Input::of::<input::CiOidcPublish>(),
                route: |_| Ok((Method::POST, path(&["oidc", "publish"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::oidc::admin(&cx.hub, &Method::POST, &["publish"], json!({})).await).await }),
            op!("ci.secrets.delete", "Delete a secret from a scope.", ro: false, destructive: true,
                input: Input::of::<input::CiSecretsDelete>(),
                route: |v| Ok((Method::DELETE, path(&["secrets", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    let gone = super::secrets::delete(&cx.hub, &scope, &text(&v, "name")?).await.map_err(internal)?;
                    if gone { Ok(json!({ "deleted": true })) } else { Err((StatusCode::NOT_FOUND, format!("no secret {} there", text(&v, "name")?))) }
                }),
            op!("ci.variables.delete", "Delete a variable from a scope.", ro: false, destructive: true,
                input: Input::of::<input::CiVariablesDelete>(),
                route: |v| Ok((Method::DELETE, path(&["variables", &text(v, "name")?]), query(v, &["scope"]))),
                run: |cx: Cx, v: Value| async move {
                    let scope = scope_of(&cx, &v).await?;
                    let gone = super::secrets::delete_var(&cx.hub, &scope, &text(&v, "name")?).await.map_err(internal)?;
                    if gone { Ok(json!({ "deleted": true })) } else { Err((StatusCode::NOT_FOUND, format!("no variable {} there", text(&v, "name")?))) }
                }),
            op!("ci.integrations.kinds", "The kinds of integration there are: each one's fields, how it authenticates (a stored secret, or a job's ID token exchanged), and what kiln has built of it.", ro: true, destructive: false,
                input: Input::of::<input::CiIntegrationsKinds>(),
                route: |_| Ok((Method::GET, path(&["integrations", "kinds"]), HashMap::new())),
                run: |_cx: Cx, _v: Value| async move { Ok(json!({ "kinds": super::integrations::KINDS })) }),
            op!("ci.integrations.create", "Make an integration: its kind (ci.integrations.kinds), a name, its config, its secret (write-only), its signing secret where its kind keeps one, and the scope it's for.", ro: false, destructive: false,
                input: Input::of::<input::CiIntegrationsCreate>(),
                route: |_| Ok((Method::POST, path(&["integrations"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let config = v.get("config").cloned().filter(|c| !c.is_null()).unwrap_or_else(|| json!({}));
                    let made = super::integrations::create(&cx.hub, &text(&v, "kind")?, &text(&v, "name")?, config, super::integrations::Secrets { secret: v.get("secret").and_then(Value::as_str), signing: v.get("signing_secret").and_then(Value::as_str) }, v.get("scope").and_then(Value::as_str), &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(serde_json::to_value(made).unwrap_or_default())
                }),
            op!("ci.integrations.get", "One integration: its kind, config, scope and what it's for (never its secret).", ro: true, destructive: false,
                input: Input::of::<input::CiIntegrationsGet>(),
                route: |v| Ok((Method::GET, path(&["integrations", &text(v, "integration")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let name = text(&v, "integration")?;
                    super::integrations::get(&cx.hub, &name).await.map_err(internal)?.map(|i| serde_json::to_value(i).unwrap_or_default()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no integration `{name}`")))
                }),
            op!("ci.integrations.update", "Change an integration's config or replace its secret or signing secret.", ro: false, destructive: false,
                input: Input::of::<input::CiIntegrationsUpdate>(),
                route: |v| Ok((Method::PATCH, path(&["integrations", &text(v, "integration")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let i = super::integrations::update(&cx.hub, &text(&v, "integration")?, v.get("config").cloned().filter(|c| !c.is_null()), v.get("secret").and_then(Value::as_str), v.get("signing_secret").and_then(Value::as_str), &by(&cx)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                    Ok(serde_json::to_value(i).unwrap_or_default())
                }),
            op!("ci.integrations.delete", "Delete an integration (jobs and deployments using it fail until another takes its name).", ro: false, destructive: true,
                input: Input::of::<input::CiIntegrationsDelete>(),
                route: |v| Ok((Method::DELETE, path(&["integrations", &text(v, "integration")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move {
                    let name = text(&v, "integration")?;
                    if super::integrations::delete(&cx.hub, &name, &by(&cx)).await.map_err(internal)? { Ok(json!({ "deleted": true })) } else { Err((StatusCode::NOT_FOUND, format!("no integration `{name}`"))) }
                }),
            op!("ci.actions.list", "The actions catalog (W07-R12): the mirrored actions and their versions and pinned commits, searched by `q`.", ro: true, destructive: false,
                input: Input::of::<input::CiActionsList>(),
                route: |v| Ok((Method::GET, path(&["actions"]), query(v, &["q"]))),
                run: |cx: Cx, v: Value| async move { answer(super::actions::route(&cx.hub, &Method::GET, &[], &query(&v, &["q"]), Value::Null).await).await }),
            op!("ci.actions.kiln", "kiln's own actions (`kiln/<name>@v1`): what each does and takes.", ro: true, destructive: false,
                input: Input::of::<input::CiActionsKiln>(),
                route: |_| Ok((Method::GET, path(&["actions", "kiln"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::actions::route(&cx.hub, &Method::GET, &["kiln"], &HashMap::new(), Value::Null).await).await }),
            op!("ci.actions.sync", "Mirror the server's prefetched actions now (`[ci.actions] prefetch`), each pinned to its commit.", ro: false, destructive: false,
                input: Input::of::<input::CiActionsSync>(),
                route: |_| Ok((Method::POST, path(&["actions", "sync"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { answer(super::actions::route(&cx.hub, &Method::POST, &["sync"], &HashMap::new(), json!({})).await).await }),
            op!("ci.runners.probe", "Have a runner probe its facts again now (W13-R01): its OS, CPU, memory and disk, GPUs, executors, tools. Answers with its new report once it comes.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiRunnersProbe>(),
                route: |v| Ok((Method::POST, path(&["agents", &text(v, "runner")?, "probe"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(crate::api::probe_agent(&cx.hub, &text(&v, "runner")?).await).await }),
            op!("ci.pools.get", "One executor pool: its kind, image, labels, sizes, and its executors now.", ro: true, destructive: false,
                input: Input::of::<input::CiPoolsGet>(),
                route: |v| Ok((Method::GET, path(&["pools", &text(v, "name")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::pools::list(&cx.hub, Some(name.as_str())).await).await }),
            op!("ci.pools.delete", "Remove an executor pool (one made by API; a config file's stays).", ro: false, destructive: true,
                input: Input::of::<input::CiPoolsDelete>(),
                route: |v| Ok((Method::DELETE, path(&["pools", &text(v, "name")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::pools::delete(&cx.hub, &text(&v, "name")?).await).await }),
            op!("ci.environments.get", "One of a repository's environments: its kind and targets, protection, latest deployment, and what waits to deploy there.", ro: true, destructive: false,
                input: Input::of::<input::CiEnvironmentsGet>(),
                route: |v| Ok((Method::GET, path(&["environments", &text(v, "name")?]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::GET, &[name.as_str()], &query(&v, &["repo"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.deployments", "An environment's deployments, newest first: each one's run, commit, who started it and how it went.", ro: true, destructive: false,
                input: Input::of::<input::CiEnvironmentsDeployments>(),
                route: |v| Ok((Method::GET, path(&["environments", &text(v, "name")?, "deployments"]), query(v, &["repo", "limit"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::GET, &[name.as_str(), "deployments"], &query(&v, &["repo", "limit"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.environments.deploy", "Deploy to an environment by hand: the workflow at `ref` (the trunk by default) that deploys there, one with `workflow_dispatch` and a job with `environment: <name>` (`workflow` when several), dispatched with `inputs`. Its job waits for the environment's protection as any deployment's does.", ro: false, destructive: false,
                input: Input::of::<input::CiEnvironmentsDeploy>(),
                route: |v| Ok((Method::POST, path(&["environments", &text(v, "name")?, "deploy"]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::POST, &[name.as_str(), "deploy"], &query(&v, &["repo"]), v.clone(), Some(&cx.caller)).await).await }),
            op!("ci.environments.delete", "Delete an environment made in kiln (a `.kiln/environments.yaml` one is removed by a change to the file).", ro: false, destructive: true,
                input: Input::of::<input::CiEnvironmentsDelete>(),
                route: |v| Ok((Method::DELETE, path(&["environments", &text(v, "name")?]), query(v, &["repo"]))),
                run: |cx: Cx, v: Value| async move { let name = text(&v, "name")?; answer(super::environments::route(&cx.hub, &Method::DELETE, &[name.as_str()], &query(&v, &["repo"]), Value::Null, Some(&cx.caller)).await).await }),
            op!("ci.webhooks.deliveries", "What outgoing webhooks sent (W19-R07): each delivery's event, integration, status and attempts, newest first.", ro: true, destructive: false,
                input: Input::of::<input::CiWebhooksDeliveries>(),
                route: |v| Ok((Method::GET, path(&["webhooks", "deliveries"]), query(v, &["integration", "event", "limit"]))),
                run: |cx: Cx, v: Value| async move { answer(super::webhooks::route(&cx.hub, &Method::GET, &["deliveries"], &query(&v, &["integration", "event", "limit"])).await).await }),
            op!("ci.webhooks.delivery", "One delivery: what was sent, each attempt's response.", ro: true, destructive: false,
                input: Input::of::<input::CiWebhooksDelivery>(),
                route: |v| Ok((Method::GET, path(&["webhooks", "deliveries", &text(v, "delivery")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let id = text(&v, "delivery")?; answer(super::webhooks::route(&cx.hub, &Method::GET, &["deliveries", id.as_str()], &HashMap::new()).await).await }),
            op!("ci.webhooks.redeliver", "Send a delivery again.", ro: false, destructive: false,
                input: Input::of::<input::CiWebhooksRedeliver>(),
                route: |v| Ok((Method::POST, path(&["webhooks", "deliveries", &text(v, "delivery")?, "redeliver"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let id = text(&v, "delivery")?; answer(super::webhooks::route(&cx.hub, &Method::POST, &["deliveries", id.as_str(), "redeliver"], &HashMap::new()).await).await }),
            op!("ci.hooks.list", "A repository's inbound webhooks (W03-R02): each one's id and the URL other systems call to start its `webhook` workflows.", ro: true, destructive: false,
                input: Input::of::<input::CiHooksList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "hooks"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::inbound::list(&cx.hub, &text(&v, "repo")?).await).await }),
            op!("ci.hooks.create", "Make an inbound webhook for a repository: its URL and signing secret, shown once.", ro: false, destructive: false,
                input: Input::of::<input::CiHooksCreate>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "hooks"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::inbound::create(&cx.hub, &text(&v, "repo")?, &v, &by(&cx)).await).await }),
            op!("ci.hooks.delete", "Delete an inbound webhook: calls to its URL stop starting anything.", ro: false, destructive: true,
                input: Input::of::<input::CiHooksDelete>(),
                route: |v| Ok((Method::DELETE, path(&["repos", &text(v, "repo")?, "hooks", &text(v, "hook")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { answer(super::inbound::delete(&cx.hub, &text(&v, "repo")?, &text(&v, "hook")?, &by(&cx)).await).await }),
            op!("ci.providers.get", "What a repository's CI runs on (W23): KilnCI, GitHub Actions, GitLab CI/CD or Bitbucket Pipelines, and each provider's state.", ro: true, destructive: false,
                input: Input::of::<input::CiProvidersGet>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "providers"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::providers::show(&cx.hub, &text(&v, "repo")?).await }),
            op!("ci.providers.set", "Choose what a repository's CI runs on: `providers`, one or more of `kiln`, `github-actions`, `gitlab-ci`, `bitbucket-pipelines`.", ro: false, destructive: false,
                input: Input::of::<input::CiProvidersSet>(),
                route: |v| Ok((Method::PUT, path(&["repos", &text(v, "repo")?, "providers"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::providers::set(&cx.hub, &text(&v, "repo")?, &json!({ "providers": v.get("providers") }), &by(&cx)).await }),
            op!("ci.providers.sync", "Look at a repository's other providers' runs now, instead of at their next poll.", ro: true, destructive: false,
                input: Input::of::<input::CiProvidersSync>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "providers", "sync"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::providers::sync_now(&cx.hub, &text(&v, "repo")?).await }),
            op!("ci.providers.workflows", "Each of a repository's providers' CI files, checked by that provider's rules (GitHub's, GitLab's, Bitbucket's).", ro: true, destructive: false,
                input: Input::of::<input::CiProvidersWorkflows>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "providers", "workflows"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::providers::workflows(&cx.hub, &text(&v, "repo")?).await }),
            op!("ci.providers.checks", "Every provider's runs of one commit: each run's provider, workflow, state and link.", ro: true, destructive: false,
                input: Input::of::<input::CiProvidersChecks>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "commits", &text(v, "sha")?, "checks"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { super::providers::checks(&cx.hub, &text(&v, "repo")?, &text(&v, "sha")?).await }),
            op!("host.pulls.list", "A repository's open pull requests (merge requests) on its host, GitHub, GitLab or Bitbucket: as you, when you signed in with that host.", ro: true, destructive: false,
                input: Input::of::<input::HostPullsList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "pulls"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { pulls(&cx, Method::GET, &text(&v, "repo")?, &[], Value::Null).await }),
            op!("host.pulls.get", "One pull request: its state, branches, checks, reviews and comments.", ro: true, destructive: false,
                input: Input::of::<input::HostPullsGet>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "pulls", &number(v)?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let n = number(&v)?; pulls(&cx, Method::GET, &text(&v, "repo")?, &[n.as_str()], Value::Null).await }),
            op!("host.pulls.comment", "Comment on a pull request, on a line (`path`, `line`, `side`), or replying to a comment (`reply_to`).", ro: false, destructive: false,
                input: Input::of::<input::HostPullsComment>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "pulls", &number(v)?, "comments"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let n = number(&v)?; pulls(&cx, Method::POST, &text(&v, "repo")?, &[n.as_str(), "comments"], v.clone()).await }),
            op!("host.pulls.review", "Review a pull request: `approve`, `request_changes` or `comment`, with a body.", ro: false, destructive: false,
                input: Input::of::<input::HostPullsReview>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "pulls", &number(v)?, "reviews"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let n = number(&v)?; pulls(&cx, Method::POST, &text(&v, "repo")?, &[n.as_str(), "reviews"], v.clone()).await }),
            op!("host.pulls.merge", "Merge a pull request on its host: `merge`, `squash` or `rebase`, as the host allows.", ro: false, destructive: false,
                input: Input::of::<input::HostPullsMerge>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "pulls", &number(v)?, "merge"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let n = number(&v)?; pulls(&cx, Method::POST, &text(&v, "repo")?, &[n.as_str(), "merge"], v.clone()).await }),
            op!("ci.diffs.versions", "A diff's versions: each one's commit, base, when it was pushed and by whom.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsVersions>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "versions"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "versions"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.files", "The files a version of a diff changes, with their hunks.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsFiles>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "versions", &version(v)?, "files"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let (d, n) = (text(&v, "diff")?, version(&v)?); diffs(&cx, Method::GET, &["diffs", d.as_str(), "versions", n.as_str(), "files"], &HashMap::new(), &Value::Null).await }),
            op!("ci.diffs.blob", "A file as a version of a diff has it (`side`: `new`, or `old` for its base's).", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsBlob>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "versions", &version(v)?, "blob"]), query(v, &["path", "side"]))),
                run: |cx: Cx, v: Value| async move { let (d, n) = (text(&v, "diff")?, version(&v)?); diffs(&cx, Method::GET, &["diffs", d.as_str(), "versions", n.as_str(), "blob"], &query(&v, &["path", "side"]), &Value::Null).await }),
            op!("ci.diffs.dependencies", "What a diff's version (the latest unless `to`) changes in the dependencies its lockfiles pin, against the commit it was made on (or version `from`): each lockfile's packages added, removed, and those whose versions changed (W18-R03).", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsDependencies>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "dependencies"]), query(v, &["from", "to"]))),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "dependencies"], &query(&v, &["from", "to"]), &Value::Null).await }),
            op!("ci.diffs.interdiff", "What changed between two versions of a diff (`from`, `to`).", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiDiffsInterdiff>(),
                route: |v| Ok((Method::GET, path(&["diffs", &text(v, "diff")?, "interdiff"]), query(v, &["from", "to"]))),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::GET, &["diffs", d.as_str(), "interdiff"], &query(&v, &["from", "to"]), &Value::Null).await }),
            op!("ci.diffs.ready", "Mark a draft diff ready for review.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsReady>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "ready"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "ready"], &HashMap::new(), &json!({})).await }),
            op!("ci.diffs.abandon", "Abandon a diff: it leaves review and the land queue (reclaim brings it back).", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsAbandon>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "abandon"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "abandon"], &HashMap::new(), &json!({})).await }),
            op!("ci.diffs.reclaim", "Bring an abandoned diff back to review.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsReclaim>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "reclaim"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "reclaim"], &HashMap::new(), &json!({})).await }),
            op!("ci.diffs.commandeer", "Take over someone else's diff: you become its author and push its next versions.", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiDiffsCommandeer>(),
                route: |v| Ok((Method::POST, path(&["diffs", &text(v, "diff")?, "commandeer"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let d = text(&v, "diff")?; diffs(&cx, Method::POST, &["diffs", d.as_str(), "commandeer"], &HashMap::new(), &json!({})).await }),
            op!("ci.change_requests.list", "A repository's change requests: branches reviewed against the trunk as a whole.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiChangeRequestsList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "change-requests"]), query(v, &["state"]))),
                run: |cx: Cx, v: Value| async move { let r = text(&v, "repo")?; diffs(&cx, Method::GET, &["repos", r.as_str(), "change-requests"], &query(&v, &["state"]), &Value::Null).await }),
            op!("ci.change_requests.open", "Open a change request: a branch reviewed against the trunk as a whole (`kiln cr open`).", base: "", ro: false, destructive: false,
                input: Input::of::<input::CiChangeRequestsOpen>(),
                route: |v| Ok((Method::POST, path(&["repos", &text(v, "repo")?, "change-requests"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let r = text(&v, "repo")?; diffs(&cx, Method::POST, &["repos", r.as_str(), "change-requests"], &HashMap::new(), &v).await }),
            op!("ci.change_requests.get", "A change request: its versions, state, reviewers and runs.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiChangeRequestsGet>(),
                route: |v| Ok((Method::GET, path(&["change-requests", &text(v, "change_request")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let k = text(&v, "change_request")?; diffs(&cx, Method::GET, &["change-requests", k.as_str()], &HashMap::new(), &Value::Null).await }),
            op!("ci.review.inbox", "Your review inbox: what needs your review, your stacks, your agents' stacks, and the conversations you're in.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiReviewInbox>(),
                route: |_| Ok((Method::GET, path(&["review", "inbox"]), HashMap::new())),
                run: |cx: Cx, _v: Value| async move { diffs(&cx, Method::GET, &["review", "inbox"], &HashMap::new(), &Value::Null).await }),
            op!("ci.queue.list", "A repository's land queue: each entry's diffs, place, state and its batch.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiQueueList>(),
                route: |v| Ok((Method::GET, path(&["repos", &text(v, "repo")?, "queue"]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let r = text(&v, "repo")?; diffs(&cx, Method::GET, &["repos", r.as_str(), "queue"], &HashMap::new(), &Value::Null).await }),
            op!("ci.queue.remove", "Take an entry out of the land queue (its id, from ci.queue.list).", base: "", ro: false, destructive: true,
                input: Input::of::<input::CiQueueRemove>(),
                route: |v| Ok((Method::DELETE, path(&["queue", &text(v, "entry")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let e = text(&v, "entry")?; diffs(&cx, Method::DELETE, &["queue", e.as_str()], &HashMap::new(), &Value::Null).await }),
            op!("ci.stacks.get", "A stack: its diffs in order, each one's state and readiness.", base: "", ro: true, destructive: false,
                input: Input::of::<input::CiStacksGet>(),
                route: |v| Ok((Method::GET, path(&["stacks", &text(v, "stack")?]), HashMap::new())),
                run: |cx: Cx, v: Value| async move { let st = text(&v, "stack")?; diffs(&cx, Method::GET, &["stacks", st.as_str()], &HashMap::new(), &Value::Null).await }),
        ]
    })
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from
/// its type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsList {
        /// The repository, `owner/name`.
        pub repo: Option<String>,
        /// Its path, e.g. `.github/workflows/ci.yml`.
        pub workflow: Option<String>,
        pub branch: Option<String>,
        pub event: Option<String>,
        /// success, failure, cancelled, skipped, timed_out, startup_failure.
        pub conclusion: Option<String>,
        pub actor: Option<String>,
        #[schemars(range(min = 1, max = 200))]
        pub limit: Option<i64>,
        /// The `next` of the previous page.
        pub cursor: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsGet {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsTrigger {
        /// The repository, `owner/name`.
        pub repo: String,
        /// Its path or file name.
        pub workflow: String,
        /// A branch or tag; the default branch when left out.
        pub r#ref: Option<String>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsAsk {
        /// The repository, `owner/name`.
        pub repo: String,
        /// Its path or file name.
        pub workflow: String,
        /// A branch or tag; the default branch when left out.
        pub r#ref: Option<String>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsFlow {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsCompare {
        /// The run's id.
        pub run: String,
        /// The run to compare with, or `last-green`.
        pub other: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsCancel {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiRunsRerunMode {
        All,
        Failed,
        Job,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsRerun {
        /// The run's id.
        pub run: String,
        pub mode: Option<CiRunsRerunMode>,
        /// With mode `job`.
        pub job: Option<String>,
        pub debug: Option<bool>,
        /// Minutes a failed leg is kept to look into (W17-R02).
        pub keep_failed: Option<i64>,
        /// Run cold (W24-R23): no reuse, no kept folders, no cache restored.
        pub cold: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiRunsApproveState {
        Approved,
        Rejected,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsApprove {
        /// The run's id.
        pub run: String,
        pub state: CiRunsApproveState,
        /// Which; all the run waits on when left out.
        pub environments: Option<Vec<String>>,
        pub comment: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsGet {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsLogs {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// The step's index (`idx` in ci.legs.get).
        pub step: i64,
        pub from: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsSearch {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// What to find, case-insensitive.
        pub q: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsFailureExcerpt {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// How many lines (40 by default).
        pub lines: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLogsSearch {
        /// What to find, case-insensitive.
        pub q: String,
        /// The repository, `owner/name`.
        pub repo: Option<String>,
        /// The workflow's path.
        pub workflow: Option<String>,
        pub branch: Option<String>,
        /// The run's: success, failure, cancelled…
        pub conclusion: Option<String>,
        pub days: Option<i64>,
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsRouting {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsExec {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// Shell text, run by bash.
        pub command: String,
        /// Seconds; 60 when left out.
        #[schemars(range(min = 1, max = 600))]
        pub timeout: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsRerunStep {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// From 1, as the run page numbers them.
        pub step: Option<i64>,
        /// Or its index, from 0.
        pub idx: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsKeep {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        #[schemars(range(min = 1, max = 1440))]
        pub minutes: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsRelease {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsRunners {
        /// The leg's id (a job of a run, one matrix combination of it), or a `.kiln/` pipeline
        /// job's.
        pub leg: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsRetry {
        /// The leg's id (a job of a run, one matrix combination of it), or a `.kiln/` pipeline
        /// job's.
        pub leg: String,
        /// The runner to run it on: one ci.legs.runners says is able to.
        pub runner: Option<String>,
        /// On any runner able to but the one its last attempt ran on.
        pub elsewhere: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsList {
        /// The repository, `owner/name`.
        pub repo: String,
        pub r#ref: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsGet {
        /// The repository, `owner/name`.
        pub repo: String,
        pub path: String,
        pub r#ref: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiImagesBuildKind {
        Dockerfile,
        Agentfile,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiImagesBuild {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The image's name.
        pub name: String,
        pub kind: Option<CiImagesBuildKind>,
        /// The definition's whole text.
        pub content: String,
        pub platforms: Option<Vec<String>>,
        /// A label, or a runner's name: one with Docker (a Dockerfile) or shards (an Agentfile).
        pub runs_on: String,
        pub push: Option<bool>,
        pub r#ref: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiActionsTry {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The action's file, e.g. `.github/actions/greet/action.yml`.
        pub path: String,
        /// Its whole text.
        pub content: String,
        /// The branch, tag or commit to try it on (the default branch when left out).
        pub r#ref: Option<String>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
        /// A label, or a runner's name.
        pub runs_on: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsLabels {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiWorkflowsValidateDialect {
        Gha,
        Kiln,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsValidate {
        /// Where it would live, e.g. `.github/workflows/ci.yml`.
        pub path: Option<String>,
        pub content: String,
        pub dialect: Option<CiWorkflowsValidateDialect>,
        /// The repository, `owner/name`.
        pub repo: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsPlanPreview {
        /// The repository, `owner/name`.
        pub repo: String,
        pub path: String,
        /// Text to plan instead of the file at the ref.
        pub content: Option<String>,
        pub event: Option<String>,
        pub r#ref: Option<String>,
        pub sha: Option<String>,
        pub payload: Option<serde_json::Map<String, serde_json::Value>>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsRerunFailed {
        /// The run's id.
        pub run: String,
        pub debug: Option<bool>,
        /// Minutes a failed leg is kept to look into (W17-R02).
        pub keep_failed: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsRerunJob {
        /// The run's id.
        pub run: String,
        /// The job's id.
        pub job: String,
        pub debug: Option<bool>,
        pub keep_failed: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiArtifactsList {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiTestsRun {
        /// The run's id.
        pub run: String,
        /// An attempt's number, or `all`; the latest when left out.
        pub attempt: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiTestsLeg {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiTestsRepoSort {
        Failing,
        Slowest,
        Flaky,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiTestsRepo {
        /// The repository, `owner/name`.
        pub repo: String,
        pub sort: Option<CiTestsRepoSort>,
        pub branch: Option<String>,
        #[schemars(range(min = 1, max = 365))]
        pub days: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiMetricsRun {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiMetricsRepo {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The metric's name.
        pub name: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunnersList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiPoolsList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiPoolsHistory {
        /// The pool.
        pub name: String,
        #[schemars(range(min = 1, max = 168))]
        pub hours: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiPoolsSetExecutor {
        Docker,
        Firecracker,
        Shards,
        K8s,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiPoolsSet {
        pub name: String,
        pub executor: CiPoolsSetExecutor,
        pub image: Option<String>,
        pub labels: Vec<String>,
        pub min_idle: Option<i64>,
        pub max: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiImagesList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRegistryMirrors {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiImagesValidate {
        pub content: String,
        /// e.g. `.kiln/images/web/Dockerfile`.
        pub path: Option<String>,
        pub kind: Option<CiImagesBuildKind>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiImagesPropose {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: String,
        pub kind: Option<CiImagesBuildKind>,
        pub content: String,
        pub draft: Option<bool>,
        pub reviewers: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRoutingRules {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRoutingExplain {
        /// The repository, `owner/name`.
        pub repo: Option<String>,
        /// A job's `runs-on`.
        pub runs_on: Option<serde_json::Value>,
        pub requires: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRoutingSet {
        /// `server` (the default) or `org:<owner>`.
        pub scope: Option<String>,
        /// Each `{ match: { labels, repos, job }, require, executor, image, prefer }`.
        pub rules: Vec<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkReapPolicies {
        pub runner: Option<String>,
        /// The repository, `owner/name`.
        pub repo: Option<String>,
        /// A session's, agent's or harness's id.
        pub session: Option<String>,
        /// The volume's size in bytes the hardware limits are shown on (1 TiB if left out).
        pub volume: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkReapSet {
        /// `server`, `runner:<name>`, `repo:<owner/name>` or `session:<id>`.
        pub scope: String,
        /// As people write it:
        /// `{ "floor": "10%", "keep_last": 3, "max_age": "14d", "schedule": "30 3 * * *" }`.
        pub policy: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiSecretsList {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiSecretsSet {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
        pub name: String,
        pub value: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiVariablesList {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiVariablesSet {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
        pub name: String,
        pub value: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsList {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsVerify {
        /// Its name.
        pub integration: String,
        /// The repository, `owner/name`.
        pub repository: Option<String>,
        pub environment: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsBoard {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsStarter {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiEnvironmentsSaveKind {
        Kubernetes,
        Host,
        Container,
        Microvm,
        Platform,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsSave {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: String,
        /// What it deploys to; leave it out for none.
        pub kind: Option<CiEnvironmentsSaveKind>,
        /// Where: `clusters` and `namespace` (kubernetes), `runners` (a constraint the runners it
        /// deploys on meet: host, container, microvm), `integration` (platform).
        pub targets: Option<serde_json::Map<String, serde_json::Value>>,
        /// `reviewers`, `prevent-self-review`, `wait-timer`, `branches`, `tags`, `windows`.
        pub protection: Option<serde_json::Map<String, serde_json::Value>>,
        pub url: Option<String>,
        #[schemars(range(min = 1))]
        pub concurrency: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiInsightsRepos {
        /// The repository, `owner/name`.
        pub repo: Option<String>,
        #[schemars(range(min = 1, max = 365))]
        pub days: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiInsightsWorkflows {
        /// The repository, `owner/name`.
        pub repo: String,
        /// Its path, e.g. `.github/workflows/ci.yml`.
        pub workflow: String,
        #[schemars(range(min = 1, max = 365))]
        pub days: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiInsightsRunners {
        #[schemars(range(min = 1, max = 365))]
        pub days: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsList {
        /// The repository, `owner/name`.
        pub repo: String,
        pub state: Option<String>,
        pub author: Option<String>,
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsGet {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsThreads {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsReadiness {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiDiffsCommentSide {
        New,
        Old,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsComment {
        /// The diff: `D12`.
        pub diff: String,
        pub body: String,
        pub file: Option<String>,
        pub line: Option<i64>,
        pub start_line: Option<i64>,
        pub end_line: Option<i64>,
        pub side: Option<CiDiffsCommentSide>,
        /// Replacement text for the lines.
        pub suggestion: Option<String>,
        pub version: Option<i64>,
        pub publish: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsReply {
        /// The diff: `D12`.
        pub diff: String,
        pub thread: String,
        pub body: String,
        pub publish: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiDiffsResolveState {
        Resolved,
        Open,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsResolve {
        /// The thread's first comment's id.
        pub comment: String,
        pub state: Option<CiDiffsResolveState>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiDiffsReviewAction {
        Accept,
        RequestChanges,
        Comment,
        Resign,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsReview {
        /// The diff: `D12`.
        pub diff: String,
        pub action: CiDiffsReviewAction,
        pub body: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsSubmitPatch {
        /// The repository, `owner/name`.
        pub repo: String,
        pub mbox: Option<String>,
        pub patches: Option<Vec<serde_json::Value>>,
        pub trunk: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    pub enum CiDiffsLandStrategy {
        #[serde(rename = "stack")]
        Stack,
        #[serde(rename = "stack-squash")]
        StackSquash,
        #[serde(rename = "ff-only")]
        FfOnly,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsLand {
        /// The diff: `D12`.
        pub diff: String,
        pub strategy: Option<CiDiffsLandStrategy>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiWorkflowsEditTo {
        Diff,
        Change,
        Branch,
        Patch,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsEdit {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The workflow's path, e.g. `.github/workflows/ci.yml`.
        pub path: String,
        /// Its whole new text.
        pub content: String,
        pub to: Option<CiWorkflowsEditTo>,
        /// For `branch`, the branch to commit to; for `change`, the new branch's name (one is made
        /// up when left out).
        pub branch: Option<String>,
        /// The diff's, change's or commit's title (`Edit <path>` when left out).
        pub title: Option<String>,
        pub draft: Option<bool>,
        pub reviewers: Option<Vec<String>>,
        pub topic: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcToken {
        /// Who it's for, as their trust policy names it.
        pub audience: String,
        /// Seconds it lasts (600 by default).
        #[schemars(range(min = 60, max = 3600))]
        pub ttl: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiNotificationsDeliveries {
        /// The run's id.
        pub run: Option<String>,
        #[schemars(range(min = 1, max = 1000))]
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiTestsProposeQuarantine {
        /// The repository, `owner/name`.
        pub repo: String,
        /// suite.name, suite::name, the name, or the id; `*` matches any run of characters.
        pub test: String,
        pub reason: String,
        /// Where it's tracked.
        pub issue: Option<String>,
        /// The last day it applies, YYYY-MM-DD.
        pub until: Option<String>,
        pub draft: Option<bool>,
        pub reviewers: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiCachesList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiCachesDelete {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The cache's id.
        pub cache: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiTestsHistory {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The test's id (from ci.tests.*).
        pub test: String,
        pub branch: Option<String>,
        #[schemars(range(min = 1, max = 500))]
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsBisect {
        /// The run's id.
        pub run: String,
        /// The job's id.
        pub job: Option<String>,
        /// A commit it passes on.
        pub good: Option<String>,
        /// A commit it fails on.
        pub bad: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsBisects {
        /// The run's id.
        pub run: String,
        /// One's id.
        pub bisect: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    pub struct ExperimentsStartOption {
        pub name: String,
        pub commit: Option<String>,
        pub branch: Option<String>,
        pub diff: Option<String>,
        /// What the option is, for an agent to implement (instead of a commit, branch or diff).
        pub implement: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum ExperimentsStartStrategy {
        Each,
        All,
        Pairs,
        Greedy,
        Custom,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum ExperimentsStartDecide {
        Ask,
        Auto,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsStart {
        /// The repository, `owner/name` (the one your folder is a checkout of when left out).
        pub repo: Option<String>,
        pub base: Option<String>,
        pub question: Option<String>,
        pub options: Vec<ExperimentsStartOption>,
        pub strategy: Option<ExperimentsStartStrategy>,
        pub variants: Option<Vec<Vec<String>>>,
        pub baseline: Option<bool>,
        pub criteria: Vec<serde_json::Map<String, serde_json::Value>>,
        #[schemars(range(min = 1, max = 20))]
        pub repetitions: Option<i64>,
        #[schemars(range(min = 1, max = 16))]
        pub max_parallel: Option<i64>,
        pub budget_minutes: Option<i64>,
        pub decide: Option<ExperimentsStartDecide>,
        /// The agent to tell (you, by default).
        pub agent: Option<String>,
        /// Where the agents implementing options run: {kind, on?, cwd?, model?}; yours by default.
        pub harness: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsList {
        /// The repository, `owner/name`.
        pub repo: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsGet {
        /// Its id: `E12`.
        pub experiment: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsDecide {
        /// Its id: `E12`.
        pub experiment: String,
        /// `a+b`, `baseline`, …
        pub variant: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ExperimentsCancel {
        /// Its id: `E12`.
        pub experiment: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiStoreVerify {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiStoreGc {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiWorkflowsSchemaKind {
        Gha,
        Kiln,
        Overlay,
        Action,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWorkflowsSchema {
        pub kind: CiWorkflowsSchemaKind,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsGraph {
        /// The run's id.
        pub run: String,
        /// An attempt's (the latest by default).
        pub attempt: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsSummary {
        /// The run's id.
        pub run: String,
        pub attempt: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsAnnotations {
        /// The run's id.
        pub run: String,
        pub attempt: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsAttestations {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsPending {
        /// The run's id.
        pub run: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsArtifact {
        /// The run's id.
        pub run: String,
        /// The artifact's id.
        pub artifact: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsRepositoryDispatch {
        /// The repository, `owner/name`.
        pub repo: String,
        pub event_type: String,
        pub client_payload: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiUploadsHeads {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiUploadsPack {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The commit the pack holds: 40 hex digits.
        pub sha: String,
        /// The pack, base64.
        pub pack: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunsLocal {
        /// The repository, `owner/name`.
        pub repo: String,
        pub sha: String,
        /// The branch, tag or `refs/…` it stands for; the default branch when left out.
        pub r#ref: Option<String>,
        /// `push` unless said.
        pub event: Option<String>,
        pub payload: Option<serde_json::Map<String, serde_json::Value>>,
        pub workflows: Option<Vec<String>>,
        pub job: Option<String>,
        pub matrix: Option<serde_json::Map<String, serde_json::Value>>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
        pub vars: Option<serde_json::Map<String, serde_json::Value>>,
        pub secrets: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiLegsCompare {
        /// The leg's id (a job of a run, one matrix combination of it).
        pub leg: String,
        /// Another leg's id, or `last-green`.
        pub other: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcSubject {
        /// `org:<owner>` or `repo:<owner/name>`.
        pub scope: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcSetSubject {
        /// `org:<owner>` or `repo:<owner/name>`.
        pub scope: String,
        pub use_default: Option<bool>,
        pub include_claim_keys: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcResetSubject {
        /// `org:<owner>` or `repo:<owner/name>`.
        pub scope: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcStatus {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcJwks {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcRotate {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiOidcPublish {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiSecretsDelete {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiVariablesDelete {
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: String,
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsKinds {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsCreate {
        pub kind: String,
        pub name: String,
        pub config: Option<serde_json::Map<String, serde_json::Value>>,
        /// Stored encrypted, never shown again.
        pub secret: Option<String>,
        /// A kind's second secret (its `signing`: a Slack app's signing secret), stored as the
        /// first.
        pub signing_secret: Option<String>,
        /// Where: `server`, `org:<owner>`, `repo:<owner/name>`, `workflow:<repo>:<path>` or
        /// `env:<repo>:<environment>`.
        pub scope: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsGet {
        /// Its name.
        pub integration: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsUpdate {
        /// Its name.
        pub integration: String,
        pub config: Option<serde_json::Map<String, serde_json::Value>>,
        pub secret: Option<String>,
        pub signing_secret: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiIntegrationsDelete {
        /// Its name.
        pub integration: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiActionsList {
        pub q: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiActionsKiln {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiActionsSync {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiRunnersProbe {
        /// The runner's name.
        pub runner: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiPoolsGet {
        /// The pool.
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiPoolsDelete {
        /// The pool.
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsGet {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsDeployments {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: String,
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsDeploy {
        /// The repository, `owner/name`.
        pub repo: String,
        /// The environment.
        pub name: String,
        pub r#ref: Option<String>,
        pub workflow: Option<String>,
        pub inputs: Option<serde_json::Map<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiEnvironmentsDelete {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWebhooksDeliveries {
        pub integration: Option<String>,
        pub event: Option<String>,
        pub limit: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWebhooksDelivery {
        /// Its id.
        pub delivery: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiWebhooksRedeliver {
        /// Its id.
        pub delivery: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiHooksList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiHooksCreate {
        /// The repository, `owner/name`.
        pub repo: String,
        pub name: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiHooksDelete {
        /// The repository, `owner/name`.
        pub repo: String,
        /// Its id.
        pub hook: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiProvidersGet {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiProvidersSet {
        /// The repository, `owner/name`.
        pub repo: String,
        pub providers: Vec<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiProvidersSync {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiProvidersWorkflows {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiProvidersChecks {
        /// The repository, `owner/name`.
        pub repo: String,
        pub sha: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct HostPullsList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct HostPullsGet {
        /// The repository, `owner/name`.
        pub repo: String,
        pub number: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum HostPullsCommentSide {
        Left,
        Right,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct HostPullsComment {
        /// The repository, `owner/name`.
        pub repo: String,
        pub number: i64,
        pub body: String,
        pub path: Option<String>,
        pub line: Option<i64>,
        pub side: Option<HostPullsCommentSide>,
        pub reply_to: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum HostPullsReviewVerdict {
        Approve,
        RequestChanges,
        Comment,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct HostPullsReview {
        /// The repository, `owner/name`.
        pub repo: String,
        pub number: i64,
        pub verdict: HostPullsReviewVerdict,
        pub body: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum HostPullsMergeMethod {
        Merge,
        Squash,
        Rebase,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct HostPullsMerge {
        /// The repository, `owner/name`.
        pub repo: String,
        pub number: i64,
        pub method: Option<HostPullsMergeMethod>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsVersions {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsFiles {
        /// The diff: `D12`.
        pub diff: String,
        pub version: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum CiDiffsBlobSide {
        Old,
        New,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsBlob {
        /// The diff: `D12`.
        pub diff: String,
        pub version: i64,
        pub path: String,
        pub side: Option<CiDiffsBlobSide>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsDependencies {
        /// The diff: `D12`.
        pub diff: String,
        pub from: Option<i64>,
        pub to: Option<i64>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsInterdiff {
        /// The diff: `D12`.
        pub diff: String,
        pub from: i64,
        pub to: i64,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsReady {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsAbandon {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsReclaim {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiDiffsCommandeer {
        /// The diff: `D12`.
        pub diff: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiChangeRequestsList {
        /// The repository, `owner/name`.
        pub repo: String,
        pub state: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiChangeRequestsOpen {
        /// The repository, `owner/name`.
        pub repo: String,
        pub branch: String,
        pub title: Option<String>,
        pub body: Option<String>,
        pub trunk: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiChangeRequestsGet {
        /// Its key: `CR4`.
        pub change_request: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiReviewInbox {}

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiQueueList {
        /// The repository, `owner/name`.
        pub repo: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiQueueRemove {
        /// The entry's id.
        pub entry: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct CiStacksGet {
        /// The stack's id (a diff's `stack`).
        pub stack: String,
    }
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
        // Read as its type, once its caller may run it: what doesn't fit is refused, saying
        // where and why.
        op.input.fits(&input).map_err(|m| (StatusCode::BAD_REQUEST, m.said(name)))?;
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
        // Every field a route reads, as one input (parsed: it outgrows `json!`).
        let probe: Value = serde_json::from_str(r#"{"run": "r1", "leg": "l1", "repo": "acme/app", "path": ".github/workflows/ci.yml", "step": 0, "command": "true", "state": "approved", "minutes": 5, "content": "x", "workflow": "ci.yml", "q": "error", "job": "build", "scope": "repo:acme/app", "name": "N", "value": "v", "integration": "hook", "diff": "D1", "thread": "t1", "body": "b", "comment": "c1", "action": "accept", "cache": "c1", "kind": "gha", "artifact": "7", "event_type": "e", "sha": "0123456789abcdef0123456789abcdef01234567", "pack": "", "other": "last-green", "delivery": "d1", "hook": "h1", "number": 4, "version": 2, "from": 1, "to": 2, "change_request": "CR1", "entry": "q1", "stack": "s1", "runner": "mac", "providers": ["kiln"], "verdict": "comment", "test": "t1", "experiment": "E1", "variant": "a"}"#).unwrap_or_default();
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
            assert!(op.input.schema().get("type").is_some(), "{}", op.name);
        }
    }
}
