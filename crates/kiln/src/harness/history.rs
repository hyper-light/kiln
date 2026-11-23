//! W21-R02: bounded views of the canonical conversation, never a second event store. Whole
//! events keep their ids and text. Current interactive context may exceed the byte budget and
//! says so; completed intermediate tool history is available by cursor instead of sent eagerly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub turns: usize,
    pub bytes: usize,
    /// The bytes of a first part its viewer takes ahead of the rest ([`Limits::first_of`]). None: the window at once.
    pub first: Option<usize>,
}

impl Limits {
    pub fn new(turns: usize, bytes: usize) -> Self { Self { turns: turns.clamp(1, 30), bytes: bytes.clamp(FLOOR, 1024 * 1024), first: None } }

    /// These, a first part of `bytes` taken ahead (`first_bytes`): a fresh snapshot is then that part of the window
    /// alone, and the rest of the window follows it on its viewer's socket ([`Limits::part`],
    /// `super::registry::Rest`). No part is smaller than the smallest window, nor larger than its own.
    pub fn first_of(self, bytes: usize) -> Self { Self { first: Some(bytes.clamp(FLOOR, self.bytes)), ..self } }

    /// What a fresh snapshot holds under these: the window's first part where its viewer takes one, else the window.
    pub fn part(self) -> Self { Self { bytes: self.first.unwrap_or(self.bytes), first: None, ..self } }

    /// These as a window at once: no part taken ahead.
    pub fn whole(self) -> Self { Self { first: None, ..self } }
}

/// The fewest bytes a window is bounded to.
const FLOOR: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub cursor: String,
    pub before: Option<u64>,
    pub has_more: bool,
    pub oversized: bool,
    /// Its bytes' bound is what ended it, not its turns' nor its session's start: under a larger one it may go back
    /// further (a window's first part then has a rest, [`Limits::first_of`]). Not said on the wire.
    #[serde(skip)]
    pub by_bytes: bool,
}

pub fn seq(e: &Value) -> Option<u64> { e.get("seq").and_then(Value::as_u64) }
fn id(e: &Value) -> Option<&str> { e.get("id").and_then(Value::as_str) }
fn main_user(e: &Value) -> bool { e.get("kind").and_then(Value::as_str) == Some("user") && e.get("parent").is_none_or(Value::is_null) }

pub fn cursor(born: u64, generation: u64, session: &str) -> String { format!("{born}.{generation}.{session}") }
pub fn valid_cursor(value: &str, born: u64, generation: u64, session: &str) -> bool { value == cursor(born, generation, session) }

/// Whether a copy holding the events numbered `seqs` (a run of them, in order) holds all an older page before
/// `before` shows ([`page`]: `limit` events, one more saying there are more): it has that many from `floor` on,
/// or it starts at `floor` (or before), so nothing older is anywhere else. Then the conversation store has
/// nothing for the page the copy doesn't (plan 01a1cc09, step 1).
pub fn covers(seqs: impl IntoIterator<Item = u64>, floor: u64, before: u64, limit: usize) -> bool {
    let (mut first, mut below) = (None::<u64>, 0usize);
    for n in seqs {
        first = Some(first.map_or(n, |f| f.min(n)));
        if n >= floor && n < before { below = below.saturating_add(1); }
    }
    first.is_some_and(|f| f <= floor) || below > limit
}

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
    let (mut turns, mut bytes, mut by_bytes) = (0usize, 0usize, false);
    for (i, e) in events.iter().enumerate().rev() {
        let n = e.to_string().len();
        if bytes.saturating_add(n) > limits.bytes { by_bytes = true; break; }
        bytes = bytes.saturating_add(n);
        start = i;
        if main_user(e) { turns += 1; if turns >= limits.turns { break; } }
    }
    if start == events.len() && !events.is_empty() { start = events.len().saturating_sub(1); }
    let before = events.get(start).and_then(|e| seq(e)).or_else(|| events.last().and_then(|e| seq(e)).map(|n| n.saturating_add(1)));
    let selected: Vec<_> = events.iter().enumerate().filter(|(i, e)| *i >= start || id(e).is_some_and(|i| required.contains(i))).map(|(_, e)| Arc::clone(e)).collect();
    let oversized = selected.iter().fold(0usize, |n, e| n.saturating_add(e.to_string().len())) > limits.bytes;
    let has_more = start > 0 || (durable && before.is_some_and(|n| n > 1));
    (selected, Window { cursor: token, before, has_more, oversized, by_bytes })
}

/// Transient selection metadata read off the shard from each canonical encoded event. Text and
/// tool bodies are skipped by serde, never materialized into a second event tree or retained cache.
#[derive(Default, serde::Deserialize)]
pub(crate) struct Row {
    id: Option<String>, seq: Option<u64>, kind: Option<String>, parent: Option<String>,
    reply_to: Option<String>, tool_id: Option<String>, status: Option<String>, streaming: Option<bool>,
    #[serde(skip)] bytes: usize,
}

impl Row {
    pub(crate) fn read(text: &str) -> Self {
        let mut row = serde_json::from_str::<Self>(text).unwrap_or_default();
        row.bytes = text.len();
        row
    }
    fn main(&self, kind: &str) -> bool { self.kind.as_deref() == Some(kind) && self.parent.is_none() }
}

/// The same policy as `recent`, over lightweight headers of immutable, chunked Copies. Returned
/// indices refer to that exact held view; snapshot writing still uses its original encoded entries.
pub(crate) fn select(rows: &[Row], state: &Value, limits: Limits, token: String, durable: bool) -> (Vec<usize>, Window) {
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
    let (mut start, mut turns, mut bytes, mut by_bytes) = (rows.len(), 0usize, 0usize, false);
    for (i, row) in rows.iter().enumerate().rev() {
        if !visible(row) { break; }
        if bytes.saturating_add(row.bytes) > limits.bytes { by_bytes = true; break; }
        bytes = bytes.saturating_add(row.bytes); start = i;
        if row.main("user") { turns += 1; if turns >= limits.turns { break; } }
    }
    if start == rows.len() && rows.last().is_some_and(visible) { start = rows.len().saturating_sub(1); }
    let before = rows.get(start).and_then(|r| r.seq).or_else(|| rows.last().and_then(|r| r.seq).map(|n| n.saturating_add(1)));
    let selected: Vec<_> = rows.iter().enumerate().filter(|(i, row)| visible(row) && (*i >= start || row.id.as_ref().is_some_and(|id| required.contains(id)))).map(|(i, _)| i).collect();
    let oversized = selected.iter().filter_map(|i| rows.get(*i)).fold(0usize, |n, r| n.saturating_add(r.bytes)) > limits.bytes;
    let has_more = rows.iter().take(start).any(visible) || (durable && before.is_some_and(|n| n > floor));
    (selected, Window { cursor: token, before, has_more, oversized, by_bytes })
}

/// What selecting a window needs of an event without its JSON: a copy's metas say these of every event it keeps,
/// compact, spilled or neither (`super::copies::Meta`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Mark {
    pub seq: Option<u64>,
    /// A person's message to the session's main thread (it begins a turn); a reply on it.
    pub user: bool,
    pub reply: bool,
    /// Still coming or under way: text that streams, a tool call running or pending.
    pub live: bool,
    /// A tool call: the one kind sent otherwise than as kept, to a viewer that takes events lean.
    pub tool: bool,
    /// A tool call its harness names otherwise than its event: found by that name only by reading every event.
    pub tool_other: bool,
    /// Its JSON's bytes as kept: what it's sent as, unless it's a tool call sent lean.
    pub len: usize,
}

/// A copy's events as a window is selected from them ([`select_marked`]): every one's [`Mark`] at no read, and an
/// event's JSON read only where the selection asks.
pub(super) trait Marked {
    /// Each event's mark, in order.
    fn marks(&self) -> Vec<Mark>;
    /// Where the event with id `id` is.
    fn find(&mut self, id: &str) -> Option<usize>;
    /// Event `i` read: what [`Row::read`] takes of its JSON as its viewer is sent it. Kept by its reader for what sends it.
    fn read(&mut self, i: usize) -> Row;
    /// Event `i` is one the window brings, said as the window is found, newest first: read now where reading it later
    /// costs more (a compact chunk's, while its chunk is inflated for the events beside it). Nothing where it costs the same.
    fn keep(&mut self, _i: usize) {}
}

/// [`select`], reading only the events it sends: the same places and the same window for the same copy, state and
/// limits (the tests hold it to `select` over generated sessions). `select` takes every event's header, so finding a
/// window read every event its copy keeps (up to 5,000: each compact chunk inflated, each spilled one read from its
/// file, each tool call's JSON parsed whole for its lean form) to send the last eight turns: an open of a 41,000-event
/// session took 38 to 67 ms at the median on a quiet machine and 0.2 to 0.7 s on a busy one (`kiln-lab bar`,
/// 2026-11-13 and 14). Here what's read is the window: the events
/// from its end back to its bounds (a tool call's lean form, where its viewer takes events lean; another event's
/// bytes are its meta's), then the few its current interactions name. Which events are a person's message, a reply,
/// or still under way, their metas say.
///
/// `lean`: whether its viewer takes events lean. None where an event is a tool call named otherwise than by its id
/// ([`Mark::tool_other`]): the approvals and tasks that name it are found only by reading every event, `select`'s to do.
pub(super) fn select_marked(events: &mut impl Marked, state: &Value, limits: Limits, token: String, durable: bool, lean: bool) -> Option<(Vec<usize>, Window)> {
    let marks = events.marks();
    if marks.iter().any(|m| m.tool_other) {
        return None;
    }
    let floor = state.get("cleared").and_then(Value::as_u64).unwrap_or(1).max(1);
    let visible = |m: &Mark| m.seq.is_some_and(|n| n >= floor);
    // An event's bytes as sent, each read at most once: a tool call's lean form's where its viewer takes events lean, else
    // its JSON's as kept, which its meta says.
    fn sent_len(sizes: &mut HashMap<usize, usize>, events: &mut impl Marked, lean: bool, i: usize, m: &Mark) -> usize {
        if !(lean && m.tool) {
            return m.len;
        }
        *sizes.entry(i).or_insert_with(|| events.read(i).bytes)
    }
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    // The suffix: from the end back to the bounds.
    let (mut start, mut turns, mut bytes, mut by_bytes) = (marks.len(), 0usize, 0usize, false);
    for (i, m) in marks.iter().enumerate().rev() {
        if !visible(m) {
            break;
        }
        let n = sent_len(&mut sizes, events, lean, i, m);
        if bytes.saturating_add(n) > limits.bytes {
            by_bytes = true;
            break;
        }
        bytes = bytes.saturating_add(n);
        start = i;
        events.keep(i);
        if m.user {
            turns += 1;
            if turns >= limits.turns {
                break;
            }
        }
    }
    if start == marks.len() && marks.last().is_some_and(visible) {
        start = marks.len().saturating_sub(1);
        events.keep(start);
    }
    // What current interactions need besides: the last message and reply, whatever is under way, what approvals and
    // live tasks name, and what each of those names in turn (its parent, what it replies to).
    enum Want {
        At(usize),
        Named(String),
    }
    let mut wanted: Vec<Want> = Vec::new();
    for last in [marks.iter().rposition(|m| visible(m) && m.user), marks.iter().rposition(|m| visible(m) && m.reply)].into_iter().flatten() {
        wanted.push(Want::At(last));
    }
    wanted.extend(marks.iter().enumerate().filter(|(_, m)| visible(m) && m.live).map(|(i, _)| Want::At(i)));
    for key in ["approvals", "tasks"] {
        for item in state.get(key).and_then(Value::as_array).into_iter().flatten() {
            if key == "tasks" && !matches!(item.get("status").and_then(Value::as_str), Some("running" | "pending" | "paused")) {
                continue;
            }
            if let Some(id) = item.get("tool_id").and_then(Value::as_str) {
                wanted.push(Want::Named(id.to_owned()));
            }
        }
    }
    let mut required: HashSet<usize> = HashSet::new();
    let mut named: HashSet<String> = HashSet::new();
    while let Some(want) = wanted.pop() {
        let i = match want {
            Want::At(i) => i,
            Want::Named(id) => {
                if !named.insert(id.clone()) {
                    continue;
                }
                match events.find(&id) {
                    Some(i) => i,
                    None => continue,
                }
            }
        };
        if !required.insert(i) {
            continue;
        }
        // What it names in turn: its JSON's header says (an event the window holds already is read for it too, as its
        // frame's writing reads it).
        let row = events.read(i);
        if lean && marks.get(i).is_some_and(|m| m.tool) {
            sizes.entry(i).or_insert(row.bytes);
        }
        wanted.extend([row.parent, row.reply_to].into_iter().flatten().map(Want::Named));
    }
    let before = marks.get(start).and_then(|m| m.seq).or_else(|| marks.last().and_then(|m| m.seq).map(|n| n.saturating_add(1)));
    let selected: Vec<usize> = marks.iter().enumerate().filter(|(i, m)| visible(m) && (*i >= start || required.contains(i))).map(|(i, _)| i).collect();
    let mut sent = 0usize;
    for i in &selected {
        if let Some(m) = marks.get(*i) {
            sent = sent.saturating_add(sent_len(&mut sizes, events, lean, *i, m));
        }
    }
    let has_more = marks.iter().take(start).any(visible) || (durable && before.is_some_and(|n| n > floor));
    Some((selected, Window { cursor: token, before, has_more, oversized: sent > limits.bytes, by_bytes }))
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
    (events, Window { cursor: token, before: next, has_more, oversized: size > bytes, by_bytes: false })
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

    /// Plan 01a1cc09, step 1: a copy covers a page when it holds the page and one more past it, or holds from
    /// the floor; one that starts later with fewer leaves the rest to the store.
    #[test]
    fn a_copy_covers_a_page_it_holds_whole() {
        // From the first event: every page.
        assert!(covers(1..=230, 1, 221, 100));
        assert!(covers(1..=230, 1, 50, 100));
        // From the cleared floor on: nothing older shows.
        assert!(covers(18..=230, 18, 60, 100));
        // A later start with the page and one more: the page.
        assert!(covers(101..=230, 1, 221, 100));
        // A later start with only the page, not one more: whether there are more is the store's to say.
        assert!(!covers(121..=230, 1, 221, 100));
        // A later start with fewer: the store has the rest.
        assert!(!covers(150..=230, 1, 221, 100));
        // No copy events at all.
        assert!(!covers(std::iter::empty(), 1, 221, 100));
    }

    /// A copy's events as a test has them: each one's JSON as kept and as sent (a tool call's lean form shorter), and how
    /// many were read.
    struct Fake {
        kept: Vec<Value>,
        sent: Vec<String>,
        reads: std::cell::Cell<usize>,
    }

    impl Fake {
        fn of(kept: Vec<Value>, lean: bool) -> Self {
            let sent = kept
                .iter()
                .map(|e| {
                    let mut e = e.clone();
                    if lean && e.get("kind").and_then(Value::as_str) == Some("tool") && let Some(o) = e.as_object_mut() {
                        o.remove("output");
                    }
                    e.to_string()
                })
                .collect();
            Self { kept, sent, reads: std::cell::Cell::new(0) }
        }

        /// Every event's header as `select` takes them: each read as sent.
        fn rows(&self) -> Vec<Row> {
            self.sent.iter().map(|t| Row::read(t)).collect()
        }
    }

    impl Marked for Fake {
        fn marks(&self) -> Vec<Mark> {
            self.kept
                .iter()
                .map(|e| {
                    let s = |k: &str| e.get(k).and_then(Value::as_str);
                    let main = e.get("parent").is_none_or(Value::is_null);
                    Mark {
                        seq: seq(e),
                        user: main && s("kind") == Some("user"),
                        reply: main && s("kind") == Some("assistant"),
                        live: e.get("streaming").and_then(Value::as_bool) == Some(true) || (s("kind") == Some("tool") && matches!(s("status"), Some("running" | "pending"))),
                        tool: s("kind") == Some("tool"),
                        tool_other: s("tool_id").is_some_and(|t| s("id") != Some(t)),
                        len: e.to_string().len(),
                    }
                })
                .collect()
        }

        fn find(&mut self, id: &str) -> Option<usize> {
            self.kept.iter().rposition(|e| e.get("id").and_then(Value::as_str) == Some(id))
        }

        fn read(&mut self, i: usize) -> Row {
            self.reads.set(self.reads.get() + 1);
            self.sent.get(i).map_or_else(Row::default, |t| Row::read(t))
        }
    }

    /// A deterministic stream of numbers.
    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n.max(1)
        }
    }

    /// A session as an agent's is: turns begun by a person's message, replies, thoughts, tool calls (some still under way,
    /// some a subagent's, their parent an earlier call), notices, now and then a reply to an earlier event.
    fn session(r: &mut Lcg, n: u64, other_names: bool) -> (Vec<Value>, Value) {
        let mut events: Vec<Value> = Vec::new();
        let mut tools: Vec<String> = Vec::new();
        for q in 1..=n {
            let id = format!("e{q}");
            let text = "x".repeat(usize::try_from(r.below(900)).unwrap_or(0));
            let parent = if !tools.is_empty() && r.below(5) == 0 { tools.get(usize::try_from(r.below(tools.len() as u64)).unwrap_or(0)).cloned() } else { None };
            let mut e = match r.below(20) {
                0 | 1 => json!({"id": id, "seq": q, "kind": "user", "text": text}),
                2..=6 => json!({"id": id, "seq": q, "kind": "assistant", "text": text, "streaming": q + 3 > n && r.below(2) == 0}),
                7 | 8 => json!({"id": id, "seq": q, "kind": "thinking", "text": text, "streaming": false}),
                9 => json!({"id": id, "seq": q, "kind": "notice", "level": "info", "text": text}),
                _ => {
                    let status = match r.below(40) {
                        0 => "running",
                        1 => "pending",
                        _ => "done",
                    };
                    let tool_id = if other_names && r.below(9) == 0 { format!("toolu_{q}") } else { id.clone() };
                    tools.push(tool_id.clone());
                    json!({"id": id, "seq": q, "kind": "tool", "tool_id": tool_id, "status": status, "title": "cargo test", "output": "y".repeat(usize::try_from(r.below(6_000)).unwrap_or(0))})
                }
            };
            if let (Some(p), Some(o)) = (parent, e.as_object_mut()) {
                o.insert("parent".into(), Value::String(p));
            }
            if q > 4 && r.below(25) == 0 && let Some(o) = e.as_object_mut() {
                o.insert("reply_to".into(), Value::String(format!("e{}", 1 + r.below(q - 1))));
            }
            events.push(e);
        }
        let pick = |r: &mut Lcg, tools: &[String]| tools.get(usize::try_from(r.below(tools.len() as u64)).unwrap_or(0)).cloned().unwrap_or_else(|| "none".into());
        let mut state = serde_json::Map::new();
        if r.below(3) == 0 {
            state.insert("cleared".into(), json!(1 + r.below(n)));
        }
        let approvals: Vec<Value> = (0..r.below(3)).map(|_| json!({"tool_id": pick(r, &tools)})).collect();
        let tasks: Vec<Value> = (0..r.below(6))
            .map(|_| {
                let status = ["running", "pending", "paused", "done", "failed"].get(usize::try_from(r.below(5)).unwrap_or(0)).copied().unwrap_or("done");
                let tool_id = if r.below(6) == 0 { "gone".to_owned() } else { pick(r, &tools) };
                json!({"status": status, "tool_id": tool_id})
            })
            .collect();
        state.insert("approvals".into(), Value::Array(approvals));
        state.insert("tasks".into(), Value::Array(tasks));
        (events, Value::Object(state))
    }

    /// W21-R02: a window selected reading only what it sends is the window selected from every event's header, for
    /// generated sessions, states and limits; and it reads a part of a long session, not all of it.
    #[test]
    fn w21_r02_a_window_selected_from_the_metas_is_the_one_selected_from_every_header() {
        let mut r = Lcg(0x5eed);
        for case in 0..400u64 {
            let of = |xs: &[usize], at: u64| xs.get(usize::try_from(at).unwrap_or(0)).copied().unwrap_or(1);
            let n = of(&[0, 1, 2, 7, 40, 160, 400], case % 7) as u64;
            let (events, state) = session(&mut r, n, false);
            let limits = Limits::new(of(&[1, 2, 8, 30], r.below(4)), of(&[16 * 1024, 64 * 1024, 1024 * 1024], r.below(3)));
            let (durable, lean) = (r.below(2) == 0, r.below(4) != 0);
            let mut fake = Fake::of(events, lean);
            let (want, old) = select(&fake.rows(), &state, limits, "c".into(), durable);
            let (got, new) = select_marked(&mut fake, &state, limits, "c".into(), durable, lean).unwrap_or_default_window();
            assert_eq!(got, want, "case {case}: {n} events, {limits:?}, durable {durable}, lean {lean}, state {state}");
            assert_eq!(serde_json::to_value(new).ok(), serde_json::to_value(old).ok(), "case {case}");
        }
        // A long session, one turn at the end: what's read is about the window, not the session.
        let mut events: Vec<Value> = (1..=5_000u64).map(|q| json!({"id": format!("e{q}"), "seq": q, "kind": if q % 50 == 1 { "user" } else { "tool" }, "tool_id": format!("e{q}"), "status": "done", "output": "y".repeat(2_000)})).collect();
        events.push(json!({"id": "answer", "seq": 5_001, "kind": "assistant", "text": "done"}));
        let limits = Limits::new(8, 262_144);
        let mut fake = Fake::of(events, true);
        let (want, _) = select(&fake.rows(), &json!({}), limits, "c".into(), true);
        let (got, _) = select_marked(&mut fake, &json!({}), limits, "c".into(), true, true).unwrap_or_default_window();
        assert_eq!(got, want);
        assert!(fake.reads.get() <= want.len() + 2 && want.len() < 500, "{} events read to send {} of 5,001", fake.reads.get(), want.len());
    }

    /// A window's first part is of its window: the newest of it, nothing the window doesn't hold, so the part and what
    /// the window holds besides are the window. A part its bytes didn't end is the window whole, and says so (no rest is
    /// sent after it). The part selected from the metas is the one selected from every header.
    #[test]
    fn w21_r02_a_windows_first_part_is_of_its_window() {
        let mut r = Lcg(0xf125);
        let (mut parted, mut at_once) = (0, 0);
        for case in 0..400u64 {
            let of = |xs: &[usize], at: u64| xs.get(usize::try_from(at).unwrap_or(0)).copied().unwrap_or(1);
            let n = of(&[1, 2, 7, 40, 160, 400, 900], case % 7) as u64;
            let (events, state) = session(&mut r, n, false);
            let limits = Limits::new(of(&[1, 2, 8, 30], r.below(4)), of(&[64 * 1024, 262_144, 1024 * 1024], r.below(3))).first_of(16 * 1024);
            assert_eq!(limits.part(), Limits::new(limits.turns, 16 * 1024), "a first part is a window of its bytes");
            assert_eq!(limits.whole(), Limits::new(limits.turns, limits.bytes));
            let (durable, lean) = (r.below(2) == 0, r.below(4) != 0);
            let mut fake = Fake::of(events, lean);
            let rows = fake.rows();
            let (whole, w) = select(&rows, &state, limits.whole(), "c".into(), durable);
            let (first, f) = select(&rows, &state, limits.part(), "c".into(), durable);
            let (marked, m) = select_marked(&mut fake, &state, limits.part(), "c".into(), durable, lean).unwrap_or_default_window();
            assert_eq!((marked, m.by_bytes, m.before), (first.clone(), f.by_bytes, f.before), "case {case}: the part selected from the metas");
            assert!(first.iter().all(|i| whole.contains(i)), "case {case}: a part holds nothing its window doesn't");
            if f.by_bytes {
                assert!(f.has_more && w.before <= f.before, "case {case}: the rest is before the part");
                parted += 1;
            } else {
                assert_eq!(first, whole, "case {case}: a part its bytes didn't end is the window");
                assert_eq!((f.before, f.has_more), (w.before, w.has_more), "case {case}");
                at_once += 1;
            }
        }
        assert!(parted > 40 && at_once > 40, "{parted} windows came in two parts, {at_once} at once");
    }

    /// A tool call named otherwise than by its id is found by that name only by reading every event: left to `select`.
    #[test]
    fn w21_r02_a_copy_with_a_tool_named_otherwise_is_selected_from_every_header() {
        let mut r = Lcg(7);
        let mut left = 0;
        for _ in 0..40 {
            let (events, state) = session(&mut r, 120, true);
            let other = events.iter().any(|e| e.get("tool_id").is_some_and(|t| e.get("id") != Some(t)));
            let mut fake = Fake::of(events, true);
            let got = select_marked(&mut fake, &state, Limits::new(8, 65_536), "c".into(), true, true);
            assert_eq!(got.is_none(), other);
            left += usize::from(other);
        }
        assert!(left > 0, "no generated session had one");
    }

    trait OrDefault {
        fn unwrap_or_default_window(self) -> (Vec<usize>, Window);
    }
    impl OrDefault for Option<(Vec<usize>, Window)> {
        fn unwrap_or_default_window(self) -> (Vec<usize>, Window) {
            self.unwrap_or_else(|| (vec![usize::MAX], Window { cursor: "none".into(), before: None, has_more: false, oversized: false, by_bytes: false }))
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
