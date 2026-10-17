//! A session's normalized state and timeline, plus what came out of the last step: updates for
//! the UI, lines for the harness's stdin, previews to compute. Adapters mutate it; the brain that
//! owns the process drains it. No I/O here, so adapters are tested by feeding lines.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::model::{Approval, Body, Event, Meta, State, Status};

/// Timeline events kept per session (the oldest go first).
pub const MAX_EVENTS: usize = 4_000;

/// A change for the UI (and for the server's mirror of a machine's session).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Update {
    /// A session appeared, or is announced again after a reconnect (a snapshot follows).
    Open { info: Box<super::session::Info> },
    /// The session is gone.
    Closed,
    State { state: Box<State> },
    Meta { meta: Box<Meta> },
    /// Upserts by id.
    Events { events: Vec<Event> },
    /// Text to append to a streaming event.
    Stream { id: String, text: String },
    /// The answer to one client's request (file suggestions ...).
    Reply { client: String, value: serde_json::Value },
    /// The timeline starts over (a snapshot follows).
    Reset,
}

/// Work the brain does off the event loop and feeds back: e.g. reading a file to show what an
/// edit would change.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewJob {
    pub approval: String,
    pub cwd: String,
    pub tool: String,
    pub input: serde_json::Value,
}

pub struct Core {
    pub state: State,
    pub meta: Meta,
    events: VecDeque<Event>,
    /// id → seq of every event still kept.
    index: HashMap<String, u64>,
    next_seq: u64,
    /// Updates not yet sent, in order.
    pub out: Vec<Update>,
    /// Lines for the harness's stdin.
    pub writes: Vec<String>,
    pub previews: Vec<PreviewJob>,
    state_dirty: bool,
    meta_dirty: bool,
    /// Streaming text not yet flushed, by event id (coalesced).
    stream: Vec<(String, String)>,
    /// Events changed since the last flush, by id (coalesced).
    changed: Vec<String>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    pub fn new() -> Self {
        Self {
            state: State::default(),
            meta: Meta::default(),
            events: VecDeque::new(),
            index: HashMap::new(),
            next_seq: 1,
            out: Vec::new(),
            writes: Vec::new(),
            previews: Vec::new(),
            state_dirty: true,
            meta_dirty: true,
            stream: Vec::new(),
            changed: Vec::new(),
        }
    }

    pub fn now() -> i64 {
        crate::util::now_ms()
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.events.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Event> {
        let seq = *self.index.get(id)?;
        let front = self.events.front()?.seq;
        self.events.get(usize::try_from(seq.checked_sub(front)?).ok()?)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Event> {
        let seq = *self.index.get(id)?;
        let front = self.events.front()?.seq;
        let pos = usize::try_from(seq.checked_sub(front)?).ok()?;
        if !self.changed.iter().any(|c| c == id) {
            self.changed.push(id.to_owned());
        }
        self.events.get_mut(pos)
    }

    /// Add an event, or replace the one with the same id (keeping its place and time).
    pub fn upsert(&mut self, id: &str, parent: Option<String>, body: Body) {
        if let Some(e) = self.get_mut(id) {
            e.body = body;
            if parent.is_some() {
                e.parent = parent;
            }
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push_back(Event { id: id.to_owned(), seq, at: Self::now(), parent, body });
        self.index.insert(id.to_owned(), seq);
        self.changed.push(id.to_owned());
        while self.events.len() > MAX_EVENTS {
            if let Some(old) = self.events.pop_front() {
                self.index.remove(&old.id);
                self.state.dropped += 1;
                self.state_dirty = true;
            }
        }
    }

    /// Text arriving for a streaming event (created by the caller beforehand).
    pub fn append(&mut self, id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(seq) = self.index.get(id).copied() else { return };
        let Some(front) = self.events.front().map(|e| e.seq) else { return };
        let Some(e) = usize::try_from(seq.saturating_sub(front)).ok().and_then(|p| self.events.get_mut(p)) else { return };
        match &mut e.body {
            Body::Assistant { text: t, .. } | Body::Thinking { text: t, .. } => {
                if t.len() < super::model::TEXT_MAX {
                    t.push_str(text);
                }
            }
            _ => return,
        }
        // An event created in this same step goes out whole; otherwise only the delta.
        if self.changed.iter().any(|c| c == id) {
            return;
        }
        match self.stream.iter_mut().find(|(i, _)| i == id) {
            Some((_, buf)) => buf.push_str(text),
            None => self.stream.push((id.to_owned(), text.to_owned())),
        }
    }

    pub fn touch_state(&mut self) {
        self.state_dirty = true;
    }

    pub fn touch_meta(&mut self) {
        self.meta_dirty = true;
    }

    pub fn write(&mut self, v: &serde_json::Value) {
        self.writes.push(v.to_string());
    }

    pub fn reply(&mut self, client: String, value: serde_json::Value) {
        self.flush();
        self.out.push(Update::Reply { client, value });
    }

    pub fn set_status(&mut self, s: Status) {
        if self.state.status != s {
            self.state.status = s;
            self.state_dirty = true;
        }
    }

    /// Recompute the status from what is pending: an open approval means the agent waits.
    pub fn settle_status(&mut self, running: bool) {
        let s = match self.state.status {
            Status::Exited | Status::Disconnected | Status::Starting if !running => self.state.status,
            _ if !self.state.approvals.is_empty() => Status::Waiting,
            _ if running => Status::Running,
            _ => Status::Idle,
        };
        self.set_status(s);
    }

    pub fn add_approval(&mut self, a: Approval) {
        self.state.approvals.retain(|x| x.id != a.id);
        self.state.approvals.push(a);
        self.state_dirty = true;
    }

    pub fn take_approval(&mut self, id: &str) -> Option<Approval> {
        let pos = self.state.approvals.iter().position(|a| a.id == id)?;
        self.state_dirty = true;
        Some(self.state.approvals.remove(pos))
    }

    /// Move everything pending into `out`, in order: changed events, streamed text, then state.
    pub fn flush(&mut self) {
        let changed = std::mem::take(&mut self.changed);
        if !changed.is_empty() {
            let events: Vec<Event> = changed.iter().filter_map(|id| self.get(id).cloned()).collect();
            // Text streamed into those same events is already in them.
            self.stream.retain(|(id, _)| !changed.contains(id));
            if !events.is_empty() {
                self.out.push(Update::Events { events });
            }
        }
        for (id, text) in std::mem::take(&mut self.stream) {
            self.out.push(Update::Stream { id, text });
        }
        if self.meta_dirty {
            self.meta_dirty = false;
            self.out.push(Update::Meta { meta: Box::new(self.meta.clone()) });
        }
        if self.state_dirty {
            self.state_dirty = false;
            self.out.push(Update::State { state: Box::new(self.state.clone()) });
        }
    }

    /// Everything a newly attached viewer needs, as updates (events in bounded batches).
    pub fn snapshot(&self, batch_bytes: usize) -> Vec<Update> {
        let mut out = vec![Update::Reset, Update::Meta { meta: Box::new(self.meta.clone()) }, Update::State { state: Box::new(self.state.clone()) }];
        let mut batch: Vec<Event> = Vec::new();
        let mut size = 0usize;
        for e in &self.events {
            let n = serde_json::to_vec(e).map(|v| v.len()).unwrap_or(0);
            if size + n > batch_bytes && !batch.is_empty() {
                out.push(Update::Events { events: std::mem::take(&mut batch) });
                size = 0;
            }
            size += n;
            batch.push(e.clone());
        }
        if !batch.is_empty() {
            out.push(Update::Events { events: batch });
        }
        out
    }

    pub fn take_out(&mut self) -> Vec<Update> {
        self.flush();
        std::mem::take(&mut self.out)
    }

    pub fn take_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.writes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_keep_order_and_stream_deltas_coalesce() {
        let mut c = Core::new();
        c.upsert("a", None, Body::Assistant { text: "he".into(), streaming: true });
        c.append("a", "llo");
        let out = c.take_out();
        // Created this step: one event carrying all the text, no separate stream update.
        assert!(matches!(out.first(), Some(Update::Events { events }) if events.len() == 1 && events.first().is_some_and(|e| matches!(&e.body, Body::Assistant { text, .. } if text == "hello"))));
        assert!(!out.iter().any(|u| matches!(u, Update::Stream { .. })));
        c.append("a", " wor");
        c.append("a", "ld");
        let out = c.take_out();
        assert_eq!(out, vec![Update::Stream { id: "a".into(), text: " world".into() }]);
        c.upsert("b", None, Body::Notice { level: super::super::model::Level::Info, text: "x".into() });
        c.upsert("a", None, Body::Assistant { text: "hello world!".into(), streaming: false });
        let out = c.take_out();
        let ids: Vec<&str> = match out.first() {
            Some(Update::Events { events }) => events.iter().map(|e| e.id.as_str()).collect(),
            _ => Vec::new(),
        };
        assert_eq!(ids, vec!["b", "a"]);
        assert_eq!(c.events().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn the_timeline_is_bounded_and_snapshots_batch() {
        let mut c = Core::new();
        for i in 0..(MAX_EVENTS + 10) {
            c.upsert(&format!("e{i}"), None, Body::Assistant { text: "x".repeat(100), streaming: false });
        }
        assert_eq!(c.events().count(), MAX_EVENTS);
        assert_eq!(c.state.dropped, 10);
        assert!(c.get("e0").is_none() && c.get("e10").is_some());
        let snap = c.snapshot(64 * 1024);
        assert!(matches!(snap.first(), Some(Update::Reset)));
        let total: usize = snap.iter().map(|u| if let Update::Events { events } = u { events.len() } else { 0 }).sum();
        assert_eq!(total, MAX_EVENTS);
        assert!(snap.len() > 4);
    }
}
