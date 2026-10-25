//! Every allocation the process makes, counted on the thread that makes it: allocations,
//! reallocations (and of those, the ones that moved their block), frees, and the bytes asked for.
//! Each thread counts into a slot of its own that only it writes, with a load and a store (no lock,
//! no read-modify-write, no cache line shared with another writer), and the profiler reads every
//! slot: a profile is the difference of two reads. A slot also counts its thread's deliveries and
//! publishes and carries the role its thread took (publisher, subscriber), so a profile divides by
//! the work done between the same two reads and says which threads allocated.
//!
//! On Linux with glibc the count sits under C's allocator itself: this binary defines `malloc`,
//! `calloc`, `realloc`, `reallocarray`, `free`, `posix_memalign`, `aligned_alloc` and `memalign`
//! (glibc's manual, "Replacing malloc": a program's definitions preempt libc's), counts, and hands
//! each call to glibc's own allocator under the names glibc exports it by (`__libc_malloc` and its
//! kin), so the C libraries a competitor's client is built on (librdkafka) count beside Rust, whose
//! `System` allocator calls `malloc`. `valloc` and `pvalloc`, which nothing here calls, stay
//! glibc's, uncounted. Elsewhere a counting global allocator wraps `System`, and Rust's allocations
//! alone count.
//!
//! The one file of the bench that needs `unsafe` for its allocator (CLAUDE.md §12).
#![allow(unsafe_code)]

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// Slots in the table: a thousand Kafka consumers run several librdkafka threads each.
const SLOTS: usize = 16_384;
/// The slot threads past the table share, counted with atomic adds.
const SHARED: usize = SLOTS - 1;

/// What a thread is to the run, as it says ([`take_role`]); a profile groups threads by it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// A client's or a runtime's own thread, or any other.
    Other,
    Publisher,
    Subscriber,
    /// The profiler's own thread, whose work is no system's: left out of every profile.
    Profiler,
}

impl Role {
    const fn code(self) -> u8 {
        match self {
            Self::Other => 0,
            Self::Publisher => 1,
            Self::Subscriber => 2,
            Self::Profiler => 3,
        }
    }

    const fn of(code: u8) -> Self {
        match code {
            1 => Self::Publisher,
            2 => Self::Subscriber,
            3 => Self::Profiler,
            _ => Self::Other,
        }
    }
}

/// One thread's counters, two cache lines to itself (a line's neighbour is prefetched with it).
#[repr(align(128))]
struct Slot {
    role: AtomicU8,
    allocations: AtomicU64,
    reallocations: AtomicU64,
    moved: AtomicU64,
    frees: AtomicU64,
    bytes: AtomicU64,
    deliveries: AtomicU64,
    published: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            role: AtomicU8::new(0),
            allocations: AtomicU64::new(0),
            reallocations: AtomicU64::new(0),
            moved: AtomicU64::new(0),
            frees: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            deliveries: AtomicU64::new(0),
            published: AtomicU64::new(0),
        }
    }
}

static TABLE: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
/// Slots handed out so far (past the table, threads share its last).
static CLAIMED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // A const-initialized `Cell` of a `Copy` value registers no destructor: it is readable for the
    // whole life of the thread, its teardown included, and reading it never allocates, so the
    // allocator reads it within any allocation.
    static MINE: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// This thread's slot, claimed at its first count, and whether it is its alone.
fn slot() -> (&'static Slot, bool) {
    let mine = MINE.try_with(Cell::get).unwrap_or(SHARED);
    let index = if mine == usize::MAX {
        let index = CLAIMED.fetch_add(1, Ordering::Relaxed).min(SHARED);
        let _ = MINE.try_with(|m| m.set(index));
        index
    } else {
        mine
    };
    (&TABLE[index.min(SHARED)], index != SHARED)
}

fn add(counter: &AtomicU64, by: u64, own: bool) {
    if own {
        // Only this thread writes its slot: a load and a store, no read-modify-write.
        counter.store(
            counter.load(Ordering::Relaxed).wrapping_add(by),
            Ordering::Relaxed,
        );
    } else {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

fn allocated(bytes: usize) {
    let (s, own) = slot();
    add(&s.allocations, 1, own);
    add(&s.bytes, bytes as u64, own);
}

fn reallocated(grew: usize, moved: bool) {
    let (s, own) = slot();
    add(&s.reallocations, 1, own);
    add(&s.moved, u64::from(moved), own);
    add(&s.bytes, grew as u64, own);
}

fn freed() {
    let (s, own) = slot();
    add(&s.frees, 1, own);
}

/// This thread is `role` to the run from here.
pub fn take_role(role: Role) {
    let (s, own) = slot();
    if own {
        s.role.store(role.code(), Ordering::Relaxed);
    }
}

/// This thread's subscriber took a message.
pub fn delivered() {
    let (s, own) = slot();
    add(&s.deliveries, 1, own);
}

/// This thread published a message.
pub fn published() {
    let (s, own) = slot();
    add(&s.published, 1, own);
}

/// What a thread's slot counted.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Counts {
    pub allocations: u64,
    pub reallocations: u64,
    pub moved: u64,
    pub frees: u64,
    /// Bytes asked for: every allocation's size, and what each reallocation grew its block by
    /// (beyond the block's usable size, under glibc).
    pub bytes: u64,
    pub deliveries: u64,
    pub published: u64,
}

impl Counts {
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            allocations: self.allocations.wrapping_sub(earlier.allocations),
            reallocations: self.reallocations.wrapping_sub(earlier.reallocations),
            moved: self.moved.wrapping_sub(earlier.moved),
            frees: self.frees.wrapping_sub(earlier.frees),
            bytes: self.bytes.wrapping_sub(earlier.bytes),
            deliveries: self.deliveries.wrapping_sub(earlier.deliveries),
            published: self.published.wrapping_sub(earlier.published),
        }
    }

    pub fn plus(&self, other: &Self) -> Self {
        Self {
            allocations: self.allocations + other.allocations,
            reallocations: self.reallocations + other.reallocations,
            moved: self.moved + other.moved,
            frees: self.frees + other.frees,
            bytes: self.bytes + other.bytes,
            deliveries: self.deliveries + other.deliveries,
            published: self.published + other.published,
        }
    }
}

/// Every slot's role and counts so far, by slot.
pub fn read() -> Vec<(Role, Counts)> {
    let claimed = CLAIMED.load(Ordering::Relaxed).min(SLOTS);
    TABLE[..claimed]
        .iter()
        .map(|s| {
            (
                Role::of(s.role.load(Ordering::Relaxed)),
                Counts {
                    allocations: s.allocations.load(Ordering::Relaxed),
                    reallocations: s.reallocations.load(Ordering::Relaxed),
                    moved: s.moved.load(Ordering::Relaxed),
                    frees: s.frees.load(Ordering::Relaxed),
                    bytes: s.bytes.load(Ordering::Relaxed),
                    deliveries: s.deliveries.load(Ordering::Relaxed),
                    published: s.published.load(Ordering::Relaxed),
                },
            )
        })
        .collect()
}

/// Whether the count sees this thread's allocations: a profile whose allocator didn't count says
/// so rather than show a zero.
pub fn counting() -> bool {
    let before = slot().0.allocations.load(Ordering::Relaxed);
    let probe: Vec<u8> = std::hint::black_box(Vec::with_capacity(64));
    let after = slot().0.allocations.load(Ordering::Relaxed);
    drop(probe);
    after != before
}

/// glibc's allocator under C's names, counted.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc {
    use std::ffi::{c_int, c_void};

    unsafe extern "C" {
        // glibc's own allocator, exported (GLIBC_2.2.5) beside the public names it backs.
        fn __libc_malloc(size: usize) -> *mut c_void;
        fn __libc_calloc(count: usize, size: usize) -> *mut c_void;
        fn __libc_realloc(block: *mut c_void, size: usize) -> *mut c_void;
        fn __libc_free(block: *mut c_void);
        fn __libc_memalign(align: usize, size: usize) -> *mut c_void;
    }

    /// C's `malloc`, counted.
    ///
    /// # Safety
    /// C's contract for `malloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
        // SAFETY: glibc's malloc with the caller's size; any size is valid.
        let block = unsafe { __libc_malloc(size) };
        if !block.is_null() {
            super::allocated(size);
        }
        block
    }

    /// C's `calloc`, counted.
    ///
    /// # Safety
    /// C's contract for `calloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        // SAFETY: glibc's calloc with the caller's arguments; it checks their product's overflow.
        let block = unsafe { __libc_calloc(count, size) };
        if !block.is_null() {
            super::allocated(count.saturating_mul(size));
        }
        block
    }

    /// C's `realloc`, counted: of no block, an allocation; to no size, a free (glibc's rule).
    ///
    /// # Safety
    /// C's contract for `realloc`: `block` is null or a live block of this allocator's.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(block: *mut c_void, size: usize) -> *mut c_void {
        if block.is_null() {
            // SAFETY: a reallocation of no block is an allocation (C's contract).
            return unsafe { malloc(size) };
        }
        // SAFETY: `block` is a live block of glibc's (every block this binary hands out is one),
        // whose usable size glibc reads from its header.
        let held = unsafe { libc::malloc_usable_size(block) };
        // SAFETY: the caller's live block and size, glibc's realloc contract.
        let moved = unsafe { __libc_realloc(block, size) };
        if size == 0 {
            // glibc frees the block and returns null.
            super::freed();
        } else if !moved.is_null() {
            super::reallocated(size.saturating_sub(held), moved != block);
        }
        moved
    }

    /// glibc's `reallocarray`, counted as the reallocation it is.
    ///
    /// # Safety
    /// As [`realloc`].
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn reallocarray(
        block: *mut c_void,
        count: usize,
        size: usize,
    ) -> *mut c_void {
        match count.checked_mul(size) {
            // SAFETY: the caller's block, with its size multiplied out: realloc's contract.
            Some(bytes) => unsafe { realloc(block, bytes) },
            None => {
                // SAFETY: errno is this thread's own.
                unsafe { *libc::__errno_location() = libc::ENOMEM };
                std::ptr::null_mut()
            }
        }
    }

    /// C's `free`, counted.
    ///
    /// # Safety
    /// C's contract for `free`: `block` is null or a live block of this allocator's.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(block: *mut c_void) {
        if !block.is_null() {
            super::freed();
        }
        // SAFETY: the caller's block, null or glibc's: glibc's free contract.
        unsafe { __libc_free(block) }
    }

    /// POSIX's `posix_memalign`, counted.
    ///
    /// # Safety
    /// POSIX's contract: `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn posix_memalign(
        out: *mut *mut c_void,
        align: usize,
        size: usize,
    ) -> c_int {
        if !align.is_power_of_two() || !align.is_multiple_of(size_of::<*mut c_void>()) {
            return libc::EINVAL;
        }
        // SAFETY: an alignment that is a power of two, as glibc's memalign takes.
        let block = unsafe { __libc_memalign(align, size) };
        if block.is_null() {
            return libc::ENOMEM;
        }
        super::allocated(size);
        // SAFETY: the caller gives `out` writable (POSIX's contract).
        unsafe { out.write(block) };
        0
    }

    /// C's `aligned_alloc`, counted.
    ///
    /// # Safety
    /// C's contract for `aligned_alloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
        if !align.is_power_of_two() {
            // SAFETY: errno is this thread's own.
            unsafe { *libc::__errno_location() = libc::EINVAL };
            return std::ptr::null_mut();
        }
        // SAFETY: an alignment that is a power of two, as glibc's memalign takes.
        let block = unsafe { __libc_memalign(align, size) };
        if !block.is_null() {
            super::allocated(size);
        }
        block
    }

    /// glibc's `memalign`, counted.
    ///
    /// # Safety
    /// glibc's contract for `memalign`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
        // SAFETY: glibc's memalign with the caller's arguments; it rounds an alignment up to a
        // power of two.
        let block = unsafe { __libc_memalign(align, size) };
        if !block.is_null() {
            super::allocated(size);
        }
        block
    }
}

/// Rust's allocations, counted, where C's allocator isn't glibc's.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod wrapped {
    use std::alloc::{GlobalAlloc, Layout, System};

    struct Counting;

    // SAFETY: every method forwards to `System` with its arguments unchanged and returns what
    // `System` returned, so the allocator keeps each guarantee `System` gives. The count touches
    // this thread's slot alone, which never allocates, never reenters the allocator and never
    // unwinds.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller keeps `GlobalAlloc::alloc`'s contract, which is `System`'s.
            let block = unsafe { System.alloc(layout) };
            if !block.is_null() {
                super::allocated(layout.size());
            }
            block
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: as `alloc`.
            let block = unsafe { System.alloc_zeroed(layout) };
            if !block.is_null() {
                super::allocated(layout.size());
            }
            block
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller gives a block this allocator, and so `System`, returned for
            // `layout`.
            unsafe { System.dealloc(block, layout) };
            super::freed();
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller keeps `GlobalAlloc::realloc`'s contract, which is `System`'s.
            let moved = unsafe { System.realloc(block, layout, new_size) };
            if !moved.is_null() {
                super::reallocated(new_size.saturating_sub(layout.size()), moved != block);
            }
            moved
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;
}
