//! The server's view of every agent session, wherever it runs: its own (`local`) and each
//! runner's. A copy per session (the latest state, meta and timeline, as JSON) serves any number
//! of viewers on any device, its live frames on kiln's bus (`super::live`, docs/bus.md); their
//! commands go to the machine that runs the session. Each copy belongs to one shard, which alone
//! changes it, and is read from the views that shard publishes (`super::copies`): no lock guards
//! any session's copy (goal 4, plan 01a1a59c2a9c; plan 01a17197 step 9).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize as _;
use serde_json::{Value, json};

use crate::rcu::Rcu;

use super::copies::{Copies, Ingest, Mirror, Msg, Shard};
use super::core::{MAX_EVENTS, Update};
use super::files;
use super::inbox::{self, Answer, Said};
use super::kept::{Job, Keeper, Kept};
use super::host::{Host, Inventory, Limits};
use super::kinds::KindInfo;
use super::model::{Command, State, Status};
use super::session::{Held, Info, MAX_NAME, Spec, SubagentBrief};
use super::live::Viewer;
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

/// One of a copy's events, or one of its changeset's files: its JSON as frames carry it, written out once
/// as it comes in (every viewer's snapshot, every live frame and every digest take that as it is), and
/// what's read of it without the JSON: an event's id, number and kind and whether it's on its session's
/// main thread, a file's path and tree. Its JSON's tree isn't kept: a long session's copy kept every
/// event's beside its text, 8,260 bytes for each event it kept where its JSON is about 3,500 (goal 11, plan
/// 01a1a87b2e56 step 2; `tests/session_resident.rs`). One that text is streamed into is held as its tree
/// while that goes on, and written out again when next asked for.
pub struct Entry {
    body: Body,
    head: Head,
    /// Its digest as a runner that starts again compares it (`crate::wire::sums`), once asked for.
    sum: std::sync::OnceLock<u64>,
}

/// An entry's JSON: as written out, or its tree while text is streamed into it.
#[derive(Clone)]
enum Body {
    Text(Box<str>),
    Changing(Box<Changing>),
}

/// An entry text is being streamed into: its tree, and its JSON once asked for again.
#[derive(Clone)]
struct Changing {
    value: Value,
    text: std::sync::OnceLock<Box<str>>,
}

/// What's read of an entry without its JSON.
#[derive(Clone, Default)]
struct Head {
    id: Option<Box<str>>,
    seq: Option<u64>,
    kind: Option<std::borrow::Cow<'static, str>>,
    /// An event of its session's main thread: no `parent`, or a null one.
    main: bool,
    /// A changeset file's path and tree ([`file_ref`]).
    file: Option<Box<(Option<Box<str>>, Option<Box<str>>)>>,
}

/// The kinds events have (`super::model::Body`'s tags), kept without a copy each.
const KINDS: [&str; 5] = ["user", "assistant", "thinking", "tool", "notice"];

impl Head {
    fn of(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str);
        let kind = s("kind").map(|k| KINDS.iter().find(|known| **known == k).map_or_else(|| std::borrow::Cow::Owned(k.to_owned()), |known| std::borrow::Cow::Borrowed(*known)));
        let file = (v.get("path").is_some() || v.get("tree").is_some()).then(|| Box::new((s("path").map(Box::from), s("tree").map(Box::from))));
        Self { id: s("id").map(Box::from), seq: v.get("seq").and_then(Value::as_u64), kind, main: v.get("parent").is_none_or(Value::is_null), file }
    }

    /// [`Head::of`] an event's JSON, read from the event itself: its kind is its body's tag (`super::model::Body`
    /// is tagged `kind`, in snake case), and an event's JSON has no changeset file's path or tree of its own.
    fn of_event(e: &super::model::Event) -> Self {
        use super::model::Body as B;
        let kind = match &e.body {
            B::User { .. } => "user",
            B::Assistant { .. } => "assistant",
            B::Thinking { .. } => "thinking",
            B::Tool(_) => "tool",
            B::Notice { .. } => "notice",
        };
        Self { id: Some(e.id.as_str().into()), seq: Some(e.seq), kind: Some(std::borrow::Cow::Borrowed(kind)), main: e.parent.is_none(), file: None }
    }
}

impl Entry {
    /// `value`, its JSON written out now.
    pub(super) fn new(value: Value) -> Self {
        let text = serde_json::to_string(&value).unwrap_or_default();
        Self::written(value, text)
    }

    /// An entry as `text`, its JSON, says (an event of a compact chunk read again): what's looked up of it
    /// read from it.
    pub(super) fn from_text(text: &str) -> Self {
        let head = serde_json::from_str::<Value>(text).map(|v| Head::of(&v)).unwrap_or_default();
        crate::memory::SESSION_ENTRIES.add(text.len());
        Self { head, body: Body::Text(text.into()), sum: std::sync::OnceLock::new() }
    }

    /// `value` as `text` writes it (made already, for a live frame): the text kept, the tree let go of.
    pub(super) fn written(value: Value, text: String) -> Self {
        crate::memory::SESSION_ENTRIES.add(text.len());
        Self { head: Head::of(&value), body: Body::Text(text.into_boxed_str()), sum: std::sync::OnceLock::new() }
    }

    /// Event `e` as a copy keeps it, written out once, straight from the event (goal 9, plan 01a1a87b2cec
    /// step 5: its owner wrote each event into a JSON tree, then the tree out, then the event again for the
    /// store's row). It's the text the tree wrote, as a runner's digests of it take it (`crate::agent`):
    /// serde_json's maps keep their keys in the order they come (its `preserve_order`, which kiln's build
    /// has). What's looked up of it is read from the event ([`Head::of_event`]); its text goes to `keep`
    /// as well (the conversation store's row), when anyone keeps it. One that can't be written out is
    /// `null`, as before.
    pub(super) fn of_event(e: &super::model::Event, keep: Option<&mut dyn FnMut(String)>) -> Self {
        let Ok(text) = serde_json::to_string(e) else { return Self::written(Value::Null, "null".to_owned()) };
        crate::memory::SESSION_ENTRIES.add(text.len());
        let body: Box<str> = match keep {
            Some(keep) => {
                let body = Box::from(text.as_str());
                keep(text);
                body
            }
            None => text.into_boxed_str(),
        };
        Self { head: Head::of_event(e), body: Body::Text(body), sum: std::sync::OnceLock::new() }
    }

    /// The bytes it holds as text, as `crate::memory::SESSION_ENTRIES` counts them.
    fn counted(&self) -> usize {
        match &self.body {
            Body::Text(t) => t.len(),
            Body::Changing(_) => 0,
        }
    }

    /// Its JSON as frames carry it: `serde_json`'s compact form of its value.
    pub fn text(&self) -> &str {
        match &self.body {
            Body::Text(t) => t,
            Body::Changing(c) => c.text.get_or_init(|| serde_json::to_string(&c.value).unwrap_or_default().into_boxed_str()),
        }
    }

    /// Its digest, of its text (`crate::wire::sums::event_text`): made once, however many ask.
    pub fn sum(&self) -> u64 {
        *self.sum.get_or_init(|| crate::wire::sums::event_text(self.text()))
    }

    /// Its id: what the copy finds an event by.
    pub fn id(&self) -> Option<&str> {
        self.head.id.as_deref()
    }

    /// Its number in its session's timeline.
    pub fn seq(&self) -> Option<u64> {
        self.head.seq
    }

    /// An event's kind (`user`, `assistant`, `thinking`, `tool`, `notice`).
    pub fn kind(&self) -> Option<&str> {
        self.head.kind.as_deref()
    }

    /// Whether it's an event of its session's main thread (one of no subagent's).
    pub fn on_main_thread(&self) -> bool {
        self.head.main
    }

    /// Whether its JSON is `null`: an event that couldn't be written out as JSON, nothing for a copy to
    /// take.
    pub fn is_null(&self) -> bool {
        self.text() == "null"
    }

    /// A changeset file's path and tree, as [`file_ref`] names a file's JSON.
    pub(super) fn file(&self) -> FileRef<'_> {
        self.head.file.as_deref().map_or((None, None), |(path, tree)| (path.as_deref(), tree.as_deref()))
    }

    /// Its JSON's tree, read again from its text: for what reads more of it than the above (an agent's
    /// conclusion, a changeset as JSON).
    pub fn value(&self) -> Value {
        match &self.body {
            Body::Text(t) => serde_json::from_str(t).unwrap_or(Value::Null),
            Body::Changing(c) => c.value.clone(),
        }
    }

    /// `more` added to its `text`, as a runner streams it: its tree held while that goes on, its JSON and
    /// digest written again when next asked for. False when it has no text to add to.
    pub(super) fn append_text(&mut self, more: &str) -> bool {
        if let Body::Text(t) = &self.body {
            let Ok(value) = serde_json::from_str::<Value>(t) else { return false };
            if !value.get("text").is_some_and(Value::is_string) {
                return false;
            }
            crate::memory::SESSION_ENTRIES.sub(t.len());
            self.body = Body::Changing(Box::new(Changing { value, text: std::sync::OnceLock::new() }));
        }
        let Body::Changing(c) = &mut self.body else { return false };
        let Some(Value::String(s)) = c.value.get_mut("text") else { return false };
        s.push_str(more);
        c.text = std::sync::OnceLock::new();
        self.sum = std::sync::OnceLock::new();
        true
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        crate::memory::SESSION_ENTRIES.add(self.counted());
        Self { body: self.body.clone(), head: self.head.clone(), sum: self.sum.clone() }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        crate::memory::SESSION_ENTRIES.sub(self.counted());
    }
}

/// A copy's changeset: its files in order, each shared, so its snapshots and patches take every
/// file that didn't change as it is (no copy, no compare, no writing it out again).
pub(super) type Files = Arc<Vec<Arc<Entry>>>;

/// The session socket's frames, written straight out (into a frame's buffer, a bus payload, a
/// snapshot's gzip): `{"t":<tag>,<fields>}`, then `"rev"` last when there is one, byte for byte as
/// [`SessionDown`](crate::wire::ui::SessionDown) says them (`frames_are_written_as_the_socket_says_them`),
/// without first copying what they carry into one. Writes to a `Vec` can't fail; to a gzip
/// encoder, they fail as it does.
pub(super) mod write {
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

    /// `{"t":"events","events":[…]}` of texts already joined by `,` (a compact chunk's: goal 11).
    pub fn joined_events(w: &mut impl Write, joined: &[u8], rev: Option<u64>) -> io::Result<()> {
        head(w, "events", Some("events"))?;
        w.write_all(b"[")?;
        w.write_all(joined)?;
        w.write_all(b"]")?;
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

/// This server process's incarnation: viewers resume from a copy's revision only under it. 53
/// bits: it travels as a JSON number, which a browser reads exactly only up to 2^53.
pub fn born() -> u64 {
    static BORN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BORN.get_or_init(|| rand::random::<u64>() >> 11)
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
pub(super) fn null() -> Arc<Value> {
    static NULL: std::sync::LazyLock<Arc<Value>> = std::sync::LazyLock::new(|| Arc::new(Value::Null));
    Arc::clone(&NULL)
}

/// A snapshot of a copy, or what a viewer lacks of it since a revision, not yet made into frames
/// (`super::copies::View::parts`, `View::parts_since`): references to what an immutable view holds.
pub struct Parts {
    /// Its info, made into the head's JSON where the snapshot is written (off the shard that takes
    /// it: goal 10, plan 01a1a87b2dae step 3).
    pub(super) info: Arc<Info>,
    pub(super) meta: Arc<Value>,
    pub(super) state: Arc<Value>,
    pub(super) changes: Option<Files>,
    pub(super) events: super::copies::Events,
    /// Since a revision: only the events changed after it (it begins with `resumed`). None: the
    /// session whole (it begins with `open`).
    pub(super) since: Option<u64>,
    /// Its meta's and state's last frames' sizes: the room theirs take.
    pub(super) meta_bytes: usize,
    pub(super) state_bytes: usize,
    /// The copy's revision as of the parts.
    pub(super) rev: u64,
}

/// A snapshot's events go in frames of about this many bytes.
const EVENTS_FRAME: usize = 256 * 1024;

/// A viewer's snapshot (or catch-up), taken from a copy's view but not written out: written on the
/// blocking pool, off the runtime's threads, a long session's being megabytes of JSON
/// (`crate::rt::blocking`). Its frames say the copy as of when it was taken, whatever comes in
/// meanwhile.
pub struct Snapshot {
    pub(super) parts: Parts,
    /// What's kept of the files its messages mention (`Keeper::frame`): ahead of the events.
    pub(super) files: Option<String>,
    /// Where a spilled chunk whose file doesn't give it back is read from instead (`super::spill`; goal 11,
    /// plan 01a1a87b2e56 step 5): the conversation store, which keeps every event of the session, by its
    /// events' numbers, with the session's id. None where the server keeps no conversations (its runners
    /// keep each session whole), and in tests.
    pub(super) store: Option<(crate::convo::store::Store, Arc<str>)>,
}

/// A whole snapshot's events at the most: as many as a copy keeps before it lets its oldest go
/// (`MAX_EVENTS` and a quarter: `super::copies::Mirror::upsert`), at the packed size an event measured
/// (`super::copies::PACKED_EVENT_BYTES`, research f20), 3.2 MB (goal 11, plan 01a1a87b2e56 step 3).
const SNAPSHOT_BYTES: usize = (super::core::MAX_EVENTS + super::core::MAX_EVENTS / 4) * super::copies::PACKED_EVENT_BYTES;

/// The room of viewers' snapshots in flight, made and not yet written to their sockets
/// (`crate::memory::SNAPSHOTS`; goal 11, plan 01a1a87b2e56 step 3): as many as the blocking pool makes at
/// once (its `fs` share's places, which the server's profile takes from the lab's measure,
/// `crate::rt::SERVER_FS`: 26), each a whole snapshot's events at the most ([`SNAPSHOT_BYTES`]), 84 MB on
/// the server. At the bound an attach waits its turn ([`InFlight::take`]: a task's wait, its shard goes on
/// with its other work) until one in flight is written out; one larger than the room goes alone.
pub fn snapshot_room() -> &'static crate::memory::queue::Room {
    static ROOM: std::sync::OnceLock<crate::memory::queue::Room> = std::sync::OnceLock::new();
    ROOM.get_or_init(|| crate::memory::queue::Room::new(&crate::memory::SNAPSHOTS, snapshot_bound(crate::rt::profile().pool.2)))
}

/// The room of snapshots in flight for a blocking pool whose `fs` share has `places`.
fn snapshot_bound(places: usize) -> usize {
    places.max(1).saturating_mul(SNAPSHOT_BYTES)
}

/// A snapshot's bytes taken of a room of snapshots in flight ([`snapshot_room`]): what's left of them given
/// back as it goes, however it ends (written, an error, a socket gone).
pub struct InFlight {
    room: &'static crate::memory::queue::Room,
    bytes: usize,
}

impl InFlight {
    /// `bytes` of `room`, waiting for them to fit (a task's wait): none taken once it's closed.
    pub async fn take(room: &'static crate::memory::queue::Room, bytes: usize) -> Self {
        let bytes = if room.take(bytes).await { bytes } else { 0 };
        Self { room, bytes }
    }

    /// The snapshot as it was made, `n` bytes: the room holds that, not the guess it was taken by (more taken
    /// whatever the room holds then, as it's made already; or what wasn't needed given back).
    pub fn made(&mut self, n: usize) {
        match n.checked_sub(self.bytes) {
            Some(more) => self.room.force(more),
            None => self.room.give(self.bytes.saturating_sub(n)),
        }
        self.bytes = n;
    }

    /// `n` of its bytes written to the socket: given back now.
    pub fn written(&mut self, n: usize) {
        let n = n.min(self.bytes);
        self.room.give(n);
        self.bytes = self.bytes.saturating_sub(n);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.room.give(self.bytes);
    }
}

/// One of a snapshot's frames, in the order they go: its head (`open` or `resumed`), its meta,
/// state, changes and kept files, then its events: a whole snapshot's a chunk of the copy's at a time
/// (`super::copies::Chunk`, each packed once: `super::packed`), a catch-up's those it brings a batch at
/// a time (where they are among them).
enum Part {
    Head,
    Meta,
    State,
    Changes,
    Files,
    Events(std::ops::Range<usize>),
    /// The copy's chunk of events at this place among its chunks, whole.
    Chunk(usize),
}

/// What a snapshot written out holds of its frames: where the events it brings are in its parts',
/// and its frames in order.
struct Plan {
    brought: Vec<usize>,
    parts: Vec<Part>,
}

impl Snapshot {
    /// About the bytes it takes made, `packed` as one message or as frames: what it takes of the room of
    /// snapshots in flight before it's made ([`snapshot_room`]; [`InFlight::made`] counts it as it was made).
    /// Packed, each chunk's segment as it's kept (in memory or spilled) and a quarter of the texts of one
    /// that isn't packed (deflate's ratio on them), beside the other frames whole.
    pub fn weight(&self, packed: bool) -> usize {
        let p = &self.parts;
        let frames = p.meta_bytes.saturating_add(p.state_bytes).saturating_add(self.files.as_ref().map_or(0, String::len));
        let events = p.events.chunks().iter().map(|c| {
            let texts = c.metas().last().map_or(0, |m| m.off + m.len);
            match (packed, c.packed(), c.spilled_bytes()) {
                (false, _, _) => texts,
                (true, Some(s), _) => s.deflated.len(),
                (true, None, Some(n)) => n,
                (true, None, None) => texts / 4,
            }
        });
        events.fold(frames, usize::saturating_add)
    }

    /// Its frames, in order. The events it brings are all of them, a frame for each of the copy's
    /// chunks (goal 10, plan 01a1a87b2dae step 3: each chunk's frame is packed once, then taken as it
    /// is), or those changed since its revision, in batches of about [`EVENTS_FRAME`] bytes each (one
    /// alone when it's larger). Planned from the events' metas alone.
    fn plan(&self) -> Plan {
        let p = &self.parts;
        let mut parts = vec![Part::Head];
        if !p.meta.is_null() {
            parts.push(Part::Meta);
        }
        if !p.state.is_null() {
            parts.push(Part::State);
        }
        if p.changes.is_some() {
            parts.push(Part::Changes);
        }
        if self.files.is_some() {
            parts.push(Part::Files);
        }
        if p.since.is_none() {
            parts.extend(p.events.chunks().iter().enumerate().filter(|(_, c)| !c.is_empty()).map(|(i, _)| Part::Chunk(i)));
            return Plan { brought: Vec::new(), parts };
        }
        let brought: Vec<usize> = p.events.metas().enumerate().filter(|(_, m)| p.since.is_none_or(|s| m.at > s)).map(|(i, _)| i).collect();
        let (mut from, mut size) = (0usize, 0usize);
        for (i, at) in brought.iter().enumerate() {
            let n = p.events.meta(*at).map_or(0, |m| m.len);
            if size + n > EVENTS_FRAME && i > from {
                parts.push(Part::Events(from..i));
                from = i;
                size = 0;
            }
            size += n;
        }
        if from < brought.len() {
            parts.push(Part::Events(from..brought.len()));
        }
        Plan { brought, parts }
    }

    /// The events of `batch` (where they are among those `plan` brings).
    fn batch(&self, plan: &Plan, batch: &std::ops::Range<usize>) -> Vec<Arc<Entry>> {
        self.parts.events.entries_at(plan.brought.get(batch.clone()).unwrap_or_default())
    }

    /// About the bytes `part` takes: the room it's written into.
    fn room(&self, plan: &Plan, part: &Part) -> usize {
        let p = &self.parts;
        match part {
            // Its info, written as it's made (a few hundred bytes).
            Part::Head => 0,
            Part::Meta => p.meta_bytes,
            Part::State => p.state_bytes,
            Part::Changes => p.changes.as_ref().map_or(0, |files| write::envelope("changes", Some("files"), None) + write::texts_len(files.iter().map(|f| &**f))),
            Part::Files => self.files.as_ref().map_or(0, String::len),
            Part::Events(batch) => write::envelope("events", Some("events"), None) + write::texts_len(self.batch(plan, batch).iter().map(|e| &**e)),
            // Its texts joined, and the `[` and `]` around them.
            Part::Chunk(i) => write::envelope("events", Some("events"), None) + p.events.chunks().get(*i).and_then(|c| c.metas().last()).map_or(0, |m| m.off + m.len) + "[]".len(),
        }
    }

    /// Writes `part` to `w`.
    fn write(&self, plan: &Plan, part: &Part, w: &mut impl std::io::Write) -> std::io::Result<()> {
        use crate::wire::ui::{SessionDown, SessionFrame};
        let p = &self.parts;
        match part {
            Part::Head => {
                let (info, born) = (serde_json::to_value(&*p.info).unwrap_or(Value::Null), Some(born()));
                let head = SessionDown { frame: if p.since.is_some() { SessionFrame::Resumed { info, born } } else { SessionFrame::Open { info, born } }, rev: Some(p.rev) }.text();
                w.write_all(head.as_bytes())
            }
            Part::Meta => write::value(w, "meta", "meta", &p.meta, None),
            Part::State => write::value(w, "state", "state", &p.state, None),
            Part::Changes => match &p.changes {
                Some(files) => write::changes(w, files.iter().map(|f| &**f), None),
                None => Ok(()),
            },
            Part::Files => w.write_all(self.files.as_deref().unwrap_or_default().as_bytes()),
            Part::Events(batch) => write::events(w, self.batch(plan, batch).iter().map(|e| &**e), None),
            Part::Chunk(i) => match p.events.chunks().get(*i) {
                // A compact chunk's texts are its segment's, as they were joined.
                Some(c) if c.is_compact() => write::joined_events(w, &self.joined(c)?, None),
                Some(c) => write::events(w, c.evs()?.iter().map(|e| &*e.entry), None),
                None => write::events(w, std::iter::empty::<&Entry>(), None),
            },
        }
    }

    /// A compact chunk's texts joined: its segment's, or a spilled one's its file doesn't give back, its
    /// events as the conversation store keeps them ([`Snapshot::read_from_store`]).
    fn joined(&self, c: &super::copies::Chunk) -> std::io::Result<Vec<u8>> {
        match c.joined() {
            Ok(joined) => Ok(joined.into_owned()),
            Err(e) if c.is_spilled() => self.read_from_store(c).map_err(|s| std::io::Error::other(format!("{e}; from the store: {s}"))),
            Err(e) => Err(e),
        }
    }

    /// A spilled chunk whose file didn't give it back (`super::spill` refuses what doesn't read back as
    /// written), its events' texts as the conversation store keeps them, by their numbers, joined as an
    /// `events` frame joins them: the session's events are never lost to a viewer while the store has them
    /// (goal 11, plan 01a1a87b2e56 step 5). Read where the snapshot is made, on the blocking pool.
    fn read_from_store(&self, c: &super::copies::Chunk) -> std::io::Result<Vec<u8>> {
        let (store, id) = self.store.as_ref().ok_or_else(|| std::io::Error::other("no conversation store to read it from"))?;
        let (Some(first), Some(last)) = (c.metas().first().and_then(|m| m.seq), c.metas().last().and_then(|m| m.seq)) else {
            return Err(std::io::Error::other("its events have no numbers to read them by"));
        };
        let n = usize::try_from(last.saturating_sub(first).saturating_add(1)).unwrap_or(usize::MAX);
        let rows = store.events(id, last.checked_add(1), n).map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        tracing::warn!(session = &**id, first, last, "a spilled chunk's file didn't give it back: its events are read from the conversation store");
        let mut joined = Vec::new();
        for (i, r) in rows.iter().filter(|r| r.seq >= first && r.seq <= last).enumerate() {
            if i > 0 {
                joined.push(b',');
            }
            joined.extend_from_slice(r.body.as_bytes());
        }
        Ok(joined)
    }

    /// The frames that bring a viewer to the copy as it was when this was taken, a message each,
    /// written here and now (tests, and what isn't on a shard).
    pub fn frames(&self) -> Vec<String> {
        let plan = self.plan();
        plan.parts.iter().map(|part| write::string(self.room(&plan, part), |b| self.write(&plan, part, b))).collect()
    }

    /// Its frames and then `last` (the socket's `synced`), a frame a line, gzip'd as one message, put
    /// together from pieces (goal 10, plan 01a1a87b2dae step 3; `super::packed`): each chunk of events
    /// as it was packed once ([`super::copies::Chunk::pack`]; the newest, which the next event changes,
    /// deflated now and kept only once it's full), between them the few bytes of its frame stored as
    /// they are, and the other frames deflated as they come. One gzip member: every decoder reads it.
    pub fn packed(&self, last: &str) -> std::io::Result<Vec<u8>> {
        let plan = self.plan();
        let chunks = self.parts.events.chunks();
        let room = chunks.iter().map(|c| c.packed().map_or_else(|| c.metas().last().map_or(0, |m| m.off + m.len) / 4, |s| s.deflated.len())).sum::<usize>();
        let mut gz = super::packed::Gzip::new(room);
        // The frames between the chunks, deflated together.
        let mut loose: Vec<u8> = Vec::new();
        for part in &plan.parts {
            self.part_into(&plan, part, &mut gz, &mut loose)?;
        }
        loose.extend_from_slice(last.as_bytes());
        gz.deflate(flate2::Compression::fast(), &loose)?;
        Ok(gz.finish())
    }

    /// `part` of `plan` as [`Snapshot::packed`] puts it in: a chunk of events after the frames before it (in
    /// `loose`, deflated together), as it was packed (the newest, which the next event changes, deflated now),
    /// between the few bytes of its frame stored as they are; another frame onto `loose`.
    fn part_into(&self, plan: &Plan, part: &Part, gz: &mut super::packed::Gzip, loose: &mut Vec<u8>) -> std::io::Result<()> {
        use flate2::Compression;
        match part {
            Part::Chunk(i) => {
                let chunks = self.parts.events.chunks();
                let Some(chunk) = chunks.get(*i) else { return Ok(()) };
                gz.deflate(Compression::fast(), loose)?;
                loose.clear();
                gz.stored(br#"{"t":"events","events":["#);
                if *i + 1 == chunks.len() && !chunk.is_full() {
                    gz.segment(&chunk.deflate(Compression::fast())?);
                } else {
                    let packed = match chunk.pack() {
                        Ok(packed) => packed,
                        // A spilled one its file doesn't give back: its events as the store keeps them.
                        Err(e) if chunk.is_spilled() => {
                            let joined = self.read_from_store(chunk).map_err(|s| std::io::Error::other(format!("{e}; from the store: {s}")))?;
                            Arc::new(super::packed::deflate(Compression::default(), [joined.as_slice()])?)
                        }
                        Err(e) => return Err(e),
                    };
                    gz.segment(&packed);
                }
                gz.stored(b"]}\n");
            }
            other => {
                loose.reserve(self.room(plan, other).saturating_add(1));
                self.write(plan, other, loose)?;
                loose.push(b'\n');
            }
        }
        Ok(())
    }

    /// It packed as it goes out ([`Packing`]): the message [`Snapshot::packed`] makes at once, made a piece
    /// at a time, `last` its end.
    pub fn packing(self, last: &str) -> Packing {
        let plan = self.plan();
        Packing { snapshot: self, plan, next: 0, gz: Some(super::packed::Gzip::new(0)), loose: Vec::new(), last: last.to_owned() }
    }
}

/// A snapshot packed as it goes out (goal 11, plan 01a1a87b2e56 step 5): its message a piece at a time, each
/// made on the blocking pool and written before the next is made. A viewer that reads slowly holds a piece and
/// the next, not its whole snapshot, nor a place of the pool while it reads: made at once, a long session's
/// packed snapshot (megabytes) was held until its last piece went, and a place held for all of its making.
pub struct Packing {
    snapshot: Snapshot,
    plan: Plan,
    /// Its next part.
    next: usize,
    /// The message being made; none once it's all out.
    gz: Option<super::packed::Gzip>,
    loose: Vec<u8>,
    last: String,
}

impl Packing {
    /// Its next piece: at least `piece` bytes of the message as it's made (the last may be fewer), none once
    /// it's all out. Piece after piece, the bytes [`Snapshot::packed`] makes at once.
    pub fn next_piece(&mut self, piece: usize) -> std::io::Result<Option<Vec<u8>>> {
        let Some(gz) = self.gz.as_mut() else { return Ok(None) };
        while gz.made() < piece {
            let Some(part) = self.plan.parts.get(self.next) else {
                // Every part in: the frames after the last chunk and `last`, then the member's end.
                self.loose.extend_from_slice(self.last.as_bytes());
                gz.deflate(flate2::Compression::fast(), &self.loose)?;
                self.loose = Vec::new();
                return Ok(self.gz.take().map(super::packed::Gzip::finish));
            };
            self.next += 1;
            self.snapshot.part_into(&self.plan, part, gz, &mut self.loose)?;
        }
        Ok(Some(gz.take()))
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
pub(super) fn state_patch_of<'a>(was: &'a Value, now: &'a Value) -> Option<(Vec<(&'a String, &'a Value)>, Vec<&'a String>)> {
    let (Value::Object(was), Value::Object(now)) = (was, now) else { return None };
    let set = now.iter().filter(|(k, v)| was.get(*k) != Some(*v)).collect();
    let gone = was.keys().filter(|k| !now.contains_key(*k)).collect();
    Some((set, gone))
}

/// A changeset's file as its JSON names it ([`crate::harness::model::FileKey`]): its path, and
/// the top of its working tree when it's in one. Borrowed from the file: naming a file makes nothing.
pub(super) type FileRef<'a> = (Option<&'a str>, Option<&'a str>);

pub(super) fn file_ref(f: &Value) -> FileRef<'_> {
    (f.get("path").and_then(Value::as_str), f.get("tree").and_then(Value::as_str))
}

/// A changeset's file's key, as a patch says it's gone (`{"path", "tree"?}`).
fn file_key((path, tree): FileRef<'_>) -> Value {
    let mut key = serde_json::Map::new();
    for (k, v) in [("path", path), ("tree", tree)] {
        if let Some(v) = v {
            key.insert(k.to_owned(), Value::String(v.to_owned()));
        }
    }
    Value::Object(key)
}

/// Takes `set` and `gone` into `files`, as a changeset's patch says: each file set goes in place of
/// the one with its key, or after the rest when it's new; each gone goes. A viewer takes a patch
/// the same way (the UI's `changesPatched`), so its list keeps this one's order.
pub(super) fn merge_changes(files: &mut Vec<Arc<Entry>>, set: Vec<Value>, gone: &[Value]) {
    if !gone.is_empty() {
        let out: std::collections::HashSet<FileRef<'_>> = gone.iter().map(file_ref).collect();
        files.retain(|f| !out.contains(&f.file()));
    }
    // Where each goes, by its key, borrowed (nothing copied to look it up): in place of the file
    // with its key, or after the rest, as one new before it with its key was.
    let places: Vec<usize> = {
        let mut at: HashMap<FileRef<'_>, usize> = files.iter().enumerate().map(|(i, f)| (f.file(), i)).collect();
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
pub(super) fn changes_patch_of<'a>(was: &'a [Arc<Entry>], now: &'a [Arc<Entry>]) -> Option<(Vec<&'a Entry>, Vec<Value>)> {
    let before: HashMap<FileRef<'a>, &'a Arc<Entry>> = was.iter().map(|f| (f.file(), f)).collect();
    let kept: std::collections::HashSet<FileRef<'a>> = now.iter().map(|f| f.file()).collect();
    let set: Vec<&'a Entry> = now.iter().filter(|f| before.get(&f.file()).is_none_or(|b| !Arc::ptr_eq(b, f) && b.text() != f.text())).map(|f| &**f).collect();
    let gone: Vec<Value> = was.iter().filter(|f| !kept.contains(&f.file())).map(|f| file_key(f.file())).collect();
    // Taken in, the files keep their places and the new ones come after: `now` must be in that order.
    let order = was.iter().map(|f| f.file()).filter(|id| kept.contains(id)).chain(set.iter().map(|f| f.file()).filter(|id| !before.contains_key(id)));
    if !order.eq(now.iter().map(|f| f.file())) {
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
#[derive(Clone)]
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
    /// Every session's copy, each owned by one shard: changed only by messages to that shard's
    /// owner, read from the views it publishes, no lock guarding any (`super::copies`).
    sessions: Copies,
    /// The bus each session's live feed rides (`super::live`).
    bus: Arc<dyn kiln_seams::Bus>,
    /// Files uploaded for messages, by id. This and the registry's other maps but three are copies
    /// read without a lock and changed by swapping a new copy in whole (`crate::rcu`; goal 5, plan
    /// 01a1a59c2aae step 2): a session's updates, on the shard that owns its copy, read them.
    uploads: Rcu<HashMap<String, Upload>>,
    /// What each machine offers ("local" included).
    kinds: Rcu<HashMap<String, Vec<KindInfo>>>,
    /// The server's own sessions.
    pub local: Arc<Host>,
    /// The server's conversation store (docs/conversations.md), when it's on.
    pub convo: Option<crate::convo::store::Store>,
    /// What runners' feeds send into it (`crate::convo::feed`), and what to answer them.
    pub feed: Option<crate::convo::feed::Intake>,
    feed_out: Option<crate::memory::queue::Receiver<crate::convo::feed::Out>>,
    /// The runners that keep a copy of it (`crate::convo::mirror`), and what to send them.
    pub copies: Option<Arc<crate::convo::mirror::Mirrors>>,
    copies_out: Option<crate::memory::queue::Receiver<crate::convo::mirror::Out>>,
    /// Every store, as the server sees it: the System page's (`super::stores`).
    pub board: crate::convo::board::Board,
    /// What agents' pages read of the store, kept in memory while it holds (`crate::reads::Views`:
    /// the plans' list, the goal stacks; goal 10, plan 01a1a87b2dae step 2).
    pub views: crate::reads::Views,
    local_rx: crate::memory::queue::Receiver<(String, Update)>,
    /// Where `kiln spawn` in the server's own terminals reaches `local`.
    local_sock: Rcu<Option<String>>,
    /// The shell pid of each of the server's own terminals (their directory and environment are
    /// where agents started "in this shell" begin).
    local_shells: Rcu<HashMap<String, u32>>,
    dir: std::path::PathBuf,
    /// The server machine's transcripts, for its past sessions (made on first use).
    scanner: Arc<Mutex<Option<crate::aisessions::Scanner>>>,
    /// Names people gave sessions, by id (the "rename" control), kept in `names.json`: a session
    /// takes its name here at once, whatever its machine's runner knows of names (one from before
    /// kiln named sessions never reports it), and keeps it across restarts. Read on every state a
    /// session's machine sends.
    names: Rcu<HashMap<String, String>>,
    /// Sessions kiln doesn't keep going when they stop part-way (`goals::keep`; the "keep_going"
    /// control), kept in `keep-off.json` so it holds across restarts.
    keep_off: Rcu<std::collections::HashSet<String>>,
    /// The UI's commands by the ids their clients made, each taken once (`super::asked`).
    pub asked: super::asked::Asked,
    /// Which of its machine's runner processes holds each session on a machine (its instance:
    /// `main`, a uuid, `legacy`), kept in `holders.json` so it holds across restarts: a process
    /// that says what it holds speaks for its own sessions, never for another's that is away
    /// ([`Registry::inventory`]). Shared with the writes of it, which go on the blocking pool.
    holders: Arc<Rcu<HashMap<String, String>>>,
    /// A write of `holders.json` is on its way (it writes what's newest as it goes); and the one
    /// writing it now (they share the file, and the one it's written beside first).
    holders_saving: Arc<std::sync::atomic::AtomicBool>,
    holders_writing: Arc<Mutex<()>>,
    /// Each session's changeset to keep in the conversation store, the newest alone (shared with
    /// its copy, nothing written out yet), and whether its writer is at work
    /// ([`Registry::store_changes`]).
    changes_to_store: Arc<Rcu<HashMap<String, Files>>>,
    changes_storing: Arc<std::sync::atomic::AtomicBool>,
    /// Each session's latest epoch (`State::epoch`) seen, those that went on on another machine at
    /// least once, kept in `epochs.json`: a machine that announces an older copy lets go of it.
    epochs: Rcu<HashMap<String, u64>>,
    /// The plan each session last proposed (plan mode), not yet kept as one of kiln's plans: the
    /// goals' driver keeps it ([`Self::take_proposed`], `crate::plans::keep_proposed`).
    proposed: Rcu<HashMap<String, String>>,
    /// Restoring the conversation store from a mirror's copy of it.
    restore: Rcu<Restore>,
    /// The mirrors to ask for their copy as they connect: the store was found empty at start
    /// while the server had mirrors (it was lost, the rest wasn't).
    restore_auto: Rcu<Vec<String>>,
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
    /// Messages waiting for their session's inbox to answer (CP-T3.2), by session and message id: one owner
    /// of their own, no lock (`super::answers`; goal 5, plan 01a1a59c2aae, its top goal).
    answers: super::answers::Answers,
    /// The ids starts under way asked for ([`Registry::open_as`]), each until its start is done.
    opening: Rcu<std::collections::HashSet<String>>,
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
    held: &'a Rcu<std::collections::HashSet<String>>,
    ids: Vec<String>,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        self.held.update(|held| {
            for id in &self.ids {
                held.remove(id);
            }
        });
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
        // The server's own sessions' updates, waiting for the loop that hands them to their owners
        // (`run_local`): as many bytes as an owner's inbox holds (`crate::net::EVENTS_BYTES`), where they go
        // next; past it a session waits to send (goal 11, plan 01a1a87b2e56 step 3). Its 8,192 places, made
        // up front, took 1.05 MB of an idle server's heap (research 01a1a87b2e57, f10).
        let (tx, rx) = crate::memory::queue::bounded::<(String, Update)>(&crate::memory::owners::LOCAL_UPDATES, crate::net::EVENTS_BYTES);
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
        // Its copies' sealed chunks spilled beside its store (goal 11, plan 01a1a87b2e56 step 5): a folder
        // that can't be made leaves them in memory, said.
        let sessions = Copies::start(Arc::clone(&bus));
        if let Err(e) = sessions.spill_to(&dir.join("spill")) {
            tracing::warn!(error = %e, "the session copies' spill folder couldn't be made: their sealed chunks stay in memory");
        }
        // The room of viewers' snapshots in flight made now, so /metrics says its bound from the start (goal 11,
        // plan 01a1a87b2e56 step 3).
        let _ = snapshot_room();
        Self {
            sessions,
            bus,
            uploads: Rcu::default(),
            kinds: Rcu::default(),
            local,
            convo,
            feed,
            feed_out,
            copies,
            copies_out,
            board: crate::convo::board::Board::default(),
            views: crate::reads::Views::default(),
            local_rx: rx,
            local_sock: Rcu::default(),
            local_shells: Rcu::default(),
            names: Rcu::new(std::fs::read(dir.join("names.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            keep_off: Rcu::new(std::fs::read(dir.join("keep-off.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            asked: super::asked::Asked::default(),
            holders: Arc::new(Rcu::new(std::fs::read(dir.join("holders.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())),
            holders_saving: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            holders_writing: Arc::new(Mutex::new(())),
            changes_to_store: Arc::default(),
            changes_storing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            epochs: Rcu::new(std::fs::read(dir.join("epochs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()),
            proposed: Rcu::default(),
            dir,
            scanner: Arc::new(Mutex::new(None)),
            restore: Rcu::new(Restore::Idle),
            restore_auto: Rcu::new(restore_auto),
            keeper,
            visuals,
            streams: super::streams::Relay::default(),
            webrtc: super::webrtc::Webrtc::default(),
            ports: super::opened::Tunnels::default(),
            opened: super::opened::Openings::default(),
            answers: super::answers::Answers::new(WAITING_MAX),
            opening: Rcu::default(),
            #[cfg(test)]
            test_program: Mutex::new(None),
        }
    }

    /// The hub this registry serves, once it's made: what each session's owner acts through
    /// (`super::copies`).
    pub fn bind(&self, hub: &Arc<Hub>) {
        self.sessions.bind(hub);
    }

    /// Back once every session's owner has applied what was sent it before this (tests, and what
    /// reads every copy next).
    pub async fn settled(&self) {
        self.sessions.settled().await;
    }

    /// Back once session `id`'s owner has applied what was sent it before this.
    pub async fn applied(&self, id: &str) {
        self.sessions.applied(id).await;
    }

    /// Tests: session `id`'s copy's revision, as its newest view says.
    #[cfg(test)]
    pub fn rev(&self, id: &str) -> Option<u64> {
        self.sessions.with(id, |v| v.rev)
    }

    /// Tests: every session's id, in order.
    #[cfg(test)]
    pub fn ids(&self) -> Vec<String> {
        let mut ids = self.sessions.ids();
        ids.sort();
        ids
    }

    /// Tests: sessions started here from now on run `program` instead of their harness.
    #[cfg(test)]
    pub fn set_test_program(&self, program: &str) {
        *self.test_program.lock() = Some(program.to_owned());
    }

    /// Where restoring the conversation store from a mirror's copy stands.
    pub fn restore_view(&self) -> Restore {
        // Read off its copy; one swapped in only as an ask goes unanswered too long.
        let now = crate::util::now_ms();
        let unanswered = |r: &Restore| match r {
            Restore::Asked { from, at } if now - at > RESTORE_ANSWER_MS => Some(Restore::Failed { from: from.clone(), error: "no answer: its kiln may be too old to send its copy back".into(), at: now }),
            _ => None,
        };
        if unanswered(&self.restore.load()).is_some() {
            self.restore.update(|r| {
                if let Some(failed) = unanswered(r) {
                    *r = failed;
                }
            });
        }
        Restore::clone(&self.restore.load())
    }

    /// Ask `machine`, a mirror, for its copy of the conversation store, to restore this one from:
    /// what the copy brings fills in what the store lacks (`docs/conversations.md`, "Mirrors").
    pub fn restore_from(&self, hub: &Arc<Hub>, machine: &str) -> anyhow::Result<()> {
        let st = self.convo.as_ref().ok_or_else(|| anyhow::anyhow!("the conversation store is off"))?;
        anyhow::ensure!(self.feed.is_some(), "this server takes no conversation feeds");
        let peer = hub.instances.load().newest_with(machine, crate::proto::CAP_CONVO).ok_or_else(|| anyhow::anyhow!("{} isn't connected, or keeps no conversation store", hub.shown(machine)))?;
        if matches!(self.restore_view(), Restore::Asked { .. } | Restore::Copying { .. }) {
            anyhow::bail!("a restore is already under way");
        }
        self.restore.set(Restore::Asked { from: machine.to_owned(), at: crate::util::now_ms() });
        self.restore_auto.set(Vec::new());
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
        let at = crate::util::now_ms();
        self.restore.update(|r| {
            let begins = match &*r {
                Restore::Asked { from, .. } => from == machine,
                // Said again (it asks again after a silence).
                Restore::Copying { from, store: s, .. } => return from == machine && s == store,
                _ => false,
            };
            if begins {
                *r = Restore::Copying { from: machine.to_owned(), store: store.to_owned(), at };
            }
            begins
        })
    }

    /// Mirror `machine` has sent back its copy `store` whole (an empty `store`: it has none):
    /// the sessions it brought back are listed, away, on the machines they ran on.
    pub fn restore_done(&self, hub: &Arc<Hub>, machine: &str, store: &str) {
        let state = Restore::clone(&self.restore.load());
        match state {
            Restore::Asked { from, .. } if from == machine && store.is_empty() => {
                self.restore.set(Restore::Failed { from, error: format!("{} keeps no copy of the conversation store", hub.shown(machine)), at: crate::util::now_ms() });
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
                            h.harness.restore.set(Restore::Done { from, sessions, at });
                        }
                        Err(e) => h.harness.restore.set(Restore::Failed { from, error: format!("{e:#}"), at }),
                    }
                });
            }
            _ => {}
        }
    }

    /// Runner `machine` keeping a store connected: asked for its copy when the store was found
    /// empty at start and it is one of the server's mirrors.
    pub fn restore_due(&self, hub: &Arc<Hub>, machine: &str) {
        let due = self.restore_auto.load().iter().any(|m| m == machine);
        if due
            && matches!(self.restore_view(), Restore::Idle)
            && let Err(e) = self.restore_from(hub, machine)
        {
            tracing::warn!(%machine, error = %format!("{e:#}"), "couldn't ask a mirror for its copy of the conversation store");
        }
    }

    fn save_names(&self) {
        let names = self.names.get();
        if let Err(e) = self.keep_json("names.json", &*names) {
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
    async fn set_keep_going(&self, hub: &Hub, id: &str, on: bool) {
        let off: Vec<String> = self.keep_off.update(|g| {
            if on {
                g.remove(id);
            } else {
                g.insert(id.to_owned());
            }
            g.iter().cloned().collect()
        });
        if let Err(e) = self.keep_json("keep-off.json", &off) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep which agents kiln doesn't keep going");
        }
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    Arc::make_mut(&mut m.info).keep_off = !on;
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        let Some(info) = info else { return };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// The plans sessions proposed (plan mode) since the last look, by session, each its latest.
    pub fn take_proposed(&self) -> Vec<(String, String)> {
        // Every tick of the goals' driver: a copy swapped in only when there's one to take.
        if self.proposed.load().is_empty() {
            return Vec::new();
        }
        self.proposed.update(std::mem::take).into_iter().collect()
    }

    /// What each agent's goals wait on a person for (`goals::held_stacks`, every tick of the goals'
    /// driver): each one that changes told to every viewer.
    pub async fn set_held(&self, hub: &Hub, held: &HashMap<String, Held>) {
        let held = Arc::new(held.clone());
        let changed: Vec<Info> = self
            .sessions
            .on_each(move |s| {
                let mut changed = Vec::new();
                for (id, m) in s.mirrors_mut() {
                    let now = held.get(id).cloned();
                    if m.info.held != now {
                        Arc::make_mut(&mut m.info).held = now;
                        m.commit(|_| None);
                        changed.push(Info::clone(&m.info));
                    }
                }
                changed
            })
            .await
            .into_iter()
            .flatten()
            .collect();
        for info in changed {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What of session `id`'s messages couldn't be read (`crate::wire::unread`) changed: told to its
    /// viewers.
    pub async fn set_refused(&self, hub: &Hub, id: &str, refused: Option<crate::wire::unread::Kept>) {
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| match s.get_mut(&sid) {
                Some(m) if m.info.refused != refused => {
                    Arc::make_mut(&mut m.info).refused = refused;
                    m.commit(|_| None);
                    Some(Info::clone(&m.info))
                }
                _ => None,
            })
            .await
            .flatten();
        if let Some(info) = info {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// A session is given a name here (the "rename" control from any device; empty gives it its
    /// harness's name back): its page and every list show it at once.
    async fn rename(&self, hub: &Hub, id: &str, value: Option<&Value>) {
        let name: String = value.and_then(Value::as_str).unwrap_or("").trim().chars().take(MAX_NAME).collect();
        self.names.update(|g| {
            if name.is_empty() {
                g.remove(id);
            } else {
                g.insert(id.to_owned(), name.clone());
            }
        });
        self.save_names();
        let sid = id.to_owned();
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    let info = Arc::make_mut(&mut m.info);
                    info.name = if name.is_empty() { info.kind_name.clone() } else { name };
                    let named = json!(info.name);
                    if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
                        o.insert("name".into(), named);
                        m.commit(|m| m.state_frame());
                    } else {
                        m.commit(|_| None);
                    }
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
        let Some(info) = info else { return };
        hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
    }

    /// Environment for one of the server's own terminals, so `kiln spawn` there works.
    pub fn terminal_env(&self, terminal: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![("KILN_TERM_SESSION", terminal.to_owned())];
        if let Some(sock) = self.local_sock.load().as_deref() {
            env.push(("KILN_HARNESS_SOCK", sock.to_owned()));
        }
        env
    }

    /// One of the server's own terminals started (its shell runs as `pid`).
    pub fn note_shell(&self, terminal: &str, pid: Option<u32>) {
        self.local_shells.update(|g| {
            if let Some(pid) = pid {
                g.insert(terminal.to_owned(), pid);
            }
            // Forget shells that are gone.
            if g.len() > 256 {
                g.retain(|_, p| crate::os::pid_alive(*p));
            }
        });
    }

    /// The server's own sessions' updates, into the mirrors (supervised; restartable).
    /// Sessions of machines that are away, as the server's store has them (the server started
    /// again since they went away): listed as away, so they can be moved on, until their machine
    /// says what it has.
    pub async fn restore_away(&self, hub: &Hub, away: Vec<(crate::convo::store::Unended, Vec<crate::convo::rows::EventRow>)>) {
        // Each copy is made here, not by its owner: a session's stored events are megabytes of
        // JSON to read, its text written out once for every snapshot and digest after. Those
        // already there when they're put in are left as they are.
        let mut made = Vec::new();
        for (u, events) in away {
            if self.sessions.contains(&u.id) {
                continue;
            }
            let Ok(mut info) = serde_json::from_str::<Info>(&u.info) else { continue };
            // The machine whose feed brought it, else the one its updates came from.
            let Some(machine) = u.machine.clone().or_else(|| info.target.strip_prefix("agent:").map(str::to_owned)) else { continue };
            info.target = format!("agent:{machine}");
            if hub.agents.load().get(&machine).is_some_and(|a| a.connected) {
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
                    let text = serde_json::to_string(&v).unwrap_or_default();
                    m.upsert(Arc::new(Entry::written(v, text)));
                }
            }
            made.push((u.id, m, info));
        }
        // To their owners, each shard's at once.
        let mut by_shard: HashMap<usize, Vec<(String, Mirror, Info)>> = HashMap::new();
        for (id, m, info) in made {
            by_shard.entry(self.sessions.owner_of(&id)).or_default().push((id, m, info));
        }
        let mut infos = Vec::new();
        for (shard, batch) in by_shard {
            let put = self.sessions.on_shard(shard, move |s| batch.into_iter().filter_map(|(id, m, info)| s.insert(id, m).then_some(info)).collect::<Vec<_>>()).await;
            infos.extend(put.unwrap_or_default());
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
            Ok(Ok(away)) => hub.harness.restore_away(hub, away).await,
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
        hub.harness.kinds.update(|k| k.insert("local".to_owned(), offered.clone()));
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
                Ok(p) => hub.harness.local_sock.set(Some(p.to_string_lossy().into_owned())),
                Err(e) => tracing::info!(error = %format!("{e:#}"), "`kiln spawn` will not be available in the server's own terminals"),
            }
        }
        // The server's own sessions' updates, as fast as they come, each to its owner (waited for
        // only while its inbox is full): in bounded turns of the shard.
        let mut turns = crate::rt::Turns::new();
        loop {
            let Ok((id, u)) = rx.recv().await else { return Ok(()) };
            hub.harness.ingest_from(&hub, &Location::Local, &id, u, false).await;
            turns.after().await;
        }
    }

    /// Session `id` and its ancestors, nearest first ([`super::lineage::LINE_MAX`] at most), each its
    /// id and parent alone: what [`super::lineage::descends`] reads of it, without a copy of every
    /// session.
    pub fn lineage_of(&self, id: &str) -> Vec<Info> {
        let mut out = Vec::new();
        let mut at = id.to_owned();
        while out.len() < super::lineage::LINE_MAX {
            let Some((sid, parent)) = self.sessions.with(&at, |v| (v.info.id.clone(), v.info.parent.clone())) else { break };
            out.push(Info { id: sid, parent: parent.clone(), ..Info::default() });
            match parent {
                Some(p) => at = p,
                None => break,
            }
        }
        out
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = Vec::new();
        self.sessions.each(|_, view| v.push(Info::clone(&view.info)));
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// [`Registry::list`], each info as its owner shares it with its view, none copied (goal 9, plan 01a1a87b2cec
    /// step 5: `agents.list` copied every session's whole info to read a few fields of each).
    pub fn list_shared(&self) -> Vec<Arc<Info>> {
        let mut v: Vec<Arc<Info>> = Vec::new();
        self.sessions.each(|_, view| v.push(Arc::clone(&view.info)));
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// [`Registry::list`] as `GET /api/harness/sessions` answers it, in its order: each session's
    /// info as its owner encoded it once ([`super::copies::View::info_json`]), joined into a JSON
    /// array once for every read that finds the sessions as they were, and answered as it is (goal 10,
    /// plan 01a1a87b2dae steps 2 and 5: `super::copies::Copies::list_json`).
    pub fn list_json(&self) -> bytes::Bytes {
        self.sessions.list_json()
    }

    /// Session `id`'s info as `GET /api/harness/sessions/{id}` answers it, as its owner encoded it
    /// ([`super::copies::View::info_json`]). None when there's no such session.
    pub fn info_json(&self, id: &str) -> Option<bytes::Bytes> {
        self.sessions.with(id, |v| v.info_json.clone())
    }

    /// Session `id`'s newest `limit` events before number `before`, as its copy holds them
    /// ([`super::copies::View::events_json`]). None when there's no such session or its copy can't
    /// say all of them.
    pub fn events_json(&self, id: &str, before: Option<u64>, limit: usize) -> Option<bytes::Bytes> {
        self.sessions.with(id, |v| v.events_json(before, limit)).flatten()
    }

    /// [`Registry::events_json`] in the pieces it's made of, for an answer that sends them as they are
    /// ([`super::copies::View::events_page`]; goal 10, plan 01a1a87b2dae step 5).
    pub fn events_page(&self, id: &str, before: Option<u64>, limit: usize) -> Option<Vec<bytes::Bytes>> {
        self.sessions.with(id, |v| v.events_page(before, limit)).flatten()
    }

    pub fn kinds(&self) -> HashMap<String, Vec<KindInfo>> {
        #[cfg(not(test))]
        super::acp::warm_models();
        let mut all: HashMap<String, Vec<KindInfo>> = HashMap::clone(&self.kinds.load());
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
        self.kinds.load().get(target).is_some_and(|ks| !ks.iter().any(|k| k.id == kind && k.available))
    }

    #[cfg(test)]
    pub fn set_kinds(&self, target: &str, kinds: Vec<KindInfo>) {
        self.kinds.update(|k| k.insert(target.to_owned(), kinds.clone()));
    }

    /// A session's meta as last reported (its harness's catalog: slash commands, models, modes).
    pub fn meta(&self, id: &str) -> Option<Value> {
        // Copied from the view held, not from where it's read.
        let meta = self.sessions.with(id, |v| Arc::clone(&v.meta))?;
        Some(Value::clone(&meta))
    }

    pub fn location(&self, id: &str) -> Option<Location> {
        self.sessions.with(id, |v| Location::clone(&v.location))
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

    /// Apply one update from a machine and pass it on to the session's viewers: back once the
    /// session's owner has (`super::copies`), so what reads the copy next finds it.
    pub async fn ingest(&self, hub: &Hub, from: &Location, id: &str, u: Update) {
        self.ingest_from(hub, from, id, u, false).await;
        self.sessions.applied(id).await;
    }

    /// Machine `machine` is where session `id` runs (or the server doesn't know the session yet):
    /// what it says of it counts.
    pub fn runs_on(&self, id: &str, machine: &str) -> bool {
        self.sessions.with(id, |v| matches!(&*v.location, Location::Machine(at) if at == machine)).unwrap_or(true)
    }

    /// Session `id` is one kiln knows, and it runs on `machine`.
    pub fn located_on(&self, id: &str, machine: &str) -> bool {
        self.sessions.with(id, |v| matches!(&*v.location, Location::Machine(at) if at == machine)).unwrap_or(false)
    }

    /// A copy of session `id` at `epoch` is from before it went on elsewhere: its machine lets go
    /// of it.
    pub fn stale(&self, id: &str, epoch: u64) -> bool {
        self.epochs.load().get(id).is_some_and(|e| *e > epoch)
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
        // Read off the epochs' copy; a new one swapped in only when this one rises.
        if self.epochs.load().get(id).is_some_and(|e| *e >= epoch) {
            return;
        }
        let raised = self.epochs.update(|g| {
            let e = g.entry(id.to_owned()).or_insert(0);
            let raised = *e < epoch;
            *e = (*e).max(epoch);
            raised
        });
        if !raised {
            return;
        }
        let snapshot = self.epochs.get();
        if let Err(e) = self.keep_json("epochs.json", &*snapshot) {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the agents' epochs");
        }
    }

    /// An update of session `id` from `from`, handed to the shard that owns its copy
    /// (`super::copies`), which applies it after whatever was handed it before ([`Registry::apply`]):
    /// back once it's handed over, waiting only while that owner's inbox is full. `shipped`: its
    /// machine keeps it in a store it sends the server's (`crate::convo::feed`), which keeps it from
    /// that, not from here. Goal 4, plan 01a1a59c2a9c step 2.
    pub async fn ingest_from(&self, hub: &Hub, from: &Location, id: &str, u: Update, shipped: bool) {
        let Some(ingest) = self.prepare(hub, from, id, u, shipped) else { return };
        if !self.sessions.send(id, Msg::Update(Box::new(ingest))).await {
            tracing::debug!(session = id, "an agent session's update came as the server's copies went");
        }
    }

    /// An update from a session's runner, handed to the shard that owns its copy to judge and take
    /// there (`crate::hub::Hub::runner_update`; goal 10, plan 01a1a87b2dae step 4): back once that
    /// owner's inbox has it, waiting only while it's full, as [`Registry::ingest_from`] does.
    pub async fn from_runner(&self, r: super::copies::RunnerUpdate) {
        let id = r.session.clone();
        if !self.sessions.send(&id, Msg::Runner(Box::new(r))).await {
            tracing::debug!(session = %id, "an agent session's update came as the server's copies went");
        }
    }

    /// An update of session `id` from `from`, taken by the shard that owns its copy, `s` (goal 10, plan
    /// 01a1a87b2dae step 4: a runner's update is judged and taken there, not on the hub's loop): what's
    /// left of it when the owner's turn ran out (`over`, [`Registry::apply`]; goal 9), to take first next.
    #[allow(clippy::too_many_arguments)]
    pub fn take_from(&self, s: &mut Shard, hub: &Arc<Hub>, from: &Location, id: &str, u: Update, shipped: bool, over: &dyn Fn() -> bool) -> Option<Ingest> {
        let ingest = self.prepare(hub, from, id, u, shipped)?;
        self.apply(s, hub, ingest, over)
    }

    /// An update of session `id` from `from` as its copy takes it ([`Registry::apply`]): an inbox's
    /// answer is told whoever waits for it, and leaves nothing to take; an Open takes where it runs
    /// and the name given here, and raises its epoch.
    fn prepare(&self, hub: &Hub, from: &Location, id: &str, u: Update, shipped: bool) -> Option<Ingest> {
        // What became of a message: for whoever waits for it, wherever it was taken or refused.
        let u = match u {
            Update::Inbox { said } => {
                self.answered(id, *said);
                return None;
            }
            other => other,
        };
        // Where it runs, and the name given here, are the server's to say: to the copy and to
        // viewers alike (a session announced again — a switch of harness, a reconnect — keeps its
        // machine and its name). The machine's own name is kept aside: it counts once it changes it.
        let mut reported = String::new();
        let u = match u {
            Update::Open { mut info, since } => {
                info.target = from.target();
                own_name(&info.name, &info.kind_name).unwrap_or_default().clone_into(&mut reported);
                if let Some(n) = self.names.load().get(id) {
                    info.name.clone_from(n);
                }
                info.keep_off = self.keep_off.load().contains(id);
                Update::Open { info, since }
            }
            other => other,
        };
        if let Update::Open { info, .. } = &u
            && info.epoch > 0
        {
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
        Some(Ingest { from: from.clone(), id: id.to_owned(), u, shipped, reported })
    }

    /// An update of a session from the machine it runs on, as the shard that owns the session's copy
    /// takes it (`super::copies`): the copy changes and its view goes out, its viewers are sent what
    /// changed, then what the change tells the rest of kiln happens. Goal 4, plan 01a1a59c2a9c
    /// step 2: only this shard writes the copy.
    ///
    /// An update's events are taken while its owner's turn lasts (`over`: its step quantum spent), one at
    /// the least, and the copy goes out with those: the rest come back, for the owner to take first at
    /// its next turn, before anything newer (goal 9, plan 01a1a87b2cec step 3: a runner's whole history
    /// in one update held its owner's shard up to 83 ms).
    pub(super) fn apply(&self, s: &mut Shard, hub: &Arc<Hub>, i: Ingest, over: &dyn Fn() -> bool) -> Option<Ingest> {
        let Ingest { from, id, mut u, shipped, reported } = i;
        let id = id.as_str();
        // Only the machine a session runs on speaks for it: a copy left behind where it went on
        // from says nothing more of it.
        if !matches!(u, Update::Open { .. }) && s.get(id).is_some_and(|m| *m.location != from) {
            return None;
        }
        // A changeset's patch is stored as the whole changeset it makes, once taken in (below); an
        // update's events as they're taken, each written out once for the copy and its row (below).
        let store = self.convo.as_ref().filter(|_| !shipped);
        if let Some(store) = store.filter(|_| !matches!(u, Update::ChangesPatch { .. } | Update::Events { .. })) {
            store.send(id, &u);
        }
        // What the copy takes: each event's JSON and its text, written out once for the copy, its
        // live frame, its digest and every snapshot after (`Entry::of_event`, as they're taken, below);
        // a state's and a meta's JSON.
        let mut entries: Vec<Arc<Entry>> = Vec::new();
        // The events' rows for the conversation store, of the same texts.
        let mut rows: Vec<crate::convo::rows::Row> = Vec::new();
        // How many of an update's events its turn took.
        let mut taken = 0;
        let mut made: Option<Value> = match &u {
            Update::State { state } => Some(serde_json::to_value(state).unwrap_or(Value::Null)),
            Update::Meta { meta } => Some(serde_json::to_value(meta).unwrap_or(Value::Null)),
            _ => None,
        };
        // Its info changed as its pages see it: the events socket is told, its frame made of the bytes its
        // view encoded the info as (goal 10, plan 01a1a87b2dae step 5: it was encoded again for the socket).
        let mut changed = false;
        // The same meta again (a harness says it at each turn): nothing to keep, encode or send.
        let mut meta_same = false;
        let mut gone = false;
        // The copy's events went another way than its viewers have them: they start over from it.
        let mut resync = false;
        // Finished messages' mentions of files, to keep once the copy is out.
        let mut jobs: Vec<Job> = Vec::new();
        // The plan it proposed (plan mode) in these events, to keep as one of kiln's plans.
        let mut proposed: Option<String> = None;
        let mut save_names = false;
        // The account's other sessions, which pause too (`hold_account`).
        let mut hold: Option<(Command, Vec<(String, Location)>)> = None;
        // Its account, and the window reached and when that starts over.
        let mut hold_for: Option<(String, Option<String>, Option<i64>)> = None;
        // The whole changeset a patch makes, for the store.
        let mut stored_changes: Option<Files> = None;
        if let Update::Open { info, since } = &u {
            let old = s.get(id);
            // A runner that started again goes on from what this copy has of the same epoch:
            // its events through `since` stay; what's after them comes again.
            let keep = since.filter(|_| old.is_some_and(|m| m.info.epoch == info.epoch));
            if let Location::Machine(machine) = &from {
                match keep {
                    Some(n) => tracing::info!(session = id, %machine, after = n, "an agent session's copy goes on from its machine's Open after event"),
                    None => tracing::info!(session = id, %machine, "an agent session's copy starts from its machine's Open"),
                }
            }
            let mut info = (**info).clone();
            // What its goals wait on a person for is the server's to say (`goals::held_stacks`): kept.
            info.held = old.and_then(|m| m.info.held.clone());
            // So is what of its messages couldn't be read (`crate::wire::unread`).
            info.refused = old.and_then(|m| m.info.refused.clone());
            // So is whether kiln started it as its parent's subagent (`Spec::subagent`): a runner
            // from before the word doesn't say it, and it stays one.
            info.subagent |= old.is_some_and(|m| m.info.subagent);
            // What kiln's own words were handed is kept too, and what waits in its queue until
            // the state after this says again (neither goes in an Open): a runner that comes
            // back doesn't make a word handed look lost (CP-T3.2, the fourteenth adversarial check's).
            if let Some(old) = old {
                info.said.clone_from(&old.info.said);
                info.queue.clone_from(&old.info.queue);
            }
            let bus = Arc::clone(s.bus());
            let m = s.entry(id, || Mirror::new(info.clone(), from.clone(), bus));
            // Its machine announced it: what it reports from now on counts.
            m.opened_at = None;
            m.info = Arc::new(info);
            m.reported = reported;
            m.location = Arc::new(from.clone());
            match keep {
                Some(n) => {
                    let had = m.events.len();
                    // Its chunks through `n` kept as they are, none read again (goal 9, plan 01a1a87b2cec step 3).
                    m.keep_through(n);
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
            changed = true;
        }
        let Some(m) = s.get_mut(id) else {
            // Its events are stored all the same, as they were before a copy took them.
            if let Some(store) = store.filter(|_| matches!(u, Update::Events { .. })) {
                store.send(id, &u);
            }
            // A session this server does not know yet (it restarted): ask for all of it.
            if let Location::Machine(machine) = &from
                && !matches!(u, Update::Closed)
            {
                self.request_snapshot(hub, machine, id);
            }
            return None;
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
                m.state = Arc::new(made.take().unwrap_or(Value::Null));
                // The info as it was, shared with the copy's views (not copied: its own is, as it changes).
                let before = Arc::clone(&m.info);
                let info = Arc::make_mut(&mut m.info);
                info.apply(state);
                info.last_activity = crate::util::now_ms();
                // Its name: one of its own the machine gives anew; else the one given here. Read off
                // the names' copy, a new one swapped in only as the machine's own is taken up.
                let names = self.names.load();
                match naming(&state.name, &m.reported, &info.kind_name, names.contains_key(id)) {
                    Naming::Adopt(own) => {
                        m.reported = own.to_owned();
                        self.names.update(|n| n.insert(id.to_owned(), own.to_owned()));
                        save_names = true;
                    }
                    Naming::Keep => {
                        if let Some(n) = names.get(id) {
                            info.name.clone_from(n);
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
                    changed = true;
                }
                if before.pause.is_none()
                    && let Some(p) = m.info.pause.as_ref().filter(|p| p.reason == super::pause::LIMIT && p.by == "harness")
                    && let Some(account) = m.info.account.clone()
                {
                    let c = Command::LimitHold { window: p.window.clone(), label: p.label.clone(), until: p.until, said: p.said.clone(), account: Some(account.clone()) };
                    hold = Some((c, Vec::new()));
                    // Collected below, once this copy is out.
                    hold_for = Some((account, p.window.clone(), p.until));
                }
            }
            Update::Meta { .. } => {
                let meta = made.take().unwrap_or(Value::Null);
                if *m.meta == meta {
                    meta_same = true;
                } else {
                    m.meta = Arc::new(meta);
                }
            }
            Update::Changes { files } => m.take_changes(files.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), Vec::new(), true),
            Update::ChangesPatch { set, gone } => {
                m.take_changes(set.iter().filter_map(|f| serde_json::to_value(f).ok()).collect(), gone.iter().filter_map(|k| serde_json::to_value(k).ok()).collect(), false);
                if self.convo.is_some() && !shipped {
                    stored_changes.clone_from(&m.changes);
                }
            }
            Update::Events { events } => {
                for e in events {
                    // Written out once, for the copy and for its row alike (goal 9, plan 01a1a87b2cec step 5).
                    let entry = Arc::new(if store.is_some() {
                        Entry::of_event(e, Some(&mut |text: String| rows.push(crate::convo::rows::Row::Event(Box::new(crate::convo::rows::event_written(id, e, text))))))
                    } else {
                        Entry::of_event(e, None)
                    });
                    if let Some(job) = mentions(id, &m.info.cwd, &m.location, e) {
                        jobs.push(job);
                    }
                    // kiln's own words handed to its harness, with what they said then: what a
                    // goal stack's word or a parent's news held when its agent took it.
                    if let super::model::Body::User { text, client_id: Some(c), .. } = &e.body
                        && super::queued::own(c)
                    {
                        super::queued::note_given(&mut Arc::make_mut(&mut m.info).said, c, super::queued::stable(text));
                    }
                    if let Some(plan) = proposed_in(e) {
                        proposed = Some(plan);
                    }
                    if !entry.is_null() {
                        resync |= m.upsert(Arc::clone(&entry));
                    }
                    entries.push(entry);
                    taken += 1;
                    // Its turn spent: the rest at the owner's next (below), one taken at the least.
                    if taken < events.len() && over() {
                        break;
                    }
                }
            }
            Update::Stream { id, text } => m.append(id, text),
        }
        // What of an update's events its turn had no time for (goal 9, plan 01a1a87b2cec step 3): the copy
        // goes out with those taken, and the rest come back to the owner.
        let rest = match &mut u {
            Update::Events { events } if taken < events.len() => Some(Update::Events { events: events.split_off(taken) }),
            _ => None,
        };
        if let Some(store) = store {
            store.put(std::mem::take(&mut rows));
        }
        // Its viewers start over from the copy as it is now (its events went another way than
        // they have them): the feed's resync marker sends each back to a snapshot, and none
        // resumes from before it.
        if resync {
            m.started_over();
        }
        // The copy goes out, then its viewers are sent what changed. A state goes out with the name
        // given here in it (the machine's may lack it). An Open that goes on from what the copy has
        // isn't one to its viewers: they go on with what they have, unless the copy's events went
        // another way than theirs.
        m.commit(|m| match &u {
            _ if resync => Some(super::copies::payload_of(super::live::RESYNC, 0)),
            Update::Open { since: Some(_), .. } => None,
            Update::State { .. } => m.state_frame(),
            Update::Changes { .. } | Update::ChangesPatch { .. } => m.changes_frame(),
            Update::Meta { .. } if meta_same => None,
            _ => Some(m.frame_of(&u, &entries)),
        });
        if gone {
            s.remove(id);
        }
        if let (Some((account, window, until)), Some((_, targets))) = (hold_for.as_ref(), hold.as_mut()) {
            let now = crate::util::now_ms();
            // From every shard's views (another shard's copies are read, never changed, here).
            s.each_view(|other, o| {
                // Not one whose own limits show the limit isn't its own: another account's (an
                // account id gone stale), another model's.
                if other != id
                    && o.info.account.as_deref() == Some(account.as_str())
                    && o.info.pause.is_none()
                    && o.info.status != Status::Exited
                    && super::pause::shares(&o.info.limits, &o.info.model, window.as_deref(), *until, now) != Some(false)
                {
                    targets.push((other.to_owned(), Location::clone(&o.location)));
                }
            });
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
            self.proposed.update(|p| p.insert(id.to_owned(), plan.clone()));
        }
        if gone {
            // Copies swapped in only for what they had of it.
            if self.names.load().contains_key(id) {
                save_names = self.names.update(|n| n.remove(id)).is_some() || save_names;
            }
            if self.changes_to_store.load().contains_key(id) {
                self.changes_to_store.update(|c| c.remove(id));
            }
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
        } else if changed && let Some(m) = s.get(id) {
            hub.emit_harness(id, m.info_encoded());
        }
        rest.map(|u| Ingest { from, id: id.to_owned(), u, shipped, reported: String::new() })
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
    pub async fn inventory(&self, hub: &Hub, machine: &str, instance: Option<&str>, present: &[String], inv: Inventory) {
        let keep: std::collections::HashSet<String> = inv.sessions.into_iter().collect();
        let (others, away): (std::collections::HashSet<String>, Vec<String>) = match instance {
            Some(me) => {
                let g = self.holders.load();
                let others: std::collections::HashSet<String> = g.iter().filter(|(_, h)| h.as_str() != me).map(|(id, _)| id.clone()).collect();
                let away = g.iter().filter(|(id, h)| h.as_str() != me && !present.contains(h) && !keep.contains(*id)).map(|(id, _)| id.clone()).collect();
                (others, away)
            }
            None => Default::default(),
        };
        let at = Location::Machine(machine.to_owned());
        let away: Vec<String> = away.into_iter().filter(|id| self.sessions.with(id, |v| *v.location == at && v.info.status != Status::Disconnected).unwrap_or(false)).collect();
        if !away.is_empty() {
            tracing::info!(%machine, sessions = away.len(), "agent sessions another runner process of this machine holds, which isn't connected: kept, away");
            self.sessions_lost(hub, &away).await;
        }
        let (keep, others) = (Arc::new(keep), Arc::new(others));
        let gone: Vec<String> = self
            .sessions
            .on_each(move |s| {
                // Not one this server opened there that the machine hasn't announced yet: the report
                // was made before the open got there.
                let gone: Vec<String> = s
                    .mirrors()
                    .filter(|(id, m)| *m.location == at && !keep.contains(*id) && !others.contains(*id) && m.opened_at.is_none_or(|t| t.elapsed() > OPEN_GRACE))
                    .map(|(id, _)| id.clone())
                    .collect();
                // Its viewers are told, and its feed's topic goes with the last of them.
                for id in &gone {
                    s.remove(id);
                }
                gone
            })
            .await
            .into_iter()
            .flatten()
            .collect();
        let kinds = inv.kinds;
        self.kinds.update(|k| k.insert(format!("agent:{machine}"), kinds.clone()));
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
        held.iter().filter(|id| self.sessions.with(id, |v| *v.location == at && v.info.status == Status::Disconnected).unwrap_or(false)).cloned().collect()
    }

    /// Session `id` is held by runner process `instance` of its machine, as that process said:
    /// kept (across restarts too) once it changes.
    pub fn held_by(&self, id: &str, instance: &str) {
        self.held_by_all(std::slice::from_ref(&id.to_owned()), instance);
    }

    /// [`Registry::held_by`] for each of `ids`: kept once for them all (a runner coming back says
    /// what it holds of every session at once).
    pub fn held_by_all(&self, ids: &[String], instance: &str) {
        // A session's updates say who holds it: read off the holders' copy, a new one swapped in
        // only when one of them changes hands.
        let held = self.holders.load();
        if ids.iter().all(|id| held.get(id).is_some_and(|h| h == instance)) {
            return;
        }
        drop(held);
        let changed = self.holders.update(|g| {
            let mut changed = false;
            for id in ids {
                if g.get(id).is_none_or(|h| h != instance) {
                    g.insert(id.clone(), instance.to_owned());
                    changed = true;
                }
            }
            changed
        });
        if changed {
            self.save_holders();
        }
    }

    /// The runner process of its machine that last said it holds session `id`, if one did.
    pub fn holder(&self, id: &str) -> Option<String> {
        self.holders.load().get(id).cloned()
    }

    fn forget_holders(&self, ids: &[String]) {
        // A copy swapped in only when one of them has a holder.
        if !ids.iter().any(|id| self.holders.load().contains_key(id)) {
            return;
        }
        let changed = self.holders.update(|g| {
            let mut changed = false;
            for id in ids {
                changed |= g.remove(id).is_some();
            }
            changed
        });
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
        // The change just swapped into the holders' copy, then the flag: past the fence a write on
        // its way either reads the change or is seen to have let go (the fence in `write` below).
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.holders_saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let (holders, saving, writing, dir) = (Arc::clone(&self.holders), Arc::clone(&self.holders_saving), Arc::clone(&self.holders_writing), self.dir.clone());
        let write = move || {
            // One at a time, each taking what's newest once it's its turn.
            let _one = writing.lock();
            // Let go first: a change after the copy below starts another write (after this one).
            saving.store(false, Ordering::Release);
            std::sync::atomic::fence(Ordering::SeqCst);
            let now = holders.get();
            if let Err(e) = keep_json_in(&dir, "holders.json", &*now) {
                tracing::warn!(error = %format!("{e:#}"), "couldn't keep which runner process holds each agent session");
            }
        };
        drop(crate::rt::spawn(async move {
            let _ = crate::rt::blocking(write).await;
        }));
    }

    /// Keeps session `id`'s changeset `files` in the conversation store as it is when its turn comes:
    /// written out on the compute pool (most of a megabyte of JSON for a long session's), kept by the
    /// store on the blocking pool (goal 4, plan 01a1a59c2a9c step 3). One writer at a time, at the pace
    /// the store commits: it writes each session's newest, waits for the store to have it, then takes
    /// what came meanwhile, a patch that comes while it works taking the place of the one before (each
    /// is the changeset whole). A burst of patches (a runner sending what it held) writes a session's
    /// changeset as often as the store takes it, not once a patch.
    fn store_changes(&self, id: &str, files: Files, store: crate::convo::store::Store) {
        use std::sync::atomic::Ordering;
        // Into the changesets' copy, then the writer's flag past a fence: a writer letting go as
        // this comes either reads this (past the fence it reads it after) or is seen to have let go.
        self.changes_to_store.update(|c| c.insert(id.to_owned(), Arc::clone(&files)));
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.changes_storing.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pending, storing) = (Arc::clone(&self.changes_to_store), Arc::clone(&self.changes_storing));
        drop(crate::rt::spawn(async move {
            loop {
                let batch: Vec<(String, Files)> = if pending.load().is_empty() { Vec::new() } else { pending.update(std::mem::take).into_iter().collect() };
                if batch.is_empty() {
                    storing.store(false, Ordering::Release);
                    std::sync::atomic::fence(Ordering::SeqCst);
                    // One that came in between finds no writer at work: this one goes on with it,
                    // unless it started another.
                    if pending.load().is_empty() || storing.swap(true, Ordering::AcqRel) {
                        return;
                    }
                    continue;
                }
                // Each changeset is its files' texts, written out once: a session at a time.
                let rows = crate::rt::compute_map(batch, |(session, files)| {
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
                    crate::convo::rows::Row::Session { session, info: None, meta: None, state: None, changes, epoch: None, open: false, at: crate::util::now_ms() }
                })
                .await;
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "the sessions' changesets couldn't be written out for the conversation store");
                        continue;
                    }
                };
                let store = store.clone();
                let kept = crate::rt::blocking(move || {
                    store.put(rows);
                    store.sync()
                })
                .await;
                if let Err(e) = kept.and_then(|r| r) {
                    tracing::debug!(error = %format!("{e:#}"), "the conversation store didn't say it had the changesets");
                }
            }
        }));
    }

    /// Machine `from` is known as `to` from now on (docs/machines.md): its sessions are there (and
    /// kept so in the conversation store), and what it offers is its. Its runner, coming back as
    /// `to`, finds them its own.
    pub async fn move_machine(&self, hub: &Hub, from: &str, to: &str) -> Vec<String> {
        let (at, now) = (Location::Machine(from.to_owned()), Location::Machine(to.to_owned()));
        let (was, there) = (at.clone(), now.clone());
        let moved: Vec<Info> = self
            .sessions
            .on_each(move |s| {
                let at = &was;
                let there = Arc::new(there.clone());
                s.mirrors_mut()
                    .filter(|(_, m)| *m.location == *at)
                    .map(|(_, m)| {
                        m.location = Arc::clone(&there);
                        Arc::make_mut(&mut m.info).target = there.target();
                        m.commit(|_| None);
                        Info::clone(&m.info)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .into_iter()
            .flatten()
            .collect();
        self.kinds.update(|k| {
            if let Some(v) = k.remove(&at.target()) {
                k.entry(now.target()).or_insert(v);
            }
        });
        if let Some(st) = &self.convo {
            st.put(moved.iter().map(|i| crate::convo::rows::Row::Machine { session: i.id.clone(), machine: to.to_owned() }).collect());
        }
        let ids: Vec<String> = moved.iter().map(|i| i.id.clone()).collect();
        for info in moved {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
        ids
    }

    /// Sessions `moved` onto `machine`, connected now (docs/machines.md, "One record a machine"):
    /// those its runner processes hold (`held`) and that read disconnected come back as they are
    /// (their snapshots are asked for, the ids returned); those none of them holds aren't on the
    /// machine, and are gone, as its next inventory would say.
    pub async fn moved_onto_connected(&self, hub: &Hub, machine: &str, moved: &[String], held: &[String]) -> Vec<String> {
        let mine: Vec<String> = moved.iter().filter(|id| held.contains(*id)).cloned().collect();
        let ask = self.still_disconnected(machine, &mine);
        let at = Location::Machine(machine.to_owned());
        let away = Arc::new(moved.iter().filter(|id| !held.contains(*id)).cloned().collect::<Vec<String>>());
        let gone: Vec<String> = self
            .sessions
            .on_each(move |s| {
                let gone: Vec<String> = s.mirrors().filter(|(id, m)| *m.location == at && away.contains(*id)).map(|(id, _)| id.clone()).collect();
                // Its viewers are told, and its feed's topic goes with the last of them.
                for id in &gone {
                    s.remove(id);
                }
                gone
            })
            .await
            .into_iter()
            .flatten()
            .collect();
        self.forget_holders(&gone);
        for id in gone {
            super::archive::closed(hub, &id);
            hub.emit(&crate::wire::events::ServerEvent::HarnessClosed { id: id.to_string() });
        }
        ask
    }

    /// The machine went away: its sessions keep running there and come back when it reconnects.
    pub async fn machine_lost(&self, hub: &Hub, machine: &str) {
        self.streams.machine_lost(machine, &hub.shown(machine));
        self.ports.machine_lost(machine);
        let at = Location::Machine(machine.to_owned());
        let infos: Vec<Info> = self
            .sessions
            .on_each(move |s| s.mirrors_mut().filter(|(_, m)| *m.location == at).map(|(_, m)| disconnected(m)).collect::<Vec<_>>())
            .await
            .into_iter()
            .flatten()
            .collect();
        self.kinds.update(|k| k.remove(&format!("agent:{machine}")));
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// One runner process of a machine went away while another of the machine stays connected
    /// (see `crate::instances`): the sessions it held keep running there and come back with it.
    pub async fn sessions_lost(&self, hub: &Hub, ids: &[String]) {
        let ids: Arc<[String]> = ids.into();
        let infos: Vec<Info> = self
            .sessions
            .on_each(move |s| ids.iter().filter_map(|id| s.get_mut(id).map(disconnected)).collect::<Vec<_>>())
            .await
            .into_iter()
            .flatten()
            .collect();
        for info in infos {
            hub.emit(&crate::wire::events::ServerEvent::Harness { info: Box::new(info) });
        }
    }

    /// What this server's copy of each session on `machine` has, said in digests: what a stream
    /// it starts sends only what the copy lacks of (`ServerMsg::Kept`, `crate::wire::sums`). Read
    /// from the copies' views: taking in updates waits for none of it.
    pub fn kept_on(&self, machine: &str) -> Vec<crate::proto::KeptSession> {
        let at = Location::Machine(machine.to_owned());
        let mut out = Vec::new();
        self.sessions.each(|id, v| {
            if *v.location == at {
                out.push(v.kept(id));
            }
        });
        out
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
        // What's kept is read first: the first time, off disk.
        let files = self.keeper.frame(id);
        let slot = self.sessions.slot(id)?;
        // Counted among its viewers first, without its owner (goal 4, plan 01a1a59c2a9c step 3):
        // from here on every change the owner publishes goes to the feed too, or is in the view
        // read below (`super::copies::Mirror::commit`).
        let viewer = slot.tap().join(takes);
        let refusals = viewer.refusals();
        // Gone already: nothing is made for it.
        if slot.closed() {
            return None;
        }
        // Where the feed ends, then the view: every frame of a change after that view is past it.
        let at = viewer.position();
        let view = slot.view();
        if view.closed {
            return None;
        }
        let parts = after.filter(|(b, _)| *b == born()).and_then(|(_, rev)| view.parts_since(rev)).unwrap_or_else(|| view.parts());
        let rev = parts.rev;
        // Written out by the caller off the runtime's threads: no other session's publish waits on
        // this one, nor any socket on its thread, nor its owner.
        let store = self.convo.clone().map(|s| (s, Arc::from(id)));
        Some(Attachment { snapshot: Snapshot { parts, files, store }, topic: viewer.topic().to_owned(), at, viewer, refusals, rev })
    }

    /// Catch-up for `viewer`, which fell behind having had revision `rev`: what it missed (it begins
    /// with `resumed`), or all of it when that can't be told apart (`open`); where on its topic it
    /// reads on from, taken before the view, so it misses nothing (and skips what the catch-up
    /// held); the feed's refusals then; and the copy's revision they bring it to. None when the
    /// session is gone.
    pub fn frames_since(&self, id: &str, viewer: &Viewer, rev: u64) -> Option<(Snapshot, Option<kiln_seams::bus::Offset>, u64, u64)> {
        let files = self.keeper.frame(id);
        let slot = self.sessions.slot(id)?;
        let refusals = viewer.refusals();
        let at = viewer.position();
        let view = slot.view();
        if view.closed {
            return None;
        }
        let parts = view.parts_since(rev).unwrap_or_else(|| view.parts());
        let now = parts.rev;
        let store = self.convo.clone().map(|s| (s, Arc::from(id)));
        Some((Snapshot { parts, files, store }, at, refusals, now))
    }

    /// Files a finished message mentioned, kept now (`super::kept`): its viewers hear of them.
    pub async fn files_added(&self, id: &str, added: &[Kept]) {
        let (sid, added) = (id.to_owned(), added.to_vec());
        self.sessions
            .on(id, move |s| {
                if let Some(m) = s.get_mut(&sid) {
                    // A revision of its own, past what its viewers have: else they'd take it as had.
                    m.rev = m.rev.saturating_add(1);
                    m.commit(|m| {
                        let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::Files { files: added }, rev: Some(m.rev) }.text();
                        Some(super::copies::payload_of(&text, m.rev))
                    });
                }
            })
            .await;
    }

    /// Where a session runs, when one of its messages mentions `path` (a relative mention, in its
    /// folder): a viewer may read that file from its machine, and no other.
    pub fn mentioned_where(&self, id: &str, path: &str) -> Option<Location> {
        let listed = self.keeper.lists_path(id, path);
        self.sessions
            .with(id, |v| {
                let cwd = &v.info.cwd;
                let said = || {
                    v.events
                        .iter()
                        .rev()
                        .filter(|e| matches!(e.entry.kind(), Some("user" | "assistant")))
                        .filter_map(|e| e.entry.value().get("text").and_then(Value::as_str).map(str::to_owned))
                        .any(|t| files::mentioned(&t).iter().any(|p| files::resolve(p, cwd).as_deref() == Some(path)))
                };
                (listed || said()).then(|| Location::clone(&v.location))
            })
            .flatten()
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
                    if let Some(pid) = spec.shell.as_ref().and_then(|t| self.local_shells.load().get(t).copied()) {
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
                    // Its copy made so the session lists before its first update arrives (unless
                    // that came first), there once this is answered.
                    let (sid, seed) = (info.id.clone(), Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus)));
                    self.sessions.on(&info.id, move |s| s.insert(sid, seed)).await;
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
                    let mut seed = Mirror::new(info.clone(), loc.clone(), Arc::clone(&self.bus));
                    seed.opened_at = Some(std::time::Instant::now());
                    self.sessions.on(&id.clone(), move |s| s.insert(id, seed)).await;
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
        // Taken on the starts' copy, swapped in whole: of two starts asking for one id at once, the
        // one whose copy goes in first has it, and the other finds it held.
        let taken = self.opening.update(|held| {
            let taken = ids.iter().find(|id| self.sessions.contains(id) || held.contains(id.as_str())).cloned();
            if taken.is_none() {
                held.extend(ids.iter().cloned());
            }
            taken
        });
        if let Some(id) = taken {
            return Err(anyhow::Error::new(IdTaken(id)));
        }
        Ok(Claim { held: &self.opening, ids: ids.to_vec() })
    }

    /// Keep an uploaded file for the messages that refer to it (by the returned id): until it
    /// expires, also after a message carried it (one sent again carries it too). Short of room, the
    /// files messages carried already go first, the oldest first.
    pub fn upload(&self, name: String, mime: String, data: bytes::Bytes) -> anyhow::Result<String> {
        let id = crate::util::new_id();
        let upload = Upload { name, mime, data, at: std::time::Instant::now(), sent: None };
        let len = upload.data.len();
        self.uploads.update(|g| {
            g.retain(|_, u| u.at.elapsed() < UPLOAD_TTL);
            let mut used: usize = g.values().map(|u| u.data.len()).sum();
            if used.saturating_add(len) > UPLOADS_MAX {
                let mut sent: Vec<(std::time::Instant, String)> = g.iter().filter_map(|(id, u)| u.sent.map(|t| (t, id.clone()))).collect();
                sent.sort();
                for (_, id) in sent {
                    if used.saturating_add(len) <= UPLOADS_MAX {
                        break;
                    }
                    if let Some(u) = g.remove(&id) {
                        used = used.saturating_sub(u.data.len());
                    }
                }
            }
            if used.saturating_add(len) > UPLOADS_MAX {
                anyhow::bail!("too many uploads waiting; send them first");
            }
            g.insert(id.clone(), upload.clone());
            Ok(())
        })?;
        Ok(id)
    }

    /// Upload `id`'s name, type and bytes, for a message that carries it: kept (marked sent), so a
    /// message sent again carries it as well.
    fn upload_of(&self, id: &str) -> Option<(String, String, bytes::Bytes)> {
        // Marked sent the first time, a copy swapped in then; read off the copy.
        if self.uploads.load().get(id)?.sent.is_none() {
            let now = std::time::Instant::now();
            self.uploads.update(|g| {
                if let Some(u) = g.get_mut(id) {
                    u.sent.get_or_insert(now);
                }
            });
        }
        self.uploads.load().get(id).map(|u| (u.name.clone(), u.mime.clone(), u.data.clone()))
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
        let seen = self.sessions.with(id, |v| v.info.epoch).unwrap_or(0);
        self.epochs.load().get(id).copied().unwrap_or(0).max(seen)
    }

    /// Waits for the answer to message `message` sent to session `id`: none when too many wait
    /// already (it isn't waited for). Its wait reaches the waits' owner before the message goes.
    async fn wait_for(&self, id: &str, message: &str) -> Option<futures::channel::oneshot::Receiver<Said>> {
        self.answers.wait_for(id, message).await
    }

    /// No longer waiting for message `message` sent to session `id`: the waits whose ends were
    /// dropped go (an answer for those still waiting still reaches them).
    fn unwait(&self, id: &str, message: &str) {
        self.answers.unwait(id, message);
    }

    /// Session `id`'s inbox said what became of a message: whoever waits for it longest is told (each
    /// send of it is answered once, in the order they went: two sent under one id at once, one
    /// taken and one refused, are each told their own).
    fn answered(&self, id: &str, said: Said) {
        crate::metrics::sends().said(&said);
        self.answers.answered(id, said);
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
            Some(Location::Machine(machine)) => hub.instances.load().wide_caps_for(&machine, &crate::instances::Owned::Agent(id.to_owned())) & crate::proto::CAP_INBOX != 0,
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
            self.set_keep_going(hub, id, value.as_ref().and_then(Value::as_bool).unwrap_or(true)).await;
            return Pending::Now(Sent::Ok);
        }
        // A name is kept here: the session takes it at once, whatever its machine can do (and
        // its harness names its conversation too, where it does).
        let renamed = matches!(&c, Command::Control { id: control, .. } if control == "rename");
        if let Command::Control { id: control, value } = &c
            && control == "rename"
        {
            self.rename(hub, id, value.as_ref()).await;
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
                let wait = match message.as_deref() {
                    Some(m) => self.wait_for(id, m).await,
                    None => None,
                };
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
                    let inst = hub.instances.load();
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
                    let wait = self.wait_for(id, &m).await;
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
        let (from, epoch) = self.sessions.with(id, |v| (Location::clone(&v.location), v.state.get("epoch").and_then(Value::as_u64).unwrap_or(0))).ok_or_else(|| "no such agent session".to_owned())?;
        let from_name = match &from {
            Location::Local => return Err("it runs on the server itself, which is up: move it instead".into()),
            Location::Machine(m) => m.clone(),
        };
        if hub.agents.load().get(&from_name).is_some_and(|a| a.connected) {
            return Err(format!("{} is online: move the session from there instead", hub.shown(&from_name)));
        }
        if !hub.agents.load().get(to).is_some_and(|a| a.connected) {
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
        let heard = hub.agents.load().get(&from_name).map(|a| a.last_seen).unwrap_or_default();
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
        let (sid, there) = (id.to_owned(), Location::Machine(to.to_owned()));
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).map(|m| {
                    m.location = Arc::new(there);
                    let info = Arc::make_mut(&mut m.info);
                    info.target = m.location.target();
                    info.status = Status::Starting;
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
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
        if !hub.agents.load().get(to).is_some_and(|a| a.connected) {
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
        let had = self.sessions.with(id, |v| v.state.get("epoch").and_then(Value::as_u64)).flatten().unwrap_or(0);
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
        let (sid, there) = (id.to_owned(), Location::Machine(to.to_owned()));
        let info = self
            .sessions
            .on(id, move |s| {
                s.get_mut(&sid).filter(|m| *m.location == Location::Local).map(|m| {
                    m.location = Arc::new(there);
                    let info = Arc::make_mut(&mut m.info);
                    info.target = m.location.target();
                    info.status = Status::Starting;
                    m.opened_at = Some(std::time::Instant::now());
                    m.commit(|_| None);
                    Info::clone(&m.info)
                })
            })
            .await
            .flatten();
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
            // Its viewers are told, and its feed's topic goes with the last of them.
            let sid = id.to_owned();
            self.sessions
                .on(id, move |s| {
                    s.remove(&sid);
                })
                .await;
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
        // Read from its view (no copy of it made).
        let state = self.sessions.with(id, |v| Arc::clone(&v.state))?;
        State::deserialize(&*state).ok()
    }

    /// A session as lists show it.
    pub fn info(&self, id: &str) -> Option<Info> {
        self.sessions.with(id, |v| Info::clone(&v.info))
    }

    /// What waits in session `id`'s queue, as its machine last said, the messages whole.
    pub fn queue(&self, id: &str) -> Vec<super::model::QueueItem> {
        let Some(state) = self.sessions.with(id, |v| Arc::clone(&v.state)) else { return Vec::new() };
        state.get("queue").and_then(|q| Vec::<super::model::QueueItem>::deserialize(q).ok()).unwrap_or_default()
    }

    /// What `agents.get` and `agents.result` read of a session (`harness::ops`): its state as
    /// last reported, its last reply on its main thread (an `assistant` event) and its changeset.
    pub fn conclusion(&self, id: &str) -> Option<(Value, Option<Value>, Value)> {
        // References from its view; the copies made after.
        let (state, reply, changes) = self.sessions.with(id, |v| {
            let reply = v.events.iter().rev().find(|e| e.entry.kind() == Some("assistant") && e.entry.on_main_thread()).map(|e| Arc::clone(&e.entry));
            (Arc::clone(&v.state), reply, v.changes.clone())
        })?;
        let changes = changes.map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value()).collect()));
        Some((Value::clone(&state), reply.map(|e| e.value()), changes))
    }

    /// Tests: session `id`'s changeset, as its machine would report it.
    #[cfg(test)]
    pub async fn set_changes(&self, id: &str, files: Value) -> bool {
        let files: Option<Files> = match files {
            Value::Array(all) => Some(Arc::new(all.into_iter().map(|f| Arc::new(Entry::new(f))).collect())),
            _ => None,
        };
        let sid = id.to_owned();
        self.sessions
            .on(id, move |s| {
                s.get_mut(&sid)
                    .map(|m| {
                        m.changes = files;
                        m.commit(|_| None);
                    })
                    .is_some()
            })
            .await
            .unwrap_or(false)
    }
}

/// A copy whose machine (or runner process) went away: said disconnected, to its viewers too.
fn disconnected(m: &mut Mirror) -> Info {
    Arc::make_mut(&mut m.info).status = Status::Disconnected;
    if let Some(o) = Arc::make_mut(&mut m.state).as_object_mut() {
        o.insert("status".into(), json!("disconnected"));
    }
    m.commit(|m| m.state_frame());
    Info::clone(&m.info)
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
            let gone = r.wait_for("s1", "m1").await;
            let kept = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            drop(gone);
            r.unwait("s1", "m1");
            assert_eq!(r.answers.held().await, Some(1), "the one still waiting stays");
            let said = Said { message: "m1".into(), answer: Err(inbox::Refusal::NoSession { session: "s1".into() }) };
            r.answered("s1", said.clone());
            assert_eq!(r.answers.held().await, Some(0));
            assert_eq!(kept.await.ok(), Some(said));
            // Given up before any answer: nothing is left.
            let w = r.wait_for("s2", "m2").await;
            drop(w);
            r.unwait("s2", "m2");
            assert_eq!(r.answers.held().await, Some(0));
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
            let first = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let second = r.wait_for("s1", "m1").await.ok_or_else(|| anyhow::anyhow!("not waited for"))?;
            let taken = Said { message: "m1".into(), answer: Ok(inbox::Answer { session: "s1".into(), message: "m1".into(), epoch: 0, at: 1, seq: 7, repeat: false }) };
            let reused = Said { message: "m1".into(), answer: Err(inbox::Refusal::Reused { session: "s1".into(), message: "m1".into() }) };
            r.answered("s1", taken.clone());
            r.answered("s1", reused.clone());
            assert_eq!(first.await.ok(), Some(taken));
            assert_eq!(second.await.ok(), Some(reused));
            assert_eq!(r.answers.held().await, Some(0));
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
        m.changes.as_ref().map_or(Value::Null, |files| Value::Array(files.iter().map(|f| f.value()).collect()))
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
        let had = m.index.clone();
        m.reindex();
        assert_eq!(had, m.index, "the index as made again");
        // A reply's words still go to its event, at the revision they came.
        let last = format!("e{n}");
        took(&mut m, |m| m.append(&last, "more"));
        let at = m.index.get(last.as_str()).copied().unwrap_or(usize::MAX);
        assert_eq!(m.events.get(at).and_then(|e| e.entry.value().get("text").and_then(Value::as_str).map(str::to_owned)).as_deref(), Some("xmore"));
        assert_eq!(m.events.get(at).map(|e| e.entry.text().contains("xmore")), Some(true), "its text written again");
        assert_eq!(m.events.get(at).map(|e| e.at), Some(m.rev), "changed at the revision it came");
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
        let read = futures::executor::block_on(bus.read(&m.feed.topic(), from, 100))?;
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
        let mut made = false;
        m.commit(|m| {
            made = true;
            m.state_frame()
        });
        assert!(made && !m.sent.is_null());
        drop(viewer);
        set_state(&mut m, json!({"status": "running", "activity": "editing"}));
        let mut made = false;
        m.commit(|m| {
            made = true;
            m.state_frame()
        });
        assert!(!made, "no frame made with none watching");
        assert!(m.sent.is_null());
        let (_viewer, at) = m.feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        let snapshot = Snapshot { parts: m.view().parts(), files: None, store: None }.frames();
        assert!(snapshot.iter().any(|f| serde_json::from_str::<Value>(f).ok() == Some(json!({"t": "state", "state": *m.state}))));
        set_state(&mut m, json!({"status": "idle"}));
        m.commit(|m| m.state_frame());
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
        let values: Vec<Value> = files.iter().map(|f| f.value()).collect();
        assert_eq!(values, vec![file("a.rs", None, 1), file("a.rs", Some("/w"), 5), file("c.rs", None, 1)]);
        // Many set at once (a changeset sent whole) go by key the same way.
        let mut files = entries((0..20).map(|n| file(&format!("f{n}.rs"), None, 1)).collect());
        let set: Vec<Value> = (10..30).rev().map(|n| file(&format!("f{n}.rs"), None, 2)).collect();
        merge_changes(&mut files, set, &[]);
        let paths: Vec<String> = files.iter().filter_map(|f| f.file().0.map(str::to_owned)).collect();
        let want: Vec<String> = (0..20).chain((20..30).rev()).map(|n| format!("f{n}.rs")).collect();
        assert_eq!(paths, want);
        assert!(files.iter().skip(10).all(|f| f.value().get("added") == Some(&json!(2))));
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
        assert_eq!(Value::Array(had.iter().map(|f| f.value()).collect()), changes_of(&m));
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

    /// Goal 10, plan 01a1a87b2dae step 5 (and goal 9 step 3's owner's poll): a state's update encodes its
    /// session's info once, for its view, and the events socket's frame of it is those bytes, as
    /// `ServerEvent::Harness` wrote it before; a state that changes nothing its viewer has goes as no frame;
    /// the same meta again is kept, encoded and sent as nothing, a new one encoded once, its frame as before.
    #[test]
    fn g10_s5_an_update_encodes_once_what_it_changed() -> anyhow::Result<()> {
        use crate::harness::model::{Meta, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Watching, so the feed publishes; and the events socket's frames, as a page takes them.
            let (_watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            let mut events = hub.harness_events.subscribe(16)?;
            let encodings = || async { hub.harness.sessions.on("s", |s| s.get("s").map(|m| m.encodings)).await.flatten().unwrap_or_default() };
            let state = State { summary: "the retry loop reads its deadline".into(), ..State::default() };
            let was = encodings().await;
            hub.harness.ingest(&hub, &there, "s", Update::State { state: Box::new(state.clone()) }).await;
            let now = encodings().await;
            anyhow::ensure!(now.info == was.info + 1, "the info encoded once: {was:?} then {now:?}");
            let view = hub.harness.sessions.view("s").ok_or_else(|| anyhow::anyhow!("no view"))?;
            let frame = loop {
                let f = events.try_recv().map_err(|e| anyhow::anyhow!("no info frame on the events socket: {e:?}"))?;
                if f.starts_with(r#"{"type":"harness""#) {
                    break f;
                }
            };
            assert_eq!(&*frame, crate::wire::events::ServerEvent::Harness { info: Box::new((*view.info).clone()) }.text(), "the frame as before");
            assert_eq!(frame.as_bytes().get(r#"{"type":"harness","info":"#.len()..frame.len() - 1), Some(view.info_json.as_ref()), "the view's own bytes");
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.iter().filter(|f| f.starts_with(r#"{"t":"state""#)).count() == 1, "one state frame: {heard:?}");
            // The same state again: its info encoded for its view (its last activity moves), no frame.
            hub.harness.ingest(&hub, &there, "s", Update::State { state: Box::new(state) }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.is_empty(), "no frame for a state that changed nothing its viewer has: {heard:?}");
            // A meta: one frame, as it was written before, encoded once; the same again, nothing.
            let meta = Meta { version: "2.1".into(), agents: vec!["explore".into()], ..Meta::default() };
            let was = encodings().await;
            hub.harness.ingest(&hub, &there, "s", Update::Meta { meta: Box::new(meta.clone()) }).await;
            let heard = sent(&hub, &topic, &mut at);
            let mut want = Vec::new();
            write::value(&mut want, "meta", "meta", &serde_json::to_value(&meta)?, hub.harness.rev("s"))?;
            anyhow::ensure!(heard == vec![String::from_utf8(want)?], "the meta's frame as before: {heard:?}");
            anyhow::ensure!(encodings().await.meta == was.meta + 1, "the meta encoded once");
            hub.harness.ingest(&hub, &there, "s", Update::Meta { meta: Box::new(meta) }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.is_empty() && encodings().await.meta == was.meta + 1, "the same meta again: nothing kept, encoded or sent: {heard:?}");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 5: `GET /api/harness/sessions`' list is joined once for all the reads
    /// that find the sessions as they were, each answered those same bytes (nothing joined, sorted or copied
    /// for it); a session whose info changed, one that came and one that went each have it joined again once,
    /// saying what `list()` says, oldest first; events that change no session's info leave it as it was.
    #[test]
    fn g10_s5_the_sessions_list_is_joined_once_for_the_reads_that_find_it_as_it_was() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let open = |id: &str, at: i64| Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), created_at: at, ..Info::default() }), since: None };
            for (id, at) in [("b", 2), ("a", 1), ("c", 3)] {
                hub.harness.ingest(&hub, &there, id, open(id, at)).await;
            }
            let listed = || anyhow::Ok(serde_json::to_vec(&hub.harness.list())?);
            let joins = || hub.harness.sessions.list_joins();
            let first = hub.harness.list_json();
            let at = joins();
            anyhow::ensure!(first.as_ref() == listed()?.as_slice(), "as the list says, oldest first: {}", String::from_utf8_lossy(&first));
            for _ in 0..3 {
                let again = hub.harness.list_json();
                anyhow::ensure!(again.as_ptr() == first.as_ptr() && again == first, "the same bytes, not joined again");
            }
            anyhow::ensure!(joins() == at, "nothing joined for the reads after the first");
            // A session's info changed: joined again once, for itself and the reads after.
            let state = State { summary: "the retry loop reads its deadline".into(), ..State::default() };
            hub.harness.ingest(&hub, &there, "b", Update::State { state: Box::new(state) }).await;
            let changed = hub.harness.list_json();
            anyhow::ensure!(changed.as_ptr() != first.as_ptr() && changed.as_ref() == listed()?.as_slice(), "the change in it");
            anyhow::ensure!(String::from_utf8_lossy(&changed).contains("the retry loop reads its deadline"), "its summary said");
            anyhow::ensure!(hub.harness.list_json().as_ptr() == changed.as_ptr() && joins() == at + 1, "joined once for it");
            // Events whose copy's info stays as it was leave the list as it was.
            let said = |n: u64| Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{n}: the deadline"), streaming: false } };
            let before = hub.harness.info_json("a");
            hub.harness.ingest(&hub, &there, "a", Update::Events { events: (1..=3).map(said).collect() }).await;
            if hub.harness.info_json("a") == before {
                anyhow::ensure!(hub.harness.list_json().as_ptr() == changed.as_ptr() && joins() == at + 1, "events that changed no info: the list as it was");
            }
            // One comes, oldest of all; one goes.
            let joined = joins();
            hub.harness.ingest(&hub, &there, "d", open("d", 0)).await;
            let more = hub.harness.list_json();
            anyhow::ensure!(more.as_ref() == listed()?.as_slice() && more.starts_with(br#"[{"id":"d""#) && joins() == joined + 1, "one came: {}", String::from_utf8_lossy(&more));
            hub.harness.ingest(&hub, &there, "c", Update::Closed).await;
            let fewer = hub.harness.list_json();
            anyhow::ensure!(fewer.as_ref() == listed()?.as_slice() && !String::from_utf8_lossy(&fewer).contains(r#""id":"c""#) && joins() == joined + 2, "one went: {}", String::from_utf8_lossy(&fewer));
            anyhow::ensure!(hub.harness.list_json().as_ptr() == fewer.as_ptr() && joins() == joined + 2, "and then as it was");
            Ok(())
        })
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
            hub.harness.ingest(&hub, &there, "s", open()).await;
            let user = |id: &str, seq: u64, text: &str| Event {
                id: id.into(),
                seq,
                at: 1,
                parent: None,
                by: None,
                body: Body::User { text: text.into(), images: 0, client_id: Some(id.into()), origin: None, attachments: Vec::new(), reply_to: None },
            };
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass"), user("m-person", 2, "hello"), user("news-1", 3, "one child settled")] }).await;
            // Upserted again (its delivery's state), with other words: what it was handed stays.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![user("goal-1", 1, "/goal tests pass, edited later")] }).await;
            // Its runner's state says another handed.
            let given = |g: &[(&str, &str)]| Update::State { state: Box::new(State { given: g.iter().map(|(m, t)| Given { id: (*m).into(), digest: stable(t) }).collect(), ..State::default() }) };
            hub.harness.ingest(&hub, &there, "s", given(&[("goal-2", "/goal docs build")])).await;
            let want = vec![
                Given { id: "goal-1".into(), digest: stable("/goal tests pass") },
                Given { id: "news-1".into(), digest: stable("one child settled") },
                Given { id: "goal-2".into(), digest: stable("/goal docs build") },
            ];
            let said = || hub.harness.info("s").map(|i| i.said).unwrap_or_default();
            assert_eq!(said(), want);
            // Its runner comes back, then says less (started again): all of it stays.
            hub.harness.ingest(&hub, &there, "s", open()).await;
            hub.harness.ingest(&hub, &there, "s", given(&[])).await;
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
            hub.harness.ingest(&hub, &there, "s", open(None)).await;
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: (1..=10).map(said).collect() }).await;
            let attached = hub.harness.attach("s").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            // Watching, so the feed publishes; and where on its topic the viewer reads from.
            let (watching, topic, mut at) = (attached.viewer, attached.topic, attached.at.unwrap_or_default());
            let resync = |heard: &[String]| heard.iter().any(|f| f == crate::harness::live::RESYNC);
            // Its machine again, with all ten: the copy goes on, and its viewer only hears what follows.
            hub.harness.ingest(&hub, &there, "s", open(Some(10))).await;
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(11)] }).await;
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(heard.len() == 1 && !heard.iter().any(|f| f.contains(r#""t":"open""#)) && !resync(&heard), "the viewer goes on: {heard:?}");
            let had = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default();
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=11));
            // Again, with only eight of them (its store hadn't kept the rest): the copy lets go of
            // nine and up, and its viewer starts over from the copy: told to by the feed, and what it
            // had can't be resumed, so it's sent the copy whole.
            hub.harness.ingest(&hub, &there, "s", open(Some(8))).await;
            anyhow::ensure!(copy_of(&hub, "s") == names(1..=8));
            let heard = sent(&hub, &topic, &mut at);
            anyhow::ensure!(resync(&heard), "the viewer is sent back to a snapshot: {heard:?}");
            let again = hub.harness.frames_since("s", &watching, had).ok_or_else(|| anyhow::anyhow!("no copy"))?.0.frames();
            anyhow::ensure!(again.first().is_some_and(|f| f.contains(r#""t":"open""#)) && ids(&again) == names(1..=8), "the viewer starts over: {again:?}");
            let kept = hub.harness.kept_on("box");
            anyhow::ensure!(kept.len() == 1 && kept.iter().all(|k| (k.epoch, k.from, k.seq, k.last.len()) == (3, 1, 8, 8)), "{kept:?}");
            // An event sent again under a number the copy has under another id: in its place.
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![Event { id: "other".into(), ..said(5) }] }).await;
            let mut want = names(1..=8);
            if let Some(slot) = want.get_mut(4) {
                "other".clone_into(slot);
            }
            anyhow::ensure!(copy_of(&hub, "s") == want, "{:?}", copy_of(&hub, "s"));
            anyhow::ensure!(resync(&sent(&hub, &topic, &mut at)), "the viewer starts over");
            // One it lacks under a number before its newest: in among the others, by number.
            hub.harness.ingest(&hub, &there, "s", open(Some(8))).await;
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(12), said(14)] }).await;
            hub.harness.ingest(&hub, &there, "s", Update::Events { events: vec![said(13)] }).await;
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
            let mut opened = Mirror::new(info("opened"), there.clone(), bus);
            opened.opened_at = Some(std::time::Instant::now());
            hub.harness.sessions.on("opened", move |s| s.insert("opened".into(), opened)).await;
            hub.harness.ingest(&hub, &there, "announced", Update::Open { info: Box::new(info("announced")), since: None }).await;
            let att = hub.harness.attach("announced").ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let at = att.at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default()).await;
            let left = hub.harness.ids();
            anyhow::ensure!(left == vec!["opened".to_owned()], "{left:?}");
            // Its viewer hears it's gone: its feed's last frame.
            let read = futures::executor::block_on(kiln_seams::Bus::read(&hub.bus, &att.topic, at, 100))?;
            let said: Vec<String> = read.messages.iter().filter_map(|m| super::super::live::decode(&m.payload)).map(|f| f.text.to_string()).collect();
            anyhow::ensure!(said.last().is_some_and(|t| t == super::super::live::closed()), "its viewer heard {said:?}");
            // Its machine announces it: from then on its reports count.
            hub.harness.ingest(&hub, &there, "opened", Update::Open { info: Box::new(info("opened")), since: None }).await;
            hub.harness.inventory(&hub, "box", None, &[], crate::harness::host::Inventory::default()).await;
            anyhow::ensure!(hub.harness.ids().is_empty());
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
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(info(id)), since: None }).await;
            }
            hub.harness.held_by("old-1", "01a14794");
            hub.harness.held_by("old-2", "01a14794");
            hub.harness.held_by("mine", "main");
            let report = |ids: &[&str]| crate::harness::host::Inventory { sessions: ids.iter().map(|s| (*s).to_owned()).collect(), ..Default::default() };
            let left = |hub: &crate::hub::Hub| hub.harness.ids();
            // `main` says what it holds while the other process is away.
            hub.harness.inventory(&hub, "box", Some("main"), &["main".to_owned()], report(&["mine"])).await;
            anyhow::ensure!(left(&hub) == ["mine", "old-1", "old-2"], "{:?}", left(&hub));
            let status = |id: &str| hub.harness.info(id).map(|i| i.status);
            anyhow::ensure!(status("old-1") == Some(Status::Disconnected) && status("old-2") == Some(Status::Disconnected), "away: {:?} {:?}", status("old-1"), status("old-2"));
            anyhow::ensure!(status("mine") != Some(Status::Disconnected), "{:?}", status("mine"));
            // The process that holds them says it holds one of them: the other ends.
            hub.harness.inventory(&hub, "box", Some("01a14794"), &["main".to_owned(), "01a14794".to_owned()], report(&["old-1"])).await;
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
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            let change = |n: u32| crate::harness::model::FileChange { path: format!("src/f{}.rs", n % 7), tree: None, created: false, deleted: false, added: n, removed: 0, hunks: Vec::new(), truncated: false, at: i64::from(n), by: Vec::new(), place: None };
            for n in 0..200 {
                hub.harness.ingest(&hub, &there, "s", Update::ChangesPatch { set: vec![change(n)], gone: Vec::new() }).await;
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
            hub.harness.ingest(&hub, &b, "twin", Update::Open { info: Box::new(info("twin", 1)), since: None }).await;
            hub.harness.ingest(&hub, &b, "arriving", Update::Open { info: Box::new(info("arriving", 0)), since: None }).await;
            hub.harness.ingest(&hub, &a, "here", Update::Open { info: Box::new(info("here", 0)), since: None }).await;
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
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(info(id)), since: None }).await;
            }
            hub.harness.ingest(&hub, &Location::Machine("elsewhere".into()), "away", Update::Open { info: Box::new(info("away")), since: None }).await;
            hub.harness.machine_lost(&hub, "box").await;
            hub.harness.machine_lost(&hub, "elsewhere").await;
            // Back: one has something to say at once, the other is quiet.
            hub.harness.ingest(&hub, &there, "busy", Update::Open { info: Box::new(info("busy")), since: None }).await;
            let held = ["quiet", "busy", "away", "unknown"].map(str::to_owned);
            let stale = hub.harness.still_disconnected("box", &held);
            anyhow::ensure!(stale == ["quiet"], "only its own quiet session: {stale:?}");
            Ok(())
        })
    }

    /// Session `id`'s events as its copy has them: each one's id and words, in order.
    fn said_in(hub: &Hub, id: &str) -> Vec<(String, String)> {
        hub.harness
            .sessions
            .with(id, |v| v.events.iter().map(|e| (e.entry.id().unwrap_or_default().to_owned(), e.entry.value().get("text").and_then(Value::as_str).unwrap_or_default().to_owned())).collect())
            .unwrap_or_default()
    }

    /// What a viewer's frames make of a session's events, taken into `events` (by number): an
    /// `open` starts it over, `events` put each in its place, `stream` adds to one's words.
    fn take_frames<'a>(events: &mut std::collections::BTreeMap<u64, (String, String)>, frames: impl IntoIterator<Item = &'a str>) -> anyhow::Result<()> {
        for f in frames {
            let v: Value = serde_json::from_str(f)?;
            match v.get("t").and_then(Value::as_str) {
                Some("open") => events.clear(),
                Some("events") => {
                    for e in v.get("events").and_then(Value::as_array).into_iter().flatten() {
                        let seq = e.get("seq").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("an event without its number: {e}"))?;
                        let id = e.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                        events.insert(seq, (id, e.get("text").and_then(Value::as_str).unwrap_or_default().to_owned()));
                    }
                }
                Some("stream") => {
                    let (id, text) = (v.get("id").and_then(Value::as_str).unwrap_or_default(), v.get("text").and_then(Value::as_str).unwrap_or_default());
                    let e = events.values_mut().find(|(e, _)| e == id).ok_or_else(|| anyhow::anyhow!("words for an event the viewer hasn't: {id}"))?;
                    e.1.push_str(text);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A viewer of session `id` that joins after `after` (as a page does, taking states and
    /// changesets as patches) and reads its feed, as the server's viewer loop does, until the
    /// updates stop (`done`) and it has the copy's newest revision: the events it ends with, and
    /// how many live frames it took.
    async fn watch(hub: Arc<Hub>, id: String, after: std::time::Duration, done: Arc<std::sync::atomic::AtomicBool>) -> anyhow::Result<(String, Vec<(String, String)>, usize)> {
        use kiln_seams::Bus as _;
        crate::rt::sleep(after).await;
        let att = hub.harness.attach_from(&id, None, CHANGES).ok_or_else(|| anyhow::anyhow!("no copy of {id}"))?;
        let mut events = std::collections::BTreeMap::new();
        take_frames(&mut events, att.frames().iter().map(String::as_str))?;
        let (mut at, mut have, mut live) = (att.at.ok_or_else(|| anyhow::anyhow!("no feed"))?, att.rev, 0);
        loop {
            let batch = hub.bus.read(&att.topic, at, 256).await?;
            if batch.messages.is_empty() {
                if done.load(std::sync::atomic::Ordering::Acquire) && hub.harness.rev(&id).is_some_and(|r| have >= r) {
                    break;
                }
                crate::rt::sleep(std::time::Duration::from_millis(2)).await;
                continue;
            }
            at = batch.next;
            for m in batch.messages {
                let f = super::super::live::decode(&m.payload).ok_or_else(|| anyhow::anyhow!("a frame the feed didn't make"))?;
                if &*f.text == super::super::live::RESYNC {
                    // Back to the copy, from what it has, as a viewer that fell behind.
                    let (snapshot, from, _, rev) = hub.harness.frames_since(&id, &att.viewer, have).ok_or_else(|| anyhow::anyhow!("{id} gone"))?;
                    take_frames(&mut events, snapshot.frames().iter().map(String::as_str))?;
                    (at, have) = (from.ok_or_else(|| anyhow::anyhow!("no feed"))?, rev);
                    break;
                }
                if f.rev != 0 && f.rev <= have {
                    continue;
                }
                take_frames(&mut events, [f.for_viewer(CHANGES)])?;
                have = have.max(f.rev);
                live += 1;
            }
        }
        Ok((id, events.into_values().collect(), live))
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: sixteen sessions' updates handed to their owners from every
    /// shard at once, every shard kept busy, and viewers joining while they come (others joining
    /// and leaving all the while). Each copy takes every event once, in order, with each reply's
    /// words in the order they came; and each viewer, from its snapshot and the frames after it,
    /// ends with what the copy has: none missed, whenever it joined.
    #[test]
    fn updates_keep_their_order_and_none_is_lost_on_busy_shards() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        use std::sync::atomic::{AtomicBool, Ordering};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let ids: Vec<String> = (0..16).map(|n| format!("s{n}")).collect();
            for id in &ids {
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            // Every shard kept busy: tasks that hold it a step at a time, giving it up only to
            // what wants it.
            let (busy, done) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
            for _ in 0..crate::rt::shard_count().max(1) * 2 {
                let busy = Arc::clone(&busy);
                crate::rt::spawn_spread(async move {
                    while busy.load(Ordering::Acquire) {
                        let t = std::time::Instant::now();
                        while t.elapsed() < std::time::Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                        crate::rt::maybe_yield().await;
                    }
                });
            }
            let per = 100u64;
            let (sent_tx, sent_rx) = async_channel::unbounded::<()>();
            for id in ids.clone() {
                let (hub, there, sent) = (Arc::clone(&hub), there.clone(), sent_tx.clone());
                crate::rt::spawn_spread(async move {
                    for n in 1..=per {
                        let e = format!("{id}-e{n}");
                        let event = Event { id: e.clone(), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: String::new(), streaming: true } };
                        hub.harness.ingest_from(&hub, &there, &id, Update::Events { events: vec![event] }, false).await;
                        for part in ["a", "b"] {
                            hub.harness.ingest_from(&hub, &there, &id, Update::Stream { id: e.clone(), text: format!("{n}{part}") }, false).await;
                        }
                        // As a runner's updates come: in bursts, a while apart.
                        if n % 2 == 0 {
                            crate::rt::sleep(std::time::Duration::from_millis(3)).await;
                        }
                    }
                    let _ = sent.send(()).await;
                });
            }
            drop(sent_tx);
            // Three viewers join each session at moments of their own while its updates come, and stay.
            let mut viewers = Vec::new();
            for (k, id) in ids.iter().enumerate() {
                for j in 0..3u64 {
                    let after = std::time::Duration::from_millis((u64::try_from(k).unwrap_or(0) * 7 + j * 41) % 140);
                    viewers.push(crate::rt::spawn(watch(Arc::clone(&hub), id.clone(), after, Arc::clone(&done))));
                }
            }
            // Others come and go: the feeds' topics let go and made again, generation by generation.
            for id in ids.iter().step_by(3) {
                let (hub, id, done) = (Arc::clone(&hub), id.clone(), Arc::clone(&done));
                crate::rt::spawn_spread(async move {
                    while !done.load(Ordering::Acquire) {
                        drop(hub.harness.attach_from(&id, None, CHANGES));
                        crate::rt::sleep(std::time::Duration::from_millis(1)).await;
                    }
                });
            }
            for _ in 0..ids.len() {
                sent_rx.recv().await?;
            }
            hub.harness.settled().await;
            done.store(true, Ordering::Release);
            busy.store(false, Ordering::Release);
            for id in &ids {
                let want: Vec<(String, String)> = (1..=per).map(|n| (format!("{id}-e{n}"), format!("{n}a{n}b"))).collect();
                anyhow::ensure!(said_in(&hub, id) == want, "{id}'s copy: {:?}", said_in(&hub, id).iter().take(4).collect::<Vec<_>>());
                anyhow::ensure!(hub.harness.rev(id) == Some(1 + per * 3), "{id}'s revision {:?}", hub.harness.rev(id));
            }
            let (count, mut lively) = (viewers.len(), 0);
            for v in viewers {
                let (id, seen, live) = crate::rt::timeout(std::time::Duration::from_secs(30), v).await.map_err(|_| anyhow::anyhow!("a viewer never caught up"))?.map_err(|_| anyhow::anyhow!("a viewer ended"))??;
                anyhow::ensure!(seen == said_in(&hub, &id), "{id}'s viewer has {} events, the copy {}: {:?}", seen.len(), said_in(&hub, &id).len(), seen.iter().zip(said_in(&hub, &id)).find(|(a, b)| *a != b));
                lively += usize::from(live > 0);
            }
            // Most joined while the updates came, and took the rest of them live.
            anyhow::ensure!(lively * 2 > count, "{lively} of {count} viewers took live frames");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: the same through runners' path, each update judged and taken by
    /// its session's owner (`Registry::from_runner`; goal 10, plan 01a1a87b2dae step 4), an update's events
    /// taken in turns of its owner's (goal 9, plan 01a1a87b2cec step 3). Sixteen sessions' runner updates are
    /// handed over from every shard at once, every shard kept busy, every fourth a run of eight events an
    /// owner's turn may run out in, each event's words streamed after it and a state after each: each copy
    /// takes every event once, in order, with its words in the order they came, and its last state.
    #[test]
    fn g4_s5_runner_updates_keep_their_order_and_none_is_lost_on_busy_shards() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event, State};
        use std::sync::atomic::{AtomicBool, Ordering};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let ids: Vec<String> = (0..16).map(|n| format!("r{n}")).collect();
            for id in &ids {
                hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(Info { id: id.clone(), kind: "claude".into(), ..Info::default() }), since: None }).await;
            }
            // Every shard kept busy, giving itself up only to what wants it.
            let busy = Arc::new(AtomicBool::new(true));
            for _ in 0..crate::rt::shard_count().max(1) * 2 {
                let busy = Arc::clone(&busy);
                crate::rt::spawn_spread(async move {
                    while busy.load(Ordering::Acquire) {
                        let t = std::time::Instant::now();
                        while t.elapsed() < std::time::Duration::from_micros(200) {
                            std::hint::spin_loop();
                        }
                        crate::rt::maybe_yield().await;
                    }
                });
            }
            let per = 60u64;
            let (sent_tx, sent_rx) = async_channel::unbounded::<()>();
            for id in ids.clone() {
                let (hub, sent) = (Arc::clone(&hub), sent_tx.clone());
                crate::rt::spawn_spread(async move {
                    let runner = |update: Update| crate::harness::copies::RunnerUpdate { peer: 7, agent: "box".into(), session: id.clone(), update, shipped: false, arrived: None, handed: None };
                    let mut n = 0u64;
                    while n < per {
                        let run = if n.is_multiple_of(4) { 8.min(per - n) } else { 1 };
                        let events: Vec<Event> = (n + 1..=n + run).map(|q| Event { id: format!("{id}-e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: String::new(), streaming: true } }).collect();
                        hub.harness.from_runner(runner(Update::Events { events })).await;
                        for q in n + 1..=n + run {
                            for part in ["a", "b"] {
                                hub.harness.from_runner(runner(Update::Stream { id: format!("{id}-e{q}"), text: format!("{q}{part}") })).await;
                            }
                        }
                        n += run;
                        hub.harness.from_runner(runner(Update::State { state: Box::new(State { summary: format!("through {n}"), ..State::default() }) })).await;
                        if n.is_multiple_of(2) {
                            crate::rt::sleep(std::time::Duration::from_millis(2)).await;
                        }
                    }
                    let _ = sent.send(()).await;
                });
            }
            drop(sent_tx);
            for _ in 0..ids.len() {
                sent_rx.recv().await?;
            }
            hub.harness.settled().await;
            busy.store(false, Ordering::Release);
            for id in &ids {
                let want: Vec<(String, String)> = (1..=per).map(|n| (format!("{id}-e{n}"), format!("{n}a{n}b"))).collect();
                anyhow::ensure!(said_in(&hub, id) == want, "{id}'s copy: {:?}", said_in(&hub, id).iter().take(4).collect::<Vec<_>>());
                let summary = hub.harness.sessions.with(id, |v| v.info.summary.clone());
                anyhow::ensure!(summary.as_deref() == Some(format!("through {per}").as_str()), "{id}'s last state: {summary:?}");
            }
            Ok(())
        })
    }

    #[test]
    fn names_are_kept_across_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Registry::new(dir.path().to_owned());
        r.names.update(|n| n.insert("s1".into(), "Mantle".into()));
        r.save_names();
        // Written off the calling thread, soon after (goal 3, plan 01a1a59c2a8e step 5).
        let kept = || Registry::new(dir.path().to_owned()).names.load().get("s1").cloned();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while kept().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(kept().as_deref(), Some("Mantle"));
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae step 2: a start's ids are claimed on the starts' copy, swapped in
    /// whole, with no lock: of threads claiming one id at once (four, as many as `crate::rcu`'s tests
    /// race), one has it and the rest find it held; let go, it's free again.
    #[test]
    fn g5_of_starts_claiming_one_id_at_once_one_has_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = Arc::new(Registry::new(dir.path().to_owned()));
        let threads = 4;
        for round in 0..50 {
            let id = format!("s{round}");
            // All claim at once, and each holds what it got until all have tried.
            let (start, tried) = (Arc::new(std::sync::Barrier::new(threads)), Arc::new(std::sync::Barrier::new(threads)));
            let claimers: Vec<_> = (0..threads)
                .map(|_| {
                    let (r, id, start, tried) = (Arc::clone(&r), id.clone(), Arc::clone(&start), Arc::clone(&tried));
                    std::thread::spawn(move || {
                        start.wait();
                        let claim = r.claim(std::slice::from_ref(&id));
                        let had = claim.is_ok();
                        tried.wait();
                        drop(claim);
                        had
                    })
                })
                .collect();
            let mut had = 0;
            for c in claimers {
                had += usize::from(c.join().map_err(|_| anyhow::anyhow!("a claimer panicked"))?);
            }
            anyhow::ensure!(had == 1, "round {round}: {had} had {id}");
            anyhow::ensure!(r.claim(std::slice::from_ref(&id)).is_ok(), "round {round}: {id} is free again once let go");
        }
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
            keeper.names.update(|n| n.insert("s1".into(), "Mantle".into()));
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

    /// Every kind of event, with what each may carry and what it may leave out.
    fn every_kind_of_event() -> Vec<crate::harness::model::Event> {
        use crate::harness::model::{Body, Event, FileDiff, Hunk, Level, Todo, ToolCall, ToolKind, ToolStatus};
        let event = |n: u64, parent: Option<&str>, body: Body| Event { id: format!("e{n}"), seq: n, at: 1_793_800_000_000, parent: parent.map(str::to_owned), by: n.is_multiple_of(2).then(|| "Claude Code".to_owned()), body };
        let tool = ToolCall {
            tool_id: "t1".into(),
            name: "mcp__github__search".into(),
            tool: ToolKind::Edit,
            title: "Edit src/lib.rs".into(),
            detail: Some("a \"quoted\" detail\n".into()),
            // A path and a tree inside it, not of the event's own.
            input: json!({"path": "src/lib.rs", "tree": "t", "nested": {"b": 2, "a": [1, 2.5, null, true]}}),
            status: ToolStatus::Done,
            output: Some("ok \u{2713}".into()),
            truncated: true,
            diffs: vec![FileDiff { path: "src/lib.rs".into(), created: true, hunks: vec![Hunk { old_start: 1, old_lines: 0, new_start: 1, new_lines: 1, lines: vec!["+x".into()] }] }],
            todos: vec![Todo { text: "do it".into(), status: "pending".into() }],
            task: Some("t".into()),
            exit_code: Some(-1),
            started: 5,
            ended: Some(6),
            server: Some("github".into()),
            tool_short: Some("search".into()),
            skill: Some("s".into()),
            runner: Some("linux/amd64".into()),
        };
        let bare = ToolCall { detail: None, output: None, truncated: false, diffs: Vec::new(), todos: Vec::new(), task: None, exit_code: None, ended: None, server: None, tool_short: None, skill: None, runner: None, ..tool.clone() };
        vec![
            event(1, None, Body::User { text: "hello \u{1F600}".into(), images: 2, client_id: Some("c1".into()), origin: Some("human".into()), attachments: vec!["a.png".into()], reply_to: Some("e0".into()) }),
            event(2, None, Body::User { text: String::new(), images: 0, client_id: None, origin: None, attachments: Vec::new(), reply_to: None }),
            event(3, Some("t1"), Body::Assistant { text: "line".into(), streaming: true }),
            event(4, Some(""), Body::Thinking { text: "hmm".into(), streaming: false, redacted: true }),
            event(5, None, Body::Tool(Box::new(tool))),
            event(6, Some("t9"), Body::Tool(Box::new(bare))),
            event(7, None, Body::Notice { level: Level::Warn, text: "retrying".into() }),
        ]
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an event written out once, straight from itself, is the text its
    /// JSON tree wrote (the copy's before, and what a runner's digest of it is of), its store row has that
    /// text, and what's looked up of it is what the tree said, for every kind of event. It holds while
    /// serde_json's maps keep their keys in the order they come (`preserve_order`, which kiln's build has
    /// through hyper-quic's qlog): a build that loses that fails here.
    #[test]
    fn g9_s5_an_event_is_written_out_once_as_its_tree_wrote_it() {
        for e in every_kind_of_event() {
            let n = e.seq;
            let v = serde_json::to_value(&e).unwrap_or(Value::Null);
            let tree = Entry::written(v.clone(), serde_json::to_string(&v).unwrap_or_default());
            let mut kept: Option<String> = None;
            let once = Entry::of_event(&e, Some(&mut |t: String| kept = Some(t)));
            assert_eq!(once.text(), tree.text(), "event {n}");
            assert_eq!(kept.as_deref(), Some(tree.text()), "event {n}: its row's text");
            assert_eq!(Entry::of_event(&e, None).text(), tree.text(), "event {n}, kept by no one else");
            assert_eq!((once.id(), once.seq(), once.kind(), once.on_main_thread()), (tree.id(), tree.seq(), tree.kind(), tree.on_main_thread()), "event {n}");
            assert!(once.head.file.is_none() && tree.head.file.is_none(), "event {n}: no file's path or tree of its own");
            assert_eq!(crate::wire::sums::event_text(once.text()), crate::wire::sums::event(&v), "event {n}: a runner's digest of it");
            // Its row is the one the event written out again makes.
            assert_eq!(kept.map(|t| crate::convo::rows::event_written("s", &e, t)), crate::convo::rows::event("s", &e), "event {n}: its row");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an update's events are taken while its owner's turn lasts, one at
    /// the least, the rest coming back to take next. Five events by turns that each run out at once take
    /// five turns, and the copy has each once, in order, as one turn would have left it.
    #[test]
    fn g9_s3_an_update_is_taken_in_turns() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            let ingest = Ingest { from: there.clone(), id: "s".into(), u: Update::Events { events: (1..=5).map(said).collect() }, shipped: false, reported: String::new() };
            let h = Arc::clone(&hub);
            let turns = hub
                .harness
                .sessions
                .on("s", move |s| {
                    let (mut turns, mut next) = (0, Some(ingest));
                    while let Some(i) = next.take() {
                        turns += 1;
                        next = h.harness.apply(s, &h, i, &|| true);
                    }
                    turns
                })
                .await;
            anyhow::ensure!(turns == Some(5), "taken in {turns:?} turns");
            let names: Vec<String> = (1..=5).map(|n| format!("m{n}")).collect();
            anyhow::ensure!(copy_of(&hub, "s") == names, "the copy: {:?}", copy_of(&hub, "s"));
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: the rest of an update its owner's turn ran out in goes on before
    /// anything sent after it. A job sent right behind an update of as many events as a copy keeps
    /// (`MAX_EVENTS`, more than a step quantum takes on any machine) finds every one of them in the copy,
    /// however many turns they took.
    #[test]
    fn g9_s3_the_rest_of_an_update_goes_before_anything_newer() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = Location::Machine("box".into());
            let said = |n: u64| Event { id: format!("m{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("line {n}"), streaming: false } };
            hub.harness.ingest(&hub, &there, "s", Update::Open { info: Box::new(Info { id: "s".into(), kind: "claude".into(), epoch: 1, ..Info::default() }), since: None }).await;
            let rev = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default();
            let n = u64::try_from(MAX_EVENTS)?;
            hub.harness.ingest_from(&hub, &there, "s", Update::Events { events: (1..=n).map(said).collect() }, false).await;
            let found = hub.harness.sessions.on("s", |s| s.get("s").map(|m| m.events.len())).await.flatten();
            anyhow::ensure!(found == Some(MAX_EVENTS), "the job behind it found {found:?} of {MAX_EVENTS}");
            let turns = hub.harness.attach("s").map(|a| a.rev).unwrap_or_default().saturating_sub(rev);
            anyhow::ensure!(turns >= 1, "the update's turns: {turns}");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: snapshots in flight hold at most their room: one past it waits its
    /// turn until one in flight is written out; each holds what it was made at, given back as it's written
    /// and the rest as it goes; one larger than the room goes alone into an empty one. The server's room is
    /// its blocking pool's `fs` places of whole snapshots.
    #[test]
    fn g11_s3_snapshots_past_their_room_wait_their_turn() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            static ROOM: std::sync::OnceLock<crate::memory::queue::Room> = std::sync::OnceLock::new();
            let room = ROOM.get_or_init(|| crate::memory::queue::Room::new(&crate::memory::SNAPSHOTS, 1000));
            let mut first = InFlight::take(room, 800).await;
            anyhow::ensure!(room.held() == 800);
            let went = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let next = {
                let went = Arc::clone(&went);
                crate::rt::spawn(async move {
                    let held = InFlight::take(room, 300).await;
                    went.store(true, std::sync::atomic::Ordering::SeqCst);
                    held
                })
            };
            crate::rt::sleep(std::time::Duration::from_millis(50)).await;
            anyhow::ensure!(!went.load(std::sync::atomic::Ordering::SeqCst), "past the room, it waits");
            first.made(900);
            anyhow::ensure!(room.held() == 900, "made, it holds what it was made at");
            first.written(600);
            let next = next.await.map_err(|_| anyhow::anyhow!("the next never went"))?;
            anyhow::ensure!(went.load(std::sync::atomic::Ordering::SeqCst) && room.held() == 600, "its turn once there was room: {}", room.held());
            drop((first, next));
            anyhow::ensure!(room.held() == 0, "the rest given back as each goes");
            let alone = InFlight::take(room, 5000).await;
            anyhow::ensure!(room.held() == 5000, "larger than the room, alone");
            drop(alone);
            anyhow::ensure!(snapshot_bound(26) == 26 * SNAPSHOT_BYTES && SNAPSHOT_BYTES == 5000 * 647, "the server's: its fs places of whole snapshots");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a snapshot's weight, what it takes of the room before it's made, is
    /// its chunks' segments as they're kept (and a quarter of the texts of one not packed) packed, and their
    /// texts as frames, beside its other frames.
    #[test]
    fn g11_s3_a_snapshots_weight_is_its_segments_packed_and_its_texts_as_frames() -> anyhow::Result<()> {
        let mut m = super::super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
        for q in 1..=300u64 {
            m.rev = q;
            m.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": format!("line {q} of what was said")}))));
        }
        let parts = m.view().parts();
        let segments: usize = parts.events.chunks().iter().filter(|c| c.is_full()).filter_map(|c| c.pack().ok().map(|s| s.deflated.len())).sum();
        let texts: usize = parts.events.chunks().iter().map(|c| c.metas().last().map_or(0, |m| m.off + m.len)).sum();
        let s = Snapshot { parts: m.view().parts(), files: None, store: None };
        anyhow::ensure!(s.weight(false) == texts, "as frames, its texts: {} of {texts}", s.weight(false));
        anyhow::ensure!(s.weight(true) >= segments && s.weight(true) < texts, "packed, its segments: {} of {segments}", s.weight(true));
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a snapshot packed as it goes out is, piece after piece, the message
    /// made at once: each piece but the last at least the size asked for, none once it's all out; whatever its
    /// chunks are (packed, spilled, the open one) and its other frames (kept files among them).
    #[test]
    fn g11_s5_a_snapshot_packed_as_it_goes_is_the_one_made_at_once() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut m = super::super::copies::Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::new(kiln_seams::bus::SimBus::default()));
        for q in 1..=500u64 {
            m.rev = q;
            m.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": format!("line {q}: {}", "of what was said ".repeat(usize::try_from(q % 7)?))}))));
        }
        // Some chunks packed, the oldest of them spilled, the newest open.
        let parts = m.view().parts();
        for c in parts.events.chunks().iter().filter(|c| c.is_full()).take(5) {
            c.pack()?;
        }
        let dir = super::super::spill::Dir::open(&tmp.path().join("spill"))?;
        let mut events = parts.events.clone();
        events.compact(None);
        let chunks = events.spillable(super::super::copies::EVENTS_PAGE, super::super::copies::PAGE_BYTES);
        let segments: Vec<Arc<super::super::packed::Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(events.spilled(&chunks, spilled) > 0, "some spilled");
        let snapshot = || Snapshot { parts: Parts { events: events.clone(), ..m.view().parts() }, files: Some(r#"{"t":"files","files":[]}"#.into()), store: None };
        let whole = snapshot().packed("synced")?;
        for size in [1, 1024, 64 << 10, usize::MAX] {
            let mut packing = snapshot().packing("synced");
            let mut pieces = Vec::new();
            while let Some(piece) = packing.next_piece(size)? {
                pieces.push(piece);
            }
            anyhow::ensure!(pieces.iter().rev().skip(1).all(|p| p.len() >= size), "each but the last at least {size}");
            anyhow::ensure!(pieces.concat() == whole, "pieces of {size}: the message made at once");
            anyhow::ensure!(packing.next_piece(size)?.is_none(), "none once it's all out");
        }
        Ok(())
    }
}
