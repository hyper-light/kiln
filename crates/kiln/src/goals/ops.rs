//! The goal stack operations (`goals.*`), in kiln's one operation table (`crate::ops::own`): the
//! MCP tools agents call (to line up goals for themselves or the agents they started) and code mode's
//! `kiln.goals.*`. Each is one of the console's `/api/harness/sessions/:id/goals` routes, which call
//! the same functions.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Action, At, By, Change, Error, Given, NewGoal, Pausing};
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
    let s = super::add_at(&cx.hub, &agent, goals.into_iter().map(NewGoal::from).collect(), start, &at_of(&v)?).await.map_err(fail)?;
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
    let action: Action =
        serde_json::from_value(v.get("action").cloned().unwrap_or(Value::Null)).map_err(|_| (StatusCode::BAD_REQUEST, "`action` is start (or resume), pause, skip, done, clear or restore".into()))?;
    let until = until_of(&v, cx.hub.clock.unix_ms())?;
    if action != Action::Pause && (until.is_some() || opt(&v, "why").is_some()) {
        return Err((StatusCode::BAD_REQUEST, "`why`, `resume_in_s` and `resume_at` go with pause".into()));
    }
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let s = super::control_as(&cx.hub, &agent, action, &by, Pausing { why: opt(&v, "why"), until, review: None }).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.control".into(), format!("{agent}: {action:?}")).await;
    json_of(&s)
}

async fn done(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let s = super::done(&cx.hub, &agent, opt(&v, "id").as_deref(), opt(&v, "evidence").as_deref(), &by).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.done".into(), format!("{agent}: {}", opt(&v, "id").unwrap_or_else(|| "the goal under way".into()))).await;
    json_of(&s)
}

async fn update(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let id = opt(&v, "id").ok_or_else(|| (StatusCode::BAD_REQUEST, "`id` is the goal's id (goals_get lists them)".to_owned()))?;
    let change: Change = serde_json::from_value(v.clone()).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let by = By::of(&cx.hub, &cx.caller, &agent);
    let s = super::update(&cx.hub, &agent, &id, change, &by).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.update".into(), format!("{agent}: {id}")).await;
    json_of(&s)
}

/// What an agent does once its goal has become a plan.
const PLANNING: &str = "Plan it now, in this order. 1. Research it, inside and outside, and record what you find as you go (research_add, with this research's id): inside, the files on your machine and on others (code, docs, data, pictures, frames of video) and kiln's own records (conversations_search, plans, CI runs, deployments); outside, the web, official docs and standards, papers (arXiv, Semantic Scholar, Crossref, PubMed), blogs and forums. Each finding is a claim with its citations (the source, the exact words you quote, where they are) and any counterclaims you found. kiln looks for each quote where you say it is and grades each finding: describe the evidence, never the grade. 2. Write the plan's steps (plans_update with the plan's id: `items`, each with what to do, its acceptance criteria, its tests, the steps it waits for in `needs`, and the findings it rests on in `basis`). 3. Say it's ready (plans_ready, with `then`: `goals` to carry on here with its steps as this goal's parts, `agents` to run it with an agent per step, `review` to pause for the person to look first). It answers with the research, graded: show it to the person in your reply, as it is, every claim with its sources and grade. Once every step is met, so is the goal.";

async fn plan(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
    let p = super::plan(&cx.hub, &agent, opt(&v, "id").as_deref(), opt(&v, "title").as_deref(), &cx.caller.name).await.map_err(fail)?;
    cx.hub.store.audit(cx.caller.name.clone(), "goals.plan".into(), format!("{agent}: {} as plan {}", p.goal, p.plan)).await;
    Ok(json!({"plan": p.plan, "research": p.research, "goal": p.goal, "part": p.part, "next": PLANNING}))
}

async fn split(cx: Cx, v: Value) -> Out {
    let agent = agent_of(&v)?;
    crate::harness::lineage::may_steer(&cx.hub, &cx.caller, &agent)?;
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
            summary: "Line goals up for an agent (for yourself, without `agent`), after the ones it has (or `next`, right after the goal under way; or `before` or `after` a goal still to go, by id): each goes to it once the one before is met (its harness's own /goal where it has one), like giving it /goal again and again. Runs the stack unless `start` is false. To pursue a document or list as a goal stack, give one goal per item, in order, each saying what done is.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "goals": {"type": "array", "items": {"type": "string"}, "description": "Each a condition or objective, one line."},
                        "start": {"type": "boolean"},
                        "next": {"type": "boolean", "description": "Right after the goal under way, before the rest."},
                        "before": {"type": "string", "description": "Before this goal still to go (its id). Before the goal under way, they go first and it waits after them."},
                        "after": {"type": "string", "description": "After this goal still to go (its id)."}
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
                        "evidence": {"type": "string", "description": "What shows it's met, a line or two: kept with the goal for the person to see."}
                    }),
                    &[],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", "done"]))),
            run: |cx, v| Box::pin(done(cx, v)),
        },
        Op {
            name: "goals.update",
            summary: "Change one goal still to go, by its `id`, without giving the whole list: `text` gives it other words (the goal under way goes to the agent again as they say); `next`, `before` or `after` (a goal id) moves it (put before the goal under way, it goes next and that one waits); `remove` takes it out (the goal under way ends as skipped; one to come goes, with its parts).",
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
                        "remove": {"type": "boolean", "description": "Take it out."}
                    }),
                    &["id"],
                )
            },
            route: |v| Ok((Method::PATCH, path(&["api", "harness", "sessions", &agent_of(v)?, "goals", &opt(v, "id").unwrap_or_default()]))),
            run: |cx, v| Box::pin(update(cx, v)),
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
