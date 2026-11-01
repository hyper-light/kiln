//! What happens to an agent's change request goes back to the agent (docs/agentic.md C2, C3), and
//! into its record, which Mission Control shows (C4):
//!
//! - a comment on it, by anyone but the agent, goes to the agent as a queued message: who wrote it
//!   (and whether that's its own person), its thread, the file and lines its thread is on with their
//!   version and commit (a reply's read from its thread), its words quoted, and a link (W26-R18);
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
            let p = place_of(hub, v).await;
            tell(hub, &r, comment_text(&r, v, &p)).await;
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
    format!(
        "{who}{} {did} on your {}change {name}{}. {who}'s words, quoted:\n\n{quoted}\nIf they ask for a change, make it and open a new version (agents.change).{reply} It's at {}",
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

    #[test]
    fn a_review_asking_for_changes_says_what_they_are() {
        let t = review_text(&record(), &json!({"reviewer": "user:ada", "action": "request-changes", "body": "Split this in two."}));
        assert!(t.starts_with("user:ada asked for changes to your change request CR12:") && t.contains("Split this in two."), "{t}");
    }
}
