//! Each agent session's copy on the server, owned by one shard (goal 4, plan 01a1a59c2a9c; plan
//! 01a17197 step 9). A session belongs to the shard its id hashes to ([`owner_of`]). That shard's
//! owner task holds the session's copy ([`Mirror`]) by value and is the only one that changes it:
//! runners' updates and people's commands come to it as messages, from a bounded inbox, in the order
//! they were sent. After each change it publishes the copy as an immutable view ([`View`]), swapped
//! in whole; every reader (a viewer's snapshot, a catch-up, a list, a command's checks) loads the
//! newest view without a lock and never waits on the owner. No lock guards a session's copy
//! (`no_lock_guards_session_copies` holds this module, `super::live` and the registry to it).
//!
//! An owner's inbox holds as many messages as the hub's queue of runner events
//! ([`crate::net::EVENTS`]). A sender it has no room for waits, asynchronously (its shard goes on
//! with its other tasks), until the owner takes the next one: nothing is dropped and the order holds.
//! The owner waits on nothing but its inbox, so it always drains it. Behind a full inbox the hub's
//! loop waits, the net thread holds what it can't hand over, QUIC's flow control holds the runner,
//! and the runner holds its states behind the last unanswered one (the newest in its place) and
//! keeps every event.
//!
//! A viewer joins a session's feed without its owner (`super::live::Tap`): it's counted, then reads
//! where the feed ends, then the view. The owner publishes a view, then asks whether anyone watches
//! ([`Mirror::commit`]): one of the two sees the other, so every change is in a viewer's view or in
//! a frame past where it reads from.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use arc_swap::ArcSwap;
use serde_json::{Value, json};

use super::core::{MAX_EVENTS, Update};
use super::live::{Feed, Payload, Tap};
use super::registry::{Entry, Files, Location, Parts, PatchOf, changes_patch_of, merge_changes, null, state_patch_of, write};
use super::session::Info;
use crate::hub::Hub;

/// One of a copy's events: its entry, and the copy's revision when it last changed (a viewer that
/// had an earlier revision is sent it again; docs/wire.md, "Revisions").
#[derive(Clone)]
pub struct Ev {
    pub entry: Arc<Entry>,
    pub at: u64,
}

/// The events a chunk of [`Events`] holds: the square root of the most a copy keeps, so a change
/// once a view shares them copies one chunk and the list of chunks (about two square roots of
/// references), never every event.
const CHUNK: usize = (MAX_EVENTS + MAX_EVENTS / 4).isqrt();

/// A copy's events in order, in chunks of [`CHUNK`] (the last may hold fewer), each shared: a view
/// takes them whole by reference, and the owner's next change copies only what it touches.
#[derive(Clone, Default)]
pub struct Events {
    chunks: Arc<Vec<Arc<Vec<Ev>>>>,
    len: usize,
}

impl Events {
    /// Made from `all`, in order.
    fn of(all: Vec<Ev>) -> Self {
        let len = all.len();
        let mut chunks = Vec::with_capacity(len.div_ceil(CHUNK));
        let mut rest = all.into_iter().peekable();
        while rest.peek().is_some() {
            chunks.push(Arc::new(rest.by_ref().take(CHUNK).collect()));
        }
        Self { chunks: Arc::new(chunks), len }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> Option<&Ev> {
        self.chunks.get(i / CHUNK)?.get(i % CHUNK)
    }

    pub fn last(&self) -> Option<&Ev> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Ev> + '_ {
        self.chunks.iter().flat_map(|c| c.iter())
    }

    /// Event `i`, to change: its chunk (and the list of chunks) copied first when a view shares it.
    pub(super) fn get_mut(&mut self, i: usize) -> Option<&mut Ev> {
        if i >= self.len {
            return None;
        }
        let chunk = Arc::make_mut(&mut self.chunks).get_mut(i / CHUNK)?;
        Arc::make_mut(chunk).get_mut(i % CHUNK)
    }

    pub(super) fn push(&mut self, ev: Ev) {
        let chunks = Arc::make_mut(&mut self.chunks);
        match chunks.last_mut() {
            Some(last) if last.len() < CHUNK => Arc::make_mut(last).push(ev),
            _ => {
                let mut chunk = Vec::with_capacity(CHUNK);
                chunk.push(ev);
                chunks.push(Arc::new(chunk));
            }
        }
        self.len += 1;
    }

    /// Each event, as owned references (what a change in the middle remakes the chunks from).
    fn to_vec(&self) -> Vec<Ev> {
        self.iter().cloned().collect()
    }

    /// `ev` in at `i` (rare: an event a runner that started again sends under an older number).
    pub(super) fn insert(&mut self, i: usize, ev: Ev) {
        let mut all = self.to_vec();
        all.insert(i.min(all.len()), ev);
        *self = Self::of(all);
    }

    /// Lets go of the first `n`, returning them (a long session's oldest, every thousand events or so).
    pub(super) fn drain_front(&mut self, n: usize) -> Vec<Ev> {
        let mut all = self.to_vec();
        let gone = all.drain(..n.min(all.len())).collect();
        *self = Self::of(all);
        gone
    }

    pub(super) fn retain(&mut self, keep: impl FnMut(&Ev) -> bool) {
        let mut all = self.to_vec();
        all.retain(keep);
        *self = Self::of(all);
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The first index whose event `pred` is false for, the events being those it's true for first.
    pub(super) fn partition_point(&self, pred: impl Fn(&Ev) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get(mid).is_some_and(&pred) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// An event's number in its session's timeline.
fn seq(v: &Value) -> Option<u64> {
    v.get("seq").and_then(Value::as_u64)
}

/// A session's copy as its owner last published it, whole and immutable: what every reader takes.
pub struct View {
    pub info: Arc<Info>,
    pub location: Arc<Location>,
    pub meta: Arc<Value>,
    pub state: Arc<Value>,
    pub changes: Option<Files>,
    pub events: Events,
    pub rev: u64,
    pub reset_at: u64,
    /// Its meta's and state's last frames' sizes: the room a snapshot's take.
    pub meta_bytes: usize,
    pub state_bytes: usize,
    /// The copy is gone (its session ended or went away): its last view.
    pub closed: bool,
}

impl View {
    /// What a snapshot is made of: all of it, by reference (its info aside, a few hundred bytes).
    /// Its frames are written off the runtime's threads (`super::registry::Snapshot`).
    pub fn parts(&self) -> Parts {
        Parts {
            info: json!(*self.info),
            meta: Arc::clone(&self.meta),
            state: Arc::clone(&self.state),
            changes: self.changes.clone(),
            events: self.events.clone(),
            since: None,
            meta_bytes: self.meta_bytes,
            state_bytes: self.state_bytes,
            rev: self.rev,
        }
    }

    /// What brings a viewer that has this copy through revision `after` up to now: its info and
    /// revision, the newest meta, state and changes, then only the events changed since. None when it
    /// can't be resumed: it started over since (an Open, a Reset), or `after` is past what it has
    /// (another process's number).
    pub fn parts_since(&self, after: u64) -> Option<Parts> {
        if after < self.reset_at || after > self.rev {
            return None;
        }
        Some(Parts { since: Some(after), ..self.parts() })
    }

    /// What this copy has of its events, said in digests (`crate::wire::sums`): each event's
    /// digest is of its text, written out once, however many ask.
    pub fn kept(&self, session: &str) -> crate::proto::KeptSession {
        let mut events: Vec<(u64, u64)> = self
            .events
            .iter()
            .filter_map(|e| {
                let seq = seq(&e.entry)?;
                e.entry.get("id").and_then(Value::as_str)?;
                Some((seq, e.entry.sum()))
            })
            .collect();
        events.sort_unstable_by_key(|(seq, _)| *seq);
        crate::wire::sums::kept(session.to_owned(), self.info.epoch, &events)
    }
}

/// What readers find of a session: its newest view, and its feed's tap (a viewer joins there).
pub struct Slot {
    id: Arc<str>,
    view: ArcSwap<View>,
    tap: Tap,
}

impl Slot {
    /// The newest view, held (a snapshot, written out over time).
    pub fn view(&self) -> Arc<View> {
        self.view.load_full()
    }

    /// Whether its copy is gone: its last view says so.
    pub fn closed(&self) -> bool {
        self.view.load().closed
    }

    pub fn tap(&self) -> &Tap {
        &self.tap
    }
}

/// A shard's sessions as its owner published them, by id: swapped in whole as one comes or goes.
type Board = HashMap<Arc<str>, Arc<Slot>>;

/// A frame of `text` (one of the session socket's, or the feed's own marker) as a payload, said at
/// revision `rev`.
pub(super) fn payload_of(text: &str, rev: u64) -> Payload {
    let mut p = Payload::new(rev, text.len());
    p.text().extend_from_slice(text.as_bytes());
    p.done();
    p
}

/// A session's copy, as the shard that owns it holds it (by value: nothing else changes it).
pub struct Mirror {
    pub(super) info: Arc<Info>,
    /// The last name of its own its machine reported for it (empty: none yet).
    pub(super) reported: String,
    pub(super) location: Arc<Location>,
    /// Its meta and state, shared with its views (a state patch's baseline is the state as it was,
    /// not a copy of it).
    pub(super) meta: Arc<Value>,
    pub(super) state: Arc<Value>,
    /// The state as its viewers last had it sent (what a state patch is made against).
    pub(super) sent: Arc<Value>,
    /// The bytes its last state frame took whole, and as a patch, and its last meta frame: the
    /// room the next of each is written into (they change little from one to the next).
    pub(super) state_bytes: usize,
    pub(super) patch_bytes: usize,
    pub(super) meta_bytes: usize,
    /// Its changeset (none yet: `None`), as a list taken in by [`merge_changes`]: its viewers take
    /// a patch of it the same way, so theirs stays in this one's order.
    pub(super) changes: Option<Files>,
    /// The changeset as its viewers last had it sent (what a changeset's patch is made against).
    pub(super) changes_sent: Option<Files>,
    /// Shared with its views, a chunk at a time ([`Events`]).
    pub(super) events: Events,
    pub(super) index: HashMap<String, usize>,
    /// Its live frames, on a topic of its own on the bus; each carries the copy's revision.
    pub(super) feed: Feed,
    /// Every update this copy has taken, counted: its revision, what a viewer resumes from
    /// (docs/wire.md, "Revisions"). Under `super::registry::born`: another server process's numbers
    /// aren't these.
    pub(super) rev: u64,
    /// The revision it last started over at (an Open, a Reset): a viewer that had it from before
    /// that starts over too.
    pub(super) reset_at: u64,
    /// The Open being taken starts the copy over (a whole snapshot follows it).
    pub(super) started_over_pending: bool,
    /// When this server opened it, until its machine announces it: a report from the machine
    /// that left it out was made before its open got there, and doesn't end it.
    pub(super) opened_at: Option<std::time::Instant>,
    /// What its readers find of it.
    slot: Arc<Slot>,
}

impl Mirror {
    pub fn new(info: Info, location: Location, bus: Arc<dyn kiln_seams::Bus>) -> Self {
        let feed = Feed::open(bus, &info.id);
        let (info, location) = (Arc::new(info), Arc::new(location));
        let view = View {
            info: Arc::clone(&info),
            location: Arc::clone(&location),
            meta: null(),
            state: null(),
            changes: None,
            events: Events::default(),
            rev: 0,
            reset_at: 0,
            meta_bytes: 0,
            state_bytes: 0,
            closed: false,
        };
        let slot = Arc::new(Slot { id: Arc::from(info.id.as_str()), view: ArcSwap::from_pointee(view), tap: feed.tap() });
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
            events: Events::default(),
            index: HashMap::new(),
            feed,
            rev: 0,
            reset_at: 0,
            started_over_pending: false,
            opened_at: None,
            slot,
        }
    }

    /// The copy as it is now, for its readers.
    pub fn view(&self) -> View {
        View {
            info: Arc::clone(&self.info),
            location: Arc::clone(&self.location),
            meta: Arc::clone(&self.meta),
            state: Arc::clone(&self.state),
            changes: self.changes.clone(),
            events: self.events.clone(),
            rev: self.rev,
            reset_at: self.reset_at,
            meta_bytes: self.meta_bytes,
            state_bytes: self.state_bytes,
            closed: false,
        }
    }

    /// Its readers take the copy as it is now: its view swapped in whole.
    pub fn publish(&self) {
        self.slot.view.store(Arc::new(self.view()));
    }

    /// After a change: the copy goes out to its readers, then, if anyone watches, its viewers are
    /// sent `frame` of it (made only then). The view goes first and the feed is asked after it, and
    /// a viewer is counted before it takes a view: one sees the other, so no viewer misses a change
    /// (`super::live::Feed::watched`). None watching, its patch baselines go (the next viewer starts
    /// from a snapshot), and so does what the feed's topic kept.
    pub fn commit(&mut self, frame: impl FnOnce(&mut Self) -> Option<Payload>) {
        self.publish();
        match self.feed.watched() {
            Some(generation) => {
                if let Some(p) = frame(self) {
                    self.feed.send_payload(generation, p);
                }
            }
            None => {
                self.sent = null();
                self.changes_sent = None;
            }
        }
    }

    /// The state as a frame for its viewers: whole for those that take it so (or when there's
    /// nothing to patch), and as what changed in it since the last one for those that take that;
    /// a state none takes whole goes as its patch alone. Made only while someone watches
    /// ([`Mirror::commit`]).
    pub fn state_frame(&mut self) -> Option<Payload> {
        let rev = Some(self.rev);
        let (mut p, whole, patched) = {
            let patch = state_patch_of(&self.sent, &self.state);
            let whole = patch.is_none() || self.feed.wants_whole(PatchOf::State);
            let room = if whole { self.state_bytes } else { 0 } + if patch.is_some() { self.patch_bytes } else { 0 };
            let mut p = Payload::new(self.rev, room);
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
    /// Made only while someone watches ([`Mirror::commit`]).
    pub fn changes_frame(&mut self) -> Option<Payload> {
        let now = self.changes.clone()?;
        let rev = Some(self.rev);
        let mut p = {
            let patch = self.changes_sent.as_deref().and_then(|was| changes_patch_of(was, &now));
            let whole = patch.is_none() || self.feed.wants_whole(PatchOf::Changes);
            // Its files as they're written out already, in their envelopes (a patch's gone keys
            // past that: a few short paths).
            let room = if whole { write::envelope("changes", Some("files"), rev) + write::texts_len(now.iter().map(|f| &**f)) } else { 0 };
            let room = room + patch.as_ref().map_or(0, |(set, _)| write::envelope("changes_patch", None, rev) + r#","set":"#.len() + write::texts_len(set.iter().copied()));
            let mut p = Payload::new(self.rev, room);
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
    pub fn frame_of(&mut self, u: &Update, entries: &[Arc<Entry>]) -> Payload {
        let rev = Some(self.rev);
        let mut p = match u {
            Update::Events { .. } => {
                let mut p = Payload::new(self.rev, write::envelope("events", Some("events"), rev) + write::texts_len(entries.iter().map(|e| &**e)));
                let _ = write::events(p.text(), entries.iter().map(|e| &**e), rev);
                p
            }
            Update::Stream { id, text } => {
                // Its id and words as JSON strings, unescaped (escaping makes them longer).
                let strings = r#""""#.len() + id.len() + r#","text":"#.len() + r#""""#.len() + text.len();
                let mut p = Payload::new(self.rev, write::envelope("stream", Some("id"), rev) + strings);
                let _ = write::stream(p.text(), id, text, rev);
                p
            }
            Update::Meta { .. } => {
                let mut p = Payload::new(self.rev, self.meta_bytes);
                let start = p.text().len();
                let _ = write::value(p.text(), "meta", "meta", &self.meta, rev);
                self.meta_bytes = p.text().len() - start;
                p
            }
            other => {
                let text = crate::wire::ui::SessionDown { frame: crate::wire::ui::SessionFrame::of(other), rev }.text();
                let mut p = Payload::new(self.rev, text.len());
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
    pub fn take_changes(&mut self, set: Vec<Value>, gone: Vec<Value>, whole: bool) {
        let mut files: Vec<Arc<Entry>> = match self.changes.take() {
            // Its own, or (shared with its views) a list of the same files.
            Some(all) => Arc::try_unwrap(all).unwrap_or_else(|shared| (*shared).clone()),
            None => Vec::new(),
        };
        if whole {
            let kept: std::collections::HashSet<super::registry::FileRef<'_>> = set.iter().map(super::registry::file_ref).collect();
            files.retain(|f| kept.contains(&super::registry::file_ref(f)));
        }
        merge_changes(&mut files, set, if whole { &[] } else { gone.as_slice() });
        self.changes = Some(Arc::new(files));
    }

    pub fn reindex(&mut self) {
        self.index = self.events.iter().enumerate().filter_map(|(i, e)| Some((e.entry.get("id")?.as_str()?.to_owned(), i))).collect();
    }

    /// Lets go of every event.
    pub fn clear_events(&mut self) {
        self.events.clear();
        self.index.clear();
    }

    /// Takes in an event, by its id, at the copy's revision. One it hasn't had numbered before its
    /// newest (a runner that started again sends it again: the copy's differed,
    /// `crate::wire::sums`) goes in its place by number, instead of one the copy has under that
    /// number: true when that one had another id (the timeline went another way), which its viewers
    /// can't see from the event alone.
    pub fn upsert(&mut self, e: Arc<Entry>) -> bool {
        let Some(id) = e.get("id").and_then(Value::as_str).map(str::to_owned) else { return false };
        let ev = Ev { entry: e, at: self.rev };
        if let Some(slot) = self.index.get(&id).copied().and_then(|i| self.events.get_mut(i)) {
            *slot = ev;
            return false;
        }
        if let (Some(q), Some(top)) = (seq(&ev.entry), self.events.last().and_then(|last| seq(&last.entry)))
            && q <= top
        {
            let at = self.events.partition_point(|x| seq(&x.entry).is_some_and(|s| s < q));
            let displaced = match self.events.get_mut(at) {
                Some(slot) if seq(&slot.entry) == Some(q) => {
                    *slot = ev;
                    true
                }
                _ => {
                    self.events.insert(at, ev);
                    false
                }
            };
            self.reindex();
            return displaced;
        }
        self.index.insert(id, self.events.len());
        self.events.push(ev);
        if self.events.len() > MAX_EVENTS + MAX_EVENTS / 4 {
            // From where a run of the digests begins, as its machine's timeline lets go of its
            // oldest (`crate::wire::sums::run_after`).
            let over = self.events.len() - MAX_EVENTS;
            let drop = match self.events.get(over).and_then(|e| seq(&e.entry)) {
                Some(q) => {
                    let from = crate::wire::sums::run_after(q);
                    self.events.partition_point(|x| seq(&x.entry).is_some_and(|s| s < from))
                }
                None => over,
            };
            // Those let go leave the index, and the rest move down by as many: the index isn't
            // made again (a long session lets go of its oldest every thousand events or so).
            for gone in self.events.drain_front(drop) {
                if let Some(id) = gone.entry.get("id").and_then(Value::as_str) {
                    self.index.remove(id);
                }
            }
            for at in self.index.values_mut() {
                *at = at.saturating_sub(drop);
            }
        }
        false
    }

    /// Text streamed into event `id`, at the copy's revision. An event a view still holds is copied
    /// first (that one alone), so the view keeps what it took.
    pub fn append(&mut self, id: &str, text: &str) {
        let rev = self.rev;
        if let Some(ev) = self.index.get(id).copied().and_then(|i| self.events.get_mut(i))
            && let Some(Value::String(s)) = Arc::make_mut(&mut ev.entry).value_mut().get_mut("text")
        {
            s.push_str(text);
            ev.at = rev;
        }
    }

    /// The copy starts over (an Open, a Reset): a viewer from before it can't resume.
    pub fn started_over(&mut self) {
        self.reset_at = self.rev;
    }

    /// Tests: what brings a viewer that has this copy through revision `after` up to now, made at
    /// once ([`View::parts_since`]).
    #[cfg(test)]
    pub fn resume(&self, after: u64, files: Option<String>) -> Option<Vec<String>> {
        self.view().parts_since(after).map(|parts| super::registry::Snapshot { parts, files }.frames())
    }

}

/// What a shard's owner holds: its sessions' copies, by id, and every shard's board (its own to
/// publish, the others' to read).
pub struct Shard {
    me: usize,
    mirrors: HashMap<String, Mirror>,
    boards: Arc<[ArcSwap<Board>]>,
    bus: Arc<dyn kiln_seams::Bus>,
    /// A session came or went since its board was last published.
    board_changed: bool,
}

impl Shard {
    pub fn bus(&self) -> &Arc<dyn kiln_seams::Bus> {
        &self.bus
    }

    pub fn get(&self, id: &str) -> Option<&Mirror> {
        self.mirrors.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Mirror> {
        self.mirrors.get_mut(id)
    }

    pub fn mirrors(&self) -> impl Iterator<Item = (&String, &Mirror)> {
        self.mirrors.iter()
    }

    pub fn mirrors_mut(&mut self) -> impl Iterator<Item = (&String, &mut Mirror)> {
        self.mirrors.iter_mut()
    }

    /// `m` as session `id`'s copy, unless it has one already (which stays): whether it went in.
    pub fn insert(&mut self, id: String, m: Mirror) -> bool {
        match self.mirrors.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                m.publish();
                slot.insert(m);
                self.board_changed = true;
                true
            }
        }
    }

    /// Session `id`'s copy, made by `make` when it has none.
    pub fn entry(&mut self, id: &str, make: impl FnOnce() -> Mirror) -> &mut Mirror {
        match self.mirrors.entry(id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                self.board_changed = true;
                v.insert(make())
            }
        }
    }

    /// Lets go of session `id`'s copy: its last view says it's gone (before its viewers are told,
    /// so one that joins after finds it gone), its viewers' feed ends ([`super::live::closed`]), and it
    /// leaves the board. Its feed's topic goes with its last viewer.
    pub fn remove(&mut self, id: &str) -> Option<Mirror> {
        let mut m = self.mirrors.remove(id)?;
        self.board_changed = true;
        m.slot.view.store(Arc::new(View { closed: true, ..m.view() }));
        if let Some(generation) = m.feed.watched() {
            m.feed.send_payload(generation, payload_of(super::live::closed(), 0));
        }
        Some(m)
    }

    /// Every session's newest view, on every shard (what spans sessions: an account's others), with
    /// its id.
    pub fn each_view(&self, mut f: impl FnMut(&str, &View)) {
        for board in self.boards.iter() {
            for (id, slot) in board.load().iter() {
                let view = slot.view.load();
                if !view.closed {
                    f(id, &view);
                }
            }
        }
    }

    /// Readers find the sessions as they are now: this shard's board, swapped in whole, when one
    /// came or went.
    fn publish_board(&mut self) {
        if !std::mem::take(&mut self.board_changed) {
            return;
        }
        let board: Board = self.mirrors.values().map(|m| (Arc::clone(&m.slot.id), Arc::clone(&m.slot))).collect();
        if let Some(b) = self.boards.get(self.me) {
            b.store(Arc::new(board));
        }
    }

    /// Takes one message, a panic in it caught (its shard goes on with the rest).
    fn take(&mut self, msg: Msg, hub: Option<&Arc<Hub>>) {
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
            Msg::Update(i) => {
                if let Some(h) = hub {
                    h.harness.apply(self, h, *i);
                }
            }
            Msg::Run(job) => job(self),
        }));
        if let Err(p) = done {
            crate::metrics::global().panics_caught.inc();
            tracing::error!(shard = self.me, panic = %crate::sup::panic_message(p.as_ref()), "a change to an agent session's copy panicked: its shard goes on");
        }
        self.publish_board();
    }
}

/// An update of a session from the machine it runs on, for its owner (`Registry::ingest_from`).
pub struct Ingest {
    pub from: Location,
    pub id: String,
    pub u: Update,
    pub shipped: bool,
    /// The name of its own its machine gave it in an Open (empty: none).
    pub reported: String,
}

type Job = Box<dyn FnOnce(&mut Shard) + Send>;

/// What an owner takes from its inbox.
pub enum Msg {
    Update(Box<Ingest>),
    /// Work on its copies (people's commands, a machine lost, a session opened or closed…).
    Run(Job),
}

/// The shard that owns session `id` among `shards`: FNV-1a over its id, so every process with as
/// many shards puts it in the same place.
pub fn owner_of(id: &str, shards: usize) -> usize {
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    usize::try_from(hash % u64::try_from(shards.max(1)).unwrap_or(1)).unwrap_or(0)
}

/// Every session's copy, each owned by one shard: where messages to them go, and what readers read.
pub struct Copies {
    inboxes: Vec<async_channel::Sender<Msg>>,
    boards: Arc<[ArcSwap<Board>]>,
    /// The hub the owners act through (what a change tells the rest of kiln), once made.
    hub: Arc<OnceLock<Weak<Hub>>>,
}

impl Copies {
    /// An owner on each of the runtime's shards, its sessions' feeds on `bus`.
    pub fn start(bus: Arc<dyn kiln_seams::Bus>) -> Self {
        Self::start_with(bus, crate::rt::shard_count().max(1), crate::net::EVENTS)
    }

    /// `shards` owners (one a shard), each inbox holding `inbox` messages.
    pub fn start_with(bus: Arc<dyn kiln_seams::Bus>, shards: usize, inbox: usize) -> Self {
        let shards = shards.max(1);
        let boards: Arc<[ArcSwap<Board>]> = (0..shards).map(|_| ArcSwap::from_pointee(Board::new())).collect::<Vec<_>>().into();
        let hub = Arc::new(OnceLock::new());
        let mut inboxes = Vec::with_capacity(shards);
        for me in 0..shards {
            let (tx, rx) = async_channel::bounded(inbox.max(1));
            let shard = Shard { me, mirrors: HashMap::new(), boards: Arc::clone(&boards), bus: Arc::clone(&bus), board_changed: false };
            crate::rt::spawn_owner(me, own(shard, rx, Arc::clone(&hub)));
            inboxes.push(tx);
        }
        Self { inboxes, boards, hub }
    }

    /// The hub the owners act through, once it's made.
    pub fn bind(&self, hub: &Arc<Hub>) {
        let _ = self.hub.set(Arc::downgrade(hub));
    }

    /// The shard that owns session `id`.
    pub fn owner_of(&self, id: &str) -> usize {
        owner_of(id, self.inboxes.len())
    }

    /// What readers find of session `id`, while it's there.
    pub fn slot(&self, id: &str) -> Option<Arc<Slot>> {
        self.boards.get(self.owner_of(id))?.load().get(id).cloned()
    }

    /// Session `id`'s newest view, held.
    pub fn view(&self, id: &str) -> Option<Arc<View>> {
        self.slot(id).map(|s| s.view()).filter(|v| !v.closed)
    }

    /// `f` of session `id`'s newest view, read where it is (nothing held after).
    pub fn with<R>(&self, id: &str, f: impl FnOnce(&View) -> R) -> Option<R> {
        let board = self.boards.get(self.owner_of(id))?.load();
        let view = board.get(id)?.view.load();
        (!view.closed).then(|| f(&view))
    }

    /// `f` of every session's newest view, with its id.
    pub fn each(&self, mut f: impl FnMut(&str, &View)) {
        for board in self.boards.iter() {
            for (id, slot) in board.load().iter() {
                let view = slot.view.load();
                if !view.closed {
                    f(id, &view);
                }
            }
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.with(id, |_| ()).is_some()
    }

    /// Every session's id.
    pub fn ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.each(|id, _| out.push(id.to_owned()));
        out
    }

    /// Hands `msg` to session `id`'s owner, waiting while its inbox is full: false when there's no
    /// owner any more (the registry is going).
    pub async fn send(&self, id: &str, msg: Msg) -> bool {
        match self.inboxes.get(self.owner_of(id)) {
            Some(inbox) => inbox.send(msg).await.is_ok(),
            None => false,
        }
    }

    /// `f` run by shard `shard`'s owner, after what was sent it before: its answer (none when there's
    /// no owner any more). What `f` adds to the board or takes off it is published before it answers.
    pub async fn on_shard<R: Send + 'static>(&self, shard: usize, f: impl FnOnce(&mut Shard) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let job: Job = Box::new(move |s| {
            let r = f(s);
            s.publish_board();
            let _ = tx.send(r);
        });
        self.inboxes.get(shard)?.send(Msg::Run(job)).await.ok()?;
        rx.await.ok()
    }

    /// `f` run by session `id`'s owner ([`Copies::on_shard`]).
    pub async fn on<R: Send + 'static>(&self, id: &str, f: impl FnOnce(&mut Shard) -> R + Send + 'static) -> Option<R> {
        self.on_shard(self.owner_of(id), f).await
    }

    /// `f` run by every owner, each after what was sent it before: their answers.
    pub async fn on_each<R: Send + 'static>(&self, f: impl Fn(&mut Shard) -> R + Clone + Send + 'static) -> Vec<R> {
        let mut answers = Vec::with_capacity(self.inboxes.len());
        for inbox in &self.inboxes {
            let (tx, rx) = futures::channel::oneshot::channel();
            let f = f.clone();
            let job: Job = Box::new(move |s| {
                let r = f(s);
                s.publish_board();
                let _ = tx.send(r);
            });
            if inbox.send(Msg::Run(job)).await.is_ok() {
                answers.push(rx);
            }
        }
        let mut out = Vec::with_capacity(answers.len());
        for rx in answers {
            if let Ok(r) = rx.await {
                out.push(r);
            }
        }
        out
    }

    /// Back once every owner has applied what was sent it before this.
    pub async fn settled(&self) {
        self.on_each(|_| ()).await;
    }

    /// Back once session `id`'s owner has applied what was sent it before this.
    pub async fn applied(&self, id: &str) {
        self.on(id, |_| ()).await;
    }
}

/// A shard's owner: its messages, in the order they came, as fast as they come, in bounded turns of
/// its shard (the HTTP listener may share it). It waits on nothing but its inbox, and ends with it.
async fn own(mut shard: Shard, rx: async_channel::Receiver<Msg>, hub: Arc<OnceLock<Weak<Hub>>>) {
    let mut turns = crate::rt::Turns::new();
    while let Ok(msg) = rx.recv().await {
        // The hub, once a turn: what each change tells the rest of kiln goes through it.
        let h = hub.get().and_then(Weak::upgrade);
        shard.take(msg, h.as_ref());
        // What's waiting already goes in the same turn.
        while !turns.over() {
            let Ok(msg) = rx.try_recv() else { break };
            shard.take(msg, h.as_ref());
        }
        drop(h);
        turns.after().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a source file's code says outside its tests, its comments left out. Lines may end in
    /// CRLF, as a Windows checkout writes them (git's autocrlf): read as LF, or the tests' own
    /// words would be read as code.
    fn code_of(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        let code = src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        code.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Whether `word` is in `text` as a word of its own (not part of a longer name).
    fn has_word(text: &str, word: &str) -> bool {
        let named = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(word).any(|(at, _)| {
            let before = text.get(..at).and_then(|t| t.chars().next_back());
            let after = text.get(at + word.len()..).and_then(|t| t.chars().next());
            !before.is_some_and(named) && !after.is_some_and(named)
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 4 (plan 01a17197 step 9), run by kiln's CI with every test:
    /// no lock guards a session's copy. The copies, their views and owners (this module) and the
    /// feeds viewers join without the owners (`super::live`) take no lock of any kind; the registry
    /// holds no copy, view, board or owner behind one.
    #[test]
    fn no_lock_guards_session_copies() {
        const LOCKS: [&str; 6] = ["Mutex", "RwLock", "Condvar", "parking_lot", "async_lock", "ReentrantLock"];
        for (file, src) in [("harness/copies.rs", include_str!("copies.rs")), ("harness/live.rs", include_str!("live.rs"))] {
            let code = code_of(src);
            for lock in LOCKS {
                assert!(!has_word(&code, lock), "{file} takes a lock ({lock}): a session's copy changes only by messages to its owner, and is read from its views");
            }
        }
        const COPIES: [&str; 8] = ["Mirror", "View", "Copies", "Shard", "Slot", "Board", "Events", "Feed"];
        for line in code_of(include_str!("registry.rs")).lines() {
            if LOCKS.iter().any(|l| has_word(line, l)) {
                for copy in COPIES {
                    assert!(!has_word(line, copy), "harness/registry.rs holds a session's copy behind a lock: {}", line.trim());
                }
            }
        }
    }

    /// The check finds a lock where one is, and none where there isn't.
    #[test]
    fn the_lock_check_finds_a_lock() {
        assert!(has_word("mirrors: Mutex<HashMap<String, Mirror>>,", "Mutex") && has_word("mirrors: Mutex<HashMap<String, Mirror>>,", "Mirror"));
        assert!(has_word("x: parking_lot::RwLock<View>", "RwLock"));
        assert!(!has_word("struct Mutexes;", "Mutex") && !has_word("MirrorView", "Mirror"));
        assert_eq!(code_of("a // Mutex\nb\n#[cfg(test)]\nmod tests {\n Mutex"), "a \nb");
        assert_eq!(code_of("a // Mutex\r\nb\r\n#[cfg(test)]\r\nmod tests {\r\n Mutex"), "a \nb", "a Windows checkout's CRLF");
    }

    fn entry(id: &str, seq: u64) -> Arc<Entry> {
        Arc::new(Entry::new(json!({"id": id, "seq": seq, "kind": "assistant", "text": format!("line {seq}")})))
    }

    fn sim_bus() -> Arc<dyn kiln_seams::Bus> {
        Arc::new(kiln_seams::bus::SimBus::default())
    }

    /// The chunked list behaves as the flat one it stands for: pushes, changes, a change in the
    /// middle, the oldest let go, and what a view took before a change stays as it was.
    #[test]
    fn events_in_chunks_are_the_list_they_stand_for() {
        let mut events = Events::default();
        let mut flat: Vec<u64> = Vec::new();
        let n = u64::try_from(CHUNK * 3 + 5).unwrap_or(0);
        for s in 1..=n {
            events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
            flat.push(s);
        }
        let seqs = |e: &Events| e.iter().filter_map(|e| seq(&e.entry)).collect::<Vec<_>>();
        assert_eq!(seqs(&events), flat);
        assert_eq!(events.len(), flat.len());
        // A view taken now keeps what it took, whatever changes after.
        let view = events.clone();
        if let Some(e) = events.get_mut(CHUNK + 1) {
            e.at = 0;
        }
        assert_eq!(view.get(CHUNK + 1).map(|e| e.at), Some(u64::try_from(CHUNK + 2).unwrap_or(0)));
        assert_eq!(events.get(CHUNK + 1).map(|e| e.at), Some(0));
        // In the middle, by number.
        let at = events.partition_point(|e| seq(&e.entry).is_some_and(|s| s < 10));
        events.insert(at, Ev { entry: entry("between", 10), at: 0 });
        flat.insert(at, 10);
        assert_eq!(seqs(&events), flat);
        let gone = events.drain_front(CHUNK + 3);
        assert_eq!(gone.len(), CHUNK + 3);
        flat.drain(..CHUNK + 3);
        assert_eq!(seqs(&events), flat);
        events.retain(|e| seq(&e.entry).is_some_and(|s| s % 2 == 0));
        flat.retain(|s| s % 2 == 0);
        assert_eq!(seqs(&events), flat);
        assert_eq!(events.last().and_then(|e| seq(&e.entry)), flat.last().copied());
        assert_eq!(view.len(), usize::try_from(n).unwrap_or(0), "the view's list is its own");
        events.clear();
        assert!(events.is_empty() && events.get(0).is_none());
    }

    /// A change copies the chunk it touches and the list of chunks, never every event: the other
    /// chunks stay shared with the view taken before it.
    #[test]
    fn a_change_after_a_view_copies_one_chunk() {
        let mut events = Events::default();
        for s in 1..=u64::try_from(CHUNK * 4).unwrap_or(0) {
            events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        }
        let view = events.clone();
        if let Some(e) = events.get_mut(0) {
            e.at = 0;
        }
        let shared = events.chunks.iter().zip(view.chunks.iter()).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!(shared, 3, "three of four chunks still shared with the view");
    }

    /// Each order of `owner` steps of the owner's and `viewer` steps of a viewer's, each keeping its
    /// own steps in order: true is the owner's next step, false the viewer's.
    fn interleavings(owner: u32, viewer: u32) -> Vec<Vec<bool>> {
        let n = owner + viewer;
        (0u32..1 << n).filter(|bits| bits.count_ones() == owner).map(|bits| (0..n).map(|i| bits & (1 << i) != 0).collect()).collect()
    }

    /// Goal 4, plan 01a1a59c2a9c step 3: the handshake between an owner's change and a viewer
    /// joining without it, in every order the two can take. The owner publishes its view (P), asks
    /// whether anyone watches (W) and, if so, sends the change's frame (F); the viewer is counted
    /// (J), takes where its feed ends (O), then loads the view (L). In all 20 orders the viewer ends
    /// with the change, in its view or in a frame past where it reads. Asked before the view is out
    /// instead (W, P, F), some order loses it: the check has teeth.
    #[test]
    fn every_order_of_a_change_and_a_join_leaves_the_viewer_the_change() -> anyhow::Result<()> {
        let orders = interleavings(3, 3);
        assert_eq!(orders.len(), 20);
        for asks_first in [false, true] {
            let mut lost = 0;
            for order in &orders {
                let bus = sim_bus();
                let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, Arc::clone(&bus));
                // A change nobody watched, out; then the next, taken in but not yet out.
                m.rev = 1;
                m.upsert(entry("e1", 1));
                m.commit(|_| None);
                m.rev = 2;
                m.upsert(entry("e2", 2));
                let (tap, slot) = (m.slot.tap().clone(), Arc::clone(&m.slot));
                let (mut viewer, mut at, mut view, mut watched) = (None, None, None, None);
                let (mut owner_did, mut viewer_did) = (0, 0);
                for owner in order {
                    if *owner {
                        match (asks_first, owner_did) {
                            (false, 0) | (true, 1) => m.publish(),
                            (false, 1) | (true, 0) => watched = Some(m.feed.watched()),
                            _ => {
                                if let Some(Some(generation)) = watched {
                                    m.feed.send_payload(generation, payload_of(r#"{"t":"events","events":[],"rev":2}"#, 2));
                                }
                            }
                        }
                        owner_did += 1;
                    } else {
                        match viewer_did {
                            0 => viewer = Some(tap.join(super::super::registry::Patches::default())),
                            1 => at = viewer.as_ref().and_then(|v| v.position()),
                            _ => view = Some(slot.view()),
                        }
                        viewer_did += 1;
                    }
                }
                let viewer = viewer.ok_or_else(|| anyhow::anyhow!("no viewer"))?;
                let (at, view) = (at.ok_or_else(|| anyhow::anyhow!("no feed"))?, view.ok_or_else(|| anyhow::anyhow!("no view"))?);
                let read = futures::executor::block_on(bus.read(viewer.topic(), at, 16))?;
                let framed = read.messages.iter().filter_map(|m| crate::harness::live::decode(&m.payload)).any(|f| f.rev == 2);
                if view.rev < 2 && !framed {
                    lost += 1;
                }
            }
            if asks_first {
                assert!(lost > 0, "asked before its view is out, some order loses the change");
            } else {
                assert_eq!(lost, 0, "a viewer lost a change in {lost} of 20 orders");
            }
        }
        Ok(())
    }

    /// Every id hashes to a shard among those there are, the same one each time; ids spread over them.
    #[test]
    fn a_session_has_one_owner_chosen_from_its_id() {
        for shards in [1, 2, 3, 4, 7] {
            let mut each = vec![0usize; shards];
            for _ in 0..4000 {
                let id = crate::util::new_id();
                let k = owner_of(&id, shards);
                assert!(k < shards);
                assert_eq!(k, owner_of(&id, shards));
                if let Some(c) = each.get_mut(k) {
                    *c += 1;
                }
            }
            assert!(each.iter().all(|c| *c * shards * 2 > 4000), "{shards} shards: {each:?}");
        }
        assert_eq!(owner_of("01a1a59c2a9c715ab5520072fcd661a4", 4), owner_of("01a1a59c2a9c715ab5520072fcd661a4", 4));
        assert_eq!(owner_of("anything", 0), 0);
    }

    /// Goal 4, plan 01a1a59c2a9c step 3: readers load the view an owner publishes without waiting
    /// on it. With the owner stuck mid-change (its shard's thread held), a reader on another thread
    /// gets the view from before at once; once the owner goes on, its readers take the new one.
    #[test]
    fn a_reader_never_waits_on_a_busy_owner() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let copies = Arc::new(Copies::start_with(sim_bus(), crate::rt::shard_count().max(1), 8));
            let (bus, id) = (sim_bus(), "s1".to_owned());
            let info = Info { id: id.clone(), kind: "claude".into(), ..Info::default() };
            let i = id.clone();
            copies.on(&id, move |s| s.insert(i, Mirror::new(info, Location::Local, bus))).await;
            // The owner, stuck for 400 ms in the middle of a change it has applied but not published.
            let (stuck, release) = (Arc::new(std::sync::Barrier::new(2)), std::time::Duration::from_millis(400));
            let (s2, i, key) = (Arc::clone(&stuck), id.clone(), id.clone());
            let busy = {
                let copies = Arc::clone(&copies);
                crate::rt::spawn(async move {
                    copies
                        .on(&key, move |s| {
                            if let Some(m) = s.get_mut(&i) {
                                m.rev += 1;
                                s2.wait();
                                std::thread::sleep(release);
                                m.publish();
                            }
                        })
                        .await
                })
            };
            let (c, i) = (Arc::clone(&copies), id.clone());
            let read = crate::rt::on_thread("reader", move || {
                stuck.wait();
                let t0 = std::time::Instant::now();
                let rev = c.with(&i, |v| v.rev);
                (rev, t0.elapsed())
            })
            .await?;
            anyhow::ensure!(read.0 == Some(0), "the view from before: {:?}", read.0);
            anyhow::ensure!(read.1 < std::time::Duration::from_millis(100), "the read waited {:?}", read.1);
            busy.await.map_err(|_| anyhow::anyhow!("the change never ended"))?;
            anyhow::ensure!(copies.with(&id, |v| v.rev) == Some(1), "the new view once published");
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: with every owner's inbox holding two messages and the owners
    /// held up (their shards busy), many senders on every shard each send their session's changes;
    /// they wait for room rather than drop any, and each session's copy takes every change once, in
    /// the order its sender sent them.
    #[test]
    fn changes_keep_their_order_and_none_is_lost_when_shards_are_busy() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let shards = crate::rt::shard_count().max(1);
            let copies = Arc::new(Copies::start_with(sim_bus(), shards, 2));
            let sessions: Vec<String> = (0..24).map(|n| format!("s{n}")).collect();
            for id in &sessions {
                let (i, bus) = (id.clone(), sim_bus());
                let info = Info { id: id.clone(), kind: "claude".into(), ..Info::default() };
                copies.on(id, move |s| s.insert(i, Mirror::new(info, Location::Local, bus))).await;
            }
            // Each shard's owner held for a while, as a busy shard holds it.
            for k in 0..shards {
                let copies = Arc::clone(&copies);
                crate::rt::spawn(async move {
                    for _ in 0..5 {
                        copies.on_shard(k, |_| std::thread::sleep(std::time::Duration::from_millis(20))).await;
                    }
                });
            }
            let per = 300u64;
            let (done_tx, done_rx) = async_channel::unbounded::<()>();
            for id in sessions.clone() {
                let (copies, done) = (Arc::clone(&copies), done_tx.clone());
                // Spread over the shards: senders on every one, as runners' and people's are.
                crate::rt::spawn_spread(async move {
                    for n in 1..=per {
                        let i = id.clone();
                        let job: Job = Box::new(move |s: &mut Shard| {
                            if let Some(m) = s.get_mut(&i) {
                                m.rev += 1;
                                m.upsert(Arc::new(Entry::new(json!({"id": format!("{i}-{n}"), "seq": n}))));
                                m.commit(|_| None);
                            }
                        });
                        if !copies.send(&id, Msg::Run(job)).await {
                            return;
                        }
                        if n % 50 == 0 {
                            crate::rt::maybe_yield().await;
                        }
                    }
                    let _ = done.send(()).await;
                });
            }
            drop(done_tx);
            for _ in 0..sessions.len() {
                done_rx.recv().await?;
            }
            copies.settled().await;
            for id in &sessions {
                let got = copies.with(id, |v| v.events.iter().filter_map(|e| seq(&e.entry)).collect::<Vec<_>>()).unwrap_or_default();
                let want: Vec<u64> = (1..=per).collect();
                anyhow::ensure!(got == want, "{id}: {} of {per} changes, in order: {}", got.len(), got == want);
                anyhow::ensure!(copies.with(id, |v| v.rev) == Some(per), "{id}'s revision");
            }
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 3: a view is swapped in whole. Readers on their own threads,
    /// loading views while the owner changes the copy as fast as it can, each find every view
    /// whole (its events, revision and state agree) and never one older than one they had.
    #[test]
    fn readers_see_whole_views_and_never_go_back() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let copies = Arc::new(Copies::start_with(sim_bus(), crate::rt::shard_count().max(1), 64));
            let (id, bus) = ("s1".to_owned(), sim_bus());
            let info = Info { id: id.clone(), kind: "claude".into(), ..Info::default() };
            let i = id.clone();
            copies.on(&id, move |s| s.insert(i, Mirror::new(info, Location::Local, bus))).await;
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut readers = Vec::new();
            for _ in 0..3 {
                let (c, i, stop) = (Arc::clone(&copies), id.clone(), Arc::clone(&stop));
                readers.push(std::thread::spawn(move || -> Result<u64, String> {
                    let (mut last, mut looked) = (0u64, 0u64);
                    while !stop.load(std::sync::atomic::Ordering::Acquire) {
                        let Some((rev, events, said)) = c.with(&i, |v| (v.rev, u64::try_from(v.events.len()).unwrap_or(0), v.state.get("rev").and_then(Value::as_u64).unwrap_or(0))) else { continue };
                        if rev < last {
                            return Err(format!("went back from {last} to {rev}"));
                        }
                        if events != rev || said != rev {
                            return Err(format!("a view not whole: revision {rev}, {events} events, its state says {said}"));
                        }
                        last = rev;
                        looked += 1;
                    }
                    Ok(looked)
                }));
            }
            let changes = 3000u64;
            for n in 1..=changes {
                let i = id.clone();
                copies
                    .send(
                        &id,
                        Msg::Run(Box::new(move |s: &mut Shard| {
                            if let Some(m) = s.get_mut(&i) {
                                m.rev += 1;
                                m.upsert(Arc::new(Entry::new(json!({"id": format!("e{n}"), "seq": n}))));
                                m.state = Arc::new(json!({"rev": m.rev}));
                                m.commit(|_| None);
                            }
                        })),
                    )
                    .await;
            }
            copies.settled().await;
            stop.store(true, std::sync::atomic::Ordering::Release);
            for r in readers {
                let looked = r.join().map_err(|_| anyhow::anyhow!("a reader panicked"))?.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(looked > 0, "a reader saw nothing");
            }
            anyhow::ensure!(copies.with(&id, |v| v.rev) == Some(changes));
            Ok(())
        })
    }
}
