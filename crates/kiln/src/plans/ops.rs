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

/// A plan's steps, for agents: each its parts, and the steps it waits for.
fn items_schema() -> Value {
    json!({
        "type": "array",
        "description": "The plan's steps: each its title, what to do, examples, its acceptance criteria, its tests, and the steps it waits for (`needs`).",
        "items": {"type": "object", "properties": {
            "title": {"type": "string"},
            "description": {"type": "string"},
            "examples": {"type": "string"},
            "acceptance": criteria_schema(),
            "testing": {"type": "array", "description": "Tests: each a scenario and the criteria it passes by.", "items": {"type": "object", "properties": {"scenario": {"type": "string"}, "criteria": criteria_schema()}}},
            "needs": {"type": "array", "items": {"type": "integer", "minimum": 1}, "description": "The steps (by number, from 1) this one waits for: run with an agent per step, it starts once each is met, and steps that don't wait for each other run side by side. Left out of every step: each waits for the one before."},
            "basis": {"type": "array", "items": {"type": "string"}, "description": "The findings of the plan's research this step rests on (their ids: f1, f2 …)."}
        }, "required": ["title"]}
    })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const ID: &str = "The plan's id.";

async fn list(cx: Cx, v: Value) -> Out {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    // An agent's (made in its conversation), and the one it proposed (plan mode), when asked.
    let session = s("session").filter(|x| !x.trim().is_empty());
    let proposed = v.get("proposed").and_then(|p| p.as_bool().or_else(|| p.as_str().map(|s| s == "true" || s == "1"))).unwrap_or(false);
    let plans = super::of_source(super::list(&cx.hub, s("q"), s("tag")).await.map_err(fail)?, session.as_deref(), proposed);
    json_of(&json!({"plans": plans}))
}

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, &id_of(&v)?).await.map_err(fail)?)
}

async fn create(cx: Cx, v: Value) -> Out {
    let n: New = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let p = super::create(&cx.hub, kept_with(n, cx.caller.agent.as_deref()), &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.create".into(), p.title.clone()).await;
    json_of(&p)
}

/// A plan an agent keeps is kept with its own session, as its Plan tab lists them (docs/plans.md,
/// "Keeping one"), unless it names another.
fn kept_with(mut n: New, agent: Option<&str>) -> New {
    if let Some(agent) = agent {
        let s = n.source.get_or_insert_with(super::Source::default);
        if s.session.as_deref().is_none_or(|x| x.trim().is_empty()) {
            s.session = Some(agent.to_owned());
        }
    }
    n
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
    // The try it replaces (asked again after it was tried again, nothing more starts: CP-T3.2).
    let seen = v.get("try").and_then(Value::as_i64);
    json_of(&super::agents::retry(&cx.hub, &run, step_of(&v)?, seen, &cx.caller.name).await.map_err(fail)?)
}

fn goals_fail(e: crate::goals::Error) -> Fail {
    match e {
        crate::goals::Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        crate::goals::Error::NoAgent => (StatusCode::NOT_FOUND, "the goal's agent isn't there".into()),
        crate::goals::Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Ends a plan's planning phase (`super::ready`); a plan made from a goal then goes on as `then`
/// says: its steps the goal's parts on its agent (`goals`, the default), the same paused for a
/// person to look first (`review`), or a run with an agent per step that the goal waits for
/// (`agents`). Once every step is met, so is the goal.
async fn ready(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let p = super::get(&cx.hub, &id).await.map_err(fail)?;
    let goal = p.source.as_ref().and_then(|s| s.goal.clone());
    if let Some(g) = &goal {
        crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &g.agent).await?;
    }
    let then = v.get("then").and_then(Value::as_str).unwrap_or("goals");
    if !matches!(then, "goals" | "agents" | "review") {
        return Err((StatusCode::BAD_REQUEST, "`then` is goals, agents or review".into()));
    }
    let skipped: Option<crate::research::Skipped> =
        v.get("skipped").cloned().filter(|s| !s.is_null()).map(serde_json::from_value).transpose().map_err(|e| (StatusCode::BAD_REQUEST, format!("`skipped`: {e}")))?;
    let was_planning = p.phase == "planning";
    let (plan, said) = super::ready(&cx.hub, &id, skipped).await.map_err(fail)?;
    let mut out = json!({"plan": plan, "warnings": said.warnings});
    // Its research, to show: every claim with its sources and grade, as a person reads it.
    if let (true, Some(rid)) = (was_planning, plan.research.as_deref())
        && let Ok(r) = crate::research::get(&cx.hub, rid).await
        && let Some(o) = out.as_object_mut()
    {
        o.insert("research".into(), json!(crate::research::view(&r, true).markdown));
        o.insert(
            "say".into(),
            json!("Show the person this research now, in your reply, as it is: every finding with its grade, score and sources, and the sources numbered; then the warnings, if any. Then carry on with the plan's first step."),
        );
    }
    if let (Some(g), true) = (goal, was_planning) {
        let by = crate::goals::By::of(&cx.hub, &cx.caller, &g.agent);
        let info = cx.hub.harness.info(&g.agent).ok_or_else(|| (StatusCode::NOT_FOUND, "the goal's agent has ended: run the plan another way (plans_run)".to_owned()))?;
        let (next, run) = if then == "agents" {
            let place = super::agents::Place {
                on: Some(crate::harness::wait::machine(&info.target).to_owned()),
                kind: Some(info.kind.clone()),
                cwd: Some(info.cwd.clone()),
                ..super::agents::Place::default()
            };
            let cfg = super::agents::Config {
                place,
                main: Some(g.agent.clone()),
                parent: Some(g.agent.clone()),
                opener: cx.caller.opener(),
                actor: cx.caller.name.clone(),
                ..super::agents::Config::default()
            };
            let started = super::agents::start(&cx.hub, &plan, cfg, None).await.map_err(fail)?;
            (crate::goals::Then::Run(started.run.clone()), started.run)
        } else {
            let run = super::note_run(&cx.hub, &plan, &g.agent, "goals", &cx.caller.name).await.map_err(fail)?;
            (if then == "review" { crate::goals::Then::Review } else { crate::goals::Then::Goals }, run)
        };
        let stack = crate::goals::planned(&cx.hub, &g.agent, &g.id, &plan, &run, &next, &by).await.map_err(goals_fail)?;
        if let Some(o) = out.as_object_mut() {
            o.insert("then".into(), json!(then));
            o.insert("goal".into(), json!(g.id));
            o.insert("run".into(), json!(run));
            o.insert("stack".into(), serde_json::to_value(&stack).unwrap_or(Value::Null));
        }
    }
    cx.hub.store.audit(cx.caller.name.clone(), "plan.ready".into(), format!("{} (v{})", plan.title, plan.version)).await;
    Ok(out)
}

async fn versions(cx: Cx, v: Value) -> Out {
    json_of(&json!({"versions": super::versions(&cx.hub, &id_of(&v)?).await.map_err(fail)?}))
}

/// How a run of one agent per step runs, from a `plans.run` input, for `caller` (an agent that
/// runs it is its steps' agents' parent, and the run's main agent unless `session` names another).
pub fn agents_config(v: &Value, caller: &crate::authz::Caller) -> Result<super::agents::Config, Fail> {
    let n = v.get("new").cloned().unwrap_or(Value::Null);
    let s = |k: &str| n.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let place = super::agents::Place { on: s("on").or_else(|| s("target")), kind: s("kind"), cwd: s("cwd"), model: s("model"), effort: s("effort"), mode: s("mode"), agent: s("agent") };
    let steps: Vec<super::agents::Place> = serde_json::from_value(v.get("steps").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("`steps`: {e}")))?;
    let parallel = v.get("parallel").and_then(Value::as_bool).unwrap_or(false);
    let max_parallel = match v.get("max_parallel").filter(|m| !m.is_null()) {
        None => None,
        Some(m) => Some(m.as_u64().filter(|m| *m > 0).and_then(|m| usize::try_from(m).ok()).ok_or_else(|| (StatusCode::BAD_REQUEST, "`max_parallel` is a number of steps, 1 or more".to_owned()))?),
    };
    let main = v.get("session").and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).or_else(|| caller.agent.clone());
    Ok(super::agents::Config { place, steps, parallel, max_parallel, main, parent: caller.agent.clone(), opener: caller.opener(), actor: caller.name.clone(), only: only_of(v)? })
}

/// `only`: the steps a run takes, by their numbers from 1 (as from 0).
fn only_of(v: &Value) -> Result<Option<Vec<usize>>, Fail> {
    let Some(o) = v.get("only").filter(|o| !o.is_null()) else { return Ok(None) };
    let bad = || (StatusCode::BAD_REQUEST, "`only` is a list of step numbers, from 1".to_owned());
    let list = o.as_array().ok_or_else(bad)?;
    let n: Vec<usize> = list.iter().map(|n| n.as_u64().filter(|n| *n >= 1).and_then(|n| usize::try_from(n - 1).ok()).ok_or_else(bad)).collect::<Result<_, _>>()?;
    if n.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "`only` names no steps: leave it out to run every step".into()));
    }
    Ok(Some(n))
}

/// `v`'s idempotency `key` (CP-T3.2), scoped to whose it is (`harness::ops::key_scope`): asked for
/// again by the same caller, wherever its agent has moved since, it's the same run.
fn scoped_key(cx: &Cx, v: &Value) -> Result<Option<String>, crate::ops::Fail> {
    Ok(crate::harness::ops::key_of(v)?.map(|k| format!("{}\u{0}{k}", crate::harness::ops::key_scope(&cx.caller))))
}

async fn run(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let mode: Mode = v.get("mode").cloned().map(serde_json::from_value).transpose().map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?.unwrap_or_default();
    if mode == Mode::Agents {
        let p = super::get(&cx.hub, &id).await.map_err(fail)?;
        let mut cfg = agents_config(&v, &cx.caller)?;
        // Steps given to agents that are there already (the main agent, or one by id) go on their
        // goal stacks: an agent gives steps only to itself and the agents it started.
        let named = cfg.steps.iter().chain(std::iter::once(&cfg.place)).filter_map(|s| s.agent.as_deref().map(str::trim)).filter(|a| !a.is_empty());
        for a in named.collect::<Vec<_>>() {
            let to = if matches!(a, "main" | "self" | "me") { cfg.main.clone() } else { Some(a.to_owned()) };
            if let Some(to) = to {
                crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &to).await?;
            }
        }
        // An agent's steps run in its own harness unless it says which (and, being its children,
        // on its machine and in its folder).
        if cfg.place.kind.as_deref().is_none_or(|k| k.trim().is_empty())
            && let Some(me) = cx.caller.agent.as_deref().and_then(|me| cx.hub.harness.info(me))
        {
            cfg.place.kind = Some(me.kind);
        }
        let key = scoped_key(&cx, &v)?;
        let started = super::agents::start(&cx.hub, &p, cfg, key.as_deref()).await.map_err(fail)?;
        cx.hub.store.audit(cx.caller.name.clone(), "plan.run".into(), format!("{id}: one agent per step ({} steps), run {}", started.steps, started.run)).await;
        return json_of(&started);
    }
    let target = match (v.get("session").and_then(Value::as_str), v.get("new")) {
        // Running it on an agent that runs is steering that agent: an agent steers only itself and
        // the agents it started.
        (Some(s), _) => {
            crate::harness::lineage::may_steer(&cx.hub, &cx.caller, s).await?;
            Target::Session(s.to_owned())
        }
        (None, Some(n)) => {
            let s = |k: &str| n.get(k).and_then(Value::as_str).map(str::to_owned);
            Target::New { target: s("target").unwrap_or_else(|| "local".into()), kind: s("kind").ok_or_else(|| (StatusCode::BAD_REQUEST, "`new.kind` is required".to_owned()))?, cwd: s("cwd") }
        }
        // An agent that says neither runs it itself.
        (None, None) => match cx.caller.agent.as_deref() {
            Some(me) => Target::Session(me.to_owned()),
            None => return Err((StatusCode::BAD_REQUEST, "say where to run it: `session`, or `new` {target, kind, cwd}".into())),
        },
    };
    let key = scoped_key(&cx, &v)?;
    let pick = super::Pick { only: only_of(&v)?, split: v.get("split").and_then(Value::as_bool).unwrap_or(false) };
    let ran = super::run(&cx.hub, &id, target, mode, &pick, &crate::harness::ops::starter(&cx)?, key.as_deref()).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "plan.run".into(), format!("{id} on {} ({} messages)", ran.session, ran.sent)).await;
    json_of(&ran)
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "plans.list",
            summary: "Plans kept in kiln, newest change first; `q` matches title, text or tags, `tag` keeps one tag, `session` an agent's (made in its conversation), and `proposed` the one each agent proposed (plan mode), kept by kiln.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"q": {"type": "string"}, "tag": {"type": "string"}, "session": {"type": "string"}, "proposed": {"type": "boolean"}}), &[]),
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
            summary: "Keep a plan in kiln (the plan you made, for the person to change, share and run later), with your session (its Plan tab lists it): its text and/or its steps (`items`: each with description, examples, acceptance criteria, testing); without them, its text's top-level list items are its steps.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "title": {"type": "string", "description": "Short; taken from the first heading when left out."},
                        "body": {"type": "string", "description": "The plan's text, in markdown (optional with items)."},
                        "items": items_schema(),
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
            input: || schema(json!({"id": {"type": "string", "description": ID}, "title": {"type": "string"}, "body": {"type": "string"}, "items": items_schema(), "tags": {"type": "array", "items": {"type": "string"}}}), &["id"]),
            route: |v| Ok((Method::PATCH, path(&["api", "plans", &id_of(v)?]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "plans.ready",
            summary: "Say a plan in its planning phase (one made from a goal: goals_plan) is ready: it has steps, its research has sources inside and outside (or `skipped` says why not), and each step rests only on findings of its research (`basis`). What's weak comes back as warnings: say them. A plan made from a goal then goes on as `then` says: `goals` (the default: its steps become the goal's parts on its agent, each splittable into its acceptance criteria and tests), `review` (the same, the stack paused for the person to look at the plan first), or `agents` (it runs with an agent per step, as its `needs` say; the goal waits for the run). Once every step is met, so is the goal.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "id": {"type": "string", "description": ID},
                        "then": {"type": "string", "enum": ["goals", "review", "agents"]},
                        "skipped": {"type": "object", "properties": {"internal": {"type": "string"}, "external": {"type": "string"}}, "description": "Why its research has no internal, or no external, sources."}
                    }),
                    &["id"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "plans", &id_of(v)?, "ready"]))),
            run: |cx, v| Box::pin(ready(cx, v)),
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
            summary: "Run a plan, or some of its steps (`only`), on an agent: `session` (one that runs) or `new` {target: local|agent:<machine>, kind, cwd}; `mode` message (the whole plan), steps (each step a queued message), goals (its steps become goals on that agent's goal stack; `split`: each step a goal stack of its acceptance criteria and tests), or agents (each step held to as a goal by an agent of its own, or by the main agent, as its steps' `needs` say: steps that don't wait for each other run side by side; plans.runs.get follows it). An agent that gives neither `session` nor `new` runs it itself; one agent per step, its steps' agents start in its harness, on its machine and in its folder unless it says otherwise, and a step given to `main` (in `steps`, {agent: \"main\"}) goes on its own goal stack instead.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "id": {"type": "string", "description": ID},
                        "session": {"type": "string", "description": "The agent it runs on; agents: the main agent, that steps given to `main` go to (left out: you)."},
                        "new": {"type": "object", "properties": {"target": {"type": "string"}, "on": {"type": "string"}, "kind": {"type": "string"}, "cwd": {"type": "string"}, "model": {"type": "string"}, "effort": {"type": "string"}, "mode": {"type": "string"}}},
                        "mode": {"type": "string", "enum": ["message", "steps", "goals", "agents"], "description": "message: the whole plan at once; steps: a message a turn; goals: each step a goal in the agent's goal stack; agents: each step held to as a goal by an agent of its own (with `new`: on, kind, cwd, model, effort, mode) or by the main agent, each starting once the steps it waits for are met."},
                        "steps": {"type": "array", "description": "agents: where and how each step runs, over `new` (by its place in the list): {on, kind, cwd, model, effort, mode}, or {agent: \"main\"} (or an agent's id) to give the step to that agent: its goal goes on that agent's goal stack, and no agent is started for it.", "items": {"type": "object"}},
                        "parallel": {"type": "boolean", "description": "agents: in a plan whose steps don't say what they wait for (`needs`), run them all side by side rather than each once the one before is met. Side by side in one folder, each step's agent keeps to its own files; give steps folders or machines of their own with `steps`."},
                        "max_parallel": {"type": "integer", "minimum": 1, "description": "agents: steps at work at once, at the most (left out: every step that's ready)."},
                        "only": {"type": "array", "items": {"type": "integer", "minimum": 1}, "description": "Only these steps, by their numbers from 1 (left out: every step); each keeps its number. The steps they wait for outside them are taken as done."},
                        "split": {"type": "boolean", "description": "goals: each step goes on the stack as a goal stack of its own: its acceptance criteria and tests, a goal each, before it."},
                        "key": {"type": "string", "description": "An idempotency key of yours, made once for this run (1 to 256 bytes). Run again with it (its answer lost), it's the same run, in every mode: its messages go once, its steps' agents start once, and it's answered as it was. Without it, each call is another run: pass it whenever you may ask again."}
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
            input: || schema(json!({"run": {"type": "string"}, "step": {"type": "integer", "minimum": 1, "description": "The step's number, from 1."}, "try": {"type": "integer", "description": "The try it replaces: the step's `tries` as plans_runs_get shows them. Asked again (its answer lost) once the step was tried again, nothing more starts. Without it, each call starts another agent: pass it."}}), &["run", "step"]),
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

/// Where a plan is kept: with the session of the agent that keeps it.
#[cfg(test)]
mod kept {
    use super::kept_with;
    use crate::plans::{New, Source};

    fn session(n: &New) -> Option<&str> {
        n.source.as_ref().and_then(|s| s.session.as_deref())
    }

    #[test]
    fn an_agent_s_plan_is_kept_with_its_session() {
        assert_eq!(session(&kept_with(New::default(), Some("a1"))), Some("a1"));
        let blank = New { source: Some(Source { session: Some(" ".into()), harness: Some("claude".into()), ..Source::default() }), ..New::default() };
        let n = kept_with(blank, Some("a1"));
        assert_eq!(session(&n), Some("a1"));
        assert_eq!(n.source.and_then(|s| s.harness).as_deref(), Some("claude"), "what else it says stays");
    }

    #[test]
    fn a_session_named_stays_and_a_person_s_plan_is_as_given() {
        let named = New { source: Some(Source { session: Some("a2".into()), ..Source::default() }), ..New::default() };
        assert_eq!(session(&kept_with(named, Some("a1"))), Some("a2"));
        assert_eq!(kept_with(New::default(), None).source, None);
    }
}
