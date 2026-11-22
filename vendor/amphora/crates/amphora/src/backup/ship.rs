//! The shell that ships a database's log and copies its bases to an object store
//! (docs/design/backup.md).
//!
//! Shipping has no timer: each [`Shipper::ship`] takes everything durable that ends at a stamped
//! commit and sends it as one segment, so a caller that ships again as soon as the last put ends
//! ships at the store's own pace, a segment holding whatever the log grew by meanwhile: its size and
//! its cadence follow the observed log rate and the store's measured put time, as the store's
//! group commit follows its writers (docs/design/store.md, decision 1).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use amphora_api::blobs::{BlobRefusal, Blobs};

use super::object::{
    self, FORMAT, Identity, MANIFEST_FORMAT, Manifest, Segment, StoredMark, Through,
};
use super::plan::{self, BaseKey, Link, Rates, SegmentKey, ThroughKey};
use super::{Hooks, Mark, SECOND_NS, second_of};
use crate::db::{Db, PAGE_BYTES};

/// SQLite's WAL format (sqlite.org/fileformat2.html): a 32-byte header, then frames of a 24-byte
/// header and a page.
pub(super) const LOG_HEADER_BYTES: u64 = 32;
pub(super) const FRAME_BYTES: u64 = PAGE_BYTES + 24;

/// S3's most keys a listing gives at once (ListObjectsV2, `MaxKeys`: "returns up to 1,000 key
/// names"): a listing asks for no more than a store gives.
pub(super) const LIST_PAGE: usize = 1000;

/// What a restore of 100 GB may take at most (AC-T1.12.3: 30 minutes), in seconds: the objective a
/// new base is due by ([`plan::base_due`]).
pub const RESTORE_OBJECTIVE_S: f64 = 1800.0;

/// What one round shipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shipped {
    pub segments: usize,
    pub bytes: u64,
}

/// The pages a run of the log's frames wrote, a bit per page number: bounded by the database's
/// pages, as a frame names a page of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Touched(Vec<u64>);

impl Touched {
    fn mark(&mut self, page: u32) -> anyhow::Result<()> {
        let word = usize::try_from(page.checked_div(64).unwrap_or(0))?;
        let bit = 1u64
            .checked_shl(page.checked_rem(64).unwrap_or(0))
            .unwrap_or(0);
        if self.0.len() <= word {
            self.0.resize(word.saturating_add(1), 0);
        }
        if let Some(w) = self.0.get_mut(word) {
            *w |= bit;
        }
        Ok(())
    }

    /// Marks the pages of a run of whole frames: each frame's header begins with its page's
    /// number, big-endian (sqlite.org/fileformat2.html, "WAL Frame Header Format").
    fn mark_frames(&mut self, frames: &[u8]) -> anyhow::Result<()> {
        for frame in frames.chunks(usize::try_from(FRAME_BYTES)?) {
            if let Some(&[a, b, c, d]) = frame.get(..4) {
                self.mark(u32::from_be_bytes([a, b, c, d]))?;
            }
        }
        Ok(())
    }

    fn merge(&mut self, other: &Self) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        for (w, o) in self.0.iter_mut().zip(&other.0) {
            *w |= o;
        }
    }

    fn has(&self, page: u64) -> bool {
        let word = usize::try_from(page.checked_div(64).unwrap_or(0)).unwrap_or(usize::MAX);
        let bit = 1u64
            .checked_shl(u32::try_from(page.checked_rem(64).unwrap_or(0)).unwrap_or(0))
            .unwrap_or(0);
        self.0.get(word).is_some_and(|w| w & bit != 0)
    }

    /// Whether any page of the bytes `from..to` of the file was written (page `n` holds bytes
    /// `(n - 1) * PAGE_BYTES..n * PAGE_BYTES`).
    fn any_in(&self, from: u64, to: u64) -> bool {
        let first = from.checked_div(PAGE_BYTES).unwrap_or(0).saturating_add(1);
        let last = to.div_ceil(PAGE_BYTES);
        (first..=last).any(|page| self.has(page))
    }
}

/// The latest base ended: what the next base may reuse of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LastBase {
    id: u64,
    start_gen: u64,
    chunk_bytes: u64,
    file_bytes: u64,
    /// The base each of its chunks is stored under.
    bases: Vec<u64>,
}

/// What an incremental base keeps of the latest: its chunks, and the pages the log changed in the
/// generations from where it began to where this one begins.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reuse {
    file_bytes: u64,
    bases: Vec<u64>,
    changed: Touched,
}

/// A base being copied, a chunk at a time ([`Shipper::copy_base`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCopy {
    id: u64,
    start_gen: u64,
    chunk_bytes: u64,
    file_bytes: u64,
    next: u64,
    reuse: Option<Reuse>,
    /// The base each chunk so far is stored under: this one's id, or the earlier base's it reuses.
    bases: Vec<u64>,
}

impl BaseCopy {
    /// The chunks so far kept from an earlier base rather than copied.
    #[must_use]
    pub fn reused(&self) -> usize {
        self.bases.iter().filter(|b| **b != self.id).count()
    }
}

/// Ships one database's log, and copies its bases, under `prefix` of a store.
pub struct Shipper {
    prefix: String,
    hooks: Arc<Hooks>,
    /// The archive's count of the log's generations, and the one shipped now.
    generation: u64,
    identity: Option<Identity>,
    header: Vec<u8>,
    /// Frames of `identity` shipped.
    shipped: u64,
    pending: VecDeque<Mark>,
    through: u64,
    archived_through: u64,
    next_link: Option<Link>,
    unknown: Option<u64>,
    /// Since the latest base: whether the log broke, the bytes shipped, and that base's bytes.
    broken: bool,
    since_base: u64,
    base_bytes: Option<u64>,
    /// Pages the frames shipped wrote: in generations before the one shipped now (since the latest
    /// base began), and in the one shipped now. What an incremental base copies again.
    older: Touched,
    current: Touched,
    /// The base whose beginning `older` and `current` trace every frame since: none after an
    /// open (frames shipped before it weren't seen) or a gap in the log.
    traced_from: Option<u64>,
    last_base: Option<LastBase>,
    /// Marks of a generation that ended before they were shipped: their commits are in a gap.
    lost_marks: u64,
    /// The key of the archive's latest through mark ([`ThroughKey`]), let go once a later one or a
    /// segment says more.
    through_mark: Option<String>,
}

pub(super) fn list_all(
    blobs: &dyn Blobs,
    prefix: &str,
    until: Instant,
) -> anyhow::Result<Vec<(String, u64)>> {
    let mut all = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = blobs.list(prefix, after.as_deref(), LIST_PAGE, until)?;
        after = page.objects.last().map(|(k, _)| k.clone());
        all.extend(page.objects);
        if !page.truncated || after.is_none() {
            return Ok(all);
        }
        if Instant::now() >= until {
            return Err(BlobRefusal::Timeout.into());
        }
    }
}

/// Creates `key` with `bytes`; where an object is there already (an earlier create whose answer was
/// lost), it must be the same.
fn create(blobs: &dyn Blobs, key: &str, bytes: &[u8], until: Instant) -> anyhow::Result<()> {
    match blobs.create(key, bytes, until) {
        Ok(()) => Ok(()),
        Err(BlobRefusal::Exists(_)) => {
            if blobs.get(key, until)? == bytes {
                Ok(())
            } else {
                anyhow::bail!("archive object {key} exists and differs")
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn identity_of(header: &[u8]) -> anyhow::Result<Identity> {
    let word = |at: usize| -> anyhow::Result<u32> {
        let bytes: [u8; 4] = header
            .get(at..at.saturating_add(4))
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow::anyhow!("the log's header is short"))?;
        Ok(u32::from_be_bytes(bytes))
    };
    if u64::from(word(8)?) != PAGE_BYTES {
        anyhow::bail!("the log's pages aren't {PAGE_BYTES} bytes");
    }
    Ok(Identity {
        seq: word(12)?,
        salt1: word(16)?,
        salt2: word(20)?,
    })
}

pub(super) fn frame_offset(frame: u64) -> anyhow::Result<u64> {
    frame
        .checked_mul(FRAME_BYTES)
        .and_then(|b| b.checked_add(LOG_HEADER_BYTES))
        .ok_or_else(|| anyhow::anyhow!("a frame's offset overflows"))
}

impl Shipper {
    /// A shipper for `db`'s log under `prefix`, carrying on from what the archive holds: the
    /// generation it last shipped, if the log is still in it, else a new generation, the log broken.
    /// Commits the log holds now that weren't stamped are stamped now, their seconds since the
    /// archive's last unknown.
    pub fn open(
        prefix: &str,
        hooks: Arc<Hooks>,
        blobs: &dyn Blobs,
        db: &Db,
        until: Instant,
    ) -> anyhow::Result<Self> {
        let mut shipper = Self {
            prefix: prefix.into(),
            hooks,
            generation: 0,
            identity: None,
            header: Vec::new(),
            shipped: 0,
            pending: VecDeque::new(),
            through: 0,
            archived_through: 0,
            next_link: None,
            unknown: None,
            broken: true,
            since_base: 0,
            base_bytes: None,
            older: Touched::default(),
            current: Touched::default(),
            traced_from: None,
            last_base: None,
            lost_marks: 0,
            through_mark: None,
        };
        let segments = list_all(blobs, &format!("{prefix}/log/"), until)?;
        if let Some((key, _)) = segments.last() {
            let last = SegmentKey::parse(prefix, key)?;
            let body: Segment = object::decode(key, &blobs.get(key, until)?)?;
            shipper.generation = last.generation;
            shipper.archived_through = last.through;
            if let Some(head) = db.read_log(0, usize::try_from(LOG_HEADER_BYTES)?)?
                && identity_of(&head)? == body.identity
            {
                shipper.identity = Some(body.identity);
                shipper.header = head;
                shipper.shipped = last.last;
            }
            // A through mark after the last segment says how long the log stayed as it left it;
            // any other is one a later segment outdid, let go.
            let marks = super::restore::through_marks(blobs, prefix, until)?;
            let newest = marks
                .iter()
                .filter(|(m, _)| (m.generation, m.last) == (last.generation, last.last))
                .max_by_key(|(m, _)| m.through)
                .map(|(m, k)| (m.through, k.clone()));
            for (_, key) in &marks {
                if newest.as_ref().is_none_or(|(_, k)| k != key) {
                    blobs.delete(key, until)?;
                }
            }
            if let Some((through, key)) = newest {
                shipper.archived_through = shipper.archived_through.max(through);
                shipper.through_mark = Some(key);
            }
        }
        // The latest base and what was shipped after it.
        let bases = list_all(blobs, &format!("{prefix}/base/"), until)?;
        let latest = bases
            .iter()
            .filter_map(|(k, _)| BaseKey::parse(prefix, k).ok().map(|b| (b, k)))
            .max_by_key(|(b, _)| (b.end, b.id));
        if let Some((base, key)) = latest {
            let manifest = object::manifest(key, &blobs.get(key, until)?)?;
            shipper.base_bytes = Some(manifest.file_bytes);
            shipper.broken = false;
            for (key, bytes) in &segments {
                let s = SegmentKey::parse(prefix, key)?;
                if (s.generation, s.last) > base.end {
                    shipper.since_base = shipper.since_base.saturating_add(*bytes);
                }
                if s.link == Some(Link::Gap) && s.generation > base.end.0 {
                    shipper.broken = true;
                }
            }
            // The pages the archived log wrote since the latest base began, read back from its
            // segments (what the log since a base holds is bounded by `base_due`), so the next base
            // is incremental after an open too. A gap since leaves it whole.
            if !shipper.broken {
                for (key, _) in &segments {
                    let s = SegmentKey::parse(prefix, key)?;
                    if s.generation < manifest.start_gen {
                        continue;
                    }
                    let body: Segment = object::decode(key, &blobs.get(key, until)?)?;
                    if s.generation == shipper.generation {
                        shipper.current.mark_frames(&body.frames)?;
                    } else {
                        shipper.older.mark_frames(&body.frames)?;
                    }
                }
                shipper.last_base = Some(LastBase {
                    id: manifest.id,
                    start_gen: manifest.start_gen,
                    chunk_bytes: manifest.chunk_bytes,
                    file_bytes: manifest.file_bytes,
                    bases: manifest.chunk_bases()?,
                });
                shipper.traced_from = Some(manifest.id);
            }
        }
        let (seq, end) = db.log_end();
        if end > shipper.shipped {
            // Commits the archive doesn't hold, made while nothing stamped them: when, since the
            // archive's last through, isn't known. A log shipped whole leaves no such commits, and
            // the seconds after a clean close stay exact (T-T1.12.D2).
            if !segments.is_empty() {
                shipper.unknown = Some(shipper.archived_through);
            }
            shipper.hooks.marks.committed(seq, end)?;
        }
        Ok(shipper)
    }

    /// Ships what is durable and stamped: one segment, split into parts where it outgrows the
    /// store's objects. With nothing new to ship, and every commit stamped so far shipped, a
    /// through mark says the archive is whole through now, once that reaches a later second.
    pub fn ship(&mut self, db: &Db, blobs: &dyn Blobs, until: Instant) -> anyhow::Result<Shipped> {
        let drained = self.hooks.marks.drain()?;
        self.pending.extend(drained.marks);
        self.through = drained.through;
        // What was committed before now is durable once this ends (store.md, decision 2): every
        // mark drained is then of a durable commit.
        (db.flusher())()?;
        let shipped = self.ship_durable(db, blobs, until)?;
        if shipped.segments > 0 {
            // The segment says as much as the mark did, and more.
            if let Some(old) = self.through_mark.take() {
                blobs.delete(&old, until)?;
            }
        } else {
            self.mark_through(blobs, until)?;
        }
        Ok(shipped)
    }

    /// Writes a through mark at the log's shipped end when every commit drained is shipped and
    /// the archive's through moves into a later second: restore targets are whole seconds
    /// (AC-T1.12.1), so a mark within the second the archive is whole through already says
    /// nothing a restore asks, and the marks written are at most one a second, only while rounds
    /// run (a commit or a base's chunk wakes one).
    fn mark_through(&mut self, blobs: &dyn Blobs, until: Instant) -> anyhow::Result<()> {
        if !self.pending.is_empty()
            || self.identity.is_none()
            || self.shipped == 0
            || self.through / SECOND_NS <= self.archived_through / SECOND_NS
        {
            return Ok(());
        }
        let mark = ThroughKey {
            generation: self.generation,
            last: self.shipped,
            through: self.through,
        };
        let key = mark.key(&self.prefix);
        let body = Through {
            format: FORMAT,
            generation: mark.generation,
            last: mark.last,
            through: mark.through,
        };
        create(blobs, &key, &object::encode(&body)?, until)?;
        self.archived_through = self.through;
        if let Some(old) = self.through_mark.replace(key) {
            blobs.delete(&old, until)?;
        }
        Ok(())
    }

    /// Sends what is durable and stamped of the log's generation now.
    fn ship_durable(
        &mut self,
        db: &Db,
        blobs: &dyn Blobs,
        until: Instant,
    ) -> anyhow::Result<Shipped> {
        let (seq, end) = db.log_end();
        let durable = db.durable_frame().unwrap_or(0).min(end);
        let Some(head) = db.read_log(0, usize::try_from(LOG_HEADER_BYTES)?)? else {
            return Ok(Shipped::default());
        };
        let identity = identity_of(&head)?;
        if identity.seq != seq {
            // The log was started again and its header isn't written yet.
            return Ok(Shipped::default());
        }
        if self.identity != Some(identity) {
            self.begin_generation(identity, head)?;
        }
        // Marks of a generation shipped no further are of commits in a gap.
        while let Some(m) = self.pending.front() {
            if m.seq == identity.seq && m.frame > self.shipped {
                break;
            }
            if m.seq != identity.seq {
                self.lost_marks = self.lost_marks.saturating_add(1);
            }
            self.pending.pop_front();
        }
        let ready = self
            .pending
            .iter()
            .take_while(|m| m.seq == identity.seq && m.frame <= durable)
            .count();
        if ready == 0 {
            return Ok(Shipped::default());
        }
        let mut marks: Vec<Mark> = self.pending.drain(..ready).collect();
        if let (Some(first), Some(unknown)) = (marks.first_mut(), self.unknown) {
            first.unknown_from = Some(first.unknown_from.map_or(unknown, |u| u.min(unknown)));
        }
        let last = marks.last().map_or(self.shipped, |m| m.frame);
        // Every commit stamped at or before `through` is in this segment or before it: those after
        // it are in the next mark's second, or weren't drained yet.
        let through = match self.pending.front() {
            Some(next) => {
                let second_before = second_of(next.ns)
                    .saturating_sub(1)
                    .saturating_mul(SECOND_NS);
                next.unknown_from
                    .map_or(second_before, |u| u.min(second_before))
            }
            None => self.through,
        };
        let shipped = self.send(db, blobs, &marks, last, through, until)?;
        self.shipped = last;
        self.hooks.fence.shipped(identity.seq, last)?;
        self.archived_through = through;
        self.unknown = None;
        self.next_link = None;
        self.since_base = self.since_base.saturating_add(shipped.bytes);
        Ok(shipped)
    }

    fn begin_generation(&mut self, identity: Identity, head: Vec<u8>) -> anyhow::Result<()> {
        let link = match self.identity {
            Some(old)
                if self
                    .hooks
                    .fence
                    .ended_whole(old.seq, self.shipped, identity.seq)? =>
            {
                Link::Whole(self.shipped)
            }
            _ => Link::Gap,
        };
        // The pages the ending generation wrote join those of the generations before it.
        self.older.merge(&self.current);
        self.current = Touched::default();
        if link == Link::Gap {
            self.broken = true;
            // Frames that never reached the archive changed pages no one saw.
            self.traced_from = None;
            if self.generation > 0 {
                self.unknown = Some(
                    self.unknown
                        .map_or(self.archived_through, |u| u.min(self.archived_through)),
                );
            }
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("the archive's generations overflow"))?;
        self.identity = Some(identity);
        self.header = head;
        self.shipped = 0;
        self.next_link = Some(link);
        Ok(())
    }

    /// Sends frames `shipped + 1..=last` with `marks`: in as few objects as the store takes.
    fn send(
        &mut self,
        db: &Db,
        blobs: &dyn Blobs,
        marks: &[Mark],
        last: u64,
        through: u64,
        until: Instant,
    ) -> anyhow::Result<Shipped> {
        let identity = self
            .identity
            .ok_or_else(|| anyhow::anyhow!("no log generation to ship"))?;
        let marks_stored: Vec<StoredMark> = marks
            .iter()
            .map(|m| StoredMark {
                frame: m.frame,
                ns: m.ns,
                unknown_from: m.unknown_from,
            })
            .collect();
        let mut body = Segment {
            format: FORMAT,
            generation: self.generation,
            identity,
            first: self.shipped.saturating_add(1),
            header: self.header.clone(),
            marks: marks_stored,
            frames: Vec::new(),
        };
        let room = blobs
            .object_max()
            .checked_sub(object::segment_overhead(&body)?)
            .ok_or_else(|| anyhow::anyhow!("the store's objects can't hold a segment's marks"))?;
        let per_object = u64::try_from(room)?.checked_div(FRAME_BYTES).unwrap_or(0);
        if per_object == 0 {
            anyhow::bail!("the store's objects can't hold one frame of the log");
        }
        let mut stored = std::mem::take(&mut body.marks);
        let mut shipped = Shipped::default();
        let mut from = self.shipped;
        while from < last {
            let to = from.saturating_add(per_object).min(last);
            let is_last = to == last;
            body.first = from.saturating_add(1);
            body.frames = db
                .read_log(
                    frame_offset(from)?,
                    usize::try_from(to.saturating_sub(from).saturating_mul(FRAME_BYTES))?,
                )?
                .ok_or_else(|| anyhow::anyhow!("the log's file is gone"))?;
            self.current.mark_frames(&body.frames)?;
            let key = SegmentKey {
                generation: self.generation,
                first: body.first,
                last: to,
                marks: if is_last {
                    marks.first().zip(marks.last()).map(|(a, b)| (a.ns, b.ns))
                } else {
                    None
                },
                through,
                unknown: if is_last {
                    marks.first().and_then(|m| m.unknown_from)
                } else {
                    None
                },
                link: if body.first == 1 {
                    self.next_link
                } else {
                    None
                },
            };
            // The marks go in the last part only: the others end at no mark.
            if is_last {
                body.marks = std::mem::take(&mut stored);
            }
            let bytes = object::encode(&body)?;
            create(blobs, &key.key(&self.prefix), &bytes, until)?;
            shipped.segments = shipped.segments.saturating_add(1);
            shipped.bytes = shipped.bytes.saturating_add(u64::try_from(bytes.len())?);
            from = to;
        }
        Ok(shipped)
    }

    /// Whether a new base is due ([`plan::base_due`]), at the restore `rates` measured, if any.
    #[must_use]
    pub fn base_due(&self, rates: Option<Rates>) -> bool {
        plan::base_due(
            self.base_bytes,
            self.broken,
            self.since_base,
            self.incremental_copy(),
            rates,
            RESTORE_OBJECTIVE_S,
        )
    }

    /// Whether the log's generation now is the one this shipper ships: a base may begin in it
    /// ([`Shipper::begin_base`]).
    #[must_use]
    pub fn ships_current(&self, db: &Db) -> bool {
        let (seq, _) = db.log_end();
        self.identity.is_some_and(|i| i.seq == seq)
    }

    /// The latest base's id, if the archive holds one.
    #[must_use]
    pub fn latest_base(&self) -> Option<u64> {
        self.last_base.as_ref().map(|b| b.id)
    }

    /// The archive's count of the log's generations: it moves on as the log starts again.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether no base restores the log shipped now: none yet, or the log broke since the latest.
    #[must_use]
    pub fn base_needed(&self) -> bool {
        self.base_bytes.is_none() || self.broken
    }

    /// The frames of the log's generation now shipped so far.
    #[must_use]
    pub fn frames_in_generation(&self) -> u64 {
        self.shipped
    }

    /// Marks dropped because their generation ended unshipped.
    #[must_use]
    pub fn lost_marks(&self) -> u64 {
        self.lost_marks
    }

    /// What an incremental base begun now would copy: the latest base's chunks whose pages the
    /// log changed since it began; none when a new base would copy the whole file.
    fn incremental_copy(&self) -> Option<u64> {
        let last = self.last_base.as_ref()?;
        if self.traced_from != Some(last.id) || last.start_gen >= self.generation {
            return None;
        }
        let mut bytes = 0u64;
        let mut from = 0u64;
        while from < last.file_bytes {
            let to = from.saturating_add(last.chunk_bytes).min(last.file_bytes);
            if self.older.any_in(from, to) {
                bytes = bytes.saturating_add(to.saturating_sub(from));
            }
            from = to;
        }
        Some(bytes)
    }

    /// Begins a copy of the database's file, in chunks as large as the store's objects and
    /// `memory` allow: a chunk is a request, and a restore's fetch moves more bytes a second the
    /// fewer requests it waits on. Refused until the log's generation now has been seen by
    /// [`Shipper::ship`].
    pub fn begin_base(
        &mut self,
        db: &Db,
        blobs: &dyn Blobs,
        memory: usize,
    ) -> anyhow::Result<BaseCopy> {
        let (seq, _) = db.log_end();
        if self.identity.is_none_or(|i| i.seq != seq) {
            anyhow::bail!("ship the log's generation before copying a base in it");
        }
        let room = blobs.object_max().saturating_sub(32).min(memory);
        let chunk_bytes = u64::try_from(room)?
            .checked_div(PAGE_BYTES)
            .unwrap_or(0)
            .saturating_mul(PAGE_BYTES);
        if chunk_bytes == 0 {
            anyhow::bail!("a base's chunk can't hold a page");
        }
        let id = {
            let mut state = self.hooks.marks.state()?;
            self.hooks.marks.stamp(&mut state)
        };
        // An incremental base (docs/design/backup.md, "Incremental bases"): a restore replays the
        // log from the first frame of the generation a base begins in, so a chunk of the latest
        // base whose pages no frame of the generations from its beginning to this one's wrote is
        // the same in both, and is kept. The frames of this generation are replayed, so they don't
        // count. Only with every frame since the latest base began seen, and a generation since.
        let changed = std::mem::take(&mut self.older);
        let reuse = match &self.last_base {
            Some(last)
                if self.traced_from == Some(last.id)
                    && last.start_gen < self.generation
                    && last.chunk_bytes == chunk_bytes =>
            {
                Some(Reuse {
                    file_bytes: last.file_bytes,
                    bases: last.bases.clone(),
                    changed,
                })
            }
            _ => None,
        };
        self.traced_from = Some(id);
        Ok(BaseCopy {
            id,
            start_gen: self.generation,
            chunk_bytes,
            file_bytes: db.file_bytes()?,
            next: 0,
            reuse,
            bases: Vec::new(),
        })
    }

    /// Copies the next chunk: whether more follow.
    pub fn copy_base(
        &mut self,
        db: &Db,
        blobs: &dyn Blobs,
        copy: &mut BaseCopy,
        until: Instant,
    ) -> anyhow::Result<bool> {
        let from = copy.next.saturating_mul(copy.chunk_bytes);
        if from >= copy.file_bytes {
            return Ok(false);
        }
        let len = copy.chunk_bytes.min(copy.file_bytes.saturating_sub(from));
        let to = from.saturating_add(len);
        // Kept when the latest base holds these same bytes of the file and the log changed none of
        // their pages since it began.
        let kept = copy.reuse.as_ref().and_then(|r| {
            let same = r.file_bytes.saturating_sub(from).min(copy.chunk_bytes) == len;
            let base = r.bases.get(usize::try_from(copy.next).ok()?).copied()?;
            if same && !r.changed.any_in(from, to) {
                Some(base)
            } else {
                None
            }
        });
        match kept {
            Some(base) => copy.bases.push(base),
            None => {
                let bytes = object::sealed(&db.read_file(from, usize::try_from(len)?)?)?;
                create(
                    blobs,
                    &plan::chunk_key(&self.prefix, copy.id, copy.next),
                    &bytes,
                    until,
                )?;
                copy.bases.push(copy.id);
            }
        }
        copy.next = copy.next.saturating_add(1);
        Ok(copy.next.saturating_mul(copy.chunk_bytes) < copy.file_bytes)
    }

    /// Ends a base whose chunks are all copied: its manifest names where the log stood once the
    /// copy ended, which a restore replays through at least.
    pub fn end_base(
        &mut self,
        db: &Db,
        blobs: &dyn Blobs,
        copy: BaseCopy,
        until: Instant,
    ) -> anyhow::Result<()> {
        if copy.next.saturating_mul(copy.chunk_bytes) < copy.file_bytes {
            anyhow::bail!("the base's chunks aren't all copied");
        }
        self.ship(db, blobs, until)?;
        let (seq, end) = db.log_end();
        let end = match self.identity {
            Some(i) if i.seq == seq => end,
            // The log started again, shipped whole, and nothing is in it yet (its header isn't
            // even written): the file holds all the shipped generation did, so the base ends
            // where that generation did.
            Some(i) if end == 0 && self.hooks.fence.ended_whole(i.seq, self.shipped, seq)? => {
                self.shipped
            }
            _ => anyhow::bail!("ship the log's generation before ending a base in it"),
        };
        let manifest = Manifest {
            format: MANIFEST_FORMAT,
            id: copy.id,
            start_gen: copy.start_gen,
            end: (self.generation, end),
            file_bytes: copy.file_bytes,
            chunk_bytes: copy.chunk_bytes,
            chunks: copy.next,
            from: object::runs(&copy.bases),
        };
        let key = BaseKey {
            end: manifest.end,
            start_gen: manifest.start_gen,
            id: manifest.id,
        };
        create(
            blobs,
            &key.key(&self.prefix),
            &object::encode(&manifest)?,
            until,
        )?;
        self.base_bytes = Some(copy.file_bytes);
        self.since_base = 0;
        self.broken = false;
        self.last_base = Some(LastBase {
            id: copy.id,
            start_gen: copy.start_gen,
            chunk_bytes: copy.chunk_bytes,
            file_bytes: copy.file_bytes,
            bases: copy.bases,
        });
        Ok(())
    }
}
