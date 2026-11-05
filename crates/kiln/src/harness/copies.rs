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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use arc_swap::ArcSwap;
use bytes::Bytes;
use flate2::Compression;
use serde_json::Value;

use super::core::{MAX_EVENTS, Update};
use super::live::{Feed, Payload, Tap};
use super::packed::Segment;
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

/// A session's newest events its copy keeps in memory, whatever else of it is spilled (`super::spill`; goal
/// 11, plan 01a1a87b2e56 step 5): the events API's page as it's asked for without a `limit` (`GET
/// /api/harness/sessions/{id}/events`, `crate::api`'s `conversation_events`), which is what a client's first
/// read asks for at the most (the lab's readers ask for 50), so the API answers it from memory as before
/// (goal 10, plan 01a1a87b2dae step 2). A sealed chunk past it, once packed, goes to its copy's spill file:
/// a copy keeps its index, its open chunk and the chunks among these (two at the most, of [`CHUNK`]).
pub const EVENTS_PAGE: usize = 100;

/// What a reader needs of one of a copy's events without its text: its id and number, the copy's
/// revision when it last changed, where its text is among its chunk's texts joined by `,` (as an
/// `events` frame joins them), and its text's digest (`crate::wire::sums::event_text`). Numbering,
/// finding, resuming, digests and a snapshot's plan read these alone. Goal 10, plan 01a1a87b2dae step
/// 3, in the shape agreed with goal 11, whose stored form keeps these and the chunk's [`Segment`] and
/// decodes an event's text only where it's read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Empty for an event without one (none the copy takes in: `Mirror::upsert`).
    pub id: Arc<str>,
    pub seq: Option<u64>,
    pub at: u64,
    /// Its text: from `off`, `len` bytes, in its chunk's texts joined.
    pub off: usize,
    pub len: usize,
    /// Its text's digest: made off the owner's shard, a chunk's all as it's packed ([`Chunk::pack`]:
    /// a packed chunk's are all there), or where first asked for ([`Meta::sum`]). Made as the copy
    /// took each event in, it held the owner's shard for a whole fill's text (goal 10's A/B).
    sum: OnceLock<u64>,
}

impl Meta {
    fn of(ev: &Ev, off: usize) -> Self {
        Self { id: Arc::from(ev.entry.id().unwrap_or_default()), seq: ev.entry.seq(), at: ev.at, off, len: ev.entry.text().len(), sum: OnceLock::new() }
    }

    /// Its text's digest, `ev`'s (the event it's the meta of), made now if it wasn't yet.
    pub fn sum(&self, ev: &Ev) -> u64 {
        *self.sum.get_or_init(|| ev.entry.sum())
    }

    /// Its text's digest, if it's been made.
    pub fn sum_made(&self) -> Option<u64> {
        self.sum.get().copied()
    }

    /// Where the next event's text begins: past this one's and the `,` after it.
    fn end(&self) -> usize {
        self.off + self.len + 1
    }
}

/// Bytes the copies' chunks hold deflated ([`Chunk::pack`]): what packing costs in memory, which goal
/// 11 exports and bounds (`kiln_memory_bytes{owner="session_segments"}`).
pub static SEGMENT_BYTES: AtomicUsize = AtomicUsize::new(0);

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
        Self { meta: self.meta.clone(), evs: self.evs.clone(), packed, sealing: AtomicBool::new(false), spilled }
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        self.unpack();
    }
}

impl Chunk {
    fn of(evs: Vec<Ev>) -> Self {
        let mut off = 0;
        let meta = evs
            .iter()
            .map(|e| {
                let m = Meta::of(e, off);
                off = m.end();
                m
            })
            .collect();
        Self { meta, evs, packed: OnceLock::new(), sealing: AtomicBool::new(false), spilled: None }
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
        self.meta
            .iter()
            .map(|m| {
                let text = joined.get(m.off..m.off + m.len).and_then(|b| std::str::from_utf8(b).ok());
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
        let mut joined = Vec::with_capacity(self.meta.last().map_or(0, |m| m.off + m.len));
        for (i, e) in self.evs.iter().enumerate() {
            if i > 0 {
                joined.push(b',');
            }
            joined.extend_from_slice(e.entry.text().as_bytes());
        }
        Ok(Cow::Owned(joined))
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
        Some(Self { meta: self.meta.clone(), evs: Vec::new(), packed, sealing: AtomicBool::new(true), spilled: None })
    }

    /// Whether its segment is on disk (`super::spill`), not in memory.
    pub fn is_spilled(&self) -> bool {
        self.spilled.is_some()
    }

    /// It spilled: its metas, and where its segment went (`s`). None unless it's compact with its segment in
    /// memory (one changed since is kept as it is).
    fn spilled_as(&self, s: super::spill::Spilled) -> Option<Self> {
        (self.is_compact() && self.packed.get().is_some()).then(|| Self { meta: self.meta.clone(), evs: Vec::new(), packed: OnceLock::new(), sealing: AtomicBool::new(true), spilled: Some(s) })
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
        // Changed, it's to be packed again once it's sealed (`Events::compact`).
        *self.sealing.get_mut() = false;
    }

    /// Its event `k` changed by `f`, with its meta and where those after it begin.
    fn change(&mut self, k: usize, f: impl FnOnce(&mut Ev)) -> bool {
        if !self.thaw() {
            return false;
        }
        self.unpack();
        let Some(ev) = self.evs.get_mut(k) else { return false };
        f(ev);
        let mut off = self.meta.get(k).map_or(0, |m| m.off);
        if let (Some(m), Some(e)) = (self.meta.get_mut(k), self.evs.get(k)) {
            *m = Meta::of(e, off);
            off = m.end();
        }
        for m in self.meta.iter_mut().skip(k + 1) {
            m.off = off;
            off = m.end();
        }
        true
    }

    fn push(&mut self, ev: Ev) {
        // The newest is never compact (`Events::compact`); one read again first all the same.
        if !self.thaw() {
            return;
        }
        self.unpack();
        self.meta.push(Meta::of(&ev, self.meta.last().map_or(0, Meta::end)));
        self.evs.push(ev);
    }
}

/// A copy's events in order, in chunks ([`Chunk`]) of [`CHUNK`]: the first may hold fewer, once its
/// oldest have gone ([`Events::drain_front`]), and the newest may; each is shared, so a view takes them
/// whole by reference, and the owner's next change copies only what it touches.
#[derive(Clone, Default)]
pub struct Events {
    chunks: Arc<Vec<Arc<Chunk>>>,
    len: usize,
    /// The events in the first chunk.
    first: usize,
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
        Self { chunks: Arc::new(chunks), len, first: len.min(CHUNK) }
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
        let chunk = self.chunks.get(c)?;
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
            let Some(chunk) = self.chunks.get(c) else { continue };
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
        self.chunks.get(c)?.meta.get(k)
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
            let chunk = self.chunks.get(c)?;
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
                let (first, last) = (chunk.meta.get(k)?, chunk.meta.get(k + n - 1)?);
                out.extend_from_slice(chunk.joined().ok()?.get(first.off..last.off + last.len)?);
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

    pub fn last(&self) -> Option<Cow<'_, Ev>> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    /// Each event in order: as its chunk keeps it, or a compact chunk's read again from its segment as the
    /// iteration comes to it (a chunk that can't be is passed over, said in the log).
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Cow<'_, Ev>> + '_ {
        self.chunks.iter().flat_map(|c| {
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
        self.chunks.iter().flat_map(|c| c.meta.iter())
    }

    /// Its chunks, in order: a snapshot's pieces.
    pub fn chunks(&self) -> &[Arc<Chunk>] {
        &self.chunks
    }

    /// Event `i` changed by `f`: its chunk (and the list of chunks) copied first when a view shares
    /// it. Whether there was one.
    pub(super) fn change(&mut self, i: usize, f: impl FnOnce(&mut Ev)) -> bool {
        let Some((c, k)) = self.place(i) else { return false };
        match Arc::make_mut(&mut self.chunks).get_mut(c) {
            Some(chunk) => Arc::make_mut(chunk).change(k, f),
            None => false,
        }
    }

    /// `ev` last: the chunk that sealed as it came (full, and no longer the newest), if one did.
    pub(super) fn push(&mut self, ev: Ev) -> Option<Arc<Chunk>> {
        let chunks = Arc::make_mut(&mut self.chunks);
        let room = chunks.last().is_some_and(|c| c.len() < CHUNK);
        let only = chunks.len() == 1;
        let sealed = if room {
            if let Some(last) = chunks.last_mut() {
                Arc::make_mut(last).push(ev);
            }
            if only {
                self.first += 1;
            }
            None
        } else {
            let sealed = chunks.last().cloned();
            chunks.push(Arc::new(Chunk::of(vec![ev])));
            if chunks.len() == 1 {
                self.first = 1;
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

    /// Lets go of the first `n` (a long session's oldest, every thousand events or so): their ids,
    /// and the chunk made again of what's left of the one the cut fell in, when that one sealed. The
    /// chunks past it stay as they were, packed.
    pub(super) fn drain_front(&mut self, n: usize) -> (Vec<Arc<str>>, Option<Arc<Chunk>>) {
        let n = n.min(self.len);
        let gone: Vec<Arc<str>> = self.metas().take(n).map(|m| Arc::clone(&m.id)).collect();
        let chunks = Arc::make_mut(&mut self.chunks);
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
        self.len -= n;
        self.first = chunks.first().map_or(0, |c| c.len());
        (gone, remade.filter(|_| chunks.len() > 1))
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Meta) -> bool) {
        let mut all = Vec::with_capacity(self.len);
        for c in self.chunks.iter() {
            let evs = c.evs().map(Cow::into_owned).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "a session copy's packed events couldn't be read again: let go of");
                Vec::new()
            });
            all.extend(c.meta.iter().zip(evs).filter(|(m, _)| keep(m)).map(|(_, e)| e));
        }
        *self = Self::of(all);
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every chunk but the newest (which takes the next event) kept compact once it's packed, and those not
    /// packed yet sent to be (goal 11, plan 01a1a87b2e56 step 2). A view taking a chunk keeps it as it was
    /// until it lets go.
    pub(super) fn compact(&mut self, home: Option<&Home>) {
        let Some(sealed) = self.chunks.len().checked_sub(1) else { return };
        for c in self.chunks.iter().take(sealed).filter(|c| !c.is_compact() && c.packed().is_none()) {
            seal(c, home.cloned());
        }
        let ready: Vec<usize> = (0..sealed).filter(|i| self.chunks.get(*i).is_some_and(|c| !c.is_compact() && c.packed().is_some())).collect();
        if ready.is_empty() {
            return;
        }
        let chunks = Arc::make_mut(&mut self.chunks);
        for i in ready {
            if let Some(slot) = chunks.get_mut(i)
                && let Some(c) = slot.compacted()
            {
                *slot = Arc::new(c);
            }
        }
    }

    /// Its sealed chunks past its newest `page` events that are packed and in memory: what its copy's owner
    /// spills (`super::spill`), oldest first.
    pub(super) fn spillable(&self, page: usize) -> Vec<Arc<Chunk>> {
        let mut after = 0usize;
        let mut out = Vec::new();
        for c in self.chunks.iter().rev() {
            if after >= page && c.is_compact() && !c.is_spilled() && c.packed().is_some() {
                out.push(Arc::clone(c));
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
        let list = Arc::make_mut(&mut self.chunks);
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
static SEALED: OnceLock<async_channel::Sender<Sealed>> = OnceLock::new();

/// A chunk that sealed, to be packed off the owner's shard ([`SEALED`]), so a snapshot after it takes
/// it as it is (goal 10, plan 01a1a87b2dae step 3), and its copy's owner then keeps it compact (goal
/// 11, plan 01a1a87b2e56 step 2). Off the runtime (a test's copy) it waits for the snapshot that needs it.
fn seal(chunk: &Arc<Chunk>, home: Option<Home>) {
    if !crate::rt::on_shard() || chunk.packed().is_some() || chunk.sealing.swap(true, Ordering::Relaxed) {
        return;
    }
    let packer = SEALED.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded();
        drop(crate::rt::spawn(pack_sealed(rx)));
        tx
    });
    let _ = packer.try_send((Arc::downgrade(chunk), home));
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
async fn pack_sealed(rx: async_channel::Receiver<Sealed>) {
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
    rx: async_channel::Receiver<Sealed>,
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
            info: Arc::clone(&self.info),
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
        let mut events: Vec<(u64, u64)> = Vec::new();
        for c in self.events.chunks() {
            let metas = c.metas();
            // A digest made is taken as it is (a packed chunk's are all made: packing makes them); a
            // chunk's events are read only for the digests still missing, as a compact chunk gives them
            // by inflating its segment (goal 11, plan 01a1a87b2e56 step 2).
            let evs = if metas.iter().any(|m| !m.id.is_empty() && m.sum_made().is_none()) {
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
            for (i, m) in metas.iter().enumerate().filter(|(_, m)| !m.id.is_empty()) {
                let sum = m.sum_made().or_else(|| evs.as_ref().and_then(|evs| evs.get(i)).map(|e| m.sum(e)));
                if let (Some(seq), Some(sum)) = (m.seq, sum) {
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
        let end = match before {
            Some(b) => self.events.partition_point(|m| m.seq.is_some_and(|s| s < b)),
            None => self.events.len(),
        };
        let start = end.saturating_sub(limit);
        if end - start < limit && self.events.meta(0).is_none_or(|m| m.seq != Some(1)) {
            return None;
        }
        let mut len = r#"{"events":[]}"#.len();
        for i in start..end {
            let m = self.events.meta(i)?;
            if m.seq.is_none() || m.id.is_empty() {
                return None;
            }
            len += m.len + 1;
        }
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(br#"{"events":["#);
        self.events.texts_into(start..end, &mut out)?;
        out.extend_from_slice(b"]}");
        Some(Bytes::from(out))
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
    /// Where each event is, by its id: the id its meta keeps, shared (goal 11, plan 01a1a87b2e56 step 2).
    pub(super) index: HashMap<Arc<str>, usize>,
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

/// An info as the API answers with it: `serde_json`'s compact form (what `srv::json` wrote of it).
fn info_json(info: &Info) -> Bytes {
    serde_json::to_vec(info).map_or_else(|_| Bytes::from_static(b"null"), Bytes::from)
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
        let encoding = (Arc::clone(&info), encoded);
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
        let chunks = self.events.spillable(EVENTS_PAGE);
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
    /// first if it changed.
    pub fn publish(&mut self) {
        if !Arc::ptr_eq(&self.info_json.0, &self.info) {
            self.info_json = (Arc::clone(&self.info), info_json(&self.info));
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
        self.index = self.events.metas().enumerate().filter(|(_, m)| !m.id.is_empty()).map(|(i, m)| (Arc::clone(&m.id), i)).collect();
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
        if self.index.get(id.as_str()).copied().is_some_and(|i| self.events.change(i, |slot| *slot = ev.clone())) {
            return false;
        }
        if let (Some(q), Some(top)) = (ev.entry.seq(), self.events.len().checked_sub(1).and_then(|i| self.events.meta(i)).and_then(|m| m.seq))
            && q <= top
        {
            let at = self.events.partition_point(|m| m.seq.is_some_and(|s| s < q));
            let displaced = if self.events.meta(at).is_some_and(|m| m.seq == Some(q)) {
                self.events.change(at, |slot| *slot = ev.clone())
            } else {
                self.events.insert(at, ev);
                false
            };
            self.reindex();
            return displaced;
        }
        let at = self.events.len();
        let sealed = self.events.push(ev);
        // Indexed by the id its meta keeps (one copy of it).
        match self.events.meta(at) {
            Some(m) => self.index.insert(Arc::clone(&m.id), at),
            None => self.index.insert(Arc::from(id), at),
        };
        if let Some(sealed) = sealed {
            seal(&sealed, self.home());
        }
        if self.events.len() > MAX_EVENTS + MAX_EVENTS / 4 {
            // From where a run of the digests begins, as its machine's timeline lets go of its
            // oldest (`crate::wire::sums::run_after`).
            let over = self.events.len() - MAX_EVENTS;
            let drop = match self.events.meta(over).and_then(|m| m.seq) {
                Some(q) => {
                    let from = crate::wire::sums::run_after(q);
                    self.events.partition_point(|m| m.seq.is_some_and(|s| s < from))
                }
                None => over,
            };
            // Those let go leave the index, and the rest move down by as many: the index isn't
            // made again (a long session lets go of its oldest every thousand events or so). The
            // chunks after the cut stay as they were, packed; the one it fell in is made again, and
            // packed in its turn.
            let (gone, remade) = self.events.drain_front(drop);
            for id in &gone {
                self.index.remove(&**id);
            }
            for at in self.index.values_mut() {
                *at = at.saturating_sub(gone.len());
            }
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
        if let Some(i) = self.index.get(id).copied() {
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
    /// its owner's turn ran out first (`over`; an update's events, `Registry::apply`), to take next.
    fn take(&mut self, msg: Msg, hub: Option<&Arc<Hub>>, over: &dyn Fn() -> bool) -> Option<Msg> {
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

/// Every session's copy, each owned by one shard: where messages to them go, and what readers read.
pub struct Copies {
    inboxes: Vec<crate::memory::queue::Sender<Msg>>,
    boards: Arc<[ArcSwap<Board>]>,
    /// The hub the owners act through (what a change tells the rest of kiln), once made.
    hub: Arc<OnceLock<Weak<Hub>>>,
    /// Where the owners spill their copies' sealed chunks, once set.
    spill: Arc<OnceLock<Arc<super::spill::Dir>>>,
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
            let shard = Shard { me, inbox: tx.downgrade(), mirrors: HashMap::new(), boards: Arc::clone(&boards), bus: Arc::clone(&bus), board_changed: false, spill: Arc::clone(&spill) };
            crate::rt::spawn_owner(me, own(shard, rx, Arc::clone(&hub)));
            inboxes.push(tx);
        }
        Self { inboxes, boards, hub, spill }
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

/// A shard's owner: its messages, in the order they came, as fast as they come, in bounded turns of
/// its shard (the HTTP listener may share it). It waits on nothing but its inbox, and ends with it.
/// A message its turn ran out in the middle of (an update's events) goes on at its next turn, before
/// anything newer (goal 9, plan 01a1a87b2cec step 3).
async fn own(mut shard: Shard, rx: crate::memory::queue::Receiver<Msg>, hub: Arc<OnceLock<Weak<Hub>>>) {
    let mut turns = crate::rt::Turns::new();
    // What's left of the message the last turn ran out in.
    let mut rest: Option<Msg> = None;
    loop {
        let msg = match rest.take() {
            Some(msg) => msg,
            None => match rx.try_recv() {
                Ok(msg) => msg,
                Err(async_channel::TryRecvError::Closed) => return,
                // None waiting: waiting for one gives the shard to its other work, so its turn starts afresh
                // once one comes (else an update after a quiet spell would find its turn spent already).
                Err(async_channel::TryRecvError::Empty) => match rx.recv().await {
                    Ok(msg) => {
                        turns = crate::rt::Turns::new();
                        msg
                    }
                    Err(_) => return,
                },
            },
        };
        // The hub, once a turn: what each change tells the rest of kiln goes through it.
        let h = hub.get().and_then(Weak::upgrade);
        let over = || turns.over();
        // The turn's poll labeled by the kind of the message that took longest in it, should the poll hold the
        // shard (goal 9, plan 01a1a87b2cec step 3: the lab's s7c named this owner's polls, up to 12 ms, not what
        // held them).
        let mut longest = std::time::Duration::ZERO;
        let mut take = |shard: &mut Shard, msg: Msg| {
            let (label, began) = (label_of(&msg), std::time::Instant::now());
            let rest = shard.take(msg, h.as_ref(), &over);
            let took = began.elapsed();
            if took >= longest {
                longest = took;
                hyper_rt::futures::label(label);
            }
            rest
        };
        rest = take(&mut shard, msg);
        // What's waiting already goes in the same turn.
        while rest.is_none() && !over() {
            let Ok(msg) = rx.try_recv() else { break };
            rest = take(&mut shard, msg);
        }
        drop(h);
        // The conversation store over its bound holds this owner back before it takes more (goal 11, plan
        // 01a1a87b2e56 step 3): the updates it would take wait in its inbox, and those behind them upstream,
        // rather than their rows piling up in the store's queue. The owner's task waits; its shard's thread
        // goes on with its other tasks.
        if let Some(store) = hub.get().and_then(Weak::upgrade).and_then(|h| h.harness.convo.clone()) {
            store.room().await;
        }
        turns.after().await;
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
        let metas = |e: &Events| e.metas().filter_map(|m| m.seq).collect::<Vec<_>>();
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
        let at = events.partition_point(|m| m.seq.is_some_and(|s| s < 10));
        events.insert(at, Ev { entry: entry("between", 10), at: 0 });
        flat.insert(at, 10);
        assert_eq!(seqs(&events), flat);
        let (gone, _) = events.drain_front(CHUNK + 3);
        assert_eq!(gone.len(), CHUNK + 3);
        flat.drain(..CHUNK + 3);
        assert_eq!(seqs(&events), flat);
        assert_eq!(metas(&events), flat);
        // Each place still finds its event once the first chunk holds fewer.
        for (i, want) in flat.iter().enumerate() {
            assert_eq!(events.get(i).and_then(|e| e.entry.seq()), Some(*want), "event {i}");
            assert_eq!(events.meta(i).and_then(|m| m.seq), Some(*want), "meta {i}");
        }
        for s in n + 1..=n + 3 {
            events.push(Ev { entry: entry(&format!("e{s}"), s), at: s });
            flat.push(s);
        }
        assert_eq!(seqs(&events), flat);
        events.retain(|m| m.seq.is_some_and(|s| s % 2 == 0));
        flat.retain(|s| s % 2 == 0);
        assert_eq!(seqs(&events), flat);
        assert_eq!(events.last().and_then(|e| e.entry.seq()), flat.last().copied());
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
        assert!(events.change(0, |e| e.at = 0));
        let shared = events.chunks.iter().zip(view.chunks.iter()).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
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
        for (m, e) in chunk.metas().iter().zip(chunk.evs()?.iter()) {
            assert_eq!(joined.get(m.off..m.off + m.len), Some(e.entry.text()), "{}", m.id);
            assert_eq!((m.seq, m.sum(e), &*m.id), (e.entry.seq(), e.entry.sum(), e.entry.id().unwrap_or_default()));
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
        assert_eq!(gone.len(), CHUNK + 5);
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
        let (tx, rx) = async_channel::unbounded();
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
        let chunks = m.events.spillable(EVENTS_PAGE);
        anyhow::ensure!(chunks.len() == 4, "{} chunks past the newest page", chunks.len());
        let segments: Vec<Arc<Segment>> = chunks.iter().filter_map(|c| c.packed().cloned()).collect();
        let held = SEGMENT_BYTES.load(Ordering::Relaxed);
        let (_file, spilled) = super::super::spill::write(&dir, None, "s", &segments)?;
        anyhow::ensure!(m.events.spilled(&chunks, spilled) == 4, "every one of them spilled");
        drop((chunks, segments));
        m.publish();
        anyhow::ensure!(m.events.chunks().iter().filter(|c| c.is_spilled()).all(|c| c.packed().is_none()), "a spilled chunk keeps no segment in memory");
        anyhow::ensure!(SEGMENT_BYTES.load(Ordering::Relaxed) < held, "what went to the file left memory");
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
        let chunks = m.events.spillable(EVENTS_PAGE);
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
                let got = copies.with(id, |v| v.events.metas().filter_map(|m| m.seq).collect::<Vec<_>>()).unwrap_or_default();
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
