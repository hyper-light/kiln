//! W21-R02: bounded views of the canonical conversation, never a second event store. Whole
//! events keep their ids and text. Current interactive context may exceed the byte budget and
//! says so; completed intermediate tool history is available by cursor instead of sent eagerly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits { pub turns: usize, pub bytes: usize }

impl Limits {
    pub fn new(turns: usize, bytes: usize) -> Self { Self { turns: turns.clamp(1, 30), bytes: bytes.clamp(16 * 1024, 1024 * 1024) } }
}

#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub cursor: String,
    pub before: Option<u64>,
    pub has_more: bool,
    pub oversized: bool,
}

pub fn seq(e: &Value) -> Option<u64> { e.get("seq").and_then(Value::as_u64) }
fn id(e: &Value) -> Option<&str> { e.get("id").and_then(Value::as_str) }
fn main_user(e: &Value) -> bool { e.get("kind").and_then(Value::as_str) == Some("user") && e.get("parent").is_none_or(Value::is_null) }

pub fn cursor(born: u64, generation: u64, session: &str) -> String { format!("{born}.{generation}.{session}") }
pub fn valid_cursor(value: &str, born: u64, generation: u64, session: &str) -> bool { value == cursor(born, generation, session) }

/// A contiguous suffix, capped by turns and bytes, plus only the events needed to understand
/// current interactions. `before` follows the suffix, not sparse older dependency events.
pub fn recent(events: &[Arc<Value>], state: &Value, limits: Limits, token: String, durable: bool) -> (Vec<Arc<Value>>, Window) {
    let mut required = HashSet::<String>::new();
    for e in events.iter().rev().filter(|e| main_user(e)).take(1) { if let Some(id) = id(e) { required.insert(id.to_owned()); } }
    for e in events.iter().rev().filter(|e| e.get("kind").and_then(Value::as_str) == Some("assistant") && e.get("parent").is_none_or(Value::is_null)).take(1) {
        if let Some(id) = id(e) { required.insert(id.to_owned()); }
    }
    for e in events {
        if (e.get("streaming").and_then(Value::as_bool) == Some(true) || (e.get("kind").and_then(Value::as_str) == Some("tool") && matches!(e.get("status").and_then(Value::as_str), Some("running" | "pending"))))
            && let Some(id) = id(e)
        {
            required.insert(id.to_owned());
        }
    }
    // State itself is always sent whole. Only references needed by pending approvals/tasks bring
    // older timeline events along; completed tools in a long current turn are not all context.
    for key in ["approvals", "tasks"] {
        for item in state.get(key).and_then(Value::as_array).into_iter().flatten() {
            if key == "tasks" && !matches!(item.get("status").and_then(Value::as_str), Some("running" | "pending" | "paused")) { continue; }
            if let Some(id) = item.get("tool_id").and_then(Value::as_str) { required.insert(id.to_owned()); }
        }
    }
    // Resolve references by id or harness tool id once, rather than repeatedly scanning a long
    // single-turn history. These are transient indexes of the canonical mirror's references.
    let mut lookup = HashMap::new();
    for e in events {
        for key in ["id", "tool_id"] { if let Some(id) = e.get(key).and_then(Value::as_str) { lookup.insert(id, e); } }
    }
    let mut pending: Vec<String> = required.iter().cloned().collect();
    let mut visited = HashSet::new();
    while let Some(key) = pending.pop() {
        if !visited.insert(key.clone()) { continue; }
        let Some(e) = lookup.get(key.as_str()) else { continue };
        if let Some(id) = id(e) { required.insert(id.to_owned()); }
        for field in ["parent", "reply_to"] {
            if let Some(id) = e.get(field).and_then(Value::as_str) { required.insert(id.to_owned()); pending.push(id.to_owned()); }
        }
    }
    let mut start = events.len();
    let (mut turns, mut bytes) = (0usize, 0usize);
    for (i, e) in events.iter().enumerate().rev() {
        let n = e.to_string().len();
        if bytes.saturating_add(n) > limits.bytes { break; }
        bytes = bytes.saturating_add(n);
        start = i;
        if main_user(e) { turns += 1; if turns >= limits.turns { break; } }
    }
    if start == events.len() && !events.is_empty() { start = events.len().saturating_sub(1); }
    let before = events.get(start).and_then(|e| seq(e)).or_else(|| events.last().and_then(|e| seq(e)).map(|n| n.saturating_add(1)));
    let selected: Vec<_> = events.iter().enumerate().filter(|(i, e)| *i >= start || id(e).is_some_and(|i| required.contains(i))).map(|(_, e)| Arc::clone(e)).collect();
    let oversized = selected.iter().fold(0usize, |n, e| n.saturating_add(e.to_string().len())) > limits.bytes;
    let has_more = start > 0 || (durable && before.is_some_and(|n| n > 1));
    (selected, Window { cursor: token, before, has_more, oversized })
}

/// Transient selection metadata read off the shard from each canonical encoded event. Text and
/// tool bodies are skipped by serde, never materialized into a second event tree or retained cache.
#[derive(Default, serde::Deserialize)]
pub(super) struct Row {
    id: Option<String>, seq: Option<u64>, kind: Option<String>, parent: Option<String>,
    reply_to: Option<String>, tool_id: Option<String>, status: Option<String>, streaming: Option<bool>,
    #[serde(skip)] bytes: usize,
}

impl Row {
    pub(super) fn read(text: &str) -> Self {
        let mut row = serde_json::from_str::<Self>(text).unwrap_or_default();
        row.bytes = text.len();
        row
    }
    fn main(&self, kind: &str) -> bool { self.kind.as_deref() == Some(kind) && self.parent.is_none() }
}

/// The same policy as `recent`, over lightweight headers of immutable, chunked Copies. Returned
/// indices refer to that exact held view; snapshot writing still uses its original encoded entries.
pub(super) fn select(rows: &[Row], state: &Value, limits: Limits, token: String, durable: bool) -> (Vec<usize>, Window) {
    let floor = state.get("cleared").and_then(Value::as_u64).unwrap_or(1).max(1);
    let visible = |row: &Row| row.seq.is_some_and(|n| n >= floor);
    let mut required = HashSet::<String>::new();
    for kind in ["user", "assistant"] {
        if let Some(row) = rows.iter().rev().find(|r| visible(r) && r.main(kind)) && let Some(id) = &row.id { required.insert(id.clone()); }
    }
    for row in rows.iter().filter(|r| visible(r)) {
        if (row.streaming == Some(true) || (row.kind.as_deref() == Some("tool") && matches!(row.status.as_deref(), Some("running" | "pending")))) && let Some(id) = &row.id { required.insert(id.clone()); }
    }
    for key in ["approvals", "tasks"] {
        for item in state.get(key).and_then(Value::as_array).into_iter().flatten() {
            if key == "tasks" && !matches!(item.get("status").and_then(Value::as_str), Some("running" | "pending" | "paused")) { continue; }
            if let Some(id) = item.get("tool_id").and_then(Value::as_str) { required.insert(id.to_owned()); }
        }
    }
    let mut lookup = HashMap::new();
    for (i, row) in rows.iter().enumerate() { for key in [&row.id, &row.tool_id].into_iter().flatten() { lookup.insert(key.as_str(), i); } }
    let mut pending: Vec<String> = required.iter().cloned().collect();
    let mut visited = HashSet::new();
    while let Some(key) = pending.pop() {
        if !visited.insert(key.clone()) { continue; }
        let Some(row) = lookup.get(key.as_str()).and_then(|i| rows.get(*i)) else { continue };
        if let Some(id) = &row.id { required.insert(id.clone()); }
        for id in [&row.parent, &row.reply_to].into_iter().flatten() { required.insert(id.clone()); pending.push(id.clone()); }
    }
    let (mut start, mut turns, mut bytes) = (rows.len(), 0usize, 0usize);
    for (i, row) in rows.iter().enumerate().rev() {
        if !visible(row) { break; }
        if bytes.saturating_add(row.bytes) > limits.bytes { break; }
        bytes = bytes.saturating_add(row.bytes); start = i;
        if row.main("user") { turns += 1; if turns >= limits.turns { break; } }
    }
    if start == rows.len() && rows.last().is_some_and(visible) { start = rows.len().saturating_sub(1); }
    let before = rows.get(start).and_then(|r| r.seq).or_else(|| rows.last().and_then(|r| r.seq).map(|n| n.saturating_add(1)));
    let selected: Vec<_> = rows.iter().enumerate().filter(|(i, row)| visible(row) && (*i >= start || row.id.as_ref().is_some_and(|id| required.contains(id)))).map(|(i, _)| i).collect();
    let oversized = selected.iter().filter_map(|i| rows.get(*i)).fold(0usize, |n, r| n.saturating_add(r.bytes)) > limits.bytes;
    let has_more = rows.iter().take(start).any(visible) || (durable && before.is_some_and(|n| n > floor));
    (selected, Window { cursor: token, before, has_more, oversized })
}

/// Newest whole events of a page, oldest first. At least one event makes progress even when
/// indivisible text alone exceeds the budget. The explicit flag distinguishes this exception.
pub fn page(mut events: Vec<Value>, before: u64, limit: usize, bytes: usize, token: String, more: bool) -> (Vec<Value>, Window) {
    events.retain(|e| seq(e).is_some_and(|n| n < before));
    events.sort_by_key(seq);
    let (mut size, mut start, mut count) = (0usize, events.len(), 0usize);
    for (i, e) in events.iter().enumerate().rev() {
        let n = e.to_string().len();
        if count > 0 && (count >= limit || size.saturating_add(n) > bytes) { break; }
        size = size.saturating_add(n); start = i; count += 1;
    }
    let has_more = start > 0 || more;
    let events: Vec<_> = events.drain(start..).collect();
    let next = events.first().and_then(seq);
    (events, Window { cursor: token, before: next, has_more, oversized: size > bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn w21_r02_recent_turns_and_bytes_keep_current_interactions_not_all_current_tools() {
        let mut events: Vec<_> = (1..=100).map(|n| Arc::new(json!({"id":format!("e{n}"),"seq":n,"kind":if n % 10 == 1 {"user"} else {"tool"},"status":"completed","text":"x".repeat(1000)}))).collect();
        events.push(Arc::new(json!({"id":"answer","seq":101,"kind":"assistant","text":"latest"})));
        let (selected, window) = recent(&events, &json!({"approvals":[{"tool_id":"e2"}]}), Limits::new(1,16 * 1024), "c".into(), true);
        assert!(selected.len() < events.len());
        assert!(selected.iter().any(|e| id(e) == Some("e2")));
        assert!(selected.iter().any(|e| id(e) == Some("e91")));
        assert!(!selected.iter().any(|e| id(e) == Some("e80")));
        assert_eq!(window.before, Some(91)); assert!(window.has_more);
    }

    #[test]
    fn w21_r02_encoded_header_projection_matches_the_full_tree_oracle() {
        for count in [0, 1, 20, 240] {
            let mut events: Vec<_> = (1..=count).map(|n| Arc::new(json!({"id":format!("e{n}"),"seq":n,"kind":if n % 9 == 1 {"user"} else {"tool"},"status":if n == 3 {"running"} else {"done"},"tool_id":format!("t{n}"),"text":"x".repeat(n % 17 * 100)}))).collect();
            events.push(Arc::new(json!({"id":"latest","seq":count+1,"kind":"assistant","text":"answer","reply_to":"e2"})));
            let state = json!({"approvals":[{"tool_id":"t2"}],"tasks":[{"status":"running","tool_id":"t3"}]});
            let rows: Vec<_> = events.iter().map(|e| Row::read(&e.to_string())).collect();
            for turns in [1, 8] {
                let limits = Limits::new(turns, 16 * 1024);
                let (oracle, old) = recent(&events, &state, limits, "c".into(), true);
                let (indices, new) = select(&rows, &state, limits, "c".into(), true);
                assert_eq!(indices.iter().filter_map(|i| events.get(*i)).map(|e| id(e)).collect::<Vec<_>>(), oracle.iter().map(|e| id(e)).collect::<Vec<_>>());
                assert_eq!(serde_json::to_value(old).ok(), serde_json::to_value(new).ok());
            }
        }
    }

    #[test]
    fn w21_r02_cursor_projection_does_not_reveal_a_cleared_timeline() {
        let rows: Vec<_> = (1..=20).map(|n| Row::read(&json!({"id":format!("e{n}"),"seq":n,"kind":"user","text":"whole"}).to_string())).collect();
        let (selected, h) = select(&rows, &json!({"cleared":18}), Limits::new(8, 16384), "cursor".into(), true);
        assert_eq!(selected, vec![17, 18, 19]);
        assert_eq!(h.before, Some(18)); assert!(!h.has_more);
    }

    #[test]
    fn w21_r02_whole_oversized_current_text_is_explicit_and_pages_progress() {
        let events = vec![Arc::new(json!({"id":"old","seq":1,"kind":"user","text":"old"})), Arc::new(json!({"id":"answer","seq":2,"kind":"assistant","text":"x".repeat(32768)}))];
        let (selected, w) = recent(&events, &Value::Null, Limits::new(1,16384), "c".into(), true);
        assert_eq!(selected.len(),2); assert!(w.oversized); assert_eq!(w.before,Some(2));
        let (p,w) = page(selected.iter().map(|e| Value::clone(e)).collect(),3,100,16384,"c".into(),false);
        assert_eq!(p.len(),1); assert!(w.oversized); assert_eq!(w.before,Some(2)); assert!(w.has_more);
        assert!(!valid_cursor("1.2.a",1,2,"b")); assert!(!valid_cursor("1.2.a",1,3,"a")); assert!(!valid_cursor("1.2.a",2,2,"a"));
    }
}
