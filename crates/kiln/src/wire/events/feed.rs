//! The events socket's feed (docs/wire.md, "The events socket"): every frame
//! [`crate::hub::Hub::emit`] sends, numbered and kept in a ring, so a page that reconnects takes up
//! where it was (`?after=<epoch>.<seq>`) instead of loading everything again; and what each socket
//! is sent of it ([`Outbox`]): the types it asked for (`topics`), gathered for a short window, only
//! the newest of each thing a frame carries whole, and none that says again what that socket was
//! last told. Pure: no clock (the socket's loop keeps time), no disk, no network.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash as _, Hasher as _};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use serde_json::Value;

/// Frames kept for a page to resume from. The events broadcast this ring stands beside holds 4096
/// frames for a socket that falls behind before that socket is told `resync` (`Hub::events`,
/// `Broadcast::new(4096)`): the ring keeps as many, sharing their text, so a socket that falls
/// behind is told `resync` exactly when it was before, and a page that was away for no more than
/// that many frames resumes instead.
pub const RING: usize = 4096;

/// How long a socket that asked for batches gathers frames before it sends them: T3 Code's
/// `Stream.groupedWithin(512, Duration.millis(50))` (`apps/server/src/ws.ts`, its shell stream).
/// It carries over: kiln's pages take events in at most every 250 ms themselves (`APPLY_MS`,
/// `ui/src/lib/resource.ts`) and load again at most once a second (`EVENT_GAP_MS`), so 50 ms more
/// shows nowhere, while a burst of agents' or legs' changes goes as one frame. A frame after a
/// quiet window goes at once (the window starts at the last send), so a lone change is as quick as
/// before.
pub const WINDOW: std::time::Duration = std::time::Duration::from_millis(50);

/// At most this many frames gathered before they go, window or not: T3 Code's 512, the same call.
pub const MAX_BATCH: usize = 512;

/// What a frame is about, for coalescing: frames with the same key say the same thing's newest, so
/// only the last of a batch goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// A thing's whole state (a run, a leg, a runner, an agent): only the newest goes, and none
    /// identical to what the socket was last sent of it.
    State(String),
    /// "Fetch this again", or a count that's only ever its newest (`queue`): only the last of a
    /// batch goes, but each batch's goes (it says something happened again).
    Latest(String),
}

/// Whose an agent's frame is, for the callers who see only some agents (`AgentsSeen`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    /// Not an agent's frame.
    Free,
    /// An agent's frame (`harness`, `harness_closed`, `agent_change`): the agent, when it names one.
    Of(Option<String>),
}

/// What the socket's filters and coalescing read of a frame: read from its text once, by the first
/// socket that asks, not once a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// Its tag (`type`); empty when the text isn't a JSON object.
    pub kind: String,
    pub key: Option<Key>,
    /// The repository it names (`repo`, as a name or `{name}`; or its run's `repo_name`).
    pub repo: Option<String>,
    /// For a leg's frame: its run.
    pub job_run: Option<String>,
    /// For an inbox's frame: whose.
    pub person: Option<String>,
    pub agent: Agent,
    /// Its text read as JSON.
    pub parsed: bool,
    /// A digest of its text: two frames of a thing with the same digest say the same.
    pub digest: u64,
}

fn text_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

impl Meta {
    /// What `text`, one of [`crate::wire::events::ServerEvent`]'s frames, says.
    pub fn of(text: &str) -> Meta {
        let mut h = std::hash::DefaultHasher::new();
        text.hash(&mut h);
        let digest = h.finish();
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Meta { kind: String::new(), key: None, repo: None, job_run: None, person: None, agent: Agent::Free, parsed: false, digest };
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        let repo = v
            .get("repo")
            .and_then(|r| r.as_str().map(str::to_owned).or_else(|| r.get("name").and_then(Value::as_str).map(str::to_owned)))
            .or_else(|| text_at(&v, "/run/repo_name").map(str::to_owned));
        let job_run = text_at(&v, "/job/run_id").map(str::to_owned);
        let person = (kind == "inbox").then(|| v.get("person").and_then(Value::as_str).map(str::to_owned)).flatten();
        let agent = match kind.as_str() {
            "harness" => Agent::Of(text_at(&v, "/info/id").map(str::to_owned)),
            "harness_closed" => Agent::Of(text_at(&v, "/id").map(str::to_owned)),
            "agent_change" => Agent::Of(text_at(&v, "/agent").map(str::to_owned)),
            _ => Agent::Free,
        };
        let key = key_of(&kind, &v, text);
        Meta { kind, key, repo, job_run, person, agent, parsed: true, digest }
    }
}

/// What a frame is about, for coalescing (docs/wire.md, "The events socket"). Only frames that
/// carry a thing's whole state, or say no more than "fetch this again", have one: a notice
/// (`run.failed`, `mention` ...), a sample of a series (`sample`), a part of a list (`cd_apps`), what
/// one refresh of Ship's rows changed (`ship`: a later frame of a repository doesn't say again the
/// rows an earlier one did) or any frame not named here is never coalesced.
fn key_of(kind: &str, v: &Value, text: &str) -> Option<Key> {
    let state = |prefix: &str, pointer: &str| text_at(v, pointer).map(|id| Key::State(format!("{prefix}:{id}")));
    match kind {
        // The whole run (`store::Run`), leg without its spec (`store::Job`), runner (`AgentLive`),
        // agent (`harness::session::Info`), upgrade with all its output (`updates::Job`).
        "run" => state("run", "/run/id"),
        "job" => state("job", "/job/id"),
        "agent" => state("agent", "/agent/name"),
        // An agent's entry and its going are one thing's state: the last of them stands.
        "harness" => state("harness", "/info/id"),
        "harness_closed" => state("harness", "/id"),
        "update_job" => state("update_job", "/job/id"),
        // The scheduler's counts: only the newest means anything (a land queue's entry, also tagged
        // `queue`, is a thing's state).
        "queue" if v.get("queued").is_some() => Some(Key::Latest("queue".to_owned())),
        "queue" => state("queue", "/entry"),
        // "Fetch it again", of something or of one thing: the same frame twice in a batch is once.
        "terminals" | "updates" | "places" | "discovery" | "join_requests" | "tailscale" | "push" | "peers" | "keep" | "ai_sessions" | "plan_stack" | "front_door" | "agent_limit" | "headroom" | "program" | "cd_rollout" | "inbox" => {
            Some(Key::Latest(text.to_owned()))
        }
        _ => None,
    }
}

/// A frame of the feed: its number, its text (shared with the events broadcast) and what's read of
/// it.
#[derive(Debug)]
pub struct Frame {
    pub seq: u64,
    pub text: Arc<str>,
    meta: OnceLock<Meta>,
}

impl Frame {
    pub fn new(seq: u64, text: Arc<str>) -> Frame {
        Frame { seq, text, meta: OnceLock::new() }
    }

    /// What's read of it, read the first time it's asked.
    pub fn meta(&self) -> &Meta {
        self.meta.get_or_init(|| Meta::of(&self.text))
    }
}

/// Where a page is in the feed: the feed's epoch (one per server process, so a number from another
/// isn't taken) and the number of the newest frame it was told of. Said `<epoch>.<seq>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub epoch: u64,
    pub seq: u64,
}

impl At {
    pub fn parse(s: &str) -> Option<At> {
        let (e, n) = s.split_once('.')?;
        Some(At { epoch: e.parse().ok()?, seq: n.parse().ok()? })
    }
}

impl std::fmt::Display for At {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.epoch, self.seq)
    }
}

/// What the feed has after a number.
#[derive(Debug)]
pub enum Since {
    /// Every frame after it, oldest first.
    Frames(Vec<Arc<Frame>>),
    /// Some of them are gone (or the number isn't one of this feed's): what's missed is unknown.
    Gap,
}

struct Ring {
    /// The number the next frame gets (the first is 1).
    next: u64,
    frames: VecDeque<Arc<Frame>>,
}

/// The ring of the newest [`RING`] frames, and a wake for the sockets that read it.
pub struct Feed {
    epoch: u64,
    cap: usize,
    ring: Mutex<Ring>,
    grew: event_listener::Event,
}

impl Feed {
    /// A feed of `epoch` (a server process's: `rand::random::<u64>() >> 11`, 53 bits, as JSON carries
    /// exactly) keeping [`RING`] frames.
    pub fn new(epoch: u64) -> Feed {
        Feed::with_capacity(epoch, RING)
    }

    pub fn with_capacity(epoch: u64, cap: usize) -> Feed {
        Feed { epoch, cap: cap.max(1), ring: Mutex::new(Ring { next: 1, frames: VecDeque::new() }), grew: event_listener::Event::new() }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Numbers `text` and keeps it (the oldest goes past [`RING`]), and wakes the sockets.
    pub fn push(&self, text: Arc<str>) -> u64 {
        let seq = {
            let mut r = self.ring.lock();
            let seq = r.next;
            r.next += 1;
            r.frames.push_back(Arc::new(Frame::new(seq, text)));
            while r.frames.len() > self.cap {
                r.frames.pop_front();
            }
            seq
        };
        self.grew.notify(usize::MAX);
        seq
    }

    /// The newest frame's number (0 before the first).
    pub fn head(&self) -> u64 {
        self.ring.lock().next.saturating_sub(1)
    }

    /// Where the feed is now.
    pub fn at(&self) -> At {
        At { epoch: self.epoch, seq: self.head() }
    }

    /// Every frame after `after`, or [`Since::Gap`] when some of them are gone.
    pub fn since(&self, after: u64) -> Since {
        let r = self.ring.lock();
        let head = r.next.saturating_sub(1);
        if after > head {
            return Since::Gap;
        }
        let oldest = r.frames.front().map_or(r.next, |f| f.seq);
        if after.saturating_add(1) < oldest {
            return Since::Gap;
        }
        let skip = usize::try_from(after.saturating_add(1) - oldest).unwrap_or(usize::MAX);
        Since::Frames(r.frames.iter().skip(skip).cloned().collect())
    }

    /// Every frame it has, oldest first.
    pub fn kept(&self) -> Vec<Arc<Frame>> {
        self.ring.lock().frames.iter().cloned().collect()
    }

    /// Where a page that asked to resume from `after` starts: its number when the feed has
    /// everything after it, else where the feed is now, and `true` (it's told `resync`). One that
    /// didn't ask starts where the feed is now.
    pub fn resume(&self, after: Option<&str>) -> (u64, bool) {
        let Some(after) = after else { return (self.head(), false) };
        match At::parse(after) {
            Some(a) if a.epoch == self.epoch && matches!(self.since(a.seq), Since::Frames(_)) => (a.seq, false),
            _ => (self.head(), true),
        }
    }

    /// Wakes when a frame is pushed: listen before looking, then wait.
    pub fn listen(&self) -> event_listener::EventListener {
        self.grew.listen()
    }
}

/// The event types of a `?topics=a,b` list.
pub fn topics(list: &str) -> HashSet<String> {
    list.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).collect()
}

/// What one socket that asked for batches is to be sent: what it's gathered since its last send,
/// and the digest of what it was last sent of each thing.
#[derive(Debug, Default)]
pub struct Outbox {
    /// The types it asked for; `None`: all.
    topics: Option<HashSet<String>>,
    pending: Vec<Arc<Frame>>,
    resync: bool,
    /// What it was last sent of each thing ([`Key::State`]), by digest.
    last: HashMap<Key, u64>,
}

impl Outbox {
    pub fn new(topics: Option<HashSet<String>>) -> Outbox {
        Outbox { topics, ..Outbox::default() }
    }

    /// It asked for frames of `kind` (`resync` it always gets).
    pub fn wants(&self, kind: &str) -> bool {
        kind == "resync" || self.topics.as_ref().is_none_or(|t| t.contains(kind))
    }

    /// The types it asks for now; what's new among them (all of them, when it asked for every type
    /// before, is nothing new). What it was last sent of each thing is forgotten: of a type it
    /// stopped hearing it may have missed a change, so the next frame of that thing has to go.
    pub fn set_topics(&mut self, topics: Option<HashSet<String>>) -> HashSet<String> {
        let added = match (&self.topics, &topics) {
            (Some(had), Some(now)) => now.difference(had).cloned().collect(),
            _ => HashSet::new(),
        };
        self.topics = topics;
        self.last.clear();
        added
    }

    /// Gathers `f` (it passed the socket's filters).
    pub fn add(&mut self, f: Arc<Frame>) {
        self.pending.push(f);
    }

    /// Gathers frames of what it newly asked for, from before (`add`s keep their order by number).
    pub fn add_earlier(&mut self, frames: impl IntoIterator<Item = Arc<Frame>>) {
        self.pending.extend(frames);
        self.pending.sort_by_key(|f| f.seq);
        self.pending.dedup_by_key(|f| f.seq);
    }

    /// Frames were missed: what it gathered goes, and the next send says `resync` (the page loads
    /// what it shows again, so what it was last sent counts for nothing).
    pub fn resync(&mut self) {
        self.pending.clear();
        self.last.clear();
        self.resync = true;
    }

    pub fn len(&self) -> usize {
        self.pending.len() + usize::from(self.resync)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What it's to be sent now, as one `batch` frame saying where it is (`at`), or `None` when
    /// nothing gathered is left to say (and `always` isn't set: the first frame of a socket always
    /// goes, saying where the feed is).
    pub fn flush(&mut self, at: At, always: bool) -> Option<String> {
        let resync = std::mem::take(&mut self.resync);
        let frames = coalesce(std::mem::take(&mut self.pending));
        let mut texts: Vec<&str> = Vec::with_capacity(frames.len() + 1);
        if resync {
            texts.push(RESYNC);
        }
        // What it holds of each thing is bounded as the ring is: past that, it forgets, which costs
        // at most a frame of each thing that says again what it was told.
        if self.last.len() > RING {
            self.last.clear();
        }
        for f in &frames {
            if let Some(k @ Key::State(_)) = &f.meta().key {
                let d = f.meta().digest;
                if self.last.get(k) == Some(&d) {
                    continue;
                }
                self.last.insert(k.clone(), d);
            }
            texts.push(&f.text);
        }
        (always || !texts.is_empty()).then(|| batch(at, &texts))
    }
}

/// `{"type":"resync"}`, as [`crate::wire::events::ServerEvent::Resync`] serializes.
const RESYNC: &str = r#"{"type":"resync"}"#;

/// The newest frame of each thing, in the order of the newest ones: a frame with a key goes only
/// when no later one of the batch has it. Frames without one all go, where they were.
pub fn coalesce(frames: Vec<Arc<Frame>>) -> Vec<Arc<Frame>> {
    let mut seen: HashSet<&Key> = HashSet::new();
    let mut keep = vec![false; frames.len()];
    for (i, f) in frames.iter().enumerate().rev() {
        let fresh = match &f.meta().key {
            Some(k) => seen.insert(k),
            None => true,
        };
        if let Some(slot) = keep.get_mut(i) {
            *slot = fresh;
        }
    }
    frames.into_iter().zip(keep).filter_map(|(f, k)| k.then_some(f)).collect()
}

/// A `batch` frame of `events` (each a frame's text), as [`crate::wire::events::EventsDown::Batch`]
/// serializes: written, not serialized again.
pub fn batch(at: At, events: &[&str]) -> String {
    let len = events.iter().map(|e| e.len() + 1).sum::<usize>() + 64;
    let mut s = String::with_capacity(len);
    s.push_str(r#"{"t":"batch","at":""#);
    s.push_str(&at.to_string());
    s.push_str(r#"","events":["#);
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(e);
    }
    s.push_str("]}");
    s
}

#[cfg(test)]
mod tests;
