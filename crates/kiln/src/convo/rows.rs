//! What a session's updates become in the store (`docs/conversations.md`, "What is kept"): rows,
//! with no engine in sight, so the writer's batches and the replication's entries are the same
//! thing and both are tested without a database.

use serde::{Deserialize, Serialize};

use crate::harness::core::Update;
use crate::harness::model::{Body, Event, ToolStatus};

/// Longest text an event is found by (its words, a tool's title and output): what a search matches.
pub const SEARCH_TEXT_MAX: usize = 16 * 1024;

/// One change to the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum Row {
    /// What a session is: kept whole, the latest wins. Each part is JSON as the model serializes it
    /// (None: unchanged).
    Session {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        info: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changes: Option<String>,
        /// Its writer's epoch ([`State::epoch`](crate::harness::model::State::epoch)), from its
        /// state (None: unchanged). Epochs only rise: a session that moves counts one more.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<u64>,
        /// It (re)started: it isn't closed, whatever was kept.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        open: bool,
        at: i64,
    },
    /// An event as it is now (upserted by id).
    Event(Box<EventRow>),
    /// The timeline starts over (a restart, a move, a rewind): the session's events from number
    /// `from` on go (all of them: None), and those the snapshot after it brings come anew. Those
    /// before `from` stay: the timeline no longer holds them in memory, but they are its history.
    Reset {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u64>,
        at: i64,
    },
    /// The session is gone for good.
    Closed { session: String, at: i64 },
    /// A line of the harness's own record of the conversation.
    Record(Box<RecordRow>),
    /// The harness's record from `source` starts over (its file was replaced): its lines go.
    RecordReset { session: String, source: String, at: i64 },
    /// The machine the session runs on, as the store it came from has it (a mirror of the
    /// server's store keeps where each session runs).
    Machine { session: String, machine: String },
}

/// A line of the harness's record: Claude Code's transcript, a Codex rollout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordRow {
    pub session: String,
    /// The file it is from: Claude Code's session id; a Codex rollout's file name.
    pub source: String,
    /// Its number among the file's non-empty lines, from 0 (a move carries those alone, so a
    /// copy on another machine numbers them alike).
    pub n: u64,
    /// The byte just past it in the file: where reading goes on.
    pub end: u64,
    pub line: String,
}

/// An event, with what it is found by pulled out of its body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub session: String,
    pub id: String,
    pub seq: u64,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// "user", "assistant", "thinking", "tool", "notice".
    pub kind: String,
    /// A notice's level, a tool's kind ("shell", "edit" …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// A tool call's name ("Bash", "Edit" …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// A tool call's status ("done", "error" …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// What a search matches.
    pub text: String,
    /// The event as the model serializes it.
    pub body: String,
}

impl Row {
    pub fn session(&self) -> &str {
        match self {
            Row::Session { session, .. } | Row::Reset { session, .. } | Row::Closed { session, .. } | Row::RecordReset { session, .. } | Row::Machine { session, .. } => session,
            Row::Event(e) => &e.session,
            Row::Record(r) => &r.session,
        }
    }

    /// Its size as a feed sends it, near enough.
    pub fn bytes(&self) -> usize {
        match self {
            Row::Session { info, meta, state, changes, .. } => 64 + [info, meta, state, changes].iter().map(|p| p.as_ref().map_or(0, String::len)).sum::<usize>(),
            Row::Event(e) => 128 + e.text.len() + e.body.len(),
            Row::Record(r) => 64 + r.source.len() + r.line.len(),
            Row::Reset { .. } | Row::Closed { .. } | Row::RecordReset { .. } | Row::Machine { .. } => 64,
        }
    }
}

/// The rows one update of `session` makes, at `now`. Streaming text and replies to one viewer make
/// none: an event's text is kept as its update says once it's whole, and a reply is no one else's.
pub fn of(session: &str, update: &Update, now: i64) -> Vec<Row> {
    let s = || session.to_owned();
    match update {
        Update::Open { info } => vec![Row::Session { session: s(), info: json(info.as_ref()), meta: None, state: None, changes: None, epoch: None, open: true, at: now }],
        Update::State { state } => vec![Row::Session { session: s(), info: None, meta: None, state: json(state.as_ref()), changes: None, epoch: Some(state.epoch), open: false, at: now }],
        Update::Meta { meta } => vec![Row::Session { session: s(), info: None, meta: json(meta.as_ref()), state: None, changes: None, epoch: None, open: false, at: now }],
        Update::Changes { files } => vec![Row::Session { session: s(), info: None, meta: None, state: None, changes: json(files), epoch: None, open: false, at: now }],
        Update::Events { events } => events.iter().filter_map(|e| event(session, e)).map(|e| Row::Event(Box::new(e))).collect(),
        Update::Reset { from } => vec![Row::Reset { session: s(), from: *from, at: now }],
        Update::Closed => vec![Row::Closed { session: s(), at: now }],
        Update::Stream { .. } | Update::Reply { .. } => Vec::new(),
    }
}

/// An event's row.
pub fn event(session: &str, e: &Event) -> Option<EventRow> {
    let body = serde_json::to_string(e).ok()?;
    let (kind, level, tool, status, text) = match &e.body {
        Body::User { text, attachments, .. } => {
            let mut t = text.clone();
            for a in attachments {
                t.push('\n');
                t.push_str(a);
            }
            ("user", None, None, None, t)
        }
        Body::Assistant { text, .. } => ("assistant", None, None, None, text.clone()),
        Body::Thinking { text, .. } => ("thinking", None, None, None, text.clone()),
        Body::Notice { level, text } => ("notice", Some(level_word(level)), None, None, text.clone()),
        Body::Tool(t) => {
            let mut words = t.title.clone();
            for part in [t.detail.as_deref(), t.output.as_deref()].into_iter().flatten() {
                words.push('\n');
                words.push_str(part);
            }
            ("tool", Some(json_word(&t.tool)), Some(t.name.clone()), Some(status_word(t.status)), words)
        }
    };
    Some(EventRow {
        session: session.to_owned(),
        id: e.id.clone(),
        seq: e.seq,
        at: e.at,
        parent: e.parent.clone(),
        by: e.by.clone(),
        kind: kind.to_owned(),
        level,
        tool,
        status,
        text: clip(&text, SEARCH_TEXT_MAX),
        body,
    })
}

/// Rows in the order they came, with what a later one replaces dropped: one write per event and
/// per session part. A reset or a close keeps its place: what came before it of that session is
/// cut by it.
pub fn coalesce(rows: Vec<Row>) -> Vec<Row> {
    use std::collections::HashMap;
    // Index of the last row each key had; a reset or close forgets the session's earlier keys.
    let mut last: HashMap<(String, String), usize> = HashMap::new();
    let mut out: Vec<Option<Row>> = Vec::with_capacity(rows.len());
    for row in rows {
        match &row {
            Row::Event(e) => {
                let key = (e.session.clone(), format!("e:{}", e.id));
                if let Some(i) = last.get(&key).copied()
                    && let Some(slot) = out.get_mut(i)
                {
                    *slot = None;
                }
                last.insert(key, out.len());
                out.push(Some(row));
            }
            Row::Session { session, info, meta, state, changes, epoch, open, at } => {
                let key = (session.clone(), "s".to_owned());
                // A later part replaces the same part; the others carry over.
                if let Some(i) = last.get(&key).copied()
                    && let Some(Some(Row::Session { info: i0, meta: m0, state: s0, changes: c0, epoch: e0, open: o0, .. })) = out.get_mut(i).map(Option::take)
                {
                    let merged = Row::Session {
                        session: session.clone(),
                        info: info.clone().or(i0),
                        meta: meta.clone().or(m0),
                        state: state.clone().or(s0),
                        changes: changes.clone().or(c0),
                        epoch: epoch.or(e0),
                        open: *open || o0,
                        at: *at,
                    };
                    last.insert(key, out.len());
                    out.push(Some(merged));
                    continue;
                }
                last.insert(key, out.len());
                out.push(Some(row));
            }
            Row::Machine { session, .. } => {
                let key = (session.clone(), "m".to_owned());
                if let Some(i) = last.get(&key).copied()
                    && let Some(slot) = out.get_mut(i)
                {
                    *slot = None;
                }
                last.insert(key, out.len());
                out.push(Some(row));
            }
            Row::Record(r) => {
                let key = (r.session.clone(), format!("r:{}:{}", r.source, r.n));
                if let Some(i) = last.get(&key).copied()
                    && let Some(slot) = out.get_mut(i)
                {
                    *slot = None;
                }
                last.insert(key, out.len());
                out.push(Some(row));
            }
            Row::RecordReset { session, source, .. } => {
                // Its lines before it go with it: the reset would drop them anyway.
                let prefix = format!("r:{source}:");
                let s = session.clone();
                last.retain(|(sess, k), i| {
                    let cut = sess == &s && k.starts_with(&prefix);
                    if cut && let Some(slot) = out.get_mut(*i) {
                        *slot = None;
                    }
                    !cut
                });
                out.push(Some(row));
            }
            Row::Reset { session, .. } => {
                let s = session.clone();
                last.retain(|(sess, k), _| !(sess == &s && k.starts_with("e:")));
                out.push(Some(row));
            }
            Row::Closed { session, .. } => {
                // A part after the close is the session's next life (reopened): it stays after it.
                let s = session.clone();
                last.retain(|(sess, k), _| !(sess == &s && (k.starts_with("e:") || k == "s")));
                out.push(Some(row));
            }
        }
    }
    out.into_iter().flatten().collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_owned()
}

fn level_word(level: &crate::harness::model::Level) -> String {
    json_word(level)
}

fn status_word(s: ToolStatus) -> String {
    json_word(&s)
}

/// An enum's word as serde writes it ("shell", "error"…).
fn json_word<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// A model value as JSON.
fn json<T: Serialize + ?Sized>(v: &T) -> Option<String> {
    serde_json::to_string(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Level, State, ToolCall, ToolKind};

    fn ev(id: &str, seq: u64, body: Body) -> Event {
        Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body }
    }

    #[test]
    fn events_are_found_by_their_words_and_tools() -> anyhow::Result<()> {
        let tool = ToolCall {
            tool_id: "t1".into(),
            name: "Bash".into(),
            tool: ToolKind::Shell,
            title: "cargo test".into(),
            detail: None,
            input: serde_json::json!({}),
            status: ToolStatus::Error,
            output: Some("1 failed".into()),
            truncated: false,
            diffs: vec![],
            todos: vec![],
            task: None,
            exit_code: Some(101),
            started: 1,
            ended: Some(2),
            server: None,
            tool_short: None,
            skill: None,
            runner: None,
        };
        let rows = of("a1", &Update::Events { events: vec![ev("t1", 3, Body::Tool(Box::new(tool))), ev("n1", 4, Body::Notice { level: Level::Warn, text: "Paused".into() })] }, 9);
        let Some(Row::Event(t)) = rows.first() else { anyhow::bail!("no tool row") };
        assert_eq!((t.kind.as_str(), t.tool.as_deref(), t.status.as_deref(), t.level.as_deref()), ("tool", Some("Bash"), Some("error"), Some("shell")));
        assert!(t.text.contains("cargo test") && t.text.contains("1 failed"));
        let Some(Row::Event(n)) = rows.get(1) else { anyhow::bail!("no notice row") };
        assert_eq!((n.kind.as_str(), n.level.as_deref(), n.text.as_str()), ("notice", Some("warn"), "Paused"));
        // The body is the event as the model writes it: it reads back the same.
        let back: Event = serde_json::from_str(&t.body)?;
        assert_eq!(back.id, "t1");
        Ok(())
    }

    #[test]
    fn streaming_and_replies_make_no_rows() {
        assert!(of("a1", &Update::Stream { id: "m".into(), text: "par".into() }, 1).is_empty());
        assert!(of("a1", &Update::Reply { client: "c".into(), value: serde_json::Value::Null }, 1).is_empty());
    }

    #[test]
    fn a_batch_keeps_one_write_per_event_and_part() {
        let a = |text: &str| Update::Events { events: vec![ev("m1", 1, Body::Assistant { text: text.into(), streaming: false })] };
        let mut rows = of("a1", &a("first"), 1);
        rows.extend(of("a1", &Update::State { state: Box::new(State { title: "one".into(), ..State::default() }) }, 2));
        rows.extend(of("a1", &a("second"), 3));
        rows.extend(of("a1", &Update::Changes { files: vec![] }, 4));
        rows.extend(of("a1", &Update::State { state: Box::new(State { title: "two".into(), ..State::default() }) }, 5));
        let out = coalesce(rows);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(matches!(out.first(), Some(Row::Event(e)) if e.text == "second"));
        assert!(matches!(out.get(1), Some(Row::Session { state: Some(s), changes: Some(_), .. }) if s.contains("\"two\"")));
    }

    #[test]
    fn a_reset_cuts_what_came_before_it() {
        let a = |id: &str| Update::Events { events: vec![ev(id, 1, Body::Assistant { text: id.into(), streaming: false })] };
        let mut rows = of("a1", &a("m1"), 1);
        rows.extend(of("a1", &Update::Reset { from: None }, 2));
        rows.extend(of("a1", &a("m1"), 3));
        let out = coalesce(rows);
        // The event before the reset stays (the reset drops it in the store), and the one after
        // isn't merged into it.
        assert_eq!(out.len(), 3);
        assert!(matches!(out.get(1), Some(Row::Reset { .. })));
    }

    #[test]
    fn search_text_is_clipped_on_a_char_boundary() {
        let s = "é".repeat(SEARCH_TEXT_MAX);
        let c = clip(&s, SEARCH_TEXT_MAX);
        assert!(c.len() <= SEARCH_TEXT_MAX && c.chars().all(|ch| ch == 'é'));
    }
}
