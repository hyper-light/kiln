//! Amphora: the versioned storage's buffer pool (Amphora's `docs/design/engine.md`, decisions 2
//! and 15; G7 step 4).
//!
//! Frames made as pages need them, a run at a time, each run's bytes taken from the memory source
//! the shell gives it ([`Room::Shared`], `super::memory`) or within a fixed count ([`Room::Frames`]); each
//! holding one page behind a latch (shared to read, exclusive to write) and a version counter that
//! every exclusive hold bumps as it ends. Readers that keep a copy of a page (the inner pages a
//! B-link descent reads, decision 3) know it is current while the version [`Pool::version`] gives
//! for the page still matches the one they copied it under.
//!
//! Page numbers reach their frames by a direct array, one slot per page (G7 step 3's design (c),
//! vmcache's translation without the virtual memory): the copy-on-update below needs a few bytes
//! of state for every page, resident or not, so the frame's index rides in the same slot at no
//! extra lookup. A pool given a disk ([`Pool::backed`]) evicts by CLOCK: a page that isn't
//! resident is read in, into a frame whose reference bit is clear and which no one latches; a
//! clean frame is reused at once, a dirty one first written to the spill file. The data file
//! changes only by checkpoints (decision 7), so the spill file is never read by recovery and
//! writing it needs no ordering against the redo log: recovery starts from the last checkpoint's
//! pages and replays the log, whatever was evicted. A page read in is verified once, as it enters
//! the pool, against the checksum taken when it was last written out (to the spill file) or when
//! the pool was opened (from the data file). Every frame latched, or a pool without a disk full,
//! is refused with a typed error.
//!
//! Checkpoints copy pages by copy-on-update (decision 7): writes fall in epochs, a checkpoint's
//! pause ends one ([`Pool::begin_copy`]), and the checkpoint then copies the pages written in it as
//! they were at the pause ([`Pool::copy_owed`]), each under its shared latch or from the spill file,
//! while writers go on: a writer that reaches such a page first keeps its bytes as they were
//! before it changes them.

use super::disk::{Disk, File};
use super::memory::Source;
use super::page::PAGE_SIZE;
use crate::error::LimboError;
use crate::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::sync::{Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use crate::Result;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

/// A frame: one page's bytes behind its latch.
struct Frame {
    page: RwLock<Box<[u8]>>,
    /// Exclusive holds ended, from where it was loaded (see [`Pool::loaded_version`]).
    version: AtomicU64,
    /// The page held; 0: none. Changed only under the exclusive latch.
    tag: AtomicU64,
    /// CLOCK's reference bit.
    referenced: AtomicBool,
    /// Changed since read in: written to the spill file before the frame is reused.
    dirty: AtomicBool,
}

impl Frame {
    fn touch(&self) {
        // A load first: a hot frame's line isn't written on every hit.
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
    }
}

/// `Slot::flags`: the page's newest bytes out of the pool are the spill file's, not the data
/// file's.
const SPILLED: u32 = 1;

/// A page's state, resident or not: 16 bytes a page, 0.4% of the data.
#[derive(Default)]
struct Slot {
    /// Its frame's index plus one; 0: not resident.
    frame: AtomicU32,
    flags: AtomicU32,
    /// The low 32 bits of the epoch of its last write (0: none since it was opened). Two epochs
    /// 2^32 checkpoints apart look alike, which only makes a checkpoint copy a page it needn't.
    written: AtomicU32,
    /// The low 32 bits of the last epoch a checkpoint has its bytes of. A false match would skip
    /// a page; [`Pool::copy_owed`] refuses then rather than copy nothing.
    copied: AtomicU32,
    /// The low 32 bits of XXH3 of its bytes where it was last written out: checked once as it is
    /// read in. PostgreSQL's data checksums keep 16 bits a page (`src/include/storage/checksum_impl.h`).
    sum: AtomicU32,
}

impl Slot {
    fn frame(&self) -> Option<usize> {
        let f = self.frame.load(Ordering::Acquire);
        usize::try_from(f).ok()?.checked_sub(1)
    }
}

/// One slot per page number, in buckets that double: bucket `b` holds pages `2^b..2^(b+1)`, made
/// when the first of them is, so the array grows with the database and is never moved.
struct Slots {
    buckets: [OnceLock<Box<[Slot]>>; 64],
}

impl Slots {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    fn at(page: u64) -> Option<(usize, usize)> {
        if page == 0 {
            return None;
        }
        let b = 63u32.checked_sub(page.leading_zeros())?;
        let first = 1u64.checked_shl(b)?;
        Some((
            usize::try_from(b).ok()?,
            usize::try_from(page.checked_sub(first)?).ok()?,
        ))
    }

    fn get(&self, page: u64) -> Option<&Slot> {
        let (b, i) = Self::at(page)?;
        self.buckets.get(b)?.get()?.get(i)
    }

    /// Page `page`'s slot, its bucket made if it isn't yet.
    fn make(&self, page: u64) -> Result<&Slot> {
        let (b, i) = Self::at(page).ok_or(LimboError::IntegerOverflow)?;
        let cell = self.buckets.get(b).ok_or(LimboError::IntegerOverflow)?;
        if cell.get().is_none() {
            let len = 1usize
                .checked_shl(u32::try_from(b).map_err(|_| LimboError::IntegerOverflow)?)
                .ok_or(LimboError::IntegerOverflow)?;
            let mut v = Vec::new();
            v.try_reserve_exact(len)
                .map_err(|_| LimboError::OutOfMemory)?;
            v.resize_with(len, Slot::default);
            // Another caller may have made it first: theirs stays.
            let _ = cell.set(v.into_boxed_slice());
        }
        cell.get()
            .and_then(|s| s.get(i))
            .ok_or(LimboError::IntegerOverflow)
    }
}

/// The first frame of run `k`: `2^k - 1` (saturating past the address space).
fn run_start(k: u32) -> usize {
    1usize
        .checked_shl(k)
        .map_or(usize::MAX, |n| n.saturating_sub(1))
}

/// The bytes one frame takes: its page and its state.
fn frame_bytes() -> u64 {
    u64::try_from(PAGE_SIZE.saturating_add(std::mem::size_of::<Frame>())).unwrap_or(u64::MAX)
}

/// How many frames a pool may make: a fixed count, or as many as a memory source grants it, a run
/// at a time, up to its slots' width (G7 step 2, `AC-NS.2.1`).
#[derive(Clone)]
pub enum Room {
    Frames(usize),
    Shared(Arc<dyn Source>),
}

/// The most frames a pool's slots can name: their frame index is a `u32` less the none mark.
const SLOT_FRAMES: usize = u32::MAX as usize - 1;

/// The pool's counts since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Pages read in from the files.
    pub misses: u64,
    /// Pages put out of a frame to make room for another.
    pub evictions: u64,
    /// Dirty pages written to the spill file as they were evicted.
    pub spilled: u64,
}

/// The frames, the slots, and the next page number to hand out.
pub struct Pool {
    /// Run `k` holds frames `2^k - 1` to `2^(k+1) - 2` (cut at the capacity), made when its first
    /// frame is first used: frames are used in order ([`Pool::victim`]), so a pool sized from the
    /// machine's memory holds only the frames its pages took, at most twice those (G7 step 2).
    runs: Box<[OnceLock<Box<[Frame]>>]>,
    /// The frames the pool may make at most: the room's count, or its slots' width for a shared
    /// room, whose source decides at each run.
    capacity: usize,
    /// The source each run's bytes are taken from (a shared room); None: within `capacity`.
    source: Option<Arc<dyn Source>>,
    /// Frames in runs made: runs are made in order, so frames `0..made`.
    made: AtomicUsize,
    /// The bytes taken from `source`, given back when the pool drops.
    taken: AtomicU64,
    /// Held while a run is made: one maker takes its bytes once.
    growing: Mutex<()>,
    slots: Slots,
    /// Where evicted pages go and pages not resident come from; None: every page stays resident.
    disk: Option<Arc<dyn Disk>>,
    /// Pages handed out: page numbers `1..=allocated`.
    allocated: AtomicU64,
    /// The epoch writes fall in now: moved on only by a checkpoint's pause, when no page is
    /// written.
    epoch: AtomicU64,
    /// The epoch a checkpoint is copying (0: none).
    copying: AtomicU64,
    /// The pages' bytes at the pause of the checkpoint copying now, kept by the first write after
    /// it: at most one copy a page written in the epoch, taken by [`Pool::copy_owed`].
    kept: Mutex<HashMap<u64, Box<[u8]>>>,
    /// A read-in of page `n` holds stripe `n % len`: two of one page load it once.
    loading: Box<[Mutex<()>]>,
    /// Frames never used yet.
    unused: AtomicUsize,
    hand: AtomicUsize,
    /// The highest version any evicted frame reached: a page read in starts past it, so no copy a
    /// reader kept from an earlier stay of the page matches its frame's version.
    evicted_version: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    spilled: AtomicU64,
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Some(source) = &self.source {
            source.give(self.taken.load(Ordering::Acquire));
        }
    }
}

/// A page read under its frame's shared latch.
pub struct PageRead<'a> {
    guard: RwLockReadGuard<'a, Box<[u8]>>,
    version: u64,
}

impl PageRead<'_> {
    /// The page's version while this hold lasts: what a copy taken now is current for.
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Deref for PageRead<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.guard
    }
}

/// A page written under its frame's exclusive latch; its version moves on as it ends, and it is
/// written in the epoch it began in, for the checkpoint that ends that epoch to copy.
pub struct PageWrite<'a> {
    guard: RwLockWriteGuard<'a, Box<[u8]>>,
    frame: &'a Frame,
    slot: &'a Slot,
    epoch: u64,
}

impl Deref for PageWrite<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.guard
    }
}

impl DerefMut for PageWrite<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.guard
    }
}

impl Drop for PageWrite<'_> {
    fn drop(&mut self) {
        self.frame.dirty.store(true, Ordering::Relaxed);
        self.slot.written.store(low(self.epoch), Ordering::Release);
        self.frame.version.fetch_add(1, Ordering::Release);
    }
}

/// The low 32 bits.
fn low(v: u64) -> u32 {
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or(0)
}

fn page_offset(page: u64) -> Result<u64> {
    let size = u64::try_from(PAGE_SIZE).map_err(|_| LimboError::IntegerOverflow)?;
    page.checked_mul(size).ok_or(LimboError::IntegerOverflow)
}

fn checksum(bytes: &[u8]) -> u32 {
    low(twox_hash::XxHash3_64::oneshot(bytes))
}

impl Pool {
    /// A pool of `frames` pages with no disk: every page stays resident, and a page past the
    /// frames is refused (`DatabaseFull`).
    pub fn new(frames: usize) -> Self {
        Self::make(Room::Frames(frames), None)
    }

    /// A pool with no disk whose frames are taken from `source` as pages need them.
    pub fn shared(source: Arc<dyn Source>) -> Self {
        Self::make(Room::Shared(source), None)
    }

    /// A pool of `frames` frames over `disk`'s data file, whose first `allocated` pages are the
    /// database's: read in as they are asked for, each checked once against the checksum taken
    /// here, and evicted by CLOCK when the frames are full. The spill file starts empty.
    pub fn backed(frames: usize, disk: Arc<dyn Disk>, allocated: u64) -> Result<Self> {
        Self::backed_in(Room::Frames(frames), disk, allocated)
    }

    /// [`Pool::backed`], its frames within `room`.
    pub fn backed_in(room: Room, disk: Arc<dyn Disk>, allocated: u64) -> Result<Self> {
        if let Room::Frames(frames) = room {
            if frames == 0 || frames > SLOT_FRAMES {
                return Err(LimboError::InvalidArgument(format!(
                    "versioned storage: a pool of {frames} frames (1 to 2^32 - 2, its slots' width)"
                )));
            }
        }
        disk.truncate(File::Spill, 0)?;
        let pool = Self::make(room, Some(disk.clone()));
        let mut buf = vec![0u8; PAGE_SIZE];
        for page in 1..=allocated {
            if disk.read_at(File::Data, page_offset(page)?, &mut buf)? != PAGE_SIZE {
                return Err(LimboError::Corrupt(
                    "versioned storage: a page past the data file's end".into(),
                ));
            }
            pool.slots
                .make(page)?
                .sum
                .store(checksum(&buf), Ordering::Relaxed);
        }
        pool.allocated.store(allocated, Ordering::Release);
        Ok(pool)
    }

    fn make(room: Room, disk: Option<Arc<dyn Disk>>) -> Self {
        let (frames, source) = match room {
            Room::Frames(n) => (n, None),
            Room::Shared(source) => (SLOT_FRAMES, Some(source)),
        };
        // Runs until one starts at or past the capacity: `usize::BITS` of them cover any.
        let runs = (0..usize::BITS)
            .take_while(|&k| run_start(k) < frames)
            .map(|_| OnceLock::new())
            .collect();
        let stripes = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .next_power_of_two();
        Self {
            runs,
            capacity: frames,
            source,
            made: AtomicUsize::new(0),
            taken: AtomicU64::new(0),
            growing: Mutex::new(()),
            slots: Slots::new(),
            disk,
            allocated: AtomicU64::new(0),
            epoch: AtomicU64::new(1),
            copying: AtomicU64::new(0),
            kept: Mutex::new(HashMap::new()),
            loading: (0..stripes).map(|_| Mutex::new(())).collect(),
            unused: AtomicUsize::new(0),
            hand: AtomicUsize::new(0),
            evicted_version: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            spilled: AtomicU64::new(0),
        }
    }

    /// The pages the pool holds resident at most: for a shared room, as many as its source grants.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The memory source its frames come from, if shared.
    pub fn source(&self) -> Option<Arc<dyn Source>> {
        self.source.clone()
    }

    /// The frames made so far.
    pub fn frames_made(&self) -> usize {
        self.made.load(Ordering::Acquire)
    }

    /// The pages handed out.
    pub fn allocated(&self) -> u64 {
        self.allocated.load(Ordering::Acquire)
    }

    /// Its counts so far.
    pub fn counts(&self) -> Counts {
        Counts {
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            spilled: self.spilled.load(Ordering::Relaxed),
        }
    }

    /// A new page's number, its frame zeroed. Without a disk, `DatabaseFull` once every frame
    /// holds a page; each failed exchange means another caller took a page, so the loop ends
    /// within the capacity.
    pub fn allocate(&self) -> Result<u64> {
        let page = if self.disk.is_some() {
            self.allocated
                .fetch_add(1, Ordering::AcqRel)
                .checked_add(1)
                .ok_or(LimboError::IntegerOverflow)?
        } else {
            let capacity = u64::try_from(self.capacity).map_err(|_| LimboError::IntegerOverflow)?;
            let mut n = self.allocated.load(Ordering::Acquire);
            loop {
                if n >= capacity {
                    return Err(LimboError::DatabaseFull);
                }
                let next = n.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
                match self
                    .allocated
                    .compare_exchange(n, next, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => break next,
                    Err(now) => n = now,
                }
            }
        };
        // A page handed out whose frame can't be had (every frame latched) is never named by
        // anything and never read: its number is skipped.
        let slot = self.slots.make(page)?;
        self.bring(page, slot, true)?;
        Ok(page)
    }

    fn slot(&self, page: u64) -> Result<&Slot> {
        if page == 0 || page > self.allocated() {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: page {page} was never handed out"
            )));
        }
        self.slots.get(page).ok_or_else(|| {
            LimboError::Corrupt(format!("versioned storage: page {page} has no slot"))
        })
    }

    fn frame(&self, i: usize) -> Result<&Frame> {
        self.frame_at(i).ok_or_else(|| {
            LimboError::InternalError(format!(
                "versioned storage: frame {i} past the pool's {}",
                self.capacity
            ))
        })
    }

    /// Frame `i`, its run made if it wasn't; None past the capacity, or when the source refuses
    /// the run's bytes.
    fn frame_at(&self, i: usize) -> Option<&Frame> {
        if i >= self.capacity {
            return None;
        }
        let k = usize::BITS - 1 - i.checked_add(1)?.leading_zeros();
        let start = run_start(k);
        let cell = self.runs.get(usize::try_from(k).ok()?)?;
        if let Some(run) = cell.get() {
            return run.get(i.checked_sub(start)?);
        }
        let _making = self.growing.lock();
        if cell.get().is_none() {
            let end = run_start(k.saturating_add(1)).min(self.capacity);
            let count = end.checked_sub(start)?;
            if let Some(source) = &self.source {
                let bytes = frame_bytes().saturating_mul(u64::try_from(count).ok()?);
                source.take("a page pool's run of frames", bytes).ok()?;
                self.taken.fetch_add(bytes, Ordering::AcqRel);
            }
            let run: Box<[Frame]> = (start..end)
                .map(|_| Frame {
                    page: RwLock::new(vec![0u8; PAGE_SIZE].into_boxed_slice()),
                    version: AtomicU64::new(0),
                    tag: AtomicU64::new(0),
                    referenced: AtomicBool::new(false),
                    dirty: AtomicBool::new(false),
                })
                .collect();
            let _ = cell.set(run);
            self.made.store(end, Ordering::Release);
        }
        cell.get()?.get(i.checked_sub(start)?)
    }

    /// The refusal of a page that left its frame as often as there are frames while it was asked
    /// for, or of a frame when every one is latched.
    fn refused(&self, what: &'static str) -> LimboError {
        let each = u64::try_from(PAGE_SIZE).unwrap_or(u64::MAX);
        let all = u64::try_from(self.frames_made())
            .unwrap_or(u64::MAX)
            .saturating_mul(each);
        LimboError::MemoryBudget {
            what,
            asked: each,
            held: all,
            limit: all,
        }
    }

    /// `page`, latched shared; read in first if it isn't resident.
    pub fn read(&self, page: u64) -> Result<PageRead<'_>> {
        let slot = self.slot(page)?;
        // Each pass that doesn't return saw the page leave its frame, or read it in: one a frame
        // at most before the pool is churning faster than this reader can latch.
        // Bounded by the frames made, read again at each try: a pool making its frames lazily has
        // as many tries as frames once they are made.
        let mut tries = 0usize;
        while tries <= self.frames_made().saturating_add(1) {
            tries = tries.saturating_add(1);
            if let Some(i) = slot.frame() {
                let fr = self.frame(i)?;
                let guard = fr.page.read();
                if fr.tag.load(Ordering::Acquire) == page {
                    fr.touch();
                    let version = fr.version.load(Ordering::Acquire);
                    return Ok(PageRead { guard, version });
                }
                continue;
            }
            self.bring(page, slot, false)?;
        }
        Err(self.refused("a page that left its frame on every try"))
    }

    /// `page`, latched exclusive; read in first if it isn't resident. The first write to a page
    /// the checkpoint copying now still owes keeps its bytes first, as they were at the
    /// checkpoint's pause.
    pub fn write(&self, page: u64) -> Result<PageWrite<'_>> {
        let slot = self.slot(page)?;
        // Bounded by the frames made, read again at each try: a pool making its frames lazily has
        // as many tries as frames once they are made.
        let mut tries = 0usize;
        while tries <= self.frames_made().saturating_add(1) {
            tries = tries.saturating_add(1);
            if let Some(i) = slot.frame() {
                let frame = self.frame(i)?;
                let guard = frame.page.write();
                if frame.tag.load(Ordering::Acquire) != page {
                    continue;
                }
                frame.touch();
                let copying = self.copying.load(Ordering::Acquire);
                if copying != 0
                    && slot.written.load(Ordering::Acquire) == low(copying)
                    && slot.copied.load(Ordering::Acquire) != low(copying)
                {
                    self.kept.lock().insert(page, guard.clone());
                    slot.copied.store(low(copying), Ordering::Release);
                }
                return Ok(PageWrite {
                    guard,
                    frame,
                    slot,
                    epoch: self.epoch.load(Ordering::Acquire),
                });
            }
            self.bring(page, slot, false)?;
        }
        Err(self.refused("a page that left its frame on every try"))
    }

    fn stripe(&self, page: u64) -> Result<&Mutex<()>> {
        let n = u64::try_from(self.loading.len()).map_err(|_| LimboError::IntegerOverflow)?;
        let s = page
            .checked_rem(n)
            .and_then(|s| usize::try_from(s).ok())
            .ok_or(LimboError::IntegerOverflow)?;
        self.loading.get(s).ok_or(LimboError::IntegerOverflow)
    }

    /// Page `page` put in a frame unless one holds it by now: zeroed and dirty if `zero` (a new
    /// page), else read in and verified. Under the page's stripe it only tries latches, never
    /// waits on one, so a holder of a latch waiting on the stripe can't wait on it in turn.
    fn bring(&self, page: u64, slot: &Slot, zero: bool) -> Result<()> {
        let _loading = self.stripe(page)?.lock();
        if slot.frame().is_some() {
            return Ok(());
        }
        let (i, mut g) = self.victim()?;
        let fr = self.frame(i)?;
        let old = fr.tag.load(Ordering::Acquire);
        if old != 0 {
            let old_slot = self.slots.get(old).ok_or_else(|| {
                LimboError::InternalError(format!("versioned storage: page {old} has no slot"))
            })?;
            if fr.dirty.load(Ordering::Relaxed) {
                let disk = self.disk.as_ref().ok_or(LimboError::DatabaseFull)?;
                disk.write_at(File::Spill, page_offset(old)?, &g)?;
                old_slot.sum.store(checksum(&g), Ordering::Relaxed);
                old_slot.flags.fetch_or(SPILLED, Ordering::Release);
                self.spilled.fetch_add(1, Ordering::Relaxed);
            }
            self.evicted_version
                .fetch_max(fr.version.load(Ordering::Acquire), Ordering::AcqRel);
            old_slot.frame.store(0, Ordering::Release);
            fr.tag.store(0, Ordering::Release);
            fr.dirty.store(false, Ordering::Relaxed);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
        if zero {
            g.fill(0);
            fr.dirty.store(true, Ordering::Relaxed);
        } else {
            self.read_in(page, slot, &mut g)?;
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        let start = self
            .evicted_version
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or(LimboError::IntegerOverflow)?;
        fr.version.fetch_max(start, Ordering::AcqRel);
        fr.tag.store(page, Ordering::Release);
        fr.referenced.store(true, Ordering::Relaxed);
        let at = u32::try_from(i.checked_add(1).ok_or(LimboError::IntegerOverflow)?)
            .map_err(|_| LimboError::IntegerOverflow)?;
        slot.frame.store(at, Ordering::Release);
        drop(g);
        Ok(())
    }

    /// Page `page`'s bytes from where they were last written out, checked against their checksum.
    fn read_in(&self, page: u64, slot: &Slot, into: &mut [u8]) -> Result<()> {
        let disk = self.disk.as_ref().ok_or_else(|| {
            LimboError::Corrupt(format!("versioned storage: page {page} isn't resident"))
        })?;
        let (file, name) = if slot.flags.load(Ordering::Acquire) & SPILLED != 0 {
            (File::Spill, "spill")
        } else {
            (File::Data, "data")
        };
        if disk.read_at(file, page_offset(page)?, into)? != PAGE_SIZE {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: page {page} past the {name} file's end"
            )));
        }
        if checksum(into) != slot.sum.load(Ordering::Relaxed) {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: page {page} read from the {name} file fails its checksum"
            )));
        }
        Ok(())
    }

    /// A frame to load into, under its exclusive latch: a never-used one, else CLOCK's next frame
    /// whose reference bit is clear and which no one latches. Two sweeps: the first clears every
    /// bit it passes, so the second finds a frame unless every one is latched, which is refused.
    fn victim(&self) -> Result<(usize, RwLockWriteGuard<'_, Box<[u8]>>)> {
        let mut u = self.unused.load(Ordering::Relaxed);
        // A frame never used, while the room grants its run: frames are used in order.
        while u < self.capacity && self.frame_at(u).is_some() {
            match self.unused.compare_exchange(
                u,
                u.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok((u, self.frame(u)?.page.write())),
                Err(now) => u = now,
            }
        }
        if self.disk.is_none() {
            return Err(LimboError::DatabaseFull);
        }
        let n = self.frames_made();
        if n == 0 {
            return Err(self.refused("a page pool's first frame"));
        }
        for _ in 0..n.saturating_mul(2) {
            let i = self
                .hand
                .fetch_add(1, Ordering::Relaxed)
                .checked_rem(n)
                .unwrap_or(0);
            let fr = self.frame(i)?;
            let Some(g) = fr.page.try_write() else {
                continue;
            };
            if fr.referenced.swap(false, Ordering::Relaxed) {
                continue;
            }
            return Ok((i, g));
        }
        Err(self.refused("a page pool frame while every frame is latched"))
    }

    /// A checkpoint's pause (call while no page can be written): ends the epoch writes fell in, and
    /// returns it, for [`Pool::copy_owed`] to copy the pages written in it as they are now.
    pub fn begin_copy(&self) -> Result<u64> {
        let ending = self.epoch.load(Ordering::Acquire);
        let next = ending.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        self.copying.store(ending, Ordering::Release);
        self.epoch.store(next, Ordering::Release);
        Ok(ending)
    }

    /// Hands `each` the pages written in `epoch` (as [`Pool::begin_copy`] ended it) among the
    /// first `pages` handed out, one at a time, each with its bytes as they were when it ended,
    /// while writers go on: a page none wrote since, copied now under its shared latch or from the
    /// spill file; one written since, as its first writer kept it. The caller holds one page at a
    /// time, not the pages written (a checkpoint's journal, written as they come). The copying
    /// ends with it.
    pub fn copy_owed(
        &self,
        epoch: u64,
        pages: u64,
        each: &mut dyn FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for page in 1..=pages {
            let Some(slot) = self.slots.get(page) else {
                continue;
            };
            if let Some(bytes) = self.owed(page, slot, epoch)? {
                each(page, &bytes)?;
            }
        }
        self.copying.store(0, Ordering::Release);
        Ok(())
    }

    /// Page `page`'s bytes at the pause that ended `epoch`, if it was written in it.
    fn owed(&self, page: u64, slot: &Slot, epoch: u64) -> Result<Option<Vec<u8>>> {
        // Bounded by the frames made, read again at each try: a pool making its frames lazily has
        // as many tries as frames once they are made.
        let mut tries = 0usize;
        while tries <= self.frames_made().saturating_add(1) {
            tries = tries.saturating_add(1);
            let take = |held: &[u8]| -> Result<Option<Vec<u8>>> {
                if slot.copied.load(Ordering::Acquire) == low(epoch) {
                    return match self.kept.lock().remove(&page) {
                        Some(kept) => Ok(Some(kept.into_vec())),
                        None if slot.written.load(Ordering::Acquire) == low(epoch) => {
                            Err(LimboError::InternalError(format!(
                                "versioned storage: page {page}'s bytes at the checkpoint's pause \
                                 were not kept"
                            )))
                        }
                        None => Ok(None),
                    };
                }
                if slot.written.load(Ordering::Acquire) == low(epoch) {
                    slot.copied.store(low(epoch), Ordering::Release);
                    return Ok(Some(held.to_vec()));
                }
                Ok(None)
            };
            if let Some(i) = slot.frame() {
                let fr = self.frame(i)?;
                let held = fr.page.read();
                if fr.tag.load(Ordering::Acquire) == page {
                    return take(&held);
                }
                continue;
            }
            // Not resident, and under its stripe none can read it in: its bytes are the spill
            // file's if it was written since the pool was opened.
            let _loading = self.stripe(page)?.lock();
            if slot.frame().is_some() {
                continue;
            }
            if slot.copied.load(Ordering::Acquire) != low(epoch)
                && slot.written.load(Ordering::Acquire) != low(epoch)
            {
                return Ok(None);
            }
            let mut buf = vec![0u8; PAGE_SIZE];
            self.read_in(page, slot, &mut buf)?;
            return take(&buf);
        }
        Err(self.refused("a page a checkpoint copies that left its frame on every try"))
    }

    /// `page`'s version now, unlatched: one no copy kept from an earlier stay of the page
    /// matches, and `u64::MAX`, which no version reaches, while it isn't resident.
    pub fn version(&self, page: u64) -> Result<u64> {
        let slot = self.slot(page)?;
        let Some(i) = slot.frame() else {
            return Ok(u64::MAX);
        };
        let fr = self.frame(i)?;
        let v = fr.version.load(Ordering::Acquire);
        if fr.tag.load(Ordering::Acquire) != page {
            return Ok(u64::MAX);
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::disk::Simulated;

    /// Do: hand out every frame and one more; read and write a page. Expect: pages 1 to the
    /// capacity, then `DatabaseFull`; a write's end moves the frame's version, a read's doesn't;
    /// a page never handed out is refused.
    #[test]
    fn pages_are_handed_out_to_the_bound_and_writes_move_versions() {
        let pool = Pool::new(3);
        assert_eq!(
            (1..=3)
                .map(|_| pool.allocate().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(matches!(pool.allocate(), Err(LimboError::DatabaseFull)));
        let before = pool.version(2).unwrap();
        {
            let mut w = pool.write(2).unwrap();
            w[0] = 7;
        }
        assert_eq!(pool.version(2).unwrap(), before + 1);
        let r = pool.read(2).unwrap();
        assert_eq!((r[0], r.version()), (7, before + 1));
        drop(r);
        assert_eq!(pool.version(2).unwrap(), before + 1);
        assert!(pool.read(0).is_err() && Pool::new(3).read(1).is_err());
    }

    fn fill(page: u64, round: u8) -> impl Fn(&mut [u8]) {
        move |p: &mut [u8]| {
            for (j, b) in p.iter_mut().enumerate() {
                *b = (page as u8) ^ (j as u8) ^ round;
            }
        }
    }

    fn holds(p: &[u8], page: u64, round: u8) -> bool {
        p.iter()
            .enumerate()
            .all(|(j, b)| *b == (page as u8) ^ (j as u8) ^ round)
    }

    /// Do: a pool of 4 frames over a disk; hand out 40 pages, write each, then read and rewrite
    /// them all twice over. Expect: every page reads as last written; pages went out to the spill
    /// file and came back (evictions, spills and misses counted), and no more than 4 frames
    /// (T-NS.4.2).
    #[test]
    fn g7s4_a_database_ten_times_the_pool_reads_back_what_was_written() {
        let disk: Arc<dyn Disk> = Arc::new(Simulated::new(3, 1).unwrap());
        let pool = Pool::backed(4, disk, 0).unwrap();
        let pages: Vec<u64> = (0..40).map(|_| pool.allocate().unwrap()).collect();
        for round in 0..3u8 {
            for &p in &pages {
                if round > 0 {
                    assert!(holds(&pool.read(p).unwrap(), p, round - 1), "page {p}");
                }
                fill(p, round)(&mut pool.write(p).unwrap());
            }
        }
        for &p in pages.iter().rev() {
            assert!(holds(&pool.read(p).unwrap(), p, 2), "page {p}");
        }
        let c = pool.counts();
        assert!(
            c.evictions >= 100 && c.spilled >= 100 && c.misses >= 80,
            "{c:?}"
        );
        assert_eq!(pool.capacity(), 4);
    }

    /// Do: write a page, push it out to the spill file, damage its bytes there, read it. Expect:
    /// the read refused as corrupt (its checksum checked as it enters the pool); an undamaged page
    /// reads back (T-NS.4.2).
    #[test]
    fn g7s4_a_page_read_in_is_verified_against_its_checksum() {
        let sim = Arc::new(Simulated::new(3, 1).unwrap());
        let disk: Arc<dyn Disk> = sim.clone();
        let pool = Pool::backed(2, disk, 0).unwrap();
        let pages: Vec<u64> = (0..4).map(|_| pool.allocate().unwrap()).collect();
        for &p in &pages {
            fill(p, 1)(&mut pool.write(p).unwrap());
        }
        // Pages 1 and 2 were evicted to make room for 3 and 4.
        sim.write_at(File::Spill, page_offset(1).unwrap() + 9, &[0xAA])
            .unwrap();
        assert!(matches!(pool.read(1), Err(LimboError::Corrupt(_))));
        assert!(holds(&pool.read(2).unwrap(), 2, 1));
    }

    /// Do: a pool of 2 frames; hold both latched, then ask for a third page. Expect: a typed
    /// refusal (`MemoryBudget`), and the page reads once a latch is let go (AC-NS.4.2's bound).
    #[test]
    fn g7s4_every_frame_latched_is_refused_with_a_typed_error() {
        let disk: Arc<dyn Disk> = Arc::new(Simulated::new(3, 1).unwrap());
        let pool = Pool::backed(2, disk, 0).unwrap();
        let pages: Vec<u64> = (0..3).map(|_| pool.allocate().unwrap()).collect();
        let a = pool.read(pages[0]).unwrap();
        let b = pool.read(pages[1]).unwrap();
        assert!(matches!(
            pool.read(pages[2]),
            Err(LimboError::MemoryBudget { .. })
        ));
        assert!(matches!(
            pool.allocate(),
            Err(LimboError::MemoryBudget { .. })
        ));
        drop((a, b));
        assert!(pool.read(pages[2]).is_ok());
    }

    /// A source of `limit` bytes for a test, counting what it holds.
    struct Capped {
        limit: u64,
        held: AtomicU64,
    }

    impl Source for Capped {
        fn take(&self, what: &'static str, bytes: u64) -> Result<()> {
            let limit = self.limit;
            self.held
                .try_update(Ordering::AcqRel, Ordering::Acquire, |h| {
                    h.checked_add(bytes).filter(|a| *a <= limit)
                })
                .map(|_| ())
                .map_err(|held| LimboError::MemoryBudget {
                    what,
                    asked: bytes,
                    held,
                    limit,
                })
        }

        fn give(&self, bytes: u64) {
            self.held.fetch_sub(bytes, Ordering::AcqRel);
        }
    }

    /// T-NS.2.10: a pool over a disk whose frames come from a source with room for three (runs of
    /// one and two; the next run, four, refused), handing out ten pages. Expect: three frames made,
    /// their bytes held by the source, the other pages evicted and read back as written; a source
    /// with no room for one frame refused typed; everything given back when the pool drops
    /// (`AC-NS.2.1`: no share picked, the source decides).
    #[test]
    fn ns2_a_shared_pools_frames_are_what_its_source_grants() {
        let source = Arc::new(Capped {
            limit: frame_bytes() * 3 + frame_bytes() / 2,
            held: AtomicU64::new(0),
        });
        let disk: Arc<dyn Disk> = Arc::new(Simulated::new(5, 1).unwrap());
        let pool = Pool::backed_in(Room::Shared(source.clone()), disk, 0).unwrap();
        let pages: Vec<u64> = (0..10).map(|_| pool.allocate().unwrap()).collect();
        for &p in &pages {
            fill(p, 1)(&mut pool.write(p).unwrap());
        }
        for &p in &pages {
            assert!(holds(&pool.read(p).unwrap(), p, 1), "page {p}");
        }
        assert_eq!(pool.frames_made(), 3);
        assert_eq!(source.held.load(Ordering::Acquire), frame_bytes() * 3);
        assert!(pool.counts().evictions > 0);
        drop(pool);
        assert_eq!(source.held.load(Ordering::Acquire), 0);
        let none = Arc::new(Capped {
            limit: frame_bytes() - 1,
            held: AtomicU64::new(0),
        });
        let disk: Arc<dyn Disk> = Arc::new(Simulated::new(6, 1).unwrap());
        let pool = Pool::backed_in(Room::Shared(none), disk, 0).unwrap();
        assert!(matches!(
            pool.allocate(),
            Err(LimboError::MemoryBudget { .. })
        ));
    }

    /// Do: write 12 pages in a pool of 3 frames, end the epoch, write some again (some evicted,
    /// some resident), copy what's owed. Expect: every page written in the ended epoch, with its
    /// bytes as they were at the pause, resident or not; then the version of a page read back in
    /// matches no copy taken before it left.
    #[test]
    fn g7s4_a_checkpoint_copies_evicted_pages_as_they_were_at_its_pause() {
        let disk: Arc<dyn Disk> = Arc::new(Simulated::new(3, 1).unwrap());
        let pool = Pool::backed(3, disk, 0).unwrap();
        let pages: Vec<u64> = (0..12).map(|_| pool.allocate().unwrap()).collect();
        for &p in &pages {
            fill(p, 1)(&mut pool.write(p).unwrap());
        }
        let v1 = pool.read(pages[0]).unwrap().version();
        let epoch = pool.begin_copy().unwrap();
        for &p in pages.iter().step_by(3) {
            fill(p, 2)(&mut pool.write(p).unwrap());
        }
        let mut owed = Vec::new();
        pool.copy_owed(epoch, pool.allocated(), &mut |p, bytes| {
            owed.push((p, bytes.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(owed.len(), 12);
        for (p, bytes) in &owed {
            assert!(holds(bytes, *p, 1), "page {p}");
        }
        for &p in &pages[1..] {
            let _ = pool.read(p).unwrap();
        }
        assert_eq!(pool.version(pages[0]).unwrap(), u64::MAX);
        assert!(holds(&pool.read(pages[0]).unwrap(), pages[0], 2));
        assert_ne!(pool.version(pages[0]).unwrap(), v1);
    }

    /// T-NS.2.10: a pool whose capacity is far past this machine's memory hands out pages across
    /// several runs of frames. Expect: each page its own frame, its bytes as written.
    #[test]
    fn ns2_frames_are_made_as_pages_are_handed_out() {
        let pool = Pool::new(1 << 40);
        for n in 1..=40u8 {
            let page = pool.allocate().unwrap();
            pool.write(page).unwrap()[0] = n;
        }
        for n in 1..=40u8 {
            assert_eq!(pool.read(u64::from(n)).unwrap()[0], n);
        }
    }
}
