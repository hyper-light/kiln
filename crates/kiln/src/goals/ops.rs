//! The goal stack operations (`goals.*`), in kiln's one operation table (`cd::ops`): the MCP tools
//! agents call (to line up goals for themselves or another agent) and code mode's `kiln.goals.*`.
//! The console's `/api/harness/sessions/:id/goals` routes call the same functions.

use http::StatusCode;
use serde_json::{Value, json};

use super::{Action, Error, Given};
use crate::cd::ops::{Cx, Fail, Op, Out};

fn fail(e: Error) -> Fail {
    match e {
        Error::Bad(m) => (StatusCode::BAD_REQUEST, m),
        Error::NoAgent => (StatusCode::NOT_FOUND, "no such agent".into()),
        Error::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn agent_of(v: &Value) -> Result<String, Fail> {
    v.get("agent").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| (StatusCode::BAD_REQUEST, "`agent` is required".into()))
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const AGENT: &str = "The agent session's id (kiln's, as `harness.sessions` lists them).";

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, &agent_of(&v)?).await.map_err(fail)?)
}

async fn add(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    let goals: Vec<String> = v.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if goals.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "`goals` is a list of at least one goal".into()));
    }
    let start = v.get("start").and_then(Value::as_bool).unwrap_or(true);
    let s = super::add(&cx.hub, &agent, goals, start).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "goals.add".into(), agent).await;
    json_of(&s)
}

async fn set(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    let given: Vec<Given> = serde_json::from_value(v.get("goals").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("`goals`: {e}")))?;
    let s = super::set(&cx.hub, &agent, given).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "goals.set".into(), agent).await;
    json_of(&s)
}

async fn control(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    let action: Action = serde_json::from_value(v.get("action").cloned().unwrap_or(Value::Null)).map_err(|_| (StatusCode::BAD_REQUEST, "`action` is start, pause, skip or clear".into()))?;
    let s = super::control(&cx.hub, &agent, action).await.map_err(fail)?;
    cx.hub.store.audit(cx.actor.clone(), "goals.control".into(), format!("{agent}: {action:?}")).await;
    json_of(&s)
}

/// The `goals.*` operations.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "goals.get",
            summary: "An agent's goal stack: its goals in order (pending, active, met, unmet, skipped), and whether it runs, is paused (and why) or is done.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "goals.add",
            summary: "Line goals up for an agent, after the ones it has: each goes to it once the one before is met (its harness's own /goal where it has one), like giving it /goal again and again. Runs the stack unless `start` is false.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {"type": "array", "items": {"type": "string"}, "description": "Each a condition or objective, one line."},
                        "start": {"type": "boolean"}
                    }),
                    &["agent", "goals"],
                )
            },
            run: |cx, v| Box::pin(add(cx, v)),
        },
        Op {
            name: "goals.set",
            summary: "Replace an agent's goals not yet started (reordered, edited, removed, added): `goals` is the list as it should be, existing ones by `id`.",
            read_only: false,
            destructive: true,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string"}}, "required": ["text"]}}
                    }),
                    &["agent", "goals"],
                )
            },
            run: |cx, v| Box::pin(set(cx, v)),
        },
        Op {
            name: "goals.control",
            summary: "Start (or resume), pause, skip the goal under way, or clear an agent's goal stack.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}, "action": {"type": "string", "enum": ["start", "pause", "skip", "clear"]}}), &["agent", "action"]),
            run: |cx, v| Box::pin(control(cx, v)),
        },
    ]
}
