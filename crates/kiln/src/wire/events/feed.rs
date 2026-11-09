//! The events socket's feed (docs/wire.md, "The events socket"): every frame
//! [`crate::hub::Hub::emit`] sends, numbered and kept in a ring, so a page that reconnects takes up
//! where it was (`?after=<epoch>.<seq>`) instead of loading everything again; and what each socket
//! is sent of it ([`Outbox`]): the types it asked for (`topics`), gathered for a short window, only
//! the newest of each thing a frame carries whole, and none that says again what that socket was
//! last told. Pure: no clock (the socket's loop keeps time), no disk, no network. No lock on any path
//! ([`Feed`]: goal 5's rule, plan 01a1a59c2aae): every emit pushes, from whichever shard emits, and
//! every socket reads, on its own.
//!
//! What it keeps is `crate::memory::owners::EVENTS_FEED` on `/metrics` (goal 11, plan 01a1a87b2e56 step 5):
//! at most [`RING`] frames and [`BYTES`] of them, and a frame an agent's next one says again whole is let go
//! of as that one comes: its pusher holds its text ([`Feed::push_lent`], [`Lent`]).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use arc_swap::ArcSwapOption;
use serde_json::Value;

/// The frames the feed keeps: as many as a listener of the hub's events holds of them
/// ([`crate::hub::EVENTS`]), sharing their text, so a socket that falls behind is told `resync` when
/// such a listener would start over, and a page that was away for no more than that many frames
/// resumes instead.
pub const RING: usize = crate::hub::EVENTS;

/// The most the feed's places hold, its frames' texts and their own room ([`Frame::weight`]): as many bytes
/// of the texts as a listener of the hub's events holds before it starts over (`crate::net::EVENTS_BYTES`,
/// `crate::memory::owners::UI_EVENTS`), as [`RING`] is as many frames. Past it the oldest go as past [`RING`]
/// (a socket after them is told `resync`), never the newest: a frame larger than this alone is kept until the
/// next (goal 11, plan 01a1a87b2e56 step 5).
pub const BYTES: usize = crate::net::EVENTS_BYTES;

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
    /// For an inbox's frame, or an attention change's: whose.
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
        // Whose a frame is, when it's one person's: an inbox's `person`, an attention change's `principal` (attention plan
        // 01a199e1b177752e84dad5dc990e6a15).
        let person = match kind.as_str() {
            "inbox" => v.get("person"),
            "attention" => v.get("principal"),
            _ => None,
        }
        .and_then(Value::as_str)
        .map(str::to_owned);
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
    /// Its text. Empty in a place of a frame whose text its pusher holds ([`Feed::push_lent`]): what a reader is
    /// handed of that place has it, while its pusher holds it.
    pub text: Arc<str>,
    /// Boxed, made by the first socket that reads it: most of a ring's frames are never read whole (an
    /// agent's said again, [`Feed::push_lent`]), and each kept a [`Meta`]'s room, about 180 bytes, regardless
    /// (goal 11, plan 01a1a87b2e56 step 5).
    meta: OnceLock<Box<Meta>>,
    /// In a place, a frame of [`Feed::push_lent`]'s: the cell of the text its pusher holds ([`Lent`]), until it's
    /// said again. A cell of its own, so the text's bytes go as its pusher lets go of it (an `Arc<str>`'s bytes are
    /// its allocation, kept while any weak reference to it is): the place keeps the cell's room alone.
    lent: Option<Weak<Arc<str>>>,
    /// Handed to a reader of a place of [`Feed::push_lent`]'s: that place's frame, which keeps what's read of it
    /// ([`Frame::meta`]) for every socket that reads it.
    place: Option<Arc<Frame>>,
}

impl Frame {
    pub fn new(seq: u64, text: Arc<str>) -> Frame {
        Frame { seq, text, meta: OnceLock::new(), lent: None, place: None }
    }

    /// Frame `seq` of the text in `cell`, which its pusher holds ([`Feed::push_lent`]): its place holds its number and
    /// a way to the text alone, so it takes little more than a frame of its number did once said again.
    fn lent(seq: u64, cell: &Arc<Arc<str>>) -> Frame {
        Frame { seq, text: Arc::from(""), meta: OnceLock::new(), lent: Some(Arc::downgrade(cell)), place: None }
    }

    /// What a reader is handed of this place's frame: itself; for one whose pusher holds its text, a frame of that
    /// text while it's held, sharing what's read of it with this one, and none once it isn't (said again whole by
    /// a later one: readers pass over it).
    fn read(self: &Arc<Frame>) -> Option<Arc<Frame>> {
        match &self.lent {
            None => Some(Arc::clone(self)),
            Some(lent) => lent.upgrade().map(|cell| Arc::new(Frame { seq: self.seq, text: Arc::clone(&cell), meta: OnceLock::new(), lent: None, place: Some(Arc::clone(self)) })),
        }
    }

    /// What's read of it, read the first time it's asked (kept by its place's frame, for one read of a place of
    /// [`Feed::push_lent`]'s, so every socket that reads it shares it).
    pub fn meta(&self) -> &Meta {
        let of = self.place.as_deref().unwrap_or(self);
        of.meta.get_or_init(|| Box::new(Meta::of(&self.text)))
    }

    /// What it takes in a place of the feed: its text and its own room, each allocation's with its
    /// reference counts (an `Arc`'s two words). What's read of it ([`Frame::meta`], made only when a socket
    /// reads it: its kind, key and names, parts of its text) isn't counted, nor a text its pusher holds
    /// ([`Lent`] counts that): its place's text is empty, and it keeps that text's cell's room.
    pub fn weight(&self) -> usize {
        let cell = if self.lent.is_some() { COUNTS + size_of::<Arc<str>>() } else { 0 };
        size_of::<Frame>() + COUNTS + self.text.len() + COUNTS + cell
    }
}

/// An `Arc`'s reference counts, two words, beside what it holds.
const COUNTS: usize = 2 * size_of::<usize>();

/// The text of a frame as its pusher holds it ([`Feed::push_lent`]): an agent's info, which its copy holds until its
/// next is in the feed. The feed's place has the frame's number and a way to the text while this is held; let go of
/// (its next said, or its copy gone), the text goes with it, and readers pass over the frame, with nothing put in its
/// place to say so (goal 9, plan 01a1a87b2cec step 5: what stood for it was a second frame put in a place, a second
/// of arc-swap's walks of every thread's debt slots an info's change). Counted in
/// `crate::memory::owners::EVENTS_FEED` while held, as the feed's own frames are.
#[derive(Debug)]
pub struct Lent {
    seq: u64,
    /// The text, in a cell of its own: the feed's place holds a weak reference to the cell, never to the text.
    cell: Arc<Arc<str>>,
}

impl Lent {
    fn new(seq: u64, cell: Arc<Arc<str>>) -> Lent {
        crate::memory::owners::EVENTS_FEED.add(Lent::weight(&cell));
        Lent { seq, cell }
    }

    /// The text's bytes and its counts.
    fn weight(text: &str) -> usize {
        text.len() + COUNTS
    }

    /// The frame's number in the feed.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        crate::memory::owners::EVENTS_FEED.sub(Lent::weight(&self.cell));
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

/// The newest [`RING`] frames, numbered in the order they're pushed, and a wake for each socket that reads
/// them, with no lock on any path (goal 5's rule, plan 01a1a59c2aae: no lock two shards share; as goal 4,
/// plan 01a1a59c2a9c step 5, made the hub's events' lanes). Every emit pushes, from whichever shard emits
/// it (an agent session's change from its owner's: about 1,700 a second in the lab's many-sessions
/// profile), and every events socket reads, on its own shard: a ring behind one lock, as this was, had
/// them all take it.
///
/// - **Numbers.** A push claims the next number (a fetch-add on `next`), so frames are numbered in the
///   order their pushes came, then puts its frame in its place (`seq % cap`), then wakes the listeners.
/// - **Places.** Each holds the newest frame put in it, swapped in whole (`arc_swap`): a push's frame goes
///   in unless a newer one already has (a push held up while a whole window of others went by: its frame
///   is gone before anyone read it, as the oldest of a full ring goes).
/// - **Reading.** A reader walks the places from its number on, taking each that holds the number it
///   expects, and stops at the first that doesn't hold it yet (its push still putting it there): it never
///   skips a frame, and that push wakes it once the frame's in. A place that holds a newer number than it
///   expects, or a number further than [`RING`] behind the head, says what it wants is gone:
///   [`Since::Gap`], and the socket is told `resync`.
/// - **Head** is the newest number claimed: a frame at or below it is in its place, being put there, or
///   gone.
/// - **Waking.** Each listener ([`Feed::listen`]) has a wake of its own, `crate::rt::Notify` (a permit, so
///   a push while it looks isn't lost), in a list swapped in whole as listeners come and go (`crate::rcu`).
///   A push wakes each. A wake is Notify's to make: where Notify still waits on event-listener, its list
///   of waiters is behind a lock of that one socket's, not one every push and every socket take, as the
///   one wake this had was; where Notify waits on a lock-free list, the wake takes no lock at all.
///
/// What it holds (goal 11, plan 01a1a87b2e56 step 5), counted as `crate::memory::owners::EVENTS_FEED`:
///
/// - **Said again whole.** Most of what's pushed is an agent's info (`harness`), each change of it whole, so
///   a ring of them held one agent's info hundreds of times over, every copy but its newest one no socket
///   is sent (a batch sends a thing's newest alone, [`coalesce`]; a page that resumes is sent what's after
///   where it was, where its newest is too). Its pusher holds its text ([`Feed::push_lent`]: a [`Lent`], which
///   an agent's copy keeps until its next info is in), and its place a way to that text alone: once the pusher
///   lets go of it, the text's memory is given back and readers pass over the frame. Nothing is put in its place
///   to say so (goal 9, plan 01a1a87b2cec step 5: a frame standing for it was, each a second store to the
///   places an info's change, so a second walk of every thread's debt slots). Numbers, gaps and resumes are as
///   they were. A text a reader still holds (a socket's batch, a session's lane) stays readable until it lets go.
/// - **Bytes.** Its places hold at most [`BYTES`] of frames ([`Frame::weight`]): past it the oldest go, as past
///   [`RING`]. `floor` is the oldest number it may still have; a push past the bytes raises it a frame at a
///   time (a compare-and-swap, so each is let go of by one push alone) and empties that frame's place.
///   Readers check `floor` before a place and again after one that holds nothing yet, so a frame let go of as
///   they look is gone to them, never one they wait for; a push held up past its frame's floor takes it out
///   again itself, so whichever of the two comes second lets it go.
pub struct Feed {
    epoch: u64,
    /// The number the next push claims (the first is 1).
    next: AtomicU64,
    /// The oldest number the feed may still have: those before it were let go of for its bytes ([`BYTES`]).
    floor: AtomicU64,
    /// What its places hold ([`Frame::weight`]), and the most they may.
    held: AtomicUsize,
    most: usize,
    /// Frame `seq`'s place is `seq % places.len()`: each holds the newest frame put in it, or none yet.
    places: Box<[ArcSwapOption<Frame>]>,
    /// The sockets listening, each its own wake.
    listeners: crate::rcu::Rcu<Vec<Arc<crate::rt::Notify>>>,
}

/// What a place holds of the frame a reader expects there.
enum Look {
    /// That frame.
    Frame(Arc<Frame>),
    /// A frame its pusher held the text of and let go ([`Feed::push_lent`]): said again whole by a later one,
    /// passed over.
    Over,
    /// Not that frame yet: its push is still putting it there (or nothing's been put there yet).
    NotYet,
    /// A newer one, or let go of for the feed's bytes: that frame's gone.
    Gone,
}

impl Drop for Feed {
    fn drop(&mut self) {
        crate::memory::owners::EVENTS_FEED.sub(*self.held.get_mut());
        crate::memory::owners::EVENTS_FEED.bound_sub(self.most);
    }
}

/// Whether two of a place's values are the same frame (or both none).
fn same(a: &Option<Arc<Frame>>, b: &Option<Arc<Frame>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Feed {
    /// A feed of `epoch` (a server process's: `rand::random::<u64>() >> 11`, 53 bits, as JSON carries
    /// exactly) keeping [`RING`] frames.
    pub fn new(epoch: u64) -> Feed {
        Feed::with_capacity(epoch, RING)
    }

    pub fn with_capacity(epoch: u64, cap: usize) -> Feed {
        Feed::with_room(epoch, cap, BYTES)
    }

    /// A feed keeping `cap` frames and `bytes` of them at the most ([`Frame::weight`]).
    pub fn with_room(epoch: u64, cap: usize, bytes: usize) -> Feed {
        let places = (0..cap.max(1)).map(|_| ArcSwapOption::empty()).collect();
        crate::memory::owners::EVENTS_FEED.bound_add(bytes);
        Feed { epoch, next: AtomicU64::new(1), floor: AtomicU64::new(1), held: AtomicUsize::new(0), most: bytes, places, listeners: crate::rcu::Rcu::default() }
    }

    /// What its places hold now ([`Frame::weight`]).
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    fn hold(&self, n: usize) {
        self.held.fetch_add(n, Ordering::AcqRel);
        crate::memory::owners::EVENTS_FEED.add(n);
    }

    fn let_hold(&self, n: usize) {
        let mut now = self.held.load(Ordering::Acquire);
        while let Err(seen) = self.held.compare_exchange_weak(now, now.saturating_sub(n), Ordering::AcqRel, Ordering::Acquire) {
            now = seen;
        }
        crate::memory::owners::EVENTS_FEED.sub(n);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// How many frames it keeps.
    fn cap(&self) -> u64 {
        u64::try_from(self.places.len()).unwrap_or(u64::MAX)
    }

    /// Frame `seq`'s place.
    fn place(&self, seq: u64) -> Option<&ArcSwapOption<Frame>> {
        let i = usize::try_from(seq % self.cap().max(1)).ok()?;
        self.places.get(i)
    }

    /// The oldest number a feed whose head is `head` keeps: the window of [`RING`] behind it, and none let
    /// go of for its bytes.
    fn oldest(&self, head: u64) -> u64 {
        head.saturating_add(1).saturating_sub(self.cap()).max(1).max(self.floor.load(Ordering::SeqCst))
    }

    /// Numbers `text` and keeps it (the oldest goes past [`RING`] or [`BYTES`]), and wakes the sockets.
    pub fn push(&self, text: Arc<str>) -> u64 {
        let seq = self.next.fetch_add(1, Ordering::AcqRel);
        self.put(Arc::new(Frame::new(seq, text)));
        self.trim(seq);
        self.wake();
        seq
    }

    /// [`Feed::push`] of a frame its pusher holds the text of ([`Lent`]): what a later frame of the same thing will
    /// say again whole (an agent's info). Its place keeps its number and a way to the text; once the pusher lets go
    /// of it (as its next is pushed, or its copy goes), readers pass over the frame, and its text's memory is given
    /// back with nothing more put in the feed (goal 9, plan 01a1a87b2cec step 5: an info's change put two frames in
    /// two places, its own and what stood for its last, each a walk of every thread's debt slots in arc-swap; now
    /// one). Numbers, gaps and resumes are as for any frame.
    pub fn push_lent(&self, text: Arc<str>) -> Lent {
        let seq = self.next.fetch_add(1, Ordering::AcqRel);
        let cell = Arc::new(text);
        self.put(Arc::new(Frame::lent(seq, &cell)));
        self.trim(seq);
        self.wake();
        Lent::new(seq, cell)
    }

    /// Puts `frame` in its place, unless a newer frame is there already, or its number was let go of for the
    /// feed's bytes before it came.
    fn put(&self, frame: Arc<Frame>) {
        if frame.seq < self.floor.load(Ordering::SeqCst) {
            return;
        }
        let Some(place) = self.place(frame.seq) else { return };
        let mut there = place.load();
        loop {
            // A push held up past a window of others: its frame's gone, as a full ring's oldest goes.
            if there.as_ref().is_some_and(|f| f.seq > frame.seq) {
                return;
            }
            let was = place.compare_and_swap(&there, Some(Arc::clone(&frame)));
            if same(&was, &there) {
                self.hold(frame.weight());
                if let Some(old) = &*there {
                    self.let_hold(old.weight());
                }
                // Let go of for the bytes as it went in (its floor rose past it after the check above): taken out
                // again, by this push or by the one that raised the floor, whichever saw it second.
                if frame.seq < self.floor.load(Ordering::SeqCst) {
                    self.let_go(frame.seq);
                }
                return;
            }
            // Another push's put went in first (a frame a window before or after this one): looked at again.
            there = was;
        }
    }

    /// The oldest frames let go of while its places hold more than [`BYTES`], up to `newest` (the frame just
    /// pushed, which stays).
    fn trim(&self, newest: u64) {
        // Those a window of [`RING`] behind are gone already, their places taken by newer frames.
        let lapped = newest.saturating_add(1).saturating_sub(self.cap()).max(1);
        let mut floor = self.floor.load(Ordering::SeqCst);
        while self.held() > self.most {
            let oldest = floor.max(lapped);
            if oldest >= newest {
                return;
            }
            match self.floor.compare_exchange(floor, oldest + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    self.let_go(oldest);
                    floor = oldest + 1;
                }
                Err(seen) => floor = seen,
            }
        }
    }

    /// Frame `seq`'s place emptied, if it holds that frame (or what stands for it): its number is below the floor.
    fn let_go(&self, seq: u64) {
        let Some(place) = self.place(seq) else { return };
        let there = place.load();
        let Some(old) = there.as_ref().filter(|f| f.seq == seq) else { return };
        let weight = old.weight();
        let was = place.compare_and_swap(&there, None::<Arc<Frame>>);
        if same(&was, &there) {
            self.let_hold(weight);
        }
    }

    /// Wakes every listener: each takes what's new when it next looks.
    fn wake(&self) {
        for l in self.listeners.load().iter() {
            l.notify();
        }
    }

    /// What frame `want`'s place holds of it.
    fn look(&self, want: u64) -> Look {
        if want < self.floor.load(Ordering::SeqCst) {
            return Look::Gone;
        }
        let Some(place) = self.place(want) else { return Look::Gone };
        match &*place.load() {
            Some(f) if f.seq == want => match f.read() {
                Some(read) => Look::Frame(read),
                None => Look::Over,
            },
            Some(f) if f.seq > want => Look::Gone,
            // Not in yet, or let go of for the bytes as it looked (the floor rises before its place empties).
            _ if want < self.floor.load(Ordering::SeqCst) => Look::Gone,
            _ => Look::NotYet,
        }
    }

    /// The newest frame's number (0 before the first): the newest a push claimed.
    pub fn head(&self) -> u64 {
        self.next.load(Ordering::Acquire).saturating_sub(1)
    }

    /// Where the feed is now.
    pub fn at(&self) -> At {
        At { epoch: self.epoch, seq: self.head() }
    }

    /// Every frame after `after`, up to the first a push is still putting in its place (that push wakes the
    /// listeners once it's in), or [`Since::Gap`] when some of them are gone. Those said again whole by a later
    /// one are passed over: that one's among them, or still to come.
    pub fn since(&self, after: u64) -> Since {
        let head = self.head();
        if after > head || after.saturating_add(1) < self.oldest(head) {
            return Since::Gap;
        }
        let mut frames = Vec::new();
        for want in after.saturating_add(1)..=head {
            match self.look(want) {
                Look::Frame(f) => frames.push(f),
                Look::Over => {}
                Look::NotYet => break,
                Look::Gone => return Since::Gap,
            }
        }
        Since::Frames(frames)
    }

    /// Every frame it has, oldest first: those of the window behind the head in their places, up to the
    /// first a push is still putting in its place, passing over those said again whole.
    pub fn kept(&self) -> Vec<Arc<Frame>> {
        let head = self.head();
        let mut frames = Vec::new();
        for want in self.oldest(head)..=head {
            match self.look(want) {
                Look::Frame(f) => frames.push(f),
                Look::NotYet => break,
                // Gone as pushes went on while it looked: the newer ones are still kept.
                Look::Over | Look::Gone => {}
            }
        }
        frames
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

    /// A listener from now: [`Listener::woken`] completes once a frame was pushed since it last completed,
    /// or since it listened. Listen before looking, then wait: a frame pushed while it looks isn't missed.
    pub fn listen(&self) -> Listener<'_> {
        let wake = Arc::new(crate::rt::Notify::new());
        self.listeners.update(|l| l.push(Arc::clone(&wake)));
        Listener { feed: self, wake }
    }
}

/// A socket listening to a [`Feed`] ([`Feed::listen`]): taken off its listeners as it goes.
pub struct Listener<'a> {
    feed: &'a Feed,
    wake: Arc<crate::rt::Notify>,
}

impl Listener<'_> {
    /// Completes once a frame was pushed since this last completed, or since it listened (a permit: pushes
    /// meanwhile are one wake); a charge of the task's turn on its shard (`crate::rt::Notify::notified`).
    pub async fn woken(&self) {
        self.wake.notified().await;
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        let wake = &self.wake;
        self.feed.listeners.update(|l| l.retain(|w| !Arc::ptr_eq(w, wake)));
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
