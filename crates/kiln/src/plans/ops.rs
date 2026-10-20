//! The plans operations (`plans.*`), in kiln's one operation table (`crate::ops::own`): the MCP
//! tools agents call (`plans_create` to keep the plan they made, `plans_get` to take one up) and
//! code mode's `kiln.plans.*`. Each is one of the console's `/api/plans` routes, which call the
//! same functions.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Error, Mode, New, Patch, Target};
use crate::ops::{Cx, Fail, Op, Out, path};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such plan".into()),
        Error::Refused(c, m) => (c, m),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn id_of(v: &Value) -> Result<String, Fail> {
    v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, "`id` is required".into()))
}

/// A step's criteria, for agents: each in words, or a measure against a threshold.
fn criteria_schema() -> Value {
    json!({
        "type": "array",
        "description": "Criteria: {kind: \"qualitative\", text} in words, or {kind: \"quantitative\", target (what's measured, e.g. \"p99.9 latency\"), op (lt, le, eq, ne, ge, gt, between, within, rises_at_least, falls_at_least, rises_at_most, falls_at_most), value (a number; for rises/falls a percentage), value2 (between: the upper bound; within: the tolerance), unit (\"ms\", \"%\"), baseline (rises/falls: compared with what), measured (how it's measured)}.",
        "items": {"type": "object", "properties": {"kind": {"type": "string", "enum": ["qualitative", "quantitative"]}}, "required": ["kind"]}
    })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const ID: &str = "The plan's id.";

async fn list(cx: Cx, v: Value) -> Out {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    json_of(&json!({"plans": super::list(&cx.hub, s("q"), s("tag")).await.map_err(fail)?}))
}

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, &id_of(&v)?).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let n: New = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let p = super::create(&cx.hub, n, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.create".into(), p.title.clone()).await;
    json_of(&p)
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let patch: Patch = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let p = super::update(&cx.hub, &id, patch, &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.update".into(), format!("{} (v{})", p.title, p.version)).await;
    json_of(&p)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    super::delete(&cx.hub, &id).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.delete".into(), id).await;
    Ok(json!({"deleted": true}))
}

fn run_of(v: &Value) -> Result<String, Fail> {
    v.get("run").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, "`run` is required".into()))
}

fn step_of(v: &Value) -> Result<usize, Fail> {
    v.get("step")
        .and_then(|s| s.as_u64().or_else(|| s.as_str().and_then(|t| t.parse().ok())))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "`step` is the step's number, from 1".into()))
}

async fn runs_list(cx: Cx, v: Value) -> Out {
    let active = v.get("active").is_some_and(|a| a.as_bool().unwrap_or_else(|| matches!(a.as_str(), Some("1" | "true"))));
    json_of(&json!({"runs": super::agents::list(&cx.hub, active).await.map_err(fail)?}))
}

async fn runs_get(cx: Cx, v: Value) -> Out {
    json_of(&super::agents::view(&cx.hub, &run_of(&v)?).await.map_err(fail)?)
}

/// Whether `cx`'s caller may stop or retry plan run `run` (its steps' agents are steered): a person
/// any run; an agent only the runs it ran, or that the agents it started ran.
async fn may_steer_run(cx: &Cx, run: &str) -> Result<(), Fail> {
    let Some(me) = cx.caller.agent.as_deref() else { return Ok(()) };
    let ran_by = super::agents::view(&cx.hub, run).await.map_err(fail)?.config.parent;
    match ran_by.as_deref() {
        Some(p) if p == me || crate::harness::lineage::descends(&cx.hub.harness.list(), p, me) => Ok(()),
        _ => Err((StatusCode::FORBIDDEN, "an agent stops and retries only the plan runs it ran (or the agents it started ran)".to_owned())),
    }
}

async fn runs_stop(cx: Cx, v: Value) -> Out {
    let run = run_of(&v)?;
    may_steer_run(&cx, &run).await?;
    json_of(&super::agents::stop(&cx.hub, &run, &cx.caller.name).await.map_err(fail)?)
}

async fn runs_retry(cx: Cx, v: Value) -> Out {
    let run = run_of(&v)?;
    may_steer_run(&cx, &run).await?;
    json_of(&super::agents::retry(&cx.hub, &run, step_of(&v)?, &cx.caller.name).await.map_err(fail)?)
}

async fn versions(cx: Cx, v: Value) -> Out {
    json_of(&json!({"versions": super::versions(&cx.hub, &id_of(&v)?).await.map_err(fail)?}))
}

/// How a run of one agent per step runs, from a `plans.run` input, for `caller` (an agent that
/// runs it is its steps' agents' parent).
pub fn agents_config(v: &Value, caller: &crate::authz::Caller) -> Result<super::agents::Config, Fail> {
    let n = v.get("new").cloned().unwrap_or(Value::Null);
    let s = |k: &str| n.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let place = super::agents::Place { on: s("on").or_else(|| s("target")), kind: s("kind"), cwd: s("cwd"), model: s("model"), effort: s("effort"), mode: s("mode") };
    let steps: Vec<super::agents::Place> = serde_json::from_value(v.get("steps").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("`steps`: {e}")))?;
    let parallel = v.get("parallel").and_then(Value::as_bool).unwrap_or(false);
    Ok(super::agents::Config { place, steps, parallel, parent: caller.agent.clone(), opener: caller.opener(), actor: caller.name.clone() })
}

async fn run(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let mode: Mode = v.get("mode").cloned().map(serde_json::from_value).transpose().map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?.unwrap_or_default();
    if mode == Mode::Agents {
        if v.get("session").is_some() {
            return Err((StatusCode::BAD_REQUEST, "one agent per step starts agents of its own: say where with `new` {on, kind, cwd}, not `session`".into()));
        }
        let p = super::get(&cx.hub, &id).await.map_err(fail)?;
        let started = super::agents::start(&cx.hub, &p, agents_config(&v, &cx.caller)?).await.map_err(fail)?;
        cx.hub.store.audit(cx.caller.name.clone(), "plan.run".into(), format!("{id}: one agent per step ({} steps), run {}", started.steps, started.run)).await;
        return json_of(&started);
    }
    let target = match (v.get("session").and_then(Value::as_str), v.get("new")) {
        // Running it on an agent that runs is steering that agent: an agent steers only itself and
        // the agents it started.
        (Some(s), _) => {
            crate::harness::lineage::may_steer(&cx.hub, &cx.caller, s)?;
            Target::Session(s.to_owned())
        }
        (None, Some(n)) => {
            let s = |k: &str| n.get(k).and_then(Value::as_str).map(str::to_owned);
            Target::New { target: s("target").unwrap_or_else(|| "local".into()), kind: s("kind").ok_or_else(|| (StatusCode::BAD_REQUEST, "`new.kind` is required".to_owned()))?, cwd: s("cwd") }
        }
        (None, None) => return Err((StatusCode::BAD_REQUEST, "say where to run it: `session`, or `new` {target, kind, cwd}".into())),
    };
    let ran = super::run(&cx.hub, &id, target, mode, &crate::harness::ops::starter(&cx)?).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.run".into(), format!("{id} on {} ({} messages)", ran.session, ran.sent)).await;
    json_of(&ran)
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "plans.list",
            summary: "Plans kept in kiln, newest change first; `q` matches title, text or tags, `tag` keeps one tag.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"q": {"type": "string"}, "tag": {"type": "string"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "plans"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "plans.get",
            summary: "One plan: its title, markdown and steps (the items of its top-level lists).",
            read_only: true,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            route: |v| Ok((Method::GET, path(&["api", "plans", &id_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "plans.create",
            summary: "Keep a plan in kiln (the plan you made, for the person to change, share and run later): its text and/or its steps (`items`: each with description, examples, acceptance criteria, testing); without them, its text's top-level list items are its steps.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "title": {"type": "string", "description": "Short; taken from the first heading when left out."},
                        "body": {"type": "string", "description": "The plan's text, in markdown (optional with items)."},
                        "items": {"type": "array", "description": "The plan's steps: each its title, what to do, examples, its acceptance criteria and its tests.", "items": {"type": "object", "properties": {"title": {"type": "string"}, "description": {"type": "string"}, "examples": {"type": "string"}, "acceptance": criteria_schema(), "testing": {"type": "array", "description": "Tests: each a scenario and the criteria it passes by.", "items": {"type": "object", "properties": {"scenario": {"type": "string"}, "criteria": criteria_schema()}}}}, "required": ["title"]}},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "source": {"type": "object", "description": "Where it came from: {session, harness, machine, cwd, agent}."}
                    }),
                    &[],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "plans"]))),
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "plans.update",
            summary: "Change a plan's title, text or tags; a new title or text is a new version.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}, "title": {"type": "string"}, "body": {"type": "string"}, "items": {"type": "array", "description": "The plan's steps: each its title, what to do, examples, its acceptance criteria and its tests.", "items": {"type": "object", "properties": {"title": {"type": "string"}, "description": {"type": "string"}, "examples": {"type": "string"}, "acceptance": criteria_schema(), "testing": {"type": "array", "description": "Tests: each a scenario and the criteria it passes by.", "items": {"type": "object", "properties": {"scenario": {"type": "string"}, "criteria": criteria_schema()}}}}, "required": ["title"]}}, "tags": {"type": "array", "items": {"type": "string"}}}), &["id"]),
            route: |v| Ok((Method::PATCH, path(&["api", "plans", &id_of(v)?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "plans.versions",
            summary: "Every version of a plan, newest first.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            route: |v| Ok((Method::GET, path(&["api", "plans", &id_of(v)?, "versions"]))),
            run: |cx, v| Box::pin(versions(cx, v)),
        },
        Op {
            name: "plans.run",
            summary: "Run a plan on an agent: `session` (one that runs) or `new` {target: local|agent:<machine>, kind, cwd}; `mode` message (the whole plan), steps (each step a queued message), goals (each step a goal), or agents (each step an agent of its own, one after another: then plans.runs.get follows it).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "id": {"type": "string", "description": ID},
                        "session": {"type": "string"},
                        "new": {"type": "object", "properties": {"target": {"type": "string"}, "on": {"type": "string"}, "kind": {"type": "string"}, "cwd": {"type": "string"}, "model": {"type": "string"}, "effort": {"type": "string"}, "mode": {"type": "string"}}},
                        "mode": {"type": "string", "enum": ["message", "steps", "goals", "agents"], "description": "message: the whole plan at once; steps: a message a turn; goals: each step a goal in the agent's goal stack; agents: each step an agent of its own, held to the step as a goal, the next starting once the one before is met (with `new`: on, kind, cwd, model, effort, mode)."},
                        "steps": {"type": "array", "description": "agents: where and how each step runs, over `new` (by its place in the list): {on, kind, cwd, model, effort, mode}.", "items": {"type": "object"}},
                        "parallel": {"type": "boolean", "description": "agents: start the steps together. In one plain folder they still run one at a time (the run says why) until agents have workspaces of their own."}
                    }),
                    &["id"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "plans", &id_of(v)?, "run"]))),
            run: |cx, v| Box::pin(run(cx, v)),
        },
        Op {
            name: "plans.runs.list",
            summary: "Plan runs of one agent per step, newest first, each with its plan's title, how it stands (running, stopped, done) and how many of its steps are met; `active` for those under way.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"active": {"type": "boolean"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "plans", "runs"]))),
            run: |cx, v| Box::pin(runs_list(cx, v)),
        },
        Op {
            name: "plans.runs.get",
            summary: "A plan run of one agent per step: each step with its goal, its agent and where it stands (waiting, working, needs_you, yours, met, unmet, stopped), what it changed, and the run's state and why.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"run": {"type": "string", "description": "The run's id (plans.run gives it)."}}), &["run"]),
            route: |v| Ok((Method::GET, path(&["api", "plans", "runs", &run_of(v)?]))),
            run: |cx, v| Box::pin(runs_get(cx, v)),
        },
        Op {
            name: "plans.runs.stop",
            summary: "Stop a plan run: no next step starts, and the agents of the steps under way are interrupted and no longer held to their goals.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"run": {"type": "string"}}), &["run"]),
            route: |v| Ok((Method::POST, path(&["api", "plans", "runs", &run_of(v)?, "stop"]))),
            run: |cx, v| Box::pin(runs_stop(cx, v)),
        },
        Op {
            name: "plans.runs.retry",
            summary: "Try a step of a plan run again with a new agent (its agent before, if it still runs, ends), the run going on from there.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"run": {"type": "string"}, "step": {"type": "integer", "minimum": 1, "description": "The step's number, from 1."}}), &["run", "step"]),
            route: |v| Ok((Method::POST, path(&["api", "plans", "runs", &run_of(v)?, "steps", &step_of(v)?.to_string(), "retry"]))),
            run: |cx, v| Box::pin(runs_retry(cx, v)),
        },
        Op {
            name: "plans.delete",
            summary: "Delete a plan, its versions and its runs.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            route: |v| Ok((Method::DELETE, path(&["api", "plans", &id_of(v)?]))),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
    ]
}
