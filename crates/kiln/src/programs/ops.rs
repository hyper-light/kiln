//! The program operations (`programs.*`), in kiln's one operation table (`crate::ops::own`): the
//! `/api/programs` routes, and code mode's `kiln.programs.*`, which a program's
//! coordinator reads its slots and each track's turn from, and assigns its tracks' workers with.
//! Only a person changes the slots and weights.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Notice, SLOTS, SLOTS_MAX, WEIGHT, WEIGHT_MAX};
use crate::ops::{Cx, Fail, Op, Out, path, text};

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

const PROGRAM: &str = "The program's id: its master plan's (as plans_list gives it).";

fn json_of<T: serde::Serialize>(v: &T) -> Out {
    serde_json::to_value(v).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn failed(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The caller is an agent: its session's (`agent`), or one that signed in as one (an agent's token).
fn is_agent(cx: &Cx) -> bool {
    cx.caller.agent.is_some() || cx.caller.kind == crate::authz::gate::Kind::Agent
}

async fn program(cx: &Cx, id: &str) -> Result<super::Program, Fail> {
    super::get(&cx.hub, id).await.map_err(failed)?.ok_or((StatusCode::NOT_FOUND, format!("no program {id}")))
}

async fn list(cx: Cx, _v: Value) -> Out {
    Ok(json!({"programs": super::list(&cx.hub).await.map_err(failed)?}))
}

async fn get(cx: Cx, v: Value) -> Out {
    let id = text(&v, "program")?;
    json_of(&program(&cx, &id).await?)
}

/// What changed of settings `s`, in words (`slots 6 → 5, T7's weight 4 → 5`).
fn changed(s: &super::Settings, slots: Option<u32>, weights: &[(String, u32)]) -> Vec<String> {
    let mut said = Vec::new();
    let was = s.slots.unwrap_or(SLOTS).clamp(1, SLOTS_MAX);
    if let Some(n) = slots.filter(|n| *n != was) {
        said.push(format!("slots {was} → {n}"));
    }
    for (t, w) in weights {
        let was = s.weights.get(t).copied().unwrap_or(WEIGHT).clamp(1, WEIGHT_MAX);
        if was != *w {
            said.push(format!("{t}'s weight {was} → {w}"));
        }
    }
    said
}

fn whole(v: &Value, max: u32, what: &str) -> Result<u32, Fail> {
    v.as_u64().and_then(|s| u32::try_from(s).ok()).filter(|s| (1..=max).contains(s)).ok_or((StatusCode::BAD_REQUEST, format!("{what} is a whole number from 1 to {max}")))
}

async fn set(cx: Cx, v: Value) -> Out {
    if is_agent(&cx) {
        return Err((StatusCode::FORBIDDEN, "a program's slots and weights are the person's to change: an agent reads them (programs.get)".into()));
    }
    let id = text(&v, "program")?;
    let before = program(&cx, &id).await?;
    let slots = match v.get("slots") {
        None | Some(Value::Null) => None,
        Some(s) => Some(whole(s, SLOTS_MAX, "`slots`")?),
    };
    let mut weights: Vec<(String, u32)> = Vec::new();
    if let Some(given) = v.get("weights").filter(|w| !w.is_null()) {
        let given = given.as_object().ok_or((StatusCode::BAD_REQUEST, "`weights` is an object, by track (`{\"T7\": 4}`)".into()))?;
        for (t, w) in given {
            if !before.tracks.iter().any(|x| x.id == *t) {
                return Err((StatusCode::BAD_REQUEST, format!("{id} has no track {t}")));
            }
            weights.push((t.clone(), whole(w, WEIGHT_MAX, &format!("{t}'s weight"))?));
        }
    }
    // Changed as they're kept now, under the lock: two changes made at once both stand.
    let writing = super::WRITING.lock().await;
    let mut s = super::settings(&cx.hub, &id).await.map_err(failed)?;
    let said = changed(&s, slots, &weights);
    if let Some(n) = slots {
        s.slots = Some(n);
    }
    for (t, w) in &weights {
        s.weights.insert(t.clone(), *w);
    }
    let now = crate::util::now_ms();
    s.updated_at = now;
    s.by.clone_from(&cx.caller.name);
    // Its coordinator follows them at its next goal boundary: it's told what changed, the
    // message kept until its session takes it (tried again every 15 s).
    if let (Some(c), false) = (&before.coordinator_id, said.is_empty()) {
        let text = format!(
            "The person changed {}'s settings: {}. From your next goal boundary, follow them: kiln's programs.get (code mode: kiln.programs.get({{program: \"{id}\"}})) gives each track's slot by them.",
            before.title,
            said.join(", ")
        );
        s.notice = Some(super::deliver(&cx.hub, c, Notice { text, at: now, ..Notice::default() }).await);
    }
    super::save_settings(&cx.hub, &id, &s).await.map_err(failed)?;
    drop(writing);
    cx.hub.store.audit(cx.caller.name.clone(), "programs.set".into(), id.clone()).await;
    cx.hub.emit(&json!({"type": "program", "program": id}));
    json_of(&program(&cx, &id).await?)
}

/// Whom an agent works for (`owner`, `user:<id>`), as kiln knows it.
async fn person_of(cx: &Cx, agent: &str) -> Option<String> {
    crate::mcp::acts_for(&cx.hub, agent).await
}

async fn assign(cx: Cx, v: Value) -> Out {
    let id = text(&v, "program")?;
    let track = text(&v, "track")?;
    let before = program(&cx, &id).await?;
    let coordinator = before.coordinator_id.clone();
    // The coordinator's, or the person's.
    if is_agent(&cx) && cx.caller.agent.as_ref().is_none_or(|me| Some(me) != coordinator.as_ref()) {
        return Err((StatusCode::FORBIDDEN, format!("a program's tracks are assigned by its coordinator ({}) or the person", coordinator.as_deref().unwrap_or("none yet"))));
    }
    let t = before.tracks.iter().find(|x| x.id == track).ok_or((StatusCode::BAD_REQUEST, format!("{id} has no track {track}")))?;
    // Changed as they're kept now, under the lock (then the stacks', to hand the goal over).
    let writing = super::WRITING.lock().await;
    let mut all = super::assignments(&cx.hub.store, &id).await.map_err(failed)?;
    let now = crate::util::now_ms();
    let mut a = all.get(&track).cloned().unwrap_or_default();
    // The worker: any agent kiln knows that works for the same person as the coordinator (or, with
    // none yet, as the person assigning it).
    match v.get("agent") {
        None => {}
        Some(Value::Null) => a.agent = String::new(),
        Some(w) => {
            let w = w.as_str().filter(|w| !w.is_empty()).ok_or((StatusCode::BAD_REQUEST, "`agent` is an agent's id, or null to unassign".into()))?;
            let info = cx.hub.harness.info(w).ok_or((StatusCode::NOT_FOUND, format!("no agent {w}")))?;
            let whose = match &coordinator {
                Some(c) => person_of(&cx, c).await,
                None => cx.caller.opener(),
            };
            let theirs = person_of(&cx, &info.id).await;
            if theirs.is_none() || theirs != whose {
                let than = if coordinator.is_some() { format!("{id}'s coordinator") } else { "you".to_owned() };
                return Err((StatusCode::FORBIDDEN, format!("{} works for another person than {than}", info.name)));
            }
            a.agent = info.id;
        }
    }
    let met = v.get("met").and_then(Value::as_bool).unwrap_or(false);
    let mut goal = match v.get("goal") {
        // A worker unassigned: the goal under way on it comes back to the stack.
        None if a.agent.is_empty() && !met => None,
        None => a.goal.clone(),
        Some(Value::Null) => None,
        Some(g) => Some(g.as_str().filter(|g| !g.is_empty()).ok_or((StatusCode::BAD_REQUEST, "`goal` is a goal's id in the coordinator's stack, or null".into()))?.to_owned()),
    };
    // The goal it had, ended or taken out of the coordinator's stack since (said met there, say):
    // it has none now.
    if v.get("goal").is_none()
        && let (Some(g), Some(c)) = (goal.clone(), coordinator.as_deref())
    {
        let stack = crate::goals::get(&cx.hub, c).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !stack.items.iter().any(|i| i.id == g && (i.status == "pending" || i.status == "active")) {
            goal = None;
            a.goal = None;
        }
    }
    if met && goal.is_none() {
        return Err((StatusCode::CONFLICT, format!("{track} has no goal under way to say met")));
    }
    if let Some(g) = &goal {
        let c = coordinator.as_deref().ok_or((StatusCode::CONFLICT, format!("{id} has no coordinator yet: its master plan hasn't run as goals")))?;
        let stack = crate::goals::get(&cx.hub, c).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let item = stack.items.iter().find(|i| i.id == *g).ok_or((StatusCode::BAD_REQUEST, format!("the coordinator's stack has no goal {g}")))?;
        let key = super::goal_key(&item.text).filter(|(_, n)| format!("T{n}") == track).ok_or((StatusCode::BAD_REQUEST, format!("goal {g} isn't one of {track}'s (its id doesn't start {track})")))?;
        let new = a.goal.as_deref() != Some(g.as_str()) && !met;
        // Above 80% of a weekly window no new goal starts (the master plan's §5).
        if new && before.rule.holds_new_goals() {
            let why = match &before.rule {
                super::Rule::Hold { why } | super::Rule::Pause { why } => why.clone(),
                super::Rule::Ok => String::new(),
            };
            return Err((StatusCode::CONFLICT, format!("no new goal starts now: {why}")));
        }
        if !met && a.agent.is_empty() {
            return Err((StatusCode::BAD_REQUEST, format!("{track} has no worker: assign one (`agent`) to put a goal under way")));
        }
        let prefix = super::track_prefix(&key.0).unwrap_or(&key.0).to_owned();
        let on = (!met).then_some(a.agent.as_str());
        if !crate::goals::delegate(&cx.hub.store, c, &prefix, Some(g.as_str()), on, met, now).await.map_err(failed)? {
            return Err((StatusCode::CONFLICT, format!("goal {g} of the coordinator's stack isn't one still to come")));
        }
        a.goal = (!met).then(|| g.clone());
    } else if (matches!(v.get("goal"), Some(Value::Null)) || a.agent.is_empty())
        && let (Some(c), Some(key)) = (coordinator.as_deref(), t.goal.as_ref().and_then(|g| g.key.clone()))
    {
        // No goal under way any more (or no worker): the track's delegated goal comes back to the
        // stack.
        let prefix = super::track_prefix(&key).unwrap_or(&key).to_owned();
        crate::goals::delegate(&cx.hub.store, c, &prefix, None, None, false, now).await.map_err(failed)?;
        a.goal = None;
    }
    a.at = now;
    a.by.clone_from(&cx.caller.name);
    if a.agent.is_empty() && a.goal.is_none() {
        all.remove(&track);
    } else {
        all.insert(track.clone(), a);
    }
    super::save_assignments(&cx.hub.store, &id, &all).await.map_err(failed)?;
    drop(writing);
    cx.hub.store.audit(cx.caller.name.clone(), "programs.assign".into(), format!("{id} {track}")).await;
    cx.hub.emit(&json!({"type": "program", "program": id}));
    json_of(&program(&cx, &id).await?)
}

pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "programs.list",
            summary: "Every multi-stack program kiln runs (a master plan and its track plans, run as goals on a coordinator), the critical path first: per track its worker and how it stands (a pause, its window and reset, its threshold), goal under way (and on which agent), goals met, change requests, pass, slot and usage; the program's slots, usage and the rule it brings.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "programs"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "programs.get",
            summary: "One program, as programs.list gives each: its coordinator reads its slots and each track's slot (running, next, waiting, held, paused, done) from it at each goal boundary.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"program": {"type": "string", "description": PROGRAM}}), &["program"]),
            route: |v| Ok((Method::GET, path(&["api", "programs", &text(v, "program")?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "programs.set",
            summary: "A person changes a program's slots (1 to 32) and its tracks' weights (1 to 100): kept in kiln; its coordinator is told (the message kept until it's delivered) and follows them at its next goal boundary. Agents can't.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "program": {"type": "string", "description": PROGRAM},
                        "slots": {"type": "integer", "minimum": 1, "maximum": SLOTS_MAX},
                        "weights": {"type": "object", "description": "By track: {\"T7\": 4}", "additionalProperties": {"type": "integer", "minimum": 1, "maximum": WEIGHT_MAX}}
                    }),
                    &["program"],
                )
            },
            route: |v| Ok((Method::PUT, path(&["api", "programs", &text(v, "program")?, "settings"]))),
            run: |cx, v| Box::pin(set(cx, v)),
        },
        Op {
            name: "programs.assign",
            summary: "The program's coordinator (or the person) assigns a track's worker (`agent`: any agent of the same person; null to unassign) and puts a goal of the coordinator's stack under way on it (`goal`: its id; null: none), or marks that goal met (`met`). Above 80% of a weekly window no new goal is put under way.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "program": {"type": "string", "description": PROGRAM},
                        "track": {"type": "string", "description": "The track: T7."},
                        "agent": {"type": ["string", "null"], "description": "Its worker's agent id; null to unassign."},
                        "goal": {"type": ["string", "null"], "description": "A goal's id in the coordinator's stack, of this track, under way on the worker; null: none."},
                        "met": {"type": "boolean", "description": "The goal (`goal`, or the one under way) is met."}
                    }),
                    &["program", "track"],
                )
            },
            route: |v| Ok((Method::PUT, path(&["api", "programs", &text(v, "program")?, "tracks", &text(v, "track")?]))),
            run: |cx, v| Box::pin(assign(cx, v)),
        },
    ]
}
