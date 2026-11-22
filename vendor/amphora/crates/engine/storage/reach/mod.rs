//! Amphora: three ways to reach a cached page, prototyped against each other in safe Rust
//! (G7 step 3, CP-T1.3 stage 3; plan "G7 The executor on the new storage").
//!
//! Each is a [`Reach`]: a read or a write of page `n` of a file, through a fixed set of frames
//! that hold some of its pages, evicting by CLOCK when a page that isn't held is asked for.
//! They share the frames, the file stand-in ([`Store`]) and the eviction, and differ only in how
//! a page number becomes a frame and how a reader keeps the frame from changing under it:
//!
//! - [`table::Table`] (a): a page table split into shards, each behind its own lock, and pins:
//!   a reader pins the frame under the shard's lock and reads it under the frame's shared latch,
//!   and keeps its last page pinned in its own count until it moves on (PostgreSQL's buffer
//!   mapping partitions and private reference counts, `src/backend/storage/buffer/bufmgr.c`).
//! - [`swizzle::Swizzle`] (b): a reader keeps a reference by frame index for each page it reached
//!   (standing in for the swizzled child pointers of LeanStore and Umbra, Leis et al., ICDE 2018;
//!   Neumann and Freitag, CIDR 2020) and reads the frame optimistically: the frame's version
//!   before and after, the read thrown away if it moved. The page's words are atomics, standing in
//!   for the unsynchronized reads those systems make (Boehm, "Can seqlocks get along with
//!   programming language memory models?", MSPC 2012, gives the fences). A reference whose frame
//!   now holds another page goes back through the table, as an unswizzled pointer would.
//! - [`direct::Direct`] (c): one slot per page of the file holding the page's frame, read
//!   optimistically as (b) reads: vmcache's direct translation (Leis et al., SIGMOD 2023) without
//!   the virtual memory, for files whose page count fits an array.
//!
//! The harness that measures them ([`bench`]) and the model test with eviction under 32 threads
//! (`model`) drive all three through the trait. The choice between them is made by the numbers
//! (the coordinator's run of the harness), and recorded with the losers in `docs/design/`.
//!
//! What these prototypes don't settle: a miss reads its page from [`Store`], an in-memory stand-in
//! for the file, so eviction-heavy runs measure the pools' own work on a miss (finding a victim,
//! moving the mapping), not the disk's.

pub mod bench;
pub mod direct;
#[cfg(test)]
mod model;
pub mod swizzle;
pub mod table;

use super::versioned::page::PAGE_SIZE;
use crate::error::LimboError;
use crate::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::{Mutex, RwLock, RwLockWriteGuard};
use crate::Result;
use std::sync::Arc;

/// Words in a page: the versioned storage's page size (its format), in 8-byte words.
pub const WORDS: usize = PAGE_SIZE / 8;

/// A way to reach a cached page.
pub trait Reach: Send + Sync {
    /// What one client keeps between its accesses (its pins or its references).
    type Client: Send;

    /// The design's name in the harness's records.
    fn name(&self) -> &'static str;

    /// A client's state, empty.
    fn client(&self) -> Self::Client;

    /// `f` run over page `no`'s words while they are page `no`'s and no write is under way. `f`
    /// may run more than once, and over words a writer is changing in a run whose result is then
    /// thrown away: it must only read.
    fn read<R>(&self, c: &mut Self::Client, no: u64, f: impl FnMut(&[AtomicU64]) -> R)
        -> Result<R>;

    /// `f` run once over page `no`'s words, alone: what it stores is the page's from then on.
    fn write<R>(
        &self,
        c: &mut Self::Client,
        no: u64,
        f: impl FnOnce(&[AtomicU64]) -> R,
    ) -> Result<R>;

    /// What the client holds, let go (its pins).
    fn release(&self, c: &mut Self::Client);

    /// Counts since it was made.
    fn stats(&self) -> Stats;
}

/// A pool's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Pages read in from the file.
    pub misses: u64,
    /// Pages put out of a frame to make room for another.
    pub evictions: u64,
    /// Optimistic reads thrown away because the frame moved under them.
    pub retries: u64,
}

/// The file's stand-in: its pages, `1..=pages`, in memory, each behind a lock.
pub struct Store {
    pages: Box<[Mutex<Box<[u64]>>]>,
}

impl Store {
    /// A file of `pages` pages, page `n`'s words as `fill(n, words)` writes them.
    pub fn new(pages: u64, fill: impl Fn(u64, &mut [u64])) -> Result<Self> {
        let n = usize::try_from(pages).map_err(|_| {
            LimboError::InvalidArgument(format!(
                "a file of {pages} pages doesn't fit this machine's memory"
            ))
        })?;
        let mut v = Vec::new();
        v.try_reserve_exact(n)
            .map_err(|_| LimboError::OutOfMemory)?;
        for i in 1..=pages {
            let mut w = vec![0u64; WORDS].into_boxed_slice();
            fill(i, &mut w);
            v.push(Mutex::new(w));
        }
        Ok(Self {
            pages: v.into_boxed_slice(),
        })
    }

    /// Its page count.
    pub fn pages(&self) -> u64 {
        // A slice's length fits in u64 on every platform Amphora builds for.
        u64::try_from(self.pages.len()).unwrap_or(u64::MAX)
    }

    fn page(&self, no: u64) -> Result<&Mutex<Box<[u64]>>> {
        no.checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.pages.get(i))
            .ok_or_else(|| {
                LimboError::InvalidArgument(format!(
                    "page {no} is past the file's {} pages",
                    self.pages.len()
                ))
            })
    }

    fn load(&self, no: u64, into: &[AtomicU64]) -> Result<()> {
        let p = self.page(no)?.lock();
        for (d, s) in into.iter().zip(p.iter()) {
            d.store(*s, Ordering::Relaxed);
        }
        Ok(())
    }

    fn save(&self, no: u64, from: &[AtomicU64]) -> Result<()> {
        let mut p = self.page(no)?.lock();
        for (d, s) in p.iter_mut().zip(from.iter()) {
            *d = s.load(Ordering::Relaxed);
        }
        Ok(())
    }

    /// Page `no`'s words as the file has them (written back pages included).
    pub fn words(&self, no: u64) -> Result<Vec<u64>> {
        Ok(self.page(no)?.lock().to_vec())
    }
}

/// Shards for a table, and stripes for the misses' locks: the machine's hardware threads, to the
/// next power of two. Contention on a shard comes from clients running at once, and no more run at
/// once than the machine has threads.
pub fn shards_for_machine() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .next_power_of_two()
}

/// A frame: one page's words, the latch writers and the evictor hold, and the version that moves
/// (odd while it changes) for optimistic readers.
pub(crate) struct Frame {
    latch: RwLock<()>,
    version: AtomicU64,
    /// The page held; 0: none.
    tag: AtomicU64,
    /// Clients holding it in place ((a) only).
    pins: AtomicU32,
    /// CLOCK's reference bit.
    referenced: AtomicBool,
    dirty: AtomicBool,
    words: Box<[AtomicU64]>,
}

impl Frame {
    fn new() -> Self {
        Self {
            latch: RwLock::new(()),
            version: AtomicU64::new(0),
            tag: AtomicU64::new(0),
            pins: AtomicU32::new(0),
            referenced: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            words: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn touch(&self) {
        // A load first: a hot frame's line isn't written on every hit.
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
    }

    /// The version made odd, under the exclusive latch: optimistic readers now throw their reads
    /// away. Boehm's writer: the store, then a release fence before the words change.
    fn begin(&self) -> u64 {
        let v = self.version.load(Ordering::Relaxed);
        self.version.store(v.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        v
    }

    /// The version made even again, past `v`, publishing the words.
    fn end(&self, v: u64) {
        self.version.store(v.wrapping_add(2), Ordering::Release);
    }
}

/// Where a design keeps page numbers' frames, for the shared miss path.
pub(crate) trait Map: Send + Sync {
    fn find(&self, no: u64) -> Option<usize>;
    fn insert(&self, no: u64, frame: usize);
    /// Page `no` taken out, unless `frame` is pinned now: false then, and it stays.
    fn take(&self, no: u64, frame: &Frame) -> bool;
}

/// What an optimistic read saw.
pub(crate) enum Seen<R> {
    Read(R),
    /// The frame holds another page now.
    Moved,
    /// A write or an eviction was under way: read it under the latch instead.
    Busy,
}

/// The frames and the file, CLOCK, and the miss path every design shares.
pub(crate) struct Core<M: Map> {
    frames: Box<[Frame]>,
    store: Arc<Store>,
    pub(crate) map: M,
    /// A miss on page `n` holds stripe `n % len`: two misses on one page load it once.
    loading: Box<[Mutex<()>]>,
    hand: AtomicUsize,
    unused: AtomicUsize,
    misses: AtomicU64,
    evictions: AtomicU64,
    retries: AtomicU64,
}

impl<M: Map> Core<M> {
    pub(crate) fn new(store: Arc<Store>, frames: usize, map: M) -> Result<Self> {
        if frames == 0 {
            return Err(LimboError::InvalidArgument("a pool of no frames".into()));
        }
        let mut v = Vec::new();
        v.try_reserve_exact(frames)
            .map_err(|_| LimboError::OutOfMemory)?;
        v.extend((0..frames).map(|_| Frame::new()));
        Ok(Self {
            frames: v.into_boxed_slice(),
            store,
            map,
            loading: (0..shards_for_machine()).map(|_| Mutex::new(())).collect(),
            hand: AtomicUsize::new(0),
            unused: AtomicUsize::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        })
    }

    pub(crate) fn frame(&self, i: usize) -> Result<&Frame> {
        self.frames.get(i).ok_or_else(|| {
            LimboError::InternalError(format!("frame {i} past the pool's {}", self.frames.len()))
        })
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
        }
    }

    /// `f` over frame `i`'s words if it holds page `no`, by the version before and after.
    pub(crate) fn optimistic<R>(
        &self,
        i: usize,
        no: u64,
        f: &mut impl FnMut(&[AtomicU64]) -> R,
    ) -> Seen<R> {
        let Some(fr) = self.frames.get(i) else {
            return Seen::Moved;
        };
        let v = fr.version.load(Ordering::Acquire);
        if v & 1 == 1 {
            return Seen::Busy;
        }
        if fr.tag.load(Ordering::Relaxed) != no {
            return Seen::Moved;
        }
        let r = f(&fr.words);
        fence(Ordering::Acquire);
        if fr.version.load(Ordering::Relaxed) != v {
            self.retries.fetch_add(1, Ordering::Relaxed);
            return Seen::Busy;
        }
        fr.touch();
        Seen::Read(r)
    }

    /// `f` over frame `i`'s words under its shared latch, if it holds page `no`.
    pub(crate) fn shared<R>(
        &self,
        i: usize,
        no: u64,
        f: &mut impl FnMut(&[AtomicU64]) -> R,
    ) -> Option<R> {
        let fr = self.frames.get(i)?;
        let _g = fr.latch.read();
        if fr.tag.load(Ordering::Relaxed) != no {
            return None;
        }
        fr.touch();
        Some(f(&fr.words))
    }

    /// An optimistic read, then one under the latch if a writer was there.
    pub(crate) fn read_at<R>(
        &self,
        i: usize,
        no: u64,
        f: &mut impl FnMut(&[AtomicU64]) -> R,
    ) -> Option<R> {
        match self.optimistic(i, no, f) {
            Seen::Read(r) => Some(r),
            Seen::Moved => None,
            Seen::Busy => self.shared(i, no, f),
        }
    }

    /// `op` on frame `i` under its exclusive latch, if it holds page `no`; `op` handed back
    /// unrun if it doesn't.
    pub(crate) fn exclusive_at<R, F: FnOnce(&Frame) -> R>(
        &self,
        i: usize,
        no: u64,
        op: F,
    ) -> std::result::Result<R, F> {
        let Some(fr) = self.frames.get(i) else {
            return Err(op);
        };
        let _g = fr.latch.write();
        if fr.tag.load(Ordering::Relaxed) != no {
            return Err(op);
        }
        let v = fr.begin();
        let r = op(fr);
        fr.end(v);
        fr.touch();
        Ok(r)
    }

    /// A write of page `no` at frame `i` (if it's there), else through the miss path.
    pub(crate) fn write<R>(
        &self,
        at: Option<usize>,
        no: u64,
        f: impl FnOnce(&[AtomicU64]) -> R,
    ) -> Result<(usize, R)> {
        let op = move |fr: &Frame| {
            let r = f(&fr.words);
            fr.dirty.store(true, Ordering::Relaxed);
            r
        };
        let op = match at {
            Some(i) => match self.exclusive_at(i, no, op) {
                Ok(r) => return Ok((i, r)),
                Err(op) => op,
            },
            None => op,
        };
        self.fetch(no, op)
    }

    /// Page `no` brought into a frame if no frame holds it by now, and `op` run on that frame
    /// under its exclusive latch: the frame, and what `op` returned.
    pub(crate) fn fetch<R>(&self, no: u64, op: impl FnOnce(&Frame) -> R) -> Result<(usize, R)> {
        if no == 0 || no > self.store.pages() {
            return Err(LimboError::InvalidArgument(format!(
                "page {no} is past the file's {} pages",
                self.store.pages()
            )));
        }
        let stripe = u64::try_from(self.loading.len())
            .ok()
            .and_then(|n| no.checked_rem(n))
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or(0);
        let _loading = self
            .loading
            .get(stripe)
            .ok_or_else(|| LimboError::InternalError("no stripe for a miss".into()))?
            .lock();
        // Another miss on this page may have loaded it while this one waited: no other can load it
        // now, and an eviction that took it out took its mapping out first.
        let op = match self.map.find(no) {
            Some(i) => match self.exclusive_at(i, no, op) {
                Ok(r) => return Ok((i, r)),
                Err(op) => op,
            },
            None => op,
        };
        // Every pass that doesn't return found its victim pinned between the clock's look and the
        // mapping's lock: at most one pass a frame.
        for _ in 0..self.frames.len() {
            let (i, _g) = self.victim()?;
            let fr = self.frame(i)?;
            let v = fr.begin();
            let old = fr.tag.load(Ordering::Relaxed);
            if old != 0 {
                if fr.dirty.load(Ordering::Relaxed) {
                    if let Err(e) = self.store.save(old, &fr.words) {
                        fr.end(v);
                        return Err(e);
                    }
                    fr.dirty.store(false, Ordering::Relaxed);
                }
                if !self.map.take(old, fr) {
                    fr.end(v);
                    continue;
                }
                fr.tag.store(0, Ordering::Relaxed);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(e) = self.store.load(no, &fr.words) {
                fr.end(v);
                return Err(e);
            }
            self.misses.fetch_add(1, Ordering::Relaxed);
            fr.tag.store(no, Ordering::Relaxed);
            fr.referenced.store(true, Ordering::Relaxed);
            let r = op(fr);
            self.map.insert(no, i);
            fr.end(v);
            return Ok((i, r));
        }
        Err(LimboError::Busy)
    }

    /// A frame to load into, under its exclusive latch: a never-used one, else CLOCK's next frame
    /// whose reference bit is clear and which no one pins or latches. Two sweeps: the first
    /// clears every bit it passes, so the second finds a frame unless every one is held, which
    /// is refused (`Busy`: a frame comes free as a holder lets go).
    fn victim(&self) -> Result<(usize, RwLockWriteGuard<'_, ()>)> {
        let n = self.frames.len();
        if let Ok(i) = self
            .unused
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                (u < n).then(|| u.saturating_add(1))
            })
        {
            let fr = self.frame(i)?;
            return Ok((i, fr.latch.write()));
        }
        for _ in 0..n.saturating_mul(2) {
            let i = self
                .hand
                .fetch_add(1, Ordering::Relaxed)
                .checked_rem(n)
                .unwrap_or(0);
            let fr = self.frame(i)?;
            if fr.pins.load(Ordering::Relaxed) > 0 {
                continue;
            }
            let Some(g) = fr.latch.try_write() else {
                continue;
            };
            if fr.referenced.swap(false, Ordering::Relaxed) || fr.pins.load(Ordering::Relaxed) > 0 {
                continue;
            }
            return Ok((i, g));
        }
        Err(LimboError::Busy)
    }
}

/// A page table split into shards by page number, each a map behind its own lock: (a)'s mapping,
/// and (b)'s for references that went cold.
pub(crate) struct Shards {
    shards: Box<[Mutex<std::collections::HashMap<u64, usize>>]>,
}

impl Shards {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..shards_for_machine())
                .map(|_| Mutex::new(std::collections::HashMap::new()))
                .collect(),
        }
    }

    pub(crate) fn shard(&self, no: u64) -> Option<&Mutex<std::collections::HashMap<u64, usize>>> {
        // A multiplicative hash of the page number picks one, so runs of pages spread across shards.
        let h = no.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_shr(32);
        let n = u64::try_from(self.shards.len()).ok()?;
        self.shards.get(usize::try_from(h.checked_rem(n)?).ok()?)
    }
}

impl Map for Shards {
    fn find(&self, no: u64) -> Option<usize> {
        self.shard(no)?.lock().get(&no).copied()
    }

    fn insert(&self, no: u64, frame: usize) {
        if let Some(s) = self.shard(no) {
            s.lock().insert(no, frame);
        }
    }

    fn take(&self, no: u64, frame: &Frame) -> bool {
        let Some(s) = self.shard(no) else { return true };
        let mut s = s.lock();
        // A reader pins under this lock: a pin seen clear here can't be taken until the mapping
        // is gone.
        if frame.pins.load(Ordering::Acquire) > 0 {
            return false;
        }
        s.remove(&no);
        true
    }
}
