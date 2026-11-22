//! Amphora: the log's frame index and header read without the locks every page read took
//! (G3 step 2; docs/research/read-path.md fix 1(b); docs/design/store.md decision 40).
//!
//! Every page read took the read side of the `RwLock` over the log's shared state, then the frame
//! index's lock (`find_frame`) or the header's (`wal_header`): at 32 point readers those were the
//! three hottest leaves (`find_frame` 26.9%, `wal_header` 23.4%, `lock_shared_slow` 13.2%). This is
//! the prototype the step asks for, behind a switch ([`LogReadIndex`]), its two ways compared:
//!
//! - `Published`: a read-only copy of the index, published whole and read through an `ArcSwap`;
//!   a reader whose snapshot reaches past the copy publishes a new one (paying the copy, once per
//!   published generation) or, when another reader is already doing so, reads under the lock.
//! - `Optimistic`: the latest frame of each page in a table sharded by the page's hash, read under
//!   each shard's version (LeanStore's validate-after-read: a read whose version moved, or that
//!   finds a frame newer than its snapshot, is answered under the lock instead, never retried).
//!
//! Both answer a page with no frame in the log from an exact bitmap of the pages the log holds,
//! which `Filtered` also does in front of the index read under its lock.
//!
//! What the structures hold is stamped with the index's generation. Appends (the only change made
//! often) update them in place, under the frame index's lock that every change already takes; any
//! other change (frames cut by a rollback or written again, the log restarted, a recovery or a
//! restore) moves the generation on, and the next reader rebuilds them from the index under its
//! lock. A reader that loaded a structure just before the generation moved still answers right:
//! frames cut or written again are past every committed snapshot, and a restart happens only when
//! no reader reads the log.

use crate::storage::sqlite3_ondisk::WalHeader;
use crate::storage::wal::HeaderCell;
use crate::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicU8, Ordering};
use crate::sync::Mutex;
use arc_swap::{ArcSwap, ArcSwapOption};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::sync::OnceLock;

/// How a page read finds its frame in the log (the prototype's switch; `AMPHORA_LOG_READ_INDEX`
/// sets it for a process: `locked`, `filtered`, `published` or `optimistic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LogReadIndex {
    /// Today's path: the frame index read under the shared state's `RwLock` and its own lock.
    Locked = 0,
    /// The page-presence bitmap first; a page it may hold is looked up as `Locked` does.
    Filtered = 1,
    /// The bitmap, then a read-only index published per generation (way (a)).
    Published = 2,
    /// The bitmap, then a sharded table of latest frames read under version checks (way (b)).
    Optimistic = 3,
}

impl LogReadIndex {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Filtered,
            2 => Self::Published,
            3 => Self::Optimistic,
            _ => Self::Locked,
        }
    }

    /// The way logs opened from now on take: `set_process_default`'s if called, else
    /// `AMPHORA_LOG_READ_INDEX` (read once), else `Locked`.
    pub fn process_default() -> Self {
        static FROM_ENV: OnceLock<LogReadIndex> = OnceLock::new();
        match PROCESS_DEFAULT.load(Ordering::Relaxed) {
            UNSET => {
                *FROM_ENV.get_or_init(
                    || match std::env::var("AMPHORA_LOG_READ_INDEX").as_deref() {
                        Ok("filtered") => Self::Filtered,
                        Ok("published") => Self::Published,
                        Ok("optimistic") => Self::Optimistic,
                        _ => Self::Locked,
                    },
                )
            }
            set => Self::from_u8(set),
        }
    }

    /// Sets the way logs opened from now on in this process take (a bench's or a test's switch).
    pub fn set_process_default(mode: Self) {
        PROCESS_DEFAULT.store(mode as u8, Ordering::Relaxed);
    }
}

/// `set_process_default`'s choice; `UNSET` until it is called.
static PROCESS_DEFAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(UNSET);
const UNSET: u8 = u8::MAX;

/// Called with the frame index and its high-water frame, under the index's lock.
pub(crate) type FromIndex<'a> = dyn FnMut(&FxHashMap<u64, Vec<u64>>, u64) + 'a;
/// Takes the index's lock and calls what it is given with it.
pub(crate) type IndexUnderLock<'a> = dyn Fn(&mut FromIndex<'_>) + 'a;

/// What the lock-free path says of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The snapshot holds no frame of the page: read it from the database file.
    Absent,
    /// The page's latest frame in the snapshot.
    Frame(u64),
    /// Not answered without the lock: look it up there.
    Ask,
}

/// The log's header, readable without its lock: its eight words under a sequence that is odd while
/// a writer changes them (a seqlock, as `crossbeam`'s `AtomicCell` keeps a value too wide for one
/// atomic). Written only by the header's guard, under the header's lock, so writers never race.
#[derive(Debug, Default)]
pub(crate) struct HeaderCopy {
    sequence: AtomicU64,
    words: [AtomicU32; 8],
}

impl HeaderCopy {
    pub(crate) fn new(header: &WalHeader) -> Self {
        let copy = Self::default();
        copy.store(header);
        copy
    }

    /// Called with the header's lock held.
    pub(crate) fn store(&self, header: &WalHeader) {
        let words = Self::words_of(header);
        let begun = self.sequence.load(Ordering::Relaxed).wrapping_add(1);
        self.sequence.store(begun, Ordering::Relaxed);
        fence(Ordering::Release);
        for (slot, word) in self.words.iter().zip(words) {
            slot.store(word, Ordering::Relaxed);
        }
        self.sequence
            .store(begun.wrapping_add(1), Ordering::Release);
    }

    /// The header as its last guard left it, or None while a guard is writing it (one attempt:
    /// the caller then takes the lock).
    pub(crate) fn load(&self) -> Option<WalHeader> {
        let before = self.sequence.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let mut words = [0u32; 8];
        for (word, slot) in words.iter_mut().zip(self.words.iter()) {
            *word = slot.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        (self.sequence.load(Ordering::Relaxed) == before).then(|| Self::header_of(words))
    }

    fn words_of(header: &WalHeader) -> [u32; 8] {
        [
            header.magic,
            header.file_format,
            header.page_size,
            header.checkpoint_seq,
            header.salt_1,
            header.salt_2,
            header.checksum_1,
            header.checksum_2,
        ]
    }

    fn header_of(words: [u32; 8]) -> WalHeader {
        let [magic, file_format, page_size, checkpoint_seq, salt_1, salt_2, checksum_1, checksum_2] =
            words;
        WalHeader {
            magic,
            file_format,
            page_size,
            checkpoint_seq,
            salt_1,
            salt_2,
            checksum_1,
            checksum_2,
        }
    }
}

/// An exact set of the pages the log holds: one bit a page number, as many words as the highest
/// page the log has held needs (the log's own size; grown by doubling, so its copies cost O(1) a
/// page). Exact: it never says a page is held that isn't, past what was cut since it was built.
#[derive(Debug)]
struct PageBits {
    generation: u64,
    words: Box<[AtomicU64]>,
}

impl PageBits {
    fn with_words(generation: u64, count: usize) -> Self {
        Self {
            generation,
            words: (0..count).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn place(page: u64) -> Option<(usize, u64)> {
        let word = usize::try_from(page / 64).ok()?;
        Some((word, 1u64 << (page % 64)))
    }

    /// None when the page is past the bits' reach (asked under the lock).
    fn holds(&self, page: u64) -> Option<bool> {
        let (word, bit) = Self::place(page)?;
        let value = self.words.get(word)?.load(Ordering::Acquire);
        Some(value & bit != 0)
    }

    /// False when the page is past their reach: the caller grows them.
    fn set(&self, page: u64) -> bool {
        let Some((word, bit)) = Self::place(page) else {
            return false;
        };
        match self.words.get(word) {
            Some(slot) => {
                slot.fetch_or(bit, Ordering::Release);
                true
            }
            None => false,
        }
    }

    fn words_for(highest: u64) -> usize {
        usize::try_from(highest / 64)
            .ok()
            .and_then(|word| word.checked_add(1))
            .and_then(usize::checked_next_power_of_two)
            .unwrap_or(usize::MAX)
    }
}

/// Way (a): the whole index as it was when published, frames up to `upto`.
#[derive(Debug, Default)]
struct Published {
    generation: u64,
    upto: u64,
    pages: FxHashMap<u64, Box<[u64]>>,
}

/// Way (b): each page's latest frame, in shards that each carry a version. A shard's slots are
/// open-addressed with linear probing, kept at most half full (Knuth, TAOCP vol. 3 §6.4: at load
/// 1/2 a search that misses probes 2.5 slots expected, one that hits 1.5).
#[derive(Debug)]
struct Latest {
    generation: u64,
    shard_bits: u32,
    shards: Box<[Shard]>,
}

#[derive(Debug)]
struct Shard {
    version: AtomicU64,
    /// The page plus one; zero is an empty slot.
    keys: Box<[AtomicU64]>,
    frames: Box<[AtomicU64]>,
    used: AtomicU64,
}

/// Fibonacci hashing's multiplier, 2^64 divided by the golden ratio (Knuth, TAOCP vol. 3 §6.4): a
/// format constant, spreading consecutive page numbers over the shards and slots.
const FIBONACCI: u64 = 0x9E37_79B9_7F4A_7C15;

impl Latest {
    /// Shards: one for each thread that can read at once where this runs (its reader count), as a
    /// power of two; slots: twice the pages the log holds, so each shard starts at most half full.
    fn sized(generation: u64, pages: usize) -> Self {
        let readers = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let shard_count = readers.checked_next_power_of_two().unwrap_or(1);
        let shard_bits = shard_count.trailing_zeros();
        let slots = pages
            .checked_mul(2)
            .and_then(|slots| slots.checked_div(shard_count))
            .and_then(|slots| slots.checked_add(1))
            .and_then(usize::checked_next_power_of_two)
            .unwrap_or(1)
            .max(1);
        let shards = (0..shard_count)
            .map(|_| Shard {
                version: AtomicU64::new(0),
                keys: (0..slots).map(|_| AtomicU64::new(0)).collect(),
                frames: (0..slots).map(|_| AtomicU64::new(0)).collect(),
                used: AtomicU64::new(0),
            })
            .collect();
        Self {
            generation,
            shard_bits,
            shards,
        }
    }

    fn place(&self, page: u64) -> (Option<&Shard>, u64) {
        let hash = page.wrapping_mul(FIBONACCI);
        let shard = hash
            .checked_shr(64u32.saturating_sub(self.shard_bits))
            .unwrap_or(0);
        let shard = usize::try_from(shard)
            .ok()
            .filter(|_| self.shard_bits > 0)
            .unwrap_or(0);
        (self.shards.get(shard), hash)
    }

    /// Under the version check: the page's latest frame, None when it has none.
    fn get(&self, page: u64) -> Option<Option<u64>> {
        let (shard, hash) = self.place(page);
        let shard = shard?;
        let before = shard.version.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let key = page.checked_add(1)?;
        let mask = u64::try_from(shard.keys.len()).ok()?.checked_sub(1)?;
        let mut found = None;
        let mut slot = hash & mask;
        for _ in 0..shard.keys.len() {
            let index = usize::try_from(slot).ok()?;
            let seen = shard.keys.get(index)?.load(Ordering::Relaxed);
            if seen == 0 {
                break;
            }
            if seen == key {
                found = Some(shard.frames.get(index)?.load(Ordering::Relaxed));
                break;
            }
            slot = slot.wrapping_add(1) & mask;
        }
        fence(Ordering::Acquire);
        (shard.version.load(Ordering::Relaxed) == before).then_some(found)
    }

    /// Under the frame index's lock. False when the shard would pass half full: the caller
    /// rebuilds the table at twice its size.
    fn put(&self, page: u64, frame: u64) -> bool {
        let (Some(shard), hash) = self.place(page) else {
            return false;
        };
        let Some(key) = page.checked_add(1) else {
            return false;
        };
        let Ok(len) = u64::try_from(shard.keys.len()) else {
            return false;
        };
        let Some(mask) = len.checked_sub(1) else {
            return false;
        };
        let mut slot = hash & mask;
        for _ in 0..shard.keys.len() {
            let Ok(index) = usize::try_from(slot) else {
                return false;
            };
            let (Some(keys), Some(frames)) = (shard.keys.get(index), shard.frames.get(index))
            else {
                return false;
            };
            let seen = keys.load(Ordering::Relaxed);
            if seen == key || seen == 0 {
                if seen == 0 {
                    let used = shard.used.load(Ordering::Relaxed).saturating_add(1);
                    if used.saturating_mul(2) > len {
                        return false;
                    }
                    shard.used.store(used, Ordering::Relaxed);
                }
                let begun = shard.version.load(Ordering::Relaxed).wrapping_add(1);
                shard.version.store(begun, Ordering::Relaxed);
                fence(Ordering::Release);
                frames.store(frame, Ordering::Relaxed);
                keys.store(key, Ordering::Relaxed);
                shard
                    .version
                    .store(begun.wrapping_add(1), Ordering::Release);
                return true;
            }
            slot = slot.wrapping_add(1) & mask;
        }
        false
    }
}

/// The lock-free side of the log's frame index: one per log, never replaced, so a connection holds
/// it without the shared state's lock.
#[derive(Debug)]
pub(crate) struct ReadIndex {
    mode: AtomicU8,
    /// Moved on by every change to the index other than an append.
    generation: AtomicU64,
    bits: ArcSwap<PageBits>,
    published: ArcSwap<Published>,
    latest: ArcSwap<Latest>,
    /// Held by the reader publishing a new copy (way (a)); others read under the lock meanwhile.
    publishing: Mutex<()>,
    /// The log's header as the shared state holds it, set wherever that `Arc` is replaced.
    header: ArcSwapOption<HeaderCell>,
}

impl Default for ReadIndex {
    fn default() -> Self {
        Self::new(LogReadIndex::process_default())
    }
}

impl ReadIndex {
    pub(crate) fn new(mode: LogReadIndex) -> Self {
        // Generation 1: nothing is built for it until a reader builds it from the index.
        Self {
            mode: AtomicU8::new(mode as u8),
            generation: AtomicU64::new(1),
            bits: ArcSwap::from_pointee(PageBits::with_words(0, 0)),
            published: ArcSwap::from_pointee(Published::default()),
            latest: ArcSwap::from_pointee(Latest::sized(0, 0)),
            publishing: Mutex::new(()),
            header: ArcSwapOption::empty(),
        }
    }

    /// Called wherever the shared state's header `Arc` is set or replaced.
    pub(crate) fn set_header(&self, header: Arc<HeaderCell>) {
        self.header.store(Some(header));
    }

    /// The header without the shared state's lock or the header's, or None (then read it there).
    pub(crate) fn header(&self) -> Option<WalHeader> {
        if self.mode() == LogReadIndex::Locked {
            return None;
        }
        self.header.load().as_ref()?.copy().load()
    }

    pub(crate) fn mode(&self) -> LogReadIndex {
        LogReadIndex::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Switches the way; what was built is built again by the next reader.
    #[cfg(test)]
    pub(crate) fn set_mode(&self, mode: LogReadIndex) {
        self.mode.store(mode as u8, Ordering::Relaxed);
        self.invalidate();
    }

    /// Any change to the index but an append: what was built is stale.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// An append of `frame` holding `pages`, under the index's lock.
    pub(crate) fn appended(
        &self,
        index: &FxHashMap<u64, Vec<u64>>,
        pages: &mut dyn Iterator<Item = u64>,
        frame: u64,
    ) {
        let mode = self.mode();
        if mode == LogReadIndex::Locked {
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let bits = self.bits.load();
        if bits.generation != generation {
            return;
        }
        let latest = (mode == LogReadIndex::Optimistic).then(|| self.latest.load());
        let latest = latest.filter(|latest| latest.generation == generation);
        let (mut grow_bits, mut grow_latest) = (false, false);
        for page in pages {
            grow_bits |= !bits.set(page);
            if let Some(latest) = &latest {
                grow_latest |= !latest.put(page, frame);
            }
        }
        if grow_bits || grow_latest {
            self.build(index, generation, grow_bits, grow_latest);
        }
    }

    /// Builds the bitmap and the mode's index from `index`, under its lock, for `generation`; at
    /// twice their size where an append outgrew them (`grow_bits`, `grow_latest`).
    fn build(
        &self,
        index: &FxHashMap<u64, Vec<u64>>,
        generation: u64,
        grow_bits: bool,
        grow_latest: bool,
    ) {
        let highest = index.keys().copied().max().unwrap_or(0);
        let previous = self.bits.load();
        let words =
            PageBits::words_for(highest).max(if grow_bits && previous.generation == generation {
                previous.words.len().saturating_mul(2)
            } else {
                0
            });
        let bits = PageBits::with_words(generation, words);
        for &page in index.keys() {
            bits.set(page);
        }
        if self.mode() == LogReadIndex::Optimistic {
            let previous = self.latest.load();
            let slots: usize = previous.shards.iter().map(|shard| shard.keys.len()).sum();
            let mut pages = index
                .len()
                .max(if grow_latest && previous.generation == generation {
                    slots
                } else {
                    0
                });
            // A shard the hash fills past half takes the table to twice its size: at most one
            // doubling a shard (every page in one shard), so the loop ends.
            for _ in 0..=previous.shards.len().max(1).ilog2().saturating_add(1) {
                let latest = Latest::sized(generation, pages);
                let whole = index.iter().all(|(&page, frames)| {
                    frames.last().is_none_or(|&frame| latest.put(page, frame))
                });
                if whole {
                    self.latest.store(Arc::new(latest));
                    break;
                }
                pages = pages.saturating_mul(2).max(1);
            }
        }
        // The bitmap last: a reader that finds it current finds the table current too.
        self.bits.store(Arc::new(bits));
    }

    /// The page's latest frame in `min_frame ..= max_frame` without the index's lock, or `Ask`.
    /// `index` is called (to take the lock) only when what was built is stale.
    pub(crate) fn find(
        &self,
        page: u64,
        min_frame: u64,
        max_frame: u64,
        index: &IndexUnderLock<'_>,
    ) -> Lookup {
        let mode = self.mode();
        if mode == LogReadIndex::Locked {
            return Lookup::Ask;
        }
        let generation = self.generation.load(Ordering::Acquire);
        if self.bits.load().generation != generation {
            index(&mut |held, _high_water| {
                let generation = self.generation.load(Ordering::Acquire);
                if self.bits.load().generation != generation {
                    self.build(held, generation, false, false);
                }
            });
        }
        let bits = self.bits.load();
        if bits.generation != generation {
            return Lookup::Ask;
        }
        match bits.holds(page) {
            Some(false) => return Lookup::Absent,
            None => return Lookup::Ask,
            Some(true) => {}
        }
        match mode {
            LogReadIndex::Locked | LogReadIndex::Filtered => Lookup::Ask,
            LogReadIndex::Published => {
                self.find_published(page, min_frame, max_frame, generation, index)
            }
            LogReadIndex::Optimistic => {
                let latest = self.latest.load();
                if latest.generation != generation {
                    return Lookup::Ask;
                }
                match latest.get(page) {
                    None => Lookup::Ask,
                    Some(None) => Lookup::Absent,
                    Some(Some(frame)) if frame > max_frame => Lookup::Ask,
                    Some(Some(frame)) if frame < min_frame => Lookup::Absent,
                    Some(Some(frame)) => Lookup::Frame(frame),
                }
            }
        }
    }

    fn find_published(
        &self,
        page: u64,
        min_frame: u64,
        max_frame: u64,
        generation: u64,
        index: &IndexUnderLock<'_>,
    ) -> Lookup {
        let mut published = self.published.load();
        if published.generation != generation || published.upto < max_frame {
            // One reader publishes; the rest read under the lock until it has.
            let Some(_publishing) = self.publishing.try_lock() else {
                return Lookup::Ask;
            };
            index(&mut |held, high_water| {
                let generation = self.generation.load(Ordering::Acquire);
                let current = self.published.load();
                if current.generation == generation && current.upto >= high_water {
                    return;
                }
                let pages = held
                    .iter()
                    .map(|(&page, frames)| (page, frames.as_slice().into()))
                    .collect();
                self.published.store(Arc::new(Published {
                    generation,
                    upto: high_water,
                    pages,
                }));
            });
            published = self.published.load();
            if published.generation != generation || published.upto < max_frame {
                return Lookup::Ask;
            }
        }
        let Some(frames) = published.pages.get(&page) else {
            return Lookup::Absent;
        };
        let end = frames.partition_point(|&frame| frame <= max_frame);
        match end.checked_sub(1).and_then(|last| frames.get(last)) {
            Some(&frame) if frame >= min_frame => Lookup::Frame(frame),
            _ => Lookup::Absent,
        }
    }
}
