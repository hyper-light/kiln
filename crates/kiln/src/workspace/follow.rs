//! What happens to an agent's change request goes back to the agent (docs/agentic.md C2, C3), and
//! into its record, which Mission Control shows (C4):
//!
//! - a comment on it, by anyone but the agent, goes to the agent as a queued message: the file, the
//!   lines, the version, the words and a link;
//! - a review that asks for changes goes to it the same way; an approval only updates the record;
//! - CI that failed on its newest version goes to it with the failed legs and the first one's
//!   failure excerpt, when its person lets it fix failing checks (`fix_checks`, on unless they
//!   turned it off when opening the change);
//! - the land queue taking it out goes to it with the reason;
//! - landed: the record says so.
//!
//! The change's author (`agent:<id>`, from the agent token it was pushed with) is how an event
//! finds its agent. An agent that has ended isn't sent anything: its person sees the change in
//! review and in Mission Control, and can start an agent on it.

use std::sync::Arc;

use serde_json::Value;

use super::change::{self, Record};
use crate::harness::model::{Command, SendMode, Status};
use crate::hub::Hub;

/// Failure lines an agent gets with a failed check.
const EXCERPT_LINES: usize = 40;
/// Failed legs an agent is told about by name.
const LEGS_NAMED: usize = 8;

/// Follows review, CI and land queue events for the agents whose changes they are, for as long as
/// the server runs.
pub async fn follow_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    let mut rx = hub.events.subscribe();
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            m = rx.recv().fuse() => match m {
                Ok(text) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
                    if matches!(kind, "comment" | "review" | "diff_run" | "queue" | "landed") {
                        heard(&hub, kind, &v).await;
                    }
                }
                Err(async_broadcast::RecvError::Overflowed(_)) => {}
                Err(async_broadcast::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// The agent a change's author is (`agent:<id>`), when kiln keeps a change of its.
async fn record_of(hub: &Hub, author: Option<&str>, diff: Option<&str>) -> Option<Record> {
    let id = author?.strip_prefix("agent:")?.split(' ').next()?;
    let r = change::of(hub, id).await?;
    // The event is about this agent's change, not an older one of the same author.
    (diff.is_none() || r.change.as_deref() == diff).then_some(r)
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

async fn heard(hub: &Arc<Hub>, kind: &str, v: &Value) {
    match kind {
        "comment" => {
            let Some(r) = record_of(hub, s(v, "diff_author"), s(v, "diff")).await else { return };
            // Its own words (a reply it posted) aren't news to it.
            let by = s(v, "author").unwrap_or("someone");
            if by.starts_with("agent:") && by.contains(crate::harness::lineage::short(&r.agent)) {
                return;
            }
            tell(hub, &r, comment_text(&r, v)).await;
        }
        "review" => {
            let Some(mut r) = record_of(hub, s(v, "diff_author"), s(v, "diff")).await else { return };
            if let Some(state) = s(v, "state") {
                r.review = Some(state.to_owned());
                r.updated_at = crate::util::now_unix();
                change::save(hub, &r).await;
            }
            if s(v, "action") == Some("request-changes") {
                tell(hub, &r, review_text(&r, v)).await;
            }
        }
        "diff_run" => {
            let Some(mut r) = record_of(hub, s(v, "diff_author"), s(v, "diff")).await else { return };
            let version = v.get("version").and_then(Value::as_u64).unwrap_or_default();
            // A run of a version since replaced is old news.
            if version != 0 && version < u64::from(r.version) {
                return;
            }
            let conclusion = s(v, "conclusion").or_else(|| s(v, "status")).unwrap_or("finished").to_owned();
            let failed = matches!(conclusion.as_str(), "failure" | "failed" | "timed_out");
            r.checks = Some(if failed { "failed".to_owned() } else if conclusion == "success" || conclusion == "succeeded" { "passed".to_owned() } else { conclusion.clone() });
            r.updated_at = crate::util::now_unix();
            change::save(hub, &r).await;
            if failed && r.fix_checks {
                let text = checks_text(hub, &r, v).await;
                tell(hub, &r, text).await;
            }
        }
        "queue" => {
            let state = s(v, "state").unwrap_or_default();
            for d in v.get("diffs").and_then(Value::as_array).into_iter().flatten() {
                let Some(mut r) = record_of(hub, s(d, "author"), s(d, "diff")).await else { continue };
                match state {
                    "ejected" => {
                        let why = s(v, "reason").unwrap_or("no reason given");
                        let name = r.change.clone().unwrap_or_default();
                        tell(hub, &r, format!("Your change request {name} was taken out of the land queue: {why}\n\nFix what that says, then open a new version of it (agents.change). It's at {}", r.url)).await;
                    }
                    "landed" => {
                        r.state = "landed".to_owned();
                        r.updated_at = crate::util::now_unix();
                        change::save(hub, &r).await;
                    }
                    _ => {}
                }
            }
        }
        "landed" => {
            for name in v.get("diffs").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let Ok(Some(d)) = crate::ci::diffs::find(hub, name).await else { continue };
                let Some(mut r) = record_of(hub, Some(&d.author), Some(&d.name)).await else { continue };
                if r.state != "landed" {
                    r.state = "landed".to_owned();
                    r.updated_at = crate::util::now_unix();
                    change::save(hub, &r).await;
                }
            }
        }
        _ => {}
    }
}

/// Sends agent `r.agent` `text`, queued after its turn, when it still runs; for a step of a plan
/// run still going, once the run ends (`crate::plans::stack::held`).
async fn tell(hub: &Arc<Hub>, r: &Record, text: String) {
    if crate::plans::stack::held(hub, r, &text).await {
        return;
    }
    deliver(hub, &r.agent, text).await;
}

/// Sends agent `agent` `text`, queued after its turn, when it still runs.
pub async fn deliver(hub: &Hub, agent: &str, text: String) {
    let Some(info) = hub.harness.info(agent) else { return };
    if info.status == Status::Exited {
        return;
    }
    let message = crate::util::new_id();
    crate::harness::wait::sent(&info.id, info.last_turn.as_ref().map(|t| t.at));
    let sent = hub.harness.command(hub, &info.id, Command::Send { id: message, text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None }).await;
    if !matches!(sent, crate::harness::registry::Sent::Ok) {
        tracing::info!(agent = %info.id, "a change request's news didn't reach its agent");
    }
}

/// Where a comment is: `src/a.rs lines 10–14`, `src/a.rs line 3`, or nothing for the change as a
/// whole.
fn place(v: &Value) -> String {
    let Some(file) = s(v, "file") else { return String::new() };
    let start = v.get("start_line").and_then(Value::as_u64);
    let end = v.get("end_line").and_then(Value::as_u64);
    match (start, end) {
        (Some(a), Some(b)) if b > a => format!(" on {file} lines {a}–{b}"),
        (Some(a), _) => format!(" on {file} line {a}"),
        _ => format!(" on {file}"),
    }
}

fn comment_text(r: &Record, v: &Value) -> String {
    let name = r.change.clone().unwrap_or_default();
    let version = v.get("version").and_then(Value::as_u64).map(|n| format!(" (version {n})")).unwrap_or_default();
    let body = s(v, "body").unwrap_or_default();
    // Answering in its thread, where kiln has the operation for it (W14's review operations).
    let reply = match s(v, "thread") {
        Some(t) if crate::ci::ops::ops().iter().any(|o| o.name == "ci.diffs.reply") => format!(" To answer in its thread: ci.diffs.reply {{\"diff\": \"{name}\", \"thread\": \"{t}\", \"body\": …}}."),
        _ => String::new(),
    };
    format!(
        "A comment on your change request {name}{version}{}, from {}:\n\n{body}\n\nIf it asks for a change, make it and open a new version (agents.change).{reply} It's at {}",
        place(v),
        s(v, "author").unwrap_or("someone"),
        r.url
    )
}

fn review_text(r: &Record, v: &Value) -> String {
    let name = r.change.clone().unwrap_or_default();
    let body = s(v, "body").map(|b| format!(":\n\n{b}")).unwrap_or_else(|| ".".to_owned());
    format!("{} asked for changes to your change request {name}{body}\n\nMake them, then open a new version (agents.change). It's at {}", s(v, "reviewer").unwrap_or("A reviewer"), r.url)
}

async fn checks_text(hub: &Hub, r: &Record, v: &Value) -> String {
    let name = r.change.clone().unwrap_or_default();
    let workflow = s(v, "workflow").map(|w| format!(" ({w})")).unwrap_or_default();
    let legs: Vec<&Value> = v.get("failed").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let named: Vec<String> = legs.iter().take(LEGS_NAMED).filter_map(|l| s(l, "name")).map(str::to_owned).collect();
    let mut text = format!("CI failed on your change request {name}{workflow}");
    if !named.is_empty() {
        text.push_str(&format!(": {}", named.join(", ")));
        if legs.len() > named.len() {
            text.push_str(&format!(" and {} more", legs.len().saturating_sub(named.len())));
        }
    }
    text.push('.');
    if let Some(leg) = legs.first().and_then(|l| s(l, "leg"))
        && let Ok(f) = crate::ci::logsearch::failure(hub, leg, EXCERPT_LINES).await
    {
        let lines: Vec<&str> = f.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("text").and_then(Value::as_str)).collect();
        if !lines.is_empty() {
            let step = s(&f, "step_name").map(|n| format!(", step \"{n}\"")).unwrap_or_default();
            text.push_str(&format!("\n\nWhere {} failed{step}:\n```\n{}\n```", named.first().map_or("it", String::as_str), lines.join("\n")));
        }
        text.push_str(&format!("\n\nThe whole log: ci.legs.logs (leg {leg}); its failure: ci.legs.failure_excerpt."));
    }
    text.push_str(&format!("\n\nFix it, then open a new version (agents.change). It's at {}", r.url));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Record {
        Record { agent: "0192abcd00000000000000000000beef".into(), change: Some("CR12".into()), url: "https://k/diffs/CR12".into(), version: 3, fix_checks: true, ..Record::default() }
    }

    #[test]
    fn a_comment_says_where_it_is_and_what_it_says() {
        let v = json!({"type": "comment", "diff": "CR12", "version": 3, "author": "user:ada", "body": "Use the helper here.", "file": "src/a.rs", "start_line": 10, "end_line": 14});
        let t = comment_text(&record(), &v);
        assert!(t.contains("CR12 (version 3) on src/a.rs lines 10–14, from user:ada") && t.contains("Use the helper here.") && t.contains("https://k/diffs/CR12"), "{t}");
        let whole = comment_text(&record(), &json!({"diff": "CR12", "author": "user:ada", "body": "Nice."}));
        assert!(whole.contains("CR12, from user:ada"), "{whole}");
        assert_eq!(place(&json!({"file": "a", "start_line": 3, "end_line": 3})), " on a line 3");
    }

    #[test]
    fn a_review_asking_for_changes_says_what_they_are() {
        let t = review_text(&record(), &json!({"reviewer": "user:ada", "action": "request-changes", "body": "Split this in two."}));
        assert!(t.starts_with("user:ada asked for changes to your change request CR12:") && t.contains("Split this in two."), "{t}");
    }
}
