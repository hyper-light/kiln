//! The server's view of every agent session, wherever it runs: its own (`local`) and each
//! runner's. A mirror per session (the latest state, meta and timeline, as JSON) serves any number
//! of viewers on any device, its live frames on kiln's bus (`super::live`, docs/bus.md); their
//! commands go to the machine that runs the session.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize as _;
use serde_json::{Value, json};

use super::core::{MAX_EVENTS, Update};
use super::files;
use super::inbox::{self, Answer, Said};
use super::kept::{Job, Keeper, Kept};
use super::host::{Host, Inventory, Limits};
use super::kinds::KindInfo;
use super::model::{Command, State, Status};
use super::session::{Held, Info, MAX_NAME, Spec, SubagentBrief};
use super::live::{Feed, Viewer};
use crate::hub::Hub;
use crate::proto::{LANE_TERM, ServerMsg};
/// How long a session this server opened on a machine waits for the machine to announce it before
/// a report from the machine that leaves it out ends it.
const OPEN_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// A message's id (what its session's inbox takes once): none for a command that isn't a message,
/// or a message without one.
pub fn message_id(c: &Command) -> Option<String> {
    match c {
        Command::Send { id, .. } if !id.is_empty() => Some(id.clone()),
        Command::SkillRun { id: Some(id), .. } if !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local,
    Machine(String),
}

impl Location {
    pub fn target(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Machine(m) => format!("agent:{m}"),
        }
    }

    pub fn of_target(target: &str) -> Self {
        match target.strip_prefix("agent:") {
            Some(m) => Self::Machine(m.to_owned()),
            None => Self::Local,
        }
    }
}

/// One of a copy's events, or one of its changeset's files: its JSON, and its text once made. A
/// long session's copy is megabytes of JSON that every viewer's snapshot, every live frame and
/// every digest reads: each is written out once, when first asked for (or as its live frame is
/// made), and shared from then on, however many take it. Changed, it's written out again when next
/// asked for.
pub struct Entry {
    value: Value,
    text: std::sync::OnceLock<Box<str>>,
}

impl Entry {
    fn new(value: Value) -> Self {
        Self { value, text: std::sync::OnceLock::new() }
    }

    /// `value` with `text`, its JSON as [`Entry::text`] writes it (made already, for a live frame).
    fn written(value: Value, text: String) -> Self {
        let made = std::sync::OnceLock::new();
        let _ = made.set(text.into_boxed_str());
        Self { value, text: made }
    }

    /// Its JSON as frames carry it: `serde_json`'s compact form of its value.
    pub fn text(&self) -> &str {
        self.text.get_or_init(|| serde_json::to_string(&self.value).unwrap_or_default().into_boxed_str())
    }

    /// Its value, to change: its text is written again when next asked for.
    fn value_mut(&mut self) -> &mut Value {
        self.text = std::sync::OnceLock::new();
        &mut self.value
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        Self { value: self.value.clone(), text: self.text.clone() }
    }
}

impl std::ops::Deref for Entry {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}

/// A copy's changeset: its files in order, each shared, so its snapshots and patches take every
/// file that didn't change as it is (no copy, no compare, no writing it out again).
type Files = Arc<Vec<Arc<Entry>>>;

/// The session socket's frames, written straight out (into a frame's buffer, a bus payload, a
/// snapshot's gzip): `{"t":<tag>,<fields>}`, then `"rev"` last when there is one, byte for byte as
/// [`SessionDown`](crate::wire::ui::SessionDown) says them (`frames_are_written_as_the_socket_says_them`),
/// without first copying what they carry into one. Writes to a `Vec` can't fail; to a gzip
/// encoder, they fail as it does.
mod write {
    use std::io::{self, Write};

    use serde_json::Value;

    use super::Entry;

    /// `{"t":"<tag>"` and `,"<key>":`. Tags and keys are the socket's own words: nothing to escape.
    fn head(w: &mut impl Write, tag: &str, key: Option<&str>) -> io::Result<()> {
        w.write_all(br#"{"t":""#)?;
        w.write_all(tag.as_bytes())?;
        w.write_all(b"\"")?;
        if let Some(k) = key {
            w.write_all(b",\"")?;
            w.write_all(k.as_bytes())?;
            w.write_all(b"\":")?;
        }
        Ok(())
    }

    /// `,"rev":N}`, or `}`.
    fn tail(w: &mut impl Write, rev: Option<u64>) -> io::Result<()> {
        match rev {
            Some(r) => write!(w, ",\"rev\":{r}}}"),
            None => w.write_all(b"}"),
        }
    }

    /// The bytes [`head`] and [`tail`] write: a frame but for what it carries.
    pub fn envelope(tag: &str, key: Option<&str>, rev: Option<u64>) -> usize {
        let head = r#"{"t":""#.len() + tag.len() + r#"""#.len() + key.map_or(0, |k| r#",""#.len() + k.len() + r#"":"#.len());
        let tail = rev.map_or("}".len(), |r| r#","rev":"#.len() + r.checked_ilog10().map_or(1, |d| d as usize + 1) + "}".len());
        head + tail
    }

    /// The bytes [`texts`] writes of `all`: `[`, their texts with a `,` between each, `]`.
    pub fn texts_len<'a>(all: impl IntoIterator<Item = &'a Entry>) -> usize {
        let (n, bytes) = all.into_iter().fold((0usize, 0usize), |(n, bytes), e| (n + 1, bytes + e.text().len()));
        "[".len() + bytes + n.saturating_sub(1) * ",".len() + "]".len()
    }

    /// `{"t":"<tag>","<key>":<value>}`: a meta, a state, a changeset.
    pub fn value(w: &mut impl Write, tag: &str, key: &str, v: &Value, rev: Option<u64>) -> io::Result<()> {
        head(w, tag, Some(key))?;
        serde_json::to_writer(&mut *w, v)?;
        tail(w, rev)
    }

    /// `[<a>,<b>,…]` of entries' texts.
    fn texts<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a Entry>) -> io::Result<()> {
        w.write_all(b"[")?;
        for (i, e) in all.into_iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            w.write_all(e.text().as_bytes())?;
        }
        w.write_all(b"]")
    }

    /// `{"t":"events","events":[…]}`.
    pub fn events<'a>(w: &mut impl Write, all: impl IntoIterator<Item = &'a Entry>, rev: Option<u64>) -> io::Result<()> {
        head(w, "events", Some("events"))?;
        texts(w, all)?;
        tail(w, rev)
    }

    /// `{"t":"changes","files":[…]}`.
    pub fn changes<'a>(w: &mut impl Write, files: impl IntoIterator<Item = &'a Entry>, rev: Option<u64>) -> io::Result<()> {
        head(w, "changes", Some("files"))?;
        texts(w, files)?;
        tail(w, rev)
    }

    /// `{"t":"changes_patch","set":[…],"gone":[…]}`, each left out when empty.
    pub fn changes_patch(w: &mut impl Write, set: &[&Entry], gone: &[Value], rev: Option<u64>) -> io::Result<()> {
        head(w, "changes_patch", None)?;
        if !set.is_empty() {
            w.write_all(br#","set":"#)?;
            texts(w, set.iter().copied())?;
        }
        if !gone.is_empty() {
            w.write_all(br#","gone":"#)?;
            serde_json::to_writer(&mut *w, gone)?;
        }
        tail(w, rev)
    }

    /// `{"t":"state_patch","set":{…},"gone":[…]}`: `set` always, `gone` left out when empty.
    pub fn state_patch(w: &mut impl Write, set: &[(&String, &Value)], gone: &[&String], rev: Option<u64>) -> io::Result<()> {
        head(w, "state_patch", Some("set"))?;
        w.write_all(b"{")?;
        for (i, (k, v)) in set.iter().enumerate() {
            if i > 0 {
                w.write_all(b",")?;
            }
            serde_json::to_writer(&mut *w, k)?;
            w.write_all(b":")?;
            serde_json::to_writer(&mut *w, v)?;
        }
        w.write_all(b"}")?;
        if !gone.is_empty() {
            w.write_all(br#","gone":"#)?;
            serde_json::to_writer(&mut *w, gone)?;
        }
        tail(w, rev)
    }

    /// `{"t":"stream","id":…,"text":…}`.
    pub fn stream(w: &mut impl Write, id: &str, text: &str, rev: Option<u64>) -> io::Result<()> {
        head(w, "stream", Some("id"))?;
        serde_json::to_writer(&mut *w, id)?;
        w.write_all(br#","text":"#)?;
        serde_json::to_writer(&mut *w, text)?;
        tail(w, rev)
    }

    /// What `f` writes, as text: what the writers above write is JSON, so UTF-8.
    pub fn string(room: usize, f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut out = Vec::with_capacity(room);
        if f(&mut out).is_err() {
            return String::new();
        }
        String::from_utf8(out).unwrap_or_default()
    }
}

struct Mirror {
    info: Info,
    /// The last name of its own its machine reported for it (empty: none yet).
    reported: String,
    location: Location,
    /// Its meta and state, shared: a snapshot takes them under the mirrors' lock as references
    /// (no copy), and a state patch's baseline is the state as it was, not a copy of it.
    meta: Arc<Value>,
    state: Arc<Value>,
    /// The state as its viewers last had it sent (what a state patch is made against).
    sent: Arc<Value>,
    /// The bytes its last state frame took whole, and as a patch, and its last meta frame: the
    /// room the next of each is written into (they change little from one to the next).
    state_bytes: usize,
    patch_bytes: usize,
    meta_bytes: usize,
    /// Its changeset (none yet: `None`), as a list taken in by [`merge_changes`]: its viewers take
    /// a patch of it the same way, so theirs stays in this one's order.
    changes: Option<Files>,
    /// The changeset as its viewers last had it sent (what a changeset's patch is made against).
    changes_sent: Option<Files>,
    /// Shared, so a snapshot takes them under the mirrors' lock as references and is made after
    /// it's let go: a viewer starting over never holds up another session's publish.
    events: Vec<Arc<Entry>>,
    index: HashMap<String, usize>,
    /// Its live frames, on a topic of its own on the bus; each carries the copy's revision.
    feed: Feed,
    /// Every update this copy has taken, counted: its revision, what a viewer resumes from
    /// (docs/wire.md, "Revisions"). Under [`born`]: another server process's numbers aren't these.
    rev: u64,
    /// The revision it last started over at (an Open, a Reset): a viewer that had it from before
    /// that starts over too.
    reset_at: u64,
    /// The revision each event last changed at, by its id.
    changed: HashMap<String, u64>,
    /// The Open being taken starts the copy over (a whole snapshot follows it).
    started_over_pending: bool,
    /// Each event's digest as a runner that starts again compares it (`crate::wire::sums`), by
    /// its id: made when first asked for, and again once the event changes.
    sums: HashMap<String, u64>,
    /// When this server opened it, until its machine announces it: a report from the machine
    /// that left it out was made before its open got there, and doesn't end it.
    opened_at: Option<std::time::Instant>,
}

/// This server process's incarnation: viewers resume from a copy's revision only under it. 53
/// bits: it travels as a JSON number, which a browser reads exactly only up to 2^53.
pub fn born() -> u64 {
    static BORN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BORN.get_or_init(|| rand::random::<u64>() >> 11)
}

impl Mirror {
    fn new(info: Info, location: Location, bus: Arc<dyn kiln_seams::Bus>) -> Self {
        let feed = Feed::open(bus, &info.id);
        Self {
            info,
            reported: String::new(),
            location,
            meta: null(),
            state: null(),
            sent: null(),
            state_bytes: 0,
            patch_bytes: 0,
            meta_bytes: 0,
            changes: None,
            changes_sent: None,
            events: Vec::new(),
            index: HashMap::new(),
            feed,
            rev: 0,
            reset_at: 0,
            changed: HashMap::new(),
            started_over_pending: false,
            sums: HashMap::new(),
            opened_at: None,
        }
    }

    /// A frame to this copy's viewers, written into its payload at the copy's revision.
    fn send(&mut self, p: super::live::Payload) {
        self.feed.publish_payload(p);
    }

    /// The state as a frame for its viewers: whole for those that take it so (or when there's
    /// nothing to patch), and as what changed in it since the last one for those that take that;
    /// a state none takes whole goes as its patch alone. None without viewers.
    fn state_frame(&mut self) -> Option<super::live::Payload> {
        if self.feed.viewers() == 0 {
            // The next viewer starts from a snapshot, not the last viewer's patch baseline.
            self.sent = null();
            return None;
        }
        let rev = Some(self.rev);
        let (mut p, whole, patched) = {
            let patch = state_patch_of(&self.sent, &self.state);
            let whole = patch.is_none() || self.feed.wants_whole(PatchOf::State);
            let room = if whole { self.state_bytes } else { 0 } + if patch.is_some() { self.patch_bytes } else { 0 };
            let mut p = super::live::Payload::new(self.rev, room);
            let start = p.text().len();
            if whole {
                let _ = write::value(p.text(), "state", "state", &self.state, rev);
            }
            let text = p.text().len();
            if let Some((set, gone)) = &patch {
                let _ = write::state_patch(p.patch(), set, gone, rev);
            }
            let end = p.text().len();
            (p, whole.then_some(text - start), patch.is_some().then_some(end - text))
        };
        p.done();
        self.state_bytes = whole.unwrap_or(self.state_bytes);
        self.patch_bytes = patched.unwrap_or(self.patch_bytes);
        self.sent = Arc::clone(&self.state);
        Some(p)
    }

    /// The changeset as a frame for its viewers, with what changed in it since the last one: a
    /// file the agent touches doesn't send them every file again (and, when none of them takes the
    /// changeset whole, isn't sent whole at all: a long session's is larger than the bus takes).
    fn changes_frame(&mut self) -> Option<super::live::Payload> {
        if self.feed.viewers() == 0 {
            // The next viewer starts from a snapshot, not the last viewer's patch baseline.
            self.changes_sent = None;
            return None;
        }
        let now = self.changes.clone()?;
        let rev = Some(self.rev);
        let mut p = {
            let patch = self.changes_sent.as_deref().and_then(|was| changes_patch_of(was, &now));
            let whole = patch.is_none() || self.feed.wants_whole(PatchOf::Changes);
            // Its files as they're written out already, in their envelopes (a patch's gone keys
            // past that: a few short paths).
            let room = if whole { write::envelope("changes", Some("files"), rev) + write::texts_len(now.iter().map(|f| &**f)) } else { 0 };
            let room = room + patch.as_ref().map_or(0, |(set, _)| write::envelope("changes_patch", None, rev) + r#","set":"#.len() + write::texts_len(set.iter().copied()));
            let mut p = super::live::Payload::new(self.rev, room);
            if whole {
                let _ = write::changes(p.text(), now.iter().map(|f| &**f), rev);
            }
            if let Some((set, gone)) = &patch {
                let _ = write::changes_patch(p.patch(), set, gone, rev);
            }
            p
        };
        p.done();
        self.changes_sent = Some(now);
        Some(p)
    }

    /// A frame of `u` for its viewers (an event, a reply's words, an announcement…): `entries`,
    /// its events as the copy took them, written out once already.
    fn frame_of(&mut self, u: &Update, entries: &[Arc<Entry>]) -> super::live::Payload {
        let rev = Some(self.rev);
        let mut p = match u {
            Update::Events { .. } => {
                let mut p = super::live::Payload::new(self.rev, write::envelope("events", Some("events"), rev) + write::texts_len(entries.iter().map(|e| &**e)));
                let _ = write::events(p.text(), entries.iter().map(|e| &**e), rev);
                p
            }
            Update::Stream { id, text } => {
                // Its id and words as JSON strings, unescaped (escaping makes them longer).
                let strings = r#""""#.len() + id.len() + r#","text":"#.len() + r#""""#.len() + text.len();
                let mut p = super::live::Payload::new(self.rev, write::envelope("stream", Some("id"), rev) + strings);
                let _ = write::stream(p.text(), id, text, rev);
                p
            }
            Update::Meta { .. } => {
                let mut p = super::live::Payload::new(self.rev, self.meta_bytes);
                let start = p.text().len();
                let _ = write::value(p.text(), "meta", "meta", &self.meta, rev);
                self.meta_bytes = p.text().len() - start;
                p
            }
            other => {
                let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::of(other), rev }.text();
                let mut p = super::live::Payload::new(self.rev, text.len());
                p.text().extend_from_slice(text.as_bytes());
                p
            }
        };
        p.done();
        p
    }

    /// Takes in a changeset, whole (`whole`: the files it doesn't have are gone) or as what changed
    /// in it, in place in the copy's ([`merge_changes`]). The files that didn't change stay as they
    /// were, shared with what viewers were last sent.
    fn take_changes(&mut self, set: Vec<Value>, gone: Vec<Value>, whole: bool) {
        let mut files: Vec<Arc<Entry>> = match self.changes.take() {
            // Its own, or (shared with what its viewers were last sent) a list of the same files.
            Some(all) => Arc::try_unwrap(all).unwrap_or_else(|shared| (*shared).clone()),
            None => Vec::new(),
        };
        if whole {
            let kept: std::collections::HashSet<FileRef<'_>> = set.iter().map(file_ref).collect();
            files.retain(|f| kept.contains(&file_ref(f)));
        }
        merge_changes(&mut files, set, if whole { &[] } else { gone.as_slice() });
        self.changes = Some(Arc::new(files));
    }

    fn reindex(&mut self) {
        self.index = self.events.iter().enumerate().filter_map(|(i, e)| Some((e.get("id")?.as_str()?.to_owned(), i))).collect();
        let index = &self.index;
        self.changed.retain(|id, _| index.contains_key(id));
        self.sums.retain(|id, _| index.contains_key(id));
    }

    /// Lets go of every event.
    fn clear_events(&mut self) {
        self.events.clear();
        self.index.clear();
        self.sums.clear();
    }

    /// Takes in an event, by its id. One it hasn't had numbered before its newest (a runner that
    /// started again sends it again: the copy's differed, `crate::wire::sums`) goes in its place
    /// by number, instead of one the copy has under that number: true when that one had another id
    /// (the timeline went another way), which its viewers can't see from the event alone.
    fn upsert(&mut self, e: Arc<Entry>) -> bool {
        let Some(id) = e.get("id").and_then(Value::as_str).map(str::to_owned) else { return false };
        self.changed.insert(id.clone(), self.rev);
        self.sums.remove(&id);
        if let Some(slot) = self.index.get(&id).and_then(|i| self.events.get_mut(*i)) {
            *slot = e;
            return false;
        }
        let seq = |v: &Value| v.get("seq").and_then(Value::as_u64);
        if let (Some(q), Some(top)) = (seq(&e), self.events.last().and_then(|last| seq(last)))
            && q <= top
        {
            let at = self.events.partition_point(|x| seq(x).is_some_and(|s| s < q));
            let displaced = match self.events.get_mut(at) {
                Some(slot) if seq(slot) == Some(q) => {
                    *slot = e;
                    true
                }
                _ => {
                    self.events.insert(at, e);
                    false
                }
            };
            self.reindex();
            return displaced;
        }
        self.index.insert(id, self.events.len());
        self.events.push(e);
        if self.events.len() > MAX_EVENTS + MAX_EVENTS / 4 {
            // From where a run of the digests begins, as its machine's timeline lets go of its
            // oldest (`crate::wire::sums::run_after`).
            let over = self.events.len() - MAX_EVENTS;
            let drop = match self.events.get(over).and_then(|e| seq(e)) {
                Some(q) => {
                    let from = crate::wire::sums::run_after(q);
                    self.events.partition_point(|x| seq(x).is_some_and(|s| s < from))
                }
                None => over,
            };
            // Those let go leave the index, and the rest move down by as many: the index isn't
            // made again (a long session lets go of its oldest every thousand events or so).
            for gone in self.events.drain(..drop) {
                if let Some(id) = gone.get("id").and_then(Value::as_str) {
                    self.index.remove(id);
                    self.changed.remove(id);
                    self.sums.remove(id);
                }
            }
            for at in self.index.values_mut() {
                *at = at.saturating_sub(drop);
            }
        }
        false
    }

    fn append(&mut self, id: &str, text: &str) {
        // An event a snapshot still holds is copied first (that one alone), so the snapshot keeps
        // what it took.
        if let Some(e) = self.index.get(id).and_then(|i| self.events.get_mut(*i))
            && let Some(Value::String(s)) = Arc::make_mut(e).value_mut().get_mut("text")
        {
            s.push_str(text);
            self.changed.insert(id.to_owned(), self.rev);
            self.sums.remove(id);
        }
    }

    /// What a snapshot is made of, taken under the mirrors' lock: everything as references (its
    /// info aside, a few hundred bytes). Its frames are written after the lock is let go, off the
    /// runtime's threads ([`Snapshot`]).
    fn parts(&self) -> Parts {
        Parts {
            info: json!(self.info),
            meta: Arc::clone(&self.meta),
            state: Arc::clone(&self.state),
            changes: self.changes.clone(),
            events: self.events.clone(),
            meta_bytes: self.meta_bytes,
            state_bytes: self.state_bytes,
            rev: self.rev,
            resumed: false,
        }
    }

    /// What brings a viewer that has this copy through revision `after` up to now, taken as
    /// [`Mirror::parts`] is: its info and revision, the newest meta, state and changes, then only the
    /// events changed since. None when it can't be resumed: it started over since (an Open, a
    /// Reset), or `after` is past what it has (another process's number).
    fn parts_since(&self, after: u64) -> Option<Parts> {
        if after < self.reset_at || after > self.rev {
            return None;
        }
        let changed = |e: &&Arc<Entry>| e.get("id").and_then(Value::as_str).and_then(|id| self.changed.get(id)).is_some_and(|r| *r > after);
        Some(Parts {
            info: json!(self.info),
            meta: Arc::clone(&self.meta),
            state: Arc::clone(&self.state),
            changes: self.changes.clone(),
            events: self.events.iter().filter(changed).cloned().collect(),
            meta_bytes: self.meta_bytes,
            state_bytes: self.state_bytes,
            rev: self.rev,
            resumed: true,
        })
    }

    /// What brings a viewer that has this copy through revision `after` up to now
    /// ([`Mirror::parts_since`], made at once).
    #[cfg(test)]
    fn resume(&self, after: u64, files: Option<String>) -> Option<Vec<String>> {
        self.parts_since(after).map(|parts| Snapshot { parts, files }.frames())
    }

    /// What this copy has of its events, said in digests (`crate::wire::sums`): each event's
    /// digest is of its text, written out once.
    fn kept(&mut self, session: &str) -> crate::proto::KeptSession {
        let sums = &mut self.sums;
        let mut events: Vec<(u64, u64)> = self
            .events
            .iter()
            .filter_map(|e| {
                let seq = e.get("seq").and_then(Value::as_u64)?;
                let id = e.get("id").and_then(Value::as_str)?;
                let sum = match sums.get(id) {
                    Some(d) => *d,
                    None => *sums.entry(id.to_owned()).or_insert_with(|| crate::wire::sums::event_text(e.text())),
                };
                Some((seq, sum))
            })
            .collect();
        events.sort_unstable_by_key(|(seq, _)| *seq);
        crate::wire::sums::kept(session.to_owned(), self.info.epoch, &events)
    }

    /// Its viewers start over from the copy as it is (its events went another way than they have
    /// them): the feed's resync marker sends each back to a snapshot, and none resumes from before
    /// it.
    fn resync(&mut self) {
        self.started_over();
        if self.feed.viewers() > 0 {
            self.feed.publish(&Frame::resync());
        }
    }

    /// The copy starts over (an Open, a Reset): a viewer from before it can't resume.
    fn started_over(&mut self) {
        self.reset_at = self.rev;
        self.changed.clear();
    }
}

/// `value` kept in `dir`'s `file`, whole (written beside it, then moved over it).
fn keep_json_in(dir: &std::path::Path, file: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let path = dir.join(file);
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// A shared JSON `null`: a copy's meta or state before it has one.
fn null() -> Arc<Value> {
    static NULL: std::sync::LazyLock<Arc<Value>> = std::sync::LazyLock::new(|| Arc::new(Value::Null));
    Arc::clone(&NULL)
}

/// A snapshot of a mirror, or what a viewer lacks of it since a revision, not yet made into frames
/// ([`Mirror::parts`], [`Mirror::parts_since`]): references, taken under the mirrors' lock.
struct Parts {
    info: Value,
    meta: Arc<Value>,
    state: Arc<Value>,
    changes: Option<Files>,
    events: Vec<Arc<Entry>>,
    /// Its meta's and state's last frames' sizes ([`Mirror::meta_bytes`]): the room theirs take.
    meta_bytes: usize,
    state_bytes: usize,
    /// The copy's revision as of the parts.
    rev: u64,
    /// Since a revision (it begins with `resumed`, the events changed since alone), or the session
    /// whole (it begins with `open`).
    resumed: bool,
}

/// A snapshot's events go in frames of about this many bytes.
const EVENTS_FRAME: usize = 256 * 1024;

/// A viewer's snapshot (or catch-up), taken but not written out: written on the blocking pool, off
/// the runtime's threads, a long session's being megabytes of JSON (`crate::rt::blocking`). Its
/// frames say the copy as of when it was taken, whatever comes in meanwhile.
pub struct Snapshot {
    parts: Parts,
    /// What's kept of the files its messages mention (`Keeper::frame`): ahead of the events.
    files: Option<String>,
}

impl Snapshot {
    /// The events' frames: where each begins and ends in `parts.events`, about [`EVENTS_FRAME`]
    /// bytes each (one alone when it's larger).
    fn batches(&self) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let (mut from, mut size) = (0usize, 0usize);
        for (i, e) in self.parts.events.iter().enumerate() {
            let n = e.text().len();
            if size + n > EVENTS_FRAME && i > from {
                out.push(from..i);
                from = i;
                size = 0;
            }
            size += n;
        }
        if from < self.parts.events.len() {
            out.push(from..self.parts.events.len());
        }
        out
    }

    /// Writes each frame in turn with `each`, which is given the frame's own bytes to write and
    /// how many there are about to be.
    fn write_each(&self, mut each: impl FnMut(usize, &dyn Fn(&mut dyn std::io::Write) -> std::io::Result<()>) -> std::io::Result<()>) -> std::io::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let p = &self.parts;
        let (info, born) = (p.info.clone(), Some(born()));
        let head = SessionDown { frame: if p.resumed { SessionFrame::Resumed { info, born } } else { SessionFrame::Open { info, born } }, rev: Some(p.rev) }.text();
        each(head.len(), &|w| w.write_all(head.as_bytes()))?;
        if !p.meta.is_null() {
            each(p.meta_bytes, &|mut w| write::value(&mut w, "meta", "meta", &p.meta, None))?;
        }
        if !p.state.is_null() {
            each(p.state_bytes, &|mut w| write::value(&mut w, "state", "state", &p.state, None))?;
        }
        if let Some(files) = &p.changes {
            let room = write::envelope("changes", Some("files"), None) + write::texts_len(files.iter().map(|f| &**f));
            each(room, &|mut w| write::changes(&mut w, files.iter().map(|f| &**f), None))?;
        }
        if let Some(f) = &self.files {
            each(f.len(), &|w| w.write_all(f.as_bytes()))?;
        }
        for range in self.batches() {
            let all = p.events.get(range).unwrap_or_default();
            let room = write::envelope("events", Some("events"), None) + write::texts_len(all.iter().map(|e| &**e));
            each(room, &|mut w| write::events(&mut w, all.iter().map(|e| &**e), None))?;
        }
        Ok(())
    }

    /// The frames that bring a viewer to the copy as it was when this was taken, a message each.
    pub fn frames(&self) -> Vec<String> {
        let mut out = Vec::new();
        let _ = self.write_each(|room, write| {
            out.push(write::string(room, |b| write(b)));
            Ok(())
        });
        out
    }

    /// Its frames and then `last` (the socket's `synced`), a frame a line, gzip'd as one message.
    /// Each is written into one buffer used again for the next, and goes to the encoder whole:
    /// written straight into the encoder, a frame's many small writes cost it more than the frame.
    pub fn packed(&self, last: &str) -> std::io::Result<Vec<u8>> {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut frame: Vec<u8> = Vec::new();
        self.write_each(|room, write| {
            frame.clear();
            frame.reserve(room);
            write(&mut frame)?;
            frame.push(b'\n');
            gz.write_all(&frame)
        })?;
        gz.write_all(last.as_bytes())?;
        gz.finish()
    }
}

/// The epoch to send a message once more for, when its session's inbox refused it as sent for an
/// epoch the session has since left (the server sends it once more, for the one named). The sends
/// simulation runs this too (`inbox::sim`).
pub(crate) fn again_for(r: &inbox::Refusal) -> Option<u64> {
    match r {
        inbox::Refusal::StaleEpoch { current, sent, .. } if current > sent => Some(*current),
        _ => None,
    }
}

/// How a command fared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// On its way to the session's machine, which can't say more: a message whose inbox's answer
    /// didn't come in time, or a command or message to a runner from before the channel (or from
    /// before inboxes). Also what kiln does here itself (a name kept).
    Ok,
    /// The session's inbox took the message, or had: `repeat`, the first time's answer (CP-T3.2).
    Accepted(Answer),
    /// The session's inbox refused the message, saying why.
    Refused(inbox::Refusal),
    /// The session took the command: its runner applied it, or the server did (docs/wire.md,
    /// "Commands").
    Taken,
    /// The session's runner refused the command, saying how and why: one it doesn't run there any
    /// more is `unknown`.
    Rejected { code: crate::wire::channel::Refusal, why: String },
    /// The runner may or may not have applied it: its link dropped before it answered a command
    /// that doesn't wait, or no answer came in time.
    Unanswered(String),
    /// A command that waits for its runner, which is away: it goes once the runner is back.
    Parked,
    NoSession,
    /// The session's machine is not connected right now.
    Unreachable,
}

/// A command sent ([`Registry::dispatch`]): how it fared already, or the answer still to come.
pub enum Pending {
    Now(Sent),
    /// A message on its way, its inbox's answer waited for ([`Registry::settle`]).
    Answer { message: String, rx: futures::channel::oneshot::Receiver<Said> },
    /// A command on its runner's command stream, its answer to come ([`Sent::of`]).
    Command(crate::hub::Pending),
}

/// What a sender is told of message `message` the session's machine took: its inbox's answer and,
/// from a machine without inboxes (`seq` 0: none took it), that nothing there checks it for repeats
/// (docs/sends.md, "Off").
pub fn accepted(message: impl serde::Serialize, a: &Answer) -> Value {
    let mut said = json!({"sent": true, "message": message, "accepted": a});
    if a.seq == 0
        && let Some(o) = said.as_object_mut()
    {
        o.insert("unchecked".into(), json!("the agent's machine checks no messages for repeats (its inbox is off): sent again, this is another message"));
    }
    said
}

impl Sent {
    /// It got to the session's machine (and its inbox didn't refuse it), or waits for it.
    pub fn went(&self) -> bool {
        matches!(self, Self::Ok | Self::Accepted(_) | Self::Taken | Self::Parked)
    }

    /// What a runner's answer to a command says of it (`crate::wire::commands`).
    pub fn of(a: crate::wire::commands::Answer) -> Self {
        use crate::wire::commands::Answer;
        match a {
            Answer::Taken => Self::Taken,
            Answer::Refused { code, why } => Self::Rejected { code, why },
            Answer::Unreachable { .. } => Self::Unreachable,
            Answer::Unanswered { why } => Self::Unanswered(why),
            Answer::Sent => Self::Ok,
            Answer::Parked => Self::Parked,
        }
    }

    /// For a command's caller over HTTP: the status and words of one that didn't go (a message's
    /// answers are its inbox's, `accepted` and `inbox::Refusal`). None: it was taken, sent, or
    /// waits for its runner.
    pub fn failure(&self) -> Option<(http::StatusCode, String)> {
        use crate::wire::channel::Refusal as Code;
        use http::StatusCode;
        match self {
            Self::Ok | Self::Accepted(_) | Self::Taken | Self::Parked => None,
            Self::Refused(r) => Some((StatusCode::CONFLICT, r.to_string())),
            Self::Rejected { code: Code::Unknown, why } => Some((StatusCode::NOT_FOUND, format!("its runner doesn't have it: {why}"))),
            Self::Rejected { code: Code::StaleEpoch, why } => Some((StatusCode::CONFLICT, format!("its runner let go of it: {why}"))),
            Self::Rejected { why, .. } => Some((StatusCode::CONFLICT, format!("its runner refused it: {why}"))),
            Self::Unanswered(why) => Some((StatusCode::GATEWAY_TIMEOUT, format!("its runner didn't say whether it did it: {why}"))),
            Self::NoSession => Some((StatusCode::NOT_FOUND, "no such agent session".to_owned())),
            Self::Unreachable => Some((StatusCode::CONFLICT, "the session's machine is not connected".to_owned())),
        }
    }
}

/// How long a message waits for its session's inbox to answer: past it, it's said to be on its way.
pub const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Messages waiting for their inbox's answer at once, at most: past it, one isn't waited for.
pub const WAITING_MAX: usize = 10_000;

/// What a viewer gets on attach: its snapshot (written out off the runtime's threads,
/// [`Snapshot`]), and where on the session's topic the live frames after it start (none while the
/// bus can't hold its feed: snapshots alone, then).
pub struct Attachment {
    pub snapshot: Snapshot,
    pub topic: String,
    pub at: Option<kiln_seams::bus::Offset>,
    /// Its place among the session's viewers, while it watches.
    pub viewer: Viewer,
    /// The feed's refusals when the snapshot was taken: more later means the feed broke since.
    pub refusals: u64,
    /// The copy's revision as of `frames`: what its viewer resumes from.
    pub rev: u64,
}

impl Attachment {
    /// Its snapshot's frames, written out here and now (tests, and what isn't on a runtime thread).
    pub fn frames(&self) -> Vec<String> {
        self.snapshot.frames()
    }
}

/// A live frame for a session's viewers: a frame of the session socket's ([`SessionDown`],
/// serialized), as the session's feed carries it, so the registry and its viewers share the type. A
/// state comes as well as only what changed since the last one (`patch`), for a viewer that takes
/// that: during a turn its usage and activity change all the time, its skills, plugins and servers
/// seldom. So does a changeset: an agent touches a file or two of the hundreds it may have changed.
///
/// [`SessionDown`]: crate::wire::ui::SessionDown
#[derive(Clone)]
pub struct Frame {
    pub text: Arc<str>,
    pub patch: Option<Arc<str>>,
    /// What `patch` is a patch of: a viewer gets it if it takes that kind.
    pub patch_of: PatchOf,
    /// The copy's revision as it went (`"rev"` in its JSON): what its viewer resumes from.
    pub rev: u64,
}

/// What a frame's patch is a patch of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOf {
    /// A state (`state_patch`).
    State,
    /// A changeset (`changes_patch`).
    Changes,
}

impl PatchOf {
    /// What `patch` (a frame's JSON, its tag first) is a patch of: a frame on the bus carries no
    /// more than its text (`super::live::encode`), and one from before changesets had patches is a
    /// state's.
    pub fn of(patch: &str) -> Self {
        if patch.starts_with(r#"{"t":"changes_patch""#) { Self::Changes } else { Self::State }
    }
}

/// The patches a viewer takes, as its socket's query says (`patch=state`, `patch=changes`): one
/// from before a kind takes that kind whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Patches {
    pub state: bool,
    pub changes: bool,
}

impl Patches {
    /// Whether a viewer takes `of` as patches.
    pub fn takes(self, of: PatchOf) -> bool {
        match of {
            PatchOf::State => self.state,
            PatchOf::Changes => self.changes,
        }
    }
}

impl Frame {
    /// `down`, as the feed carries it.
    pub fn of(down: crate::wire::ui::SessionDown) -> Self {
        Self { text: Arc::from(down.text()), patch: None, patch_of: PatchOf::State, rev: down.rev.unwrap_or(0) }
    }

    /// The feed's resync marker ([`super::live::RESYNC`]): its viewers start over from a snapshot.
    /// It's the feed's own, never sent to a viewer.
    pub fn resync() -> Self {
        Self { text: Arc::from(super::live::RESYNC), patch: None, patch_of: PatchOf::State, rev: 0 }
    }

    /// Any text as a frame: the feed's tests, which carry words of their own.
    #[cfg(test)]
    pub fn text(text: &str) -> Self {
        Self { text: Arc::from(text), patch: None, patch_of: PatchOf::State, rev: 0 }
    }

    /// The frame with its copy's revision in it, as `"rev"`: as [`SessionDown`] says it, its last
    /// field (`a_frame_says_its_copys_revision`). Live frames are written with it now
    /// (`write`); this says what they must match.
    ///
    /// [`SessionDown`]: crate::wire::ui::SessionDown
    #[cfg(test)]
    fn stamped(self, rev: u64) -> Self {
        let stamp = |t: &str| -> Arc<str> {
            match t.strip_suffix('}') {
                Some(head) if head.len() > 1 => Arc::from(format!("{head},\"rev\":{rev}}}")),
                _ => Arc::from(t),
            }
        };
        Self { text: stamp(&self.text), patch: self.patch.as_deref().map(stamp), patch_of: self.patch_of, rev }
    }

    /// The frame for a viewer that takes the patches `takes` says.
    pub fn for_viewer(&self, takes: Patches) -> &str {
        match &self.patch {
            Some(p) if takes.takes(self.patch_of) => p,
            _ => &self.text,
        }
    }
}

/// What changed in a state, for `{"t":"state_patch","set":{..},"gone":[..]}`: the top-level fields
/// of `now` that differ from `was` (by reference: nothing copied), and those it no longer has. None
/// when there is nothing to patch (no state before).
#[allow(clippy::type_complexity)]
fn state_patch_of<'a>(was: &'a Value, now: &'a Value) -> Option<(Vec<(&'a String, &'a Value)>, Vec<&'a String>)> {
    let (Value::Object(was), Value::Object(now)) = (was, now) else { return None };
    let set = now.iter().filter(|(k, v)| was.get(*k) != Some(*v)).collect();
    let gone = was.keys().filter(|k| !now.contains_key(*k)).collect();
    Some((set, gone))
}

/// A changeset's file as its JSON names it ([`crate::harness::model::FileKey`]): its path, and
/// the top of its working tree when it's in one. Borrowed from the file: naming a file makes nothing.
type FileRef<'a> = (Option<&'a str>, Option<&'a str>);

fn file_ref(f: &Value) -> FileRef<'_> {
    (f.get("path").and_then(Value::as_str), f.get("tree").and_then(Value::as_str))
}

/// A changeset's file's key, as a patch says it's gone (`{"path", "tree"?}`).
fn file_key(f: &Value) -> Value {
    let mut key = serde_json::Map::new();
    for k in ["path", "tree"] {
        if let Some(v) = f.get(k).filter(|v| !v.is_null()) {
            key.insert(k.to_owned(), v.clone());
        }
    }
    Value::Object(key)
}

/// Takes `set` and `gone` into `files`, as a changeset's patch says: each file set goes in place of
/// the one with its key, or after the rest when it's new; each gone goes. A viewer takes a patch
/// the same way (the UI's `changesPatched`), so its list keeps this one's order.
fn merge_changes(files: &mut Vec<Arc<Entry>>, set: Vec<Value>, gone: &[Value]) {
    if !gone.is_empty() {
        let out: std::collections::HashSet<FileRef<'_>> = gone.iter().map(file_ref).collect();
        files.retain(|f| !out.contains(&file_ref(f)));
    }
    // Where each goes, by its key, borrowed (nothing copied to look it up): in place of the file
    // with its key, or after the rest, as one new before it with its key was.
    let places: Vec<usize> = {
        let mut at: HashMap<FileRef<'_>, usize> = files.iter().enumerate().map(|(i, f)| (file_ref(f), i)).collect();
        let mut next = files.len();
        set.iter()
            .map(|f| {
                *at.entry(file_ref(f)).or_insert_with(|| {
                    next += 1;
                    next - 1
                })
            })
            .collect()
    };
    for (f, i) in set.into_iter().zip(places) {
        let entry = Arc::new(Entry::new(f));
        match files.get_mut(i) {
            Some(slot) => *slot = entry,
            None => files.push(entry),
        }
    }
}

/// What changed in a changeset, for `{"t":"changes_patch","set":[..],"gone":[..]}`: the files of
/// `now` new or different from `was`'s, in `now`'s order (a file shared with `was` didn't change,
/// and isn't compared), and the keys of those it no longer has: taken into `was` as
/// [`merge_changes`] does, they make `now`. None when they wouldn't make `now` (a changeset that
/// started over in another order): then it goes whole.
fn changes_patch_of<'a>(was: &'a [Arc<Entry>], now: &'a [Arc<Entry>]) -> Option<(Vec<&'a Entry>, Vec<Value>)> {
    let before: HashMap<FileRef<'a>, &'a Arc<Entry>> = was.iter().map(|f| (file_ref(f), f)).collect();
    let kept: std::collections::HashSet<FileRef<'a>> = now.iter().map(|f| file_ref(f)).collect();
    let set: Vec<&'a Entry> = now.iter().filter(|f| before.get(&file_ref(f)).is_none_or(|b| !Arc::ptr_eq(b, f) && b.value != f.value)).map(|f| &**f).collect();
    let gone: Vec<Value> = was.iter().filter(|f| !kept.contains(&file_ref(f))).map(|f| file_key(f)).collect();
    // Taken in, the files keep their places and the new ones come after: `now` must be in that order.
    let order = was.iter().map(|f| file_ref(f)).filter(|id| kept.contains(id)).chain(set.iter().map(|f| file_ref(f)).filter(|id| !before.contains_key(id)));
    if !order.eq(now.iter().map(|f| file_ref(f))) {
        return None;
    }
    Some((set, gone))
}

/// [`changes_patch_of`] of two changesets as JSON, written out: None when either isn't one.
#[cfg(test)]
fn changes_patch(was: &Value, now: &Value) -> Option<String> {
    let files = |v: &Value| v.as_array().map(|a| a.iter().cloned().map(|f| Arc::new(Entry::new(f))).collect::<Vec<_>>());
    let (was, now) = (files(was)?, files(now)?);
    changes_patch_of(&was, &now).map(|(set, gone)| write::string(256, |b| write::changes_patch(b, &set, &gone, None)))
}

/// A file uploaded for a message, kept briefly until the message is sent.
struct Upload {
    name: String,
    mime: String,
    data: bytes::Bytes,
    at: std::time::Instant,
    /// When a message first carried it: kept after, so a message sent again (its answer lost, or
    /// sent once more for the epoch its session is at) carries it too, and reads as the same
    /// message (CP-T3.2).
    sent: Option<std::time::Instant>,
}

/// Uploads kept at most this long, and this much in all.
const UPLOAD_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);
const UPLOADS_MAX: usize = 512 << 20;
/// Pieces a file is sent to a machine in (lane frames are bounded).
const FILE_CHUNK: usize = 512 * 1024;

/// Restoring the conversation store from a mirror's copy of it (`docs/conversations.md`,
/// "Mirrors"): `GET /api/conversations/restore`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Restore {
    Idle,
    /// `from` was asked for its copy.
    Asked { from: String, at: i64 },
    /// `from` is sending its copy `store`.
    Copying { from: String, store: String, at: i64 },
    /// Done: `sessions` came back.
    Done { from: String, sessions: usize, at: i64 },
    Failed { from: String, error: String, at: i64 },
}

/// A mirror asked for its copy that hasn't begun sending it after this long isn't answering.
const RESTORE_ANSWER_MS: i64 = 120_000;

pub struct Registry {
    mirrors: Mutex<HashMap<String, Mirror>>,
    /// The bus each session's live feed rides (`super::live`).
    bus: Arc<dyn kiln_seams::Bus>,
    uploads: Mutex<HashMap<String, Upload>>,
    /// What each machine offers ("local" included).
    kinds: Mutex<HashMap<String, Vec<KindInfo>>>,
    /// The server's own sessions.
    pub local: Arc<Host>,
    /// The server's conversation store (docs/conversations.md), when it's on.
    pub convo: Option<crate::convo::store::Store>,
    /// What runners' feeds send into it (`crate::convo::feed`), and what to answer them.
    pub feed: Option<crate::convo::feed::Intake>,
    feed_out: Option<async_channel::Receiver<crate::convo::feed::Out>>,
    /// The runners that keep a copy of it (`crate::convo::mirror`), and what to send them.
    pub copies: Option<Arc<crate::convo::mirror::Mirrors>>,
    copies_out: Option<async_channel::Receiver<(hyper_transport::PeerId, String, Vec<u8>)>>,
    /// Every store, as the server sees it: the System page's (`super::stores`).
    pub board: crate::convo::board::Board,
    local_rx: async_channel::Receiver<(String, Update)>,
    /// Where `kiln spawn` in the server's own terminals reaches `local`.
    local_sock: Mutex<Option<String>>,
    /// The shell pid of each of the server's own terminals (their directory and environment are
    /// where agents started "in this shell" begin).
    local_shells: Mutex<HashMap<String, u32>>,
    dir: std::path::PathBuf,
    /// The server machine's transcripts, for its past sessions (made on first use).
    scanner: Arc<Mutex<Option<crate::aisessions::Scanner>>>,
    /// Names people gave sessions, by id (the "rename" control), kept in `names.json`: a session
    /// takes its name here at once, whatever its machine's runner knows of names (one from before
    /// kiln named sessions never reports it), and keeps it across restarts.
    names: Mutex<HashMap<String, String>>,
    /// Sessions kiln doesn't keep going when they stop part-way (`goals::keep`; the "keep_going"
    /// control), kept in `keep-off.json` so it holds across restarts.
    keep_off: Mutex<std::collections::HashSet<String>>,
    /// The UI's commands by the ids their clients made, each taken once (`super::asked`).
    pub asked: super::asked::Asked,
    /// Which of its machine's runner processes holds each session on a machine (its instance:
    /// `main`, a uuid, `legacy`), kept in `holders.json` so it holds across restarts: a process
    /// that says what it holds speaks for its own sessions, never for another's that is away
    /// ([`Registry::inventory`]). Shared with the writes of it, which go on the blocking pool.
    holders: Arc<Mutex<HashMap<String, String>>>,
    /// A write of `holders.json` is on its way (it writes what's newest as it goes); and the one
    /// writing it now (they share the file, and the one it's written beside first).
    holders_saving: Arc<std::sync::atomic::AtomicBool>,
    holders_writing: Arc<Mutex<()>>,
    /// Each session's changeset to keep in the conversation store, the newest alone (shared with
    /// its copy, nothing written out yet), and whether its writer is at work
    /// ([`Registry::store_changes`]).
    changes_to_store: Arc<Mutex<HashMap<String, Files>>>,
    changes_storing: Arc<std::sync::atomic::AtomicBool>,
    /// Each session's latest epoch (`State::epoch`) seen, those that went on on another machine at
    /// least once, kept in `epochs.json`: a machine that announces an older copy lets go of it.
    epochs: Mutex<HashMap<String, u64>>,
    /// The plan each session last proposed (plan mode), not yet kept as one of kiln's plans: the
    /// goals' driver keeps it ([`Self::take_proposed`], `crate::plans::keep_proposed`).
    proposed: Mutex<HashMap<String, String>>,
    /// Restoring the conversation store from a mirror's copy of it.
    restore: Mutex<Restore>,
    /// The mirrors to ask for their copy as they connect: the store was found empty at start
    /// while the server had mirrors (it was lost, the rest wasn't).
    restore_auto: Mutex<Vec<String>>,
    /// Files finished messages mention, kept so they still show after they go (`super::kept`).
    pub keeper: Keeper,
    /// Pages agents show in their conversations (`super::visuals`).
    pub visuals: super::visuals::Visuals,
    /// Pages streamed from agents' machines, relayed to their viewers (`super::streams`).
    pub streams: super::streams::Relay,
    /// Streams to their viewers over WebRTC, when the server has it (`super::webrtc`).
    pub webrtc: super::webrtc::Webrtc,
    /// Connections to ports of agents' machines, and the apps opened on people's devices over
    /// them (`super::opened`).
    pub ports: super::opened::Tunnels,
    pub opened: super::opened::Openings,
    /// Messages waiting for their session's inbox to answer (CP-T3.2), by session and message id.
    waiting: Mutex<HashMap<(String, String), Vec<futures::channel::oneshot::Sender<Said>>>>,
    /// The ids starts under way asked for ([`Registry::open_as`]), each until its start is done.
    opening: Mutex<std::collections::HashSet<String>>,
    /// Tests: what every session started here runs instead of its harness (a program that isn't
    /// there, so no harness ever starts, whatever this machine has installed).
    #[cfg(test)]
    test_program: Mutex<Option<String>>,
}

/// The name a session's machine reports as its own: none when it's empty or its harness's name.
fn own_name<'a>(name: &'a str, kind_name: &str) -> Option<&'a str> {
    let n = name.trim();
    (!n.is_empty() && !n.eq_ignore_ascii_case(kind_name.trim())).then_some(n)
}

/// Whose name a session shows once its machine reports `reported` in a state.
#[derive(Debug, PartialEq, Eq)]
enum Naming<'a> {
    /// A name of its own the machine gives anew (not the `last` it gave): it's kept here too.
    Adopt(&'a str),
    /// The name kept here: the machine has none of its own (a runner from before names, or one
    /// upgraded since), or hasn't taken the one given here yet.
    Keep,
    /// The machine's: nothing is kept here.
    Theirs,
}

fn naming<'a>(reported: &'a str, last: &str, kind_name: &str, kept: bool) -> Naming<'a> {
    match own_name(reported, kind_name) {
        Some(own) if own != last => Naming::Adopt(own),
        _ if kept => Naming::Keep,
        _ => Naming::Theirs,
    }
}

async fn send_package(hub: &Hub, id: &str, to: &str, bytes: &[u8]) -> Result<(), String> {
    // In pieces that fit a command's frame, each answered before the next goes (docs/wire.md,
    // "Commands"): a move whose machine went away mid-way says so.
    const PIECE: usize = 768 << 10;
    let pieces: Vec<&[u8]> = bytes.chunks(PIECE).collect();
    let mut offset = 0u64;
    for (n, piece) in pieces.iter().enumerate() {
        use crate::wire::commands::Answer;
        let msg = ServerMsg::HarnessPackage { session: id.to_owned(), offset, data: piece.to_vec(), done: n + 1 == pieces.len() };
        match hub.command(to, &msg, crate::hub::COMMAND_WITHIN).await {
            Answer::Taken | Answer::Sent => {}
            Answer::Refused { why, .. } => return Err(format!("{} couldn't take the session: {why}", hub.shown(to))),
            Answer::Unreachable { .. } | Answer::Parked => return Err(format!("{} went away while the session was on its way", hub.shown(to))),
            Answer::Unanswered { why } => return Err(format!("{} didn't answer while the session was on its way: {why}", hub.shown(to))),
        }
        offset += piece.len() as u64;
    }
    Ok(())
}

/// A start was asked under an id that's a session's already, or another start's under way
/// ([`Registry::open_as`]).
#[derive(Debug)]
pub struct IdTaken(pub String);

impl std::fmt::Display for IdTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "an agent {} is there already, or starting", self.0)
    }
}

impl std::error::Error for IdTaken {}

/// The ids a start holds while it's under way ([`Registry::claim`]), let go of as it ends.
struct Claim<'a> {
    held: &'a Mutex<std::collections::HashSet<String>>,
    ids: Vec<String>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let mut held = self.held.lock();
        for id in &self.ids {
            held.remove(id);
        }
    }
}

impl Registry {
    /// A registry of tests: its store in `dir`, its feeds on the bus's simulator.
    #[cfg(test)]
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self::with(dir, &crate::config::Conversations::default(), crate::rt::clock(), Arc::new(kiln_seams::bus::SimBus::default()))
    }

    /// The registry, its store in `dir`, its mirrors as `conversations` says, the server's own
    /// sessions' host on `clock` (the hub's), its sessions' live feeds on `bus`.
    pub fn with(dir: std::path::PathBuf, conversations: &crate::config::Conversations, clock: Arc<dyn kiln_seams::Clock>, bus: Arc<dyn kiln_seams::Bus>) -> Self {
        let (tx, rx) = async_channel::bounded(8192);
        // Every session's conversation, on every machine, kept on disk as it comes in
        // (docs/conversations.md): the server's own sessions' too, as they pass through `ingest`.
        let convo = if conversations.off { None } else { crate::convo::open(&dir, conversations.upkeep()) };
        // No record goes before kiln's store of bytes has it: held back entirely until the archive
        // of transcripts (`super::archive`) says how far it has come.
        if let Some(c) = &convo {
            c.hold_prune(0);
        }
        // Runners that keep a store send it here; the server keeps their sessions from that. What
        // a mirror sends back to restore the store from isn't sent to it again.
        let echoes = std::sync::Arc::new(crate::convo::feed::Echoes::default());
        let (feed, feed_out) = match convo.as_ref().map(|st| crate::convo::feed::Intake::start(st.clone(), crate::convo::feed::Brings::TheirMachine, Some(echoes.clone()))) {
            Some(Ok((i, o))) => (Some(i), Some(o)),
            Some(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "runners' conversation stores won't reach the server's");
                (None, None)
            }
            None => (None, None),
        };
        let (copies, copies_out) = match convo.clone().filter(|_| conversations.mirrors > 0) {
            Some(st) => {
                let (m, out) = crate::convo::mirror::Mirrors::start(st, conversations.mirrors, conversations.mirror_on.clone(), dir.join("mirrors.json"), Some(echoes));
                (Some(m), Some(out))
            }
            None => (None, None),
        };
        let restore_auto = match (&copies, &convo) {
            (Some(m), Some(st)) if st.is_empty().unwrap_or(false) => m.chosen_machines(),
            _ => Vec::new(),
        };
        if !restore_auto.is_empty() {
            tracing::info!(mirrors = ?restore_auto, "the conversation store is empty: it is restored from a mirror's copy once one connects");
        }
        let local = Host::with_clock(tx, Some(dir.join("local.json")), Limits::default(), clock);
        // The server's own agents' workspaces of their own are kept beside their record.
        crate::workspace::slates::init(&dir);
        super::whose::note_kiln(&dir);
        let keeper = Keeper::new(&dir);
        let visuals = super::visuals::Visuals::new(&dir);
        Self {
            mirrors: Mutex::new(HashMap::new()),
            bus,
            uploads: Mutex::new(HashMap::new()),
            kinds: Mutex::new(HashMap::new()),
            local,
            convo,
            feed,
            feed_out,
            copies,
            copies_out,
            board: crate::convo::board::Board::default(),
            local_rx: rx,
            local_sock: Mutex::new(None),
            local_shells: Mutex::new(HashMap::new()),
            names: Mutex::new(std::fs::read(dir.join("names.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            keep_off: Mutex::new(std::fs::read(dir.join("keep-off.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            asked: super::asked::Asked::default(),
            holders: Arc::new(Mutex::new(std::fs::read(dir.join("holders.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())),
            holders_saving: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            holders_writing: Arc::new(Mutex::new(())),
            changes_to_store: Arc::new(Mutex::new(HashMap::new())),
            changes_storing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            epochs: Mutex::new(std::fs::read(dir.join("epochs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            proposed: Mutex::new(HashMap::new()),
            dir,
            scanner: Arc::new(Mutex::new(None)),
            restore: Mutex::new(Restore::Idle),
            restore_auto: Mutex::new(restore_auto),
            keeper,
            visuals,
            streams: super::streams::Relay::default(),
            webrtc: super::webrtc::Webrtc::default(),
            ports: super::opened::Tunnels::default(),
            opened: super::opened::Openings::default(),
            waiting: Mutex::new(HashMap::new()),
            opening: Mutex::new(std::collections::HashSet::new()),
            #[cfg(test)]
            test_program: Mutex::new(None),
        }
    }

    /// Tests: sessions started here from now on run `program` instead of their harness.
    #[cfg(test)]
    pub fn set_test_program(&self, program: &str) {
        *self.test_program.lock() = Some(program.to_owned());
    }

    /// Where restoring the conversation store from a mirror's copy stands.
    pub fn restore_view(&self) -> Restore {
        let mut r = self.restore.lock();
        if let Restore::Asked { from, at } = &*r
            && crate::util::now_ms() - at > RESTORE_ANSWER_MS
        {
            *r = Restore::Failed { from: from.clone(), error: "no answer: its kiln may be too old to send its copy back".into(), at: crate::util::now_ms() };
        }
        r.clone()
    }

    /// Ask `machine`, a mirror, for its copy of the conversation store, to restore this one from:
    /// what the copy brings fills in what the store lacks (`docs/conversations.md`, "Mirrors").
    pub fn restore_from(&self, hub: &Arc<Hub>, machine: &str) -> anyhow::Result<()> {
        let st = self.convo.as_ref().ok_or_else(|| anyhow::anyhow!("the conversation store is off"))?;
        anyhow::ensure!(self.feed.is_some(), "this server takes no conversation feeds");
        let peer = hub.instances.lock().newest_with(machine, crate::proto::CAP_CONVO).ok_or_else(|| anyhow::anyhow!("{} isn't connected, or keeps no conversation store", hub.shown(machine)))?;
        if matches!(self.restore_view(), Restore::Asked { .. } | Restore::Copying { .. }) {
            anyhow::bail!("a restore is already under way");
        }
        *self.restore.lock() = Restore::Asked { from: machine.to_owned(), at: crate::util::now_ms() };
        self.restore_auto.lock().clear();
        tracing::info!(%machine, "asked a mirror for its copy of the conversation store, to restore this one from");
        let json = crate::convo::feed::encode(&crate::convo::feed::Msg::Restore { store: st.id().to_owned() });
        let h = Arc::clone(hub);
        crate::rt::spawn(async move {
            if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data: Vec::new() }) {
                h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
            }
        });
        Ok(())
    }

    /// Mirror `machine` begins sending back its copy `store`: whether it was asked for.
    pub fn restore_accepts(&self, machine: &str, store: &str) -> bool {
        let mut r = self.restore.lock();
        match &*r {
            Restore::Asked { from, .. } if from == machine => {
                *r = Restore::Copying { from: machine.to_owned(), store: store.to_owned(), at: crate::util::now_ms() };
                true
            }
            // Said again (it asks again after a silence).
            Restore::Copying { from, store: s, .. } => from == machine && s == store,
            _ => false,
        }
    }

    /// Mirror `machine` has sent back its copy `store` whole (an empty `store`: it has none):
    /// the sessions it brought back are listed, away, on the machines they ran on.
    pub fn restore_done(&self, hub: &Arc<Hub>, machine: &str, store: &str) {
        let state = self.restore.lock().clone();
        match state {
            Restore::Asked { from, .. } if from == machine && store.is_empty() => {
                *self.restore.lock() = Restore::Failed { from, error: format!("{} keeps no copy of the conversation store", hub.shown(machine)), at: crate::util::now_ms() };
            }
            Restore::Copying { from, store: s, .. } if from == machine && s == store => {
                let (h, st) = (Arc::clone(hub), self.convo.clone());
                crate::rt::spawn(async move {
                    let ended = match st {
                        Some(st) => crate::rt::blocking(move || st.end_copy()).await.and_then(|r| r),
                        None => Err(anyhow::anyhow!("the conversation store is off")),
                    };
                    let at = crate::util::now_ms();
                    match ended {
                        Ok(sessions) => {
                            tracing::info!(machine = %from, sessions, "the conversation store was restored from a mirror's copy");
                            Self::list_away(&h).await;
                            *h.harness.restore.lock() = Restore::Done { from, sessions, at };
                        }
                        Err(e) => *h.harness.restore.lock() = Restore::Failed { from, error: format!("{e:#}"), at },
                    }
                });
            }
            _ => {}
        }
    }

    /// Runner `machine` keeping a store connected: asked for its copy when the store was found
    /// empty at start and it is one of the server's mirrors.
    pub fn restore_due(&self, hub: &Arc<Hub>, machine: &str) {
        let due = self.restore_auto.lock().iter().any(|m| m == machine);
        if due
            && matches!(self.restore_view(), Restore::Idle)
            && let Err(e) = self.restore_from(hub, machine)
        {
            tracing::warn!(%machine, error = %format!("{e:#}"), "couldn't ask a mirror for its copy of the conversation store");
        }
    }

    fn save_names(&self) {
        let names = self.names.lock().clone();
        if let Err(e) = self.keep_json("names.json", &names) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' names");
        }
    }

    /// `value` as `file` in the registry's folder, whole or not at all: written off the calling
    /// thread, soon after, in the order kept (goal 3, plan 01a1a59c2a8e step 5: `crate::keep`).
    /// Written here, renaming an agent or raising its epoch waited on the disk, on a shard.
    fn keep_json(&self, file: &str, value: &impl serde::Serialize) -> anyhow::Result<()> {
        crate::keep::later(self.dir.join(file), serde_json::to_vec(value)?);
        Ok(())
    }

    /// Whether kiln keeps a session going when it stops part-way (the "keep_going" control, from
    /// any device): kept here, so it holds across restarts whatever its machine's runner knows.
    fn set_keep_going(&self, hub: &Hub, id: &str, on: bool) {
        let off: Vec<String> = {
            let mut g = self.keep_off.lock();
            if on {
                g.remove(id);
            } else {
                g.insert(id.to_owned());
            }
            g.iter().cloned().collect()
        };
        if let Err(e) = self.keep_json("keep-off.json", &off) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep which agents kiln doesn't keep going");
        }
        let info = {
            let mut g = self.mirrors.lock();
            let Some(m) = g.get_mut(id) else { return };
            m.info.keep_off = !on;
            m.info.clone()
        };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// The plans sessions proposed (plan mode) since the last look, by session, each its latest.
    pub fn take_proposed(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.proposed.lock()).into_iter().collect()
    }

    /// What each agent's goals wait on a person for (`goals::held_stacks`, every tick of the goals'
    /// driver): each one that changes told to every viewer.
    pub fn set_held(&self, hub: &Hub, held: &HashMap<String, Held>) {
        let mut changed = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for (id, m) in g.iter_mut() {
                let now = held.get(id).cloned();
                if m.info.held != now {
                    m.info.held = now;
                    changed.push(m.info.clone());
                }
            }
        }
        for info in changed {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What of session `id`'s messages couldn't be read (`crate::wire::unread`) changed: told to its
    /// viewers.
    pub fn set_refused(&self, hub: &Hub, id: &str, refused: Option<crate::wire::unread::Kept>) {
        let info = {
            let mut g = self.mirrors.lock();
            match g.get_mut(id) {
                Some(m) if m.info.refused != refused => {
                    m.info.refused = refused;
                    Some(m.info.clone())
                }
                _ => None,
            }
        };
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// A session is given a name here (the "rename" control from any device; empty gives it its
    /// harness's name back): its page and every list show it at once.
    fn rename(&self, hub: &Hub, id: &str, value: Option<&Value>) {
        let name: String = value.and_then(Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
        {
            let mut g = self.names.lock();
            if name.is_empty() {
                g.remove(id);
            } else {
                g.insert(id.to_owned(), name.clone());
            }
        }
        self.save_names();
        let info = {
            let mut g = self.mirrors.lock();
            let Some(m) = g.get_mut(id) else { return };
            m.info.name = if name.is_empty() { m.info.kind_name.clone() } else { name };
            if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                o.insert("name".into(), json!(m.info.name));
                if let Some(frame) = m.state_frame() {
                    m.send(frame);
                }
            }
            m.info.clone()
        };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// Environment for one of the server's own terminals, so `kiln spawn` there works.
    pub fn terminal_env(&self, terminal: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![("KILN_TERM_SESSION", terminal.to_owned())];
        if let Some(sock) = self.local_sock.lock().clone() {
            env.push(("KILN_HARNESS_SOCK", sock));
        }
        env
    }

    /// One of the server's own terminals started (its shell runs as `pid`).
    pub fn note_shell(&self, terminal: &str, pid: Option<u32>) {
        let mut g = self.local_shells.lock();
        if let Some(pid) = pid {
            g.insert(terminal.to_owned(), pid);
        }
        // Forget shells that are gone.
        if g.len() > 256 {
            g.retain(|_, p| crate::os::pid_alive(*p));
        }
    }

    /// The server's own sessions' updates, into the mirrors (supervised; restartable).
    /// Sessions of machines that are away, as the server's store has them (the server started
    /// again since they went away): listed as away, so they can be moved on, until their machine
    /// says what it has.
    pub fn restore_away(&self, hub: &Hub, away: Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>) {
        // Each copy is made with no lock held: a session's stored events are megabytes of JSON to
        // read, and every session's updates wait on the mirrors' lock. Those already here when
        // they're put in are left as they are.
        let here: std::collections::HashSet<String> = self.mirrors.lock().keys().cloned().collect();
        let mut made = Vec::new();
        for (u, events) in away {
            if here.contains(&u.id) {
                continue;
            }
            let Ok(mut info) = serde_json::from_str::<Info>(&u.info) else { continue };
            // The machine whose feed brought it, else the one its updates came from.
            let Some(machine) = u.machine.clone().or_else(|| info.target.strip_prefix("agent:").map(str::to_owned)) else { continue };
            info.target = format!("agent:{machine}");
            if hub.agents.lock().get(&machine).is_some_and(|a| a.connected) {
                continue;
            }
            info.status = Status::Disconnected;
            let state: Value = u.state.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null);
            // Old stored list entries predate native subagent briefs. Their last task
            // snapshot still supplies those read-only children while the machine is away.
            if let Some(tasks) = state.get("tasks").and_then(|v| Vec::<super::model::Task>::deserialize(v).ok()) {
                info.subagents = SubagentBrief::of(&tasks);
            }
            let mut m = Mirror::new(info.clone(), Location::Machine(machine), Arc::clone(&self.bus));
            m.meta = Arc::new(u.meta.as_deref().and_then(|j| serde_json::from_str(j).ok()).unwrap_or(Value::Null));
            m.state = Arc::new(state);
            if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                o.insert("status".into(), json!("disconnected"));
            }
            // A conversation cleared since: what came before it is history, not its timeline.
            let cleared = m.state.get("cleared").and_then(Value::as_u64).unwrap_or(0);
            for e in events.into_iter().filter(|e| e.seq >= cleared) {
                if let Ok(v) = serde_json::from_str::<Value>(&e.body) {
                    m.upsert(Arc::new(Entry::new(v)));
                }
            }
            made.push((u.id, m, info));
        }
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for (id, m, info) in made {
                if let std::collections::hash_map::Entry::Vacant(slot) = g.entry(id) {
                    slot.insert(m);
                    infos.push(info);
                }
            }
        }
        if !infos.is_empty() {
            tracing::info!(sessions = infos.len(), "agent sessions on machines that are away, from the conversation store");
        }
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// Lists the sessions of machines that are away, from the store (the last month's): after a
    /// start, and after a restore from a mirror's copy.
    ///
    /// The server lists them as it starts, before it takes in any runner's messages: a runner that
    /// comes back finds its sessions' copies here, and its streams send only what they lack
    /// (`ServerMsg::Kept`, docs/wire.md "Restarts"). A session that changed within the last day
    /// is rebuilt with as many events as a live copy keeps, so the copy its machine goes on from
    /// is the session's; an older one with its last few, to show.
    pub async fn list_away(hub: &Arc<Hub>) {
        let Some(st) = hub.harness.convo.clone() else { return };
        const AWAY_DAYS: i64 = 30;
        const AWAY_EVENTS: usize = 200;
        const RECENT: i64 = 24 * 3600 * 1000;
        let now = crate::util::now_ms();
        let since = now - AWAY_DAYS * 24 * 3600 * 1000;
        let read = crate::rt::blocking(move || -> anyhow::Result<Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>> {
            let recent: std::collections::HashSet<String> = st.unended(now - RECENT)?.into_iter().map(|u| u.id).collect();
            let mut out = Vec::new();
            for u in st.unended(since)? {
                let keep = if recent.contains(&u.id) { MAX_EVENTS } else { AWAY_EVENTS };
                let mut events = st.events(&u.id, None, keep)?;
                // Cut at `keep`: from where a run of the digests begins, as a live copy lets go of
                // its oldest (`crate::wire::sums::run_after`).
                if events.len() >= keep
                    && let Some(first) = events.first().map(|e| e.seq)
                {
                    let from = crate::wire::sums::run_after(first);
                    events.retain(|e| e.seq >= from);
                }
                out.push((u, events));
            }
            Ok(out)
        })
        .await;
        match read {
            Ok(Ok(away)) => hub.harness.restore_away(hub, away),
            Ok(Err(e)) | Err(e) => tracing::warn!(error = %format!("{e:#}"), "sessions of machines that are away couldn't be read from the conversation store"),
        }
    }

    pub async fn run_local(hub: Arc<Hub>) -> anyhow::Result<()> {
        // Once a process, however often this loop starts again (it's supervised).
        static RESTORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        // Sessions of machines that are away: listed from the store.
        Self::list_away(&hub).await;
        let rx = hub.harness.local_rx.clone();
        let offered = crate::rt::blocking(super::kinds::offered).await.unwrap_or_default();
        hub.harness.kinds.lock().insert("local".to_owned(), offered);
        if !RESTORED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            // Files finished messages mention, kept as they come.
            crate::rt::spawn(Keeper::run(hub.clone()));
            // The server's own sessions back, on the blocking pool: their record read, what
            // outlived them stopped (seconds each), the conversation store asked what it kept of
            // each (up to 10 s): none of it on the runtime's thread, which serves the listener.
            let (local, convo) = (hub.harness.local.clone(), hub.harness.convo.clone());
            if let Err(e) = crate::rt::blocking(move || local.restore_from(convo.as_ref())).await {
                tracing::warn!(error = %format!("{e:#}"), "the server's own agent sessions couldn't be restored");
            }
            // The server's own sessions' harness records, followed into its store.
            if let Some(st) = &hub.harness.convo {
                let local = hub.harness.local.clone();
                if let Err(e) = crate::convo::record::Recorder::start(st.clone(), move || local.record_sources()) {
                    tracing::warn!(error = %format!("{e:#}"), "the harnesses' records won't be kept in the conversation store");
                }
            }
            // What the server sends its mirrors, on the feed's lane.
            if let Some(out) = hub.harness.copies_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok((peer, json, data)) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json, data }) {
                            h.net.send(peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server answers runners' feeds, on the feed's lane.
            if let Some(out) = hub.harness.feed_out.clone() {
                let h = hub.clone();
                crate::rt::spawn(async move {
                    while let Ok(o) = out.recv().await {
                        if let Ok(b) = crate::proto::encode(&crate::proto::ServerMsg::Convo { json: o.json, data: Vec::new() }) {
                            h.net.send(o.peer, crate::proto::LANE_CONVO, crate::net::Kind::Log, b).await;
                        }
                    }
                });
            }
            // What the server's own terminals and agents ask of kiln (`kiln spawn --on`, `kiln
            // agents wait`) is answered here.
            let forward = Some(crate::harness::ops::forward(Arc::clone(&hub)));
            match super::socket::serve_with(&hub.harness.local, super::socket::path("server", &hub.harness.dir), forward, false) {
                Ok(p) => *hub.harness.local_sock.lock() = Some(p.to_string_lossy().into_owned()),
                Err(e) => tracing::info!(error = %format!("{e:#}"), "`kiln spawn` will not be available in the server's own terminals"),
            }
        }
        // The server's own sessions' updates, as fast as they come: in bounded turns of the shard.
        let mut turns = crate::rt::Turns::new();
        loop {
            let Ok((id, u)) = rx.recv().await else { return Ok(()) };
            hub.harness.ingest(&hub, &Location::Local, &id, u);
            turns.after().await;
        }
    }

    /// Session `id` and its ancestors, nearest first ([`super::lineage::LINE_MAX`] at most), each its
    /// id and parent alone: what [`super::lineage::descends`] reads of it, without a copy of every
    /// session.
    pub fn lineage_of(&self, id: &str) -> Vec<Info> {
        let g = self.mirrors.lock();
        let mut out = Vec::new();
        let mut at = id;
        while out.len() < super::lineage::LINE_MAX {
            let Some(m) = g.get(at) else { break };
            out.push(Info { id: m.info.id.clone(), parent: m.info.parent.clone(), ..Info::default() });
            match m.info.parent.as_deref() {
                Some(p) => at = p,
                None => break,
            }
        }
        out
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = self.mirrors.lock().values().map(|m| m.info.clone()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    pub fn kinds(&self) -> HashMap<String, Vec<KindInfo>> {
        #[cfg(not(test))]
        super::acp::warm_models();
        let mut all = self.kinds.lock().clone();
        // The server's own harnesses: the models its sessions listed since it started.
        for k in all.get_mut("local").into_iter().flatten() {
            let m = super::kinds::models_of(&k.id);
            if !m.is_empty() {
                k.models = m;
            }
            if k.id == "devin" && let Some(kind) = super::kinds::Kind::get(&k.id) {
                k.efforts = kind.efforts();
            }
        }
        all
    }

    pub fn lacks(&self, target: &str, kind: &str) -> bool {
        #[cfg(test)]
        if self.test_program.lock().is_some() {
            return false;
        }
        self.kinds.lock().get(target).is_some_and(|ks| !ks.iter().any(|k| k.id == kind && k.available))
    }

    #[cfg(test)]
    pub fn set_kinds(&self, target: &str, kinds: Vec<KindInfo>) {
        self.kinds.lock().insert(target.to_owned(), kinds);
    }

    /// A session's meta as last reported (its harness's catalog: slash commands, models, modes).
    pub fn meta(&self, id: &str) -> Option<Value> {
        // Copied with the mirrors let go: the lock is held for a reference alone.
        let meta = self.mirrors.lock().get(id).map(|m| Arc::clone(&m.meta))?;
        Some(Value::clone(&meta))
    }

    pub fn location(&self, id: &str) -> Option<Location> {
        self.mirrors.lock().get(id).map(|m| m.location.clone())
    }

    /// A task's output, read on the session's machine (`None`: no such session).
    pub async fn task_output(&self, hub: &Hub, id: &str, task: &str, since: Option<u64>) -> anyhow::Result<Option<Value>> {
        match self.location(id) {
            None => Ok(None),
            Some(Location::Local) => {
                let Some(s) = self.local.get(id) else { return Ok(None) };
                let task = task.to_owned();
                Ok(Some(crate::rt::blocking(move || s.task_output(&task, since)).await?))
            }
            Some(Location::Machine(m)) => hub.harness_task_output(&m, id, task, since).await,
        }
    }

    /// A machine's past sessions, newest first, as JSON (`None`: that machine does not look, e.g.
    /// an ephemeral runner). The server's own skip what its sessions are running.
    pub async fn history(&self, hub: &Hub, target: &str, kind: Option<String>, limit: usize) -> anyhow::Result<Option<Value>> {
        match Location::of_target(target) {
            Location::Local => {
                let running: std::collections::HashSet<String> = self.local.list().into_iter().filter_map(|i| i.harness_session).collect();
                let scanner = Arc::clone(&self.scanner);
                let past = crate::rt::blocking(move || {
                    let mut g = scanner.lock();
                    g.get_or_insert_with(|| crate::aisessions::Scanner::new(crate::os::home_dir())).history(kind.as_deref(), limit, &running)
                })
                .await?;
                Ok(Some(serde_json::to_value(past)?))
            }
            Location::Machine(m) => hub.harness_history(&m, kind, u32::try_from(limit).unwrap_or(u32::MAX)).await?.map(serde_json::to_value).transpose().map_err(Into::into),
        }
    }

    /// Apply one update from a machine and pass it on to the session's viewers.
    pub fn ingest(&self, hub: &Hub, from: &Location, id: &str, u: Update) {
        self.ingest_from(hub, from, id, u, false);
    }

    /// Machine `machine` is where session `id` runs (or the server doesn't know the session yet):
    /// what it says of it counts.
    pub fn runs_on(&self, id: &str, machine: &str) -> bool {
        self.mirrors.lock().get(id).is_none_or(|m| matches!(&m.location, Location::Machine(at) if at == machine))
    }

    /// Session `id` is one kiln knows, and it runs on `machine`.
    pub fn located_on(&self, id: &str, machine: &str) -> bool {
        self.mirrors.lock().get(id).is_some_and(|m| matches!(&m.location, Location::Machine(at) if at == machine))
    }

    /// A copy of session `id` at `epoch` is from before it went on elsewhere: its machine lets go
    /// of it.
    pub fn stale(&self, id: &str, epoch: u64) -> bool {
        self.epochs.lock().get(id).is_some_and(|e| *e > epoch)
    }

    /// The sessions runner `machine` reports holding (`inv`) whose copy there is one left behind:
    /// the session runs elsewhere now, at a later epoch than that copy (docs/ci.md W27-R08). One
    /// just arriving there (its Open on its way) is at the epoch it goes on at, which isn't; a
    /// runner from before says no epochs, so none of its are.
    pub fn left_behind(&self, machine: &str, inv: &super::host::Inventory) -> Vec<String> {
        inv.sessions.iter().filter(|id| !self.runs_on(id, machine) && inv.epochs.get(*id).is_some_and(|e| self.stale(id, *e))).cloned().collect()
    }

    /// Session `id` is at `epoch` now (epochs only rise), kept when it changes.
    pub(crate) fn raise_epoch(&self, id: &str, epoch: u64) {
        let snapshot = {
            let mut g = self.epochs.lock();
            let e = g.entry(id.to_owned()).or_insert(0);
            if *e >= epoch {
                return;
            }
            *e = epoch;
            g.clone()
        };
        if let Err(e) = self.keep_json("epochs.json", &snapshot) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' epochs");
        }
    }

    /// An update of session `id` from `from`. `shipped`: its machine keeps it in a store it sends
    /// the server's (`crate::convo::feed`), which keeps it from that, not from here.
    pub fn ingest_from(&self, hub: &Hub, from: &Location, id: &str, u: Update, shipped: bool) {
        // What became of a message: for whoever waits for it, wherever it was taken or refused.
        let u = match u {
            Update::Inbox { said } => {
                self.answered(id, *said);
                return;
            }
            other => other,
        };
        let mut changed: Option<Info> = None;
        let mut gone = false;
        // The copy's events went another way than its viewers have them: they start over from it.
        let mut resync = false;
        // Finished messages' mentions of files, to keep once the mirrors are let go.
        let mut jobs: Vec<Job> = Vec::new();
        // The plan it proposed (plan mode) in these events, to keep as one of kiln's plans.
        let mut proposed: Option<String> = None;
        let mut save_names = false;
        // The account's other sessions, which pause too (`hold_account`).
        let mut hold: Option<(Command, Vec<(String, Location)>)> = None;
        // Its account, and the window reached and when that starts over.
        let mut hold_for: Option<(String, Option<String>, Option<i64>)> = None;
        // Where it runs, and the name given here, are the server's to say: to the mirror and to
        // viewers alike (a session announced again — a switch of harness, a reconnect — keeps its
        // machine and its name). The machine's own name is kept aside: it counts once it changes it.
        let mut reported = String::new();
        let u = match u {
            Update::Open { mut info, since } => {
                info.target = from.target();
                own_name(&info.name, &info.kind_name).unwrap_or_default().clone_into(&mut reported);
                if let Some(n) = self.names.lock().get(id) {
                    info.name.clone_from(n);
                }
                info.keep_off = self.keep_off.lock().contains(id);
                Update::Open { info, since }
            }
            other => other,
        };
        match &u {
            Update::Open { info, .. } if info.epoch > 0 => {
                // Gone on at a later epoch on a runner from before signed hand-overs: unsigned,
                // recorded saying so (docs/ci.md W27-R08).
                if let Location::Machine(machine) = from
                    && info.epoch > self.epoch_of(id)
                    && hub.runner_wide_caps(machine) & crate::proto::CAP_CUSTODY == 0
                {
                    let why = format!("the process it went to, on {}, runs a kiln from before signed hand-overs: it signed no receipt", hub.shown(machine));
                    unsigned(hub, id, info.epoch, &format!("runner {machine}"), why);
                }
                self.raise_epoch(id, info.epoch);
            }
            Update::Open { .. } => {}
            // Only the machine a session runs on speaks for it: a copy left behind where it went
            // on from says nothing more of it.
            _ => {
                if self.mirrors.lock().get(id).is_some_and(|m| m.location != *from) {
                    return;
                }
            }
        }
        // A changeset's patch is stored as the whole changeset it makes, once taken in (below).
        if let Some(store) = self.convo.as_ref().filter(|_| !shipped && !matches!(u, Update::ChangesPatch { .. })) {
            store.send(id, &u);
        }
        // What the copy takes, made before the mirrors are locked (every session's updates wait on
        // that lock): each event's JSON and its text, written out once for the copy, its live frame,
        // its digest and every snapshot after; a state's and a meta's JSON.
        let entries: Vec<Arc<Entry>> = match &u {
            Update::Events { events } => events
                .iter()
                .map(|e| {
                    let v = serde_json::to_value(e).unwrap_or(Value::Null);
                    let text = serde_json::to_string(&v).unwrap_or_default();
                    Arc::new(Entry::written(v, text))
                })
                .collect(),
            _ => Vec::new(),
        };
        let made: Option<Value> = match &u {
            Update::State { state } => Some(serde_json::to_value(state).unwrap_or(Value::Null)),
            Update::Meta { meta } => Some(serde_json::to_value(meta).unwrap_or(Value::Null)),
            _ => None,
        };
        // The whole changeset a patch makes, for the store once the mirrors are let go.
        let mut stored_changes: Option<Files> = None;
        {
            let mut g = self.mirrors.lock();
            if let Update::Open { info, since } = &u {
                // A runner that started again goes on from what this copy has of the same epoch:
                // its events through `since` stay; what's after them comes again.
                let keep = since.filter(|_| g.get(id).is_some_and(|m| m.info.epoch == info.epoch));
                if let Location::Machine(machine) = from {
                    match keep {
                        Some(n) => tracing::info!(session = id, %machine, after = n, "an agent session's copy goes on from its machine's Open after event"),
                        None => tracing::info!(session = id, %machine, "an agent session's copy starts from its machine's Open"),
                    }
                }
                let mut info = (**info).clone();
                // What its goals wait on a person for is the server's to say (`goals::held_stacks`): kept.
                info.held = g.get(id).and_then(|m| m.info.held.clone());
                // So is what of its messages couldn't be read (`crate::wire::unread`).
                info.refused = g.get(id).and_then(|m| m.info.refused.clone());
                // So is whether kiln started it as its parent's subagent (`Spec::subagent`): a runner
                // from before the word doesn't say it, and it stays one.
                info.subagent |= g.get(id).is_some_and(|m| m.info.subagent);
                // What kiln's own words were handed is kept too, and what waits in its queue until
                // the state after this says again (neither goes in an Open): a runner that comes
                // back doesn't make a word handed look lost (CP-T3.2, the fourteenth adversarial check's).
                if let Some(old) = g.get(id) {
                    info.said.clone_from(&old.info.said);
                    info.queue.clone_from(&old.info.queue);
                }
                let m = g.entry(id.to_owned()).or_insert_with(|| Mirror::new(info.clone(), from.clone(), Arc::clone(&self.bus)));
                // Its machine announced it: what it reports from now on counts.
                m.opened_at = None;
                m.info = info.clone();
                m.reported = reported;
                m.location = from.clone();
                match keep {
                    Some(n) => {
                        let had = m.events.len();
                        m.events.retain(|e| e.get("seq").and_then(Value::as_u64).is_some_and(|q| q <= n));
                        m.reindex();
                        // Past its machine's newest it had what the machine no longer has: its
                        // viewers let go of that too.
                        resync = m.events.len() != had;
                    }
                    None => {
                        m.clear_events();
                        m.started_over_pending = true;
                    }
                }
                m.changes = None;
                m.changes_sent = None;
                changed = Some(info);
            }
            let Some(m) = g.get_mut(id) else {
                drop(g);
                // A session this server does not know yet (it restarted): ask for all of it.
                if let Location::Machine(machine) = from
                    && !matches!(u, Update::Closed)
                {
                    self.request_snapshot(hub, machine, id);
                }
                return;
            };
            m.rev = m.rev.saturating_add(1);
            match &u {
                // A whole snapshot follows (not one that goes on from what the copy has): its
                // viewers from before start over too.
                Update::Open { .. } => {
                    if std::mem::take(&mut m.started_over_pending) {
                        m.started_over();
                    }
                }
                // An inbox's answer went to whoever waits for it, before any of this.
                Update::Reply { .. } | Update::Inbox { .. } => {}
                Update::Closed => {
                    gone = true;
                    // Its identities with it (docs/ci.md W27-R01).
                    let (store, id) = (hub.store.clone(), id.to_owned());
                    drop(crate::rt::spawn(async move {
                        if let Err(e) = crate::identities::agent_ended(&store, &id).await {
                            tracing::warn!(session = %id, error = %format!("{e:#}"), "an ended agent's identities weren't revoked");
                        }
                    }));
                }
                Update::Reset { .. } => {
                    m.clear_events();
                    m.started_over();
                }
                Update::State { state } => {
                    m.state = Arc::new(made.unwrap_or(Value::Null));
                    let before = m.info.clone();
                    m.info.apply(state);
                    m.info.last_activity = crate::util::now_ms();
                    // Its name: one of its own the machine gives anew; else the one given here.
                    let mut names = self.names.lock();
                    match naming(&state.name, &m.reported, &m.info.kind_name, names.contains_key(id)) {
                        Naming::Adopt(own) => {
                            m.reported = own.to_owned();
                            names.insert(id.to_owned(), own.to_owned());
                            save_names = true;
                        }
                        Naming::Keep => {
                            if let Some(n) = names.get(id) {
                                m.info.name.clone_from(n);
                                // The state is the copy's own here (just made): nothing is copied.
                                if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                                    o.insert("name".into(), json!(n));
                                }
                            }
                        }
                        Naming::Theirs => {}
                    }
                    drop(names);
                    if m.info.differs(&before) {
                        changed = Some(m.info.clone());
                    }
                    if before.pause.is_none()
                        && let Some(p) = m.info.pause.as_ref().filter(|p| p.reason == super::pause::LIMIT && p.by == "harness")
                        && let Some(account) = m.info.account.clone()
                    {
                        let c = Command::LimitHold { window: p.window.clone(), label: p.label.clone(), until: p.until, said: p.said.clone(), account: Some(account.clone()) };
                        hold = Some((c, Vec::new()));
                        // Collected below, once this mirror is let go.
                        hold_for = Some((account, p.window.clone(), p.until));
                    }
                }
                Update::Meta { .. } => m.meta = Arc::new(made.unwrap_or(Value::Null)),
                Update::Changes { files } => m.take_changes(files.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), Vec::new(), true),
                Update::ChangesPatch { set, gone } => {
                    m.take_changes(set.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), gone.iter().filter_map(|k| serde_json::to_value(k).ok()).collect(), false);
                    if self.convo.is_some() && !shipped {
                        stored_changes.clone_from(&m.changes);
                    }
                }
                Update::Events { events } => {
                    for (e, entry) in events.iter().zip(&entries) {
                        if let Some(job) = mentions(id, &m.info.cwd, &m.location, e) {
                            jobs.push(job);
                        }
                        // kiln's own words handed to its harness, with what they said then: what a
                        // goal stack's word or a parent's news held when its agent took it.
                        if let super::model::Body::User { text, client_id: Some(c), .. } = &e.body
                            && super::queued::own(c)
                        {
                            super::queued::note_given(&mut m.info.said, c, super::queued::stable(text));
                        }
                        if let Some(plan) = proposed_in(e) {
                            proposed = Some(plan);
                        }
                        if !entry.is_null() {
                            resync |= m.upsert(Arc::clone(entry));
                        }
                    }
                }
                Update::Stream { id, text } => m.append(id, text),
            }
            // A state goes out with the name given here in it (the machine's may lack it). An Open
            // that goes on from what the copy has isn't one to its viewers: they go on with what
            // they have, unless the copy's events went another way than theirs.
            let frame = match &u {
                _ if resync => {
                    m.resync();
                    None
                }
                Update::Open { since: Some(_), .. } => None,
                Update::State { .. } => m.state_frame(),
                Update::Changes { .. } | Update::ChangesPatch { .. } => m.changes_frame(),
                _ if m.feed.viewers() > 0 => Some(m.frame_of(&u, &entries)),
                _ => None,
            };
            if let Some(frame) = frame {
                m.send(frame);
            }
            if gone {
                g.remove(id);
                save_names = self.names.lock().remove(id).is_some() || save_names;
                self.changes_to_store.lock().remove(id);
            }
            if let (Some((account, window, until)), Some((_, targets))) = (hold_for.as_ref(), hold.as_mut()) {
                let now = crate::util::now_ms();
                *targets = g
                    .iter()
                    .filter(|(other, o)| other.as_str() != id && o.info.account.as_deref() == Some(account.as_str()) && o.info.pause.is_none() && o.info.status != Status::Exited)
                    // Not one whose own limits show the limit isn't its own: another account's
                    // (an account id gone stale), another model's.
                    .filter(|(_, o)| super::pause::shares(&o.info.limits, &o.info.model, window.as_deref(), *until, now) != Some(false))
                    .map(|(other, o)| (other.clone(), o.location.clone()))
                    .collect();
            }
        }
        // The whole changeset a patch made, kept in the store as it is when that write goes.
        if let (Some(files), Some(store)) = (stored_changes, self.convo.as_ref()) {
            self.store_changes(id, files, store.clone());
        }
        if let Some((c, targets)) = hold {
            self.hold_account(hub, &c, targets);
        }
        for job in jobs {
            self.keeper.enqueue(job);
        }
        if let Some(plan) = proposed {
            self.proposed.lock().insert(id.to_owned(), plan);
        }
        if save_names {
            self.save_names();
        }
        if gone {
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.forget_holders(&[id.to_owned()]);
            self.opened.forget(id);
            self.asked.forget(id);
            super::archive::closed(hub, id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        } else if let Some(info) = changed {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// A session reached its account's usage limit: the account's other sessions, on every machine,
    /// pause until it resets too, rather than each finding out by failing; each one whose own usage
    /// says the limit is its own as well (`pause::account_hold`).
    fn hold_account(&self, hub: &Hub, c: &Command, targets: Vec<(String, Location)>) {
        let Ok(json) = serde_json::to_string(c) else { return };
        for (id, loc) in targets {
            match loc {
                Location::Local => {
                    self.local.command(&id, c.clone());
                }
                Location::Machine(machine) => {
                    hub.command_soon(&machine, &ServerMsg::HarnessCmd { session: id, json: json.clone() }).log_when_answered(crate::hub::LOG_WITHIN);
                }
            }
        }
    }

    fn request_snapshot(&self, hub: &Hub, machine: &str, id: &str) {
        let msg = ServerMsg::HarnessCmd { session: id.to_owned(), json: json!({"type": "snapshot"}).to_string() };
        hub.command_soon(machine, &msg).log_when_answered(crate::hub::LOG_WITHIN);
    }

    /// A machine (re)connected and said what it runs: forget sessions it no longer has. Its
    /// runner process `instance` speaks for the sessions it holds: one another process of the
    /// machine holds ([`Registry::holder`]) stays, and while that process isn't among those
    /// connected (`present`), it is said to be away, as a machine's sessions are when it is.
    pub fn inventory(&self, hub: &Hub, machine: &str, instance: Option<&str>, present: &[String], inv: Inventory) {
        let keep: std::collections::HashSet<String> = inv.sessions.into_iter().collect();
        let (others, away): (std::collections::HashSet<String>, Vec<String>) = match instance {
            Some(me) => {
                let g = self.holders.lock();
                let others: std::collections::HashSet<String> = g.iter().filter(|(_, h)| h.as_str() != me).map(|(id, _)| id.clone()).collect();
                let away = g.iter().filter(|(id, h)| h.as_str() != me && !present.contains(h) && !keep.contains(*id)).map(|(id, _)| id.clone()).collect();
                (others, away)
            }
            None => Default::default(),
        };
        let at = Location::Machine(machine.to_owned());
        let away: Vec<String> = {
            let g = self.mirrors.lock();
            away.into_iter().filter(|id| g.get(id).is_some_and(|m| m.location == at && m.info.status != Status::Disconnected)).collect()
        };
        if !away.is_empty() {
            tracing::info!(%machine, sessions = away.len(), "agent sessions another runner process of this machine holds, which isn't connected: kept, away");
            self.sessions_lost(hub, &away);
        }
        let gone: Vec<String> = {
            let mut g = self.mirrors.lock();
            // Not one this server opened there that the machine hasn't announced yet: the report
            // was made before the open got there.
            let gone: Vec<String> = g
                .iter()
                .filter(|(id, m)| {
                    m.location == Location::Machine(machine.to_owned()) && !keep.contains(*id) && !others.contains(*id) && m.opened_at.is_none_or(|t| t.elapsed() > OPEN_GRACE)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in &gone {
                if let Some(mut m) = g.remove(id) {
                    // Its feed's topic goes with it once its viewers are told.
                    m.feed.publish(&Frame::of(crate::wire::ui::SessionFrame::Closed.into()));
                }
            }
            gone
        };
        self.kinds.lock().insert(format!("agent:{machine}"), inv.kinds);
        self.forget_holders(&gone);
        for id in gone {
            super::archive::closed(hub, &id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
    }

    /// Of the sessions a runner of `machine` just said it holds (`held`), those this server still
    /// has as disconnected. Its link dropped and came back, and only a session's own update clears
    /// that, so one that's quiet (idle, ended, waiting on a person) stays offline while its machine
    /// is connected. Their snapshots are asked for ([`Command::Snapshot`]).
    pub fn still_disconnected(&self, machine: &str, held: &[String]) -> Vec<String> {
        let at = Location::Machine(machine.to_owned());
        let g = self.mirrors.lock();
        held.iter().filter(|id| g.get(*id).is_some_and(|m| m.location == at && m.info.status == Status::Disconnected)).cloned().collect()
    }

    /// Session `id` is held by runner process `instance` of its machine, as that process said:
    /// kept (across restarts too) once it changes.
    pub fn held_by(&self, id: &str, instance: &str) {
        self.held_by_all(std::slice::from_ref(&id.to_owned()), instance);
    }

    /// [`Registry::held_by`] for each of `ids`: kept once for them all (a runner coming back says
    /// what it holds of every session at once).
    pub fn held_by_all(&self, ids: &[String], instance: &str) {
        let changed = {
            let mut g = self.holders.lock();
            let mut changed = false;
            for id in ids {
                if g.get(id).is_none_or(|h| h != instance) {
                    g.insert(id.clone(), instance.to_owned());
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.save_holders();
        }
    }

    /// The runner process of its machine that last said it holds session `id`, if one did.
    pub fn holder(&self, id: &str) -> Option<String> {
        self.holders.lock().get(id).cloned()
    }

    fn forget_holders(&self, ids: &[String]) {
        let mut changed = false;
        {
            let mut g = self.holders.lock();
            for id in ids {
                changed |= g.remove(id).is_some();
            }
        }
        if changed {
            self.save_holders();
        }
    }

    /// Keeps `holders.json`, on the blocking pool and as it is when the write goes: those that
    /// change while one is on its way go with it, or with the next (one a change starts once that
    /// one began). A runner coming back with every session it holds writes it once, never on the
    /// loop that takes its messages.
    fn save_holders(&self) {
        use std::sync::atomic::Ordering;
        if self.holders_saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let (holders, saving, writing, dir) = (Arc::clone(&self.holders), Arc::clone(&self.holders_saving), Arc::clone(&self.holders_writing), self.dir.clone());
        let write = move || {
            // One at a time, each taking what's newest once it's its turn.
            let _one = writing.lock();
            // Let go first: a change after the copy below starts another write (after this one).
            saving.store(false, Ordering::Release);
            let now = holders.lock().clone();
            if let Err(e) = keep_json_in(&dir, "holders.json", &now) {
                tracing::warn!(error = %format!("{e:#}"), "couldn't keep which runner process holds each agent session");
            }
        };
        drop(crate::rt::spawn(async move {
            let _ = crate::rt::blocking(write).await;
        }));
    }

    /// Keeps session `id`'s changeset `files` in the conversation store, written out on the blocking
    /// pool as it is when its turn comes. One writer at a time, at the pace the store commits: it
    /// writes each session's newest, waits for the store to have it, then takes what came
    /// meanwhile, a patch that comes while it works taking the place of the one before (each is the
    /// changeset whole). A burst of patches (a runner sending what it held) writes a session's
    /// changeset as often as the store takes it, not once a patch (most of a megabyte each).
    fn store_changes(&self, id: &str, files: Files, store: crate::convo::store::Store) {
        use std::sync::atomic::Ordering;
        self.changes_to_store.lock().insert(id.to_owned(), files);
        if self.changes_storing.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, storing) = (Arc::clone(&self.changes_to_store), Arc::clone(&self.changes_storing));
        let write = move || {
            loop {
                let batch = std::mem::take(&mut *pending.lock());
                if batch.is_empty() {
                    storing.store(false, Ordering::Release);
                    // One that came in between finds no writer at work: this one goes on with it,
                    // unless it started another.
                    if pending.lock().is_empty() || storing.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    continue;
                }
                for (session, files) in batch {
                    // Its files are their texts, written out once.
                    let changes = Some(write::string(write::texts_len(files.iter().map(|f| &**f)), |b| {
                        b.push(b'[');
                        for (i, f) in files.iter().enumerate() {
                            if i > 0 {
                                b.push(b',');
                            }
                            b.extend_from_slice(f.text().as_bytes());
                        }
                        b.push(b']');
                        Ok(())
                    }));
                    store.put(vec![crate::convo::rows::Row::Session { session, info: None, meta: None, state: None, changes, epoch: None, open: false, at: crate::util::now_ms() }]);
                }
                if let Err(e) = store.sync() {
                    tracing::debug!(error = %format!("{e:#}"), "the conversation store didn't say it had the changesets");
                }
            }
        };
        drop(crate::rt::spawn(async move {
            let _ = crate::rt::blocking(write).await;
        }));
    }

    /// Machine `from` is known as `to` from now on (docs/machines.md): its sessions are there (and
    /// kept so in the conversation store), and what it offers is its. Its runner, coming back as
    /// `to`, finds them its own.
    pub fn move_machine(&self, hub: &Hub, from: &str, to: &str) {
        let (at, now) = (Location::Machine(from.to_owned()), Location::Machine(to.to_owned()));
        let moved: Vec<Info> = {
            let mut g = self.mirrors.lock();
            g.values_mut()
                .filter(|m| m.location == at)
                .map(|m| {
                    m.location = now.clone();
                    m.info.target = now.target();
                    m.info.clone()
                })
                .collect()
        };
        {
            let mut k = self.kinds.lock();
            if let Some(v) = k.remove(&at.target()) {
                k.entry(now.target()).or_insert(v);
            }
        }
        if let Some(st) = &self.convo {
            st.put(moved.iter().map(|i| crate::convo::rows::Row::Machine { session: i.id.clone(), machine: to.to_owned() }).collect());
        }
        for info in moved {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// The machine went away: its sessions keep running there and come back when it reconnects.
    pub fn machine_lost(&self, hub: &Hub, machine: &str) {
        self.streams.machine_lost(machine, &hub.shown(machine));
        self.ports.machine_lost(machine);
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for m in g.values_mut().filter(|m| m.location == Location::Machine(machine.to_owned())) {
                m.info.status = Status::Disconnected;
                if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                if let Some(frame) = m.state_frame() {
                    m.send(frame);
                }
                infos.push(m.info.clone());
            }
        }
        self.kinds.lock().remove(&format!("agent:{machine}"));
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// One runner process of a machine went away while another of the machine stays connected
    /// (see `crate::instances`): the sessions it held keep running there and come back with it.
    pub fn sessions_lost(&self, hub: &Hub, ids: &[String]) {
        let mut infos = Vec::new();
        {
            let mut g = self.mirrors.lock();
            for id in ids {
                let Some(m) = g.get_mut(id) else { continue };
                m.info.status = Status::Disconnected;
                if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                    o.insert("status".into(), json!("disconnected"));
                }
                if let Some(frame) = m.state_frame() {
                    m.send(frame);
                }
                infos.push(m.info.clone());
            }
        }
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What this server's copy of each session on `machine` has, said in digests: what a stream
    /// it starts sends only what the copy lacks of (`ServerMsg::Kept`, `crate::wire::sums`). One
    /// copy at a time, so taking in updates waits for no more than one.
    pub fn kept_on(&self, machine: &str) -> Vec<crate::proto::KeptSession> {
        let at = Location::Machine(machine.to_owned());
        let ids: Vec<String> = self.mirrors.lock().iter().filter(|(_, m)| m.location == at).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| self.mirrors.lock().get_mut(&id).filter(|m| m.location == at).map(|m| m.kept(&id))).collect()
    }

    /// Join a session's stream, as a viewer that takes every frame whole. None when there is no
    /// such session.
    pub fn attach(&self, id: &str) -> Option<Attachment> {
        self.attach_from(id, None, Patches::default())
    }

    /// Join a session's stream as a viewer that had it through revision `after` (`(born, rev)`),
    /// and takes the patches `takes` says: what it lacks if the copy can resume it (the same
    /// process, nothing started over since), else all of it. None when there is no such session.
    pub fn attach_from(&self, id: &str, after: Option<(u64, u64)>, takes: Patches) -> Option<Attachment> {
        // What's kept is read before the mirrors are locked: the first time, off disk.
        let files = self.keeper.frame(id);
        let (parts, topic, at, viewer, refusals) = {
            let mut g = self.mirrors.lock();
            let m = g.get_mut(id)?;
            // The offset under the same lock as the parts: every publish takes it too.
            let (viewer, at) = m.feed.attach(takes);
            let parts = after.filter(|(b, _)| *b == born()).and_then(|(_, rev)| m.parts_since(rev)).unwrap_or_else(|| m.parts());
            (parts, m.feed.topic().to_owned(), at, viewer, m.feed.refusals())
        };
        let rev = parts.rev;
        // Written out with the lock let go, and by the caller off the runtime's threads: no other
        // session's publish waits on this one, nor any socket on its thread.
        Some(Attachment { snapshot: Snapshot { parts, files }, topic, at, viewer, refusals, rev })
    }

    /// Catch-up for a viewer that fell behind having had revision `rev`: what it missed (it begins
    /// with `resumed`), or all of it when that can't be told apart (`open`); where on the
    /// session's topic it reads on from, taken under the same lock, so it reads nothing twice; the
    /// feed's refusals then; and the copy's revision they bring it to. None when the session is gone.
    pub fn frames_since(&self, id: &str, rev: u64) -> Option<(Snapshot, Option<kiln_seams::bus::Offset>, u64, u64)> {
        let files = self.keeper.frame(id);
        let (parts, at, refusals) = self.mirrors.lock().get_mut(id).map(|m| (m.parts_since(rev).unwrap_or_else(|| m.parts()), m.feed.position(), m.feed.refusals()))?;
        let now = parts.rev;
        Some((Snapshot { parts, files }, at, refusals, now))
    }

    /// Files a finished message mentioned, kept now (`super::kept`): its viewers hear of them.
    pub fn files_added(&self, id: &str, added: &[Kept]) {
        if let Some(m) = self.mirrors.lock().get_mut(id) {
            // A revision of its own, past what its viewers have: else they'd take it as had.
            m.rev = m.rev.saturating_add(1);
            let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::Files { files: added.to_vec() }, rev: Some(m.rev) }.text();
            let mut p = super::live::Payload::new(m.rev, text.len());
            p.text().extend_from_slice(text.as_bytes());
            p.done();
            m.send(p);
        }
    }

    /// Where a session runs, when one of its messages mentions `path` (a relative mention, in its
    /// folder): a viewer may read that file from its machine, and no other.
    pub fn mentioned_where(&self, id: &str, path: &str) -> Option<Location> {
        let listed = self.keeper.lists_path(id, path);
        let g = self.mirrors.lock();
        let m = g.get(id)?;
        let cwd = &m.info.cwd;
        let said = || {
            m.events
                .iter()
                .rev()
                .filter(|e| matches!(e.get("kind").and_then(Value::as_str), Some("user" | "assistant")))
                .filter_map(|e| e.get("text").and_then(Value::as_str))
                .any(|t| files::mentioned(t).iter().any(|p| files::resolve(p, cwd).as_deref() == Some(path)))
        };
        (listed || said()).then(|| m.location.clone())
    }

    /// Start sessions on `target`. Sessions on machines are named here and appear at once
    /// (starting); the machine fills them in.
    pub async fn open(&self, hub: &Hub, target: &str, spec: Spec, count: usize) -> anyhow::Result<Vec<Info>> {
        self.open_as(hub, target, spec, count, &[]).await
    }

    /// [`Registry::open`], the sessions under `ids` in order (made by the client that asks, so its
    /// page can open a session's own page before it has started, W21-R15; those it names no id
    /// for get one made here). One that's a session's already, or a start's under way, is refused
    /// ([`IdTaken`]) before anything starts: asked again under the same ids, as its answer was
    /// lost, a start never starts its sessions twice.
    pub async fn open_as(&self, hub: &Hub, target: &str, spec: Spec, count: usize, ids: &[String]) -> anyhow::Result<Vec<Info>> {
        let count = count.clamp(1, 16);
        let loc = Location::of_target(target);
        // A machine whose kiln is to be upgraded takes no new agent (`crate::skew`).
        if let Location::Machine(m) = &loc
            && let Some(why) = hub.skew_holds(m)
        {
            anyhow::bail!("{why}");
        }
        let _held = self.claim(ids)?;
        let mut out = Vec::new();
        for i in 0..count {
            let mut spec = spec.clone();
            #[cfg(test)]
            if spec.program.is_none() {
                spec.program.clone_from(&self.test_program.lock());
            }
            if count > 1 {
                spec.name = Some(format!("{} {}", spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)), i + 1));
            }
            match &loc {
                Location::Local => {
                    // Started on one of the server's own shells: in that shell's directory.
                    if let Some(pid) = spec.shell.as_ref().and_then(|t| self.local_shells.lock().get(t).copied()) {
                        let (cwd, env) = crate::rt::blocking(move || super::host::shell_context(pid)).await.unwrap_or((None, Vec::new()));
                        if spec.cwd.is_none() {
                            spec.cwd = cwd;
                        }
                        if spec.env.is_empty() {
                            spec.env = env;
                        }
                    }
                    // A workspace of its own first: a volume over its folder, mounted, its folder in it.
                    let id = ids.get(i).cloned().unwrap_or_else(crate::util::new_id);
                    crate::workspace::prepare(&mut spec, &id).await.map_err(|e| e.context("its workspace"))?;
                    let made = spec.workspace.as_ref().and_then(|w| w.made.clone());
                    let s = match self.local.open(Some(id), spec) {
                        Ok(s) => s,
                        Err(e) => {
                            // What was made for it goes with it: asked again under its id, it's made anew.
                            if let Some(m) = made
                                && let Err(gone) = crate::workspace::slates::remove(&m).await
                            {
                                tracing::warn!(volume = %m.volume, error = %format!("{gone:#}"), "the workspace of an agent that couldn't start stayed");
                            }
                            return Err(e);
                        }
                    };
                    let mut info = s.info();
                    info.target = loc.target();
                    // Seed the mirror so the session lists before its first update arrives.
                    self.mirrors.lock().entry(info.id.clone()).or_insert_with(|| Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus)));
                    out.push(info);
                }
                Location::Machine(machine) => {
                    let id = ids.get(i).cloned().unwrap_or_else(crate::util::new_id);
                    let json = serde_json::to_string(&spec)?;
                    // Its runner answers once it started it (docs/wire.md, "Commands").
                    use crate::wire::commands::Answer;
                    match hub.command(machine, &ServerMsg::HarnessOpen { session: id.clone(), json }, crate::hub::COMMAND_WITHIN).await {
                        Answer::Taken | Answer::Sent | Answer::Parked => {}
                        Answer::Refused { why, .. } => anyhow::bail!("{} refused to start it: {why}", hub.shown(machine)),
                        Answer::Unreachable { .. } => anyhow::bail!("machine {} is not connected", hub.shown(machine)),
                        Answer::Unanswered { why } => anyhow::bail!("{} may or may not have started it: {why}", hub.shown(machine)),
                    }
                    let now = crate::util::now_ms();
                    let kind = super::kinds::Kind::get(&spec.kind);
                    let info = Info {
                        id: id.clone(),
                        kind: spec.kind.clone(),
                        kind_name: kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned()),
                        target: loc.target(),
                        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name(&spec.kind)),
                        shell: spec.shell.clone(),
                        cwd: spec.cwd.clone().unwrap_or_default(),
                        tree: None,
                        created_at: now,
                        status: Status::Starting,
                        title: String::new(),
                        model: spec.model.clone().unwrap_or_default(),
                        mode: spec.mode.clone().unwrap_or_default(),
                        harness_session: spec.resume.clone(),
                        approvals: Vec::new(),
                        queued: 0,
                        queue: Vec::new(),
                        said: Vec::new(),
                        tasks_running: 0,
                        subagents: Vec::new(),
                        cost_usd: 0.0,
                        activity: None,
                        last_activity: now,
                        exit: None,
                        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
                        objective: None,
                        summary: String::new(),
                        doing: None,
                        epoch: 0,
                        todos_done: 0,
                        todos_total: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        context_pct: None,
                        turn_started: None,
                        limits: Vec::new(),
                        account: None,
                        plan: None,
                        left_behind: 0,
                        reach: spec.reach.clone(),
                        pause: None,
                        frozen: None,
                        perms: None,
                        room_wait: None,
                        last_turn: None,
                        keep_off: false,
                        held: None,
                        refused: None,
                        move_on_limit: false,
                        move_plan: Default::default(),
                        pause_at: None,
                        max_subagents: None,
                        parent: spec.parent.clone(),
                        plan_run: spec.plan_run.clone(),
                        helper: spec.helper.clone(),
                        subagent: spec.subagent,
                        variant_of: spec.variant_of.clone(),
                        no_bypass: !spec.allow_bypass,
                        workspace: spec.workspace.clone(),
                    };
                    self.mirrors.lock().entry(id).or_insert_with(|| Mirror { opened_at: Some(std::time::Instant::now()), ..Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus)) });
                    out.push(info);
                }
            }
        }
        for info in &out {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info.clone()) });
        }
        Ok(out)
    }

    /// Holds `ids` for a start ([`Registry::open_as`]) until what's returned goes: none of them may
    /// be a session's already, or held by another start.
    fn claim(&self, ids: &[String]) -> anyhow::Result<Claim<'_>> {
        if ids.is_empty() {
            return Ok(Claim { held: &self.opening, ids: Vec::new() });
        }
        let mirrors = self.mirrors.lock();
        let mut held = self.opening.lock();
        if let Some(id) = ids.iter().find(|id| mirrors.contains_key(id.as_str()) || held.contains(id.as_str())) {
            return Err(anyhow::Error::new(IdTaken(id.clone())));
        }
        held.extend(ids.iter().cloned());
        Ok(Claim { held: &self.opening, ids: ids.to_vec() })
    }

    /// Keep an uploaded file for the messages that refer to it (by the returned id): until it
    /// expires, also after a message carried it (one sent again carries it too). Short of room, the
    /// files messages carried already go first, the oldest first.
    pub fn upload(&self, name: String, mime: String, data: bytes::Bytes) -> anyhow::Result<String> {
        let mut g = self.uploads.lock();
        g.retain(|_, u| u.at.elapsed() < UPLOAD_TTL);
        let mut used: usize = g.values().map(|u| u.data.len()).sum();
        if used.saturating_add(data.len()) > UPLOADS_MAX {
            let mut sent: Vec<(std::time::Instant, String)> = g.iter().filter_map(|(id, u)| u.sent.map(|t| (t, id.clone()))).collect();
            sent.sort();
            for (_, id) in sent {
                if used.saturating_add(data.len()) <= UPLOADS_MAX {
                    break;
                }
                if let Some(u) = g.remove(&id) {
                    used = used.saturating_sub(u.data.len());
                }
            }
        }
        if used.saturating_add(data.len()) > UPLOADS_MAX {
            anyhow::bail!("too many uploads waiting; send them first");
        }
        let id = crate::util::new_id();
        g.insert(id.clone(), Upload { name, mime, data, at: std::time::Instant::now(), sent: None });
        Ok(id)
    }

    /// Upload `id`'s name, type and bytes, for a message that carries it: kept (marked sent), so a
    /// message sent again carries it as well.
    fn upload_of(&self, id: &str) -> Option<(String, String, bytes::Bytes)> {
        let mut g = self.uploads.lock();
        let u = g.get_mut(id)?;
        u.sent.get_or_insert_with(std::time::Instant::now);
        Some((u.name.clone(), u.mime.clone(), u.data.clone()))
    }

    /// Attachments that refer to uploads, with the uploads' bytes in them instead: what one
    /// message to many agents carries to each. One whose upload is gone (expired, let go for room,
    /// or the server started again) keeps its name without its bytes, as [`Registry::route_attachments`]
    /// does: the message is the one it was, so sent again under its key it's answered as its repeat
    /// where it was taken (CP-T3.2, the twenty-second adversarial check's).
    pub fn inline_uploads(&self, attachments: Vec<super::model::Attachment>) -> Vec<super::model::Attachment> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        attachments
            .into_iter()
            .filter_map(|mut a| {
                a.path = None;
                let Some(fid) = a.file_id.take() else { return (!a.data_base64.is_empty()).then_some(a) };
                let Some((name, mime, data)) = self.upload_of(&fid) else {
                    a.data_base64.clear();
                    return (!a.name.is_empty()).then_some(a);
                };
                if a.name.is_empty() {
                    a.name = name;
                }
                if a.mime.is_empty() {
                    a.mime = mime;
                }
                a.data_base64 = b64.encode(&data);
                Some(a)
            })
            .collect()
    }

    /// A message's attachments, made ready for where the session runs: uploads resolved, and
    /// for a machine, every file sent ahead in pieces (the message then refers to them by id).
    async fn route_attachments(&self, hub: &Hub, id: &str, loc: &Location, c: Command) -> Result<Command, Sent> {
        use base64::Engine as _;
        let Command::Send { id: mid, text, mode, attachments, overrides, reply_to, task } = c else { return Ok(c) };
        if attachments.is_empty() {
            return Ok(Command::Send { id: mid, text, mode, attachments, overrides, reply_to, task });
        }
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut out = Vec::new();
        for mut a in attachments {
            a.path = None;
            // The bytes: inline, or an upload.
            let data: Option<bytes::Bytes> = match a.file_id.take().and_then(|f| self.upload_of(&f)) {
                Some((name, mime, data)) => {
                    if a.name.is_empty() {
                        a.name = name;
                    }
                    if a.mime.is_empty() {
                        a.mime = mime;
                    }
                    Some(data)
                }
                None => b64.decode(a.data_base64.as_bytes()).ok().map(bytes::Bytes::from),
            };
            let Some(data) = data.filter(|d| !d.is_empty()) else {
                // Its upload is gone (the server started again, or let it go for room or age): its
                // name still goes, so the message is the one it was (CP-T3.2, the twenty-second
                // adversarial check's). Sent again after its first try was taken, it's answered as
                // its repeat, never refused as another message under its id (whose Retry would be a
                // second prompt). New, it goes without the file, which its runner leaves out, said.
                a.data_base64.clear();
                out.push(a);
                continue;
            };
            match loc {
                Location::Local => {
                    let fid = crate::util::new_id();
                    let (host, sid, f) = (Arc::clone(&self.local), id.to_owned(), fid.clone());
                    let stored = crate::rt::blocking(move || host.receive(&sid, &f, 0, &data, true)).await;
                    if !matches!(stored, Ok(Ok(()))) {
                        continue;
                    }
                    a.data_base64.clear();
                    a.file_id = Some(fid);
                }
                Location::Machine(machine) => {
                    let fid = crate::util::new_id();
                    let total = data.len();
                    let mut offset = 0usize;
                    while offset < total || total == 0 {
                        let end = (offset + FILE_CHUNK).min(total);
                        let piece = data.get(offset..end).map(<[u8]>::to_vec).unwrap_or_default();
                        let msg = ServerMsg::HarnessFile { session: id.to_owned(), file: fid.clone(), offset: offset as u64, data: piece, done: end >= total };
                        if !hub.send_to_agent(machine, LANE_TERM, &msg).await {
                            return Err(Sent::Unreachable);
                        }
                        offset = end;
                        if end >= total {
                            break;
                        }
                    }
                    a.data_base64.clear();
                    a.file_id = Some(fid);
                }
            }
            out.push(a);
        }
        Ok(Command::Send { id: mid, text, mode, attachments: out, overrides, reply_to, task })
    }

    /// A harness signed in on a machine (kiln's /login): each of that machine's sessions of it is
    /// told, to take up the account (Claude Code starts again with it once its turn ends; Codex reads
    /// it again).
    pub async fn signed_in(&self, hub: &Hub, target: &str, kind: &str, account: Option<&crate::accounts::Account>) {
        // Who it's signed in as now, so each session takes it at once rather than when it next
        // starts (its page would show the old account, and that account's usage, until then).
        let value = account.map(|a| serde_json::json!({"email": a.email, "org": a.org, "plan": a.plan, "method": a.method}));
        let ids: Vec<String> = self.list().into_iter().filter(|i| i.target == target && i.kind == kind).map(|i| i.id).collect();
        for id in ids {
            let _ = self.command(hub, &id, Command::Control { id: "account".into(), value: value.clone() }).await;
        }
    }

    /// The epoch the server last heard session `id` is at.
    pub fn epoch_of(&self, id: &str) -> u64 {
        let seen = self.mirrors.lock().get(id).map_or(0, |m| m.info.epoch);
        self.epochs.lock().get(id).copied().unwrap_or(0).max(seen)
    }

    /// Waits for the answer to message `message` sent to session `id`: none when too many wait
    /// already (it isn't waited for).
    fn wait_for(&self, id: &str, message: &str) -> Option<futures::channel::oneshot::Receiver<Said>> {
        let mut w = self.waiting.lock();
        if w.len() >= WAITING_MAX {
            // Those whose senders went away (and no answer came for) go first.
            w.retain(|_, v| {
                v.retain(|tx| !tx.is_canceled());
                !v.is_empty()
            });
            if w.len() >= WAITING_MAX {
                return None;
            }
        }
        let (tx, rx) = futures::channel::oneshot::channel();
        w.entry((id.to_owned(), message.to_owned())).or_default().push(tx);
        Some(rx)
    }

    /// No longer waiting for message `message` sent to session `id`: the waits whose ends were
    /// dropped go (an answer for those still waiting still reaches them).
    fn unwait(&self, id: &str, message: &str) {
        let mut w = self.waiting.lock();
        let key = (id.to_owned(), message.to_owned());
        if let Some(v) = w.get_mut(&key) {
            v.retain(|tx| !tx.is_canceled());
            if v.is_empty() {
                w.remove(&key);
            }
        }
    }

    /// Session `id`'s inbox said what became of a message: whoever waits for it longest is told (each
    /// send of it is answered once, in the order they went: two sent under one id at once, one
    /// taken and one refused, are each told their own).
    fn answered(&self, id: &str, said: Said) {
        crate::metrics::sends().said(&said);
        let key = (id.to_owned(), said.message.clone());
        let first = {
            let mut w = self.waiting.lock();
            let Some(v) = w.get_mut(&key) else { return };
            v.retain(|tx| !tx.is_canceled());
            let first = (!v.is_empty()).then(|| v.remove(0));
            if v.is_empty() {
                w.remove(&key);
            }
            first
        };
        if let Some(tx) = first {
            let _ = tx.send(said);
        }
    }

    /// The answer to message `message` sent to session `id`, once it comes (by [`ANSWER_WAIT`]
    /// on `hub`'s clock): on its way when it doesn't.
    async fn answer(&self, hub: &Hub, id: &str, message: &str, rx: futures::channel::oneshot::Receiver<Said>) -> Sent {
        let late = hub.clock.sleep(ANSWER_WAIT);
        let sent = match futures::future::select(rx, late).await {
            futures::future::Either::Left((Ok(said), _)) => match said.answer {
                Ok(a) => Sent::Accepted(a),
                Err(r) => Sent::Refused(r),
            },
            _ => {
                crate::metrics::sends().unanswered.inc();
                Sent::Ok
            }
        };
        self.unwait(id, message);
        sent
    }

    /// A command for a session, from any device. A message (`Command::Send`, a skill run with an
    /// id) goes for the epoch the server last heard the session is at, and its inbox's answer is
    /// waited for (CP-T3.2): one sent for an epoch the session has since left is sent again, once,
    /// for the epoch its inbox named.
    pub async fn command(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        let pending = self.dispatch(hub, id, c.clone()).await;
        self.resolve(hub, id, c, pending).await
    }

    /// What command `c` to session `id` is, for what its answers mean (`super::asked`): a message,
    /// and whether it goes under its id to an inbox that takes it once.
    pub fn kind_of(&self, hub: &Hub, id: &str, c: &Command) -> super::asked::Kind {
        let message = super::session::is_message(c);
        let inbox = match self.location(id) {
            Some(Location::Local) => self.local.inbox().is_some(),
            Some(Location::Machine(machine)) => hub.instances.lock().wide_caps_for(&machine, &crate::instances::Owned::Agent(id.to_owned())) & crate::proto::CAP_INBOX != 0,
            None => false,
        };
        super::asked::Kind { message, deduped: message && inbox && message_id(c).is_some() }
    }

    /// Sends on command `c` to session `id`, new under the id `cmd` its client made, from `scope`
    /// (the UI's sockets', and an API call's under its key: [`super::asked::Asked::take`] took
    /// it). Its answers are kept as their stand says, whether or not who sent it is still there to
    /// hear them: one parked at once, then its runner's once it's back.
    pub async fn send_asked(&self, hub: &Arc<Hub>, id: &str, scope: &str, cmd: &str, c: Command) {
        use std::time::Instant;
        let pending = self.dispatch(hub, id, c.clone()).await;
        let (h, session, scope, cmd) = (Arc::clone(hub), id.to_owned(), scope.to_owned(), cmd.to_owned());
        crate::rt::spawn(async move {
            let sent = match pending {
                Pending::Command(p) if p.is_away() => {
                    h.harness.asked.answered(&session, &scope, &cmd, &Sent::Parked, Instant::now());
                    Sent::of(p.answer(crate::wire::commands::PARK_FOR).await)
                }
                other => h.harness.resolve(&h, &session, c, other).await,
            };
            h.harness.asked.answered(&session, &scope, &cmd, &sent, Instant::now());
        });
    }

    /// What became of command `c` to session `id`, sent as [`Registry::dispatch`] said (`p`): its
    /// answer, waited for.
    pub async fn resolve(&self, hub: &Hub, id: &str, c: Command, p: Pending) -> Sent {
        match p {
            Pending::Now(sent) => sent,
            Pending::Answer { message, rx } => self.settle(hub, id, c, &message, rx).await,
            Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
        }
    }

    /// The answer to command `c`, message `message`, sent to session `id` ([`Registry::dispatch`]):
    /// sent for an epoch the session has since left, it's sent again, once, for the one its inbox
    /// named.
    pub async fn settle(&self, hub: &Hub, id: &str, c: Command, message: &str, rx: futures::channel::oneshot::Receiver<Said>) -> Sent {
        let sent = self.answer(hub, id, message, rx).await;
        if let Sent::Refused(r) = &sent
            && let Some(current) = again_for(r)
        {
            self.raise_epoch(id, current);
            return self.command_once(hub, id, c).await;
        }
        sent
    }

    async fn command_once(&self, hub: &Hub, id: &str, c: Command) -> Sent {
        match self.dispatch(hub, id, c).await {
            Pending::Now(sent) => sent,
            Pending::Answer { message, rx } => self.answer(hub, id, &message, rx).await,
            Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
        }
    }

    /// Sends command `c` to session `id` now, after whatever was sent before: how it fared, or (a
    /// message) its inbox's answer to wait for ([`Registry::settle`]), so a sender that goes on
    /// meanwhile (a viewer's socket) waits for nothing.
    pub async fn dispatch(&self, hub: &Hub, id: &str, c: Command) -> Pending {
        // A person's pause of a session holds the subagents kiln started under it too, and resuming
        // it lets them go: each still going is sent its own, its answer not waited for.
        if let Command::Pause { on, task: None } = &c {
            let all = self.list();
            for sub in super::lineage::subagents_under(&all, id) {
                if all.iter().any(|i| i.id == sub && i.status != Status::Exited) {
                    let _ = Box::pin(self.dispatch_one(hub, &sub, Command::Pause { on: *on, task: None })).await;
                }
            }
        }
        // What a person lets a session do: never more than its parent's, and the subagents kiln
        // started under it narrowed by it, each sent its own.
        if let Command::Perms { perms, task: None } = &c {
            let all = self.list();
            let mut mine = None;
            for (sid, p) in super::lineage::perms_down(&all, id, perms.as_ref()) {
                if sid == id {
                    mine = Some(p);
                } else if all.iter().any(|i| i.id == sid && i.status != Status::Exited) {
                    let _ = Box::pin(self.dispatch_one(hub, &sid, Command::Perms { perms: p, task: None })).await;
                }
            }
            return Box::pin(self.dispatch_one(hub, id, Command::Perms { perms: mine.flatten(), task: None })).await;
        }
        Box::pin(self.dispatch_one(hub, id, c)).await
    }

    /// Command `c` for session `id` as [`Registry::command`] sends it, with what kiln and the relays
    /// before it record of passing it on (`relayed`: docs/ci.md W27-R08's relays, `crate::relays::words`)
    /// beside it in what goes to the runner, which checks it before the session takes it; sent again
    /// so when its epoch was left meanwhile.
    pub async fn command_relayed(&self, hub: &Hub, id: &str, c: Command, relayed: Value) -> Sent {
        let pending = self.dispatch_relayed(hub, id, c.clone(), Some(&relayed)).await;
        if let Pending::Answer { message, rx } = pending {
            let sent = self.answer(hub, id, &message, rx).await;
            if let Sent::Refused(r) = &sent
                && let Some(current) = again_for(r)
            {
                self.raise_epoch(id, current);
                return match self.dispatch_relayed(hub, id, c, Some(&relayed)).await {
                    Pending::Now(sent) => sent,
                    Pending::Answer { message, rx } => self.answer(hub, id, &message, rx).await,
                    Pending::Command(p) => Sent::of(p.answer_or_parked(crate::hub::COMMAND_WITHIN).await),
                };
            }
            return sent;
        }
        self.resolve(hub, id, c, pending).await
    }

    /// [`Registry::dispatch`], to session `id` alone.
    async fn dispatch_one(&self, hub: &Hub, id: &str, c: Command) -> Pending {
        self.dispatch_relayed(hub, id, c, None).await
    }

    /// [`Registry::dispatch_one`], with what kiln and the relays before it record of passing it on
    /// (`relayed`, [`Registry::command_relayed`]) beside it in what goes to the runner.
    async fn dispatch_relayed(&self, hub: &Hub, id: &str, c: Command, relayed: Option<&Value>) -> Pending {
        let Some(loc) = self.location(id) else { return Pending::Now(Sent::NoSession) };
        // Kiln's own (`goals::keep`), kept here: its harness has nothing to do with it.
        if let Command::Control { id: control, value } = &c
            && control == "keep_going"
        {
            self.set_keep_going(hub, id, value.as_ref().and_then(Value::as_bool).unwrap_or(true));
            return Pending::Now(Sent::Ok);
        }
        // A name is kept here: the session takes it at once, whatever its machine can do (and
        // its harness names its conversation too, where it does).
        let renamed = matches!(&c, Command::Control { id: control, .. } if control == "rename");
        if let Command::Control { id: control, value } = &c
            && control == "rename"
        {
            self.rename(hub, id, value.as_ref());
        }
        let c = match self.route_attachments(hub, id, &loc, c).await {
            Ok(c) => c,
            Err(e) => return Pending::Now(e),
        };
        let message = message_id(&c);
        let is_message = super::session::is_message(&c);
        // A message without an id goes on unchecked (counted).
        let unchecked = message.is_none() && is_message;
        let epoch = self.epoch_of(id);
        match loc {
            Location::Local => {
                let wait = message.as_deref().and_then(|m| self.wait_for(id, m));
                if !self.local.command_at(id, c, Some(epoch)) {
                    // Not waited for after all: its end of the wait goes first, then the wait.
                    drop(wait);
                    if let Some(m) = &message {
                        self.unwait(id, m);
                    }
                    // Its session is here: its lane holds its most (a message is refused so).
                    if message.is_some() && self.local.get(id).is_some() {
                        return Pending::Now(Sent::Refused(inbox::Refusal::Full { session: id.to_owned(), limit: super::host::LANE }));
                    }
                    return Pending::Now(Sent::NoSession);
                }
                if unchecked {
                    crate::metrics::sends().unchecked.inc();
                }
                match (wait, message) {
                    (Some(rx), Some(message)) => Pending::Answer { message, rx },
                    // A command its session here took.
                    _ if !is_message => Pending::Now(Sent::Taken),
                    _ => Pending::Now(Sent::Ok),
                }
            }
            Location::Machine(machine) => {
                let Ok(mut json) = serde_json::to_string(&c) else { return Pending::Now(Sent::NoSession) };
                if let Some(r) = relayed
                    && let Ok(mut v) = serde_json::from_str::<Value>(&json)
                    && let Some(o) = v.as_object_mut()
                {
                    o.insert("relayed".into(), r.clone());
                    json = v.to_string();
                }
                // A runner with inboxes takes a message for an epoch, and answers it.
                let (caps, away) = {
                    let inst = hub.instances.lock();
                    (inst.wide_caps_for(&machine, &crate::instances::Owned::Agent(id.to_owned())), inst.newest(&machine).is_none())
                };
                let had_id = message.is_some();
                // A message with an id is refused at once while its runner is away (docs/wire.md's
                // `Send`), as before the command streams: parked on one, it would go with no epoch
                // to the copy there once that runner was back, though its session went on elsewhere
                // meanwhile (resumed, moved) and took it there too, sent again under its id: one
                // message, two harnesses (CP-T3.2, the twenty-second adversarial check's). CP-T3.3
                // parks it under its epoch.
                if had_id && away {
                    return Pending::Now(Sent::Unreachable);
                }
                if let Some(m) = message.filter(|_| caps & crate::proto::CAP_INBOX != 0) {
                    let wait = self.wait_for(id, &m);
                    if !hub.send_to_agent(&machine, LANE_TERM, &ServerMsg::HarnessSend { session: id.to_owned(), epoch, json }).await {
                        drop(wait);
                        self.unwait(id, &m);
                        return Pending::Now(Sent::Unreachable);
                    }
                    return match wait {
                        Some(rx) => Pending::Answer { message: m, rx },
                        None => Pending::Now(Sent::Ok),
                    };
                }
                // On the runner's command stream, which answers it (docs/wire.md, "Commands").
                let p = hub.command_start(&machine, &ServerMsg::HarnessCmd { session: id.to_owned(), json }).await;
                let unreachable = matches!(p.at_once(), Some(crate::wire::commands::Answer::Unreachable { .. }));
                if unreachable && !renamed {
                    return Pending::Now(Sent::Unreachable);
                }
                // A message to a runner from before inboxes, or one without an id: unchecked.
                if unchecked || had_id {
                    crate::metrics::sends().unchecked.inc();
                }
                // A name is given even while the machine is away: it's kept here.
                if unreachable { Pending::Now(Sent::Ok) } else { Pending::Command(p) }
            }
        }
    }

    /// Stop a session for good.
    /// Session `id` goes on on machine `to` (in folder `cwd`, else the one it worked in), from the
    /// server's conversation store, because its own machine is gone (`docs/conversations.md`,
    /// "Resuming on another machine"). From here on the copy on the machine it was on doesn't
    /// count: that machine lets go of it if it comes back. Ok once the package is on its way.
    pub async fn resume_on(&self, hub: &Hub, id: &str, to: &str, cwd: Option<String>) -> Result<(), String> {
        let store = self.convo.clone().ok_or_else(|| "this server keeps no conversations (KILN_CONVERSATIONS is off)".to_owned())?;
        let (from, epoch) = {
            let g = self.mirrors.lock();
            let m = g.get(id).ok_or_else(|| "no such agent session".to_owned())?;
            (m.location.clone(), m.state.get("epoch").and_then(Value::as_u64).unwrap_or(0))
        };
        let from_name = match &from {
            Location::Local => return Err("it runs on the server itself, which is up: move it instead".into()),
            Location::Machine(m) => m.clone(),
        };
        if hub.agents.lock().get(&from_name).is_some_and(|a| a.connected) {
            return Err(format!("{} is online: move the session from there instead", hub.shown(&from_name)));
        }
        if !hub.agents.lock().get(to).is_some_and(|a| a.connected) {
            return Err(format!("{} isn't connected", hub.shown(to)));
        }
        if hub.runner_caps(to) & crate::proto::CAP_CONVO == 0 {
            return Err(format!("{} runs a kiln too old to take a session from the server; upgrade it first", hub.shown(to)));
        }
        let (st, i, f) = (store.clone(), id.to_owned(), from_name.clone());
        let (from_shown, to_shown) = (hub.shown(&from_name), hub.shown(to));
        let mut pkg = crate::rt::blocking(move || crate::convo::resume::package(&st, &i, &f, cwd.as_deref(), (&from_shown, &to_shown)))
            .await
            .map_err(|e| format!("{e:#}"))?
            .map_err(|e| format!("{e:#}"))?;
        // kiln takes it for the process that held it, which can't hand it on: the package carries
        // the take, which the receipt follows as a hand-on's (docs/ci.md W27-R08).
        let heard = hub.agents.lock().get(&from_name).map(|a| a.last_seen).unwrap_or_default();
        let when = time::OffsetDateTime::from_unix_timestamp(heard).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_else(|| "never".to_owned());
        let why = format!("resumed from kiln's copy: {} was last heard from at {when}", hub.shown(&from_name));
        match crate::custody::take_for(&hub.store, &hub.pki, id, pkg.state.epoch, &pkg.events, &why).await {
            Ok(take) => pkg.hand_on = take,
            Err(e) => tracing::warn!(target: "kiln.custody", session = %id, error = %format!("{e:#}"), "kiln couldn't take a session it resumes from its copy: it goes on unsigned"),
        }
        if hub.runner_wide_caps(to) & crate::proto::CAP_CUSTODY == 0 {
            let why = format!("{} runs a kiln from before signed hand-overs: it signs no receipt of kiln's take", hub.shown(to));
            unsigned(hub, id, pkg.state.epoch.saturating_add(1), &format!("runner {to}"), why);
        }
        let bytes = serde_json::to_vec(&pkg).map_err(|e| e.to_string())?;
        // Its copy where it was doesn't count from now on: not here, not in the store (rows that
        // machine still sends are left out).
        let next = epoch.max(pkg.state.epoch) + 1;
        self.raise_epoch(id, next);
        store.put(vec![crate::convo::rows::Row::Session { session: id.to_owned(), info: None, meta: None, state: None, changes: None, epoch: Some(next), open: false, at: crate::util::now_ms() }]);
        let info = {
            let mut g = self.mirrors.lock();
            g.get_mut(id).map(|m| {
                m.location = Location::Machine(to.to_owned());
                m.info.target = m.location.target();
                m.info.status = Status::Starting;
                m.info.clone()
            })
        };
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info.clone()) });
        }
        tracing::info!(session = %id, from = %from_name, %to, bytes = bytes.len(), "agent session goes on on another machine, from the server's store");
        send_package(hub, id, to, &bytes).await
    }

    pub fn never_started(&self, id: &str) -> bool {
        let Some(s) = self.local.get(id) else { return false };
        if s.pid.lock().is_some() {
            return false;
        }
        let core = s.core.lock();
        core.state.status == Status::Exited && core.state.harness_session.is_none() && !core.events().any(|e| matches!(e.body, super::model::Body::User { .. } | super::model::Body::Assistant { .. }))
    }

    pub async fn move_local(&self, hub: &Hub, id: &str, to: &str, cwd: &str) -> Result<(), String> {
        let s = self.local.get(id).filter(|_| self.never_started(id)).ok_or_else(|| "only an agent whose harness never started on the kiln server moves from it".to_owned())?;
        if !hub.agents.lock().get(to).is_some_and(|a| a.connected) {
            return Err(format!("{} isn't connected", hub.shown(to)));
        }
        if hub.runner_caps(to) & crate::proto::CAP_CONVO == 0 {
            return Err(format!("{} runs a kiln too old to take a session from the server; upgrade it first", hub.shown(to)));
        }
        let spec = s.resume_spec();
        let (events, mut state) = {
            let core = s.core.lock();
            (core.events().cloned().collect::<Vec<_>>(), core.state.clone())
        };
        let had = self.mirrors.lock().get(id).and_then(|m| m.state.get("epoch").and_then(Value::as_u64)).unwrap_or(0);
        let next = had.max(state.epoch) + 1;
        state.epoch = next - 1;
        let from_cwd = state.cwd.clone();
        let pkg = super::moving::pack(id, spec, cwd, "the kiln server", &from_cwd, super::moving::Carry::Nothing, events, state);
        let bytes = serde_json::to_vec(&pkg).map_err(|e| e.to_string())?;
        send_package(hub, id, to, &bytes).await?;
        self.raise_epoch(id, next);
        if let Some(store) = &self.convo {
            store.put(vec![crate::convo::rows::Row::Session { session: id.to_owned(), info: None, meta: None, state: None, changes: None, epoch: Some(next), open: false, at: crate::util::now_ms() }]);
        }
        let info = {
            let mut g = self.mirrors.lock();
            g.get_mut(id).filter(|m| m.location == Location::Local).map(|m| {
                m.location = Location::Machine(to.to_owned());
                m.info.target = m.location.target();
                m.info.status = Status::Starting;
                m.opened_at = Some(std::time::Instant::now());
                m.info.clone()
            })
        };
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
        self.local.let_go(id);
        tracing::info!(session = %id, %to, bytes = bytes.len(), "an agent whose harness never started on the server goes on on a runner");
        Ok(())
    }

    pub async fn close(&self, hub: &Hub, id: &str) -> bool {
        let loc = self.location(id);
        let ok = match &loc {
            None => false,
            Some(Location::Local) => self.local.close(id),
            // Closed there, or once its machine is back; one its machine doesn't run is gone there.
            Some(Location::Machine(machine)) => {
                use crate::wire::commands::Answer;
                matches!(hub.command(machine, &ServerMsg::HarnessClose { session: id.to_owned() }, crate::hub::COMMAND_WITHIN).await, Answer::Taken | Answer::Sent | Answer::Parked)
            }
        };
        // A machine that is gone cannot confirm: forget it here; it is dropped there on reconnect.
        // Ended in the store too, or it would be listed as away again after a restart.
        if loc.is_some() && (!ok || matches!(loc, Some(Location::Local))) {
            if let Some(mut m) = self.mirrors.lock().remove(id) {
                // Its feed's topic goes with it once its viewers are told.
                m.feed.publish(&Frame::of(crate::wire::ui::SessionFrame::Closed.into()));
            }
            self.keeper.forget(id);
            self.visuals.forget(id);
            self.streams.forget(hub, id).await;
            self.opened.forget(id);
            self.asked.forget(id);
            if let Some(store) = &self.convo {
                store.put(vec![crate::convo::rows::Row::Closed { session: id.to_owned(), at: crate::util::now_ms() }]);
            }
            super::archive::closed(hub, id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
        ok || loc.is_some()
    }

    /// The parsed state of a session (for checks before acting on it).
    pub fn state(&self, id: &str) -> Option<State> {
        // Read with the mirrors let go, from the copy's own (no copy of it made).
        let state = self.mirrors.lock().get(id).map(|m| Arc::clone(&m.state))?;
        State::deserialize(&*state).ok()
    }

    /// A session as lists show it.
    pub fn info(&self, id: &str) -> Option<Info> {
        self.mirrors.lock().get(id).map(|m| m.info.clone())
    }

    /// What waits in session `id`'s queue, as its machine last said, the messages whole.
    pub fn queue(&self, id: &str) -> Vec<super::model::QueueItem> {
        let Some(state) = self.mirrors.lock().get(id).map(|m| Arc::clone(&m.state)) else { return Vec::new() };
        state.get("queue").and_then(|q| Vec::<super::model::QueueItem>::deserialize(q).ok()).unwrap_or_default()
    }

    /// What `agents.get` and `agents.result` read of a session (`harness::ops`): its state as
    /// last reported, its last reply on its main thread (an `assistant` event) and its changeset.
    pub fn conclusion(&self, id: &str) -> Option<(Value, Option<Value>, Value)> {
        // References under the lock; the copies made with it let go.
        let (state, reply, changes) = {
            let g = self.mirrors.lock();
            let m = g.get(id)?;
            let reply = m.events.iter().rev().find(|e| e.get("kind").and_then(Value::as_str) == Some("assistant") && e.get("parent").is_none_or(Value::is_null)).cloned();
            (Arc::clone(&m.state), reply, m.changes.clone())
        };
        let changes = changes.map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value.clone()).collect()));
        Some((Value::clone(&state), reply.map(|e| e.value.clone()), changes))
    }

    /// Tests: session `id`'s changeset, as its machine would report it.
    #[cfg(test)]
    pub fn set_changes(&self, id: &str, files: Value) -> bool {
        let files = match files {
            Value::Array(all) => Some(Arc::new(all.into_iter().map(|f| Arc::new(Entry::new(f))).collect())),
            _ => None,
        };
        self.mirrors.lock().get_mut(id).map(|m| m.changes = files).is_some()
    }
}

/// A finished message's mentions of files, to keep (`super::kept`): what the person or the agent
/// said, never a tool's input or output.
fn mentions(id: &str, cwd: &str, location: &Location, e: &super::model::Event) -> Option<Job> {
    let text = match &e.body {
        super::model::Body::User { text, .. } | super::model::Body::Assistant { text, streaming: false } => text,
        _ => return None,
    };
    let paths: Vec<String> = files::mentioned(text).iter().filter_map(|p| files::resolve(p, cwd)).collect();
    (!paths.is_empty()).then(|| Job { session: id.to_owned(), seq: e.seq, paths, location: location.clone() })
}

fn default_name(kind: &str) -> String {
    super::kinds::Kind::get(kind).map_or_else(|| kind.to_owned(), |k| k.name.to_owned())
}

/// The plan an event proposes (plan mode): its main thread's plan tool, with its words.
fn proposed_in(e: &super::model::Event) -> Option<String> {
    let super::model::Body::Tool(call) = &e.body else { return None };
    if e.parent.is_some() || call.tool != super::model::ToolKind::Plan {
        return None;
    }
    call.input.get("plan").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned)
}


/// Session `id`'s hand-over to `epoch`, taken by `took`, went unsigned because `why`: recorded,
/// with the process kiln last certified it as before then (docs/ci.md W27-R08).
fn unsigned(hub: &Hub, id: &str, epoch: u64, took: &str, why: String) {
    let (store, session, took) = (hub.store.clone(), id.to_owned(), took.to_owned());
    crate::rt::spawn(async move {
        let gave = crate::identities::holder_at(&store, &session, epoch.saturating_sub(1)).await.ok().flatten().unwrap_or_default();
        if let Err(e) = crate::custody::unsigned(&store, &session, epoch, &gave, &took, &why, crate::util::now_unix()).await {
            tracing::warn!(target: "kiln.custody", %session, error = %format!("{e:#}"), "an unsigned hand-over couldn't be recorded");
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A viewer that takes state patches, and one that takes nothing as a patch.
    const STATE: Patches = Patches { state: true, changes: false };
    const WHOLE: Patches = Patches { state: false, changes: false };

    /// W21-R15: a start under ids its client made starts its sessions under them, and one asked
    /// again under any of them (its answer lost on the way back) starts nothing: it's refused as
    /// taken. An id a start under way holds is refused too, until that start is done.
    #[test]
    fn w21_r15_a_start_under_ids_its_client_made_starts_each_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            let dir = tempfile::tempdir()?;
            let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), ..Spec::default() };
            let (a, b) = (crate::util::new_id(), crate::util::new_id());
            let started = hub.harness.open_as(&hub, "local", spec.clone(), 2, &[a.clone(), b.clone()]).await?;
            assert_eq!(started.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), [a.as_str(), b.as_str()]);
            let listed = hub.harness.list().len();
            let again = hub.harness.open_as(&hub, "local", spec.clone(), 1, std::slice::from_ref(&b)).await;
            assert!(again.as_ref().err().and_then(|e| e.downcast_ref::<IdTaken>()).is_some_and(|t| t.0 == b), "{again:?}");
            assert_eq!(hub.harness.list().len(), listed, "nothing started twice");
            // Held by a start under way: refused until that start is done.
            let c = crate::util::new_id();
            let held = hub.harness.claim(std::slice::from_ref(&c))?;
            assert!(hub.harness.claim(std::slice::from_ref(&c)).is_err());
            drop(held);
            let one = hub.harness.open_as(&hub, "local", spec, 1, std::slice::from_ref(&c)).await?;
            assert_eq!(one.first().map(|i| i.id.as_str()), Some(c.as_str()));
            assert!(crate::util::is_id(&crate::util::new_id()) && !crate::util::is_id("../../etc") && !crate::util::is_id(&a.to_uppercase()));
            Ok(())
        })
    }

    /// CP-T3.2: a message a machine without inboxes took (`seq` 0) is answered as taken, and as
    /// unchecked: sent again, it's another message. One an inbox took says nothing of the kind.
    #[test]
    fn a_message_no_inbox_checked_is_answered_unchecked() {
        let answer = |seq: u64| Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq, repeat: false };
        let unchecked = super::accepted("m1", &answer(0));
        assert!(unchecked.get("unchecked").and_then(Value::as_str).is_some_and(|w| w.contains("another message")), "{unchecked}");
        assert_eq!(unchecked.get("sent"), Some(&json!(true)));
        assert!(super::accepted("m1", &answer(7)).get("unchecked").is_none());
    }

    /// CP-T3.2: a wait for a message's answer that's given up goes (none is left behind to fill the
    /// most there may be); one still waiting stays, and is told the answer when it comes.
    #[test]
    fn waits_given_up_go_and_those_still_waiting_are_told() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let conversations = crate::config::Conversations { off: true, ..Default::default() };
            let r = Registry::with(dir.path().to_path_buf(), &conversations, Arc::new(kiln_seams::clock::SimClock::new()), sim_bus());
            let gone = r.wait_for("s1", "m1");
            let kept = r.wait_for("s1", "m1").ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            drop(gone);
            r.unwait("s1", "m1");
            assert_eq!(r.waiting.lock().get(&("s1".to_owned(), "m1".to_owned())).map(Vec::len), Some(1), "the one still waiting stays");
            let said = Said { message: "m1".into(), answer: Err(inbox::Refusal::NoSession { session: "s1".into() }) };
            r.answered("s1", said.clone());
            assert!(r.waiting.lock().is_empty());
            assert_eq!(kept.await.ok(), Some(said));
            // Given up before any answer: nothing is left.
            let w = r.wait_for("s2", "m2");
            drop(w);
            r.unwait("s2", "m2");
            assert!(r.waiting.lock().is_empty());
            Ok(())
        })
    }

    /// CP-T3.2: each send of a message is answered once, in the order they went: two sent under one
    /// id at once, the first taken and the other refused (another message under its id), are each
    /// told their own.
    #[test]
    fn each_send_under_an_id_is_told_its_own_answer() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let conversations = crate::config::Conversations { off: true, ..Default::default() };
            let r = Registry::with(dir.path().to_path_buf(), &conversations, Arc::new(kiln_seams::clock::SimClock::new()), sim_bus());
            let first = r.wait_for("s1", "m1").ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let second = r.wait_for("s1", "m1").ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let taken = Said { message: "m1".into(), answer: Ok(inbox::Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq: 7, repeat: false }) };
            let reused = Said { message: "m1".into(), answer: Err(inbox::Refusal::Reused { session: "s1".into(), message: "m1".into() }) };
            r.answered("s1", taken.clone());
            r.answered("s1", reused.clone());
            assert_eq!(first.await.ok(), Some(taken));
            assert_eq!(second.await.ok(), Some(reused));
            assert!(r.waiting.lock().is_empty());
            Ok(())
        })
    }

    /// One update taken by a copy, as `Registry::ingest_from` takes it: its revision first.
    fn took<R>(m: &mut Mirror, f: impl FnOnce(&mut Mirror) -> R) {
        m.rev += 1;
        f(m);
    }

    fn event(id: &str, text: &str) -> Arc<Entry> {
        Arc::new(Entry::new(json!({"id": id, "kind": "assistant", "text": text})))
    }

    /// A copy's state, set as its runner's would be.
    fn set_state(m: &mut Mirror, v: Value) {
        m.state = Arc::new(v);
    }

    /// A copy's changeset as JSON (null before it has one).
    fn changes_of(m: &Mirror) -> Value {
        m.changes.as_ref().map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value.clone()).collect()))
    }

    /// Changeset files as a copy keeps them.
    fn entries(files: Vec<Value>) -> Vec<Arc<Entry>> {
        files.into_iter().map(|f| Arc::new(Entry::new(f))).collect()
    }

    /// The ids of the events in a viewer's frames, in order.
    fn ids(frames: &[String]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| serde_json::from_str::<Value>(f).ok())
            .filter(|f| f.get("t").and_then(Value::as_str) == Some("events"))
            .flat_map(|f| f.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    #[test]
    fn a_viewer_resumes_with_only_what_changed() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        for id in ["e1", "e2", "e3"] {
            took(&mut m, |m| m.upsert(event(id, "x")));
        }
        let had = m.rev;
        took(&mut m, |m| m.upsert(event("e4", "new")));
        took(&mut m, |m| m.append("e2", " more"));
        let frames = m.resume(had, None).unwrap_or_default();
        assert!(frames.first().is_some_and(|f| f.contains("\"t\":\"resumed\"")), "it resumes: {frames:?}");
        assert_eq!(ids(&frames), vec!["e2".to_owned(), "e4".to_owned()], "only what changed since, in the timeline's order");
        // Nothing changed since now: only the copy's newest state and its revision.
        assert_eq!(m.resume(m.rev, None).map(|f| ids(&f)), Some(Vec::new()));
        // A revision past what it has is another process's: not resumed.
        assert!(m.resume(m.rev + 1, None).is_none());
    }

    #[test]
    fn a_copy_that_started_over_is_sent_whole() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        took(&mut m, |m| m.upsert(event("e1", "x")));
        let had = m.rev;
        took(&mut m, |m| {
            m.clear_events();
            m.started_over();
        });
        took(&mut m, |m| m.upsert(event("e9", "after")));
        assert!(m.resume(had, None).is_none(), "a viewer from before the reset starts over");
        assert_eq!(m.resume(m.reset_at, None).map(|f| ids(&f)), Some(vec!["e9".to_owned()]));
    }

    /// The frames the copy writes out itself (live, with their revision, and in snapshots, without)
    /// are byte for byte the socket's own (`SessionDown`), whatever they carry: escapes, nesting,
    /// empty patches, a field gone.
    #[test]
    fn frames_are_written_as_the_socket_says_them() -> anyhow::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let odd = json!({"id": "e\"1\u{2028}", "seq": 3, "text": "line\nnext } ] \\ ☃", "n": [1, 2.5, null, true], "o": {"z": 1, "a": {"b": []}}});
        let files = [json!({"path": "src/a.rs", "added": 3, "hunks": [{"lines": ["+x", "-y \"q\""]}]}), json!({"path": "b.rs", "tree": "/w"})];
        let gone = vec![json!({"path": "c.rs"}), json!({"path": "d.rs", "tree": "/w"})];
        for rev in [None, Some(0), Some(42)] {
            let e = Entry::new(odd.clone());
            let fs: Vec<Entry> = files.iter().cloned().map(Entry::new).collect();
            let say = |frame: SessionFrame| SessionDown { frame, rev }.text();
            let written = |f: &dyn Fn(&mut Vec<u8>) -> std::io::Result<()>| write::string(64, |b| f(b));
            assert_eq!(written(&|b| write::events(b, [&e, &e], rev)), say(SessionFrame::Events { events: vec![odd.clone(), odd.clone()] }));
            assert_eq!(written(&|b| write::events(b, std::iter::empty::<&Entry>(), rev)), say(SessionFrame::Events { events: Vec::new() }));
            assert_eq!(written(&|b| write::value(b, "state", "state", &odd, rev)), say(SessionFrame::State { state: odd.clone() }));
            assert_eq!(written(&|b| write::value(b, "meta", "meta", &odd, rev)), say(SessionFrame::Meta { meta: odd.clone() }));
            assert_eq!(written(&|b| write::changes(b, fs.iter(), rev)), say(SessionFrame::Changes { files: Value::Array(files.to_vec()) }));
            let set: Vec<&Entry> = fs.iter().collect();
            assert_eq!(written(&|b| write::changes_patch(b, &set, &gone, rev)), say(SessionFrame::ChangesPatch { set: files.to_vec(), gone: gone.clone() }));
            assert_eq!(written(&|b| write::changes_patch(b, &[], &[], rev)), say(SessionFrame::ChangesPatch { set: Vec::new(), gone: Vec::new() }));
            assert_eq!(written(&|b| write::changes_patch(b, set.get(..1).unwrap_or_default(), &[], rev)), say(SessionFrame::ChangesPatch { set: files.get(..1).unwrap_or_default().to_vec(), gone: Vec::new() }));
            assert_eq!(written(&|b| write::stream(b, "e\"1", "}\n☃", rev)), say(SessionFrame::Stream { id: "e\"1".into(), text: "}\n☃".into() }));
            let (was, now) = (json!({"a": 1, "b": {"c": 2}, "gone": 3}), json!({"a": 1, "b": {"c": 4}, "new\"": [5]}));
            let (set, gone) = state_patch_of(&was, &now).ok_or_else(|| anyhow::anyhow!("no patch"))?;
            let want = SessionFrame::StatePatch { set: set.iter().map(|(k, v)| ((*k).clone(), (*v).clone())).collect(), gone: gone.iter().map(|k| (*k).clone()).collect() };
            assert_eq!(written(&|b| write::state_patch(b, &set, &gone, rev)), say(want));
            let (set, gone) = state_patch_of(&now, &now).ok_or_else(|| anyhow::anyhow!("no patch"))?;
            assert_eq!(written(&|b| write::state_patch(b, &set, &gone, rev)), say(SessionFrame::StatePatch { set: serde_json::Map::new(), gone: Vec::new() }));
            // An entry's text is serde_json's, and its digest the one the runner makes of it.
            assert_eq!(e.text(), serde_json::to_string(&odd)?);
            assert_eq!(crate::wire::sums::event_text(e.text()), crate::wire::sums::event(&odd));
        }
        Ok(())
    }

    /// A long session lets go of its oldest events as it goes: those let go leave its index and the
    /// rest are found where they are, as an index made again would have them.
    #[test]
    fn a_copy_that_lets_go_of_its_oldest_finds_the_rest_where_they_are() {
        let mut m = Mirror::new(Info::default(), Location::Local, sim_bus());
        let n = MAX_EVENTS + MAX_EVENTS / 2;
        for i in 1..=n {
            took(&mut m, |m| m.upsert(Arc::new(Entry::new(json!({"id": format!("e{i}"), "seq": i, "kind": "assistant", "text": "x"})))));
        }
        assert!(m.events.len() <= MAX_EVENTS + MAX_EVENTS / 4, "{} kept", m.events.len());
        let had = (m.index.clone(), m.changed.len());
        m.reindex();
        assert_eq!(had.0, m.index, "the index as made again");
        assert_eq!(had.1, m.changed.len(), "nothing changed is kept for an event let go");
        // A reply's words still go to its event.
        let last = format!("e{n}");
        took(&mut m, |m| m.append(&last, "more"));
        let at = m.index.get(&last).copied().unwrap_or(usize::MAX);
        assert_eq!(m.events.get(at).and_then(|e| e.get("text")).and_then(Value::as_str), Some("xmore"));
        assert_eq!(m.events.get(at).map(|e| e.text().contains("xmore")), Some(true), "its text written again");
    }

    /// With every viewer taking states and changesets as patches, they go as patches alone: a long
    /// session's whole changeset is never put on the bus (it takes 1 MiB at most). A viewer that
    /// takes them whole gets them whole again.
    #[test]
    fn frames_go_as_patches_alone_when_every_viewer_takes_them() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (patching, at) = m.feed.attach(CHANGES);
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        // 400 files of diffs, whole more than the bus takes (each line `"+x",`).
        let lines = u32::try_from(kiln_seams::bus::PAYLOAD_MAX / 400 / r#""+x","#.len() + 1)?;
        let many: Vec<Value> = (0..400).map(|n| file(&format!("src/f{n}.rs"), None, lines)).collect();
        m.take_changes(many, Vec::new(), true);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        // The first goes whole, nothing before it to patch: the bus refuses it, its viewers start over.
        assert_eq!(m.feed.publish_payload(f), super::super::live::Published::Refused);
        m.take_changes(vec![file("src/f3.rs", None, lines + 1)], Vec::new(), false);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        assert_eq!(m.feed.publish_payload(f), super::super::live::Published::Sent);
        set_state(&mut m, json!({"status": "running", "activity": "reading"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        set_state(&mut m, json!({"status": "running", "activity": "editing"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        // After the resync marker: the changeset's patch alone, the first state whole, the second its patch alone.
        let [_resync, changes, first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        assert!(changes.text.is_empty() && changes.patch.as_deref().is_some_and(|p| p.len() < 64 * 1024), "{}", changes.text.len());
        assert_eq!(serde_json::from_str::<Value>(changes.for_viewer(CHANGES))?, json!({"t": "changes_patch", "set": [file("src/f3.rs", None, lines + 1)], "rev": changes.rev}));
        assert!(first.patch.is_none() && !first.text.is_empty());
        assert!(second.text.is_empty());
        assert_eq!(serde_json::from_str::<Value>(second.for_viewer(CHANGES))?, json!({"t": "state_patch", "set": {"activity": "editing"}, "rev": second.rev}));
        // A viewer that takes them whole: whole again, its patch beside it for the rest.
        drop(patching);
        let (_whole, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        set_state(&mut m, json!({"status": "idle"}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [f] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        assert_eq!(serde_json::from_str::<Value>(f.for_viewer(WHOLE))?, json!({"t": "state", "state": {"status": "idle"}, "rev": f.rev}));
        Ok(())
    }

    #[test]
    fn a_frame_says_its_copys_revision() -> anyhow::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let f = Frame::of(SessionFrame::Events { events: Vec::new() }.into()).stamped(7);
        let v: Value = serde_json::from_str(&f.text)?;
        assert_eq!(v.get("rev").and_then(Value::as_u64), Some(7));
        assert_eq!(f.rev, 7);
        // Stamped, a frame is its type's with its revision.
        for frame in [SessionFrame::Events { events: vec![json!({"id": "e1"})] }, SessionFrame::Closed, SessionFrame::Stream { id: "e1".into(), text: "}".into() }] {
            let stamped = Frame::of(frame.clone().into()).stamped(7);
            assert_eq!(stamped.text.as_ref(), SessionDown { frame, rev: Some(7) }.text());
        }
        Ok(())
    }

    /// A plan its main thread proposes (plan mode) is one to keep; a subagent's, another tool's,
    /// or one without words isn't.
    #[test]
    fn a_plan_proposed_in_plan_mode_is_one_to_keep() -> anyhow::Result<()> {
        let ev = |parent: Value, tool: &str, input: Value| -> anyhow::Result<crate::harness::model::Event> {
            Ok(serde_json::from_value(
                json!({"id": "e1", "seq": 1, "at": 1, "parent": parent, "kind": "tool", "tool_id": "t1", "name": "ExitPlanMode", "tool": tool, "title": "Plan", "input": input, "status": "pending", "started": 1}),
            )?)
        };
        let plan = "# Fix it\n\n1. Read\n2. Fix";
        assert_eq!(proposed_in(&ev(Value::Null, "plan", json!({ "plan": plan }))?).as_deref(), Some(plan));
        assert_eq!(proposed_in(&ev(json!("task1"), "plan", json!({"plan": "1. a"}))?), None, "a subagent's");
        assert_eq!(proposed_in(&ev(Value::Null, "shell", json!({"plan": "1. a"}))?), None, "another tool's");
        assert_eq!(proposed_in(&ev(Value::Null, "plan", json!({"plan": "  "}))?), None, "no words");
        Ok(())
    }

    #[test]
    fn a_name_given_here_holds_whatever_the_machine_knows_of_names() {
        // A runner from before names reports none: the name given here shows.
        assert_eq!(naming("", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("", "", "Claude Code", false), Naming::Theirs);
        // Upgraded since, it reports its harness's name (in any case): still the one given here.
        assert_eq!(naming("Claude Code", "", "Claude Code", true), Naming::Keep);
        assert_eq!(naming("claude code ", "", "Claude Code", true), Naming::Keep);
        // A machine that names it itself: taken, and kept here too.
        assert_eq!(naming("slates-6d", "", "Claude Code", false), Naming::Adopt("slates-6d"));
        assert_eq!(naming("Mantle", "slates-6d", "Codex", true), Naming::Adopt("Mantle"));
        // Renamed here while the machine still reports its old one: the new one shows until the
        // machine takes it.
        assert_eq!(naming("slates-6d", "slates-6d", "Claude Code", true), Naming::Keep);
    }

    fn sim_bus() -> Arc<dyn kiln_seams::Bus> {
        Arc::new(kiln_seams::bus::SimBus::default())
    }

    /// The frames on a mirror's feed from `from`, as a viewer that takes patches reads them, and
    /// as one that doesn't.
    fn on_feed(bus: &Arc<dyn kiln_seams::Bus>, m: &Mirror, from: u64) -> anyhow::Result<Vec<Frame>> {
        let read = futures::executor::block_on(bus.read(m.feed.topic(), from, 100))?;
        read.messages.iter().map(|msg| super::super::live::decode(&msg.payload).ok_or_else(|| anyhow::anyhow!("a frame the feed didn't make"))).collect()
    }

    #[test]
    fn a_state_goes_out_whole_and_as_what_changed_in_it() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (_viewer, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let skills = json!([{"name": "deploy", "description": "Ship it. ".repeat(200)}]);
        set_state(&mut m, json!({"status": "running", "activity": "reading", "skills": skills, "pause": {"reason": "limit"}}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        set_state(&mut m, json!({"status": "running", "activity": "editing", "skills": skills}));
        let f = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // The first went whole: there was nothing before it to patch.
        assert!(first.patch.is_none());
        assert_eq!(first.for_viewer(STATE), first.for_viewer(WHOLE));
        let whole: Value = serde_json::from_str(second.for_viewer(WHOLE))?;
        assert_eq!(whole, json!({"t": "state", "state": *m.state, "rev": second.rev}));
        // As what changed: the activity set, the pause gone, the skills (most of it) left out.
        let patch: Value = serde_json::from_str(second.for_viewer(STATE))?;
        assert_eq!(patch, json!({"t": "state_patch", "set": {"activity": "editing"}, "gone": ["pause"], "rev": second.rev}));
        assert!(second.for_viewer(STATE).len() * 20 < second.for_viewer(WHOLE).len());
        // Applied to what came first, it makes the state now.
        let mut had: Value = serde_json::from_str(first.for_viewer(STATE))?;
        let had = had.get_mut("state").and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("no state"))?;
        for (k, v) in patch.get("set").and_then(Value::as_object).into_iter().flatten() {
            had.insert(k.clone(), v.clone());
        }
        for k in patch.get("gone").and_then(Value::as_array).into_iter().flatten() {
            had.remove(k.as_str().unwrap_or_default());
        }
        assert_eq!(Value::Object(had.clone()), *m.state);
        Ok(())
    }

    #[test]
    fn states_without_viewers_do_not_serialize_or_keep_a_patch_baseline() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        let (viewer, _) = m.feed.attach(Patches::default());
        set_state(&mut m, json!({"status": "running"}));
        assert!(m.state_frame().is_some());
        drop(viewer);
        set_state(&mut m, json!({"status": "running", "activity": "editing"}));
        assert!(m.state_frame().is_none());
        assert!(m.sent.is_null());
        let (_viewer, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let snapshot = Snapshot { parts: m.parts(), files: None }.frames();
        assert!(snapshot.iter().any(|f| serde_json::from_str::<Value>(f).ok() == Some(json!({"t": "state", "state": *m.state}))));
        set_state(&mut m, json!({"status": "idle"}));
        let frame = m.state_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(frame);
        let frames = on_feed(&bus, &m, at)?;
        let [frame] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // A field that appeared while nobody watched is absent from the whole state too.
        assert!(frame.patch.is_none());
        assert_eq!(serde_json::from_str::<Value>(frame.for_viewer(STATE))?, json!({"t": "state", "state": *m.state, "rev": frame.rev}));
        Ok(())
    }

    /// A viewer that takes changesets as patches too.
    const CHANGES: Patches = Patches { state: true, changes: true };

    /// A file of a changeset, as its JSON goes.
    fn file(path: &str, tree: Option<&str>, added: u32) -> Value {
        let mut f = json!({"path": path, "added": added, "removed": 0, "hunks": [{"old_start": 1, "old_lines": 0, "new_start": 1, "new_lines": added, "lines": vec!["+x"; added as usize]}], "at": 1});
        if let (Some(t), Some(o)) = (tree, f.as_object_mut()) {
            o.insert("tree".into(), json!(t));
        }
        f
    }

    /// A changeset's patch is taken in place: a file set replaces the one with its key where it
    /// is (the same path in another working tree is another file), a new one comes after the
    /// rest, and one gone goes.
    #[test]
    fn a_changesets_patch_is_taken_in_place() {
        let mut files = entries(vec![file("a.rs", None, 1), file("b.rs", None, 1), file("a.rs", Some("/w"), 1)]);
        merge_changes(&mut files, vec![file("c.rs", None, 1), file("a.rs", Some("/w"), 5)], &[json!({"path": "b.rs"})]);
        let values: Vec<Value> = files.iter().map(|f| f.value.clone()).collect();
        assert_eq!(values, vec![file("a.rs", None, 1), file("a.rs", Some("/w"), 5), file("c.rs", None, 1)]);
        // Many set at once (a changeset sent whole) go by key the same way.
        let mut files = entries((0..20).map(|n| file(&format!("f{n}.rs"), None, 1)).collect());
        let set: Vec<Value> = (10..30).rev().map(|n| file(&format!("f{n}.rs"), None, 2)).collect();
        merge_changes(&mut files, set, &[]);
        let paths: Vec<String> = files.iter().filter_map(|f| f.get("path").and_then(Value::as_str).map(str::to_owned)).collect();
        let want: Vec<String> = (0..20).chain((20..30).rev()).map(|n| format!("f{n}.rs")).collect();
        assert_eq!(paths, want);
        assert!(files.iter().skip(10).all(|f| f.get("added") == Some(&json!(2))));
    }

    /// A changeset goes to its viewers whole and as what changed in it: the file the agent touched
    /// and the one it put back, not every file's diff again. A viewer that takes only states as
    /// patches (one from before changesets had them) gets it whole.
    #[test]
    fn a_changeset_goes_out_whole_and_as_what_changed_in_it() -> anyhow::Result<()> {
        let bus = sim_bus();
        let mut m = Mirror::new(Info::default(), Location::Local, Arc::clone(&bus));
        // One viewer from before changesets had patches: changesets go whole for it.
        let (_viewer, at) = m.feed.attach(STATE);
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let many: Vec<Value> = (0..40).map(|n| file(&format!("src/f{n}.rs"), None, 50)).collect();
        m.take_changes(many, Vec::new(), true);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        m.take_changes(vec![file("src/f3.rs", None, 51), file("src/new.rs", None, 2)], vec![json!({"path": "src/f7.rs"})], false);
        let f = m.changes_frame().ok_or_else(|| anyhow::anyhow!("no viewer frame"))?;
        m.feed.publish_payload(f);
        let frames = on_feed(&bus, &m, at)?;
        let [first, second] = frames.as_slice() else { anyhow::bail!("{} frames on the feed", frames.len()) };
        // The first went whole: there was nothing before it to patch.
        assert!(first.patch.is_none());
        assert_eq!(serde_json::from_str::<Value>(second.for_viewer(STATE))?, json!({"t": "changes", "files": changes_of(&m), "rev": second.rev}));
        let patch: Value = serde_json::from_str(second.for_viewer(CHANGES))?;
        assert_eq!(patch, json!({"t": "changes_patch", "set": [file("src/f3.rs", None, 51), file("src/new.rs", None, 2)], "gone": [{"path": "src/f7.rs"}], "rev": second.rev}));
        assert!(second.for_viewer(CHANGES).len() * 10 < second.for_viewer(STATE).len());
        // Taken into what came first as the copy takes one, it makes the changeset now, in order.
        let mut had = entries(serde_json::from_str::<Value>(first.for_viewer(CHANGES))?.get("files").and_then(Value::as_array).cloned().unwrap_or_default());
        let set = patch.get("set").and_then(Value::as_array).cloned().unwrap_or_default();
        let gone = patch.get("gone").and_then(Value::as_array).cloned().unwrap_or_default();
        merge_changes(&mut had, set, &gone);
        assert_eq!(Value::Array(had.iter().map(|f| f.value.clone()).collect()), changes_of(&m));
        Ok(())
    }

    /// A changeset its viewers couldn't make from theirs by taking a patch (the same files in
    /// another order) goes whole.
    #[test]
    fn a_changeset_in_another_order_goes_whole() {
        let was = json!([file("a.rs", None, 1), file("b.rs", None, 1)]);
        assert!(changes_patch(&was, &json!([file("b.rs", None, 1), file("a.rs", None, 1)])).is_none());
        assert!(changes_patch(&Value::Null, &was).is_none());
        let patch = changes_patch(&was, &json!([file("a.rs", None, 2), file("b.rs", None, 1)])).unwrap_or_default();
        assert_eq!(serde_json::from_str::<Value>(&patch).ok(), Some(json!({"t": "changes_patch", "set": [file("a.rs", None, 2)]})));
    }

    /// The ids of the events a viewer attaching to `id` gets, in order.
    fn copy_of(hub: &Hub, id: &str) -> Vec<String> {
        hub.harness.attach(id).map(|a| ids(&a.frames())).unwrap_or_default()
    }

    /// What a live viewer was sent on its session's feed since it last looked (from `at`, moved past
    /// what it read).
    fn sent(hub: &Hub, topic: &str, at: &mut u64) -> Vec<String> {
        use kiln_seams::Bus as _;
        let Ok(read) = futures::executor::block_on(hub.bus.read(topic, *at, 1000)) else { return Vec::new() };
        *at = read.next;
        read.messages.iter().filter_map(|m| super::super::live::decode(&m.payload)).map(|f| f.text.to_string()).collect()
    }

    /// CP-T3.2 (the thirteenth and fourteenth adversarial checks'): what kiln's own words (a goal
    /// stack's, a parent's news) said when their harness was handed them is kept on the session's
    /// info (`queued::Kept::look` reads it): from its runner's state (`State::given`), and from its
    /// user events (a runner from before says no `given`), once; another's message isn't. A runner
    /// that comes back (an Open) or started again (its state saying less) takes none of it away.
    #[test]
    fn kiln_s_own_words_handed_to_a_harness_are_noted_with_what_they_said() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, Given, State};
        use crate::harness::queued::stable;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = || Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None };
            hub.harness.ingest(&hub, &there, "s", open());
            let user = |id: &str, seq: u64, text: &str| Event {
                id: id.into(),
                seq,
                at: 1,
                parent: None,
                by: None,
                body: Body::User { text: text.into(), images: 0, client_id: Some(id.into()), origin: None, attachments: Vec::new(), reply_to: None },
            };
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass"), user("m-person", 2, "hello"), user("news-1", 3, "one child settled")] });
            // Upserted again (its delivery's state), with other words: what it was handed stays.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass, edited later")] });
            // Its runner's state says another handed.
            let given = |g: &[(&str, &str)]| Update::State { state: Box::new(State { given: g.iter().map(|(m, t)| Given { id: (*m).into(), digest: stable(t) }).collect(), ..State::default() }) };
            hub.harness.ingest(&hub, &there, "s", given(&[("goal-2", "/goal docs build")]));
            let want = vec![
                Given { id: "goal-1".into(), digest: stable("/goal tests pass") },
                Given { id: "news-1".into(), digest: stable("one child settled") },
                Given { id: "goal-2".into(), digest: stable("/goal docs build") },
            ];
            let said = || hub.harness.info("s").map(|i| i.said).unwrap_or_default();
            assert_eq!(said(), want);
            // Its runner comes back, then says less (started again): all of it stays.
            hub.harness.ingest(&hub, &there, "s", open());
            hub.harness.ingest(&hub, &there, "s", given(&[]));
            assert_eq!(said(), want, "kept across the Open and a state that says less");
            Ok(())
        })
    }

    /// A runner that started again goes on from what the copy has (`crate::wire::sums`): the copy
    /// keeps its events through the Open's `since` and its viewers go on with what they have. Past
    /// the runner's newest the copy lets go, and an event under a number the copy has under
    /// another id takes its place: its viewers start over from the copy then, sent back by the
    /// feed's resync marker to a snapshot, since what they had can't be resumed.
    #[test]
    fn a_copy_goes_on_from_its_machines_open_and_its_viewers_with_it() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |since| Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 3, ..Info::default() }), since };
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            let names = |r: std::ops::RangeInclusive<u64>| r.map(|n| format!("m{n}")).collect::<Vec<_>>();
            hub.harness.ingest(&hub, &there, "s", open(None));
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: (1..=10).map(said).collect() });
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Watching, so the feed publishes; and where on its topic the viewer reads from.
            let (_watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            let resync = |heard: &[String]| heard.iter().any(|f| f == crate::harness::live::RESYNC);
            // Its machine again, with all ten: the copy goes on, and its viewer only hears what follows.
            hub.harness.ingest(&hub, &there, "s", open(Some(10)));
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(11)] });
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.len() == 1 && !heard.iter().any(|f| f.contains(r#""t":"open""#)) && !resync(&heard), "the viewer goes on: {heard:?}");
            let had = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default();
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=11));
            // Again, with only eight of them (its store hadn't kept the rest): the copy lets go of
            // nine and up, and its viewer starts over from the copy: told to by the feed, and what it
            // had can't be resumed, so it's sent the copy whole.
            hub.harness.ingest(&hub, &there, "s", open(Some(8)));
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=8));
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(resync(&heard), "the viewer is sent back to a snapshot: {heard:?}");
            let again = hub.harness.frames_since("s", had).ok_or_else(|| anyhow::anyhow!("no copy"))?.0.frames();
            anyhow::ensure!(again.first().is_some_and(|f| f.contains(r#""t":"open""#)) && ids(&again) == names(1..=8), "the viewer starts over: {again:?}");
            let kept = hub.harness.kept_on("box");
            anyhow::ensure!(kept.len() == 1 && kept.iter().all(|k| (k.epoch, k.from, k.seq, k.last.len()) == (3, 1, 8, 8)), "{kept:?}");
            // An event sent again under a number the copy has under another id: in its place.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![Event { id: "other".into(), ..said(5) }] });
            let mut want = names(1..=8);
            if let Some(slot) = want.get_mut(4) {
                "other".clone_into(slot);
            }
            anyhow::ensure!(copy_of(&hub, "s") == want, "{:?}", copy_of(&hub, "s"));
            anyhow::ensure!(resync(&sent(&hub, &topic, &mut at)), "the viewer starts over");
            // One it lacks under a number before its newest: in among the others, by number.
            hub.harness.ingest(&hub, &there, "s", open(Some(8)));
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(12), said(14)] });
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(13)] });
            want.extend(names(12..=14));
            anyhow::ensure!(copy_of(&hub, "s") == want, "{:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// A session this server opened on a machine isn't ended by a report the machine made before
    /// the open got there; one it had announced is, once a report leaves it out.
    #[test]
    fn a_report_made_before_an_open_got_there_doesnt_end_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            let bus: Arc<dyn kiln_seams::Bus> = Arc::new(hub.bus.clone());
            hub.harness.mirrors.lock().insert("opened".into(), Mirror { opened_at: Some(std::time::Instant::now()), ..Mirror::new(info("opened"), there.clone(), bus) });
            hub.harness.ingest(&hub, &there, "announced", Update::Open { info: Box::new(info("announced")), since: None });
            let att = hub.harness.attach("announced").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let at = att.at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default());
            let left: Vec<String> = hub.harness.mirrors.lock().keys().cloned().collect();
            anyhow::ensure!(left == vec!["opened".to_owned()], "{left:?}");
            // Its viewer hears it's gone: its feed's last frame.
            let read = futures::executor::block_on(kiln_seams::Bus::read(&hub.bus, &att.topic, at, 100))?;
            let said: Vec<String> = read.messages.iter().filter_map(|m| super::super::live::decode(&m.payload)).map(|f| f.text.to_string()).collect();
            anyhow::ensure!(said.last().is_some_and(|t| t == super::super::live::closed()), "its viewer heard {said:?}");
            // Its machine announces it: from then on its reports count.
            hub.harness.ingest(&hub, &there, "opened", Update::Open { info: Box::new(info("opened")), since: None });
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default());
            anyhow::ensure!(hub.harness.mirrors.lock().is_empty());
            Ok(())
        })
    }

    /// A runner process's report speaks for the sessions it holds: those another process of its
    /// machine holds stay, listed away while that process isn't connected, never ended (a second
    /// runner on adacachy once ended every agent of the first, which was away).
    #[test]
    fn a_report_never_ends_what_another_process_of_the_machine_holds() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            for id in ["old-1", "old-2", "mine"] {
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(info(id)), since: None });
            }
            hub.harness.held_by("old-1", "01a14794");
            hub.harness.held_by("old-2", "01a14794");
            hub.harness.held_by("mine", "main");
            let report = |ids: &[&str]| crate::harness::host::Inventory { sessions: ids.iter().map(|s| (*s).to_owned()).collect(), ..Default::default() };
            let left = |hub: &crate::hub::Hub| {
                let mut v: Vec<String> = hub.harness.mirrors.lock().keys().cloned().collect();
                v.sort();
                v
            };
            // `main` says what it holds while the other process is away.
            hub.harness.inventory(&hub, "box", Some("main"), &["main".to_owned()], report(&["mine"]));
            anyhow::ensure!(left(&hub) == ["mine", "old-1", "old-2"], "{:?}", left(&hub));
            let status = |id: &str| hub.harness.info(id).map(|i| i.status);
            anyhow::ensure!(status("old-1") == Some(Status::Disconnected) && status("old-2") == Some(Status::Disconnected), "away: {:?} {:?}", status("old-1"), status("old-2"));
            anyhow::ensure!(status("mine") != Some(Status::Disconnected), "{:?}", status("mine"));
            // The process that holds them says it holds one of them: the other ends.
            hub.harness.inventory(&hub, "box", Some("01a14794"), &["main".to_owned(), "01a14794".to_owned()], report(&["old-1"]));
            anyhow::ensure!(left(&hub) == ["mine", "old-1"], "{:?}", left(&hub));
            anyhow::ensure!(hub.harness.holder("old-2").is_none(), "its holder goes with it");
            // Who holds what is kept for the next start: written on the blocking pool, soon after.
            let read = || -> Option<HashMap<String, String>> { serde_json::from_slice(&std::fs::read(hub.harness.dir.join("holders.json")).ok()?).ok() };
            let is_now = |k: &HashMap<String, String>| k.get("old-1").map(String::as_str) == Some("01a14794") && !k.contains_key("old-2");
            let mut kept = read().unwrap_or_default();
            for _ in 0..200 {
                if is_now(&kept) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(25)).await;
                kept = read().unwrap_or_default();
            }
            anyhow::ensure!(is_now(&kept), "{kept:?}");
            Ok(())
        })
    }

    /// A burst of a changeset's patches (a runner sending what it held after a cut) is kept in the
    /// conversation store as the changeset they make, as the copy has it: the whole changeset isn't
    /// queued for the store at each patch (most of a megabyte each, for a long session).
    #[test]
    fn a_burst_of_patches_is_stored_as_the_changeset_they_make() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let store = hub.harness.convo.clone().ok_or_else(|| anyhow::anyhow!("no conversation store"))?;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None });
            let change = |n: u32| crate::harness::model::FileChange { path: format!("src/f{}.rs", n % 7), tree: None, created: false, deleted: false, added: n, removed: 0, hunks: Vec::new(), truncated: false, at: i64::from(n), by: Vec::new(), place: None };
            for n in 0..200 {
                hub.harness.ingest(&hub, &there, "s", Update::ChangesPatch { set: vec![change(n)], gone: Vec::new() });
            }
            // Each file in the place its first patch gave it, as its last patch has it.
            let want = serde_json::to_value([196, 197, 198, 199, 193, 194, 195].map(change))?;
            let mut kept = None;
            for _ in 0..200 {
                let st = store.clone();
                kept = crate::rt::blocking(move || st.kept("s", 0)).await??.and_then(|k| k.changes).and_then(|c| serde_json::from_str::<Value>(&c).ok());
                if kept.as_ref() == Some(&want) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(25)).await;
            }
            anyhow::ensure!(kept.as_ref() == Some(&want), "{kept:?}");
            Ok(())
        })
    }

    /// W27-R08 (the twin): a runner's report names the sessions it holds a copy of that went on
    /// elsewhere at a later epoch, which it's told to let go of; not one just arriving there (its
    /// Open on its way, at the epoch it goes on at), not one it runs, and none from a runner that
    /// says no epochs.
    #[test]
    fn w27_r08_a_report_names_the_copies_left_behind() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let info = |id: &str, epoch: u64| Info { id: id.into(), kind: "claude".into(), epoch, ..Info::default() };
            // "twin" went on on box-b at epoch 1, its copy left on box-a; "arriving" is on its way
            // from box-b to box-a, at epoch 1 there; "here" runs on box-a.
            let (a, b) = (Location::Machine("box-a".into()), Location::Machine("box-b".into()));
            hub.harness.ingest(&hub, &b, "twin", Update::Open { info: Box::new(info("twin", 1)), since: None });
            hub.harness.ingest(&hub, &b, "arriving", Update::Open { info: Box::new(info("arriving", 0)), since: None });
            hub.harness.ingest(&hub, &a, "here", Update::Open { info: Box::new(info("here", 0)), since: None });
            let report = |held: &[(&str, u64)]| crate::harness::host::Inventory { sessions: held.iter().map(|(s, _)| (*s).to_owned()).collect(), epochs: held.iter().map(|(s, e)| ((*s).to_owned(), *e)).collect(), ..Default::default() };
            assert_eq!(hub.harness.left_behind("box-a", &report(&[("twin", 0), ("arriving", 1), ("here", 0)])), ["twin"]);
            let before = crate::harness::host::Inventory { sessions: vec!["twin".into()], ..Default::default() };
            assert!(hub.harness.left_behind("box-a", &before).is_empty(), "a runner from before says no epochs");
            Ok(())
        })
    }

    /// A runner's link dropped and came back to the same server: the sessions it holds that have
    /// said nothing since still read disconnected, offline while their machine is connected (19
    /// agents on three machines, 2026-11-01). Its report finds them, for their snapshots.
    #[test]
    fn a_report_finds_its_sessions_that_still_read_disconnected() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let info = |id: &str| Info { id: id.into(), kind: "claude".into(), ..Info::default() };
            for id in ["quiet", "busy"] {
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(info(id)), since: None });
            }
            hub.harness.ingest(&hub, &Location::Machine("elsewhere".into()), "away", Update::Open { info: Box::new(info("away")), since: None });
            hub.harness.machine_lost(&hub, "box");
            hub.harness.machine_lost(&hub, "elsewhere");
            // Back: one has something to say at once, the other is quiet.
            hub.harness.ingest(&hub, &there, "busy", Update::Open { info: Box::new(info("busy")), since: None });
            let held = ["quiet", "busy", "away", "unknown"].map(str::to_owned);
            let stale = hub.harness.still_disconnected("box", &held);
            anyhow::ensure!(stale == ["quiet"], "only its own quiet session: {stale:?}");
            Ok(())
        })
    }

    #[test]
    fn names_are_kept_across_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Registry::new(dir.path().to_owned());
        r.names.lock().insert("s1".into(), "Mantle".into());
        r.save_names();
        // Written off the calling thread, soon after (goal 3, plan 01a1a59c2a8e step 5).
        let kept = || Registry::new(dir.path().to_owned()).names.lock().get("s1").cloned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while kept().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(kept().as_deref(), Some("Mantle"));
        Ok(())
    }

    /// Goal 3, plan 01a1a59c2a8e step 5 (perf/api-pass's leftover): an agent's name and its epoch
    /// are kept without the caller waiting on the disk. Here their files' writes wait (each `.part`
    /// a named pipe nobody reads yet): keeping a name and raising an epoch return at once all the
    /// same, where they used to wait on the write; read, each pipe then holds what was kept.
    #[cfg(unix)]
    #[test]
    fn g3_s5_names_and_epochs_are_kept_without_waiting_on_the_disk() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for file in ["names.json.part", "epochs.json.part"] {
            let made = std::process::Command::new("mkfifo").arg(dir.path().join(file)).status()?;
            anyhow::ensure!(made.success(), "mkfifo {file}");
        }
        let r = Arc::new(Registry::new(dir.path().to_owned()));
        let (done, kept) = std::sync::mpsc::channel();
        let keeper = Arc::clone(&r);
        std::thread::spawn(move || {
            keeper.names.lock().insert("s1".into(), "Mantle".into());
            keeper.save_names();
            keeper.raise_epoch("s1", 7);
            let _ = done.send(());
        });
        anyhow::ensure!(kept.recv_timeout(std::time::Duration::from_secs(10)).is_ok(), "keeping a name and an epoch waited on the disk");
        // Each pipe read: what the writer, waiting on it, was handed.
        let names: HashMap<String, String> = serde_json::from_slice(&std::fs::read(dir.path().join("names.json.part"))?)?;
        anyhow::ensure!(names.get("s1").map(String::as_str) == Some("Mantle"), "{names:?}");
        let epochs: HashMap<String, u64> = serde_json::from_slice(&std::fs::read(dir.path().join("epochs.json.part"))?)?;
        anyhow::ensure!(epochs.get("s1") == Some(&7), "{epochs:?}");
        Ok(())
    }
}
