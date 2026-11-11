//! What a session's update takes of memory while it waits in a queue (goal 11, plan 01a1a87b2e56 step 3;
//! `crate::memory::queue::Weigh`): its place and the text it carries, counted by walking its strings' lengths
//! (nothing copied, nothing encoded). The texts are where an update's bytes are: an event's words, a
//! tool's input, output and diffs, a changeset's hunks. A state, a meta or a session's info is counted by
//! its place: what it holds past that is a few names and a person's queued words.

use serde_json::Value;

use super::core::Update;
use super::model::{Body, Event, FileChange, FileDiff, FileKey, Hunk, ToolCall};
use crate::memory::queue::Weigh;

/// The bytes of what an optional text holds.
fn maybe(s: Option<&String>) -> usize {
    s.map_or(0, String::len)
}

/// The bytes of a list of texts: each one's place and its text.
fn texts(l: &[String]) -> usize {
    l.iter().map(|t| size_of::<String>().saturating_add(t.len())).fold(0usize, usize::saturating_add)
}

/// A JSON value's bytes: its node, its strings, and its children's, each key with its value.
pub fn value(v: &Value) -> usize {
    size_of::<Value>().saturating_add(match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(value).fold(0usize, usize::saturating_add),
        Value::Object(o) => o.iter().map(|(k, v)| k.len().saturating_add(value(v))).fold(0usize, usize::saturating_add),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    })
}

fn hunk(h: &Hunk) -> usize {
    size_of::<Hunk>().saturating_add(texts(&h.lines))
}

fn hunks(hs: &[Hunk]) -> usize {
    hs.iter().map(hunk).fold(0usize, usize::saturating_add)
}

fn diff(d: &FileDiff) -> usize {
    size_of::<FileDiff>().saturating_add(d.path.len()).saturating_add(hunks(&d.hunks))
}

fn tool(t: &ToolCall) -> usize {
    let names = t.tool_id.len().saturating_add(t.name.len()).saturating_add(t.title.len());
    let maybes = [t.detail.as_ref(), t.output.as_ref(), t.task.as_ref(), t.server.as_ref(), t.tool_short.as_ref(), t.skill.as_ref(), t.runner.as_ref()].into_iter().map(maybe).fold(0usize, usize::saturating_add);
    let diffs = t.diffs.iter().map(diff).fold(0usize, usize::saturating_add);
    let todos = t.todos.iter().map(|d| d.text.len().saturating_add(d.status.len())).fold(0usize, usize::saturating_add);
    size_of::<ToolCall>().saturating_add(names).saturating_add(maybes).saturating_add(value(&t.input).saturating_sub(size_of::<Value>())).saturating_add(diffs).saturating_add(todos)
}

fn body(b: &Body) -> usize {
    match b {
        Body::User { text, client_id, origin, attachments, reply_to, .. } => text.len().saturating_add(maybe(client_id.as_ref())).saturating_add(maybe(origin.as_ref())).saturating_add(maybe(reply_to.as_ref())).saturating_add(texts(attachments)),
        Body::Assistant { text, .. } | Body::Thinking { text, .. } | Body::Notice { text, .. } => text.len(),
        Body::Tool(t) => tool(t),
    }
}

/// An event's bytes: its place and its texts.
pub fn event(e: &Event) -> usize {
    size_of::<Event>().saturating_add(e.id.len()).saturating_add(maybe(e.parent.as_ref())).saturating_add(maybe(e.by.as_ref())).saturating_add(body(&e.body))
}

fn change(f: &FileChange) -> usize {
    size_of::<FileChange>().saturating_add(f.path.len()).saturating_add(maybe(f.tree.as_ref())).saturating_add(hunks(&f.hunks)).saturating_add(texts(&f.by))
}

fn key(k: &FileKey) -> usize {
    size_of::<FileKey>().saturating_add(k.path.len()).saturating_add(maybe(k.tree.as_ref()))
}

impl Weigh for Update {
    fn weight(&self) -> usize {
        size_of::<Update>().saturating_add(match self {
            Update::Open { .. } => size_of::<super::session::Info>(),
            Update::State { .. } => size_of::<super::model::State>(),
            Update::Meta { .. } => size_of::<super::model::Meta>(),
            Update::Inbox { .. } => size_of::<super::inbox::Said>(),
            Update::MailFlow { event } => {
                use super::admission::FlowEvent;
                let capacity = match &**event {
                    FlowEvent::Accepted { capacity, .. } | FlowEvent::Consumed { capacity, .. }
                    | FlowEvent::Waiting { capacity, .. } | FlowEvent::Capacity { capacity }
                    | FlowEvent::Disposed { capacity, .. } | FlowEvent::Fault { capacity, .. } => capacity,
                };
                size_of::<super::admission::FlowEvent>().saturating_add(capacity.eligible_peer.as_ref().map_or(0, String::capacity))
            }
            Update::Events { events } => events.iter().map(event).fold(0usize, usize::saturating_add),
            Update::Changes { files } => files.iter().map(change).fold(0usize, usize::saturating_add),
            Update::ChangesPatch { set, gone } => set.iter().map(change).chain(gone.iter().map(key)).fold(0usize, usize::saturating_add),
            Update::Stream { id, text } => id.len().saturating_add(text.len()),
            Update::Reply { client, value: v } => client.len().saturating_add(value(v)),
            Update::Closed | Update::Reset { .. } => 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 3: an update weighs what its texts take, so a long reply weighs
    /// more than a short one by about the text between them, and a tool's input and output count.
    #[test]
    fn g11_s3_an_update_weighs_its_texts() -> anyhow::Result<()> {
        let said = |words: &str| -> anyhow::Result<Event> { Ok(serde_json::from_value(json!({"id": "e1", "seq": 1, "at": 0, "kind": "assistant", "text": words}))?) };
        let short = Update::Events { events: vec![said("hi")?] }.weight();
        let long = Update::Events { events: vec![said(&"word ".repeat(2000))?] }.weight();
        anyhow::ensure!(long - short == 10_000 - 2, "{long} - {short}");
        let tool: Event = serde_json::from_value(json!({"id": "t1", "seq": 2, "at": 0, "kind": "tool", "tool_id": "x", "name": "Bash", "tool": "shell", "title": "ls", "input": {"command": "ls -la"}, "status": "done", "output": "a".repeat(5000), "started": 0}))?;
        let w = Update::Events { events: vec![tool] }.weight();
        anyhow::ensure!(w > 5000 + "command".len() + "ls -la".len(), "a tool's output and input count: {w}");
        let reply = Update::Reply { client: "c".into(), value: json!({"files": ["a".repeat(300), "b".repeat(300)]}) }.weight();
        anyhow::ensure!(reply > 600, "a reply's value counts: {reply}");
        anyhow::ensure!(Update::Closed.weight() == size_of::<Update>());
        Ok(())
    }
}
