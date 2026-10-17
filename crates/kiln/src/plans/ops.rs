//! The plans operations (`plans.*`), in kiln's one operation table (`cd::ops`): the MCP tools
//! agents call (`plans_create` to keep the plan they made, `plans_get` to take one up) and code
//! mode's `kiln.plans.*`. The console's `/api/plans` routes call the same functions.

use http::StatusCode;
use serde_json::{Value, json};

use super::{Error, Mode, New, Patch, Target};
use crate::cd::ops::{Cx, Fail, Op, Out};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::Missing => (StatusCode::NOT_FOUND, "no such plan".into()),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn id_of(v: &Value) -> Result<String, Fail> {
    v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, "`id` is required".into()))
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
    let p = super::create(&cx.hub, n, &cx.actor).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "plan.create".into(), p.title.clone()).await;
    json_of(&p)
}

async fn update(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let patch: Patch = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let p = super::update(&cx.hub, &id, patch, &cx.actor).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "plan.update".into(), format!("{} (v{})", p.title, p.version)).await;
    json_of(&p)
}

async fn delete(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    super::delete(&cx.hub, &id).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "plan.delete".into(), id).await;
    Ok(json!({"deleted": true}))
}

async fn versions(cx: Cx, v: Value) -> Out {
    json_of(&json!({"versions": super::versions(&cx.hub, &id_of(&v)?).await.map_err(fail)?}))
}

async fn run(cx: Cx, v: Value) -> Out {
    let id = id_of(&v)?;
    let mode: Mode = v.get("mode").cloned().map(serde_json::from_value).transpose().map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?.unwrap_or_default();
    let target = match (v.get("session").and_then(Value::as_str), v.get("new")) {
        (Some(s), _) => Target::Session(s.to_owned()),
        (None, Some(n)) => {
            let s = |k: &str| n.get(k).and_then(Value::as_str).map(str::to_owned);
            Target::New { target: s("target").unwrap_or_else(|| "local".into()), kind: s("kind").ok_or_else(|| (StatusCode::BAD_REQUEST, "`new.kind` is required".to_owned()))?, cwd: s("cwd") }
        }
        (None, None) => return Err((StatusCode::BAD_REQUEST, "say where to run it: `session`, or `new` {target, kind, cwd}".into())),
    };
    let ran = super::run(&cx.hub, &id, target, mode, &cx.actor).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "plan.run".into(), format!("{id} on {} ({} messages)", ran.session, ran.sent)).await;
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
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "plans.get",
            summary: "One plan: its title, markdown and steps (the items of its top-level lists).",
            read_only: true,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "plans.create",
            summary: "Keep a plan in kiln (the plan you made, for the person to change, share and run later). Steps are its top-level list items.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "title": {"type": "string", "description": "Short; taken from the first heading when left out."},
                        "body": {"type": "string", "description": "The plan, in markdown."},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "source": {"type": "object", "description": "Where it came from: {session, harness, machine, cwd, agent}."}
                    }),
                    &["body"],
                )
            },
            run: |cx, v| Box::pin(create(cx, v)),
        },
        Op {
            name: "plans.update",
            summary: "Change a plan's title, text or tags; a new title or text is a new version.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}, "title": {"type": "string"}, "body": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}), &["id"]),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "plans.versions",
            summary: "Every version of a plan, newest first.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            run: |cx, v| Box::pin(versions(cx, v)),
        },
        Op {
            name: "plans.run",
            summary: "Run a plan on an agent: `session` (one that runs) or `new` {target: local|agent:<machine>, kind, cwd}; `mode` message (the whole plan) or steps (each step a queued message).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "id": {"type": "string", "description": ID},
                        "session": {"type": "string"},
                        "new": {"type": "object", "properties": {"target": {"type": "string"}, "kind": {"type": "string"}, "cwd": {"type": "string"}}},
                        "mode": {"type": "string", "enum": ["message", "steps", "goals"], "description": "message: the whole plan at once; steps: a message a turn; goals: each step a goal in the agent's goal stack."}
                    }),
                    &["id"],
                )
            },
            run: |cx, v| Box::pin(run(cx, v)),
        },
        Op {
            name: "plans.delete",
            summary: "Delete a plan, its versions and its runs.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"id": {"type": "string", "description": ID}}), &["id"]),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
    ]
}
