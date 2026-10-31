//! Amphora: the packed log (Amphora's `docs/design/store.md`, decisions 16 and 17).
//!
//! A generation of the log whose header carries [`PACKED_FORMAT`] holds records instead of one
//! page a frame. A prepared batch of pages (a commit's, or a spill's) is cut into records of up to
//! [`RECORD_BYTES`], each its pages' entries compressed together (LZ4) and laid in as many of the
//! log's fixed-size frame slots as it takes. An entry is a page's whole image, or the bytes that
//! changed since the page's last version in the log (a delta, at most [`MOST_DELTAS`] in a row
//! after an image). Every page of a record maps to the record's first slot, so frame ids keep their
//! meaning everywhere else (read marks, backfill, restarts). The first slot's frame header carries
//! the record's slot count where a page number would be, the others [`CONTINUATION`]; the record's
//! last slot carries the commit's database size when it ends one. The slots' checksums chain as
//! frames' do.

use std::collections::VecDeque;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::error::LimboError;
use crate::storage::sqlite3_ondisk::WAL_FRAME_HEADER_SIZE;
use crate::Result;

/// The log header's file format for a generation of one page a frame (SQLite's).
pub(crate) const PAGE_FORMAT: u32 = 3_007_000;

/// The log header's file format for a packed generation.
pub(crate) const PACKED_FORMAT: u32 = 0x414d_5002;

/// The page-number field of a record's slots after its first.
pub(crate) const CONTINUATION: u32 = u32::MAX;

/// A record's payload starts with its stored and raw lengths.
const PAYLOAD_HEADER: usize = 8;

/// Pages a record holds at most: a prepared batch is at most 1024 (`IOV_MAX` in `wal.rs`).
const MOST_PAGES: usize = 1024;

/// Raw bytes a record is cut at: LZ4 finds a match at most 64 KiB back (its block format,
/// `lz4_Block_format.md`), so a larger record compresses little better, while a reader that misses
/// its page cache reads and decodes a whole record. That is 64 whole pages of 4 KiB, about 18
/// slots at the 3x measured for whole pages (decision 16), the last slot's padding about 3% of it.
pub(crate) const RECORD_BYTES: usize = 256 << 10;

/// Deltas in a row before a page's next entry is its whole image: a read of a page applies at most
/// this many, from as many records. At 8, a page whose changes average 300 bytes costs about a
/// fifth of its images (decision 17).
pub(crate) const MOST_DELTAS: u8 = 8;

/// An entry's header: its page, its kind, its depth (deltas since its page's last image), and its
/// body's length.
const ENTRY_HEADER: usize = 12;

/// A run's header in a delta: its offset in the page and its length less one, u16s big-endian.
const RUN_HEADER: usize = 4;

const IMAGE: u8 = 0;
const DELTA: u8 = 1;

fn corrupt(what: &str) -> LimboError {
    LimboError::Corrupt(format!("packed log record: {what}"))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| corrupt("offset overflows"))?;
    let word: [u8; 4] = bytes
        .get(at..end)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| corrupt("cut short"))?;
    Ok(u32::from_be_bytes(word))
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let end = at
        .checked_add(2)
        .ok_or_else(|| corrupt("offset overflows"))?;
    let word: [u8; 2] = bytes
        .get(at..end)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| corrupt("cut short"))?;
    Ok(u16::from_be_bytes(word))
}

fn as_usize(n: u32) -> Result<usize> {
    usize::try_from(n).map_err(|_| corrupt("length too large"))
}

/// A page's entry in a record.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Body<'a> {
    /// The page's whole image.
    Image(&'a [u8]),
    /// What changed since the page's last version in the log: runs of bytes, each its offset, its
    /// length less one (u16s big-endian) and its bytes. `depth` counts the deltas since the page's
    /// last image, this one included (1 to [`MOST_DELTAS`]).
    Delta { depth: u8, runs: &'a [u8] },
}

impl Body<'_> {
    /// Deltas since the page's last image: 0 for an image.
    pub(crate) fn depth(&self) -> u8 {
        match self {
            Body::Image(_) => 0,
            Body::Delta { depth, .. } => *depth,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Body::Image(image) => image,
            Body::Delta { runs, .. } => runs,
        }
    }

    /// The raw bytes it takes in a record.
    pub(crate) fn raw_len(&self) -> usize {
        ENTRY_HEADER.saturating_add(self.bytes().len())
    }
}

/// The raw bytes a record holds at most: [`MOST_PAGES`] entries, each at most a page.
fn raw_len_bound(page_size: usize) -> Result<usize> {
    page_size
        .checked_add(ENTRY_HEADER)
        .and_then(|per| per.checked_mul(MOST_PAGES))
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| corrupt("record too large"))
}

/// A record's payload for `entries` (each page once, an image a page long): [stored length][raw
/// length][the raw bytes compressed], or, where LZ4 doesn't make them smaller, stored length 0 and
/// the raw bytes as they are (an LZ4 block of anything is never empty). The raw bytes are [entry
/// count] then each entry's header ([page][kind][depth][0u16][body length]) then the bodies, in
/// order, integers big-endian. With it, the record as a read decodes it.
pub(crate) fn encode(entries: &[(u32, Body<'_>)], page_size: usize) -> Result<(Vec<u8>, Record)> {
    if entries.is_empty() || entries.len() > MOST_PAGES {
        return Err(corrupt("a record holds 1 to 1024 pages"));
    }
    let count = u32::try_from(entries.len()).map_err(|_| corrupt("too many pages"))?;
    let len = entries
        .iter()
        .try_fold(4usize, |sum, (_, body)| sum.checked_add(body.raw_len()))
        .ok_or_else(|| corrupt("record too large"))?;
    let mut raw = Vec::with_capacity(len);
    raw.extend_from_slice(&count.to_be_bytes());
    for (page, body) in entries {
        let (kind, ok) = match body {
            Body::Image(image) => (IMAGE, image.len() == page_size),
            Body::Delta { depth, runs } => (
                DELTA,
                (1..=MOST_DELTAS).contains(depth) && runs.len() < page_size,
            ),
        };
        if !ok {
            return Err(corrupt(
                "an entry isn't an image a page long or a delta shorter than one",
            ));
        }
        let body_len = u32::try_from(body.bytes().len()).map_err(|_| corrupt("entry too large"))?;
        raw.extend_from_slice(&page.to_be_bytes());
        raw.extend_from_slice(&[kind, body.depth(), 0, 0]);
        raw.extend_from_slice(&body_len.to_be_bytes());
    }
    for (_, body) in entries {
        raw.extend_from_slice(body.bytes());
    }
    let compressed = lz4_flex::block::compress(&raw);
    let (stored_len, stored) = if compressed.len() < raw.len() {
        let n = u32::try_from(compressed.len()).map_err(|_| corrupt("record too large"))?;
        (n, compressed.as_slice())
    } else {
        (0, raw.as_slice())
    };
    let raw_len = u32::try_from(raw.len()).map_err(|_| corrupt("record too large"))?;
    let mut payload = Vec::with_capacity(PAYLOAD_HEADER.saturating_add(stored.len()));
    payload.extend_from_slice(&stored_len.to_be_bytes());
    payload.extend_from_slice(&raw_len.to_be_bytes());
    payload.extend_from_slice(stored);
    drop(compressed);
    Ok((payload, Record::parse(raw, page_size)?))
}

/// Byte ranges of `new` that differ from `base` closer than this are one run: a run's header costs
/// as much.
const RUN_GAP: usize = RUN_HEADER;

/// The runs that turn `base` into `new` (both a page long, at most 65,536 bytes), the last `tail`
/// bytes always among them (a page checksum the page's buffer doesn't hold up to date), when they
/// take fewer than `most` bytes; else none (the page is then written whole).
pub(crate) fn diff(base: &[u8], new: &[u8], tail: usize, most: usize) -> Option<Vec<u8>> {
    if base.len() != new.len() || new.len() > 65_536 {
        return None;
    }
    // The tail counts as changed byte by byte: where the buffer's stale copy happens to equal the
    // new one, the log's copy can still differ.
    let carried = new.len().saturating_sub(tail);
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (word, (a, b)) in base.chunks(8).zip(new.chunks(8)).enumerate() {
        let at = word.checked_mul(8)?;
        let end = at.checked_add(a.len())?;
        let forced = (end > carried).then(|| carried.max(at).saturating_sub(at));
        if a == b && forced.is_none() {
            continue;
        }
        let differs = a.iter().zip(b).position(|(x, y)| x != y);
        let first = match (differs, forced) {
            (Some(d), Some(f)) => d.min(f),
            (Some(d), None) => d,
            (None, Some(f)) => f,
            (None, None) => continue,
        };
        // A forced suffix runs to the chunk's end.
        let last = match forced {
            Some(_) => a.len().checked_sub(1)?,
            None => a.iter().zip(b).rposition(|(x, y)| x != y)?,
        };
        let (start, stop) = (
            at.checked_add(first)?,
            at.checked_add(last)?.checked_add(1)?,
        );
        match ranges.last_mut() {
            Some(prev) if start <= prev.1.saturating_add(RUN_GAP) => {
                prev.0 = prev.0.min(start);
                prev.1 = prev.1.max(stop);
            }
            _ => ranges.push((start, stop)),
        }
    }
    let mut runs = Vec::new();
    for (start, end) in ranges {
        let len = end.checked_sub(start)?;
        let offset = u16::try_from(start).ok()?;
        let less_one = u16::try_from(len.checked_sub(1)?).ok()?;
        runs.extend_from_slice(&offset.to_be_bytes());
        runs.extend_from_slice(&less_one.to_be_bytes());
        runs.extend_from_slice(new.get(start..end)?);
        if runs.len() >= most {
            return None;
        }
    }
    Some(runs)
}

/// Applies a delta's runs to `image`, which they turn into the page's next version.
pub(crate) fn apply(runs: &[u8], image: &mut [u8]) -> Result<()> {
    let mut at = 0usize;
    while at < runs.len() {
        let offset = usize::from(u16_at(runs, at)?);
        let len = usize::from(u16_at(runs, at.saturating_add(2))?)
            .checked_add(1)
            .ok_or_else(|| corrupt("run length overflows"))?;
        let start = at
            .checked_add(RUN_HEADER)
            .ok_or_else(|| corrupt("run offset overflows"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| corrupt("run offset overflows"))?;
        let bytes = runs
            .get(start..end)
            .ok_or_else(|| corrupt("a run cut short"))?;
        let to = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("run offset overflows"))?;
        image
            .get_mut(offset..to)
            .ok_or_else(|| corrupt("a run past the page's end"))?
            .copy_from_slice(bytes);
        at = end;
    }
    Ok(())
}

/// A page's version from its chain: `chain` its entries newest first, back to an image.
pub(crate) fn rebuild(chain: &[Body<'_>], page_size: usize) -> Result<Vec<u8>> {
    let base = chain
        .iter()
        .position(|body| matches!(body, Body::Image(_)))
        .ok_or_else(|| corrupt("a page's deltas don't go back to an image"))?;
    let (deltas, from) = chain.split_at(base);
    let Some(Body::Image(image)) = from.first() else {
        return Err(corrupt("a page's deltas don't go back to an image"));
    };
    if image.len() != page_size {
        return Err(corrupt("an image isn't a page long"));
    }
    let mut page = image.to_vec();
    for body in deltas.iter().rev() {
        match body {
            Body::Delta { runs, .. } => apply(runs, &mut page)?,
            Body::Image(_) => return Err(corrupt("an image among a page's deltas")),
        }
    }
    Ok(page)
}

/// The most slots a record of `page_size` pages takes, whatever wrote it: a payload's header and
/// the raw bytes of [`MOST_PAGES`] entries, stored or compressed (LZ4's bound for bytes that don't
/// compress). A first slot that claims more is no record's.
pub(crate) fn most_slots(page_size: usize) -> Result<u32> {
    let raw = raw_len_bound(page_size)?;
    let stored = lz4_flex::block::get_maximum_output_size(raw).max(raw);
    let payload = PAYLOAD_HEADER
        .checked_add(stored)
        .ok_or_else(|| corrupt("record too large"))?;
    slots(payload, page_size)
}

/// Fills the page body of a record's `slot`-th slot (0-based) with its share of `payload`, the
/// rest zeros.
pub(crate) fn fill_slot(payload: &[u8], slot: usize, body: &mut [u8]) -> Result<()> {
    let start = slot
        .checked_mul(body.len())
        .ok_or_else(|| corrupt("slot offset overflows"))?;
    let share = payload.get(start..).unwrap_or_default();
    let n = share.len().min(body.len());
    let (filled, rest) = body
        .split_at_mut_checked(n)
        .ok_or_else(|| corrupt("slot cut short"))?;
    filled.copy_from_slice(share.get(..n).ok_or_else(|| corrupt("payload cut short"))?);
    rest.fill(0);
    Ok(())
}

/// A record's payload from its slots as read from the log, each a frame header and a page body.
pub(crate) fn gather(slots: &[u8], page_size: usize) -> Result<Vec<u8>> {
    let frame = WAL_FRAME_HEADER_SIZE
        .checked_add(page_size)
        .ok_or_else(|| corrupt("frame size overflows"))?;
    if page_size == 0 || slots.len().checked_rem(frame) != Some(0) {
        return Err(corrupt("slots aren't whole frames"));
    }
    let mut payload = Vec::with_capacity(slots.len());
    for slot in slots.chunks_exact(frame) {
        payload.extend_from_slice(
            slot.get(WAL_FRAME_HEADER_SIZE..)
                .ok_or_else(|| corrupt("slot cut short"))?,
        );
    }
    Ok(payload)
}

/// The slots a payload of `payload_len` bytes takes, a slot holding `page_size` bytes of it.
pub(crate) fn slots(payload_len: usize, page_size: usize) -> Result<u32> {
    if page_size == 0 {
        return Err(corrupt("page size 0"));
    }
    let slots = payload_len.div_ceil(page_size).max(1);
    u32::try_from(slots).map_err(|_| corrupt("record takes too many slots"))
}

/// The slot count a record's first slot carries in its frame header's page-number field: one that
/// is neither 0 (no frame) nor [`CONTINUATION`].
pub(crate) fn is_first_slot(page_field: u32) -> bool {
    page_field != 0 && page_field != CONTINUATION
}

/// An entry of a decoded record: its page, kind, depth, and where its body is in the raw bytes.
#[derive(Debug, Clone, Copy)]
struct Entry {
    page: u32,
    kind: u8,
    depth: u8,
    start: usize,
    end: usize,
}

/// A record, decoded: its pages' entries.
#[derive(Debug)]
pub(crate) struct Record {
    entries: Vec<Entry>,
    /// The entries' indexes ordered by page: a page's entry is found by binary search.
    by_page: Vec<(u32, usize)>,
    raw: Vec<u8>,
}

impl Record {
    /// Decodes a record from its slots' bytes (`payload`, padding after it ignored), checking that
    /// every entry is an image a page long or a delta whose runs stay within the page.
    pub(crate) fn decode(payload: &[u8], page_size: usize) -> Result<Self> {
        let stored_len = as_usize(u32_at(payload, 0)?)?;
        let raw_len = as_usize(u32_at(payload, 4)?)?;
        if raw_len > raw_len_bound(page_size)? {
            return Err(corrupt("raw length beyond any record's"));
        }
        let end = PAYLOAD_HEADER
            .checked_add(if stored_len == 0 { raw_len } else { stored_len })
            .ok_or_else(|| corrupt("stored length overflows"))?;
        let stored = payload
            .get(PAYLOAD_HEADER..end)
            .ok_or_else(|| corrupt("stored bytes cut short"))?;
        let raw = if stored_len == 0 {
            stored.to_vec()
        } else {
            let mut raw = vec![0u8; raw_len];
            let n = lz4_flex::block::decompress_into(stored, &mut raw)
                .map_err(|e| corrupt(&format!("doesn't decompress: {e}")))?;
            if n != raw_len {
                return Err(corrupt("decompressed length isn't the raw length"));
            }
            raw
        };
        Self::parse(raw, page_size)
    }

    /// A record from its raw bytes, each entry checked: an image a page long, or a delta whose
    /// runs stay within the page.
    fn parse(raw: Vec<u8>, page_size: usize) -> Result<Self> {
        let count = as_usize(u32_at(&raw, 0)?)?;
        if count == 0 || count > MOST_PAGES {
            return Err(corrupt("page count out of range"));
        }
        let mut body_at = count
            .checked_mul(ENTRY_HEADER)
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| corrupt("entry headers overflow"))?;
        let mut entries = Vec::with_capacity(count);
        let mut scratch = vec![0u8; page_size];
        for i in 0..count {
            let at = i
                .checked_mul(ENTRY_HEADER)
                .and_then(|n| n.checked_add(4))
                .ok_or_else(|| corrupt("entry header offset overflows"))?;
            let page = u32_at(&raw, at)?;
            let flags = raw
                .get(at.saturating_add(4)..at.saturating_add(8))
                .ok_or_else(|| corrupt("entry header cut short"))?;
            let (kind, depth) = match flags {
                [kind, depth, 0, 0] => (*kind, *depth),
                _ => return Err(corrupt("entry header malformed")),
            };
            let len = as_usize(u32_at(&raw, at.saturating_add(8))?)?;
            let start = body_at;
            let end = start
                .checked_add(len)
                .ok_or_else(|| corrupt("entry length overflows"))?;
            let body = raw
                .get(start..end)
                .ok_or_else(|| corrupt("entry cut short"))?;
            match (kind, depth) {
                (IMAGE, 0) if len == page_size => {}
                (DELTA, 1..=MOST_DELTAS) if len < page_size => apply(body, &mut scratch)?,
                _ => return Err(corrupt("entry neither an image nor a delta")),
            }
            entries.push(Entry {
                page,
                kind,
                depth,
                start,
                end,
            });
            body_at = end;
        }
        if body_at != raw.len() {
            return Err(corrupt("bytes after the last entry"));
        }
        let mut by_page: Vec<(u32, usize)> = entries
            .iter()
            .enumerate()
            .map(|(index, e)| (e.page, index))
            .collect();
        by_page.sort_unstable();
        if by_page
            .windows(2)
            .any(|w| matches!(w, [a, b] if a.0 == b.0))
        {
            return Err(corrupt("a page twice in a record"));
        }
        Ok(Self {
            entries,
            by_page,
            raw,
        })
    }

    /// The pages the record holds, in its order.
    pub(crate) fn pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.iter().map(|e| e.page)
    }

    /// The entry of `page`, if the record holds it.
    pub(crate) fn body(&self, page: u32) -> Option<Body<'_>> {
        let at = self.by_page.binary_search_by_key(&page, |(p, _)| *p).ok()?;
        let (_, index) = self.by_page.get(at)?;
        let e = self.entries.get(*index)?;
        let bytes = self.raw.get(e.start..e.end)?;
        Some(if e.kind == IMAGE {
            Body::Image(bytes)
        } else {
            Body::Delta {
                depth: e.depth,
                runs: bytes,
            }
        })
    }

    /// The bytes it holds once decoded, for a cache's budget.
    pub(crate) fn size(&self) -> usize {
        let per_entry =
            std::mem::size_of::<Entry>().saturating_add(std::mem::size_of::<(u32, usize)>());
        self.raw
            .len()
            .saturating_add(self.entries.len().saturating_mul(per_entry))
    }
}

/// Bytes the decoded records may hold, the oldest going first past it. The writer keeps each record
/// it publishes, and reads (a reader's miss, the checkpoint's copies, a page's chain back to its
/// image) mostly want recent ones: a chain of a page changed at every commit reaches back
/// [`MOST_DELTAS`] commits, a few hundred KiB at 10,000 writers. Chosen, a group's share of the
/// memory, not yet measured; a record not kept is read and decoded again.
pub(crate) const DECODED_BUDGET: usize = 4 << 20;

/// What the shared log keeps of its packed records: each record's slot count by its first slot
/// (written before any page maps to the record), and records decoded, keyed by the log's
/// generation (its header's checkpoint sequence) and first slot, within [`DECODED_BUDGET`].
#[derive(Debug, Default)]
pub(crate) struct Records {
    slots: FxHashMap<u64, u32>,
    decoded: FxHashMap<(u32, u64), Arc<Record>>,
    order: VecDeque<(u32, u64)>,
    bytes: usize,
}

impl Records {
    /// A record of `slots` slots now begins at `head`: whatever was decoded there before is dropped
    /// (a rollback or a restart can lay a new record in the same slots).
    pub(crate) fn publish(&mut self, head: u64, slots: u32) {
        self.slots.insert(head, slots);
        self.forget(|(_, h)| h == head);
    }

    /// The slots of the record at `head`.
    pub(crate) fn slots_of(&self, head: u64) -> Option<u32> {
        self.slots.get(&head).copied()
    }

    /// The first slot of the record whose slots hold `frame`, if one does.
    pub(crate) fn covering(&self, frame: u64) -> Option<u64> {
        self.slots.iter().find_map(|(head, slots)| {
            let last = head.checked_add(u64::from(*slots))?.checked_sub(1)?;
            if *head <= frame && frame <= last {
                Some(*head)
            } else {
                None
            }
        })
    }

    /// The record at `head` of `generation`, if it was decoded and kept.
    pub(crate) fn decoded(&self, generation: u32, head: u64) -> Option<Arc<Record>> {
        self.decoded.get(&(generation, head)).cloned()
    }

    /// Keeps a decoded record, the oldest kept going first past the budget.
    pub(crate) fn keep(&mut self, generation: u32, head: u64, record: Arc<Record>) {
        let size = record.size();
        if size > DECODED_BUDGET || self.decoded.contains_key(&(generation, head)) {
            return;
        }
        while self.bytes.saturating_add(size) > DECODED_BUDGET {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.decoded.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(gone.size());
            }
        }
        self.decoded.insert((generation, head), record);
        self.order.push_back((generation, head));
        self.bytes = self.bytes.saturating_add(size);
    }

    /// Forgets the records past `max_frame` (a rollback's).
    pub(crate) fn drop_after(&mut self, max_frame: u64) {
        self.slots.retain(|head, _| *head <= max_frame);
        self.forget(|(_, h)| h > max_frame);
    }

    /// Forgets every record (a restart's or a truncation's).
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.decoded.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn forget(&mut self, gone: impl Fn((u32, u64)) -> bool) {
        let mut freed = 0usize;
        self.decoded.retain(|key, record| {
            if gone(*key) {
                freed = freed.saturating_add(record.size());
                false
            } else {
                true
            }
        });
        self.order.retain(|key| !gone(*key));
        self.bytes = self.bytes.saturating_sub(freed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8, size: usize) -> Vec<u8> {
        (0..size)
            .map(|i| fill.wrapping_add((i % 7) as u8))
            .collect()
    }

    fn images<'a>(pages: &[(u32, &'a [u8])]) -> Vec<(u32, Body<'a>)> {
        pages.iter().map(|(p, i)| (*p, Body::Image(i))).collect()
    }

    fn image_of(record: &Record, page: u32) -> Option<Vec<u8>> {
        match record.body(page)? {
            Body::Image(image) => Some(image.to_vec()),
            Body::Delta { .. } => None,
        }
    }

    #[test]
    fn a_record_decodes_to_the_pages_it_was_made_of() {
        let size = 4096;
        let (a, b, c) = (page(1, size), page(2, size), vec![0u8; size]);
        let payload = encode(&images(&[(7, &a), (3, &b), (900, &c)]), size)
            .unwrap()
            .0;
        assert!(payload.len() < 3 * size, "the record didn't compress");
        let record = Record::decode(&payload, size).unwrap();
        assert_eq!(record.pages().collect::<Vec<_>>(), vec![7, 3, 900]);
        assert_eq!(image_of(&record, 7).unwrap(), a);
        assert_eq!(image_of(&record, 3).unwrap(), b);
        assert_eq!(image_of(&record, 900).unwrap(), c);
        assert!(record.body(8).is_none());
    }

    #[test]
    fn padding_after_a_payload_is_ignored() {
        let size = 1024;
        let a = page(9, size);
        let mut payload = encode(&images(&[(1, &a)]), size).unwrap().0;
        let n = slots(payload.len(), size).unwrap() as usize;
        payload.resize(n * size, 0);
        let record = Record::decode(&payload, size).unwrap();
        assert_eq!(image_of(&record, 1).unwrap(), a);
    }

    #[test]
    fn a_payload_takes_the_slots_it_fills() {
        assert_eq!(slots(1, 4096).unwrap(), 1);
        assert_eq!(slots(4096, 4096).unwrap(), 1);
        assert_eq!(slots(4097, 4096).unwrap(), 2);
        assert_eq!(slots(0, 4096).unwrap(), 1);
        assert!(slots(10, 0).is_err());
    }

    #[test]
    fn a_damaged_record_is_refused_not_read() {
        let size = 512;
        let a = page(4, size);
        let mut b = a.clone();
        b[100..140].fill(0xee);
        let runs = diff(&a, &b, 0, size).unwrap();
        let entries = [
            (5, Body::Image(&a)),
            (
                6,
                Body::Delta {
                    depth: 1,
                    runs: &runs,
                },
            ),
        ];
        let (payload, made) = encode(&entries, size).unwrap();
        assert_eq!(made.pages().collect::<Vec<_>>(), vec![5, 6]);
        // Cut short anywhere, or its lengths or bytes damaged: an error, never a panic.
        for cut in 0..payload.len() {
            assert!(
                Record::decode(&payload[..cut], size).is_err(),
                "cut at {cut}"
            );
        }
        for at in 0..payload.len() {
            for flip in [0x01u8, 0xa5, 0xff] {
                let mut damaged = payload.clone();
                damaged[at] ^= flip;
                if let Ok(record) = Record::decode(&damaged, size) {
                    // A flipped byte in a body can decode; what decodes stays well formed.
                    for p in record.pages() {
                        let mut image = vec![0u8; size];
                        match record.body(p).unwrap() {
                            Body::Image(i) => assert_eq!(i.len(), size),
                            Body::Delta { runs, .. } => apply(runs, &mut image).unwrap(),
                        }
                    }
                }
            }
        }
        let mut huge = payload;
        huge[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Record::decode(&huge, size).is_err());
    }

    #[test]
    fn a_record_holds_one_to_1024_entries_each_well_formed() {
        let size = 512;
        let a = page(1, size);
        assert!(encode(&[], size).is_err());
        assert!(encode(&[(1, Body::Image(&a[..100]))], size).is_err());
        assert!(encode(
            &[(
                1,
                Body::Delta {
                    depth: 0,
                    runs: &[]
                }
            )],
            size
        )
        .is_err());
        let too_deep = MOST_DELTAS + 1;
        assert!(encode(
            &[(
                1,
                Body::Delta {
                    depth: too_deep,
                    runs: &[]
                }
            )],
            size
        )
        .is_err());
        let many: Vec<(u32, &[u8])> = (1..=1025u32).map(|p| (p, &a[..])).collect();
        assert!(encode(&images(&many), size).is_err());
        assert!(encode(&images(&many[..1024]), size).is_ok());
        assert!(
            encode(&images(&[(3, &a[..]), (3, &a[..])]), size).is_err(),
            "a page twice"
        );
    }

    #[test]
    fn what_lz4_doesnt_shrink_is_stored_as_it_is() {
        let size = 512;
        // Bytes with no repeats LZ4 could use: a xorshift's.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..size)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let payload = encode(&images(&[(2, &noise)]), size).unwrap().0;
        assert_eq!(&payload[..4], &[0, 0, 0, 0], "stored, not compressed");
        let record = Record::decode(&payload, size).unwrap();
        assert_eq!(image_of(&record, 2).unwrap(), noise);
    }

    #[test]
    fn a_delta_turns_a_page_into_its_next_version() {
        let size = 4096;
        let base = page(3, size);
        let mut next = base.clone();
        next[10] ^= 1;
        next[12] ^= 1; // within a run's header of the last: one run
        next[2000..2300].fill(9);
        next[4095] ^= 0x80;
        let runs = diff(&base, &next, 0, size).unwrap();
        assert_eq!(runs.len(), (4 + 3) + (4 + 300) + (4 + 1));
        let mut image = base.clone();
        apply(&runs, &mut image).unwrap();
        assert_eq!(image, next);
        // The tail is carried whatever it holds, and nothing changed is an empty delta.
        assert_eq!(diff(&base, &base, 0, size).unwrap(), Vec::<u8>::new());
        let tail = diff(&base, &base, 8, size).unwrap();
        assert_eq!(&tail[..4], &[0x0f, 0xf8, 0, 7]);
        // Carried whole even where its first bytes didn't change and a later one did.
        let mut late = base.clone();
        late[4093] ^= 1;
        let runs = diff(&base, &late, 8, size).unwrap();
        assert_eq!(
            &runs[..4],
            &[0x0f, 0xf8, 0, 7],
            "the whole tail, from its first byte"
        );
        let mut log_copy = base.clone();
        log_copy[4088..].fill(0xaa);
        apply(&runs, &mut log_copy).unwrap();
        assert_eq!(log_copy, late);
        // A delta that saves too little isn't one.
        let other = page(4, size);
        assert!(diff(&base, &other, 0, size / 2).is_none());
    }

    #[test]
    fn a_chain_of_deltas_rebuilds_the_newest_version() {
        let size = 1024;
        let v0 = page(5, size);
        let mut versions = vec![v0.clone()];
        let mut runs = Vec::new();
        for k in 1..=MOST_DELTAS as usize {
            let mut next = versions[k - 1].clone();
            next[k * 50..k * 50 + 20].fill(k as u8);
            runs.push(diff(&versions[k - 1], &next, 0, size).unwrap());
            versions.push(next);
        }
        let mut chain: Vec<Body> = runs
            .iter()
            .enumerate()
            .rev()
            .map(|(i, r)| Body::Delta {
                depth: (i + 1) as u8,
                runs: r,
            })
            .collect();
        chain.push(Body::Image(&v0));
        assert_eq!(rebuild(&chain, size).unwrap(), *versions.last().unwrap());
        // Any suffix of the chain back to its image rebuilds that version.
        assert_eq!(
            rebuild(&chain[3..], size).unwrap(),
            versions[versions.len() - 4]
        );
        // A chain that doesn't reach an image is refused.
        assert!(rebuild(&chain[..2], size).is_err());
    }

    #[test]
    fn a_run_past_the_page_is_refused() {
        let size = 64;
        let mut image = vec![0u8; size];
        let run = [0, 60, 0, 7, 1, 2, 3, 4, 5, 6, 7, 8];
        assert!(apply(&run, &mut image).is_err());
        assert!(apply(&run[..6], &mut image).is_err(), "cut short");
        assert!(apply(&[0, 56, 0, 7, 1, 2, 3, 4, 5, 6, 7, 8], &mut image).is_ok());
        assert_eq!(&image[56..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn slots_filled_from_a_payload_gather_back_to_it() {
        let size = 1024;
        let a = page(3, size);
        let pages: Vec<(u32, &[u8])> = (1..=9u32).map(|p| (p, &a[..])).collect();
        let payload = encode(&images(&pages), size).unwrap().0;
        let n = slots(payload.len(), size).unwrap() as usize;
        let frame = WAL_FRAME_HEADER_SIZE + size;
        let mut log = vec![0xffu8; n * frame];
        for (i, slot) in log.chunks_exact_mut(frame).enumerate() {
            fill_slot(&payload, i, &mut slot[WAL_FRAME_HEADER_SIZE..]).unwrap();
        }
        let gathered = gather(&log, size).unwrap();
        assert_eq!(&gathered[..payload.len()], &payload[..]);
        assert!(
            gathered[payload.len()..].iter().all(|b| *b == 0),
            "padding is zeros"
        );
        let record = Record::decode(&gathered, size).unwrap();
        assert_eq!(
            record.pages().collect::<Vec<_>>(),
            (1..=9u32).collect::<Vec<_>>()
        );
        assert!(gather(&log[..frame - 1], size).is_err(), "a slot cut short");
    }

    #[test]
    fn no_record_takes_more_slots_than_most_slots_says() {
        for size in [512usize, 4096, 65536] {
            let most = most_slots(size).unwrap() as usize;
            assert!(
                most >= MOST_PAGES,
                "1024 pages that don't compress take a slot each at least"
            );
        }
    }

    #[test]
    fn decoded_records_stay_within_their_budget_and_go_with_their_slots() {
        let size = 4096;
        let a = page(1, size);
        let pages: Vec<(u32, &[u8])> = (1..=256u32).map(|p| (p, &a[..])).collect();
        let big = Arc::new(encode(&images(&pages), size).unwrap().1);
        let mut records = Records::default();
        for head in 1..=40u64 {
            records.publish(head * 10, 3);
            records.keep(1, head * 10, big.clone());
            assert!(records.bytes <= DECODED_BUDGET);
        }
        assert!(
            big.size() * 2 < DECODED_BUDGET,
            "the test keeps two or more"
        );
        assert!(records.decoded(1, 400).is_some(), "the newest is kept");
        assert!(records.decoded(1, 10).is_none(), "the oldest went first");
        assert_eq!(records.slots_of(10), Some(3));
        // Republished slots drop what was decoded there; a rollback forgets what's past its end.
        records.publish(400, 2);
        assert!(records.decoded(1, 400).is_none());
        assert_eq!(records.slots_of(400), Some(2));
        records.drop_after(300);
        assert_eq!(records.slots_of(390), None);
        assert_eq!(records.slots_of(300), Some(3));
        assert!(records.decoded(1, 390).is_none());
        records.clear();
        assert_eq!(records.slots_of(10), None);
        assert_eq!(records.bytes, 0);
    }

    #[test]
    fn a_frame_is_found_in_the_record_that_covers_it() {
        let mut records = Records::default();
        records.publish(5, 3);
        records.publish(8, 1);
        assert_eq!(records.covering(4), None);
        assert_eq!(records.covering(5), Some(5));
        assert_eq!(records.covering(7), Some(5));
        assert_eq!(records.covering(8), Some(8));
        assert_eq!(records.covering(9), None);
    }

    #[test]
    fn a_first_slot_is_told_from_a_continuation() {
        assert!(is_first_slot(1));
        assert!(is_first_slot(513));
        assert!(!is_first_slot(0));
        assert!(!is_first_slot(CONTINUATION));
    }
}
