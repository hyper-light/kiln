//! A session's live feed on kiln's bus (docs/bus.md; CP-T2.1). What the server learns of a session
//! from the machine it runs on goes to every viewer watching it, on any device, through a topic of
//! its own on the bus: Quiver's ephemeral class, in the server's process. The session's copy
//! publishes each frame from the shard that owns it (`super::copies`); each viewer joins without
//! asking that shard, and reads the topic from the offset its snapshot was taken at, so it misses no
//! frame and sees none twice. A viewer that falls behind
//! what the topic keeps is refused `Behind` (the bus's typed lag notice) and starts over from a
//! snapshot, as one that fell behind always did, while the publish, and every other viewer, goes
//! on: nothing a viewer does holds up the mirror or another viewer.
//!
//! A frame the bus can't take (over its size, or the bus's memory full) is never dropped silently:
//! the feed marks itself broken, and its next publish goes after a resync marker that sends every
//! viewer back to a snapshot. A mirror let go (the session ended) drops its feed, and the feed's
//! topic goes with it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use futures::FutureExt as _;
use kiln_seams::Bus;
use kiln_seams::bus::{Class, Offset, Refusal, Topic};

use super::registry::{Frame, PatchOf, Patches};

/// Frames a session's topic keeps: a viewer further behind starts over from a snapshot.
pub const RETAIN: usize = 2048;

/// Frames a topic made now keeps: [`RETAIN`], or fewer in a test build that asks (so a viewer that
/// stops reading falls behind sooner).
fn retain() -> usize {
    crate::runner::chaos::get().and_then(|c| c.fanout).unwrap_or(RETAIN)
}

/// What a feed's topic keeps by its retention, frame by frame (goal 11, plan 01a1a87b2e56 step 3):
/// each frame's bytes as the bus counts them against `[bus] topic_bytes` (its key and payload; a
/// feed's frames have no key), its newest `retain` alone, and their sum. That's what the topic asks
/// the bus to keep, whether or not `topic_bytes` lets it (past it the bus keeps fewer frames): the
/// largest any topic has asked since the server started is on `/metrics`
/// (`kiln_bus_topic_bytes_peak`), what `topic_bytes` is derived from. Its owner's alone, as the feed
/// is; at most `retain` sizes of 4 bytes (8 KiB a topic), let go with the topic.
#[derive(Default)]
struct Kept {
    sizes: VecDeque<u32>,
    retain: usize,
    bytes: u64,
}

impl Kept {
    /// A new topic's, keeping `retain` frames.
    fn new(retain: usize) -> Self {
        Self { sizes: VecDeque::new(), retain: retain.max(1), bytes: 0 }
    }

    /// A frame of `n` bytes went on the topic: past the retention its oldest goes. What the topic
    /// keeps now.
    fn add(&mut self, n: usize) -> u64 {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        self.sizes.push_back(n);
        self.bytes = self.bytes.saturating_add(u64::from(n));
        while self.sizes.len() > self.retain {
            match self.sizes.pop_front() {
                Some(old) => self.bytes = self.bytes.saturating_sub(u64::from(old)),
                None => break,
            }
        }
        self.bytes
    }
}

/// The marker a feed publishes after a frame the bus refused: its viewers start over. The feed's
/// own, never sent to a viewer: every other frame on a feed is one of the session socket's
/// ([`SessionDown`](crate::wire::ui::SessionDown)).
pub const RESYNC: &str = r#"{"t":"resync"}"#;

/// A session's last frame, as its feed carries it: its viewers end with it.
pub fn closed() -> &'static str {
    static CLOSED: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| crate::wire::ui::SessionDown::from(crate::wire::ui::SessionFrame::Closed).text());
    &CLOSED
}

/// Feeds made in this process: each takes the next, so a session's mirror made again (a restart
/// of its machine's link) has a topic of its own and never shares one it could delete.
static FEEDS: AtomicU64 = AtomicU64::new(0);

/// The topic of generation `generation` of feed `n` of session `id`: `sessions/<id>/<n>.<generation>`,
/// or, for an id outside a topic name's alphabet (or one too long for the name to fit), its SHA-256
/// in its place. Each generation has a topic of its own: one let go while nobody watched is never
/// the one a viewer that came after it reads.
pub fn topic(id: &str, n: u64, generation: u32) -> String {
    let plain = !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    let name = format!("sessions/{id}/{n}.{generation}");
    if plain && name.len() <= kiln_seams::bus::NAME_MAX { name } else { format!("sessions/sha256-{}/{n}.{generation}", kiln_seams::sha256_hex(id.as_bytes())) }
}

/// Microseconds on this process's monotonic clock: a frame's publish and its read are both here
/// (the feed's topic is this process's), so their difference is its delivery, finer than the
/// bus's millisecond stamps.
pub fn now_us() -> u64 {
    us_at(Instant::now())
}

/// [`now_us`]'s clock's start.
fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

/// `at` on [`now_us`]'s clock: an instant read elsewhere (a frame's arrival off the wire), set against a publish.
pub fn us_at(at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(base()).as_micros()).unwrap_or(u64::MAX)
}

/// Where the agent session's update a frame was made of came from, as the frame carries it to the viewers who ask
/// (goal 10, plan 01a1a87b2dae step 5: a frame's time split frame by frame, as an API read's is, since its legs'
/// percentiles don't add up): its frame's arrival off the wire and its owner's take of it, on [`now_us`]'s clock, and
/// its runner's hold of it as its runner said it (`crate::wire::stamp`). Set by the owner while it takes the update
/// ([`carrying`]); every frame it publishes meanwhile carries it ([`Payload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Came {
    /// Its frame's arrival off the wire, where the hub's loop knew it (the first update a frame carried).
    pub arrived_us: Option<u64>,
    /// Its owner's take of it.
    pub took_us: u64,
    /// Its runner's hold of it, where its runner said it.
    pub runner_us: Option<u64>,
}

thread_local! {
    /// What the frames this thread publishes now were made of ([`carrying`]): an owner takes one update at a time, start to
    /// end on its own thread.
    static CAME: std::cell::Cell<Option<Came>> = const { std::cell::Cell::new(None) };
}

/// The frames this thread publishes until the guard goes carry `came` ([`Came`]); what they carried before comes back then.
pub fn carrying(came: Option<Came>) -> Carrying {
    Carrying(CAME.replace(came))
}

/// [`carrying`]'s guard.
pub struct Carrying(Option<Came>);

impl Drop for Carrying {
    fn drop(&mut self) {
        CAME.set(self.0);
    }
}

/// What the frames this thread publishes now carry ([`carrying`]).
pub fn came() -> Option<Came> {
    CAME.get()
}

/// A frame's [`Came`] in its payload's head: its arrival and its owner's take each as microseconds before its publish, and
/// its runner's hold, each a `u32`, [`UNSAID`] where it wasn't known.
const CAME_BYTES: usize = 3 * size_of::<u32>();

/// A [`Came`]'s part a frame doesn't know.
const UNSAID: u32 = u32::MAX;

/// Bytes of a frame's payload ahead of its text ([`encode`]): its publish time (a `u64`), its
/// revision (a `u64`), where its update came from ([`CAME_BYTES`]) and its text's length (a `u32`).
const HEAD: usize = size_of::<u64>() + size_of::<u64>() + CAME_BYTES + size_of::<u32>();

/// Where [`CAME_BYTES`] are in a payload.
const CAME_AT: usize = 2 * size_of::<u64>();

/// What this thread's frames carry now ([`came`]), as a frame published at `published` says it.
fn came_bytes(published: u64) -> [u8; CAME_BYTES] {
    let before = |at: Option<u64>| at.map_or(UNSAID, |at| u32::try_from(published.saturating_sub(at)).unwrap_or(UNSAID));
    let (arrived, took, runner) = match came() {
        Some(c) => (before(c.arrived_us), before(Some(c.took_us)), c.runner_us.map_or(UNSAID, |r| u32::try_from(r).unwrap_or(UNSAID))),
        None => (UNSAID, UNSAID, UNSAID),
    };
    let mut out = [0u8; CAME_BYTES];
    let (words, _) = out.as_chunks_mut::<{ size_of::<u32>() }>();
    for (to, v) in words.iter_mut().zip([arrived, took, runner]) {
        *to = v.to_le_bytes();
    }
    out
}

/// Where the update the frame in `bytes` was made of came from ([`Came`]), on [`now_us`]'s clock; `None` for a frame
/// made of none (the server's own, a resync marker) or bytes [`encode`] didn't make.
pub fn came_of(bytes: &[u8]) -> Option<Came> {
    let published = published_us(bytes)?;
    let said = bytes.get(CAME_AT..CAME_AT + CAME_BYTES)?;
    let (words, _) = said.as_chunks::<{ size_of::<u32>() }>();
    let mut parts = words.iter().map(|w| u32::from_le_bytes(*w)).map(|v| (v != UNSAID).then_some(u64::from(v)));
    let (arrived, took, runner) = (parts.next().flatten(), parts.next().flatten()?, parts.next().flatten());
    Some(Came { arrived_us: arrived.map(|a| published.saturating_sub(a)), took_us: published.saturating_sub(took), runner_us: runner })
}

/// A frame written straight into its bus payload, in [`encode`]'s form: its text, then its patch.
/// Nothing of it is copied to send it; its publish time is said as it goes ([`Feed::publish_payload`]).
pub struct Payload {
    bytes: Vec<u8>,
    /// Its text is written: what follows is its patch.
    patching: bool,
}

impl Payload {
    /// A frame of the copy's revision `rev`, about `room` bytes.
    pub fn new(rev: u64, room: usize) -> Self {
        let mut bytes = Vec::with_capacity(HEAD.saturating_add(room));
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&rev.to_le_bytes());
        bytes.extend_from_slice(&[0u8; CAME_BYTES]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Self { bytes, patching: false }
    }

    /// Where its text is written (before its patch).
    pub fn text(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Where its patch is written: its text is done.
    pub fn patch(&mut self) -> &mut Vec<u8> {
        self.done();
        &mut self.bytes
    }

    /// Its text is done (when it has no patch): its length is said.
    pub fn done(&mut self) {
        if std::mem::replace(&mut self.patching, true) {
            return;
        }
        let len = u32::try_from(self.bytes.len().saturating_sub(HEAD)).unwrap_or(u32::MAX).to_le_bytes();
        if let Some(at) = self.bytes.get_mut(HEAD - size_of::<u32>()..HEAD) {
            at.copy_from_slice(&len);
        }
    }

    /// The payload, said published now, with where its update came from ([`came`]).
    fn stamped(mut self) -> Vec<u8> {
        self.done();
        let now = now_us();
        if let Some(at) = self.bytes.get_mut(..8) {
            at.copy_from_slice(&now.to_le_bytes());
        }
        if let Some(at) = self.bytes.get_mut(CAME_AT..CAME_AT + CAME_BYTES) {
            at.copy_from_slice(&came_bytes(now));
        }
        self.bytes
    }
}

/// A frame as the bus carries it: when it was published ([`now_us`], 8 bytes, little-endian), the
/// copy's revision it carries (8 bytes, little-endian; docs/wire.md, "Revisions"), where the update it
/// was made of came from ([`CAME_BYTES`], little-endian), its whole text's length (4 bytes,
/// little-endian), the text, then what changed in a state when there's a patch (the rest). The topic is
/// this process's alone, so the form is free to change.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let patch = frame.patch.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(frame.text.len().saturating_add(patch.len()).saturating_add(HEAD));
    let now = now_us();
    out.extend_from_slice(&now.to_le_bytes());
    out.extend_from_slice(&frame.rev.to_le_bytes());
    out.extend_from_slice(&came_bytes(now));
    out.extend_from_slice(&u32::try_from(frame.text.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(frame.text.as_bytes());
    out.extend_from_slice(patch.as_bytes());
    out
}

/// When the frame in `bytes` was published ([`now_us`]); `None` for bytes [`encode`] didn't make.
pub fn published_us(bytes: &[u8]) -> Option<u64> {
    bytes.first_chunk::<8>().map(|at| u64::from_le_bytes(*at))
}

/// A frame as [`decode_ref`] reads it: its text and patch where they are in the payload.
pub struct FrameRef<'a> {
    pub text: &'a str,
    pub patch: &'a str,
    pub rev: u64,
}

impl FrameRef<'_> {
    /// The frame for a viewer that takes the patches `takes` says: its patch when it has one of a
    /// kind the viewer takes, else its text (empty when it went as its patch alone, which a viewer
    /// that takes it whole is never sent: [`Feed::wants_whole`]).
    pub fn for_viewer(&self, takes: Patches) -> &str {
        if !self.patch.is_empty() && takes.takes(PatchOf::of(self.patch)) { self.patch } else { self.text }
    }

    /// Where in its payload the frame for a viewer that takes `takes` is ([`FrameRef::for_viewer`]).
    pub fn range_for_viewer(&self, takes: Patches) -> std::ops::Range<usize> {
        let text = HEAD..HEAD + self.text.len();
        if !self.patch.is_empty() && takes.takes(PatchOf::of(self.patch)) { text.end..text.end + self.patch.len() } else { text }
    }
}

/// The text at `range` of `payload` (a frame [`decode_ref`] read), as a string of its own made of
/// the payload's bytes, moved: a viewer's frame goes to its socket without another copy of it.
pub fn into_text(mut payload: Vec<u8>, range: std::ops::Range<usize>) -> String {
    payload.truncate(range.end);
    payload.drain(..range.start.min(payload.len()));
    String::from_utf8(payload).unwrap_or_default()
}

/// A frame from what [`encode`] made, read where it is (nothing copied); `None` for bytes it
/// didn't make.
pub fn decode_ref(bytes: &[u8]) -> Option<FrameRef<'_>> {
    let (_, rest) = bytes.split_first_chunk::<8>()?;
    let (rev, rest) = rest.split_first_chunk::<8>()?;
    let (_, rest) = rest.split_first_chunk::<CAME_BYTES>()?;
    let (len, rest) = rest.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    let text = std::str::from_utf8(rest.get(..len)?).ok()?;
    let patch = std::str::from_utf8(rest.get(len..)?).ok()?;
    Some(FrameRef { text, patch, rev: u64::from_le_bytes(*rev) })
}

/// A frame from what [`encode`] made; `None` for bytes it didn't make.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let (_, rest) = bytes.split_first_chunk::<8>()?;
    let (rev, rest) = rest.split_first_chunk::<8>()?;
    let (_, rest) = rest.split_first_chunk::<CAME_BYTES>()?;
    let (len, rest) = rest.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    let text = std::str::from_utf8(rest.get(..len)?).ok()?;
    let patch = std::str::from_utf8(rest.get(len..)?).ok()?;
    Some(Frame { text: Arc::from(text), patch: (!patch.is_empty()).then(|| Arc::from(patch)), patch_of: super::registry::PatchOf::of(patch), rev: u64::from_le_bytes(*rev) })
}

/// How a feed's publish went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// On the bus.
    Sent,
    /// No viewer watches: nothing was published.
    Unwatched,
    /// The bus refused it: the feed is marked broken, and its viewers start over at its next frame.
    Refused,
}

/// A feed's word ([`Shared::word`]): the viewers in its generation now, in its low 31 bits.
const VIEWERS: u64 = (1 << 31) - 1;
/// A viewer joined the generation: its topic may be made.
const JOINED: u64 = 1 << 31;
/// Where its generation begins: its high 32 bits.
const GENERATION: u32 = 32;

fn generation_of(word: u64) -> u32 {
    u32::try_from(word >> GENERATION).unwrap_or(u32::MAX)
}

/// What a feed shares with its viewers.
struct Shared {
    bus: Arc<dyn Bus>,
    /// Its session's id and the feed's number: what its topics are named after ([`topic`]).
    id: String,
    n: u64,
    /// Its generation (the high 32 bits), whether a viewer joined it ([`JOINED`]) and the viewers
    /// in it now ([`VIEWERS`]), in one word. The feed lets a generation's topic go only while no
    /// viewer is in it, swapping in the next generation as it does, so a viewer never reads a topic
    /// let go: one that joins after reads the next generation's.
    word: AtomicU64,
    /// Of them, those that take a state whole, and a changeset whole (not as patches): with none, a
    /// state or a changeset goes as its patch alone ([`Feed::wants_whole`]).
    whole_state: AtomicUsize,
    whole_changes: AtomicUsize,
    /// Frames and topics the bus refused the feed: a viewer whose feed was refused since it last
    /// started over starts over again, whether or not a resync marker ever reaches it (a full bus
    /// may refuse that too).
    refusals: AtomicU64,
    /// Told as `refusals` moves: a viewer waits on it beside its feed, so it learns its feed broke as it breaks (goal
    /// 9, plan 01a1a87b2cec step 5: it looked after every four quiet seconds).
    refused: crate::rt::Changed,
    /// The session ended (its feed dropped): the last of its viewers to go deletes the topic.
    ended: AtomicBool,
    /// The ended feed's topic was deleted: by whichever of the feed and its last viewer came first.
    deleted: AtomicBool,
}

impl Shared {
    fn topic(&self, generation: u32) -> String {
        topic(&self.id, self.n, generation)
    }

    /// Makes generation `generation`'s topic, which the bus makes once however often it's asked (a
    /// topic made again with the same settings is the one made). A refusal is counted.
    fn make(&self, generation: u32) -> Result<(), Refusal> {
        at_once(self.bus.create(&self.topic(generation), Topic { class: Class::Ephemeral, retain: retain() })).inspect_err(|_| {
            self.refuse();
            crate::metrics::bus().refused.inc(&["create"]);
        })
    }

    /// A refusal counted, and the viewers told.
    fn refuse(&self) {
        self.refusals.fetch_add(1, Ordering::AcqRel);
        self.refused.notify();
    }

    /// Deletes the ended feed's topic (of its last generation, `generation`), once.
    fn delete_last(&self, generation: u32) {
        if !self.deleted.swap(true, Ordering::AcqRel) {
            delete(self.bus.as_ref(), &self.topic(generation));
        }
    }
}

fn delete(bus: &dyn Bus, topic: &str) {
    match at_once(bus.delete(topic)) {
        // None to delete (no viewer made it, or another deleted it first): what deleting it does.
        Ok(()) | Err(Refusal::NoTopic(_)) => {}
        Err(e) => {
            crate::metrics::bus().refused.inc(&["delete"]);
            // No viewer loses anything (none watches, or the session ended): info, where a viewer
            // sent back to a snapshot warns.
            tracing::info!(target: "kiln.bus", event = "feed.refused", topic = %topic, error = %e, "a session's live feed couldn't be let go");
        }
    }
}

/// What a feed's readers hold of it: viewers join through it, its owner never asked (goal 4, plan
/// 01a1a59c2a9c step 3).
#[derive(Clone)]
pub struct Tap(Arc<Shared>);

impl Tap {
    /// A viewer joins, taking what `takes` says as patches: counted in the feed's generation now,
    /// whose topic isn't let go while it's there.
    pub fn join(&self, takes: Patches) -> Viewer {
        Viewer::new(&self.0, takes)
    }

    /// The feed's refusals so far (what a viewer starting over now compares against later).
    pub fn refusals(&self) -> u64 {
        self.0.refusals.load(Ordering::Acquire)
    }
}

/// One viewer's place in a feed's count (and in `kiln_bus_viewers`), in the generation it joined,
/// whose topic it reads; dropping it gives the place back, and the last viewer of an ended session
/// takes its topic with it. It reads the feed's refusals, so it can tell its feed broke without a
/// frame saying so.
pub struct Viewer {
    shared: Arc<Shared>,
    takes: Patches,
    generation: u32,
    topic: String,
    /// Its topic is made (by it, or by the feed).
    made: AtomicBool,
}

impl Viewer {
    fn new(shared: &Arc<Shared>, takes: Patches) -> Self {
        // Counted whole before it's counted at all: a publish that sees it sees what it takes.
        if !takes.state {
            shared.whole_state.fetch_add(1, Ordering::AcqRel);
        }
        if !takes.changes {
            shared.whole_changes.fetch_add(1, Ordering::AcqRel);
        }
        // Counted in the generation it joins (SeqCst: the feed's `watched` and this see each other).
        let mut word = shared.word.load(Ordering::SeqCst);
        while let Err(now) = shared.word.compare_exchange_weak(word, word.wrapping_add(1) | JOINED, Ordering::SeqCst, Ordering::SeqCst) {
            word = now;
        }
        crate::metrics::bus_gauges().viewers.add(1);
        let generation = generation_of(word);
        Self { shared: Arc::clone(shared), takes, generation, topic: shared.topic(generation), made: AtomicBool::new(false) }
    }

    /// The topic it reads: its generation's.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Where it reads on from if it starts over now: the end of its generation's topic, made first
    /// (none while the bus can't hold it). Taken before the copy's view a snapshot is made of, so
    /// every frame after that view is past it (`super::registry::Registry::attach_from`).
    pub fn position(&self) -> Option<Offset> {
        if !self.made.load(Ordering::Acquire) {
            self.shared.make(self.generation).ok()?;
            self.made.store(true, Ordering::Release);
        }
        at_once(self.shared.bus.bounds(&self.topic)).ok().map(|(_, end)| end)
    }

    /// The feed's refusals so far: more than when the viewer last started over means the feed
    /// broke since.
    pub fn refusals(&self) -> u64 {
        self.shared.refusals.load(Ordering::Acquire)
    }

    /// A wait for the feed's next refusal, begun now: listened for before [`Viewer::refusals`] is read.
    pub fn refused(&self) -> crate::rt::Listen<'_> {
        self.shared.refused.listen()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // Its place, taken once as it was made, given back once.
        crate::metrics::bus_gauges().viewers.add(-1);
        if !self.takes.state {
            self.shared.whole_state.fetch_sub(1, Ordering::AcqRel);
        }
        if !self.takes.changes {
            self.shared.whole_changes.fetch_sub(1, Ordering::AcqRel);
        }
        if self.shared.word.fetch_sub(1, Ordering::SeqCst) & VIEWERS == 1 && self.shared.ended.load(Ordering::SeqCst) {
            self.shared.delete_last(self.generation);
        }
    }
}

/// The bus answers a feed at once: Quiver's ephemeral class never waits, nor does the simulator.
/// An answer that would wait is a refusal here (counted, and the viewers resynced), never a frame
/// dropped silently.
fn at_once<T>(f: futures::future::BoxFuture<'_, Result<T, Refusal>>) -> Result<T, Refusal> {
    f.now_or_never().unwrap_or_else(|| Err(Refusal::Unavailable("the bus didn't answer at once".to_owned())))
}

/// One session's live feed, as its copy's owner holds it (`super::copies`). Dropped (its copy let go:
/// the session ended or went away), its topic goes once its viewers have: each reads on to the
/// session's last frame ([`closed`]) and ends with it, so none misses the frames it hadn't read yet.
pub struct Feed {
    shared: Arc<Shared>,
    /// The generation whose topic it made: what it publishes to while that generation lasts.
    made: Option<u32>,
    /// That generation's topic, named once as it was made (goal 9, plan 01a1a87b2cec step 5: each frame named it
    /// again, a string made and let go for every publish).
    made_topic: String,
    /// A frame was refused: the next publish goes after [`RESYNC`]. Set too while the topic
    /// couldn't be made (it's asked again at the next publish).
    broken: bool,
    /// What the topic it made keeps by its retention (goal 11, plan 01a1a87b2e56 step 3).
    kept: Kept,
}

impl Feed {
    /// A feed for session `id` on `bus`: its topic is made as a viewer joins.
    pub fn open(bus: Arc<dyn Bus>, id: &str) -> Self {
        let shared = Shared {
            bus,
            id: id.to_owned(),
            n: FEEDS.fetch_add(1, Ordering::Relaxed),
            word: AtomicU64::new(0),
            whole_state: AtomicUsize::new(0),
            whole_changes: AtomicUsize::new(0),
            refusals: AtomicU64::new(0),
            refused: crate::rt::Changed::new(),
            ended: AtomicBool::new(false),
            deleted: AtomicBool::new(false),
        };
        Self { shared: Arc::new(shared), made: None, made_topic: String::new(), broken: false, kept: Kept::default() }
    }

    /// What its readers hold of it: viewers join through it.
    pub fn tap(&self) -> Tap {
        Tap(Arc::clone(&self.shared))
    }

    /// The topic a viewer joining now reads: its generation's.
    pub fn topic(&self) -> String {
        self.shared.topic(generation_of(self.shared.word.load(Ordering::Acquire)))
    }

    /// Viewers watching now.
    pub fn viewers(&self) -> usize {
        usize::try_from(self.shared.word.load(Ordering::Acquire) & VIEWERS).unwrap_or(usize::MAX)
    }

    /// Whether a viewer watching now takes `of` whole (one from before patches, an API client):
    /// asked once [`Feed::watched`] has seen it, which a viewer is counted whole before.
    pub fn wants_whole(&self, of: PatchOf) -> bool {
        match of {
            PatchOf::State => self.shared.whole_state.load(Ordering::Acquire) > 0,
            PatchOf::Changes => self.shared.whole_changes.load(Ordering::Acquire) > 0,
        }
    }

    /// The feed's refusals so far (what a viewer starting over now compares against later).
    pub fn refusals(&self) -> u64 {
        self.shared.refusals.load(Ordering::Acquire)
    }

    /// A viewer's place, and where it reads from (tests, and a feed outside a session's copy).
    pub fn attach(&self, takes: Patches) -> (Viewer, Option<Offset>) {
        let viewer = self.tap().join(takes);
        let at = viewer.position();
        (viewer, at)
    }

    /// Whether anyone watches now: the generation they read, when one does. Asked after the copy's
    /// newest view is out (`super::copies::Mirror::commit`), and a viewer is counted before it reads
    /// the view: one or the other sees the other (both SeqCst, a Dekker handshake), so a viewer has
    /// every change in its view or in a frame. None watching, what the generation's topic kept goes
    /// with it (the next viewer joins the next generation): a session nobody watches holds none of
    /// the bus's memory.
    pub fn watched(&mut self) -> Option<u32> {
        std::sync::atomic::fence(Ordering::SeqCst);
        let mut word = self.shared.word.load(Ordering::SeqCst);
        loop {
            let generation = generation_of(word);
            if word & VIEWERS > 0 {
                return Some(generation);
            }
            // No topic of this generation to let go: none joined it, and the feed made none.
            if word & JOINED == 0 && self.made != Some(generation) {
                return None;
            }
            let next = u64::from(generation.wrapping_add(1)) << GENERATION;
            match self.shared.word.compare_exchange(word, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    delete(self.shared.bus.as_ref(), &self.shared.topic(generation));
                    self.made = None;
                    self.broken = false;
                    self.kept = Kept::default();
                    return None;
                }
                // A viewer joined meanwhile: it's watched after all.
                Err(now) => word = now,
            }
        }
    }

    /// Publishes `frame` to its viewers: none watching, nothing goes ([`Feed::watched`]).
    pub fn publish(&mut self, frame: &Frame) -> Published {
        match self.watched() {
            Some(generation) => self.send(generation, encode(frame)),
            None => Published::Unwatched,
        }
    }

    /// Publishes a frame written straight into its payload ([`Payload`]), as [`Feed::publish`] does.
    pub fn publish_payload(&mut self, p: Payload) -> Published {
        match self.watched() {
            Some(generation) => self.send(generation, p.stamped()),
            None => Published::Unwatched,
        }
    }

    /// Publishes a frame written straight into its payload to generation `generation`'s viewers,
    /// as [`Feed::watched`] said they watch.
    pub fn send_payload(&mut self, generation: u32, p: Payload) -> Published {
        self.send(generation, p.stamped())
    }

    fn send(&mut self, generation: u32, payload: Vec<u8>) -> Published {
        if self.made != Some(generation) {
            if let Err(e) = self.shared.make(generation) {
                // Said once as the feed breaks; asked again at each frame, each refusal counted.
                if !self.broken {
                    tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.shared.topic(generation), error = %e, "a session's live feed couldn't be made: its viewers get snapshots until it is");
                }
                self.broken = true;
                return Published::Refused;
            }
            self.made = Some(generation);
            self.made_topic = self.shared.topic(generation);
            // A topic of its own: none of the last one's frames.
            self.kept = Kept::new(retain());
        }
        let m = crate::metrics::bus();
        if self.broken {
            let resync = encode(&Frame::resync());
            let n = resync.len();
            match at_once(self.shared.bus.publish(&self.made_topic, None, resync)) {
                Ok(_) => {
                    self.broken = false;
                    m.published.inc();
                    self.kept_frame(n);
                }
                Err(e) => {
                    let topic = self.made_topic.clone();
                    return self.refused(&topic, &e);
                }
            }
        }
        let n = payload.len();
        match at_once(self.shared.bus.publish(&self.made_topic, None, payload)) {
            Ok(_) => {
                m.published.inc();
                self.kept_frame(n);
                Published::Sent
            }
            Err(e) => {
                let topic = self.made_topic.clone();
                self.refused(&topic, &e)
            }
        }
    }

    /// A frame of `n` bytes went on its topic: counted in what the topic keeps by its retention, and
    /// the most any topic has (`kiln_bus_topic_bytes_peak`) raised to it.
    fn kept_frame(&mut self, n: usize) {
        let bytes = self.kept.add(n);
        crate::metrics::bus_gauges().topic_bytes_peak.raise(i64::try_from(bytes).unwrap_or(i64::MAX));
    }

    fn refused(&mut self, topic: &str, e: &Refusal) -> Published {
        self.broken = true;
        self.shared.refuse();
        crate::metrics::bus().refused.inc(&["publish"]);
        tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %topic, error = %e, "a frame the bus refused: the session's viewers start over at its next one");
        Published::Refused
    }
}

impl Drop for Feed {
    /// The session ended: its topic goes now if no viewer reads it, else with its last viewer.
    fn drop(&mut self) {
        self.shared.ended.store(true, Ordering::SeqCst);
        let word = self.shared.word.load(Ordering::SeqCst);
        if word & VIEWERS == 0 && (word & JOINED != 0 || self.made.is_some()) {
            self.shared.delete_last(generation_of(word));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> anyhow::Result<Arc<dyn Bus>> {
        Ok(Arc::new(crate::bus::QuiverBus::open(&crate::bus::Config::default(), Arc::new(kiln_seams::clock::SimClock::new()))?))
    }

    /// A frame goes on the bus and comes back as it went, its patch (and what that's a patch of) and
    /// its revision with it; bytes it didn't make come back as none.
    #[test]
    fn a_frame_comes_back_from_the_bus_as_it_went() {
        use super::super::registry::PatchOf;
        let frames = [
            Frame::text(r#"{"t":"events","events":[]}"#),
            Frame { text: Arc::from(r#"{"t":"state","state":{"a":1},"rev":42}"#), patch: Some(Arc::from(r#"{"t":"state_patch","set":{"a":1},"rev":42}"#)), patch_of: PatchOf::State, rev: 42 },
            Frame { text: Arc::from(r#"{"t":"changes","files":[],"rev":43}"#), patch: Some(Arc::from(r#"{"t":"changes_patch","gone":[{"path":"a"}],"rev":43}"#)), patch_of: PatchOf::Changes, rev: 43 },
        ];
        for f in frames {
            let back = decode(&encode(&f)).map(|b| (b.text.to_string(), b.patch.as_deref().map(str::to_owned), b.patch.is_some().then_some(b.patch_of), b.rev));
            assert_eq!(back, Some((f.text.to_string(), f.patch.as_deref().map(str::to_owned), f.patch.is_some().then_some(f.patch_of), f.rev)));
        }
        assert!(decode(b"\x09\x00").is_none());
        assert!(decode(b"\x00\x00\x00\x00\x00\x00\x00\x00\x05\x00\x00\x00abc").is_none());
        assert!(decode(b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x05\x00\x00\x00abc").is_none());
    }

    /// A frame carries when it was published, on this process's microsecond clock: what the
    /// feed's delivery is timed from.
    #[test]
    fn a_frame_carries_the_microsecond_it_was_published() {
        let before = now_us();
        let bytes = encode(&Frame::text(r#"{"t":"events","events":[]}"#));
        let after = now_us();
        let at = published_us(&bytes);
        assert!(at.is_some_and(|at| (before..=after).contains(&at)), "{at:?} not in {before}..={after}");
        assert_eq!(published_us(b"\x01\x02"), None);
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a frame published while its owner takes a runner's update carries where that came
    /// from (its arrival, its owner's take, its runner's hold), on the publish's clock, written straight or into its payload;
    /// one published after says none, the frame itself the same either way.
    #[test]
    fn g10_s5_a_frame_carries_where_its_update_came_from() {
        // Stamps 1,500 µs before now, which the clock can say once it's that old (it starts at its first reading).
        const BEFORE: u64 = 1_500;
        while now_us() <= BEFORE {
            std::thread::sleep(std::time::Duration::from_micros(BEFORE));
        }
        let now = now_us();
        let said = Came { arrived_us: Some(now - BEFORE), took_us: now - 400, runner_us: Some(65) };
        let text = r#"{"t":"events","events":[]}"#;
        let (written, made) = {
            let _came = carrying(Some(said));
            let mut p = Payload::new(7, text.len());
            p.text().extend_from_slice(text.as_bytes());
            (encode(&Frame::text(text)), p.stamped())
        };
        for bytes in [&written, &made] {
            assert_eq!(came_of(bytes), Some(said));
            assert_eq!(decode_ref(bytes).map(|f| f.text), Some(text));
        }
        // An update without an arrival or a runner's word: its take alone.
        let took = Came { arrived_us: None, took_us: now, runner_us: None };
        let bytes = {
            let _came = carrying(Some(took));
            encode(&Frame::text(text))
        };
        assert_eq!(came_of(&bytes), Some(took));
        // After: none.
        assert_eq!(came(), None);
        let after = encode(&Frame::text(text));
        assert_eq!(came_of(&after), None);
        assert_eq!(decode(&after).map(|f| f.text.to_string()), Some(text.to_owned()));
    }

    /// Topic names: a session's id where it fits a name, its SHA-256 where it doesn't (another
    /// alphabet, or too long with its feed's number and generation); the bus takes both, and each
    /// feed of a session has its own.
    #[test]
    fn every_feed_has_a_topic_of_its_own_the_bus_takes() -> anyhow::Result<()> {
        assert_eq!(topic("0192f0c5d4a87b3e9f1a2b3c4d5e6f70", 7, 2), "sessions/0192f0c5d4a87b3e9f1a2b3c4d5e6f70/7.2");
        let odd = topic("an id with spaces / and ünïcode", u64::MAX, u32::MAX);
        assert!(odd.starts_with("sessions/sha256-") && odd.len() <= kiln_seams::bus::NAME_MAX, "{odd}");
        let long = topic(&"a".repeat(96), u64::MAX, u32::MAX);
        assert!(long.starts_with("sessions/sha256-") && long.len() <= kiln_seams::bus::NAME_MAX, "{long}");
        let b = bus()?;
        for name in [topic("plain-id", 0, 0), odd, long] {
            futures::executor::block_on(b.create(&name, Topic { class: Class::Ephemeral, retain: RETAIN }))?;
        }
        let (one, two) = (Feed::open(Arc::clone(&b), "s1"), Feed::open(Arc::clone(&b), "s1"));
        assert_ne!(one.topic(), two.topic());
        Ok(())
    }

    /// No viewer, nothing published; a viewer reads from the offset it attached at; a frame the
    /// bus refuses marks the feed broken, and the next goes after the resync marker; dropped, the
    /// feed's topic goes.
    #[test]
    fn a_feed_publishes_only_to_viewers_and_a_refused_frame_sends_them_back_to_a_snapshot() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        assert_eq!(feed.publish(&Frame::text("unwatched")), Published::Unwatched);
        let (viewer, at) = feed.attach(Patches::default());
        assert_eq!(at, Some(0));
        assert_eq!(feed.publish(&Frame::text("first")), Published::Sent);
        let huge = Frame::text(&"x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert_eq!(feed.publish(&Frame::text("after")), Published::Sent);
        let topic = viewer.topic().to_owned();
        let read = futures::executor::block_on(b.read(&topic, 0, 10))?;
        let texts: Vec<String> = read.messages.iter().map(|m| decode(&m.payload).map(|f| f.text.to_string()).unwrap_or_default()).collect();
        assert_eq!(texts, ["first", RESYNC, "after"]);
        drop(viewer);
        assert_eq!(feed.viewers(), 0);
        drop(feed);
        assert_eq!(futures::executor::block_on(b.bounds(&topic)), Err(Refusal::NoTopic(topic)));
        Ok(())
    }
    /// Goal 11, plan 01a1a87b2e56 step 3: what a topic keeps by its retention is its newest frames'
    /// bytes, as many as it retains: past that its oldest go from the sum.
    #[test]
    fn g11_s3_a_topics_bytes_are_its_newest_frames() {
        let mut kept = Kept::new(3);
        assert_eq!([10, 20, 30].map(|n| kept.add(n)), [10, 30, 60]);
        assert_eq!(kept.add(40), 90, "the oldest went");
        assert_eq!(kept.add(5), 75);
        assert_eq!(kept.sizes.len(), 3);
        let mut one = Kept::new(0);
        assert_eq!((one.add(7), one.add(9)), (7, 9), "a topic keeps its newest frame at least");
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a feed counts what its topic keeps as the bus counts it (each
    /// frame's payload, the resync marker too), raises `kiln_bus_topic_bytes_peak` to it, and starts
    /// from nothing with each topic: one let go while nobody watches takes its count with it.
    #[test]
    fn g11_s3_a_feed_counts_what_its_topic_keeps() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (viewer, _) = feed.attach(Patches::default());
        let frames = [Frame::text("first"), Frame::text(&"x".repeat(5000))];
        for f in &frames {
            anyhow::ensure!(feed.publish(f) == Published::Sent);
        }
        let sent: u64 = frames.iter().map(|f| encode(f).len() as u64).sum();
        anyhow::ensure!(feed.kept.bytes == sent, "counted {} of the {sent} bytes published", feed.kept.bytes);
        // What the bus keeps of the topic, as it counts its retention: the same bytes.
        let read = futures::executor::block_on(b.read(viewer.topic(), 0, 10))?;
        let kept: u64 = read.messages.iter().map(|m| m.payload.len() as u64).sum();
        anyhow::ensure!(kept == feed.kept.bytes, "the bus keeps {kept} bytes, the feed counted {}", feed.kept.bytes);
        anyhow::ensure!(crate::metrics::bus_gauges().topic_bytes_peak.get() >= i64::try_from(sent)?, "the peak on /metrics is at least this topic's");
        // A refused frame isn't kept; the resync marker after it is.
        let huge = Frame::text(&"x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
        anyhow::ensure!(feed.publish(&huge) == Published::Refused);
        anyhow::ensure!(feed.kept.bytes == sent);
        anyhow::ensure!(feed.publish(&Frame::text("after")) == Published::Sent);
        let after = (encode(&Frame::resync()).len() + encode(&Frame::text("after")).len()) as u64;
        anyhow::ensure!(feed.kept.bytes == sent + after, "the marker and the frame after it: {}", feed.kept.bytes);
        drop(viewer);
        anyhow::ensure!(feed.publish(&Frame::text("unseen")) == Published::Unwatched);
        anyhow::ensure!(feed.kept.bytes == 0 && feed.kept.sizes.capacity() == 0, "a topic let go takes its count with it");
        Ok(())
    }

    /// A feed nobody watches keeps none of the bus's memory: its next frame lets the topic go,
    /// and the next viewer joins the next generation, its topic made from its start.
    #[test]
    fn a_feed_nobody_watches_lets_its_topic_go_and_a_viewer_makes_it_again() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (viewer, _) = feed.attach(Patches::default());
        let first = viewer.topic().to_owned();
        assert_eq!(feed.publish(&Frame::text("seen")), Published::Sent);
        drop(viewer);
        assert_eq!(feed.publish(&Frame::text("unseen")), Published::Unwatched);
        assert_eq!(futures::executor::block_on(b.bounds(&first)), Err(Refusal::NoTopic(first.clone())));
        let (again, at) = feed.attach(Patches::default());
        assert_ne!(again.topic(), first, "the next viewer reads a generation of its own");
        assert_eq!(at, Some(0));
        assert_eq!(feed.publish(&Frame::text("again")), Published::Sent);
        let read = futures::executor::block_on(b.read(again.topic(), 0, 10))?;
        let texts: Vec<String> = read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()).collect();
        assert_eq!(texts, ["again"]);
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 3: viewers join and leave on threads of their own while the
    /// feed's owner publishes, letting each generation's topic go once nobody is in it. None finds
    /// the topic it joined let go while it's there, and each reads what was published after it took
    /// its place, in order.
    #[test]
    fn viewers_joining_without_the_owner_never_read_a_topic_let_go() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let tap = feed.tap();
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let (tap, b, stop) = (tap.clone(), Arc::clone(&b), Arc::clone(&stop));
                std::thread::spawn(move || -> Result<u64, String> {
                    let mut joins = 0;
                    while !stop.load(Ordering::Acquire) {
                        let viewer = tap.join(Patches::default());
                        let Some(mut at) = viewer.position() else { return Err("no topic made".into()) };
                        let mut last: Option<u64> = None;
                        for _ in 0..16 {
                            let read = match futures::executor::block_on(b.read(viewer.topic(), at, 64)) {
                                Ok(read) => read,
                                // Further behind than the topic keeps: on from its oldest, as a viewer
                                // that caught up reads on.
                                Err(Refusal::Behind { first, .. }) => {
                                    at = first;
                                    continue;
                                }
                                Err(e) => return Err(format!("{e}")),
                            };
                            for m in &read.messages {
                                let f = decode(&m.payload).ok_or("a frame the feed didn't make")?;
                                if &*f.text == RESYNC {
                                    continue;
                                }
                                let n: u64 = f.text.parse().map_err(|_| format!("a frame not the owner's: {}", f.text))?;
                                if last.is_some_and(|l| n <= l) {
                                    return Err(format!("{n} after {last:?}"));
                                }
                                last = Some(n);
                            }
                            at = read.next;
                        }
                        joins += 1;
                        drop(viewer);
                    }
                    Ok(joins)
                })
            })
            .collect();
        for n in 0..20_000u64 {
            feed.publish(&Frame::text(&n.to_string()));
        }
        stop.store(true, Ordering::Release);
        for r in readers {
            let joins = r.join().map_err(|_| anyhow::anyhow!("a reader panicked"))?.map_err(anyhow::Error::msg)?;
            anyhow::ensure!(joins > 0, "a reader never joined");
        }
        drop(feed);
        Ok(())
    }

    /// A viewer learns its feed broke from the feed's refusals, without a frame saying so: a full
    /// bus may refuse the resync marker as well as the frame, and the viewer must not wait on.
    #[test]
    fn a_viewer_learns_its_feed_broke_without_a_frame_saying_so() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (viewer, _) = feed.attach(Patches::default());
        let before = viewer.refusals();
        assert_eq!(feed.refusals(), before);
        let huge = Frame::text(&"x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert!(viewer.refusals() > before, "the viewer sees the refusals");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (train 34's census: a viewer of a quiet session looked whether its feed broke
    /// every four seconds, about 0.3 timer wakes a second a viewer): a refusal wakes a viewer's wait begun before it,
    /// and nothing else does. Do: a viewer's wait on its feed's refusals; looked at with none; a frame refused.
    /// Expect: pending; then over.
    #[test]
    fn g9_s5_a_refusal_wakes_a_viewer_waiting_on_its_feed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let b = bus()?;
            let mut feed = Feed::open(Arc::clone(&b), "s1");
            let (viewer, _) = feed.attach(Patches::default());
            let mut wait = std::pin::pin!(viewer.refused());
            anyhow::ensure!(futures::poll!(wait.as_mut()).is_pending(), "no refusal: still waiting");
            let huge = Frame::text(&"x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
            anyhow::ensure!(feed.publish(&huge) == Published::Refused, "refused");
            // Awaited, not polled once: a wait's poll charges its task's turn first (`hyper_rt::coop`), and the turn the
            // publish ran in may have none left, so that one poll yields. The bound only ends a hang.
            crate::rt::timeout(std::time::Duration::from_secs(300), wait).await.map_err(|_| anyhow::anyhow!("the refusal never ended the wait"))?;
            Ok(())
        })
    }

    /// An ended session's topic stays while a viewer reads it, so the frames it hadn't read yet,
    /// the session's last ([`closed`]) among them, still reach it; it goes with the last viewer.
    #[test]
    fn an_ended_sessions_topic_stays_until_its_last_viewer_goes() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (first, at) = feed.attach(Patches::default());
        let (second, _) = feed.attach(Patches::default());
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        assert_eq!(feed.publish(&Frame::text("last words")), Published::Sent);
        assert_eq!(feed.publish(&Frame::text(closed())), Published::Sent);
        let topic = feed.topic();
        drop(feed);
        let read = futures::executor::block_on(b.read(&topic, at, 10))?;
        let texts: Vec<String> = read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()).collect();
        assert_eq!(texts, ["last words", closed()]);
        drop(first);
        assert!(futures::executor::block_on(b.bounds(&topic)).is_ok(), "one viewer still reads");
        drop(second);
        assert_eq!(futures::executor::block_on(b.bounds(&topic)), Err(Refusal::NoTopic(topic)));
        Ok(())
    }

    /// CP-T2.1, AC-T2.1.5 in kiln: a session's feed with 100 viewers, one stalled. Every frame is
    /// published at once; the 99 that read as frames come take all 3000 (more than the feed keeps)
    /// in order; the stalled one, reading at last from where it attached, is refused `Behind` (the
    /// typed lag notice that sends a viewer back to a snapshot), naming where the feed starts.
    #[test]
    fn a_stalled_viewer_among_100_is_told_it_fell_behind_and_delays_no_other() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let mut viewers = Vec::new();
        for _ in 0..100 {
            let (viewer, at) = feed.attach(Patches::default());
            viewers.push((viewer, at.ok_or_else(|| anyhow::anyhow!("no feed"))?, Vec::<String>::new()));
        }
        let frames = 3000;
        for n in 0..frames {
            assert_eq!(feed.publish(&Frame::text(&format!("frame {n}"))), Published::Sent, "frame {n} went at once");
            // The 99: each reads what's there, as its socket's loop does.
            for (_, at, got) in viewers.iter_mut().skip(1) {
                let read = futures::executor::block_on(b.read(&feed.topic(), *at, 1000))?;
                got.extend(read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()));
                *at = read.next;
            }
        }
        let want: Vec<String> = (0..frames).map(|n| format!("frame {n}")).collect();
        for (i, (_, _, got)) in viewers.iter().enumerate().skip(1) {
            assert_eq!(got, &want, "viewer {i} keeping up");
        }
        let (_, stalled_at, _) = viewers.first().ok_or_else(|| anyhow::anyhow!("no viewers"))?;
        let refused = futures::executor::block_on(b.read(&feed.topic(), *stalled_at, 1000));
        let first = u64::try_from(frames - RETAIN)?;
        assert_eq!(refused, Err(Refusal::Behind { asked: 0, first }));
        Ok(())
    }
}
