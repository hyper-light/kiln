//! What happens to an agent's change request goes back to the agent (docs/agentic.md C2, C3), and
//! into its record, which Mission Control shows (C4):
//!
//! - a comment on it, by anyone but the agent, goes to the agent as a queued message: who wrote it
//!   (and whether that's its own person), its thread, the file and lines its thread is on with their
//!   version and commit (a reply's read from its thread), its words quoted, and a link (W26-R18);
//! - a review that asks for changes goes to it the same way; an approval only updates the record;
//! - agents' words (another agent's, a CI job's) go only where the change's repository lets agents
//!   fix code from agents' feedback (its switch in Repos, off by default), each saying why kiln sent
//!   it, and kiln keeps a hand-off so Ship reads the agent fixing until its next version; a thread
//!   where agents have started 3 fixes since a person spoke in it waits for a person; agents'
//!   feedback for an agent that has ended is held, and the item says it waits for a person to start
//!   or resume it (W26-AC10, `kiln_ci::review::agents` decides);
//! - a reply in a thread a review agent opened goes to that agent too, by the same rule, so it can
//!   resolve what was fixed (W26-R18);
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

use kiln_ci::review::agents::{self as decide, Heard, Route, Speaker, Why};

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
            let author = record_of(hub, s(v, "diff_author"), s(v, "diff")).await;
            let by = s(v, "author").unwrap_or("someone");
            let p = place_of(hub, v).await;
            let fixes = match &p.thread {
                Some(t) => fixes_in(hub, t, s(v, "comment")).await,
                None => 0,
            };
            if let Some(r) = &author {
                routed(hub, r, v, by, fixes, comment_text(r, v, &p)).await;
            }
            // A review agent's own threads (W26-R18): what's said in them reaches it too.
            if let Some(t) = &p.thread
                && let Some((reviewer, first)) = thread_agent(hub, t).await
                && author.as_ref().is_none_or(|r| r.agent != reviewer)
            {
                reviewed(hub, &reviewer, &first, v, by, fixes, &p).await;
            }
        }
        "review" => {
            let Some(mut r) = record_of(hub, s(v, "diff_author"), s(v, "diff")).await else { return };
            if let Some(state) = s(v, "state") {
                r.review = Some(state.to_owned());
                r.updated_at = crate::util::now_unix();
                change::save(hub, &r).await;
            }
            if s(v, "action") == Some("request-changes") {
                routed(hub, &r, v, s(v, "reviewer").unwrap_or("someone"), 0, review_text(&r, v)).await;
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

/// Feedback on agent `r`'s change, by `by` (event `v`; `fixes`, agents' fixes in its thread since a
/// person spoke there), to the agent or nowhere, as `kiln_ci::review::agents` decides (W26-R18,
/// W26-AC10): a person's words as `text`; agents' words, where the repository lets them start a fix,
/// as `text` with why kiln sent it, and a hand-off by kiln so Ship reads the agent fixing; held for
/// a person when the agent has ended, which the item says; otherwise nowhere.
async fn routed(hub: &Arc<Hub>, r: &Record, v: &Value, by: &str, fixes: u32, text: String) {
    let speaker = Speaker::of(by);
    // Its own words (a reply it posted) aren't news to it.
    let own = own_words(hub, by, s(v, "comment"), &r.agent).await;
    let (agents_fix, set) = match s(v, "repo").or(r.repo.as_deref()) {
        Some(repo) => crate::ci::agent_fixes::of_named(hub, repo).await,
        None => (false, None),
    };
    let info = hub.harness.info(&r.agent);
    let runs = info.as_ref().is_some_and(|i| i.status != Status::Exited);
    let name = info.map(|i| i.name).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("agent {}", crate::harness::lineage::short(&r.agent)));
    match decide::route(Heard { speaker, own, agents_fix, fixes, runs }) {
        Route::Tell if speaker == Speaker::Person => {
            tell(hub, r, text).await;
        }
        Route::Tell => {
            let repo = s(v, "repo").or(r.repo.as_deref()).unwrap_or("its repository");
            let why = format!("{repo} lets agents fix code from agents' feedback ({})", crate::ci::agent_fixes::set_words(true, set.as_ref()));
            let thread = s(v, "thread").map(|t| format!(" in thread {t}")).unwrap_or_default();
            let count = format!("Agents have started {fixes} of {} fixes{thread} since a person last spoke there; at {} it waits for a person.", decide::FIXES_PER_THREAD, decide::FIXES_PER_THREAD);
            if tell(hub, r, format!("{text}\n\nWhy kiln sent you this: {why}. {count}")).await {
                keep(hub, r, "threads", &name, &why).await;
            }
        }
        Route::Hold(Why::Ended) if speaker != Speaker::Person => {
            keep(hub, r, crate::ship::HELD, &name, &format!("The fix waits for a person to start or resume {name}")).await;
        }
        Route::Hold(_) => {}
    }
}

/// Whether comment `comment`, by `by`, is agent `agent`'s own words (W26-R18): by its signature,
/// whose identity names its whole session, when kiln took one (an agent's words through its MCP
/// server are signed by its runner); else by its principal, which names only its id's start and
/// its runner (`agent:<start>@<runner>`), a start agents begun within a minute of each other share.
async fn own_words(hub: &Hub, by: &str, comment: Option<&str>, agent: &str) -> bool {
    if !by.starts_with("agent:") {
        return false;
    }
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        let id = c.to_owned();
        let signer: Option<String> = hub
            .store
            .call(move |db| {
                use rusqlite::OptionalExtension as _;
                Ok(db.query_row("SELECT signer FROM review_signatures WHERE kind = 'comment' AND id = ?1", [&id], |r| r.get(0)).optional()?)
            })
            .await
            .ok()
            .flatten();
        if let Some(Ok(identity)) = signer.map(|uri| kiln_ci::identity::chain::Identity::parse(&uri))
            && let kiln_ci::identity::chain::Actor::Agent { agent: signed, .. } = &identity.actor
        {
            return signed == agent;
        }
    }
    by.contains(crate::harness::lineage::short(agent))
}

/// The agent session that opened thread `thread`, by its first comment's signature (which names its
/// whole session), and that comment's id (what resolves the thread); none for a person's thread or
/// an unsigned one.
async fn thread_agent(hub: &Hub, thread: &str) -> Option<(String, String)> {
    let t = thread.to_owned();
    let first: Option<(String, Option<String>)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT c.id, (SELECT s.signer FROM review_signatures s WHERE s.kind = 'comment' AND s.id = c.id) FROM review_comments c WHERE c.thread_id = ?1 ORDER BY c.created_at, c.rowid LIMIT 1",
                [&t],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten();
    let (comment, signer) = first?;
    match kiln_ci::identity::chain::Identity::parse(&signer?).ok()?.actor {
        kiln_ci::identity::chain::Actor::Agent { agent, .. } => Some((agent, comment)),
        _ => None,
    }
}

/// What's said by `by` (event `v`) in a thread review agent `agent` opened (its first comment
/// `first`), to that agent, by the same rule as feedback on an agent's own change: a person's
/// always, an agent's where the repository lets agents act on agents' feedback, within the thread's
/// bound, its own never, and nothing once it has ended (W26-R18).
async fn reviewed(hub: &Arc<Hub>, agent: &str, first: &str, v: &Value, by: &str, fixes: u32, p: &Place) {
    let speaker = Speaker::of(by);
    let own = own_words(hub, by, s(v, "comment"), agent).await;
    let agents_fix = match s(v, "repo") {
        Some(repo) => crate::ci::agent_fixes::of_named(hub, repo).await.0,
        None => false,
    };
    let runs = hub.harness.info(agent).is_some_and(|i| i.status != Status::Exited);
    if decide::route(Heard { speaker, own, agents_fix, fixes, runs }) != Route::Tell {
        return;
    }
    let diff = s(v, "diff").unwrap_or_default();
    let who = kiln_ci::review::byline(by, None, None, None).words();
    let thread = p.thread.clone().unwrap_or_default();
    let quoted: String = s(v, "body").unwrap_or_default().lines().map(|l| if l.is_empty() { ">\n".to_owned() } else { format!("> {l}\n") }).collect();
    let text = format!(
        "{who} replied in thread {thread} on {diff}, which you're reviewing{}. {who}'s words, quoted:\n\n{quoted}\nIf it's fixed, resolve the thread: ci.diffs.resolve {{\"comment\": \"{first}\"}}. Otherwise answer in it: ci.diffs.reply {{\"diff\": \"{diff}\", \"thread\": \"{thread}\", \"body\": …}}.",
        place_words(p)
    );
    deliver(hub, agent, text).await;
}

/// Keeps what kiln did with agents' feedback on agent `r`'s change at its newest version (`what`:
/// `threads` handed to it, or held), as Ship reads it (`crate::ship::kiln_kept`).
async fn keep(hub: &Hub, r: &Record, what: &str, name: &str, why: &str) {
    let Some(diff) = r.change.as_deref() else { return };
    let Ok(Some(d)) = crate::ci::diffs::find(hub, diff).await else { return };
    let Some(v) = crate::ci::diffs::versions(hub, &d).await.ok().and_then(|vs| vs.last().cloned()) else { return };
    if let Err(e) = crate::ship::kiln_kept(hub, &d.repo, &v.commit, v.n, what, name, &r.agent, why).await {
        tracing::warn!(agent = %r.agent, diff, error = %format!("{e:#}"), "what kiln did with agents' feedback wasn't kept");
    }
}

/// Fixes agents' feedback had started in thread `thread` since a person last spoke there, as of its
/// comment `comment` (what came after it doesn't count): its change's versions whose why is an
/// agent's fix answering it, after its newest comment by a person.
async fn fixes_in(hub: &Hub, thread: &str, comment: Option<&str>) -> u32 {
    let (t, comment) = (thread.to_owned(), comment.unwrap_or_default().to_owned());
    let found = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            let until: i64 = c.query_row("SELECT created_at FROM review_comments WHERE id = ?1", [&comment], |r| r.get(0)).optional()?.unwrap_or(i64::MAX);
            let mut st = c.prepare("SELECT author, created_at FROM review_comments WHERE thread_id = ?1 AND published = 1 AND created_at <= ?2")?;
            let said: Vec<(String, i64)> = st.query_map(rusqlite::params![t, until], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let mut st = c.prepare("SELECT v.thread, v.note, v.created_at FROM diff_versions v JOIN review_threads t ON t.diff_id = v.diff_id WHERE t.id = ?1 AND v.why = ?2 AND v.created_at <= ?3")?;
            let fixes: Vec<(Option<String>, Option<String>, i64)> = st.query_map(rusqlite::params![t, decide::AGENT_FIX, until], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            Ok((said, fixes))
        })
        .await;
    let Ok((said, fixes)) = found else { return 0 };
    let said: Vec<(&str, i64)> = said.iter().map(|(p, at)| (p.as_str(), *at)).collect();
    let answering: Vec<i64> = fixes
        .into_iter()
        .filter(|(first, note, _)| crate::ci::diffs::answers(first.as_deref(), note.as_deref(), thread))
        .map(|(_, _, at)| at)
        .collect();
    decide::fixes_since_a_person(&said, &answering)
}

/// Sends agent `r.agent` `text`, queued after its turn, when it still runs; for a step of a plan
/// run still going, once the run ends (`crate::plans::stack::held`). Whether it went or waits.
async fn tell(hub: &Arc<Hub>, r: &Record, text: String) -> bool {
    if crate::plans::stack::held(hub, r, &text).await {
        return true;
    }
    deliver(hub, &r.agent, text).await
}

/// Sends agent `agent` `text`, queued after its turn, when it still runs; whether it went.
pub async fn deliver(hub: &Hub, agent: &str, text: String) -> bool {
    let Some(info) = hub.harness.info(agent) else { return false };
    if info.status == Status::Exited {
        return false;
    }
    let message = crate::util::new_id();
    crate::harness::wait::sent(&info.id, info.last_turn.as_ref().map(|t| t.at));
    let sent = hub.harness.command(hub, &info.id, Command::Send { id: message, text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }).await;
    if !sent.went() {
        tracing::info!(agent = %info.id, "a change request's news didn't reach its agent");
    }
    sent.went()
}

/// Where a comment is, as its thread has it (W26-R18): the thread, the file and lines it's on, the
/// version of the change those lines are on and that version's commit, whether the comment answers
/// one before it, and whether the change is a draft. A reply's own request says none of this.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Place {
    thread: Option<String>,
    file: Option<String>,
    start: Option<u64>,
    end: Option<u64>,
    version: Option<u64>,
    commit: Option<String>,
    reply: bool,
    draft: bool,
}

/// `v`'s place: read from its thread when it has one (a reply's event carries no file or lines),
/// else from the event itself.
async fn place_of(hub: &Hub, v: &Value) -> Place {
    let from_event = Place {
        thread: s(v, "thread").map(str::to_owned),
        file: s(v, "file").map(str::to_owned),
        start: v.get("start_line").and_then(Value::as_u64),
        end: v.get("end_line").and_then(Value::as_u64),
        version: v.get("version").and_then(Value::as_u64),
        ..Place::default()
    };
    let Some(thread) = from_event.thread.clone() else { return from_event };
    let comment = s(v, "comment").unwrap_or_default().to_owned();
    let line = |n: Option<i64>| n.and_then(|n| u64::try_from(n).ok());
    let looked = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT t.file, t.start_line, t.end_line, t.version, v.commit_sha, d.state = 'draft',
                        EXISTS (SELECT 1 FROM review_comments x WHERE x.thread_id = t.id AND x.id < ?2)
                 FROM review_threads t JOIN diffs d ON d.id = t.diff_id LEFT JOIN diff_versions v ON v.diff_id = t.diff_id AND v.n = t.version
                 WHERE t.id = ?1",
                rusqlite::params![thread, comment],
                |r| {
                    Ok(Place {
                        thread: Some(thread.clone()),
                        file: r.get(0)?,
                        start: line(r.get(1)?),
                        end: line(r.get(2)?),
                        version: line(r.get(3)?),
                        commit: r.get(4)?,
                        draft: r.get(5)?,
                        reply: r.get(6)?,
                    })
                },
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten();
    looked.unwrap_or(from_event)
}

/// Where `p` is, in words: `, on src/a.rs lines 10–14 (version 5, commit 4141414)`, `, on src/a.rs
/// line 3`, `, on the whole change (version 2)`.
fn place_words(p: &Place) -> String {
    let lines = match (&p.file, p.start, p.end) {
        (Some(f), Some(a), Some(b)) if b > a => format!("{f} lines {a}–{b}"),
        (Some(f), Some(a), _) => format!("{f} line {a}"),
        (Some(f), None, _) => f.clone(),
        (None, ..) => "the whole change".to_owned(),
    };
    let version = match (p.version, &p.commit) {
        (Some(n), Some(c)) => format!(" (version {n}, commit {})", c.get(..7).unwrap_or(c)),
        (Some(n), None) => format!(" (version {n})"),
        (None, _) => String::new(),
    };
    format!(", on {lines}{version}")
}

/// A comment on agent `r`'s change, as its session is told it (docs/agentic.md C3, W26-R18): who
/// wrote it (and whether that's the agent's own person), its thread and where that is, and its words
/// quoted as theirs, never as kiln's own; then how to answer in its thread and make the change.
fn comment_text(r: &Record, v: &Value, p: &Place) -> String {
    let name = r.change.clone().unwrap_or_default();
    let author = s(v, "author").unwrap_or("someone");
    let who = kiln_ci::review::byline(author, None, None, None).words();
    let theirs = s(v, "diff_author_for").is_some_and(|f| kiln_ci::review::named(f) == kiln_ci::review::named(author));
    let did = match (&p.thread, p.reply) {
        (Some(t), true) => format!("replied in thread {t}"),
        (Some(t), false) => format!("started thread {t}"),
        (None, _) => "commented".to_owned(),
    };
    let quoted: String = s(v, "body").unwrap_or_default().lines().map(|l| if l.is_empty() { ">\n".to_owned() } else { format!("> {l}\n") }).collect();
    // Answering in its thread, where kiln has the operation for it (W14's review operations).
    let reply = match &p.thread {
        Some(t) if crate::ci::ops::ops().iter().any(|o| o.name == "ci.diffs.reply") => format!(" To answer in its thread: ci.diffs.reply {{\"diff\": \"{name}\", \"thread\": \"{t}\", \"body\": …}}."),
        _ => String::new(),
    };
    // A new version that answers its thread says so (W26-R18).
    let answers = p.thread.as_ref().map(|t| format!(" that answers this thread: agents.change {{\"answers\": [\"{t}\"]}}")).unwrap_or_else(|| " (agents.change)".to_owned());
    format!(
        "{who}{} {did} on your {}change {name}{}. {who}'s words, quoted:\n\n{quoted}\nIf they ask for a change, make it in a new version{answers}.{reply} It's at {}",
        if theirs { " (your person)" } else { "" },
        if p.draft { "draft " } else { "" },
        place_words(p),
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

    fn at(file: Option<&str>, start: Option<u64>, end: Option<u64>) -> Place {
        Place { thread: Some("T1".into()), file: file.map(Into::into), start, end, version: Some(5), commit: Some("4141414c0ffee".into()), reply: false, draft: false }
    }

    /// A reply carries no place of its own: the agent is told its thread's, with the version and
    /// commit its lines are on, who wrote it (here its own person) and their words quoted (W26-R18).
    #[test]
    fn w26_r18_a_reply_says_its_threads_place_and_who_wrote_it() {
        let v = json!({"type": "comment", "diff": "CR12", "thread": "T1", "version": 7, "author": "user:kim", "diff_author_for": "user:kim", "body": "still empty here?\n\nsay why"});
        let t = comment_text(&record(), &v, &Place { reply: true, ..at(Some("crates/kiln-ci/src/plan/matrix.rs"), Some(40), Some(42)) });
        assert!(t.starts_with("kim (your person) replied in thread T1 on your change CR12, on crates/kiln-ci/src/plan/matrix.rs lines 40–42 (version 5, commit 4141414)."), "{t}");
        assert!(t.contains("kim's words, quoted:\n\n> still empty here?\n>\n> say why\n\n"), "quoted as hers, never as kiln's: {t}");
        assert!(t.contains("ci.diffs.reply {\"diff\": \"CR12\", \"thread\": \"T1\", \"body\": …}") && t.contains("agents.change") && t.ends_with("https://k/diffs/CR12"), "{t}");
    }

    /// A new thread says so; someone else's words aren't its person's; a draft says it's one; a
    /// comment on the whole change says that.
    #[test]
    fn w26_r18_a_new_thread_on_a_draft_says_whose_and_where() {
        let v = json!({"diff": "CR12", "thread": "T1", "author": "user:sam", "diff_author_for": "user:kim", "body": "Nice."});
        let t = comment_text(&record(), &v, &Place { draft: true, ..at(None, None, None) });
        assert!(t.starts_with("sam started thread T1 on your draft change CR12, on the whole change (version 5, commit 4141414). sam's words, quoted:\n\n> Nice.\n"), "{t}");
        assert_eq!(place_words(&at(Some("a.rs"), Some(3), Some(3))), ", on a.rs line 3 (version 5, commit 4141414)");
        assert_eq!(place_words(&Place { commit: None, ..at(Some("a.rs"), Some(3), Some(9)) }), ", on a.rs lines 3–9 (version 5)");
    }

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or_else(|_| Err(anyhow::anyhow!("not done within two minutes")))
    }

    /// What kiln's fake harness in `dir` was sent, each message's text.
    fn heard(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join(".fake-claude-messages"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|m| match m.pointer("/message/content") {
                Some(Value::String(t)) => t.clone(),
                Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            })
            .collect()
    }

    /// W26-R18: a person's comment reaches the agent with its place. Four agents (kiln's fake
    /// harness), each with a change of its own: a branch change request, a diff of a plan run's
    /// stack (its run over), a draft change request, and a draft diff of a stack (the bottom of a
    /// stack its agent pushed with `-o draft`). On each, kim starts a thread on src/a.rs lines 10–12
    /// and replies in it, as kiln's events say them: the reply's event names its thread and no
    /// place. Each agent hears both with the thread, its file and lines, the version and its commit,
    /// whether the change is a draft, and kim's words quoted as hers.
    #[cfg(unix)]
    #[test]
    fn w26_r18_a_persons_reply_reaches_the_agent_with_its_place() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = follow_loop(h).await;
            });
            let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-claude/claude");
            hub.store.call(|c| Ok(c.execute_batch("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('acme', 'acme/app', 'https://example.invalid/app.git', 1);")?)).await?;
            let mut agents = Vec::new();
            for (n, kind, state, name) in [(1u32, "change_request", "needs-review", "CR1"), (2, "diff", "needs-review", "D2"), (3, "change_request", "draft", "CR3"), (4, "diff", "draft", "D4")] {
                let dir = tempfile::tempdir()?;
                std::fs::write(dir.path().join(".fake-claude-messages"), "")?;
                let env = vec![("FAKE_CLAUDE_LINES".to_owned(), "1".to_owned()), ("FAKE_CLAUDE_EVERY_MS".to_owned(), "10".to_owned())];
                let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(fake.into()), env, ..Default::default() };
                let agent = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
                let commit = n.to_string().repeat(40);
                // D4 is a stack's, as a push with `-o draft` makes it: a draft diff in a stack of its own.
                let (stack_row, stack_id) = if n == 4 { (format!("INSERT INTO stacks (id, repo_id, trunk, created_at) VALUES ('s{n}', 'acme', 'main', 1);"), format!("'s{n}'")) } else { (String::new(), "NULL".to_owned()) };
                let sql = format!(
                    "{stack_row}
                     INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, stack_id, created_at, updated_at) VALUES ('d{n}', 'acme', {n}, '{kind}', 'change {n}', 'agent:{agent}', 'agent', 'user:kim', '{state}', 'main', {stack_id}, 1, 1);
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d{n}', 1, '{commit}', 'p', 'b', 'change {n}', 'agent:{agent}', 1);
                     INSERT INTO review_threads (id, diff_id, version, file, side, start_line, end_line, state, created_by, created_at, kind) VALUES ('th{n}', 'd{n}', 1, 'src/a.rs', 'new', 10, 12, 'open', 'user:kim', 1, 'question');
                     INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES ('c{n}a', 'th{n}', 'user:kim', 'why ten?', 1, 1, 1), ('c{n}b', 'th{n}', 'user:kim', 'still empty here?', 2, 2, 1);"
                );
                hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
                let stack = (n == 2).then(|| crate::workspace::change::InStack { run: "a-run-that-ended".into(), step: 1 });
                change::save(&hub, &Record { agent: agent.clone(), change: Some(name.into()), url: format!("https://k/diffs/{name}"), version: 1, stack, ..Record::default() }).await;
                agents.push((n, name, agent, dir));
            }
            for (n, name, agent, _) in &agents {
                // kim's new thread on the lines, as kiln emits it; then her reply in it, whose event
                // has the thread alone.
                let event = |comment: &str, body: &str, place: bool| crate::wire::events::Comment {
                    repo: "acme/app".into(),
                    diff: (*name).into(),
                    version: 1,
                    thread: format!("th{n}"),
                    comment: comment.into(),
                    author: "user:kim".into(),
                    body: body.into(),
                    file: place.then(|| "src/a.rs".into()),
                    side: "new".into(),
                    start_line: place.then_some(10),
                    end_line: place.then_some(12),
                    diff_author: format!("agent:{agent}"),
                    diff_author_for: Some("user:kim".into()),
                };
                hub.emit(&crate::wire::events::ServerEvent::Comment(Box::new(event(&format!("c{n}a"), "why ten?", true))));
                hub.emit(&crate::wire::events::ServerEvent::Comment(Box::new(event(&format!("c{n}b"), "still empty here?", false))));
            }
            for (n, name, _, dir) in &agents {
                let draft = if matches!(n, 3 | 4) { "draft " } else { "" };
                let first = format!("kim (your person) started thread th{n} on your {draft}change {name}, on src/a.rs lines 10–12 (version 1, commit {}).", n.to_string().repeat(7));
                let reply = format!("kim (your person) replied in thread th{n} on your {draft}change {name}, on src/a.rs lines 10–12 (version 1, commit {}).", n.to_string().repeat(7));
                let mut got = Vec::new();
                for _ in 0..500 {
                    got = heard(dir.path());
                    if got.iter().any(|t| t.starts_with(&reply)) {
                        break;
                    }
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                }
                anyhow::ensure!(got.iter().any(|t| t.starts_with(&first) && t.contains("> why ten?")), "{name}: the new thread, with its place: {got:?}");
                anyhow::ensure!(got.iter().any(|t| t.starts_with(&reply) && t.contains("kim's words, quoted:\n\n> still empty here?\n") && t.contains(&format!("\"thread\": \"th{n}\""))), "{name}: the reply, with its thread's place: {got:?}");
            }
            Ok(())
        })
    }

    /// W26-AC10, kiln's part, with kiln's fake harness: agents' feedback on an agent's change starts
    /// a fix only where its repository lets it. CR1 is a running agent's change request on acme/app;
    /// a review agent and kim talk in its thread th1 (each comment in the store as its event says it).
    /// - The switch off: the review agent's comment starts nothing (kim's after it reaches the agent;
    ///   the review agent's never does), and kiln keeps no hand-off.
    /// - On: the review agent's next comment reaches it, saying why kiln sent it (who turned the
    ///   switch on, when, and the thread's count), and kiln keeps a hand-off by kiln at CR1's newest
    ///   version, saying why.
    /// - Three versions later, each an agent's fix answering th1, its next agent comment starts
    ///   nothing; kim's still goes, and starts the count again, so the review agent's after hers goes.
    /// - CR2's agent has ended: the review agent's comment on it is held, kept with why.
    #[cfg(unix)]
    #[test]
    fn w26_ac10_agents_feedback_starts_a_fix_only_where_the_switch_is_on() -> anyhow::Result<()> {
        within(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = follow_loop(h).await;
            });
            let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-claude/claude");
            hub.store.call(|c| Ok(c.execute_batch("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('acme', 'acme/app', 'https://example.invalid/app.git', 1);")?)).await?;
            let mut agents = Vec::new();
            for (n, program) in [(1u32, fake.to_owned()), (2, "/nonexistent/kiln-test-no-harness".to_owned())] {
                let dir = tempfile::tempdir()?;
                std::fs::write(dir.path().join(".fake-claude-messages"), "")?;
                let env = vec![("FAKE_CLAUDE_LINES".to_owned(), "1".to_owned()), ("FAKE_CLAUDE_EVERY_MS".to_owned(), "10".to_owned())];
                let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program), env, ..Default::default() };
                let agent = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
                let commit = n.to_string().repeat(40);
                let sql = format!(
                    "INSERT INTO diffs (id, repo_id, number, kind, title, author, author_kind, author_for, state, trunk, created_at, updated_at) VALUES ('d{n}', 'acme', {n}, 'change_request', 'change {n}', 'agent:{agent}', 'agent', 'user:kim', 'needs-review', 'main', 1, 1);
                     INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at) VALUES ('d{n}', 1, '{commit}', 'p', 'b', 'change {n}', 'agent:{agent}', 1);
                     INSERT INTO review_threads (id, diff_id, version, file, side, start_line, end_line, state, created_by, created_at, kind) VALUES ('th{n}', 'd{n}', 1, 'src/a.rs', 'new', 10, 12, 'open', 'agent:review@box-a', 1, 'question');"
                );
                hub.store.call(move |c| Ok(c.execute_batch(&sql)?)).await?;
                change::save(&hub, &Record { agent: agent.clone(), repo: Some("acme/app".into()), change: Some(format!("CR{n}")), url: format!("https://k/diffs/CR{n}"), version: 1, ..Record::default() }).await;
                agents.push((agent, dir));
            }
            // CR2's agent's harness isn't there: once it has said so, it has ended.
            let ended = agents.get(1).map(|(a, _)| a.clone()).unwrap_or_default();
            for _ in 0..250 {
                if hub.harness.info(&ended).is_some_and(|i| i.status == Status::Exited) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            let (agent, dir) = agents.first().map(|(a, d)| (a.clone(), d.path().to_path_buf())).ok_or_else(|| anyhow::anyhow!("no agent"))?;
            // A comment in thread th{n}, in the store and as kiln emits it.
            let say = |n: u32, id: &str, by: &str, body: &str, at: i64| {
                let (hub, id, by, body, a) = (Arc::clone(&hub), id.to_owned(), by.to_owned(), body.to_owned(), if n == 1 { agent.clone() } else { ended.clone() });
                async move {
                    let (i, b, w) = (id.clone(), by.clone(), body.clone());
                    hub.store.call(move |c| Ok(c.execute("INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at, published) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)", rusqlite::params![i, format!("th{n}"), b, w, at])?)).await?;
                    hub.emit(&crate::wire::events::ServerEvent::Comment(Box::new(crate::wire::events::Comment {
                        repo: "acme/app".into(),
                        diff: format!("CR{n}"),
                        version: 1,
                        thread: format!("th{n}"),
                        comment: id,
                        author: by,
                        body,
                        file: None,
                        side: "new".into(),
                        start_line: None,
                        end_line: None,
                        diff_author: format!("agent:{a}"),
                        diff_author_for: Some("user:kim".into()),
                    })));
                    anyhow::Ok(())
                }
            };
            // Until the agent has heard words with `quoted`, or ten seconds; what it heard.
            let until = |quoted: &str| {
                let (dir, quoted) = (dir.clone(), format!("> {quoted}"));
                async move {
                    let mut got = Vec::new();
                    for _ in 0..500 {
                        got = heard(&dir);
                        if got.iter().any(|t| t.contains(&quoted)) {
                            break;
                        }
                        crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    got
                }
            };
            let kept = |what: &'static str| {
                let hub = Arc::clone(&hub);
                async move { hub.store.call(move |c| Ok(c.query_row("SELECT COUNT(*) FROM ship_handoffs WHERE what = ?1 AND by = 'kiln'", [what], |r| r.get::<_, i64>(0))?)).await }
            };

            // The switch off (the default): the review agent's words go nowhere; kim's after them do.
            say(1, "c1", "agent:review@box-a", "an empty matrix should mean no legs", 2).await?;
            say(1, "c2", "user:kim", "agreed: no legs", 3).await?;
            let got = until("agreed: no legs").await;
            anyhow::ensure!(got.iter().any(|t| t.contains("> agreed: no legs")), "kim's words reach it: {got:?}");
            anyhow::ensure!(!got.iter().any(|t| t.contains("> an empty matrix should mean no legs")), "the review agent's don't, with the switch off: {got:?}");
            anyhow::ensure!(kept("threads").await? == 0, "no hand-off with the switch off");

            // On, turned on by ada at noon on 31 Oct 2026: the review agent's words go, saying why.
            hub.store.call(|c| Ok(c.execute_batch("INSERT INTO review_agent_fixes (repo_id, agents_fix, set_by, set_at) VALUES ('acme', 1, 'user:ada', 1793448000);")?)).await?;
            say(1, "c3", "agent:review@box-a", "still one leg for an empty matrix", 4).await?;
            let got = until("still one leg").await;
            let sent = got.iter().find(|t| t.contains("> still one leg")).cloned().unwrap_or_default();
            anyhow::ensure!(sent.contains("Why kiln sent you this: acme/app lets agents fix code from agents' feedback (turned on by ada, 31 Oct 2026). Agents have started 0 of 3 fixes in thread th1"), "with why: {sent}");
            anyhow::ensure!(sent.contains("agents.change {\"answers\": [\"th1\"]}") && sent.contains("ci.diffs.reply"), "how to answer it: {sent}");
            let hand: (String, String, u32, String) = hub.store.call(|c| Ok(c.query_row("SELECT to_name, sha, version, why FROM ship_handoffs WHERE what = 'threads' AND by = 'kiln'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)).await?;
            anyhow::ensure!(hand.1 == "1".repeat(40) && hand.2 == 1 && hand.3.starts_with("acme/app lets agents fix code from agents' feedback"), "kiln's hand-off at CR1's newest version: {hand:?}");

            // Three agents' fixes answering th1 since kim spoke: the next agent's words go nowhere.
            hub.store
                .call(|c| {
                    Ok(c.execute_batch(
                        "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, message, pushed_by, created_at, why, thread, note) VALUES
                         ('d1', 2, 'v2', 'p', 'b', 'fix', 'agent:a', 5, 'agent''s fix', 'th1', 'answers th1'),
                         ('d1', 3, 'v3', 'p', 'b', 'fix', 'agent:a', 6, 'agent''s fix', 'th0', 'answers th0, th1'),
                         ('d1', 4, 'v4', 'p', 'b', 'fix', 'agent:a', 7, 'agent''s fix', 'th1', 'answers th1');",
                    )?)
                })
                .await?;
            anyhow::ensure!(fixes_in(&hub, "th1", None).await == 3, "three fixes counted, one by its note");
            say(1, "c4", "agent:review@box-a", "and the zero-axis case", 8).await?;
            say(1, "c5", "user:kim", "zero axes too, please", 9).await?;
            let got = until("zero axes too").await;
            anyhow::ensure!(got.iter().any(|t| t.contains("> zero axes too")) && !got.iter().any(|t| t.contains("> and the zero-axis case")), "at three fixes the thread waits for a person; kim's still goes: {got:?}");
            say(1, "c6", "agent:review@box-a", "and one-axis matrices", 10).await?;
            let got = until("and one-axis").await;
            anyhow::ensure!(got.iter().any(|t| t.contains("> and one-axis") && t.contains("Agents have started 0 of 3 fixes")), "kim started the count again: {got:?}");

            // CR2's agent has ended: held, and kept with why.
            say(2, "d1c", "agent:review@box-a", "a bad expression here", 11).await?;
            let mut held = String::new();
            for _ in 0..500 {
                held = hub.store.call(|c| Ok(c.query_row("SELECT COALESCE(MAX(why), '') FROM ship_handoffs WHERE what = 'held' AND by = 'kiln' AND sha = ?1", ["2".repeat(40)], |r| r.get::<_, String>(0))?)).await?;
                if !held.is_empty() {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
            anyhow::ensure!(held.starts_with("The fix waits for a person to start or resume "), "held for a person: {held:?}");
            Ok(())
        })
    }

    #[test]
    fn a_review_asking_for_changes_says_what_they_are() {
        let t = review_text(&record(), &json!({"reviewer": "user:ada", "action": "request-changes", "body": "Split this in two."}));
        assert!(t.starts_with("user:ada asked for changes to your change request CR12:") && t.contains("Split this in two."), "{t}");
    }
}
