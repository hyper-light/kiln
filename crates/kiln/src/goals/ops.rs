//! The goal stack operations (`goals.*`), in kiln's one operation table (`crate::ops::own`): the
//! MCP tools agents call (to line up goals for themselves or the agents they started) and code mode's
//! `kiln.goals.*`. Each is one of the console's `/api/harness/sessions/:id/goals` routes, which call
//! the same functions.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Action, At, By, Change, Error, Given, Need, NewGoal, Pausing};
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
const GOAL_ID: &str = "A goal's id, as goals_get gives it.";

/// A string field of an input, when it's given and not empty.
fn opt(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Where goals added go, from `before`, `after` or `next`.
fn at_of(v: &Value) -> Result<At, Fail> {
    match (opt(v, "before"), opt(v, "after"), v.get("next").and_then(Value::as_bool).unwrap_or(false)) {
        (None, None, false) => Ok(At::End),
        (Some(b), None, false) => Ok(At::Before(b)),
        (None, Some(a), false) => Ok(At::After(a)),
        (None, None, true) => Ok(At::Next),
        _ => Err((StatusCode::BAD_REQUEST, "say one place for them: `before`, `after` or `next`".into())),
    }
}

/// When a pause ends by itself, from `resume_in_s` (seconds from now) or `resume_at` (unix ms, or
/// an RFC 3339 time): unix ms.
fn until_of(v: &Value, now: i64) -> Result<Option<i64>, Fail> {
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_owned());
    match (v.get("resume_in_s").filter(|x| !x.is_null()), v.get("resume_at").filter(|x| !x.is_null())) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(bad("say when it resumes one way: `resume_in_s` or `resume_at`")),
        (Some(s), None) => {
            let secs = s.as_f64().filter(|x| x.is_finite() && *x > 0.0).ok_or_else(|| bad("`resume_in_s` is a number of seconds, more than 0"))?;
            // At most a pause's longest, checked by the stack.
            #[allow(clippy::cast_possible_truncation)]
            let ms = (secs.min(1e12) * 1000.0) as i64;
            Ok(Some(now.saturating_add(ms)))
        }
        (None, Some(t)) => match (t.as_i64(), t.as_str()) {
            (Some(ms), _) => Ok(Some(ms)),
            (None, Some(text)) => time::OffsetDateTime::parse(text.trim(), &time::format_description::well_known::Rfc3339)
                .map(|d| i64::try_from(d.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX))
                .map(Some)
                .map_err(|_| bad("`resume_at` is unix ms, or a time like 2026-10-21T15:00:00Z")),
            _ => Err(bad("`resume_at` is unix ms, or a time like 2026-10-21T15:00:00Z")),
        },
    }
}

async fn get(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    if cx.hub.harness.info(&agent).is_some() {
        crate::harness::lineage::may_see(&cx.hub, &cx.caller, &agent).await?;
    }
    json_of(&super::get(&cx.hub, &agent).await.map_err(fail)?)
}

/// The goals `goals_add` takes (`goals`): each its words, or `{text, needs}` (`needs`: the goals it
/// waits for, a number for one of these from 1, or a goal's id). `parallel`: those that say nothing
/// wait for nothing. What they wait for is said by `by`.
pub(crate) fn goals_of(v: &Value, by: &str) -> Result<Vec<NewGoal>, String> {
    let parallel = v.get("parallel").and_then(Value::as_bool).unwrap_or(false);
    let each = "each goal is its words, or {text, needs}";
    let need = |n: &Value| match n {
        Value::Number(k) => k.as_u64().and_then(|k| usize::try_from(k).ok()).map(Need::Added).ok_or_else(|| "`needs` counts these goals from 1".to_owned()),
        Value::String(s) if !s.trim().is_empty() => Ok(Need::Id(s.trim().to_owned())),
        _ => Err("`needs` lists goals: a number (one of these, from 1) or a goal's id".to_owned()),
    };
    let goals = v.get("goals").and_then(Value::as_array).ok_or_else(|| "`goals` is a list of at least one goal".to_owned())?;
    if goals.is_empty() {
        return Err("`goals` is a list of at least one goal".into());
    }
    goals
        .iter()
        .map(|g| {
            let (text, needs) = match g {
                Value::String(s) => (s.clone(), None),
                Value::Object(o) => {
                    let text = o.get("text").and_then(Value::as_str).ok_or_else(|| each.to_owned())?.to_owned();
                    let needs = match o.get("needs") {
                        None | Some(Value::Null) => None,
                        Some(Value::Array(a)) => Some(a.iter().map(need).collect::<Result<Vec<_>, _>>()?),
                        Some(_) => return Err("`needs` is a list".to_owned()),
                    };
                    (text, needs)
                }
                _ => return Err(each.to_owned()),
            };
            let needs = needs.or_else(|| parallel.then(Vec::new));
            Ok(NewGoal { text, needs_by: needs.as_ref().map(|_| by.to_owned()), needs, ..NewGoal::default() })
        })
        .collect()
}

async fn add(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let goals = goals_of(&v, &By::of(&cx.hub, &cx.caller, &agent).name()).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    let start = v.get("start").and_then(Value::as_bool).unwrap_or(true);
    // Under a key (CP-T3.2), asked again (its answer lost), the goals it added are found, never
    // added twice.
    let key = crate::harness::ops::key_of(&v)?;
    let goals = super::keyed(&crate::harness::ops::key_scope(&cx.caller), &agent, key.as_deref(), goals);
    let s = super::add_at(&cx.hub, &agent, goals, start, &at_of(&v)?).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.add".into(), agent).await;
    json_of(&s)
}

/// What several goals wait for, said at once (`super::order`).
async fn order(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let bad = |m: &str| (StatusCode::BAD_REQUEST, m.to_owned());
    let goals = v.get("goals").and_then(Value::as_array).ok_or_else(|| bad("`goals` lists goals, each {id, needs}"))?;
    let given = goals
        .iter()
        .map(|g| {
            let id = g.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad("each goal is {id, needs}: its id, as goals_get gives it"))?;
            let needs = g.get("needs").and_then(Value::as_array).ok_or_else(|| bad("each goal is {id, needs}: `needs` lists the ids of the goals it waits for ([] for none)"))?;
            let needs = needs.iter().map(|n| n.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| bad("`needs` lists goals' ids"))).collect::<Result<Vec<_>, _>>()?;
            Ok((id.to_owned(), needs))
        })
        .collect::<Result<Vec<_>, Fail>>()?;
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let s = super::order(&cx.hub, &agent, given, &by).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.order".into(), agent).await;
    json_of(&s)
}

async fn set(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let given: Vec<Given> = serde_json::from_value(v.get("goals").cloned().unwrap_or_else(|| json!([]))).map_err(|e| (StatusCode::BAD_REQUEST, format!("`goals`: {e}")))?;
    let s = super::set(&cx.hub, &agent, given).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.set".into(), agent).await;
    json_of(&s)
}

async fn control(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let action: Action =
        serde_json::from_value(v.get("action").cloned().unwrap_or(Value::Null)).map_err(|_| (StatusCode::BAD_REQUEST, "`action` is start (or resume), pause, skip, done, clear or restore".into()))?;
    let until = until_of(&v, cx.hub.clock.unix_ms())?;
    if action != Action::Pause && (until.is_some() || opt(&v, "why").is_some()) {
        return Err((StatusCode::BAD_REQUEST, "`why`, `resume_in_s` and `resume_at` go with pause".into()));
    }
    let by = By::of(&cx.hub, &cx.caller, &agent);
    // The goal it's for (skip, done): asked again once that goal has ended, nothing more ends.
    let goal = opt(&v, "id");
    // Asked again under its key (its answer lost): answered with the stack as it is (CP-T3.2).
    let key = crate::harness::ops::key_of(&v)?;
    let asked = super::asked_id(&crate::harness::ops::key_scope(&cx.caller), &agent, &format!("{action:?}"), goal.as_deref(), key.as_deref());
    let s = super::control_asked(&cx.hub, &agent, action, &by, Pausing { why: opt(&v, "why"), until, review: None }, goal.as_deref(), asked.as_deref()).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.control".into(), format!("{agent}: {action:?}")).await;
    json_of(&s)
}

async fn done(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let (goal, key) = (opt(&v, "id"), crate::harness::ops::key_of(&v)?);
    let asked = super::asked_id(&crate::harness::ops::key_scope(&cx.caller), &agent, &format!("{:?}", Action::Done), goal.as_deref(), key.as_deref());
    let s = super::done_asked(&cx.hub, &agent, goal.as_deref(), opt(&v, "evidence").as_deref(), &by, asked.as_deref()).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.done".into(), format!("{agent}: {}", opt(&v, "id").unwrap_or_else(|| "the goal under way".into()))).await;
    json_of(&s)
}

async fn update(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let id = opt(&v, "id").ok_or_else(|| (StatusCode::BAD_REQUEST, "`id` is the goal's id (goals_get lists them)".to_owned()))?;
    let change: Change = serde_json::from_value(v.clone()).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let s = super::update(&cx.hub, &agent, &id, change, &by).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.update".into(), format!("{agent}: {id}")).await;
    json_of(&s)
}

/// What an agent does once its goal has become a plan.
const PLANNING: &str = "Plan it now, in this order. 1. Research it, inside and outside, and record what you find as you go (research_add, with this research's id): inside, the files on your machine and on others (code, docs, data, pictures, frames of video) and kiln's own records (conversations_search, plans, CI runs, deployments); outside, the web, official docs and standards, papers (arXiv, Semantic Scholar, Crossref, PubMed), blogs and forums. Each finding is a claim with its citations (the source, the exact words you quote, where they are) and any counterclaims you found. kiln looks for each quote where you say it is and grades each finding: describe the evidence, never the grade. 2. Write the plan's steps (plans_update with the plan's id: `items`, each with what to do, its acceptance criteria, its tests, the steps it waits for in `needs`, and the findings it rests on in `basis`). 3. Say it's ready (plans_ready, with `then`: `goals` to carry on here with its steps as this goal's parts, `agents` to run it with an agent per step, `review` to pause for the person to look first). It answers with the research, graded: show it to the person in your reply, as it is, every claim with its sources and grade. Once every step is met, so is the goal.";

/// How long kiln's ask for a subagent to plan a goal stands, none started yet, before another may be
/// asked for (the console's `ASK_STANDS_MS`).
const DISPATCH_AGAIN_MS: i64 = 30 * 60_000;

async fn plan(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    // A person's Make a plan (or another agent's): a goal the agent isn't on yet is planned now, by
    // a subagent of its own, beside what it's doing.
    let asked_by_another = cx.caller.agent.as_deref() != Some(agent.as_str());
    let p = super::plan(&cx.hub, &agent, opt(&v, "id").as_deref(), opt(&v, "title").as_deref(), &cx.caller.name, asked_by_another).await.map_err(fail)?;
    let (dispatched, why) = if p.delegate {
        match dispatch(&cx.hub, &agent, &p).await {
            Ok(()) => (true, None),
            Err(why) => (false, Some(why)),
        }
    } else {
        (false, None)
    };
    cx.hub.store.audit(cx.caller.name.clone(), "goals.plan".into(), format!("{agent}: {} as plan {}{}", p.goal, p.plan, if dispatched { ", a subagent planning it" } else { "" })).await;
    Ok(json!({"plan": p.plan, "research": p.research, "goal": p.goal, "part": p.part, "dispatched": dispatched, "why": why, "next": PLANNING}))
}

/// Tells agent `agent`, steered into what it's doing, to plan goal `p` now with a subagent of its
/// own ([`dispatch_words`]); else why it couldn't be (one at it, asked for moments ago, the agent
/// not running or unreachable).
async fn dispatch(hub: &crate::hub::Hub, agent: &str, p: &super::Planned) -> Result<(), String> {
    let key = format!("goals.plan.dispatched:{}", p.plan);
    let now = crate::util::now_ms();
    let asked = hub.store.kv_get(key.clone()).await.ok().flatten().and_then(|at| at.parse::<i64>().ok());
    let tasks = hub.harness.state(agent).map(|s| s.tasks).unwrap_or_default();
    may_send(&tasks, &p.plan, asked, now).map_err(str::to_owned)?;
    // Its id made once (CP-T3.2): from the plan and when kiln last asked for it, so asked for twice
    // at once it's one message, which its agent's inbox takes once.
    let id = format!("{}plan-{}", crate::harness::pause::KILN_ID, crate::util::derive_id(&["goals.plan.dispatch", &p.plan, &asked.map(|a| a.to_string()).unwrap_or_default()]));
    let send = crate::harness::model::Command::Send { id, text: dispatch_words(p), mode: crate::harness::model::SendMode::Steer, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
    match hub.harness.command(hub, agent, send).await {
        crate::harness::registry::Sent::Ok | crate::harness::registry::Sent::Accepted(_) | crate::harness::registry::Sent::Taken | crate::harness::registry::Sent::Parked => {
            let _ = hub.store.kv_set(key, now.to_string()).await;
            Ok(())
        }
        crate::harness::registry::Sent::Refused(why) => Err(format!("its agent's inbox refused it ({why}), so it's planned when the stack gets to it")),
        s @ (crate::harness::registry::Sent::Rejected { .. } | crate::harness::registry::Sent::Unanswered(_)) => {
            Err(format!("{}, so it's planned when the stack gets to it", s.failure().map_or_else(|| "its agent's runner refused it".to_owned(), |(_, why)| why)))
        }
        crate::harness::registry::Sent::NoSession => Err("the agent isn't running, so it's planned when the stack gets to it".into()),
        crate::harness::registry::Sent::Unreachable => Err("the agent's machine can't be reached, so it's planned when the stack gets to it".into()),
    }
}

/// Whether a subagent may be sent to plan plan `plan` now, as the agent's session shows its
/// subagents (`tasks`: the one planning it is the latest whose brief names the plan) and as kiln
/// last asked for one (`asked`, unix ms): not while one is at it, nor moments after kiln asked
/// when none has started since; once the last one stopped with the plan not ready, it may.
fn may_send(tasks: &[crate::harness::model::Task], plan: &str, asked: Option<i64>, now: i64) -> Result<(), &'static str> {
    let last = tasks.iter().filter(|t| t.subagent() && t.prompt.as_deref().is_some_and(|b| b.contains(plan))).max_by_key(|t| t.started);
    if last.is_some_and(crate::harness::model::Task::live) {
        return Err("a subagent is planning it now");
    }
    let ended = last.map(|t| t.ended.unwrap_or(t.started));
    match asked {
        Some(at) if now - at < DISPATCH_AGAIN_MS && ended.is_none_or(|e| at > e) => Err("a subagent was asked for moments ago: it starts at the agent's next step"),
        _ => Ok(()),
    }
}

/// What an agent hears to plan a goal of its now, beside its own work: to send a subagent of its own
/// with [`PLANNING`]'s brief, the plan's and the research's ids in it, and to go on with its work.
fn dispatch_words(p: &super::Planned) -> String {
    format!(
        "Your person asked for one of your goals to be planned now, beside what you're doing: \"{goal}\". Dispatch a subagent for it (your Agent or Task tool; without one, start an agent with agents_start), give it the brief below, and go on with your own work meanwhile. When it reports back, check the plan is ready (plans_get {plan}) and say so in a line.\n\nThe brief: plan {plan}, research {research}, for the goal \"{goal}\". {PLANNING} Say it's ready with `then: goals`. Report back the plan's title and steps, and its research as research_get presents it.",
        goal = p.words,
        plan = p.plan,
        research = p.research,
    )
}

async fn settings(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let flag = |k: &str| match v.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(b) => b.as_bool().map(Some).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{k}` is true or false"))),
    };
    let (plan_big, side_by_side) = (flag("plan_big")?, flag("side_by_side")?);
    if plan_big.is_none() && side_by_side.is_none() {
        return Err((StatusCode::BAD_REQUEST, "say what's set: `plan_big`, `side_by_side`, or both".to_owned()));
    }
    let s = super::settings(&cx.hub, &agent, plan_big, side_by_side).await.map_err(fail)?;
    let on = |b: bool| if b { "on" } else { "off" };
    let said: Vec<String> = [plan_big.map(|b| format!("plan big goals first {}", on(b))), side_by_side.map(|b| format!("goals side by side {}", on(b)))].into_iter().flatten().collect();
    cx.hub.store.audit(cx.caller.name.clone(), "goals.settings".into(), format!("{agent}: {}", said.join(", "))).await;
    json_of(&s)
}

async fn split(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent).await?;
    let parts: Vec<String> = v.get("goals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let s = super::split(&cx.hub, &agent, opt(&v, "id").as_deref(), parts).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.split".into(), format!("{agent}: {}", opt(&v, "id").unwrap_or_else(|| "the goal under way".into()))).await;
    json_of(&s)
}

/// The `goals.*` operations.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "goals.get",
            summary: "An agent's goal stack (yours, without `agent`): its goals in order, each with its id and status (pending, active, met, unmet, skipped), the goal it's a part of (`part_of`) and the plan step it's made from (`step`); and whether the stack runs, is paused (why, and `resume_at` when it resumes by itself) or is done; `cleared`: how many goals were cleared from it last (goals_control restore brings them back). The goal under way is sent again whenever the agent stops short of it, until it's met: say so with goals_done.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &[]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "sessions", &agent_of(v)?, "goals"]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "goals.add",
            summary: "Line goals up for an agent (for yourself, without `agent`), after the ones it has (or `next`, right after the goal under way; or `before` or `after` a goal still to go, by id). Say what each waits for (`needs`, [] for nothing): a goal goes once what it waits for has ended, and goals that don't wait for each other run side by side, on subagents while there's room. One that says nothing is ordered before it goes (kiln asks the agent: goals_order). Each goes as its harness's own /goal where it has one, like giving it /goal again and again. Runs the stack unless `start` is false. To pursue a document or list as a goal stack, give one goal per item, each saying what done is and what it needs.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {
                            "type": "array",
                            "description": "Each a condition or objective, one line: its words, or {text, needs}.",
                            "items": {
                                "type": ["string", "object"],
                                "properties": {
                                    "text": {"type": "string"},
                                    "needs": {"type": "array", "items": {"type": ["integer", "string"]}, "description": "The goals it waits for: a number for one of these (from 1), or a goal's id. [] for none."}
                                },
                                "required": ["text"]
                            }
                        },
                        "parallel": {"type": "boolean", "description": "Those of these that don't say what they need wait for nothing."},
                        "start": {"type": "boolean"},
                        "next": {"type": "boolean", "description": "Right after the goal under way, before the rest."},
                        "before": {"type": "string", "description": "Before this goal still to go (its id). Before the goal under way, they go first and it waits after them."},
                        "after": {"type": "string", "description": "After this goal still to go (its id)."},
                        "key": {"type": "string", "description": "An idempotency key, made once for this request (1 to 256 bytes): asked again under it (its answer lost), the goals it added are found and none is added twice. Without it, each call adds its goals again: pass it whenever you may ask again."}
                    }),
                    &["goals"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals"]))),
            run: |cx, v| Box::pin(add(cx, v)),
        },
        Op {
            name: "goals.set",
            summary: "Replace an agent's goals still to go (reordered, edited, removed, added): `goals` is the list as it should be, existing ones by `id` (one given by its words alone is the goal still to go with those words). The goal under way may be in it: first, it stays under way (with other words, it goes to the agent again as they say); after others, it goes back among them and the first goes next; left out, it stays under way. Goals done are left as they are.",
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
            summary: "Start or resume, pause, skip the goal under way (left unmet: to say it's met, use goals_done), or clear an agent's goal stack; restore brings back the goals cleared last, as they were, and runs it. Starting, resuming or skipping sends what's next to the agent at once. A pause can say `why` and resume by itself: `resume_in_s` (seconds from now) or `resume_at` (unix ms, or a time like 2026-10-21T15:00:00Z); paused, the agent isn't held to its goal (its harness's own goal is cleared) and nothing is sent until it resumes.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "action": {"type": "string", "enum": ["start", "resume", "pause", "skip", "done", "clear", "restore"]},
                        "why": {"type": "string", "description": "pause: why, shown with the stack (what it waits for)."},
                        "id": {"type": "string", "description": "skip, done: the goal it's for (the goal under way, by its id); asked again once that goal has ended, nothing more ends."},
                        "key": {"type": "string", "description": "An idempotency key, made once for this request (1 to 256 bytes): asked again under it (its answer lost), it's answered with the stack as it is and does nothing more, whatever changed since. Without it, a resume or a skip asked again after the stack changed is done again: pass it whenever you may ask again."},
                        "resume_in_s": {"type": "number", "description": "pause: resume by itself this many seconds from now (at most 30 days)."},
                        "resume_at": {"type": ["integer", "string"], "description": "pause: resume by itself then: unix ms, or an RFC 3339 time."}
                    }),
                    &["action"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", v.get("action").and_then(Value::as_str).unwrap_or("start")]))),
            run: |cx, v| Box::pin(control(cx, v)),
        },
        Op {
            name: "goals.done",
            summary: "Say a goal is met: the goal under way (or the one `id` names), with `evidence`, what shows it (the tests that passed, the numbers measured, where it's live). It ends as met, its harness's own goal is cleared, and the next goes to the agent at once. Use this when you've met your goal rather than skipping it (skip leaves a goal unmet); a goal split into parts that's said met takes its parts still to go with it, and a goal skipped earlier can be said met after all.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "id": {"type": "string", "description": "The goal that's met; left out, the goal under way."},
                        "evidence": {"type": "string", "description": "What shows it's met, a line or two: kept with the goal for the person to see."},
                        "key": {"type": "string", "description": "An idempotency key, made once for this request (1 to 256 bytes): asked again under it (its answer lost), it's answered with the stack as it is and ends nothing more, whatever ended since. Without it, asked again late it may end the goal under way then: pass it (or the goal's `id`) whenever you may ask again."}
                    }),
                    &[],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "done"]))),
            run: |cx, v| Box::pin(done(cx, v)),
        },
        Op {
            name: "goals.update",
            summary: "Change one goal still to go, by its `id`, without giving the whole list: `text` gives it other words (the goal under way goes to the agent again as they say); `next`, `before` or `after` (a goal id) moves it (put before the goal under way, it goes next and that one waits); `needs` says what it waits for (goals' ids, [] for none); `remove` takes it out (the goal under way ends as skipped; one to come goes, with its parts, and what waited for it waits for what it waited for).",
            read_only: false,
            destructive: true,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "id": {"type": "string", "description": GOAL_ID},
                        "text": {"type": "string", "description": "Its new words: a condition or objective, one line."},
                        "next": {"type": "boolean", "description": "Move it right after the goal under way."},
                        "before": {"type": "string", "description": "Move it before this goal still to go (its id)."},
                        "after": {"type": "string", "description": "Move it after this goal still to go (its id)."},
                        "needs": {"type": "array", "items": {"type": "string"}, "description": "What it waits for: the ids of goals of the stack ([] for nothing)."},
                        "remove": {"type": "boolean", "description": "Take it out."}
                    }),
                    &["id"],
                )
            },
            route: |v| Ok((Method::PATCH, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", &opt(v, "id").unwrap_or_default()]))),
            run: |cx, v| Box::pin(update(cx, v)),
        },
        Op {
            name: "goals.order",
            summary: "Say what goals still to go wait for, several at once: `goals`, each {id, needs} (`needs`: the ids of the goals of the stack it waits for, [] for none). A goal goes once what it waits for has ended; goals that don't wait for each other run side by side, on subagents of yours while there's room, and you coordinate. While they can, kiln asks you to order the goals that say nothing before any of them goes: say only what you're sure of from the goals, the conversation and the code, and give every goal it asks about its needs. A loop of waiting is refused.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {"id": {"type": "string", "description": GOAL_ID}, "needs": {"type": "array", "items": {"type": "string"}, "description": "The ids of the goals it waits for ([] for none)."}},
                                "required": ["id", "needs"]
                            }
                        }
                    }),
                    &["goals"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "order"]))),
            run: |cx, v| Box::pin(order(cx, v)),
        },
        Op {
            name: "goals.plan",
            summary: "Turn a goal (the one under way, or the one `id` names) into a plan kiln keeps: a plan in its planning phase (the goal's words its brief) and its research. The goal gets one part first, planning it: research it inside and outside (research_add: each claim with its sources and the words quoted; kiln grades each), write its steps (plans_update: acceptance criteria, tests, `needs`, and the findings each rests on in `basis`), present the research with its sources and grades (research_get's markdown), then say it's ready (plans_ready). Its steps then become the goal's parts (or run with an agent each), and once every one is met, so is the goal.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "id": {"type": "string", "description": "The goal to plan; left out, the goal under way."},
                        "title": {"type": "string", "description": "The plan's title (left out: from the goal's words)."}
                    }),
                    &[],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "plan"]))),
            run: |cx, v| Box::pin(plan(cx, v)),
        },
        Op {
            name: "goals.settings",
            summary: "Whether an agent's goal stack plans its big goals first (`plan_big`, on unless turned off): a goal of the agent's own longer than a couple of sentences is researched and planned before it's worked on, as goals_plan does, its planning going to the agent in its place. And whether its goals that don't wait for each other run side by side on subagents (`side_by_side`, on unless turned off), or one at a time on the agent. A person's settings: not one of the agents' own tools.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "plan_big": {"type": "boolean", "description": "Plan big goals first."},
                        "side_by_side": {"type": "boolean", "description": "Run goals that don't wait for each other side by side, on subagents."}
                    }),
                    &["agent"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "settings"]))),
            run: |cx, v| Box::pin(settings(cx, v)),
        },
        Op {
            name: "goals.split",
            summary: "Split a goal (the one under way, or the one `id` names) into a goal stack of its own: its parts (`goals`, a goal each, one line saying what done is) go right before it and are pursued first, each in turn, then the goal itself comes back for its own check. A goal made from a plan's step (its `step` in goals_get), given no parts, splits into its acceptance criteria and its tests, a goal each. The goal under way split waits after its parts, and the first part goes now.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "id": {"type": "string", "description": "The goal to split; left out, the goal under way."},
                        "goals": {"type": "array", "items": {"type": "string"}, "description": "Its parts, in order. Left out for a plan step's goal: its acceptance criteria and tests."}
                    }),
                    &[],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "split"]))),
            run: |cx, v| Box::pin(split(cx, v)),
        },
    ]
}

/// Whether another subagent may be sent to plan a goal (Make a plan, Plan it now, Plan it again).
#[cfg(test)]
mod planners {
    use super::{DISPATCH_AGAIN_MS, may_send};
    use crate::harness::model::Task;

    const NOW: i64 = 10 * DISPATCH_AGAIN_MS;

    fn planner(plan: &str, status: &str, started: i64, ended: Option<i64>) -> Task {
        Task {
            id: format!("t{started}"),
            description: "Plan it".into(),
            class: Some("agent".into()),
            status: status.into(),
            started,
            ended,
            prompt: Some(format!("The brief: plan {plan}, research rs_1, for the goal \"x\".")),
            ..Task::default()
        }
    }

    #[test]
    fn none_asked_for_nor_at_it_may_go() {
        assert_eq!(may_send(&[], "pl_1", None, NOW), Ok(()));
    }

    #[test]
    fn asked_for_moments_ago_and_not_started_waits_but_not_past_half_an_hour() {
        assert!(may_send(&[], "pl_1", Some(NOW - 60_000), NOW).is_err_and(|w| w.contains("moments ago")));
        assert_eq!(may_send(&[], "pl_1", Some(NOW - DISPATCH_AGAIN_MS - 1), NOW), Ok(()));
    }

    #[test]
    fn one_at_it_holds_off_another_whenever_it_was_asked_for() {
        let at_it = [planner("pl_1", "running", NOW - 2 * DISPATCH_AGAIN_MS, None)];
        assert!(may_send(&at_it, "pl_1", Some(NOW - 2 * DISPATCH_AGAIN_MS), NOW).is_err_and(|w| w.contains("planning it now")));
        assert!(may_send(&at_it, "pl_1", None, NOW).is_err_and(|w| w.contains("planning it now")));
    }

    #[test]
    fn one_that_stopped_short_may_be_sent_again_at_once() {
        let stopped = [planner("pl_1", "completed", NOW - 120_000, Some(NOW - 30_000))];
        assert_eq!(may_send(&stopped, "pl_1", Some(NOW - 125_000), NOW), Ok(()));
    }

    #[test]
    fn asked_again_since_the_last_stopped_waits_for_the_new_one() {
        let stopped = [planner("pl_1", "completed", NOW - 300_000, Some(NOW - 200_000))];
        assert!(may_send(&stopped, "pl_1", Some(NOW - 10_000), NOW).is_err_and(|w| w.contains("moments ago")));
    }

    #[test]
    fn the_latest_counts_and_only_subagents_planning_this_plan() {
        let earlier_then_now = [planner("pl_1", "completed", NOW - 400_000, Some(NOW - 300_000)), planner("pl_1", "running", NOW - 60_000, None)];
        assert!(may_send(&earlier_then_now, "pl_1", Some(NOW - 70_000), NOW).is_err_and(|w| w.contains("planning it now")));
        let others = [
            planner("pl_2", "running", NOW - 60_000, None),
            Task { class: Some("shell".into()), status: "running".into(), command: Some("plan pl_1".into()), prompt: Some("pl_1".into()), ..Task::default() },
        ];
        assert_eq!(may_send(&others, "pl_1", None, NOW), Ok(()));
    }
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

    /// CP-T3.2: a plan run under an idempotency key, run again with it, is the same run: on an
    /// agent that runs its messages go once (its inbox takes each once) and it's answered as it
    /// was; one that starts an agent starts one, once; one that adds goals adds them once.
    #[test]
    fn a_plan_run_again_under_its_key_is_the_same_run() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |c| {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
            let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd.clone()), name: Some("A".into()), count: Some(1), ..Ask::default() };
            let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?;
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let run = |input: Value| {
                let me = me.clone();
                async move { call(me, "plans.run", input).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}")) }
            };
            // Steps on an agent that runs: twice under one key, one run, its two messages once.
            let first = run(json!({"id": p.id, "session": a.id, "mode": "steps", "key": "tidy-1"})).await?;
            let again = run(json!({"id": p.id, "session": a.id, "mode": "steps", "key": "tidy-1"})).await?;
            assert_eq!(first, again, "answered as it was");
            assert_eq!(first.get("sent"), Some(&json!(2)));
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            assert_eq!(inbox.taken(&a.id, 100).await?.len(), 2, "its two messages, once each");
            // Without a key, another run.
            let other = run(json!({"id": p.id, "session": a.id, "mode": "steps"})).await?;
            assert_ne!(other.get("run"), first.get("run"));
            // On a new agent: one agent, once.
            let agents = hub.harness.list().len();
            let made = run(json!({"id": p.id, "new": {"target": "local", "kind": "claude", "cwd": cwd}, "mode": "message", "key": "tidy-2"})).await?;
            let made_again = run(json!({"id": p.id, "new": {"target": "local", "kind": "claude", "cwd": cwd}, "mode": "message", "key": "tidy-2"})).await?;
            assert_eq!(made, made_again);
            assert_eq!(hub.harness.list().len(), agents + 1, "one agent started");
            // As goals: added once.
            let goals = run(json!({"id": p.id, "session": a.id, "mode": "goals", "key": "tidy-3"})).await?;
            let goals_again = run(json!({"id": p.id, "session": a.id, "mode": "goals", "key": "tidy-3"})).await?;
            assert_eq!(goals, goals_again);
            assert_eq!(crate::goals::get(&hub, &a.id).await?.items.len(), 2, "its two goals, once");
            // One agent per step: twice under one key, one run (its first step's agent once).
            let agents = hub.harness.list().len();
            let steps = run(json!({"id": p.id, "new": {"on": "local", "kind": "claude", "cwd": cwd}, "mode": "agents", "key": "tidy-4"})).await?;
            let steps_again = run(json!({"id": p.id, "new": {"on": "local", "kind": "claude", "cwd": cwd}, "mode": "agents", "key": "tidy-4"})).await?;
            assert_eq!(steps.get("run"), steps_again.get("run"), "the same run");
            assert_eq!(steps_again.get("steps"), Some(&json!(2)));
            assert_eq!(hub.harness.list().len(), agents + 1, "its first step's agent, once");
            // A run that failed before it did anything isn't held against its key: run again under
            // it, it's tried again (and fails the same way), not refused as under way.
            for input in [
                json!({"id": p.id, "new": {"target": "no-such-runner", "kind": "claude", "cwd": cwd}, "mode": "message", "key": "tidy-5"}),
                json!({"id": p.id, "session": "no-such-agent", "mode": "goals", "key": "tidy-6"}),
            ] {
                let first = run(input.clone()).await.map_err(|e| e.to_string());
                let again = run(input).await.map_err(|e| e.to_string());
                anyhow::ensure!(first.is_err() && again == first, "{first:?}, then {again:?}");
            }
            Ok(())
        })
    }

    /// CP-T3.2 (the ninth adversarial check's): goals.add (the goals panel's Add, an agent's
    /// goals_add) asked again under its key (its answer lost) adds its goals once: added twice,
    /// each would go to the agent as a message of its own (the same words, a second prompt).
    /// Another key is another request: its goals are added, the same words or not.
    #[test]
    fn goals_add_asked_again_under_its_key_adds_its_goals_once() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |c| {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
            let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd), name: Some("A".into()), count: Some(1), ..Ask::default() };
            let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let input = |key: &str| json!({"agent": a.id, "goals": ["the parser takes every input", "the lexer too"], "start": false, "key": key});
            let texts = |s: crate::goals::Stack| s.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>();
            call(me.clone(), "goals.add", input("add-1")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let once = texts(crate::goals::get(&hub, &a.id).await?);
            call(me.clone(), "goals.add", input("add-1")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let again = texts(crate::goals::get(&hub, &a.id).await?);
            anyhow::ensure!(once.len() == 2 && again == once, "goals.add asked again under its key: {once:?}, then {again:?}");
            call(me.clone(), "goals.add", input("add-2")).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let other = texts(crate::goals::get(&hub, &a.id).await?);
            anyhow::ensure!(other.len() == 4, "another key, its goals added: {other:?}");
            // Goals given with what they wait for (main's G1), asked again under their key: added
            // once, waiting as first said.
            let waiting = json!({"agent": a.id, "goals": [{"text": "the docs say so", "needs": []}, {"text": "the notes too", "needs": [1]}], "start": false, "key": "add-3"});
            call(me.clone(), "goals.add", waiting.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let once = texts(crate::goals::get(&hub, &a.id).await?);
            call(me.clone(), "goals.add", waiting).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let s = crate::goals::get(&hub, &a.id).await?;
            let docs = s.items.iter().find(|i| i.text == "the docs say so").map(|i| i.id.clone());
            let notes = s.items.iter().find(|i| i.text == "the notes too").and_then(|i| i.needs.clone());
            anyhow::ensure!(once.len() == 6 && texts(s.clone()) == once, "goals with needs asked again under their key: {once:?}, then {:?}", texts(s));
            anyhow::ensure!(notes == docs.map(|d| vec![d]), "what it waits for: {notes:?}");
            Ok(())
        })
    }

    /// A hub whose agent's harness is a program that isn't there (it stands stopped, and a message
    /// waits in its queue), as the tests above have it.
    async fn stopped_agent() -> anyhow::Result<(Arc<crate::hub::Hub>, String, tempfile::TempDir)> {
        let hub = crate::hub::tests::scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |c| {
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
        let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
        let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd), name: Some("A".into()), count: Some(1), ..Ask::default() };
        let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?;
        Ok((hub, a.id, folder))
    }

    /// What agent `id`'s inbox took, once it has taken `n` (or after a while), each message then
    /// taken out of its queue as its harness would take it, and the agent stopped again.
    async fn taken_by(hub: &Arc<crate::hub::Hub>, id: &str, n: usize) -> anyhow::Result<Vec<String>> {
        let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
        let mut taken = Vec::new();
        for _ in 0..100 {
            taken = inbox.taken(id, 100).await?;
            if taken.len() >= n {
                break;
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        for m in &taken {
            let _ = hub.harness.command(hub, id, crate::harness::model::Command::QueueRemove { id: m.clone() }).await;
        }
        for _ in 0..200 {
            if hub.harness.info(id).is_some_and(|i| i.status == crate::harness::model::Status::Exited && i.queue.is_empty()) {
                break;
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        Ok(taken)
    }

    /// CP-T3.2 (the tenth adversarial check's): goals.add under a key with `start` (MCP goals_add's
    /// default; the goals panel's on a stack that was done), asked again after its answer was lost,
    /// prompts the agent no second time: its inbox takes the first goal's message once.
    #[test]
    fn goals_add_with_start_asked_again_under_its_key_is_no_second_prompt() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = stopped_agent().await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let input = json!({"agent": id, "goals": ["the parser takes every input", "the lexer too"], "start": true, "key": "add-1"});
            call(me.clone(), "goals.add", input.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let once = taken_by(&hub, &id, 1).await?;
            call(me.clone(), "goals.add", input).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let again = taken_by(&hub, &id, 2).await?;
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(once.len() == 1 && again == once, "goals.add asked again under its key: its inbox took {once:?}, then {again:?}; the stack {:?}", s.items.iter().map(|i| (i.status.clone(), i.text.clone())).collect::<Vec<_>>());
            Ok(())
        })
    }

    /// CP-T3.2 (the tenth adversarial check's): Resume (goals.control start: the goals panel's,
    /// Mission Control's and the held card's button, an agent's goals_control) asked again after
    /// its answer was lost prompts the agent no second time.
    #[test]
    fn a_resume_asked_again_is_no_second_prompt() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = stopped_agent().await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            call(me.clone(), "goals.add", json!({"agent": id, "goals": ["the parser takes every input"], "start": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let first = taken_by(&hub, &id, 1).await?;
            call(me.clone(), "goals.control", json!({"agent": id, "action": "pause"})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            call(me.clone(), "goals.control", json!({"agent": id, "action": "resume"})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let resumed = taken_by(&hub, &id, 2).await?;
            call(me.clone(), "goals.control", json!({"agent": id, "action": "resume"})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let again = taken_by(&hub, &id, 3).await?;
            anyhow::ensure!(again == resumed, "a resume asked again: its inbox took {first:?} for the goal, {resumed:?} once resumed, then {again:?} after the resume was asked again");
            Ok(())
        })
    }

    /// CP-T3.2 (the tenth adversarial check's): goals.done for the goal under way (no `id`, as kiln
    /// tells agents to say their goal is met), asked again after its answer was lost, ends that goal
    /// alone: not the next one too, nor sends the one after it. A skip named by its goal, asked
    /// again, skips no more.
    #[test]
    fn goals_done_or_skip_asked_again_ends_one_goal() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = stopped_agent().await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            call(me.clone(), "goals.add", json!({"agent": id, "goals": ["one", "two", "three", "four"], "start": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 1).await?;
            let done = json!({"agent": id, "evidence": "the tests pass"});
            call(me.clone(), "goals.done", done.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let after = taken_by(&hub, &id, 2).await?;
            let again = call(me.clone(), "goals.done", done).await;
            let taken = taken_by(&hub, &id, 3).await?;
            let st = |s: &crate::goals::Stack| s.items.iter().map(|i| (i.text.clone(), i.status.clone())).collect::<Vec<_>>();
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(
                s.items.iter().filter(|i| i.status == "met").count() == 1 && taken == after,
                "goals.done asked again: answered {:?}; the stack {:?}; its inbox took {after:?} after the first, {taken:?} after the second",
                again.map(|_| "ok").map_err(|(c, m)| format!("{c}: {m}")),
                st(&s)
            );
            // The goal under way skipped by its id, and the skip asked again: one skipped.
            let two = s.items.iter().find(|i| i.status == "active").map(|i| i.id.clone()).unwrap_or_default();
            let skip = json!({"agent": id, "action": "skip", "id": two});
            call(me.clone(), "goals.control", skip.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 3).await?;
            call(me.clone(), "goals.control", skip).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(s.items.iter().filter(|i| i.status == "skipped").count() == 1 && s.items.iter().any(|i| i.text == "three" && i.status == "active"), "a skip asked again: {:?}", st(&s));
            Ok(())
        })
    }

    /// CP-T3.2 (the eleventh adversarial check's): goals.add under a key with `start`, its answer
    /// lost, asked again under the key after the stack was paused in between (by the person, its
    /// agent or kiln's usage rules) is the same request: answered with the stack as it is, paused,
    /// and no second prompt.
    #[test]
    fn goals_add_with_start_asked_again_after_a_pause_is_no_second_prompt() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = stopped_agent().await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let input = json!({"agent": id, "goals": ["the parser takes every input"], "start": true, "key": "add-1"});
            call(me.clone(), "goals.add", input.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let once = taken_by(&hub, &id, 1).await?;
            call(me.clone(), "goals.control", json!({"agent": id, "action": "pause"})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            call(me.clone(), "goals.add", input).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let again = taken_by(&hub, &id, 2).await?;
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(once.len() == 1 && again == once && s.state == "paused", "goals.add asked again under its key after a pause: its inbox took {once:?}, then {again:?}; the stack is {} now", s.state);
            Ok(())
        })
    }

    /// CP-T3.2 (the eleventh adversarial check's): goals.done for the goal under way as kiln tells
    /// agents to say it (no `id`), its answer lost, asked again by the agent after the person
    /// skipped the next goal (by its id, as the goals panel does) ends nothing more: the agent's own
    /// end with those words is what it asks again, whatever ended since. On a clock that moves, each
    /// end at a time of its own (under one that stands still, every end is the newest).
    #[test]
    fn goals_done_asked_again_after_another_end_ends_nothing_more() -> anyhow::Result<()> {
        within(async {
            let clock = kiln_seams::clock::SimClock::new();
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(clock.clone()), |c| {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
            let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd), name: Some("A".into()), count: Some(1), ..Ask::default() };
            let id = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?.id;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let agent = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &id, None).await.map_err(anyhow::Error::msg)? };
            call(me.clone(), "goals.add", json!({"agent": id, "goals": ["one", "two", "three", "four"], "start": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 1).await?;
            clock.advance(Duration::from_secs(3));
            // The agent says its goal is met (no `id`); its answer is lost.
            let done = json!({"agent": id, "evidence": "the tests pass"});
            call(agent.clone(), "goals.done", done.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 2).await?;
            clock.advance(Duration::from_secs(3));
            // Meanwhile the person skips the next goal by its id (the goals panel's Skip).
            let two = crate::goals::get(&hub, &id).await?.items.iter().find(|i| i.status == "active").map(|i| i.id.clone()).unwrap_or_default();
            call(me.clone(), "goals.control", json!({"agent": id, "action": "skip", "id": two})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let before = taken_by(&hub, &id, 3).await?;
            clock.advance(Duration::from_secs(3));
            // The agent's call, asked again (its answer lost), 9 s after the first.
            let again = call(agent.clone(), "goals.done", done).await;
            let after = taken_by(&hub, &id, 4).await?;
            let s = crate::goals::get(&hub, &id).await?;
            let st: Vec<(String, String)> = s.items.iter().map(|i| (i.text.clone(), i.status.clone())).collect();
            anyhow::ensure!(
                s.items.iter().any(|i| i.text == "three" && i.status == "active") && after == before,
                "goals.done asked again after the person skipped the next goal: answered {:?}; the stack {st:?}; its inbox took {} messages before the retry, {} after",
                again.map(|_| "ok").map_err(|(c, m)| format!("{c}: {m}")),
                before.len(),
                after.len()
            );
            Ok(())
        })
    }

    /// CP-T3.2 (the twelfth adversarial check's): a resume under a key, its answer lost, asked again
    /// under the key after the stack was paused again (by its agent, or kiln's usage rules) prompts
    /// the agent no second time: it's answered with the stack as it is, paused; under another key, it
    /// resumes. A person's done under a key (no words: asked again unkeyed, it would end the goal
    /// under way then), asked again after the next goal was skipped, ends nothing more.
    #[test]
    fn a_control_or_an_end_asked_again_under_its_key_does_nothing_more() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = stopped_agent().await?;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let ask = |input: Value| {
                let me = me.clone();
                async move { call(me, if input.get("action").is_some() { "goals.control" } else { "goals.done" }, input).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}")) }
            };
            call(me.clone(), "goals.add", json!({"agent": id, "goals": ["one", "two", "three"], "start": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 1).await?;
            ask(json!({"agent": id, "action": "pause"})).await?;
            ask(json!({"agent": id, "action": "resume", "key": "r1"})).await?;
            let resumed = taken_by(&hub, &id, 2).await?;
            ask(json!({"agent": id, "action": "pause"})).await?;
            ask(json!({"agent": id, "action": "resume", "key": "r1"})).await?;
            let again = taken_by(&hub, &id, 3).await?;
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(again == resumed && s.state == "paused", "a resume asked again under its key after a pause: its inbox took {} messages, then {}; the stack is {}", resumed.len(), again.len(), s.state);
            ask(json!({"agent": id, "action": "resume", "key": "r2"})).await?;
            let other = taken_by(&hub, &id, 3).await?;
            anyhow::ensure!(other.len() == resumed.len() + 1, "a resume under another key: its inbox took {} messages", other.len());
            // A person's done (no words) under its key; the next goal skipped by its id; the done
            // asked again under its key.
            ask(json!({"agent": id, "key": "d1"})).await?;
            taken_by(&hub, &id, 4).await?;
            let two = crate::goals::get(&hub, &id).await?.items.iter().find(|i| i.status == "active").map(|i| i.id.clone()).unwrap_or_default();
            ask(json!({"agent": id, "action": "skip", "id": two})).await?;
            let before = taken_by(&hub, &id, 5).await?;
            ask(json!({"agent": id, "key": "d1"})).await?;
            let after = taken_by(&hub, &id, 6).await?;
            let s = crate::goals::get(&hub, &id).await?;
            let st: Vec<(String, String)> = s.items.iter().map(|i| (i.text.clone(), i.status.clone())).collect();
            anyhow::ensure!(s.items.iter().any(|i| i.text == "three" && i.status == "active") && after == before, "a done asked again under its key: the stack {st:?}; its inbox took {} messages, then {}", before.len(), after.len());
            Ok(())
        })
    }

    /// Agent `id`'s harness at work in a turn its machine began at `t` (by that machine's clock), as
    /// its runner reports it to the server.
    /// The agent's session at a turn begun at `t` (by its machine's clock), as the server sees it:
    /// said again while a state its session sent before this (a queue command's, on its way) comes in
    /// after it, until the server has held it a while.
    async fn in_turn(hub: &Arc<crate::hub::Hub>, id: &str, t: i64) -> anyhow::Result<()> {
        let s = hub.harness.local.get(id).ok_or_else(|| anyhow::anyhow!("no session {id}"))?;
        let mut steady = 0;
        for _ in 0..250 {
            if hub.harness.info(id).is_some_and(|i| i.turn_started == Some(t)) {
                steady += 1;
                if steady >= 25 {
                    return Ok(());
                }
            } else {
                steady = 0;
                let state = {
                    let mut core = s.core.lock();
                    core.state.turn_started = Some(t);
                    core.state.clone()
                };
                hub.harness.ingest(hub, &crate::harness::registry::Location::Local, id, crate::harness::core::Update::State { state: Box::new(state) });
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("the server doesn't see the turn")
    }

    /// CP-T3.2 (the twelfth adversarial check's): the agent's goals.done (no `id`), its answer lost,
    /// asked again from the same turn while its machine's clock runs 5 s ahead of the server's, ends
    /// nothing more: the end keeps the turn it was made in, by that machine's clock, and the retry is
    /// matched to it there. Then, its clock set back (NTP stepping it) and a later turn begun, the
    /// same words end the goal under way.
    #[test]
    fn goals_done_asked_again_is_matched_to_its_turn_whatever_the_agent_s_clock() -> anyhow::Result<()> {
        use kiln_seams::clock::Clock as _;
        within(async {
            let clock = kiln_seams::clock::SimClock::new();
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(clock.clone()), |c| {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
            let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd), name: Some("A".into()), count: Some(1), ..Ask::default() };
            let id = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?.id;
            let me = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let agent = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &id, None).await.map_err(anyhow::Error::msg)? };
            call(me.clone(), "goals.add", json!({"agent": id, "goals": ["one", "two", "three", "four"], "start": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            taken_by(&hub, &id, 1).await?;
            // Its turn begins, by its machine's clock 5 s ahead of the server's.
            let begun = clock.unix_ms();
            in_turn(&hub, &id, begun + 5_000).await?;
            clock.advance(Duration::from_secs(3));
            let done = json!({"agent": id, "evidence": "the tests pass"});
            call(agent.clone(), "goals.done", done.clone()).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let first = taken_by(&hub, &id, 2).await?;
            clock.advance(Duration::from_secs(3));
            // Asked again from that turn, its answer lost.
            let again = call(agent.clone(), "goals.done", done.clone()).await;
            let after = taken_by(&hub, &id, 3).await?;
            let st = |s: &crate::goals::Stack| s.items.iter().map(|i| (i.text.clone(), i.status.clone())).collect::<Vec<_>>();
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(
                s.items.iter().filter(|i| i.status == "met").count() == 1 && s.items.iter().any(|i| i.text == "two" && i.status == "active") && after == first,
                "goals.done asked again from its turn, the agent's clock ahead: answered {:?}; the stack {:?}; its inbox took {} messages, then {}",
                again.map(|_| "ok").map_err(|(c, m)| format!("{c}: {m}")),
                st(&s),
                first.len(),
                after.len()
            );
            // Its clock set back 15 s, and a later turn begun (its start before the first end by
            // the server's clock): the same words end the goal under way, and the next goes.
            in_turn(&hub, &id, clock.unix_ms() - 10_000).await?;
            clock.advance(Duration::from_secs(3));
            call(agent.clone(), "goals.done", done).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let later = taken_by(&hub, &id, 3).await?;
            let s = crate::goals::get(&hub, &id).await?;
            anyhow::ensure!(
                s.items.iter().any(|i| i.text == "two" && i.status == "met") && s.items.iter().any(|i| i.text == "three" && i.status == "active") && later.len() == first.len() + 1,
                "goals.done from a later turn, the agent's clock set back: the stack {:?}; its inbox took {} messages, then {}",
                st(&s),
                first.len(),
                later.len()
            );
            Ok(())
        })
    }

    /// On a hub with a simulated clock: goals' times are its (CP-M2, AC-M2.1).
    #[test]
    fn an_agent_steers_the_goals_and_plans_of_itself_and_its_own_only() -> anyhow::Result<()> {
        within(async {
            // On a simulated clock (goals' times are its), with no conversation store: what's
            // steered is goals and plans, not conversations.
            let hub = crate::hub::tests::scratch_hub_on(Arc::new(kiln_seams::clock::SimClock::new()), |c| {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
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
            let theirs = crate::plans::agents::start(&hub, &p, config, None).await?.run;
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
