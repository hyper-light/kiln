//! The allocator of the crate's unit tests, counting (goal 9, plan 01a1a87b2cec step 5: a tenth of the CPU). Every
//! allocation the test binary makes goes to the system's allocator as before (the server binary's allocator is
//! `src/bin/kiln.rs`'s, never this); each thread also counts the allocations it makes and their bytes, so a test can say what
//! a piece of work allocated on whichever threads ran it ([`counted`]): a request served, say.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a thread allocated: how many blocks (a reallocation is one), and their bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Made {
    pub blocks: u64,
    pub bytes: u64,
}

thread_local! {
    /// This thread's count. Const-initialized and without a destructor, so counting allocates nothing and works
    /// while the thread is torn down.
    static MADE: Cell<Made> = const { Cell::new(Made { blocks: 0, bytes: 0 }) };
}

fn count(bytes: usize) {
    let _ = MADE.try_with(|m| {
        let now = m.get();
        m.set(Made { blocks: now.blocks.wrapping_add(1), bytes: now.bytes.wrapping_add(u64::try_from(bytes).unwrap_or(u64::MAX)) });
    });
}

/// The system's allocator, each allocation counted on the thread that made it.
struct Counting;

// SAFETY: each method passes its caller's arguments, and so its caller's contract, to the system's allocator and
// returns what that returned; the count beside it touches only a thread-local cell.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: `layout` as the caller gave it, under `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: `ptr` came from this allocator (so from the system's) with `layout`, as the caller promises.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator (so from the system's) with `layout`, as the caller promises.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// What this thread has allocated so far.
pub fn made() -> Made {
    MADE.try_with(Cell::get).unwrap_or_default()
}

/// A count several threads add to: what a task allocated, poll by poll, wherever it ran ([`counted_into`]).
#[derive(Debug, Default)]
pub struct Tally {
    blocks: AtomicU64,
    bytes: AtomicU64,
    /// Time spent in the task's polls, nanoseconds.
    polled_ns: AtomicU64,
}

impl Tally {
    /// What it has counted so far, and the time its task's polls took.
    pub fn now(&self) -> (Made, u64) {
        (Made { blocks: self.blocks.load(Ordering::Acquire), bytes: self.bytes.load(Ordering::Acquire) }, self.polled_ns.load(Ordering::Acquire))
    }
}

/// `fut`, each of its polls' allocations (on the thread that polled it) and time added to `tally` as the poll ends.
pub async fn counted_into<F: Future>(tally: &Tally, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        let (before, began) = (made(), std::time::Instant::now());
        let polled = fut.as_mut().poll(cx);
        let (after, took) = (made(), began.elapsed());
        tally.blocks.fetch_add(after.blocks.wrapping_sub(before.blocks), Ordering::AcqRel);
        tally.bytes.fetch_add(after.bytes.wrapping_sub(before.bytes), Ordering::AcqRel);
        tally.polled_ns.fetch_add(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX), Ordering::AcqRel);
        polled
    })
    .await
}

/// `fut`'s output, and what its polls allocated.
pub async fn counted<F: Future>(fut: F) -> (F::Output, Made) {
    let tally = Tally::default();
    let out = counted_into(&tally, fut).await;
    (out, tally.now().0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 5: what a piece of work allocates is counted on its thread, block by block
    /// and byte by byte, and a poll that allocates nothing counts nothing.
    #[test]
    fn g9_s5_a_threads_allocations_are_counted() -> anyhow::Result<()> {
        let before = made();
        let v: Vec<u8> = Vec::with_capacity(1000);
        let after = made();
        drop(v);
        anyhow::ensure!(after.blocks == before.blocks + 1 && after.bytes == before.bytes + 1000, "{before:?} then {after:?}");
        crate::rt::block_on(async {
            let ((), none) = counted(async {}).await;
            anyhow::ensure!(none == Made::default(), "nothing allocated: {none:?}");
            let (kept, some) = counted(async { vec![0u64; 16] }).await;
            anyhow::ensure!(some == Made { blocks: 1, bytes: 128 } && kept.len() == 16, "one block of 128 bytes: {some:?}");
            Ok(())
        })
    }
}
