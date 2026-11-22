//! Amphora: the packed log (Amphora's `docs/design/store.md`, decisions 16 and 17).
//!
//! A generation of the log whose header carries [`PACKED_FORMAT`] holds records instead of one
//! page a frame. A prepared batch of pages (a commit's, or a spill's) is cut into records of up to
//! [`RECORD_BYTES`], each its pages' entries compressed together (LZ4) and laid in as many of the
//! log's fixed-size frame slots as it takes. An entry is a page's whole image, or the bytes that
//! changed since the page's last version in the log (a delta, while the deltas since its image take
//! less than the page). Every page of a record maps to the record's first slot, so frame ids keep their
//! meaning everywhere else (read marks, backfill, restarts). The first slot's frame header carries
//! the record's slot count where a page number would be, the others [`CONTINUATION`]; the record's
//! last slot carries the commit's database size when it ends one. The slots' checksums chain as
//! frames' do.
//!
//! A mixed generation ([`MIXED_FORMAT`], decision 37) holds page frames and records side by side:
//! spills write page frames (over the transaction's own, decision 30), commits write records. A
//! record's first slot carries [`RECORD_HEAD`] with its slot count, apart from page numbers.
//!
//! A mixed generation of [`MIXED_FRAMES_FORMAT`] (decision 39) writes a commit's pages by what a
//! read of them needs: a page the log can't chain from is a page frame, as page-a-frame writes it;
//! the entries of chains (deltas, and the images that end a chain) are packed into records of one
//! slot each ([`one_slot_records`]), and a chain may begin at a page frame. A read past the cache
//! then reads one frame for each entry of its page's chain.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::error::LimboError;
use crate::storage::sqlite3_ondisk::WAL_FRAME_HEADER_SIZE;
use crate::Result;

/// The log header's file format for a generation of one page a frame (SQLite's).
pub(crate) const PAGE_FORMAT: u32 = 3_007_000;

/// The log header's file format for a packed generation whose chains end at [`EIGHT_DELTAS`] in a
/// row (decision 17, to 2026-11-11): read as ever, and written so while a generation begun so
/// goes on, so a binary from before reads it.
pub(crate) const PACKED_FORMAT: u32 = 0x414d_5002;

/// The log header's file format for a mixed generation: page frames and records side by side
/// (Amphora's docs/design/store.md, decision 37), its chains ending at [`EIGHT_DELTAS`].
pub(crate) const MIXED_FORMAT: u32 = 0x414d_5003;

/// The packed format whose chains end where their bytes reach a page (decision 17, 2026-11-11):
/// what a packed generation begins with now. A binary from before refuses it as a format it doesn't
/// know, naming the value, never reads it as corrupt.
pub(crate) const PACKED_CHAINS_FORMAT: u32 = 0x414d_5004;

/// The mixed format whose chains end where their bytes reach a page (decision 17, 2026-11-11), laid
/// out as decision 37 has it: read as ever, and written so while a generation begun so goes on.
pub(crate) const MIXED_CHAINS_FORMAT: u32 = 0x414d_5005;

/// The log header's file format for a mixed generation whose images of pages new to a chain are
/// page frames, a chain's entries records of one slot, and whose chains may begin at a page frame
/// (decision 39), its chains ended by their bytes as [`MIXED_CHAINS_FORMAT`]'s are (decision 17):
/// what a mixed generation begins with now. A binary that knows only [`MIXED_FORMAT`] or
/// [`MIXED_CHAINS_FORMAT`] refuses it by name rather than misread a chain that begins at a page
/// frame. (0x414d_5006 is a value no binary writes: tests use it as an unknown format.)
pub(crate) const MIXED_FRAMES_FORMAT: u32 = 0x414d_5007;

/// Whether a generation of `file_format` writes its pages new to a chain as page frames and its
/// chains' entries as records of one slot (decision 39); a generation of another format is written
/// as it began.
pub(crate) fn images_as_frames(file_format: u32) -> bool {
    file_format == MIXED_FRAMES_FORMAT
}

/// Deltas in a row at most in a generation of [`PACKED_FORMAT`] or [`MIXED_FORMAT`]: those
/// formats' bound, to which binaries from before 2026-11-11 hold what they read.
pub(crate) const EIGHT_DELTAS: u8 = 8;

/// Deltas in a row at most in a generation of `file_format`: [`EIGHT_DELTAS`] in the formats of
/// before, the depth field's width ([`MOST_DELTAS`]) in those of chains ended by their bytes
/// ([`PACKED_CHAINS_FORMAT`], [`MIXED_CHAINS_FORMAT`], [`MIXED_FRAMES_FORMAT`]).
pub(crate) fn most_deltas(file_format: u32) -> u8 {
    match file_format {
        PACKED_FORMAT | MIXED_FORMAT => EIGHT_DELTAS,
        _ => MOST_DELTAS,
    }
}

/// The page-number field of a record's slots after its first.
pub(crate) const CONTINUATION: u32 = u32::MAX;

/// In a mixed generation, the bit of a page-number field that marks a record's first slot (its
/// slot count in the bits below): a page frame's page number is below it.
pub(crate) const RECORD_HEAD: u32 = 0x8000_0000;

/// What a generation of the log holds: one page a frame (SQLite's), packed records only
/// (decision 16), or page frames for spills and records for commits (decision 37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pages,
    Packed,
    Mixed,
}

impl LogFormat {
    /// The format a log header's file format names: None for one this engine doesn't write.
    pub(crate) fn of(file_format: u32) -> Option<Self> {
        match file_format {
            PAGE_FORMAT => Some(Self::Pages),
            PACKED_FORMAT | PACKED_CHAINS_FORMAT => Some(Self::Packed),
            MIXED_FORMAT | MIXED_CHAINS_FORMAT | MIXED_FRAMES_FORMAT => Some(Self::Mixed),
            _ => None,
        }
    }

    /// The log header's file format a generation of this format begins with.
    pub(crate) fn file_format(self) -> u32 {
        match self {
            Self::Pages => PAGE_FORMAT,
            Self::Packed => PACKED_CHAINS_FORMAT,
            Self::Mixed => MIXED_FRAMES_FORMAT,
        }
    }

    /// Whether a generation of this format writes records.
    pub(crate) fn holds_records(self) -> bool {
        !matches!(self, Self::Pages)
    }

    /// The page-number field of the first slot of a record of `slots` slots.
    pub(crate) fn head_field(self, slots: u32) -> Result<u32> {
        match self {
            Self::Packed => Ok(slots),
            Self::Mixed if slots < RECORD_HEAD => Ok(RECORD_HEAD | slots),
            Self::Mixed => Err(corrupt("record takes too many slots")),
            Self::Pages => Err(LimboError::InternalError(
                "a record in a log of one page a frame".into(),
            )),
        }
    }

    /// The slot count a record's first slot claims in `page_field`: None for a field that is no
    /// record's first slot (0, [`CONTINUATION`], or in a mixed generation a page number).
    pub(crate) fn head_slots(self, page_field: u32) -> Option<u32> {
        match self {
            Self::Packed => {
                if is_first_slot(page_field) {
                    Some(page_field)
                } else {
                    None
                }
            }
            Self::Mixed => {
                let slots = page_field & !RECORD_HEAD;
                if page_field != CONTINUATION && page_field & RECORD_HEAD != 0 && slots != 0 {
                    Some(slots)
                } else {
                    None
                }
            }
            Self::Pages => None,
        }
    }
}

/// A record's payload starts with its stored and raw lengths.
const PAYLOAD_HEADER: usize = 8;

/// Pages a record holds at most: a prepared batch is at most 1024 (`IOV_MAX` in `wal.rs`).
const MOST_PAGES: usize = 1024;

/// Raw bytes a record is cut at: LZ4 finds a match at most 64 KiB back (its block format,
/// `lz4_Block_format.md`), so a larger record compresses little better, while a reader that misses
/// its page cache reads and decodes a whole record. That is 64 whole pages of 4 KiB, about 18
/// slots at the 3x measured for whole pages (decision 16), the last slot's padding about 3% of it.
pub(crate) const RECORD_BYTES: usize = 256 << 10;

/// Deltas in a row at most: the width of an entry's depth field, a format bound. The writer ends a
/// chain long before it by its bytes (decision 17): a page's deltas since its image take less than
/// the page, each at least a run's header and a byte.
pub(crate) const MOST_DELTAS: u8 = u8::MAX;

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
    /// last image, this one included (1 to [`MOST_DELTAS`]); their runs take less than a page.
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
/// length][the raw bytes compressed], or, where LZ4 doesn't make them smaller or `compress` says
/// not to try, stored length 0 and the raw bytes as they are (an LZ4 block of anything is never
/// empty). The raw bytes are [entry
/// count] then each entry's header ([page][kind][depth][0u16][body length]) then the bodies, in
/// order, integers big-endian. With it, the record as a read decodes it, built from the entries
/// it was made of rather than parsed again (each was checked as it was laid in).
pub(crate) fn encode(
    entries: &[(u32, Body<'_>)],
    page_size: usize,
    compress: bool,
) -> Result<(Vec<u8>, Record)> {
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
    let mut made = Vec::with_capacity(entries.len());
    let mut body_at = entries
        .len()
        .checked_mul(ENTRY_HEADER)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| corrupt("entry headers overflow"))?;
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
        let end = body_at
            .checked_add(body.bytes().len())
            .ok_or_else(|| corrupt("entry length overflows"))?;
        made.push(Entry {
            page: *page,
            kind,
            depth: body.depth(),
            start: body_at,
            end,
        });
        body_at = end;
    }
    for (_, body) in entries {
        raw.extend_from_slice(body.bytes());
    }
    let compressed = if compress {
        lz4_flex::block::compress(&raw)
    } else {
        Vec::new()
    };
    let (stored_len, stored) = if compress && compressed.len() < raw.len() {
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
    Ok((payload, Record::index(made, raw)?))
}

/// What [`one_slot_records`] made of a commit's entries of chains: each record's entries (indexes
/// into the entries it was given), its payload and the record as a read decodes it; and the images
/// that take more than a slot alone, which the caller writes as page frames.
pub(crate) struct OneSlot {
    pub(crate) records: Vec<(Vec<usize>, Vec<u8>, Record)>,
    pub(crate) frames: Vec<usize>,
}

/// A commit's entries of chains (deltas, and the images that end a chain) as records of one slot
/// each (decision 39), so a read past the cache reads one slot for each entry of its page's chain
/// where a record of many slots had it read them all. Entries go largest first, each into the
/// first record with room left for it (first-fit decreasing, which takes at most 11/9 of the fewest
/// records and one more: Dósa, ESCAPE 2007), an image sized by what LZ4 makes of it alone and a
/// delta by its bytes. Each record is then LZ4'd where that makes it smaller; an entry its record
/// can't hold once encoded moves to a record of its own. An image LZ4 can't fit in a slot is left
/// for a page frame; a delta too large for a slot takes the slots it needs, in a record of its own.
pub(crate) fn one_slot_records(entries: &[(u32, Body<'_>)], page_size: usize) -> Result<OneSlot> {
    if entries.len() > MOST_PAGES {
        return Err(corrupt("a record holds 1 to 1024 pages"));
    }
    let fixed = PAYLOAD_HEADER
        .checked_add(4)
        .ok_or_else(|| corrupt("record too large"))?;
    let mut sized = Vec::with_capacity(entries.len());
    let mut frames = Vec::new();
    for (index, (_, body)) in entries.iter().enumerate() {
        let len = match body {
            Body::Image(image) => lz4_flex::block::compress(image).len().min(image.len()),
            Body::Delta { runs, .. } => runs.len(),
        };
        let size = ENTRY_HEADER
            .checked_add(len)
            .ok_or_else(|| corrupt("entry too large"))?;
        let alone = fixed
            .checked_add(size)
            .ok_or_else(|| corrupt("entry too large"))?;
        if alone > page_size && matches!(body, Body::Image(_)) {
            frames.push(index);
        } else {
            sized.push((index, size, alone));
        }
    }
    sized.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // Each record's entries and the bytes they take with its header.
    let mut bins: Vec<(Vec<usize>, usize)> = Vec::new();
    for (index, size, alone) in sized {
        let room = bins.iter_mut().find_map(|(held, used)| {
            let after = used.checked_add(size).filter(|&n| n <= page_size)?;
            Some((held, used, after))
        });
        match room {
            Some((held, used, after)) => {
                held.push(index);
                *used = after;
            }
            None => bins.push((vec![index], alone)),
        }
    }
    let mut records = Vec::with_capacity(bins.len());
    let mut queue: VecDeque<Vec<usize>> = bins.into_iter().map(|(held, _)| held).collect();
    let mut part: Vec<(u32, Body<'_>)> = Vec::with_capacity(entries.len());
    // Each pass encodes a record or moves one entry out of it: at most twice the entries.
    while let Some(mut held) = queue.pop_front() {
        part.clear();
        for &index in &held {
            let entry = entries
                .get(index)
                .copied()
                .ok_or_else(|| corrupt("an entry past the commit's"))?;
            part.push(entry);
        }
        let (payload, record) = encode(&part, page_size, true)?;
        if payload.len() <= page_size || held.len() == 1 {
            records.push((held, payload, record));
        } else if let Some(moved) = held.pop() {
            queue.push_front(held);
            queue.push_back(vec![moved]);
        }
    }
    Ok(OneSlot { records, frames })
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
    // Runs are written as their ranges close: one buffer, taken at the most a delta may hold.
    let mut runs = Vec::with_capacity(most.min(new.len()));
    let mut open: Option<(usize, usize)> = None;
    // A word of 8 bytes at a time, each compared as one integer (`differing`): a word as it was,
    // none of it carried, costs one comparison, and a page's changes are a few runs in it
    // (decision 17), so most words are. No block size is picked: the integer's width is the unit.
    for (word, (a, b)) in base.chunks(8).zip(new.chunks(8)).enumerate() {
        let at = word.checked_mul(8)?;
        let end = at.checked_add(a.len())?;
        let forced = (end > carried).then(|| carried.max(at).saturating_sub(at));
        let Some((differs_first, differs_last)) =
            differing(a, b).or_else(|| forced.map(|f| (f, f)))
        else {
            continue;
        };
        let first = match forced {
            Some(f) => differs_first.min(f),
            None => differs_first,
        };
        // A forced suffix runs to the chunk's end.
        let last = match forced {
            Some(_) => a.len().checked_sub(1)?,
            None => differs_last,
        };
        let (start, stop) = (
            at.checked_add(first)?,
            at.checked_add(last)?.checked_add(1)?,
        );
        open = match open {
            Some((from, to)) if start <= to.saturating_add(RUN_GAP) => {
                Some((from.min(start), to.max(stop)))
            }
            Some(closed) => {
                put_run(&mut runs, new, closed, most)?;
                Some((start, stop))
            }
            None => Some((start, stop)),
        };
    }
    if let Some(closed) = open {
        put_run(&mut runs, new, closed, most)?;
    }
    Some(runs)
}

/// The first and last byte at which two chunks of `diff` differ, if any: a whole word of 8 (a
/// `u64`'s width) is compared as one integer, its first and last differing bytes the trailing and
/// leading zero bytes of the two XORed (little-endian, so the first byte is the lowest), where a
/// byte at a time took up to 16 comparisons a word (Decision (lane wal-read)). A chunk shorter than
/// a word (the end of a page whose size isn't a multiple of 8) is compared byte by byte.
fn differing(a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    match (<[u8; 8]>::try_from(a), <[u8; 8]>::try_from(b)) {
        (Ok(a), Ok(b)) => {
            let x = u64::from_le_bytes(a) ^ u64::from_le_bytes(b);
            if x == 0 {
                return None;
            }
            let first = usize::try_from(x.trailing_zeros() / 8).ok()?;
            let last = 7usize.checked_sub(usize::try_from(x.leading_zeros() / 8).ok()?)?;
            Some((first, last))
        }
        _ => {
            let first = a.iter().zip(b).position(|(x, y)| x != y)?;
            let last = a.iter().zip(b).rposition(|(x, y)| x != y)?;
            Some((first, last))
        }
    }
}

/// Writes the run of `new`'s bytes `range` covers into `runs`: none (the page is then written
/// whole) once they take `most` bytes or more.
fn put_run(runs: &mut Vec<u8>, new: &[u8], range: (usize, usize), most: usize) -> Option<()> {
    let (start, end) = range;
    let len = end.checked_sub(start)?;
    let offset = u16::try_from(start).ok()?;
    let less_one = u16::try_from(len.checked_sub(1)?).ok()?;
    runs.extend_from_slice(&offset.to_be_bytes());
    runs.extend_from_slice(&less_one.to_be_bytes());
    runs.extend_from_slice(new.get(start..end)?);
    if runs.len() >= most {
        return None;
    }
    Some(())
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

/// The slot count a packed record's first slot carries in its frame header's page-number field: one
/// that is neither 0 (no frame) nor [`CONTINUATION`].
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
        Self::index(entries, raw)
    }

    /// A record of `entries` (each its body's place in `raw`), each found by its page; a page
    /// twice in it is refused.
    fn index(entries: Vec<Entry>, raw: Vec<u8>) -> Result<Self> {
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

/// Bytes the decoded records may hold, the oldest going first past it: what a connection's page
/// cache holds by default (`page_cache::DEFAULT_PAGE_CACHE_SIZE_IN_PAGES` pages of `page_size`, the
/// budget Amphora's readers keep, decision 5). A decoded record stands for the pages reads rebuild
/// from it, so the records kept take no more than the pages a cache keeps (decision 39). The writer
/// keeps each record it publishes, and reads (a miss's chain back to its image, the checkpoint's
/// copies) mostly want recent ones: a chain of a page changed at every commit reaches back as many
/// commits as its deltas take less than the page (decision 17). A record not kept is read again, a
/// slot (decision 39) or the record it was.
pub(crate) fn decoded_budget(page_size: usize) -> usize {
    // A budget past what memory can address is all of it.
    crate::storage::page_cache::DEFAULT_PAGE_CACHE_SIZE_IN_PAGES.saturating_mul(page_size)
}

/// What the shared log keeps of its packed records: each record's slot count by its first slot
/// (written before any page maps to the record), and records decoded, each with the log's generation
/// (its header's checkpoint sequence) it was kept for, within [`decoded_budget`].
///
/// Publishing a record, which the writer does for every record it commits, costs a lookup, not a
/// pass over what is kept: a commit of decision 39 publishes a record for every slot of its chains'
/// entries, and a pass over the hundreds of records the budget keeps, at each of them, was the
/// commit's cost (Decision (lane wal-read)).
#[derive(Debug, Default)]
pub(crate) struct Records {
    /// Slot counts by first slot, in frame order: the record that covers a frame is the last one
    /// that begins at or before it.
    slots: BTreeMap<u64, u32>,
    /// The records kept decoded, by first slot.
    decoded: FxHashMap<u64, Kept>,
    /// The first slots in the order their records were kept, each with its keep's number: an entry
    /// whose record went (published over, dropped, or kept again) no longer matches its number and
    /// is passed over. Stale entries are swept once they outnumber the records kept: a sweep then
    /// costs no more than twice the stale entries it removes, so each costs a constant share of
    /// one, and the order holds at most twice the records kept and one more. No number is picked:
    /// the bound is where stale entries pass live ones.
    order: VecDeque<(u64, u64)>,
    /// The next keep's number.
    kept: u64,
    bytes: usize,
}

/// A record [`Records`] keeps decoded: the generation it is of, its keep's number in
/// `Records::order`, and the record.
#[derive(Debug)]
struct Kept {
    generation: u32,
    number: u64,
    record: Arc<Record>,
}

impl Records {
    /// A record of `slots` slots now begins at `head`: whatever was decoded there before is dropped
    /// (a rollback or a restart can lay a new record in the same slots).
    pub(crate) fn publish(&mut self, head: u64, slots: u32) {
        self.slots.insert(head, slots);
        self.release(head);
    }

    /// The slots of the record at `head`.
    pub(crate) fn slots_of(&self, head: u64) -> Option<u32> {
        self.slots.get(&head).copied()
    }

    /// The first slot of the record whose slots hold `frame`, if one does.
    pub(crate) fn covering(&self, frame: u64) -> Option<u64> {
        let (&head, &slots) = self.slots.range(..=frame).next_back()?;
        let last = head.checked_add(u64::from(slots))?.checked_sub(1)?;
        if frame <= last {
            Some(head)
        } else {
            None
        }
    }

    /// The record at `head` of `generation`, if it was decoded and kept.
    pub(crate) fn decoded(&self, generation: u32, head: u64) -> Option<Arc<Record>> {
        self.decoded
            .get(&head)
            .filter(|kept| kept.generation == generation)
            .map(|kept| kept.record.clone())
    }

    /// Keeps a decoded record of a log of `page_size` pages, the oldest kept going first past the
    /// budget ([`decoded_budget`]). One of another generation kept at the same first slot goes.
    pub(crate) fn keep(
        &mut self,
        generation: u32,
        head: u64,
        record: Arc<Record>,
        page_size: usize,
    ) {
        let budget = decoded_budget(page_size);
        let size = record.size();
        if size > budget {
            return;
        }
        match self.decoded.get(&head) {
            Some(kept) if kept.generation == generation => return,
            Some(_) => self.release(head),
            None => {}
        }
        while self.bytes.saturating_add(size) > budget {
            let Some((oldest, number)) = self.order.pop_front() else {
                break;
            };
            if self
                .decoded
                .get(&oldest)
                .is_some_and(|kept| kept.number == number)
            {
                self.release(oldest);
            }
        }
        let number = self.kept;
        self.kept = self.kept.wrapping_add(1);
        self.decoded.insert(
            head,
            Kept {
                generation,
                number,
                record,
            },
        );
        self.order.push_back((head, number));
        self.bytes = self.bytes.saturating_add(size);
        if self.order.len() > self.decoded.len().saturating_mul(2) {
            let decoded = &self.decoded;
            self.order.retain(|(head, number)| {
                decoded.get(head).is_some_and(|kept| kept.number == *number)
            });
        }
    }

    /// Forgets the records past `max_frame` (a rollback's).
    pub(crate) fn drop_after(&mut self, max_frame: u64) {
        let Some(from) = max_frame.checked_add(1) else {
            return;
        };
        drop(self.slots.split_off(&from));
        let mut freed = 0usize;
        self.decoded.retain(|head, kept| {
            if *head > max_frame {
                freed = freed.saturating_add(kept.record.size());
                false
            } else {
                true
            }
        });
        self.bytes = self.bytes.saturating_sub(freed);
        let decoded = &self.decoded;
        self.order
            .retain(|(head, number)| decoded.get(head).is_some_and(|kept| kept.number == *number));
    }

    /// Forgets every record (a restart's or a truncation's).
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.decoded.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// Drops the record decoded at `head`, if one is kept; its entry in `order` goes stale.
    fn release(&mut self, head: u64) {
        if let Some(gone) = self.decoded.remove(&head) {
            self.bytes = self.bytes.saturating_sub(gone.record.size());
        }
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
        let payload = encode(&images(&[(7, &a), (3, &b), (900, &c)]), size, true)
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
        let mut payload = encode(&images(&[(1, &a)]), size, true).unwrap().0;
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
        let (payload, made) = encode(&entries, size, true).unwrap();
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
        assert!(encode(&[], size, true).is_err());
        assert!(encode(&[(1, Body::Image(&a[..100]))], size, true).is_err());
        assert!(encode(
            &[(
                1,
                Body::Delta {
                    depth: 0,
                    runs: &[]
                }
            )],
            size,
            true
        )
        .is_err());
        let many: Vec<(u32, &[u8])> = (1..=1025u32).map(|p| (p, &a[..])).collect();
        assert!(encode(&images(&many), size, true).is_err());
        assert!(encode(&images(&many[..1024]), size, true).is_ok());
        assert!(
            encode(&images(&[(3, &a[..]), (3, &a[..])]), size, true).is_err(),
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
        let payload = encode(&images(&[(2, &noise)]), size, true).unwrap().0;
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
        // Deeper than the eight in a row a chain once ran to (decision 17).
        for k in 1..=12usize {
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
        let payload = encode(&images(&pages), size, true).unwrap().0;
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
        let big = Arc::new(encode(&images(&pages), size, true).unwrap().1);
        let mut records = Records::default();
        for head in 1..=40u64 {
            records.publish(head * 10, 3);
            records.keep(1, head * 10, big.clone(), size);
            assert!(records.bytes <= decoded_budget(size));
        }
        assert!(
            big.size() * 2 < decoded_budget(size),
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

    /// Decision (lane wal-read), AC-T1.1.3: do: publish records over kept ones at the same first
    /// slots, round after round, as a commit's are. Expect: the registry's order of keeps stays at
    /// most twice the records kept and one more, its bytes are the kept records' sizes, the newest
    /// are kept, a record of another generation at the same first slot is never given out, and a
    /// rollback leaves no stale keep listed.
    #[test]
    fn records_published_over_kept_ones_leave_a_bounded_order() {
        let size = 4096;
        let a = page(1, size);
        let pages: Vec<(u32, &[u8])> = (1..=2u32).map(|p| (p, &a[..])).collect();
        let small = Arc::new(encode(&images(&pages), size, true).unwrap().1);
        let mut records = Records::default();
        for round in 0..50u64 {
            for head in 1..=100u64 {
                records.publish(head, 1);
                records.keep(1, head, small.clone(), size);
                assert!(
                    records.order.len() <= records.decoded.len() * 2 + 1,
                    "round {round}, head {head}: {} keeps listed for {} kept",
                    records.order.len(),
                    records.decoded.len()
                );
            }
        }
        assert_eq!(records.bytes, records.decoded.len() * small.size());
        assert!(records.decoded(1, 100).is_some(), "the newest is kept");
        records.keep(2, 100, small.clone(), size);
        assert!(
            records.decoded(1, 100).is_none(),
            "another generation's record at the same slot replaced it"
        );
        assert!(records.decoded(2, 100).is_some());
        assert_eq!(records.bytes, records.decoded.len() * small.size());
        records.drop_after(50);
        assert!(records.decoded(1, 50).is_some() && records.decoded(1, 51).is_none());
        assert_eq!(records.order.len(), records.decoded.len());
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

    /// Decision 37: in a mixed generation a record's first slot is told from a page frame, a
    /// continuation and an empty slot by its marked page field, and every count a record can take
    /// reads back as written; a packed generation keeps its unmarked counts.
    #[test]
    fn a_mixed_records_first_slot_is_told_from_a_page_frame() {
        let mixed = LogFormat::Mixed;
        for slots in [1, 2, 65, most_slots(4096).unwrap()] {
            let field = mixed.head_field(slots).unwrap();
            assert_eq!(mixed.head_slots(field), Some(slots));
            assert_eq!(LogFormat::Packed.head_field(slots).unwrap(), slots);
        }
        for page in [1, 2, 4096, RECORD_HEAD - 1] {
            assert_eq!(mixed.head_slots(page), None, "page {page} is a page frame");
        }
        assert_eq!(mixed.head_slots(0), None);
        assert_eq!(mixed.head_slots(CONTINUATION), None);
        assert_eq!(mixed.head_slots(RECORD_HEAD), None, "a head of no slots");
        assert!(mixed.head_field(RECORD_HEAD).is_err());
        assert!(LogFormat::Pages.head_field(1).is_err());
        for format in [LogFormat::Pages, LogFormat::Packed, LogFormat::Mixed] {
            assert_eq!(LogFormat::of(format.file_format()), Some(format));
        }
        // A generation begins with the format of chains ended by their bytes (decision 17); a mixed
        // one with page frames for images new to a chain too (decision 39). One begun before keeps
        // its format, read as ever and written as it began, its chains at its format's bound.
        assert_eq!(LogFormat::Packed.file_format(), PACKED_CHAINS_FORMAT);
        assert_eq!(LogFormat::Mixed.file_format(), MIXED_FRAMES_FORMAT);
        assert_eq!(LogFormat::of(PACKED_FORMAT), Some(LogFormat::Packed));
        assert_eq!(LogFormat::of(MIXED_FORMAT), Some(LogFormat::Mixed));
        assert_eq!(LogFormat::of(MIXED_CHAINS_FORMAT), Some(LogFormat::Mixed));
        assert!(images_as_frames(MIXED_FRAMES_FORMAT));
        for older in [
            PAGE_FORMAT,
            PACKED_FORMAT,
            MIXED_FORMAT,
            PACKED_CHAINS_FORMAT,
            MIXED_CHAINS_FORMAT,
        ] {
            assert!(!images_as_frames(older), "{older:#x}");
        }
        assert_eq!(most_deltas(PACKED_FORMAT), EIGHT_DELTAS);
        assert_eq!(most_deltas(MIXED_FORMAT), EIGHT_DELTAS);
        assert_eq!(most_deltas(PACKED_CHAINS_FORMAT), MOST_DELTAS);
        assert_eq!(most_deltas(MIXED_CHAINS_FORMAT), MOST_DELTAS);
        assert_eq!(most_deltas(MIXED_FRAMES_FORMAT), MOST_DELTAS);
        assert_eq!(LogFormat::of(0x414d_5006), None);
    }

    /// Do: change pages of 4 KiB at random places (none, one byte, runs across the edges of the
    /// words diff compares at once, the checksum's tail), diff them against the page before,
    /// with a tail carried and without. Expect: the runs turn the page before into the page
    /// after every time, the carried tail among them, and an unchanged page with nothing carried
    /// gives no runs.
    #[test]
    fn a_delta_found_word_by_word_rebuilds_every_change() {
        let size = 4096;
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let base = page(9, size);
        assert_eq!(diff(&base, &base, 0, size), Some(Vec::new()));
        for round in 0..400 {
            let mut new = base.clone();
            for _ in 0..(round % 7) {
                let at = (next() as usize) % size;
                let len = 1 + (next() as usize) % 90;
                for b in new.iter_mut().skip(at).take(len) {
                    *b = b.wrapping_add(1);
                }
            }
            // Edges of the words compared at once.
            if round % 5 == 0 {
                let edge = 8 * (1 + (next() as usize) % (size / 8 - 1));
                new[edge - 1] ^= 0x80;
                new[edge] ^= 0x40;
            }
            for tail in [0usize, 8] {
                let Some(runs) = diff(&base, &new, tail, size) else {
                    continue;
                };
                let mut rebuilt = base.clone();
                apply(&runs, &mut rebuilt).unwrap();
                assert_eq!(rebuilt, new, "round {round}, tail {tail}");
                if tail > 0 {
                    let mut stale = new.clone();
                    stale[size - tail..].copy_from_slice(&base[size - tail..]);
                    apply(&runs, &mut stale).unwrap();
                    assert_eq!(stale, new, "the tail carried, round {round}");
                }
            }
        }
    }

    /// The records `packed` holds, decoded, with what each holds; every entry once in all.
    fn unpacked(packed: &OneSlot, entries: &[(u32, Body<'_>)], size: usize) -> Vec<u32> {
        let mut seen: Vec<usize> = packed.frames.clone();
        for (held, payload, record) in &packed.records {
            let decoded = Record::decode(payload, size).unwrap();
            for &index in held {
                let (page, body) = entries[index];
                let got = decoded
                    .body(page)
                    .expect("a record holds what it was given");
                assert_eq!(got.depth(), body.depth());
                assert_eq!(got.bytes(), body.bytes());
                assert!(record.body(page).is_some());
            }
            assert_eq!(decoded.pages().count(), held.len());
            seen.extend(held);
        }
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..entries.len()).collect::<Vec<_>>(),
            "each entry once"
        );
        packed
            .records
            .iter()
            .map(|(_, payload, _)| slots(payload.len(), size).unwrap())
            .collect()
    }

    /// Do: pack a commit's entries of chains (images that LZ4 halves, deltas of a few hundred
    /// bytes) into records of one slot (decision 39). Expect: every record one slot, each entry in
    /// one record and decoding as it went in, and no more records than first-fit decreasing's
    /// bound over the bytes (11/9 of a slot's worth and one more).
    #[test]
    fn a_chains_entries_pack_into_records_of_one_slot() {
        let size = 4096;
        // Compressible images: a run of zeros past a patterned half.
        let image = |fill: u8| {
            let mut p = page(fill, size);
            p[size / 2..].fill(0);
            p
        };
        let images: Vec<Vec<u8>> = (0..6u8).map(image).collect();
        let runs: Vec<Vec<u8>> = (0..40u16)
            .map(|k| {
                let len = 100 + usize::from(k) * 9;
                let mut r = Vec::with_capacity(len + 4);
                r.extend_from_slice(&(k * 10).to_be_bytes());
                r.extend_from_slice(&u16::try_from(len - 1).unwrap().to_be_bytes());
                r.extend((0..len).map(|i| (i as u8) ^ (k as u8)));
                r
            })
            .collect();
        let mut entries: Vec<(u32, Body<'_>)> = images
            .iter()
            .enumerate()
            .map(|(i, p)| (u32::try_from(i).unwrap() + 1, Body::Image(p)))
            .collect();
        entries.extend(runs.iter().enumerate().map(|(i, r)| {
            (
                u32::try_from(i).unwrap() + 100,
                Body::Delta {
                    depth: 1 + (i % 8) as u8,
                    runs: r,
                },
            )
        }));
        let packed = one_slot_records(&entries, size).unwrap();
        assert!(
            packed.frames.is_empty(),
            "every image fits a slot once LZ4'd"
        );
        let slots = unpacked(&packed, &entries, size);
        assert!(
            slots.iter().all(|&s| s == 1),
            "records of one slot: {slots:?}"
        );
        let bytes: usize = images
            .iter()
            .map(|p| lz4_flex::block::compress(p).len() + ENTRY_HEADER)
            .chain(runs.iter().map(|r| r.len() + ENTRY_HEADER))
            .sum();
        let fewest = bytes.div_ceil(size - PAYLOAD_HEADER - 4);
        assert!(
            packed.records.len() * 9 <= fewest * 11 + 9,
            "{} records where the bytes take {fewest} slots",
            packed.records.len()
        );
    }

    /// Do: pack an image LZ4 can't shrink below a slot, and a delta near a page long, beside
    /// small deltas. Expect: the image left for a page frame, the delta in a record of its own of
    /// the slots it needs, the rest in records of one slot.
    #[test]
    fn an_entry_that_fits_no_slot_takes_a_frame_or_a_record_of_its_own() {
        let size = 4096;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..size)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let mut long = Vec::new();
        long.extend_from_slice(&0u16.to_be_bytes());
        long.extend_from_slice(&u16::try_from(size - 8 - 1).unwrap().to_be_bytes());
        long.extend_from_slice(&noise[..size - 8]);
        let small: Vec<u8> = [0u8, 5, 0, 2, 1, 2, 3].to_vec();
        let entries: Vec<(u32, Body<'_>)> = vec![
            (1, Body::Image(&noise)),
            (
                2,
                Body::Delta {
                    depth: 1,
                    runs: &long,
                },
            ),
            (
                3,
                Body::Delta {
                    depth: 2,
                    runs: &small,
                },
            ),
            (
                4,
                Body::Delta {
                    depth: 1,
                    runs: &small,
                },
            ),
        ];
        let packed = one_slot_records(&entries, size).unwrap();
        assert_eq!(packed.frames, vec![0], "the image a page frame");
        let slots = unpacked(&packed, &entries, size);
        let alone = packed
            .records
            .iter()
            .position(|(held, _, _)| held.contains(&1))
            .unwrap();
        assert_eq!(packed.records[alone].0, vec![1], "the long delta alone");
        assert_eq!(slots[alone], 2);
        for (at, s) in slots.iter().enumerate() {
            if at != alone {
                assert_eq!(*s, 1);
            }
        }
    }
}
