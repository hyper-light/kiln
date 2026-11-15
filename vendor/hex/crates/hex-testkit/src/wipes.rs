//! The wipes watch (the review's E-6): a test binary's memory allocator that tells whether a block freed while a test
//! watches still held that test's secret, so a test holds every copy the node and hex-quic make of a secret they carry
//! to being wiped before its memory is given back (docs/design/credential-grants.md C7, C8).
//!
//! A test binary names it as its own, in its tests alone (CLAUDE.md §12):
//! `#[cfg(test)] #[global_allocator] static WIPES: hex_testkit::wipes::Watch = hex_testkit::wipes::Watch;`
//! A test then watches for its secret ([`watch`]), does what carries it, lets go of everything that held it, and reads
//! how many freed blocks still held it ([`Watching::found`]), which is none where every holder wiped its copy. The
//! watch also notes the first sixteen such blocks' sizes and where in each the secret lay ([`Watching::seen`]), in
//! atomics as it counts them, so a failing test names the copies it found.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "the wipes watch reads a block with x86_64's or aarch64's load, Hex's two architectures (CLAUDE.md §12)"
);

/// A secret's length: 32 random bytes, which no block holds unless a copy of the secret put them there.
pub const SECRET: usize = 32;

/// The allocator: the system's (`std::alloc::System`), each block it is asked to free read for the watched secret
/// first, while a test watches.
pub struct Watch;

/// Whether a test watches now.
static WATCHING: AtomicBool = AtomicBool::new(false);
/// The secret watched for.
static MARKER: [AtomicU8; SECRET] = [const { AtomicU8::new(0) }; SECRET];
/// The blocks freed since the watch began that still held the secret.
static FOUND: AtomicUsize = AtomicUsize::new(0);
/// How many of them a watch tells apart ([`Watching::seen`]).
const SEEN: usize = 16;
/// The first [`SEEN`] of them: each one's size, and where in it the secret lay.
static SEEN_SIZE: [AtomicUsize; SEEN] = [const { AtomicUsize::new(0) }; SEEN];
static SEEN_AT: [AtomicUsize; SEEN] = [const { AtomicUsize::new(0) }; SEEN];
/// One test watches at a time: the test thread's lock, never the allocator's.
static ONE: Mutex<()> = Mutex::new(());

// SAFETY: every allocation and free is the system allocator's, with the caller's layout unchanged, so its contract is
// the system allocator's. A block that changes size takes the trait's own `realloc`: a new block of the same alignment,
// the bytes copied, and the old block freed through `dealloc` here. Nothing here unwinds or allocates as it frees.
unsafe impl GlobalAlloc for Watch {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, of non-zero size as `GlobalAlloc::alloc` requires of its caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCHING.load(Ordering::Acquire) {
            // SAFETY: `ptr` is a block this allocator gave out with `layout` and not yet freed (the caller's contract),
            // so its `layout.size()` bytes are the program's to read until the free below.
            if let Some(at) = unsafe { holds(ptr, layout.size()) } {
                let n = FOUND.fetch_add(1, Ordering::AcqRel);
                if let (Some(size), Some(place)) = (SEEN_SIZE.get(n), SEEN_AT.get(n)) {
                    size.store(layout.size(), Ordering::Release);
                    place.store(at, Ordering::Release);
                }
            }
        }
        // SAFETY: the block and layout the caller gives, which the system allocator gave out through `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Where the `size` bytes at `block` first hold the watched secret, if anywhere.
///
/// # Safety
///
/// `block` is valid for reads of `size` bytes.
unsafe fn holds(block: *const u8, size: usize) -> Option<usize> {
    let marker: [u8; SECRET] =
        std::array::from_fn(|k| MARKER.get(k).map_or(0, |b| b.load(Ordering::Acquire)));
    let last = size.checked_sub(SECRET)?;
    let (&first, rest) = marker.split_first()?;
    (0..=last).find(|&at| {
        // SAFETY: `at <= size - SECRET`, so `at` and the `SECRET - 1` bytes after it lie in the block, which the
        // caller's contract makes readable.
        let read = |offset: usize| unsafe { byte(block.add(at.wrapping_add(offset))) };
        read(0) == first
            && rest
                .iter()
                .zip(1usize..)
                .all(|(&want, offset)| read(offset) == want)
    })
}

/// The byte at `at`, read by one load instruction, which writes nothing: a byte the program never wrote (a struct's
/// padding) is read as whatever memory holds, never as a Rust value.
///
/// # Safety
///
/// `at` is valid for a read of one byte.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn byte(at: *const u8) -> u8 {
    let value: u8;
    // SAFETY: `at` is readable for one byte (the caller's contract); the load writes no memory, uses no stack and keeps
    // the flags, as its options say.
    unsafe {
        std::arch::asm!(
            "mov {value}, byte ptr [{at}]",
            at = in(reg) at,
            value = out(reg_byte) value,
            options(nostack, readonly, preserves_flags),
        );
    }
    value
}

/// As x86_64's: `ldrb`, which loads one byte and zero-extends it.
///
/// # Safety
///
/// `at` is valid for a read of one byte.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn byte(at: *const u8) -> u8 {
    let value: u8;
    // SAFETY: as x86_64's.
    unsafe {
        std::arch::asm!(
            "ldrb {value:w}, [{at}]",
            at = in(reg) at,
            value = out(reg) value,
            options(nostack, readonly, preserves_flags),
        );
    }
    value
}

/// A test's watch for its secret, from [`watch`] until it drops.
pub struct Watching {
    _one: MutexGuard<'static, ()>,
}

/// Watches every block freed from now for `secret`, one test at a time: a test that watches already holds this one
/// back until its own watch ends. Refused where the watch isn't this test binary's global allocator: a block holding
/// the secret is freed first, unwiped, and must be found, so a watch that sees nothing never passes a test.
pub fn watch(secret: &[u8; SECRET]) -> Result<Watching, String> {
    let one = ONE.lock().unwrap_or_else(PoisonError::into_inner);
    for (into, from) in MARKER.iter().zip(secret) {
        into.store(*from, Ordering::Release);
    }
    FOUND.store(0, Ordering::Release);
    WATCHING.store(true, Ordering::Release);
    let watching = Watching { _one: one };
    drop(std::hint::black_box(secret.to_vec()));
    match FOUND.swap(0, Ordering::AcqRel) {
        1 => Ok(watching),
        seen => Err(format!(
            "the wipes watch found {seen} freed blocks holding the secret where one was freed: is it this test binary's global allocator (#[global_allocator])?"
        )),
    }
}

impl Watching {
    /// The blocks freed since the watch began that still held the secret.
    pub fn found(&self) -> usize {
        FOUND.load(Ordering::Acquire)
    }

    /// The first of those blocks (at most 16): each one's size, and where in it the secret lay, so a test names
    /// the copies it found.
    pub fn seen(&self) -> Vec<(usize, usize)> {
        SEEN_SIZE
            .iter()
            .zip(&SEEN_AT)
            .take(self.found())
            .map(|(size, at)| (size.load(Ordering::Acquire), at.load(Ordering::Acquire)))
            .collect()
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        WATCHING.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32 bytes no other block holds: the test's name's digest in each byte's place, as random as a test needs.
    fn secret(test: &str) -> [u8; SECRET] {
        let mut out = [0u8; SECRET];
        for (k, byte) in out.iter_mut().enumerate() {
            let mut hash = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(&(test, k), &mut hash);
            *byte = std::hash::Hasher::finish(&hash).to_le_bytes()[0];
        }
        out
    }

    /// E-6's watch: do free a block holding the secret, one holding other bytes, one wiped first, and the block a grown
    /// Vec leaves behind; expect the first and the last found, and neither the other bytes nor the wiped one.
    #[test]
    fn a_freed_block_is_found_while_it_holds_the_secret_and_not_once_wiped() {
        let secret = secret("a_freed_block_is_found");
        let watching = watch(&secret).unwrap();
        let mut held = vec![7u8; 100];
        held[40..72].copy_from_slice(&secret);
        drop(std::hint::black_box(held));
        assert_eq!(watching.found(), 1, "a freed block holding the secret");

        let mut other = vec![7u8; 100];
        other[40..72].copy_from_slice(&self::secret("another test's"));
        drop(std::hint::black_box(other));
        assert_eq!(watching.found(), 1, "a freed block holding other bytes");

        let mut wiped = vec![7u8; 100];
        wiped[3..35].copy_from_slice(&secret);
        wiped.iter_mut().for_each(|byte| *byte = 0);
        // Read after the wipe, so the wipe is no dead store a compiler may drop before the free.
        drop(std::hint::black_box(wiped));
        assert_eq!(watching.found(), 1, "a block wiped before its free");

        let mut grown = Vec::with_capacity(SECRET);
        grown.extend_from_slice(&secret);
        grown.reserve(4096);
        assert_eq!(watching.found(), 2, "the block a grown Vec left behind");
        grown.iter_mut().for_each(|byte| *byte = 0);
        drop(std::hint::black_box(grown));
        assert_eq!(
            watching.found(),
            2,
            "the grown block, wiped before its free"
        );
    }
}
