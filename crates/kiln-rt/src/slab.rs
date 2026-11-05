//! A slab of a shard's desk made as it is used (kiln's goal 11, plan 01a1a87b2e56): room for `capacity` cells is
//! allocated as the shard is built and not written, so the operating system gives it pages only as cells are
//! made in it, at the slab's high-water mark. Memory follows the most a shard has held, not its bound, and the
//! bound stays a bound: past `capacity` nothing is made. A cell once made is never moved, nor made again; a
//! slot given back goes onto its owner's free stack, as before, and is taken from there first.
//!
//! The desk is single-threaded (`Cell`, `!Sync`), as everything on it: a cell is made through a shared
//! reference only at the high-water mark, which no reference has ever reached, and read only below it.

#![allow(unsafe_code)]

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;

/// Room for `capacity` cells of `T`, made in order as they are first needed.
pub(crate) struct Slab<T> {
    cells: Box<[UnsafeCell<MaybeUninit<T>>]>,
    /// Cells made: the high-water mark. Below it every cell is made; at and past it, none.
    made: Cell<usize>,
}

impl<T> Slab<T> {
    /// Room for `capacity` cells, none of them made (nor its pages written).
    pub(crate) fn new(capacity: usize) -> Self {
        let room: Box<[MaybeUninit<T>]> = Box::new_uninit_slice(capacity);
        // SAFETY: `UnsafeCell<U>` is `repr(transparent)` over `U`, so a boxed slice of `MaybeUninit<T>` and one
        // of `UnsafeCell<MaybeUninit<T>>` have the same layout; the box's allocation passes to the new one whole.
        let cells = unsafe { Box::from_raw(Box::into_raw(room) as *mut [UnsafeCell<MaybeUninit<T>>]) };
        Self {
            cells,
            made: Cell::new(0),
        }
    }

    /// The most cells it may hold.
    pub(crate) fn capacity(&self) -> usize {
        self.cells.len()
    }

    /// Cells made so far: its high-water mark.
    pub(crate) fn made(&self) -> usize {
        self.made.get()
    }

    /// Cell `index`, once made.
    #[inline]
    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        if index >= self.made.get() {
            return None;
        }
        let cell = self.cells.get(index)?;
        // SAFETY: cells below `made` were each written once by `make` and are never written through the slab
        // again (what changes in one changes through `T`'s own `Cell`s), so a shared reference is sound for as
        // long as the slab lives.
        Some(unsafe { (*cell.get()).assume_init_ref() })
    }

    /// The next cell made, `value`: its index. None at capacity.
    pub(crate) fn make(&self, value: T) -> Option<usize> {
        let index = self.made.get();
        let next = index.checked_add(1)?;
        let cell = self.cells.get(index)?;
        // SAFETY: cell `index` is at the high-water mark: `get` never handed out a reference to it, and the desk
        // is single-threaded, so nothing reads or writes it while it is written here.
        unsafe {
            (*cell.get()).write(value);
        }
        self.made.set(next);
        Some(index)
    }

    /// Each cell made, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.made.get()).filter_map(|index| self.get(index))
    }
}

impl<T> Drop for Slab<T> {
    fn drop(&mut self) {
        let made = self.made.get();
        for cell in self.cells.iter_mut().take(made) {
            // SAFETY: the cells below `made` were made by `make` and are dropped here once, the slab going.
            unsafe { cell.get_mut().assume_init_drop() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_made_in_order_up_to_capacity_and_read_once_made() {
        let slab = Slab::new(3);
        assert_eq!((slab.capacity(), slab.made()), (3, 0));
        assert!(slab.get(0).is_none());
        assert_eq!(slab.make(Cell::new(10)), Some(0));
        assert_eq!(slab.make(Cell::new(11)), Some(1));
        assert_eq!(slab.get(1).map(Cell::get), Some(11));
        assert!(slab.get(2).is_none(), "not made yet");
        assert_eq!(slab.make(Cell::new(12)), Some(2));
        assert_eq!(slab.make(Cell::new(13)), None, "the bound stays a bound");
        if let Some(c) = slab.get(0) {
            c.set(20);
        }
        assert_eq!(slab.iter().map(Cell::get).collect::<Vec<_>>(), [20, 11, 12]);
    }

    /// Counts its drops.
    struct Counted<'a>(&'a std::sync::atomic::AtomicUsize);

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn what_was_made_is_dropped_with_the_slab_and_nothing_else() {
        let dropped = std::sync::atomic::AtomicUsize::new(0);
        {
            let slab = Slab::new(8);
            for _ in 0..5 {
                let _ = slab.make(Counted(&dropped));
            }
            assert_eq!(dropped.load(std::sync::atomic::Ordering::Relaxed), 0);
        }
        assert_eq!(dropped.load(std::sync::atomic::Ordering::Relaxed), 5, "the five made, not the eight it had room for");
    }
}
