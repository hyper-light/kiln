//! Each agent session's copy on the server, owned by one shard (goal 4, plan 01a1a59c2a9c; plan
//! 01a17197 step 9). A session belongs to the shard its id hashes to ([`owner_of`]). That shard's
//! owner task holds the session's copy ([`Mirror`]) by value and is the only one that changes it:
//! runners' updates and people's commands come to it as messages, from a bounded inbox, in the order
//! they were sent. After each change it publishes the copy as an immutable view ([`View`]), swapped
//! in whole; every reader (a viewer's snapshot, a catch-up, a list, a command's checks) loads the
//! newest view without a lock and never waits on the owner. No lock guards a session's copy
//! (`no_lock_guards_session_copies` holds this module, `super::live` and the registry to it).
//!
//! An owner's inbox holds as many bytes as the hub's queue of runner events
//! ([`crate::net::EVENTS_BYTES`]; goal 11, plan 01a1a87b2e56 step 3: its messages counted by what they
//! carry, [`crate::memory::queue`], its places made as it fills). A sender it has no room for waits,
//! asynchronously (its shard goes on with its other tasks), until the owner takes enough: nothing is
//! dropped and the order holds.
//! The owner waits on nothing but its inbox, so it always drains it. Behind a full inbox the hub's
//! loop waits, the net thread holds what it can't hand over, QUIC's flow control holds the runner,
//! and the runner holds its states behind the last unanswered one (the newest in its place) and
//! keeps every event.
//!
//! A viewer joins a session's feed without its owner (`super::live::Tap`): it's counted, then reads
//! where the feed ends, then the view. The owner publishes a view, then asks whether anyone watches
//! ([`Mirror::commit`]): one of the two sees the other, so every change is in a viewer's view or in
//! a frame past where it reads from.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::hash::BuildHasher as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use bytes::Bytes;
use flate2::Compression;
use serde_json::Value;

use super::core::{MAX_EVENTS, Update};
use super::live::{Feed, Payload, Tap};
use super::packed::Segment;
use super::registry::{Entry, Files, Location, Parts, PatchOf, changes_patch_of, merge_changes, null, write};
use super::state_doc::StateDoc;
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

/// A session's newest events its copy keeps in memory, whatever else of it is spilled (`super::spill`; goal
/// 11, plan 01a1a87b2e56 step 5): the events API's page as it's asked for without a `limit` (`GET
/// /api/harness/sessions/{id}/events`, `crate::api`'s `conversation_events`), which is what a client's first
/// read asks for at the most (the lab's readers ask for 50), so the API answers it from memory as before
/// (goal 10, plan 01a1a87b2dae step 2). A sealed chunk past it, once packed, goes to its copy's spill file:
/// a copy keeps its index, its open chunk and the chunks among these (two at the most, of [`CHUNK`]).
pub const EVENTS_PAGE: usize = 100;

/// The packed bytes an event measured (research f20: a 20,000-event session's copy kept 4,640 events packed
/// in 3.00 MB, 647 bytes each): what the bounds of what's packed are reckoned in (goal 11, plan 01a1a87b2e56
/// step 3).
pub const PACKED_EVENT_BYTES: usize = 647;

/// The most of its packed segments a copy keeps in memory once its spills land (goal 11, plan 01a1a87b2e56
/// step 3): the sealed chunks among its newest [`EVENTS_PAGE`] events, two at the most (the open chunk holds
/// at least one of those events), at the packed size an event measured: 2 × [`CHUNK`] × [`PACKED_EVENT_BYTES`],
/// 90,580 bytes. Chunks of larger events than that past it are spilled too, the newest page or not (the
/// events API then reads that copy's page from the store), so `session_segments` holds at most this for each
/// copy ([`segments_bound`]), beside what's still on its way to a spill: a copy's one spill in flight, the
/// packer's backlog, and the views a snapshot being made still holds.
pub const PAGE_BYTES: usize = 2 * CHUNK * PACKED_EVENT_BYTES;

/// Session copies there are (a copy made, less one let go of): what `session_segments`' bound is reckoned for.
static COPIES: AtomicUsize = AtomicUsize::new(0);

/// `session_segments`' bound on `/metrics` (`kiln_memory_bound_bytes{owner="session_segments"}`): [`PAGE_BYTES`]
/// for each copy there is.
pub fn segments_bound() -> u64 {
    u64::try_from(COPIES.load(Ordering::Relaxed).saturating_mul(PAGE_BYTES)).unwrap_or(u64::MAX)
}

/// What a reader needs of one of a copy's events without its text: its id and number, the copy's
/// revision when it last changed, its text's length (where it is among its chunk's texts joined by `,`,
/// as an `events` frame joins them, is reckoned from the lengths before it: [`Chunk::off`]), and its
/// text's digest (`crate::wire::sums::event_text`). Numbering, finding, resuming, digests and a
/// snapshot's plan read these alone. Goal 10, plan 01a1a87b2dae step 3, in the shape agreed with goal 11,
/// whose stored form keeps these and the chunk's [`Segment`] and decodes an event's text only where it's
/// read.
///
/// 72 bytes (goal 11, plan 01a1a87b2e56 step 5): a copy keeps a meta for each event it keeps, spilled or
/// not, and a compact chunk its [`CHUNK`] metas in one allocation, which jemalloc's size classes round up
/// (a quarter of a doubling apart past 4 KiB): 70 metas of 72 bytes take 5,120 bytes, where of 88 they took
/// 7,168 (and of 96 the same), about 29 bytes an event.
#[derive(Debug)]
pub struct Meta {
    /// Empty for an event without one (none the copy takes in: `Mirror::upsert`). Kept in the meta itself
    /// ([`Id`]), not an allocation of its own: a copy keeps a meta for each event it keeps, spilled or not.
    id: Id,
    /// Its number, when it has one ([`Meta::seq`]: `len`'s top bit says so).
    seq: u64,
    pub at: u64,
    /// Its text's length in bytes, in the low 63 bits (no allocation is 2^63 bytes long: Rust's are at most
    /// `isize::MAX`), and in the top bit whether it has a number ([`NUMBERED`]), where an `Option<u64>` took 16
    /// bytes of its own. Where its text begins is reckoned from the lengths before it ([`Chunk::off`]: a chunk
    /// holds at most [`CHUNK`]), never kept.
    len: u64,
    /// Its text's digest: made off the owner's shard, a chunk's all as it's packed ([`Chunk::pack`]:
    /// a packed chunk's are all there), or where first asked for ([`Meta::sum`]). Made as the copy
    /// took each event in, it held the owner's shard for a whole fill's text (goal 10's A/B).
    /// [`NO_SUM`] until it's made: 8 bytes, where a `OnceLock<u64>` took 16 (goal 11, plan 01a1a87b2e56
    /// step 5: a copy keeps a meta for each of its events, spilled or not).
    sum: AtomicU64,
}

/// A meta's digest not made yet ([`Meta::sum`]). A text whose digest is this itself (one in 2^64) has its
/// digest made again each time it's asked for, as one not made yet: never another's.
const NO_SUM: u64 = 0;

/// A meta's `len`'s top bit: its event has a number ([`Meta::seq`]).
const NUMBERED: u64 = 1 << 63;

impl Clone for Meta {
    fn clone(&self) -> Self {
        Self { id: self.id.clone(), seq: self.seq, at: self.at, len: self.len, sum: AtomicU64::new(self.sum.load(Ordering::Relaxed)) }
    }
}

/// Metas are equal as a `OnceLock`'s digest was: the same event, and the same digest made (or none).
impl PartialEq for Meta {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.seq() == other.seq() && self.at == other.at && self.len == other.len && self.sum_made() == other.sum_made()
    }
}

impl Eq for Meta {}

impl Meta {
    fn of(ev: &Ev) -> Self {
        let seq = ev.entry.seq();
        let len = u64::try_from(ev.entry.text().len()).unwrap_or(u64::MAX) & !NUMBERED;
        Self { id: Id::of(ev.entry.id().unwrap_or_default()), seq: seq.unwrap_or_default(), at: ev.at, len: if seq.is_some() { len | NUMBERED } else { len }, sum: AtomicU64::new(NO_SUM) }
    }

    /// Its event's number in its session's timeline, if it has one.
    pub fn seq(&self) -> Option<u64> {
        (self.len & NUMBERED != 0).then_some(self.seq)
    }

    /// Its text's length in bytes.
    pub fn text_len(&self) -> usize {
        usize::try_from(self.len & !NUMBERED).unwrap_or(usize::MAX)
    }

    /// Its event's id (empty: none).
    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    /// Its text's digest, `ev`'s (the event it's the meta of), made now if it wasn't yet. The digest is its
    /// text's alone, so any thread that makes it makes the same: whichever keeps it last keeps that.
    pub fn sum(&self, ev: &Ev) -> u64 {
        if let Some(made) = self.sum_made() {
            return made;
        }
        let made = ev.entry.sum();
        self.sum.store(made, Ordering::Relaxed);
        made
    }

    /// Its text's digest, if it's been made.
    pub fn sum_made(&self) -> Option<u64> {
        Some(self.sum.load(Ordering::Relaxed)).filter(|s| *s != NO_SUM)
    }
}

/// The most bytes of an id a meta keeps in itself ([`Id`]): a UUID's 36 with room to spare (kiln's 32, a
/// harness's `msg_` and `toolu_` ids about 30), so the id takes 40 bytes of its meta, against the 16 of the
/// `Arc<str>` it held and that one's allocation of 48 to 64.
const ID_INLINE: usize = 38;

/// An event's id as its meta keeps it (goal 11, plan 01a1a87b2e56 step 5): in the meta up to [`ID_INLINE`]
/// bytes, a longer one boxed. A copy keeps a meta for each of up to `MAX_EVENTS` and a quarter of its events,
/// spilled or not, so each id's own allocation (its counts and its bytes, jemalloc's 48 or 64-byte class) was
/// most of what a copy held past its newest page.
#[derive(Clone, PartialEq, Eq)]
enum Id {
    Inline { len: u8, bytes: [u8; ID_INLINE] },
    Boxed(Box<str>),
}

impl Id {
    fn of(id: &str) -> Self {
        let Ok(len) = u8::try_from(id.len()) else { return Id::Boxed(id.into()) };
        let mut bytes = [0u8; ID_INLINE];
        match bytes.get_mut(..id.len()) {
            Some(room) => room.copy_from_slice(id.as_bytes()),
            None => return Id::Boxed(id.into()),
        }
        Id::Inline { len, bytes }
    }

    fn as_str(&self) -> &str {
        match self {
            // Bytes of a `&str` whole, so a whole UTF-8 string.
            Id::Inline { len, bytes } => bytes.get(..usize::from(*len)).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or_default(),
            Id::Boxed(id) => id,
        }
    }
}

impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Where each of a copy's events is, by its id (goal 11, plan 01a1a87b2e56 step 5): keyed by a hash of the id, each
/// place it finds checked against the id its event's meta keeps (in memory, its chunk spilled or not), and an id
/// whose hash another's already has kept apart, by the id itself. Its places move as events go from the front of the
/// copy or past a number ([`Index::retain`]), as the copy's own did.
///
/// A bucket is 8 bytes: half of the id's keyed hash and the event's place, each a `u32` (a copy keeps at most
/// `MAX_EVENTS` and a quarter, and one past a `u32` is kept apart), where a `u64` and a `usize` took 16. The table
/// hashes its keys by spreading them ([`Spread`]), not with SipHash again: they're a keyed SipHash already.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Index {
    by_hash: HashMap<u32, u32, std::hash::BuildHasherDefault<Spread>>,
    apart: HashMap<Box<str>, usize>,
}

/// How the index's table hashes a key it's given (goal 11, plan 01a1a87b2e56 step 5): a key is half of a keyed
/// SipHash ([`Index::hash`]), its bits as random as the hash's, so the key itself is its hash, written in both halves
/// of the table's `u64`: hashbrown picks a bucket by the low bits and tags it by the top seven, and here each comes
/// from the key's own bits. A key isn't a runner's to choose: it doesn't know the process's SipHash key.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Spread(u64);

impl std::hash::Hasher for Spread {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u32(&mut self, key: u32) {
        self.0 = (u64::from(key) << 32) | u64::from(key);
    }

    /// Only `u32` keys are hashed with it; anything else, as FNV-1a writes bytes (its 64-bit prime), so it still
    /// hashes as a hasher must.
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl Index {
    /// The process's own key for ids' hashes: an id's hash isn't another server's, or a runner's to choose. The
    /// index keys an id by the high half of its 64-bit SipHash.
    fn hash(id: &str) -> u32 {
        static KEY: OnceLock<std::collections::hash_map::RandomState> = OnceLock::new();
        u32::try_from(KEY.get_or_init(std::collections::hash_map::RandomState::new).hash_one(id) >> 32).unwrap_or_default()
    }

    /// Where event `id` is in `events`.
    pub(super) fn get(&self, events: &Events, id: &str) -> Option<usize> {
        if let Some(at) = self.by_hash.get(&Self::hash(id)).and_then(|at| usize::try_from(*at).ok())
            && events.meta(at).is_some_and(|m| m.id() == id)
        {
            return Some(at);
        }
        self.apart.get(id).copied()
    }

    /// Event `id` is at `at` in `events`.
    pub(super) fn insert(&mut self, events: &Events, id: &str, at: usize) {
        let Ok(place) = u32::try_from(at) else {
            self.apart.insert(id.into(), at);
            return;
        };
        match self.by_hash.entry(Self::hash(id)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(place);
            }
            // The same id's, or one no event holds any more: its place.
            std::collections::hash_map::Entry::Occupied(mut o) if usize::try_from(*o.get()).ok().and_then(|was| events.meta(was)).is_none_or(|m| m.id() == id) => {
                o.insert(place);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.apart.insert(id.into(), at);
            }
        }
    }

    /// Lets go of event `id`'s place, where it's `at`: another event takes that place (`Mirror::upsert`, goal 9, plan
    /// 01a1a87b2cec step 3: the index was made again for it, every event's id hashed, on the owner's shard).
    pub(super) fn forget(&mut self, id: &str, at: usize) {
        if let std::collections::hash_map::Entry::Occupied(o) = self.by_hash.entry(Self::hash(id))
            && usize::try_from(*o.get()).ok() == Some(at)
        {
            o.remove();
        }
        if self.apart.get(id) == Some(&at) {
            self.apart.remove(id);
        }
    }

    /// `events`' ids, each where it is.
    fn of(events: &Events) -> Self {
        let mut index = Index::default();
        for (at, m) in events.metas().enumerate().filter(|(_, m)| !m.id().is_empty()) {
            index.insert(events, m.id(), at);
        }
        index
    }

    /// Keeps the places `keep` says to, each as `keep` leaves it (moved down past those let go of).
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&mut usize) -> bool) {
        self.by_hash.retain(|_, place| {
            let Ok(mut at) = usize::try_from(*place) else { return false };
            let kept = keep(&mut at);
            match u32::try_from(at) {
                Ok(now) => {
                    *place = now;
                    kept
                }
                // Moved past what a bucket holds (never down): let go, as a place it can't say.
                Err(_) => false,
            }
        });
        self.apart.retain(|_, at| keep(at));
    }

    pub(super) fn clear(&mut self) {
        self.by_hash.clear();
        self.apart.clear();
    }

    /// How many ids it finds.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.by_hash.len() + self.apart.len()
    }

    /// Those of the first `n` places let go of, the rest moved down by as many: the copy's oldest `n` went.
    pub(super) fn drop_front(&mut self, n: usize) {
        self.retain(|at| match at.checked_sub(n) {
            Some(now) => {
                *at = now;
                true
            }
            None => false,
        });
    }
}

/// Bytes the copies' chunks hold deflated ([`Chunk::pack`]): what packing costs in memory, which goal
/// 11 exports and bounds (`kiln_memory_bytes{owner="session_segments"}`).
pub static SEGMENT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// What the reads of its copy's newest page kept of a compact chunk's texts joined ([`Chunk::page_texts`]):
/// those from byte `from` of them on, the furthest back a read reached (goal 11, plan 01a1a87b2e56 step 5).
struct PageTexts {
    from: usize,
    bytes: Bytes,
}

/// A chunk of a copy's events: each one's [`Meta`], the events themselves, and, once it's sealed (full,
/// and not the newest) or a snapshot took it whole, their texts joined as an `events` frame joins them,
/// deflated once ([`Segment`]): every viewer's packed snapshot takes that as it is (`super::packed`;
/// goal 10, plan 01a1a87b2dae step 3). Copied to change (a view shares it), the copy starts without
/// one; changed in place, it lets go of it.
///
/// A sealed chunk, once packed, is kept compact: its metas and its segment alone, its events read again
/// from the segment when they're asked for and kept by no one after (goal 11, plan 01a1a87b2e56 step 2: a
/// 20,000-event session's copy kept 8,260 bytes for each event, its JSON's tree and its text). Only the
/// newest chunk, which takes the next event, keeps its events as they are.
pub struct Chunk {
    meta: Vec<Meta>,
    /// Its events as they are; none in a compact one ([`Chunk::is_compact`]).
    evs: Vec<Ev>,
    packed: OnceLock<Arc<Segment>>,
    /// Whether it was sent to be packed (`seal`): once a chunk.
    sealing: AtomicBool,
    /// Where its segment went, once spilled (`super::spill`): a spilled chunk keeps its metas alone in
    /// memory, its segment read back where it's asked for (a snapshot, on the blocking pool).
    spilled: Option<super::spill::Spilled>,
    /// A compact one's texts, inflated by the first read of its copy's newest page that needed them and kept
    /// for the reads after it while it's the newest compact chunk of its copy ([`Chunk::page_texts`]; goal 10,
    /// plan 01a1a87b2dae step 5): the lab's events reads inflated a chunk's whole segment each time, on the
    /// request's shard (events at p99 1.1 to 2.0 ms, rt5 calm, the slowest read). Counted as text kept of a
    /// packed chunk (`crate::memory::SESSION_ENTRIES`). Only those its reads reached back to are kept
    /// ([`PageTexts`]; goal 11, plan 01a1a87b2e56 step 5).
    ///
    /// Its cell is made by the first read that keeps them, and a chunk no read paged has none (goal 9, plan
    /// 01a1a87b2cec step 5): arc-swap pays every thread's debt slots, a locked compare-and-swap each, whenever
    /// one of its cells is swapped or let go of, an empty one too (`Debt::pay_all`, no fast path for nothing
    /// held). Every chunk dropped did that three times (its unpack's swap, its drop's, the cell's own), and
    /// every event taken in at a copy's newest chunk drops one (the copy of it the view shared): rt7's calm
    /// profile had it at 0.08% of the process, 0.5% of the shards' time, beside the copy's view's own store.
    texts: OnceLock<ArcSwapOption<PageTexts>>,
    /// Whether it's its copy's newest compact chunk, whose texts a read keeps: set and unset by its owner
    /// alone, as chunks are compacted ([`Events::compact`]).
    paged: AtomicBool,
}

impl Clone for Chunk {
    fn clone(&self) -> Self {
        // A compact chunk's events are its segment: its copy keeps it. Another's copy is made to be changed,
        // and starts without one.
        let packed = OnceLock::new();
        if self.is_compact()
            && let Some(s) = self.packed.get()
            && packed.set(Arc::clone(s)).is_ok()
        {
            SEGMENT_BYTES.fetch_add(s.deflated.len(), Ordering::Relaxed);
        }
        let spilled = self.spilled.clone().filter(|_| self.is_compact());
        Self { meta: self.meta.clone(), evs: self.evs.clone(), packed, sealing: AtomicBool::new(false), spilled, texts: OnceLock::new(), paged: AtomicBool::new(false) }
    }
}

impl Drop for Chunk {
    /// Its segment counted out, and what a page's read kept of its texts let go of (`unpack`).
    fn drop(&mut self) {
        self.unpack();
    }
}

impl Chunk {
    fn of(evs: Vec<Ev>) -> Self {
        let meta = evs.iter().map(Meta::of).collect();
        Self { meta, evs, packed: OnceLock::new(), sealing: AtomicBool::new(false), spilled: None, texts: OnceLock::new(), paged: AtomicBool::new(false) }
    }

    pub fn len(&self) -> usize {
        self.meta.len()
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// Whether it holds all a chunk holds: no event goes in it after.
    pub fn is_full(&self) -> bool {
        self.meta.len() >= CHUNK
    }

    pub fn metas(&self) -> &[Meta] {
        &self.meta
    }

    /// Where event `k`'s text begins among its texts joined (a `,` between each two, as an `events` frame
    /// joins them): past the texts before it and their commas, reckoned from their lengths (goal 11, plan
    /// 01a1a87b2e56 step 5: a meta keeps no offset), at most [`CHUNK`] of them.
    pub fn off(&self, k: usize) -> usize {
        self.meta.iter().take(k).map(|m| m.text_len() + 1).sum()
    }

    /// Its texts joined, in bytes: each one's, and a `,` between each two.
    pub fn texts_len(&self) -> usize {
        self.meta.iter().map(Meta::text_len).sum::<usize>() + self.meta.len().saturating_sub(1)
    }

    /// Where its events `k` to `k + n` (at least one) lie among its texts joined: from the first one's text's
    /// beginning to the last one's end. None past its events.
    pub fn span(&self, k: usize, n: usize) -> Option<std::ops::Range<usize>> {
        let from = self.off(k);
        let texts = self.meta.get(k..k.checked_add(n)?)?.iter().map(Meta::text_len).sum::<usize>();
        Some(from..from + texts + n.checked_sub(1)?)
    }

    /// Whether its events are kept in its segment alone (goal 11, plan 01a1a87b2e56 step 2).
    pub fn is_compact(&self) -> bool {
        self.evs.is_empty() && !self.meta.is_empty()
    }

    /// Its events: as it keeps them, or a compact one's read again from its segment, each text as it was and
    /// what's looked up of it read from that (kept by no one after). Refused when its segment doesn't hold
    /// what its metas say.
    pub fn evs(&self) -> std::io::Result<Cow<'_, [Ev]>> {
        if !self.is_compact() {
            return Ok(Cow::Borrowed(&self.evs));
        }
        let joined = self.joined()?;
        let mut off = 0;
        self.meta
            .iter()
            .map(|m| {
                let (from, len) = (off, m.text_len());
                off = from + len + 1;
                let text = joined.get(from..from + len).and_then(|b| std::str::from_utf8(b).ok());
                let text = text.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "a packed event's text isn't where its meta says"))?;
                Ok(Ev { entry: Arc::new(Entry::from_text(text)), at: m.at })
            })
            .collect::<std::io::Result<Vec<Ev>>>()
            .map(Cow::Owned)
    }

    /// Its texts joined as an `events` frame joins them: a compact one's segment inflated (read back first
    /// when it's spilled), another's written out now.
    pub fn joined(&self) -> std::io::Result<Cow<'_, [u8]>> {
        if self.is_compact() {
            if let Some(s) = self.packed.get() {
                return super::packed::inflate(s).map(Cow::Owned);
            }
            let s = self.spilled.as_ref().ok_or_else(|| std::io::Error::other("a compact chunk without its segment"))?;
            return super::packed::inflate(&s.segment()?).map(Cow::Owned);
        }
        let mut joined = Vec::with_capacity(self.texts_len());
        for (i, e) in self.evs.iter().enumerate() {
            if i > 0 {
                joined.push(b',');
            }
            joined.extend_from_slice(e.entry.text().as_bytes());
        }
        Ok(Cow::Owned(joined))
    }

    /// A compact one's texts joined ([`Chunk::joined`]) from byte `from` of them on, as bytes a page's answer
    /// takes slices of, none copied, and the byte of them they begin at (`from` or before): as a read of its
    /// copy's newest page kept them, else inflated now, and kept for the reads after while it's its copy's
    /// newest compact chunk (`paged`; goal 10, plan 01a1a87b2dae step 5). What's kept begins where the reads
    /// reached back to, no further (goal 11, plan 01a1a87b2e56 step 5): the newest page ends at the copy's
    /// newest event, so its reads take a compact chunk's last events alone, the fewer the more of the page its
    /// open chunk holds, and the same page read again takes no more of it. A read that reaches further back
    /// than what's kept inflates the chunk again and keeps from there; else the longest kept stays. Its owner
    /// unsetting `paged` lets them go, and so does a read that finds it unset just after keeping them (one of
    /// the two sees the other). The cell they're kept in is made here, by the first read that keeps them (the
    /// field's notes).
    fn page_texts(&self, from: usize) -> std::io::Result<(usize, Bytes)> {
        let cell = self.texts.get();
        if let Some(kept) = cell.and_then(ArcSwapOption::load_full).filter(|k| k.from <= from) {
            return Ok((kept.from, Bytes::clone(&kept.bytes)));
        }
        let joined = self.joined()?.into_owned();
        if !(self.is_compact() && self.paged.load(Ordering::SeqCst)) {
            return Ok((0, Bytes::from(joined)));
        }
        // Those it reached back to, alone: a slice of the whole would hold it all.
        let made = Arc::new(PageTexts { from, bytes: Bytes::copy_from_slice(joined.get(from..).ok_or_else(|| std::io::Error::other("a page's read began past its chunk's texts"))?) });
        let cell = self.texts.get_or_init(ArcSwapOption::empty);
        let mut now = cell.load_full();
        // Kept unless what's kept by now reaches as far back (a read beside this one kept it meanwhile).
        while now.as_ref().is_none_or(|k| k.from > from) {
            let was = cell.compare_and_swap(&now, Some(Arc::clone(&made)));
            let swapped = match (&*was, &now) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !swapped {
                now = Option::clone(&*was);
                continue;
            }
            crate::memory::SESSION_ENTRIES.add(made.bytes.len());
            if let Some(shorter) = now {
                crate::memory::SESSION_ENTRIES.sub(shorter.bytes.len());
            }
            // The cell made and the texts kept, then `paged` read; `unpage` writes `paged` and then looks for the
            // cell: a fence on each side, so one of the two sees the other's (the cell is a `OnceLock`'s, read
            // with Acquire alone, where the swaps of a cell always there were all SeqCst).
            std::sync::atomic::fence(Ordering::SeqCst);
            if !self.paged.load(Ordering::SeqCst) {
                self.unpage();
            }
            break;
        }
        Ok((from, made.bytes.clone()))
    }

    /// Lets go of the texts a page's read kept ([`Chunk::page_texts`]); with no cell made for them, nothing to
    /// swap. Its caller unset `paged` first, or found it unset: the fence pairs with `page_texts`' own.
    fn unpage(&self) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if let Some(kept) = self.texts.get().and_then(|cell| cell.swap(None)) {
            crate::memory::SESSION_ENTRIES.sub(kept.bytes.len());
        }
    }

    /// The bytes of its texts a page's read keeps now ([`Chunk::page_texts`]), if any.
    pub fn page_kept(&self) -> Option<usize> {
        self.texts.get().and_then(|cell| cell.load().as_ref().map(|t| t.bytes.len()))
    }

    /// It kept compact, once packed: its metas and its segment (which a view of it still taking it shares).
    /// None when it isn't packed, or is compact already.
    fn compacted(&self) -> Option<Self> {
        let s = self.packed.get()?;
        if self.is_compact() {
            return None;
        }
        let packed = OnceLock::new();
        if packed.set(Arc::clone(s)).is_ok() {
            SEGMENT_BYTES.fetch_add(s.deflated.len(), Ordering::Relaxed);
        }
        Some(Self { meta: self.meta.clone(), evs: Vec::new(), packed, sealing: AtomicBool::new(true), spilled: None, texts: OnceLock::new(), paged: AtomicBool::new(false) })
    }

    /// Whether its segment is on disk (`super::spill`), not in memory.
    pub fn is_spilled(&self) -> bool {
        self.spilled.is_some()
    }

    /// Its segment's bytes on disk, once spilled.
    pub fn spilled_bytes(&self) -> Option<usize> {
        self.spilled.as_ref().map(super::spill::Spilled::bytes)
    }

    /// It spilled: its metas, and where its segment went (`s`). None unless it's compact with its segment in
    /// memory (one changed since is kept as it is).
    fn spilled_as(&self, s: super::spill::Spilled) -> Option<Self> {
        (self.is_compact() && self.packed.get().is_some()).then(|| Self { meta: self.meta.clone(), evs: Vec::new(), packed: OnceLock::new(), sealing: AtomicBool::new(true), spilled: Some(s), texts: OnceLock::new(), paged: AtomicBool::new(false) })
    }

    /// A compact chunk's events read again into it, to change one. False when its segment can't give them
    /// back (it's left as it was).
    fn thaw(&mut self) -> bool {
        if !self.is_compact() {
            return true;
        }
        match self.evs().map(Cow::into_owned) {
            Ok(evs) => {
                // Read back to change: kept in memory again (a later spill writes it anew).
                self.evs = evs;
                self.spilled = None;
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "a session copy's packed events couldn't be read again: what was to change them is let go of");
                false
            }
        }
    }

    /// Its texts as an `events` frame joins them, deflated: made once and kept, then taken as it is.
    /// No one waits on another making it: whoever finds none makes its own, and the first kept stays.
    pub fn pack(&self) -> std::io::Result<Arc<Segment>> {
        if let Some(s) = self.packed.get() {
            return Ok(Arc::clone(s));
        }
        // Spilled: read back (on the blocking pool, as snapshots are made), and kept nowhere.
        if let Some(s) = &self.spilled {
            return s.segment().map(Arc::new);
        }
        // Its digests with it, off the owner's shard: a packed chunk's are all there.
        for (m, e) in self.meta.iter().zip(&self.evs) {
            m.sum(e);
        }
        Ok(self.keep(self.deflate(Compression::default())?))
    }

    /// `made` kept as what it holds deflated, unless it keeps one already (the first kept stays): what it
    /// keeps.
    fn keep(&self, made: Segment) -> Arc<Segment> {
        let made = Arc::new(made);
        if self.packed.set(Arc::clone(&made)).is_ok() {
            SEGMENT_BYTES.fetch_add(made.deflated.len(), Ordering::Relaxed);
        }
        self.packed.get().map_or(made, Arc::clone)
    }

    /// Its texts as an `events` frame joins them, deflated at `level` now and kept nowhere (the newest
    /// chunk, which the next event changes).
    pub fn deflate(&self, level: Compression) -> std::io::Result<Segment> {
        let parts = self.evs.iter().enumerate().flat_map(|(i, e)| [if i == 0 { b"".as_slice() } else { b",".as_slice() }, e.entry.text().as_bytes()]);
        super::packed::deflate(level, parts)
    }

    /// What it holds deflated, once it does.
    pub fn packed(&self) -> Option<&Arc<Segment>> {
        self.packed.get()
    }

    fn unpack(&mut self) {
        if let Some(s) = self.packed.take() {
            SEGMENT_BYTES.fetch_sub(s.deflated.len(), Ordering::Relaxed);
        }
        // Changed, it's to be packed again once it's sealed (`Events::compact`), and what a page's read kept
        // of its texts is no longer what it holds: its cell taken out whole, this chunk's alone now, so a paged
        // one pays arc-swap's debts once as it goes, and one never paged not at all (goal 9, plan 01a1a87b2cec
        // step 5). A later page's read makes another.
        *self.sealing.get_mut() = false;
        *self.paged.get_mut() = false;
        if let Some(kept) = self.texts.take().and_then(ArcSwapOption::into_inner) {
            crate::memory::SESSION_ENTRIES.sub(kept.bytes.len());
        }
    }

    /// Its event `k` changed by `f`, with its meta (where those after it begin is reckoned from the lengths:
    /// [`Chunk::off`]).
    fn change(&mut self, k: usize, f: impl FnOnce(&mut Ev)) -> bool {
        if !self.thaw() {
            return false;
        }
        self.unpack();
        let Some(ev) = self.evs.get_mut(k) else { return false };
        f(ev);
        if let (Some(m), Some(e)) = (self.meta.get_mut(k), self.evs.get(k)) {
            *m = Meta::of(e);
        }
        true
    }

    fn push(&mut self, ev: Ev) {
        // The newest is never compact (`Events::compact`); one read again first all the same.
        if !self.thaw() {
            return;
        }
        self.unpack();
        self.meta.push(Meta::of(&ev));
        self.evs.push(ev);
    }
}

/// A copy's events in order, in chunks ([`Chunk`]) of [`CHUNK`]: the first may hold fewer, once its
/// oldest have gone ([`Events::drain_front`]), and the newest may; each is shared, so a view takes them
/// whole by reference, and the owner's next change copies only what it touches. The newest chunk is held
/// apart from the list of the others (goal 9, plan 01a1a87b2cec step 5): an event taken in at it, or one in
/// it changed, copies that chunk alone where a view shares it; with one list of every chunk, each such event
/// copied the list too (a long session's ~72 chunks) and let the last one go.
#[derive(Clone, Default)]
pub struct Events {
    /// Every chunk but the newest, in order.
    older: Arc<Vec<Arc<Chunk>>>,
    /// The newest chunk, which takes the next event: none only with no events, and no older chunks.
    newest: Option<Arc<Chunk>>,
    len: usize,
    /// The events in the first chunk.
    first: usize,
}

/// A copy's chunks in order, the newest last ([`Events::chunks`]): what a slice of them was, made of the older
/// ones' list and the newest held apart, nothing collected.
#[derive(Clone, Copy)]
pub struct Chunks<'a> {
    older: &'a [Arc<Chunk>],
    newest: Option<&'a Arc<Chunk>>,
}

/// [`Chunks`]' chunks in order.
pub type ChunksIter<'a> = std::iter::Chain<std::slice::Iter<'a, Arc<Chunk>>, std::option::IntoIter<&'a Arc<Chunk>>>;

impl<'a> Chunks<'a> {
    pub fn len(&self) -> usize {
        self.older.len() + usize::from(self.newest.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Option<&'a Arc<Chunk>> {
        match self.older.get(i) {
            Some(c) => Some(c),
            None => self.newest.filter(|_| i == self.older.len()),
        }
    }

    pub fn first(&self) -> Option<&'a Arc<Chunk>> {
        self.get(0)
    }

    pub fn last(&self) -> Option<&'a Arc<Chunk>> {
        self.newest.or_else(|| self.older.last())
    }

    pub fn iter(&self) -> ChunksIter<'a> {
        self.older.iter().chain(self.newest)
    }

    pub fn to_vec(&self) -> Vec<Arc<Chunk>> {
        self.iter().cloned().collect()
    }
}

impl<'a> IntoIterator for Chunks<'a> {
    type Item = &'a Arc<Chunk>;
    type IntoIter = ChunksIter<'a>;

    fn into_iter(self) -> ChunksIter<'a> {
        self.iter()
    }
}

impl Events {
    /// Made from `all`, in order.
    fn of(all: Vec<Ev>) -> Self {
        let len = all.len();
        let mut chunks = Vec::with_capacity(len.div_ceil(CHUNK));
        let mut rest = all.into_iter().peekable();
        while rest.peek().is_some() {
            chunks.push(Arc::new(Chunk::of(rest.by_ref().take(CHUNK).collect())));
        }
        let newest = chunks.pop();
        Self { older: Arc::new(chunks), newest, len, first: len.min(CHUNK) }
    }

    /// Chunk `c` of [`Events::chunks`].
    fn chunk(&self, c: usize) -> Option<&Arc<Chunk>> {
        self.chunks().get(c)
    }

    /// Its chunks as one list for `f` to change, the newest last, then held apart again: what's done other than taking an
    /// event in at the newest or changing one there (a chunk sealing, a cut, a compaction, a spill, a chunk read back):
    /// each copies the list where a view shares it, as each did.
    fn with_list<R>(&mut self, f: impl FnOnce(&mut Vec<Arc<Chunk>>) -> R) -> R {
        let mut list = Arc::unwrap_or_clone(std::mem::take(&mut self.older));
        list.extend(self.newest.take());
        let out = f(&mut list);
        self.newest = list.pop();
        self.older = Arc::new(list);
        out
    }

    /// Whether these are `other`'s as they were: the same chunks, shared, and as many events.
    pub(super) fn same_as(&self, other: &Events) -> bool {
        let newest = match (&self.newest, &other.newest) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        newest && Arc::ptr_eq(&self.older, &other.older) && self.len == other.len
    }

    /// Where event `i` is: its chunk, and its place in it.
    fn place(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.len {
            return None;
        }
        match i.checked_sub(self.first) {
            None => Some((0, i)),
            Some(past) => Some((1 + past / CHUNK, past % CHUNK)),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Event `i`: as its chunk keeps it, or read again from a compact chunk's segment.
    pub fn get(&self, i: usize) -> Option<Cow<'_, Ev>> {
        let (c, k) = self.place(i)?;
        let chunk = self.chunk(c)?;
        if !chunk.is_compact() {
            return chunk.evs.get(k).map(Cow::Borrowed);
        }
        chunk.evs().ok()?.into_owned().into_iter().nth(k).map(Cow::Owned)
    }

    /// The entries at `at` (in order), each compact chunk among them read again once.
    pub fn entries_at(&self, at: &[usize]) -> Vec<Arc<Entry>> {
        let mut read: Option<(usize, Vec<Ev>)> = None;
        let mut out = Vec::with_capacity(at.len());
        for i in at {
            let Some((c, k)) = self.place(*i) else { continue };
            let Some(chunk) = self.chunk(c) else { continue };
            if !chunk.is_compact() {
                out.extend(chunk.evs.get(k).map(|e| Arc::clone(&e.entry)));
                continue;
            }
            if read.as_ref().is_none_or(|(was, _)| *was != c) {
                read = chunk.evs().ok().map(|evs| (c, evs.into_owned()));
            }
            out.extend(read.as_ref().and_then(|(_, evs)| evs.get(k)).map(|e| Arc::clone(&e.entry)));
        }
        out
    }

    pub fn meta(&self, i: usize) -> Option<&Meta> {
        let (c, k) = self.place(i)?;
        self.chunk(c)?.meta.get(k)
    }

    /// The texts of the events in `range`, joined by `,` as an `events` frame joins them, onto `out`: a
    /// compact chunk's inflated once for all of its events there (goal 11, plan 01a1a87b2e56 step 2), whose
    /// texts lie joined so in its segment, from the first one's `off` to the end of the last one's. None when
    /// the range isn't all there, a compact chunk's segment can't be read, or one is spilled: it's read on the
    /// runtime's threads, which don't wait on disk, and the store answers instead (goal 11, plan 01a1a87b2e56
    /// step 5; the newest [`EVENTS_PAGE`] are never spilled).
    pub fn texts_into(&self, range: std::ops::Range<usize>, out: &mut Vec<u8>) -> Option<()> {
        let mut i = range.start;
        while i < range.end {
            let (c, k) = self.place(i)?;
            let chunk = self.chunk(c)?;
            let n = chunk.len().checked_sub(k)?.min(range.end - i);
            if n == 0 {
                return None;
            }
            if chunk.is_spilled() {
                return None;
            }
            if i > range.start {
                out.push(b',');
            }
            if chunk.is_compact() {
                out.extend_from_slice(chunk.joined().ok()?.get(chunk.span(k, n)?)?);
            } else {
                for (j, e) in chunk.evs.get(k..k + n)?.iter().enumerate() {
                    if j > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(e.entry.text().as_bytes());
                }
            }
            i += n;
        }
        Some(())
    }

    /// [`Events::texts_into`] as pieces onto `out`, none of them copied twice: a compact chunk's part a slice
    /// of its texts as its newest page's reads keep them ([`Chunk::page_texts`]), inflated once for all the
    /// reads of it rather than at each (goal 10, plan 01a1a87b2dae step 5); a `,` between chunks; another's
    /// events' texts written out once. None as `texts_into` says none.
    pub fn pieces_into(&self, range: std::ops::Range<usize>, out: &mut Vec<Bytes>) -> Option<()> {
        let mut i = range.start;
        while i < range.end {
            let (c, k) = self.place(i)?;
            let chunk = self.chunk(c)?;
            let n = chunk.len().checked_sub(k)?.min(range.end - i);
            if n == 0 || chunk.is_spilled() {
                return None;
            }
            if i > range.start {
                out.push(Bytes::from_static(b","));
            }
            if chunk.is_compact() {
                let std::ops::Range { start: from, end: to } = chunk.span(k, n)?;
                let (at, texts) = chunk.page_texts(from).ok()?;
                let (from, to) = (from.checked_sub(at)?, to.checked_sub(at)?);
                if from > to || to > texts.len() {
                    return None;
                }
                out.push(texts.slice(from..to));
            } else {
                let evs = chunk.evs.get(k..k + n)?;
                let mut joined = Vec::with_capacity(evs.iter().map(|e| e.entry.text().len() + 1).sum());
                for (j, e) in evs.iter().enumerate() {
                    if j > 0 {
                        joined.push(b',');
                    }
                    joined.extend_from_slice(e.entry.text().as_bytes());
                }
                out.push(Bytes::from(joined));
            }
            i += n;
        }
        Some(())
    }

    pub fn last(&self) -> Option<Cow<'_, Ev>> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Each event in order: as its chunk keeps it, or a compact chunk's read again from its segment as the
    /// iteration comes to it (a chunk that can't be is passed over, said in the log).
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Cow<'_, Ev>> + '_ {
        self.chunks().iter().flat_map(|c| {
            let evs: Vec<Cow<'_, Ev>> = match c.evs() {
                Ok(Cow::Borrowed(evs)) => evs.iter().map(Cow::Borrowed).collect(),
                Ok(Cow::Owned(evs)) => evs.into_iter().map(Cow::Owned).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "a session copy's packed events couldn't be read again: passed over");
                    Vec::new()
                }
            };
            evs
        })
    }

    pub fn metas(&self) -> impl DoubleEndedIterator<Item = &Meta> + '_ {
        self.chunks().iter().flat_map(|c| c.meta.iter())
    }

    /// Its chunks, in order: a snapshot's pieces.
    pub fn chunks(&self) -> Chunks<'_> {
        Chunks { older: self.older.as_slice(), newest: self.newest.as_ref() }
    }

    /// Event `i` changed by `f`: its chunk copied first when a view shares it, and, for one of the older chunks, their
    /// list too. Whether there was one.
    pub(super) fn change(&mut self, i: usize, f: impl FnOnce(&mut Ev)) -> bool {
        let Some((c, k)) = self.place(i) else { return false };
        if c == self.older.len() {
            return match self.newest.as_mut() {
                Some(chunk) => Arc::make_mut(chunk).change(k, f),
                None => false,
            };
        }
        match Arc::make_mut(&mut self.older).get_mut(c) {
            Some(chunk) => Arc::make_mut(chunk).change(k, f),
            None => false,
        }
    }

    /// `ev` last: the chunk that sealed as it came (full, and no longer the newest), if one did. Where the newest has room,
    /// that chunk alone is copied (when a view shares it); the older chunks' list only as one seals, once a chunk.
    pub(super) fn push(&mut self, ev: Ev) -> Option<Arc<Chunk>> {
        let room = self.newest.as_ref().is_some_and(|c| c.len() < CHUNK);
        let sealed = if room {
            if let Some(newest) = self.newest.as_mut() {
                Arc::make_mut(newest).push(ev);
            }
            if self.older.is_empty() {
                self.first += 1;
            }
            None
        } else {
            // The newest full (or none yet): it joins the older ones, and a chunk of this event is the newest.
            let sealed = self.newest.replace(Arc::new(Chunk::of(vec![ev])));
            match &sealed {
                Some(full) => Arc::make_mut(&mut self.older).push(Arc::clone(full)),
                None => self.first = 1,
            }
            sealed
        };
        self.len += 1;
        sealed
    }

    /// Each event, as owned references (what a change in the middle remakes the chunks from).
    fn to_vec(&self) -> Vec<Ev> {
        self.iter().map(Cow::into_owned).collect()
    }

    /// `ev` in at `i` (rare: an event a runner that started again sends under an older number).
    pub(super) fn insert(&mut self, i: usize, ev: Ev) {
        let mut all = self.to_vec();
        all.insert(i.min(all.len()), ev);
        *self = Self::of(all);
    }

    /// These with `ev` in at `i` ([`Events::insert`]), and their index (`Mirror::reindex`'s), made apart from them: on
    /// the blocking pool, where every chunk is read again, a compact one inflated and each of its events parsed, as the
    /// one that takes the place of each is made (goal 9, plan 01a1a87b2cec step 3: [`Shard::want_insert`]).
    pub(super) fn inserted(&self, i: usize, ev: Ev) -> (Events, Index) {
        let mut made = self.clone();
        made.insert(i, ev);
        let index = Index::of(&made);
        (made, index)
    }

    /// Lets go of the first `n` (a long session's oldest, every thousand events or so): how many went,
    /// and the chunk made again of what's left of the one the cut fell in, when that one sealed. The
    /// chunks past it stay as they were, packed.
    pub(super) fn drain_front(&mut self, n: usize) -> (usize, Option<Arc<Chunk>>) {
        let n = n.min(self.len);
        let remade = self.with_list(|chunks| {
            // Whole chunks go as they are; the one the cut falls in is made again of its rest.
            let (mut left, mut whole) = (n, 0);
            while let Some(c) = chunks.get(whole)
                && left >= c.len()
            {
                left -= c.len();
                whole += 1;
            }
            chunks.drain(..whole.min(chunks.len()));
            let mut remade = None;
            if left > 0
                && let Some(c) = chunks.first_mut()
            {
                let rest: Vec<Ev> = c.evs().map(|evs| evs.iter().skip(left).cloned().collect()).unwrap_or_default();
                *c = Arc::new(Chunk::of(rest));
                remade = Some(Arc::clone(c));
            }
            remade.filter(|_| chunks.len() > 1)
        });
        self.len -= n;
        self.first = self.chunks().first().map_or(0, |c| c.len());
        (n, remade)
    }

    /// [`Events::drain_front`] where the chunk the cut falls in is compact, so it would be read again to be made
    /// of its rest (inflated, or read back from its spill file, and each event parsed: a millisecond or more on
    /// its owner's shard, every thousand events of a long session; goal 9, plan 01a1a87b2cec step 3): the
    /// chunks wholly before it go now, and it stays as it is, its first `left` with it, for its owner to make
    /// again off the shard and take in ([`Events::trim_first`]). How many go now, that chunk, and `left`.
    /// None, and nothing changed, where the cut falls on a chunk's edge or in one that isn't compact
    /// (`drain_front`'s, which reads nothing again then).
    pub(super) fn drain_whole(&mut self, n: usize) -> Option<(usize, Arc<Chunk>, usize)> {
        let n = n.min(self.len);
        let (mut left, mut whole) = (n, 0);
        while let Some(c) = self.chunk(whole)
            && left >= c.len()
        {
            left -= c.len();
            whole += 1;
        }
        let cut = Arc::clone(self.chunk(whole).filter(|c| left > 0 && c.is_compact())?);
        let now = n - left;
        self.with_list(|chunks| {
            chunks.drain(..whole.min(chunks.len()));
        });
        self.len -= now;
        self.first = self.chunks().first().map_or(0, |c| c.len());
        Some((now, cut, left))
    }

    /// The chunk `was`, still its first as it was, made again of its events past its first `left` (`made`, read
    /// off the shard: [`Events::drain_whole`]), those first ones let go of. None, and nothing changed, when its
    /// first chunk isn't `was` any more (an event in it changed, or a later cut let it go) or `made` isn't its
    /// rest. How many it let go of (`left`).
    pub(super) fn trim_first(&mut self, was: &Arc<Chunk>, left: usize, made: Chunk) -> Option<usize> {
        let rest = was.metas().get(left..)?;
        let same = self.chunks().first().is_some_and(|c| Arc::ptr_eq(c, was)) && made.metas().len() == rest.len() && made.metas().iter().zip(rest).all(|(a, b)| a.id == b.id && a.seq() == b.seq());
        if !same {
            return None;
        }
        self.with_list(|chunks| {
            if let Some(first) = chunks.first_mut() {
                *first = Arc::new(made);
            }
        });
        self.len -= left;
        self.first = self.chunks().first().map_or(0, |c| c.len());
        self.page_newest();
        Some(left)
    }

    /// Chunk `was`, still one of these as it was, in `made`'s place: the same events, read back (a compact chunk kept as
    /// its events again, to be changed: [`Shard::want_thaw`]). Whether it was.
    pub(super) fn swap_chunk(&mut self, was: &Arc<Chunk>, made: Chunk) -> bool {
        if made.metas().len() != was.metas().len() || !made.metas().iter().zip(was.metas()).all(|(a, b)| a.id == b.id && a.seq() == b.seq()) {
            return false;
        }
        let Some(at) = self.chunks().iter().position(|c| Arc::ptr_eq(c, was)) else { return false };
        self.with_list(|chunks| match chunks.get_mut(at) {
            Some(slot) => {
                *slot = Arc::new(made);
                true
            }
            None => false,
        })
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Meta) -> bool) {
        let mut all = Vec::with_capacity(self.len);
        for c in self.chunks().iter() {
            let evs = c.evs().map(Cow::into_owned).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "a session copy's packed events couldn't be read again: let go of");
                Vec::new()
            });
            all.extend(c.meta.iter().zip(evs).filter(|(m, _)| keep(m)).map(|(_, e)| e));
        }
        *self = Self::of(all);
    }

    /// [`Events::retain`] where those `keeps` keeps are its first events (a copy's go in by number, so a
    /// number's those through it are): the chunks before the first it lets go of stay as they are (packed,
    /// spilled or neither, none read again), the one that falls in is made again of its events before it,
    /// and the rest go. How many it keeps. None, and nothing changed, when
    /// `keeps` keeps one after one it doesn't: `retain`'s to do. Goal 9, plan 01a1a87b2cec step 3: an Open
    /// going on from a number read every chunk of its copy again (inflating, reading back from its spill file
    /// and parsing each event) on its owner's shard, at every runner's reconnect, to keep them all.
    pub(super) fn keep_first(&mut self, keeps: impl Fn(&Meta) -> bool) -> Option<usize> {
        let kept = self.metas().take_while(|m| keeps(m)).count();
        if self.metas().skip(kept).any(&keeps) {
            return None;
        }
        // All of them kept: nothing to do.
        let Some((c, k)) = self.place(kept) else { return Some(kept) };
        let remade = match self.chunk(c) {
            Some(chunk) if k > 0 => match chunk.evs() {
                Ok(evs) => Some(Chunk::of(evs.iter().take(k).cloned().collect())),
                Err(e) => {
                    tracing::warn!(error = %e, "a session copy's packed events couldn't be read again: let go of");
                    None
                }
            },
            _ => None,
        };
        let len = self.with_list(|chunks| {
            chunks.truncate(c);
            let mut len = kept.saturating_sub(k);
            if let Some(chunk) = remade {
                len += chunk.len();
                chunks.push(Arc::new(chunk));
            }
            len
        });
        self.len = len;
        self.first = self.chunks().first().map_or(0, |c| c.len());
        self.page_newest();
        Some(len)
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every chunk but the newest (which takes the next event) kept compact once it's packed, and those not
    /// packed yet sent to be (goal 11, plan 01a1a87b2e56 step 2). A view taking a chunk keeps it as it was
    /// until it lets go.
    pub(super) fn compact(&mut self, home: Option<&Home>) {
        // Every chunk but the newest: the older ones.
        for c in self.older.iter().filter(|c| !c.is_compact() && c.packed().is_none()) {
            seal(c, home.cloned());
        }
        let ready: Vec<usize> = (0..self.older.len()).filter(|i| self.older.get(*i).is_some_and(|c| !c.is_compact() && c.packed().is_some())).collect();
        if ready.is_empty() {
            return;
        }
        let chunks = Arc::make_mut(&mut self.older);
        for i in ready {
            if let Some(slot) = chunks.get_mut(i)
                && let Some(c) = slot.compacted()
            {
                *slot = Arc::new(c);
            }
        }
        self.page_newest();
    }

    /// Its newest compact chunk in memory is the one whose texts a read of its newest page keeps
    /// ([`Chunk::page_texts`]; goal 10, plan 01a1a87b2dae step 5): set so, and the one before it (the newest
    /// until now, the only other one set) let go of, so a copy keeps one chunk's texts at the most.
    fn page_newest(&self) {
        let mut newest = true;
        for c in self.chunks().iter().rev().filter(|c| c.is_compact() && !c.is_spilled()) {
            if newest {
                c.paged.store(true, Ordering::SeqCst);
                newest = false;
            } else if c.paged.swap(false, Ordering::SeqCst) {
                c.unpage();
            } else {
                break;
            }
        }
    }

    /// Its sealed chunks that are packed and in memory past its newest `page` events, and those among them
    /// past the first `bytes` of segments they keep (counted from the newest): what its copy's owner spills
    /// (`super::spill`), oldest first.
    pub(super) fn spillable(&self, page: usize, bytes: usize) -> Vec<Arc<Chunk>> {
        let (mut after, mut kept) = (0usize, 0usize);
        let mut out = Vec::new();
        for c in self.chunks().iter().rev() {
            if c.is_compact()
                && !c.is_spilled()
                && let Some(s) = c.packed()
            {
                let with = kept.saturating_add(s.deflated.len());
                if after < page && with <= bytes {
                    kept = with;
                } else {
                    out.push(Arc::clone(c));
                }
            }
            after = after.saturating_add(c.len());
        }
        out.reverse();
        out
    }

    /// Each of `chunks` swapped for its spilled form (`spilled`, in the same order), where it's still one of
    /// these events' chunks as it was: one changed or let go of meanwhile stays as it is (its spilled bytes
    /// go when nothing refers to them). How many were.
    pub(super) fn spilled(&mut self, chunks: &[Arc<Chunk>], spilled: Vec<super::spill::Spilled>) -> usize {
        let mut n = 0;
        // Spilled chunks are compact, so older ones: the newest never is.
        let list = Arc::make_mut(&mut self.older);
        for (c, s) in chunks.iter().zip(spilled) {
            if let Some(slot) = list.iter_mut().find(|slot| Arc::ptr_eq(slot, c))
                && let Some(made) = c.spilled_as(s)
            {
                *slot = Arc::new(made);
                n += 1;
            }
        }
        n
    }

    /// The first index whose event `pred` is false for, the events being those it's true for first.
    pub(super) fn partition_point(&self, pred: impl Fn(&Meta) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.meta(mid).is_some_and(&pred) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// A chunk that sealed, waiting to be packed, with its copy's home, whose owner keeps it compact once
/// it's packed (goal 11).
type Sealed = (Weak<Chunk>, Option<Home>);

/// Sealed chunks waiting to be packed: one packer for the process ([`Packer`]), in one job on the
/// compute pool, an event a unit, which gives its thread up between any two events whenever another job
/// waits for it (`crate::rt::compute_pieces`). A job a seal flooded the pool as a fill sealed thousands,
/// holding its other jobs seconds (goal 10's A/B: the compute pool's wait at p99, 0.9 ms to 2.3 s), and
/// a chunk a unit held a thread for a chunk's whole deflate (its pieces at p99, 5 to 45 ms). A chunk
/// gone or packed by then (let go of, changed, packed by a snapshot that needed it) is passed over.
static SEALED: OnceLock<crate::memory::queue::Sender<Sealed>> = OnceLock::new();

/// The packer's queue ([`SEALED`]): lock-free, its owners on every shard sending and the packer taking (goal 4,
/// plan 01a1a59c2a9c step 5: an async-channel's wait took event-listener's `Mutex` on both sides). No bound, as
/// the channel it was had none: an owner never waits for the packer, and each chunk seals once, so it holds at
/// most one entry a chunk the copies keep as text (`crate::memory::owners::SESSION_ENTRIES`' bound).
fn sealed_queue() -> (crate::memory::queue::Sender<Sealed>, crate::memory::queue::Receiver<Sealed>) {
    crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS)
}

/// A chunk that sealed, to be packed off the owner's shard ([`SEALED`]), so a snapshot after it takes
/// it as it is (goal 10, plan 01a1a87b2dae step 3), and its copy's owner then keeps it compact (goal
/// 11, plan 01a1a87b2e56 step 2). Off the runtime (a test's copy) it waits for the snapshot that needs it.
fn seal(chunk: &Arc<Chunk>, home: Option<Home>) {
    if !crate::rt::on_shard() || chunk.packed().is_some() || chunk.sealing.swap(true, Ordering::Relaxed) {
        return;
    }
    let packer = SEALED.get_or_init(|| {
        let (tx, rx) = sealed_queue();
        drop(crate::rt::spawn(pack_sealed(rx)));
        tx
    });
    let _ = packer.try_send((Arc::downgrade(chunk), home));
}

/// The chunk a copy's cut fell in (`cut`, compact: [`Events::drain_whole`]) made again of its events past its
/// first `left`, read on the blocking pool (from its spill file when it's there), and handed to the copy's
/// owner (`home`) to take in its turn ([`Shard::trimmed`]). One that can't be read is said, and its owner
/// keeps it whole until its next cut.
fn trim_later(home: Home, cut: Arc<Chunk>, left: usize) {
    drop(crate::rt::spawn(async move {
        let was = Arc::clone(&cut);
        let made = crate::rt::blocking(move || cut.evs().map(|evs| Chunk::of(evs.iter().skip(left).cloned().collect()))).await;
        let made = match made {
            Ok(Ok(made)) => made,
            Ok(Err(e)) => {
                tracing::warn!(session = %home.id, error = %e, "a session copy's packed events couldn't be read again: its oldest are kept till its next cut");
                return;
            }
            Err(e) => {
                tracing::warn!(session = %home.id, error = %format!("{e:#}"), "a session copy's oldest weren't let go of: kept till its next cut");
                return;
            }
        };
        if let Some(inbox) = home.inbox.upgrade() {
            let id = Arc::clone(&home.id);
            let _ = inbox.send(Msg::Run(Box::new(move |s: &mut Shard| s.trimmed(&id, &was, left, made)))).await;
        }
    }));
}

/// What tells a copy's owner to keep its chunks compact (goal 11): at once once one is packed, not at
/// the copy's next event, which an idle session may never have.
fn compact_msg(home: &Home) -> Msg {
    let id = Arc::clone(&home.id);
    Msg::Run(Box::new(move |s: &mut Shard| s.compact(&id)))
}

/// The packer ([`SEALED`]): a job on the compute pool for as long as chunks wait ([`Packer::piece`]).
/// A packed chunk's owner is told from the pool when its inbox takes the message now, else once the
/// job ends (its inbox full then: the owner busy).
async fn pack_sealed(rx: crate::memory::queue::Receiver<Sealed>) {
    while let Ok(first) = rx.recv().await {
        let packer = Packer { rx: rx.clone(), waiting: vec![first], now: None, later: Vec::new() };
        let packed = crate::rt::compute_pieces(packer, |p, turn| p.piece(&|| turn.over())).await;
        match packed {
            Ok(later) => {
                for home in later {
                    if let Some(inbox) = home.inbox.upgrade() {
                        let _ = inbox.send(compact_msg(&home)).await;
                    }
                }
            }
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "sealed chunks weren't packed: snapshots pack them as they need them"),
        }
    }
}

/// The packer's job ([`pack_sealed`]): the sealed chunks waiting, the one being packed, and the owners
/// to tell once it ends.
struct Packer {
    rx: crate::memory::queue::Receiver<Sealed>,
    /// Those not begun, the newest last: taken newest first, so in a fill a long session's oldest, which
    /// it lets go of first ([`Events::drain_front`]), are gone before their turn comes rather than packed
    /// for nothing.
    waiting: Vec<Sealed>,
    now: Option<Packing>,
    /// Owners told to keep a chunk compact whose inbox was full then: told again once the job ends.
    later: Vec<Home>,
}

/// A sealed chunk being packed ([`Packer`]): its events fed in so far, and what they deflated to.
struct Packing {
    chunk: Arc<Chunk>,
    home: Option<Home>,
    next: usize,
    deflating: super::packed::Deflating,
}

impl Packer {
    /// A piece of the job: an event at a time until its turn is `over` (another job waits for its thread:
    /// `crate::rt::Turn::over`), or no sealed chunk waits, which ends it with the owners to tell after.
    fn piece(&mut self, over: &dyn Fn() -> bool) -> crate::rt::Step<Vec<Home>> {
        loop {
            while let Ok(sealed) = self.rx.try_recv() {
                self.waiting.push(sealed);
            }
            let Some(p) = self.now.take().or_else(|| self.begin()) else {
                return crate::rt::Step::Done(std::mem::take(&mut self.later));
            };
            self.now = self.feed(p);
            if over() {
                return crate::rt::Step::More;
            }
        }
    }

    /// The newest that's still there and not packed yet, begun.
    fn begin(&mut self) -> Option<Packing> {
        while let Some((chunk, home)) = self.waiting.pop() {
            if let Some(chunk) = chunk.upgrade().filter(|c| c.packed().is_none()) {
                return Some(Packing { chunk, home, next: 0, deflating: super::packed::Deflating::new(Compression::default()) });
            }
        }
        None
    }

    /// `p`'s next event in: its digest made, and its text (after the `,` before it) deflated; back while
    /// some are still to go in. Once all are in, what they deflated to is kept ([`Chunk::pack`]'s bytes)
    /// and its owner told to keep it compact. One packed meanwhile (a snapshot needed it) is done with as
    /// it is; one nothing holds but this (its copy and every view let go of it) is let go of unpacked.
    fn feed(&mut self, mut p: Packing) -> Option<Packing> {
        if p.chunk.packed().is_some() {
            self.tell(p.home);
            return None;
        }
        if Arc::strong_count(&p.chunk) == 1 {
            return None;
        }
        if p.next >= p.chunk.evs.len() {
            let Packing { chunk, home, deflating, .. } = p;
            match deflating.finish() {
                Ok(made) => {
                    chunk.keep(made);
                    self.tell(home);
                }
                Err(e) => tracing::debug!(error = %e, "a sealed chunk wasn't packed: a snapshot packs it if it needs it"),
            }
            return None;
        }
        let fed = match (p.chunk.meta.get(p.next), p.chunk.evs.get(p.next)) {
            (Some(m), Some(e)) => {
                m.sum(e);
                let comma = if p.next == 0 { Ok(()) } else { p.deflating.part(b",") };
                comma.and_then(|()| p.deflating.part(e.entry.text().as_bytes()))
            }
            _ => Err(std::io::Error::other("a chunk's metas and events don't match")),
        };
        if let Err(e) = fed {
            tracing::debug!(error = %e, "a sealed chunk wasn't packed: a snapshot packs it if it needs it");
            return None;
        }
        p.next += 1;
        Some(p)
    }

    /// A packed chunk's owner told to keep it compact: now if its inbox takes it, else once the job ends.
    fn tell(&mut self, home: Option<Home>) {
        if let Some(home) = home
            && let Some(inbox) = home.inbox.upgrade()
            && matches!(inbox.try_send(compact_msg(&home)), Err(async_channel::TrySendError::Full(_)))
        {
            self.later.push(home);
        }
    }
}

/// A session's copy as its owner last published it, whole and immutable: what every reader takes.
pub struct View {
    pub info: Arc<Info>,
    /// Its info as the API answers with it (`GET /api/harness/sessions` and `.../{id}`): encoded by
    /// its owner once per change of it, shared by every view until the next, so no read encodes it
    /// (goal 10, plan 01a1a87b2dae step 2); the events socket's frame of the change is made of it too
    /// (`crate::wire::events::harness_frame`, step 5).
    pub info_json: Bytes,
    pub location: Arc<Location>,
    pub meta: Arc<Value>,
    pub state: Arc<StateDoc>,
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
            info: Arc::clone(&self.info),
            meta: Arc::clone(&self.meta),
            state: Arc::clone(&self.state),
            changes: self.changes.clone(),
            events: self.events.clone(),
            since: None,
            meta_bytes: self.meta_bytes,
            state_bytes: self.state_bytes,
            rev: self.rev,
            lean: false,
            window: None,
            from: 0,
        }
    }

    /// A snapshot for a viewer that takes the session as `view` says (W21-R15, docs/wire.md,
    /// "Windows"): all of it, or its newest events alone, as many as a window holds
    /// (`super::window::start`), and where they begin. `older`: events before what the copy holds may
    /// be kept elsewhere (the server's conversation store), so a window that begins at the copy's
    /// first may still have more before it.
    pub fn parts_for(&self, view: super::registry::View, older: bool) -> Parts {
        let from = if view.window { self.window_start(self.events.len(), super::window::TURNS, view.lean) } else { 0 };
        Parts { lean: view.lean, window: view.window.then(|| self.window_at(from, older)), from, ..self.parts() }
    }

    /// What a viewer that had revision `after` (None: nothing) and takes the session as `view` says
    /// is sent: what it lacks when the copy can say, else a snapshot. To a viewer that takes a window,
    /// what changed since is sent only while it's no more than a window holds; past that it's sent a
    /// window: a resume never costs more than a first load.
    pub fn parts_after(&self, after: Option<u64>, view: super::registry::View, older: bool) -> Parts {
        let since = after.and_then(|rev| self.parts_since(rev)).filter(|p| !view.window || self.fits(p.since.unwrap_or_default(), view.lean));
        match since {
            Some(p) => Parts { lean: view.lean, ..p },
            None => self.parts_for(view, older),
        }
    }

    /// Whether the events changed after revision `after` fit a window's bounds.
    fn fits(&self, after: u64, lean: bool) -> bool {
        let (mut rows, mut bytes) = (0usize, 0usize);
        for (i, m) in self.events.metas().enumerate() {
            if m.at <= after {
                continue;
            }
            rows += 1;
            bytes = bytes.saturating_add(if lean { self.events.get(i).map_or(0, |e| e.entry.sent(true).len()) } else { m.text_len() });
            if rows > super::window::ROWS || bytes > super::window::BYTES {
                return false;
            }
        }
        true
    }

    /// Where a window of the events before `end` begins, holding at most `turns` turns.
    fn window_start(&self, end: usize, turns: usize, lean: bool) -> usize {
        super::window::start_at(end, turns, |i| self.events.get(i).map(|e| (e.entry.sent(lean).len(), e.entry.kind() == Some("user") && e.entry.on_main_thread())))
    }

    /// The number its timeline begins at: where a clear began it again (`State::cleared`, read from the state's own
    /// field: goal 9, plan 01a1a87b2cec step 5 keeps no tree of it), else 1.
    fn floor(&self) -> u64 {
        self.state.get::<u64>("cleared").unwrap_or(1).max(1)
    }

    /// A window (or a page) that begins at event `begin`: that event's number, and whether there's
    /// more before it, in the copy or (`older`) perhaps in the conversation store.
    pub(super) fn window_at(&self, begin: usize, older: bool) -> crate::wire::ui::EventsWindow {
        let before = self.events.meta(begin).and_then(|m| m.seq());
        crate::wire::ui::EventsWindow { before, more: begin > 0 || (older && before.is_some_and(|s| s > self.floor())) }
    }

    /// An older page of its events, for a viewer that has them from number `before` on: those just
    /// before it, as many as a page holds (`super::window::PAGE_TURNS`), as `lean` says, oldest
    /// first. [`super::registry::Paged::Store`] when the copy has none before it.
    pub fn page(&self, before: u64, lean: bool, older: bool) -> super::registry::Paged {
        use super::registry::Paged;
        let end = self.events.partition_point(|m| m.seq().is_some_and(|s| s < before));
        if end == 0 {
            let floor = self.floor();
            return if older && before > floor { Paged::Store { before, floor } } else { Paged::Page { events: Vec::new(), window: crate::wire::ui::EventsWindow { before: None, more: false } } };
        }
        let begin = self.window_start(end, super::window::PAGE_TURNS, lean);
        let events = (begin..end).filter_map(|i| self.events.get(i)).map(|e| e.entry.sent(lean).to_owned()).collect();
        Paged::Page { events, window: self.window_at(begin, older) }
    }

    /// Event `event`, whole, as the copy has it: what a lean one held back (`super::window::lean`).
    pub fn event_text(&self, event: &str) -> Option<String> {
        // Its meta's id, kept in the meta (goal 11, plan 01a1a87b2e56 step 5).
        let i = self.events.metas().position(|m| m.id() == event)?;
        self.events.get(i).map(|e| e.entry.text().to_owned())
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
        let mut events: Vec<(u64, u64)> = Vec::new();
        for c in self.events.chunks() {
            let metas = c.metas();
            // A digest made is taken as it is (a packed chunk's are all made: packing makes them); a
            // chunk's events are read only for the digests still missing, as a compact chunk gives them
            // by inflating its segment (goal 11, plan 01a1a87b2e56 step 2).
            let evs = if metas.iter().any(|m| !m.id().is_empty() && m.sum_made().is_none()) {
                match c.evs() {
                    Ok(evs) => Some(evs),
                    Err(e) => {
                        // Said less of than it has: its runner sends those events again.
                        tracing::warn!(session, error = %e, "a chunk's events couldn't be read for their digests");
                        None
                    }
                }
            } else {
                None
            };
            for (i, m) in metas.iter().enumerate().filter(|(_, m)| !m.id().is_empty()) {
                let sum = m.sum_made().or_else(|| evs.as_ref().and_then(|evs| evs.get(i)).map(|e| m.sum(e)));
                if let (Some(seq), Some(sum)) = (m.seq(), sum) {
                    events.push((seq, sum));
                }
            }
        }
        events.sort_unstable_by_key(|(seq, _)| *seq);
        crate::wire::sums::kept(session.to_owned(), self.info.epoch, &events)
    }

    /// `{"events":[…]}` of its newest `limit` events numbered before `before` (when given), oldest
    /// first: what the conversation store answers `GET /api/harness/sessions/{id}/events` with
    /// (`crate::convo::store::Store::events`), made of the events' texts as each was written once,
    /// joined, so the read neither touches the store nor encodes anything (goal 10, plan 01a1a87b2dae
    /// step 2). None when the copy can't say all of it: it holds fewer than `limit` of them and not
    /// its timeline from the first event (its oldest let go of, a conversation cleared, a copy that
    /// went on from part of it), or one of them has no number or id. Fresher than the store's while
    /// an event streams: the store keeps an event as it last came whole.
    pub fn events_json(&self, before: Option<u64>, limit: usize) -> Option<Bytes> {
        let parts = self.events_page(before, limit)?;
        let mut out = Vec::with_capacity(parts.iter().map(Bytes::len).sum());
        for p in &parts {
            out.extend_from_slice(p);
        }
        Some(Bytes::from(out))
    }

    /// [`View::events_json`] in the pieces it's made of, in order, for an answer that sends them as they are
    /// (`crate::http::server::pieces`): a compact chunk's part a slice of its texts as the reads of its newest
    /// page keep them, inflated once for all of those reads, so a read neither inflates nor copies them (goal
    /// 10, plan 01a1a87b2dae step 5: the lab's events reads, at p99 1.1 to 2.0 ms on rt5 calm, inflated a
    /// whole chunk's segment each, on the request's shard).
    pub fn events_page(&self, before: Option<u64>, limit: usize) -> Option<Vec<Bytes>> {
        let end = match before {
            Some(b) => self.events.partition_point(|m| m.seq().is_some_and(|s| s < b)),
            None => self.events.len(),
        };
        let start = end.saturating_sub(limit);
        if end - start < limit && self.events.meta(0).is_none_or(|m| m.seq() != Some(1)) {
            return None;
        }
        for i in start..end {
            let m = self.events.meta(i)?;
            if m.seq().is_none() || m.id().is_empty() {
                return None;
            }
        }
        let mut parts = vec![Bytes::from_static(br#"{"events":["#)];
        self.events.pieces_into(start..end, &mut parts)?;
        parts.push(Bytes::from_static(b"]}"));
        Some(parts)
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
    /// not a copy of it). The state kept field by field, each field's JSON ([`StateDoc`]; goal 9, plan
    /// 01a1a87b2cec step 5).
    pub(super) meta: Arc<Value>,
    pub(super) state: Arc<StateDoc>,
    /// The state and the meta as its machine last said them, each with what was made of it (goal 9, plan
    /// 01a1a87b2cec step 5): the same one again isn't made again while the copy's is still that one
    /// (`super::registry::Registry::apply`). Held here too, each is copied before any change, so a
    /// changed one is told apart by its pointer.
    pub(super) state_of: Option<(Box<super::model::State>, Arc<StateDoc>)>,
    pub(super) meta_of: Option<(Box<super::model::Meta>, Arc<Value>)>,
    /// The state as its viewers last had it sent (what a state patch is made against).
    pub(super) sent: Arc<StateDoc>,
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
    /// Where each event is, by its id ([`Index`]: goal 11, plan 01a1a87b2e56 step 5).
    pub(super) index: Index,
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
    /// The events socket's feed's number of its info's last `harness` frame (0: none yet), which its next lets
    /// go of (`crate::hub::Hub::emit_harness`; goal 11, plan 01a1a87b2e56 step 5).
    pub(super) harness_at: u64,
    /// When this server opened it, until its machine announces it: a report from the machine
    /// that left it out was made before its open got there, and doesn't end it.
    pub(super) opened_at: Option<std::time::Instant>,
    /// What its readers find of it.
    slot: Arc<Slot>,
    /// Its info's encoding ([`View::info_json`]), and the info it's of: made again when that changes.
    info_json: (Arc<Info>, Bytes),
    /// How often its info and meta were encoded.
    pub(super) encodings: Encodings,
    /// Its owner's inbox, held weakly (none until a shard takes it in): a chunk packed off the shard
    /// comes back through it to be kept compact (goal 11, plan 01a1a87b2e56 step 2).
    owner: Option<crate::memory::queue::WeakSender<Msg>>,
    /// Where its sealed chunks are spilled (none: they're kept in memory), the file they go to now, and
    /// whether a spill is under way (one at a time): goal 11, plan 01a1a87b2e56 step 5.
    spill: Option<Arc<super::spill::Dir>>,
    spill_file: Option<Arc<super::spill::File>>,
    spilling: bool,
    /// Its last spill was refused (said once, until one is written again).
    spill_refused: bool,
}

impl Drop for Mirror {
    fn drop(&mut self) {
        let mut now = COPIES.load(Ordering::Relaxed);
        while let Err(seen) = COPIES.compare_exchange_weak(now, now.saturating_sub(1), Ordering::Relaxed, Ordering::Relaxed) {
            now = seen;
        }
    }
}

/// An info as the API answers with it: `serde_json`'s compact form (what `srv::json` wrote of it).
fn info_json(info: &Info) -> Bytes {
    serde_json::to_vec(info).map_or_else(|_| Bytes::from_static(b"null"), Bytes::from)
}

/// `info`'s encoding, `was`'s being `last` (goal 9, plan 01a1a87b2cec step 5): where nothing it says but its
/// `last_activity` changed ([`Info::differs`] compares all the rest; the other two it leaves out, `queue` and `said`,
/// aren't in its JSON), `last` with that number said anew ([`with_last_activity`]), else written whole, into room for
/// `last`'s bytes (they change little), never grown from serde_json's 128. Every state a session's machine sends sets
/// `last_activity`, so its info was written whole at each: rt7's calm profile had that at 0.10% of the process, the
/// most a copy's publish spent but letting the last view go.
fn info_json_after(info: &Info, was: &Info, last: &[u8]) -> Bytes {
    if !info.differs(was)
        && let Some(json) = with_last_activity(last, was.last_activity, info.last_activity)
    {
        return json;
    }
    let mut out = Vec::with_capacity(last.len());
    match serde_json::to_writer(&mut out, info) {
        Ok(()) => Bytes::from(out),
        Err(_) => Bytes::from_static(b"null"),
    }
}

/// `json`, an info's encoding whose `last_activity` is `old`, saying `new` there instead: the same bytes but that
/// number's, written as `serde_json` writes an `i64`. None unless its key and number are found once, whole (followed by
/// `,` or `}`), so nothing else is taken for them: the key's quotes can't appear inside a JSON string unescaped, and a
/// nested `last_activity` of the same value makes two.
fn with_last_activity(json: &[u8], old: i64, new: i64) -> Option<Bytes> {
    use std::io::Write as _;
    const KEY: &[u8] = br#""last_activity":"#;
    // The key and its number, on the stack: an `i64` is at most 20 characters, its sign among them.
    let mut said = [0u8; KEY.len() + 20];
    let room = said.len();
    let mut at_end = &mut said[..];
    at_end.write_all(KEY).ok()?;
    write!(at_end, "{old}").ok()?;
    let len = room - at_end.len();
    let needle = said.get(..len)?;
    let mut found = memchr::memmem::find_iter(json, needle).filter(|&at| matches!(json.get(at + len), Some(b',' | b'}')));
    let at = found.next()?;
    if found.next().is_some() {
        return None;
    }
    let mut digits = [0u8; 20];
    let mut rest = &mut digits[..];
    write!(rest, "{new}").ok()?;
    let n = 20 - rest.len();
    let mut made = Vec::with_capacity(json.len() - len + KEY.len() + n);
    made.extend_from_slice(json.get(..at + KEY.len())?);
    made.extend_from_slice(digits.get(..n)?);
    made.extend_from_slice(json.get(at + len..)?);
    Some(Bytes::from(made))
}

/// How many times a copy encoded its info (for its view, which the API's reads of it and the events
/// socket's frame of it take as it is) and its meta (for its viewers' frame): once a change of each (goal
/// 10, plan 01a1a87b2dae step 5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encodings {
    pub info: u64,
    pub meta: u64,
}

/// Where a packed chunk's copy is: its owner's inbox and its session, to keep it compact once packed.
#[derive(Clone)]
pub(super) struct Home {
    inbox: crate::memory::queue::WeakSender<Msg>,
    id: Arc<str>,
}

impl Mirror {
    pub fn new(info: Info, location: Location, bus: Arc<dyn kiln_seams::Bus>) -> Self {
        let feed = Feed::open(bus, &info.id);
        let (info, location) = (Arc::new(info), Arc::new(location));
        let encoded = info_json(&info);
        let view = View {
            info: Arc::clone(&info),
            info_json: encoded.clone(),
            location: Arc::clone(&location),
            meta: null(),
            state: super::state_doc::none(),
            changes: None,
            events: Events::default(),
            rev: 0,
            reset_at: 0,
            meta_bytes: 0,
            state_bytes: 0,
            closed: false,
        };
        let slot = Arc::new(Slot { id: Arc::from(info.id.as_str()), view: ArcSwap::from_pointee(view), tap: feed.tap() });
        let encoding = (Arc::clone(&info), encoded);
        COPIES.fetch_add(1, Ordering::Relaxed);
        Self {
            info,
            reported: String::new(),
            location,
            meta: null(),
            state: super::state_doc::none(),
            state_of: None,
            meta_of: None,
            sent: super::state_doc::none(),
            state_bytes: 0,
            patch_bytes: 0,
            meta_bytes: 0,
            changes: None,
            changes_sent: None,
            events: Events::default(),
            index: Index::default(),
            feed,
            rev: 0,
            reset_at: 0,
            started_over_pending: false,
            harness_at: 0,
            opened_at: None,
            slot,
            info_json: encoding,
            encodings: Encodings::default(),
            owner: None,
            spill: None,
            spill_file: None,
            spilling: false,
            spill_refused: false,
        }
    }

    /// Where its chunks come back to once packed, once a shard took it in.
    fn home(&self) -> Option<Home> {
        Some(Home { inbox: self.owner.clone()?, id: Arc::clone(&self.slot.id) })
    }

    /// Its sealed chunks past its newest [`EVENTS_PAGE`] events, once packed, written to its spill file off
    /// the runtime's threads (the blocking pool), then kept as their metas and where they went: its owner
    /// swaps them in as the write comes back (`Shard::spilled`), a spill at a time (goal 11, plan
    /// 01a1a87b2e56 step 5). None is let go of until its bytes are written and synced; a write that fails
    /// leaves them in memory.
    fn spill_old(&mut self) {
        let (Some(dir), Some(home)) = (self.spill.clone(), self.home()) else { return };
        if self.spilling || !crate::rt::on_shard() {
            return;
        }
        let chunks = self.events.spillable(EVENTS_PAGE, PAGE_BYTES);
        let segments: Vec<Arc<Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        if chunks.is_empty() || segments.len() != chunks.len() {
            return;
        }
        self.spilling = true;
        let file = self.spill_file.clone();
        drop(crate::rt::spawn(async move {
            let id = Arc::clone(&home.id);
            let written = match crate::rt::blocking(move || super::spill::write(&dir, file, &id, &segments)).await {
                Ok(written) => written,
                Err(e) => Err(std::io::Error::other(format!("{e:#}"))),
            };
            if let Some(inbox) = home.inbox.upgrade() {
                let id = Arc::clone(&home.id);
                let _ = inbox.send(Msg::Run(Box::new(move |s: &mut Shard| s.spilled(&id, &chunks, written)))).await;
            }
        }));
    }

    /// The copy as it is now, for its readers.
    pub fn view(&self) -> View {
        let (of, encoded) = &self.info_json;
        View {
            info: Arc::clone(&self.info),
            // Encoded as it was published ([`Mirror::publish`]), or now, should the info have changed
            // since without a publish.
            info_json: if Arc::ptr_eq(of, &self.info) { encoded.clone() } else { info_json(&self.info) },
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

    /// Its readers take the copy as it is now: its view swapped in whole, its info encoded again
    /// first if it changed ([`info_json_after`]: its last encoding's number changed alone, where that's all
    /// that changed).
    pub fn publish(&mut self) {
        if !Arc::ptr_eq(&self.info_json.0, &self.info) {
            let json = info_json_after(&self.info, &self.info_json.0, &self.info_json.1);
            self.info_json = (Arc::clone(&self.info), json);
            self.encodings.info += 1;
        }
        self.slot.view.store(Arc::new(self.view()));
    }

    /// Its info as it was last published, encoded ([`View::info_json`]): what the events socket's frame
    /// of a change of it is made of, rather than encoded again (`crate::wire::events::harness_frame`;
    /// goal 10, plan 01a1a87b2dae step 5).
    pub fn info_encoded(&self) -> &Bytes {
        &self.info_json.1
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
                self.sent = super::state_doc::none();
                self.changes_sent = None;
            }
        }
    }

    /// The state as a frame for its viewers: whole for those that take it so (or when there's
    /// nothing to patch), and as what changed in it since the last one for those that take that;
    /// a state none takes whole goes as its patch alone. Made only while someone watches
    /// ([`Mirror::commit`]).
    pub fn state_frame(&mut self) -> Option<Payload> {
        // The state its viewers were last sent, not changed since (the same one again, goal 9, plan 01a1a87b2cec
        // step 5): nothing to patch, as its patch would say, without comparing each field with itself.
        if Arc::ptr_eq(&self.sent, &self.state) && self.state.is_object() {
            return None;
        }
        let rev = Some(self.rev);
        let (mut p, whole, patched) = {
            // Its fields compared with those last sent as their JSON's bytes (goal 9, plan 01a1a87b2cec step 5).
            let patch = self.state.patch(&self.sent);
            // A state that changed nothing its viewers were last sent goes as nothing: no field to patch,
            // and those that take it whole have it (goal 10, plan 01a1a87b2dae step 5).
            if patch.as_ref().is_some_and(|(set, gone)| set.is_empty() && gone.is_empty()) {
                return None;
            }
            let whole = patch.is_none() || self.feed.wants_whole(PatchOf::State);
            let room = if whole { self.state_bytes } else { 0 } + if patch.is_some() { self.patch_bytes } else { 0 };
            let mut p = Payload::new(self.rev, room);
            let start = p.text().len();
            if whole {
                let _ = write::state(p.text(), &self.state, rev);
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
                self.encodings.meta += 1;
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
            files.retain(|f| kept.contains(&f.file()));
        }
        merge_changes(&mut files, set, if whole { &[] } else { gone.as_slice() });
        self.changes = Some(Arc::new(files));
    }

    pub fn reindex(&mut self) {
        self.index = Index::of(&self.events);
    }

    /// Keeps its events through number `n` and lets go of those after it, and of any without a number: its
    /// machine's runner started again and goes on from there (an Open's `since`, `Registry::apply`). Those kept
    /// stay where they were, its chunks with them, and its index lets go of the rest alone, nothing read again
    /// or indexed anew; where those it keeps aren't its first ones (never, as a copy takes them in by number)
    /// its events and index are made again (goal 9, plan 01a1a87b2cec step 3).
    pub fn keep_through(&mut self, n: u64) {
        let keeps = |m: &Meta| m.seq().is_some_and(|q| q <= n);
        match self.events.keep_first(keeps) {
            Some(kept) => self.index.retain(|at| *at < kept),
            None => {
                self.events.retain(keeps);
                self.reindex();
            }
        }
    }

    /// The compact chunk that taking event `id` numbered `seq` would read back to change (`upsert`: an event it has under
    /// that id, or under that number with another id), if one would be.
    pub(super) fn thaw_for(&self, id: &str, seq: u64) -> Option<Arc<Chunk>> {
        let at = match self.index.get(&self.events, id) {
            Some(at) => at,
            None => {
                let top = self.events.len().checked_sub(1).and_then(|i| self.events.meta(i)).and_then(|m| m.seq())?;
                if seq > top {
                    return None;
                }
                let at = self.events.partition_point(|m| m.seq().is_some_and(|s| s < seq));
                self.events.meta(at).filter(|m| m.seq() == Some(seq))?;
                at
            }
        };
        let (c, _) = self.events.place(at)?;
        self.events.chunks().get(c).filter(|c| c.is_compact()).cloned()
    }

    /// Where taking event `id` numbered `seq` would put it in among the copy's events (`upsert`: one it has under
    /// neither that id nor that number, numbered before its newest), if it would: a place every event after which
    /// moves along, each chunk made again ([`Events::insert`]).
    pub(super) fn insert_at(&self, id: &str, seq: u64) -> Option<usize> {
        if self.index.get(&self.events, id).is_some_and(|at| at < self.events.len()) {
            return None;
        }
        let top = self.events.len().checked_sub(1).and_then(|i| self.events.meta(i)).and_then(|m| m.seq())?;
        if seq > top {
            return None;
        }
        let at = self.events.partition_point(|m| m.seq().is_some_and(|s| s < seq));
        if self.events.meta(at).is_some_and(|m| m.seq() == Some(seq)) {
            return None;
        }
        Some(at)
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
        let Some(id) = e.id().map(str::to_owned) else { return false };
        let ev = Ev { entry: e, at: self.rev };
        let found = self.index.get(&self.events, &id);
        if found.is_some_and(|i| self.events.change(i, |slot| *slot = ev.clone())) {
            return false;
        }
        if let (Some(q), Some(top)) = (ev.entry.seq(), self.events.len().checked_sub(1).and_then(|i| self.events.meta(i)).and_then(|m| m.seq()))
            && q <= top
        {
            let at = self.events.partition_point(|m| m.seq().is_some_and(|s| s < q));
            if let Some(was) = self.events.meta(at).filter(|m| m.seq() == Some(q)).map(|m| m.id().to_owned()) {
                // Another id under that number, in its place: the index lets go of that one there and finds this one, the
                // rest of it as it was (goal 9, plan 01a1a87b2cec step 3: it was made again, every event's id hashed, on the
                // owner's shard). One that couldn't be changed leaves the copy, and so its index, as they were.
                let displaced = self.events.change(at, |slot| *slot = ev.clone());
                if displaced {
                    self.index.forget(&was, at);
                    self.index.insert(&self.events, &id, at);
                }
                return displaced;
            }
            self.events.insert(at, ev);
            self.reindex();
            return false;
        }
        let at = self.events.len();
        let sealed = self.events.push(ev);
        self.index.insert(&self.events, &id, at);
        if let Some(sealed) = sealed {
            seal(&sealed, self.home());
        }
        if self.events.len() > MAX_EVENTS + MAX_EVENTS / 4 {
            // From where a run of the digests begins, as its machine's timeline lets go of its
            // oldest (`crate::wire::sums::run_after`).
            let over = self.events.len() - MAX_EVENTS;
            let drop = match self.events.meta(over).and_then(|m| m.seq()) {
                Some(q) => {
                    let from = crate::wire::sums::run_after(q);
                    self.events.partition_point(|m| m.seq().is_some_and(|s| s < from))
                }
                None => over,
            };
            // Those let go leave the index, and the rest move down by as many: the index isn't
            // made again (a long session lets go of its oldest every thousand events or so). The
            // chunks after the cut stay as they were, packed; the one it fell in is made again, and
            // packed in its turn: off the shard where it's compact, taken in by its owner after (goal 9,
            // plan 01a1a87b2cec step 3), its first ones kept till then.
            let later = self.home().filter(|_| crate::rt::on_shard()).and_then(|home| self.events.drain_whole(drop).map(|d| (home, d)));
            let (gone, remade) = match later {
                Some((home, (gone, cut, left))) => {
                    trim_later(home, cut, left);
                    (gone, None)
                }
                None => self.events.drain_front(drop),
            };
            self.index.drop_front(gone);
            if let Some(chunk) = remade {
                seal(&chunk, self.home());
            }
        }
        let home = self.home();
        self.events.compact(home.as_ref());
        self.spill_old();
        false
    }

    /// Text streamed into event `id`, at the copy's revision. An event a view still holds is copied
    /// first (that one alone), so the view keeps what it took.
    pub fn append(&mut self, id: &str, text: &str) {
        let rev = self.rev;
        if let Some(i) = self.index.get(&self.events, id) {
            self.events.change(i, |ev| {
                if Arc::make_mut(&mut ev.entry).append_text(text) {
                    ev.at = rev;
                }
            });
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
        self.view().parts_since(after).map(|parts| super::registry::Snapshot { parts, files, store: None }.frames())
    }
}

/// What a shard's owner holds: its sessions' copies, by id, and every shard's board (its own to
/// publish, the others' to read).
pub struct Shard {
    me: usize,
    /// Its own inbox, held weakly: what its copies' packed chunks come back through.
    inbox: crate::memory::queue::WeakSender<Msg>,
    mirrors: HashMap<String, Mirror>,
    boards: Arc<[ArcSwap<Board>]>,
    bus: Arc<dyn kiln_seams::Bus>,
    /// A session came or went since its board was last published.
    board_changed: bool,
    /// Where its copies' sealed chunks are spilled, once set (`Copies::spill_to`).
    spill: Arc<OnceLock<Arc<super::spill::Dir>>>,
    /// A compact chunk of a session's copy that an update's next event changes, to be read back off the shard before it's
    /// taken ([`Shard::want_thaw`]; goal 9, plan 01a1a87b2cec step 3).
    thaw: Option<(String, Arc<Chunk>)>,
    /// The chunk last read back so (or tried): one that's still compact as an update comes back to it is changed where it
    /// is, as before, so an update always goes on.
    thawed: Weak<Chunk>,
    /// An event an update takes in under an older number than a session's copy's newest, with where it goes and the
    /// copy's number then: the copy's events are made again with it in, off the shard, before the update goes on
    /// ([`Shard::want_insert`]; goal 9, plan 01a1a87b2cec step 3).
    insert: Option<(String, usize, super::model::Event, u64)>,
    /// The session and event last put in so (or tried): one still to be put in as an update comes back to it is put in
    /// where it is, as before, so an update always goes on.
    inserted: Option<(String, String)>,
    /// An event an update takes that's long to write out (a large tool output, [`long_to_write`]), the first of what's left
    /// of the update: written out off the shard before the update goes on ([`Shard::want_write`]; goal 9, plan 01a1a87b2cec
    /// step 3).
    write: Option<Writing>,
    /// What was made of the event last written out so, or tried, for the update's next take alone ([`Shard::take_written`]).
    written: Option<Written>,
    /// Events written out so ([`Shard::written_off`]).
    written_off: u64,
}

/// What its owner needs to write an event of session `session` out off the shard ([`Shard::want_write`]): whether a row of it
/// is kept for the conversation store, and where the session works, for the files the event mentions.
struct Writing {
    session: String,
    keep: bool,
    cwd: String,
    location: Location,
}

/// An event of a session's update written out off its owner's shard ([`Shard::want_write`]), or tried: what was made of it,
/// for the update to take as it comes back to it (`Registry::apply`).
pub(super) struct Written {
    session: String,
    id: String,
    seq: u64,
    /// None when it couldn't be written there: it's written where it is, as before, so an update always goes on.
    pub(super) made: Option<Made>,
}

impl Written {
    /// Whether it's session `session`'s event `e`.
    pub(super) fn is(&self, session: &str, e: &super::model::Event) -> bool {
        self.session == session && self.id == e.id && self.seq == e.seq
    }
}

/// An event written out: its entry, its row for the conversation store (where one is kept), and the files it mentions
/// (`super::registry::mentions`), each made as `Registry::apply` makes them.
pub(super) struct Made {
    pub(super) entry: Arc<Entry>,
    pub(super) row: Option<crate::convo::rows::Row>,
    pub(super) mentions: Option<super::kept::Job>,
}

/// Event `e` of session `session` written out as `Registry::apply` writes it ([`Made`]): its row kept when `keep`, the files it
/// mentions found as a session working in `cwd` at `location` finds them.
fn made_of(session: &str, e: &super::model::Event, keep: bool, cwd: &str, location: &Location) -> Made {
    let mut row = None;
    let entry = if keep {
        Entry::of_event(e, Some(&mut |text: String| row = Some(crate::convo::rows::Row::Event(Box::new(crate::convo::rows::event_written(session, e, text))))))
    } else {
        Entry::of_event(e, None)
    };
    Made { entry: Arc::new(entry), row, mentions: super::registry::mentions(session, cwd, location, e) }
}

/// What writing an event out takes by its bytes (`super::weigh::event`), as the owners write them on their shards and off
/// (goal 9, plan 01a1a87b2cec step 3): moving averages of each one's bytes and of how long it took, nanoseconds, a new
/// sample's weight 1/8 as RFC 6298 smooths a round trip, as `crate::wire::payload` learns what reading a runner's frame
/// takes. Every owner learns into it, a load and a store each: a sample lost to another owner's at the same moment is one
/// of many.
static WRITES: Rate = Rate::new();

/// A rate learned by moving averages of samples' sizes and times ([`WRITES`]).
struct Rate {
    bytes: AtomicU64,
    ns: AtomicU64,
}

impl Rate {
    const fn new() -> Self {
        Self { bytes: AtomicU64::new(0), ns: AtomicU64::new(0) }
    }

    /// Nanoseconds a sample of `bytes` is expected to take, at the rate learned: none until one was taken.
    fn expected(&self, bytes: u64) -> u64 {
        bytes.saturating_mul(self.ns.load(Ordering::Relaxed)).checked_div(self.bytes.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// A sample of `bytes` took `ns`.
    fn learn(&self, bytes: u64, ns: u64) {
        // x ← x + (sample − x)/8, in whole bytes and nanoseconds.
        let smooth = |avg: u64, sample: u64| if avg == 0 { sample } else { avg - avg / 8 + sample / 8 };
        self.bytes.store(smooth(self.bytes.load(Ordering::Relaxed), bytes), Ordering::Relaxed);
        self.ns.store(smooth(self.ns.load(Ordering::Relaxed), ns), Ordering::Relaxed);
    }
}

/// Whole nanoseconds of `d`.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Nanoseconds writing event `e` out is expected to take ([`WRITES`]).
pub(super) fn write_ns(e: &super::model::Event) -> u64 {
    WRITES.expected(u64::try_from(super::weigh::event(e)).unwrap_or(u64::MAX))
}

/// Whether writing event `e` out is expected to hold its owner's shard past a slow poll (`hyper_rt::slow::SLOW_POLL_NS`), as a
/// runner's frame expected to is read off the shard (`crate::wire::payload`): it's written on the compute pool instead
/// ([`Shard::want_write`]).
pub(super) fn long_to_write(e: &super::model::Event) -> bool {
    write_ns(e) > hyper_rt::slow::SLOW_POLL_NS
}

/// Event `e` was written out in `took`, on its owner's shard ([`WRITES`]).
pub(super) fn wrote(e: &super::model::Event, took: Duration) {
    WRITES.learn(u64::try_from(super::weigh::event(e)).unwrap_or(u64::MAX), nanos(took));
}

impl Shard {
    pub fn bus(&self) -> &Arc<dyn kiln_seams::Bus> {
        &self.bus
    }

    /// Session `id`'s update stopped before an event that changes one in compact chunk `chunk` (`Registry::apply`): its
    /// owner reads that chunk back on the blocking pool before it takes the rest ([`own`]), where it was read back on
    /// this shard, its every event inflated and parsed, for each such change (goal 9, plan 01a1a87b2cec step 3).
    pub(super) fn want_thaw(&mut self, id: &str, chunk: Arc<Chunk>) {
        self.thaw = Some((id.to_owned(), chunk));
    }

    /// The chunk last read back for a change ([`Shard::want_thaw`]), or tried: a change that finds it still compact is
    /// made where it is.
    pub(super) fn thawed(&self) -> Weak<Chunk> {
        Weak::clone(&self.thawed)
    }

    /// Chunk `was` of session `id`'s copy, read back (`made`: the same events, kept as they are): it takes its place where
    /// it's still the copy's, as it was. Whether it did.
    fn take_thawed(&mut self, id: &str, was: &Arc<Chunk>, made: Chunk) -> bool {
        self.thawed = Arc::downgrade(was);
        self.mirrors.get_mut(id).is_some_and(|m| m.events.swap_chunk(was, made))
    }

    /// Session `id`'s update stopped before event `e`, which goes in at `at` among its copy's events, numbered before its
    /// newest (`Registry::apply`): its owner makes the copy's events again with it in, on the blocking pool, before it
    /// takes the rest ([`own`]), where they were made again on this shard, every chunk read again (goal 9, plan
    /// 01a1a87b2cec step 3). `rev`: the copy's number as it's put in.
    pub(super) fn want_insert(&mut self, id: &str, at: usize, e: super::model::Event, rev: u64) {
        self.insert = Some((id.to_owned(), at, e, rev));
    }

    /// The event of session `id` last put in off the shard ([`Shard::want_insert`]), or tried: one still to be put in as
    /// an update comes back to it is put in where it is.
    pub(super) fn tried_insert(&self, id: &str) -> Option<String> {
        self.inserted.as_ref().filter(|(s, _)| s == id).map(|(_, e)| e.clone())
    }

    /// Session `id`'s copy's events made again with an event put in (`made`, from `was`, and their index): taken where
    /// its events are still `was`'s. Whether they were.
    fn take_inserted(&mut self, id: &str, was: &Events, made: Events, index: Index) -> bool {
        let Some(m) = self.mirrors.get_mut(id) else { return false };
        if !m.events.same_as(was) {
            return false;
        }
        m.events = made;
        m.index = index;
        true
    }

    /// Session `id`'s update stopped before an event that's long to write out ([`long_to_write`]: a large tool output), the
    /// first of what's left of it (`Registry::apply`): its owner writes it out on the compute pool before it takes the rest
    /// ([`own`]), where it was written on this shard, its JSON and its row's text, past the step's quantum (goal 9, plan
    /// 01a1a87b2cec step 3). `keep`: a row of it is kept for the conversation store. `cwd` and `location`: where the session
    /// works, for the files the event mentions.
    pub(super) fn want_write(&mut self, id: &str, keep: bool, cwd: String, location: Location) {
        self.write = Some(Writing { session: id.to_owned(), keep, cwd, location });
    }

    /// What was made of session `id`'s event written out off the shard ([`Shard::want_write`]), or tried, for its update to take
    /// as it comes back to it: its owner's next take alone has it.
    pub(super) fn take_written(&mut self, id: &str) -> Option<Written> {
        self.written.take().filter(|w| w.session == id)
    }

    /// How many events of its sessions' updates this owner wrote out off its shard ([`Shard::want_write`]).
    pub(super) fn written_off(&self) -> u64 {
        self.written_off
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
    pub fn insert(&mut self, id: String, mut m: Mirror) -> bool {
        match self.mirrors.entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                m.owner = Some(self.inbox.clone());
                m.spill = self.spill.get().cloned();
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
                let m = v.insert(make());
                m.owner = Some(self.inbox.clone());
                m.spill = self.spill.get().cloned();
                m
            }
        }
    }

    /// Session `id`'s copy kept compact, its chunks packed since kept as their segments (goal 11, plan
    /// 01a1a87b2e56 step 2), and its readers given the view of it that is.
    pub fn compact(&mut self, id: &str) {
        if let Some(m) = self.mirrors.get_mut(id) {
            let home = m.home();
            m.events.compact(home.as_ref());
            m.spill_old();
            m.publish();
        }
    }

    /// A spill of session `id`'s chunks come back (`Mirror::spill_old`): written, each chunk still its
    /// copy's as it was swapped for its spilled form, its readers given the view of that, and what sealed
    /// meanwhile spilled in turn; refused, they stay in memory as they were, said (goal 11, plan 01a1a87b2e56
    /// step 5). Either way the next spill may go.
    fn spilled(&mut self, id: &str, chunks: &[Arc<Chunk>], written: std::io::Result<(Arc<super::spill::File>, Vec<super::spill::Spilled>)>) {
        let Some(m) = self.mirrors.get_mut(id) else { return };
        m.spilling = false;
        match written {
            Ok((file, spilled)) => {
                m.spill_refused = false;
                m.spill_file = Some(file);
                if m.events.spilled(chunks, spilled) > 0 {
                    m.publish();
                }
                m.spill_old();
            }
            Err(e) => {
                if !std::mem::replace(&mut m.spill_refused, true) {
                    tracing::warn!(session = id, error = %e, "a session copy's sealed chunks couldn't be spilled: they stay in memory (said again once a spill is written)");
                }
            }
        }
    }

    /// The chunk session `id`'s cut fell in, made again off the shard of its events past its first `left`
    /// (`made`; [`trim_later`]): taken in where it's still the copy's first as it was, those first ones let go
    /// of and the rest moved down by as many in its index, the new chunk sealed to be packed in its turn, and
    /// its readers given the view of it (goal 9, plan 01a1a87b2cec step 3). Else the copy keeps them till its
    /// next cut.
    fn trimmed(&mut self, id: &str, was: &Arc<Chunk>, left: usize, made: Chunk) {
        let Some(m) = self.mirrors.get_mut(id) else { return };
        let Some(gone) = m.events.trim_first(was, left, made) else { return };
        m.index.drop_front(gone);
        if m.events.chunks().len() > 1
            && let Some(first) = m.events.chunks().first()
        {
            seal(first, m.home());
        }
        m.publish();
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

    /// Takes one message, a panic in it caught (its shard goes on with the rest): what's left of it when
    /// its owner's turn ran out first (`over`, asked with what the next piece is expected to take; an update's
    /// events, `Registry::apply`), to take next.
    fn take(&mut self, msg: Msg, hub: Option<&Arc<Hub>>, over: &dyn Fn(u64) -> bool) -> Option<Msg> {
        let mut rest = None;
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
            Msg::Update(i) => {
                if let Some(h) = hub {
                    rest = h.harness.apply(self, h, *i, over).map(|i| Msg::Update(Box::new(i)));
                }
            }
            Msg::Run(job) => job(self),
            Msg::Runner(r) => {
                if let Some(h) = hub {
                    rest = h.runner_update(self, *r, over).map(|i| Msg::Update(Box::new(i)));
                }
            }
        }));
        if let Err(p) = done {
            crate::metrics::global().panics_caught.inc();
            tracing::error!(shard = self.me, panic = %crate::sup::panic_message(p.as_ref()), "a change to an agent session's copy panicked: its shard goes on");
        }
        self.publish_board();
        rest
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

/// An agent session's update as one of its machine's runner processes sent it, for the shard that owns
/// the session's copy to judge (whether it counts, which of the machine's processes holds the session)
/// and take, in the order its runner sent it (goal 10, plan 01a1a87b2dae step 4; `Hub::runner_update`).
/// Every runner's updates were judged on the hub's one loop, on the HTTP listener's shard, behind every
/// runner's other messages (the lab's runner wait at p99, 155 to 195 ms).
pub struct RunnerUpdate {
    /// The runner process it came from, and its machine.
    pub peer: hyper_transport::PeerId,
    pub agent: String,
    pub session: String,
    pub update: Update,
    /// Its machine keeps the session in a store it sends the server's (`crate::convo::feed`), as its
    /// process's caps said when it came.
    pub shipped: bool,
    /// When its frame came off the wire: what its wait to be taken is measured from (`crate::route_latency`).
    pub arrived: Option<std::time::Instant>,
    /// When the hub's loop handed it to this owner's inbox: its wait's last leg is from there
    /// (`crate::route_latency::RunnerLeg::ToOwner`).
    pub handed: Option<std::time::Instant>,
    /// How long before its frame was made its runner took it in, where its runner said: a duration on the
    /// runner's clock (`crate::wire::stamp`; goal 10, plan 01a1a87b2dae step 5), its leg beside the server's
    /// (`crate::route_latency::RunnerLeg::Runner`, `Server`).
    pub taken: Option<std::time::Duration>,
}

type Job = Box<dyn FnOnce(&mut Shard) + Send>;

/// What an owner takes from its inbox.
pub enum Msg {
    Update(Box<Ingest>),
    /// Work on its copies (people's commands, a machine lost, a session opened or closed…).
    Run(Job),
    /// An update from a session's runner, to judge and take ([`RunnerUpdate`]).
    Runner(Box<RunnerUpdate>),
}

impl crate::memory::queue::Weigh for Msg {
    /// Its place, and an update's ingest with the texts its update carries (`super::weigh`); a job's
    /// closure as it's kept (what it captured, by value).
    fn weight(&self) -> usize {
        size_of::<Msg>().saturating_add(match self {
            Msg::Update(i) => size_of::<Ingest>().saturating_add(i.id.len()).saturating_add(i.reported.len()).saturating_add(crate::memory::queue::Weigh::weight(&i.u)),
            Msg::Run(job) => size_of_val(&**job),
            Msg::Runner(r) => size_of::<RunnerUpdate>().saturating_add(r.agent.len()).saturating_add(r.session.len()).saturating_add(crate::memory::queue::Weigh::weight(&r.update)),
        })
    }
}

/// The shard that owns session `id` among `shards`: FNV-1a over its id, so every process with as
/// many shards puts it in the same place.
pub fn owner_of(id: &str, shards: usize) -> usize {
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    usize::try_from(hash % u64::try_from(shards.max(1)).unwrap_or(1)).unwrap_or(0)
}

/// The sessions' list as `GET /api/harness/sessions` answers it, joined once for every read that finds the
/// sessions as they were ([`Copies::list_json`]; goal 10, plan 01a1a87b2dae step 5), with what it was joined
/// of, held by identity: each shard's board, weakly (a session gone from one is let go of, and no other board
/// takes its place unseen), and each session's info as its view said it, in the boards' order (no other
/// encoding takes its place unseen).
struct Listed {
    boards: Vec<Weak<Board>>,
    infos: Vec<Bytes>,
    json: Bytes,
}

impl Listed {
    /// Whether `boards` hold the sessions it was joined of, each as its view said it then: the same board on
    /// every shard, and in each, the same sessions open with the same info's encoding.
    fn holds(&self, boards: &[ArcSwap<Board>]) -> bool {
        if self.boards.len() != boards.len() {
            return false;
        }
        let mut infos = self.infos.iter();
        for (was, board) in self.boards.iter().zip(boards.iter()) {
            let board = board.load();
            if !std::ptr::eq(was.as_ptr(), Arc::as_ptr(&board)) {
                return false;
            }
            for slot in board.values() {
                let view = slot.view.load();
                if view.closed {
                    continue;
                }
                match infos.next() {
                    Some(info) if info.as_ptr() == view.info_json.as_ptr() && info.len() == view.info_json.len() => {}
                    _ => return false,
                }
            }
        }
        infos.next().is_none()
    }
}

/// Every session's copy, each owned by one shard: where messages to them go, and what readers read.
pub struct Copies {
    inboxes: Vec<crate::memory::queue::Sender<Msg>>,
    boards: Arc<[ArcSwap<Board>]>,
    /// The hub the owners act through (what a change tells the rest of kiln), once made.
    hub: Arc<OnceLock<Weak<Hub>>>,
    /// Where the owners spill their copies' sealed chunks, once set.
    spill: Arc<OnceLock<Arc<super::spill::Dir>>>,
    /// The sessions' list as last joined ([`Copies::list_json`]), and how many times it was.
    listed: ArcSwapOption<Listed>,
    joins: AtomicU64,
    /// [`Copies::inboxes_bound`].
    inboxes_bound: usize,
}

impl Copies {
    /// An owner on each of the runtime's shards, its sessions' feeds on `bus`.
    pub fn start(bus: Arc<dyn kiln_seams::Bus>) -> Self {
        Self::start_with(bus, crate::rt::shard_count().max(1), crate::net::EVENTS_BYTES)
    }

    /// `shards` owners (one a shard), each inbox holding `inbox` bytes of messages (their places made as
    /// it fills: `crate::memory::owners::SESSION_INBOXES`).
    pub fn start_with(bus: Arc<dyn kiln_seams::Bus>, shards: usize, inbox: usize) -> Self {
        let shards = shards.max(1);
        let boards: Arc<[ArcSwap<Board>]> = (0..shards).map(|_| ArcSwap::from_pointee(Board::new())).collect::<Vec<_>>().into();
        let hub = Arc::new(OnceLock::new());
        let spill = Arc::new(OnceLock::new());
        let mut inboxes = Vec::with_capacity(shards);
        for me in 0..shards {
            let (tx, rx) = crate::memory::queue::bounded(&crate::memory::owners::SESSION_INBOXES, inbox);
            let shard = Shard { me, inbox: tx.downgrade(), mirrors: HashMap::new(), boards: Arc::clone(&boards), bus: Arc::clone(&bus), board_changed: false, spill: Arc::clone(&spill), thaw: None, thawed: Weak::new(), insert: None, inserted: None, write: None, written: None, written_off: 0 };
            crate::rt::spawn_owner(me, own(shard, rx, Arc::clone(&hub)));
            inboxes.push(tx);
        }
        Self { inboxes, boards, hub, spill, listed: ArcSwapOption::empty(), joins: AtomicU64::new(0), inboxes_bound: inbox.saturating_mul(shards) }
    }

    /// What the owners' inboxes may hold together, in bytes: as much as may wait for the owners, and so as much
    /// as their updates' rows may wait for the conversation store before what hands them updates waits
    /// (`crate::convo::store::Store::room_within`; goal 10, plan 01a1a87b2dae step 5).
    pub fn inboxes_bound(&self) -> usize {
        self.inboxes_bound
    }

    /// Its copies' sealed chunks spilled to the folder at `path` (`super::spill`), emptied of what an
    /// earlier process spilled there: set once, as the registry starts, before any copy is made. Without
    /// it they're kept in memory.
    pub fn spill_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let dir = super::spill::Dir::open(path)?;
        let _ = self.spill.set(dir);
        Ok(())
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

    /// `GET /api/harness/sessions` as it answers: every session's info as its owner encoded it, oldest first
    /// (by when it was made, then its id), joined into a JSON array once for all the reads that find the
    /// sessions as they were ([`Listed`]), each of those answered the same bytes: nothing encoded, sorted,
    /// joined or copied (goal 10, plan 01a1a87b2dae steps 2 and 5). A read that finds a session come, gone
    /// or changed since joins it again, for itself and the reads after it.
    pub fn list_json(&self) -> Bytes {
        if let Some(listed) = self.listed.load().as_ref().filter(|l| l.holds(&self.boards)) {
            return listed.json.clone();
        }
        self.joins.fetch_add(1, Ordering::Relaxed);
        let (mut boards, mut infos, mut views) = (Vec::with_capacity(self.boards.len()), Vec::new(), Vec::new());
        for board in self.boards.iter() {
            let board = board.load_full();
            for slot in board.values() {
                let view = slot.view.load_full();
                if !view.closed {
                    infos.push(view.info_json.clone());
                    views.push(view);
                }
            }
            boards.push(Arc::downgrade(&board));
        }
        views.sort_by(|a, b| a.info.created_at.cmp(&b.info.created_at).then_with(|| a.info.id.cmp(&b.info.id)));
        let json = crate::reads::json_array(views.iter().map(|v| &v.info_json));
        self.listed.store(Some(Arc::new(Listed { boards, infos, json: json.clone() })));
        json
    }

    /// How many times the sessions' list was joined ([`Copies::list_json`]): what a read that found it as it
    /// was didn't do.
    pub fn list_joins(&self) -> u64 {
        self.joins.load(Ordering::Relaxed)
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

/// What an owner's polls taking `msg` are labeled with in the shards' table of what held them (goal 9, plan
/// 01a1a87b2cec step 3): an update by its kind (`crate::wire::update_label`), a runner's as a taken one, and
/// work on its copies as such.
fn label_of(msg: &Msg) -> &'static hyper_rt::slow::Label {
    static WORK: hyper_rt::slow::Label = hyper_rt::slow::Label { name: "owner work on its copies" };
    match msg {
        Msg::Update(i) => crate::wire::update_label(&i.u),
        Msg::Runner(r) => crate::wire::update_label(&r.update),
        Msg::Run(_) => &WORK,
    }
}

/// An owner's turn on its shard (goal 9, plan 01a1a87b2cec step 3): a poll of its own, from when it took the shard up to
/// when it gives it up, held within the shard's step quantum (`crate::rt::STEP_BUDGET_NS`, 266.7 µs: the longest a poll may
/// hold a shard, step 3's done-when). What it takes is learned as it's taken ([`Costs`]); within an update its events are
/// pieces, timed between the looks at them, as `crate::rt::Turns` times a loop's.
struct Turn {
    began: Instant,
    /// When an update's events were last looked at (the end of the last one taken, or the update's start), and the longest
    /// piece between two looks this turn.
    looked: Cell<Instant>,
    longest: Cell<Duration>,
}

impl Turn {
    fn new() -> Self {
        let now = Instant::now();
        Self { began: now, looked: Cell::new(now), longest: Cell::new(Duration::ZERO) }
    }

    /// The shard's step quantum (`hyper_rt::futures::step_budget_ns`): none off a shard, where a turn never ends.
    fn quantum() -> Option<Duration> {
        hyper_rt::futures::step_budget_ns().map(Duration::from_nanos)
    }

    /// Whether a piece expected to take `piece` would still end within the quantum.
    fn fits(&self, piece: Duration) -> bool {
        self.fits_at(Instant::now(), Self::quantum(), piece)
    }

    /// [`Turn::fits`] at `now`, against `quantum`.
    fn fits_at(&self, now: Instant, quantum: Option<Duration>, piece: Duration) -> bool {
        quantum.is_none_or(|q| now.saturating_duration_since(self.began).saturating_add(piece) < q)
    }

    /// A message begins at `now`: its pieces are timed from here, what came before it none of them.
    fn begin(&self, now: Instant) {
        self.looked.set(now);
    }

    /// After a piece, at `now`: whether the next, expected to take `next` and taken to be as long as the longest yet (the one
    /// just done among them), with `room` after it for what ends its message, would end past `quantum`. So a turn ends within
    /// the quantum where its pieces don't outgrow what's expected of them; one that looked only for its quantum to have
    /// passed held its shard a piece past it, and what ends the message (an update's commit) past that.
    fn over_at(&self, now: Instant, quantum: Option<Duration>, next: Duration, room: Duration) -> bool {
        let piece = now.saturating_duration_since(self.looked.get());
        self.looked.set(now);
        let longest = self.longest.get().max(piece);
        self.longest.set(longest);
        !self.fits_at(now, quantum, longest.max(next).saturating_add(room))
    }
}

/// What a kind of an owner's pieces takes, nanoseconds, as RFC 6298 estimates a round trip (goal 9, plan 01a1a87b2cec step
/// 3): a smoothed mean (gain 1/8) and mean deviation (gain 1/4), the first sample taken as the mean with half of it as its
/// deviation, and what's expected the mean and four deviations (RFC 6298's retransmission timeout, `K` = 4), so a piece
/// rarely takes past it. None expected until one was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Expect {
    mean: u64,
    dev: u64,
    seen: bool,
}

impl Expect {
    fn learn(&mut self, ns: u64) {
        if !self.seen {
            *self = Self { mean: ns, dev: ns / 2, seen: true };
            return;
        }
        self.dev = self.dev - self.dev / 4 + self.mean.abs_diff(ns) / 4;
        self.mean = self.mean - self.mean / 8 + ns / 8;
    }

    fn expected(&self) -> Duration {
        Duration::from_nanos(self.mean.saturating_add(self.dev.saturating_mul(4)))
    }
}

/// What an owner's messages take, learned as it takes them (goal 9, plan 01a1a87b2cec step 3): each kind's start (a message
/// whole, or an update's head and first event, up to the first look at its events), and the tail of an update its turn ran
/// out in (from that look on: its copy's commit, its frame, what it tells the rest of kiln). A turn takes a message only
/// where what it's expected to take still ends within the quantum ([`Turn::fits`]), and an update's events only while the
/// next and that tail do ([`Turn::over_at`]): a turn that took the next message while its last piece did held its shard past
/// the quantum by as much as that message took past it (a state after an update's events), and by its commit.
#[derive(Debug, Default)]
struct Costs {
    kinds: Vec<(&'static hyper_rt::slow::Label, Expect)>,
    tail: Expect,
}

impl Costs {
    /// A start of kind `label` took `ns`.
    fn learn(&mut self, label: &'static hyper_rt::slow::Label, ns: u64) {
        match self.kinds.iter_mut().find(|(l, _)| std::ptr::eq(*l, label)) {
            Some((_, e)) => e.learn(ns),
            None => {
                let mut e = Expect::default();
                e.learn(ns);
                self.kinds.push((label, e));
            }
        }
    }

    /// What taking `msg` is expected to take: its kind's start, and for an update's events the tail after them, its first
    /// event written out ([`write_ns`]) where that's more.
    fn expected(&self, msg: &Msg) -> Duration {
        let label = label_of(msg);
        let start = self.kinds.iter().find(|(l, _)| std::ptr::eq(*l, label)).map_or(Duration::ZERO, |(_, e)| e.expected());
        let u = match msg {
            Msg::Update(i) => &i.u,
            Msg::Runner(r) => &r.update,
            Msg::Run(_) => return start,
        };
        match u {
            Update::Events { events } => start.max(Duration::from_nanos(events.first().map_or(0, write_ns))).saturating_add(self.tail.expected()),
            _ => start,
        }
    }

    /// `msg` taken by `shard` in `turn`: what's left of it when the turn ran out in its events, and how long it took, both
    /// learned (its start, and the tail after the look that ended its turn).
    fn take(&mut self, shard: &mut Shard, msg: Msg, hub: Option<&Arc<Hub>>, turn: &Turn) -> (Option<Msg>, Duration) {
        let label = label_of(&msg);
        let (quantum, room) = (Turn::quantum(), self.tail.expected());
        let began = Instant::now();
        turn.begin(began);
        // The first look at its events, and the one that ended its turn.
        let (first, ran_out) = (Cell::new(None::<Instant>), Cell::new(None::<Instant>));
        let over = |next_ns: u64| {
            let now = Instant::now();
            if first.get().is_none() {
                first.set(Some(now));
            }
            let over = turn.over_at(now, quantum, Duration::from_nanos(next_ns), room);
            if over {
                ran_out.set(Some(now));
            }
            over
        };
        let left = shard.take(msg, hub, &over);
        // What was written out off the shard for this take alone: one it didn't take (its update gone meanwhile) isn't kept.
        shard.written = None;
        let end = Instant::now();
        self.learn(label, nanos(first.get().unwrap_or(end).saturating_duration_since(began)));
        if let Some(at) = ran_out.get() {
            self.tail.learn(nanos(end.saturating_duration_since(at)));
        }
        (left, end.saturating_duration_since(began))
    }
}

/// `fut`, and whether it gave its shard up waiting (it was pending at least once) rather than being ready at once.
async fn waited<F: std::future::Future>(fut: F) -> (F::Output, bool) {
    let mut fut = std::pin::pin!(fut);
    let mut gave_up = false;
    let out = std::future::poll_fn(|cx| {
        let poll = fut.as_mut().poll(cx);
        gave_up |= poll.is_pending();
        poll
    })
    .await;
    (out, gave_up)
}

/// A placeholder for an event taken out of its update while it's written out off the shard ([`write_off`]): never taken, the
/// event put back before its update is.
fn taken_out() -> super::model::Event {
    super::model::Event { id: String::new(), seq: 0, at: 0, parent: None, by: None, body: super::model::Body::Assistant { text: String::new(), streaming: false } }
}

/// The event `writing` names, the first of what's left of its update (`rest`), written out on the compute pool while the shard
/// does its other work (goal 9, plan 01a1a87b2cec step 3): taken out of the update, not copied (a large tool output's
/// megabytes, on the shard), and put back once written, what was made of it kept for the update's next take
/// ([`Shard::take_written`]). One that couldn't be written there is marked so, and written where it is. Whether the shard was
/// given up.
async fn write_off(shard: &mut Shard, writing: Writing, rest: &mut Option<Msg>) -> bool {
    let Some(Msg::Update(i)) = rest.as_mut() else { return false };
    let Update::Events { events } = &mut i.u else { return false };
    let Some(slot) = events.first_mut() else { return false };
    let Writing { session, keep, cwd, location } = writing;
    let e = std::mem::replace(slot, taken_out());
    let (id, seq, of) = (e.id.clone(), e.seq, session.clone());
    let (written, gave_up) = waited(crate::rt::compute(move || {
        let began = Instant::now();
        // Caught here, so the event comes back whatever: its update keeps every event.
        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| made_of(&of, &e, keep, &cwd, &location))).ok();
        let took = began.elapsed();
        (e, made, took)
    }))
    .await;
    match written {
        Ok((e, made, took)) => {
            if made.is_some() {
                wrote(&e, took);
                shard.written_off = shard.written_off.saturating_add(1);
            }
            *slot = e;
            shard.written = Some(Written { session, id, seq, made });
        }
        Err(err) => {
            // The pool dropped the job unrun (the process is going), the event with it: its update goes on without it, and its
            // runner sends it again, as it sends what a copy lacks (`crate::wire::sums`).
            tracing::error!(session = %session, event = %id, error = %format!("{err:#}"), "a session's event was lost being written out off its owner's shard");
            if !events.is_empty() {
                events.remove(0);
            }
            if events.is_empty() {
                *rest = None;
            }
        }
    }
    gave_up
}

/// A shard's owner: its messages, in the order they came, as fast as they come, in turns of its shard (the HTTP listener may
/// share it), each a poll of its own within the shard's step quantum ([`Turn`]; goal 9, plan 01a1a87b2cec step 3). It waits on
/// nothing but its inbox, and ends with it. A turn takes its first piece whatever that's expected to take (so each takes
/// one), then each message while it's expected to end within the quantum ([`Costs`]), an update's events while the next and
/// what ends the update are. What's left of a message its turn ran out in (an update's events), or one it had no room for,
/// goes first at its next turn, before anything newer. A turn spent with more to take gives the shard up whether or not
/// another task wants it: one that went on while none did (`crate::rt::Turns::after`, `maybe_yield`) ran on in the same poll,
/// which the shards' table counted whole (the lab's g9p9 fill: this owner's polls past 100 µs, 0.7 to 1.6 ms on average by
/// round, the longest 7 to 53 ms on the CPU, each labeled by a message of its last turn alone).
async fn own(mut shard: Shard, rx: crate::memory::queue::Receiver<Msg>, hub: Arc<OnceLock<Weak<Hub>>>) {
    let mut costs = Costs::default();
    let mut turn = Turn::new();
    // Nothing taken yet this turn: its first piece goes whatever it's expected to take.
    let mut fresh = true;
    // What's left of the message the last turn ran out in, or the next it had no room for.
    let mut rest: Option<Msg> = None;
    loop {
        let msg = match rest.take() {
            Some(msg) => msg,
            None => match rx.try_recv() {
                Ok(msg) => msg,
                Err(async_channel::TryRecvError::Closed) => return,
                // None waiting: waiting for one gives the shard to its other work, so a turn starts afresh once one comes
                // (else an update after a quiet spell would find its turn spent already). One that came as it asked is this
                // turn's still.
                Err(async_channel::TryRecvError::Empty) => match waited(rx.recv()).await {
                    (Ok(msg), gave_up) => {
                        if gave_up {
                            (turn, fresh) = (Turn::new(), true);
                        }
                        msg
                    }
                    (Err(_), _) => return,
                },
            },
        };
        // The hub, once a turn: what each change tells the rest of kiln goes through it.
        let h = hub.get().and_then(Weak::upgrade);
        // The turn's poll labeled by the kind of the message that took longest in it, should the poll hold the shard (goal 9,
        // plan 01a1a87b2cec step 3: the lab's s7c named this owner's polls, up to 12 ms, not what held them).
        let mut longest = Duration::ZERO;
        // What's waiting already goes in the same turn, where it's expected to end within the quantum.
        let mut next = Some(msg);
        while let Some(msg) = next.take() {
            if !fresh && !turn.fits(costs.expected(&msg)) {
                rest = Some(msg);
                break;
            }
            fresh = false;
            let label = label_of(&msg);
            let (left, took) = costs.take(&mut shard, msg, h.as_ref(), &turn);
            if took >= longest {
                longest = took;
                hyper_rt::futures::label(label);
            }
            rest = left;
            if rest.is_none() {
                next = rx.try_recv().ok();
            }
        }
        drop(h);
        // A change to an event of a compact chunk: that chunk read back on the blocking pool, the shard given up meanwhile,
        // before the rest of its update is taken (goal 9, plan 01a1a87b2cec step 3). This owner's next messages wait for it,
        // as they waited for it to be read back here.
        if let Some((id, chunk)) = shard.thaw.take() {
            let c = Arc::clone(&chunk);
            let (read, gave_up) = waited(crate::rt::blocking(move || c.evs().map(|evs| Chunk::of(evs.into_owned())))).await;
            match read {
                Ok(Ok(made)) => {
                    shard.take_thawed(&id, &chunk, made);
                }
                Ok(Err(e)) => {
                    tracing::warn!(session = %id, error = %e, "a session copy's packed events couldn't be read back off its shard: changed where they are");
                    shard.thawed = Arc::downgrade(&chunk);
                }
                Err(e) => {
                    tracing::warn!(session = %id, error = %format!("{e:#}"), "a session copy's packed events couldn't be read back off its shard: changed where they are");
                    shard.thawed = Arc::downgrade(&chunk);
                }
            }
            if gave_up {
                (turn, fresh) = (Turn::new(), true);
            }
        }
        // An event under an older number than the copy's newest: the copy's events made again with it in on the blocking
        // pool, the shard given up meanwhile, before the rest of its update is taken (goal 9, plan 01a1a87b2cec step 3).
        // Its update, taken then, finds it in and changes it where it is.
        if let Some((id, at, e, rev)) = shard.insert.take() {
            shard.inserted = Some((id.clone(), e.id.clone()));
            if let Some(was) = shard.get(&id).map(|m| m.events.clone()) {
                let from = was.clone();
                let (made, gave_up) = waited(crate::rt::blocking(move || from.inserted(at, Ev { entry: Arc::new(Entry::of_event(&e, None)), at: rev }))).await;
                match made {
                    Ok((made, index)) => {
                        shard.take_inserted(&id, &was, made, index);
                    }
                    Err(e) => tracing::warn!(session = %id, error = %format!("{e:#}"), "a session copy's events couldn't be made again off its shard: the event goes in where they are"),
                }
                if gave_up {
                    (turn, fresh) = (Turn::new(), true);
                }
            }
        }
        // An event long to write out (a large tool output): written on the compute pool, the shard given up meanwhile, before
        // the rest of its update is taken (goal 9, plan 01a1a87b2cec step 3).
        if let Some(writing) = shard.write.take()
            && write_off(&mut shard, writing, &mut rest).await
        {
            (turn, fresh) = (Turn::new(), true);
        }
        // The turn spent with more to take: the shard given up, whether or not another task wants it, so each turn is a poll
        // of its own. With nothing to take, the inbox's wait gives it up.
        //
        // The owner never waits for the conversation store (goal 10, plan 01a1a87b2dae step 5): it did, between its
        // turns, while the store was over its bound (goal 11's, what its writer commits in 50 ms), and a disk that
        // stalled the writer stalled every frame and attach of every session it owned. The rows its updates make
        // wait in the store's queue instead, as many as would have waited in its inbox, and what hands it updates
        // waits past that (`crate::harness::registry::Registry::from_runner`, `ingest_from`).
        if rest.is_some() && !fresh {
            hyper_rt::futures::yield_now().await;
            (turn, fresh) = (Turn::new(), true);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// About one of these tests' messages, in an inbox's bytes (goal 11, plan 01a1a87b2e56 step 3): its
    /// place, and a job's closure of a few captured words.
    const MSG: usize = size_of::<Msg>() + crate::memory::queue::PLACE + 4 * size_of::<usize>();

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
        let seqs = |e: &Events| e.iter().filter_map(|e| e.entry.seq()).collect::<Vec<_>>();
        let metas = |e: &Events| e.metas().filter_map(|m| m.seq()).collect::<Vec<_>>();
        assert_eq!(seqs(&events), flat);
        assert_eq!(metas(&events), flat);
        assert_eq!(events.len(), flat.len());
        // A view taken now keeps what it took, whatever changes after.
        let view = events.clone();
        assert!(events.change(CHUNK + 1, |e| e.at = 0));
        assert_eq!(view.get(CHUNK + 1).map(|e| e.at), Some(u64::try_from(CHUNK + 2).unwrap_or(0)));
        assert_eq!(events.get(CHUNK + 1).map(|e| e.at), Some(0));
        assert_eq!(events.meta(CHUNK + 1).map(|m| m.at), Some(0), "its meta with it");
        // In the middle, by number.
        let at = events.partition_point(|m| m.seq().is_some_and(|s| s < 10));
        events.insert(at, Ev { entry: entry("between", 10), at: 0 });
        flat.insert(at, 10);
        assert_eq!(seqs(&events), flat);
        let (gone, _) = events.drain_front(CHUNK + 3);
        assert_eq!(gone, CHUNK + 3);
        flat.drain(..CHUNK + 3);
        assert_eq!(seqs(&events), flat);
        assert_eq!(metas(&events), flat);
        // Each place still finds its event once the first chunk holds fewer.
        for (i, want) in flat.iter().enumerate() {
            assert_eq!(events.get(i).and_then(|e| e.entry.seq()), Some(*want), "event {i}");
            assert_eq!(events.meta(i).and_then(|m| m.seq()), Some(*want), "meta {i}");
        }
        for s in n + 1..=n + 3 {
            events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
            flat.push(s);
        }
        assert_eq!(seqs(&events), flat);
        events.retain(|m| m.seq().is_some_and(|s| s % 2 == 0));
        flat.retain(|s| s % 2 == 0);
        assert_eq!(seqs(&events), flat);
        assert_eq!(events.last().and_then(|e| e.entry.seq()), flat.last().copied());
        assert_eq!(view.len(), usize::try_from(n).unwrap_or(0), "the view's list is its own");
        events.clear();
        assert!(events.is_empty() && events.get(0).is_none());
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an event taken in at the newest chunk, or one in it changed, copies that chunk
    /// alone where a view shares it: the older chunks' list stays the view's. The chunk sealing as the next event comes
    /// joins the list (copied then, once a chunk) as it was; an event in an older chunk changed copies it and the list.
    /// The events, their order and the view's are as one list of every chunk had them.
    #[test]
    fn g9_s5_an_event_taken_in_at_the_newest_chunk_copies_that_chunk_alone() -> anyhow::Result<()> {
        let ids = |e: &Events| e.metas().map(|m| m.id().to_owned()).collect::<Vec<_>>();
        let mut events = Events::default();
        let n = u64::try_from(CHUNK * 3 + 5)?;
        for s in 1..=n {
            let _ = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        }
        let view = events.clone();
        let view_ids = ids(&view);
        anyhow::ensure!(events.push(Ev { entry: entry("x1", n + 1), at: n + 1 }).is_none(), "the newest had room");
        let newest_copied = !matches!((&events.newest, &view.newest), (Some(a), Some(b)) if Arc::ptr_eq(a, b));
        anyhow::ensure!(Arc::ptr_eq(&events.older, &view.older) && newest_copied, "the older chunks' list the view's, the newest copied");
        let last = events.len() - 1;
        let before = events.clone();
        anyhow::ensure!(events.change(last, |e| e.at = 0) && Arc::ptr_eq(&events.older, &before.older), "a change in the newest copies it alone");
        // Filled to the brim, the next event seals it: it joins the older ones as it was.
        while events.chunks().last().is_some_and(|c| c.len() < CHUNK) {
            let s = u64::try_from(events.len() + 1)?;
            let _ = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        }
        let full = events.newest.clone();
        let before = events.clone();
        let s = u64::try_from(events.len() + 1)?;
        let sealed = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        anyhow::ensure!(matches!((&sealed, &full), (Some(a), Some(b)) if Arc::ptr_eq(a, b)), "the full one sealed");
        anyhow::ensure!(!Arc::ptr_eq(&events.older, &before.older) && events.older.len() == before.older.len() + 1, "the list copied once, the sealed chunk in it");
        anyhow::ensure!(events.chunks().len() == 5 && events.chunks().iter().map(|c| c.len()).sum::<usize>() == events.len(), "every chunk, in order");
        // An event in an older chunk changed: that chunk and the list copied, the others shared.
        let before = events.clone();
        anyhow::ensure!(events.change(0, |e| e.at = 0) && !Arc::ptr_eq(&events.older, &before.older));
        let shared = events.chunks().iter().zip(before.chunks().iter()).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        anyhow::ensure!(shared == 4, "{shared} of five chunks still shared");
        // As one list of every chunk had them: in order, the view's as they were.
        let made = Events::of(events.iter().map(Cow::into_owned).collect());
        anyhow::ensure!(ids(&events) == ids(&made) && events.first == made.first && ids(&view) == view_ids && view.len() == view_ids.len());
        Ok(())
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
        assert!(events.change(0, |e| e.at = 0));
        let shared = events.chunks().iter().zip(view.chunks().iter()).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!(shared, 3, "three of four chunks still shared with the view");
    }

    /// Goal 10, plan 01a1a87b2dae step 3: each event's meta says where its text is among its chunk's
    /// texts joined by `,`, as they're deflated, so the segment gives back each event's exact text; a
    /// change in the middle moves those after it.
    #[test]
    fn g10_s3_a_chunks_metas_find_each_text_in_its_segment() -> anyhow::Result<()> {
        let mut events = Events::default();
        for s in 1..=u64::try_from(CHUNK + 4).unwrap_or(0) {
            events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        }
        assert!(events.change(2, |e| e.entry = Arc::new(Entry::new(json!({"id": "e3", "seq": 3, "kind": "assistant", "text": "a much longer line than before"})))));
        let chunk = events.chunks().first().cloned().ok_or_else(|| anyhow::anyhow!("no chunk"))?;
        let seg = chunk.deflate(Compression::default())?;
        let joined = String::from_utf8(super::super::packed::inflate(&seg)?)?;
        assert_eq!(u64::try_from(joined.len()).unwrap_or(0), seg.len);
        assert_eq!(seg.crc, crc32fast::hash(joined.as_bytes()));
        for (k, (m, e)) in chunk.metas().iter().zip(chunk.evs()?.iter()).enumerate() {
            assert_eq!(chunk.span(k, 1).and_then(|r| joined.get(r)), Some(e.entry.text()), "{}", m.id());
            assert_eq!((m.seq(), m.sum(e), m.id()), (e.entry.seq(), e.entry.sum(), e.entry.id().unwrap_or_default()));
        }
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 3: an owner takes events in without making a digest of any;
    /// the chunks that seal as they come are packed in the background by the one packer, their
    /// digests made with them, and the newest, still open, waits for a snapshot.
    #[test]
    fn g10_s3_sealed_chunks_are_packed_in_the_background_with_their_digests() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
            for n in 1..=u64::try_from(CHUNK * 3 + 1).unwrap_or(0) {
                m.rev = n;
                m.upsert(entry(&format!("e{n}"), n));
            }
            // The newest chunk, still open, nobody packs: what the owner made of its events is all it has.
            assert!(m.events.chunks().last().is_some_and(|c| c.metas().iter().all(|meta| meta.sum_made().is_none())), "no digest made as the events came in");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while m.events.chunks().iter().take(3).any(|c| c.packed().is_none()) {
                anyhow::ensure!(std::time::Instant::now() < deadline, "the sealed chunks weren't packed");
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            for c in m.events.chunks().iter().take(3) {
                assert!(c.metas().iter().all(|meta| meta.sum_made().is_some()), "a packed chunk's digests are all there");
                let evs = c.evs()?;
                for (meta, e) in c.metas().iter().zip(evs.iter()) {
                    assert_eq!(meta.sum(e), e.entry.sum());
                }
            }
            assert!(m.events.chunks().last().is_some_and(|c| c.packed().is_none() && c.len() == 1), "the newest waits");
            Ok(())
        })
    }

    /// Goal 10, plan 01a1a87b2dae step 3: a chunk is packed once and then taken as it is; a change lets
    /// go of what it packed (and a view that shares it keeps its own); and letting go of a long session's
    /// oldest keeps every chunk past the cut as it was, packed, the one the cut fell in made again of
    /// its rest.
    #[test]
    fn g10_s3_packed_chunks_stay_packed_past_the_oldest_letting_go() -> anyhow::Result<()> {
        let mut events = Events::default();
        let mut sealed = Vec::new();
        for s in 1..=u64::try_from(CHUNK * 4 + 2).unwrap_or(0) {
            if let Some(c) = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s }) {
                sealed.push(c);
            }
        }
        assert_eq!(sealed.len(), 4, "a chunk seals as the next one starts");
        for c in events.chunks() {
            if c.len() == CHUNK {
                c.pack()?;
            }
        }
        let first = events.chunks().first().and_then(|c| c.packed().cloned()).ok_or_else(|| anyhow::anyhow!("not packed"))?;
        assert!(SEGMENT_BYTES.load(Ordering::Relaxed) >= first.deflated.len());
        let again = events.chunks().first().map(|c| c.pack()).transpose()?.ok_or_else(|| anyhow::anyhow!("no chunk"))?;
        assert!(Arc::ptr_eq(&first, &again), "taken as it is");
        let view = events.clone();
        assert!(events.change(1, |e| e.at = 0));
        assert!(events.chunks().first().is_some_and(|c| c.packed().is_none()), "changed: let go of");
        assert!(view.chunks().first().is_some_and(|c| c.packed().is_some()), "the view's stays");
        // The cut falls a few events into the second chunk.
        let before: Vec<Arc<Chunk>> = events.chunks().to_vec();
        let (gone, remade) = events.drain_front(CHUNK + 5);
        assert_eq!(gone, CHUNK + 5);
        assert!(remade.is_some(), "the chunk the cut fell in, made again and sealed");
        assert_eq!(events.chunks().first().map(|c| c.len()), Some(CHUNK - 5));
        for (now, was) in events.chunks().iter().skip(1).zip(before.iter().skip(2)) {
            assert!(Arc::ptr_eq(now, was), "past the cut, as it was");
        }
        assert!(events.chunks().get(1).is_some_and(|c| c.packed().is_some()), "and still packed");
        assert_eq!(events.get(0).and_then(|e| e.entry.seq()), Some(u64::try_from(CHUNK + 6).unwrap_or(0)));
        assert_eq!(events.len(), CHUNK * 3 - 3);
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 3: the packer packs a sealed chunk an event at a time, giving its
    /// thread up after any one when another job waits (here, after every one), into the very bytes one
    /// deflate of the chunk makes, each event's digest made as it goes in; the newest first; and each
    /// packed chunk's owner is told to keep it compact. One packed meanwhile (a snapshot needed it) is
    /// done with as it is; one nothing holds but the packer is let go of, unpacked.
    #[test]
    fn g10_s3_the_packer_packs_an_event_at_a_time_newest_first() -> anyhow::Result<()> {
        use crate::rt::Step;
        let mut events = Events::default();
        let mut sealed = Vec::new();
        for s in 1..=u64::try_from(CHUNK * 3 + 1)? {
            if let Some(c) = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s }) {
                sealed.push(c);
            }
        }
        let [oldest, middle, newest] = <[Arc<Chunk>; 3]>::try_from(sealed).map_err(|s| anyhow::anyhow!("{} chunks sealed", s.len()))?;
        let want = newest.deflate(Compression::default())?;
        let (owner, told) = crate::memory::queue::bounded::<Msg>(&crate::memory::owners::SESSION_INBOXES, 8 * MSG);
        let home = Home { inbox: owner.downgrade(), id: Arc::from("s") };
        let (tx, rx) = sealed_queue();
        for c in [&oldest, &middle, &newest] {
            tx.try_send((Arc::downgrade(c), Some(home.clone()))).map_err(|_| anyhow::anyhow!("the packer's queue refused a chunk"))?;
        }
        let mut packer = Packer { rx, waiting: Vec::new(), now: None, later: Vec::new() };
        let after_each = || true;
        for k in 1..=CHUNK {
            anyhow::ensure!(matches!(packer.piece(&after_each), Step::More), "piece {k} ended the job");
            assert!(packer.now.as_ref().is_some_and(|p| Arc::ptr_eq(&p.chunk, &newest) && p.next == k), "piece {k}: the newest, its {k} first in");
            assert_eq!(newest.metas().iter().filter(|m| m.sum_made().is_some()).count(), k, "piece {k}: a digest an event in");
            assert!(newest.packed().is_none() && told.is_empty(), "piece {k}: not packed yet");
        }
        anyhow::ensure!(matches!(packer.piece(&after_each), Step::More));
        let got = newest.packed().cloned().ok_or_else(|| anyhow::anyhow!("the newest wasn't packed"))?;
        assert_eq!((&got.deflated, got.crc, got.len), (&want.deflated, want.crc, want.len), "the bytes one deflate makes");
        assert!(matches!(told.try_recv(), Ok(Msg::Run(_))), "its owner told to keep it compact");
        // The middle begun, then packed by a snapshot that needed it.
        anyhow::ensure!(matches!(packer.piece(&after_each), Step::More));
        assert!(packer.now.as_ref().is_some_and(|p| Arc::ptr_eq(&p.chunk, &middle) && p.next == 1), "the middle next");
        let snapshot = middle.pack()?;
        anyhow::ensure!(matches!(packer.piece(&after_each), Step::More));
        assert!(packer.now.is_none() && middle.packed().is_some_and(|s| Arc::ptr_eq(s, &snapshot)), "the snapshot's kept, the packer's let go of");
        assert!(matches!(told.try_recv(), Ok(Msg::Run(_))), "its owner told");
        // The oldest begun, then let go of by its copy (the oldest let go of) and everyone else.
        anyhow::ensure!(matches!(packer.piece(&after_each), Step::More));
        assert!(packer.now.as_ref().is_some_and(|p| Arc::ptr_eq(&p.chunk, &oldest)), "the oldest last");
        let was = Arc::downgrade(&oldest);
        let _ = events.drain_front(CHUNK);
        drop(oldest);
        anyhow::ensure!(matches!(packer.piece(&after_each), Step::More));
        assert!(packer.now.is_none() && was.upgrade().is_none(), "let go of, unpacked");
        assert!(told.is_empty(), "no owner told of a chunk gone");
        assert!(matches!(packer.piece(&after_each), Step::Done(later) if later.is_empty()), "nothing left: the job ends");
        drop(owner);
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: the packer's queue (`sealed_queue`, lock-free) takes every owner's
    /// sealed chunks at once from every shard, none refused and none waiting (no bound, as the channel it was
    /// had none), and the packer has each owner's in the order it sealed them.
    #[test]
    fn g4_s5_sealed_chunks_reach_the_packer_from_every_shard_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (owners, per) = (8usize, 2_000usize);
            let (tx, rx) = sealed_queue();
            let (inbox, _told) = crate::memory::queue::bounded::<Msg>(&crate::memory::owners::SESSION_INBOXES, MSG);
            let mut sealing = Vec::new();
            for o in 0..owners {
                let (tx, inbox) = (tx.clone(), inbox.downgrade());
                sealing.push(crate::rt::spawn_spread_joined(async move {
                    (0..per).all(|n| tx.try_send((Weak::new(), Some(Home { inbox: inbox.clone(), id: Arc::from(format!("{o}/{n}")) }))).is_ok())
                }));
            }
            drop(tx);
            for s in sealing {
                anyhow::ensure!(s.await.unwrap_or(false), "an owner's sealed chunk was refused");
            }
            let mut got = vec![Vec::new(); owners];
            while let Ok((_, home)) = rx.try_recv() {
                let id = home.map(|h| h.id.to_string()).unwrap_or_default();
                let (o, n) = id.split_once('/').ok_or_else(|| anyhow::anyhow!("an entry without its home: {id}"))?;
                got.get_mut(o.parse::<usize>()?).ok_or_else(|| anyhow::anyhow!("no owner {o}"))?.push(n.parse::<usize>()?);
            }
            for (o, theirs) in got.iter().enumerate() {
                anyhow::ensure!(theirs.iter().copied().eq(0..per), "owner {o}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
            }
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 2: a copy whose sealed chunks are kept compact (their metas and
    /// segment alone) reads as it did: each event, and the events' texts joined as `View::events_json`
    /// joins them, over ranges across the chunks' edges, each compact chunk's segment read once a range.
    #[test]
    fn g11_s2_compact_chunks_read_as_they_did() -> anyhow::Result<()> {
        let mut events = Events::default();
        let n = CHUNK * 3 + 7;
        for s in 1..=u64::try_from(n)? {
            let _ = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
        }
        let texts: Vec<String> = events.iter().map(|e| e.entry.text().to_owned()).collect();
        for c in events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        events.compact(None);
        assert_eq!(events.chunks().iter().filter(|c| c.is_compact()).count(), 3, "every sealed chunk, not the newest");
        for (i, t) in texts.iter().enumerate() {
            assert_eq!(events.get(i).map(|e| e.entry.text().to_owned()).as_deref(), Some(t.as_str()), "event {i}");
        }
        for (start, end) in [(0, n), (0, 1), (CHUNK - 2, CHUNK + 3), (CHUNK, 2 * CHUNK), (5, 3 * CHUNK + 2), (n - 4, n), (7, 7)] {
            let mut out = Vec::new();
            events.texts_into(start..end, &mut out).ok_or_else(|| anyhow::anyhow!("{start}..{end} not read"))?;
            assert_eq!(String::from_utf8(out)?, texts.get(start..end).unwrap_or_default().join(","), "{start}..{end}");
        }
        assert!(events.texts_into(n - 1..n + 1, &mut Vec::new()).is_none(), "past its end");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a copy's newest page read twice inflates its compact chunk once. The
    /// first read keeps the texts of the newest compact chunk (its alone), the second takes a slice of them,
    /// nothing inflated, and both say what the events' texts joined say. A newer chunk compacted, the older's
    /// texts go; a read of a view from before then inflates and keeps nothing, and one of the new view keeps
    /// the newest's.
    #[test]
    fn g10_s5_a_page_read_twice_inflates_its_chunk_once() -> anyhow::Result<()> {
        let inflated = || super::super::packed::INFLATED.with(std::cell::Cell::get);
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        let push = |m: &mut Mirror, seqs: std::ops::RangeInclusive<u64>| -> anyhow::Result<()> {
            for q in seqs {
                m.rev = q;
                m.upsert(entry(&format!("e{q}"), q));
            }
            for c in m.events.chunks().iter().filter(|c| c.is_full()) {
                c.pack()?;
            }
            m.events.compact(None);
            m.publish();
            Ok(())
        };
        // Two sealed chunks and the newest open with nine: the newest fifty reach into the second.
        let n = u64::try_from(CHUNK * 2 + 9)?;
        push(&mut m, 1..=n)?;
        let page = |m: &Mirror| -> String {
            let texts: Vec<String> = m.events.iter().map(|e| e.entry.text().to_owned()).collect();
            format!(r#"{{"events":[{}]}}"#, texts.get(texts.len().saturating_sub(50)..).unwrap_or_default().join(","))
        };
        let (want, view) = (page(&m), m.view());
        let kept = |v: &View, i: usize| v.events.chunks().get(i).and_then(|c| c.page_kept());
        let was = inflated();
        let first = view.events_json(None, 50).ok_or_else(|| anyhow::anyhow!("the newest page from memory"))?;
        anyhow::ensure!(inflated() == was + 1, "the first read inflates its compact chunk once");
        anyhow::ensure!(String::from_utf8_lossy(&first) == want, "the events' texts joined");
        anyhow::ensure!(kept(&view, 1).is_some() && kept(&view, 0).is_none(), "the newest compact chunk's texts kept, the older's not");
        let parts = view.events_page(None, 50).ok_or_else(|| anyhow::anyhow!("the newest page again"))?;
        anyhow::ensure!(inflated() == was + 1, "the second read inflates nothing");
        anyhow::ensure!(parts.concat() == first.as_ref(), "the same page");
        // A newer chunk sealed and compacted: the older's texts go.
        push(&mut m, n + 1..=n + u64::try_from(CHUNK)?)?;
        anyhow::ensure!(kept(&m.view(), 1).is_none(), "the older's texts let go of");
        let was = inflated();
        let _ = view.events_page(None, 50).ok_or_else(|| anyhow::anyhow!("the view from before"))?;
        anyhow::ensure!(inflated() == was + 1 && kept(&view, 1).is_none(), "a view from before inflates its own and keeps nothing");
        let now = m.view();
        let newest = now.events_json(None, 50).ok_or_else(|| anyhow::anyhow!("the newest page now"))?;
        anyhow::ensure!(String::from_utf8_lossy(&newest) == page(&m) && kept(&now, 2).is_some(), "the newest compact chunk's kept now");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a chunk has no cell for its texts until a read of its copy's newest page keeps
    /// them, so a chunk let go of without one pays arc-swap nothing (it pays every thread's debt slots as any of its
    /// cells is swapped or goes, an empty one too). None after events, sealing, packing and compacting; none in the
    /// copy of the newest chunk an event makes while a view shares it; the newest page's read makes one for the newest
    /// compact chunk alone, whose texts it keeps and gives back as before; and a chunk changed in place lets its cell go
    /// whole, its texts counted out.
    #[test]
    fn g9_s5_a_chunk_has_no_cell_for_its_texts_until_a_page_keeps_them() -> anyhow::Result<()> {
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        // Two sealed chunks and the newest open with nine, a view published at each event (its newest chunk copied at the next).
        let n = u64::try_from(CHUNK * 2 + 9)?;
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
            m.publish();
        }
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        m.publish();
        let cells = |v: &View| v.events.chunks().iter().map(|c| c.texts.get().is_some()).collect::<Vec<_>>();
        let view = m.view();
        anyhow::ensure!(cells(&view) == [false, false, false], "no chunk read, none has a cell: {:?}", cells(&view));
        m.rev = n + 1;
        m.upsert(entry(&format!("e{}", n + 1), n + 1));
        anyhow::ensure!(m.events.chunks().last().is_some_and(|c| c.texts.get().is_none()), "the newest chunk's copy has none");
        let page = view.events_json(None, 50).ok_or_else(|| anyhow::anyhow!("the newest page"))?;
        anyhow::ensure!(cells(&view) == [false, true, false], "the newest compact chunk's alone: {:?}", cells(&view));
        let kept = view.events.chunks().get(1).and_then(|c| c.page_kept());
        anyhow::ensure!(kept.is_some_and(|k| k > 0), "its texts kept: {kept:?}");
        anyhow::ensure!(view.events_json(None, 50).as_deref() == Some(page.as_ref()), "the same page again, from what it kept");
        // A compact chunk with its texts kept, changed in place (its own, no view sharing it): its cell goes whole, and
        // what it holds then reads as the same texts, kept nowhere (it's no compact chunk now).
        let evs: Vec<Ev> = (1..=u64::try_from(CHUNK)?).map(|q| Ev { entry: entry(&format!("c{q}"), q), at: q }).collect();
        let sealed = Chunk::of(evs);
        sealed.pack()?;
        let mut compact = sealed.compacted().ok_or_else(|| anyhow::anyhow!("a packed chunk kept compact"))?;
        compact.paged.store(true, Ordering::SeqCst);
        let (_, texts) = compact.page_texts(0)?;
        anyhow::ensure!(compact.texts.get().is_some() && compact.page_kept() == Some(texts.len()), "a page's read keeps its texts");
        anyhow::ensure!(compact.change(0, |_| {}), "changed in place");
        anyhow::ensure!(compact.texts.get().is_none() && compact.page_kept().is_none(), "changed, its cell let go of whole");
        anyhow::ensure!(compact.page_texts(0)?.1 == texts && compact.texts.get().is_none(), "read again: the same texts, no cell made");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: an info changed in nothing a list shows but its `last_activity` (every state its
    /// machine sends sets that) is its last encoding with that number said anew, byte for byte the info written whole, at
    /// every width and sign of the number, its words in a string beside it; anything else changed is written whole, and so
    /// is one whose key and number are found twice or only as the start of another number. A copy's published views say
    /// its info as written whole, an encoding counted at each change.
    #[test]
    fn g9_s5_an_infos_new_activity_is_said_in_its_last_encoding() -> anyhow::Result<()> {
        let base = Info { id: "s".into(), kind: "claude".into(), name: r#"a "last_activity":17, b"#.into(), title: r#"{"last_activity":17}"#.into(), cwd: "/w".into(), goal: "ship it".into(), ..Info::default() };
        for (old, new) in [(17, 1_793_851_234_567), (0, -1), (-1, 9), (9, 10), (i64::MIN, i64::MAX), (1_793_851_234_567, 1_793_851_234_568)] {
            let was = Info { last_activity: old, ..base.clone() };
            let now = Info { last_activity: new, ..base.clone() };
            let last = info_json(&was);
            let made = with_last_activity(&last, old, new).ok_or_else(|| anyhow::anyhow!("{old} -> {new}: its number not found"))?;
            anyhow::ensure!(made == info_json(&now), "{old} -> {new}: {}", String::from_utf8_lossy(&made));
            anyhow::ensure!(info_json_after(&now, &was, &last) == info_json(&now), "{old} -> {new}, as a copy says it");
        }
        let was = Info { last_activity: 5, ..base.clone() };
        let now = Info { last_activity: 6, title: "renamed".into(), ..base.clone() };
        anyhow::ensure!(info_json_after(&now, &was, &info_json(&was)) == info_json(&now), "a title changed too: written whole");
        anyhow::ensure!(with_last_activity(br#"{"a":{"last_activity":3},"last_activity":3}"#, 3, 4).is_none(), "found twice: none");
        anyhow::ensure!(with_last_activity(br#"{"last_activity":31}"#, 3, 4).is_none(), "only another number's start: none");
        let mut m = Mirror::new(base.clone(), Location::Local, sim_bus());
        for (n, at) in [100, 1_000, 99, 99].into_iter().enumerate() {
            let encoded = m.encodings.info;
            Arc::make_mut(&mut m.info).last_activity = at;
            if n == 2 {
                Arc::make_mut(&mut m.info).status = super::super::model::Status::Exited;
            }
            m.publish();
            anyhow::ensure!(m.view().info_json == info_json(&m.info), "published {n}: {}", String::from_utf8_lossy(&m.view().info_json));
            anyhow::ensure!(m.encodings.info == encoded + 1, "published {n}: one encoding, {} then {}", encoded, m.encodings.info);
        }
        Ok(())
    }

    /// A viewer's snapshot of `m` as it is: its frames, all of it packed as one message, and a catch-up from
    /// revision 3 (as `crate::api`'s `send_snapshot` writes them, here on the test's thread).
    fn attached(m: &Mirror) -> anyhow::Result<(Vec<String>, Vec<u8>, Option<Vec<String>>)> {
        let whole = super::super::registry::Snapshot { parts: m.view().parts(), files: None, store: None };
        let caught_up = m.view().parts_since(3).map(|parts| super::super::registry::Snapshot { parts, files: None, store: None }.frames());
        Ok((whole.frames(), whole.packed("synced")?, caught_up))
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a copy whose sealed chunks past its newest page went to its spill
    /// file attaches as it did: a viewer's snapshot (its frames, and packed as one message) and a catch-up are
    /// the same bytes, in the same order, as before the spill. The events API's newest page still comes from
    /// memory, an older page from the store; and an event changed in a spilled chunk is read back and changed.
    #[test]
    fn g11_s5_a_spilled_copy_attaches_as_it_did() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = super::super::spill::Dir::open(&tmp.path().join("spill"))?;
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        // Seven chunks, the newest open with nine: the three among the newest hundred events stay, four go.
        let n = u64::try_from(CHUNK * 6 + 9)?;
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
        }
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        m.publish();
        let before = attached(&m)?;
        let newest = m.view().events_json(None, 50);
        anyhow::ensure!(newest.is_some(), "the newest page from memory");
        let chunks = m.events.spillable(EVENTS_PAGE, PAGE_BYTES);
        anyhow::ensure!(chunks.len() == 4, "{} chunks past the newest page", chunks.len());
        let segments: Vec<Arc<Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        // This copy's own segments in memory (not the process's: other tests pack beside it).
        let resident = |m: &Mirror| m.events.chunks().iter().filter_map(|c| c.packed()).map(|s| s.deflated.len()).sum::<usize>();
        let held = resident(&m);
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(m.events.spilled(&chunks, spilled) == 4, "every one of them spilled");
        drop((chunks, segments));
        m.publish();
        anyhow::ensure!(m.events.chunks().iter().filter(|c| c.is_spilled()).all(|c| c.packed().is_none()), "a spilled chunk keeps no segment in memory");
        anyhow::ensure!(resident(&m) < held, "what went to the file left the copy's memory: {} of {held}", resident(&m));
        anyhow::ensure!(attached(&m)? == before, "the same bytes, in the same order, after the spill");
        anyhow::ensure!(m.view().events_json(None, 50) == newest, "the newest page still from memory");
        anyhow::ensure!(m.view().events_json(Some(20), 10).is_none(), "an older page is the store's to answer");
        // An event of a spilled chunk changed: read back, changed, its chunk in memory again.
        m.rev = n + 1;
        m.upsert(Arc::new(Entry::new(json!({"id": "e3", "seq": 3, "kind": "assistant", "text": "said again"}))));
        anyhow::ensure!(m.events.chunks().first().is_some_and(|c| !c.is_spilled()), "the changed chunk is in memory again");
        anyhow::ensure!(m.events.get(2).is_some_and(|e| e.entry.text().contains("said again")), "changed");
        anyhow::ensure!(m.events.get(80).and_then(|e| e.entry.seq()) == Some(81), "the rest as they were");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a spilled chunk whose file doesn't give it back is never lost to a
    /// viewer: its snapshot reads those events from the conversation store by their numbers (here kept as the
    /// copy had them: the same frames as before), and without a store the snapshot is refused, not made wrong.
    #[test]
    fn g11_s5_a_spill_that_doesnt_read_back_is_read_from_the_store() -> anyhow::Result<()> {
        use crate::convo::rows::{EventRow, Row};
        let tmp = tempfile::tempdir()?;
        let folder = tmp.path().join("spill");
        let dir = super::super::spill::Dir::open(&folder)?;
        let store = crate::convo::store::Store::start(Box::new(crate::convo::turso::Turso::open(&tmp.path().join("convo.db"))?), crate::convo::store::Upkeep::default())?;
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        let n = u64::try_from(CHUNK * 6 + 9)?;
        let mut rows = Vec::new();
        for q in 1..=n {
            m.rev = q;
            let e = entry(&format!("e{q}"), q);
            let body = e.text().to_owned();
            rows.push(Row::Event(Box::new(EventRow { session: "s".into(), id: format!("e{q}"), seq: q, at: 0, parent: None, by: None, kind: "assistant".into(), level: None, tool: None, status: None, text: String::new(), body })));
            m.upsert(e);
        }
        store.put(rows);
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        let before = super::super::registry::Snapshot { parts: m.view().parts(), files: None, store: None }.frames();
        let chunks = m.events.spillable(EVENTS_PAGE, PAGE_BYTES);
        let segments: Vec<Arc<Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(m.events.spilled(&chunks, spilled) == 4);
        drop((chunks, segments));
        // The file's bytes lost under it: each spilled chunk is refused as it's read back.
        for f in std::fs::read_dir(&folder)? {
            let path = f?.path();
            let len = std::fs::metadata(&path)?.len();
            std::fs::write(&path, vec![0u8; usize::try_from(len)?])?;
        }
        let with = super::super::registry::Snapshot { parts: m.view().parts(), files: None, store: Some((store.clone(), Arc::from("s"))) };
        anyhow::ensure!(with.frames() == before, "the store's events, the same frames");
        anyhow::ensure!(with.packed("synced").is_ok(), "packed from the store's events too");
        let without = super::super::registry::Snapshot { parts: m.view().parts(), files: None, store: None };
        anyhow::ensure!(without.packed("synced").is_err(), "no store: refused, not made wrong");
        store.close()?;
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a session's owner spills its sealed chunks past its newest page by
    /// itself, once they're packed; a viewer's snapshot of it, read back on the blocking pool, is the one a
    /// copy that kept them all in memory makes; and its spill file goes with the session.
    #[test]
    fn g11_s5_an_owner_spills_its_sealed_chunks_past_the_newest_page() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let tmp = tempfile::tempdir()?;
            let folder = tmp.path().join("spill");
            let copies = Arc::new(Copies::start_with(sim_bus(), 1, 64 * MSG));
            copies.spill_to(&folder)?;
            let (bus, id) = (sim_bus(), "s1".to_owned());
            let info = Info { id: id.clone(), ..Info::default() };
            let i = id.clone();
            copies.on(&id, move |s| s.insert(i, Mirror::new(info, Location::Local, bus))).await;
            let n = u64::try_from(CHUNK * 6 + 3)?;
            let i = id.clone();
            copies
                .on(&id, move |s| {
                    if let Some(m) = s.get_mut(&i) {
                        for q in 1..=n {
                            m.rev = q;
                            m.upsert(entry(&format!("e{q}"), q));
                        }
                        m.publish();
                    }
                })
                .await;
            // Seven chunks, the newest open: the three among the newest hundred events stay, four go.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let spilled = copies.with(&id, |v| v.events.chunks().iter().filter(|c| c.is_spilled()).count()).unwrap_or(0);
                if spilled == 4 {
                    break;
                }
                anyhow::ensure!(std::time::Instant::now() < deadline, "{spilled} of 4 chunks spilled");
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            // The same copy, its chunks packed and kept in memory.
            let mut kept = Mirror::new(Info { id: id.clone(), ..Info::default() }, Location::Local, sim_bus());
            for q in 1..=n {
                kept.rev = q;
                kept.upsert(entry(&format!("e{q}"), q));
            }
            for c in kept.events.chunks().iter().filter(|c| c.is_full()) {
                c.pack()?;
            }
            kept.events.compact(None);
            kept.publish();
            let want = super::super::registry::Snapshot { parts: kept.view().parts(), files: None, store: None }.frames();
            let view = copies.view(&id).ok_or_else(|| anyhow::anyhow!("no copy"))?;
            let got = crate::rt::blocking(move || super::super::registry::Snapshot { parts: view.parts(), files: None, store: None }.frames()).await?;
            anyhow::ensure!(got == want, "the spilled copy's snapshot is the kept one's");
            anyhow::ensure!(std::fs::read_dir(&folder)?.count() == 1, "one spill file, the copy's");
            // The session gone, its file goes with it.
            let i = id.clone();
            copies.on(&id, move |s| drop(s.remove(&i))).await;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::fs::read_dir(&folder)?.count() > 0 {
                anyhow::ensure!(std::time::Instant::now() < deadline, "the spill file outlived its session");
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: once its spills land a copy keeps at most [`PAGE_BYTES`] of segments
    /// in memory: the sealed chunks among its newest page while they fit (two of events the lab's size), and of
    /// events too large for two chunks of them to fit, those past it spilled as well; `session_segments`' bound
    /// counts each copy.
    #[test]
    fn g11_s3_a_copy_keeps_its_page_bytes_of_segments_at_most() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir = super::super::spill::Dir::open(&tmp.path().join("spill"))?;
        let resident = |m: &Mirror| m.events.chunks().iter().filter(|c| !c.is_spilled()).filter_map(|c| c.packed()).map(|s| s.deflated.len()).sum::<usize>();
        let spill = |m: &mut Mirror| -> anyhow::Result<usize> {
            for c in m.events.chunks().iter().filter(|c| c.is_full()) {
                c.pack()?;
            }
            m.events.compact(None);
            let chunks = m.events.spillable(EVENTS_PAGE, PAGE_BYTES);
            let segments: Vec<Arc<Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
            let (_file, spilled) = super::super::spill::write(&dir, None, &m.info.id, &segments)?;
            Ok(m.events.spilled(&chunks, spilled))
        };
        // Events the lab's size: the two sealed chunks among the newest page stay.
        let mut small = Mirror::new(Info { id: "small".into(), ..Info::default() }, Location::Local, sim_bus());
        for q in 1..=u64::try_from(CHUNK * 6 + 9)? {
            small.rev = q;
            small.upsert(entry(&format!("e{q}"), q));
        }
        anyhow::ensure!(spill(&mut small)? == 4, "past the newest page");
        anyhow::ensure!(small.events.chunks().iter().filter(|c| c.is_compact() && !c.is_spilled()).count() == 2, "the newest page's two stay");
        anyhow::ensure!(resident(&small) <= PAGE_BYTES, "{} resident", resident(&small));
        // Events of 1,500 bytes that don't compress: two chunks of them are past PAGE_BYTES.
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        let mut noise = |n: usize| -> String {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    char::from(b"0123456789abcdef".get(usize::try_from(x & 15).unwrap_or(0)).copied().unwrap_or(b'0'))
                })
                .collect()
        };
        let mut large = Mirror::new(Info { id: "large".into(), ..Info::default() }, Location::Local, sim_bus());
        for q in 1..=u64::try_from(CHUNK * 4 + 9)? {
            large.rev = q;
            large.upsert(Arc::new(Entry::new(json!({"id": format!("e{q}"), "seq": q, "kind": "assistant", "text": noise(1500)}))));
        }
        spill(&mut large)?;
        anyhow::ensure!(resident(&large) <= PAGE_BYTES, "{} resident of large events", resident(&large));
        anyhow::ensure!(large.events.chunks().iter().rev().skip(1).take(2).any(|c| c.is_spilled()), "the newest page's too, past its bytes");
        anyhow::ensure!(segments_bound() >= u64::try_from(2 * PAGE_BYTES)?, "both copies counted");
        Ok(())
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
            let copies = Arc::new(Copies::start_with(sim_bus(), crate::rt::shard_count().max(1), 8 * MSG));
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

    /// Goal 4, plan 01a1a59c2a9c step 5: with every owner's inbox holding about two messages and the owners
    /// held up (their shards busy), many senders on every shard each send their session's changes;
    /// they wait for room rather than drop any, and each session's copy takes every change once, in
    /// the order its sender sent them.
    #[test]
    fn changes_keep_their_order_and_none_is_lost_when_shards_are_busy() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let shards = crate::rt::shard_count().max(1);
            let copies = Arc::new(Copies::start_with(sim_bus(), shards, 2 * MSG));
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
                let got = copies.with(id, |v| v.events.metas().filter_map(|m| m.seq()).collect::<Vec<_>>()).unwrap_or_default();
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
            let copies = Arc::new(Copies::start_with(sim_bus(), crate::rt::shard_count().max(1), 64 * MSG));
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
                        let Some((rev, events, said)) = c.with(&i, |v| (v.rev, u64::try_from(v.events.len()).unwrap_or(0), v.state.get::<u64>("rev").unwrap_or(0))) else { continue };
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
                                m.state = Arc::new(StateDoc::of_value(&json!({"rev": m.rev})));
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

    /// A copy of events numbered 1 to `n`, its sealed chunks packed and kept compact, its newest open.
    fn compact_copy(n: u64) -> anyhow::Result<Mirror> {
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
        }
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        Ok(m)
    }

    fn inflated() -> u64 {
        super::super::packed::INFLATED.with(std::cell::Cell::get)
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an Open that goes on from its copy's newest (a runner's reconnect, as
    /// it mostly is: `crate::wire::sums::delta` says the copy's newest) keeps the copy as it is, on its owner's
    /// shard: no chunk read again (none inflated), each the one it was, its index as it was. One going on from
    /// past its newest the same. It read every chunk again, and parsed every event, to keep them all.
    #[test]
    fn g9_s3_an_open_going_on_from_its_copys_newest_reads_nothing_again() -> anyhow::Result<()> {
        let n = u64::try_from(CHUNK * 4 + 9)?;
        let mut m = compact_copy(n)?;
        anyhow::ensure!(m.events.chunks().iter().filter(|c| c.is_compact()).count() == 4, "four compact chunks, the newest open");
        let (chunks, index) = (m.events.chunks().to_vec(), m.index.clone());
        for since in [n, n + 100] {
            let was = inflated();
            m.keep_through(since);
            anyhow::ensure!(inflated() == was, "going on from {since}, a chunk was read again");
            anyhow::ensure!(m.events.chunks().len() == chunks.len() && m.events.chunks().iter().zip(&chunks).all(|(a, b)| Arc::ptr_eq(a, b)), "going on from {since}, each chunk the one it was");
            anyhow::ensure!(m.events.len() == CHUNK * 4 + 9 && m.index == index, "going on from {since}, its events and index as they were");
        }
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an Open that goes on from before its copy's newest keeps its events
    /// through that number as `Events::retain` keeps them: the chunks wholly before the cut the ones they were,
    /// the one it falls in made again of its events before it (read again only when it's compact, once), the
    /// rest let go of; its index finds each kept event where it is and none of the others; and the copy goes on
    /// taking events after it.
    #[test]
    fn g9_s3_an_open_going_on_from_before_its_newest_reads_only_the_chunk_it_cuts() -> anyhow::Result<()> {
        let n = u64::try_from(CHUNK * 4 + 9)?;
        let chunk = u64::try_from(CHUNK)?;
        let texts = |e: &Events| e.iter().map(|e| e.entry.text().to_owned()).collect::<Vec<_>>();
        // Into the newest, open chunk; into a compact one; on a chunk's edge; before them all.
        for (since, reads) in [(chunk * 4 + 3, 0), (chunk + 5, 1), (chunk * 2, 0), (0, 0)] {
            let mut m = compact_copy(n)?;
            let before = m.events.chunks().to_vec();
            let mut want = m.events.clone();
            want.retain(|e| e.seq().is_some_and(|q| q <= since));
            let was = inflated();
            m.keep_through(since);
            let read = inflated().saturating_sub(was);
            anyhow::ensure!(read == reads, "going on from {since}: {read} chunks read again, not {reads}");
            let whole = usize::try_from(since / chunk)?;
            anyhow::ensure!(m.events.chunks().iter().zip(&before).take(whole).all(|(a, b)| Arc::ptr_eq(a, b)), "going on from {since}, the chunks before the cut the ones they were");
            anyhow::ensure!(texts(&m.events) == texts(&want) && m.events.len() == usize::try_from(since)?, "going on from {since}: as retain keeps them");
            let index = m.index.clone();
            m.reindex();
            anyhow::ensure!(m.index == index && index.len() == m.events.len(), "going on from {since}, its index as one made anew: {} entries", index.len());
            for q in since + 1..=since + 3 {
                m.rev += 1;
                m.upsert(entry(&format!("e{q}"), q));
            }
            let seqs: Vec<u64> = m.events.metas().filter_map(|meta| meta.seq()).collect();
            anyhow::ensure!(seqs == (1..=since + 3).collect::<Vec<_>>(), "going on from {since}, it goes on: {seqs:?}");
            anyhow::ensure!(m.index.get(&m.events, &format!("e{}", since + 3)) == Some(m.events.len().saturating_sub(1)), "the newest indexed where it is");
        }
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: where those an Open keeps aren't the copy's first events (one without
    /// a number among them), its events and index are made again, as before.
    #[test]
    fn g9_s3_a_copy_whose_kept_events_arent_its_first_is_made_again() {
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        let take = |m: &mut Mirror, e: Arc<Entry>| {
            m.rev += 1;
            m.upsert(e);
        };
        for q in 1..=5 {
            take(&mut m, entry(&format!("e{q}"), q));
        }
        take(&mut m, Arc::new(Entry::new(json!({"id": "loose", "kind": "notice", "text": "no number"}))));
        for q in 6..=8 {
            take(&mut m, entry(&format!("e{q}"), q));
        }
        m.keep_through(7);
        let ids: Vec<String> = m.events.metas().map(|meta| meta.id().to_string()).collect();
        assert_eq!(ids, ["e1", "e2", "e3", "e4", "e5", "e6", "e7"]);
        let index = m.index.clone();
        m.reindex();
        assert_eq!(m.index, index, "its index made anew");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: letting go of a long copy's oldest where the cut falls in a compact chunk
    /// reads nothing again at once: the chunks wholly before it go, and it stays as it was; made again of its rest
    /// (as its owner has it read off the shard) and taken in, the copy holds what `drain_front` leaves, the chunks
    /// past the cut the ones they were. A cut on a chunk's edge, or in one that isn't compact, is `drain_front`'s;
    /// and one taken in after its chunk changed changes nothing.
    #[test]
    fn g9_s3_a_cut_in_a_compact_chunk_reads_nothing_again_at_once() -> anyhow::Result<()> {
        let n = u64::try_from(CHUNK * 4 + 9)?;
        let texts = |e: &Events| e.iter().map(|e| e.entry.text().to_owned()).collect::<Vec<_>>();
        let compact = |n: u64| -> anyhow::Result<Events> {
            let mut events = Events::default();
            for s in 1..=n {
                let _ = events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
            }
            for c in events.chunks().iter().filter(|c| c.is_full()) {
                c.pack()?;
            }
            events.compact(None);
            Ok(events)
        };
        let cut = CHUNK + 5;
        let mut events = compact(n)?;
        let before = events.chunks().to_vec();
        let mut want = events.clone();
        let _ = want.drain_front(cut);
        let was = inflated();
        let (gone, chunk, left) = events.drain_whole(cut).ok_or_else(|| anyhow::anyhow!("a cut in a compact chunk is left for later"))?;
        anyhow::ensure!(inflated() == was, "a chunk was read again at once");
        anyhow::ensure!(gone == CHUNK && left == 5 && before.get(1).is_some_and(|c| Arc::ptr_eq(c, &chunk)), "{gone} gone, {left} left");
        anyhow::ensure!(events.chunks().first().is_some_and(|c| Arc::ptr_eq(c, &chunk)) && events.len() == CHUNK * 3 + 9, "the chunk the cut fell in stays as it was");
        // As its owner has it made again, off the shard.
        let made = Chunk::of(chunk.evs()?.iter().skip(left).cloned().collect());
        let trimmed = events.trim_first(&chunk, left, made).ok_or_else(|| anyhow::anyhow!("not taken in"))?;
        anyhow::ensure!(trimmed == left && texts(&events) == texts(&want) && events.len() == want.len(), "as drain_front leaves it");
        anyhow::ensure!(events.chunks().iter().skip(1).zip(before.iter().skip(2)).all(|(a, b)| Arc::ptr_eq(a, b)), "past the cut, the chunks as they were");
        // On a chunk's edge, and in the open chunk: drain_front's.
        let mut edge = compact(n)?;
        anyhow::ensure!(edge.drain_whole(CHUNK * 2).is_none() && edge.len() == usize::try_from(n)?, "a cut on an edge reads nothing again anyway");
        anyhow::ensure!(edge.drain_whole(CHUNK * 4 + 3).is_none(), "nor one in the open chunk");
        // Its chunk changed before it's taken in: nothing changes.
        let mut changed = compact(n)?;
        let (_, chunk, left) = changed.drain_whole(cut).ok_or_else(|| anyhow::anyhow!("left for later"))?;
        anyhow::ensure!(changed.change(0, |e| e.at = 0), "an event in it changed");
        let len = changed.len();
        let made = Chunk::of(chunk.evs()?.iter().skip(left).cloned().collect());
        anyhow::ensure!(changed.trim_first(&chunk, left, made).is_none() && changed.len() == len, "taken in after its chunk changed");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a long session's copy, past its bound, lets go of its oldest on its owner's
    /// shard with no chunk read again there: the chunks before the cut go at once, and the one it fell in is made
    /// again on the blocking pool and taken in by its owner after, which then holds what it held before, from the
    /// same event, indexed where each is.
    #[test]
    fn g9_s3_a_long_copys_cut_is_made_again_off_its_owners_shard() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let copies = Copies::start_with(sim_bus(), 1, 64 * MSG);
            let bound = u64::try_from(MAX_EVENTS + MAX_EVENTS / 4)?;
            let id = "long".to_owned();
            let i = id.clone();
            copies
                .on(&id, move |s| {
                    let m = s.entry(&i, || Mirror::new(Info { id: i.clone(), ..Info::default() }, Location::Local, sim_bus()));
                    for q in 1..=bound {
                        m.rev = q;
                        m.upsert(entry(&format!("e{q}"), q));
                    }
                    for c in m.events.chunks().iter().filter(|c| c.is_full()) {
                        let _ = c.pack();
                    }
                    let home = m.home();
                    m.events.compact(home.as_ref());
                })
                .await;
            // One more, past the bound: from the first run's start after its 1001st (`run_after`), 1025 on.
            let i = id.clone();
            let at_once = copies
                .on(&id, move |s| {
                    let m = s.get_mut(&i)?;
                    let was = inflated();
                    m.rev += 1;
                    m.upsert(entry(&format!("e{}", bound + 1), bound + 1));
                    Some((inflated() - was, m.events.len(), m.events.meta(0).and_then(|x| x.seq())))
                })
                .await
                .flatten();
            let from = crate::wire::sums::run_after(1002);
            let whole = usize::try_from(from - 1)? / CHUNK * CHUNK;
            anyhow::ensure!(at_once == Some((0, usize::try_from(bound + 1)? - whole, u64::try_from(whole + 1).ok())), "{at_once:?}: read on the owner's shard, or not cut by whole chunks");
            // Taken in once it's made again.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let i = id.clone();
                let now = copies
                    .on(&id, move |s| {
                        let m = s.get_mut(&i)?;
                        let seqs: Vec<u64> = m.events.metas().filter_map(|x| x.seq()).collect();
                        let index = m.index.clone();
                        m.reindex();
                        Some((seqs, index == m.index, m.index.get(&m.events, &format!("e{}", bound + 1))))
                    })
                    .await
                    .flatten();
                if let Some((seqs, indexed, newest)) = now
                    && seqs.first() == Some(&from)
                {
                    anyhow::ensure!(seqs == (from..=bound + 1).collect::<Vec<_>>() && indexed && newest == Some(seqs.len() - 1), "{indexed} {newest:?} {seqs:?}");
                    return Ok(());
                }
                anyhow::ensure!(std::time::Instant::now() < deadline, "the chunk the cut fell in was never taken in");
                crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an update that changes an event of a long copy's compact chunk, taken by its owner,
    /// reads nothing back on the owner's shard: the chunk is read back on the blocking pool first, the shard given up
    /// meanwhile (the owner marks the chunk it read back so), and the event then changes where it is, as it did, the rest
    /// of the copy as it was. (A count of inflations on the owner's thread would count other tests' on that shard too.)
    #[test]
    fn g9_s3_a_change_to_an_old_event_reads_its_chunk_back_off_the_owners_shard() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let copies = Copies::start_with(sim_bus(), 1, 64 * MSG);
            copies.bind(&hub);
            let id = "old".to_owned();
            let n = u64::try_from(CHUNK * 3 + 9)?;
            let i = id.clone();
            let first = copies
                .on(&id, move |s| {
                    let m = s.entry(&i, || Mirror::new(Info { id: i.clone(), ..Info::default() }, Location::Local, sim_bus()));
                    for q in 1..=n {
                        m.rev = q;
                        m.upsert(entry(&format!("e{q}"), q));
                    }
                    for c in m.events.chunks().iter().filter(|c| c.is_full()) {
                        let _ = c.pack();
                    }
                    let home = m.home();
                    m.events.compact(home.as_ref());
                    m.events.chunks().first().filter(|c| c.is_compact()).cloned()
                })
                .await
                .flatten();
            let Some(first) = first else { anyhow::bail!("its first chunk compact") };
            // e5, in the first chunk, said again in other words.
            let said = Event { id: "e5".into(), seq: 5, at: 1, parent: None, by: None, body: Body::Assistant { text: "said again".into(), streaming: false } };
            anyhow::ensure!(copies.send(&id, Msg::Update(Box::new(Ingest { from: Location::Local, id: id.clone(), u: Update::Events { events: vec![said] }, shipped: true, reported: String::new() }))).await, "sent");
            copies.settled().await;
            let (i, was) = (id.clone(), Arc::clone(&first));
            let read_back = copies.on(&id, move |s| (s.thawed().upgrade().is_some_and(|c| Arc::ptr_eq(&c, &was)), s.get(&i).and_then(|m| m.events.chunks().first().map(|c| !Arc::ptr_eq(c, &was) && !c.is_compact())))).await;
            anyhow::ensure!(read_back == Some((true, Some(true))), "its chunk read back by its owner off the shard, and in its place: {read_back:?}");
            let i = id.clone();
            let now = copies
                .on(&id, move |s| s.get(&i).map(|m| (m.events.len(), m.events.get(4).map(|e| e.entry.text().to_owned()), m.events.get(5).map(|e| e.entry.text().to_owned()))))
                .await
                .flatten();
            let Some((len, Some(changed), Some(next))) = now else { anyhow::bail!("no copy: {now:?}") };
            anyhow::ensure!(len == CHUNK * 3 + 9 && changed.contains("said again") && next == entry("e6", 6).text(), "{len} {changed} {next}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an event taken under an older number than a long copy's newest, one it doesn't
    /// have (a runner that started again sends it), goes in where its number says with nothing read again on its owner's
    /// shard: the copy's events are made again with it in on the blocking pool, the shard given up meanwhile (the owner
    /// marks the event it put in so), and its update goes on from there. Every chunk was read again on the owner's
    /// shard, a compact one inflated, each event parsed.
    #[test]
    fn g9_s3_an_event_under_an_older_number_goes_in_off_the_owners_shard() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let copies = Copies::start_with(sim_bus(), 1, 64 * MSG);
            copies.bind(&hub);
            let id = "older".to_owned();
            let n = u64::try_from(CHUNK * 3 + 9)?;
            let i = id.clone();
            // Every event but the fifth, its first chunks compact.
            let compact = copies
                .on(&id, move |s| {
                    let m = s.entry(&i, || Mirror::new(Info { id: i.clone(), ..Info::default() }, Location::Local, sim_bus()));
                    for q in (1..=n).filter(|q| *q != 5) {
                        m.rev = q;
                        m.upsert(entry(&format!("e{q}"), q));
                    }
                    for c in m.events.chunks().iter().filter(|c| c.is_full()) {
                        let _ = c.pack();
                    }
                    let home = m.home();
                    m.events.compact(home.as_ref());
                    m.events.chunks().first().is_some_and(|c| c.is_compact())
                })
                .await;
            anyhow::ensure!(compact == Some(true), "its first chunk compact");
            // The fifth, and the newest after it, in one update.
            let event = |q: u64, text: &str| Event { id: format!("e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } };
            let update = Update::Events { events: vec![event(5, "the fifth, late"), event(n + 1, "the newest")] };
            anyhow::ensure!(copies.send(&id, Msg::Update(Box::new(Ingest { from: Location::Local, id: id.clone(), u: update, shipped: true, reported: String::new() }))).await, "sent");
            copies.settled().await;
            // Put in by its owner off the shard, as it marks it. (A count of inflations on the owner's thread would count
            // other tests' on that shard too.)
            let i = id.clone();
            let put_in = copies.on(&id, move |s| s.tried_insert(&i)).await.flatten();
            anyhow::ensure!(put_in.as_deref() == Some("e5"), "put in by its owner off the shard: {put_in:?}");
            let i = id.clone();
            let now = copies
                .on(&id, move |s| {
                    s.get(&i).map(|m| {
                        let at = |e: &str| m.index.get(&m.events, e);
                        let text = |k: usize| m.events.get(k).map(|e| e.entry.text().to_owned());
                        (m.events.len(), at("e4"), at("e5"), at("e6"), text(4), m.events.last().map(|e| e.entry.text().to_owned()))
                    })
                })
                .await
                .flatten();
            let total = usize::try_from(n + 1)?;
            let Some((len, e4, e5, e6, fifth, newest)) = now else { anyhow::bail!("no copy") };
            anyhow::ensure!(e4 == Some(3) && e5 == Some(4) && e6 == Some(5), "in where its number says: e4 {e4:?}, e5 {e5:?}, e6 {e6:?}");
            anyhow::ensure!(len == total && fifth.as_deref().is_some_and(|t| t.contains("the fifth, late")) && newest.as_deref().is_some_and(|t| t.contains("the newest")), "{len} {fifth:?} {newest:?}");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: an event's id is kept in its meta, with no allocation of its own up to
    /// `ID_INLINE` bytes (a UUID written either way, a harness's ids), a longer one boxed, each read back as it was;
    /// a meta takes 96 bytes, against 72 and the 48 to 64 its id's `Arc<str>` took besides.
    #[test]
    fn g11_s5_an_id_is_kept_in_its_meta() {
        let (kilns, accented, long) = (crate::util::new_id(), "é".repeat(ID_INLINE / 2), "x".repeat(ID_INLINE + 1));
        for id in ["", "e1", "msg_01ABCDEFGHJKMNPQRSTVWXYZ", "0192f3a4-5b6c-7d8e-9f01-23456789abcd", kilns.as_str(), accented.as_str(), long.as_str()] {
            let kept = Id::of(id);
            assert_eq!(kept.as_str(), id, "read back as it was");
            assert_eq!(matches!(kept, Id::Inline { .. }), id.len() <= ID_INLINE, "{id:?} ({} bytes) kept in the meta", id.len());
            let m = Meta::of(&Ev { entry: entry(id, 1), at: 0 });
            assert_eq!(m.id(), id);
        }
        assert!(size_of::<Id>() <= 40 && size_of::<Meta>() <= 96, "an id {} bytes, a meta {}", size_of::<Id>(), size_of::<Meta>());
    }

    /// Goal 11, plan 01a1a87b2e56 step 5 (g11-heap): a meta's digest takes 8 bytes, where a `OnceLock<u64>` took 16:
    /// none until it's made, then kept, cloned with its meta and compared as the `OnceLock`'s was. A meta was 88
    /// bytes then, against 96 (72 now: `g11_s5_heap_a_meta_is_seventy_two_bytes`).
    #[test]
    fn g11_s5_heap_a_metas_digest_takes_eight_bytes() {
        let ev = Ev { entry: entry("e1", 1), at: 0 };
        let m = Meta::of(&ev);
        assert_eq!(m.sum_made(), None, "none made as it came in");
        let made = m.sum(&ev);
        assert_eq!((made, m.sum_made()), (ev.entry.sum(), Some(ev.entry.sum())), "made, then kept");
        let copy = m.clone();
        assert!(copy.sum_made() == Some(made) && copy == m, "a clone keeps it");
        assert_ne!(Meta::of(&ev), m, "one made and one not aren't equal, as with a OnceLock");
        assert!(size_of::<Meta>() <= 88, "a meta takes {} bytes", size_of::<Meta>());
    }

    /// Goal 11, plan 01a1a87b2e56 step 5 (g11-heap): the index's bucket is 8 bytes (half of an id's keyed hash and its
    /// event's place, each a `u32`), where it was 16; its table hashes a key by the key's own bits, in both halves of
    /// the `u64` (a bucket picked by the low ones, tagged by the top seven); a place past a `u32` is kept apart by
    /// its id; and every id is found where it is. A 5,000-event copy's table of 8,192 buckets is 72 KiB, not 136.
    #[test]
    fn g11_s5_heap_the_indexs_bucket_is_eight_bytes() {
        use std::hash::Hasher as _;
        assert_eq!(size_of::<(u32, u32)>(), 8);
        let mut spread = Spread::default();
        spread.write_u32(0x8000_0001);
        assert_eq!(spread.finish(), 0x8000_0001_8000_0001, "the key in both halves");
        let mut events = Events::default();
        for q in 1..=3 {
            events.push(Ev { entry: entry(&format!("e{q}"), q), at: 0 });
        }
        let mut index = Index::of(&events);
        index.insert(&events, "far", usize::MAX);
        assert_eq!((index.apart.get("far"), index.get(&events, "far")), (Some(&usize::MAX), Some(usize::MAX)), "a place past a bucket's, kept apart");
        for q in 1..=3_u64 {
            assert_eq!(index.get(&events, &format!("e{q}")), usize::try_from(q - 1).ok(), "e{q}");
        }
        index.drop_front(1);
        assert_eq!((index.get(&events, "e1"), index.by_hash.len()), (None, 2), "the oldest let go of");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5 (g11-heap): a meta is 72 bytes. It keeps no offset (where an event's text
    /// begins is reckoned from the lengths before it), and whether its event has a number in its length's top bit,
    /// where an `Option<u64>` took 16 bytes. A compact chunk's 70 metas take 5,040 bytes, within jemalloc's
    /// 5,120-byte size class, where at 88 bytes they took 6,160 (its 7,168-byte class). Numbers (none, 0, the most
    /// a `u64` holds), lengths, offsets and spans read as the texts joined say.
    #[test]
    fn g11_s5_heap_a_meta_is_seventy_two_bytes() -> anyhow::Result<()> {
        assert_eq!(size_of::<Meta>(), 72, "a meta takes {} bytes", size_of::<Meta>());
        assert!(CHUNK * size_of::<Meta>() <= 5 << 10, "a chunk's metas take {} bytes", CHUNK * size_of::<Meta>());
        for seq in [1, 0, u64::MAX] {
            let ev = Ev { entry: entry("e", seq), at: 3 };
            let m = Meta::of(&ev);
            assert_eq!((m.seq(), m.text_len(), m.at), (Some(seq), ev.entry.text().len(), 3), "numbered {seq}");
        }
        let bare = Ev { entry: Arc::new(Entry::new(json!({"id": "n", "kind": "notice", "text": "no number"}))), at: 0 };
        let m = Meta::of(&bare);
        assert_eq!((m.seq(), m.text_len()), (None, bare.entry.text().len()), "no number");
        let evs: Vec<Ev> = (1..=5).map(|q| Ev { entry: entry(&format!("e{q}"), q * 1_000), at: q }).collect();
        let chunk = Chunk::of(evs.clone());
        let joined = String::from_utf8(chunk.joined()?.into_owned())?;
        anyhow::ensure!(chunk.texts_len() == joined.len(), "its texts joined: {} bytes, not {}", joined.len(), chunk.texts_len());
        let texts: Vec<&str> = evs.iter().map(|e| e.entry.text()).collect();
        for (k, text) in texts.iter().enumerate() {
            anyhow::ensure!(chunk.off(k) == texts.iter().take(k).map(|t| t.len() + 1).sum::<usize>(), "event {k} begins past those before it");
            anyhow::ensure!(chunk.span(k, 1).and_then(|r| joined.get(r)) == Some(*text), "event {k}'s text");
        }
        anyhow::ensure!(chunk.span(1, 3).and_then(|r| joined.get(r)) == texts.get(1..4).map(|t| t.join(",")).as_deref(), "events 1 to 3 joined");
        anyhow::ensure!(chunk.span(5, 1).is_none() && chunk.span(0, 0).is_none() && chunk.span(4, 2).is_none(), "nothing past its events");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5 (g11-heap): the newest compact chunk keeps of its texts only those its copy's
    /// newest page's reads reached back to. With nine events in the open chunk, a page of 30 keeps the compact chunk's
    /// last 21 (inflated once); a page of 20 reaches less far back and takes a slice of them (nothing inflated); a page
    /// of 50 reaches further, inflates once more and keeps its last 41; a page of 30 again takes a slice of those. Each
    /// page reads as the events' texts joined, and a page that reaches past the newest compact chunk keeps nothing of
    /// the older one.
    #[test]
    fn g11_s5_heap_a_page_keeps_the_texts_its_reads_reach_back_to() -> anyhow::Result<()> {
        let inflated = || super::super::packed::INFLATED.with(std::cell::Cell::get);
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        // Two sealed chunks and the newest open with nine.
        let n = u64::try_from(CHUNK * 2 + 9)?;
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
        }
        for c in m.events.chunks().iter().filter(|c| c.is_full()) {
            c.pack()?;
        }
        m.events.compact(None);
        m.publish();
        let view = m.view();
        let texts: Vec<String> = view.events.iter().map(|e| e.entry.text().to_owned()).collect();
        let page = |limit: usize| format!(r#"{{"events":[{}]}}"#, texts.get(texts.len().saturating_sub(limit)..).unwrap_or_default().join(","));
        let (older, compact) = (view.events.chunks().first().ok_or_else(|| anyhow::anyhow!("no chunk"))?, view.events.chunks().get(1).ok_or_else(|| anyhow::anyhow!("one chunk"))?);
        anyhow::ensure!(compact.is_compact() && older.is_compact(), "two compact chunks");
        // The bytes of the compact chunk's texts joined from its last `events` on.
        let suffix = |events: usize| -> Option<usize> { Some(compact.texts_len() - compact.off(compact.metas().len().checked_sub(events)?)) };
        let read = |limit: usize| -> anyhow::Result<()> {
            let got = view.events_json(None, limit).ok_or_else(|| anyhow::anyhow!("a page of {limit} from memory"))?;
            anyhow::ensure!(String::from_utf8_lossy(&got) == page(limit), "a page of {limit}: the events' texts joined");
            Ok(())
        };
        let was = inflated();
        read(30)?;
        anyhow::ensure!(inflated() == was + 1 && compact.page_kept() == suffix(21), "a page of 30: its last 21 kept ({:?})", compact.page_kept());
        read(20)?;
        anyhow::ensure!(inflated() == was + 1 && compact.page_kept() == suffix(21), "a page of 20: a slice of them, nothing inflated");
        read(50)?;
        anyhow::ensure!(inflated() == was + 2 && compact.page_kept() == suffix(41), "a page of 50: inflated once more, its last 41 kept ({:?})", compact.page_kept());
        read(30)?;
        anyhow::ensure!(inflated() == was + 2 && compact.page_kept() == suffix(41), "a page of 30 again: a slice of the longer");
        anyhow::ensure!(suffix(41) < suffix(70), "less than the whole chunk's texts");
        // A page into the older compact chunk: it's inflated for that read and keeps nothing.
        read(100)?;
        anyhow::ensure!(older.page_kept().is_none() && compact.page_kept() == suffix(70), "the older keeps nothing; the newest compact chunk's whole kept");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the index finds each id where it is, checked against the id its event's
    /// meta keeps; an id whose hash another's place already has is kept apart by itself and found there; places move
    /// down as the oldest go, those let go of leave it, and it stays what one made anew from the events is.
    #[test]
    fn g11_s5_the_index_finds_each_id_and_keeps_clashing_ones_apart() {
        let mut events = Events::default();
        for q in 1..=5 {
            events.push(Ev { entry: entry(&format!("e{q}"), q), at: 0 });
        }
        let mut index = Index::of(&events);
        for q in 1..=5_u64 {
            assert_eq!(index.get(&events, &format!("e{q}")), usize::try_from(q - 1).ok(), "e{q}");
        }
        assert_eq!(index.get(&events, "e9"), None);
        // e3's hash already finding e1's place, as a clash would have it: e3 kept apart, and found there.
        let mut clashed = Index::default();
        clashed.by_hash.insert(Index::hash("e3"), 0);
        clashed.insert(&events, "e3", 2);
        assert_eq!((clashed.apart.get("e3"), clashed.get(&events, "e3"), clashed.get(&events, "e1")), (Some(&2), Some(2), None));
        clashed.drop_front(1);
        assert!(clashed.by_hash.is_empty() && clashed.apart.get("e3") == Some(&1), "{clashed:?}");
        // The oldest two go from the events and the index alike.
        let (gone, _) = events.drain_front(2);
        index.drop_front(gone);
        for q in 3..=5_u64 {
            assert_eq!(index.get(&events, &format!("e{q}")), usize::try_from(q - 3).ok(), "e{q}");
        }
        assert_eq!(index.get(&events, "e1"), None);
        assert_eq!(index, Index::of(&events), "as one made anew");
        // Those past a place go (a copy kept through a number).
        index.retain(|at| *at < 1);
        assert_eq!((index.len(), index.get(&events, "e3"), index.get(&events, "e4")), (1, Some(0), None));
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a copy that takes events past `MAX_EVENTS` and a quarter lets go of its
    /// oldest, and its index, moved down by as many, finds each event it keeps where it is and none it let go of, as
    /// an index made anew from its events does; an event taken again by its id changes where it is.
    #[test]
    fn g11_s5_a_copys_index_moves_as_its_oldest_go() {
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        let n = u64::try_from(MAX_EVENTS + MAX_EVENTS / 4 + 10).unwrap_or(u64::MAX);
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
        }
        assert!(m.events.len() < usize::try_from(n).unwrap_or(0) && m.events.meta(0).and_then(|x| x.seq()).is_some_and(|q| q > 1), "its oldest went");
        assert_eq!(m.index.get(&m.events, "e1"), None, "one let go of isn't found");
        for (at, meta) in m.events.metas().enumerate() {
            assert_eq!(m.index.get(&m.events, meta.id()), Some(at), "{}", meta.id());
        }
        let kept = m.index.clone();
        m.reindex();
        assert_eq!(m.index, kept, "as one made anew");
        let len = m.events.len();
        m.upsert(entry(&format!("e{n}"), n));
        assert_eq!(m.events.len(), len, "taken again where it is");
    }

    /// A turn begun at `t0`, as `Turn::new` begins one.
    fn turn_at(t0: Instant) -> Turn {
        Turn { began: t0, looked: Cell::new(t0), longest: Cell::new(Duration::ZERO) }
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a turn takes pieces while the next (as long as the longest yet, or as what's expected
    /// of it where that's more) and what ends its message would still end within the quantum, so its poll ends within it.
    /// Pieces of 20 µs with a commit of 40 µs after them stop at 220 µs of a 266.7 µs quantum, the commit ending at 260 µs
    /// (one more piece would have ended it at 280); a next piece expected to take 100 µs stops them at 140 µs. A message is
    /// taken where what it's expected to take still ends within the quantum. Off a shard nothing ends a turn.
    #[test]
    fn g9_s3_a_turn_ends_within_its_quantum_with_room_for_what_ends_it() {
        let us = Duration::from_micros;
        let quantum = Duration::from_nanos(crate::rt::STEP_BUDGET_NS);
        let stops = |piece: Duration, next: Duration, room: Duration| -> Option<Duration> {
            let t0 = Instant::now();
            let turn = turn_at(t0);
            let mut at = t0;
            for _ in 0..1000 {
                at = at.checked_add(piece)?;
                if turn.over_at(at, Some(quantum), next, room) {
                    return Some(at.saturating_duration_since(t0));
                }
            }
            None
        };
        let ended = stops(us(20), Duration::ZERO, us(40));
        assert_eq!(ended, Some(us(220)), "pieces of 20 µs, a commit of 40 µs after them");
        assert!(ended.is_some_and(|at| at + us(40) <= quantum && at + us(20) + us(40) > quantum), "the commit ends within the quantum, and one more piece wouldn't have");
        assert_eq!(stops(us(20), us(100), us(40)), Some(us(140)), "a next piece expected to take 100 µs");
        assert_eq!(stops(us(300), Duration::ZERO, Duration::ZERO), Some(us(300)), "a piece past the quantum ends its turn at once");
        let t0 = Instant::now();
        let turn = turn_at(t0);
        assert!(turn.fits_at(t0 + us(200), Some(quantum), us(60)), "60 µs from 200 µs ends within it");
        assert!(!turn.fits_at(t0 + us(210), Some(quantum), us(60)), "from 210 µs it wouldn't");
        assert!(turn.fits_at(t0 + Duration::from_secs(1), None, Duration::from_secs(1)) && !turn.over_at(t0 + Duration::from_secs(1), None, Duration::from_secs(1), Duration::from_secs(1)), "off a shard nothing ends a turn");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a piece is expected to take is estimated as RFC 6298 estimates a round trip:
    /// nothing until one was taken, then its mean and four of its mean deviations (the first sample the mean, half of it the
    /// deviation), down to the mean as samples agree, up past an outlier at once, and back down as they agree again.
    #[test]
    fn g9_s3_what_a_piece_takes_is_expected_as_rfc_6298_expects_a_round_trip() {
        let us = Duration::from_micros;
        let mut e = Expect::default();
        assert_eq!(e.expected(), Duration::ZERO, "none expected until one was taken");
        e.learn(100_000);
        assert_eq!(e.expected(), us(300), "the first: itself and four half-samples");
        e.learn(100_000);
        assert_eq!(e.expected(), us(250), "its deviation by a quarter toward none");
        for _ in 0..200 {
            e.learn(100_000);
        }
        assert!(e.expected() < us(101), "samples that agree: their mean, {:?}", e.expected());
        e.learn(1_000_000);
        assert!(e.expected() > Duration::from_millis(1), "past an outlier at once: {:?}", e.expected());
        for _ in 0..100 {
            e.learn(100_000);
        }
        assert!(e.expected() < us(110), "back down as they agree again: {:?}", e.expected());
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what an owner expects a message to take is what its kind's starts took (nothing
    /// before one did), and for an update's events the tail after them too (its commit), its first event's writing where
    /// that's more; a turn takes it while that still ends within its quantum, and leaves it for its next turn otherwise.
    #[test]
    fn g9_s3_a_turn_takes_a_message_while_what_its_expected_to_take_ends_within_it() {
        use crate::harness::model::{Body, Event};
        let us = Duration::from_micros;
        let job = || Msg::Run(Box::new(|_: &mut Shard| {}));
        let update = |u: Update| Msg::Update(Box::new(Ingest { from: Location::Local, id: "s".into(), u, shipped: true, reported: String::new() }));
        let first = Event { id: "e1".into(), seq: 1, at: 1, parent: None, by: None, body: Body::Assistant { text: "said".into(), streaming: false } };
        let events = || update(Update::Events { events: vec![first.clone()] });
        let mut costs = Costs::default();
        assert_eq!(costs.expected(&job()), Duration::ZERO, "nothing before one was taken");
        costs.learn(label_of(&job()), 120_000);
        assert_eq!(costs.expected(&job()), us(360), "a job: its starts' mean and four deviations");
        costs.learn(label_of(&events()), 20_000);
        costs.tail.learn(30_000);
        // Its first event's writing is learned by every owner as they write (the tests beside this one's too): a few bytes', far
        // under the start learned here.
        let got = costs.expected(&events());
        assert!(got >= us(60) + us(90) && got < us(60) + us(90) + Duration::from_millis(1), "an update's events: its start, then its tail: {got:?}");
        assert_eq!(costs.expected(&update(Update::Closed)), Duration::ZERO, "another kind isn't these");
        let t0 = Instant::now();
        let (turn, quantum) = (turn_at(t0), Some(Duration::from_nanos(crate::rt::STEP_BUDGET_NS)));
        assert!(!turn.fits_at(t0 + us(150), quantum, costs.expected(&job())), "a job expected to take 360 µs waits for the next turn");
        assert!(turn.fits_at(t0 + us(100), quantum, us(150)), "one expected to take 150 µs from 100 µs goes in this one");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an event under a number its copy has with another id (the timeline went another way)
    /// takes that one's place, and the copy's index lets go of the one that was there and finds this one, every other one
    /// where it was, as an index made anew finds them: the index was made anew for each such event, on its owner's shard.
    #[test]
    fn g9_s3_an_event_in_anothers_place_moves_the_index_there_alone() {
        let mut m = Mirror::new(Info { id: "s".into(), ..Info::default() }, Location::Local, sim_bus());
        let n = u64::try_from(CHUNK * 2 + 5).unwrap_or(u64::MAX);
        for q in 1..=n {
            m.rev = q;
            m.upsert(entry(&format!("e{q}"), q));
        }
        let held = m.index.len();
        assert!(m.upsert(entry("x7", 7)), "said as the timeline going another way");
        assert_eq!((m.index.get(&m.events, "x7"), m.index.get(&m.events, "e7")), (Some(6), None), "this one where that one was");
        assert_eq!((m.index.len(), m.events.len()), (held, usize::try_from(n).unwrap_or(0)), "one in another's place");
        for (at, meta) in m.events.metas().enumerate() {
            assert_eq!(m.index.get(&m.events, meta.id()), Some(at), "{}", meta.id());
        }
        assert_eq!(m.index, Index::of(&m.events), "as one made anew");
        // One the copy has under that number with that id already is changed where it is, as before.
        assert!(!m.upsert(entry("x7", 7)) && m.index.get(&m.events, "x7") == Some(6));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an event written out off its owner's shard is what `Registry::apply` writes on it: the
    /// same entry, the same row for the conversation store (where one is kept), the same files it mentions.
    #[test]
    fn g9_s3_an_event_written_off_the_shard_is_as_one_written_on_it() {
        use crate::harness::model::{Body, Event};
        let e = Event { id: "e1".into(), seq: 1, at: 1, parent: None, by: None, body: Body::Assistant { text: "see `/work/shot.png` for it".into(), streaming: false } };
        let made = made_of("s", &e, true, "/work", &Location::Local);
        let mut row = None;
        let here = Entry::of_event(&e, Some(&mut |text: String| row = Some(crate::convo::rows::Row::Event(Box::new(crate::convo::rows::event_written("s", &e, text))))));
        assert_eq!(made.entry.text(), here.text());
        assert_eq!((made.entry.id(), made.entry.seq(), made.entry.kind()), (here.id(), here.seq(), here.kind()));
        assert!(made.row.is_some() && made.row == row, "its row as apply makes it");
        assert!(made.mentions.as_ref().is_some_and(|j| j.paths == ["/work/shot.png"] && j.seq == 1), "{:?}", made.mentions);
        assert!(made_of("s", &e, false, "/work", &Location::Local).row.is_none(), "none where no row is kept");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an owner takes a long update in turns that each give its shard up: a task on the same
    /// shard that always wants it runs between them, about a quantum apart (the median gap within four quanta, whatever the
    /// host does to a few of them; the update whole took a poll of tens of milliseconds), and a job sent behind the update
    /// finds every one of its events in the copy, in order, none lost.
    #[test]
    fn g9_s3_an_owner_takes_a_long_update_in_turns_that_give_its_shard_up() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let copies = Arc::new(Copies::start_with(sim_bus(), 1, 64 * MSG));
            copies.bind(&hub);
            let id = "turns".to_owned();
            let i = id.clone();
            copies
                .on(&id, move |s| {
                    s.entry(&i, || Mirror::new(Info { id: i.clone(), ..Info::default() }, Location::Local, sim_bus()));
                })
                .await;
            // A task on the owner's shard that always wants it, noting when it runs.
            let (stop, runs) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)));
            let (tx, rx) = futures::channel::oneshot::channel::<Vec<Instant>>();
            let witness = {
                let (stop, runs) = (Arc::clone(&stop), Arc::clone(&runs));
                async move {
                    let mut at = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        at.push(Instant::now());
                        runs.fetch_add(1, Ordering::SeqCst);
                        hyper_rt::futures::yield_now().await;
                    }
                    let _ = tx.send(at);
                }
            };
            anyhow::ensure!(crate::rt::spawn_on(copies.owner_of(&id), witness), "a task on the owner's shard");
            let read = |copies: Arc<Copies>, id: String, runs: Arc<AtomicU64>| async move {
                let i = id.clone();
                copies.on(&id, move |s| s.get(&i).map(|m| (m.rev, m.events.metas().filter_map(|x| x.seq()).collect::<Vec<u64>>(), runs.load(Ordering::SeqCst)))).await.flatten()
            };
            let Some((rev, _, ran)) = read(Arc::clone(&copies), id.clone(), Arc::clone(&runs)).await else { anyhow::bail!("no copy") };
            let n = u64::try_from(MAX_EVENTS)?;
            let said = |q: u64| Event { id: format!("e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text: format!("{q} {}", "word ".repeat(40)), streaming: false } };
            let update = Ingest { from: Location::Local, id: id.clone(), u: Update::Events { events: (1..=n).map(said).collect() }, shipped: true, reported: String::new() };
            anyhow::ensure!(copies.send(&id, Msg::Update(Box::new(update))).await, "sent");
            let after = read(Arc::clone(&copies), id.clone(), Arc::clone(&runs)).await;
            stop.store(true, Ordering::SeqCst);
            let at = rx.await?;
            let Some((now, seqs, ran_after)) = after else { anyhow::bail!("no copy") };
            anyhow::ensure!(seqs == (1..=n).collect::<Vec<u64>>(), "every event, in order: {} of {n}", seqs.len());
            // Each turn of the update took its events in one apply, and counted one revision.
            let turns = now.saturating_sub(rev);
            let between = ran_after.saturating_sub(ran);
            anyhow::ensure!(turns >= 4 && between >= turns / 2, "{turns} turns, the other task run {between} times between them");
            let during = at.get(usize::try_from(ran)?..usize::try_from(ran_after)?).unwrap_or_default();
            let mut gaps: Vec<Duration> = during.windows(2).filter_map(|w| Some(w.get(1)?.saturating_duration_since(*w.first()?))).collect();
            gaps.sort();
            let median = gaps.get(gaps.len() / 2).copied().unwrap_or_default();
            let quantum = Duration::from_nanos(hyper_rt::futures::step_budget_ns().unwrap_or(crate::rt::STEP_BUDGET_NS));
            anyhow::ensure!(median <= quantum * 4, "the other task ran {median:?} apart at the median ({} gaps), the quantum {quantum:?}", gaps.len());
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: an event long to write out (a large tool output's megabytes) is written by its owner off
    /// its shard, as the owner counts it, and taken as it was written there: in its place in its update, those before it written
    /// on the shard as they were, those after it taken after it, none lost, each where its index says.
    #[test]
    fn g9_s3_an_event_long_to_write_out_is_written_off_its_owners_shard() -> anyhow::Result<()> {
        use crate::harness::model::{Body, Event};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let copies = Copies::start_with(sim_bus(), 1, 64 * MSG);
            copies.bind(&hub);
            let id = "large".to_owned();
            let i = id.clone();
            copies
                .on(&id, move |s| {
                    s.entry(&i, || Mirror::new(Info { id: i.clone(), ..Info::default() }, Location::Local, sim_bus()));
                })
                .await;
            let said = |q: u64, text: String| Event { id: format!("e{q}"), seq: q, at: 1, parent: None, by: None, body: Body::Assistant { text, streaming: false } };
            let update = |events: Vec<Event>| Msg::Update(Box::new(Ingest { from: Location::Local, id: id.clone(), u: Update::Events { events }, shipped: true, reported: String::new() }));
            // Short ones, written on the shard, teach what writing one out takes.
            anyhow::ensure!(copies.send(&id, update((1..=8).map(|q| said(q, format!("line {q}"))).collect())).await, "sent");
            let was = copies.on(&id, |s| s.written_off()).await.unwrap_or_default();
            let large = "x".repeat(8 << 20);
            anyhow::ensure!(long_to_write(&said(10, large.clone())), "8 MB expected to take past a slow poll to write out");
            anyhow::ensure!(copies.send(&id, update(vec![said(9, "before".into()), said(10, large.clone()), said(11, "after".into())])).await, "sent");
            let i = id.clone();
            let now = copies
                .on(&id, move |s| {
                    let off = s.written_off();
                    s.get(&i).map(|m| (off, m.events.metas().filter_map(|x| x.seq()).collect::<Vec<u64>>(), m.events.get(9).map(|e| e.entry.text().len()), m.index.get(&m.events, "e10"), m.index.get(&m.events, "e11")))
                })
                .await
                .flatten();
            let Some((off, seqs, len, at, after)) = now else { anyhow::bail!("no copy") };
            anyhow::ensure!(off == was + 1, "written off its owner's shard once: {was} then {off}");
            anyhow::ensure!(seqs == (1..=11).collect::<Vec<u64>>(), "in order, none lost: {seqs:?}");
            anyhow::ensure!(len.is_some_and(|l| l > large.len()) && at == Some(9) && after == Some(10), "taken as written, in its place: {len:?} {at:?} {after:?}");
            Ok(())
        })
    }
}
