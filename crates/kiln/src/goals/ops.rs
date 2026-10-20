//! The goal stack operations (`goals.*`), in kiln's one operation table (`crate::ops::own`): the
//! MCP tools agents call (to line up goals for themselves or the agents they started) and code mode's
//! `kiln.goals.*`. Each is one of the console's `/api/harness/sessions/:id/goals` routes, which call
//! the same functions.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Action, Error, Given};
use crate::ops::{Cx, Fail, Op, Out, path};

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

const AGENT: &str = "The agent's id (as agents_list gives it); leave it out for your own goal stack.";

async fn get(cx: Cx, v: Value) -> Out {
    json_of(&super::get(&cx.hub, &agent_of(&v)?).await.map_err(fail)?)
}

async fn add(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let goals: Vec<String> = v.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    if goals.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "`goals` is a list of at least one goal".into()));
    }
    let start = v.get("start").and_then(Value::as_bool).unwrap_or(true);
    let s = super::add(&cx.hub, &agent, goals, start).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.add".into(), agent).await;
    json_of(&s)
}

async fn set(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let given: Vec<Given> = serde_json::from_value(v.get("goals").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("`goals`: {e}")))?;
    let s = super::set(&cx.hub, &agent, given).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.set".into(), agent).await;
    json_of(&s)
}

async fn control(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let action: Action = serde_json::from_value(v.get("action").cloned().unwrap_or(Value::Null)).map_err(|_| (StatusCode::BAD_REQUEST, "`action` is start, pause, skip or clear".into()))?;
    let s = super::control(&cx.hub, &agent, action).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.control".into(), format!("{agent}: {action:?}")).await;
    json_of(&s)
}

/// The `goals.*` operations.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "goals.get",
            summary: "An agent's goal stack (yours, without `agent`): its goals in order (pending, active, met, unmet, skipped), and whether it runs, is paused (and why) or is done. The goal under way is sent again whenever the agent stops short of it, until it's met; an agent without goals of its own says it's met by ending its reply with the line GOAL MET.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &[]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "sessions", &agent_of(v)?, "goals"]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "goals.add",
            summary: "Line goals up for an agent (for yourself, without `agent`), after the ones it has: each goes to it once the one before is met (its harness's own /goal where it has one), like giving it /goal again and again. Runs the stack unless `start` is false. To pursue a document or list as a goal stack, give one goal per item, in order, each saying what done is.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {"type": "array", "items": {"type": "string"}, "description": "Each a condition or objective, one line."},
                        "start": {"type": "boolean"}
                    }),
                    &["goals"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals"]))),
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
                    &["goals"],
                )
            },
            route: |v| Ok((Method::PUT, path(&["api", "harness", "sessions", &agent_of(v)?, "goals"]))),
            run: |cx, v| Box::pin(set(cx, v)),
        },
        Op {
            name: "goals.control",
            summary: "Start (or resume), pause, skip the goal under way, or clear an agent's goal stack.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}, "action": {"type": "string", "enum": ["start", "pause", "skip", "clear"]}}), &["action"]),
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", v.get("action").and_then(Value::as_str).unwrap_or("start")]))),
            run: |cx, v| Box::pin(control(cx, v)),
        },
    ]
}

/// The steering rule on a hub of its own, under a timeout: an agent changes the goals of itself and
/// the agents it started, never another's; runs a plan on those only; and a plan run on a new agent
/// makes that agent its child, held to its mode (docs/agents.md, "No more than its parent"). No
/// harness runs (each agent's is a program that isn't there).
#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::harness::lineage::{Ask, Starter, start};
    use crate::ops::{Cx, call};

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("it hung"),
        }
    }

    #[test]
    fn an_agent_steers_the_goals_and_plans_of_itself_and_its_own_only() -> anyhow::Result<()> {
        within(async {
            // No conversation store: what's steered is goals and plans, not conversations.
            let hub = crate::hub::tests::scratch_hub_with(|c| {
                c.terminal.enabled = true;
                c.conversations.off = true;
            })
            .await?;
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let folder = tempfile::tempdir()?;
            let cwd = folder.path().to_string_lossy().into_owned();
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None };
            let ask = |name: &str, kind: &str| Ask { kind: kind.into(), on: Some("local".into()), cwd: Some(cwd.clone()), mode: Some(if kind == "claude" { "default" } else { "untrusted" }.into()), name: Some(name.into()), count: Some(1), ..Ask::default() };
            let one = |v: Vec<crate::harness::session::Info>| v.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent started"));
            let a = one(start(&hub, &person, ask("A", "claude")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?)?;
            let b = one(start(&hub, &Starter { parent: Some(a.clone()), ..person.clone() }, ask("B", "claude")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?)?;
            let other = one(start(&hub, &person, ask("C", "claude")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?)?;
            let as_a = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &a.id, None).await.map_err(anyhow::Error::msg)? };
            let code = |r: Result<Value, crate::ops::Fail>| r.err().map(|(c, _)| c.as_u16());

            // Its own goals, and its child's: yes.
            for id in [&a.id, &b.id] {
                call(as_a.clone(), "goals.add", json!({"agent": id, "goals": ["tests pass"], "start": false})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            }
            // Another's: no, whichever way.
            assert_eq!(code(call(as_a.clone(), "goals.add", json!({"agent": other.id, "goals": ["stop"]})).await), Some(403));
            assert_eq!(code(call(as_a.clone(), "goals.set", json!({"agent": other.id, "goals": []})).await), Some(403));
            assert_eq!(code(call(as_a.clone(), "goals.control", json!({"agent": other.id, "action": "clear"})).await), Some(403));
            assert!(crate::goals::get(&hub, &other.id).await?.items.is_empty(), "its goals are untouched");
            // A person may steer any.
            let person_cx = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            call(person_cx, "goals.add", json!({"agent": other.id, "goals": ["ship it"], "start": false})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;

            // A plan: not on another's agent; on a new one, which is its child under its mode.
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            assert_eq!(code(call(as_a.clone(), "plans.run", json!({"id": p.id, "session": other.id, "mode": "message"})).await), Some(403));
            let ran = call(as_a.clone(), "plans.run", json!({"id": p.id, "new": {"target": "local", "kind": "codex", "cwd": cwd}, "mode": "goals"})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let new = ran.get("session").and_then(Value::as_str).unwrap_or_default().to_owned();
            let info = hub.harness.info(&new).ok_or_else(|| anyhow::anyhow!("the plan's agent isn't listed: {ran}"))?;
            assert_eq!(info.parent.as_deref(), Some(a.id.as_str()), "the plan's new agent is the caller's child");
            assert_eq!(info.mode, "untrusted", "Codex under an asking parent asks too (its default, auto, would be more)");

            // Plan runs of one agent per step: it stops its own, never a person's.
            let place = crate::plans::agents::Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some(cwd.clone()), ..Default::default() };
            let config = crate::plans::agents::Config { place, opener: Some("owner".into()), actor: "admin".into(), ..Default::default() };
            let theirs = crate::plans::agents::start(&hub, &p, config).await?.run;
            assert_eq!(code(call(as_a.clone(), "plans.runs.stop", json!({"run": theirs})).await), Some(403));
            assert_eq!(code(call(as_a.clone(), "plans.runs.retry", json!({"run": theirs, "step": 1})).await), Some(403));
            let mine = call(as_a.clone(), "plans.run", json!({"id": p.id, "mode": "agents", "new": {"on": "local", "kind": "claude", "cwd": cwd}})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let mine = mine.get("run").and_then(Value::as_str).unwrap_or_default().to_owned();
            let stopped = call(as_a, "plans.runs.stop", json!({"run": mine})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert_eq!(stopped.get("state"), Some(&json!("stopped")), "{stopped}");
            Ok(())
        })
    }
}
