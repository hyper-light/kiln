//! An agent session as a topic (`kind::SESSION`, keyed by the session's id): what its socket
//! (`/api/harness/sessions/:id/ws`) sends as whole frames, as items that change one at a time:
//! `info`, `meta`, each of its state's fields (`state.<field>`), each file of its changeset
//! (`change.<tree>|<path>`), `files`, and each event by its number, with the text streamed into it
//! appended (`Extend`, then `Tail`). Its cursor is the topic's revision; the server's copy's
//! `(born, rev)` stays the session socket's.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kiln_bp::frames::{Delta, Key, Topic};
use kiln_bp::records::SessionItem;
use kiln_bp::wire::{Bytes, Fields};
use serde_json::Value;

use super::{Out, kind};
use crate::wire::ui::SessionFrame;

fn json(v: &Value) -> Bytes {
    Bytes(serde_json::to_vec(v).unwrap_or_default())
}

/// A changed file's item key.
pub fn change_key(v: &Value) -> String {
    let path = v.get("path").and_then(Value::as_str).unwrap_or_default();
    let tree = v.get("tree").and_then(Value::as_str).unwrap_or_default();
    format!("change.{tree}|{path}")
}

/// One session's adapter: what it has published, so a frame becomes only what changed.
#[derive(Debug)]
pub struct SessionFeed {
    topic: Topic,
    state: BTreeSet<String>,
    changes: BTreeSet<String>,
    /// Each event's number by its id, and how much text has streamed into it.
    events: BTreeMap<u64, u64>,
    ids: HashMap<String, u64>,
}

impl SessionFeed {
    pub fn new(session: &str) -> Self {
        Self { topic: Topic { kind: kind::SESSION, key: session.to_owned() }, state: BTreeSet::new(), changes: BTreeSet::new(), events: BTreeMap::new(), ids: HashMap::new() }
    }

    fn put(&self, key: &str, v: Bytes) -> Out {
        Out { topic: self.topic.clone(), delta: Delta::Put { key: Key::Str { s: key.to_owned() }, fields: SessionItem { json: v, streamed: String::new() }.fields() } }
    }

    fn remove(&self, key: Key) -> Out {
        Out { topic: self.topic.clone(), delta: Delta::Remove { key } }
    }

    fn drop_events(&mut self, from: Option<u64>, out: &mut Vec<Out>) {
        let gone: Vec<u64> = self.events.keys().copied().filter(|s| from.is_none_or(|f| *s >= f)).collect();
        for s in gone {
            self.events.remove(&s);
            out.push(self.remove(Key::Num { n: s }));
        }
        self.ids.retain(|_, s| from.is_some_and(|f| *s < f));
    }

    /// The changes a frame makes, and whether the session ended.
    pub fn frame(&mut self, f: &SessionFrame) -> (Vec<Out>, bool) {
        let mut out = Vec::new();
        match f {
            SessionFrame::Open { info, .. } | SessionFrame::Resumed { info, .. } => {
                if matches!(f, SessionFrame::Open { .. }) {
                    // A session sent whole: what came before goes.
                    self.drop_events(None, &mut out);
                    for k in std::mem::take(&mut self.state) {
                        out.push(self.remove(Key::Str { s: format!("state.{k}") }));
                    }
                    for k in std::mem::take(&mut self.changes) {
                        out.push(self.remove(Key::Str { s: k }));
                    }
                }
                out.push(self.put("info", json(info)));
            }
            SessionFrame::Meta { meta } => {
                out.push(self.put("meta", json(meta)));
            }
            SessionFrame::State { state } => {
                let now: BTreeSet<String> = state.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                for k in self.state.difference(&now).cloned().collect::<Vec<_>>() {
                    out.push(self.remove(Key::Str { s: format!("state.{k}") }));
                }
                if let Some(o) = state.as_object() {
                    for (k, v) in o {
                        out.push(self.put(&format!("state.{k}"), json(v)));
                    }
                }
                self.state = now;
            }
            SessionFrame::StatePatch { set, gone } => {
                for (k, v) in set {
                    self.state.insert(k.clone());
                    out.push(self.put(&format!("state.{k}"), json(v)));
                }
                for k in gone {
                    if self.state.remove(k) {
                        out.push(self.remove(Key::Str { s: format!("state.{k}") }));
                    }
                }
            }
            SessionFrame::Changes { files } => {
                let now: BTreeSet<String> = files.as_array().map(|a| a.iter().map(change_key).collect()).unwrap_or_default();
                for k in self.changes.difference(&now).cloned().collect::<Vec<_>>() {
                    out.push(self.remove(Key::Str { s: k }));
                }
                for v in files.as_array().into_iter().flatten() {
                    out.push(self.put(&change_key(v), json(v)));
                }
                self.changes = now;
            }
            SessionFrame::ChangesPatch { set, gone } => {
                for v in set {
                    let k = change_key(v);
                    self.changes.insert(k.clone());
                    out.push(self.put(&k, json(v)));
                }
                for v in gone {
                    let k = change_key(v);
                    if self.changes.remove(&k) {
                        out.push(self.remove(Key::Str { s: k }));
                    }
                }
            }
            SessionFrame::Files { files } => {
                out.push(self.put("files", Bytes(serde_json::to_vec(files).unwrap_or_default())));
            }
            SessionFrame::Events { events } => {
                for e in events {
                    let Some(seq) = e.get("seq").and_then(Value::as_u64) else { continue };
                    if let Some(id) = e.get("id").and_then(Value::as_str) {
                        self.ids.insert(id.to_owned(), seq);
                    }
                    self.events.insert(seq, 0);
                    out.push(Out { topic: self.topic.clone(), delta: Delta::Put { key: Key::Num { n: seq }, fields: SessionItem { json: json(e), streamed: String::new() }.fields() } });
                }
            }
            SessionFrame::Stream { id, text } => {
                if let Some(seq) = self.ids.get(id).copied() {
                    let at = self.events.get(&seq).copied().unwrap_or(0);
                    self.events.insert(seq, at.saturating_add(text.len() as u64));
                    out.push(Out { topic: self.topic.clone(), delta: Delta::Extend { key: Key::Num { n: seq }, field: 1, at, data: Bytes(text.as_bytes().to_vec()) } });
                }
            }
            SessionFrame::Reset { from } => self.drop_events(*from, &mut out),
            SessionFrame::Closed => return (out, true),
            // `synced` is the protocol's `Ready`; a command's answers (`ack`, `nack`, `error`),
            // a reply to one viewer's ask and an inbox's word to its sender go to their asker as
            // `Answer`s, never to every viewer.
            SessionFrame::Synced | SessionFrame::Reply { .. } | SessionFrame::Inbox { .. } | SessionFrame::Error { .. } | SessionFrame::Ack { .. } | SessionFrame::Nack { .. } => {}
        }
        (out, false)
    }
}
